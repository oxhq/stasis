/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::any::Any;
use std::io;
use std::mem;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};

use servo_config::pref;

type ThreadFailure = Box<dyn Any + Send + 'static>;

/// Admission and physical ownership state for the shared Rayon pool.
struct ThreadPoolState {
    pool: Option<rayon::ThreadPool>,
    accepting_work: bool,
}

/// Retains every OS worker handle and every unhandled task panic that Rayon would otherwise
/// detach from the lifecycle owner.
struct RayonWorkerOwner {
    handles: Mutex<Vec<JoinHandle<()>>>,
    task_failures: Mutex<Vec<ThreadFailure>>,
}

impl RayonWorkerOwner {
    fn new() -> Self {
        Self {
            handles: Mutex::new(Vec::new()),
            task_failures: Mutex::new(Vec::new()),
        }
    }

    fn spawn(&self, rayon_thread: rayon::ThreadBuilder) -> io::Result<()> {
        let mut builder = thread::Builder::new();
        if let Some(name) = rayon_thread.name() {
            builder = builder.name(name.to_owned());
        }
        if let Some(stack_size) = rayon_thread.stack_size() {
            builder = builder.stack_size(stack_size);
        }

        let handle = builder.spawn(move || rayon_thread.run())?;
        self.handles
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(handle);
        Ok(())
    }

    fn record_task_failure(&self, payload: ThreadFailure) {
        self.task_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .push(payload);
    }

    fn drain_first_task_failure(&self) -> Option<ThreadFailure> {
        mem::take(
            &mut *self
                .task_failures
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
        .into_iter()
        .next()
    }

    /// Join every retained worker, continuing after a failure and returning the first payload.
    fn join_all(&self) -> thread::Result<()> {
        let handles = mem::take(
            &mut *self
                .handles
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        );
        let mut first_failure = None;

        for handle in handles {
            if let Err(payload) = handle.join()
                && first_failure.is_none()
            {
                first_failure = Some(payload);
            }
        }

        match first_failure {
            Some(payload) => Err(payload),
            None => Ok(()),
        }
    }
}

/// The thread pool used throughout Servo, apart from Layout and WebRender which are owned
/// separately.
pub struct ThreadPool {
    state: Mutex<ThreadPoolState>,
    worker_owner: Arc<RayonWorkerOwner>,
}

static GLOBAL_THREAD_POOL: OnceLock<Arc<ThreadPool>> = OnceLock::new();

impl ThreadPool {
    fn new(worker_count: usize) -> Result<Self, rayon::ThreadPoolBuildError> {
        let worker_owner = Arc::new(RayonWorkerOwner::new());
        let spawn_owner = Arc::clone(&worker_owner);
        let panic_owner = Arc::clone(&worker_owner);
        let pool = rayon::ThreadPoolBuilder::new()
            .thread_name(move |index| format!("GlobalPool#{index}"))
            .num_threads(worker_count)
            .spawn_handler(move |rayon_thread| spawn_owner.spawn(rayon_thread))
            .panic_handler(move |payload| panic_owner.record_task_failure(payload))
            .build()?;

        Ok(Self {
            state: Mutex::new(ThreadPoolState {
                pool: Some(pool),
                accepting_work: true,
            }),
            worker_owner,
        })
    }

    /// Get the global thread pool for the process.
    pub fn global() -> Arc<Self> {
        GLOBAL_THREAD_POOL
            .get_or_init(|| {
                let parallelism = thread::available_parallelism()
                    .map(|parallelism| parallelism.get())
                    .unwrap_or(pref!(thread_pool_fallback_workers) as usize)
                    .min(pref!(thread_pool_workers_max) as usize);
                Arc::new(
                    Self::new(parallelism).expect("failed to initialize the global thread pool"),
                )
            })
            .clone()
    }

    /// Spawn work on the thread pool while its lifecycle owner is accepting work.
    ///
    /// Rejection needs no caller feedback because it only happens after process-wide subsystem
    /// shutdown has begun.
    pub fn spawn<OP>(&self, work: OP)
    where
        OP: FnOnce() + Send + 'static,
    {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !state.accepting_work {
            return;
        }
        if let Some(pool) = state.pool.as_ref() {
            pool.spawn(work);
        }
    }

    fn shutdown_with_boundary<F>(&self, after_admission_closed: F) -> thread::Result<()>
    where
        F: FnOnce(),
    {
        let pool = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.accepting_work = false;
            state.pool.take()
        };

        after_admission_closed();
        drop(pool);
        let worker_failure = self.worker_owner.join_all().err();
        let task_failure = self.worker_owner.drain_first_task_failure();

        match task_failure.or(worker_failure) {
            Some(payload) => Err(payload),
            None => Ok(()),
        }
    }

    fn shutdown(&self) -> thread::Result<()> {
        self.shutdown_with_boundary(|| {})
    }

    /// Prevent new work and physically join every accepted task and OS worker.
    ///
    /// This legacy entry point preserves the previous unit-returning API while surfacing a worker
    /// failure as a panic to its caller.
    pub fn exit(&self) {
        if let Err(payload) = self.shutdown() {
            std::panic::resume_unwind(payload);
        }
    }

    /// Shut down the process-global pool if it was initialized, without creating it during exit.
    pub fn shutdown_global() -> thread::Result<()> {
        match GLOBAL_THREAD_POOL.get() {
            Some(pool) => pool.shutdown(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod global_thread_pool_ownership_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::{self, TryRecvError};
    use std::thread;

    use super::{RayonWorkerOwner, ThreadPool};

    struct DropSentinel(Arc<AtomicBool>);

    impl Drop for DropSentinel {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn shutdown_fences_accepted_work_and_physical_worker_drop() {
        let pool = Arc::new(ThreadPool::new(1).expect("the fixed-size test pool should build"));
        let task_dropped = Arc::new(AtomicBool::new(false));
        let task_sentinel = Arc::clone(&task_dropped);
        let (entered_sender, entered_receiver) = mpsc::sync_channel(0);
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        pool.spawn(move || {
            let _sentinel = DropSentinel(task_sentinel);
            entered_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
        });
        entered_receiver.recv().unwrap();

        let shutdown_pool = Arc::clone(&pool);
        let (boundary_sender, boundary_receiver) = mpsc::sync_channel(0);
        let (completed_sender, completed_receiver) = mpsc::sync_channel(1);
        let shutdown_thread = thread::spawn(move || {
            let result = shutdown_pool.shutdown_with_boundary(|| {
                boundary_sender.send(()).unwrap();
            });
            completed_sender.send(result.is_ok()).unwrap();
        });

        boundary_receiver.recv().unwrap();
        assert_eq!(completed_receiver.try_recv(), Err(TryRecvError::Empty));
        assert!(!task_dropped.load(Ordering::SeqCst));
        release_sender.send(()).unwrap();
        assert_eq!(completed_receiver.recv(), Ok(true));
        shutdown_thread.join().unwrap();
        assert!(task_dropped.load(Ordering::SeqCst));
        assert!(pool.shutdown().is_ok(), "shutdown must be idempotent");
    }

    #[test]
    fn shutdown_surfaces_first_and_drains_every_unhandled_task_panic() {
        struct SecondaryFailure(Arc<AtomicBool>);

        impl Drop for SecondaryFailure {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let pool = ThreadPool::new(1).expect("the fixed-size test pool should build");
        let (entered_sender, entered_receiver) = mpsc::sync_channel(0);
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        pool.spawn(move || {
            entered_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            std::panic::panic_any("deterministic global-pool task failure");
        });
        entered_receiver.recv().unwrap();
        let secondary_dropped = Arc::new(AtomicBool::new(false));
        let secondary_payload = Arc::clone(&secondary_dropped);
        pool.spawn(move || {
            std::panic::panic_any(SecondaryFailure(secondary_payload));
        });
        release_sender.send(()).unwrap();

        let failure = pool
            .shutdown()
            .expect_err("the unhandled task panic must remain observable at shutdown");
        assert_eq!(
            failure.downcast_ref::<&'static str>(),
            Some(&"deterministic global-pool task failure")
        );
        assert!(secondary_dropped.load(Ordering::SeqCst));
        assert!(pool.shutdown().is_ok(), "shutdown must be idempotent");
    }

    #[test]
    fn worker_join_continues_after_first_panicking_handle() {
        let owner = RayonWorkerOwner::new();
        let later_worker_dropped = Arc::new(AtomicBool::new(false));
        let later_sentinel = Arc::clone(&later_worker_dropped);
        let mut handles = owner.handles.lock().unwrap();
        handles.push(thread::spawn(|| {
            std::panic::panic_any("deterministic first worker failure")
        }));
        handles.push(thread::spawn(move || {
            let _sentinel = DropSentinel(later_sentinel);
        }));
        drop(handles);

        assert!(owner.join_all().is_err());
        assert!(later_worker_dropped.load(Ordering::SeqCst));
    }
}
