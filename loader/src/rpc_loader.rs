use crate::{
    loader::{
        AssetLoadOp, AssetStorage, HandleOp, LoadHandle, LoadInfo, LoadStatus, Loader,
        LoaderInfoProvider,
    },
    rpc_state::{ConnectionState, ResponsePromise, RpcState},
};
use atelier_core::{utils::make_array, AssetTypeId, AssetUuid};
use atelier_schema::{
    data::{artifact, asset_metadata},
    service::asset_hub::{
        snapshot::get_asset_metadata_with_dependencies_results::Owned as GetAssetMetadataWithDependenciesResults,
        snapshot::get_build_artifacts_results::Owned as GetBuildArtifactsResults,
    },
};
use ccl::dhashmap::DHashMap;
use crossbeam_channel::{unbounded, Receiver, Sender};
use futures::Future;
use log::{error, warn};
use std::{
    collections::HashMap,
    error::Error,
    sync::{
        atomic::{AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use tokio::prelude::*;

/// Describes the state of an asset load operation
#[derive(Copy, Clone, PartialEq, Debug)]
enum LoadState {
    /// Indeterminate state - may transition into a load, or result in removal if ref count is < 0
    None,
    /// The load operation needs metadata to progress
    WaitingForMetadata,
    /// Metadata is being fetched for the load operation
    RequestingMetadata,
    /// Dependencies are requested for loading
    RequestDependencies,
    /// Waiting for dependencies to complete loading
    WaitingForDependencies,
    /// Asset is loading. [AssetLoadState] describes the sub-state.
    LoadingAsset(AssetLoadState),
    /// Asset is loaded and available to use. [AssetLoadState] describes the sub-state.
    Loaded(AssetLoadState),
    /// Asset should be unloaded
    UnloadRequested,
    /// Asset is being unloaded by engine systems
    Unloading,
}

impl LoadState {
    fn map_asset_load_state<F>(self, f: F) -> LoadState
    where
        F: FnOnce(AssetLoadState) -> AssetLoadState,
    {
        match self {
            LoadState::LoadingAsset(asset_state) => LoadState::LoadingAsset(f(asset_state)),
            LoadState::Loaded(asset_state) => LoadState::Loaded(f(asset_state)),
            load_state => panic!(
                "map_asset_load_state expected AssetLoadState, LoadState was {:?}",
                load_state
            ),
        }
    }
}

/// Describes the state of loading an asset.
/// This is separate to LoadState to support tracking load states
/// even when an asset is already loaded, like for hot reload.
#[derive(Copy, Clone, PartialEq, Debug)]
enum AssetLoadState {
    /// Waiting for asset data to be fetched
    WaitingForData,
    /// Asset data is being fetched
    RequestingData,
    /// Engine systems are loading asset
    LoadingAsset,
    /// Engine systems have loaded asset, but the asset is not committed.
    /// This state is only reached when AssetLoad.auto_commit == false.
    LoadedUncommitted,
    /// Asset is loaded and ready to use
    Loaded,
}

#[derive(Debug)]
struct AssetLoad {
    asset_id: AssetUuid,
    state: LoadState,
    refs: AtomicUsize,
    asset_type: Option<AssetTypeId>,
    requested_version: Option<u32>,
    loaded_version: Option<u32>,
    auto_commit: bool,
    pending_reload: bool,
}
struct AssetMetadata {
    load_deps: Vec<AssetUuid>,
}

struct HandleAllocator(AtomicU64);
impl HandleAllocator {
    fn alloc(&self) -> LoadHandle {
        LoadHandle(self.0.fetch_add(1, Ordering::Relaxed))
    }
}

/// Keeps track of a pending reload
struct PendingReload {
    /// ID of asset that should be reloaded
    asset_id: AssetUuid,
    /// The version of the asset before it was reloaded
    version_before: u32,
}

struct LoaderData {
    handle_allocator: HandleAllocator,
    load_states: DHashMap<LoadHandle, AssetLoad>,
    uuid_to_load: DHashMap<AssetUuid, LoadHandle>,
    metadata: HashMap<AssetUuid, AssetMetadata>,
    op_tx: Arc<Sender<HandleOp>>,
    op_rx: Receiver<HandleOp>,
    pending_reloads: Vec<PendingReload>,
}

struct RpcRequests {
    pending_data_requests: Vec<ResponsePromise<GetBuildArtifactsResults, LoadHandle>>,
    pending_metadata_requests:
        Vec<ResponsePromise<GetAssetMetadataWithDependenciesResults, Vec<(AssetUuid, LoadHandle)>>>,
}

unsafe impl Send for RpcRequests {}

impl LoaderData {
    fn add_ref(
        uuid_to_load: &DHashMap<AssetUuid, LoadHandle>,
        handle_allocator: &HandleAllocator,
        load_states: &DHashMap<LoadHandle, AssetLoad>,
        id: AssetUuid,
    ) -> LoadHandle {
        let handle = uuid_to_load.get(&id).map(|h| *h);
        let handle = if let Some(handle) = handle {
            handle
        } else {
            *uuid_to_load.get_or_insert_with(&id, || {
                let new_handle = handle_allocator.alloc();
                load_states.insert(
                    new_handle,
                    AssetLoad {
                        asset_id: id,
                        state: LoadState::None,
                        refs: AtomicUsize::new(0),
                        asset_type: None,
                        requested_version: None,
                        loaded_version: None,
                        auto_commit: true,
                        pending_reload: false,
                    },
                );
                new_handle
            })
        };
        load_states
            .get(&handle)
            .map(|h| h.refs.fetch_add(1, Ordering::Relaxed));
        handle
    }
    fn get_asset(&self, load: LoadHandle) -> Option<(AssetTypeId, LoadHandle)> {
        self.load_states
            .get(&load)
            .filter(|a| match a.state {
                LoadState::Loaded(_) => true,
                _ => false,
            })
            .and_then(|a| a.asset_type.map(|t| (t, load)))
    }
    fn remove_ref(load_states: &DHashMap<LoadHandle, AssetLoad>, load: LoadHandle) {
        load_states
            .get(&load)
            .map(|h| h.refs.fetch_sub(1, Ordering::Relaxed));
    }
}

/// [Loader] implementation which communicates with `atelier-daemon`.
/// `RpcLoader` is intended for use in development environments.
pub struct RpcLoader {
    connect_string: String,
    rpc: Arc<Mutex<RpcState>>,
    data: LoaderData,
    requests: Mutex<RpcRequests>,
}

impl Loader for RpcLoader {
    fn get_load(&self, id: AssetUuid) -> Option<LoadHandle> {
        self.data.uuid_to_load.get(&id).map(|l| *l)
    }
    fn get_load_info(&self, load: LoadHandle) -> Option<LoadInfo> {
        self.data.load_states.get(&load).map(|s| LoadInfo {
            asset_id: s.asset_id,
            refs: s.refs.load(Ordering::Relaxed) as u32,
        })
    }
    fn get_load_status(&self, load: LoadHandle) -> LoadStatus {
        use LoadState::*;
        self.data
            .load_states
            .get(&load)
            .map(|s| match s.state {
                None => LoadStatus::NotRequested,
                WaitingForMetadata
                | RequestingMetadata
                | RequestDependencies
                | WaitingForDependencies
                | LoadingAsset(_) => LoadStatus::Loading,
                Loaded(_) => {
                    if let Some(_) = s.loaded_version {
                        LoadStatus::Loaded
                    } else {
                        LoadStatus::Loading
                    }
                }
                UnloadRequested | Unloading => LoadStatus::Unloading,
            })
            .unwrap_or(LoadStatus::NotRequested)
    }
    fn add_ref(&self, id: AssetUuid) -> LoadHandle {
        LoaderData::add_ref(
            &self.data.uuid_to_load,
            &self.data.handle_allocator,
            &self.data.load_states,
            id,
        )
    }
    fn get_asset(&self, load: LoadHandle) -> Option<(AssetTypeId, LoadHandle)> {
        self.data.get_asset(load)
    }
    fn remove_ref(&self, load: LoadHandle) {
        LoaderData::remove_ref(&self.data.load_states, load)
    }
    fn process(&mut self, asset_storage: &dyn AssetStorage) -> Result<(), Box<dyn Error>> {
        let mut rpc = self.rpc.lock().expect("rpc mutex poisoned");
        let mut requests = self.requests.lock().expect("rpc requests mutex poisoned");
        match rpc.connection_state() {
            ConnectionState::Error(err) => {
                error!("Error connecting RPC: {}", err);
                rpc.connect(&self.connect_string);
            }
            ConnectionState::None => rpc.connect(&self.connect_string),
            _ => {}
        };
        rpc.poll();
        process_asset_changes(&mut self.data, &mut rpc, asset_storage)?;
        {
            process_load_ops(asset_storage, &mut self.data.load_states, &self.data.op_rx);
            process_load_states(
                asset_storage,
                &self.data.handle_allocator,
                &mut self.data.load_states,
                &self.data.uuid_to_load,
                &self.data.metadata,
            );
        }
        process_metadata_requests(&mut requests, &mut self.data, &mut rpc)?;
        process_data_requests(&mut requests, &mut self.data, asset_storage, &mut rpc)?;
        Ok(())
    }
}

impl LoaderInfoProvider
    for (
        &DHashMap<AssetUuid, LoadHandle>,
        &DHashMap<LoadHandle, AssetLoad>,
        &HandleAllocator,
    )
{
    fn get_load_handle(&self, id: AssetUuid) -> Option<LoadHandle> {
        self.0.get(&id).map(|l| *l)
    }
    fn get_asset_id(&self, load: LoadHandle) -> Option<AssetUuid> {
        self.1.get(&load).map(|l| l.asset_id)
    }
}

impl RpcLoader {
    pub fn new(connect_string: String) -> std::io::Result<RpcLoader> {
        let (tx, rx) = unbounded();
        Ok(RpcLoader {
            connect_string: connect_string,
            data: LoaderData {
                handle_allocator: HandleAllocator(AtomicU64::new(1)),
                load_states: DHashMap::default(),
                uuid_to_load: DHashMap::default(),
                metadata: HashMap::new(),
                op_rx: rx,
                op_tx: Arc::new(tx),
                pending_reloads: Vec::new(),
            },
            rpc: Arc::new(Mutex::new(RpcState::new()?)),
            requests: Mutex::new(RpcRequests {
                pending_metadata_requests: Vec::new(),
                pending_data_requests: Vec::new(),
            }),
        })
    }
}

fn update_asset_metadata(
    metadata: &mut HashMap<AssetUuid, AssetMetadata>,
    uuid: &AssetUuid,
    reader: &asset_metadata::Reader<'_>,
) -> Result<(), capnp::Error> {
    let mut load_deps = Vec::new();
    for dep in reader.get_load_deps()? {
        load_deps.push(make_array(dep.get_id()?));
    }
    metadata.insert(*uuid, AssetMetadata { load_deps });
    Ok(())
}

struct AssetLoadResult {
    new_state: LoadState,
    new_version: Option<u32>,
    asset_type: Option<AssetTypeId>,
}

impl AssetLoadResult {
    pub fn from_state(new_state: LoadState) -> Self {
        Self {
            new_state,
            new_version: None,
            asset_type: None,
        }
    }
}

fn load_data(
    loader_info: &dyn LoaderInfoProvider,
    chan: &Arc<Sender<HandleOp>>,
    handle: LoadHandle,
    state: &AssetLoad,
    reader: &artifact::Reader<'_>,
    storage: &dyn AssetStorage,
) -> Result<AssetLoadResult, Box<dyn Error>> {
    match state.state {
        LoadState::LoadingAsset(asset_state) | LoadState::Loaded(asset_state) => {
            assert!(
                AssetLoadState::RequestingData == asset_state,
                "load_data expected AssetLoadState::RequestingData, was {:?}",
                asset_state
            );
        }
        load_state => panic!(
            "load_data expected AssetLoadState, LoadState was {:?}",
            load_state
        ),
    }
    let serialized_asset = reader.get_data()?;
    let asset_type: AssetTypeId = make_array(serialized_asset.get_type_uuid()?);
    if let Some(prev_type) = state.asset_type {
        // TODO handle asset type changing?
        assert!(prev_type == asset_type);
    }
    let new_version = state.requested_version.unwrap_or(0) + 1;
    storage.update_asset(
        loader_info,
        &asset_type,
        &serialized_asset.get_data()?,
        handle,
        AssetLoadOp::new(chan.clone(), handle),
        new_version,
    )?;
    let new_state = if state.loaded_version.is_none() {
        LoadState::LoadingAsset(AssetLoadState::LoadingAsset)
    } else {
        LoadState::Loaded(AssetLoadState::LoadingAsset)
    };
    Ok(AssetLoadResult {
        new_state,
        new_version: Some(new_version),
        asset_type: Some(asset_type),
    })
}

fn process_pending_requests<T, U, ProcessFunc>(
    requests: &mut Vec<ResponsePromise<T, U>>,
    mut process_request_func: ProcessFunc,
) where
    ProcessFunc: for<'a> FnMut(
        Result<
            capnp::message::TypedReader<capnp::message::Builder<capnp::message::HeapAllocator>, T>,
            Box<dyn Error>,
        >,
        &mut U,
    ) -> Result<(), Box<dyn Error>>,
    T: for<'a> capnp::traits::Owned<'a> + 'static,
{
    // reverse range so we can remove inside the loop without consequence
    for i in (0..requests.len()).rev() {
        let request = requests
            .get_mut(i)
            .expect("invalid iteration logic when processing RPC requests");
        let result: Result<Async<()>, Box<dyn Error>> = match request.poll() {
            Ok(Async::Ready(response)) => {
                process_request_func(Ok(response), request.get_user_data()).map(|r| Async::Ready(r))
            }
            Ok(Async::NotReady) => Ok(Async::NotReady),
            Err(err) => Err(err),
        };
        match result {
            Err(err) => {
                let _ = process_request_func(Err(err), request.get_user_data());
                requests.swap_remove(i);
            }
            Ok(Async::Ready(_)) => {
                requests.swap_remove(i);
            }
            Ok(Async::NotReady) => {}
        }
    }
}

fn process_data_requests(
    requests: &mut RpcRequests,
    data: &mut LoaderData,
    storage: &dyn AssetStorage,
    rpc: &mut RpcState,
) -> Result<(), Box<dyn Error>> {
    let op_channel = &data.op_tx;
    process_pending_requests(&mut requests.pending_data_requests, |result, handle| {
        let load_result = {
            let load = data
                .load_states
                .get(handle)
                .expect("load did not exist when data request completed");
            match result {
                Ok(reader) => {
                    let reader = reader.get()?;
                    let artifacts = reader.get_artifacts()?;
                    if artifacts.len() == 0 {
                        warn!(
                            "asset data request did not return any data for asset {:?}",
                            load.asset_id
                        );
                        AssetLoadResult::from_state(
                            load.state
                                .map_asset_load_state(|_| AssetLoadState::WaitingForData),
                        )
                    } else {
                        load_data(
                            &(
                                &data.uuid_to_load,
                                &data.load_states,
                                &data.handle_allocator,
                            ),
                            op_channel,
                            *handle,
                            &load,
                            &artifacts.get(0),
                            storage,
                        )?
                    }
                }
                Err(err) => {
                    error!(
                        "asset data request failed for asset {:?}: {}",
                        load.asset_id, err
                    );
                    AssetLoadResult::from_state(
                        load.state
                            .map_asset_load_state(|_| AssetLoadState::WaitingForData),
                    )
                }
            }
        };

        let mut load = data
            .load_states
            .get_mut(handle)
            .expect("load did not exist when data request completed");
        load.state = load_result.new_state;
        if let Some(version) = load_result.new_version {
            load.requested_version = Some(version);
        }
        if let Some(asset_type) = load_result.asset_type {
            load.asset_type = Some(asset_type);
        }
        Ok(())
    });
    if let ConnectionState::Connected = rpc.connection_state() {
        let mut assets_to_request = Vec::new();
        for mut chunk in data.load_states.chunks_write() {
            assets_to_request.extend(
                chunk
                    .iter_mut()
                    .filter(|(_, v)| match v.state {
                        LoadState::LoadingAsset(asset_state) | LoadState::Loaded(asset_state) => {
                            AssetLoadState::WaitingForData == asset_state
                        }
                        _ => false,
                    })
                    .map(|(k, v)| {
                        v.state = v
                            .state
                            .map_asset_load_state(|_| AssetLoadState::RequestingData);
                        (v.asset_id, *k)
                    }),
            );
        }
        if assets_to_request.len() > 0 {
            for (asset, handle) in assets_to_request {
                let response = rpc.request(move |_conn, snapshot| {
                    let mut request = snapshot.get_build_artifacts_request();
                    let mut assets = request.get().init_assets(1);
                    assets.reborrow().get(0).set_id(&asset);
                    (request, handle)
                });
                requests.pending_data_requests.push(response);
            }
        }
    }
    Ok(())
}

fn process_metadata_requests(
    requests: &mut RpcRequests,
    data: &mut LoaderData,
    rpc: &mut RpcState,
) -> Result<(), capnp::Error> {
    let metadata = &mut data.metadata;
    let uuid_to_load = &data.uuid_to_load;
    let load_states = &data.load_states;
    process_pending_requests(
        &mut requests.pending_metadata_requests,
        |result, requested_assets| {
            match result {
                Ok(reader) => {
                    let reader = reader.get()?;
                    let assets = reader.get_assets()?;
                    for asset in assets {
                        let asset_uuid: AssetUuid = make_array(asset.get_id()?.get_id()?);
                        update_asset_metadata(metadata, &asset_uuid, &asset)?;
                        if let Some(load_handle) = uuid_to_load.get(&asset_uuid) {
                            let mut state = load_states
                                .get_mut(&*load_handle)
                                .expect("uuid in uuid_to_load but not in load_states");
                            if let LoadState::RequestingMetadata = state.state {
                                state.state = LoadState::RequestDependencies
                            }
                        }
                    }
                    for (_, load_handle) in requested_assets {
                        let mut state = load_states
                            .get_mut(load_handle)
                            .expect("uuid in uuid_to_load but not in load_states");
                        if let LoadState::RequestingMetadata = state.state {
                            state.state = LoadState::WaitingForMetadata
                        }
                    }
                }
                Err(err) => {
                    error!("metadata request failed: {}", err);
                    for (_, load_handle) in requested_assets {
                        let mut state = load_states
                            .get_mut(load_handle)
                            .expect("uuid in uuid_to_load but not in load_states");
                        if let LoadState::RequestingMetadata = state.state {
                            state.state = LoadState::WaitingForMetadata
                        }
                    }
                }
            };
            Ok(())
        },
    );
    if let ConnectionState::Connected = rpc.connection_state() {
        let mut assets_to_request = Vec::new();
        for mut chunk in load_states.chunks_write() {
            assets_to_request.extend(
                chunk
                    .iter_mut()
                    .filter(|(_, v)| {
                        if let LoadState::WaitingForMetadata = v.state {
                            true
                        } else {
                            false
                        }
                    })
                    .map(|(k, v)| {
                        v.state = LoadState::RequestingMetadata;
                        (v.asset_id, *k)
                    }),
            );
        }
        if assets_to_request.len() > 0 {
            let response = rpc.request(move |_conn, snapshot| {
                let mut request = snapshot.get_asset_metadata_with_dependencies_request();
                let mut assets = request.get().init_assets(assets_to_request.len() as u32);
                for (idx, (asset, _)) in assets_to_request.iter().enumerate() {
                    assets.reborrow().get(idx as u32).set_id(asset);
                }
                (request, assets_to_request)
            });
            requests.pending_metadata_requests.push(response);
        }
    }
    Ok(())
}

fn commit_asset(handle: LoadHandle, load: &mut AssetLoad, asset_storage: &dyn AssetStorage) {
    match load.state {
        LoadState::LoadingAsset(asset_state) | LoadState::Loaded(asset_state) => {
            assert!(
                AssetLoadState::LoadingAsset == asset_state
                    || AssetLoadState::LoadedUncommitted == asset_state
            );
            let asset_type = load
                .asset_type
                .as_ref()
                .expect("in LoadingAsset state but asset_type is None");
            asset_storage.commit_asset_version(asset_type, handle, load.requested_version.unwrap());
            load.loaded_version = load.requested_version;
            load.state = LoadState::Loaded(AssetLoadState::Loaded);
        }
        _ => panic!(
            "attempting to commit asset but load state is {:?}",
            load.state
        ),
    }
}

fn process_load_ops(
    asset_storage: &dyn AssetStorage,
    load_states: &mut DHashMap<LoadHandle, AssetLoad>,
    op_rx: &Receiver<HandleOp>,
) {
    while let Ok(op) = op_rx.try_recv() {
        match op {
            HandleOp::LoadError(_handle, err) => {
                panic!("load error {}", err);
            }
            HandleOp::LoadComplete(handle) => {
                let mut load = load_states
                    .get_mut(&handle)
                    .expect("load op completed but load state does not exist");
                if load.auto_commit {
                    commit_asset(handle, &mut load, asset_storage);
                } else {
                    load.state = load
                        .state
                        .map_asset_load_state(|_| AssetLoadState::LoadedUncommitted)
                }
            }
            HandleOp::LoadDrop(_handle) => panic!("load op dropped without calling complete/error"),
        }
    }
}

fn process_load_states(
    asset_storage: &dyn AssetStorage,
    handle_allocator: &HandleAllocator,
    load_states: &mut DHashMap<LoadHandle, AssetLoad>,
    uuid_to_load: &DHashMap<AssetUuid, LoadHandle>,
    metadata: &HashMap<AssetUuid, AssetMetadata>,
) {
    let mut to_remove = Vec::new();
    for mut chunk in load_states.chunks_write() {
        for (key, mut value) in chunk.iter_mut() {
            let new_state = match value.state {
                LoadState::None if value.refs.load(Ordering::Relaxed) > 0 => {
                    if metadata.contains_key(&value.asset_id) {
                        LoadState::RequestDependencies
                    } else {
                        LoadState::WaitingForMetadata
                    }
                }
                LoadState::None => {
                    // no refs, inactive load
                    LoadState::UnloadRequested
                }
                LoadState::WaitingForMetadata => {
                    if metadata.contains_key(&value.asset_id) {
                        LoadState::RequestDependencies
                    } else {
                        LoadState::WaitingForMetadata
                    }
                }
                LoadState::RequestingMetadata => LoadState::RequestingMetadata,
                LoadState::RequestDependencies => {
                    // Add ref to each of the dependent assets.
                    let asset_id = value.asset_id;
                    let asset_metadata = metadata.get(&asset_id).unwrap_or_else(|| {
                        panic!("Expected metadata for asset `{:?}` to exist.", asset_id)
                    });
                    asset_metadata
                        .load_deps
                        .iter()
                        .for_each(|dependency_asset_id| {
                            LoaderData::add_ref(
                                uuid_to_load,
                                handle_allocator,
                                load_states,
                                *dependency_asset_id,
                            );
                        });

                    LoadState::WaitingForDependencies
                }
                LoadState::WaitingForDependencies => {
                    let asset_id = value.asset_id;
                    let asset_metadata = metadata.get(&asset_id).unwrap_or_else(|| {
                        panic!("Expected metadata for asset `{:?}` to exist.", asset_id)
                    });

                    // Ensure dependencies are loaded by engine before continuing to load this asset.
                    let asset_dependencies_committed =
                        asset_metadata.load_deps.iter().all(|dependency_asset_id| {
                            uuid_to_load
                                .get(dependency_asset_id)
                                .as_ref()
                                .and_then(|dep_load_handle| load_states.get(dep_load_handle))
                                .map(|dep_load| match dep_load.state {
                                    LoadState::Loaded(asset_state)
                                    | LoadState::LoadingAsset(asset_state) => {
                                        // Note that we accept assets to be uncommitted but loaded
                                        // This is to support atomically committing a set of changes when hot reloading
                                        match asset_state {
                                            AssetLoadState::Loaded
                                            | AssetLoadState::LoadedUncommitted => true,
                                            _ => false,
                                        }
                                    }
                                    _ => false,
                                })
                                .unwrap_or(false)
                        });

                    if asset_dependencies_committed {
                        LoadState::LoadingAsset(AssetLoadState::WaitingForData)
                    } else {
                        LoadState::WaitingForDependencies
                    }
                }
                LoadState::LoadingAsset(asset_state) => LoadState::LoadingAsset(asset_state),
                LoadState::Loaded(asset_state) => {
                    match asset_state {
                        AssetLoadState::Loaded => {
                            if value.refs.load(Ordering::Relaxed) <= 0 {
                                LoadState::UnloadRequested
                            } else if value.pending_reload {
                                // turn off auto_commit for hot reloads
                                value.auto_commit = false;
                                value.pending_reload = false;
                                LoadState::Loaded(AssetLoadState::WaitingForData)
                            } else {
                                LoadState::Loaded(AssetLoadState::Loaded)
                            }
                        }
                        _ => LoadState::Loaded(asset_state),
                    }
                }
                LoadState::UnloadRequested => {
                    if let Some(asset_type) = value.asset_type.take() {
                        asset_storage.free(&asset_type, *key);
                        value.requested_version = None;
                        value.loaded_version = None;
                    }

                    // Remove reference from asset dependencies.
                    let asset_id = value.asset_id;
                    let asset_metadata = metadata.get(&asset_id).unwrap_or_else(|| {
                        panic!("Expected metadata for asset `{:?}` to exist.", asset_id)
                    });
                    asset_metadata
                        .load_deps
                        .iter()
                        .for_each(|dependency_asset_id| {
                            if let Some(dependency_load_handle) =
                                uuid_to_load.get(dependency_asset_id).as_ref()
                            {
                                log::debug!("Removing ref from `{:?}`", *dependency_asset_id);
                                LoaderData::remove_ref(load_states, **dependency_load_handle)
                            } else {
                                panic!(
                                    "Expected load handle to exist for asset `{:?}`.",
                                    dependency_asset_id
                                );
                            }
                        });

                    LoadState::Unloading
                }
                LoadState::Unloading => {
                    if value.refs.load(Ordering::Relaxed) <= 0 {
                        to_remove.push(*key);
                    }
                    LoadState::None
                }
            };
            value.state = new_state;
        }
    }
    for i in to_remove {
        load_states.remove(&i);
    }
}

/// Checks for changed assets that need to be reloaded or unloaded
fn process_asset_changes(
    data: &mut LoaderData,
    rpc: &mut RpcState,
    asset_storage: &dyn AssetStorage,
) -> Result<(), Box<dyn Error>> {
    if data.pending_reloads.is_empty() {
        // if we have no pending hot reloads, poll for new changes
        let changes = rpc.check_asset_changes();
        if let Some(changes) = changes {
            // TODO handle deleted assets
            for asset_id in changes.changed.iter() {
                let current_version = data
                    .uuid_to_load
                    .get(asset_id)
                    .map(|l| *l)
                    .and_then(|load_handle| {
                        data.load_states
                            .get(&load_handle)
                            .map(|load| (load_handle, load))
                    })
                    .map(|(load_handle, load)| {
                        load.requested_version.map(|version| (load_handle, version))
                    })
                    .unwrap_or(None);
                if let Some((handle, current_version)) = current_version {
                    let mut load = data
                        .load_states
                        .get_mut(&handle)
                        .expect("load state should exist for pending reload");
                    load.pending_reload = true;
                    data.pending_reloads.push(PendingReload {
                        asset_id: *asset_id,
                        version_before: current_version,
                    });
                }
            }
        }
    } else {
        let is_finished = data.pending_reloads.iter().all(|reload| {
            data.uuid_to_load
                .get(&reload.asset_id)
                .as_ref()
                .and_then(|load_handle| data.load_states.get(load_handle))
                .map(|load| match load.state {
                    LoadState::Loaded(asset_state) | LoadState::LoadingAsset(asset_state) => {
                        match asset_state {
                            // The reload is finished if we have a loaded asset with a version
                            // that is higher than the version observed when the reload was requested
                            AssetLoadState::Loaded | AssetLoadState::LoadedUncommitted => {
                                load.requested_version.unwrap() > reload.version_before
                            }
                            _ => false,
                        }
                    }
                    _ => false,
                })
                // A pending reload for something that is not supposed to be loaded is considered finished.
                // The asset could have been unloaded by being unreferenced.
                .unwrap_or(true)
        });
        if is_finished {
            data.pending_reloads.iter().for_each(|reload| {
                data.uuid_to_load
                    .get(&reload.asset_id)
                    .as_ref()
                    .and_then(|load_handle| {
                        data.load_states
                            .get_mut(load_handle)
                            .map(|load| (load_handle, load))
                    })
                    .map(|(load_handle, mut load)| match load.state {
                        LoadState::Loaded(asset_state) | LoadState::LoadingAsset(asset_state) => {
                            match asset_state {
                                AssetLoadState::LoadedUncommitted => {
                                    // Commit reloaded asset and turn auto_commit back on
                                    // The assets are not auto_commit for reloads to ensure all assets in a
                                    // changeset are made visible together, atomically
                                    commit_asset(**load_handle, &mut load, asset_storage);
                                    load.auto_commit = true;
                                }
                                _ => {}
                            }
                        }
                        _ => {}
                    });
            });
            data.pending_reloads.clear();
        }
    }
    Ok(())
}

impl Default for RpcLoader {
    fn default() -> RpcLoader {
        RpcLoader::new("127.0.0.1:9999".to_string()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TypeUuid;
    use atelier_core::AssetUuid;
    use atelier_daemon::{init_logging, AssetDaemon};
    use atelier_importer::{BoxedImporter, ImportedAsset, Importer, ImporterValue};
    use serde::{Deserialize, Serialize};
    use std::{
        iter::FromIterator,
        path::PathBuf,
        str::FromStr,
        string::FromUtf8Error,
        sync::RwLock,
        thread::{self, JoinHandle},
    };
    use uuid::Uuid;

    #[derive(Debug)]
    struct LoadState {
        size: Option<usize>,
        commit_version: Option<u32>,
        load_version: Option<u32>,
    }
    struct Storage {
        map: RwLock<HashMap<LoadHandle, LoadState>>,
    }
    impl AssetStorage for Storage {
        fn update_asset(
            &self,
            _loader_info: &dyn LoaderInfoProvider,
            _asset_type: &AssetTypeId,
            data: &[u8],
            loader_handle: LoadHandle,
            load_op: AssetLoadOp,
            version: u32,
        ) -> Result<(), Box<dyn Error>> {
            println!(
                "update asset {:?} data size {}",
                loader_handle,
                data.as_ref().len()
            );
            let mut map = self.map.write().unwrap();
            let state = map.entry(loader_handle).or_insert(LoadState {
                size: None,
                commit_version: None,
                load_version: None,
            });

            state.size = Some(data.as_ref().len());
            state.load_version = Some(version);
            load_op.complete();
            Ok(())
        }
        fn commit_asset_version(
            &self,
            _asset_type: &AssetTypeId,
            loader_handle: LoadHandle,
            version: u32,
        ) {
            println!("commit asset {:?}", loader_handle,);
            let mut map = self.map.write().unwrap();
            let state = map.get_mut(&loader_handle).unwrap();

            assert!(state.load_version.unwrap() == version);
            state.commit_version = Some(version);
            state.load_version = None;
        }
        fn free(&self, _asset_type: &AssetTypeId, loader_handle: LoadHandle) {
            println!("free asset {:?}", loader_handle);
            self.map.write().unwrap().remove(&loader_handle);
        }
    }

    /// Removes file comments (begin with `#`) and empty lines.
    #[derive(Clone, Debug, Default, Deserialize, Serialize, TypeUuid)]
    #[uuid = "346e6a3e-3278-4c53-b21c-99b4350662db"]
    pub struct TxtFormat;
    impl TxtFormat {
        fn from_utf8(&self, vec: Vec<u8>) -> Result<String, FromUtf8Error> {
            String::from_utf8(vec).map(|data| {
                let processed = data
                    .lines()
                    .map(|line| {
                        line.find('#')
                            .map(|index| line.split_at(index).0)
                            .unwrap_or(line)
                            .trim()
                    })
                    .filter(|line| !line.is_empty())
                    .flat_map(|line| line.chars().chain(std::iter::once('\n')));
                String::from_iter(processed)
            })
        }
    }
    /// A simple state for Importer to retain the same UUID between imports
    /// for all single-asset source files
    #[derive(Default, Deserialize, Serialize, TypeUuid)]
    #[uuid = "c50c36fe-8df0-48fe-b1d7-3e69ab00a997"]
    pub struct TxtImporterState {
        id: Option<AssetUuid>,
    }
    #[derive(TypeUuid)]
    #[uuid = "fa50e08c-af6c-4ada-aed1-447c116d63bc"]
    struct TxtImporter;
    impl Importer for TxtImporter {
        type State = TxtImporterState;
        type Options = TxtFormat;

        fn version_static() -> u32
        where
            Self: Sized,
        {
            1
        }
        fn version(&self) -> u32 {
            Self::version_static()
        }

        fn import(
            &self,
            source: &mut dyn Read,
            txt_format: Self::Options,
            state: &mut Self::State,
        ) -> atelier_importer::Result<ImporterValue> {
            if state.id.is_none() {
                state.id = Some(*uuid::Uuid::new_v4().as_bytes());
            }
            let mut bytes = Vec::new();
            source.read_to_end(&mut bytes)?;
            let parsed_asset_data = txt_format
                .from_utf8(bytes)
                .expect("Failed to construct string asset.");

            let load_deps = parsed_asset_data
                .lines()
                .filter_map(|line| Uuid::from_str(line).ok())
                .map(|uuid| *uuid.as_bytes())
                .collect::<Vec<AssetUuid>>();

            Ok(ImporterValue {
                assets: vec![ImportedAsset {
                    id: state.id.expect("AssetUuid not generated"),
                    search_tags: Vec::new(),
                    build_deps: Vec::new(),
                    load_deps,
                    instantiate_deps: Vec::new(),
                    asset_data: Box::new(parsed_asset_data),
                    build_pipeline: None,
                }],
            })
        }
    }

    fn wait_for_status(
        status: LoadStatus,
        handle: LoadHandle,
        loader: &mut RpcLoader,
        storage: &Storage,
    ) {
        loop {
            println!("state {:?}", loader.get_load_status(handle));
            if std::mem::discriminant(&status)
                == std::mem::discriminant(&loader.get_load_status(handle))
            {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
            if let Err(e) = loader.process(storage) {
                println!("err {:?}", e);
            }
        }
    }

    #[test]
    fn test_connect() {
        let _ = init_logging(); // Another test may have initialized logging, so we ignore errors.

        // Start daemon in a separate thread
        let daemon_port = 2500;
        let daemon_address = format!("127.0.0.1:{}", daemon_port);
        let _atelier_daemon = spawn_daemon(&daemon_address);

        let mut loader = RpcLoader::new(daemon_address).expect("Failed to construct `RpcLoader`.");
        let handle = loader.add_ref(
            // asset uuid of "tests/assets/asset.txt"
            *uuid::Uuid::parse_str("60352042-616f-460e-abd2-546195c060fe")
                .unwrap()
                .as_bytes(),
        );
        let storage = &mut Storage {
            map: RwLock::new(HashMap::new()),
        };
        wait_for_status(LoadStatus::Loaded, handle, &mut loader, &storage);
        loader.remove_ref(handle);
        wait_for_status(LoadStatus::NotRequested, handle, &mut loader, &storage);
    }

    #[test]
    fn test_load_with_dependencies() {
        let _ = init_logging(); // Another test may have initialized logging, so we ignore errors.

        // Start daemon in a separate thread
        let daemon_port = 2505;
        let daemon_address = format!("127.0.0.1:{}", daemon_port);
        let _atelier_daemon = spawn_daemon(&daemon_address);

        let mut loader = RpcLoader::new(daemon_address).expect("Failed to construct `RpcLoader`.");
        let handle = loader.add_ref(
            // asset uuid of "tests/assets/asset_a.txt"
            *uuid::Uuid::parse_str("a5ce4da0-675e-4460-be02-c8b145c2ee49")
                .unwrap()
                .as_bytes(),
        );
        let storage = &mut Storage {
            map: RwLock::new(HashMap::new()),
        };
        wait_for_status(LoadStatus::Loaded, handle, &mut loader, &storage);

        // Check that dependent assets are loaded
        let asset_handles = asset_tree()
            .iter()
            .map(|(asset_uuid, file_name)| {
                let asset_load_handle = loader
                    .get_load(*asset_uuid)
                    .unwrap_or_else(|| panic!("Expected `{}` to be loaded.", file_name));

                (asset_load_handle, *file_name)
            })
            .collect::<Vec<(LoadHandle, &'static str)>>();

        asset_handles
            .iter()
            .for_each(|(asset_load_handle, file_name)| {
                assert_eq!(
                    std::mem::discriminant(&LoadStatus::Loaded),
                    std::mem::discriminant(&loader.get_load_status(*asset_load_handle)),
                    "Expected `{}` to be loaded.",
                    file_name
                );
            });

        // Remove reference to top level asset.
        loader.remove_ref(handle);
        wait_for_status(LoadStatus::NotRequested, handle, &mut loader, &storage);

        // Remove ref when unloading top level asset.
        asset_handles
            .iter()
            .for_each(|(asset_load_handle, file_name)| {
                println!("Waiting for {} to be `NotRequested`.", file_name);
                wait_for_status(
                    LoadStatus::NotRequested,
                    *asset_load_handle,
                    &mut loader,
                    &storage,
                );
            });
    }

    fn asset_tree() -> Vec<(AssetUuid, &'static str)> {
        [
            ("a5ce4da0-675e-4460-be02-c8b145c2ee49", "asset_a.txt"),
            ("039dc5f8-ee1c-4949-a7df-72383f12c7a2", "asset_b.txt"),
            ("c071f3ff-c9ea-4bf5-b3b9-bf5fc29f9b59", "asset_c.txt"),
            ("55adb689-b91c-42a0-941b-de4a9f7f4f03", "asset_d.txt"),
        ]
        .into_iter()
        .map(|(id, file_name)| {
            let asset_uuid = *uuid::Uuid::parse_str(id)
                .unwrap_or_else(|_| panic!("Failed to parse `{}` as `Uuid`.", id))
                .as_bytes();

            (asset_uuid, *file_name)
        })
        .collect::<Vec<(AssetUuid, &'static str)>>()
    }

    fn spawn_daemon(daemon_address: &str) -> JoinHandle<()> {
        let daemon_address = daemon_address
            .parse()
            .expect("Failed to parse string as `SocketAddr`.");
        thread::Builder::new()
            .name("atelier-daemon".to_string())
            .spawn(move || {
                let tests_path = PathBuf::from_iter(&[env!("CARGO_MANIFEST_DIR"), "tests"]);

                AssetDaemon::default()
                    .with_db_path(tests_path.join("assets_db"))
                    .with_address(daemon_address)
                    .with_importers(std::iter::once((
                        "txt",
                        Box::new(TxtImporter) as Box<dyn BoxedImporter>,
                    )))
                    .with_asset_dirs(vec![tests_path.join("assets")])
                    .run();
            })
            .expect("Failed to spawn `atelier-daemon` thread.")
    }
}
