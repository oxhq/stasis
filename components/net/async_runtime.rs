/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use futures::Future;
use net_traits::AsyncRuntime;
use tokio::runtime::{Builder, Handle, Runtime};

/// The actual runtime,
/// to be used as part of shut-down.
pub struct AsyncRuntimeHolder {
    runtime: Option<Runtime>,
}

impl AsyncRuntimeHolder {
    pub(crate) fn new(runtime: Runtime) -> Self {
        Self {
            runtime: Some(runtime),
        }
    }
}

impl AsyncRuntime for AsyncRuntimeHolder {
    fn shutdown(&mut self) {
        // Dropping a Runtime outside one of its own async contexts waits for its worker threads to
        // terminate. A bounded shutdown timeout deliberately leaks unfinished worker threads,
        // which is not a physical lifecycle boundary for process-global memory/JS teardown.
        drop(
            self.runtime
                .take()
                .expect("Runtime should have been initialized on start-up."),
        );
    }
}

#[cfg(test)]
mod thread_ownership_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    use futures::future;
    use net_traits::AsyncRuntime;
    use tokio::runtime::Builder;

    use super::AsyncRuntimeHolder;

    struct DropSentinel(Arc<AtomicBool>);

    impl Drop for DropSentinel {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn shutdown_waits_for_worker_owned_task_drop() {
        let runtime = Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("the deterministic test runtime should build");
        let dropped = Arc::new(AtomicBool::new(false));
        let task_dropped = Arc::clone(&dropped);
        let (entered_sender, entered_receiver) = mpsc::sync_channel(0);
        runtime.spawn(async move {
            let _task_state = DropSentinel(task_dropped);
            entered_sender
                .send(())
                .expect("the test must observe the live runtime task");
            future::pending::<()>().await;
        });
        entered_receiver
            .recv()
            .expect("the runtime task must start without scheduler timing assumptions");

        let mut owner = AsyncRuntimeHolder::new(runtime);
        owner.shutdown();

        assert!(
            dropped.load(Ordering::SeqCst),
            "shutdown must not return before worker-owned task state is dropped"
        );
    }
}

/// A shared handle to the runtime,
/// to be initialized on start-up.
static ASYNC_RUNTIME_HANDLE: OnceLock<Handle> = OnceLock::new();

pub fn init_async_runtime() -> Box<dyn AsyncRuntime> {
    // Initialize a tokio runtime.
    let runtime = Builder::new_multi_thread()
        .thread_name_fn(|| {
            static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
            let id = ATOMIC_ID.fetch_add(1, Ordering::Relaxed);
            format!("tokio-runtime-{}", id)
        })
        .worker_threads(
            thread::available_parallelism()
                .map(|i| i.get())
                .unwrap_or(servo_config::pref!(thread_pool_fallback_workers) as usize)
                .min(servo_config::pref!(thread_pool_async_runtime_workers_max).max(1) as usize),
        )
        .enable_io()
        .enable_time()
        .build()
        .expect("Unable to build tokio-runtime runtime");

    // Make the runtime available to users inside this crate.
    ASYNC_RUNTIME_HANDLE
        .set(runtime.handle().clone())
        .expect("Runtime handle should be initialized once on start-up");

    // Return an async runtime for use in shutdown.
    Box::new(AsyncRuntimeHolder::new(runtime))
}

pub fn async_runtime_initialized() -> bool {
    ASYNC_RUNTIME_HANDLE.get().is_some()
}

/// Spawn a task using the handle to the runtime.
pub fn spawn_task<F>(task: F)
where
    F: Future + 'static + std::marker::Send,
    F::Output: Send + 'static,
{
    ASYNC_RUNTIME_HANDLE
        .get()
        .expect("Runtime handle should be initialized on start-up")
        .spawn(task);
}

/// Spawn a blocking task using the handle to the runtime.
pub fn spawn_blocking_task<F, R>(task: F) -> F::Output
where
    F: Future,
{
    ASYNC_RUNTIME_HANDLE
        .get()
        .expect("Runtime handle should be initialized on start-up")
        .block_on(task)
}
