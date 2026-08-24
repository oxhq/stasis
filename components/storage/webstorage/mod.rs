/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

mod engines;

use std::borrow::ToOwned;
use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;

use log::warn;
use malloc_size_of::MallocSizeOf;
use malloc_size_of_derive::MallocSizeOf;
use net_traits::pub_domains::registered_domain_name;
use profile_traits::mem::{
    ProcessReports, ProfilerChan as MemProfilerChan, Report, ReportKind, perform_memory_report,
};
use profile_traits::path;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use servo_base::generic_channel::{self, GenericReceiver, GenericSender};
use servo_base::id::WebViewId;
use servo_base::threadpool::ThreadPool;
use servo_base::{read_json_from_file, write_json_to_file};
use servo_url::{ImmutableOrigin, ServoUrl};
use storage_traits::webstorage_thread::{
    OriginDescriptor, WEB_STORAGE_STATE_MAX_ENTRIES_PER_AREA_V1,
    WEB_STORAGE_STATE_MAX_KEY_BYTES_V1, WEB_STORAGE_STATE_MAX_ORIGIN_BYTES_V1,
    WEB_STORAGE_STATE_MAX_ORIGINS_V1, WEB_STORAGE_STATE_MAX_PUBLIC_JSON_BYTES_V1,
    WEB_STORAGE_STATE_MAX_TOTAL_BYTES_V1, WEB_STORAGE_STATE_MAX_VALUE_BYTES_V1,
    WEB_STORAGE_STATE_SCHEMA_VERSION_V1, WebStorageMutationError, WebStorageMutationPolicy,
    WebStorageOriginStateV1, WebStorageStateEntryV1, WebStorageStateError,
    WebStorageStateSnapshotV1, WebStorageThreadMsg, WebStorageType,
};
use uuid::Uuid;

use crate::webstorage::engines::WebStorageEngine;
use crate::webstorage::engines::sqlite::SqliteEngine;

const QUOTA_SIZE_LIMIT: usize = 5 * 1024 * 1024;

pub trait WebStorageThreadFactory {
    fn new(
        config_dir: Option<PathBuf>,
        mem_profiler_chan: MemProfilerChan,
        reporter_name: String,
    ) -> Self;
}

impl WebStorageThreadFactory for GenericSender<WebStorageThreadMsg> {
    /// Create a storage thread
    fn new(
        config_dir: Option<PathBuf>,
        mem_profiler_chan: MemProfilerChan,
        reporter_name: String,
    ) -> GenericSender<WebStorageThreadMsg> {
        let (chan, port) = generic_channel::channel().unwrap();
        let chan2 = chan.clone();
        thread::Builder::new()
            .name("WebStorageManager".to_owned())
            .spawn(move || {
                mem_profiler_chan.run_with_memory_reporting(
                    || WebStorageManager::new(port, config_dir).start(),
                    reporter_name,
                    chan2,
                    WebStorageThreadMsg::CollectMemoryReport,
                );
            })
            .expect("Thread spawning failed");
        chan
    }
}

#[derive(Deserialize, MallocSizeOf, Serialize)]
pub struct StorageOrigins {
    // TODO: Consider grouping by eTLD+1
    // TODO: Consider ImmutableOrigin instead of String for tracking origins
    origin_descriptors: FxHashMap<String, OriginDescriptor>,
}

impl StorageOrigins {
    fn new() -> Self {
        StorageOrigins {
            origin_descriptors: FxHashMap::default(),
        }
    }

    /// Ensures that an origin descriptor exists for the given origin.
    ///
    /// Returns `true` if a new origin descriptor was created, or `false` if
    /// one already existed.
    fn ensure_origin_descriptor(&mut self, origin: &ImmutableOrigin) -> bool {
        let origin = origin.ascii_serialization().into_owned();
        match self.origin_descriptors.entry(origin.clone()) {
            Entry::Occupied(_) => false,
            Entry::Vacant(entry) => {
                entry.insert(OriginDescriptor::new(origin));
                true
            },
        }
    }

    fn origin_descriptors(&self) -> Vec<OriginDescriptor> {
        self.origin_descriptors.values().cloned().collect()
    }

    fn take_origins_for_sites(&mut self, sites: &[String]) -> Vec<ImmutableOrigin> {
        // TODO: This can use `extract_if` once MSVR is bumbed (>=1.88)

        let mut result = Vec::new();

        self.origin_descriptors.retain(|_, descriptor| {
            let url =
                ServoUrl::parse(&descriptor.name).expect("Should always be able to parse origins.");

            let Some(domain) = registered_domain_name(&url) else {
                warn!("Failed to get a registered domain name for: {url}");
                return true;
            };
            let domain = domain.to_string();

            if sites.contains(&domain) {
                result.push(url.origin());
                false
            } else {
                true
            }
        });

        result
    }
}

#[derive(Clone, Default, MallocSizeOf)]
pub struct OriginEntry {
    tree: BTreeMap<String, String>,
    size: usize,
}

impl OriginEntry {
    pub fn inner(&self) -> &BTreeMap<String, String> {
        &self.tree
    }

    pub fn insert(&mut self, key: String, value: String) -> Option<String> {
        let old_value = self.tree.insert(key.clone(), value.clone());
        let size_change = match &old_value {
            Some(old) => value.len() as isize - old.len() as isize,
            None => (key.len() + value.len()) as isize,
        };
        self.size = (self.size as isize + size_change) as usize;
        old_value
    }

    pub fn remove(&mut self, key: &str) -> Option<String> {
        let old_value = self.tree.remove(key);
        if let Some(old) = &old_value {
            self.size -= key.len() + old.len();
        }
        old_value
    }

    pub fn clear(&mut self) {
        self.tree.clear();
        self.size = 0;
    }

    pub fn size(&self) -> usize {
        self.size
    }
}

struct WebStorageEnvironment<E: WebStorageEngine> {
    engine: E,
    data: OriginEntry,
}

impl<E: WebStorageEngine> MallocSizeOf for WebStorageEnvironment<E> {
    fn size_of(&self, ops: &mut malloc_size_of::MallocSizeOfOps) -> usize {
        self.data.size_of(ops)
    }
}

impl<E: WebStorageEngine> WebStorageEnvironment<E> {
    fn new(engine: E) -> Self {
        WebStorageEnvironment {
            data: engine.load().unwrap_or_default(),
            engine,
        }
    }

    fn clear(&mut self) {
        self.data.clear();
        let _ = self.engine.clear();
    }

    fn delete(&mut self, key: &str) {
        let _ = self.engine.delete(key);
    }

    fn set(&mut self, key: &str, value: &str) {
        let _ = self.engine.set(key, value);
    }
}

impl<E: WebStorageEngine> Drop for WebStorageEnvironment<E> {
    fn drop(&mut self) {
        self.engine.save(&self.data);
    }
}

struct WebStorageManager {
    port: GenericReceiver<WebStorageThreadMsg>,
    session_storage_origins: StorageOrigins,
    local_storage_origins: StorageOrigins,
    session_data: FxHashMap<WebViewId, FxHashMap<ImmutableOrigin, OriginEntry>>,
    config_dir: Option<PathBuf>,
    thread_pool: Arc<ThreadPool>,
    environments: FxHashMap<ImmutableOrigin, WebStorageEnvironment<SqliteEngine>>,
    revision: u64,
    revision_exhausted: bool,
}

impl WebStorageManager {
    fn new(
        port: GenericReceiver<WebStorageThreadMsg>,
        config_dir: Option<PathBuf>,
    ) -> WebStorageManager {
        let mut local_storage_origins = StorageOrigins::new();
        if let Some(ref config_dir) = config_dir {
            read_json_from_file(&mut local_storage_origins, config_dir, "localstorage.json");
        }
        WebStorageManager {
            port,
            session_storage_origins: StorageOrigins::new(),
            local_storage_origins,
            session_data: FxHashMap::default(),
            config_dir,
            thread_pool: ThreadPool::global(),
            environments: FxHashMap::default(),
            revision: 0,
            revision_exhausted: false,
        }
    }
}

impl WebStorageManager {
    fn bump_revision(&mut self) {
        match self.revision.checked_add(1) {
            Some(revision) => self.revision = revision,
            None => self.revision_exhausted = true,
        }
    }

    fn start(&mut self) {
        loop {
            match self.port.recv().unwrap() {
                WebStorageThreadMsg::ExportState { sender, webview_id } => {
                    let _ = sender.send(self.export_state(webview_id));
                },
                WebStorageThreadMsg::ReplaceState {
                    sender,
                    webview_id,
                    expected_revision,
                    snapshot,
                } => {
                    let _ =
                        sender.send(self.replace_state(webview_id, expected_revision, snapshot));
                },
                WebStorageThreadMsg::Length(sender, storage_type, webview_id, url) => {
                    self.length(sender, storage_type, webview_id, url)
                },
                WebStorageThreadMsg::Key(sender, storage_type, webview_id, url, index) => {
                    self.key(sender, storage_type, webview_id, url, index)
                },
                WebStorageThreadMsg::Keys(sender, storage_type, webview_id, url) => {
                    self.keys(sender, storage_type, webview_id, url)
                },
                WebStorageThreadMsg::SetItem(
                    sender,
                    storage_type,
                    mutation_policy,
                    webview_id,
                    url,
                    name,
                    value,
                ) => {
                    self.set_item(
                        sender,
                        storage_type,
                        mutation_policy,
                        webview_id,
                        url,
                        name,
                        value,
                    );
                },
                WebStorageThreadMsg::GetItem(sender, storage_type, webview_id, url, name) => {
                    self.request_item(sender, storage_type, webview_id, url, name)
                },
                WebStorageThreadMsg::RemoveItem(
                    sender,
                    storage_type,
                    mutation_policy,
                    webview_id,
                    url,
                    name,
                ) => self.remove_item(sender, storage_type, mutation_policy, webview_id, url, name),
                WebStorageThreadMsg::Clear(
                    sender,
                    storage_type,
                    mutation_policy,
                    webview_id,
                    url,
                ) => self.clear(sender, storage_type, mutation_policy, webview_id, url),
                WebStorageThreadMsg::Clone {
                    sender,
                    src: src_webview_id,
                    dest: dest_webview_id,
                } => {
                    self.clone(src_webview_id, dest_webview_id);
                    let _ = sender.send(());
                },
                WebStorageThreadMsg::ListOrigins(sender, storage_type) => {
                    let _ = sender.send(self.origin_descriptors(storage_type));
                },
                WebStorageThreadMsg::ClearDataForSites(sender, storage_type, sites) => {
                    self.clear_data_for_sites(storage_type, &sites);
                    let _ = sender.send(());
                },
                WebStorageThreadMsg::CollectMemoryReport(sender) => {
                    let reports = self.collect_memory_reports();
                    sender.send(ProcessReports::new(reports));
                },
                WebStorageThreadMsg::Exit(sender) => {
                    // Nothing to do since we save localstorage set eagerly.
                    let _ = sender.send(());
                    break;
                },
            }
        }
    }

    fn collect_memory_reports(&self) -> Vec<Report> {
        let mut reports = vec![];
        perform_memory_report(|ops| {
            reports.push(Report {
                path: path!["storage", "local"],
                kind: ReportKind::ExplicitJemallocHeapSize,
                size: self.environments.size_of(ops) + self.local_storage_origins.size_of(ops),
            });

            reports.push(Report {
                path: path!["storage", "session"],
                kind: ReportKind::ExplicitJemallocHeapSize,
                size: self.session_data.size_of(ops) + self.session_storage_origins.size_of(ops),
            });
        });
        reports
    }

    fn save_local_storage_origins(&self) {
        if let Some(ref config_dir) = self.config_dir {
            write_json_to_file(&self.local_storage_origins, config_dir, "localstorage.json");
        }
    }

    fn get_origin_location(&self, origin: &ImmutableOrigin) -> Option<PathBuf> {
        match &self.config_dir {
            Some(config_dir) => {
                const NAMESPACE_SERVO_WEBSTORAGE: &uuid::Uuid = &Uuid::from_bytes([
                    0x37, 0x9e, 0x56, 0xb0, 0x1a, 0x76, 0x44, 0xc5, 0xa4, 0xdb, 0xe2, 0x18, 0xc5,
                    0xc8, 0xa3, 0x5d,
                ]);
                let origin_uuid = Uuid::new_v5(
                    NAMESPACE_SERVO_WEBSTORAGE,
                    origin.ascii_serialization().as_bytes(),
                );
                Some(config_dir.join("webstorage").join(origin_uuid.to_string()))
            },
            None => None,
        }
    }

    fn add_new_environment(&mut self, origin: &ImmutableOrigin) -> Result<(), rusqlite::Error> {
        let origin_location = self.get_origin_location(origin);

        let engine = SqliteEngine::new(&origin_location, self.thread_pool.clone())?;
        let environment = WebStorageEnvironment::new(engine);
        self.environments.insert(origin.clone(), environment);
        Ok(())
    }

    fn get_environment(
        &mut self,
        origin: &ImmutableOrigin,
    ) -> Result<&WebStorageEnvironment<SqliteEngine>, rusqlite::Error> {
        if self.environments.contains_key(origin) {
            return Ok(self
                .environments
                .get(origin)
                .expect("environment should exist after contains_key check"));
        }

        self.add_new_environment(origin)?;

        Ok(self
            .environments
            .get(origin)
            .expect("environment should exist after add_new_environment"))
    }

    fn get_environment_mut(
        &mut self,
        origin: &ImmutableOrigin,
    ) -> Result<&mut WebStorageEnvironment<SqliteEngine>, rusqlite::Error> {
        if self.environments.contains_key(origin) {
            return Ok(self
                .environments
                .get_mut(origin)
                .expect("environment should exist after contains_key check"));
        }

        self.add_new_environment(origin)?;

        Ok(self
            .environments
            .get_mut(origin)
            .expect("environment should exist after add_new_environment"))
    }

    fn select_data(
        &mut self,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) -> Option<&OriginEntry> {
        match storage_type {
            WebStorageType::Session => self
                .session_data
                .get(&webview_id)
                .and_then(|origin_map| origin_map.get(&origin)),
            WebStorageType::Local => {
                // FIXME: Selecting data for read only operations should not
                // create a new origin descriptor. However, this currently
                // needs to happen because get_environment always creates an
                // environment, even for read only operations.
                if self.local_storage_origins.ensure_origin_descriptor(&origin) {
                    self.save_local_storage_origins();
                }
                match self.get_environment(&origin) {
                    Ok(env) => Some(&env.data),
                    Err(e) => {
                        warn!("Failed to get storage environment: {:?}", e);
                        None
                    },
                }
            },
        }
    }

    fn select_data_mut(
        &mut self,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) -> Option<&mut OriginEntry> {
        match storage_type {
            WebStorageType::Session => self
                .session_data
                .get_mut(&webview_id)
                .and_then(|origin_map| origin_map.get_mut(&origin)),
            WebStorageType::Local => {
                // FIXME: Selecting data for read only operations should not
                // create a new origin descriptor. However, this currently
                // needs to happen because get_environment always creates an
                // environment, even for read only operations.
                if self.local_storage_origins.ensure_origin_descriptor(&origin) {
                    self.save_local_storage_origins();
                }
                match self.get_environment_mut(&origin) {
                    Ok(env) => Some(&mut env.data),
                    Err(e) => {
                        warn!("Failed to get storage environment: {:?}", e);
                        None
                    },
                }
            },
        }
    }

    fn ensure_data_mut(
        &mut self,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) -> Option<&mut OriginEntry> {
        match storage_type {
            WebStorageType::Session => {
                self.session_storage_origins
                    .ensure_origin_descriptor(&origin);
                Some(
                    self.session_data
                        .entry(webview_id)
                        .or_default()
                        .entry(origin)
                        .or_default(),
                )
            },
            WebStorageType::Local => {
                if self.local_storage_origins.ensure_origin_descriptor(&origin) {
                    self.save_local_storage_origins();
                }
                match self.get_environment_mut(&origin) {
                    Ok(env) => Some(&mut env.data),
                    Err(e) => {
                        warn!("Failed to get storage environment: {:?}", e);
                        None
                    },
                }
            },
        }
    }

    fn length(
        &mut self,
        sender: GenericSender<usize>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) {
        let data = self.select_data(storage_type, webview_id, origin);
        sender
            .send(data.map_or(0, |entry| entry.inner().len()))
            .unwrap();
    }

    fn key(
        &mut self,
        sender: GenericSender<Option<String>>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
        index: u32,
    ) {
        let data = self.select_data(storage_type, webview_id, origin);
        let key = data
            .and_then(|entry| entry.inner().keys().nth(index as usize))
            .cloned();
        sender.send(key).unwrap();
    }

    fn keys(
        &mut self,
        sender: GenericSender<Vec<String>>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) {
        let data = self.select_data(storage_type, webview_id, origin);
        let keys = data.map_or(vec![], |entry| entry.inner().keys().cloned().collect());

        sender.send(keys).unwrap();
    }

    fn controlled_entry(
        &self,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: &ImmutableOrigin,
    ) -> Option<&OriginEntry> {
        match storage_type {
            WebStorageType::Session => self
                .session_data
                .get(&webview_id)
                .and_then(|origins| origins.get(origin)),
            WebStorageType::Local => self
                .environments
                .get(origin)
                .map(|environment| &environment.data),
        }
    }

    /// Validate the exact post-mutation state on a private projection before touching either
    /// storage area. The storage thread is sequential, so the subsequent insert linearizes
    /// against precisely this candidate.
    fn validate_controlled_set_candidate(
        &self,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: &ImmutableOrigin,
        name: &str,
        value: &str,
    ) -> Result<(), WebStorageStateError> {
        validate_entry(name, value)?;
        let mut snapshot = self.export_state(webview_id)?;
        let serialized_origin = origin.ascii_serialization().into_owned();
        let origin_index = match snapshot
            .origins
            .binary_search_by(|record| record.origin.cmp(&serialized_origin))
        {
            Ok(index) => index,
            Err(index) => {
                if snapshot.origins.len() == WEB_STORAGE_STATE_MAX_ORIGINS_V1 {
                    return Err(WebStorageStateError::TooManyOrigins);
                }
                snapshot.origins.insert(
                    index,
                    WebStorageOriginStateV1 {
                        origin: serialized_origin,
                        local_storage: Vec::new(),
                        session_storage: Vec::new(),
                    },
                );
                index
            },
        };
        let record = &mut snapshot.origins[origin_index];
        let entries = match storage_type {
            WebStorageType::Local => &mut record.local_storage,
            WebStorageType::Session => &mut record.session_storage,
        };
        match entries.binary_search_by(|entry| entry.key.as_str().cmp(name)) {
            Ok(index) => entries[index].value = value.to_owned(),
            Err(index) => {
                if entries.len() == WEB_STORAGE_STATE_MAX_ENTRIES_PER_AREA_V1 {
                    return Err(WebStorageStateError::TooManyEntries);
                }
                entries.insert(
                    index,
                    WebStorageStateEntryV1 {
                        key: name.to_owned(),
                        value: value.to_owned(),
                    },
                );
            },
        }
        validate_origin_bytes(
            &record.origin,
            &record.local_storage,
            &record.session_storage,
        )?;
        validate_snapshot_bytes(&snapshot)
    }

    /// Sends `Ok(changed, Some(old_value))` when a different value was replaced, or a typed
    /// capacity error before mutation.
    fn set_item(
        &mut self,
        sender: GenericSender<Result<(bool, Option<String>), WebStorageMutationError>>,
        storage_type: WebStorageType,
        mutation_policy: WebStorageMutationPolicy,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
        name: String,
        value: String,
    ) {
        if mutation_policy == WebStorageMutationPolicy::ControlledSessionV1 {
            let old_value = self
                .controlled_entry(storage_type, webview_id, &origin)
                .and_then(|entry| entry.inner().get(&name))
                .cloned();
            if old_value.as_deref() == Some(value.as_str()) {
                sender.send(Ok((false, None))).unwrap();
                return;
            }
            if self.revision_exhausted || self.revision.checked_add(1).is_none() {
                sender
                    .send(Err(WebStorageMutationError::RevisionExhausted))
                    .unwrap();
                return;
            }
            if self
                .validate_controlled_set_candidate(storage_type, webview_id, &origin, &name, &value)
                .is_err()
            {
                sender
                    .send(Err(WebStorageMutationError::ControlledStateLimit))
                    .unwrap();
                return;
            }

            let Some(entry) = self.ensure_data_mut(storage_type, webview_id, origin.clone()) else {
                sender
                    .send(Err(WebStorageMutationError::QuotaExceeded))
                    .unwrap();
                return;
            };
            let replaced = entry.insert(name.clone(), value.clone());
            debug_assert_eq!(replaced, old_value);
            if storage_type == WebStorageType::Local
                && let Ok(environment) = self.get_environment_mut(&origin)
            {
                environment.set(&name, &value);
            }
            self.bump_revision();
            sender.send(Ok((true, old_value))).unwrap();
            return;
        }

        let Some(entry) = self.ensure_data_mut(storage_type, webview_id, origin.clone()) else {
            sender
                .send(Err(WebStorageMutationError::QuotaExceeded))
                .unwrap();
            return;
        };
        let total_size = entry.size();

        let mut new_total_size = total_size + value.len();
        if let Some(old_value) = entry.inner().get(&name) {
            new_total_size -= old_value.len();
        } else {
            new_total_size += name.len();
        }

        let message = if new_total_size > QUOTA_SIZE_LIMIT {
            Err(WebStorageMutationError::QuotaExceeded)
        } else {
            let result =
                entry
                    .insert(name.clone(), value.clone())
                    .map_or(Ok((true, None)), |old| {
                        if old == value {
                            Ok((false, None))
                        } else {
                            Ok((true, Some(old)))
                        }
                    });
            if storage_type == WebStorageType::Local
                && let Ok(env) = self.get_environment_mut(&origin)
            {
                env.set(&name, &value);
            }
            result
        };
        let changed = matches!(message, Ok((true, _)));
        sender.send(message).unwrap();
        if changed {
            self.bump_revision();
        }
    }

    fn request_item(
        &mut self,
        sender: GenericSender<Option<String>>,
        storage_type: WebStorageType,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
        name: String,
    ) {
        let data = self.select_data(storage_type, webview_id, origin);
        sender
            .send(data.and_then(|entry| entry.inner().get(&name)).cloned())
            .unwrap();
    }

    /// Sends Some(old_value) in case there was a previous value with the key name, otherwise sends None
    fn remove_item(
        &mut self,
        sender: GenericSender<Option<String>>,
        storage_type: WebStorageType,
        mutation_policy: WebStorageMutationPolicy,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
        name: String,
    ) {
        if mutation_policy == WebStorageMutationPolicy::ControlledSessionV1 {
            let present = self
                .controlled_entry(storage_type, webview_id, &origin)
                .is_some_and(|entry| entry.inner().contains_key(&name));
            if !present || self.revision_exhausted || self.revision.checked_add(1).is_none() {
                sender.send(None).unwrap();
                return;
            }
        }
        let data = self.select_data_mut(storage_type, webview_id, origin.clone());
        let old_value = data.and_then(|entry| entry.remove(&name));
        let changed = old_value.is_some();
        sender.send(old_value).unwrap();
        if storage_type == WebStorageType::Local
            && let Ok(env) = self.get_environment_mut(&origin)
        {
            env.delete(&name);
        }
        if changed {
            self.bump_revision();
        }
    }

    fn clear(
        &mut self,
        sender: GenericSender<bool>,
        storage_type: WebStorageType,
        mutation_policy: WebStorageMutationPolicy,
        webview_id: WebViewId,
        origin: ImmutableOrigin,
    ) {
        if mutation_policy == WebStorageMutationPolicy::ControlledSessionV1 {
            let present = self
                .controlled_entry(storage_type, webview_id, &origin)
                .is_some_and(|entry| !entry.inner().is_empty());
            if !present || self.revision_exhausted || self.revision.checked_add(1).is_none() {
                sender.send(false).unwrap();
                return;
            }
        }
        let data = self.select_data_mut(storage_type, webview_id, origin.clone());
        let changed = data.is_some_and(|entry| {
            if !entry.inner().is_empty() {
                entry.clear();
                true
            } else {
                false
            }
        });
        sender.send(changed).unwrap();
        if storage_type == WebStorageType::Local
            && let Ok(env) = self.get_environment_mut(&origin)
        {
            env.clear();
        }
        if changed {
            self.bump_revision();
        }
    }

    fn clone(&mut self, src_webview_id: WebViewId, dest_webview_id: WebViewId) {
        let Some(src_origin_entries) = self.session_data.get(&src_webview_id) else {
            return;
        };

        let dest_origin_entries = src_origin_entries.clone();
        self.session_data
            .insert(dest_webview_id, dest_origin_entries);
        self.bump_revision();
    }

    fn origin_descriptors(&mut self, storage_type: WebStorageType) -> Vec<OriginDescriptor> {
        match storage_type {
            WebStorageType::Session => self.session_storage_origins.origin_descriptors(),
            WebStorageType::Local => self.local_storage_origins.origin_descriptors(),
        }
    }

    fn clear_data_for_sites(&mut self, storage_type: WebStorageType, sites: &[String]) {
        let old_entry_count = self.entry_count(storage_type);
        match storage_type {
            WebStorageType::Session => {
                let origins = self.session_storage_origins.take_origins_for_sites(sites);

                self.session_data.retain(|_, origins_map| {
                    for origin in &origins {
                        origins_map.remove(origin);
                    }
                    !origins_map.is_empty()
                });
            },
            WebStorageType::Local => {
                let origins = self.local_storage_origins.take_origins_for_sites(sites);

                if self.config_dir.is_some() {
                    for origin in origins {
                        self.environments.remove(&origin);

                        let origin_location = self
                            .get_origin_location(&origin)
                            .expect("Should always be able to get origin location.");

                        if let Err(error) = std::fs::remove_dir_all(&origin_location) {
                            warn!("Failed to delete origin location: {:?}", error);
                            self.local_storage_origins.ensure_origin_descriptor(&origin);
                        }
                    }

                    self.save_local_storage_origins();
                } else {
                    for origin in origins {
                        self.environments.remove(&origin);
                    }
                }
            },
        }
        if self.entry_count(storage_type) != old_entry_count {
            self.bump_revision();
        }
    }

    fn export_state(
        &self,
        webview_id: WebViewId,
    ) -> Result<WebStorageStateSnapshotV1, WebStorageStateError> {
        if self.config_dir.is_some() {
            return Err(WebStorageStateError::PersistentBackendUnsupported);
        }
        if self.revision_exhausted {
            return Err(WebStorageStateError::RevisionExhausted);
        }

        let mut origins: BTreeMap<String, (Option<&OriginEntry>, Option<&OriginEntry>)> =
            BTreeMap::new();
        for (origin, environment) in &self.environments {
            if !environment.data.inner().is_empty() {
                origins
                    .entry(origin.ascii_serialization().into_owned())
                    .or_default()
                    .0 = Some(&environment.data);
            }
        }
        if let Some(session) = self.session_data.get(&webview_id) {
            for (origin, entry) in session {
                if !entry.inner().is_empty() {
                    origins
                        .entry(origin.ascii_serialization().into_owned())
                        .or_default()
                        .1 = Some(entry);
                }
            }
        }
        if origins.len() > WEB_STORAGE_STATE_MAX_ORIGINS_V1 {
            return Err(WebStorageStateError::TooManyOrigins);
        }

        let mut records = Vec::with_capacity(origins.len());
        for (origin, (local, session)) in origins {
            let local_storage = project_entries(local)?;
            let session_storage = project_entries(session)?;
            validate_origin_bytes(&origin, &local_storage, &session_storage)?;
            records.push(WebStorageOriginStateV1 {
                origin,
                local_storage,
                session_storage,
            });
        }
        let snapshot = WebStorageStateSnapshotV1 {
            schema_version: WEB_STORAGE_STATE_SCHEMA_VERSION_V1,
            revision: self.revision,
            origins: records,
        };
        validate_snapshot_bytes(&snapshot)?;
        Ok(snapshot)
    }

    fn replace_state(
        &mut self,
        webview_id: WebViewId,
        expected_revision: u64,
        snapshot: WebStorageStateSnapshotV1,
    ) -> Result<u64, WebStorageStateError> {
        if self.config_dir.is_some() {
            return Err(WebStorageStateError::PersistentBackendUnsupported);
        }
        if self.revision_exhausted {
            return Err(WebStorageStateError::RevisionExhausted);
        }
        if expected_revision != self.revision {
            return Err(WebStorageStateError::StaleRevision);
        }
        let next_revision = self
            .revision
            .checked_add(1)
            .ok_or(WebStorageStateError::RevisionExhausted)?;
        if snapshot.schema_version != WEB_STORAGE_STATE_SCHEMA_VERSION_V1 {
            return Err(WebStorageStateError::UnsupportedSchemaVersion);
        }
        if snapshot.origins.len() > WEB_STORAGE_STATE_MAX_ORIGINS_V1 {
            return Err(WebStorageStateError::TooManyOrigins);
        }
        validate_snapshot_bytes(&snapshot)?;

        let mut seen_origins = HashSet::new();
        let mut replacement_local = FxHashMap::default();
        let mut replacement_session = FxHashMap::default();
        for record in snapshot.origins {
            let origin = parse_state_origin(&record.origin)?;
            if !seen_origins.insert(origin.clone()) {
                return Err(WebStorageStateError::DuplicateOrigin);
            }
            validate_origin_bytes(
                &record.origin,
                &record.local_storage,
                &record.session_storage,
            )?;
            let local = import_entries(record.local_storage)?;
            let session = import_entries(record.session_storage)?;

            if !local.inner().is_empty() {
                let engine = SqliteEngine::new(&None, self.thread_pool.clone())
                    .map_err(|_| WebStorageStateError::BackendFailure)?;
                replacement_local.insert(
                    origin.clone(),
                    WebStorageEnvironment {
                        engine,
                        data: local,
                    },
                );
            }
            if !session.inner().is_empty() {
                replacement_session.insert(origin, session);
            }
        }

        self.environments = replacement_local;
        if replacement_session.is_empty() {
            self.session_data.remove(&webview_id);
        } else {
            self.session_data.insert(webview_id, replacement_session);
        }
        self.rebuild_origin_descriptors();
        self.revision = next_revision;
        Ok(self.revision)
    }

    fn rebuild_origin_descriptors(&mut self) {
        let mut local = StorageOrigins::new();
        for origin in self.environments.keys() {
            local.ensure_origin_descriptor(origin);
        }
        self.local_storage_origins = local;

        let mut session = StorageOrigins::new();
        for origins in self.session_data.values() {
            for origin in origins.keys() {
                session.ensure_origin_descriptor(origin);
            }
        }
        self.session_storage_origins = session;
    }

    fn entry_count(&self, storage_type: WebStorageType) -> usize {
        match storage_type {
            WebStorageType::Session => self
                .session_data
                .values()
                .flat_map(|origins| origins.values())
                .map(|entry| entry.inner().len())
                .sum(),
            WebStorageType::Local => self
                .environments
                .values()
                .map(|environment| environment.data.inner().len())
                .sum(),
        }
    }
}

fn project_entries(
    entry: Option<&OriginEntry>,
) -> Result<Vec<WebStorageStateEntryV1>, WebStorageStateError> {
    let Some(entry) = entry else {
        return Ok(Vec::new());
    };
    if entry.inner().len() > WEB_STORAGE_STATE_MAX_ENTRIES_PER_AREA_V1 {
        return Err(WebStorageStateError::TooManyEntries);
    }
    entry
        .inner()
        .iter()
        .map(|(key, value)| {
            validate_entry(key, value)?;
            Ok(WebStorageStateEntryV1 {
                key: key.clone(),
                value: value.clone(),
            })
        })
        .collect()
}

fn import_entries(
    records: Vec<WebStorageStateEntryV1>,
) -> Result<OriginEntry, WebStorageStateError> {
    if records.len() > WEB_STORAGE_STATE_MAX_ENTRIES_PER_AREA_V1 {
        return Err(WebStorageStateError::TooManyEntries);
    }
    let mut entry = OriginEntry::default();
    for record in records {
        validate_entry(&record.key, &record.value)?;
        if entry.insert(record.key, record.value).is_some() {
            return Err(WebStorageStateError::DuplicateKey);
        }
    }
    Ok(entry)
}

fn validate_entry(key: &str, value: &str) -> Result<(), WebStorageStateError> {
    if key.len() > WEB_STORAGE_STATE_MAX_KEY_BYTES_V1 {
        return Err(WebStorageStateError::KeyTooLarge);
    }
    if value.len() > WEB_STORAGE_STATE_MAX_VALUE_BYTES_V1 {
        return Err(WebStorageStateError::ValueTooLarge);
    }
    Ok(())
}

fn validate_origin_bytes(
    origin: &str,
    local: &[WebStorageStateEntryV1],
    session: &[WebStorageStateEntryV1],
) -> Result<(), WebStorageStateError> {
    let bytes = local
        .iter()
        .chain(session)
        .try_fold(origin.len(), |total, entry| {
            total
                .checked_add(entry.key.len())
                .and_then(|total| total.checked_add(entry.value.len()))
        })
        .ok_or(WebStorageStateError::OriginTooLarge)?;
    if bytes > WEB_STORAGE_STATE_MAX_ORIGIN_BYTES_V1 {
        return Err(WebStorageStateError::OriginTooLarge);
    }
    Ok(())
}

fn validate_snapshot_bytes(
    snapshot: &WebStorageStateSnapshotV1,
) -> Result<(), WebStorageStateError> {
    let bytes = postcard::to_stdvec(snapshot).map_err(|_| WebStorageStateError::BackendFailure)?;
    if bytes.len() > WEB_STORAGE_STATE_MAX_TOTAL_BYTES_V1 {
        return Err(WebStorageStateError::SnapshotTooLarge);
    }
    let public_json =
        serde_json::to_vec(&snapshot.origins).map_err(|_| WebStorageStateError::BackendFailure)?;
    if public_json.len() > WEB_STORAGE_STATE_MAX_PUBLIC_JSON_BYTES_V1 {
        return Err(WebStorageStateError::SnapshotTooLarge);
    }
    Ok(())
}

fn parse_state_origin(origin: &str) -> Result<ImmutableOrigin, WebStorageStateError> {
    let url = ServoUrl::parse(origin).map_err(|_| WebStorageStateError::InvalidOrigin)?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(WebStorageStateError::InvalidOrigin);
    }
    let parsed = url.origin();
    if !parsed.is_tuple() || parsed.ascii_serialization().as_ref() != origin {
        return Err(WebStorageStateError::InvalidOrigin);
    }
    Ok(parsed)
}
