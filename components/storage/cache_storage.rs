/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::fmt::Debug;
use std::path::PathBuf;
use std::thread;

use log::error;
use servo_base::generic_channel::{self, GenericReceiver, GenericSender};
use storage_traits::cache_storage::{
    CacheStorageError, CacheStorageThreadHandle, CacheStorageThreadMessage,
    CacheStorageThreadResponse,
};

trait CacheStorageEngine {
    type Error: Debug;

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-has>
    fn has_cache(&mut self, cache_name: &str) -> Result<bool, CacheStorageError<Self::Error>>;
}

pub struct DummyCacheStorageEngine;

impl CacheStorageEngine for DummyCacheStorageEngine {
    type Error = ();

    /// <https://w3c.github.io/ServiceWorker/#cache-storage-has>
    /// The parallel steps.
    fn has_cache(&mut self, _cache_name: &str) -> Result<bool, CacheStorageError<Self::Error>> {
        // TODO: implement.
        // Step 2.1:For each key → value of the relevant name to cache map:
        // Step 2.1.1: If cacheName matches key, resolve promise with true and abort these steps.
        // Step 2.2: Resolve promise with false.
        // Note: promise resolved in the callback in CacheStorage.
        Ok(false)
    }
}

pub trait CacheStorageThreadFactory {
    fn new(config_dir: Option<PathBuf>, temporary_storage: bool) -> Self;
}

pub(crate) fn new_cache_storage_thread(
    config_dir: Option<PathBuf>,
    temporary_storage: bool,
) -> (CacheStorageThreadHandle, thread::JoinHandle<()>) {
    let (generic_sender, generic_receiver) = generic_channel::channel().unwrap();
    let mut temp_dir: Option<tempfile::TempDir> = None;
    let base_dir = config_dir
        .unwrap_or_else(|| {
            let tmp_dir = tempfile::tempdir().unwrap();
            let path = tmp_dir.path().to_path_buf();
            temp_dir = Some(tmp_dir);
            path
        })
        .join("cachestorage");
    let storage_dir = if temporary_storage {
        let unique_id = uuid::Uuid::new_v4().to_string();
        base_dir.join("temporary").join(unique_id)
    } else {
        base_dir.join("default_v1")
    };
    std::fs::create_dir_all(&storage_dir).expect("Failed to create CacheStorage storage directory");
    let sender_clone = generic_sender.clone();
    let thread = thread::Builder::new()
        .name("CacheStorageThread".to_owned())
        .spawn(move || {
            // Keep temp_dir alive while the thread runs.
            let _ = temp_dir;
            let engine = DummyCacheStorageEngine;
            CacheStorageThread::new(sender_clone, generic_receiver, engine).start();
        })
        .expect("Thread spawning failed");

    (CacheStorageThreadHandle::new(generic_sender), thread)
}

impl CacheStorageThreadFactory for CacheStorageThreadHandle {
    fn new(config_dir: Option<PathBuf>, temporary_storage: bool) -> CacheStorageThreadHandle {
        new_cache_storage_thread(config_dir, temporary_storage).0
    }
}

struct CacheStorageThread<E: CacheStorageEngine> {
    receiver: GenericReceiver<CacheStorageThreadMessage>,
    // Note: a sender to self might be required later for the storage engine.
    _sender: GenericSender<CacheStorageThreadMessage>,
    engine: E,
}

impl<E> CacheStorageThread<E>
where
    E: CacheStorageEngine,
{
    pub fn new(
        _sender: GenericSender<CacheStorageThreadMessage>,
        receiver: GenericReceiver<CacheStorageThreadMessage>,
        engine: E,
    ) -> CacheStorageThread<E> {
        CacheStorageThread {
            _sender,
            receiver,
            engine,
        }
    }

    pub fn start(&mut self) {
        while let Ok(message) = self.receiver.recv() {
            match message {
                CacheStorageThreadMessage::HasCache {
                    cache_name,
                    callback,
                    proxy: _,
                    origin: _,
                } => {
                    let result = self.engine.has_cache(&cache_name);
                    if callback
                        .send(CacheStorageThreadResponse::HasCacheResult(
                            result.map(|_| false).map_err(|e| format!("{:?}", e)),
                        ))
                        .is_err()
                    {
                        error!("Failed to send response to script for HasCache message.");
                    }
                },
                CacheStorageThreadMessage::Exit(sender) => {
                    let _ = sender.send(());
                    break;
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};

    use super::*;
    use crate::storage_thread::StorageThreadOwner;

    struct BlockingDrop {
        started: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        finished: Arc<AtomicBool>,
    }

    impl Drop for BlockingDrop {
        fn drop(&mut self) {
            self.started.send(()).unwrap();
            self.release.recv().unwrap();
            self.finished.store(true, Ordering::SeqCst);
        }
    }

    struct BlockingDropEngine {
        _thread_owned_state: BlockingDrop,
    }

    impl CacheStorageEngine for BlockingDropEngine {
        type Error = ();

        fn has_cache(&mut self, _cache_name: &str) -> Result<bool, CacheStorageError<Self::Error>> {
            Ok(false)
        }
    }

    #[test]
    fn owner_join_closes_exit_acknowledgement_before_physical_drop_gap() {
        let (thread_sender, thread_receiver) = generic_channel::channel().unwrap();
        let sender_clone = thread_sender.clone();
        let (drop_started_sender, drop_started_receiver) = mpsc::channel();
        let (release_drop_sender, release_drop_receiver) = mpsc::channel();
        let drop_finished = Arc::new(AtomicBool::new(false));
        let thread_drop_finished = Arc::clone(&drop_finished);
        let handle = thread::spawn(move || {
            CacheStorageThread::new(
                sender_clone,
                thread_receiver,
                BlockingDropEngine {
                    _thread_owned_state: BlockingDrop {
                        started: drop_started_sender,
                        release: release_drop_receiver,
                        finished: thread_drop_finished,
                    },
                },
            )
            .start();
        });
        let owner = StorageThreadOwner::from_test_thread("controlled CacheStorageThread", handle);

        let (acknowledged_sender, acknowledged_receiver) = generic_channel::channel().unwrap();
        thread_sender
            .send(CacheStorageThreadMessage::Exit(acknowledged_sender))
            .unwrap();
        acknowledged_receiver.recv().unwrap();
        drop_started_receiver.recv().unwrap();

        assert!(!owner.all_finished());
        assert!(!drop_finished.load(Ordering::SeqCst));

        release_drop_sender.send(()).unwrap();
        owner.join().unwrap();
        assert!(drop_finished.load(Ordering::SeqCst));
    }
}
