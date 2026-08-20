/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Implementation of [microtasks](https://html.spec.whatwg.org/multipage/#microtask) and
//! microtask queues. It is up to implementations of event loops to store a queue and
//! perform checkpoints at appropriate times, as well as enqueue microtasks as required.

use std::cell::Cell;
use std::mem;
use std::rc::Rc;

use js::context::JSContext;
use js::rust::wrappers2::JobQueueMayNotBeEmpty;
use malloc_size_of::MallocSizeOf;
use script_bindings::cell::DomRefCell;
use script_bindings::root::Dom;

use crate::JSTraceable;
use crate::dom::bindings::callback::ExceptionHandling;
use crate::dom::bindings::codegen::Bindings::PromiseBinding::PromiseJobCallback;
use crate::dom::bindings::codegen::Bindings::VoidFunctionBinding::VoidFunction;
use crate::dom::bindings::root::DomRoot;
use crate::dom::globalscope::GlobalScope;
use crate::event_loop::script_thread::ScriptThread;
use crate::realms::enter_auto_realm;
use crate::script_runtime::notify_about_rejected_promises;

/// A sticky failure that prevents this queue from completing more checkpoints.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MicrotaskCheckpointError {
    /// The completed-checkpoint generation cannot represent another checkpoint.
    GenerationExhausted,
}

/// Outcome of attempting one HTML microtask checkpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[must_use]
pub(crate) enum CheckpointResult {
    /// A surrounding checkpoint already owns this queue; no checkpoint completed.
    AlreadyPerforming,
    /// All end-of-checkpoint steps completed.
    Completed {
        /// Monotonic generation assigned to this completed checkpoint.
        generation: u64,
    },
    /// This queue is terminal and cannot complete another checkpoint.
    Terminated {
        /// The first sticky failure observed by this queue.
        error: MicrotaskCheckpointError,
    },
}

/// One internally consistent observation of a single microtask queue.
///
/// This does not aggregate other [`MicrotaskQueue`] instances, including SpiderMonkey interrupt
/// queues. Event-loop-wide observation must combine every queue owned by that event loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct MicrotaskQueueObservation {
    /// Microtasks still stored in the active queue.
    pub(crate) queued_count: usize,
    /// Whether a checkpoint currently owns this queue.
    pub(crate) checkpoint_in_progress: bool,
    /// Number of non-reentrant checkpoints that completed all checkpoint steps.
    pub(crate) completed_checkpoint_generation: u64,
    /// The first sticky failure that prevents another checkpoint from completing.
    pub(crate) terminal_error: Option<MicrotaskCheckpointError>,
}

impl MicrotaskQueueObservation {
    /// Whether no queued or currently executing checkpoint work can remain in this queue.
    pub(crate) const fn authoritative_empty(self) -> bool {
        self.queued_count == 0 && !self.checkpoint_in_progress
    }
}

/// A collection of microtasks in FIFO order.
#[derive(Default, JSTraceable, MallocSizeOf)]
pub(crate) struct MicrotaskQueue {
    /// The list of enqueued microtasks that will be invoked at the next microtask checkpoint.
    microtask_queue: DomRefCell<Vec<Box<dyn MicrotaskRunnable>>>,
    /// <https://html.spec.whatwg.org/multipage/#performing-a-microtask-checkpoint>
    performing_a_microtask_checkpoint: Cell<bool>,
    /// Monotonic generation advanced after every completed, non-reentrant checkpoint.
    completed_checkpoint_generation: Cell<u64>,
    /// The first terminal checkpoint failure. Once set, this queue never resumes.
    #[no_trace]
    #[ignore_malloc_size_of = "Copy-only checked failure state"]
    terminal_error: Cell<Option<MicrotaskCheckpointError>>,
}

#[derive(JSTraceable, MallocSizeOf)]
pub struct NotifyMutationObserversMicrotask;

impl NotifyMutationObserversMicrotask {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl MicrotaskRunnable for NotifyMutationObserversMicrotask {
    fn handler(&self, cx: &mut JSContext) {
        ScriptThread::mutation_observers().notify_mutation_observers(cx);
    }
}

#[derive(JSTraceable, MallocSizeOf)]
pub struct CustomElementReactionMicrotask;

impl CustomElementReactionMicrotask {
    pub(crate) fn new() -> Self {
        Self
    }
}

impl MicrotaskRunnable for CustomElementReactionMicrotask {
    fn handler(&self, cx: &mut JSContext) {
        ScriptThread::invoke_backup_element_queue(cx);
    }
}

pub(crate) trait MicrotaskRunnable: JSTraceable + MallocSizeOf {
    // must also take care of entering the realm
    fn handler(&self, _cx: &mut JSContext) {}
}

/// A promise callback scheduled to run during the next microtask checkpoint (#4283).
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct EnqueuedPromiseCallback {
    #[conditional_malloc_size_of]
    pub(crate) callback: Rc<PromiseJobCallback>,
    pub(crate) global: Dom<GlobalScope>,
    pub(crate) is_user_interacting: bool,
}

impl MicrotaskRunnable for EnqueuedPromiseCallback {
    fn handler(&self, cx: &mut JSContext) {
        let _maybe_user_interacting_guard = if self.is_user_interacting {
            Some(ScriptThread::user_interacting_guard())
        } else {
            None
        };
        let mut realm = enter_auto_realm(cx, &*self.global);
        let cx = &mut realm;
        let _ = self
            .callback
            .Call_(cx, &*self.global, ExceptionHandling::Report);
    }
}

/// A microtask that comes from a queueMicrotask() Javascript call,
/// identical to EnqueuedPromiseCallback once it's on the queue
#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct UserMicrotask {
    #[conditional_malloc_size_of]
    pub(crate) callback: Rc<VoidFunction>,
    pub(crate) global: Dom<GlobalScope>,
}

impl MicrotaskRunnable for UserMicrotask {
    fn handler(&self, cx: &mut JSContext) {
        let mut realm = enter_auto_realm(cx, &*self.global);
        let cx = &mut realm;
        let _ = self
            .callback
            .Call_(cx, &*self.global, ExceptionHandling::Report);
    }
}

impl MicrotaskQueue {
    /// Add a new microtask to this queue. It will be invoked as part of the next
    /// microtask checkpoint.
    #[expect(unsafe_code)]
    pub(crate) fn enqueue(&self, cx: &JSContext, task: Box<dyn MicrotaskRunnable>) {
        self.microtask_queue.borrow_mut().push(task);
        unsafe { JobQueueMayNotBeEmpty(cx) };
    }

    /// <https://html.spec.whatwg.org/multipage/#perform-a-microtask-checkpoint>
    /// Perform a microtask checkpoint, executing all queued microtasks until the queue is empty.
    pub(crate) fn checkpoint(&self, cx: &mut JSContext, globalscopes: Vec<DomRoot<GlobalScope>>) {
        let _ = self.checkpoint_with_result(cx, globalscopes);
    }

    /// Perform a checkpoint and report whether this call completed one.
    ///
    /// Existing event-loop call sites use [`Self::checkpoint`] and intentionally discard this
    /// result. Controlled execution can use this typed form to distinguish a reentrant no-op from
    /// a completed checkpoint without inferring that distinction from queue emptiness.
    #[expect(unsafe_code)]
    pub(crate) fn checkpoint_with_result(
        &self,
        cx: &mut JSContext,
        globalscopes: Vec<DomRoot<GlobalScope>>,
    ) -> CheckpointResult {
        // Steps 1-2. Enter only when no surrounding checkpoint already owns this queue.
        if let Err(result) = self.begin_checkpoint() {
            return result;
        }

        debug!("Now performing a microtask checkpoint");

        // Step 3. While the event loop's microtask queue is not empty:
        while !self.microtask_queue.borrow().is_empty() {
            rooted_vec!(let mut pending_queue);
            self.swap_active_queue_into(&mut pending_queue);

            for (idx, job) in pending_queue.iter().enumerate() {
                if idx == pending_queue.len() - 1 && self.microtask_queue.borrow().is_empty() {
                    unsafe { js::rust::wrappers2::JobQueueIsEmpty(cx) };
                }

                job.handler(cx);
            }
        }

        // Step 4. For each environment settings object settingsObject whose responsible
        // event loop is this event loop, notify about rejected promises given
        // settingsObject's global object.
        for global in globalscopes.clone().into_iter() {
            notify_about_rejected_promises(cx, &global);
        }

        // https://html.spec.whatwg.org/multipage/#perform-a-microtask-checkpoint
        // Step 5. Cleanup Indexed Database transactions.
        // https://w3c.github.io/IndexedDB/#cleanup-indexed-database-transactions
        // “These steps are invoked by [HTML]. They ensure that transactions created by a script call
        // to transaction() are deactivated once the task that invoked the script has completed.”
        for global in globalscopes.iter() {
            if let Some(factory) = global.indexeddb_factory() {
                let _ = factory.cleanup_indexeddb_transactions(cx);
            }
        }

        // TODO: Step 6. Perform ClearKeptObjects().

        // TODO: Step 8. Record timing info for microtask checkpoint.
        self.finish_checkpoint()
    }

    fn begin_checkpoint(&self) -> Result<(), CheckpointResult> {
        if let Some(error) = self.terminal_error.get() {
            return Err(CheckpointResult::Terminated { error });
        }
        if self.performing_a_microtask_checkpoint.get() {
            return Err(CheckpointResult::AlreadyPerforming);
        }
        self.performing_a_microtask_checkpoint.set(true);
        Ok(())
    }

    fn finish_checkpoint(&self) -> CheckpointResult {
        debug_assert!(self.performing_a_microtask_checkpoint.get());
        let Some(generation) = self.completed_checkpoint_generation.get().checked_add(1) else {
            let error = MicrotaskCheckpointError::GenerationExhausted;
            self.terminal_error.set(Some(error));
            // A terminal queue must never retain checkpoint ownership.
            self.performing_a_microtask_checkpoint.set(false);
            return CheckpointResult::Terminated { error };
        };
        self.completed_checkpoint_generation.set(generation);
        // Step 7. Set the event loop's performing a microtask checkpoint to false.
        self.performing_a_microtask_checkpoint.set(false);
        CheckpointResult::Completed { generation }
    }

    fn swap_active_queue_into(&self, pending_queue: &mut Vec<Box<dyn MicrotaskRunnable>>) {
        mem::swap(pending_queue, &mut *self.microtask_queue.borrow_mut());
    }

    /// Observe queued work, checkpoint ownership, and completed-checkpoint generation together.
    pub(crate) fn observation(&self) -> MicrotaskQueueObservation {
        MicrotaskQueueObservation {
            queued_count: self.microtask_queue.borrow().len(),
            checkpoint_in_progress: self.performing_a_microtask_checkpoint.get(),
            completed_checkpoint_generation: self.completed_checkpoint_generation.get(),
            terminal_error: self.terminal_error.get(),
        }
    }

    pub(crate) fn queued_count(&self) -> usize {
        self.observation().queued_count
    }

    pub(crate) fn checkpoint_in_progress(&self) -> bool {
        self.observation().checkpoint_in_progress
    }

    pub(crate) fn completed_checkpoint_generation(&self) -> u64 {
        self.observation().completed_checkpoint_generation
    }

    pub(crate) fn terminal_error(&self) -> Option<MicrotaskCheckpointError> {
        self.observation().terminal_error
    }

    /// Return true only when neither queued nor currently draining checkpoint work remains.
    pub(crate) fn authoritative_empty(&self) -> bool {
        self.observation().authoritative_empty()
    }

    /// Return whether the active queue is empty, without accounting for a checkpoint's local
    /// pending queue. SpiderMonkey's job-queue callback requires this narrower answer.
    pub(crate) fn empty(&self) -> bool {
        self.microtask_queue.borrow().is_empty()
    }

    pub(crate) fn clear(&self) {
        self.microtask_queue.borrow_mut().clear();
    }

    #[cfg(test)]
    fn set_completed_checkpoint_generation_for_testing(&self, generation: u64) {
        debug_assert!(!self.performing_a_microtask_checkpoint.get());
        self.completed_checkpoint_generation.set(generation);
    }
}

#[cfg(test)]
mod tests {
    use style::thread_state::{self, ThreadState};

    use super::*;

    struct ScriptThreadStateGuard {
        entered_script: bool,
    }

    impl ScriptThreadStateGuard {
        fn enter() -> Self {
            let entered_script = !thread_state::get().is_script();
            if entered_script {
                thread_state::enter(ThreadState::SCRIPT);
            }
            Self { entered_script }
        }
    }

    impl Drop for ScriptThreadStateGuard {
        fn drop(&mut self) {
            if self.entered_script {
                thread_state::exit(ThreadState::SCRIPT);
            }
        }
    }

    #[test]
    fn reentrant_checkpoint_does_not_complete_or_advance_generation() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let queue = MicrotaskQueue::default();

        assert_eq!(queue.begin_checkpoint(), Ok(()));
        assert_eq!(
            queue.begin_checkpoint(),
            Err(CheckpointResult::AlreadyPerforming)
        );
        assert_eq!(queue.completed_checkpoint_generation(), 0);
        assert!(queue.checkpoint_in_progress());

        assert_eq!(
            queue.finish_checkpoint(),
            CheckpointResult::Completed { generation: 1 }
        );
        assert_eq!(queue.completed_checkpoint_generation(), 1);
        assert!(queue.authoritative_empty());
    }

    #[test]
    fn completed_checkpoint_generation_is_monotonic() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let queue = MicrotaskQueue::default();

        assert_eq!(queue.begin_checkpoint(), Ok(()));
        assert_eq!(
            queue.finish_checkpoint(),
            CheckpointResult::Completed { generation: 1 }
        );
        assert_eq!(queue.begin_checkpoint(), Ok(()));
        assert_eq!(
            queue.finish_checkpoint(),
            CheckpointResult::Completed { generation: 2 }
        );
        assert_eq!(queue.completed_checkpoint_generation(), 2);
    }

    #[test]
    fn swapped_work_is_not_authoritatively_empty_during_a_checkpoint() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let queue = MicrotaskQueue::default();
        assert!(queue.authoritative_empty());

        queue
            .microtask_queue
            .borrow_mut()
            .push(Box::new(NotifyMutationObserversMicrotask::new()));
        assert_eq!(queue.queued_count(), 1);
        assert_eq!(queue.begin_checkpoint(), Ok(()));
        let mut pending_queue = Vec::new();
        queue.swap_active_queue_into(&mut pending_queue);

        let during = queue.observation();
        assert_eq!(during.queued_count, 0);
        assert!(during.checkpoint_in_progress);
        assert!(!during.authoritative_empty());
        assert_eq!(during.completed_checkpoint_generation, 0);
        assert_eq!(
            queue.begin_checkpoint(),
            Err(CheckpointResult::AlreadyPerforming)
        );
        assert_eq!(queue.completed_checkpoint_generation(), 0);
        assert_eq!(pending_queue.len(), 1);
        drop(pending_queue);

        assert_eq!(
            queue.finish_checkpoint(),
            CheckpointResult::Completed { generation: 1 }
        );
        assert!(queue.authoritative_empty());
    }

    #[test]
    fn generation_exhaustion_is_sticky_and_releases_checkpoint_ownership() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let queue = MicrotaskQueue::default();
        queue.set_completed_checkpoint_generation_for_testing(u64::MAX);

        assert_eq!(queue.begin_checkpoint(), Ok(()));
        assert_eq!(
            queue.finish_checkpoint(),
            CheckpointResult::Terminated {
                error: MicrotaskCheckpointError::GenerationExhausted,
            }
        );

        let terminated = queue.observation();
        assert_eq!(terminated.completed_checkpoint_generation, u64::MAX);
        assert!(!terminated.checkpoint_in_progress);
        assert_eq!(
            terminated.terminal_error,
            Some(MicrotaskCheckpointError::GenerationExhausted)
        );
        assert_eq!(
            queue.begin_checkpoint(),
            Err(CheckpointResult::Terminated {
                error: MicrotaskCheckpointError::GenerationExhausted,
            })
        );
        assert_eq!(
            queue.terminal_error(),
            Some(MicrotaskCheckpointError::GenerationExhausted)
        );
    }
}
