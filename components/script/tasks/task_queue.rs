/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Machinery for [task-queue](https://html.spec.whatwg.org/multipage/#task-queue).

use std::cell::Cell;
use std::collections::VecDeque;
use std::default::Default;

use crossbeam_channel::{self, Receiver, Sender};
use rustc_hash::{FxHashMap, FxHashSet};
use script_bindings::cell::DomRefCell;
use servo_base::id::PipelineId;
use strum::VariantArray;

use crate::dom::worker::TrustedWorkerAddress;
use crate::script_runtime::ScriptThreadEventCategory;
use crate::tasks::task::TaskBox;
use crate::tasks::task_source::TaskSourceName;

#[derive(MallocSizeOf)]
pub(crate) struct QueuedTask {
    pub(crate) worker: Option<TrustedWorkerAddress>,
    pub(crate) event_category: ScriptThreadEventCategory,
    #[ignore_malloc_size_of = "TaskBox is difficult"]
    pub(crate) task: Box<dyn TaskBox>,
    pub(crate) pipeline_id: Option<PipelineId>,
    pub(crate) task_source: TaskSourceName,
}

/// Defining the operations used to convert from a msg T to a QueuedTask.
pub(crate) trait QueuedTaskConversion {
    fn task_source_name(&self) -> Option<&TaskSourceName>;
    fn pipeline_id(&self) -> Option<PipelineId>;
    fn into_queued_task(self) -> Option<QueuedTask>;
    fn from_queued_task(queued_task: QueuedTask) -> Self;
    fn inactive_msg() -> Self;
    fn wake_up_msg() -> Self;
    fn is_wake_up(&self) -> bool;
}

/// Side-effect-free counts for work already retained by a [`TaskQueue`].
///
/// The raw producer channel is deliberately not sampled here. Controlled intake accounts for
/// that channel with its own bounded-intake saturation bit.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TaskQueueObservation {
    pub(crate) ready: usize,
    pub(crate) throttled: usize,
    pub(crate) inactive: usize,
}

#[derive(MallocSizeOf)]
pub(crate) struct TaskQueue<T> {
    /// The original port on which the task-sources send tasks as messages.
    port: Receiver<T>,
    /// A sender to ensure the port doesn't block on select while there are throttled tasks.
    wake_up_sender: Sender<T>,
    /// A queue from which the event-loop can drain tasks.
    msg_queue: DomRefCell<VecDeque<T>>,
    /// A "business" counter, reset for each iteration of the event-loop
    taken_task_counter: Cell<u64>,
    /// Tasks that will be throttled for as long as we are "busy".
    throttled: DomRefCell<FxHashMap<TaskSourceName, VecDeque<QueuedTask>>>,
    /// Tasks for not fully-active documents.
    inactive: DomRefCell<FxHashMap<PipelineId, VecDeque<QueuedTask>>>,
    /// Pipelines whose tasks must be discarded even when they arrive after teardown.
    closed_pipelines: Option<DomRefCell<FxHashSet<PipelineId>>>,
}

impl<T: QueuedTaskConversion> TaskQueue<T> {
    pub(crate) fn new(port: Receiver<T>, wake_up_sender: Sender<T>) -> TaskQueue<T> {
        Self::new_with_producer_tracking(port, wake_up_sender, false)
    }

    /// Construct a task queue that permanently filters tasks after their pipeline exits.
    pub(crate) fn new_with_producer_tracking(
        port: Receiver<T>,
        wake_up_sender: Sender<T>,
        producer_tracking: bool,
    ) -> TaskQueue<T> {
        TaskQueue {
            port,
            wake_up_sender,
            msg_queue: DomRefCell::new(VecDeque::new()),
            taken_task_counter: Default::default(),
            throttled: Default::default(),
            inactive: Default::default(),
            closed_pipelines: producer_tracking.then(Default::default),
        }
    }

    /// Permanently discard tasks for `pipeline_id` from every queue and future channel intake.
    pub(crate) fn discard_pipeline(&self, pipeline_id: PipelineId) {
        let Some(closed_pipelines) = &self.closed_pipelines else {
            return;
        };
        closed_pipelines.borrow_mut().insert(pipeline_id);

        self.msg_queue
            .borrow_mut()
            .retain(|message| message.pipeline_id() != Some(pipeline_id));

        let mut throttled = self.throttled.borrow_mut();
        throttled.retain(|_, queue| {
            queue.retain(|task| task.pipeline_id != Some(pipeline_id));
            !queue.is_empty()
        });
        drop(throttled);

        self.inactive.borrow_mut().remove(&pipeline_id);
    }

    /// Observe locally retained tasks without receiving, promoting, or reclassifying work.
    pub(crate) fn observation(&self) -> TaskQueueObservation {
        TaskQueueObservation {
            ready: self.msg_queue.borrow().len(),
            throttled: self.throttled.borrow().values().map(VecDeque::len).sum(),
            inactive: self.inactive.borrow().values().map(VecDeque::len).sum(),
        }
    }

    /// Release previously held-back tasks for documents that are now fully-active.
    /// <https://html.spec.whatwg.org/multipage/#event-loop-processing-model:fully-active>
    fn release_tasks_for_fully_active_documents(
        &self,
        fully_active: &FxHashSet<PipelineId>,
        limit: usize,
    ) -> Vec<T> {
        let mut released = Vec::new();
        let mut remaining = limit;
        let mut inactive = self.inactive.borrow_mut();

        for (pipeline_id, inactive_queue) in inactive.iter_mut() {
            if remaining == 0 {
                break;
            }
            if !fully_active.contains(pipeline_id) {
                continue;
            }

            let release_count = remaining.min(inactive_queue.len());
            released.extend(
                inactive_queue
                    .drain(..release_count)
                    .map(T::from_queued_task),
            );
            remaining -= release_count;
        }

        released
    }

    /// Hold back tasks for currently not fully-active documents.
    /// <https://html.spec.whatwg.org/multipage/#event-loop-processing-model:fully-active>
    fn store_task_for_inactive_pipeline(&self, msg: T, pipeline_id: &PipelineId) {
        let mut inactive = self.inactive.borrow_mut();
        let inactive_queue = inactive.entry(*pipeline_id).or_default();
        inactive_queue.push_back(
            msg.into_queued_task()
                .expect("Incoming messages should always be convertible into queued tasks"),
        );
        let mut msg_queue = self.msg_queue.borrow_mut();
        if msg_queue.is_empty() {
            // Ensure there is at least one message.
            // Otherwise if the just stored inactive message
            // was the first and last of this iteration,
            // it will result in a spurious wake-up of the event-loop.
            msg_queue.push_back(T::inactive_msg());
        }
    }

    /// Process incoming tasks, immediately sending priority ones downstream,
    /// and categorizing potential throttles.
    fn process_incoming_tasks(
        &self,
        first_msg: T,
        fully_active: &FxHashSet<PipelineId>,
        drain_ready_port: bool,
        inactive_release_limit: usize,
    ) {
        // 1. Make any previously stored task from now fully-active document available.
        let mut incoming =
            self.release_tasks_for_fully_active_documents(fully_active, inactive_release_limit);

        // 2. Process the first message(artifact of the fact that select always returns a message).
        if !first_msg.is_wake_up() {
            incoming.push(first_msg);
        }

        // 3. Process any other incoming message.
        if drain_ready_port {
            while let Ok(msg) = self.port.try_recv() {
                if !msg.is_wake_up() {
                    incoming.push(msg);
                }
            }
        }

        // Pipeline identity is permanent. Drop both tasks already present at close time and any
        // producer that races teardown and arrives later; global (`None`) tasks remain eligible.
        if let Some(closed_pipelines) = &self.closed_pipelines {
            let closed_pipelines = closed_pipelines.borrow();
            incoming.retain(|message| {
                message
                    .pipeline_id()
                    .is_none_or(|pipeline_id| !closed_pipelines.contains(&pipeline_id))
            });
        }

        // 4. Filter tasks from non-priority task-sources.
        // TODO: This can use `extract_if` once that is stabilized.
        let mut to_be_throttled = Vec::new();
        let mut index = 0;
        while index != incoming.len() {
            index += 1; // By default we go to the next index of the vector.

            let task_source = match incoming[index - 1].task_source_name() {
                Some(task_source) => task_source,
                None => continue,
            };

            match task_source {
                TaskSourceName::PerformanceTimeline => {
                    to_be_throttled.push(incoming.remove(index - 1));
                    index -= 1; // We've removed an element, so the next has the same index.
                },
                _ => {
                    // A task that will not be throttled, start counting "business"
                    self.taken_task_counter
                        .set(self.taken_task_counter.get() + 1);
                },
            }
        }

        for msg in incoming {
            // Always run "update the rendering" tasks,
            // TODO: fix "fully active" concept for iframes.
            if let Some(TaskSourceName::Rendering) = msg.task_source_name() {
                self.msg_queue.borrow_mut().push_back(msg);
                continue;
            }
            // Only task messages participate in fully-active document throttling. Direct
            // pipeline messages expose identity for tombstone filtering and purge, but are not
            // convertible into `QueuedTask` and preserve their upstream ready-path behavior.
            if msg.task_source_name().is_some() {
                if let Some(pipeline_id) = msg.pipeline_id() &&
                    !fully_active.contains(&pipeline_id)
                {
                    self.store_task_for_inactive_pipeline(msg, &pipeline_id);
                    continue;
                }
            }
            // Immediately send non-throttled tasks for processing.
            self.msg_queue.borrow_mut().push_back(msg);
        }

        for msg in to_be_throttled {
            // Categorize tasks per task queue.
            let Some(queued_task) = msg.into_queued_task() else {
                unreachable!(
                    "A message to be throttled should always be convertible into a queued task"
                );
            };
            let mut throttled_tasks = self.throttled.borrow_mut();
            throttled_tasks
                .entry(queued_task.task_source)
                .or_default()
                .push_back(queued_task);
        }
    }

    /// Reset the queue for a new iteration of the event-loop,
    /// returning the port about whose readiness we want to be notified.
    pub(crate) fn select(&self) -> &crossbeam_channel::Receiver<T> {
        // This is a new iteration of the event-loop, so we reset the "business" counter.
        self.start_event_loop_iteration();
        // We want to be notified when the script-port is ready to receive.
        // Hence that's the one we need to include in the select.
        &self.port
    }

    /// Reset per-iteration task throttling before a controlled event-loop turn.
    pub(crate) fn start_event_loop_iteration(&self) {
        self.taken_task_counter.set(0);
    }

    /// Take a message from the front of the queue, without waiting if empty.
    pub(crate) fn recv(&self) -> Result<T, ()> {
        self.msg_queue.borrow_mut().pop_front().ok_or(())
    }

    /// Take all tasks again and then run `recv()`.
    pub(crate) fn take_tasks_and_recv(
        &self,
        fully_active: &FxHashSet<PipelineId>,
    ) -> Result<T, ()> {
        self.take_tasks(T::wake_up_msg(), fully_active);
        self.recv()
    }

    /// Take at most one newly received task and then run [`Self::recv`].
    ///
    /// Controlled execution uses this path so a continuously-ready producer cannot move an
    /// unbounded channel suffix into ScriptThread-private queues during one intake step.
    pub(crate) fn take_one_task_and_recv(
        &self,
        fully_active: &FxHashSet<PipelineId>,
    ) -> Result<T, ()> {
        if let Ok(message) = self.recv() {
            return Ok(message);
        }
        if let Ok(first_msg) = self.port.try_recv() {
            return Ok(self.take_controlled_task_and_recv(first_msg, fully_active));
        }

        // With no raw producer input to order first, promote one retained inactive task or one
        // eligible throttle. Repeated controlled polls make further bounded progress.
        self.take_one_task(T::wake_up_msg(), fully_active);
        if let Ok(message) = self.recv() {
            return Ok(message);
        }

        // A producer may have committed after the first nonblocking port check. Close that local
        // race without draining more than one selected producer item.
        let first_msg = self.port.try_recv().map_err(|_| ())?;
        Ok(self.take_controlled_task_and_recv(first_msg, fully_active))
    }

    /// Process one selected controlled task-port input without letting stale wake markers
    /// overtake or consume a ready ordinary task.
    pub(crate) fn take_controlled_task_and_recv(
        &self,
        mut first_msg: T,
        fully_active: &FxHashSet<PipelineId>,
    ) -> T {
        const WAKE_SCAN_LIMIT: usize = 64;

        for wake_index in 0..WAKE_SCAN_LIMIT {
            if !first_msg.is_wake_up() {
                // Raw ordinary input stays ahead of retained inactive work and throttles.
                self.take_one_incoming_task(first_msg, fully_active);
                return self.recv().unwrap_or_else(|_| T::wake_up_msg());
            }
            if wake_index + 1 == WAKE_SCAN_LIMIT {
                // Do not fetch a successor that this bounded step cannot also retain.
                return T::wake_up_msg();
            }
            let Ok(next_msg) = self.port.try_recv() else {
                self.take_one_task(T::wake_up_msg(), fully_active);
                return self.recv().unwrap_or_else(|_| T::wake_up_msg());
            };
            first_msg = next_msg;
        }

        T::wake_up_msg()
    }

    /// Drain the queue for the current iteration of the event-loop.
    /// Holding-back throttles above a given high-water mark.
    pub(crate) fn take_tasks(&self, first_msg: T, fully_active: &FxHashSet<PipelineId>) {
        self.take_tasks_with_options(first_msg, fully_active, true, usize::MAX, usize::MAX);
    }

    /// Make one retained task available without draining the ready producer port.
    fn take_one_task(&self, first_msg: T, fully_active: &FxHashSet<PipelineId>) {
        self.take_tasks_with_options(first_msg, fully_active, false, 1, 1);
    }

    /// Categorize one ready producer-port item while keeping retained work behind it.
    fn take_one_incoming_task(&self, first_msg: T, fully_active: &FxHashSet<PipelineId>) {
        self.take_tasks_with_options(first_msg, fully_active, false, 0, 0);
    }

    fn take_tasks_with_options(
        &self,
        first_msg: T,
        fully_active: &FxHashSet<PipelineId>,
        drain_ready_port: bool,
        inactive_release_limit: usize,
        throttle_release_limit: usize,
    ) {
        // High-watermark: once reached, throttled tasks will be held-back.
        const PER_ITERATION_MAX: u64 = 5;
        // Always first check for new tasks, but don't reset 'taken_task_counter'.
        self.process_incoming_tasks(
            first_msg,
            fully_active,
            drain_ready_port,
            inactive_release_limit,
        );
        if throttle_release_limit == 0 {
            return;
        }
        let mut throttled = self.throttled.borrow_mut();
        let mut throttled_length: usize = throttled.values().map(|queue| queue.len()).sum();
        let mut task_source_cycler = TaskSourceName::VARIANTS.iter().cycle();
        let mut consumed_throttles = 0;
        let controlled_release = throttle_release_limit != usize::MAX;
        // "being busy", is defined as having more than x tasks for this loop's iteration.
        // As long as we're not busy, and there are throttled tasks left:
        loop {
            let max_reached = self.taken_task_counter.get() > PER_ITERATION_MAX;
            let none_left = throttled_length == 0;
            let release_limit_reached = consumed_throttles == throttle_release_limit;
            match (max_reached || release_limit_reached, none_left) {
                (_, true) => break,
                (true, false) => {
                    // We have reached the high-watermark for this iteration of the event-loop,
                    // or the controlled release cap, yet throttled messages remain.
                    // Real mode needs a wake for its next event-loop iteration. Controlled intake
                    // observes retained throttles directly and must not manufacture an endless
                    // stream of no-op owner events while page turns are paused.
                    if !controlled_release {
                        let _ = self.wake_up_sender.send(T::wake_up_msg());
                    }
                    break;
                },
                (false, false) => {
                    // Cycle through non-priority task sources, taking one throttled task from each.
                    let task_source = task_source_cycler.next().unwrap();
                    let throttled_queue = match throttled.get_mut(task_source) {
                        Some(queue) => queue,
                        None => continue,
                    };
                    let queued_task = match throttled_queue.pop_front() {
                        Some(queued_task) => queued_task,
                        None => continue,
                    };
                    consumed_throttles += 1;
                    let msg = T::from_queued_task(queued_task);

                    // Hold back tasks for currently inactive documents.
                    if let Some(pipeline_id) = msg.pipeline_id() &&
                        !fully_active.contains(&pipeline_id)
                    {
                        self.store_task_for_inactive_pipeline(msg, &pipeline_id);
                        // Reduce the length of throttles,
                        // but don't add the task to "msg_queue",
                        // and neither increment "taken_task_counter".
                        throttled_length -= 1;
                        continue;
                    }

                    // Make the task available for the event-loop to handle as a message.
                    self.msg_queue.borrow_mut().push_back(msg);
                    self.taken_task_counter
                        .set(self.taken_task_counter.get() + 1);
                    throttled_length -= 1;
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use servo_base::id::TEST_PIPELINE_ID;
    use style::thread_state::{self, ThreadState};

    use super::*;

    struct ScriptThreadStateGuard(bool);

    impl ScriptThreadStateGuard {
        fn enter() -> Self {
            let entered = !thread_state::get().is_script();
            if entered {
                thread_state::enter(ThreadState::SCRIPT);
            }
            Self(entered)
        }
    }

    impl Drop for ScriptThreadStateGuard {
        fn drop(&mut self) {
            if self.0 {
                thread_state::exit(ThreadState::SCRIPT);
            }
        }
    }

    struct NeverRunTask;

    impl TaskBox for NeverRunTask {
        fn name(&self) -> &'static str {
            "NeverRunTask"
        }

        fn run_box(self: Box<Self>, _: &mut js::context::JSContext) {
            panic!("task-queue tests never execute tasks")
        }
    }

    enum TestMessage {
        Task {
            source: TaskSourceName,
            pipeline_id: Option<PipelineId>,
        },
        Inactive,
        WakeUp,
    }

    impl TestMessage {
        fn task(source: TaskSourceName) -> Self {
            Self::Task {
                source,
                pipeline_id: None,
            }
        }
    }

    impl QueuedTaskConversion for TestMessage {
        fn task_source_name(&self) -> Option<&TaskSourceName> {
            match self {
                Self::Task { source, .. } => Some(source),
                Self::Inactive | Self::WakeUp => None,
            }
        }

        fn pipeline_id(&self) -> Option<PipelineId> {
            match self {
                Self::Task { pipeline_id, .. } => *pipeline_id,
                Self::Inactive | Self::WakeUp => None,
            }
        }

        fn into_queued_task(self) -> Option<QueuedTask> {
            let Self::Task {
                source,
                pipeline_id,
            } = self
            else {
                return None;
            };
            Some(QueuedTask {
                worker: None,
                event_category: source.into(),
                task: Box::new(NeverRunTask),
                pipeline_id,
                task_source: source,
            })
        }

        fn from_queued_task(queued_task: QueuedTask) -> Self {
            Self::Task {
                source: queued_task.task_source,
                pipeline_id: queued_task.pipeline_id,
            }
        }

        fn inactive_msg() -> Self {
            Self::Inactive
        }

        fn wake_up_msg() -> Self {
            Self::WakeUp
        }

        fn is_wake_up(&self) -> bool {
            matches!(self, Self::WakeUp)
        }
    }

    #[test]
    fn controlled_poll_leaves_the_ready_port_suffix_unadmitted() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let queue = TaskQueue::new(receiver, sender.clone());
        let fully_active = FxHashSet::default();

        sender
            .send(TestMessage::task(TaskSourceName::Timer))
            .unwrap();
        sender
            .send(TestMessage::task(TaskSourceName::Networking))
            .unwrap();

        assert!(matches!(
            queue.take_one_task_and_recv(&fully_active),
            Ok(TestMessage::Task {
                source: TaskSourceName::Timer,
                ..
            })
        ));
        assert_eq!(queue.observation(), TaskQueueObservation::default());
        assert!(matches!(
            queue.take_one_task_and_recv(&fully_active),
            Ok(TestMessage::Task {
                source: TaskSourceName::Networking,
                ..
            })
        ));
    }

    #[test]
    fn realtime_bulk_intake_retains_its_existing_drain_behavior() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let queue = TaskQueue::new(receiver, sender.clone());
        let fully_active = FxHashSet::default();

        sender
            .send(TestMessage::task(TaskSourceName::Timer))
            .unwrap();
        sender
            .send(TestMessage::task(TaskSourceName::Networking))
            .unwrap();

        assert!(matches!(
            queue.take_tasks_and_recv(&fully_active),
            Ok(TestMessage::Task {
                source: TaskSourceName::Timer,
                ..
            })
        ));
        assert_eq!(
            queue.observation(),
            TaskQueueObservation {
                ready: 1,
                throttled: 0,
                inactive: 0,
            }
        );
        assert!(matches!(
            queue.recv(),
            Ok(TestMessage::Task {
                source: TaskSourceName::Networking,
                ..
            })
        ));
    }

    #[test]
    fn controlled_poll_promotes_at_most_one_newly_active_retained_task() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let queue = TaskQueue::new(receiver, sender.clone());
        let mut fully_active = FxHashSet::default();

        for _ in 0..2 {
            sender
                .send(TestMessage::Task {
                    source: TaskSourceName::Timer,
                    pipeline_id: Some(TEST_PIPELINE_ID),
                })
                .unwrap();
            assert!(matches!(
                queue.take_one_task_and_recv(&fully_active),
                Ok(TestMessage::Inactive)
            ));
        }
        assert_eq!(queue.observation().inactive, 2);

        fully_active.insert(TEST_PIPELINE_ID);
        assert!(matches!(
            queue.take_one_task_and_recv(&fully_active),
            Ok(TestMessage::Task {
                source: TaskSourceName::Timer,
                pipeline_id: Some(TEST_PIPELINE_ID),
            })
        ));
        assert_eq!(queue.observation().inactive, 1);
        assert!(matches!(
            queue.take_one_task_and_recv(&fully_active),
            Ok(TestMessage::Task {
                source: TaskSourceName::Timer,
                pipeline_id: Some(TEST_PIPELINE_ID),
            })
        ));
        assert_eq!(queue.observation().inactive, 0);
    }

    #[test]
    fn controlled_poll_keeps_ready_ordinary_tasks_ahead_of_throttles() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let queue = TaskQueue::new(receiver, sender.clone());
        let fully_active = FxHashSet::default();
        queue.start_event_loop_iteration();

        for _ in 0..6 {
            sender
                .send(TestMessage::task(TaskSourceName::Timer))
                .unwrap();
            assert!(matches!(
                queue.take_one_task_and_recv(&fully_active),
                Ok(TestMessage::Task {
                    source: TaskSourceName::Timer,
                    ..
                })
            ));
        }
        sender
            .send(TestMessage::task(TaskSourceName::PerformanceTimeline))
            .unwrap();
        assert!(matches!(
            queue.take_one_task_and_recv(&fully_active),
            Ok(TestMessage::WakeUp)
        ));
        assert_eq!(queue.observation().throttled, 1);
        assert!(
            queue.take_one_task_and_recv(&fully_active).is_err(),
            "paused controlled intake must not manufacture throttle wakeups"
        );
        assert_eq!(queue.observation().throttled, 1);

        queue.start_event_loop_iteration();
        sender
            .send(TestMessage::task(TaskSourceName::Networking))
            .unwrap();
        assert!(matches!(
            queue.take_one_task_and_recv(&fully_active),
            Ok(TestMessage::Task {
                source: TaskSourceName::Networking,
                ..
            })
        ));
        assert_eq!(queue.observation().throttled, 1);
        assert!(matches!(
            queue.take_one_task_and_recv(&fully_active),
            Ok(TestMessage::Task {
                source: TaskSourceName::PerformanceTimeline,
                ..
            })
        ));
    }

    #[test]
    fn controlled_poll_promotes_at_most_one_retained_throttle() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let queue = TaskQueue::new(receiver, sender.clone());
        let fully_active = FxHashSet::default();
        queue.start_event_loop_iteration();

        for _ in 0..6 {
            sender
                .send(TestMessage::task(TaskSourceName::Timer))
                .unwrap();
            drop(queue.take_one_task_and_recv(&fully_active).unwrap());
        }
        for _ in 0..2 {
            sender
                .send(TestMessage::task(TaskSourceName::PerformanceTimeline))
                .unwrap();
            assert!(matches!(
                queue.take_one_task_and_recv(&fully_active),
                Ok(TestMessage::WakeUp)
            ));
        }
        assert_eq!(queue.observation().throttled, 2);

        queue.start_event_loop_iteration();
        assert!(matches!(
            queue.take_one_task_and_recv(&fully_active),
            Ok(TestMessage::Task {
                source: TaskSourceName::PerformanceTimeline,
                ..
            })
        ));
        assert_eq!(queue.observation().throttled, 1);
        assert!(matches!(
            queue.take_one_task_and_recv(&fully_active),
            Ok(TestMessage::Task {
                source: TaskSourceName::PerformanceTimeline,
                ..
            })
        ));
        assert_eq!(queue.observation().throttled, 0);
    }

    #[test]
    fn controlled_poll_does_not_drop_task_after_exact_wake_scan_limit() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let queue = TaskQueue::new(receiver, sender.clone());
        let fully_active = FxHashSet::default();

        for _ in 0..64 {
            sender.send(TestMessage::WakeUp).unwrap();
        }
        sender
            .send(TestMessage::task(TaskSourceName::Timer))
            .unwrap();

        assert!(matches!(
            queue.take_one_task_and_recv(&fully_active),
            Ok(TestMessage::WakeUp)
        ));
        assert!(matches!(
            queue.take_one_task_and_recv(&fully_active),
            Ok(TestMessage::Task {
                source: TaskSourceName::Timer,
                ..
            })
        ));
    }

    #[test]
    fn observation_is_side_effect_free_and_pipeline_discard_purges_all_classes() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (sender, receiver) = crossbeam_channel::unbounded();
        let queue = TaskQueue::new_with_producer_tracking(receiver, sender, true);
        let fully_active = FxHashSet::default();

        queue.take_one_incoming_task(
            TestMessage::Task {
                source: TaskSourceName::Timer,
                pipeline_id: None,
            },
            &fully_active,
        );
        queue.take_one_incoming_task(
            TestMessage::Task {
                source: TaskSourceName::PerformanceTimeline,
                pipeline_id: Some(TEST_PIPELINE_ID),
            },
            &fully_active,
        );
        queue.take_one_incoming_task(
            TestMessage::Task {
                source: TaskSourceName::Timer,
                pipeline_id: Some(TEST_PIPELINE_ID),
            },
            &fully_active,
        );

        let before = TaskQueueObservation {
            ready: 1,
            throttled: 1,
            inactive: 1,
        };
        assert_eq!(queue.observation(), before);
        assert_eq!(queue.observation(), before);

        queue.discard_pipeline(TEST_PIPELINE_ID);
        assert_eq!(
            queue.observation(),
            TaskQueueObservation {
                ready: 1,
                throttled: 0,
                inactive: 0,
            }
        );
    }
}
