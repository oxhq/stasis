/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::error::Error;
use std::path::PathBuf;
use std::{fmt, thread};

use profile_traits::mem::ProfilerChan as MemProfilerChan;
use servo_base::generic_channel::GenericSender;
use storage_traits::StorageThreads;
use storage_traits::cache_storage::CacheStorageThreadHandle;
use storage_traits::client_storage::ClientStorageThreadHandle;
use storage_traits::indexeddb::IndexedDBThreadMsg;
use storage_traits::webstorage_thread::WebStorageThreadMsg;

use crate::cache_storage::new_cache_storage_thread;
use crate::client_storage::new_client_storage_thread;
use crate::indexeddb::new_indexeddb_thread;
use crate::webstorage::new_webstorage_thread;

struct OwnedStorageThread {
    name: String,
    handle: thread::JoinHandle<()>,
}

/// Physical ownership of the storage manager threads created for one Servo instance.
///
/// The storage protocol's exit acknowledgements are sent before each manager's
/// thread-owned state is dropped. Call [`StorageThreadOwner::join`] only after those
/// acknowledgements have been received to close that physical-shutdown gap.
#[must_use = "storage manager threads remain physically unowned until this owner is joined"]
pub struct StorageThreadOwner {
    threads: Vec<OwnedStorageThread>,
}

impl StorageThreadOwner {
    fn new(threads: Vec<OwnedStorageThread>) -> Self {
        Self { threads }
    }

    #[cfg(test)]
    pub(crate) fn from_test_thread(name: &str, handle: thread::JoinHandle<()>) -> Self {
        Self::new(vec![OwnedStorageThread {
            name: name.to_owned(),
            handle,
        }])
    }

    /// Wait for every owned storage manager thread to finish.
    ///
    /// All handles are joined even when one or more threads panicked.
    pub fn join(self) -> Result<(), StorageThreadJoinError> {
        let mut failed_thread_names = Vec::new();

        for thread in self.threads {
            if thread.handle.join().is_err() {
                failed_thread_names.push(thread.name);
            }
        }

        if failed_thread_names.is_empty() {
            Ok(())
        } else {
            Err(StorageThreadJoinError {
                failed_thread_names,
            })
        }
    }

    pub fn thread_count(&self) -> usize {
        self.threads.len()
    }

    #[cfg(test)]
    pub(crate) fn all_finished(&self) -> bool {
        self.threads
            .iter()
            .all(|thread| thread.handle.is_finished())
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct StorageThreadJoinError {
    failed_thread_names: Vec<String>,
}

impl StorageThreadJoinError {
    pub fn failed_thread_names(&self) -> &[String] {
        &self.failed_thread_names
    }
}

impl fmt::Display for StorageThreadJoinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "storage manager threads panicked during shutdown: {}",
            self.failed_thread_names.join(", ")
        )
    }
}

impl Error for StorageThreadJoinError {}

fn new_storage_thread_group(
    mem_profiler_chan: MemProfilerChan,
    config_dir: Option<PathBuf>,
    temporary_storage: bool,
    label: &str,
) -> (StorageThreads, Vec<OwnedStorageThread>) {
    let (client_storage, client_storage_thread): (
        ClientStorageThreadHandle,
        thread::JoinHandle<()>,
    ) = new_client_storage_thread(config_dir.clone(), temporary_storage);
    let (idb, indexeddb_thread): (GenericSender<IndexedDBThreadMsg>, thread::JoinHandle<()>) =
        new_indexeddb_thread(
            mem_profiler_chan.clone(),
            format!("indexedDB-reporter-{label}"),
        );
    let (web_storage, webstorage_thread): (
        GenericSender<WebStorageThreadMsg>,
        thread::JoinHandle<()>,
    ) = new_webstorage_thread(
        config_dir.clone(),
        mem_profiler_chan,
        format!("storage-reporter-{label}"),
    );
    let (cache_storage, cache_storage_thread): (CacheStorageThreadHandle, thread::JoinHandle<()>) =
        new_cache_storage_thread(config_dir, temporary_storage);

    (
        StorageThreads::new(
            client_storage.into(),
            idb,
            web_storage,
            cache_storage.into(),
        ),
        vec![
            OwnedStorageThread {
                name: format!("{label} ClientStorageThread"),
                handle: client_storage_thread,
            },
            OwnedStorageThread {
                name: format!("{label} IndexedDBManager"),
                handle: indexeddb_thread,
            },
            OwnedStorageThread {
                name: format!("{label} WebStorageManager"),
                handle: webstorage_thread,
            },
            OwnedStorageThread {
                name: format!("{label} CacheStorageThread"),
                handle: cache_storage_thread,
            },
        ],
    )
}

/// Compatibility constructor for callers that do not yet own physical thread shutdown.
/// Production callers should use [`new_storage_threads_with_owner`].
pub fn new_storage_threads(
    mem_profiler_chan: MemProfilerChan,
    config_dir: Option<PathBuf>,
    temporary_storage: bool,
) -> (StorageThreads, StorageThreads) {
    let (private_storage_threads, public_storage_threads, _owner) =
        new_storage_threads_with_owner(mem_profiler_chan, config_dir, temporary_storage);

    (private_storage_threads, public_storage_threads)
}

pub fn new_storage_threads_with_owner(
    mem_profiler_chan: MemProfilerChan,
    config_dir: Option<PathBuf>,
    temporary_storage: bool,
) -> (StorageThreads, StorageThreads, StorageThreadOwner) {
    let (private_storage_threads, mut private_threads) = new_storage_thread_group(
        mem_profiler_chan.clone(),
        config_dir.clone(),
        temporary_storage,
        "private",
    );
    let (public_storage_threads, public_threads) =
        new_storage_thread_group(mem_profiler_chan, config_dir, temporary_storage, "public");
    private_threads.extend(public_threads);

    (
        private_storage_threads,
        public_storage_threads,
        StorageThreadOwner::new(private_threads),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_attempts_every_join_and_reports_every_failure() {
        let first = thread::spawn(|| panic!("first expected storage test panic"));
        let second = thread::spawn(|| panic!("second expected storage test panic"));
        let owner = StorageThreadOwner::new(vec![
            OwnedStorageThread {
                name: "first".to_owned(),
                handle: first,
            },
            OwnedStorageThread {
                name: "second".to_owned(),
                handle: second,
            },
        ]);

        let error = owner.join().unwrap_err();
        assert_eq!(
            error.failed_thread_names(),
            &["first".to_owned(), "second".to_owned()]
        );
    }
}
