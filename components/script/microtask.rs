/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Implementation of [microtasks](https://html.spec.whatwg.org/multipage/#microtask) and
//! microtask queues. It is up to implementations of event loops to store a queue and
//! perform checkpoints at appropriate times, as well as enqueue microtasks as required.

use std::cell::{Cell, RefCell};
use std::mem;
use std::rc::Rc;

use js::context::JSContext;
use js::rust::wrappers2::JobQueueMayNotBeEmpty;
use malloc_size_of::MallocSizeOf;
use script_bindings::cell::DomRefCell;
use script_bindings::root::Dom;
use timers::DocumentExecutionLedger;

use crate::JSTraceable;
use crate::dom::bindings::callback::ExceptionHandling;
use crate::dom::bindings::codegen::Bindings::PromiseBinding::PromiseJobCallback;
use crate::dom::bindings::codegen::Bindings::VoidFunctionBinding::VoidFunction;
use crate::dom::bindings::root::DomRoot;
use crate::dom::globalscope::GlobalScope;
use crate::event_loop::script_thread::ScriptThread;
use crate::realms::enter_auto_realm;
use crate::script_runtime::notify_about_rejected_promises;

/// Policy slot shared by the main SpiderMonkey job queue and every nested interrupt queue.
#[derive(Clone, Default)]
pub(crate) struct MicrotaskExecutionLedgerSlot(Rc<RefCell<Option<DocumentExecutionLedger>>>);

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
    /// Controlled execution became terminal and all remaining work was discarded.
    ExecutionTerminated,
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
    /// Controlled-session accounting installed before the first navigation.
    #[no_trace]
    #[ignore_malloc_size_of = "The execution ledger is shared with the document clock domain"]
    execution_ledger: MicrotaskExecutionLedgerSlot,
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
    /// Construct an empty SpiderMonkey interrupt queue bound to the main queue's policy slot.
    pub(crate) fn with_execution_ledger_slot(
        execution_ledger: MicrotaskExecutionLedgerSlot,
    ) -> Self {
        Self {
            execution_ledger,
            ..Self::default()
        }
    }

    /// Clone the policy slot used to construct nested SpiderMonkey interrupt queues.
    pub(crate) fn execution_ledger_slot(&self) -> MicrotaskExecutionLedgerSlot {
        self.execution_ledger.clone()
    }

    /// Install the execution ledger before any page microtask can be queued.
    pub(crate) fn install_execution_ledger(&self, ledger: Option<DocumentExecutionLedger>) {
        debug_assert!(self.microtask_queue.borrow().is_empty());
        debug_assert!(!self.performing_a_microtask_checkpoint.get());
        *self.execution_ledger.0.borrow_mut() = ledger;
    }

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

        if self.execution_is_terminal() {
            return self
                .abort_terminal_checkpoint(|| unsafe { js::rust::wrappers2::JobQueueIsEmpty(cx) });
        }

        debug!("Now performing a microtask checkpoint");

        // Step 3. While the event loop's microtask queue is not empty:
        while !self.microtask_queue.borrow().is_empty() {
            rooted_vec!(let mut pending_queue);
            self.swap_active_queue_into(&mut pending_queue);

            for (idx, job) in pending_queue.iter().enumerate() {
                // Count every individual job before invoking it. A sticky failure stops this
                // checkpoint even when the preceding job requeued itself; terminal execution
                // never resumes or publishes the discarded queue suffix.
                if !self.begin_microtask_job() {
                    return self.abort_terminal_checkpoint(|| unsafe {
                        js::rust::wrappers2::JobQueueIsEmpty(cx)
                    });
                }
                if idx == pending_queue.len() - 1 && self.microtask_queue.borrow().is_empty() {
                    unsafe { js::rust::wrappers2::JobQueueIsEmpty(cx) };
                }

                job.handler(cx);

                // Mutation accounting is non-rejecting and can therefore latch during a job.
                // Stop before invoking another queued job.
                if self.execution_is_terminal() {
                    return self.abort_terminal_checkpoint(|| unsafe {
                        js::rust::wrappers2::JobQueueIsEmpty(cx)
                    });
                }
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

    fn abort_terminal_checkpoint(&self, notify_job_queue_empty: impl FnOnce()) -> CheckpointResult {
        debug_assert!(self.performing_a_microtask_checkpoint.get());
        // Jobs moved into the local pending queue are dropped by the caller's early return. Jobs
        // requeued by an already-run job remain in this active queue, so clear them explicitly.
        // This same method runs for main and SpiderMonkey interrupt queues.
        self.microtask_queue.borrow_mut().clear();
        notify_job_queue_empty();
        self.performing_a_microtask_checkpoint.set(false);
        CheckpointResult::ExecutionTerminated
    }

    fn begin_microtask_job(&self) -> bool {
        self.execution_ledger
            .0
            .borrow()
            .as_ref()
            .is_none_or(|ledger| ledger.begin_microtask().is_ok())
    }

    fn execution_is_terminal(&self) -> bool {
        self.execution_ledger
            .0
            .borrow()
            .as_ref()
            .is_some_and(|ledger| ledger.observation().terminal.is_some())
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
    use std::collections::VecDeque;

    use style::thread_state::{self, ThreadState};
    use timers::{
        DocumentClock, DocumentExecutionBudget, DocumentExecutionLimits, DocumentExecutionTerminal,
    };

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

    fn execution_limits(microtasks: u64) -> DocumentExecutionLimits {
        DocumentExecutionLimits {
            ordinary_tasks: 1,
            microtasks,
            rendering_opportunities: 1,
            mutations: 1,
        }
    }

    fn execution_ledger(limits: DocumentExecutionLimits) -> DocumentExecutionLedger {
        let clock = DocumentClock::default();
        DocumentExecutionLedger::new(clock.id(), limits)
    }

    #[test]
    fn self_rescheduling_microtask_is_cut_off_inside_one_checkpoint_budget() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let queue = MicrotaskQueue::default();
        let ledger = execution_ledger(execution_limits(3));
        queue.install_execution_ledger(Some(ledger.clone()));

        let mut pending = VecDeque::from([()]);
        let mut invoked = 0;
        while pending.pop_front().is_some() {
            if !queue.begin_microtask_job() {
                break;
            }
            invoked += 1;
            pending.push_back(());
        }

        assert_eq!(invoked, 3);
        assert_eq!(ledger.observation().counters.microtasks, 3);
        assert!(matches!(
            ledger.observation().terminal,
            Some(DocumentExecutionTerminal::BudgetExceeded {
                budget: DocumentExecutionBudget::Microtasks,
                limit: 3,
                observed: 4,
            })
        ));
    }

    #[test]
    fn interrupt_queue_created_before_install_shares_the_exact_ledger() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let main_queue = MicrotaskQueue::default();
        let interrupt_queue =
            MicrotaskQueue::with_execution_ledger_slot(main_queue.execution_ledger_slot());
        let ledger = execution_ledger(execution_limits(2));
        main_queue.install_execution_ledger(Some(ledger.clone()));

        assert!(interrupt_queue.begin_microtask_job());
        assert!(main_queue.begin_microtask_job());
        assert!(!interrupt_queue.begin_microtask_job());
        assert_eq!(ledger.observation().counters.microtasks, 2);
        assert!(matches!(
            ledger.observation().terminal,
            Some(DocumentExecutionTerminal::BudgetExceeded {
                budget: DocumentExecutionBudget::Microtasks,
                limit: 2,
                observed: 3,
            })
        ));
    }

    #[test]
    fn terminal_abort_discards_requeued_work_without_completing_a_generation() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let queue = MicrotaskQueue::default();
        let ledger = execution_ledger(DocumentExecutionLimits {
            mutations: 0,
            ..execution_limits(1)
        });
        queue.install_execution_ledger(Some(ledger.clone()));
        assert_eq!(queue.begin_checkpoint(), Ok(()));

        // Model the last admitted job requeueing work before a non-rejecting mutation hook latches
        // the terminal. The pending suffix is local to checkpoint(); this is the active suffix that
        // would otherwise survive its early return.
        assert!(queue.begin_microtask_job());
        queue
            .microtask_queue
            .borrow_mut()
            .push(Box::new(NotifyMutationObserversMicrotask::new()));
        ledger.record_mutation_record();
        assert!(queue.execution_is_terminal());

        let empty_notifications = Cell::new(0);
        assert_eq!(
            queue.abort_terminal_checkpoint(|| {
                empty_notifications.set(empty_notifications.get() + 1)
            }),
            CheckpointResult::ExecutionTerminated
        );
        assert!(queue.authoritative_empty());
        assert_eq!(queue.completed_checkpoint_generation(), 0);
        assert_eq!(queue.terminal_error(), None);
        assert_eq!(empty_notifications.get(), 1);
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
