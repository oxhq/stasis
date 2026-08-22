/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use core::fmt;
#[cfg(feature = "webgpu")]
use std::cell::RefCell;
use std::option::Option;
use std::result::Result;
use std::time::Duration;

use crossbeam_channel::{Receiver, RecvTimeoutError, Select, SelectedOperation, Sender};
use devtools_traits::{DevtoolScriptControlMsg, ScriptToDevtoolsControlMsg};
use embedder_traits::{EmbedderControlId, EmbedderControlResponse, ScriptToEmbedderChan};
use net_traits::image_cache::ImageCacheResponseMessage;
use net_traits::{BoxedFetchCallback, FetchResponseMsg};
use profile_traits::mem::{self as profile_mem, OpaqueSender, ReportsChan};
use profile_traits::time::{self as profile_time};
use rustc_hash::FxHashSet;
use script_traits::{Painter, ScriptThreadControlMessage, ScriptThreadMessage};
use servo_base::generic_channel::{GenericCallback, GenericSender, RoutedReceiver};
use servo_base::id::{PipelineId, WebViewId};
#[cfg(feature = "bluetooth")]
use servo_bluetooth_traits::BluetoothRequest;
use servo_constellation_traits::ScriptToConstellationMessage;
use stylo_atoms::Atom;
use timers::{
    DocumentProducerFence, DocumentProducerFenceError, DocumentProducerGuard, DocumentProducerKind,
    TimerScheduler,
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
use crate::producer_fence::{
    DocumentProducerEnvelope, ProducerFencedTaskBox,
    fence_fetch_until_eof as fence_resource_fetch_until_eof,
};
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

#[derive(Debug)]
pub(crate) enum ControlledMessage {
    Control(ScriptThreadControlMessage),
    Ordinary(MixedMessage),
}

/// Result of one bounded wait on the priority document-control lane.
pub(crate) enum DocumentControlWaitResult {
    Message(ScriptThreadControlMessage),
    TimedOut,
    Closed,
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
        response: Box<DocumentProducerEnvelope<FetchResponseMsg>>,
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
    /// Fence one Fetch response stream when this sender targets a controlled Window event loop.
    ///
    /// Realtime main-thread and worker senders return the original callback without producer
    /// accounting. A controlled producer-admission failure is returned to the caller so it can
    /// fail closed before starting network work rather than silently falling back to an unfenced
    /// callback.
    pub(crate) fn fence_fetch_until_eof(
        &self,
        callback: BoxedFetchCallback,
    ) -> Result<BoxedFetchCallback, ScriptEventLoopSendError> {
        let Self::MainThread {
            producer_fence: Some(producer_fence),
            ..
        } = self
        else {
            return Ok(callback);
        };

        fence_resource_fetch_until_eof(producer_fence, callback)
            .map_err(ScriptEventLoopSendError::Producer)
    }

    /// Begin an externally owned callback that will eventually hand work back to this event loop.
    ///
    /// Only a controlled main-thread event loop installs producer tracking. Realtime and worker
    /// senders preserve their ordinary transport path and therefore return no guard. Admission is
    /// checked so an exhausted producer fence remains a typed terminal condition.
    pub(crate) fn begin_external_callback(
        &self,
    ) -> Result<Option<DocumentProducerGuard>, DocumentProducerFenceError> {
        let Self::MainThread {
            producer_fence: Some(producer_fence),
            ..
        } = self
        else {
            return Ok(None);
        };

        producer_fence
            .begin(DocumentProducerKind::ExternalCallback)
            .map(Some)
    }

    /// Queue one navigation response while retaining its Task producer through ScriptThread
    /// handling.
    ///
    /// Navigation responses use a dedicated main-thread message rather than `CommonScriptMsg::Task`,
    /// so they need the same checked admission and post-commit wake explicitly. Realtime preserves
    /// Servo's direct message path and carries no producer guard.
    pub(crate) fn send_navigation_response(
        &self,
        pipeline_id: PipelineId,
        message: FetchResponseMsg,
    ) -> Result<(), ScriptEventLoopSendError> {
        let Self::MainThread {
            sender,
            producer_fence,
        } = self
        else {
            unreachable!("document navigation responses only target the main ScriptThread")
        };

        let guard = producer_fence
            .as_ref()
            .map(|producer_fence| producer_fence.begin(DocumentProducerKind::Task))
            .transpose()
            .map_err(ScriptEventLoopSendError::Producer)?;
        let response = Box::new(DocumentProducerEnvelope::new(message, guard));

        sender
            .send(MainThreadScriptMsg::NavigationResponse {
                pipeline_id,
                response,
            })
            .map_err(|_| ScriptEventLoopSendError::ChannelClosed)?;

        if let Some(producer_fence) = producer_fence {
            // Admission wakes before the channel mutation. Wake again after commit so a controlled
            // owner cannot observe Busy, sleep, and strand this newly queued response.
            producer_fence.notify_observer_after_commit();
        }
        Ok(())
    }

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
        match self {
            MainThreadScriptMsg::Common(CommonScriptMsg::Task(
                _category,
                _boxed,
                pipeline_id,
                _task_source,
            )) => *pipeline_id,
            MainThreadScriptMsg::Common(CommonScriptMsg::ReportCspViolations(pipeline_id, _)) => {
                Some(*pipeline_id)
            },
            MainThreadScriptMsg::NavigationResponse { pipeline_id, .. } => Some(*pipeline_id),
            MainThreadScriptMsg::WorkletLoaded(pipeline_id) => Some(*pipeline_id),
            MainThreadScriptMsg::RegisterPaintWorklet { pipeline_id, .. } => Some(*pipeline_id),
            MainThreadScriptMsg::ForwardEmbedderControlResponseFromFileManager(control_id, ..) => {
                Some(control_id.pipeline_id)
            },
            MainThreadScriptMsg::Common(CommonScriptMsg::CollectReports(_)) |
            MainThreadScriptMsg::Inactive |
            MainThreadScriptMsg::WakeUp => None,
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use embedder_traits::{EventLoopWaker, ScriptToEmbedderChan};
    use net_traits::request::RequestId;
    use net_traits::{ResourceFetchTiming, ResourceTimingType};
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

    fn response_eof() -> FetchResponseMsg {
        FetchResponseMsg::ProcessResponseEOF(
            RequestId::default(),
            Ok(()),
            ResourceFetchTiming::new(ResourceTimingType::Resource),
        )
    }

    #[test]
    fn controlled_navigation_eof_hands_resource_off_to_a_guarded_response_task() {
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender, &producer_fence);
        let callback_sender = event_loop_sender.clone();
        let mut callback = event_loop_sender
            .fence_fetch_until_eof(Box::new(move |message| {
                callback_sender
                    .send_navigation_response(TEST_PIPELINE_ID, message)
                    .unwrap();
            }))
            .unwrap();

        callback(response_eof());

        let after_eof = producer_fence.snapshot();
        assert_eq!(
            after_eof.for_kind(DocumentProducerKind::Resource).pending(),
            0
        );
        assert_eq!(after_eof.for_kind(DocumentProducerKind::Task).pending(), 1);

        let response = match receiver.recv().unwrap() {
            MainThreadScriptMsg::NavigationResponse {
                pipeline_id,
                response,
            } => {
                assert_eq!(pipeline_id, TEST_PIPELINE_ID);
                response
            },
            _ => panic!("expected a navigation response"),
        };
        let (_message, producer_guard) = response.into_parts();
        assert!(producer_guard.is_some());
        assert_eq!(producer_fence.snapshot().pending(), 1);
        drop(producer_guard);

        let complete = producer_fence.snapshot();
        assert!(complete.is_empty());
        assert_eq!(
            complete
                .for_kind(DocumentProducerKind::Resource)
                .completed(),
            1
        );
        assert_eq!(complete.for_kind(DocumentProducerKind::Task).completed(), 1);
    }

    #[test]
    fn navigation_response_wakes_before_and_after_direct_queue_commit() {
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
            .send_navigation_response(TEST_PIPELINE_ID, response_eof())
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
    fn failed_navigation_response_send_releases_its_task_ticket() {
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        drop(receiver);
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender, &producer_fence);

        assert_eq!(
            event_loop_sender.send_navigation_response(TEST_PIPELINE_ID, response_eof()),
            Err(ScriptEventLoopSendError::ChannelClosed)
        );

        let complete = producer_fence.snapshot();
        assert!(complete.is_empty());
        assert_eq!(complete.for_kind(DocumentProducerKind::Task).completed(), 1);
    }

    #[test]
    fn realtime_navigation_response_preserves_the_raw_direct_message_path() {
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = ScriptEventLoopSender::MainThread {
            sender: raw_sender,
            producer_fence: None,
        };

        event_loop_sender
            .send_navigation_response(TEST_PIPELINE_ID, response_eof())
            .unwrap();

        assert!(producer_fence.snapshot().is_empty());
        let response = match receiver.recv().unwrap() {
            MainThreadScriptMsg::NavigationResponse { response, .. } => response,
            _ => panic!("expected a navigation response"),
        };
        let (_message, producer_guard) = response.into_parts();
        assert!(producer_guard.is_none());
        assert!(producer_fence.snapshot().is_empty());
    }

    #[test]
    fn controlled_fetch_resource_overlaps_response_tasks_and_terminal_handoff() {
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender, &producer_fence);
        let callback_sender = event_loop_sender.clone();
        let callback_fence = producer_fence.clone();
        let observations = Arc::new(Mutex::new(Vec::new()));
        let callback_observations = observations.clone();
        let mut callback = event_loop_sender
            .fence_fetch_until_eof(Box::new(move |_| {
                callback_sender
                    .send(task_message(None, TaskSourceName::Networking))
                    .unwrap();
                let snapshot = callback_fence.snapshot();
                callback_observations.lock().unwrap().push((
                    snapshot.for_kind(DocumentProducerKind::Resource).pending(),
                    snapshot.for_kind(DocumentProducerKind::Task).pending(),
                ));
            }))
            .unwrap();

        let admitted = producer_fence.snapshot();
        assert_eq!(
            admitted.for_kind(DocumentProducerKind::Resource).pending(),
            1
        );
        assert_eq!(admitted.for_kind(DocumentProducerKind::Task).pending(), 0);

        callback(FetchResponseMsg::ProcessRequestBody(RequestId::default()));
        assert_eq!(*observations.lock().unwrap(), vec![(1, 1)]);
        let after_nonterminal = producer_fence.snapshot();
        assert_eq!(
            after_nonterminal
                .for_kind(DocumentProducerKind::Resource)
                .pending(),
            1
        );
        assert_eq!(
            after_nonterminal
                .for_kind(DocumentProducerKind::Task)
                .pending(),
            1
        );
        drop(receiver.recv().unwrap());
        assert_eq!(producer_fence.snapshot().pending(), 1);

        callback(response_eof());
        assert_eq!(*observations.lock().unwrap(), vec![(1, 1), (1, 1)]);
        let after_eof = producer_fence.snapshot();
        assert_eq!(
            after_eof.for_kind(DocumentProducerKind::Resource).pending(),
            0
        );
        assert_eq!(after_eof.for_kind(DocumentProducerKind::Task).pending(), 1);

        drop(receiver.recv().unwrap());
        let complete = producer_fence.snapshot();
        assert!(complete.is_empty());
        assert_eq!(
            complete
                .for_kind(DocumentProducerKind::Resource)
                .completed(),
            1
        );
        assert_eq!(complete.for_kind(DocumentProducerKind::Task).completed(), 2);
    }

    #[test]
    fn realtime_and_worker_fetch_callbacks_preserve_the_raw_path() {
        let (main_sender, _main_receiver) = crossbeam_channel::unbounded();
        let (worker_sender, _worker_receiver) = crossbeam_channel::unbounded();
        let senders = [
            ScriptEventLoopSender::MainThread {
                sender: main_sender,
                producer_fence: None,
            },
            ScriptEventLoopSender::SharedWorker(worker_sender),
        ];
        let invocations = Arc::new(AtomicUsize::new(0));

        for sender in senders {
            let callback_invocations = invocations.clone();
            let mut callback = sender
                .fence_fetch_until_eof(Box::new(move |_| {
                    callback_invocations.fetch_add(1, Ordering::SeqCst);
                }))
                .unwrap();
            callback(response_eof());
        }

        assert_eq!(invocations.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn failed_terminal_task_send_releases_both_task_and_resource_tickets() {
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        drop(receiver);
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender, &producer_fence);
        let callback_sender = event_loop_sender.clone();
        let callback_fence = producer_fence.clone();
        let observed_during_callback = Arc::new(Mutex::new(None));
        let callback_observation = observed_during_callback.clone();
        let mut callback = event_loop_sender
            .fence_fetch_until_eof(Box::new(move |_| {
                let result = callback_sender.send(task_message(None, TaskSourceName::Networking));
                let snapshot = callback_fence.snapshot();
                *callback_observation.lock().unwrap() = Some((
                    result,
                    snapshot.for_kind(DocumentProducerKind::Resource).pending(),
                    snapshot.for_kind(DocumentProducerKind::Task).pending(),
                    snapshot.for_kind(DocumentProducerKind::Task).completed(),
                ));
            }))
            .unwrap();

        callback(response_eof());

        assert_eq!(
            *observed_during_callback.lock().unwrap(),
            Some((Err(ScriptEventLoopSendError::ChannelClosed), 1, 0, 1))
        );
        let complete = producer_fence.snapshot();
        assert!(complete.is_empty());
        assert_eq!(
            complete
                .for_kind(DocumentProducerKind::Resource)
                .completed(),
            1
        );
        assert_eq!(complete.for_kind(DocumentProducerKind::Task).completed(), 1);
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
    fn external_callback_is_visible_before_it_hands_off_a_task() {
        let (raw_sender, _receiver) = crossbeam_channel::unbounded();
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender, &producer_fence);

        let callback = event_loop_sender
            .begin_external_callback()
            .unwrap()
            .unwrap();
        let during_callback = producer_fence.snapshot();
        assert_eq!(
            during_callback
                .for_kind(DocumentProducerKind::ExternalCallback)
                .pending(),
            1
        );
        assert_eq!(
            during_callback
                .for_kind(DocumentProducerKind::Task)
                .pending(),
            0
        );

        drop(callback);
        let complete = producer_fence.snapshot();
        assert!(complete.is_empty());
        assert_eq!(
            complete
                .for_kind(DocumentProducerKind::ExternalCallback)
                .completed(),
            1
        );
    }

    #[test]
    fn external_callback_handoff_overlaps_then_leaves_only_the_queued_task() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender.clone(), &producer_fence);
        let task_queue = TaskQueue::new_with_producer_tracking(receiver, raw_sender, true);

        let callback = event_loop_sender
            .begin_external_callback()
            .unwrap()
            .unwrap();
        event_loop_sender
            .send(task_message(None, TaskSourceName::Networking))
            .unwrap();

        let during_handoff = producer_fence.snapshot();
        assert_eq!(during_handoff.pending(), 2);
        assert_eq!(
            during_handoff
                .for_kind(DocumentProducerKind::ExternalCallback)
                .pending(),
            1
        );
        assert_eq!(
            during_handoff
                .for_kind(DocumentProducerKind::Task)
                .pending(),
            1
        );

        drop(callback);
        let after_handoff = producer_fence.snapshot();
        assert_eq!(after_handoff.pending(), 1);
        assert_eq!(
            after_handoff
                .for_kind(DocumentProducerKind::ExternalCallback)
                .pending(),
            0
        );
        assert_eq!(
            after_handoff.for_kind(DocumentProducerKind::Task).pending(),
            1
        );

        let queued_task = task_queue
            .take_tasks_and_recv(&FxHashSet::default())
            .unwrap();
        assert_eq!(
            producer_fence
                .snapshot()
                .for_kind(DocumentProducerKind::Task)
                .pending(),
            1
        );
        drop(queued_task);
        assert!(producer_fence.snapshot().is_empty());
    }

    #[test]
    fn failed_external_callback_handoff_releases_the_attempted_task_ticket() {
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        drop(receiver);
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender, &producer_fence);

        let callback = event_loop_sender
            .begin_external_callback()
            .unwrap()
            .unwrap();
        assert_eq!(
            event_loop_sender.send(task_message(None, TaskSourceName::Networking)),
            Err(ScriptEventLoopSendError::ChannelClosed)
        );

        let after_failed_send = producer_fence.snapshot();
        assert_eq!(after_failed_send.pending(), 1);
        assert_eq!(
            after_failed_send
                .for_kind(DocumentProducerKind::ExternalCallback)
                .pending(),
            1
        );
        assert_eq!(
            after_failed_send
                .for_kind(DocumentProducerKind::Task)
                .pending(),
            0
        );
        assert_eq!(
            after_failed_send
                .for_kind(DocumentProducerKind::Task)
                .completed(),
            1
        );

        drop(callback);
        let complete = producer_fence.snapshot();
        assert!(complete.is_empty());
        assert_eq!(
            complete
                .for_kind(DocumentProducerKind::ExternalCallback)
                .completed(),
            1
        );
    }

    #[test]
    fn external_callback_ticket_is_released_when_callback_panics() {
        let (raw_sender, _receiver) = crossbeam_channel::unbounded();
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender, &producer_fence);

        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let callback = event_loop_sender
                .begin_external_callback()
                .unwrap()
                .unwrap();
            let parse_work = move || {
                let _keep_callback_live = &callback;
                panic!("synthetic stylesheet parse failure");
            };
            parse_work();
        }));

        assert!(unwind.is_err());
        assert!(producer_fence.snapshot().is_empty());
    }

    #[test]
    fn discarded_external_callback_closure_releases_its_ticket() {
        let (raw_sender, _receiver) = crossbeam_channel::unbounded();
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = main_thread_sender(raw_sender, &producer_fence);

        let callback = event_loop_sender
            .begin_external_callback()
            .unwrap()
            .unwrap();
        let parse_work = move || drop(callback);
        assert_eq!(producer_fence.snapshot().pending(), 1);

        drop(parse_work);
        assert!(producer_fence.snapshot().is_empty());
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
        let task_queue = TaskQueue::new_with_producer_tracking(receiver, raw_sender.clone(), true);

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
    fn discarding_a_pipeline_purges_direct_navigation_messages() {
        let _script_thread_state = ScriptThreadStateGuard::enter();
        let (raw_sender, receiver) = crossbeam_channel::unbounded();
        let task_queue = TaskQueue::new_with_producer_tracking(receiver, raw_sender.clone(), true);
        let producer_fence = DocumentProducerFence::default();
        let fully_active = FxHashSet::default();

        raw_sender
            .send(MainThreadScriptMsg::NavigationResponse {
                pipeline_id: TEST_PIPELINE_ID,
                response: Box::new(DocumentProducerEnvelope::new(
                    response_eof(),
                    Some(producer_fence.begin(DocumentProducerKind::Task).unwrap()),
                )),
            })
            .unwrap();
        task_queue.take_tasks(MainThreadScriptMsg::WakeUp, &fully_active);
        assert_eq!(task_queue.observation().ready, 1);
        assert_eq!(producer_fence.snapshot().pending(), 1);

        task_queue.discard_pipeline(TEST_PIPELINE_ID);
        assert_eq!(task_queue.observation().ready, 0);
        assert!(task_queue.recv().is_err());
        assert!(producer_fence.snapshot().is_empty());

        raw_sender
            .send(MainThreadScriptMsg::NavigationResponse {
                pipeline_id: TEST_PIPELINE_ID,
                response: Box::new(DocumentProducerEnvelope::new(
                    response_eof(),
                    Some(producer_fence.begin(DocumentProducerKind::Task).unwrap()),
                )),
            })
            .unwrap();
        task_queue.take_tasks(MainThreadScriptMsg::WakeUp, &fully_active);
        assert_eq!(task_queue.observation().ready, 0);
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
        let producer_fence = DocumentProducerFence::default();
        let event_loop_sender = ScriptEventLoopSender::MainThread {
            sender: raw_sender.clone(),
            producer_fence: None,
        };
        let task_queue = TaskQueue::new(receiver, raw_sender);
        let mut fully_active = FxHashSet::default();
        fully_active.insert(TEST_PIPELINE_ID);

        task_queue.discard_pipeline(TEST_PIPELINE_ID);
        assert!(
            event_loop_sender
                .begin_external_callback()
                .unwrap()
                .is_none()
        );
        event_loop_sender
            .send(task_message(Some(TEST_PIPELINE_ID), TaskSourceName::Timer))
            .unwrap();

        assert!(producer_fence.snapshot().is_empty());

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
    /// Priority document-control lane. Commands on this receiver are never page events.
    #[no_trace]
    pub(crate) document_control_receiver: RoutedReceiver<ScriptThreadControlMessage>,

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

    /// Block for one controlled input without draining a ready task-port suffix.
    pub(crate) fn recv_controlled(
        &self,
        task_queue: &TaskQueue<MainThreadScriptMsg>,
        timer_scheduler: &TimerScheduler,
        fully_active: &FxHashSet<PipelineId>,
    ) -> ControlledMessage {
        // A blocking wait begins a fresh event-loop intake iteration. Reset before the
        // nonblocking precheck so newly eligible retained throttles cannot be stranded while the
        // raw receiver sleeps.
        let task_recv = task_queue.select();
        if let Some(message) = self.try_recv_document_control() {
            return ControlledMessage::Control(message);
        }
        if let Some(message) = self.try_recv_controlled(task_queue, fully_active) {
            return ControlledMessage::Ordinary(message);
        }

        let mut select = Select::new();

        let document_control_index = select.recv(&self.document_control_receiver);
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
            if index == document_control_index {
                ControlledMessage::Control(
                    operation
                        .recv(&self.document_control_receiver)
                        .unwrap()
                        .unwrap(),
                )
            } else if index == task_index {
                let msg = operation.recv(task_recv).unwrap();
                ControlledMessage::Ordinary(MixedMessage::FromScript(
                    task_queue.take_controlled_task_and_recv(msg, fully_active),
                ))
            } else if index == constellation_index {
                ControlledMessage::Ordinary(MixedMessage::FromConstellation(
                    operation
                        .recv(&self.constellation_receiver)
                        .unwrap()
                        .unwrap(),
                ))
            } else if index == devtools_index {
                ControlledMessage::Ordinary(MixedMessage::FromDevtools(
                    operation
                        .recv(&self.devtools_server_receiver)
                        .unwrap()
                        .unwrap(),
                ))
            } else if index == image_cache_index {
                ControlledMessage::Ordinary(MixedMessage::FromImageCache(
                    operation.recv(&self.image_cache_receiver).unwrap(),
                ))
            } else {
                #[cfg(feature = "webgpu")]
                {
                    debug_assert_eq!(index, webgpu_index);
                    ControlledMessage::Ordinary(MixedMessage::FromWebGPUServer(
                        operation.recv(&*webgpu_receiver).unwrap().unwrap(),
                    ))
                }
                #[cfg(not(feature = "webgpu"))]
                unreachable!("select returned an unknown index {index}")
            }
        };

        if let Some(deadline) = timer_scheduler.next_deadline() {
            select
                .select_deadline(deadline)
                .map(message_from_operation)
                .unwrap_or(ControlledMessage::Ordinary(MixedMessage::TimerFired))
        } else {
            message_from_operation(select.select())
        }
    }

    /// Wait for at most `timeout` on the priority document-control lane.
    ///
    /// This is used while an already-executed initial navigation-header turn waits for the
    /// Constellation to return the exact pending-to-active target authority. The bounded wait lets
    /// ScriptThread observe forced shutdown and priority lifecycle input without running ordinary
    /// page work during that handoff.
    pub(crate) fn recv_document_control_timeout(
        &self,
        timeout: Duration,
    ) -> DocumentControlWaitResult {
        match self.document_control_receiver.recv_timeout(timeout) {
            Ok(Ok(message)) => DocumentControlWaitResult::Message(message),
            Ok(Err(error)) => {
                log::warn!(
                    "ScriptThreadReceivers IPC error on document_control_receiver: {:?}",
                    error
                );
                DocumentControlWaitResult::Closed
            },
            Err(RecvTimeoutError::Timeout) => DocumentControlWaitResult::TimedOut,
            Err(RecvTimeoutError::Disconnected) => {
                log::warn!("ScriptThreadReceivers disconnected document_control_receiver");
                DocumentControlWaitResult::Closed
            },
        }
    }

    /// Receive one priority command or cancellation without touching ordinary page input.
    pub(crate) fn try_recv_document_control(&self) -> Option<ScriptThreadControlMessage> {
        let message = self.document_control_receiver.try_recv().ok()?;
        message
            .inspect_err(|error| {
                log::warn!(
                    "ScriptThreadReceivers IPC error on document_control_receiver: {:?}",
                    error
                );
            })
            .ok()
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

    /// Receive at most one controlled input from the selected source invocation.
    pub(crate) fn try_recv_controlled(
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
            return Some(MixedMessage::FromConstellation(message));
        }
        if let Ok(message) = task_queue.take_one_task_and_recv(fully_active) {
            return Some(MixedMessage::FromScript(message));
        }
        if let Ok(message) = self.devtools_server_receiver.try_recv() {
            return Some(MixedMessage::FromDevtools(message.unwrap()));
        }
        if let Ok(message) = self.image_cache_receiver.try_recv() {
            return Some(MixedMessage::FromImageCache(message));
        }
        #[cfg(feature = "webgpu")]
        if let Ok(message) = self.webgpu_receiver.borrow().try_recv() {
            return Some(MixedMessage::FromWebGPUServer(message.unwrap()));
        }
        None
    }
}
