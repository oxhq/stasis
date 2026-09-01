/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The script thread is the thread that owns the DOM in memory, runs JavaScript, and triggers
//! layout. It's in charge of processing events for all same-origin pages in a frame
//! tree, and manages the entire lifetime of pages in the frame tree from initial request to
//! teardown.
//!
//! Page loads follow a two-step process. When a request for a new page load is received, the
//! network request is initiated and the relevant data pertaining to the new page is stashed.
//! While the non-blocking request is ongoing, the script thread is free to process further events,
//! noting when they pertain to ongoing loads (such as resizes/viewport adjustments). When the
//! initial response is received for an ongoing load, the second phase starts - the frame tree
//! entry is created, along with the Window and Document objects, and the appropriate parser
//! takes over the response body. Once parsing is complete, the document lifecycle for loading
//! a page runs its course and the script thread returns to processing events in the main event
//! loop.

use std::cell::{Cell, RefCell};
use std::collections::{HashSet, VecDeque};
use std::default::Default;
use std::option::Option;
use std::rc::{Rc, Weak};
use std::result::Result;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use background_hang_monitor_api::{
    BackgroundHangMonitor, BackgroundHangMonitorExitSignal, BackgroundHangMonitorRegister,
    HangAnnotation, MonitoredComponentId, MonitoredComponentType,
};
use chrono::{DateTime, Local};
use crossbeam_channel::unbounded;
use data_url::mime::Mime;
use devtools_traits::{
    CSSError, DevtoolScriptControlMsg, DevtoolsPageInfo, NavigationState,
    ScriptToDevtoolsControlMsg, WorkerId,
};
use embedder_traits::document_automation::{
    DocumentAutomationError, DocumentAutomationOperation, DocumentAutomationOperationKind,
    DocumentAutomationRequest,
};
use embedder_traits::document_control::{
    DocumentAdvanceToken, DocumentAdvanceTokenId, DocumentAdvanceTokenInvariantError,
    DocumentControlAction, DocumentControlAutomationKind, DocumentControlCancellationId,
    DocumentControlCommand, DocumentControlError, DocumentControlObservation,
    DocumentControlObservationInvariantError, DocumentControlOutcome, DocumentControlRequestId,
    DocumentPendingFact, is_exact_initial_pipeline_activation_transition,
    is_exact_replacement_pipeline_activation_transition,
};
use embedder_traits::document_pending::{
    PendingAnimatedImageObservation, PendingAnimatedImageUnsupportedCounts,
    PendingCanvasObservation, PendingCanvasUnsupportedCounts, PendingExternalIoEvidence,
    PendingExternalIoLoadBlocking, PendingExternalIoOwner, PendingGenerationTerminal,
    PendingGenerationTerminalObservation, PendingImageTimerTerminalObservation,
    PendingInputRevision, PendingLogicalTimerKind, PendingLogicalTimerStableId,
    PendingLogicalTimerTerminalObservation, PendingMicrotaskCheckpoint, PendingParserPhase,
    PendingParserSourceKind, PendingPipelineRenderingObservation, PendingProducerObservation,
    PendingProducerStability, PendingRenderingObservation, PendingRenderingPipelineActivity,
    PendingRuntimeTerminals, PendingSourceDisposition, PendingTargetObservation,
    PendingUnsupportedSourceReason, RawPendingSnapshot,
};
use embedder_traits::user_contents::{UserContentManagerId, UserContents, UserScript};
use embedder_traits::{
    DocumentControlProfile, DocumentExecutionProfile, EmbedderControlId, EmbedderControlResponse,
    EmbedderMsg, FocusSequenceNumber, InputEventOutcome, JavaScriptEvaluationError,
    JavaScriptEvaluationId, MediaSessionActionType, ScriptToEmbedderChan, Theme, ViewportDetails,
    WebDriverScriptCommand,
};
use encoding_rs::Encoding;
use fonts::{FontContext, SystemFontServiceProxy, WebFontLoadEvent};
use headers::{HeaderMapExt, LastModified, ReferrerPolicy as ReferrerPolicyHeader};
use http::header::REFRESH;
use hyper_serde::Serde;
use ipc_channel::router::ROUTER;
use js::context::{JSContext, NoGC};
use js::glue::GetWindowProxyClass;
use js::jsapi::{GCReason, JSContext as UnsafeJSContext};
use js::jsval::UndefinedValue;
use js::rust::ParentRuntime;
use js::rust::wrappers2::{JS_AddInterruptCallback, JS_GC, SetWindowProxyClass};
use layout_api::{LayoutConfig, LayoutFactory, RestyleReason, ScriptThreadFactory};
use media::WindowGLContext;
use metrics::MAX_TASK_NS;
use net_traits::image_cache::{ImageCacheFactory, ImageCacheResponseMessage};
use net_traits::pub_domains::is_same_site;
use net_traits::request::{Referrer, RequestId};
use net_traits::response::ResponseInit;
use net_traits::{
    FetchMetadata, FetchResponseMsg, Metadata, NetworkError, ResourceFetchTiming, ResourceThreads,
    ResourceTimingType,
};
use paint_api::{
    CrossProcessPaintApi, PinchZoomInfos, PipelineExitMarkerStatus, PipelineExitSource,
};
use percent_encoding::percent_decode;
use profile_traits::mem::{ProcessReports, ReportsChan, perform_memory_report};
use profile_traits::time::ProfilerCategory;
use profile_traits::time_profile;
use rustc_hash::{FxHashMap, FxHashSet};
use script_bindings::cell::DomRefCell;
use script_traits::{
    ConstellationInputEvent, DiscardBrowsingContext, DocumentActivity, InitialScriptState,
    NewPipelineInfo, Painter, ProgressiveWebMetricType, ScriptThreadControlMessage,
    ScriptThreadMessage, UpdatePipelineIdReason,
};
use servo_arc::Arc as ServoArc;
use servo_base::cross_process_instant::CrossProcessInstant;
use servo_base::generic_channel::GenericSender;
use servo_base::id::{
    BrowsingContextId, HistoryStateId, PipelineId, PipelineNamespace, ScriptEventLoopId, WebViewId,
};
use servo_base::threadboost::{BoostAffinity, ThreadPriority};
use servo_base::{Epoch, generic_channel};
#[cfg(feature = "webgl")]
use servo_canvas_traits::webgl::WebGLPipeline;
use servo_config::opts::{self, DiagnosticsLoggingOption};
use servo_config::{pref, prefs};
use servo_constellation_traits::{
    HistoryTraversalSource, InitialPipelineActivationCorrelation, LoadData, LoadOrigin,
    NavigationHistoryBehavior, RemoteFocusOperation, ScreenshotReadinessResponse,
    ScriptToConstellationChan, ScriptToConstellationMessage, ScrollStateUpdate,
    SessionHistoryTraversalRequest, StructuredSerializedData, TargetSnapshotParams,
    TraversalDirection, WindowSizeType,
};
use servo_url::{ImmutableOrigin, MutableOrigin, OriginSnapshot, ServoUrl};
use storage_traits::StorageThreads;
use storage_traits::webstorage_thread::WebStorageType;
use style::context::QuirksMode;
use style::error_reporting::RustLogReporter;
use style::media_queries::MediaList;
use style::shared_lock::SharedRwLock;
use style::stylesheets::{AllowImportRules, DocumentStyleSheet, Origin, Stylesheet};
use style::thread_state::{self, ThreadState};
use stylo_atoms::Atom;
use timers::{
    DetachedTimerEvent, DocumentClock, DocumentClockError, DocumentExecutionLedger,
    DocumentExecutionLimits, DocumentProducerCheckpoint, DocumentProducerFence,
    DocumentProducerGuard, DocumentProducerKind, DocumentProducerObserver, DocumentTime,
    DocumentTimeSurface, TimerControlError, TimerDeadlineSnapshot, TimerEventRequest, TimerId,
    TimerScheduler,
};
use url::Position;
#[cfg(feature = "webgpu")]
use webgpu_traits::{WebGPUDevice, WebGPUMsg};

use crate::automation::execute_prevalidated_document_automation;
use crate::devtools::DevtoolsState;
use crate::dom::bindings::codegen::Bindings::DocumentBinding::{
    DocumentMethods, DocumentReadyState,
};
use crate::dom::bindings::codegen::Bindings::NavigatorBinding::NavigatorMethods;
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::conversions::{
    ConversionResult, FromJSValConvertible, StringificationBehavior,
};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::csp::{CspReporting, GlobalCspReporting, Violation};
use crate::dom::customelementregistry::{
    CallbackReaction, CustomElementDefinition, CustomElementReactionStack,
};
use crate::dom::document::focus::FocusableArea;
use crate::dom::document::{
    Document, DocumentSource, HasBrowsingContext, IsHTMLDocument, RenderingUpdateReason,
};
use crate::dom::element::Element;
use crate::dom::globalscope::GlobalScope;
use crate::dom::html::htmliframeelement::{HTMLIFrameElement, IframeContext, ProcessingMode};
use crate::dom::intersectionobserver::IntersectionObserverRenderingTime;
use crate::dom::node::{Node, NodeTraits};
use crate::dom::servoparser::{ParserContext, ServoParser};
use crate::dom::types::DebuggerGlobalScope;
#[cfg(feature = "webgpu")]
use crate::dom::webgpu::identityhub::IdentityHub;
use crate::dom::window::{ImageCallbackDelivery, Window};
use crate::dom::windowproxy::{CreatorBrowsingContextInfo, WindowProxy};
use crate::event_loop::document_collection::DocumentCollection;
use crate::event_loop::document_loader::DocumentLoader;
use crate::event_loop::pending_snapshot::{
    PendingInputBarrierFacts, capture_barrier_observation, map_pending_normalize_error,
};
use crate::event_loop::pending_state::{
    PendingClockFacts, PendingInputFacts, PendingLogicalTimerFacts, PendingLogicalTimerIdentity,
    PendingMicrotaskFacts, PendingNormalizeError, PendingParserFacts, PendingParserOwnerId,
    PendingPersistentSourceIdentity, PendingProducerQualificationLedger, PendingSchedulerFacts,
    PendingStateError, PendingStateLedger, PendingTaskFacts, RawPendingBuildFacts,
};
use crate::event_loop::script_mutation_observers::ScriptMutationObservers;
use crate::event_loop::script_window_proxies::ScriptWindowProxies;
use crate::event_loop::svg_font::SvgFontResolver;
use crate::fetch::fetch::FetchCanceller;
use crate::fetch::network_listener::{FetchResponseListener, submit_timing};
use crate::messaging::{
    CommonScriptMsg, ControlledMessage, DocumentControlWaitResult, ImageCacheMessage,
    MainThreadScriptMsg, MixedMessage, ScriptEventLoopSender, ScriptThreadReceivers,
    ScriptThreadSenders,
};
use crate::microtask::{MicrotaskQueue, MicrotaskRunnable};
use crate::mime::{APPLICATION, CHARSET, MimeExt, TEXT, XML};
use crate::navigation::{InProgressLoad, NavigationListener};
use crate::realms::enter_auto_realm;
use crate::script_runtime::{
    IntroductionType, Runtime, ScriptThreadEventCategory, ThreadSafeJSContext, get_reports,
};
use crate::tasks::task_queue::TaskQueue;
use crate::timers::{DomTimerKind, DomTimerOuterWakeObservation, DomTimerPendingObservation};
use crate::webdriver_handlers::jsval_to_webdriver;
use crate::{devtools, webdriver_handlers};

thread_local!(static SCRIPT_THREAD_ROOT: Cell<Option<*const ScriptThread>> = const { Cell::new(None) });

fn with_optional_script_thread<R>(f: impl FnOnce(Option<&ScriptThread>) -> R) -> R {
    SCRIPT_THREAD_ROOT.with(|root| {
        f(root
            .get()
            .and_then(|script_thread| unsafe { script_thread.as_ref() }))
    })
}

pub(crate) fn with_script_thread<R: Default>(f: impl FnOnce(&ScriptThread) -> R) -> R {
    with_optional_script_thread(|script_thread| script_thread.map(f).unwrap_or_default())
}

// We borrow the incomplete parser contexts mutably during parsing,
// which is fine except that parsing can trigger evaluation,
// which can trigger GC, and so we can end up tracing the script
// thread during parsing. For this reason, we don't trace the
// incomplete parser contexts during GC.
pub(crate) struct IncompleteParserContexts(RefCell<Vec<(PipelineId, ParserContext)>>);

unsafe_no_jsmanaged_fields!(TaskQueue<MainThreadScriptMsg>);

type NodeIdSet = HashSet<String>;

const CONTROLLED_INPUT_BATCH_LIMIT: usize = 64;
const CONTROLLED_AUTHORITY_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ControlledInputBatch {
    admitted: usize,
    saturated: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct ControlledControlBatch {
    admitted: usize,
    saturated: bool,
    active_cancelled: bool,
}

/// ScriptThread-owned authority which must survive across controlled commands.
struct DocumentControlState {
    pending: PendingStateLedger,
    producer_observer: DocumentProducerObserver,
    producer_qualification: PendingProducerQualificationLedger,
    producer_checkpoint: DocumentProducerCheckpoint,
    token_sequence: u64,
    issued_token: Option<DocumentAdvanceToken>,
    initial_pipeline_activation: Option<InitialPipelineActivationMarker>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InitialPipelineActivationMarker {
    pipeline_id: PipelineId,
    correlation: InitialPipelineActivationCorrelation,
}

enum InitialPipelineActivationAuthority {
    Authorized {
        target: Box<PendingTargetObservation>,
        target_terminals: PendingRuntimeTerminals,
    },
    Cancelled,
    Failed,
}

enum ReplacementPipelineBootstrapWaitOutcome {
    Ready { event_index: usize },
    Cancelled,
    Rejected(DocumentControlError),
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InitialPipelineActivationWaitInterruption {
    Closing,
    TerminalLifecycle,
    UnrelatedPipelineExit,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplacementPipelineBootstrapQueuedEvent {
    Ordinary,
    Lifecycle,
    ImmediateBarrier,
    Spawn(PipelineId),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplacementPipelineBootstrapQueueState {
    AwaitingInput,
    Ready { event_index: usize },
    Interrupted,
    InputRevisionOverflow,
    Unavailable,
}

enum ControlledDriveEventDisposition<'a> {
    PipelineBootstrapRequired,
    Ready(Option<&'a MixedMessage>),
}

#[derive(Clone, Copy)]
enum ProducerCapture {
    Passive,
    FreshCheckpoint,
    Exact(PendingProducerObservation),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ControlledLogicalTimerOwnerObservation {
    Active {
        pipeline_id: PipelineId,
        observation: DomTimerPendingObservation,
    },
    Terminal(PendingLogicalTimerTerminalObservation),
}

impl DocumentControlState {
    fn new(event_loop_id: ScriptEventLoopId) -> Self {
        Self {
            pending: PendingStateLedger::new(event_loop_id),
            producer_observer: DocumentProducerObserver::default(),
            producer_qualification: PendingProducerQualificationLedger::new(event_loop_id),
            producer_checkpoint: DocumentProducerCheckpoint::ZERO,
            token_sequence: 0,
            issued_token: None,
            initial_pipeline_activation: None,
        }
    }

    fn ensure_webview(&mut self, webview_id: WebViewId) -> Result<(), PendingStateError> {
        match self.pending.register_webview(webview_id) {
            Ok(()) | Err(PendingStateError::DuplicateWebView(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }
}

/// ScriptThread-owned ordinary input admitted ahead of controlled page turns.
///
/// Transport receivers are not authoritative queue state: this owner queue and its checked
/// revision establish the barrier that later pending snapshots and advance tokens can bind.
#[derive(Default)]
struct ControlledInputState {
    controls: VecDeque<ScriptThreadControlMessage>,
    retired_controls: HashSet<(DocumentControlRequestId, DocumentControlCancellationId)>,
    ready: VecDeque<MixedMessage>,
    revision: PendingInputRevision,
    intake_saturated: bool,
    revision_overflowed: bool,
}

impl ControlledInputState {
    fn admit_control(&mut self, message: ScriptThreadControlMessage) {
        match message {
            ScriptThreadControlMessage::Cancel {
                request_id,
                cancellation_id,
            } => {
                self.remove_control((request_id, cancellation_id));
            },
            command @ ScriptThreadControlMessage::Command {
                request_id,
                cancellation_id,
                ..
            } => {
                if !self
                    .retired_controls
                    .contains(&(request_id, cancellation_id))
                {
                    self.controls.push_back(command);
                }
            },
        }
    }

    fn retire_control(
        &mut self,
        active: (DocumentControlRequestId, DocumentControlCancellationId),
    ) {
        self.retired_controls.insert(active);
        self.remove_control(active);
    }

    fn remove_control(
        &mut self,
        active: (DocumentControlRequestId, DocumentControlCancellationId),
    ) {
        let (request_id, cancellation_id) = active;
        self.controls.retain(|message| {
            !matches!(
                message,
                ScriptThreadControlMessage::Command {
                    request_id: queued_request_id,
                    cancellation_id: queued_cancellation_id,
                    ..
                } if *queued_request_id == request_id &&
                    *queued_cancellation_id == cancellation_id
            )
        });
    }

    fn take_control(&mut self) -> Option<ScriptThreadControlMessage> {
        self.controls.pop_front()
    }

    fn drain_controls_bounded(
        &mut self,
        input: &mut impl Iterator<Item = ScriptThreadControlMessage>,
        active: Option<(DocumentControlRequestId, DocumentControlCancellationId)>,
    ) -> ControlledControlBatch {
        let mut admitted = 0;
        let mut active_cancelled = false;
        while admitted < CONTROLLED_INPUT_BATCH_LIMIT {
            let Some(message) = input.next() else {
                return ControlledControlBatch {
                    admitted,
                    saturated: false,
                    active_cancelled,
                };
            };
            admitted += 1;
            match message {
                ScriptThreadControlMessage::Cancel {
                    request_id,
                    cancellation_id,
                } if active == Some((request_id, cancellation_id)) => {
                    active_cancelled = true;
                    self.retire_control((request_id, cancellation_id));
                },
                message => self.admit_control(message),
            }
        }
        ControlledControlBatch {
            admitted,
            saturated: true,
            active_cancelled,
        }
    }

    fn admit(&mut self, event: MixedMessage) {
        // Commit the event before changing its revision. If the checked sequence is exhausted,
        // the event remains owned and the sticky terminal prevents a false-empty observation.
        self.ready.push_back(event);
        if self.revision_overflowed {
            return;
        }
        let Some(revision) = self.revision.checked_next() else {
            self.revision_overflowed = true;
            return;
        };
        self.revision = revision;
    }

    fn drain_bounded(
        &mut self,
        input: &mut impl Iterator<Item = MixedMessage>,
    ) -> ControlledInputBatch {
        let mut admitted = 0;
        while admitted < CONTROLLED_INPUT_BATCH_LIMIT {
            let Some(event) = input.next() else {
                self.intake_saturated = false;
                return ControlledInputBatch {
                    admitted,
                    saturated: false,
                };
            };
            self.admit(event);
            admitted += 1;
        }

        // Filling the cap is conservatively saturated. Peeking would remove an event that this
        // batch is not allowed to own.
        self.intake_saturated = true;
        ControlledInputBatch {
            admitted,
            saturated: true,
        }
    }

    #[cfg(test)]
    fn revision(&self) -> Result<PendingInputRevision, DocumentControlError> {
        if self.revision_overflowed {
            Err(DocumentControlError::InputRevisionOverflow)
        } else {
            Ok(self.revision)
        }
    }

    fn last_revision(&self) -> PendingInputRevision {
        self.revision
    }

    fn revision_overflowed(&self) -> bool {
        self.revision_overflowed
    }

    fn ready_len(&self) -> usize {
        self.ready.len()
    }

    fn intake_saturated(&self) -> bool {
        self.intake_saturated
    }

    #[cfg(test)]
    fn pop_front(&mut self) -> Option<MixedMessage> {
        self.ready.pop_front()
    }

    fn discard_pipeline(&mut self, pipeline_id: PipelineId) {
        self.ready
            .retain(|event| event.pipeline_id() != Some(pipeline_id));
    }
}

fn is_controlled_lifecycle_event(event: &MixedMessage) -> bool {
    matches!(
        event,
        MixedMessage::FromConstellation(
            ScriptThreadMessage::ExitPipeline(..) | ScriptThreadMessage::ExitScriptThread
        )
    )
}

fn replacement_pipeline_bootstrap_queued_event(
    event: &MixedMessage,
) -> ReplacementPipelineBootstrapQueuedEvent {
    match event {
        MixedMessage::FromConstellation(ScriptThreadMessage::SpawnPipeline(info)) => {
            ReplacementPipelineBootstrapQueuedEvent::Spawn(info.new_pipeline_id)
        },
        event if is_controlled_lifecycle_event(event) => {
            ReplacementPipelineBootstrapQueuedEvent::Lifecycle
        },
        MixedMessage::FromConstellation(ScriptThreadMessage::ExitFullScreen(_)) => {
            ReplacementPipelineBootstrapQueuedEvent::ImmediateBarrier
        },
        _ => ReplacementPipelineBootstrapQueuedEvent::Ordinary,
    }
}

/// Keep every `SpawnPipeline` behind its explicit initial/replacement bootstrap authority.
///
/// A source-bound `DriveOneTurn` can race a Constellation navigation observation: Script may
/// still exactly match the source target while the replacement's sole `SpawnPipeline` is already
/// queued. Treating that event as an ordinary turn would consume it, report the resulting target
/// drift as indeterminate, and leave the subsequent replacement bootstrap waiting for an event
/// which no longer exists.
fn controlled_drive_may_consume_classified_event(
    event: ReplacementPipelineBootstrapQueuedEvent,
) -> bool {
    !matches!(event, ReplacementPipelineBootstrapQueuedEvent::Spawn(_))
}

fn controlled_drive_event_disposition(
    pending_events: &VecDeque<MixedMessage>,
) -> ControlledDriveEventDisposition<'_> {
    let event = next_controlled_turn_event(pending_events);
    if event.is_some_and(|event| {
        !controlled_drive_may_consume_classified_event(replacement_pipeline_bootstrap_queued_event(
            event,
        ))
    }) {
        ControlledDriveEventDisposition::PipelineBootstrapRequired
    } else {
        ControlledDriveEventDisposition::Ready(event)
    }
}

/// Select the sole exact replacement SpawnPipeline only after owner intake is complete.
///
/// Servo's ordinary loop processes SpawnPipeline while retaining sequential input for later, so
/// an exact replacement may pass ordinary source-document backlog. Lifecycle input anywhere and
/// immediate input which arrived before SpawnPipeline remain barriers. A saturated intake cannot
/// prove that a second SpawnPipeline is not still waiting in the receiver suffix.
fn replacement_pipeline_bootstrap_classified_position(
    events: impl IntoIterator<Item = ReplacementPipelineBootstrapQueuedEvent>,
    intake_saturated: bool,
    pipeline_id: PipelineId,
) -> ReplacementPipelineBootstrapQueueState {
    let mut candidate = None;
    for (event_index, event) in events.into_iter().enumerate() {
        match event {
            ReplacementPipelineBootstrapQueuedEvent::Ordinary => {},
            ReplacementPipelineBootstrapQueuedEvent::Lifecycle => {
                return ReplacementPipelineBootstrapQueueState::Interrupted;
            },
            ReplacementPipelineBootstrapQueuedEvent::ImmediateBarrier if candidate.is_none() => {
                return ReplacementPipelineBootstrapQueueState::Unavailable;
            },
            ReplacementPipelineBootstrapQueuedEvent::ImmediateBarrier => {},
            ReplacementPipelineBootstrapQueuedEvent::Spawn(observed_pipeline_id) => {
                if observed_pipeline_id != pipeline_id || candidate.replace(event_index).is_some() {
                    return ReplacementPipelineBootstrapQueueState::Unavailable;
                }
            },
        }
    }

    if intake_saturated {
        return ReplacementPipelineBootstrapQueueState::AwaitingInput;
    }
    candidate.map_or(
        ReplacementPipelineBootstrapQueueState::AwaitingInput,
        |event_index| ReplacementPipelineBootstrapQueueState::Ready { event_index },
    )
}

fn initial_pipeline_activation_wait_interrupted(
    awaited_pipeline_id: PipelineId,
    closing: bool,
    pending_events: &VecDeque<MixedMessage>,
) -> Option<InitialPipelineActivationWaitInterruption> {
    if let Some(event) = pending_events
        .iter()
        .find(|event| is_controlled_lifecycle_event(event))
    {
        return Some(match event {
            MixedMessage::FromConstellation(ScriptThreadMessage::ExitPipeline(
                _,
                pipeline_id,
                _,
            )) if *pipeline_id != awaited_pipeline_id => {
                InitialPipelineActivationWaitInterruption::UnrelatedPipelineExit
            },
            _ => InitialPipelineActivationWaitInterruption::TerminalLifecycle,
        });
    }
    closing.then_some(InitialPipelineActivationWaitInterruption::Closing)
}

fn take_controlled_lifecycle_event(
    pending_events: &mut VecDeque<MixedMessage>,
) -> Option<MixedMessage> {
    let lifecycle_index = pending_events
        .iter()
        .position(is_controlled_lifecycle_event)?;
    pending_events.remove(lifecycle_index)
}

fn next_controlled_turn_event(pending_events: &VecDeque<MixedMessage>) -> Option<&MixedMessage> {
    pending_events
        .iter()
        .find(|event| is_controlled_lifecycle_event(event))
        .or_else(|| pending_events.front())
}

fn take_controlled_turn(pending_events: &mut VecDeque<MixedMessage>) -> (MixedMessage, bool) {
    match pending_events.pop_front() {
        Some(event) => (event, false),
        None => (MixedMessage::FromScript(MainThreadScriptMsg::WakeUp), true),
    }
}

fn controlled_event_consumes_ordinary_task_budget(event: &MixedMessage) -> bool {
    // These messages only pump the existing event-loop tail in controlled mode. TaskQueue emits
    // Inactive while retaining a task and WakeUp while promoting throttled work; neither executes
    // that retained task. TimerFired follows scheduler activation, which separately enqueues the
    // actual timer task. Host animation ticks are ignored by a controlled document clock.
    !matches!(
        event,
        MixedMessage::TimerFired |
            MixedMessage::FromScript(MainThreadScriptMsg::Inactive | MainThreadScriptMsg::WakeUp) |
            MixedMessage::FromConstellation(ScriptThreadMessage::TickAllAnimations(_))
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ControlledImageDeliveryTarget {
    Live,
    Retired,
    Unknown,
}

fn controlled_image_delivery_target(
    window_present: bool,
    pipeline_tombstoned: bool,
) -> ControlledImageDeliveryTarget {
    match (window_present, pipeline_tombstoned) {
        (true, false) => ControlledImageDeliveryTarget::Live,
        (false, true) => ControlledImageDeliveryTarget::Retired,
        (true, true) | (false, false) => ControlledImageDeliveryTarget::Unknown,
    }
}

struct ControlledImageMessageCompletion {
    guard: Option<DocumentProducerGuard>,
}

impl ControlledImageMessageCompletion {
    fn new(guard: Option<DocumentProducerGuard>) -> Self {
        Self { guard }
    }
}

impl Drop for ControlledImageMessageCompletion {
    fn drop(&mut self) {
        let Some(guard) = self.guard.take() else {
            return;
        };
        if std::thread::panicking() {
            let _ = guard.abandon();
        } else {
            drop(guard);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InitialPipelineBootstrapFacts {
    pipeline_id: PipelineId,
    webview_id: WebViewId,
    browsing_context_id: BrowsingContextId,
    parent_pipeline_id: Option<PipelineId>,
    local_document_count: usize,
    local_incomplete_load_count: usize,
    local_parser_context_count: usize,
    is_http_or_https: bool,
    has_javascript_result: bool,
    has_srcdoc: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReplacementPipelineBootstrapFacts {
    source_pipeline_id: PipelineId,
    pipeline_id: PipelineId,
    webview_id: WebViewId,
    browsing_context_id: BrowsingContextId,
    parent_pipeline_id: Option<PipelineId>,
    local_document_pipeline_id: Option<PipelineId>,
    local_document_count: usize,
    local_incomplete_load_count: usize,
    local_parser_context_count: usize,
    is_http_or_https: bool,
    has_javascript_result: bool,
    has_srcdoc: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct InitialPipelineActivationFacts {
    pipeline_id: PipelineId,
    webview_id: WebViewId,
    browsing_context_id: BrowsingContextId,
    parent_pipeline_id: Option<PipelineId>,
    local_document_pipeline_id: Option<PipelineId>,
    local_document_count: usize,
    local_incomplete_load_count: usize,
    local_parser_context_count: usize,
    parser_pipeline_id: Option<PipelineId>,
    is_http_or_https: bool,
    has_javascript_result: bool,
    has_srcdoc: bool,
    response_will_activate: bool,
}

/// Qualify only the first async fetch-backed root SpawnPipeline admitted for the exact target.
/// This pure check deliberately excludes every synchronous document-construction path.
fn initial_pipeline_bootstrap_pipeline(
    target: &PendingTargetObservation,
    facts: InitialPipelineBootstrapFacts,
) -> Option<PipelineId> {
    let only_pipeline = match target.pipelines() {
        [pipeline_id] => *pipeline_id,
        _ => return None,
    };
    let only_pending = match target.pending_top_level_pipelines() {
        [pipeline_id] => *pipeline_id,
        _ => return None,
    };
    (target.active_top_level.is_none() &&
        target.fully_active_pipelines().is_empty() &&
        only_pipeline == facts.pipeline_id &&
        only_pending == facts.pipeline_id &&
        facts.webview_id == target.webview_id &&
        facts.browsing_context_id == target.webview_id &&
        facts.parent_pipeline_id.is_none() &&
        facts.local_document_count == 0 &&
        facts.local_incomplete_load_count == 0 &&
        facts.local_parser_context_count == 0 &&
        facts.is_http_or_https &&
        !facts.has_javascript_result &&
        !facts.has_srcdoc)
        .then_some(facts.pipeline_id)
}

/// Qualify only the replacement root SpawnPipeline admitted beside one active source document.
fn replacement_pipeline_bootstrap_pipeline(
    target: &PendingTargetObservation,
    facts: ReplacementPipelineBootstrapFacts,
) -> Option<PipelineId> {
    let active = target.active_top_level?;
    let only_pending = match target.pending_top_level_pipelines() {
        [pipeline_id] => *pipeline_id,
        _ => return None,
    };
    (facts.source_pipeline_id != facts.pipeline_id &&
        active.pipeline_id == facts.source_pipeline_id &&
        target.pipelines().len() == 2 &&
        target.contains_pipeline(facts.source_pipeline_id) &&
        target.contains_pipeline(facts.pipeline_id) &&
        target.fully_active_pipelines() == [facts.source_pipeline_id] &&
        only_pending == facts.pipeline_id &&
        facts.webview_id == target.webview_id &&
        facts.browsing_context_id == target.webview_id &&
        facts.parent_pipeline_id.is_none() &&
        facts.local_document_pipeline_id == Some(facts.source_pipeline_id) &&
        facts.local_document_count == 1 &&
        facts.local_incomplete_load_count == 0 &&
        facts.local_parser_context_count == 0 &&
        facts.is_http_or_https &&
        !facts.has_javascript_result &&
        !facts.has_srcdoc)
        .then_some(facts.pipeline_id)
}

/// Qualify only the response-headers turn which transforms the already-bootstrapped pending root
/// into the same active root. The actual target transition is authorized separately by the
/// Constellation after it processes the correlated `ActivateDocument` message.
fn initial_pipeline_activation_pipeline(
    target: &PendingTargetObservation,
    facts: InitialPipelineActivationFacts,
) -> Option<PipelineId> {
    let only_pending = match target.pending_top_level_pipelines() {
        [pipeline_id] => *pipeline_id,
        _ => return None,
    };
    let exact_initial_target = target.active_top_level.is_none() &&
        target.pipelines() == [facts.pipeline_id] &&
        target.fully_active_pipelines().is_empty() &&
        facts.local_document_pipeline_id.is_none() &&
        facts.local_document_count == 0;
    let exact_replacement_target = target.active_top_level.is_some_and(|active| {
        active.pipeline_id != facts.pipeline_id &&
            facts.local_document_pipeline_id == Some(active.pipeline_id) &&
            target.pipelines().len() == 2 &&
            target.contains_pipeline(active.pipeline_id) &&
            target.contains_pipeline(facts.pipeline_id) &&
            target.fully_active_pipelines() == [active.pipeline_id] &&
            facts.local_document_count == 1
    });
    ((exact_initial_target || exact_replacement_target) &&
        only_pending == facts.pipeline_id &&
        facts.webview_id == target.webview_id &&
        facts.browsing_context_id == target.webview_id &&
        facts.parent_pipeline_id.is_none() &&
        facts.local_incomplete_load_count == 1 &&
        facts.local_parser_context_count == 1 &&
        facts.parser_pipeline_id == Some(facts.pipeline_id) &&
        facts.is_http_or_https &&
        !facts.has_javascript_result &&
        !facts.has_srcdoc &&
        facts.response_will_activate)
        .then_some(facts.pipeline_id)
}

fn is_pending_capture_error(error: &DocumentControlError) -> bool {
    matches!(
        error,
        DocumentControlError::PendingFactUnavailable(_) |
            DocumentControlError::PendingSnapshot(_) |
            DocumentControlError::TargetChanged { .. }
    )
}

fn checked_pending_count(count: usize) -> Result<u64, DocumentControlError> {
    u64::try_from(count).map_err(|_| DocumentControlError::QueueLengthOverflow)
}

/// The native executor performs all fallible selector and element validation before activation.
/// A failed fill or submit DOM operation can have reached the value setter/request-submit path
/// before Servo reports the error. Select also deliberately maps every fallible post-event rescan
/// to a select DOM-operation error: synchronous handlers have already observed the mutation, so
/// none can be a definitive rejection.
fn automation_error_may_follow_mutation(
    request: &DocumentAutomationRequest,
    error: &DocumentAutomationError,
) -> bool {
    matches!(
        (request.operation(), error),
        (
            DocumentAutomationOperation::Fill { .. },
            DocumentAutomationError::DomOperationFailed {
                operation: DocumentAutomationOperationKind::Fill,
            }
        ) | (
            DocumentAutomationOperation::Select { .. },
            DocumentAutomationError::DomOperationFailed {
                operation: DocumentAutomationOperationKind::Select,
            }
        ) | (
            DocumentAutomationOperation::Submit { .. },
            DocumentAutomationError::DomOperationFailed {
                operation: DocumentAutomationOperationKind::Submit,
            }
        )
    )
}

fn controlled_input_state_for_clock(
    document_clock: &DocumentClock,
) -> Option<RefCell<ControlledInputState>> {
    document_clock
        .is_controlled()
        .then(|| RefCell::new(ControlledInputState::default()))
}

/// A simple guard structure that restore the user interacting state when dropped
#[derive(Default)]
pub(crate) struct ScriptUserInteractingGuard {
    was_interacting: bool,
    user_interaction_cell: Rc<Cell<bool>>,
}

impl ScriptUserInteractingGuard {
    fn new(user_interaction_cell: Rc<Cell<bool>>) -> Self {
        let was_interacting = user_interaction_cell.get();
        user_interaction_cell.set(true);
        Self {
            was_interacting,
            user_interaction_cell,
        }
    }
}

impl Drop for ScriptUserInteractingGuard {
    fn drop(&mut self) {
        self.user_interaction_cell.set(self.was_interacting)
    }
}

/// This is the `ScriptThread`'s version of [`UserContents`] with the difference that user
/// stylesheets are represented as parsed `DocumentStyleSheet`s instead of simple source strings.
struct ScriptThreadUserContents {
    user_scripts: Rc<Vec<UserScript>>,
    user_stylesheets: Rc<Vec<DocumentStyleSheet>>,
}

impl ScriptThreadUserContents {
    fn new(user_contents: UserContents, shared_locks: &SharedRwLocks) -> Self {
        let user_stylesheets = user_contents
            .stylesheets
            .iter()
            .map(|user_stylesheet| {
                DocumentStyleSheet(ServoArc::new(Stylesheet::from_str(
                    user_stylesheet.source(),
                    user_stylesheet.url().into(),
                    Origin::User,
                    ServoArc::new(shared_locks.ua_or_user.wrap(MediaList::empty())),
                    shared_locks.ua_or_user.clone(),
                    None,
                    Some(&RustLogReporter),
                    QuirksMode::NoQuirks,
                    AllowImportRules::Yes,
                )))
            })
            .collect();
        Self {
            user_scripts: Rc::new(user_contents.scripts),
            user_stylesheets: Rc::new(user_stylesheets),
        }
    }
}

#[derive(Clone, MallocSizeOf)]
pub struct SharedRwLocks {
    pub author: SharedRwLock,
    pub ua_or_user: SharedRwLock,
}

impl Default for SharedRwLocks {
    fn default() -> Self {
        Self {
            author: SharedRwLock::new(),
            ua_or_user: SharedRwLock::new(),
        }
    }
}

/// Sticky local failure of the checked semantic DOM-mutation epoch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DomMutationEpochError {
    /// The epoch reached `u64::MAX` and cannot represent another distinct DOM state.
    Exhausted,
}

/// Owner-thread observation of semantic DOM mutations for one WebView.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct DomMutationObservation {
    /// Monotonic epoch of semantic attribute, character-data, and child-list mutations.
    pub(crate) epoch: u64,
    /// First checked epoch failure, retained without rejecting later DOM mutations.
    pub(crate) terminal: Option<DomMutationEpochError>,
}

/// ScriptThread-owned mutation epochs keyed by WebView rather than Document or Pipeline.
///
/// A navigation can replace either of those lower-level owners, while the WebView's semantic DOM
/// history must continue monotonically for ABA-safe pending-work observations.
#[derive(Default)]
struct DomMutationEpochTracker {
    observations: FxHashMap<WebViewId, DomMutationObservation>,
}

impl DomMutationEpochTracker {
    fn record(&mut self, webview_id: WebViewId) {
        let observation = self.observations.entry(webview_id).or_default();
        if observation.terminal.is_some() {
            return;
        }

        match observation.epoch.checked_add(1) {
            Some(epoch) => observation.epoch = epoch,
            None => observation.terminal = Some(DomMutationEpochError::Exhausted),
        }
    }

    fn observe(&self, webview_id: WebViewId) -> DomMutationObservation {
        self.observations
            .get(&webview_id)
            .copied()
            .unwrap_or_default()
    }
}

fn dom_mutation_epoch_tracker_for_clock(
    document_clock: &DocumentClock,
) -> Option<RefCell<DomMutationEpochTracker>> {
    document_clock
        .is_controlled()
        .then(|| RefCell::new(DomMutationEpochTracker::default()))
}

fn admit_controlled_session_history_revision(
    profile: DocumentControlProfile,
    revision: &Cell<u64>,
) -> bool {
    if profile != DocumentControlProfile::TopLevelSession {
        return true;
    }
    let Some(next_revision) = revision.get().checked_add(1) else {
        return false;
    };
    if next_revision > embedder_traits::CONTROLLED_SESSION_MAX_HISTORY_REVISIONS {
        return false;
    }
    revision.set(next_revision);
    true
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum SynchronousNavigationEmissionCapture {
    #[default]
    Inactive,
    Active {
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        emissions: u64,
    },
    Failed {
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        failure: SynchronousNavigationEmissionCaptureFailure,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SynchronousNavigationEmissionCaptureFailure {
    CaptureCorrupted,
    EmissionLimitExceeded,
    SendFailed,
}

fn record_synchronous_navigation_emission(
    capture: SynchronousNavigationEmissionCapture,
    webview_id: WebViewId,
    pipeline_id: PipelineId,
    send_succeeded: bool,
) -> SynchronousNavigationEmissionCapture {
    match capture {
        SynchronousNavigationEmissionCapture::Inactive => {
            SynchronousNavigationEmissionCapture::Inactive
        },
        SynchronousNavigationEmissionCapture::Active {
            webview_id: captured_webview_id,
            pipeline_id: captured_pipeline_id,
            ..
        } |
        SynchronousNavigationEmissionCapture::Failed {
            webview_id: captured_webview_id,
            pipeline_id: captured_pipeline_id,
            ..
        } if captured_webview_id != webview_id || captured_pipeline_id != pipeline_id => capture,
        SynchronousNavigationEmissionCapture::Failed { .. } => capture,
        SynchronousNavigationEmissionCapture::Active { .. } if !send_succeeded => {
            SynchronousNavigationEmissionCapture::Failed {
                webview_id,
                pipeline_id,
                failure: SynchronousNavigationEmissionCaptureFailure::SendFailed,
            }
        },
        SynchronousNavigationEmissionCapture::Active { emissions, .. } => emissions
            .checked_add(1)
            .filter(|next| *next <= embedder_traits::CONTROLLED_SESSION_MAX_HISTORY_REVISIONS)
            .map(|emissions| SynchronousNavigationEmissionCapture::Active {
                webview_id,
                pipeline_id,
                emissions,
            })
            .unwrap_or(SynchronousNavigationEmissionCapture::Failed {
                webview_id,
                pipeline_id,
                failure: SynchronousNavigationEmissionCaptureFailure::EmissionLimitExceeded,
            }),
    }
}

fn controlled_session_redirect_limit_before_next_fetch(
    profile: DocumentControlProfile,
    is_top_level: bool,
    current_url_list_len: usize,
) -> Option<u64> {
    if profile != DocumentControlProfile::TopLevelSession || !is_top_level {
        return None;
    }
    let observed = u64::try_from(current_url_list_len).unwrap_or(u64::MAX);
    (observed > embedder_traits::CONTROLLED_SESSION_MAX_REDIRECTS).then_some(observed)
}

fn complete_pipeline_exit_marker_ack(
    completed: &AtomicBool,
    result: Result<PipelineExitMarkerStatus, ipc_channel::IpcError>,
    report_failure: impl FnOnce(),
) {
    if completed.swap(true, Ordering::AcqRel) {
        return;
    }
    if !matches!(result, Ok(PipelineExitMarkerStatus::Recorded)) {
        report_failure();
    }
}

#[cfg(test)]
mod pipeline_exit_paint_marker_tests {
    use std::cell::Cell;
    use std::sync::atomic::AtomicBool;

    use ipc_channel::IpcError;
    use paint_api::PipelineExitMarkerStatus;

    use super::complete_pipeline_exit_marker_ack;

    #[test]
    fn recorded_ack_is_terminal_and_never_reports_failure() {
        let completed = AtomicBool::new(false);
        let failures = Cell::new(0);

        complete_pipeline_exit_marker_ack(
            &completed,
            Ok(PipelineExitMarkerStatus::Recorded),
            || failures.set(failures.get() + 1),
        );
        complete_pipeline_exit_marker_ack(
            &completed,
            Ok(PipelineExitMarkerStatus::PaintOwnerUnavailable),
            || failures.set(failures.get() + 1),
        );

        assert_eq!(failures.get(), 0);
    }

    #[test]
    fn marker_loss_reports_exactly_one_failure() {
        for result in [
            Ok(PipelineExitMarkerStatus::CrossProcessSendFailed),
            Ok(PipelineExitMarkerStatus::LocalQueueRejected),
            Ok(PipelineExitMarkerStatus::PaintOwnerUnavailable),
            Ok(PipelineExitMarkerStatus::DrainedDuringShutdown),
            Ok(PipelineExitMarkerStatus::PaintShutdownFinished),
            Err(IpcError::Disconnected),
        ] {
            let completed = AtomicBool::new(false);
            let failures = Cell::new(0);

            complete_pipeline_exit_marker_ack(&completed, result, || {
                failures.set(failures.get() + 1)
            });
            complete_pipeline_exit_marker_ack(
                &completed,
                Ok(PipelineExitMarkerStatus::Recorded),
                || failures.set(failures.get() + 1),
            );

            assert_eq!(failures.get(), 1);
        }
    }

    #[test]
    fn script_marker_is_published_before_logical_pipeline_exit() {
        let source = include_str!("script_thread.rs");
        let start = source.rfind("fn handle_exit_pipeline_msg").unwrap();
        let end = source[start..]
            .find("/// Handles a request to exit the script thread")
            .map(|offset| start + offset)
            .unwrap();
        let exit_source = &source[start..end];
        let paint_marker = exit_source.find("self.paint_api.pipeline_exited(").unwrap();
        let logical_exit = exit_source
            .find("ScriptToConstellationMessage::PipelineExited,")
            .unwrap();

        assert!(paint_marker < logical_exit);
        assert!(exit_source[paint_marker..logical_exit].contains("PipelineExitSource::Script"));
    }
}

#[derive(JSTraceable)]
// ScriptThread instances are rooted on creation, so this is okay
#[cfg_attr(crown, expect(crown::unrooted_must_root))]
pub struct ScriptThread {
    /// Immutable identity of this event loop, retained for controlled target authority.
    #[no_trace]
    event_loop_id: ScriptEventLoopId,

    /// A reference to the currently operating `ScriptThread`. This should always be
    /// upgradable to an `Rc` as long as the `ScriptThread` is running.
    #[no_trace]
    this: Weak<ScriptThread>,

    /// <https://html.spec.whatwg.org/multipage/#last-render-opportunity-time>
    #[no_trace]
    last_render_opportunity_time: Cell<Option<DocumentTime>>,

    /// The documents for pipelines managed by this thread
    documents: DomRefCell<DocumentCollection>,
    /// The window proxies known by this thread
    window_proxies: Rc<ScriptWindowProxies>,
    /// A list of data pertaining to loads that have not yet received a network response
    incomplete_loads: DomRefCell<Vec<InProgressLoad>>,
    /// A vector containing parser contexts which have not yet been fully processed
    incomplete_parser_contexts: IncompleteParserContexts,
    /// An [`ImageCacheFactory`] to use for creating [`ImageCache`]s for all of the
    /// child `Pipeline`s.
    #[no_trace]
    image_cache_factory: Arc<dyn ImageCacheFactory>,

    /// A [`ScriptThreadReceivers`] holding all of the incoming `Receiver`s for messages
    /// to this [`ScriptThread`].
    receivers: ScriptThreadReceivers,

    /// A [`ScriptThreadSenders`] that holds all outgoing sending channels necessary to communicate
    /// to other parts of Servo.
    senders: ScriptThreadSenders,

    /// A handle to the resource thread. This is an `Arc` to avoid running out of file descriptors if
    /// there are many iframes.
    #[no_trace]
    resource_threads: ResourceThreads,

    #[no_trace]
    storage_threads: StorageThreads,

    /// A queue of tasks to be executed in this script-thread.
    task_queue: TaskQueue<MainThreadScriptMsg>,

    /// The dedicated means of communication with the background-hang-monitor for this script-thread.
    #[no_trace]
    background_hang_monitor: Box<dyn BackgroundHangMonitor>,
    /// A flag set to `true` by the BHM on exit, and checked from within the interrupt handler.
    closing: Arc<AtomicBool>,

    /// A [`TimerScheduler`] used to schedule timers for this [`ScriptThread`]. Timers are handled
    /// in the [`ScriptThread`] event loop.
    #[no_trace]
    timer_scheduler: RefCell<TimerScheduler>,

    /// The document-observable clock shared by this event loop's Window realms and timer queue.
    #[no_trace]
    document_clock: DocumentClock,

    /// The immutable top-level document authority selected independently from the clock.
    #[no_trace]
    document_control_profile: DocumentControlProfile,

    /// The immutable execution-surface policy selected independently from document authority.
    #[no_trace]
    document_execution_profile: DocumentExecutionProfile,

    /// Script-side pre-mutation mirror of Constellation's session history revision.
    controlled_session_history_revision: Cell<u64>,

    /// Checked capture of successful top-level authority messages emitted synchronously by the
    /// currently executing v2 mutating automation command.
    #[no_trace]
    controlled_automation_navigation_capture: Cell<SynchronousNavigationEmissionCapture>,

    /// Session-scoped work accounting installed before controlled navigation begins.
    #[no_trace]
    document_execution_ledger: Option<DocumentExecutionLedger>,

    /// Optional linearizable lifecycle fence shared by tracked tasks on this event loop.
    #[no_trace]
    document_producer_fence: Option<DocumentProducerFence>,

    /// Ordinary input retained behind the controlled event-loop barrier.
    #[no_trace]
    controlled_input: Option<RefCell<ControlledInputState>>,

    /// Persistent generation, producer, and single-use token authority for controlled commands.
    #[no_trace]
    document_control_state: Option<RefCell<DocumentControlState>>,

    /// First checked timer-scheduler failure observed by this ScriptThread.
    #[no_trace]
    timer_control_terminal: Cell<Option<TimerControlError>>,

    /// Checked semantic DOM-mutation epochs, isolated by WebView and retained across navigation.
    ///
    /// This owner-thread state is enabled only for controlled execution. Keeping it on the
    /// ScriptThread rather than a Document preserves the counter when navigation replaces the
    /// active Document for the same WebView.
    #[no_trace]
    dom_mutation_epochs: Option<RefCell<DomMutationEpochTracker>>,

    /// A proxy to the `SystemFontService` to use for accessing system font lists.
    #[no_trace]
    system_font_service: Arc<SystemFontServiceProxy>,

    /// The JavaScript runtime.
    js_runtime: Rc<Runtime>,

    /// List of pipelines that have been owned and closed by this script thread.
    #[no_trace]
    closed_pipelines: DomRefCell<FxHashSet<PipelineId>>,

    /// <https://html.spec.whatwg.org/multipage/#microtask-queue>
    microtask_queue: Rc<MicrotaskQueue>,

    mutation_observers: Rc<ScriptMutationObservers>,

    /// A handle to the WebGL thread
    #[no_trace]
    #[cfg(feature = "webgl")]
    webgl_chan: Option<WebGLPipeline>,

    /// The WebXR device registry
    #[no_trace]
    #[cfg(feature = "webxr")]
    webxr_registry: Option<webxr_api::Registry>,

    /// A list of pipelines containing documents that finished loading all their blocking
    /// resources during a turn of the event loop.
    /// TODO(43149): Remove when document replacement is implemented
    docs_with_no_blocking_loads: DomRefCell<FxHashSet<Dom<Document>>>,

    /// <https://html.spec.whatwg.org/multipage/#custom-element-reactions-stack>
    custom_element_reaction_stack: Rc<CustomElementReactionStack>,

    /// Cross-process access to `Paint`'s API.
    #[no_trace]
    paint_api: CrossProcessPaintApi,

    /// Periodically print out on which events script threads spend their processing time.
    profile_script_events: bool,

    /// Unminify Javascript.
    unminify_js: bool,

    /// Directory with stored unminified scripts
    local_script_source: Option<String>,

    /// Unminify Css.
    unminify_css: bool,

    /// The [`SharedRwLocks`] that are used by all Stylo operations in this ScriptThread.
    #[no_trace]
    shared_style_locks: SharedRwLocks,

    /// A map from [`UserContentManagerId`] to its [`UserContents`]. This is initialized
    /// with a copy of the map in constellation (via the `InitialScriptState`). After that,
    /// the constellation forwards any mutations to this `ScriptThread` using messages.
    #[no_trace]
    user_contents_for_manager_id:
        RefCell<FxHashMap<UserContentManagerId, ScriptThreadUserContents>>,

    /// Application window's GL Context for Media player
    #[no_trace]
    player_context: WindowGLContext,

    /// A map from pipelines to all owned nodes ever created in this script thread
    #[no_trace]
    pipeline_to_node_ids: DomRefCell<FxHashMap<PipelineId, NodeIdSet>>,

    /// Code is running as a consequence of a user interaction
    is_user_interacting: Rc<Cell<bool>>,

    /// Identity manager for WebGPU resources
    #[no_trace]
    #[cfg(feature = "webgpu")]
    gpu_id_hub: Arc<IdentityHub>,

    /// A factory for making new layouts. This allows layout to depend on script.
    #[no_trace]
    layout_factory: Arc<dyn LayoutFactory>,

    /// The [`TimerId`] of a ScriptThread-scheduled "update the rendering" call, if any.
    /// The ScriptThread schedules calls to "update the rendering," but the renderer can
    /// also do this when animating. Renderer-based calls always take precedence.
    #[no_trace]
    scheduled_update_the_rendering: RefCell<Option<TimerId>>,

    /// The scheduled rendering callback has detached from the scheduler and confirmed dispatch.
    /// This distinguishes a ready opportunity from the unsafe detach-to-callback handoff window.
    #[no_trace]
    scheduled_rendering_delivery_ready: Arc<AtomicBool>,

    /// Whether an animation tick or ScriptThread-triggered rendering update is pending. This might
    /// either be because the Servo renderer is managing animations and the [`ScriptThread`] has
    /// received a [`ScriptThreadMessage::TickAllAnimations`] message, because the [`ScriptThread`]
    /// itself is managing animations the timer fired triggering a [`ScriptThread`]-based
    /// animation tick, or if there are no animations running and the [`ScriptThread`] has noticed a
    /// change that requires a rendering update.
    needs_rendering_update: Arc<AtomicBool>,

    debugger_global: Dom<DebuggerGlobalScope>,

    debugger_paused: Cell<bool>,

    controlled_debugger_unsupported: Cell<bool>,

    /// A list of URLs that can access privileged internal APIs.
    #[no_trace]
    privileged_urls: Vec<ServoUrl>,

    devtools_state: DevtoolsState,
}

struct BHMExitSignal {
    closing: Arc<AtomicBool>,
    js_context: ThreadSafeJSContext,
}

impl BackgroundHangMonitorExitSignal for BHMExitSignal {
    fn signal_to_exit(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.js_context.request_interrupt_callback();
    }
}

#[expect(unsafe_code)]
unsafe extern "C" fn interrupt_callback(_cx: *mut UnsafeJSContext) -> bool {
    let res = ScriptThread::can_continue_running();
    if !res {
        ScriptThread::prepare_for_shutdown();
    }
    res
}

/// In the event of thread panic, all data on the stack runs its destructor. However, there
/// are no reachable, owning pointers to the DOM memory, so it never gets freed by default
/// when the script thread fails. The ScriptMemoryFailsafe uses the destructor bomb pattern
/// to forcibly tear down the JS realms for pages associated with the failing ScriptThread.
struct ScriptMemoryFailsafe<'a> {
    owner: Option<&'a ScriptThread>,
}

impl<'a> ScriptMemoryFailsafe<'a> {
    fn neuter(&mut self) {
        self.owner = None;
    }

    fn new(owner: &'a ScriptThread) -> ScriptMemoryFailsafe<'a> {
        ScriptMemoryFailsafe { owner: Some(owner) }
    }
}

impl Drop for ScriptMemoryFailsafe<'_> {
    fn drop(&mut self) {
        if let Some(owner) = self.owner {
            for (_, document) in owner.documents.borrow().iter() {
                document.window().clear_js_runtime_for_script_deallocation();
            }
        }
    }
}

impl ScriptThreadFactory for ScriptThread {
    fn create(
        state: InitialScriptState,
        layout_factory: Arc<dyn LayoutFactory>,
        image_cache_factory: Arc<dyn ImageCacheFactory>,
        background_hang_monitor_register: Box<dyn BackgroundHangMonitorRegister>,
    ) -> JoinHandle<()> {
        // Setup pipeline-namespace-installing for all threads in this process.
        // Idempotent in single-process mode.
        PipelineNamespace::set_installer_sender(state.namespace_request_sender.clone());

        let script_thread_id = state.id;
        thread::Builder::new()
            .name(format!("Script#{script_thread_id}"))
            .stack_size(8 * 1024 * 1024) // 8 MiB stack to be consistent with other browsers.
            .spawn(move || {
                profile_traits::debug_event!(
                    "ScriptThread::spawned",
                    script_thread_id = script_thread_id.to_string()
                );
                thread_state::initialize(ThreadState::SCRIPT);
                PipelineNamespace::install(state.pipeline_namespace_id);
                ScriptEventLoopId::install(state.id);
                let memory_profiler_sender = state.memory_profiler_sender.clone();
                let reporter_name = format!("script-reporter-{script_thread_id:?}");
                let (script_thread, mut cx) = ScriptThread::new(
                    state,
                    layout_factory,
                    image_cache_factory,
                    background_hang_monitor_register,
                );
                SCRIPT_THREAD_ROOT.with(|root| {
                    root.set(Some(Rc::as_ptr(&script_thread)));
                });
                servo_base::threadboost::boost_thread(
                    ThreadPriority::Critical,
                    BoostAffinity::Boost,
                );
                let mut failsafe = ScriptMemoryFailsafe::new(&script_thread);

                memory_profiler_sender.run_with_memory_reporting(
                    || {
                        if script_thread.document_clock.is_controlled() {
                            script_thread.start_controlled(&mut cx);
                        } else {
                            script_thread.start(&mut cx);
                        }
                    },
                    reporter_name,
                    ScriptEventLoopSender::MainThread {
                        sender: script_thread.senders.self_sender.clone(),
                        producer_fence: script_thread.document_producer_fence.clone(),
                    },
                    CommonScriptMsg::CollectReports,
                );

                // This must always be the very last operation performed before the thread completes
                failsafe.neuter();
            })
            .expect("Thread spawning failed")
    }
}

#[servo_tracing::instrument_all(skip_all)]
impl ScriptThread {
    pub(crate) fn runtime_handle() -> ParentRuntime {
        with_optional_script_thread(|script_thread| {
            script_thread.unwrap().js_runtime.prepare_for_new_child()
        })
    }

    pub(crate) fn can_continue_running() -> bool {
        with_script_thread(|script_thread| script_thread.can_continue_running_inner())
    }

    pub(crate) fn prepare_for_shutdown() {
        with_script_thread(|script_thread| {
            script_thread.prepare_for_shutdown_inner();
        })
    }

    pub(crate) fn mutation_observers() -> Rc<ScriptMutationObservers> {
        with_script_thread(|script_thread| script_thread.mutation_observers.clone())
    }

    pub(crate) fn microtask_queue() -> Rc<MicrotaskQueue> {
        with_script_thread(|script_thread| script_thread.microtask_queue.clone())
    }

    pub(crate) fn shared_style_locks(&self) -> &SharedRwLocks {
        &self.shared_style_locks
    }

    pub(crate) fn mark_document_with_no_blocked_loads(doc: &Document) {
        with_script_thread(|script_thread| {
            script_thread
                .docs_with_no_blocking_loads
                .borrow_mut()
                .insert(Dom::from_ref(doc));
        })
    }

    pub(crate) fn page_headers_available(
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        metadata: Option<&Metadata>,
        origin: MutableOrigin,
        cx: &mut js::context::JSContext,
    ) -> Option<DomRoot<Document>> {
        with_script_thread(|script_thread| {
            script_thread.handle_page_headers_available(
                webview_id,
                pipeline_id,
                metadata,
                origin,
                cx,
            )
        })
    }

    /// Process a single event as if it were the next event
    /// in the queue for this window event-loop.
    /// Returns a boolean indicating whether further events should be processed.
    pub(crate) fn process_event(msg: CommonScriptMsg, cx: &mut js::context::JSContext) -> bool {
        with_script_thread(|script_thread| {
            if !script_thread.can_continue_running_inner() {
                return false;
            }
            script_thread.handle_msg_from_script(MainThreadScriptMsg::Common(msg), cx);
            true
        })
    }

    /// Schedule a timer while preserving checked clock, deadline, and sequence failures.
    pub(crate) fn try_schedule_timer(
        &self,
        request: TimerEventRequest,
    ) -> Result<TimerId, TimerControlError> {
        try_schedule_timer_recording_terminal(
            &mut self.timer_scheduler.borrow_mut(),
            &self.timer_control_terminal,
            request,
        )
    }

    /// Return the first checked timer-scheduler failure without clearing it.
    pub(crate) fn timer_control_terminal_error(&self) -> Option<TimerControlError> {
        self.timer_control_terminal.get()
    }

    /// Record one semantic DOM mutation for a WebView without allowing its epoch to wrap.
    ///
    /// Overflow is observational: the DOM mutation is not rejected or rolled back. Instead the
    /// first terminal is latched and all later mutations leave both the maximum epoch and terminal
    /// unchanged.
    pub(crate) fn record_dom_mutation(&self, webview_id: WebViewId) {
        if let Some(ledger) = &self.document_execution_ledger {
            ledger.record_mutation_record();
        }
        let Some(dom_mutation_epochs) = &self.dom_mutation_epochs else {
            return;
        };
        dom_mutation_epochs.borrow_mut().record(webview_id);
    }

    /// Observe the semantic DOM epoch owned by this ScriptThread for one WebView.
    ///
    /// Repeated observation is side-effect free. `None` means this ScriptThread is not collecting
    /// controlled pending-work evidence; a tracked WebView with no mutations observes epoch zero.
    pub(crate) fn dom_mutation_observation(
        &self,
        webview_id: WebViewId,
    ) -> Option<DomMutationObservation> {
        self.dom_mutation_epochs
            .as_ref()
            .map(|epochs| epochs.borrow().observe(webview_id))
    }

    /// Cancel a the [`TimerEventRequest`] for the given [`TimerId`] on this
    /// [`ScriptThread`]'s [`TimerScheduler`].
    pub(crate) fn cancel_timer(&self, timer_id: TimerId) {
        self.timer_scheduler.borrow_mut().cancel_timer(timer_id)
    }

    /// Return the document clock for the current ScriptThread.
    pub(crate) fn current_document_clock() -> DocumentClock {
        with_script_thread(|script_thread| script_thread.document_clock.clone())
    }

    /// Return the top-level document authority for the current ScriptThread.
    pub(crate) fn current_document_control_profile() -> DocumentControlProfile {
        with_script_thread(|script_thread| script_thread.document_control_profile)
    }

    /// Return the execution-surface policy for the current ScriptThread.
    pub(crate) fn current_document_execution_profile() -> DocumentExecutionProfile {
        with_script_thread(|script_thread| script_thread.document_execution_profile)
    }

    /// Whether `window` can be conservatively reconstructed as the sole fully-active,
    /// non-auxiliary top-level Document currently retained by this controlled event loop.
    ///
    /// A ScriptThread profile is event-loop-wide, so checking it together with
    /// `Window::is_top_level()` is not sufficient: an auxiliary WebView can share the same loop.
    /// This intentionally denies timestamp authority whenever that singleton membership cannot
    /// be reconstructed; it does not claim a separately retained Constellation target identity.
    pub(crate) fn current_controlled_top_level_target_matches(window: &Window) -> bool {
        with_script_thread(|script_thread| {
            if script_thread.document_control_profile != DocumentControlProfile::TopLevelSession ||
                script_thread.document_execution_profile !=
                    DocumentExecutionProfile::ControlledWebSessionV2 ||
                script_thread.controlled_input.is_none() ||
                !window.is_top_level()
            {
                return false;
            }

            let Some(window_proxy) = window.undiscarded_window_proxy() else {
                return false;
            };
            if window_proxy.is_auxiliary() {
                return false;
            }

            let webview_id = window.webview_id();
            let Some(state) = &script_thread.document_control_state else {
                return false;
            };
            if state.borrow().pending.owner_snapshot(webview_id).is_err() ||
                script_thread
                    .incomplete_loads
                    .borrow()
                    .iter()
                    .any(|load| load.webview_id != webview_id)
            {
                return false;
            }

            let documents = script_thread.documents.borrow();
            let mut documents = documents.iter();
            let Some((pipeline_id, document)) = documents.next() else {
                return false;
            };
            documents.next().is_none() &&
                pipeline_id == window.pipeline_id() &&
                document.webview_id() == webview_id &&
                document.is_fully_active() &&
                std::ptr::eq(document.window(), window)
        })
    }

    /// Admit one same-document authority change before script-visible history mutation.
    pub(crate) fn admit_controlled_session_history_change() -> bool {
        with_script_thread(|script_thread| {
            admit_controlled_session_history_revision(
                script_thread.document_control_profile,
                &script_thread.controlled_session_history_revision,
            )
        })
    }

    fn begin_synchronous_navigation_emission_capture(
        &self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
    ) -> Result<(), ()> {
        match self.controlled_automation_navigation_capture.replace(
            SynchronousNavigationEmissionCapture::Active {
                webview_id,
                pipeline_id,
                emissions: 0,
            },
        ) {
            SynchronousNavigationEmissionCapture::Inactive => Ok(()),
            _ => {
                self.controlled_automation_navigation_capture.set(
                    SynchronousNavigationEmissionCapture::Failed {
                        webview_id,
                        pipeline_id,
                        failure: SynchronousNavigationEmissionCaptureFailure::CaptureCorrupted,
                    },
                );
                Err(())
            },
        }
    }

    fn finish_synchronous_navigation_emission_capture(
        &self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
    ) -> Result<bool, ()> {
        match self
            .controlled_automation_navigation_capture
            .replace(SynchronousNavigationEmissionCapture::Inactive)
        {
            SynchronousNavigationEmissionCapture::Active {
                webview_id: captured_webview_id,
                pipeline_id: captured_pipeline_id,
                emissions,
            } if captured_webview_id == webview_id && captured_pipeline_id == pipeline_id => {
                Ok(emissions > 0)
            },
            SynchronousNavigationEmissionCapture::Inactive |
            SynchronousNavigationEmissionCapture::Active { .. } |
            SynchronousNavigationEmissionCapture::Failed { .. } => Err(()),
        }
    }

    /// Record one successful or failed synchronous top-level authority-message send.
    ///
    /// The return value is true only when a v2 mutating automation capture owns the send. Callers
    /// use this to suppress their ordinary transport panic/error after latching a failure so the
    /// command can complete as explicitly indeterminate instead.
    pub(crate) fn record_synchronous_navigation_emission(
        is_top_level: bool,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        send_succeeded: bool,
    ) -> bool {
        if !is_top_level {
            return false;
        }
        with_optional_script_thread(|script_thread| {
            let Some(script_thread) = script_thread else {
                return false;
            };
            let capture = script_thread.controlled_automation_navigation_capture.get();
            if matches!(capture, SynchronousNavigationEmissionCapture::Inactive) {
                return false;
            }
            let capture_matches_sender = matches!(
                capture,
                SynchronousNavigationEmissionCapture::Active {
                    webview_id: captured_webview_id,
                    pipeline_id: captured_pipeline_id,
                    ..
                } | SynchronousNavigationEmissionCapture::Failed {
                    webview_id: captured_webview_id,
                    pipeline_id: captured_pipeline_id,
                    ..
                } if captured_webview_id == webview_id && captured_pipeline_id == pipeline_id
            );
            if !capture_matches_sender {
                return false;
            }
            script_thread.controlled_automation_navigation_capture.set(
                record_synchronous_navigation_emission(
                    capture,
                    webview_id,
                    pipeline_id,
                    send_succeeded,
                ),
            );
            true
        })
    }

    /// Return producer tracking shared by main-thread task senders on this ScriptThread.
    pub(crate) fn document_producer_fence(&self) -> Option<DocumentProducerFence> {
        self.document_producer_fence.clone()
    }

    // https://html.spec.whatwg.org/multipage/#await-a-stable-state
    pub(crate) fn await_stable_state(cx: &JSContext, task: Box<dyn MicrotaskRunnable>) {
        with_script_thread(|script_thread| {
            script_thread.microtask_queue.enqueue(cx, task);
        });
    }

    /// Check that two origins are "similar enough",
    /// for now only used to prevent cross-origin JS url evaluation.
    ///
    /// <https://github.com/whatwg/html/issues/2591>
    fn check_load_origin(source: &LoadOrigin, target: &OriginSnapshot) -> bool {
        match source {
            LoadOrigin::Constellation | LoadOrigin::WebDriver => {
                // Always allow loads initiated by the constellation or webdriver.
                true
            },
            LoadOrigin::Script(source_origin) => source_origin.same_origin_domain(target),
        }
    }

    /// Inform the `ScriptThread` that it should make a call to
    /// [`ScriptThread::update_the_rendering`] as soon as possible, as the rendering
    /// update timer has fired or the renderer has asked us for a new rendering update.
    pub(crate) fn set_needs_rendering_update(&self) {
        self.needs_rendering_update.store(true, Ordering::Relaxed);
    }

    /// <https://html.spec.whatwg.org/multipage/#navigate-to-a-javascript:-url>
    pub(crate) fn can_navigate_to_javascript_url(
        cx: &mut js::context::JSContext,
        initiator_global: &GlobalScope,
        target_global: &GlobalScope,
        load_data: &mut LoadData,
        container: Option<&Element>,
    ) -> bool {
        // Step 3. If initiatorOrigin is not same origin-domain with targetNavigable's active document's origin, then return.
        //
        // Important re security. See https://github.com/servo/servo/issues/23373
        if !Self::check_load_origin(&load_data.load_origin, &target_global.origin().snapshot()) {
            return false;
        }

        // Step 5: If the result of should navigation request of type be blocked by
        // Content Security Policy? given request and cspNavigationType is "Blocked", then return. [CSP]
        if initiator_global
            .get_csp_list()
            .should_navigation_request_be_blocked(cx, initiator_global, load_data, container)
        {
            return false;
        }

        true
    }

    /// Attempt to navigate a global to a javascript: URL. Returns true if a new document is created.
    /// <https://html.spec.whatwg.org/multipage/#navigate-to-a-javascript:-url>
    pub(crate) fn navigate_to_javascript_url(
        cx: &mut js::context::JSContext,
        initiator_global: &GlobalScope,
        target_global: &GlobalScope,
        load_data: &mut LoadData,
        container: Option<&Element>,
        initial_insertion: Option<bool>,
    ) -> bool {
        // Step 6. If the result of should navigation request of type be blocked by Content Security Policy? given request and cspNavigationType is "Blocked", then return.
        if !Self::can_navigate_to_javascript_url(
            cx,
            initiator_global,
            target_global,
            load_data,
            container,
        ) {
            return false;
        }

        // Step 7. Let newDocument be the result of evaluating a javascript: URL given targetNavigable,
        // url, initiatorOrigin, and userInvolvement.
        let Some(body) = Self::eval_js_url(cx, target_global, &load_data.url) else {
            // Step 8. If newDocument is null:
            let window_proxy = target_global.as_window().window_proxy();
            if let Some(frame_element) = window_proxy
                .frame_element()
                .and_then(Castable::downcast::<HTMLIFrameElement>)
            {
                // Step 8.1 If initialInsertion is true and targetNavigable's active document's is initial about:blank is true, then run the iframe load event steps given targetNavigable's container.
                if initial_insertion == Some(true) && frame_element.is_initial_blank_document() {
                    frame_element.run_iframe_load_event_steps(cx);
                }
            }
            // Step 8.2. Return.
            return false;
        };

        // Step 11. of <https://html.spec.whatwg.org/multipage/#evaluate-a-javascript:-url>.
        // Let response be a new response with
        // URL         targetNavigable's active document's URL
        // header list « (`Content-Type`, `text/html;charset=utf-8`) »
        // body        the UTF-8 encoding of result, as a body
        load_data.js_eval_result = Some(body);
        load_data.url = target_global.get_url();
        load_data
            .headers
            .typed_insert(headers::ContentType::from(mime::TEXT_HTML_UTF_8));
        true
    }

    pub(crate) fn get_top_level_for_browsing_context(
        sender_webview_id: WebViewId,
        sender_pipeline_id: PipelineId,
        browsing_context_id: BrowsingContextId,
    ) -> Option<WebViewId> {
        with_script_thread(|script_thread| {
            script_thread.ask_constellation_for_top_level_info(
                sender_webview_id,
                sender_pipeline_id,
                browsing_context_id,
            )
        })
    }

    pub(crate) fn find_window(id: PipelineId) -> Option<DomRoot<Window>> {
        with_script_thread(|script_thread| script_thread.documents.borrow().find_window(id))
    }

    pub(crate) fn find_document(id: PipelineId) -> Option<DomRoot<Document>> {
        with_script_thread(|script_thread| script_thread.documents.borrow().find_document(id))
    }

    /// Creates a guard that sets user_is_interacting to true and returns the
    /// state of user_is_interacting on drop of the guard.
    /// Notice that you need to use `let _guard = ...` as `let _ = ...` is not enough
    #[must_use]
    pub(crate) fn user_interacting_guard() -> ScriptUserInteractingGuard {
        with_script_thread(|script_thread| {
            ScriptUserInteractingGuard::new(script_thread.is_user_interacting.clone())
        })
    }

    pub(crate) fn is_user_interacting() -> bool {
        with_script_thread(|script_thread| script_thread.is_user_interacting.get())
    }

    pub(crate) fn get_fully_active_document_ids(&self) -> FxHashSet<PipelineId> {
        self.documents
            .borrow()
            .iter()
            .filter_map(|(id, document)| {
                if document.is_fully_active() {
                    Some(id)
                } else {
                    None
                }
            })
            .fold(FxHashSet::default(), |mut set, id| {
                let _ = set.insert(id);
                set
            })
    }

    pub(crate) fn window_proxies() -> Rc<ScriptWindowProxies> {
        with_script_thread(|script_thread| script_thread.window_proxies.clone())
    }

    pub(crate) fn find_window_proxy_by_name(name: &DOMString) -> Option<DomRoot<WindowProxy>> {
        with_script_thread(|script_thread| {
            script_thread.window_proxies.find_window_proxy_by_name(name)
        })
    }

    fn handle_register_paint_worklet(
        &self,
        pipeline_id: PipelineId,
        name: Atom,
        properties: Vec<Atom>,
        painter: Box<dyn Painter>,
    ) {
        let Some(window) = self.documents.borrow().find_window(pipeline_id) else {
            warn!("Paint worklet registered after pipeline {pipeline_id} closed.");
            return;
        };

        window
            .layout_mut()
            .register_paint_worklet_modules(name, properties, painter);
    }

    pub(crate) fn custom_element_reaction_stack() -> Rc<CustomElementReactionStack> {
        with_optional_script_thread(|script_thread| {
            script_thread
                .as_ref()
                .unwrap()
                .custom_element_reaction_stack
                .clone()
        })
    }

    pub(crate) fn enqueue_callback_reaction(
        cx: &mut js::context::JSContext,
        element: &Element,
        reaction: CallbackReaction,
        definition: Option<Rc<CustomElementDefinition>>,
    ) {
        with_script_thread(|script_thread| {
            script_thread
                .custom_element_reaction_stack
                .enqueue_callback_reaction(cx, element, reaction, definition);
        })
    }

    pub(crate) fn enqueue_upgrade_reaction(
        cx: &js::context::JSContext,
        element: &Element,
        definition: Rc<CustomElementDefinition>,
    ) {
        with_script_thread(|script_thread| {
            script_thread
                .custom_element_reaction_stack
                .enqueue_upgrade_reaction(cx, element, definition);
        })
    }

    pub(crate) fn invoke_backup_element_queue(cx: &mut js::context::JSContext) {
        with_script_thread(|script_thread| {
            script_thread
                .custom_element_reaction_stack
                .invoke_backup_element_queue(cx);
        })
    }

    pub(crate) fn save_node_id(pipeline: PipelineId, node_id: String) {
        with_script_thread(|script_thread| {
            script_thread
                .pipeline_to_node_ids
                .borrow_mut()
                .entry(pipeline)
                .or_default()
                .insert(node_id);
        })
    }

    pub(crate) fn has_node_id(pipeline: PipelineId, node_id: &str) -> bool {
        with_script_thread(|script_thread| {
            script_thread
                .pipeline_to_node_ids
                .borrow()
                .get(&pipeline)
                .is_some_and(|node_ids| node_ids.contains(node_id))
        })
    }

    /// Creates a new script thread.
    #[servo_tracing::instrument(name = "ScripThread::new", level = "debug", skip_all)]
    pub(crate) fn new(
        state: InitialScriptState,
        layout_factory: Arc<dyn LayoutFactory>,
        image_cache_factory: Arc<dyn ImageCacheFactory>,
        background_hang_monitor_register: Box<dyn BackgroundHangMonitorRegister>,
    ) -> (Rc<ScriptThread>, js::context::JSContext) {
        let (self_sender, self_receiver) = unbounded();
        // The clock mode is immutable and selected by the WebView before its initial navigation.
        // Every Window and timer queue on this event loop shares this one clock domain.
        let document_clock = DocumentClock::new(state.document_clock);
        let document_control_profile = state.document_control_profile;
        let document_execution_profile = state.document_execution_profile;
        let document_execution_ledger = document_clock.is_controlled().then(|| {
            DocumentExecutionLedger::new(
                document_clock.id(),
                DocumentExecutionLimits::CONTROLLED_WEBAPP_V1,
            )
        });
        let dom_mutation_epochs = dom_mutation_epoch_tracker_for_clock(&document_clock);
        let document_producer_fence =
            document_producer_fence_for_clock(&document_clock, &state.script_to_embedder_sender);
        let controlled_input = controlled_input_state_for_clock(&document_clock);
        let document_control_state = document_clock
            .is_controlled()
            .then(|| RefCell::new(DocumentControlState::new(state.id)));
        let mut runtime = Runtime::new(Some(ScriptEventLoopSender::MainThread {
            sender: self_sender.clone(),
            producer_fence: document_producer_fence.clone(),
        }));
        runtime
            .microtask_queue
            .install_execution_ledger(document_execution_ledger.clone());

        // SAFETY: We ensure that only one JSContext exists in this thread.
        // This is the first one and the only one
        let mut cx = unsafe { runtime.cx() };

        unsafe {
            SetWindowProxyClass(&cx, GetWindowProxyClass());
            JS_AddInterruptCallback(&cx, Some(interrupt_callback));
        }

        let constellation_receiver = state
            .constellation_to_script_receiver
            .route_preserving_errors();
        let document_control_receiver = state.document_control_receiver.route_preserving_errors();

        // Ask the router to proxy IPC messages from the devtools to us.
        let devtools_server_sender = state.devtools_server_sender;
        let (ipc_devtools_sender, ipc_devtools_receiver) = generic_channel::channel().unwrap();
        let devtools_server_receiver = ipc_devtools_receiver.route_preserving_errors();

        let task_queue = TaskQueue::new_with_producer_tracking(
            self_receiver,
            self_sender.clone(),
            document_producer_fence.is_some(),
        );

        let closing = Arc::new(AtomicBool::new(false));
        let background_hang_monitor_exit_signal = BHMExitSignal {
            closing: closing.clone(),
            js_context: runtime.thread_safe_js_context(),
        };

        let background_hang_monitor = background_hang_monitor_register.register_component(
            // TODO: We shouldn't rely on this PipelineId as a ScriptThread can have multiple
            // Pipelines and any of them might disappear at any time.
            MonitoredComponentId(state.id, MonitoredComponentType::Script),
            Duration::from_millis(1000),
            Duration::from_millis(5000),
            Box::new(background_hang_monitor_exit_signal),
        );

        let (image_cache_sender, image_cache_receiver) = unbounded();

        let receivers = ScriptThreadReceivers {
            document_control_receiver,
            constellation_receiver,
            image_cache_receiver,
            devtools_server_receiver,
            // Initialized to `never` until WebGPU is initialized.
            #[cfg(feature = "webgpu")]
            webgpu_receiver: RefCell::new(crossbeam_channel::never()),
        };

        let opts = opts::get();
        let senders = ScriptThreadSenders {
            self_sender,
            #[cfg(feature = "bluetooth")]
            bluetooth_sender: state.bluetooth_sender,
            constellation_sender: state.constellation_to_script_sender,
            pipeline_to_constellation_sender: state.script_to_constellation_sender,
            pipeline_to_embedder_sender: state.script_to_embedder_sender.clone(),
            image_cache_sender,
            time_profiler_sender: state.time_profiler_sender,
            memory_profiler_sender: state.memory_profiler_sender,
            devtools_server_sender,
            devtools_client_to_script_thread_sender: ipc_devtools_sender,
        };

        let microtask_queue = runtime.microtask_queue.clone();
        #[cfg(feature = "webgpu")]
        let gpu_id_hub = Arc::new(IdentityHub::default());

        let debugger_global = DebuggerGlobalScope::new(
            PipelineId::new(),
            senders.devtools_server_sender.clone(),
            senders.devtools_client_to_script_thread_sender.clone(),
            senders.memory_profiler_sender.clone(),
            senders.time_profiler_sender.clone(),
            senders.pipeline_to_constellation_sender.clone(),
            senders.pipeline_to_embedder_sender.clone(),
            state.resource_threads.clone(),
            state.storage_threads.clone(),
            #[cfg(feature = "webgpu")]
            gpu_id_hub.clone(),
            &mut cx,
        );

        debugger_global.execute(&mut cx);

        let shared_style_locks = Default::default();
        let user_contents_for_manager_id =
            FxHashMap::from_iter(state.user_contents_for_manager_id.into_iter().map(
                |(user_content_manager_id, user_contents)| {
                    (
                        user_content_manager_id,
                        ScriptThreadUserContents::new(user_contents, &shared_style_locks),
                    )
                },
            ));

        (
            Rc::new_cyclic(|weak_script_thread| {
                runtime.set_script_thread(weak_script_thread.clone());
                Self {
                    event_loop_id: state.id,
                    documents: DomRefCell::new(DocumentCollection::default()),
                    last_render_opportunity_time: Default::default(),
                    window_proxies: Default::default(),
                    incomplete_loads: DomRefCell::new(vec![]),
                    incomplete_parser_contexts: IncompleteParserContexts(RefCell::new(vec![])),
                    senders,
                    receivers,
                    image_cache_factory,
                    resource_threads: state.resource_threads,
                    storage_threads: state.storage_threads,
                    task_queue,
                    background_hang_monitor,
                    closing,
                    timer_scheduler: RefCell::new(TimerScheduler::with_clock(
                        document_clock.clone(),
                    )),
                    document_clock,
                    document_control_profile,
                    document_execution_profile,
                    controlled_session_history_revision: Cell::new(0),
                    controlled_automation_navigation_capture: Cell::new(
                        SynchronousNavigationEmissionCapture::Inactive,
                    ),
                    document_execution_ledger,
                    document_producer_fence,
                    controlled_input,
                    document_control_state,
                    timer_control_terminal: Default::default(),
                    dom_mutation_epochs,
                    microtask_queue,
                    js_runtime: Rc::new(runtime),
                    closed_pipelines: DomRefCell::new(FxHashSet::default()),
                    mutation_observers: Default::default(),
                    system_font_service: Arc::new(state.system_font_service.to_proxy()),
                    #[cfg(feature = "webgl")]
                    webgl_chan: state.webgl_chan,
                    #[cfg(feature = "webxr")]
                    webxr_registry: state.webxr_registry,
                    docs_with_no_blocking_loads: Default::default(),
                    custom_element_reaction_stack: Rc::new(CustomElementReactionStack::new()),
                    paint_api: state.cross_process_paint_api,
                    profile_script_events: opts
                        .debug
                        .is_enabled(DiagnosticsLoggingOption::ProfileScriptEvents),
                    unminify_js: opts.unminify_js,
                    local_script_source: opts.local_script_source.clone(),
                    unminify_css: opts.unminify_css,
                    shared_style_locks,
                    user_contents_for_manager_id: RefCell::new(user_contents_for_manager_id),
                    player_context: state.player_context,
                    pipeline_to_node_ids: Default::default(),
                    is_user_interacting: Rc::new(Cell::new(false)),
                    #[cfg(feature = "webgpu")]
                    gpu_id_hub,
                    layout_factory,
                    scheduled_update_the_rendering: Default::default(),
                    scheduled_rendering_delivery_ready: Arc::new(AtomicBool::new(false)),
                    needs_rendering_update: Arc::new(AtomicBool::new(false)),
                    debugger_global: debugger_global.as_traced(),
                    debugger_paused: Cell::new(false),
                    controlled_debugger_unsupported: Cell::new(false),
                    privileged_urls: state.privileged_urls,
                    this: weak_script_thread.clone(),
                    devtools_state: Default::default(),
                }
            }),
            cx,
        )
    }

    /// Check if we are closing.
    fn can_continue_running_inner(&self) -> bool {
        if self.closing.load(Ordering::SeqCst) {
            return false;
        }
        true
    }

    /// We are closing, ensure no script can run and potentially hang.
    fn prepare_for_shutdown_inner(&self) {
        let docs = self.documents.borrow();
        for (_, document) in docs.iter() {
            document
                .owner_global()
                .task_manager()
                .cancel_all_tasks_and_ignore_future_tasks();
        }
    }

    /// Starts the script thread. After calling this method, the script thread will loop receiving
    /// messages on its port.
    pub(crate) fn start(&self, cx: &mut js::context::JSContext) {
        debug!("Starting script thread.");
        while self.handle_msgs(cx) {
            // Go on...
            debug!("Running script thread.");
        }
        debug!("Stopped script thread.");
    }

    /// Run the owner-controlled event loop without executing ordinary page input implicitly.
    fn start_controlled(&self, cx: &mut js::context::JSContext) {
        debug!("Starting controlled script thread.");
        loop {
            self.drain_ready_document_controls(None);

            if let Some(event) = self.take_controlled_lifecycle_input() {
                if !self.process_one_controlled_event(cx, event) {
                    break;
                }
                continue;
            }

            if let Some(command) = self.take_controlled_command() {
                if !self.handle_document_control(cx, command) {
                    break;
                }
                continue;
            }
            let ordinary = self.drain_ready_controlled_inputs();
            let controls = self.drain_ready_document_controls(None);

            if let Some(event) = self.take_controlled_lifecycle_input() {
                if !self.process_one_controlled_event(cx, event) {
                    break;
                }
                continue;
            }
            if self.has_controlled_command() || controls.saturated || ordinary.saturated {
                continue;
            }

            self.background_hang_monitor.notify_wait();
            debug!("Waiting for controlled event.");
            let fully_active = self.get_fully_active_document_ids();
            match self.receivers.recv_controlled(
                &self.task_queue,
                &self.timer_scheduler,
                &fully_active,
            ) {
                ControlledMessage::Control(message) => self.admit_controlled_command(message),
                ControlledMessage::DocumentControlClosed => {
                    // EventLoop::drop queues ExitScriptThread before its document-control sender
                    // is dropped. If Select observes the disconnect first, take the identical
                    // lifecycle path instead of panicking or spinning on an always-ready channel.
                    let continued = self.process_one_controlled_event(
                        cx,
                        MixedMessage::FromConstellation(ScriptThreadMessage::ExitScriptThread),
                    );
                    debug_assert!(!continued);
                    break;
                },
                ControlledMessage::Ordinary(event) => self.admit_controlled_input(event),
            }
        }
        debug!("Stopped controlled script thread.");
    }

    /// Process input events as part of a "update the rendering task".
    fn process_pending_input_events(
        &self,
        cx: &mut js::context::JSContext,
        pipeline_id: PipelineId,
    ) {
        let Some(document) = self.documents.borrow().find_document(pipeline_id) else {
            warn!("Processing pending input events for closed pipeline {pipeline_id}.");
            return;
        };
        // Do not handle events if the BC has been, or is being, discarded
        if document.window().Closed() {
            warn!("Input event sent to a pipeline with a closed window {pipeline_id}.");
            return;
        }
        if !document.event_handler().has_pending_input_events() {
            return;
        }

        let _guard = ScriptUserInteractingGuard::new(self.is_user_interacting.clone());
        document.event_handler().handle_pending_input_events(cx);
    }

    fn cancel_scheduled_update_the_rendering(&self) {
        if let Some(timer_id) = self.scheduled_update_the_rendering.borrow_mut().take() {
            self.timer_scheduler.borrow_mut().cancel_timer(timer_id);
        }
        self.scheduled_rendering_delivery_ready
            .store(false, Ordering::SeqCst);
    }

    fn schedule_update_the_rendering_timer_if_necessary(&self, delay: Duration) {
        if self.scheduled_update_the_rendering.borrow().is_some() {
            return;
        }

        debug!("Scheduling ScriptThread animation frame.");
        let trigger_script_thread_animation = self.needs_rendering_update.clone();
        let delivery_ready = self.scheduled_rendering_delivery_ready.clone();
        delivery_ready.store(false, Ordering::SeqCst);
        let timer_id = match try_schedule_rendering_update_timer(
            &mut self.timer_scheduler.borrow_mut(),
            &self.timer_control_terminal,
            trigger_script_thread_animation,
            delivery_ready,
            delay,
        ) {
            Ok(timer_id) => timer_id,
            Err(error) => {
                // In controlled mode the document clock retains the typed terminal for the
                // control plane. Rendering work must not turn that terminal into a panic.
                debug!(
                    "Not scheduling ScriptThread animation frame: {error}; first timer terminal: {:?}",
                    self.timer_control_terminal_error(),
                );
                return;
            },
        };

        *self.scheduled_update_the_rendering.borrow_mut() = Some(timer_id);
    }

    /// <https://html.spec.whatwg.org/multipage/#update-the-rendering>
    ///
    /// Attempt to update the rendering and then do a microtask checkpoint if rendering was
    /// actually updated.
    ///
    /// Returns true if any reflows produced a new display list.
    pub(crate) fn update_the_rendering(&self, cx: &mut js::context::JSContext) -> bool {
        if !self.begin_controlled_rendering_opportunity() {
            return false;
        }
        let frame_time = self
            .document_clock
            .rendering_time()
            .expect("update the rendering requires a supported document clock");
        self.last_render_opportunity_time
            .set(Some(frame_time.document_time()));
        self.cancel_scheduled_update_the_rendering();
        self.needs_rendering_update.store(false, Ordering::Relaxed);

        if !self.can_continue_running_inner() {
            return false;
        }

        // TODO(#31242): the filtering of docs is extended to not exclude the ones that
        // has pending initial observation targets
        // https://w3c.github.io/IntersectionObserver/#pending-initial-observation

        // > 2. Let docs be all fully active Document objects whose relevant agent's event loop
        // > is eventLoop, sorted arbitrarily except that the following conditions must be
        // > met:
        //
        // > Any Document B whose container document is A must be listed after A in the
        // > list.
        //
        // > If there are two documents A and B that both have the same non-null container
        // > document C, then the order of A and B in the list must match the
        // > shadow-including tree order of their respective navigable containers in C's
        // > node tree.
        //
        // > In the steps below that iterate over docs, each Document must be processed in
        // > the order it is found in the list.
        let documents_in_order = self.documents.borrow().documents_in_order();

        // TODO: The specification reads: "for doc in docs" at each step whereas this runs all
        // steps per doc in docs. Currently `<iframe>` resizing depends on a parent being able to
        // queue resize events on a child and have those run in the same call to this method, so
        // that needs to be sorted out to fix this.
        let mut painters_generating_frames = FxHashSet::default();
        for pipeline_id in documents_in_order.iter() {
            let Some(document) = self.documents.borrow().find_document(*pipeline_id) else {
                continue;
            };

            if !document.is_fully_active() {
                continue;
            }

            if document.waiting_on_canvas_image_updates() {
                continue;
            }

            // Step 3. Filter non-renderable documents:
            // Remove from docs any Document object doc for which any of the following are true:
            if
            // doc is render-blocked;
            document.is_render_blocked()
            // doc's visibility state is "hidden";
            // TODO: Currently, this would mean that the script thread does nothing, since
            // documents aren't currently correctly set to the visible state when navigating

            // doc's rendering is suppressed for view transitions; or
            // TODO

            // doc's node navigable doesn't currently have a rendering opportunity.
            //
            // This is implicitly the case when we call this method
            {
                continue;
            }

            // Clear this as early as possible so that any callbacks that
            // trigger new reasons for updating the rendering don't get lost.
            document.clear_rendering_update_reasons();

            // TODO(#31581): The steps in the "Revealing the document" section need to be implemented
            // `process_pending_input_events` handles the focusing steps as well as other events
            // from `Paint`.

            // TODO: Should this be broken and to match the specification more closely? For instance see
            // https://html.spec.whatwg.org/multipage/#flush-autofocus-candidates.
            self.process_pending_input_events(cx, *pipeline_id);

            // > 8. For each doc of docs, run the resize steps for doc. [CSSOMVIEW]
            let resized = document.window().run_the_resize_steps(cx);

            // > 9. For each doc of docs, run the scroll steps for doc.
            document.run_the_scroll_steps(cx);

            // > 10. For each doc of docs, evaluate media queries and report changes for doc.
            //
            // Resize is the most common cause, but media queries can also change because
            // of the platform theme (`prefers-color-scheme`) or other media features.
            // The window tracks those via `pending_media_query_evaluation`, so we only
            // pay the cost when something has actually changed.
            let media_features_changed = document.window().take_pending_media_query_evaluation();
            if resized || media_features_changed {
                document
                    .window()
                    .evaluate_media_queries_and_report_changes(cx);
            }
            if resized {
                // https://html.spec.whatwg.org/multipage/#img-environment-changes
                // As per the spec, this can be run at any time.
                document.react_to_environment_changes(cx);
            }

            let mut realm = enter_auto_realm(cx, &*document);
            let cx = &mut realm.current_realm();

            // > 11. For each doc of docs, update animations and send events for doc, passing
            // > in relative high resolution time given frameTimestamp and doc's relevant
            // > global object as the timestamp [WEBANIMATIONS]
            document.update_animations_and_send_events(cx, frame_time);

            // TODO(#31866): Implement "run the fullscreen steps" from
            // https://fullscreen.spec.whatwg.org/multipage/#run-the-fullscreen-steps.

            // TODO(#31868): Implement the "context lost steps" from
            // https://html.spec.whatwg.org/multipage/#context-lost-steps.

            // > 14. For each doc of docs, run the animation frame callbacks for doc, passing
            // > in the relative high resolution time given frameTimestamp and doc's
            // > relevant global object as the timestamp.
            document.run_the_animation_frame_callbacks(cx, frame_time);

            // Run the resize observer steps.
            let mut depth = Default::default();
            while document.gather_active_resize_observations_at_depth(cx.no_gc(), &depth) {
                // Note: this will reflow the doc.
                depth = document.broadcast_active_resize_observations(cx);
            }

            if document.has_skipped_resize_observations() {
                document.deliver_resize_loop_error_notification(cx);
                // Ensure that another turn of the event loop occurs to process
                // the skipped observations.
                document.add_rendering_update_reason(
                    RenderingUpdateReason::ResizeObserverStartedObservingTarget,
                );
            }

            // <https://html.spec.whatwg.org/multipage/#focus-fixup-rule>
            // > For each doc of docs, if the focused area of doc is not a focusable area, then run the
            // > focusing steps for doc's viewport, and set doc's relevant global object's navigation API's
            // > focus changed during ongoing navigation to false.
            document.focus_handler().perform_focus_fixup_rule(cx);

            // TODO: Perform pending transition operations from
            // https://drafts.csswg.org/css-view-transitions/#perform-pending-transition-operations.

            // > 19. For each doc of docs, run the update intersection observations steps for doc,
            // > passing in the relative high resolution time given now and
            // > doc's relevant global object as the timestamp. [INTERSECTIONOBSERVER]
            let intersection_observer_time =
                IntersectionObserverRenderingTime::for_rendering_update(
                    &self.document_clock,
                    frame_time,
                );
            document.update_intersection_observer_steps(cx, intersection_observer_time);

            // TODO: Mark paint timing from https://w3c.github.io/paint-timing.

            // See <https://github.com/whatwg/html/issues/12704>.
            // Unspecified, but necessary: Any of the previous callbacks may have put the
            // document into a render-blocked state. If that's the case, then abort the
            // rendering process now.
            if document.is_render_blocked() {
                continue;
            }

            // > Step 22: For each doc of docs, update the rendering or user interface of
            // > doc and its node navigable to reflect the current state.
            if document.update_the_rendering(cx).0.needs_frame() {
                painters_generating_frames.insert(document.webview_id().into());
            }

            // TODO: Process top layer removals according to
            // https://drafts.csswg.org/css-position-4/#process-top-layer-removals.
        }

        let should_generate_frame = !painters_generating_frames.is_empty();
        if should_generate_frame {
            self.paint_api
                .generate_frame(painters_generating_frames.into_iter().collect());
        }

        // Perform a microtask checkpoint as the specifications says that *update the rendering*
        // should be run in a task and a microtask checkpoint is always done when running tasks.
        self.perform_a_microtask_checkpoint(cx);
        should_generate_frame
    }

    /// Schedule a rendering update ("update the rendering"), if necessary. This
    /// can be necessary for a couple reasons. For instance, when the DOM
    /// changes a scheduled rendering update becomes necessary if one isn't
    /// scheduled already. Another example is if rAFs are running but no display
    /// lists are being produced. In that case the [`ScriptThread`] is
    /// responsible for scheduling animation ticks.
    fn maybe_schedule_rendering_opportunity_after_ipc_message(
        &self,
        no_gc: &NoGC,
        built_any_display_lists: bool,
    ) {
        let needs_rendering_update = self
            .documents
            .borrow()
            .iter()
            .any(|(_, document)| document.needs_rendering_update(no_gc));
        let running_animations = self.documents.borrow().iter().any(|(_, document)| {
            document.is_fully_active() &&
                !document.window().throttled() &&
                (document.animations().running_animation_count() != 0 ||
                    document.has_active_request_animation_frame_callbacks())
        });

        // If we are not running animations and no rendering update is
        // necessary, just exit early and schedule the next rendering update
        // when it becomes necessary.
        if !needs_rendering_update && !running_animations {
            return;
        }

        // If animations are running and a reflow in this event loop iteration
        // produced a display list, rely on the renderer to inform us of the
        // next animation tick / rendering opportunity.
        if renderer_may_drive_rendering(&self.document_clock) &&
            running_animations &&
            built_any_display_lists
        {
            return;
        }

        // There are two possibilities: rendering needs to be updated or we are
        // scheduling a new animation tick because animations are running, but
        // not changing the DOM. In the later case we can wait a bit longer
        // until the next "update the rendering" call as it's more efficient to
        // slow down rAFs that don't change the DOM.
        //
        // TODO: Should either of these delays be reduced to also reduce update latency?
        let animation_delay = if running_animations && !needs_rendering_update {
            // 30 milliseconds (33 FPS) is used here as the rendering isn't changing
            // so it isn't a problem to slow down rAF callback calls. In addition, this allows
            // renderer-based ticks to arrive first.
            Duration::from_millis(30)
        } else {
            // 20 milliseconds (50 FPS) is used here in order to allow any renderer-based
            // animation ticks to arrive first.
            Duration::from_millis(20)
        };

        let now = self
            .document_clock
            .now_for_surface(DocumentTimeSurface::UpdateRendering)
            .expect("rendering opportunity scheduling requires a supported document clock");
        let remaining_delay = remaining_rendering_opportunity_delay(
            self.last_render_opportunity_time.get(),
            now,
            animation_delay,
        )
        .expect("the document clock cannot move backwards between rendering opportunities");
        self.schedule_update_the_rendering_timer_if_necessary(remaining_delay);
    }

    /// Fulfill the possibly-pending pending `document.fonts.ready` promise if
    /// all web fonts have loaded.
    fn maybe_fulfill_font_ready_promises(&self, cx: &mut js::context::JSContext) {
        let mut sent_message = false;
        for (_, document) in self.documents.borrow().iter() {
            sent_message = document.maybe_fulfill_font_ready_promise(cx) || sent_message;
        }

        if sent_message {
            self.perform_a_microtask_checkpoint(cx);
        }
    }

    /// If any `Pipeline`s are waiting to become ready for the purpose of taking a
    /// screenshot, check to see if the `Pipeline` is now ready and send a message to the
    /// Constellation, if so.
    fn maybe_resolve_pending_screenshot_readiness_requests(&self, cx: &mut js::context::JSContext) {
        for (_, document) in self.documents.borrow().iter() {
            document
                .window()
                .maybe_resolve_pending_screenshot_readiness_requests(cx);
        }
    }

    fn document_execution_is_terminal(&self) -> bool {
        self.document_execution_ledger
            .as_ref()
            .is_some_and(|ledger| ledger.observation().terminal.is_some())
    }

    fn begin_controlled_ordinary_task(&self) -> bool {
        self.document_execution_ledger
            .as_ref()
            .is_none_or(|ledger| ledger.begin_ordinary_task().is_ok())
    }

    fn begin_controlled_rendering_opportunity(&self) -> bool {
        self.document_execution_ledger
            .as_ref()
            .is_none_or(|ledger| ledger.begin_rendering_opportunity().is_ok())
    }

    /// Admit one selected event before removing it from the controlled queue.
    ///
    /// A previously latched terminal stops even checkpoint-only turns. Pump markers remain free,
    /// but only while the shared session ledger is active.
    fn admit_controlled_event(&self, event: Option<&MixedMessage>) -> bool {
        if self.document_execution_is_terminal() {
            return false;
        }
        event
            .filter(|event| controlled_event_consumes_ordinary_task_budget(event))
            .is_none_or(|_| self.begin_controlled_ordinary_task())
    }

    /// Admit a bounded ready-input batch without executing page work.
    fn drain_ready_controlled_inputs(&self) -> ControlledInputBatch {
        let Some(controlled_input) = &self.controlled_input else {
            return ControlledInputBatch::default();
        };
        let fully_active = self.get_fully_active_document_ids();
        let mut input = std::iter::from_fn(|| {
            self.receivers
                .try_recv_controlled(&self.task_queue, &fully_active)
        });
        let batch = controlled_input.borrow_mut().drain_bounded(&mut input);
        batch
    }

    fn drain_ready_document_controls(
        &self,
        active: Option<(DocumentControlRequestId, DocumentControlCancellationId)>,
    ) -> ControlledControlBatch {
        let Some(controlled_input) = &self.controlled_input else {
            return ControlledControlBatch::default();
        };
        let mut input = std::iter::from_fn(|| self.receivers.try_recv_document_control());
        controlled_input
            .borrow_mut()
            .drain_controls_bounded(&mut input, active)
    }

    fn admit_controlled_command(&self, message: ScriptThreadControlMessage) {
        self.controlled_input
            .as_ref()
            .expect("the controlled loop requires an owner input queue")
            .borrow_mut()
            .admit_control(message);
    }

    fn admit_controlled_input(&self, event: MixedMessage) {
        self.controlled_input
            .as_ref()
            .expect("the controlled loop requires an owner input queue")
            .borrow_mut()
            .admit(event);
    }

    fn take_controlled_lifecycle_input(&self) -> Option<MixedMessage> {
        let mut input = self
            .controlled_input
            .as_ref()
            .expect("the controlled loop requires an owner input queue")
            .borrow_mut();
        take_controlled_lifecycle_event(&mut input.ready)
    }

    fn take_controlled_command(&self) -> Option<ScriptThreadControlMessage> {
        self.controlled_input
            .as_ref()
            .expect("the controlled loop requires an owner input queue")
            .borrow_mut()
            .take_control()
    }

    fn has_controlled_command(&self) -> bool {
        !self
            .controlled_input
            .as_ref()
            .expect("the controlled loop requires an owner input queue")
            .borrow()
            .controls
            .is_empty()
    }

    fn drain_active_controls_until_quiet(
        &self,
        active: (DocumentControlRequestId, DocumentControlCancellationId),
    ) -> bool {
        loop {
            let batch = self.drain_ready_document_controls(Some(active));
            if batch.active_cancelled {
                return true;
            }
            if !batch.saturated {
                return false;
            }
        }
    }

    fn handle_document_control(
        &self,
        cx: &mut js::context::JSContext,
        message: ScriptThreadControlMessage,
    ) -> bool {
        let ScriptThreadControlMessage::Command {
            request_id,
            cancellation_id,
            target,
            target_terminals,
            command,
        } = message
        else {
            return true;
        };
        let active = (request_id, cancellation_id);

        let retained_token = {
            let state = self
                .document_control_state
                .as_ref()
                .expect("the controlled loop requires document-control authority");
            let mut state = state.borrow_mut();
            state.issued_token.take()
        };

        if matches!(
            &command,
            DocumentControlCommand::DriveOneTurn |
                DocumentControlCommand::BootstrapInitialPipeline { .. } |
                DocumentControlCommand::BootstrapReplacementPipeline { .. }
        ) {
            self.task_queue.start_event_loop_iteration();
        }
        self.drain_ready_controlled_inputs();
        if self.drain_active_controls_until_quiet(active) {
            return true;
        }

        if let Err(error) = self.validate_controlled_route(&target) {
            let outcome = match &command {
                DocumentControlCommand::DriveOneTurn |
                DocumentControlCommand::BootstrapInitialPipeline { .. } |
                DocumentControlCommand::BootstrapReplacementPipeline { .. }
                    if is_pending_capture_error(&error) =>
                {
                    DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate {
                        target: target.clone(),
                    }
                },
                DocumentControlCommand::AdvanceTo(token) if is_pending_capture_error(&error) => {
                    DocumentControlOutcome::AdvanceOutcomeIndeterminate {
                        token_id: token.id(),
                        target: target.clone(),
                        deadline: token.deadline(),
                    }
                },
                DocumentControlCommand::Automate(request)
                    if DocumentControlAutomationKind::from_request(request).is_mutating() &&
                        is_pending_capture_error(&error) =>
                {
                    DocumentControlOutcome::AutomationOutcomeIndeterminate {
                        target: target.clone(),
                        operation: DocumentControlAutomationKind::from_request(request),
                    }
                },
                _ => DocumentControlOutcome::Rejected(error),
            };
            self.send_document_control_response(request_id, cancellation_id, target, outcome);
            return true;
        }

        match command {
            DocumentControlCommand::Observe => {
                let bootstrap_pipeline = self.initial_pipeline_bootstrap_event(&target);
                let outcome = if let Some(pipeline_id) = bootstrap_pipeline {
                    DocumentControlOutcome::Rejected(
                        DocumentControlError::InitialPipelineBootstrapRequired { pipeline_id },
                    )
                } else if let Some((source_pipeline_id, pipeline_id)) =
                    self.pending_replacement_pipeline_bootstrap_event(&target)
                {
                    DocumentControlOutcome::Rejected(
                        DocumentControlError::ReplacementPipelineBootstrapRequired {
                            source_pipeline_id,
                            pipeline_id,
                        },
                    )
                } else {
                    self.capture_controlled_pending(
                        &target,
                        target_terminals,
                        ProducerCapture::Passive,
                        &cx.no_gc(),
                    )
                    .and_then(|pending| {
                        self.completed_control_observation(DocumentControlAction::Observed, pending)
                    })
                    .map(Box::new)
                    .map(DocumentControlOutcome::Completed)
                    .unwrap_or_else(DocumentControlOutcome::Rejected)
                };
                if !self.drain_active_controls_until_quiet(active) {
                    self.send_document_control_response(
                        request_id,
                        cancellation_id,
                        target,
                        outcome,
                    );
                }
                true
            },
            bootstrap @ (DocumentControlCommand::BootstrapInitialPipeline { .. } |
            DocumentControlCommand::BootstrapReplacementPipeline { .. }) => {
                let (_pipeline_id, event_index, qualified, unavailable) = match bootstrap {
                    DocumentControlCommand::BootstrapInitialPipeline { pipeline_id } => (
                        pipeline_id,
                        0,
                        self.initial_pipeline_bootstrap_event(&target) == Some(pipeline_id),
                        DocumentControlError::InitialPipelineBootstrapUnavailable { pipeline_id },
                    ),
                    DocumentControlCommand::BootstrapReplacementPipeline {
                        source_pipeline_id,
                        pipeline_id,
                    } => {
                        let event_index = match self.await_replacement_pipeline_bootstrap_event(
                            active,
                            &target,
                            source_pipeline_id,
                            pipeline_id,
                        ) {
                            ReplacementPipelineBootstrapWaitOutcome::Ready { event_index } => {
                                event_index
                            },
                            ReplacementPipelineBootstrapWaitOutcome::Cancelled => return true,
                            ReplacementPipelineBootstrapWaitOutcome::Rejected(error) => {
                                let outcome = DocumentControlOutcome::Rejected(error);
                                if !self.drain_active_controls_until_quiet(active) {
                                    self.send_document_control_response(
                                        request_id,
                                        cancellation_id,
                                        target,
                                        outcome,
                                    );
                                }
                                return true;
                            },
                            ReplacementPipelineBootstrapWaitOutcome::Failed => return false,
                        };
                        (
                            pipeline_id,
                            event_index,
                            true,
                            DocumentControlError::ReplacementPipelineBootstrapUnavailable {
                                source_pipeline_id,
                                pipeline_id,
                            },
                        )
                    },
                    _ => unreachable!("the bootstrap command was matched above"),
                };
                if !qualified {
                    let outcome = DocumentControlOutcome::Rejected(unavailable);
                    if !self.drain_active_controls_until_quiet(active) {
                        self.send_document_control_response(
                            request_id,
                            cancellation_id,
                            target,
                            outcome,
                        );
                    }
                    return true;
                }

                // This is the final cancellation intake before the bootstrap mutation. The ready
                // queue is not touched between this barrier and removal, so an owner-observed
                // cancellation leaves the selected SpawnPipeline and every predecessor intact.
                if self.drain_active_controls_until_quiet(active) {
                    return true;
                }
                let bootstrap_admitted = {
                    let input = self
                        .controlled_input
                        .as_ref()
                        .expect("the controlled loop requires an owner input queue")
                        .borrow();
                    self.admit_controlled_event(input.ready.get(event_index))
                };
                if !bootstrap_admitted {
                    let outcome = self.controlled_drive_completion(
                        &target,
                        target_terminals,
                        DocumentControlAction::ExecutionTerminated,
                        ProducerCapture::Passive,
                        &cx.no_gc(),
                    );
                    if !self.drain_active_controls_until_quiet(active) {
                        self.send_document_control_response(
                            request_id,
                            cancellation_id,
                            target,
                            outcome,
                        );
                    }
                    return true;
                }

                let before_checkpoint = self.microtask_queue.completed_checkpoint_generation();
                let event = self
                    .controlled_input
                    .as_ref()
                    .expect("the controlled loop requires an owner input queue")
                    .borrow_mut()
                    .ready
                    .remove(event_index)
                    .expect("validated pipeline bootstrap event must remain queued");
                if !self.process_one_controlled_event(cx, event) {
                    return false;
                }
                let checkpoint_advanced =
                    self.microtask_queue.completed_checkpoint_generation() > before_checkpoint;
                let action = DocumentControlAction::TurnProcessed {
                    microtask_checkpoint_advanced: checkpoint_advanced,
                };

                self.drain_ready_controlled_inputs();
                if self.drain_active_controls_until_quiet(active) {
                    return true;
                }
                let capture = if checkpoint_advanced {
                    ProducerCapture::FreshCheckpoint
                } else {
                    ProducerCapture::Passive
                };
                let outcome = match self
                    .validate_controlled_target(&target)
                    .and_then(|()| {
                        self.capture_controlled_pending(
                            &target,
                            target_terminals,
                            capture,
                            &cx.no_gc(),
                        )
                    })
                    .and_then(|pending| self.completed_control_observation(action, pending))
                {
                    Ok(observation) => DocumentControlOutcome::Completed(Box::new(observation)),
                    Err(_) => DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate {
                        target: target.clone(),
                    },
                };
                if !self.drain_active_controls_until_quiet(active) {
                    self.send_document_control_response(
                        request_id,
                        cancellation_id,
                        target,
                        outcome,
                    );
                }
                true
            },
            DocumentControlCommand::DriveOneTurn => {
                let (target_validation, initial_activation_pipeline) = {
                    let input = self
                        .controlled_input
                        .as_ref()
                        .expect("the controlled loop requires an owner input queue")
                        .borrow();
                    match controlled_drive_event_disposition(&input.ready) {
                        ControlledDriveEventDisposition::PipelineBootstrapRequired => (
                            Err(DocumentControlError::PendingFactUnavailable(
                                DocumentPendingFact::TargetMembership,
                            )),
                            None,
                        ),
                        ControlledDriveEventDisposition::Ready(Some(event)) => {
                            let validation =
                                self.validate_controlled_target_for_event(&target, event);
                            let activation = validation
                                .is_ok()
                                .then(|| self.initial_pipeline_activation_event(&target, event))
                                .flatten();
                            (validation, activation)
                        },
                        ControlledDriveEventDisposition::Ready(None) => {
                            (self.validate_controlled_target(&target), None)
                        },
                    }
                };
                if target_validation.is_err() {
                    let outcome = DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate {
                        target: target.clone(),
                    };
                    if !self.drain_active_controls_until_quiet(active) {
                        self.send_document_control_response(
                            request_id,
                            cancellation_id,
                            target,
                            outcome,
                        );
                    }
                    return true;
                }
                let event_admitted = {
                    let input = self
                        .controlled_input
                        .as_ref()
                        .expect("the controlled loop requires an owner input queue")
                        .borrow();
                    self.admit_controlled_event(next_controlled_turn_event(&input.ready))
                };
                if !event_admitted {
                    let outcome = self.controlled_drive_completion(
                        &target,
                        target_terminals,
                        DocumentControlAction::ExecutionTerminated,
                        ProducerCapture::Passive,
                        &cx.no_gc(),
                    );
                    if !self.drain_active_controls_until_quiet(active) {
                        self.send_document_control_response(
                            request_id,
                            cancellation_id,
                            target,
                            outcome,
                        );
                    }
                    return true;
                }
                if let Some(pipeline_id) = initial_activation_pipeline {
                    let state = self
                        .document_control_state
                        .as_ref()
                        .expect("the controlled loop requires document-control authority");
                    let mut state = state.borrow_mut();
                    if state.initial_pipeline_activation.is_some() {
                        return false;
                    }
                    state.initial_pipeline_activation = Some(InitialPipelineActivationMarker {
                        pipeline_id,
                        correlation: InitialPipelineActivationCorrelation::new_for_drive_one_turn(
                            request_id,
                            cancellation_id,
                        ),
                    });
                }
                let before_checkpoint = self.microtask_queue.completed_checkpoint_generation();
                let (event, checkpoint_only) = {
                    let mut input = self
                        .controlled_input
                        .as_ref()
                        .expect("the controlled loop requires an owner input queue")
                        .borrow_mut();
                    take_controlled_lifecycle_event(&mut input.ready)
                        .map(|event| (event, false))
                        .unwrap_or_else(|| take_controlled_turn(&mut input.ready))
                };
                if !self.process_one_controlled_event(cx, event) {
                    return false;
                }
                let initial_activation_emitted = initial_activation_pipeline.is_some_and(|_| {
                    self.document_control_state
                        .as_ref()
                        .expect("the controlled loop requires document-control authority")
                        .borrow_mut()
                        .initial_pipeline_activation
                        .take()
                        .is_none()
                });
                let after_checkpoint = self.microtask_queue.completed_checkpoint_generation();
                let checkpoint_advanced = after_checkpoint > before_checkpoint;
                let action = if checkpoint_only {
                    DocumentControlAction::CheckpointTurnProcessed {
                        microtask_checkpoint_advanced: checkpoint_advanced,
                    }
                } else {
                    DocumentControlAction::TurnProcessed {
                        microtask_checkpoint_advanced: checkpoint_advanced,
                    }
                };

                let (target, target_terminals) = if initial_activation_emitted {
                    let pipeline_id = initial_activation_pipeline
                        .expect("an emitted initial activation was prequalified");
                    match self.await_initial_pipeline_activation_authority(
                        cx,
                        active,
                        pipeline_id,
                        &target,
                        &target_terminals,
                    ) {
                        InitialPipelineActivationAuthority::Authorized {
                            target,
                            target_terminals,
                        } => (target, target_terminals),
                        InitialPipelineActivationAuthority::Cancelled => return true,
                        InitialPipelineActivationAuthority::Failed => return false,
                    }
                } else {
                    (target, target_terminals)
                };

                self.drain_ready_controlled_inputs();
                if self.drain_active_controls_until_quiet(active) {
                    return true;
                }
                let capture = if checkpoint_advanced {
                    ProducerCapture::FreshCheckpoint
                } else {
                    ProducerCapture::Passive
                };
                let outcome = match self
                    .validate_controlled_target(&target)
                    .and_then(|()| {
                        self.capture_controlled_pending(
                            &target,
                            target_terminals,
                            capture,
                            &cx.no_gc(),
                        )
                    })
                    .and_then(|pending| self.completed_control_observation(action, pending))
                {
                    Ok(observation) => DocumentControlOutcome::Completed(Box::new(observation)),
                    Err(_) => DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate {
                        target: target.clone(),
                    },
                };
                if !self.drain_active_controls_until_quiet(active) {
                    self.send_document_control_response(
                        request_id,
                        cancellation_id,
                        target,
                        outcome,
                    );
                }
                true
            },
            DocumentControlCommand::Automate(request) => {
                self.handle_controlled_automation(
                    cx,
                    request_id,
                    cancellation_id,
                    target,
                    target_terminals,
                    *request,
                );
                true
            },
            DocumentControlCommand::AdvanceTo(supplied_token) => {
                self.handle_controlled_advance(
                    cx,
                    request_id,
                    cancellation_id,
                    target,
                    target_terminals,
                    *supplied_token,
                    retained_token,
                );
                true
            },
        }
    }

    fn handle_controlled_automation(
        &self,
        cx: &mut js::context::JSContext,
        request_id: DocumentControlRequestId,
        cancellation_id: DocumentControlCancellationId,
        target: Box<PendingTargetObservation>,
        target_terminals: PendingRuntimeTerminals,
        request: DocumentAutomationRequest,
    ) {
        let active = (request_id, cancellation_id);
        let operation = DocumentControlAutomationKind::from_request(&request);
        let reject = |error| {
            self.send_document_control_response(
                request_id,
                cancellation_id,
                target.clone(),
                DocumentControlOutcome::Rejected(error),
            );
        };
        let indeterminate = || {
            self.send_document_control_response(
                request_id,
                cancellation_id,
                target.clone(),
                DocumentControlOutcome::AutomationOutcomeIndeterminate {
                    target: target.clone(),
                    operation,
                },
            );
        };

        // Bind the request to one complete owner snapshot immediately before rooting and touching
        // the document. Any failure through this point proves that no automation mutation began.
        let pending = match self.validate_controlled_target(&target).and_then(|()| {
            self.capture_controlled_pending(
                &target,
                target_terminals.clone(),
                ProducerCapture::Passive,
                &cx.no_gc(),
            )
        }) {
            Ok(pending) => pending,
            Err(error) => {
                if operation.is_mutating() && is_pending_capture_error(&error) {
                    indeterminate();
                } else {
                    reject(error);
                }
                return;
            },
        };
        if let Err(error) = request.validate_for_execution(&target, pending.state_generation) {
            reject(DocumentControlError::Automation(error));
            return;
        }
        if operation.is_mutating() &&
            pending
                .execution
                .is_some_and(|execution| execution.terminal.is_some())
        {
            reject(DocumentControlError::Automation(
                DocumentAutomationError::ExecutionTerminated,
            ));
            return;
        }
        if self.drain_active_controls_until_quiet(active) {
            return;
        }

        let Some(active_pipeline) = target.active_top_level else {
            reject(DocumentControlError::Automation(
                DocumentAutomationError::TargetChanged,
            ));
            return;
        };
        let Some(document) = self
            .documents
            .borrow()
            .find_document(active_pipeline.pipeline_id)
        else {
            reject(DocumentControlError::Automation(
                DocumentAutomationError::TargetChanged,
            ));
            return;
        };
        if document.webview_id() != target.webview_id || !document.window().is_top_level() {
            reject(DocumentControlError::Automation(
                DocumentAutomationError::TargetChanged,
            ));
            return;
        }

        let synchronous_automation_event_time = if operation.is_mutating() &&
            self.document_control_profile == DocumentControlProfile::TopLevelSession &&
            self.document_execution_profile == DocumentExecutionProfile::ControlledWebSessionV2
        {
            match document
                .window()
                .sample_controlled_v2_document_performance_time()
            {
                Ok(sampled) => Some(sampled),
                Err(error) => {
                    reject(DocumentControlError::Clock(error));
                    return;
                },
            }
        } else {
            None
        };

        let capture_synchronous_navigation = operation.is_mutating() &&
            self.document_control_profile == DocumentControlProfile::TopLevelSession;
        if capture_synchronous_navigation &&
            self.begin_synchronous_navigation_emission_capture(
                target.webview_id,
                active_pipeline.pipeline_id,
            )
            .is_err()
        {
            indeterminate();
            return;
        }

        let execution = {
            let _event_time_scope = synchronous_automation_event_time.map(|sampled| {
                document
                    .window()
                    .begin_synchronous_automation_event_time(sampled)
            });
            let mut realm = enter_auto_realm(cx, &*document);
            let cx = &mut realm.current_realm();
            execute_prevalidated_document_automation(cx, &document, &request)
        };
        let synchronous_navigation_emitted = if capture_synchronous_navigation {
            match self.finish_synchronous_navigation_emission_capture(
                target.webview_id,
                active_pipeline.pipeline_id,
            ) {
                Ok(emitted) => emitted,
                Err(()) => {
                    indeterminate();
                    return;
                },
            }
        } else {
            false
        };
        let result = match execution {
            Ok(result) => result,
            Err(error) => {
                if synchronous_navigation_emitted ||
                    automation_error_may_follow_mutation(&request, &error)
                {
                    indeterminate();
                } else {
                    reject(DocumentControlError::Automation(error));
                }
                return;
            },
        };

        // Synchronous activation or input handlers can enqueue ordinary work. Own that input before
        // the post-action snapshot without executing it; settlement will decide which turn to run.
        self.drain_ready_controlled_inputs();
        if self.drain_active_controls_until_quiet(active) {
            return;
        }
        let observation = self
            .validate_controlled_target(&target)
            .and_then(|()| {
                self.capture_controlled_pending(
                    &target,
                    target_terminals,
                    ProducerCapture::Passive,
                    &cx.no_gc(),
                )
            })
            .and_then(|pending| {
                self.completed_control_observation(
                    DocumentControlAction::Automated(operation),
                    pending,
                )
            });
        let outcome = match observation {
            Ok(observation) => DocumentControlOutcome::AutomationCompleted {
                result,
                observation: Box::new(observation),
                synchronous_navigation_emitted,
            },
            Err(error) if operation.is_mutating() => {
                let _ = error;
                DocumentControlOutcome::AutomationOutcomeIndeterminate {
                    target: target.clone(),
                    operation,
                }
            },
            Err(error) => DocumentControlOutcome::Rejected(error),
        };
        if !self.drain_active_controls_until_quiet(active) {
            self.send_document_control_response(request_id, cancellation_id, target, outcome);
        }
    }

    fn controlled_drive_completion(
        &self,
        target: &PendingTargetObservation,
        target_terminals: PendingRuntimeTerminals,
        action: DocumentControlAction,
        producer_capture: ProducerCapture,
        no_gc: &NoGC,
    ) -> DocumentControlOutcome {
        match self
            .validate_controlled_target(target)
            .and_then(|()| {
                self.capture_controlled_pending(target, target_terminals, producer_capture, no_gc)
            })
            .and_then(|pending| self.completed_control_observation(action, pending))
        {
            Ok(observation) => DocumentControlOutcome::Completed(Box::new(observation)),
            Err(_) => DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate {
                target: Box::new(target.clone()),
            },
        }
    }

    fn completed_control_observation(
        &self,
        action: DocumentControlAction,
        pending: RawPendingSnapshot,
    ) -> Result<DocumentControlObservation, DocumentControlError> {
        let advance_token = self.issue_advance_token(&pending)?;
        let observation =
            DocumentControlObservation::new_internal(action, Box::new(pending), advance_token)
                .map_err(|error| match error {
                    DocumentControlObservationInvariantError::PendingSnapshot(error) => {
                        DocumentControlError::PendingSnapshot(error)
                    },
                    DocumentControlObservationInvariantError::ExecutionTerminationMissing => {
                        DocumentControlError::PendingFactUnavailable(
                            DocumentPendingFact::RuntimeTerminals,
                        )
                    },
                    DocumentControlObservationInvariantError::AdvanceToken(error) => {
                        DocumentControlError::AdvancePrecondition(error)
                    },
                });
        if observation.is_err() {
            self.document_control_state
                .as_ref()
                .expect("the controlled loop requires document-control authority")
                .borrow_mut()
                .issued_token = None;
        }
        observation
    }

    fn issue_advance_token(
        &self,
        pending: &RawPendingSnapshot,
    ) -> Result<Option<DocumentAdvanceToken>, DocumentControlError> {
        if DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(1), pending).is_err() {
            return Ok(None);
        }
        let mut state = self
            .document_control_state
            .as_ref()
            .expect("the controlled loop requires document-control authority")
            .borrow_mut();
        let sequence = state
            .token_sequence
            .checked_add(1)
            .ok_or(DocumentControlError::AdvanceTokenSequenceOverflow)?;
        let token =
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(sequence), pending)
                .map_err(DocumentControlError::AdvancePrecondition)?;
        state.token_sequence = sequence;
        state.issued_token = Some(token.clone());
        Ok(Some(token))
    }

    /// Copy every Window-owned logical-timer queue before borrowing the outer scheduler.
    /// Observation can latch a clock terminal and cancel the queue's outer wake, so holding the
    /// scheduler borrow across this step would make terminal handling re-enter its RefCell.
    fn observe_controlled_logical_timers(
        &self,
        target: &PendingTargetObservation,
    ) -> Result<Vec<ControlledLogicalTimerOwnerObservation>, DocumentControlError> {
        let unavailable =
            || DocumentControlError::PendingFactUnavailable(DocumentPendingFact::LogicalTimers);
        let mut owners = Vec::new();
        for (pipeline_id, document) in self.documents.borrow().iter() {
            if !target.contains_pipeline(pipeline_id) {
                return Err(unavailable());
            }
            let observation = document
                .window()
                .upcast::<GlobalScope>()
                .pending_timer_observation();
            owners.push(match observation {
                Ok(observation) => ControlledLogicalTimerOwnerObservation::Active {
                    pipeline_id,
                    observation,
                },
                Err(error) => ControlledLogicalTimerOwnerObservation::Terminal(
                    PendingLogicalTimerTerminalObservation { pipeline_id, error },
                ),
            });
        }
        Ok(owners)
    }

    /// Join every copied logical timer to the same exact scheduler snapshot used by the pending
    /// barrier. A detached callback which has not confirmed delivery is neither a deadline nor
    /// ready work and therefore fails closed.
    fn capture_controlled_logical_timers(
        owners: Vec<ControlledLogicalTimerOwnerObservation>,
        scheduler: &TimerScheduler,
    ) -> Result<
        (
            Vec<PendingLogicalTimerFacts>,
            Vec<PendingLogicalTimerTerminalObservation>,
        ),
        DocumentControlError,
    > {
        let unavailable =
            || DocumentControlError::PendingFactUnavailable(DocumentPendingFact::LogicalTimers);
        let mut facts = Vec::new();
        let mut terminals = Vec::new();
        for owner in owners {
            let (pipeline_id, observation) = match owner {
                ControlledLogicalTimerOwnerObservation::Active {
                    pipeline_id,
                    observation,
                } => (pipeline_id, observation),
                ControlledLogicalTimerOwnerObservation::Terminal(terminal) => {
                    terminals.push(terminal);
                    continue;
                },
            };
            let selected = observation.selected;
            let outer = match (selected, observation.outer_wake) {
                (None, DomTimerOuterWakeObservation::Unbound) => None,
                (Some(_), DomTimerOuterWakeObservation::DeliveryReady) => Some((true, None)),
                (Some(_), DomTimerOuterWakeObservation::Scheduled(timer_id)) => {
                    let joined = scheduler
                        .join_live_deadlines(scheduler.id(), &[timer_id])
                        .map_err(|_| unavailable())?;
                    let joined = joined.first().copied().ok_or_else(unavailable)?;
                    let deadline = joined.deadline.ok_or_else(unavailable)?;
                    Some((
                        false,
                        Some(TimerDeadlineSnapshot {
                            scheduler_id: joined.scheduler_id,
                            id: joined.id,
                            deadline,
                        }),
                    ))
                },
                (
                    Some(_),
                    DomTimerOuterWakeObservation::DeliveryHandoffInProgress |
                    DomTimerOuterWakeObservation::Unbound,
                ) |
                (
                    None,
                    DomTimerOuterWakeObservation::Scheduled(_) |
                    DomTimerOuterWakeObservation::DeliveryHandoffInProgress |
                    DomTimerOuterWakeObservation::DeliveryReady,
                ) => return Err(unavailable()),
            };

            for timer in observation.timers {
                let is_ordering_head = selected.is_some_and(|selected| {
                    selected.handle == timer.handle &&
                        selected.creation_sequence == timer.creation_sequence
                });
                let (delivery_ready, outer_wake) = if is_ordering_head {
                    outer.ok_or_else(unavailable)?
                } else {
                    (false, None)
                };
                let (stable_id, kind) = match timer.kind {
                    DomTimerKind::JsOneShot => (
                        PendingLogicalTimerStableId::JavaScriptHandle(
                            timer.javascript_handle.ok_or_else(unavailable)?,
                        ),
                        PendingLogicalTimerKind::JavaScriptOneShot,
                    ),
                    DomTimerKind::JsInterval { requested_period } => (
                        PendingLogicalTimerStableId::JavaScriptHandle(
                            timer.javascript_handle.ok_or_else(unavailable)?,
                        ),
                        PendingLogicalTimerKind::JavaScriptInterval { requested_period },
                    ),
                    DomTimerKind::XhrTimeout => (
                        PendingLogicalTimerStableId::EngineHandle(timer.handle.sequence()),
                        PendingLogicalTimerKind::XmlHttpRequestTimeout,
                    ),
                    DomTimerKind::EventSourceReconnect => (
                        PendingLogicalTimerStableId::EngineHandle(timer.handle.sequence()),
                        PendingLogicalTimerKind::EventSourceReconnect,
                    ),
                    DomTimerKind::RefreshRedirect => (
                        PendingLogicalTimerStableId::EngineHandle(timer.handle.sequence()),
                        PendingLogicalTimerKind::RefreshRedirect,
                    ),
                    DomTimerKind::RunStepsAfterTimeout => (
                        PendingLogicalTimerStableId::EngineHandle(timer.handle.sequence()),
                        PendingLogicalTimerKind::RunStepsAfterTimeout,
                    ),
                    #[cfg(feature = "testbinding")]
                    DomTimerKind::TestBindingCallback => (
                        PendingLogicalTimerStableId::EngineHandle(timer.handle.sequence()),
                        PendingLogicalTimerKind::TestBindingCallback,
                    ),
                };
                facts.push(PendingLogicalTimerFacts {
                    identity: PendingLogicalTimerIdentity {
                        pipeline_id,
                        stable_id,
                    },
                    creation_sequence: timer.creation_sequence,
                    kind,
                    logical_deadline: timer.deadline,
                    suspended: timer.suspended,
                    eligible_in_controlled_turn: timer.eligible_in_controlled_turn,
                    is_ordering_head,
                    delivery_ready,
                    outer_wake,
                });
            }
            if selected.is_some() &&
                !facts.iter().any(|timer| {
                    timer.identity.pipeline_id == pipeline_id && timer.is_ordering_head
                })
            {
                return Err(unavailable());
            }
        }
        Ok((facts, terminals))
    }

    /// Capture passive rendering owners and join every retained callback identity against the
    /// same controlled scheduler snapshot used by the pending barrier.
    fn capture_controlled_rendering(
        &self,
        target: &PendingTargetObservation,
        scheduler: &TimerScheduler,
        no_gc: &NoGC,
    ) -> Result<
        (
            PendingRenderingObservation,
            Vec<PendingImageTimerTerminalObservation>,
            usize,
        ),
        DocumentControlError,
    > {
        let unavailable =
            || DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Rendering);
        let opportunity_ready = self.needs_rendering_update.load(Ordering::Relaxed);
        let delivery_ready = self
            .scheduled_rendering_delivery_ready
            .load(Ordering::SeqCst);
        let scheduled_id = *self.scheduled_update_the_rendering.borrow();
        if delivery_ready && (!opportunity_ready || scheduled_id.is_none()) {
            return Err(unavailable());
        }
        let scheduled_opportunity = match scheduled_id {
            None => None,
            Some(timer_id) => {
                let joined = scheduler
                    .join_live_deadlines(scheduler.id(), &[timer_id])
                    .map_err(|_| unavailable())?;
                let joined = joined.first().copied().ok_or_else(unavailable)?;
                match joined.deadline {
                    Some(_) if delivery_ready => return Err(unavailable()),
                    Some(deadline) => Some(TimerDeadlineSnapshot {
                        scheduler_id: joined.scheduler_id,
                        id: joined.id,
                        deadline,
                    }),
                    None if delivery_ready => None,
                    None => return Err(unavailable()),
                }
            },
        };

        let mut pipelines = Vec::new();
        let mut image_terminals = Vec::new();
        let mut controlled_image_work_items = 0usize;
        for (pipeline_id, document) in self.documents.borrow().iter() {
            if !target.contains_pipeline(pipeline_id) {
                return Err(unavailable());
            }
            let rendering = document.pending_rendering_observation(no_gc);
            let eligibility = rendering.eligibility;
            if rendering.animation_frames.callbacks_running ||
                eligibility.render_blocked != (rendering.render_blocking_elements != 0) ||
                eligibility.fully_active && eligibility.throttled ||
                eligibility.animation_tick_eligible !=
                    (eligibility.fully_active && !eligibility.throttled) ||
                eligibility.rendering_opportunity_eligible !=
                    (eligibility.fully_active &&
                        !eligibility.render_blocked &&
                        !rendering.canvas.waiting_on_canvas_image_updates)
            {
                return Err(unavailable());
            }
            let activity = if eligibility.fully_active {
                PendingRenderingPipelineActivity::FullyActive
            } else if eligibility.throttled {
                PendingRenderingPipelineActivity::Throttled
            } else {
                PendingRenderingPipelineActivity::Inactive
            };

            let css_animations = rendering.css_animations;
            let unsupported_animations = css_animations
                .unsupported_pending_or_running
                .checked_total()
                .ok_or(DocumentControlError::QueueLengthOverflow)?;

            let image =
                document.pending_image_animation_observation(self.document_clock.is_controlled());
            if image.timeline_terminal.is_some() {
                return Err(unavailable());
            }
            if let Some(error) = image.scheduler_terminal {
                image_terminals.push(PendingImageTimerTerminalObservation { pipeline_id, error });
            }
            let mut finite_images = image.counts.finite;
            let mut infinite_images = image.counts.infinite;
            let mut unsupported_images = PendingAnimatedImageUnsupportedCounts {
                loop_count_unavailable: image.counts.unsupported_loop_count,
                timeline_uncontrolled: image.counts.unsupported_timeline,
                timer_binding_unavailable: 0,
            };
            let scheduled_image_timer = match image.retained_callback_timer_id {
                None => None,
                Some(timer_id) => {
                    let joined = scheduler
                        .join_live_deadlines(scheduler.id(), &[timer_id])
                        .map_err(|_| unavailable())?;
                    let joined = joined.first().copied().ok_or_else(unavailable)?;
                    match joined.deadline {
                        Some(deadline) => Some(TimerDeadlineSnapshot {
                            scheduler_id: joined.scheduler_id,
                            id: joined.id,
                            deadline,
                        }),
                        None => {
                            let non_inert = image
                                .counts
                                .retained
                                .checked_sub(image.counts.inert)
                                .ok_or_else(unavailable)?;
                            let originally_non_inert = finite_images
                                .checked_add(infinite_images)
                                .and_then(|count| {
                                    count.checked_add(unsupported_images.loop_count_unavailable)
                                })
                                .and_then(|count| {
                                    count.checked_add(unsupported_images.timeline_uncontrolled)
                                })
                                .ok_or(DocumentControlError::QueueLengthOverflow)?;
                            if originally_non_inert != non_inert {
                                return Err(unavailable());
                            }
                            finite_images = 0;
                            infinite_images = 0;
                            unsupported_images.loop_count_unavailable = 0;
                            unsupported_images.timeline_uncontrolled = 0;
                            unsupported_images.timer_binding_unavailable = non_inert;
                            None
                        },
                    }
                },
            };

            let canvas_count = rendering.canvas.live_canvas_count.ok_or_else(unavailable)?;
            if rendering
                .canvas
                .live_html_canvas_count
                .checked_add(rendering.canvas.live_window_offscreen_canvas_count) !=
                Some(canvas_count)
            {
                return Err(unavailable());
            }
            let nonanimated_images = document.window().pending_nonanimated_image_observation();
            let retained_image_work = nonanimated_images
                .retained_work_items
                .ok_or(DocumentControlError::QueueLengthOverflow)?;
            let controlled_image_work = nonanimated_images
                .controlled_work_items
                .ok_or(DocumentControlError::QueueLengthOverflow)?;
            let unsupported_image_work = nonanimated_images
                .unsupported_work_items
                .ok_or(DocumentControlError::QueueLengthOverflow)?;
            if !nonanimated_images.controlled_retained_record_inventory_matches ||
                controlled_image_work.checked_add(unsupported_image_work) !=
                    Some(retained_image_work)
            {
                return Err(unavailable());
            }
            controlled_image_work_items = controlled_image_work_items
                .checked_add(controlled_image_work)
                .ok_or(DocumentControlError::QueueLengthOverflow)?;
            let pending_images = if self.document_execution_profile ==
                DocumentExecutionProfile::ControlledWebSessionV2
            {
                unsupported_image_work
            } else {
                retained_image_work
            };

            pipelines.push(PendingPipelineRenderingObservation {
                pipeline_id,
                activity,
                render_blocking_elements: u64::from(rendering.render_blocking_elements),
                retained_animation_frame_callbacks: checked_pending_count(
                    rendering.animation_frames.retained_slots,
                )?,
                runnable_animation_frame_callbacks: checked_pending_count(
                    rendering.animation_frames.runnable_callbacks,
                )?,
                document_update_required: rendering.needs_rendering_update,
                pending_animation_events: checked_pending_count(
                    css_animations.pending_event_count,
                )?,
                finite_animations: checked_pending_count(css_animations.finite_pending_or_running)?,
                infinite_animations: checked_pending_count(
                    css_animations.infinite_pending_or_running,
                )?,
                unsupported_animations: checked_pending_count(unsupported_animations)?,
                animated_images: PendingAnimatedImageObservation {
                    retained_images: image.counts.retained,
                    finite_images,
                    infinite_images,
                    inert_images: image.counts.inert,
                    unsupported: unsupported_images,
                    update_ready: rendering.animated_image_update_ready,
                    scheduled_timer: scheduled_image_timer,
                },
                canvas: PendingCanvasObservation {
                    dirty_contexts: checked_pending_count(rendering.canvas.dirty_canvas_count)?,
                    awaiting_async_upload: rendering.canvas.waiting_on_canvas_image_updates,
                    unsupported: PendingCanvasUnsupportedCounts {
                        live_source_inventory_unavailable: 0,
                        offscreen_execution: checked_pending_count(
                            rendering.canvas.offscreen_execution_count,
                        )?,
                        mutation_generation_unbound: checked_pending_count(canvas_count)?,
                    },
                },
                pending_fonts: checked_pending_count(rendering.web_fonts_still_loading)?,
                pending_images: checked_pending_count(pending_images)?,
            });
        }

        let rendering =
            PendingRenderingObservation::new(scheduled_opportunity, opportunity_ready, pipelines)
                .map_err(DocumentControlError::PendingSnapshot)?;
        Ok((rendering, image_terminals, controlled_image_work_items))
    }

    /// Capture rooted active Document parsers. Pending top-level membership remains target
    /// authority, not invented parser-phase evidence; its fetch lifecycle is covered by the exact
    /// Resource fence until a live parser owner exists.
    fn capture_controlled_parsers(
        &self,
        target: &PendingTargetObservation,
    ) -> Result<Vec<PendingParserFacts>, DocumentControlError> {
        let mut facts = Vec::new();
        for (pipeline_id, document) in self.documents.borrow().iter() {
            if !target.contains_pipeline(pipeline_id) {
                return Err(DocumentControlError::PendingFactUnavailable(
                    DocumentPendingFact::Parser,
                ));
            }
            let Some(rooted_parser) = document.active_parser() else {
                continue;
            };
            let owner_id = PendingParserOwnerId::try_new(rooted_parser.pending_owner_id().get())
                .ok_or(DocumentControlError::PendingFactUnavailable(
                    DocumentPendingFact::Parser,
                ))?;
            let Some(parser) = rooted_parser.pending_state() else {
                continue;
            };
            let (phase, disposition) = if parser.is_suspended() {
                (
                    PendingParserPhase::Suspended,
                    PendingSourceDisposition::Unsupported(
                        PendingUnsupportedSourceReason::SuspendedParser,
                    ),
                )
            } else if parser.is_script_created() && parser.is_awaiting_input() {
                (
                    PendingParserPhase::AwaitingScriptInput,
                    PendingSourceDisposition::Unsupported(
                        PendingUnsupportedSourceReason::ScriptCreatedParserInput,
                    ),
                )
            } else if parser.is_runnable() {
                (PendingParserPhase::Ready, PendingSourceDisposition::Ready)
            } else if parser.is_awaiting_input() {
                (
                    PendingParserPhase::AwaitingExternalInput,
                    PendingSourceDisposition::AwaitingExternalIo(PendingExternalIoEvidence {
                        owner: PendingExternalIoOwner::DocumentParser,
                        load_blocking: PendingExternalIoLoadBlocking::Unknown,
                    }),
                )
            } else {
                return Err(DocumentControlError::PendingFactUnavailable(
                    DocumentPendingFact::Parser,
                ));
            };
            facts.push(PendingParserFacts {
                owner_id,
                pipeline_id,
                kind: PendingParserSourceKind::DocumentParser,
                phase,
                disposition,
            });
        }
        Ok(facts)
    }

    /// Capture every retained externally-triggered source from the rooted target Documents.
    /// Each GlobalScope canonicalizes its own native identities; the transactional ledger rejects
    /// any cross-owner duplicate before making the complete inventory authoritative.
    fn capture_controlled_persistent_sources(
        &self,
        target: &PendingTargetObservation,
    ) -> Result<Vec<PendingPersistentSourceIdentity>, DocumentControlError> {
        let unavailable =
            || DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Sources);
        let mut sources = Vec::new();
        for (pipeline_id, document) in self.documents.borrow().iter() {
            if !target.contains_pipeline(pipeline_id) {
                return Err(unavailable());
            }
            sources.extend(
                document
                    .window()
                    .upcast::<GlobalScope>()
                    .pending_persistent_sources()
                    .map_err(|_| unavailable())?,
            );
        }
        Ok(sources)
    }

    fn capture_controlled_pending(
        &self,
        target: &PendingTargetObservation,
        mut target_terminals: PendingRuntimeTerminals,
        producer_capture: ProducerCapture,
        no_gc: &NoGC,
    ) -> Result<RawPendingSnapshot, DocumentControlError> {
        self.validate_controlled_target(target)?;
        if self.debugger_paused.get() || self.controlled_debugger_unsupported.get() {
            return Err(DocumentControlError::PendingFactUnavailable(
                DocumentPendingFact::MicrotaskCoverage,
            ));
        }
        let input = self
            .controlled_input
            .as_ref()
            .ok_or(DocumentControlError::NotControlled)?
            .borrow();
        let task_observation = self.task_queue.observation();
        let input_facts = PendingInputBarrierFacts {
            revision: input.last_revision(),
            revision_exhausted: input.revision_overflowed(),
            ready_events: input.ready_len(),
            intake_saturated: input.intake_saturated(),
            tasks: task_observation,
        };
        let microtasks = self.microtask_queue.observation();
        let execution = self
            .document_execution_ledger
            .as_ref()
            .ok_or(DocumentControlError::PendingFactUnavailable(
                DocumentPendingFact::RuntimeTerminals,
            ))?
            .observation();
        let logical_timer_owners = self.observe_controlled_logical_timers(target)?;
        let parsers = self.capture_controlled_parsers(target)?;
        let persistent_sources = self.capture_controlled_persistent_sources(target)?;
        let scheduler = self.timer_scheduler.borrow();
        let barrier = capture_barrier_observation(
            self.event_loop_id,
            &self.document_clock,
            &scheduler,
            self.timer_control_terminal_error(),
            input_facts,
            microtasks,
            execution,
        )?;
        let (logical_timers, logical_timer_terminals) =
            Self::capture_controlled_logical_timers(logical_timer_owners, &scheduler)?;
        let (rendering, image_terminals, controlled_image_work_items) =
            self.capture_controlled_rendering(target, &scheduler, no_gc)?;
        drop(scheduler);
        drop(input);

        target_terminals = target_terminals
            .with_additional_timer_terminals(logical_timer_terminals, image_terminals)
            .map_err(DocumentControlError::PendingSnapshot)?;

        let dom = self.dom_mutation_observation(target.webview_id).ok_or(
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::DomMutationEpoch),
        )?;
        if target_terminals.dom_generation.is_some() {
            return Err(DocumentControlError::PendingFactUnavailable(
                DocumentPendingFact::RuntimeTerminals,
            ));
        }
        target_terminals.dom_generation =
            dom.terminal.map(|_| PendingGenerationTerminalObservation {
                webview_id: target.webview_id,
                error: PendingGenerationTerminal::Exhausted,
            });

        let fence = self.document_producer_fence.as_ref().ok_or(
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Producers),
        )?;
        let microtask_checkpoint =
            PendingMicrotaskCheckpoint::new(microtasks.completed_checkpoint_generation);
        let mut state = self
            .document_control_state
            .as_ref()
            .ok_or(DocumentControlError::NotControlled)?
            .borrow_mut();
        state
            .ensure_webview(target.webview_id)
            .map_err(|error| map_pending_normalize_error(PendingNormalizeError::State(error)))?;
        state
            .pending
            .replace_logical_timers(target.webview_id, logical_timers)
            .map_err(|error| map_pending_normalize_error(PendingNormalizeError::State(error)))?;
        state
            .pending
            .replace_parsers(target.webview_id, parsers)
            .map_err(|error| map_pending_normalize_error(PendingNormalizeError::State(error)))?;
        state
            .pending
            .replace_persistent_sources(target.webview_id, persistent_sources)
            .map_err(|error| map_pending_normalize_error(PendingNormalizeError::State(error)))?;
        state
            .pending
            .bind_resource_fence_network_authority(target.webview_id, fence.id())
            .map_err(|error| map_pending_normalize_error(PendingNormalizeError::State(error)))?;
        let producers = match producer_capture {
            ProducerCapture::Exact(observation) => observation,
            ProducerCapture::Passive
                if microtask_checkpoint == PendingMicrotaskCheckpoint::ZERO =>
            {
                state
                    .producer_qualification
                    .not_checkpointed(fence.snapshot())
                    .map_err(|error| {
                        map_pending_normalize_error(PendingNormalizeError::State(error))
                    })?
            },
            ProducerCapture::Passive => {
                if state.producer_checkpoint == DocumentProducerCheckpoint::ZERO {
                    return Err(DocumentControlError::PendingFactUnavailable(
                        DocumentPendingFact::Producers,
                    ));
                }
                let producer_checkpoint = state.producer_checkpoint;
                state
                    .producer_qualification
                    .passive(microtask_checkpoint, producer_checkpoint, fence.snapshot())
                    .map_err(|error| {
                        map_pending_normalize_error(PendingNormalizeError::State(error))
                    })?
            },
            ProducerCapture::FreshCheckpoint => {
                if microtask_checkpoint == PendingMicrotaskCheckpoint::ZERO {
                    return Err(DocumentControlError::PendingFactUnavailable(
                        DocumentPendingFact::Producers,
                    ));
                }
                let checkpoint = state
                    .producer_checkpoint
                    .checked_next()
                    .map_err(DocumentControlError::ProducerFence)?;
                let observation = state
                    .producer_observer
                    .observe(fence, checkpoint)
                    .map_err(DocumentControlError::ProducerFence)?;
                state.producer_checkpoint = checkpoint;
                let fresh_snapshot = fence.snapshot();
                state
                    .producer_qualification
                    .qualify(
                        microtask_checkpoint,
                        checkpoint,
                        observation,
                        fresh_snapshot,
                    )
                    .map_err(|error| {
                        map_pending_normalize_error(PendingNormalizeError::State(error))
                    })?
            },
        };
        if self.document_execution_profile == DocumentExecutionProfile::ControlledWebSessionV2 &&
            producers.snapshot.terminal_error().is_none() &&
            producers
                .snapshot
                .for_kind(DocumentProducerKind::Image)
                .pending() <
                u64::try_from(controlled_image_work_items)
                    .map_err(|_| DocumentControlError::QueueLengthOverflow)?
        {
            return Err(DocumentControlError::PendingFactUnavailable(
                DocumentPendingFact::Rendering,
            ));
        }
        let owner = state
            .pending
            .owner_snapshot(target.webview_id)
            .map_err(|error| map_pending_normalize_error(PendingNormalizeError::State(error)))?;
        let facts = RawPendingBuildFacts {
            target: target.clone(),
            owner,
            dom_epoch: embedder_traits::document_pending::DomEpoch::new(dom.epoch),
            clock: PendingClockFacts {
                observation: barrier.clock,
                terminal: barrier.clock_terminal.map(|terminal| terminal.error),
            },
            scheduler: PendingSchedulerFacts {
                observation: barrier.scheduler,
                terminal: barrier.scheduler_terminal.map(|terminal| terminal.error),
            },
            input: PendingInputFacts {
                revision: input_facts.revision,
                revision_exhausted: input_facts.revision_exhausted,
                ready_events: input_facts.ready_events,
                intake_saturated: input_facts.intake_saturated,
                tasks: PendingTaskFacts {
                    ready: task_observation.ready,
                    throttled: task_observation.throttled,
                    inactive: task_observation.inactive,
                },
            },
            microtasks: PendingMicrotaskFacts {
                queued: microtasks.queued_count,
                completed_checkpoint: microtask_checkpoint,
                checkpoint_in_progress: microtasks.checkpoint_in_progress,
                terminal: barrier.microtask_terminal.map(|terminal| terminal.error),
            },
            execution: Some(barrier.execution),
            producers,
            rendering: Some(rendering),
            supplemental_terminals: target_terminals,
        };
        state
            .pending
            .normalize_and_build(facts)
            .map_err(map_pending_normalize_error)
    }

    fn handle_controlled_advance(
        &self,
        cx: &mut js::context::JSContext,
        request_id: DocumentControlRequestId,
        cancellation_id: DocumentControlCancellationId,
        target: Box<PendingTargetObservation>,
        target_terminals: PendingRuntimeTerminals,
        supplied_token: DocumentAdvanceToken,
        retained_token: Option<DocumentAdvanceToken>,
    ) {
        let reject = |error| {
            self.send_document_control_response(
                request_id,
                cancellation_id,
                target.clone(),
                DocumentControlOutcome::Rejected(error),
            );
        };
        let Some(token) = retained_token else {
            reject(DocumentControlError::AdvanceTokenUnavailable {
                observed: supplied_token.id(),
            });
            return;
        };
        if token.id() != supplied_token.id() || token != supplied_token {
            reject(DocumentControlError::StaleAdvanceToken {
                expected: token.id(),
                observed: supplied_token.id(),
            });
            return;
        }
        if token.target() != &*target {
            reject(DocumentControlError::AdvancePrecondition(
                DocumentAdvanceTokenInvariantError::TargetChanged {
                    expected: Box::new(token.target().clone()),
                    observed: target.clone(),
                },
            ));
            return;
        }
        let Some(fence) = &self.document_producer_fence else {
            self.send_document_control_response(
                request_id,
                cancellation_id,
                target.clone(),
                DocumentControlOutcome::AdvanceOutcomeIndeterminate {
                    token_id: token.id(),
                    target: target.clone(),
                    deadline: token.deadline(),
                },
            );
            return;
        };
        let active = (request_id, cancellation_id);
        if self.drain_active_controls_until_quiet(active) {
            return;
        }
        // Intake can discard a producer-fenced envelope (for example when a pipeline was
        // closed). Do all ordinary intake before producer exclusion so dropping such an
        // envelope can complete its guard without trying to re-enter the fence mutex.
        self.drain_ready_controlled_inputs();
        if self.drain_active_controls_until_quiet(active) {
            return;
        }

        let guarded = fence.with_matching_snapshot(token.producers().snapshot, || {
            if self.drain_active_controls_until_quiet(active) {
                return Ok(None);
            }
            self.validate_controlled_target(&target)?;
            let pending = self.capture_controlled_pending(
                &target,
                target_terminals.clone(),
                ProducerCapture::Exact(token.producers()),
                &cx.no_gc(),
            )?;
            token
                .validate_against(&pending)
                .map_err(DocumentControlError::AdvancePrecondition)?;
            if self.drain_active_controls_until_quiet(active) {
                return Ok(None);
            }
            self.timer_scheduler
                .borrow_mut()
                .validate_advance_and_detach(token.now(), token.deadline())
                .map(Some)
                .map_err(DocumentControlError::Timer)
        });
        let detached: DetachedTimerEvent = match guarded {
            Ok(Ok(Some(detached))) => detached,
            Ok(Ok(None)) => return,
            Ok(Err(error)) => {
                if is_pending_capture_error(&error) {
                    self.send_document_control_response(
                        request_id,
                        cancellation_id,
                        target.clone(),
                        DocumentControlOutcome::AdvanceOutcomeIndeterminate {
                            token_id: token.id(),
                            target: target.clone(),
                            deadline: token.deadline(),
                        },
                    );
                } else {
                    reject(error);
                }
                return;
            },
            Err(mismatch) => {
                let observed = PendingProducerObservation::new(
                    self.event_loop_id,
                    token.producers().microtask_checkpoint,
                    token.producers().checkpoint,
                    mismatch.observed(),
                    PendingProducerStability::Unqualified,
                    None,
                );
                let error = match observed {
                    Ok(observed) => DocumentControlError::AdvancePrecondition(
                        DocumentAdvanceTokenInvariantError::ProducersChanged {
                            expected: Box::new(token.producers()),
                            observed: Box::new(observed),
                        },
                    ),
                    Err(error) => DocumentControlError::PendingSnapshot(error),
                };
                reject(error);
                return;
            },
        };

        // Dispatch only after producer exclusion and the scheduler borrow have both ended.
        detached.dispatch();
        self.admit_controlled_input(MixedMessage::TimerFired);
        self.drain_ready_controlled_inputs();
        if self.drain_active_controls_until_quiet(active) {
            return;
        }
        let outcome = match self
            .validate_controlled_target(&target)
            .and_then(|()| {
                self.capture_controlled_pending(
                    &target,
                    target_terminals,
                    ProducerCapture::Passive,
                    &cx.no_gc(),
                )
            })
            .and_then(|pending| {
                self.completed_control_observation(
                    DocumentControlAction::TimerActivated(token.deadline()),
                    pending,
                )
            }) {
            Ok(observation) => DocumentControlOutcome::Completed(Box::new(observation)),
            Err(_) => DocumentControlOutcome::AdvanceOutcomeIndeterminate {
                token_id: token.id(),
                target: target.clone(),
                deadline: token.deadline(),
            },
        };
        if !self.drain_active_controls_until_quiet(active) {
            self.send_document_control_response(request_id, cancellation_id, target, outcome);
        }
    }

    fn send_document_control_response(
        &self,
        request_id: DocumentControlRequestId,
        cancellation_id: DocumentControlCancellationId,
        target: Box<PendingTargetObservation>,
        outcome: DocumentControlOutcome,
    ) -> bool {
        let Some(source_pipeline_id) = target
            .active_top_level
            .map(|active| active.pipeline_id)
            .or_else(|| target.pending_top_level_pipelines().first().copied())
        else {
            warn!("cannot route a controlled-document response for an empty target");
            return false;
        };
        self.senders
            .pipeline_to_constellation_sender
            .send((
                target.webview_id,
                source_pipeline_id,
                ScriptToConstellationMessage::DocumentControlResponse {
                    request_id,
                    cancellation_id,
                    target,
                    outcome,
                },
            ))
            .is_ok()
    }

    fn validate_controlled_route(
        &self,
        target: &PendingTargetObservation,
    ) -> Result<(), DocumentControlError> {
        if self.controlled_input.is_none() {
            return Err(DocumentControlError::NotControlled);
        }
        if target.event_loop_id != self.event_loop_id {
            return Err(DocumentControlError::EventLoopUnavailable);
        }
        for (_, document) in self.documents.borrow().iter() {
            if document.webview_id() != target.webview_id {
                return Err(DocumentControlError::SharedEventLoopWebView);
            }
            if !document.window().is_top_level() {
                return Err(DocumentControlError::PendingFactUnavailable(
                    DocumentPendingFact::TargetMembership,
                ));
            }
        }
        for load in self.incomplete_loads.borrow().iter() {
            if load.webview_id != target.webview_id {
                return Err(DocumentControlError::SharedEventLoopWebView);
            }
            if load.parent_info.is_some() {
                return Err(DocumentControlError::PendingFactUnavailable(
                    DocumentPendingFact::TargetMembership,
                ));
            }
        }
        Ok(())
    }

    /// Return the one exact initial root pipeline whose async HTTP(S) navigation can be admitted
    /// by an explicit bootstrap Drive without constructing a Document or executing author script.
    fn initial_pipeline_bootstrap_event(
        &self,
        target: &PendingTargetObservation,
    ) -> Option<PipelineId> {
        let local_document_count = self.documents.borrow().iter().count();
        let local_incomplete_load_count = self.incomplete_loads.borrow().len();
        let local_parser_context_count = self.incomplete_parser_contexts.0.borrow().len();
        let input = self.controlled_input.as_ref()?.borrow();
        let MixedMessage::FromConstellation(ScriptThreadMessage::SpawnPipeline(info)) =
            input.ready.front()?
        else {
            return None;
        };
        let pipeline_id = initial_pipeline_bootstrap_pipeline(
            target,
            InitialPipelineBootstrapFacts {
                pipeline_id: info.new_pipeline_id,
                webview_id: info.webview_id,
                browsing_context_id: info.browsing_context_id,
                parent_pipeline_id: info.parent_info,
                local_document_count,
                local_incomplete_load_count,
                local_parser_context_count,
                is_http_or_https: matches!(info.load_data.url.scheme(), "http" | "https"),
                has_javascript_result: info.load_data.js_eval_result.is_some(),
                has_srcdoc: !info.load_data.srcdoc.is_empty(),
            },
        )?;
        drop(input);
        self.validate_controlled_target_projection(target, Some(pipeline_id), None)
            .ok()?;
        Some(pipeline_id)
    }

    /// Return the sole exact replacement SpawnPipeline after complete owner intake.
    ///
    /// Ordinary source-document events may precede it because Servo gives SpawnPipeline intake
    /// priority over the sequential backlog. Lifecycle and earlier immediate events are barriers.
    fn replacement_pipeline_bootstrap_event_position(
        &self,
        target: &PendingTargetObservation,
        source_pipeline_id: PipelineId,
        pipeline_id: PipelineId,
    ) -> ReplacementPipelineBootstrapQueueState {
        let document_ids = self
            .documents
            .borrow()
            .iter()
            .map(|(pipeline_id, _)| pipeline_id)
            .collect::<Vec<_>>();
        let local_document_pipeline_id = match document_ids.as_slice() {
            [pipeline_id] => Some(*pipeline_id),
            _ => None,
        };
        let local_incomplete_load_count = self.incomplete_loads.borrow().len();
        let local_parser_context_count = self.incomplete_parser_contexts.0.borrow().len();
        let input = self
            .controlled_input
            .as_ref()
            .expect("the controlled loop requires an owner input queue")
            .borrow();
        if input.revision_overflowed() {
            return ReplacementPipelineBootstrapQueueState::InputRevisionOverflow;
        }
        let queue_state = replacement_pipeline_bootstrap_classified_position(
            input
                .ready
                .iter()
                .map(replacement_pipeline_bootstrap_queued_event),
            input.intake_saturated(),
            pipeline_id,
        );
        let ReplacementPipelineBootstrapQueueState::Ready { event_index } = queue_state else {
            return queue_state;
        };
        let MixedMessage::FromConstellation(ScriptThreadMessage::SpawnPipeline(info)) =
            input.ready.get(event_index).expect("qualified queue index")
        else {
            return ReplacementPipelineBootstrapQueueState::Unavailable;
        };
        let facts = ReplacementPipelineBootstrapFacts {
            source_pipeline_id,
            pipeline_id: info.new_pipeline_id,
            webview_id: info.webview_id,
            browsing_context_id: info.browsing_context_id,
            parent_pipeline_id: info.parent_info,
            local_document_pipeline_id,
            local_document_count: document_ids.len(),
            local_incomplete_load_count,
            local_parser_context_count,
            is_http_or_https: matches!(info.load_data.url.scheme(), "http" | "https"),
            has_javascript_result: info.load_data.js_eval_result.is_some(),
            has_srcdoc: !info.load_data.srcdoc.is_empty(),
        };
        let pipeline_id = replacement_pipeline_bootstrap_pipeline(target, facts).or_else(|| {
            warn!("replacement bootstrap facts did not qualify: {facts:?}");
            None
        });
        let Some(pipeline_id) = pipeline_id else {
            return ReplacementPipelineBootstrapQueueState::Unavailable;
        };
        drop(input);
        if self
            .validate_controlled_target_projection(target, Some(pipeline_id), None)
            .is_err()
        {
            return ReplacementPipelineBootstrapQueueState::Unavailable;
        }
        ReplacementPipelineBootstrapQueueState::Ready { event_index }
    }

    fn replacement_pipeline_bootstrap_event(
        &self,
        target: &PendingTargetObservation,
        source_pipeline_id: PipelineId,
        pipeline_id: PipelineId,
    ) -> Option<PipelineId> {
        matches!(
            self.replacement_pipeline_bootstrap_event_position(
                target,
                source_pipeline_id,
                pipeline_id,
            ),
            ReplacementPipelineBootstrapQueueState::Ready { .. }
        )
        .then_some(pipeline_id)
    }

    /// Await owner admission of the exact replacement SpawnPipeline without running ordinary
    /// input. The control lane is drained after every bounded ordinary-input batch so cancellation
    /// cannot be starved by a saturated producer stream. Owner-observed cancellation wins until
    /// the caller removes the selected event, which is the bootstrap linearization point.
    fn await_replacement_pipeline_bootstrap_event(
        &self,
        active: (DocumentControlRequestId, DocumentControlCancellationId),
        target: &PendingTargetObservation,
        source_pipeline_id: PipelineId,
        pipeline_id: PipelineId,
    ) -> ReplacementPipelineBootstrapWaitOutcome {
        let unavailable = || DocumentControlError::ReplacementPipelineBootstrapUnavailable {
            source_pipeline_id,
            pipeline_id,
        };
        loop {
            let ordinary = self.drain_ready_controlled_inputs();
            if self.drain_active_controls_until_quiet(active) {
                return ReplacementPipelineBootstrapWaitOutcome::Cancelled;
            }
            if self.closing.load(Ordering::SeqCst) {
                self.prepare_for_shutdown_inner();
                self.controlled_input
                    .as_ref()
                    .expect("the controlled loop requires an owner input queue")
                    .borrow_mut()
                    .retire_control(active);
                return ReplacementPipelineBootstrapWaitOutcome::Failed;
            }

            match self.replacement_pipeline_bootstrap_event_position(
                target,
                source_pipeline_id,
                pipeline_id,
            ) {
                ReplacementPipelineBootstrapQueueState::Ready { event_index } => {
                    // This is the final owner-control barrier before the caller removes the event.
                    // No ordinary intake runs between this check and that removal, so the selected
                    // index and the relative order of every retained event remain stable.
                    if self.drain_active_controls_until_quiet(active) {
                        return ReplacementPipelineBootstrapWaitOutcome::Cancelled;
                    }
                    if self.closing.load(Ordering::SeqCst) {
                        self.prepare_for_shutdown_inner();
                        self.controlled_input
                            .as_ref()
                            .expect("the controlled loop requires an owner input queue")
                            .borrow_mut()
                            .retire_control(active);
                        return ReplacementPipelineBootstrapWaitOutcome::Failed;
                    }
                    return ReplacementPipelineBootstrapWaitOutcome::Ready { event_index };
                },
                ReplacementPipelineBootstrapQueueState::Interrupted |
                ReplacementPipelineBootstrapQueueState::Unavailable => {
                    return ReplacementPipelineBootstrapWaitOutcome::Rejected(unavailable());
                },
                ReplacementPipelineBootstrapQueueState::InputRevisionOverflow => {
                    return ReplacementPipelineBootstrapWaitOutcome::Rejected(
                        DocumentControlError::InputRevisionOverflow,
                    );
                },
                ReplacementPipelineBootstrapQueueState::AwaitingInput => {},
            }

            if ordinary.saturated {
                continue;
            }
            match self
                .receivers
                .recv_document_control_timeout(CONTROLLED_AUTHORITY_POLL_INTERVAL)
            {
                DocumentControlWaitResult::Message(ScriptThreadControlMessage::Cancel {
                    request_id,
                    cancellation_id,
                }) if (request_id, cancellation_id) == active => {
                    self.controlled_input
                        .as_ref()
                        .expect("the controlled loop requires an owner input queue")
                        .borrow_mut()
                        .retire_control(active);
                    return ReplacementPipelineBootstrapWaitOutcome::Cancelled;
                },
                DocumentControlWaitResult::Message(message) => {
                    self.admit_controlled_command(message);
                },
                DocumentControlWaitResult::TimedOut => {},
                DocumentControlWaitResult::Closed => {
                    self.controlled_input
                        .as_ref()
                        .expect("the controlled loop requires an owner input queue")
                        .borrow_mut()
                        .retire_control(active);
                    return ReplacementPipelineBootstrapWaitOutcome::Failed;
                },
            }
        }
    }

    fn pending_replacement_pipeline_bootstrap_event(
        &self,
        target: &PendingTargetObservation,
    ) -> Option<(PipelineId, PipelineId)> {
        let source_pipeline_id = target.active_top_level?.pipeline_id;
        let [pipeline_id] = target.pending_top_level_pipelines() else {
            return None;
        };
        let pipeline_id = *pipeline_id;
        self.replacement_pipeline_bootstrap_event(target, source_pipeline_id, pipeline_id)?;
        Some((source_pipeline_id, pipeline_id))
    }

    /// Return the sole response-headers event which can synchronously emit the first correlated
    /// top-level activation. Redirect, cancellation, 204/205, nested, replacement, and
    /// synchronous-content paths are excluded before the event is removed from owner input.
    fn initial_pipeline_activation_event(
        &self,
        target: &PendingTargetObservation,
        event: &MixedMessage,
    ) -> Option<PipelineId> {
        let MixedMessage::FromScript(MainThreadScriptMsg::NavigationResponse {
            pipeline_id,
            response,
        }) = event
        else {
            return None;
        };
        if NavigationListener::http_redirect_metadata(&response.message).is_some() {
            return None;
        }
        let response_will_activate = match &response.message {
            FetchResponseMsg::ProcessResponse(_, Ok(metadata)) => {
                !metadata.metadata().status.in_range(204..=205)
            },
            FetchResponseMsg::ProcessResponse(_, Err(NetworkError::LoadCancelled)) => false,
            FetchResponseMsg::ProcessResponse(_, Err(_)) => true,
            _ => false,
        };

        let incomplete_loads = self.incomplete_loads.borrow();
        let [load] = incomplete_loads.as_slice() else {
            return None;
        };
        let parser_contexts = self.incomplete_parser_contexts.0.borrow();
        let parser_pipeline_id = match parser_contexts.as_slice() {
            [(pipeline_id, _)] => Some(*pipeline_id),
            _ => None,
        };
        let document_ids = self
            .documents
            .borrow()
            .iter()
            .map(|(pipeline_id, _)| pipeline_id)
            .collect::<Vec<_>>();
        let local_document_pipeline_id = match document_ids.as_slice() {
            [pipeline_id] => Some(*pipeline_id),
            _ => None,
        };
        let pipeline_id = initial_pipeline_activation_pipeline(
            target,
            InitialPipelineActivationFacts {
                pipeline_id: *pipeline_id,
                webview_id: load.webview_id,
                browsing_context_id: load.browsing_context_id,
                parent_pipeline_id: load.parent_info,
                local_document_pipeline_id,
                local_document_count: document_ids.len(),
                local_incomplete_load_count: incomplete_loads.len(),
                local_parser_context_count: parser_contexts.len(),
                parser_pipeline_id,
                is_http_or_https: matches!(load.load_data.url.scheme(), "http" | "https"),
                has_javascript_result: load.load_data.js_eval_result.is_some(),
                has_srcdoc: !load.load_data.srcdoc.is_empty(),
                response_will_activate,
            },
        )?;
        drop(parser_contexts);
        drop(incomplete_loads);
        self.validate_controlled_target(target).ok()?;
        Some(pipeline_id)
    }

    fn validate_initial_pipeline_activation_local_target(
        &self,
        target: &PendingTargetObservation,
        pipeline_id: PipelineId,
    ) -> Result<(), DocumentControlError> {
        self.validate_controlled_target(target)?;
        let document_ids = self
            .documents
            .borrow()
            .iter()
            .map(|(pipeline_id, _)| pipeline_id)
            .collect::<Vec<_>>();
        let parser_context_ids = self
            .incomplete_parser_contexts
            .0
            .borrow()
            .iter()
            .map(|(pipeline_id, _)| *pipeline_id)
            .collect::<Vec<_>>();
        if document_ids != [pipeline_id] ||
            !self.incomplete_loads.borrow().is_empty() ||
            parser_context_ids != [pipeline_id]
        {
            return Err(DocumentControlError::PendingFactUnavailable(
                DocumentPendingFact::TargetMembership,
            ));
        }
        Ok(())
    }

    fn resolve_initial_pipeline_activation_control(
        &self,
        active: (DocumentControlRequestId, DocumentControlCancellationId),
        pipeline_id: PipelineId,
        before: &PendingTargetObservation,
        before_terminals: &PendingRuntimeTerminals,
        message: ScriptThreadControlMessage,
    ) -> Option<InitialPipelineActivationAuthority> {
        match message {
            ScriptThreadControlMessage::Cancel {
                request_id,
                cancellation_id,
            } if (request_id, cancellation_id) == active => {
                self.controlled_input
                    .as_ref()
                    .expect("the controlled loop requires an owner input queue")
                    .borrow_mut()
                    .retire_control(active);
                Some(InitialPipelineActivationAuthority::Cancelled)
            },
            ScriptThreadControlMessage::Command {
                request_id,
                cancellation_id,
                target,
                target_terminals,
                command,
            } if (request_id, cancellation_id) == active => {
                let authorized = matches!(command, DocumentControlCommand::DriveOneTurn) &&
                    target_terminals == *before_terminals &&
                    before.active_top_level.map_or_else(
                        || {
                            is_exact_initial_pipeline_activation_transition(
                                before,
                                &target,
                                pipeline_id,
                            )
                        },
                        |source| {
                            is_exact_replacement_pipeline_activation_transition(
                                before,
                                &target,
                                source.pipeline_id,
                                pipeline_id,
                            )
                        },
                    ) &&
                    self.validate_initial_pipeline_activation_local_target(&target, pipeline_id)
                        .is_ok();
                if !authorized {
                    self.controlled_input
                        .as_ref()
                        .expect("the controlled loop requires an owner input queue")
                        .borrow_mut()
                        .retire_control(active);
                    return Some(InitialPipelineActivationAuthority::Failed);
                }
                Some(InitialPipelineActivationAuthority::Authorized {
                    target,
                    target_terminals,
                })
            },
            message => {
                self.admit_controlled_command(message);
                None
            },
        }
    }

    /// Wait only for the exact Constellation authority produced by the correlated activation.
    /// Ordinary input is admitted, but cannot execute while the already-linearized headers turn
    /// is unresolved. Forced shutdown and lifecycle input can interrupt the bounded wait.
    fn await_initial_pipeline_activation_authority(
        &self,
        cx: &mut js::context::JSContext,
        active: (DocumentControlRequestId, DocumentControlCancellationId),
        pipeline_id: PipelineId,
        before: &PendingTargetObservation,
        before_terminals: &PendingRuntimeTerminals,
    ) -> InitialPipelineActivationAuthority {
        let mut deferred_active_command = None;
        loop {
            let ordinary = self.drain_ready_controlled_inputs();
            let closing = self.closing.load(Ordering::SeqCst);
            let interruption = {
                let input = self
                    .controlled_input
                    .as_ref()
                    .expect("the controlled loop requires an owner input queue")
                    .borrow();
                initial_pipeline_activation_wait_interrupted(pipeline_id, closing, &input.ready)
            };
            match interruption {
                Some(InitialPipelineActivationWaitInterruption::TerminalLifecycle) => {
                    self.controlled_input
                        .as_ref()
                        .expect("the controlled loop requires an owner input queue")
                        .borrow_mut()
                        .retire_control(active);
                    let event = self
                        .take_controlled_lifecycle_input()
                        .expect("the observed lifecycle input must remain owner-queued");
                    self.process_one_controlled_event(cx, event);
                    return InitialPipelineActivationAuthority::Failed;
                },
                Some(InitialPipelineActivationWaitInterruption::UnrelatedPipelineExit) => {
                    let event = self
                        .take_controlled_lifecycle_input()
                        .expect("the observed lifecycle input must remain owner-queued");
                    if !self.process_one_controlled_event(cx, event) {
                        self.controlled_input
                            .as_ref()
                            .expect("the controlled loop requires an owner input queue")
                            .borrow_mut()
                            .retire_control(active);
                        return InitialPipelineActivationAuthority::Failed;
                    }
                    continue;
                },
                Some(InitialPipelineActivationWaitInterruption::Closing) => {
                    self.prepare_for_shutdown_inner();
                    self.controlled_input
                        .as_ref()
                        .expect("the controlled loop requires an owner input queue")
                        .borrow_mut()
                        .retire_control(active);
                    return InitialPipelineActivationAuthority::Failed;
                },
                None => {},
            }

            // The exact reroute and cancellation use the priority control lane. Poll it before a
            // saturated ordinary-input batch can continue, otherwise an always-ready resource
            // stream can starve the authority handoff indefinitely. Retain an exact command until
            // a non-saturated intake proves there is no unseen lifecycle suffix; cancellation is
            // still resolved immediately.
            loop {
                match self.receivers.recv_document_control_timeout(Duration::ZERO) {
                    DocumentControlWaitResult::Message(message) => {
                        let is_active_command = matches!(
                            &message,
                            ScriptThreadControlMessage::Command {
                                request_id,
                                cancellation_id,
                                ..
                            } if (*request_id, *cancellation_id) == active
                        );
                        if is_active_command {
                            if deferred_active_command.replace(message).is_some() {
                                self.controlled_input
                                    .as_ref()
                                    .expect("the controlled loop requires an owner input queue")
                                    .borrow_mut()
                                    .retire_control(active);
                                return InitialPipelineActivationAuthority::Failed;
                            }
                            continue;
                        }
                        if let Some(authority) = self.resolve_initial_pipeline_activation_control(
                            active,
                            pipeline_id,
                            before,
                            before_terminals,
                            message,
                        ) {
                            return authority;
                        }
                    },
                    DocumentControlWaitResult::TimedOut => break,
                    DocumentControlWaitResult::Closed => {
                        self.controlled_input
                            .as_ref()
                            .expect("the controlled loop requires an owner input queue")
                            .borrow_mut()
                            .retire_control(active);
                        return InitialPipelineActivationAuthority::Failed;
                    },
                }
            }
            if ordinary.saturated {
                continue;
            }
            if let Some(message) = deferred_active_command.take() {
                return self
                    .resolve_initial_pipeline_activation_control(
                        active,
                        pipeline_id,
                        before,
                        before_terminals,
                        message,
                    )
                    .expect("a retained active command must resolve the authority wait");
            }

            let message = match self
                .receivers
                .recv_document_control_timeout(CONTROLLED_AUTHORITY_POLL_INTERVAL)
            {
                DocumentControlWaitResult::Message(message) => message,
                DocumentControlWaitResult::TimedOut => continue,
                DocumentControlWaitResult::Closed => {
                    self.controlled_input
                        .as_ref()
                        .expect("the controlled loop requires an owner input queue")
                        .borrow_mut()
                        .retire_control(active);
                    return InitialPipelineActivationAuthority::Failed;
                },
            };
            if self.closing.load(Ordering::SeqCst) {
                self.prepare_for_shutdown_inner();
                self.controlled_input
                    .as_ref()
                    .expect("the controlled loop requires an owner input queue")
                    .borrow_mut()
                    .retire_control(active);
                return InitialPipelineActivationAuthority::Failed;
            }
            if let Some(authority) = self.resolve_initial_pipeline_activation_control(
                active,
                pipeline_id,
                before,
                before_terminals,
                message,
            ) {
                return authority;
            }
        }
    }

    fn validate_controlled_target(
        &self,
        target: &PendingTargetObservation,
    ) -> Result<(), DocumentControlError> {
        self.validate_controlled_route(target)?;
        let mut local_pipelines = Vec::new();
        for (pipeline_id, _) in self.documents.borrow().iter() {
            if target.active_top_level.map(|active| active.pipeline_id) != Some(pipeline_id) {
                // 0.1 controls one active top-level document and does not claim BFCache history.
                return Err(DocumentControlError::PendingFactUnavailable(
                    DocumentPendingFact::TargetMembership,
                ));
            }
            local_pipelines.push(pipeline_id);
        }
        for load in self.incomplete_loads.borrow().iter() {
            local_pipelines.push(load.pipeline_id);
        }
        local_pipelines.sort_unstable();
        local_pipelines.dedup();
        if local_pipelines != target.pipelines() {
            return Err(DocumentControlError::PendingFactUnavailable(
                DocumentPendingFact::TargetMembership,
            ));
        }

        let mut fully_active: Vec<_> = self.get_fully_active_document_ids().into_iter().collect();
        fully_active.sort_unstable();
        if fully_active != target.fully_active_pipelines() {
            return Err(DocumentControlError::PendingFactUnavailable(
                DocumentPendingFact::TargetMembership,
            ));
        }
        Ok(())
    }

    /// Validate the target immediately before a Drive turn. A lifecycle event may be the exact
    /// one-event transition which makes ScriptThread membership catch up with Constellation, so
    /// admit only a projection that reconciles the complete captured target.
    fn validate_controlled_target_for_event(
        &self,
        target: &PendingTargetObservation,
        event: &MixedMessage,
    ) -> Result<(), DocumentControlError> {
        let current_error = match self.validate_controlled_target(target) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };

        match event {
            MixedMessage::FromConstellation(ScriptThreadMessage::ExitPipeline(
                webview_id,
                pipeline_id,
                _,
            )) if *webview_id == target.webview_id && !target.contains_pipeline(*pipeline_id) => {
                self.validate_controlled_target_projection(target, None, Some(*pipeline_id))
            },
            MixedMessage::FromConstellation(ScriptThreadMessage::ExitScriptThread) => Ok(()),
            _ => Err(current_error),
        }
    }

    fn validate_controlled_target_projection(
        &self,
        target: &PendingTargetObservation,
        added_pipeline: Option<PipelineId>,
        removed_pipeline: Option<PipelineId>,
    ) -> Result<(), DocumentControlError> {
        self.validate_controlled_route(target)?;
        let unavailable =
            || DocumentControlError::PendingFactUnavailable(DocumentPendingFact::TargetMembership);
        let mut local_pipelines = Vec::new();
        let mut removed_observed = removed_pipeline.is_none();
        for (pipeline_id, _) in self.documents.borrow().iter() {
            if Some(pipeline_id) == removed_pipeline {
                removed_observed = true;
                continue;
            }
            if target.active_top_level.map(|active| active.pipeline_id) != Some(pipeline_id) {
                return Err(unavailable());
            }
            local_pipelines.push(pipeline_id);
        }
        for load in self.incomplete_loads.borrow().iter() {
            if Some(load.pipeline_id) == removed_pipeline {
                removed_observed = true;
                continue;
            }
            local_pipelines.push(load.pipeline_id);
        }
        if !removed_observed {
            return Err(unavailable());
        }
        if let Some(pipeline_id) = added_pipeline {
            if local_pipelines.contains(&pipeline_id) {
                return Err(unavailable());
            }
            local_pipelines.push(pipeline_id);
        }
        local_pipelines.sort_unstable();
        local_pipelines.dedup();
        if local_pipelines != target.pipelines() {
            return Err(unavailable());
        }

        let mut fully_active: Vec<_> = self
            .get_fully_active_document_ids()
            .into_iter()
            .filter(|pipeline_id| Some(*pipeline_id) != removed_pipeline)
            .collect();
        fully_active.sort_unstable();
        if fully_active != target.fully_active_pipelines() {
            return Err(unavailable());
        }
        Ok(())
    }

    /// Process exactly one owner-admitted event through Servo's ordinary turn semantics.
    fn process_one_controlled_event(
        &self,
        cx: &mut js::context::JSContext,
        event: MixedMessage,
    ) -> bool {
        self.timer_scheduler
            .borrow_mut()
            .dispatch_completed_timers();
        match event {
            MixedMessage::FromConstellation(ScriptThreadMessage::SpawnPipeline(
                new_pipeline_info,
            )) => {
                self.spawn_pipeline(cx, new_pipeline_info);
                self.finish_controlled_event_loop_turn(cx);
                true
            },
            MixedMessage::FromScript(MainThreadScriptMsg::Inactive) => {
                self.finish_controlled_event_loop_turn(cx);
                true
            },
            MixedMessage::FromConstellation(ScriptThreadMessage::ExitFullScreen(id)) => {
                self.profile_event(ScriptThreadEventCategory::ExitFullscreen, Some(id), || {
                    self.handle_exit_fullscreen(id, cx);
                });
                self.finish_controlled_event_loop_turn(cx);
                true
            },
            event => self.process_controlled_sequential_event(cx, event),
        }
    }

    fn process_controlled_sequential_event(
        &self,
        cx: &mut js::context::JSContext,
        event: MixedMessage,
    ) -> bool {
        for msg in std::iter::once(event) {
            debug!("Processing controlled event {:?}.", msg);
            let category = self.categorize_msg(&msg);
            let pipeline_id = msg.pipeline_id();
            macro_rules! handle_message(
                ( $cx:ident ) => (
                    if self.closing.load(Ordering::SeqCst) {
                        match msg {
                            MixedMessage::FromConstellation(ScriptThreadMessage::ExitScriptThread) => {
                                self.handle_exit_script_thread_msg($cx);
                                return false;
                            },
                            MixedMessage::FromConstellation(ScriptThreadMessage::ExitPipeline(
                                webview_id,
                                pipeline_id,
                                discard_browsing_context,
                            )) => {
                                self.handle_exit_pipeline_msg(
                                    webview_id,
                                    pipeline_id,
                                    discard_browsing_context,
                                    $cx,
                                );
                            },
                            _ => {},
                        }
                        continue;
                    }

                    let exiting = self.profile_event(category, pipeline_id, || {
                        match msg {
                            MixedMessage::FromConstellation(ScriptThreadMessage::ExitScriptThread) => {
                                self.handle_exit_script_thread_msg($cx);
                                return true;
                            },
                            MixedMessage::FromConstellation(inner_msg) => {
                                self.handle_msg_from_constellation(inner_msg, $cx)
                            },
                            MixedMessage::FromScript(inner_msg) => {
                                self.handle_msg_from_script(inner_msg, $cx)
                            },
                            MixedMessage::FromDevtools(inner_msg) => {
                                self.handle_msg_from_devtools(inner_msg, $cx)
                            },
                            MixedMessage::FromImageCache(inner_msg) => {
                                self.handle_msg_from_image_cache(inner_msg, $cx)
                            },
                            #[cfg(feature = "webgpu")]
                            MixedMessage::FromWebGPUServer(inner_msg) => {
                                self.handle_msg_from_webgpu_server(inner_msg, $cx)
                            },
                            MixedMessage::TimerFired => {},
                        }

                        false
                    });

                    if exiting {
                        return false;
                    }

                    self.perform_a_microtask_checkpoint($cx);
                )
            );

            let global = pipeline_id.and_then(|id| self.documents.borrow().find_global(id));
            match global {
                None => {
                    handle_message!(cx);
                },
                Some(global) => {
                    let mut realm = enter_auto_realm(cx, &*global);
                    let cx = &mut realm.current_realm();
                    handle_message!(cx);
                },
            };
        }

        self.finish_controlled_event_loop_turn(cx);
        true
    }

    fn finish_controlled_event_loop_turn(&self, cx: &mut js::context::JSContext) {
        for (_, doc) in self.documents.borrow().iter() {
            let window = doc.window();
            window
                .upcast::<GlobalScope>()
                .perform_a_dom_garbage_collection_checkpoint();
        }

        // TODO(43149): Remove when document replacement is implemented.
        {
            {
                let docs = self.docs_with_no_blocking_loads.borrow();
                for document in docs.iter() {
                    let mut realm = enter_auto_realm(cx, &**document);
                    let cx = &mut realm.current_realm();
                    document.maybe_queue_document_completion(cx);
                }
            }
            self.docs_with_no_blocking_loads.borrow_mut().clear();
        }

        let built_any_display_lists =
            self.needs_rendering_update.load(Ordering::Relaxed) && self.update_the_rendering(cx);

        self.maybe_fulfill_font_ready_promises(cx);
        self.maybe_resolve_pending_screenshot_readiness_requests(cx);

        self.maybe_schedule_rendering_opportunity_after_ipc_message(
            cx.no_gc(),
            built_any_display_lists,
        );
    }

    /// Handle incoming messages from other tasks and the task queue.
    fn handle_msgs(&self, cx: &mut js::context::JSContext) -> bool {
        // Proritize rendering tasks and others, and gather all other events as `sequential`.
        let mut sequential = vec![];

        // Notify the background-hang-monitor we are waiting for an event.
        self.background_hang_monitor.notify_wait();

        // Receive at least one message so we don't spinloop.
        debug!("Waiting for event.");
        let fully_active = self.get_fully_active_document_ids();
        let mut event = self.receivers.recv(
            &self.task_queue,
            &self.timer_scheduler.borrow(),
            &fully_active,
        );

        loop {
            debug!("Handling event: {event:?}");

            // Dispatch any completed timers, so that their tasks can be run below.
            self.timer_scheduler
                .borrow_mut()
                .dispatch_completed_timers();

            // https://html.spec.whatwg.org/multipage/#event-loop-processing-model step 7
            match event {
                // This has to be handled before the ResizeMsg below,
                // otherwise the page may not have been added to the
                // child list yet, causing the find() to fail.
                MixedMessage::FromConstellation(ScriptThreadMessage::SpawnPipeline(
                    new_pipeline_info,
                )) => {
                    self.spawn_pipeline(cx, new_pipeline_info);
                },
                MixedMessage::FromScript(MainThreadScriptMsg::Inactive) => {
                    // An event came-in from a document that is not fully-active, it has been stored by the task-queue.
                    // Continue without adding it to "sequential".
                },
                MixedMessage::FromConstellation(ScriptThreadMessage::ExitFullScreen(id)) => self
                    .profile_event(ScriptThreadEventCategory::ExitFullscreen, Some(id), || {
                        self.handle_exit_fullscreen(id, cx);
                    }),
                _ => {
                    sequential.push(event);
                },
            }

            // If any of our input sources has an event pending, we'll perform another
            // iteration and check for events. If there are no events pending, we'll move
            // on and execute the sequential events.
            match self.receivers.try_recv(&self.task_queue, &fully_active) {
                Some(new_event) => event = new_event,
                None => break,
            }
        }

        // Process the gathered events.
        debug!("Processing events.");
        for msg in sequential {
            debug!("Processing event {:?}.", msg);
            let category = self.categorize_msg(&msg);
            let pipeline_id = msg.pipeline_id();
            // Define a macro to be able to handle the `cx` whether the global exists or not.
            // That's because we need to enter the realm and take the `cx` from it. We cannot
            // use a `match`-statement, since the `realm` would be dropped and the `cx` would
            // outlive the branch.
            macro_rules! handle_message(
                ( $cx:ident ) => (
                    if self.closing.load(Ordering::SeqCst) {
                        // If we've received the closed signal from the BHM, only handle exit messages.
                        match msg {
                            MixedMessage::FromConstellation(ScriptThreadMessage::ExitScriptThread) => {
                                self.handle_exit_script_thread_msg($cx);
                                return false;
                            },
                            MixedMessage::FromConstellation(ScriptThreadMessage::ExitPipeline(
                                webview_id,
                                pipeline_id,
                                discard_browsing_context,
                            )) => {
                                self.handle_exit_pipeline_msg(
                                    webview_id,
                                    pipeline_id,
                                    discard_browsing_context,
                                    $cx,
                                );
                            },
                            _ => {},
                        }
                        continue;
                    }

                    let exiting = self.profile_event(category, pipeline_id, || {
                        match msg {
                            MixedMessage::FromConstellation(ScriptThreadMessage::ExitScriptThread) => {
                                self.handle_exit_script_thread_msg($cx);
                                return true;
                            },
                            MixedMessage::FromConstellation(inner_msg) => {
                                self.handle_msg_from_constellation(inner_msg, $cx)
                            },
                            MixedMessage::FromScript(inner_msg) => {
                                self.handle_msg_from_script(inner_msg, $cx)
                            },
                            MixedMessage::FromDevtools(inner_msg) => {
                                self.handle_msg_from_devtools(inner_msg, $cx)
                            },
                            MixedMessage::FromImageCache(inner_msg) => {
                                self.handle_msg_from_image_cache(inner_msg, $cx)
                            },
                            #[cfg(feature = "webgpu")]
                            MixedMessage::FromWebGPUServer(inner_msg) => {
                                self.handle_msg_from_webgpu_server(inner_msg, $cx)
                            },
                            MixedMessage::TimerFired => {},
                        }

                        false
                    });

                    // If an `ExitScriptThread` message was handled above, bail out now.
                    if exiting {
                        return false;
                    }

                    // https://html.spec.whatwg.org/multipage/#event-loop-processing-model step 6
                    // TODO(#32003): A microtask checkpoint is only supposed to be performed after running a task.
                    self.perform_a_microtask_checkpoint($cx);
                )
            );

            let global = pipeline_id.and_then(|id| self.documents.borrow().find_global(id));
            match global {
                None => {
                    handle_message!(cx);
                },
                Some(global) => {
                    let mut realm = enter_auto_realm(cx, &*global);
                    let cx = &mut realm.current_realm();
                    handle_message!(cx);
                },
            };
        }

        for (_, doc) in self.documents.borrow().iter() {
            let window = doc.window();
            window
                .upcast::<GlobalScope>()
                .perform_a_dom_garbage_collection_checkpoint();
        }

        // TODO(43149): Remove when document replacement is implemented
        {
            // https://html.spec.whatwg.org/multipage/#the-end step 6
            {
                let docs = self.docs_with_no_blocking_loads.borrow();
                for document in docs.iter() {
                    let mut realm = enter_auto_realm(cx, &**document);
                    let cx = &mut realm.current_realm();
                    document.maybe_queue_document_completion(cx);
                }
            }
            self.docs_with_no_blocking_loads.borrow_mut().clear();
        }

        let built_any_display_lists =
            self.needs_rendering_update.load(Ordering::Relaxed) && self.update_the_rendering(cx);

        self.maybe_fulfill_font_ready_promises(cx);
        self.maybe_resolve_pending_screenshot_readiness_requests(cx);

        // This must happen last to detect if any change above makes a rendering update necessary.
        self.maybe_schedule_rendering_opportunity_after_ipc_message(
            cx.no_gc(),
            built_any_display_lists,
        );

        true
    }

    fn categorize_msg(&self, msg: &MixedMessage) -> ScriptThreadEventCategory {
        match *msg {
            MixedMessage::FromConstellation(ref inner_msg) => match *inner_msg {
                ScriptThreadMessage::SendInputEvent(..) => ScriptThreadEventCategory::InputEvent,
                _ => ScriptThreadEventCategory::ConstellationMsg,
            },
            MixedMessage::FromDevtools(_) => ScriptThreadEventCategory::DevtoolsMsg,
            MixedMessage::FromImageCache(_) => ScriptThreadEventCategory::ImageCacheMsg,
            MixedMessage::FromScript(ref inner_msg) => match *inner_msg {
                MainThreadScriptMsg::Common(CommonScriptMsg::Task(category, ..)) => category,
                MainThreadScriptMsg::RegisterPaintWorklet { .. } => {
                    ScriptThreadEventCategory::WorkletEvent
                },
                _ => ScriptThreadEventCategory::ScriptEvent,
            },
            #[cfg(feature = "webgpu")]
            MixedMessage::FromWebGPUServer(_) => ScriptThreadEventCategory::WebGPUMsg,
            MixedMessage::TimerFired => ScriptThreadEventCategory::TimerEvent,
        }
    }

    fn profile_event<F, R>(
        &self,
        category: ScriptThreadEventCategory,
        pipeline_id: Option<PipelineId>,
        f: F,
    ) -> R
    where
        F: FnOnce() -> R,
    {
        self.background_hang_monitor
            .notify_activity(HangAnnotation::Script(category.into()));
        let start = Instant::now();
        let value = if self.profile_script_events {
            let profiler_chan = self.senders.time_profiler_sender.clone();
            match category {
                ScriptThreadEventCategory::SpawnPipeline => {
                    time_profile!(
                        ProfilerCategory::ScriptSpawnPipeline,
                        None,
                        profiler_chan,
                        f
                    )
                },
                ScriptThreadEventCategory::ConstellationMsg => time_profile!(
                    ProfilerCategory::ScriptConstellationMsg,
                    None,
                    profiler_chan,
                    f
                ),
                ScriptThreadEventCategory::DatabaseAccessEvent => time_profile!(
                    ProfilerCategory::ScriptDatabaseAccessEvent,
                    None,
                    profiler_chan,
                    f
                ),
                ScriptThreadEventCategory::DevtoolsMsg => {
                    time_profile!(ProfilerCategory::ScriptDevtoolsMsg, None, profiler_chan, f)
                },
                ScriptThreadEventCategory::DocumentEvent => time_profile!(
                    ProfilerCategory::ScriptDocumentEvent,
                    None,
                    profiler_chan,
                    f
                ),
                ScriptThreadEventCategory::InputEvent => {
                    time_profile!(ProfilerCategory::ScriptInputEvent, None, profiler_chan, f)
                },
                ScriptThreadEventCategory::FileRead => {
                    time_profile!(ProfilerCategory::ScriptFileRead, None, profiler_chan, f)
                },
                ScriptThreadEventCategory::FontLoading => {
                    time_profile!(ProfilerCategory::ScriptFontLoading, None, profiler_chan, f)
                },
                ScriptThreadEventCategory::FormPlannedNavigation => time_profile!(
                    ProfilerCategory::ScriptPlannedNavigation,
                    None,
                    profiler_chan,
                    f
                ),
                ScriptThreadEventCategory::GeolocationEvent => {
                    time_profile!(
                        ProfilerCategory::ScriptGeolocationEvent,
                        None,
                        profiler_chan,
                        f
                    )
                },
                ScriptThreadEventCategory::NavigationAndTraversalEvent => {
                    time_profile!(
                        ProfilerCategory::ScriptNavigationAndTraversalEvent,
                        None,
                        profiler_chan,
                        f
                    )
                },
                ScriptThreadEventCategory::ImageCacheMsg => time_profile!(
                    ProfilerCategory::ScriptImageCacheMsg,
                    None,
                    profiler_chan,
                    f
                ),
                ScriptThreadEventCategory::NetworkEvent => {
                    time_profile!(ProfilerCategory::ScriptNetworkEvent, None, profiler_chan, f)
                },
                ScriptThreadEventCategory::PortMessage => {
                    time_profile!(ProfilerCategory::ScriptPortMessage, None, profiler_chan, f)
                },
                ScriptThreadEventCategory::Resize => {
                    time_profile!(ProfilerCategory::ScriptResize, None, profiler_chan, f)
                },
                ScriptThreadEventCategory::ScriptEvent => {
                    time_profile!(ProfilerCategory::ScriptEvent, None, profiler_chan, f)
                },
                ScriptThreadEventCategory::SetScrollState => time_profile!(
                    ProfilerCategory::ScriptSetScrollState,
                    None,
                    profiler_chan,
                    f
                ),
                ScriptThreadEventCategory::UpdateReplacedElement => time_profile!(
                    ProfilerCategory::ScriptUpdateReplacedElement,
                    None,
                    profiler_chan,
                    f
                ),
                ScriptThreadEventCategory::StylesheetLoad => time_profile!(
                    ProfilerCategory::ScriptStylesheetLoad,
                    None,
                    profiler_chan,
                    f
                ),
                ScriptThreadEventCategory::SetViewport => {
                    time_profile!(ProfilerCategory::ScriptSetViewport, None, profiler_chan, f)
                },
                ScriptThreadEventCategory::TimerEvent => {
                    time_profile!(ProfilerCategory::ScriptTimerEvent, None, profiler_chan, f)
                },
                ScriptThreadEventCategory::WebSocketEvent => time_profile!(
                    ProfilerCategory::ScriptWebSocketEvent,
                    None,
                    profiler_chan,
                    f
                ),
                ScriptThreadEventCategory::WorkerEvent => {
                    time_profile!(ProfilerCategory::ScriptWorkerEvent, None, profiler_chan, f)
                },
                ScriptThreadEventCategory::WorkletEvent => {
                    time_profile!(ProfilerCategory::ScriptWorkletEvent, None, profiler_chan, f)
                },
                ScriptThreadEventCategory::ServiceWorkerEvent => time_profile!(
                    ProfilerCategory::ScriptServiceWorkerEvent,
                    None,
                    profiler_chan,
                    f
                ),
                ScriptThreadEventCategory::EnterFullscreen => time_profile!(
                    ProfilerCategory::ScriptEnterFullscreen,
                    None,
                    profiler_chan,
                    f
                ),
                ScriptThreadEventCategory::ExitFullscreen => time_profile!(
                    ProfilerCategory::ScriptExitFullscreen,
                    None,
                    profiler_chan,
                    f
                ),
                ScriptThreadEventCategory::PerformanceTimelineTask => time_profile!(
                    ProfilerCategory::ScriptPerformanceEvent,
                    None,
                    profiler_chan,
                    f
                ),
                ScriptThreadEventCategory::Rendering => {
                    time_profile!(ProfilerCategory::ScriptRendering, None, profiler_chan, f)
                },
                #[cfg(feature = "webgpu")]
                ScriptThreadEventCategory::WebGPUMsg => {
                    time_profile!(ProfilerCategory::ScriptWebGPUMsg, None, profiler_chan, f)
                },
            }
        } else {
            f()
        };
        let task_duration = start.elapsed();
        for (doc_id, doc) in self.documents.borrow().iter() {
            if let Some(pipeline_id) = pipeline_id &&
                pipeline_id == doc_id &&
                task_duration.as_nanos() > MAX_TASK_NS
            {
                if opts::get()
                    .debug
                    .is_enabled(DiagnosticsLoggingOption::ProgressiveWebMetrics)
                {
                    println!(
                        "Task took longer than max allowed ({category:?}) {:?}",
                        task_duration.as_nanos()
                    );
                }
                doc.start_tti();
            }
            doc.record_tti_if_necessary();
        }
        value
    }

    fn handle_msg_from_constellation(
        &self,
        msg: ScriptThreadMessage,
        cx: &mut js::context::JSContext,
    ) {
        match msg {
            ScriptThreadMessage::StopDelayingLoadEventsMode(pipeline_id) => {
                self.handle_stop_delaying_load_events_mode(pipeline_id)
            },
            ScriptThreadMessage::NavigateIframe(
                parent_pipeline_id,
                browsing_context_id,
                load_data,
                history_handling,
                target_snapshot_params,
            ) => self.handle_navigate_iframe(
                parent_pipeline_id,
                browsing_context_id,
                load_data,
                history_handling,
                target_snapshot_params,
                cx,
            ),
            ScriptThreadMessage::UnloadDocument(pipeline_id) => {
                self.handle_unload_document(cx, pipeline_id)
            },
            ScriptThreadMessage::ResizeInactive(id, new_size) => {
                self.handle_resize_inactive_msg(id, new_size)
            },
            ScriptThreadMessage::ThemeChange(_, theme) => {
                self.handle_theme_change_msg(theme);
            },
            ScriptThreadMessage::GetDocumentOrigin(pipeline_id, result_sender) => {
                self.handle_get_document_origin(pipeline_id, result_sender);
            },
            ScriptThreadMessage::GetTitle(pipeline_id) => self.handle_get_title_msg(pipeline_id),
            ScriptThreadMessage::SetDocumentActivity(pipeline_id, activity) => {
                self.handle_set_document_activity_msg(cx, pipeline_id, activity)
            },
            ScriptThreadMessage::SetThrottled(webview_id, pipeline_id, throttled) => {
                self.handle_set_throttled_msg(webview_id, pipeline_id, throttled)
            },
            ScriptThreadMessage::SetThrottledInContainingIframe(
                _,
                parent_pipeline_id,
                browsing_context_id,
                throttled,
            ) => self.handle_set_throttled_in_containing_iframe_msg(
                parent_pipeline_id,
                browsing_context_id,
                throttled,
            ),
            ScriptThreadMessage::PostMessage {
                target: target_pipeline_id,
                source_webview,
                source_with_ancestry,
                target_origin: origin,
                source_origin,
                data,
            } => self.handle_post_message_msg(
                cx,
                target_pipeline_id,
                source_webview,
                source_with_ancestry,
                origin,
                source_origin,
                *data,
            ),
            ScriptThreadMessage::UpdatePipelineId(
                parent_pipeline_id,
                browsing_context_id,
                webview_id,
                new_pipeline_id,
                reason,
            ) => self.handle_update_pipeline_id(
                parent_pipeline_id,
                browsing_context_id,
                webview_id,
                new_pipeline_id,
                reason,
                cx,
            ),
            ScriptThreadMessage::UpdateHistoryState(pipeline_id, history_state_id, url) => {
                self.handle_update_history_state_msg(cx, pipeline_id, history_state_id, url)
            },
            ScriptThreadMessage::RemoveHistoryStates(pipeline_id, history_states) => {
                self.handle_remove_history_states(cx, pipeline_id, history_states)
            },
            ScriptThreadMessage::FocusDocumentAsPartOfFocusingSteps(
                pipeline_id,
                sequence,
                iframe_browsing_context_id,
            ) => self.handle_focus_document_as_part_of_focusing_steps(
                cx,
                pipeline_id,
                sequence,
                iframe_browsing_context_id,
            ),
            ScriptThreadMessage::UnfocusDocumentAsPartOfFocusingSteps(pipeline_id, sequence) => {
                self.handle_unfocus_document_as_part_of_focusing_steps(cx, pipeline_id, sequence);
            },
            ScriptThreadMessage::FocusDocument(pipeline_id, remote_focus_operation) => {
                self.handle_focus_document(cx, pipeline_id, remote_focus_operation);
            },
            ScriptThreadMessage::WebDriverScriptCommand(pipeline_id, msg) => {
                self.handle_webdriver_msg(pipeline_id, msg, cx)
            },
            ScriptThreadMessage::WebFontLoadFinished(pipeline_id, event) => {
                // If the font load did not succeed then this message only serves to bump the script thread
                // so it attempts to resolve the document.fonts.ready promise. This happens as a result
                // of processing this message, so there's nothing more to do.
                if event == WebFontLoadEvent::LoadedSuccessfully {
                    self.handle_web_font_loaded(cx.no_gc(), pipeline_id)
                }
            },
            ScriptThreadMessage::DispatchIFrameLoadEvent {
                target: browsing_context_id,
                parent: parent_id,
                child: child_id,
            } => self.handle_iframe_load_event(parent_id, browsing_context_id, child_id, cx),
            ScriptThreadMessage::DispatchStorageEvent(
                pipeline_id,
                storage,
                url,
                key,
                old_value,
                new_value,
            ) => {
                self.handle_storage_event(pipeline_id, storage, url, key, old_value, new_value, cx)
            },
            ScriptThreadMessage::ReportCSSError(pipeline_id, filename, line, column, msg) => {
                self.handle_css_error_reporting(pipeline_id, filename, line, column, msg)
            },
            ScriptThreadMessage::Reload(pipeline_id) => self.handle_reload(pipeline_id, cx),
            ScriptThreadMessage::Resize(id, size, size_type) => {
                self.handle_resize_message(id, size, size_type);
            },
            ScriptThreadMessage::ExitPipeline(
                webview_id,
                pipeline_id,
                discard_browsing_context,
            ) => {
                self.handle_exit_pipeline_msg(webview_id, pipeline_id, discard_browsing_context, cx)
            },
            ScriptThreadMessage::PaintMetric(
                pipeline_id,
                metric_type,
                metric_value,
                first_reflow,
            ) => self.handle_paint_metric(cx, pipeline_id, metric_type, metric_value, first_reflow),
            ScriptThreadMessage::MediaSessionAction(pipeline_id, action) => {
                self.handle_media_session_action(cx, pipeline_id, action)
            },
            ScriptThreadMessage::SendInputEvent(webview_id, id, event) => {
                self.handle_input_event(webview_id, id, event)
            },
            #[cfg(feature = "webgpu")]
            ScriptThreadMessage::SetWebGPUPort(port) => {
                *self.receivers.webgpu_receiver.borrow_mut() = port.route_preserving_errors();
            },
            ScriptThreadMessage::TickAllAnimations(_webviews) => {
                if renderer_may_drive_rendering(&self.document_clock) {
                    self.set_needs_rendering_update();
                }
            },
            ScriptThreadMessage::NoLongerWaitingOnAsychronousImageUpdates(pipeline_id) => {
                if let Some(document) = self.documents.borrow().find_document(pipeline_id) {
                    document.handle_no_longer_waiting_on_asynchronous_image_updates();
                }
            },
            msg @ ScriptThreadMessage::SpawnPipeline(..) |
            msg @ ScriptThreadMessage::ExitFullScreen(..) |
            msg @ ScriptThreadMessage::ExitScriptThread => {
                panic!("should have handled {:?} already", msg)
            },
            ScriptThreadMessage::SetScrollStates(pipeline_id, scroll_states) => {
                self.handle_set_scroll_states(pipeline_id, scroll_states)
            },
            ScriptThreadMessage::EvaluateJavaScript(
                webview_id,
                pipeline_id,
                evaluation_id,
                script,
            ) => {
                self.handle_evaluate_javascript(webview_id, pipeline_id, evaluation_id, script, cx);
            },
            ScriptThreadMessage::SendImageKeysBatch(pipeline_id, image_keys) => {
                if let Some(window) = self.documents.borrow().find_window(pipeline_id) {
                    window
                        .image_cache()
                        .dispatch_fill_key_cache_with_batch_of_keys(image_keys);
                } else {
                    warn!(
                        "Could not find window corresponding to an image cache to send image keys to pipeline {:?}",
                        pipeline_id
                    );
                }
            },
            ScriptThreadMessage::RefreshCursor(pipeline_id) => {
                self.handle_refresh_cursor(pipeline_id);
            },
            ScriptThreadMessage::PreferencesUpdated(updates) => {
                let mut current_preferences = prefs::get().clone();
                for (name, value) in updates {
                    current_preferences.set_value(&name, value);
                }
                prefs::set(current_preferences);
            },
            ScriptThreadMessage::ForwardKeyboardScroll(pipeline_id, scroll) => {
                if let Some(document) = self.documents.borrow().find_document(pipeline_id) {
                    document.event_handler().do_keyboard_scroll(cx, scroll);
                }
            },
            ScriptThreadMessage::RequestScreenshotReadiness(webview_id, pipeline_id) => {
                self.handle_request_screenshot_readiness(webview_id, pipeline_id, cx);
            },
            ScriptThreadMessage::EmbedderControlResponse(id, response) => {
                self.handle_embedder_control_response(id, response, cx);
            },
            ScriptThreadMessage::SetUserContents(user_content_manager_id, user_contents) => {
                self.user_contents_for_manager_id.borrow_mut().insert(
                    user_content_manager_id,
                    ScriptThreadUserContents::new(user_contents, &self.shared_style_locks),
                );
            },
            ScriptThreadMessage::DestroyUserContentManager(user_content_manager_id) => {
                self.user_contents_for_manager_id
                    .borrow_mut()
                    .remove(&user_content_manager_id);
            },
            ScriptThreadMessage::UpdatePinchZoomInfos(id, pinch_zoom_infos) => {
                self.handle_update_pinch_zoom_infos(cx, id, pinch_zoom_infos);
            },
            ScriptThreadMessage::SetAccessibilityActive(pipeline_id, active, epoch) => {
                self.set_accessibility_active(pipeline_id, active, epoch);
            },
            ScriptThreadMessage::TriggerGarbageCollection => unsafe {
                JS_GC(cx, GCReason::API);
            },
        }
    }

    fn handle_set_scroll_states(&self, pipeline_id: PipelineId, scroll_states: ScrollStateUpdate) {
        let Some(window) = self.documents.borrow().find_window(pipeline_id) else {
            warn!("Received scroll states for closed pipeline {pipeline_id}");
            return;
        };

        self.profile_event(
            ScriptThreadEventCategory::SetScrollState,
            Some(pipeline_id),
            || {
                window
                    .layout_mut()
                    .set_scroll_offsets_from_renderer(&scroll_states.offsets);
            },
        );

        window
            .Document()
            .event_handler()
            .handle_embedder_scroll_event(scroll_states.scrolled_node);
    }

    #[cfg(feature = "webgpu")]
    fn handle_msg_from_webgpu_server(&self, msg: WebGPUMsg, cx: &mut js::context::JSContext) {
        match msg {
            WebGPUMsg::FreeAdapter(id) => self.gpu_id_hub.free_adapter_id(id),
            WebGPUMsg::FreeDevice {
                device_id,
                pipeline_id,
            } => {
                self.gpu_id_hub.free_device_id(device_id);
                if let Some(global) = self.documents.borrow().find_global(pipeline_id) {
                    global.remove_gpu_device(WebGPUDevice(device_id));
                } // page can already be destroyed
            },
            WebGPUMsg::FreeBuffer(id) => self.gpu_id_hub.free_buffer_id(id),
            WebGPUMsg::FreePipelineLayout(id) => self.gpu_id_hub.free_pipeline_layout_id(id),
            WebGPUMsg::FreeComputePipeline(id) => self.gpu_id_hub.free_compute_pipeline_id(id),
            WebGPUMsg::FreeBindGroup(id) => self.gpu_id_hub.free_bind_group_id(id),
            WebGPUMsg::FreeBindGroupLayout(id) => self.gpu_id_hub.free_bind_group_layout_id(id),
            WebGPUMsg::FreeCommandBuffer(id) => self.gpu_id_hub.free_command_buffer_id(id),
            WebGPUMsg::FreeSampler(id) => self.gpu_id_hub.free_sampler_id(id),
            WebGPUMsg::FreeShaderModule(id) => self.gpu_id_hub.free_shader_module_id(id),
            WebGPUMsg::FreeRenderBundle(id) => self.gpu_id_hub.free_render_bundle_id(id),
            WebGPUMsg::FreeRenderPipeline(id) => self.gpu_id_hub.free_render_pipeline_id(id),
            WebGPUMsg::FreeTexture(id) => self.gpu_id_hub.free_texture_id(id),
            WebGPUMsg::FreeTextureView(id) => self.gpu_id_hub.free_texture_view_id(id),
            WebGPUMsg::FreeComputePass(id) => self.gpu_id_hub.free_compute_pass_id(id),
            WebGPUMsg::FreeRenderPass(id) => self.gpu_id_hub.free_render_pass_id(id),
            WebGPUMsg::Exit => {
                *self.receivers.webgpu_receiver.borrow_mut() = crossbeam_channel::never()
            },
            WebGPUMsg::DeviceLost {
                pipeline_id,
                device,
                reason,
                msg,
            } => {
                let global = self.documents.borrow().find_global(pipeline_id).unwrap();
                let _ac = enter_auto_realm(cx, &*global);
                global.gpu_device_lost(device, reason, msg);
            },
            WebGPUMsg::UncapturedError {
                device,
                pipeline_id,
                error,
            } => {
                let global = self.documents.borrow().find_global(pipeline_id).unwrap();
                let _ac = enter_auto_realm(cx, &*global);
                global.handle_uncaptured_gpu_error(device, error);
            },
            _ => {},
        }
    }

    fn handle_msg_from_script(&self, msg: MainThreadScriptMsg, cx: &mut js::context::JSContext) {
        match msg {
            MainThreadScriptMsg::Common(CommonScriptMsg::Task(_, task, pipeline_id, _)) => {
                if self.document_producer_fence.is_some() &&
                    pipeline_id.is_some_and(|pipeline_id| {
                        self.closed_pipelines.borrow().contains(&pipeline_id)
                    })
                {
                    return;
                }
                let global = pipeline_id.and_then(|id| self.documents.borrow().find_global(id));
                match global {
                    None => task.run_box(cx),
                    Some(global) => {
                        let mut realm = enter_auto_realm(cx, &*global);
                        let cx = &mut realm.current_realm();
                        task.run_box(cx)
                    },
                }
            },
            MainThreadScriptMsg::Common(CommonScriptMsg::CollectReports(chan)) => {
                self.collect_reports(cx, chan)
            },
            MainThreadScriptMsg::Common(CommonScriptMsg::ReportCspViolations(
                pipeline_id,
                violations,
            )) => {
                if let Some(global) = self.documents.borrow().find_global(pipeline_id) {
                    let mut realm = enter_auto_realm(cx, &*global);
                    let cx = &mut realm.current_realm();
                    global.run_worker_csp_violation_report_tasks(violations, cx);
                }
            },
            MainThreadScriptMsg::NavigationResponse {
                pipeline_id,
                response,
            } => {
                let (message, producer_guard) = (*response).into_parts();
                if self.document_producer_fence.is_some() &&
                    self.closed_pipelines.borrow().contains(&pipeline_id)
                {
                    // Controlled teardown establishes this tombstone before document destruction.
                    // A queued redirect must not resurrect network work for the closed pipeline.
                    return;
                }
                self.handle_navigation_response(cx, pipeline_id, message);
                drop(producer_guard);
            },
            MainThreadScriptMsg::WorkletLoaded(pipeline_id) => {
                self.handle_worklet_loaded(pipeline_id)
            },
            MainThreadScriptMsg::RegisterPaintWorklet {
                pipeline_id,
                name,
                properties,
                painter,
            } => self.handle_register_paint_worklet(pipeline_id, name, properties, painter),
            MainThreadScriptMsg::Inactive => {},
            MainThreadScriptMsg::WakeUp => {},
            MainThreadScriptMsg::ForwardEmbedderControlResponseFromFileManager(
                control_id,
                response,
            ) => {
                self.handle_embedder_control_response(control_id, response, cx);
            },
        }
    }

    fn handle_msg_from_devtools(
        &self,
        msg: DevtoolScriptControlMsg,
        cx: &mut js::context::JSContext,
    ) {
        let documents = self.documents.borrow();
        match msg {
            DevtoolScriptControlMsg::GetEventListenerInfo(id, node, reply) => {
                devtools::handle_get_event_listener_info(&self.devtools_state, id, &node, reply)
            },
            DevtoolScriptControlMsg::GetRootNode(id, reply) => {
                devtools::handle_get_root_node(cx, &self.devtools_state, &documents, id, reply)
            },
            DevtoolScriptControlMsg::GetDocumentElement(id, reply) => {
                devtools::handle_get_document_element(
                    cx,
                    &self.devtools_state,
                    &documents,
                    id,
                    reply,
                )
            },
            DevtoolScriptControlMsg::GetStyleSheets(id, reply) => {
                devtools::handle_get_stylesheets(cx, &documents, id, reply);
            },
            DevtoolScriptControlMsg::GetStyleSheetText(id, index, reply) => {
                devtools::handle_get_stylesheet_text(cx, &documents, id, index, reply);
            },
            DevtoolScriptControlMsg::GetChildren(id, node_id, reply) => {
                devtools::handle_get_children(cx, &self.devtools_state, id, &node_id, reply)
            },
            DevtoolScriptControlMsg::GetAttributeStyle(id, node_id, reply) => {
                devtools::handle_get_attribute_style(cx, &self.devtools_state, id, &node_id, reply)
            },
            DevtoolScriptControlMsg::GetStylesheetStyle(id, node_id, matched_rule, reply) => {
                devtools::handle_get_stylesheet_style(
                    cx,
                    &self.devtools_state,
                    &documents,
                    id,
                    &node_id,
                    matched_rule,
                    reply,
                )
            },
            DevtoolScriptControlMsg::GetSelectors(id, node_id, reply) => {
                devtools::handle_get_selectors(
                    cx,
                    &self.devtools_state,
                    &documents,
                    id,
                    &node_id,
                    reply,
                )
            },
            DevtoolScriptControlMsg::GetComputedStyle(id, node_id, reply) => {
                devtools::handle_get_computed_style(cx, &self.devtools_state, id, &node_id, reply)
            },
            DevtoolScriptControlMsg::GetLayout(id, node_id, reply) => {
                devtools::handle_get_layout(cx, &self.devtools_state, id, &node_id, reply)
            },
            DevtoolScriptControlMsg::GetXPath(id, node_id, reply) => {
                devtools::handle_get_xpath(&self.devtools_state, id, &node_id, reply)
            },
            DevtoolScriptControlMsg::GetInnerOrOuterHTML(id, node_id, reply, html_type) => {
                devtools::handle_get_inner_or_outer_html(
                    cx,
                    &self.devtools_state,
                    id,
                    &node_id,
                    reply,
                    html_type,
                )
            },
            DevtoolScriptControlMsg::ModifyAttribute(id, node_id, modifications) => {
                devtools::handle_modify_attribute(
                    cx,
                    &self.devtools_state,
                    &documents,
                    id,
                    &node_id,
                    modifications,
                )
            },
            DevtoolScriptControlMsg::ModifyRule(id, node_id, modifications) => {
                devtools::handle_modify_rule(
                    cx,
                    &self.devtools_state,
                    &documents,
                    id,
                    &node_id,
                    modifications,
                )
            },
            DevtoolScriptControlMsg::WantsLiveNotifications(id, to_send) => {
                match documents.find_window(id) {
                    Some(window) => {
                        window.set_devtools_wants_updates(to_send);
                    },
                    None => warn!("Message sent to closed pipeline {}.", id),
                }
            },
            DevtoolScriptControlMsg::SetTimelineMarkers(id, marker_types, reply) => {
                devtools::handle_set_timeline_markers(&documents, id, marker_types, reply)
            },
            DevtoolScriptControlMsg::DropTimelineMarkers(id, marker_types) => {
                devtools::handle_drop_timeline_markers(&documents, id, marker_types)
            },
            DevtoolScriptControlMsg::RequestAnimationFrame(id, name) => {
                devtools::handle_request_animation_frame(&documents, id, name)
            },
            DevtoolScriptControlMsg::NavigateTo(pipeline_id, url) => {
                self.handle_navigate_to(pipeline_id, url)
            },
            DevtoolScriptControlMsg::GoBack(pipeline_id) => {
                self.handle_traverse_history(pipeline_id, TraversalDirection::Back(1))
            },
            DevtoolScriptControlMsg::GoForward(pipeline_id) => {
                self.handle_traverse_history(pipeline_id, TraversalDirection::Forward(1))
            },
            DevtoolScriptControlMsg::Reload(id) => self.handle_reload(id, cx),
            DevtoolScriptControlMsg::GetCssDatabase(reply) => {
                devtools::handle_get_css_database(reply)
            },
            DevtoolScriptControlMsg::SimulateColorScheme(id, theme) => {
                match documents.find_window(id) {
                    Some(window) => {
                        window.set_embedder_theme(theme);
                    },
                    None => warn!("Message sent to closed pipeline {}.", id),
                }
            },
            DevtoolScriptControlMsg::HighlightDomNode(id, node_id) => {
                devtools::handle_highlight_dom_node(
                    &self.devtools_state,
                    &documents,
                    id,
                    node_id.as_deref(),
                )
            },
            DevtoolScriptControlMsg::Eval(code, id, frame_actor_id, reply) => {
                self.debugger_global
                    .fire_eval(cx, code.into(), id, None, frame_actor_id, reply);
            },
            DevtoolScriptControlMsg::GetPossibleBreakpoints(spidermonkey_id, result_sender) => {
                self.debugger_global.fire_get_possible_breakpoints(
                    cx,
                    spidermonkey_id,
                    result_sender,
                );
            },
            DevtoolScriptControlMsg::SetBreakpoint(spidermonkey_id, script_id, offset) => {
                self.debugger_global
                    .fire_set_breakpoint(cx, spidermonkey_id, script_id, offset);
            },
            DevtoolScriptControlMsg::ClearBreakpoint(spidermonkey_id, script_id, offset) => {
                self.debugger_global
                    .fire_clear_breakpoint(cx, spidermonkey_id, script_id, offset);
            },
            DevtoolScriptControlMsg::Interrupt => {
                self.debugger_global.fire_interrupt(cx);
            },
            DevtoolScriptControlMsg::ListFrames(pipeline_id, start, count, result_sender) => {
                self.debugger_global
                    .fire_list_frames(cx, pipeline_id, start, count, result_sender);
            },
            DevtoolScriptControlMsg::GetEnvironment(request, result_sender) => {
                self.debugger_global
                    .fire_get_environment(cx, request, result_sender);
            },
            DevtoolScriptControlMsg::Resume(resume_limit_type, frame_actor_id) => {
                self.debugger_global
                    .fire_resume(cx, resume_limit_type, frame_actor_id);
                self.debugger_paused.set(false);
            },
            DevtoolScriptControlMsg::Blackbox(spidermonkey_id, coverage) => {
                self.debugger_global
                    .fire_blackbox(cx, spidermonkey_id, coverage);
            },
            DevtoolScriptControlMsg::Unblackbox(spidermonkey_id, coverage) => {
                self.debugger_global
                    .fire_unblackbox(cx, spidermonkey_id, coverage);
            },
        }
    }

    /// Enter a nested event loop for debugger pause.
    /// TODO: This should also be called when manual pause is triggered.
    pub(crate) fn enter_debugger_pause_loop(&self) {
        if self.document_clock.is_controlled() {
            // A debugger pause owns a nested event loop which can run an unbounded number of
            // devtools evaluations outside DriveOneTurn authority. Controlled mode therefore
            // refuses to enter it and latches the unsupported coverage loss for every later
            // pending observation.
            self.controlled_debugger_unsupported.set(true);
            return;
        }
        self.debugger_paused.set(true);

        #[allow(unsafe_code)]
        let mut cx = unsafe { js::context::JSContext::from_ptr(js::rust::Runtime::get().unwrap()) };

        while self.debugger_paused.get() {
            match self.receivers.devtools_server_receiver.recv() {
                Ok(Ok(msg)) => self.handle_msg_from_devtools(msg, &mut cx),
                _ => {
                    self.debugger_paused.set(false);
                    break;
                },
            }
        }
    }

    fn handle_msg_from_image_cache(
        &self,
        transport: ImageCacheMessage,
        cx: &mut js::context::JSContext,
    ) {
        let (response, delivery, message_guard) = match transport {
            ImageCacheMessage::Baseline(response) => {
                (response, ImageCallbackDelivery::Baseline, None)
            },
            ImageCacheMessage::ControlledV2(envelope) => {
                let (response, guard) = envelope.into_parts();
                let guard = guard.expect("controlled image transport requires an Image guard");
                let pipeline_id = match &response {
                    ImageCacheResponseMessage::NotifyPendingImageLoadStatus(response) => {
                        response.pipeline_id
                    },
                    ImageCacheResponseMessage::VectorImageRasterizationComplete(response) => {
                        response.pipeline_id
                    },
                };
                let window = self.documents.borrow().find_window(pipeline_id);
                let pipeline_tombstoned = self.closed_pipelines.borrow().contains(&pipeline_id);
                let window =
                    match controlled_image_delivery_target(window.is_some(), pipeline_tombstoned) {
                        ControlledImageDeliveryTarget::Live => {
                            window.expect("live controlled image delivery requires a Window")
                        },
                        ControlledImageDeliveryTarget::Retired => {
                            // Pipeline teardown established the tombstone before removing this Window,
                            // so the queued response has no remaining mutation target.
                            drop(guard);
                            return;
                        },
                        ControlledImageDeliveryTarget::Unknown => {
                            // A missing untombstoned target or a live tombstoned target violates the
                            // ScriptThread routing invariant and is not an owned cancellation.
                            let _ = guard.abandon();
                            return;
                        },
                    };
                if self.document_control_profile != DocumentControlProfile::TopLevelSession ||
                    self.document_execution_profile !=
                        DocumentExecutionProfile::ControlledWebSessionV2 ||
                    !Self::current_controlled_top_level_target_matches(&window)
                {
                    let _ = guard.abandon();
                    return;
                }
                let Ok(completion_time) = window.sample_controlled_v2_document_performance_time()
                else {
                    let _ = guard.abandon();
                    return;
                };
                (
                    response,
                    ImageCallbackDelivery::ControlledV2Fenced { completion_time },
                    Some(guard),
                )
            },
        };
        let _message_completion = ControlledImageMessageCompletion::new(message_guard);

        let _retained_pending_state = match response {
            ImageCacheResponseMessage::NotifyPendingImageLoadStatus(pending_image_response) => {
                let window = self
                    .documents
                    .borrow()
                    .find_window(pending_image_response.pipeline_id);
                if let Some(ref window) = window {
                    window.pending_image_notification(pending_image_response, delivery, cx)
                } else {
                    Ok(())
                }
            },
            ImageCacheResponseMessage::VectorImageRasterizationComplete(response) => {
                let window = self.documents.borrow().find_window(response.pipeline_id);
                if let Some(ref window) = window {
                    window.handle_image_rasterization_complete_notification(response, delivery, cx)
                } else {
                    Ok(())
                }
            },
        };
        // Delivery reached the owning event loop. A handler `Err` preserves the rejected
        // provenance/key in Window's pending collections, where settlement reports it as typed
        // unsupported work. It is not a lost producer handoff.
    }

    fn handle_webdriver_msg(
        &self,
        pipeline_id: PipelineId,
        msg: WebDriverScriptCommand,
        cx: &mut js::context::JSContext,
    ) {
        let documents = self.documents.borrow();
        match msg {
            WebDriverScriptCommand::AddCookie(params, reply) => {
                webdriver_handlers::handle_add_cookie(&documents, pipeline_id, params, reply)
            },
            WebDriverScriptCommand::DeleteCookies(reply) => {
                webdriver_handlers::handle_delete_cookies(&documents, pipeline_id, reply)
            },
            WebDriverScriptCommand::DeleteCookie(name, reply) => {
                webdriver_handlers::handle_delete_cookie(&documents, pipeline_id, name, reply)
            },
            WebDriverScriptCommand::ElementClear(element_id, reply) => {
                webdriver_handlers::handle_element_clear(
                    cx,
                    &documents,
                    pipeline_id,
                    element_id,
                    reply,
                )
            },
            WebDriverScriptCommand::FindElementsCSSSelector(selector, reply) => {
                webdriver_handlers::handle_find_elements_css_selector(
                    cx,
                    &documents,
                    pipeline_id,
                    selector,
                    reply,
                )
            },
            WebDriverScriptCommand::FindElementsLinkText(selector, partial, reply) => {
                webdriver_handlers::handle_find_elements_link_text(
                    cx,
                    &documents,
                    pipeline_id,
                    selector,
                    partial,
                    reply,
                )
            },
            WebDriverScriptCommand::FindElementsTagName(selector, reply) => {
                webdriver_handlers::handle_find_elements_tag_name(
                    cx,
                    &documents,
                    pipeline_id,
                    selector,
                    reply,
                )
            },
            WebDriverScriptCommand::FindElementsXpathSelector(selector, reply) => {
                webdriver_handlers::handle_find_elements_xpath_selector(
                    cx,
                    &documents,
                    pipeline_id,
                    selector,
                    reply,
                )
            },
            WebDriverScriptCommand::FindElementElementsCSSSelector(selector, element_id, reply) => {
                webdriver_handlers::handle_find_element_elements_css_selector(
                    cx,
                    &documents,
                    pipeline_id,
                    element_id,
                    selector,
                    reply,
                )
            },
            WebDriverScriptCommand::FindElementElementsLinkText(
                selector,
                element_id,
                partial,
                reply,
            ) => webdriver_handlers::handle_find_element_elements_link_text(
                cx,
                &documents,
                pipeline_id,
                element_id,
                selector,
                partial,
                reply,
            ),
            WebDriverScriptCommand::FindElementElementsTagName(selector, element_id, reply) => {
                webdriver_handlers::handle_find_element_elements_tag_name(
                    cx,
                    &documents,
                    pipeline_id,
                    element_id,
                    selector,
                    reply,
                )
            },
            WebDriverScriptCommand::FindElementElementsXPathSelector(
                selector,
                element_id,
                reply,
            ) => webdriver_handlers::handle_find_element_elements_xpath_selector(
                cx,
                &documents,
                pipeline_id,
                element_id,
                selector,
                reply,
            ),
            WebDriverScriptCommand::FindShadowElementsCSSSelector(
                selector,
                shadow_root_id,
                reply,
            ) => webdriver_handlers::handle_find_shadow_elements_css_selector(
                cx,
                &documents,
                pipeline_id,
                shadow_root_id,
                selector,
                reply,
            ),
            WebDriverScriptCommand::FindShadowElementsLinkText(
                selector,
                shadow_root_id,
                partial,
                reply,
            ) => webdriver_handlers::handle_find_shadow_elements_link_text(
                cx,
                &documents,
                pipeline_id,
                shadow_root_id,
                selector,
                partial,
                reply,
            ),
            WebDriverScriptCommand::FindShadowElementsTagName(selector, shadow_root_id, reply) => {
                webdriver_handlers::handle_find_shadow_elements_tag_name(
                    cx,
                    &documents,
                    pipeline_id,
                    shadow_root_id,
                    selector,
                    reply,
                )
            },
            WebDriverScriptCommand::FindShadowElementsXPathSelector(
                selector,
                shadow_root_id,
                reply,
            ) => webdriver_handlers::handle_find_shadow_elements_xpath_selector(
                cx,
                &documents,
                pipeline_id,
                shadow_root_id,
                selector,
                reply,
            ),
            WebDriverScriptCommand::GetElementShadowRoot(element_id, reply) => {
                webdriver_handlers::handle_get_element_shadow_root(
                    &documents,
                    pipeline_id,
                    element_id,
                    reply,
                )
            },
            WebDriverScriptCommand::ElementClick(element_id, reply) => {
                webdriver_handlers::handle_element_click(
                    cx,
                    &documents,
                    pipeline_id,
                    element_id,
                    reply,
                )
            },
            WebDriverScriptCommand::GetKnownElement(element_id, reply) => {
                webdriver_handlers::handle_get_known_element(
                    &documents,
                    pipeline_id,
                    element_id,
                    reply,
                )
            },
            WebDriverScriptCommand::GetKnownWindow(webview_id, reply) => {
                webdriver_handlers::handle_get_known_window(
                    &documents,
                    pipeline_id,
                    webview_id,
                    reply,
                )
            },
            WebDriverScriptCommand::GetKnownShadowRoot(element_id, reply) => {
                webdriver_handlers::handle_get_known_shadow_root(
                    &documents,
                    pipeline_id,
                    element_id,
                    reply,
                )
            },
            WebDriverScriptCommand::GetActiveElement(reply) => {
                webdriver_handlers::handle_get_active_element(&documents, pipeline_id, reply)
            },
            WebDriverScriptCommand::GetComputedRole(node_id, reply) => {
                webdriver_handlers::handle_get_computed_role(
                    &documents,
                    pipeline_id,
                    node_id,
                    reply,
                )
            },
            WebDriverScriptCommand::GetPageSource(reply) => {
                webdriver_handlers::handle_get_page_source(cx, &documents, pipeline_id, reply)
            },
            WebDriverScriptCommand::GetCookies(reply) => {
                webdriver_handlers::handle_get_cookies(&documents, pipeline_id, reply)
            },
            WebDriverScriptCommand::GetCookie(name, reply) => {
                webdriver_handlers::handle_get_cookie(&documents, pipeline_id, name, reply)
            },
            WebDriverScriptCommand::GetElementTagName(node_id, reply) => {
                webdriver_handlers::handle_get_name(&documents, pipeline_id, node_id, reply)
            },
            WebDriverScriptCommand::GetElementAttribute(node_id, name, reply) => {
                webdriver_handlers::handle_get_attribute(
                    cx,
                    &documents,
                    pipeline_id,
                    node_id,
                    name,
                    reply,
                )
            },
            WebDriverScriptCommand::GetElementProperty(node_id, name, reply) => {
                webdriver_handlers::handle_get_property(
                    &documents,
                    pipeline_id,
                    node_id,
                    name,
                    reply,
                    cx,
                )
            },
            WebDriverScriptCommand::GetElementCSS(node_id, name, reply) => {
                webdriver_handlers::handle_get_css(
                    cx,
                    &documents,
                    pipeline_id,
                    node_id,
                    name,
                    reply,
                )
            },
            WebDriverScriptCommand::GetElementRect(node_id, reply) => {
                webdriver_handlers::handle_get_rect(cx, &documents, pipeline_id, node_id, reply)
            },
            WebDriverScriptCommand::ScrollAndGetBoundingClientRect(node_id, reply) => {
                webdriver_handlers::handle_scroll_and_get_bounding_client_rect(
                    cx,
                    &documents,
                    pipeline_id,
                    node_id,
                    reply,
                )
            },
            WebDriverScriptCommand::GetElementText(node_id, reply) => {
                webdriver_handlers::handle_get_text(&documents, pipeline_id, node_id, reply)
            },
            WebDriverScriptCommand::GetElementInViewCenterPoint(node_id, reply) => {
                webdriver_handlers::handle_get_element_in_view_center_point(
                    cx,
                    &documents,
                    pipeline_id,
                    node_id,
                    reply,
                )
            },
            WebDriverScriptCommand::GetParentFrameId(reply) => {
                webdriver_handlers::handle_get_parent_frame_id(&documents, pipeline_id, reply)
            },
            WebDriverScriptCommand::GetBrowsingContextId(webdriver_frame_id, reply) => {
                webdriver_handlers::handle_get_browsing_context_id(
                    &documents,
                    pipeline_id,
                    webdriver_frame_id,
                    reply,
                )
            },
            WebDriverScriptCommand::GetUrl(reply) => {
                webdriver_handlers::handle_get_url(&documents, pipeline_id, reply)
            },
            WebDriverScriptCommand::IsEnabled(element_id, reply) => {
                webdriver_handlers::handle_is_enabled(&documents, pipeline_id, element_id, reply)
            },
            WebDriverScriptCommand::IsSelected(element_id, reply) => {
                webdriver_handlers::handle_is_selected(&documents, pipeline_id, element_id, reply)
            },
            WebDriverScriptCommand::GetTitle(reply) => {
                webdriver_handlers::handle_get_title(&documents, pipeline_id, reply)
            },
            WebDriverScriptCommand::WillSendKeys(
                element_id,
                text,
                strict_file_interactability,
                reply,
            ) => webdriver_handlers::handle_will_send_keys(
                cx,
                &documents,
                pipeline_id,
                element_id,
                text,
                strict_file_interactability,
                reply,
            ),
            WebDriverScriptCommand::AddLoadStatusSender(_, response_sender) => {
                webdriver_handlers::handle_add_load_status_sender(
                    &documents,
                    pipeline_id,
                    response_sender,
                )
            },
            WebDriverScriptCommand::RemoveLoadStatusSender(_) => {
                webdriver_handlers::handle_remove_load_status_sender(&documents, pipeline_id)
            },
            // https://github.com/servo/servo/issues/23535
            // The Script messages need different treatment since the JS script might mutate
            // `self.documents`, which would conflict with the immutable borrow of it that
            // occurs for the rest of the messages.
            // We manually drop the immutable borrow first, and quickly
            // end the borrow of documents to avoid runtime error.
            WebDriverScriptCommand::ExecuteScriptWithCallback(script, reply) => {
                let window = documents.find_window(pipeline_id);
                drop(documents);
                webdriver_handlers::handle_execute_async_script(window, script, reply, cx);
            },
            WebDriverScriptCommand::SetProtocolHandlerAutomationMode(mode) => {
                webdriver_handlers::set_protocol_handler_automation_mode(
                    &documents,
                    pipeline_id,
                    mode,
                )
            },
        }
    }

    /// Batch window resize operations into a single "update the rendering" task,
    /// or, if a load is in progress, set the window size directly.
    pub(crate) fn handle_resize_message(
        &self,
        id: PipelineId,
        viewport_details: ViewportDetails,
        size_type: WindowSizeType,
    ) {
        self.profile_event(ScriptThreadEventCategory::Resize, Some(id), || {
            let window = self.documents.borrow().find_window(id);
            if let Some(ref window) = window {
                window.add_resize_event(viewport_details, size_type);
                return;
            }
            let mut loads = self.incomplete_loads.borrow_mut();
            if let Some(ref mut load) = loads.iter_mut().find(|load| load.pipeline_id == id) {
                load.viewport_details = viewport_details;
            }
        })
    }

    /// Handle changes to the theme, triggering reflow if the theme actually changed.
    fn handle_theme_change_msg(&self, theme: Theme) {
        for (_, document) in self.documents.borrow().iter() {
            document.window().set_embedder_theme(theme);
        }
        let mut loads = self.incomplete_loads.borrow_mut();
        for load in loads.iter_mut() {
            load.embedder_theme = theme;
        }
    }

    fn handle_get_document_origin(
        &self,
        id: PipelineId,
        result_sender: GenericSender<Option<String>>,
    ) {
        let _ = result_sender.send(self.documents.borrow().find_document(id).map(|document| {
            document
                .origin()
                .immutable()
                .ascii_serialization()
                .into_owned()
        }));
    }

    // exit_fullscreen creates a new JS promise object, so we need to have entered a realm
    fn handle_exit_fullscreen(&self, id: PipelineId, cx: &mut js::context::JSContext) {
        let document = self.documents.borrow().find_document(id);
        if let Some(document) = document {
            let mut realm = enter_auto_realm(cx, &*document);
            document.exit_fullscreen(&mut realm);
        }
    }

    pub(crate) fn spawn_pipeline(
        &self,
        cx: &mut js::context::JSContext,
        new_pipeline_info: NewPipelineInfo,
    ) {
        self.profile_event(
            ScriptThreadEventCategory::SpawnPipeline,
            Some(new_pipeline_info.new_pipeline_id),
            || {
                self.devtools_state
                    .notify_pipeline_created(new_pipeline_info.new_pipeline_id);

                // Capture the document-clock origin before navigation begins so redirects and
                // response delivery cannot move the observable navigation origin.
                let document_time_origin = self.document_clock.now();
                self.pre_page_load(
                    cx,
                    InProgressLoad::new(new_pipeline_info, document_time_origin),
                );
            },
        );
    }

    fn collect_reports(&self, cx: &mut js::context::JSContext, reports_chan: ReportsChan) {
        let documents = self.documents.borrow();
        let urls = itertools::join(documents.iter().map(|(_, d)| d.url().to_string()), ", ");

        let mut reports = vec![];
        perform_memory_report(|ops| {
            for (_, document) in documents.iter() {
                document
                    .window()
                    .layout()
                    .collect_reports(&mut reports, ops);
            }

            let prefix = format!("url({urls})");
            reports.extend(get_reports(cx, prefix, ops));
        });

        reports_chan.send(ProcessReports::new(reports));
    }

    /// Updates iframe element after a change in visibility
    fn handle_set_throttled_in_containing_iframe_msg(
        &self,
        parent_pipeline_id: PipelineId,
        browsing_context_id: BrowsingContextId,
        throttled: bool,
    ) {
        let iframe = self
            .documents
            .borrow()
            .find_iframe(parent_pipeline_id, browsing_context_id);
        if let Some(iframe) = iframe {
            iframe.set_throttled(throttled);
        }
    }

    fn handle_set_throttled_msg(
        &self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        throttled: bool,
    ) {
        // Separate message sent since parent script thread could be different (Iframe of different
        // domain)
        self.senders
            .pipeline_to_constellation_sender
            .send((
                webview_id,
                pipeline_id,
                ScriptToConstellationMessage::SetThrottledComplete(throttled),
            ))
            .unwrap();

        let window = self.documents.borrow().find_window(pipeline_id);
        match window {
            Some(window) => {
                window.set_throttled(throttled);
                return;
            },
            None => {
                let mut loads = self.incomplete_loads.borrow_mut();
                if let Some(ref mut load) = loads
                    .iter_mut()
                    .find(|load| load.pipeline_id == pipeline_id)
                {
                    load.throttled = throttled;
                    return;
                }
            },
        }

        warn!("SetThrottled sent to nonexistent pipeline");
    }

    /// Handles activity change message
    fn handle_set_document_activity_msg(
        &self,
        cx: &mut js::context::JSContext,
        id: PipelineId,
        activity: DocumentActivity,
    ) {
        debug!(
            "Setting activity of {} to be {:?} in {:?}.",
            id,
            activity,
            thread::current().name()
        );

        // If a pipeline transitions to fully active, the next turn of the event
        // loop will release any pending tasks targeting that pipeline. To ensure
        // we always run those as soon as possible, not just whenever we happen to
        // receive another event, we make sure the event loop has an event waiting.
        let _ = self.senders.self_sender.send(MainThreadScriptMsg::Inactive);

        let document = self.documents.borrow().find_document(id);
        if let Some(document) = document {
            document.set_activity(cx, activity);
            return;
        }
        let mut loads = self.incomplete_loads.borrow_mut();
        if let Some(ref mut load) = loads.iter_mut().find(|load| load.pipeline_id == id) {
            load.activity = activity;
            return;
        }
        warn!("change of activity sent to nonexistent pipeline");
    }

    fn handle_focus_document_as_part_of_focusing_steps(
        &self,
        cx: &mut js::context::JSContext,
        pipeline_id: PipelineId,
        sequence: FocusSequenceNumber,
        browsing_context_id: Option<BrowsingContextId>,
    ) {
        let Some(document) = self.documents.borrow().find_document(pipeline_id) else {
            warn!("Unknown {pipeline_id:?} for FocusDocumentAsPartOfFocusingSteps message.");
            return;
        };

        let focus_handler = document.focus_handler();
        if focus_handler.focus_sequence() > sequence {
            debug!(
                "Disregarding the FocusDocumentAsPartOfFocusingSteps message because \
                the contained sequence number is too old ({sequence:?} < {:?})",
                focus_handler.focus_sequence()
            );
            return;
        }

        // This is separate from the next few lines in order to drop the borrow
        // on `document.iframes()`.
        let iframe_element = browsing_context_id.and_then(|browsing_context_id| {
            document
                .iframes()
                .get(browsing_context_id)
                .map(|iframe| iframe.element.as_rooted())
        });

        rooted!(&in(cx) let focusable_area = iframe_element
            .map(|iframe_element| FocusableArea::IFrameViewport {
                iframe_element: iframe_element.as_traced(),
                kind: iframe_element
                    .upcast::<Element>()
                    .focusable_area_kind(cx.no_gc()),
            })
            .unwrap_or(FocusableArea::Viewport)
        );

        rooted!(&in(cx) let new_focus_chain = focusable_area.focus_chain());
        rooted!(&in(cx) let old_focus_chain = focus_handler.current_focus_chain());

        focus_handler.focus_update_steps(cx, new_focus_chain, old_focus_chain, &focusable_area);
    }

    fn handle_focus_document(
        &self,
        cx: &mut js::context::JSContext,
        pipeline_id: PipelineId,
        remote_focus_operation: RemoteFocusOperation,
    ) {
        let Some(document) = self.documents.borrow().find_document(pipeline_id) else {
            warn!("Unknown {pipeline_id:?} for FocusDocument message.");
            return;
        };
        match remote_focus_operation {
            RemoteFocusOperation::Viewport => document.window().Focus(cx),
            RemoteFocusOperation::Sequential(direction, iframe_browsing_context_id) => document
                .focus_handler()
                .sequential_focus_from_another_document(cx, iframe_browsing_context_id, direction),
        }
    }

    fn handle_unfocus_document_as_part_of_focusing_steps(
        &self,
        cx: &mut js::context::JSContext,
        pipeline_id: PipelineId,
        sequence: FocusSequenceNumber,
    ) {
        let Some(document) = self.documents.borrow().find_document(pipeline_id) else {
            warn!("Unknown {pipeline_id:?} for UnfocusDocumentAsPartOfFocusingSteps");
            return;
        };

        // We ignore unfocus requests for top-level `Document`s as they *always* have focus.
        // Note that this does not take into account system focus.
        let window = document.window();
        if window.is_top_level() {
            return;
        }

        let focus_handler = document.focus_handler();
        if focus_handler.focus_sequence() > sequence {
            debug!(
                "Disregarding the Unfocus message because the contained sequence number is \
                too old ({:?} < {:?})",
                sequence,
                focus_handler.focus_sequence()
            );
            return;
        }

        rooted!(&in(cx) let new_focus_chain = vec![]);
        rooted!(&in(cx) let old_focus_chain = focus_handler.current_focus_chain());

        focus_handler.focus_update_steps(
            cx,
            new_focus_chain,
            old_focus_chain,
            &FocusableArea::Viewport,
        );
    }

    #[expect(clippy::too_many_arguments)]
    /// <https://html.spec.whatwg.org/multipage/#window-post-message-steps>
    fn handle_post_message_msg(
        &self,
        cx: &mut js::context::JSContext,
        pipeline_id: PipelineId,
        source_webview: WebViewId,
        source_with_ancestry: Vec<BrowsingContextId>,
        origin: Option<ImmutableOrigin>,
        source_origin: ImmutableOrigin,
        data: StructuredSerializedData,
    ) {
        let window = self.documents.borrow().find_window(pipeline_id);
        match window {
            None => warn!("postMessage after target pipeline {} closed.", pipeline_id),
            Some(window) => {
                let mut last = None;
                for browsing_context_id in source_with_ancestry.into_iter().rev() {
                    if let Some(window_proxy) =
                        self.window_proxies.find_window_proxy(browsing_context_id)
                    {
                        last = Some(window_proxy);
                        continue;
                    }
                    let window_proxy = WindowProxy::new_dissimilar_origin(
                        cx,
                        window.upcast::<GlobalScope>(),
                        browsing_context_id,
                        source_webview,
                        last.as_deref(),
                        None,
                        CreatorBrowsingContextInfo::from(last.as_deref(), None),
                    );
                    self.window_proxies
                        .insert(browsing_context_id, &window_proxy);
                    last = Some(window_proxy);
                }

                // Step 8.3: Let source be the WindowProxy object corresponding to
                // incumbentSettings's global object (a Window object).
                let source = last.expect("Source with ancestry should contain at least one bc.");

                // FIXME(#22512): enqueues a task; unnecessary delay.
                window.post_message(origin, source_origin, &source, data)
            },
        }
    }

    fn handle_stop_delaying_load_events_mode(&self, pipeline_id: PipelineId) {
        let window = self.documents.borrow().find_window(pipeline_id);
        if let Some(window) = window {
            match window.undiscarded_window_proxy() {
                Some(window_proxy) => window_proxy.stop_delaying_load_events_mode(),
                None => warn!(
                    "Attempted to take {} of 'delaying-load-events-mode' after having been discarded.",
                    pipeline_id
                ),
            };
        }
    }

    fn handle_unload_document(&self, cx: &mut js::context::JSContext, pipeline_id: PipelineId) {
        let document = self.documents.borrow().find_document(pipeline_id);
        if let Some(document) = document {
            document.unload(cx, false);
        }
    }

    fn handle_update_pipeline_id(
        &self,
        parent_pipeline_id: PipelineId,
        browsing_context_id: BrowsingContextId,
        webview_id: WebViewId,
        new_pipeline_id: PipelineId,
        reason: UpdatePipelineIdReason,
        cx: &mut js::context::JSContext,
    ) {
        let frame_element = self
            .documents
            .borrow()
            .find_iframe(parent_pipeline_id, browsing_context_id);
        let Some(frame_element) = frame_element else {
            return;
        };
        if !frame_element.update_pipeline_id(new_pipeline_id, reason, cx) {
            return;
        };

        let Some(window) = self.documents.borrow().find_window(new_pipeline_id) else {
            return;
        };
        // Ensure that the state of any local window proxies accurately reflects
        // the new pipeline.
        let _ = self.window_proxies.local_window_proxy(
            cx,
            &self.senders,
            &self.documents,
            &window,
            browsing_context_id,
            webview_id,
            Some(parent_pipeline_id),
            // Any local window proxy has already been created, so there
            // is no need to pass along existing opener information that
            // will be discarded.
            None,
        );
    }

    fn handle_update_history_state_msg(
        &self,
        cx: &mut js::context::JSContext,
        pipeline_id: PipelineId,
        history_state_id: Option<HistoryStateId>,
        url: ServoUrl,
    ) {
        let Some(window) = self.documents.borrow().find_window(pipeline_id) else {
            return warn!("update history state after pipeline {pipeline_id} closed.",);
        };
        window.History(cx).activate_state(cx, history_state_id, url);
    }

    fn handle_remove_history_states(
        &self,
        cx: &mut js::context::JSContext,
        pipeline_id: PipelineId,
        history_states: Vec<HistoryStateId>,
    ) {
        let Some(window) = self.documents.borrow().find_window(pipeline_id) else {
            return warn!("update history state after pipeline {pipeline_id} closed.",);
        };
        window.History(cx).remove_states(history_states);
    }

    /// Window was resized, but this script was not active, so don't reflow yet
    fn handle_resize_inactive_msg(&self, id: PipelineId, new_viewport_details: ViewportDetails) {
        let window = self.documents.borrow().find_window(id)
            .expect("ScriptThread: received a resize msg for a pipeline not in this script thread. This is a bug.");
        window.set_viewport_details(new_viewport_details);
    }

    /// We have received notification that the response associated with a load has completed.
    /// Kick off the document and frame tree creation process using the result.
    fn handle_page_headers_available(
        &self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        metadata: Option<&Metadata>,
        origin: MutableOrigin,
        cx: &mut js::context::JSContext,
    ) -> Option<DomRoot<Document>> {
        if self.closed_pipelines.borrow().contains(&pipeline_id) {
            // If the pipeline closed, do not process the headers.
            return None;
        }

        let Some(idx) = self
            .incomplete_loads
            .borrow()
            .iter()
            .position(|load| load.pipeline_id == pipeline_id)
        else {
            unreachable!("Pipeline shouldn't have finished loading.");
        };

        // https://html.spec.whatwg.org/multipage/#process-a-navigate-response
        // 2. If response's status is 204 or 205, then abort these steps.
        //
        // TODO: The specification has been updated and we no longer should abort.
        let is_204_205 = match metadata {
            Some(metadata) => metadata.status.in_range(204..=205),
            _ => false,
        };

        if is_204_205 {
            // If we have an existing window that is being navigated:
            if let Some(window) = self.documents.borrow().find_window(pipeline_id) {
                let window_proxy = window.window_proxy();
                // https://html.spec.whatwg.org/multipage/
                // #navigating-across-documents:delaying-load-events-mode-2
                if window_proxy.parent().is_some() {
                    // The user agent must take this nested browsing context
                    // out of the delaying load events mode
                    // when this navigation algorithm later matures,
                    // or when it terminates (whether due to having run all the steps,
                    // or being canceled, or being aborted), whichever happens first.
                    window_proxy.stop_delaying_load_events_mode();
                }
            }
            self.senders
                .pipeline_to_constellation_sender
                .send((
                    webview_id,
                    pipeline_id,
                    ScriptToConstellationMessage::AbortLoadUrl,
                ))
                .unwrap();
            if self.document_producer_fence.is_some() {
                // The response stream is still a live Resource producer until cancellation
                // delivers EOF. Cancel and remove the InProgressLoad now; handle_fetch_metadata
                // removes the parser context after its currently borrowed callback returns.
                self.terminate_incomplete_navigation_loads(pipeline_id);
            }
            return None;
        };

        let load = self.incomplete_loads.borrow_mut().remove(idx);
        metadata.map(|meta| self.load(meta, load, origin, cx))
    }

    /// Handles a request for the window title.
    fn handle_get_title_msg(&self, pipeline_id: PipelineId) {
        let Some(document) = self.documents.borrow().find_document(pipeline_id) else {
            return warn!("Message sent to closed pipeline {pipeline_id}.");
        };
        document.send_title_to_embedder();
    }

    /// Cancel navigation fetches that have not yet reached a complete Document.
    fn terminate_incomplete_navigation_loads(&self, pipeline_id: PipelineId) {
        loop {
            let incomplete_load = {
                let mut incomplete_loads = self.incomplete_loads.borrow_mut();
                incomplete_loads
                    .iter()
                    .position(|load| load.pipeline_id == pipeline_id)
                    .map(|index| incomplete_loads.remove(index))
            };
            let Some(mut incomplete_load) = incomplete_load else {
                break;
            };
            incomplete_load.canceller.terminate();
        }
    }

    /// Cancel and forget navigation state that has not yet reached a complete Document.
    fn terminate_incomplete_navigation(&self, pipeline_id: PipelineId) {
        self.terminate_incomplete_navigation_loads(pipeline_id);
        self.incomplete_parser_contexts
            .0
            .borrow_mut()
            .retain(|(parser_pipeline_id, _)| *parser_pipeline_id != pipeline_id);
    }

    /// Handles a request to exit a pipeline and shut down layout.
    fn handle_exit_pipeline_msg(
        &self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        discard_bc: DiscardBrowsingContext,
        cx: &mut js::context::JSContext,
    ) {
        debug!("{pipeline_id}: Starting pipeline exit.");

        if self.document_producer_fence.is_some() {
            // Establish a permanent task-queue tombstone before tearing down the document. This
            // purges every locally retained task and makes producers racing the exit fail closed
            // on their next intake. Tasks already gathered into this turn are rejected by
            // `handle_msg_from_script` against the same ScriptThread tombstone.
            self.closed_pipelines.borrow_mut().insert(pipeline_id);
            self.task_queue.discard_pipeline(pipeline_id);
            if let Some(controlled_input) = &self.controlled_input {
                controlled_input.borrow_mut().discard_pipeline(pipeline_id);
            }
            self.terminate_incomplete_navigation(pipeline_id);
        }

        // Abort the parser, if any,
        // to prevent any further incoming networking messages from being handled.
        let document = self.documents.borrow_mut().remove(pipeline_id);
        if let Some(document) = document {
            // We should never have a pipeline that's still an incomplete load, but also has a Document.
            debug_assert!(
                !self
                    .incomplete_loads
                    .borrow()
                    .iter()
                    .any(|load| load.pipeline_id == pipeline_id)
            );

            if self.document_producer_fence.is_some() {
                // Once response headers create a Document, it owns the navigation canceller.
                // Terminate it before dropping the DOM so the fenced response stream reaches EOF.
                document.terminate_navigation_and_subresource_fetches();
            }

            if let Some(parser) = document.get_current_parser() {
                parser.abort(cx);
            }

            if !document.window_detached() {
                debug!("{pipeline_id}: Shutting down layout");
                document.window().layout_mut().exit_now();
            }

            // Clear any active animations and unroot all of the associated DOM objects.
            debug!("{pipeline_id}: Clearing animations");
            document.animations().clear();

            if !document.window_detached() {
                // We discard the browsing context after requesting layout shut down,
                // to avoid running layout on detached iframes.
                let window = document.window();
                if discard_bc == DiscardBrowsingContext::Yes {
                    window.discard_browsing_context();
                }

                // Clear the image cache now, instead of waiting for the Window to be
                // garbage collected. See servo/servo#45239.
                window.image_cache().clear();

                debug!("{pipeline_id}: Clearing JavaScript runtime");
                window.clear_js_runtime();
            }
        }

        if self.document_producer_fence.is_none() {
            // Preserve upstream Real-mode ordering: the pipeline closes after document teardown.
            self.closed_pipelines.borrow_mut().insert(pipeline_id);
        }

        // This marker is FIFO-ordered behind every earlier Script -> Paint message for the
        // pipeline. Paint acknowledges only after recording the Script owner bit. Every path
        // which loses or rejects the marker reports a typed failure to Constellation.
        let marker_ack_completed = Arc::new(AtomicBool::new(false));
        let marker_ack_completed_in_callback = marker_ack_completed.clone();
        let marker_failure_sender = self.senders.pipeline_to_constellation_sender.clone();
        let marker_ack = generic_channel::GenericCallback::new(
            move |result: Result<PipelineExitMarkerStatus, ipc_channel::IpcError>| {
                complete_pipeline_exit_marker_ack(
                    &marker_ack_completed_in_callback,
                    result,
                    || {
                        let _ = marker_failure_sender.send((
                            webview_id,
                            pipeline_id,
                            ScriptToConstellationMessage::PipelineExitPaintMarkerFailed,
                        ));
                    },
                );
            },
        );

        match marker_ack {
            Ok(marker_ack) => {
                let retained_marker_ack = marker_ack.clone();
                if !self.paint_api.pipeline_exited(
                    webview_id,
                    pipeline_id,
                    PipelineExitSource::Script,
                    marker_ack,
                ) && retained_marker_ack
                    .send(PipelineExitMarkerStatus::CrossProcessSendFailed)
                    .is_err()
                {
                    complete_pipeline_exit_marker_ack(
                        &marker_ack_completed,
                        Ok(PipelineExitMarkerStatus::CrossProcessSendFailed),
                        || {
                            let _ = self.senders.pipeline_to_constellation_sender.send((
                                webview_id,
                                pipeline_id,
                                ScriptToConstellationMessage::PipelineExitPaintMarkerFailed,
                            ));
                        },
                    );
                }
            },
            Err(error) => {
                warn!("Could not create Paint pipeline-exit marker acknowledgement: {error}");
                complete_pipeline_exit_marker_ack(
                    &marker_ack_completed,
                    Ok(PipelineExitMarkerStatus::CrossProcessSendFailed),
                    || {
                        let _ = self.senders.pipeline_to_constellation_sender.send((
                            webview_id,
                            pipeline_id,
                            ScriptToConstellationMessage::PipelineExitPaintMarkerFailed,
                        ));
                    },
                );
            },
        }

        // Logical teardown remains independent of the asynchronous Paint acknowledgement. The
        // ordering above is nevertheless essential: the Script marker enters the same Paint
        // channel after prior display/epoch messages and before Constellation publishes its bit.
        debug!("{pipeline_id}: Sending PipelineExited message to constellation");
        self.senders
            .pipeline_to_constellation_sender
            .send((
                webview_id,
                pipeline_id,
                ScriptToConstellationMessage::PipelineExited,
            ))
            .ok();

        self.devtools_state.notify_pipeline_exited(pipeline_id);

        debug!("{pipeline_id}: Finished pipeline exit");
    }

    /// Handles a request to exit the script thread and shut down layout.
    fn handle_exit_script_thread_msg(&self, cx: &mut js::context::JSContext) {
        debug!("Exiting script thread.");

        let mut webview_and_pipeline_ids = Vec::new();
        webview_and_pipeline_ids.extend(
            self.incomplete_loads
                .borrow()
                .iter()
                .next()
                .map(|load| (load.webview_id, load.pipeline_id)),
        );
        webview_and_pipeline_ids.extend(
            self.documents
                .borrow()
                .iter()
                .next()
                .map(|(pipeline_id, document)| (document.webview_id(), pipeline_id)),
        );

        for (webview_id, pipeline_id) in webview_and_pipeline_ids {
            self.handle_exit_pipeline_msg(webview_id, pipeline_id, DiscardBrowsingContext::Yes, cx);
        }

        self.background_hang_monitor.unregister();

        // If we're in multiprocess mode, shut-down the IPC router for this process.
        if opts::get().multiprocess {
            debug!("Exiting IPC router thread in script thread.");
            ROUTER.shutdown();
        }

        debug!("Exited script thread.");
    }

    /// Handles animation tick requested during testing.
    pub(crate) fn handle_tick_all_animations_for_testing(no_gc: &NoGC, id: PipelineId) {
        with_script_thread(|script_thread| {
            let Some(document) = script_thread.documents.borrow().find_document(id) else {
                warn!("Animation tick for tests for closed pipeline {id}.");
                return;
            };
            document.maybe_mark_animating_nodes_as_dirty(no_gc);
        });
    }

    /// Handles a Web font being loaded. Does nothing if the page no longer exists.
    fn handle_web_font_loaded(&self, no_gc: &NoGC, pipeline_id: PipelineId) {
        let Some(document) = self.documents.borrow().find_document(pipeline_id) else {
            warn!("Web font loaded in closed pipeline {}.", pipeline_id);
            return;
        };

        // TODO: This should only dirty nodes that are waiting for a web font to finish loading!
        document.dirty_all_nodes(no_gc);

        document
            .window()
            .font_context()
            .decrement_count_of_loading_fonts_by_one();
    }

    /// Handles a worklet being loaded by triggering a relayout of the page. Does nothing if the
    /// page no longer exists.
    fn handle_worklet_loaded(&self, pipeline_id: PipelineId) {
        if let Some(document) = self.documents.borrow().find_document(pipeline_id) {
            document.add_restyle_reason(RestyleReason::PaintWorkletLoaded);
        }
    }

    /// Notify a window of a storage event
    #[allow(clippy::too_many_arguments)]
    fn handle_storage_event(
        &self,
        pipeline_id: PipelineId,
        storage_type: WebStorageType,
        url: ServoUrl,
        key: Option<String>,
        old_value: Option<String>,
        new_value: Option<String>,
        cx: &mut js::context::JSContext,
    ) {
        let Some(window) = self.documents.borrow().find_window(pipeline_id) else {
            return warn!("Storage event sent to closed pipeline {pipeline_id}.");
        };

        let storage = match storage_type {
            WebStorageType::Local => window.GetLocalStorage(cx),
            WebStorageType::Session => window.GetSessionStorage(cx),
        };
        let Ok(storage) = storage else {
            return;
        };

        storage.queue_storage_event(url, key, old_value, new_value);
    }

    /// Notify the containing document of a child iframe that has completed loading.
    fn handle_iframe_load_event(
        &self,
        parent_id: PipelineId,
        browsing_context_id: BrowsingContextId,
        child_id: PipelineId,
        cx: &mut js::context::JSContext,
    ) {
        let iframe = self
            .documents
            .borrow()
            .find_iframe(parent_id, browsing_context_id);
        match iframe {
            Some(iframe) => iframe.iframe_load_event_steps(child_id, cx),
            None => warn!("Message sent to closed pipeline {}.", parent_id),
        }
    }

    fn ask_constellation_for_top_level_info(
        &self,
        sender_webview_id: WebViewId,
        sender_pipeline_id: PipelineId,
        browsing_context_id: BrowsingContextId,
    ) -> Option<WebViewId> {
        let (result_sender, result_receiver) = generic_channel::channel().unwrap();
        let msg = ScriptToConstellationMessage::GetTopForBrowsingContext(
            browsing_context_id,
            result_sender,
        );
        self.senders
            .pipeline_to_constellation_sender
            .send((sender_webview_id, sender_pipeline_id, msg))
            .expect("Failed to send to constellation.");
        result_receiver
            .recv()
            .expect("Failed to get top-level id from constellation.")
    }

    /// The entry point to document loading. Defines bindings, sets up the window and document
    /// objects, parses HTML and CSS, and kicks off initial layout.
    fn load(
        &self,
        metadata: &Metadata,
        incomplete: InProgressLoad,
        origin: MutableOrigin,
        cx: &mut js::context::JSContext,
    ) -> DomRoot<Document> {
        let script_to_constellation_chan = ScriptToConstellationChan {
            sender: self.senders.pipeline_to_constellation_sender.clone(),
            webview_id: incomplete.webview_id,
            pipeline_id: incomplete.pipeline_id,
        };

        let final_url = metadata.final_url.clone();
        let _ = script_to_constellation_chan
            .send(ScriptToConstellationMessage::SetFinalUrl(final_url.clone()));

        debug!(
            "ScriptThread: loading {} on pipeline {:?}",
            incomplete.load_data.url, incomplete.pipeline_id
        );

        let font_context = Arc::new(FontContext::new(
            self.system_font_service.clone(),
            self.paint_api.clone(),
            self.resource_threads.clone(),
        ));

        let font_resolver = Arc::new(SvgFontResolver::new(font_context.clone()));

        let image_cache = self.image_cache_factory.create(
            incomplete.webview_id,
            incomplete.pipeline_id,
            &self.paint_api,
            font_resolver,
        );

        let (user_contents, user_stylesheets) = incomplete
            .user_content_manager_id
            .and_then(|user_content_manager_id| {
                self.user_contents_for_manager_id
                    .borrow()
                    .get(&user_content_manager_id)
                    .map(|script_thread_user_contents| {
                        (
                            script_thread_user_contents.user_scripts.clone(),
                            script_thread_user_contents.user_stylesheets.clone(),
                        )
                    })
            })
            .unwrap_or_default();

        let layout_config = LayoutConfig {
            id: incomplete.pipeline_id,
            webview_id: incomplete.webview_id,
            url: final_url.clone(),
            is_iframe: incomplete.parent_info.is_some(),
            script_chan: self.senders.constellation_sender.clone(),
            image_cache: image_cache.clone(),
            font_context,
            time_profiler_chan: self.senders.time_profiler_sender.clone(),
            paint_api: self.paint_api.clone(),
            viewport_details: incomplete.viewport_details,
            user_stylesheets,
            theme: incomplete.embedder_theme,
            embedder_chan: self.senders.pipeline_to_embedder_sender.clone(),
        };

        // Create the window and document objects.
        // <https://html.spec.whatwg.org/multipage/#set-up-a-window-environment-settings-object>
        // <https://html.spec.whatwg.org/multipage/#initialise-the-document-object>
        // Step 3. Let creationURL be navigationParams's response's URL.
        let creation_url = final_url.clone();
        let window = match window_for_replacement(
            &self.window_proxies,
            incomplete.browsing_context_id,
            &origin,
        ) {
            Some(window) => {
                window.set_up_a_window_environment_settings_object(
                    self.layout_factory.create(layout_config),
                    creation_url,
                    // TODO(37417): Set correct top-level URL here.
                    final_url.clone(),
                    incomplete.navigation_start,
                    incomplete.document_time_origin,
                    incomplete.viewport_details,
                );
                window
            },
            None => {
                Window::new(
                    cx,
                    incomplete.webview_id,
                    self.js_runtime.clone(),
                    self.senders.self_sender.clone(),
                    self.layout_factory.create(layout_config),
                    self.senders.image_cache_sender.clone(),
                    self.resource_threads.clone(),
                    self.storage_threads.clone(),
                    #[cfg(feature = "bluetooth")]
                    self.senders.bluetooth_sender.clone(),
                    self.senders.memory_profiler_sender.clone(),
                    self.senders.time_profiler_sender.clone(),
                    self.senders.devtools_server_sender.clone(),
                    self.senders.pipeline_to_constellation_sender.clone(),
                    self.senders.pipeline_to_embedder_sender.clone(),
                    self.senders.constellation_sender.clone(),
                    incomplete.pipeline_id,
                    incomplete.parent_info,
                    incomplete.viewport_details,
                    origin.clone(),
                    creation_url,
                    // TODO(37417): Set correct top-level URL here. Currently, we only specify the
                    // url of the current window. However, in case this is an iframe, we should
                    // pass in the URL from the frame that includes the iframe (which potentially
                    // is another nested iframe in a frame).
                    final_url.clone(),
                    incomplete.navigation_start,
                    incomplete.document_time_origin,
                    #[cfg(feature = "webgl")]
                    self.webgl_chan.as_ref().map(|chan| chan.channel()),
                    #[cfg(feature = "webxr")]
                    self.webxr_registry.clone(),
                    self.paint_api.clone(),
                    self.unminify_js,
                    self.unminify_css,
                    self.local_script_source.clone(),
                    user_contents,
                    self.player_context.clone(),
                    #[cfg(feature = "webgpu")]
                    self.gpu_id_hub.clone(),
                    incomplete.load_data.inherited_secure_context,
                    incomplete.embedder_theme,
                    self.this.clone(),
                )
            },
        };
        if self.senders.devtools_server_sender.is_some() {
            self.debugger_global.fire_add_debuggee(
                cx,
                window.upcast(),
                incomplete.pipeline_id,
                None,
            );
        }

        let mut realm = enter_auto_realm(cx, &*window);
        let cx = &mut realm;

        // https://html.spec.whatwg.org/multipage/#resource-metadata-management
        // > The Document's source file's last modification date and time must be derived from
        // > relevant features of the networking protocols used, e.g.
        // > from the value of the HTTP `Last-Modified` header of the document,
        // > or from metadata in the file system for local files.
        // > If the last modification date and time are not known,
        // > the attribute must return the current date and time in the above format.
        let last_modified = metadata.headers.as_ref().and_then(|headers| {
            headers.typed_get::<LastModified>().map(|tm| {
                let tm: SystemTime = tm.into();
                let local_time: DateTime<Local> = tm.into();
                local_time.format("%m/%d/%Y %H:%M:%S").to_string()
            })
        });

        let loader = DocumentLoader::new_with_threads(
            self.resource_threads.clone(),
            Some(final_url.clone()),
        );

        let content_type: Option<Mime> = metadata
            .content_type
            .clone()
            .map(Serde::into_inner)
            .map(Mime::from_ct);
        let encoding_hint_from_content_type = content_type
            .as_ref()
            .and_then(|mime| mime.get_parameter(CHARSET))
            .and_then(|charset| Encoding::for_label(charset.as_bytes()));

        let is_html_document = match content_type {
            Some(ref mime) if mime.type_ == APPLICATION && mime.has_suffix("xml") => {
                IsHTMLDocument::NonHTMLDocument
            },

            Some(ref mime) if mime.matches(TEXT, XML) || mime.matches(APPLICATION, XML) => {
                IsHTMLDocument::NonHTMLDocument
            },
            _ => IsHTMLDocument::HTMLDocument,
        };

        // Step 14. If navigationParams's request is non-null:
        // Step 14.1. Set document's referrer to the empty string.
        // Step 14.2. Let referrer be navigationParams's request's referrer.
        // Step 14.3. If referrer is a URL record, then set document's referrer
        //   to the serialization of referrer.
        // TODO: verify that this actually matches the specification.
        let referrer = metadata
            .referrer
            .as_ref()
            .map(|referrer| referrer.clone().into_string());

        let document_source = if incomplete.load_data.is_initial_about_blank {
            DocumentSource::NotFromParser
        } else {
            DocumentSource::FromParser
        };

        // Step 9. Let document be a new Document, with
        // - content type: contentType
        // - origin: navigationParams's origin
        // - active sandboxing set: navigationParams's final sandboxing flag set
        // - load timing info: loadTimingInfo
        // - URL: creationURL
        // - current document readiness: "loading"
        // - about base URL: navigationParams's about base URL
        let document = Document::new(
            cx,
            &window,
            HasBrowsingContext::Yes,
            Some(final_url.clone()),
            incomplete.load_data.about_base_url,
            origin,
            is_html_document,
            content_type,
            last_modified,
            incomplete.activity,
            document_source,
            loader,
            referrer,
            Some(metadata.status.raw_code()),
            incomplete.canceller,
            incomplete.load_data.is_initial_about_blank,
            true,
            incomplete.load_data.inherited_insecure_requests_policy,
            incomplete.load_data.has_trustworthy_ancestor_origin,
            self.custom_element_reaction_stack.clone(),
            incomplete.load_data.creation_sandboxing_flag_set,
            incomplete.pipeline_id,
            image_cache,
        );

        document.set_ready_state(cx, DocumentReadyState::Loading);

        // Step 8. Let loadTimingInfo be a new document load timing info with its
        //   navigation start time set to navigationParams's response's timing
        //   info's start time.
        document.set_navigation_start(incomplete.navigation_start);

        let referrer_policy = metadata
            .headers
            .as_deref()
            .and_then(|h| h.typed_get::<ReferrerPolicyHeader>())
            .into();
        document.set_referrer_policy(referrer_policy);

        self.documents
            .borrow_mut()
            .insert(incomplete.pipeline_id, &document);

        // Step 10. Set window's associated Document to document.
        window.init_document(&document);

        // Initialize the browsing context for the window.
        let window_proxy = self.window_proxies.local_window_proxy(
            cx,
            &self.senders,
            &self.documents,
            &window,
            incomplete.browsing_context_id,
            incomplete.webview_id,
            incomplete.parent_info,
            incomplete.opener,
        );
        if let Some(name) = incomplete.frame_name {
            window_proxy.set_name(DOMString::from(name));
        }
        if window_proxy.parent().is_some() {
            // https://html.spec.whatwg.org/multipage/#navigating-across-documents:delaying-load-events-mode-2
            // The user agent must take this nested browsing context
            // out of the delaying load events mode
            // when this navigation algorithm later matures.
            window_proxy.stop_delaying_load_events_mode();
        }
        window.init_window_proxy(&window_proxy);

        // For any similar-origin iframe, ensure that the contentWindow/contentDocument
        // APIs resolve to the new window/document as soon as parsing starts.
        if let Some(frame) = window_proxy
            .frame_element()
            .and_then(|e| e.downcast::<HTMLIFrameElement>())
        {
            let parent_pipeline = frame.global().pipeline_id();
            self.handle_update_pipeline_id(
                parent_pipeline,
                window_proxy.browsing_context_id(),
                window_proxy.webview_id(),
                incomplete.pipeline_id,
                UpdatePipelineIdReason::Navigation,
                cx,
            );
        }

        let refresh_header = metadata.headers.as_deref().and_then(|h| h.get(REFRESH));
        // Step 17. If navigationParams's response has a `Refresh` header:
        if let Some(refresh_val) = refresh_header {
            // Step 17.1. Let value be the isomorphic decoding of the value of the header.
            // Step 17.2. Run the shared declarative refresh steps with document and value.

            // There are tests that this header handles Unicode code points
            document.shared_declarative_refresh_steps(
                refresh_val.as_bytes(),
                /* from_meta_element */ false,
            );
        }

        let activation_correlation = self.document_control_state.as_ref().and_then(|state| {
            let mut state = state.borrow_mut();
            state
                .initial_pipeline_activation
                .is_some_and(|marker| marker.pipeline_id == incomplete.pipeline_id)
                .then(|| state.initial_pipeline_activation.take())
                .flatten()
                .map(|marker| marker.correlation)
        });
        let controlled_cookie_site_for_cookies = (self.document_execution_profile
            == DocumentExecutionProfile::ControlledWebSessionV2)
            .then(|| {
                document
                    .window()
                    .controlled_cookie_site_for_document(&document)
            })
            .flatten();
        self.senders
            .pipeline_to_constellation_sender
            .send((
                incomplete.webview_id,
                incomplete.pipeline_id,
                ScriptToConstellationMessage::ActivateDocument(
                    activation_correlation,
                    controlled_cookie_site_for_cookies,
                ),
            ))
            .unwrap();

        // Notify devtools that a new script global exists.
        let incomplete_browsing_context_id: BrowsingContextId = incomplete.webview_id.into();
        let is_top_level_global = incomplete_browsing_context_id == incomplete.browsing_context_id;
        self.notify_devtools(
            document.Title(),
            final_url.clone(),
            is_top_level_global,
            (
                incomplete.browsing_context_id,
                incomplete.pipeline_id,
                None,
                incomplete.webview_id,
            ),
        );

        if !incomplete.load_data.is_initial_about_blank {
            if is_html_document == IsHTMLDocument::NonHTMLDocument {
                ServoParser::parse_xml_document(
                    cx,
                    &document,
                    None,
                    final_url,
                    encoding_hint_from_content_type,
                );
            } else {
                ServoParser::parse_html_document(
                    cx,
                    &document,
                    None,
                    final_url,
                    encoding_hint_from_content_type,
                    incomplete.load_data.container_document_encoding,
                );
            }
        }

        if incomplete.activity == DocumentActivity::FullyActive {
            window.resume(cx);
        } else {
            window.suspend(cx);
        }

        if incomplete.throttled {
            window.set_throttled(true);
        }

        document
    }

    fn notify_devtools(
        &self,
        title: DOMString,
        url: ServoUrl,
        is_top_level_global: bool,
        (browsing_context_id, pipeline_id, worker_id, webview_id): (
            BrowsingContextId,
            PipelineId,
            Option<WorkerId>,
            WebViewId,
        ),
    ) {
        if let Some(ref chan) = self.senders.devtools_server_sender {
            let page_info = DevtoolsPageInfo {
                title: String::from(title),
                url,
                is_top_level_global,
                is_service_worker: false,
            };
            chan.send(ScriptToDevtoolsControlMsg::NewGlobal(
                (browsing_context_id, pipeline_id, worker_id, webview_id),
                self.senders.devtools_client_to_script_thread_sender.clone(),
                page_info.clone(),
            ))
            .unwrap();

            let state = NavigationState::Stop(pipeline_id, page_info);
            let _ = chan.send(ScriptToDevtoolsControlMsg::Navigate(
                browsing_context_id,
                state,
            ));
        }
    }

    /// Queue input events for later dispatching as part of a `update_the_rendering` task.
    fn handle_input_event(
        &self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        event: ConstellationInputEvent,
    ) {
        let Some(document) = self.documents.borrow().find_document(pipeline_id) else {
            warn!("Input event sent to closed pipeline {pipeline_id}.");
            let _ = self
                .senders
                .pipeline_to_embedder_sender
                .send(EmbedderMsg::InputEventsHandled(
                    webview_id,
                    vec![InputEventOutcome {
                        id: event.event.id,
                        result: Default::default(),
                    }],
                ));
            return;
        };
        document.event_handler().note_pending_input_event(event);
    }

    /// See the docs for [`ScriptThreadMessage::SetAccessibilityActive`].
    fn set_accessibility_active(&self, pipeline_id: PipelineId, active: bool, epoch: Epoch) {
        if !(pref!(accessibility_enabled)) {
            return;
        }

        let Some(document) = self.documents.borrow().find_document(pipeline_id) else {
            if active {
                error!("Trying to set accessibility active on stale document: {pipeline_id}");
            }
            return;
        };

        document
            .window()
            .layout()
            .set_accessibility_active(active, epoch);
    }

    /// Handle a "navigate an iframe" message from the constellation.
    fn handle_navigate_iframe(
        &self,
        parent_pipeline_id: PipelineId,
        browsing_context_id: BrowsingContextId,
        load_data: LoadData,
        history_handling: NavigationHistoryBehavior,
        target_snapshot_params: TargetSnapshotParams,
        cx: &mut js::context::JSContext,
    ) {
        let iframe = self
            .documents
            .borrow()
            .find_iframe(parent_pipeline_id, browsing_context_id);
        if let Some(iframe) = iframe {
            iframe.navigate_or_reload_child_browsing_context(
                load_data,
                history_handling,
                ProcessingMode::NotFirstTime,
                target_snapshot_params,
                cx,
            );
        }
    }

    /// Turn javascript: URL into JS code to eval, according to the steps in
    /// <https://html.spec.whatwg.org/multipage/#evaluate-a-javascript:-url>
    /// Returns the evaluated body, if available.
    fn eval_js_url(
        cx: &mut js::context::JSContext,
        global_scope: &GlobalScope,
        url: &ServoUrl,
    ) -> Option<String> {
        // Step 1. Let urlString be the result of running the URL serializer on url.
        // Step 2. Let encodedScriptSource be the result of removing the leading "javascript:" from urlString.
        let encoded = &url[Position::AfterScheme..][1..];

        // // Step 3. Let scriptSource be the UTF-8 decoding of the percent-decoding of encodedScriptSource.
        let script_source = percent_decode(encoded.as_bytes()).decode_utf8_lossy();

        // Step 4. Let settings be targetNavigable's active document's relevant settings object.
        // Step 5. Let baseURL be settings's API base URL.
        // Step 6. Let script be the result of creating a classic script given scriptSource, settings, baseURL, and the default script fetch options.
        // Note: these steps are handled by `evaluate_js_on_global`.
        let mut realm = enter_auto_realm(cx, global_scope);
        let cx = &mut realm.current_realm();

        rooted!(&in(cx) let mut jsval = UndefinedValue());
        // Step 7. Let evaluationStatus be the result of running the classic script script.
        let evaluation_status = global_scope.evaluate_js_on_global(
            cx,
            script_source,
            "",
            Some(IntroductionType::JAVASCRIPT_URL),
            Some(jsval.handle_mut()),
        );

        // Step 9. If evaluationStatus is a normal completion, and evaluationStatus.[[Value]]
        //   is a String, then set result to evaluationStatus.[[Value]].
        // Step 10. Otherwise, return null.
        if evaluation_status.is_err() || !jsval.get().is_string() {
            return None;
        }

        let strval = DOMString::safe_from_jsval(cx, jsval.handle(), StringificationBehavior::Empty);
        match strval {
            Ok(ConversionResult::Success(s)) => {
                // Step 11. Let response be a new response with
                // the UTF-8 encoding of result, as a body.
                Some(String::from(s))
            },
            _ => unreachable!("Couldn't get a string from a JS string??"),
        }
    }

    /// Instructs the constellation to fetch the document that will be loaded. Stores the InProgressLoad
    /// argument until a notification is received that the fetch is complete.
    #[servo_tracing::instrument(skip_all)]
    fn pre_page_load(&self, cx: &mut js::context::JSContext, mut incomplete: InProgressLoad) {
        let url_str = incomplete.load_data.url.as_str();
        if url_str == "about:blank" || incomplete.load_data.js_eval_result.is_some() {
            self.start_synchronous_page_load(cx, incomplete);
            return;
        }
        if url_str == "about:srcdoc" {
            self.page_load_about_srcdoc(cx, incomplete);
            return;
        }

        let context = ParserContext::new(
            incomplete.webview_id,
            incomplete.pipeline_id,
            incomplete.load_data.url.clone(),
            incomplete.load_data.creation_sandboxing_flag_set,
            incomplete.parent_info,
            incomplete.target_snapshot_params,
            incomplete.load_data.load_origin.clone(),
        );
        self.incomplete_parser_contexts
            .0
            .borrow_mut()
            .push((incomplete.pipeline_id, context));

        let request_builder = incomplete.request_builder();
        incomplete.canceller = FetchCanceller::new(
            request_builder.id,
            false,
            self.resource_threads.core_thread.clone(),
        );
        let event_loop_sender = ScriptEventLoopSender::MainThread {
            sender: self.senders.self_sender.clone(),
            producer_fence: self.document_producer_fence.clone(),
        };
        if let Err(error) = NavigationListener::new(request_builder, event_loop_sender)
            .initiate_fetch(&self.resource_threads.core_thread, None)
        {
            // Producer admission failure is sticky. Remove the parser context created for this
            // request and refuse to start an untracked navigation.
            self.incomplete_parser_contexts
                .0
                .borrow_mut()
                .retain(|(pipeline_id, _)| *pipeline_id != incomplete.pipeline_id);
            error!(
                "Refusing to start an unfenced navigation for pipeline {:?}: {error}",
                incomplete.pipeline_id
            );
            return;
        }
        self.incomplete_loads.borrow_mut().push(incomplete);
    }

    fn handle_navigation_response(
        &self,
        cx: &mut js::context::JSContext,
        pipeline_id: PipelineId,
        message: FetchResponseMsg,
    ) {
        if let Some(metadata) = NavigationListener::http_redirect_metadata(&message) {
            self.handle_navigation_redirect(pipeline_id, metadata);
            return;
        };

        match message {
            FetchResponseMsg::ProcessResponse(request_id, metadata) => {
                self.handle_fetch_metadata(cx, pipeline_id, request_id, metadata)
            },
            FetchResponseMsg::ProcessResponseChunk(request_id, chunk) => {
                self.handle_fetch_chunk(cx, pipeline_id, request_id, chunk.0)
            },
            FetchResponseMsg::ProcessResponseEOF(request_id, eof, timing) => {
                self.handle_fetch_eof(cx, pipeline_id, request_id, eof, timing)
            },
            FetchResponseMsg::ProcessCspViolations(request_id, violations) => {
                self.handle_csp_violations(cx, pipeline_id, request_id, violations)
            },
            FetchResponseMsg::ProcessRequestBody(..) => {},
            FetchResponseMsg::ProcessContentLength(_request_id, _size) => {},
        }
    }

    fn handle_fetch_metadata(
        &self,
        cx: &mut js::context::JSContext,
        id: PipelineId,
        request_id: RequestId,
        fetch_metadata: Result<FetchMetadata, NetworkError>,
    ) {
        match fetch_metadata {
            Ok(_) => (),
            Err(NetworkError::Crash(..)) => (),
            Err(ref e) => {
                warn!("Network error: {:?}", e);
            },
        };

        let mut incomplete_parser_contexts = self.incomplete_parser_contexts.0.borrow_mut();
        let Some(parser_index) = incomplete_parser_contexts
            .iter()
            .position(|(pipeline_id, _)| *pipeline_id == id)
        else {
            return;
        };

        incomplete_parser_contexts[parser_index]
            .1
            .process_response(cx, request_id, fetch_metadata);

        if self.document_producer_fence.is_some() &&
            incomplete_parser_contexts[parser_index]
                .1
                .get_document()
                .is_none() &&
            !self
                .incomplete_loads
                .borrow()
                .iter()
                .any(|load| load.pipeline_id == id)
        {
            // `page_headers_available` runs inside `process_response`, while this RefCell is
            // already borrowed. A Controlled 204/205 cancels and removes the InProgressLoad there;
            // remove its now-orphaned parser context only after the callback has returned.
            incomplete_parser_contexts.remove(parser_index);
        }
    }

    fn handle_fetch_chunk(
        &self,
        cx: &mut js::context::JSContext,
        pipeline_id: PipelineId,
        request_id: RequestId,
        chunk: Vec<u8>,
    ) {
        let mut incomplete_parser_contexts = self.incomplete_parser_contexts.0.borrow_mut();
        let parser = incomplete_parser_contexts
            .iter_mut()
            .find(|&&mut (parser_pipeline_id, _)| parser_pipeline_id == pipeline_id);
        if let Some(&mut (_, ref mut ctxt)) = parser {
            ctxt.process_response_chunk(cx, request_id, chunk);
        }
    }

    #[expect(clippy::redundant_clone, reason = "False positive")]
    fn handle_fetch_eof(
        &self,
        cx: &mut js::context::JSContext,
        id: PipelineId,
        request_id: RequestId,
        eof: Result<(), NetworkError>,
        timing: ResourceFetchTiming,
    ) {
        let idx = self
            .incomplete_parser_contexts
            .0
            .borrow()
            .iter()
            .position(|&(pipeline_id, _)| pipeline_id == id);

        if let Some(idx) = idx {
            let (_, context) = self.incomplete_parser_contexts.0.borrow_mut().remove(idx);

            // we need to register an iframe entry to the performance timeline if present
            if let Some(window_proxy) = context
                .get_document()
                .and_then(|document| document.browsing_context()) &&
                let Some(frame_element) = window_proxy.frame_element()
            {
                let iframe_ctx = IframeContext::new(
                    frame_element
                        .downcast::<HTMLIFrameElement>()
                        .expect("WindowProxy::frame_element should be an HTMLIFrameElement"),
                );

                // submit_timing will only accept timing that is of type ResourceTimingType::Resource
                let mut resource_timing = timing.clone();
                resource_timing.timing_type = ResourceTimingType::Resource;
                submit_timing(cx, &iframe_ctx, &eof, &resource_timing);
            }

            context.process_response_eof(cx, request_id, eof, timing);
        }
    }

    fn handle_csp_violations(
        &self,
        cx: &mut js::context::JSContext,
        pipeline_id: PipelineId,
        _request_id: RequestId,
        violations: Vec<Violation>,
    ) {
        let mut incomplete_parser_contexts = self.incomplete_parser_contexts.0.borrow_mut();
        let parser = incomplete_parser_contexts
            .iter_mut()
            .find(|&&mut (parser_pipeline_id, _)| parser_pipeline_id == pipeline_id);
        let Some(&mut (_, ref mut ctxt)) = parser else {
            return;
        };
        // We need to report violations for navigations in iframes in the parent page
        let pipeline_id = ctxt.parent_info().unwrap_or(pipeline_id);
        if let Some(global) = self.documents.borrow().find_global(pipeline_id) {
            global.report_csp_violations(cx, violations, None, None);
        }
    }

    fn handle_navigation_redirect(&self, id: PipelineId, metadata: &Metadata) {
        // TODO(mrobinson): This tries to accomplish some steps from
        // <https://html.spec.whatwg.org/multipage/#process-a-navigate-fetch>, but it's
        // very out of sync with the specification.
        assert!(metadata.location_url.is_some());

        let mut incomplete_loads = self.incomplete_loads.borrow_mut();
        let Some(incomplete_load) = incomplete_loads
            .iter_mut()
            .find(|incomplete_load| incomplete_load.pipeline_id == id)
        else {
            return;
        };

        if let Some(observed_redirect) = controlled_session_redirect_limit_before_next_fetch(
            self.document_control_profile,
            incomplete_load.parent_info.is_none(),
            incomplete_load.url_list.len(),
        ) {
            let _ = self.senders.pipeline_to_constellation_sender.send((
                incomplete_load.webview_id,
                id,
                ScriptToConstellationMessage::ControlledSessionRedirectLimitExceeded {
                    observed: observed_redirect,
                },
            ));
            return;
        }

        // Update the `url_list` of the incomplete load to track all redirects. This will be reflected
        // in the new `RequestBuilder` as well.
        incomplete_load.url_list.push(metadata.final_url.clone());

        let mut request_builder = incomplete_load.request_builder();
        request_builder.referrer = metadata
            .referrer
            .clone()
            .map(Referrer::ReferrerUrl)
            .unwrap_or(Referrer::NoReferrer);
        request_builder.referrer_policy = metadata.referrer_policy;
        request_builder.origin = request_builder
            .client
            .as_ref()
            .expect("Must have a client during redirect")
            .origin
            .clone();

        let headers = metadata
            .headers
            .as_ref()
            .map(|headers| headers.clone().into_inner())
            .unwrap_or_default();

        let response_init = Some(ResponseInit {
            url: metadata.final_url.clone(),
            location_url: metadata.location_url.clone(),
            headers,
            referrer: metadata.referrer.clone(),
            status_code: metadata
                .status
                .try_code()
                .map(|code| code.as_u16())
                .unwrap_or(200),
        });

        incomplete_load.canceller = FetchCanceller::new(
            request_builder.id,
            false,
            self.resource_threads.core_thread.clone(),
        );
        let event_loop_sender = ScriptEventLoopSender::MainThread {
            sender: self.senders.self_sender.clone(),
            producer_fence: self.document_producer_fence.clone(),
        };
        if let Err(error) = NavigationListener::new(request_builder, event_loop_sender)
            .initiate_fetch(&self.resource_threads.core_thread, response_init)
        {
            // The old redirect response remains guarded until this handler returns. The failed new
            // admission is a sticky terminal, so never fall back to an untracked redirect fetch.
            error!("Refusing to start an unfenced redirect for pipeline {id:?}: {error}");
        }
    }

    /// Synchronously fetch a page with fixed content. Stores the `InProgressLoad`
    /// argument until a notification is received that the fetch is complete.
    fn start_synchronous_page_load(
        &self,
        cx: &mut js::context::JSContext,
        mut incomplete: InProgressLoad,
    ) {
        let mut context = ParserContext::new(
            incomplete.webview_id,
            incomplete.pipeline_id,
            incomplete.load_data.url.clone(),
            incomplete.load_data.creation_sandboxing_flag_set,
            incomplete.parent_info,
            incomplete.target_snapshot_params,
            incomplete.load_data.load_origin.clone(),
        );

        let mut meta = Metadata::default(incomplete.load_data.url.clone());
        meta.set_content_type(Some(&mime::TEXT_HTML));
        meta.set_referrer_policy(incomplete.load_data.referrer_policy);

        // If this page load is the result of a javascript scheme url, map
        // the evaluation result into a response.
        let chunk = match incomplete.load_data.js_eval_result {
            Some(ref mut content) => std::mem::take(content),
            None => String::new(),
        };

        let policy_container = incomplete.load_data.policy_container.clone();
        let about_base_url = incomplete.load_data.about_base_url.clone();
        self.incomplete_loads.borrow_mut().push(incomplete);

        let dummy_request_id = RequestId::default();
        context.process_response(cx, dummy_request_id, Ok(FetchMetadata::Unfiltered(meta)));
        context.set_policy_container(policy_container.as_ref());
        context.set_about_base_url(about_base_url);
        context.process_response_chunk(cx, dummy_request_id, chunk.into());
        context.process_response_eof(
            cx,
            dummy_request_id,
            Ok(()),
            ResourceFetchTiming::new(ResourceTimingType::None),
        );
    }

    /// Synchronously parse a srcdoc document from a giving HTML string.
    fn page_load_about_srcdoc(
        &self,
        cx: &mut js::context::JSContext,
        mut incomplete: InProgressLoad,
    ) {
        let url = ServoUrl::parse("about:srcdoc").unwrap();
        let mut meta = Metadata::default(url.clone());
        meta.set_content_type(Some(&mime::TEXT_HTML));
        meta.set_referrer_policy(incomplete.load_data.referrer_policy);

        let srcdoc = std::mem::take(&mut incomplete.load_data.srcdoc);
        let chunk = srcdoc.into_bytes();

        let policy_container = incomplete.load_data.policy_container.clone();
        let creation_sandboxing_flag_set = incomplete.load_data.creation_sandboxing_flag_set;

        let webview_id = incomplete.webview_id;
        let pipeline_id = incomplete.pipeline_id;
        let parent_info = incomplete.parent_info;
        let about_base_url = incomplete.load_data.about_base_url.clone();
        let target_snapshot_params = incomplete.target_snapshot_params;
        let load_origin = incomplete.load_data.load_origin.clone();
        self.incomplete_loads.borrow_mut().push(incomplete);

        let mut context = ParserContext::new(
            webview_id,
            pipeline_id,
            url,
            creation_sandboxing_flag_set,
            parent_info,
            target_snapshot_params,
            load_origin,
        );
        let dummy_request_id = RequestId::default();

        context.process_response(cx, dummy_request_id, Ok(FetchMetadata::Unfiltered(meta)));
        context.set_policy_container(policy_container.as_ref());
        context.set_about_base_url(about_base_url);
        context.process_response_chunk(cx, dummy_request_id, chunk);
        context.process_response_eof(
            cx,
            dummy_request_id,
            Ok(()),
            ResourceFetchTiming::new(ResourceTimingType::None),
        );
    }

    fn handle_css_error_reporting(
        &self,
        pipeline_id: PipelineId,
        filename: String,
        line: u32,
        column: u32,
        msg: String,
    ) {
        let Some(ref sender) = self.senders.devtools_server_sender else {
            return;
        };

        if let Some(window) = self.documents.borrow().find_window(pipeline_id) &&
            window.live_devtools_updates()
        {
            let css_error = CSSError {
                filename,
                line,
                column,
                msg,
            };
            let message = ScriptToDevtoolsControlMsg::ReportCSSError(pipeline_id, css_error);
            sender.send(message).unwrap();
        }
    }

    fn handle_navigate_to(&self, pipeline_id: PipelineId, url: ServoUrl) {
        // The constellation only needs to know the WebView ID for navigation,
        // but actors don't keep track of it. Infer WebView ID from pipeline ID instead.
        if let Some(document) = self.documents.borrow().find_document(pipeline_id) {
            let mut load_data = LoadData::new_for_new_unrelated_webview(url);
            if self.document_execution_profile == DocumentExecutionProfile::ControlledWebSessionV2 {
                load_data.controlled_cookie_site_for_cookies = document
                    .window()
                    .controlled_cookie_site_for_document(&document);
            }
            self.senders
                .pipeline_to_constellation_sender
                .send((
                    document.webview_id(),
                    pipeline_id,
                    ScriptToConstellationMessage::LoadUrl(
                        load_data,
                        NavigationHistoryBehavior::Push,
                        TargetSnapshotParams::default(),
                    ),
                ))
                .unwrap();
        }
    }

    fn handle_traverse_history(&self, pipeline_id: PipelineId, direction: TraversalDirection) {
        // The constellation only needs to know the WebView ID for navigation,
        // but actors don't keep track of it. Infer WebView ID from pipeline ID instead.
        if let Some(document) = self.documents.borrow().find_document(pipeline_id) {
            let webview_id = document.webview_id();
            self.senders
                .pipeline_to_constellation_sender
                .send((
                    webview_id,
                    pipeline_id,
                    ScriptToConstellationMessage::TraverseHistory(
                        SessionHistoryTraversalRequest::new(
                            webview_id,
                            direction,
                            HistoryTraversalSource::Script,
                        ),
                    ),
                ))
                .unwrap();
        }
    }

    fn handle_reload(&self, pipeline_id: PipelineId, cx: &mut js::context::JSContext) {
        let window = self.documents.borrow().find_window(pipeline_id);
        if let Some(window) = window {
            window.Location(cx).reload_without_origin_check(cx);
        }
    }

    fn handle_paint_metric(
        &self,
        cx: &mut js::context::JSContext,
        pipeline_id: PipelineId,
        metric_type: ProgressiveWebMetricType,
        metric_value: CrossProcessInstant,
        first_reflow: bool,
    ) {
        match self.documents.borrow().find_document(pipeline_id) {
            Some(document) => {
                document.handle_paint_metric(cx, metric_type, metric_value, first_reflow)
            },
            None => warn!(
                "Received paint metric ({metric_type:?}) for unknown document: {pipeline_id:?}"
            ),
        }
    }

    fn handle_media_session_action(
        &self,
        cx: &mut js::context::JSContext,
        pipeline_id: PipelineId,
        action: MediaSessionActionType,
    ) {
        if let Some(window) = self.documents.borrow().find_window(pipeline_id) {
            let media_session = window.Navigator(cx).MediaSession(cx);
            media_session.handle_action(cx, action);
        } else {
            warn!("No MediaSession for this pipeline ID");
        };
    }

    pub(crate) fn enqueue_microtask(cx: &js::context::JSContext, job: Box<dyn MicrotaskRunnable>) {
        with_script_thread(|script_thread| {
            script_thread.microtask_queue.enqueue(cx, job);
        });
    }

    pub(crate) fn perform_a_microtask_checkpoint(&self, cx: &mut js::context::JSContext) {
        // Only perform the checkpoint if we're not shutting down.
        if self.can_continue_running_inner() {
            let globals = self
                .documents
                .borrow()
                .iter()
                .map(|(_id, document)| DomRoot::from_ref(document.window().upcast()))
                .collect();

            self.microtask_queue.checkpoint(cx, globals)
        }
    }

    fn handle_evaluate_javascript(
        &self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        evaluation_id: JavaScriptEvaluationId,
        script: String,
        cx: &mut js::context::JSContext,
    ) {
        let Some(window) = self.documents.borrow().find_window(pipeline_id) else {
            let _ = self.senders.pipeline_to_constellation_sender.send((
                webview_id,
                pipeline_id,
                ScriptToConstellationMessage::FinishJavaScriptEvaluation(
                    evaluation_id,
                    Err(JavaScriptEvaluationError::WebViewNotReady),
                ),
            ));
            return;
        };

        let global_scope = window.as_global_scope();
        let mut realm = enter_auto_realm(cx, global_scope);
        let cx = &mut realm.current_realm();

        rooted!(&in(cx) let mut return_value = UndefinedValue());
        if let Err(err) = global_scope.evaluate_js_on_global(
            cx,
            script.into(),
            "",
            None, // No known `introductionType` for JS code from embedder
            Some(return_value.handle_mut()),
        ) {
            _ = self.senders.pipeline_to_constellation_sender.send((
                webview_id,
                pipeline_id,
                ScriptToConstellationMessage::FinishJavaScriptEvaluation(evaluation_id, Err(err)),
            ));
            return;
        };

        let result = jsval_to_webdriver(cx, global_scope, return_value.handle());
        let _ = self.senders.pipeline_to_constellation_sender.send((
            webview_id,
            pipeline_id,
            ScriptToConstellationMessage::FinishJavaScriptEvaluation(evaluation_id, result),
        ));
    }

    fn handle_refresh_cursor(&self, pipeline_id: PipelineId) {
        let Some(document) = self.documents.borrow().find_document(pipeline_id) else {
            return;
        };
        document.event_handler().handle_refresh_cursor();
    }

    pub(crate) fn is_servo_privileged(url: ServoUrl) -> bool {
        with_script_thread(|script_thread| script_thread.privileged_urls.contains(&url))
    }

    fn handle_request_screenshot_readiness(
        &self,
        webview_id: WebViewId,
        pipeline_id: PipelineId,
        cx: &mut js::context::JSContext,
    ) {
        let Some(window) = self.documents.borrow().find_window(pipeline_id) else {
            let _ = self.senders.pipeline_to_constellation_sender.send((
                webview_id,
                pipeline_id,
                ScriptToConstellationMessage::RespondToScreenshotReadinessRequest(
                    ScreenshotReadinessResponse::NoLongerActive,
                ),
            ));
            return;
        };
        window.request_screenshot_readiness(cx);
    }

    fn handle_embedder_control_response(
        &self,
        id: EmbedderControlId,
        response: EmbedderControlResponse,
        cx: &mut js::context::JSContext,
    ) {
        let Some(document) = self.documents.borrow().find_document(id.pipeline_id) else {
            return;
        };
        document
            .embedder_controls()
            .handle_embedder_control_response(cx, id, response);
    }

    pub(crate) fn handle_update_pinch_zoom_infos(
        &self,
        cx: &mut JSContext,
        pipeline_id: PipelineId,
        pinch_zoom_infos: PinchZoomInfos,
    ) {
        let Some(window) = self.documents.borrow().find_window(pipeline_id) else {
            warn!("Visual viewport update for closed pipeline {pipeline_id}.");
            return;
        };

        window.maybe_update_visual_viewport(cx, pinch_zoom_infos);
    }

    pub(crate) fn devtools_want_updates_for_node(pipeline: PipelineId, node: &Node) -> bool {
        with_script_thread(|script_thread| {
            script_thread
                .devtools_state
                .wants_updates_for_node(pipeline, node)
        })
    }
}

fn remaining_rendering_opportunity_delay(
    last_rendering_time: Option<DocumentTime>,
    now: DocumentTime,
    target_delay: Duration,
) -> Result<Duration, DocumentClockError> {
    let elapsed = match last_rendering_time {
        Some(last_rendering_time) => now.checked_duration_since(last_rendering_time)?,
        None => Duration::MAX,
    };
    Ok(target_delay - elapsed.min(target_delay))
}

fn renderer_may_drive_rendering(clock: &DocumentClock) -> bool {
    !clock.is_controlled()
}

fn controlled_producer_state_change_notifier(
    document_clock: &DocumentClock,
    script_to_embedder_sender: &ScriptToEmbedderChan,
) -> Option<Arc<dyn Fn() + Send + Sync>> {
    document_clock.is_controlled().then(|| {
        let script_to_embedder_sender = script_to_embedder_sender.clone();
        Arc::new(move || {
            let _ = script_to_embedder_sender.wake();
        }) as Arc<dyn Fn() + Send + Sync>
    })
}

fn document_producer_fence_for_clock(
    document_clock: &DocumentClock,
    script_to_embedder_sender: &ScriptToEmbedderChan,
) -> Option<DocumentProducerFence> {
    controlled_producer_state_change_notifier(document_clock, script_to_embedder_sender)
        .map(|notifier| DocumentProducerFence::with_notifier(Some(notifier)))
}

fn record_first_timer_control_error(
    terminal: &Cell<Option<TimerControlError>>,
    error: TimerControlError,
) {
    if terminal.get().is_none() {
        terminal.set(Some(error));
    }
}

fn try_schedule_timer_recording_terminal(
    timer_scheduler: &mut TimerScheduler,
    terminal: &Cell<Option<TimerControlError>>,
    request: TimerEventRequest,
) -> Result<TimerId, TimerControlError> {
    let result = timer_scheduler.try_schedule_timer(request);
    if let Err(error) = result {
        record_first_timer_control_error(terminal, error);
    }
    result
}

fn try_schedule_rendering_update_timer(
    timer_scheduler: &mut TimerScheduler,
    terminal: &Cell<Option<TimerControlError>>,
    trigger_rendering_update: Arc<AtomicBool>,
    delivery_ready: Arc<AtomicBool>,
    delay: Duration,
) -> Result<TimerId, TimerControlError> {
    try_schedule_timer_recording_terminal(
        timer_scheduler,
        terminal,
        TimerEventRequest {
            callback: Box::new(move || {
                delivery_ready.store(true, Ordering::SeqCst);
                trigger_rendering_update.store(true, Ordering::Relaxed);
            }),
            duration: delay,
        },
    )
}

impl Drop for ScriptThread {
    fn drop(&mut self) {
        SCRIPT_THREAD_ROOT.with(|root| {
            root.set(None);
        });
    }
}

#[cfg(test)]
mod controlled_image_delivery_tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use timers::{DocumentProducerFence, DocumentProducerFenceError, DocumentProducerKind};

    use super::{
        ControlledImageDeliveryTarget, ControlledImageMessageCompletion,
        controlled_image_delivery_target,
    };

    #[test]
    fn image_delivery_completes_only_live_or_proven_retired_targets() {
        assert_eq!(
            controlled_image_delivery_target(true, false),
            ControlledImageDeliveryTarget::Live
        );
        assert_eq!(
            controlled_image_delivery_target(false, true),
            ControlledImageDeliveryTarget::Retired
        );
        assert_eq!(
            controlled_image_delivery_target(false, false),
            ControlledImageDeliveryTarget::Unknown
        );
        assert_eq!(
            controlled_image_delivery_target(true, true),
            ControlledImageDeliveryTarget::Unknown
        );
    }

    #[test]
    fn image_handler_unwind_abandons_while_normal_return_completes() {
        let completed = DocumentProducerFence::default();
        drop(ControlledImageMessageCompletion::new(Some(
            completed.begin(DocumentProducerKind::Image).unwrap(),
        )));
        assert!(completed.snapshot().is_empty());
        assert_eq!(completed.snapshot().terminal_error(), None);

        let abandoned = DocumentProducerFence::default();
        let unwind = catch_unwind(AssertUnwindSafe(|| {
            let _completion = ControlledImageMessageCompletion::new(Some(
                abandoned.begin(DocumentProducerKind::Image).unwrap(),
            ));
            panic!("synthetic image handler panic");
        }));
        assert!(unwind.is_err());
        assert!(matches!(
            abandoned.snapshot().terminal_error(),
            Some(DocumentProducerFenceError::ProducerAbandoned(lease_id))
                if lease_id.kind() == DocumentProducerKind::Image
        ));
    }
}

#[cfg(test)]
mod controlled_session_history_tests {
    use std::cell::Cell;

    use embedder_traits::{
        CONTROLLED_SESSION_MAX_HISTORY_REVISIONS, CONTROLLED_SESSION_MAX_REDIRECTS,
        DocumentControlProfile,
    };
    use servo_base::id::{PipelineId, PipelineNamespaceId, TEST_PIPELINE_ID, TEST_WEBVIEW_ID};

    use super::{
        SynchronousNavigationEmissionCapture, SynchronousNavigationEmissionCaptureFailure,
        admit_controlled_session_history_revision,
        controlled_session_redirect_limit_before_next_fetch,
        record_synchronous_navigation_emission,
    };

    #[test]
    fn synchronous_navigation_capture_counts_success_and_latches_send_failure() {
        let active = SynchronousNavigationEmissionCapture::Active {
            webview_id: TEST_WEBVIEW_ID,
            pipeline_id: TEST_PIPELINE_ID,
            emissions: 0,
        };
        let counted =
            record_synchronous_navigation_emission(active, TEST_WEBVIEW_ID, TEST_PIPELINE_ID, true);
        assert_eq!(
            counted,
            SynchronousNavigationEmissionCapture::Active {
                webview_id: TEST_WEBVIEW_ID,
                pipeline_id: TEST_PIPELINE_ID,
                emissions: 1,
            }
        );
        assert_eq!(
            record_synchronous_navigation_emission(
                counted,
                TEST_WEBVIEW_ID,
                TEST_PIPELINE_ID,
                false,
            ),
            SynchronousNavigationEmissionCapture::Failed {
                webview_id: TEST_WEBVIEW_ID,
                pipeline_id: TEST_PIPELINE_ID,
                failure: SynchronousNavigationEmissionCaptureFailure::SendFailed,
            }
        );
    }

    #[test]
    fn synchronous_navigation_capture_fails_closed_at_its_checked_bound() {
        let at_limit = SynchronousNavigationEmissionCapture::Active {
            webview_id: TEST_WEBVIEW_ID,
            pipeline_id: TEST_PIPELINE_ID,
            emissions: CONTROLLED_SESSION_MAX_HISTORY_REVISIONS,
        };
        assert_eq!(
            record_synchronous_navigation_emission(
                at_limit,
                TEST_WEBVIEW_ID,
                TEST_PIPELINE_ID,
                true,
            ),
            SynchronousNavigationEmissionCapture::Failed {
                webview_id: TEST_WEBVIEW_ID,
                pipeline_id: TEST_PIPELINE_ID,
                failure: SynchronousNavigationEmissionCaptureFailure::EmissionLimitExceeded,
            }
        );
    }

    #[test]
    fn synchronous_navigation_capture_ignores_a_foreign_top_level_pipeline() {
        let active = SynchronousNavigationEmissionCapture::Active {
            webview_id: TEST_WEBVIEW_ID,
            pipeline_id: TEST_PIPELINE_ID,
            emissions: 0,
        };
        let foreign_pipeline = PipelineId {
            namespace_id: PipelineNamespaceId(4321),
            index: TEST_PIPELINE_ID.index,
        };
        assert_eq!(
            record_synchronous_navigation_emission(active, TEST_WEBVIEW_ID, foreign_pipeline, true,),
            active
        );
    }

    #[test]
    fn inactive_synchronous_navigation_capture_ignores_later_work() {
        assert_eq!(
            record_synchronous_navigation_emission(
                SynchronousNavigationEmissionCapture::Inactive,
                TEST_WEBVIEW_ID,
                TEST_PIPELINE_ID,
                true,
            ),
            SynchronousNavigationEmissionCapture::Inactive
        );
    }

    #[test]
    fn session_history_limit_rejects_before_the_revision_cell_is_mutated() {
        let revision = Cell::new(CONTROLLED_SESSION_MAX_HISTORY_REVISIONS);

        assert!(!admit_controlled_session_history_revision(
            DocumentControlProfile::TopLevelSession,
            &revision,
        ));
        assert_eq!(revision.get(), CONTROLLED_SESSION_MAX_HISTORY_REVISIONS);
    }

    #[test]
    fn session_history_limit_admits_max_then_rejects_max_plus_one_without_revision_drift() {
        let revision = Cell::new(CONTROLLED_SESSION_MAX_HISTORY_REVISIONS - 1);

        assert!(admit_controlled_session_history_revision(
            DocumentControlProfile::TopLevelSession,
            &revision,
        ));
        assert_eq!(revision.get(), CONTROLLED_SESSION_MAX_HISTORY_REVISIONS);

        assert!(!admit_controlled_session_history_revision(
            DocumentControlProfile::TopLevelSession,
            &revision,
        ));
        assert_eq!(revision.get(), CONTROLLED_SESSION_MAX_HISTORY_REVISIONS);
    }

    #[test]
    fn single_document_profile_does_not_opt_into_session_history_accounting() {
        let revision = Cell::new(7);

        assert!(admit_controlled_session_history_revision(
            DocumentControlProfile::SingleDocument,
            &revision,
        ));
        assert_eq!(revision.get(), 7);
    }

    #[test]
    fn redirect_hop_twenty_one_is_rejected_before_its_fetch_starts() {
        assert_eq!(
            controlled_session_redirect_limit_before_next_fetch(
                DocumentControlProfile::TopLevelSession,
                true,
                CONTROLLED_SESSION_MAX_REDIRECTS as usize,
            ),
            None,
        );
        assert_eq!(
            controlled_session_redirect_limit_before_next_fetch(
                DocumentControlProfile::TopLevelSession,
                true,
                CONTROLLED_SESSION_MAX_REDIRECTS as usize + 1,
            ),
            Some(CONTROLLED_SESSION_MAX_REDIRECTS + 1),
        );
    }
}

#[cfg(test)]
mod dom_mutation_epoch_tests {
    use servo_base::id::{BrowsingContextId, TEST_WEBVIEW_ID, WebViewId};
    use timers::{DocumentClock, DocumentClockConfiguration, DocumentUnixTime};

    use super::{
        DomMutationEpochError, DomMutationEpochTracker, DomMutationObservation,
        dom_mutation_epoch_tracker_for_clock,
    };

    fn other_webview_id() -> WebViewId {
        WebViewId::mock_for_testing(
            BrowsingContextId::from_string("BrowsingContext(0,2)")
                .expect("the test browsing-context id must be valid"),
        )
    }

    #[test]
    fn mutation_epochs_are_isolated_per_webview() {
        let mut tracker = DomMutationEpochTracker::default();
        let other_webview = other_webview_id();

        tracker.record(TEST_WEBVIEW_ID);
        tracker.record(TEST_WEBVIEW_ID);
        tracker.record(other_webview);

        assert_eq!(tracker.observe(TEST_WEBVIEW_ID).epoch, 2);
        assert_eq!(tracker.observe(other_webview).epoch, 1);
    }

    #[test]
    fn document_replacement_does_not_reset_a_webview_epoch() {
        let mut tracker = DomMutationEpochTracker::default();
        tracker.record(TEST_WEBVIEW_ID);
        let before_document_replacement = tracker.observe(TEST_WEBVIEW_ID);

        // Navigation replaces a Document or Pipeline, neither of which is a tracker key. The
        // replacement document therefore starts from the same WebView-owned epoch.
        let at_replacement_document_start = tracker.observe(TEST_WEBVIEW_ID);
        tracker.record(TEST_WEBVIEW_ID);

        assert_eq!(before_document_replacement.epoch, 1);
        assert_eq!(at_replacement_document_start, before_document_replacement);
        assert_eq!(tracker.observe(TEST_WEBVIEW_ID).epoch, 2);
    }

    #[test]
    fn observing_an_epoch_is_side_effect_free() {
        let mut tracker = DomMutationEpochTracker::default();
        tracker.record(TEST_WEBVIEW_ID);

        let first = tracker.observe(TEST_WEBVIEW_ID);
        let second = tracker.observe(TEST_WEBVIEW_ID);
        let untouched = tracker.observe(other_webview_id());

        assert_eq!(first, second);
        assert_eq!(first.epoch, 1);
        assert_eq!(untouched, DomMutationObservation::default());
    }

    #[test]
    fn overflow_latches_a_sticky_terminal_without_wrapping() {
        let mut tracker = DomMutationEpochTracker::default();
        tracker.observations.insert(
            TEST_WEBVIEW_ID,
            DomMutationObservation {
                epoch: u64::MAX - 1,
                terminal: None,
            },
        );

        tracker.record(TEST_WEBVIEW_ID);
        assert_eq!(
            tracker.observe(TEST_WEBVIEW_ID),
            DomMutationObservation {
                epoch: u64::MAX,
                terminal: None,
            }
        );

        tracker.record(TEST_WEBVIEW_ID);
        let exhausted = DomMutationObservation {
            epoch: u64::MAX,
            terminal: Some(DomMutationEpochError::Exhausted),
        };
        assert_eq!(tracker.observe(TEST_WEBVIEW_ID), exhausted);

        tracker.record(TEST_WEBVIEW_ID);
        assert_eq!(tracker.observe(TEST_WEBVIEW_ID), exhausted);
    }

    #[test]
    fn realtime_script_threads_do_not_collect_dom_epochs() {
        assert!(dom_mutation_epoch_tracker_for_clock(&DocumentClock::default()).is_none());

        let controlled = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 0,
            unix_time_origin_ns: DocumentUnixTime::default(),
        });
        assert!(dom_mutation_epoch_tracker_for_clock(&controlled).is_some());
    }
}

#[cfg(test)]
mod controlled_input_tests {
    use embedder_traits::document_control::{
        DocumentControlCancellationId, DocumentControlCommand, DocumentControlError,
        DocumentControlRequestId,
    };
    use embedder_traits::document_pending::{
        PendingActiveTopLevelPipeline, PendingInputRevision,
        PendingLogicalTimerTerminalObservation, PendingNavigationRevision,
        PendingPipelineMembershipRevision, PendingRuntimeTerminals, PendingTargetObservation,
    };
    use embedder_traits::{Theme, ViewportDetails};
    use script_traits::{
        DiscardBrowsingContext, NewPipelineInfo, ScriptThreadControlMessage, ScriptThreadMessage,
    };
    use servo_base::Epoch;
    use servo_base::id::{
        BrowsingContextId, Index, PipelineId, ScriptEventLoopId, TEST_BROWSING_CONTEXT_ID,
        TEST_NAMESPACE, TEST_PIPELINE_ID, TEST_WEBVIEW_ID,
    };
    use servo_constellation_traits::{LoadData, TargetSnapshotParams};
    use servo_url::ServoUrl;
    use timers::{
        DocumentClock, DocumentClockConfiguration, DocumentClockError, DocumentUnixTime,
        TimerScheduler,
    };

    use super::{
        CONTROLLED_INPUT_BATCH_LIMIT, ControlledDriveEventDisposition, ControlledInputBatch,
        ControlledInputState, ControlledLogicalTimerOwnerObservation,
        InitialPipelineActivationFacts, InitialPipelineActivationWaitInterruption,
        InitialPipelineBootstrapFacts, MainThreadScriptMsg, MixedMessage,
        ReplacementPipelineBootstrapFacts, ReplacementPipelineBootstrapQueueState,
        ReplacementPipelineBootstrapQueuedEvent, ScriptThread, controlled_drive_event_disposition,
        controlled_event_consumes_ordinary_task_budget, controlled_input_state_for_clock,
        initial_pipeline_activation_pipeline, initial_pipeline_activation_wait_interrupted,
        initial_pipeline_bootstrap_pipeline, replacement_pipeline_bootstrap_classified_position,
        replacement_pipeline_bootstrap_pipeline, replacement_pipeline_bootstrap_queued_event,
        take_controlled_lifecycle_event, take_controlled_turn,
    };

    #[test]
    fn controlled_task_classifier_excludes_only_event_loop_pump_markers() {
        let pump_markers = [
            MixedMessage::TimerFired,
            MixedMessage::FromScript(MainThreadScriptMsg::Inactive),
            MixedMessage::FromScript(MainThreadScriptMsg::WakeUp),
            MixedMessage::FromConstellation(ScriptThreadMessage::TickAllAnimations(Vec::new())),
        ];
        assert!(
            pump_markers
                .iter()
                .all(|event| !controlled_event_consumes_ordinary_task_budget(event))
        );

        let real_event = MixedMessage::FromConstellation(ScriptThreadMessage::ExitScriptThread);
        assert!(controlled_event_consumes_ordinary_task_budget(&real_event));
    }

    #[test]
    fn controlled_input_batch_is_bounded_and_conservatively_saturated() {
        let mut state = ControlledInputState::default();
        let mut input = (0..CONTROLLED_INPUT_BATCH_LIMIT + 1).map(|_| MixedMessage::TimerFired);

        assert_eq!(
            state.drain_bounded(&mut input),
            ControlledInputBatch {
                admitted: CONTROLLED_INPUT_BATCH_LIMIT,
                saturated: true,
            }
        );
        assert_eq!(state.ready_len(), CONTROLLED_INPUT_BATCH_LIMIT);
        assert_eq!(
            state.revision().unwrap().get(),
            CONTROLLED_INPUT_BATCH_LIMIT as u64
        );
        assert!(state.intake_saturated());
        assert_eq!(
            input.size_hint(),
            (1, Some(1)),
            "the capped suffix stays unconsumed"
        );

        assert_eq!(
            state.drain_bounded(&mut input),
            ControlledInputBatch {
                admitted: 1,
                saturated: false,
            }
        );
        assert_eq!(state.revision().unwrap().get(), 65);
        assert!(!state.intake_saturated());
    }

    #[test]
    fn revision_overflow_is_sticky_and_never_drops_owned_input() {
        let mut state = ControlledInputState {
            revision: PendingInputRevision::new(u64::MAX),
            ..ControlledInputState::default()
        };

        state.admit(MixedMessage::TimerFired);
        assert_eq!(state.ready_len(), 1);
        assert_eq!(state.last_revision(), PendingInputRevision::new(u64::MAX));
        assert!(state.revision_overflowed());
        assert_eq!(
            state.revision(),
            Err(DocumentControlError::InputRevisionOverflow)
        );

        state.admit(MixedMessage::TimerFired);
        assert_eq!(state.ready_len(), 2);
        assert_eq!(
            state.revision(),
            Err(DocumentControlError::InputRevisionOverflow)
        );
    }

    #[test]
    fn pipeline_discard_purges_only_matching_owned_input() {
        let mut state = ControlledInputState::default();
        state.admit(MixedMessage::FromConstellation(
            ScriptThreadMessage::ExitPipeline(
                TEST_WEBVIEW_ID,
                TEST_PIPELINE_ID,
                DiscardBrowsingContext::No,
            ),
        ));
        state.admit(MixedMessage::TimerFired);

        state.discard_pipeline(TEST_PIPELINE_ID);

        assert_eq!(state.ready_len(), 1);
        assert!(matches!(state.pop_front(), Some(MixedMessage::TimerFired)));
    }

    #[test]
    fn pipeline_discard_preserves_routed_control_for_a_typed_response() {
        let target = PendingTargetObservation::new_with_authority(
            TEST_WEBVIEW_ID,
            ScriptEventLoopId::new(),
            None,
            PendingNavigationRevision::new(1),
            PendingPipelineMembershipRevision::new(1),
            None,
            vec![TEST_PIPELINE_ID],
            Vec::new(),
            vec![TEST_PIPELINE_ID],
        )
        .unwrap();
        let request_id = DocumentControlRequestId::new(7);
        let cancellation_id = DocumentControlCancellationId::new(11);
        let mut state = ControlledInputState::default();
        state.admit_control(ScriptThreadControlMessage::Command {
            request_id,
            cancellation_id,
            target: Box::new(target),
            target_terminals: PendingRuntimeTerminals::default(),
            command: DocumentControlCommand::Observe,
        });

        state.discard_pipeline(TEST_PIPELINE_ID);

        assert!(matches!(
            state.take_control(),
            Some(ScriptThreadControlMessage::Command {
                request_id: observed_request_id,
                cancellation_id: observed_cancellation_id,
                ..
            }) if observed_request_id == request_id && observed_cancellation_id == cancellation_id
        ));
    }

    #[test]
    fn owner_queue_exists_only_for_controlled_time() {
        assert!(controlled_input_state_for_clock(&DocumentClock::default()).is_none());

        let controlled = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 0,
            unix_time_origin_ns: DocumentUnixTime::default(),
        });
        assert!(controlled_input_state_for_clock(&controlled).is_some());
    }

    #[test]
    fn lifecycle_input_bypasses_the_paused_ordinary_backlog() {
        let mut ready = std::collections::VecDeque::from([
            MixedMessage::TimerFired,
            MixedMessage::FromConstellation(ScriptThreadMessage::ExitScriptThread),
            MixedMessage::TimerFired,
        ]);

        assert!(matches!(
            take_controlled_lifecycle_event(&mut ready),
            Some(MixedMessage::FromConstellation(
                ScriptThreadMessage::ExitScriptThread
            ))
        ));
        assert_eq!(ready.len(), 2);
        assert!(matches!(ready.pop_front(), Some(MixedMessage::TimerFired)));
        assert!(matches!(ready.pop_front(), Some(MixedMessage::TimerFired)));
    }

    #[test]
    fn stale_source_drive_defers_spawn_for_exact_replacement_bootstrap() {
        let replacement = second_pipeline_id();
        let spawn =
            MixedMessage::FromConstellation(ScriptThreadMessage::SpawnPipeline(NewPipelineInfo {
                parent_info: None,
                new_pipeline_id: replacement,
                browsing_context_id: TEST_BROWSING_CONTEXT_ID,
                webview_id: TEST_WEBVIEW_ID,
                opener: None,
                load_data: LoadData::new_for_new_unrelated_webview(
                    ServoUrl::parse("https://replacement.example.test/").unwrap(),
                ),
                viewport_details: ViewportDetails::default(),
                user_content_manager_id: None,
                embedder_theme: Theme::Light,
                target_snapshot_params: TargetSnapshotParams::default(),
                frame_name: None,
            }));
        let mut owner_queue = std::collections::VecDeque::from([spawn]);

        assert!(matches!(
            controlled_drive_event_disposition(&owner_queue),
            ControlledDriveEventDisposition::PipelineBootstrapRequired
        ));
        assert_eq!(owner_queue.len(), 1);
        assert_eq!(
            replacement_pipeline_bootstrap_classified_position(
                owner_queue
                    .iter()
                    .map(replacement_pipeline_bootstrap_queued_event),
                false,
                replacement,
            ),
            ReplacementPipelineBootstrapQueueState::Ready { event_index: 0 }
        );
        owner_queue.push_back(MixedMessage::FromConstellation(
            ScriptThreadMessage::ExitScriptThread,
        ));
        assert!(matches!(
            controlled_drive_event_disposition(&owner_queue),
            ControlledDriveEventDisposition::Ready(Some(MixedMessage::FromConstellation(
                ScriptThreadMessage::ExitScriptThread
            )))
        ));
        for event in [
            MixedMessage::TimerFired,
            MixedMessage::FromConstellation(ScriptThreadMessage::ExitFullScreen(TEST_PIPELINE_ID)),
        ] {
            assert!(matches!(
                controlled_drive_event_disposition(&std::collections::VecDeque::from([event])),
                ControlledDriveEventDisposition::Ready(Some(_))
            ));
        }
    }

    #[test]
    fn replacement_bootstrap_selects_exact_spawn_through_ordinary_backlog() {
        let replacement = second_pipeline_id();
        let events = [
            ReplacementPipelineBootstrapQueuedEvent::Ordinary,
            ReplacementPipelineBootstrapQueuedEvent::Spawn(replacement),
            ReplacementPipelineBootstrapQueuedEvent::Ordinary,
        ];

        let state = replacement_pipeline_bootstrap_classified_position(events, false, replacement);
        assert_eq!(
            state,
            ReplacementPipelineBootstrapQueueState::Ready { event_index: 1 }
        );

        let mut retained = std::collections::VecDeque::from(events);
        retained.remove(1);
        assert_eq!(
            retained,
            std::collections::VecDeque::from([
                ReplacementPipelineBootstrapQueuedEvent::Ordinary,
                ReplacementPipelineBootstrapQueuedEvent::Ordinary,
            ])
        );
    }

    #[test]
    fn replacement_bootstrap_waits_until_saturated_intake_is_complete() {
        let replacement = second_pipeline_id();
        let events = [
            ReplacementPipelineBootstrapQueuedEvent::Ordinary,
            ReplacementPipelineBootstrapQueuedEvent::Spawn(replacement),
        ];

        assert_eq!(
            replacement_pipeline_bootstrap_classified_position([], false, replacement),
            ReplacementPipelineBootstrapQueueState::AwaitingInput,
            "cross-channel delivery lag is not definitive unavailability"
        );
        assert_eq!(
            replacement_pipeline_bootstrap_classified_position(events, true, replacement),
            ReplacementPipelineBootstrapQueueState::AwaitingInput
        );
        assert_eq!(
            replacement_pipeline_bootstrap_classified_position(events, false, replacement),
            ReplacementPipelineBootstrapQueueState::Ready { event_index: 1 }
        );
    }

    #[test]
    fn replacement_bootstrap_never_leapfrogs_lifecycle_or_immediate_input() {
        let replacement = second_pipeline_id();
        for events in [
            [
                ReplacementPipelineBootstrapQueuedEvent::Lifecycle,
                ReplacementPipelineBootstrapQueuedEvent::Spawn(replacement),
            ],
            [
                ReplacementPipelineBootstrapQueuedEvent::Spawn(replacement),
                ReplacementPipelineBootstrapQueuedEvent::Lifecycle,
            ],
        ] {
            assert_eq!(
                replacement_pipeline_bootstrap_classified_position(events, false, replacement),
                ReplacementPipelineBootstrapQueueState::Interrupted
            );
        }
        assert_eq!(
            replacement_pipeline_bootstrap_classified_position(
                [
                    ReplacementPipelineBootstrapQueuedEvent::ImmediateBarrier,
                    ReplacementPipelineBootstrapQueuedEvent::Spawn(replacement),
                ],
                false,
                replacement,
            ),
            ReplacementPipelineBootstrapQueueState::Unavailable
        );
        assert_eq!(
            replacement_pipeline_bootstrap_classified_position(
                [
                    ReplacementPipelineBootstrapQueuedEvent::Spawn(replacement),
                    ReplacementPipelineBootstrapQueuedEvent::ImmediateBarrier,
                ],
                false,
                replacement,
            ),
            ReplacementPipelineBootstrapQueueState::Ready { event_index: 0 }
        );
    }

    #[test]
    fn replacement_bootstrap_classifies_real_owner_queue_barriers() {
        assert_eq!(
            replacement_pipeline_bootstrap_queued_event(&MixedMessage::FromConstellation(
                ScriptThreadMessage::ExitPipeline(
                    TEST_WEBVIEW_ID,
                    TEST_PIPELINE_ID,
                    DiscardBrowsingContext::No,
                ),
            )),
            ReplacementPipelineBootstrapQueuedEvent::Lifecycle
        );
        assert_eq!(
            replacement_pipeline_bootstrap_queued_event(&MixedMessage::FromConstellation(
                ScriptThreadMessage::ExitScriptThread,
            )),
            ReplacementPipelineBootstrapQueuedEvent::Lifecycle
        );
        assert_eq!(
            replacement_pipeline_bootstrap_queued_event(&MixedMessage::FromConstellation(
                ScriptThreadMessage::ExitFullScreen(TEST_PIPELINE_ID),
            )),
            ReplacementPipelineBootstrapQueuedEvent::ImmediateBarrier
        );
        assert_eq!(
            replacement_pipeline_bootstrap_queued_event(&MixedMessage::TimerFired),
            ReplacementPipelineBootstrapQueuedEvent::Ordinary
        );
    }

    #[test]
    fn replacement_bootstrap_rejects_wrong_or_ambiguous_spawn_without_selection() {
        let replacement = second_pipeline_id();
        assert_eq!(
            replacement_pipeline_bootstrap_classified_position(
                [ReplacementPipelineBootstrapQueuedEvent::Spawn(
                    TEST_PIPELINE_ID
                )],
                false,
                replacement,
            ),
            ReplacementPipelineBootstrapQueueState::Unavailable
        );
        assert_eq!(
            replacement_pipeline_bootstrap_classified_position(
                [
                    ReplacementPipelineBootstrapQueuedEvent::Spawn(replacement),
                    ReplacementPipelineBootstrapQueuedEvent::Spawn(replacement),
                ],
                false,
                replacement,
            ),
            ReplacementPipelineBootstrapQueueState::Unavailable
        );
    }

    #[test]
    fn initial_activation_wait_observes_shutdown_and_lifecycle_without_consuming_input() {
        let ordinary = std::collections::VecDeque::from([MixedMessage::TimerFired]);
        assert_eq!(
            initial_pipeline_activation_wait_interrupted(TEST_PIPELINE_ID, false, &ordinary),
            None
        );
        assert_eq!(
            initial_pipeline_activation_wait_interrupted(TEST_PIPELINE_ID, true, &ordinary),
            Some(InitialPipelineActivationWaitInterruption::Closing)
        );

        let pipeline_exit = std::collections::VecDeque::from([
            MixedMessage::TimerFired,
            MixedMessage::FromConstellation(ScriptThreadMessage::ExitPipeline(
                TEST_WEBVIEW_ID,
                TEST_PIPELINE_ID,
                DiscardBrowsingContext::No,
            )),
            MixedMessage::TimerFired,
        ]);
        assert_eq!(
            initial_pipeline_activation_wait_interrupted(TEST_PIPELINE_ID, false, &pipeline_exit),
            Some(InitialPipelineActivationWaitInterruption::TerminalLifecycle)
        );
        assert_eq!(pipeline_exit.len(), 3);
        assert!(matches!(
            pipeline_exit.front(),
            Some(MixedMessage::TimerFired)
        ));
        assert!(matches!(
            pipeline_exit.back(),
            Some(MixedMessage::TimerFired)
        ));

        assert_eq!(
            initial_pipeline_activation_wait_interrupted(
                second_pipeline_id(),
                false,
                &pipeline_exit,
            ),
            Some(InitialPipelineActivationWaitInterruption::UnrelatedPipelineExit)
        );
        assert_eq!(pipeline_exit.len(), 3);

        let script_exit = std::collections::VecDeque::from([MixedMessage::FromConstellation(
            ScriptThreadMessage::ExitScriptThread,
        )]);
        assert_eq!(
            initial_pipeline_activation_wait_interrupted(TEST_PIPELINE_ID, true, &script_exit),
            Some(InitialPipelineActivationWaitInterruption::TerminalLifecycle)
        );
        assert_eq!(script_exit.len(), 1);
    }

    #[test]
    fn an_empty_drive_owns_one_synthetic_checkpoint_turn() {
        let (event, checkpoint_only) = take_controlled_turn(&mut std::collections::VecDeque::new());

        assert!(checkpoint_only);
        assert!(matches!(
            event,
            MixedMessage::FromScript(super::MainThreadScriptMsg::WakeUp)
        ));
    }

    fn second_pipeline_id() -> PipelineId {
        PipelineId {
            namespace_id: TEST_NAMESPACE,
            index: Index::new(TEST_PIPELINE_ID.index.0.get() + 1).unwrap(),
        }
    }

    fn initial_bootstrap_target() -> PendingTargetObservation {
        PendingTargetObservation::new_with_authority(
            TEST_WEBVIEW_ID,
            ScriptEventLoopId::new(),
            None,
            PendingNavigationRevision::new(1),
            PendingPipelineMembershipRevision::new(1),
            None,
            vec![TEST_PIPELINE_ID],
            Vec::new(),
            vec![TEST_PIPELINE_ID],
        )
        .unwrap()
    }

    fn initial_bootstrap_facts() -> InitialPipelineBootstrapFacts {
        InitialPipelineBootstrapFacts {
            pipeline_id: TEST_PIPELINE_ID,
            webview_id: TEST_WEBVIEW_ID,
            browsing_context_id: TEST_BROWSING_CONTEXT_ID,
            parent_pipeline_id: None,
            local_document_count: 0,
            local_incomplete_load_count: 0,
            local_parser_context_count: 0,
            is_http_or_https: true,
            has_javascript_result: false,
            has_srcdoc: false,
        }
    }

    fn initial_activation_facts() -> InitialPipelineActivationFacts {
        InitialPipelineActivationFacts {
            pipeline_id: TEST_PIPELINE_ID,
            webview_id: TEST_WEBVIEW_ID,
            browsing_context_id: TEST_BROWSING_CONTEXT_ID,
            parent_pipeline_id: None,
            local_document_pipeline_id: None,
            local_document_count: 0,
            local_incomplete_load_count: 1,
            local_parser_context_count: 1,
            parser_pipeline_id: Some(TEST_PIPELINE_ID),
            is_http_or_https: true,
            has_javascript_result: false,
            has_srcdoc: false,
            response_will_activate: true,
        }
    }

    fn replacement_bootstrap_target() -> PendingTargetObservation {
        PendingTargetObservation::new_with_authority(
            TEST_WEBVIEW_ID,
            ScriptEventLoopId::new(),
            Some(PendingActiveTopLevelPipeline {
                pipeline_id: TEST_PIPELINE_ID,
                epoch: Epoch(1),
            }),
            PendingNavigationRevision::new(2),
            PendingPipelineMembershipRevision::new(2),
            None,
            vec![TEST_PIPELINE_ID, second_pipeline_id()],
            vec![TEST_PIPELINE_ID],
            vec![second_pipeline_id()],
        )
        .unwrap()
    }

    fn replacement_bootstrap_facts() -> ReplacementPipelineBootstrapFacts {
        ReplacementPipelineBootstrapFacts {
            source_pipeline_id: TEST_PIPELINE_ID,
            pipeline_id: second_pipeline_id(),
            webview_id: TEST_WEBVIEW_ID,
            browsing_context_id: TEST_BROWSING_CONTEXT_ID,
            parent_pipeline_id: None,
            local_document_pipeline_id: Some(TEST_PIPELINE_ID),
            local_document_count: 1,
            local_incomplete_load_count: 0,
            local_parser_context_count: 0,
            is_http_or_https: true,
            has_javascript_result: false,
            has_srcdoc: false,
        }
    }

    fn replacement_activation_facts() -> InitialPipelineActivationFacts {
        InitialPipelineActivationFacts {
            pipeline_id: second_pipeline_id(),
            webview_id: TEST_WEBVIEW_ID,
            browsing_context_id: TEST_BROWSING_CONTEXT_ID,
            parent_pipeline_id: None,
            local_document_pipeline_id: Some(TEST_PIPELINE_ID),
            local_document_count: 1,
            local_incomplete_load_count: 1,
            local_parser_context_count: 1,
            parser_pipeline_id: Some(second_pipeline_id()),
            is_http_or_https: true,
            has_javascript_result: false,
            has_srcdoc: false,
            response_will_activate: true,
        }
    }

    #[test]
    fn initial_bootstrap_accepts_only_one_empty_http_root_target() {
        let target = initial_bootstrap_target();
        assert_eq!(
            initial_pipeline_bootstrap_pipeline(&target, initial_bootstrap_facts()),
            Some(TEST_PIPELINE_ID)
        );
        assert_eq!(
            initial_pipeline_bootstrap_pipeline(&target, initial_bootstrap_facts()),
            Some(TEST_PIPELINE_ID),
            "repeated passive qualification cannot consume hidden authorization"
        );

        let mut iframe = initial_bootstrap_facts();
        iframe.parent_pipeline_id = Some(second_pipeline_id());
        assert_eq!(initial_pipeline_bootstrap_pipeline(&target, iframe), None);

        let mut wrong_root = initial_bootstrap_facts();
        wrong_root.browsing_context_id = BrowsingContextId {
            namespace_id: TEST_NAMESPACE,
            index: Index::new(TEST_BROWSING_CONTEXT_ID.index.0.get() + 1).unwrap(),
        };
        assert_eq!(
            initial_pipeline_bootstrap_pipeline(&target, wrong_root),
            None
        );

        let mut synchronous_url = initial_bootstrap_facts();
        synchronous_url.is_http_or_https = false;
        assert_eq!(
            initial_pipeline_bootstrap_pipeline(&target, synchronous_url),
            None
        );

        for facts in [
            InitialPipelineBootstrapFacts {
                has_srcdoc: true,
                ..initial_bootstrap_facts()
            },
            InitialPipelineBootstrapFacts {
                has_javascript_result: true,
                ..initial_bootstrap_facts()
            },
        ] {
            assert_eq!(initial_pipeline_bootstrap_pipeline(&target, facts), None);
        }

        for facts in [
            InitialPipelineBootstrapFacts {
                local_document_count: 1,
                ..initial_bootstrap_facts()
            },
            InitialPipelineBootstrapFacts {
                local_incomplete_load_count: 1,
                ..initial_bootstrap_facts()
            },
            InitialPipelineBootstrapFacts {
                local_parser_context_count: 1,
                ..initial_bootstrap_facts()
            },
        ] {
            assert_eq!(initial_pipeline_bootstrap_pipeline(&target, facts), None);
        }
    }

    #[test]
    fn initial_bootstrap_rejects_active_replacement_and_multiple_targets() {
        let replacement_pipeline = second_pipeline_id();
        let active_replacement = PendingTargetObservation::new_with_authority(
            TEST_WEBVIEW_ID,
            ScriptEventLoopId::new(),
            Some(PendingActiveTopLevelPipeline {
                pipeline_id: TEST_PIPELINE_ID,
                epoch: Epoch(1),
            }),
            PendingNavigationRevision::new(2),
            PendingPipelineMembershipRevision::new(2),
            None,
            vec![TEST_PIPELINE_ID, replacement_pipeline],
            vec![TEST_PIPELINE_ID],
            vec![replacement_pipeline],
        )
        .unwrap();
        let replacement_facts = InitialPipelineBootstrapFacts {
            pipeline_id: replacement_pipeline,
            ..initial_bootstrap_facts()
        };
        assert_eq!(
            initial_pipeline_bootstrap_pipeline(&active_replacement, replacement_facts),
            None
        );

        let multiple_pending = PendingTargetObservation::new_with_authority(
            TEST_WEBVIEW_ID,
            ScriptEventLoopId::new(),
            None,
            PendingNavigationRevision::new(1),
            PendingPipelineMembershipRevision::new(1),
            None,
            vec![TEST_PIPELINE_ID, replacement_pipeline],
            Vec::new(),
            vec![TEST_PIPELINE_ID, replacement_pipeline],
        )
        .unwrap();
        assert_eq!(
            initial_pipeline_bootstrap_pipeline(&multiple_pending, initial_bootstrap_facts()),
            None
        );
    }

    #[test]
    fn replacement_bootstrap_accepts_only_exact_active_source_and_pending_root() {
        let target = replacement_bootstrap_target();
        assert_eq!(
            replacement_pipeline_bootstrap_pipeline(&target, replacement_bootstrap_facts()),
            Some(second_pipeline_id())
        );
        assert_eq!(
            replacement_pipeline_bootstrap_pipeline(&target, replacement_bootstrap_facts()),
            Some(second_pipeline_id()),
            "passive qualification remains non-consuming"
        );

        for facts in [
            ReplacementPipelineBootstrapFacts {
                source_pipeline_id: second_pipeline_id(),
                ..replacement_bootstrap_facts()
            },
            ReplacementPipelineBootstrapFacts {
                pipeline_id: TEST_PIPELINE_ID,
                ..replacement_bootstrap_facts()
            },
            ReplacementPipelineBootstrapFacts {
                local_document_pipeline_id: Some(second_pipeline_id()),
                ..replacement_bootstrap_facts()
            },
            ReplacementPipelineBootstrapFacts {
                local_document_count: 2,
                ..replacement_bootstrap_facts()
            },
            ReplacementPipelineBootstrapFacts {
                local_incomplete_load_count: 1,
                ..replacement_bootstrap_facts()
            },
            ReplacementPipelineBootstrapFacts {
                local_parser_context_count: 1,
                ..replacement_bootstrap_facts()
            },
            ReplacementPipelineBootstrapFacts {
                parent_pipeline_id: Some(TEST_PIPELINE_ID),
                ..replacement_bootstrap_facts()
            },
            ReplacementPipelineBootstrapFacts {
                is_http_or_https: false,
                ..replacement_bootstrap_facts()
            },
            ReplacementPipelineBootstrapFacts {
                has_javascript_result: true,
                ..replacement_bootstrap_facts()
            },
            ReplacementPipelineBootstrapFacts {
                has_srcdoc: true,
                ..replacement_bootstrap_facts()
            },
        ] {
            assert_eq!(
                replacement_pipeline_bootstrap_pipeline(&target, facts),
                None
            );
        }
        assert_eq!(
            replacement_pipeline_bootstrap_pipeline(
                &initial_bootstrap_target(),
                replacement_bootstrap_facts(),
            ),
            None
        );
    }

    #[test]
    fn initial_activation_accepts_only_one_bootstrapped_http_headers_turn() {
        let target = initial_bootstrap_target();
        assert_eq!(
            initial_pipeline_activation_pipeline(&target, initial_activation_facts()),
            Some(TEST_PIPELINE_ID)
        );

        for facts in [
            InitialPipelineActivationFacts {
                response_will_activate: false,
                ..initial_activation_facts()
            },
            InitialPipelineActivationFacts {
                parent_pipeline_id: Some(second_pipeline_id()),
                ..initial_activation_facts()
            },
            InitialPipelineActivationFacts {
                local_document_pipeline_id: Some(second_pipeline_id()),
                local_document_count: 1,
                ..initial_activation_facts()
            },
            InitialPipelineActivationFacts {
                local_incomplete_load_count: 2,
                ..initial_activation_facts()
            },
            InitialPipelineActivationFacts {
                local_parser_context_count: 0,
                parser_pipeline_id: None,
                ..initial_activation_facts()
            },
            InitialPipelineActivationFacts {
                parser_pipeline_id: Some(second_pipeline_id()),
                ..initial_activation_facts()
            },
            InitialPipelineActivationFacts {
                is_http_or_https: false,
                ..initial_activation_facts()
            },
            InitialPipelineActivationFacts {
                has_javascript_result: true,
                ..initial_activation_facts()
            },
            InitialPipelineActivationFacts {
                has_srcdoc: true,
                ..initial_activation_facts()
            },
        ] {
            assert_eq!(initial_pipeline_activation_pipeline(&target, facts), None);
        }
    }

    #[test]
    fn replacement_activation_accepts_only_the_pending_pipeline_headers_turn() {
        let target = replacement_bootstrap_target();
        assert_eq!(
            initial_pipeline_activation_pipeline(&target, replacement_activation_facts()),
            Some(second_pipeline_id())
        );

        for facts in [
            InitialPipelineActivationFacts {
                pipeline_id: TEST_PIPELINE_ID,
                parser_pipeline_id: Some(TEST_PIPELINE_ID),
                ..replacement_activation_facts()
            },
            InitialPipelineActivationFacts {
                local_document_pipeline_id: Some(second_pipeline_id()),
                ..replacement_activation_facts()
            },
            InitialPipelineActivationFacts {
                local_document_count: 0,
                ..replacement_activation_facts()
            },
            InitialPipelineActivationFacts {
                parser_pipeline_id: Some(TEST_PIPELINE_ID),
                ..replacement_activation_facts()
            },
            InitialPipelineActivationFacts {
                response_will_activate: false,
                ..replacement_activation_facts()
            },
        ] {
            assert_eq!(initial_pipeline_activation_pipeline(&target, facts), None);
        }
    }

    #[test]
    fn logical_timer_capture_preserves_owner_terminal_without_scheduler_reentry() {
        let terminal = PendingLogicalTimerTerminalObservation {
            pipeline_id: TEST_PIPELINE_ID,
            error: DocumentClockError::Overflow,
        };
        let scheduler = TimerScheduler::default();

        let (facts, terminals) = ScriptThread::capture_controlled_logical_timers(
            vec![ControlledLogicalTimerOwnerObservation::Terminal(terminal)],
            &scheduler,
        )
        .unwrap();

        assert!(facts.is_empty());
        assert_eq!(terminals, vec![terminal]);
    }

    #[test]
    fn active_cancellation_leaves_owner_ready_input_untouched() {
        let request_id = DocumentControlRequestId::new(7);
        let cancellation_id = DocumentControlCancellationId::new(11);
        let mut state = ControlledInputState::default();
        state.admit(MixedMessage::TimerFired);
        state.admit(MixedMessage::FromScript(MainThreadScriptMsg::WakeUp));
        let before_revision = state.revision().unwrap();
        let mut controls = [ScriptThreadControlMessage::Cancel {
            request_id,
            cancellation_id,
        }]
        .into_iter();

        let batch =
            state.drain_controls_bounded(&mut controls, Some((request_id, cancellation_id)));

        assert!(batch.active_cancelled);
        assert_eq!(state.revision().unwrap(), before_revision);
        assert_eq!(state.ready_len(), 2);
        assert!(matches!(state.pop_front(), Some(MixedMessage::TimerFired)));
        assert!(matches!(
            state.pop_front(),
            Some(MixedMessage::FromScript(MainThreadScriptMsg::WakeUp))
        ));
    }

    #[test]
    fn an_authenticated_active_cancellation_retires_only_the_exact_replay() {
        let request_id = DocumentControlRequestId::new(7);
        let cancellation_id = DocumentControlCancellationId::new(11);
        let mut controls = [ScriptThreadControlMessage::Cancel {
            request_id,
            cancellation_id,
        }]
        .into_iter();
        let mut state = ControlledInputState::default();

        let batch =
            state.drain_controls_bounded(&mut controls, Some((request_id, cancellation_id)));

        assert_eq!(batch.admitted, 1);
        assert!(!batch.saturated);
        assert!(batch.active_cancelled);
        assert_eq!(state.ready_len(), 0);
        assert_eq!(state.revision().unwrap(), PendingInputRevision::ZERO);
        assert!(state.take_control().is_none());

        state.admit_control(ScriptThreadControlMessage::Command {
            request_id,
            cancellation_id,
            target: Box::new(initial_bootstrap_target()),
            target_terminals: PendingRuntimeTerminals::default(),
            command: DocumentControlCommand::DriveOneTurn,
        });
        assert!(state.take_control().is_none());

        let other_cancellation_id = DocumentControlCancellationId::new(12);
        state.admit_control(ScriptThreadControlMessage::Command {
            request_id,
            cancellation_id: other_cancellation_id,
            target: Box::new(initial_bootstrap_target()),
            target_terminals: PendingRuntimeTerminals::default(),
            command: DocumentControlCommand::DriveOneTurn,
        });
        assert!(matches!(
            state.take_control(),
            Some(ScriptThreadControlMessage::Command {
                request_id: observed_request_id,
                cancellation_id: observed_cancellation_id,
                ..
            }) if observed_request_id == request_id &&
                observed_cancellation_id == other_cancellation_id
        ));
    }
}

#[cfg(test)]
mod document_clock_tests {
    use std::cell::Cell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use embedder_traits::{EventLoopWaker, ScriptToEmbedderChan};
    use timers::{
        DocumentClock, DocumentClockConfiguration, DocumentClockError, DocumentProducerKind,
        DocumentTime, DocumentUnixTime, TimerControlError, TimerScheduler,
    };

    use super::{
        document_producer_fence_for_clock, record_first_timer_control_error,
        remaining_rendering_opportunity_delay, renderer_may_drive_rendering,
        try_schedule_rendering_update_timer,
    };

    #[derive(Clone)]
    struct CountingEventLoopWaker(Arc<AtomicUsize>);

    impl EventLoopWaker for CountingEventLoopWaker {
        fn clone_box(&self) -> Box<dyn EventLoopWaker> {
            Box::new(self.clone())
        }

        fn wake(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn rendering_opportunity_delay_uses_checked_document_time() {
        let target = Duration::from_millis(20);

        assert_eq!(
            remaining_rendering_opportunity_delay(
                Some(DocumentTime::from_nanos(10_000_000)),
                DocumentTime::from_nanos(15_000_000),
                target,
            ),
            Ok(Duration::from_millis(15))
        );
        assert_eq!(
            remaining_rendering_opportunity_delay(None, DocumentTime::ZERO, target),
            Ok(Duration::ZERO)
        );
        assert_eq!(
            remaining_rendering_opportunity_delay(
                Some(DocumentTime::from_nanos(2)),
                DocumentTime::from_nanos(1),
                target,
            ),
            Err(DocumentClockError::TimeMovedBackwards {
                current: DocumentTime::from_nanos(2),
                requested: DocumentTime::from_nanos(1),
            })
        );
    }

    #[test]
    fn renderer_ticks_cannot_activate_controlled_rendering() {
        let controlled = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 0,
            unix_time_origin_ns: DocumentUnixTime::default(),
        });

        assert!(!renderer_may_drive_rendering(&controlled));
        assert!(renderer_may_drive_rendering(&DocumentClock::default()));
    }

    #[test]
    fn controlled_producer_wakes_do_not_require_settlement_accounting() {
        let (embedder_sender, embedder_receiver) = crossbeam_channel::unbounded();
        let wake_count = Arc::new(AtomicUsize::new(0));
        let sender = ScriptToEmbedderChan::new(
            embedder_sender,
            Box::new(CountingEventLoopWaker(wake_count.clone())),
        );
        let controlled_clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 0,
            unix_time_origin_ns: DocumentUnixTime::default(),
        });
        let fence = document_producer_fence_for_clock(&controlled_clock, &sender)
            .expect("controlled clocks must install producer tracking");

        let producer = fence.begin(DocumentProducerKind::Task).unwrap();
        assert_eq!(wake_count.load(Ordering::SeqCst), 1);
        drop(producer);
        assert_eq!(wake_count.load(Ordering::SeqCst), 2);
        assert!(matches!(
            embedder_receiver.try_recv(),
            Err(crossbeam_channel::TryRecvError::Empty)
        ));

        assert!(document_producer_fence_for_clock(&DocumentClock::default(), &sender).is_none());
        assert_eq!(wake_count.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn rendering_timer_preserves_a_controlled_clock_terminal_without_panicking() {
        const ADVERSARIAL_MILLISECONDS: i128 = 8_639_999_999_999_979;

        let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 0,
            unix_time_origin_ns: DocumentUnixTime::from_nanos(ADVERSARIAL_MILLISECONDS * 1_000_000),
        });
        let terminal = clock
            .javascript_date_time_microseconds()
            .expect_err("adversarial Date time must latch a precision terminal");
        let trigger = Arc::new(AtomicBool::new(false));
        let delivery_ready = Arc::new(AtomicBool::new(false));
        let mut scheduler = TimerScheduler::with_clock(clock.clone());
        let timer_terminal = Cell::new(None);

        assert_eq!(
            try_schedule_rendering_update_timer(
                &mut scheduler,
                &timer_terminal,
                trigger.clone(),
                delivery_ready.clone(),
                Duration::ZERO,
            ),
            Err(TimerControlError::Clock(terminal))
        );
        assert!(!trigger.load(Ordering::Relaxed));
        assert!(!delivery_ready.load(Ordering::SeqCst));
        assert_eq!(clock.terminal_error(), Some(terminal));
        assert_eq!(
            timer_terminal.get(),
            Some(TimerControlError::Clock(terminal))
        );
    }

    #[test]
    fn rendering_timer_still_runs_on_the_realtime_scheduler() {
        let trigger = Arc::new(AtomicBool::new(false));
        let delivery_ready = Arc::new(AtomicBool::new(false));
        let mut scheduler = TimerScheduler::default();
        let timer_terminal = Cell::new(None);

        try_schedule_rendering_update_timer(
            &mut scheduler,
            &timer_terminal,
            trigger.clone(),
            delivery_ready.clone(),
            Duration::ZERO,
        )
        .expect("realtime rendering timer should schedule");
        scheduler.dispatch_completed_timers();

        assert!(trigger.load(Ordering::Relaxed));
        assert!(delivery_ready.load(Ordering::SeqCst));
        assert_eq!(timer_terminal.get(), None);
    }

    #[test]
    fn controlled_exact_now_rendering_timer_requires_advance_and_then_becomes_ready() {
        let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 1_000_000_000,
            unix_time_origin_ns: DocumentUnixTime::default(),
        });
        let trigger = Arc::new(AtomicBool::new(false));
        let delivery_ready = Arc::new(AtomicBool::new(false));
        let mut scheduler = TimerScheduler::with_clock(clock.clone());
        let timer_terminal = Cell::new(None);

        try_schedule_rendering_update_timer(
            &mut scheduler,
            &timer_terminal,
            trigger.clone(),
            delivery_ready.clone(),
            Duration::ZERO,
        )
        .unwrap();
        let head = scheduler.finite_deadline_snapshot().unwrap().unwrap();
        assert_eq!(head.deadline, clock.now());

        scheduler.dispatch_completed_timers();
        assert!(!trigger.load(Ordering::Relaxed));
        assert!(!delivery_ready.load(Ordering::SeqCst));

        let detached = scheduler
            .validate_advance_and_detach(clock.now(), head)
            .unwrap();
        assert!(!trigger.load(Ordering::Relaxed));
        assert!(!delivery_ready.load(Ordering::SeqCst));
        detached.dispatch();

        assert!(trigger.load(Ordering::Relaxed));
        assert!(delivery_ready.load(Ordering::SeqCst));
        assert_eq!(timer_terminal.get(), None);
    }

    #[test]
    fn timer_control_terminal_keeps_the_first_non_clock_failure() {
        let terminal = Cell::new(None);

        record_first_timer_control_error(&terminal, TimerControlError::DeadlineOverflow);
        record_first_timer_control_error(&terminal, TimerControlError::SequenceExhausted);

        assert_eq!(terminal.get(), Some(TimerControlError::DeadlineOverflow));
    }
}

/// Steps 1, 5, and 6 of <https://html.spec.whatwg.org/multipage/#initialise-the-document-object>
fn window_for_replacement(
    script_window_proxies: &ScriptWindowProxies,
    id: BrowsingContextId,
    origin: &MutableOrigin,
) -> Option<DomRoot<Window>> {
    // Step 1. Let browsingContext be the result of obtaining a browsing context
    //   to use for a navigation response given navigationParams.
    let browsing_context = obtain_a_browsing_context(script_window_proxies, id, origin);

    // Step 5. Let window be null.
    // Step 6. If browsingContext's active document's is initial about:blank is true,
    //   and browsingContext's active document's origin is same origin-domain with
    //   navigationParams's origin, then set window to browsingContext's active window.
    browsing_context
        .and_then(|window_proxy| window_proxy.document())
        .filter(|document| {
            document.is_initial_about_blank() && document.origin().same_origin_domain(origin)
        })
        .map(|document| DomRoot::from_ref(document.window()))
}

/// <https://html.spec.whatwg.org/multipage/#obtain-browsing-context-navigation>
fn obtain_a_browsing_context(
    script_window_proxies: &ScriptWindowProxies,
    id: BrowsingContextId,
    destination_origin: &MutableOrigin,
) -> Option<DomRoot<WindowProxy>> {
    // Step 1. Let browsingContext be navigationParams's navigable's active browsing context.
    let browsing_context = script_window_proxies.find_window_proxy(id)?;
    // Step 2. If browsingContext is not a top-level browsing context, then return browsingContext.
    if browsing_context.parent().is_none() {
        return Some(browsing_context);
    }
    // Step 3. Let coopEnforcementResult be navigationParams's COOP enforcement result.
    // TODO
    // Step 4. Let swapGroup be coopEnforcementResult's needs a browsing context group switch.
    // TODO
    let swap_group = false;
    // Step 5. Let sourceOrigin be browsingContext's active document's origin.
    let document = browsing_context.document()?;
    let source_origin = document.origin();
    // Step 6. Let destinationOrigin be navigationParams's origin.
    // Passed as `destination_origin`.
    // Step 7. If sourceOrigin is not same site with destinationOrigin:
    if !is_same_site(source_origin.immutable(), destination_origin.immutable()) {
        // Step 7.1. If either of sourceOrigin or destinationOrigin have a scheme that is not an
        //   HTTP(S) scheme and the user agent considers it necessary for sourceOrigin and
        //   destinationOrigin to be isolated from each other (for implementation-defined reasons),
        //   optionally set swapGroup to true.
        // TODO
        // Step 7.2. If navigationParams's user involvement is "browser UI", optionally set
        //   swapGroup to true.
        // TODO
    }
    // Step 8. If browsingContext's group's browsing context set's size is 1, optionally set
    //   swapGroup to true.
    // TODO
    // Step 9. If swapGroup is false:
    if !swap_group {
        // Step 9.1. If coopEnforcementResult's would need a browsing context group switch due to
        //   report-only is true, set browsingContext's virtual browsing context group ID to a new
        //   unique identifier.
        // TODO
        // Step 9.2. Return browsingContext.
        return Some(browsing_context);
    }
    // Step 10. Let newBrowsingContext be the first return value of creating a new top-level browsing context and document.
    // Step 11. Let navigationCOOP be navigationParams's cross-origin opener policy.
    // Step 12. If navigationCOOP's value is "same-origin-plus-COEP", then set newBrowsingContext's
    //   group's cross-origin isolation mode to either "logical" or "concrete". The choice of which
    //   is implementation-defined.
    // Step 13. Let sandboxFlags be a clone of navigationParams's final sandboxing flag set.
    // Step 14. If sandboxFlags is not empty:
    // Step 14.1. Assert: navigationCOOP's value is "unsafe-none".
    // Step 14.2. Assert: newBrowsingContext's popup sandboxing flag set is empty.
    // Step 14.3. Set newBrowsingContext's popup sandboxing flag set to sandboxFlags.
    // Step 15. Return newBrowsingContext.
    // TODO
    Some(browsing_context)
}
