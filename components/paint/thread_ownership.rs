/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Physical ownership of the custom Rayon workers supplied to WebRender.

use std::io;
use std::mem;
use std::sync::Mutex;
use std::thread::{self, JoinHandle};

/// Retains the OS thread handles that Rayon normally detaches.
///
/// This owner intentionally does not retain the Rayon pool. Once WebRender's backend has exited,
/// its final pool references can drop, signal worker termination, and allow these joins to finish.
pub(crate) struct RayonWorkerOwner {
    handles: Mutex<Vec<JoinHandle<()>>>,
}

impl RayonWorkerOwner {
    pub(crate) fn new() -> Self {
        Self {
            handles: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn spawn(&self, rayon_thread: rayon::ThreadBuilder) -> io::Result<()> {
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

    /// Join every retained worker, continuing after a panic and returning the first payload.
    pub(crate) fn join_all(&self) -> thread::Result<()> {
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;

    use super::RayonWorkerOwner;

    struct DropSentinel(Arc<AtomicBool>);

    impl Drop for DropSentinel {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn backend_shutdown_ack_does_not_hide_a_later_thread_panic() {
        let (ack_sender, ack_receiver) = mpsc::sync_channel(0);
        let backend = thread::spawn(move || {
            ack_sender
                .send(())
                .expect("the deterministic acknowledgement receiver should remain open");
            std::panic::resume_unwind(Box::new("deterministic post-ack backend panic"));
        });
        let scene_builder = thread::spawn(|| {});
        let mut handles = webrender::BackendThreadHandles::new(backend, scene_builder, None);

        ack_receiver
            .recv()
            .expect("the deterministic backend should acknowledge shutdown");
        assert!(handles.join_all().is_err());
        assert!(handles.join_all().is_ok(), "joining is idempotent");
    }

    #[test]
    fn backend_join_fences_post_ack_thread_owned_state() {
        let dropped = Arc::new(AtomicBool::new(false));
        let thread_dropped = dropped.clone();
        let (ack_sender, ack_receiver) = mpsc::sync_channel(0);
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        let backend = thread::spawn(move || {
            let _sentinel = DropSentinel(thread_dropped);
            ack_sender
                .send(())
                .expect("the deterministic acknowledgement receiver should remain open");
            release_receiver
                .recv()
                .expect("the deterministic release sender should remain open");
        });
        let scene_builder = thread::spawn(|| {});
        let mut handles = webrender::BackendThreadHandles::new(backend, scene_builder, None);

        ack_receiver
            .recv()
            .expect("the deterministic backend should acknowledge shutdown");
        assert!(!dropped.load(Ordering::SeqCst));
        release_sender
            .send(())
            .expect("the deterministic backend should remain blocked until release");
        handles
            .join_all()
            .expect("both deterministic backend handles should exit cleanly");
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn backend_join_continues_after_the_first_panicking_handle() {
        let later_handle_completed = Arc::new(AtomicBool::new(false));
        let later_thread_completed = later_handle_completed.clone();
        let backend = thread::spawn(|| std::panic::resume_unwind(Box::new("first handle panic")));
        let scene_builder = thread::spawn(move || {
            let _sentinel = DropSentinel(later_thread_completed);
        });
        let mut handles = webrender::BackendThreadHandles::new(backend, scene_builder, None);

        assert!(handles.join_all().is_err());
        assert!(later_handle_completed.load(Ordering::SeqCst));
    }

    #[test]
    fn production_spawn_handler_joins_every_custom_rayon_worker() {
        const WORKER_COUNT: usize = 2;
        let owner = Arc::new(RayonWorkerOwner::new());
        let spawn_owner = owner.clone();
        let exited = Arc::new(AtomicUsize::new(0));
        let exit_counter = exited.clone();

        let pool = Arc::new(
            rayon::ThreadPoolBuilder::new()
                .num_threads(WORKER_COUNT)
                .spawn_handler(move |rayon_thread| spawn_owner.spawn(rayon_thread))
                .exit_handler(move |_| {
                    exit_counter.fetch_add(1, Ordering::SeqCst);
                })
                .build()
                .expect("the fixed-size test pool should build"),
        );

        drop(pool);
        owner
            .join_all()
            .expect("every test worker should exit without panicking");
        assert_eq!(exited.load(Ordering::SeqCst), WORKER_COUNT);
    }

    #[test]
    fn worker_join_continues_after_the_first_panicking_handle() {
        let owner = RayonWorkerOwner::new();
        let later_handle_completed = Arc::new(AtomicBool::new(false));
        let later_thread_completed = later_handle_completed.clone();
        let mut handles = owner
            .handles
            .lock()
            .expect("the test owns the unpoisoned worker handle list");
        handles.push(thread::spawn(|| {
            std::panic::resume_unwind(Box::new("first worker handle panic"))
        }));
        handles.push(thread::spawn(move || {
            let _sentinel = DropSentinel(later_thread_completed);
        }));
        drop(handles);

        assert!(owner.join_all().is_err());
        assert!(later_handle_completed.load(Ordering::SeqCst));
    }
}
