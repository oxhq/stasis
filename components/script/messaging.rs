/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use core::fmt;
#[cfg(feature = "webgpu")]
use std::cell::RefCell;
use std::option::Option;
use std::result::Result;

use crossbeam_channel::{Receiver, Select, SelectedOperation, Sender};
use devtools_traits::{DevtoolScriptControlMsg, ScriptToDevtoolsControlMsg};
use embedder_traits::{EmbedderControlId, EmbedderControlResponse, ScriptToEmbedderChan};
use net_traits::FetchResponseMsg;
use net_traits::image_cache::ImageCacheResponseMessage;
use profile_traits::mem::{self as profile_mem, OpaqueSender, ReportsChan};
use profile_traits::time::{self as profile_time};
use rustc_hash::FxHashSet;
use script_traits::{Painter, ScriptThreadMessage};
use servo_base::generic_channel::{GenericCallback, GenericSender, RoutedReceiver};
use servo_base::id::{PipelineId, WebViewId};
#[cfg(feature = "bluetooth")]
use servo_bluetooth_traits::BluetoothRequest;
use servo_constellation_traits::ScriptToConstellationMessage;
use stylo_atoms::Atom;
use timers::{
    DocumentProducerFence, DocumentProducerFenceError, DocumentProducerKind, TimerScheduler,
};
#[cfg(feature = "webgpu")]
use webgpu_traits::WebGPUMsg;

use crate::dom::abstractworker::WorkerScriptMsg;
use crate::dom::bindings::trace::CustomTraceable;
use crate::dom::csp::Violation;
use crate::dom::dedicatedworkerglobalscope::DedicatedWorkerScriptMsg;
use crate::dom::serviceworkerglobalscope::ServiceWorkerScriptMsg;
use crate::dom::sharedworkerglobalscope::SharedWorkerScriptMsg;
use crate::dom::worker::TrustedWorkerAddress;
use crate::dom::{WorkletControl, WorkletExecutor};
use crate::producer_fence::ProducerFencedTaskBox;
use crate::script_runtime::ScriptThreadEventCategory;
use crate::tasks::task::TaskBox;
use crate::tasks::task_queue::{QueuedTask, QueuedTaskConversion, TaskQueue};
use crate::tasks::task_source::TaskSourceName;

#[expect(clippy::large_enum_variant)]
#[derive(Debug)]
pub(crate) enum MixedMessage {
    FromConstellation(ScriptThreadMessage),
    FromScript(MainThreadScriptMsg),
    FromDevtools(DevtoolScriptControlMsg),
    FromImageCache(ImageCacheResponseMessage),
    #[cfg(feature = "webgpu")]
    FromWebGPUServer(WebGPUMsg),
    TimerFired,
}

impl MixedMessage {
    pub(crate) fn pipeline_id(&self) -> Option<PipelineId> {
        match self {
            MixedMessage::FromConstellation(inner_msg) => match inner_msg {
                ScriptThreadMessage::StopDelayingLoadEventsMode(id) => Some(*id),
                ScriptThreadMessage::SpawnPipeline(new_pipeline_info) => new_pipeline_info
                    .parent_info
                    .or(Some(new_pipeline_info.new_pipeline_id)),
                ScriptThreadMessage::Resize(id, ..) => Some(*id),
                ScriptThreadMessage::ThemeChange(id, ..) => Some(*id),
                ScriptThreadMessage::ResizeInactive(id, ..) => Some(*id),
                ScriptThreadMessage::UnloadDocument(id) => Some(*id),
                ScriptThreadMessage::ExitPipeline(_webview_id, id, ..) => Some(*id),
                ScriptThreadMessage::ExitScriptThread => None,
                ScriptThreadMessage::SendInputEvent(_, id, _) => Some(*id),
                ScriptThreadMessage::RefreshCursor(id, ..) => Some(*id),
                ScriptThreadMessage::GetTitle(id) => Some(*id),
                ScriptThreadMessage::GetDocumentOrigin(id, _) => Some(*id),
                ScriptThreadMessage::SetDocumentActivity(id, ..) => Some(*id),
                ScriptThreadMessage::SetThrottled(_, id, ..) => Some(*id),
                ScriptThreadMessage::SetThrottledInContainingIframe(_, id, ..) => Some(*id),
                ScriptThreadMessage::NavigateIframe(id, ..) => Some(*id),
                ScriptThreadMessage::PostMessage { target: id, .. } => Some(*id),
                ScriptThreadMessage::UpdatePipelineId(_, _, _, id, _) => Some(*id),
                ScriptThreadMessage::UpdateHistoryState(id, ..) => Some(*id),
                ScriptThreadMessage::RemoveHistoryStates(id, ..) => Some(*id),

                ScriptThreadMessage::FocusDocumentAsPartOfFocusingSteps(id, ..) => Some(*id),
                ScriptThreadMessage::UnfocusDocumentAsPartOfFocusingSteps(id, ..) => Some(*id),
                ScriptThreadMessage::FocusDocument(id, ..) => Some(*id),
                ScriptThreadMessage::WebDriverScriptCommand(id, ..) => Some(*id),
                ScriptThreadMessage::TickAllAnimations(..) => None,
                ScriptThreadMessage::WebFontLoadFinished(id, ..) => Some(*id),
                ScriptThreadMessage::DispatchIFrameLoadEvent {
                    target: _,
                    parent: id,
                    child: _,
                } => Some(*id),
                ScriptThreadMessage::DispatchStorageEvent(id, ..) => Some(*id),
                ScriptThreadMessage::ReportCSSError(id, ..) => Some(*id),
                ScriptThreadMessage::Reload(id, ..) => Some(*id),
                ScriptThreadMessage::PaintMetric(id, ..) => Some(*id),
                ScriptThreadMessage::ExitFullScreen(id, ..) => Some(*id),
                ScriptThreadMessage::MediaSessionAction(..) => None,
                #[cfg(feature = "webgpu")]
                ScriptThreadMessage::SetWebGPUPort(..) => None,
                ScriptThreadMessage::SetScrollStates(id, ..) => Some(*id),
                ScriptThreadMessage::EvaluateJavaScript(_, id, _, _) => Some(*id),
                ScriptThreadMessage::SendImageKeysBatch(..) => None,
                ScriptThreadMessage::PreferencesUpdated(..) => None,
                ScriptThreadMessage::NoLongerWaitingOnAsychronousImageUpdates(_) => None,
                ScriptThreadMessage::ForwardKeyboardScroll(id, _) => Some(*id),
                ScriptThreadMessage::RequestScreenshotReadiness(_, id) => Some(*id),
                ScriptThreadMessage::EmbedderControlResponse(id, _) => Some(id.pipeline_id),
                ScriptThreadMessage::SetUserContents(..) => None,
                ScriptThreadMessage::DestroyUserContentManager(..) => None,
                ScriptThreadMessage::UpdatePinchZoomInfos(id, _) => Some(*id),
                ScriptThreadMessage::SetAccessibilityActive(..) => None,
                ScriptThreadMessage::TriggerGarbageCollection => None,
            },
            MixedMessage::FromScript(inner_msg) => match inner_msg {
                MainThreadScriptMsg::Common(CommonScriptMsg::Task(_, _, pipeline_id, _)) => {
                    *pipeline_id
                },
                MainThreadScriptMsg::Common(CommonScriptMsg::CollectReports(_)) => None,
                MainThreadScriptMsg::Common(CommonScriptMsg::ReportCspViolations(
                    pipeline_id,
                    _,
                )) => Some(*pipeline_id),
                MainThreadScriptMsg::NavigationResponse { pipeline_id, .. } => Some(*pipeline_id),
                MainThreadScriptMsg::WorkletLoaded(pipeline_id) => Some(*pipeline_id),
                MainThreadScriptMsg::RegisterPaintWorklet { pipeline_id, .. } => Some(*pipeline_id),
                MainThreadScriptMsg::Inactive => None,
                MainThreadScriptMsg::WakeUp => None,
                MainThreadScriptMsg::ForwardEmbedderControlResponseFromFileManager(
                    control_id,
                    ..,
                ) => Some(control_id.pipeline_id),
            },
            MixedMessage::FromImageCache(response) => match response {
                ImageCacheResponseMessage::NotifyPendingImageLoadStatus(response) => {
                    Some(response.pipeline_id)
                },
                ImageCacheResponseMessage::VectorImageRasterizationComplete(response) => {
                    Some(response.pipeline_id)
                },
            },
            MixedMessage::FromDevtools(_) | MixedMessage::TimerFired => None,
            #[cfg(feature = "webgpu")]
            MixedMessage::FromWebGPUServer(..) => None,
        }
    }
}

/// Messages used to control the script event loop.
#[derive(Debug)]
pub(crate) enum MainThreadScriptMsg {
    /// Common variants associated with the script messages
    Common(CommonScriptMsg),
    /// Notifies the script thread that a new worklet has been loaded, and thus the page should be
    /// reflowed.
    WorkletLoaded(PipelineId),
    NavigationResponse {
        pipeline_id: PipelineId,
        message: Box<FetchResponseMsg>,
    },
    /// Notifies the script thread that a new paint worklet has been registered.
    RegisterPaintWorklet {
        pipeline_id: PipelineId,
        name: Atom,
        properties: Vec<Atom>,
        painter: Box<dyn Painter>,
    },
    /// A task related to a not fully-active document has been throttled.
    Inactive,
    /// Wake-up call from the task queue.
    WakeUp,
    /// The `FileManagerThread` has finished selecting files is forwarding the response to
    /// the main thread of this `ScriptThread`.
    ForwardEmbedderControlResponseFromFileManager(EmbedderControlId, EmbedderControlResponse),
}

/// Common messages used to control the event loops in both the script, the worker, and the
/// worklet
pub(crate) enum CommonScriptMsg {
    /// Requests that the script thread measure its memory usage. The results are sent back via the
    /// supplied channel.
    CollectReports(ReportsChan),
    /// Generic message that encapsulates event handling.
    Task(
        ScriptThreadEventCategory,
        Box<dyn TaskBox>,
        Option<PipelineId>,
        TaskSourceName,
    ),
    /// Report CSP violations in the script
    ReportCspViolations(PipelineId, Vec<Violation>),
}

/// A checked failure to hand work to a script event loop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScriptEventLoopSendError {
    /// The receiving event loop has shut down.
    ChannelClosed,
    /// Producer admission failed and the owning fence latched the terminal.
    Producer(DocumentProducerFenceError),
}

impl fmt::Display for ScriptEventLoopSendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ChannelClosed => formatter.write_str("script event-loop channel closed"),
            Self::Producer(error) => {
                write!(formatter, "document producer admission failed: {error}")
            },
        }
    }
}

impl fmt::Debug for CommonScriptMsg {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            CommonScriptMsg::CollectReports(_) => write!(f, "CollectReports(...)"),
            CommonScriptMsg::Task(ref category, ref task, _, _) => {
                f.debug_tuple("Task").field(category).field(task).finish()
            },
            CommonScriptMsg::ReportCspViolations(..) => write!(f, "ReportCspViolations(...)"),
        }
    }
}

/// A wrapper around various types of `Sender`s that send messages back to the event loop
/// of a script context event loop. This will either target the main `ScriptThread` event
/// loop or that of a worker.
#[derive(Clone, JSTraceable, MallocSizeOf)]
pub(crate) enum ScriptEventLoopSender {
    /// A sender that sends to the main `ScriptThread` event loop.
    MainThread {
        sender: Sender<MainThreadScriptMsg>,
        #[no_trace]
        #[ignore_malloc_size_of = "The producer fence is shared by the ScriptThread"]
        producer_fence: Option<DocumentProducerFence>,
    },
    /// A sender that sends to a `SharedWorker` event loop.
    SharedWorker(Sender<SharedWorkerScriptMsg>),
    /// A sender that sends to a `ServiceWorker` event loop.
    ServiceWorker(Sender<ServiceWorkerScriptMsg>),
    /// A wrapper that sends to the event loops of all threads belonging to a `Worklet`.
    ///
    /// Worklets intentionally do not participate in this ScriptThread-owned fence: controlled
    /// mode rejects the Worklet surface before its separate event loop can be created.
    Worklet(WorkletExecutor),
    /// A sender that sends to a dedicated worker (such as a generic Web Worker) event loop.
    /// Note that this sender keeps the main thread Worker DOM object alive as long as it or
    /// or any message it sends is not dropped.
    DedicatedWorker {
        sender: Sender<DedicatedWorkerScriptMsg>,
        main_thread_worker: TrustedWorkerAddress,
    },
}

impl ScriptEventLoopSender {
    /// Send a message to the event loop, which might be a main thread event loop or a worker event loop.
    pub(crate) fn send(
        &self,
        mut message: CommonScriptMsg,
    ) -> Result<(), ScriptEventLoopSendError> {
        match self {
            Self::MainThread {
                sender,
                producer_fence,
            } => {
                let notify_after_commit = if let Some(producer_fence) = producer_fence {
                    if let CommonScriptMsg::Task(category, task, pipeline_id, task_source) = message
                    {
                        let guard = producer_fence
                            .begin(DocumentProducerKind::Task)
                            .map_err(ScriptEventLoopSendError::Producer)?;
                        message = CommonScriptMsg::Task(
                            category,
                            Box::new(ProducerFencedTaskBox::new(task, guard)),
                            pipeline_id,
                            task_source,
                        );
                        Some(producer_fence)
                    } else {
                        None
                    }
                } else {
                    None
                };
                match sender.send(MainThreadScriptMsg::Common(message)) {
                    Ok(()) => {
                        if let Some(producer_fence) = notify_after_commit {
                            // `begin` wakes before this queue mutation. Wake again after commit so
                            // an owner that observed Busy between the two cannot go to sleep with
                            // a newly queued task and no subsequent notification.
                            producer_fence.notify_observer_after_commit();
                        }
                        Ok(())
                    },
                    Err(_) => Err(ScriptEventLoopSendError::ChannelClosed),
                }
            },
            Self::SharedWorker(sender) => sender
                .send(SharedWorkerScriptMsg::CommonWorker(
                    WorkerScriptMsg::Common(message),
                ))
                .map_err(|_| ScriptEventLoopSendError::ChannelClosed),
            Self::ServiceWorker(sender) => sender
                .send(ServiceWorkerScriptMsg::CommonWorker(
                    WorkerScriptMsg::Common(message),
                ))
                .map_err(|_| ScriptEventLoopSendError::ChannelClosed),
            Self::DedicatedWorker {
                sender,
                main_thread_worker,
            } => {
                let common_message = WorkerScriptMsg::Common(message);
                sender
                    .send(DedicatedWorkerScriptMsg::CommonWorker(
                        main_thread_worker.clone(),
                        common_message,
                    ))
                    .map_err(|_| ScriptEventLoopSendError::ChannelClosed)
            },
            Self::Worklet(executor) => executor
                .send_control_message(WorkletControl::Common(message))
                .map_err(|_| ScriptEventLoopSendError::ChannelClosed),
        }
    }
}

/// A wrapper around various types of `Receiver`s that receive event loop messages. Used for
/// synchronous DOM APIs that need to abstract over multiple kinds of event loops (worker/main
/// thread) with different Receiver interfaces.
pub(crate) enum ScriptEventLoopReceiver {
    /// A receiver that receives messages to the main `ScriptThread` event loop.
    MainThread(Receiver<MainThreadScriptMsg>),
    /// A receiver that receives messages to shared worker event loops.
    SharedWorker(Receiver<SharedWorkerScriptMsg>),
    /// A receiver that receives messages to dedicated workers (such as a generic Web Worker) event loop.
    DedicatedWorker(Receiver<DedicatedWorkerScriptMsg>),
}

impl ScriptEventLoopReceiver {
    pub(crate) fn recv(&self) -> Result<CommonScriptMsg, ()> {
        match self {
            Self::MainThread(receiver) => match receiver.recv() {
                Ok(MainThreadScriptMsg::Common(script_msg)) => Ok(script_msg),
                Ok(_) => panic!("unexpected main thread event message!"),
                Err(_) => Err(()),
            },
            Self::SharedWorker(receiver) => match receiver.recv() {
                Ok(SharedWorkerScriptMsg::CommonWorker(WorkerScriptMsg::Common(message))) => {
                    Ok(message)
                },
                Ok(_) => panic!("unexpected shared worker event message!"),
                Err(_) => Err(()),
            },
            Self::DedicatedWorker(receiver) => match receiver.recv() {
                Ok(DedicatedWorkerScriptMsg::CommonWorker(_, WorkerScriptMsg::Common(message))) => {
                    Ok(message)
                },
                Ok(_) => panic!("unexpected worker event message!"),
                Err(_) => Err(()),
            },
        }
    }
}

impl QueuedTaskConversion for MainThreadScriptMsg {
    fn task_source_name(&self) -> Option<&TaskSourceName> {
        let script_msg = match self {
            MainThreadScriptMsg::Common(script_msg) => script_msg,
            _ => return None,
        };
        match script_msg {
            CommonScriptMsg::Task(_category, _boxed, _pipeline_id, task_source) => {
                Some(task_source)
            },
            _ => None,
        }
    }

    fn pipeline_id(&self) -> Option<PipelineId> {
        let script_msg = match self {
            MainThreadScriptMsg::Common(script_msg) => script_msg,
            _ => return None,
        };
        match script_msg {
            CommonScriptMsg::Task(_category, _boxed, pipeline_id, _task_source) => *pipeline_id,
            _ => None,
        }
    }

    fn into_queued_task(self) -> Option<QueuedTask> {
        let script_msg = match self {
            MainThreadScriptMsg::Common(script_msg) => script_msg,
            _ => return None,
        };
        let (event_category, task, pipeline_id, task_source) = match script_msg {
            CommonScriptMsg::Task(category, boxed, pipeline_id, task_source) => {
                (category, boxed, pipeline_id, task_source)
            },
            _ => return None,
        };
        Some(QueuedTask {
            worker: None,
            event_category,
            task,
            pipeline_id,
            task_source,
        })
    }

    fn from_queued_task(queued_task: QueuedTask) -> Self {
        let script_msg = CommonScriptMsg::Task(
            queued_task.event_category,
            queued_task.task,
            queued_task.pipeline_id,
            queued_task.task_source,
        );
        MainThreadScriptMsg::Common(script_msg)
    }

    fn inactive_msg() -> Self {
        MainThreadScriptMsg::Inactive
    }

    fn wake_up_msg() -> Self {
        MainThreadScriptMsg::WakeUp
    }

    fn is_wake_up(&self) -> bool {
        matches!(self, MainThreadScriptMsg::WakeUp)
    }
}

impl OpaqueSender<CommonScriptMsg> for ScriptEventLoopSender {
    fn send(&self, message: CommonScriptMsg) {
        self.send(message).unwrap()
    }
}

#[cfg(test)]
mod producer_fence_tests {
    use std::sync::{Arc, Mutex};

    use embedder_traits::{EventLoopWaker, ScriptToEmbedderChan};
    use servo_base::id::TEST_PIPELINE_ID;
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

    struct NeverRunTask;

    impl TaskBox for NeverRunTask {
        fn name(&self) -> &'static str {
            "NeverRunTask"
        }

        fn run_box(self: Box<Self>, _: &mut js::context::JSContext) {
            panic!("producer-fence transport tests never execute tasks")
        }
    }

    #[derive(Clone)]
    struct QueueObservingWaker {
        receiver: Receiver<MainThreadScriptMsg>,
        observations: Arc<Mutex<Vec<bool>>>,
    }

    impl EventLoopWaker for QueueObservingWaker {
        fn clone_box(&self) -> Box<dyn EventLoopWaker> {
            Box::new(self.clone())
        }

        fn wake(&self) {
            self.observations
                .lock()
                .unwrap()
                .push(!self.receiver.is_empty());
        }
    }

    fn task_message(
        pipeline_id: Option<PipelineId>,
        task_source: TaskSourceName,
    ) -> CommonScriptMsg {
        CommonScriptMsg::Task(
            ScriptThreadEventCategory::ScriptEvent,
            Box::new(NeverRunTask),
            pipeline_id,
            task_source,
        )
    }

    fn main_thread_sender(
        sender: Sender<MainThreadScriptMsg>,
        producer_fence: &DocumentProducerFence,
    ) -> ScriptEventLoopSender {
        ScriptEventLoopSender::MainThread {
            sender,
            producer_fence: Some(producer_fence.clone()),
        }
    }

    #[test]
    fn successful_main_thread_send_stays_fenced_through_the_ready_queue() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender.clone(), &producer_fence);
        let task_queue = TaskQueue::new_with_producer_tracking(receiver, raw_sender, true);

        event_loop_sender
            .send(task_message(None, TaskSourceName::Timer))
            .unwrap();
        assert_eq!(producer_fence.snapshot().pending(), 1);

        let message = task_queue
            .take_tasks_and_recv(&FxHashSet::default())
            .unwrap();
        assert_eq!(producer_fence.snapshot().pending(), 1);

        drop(message);
        let complete = producer_fence.snapshot();
        assert!(complete.is_empty());
        assert_eq!(complete.for_kind(DocumentProducerKind::Task).completed(), 1);
    }

    #[test]
    fn successful_send_wakes_before_and_after_queue_commit_without_an_embedder_message() {
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        let observations = Arc::new(Mutex::new(Vec::new()));
        let (embedder_message_sender, embedder_message_receiver) = crossbeam_channel::unbounded();
        let embedder_sender = ScriptToEmbedderChan::new(
            embedder_message_sender,
            Box::new(QueueObservingWaker {
                receiver: receiver.clone(),
                observations: observations.clone(),
            }),
        );
        let notifier_sender = embedder_sender.clone();
        let producer_fence = DocumentProducerFence::with_notifier(Some(Arc::new(move || {
            let _ = notifier_sender.wake();
        })));
        let event_loop_sender = main_thread_sender(raw_sender, &producer_fence);

        event_loop_sender
            .send(task_message(None, TaskSourceName::Timer))
            .unwrap();

        assert_eq!(*observations.lock().unwrap(), vec![false, true]);
        assert!(matches!(
            embedder_message_receiver.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ));

        drop(receiver.recv().unwrap());
        assert_eq!(*observations.lock().unwrap(), vec![false, true, false]);
    }

    #[test]
    fn failed_main_thread_send_completes_the_task_ticket() {
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        drop(receiver);
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender, &producer_fence);

        assert_eq!(
            event_loop_sender.send(task_message(None, TaskSourceName::Timer)),
            Err(ScriptEventLoopSendError::ChannelClosed)
        );
        let complete = producer_fence.snapshot();
        assert!(complete.is_empty());
        assert_eq!(complete.revision(), 2);
        assert_eq!(complete.for_kind(DocumentProducerKind::Task).completed(), 1);
    }

    #[test]
    fn discarding_an_inactive_task_queue_completes_its_ticket() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender.clone(), &producer_fence);
        let task_queue = TaskQueue::new_with_producer_tracking(receiver, raw_sender, true);

        event_loop_sender
            .send(task_message(Some(TEST_PIPELINE_ID), TaskSourceName::Timer))
            .unwrap();
        task_queue.take_tasks(MainThreadScriptMsg::WakeUp, &FxHashSet::default());
        assert!(matches!(
            task_queue.recv(),
            Ok(MainThreadScriptMsg::Inactive)
        ));
        assert!(task_queue.recv().is_err());
        assert_eq!(producer_fence.snapshot().pending(), 1);

        drop(task_queue);
        assert!(producer_fence.snapshot().is_empty());
    }

    #[test]
    fn discarding_a_throttled_task_queue_completes_its_ticket() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender.clone(), &producer_fence);
        let task_queue =
            TaskQueue::new_with_producer_tracking(receiver, raw_sender.clone(), true);

        for _ in 0..6 {
            event_loop_sender
                .send(task_message(None, TaskSourceName::Timer))
                .unwrap();
        }
        event_loop_sender
            .send(task_message(None, TaskSourceName::PerformanceTimeline))
            .unwrap();
        task_queue.take_tasks(MainThreadScriptMsg::WakeUp, &FxHashSet::default());
        for expected_pending in (1..=6).rev() {
            drop(task_queue.recv().unwrap());
            assert_eq!(producer_fence.snapshot().pending(), expected_pending);
        }
        assert!(task_queue.recv().is_err());
        assert_eq!(producer_fence.snapshot().pending(), 1);

        drop(task_queue);
        let complete = producer_fence.snapshot();
        assert!(complete.is_empty());
        assert_eq!(complete.for_kind(DocumentProducerKind::Task).completed(), 7);
    }

    #[test]
    fn discarding_a_pipeline_purges_local_queues_and_filters_channel_on_intake() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender.clone(), &producer_fence);
        let task_queue = TaskQueue::new_with_producer_tracking(receiver, raw_sender, true);
        let mut fully_active = FxHashSet::default();
        fully_active.insert(TEST_PIPELINE_ID);

        for _ in 0..6 {
            event_loop_sender
                .send(task_message(Some(TEST_PIPELINE_ID), TaskSourceName::Timer))
                .unwrap();
        }
        event_loop_sender
            .send(task_message(
                Some(TEST_PIPELINE_ID),
                TaskSourceName::PerformanceTimeline,
            ))
            .unwrap();
        task_queue.take_tasks(MainThreadScriptMsg::WakeUp, &fully_active);

        event_loop_sender
            .send(task_message(Some(TEST_PIPELINE_ID), TaskSourceName::Timer))
            .unwrap();
        task_queue.take_tasks(MainThreadScriptMsg::WakeUp, &FxHashSet::default());

        event_loop_sender
            .send(task_message(Some(TEST_PIPELINE_ID), TaskSourceName::Timer))
            .unwrap();
        event_loop_sender
            .send(task_message(None, TaskSourceName::Timer))
            .unwrap();
        assert_eq!(producer_fence.snapshot().pending(), 10);

        task_queue.discard_pipeline(TEST_PIPELINE_ID);

        // Ready, throttled, and inactive tasks were purged synchronously. Channel input remains
        // bounded by normal intake, where the tombstone filters the target and preserves global
        // work without dropping the TaskQueue.
        assert_eq!(producer_fence.snapshot().pending(), 2);
        let global_task = task_queue
            .take_tasks_and_recv(&FxHashSet::default())
            .unwrap();
        assert_eq!(producer_fence.snapshot().pending(), 1);
        assert_eq!(global_task.pipeline_id(), None);
        drop(global_task);
        assert!(producer_fence.snapshot().is_empty());
    }

    #[test]
    fn task_for_a_pipeline_arriving_after_discard_is_dropped_on_intake() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender.clone(), &producer_fence);
        let task_queue = TaskQueue::new_with_producer_tracking(receiver, raw_sender, true);
        task_queue.discard_pipeline(TEST_PIPELINE_ID);

        event_loop_sender
            .send(task_message(Some(TEST_PIPELINE_ID), TaskSourceName::Timer))
            .unwrap();
        event_loop_sender
            .send(task_message(None, TaskSourceName::Timer))
            .unwrap();
        assert_eq!(producer_fence.snapshot().pending(), 2);

        let global_task = task_queue
            .take_tasks_and_recv(&FxHashSet::default())
            .unwrap();
        assert_eq!(producer_fence.snapshot().pending(), 1);
        assert_eq!(global_task.pipeline_id(), None);
        drop(global_task);
        assert!(producer_fence.snapshot().is_empty());

        event_loop_sender
            .send(task_message(None, TaskSourceName::Timer))
            .unwrap();
        drop(
            task_queue
                .take_tasks_and_recv(&FxHashSet::default())
                .unwrap(),
        );
        assert!(producer_fence.snapshot().is_empty());
    }

    #[test]
    fn realtime_transport_does_not_install_producer_tracking_or_pipeline_tombstones() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        let event_loop_sender = ScriptEventLoopSender::MainThread {
            sender: raw_sender.clone(),
            producer_fence: None,
        };
        let task_queue = TaskQueue::new(receiver, raw_sender);
        let mut fully_active = FxHashSet::default();
        fully_active.insert(TEST_PIPELINE_ID);

        task_queue.discard_pipeline(TEST_PIPELINE_ID);
        event_loop_sender
            .send(task_message(Some(TEST_PIPELINE_ID), TaskSourceName::Timer))
            .unwrap();

        let message = task_queue.take_tasks_and_recv(&fully_active).unwrap();
        assert_eq!(message.pipeline_id(), Some(TEST_PIPELINE_ID));
        drop(message);
    }
}

#[derive(Clone, JSTraceable)]
pub(crate) struct ScriptThreadSenders {
    /// A channel to hand out to script thread-based entities that need to be able to enqueue
    /// events in the event queue.
    pub(crate) self_sender: Sender<MainThreadScriptMsg>,

    /// A handle to the bluetooth thread.
    #[no_trace]
    #[cfg(feature = "bluetooth")]
    pub(crate) bluetooth_sender: GenericSender<BluetoothRequest>,

    /// A [`Sender`] that sends messages to the `ScriptThread`.
    #[no_trace]
    pub(crate) constellation_sender: GenericSender<ScriptThreadMessage>,

    /// A [`Sender`] that sends messages to the `Constellation` associated with
    /// particular pipelines.
    #[no_trace]
    pub(crate) pipeline_to_constellation_sender:
        GenericSender<(WebViewId, PipelineId, ScriptToConstellationMessage)>,

    /// A channel to send messages to the Embedder.
    #[no_trace]
    pub(crate) pipeline_to_embedder_sender: ScriptToEmbedderChan,

    /// The shared [`Sender`] which is sent to the `ImageCache` when requesting an image.
    /// Messages on this channel are sent to [`ScriptThreadReceivers::image_cache_receiver`].
    #[no_trace]
    pub(crate) image_cache_sender: Sender<ImageCacheResponseMessage>,

    /// For providing contact with the time profiler.
    #[no_trace]
    pub(crate) time_profiler_sender: profile_time::ProfilerChan,

    /// For providing contact with the memory profiler.
    #[no_trace]
    pub(crate) memory_profiler_sender: profile_mem::ProfilerChan,

    /// For providing instructions to an optional devtools server.
    #[no_trace]
    pub(crate) devtools_server_sender: Option<GenericCallback<ScriptToDevtoolsControlMsg>>,

    #[no_trace]
    pub(crate) devtools_client_to_script_thread_sender: GenericSender<DevtoolScriptControlMsg>,
}

#[derive(JSTraceable)]
pub(crate) struct ScriptThreadReceivers {
    /// A [`Receiver`] that receives messages from the constellation.
    #[no_trace]
    pub(crate) constellation_receiver: RoutedReceiver<ScriptThreadMessage>,

    /// The [`Receiver`] which receives incoming messages from the `ImageCache`.
    #[no_trace]
    pub(crate) image_cache_receiver: Receiver<ImageCacheResponseMessage>,

    /// For receiving commands from an optional devtools server. Will be ignored if no such server
    /// exists. When devtools are not active this will be [`crossbeam_channel::never()`].
    #[no_trace]
    pub(crate) devtools_server_receiver: RoutedReceiver<DevtoolScriptControlMsg>,

    /// Receiver to receive commands from optional WebGPU server. When there is no active
    /// WebGPU context, this will be [`crossbeam_channel::never()`].
    #[no_trace]
    #[cfg(feature = "webgpu")]
    pub(crate) webgpu_receiver: RefCell<RoutedReceiver<WebGPUMsg>>,
}

impl ScriptThreadReceivers {
    /// Block until a message is received by any of the receivers of this [`ScriptThreadReceivers`]
    /// or the given [`TaskQueue`] or [`TimerScheduler`]. Return the first message received.
    pub(crate) fn recv(
        &self,
        task_queue: &TaskQueue<MainThreadScriptMsg>,
        timer_scheduler: &TimerScheduler,
        fully_active: &FxHashSet<PipelineId>,
    ) -> MixedMessage {
        let mut select = Select::new();

        let task_recv = task_queue.select();
        let task_index = select.recv(task_recv);
        let constellation_index = select.recv(&self.constellation_receiver);
        let devtools_index = select.recv(&self.devtools_server_receiver);
        let image_cache_index = select.recv(&self.image_cache_receiver);

        #[cfg(feature = "webgpu")]
        let webgpu_receiver = self.webgpu_receiver.borrow();
        #[cfg(feature = "webgpu")]
        let webgpu_index = select.recv(&*webgpu_receiver);

        let message_from_operation = |operation: SelectedOperation| {
            let index = operation.index();
            if index == task_index {
                let msg = operation.recv(task_recv).unwrap();
                task_queue.take_tasks(msg, fully_active);
                let event = task_queue.recv().expect(
                    "Spurious wake-up of the event-loop, task-queue has no tasks available",
                );
                MixedMessage::FromScript(event)
            } else if index == constellation_index {
                MixedMessage::FromConstellation(
                    operation
                        .recv(&self.constellation_receiver)
                        .unwrap()
                        .unwrap(),
                )
            } else if index == devtools_index {
                MixedMessage::FromDevtools(
                    operation
                        .recv(&self.devtools_server_receiver)
                        .unwrap()
                        .unwrap(),
                )
            } else if index == image_cache_index {
                MixedMessage::FromImageCache(operation.recv(&self.image_cache_receiver).unwrap())
            } else {
                #[cfg(feature = "webgpu")]
                {
                    debug_assert_eq!(index, webgpu_index);
                    MixedMessage::FromWebGPUServer(
                        operation.recv(&*webgpu_receiver).unwrap().unwrap(),
                    )
                }
                #[cfg(not(feature = "webgpu"))]
                unreachable!("select returned an unknown index {index}")
            }
        };

        if let Some(deadline) = timer_scheduler.next_deadline() {
            select
                .select_deadline(deadline)
                .map(message_from_operation)
                .unwrap_or(MixedMessage::TimerFired)
        } else {
            message_from_operation(select.select())
        }
    }

    /// Try to receive a from any of the receivers of this [`ScriptThreadReceivers`] or the given
    /// [`TaskQueue`]. Return `None` if no messages are ready to be received.
    pub(crate) fn try_recv(
        &self,
        task_queue: &TaskQueue<MainThreadScriptMsg>,
        fully_active: &FxHashSet<PipelineId>,
    ) -> Option<MixedMessage> {
        if let Ok(message) = self.constellation_receiver.try_recv() {
            let message = message
                .inspect_err(|e| {
                    log::warn!(
                        "ScriptThreadReceivers IPC error on constellation_receiver: {:?}",
                        e
                    );
                })
                .ok()?;
            return MixedMessage::FromConstellation(message).into();
        }
        if let Ok(message) = task_queue.take_tasks_and_recv(fully_active) {
            return MixedMessage::FromScript(message).into();
        }
        if let Ok(message) = self.devtools_server_receiver.try_recv() {
            return MixedMessage::FromDevtools(message.unwrap()).into();
        }
        if let Ok(message) = self.image_cache_receiver.try_recv() {
            return MixedMessage::FromImageCache(message).into();
        }
        #[cfg(feature = "webgpu")]
        if let Ok(message) = self.webgpu_receiver.borrow().try_recv() {
            return MixedMessage::FromWebGPUServer(message.unwrap()).into();
        }
        None
    }
}
