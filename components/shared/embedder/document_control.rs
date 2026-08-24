/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Same-build control types for observing and mechanically driving one document event loop.
//!
//! These types are an internal Servo protocol. In particular, advance tokens contain
//! process-local clock, scheduler, event-loop, and producer-fence identities which must never be
//! projected directly into a product wire protocol.
//!
//! Wiring must preserve a stricter linearization boundary than these passive types can enforce:
//! invalidate any retained token before accepting `Observe` or `DriveOneTurn`, consume a retained
//! token on every `AdvanceTo` attempt, and perform the final input intake, proof comparison, clock
//! advance, and scheduler-head mutation under one producer-fence exclusion. A failure after a
//! drive or advance may have committed and therefore cannot be reported as a definitive rejection.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use servo_base::generic_channel::{GenericReceiver, ReceiveError, TryReceiveError};
use servo_base::id::PipelineId;
use timers::{
    DocumentClockError, DocumentClockId, DocumentProducerFenceError, DocumentTime,
    DocumentTimeSurface, TimerControlError, TimerDeadlineSnapshot, TimerSchedulerId,
};

use crate::document_automation::{
    DocumentAutomationError, DocumentAutomationOperation, DocumentAutomationOperationKind,
    DocumentAutomationRequest, DocumentAutomationResult,
};
use crate::document_pending::{
    PendingClockMode, PendingExternalIoObservation, PendingExternalIoPhase,
    PendingInputObservation, PendingLogicalTimerObservation, PendingMicrotaskObservation,
    PendingNavigationRevision, PendingParserPhase, PendingParserSourceObservation,
    PendingPipelineRenderingObservation, PendingProducerObservation, PendingProducerStability,
    PendingSnapshotInvariantError, PendingTargetObservation, RawPendingSnapshot,
    RuntimeStateGeneration,
};

/// Stable identity for one in-flight document-control request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct DocumentControlRequestId(u64);

impl DocumentControlRequestId {
    /// Construct an identifier from a checked Constellation sequence.
    #[doc(hidden)]
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Return the underlying request sequence.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Trusted-channel correlation nonce for abandoning one control response.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct DocumentControlCancellationId(u64);

impl DocumentControlCancellationId {
    /// Construct an identifier from a checked per-WebView sequence.
    #[doc(hidden)]
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Return the underlying cancellation sequence.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Stable identity of one ScriptThread-issued conditional-advance precondition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct DocumentAdvanceTokenId(u64);

impl DocumentAdvanceTokenId {
    /// Construct an identifier from a checked ScriptThread sequence.
    #[doc(hidden)]
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Return the underlying ScriptThread sequence.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A single-use precondition for activating one exact future scheduler head.
///
/// The fields are private so callers cannot assemble or weaken individual facts. ScriptThread
/// must retain the exact issued token, consume it on every attempted advance, and compare it with
/// a newly captured [`RawPendingSnapshot`] at the action's linearization point.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DocumentAdvanceToken {
    id: DocumentAdvanceTokenId,
    target: PendingTargetObservation,
    state_generation: RuntimeStateGeneration,
    clock: crate::document_pending::PendingClockObservation,
    scheduler_id: TimerSchedulerId,
    deadline: TimerDeadlineSnapshot,
    input: PendingInputObservation,
    microtasks: PendingMicrotaskObservation,
    producers: PendingProducerObservation,
}

impl DocumentAdvanceToken {
    /// Issue a token from one internally qualified raw observation.
    #[doc(hidden)]
    pub fn new_internal(
        id: DocumentAdvanceTokenId,
        pending: &RawPendingSnapshot,
    ) -> Result<Self, DocumentAdvanceTokenInvariantError> {
        let authority = DocumentAdvanceAuthority::from_pending(pending)?;
        Ok(Self {
            id,
            target: authority.target,
            state_generation: authority.state_generation,
            clock: authority.clock,
            scheduler_id: authority.scheduler_id,
            deadline: authority.deadline,
            input: authority.input,
            microtasks: authority.microtasks,
            producers: authority.producers,
        })
    }

    /// Return this token's single-use sequence.
    pub const fn id(&self) -> DocumentAdvanceTokenId {
        self.id
    }

    /// Return the exact target authority bound by this token.
    pub const fn target(&self) -> &PendingTargetObservation {
        &self.target
    }

    /// Return the complete-state generation observed when this token was issued.
    pub const fn state_generation(&self) -> RuntimeStateGeneration {
        self.state_generation
    }

    /// Return the identity of the one clock domain this token may advance.
    pub const fn clock_id(&self) -> DocumentClockId {
        self.clock.clock_id
    }

    /// Return the clock offset observed when this token was issued.
    pub const fn now(&self) -> DocumentTime {
        self.clock.now
    }

    /// Return the identity of the one scheduler this token may mutate.
    pub const fn scheduler_id(&self) -> TimerSchedulerId {
        self.scheduler_id
    }

    /// Return the exact finite scheduler entry this token may activate.
    pub const fn deadline(&self) -> TimerDeadlineSnapshot {
        self.deadline
    }

    /// Return the exact ordinary-input facts bound by this token.
    pub const fn input(&self) -> PendingInputObservation {
        self.input
    }

    /// Return the ordinary-input revision bound by this token.
    pub const fn input_revision(&self) -> crate::document_pending::PendingInputRevision {
        self.input.revision
    }

    /// Return whether bounded input intake was saturated at token issuance.
    pub const fn input_intake_saturated(&self) -> bool {
        self.input.intake_saturated
    }

    /// Return the exact microtask checkpoint proof bound by this token.
    pub const fn microtasks(&self) -> PendingMicrotaskObservation {
        self.microtasks
    }

    /// Return the exact producer-fence proof bound by this token.
    pub const fn producers(&self) -> PendingProducerObservation {
        self.producers
    }

    /// Revalidate every private precondition against a fresh authoritative observation.
    pub fn validate_against(
        &self,
        pending: &RawPendingSnapshot,
    ) -> Result<(), DocumentAdvanceTokenInvariantError> {
        let observed = DocumentAdvanceAuthority::from_pending(pending)?;
        if self.target != observed.target {
            return Err(DocumentAdvanceTokenInvariantError::TargetChanged {
                expected: Box::new(self.target.clone()),
                observed: Box::new(observed.target),
            });
        }
        if self.state_generation != observed.state_generation {
            return Err(DocumentAdvanceTokenInvariantError::StateGenerationChanged {
                expected: self.state_generation,
                observed: observed.state_generation,
            });
        }
        if self.clock != observed.clock {
            return Err(DocumentAdvanceTokenInvariantError::ClockChanged {
                expected_id: self.clock.clock_id,
                observed_id: observed.clock.clock_id,
                expected_now: self.clock.now,
                observed_now: observed.clock.now,
            });
        }
        if self.scheduler_id != observed.scheduler_id {
            return Err(DocumentAdvanceTokenInvariantError::SchedulerChanged {
                expected: self.scheduler_id,
                observed: observed.scheduler_id,
            });
        }
        if self.deadline != observed.deadline {
            return Err(DocumentAdvanceTokenInvariantError::DeadlineChanged {
                expected: self.deadline,
                observed: observed.deadline,
            });
        }
        if self.input != observed.input {
            return Err(DocumentAdvanceTokenInvariantError::InputChanged {
                expected: self.input,
                observed: observed.input,
            });
        }
        if self.microtasks != observed.microtasks {
            return Err(DocumentAdvanceTokenInvariantError::MicrotasksChanged {
                expected: self.microtasks,
                observed: observed.microtasks,
            });
        }
        if self.producers != observed.producers {
            return Err(DocumentAdvanceTokenInvariantError::ProducersChanged {
                expected: Box::new(self.producers),
                observed: Box::new(observed.producers),
            });
        }
        Ok(())
    }
}

struct DocumentAdvanceAuthority {
    target: PendingTargetObservation,
    state_generation: RuntimeStateGeneration,
    clock: crate::document_pending::PendingClockObservation,
    scheduler_id: TimerSchedulerId,
    deadline: TimerDeadlineSnapshot,
    input: PendingInputObservation,
    microtasks: PendingMicrotaskObservation,
    producers: PendingProducerObservation,
}

impl DocumentAdvanceAuthority {
    fn from_pending(
        pending: &RawPendingSnapshot,
    ) -> Result<Self, DocumentAdvanceTokenInvariantError> {
        pending
            .validate()
            .map_err(DocumentAdvanceTokenInvariantError::PendingSnapshot)?;
        if !pending.terminals.is_empty()
            || pending
                .execution
                .is_some_and(|execution| execution.terminal.is_some())
        {
            return Err(DocumentAdvanceTokenInvariantError::RuntimeTerminal);
        }
        if pending.clock.mode != PendingClockMode::Controlled {
            return Err(DocumentAdvanceTokenInvariantError::ClockNotControlled(
                pending.clock.mode,
            ));
        }
        if let Some(surface) = pending.clock.unsupported_surface {
            return Err(DocumentAdvanceTokenInvariantError::UnsupportedClockSurface(
                surface,
            ));
        }
        if let Some(work) = authoritative_ready_work(pending) {
            return Err(DocumentAdvanceTokenInvariantError::AuthoritativeReadyWork(
                work,
            ));
        }
        let deadline = pending
            .scheduler
            .next_deadline
            .ok_or(DocumentAdvanceTokenInvariantError::NoFiniteDeadline)?;
        if deadline.deadline < pending.clock.now {
            return Err(
                DocumentAdvanceTokenInvariantError::DeadlineBeforeCurrentTime {
                    now: pending.clock.now,
                    deadline,
                },
            );
        }
        if pending.input.intake_saturated {
            return Err(DocumentAdvanceTokenInvariantError::InputIntakeSaturated {
                revision: pending.input.revision,
            });
        }
        if pending.input.ready_events != 0 || pending.input.tasks.ready != 0 {
            return Err(DocumentAdvanceTokenInvariantError::ReadyInput(
                pending.input,
            ));
        }
        if pending.microtasks.queued != 0 || pending.microtasks.checkpoint_in_progress {
            return Err(DocumentAdvanceTokenInvariantError::MicrotasksNotDrained(
                pending.microtasks,
            ));
        }
        if pending.producers.stability != PendingProducerStability::StableEmpty {
            return Err(DocumentAdvanceTokenInvariantError::ProducerNotStableEmpty(
                Box::new(pending.producers),
            ));
        }
        Ok(Self {
            target: pending.target.clone(),
            state_generation: pending.state_generation,
            clock: pending.clock,
            scheduler_id: pending.scheduler.scheduler_id,
            deadline,
            input: pending.input,
            microtasks: pending.microtasks,
            producers: pending.producers,
        })
    }
}

fn authoritative_ready_work(pending: &RawPendingSnapshot) -> Option<DocumentAdvanceReadyWork> {
    if let Some(timer) = pending
        .logical_timers
        .timers()
        .iter()
        .copied()
        .find(|timer| timer.delivery_ready)
    {
        return Some(DocumentAdvanceReadyWork::LogicalTimer(timer));
    }
    if let Some(source) = pending.parser.sources().iter().copied().find(|source| {
        matches!(
            source.phase,
            PendingParserPhase::Ready | PendingParserPhase::AwaitingCommit
        )
    }) {
        return Some(DocumentAdvanceReadyWork::Parser(source));
    }
    if let Some(operation) = pending
        .network
        .active()
        .iter()
        .copied()
        .find(|operation| operation.phase == PendingExternalIoPhase::TerminalTaskQueued)
    {
        return Some(DocumentAdvanceReadyWork::Network(operation));
    }
    if pending.rendering.opportunity_ready {
        return Some(DocumentAdvanceReadyWork::RenderingOpportunity);
    }
    // A retained rendering-opportunity scheduler entry is not ready work, including when its
    // deadline is exactly `now`. Controlled schedulers detach/activate that entry only through an
    // authorized AdvanceTo; an ordinary turn cannot consume it. A genuinely stale (past) head is
    // still rejected below by the token's deadline invariant.
    if pending.rendering.scheduled_opportunity.is_some() {
        return None;
    }
    pending
        .rendering
        .pipelines()
        .iter()
        .copied()
        .find(|pipeline| pipeline.document_update_required)
        .map(|pipeline| DocumentAdvanceReadyWork::RenderingRequired(Box::new(pipeline)))
}

/// Policy-neutral authoritative work which must run before virtual time advances.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentAdvanceReadyWork {
    /// A detached outer wake has already made one logical DOM-timer delivery ready.
    LogicalTimer(PendingLogicalTimerObservation),
    /// A parser or top-level navigation is runnable or awaiting its commit turn.
    Parser(PendingParserSourceObservation),
    /// A network operation has already queued its terminal event-loop delivery.
    Network(PendingExternalIoObservation),
    /// The rendering-opportunity flag is ready for an ordinary turn.
    RenderingOpportunity,
    /// A document's exact rendering predicate requires an immediate update.
    RenderingRequired(Box<PendingPipelineRenderingObservation>),
}

/// A contradiction or stale fact in a private advance precondition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentAdvanceTokenInvariantError {
    /// The raw observation was structurally inconsistent.
    PendingSnapshot(PendingSnapshotInvariantError),
    /// A sticky owner terminal makes guarded mutation unsafe.
    RuntimeTerminal,
    /// Conditional advance requires a controlled clock.
    ClockNotControlled(PendingClockMode),
    /// A time-observing surface escaped the controlled clock.
    UnsupportedClockSurface(DocumentTimeSurface),
    /// Authoritative ready work must run before any future timer is activated.
    AuthoritativeReadyWork(DocumentAdvanceReadyWork),
    /// No finite scheduler head existed to bind.
    NoFiniteDeadline,
    /// A scheduler head behind current document time is stale. An exact-now head remains eligible:
    /// its zero-delta guarded advance detaches exactly one same-timestamp callback.
    DeadlineBeforeCurrentTime {
        /// Current document time.
        now: DocumentTime,
        /// Scheduler head which is behind current document time.
        deadline: TimerDeadlineSnapshot,
    },
    /// Bounded ordinary-input intake may have left unseen channel input.
    InputIntakeSaturated {
        /// Input revision at the saturated boundary.
        revision: crate::document_pending::PendingInputRevision,
    },
    /// Ready ordinary input must run before virtual time advances.
    ReadyInput(PendingInputObservation),
    /// Queued microtasks or an active checkpoint must complete first.
    MicrotasksNotDrained(PendingMicrotaskObservation),
    /// Two fresh checkpoints have not proven the exact producer revision empty.
    ProducerNotStableEmpty(Box<PendingProducerObservation>),
    /// Immutable WebView, navigation, event-loop, or pipeline authority changed.
    TargetChanged {
        /// Authority embedded in the token.
        expected: Box<PendingTargetObservation>,
        /// Authority observed at the guarded action.
        observed: Box<PendingTargetObservation>,
    },
    /// Complete normalized runtime state changed.
    StateGenerationChanged {
        /// Generation embedded in the token.
        expected: RuntimeStateGeneration,
        /// Generation observed at the guarded action.
        observed: RuntimeStateGeneration,
    },
    /// Clock identity or offset changed.
    ClockChanged {
        /// Clock identity embedded in the token.
        expected_id: DocumentClockId,
        /// Clock identity observed at the guarded action.
        observed_id: DocumentClockId,
        /// Clock offset embedded in the token.
        expected_now: DocumentTime,
        /// Clock offset observed at the guarded action.
        observed_now: DocumentTime,
    },
    /// Outer-scheduler ownership changed.
    SchedulerChanged {
        /// Scheduler embedded in the token.
        expected: TimerSchedulerId,
        /// Scheduler observed at the guarded action.
        observed: TimerSchedulerId,
    },
    /// The exact scheduler head changed.
    DeadlineChanged {
        /// Deadline embedded in the token.
        expected: TimerDeadlineSnapshot,
        /// Deadline observed at the guarded action.
        observed: TimerDeadlineSnapshot,
    },
    /// Ordinary-input revision, saturation, or queue counts changed.
    InputChanged {
        /// Input evidence embedded in the token.
        expected: PendingInputObservation,
        /// Input evidence observed at the guarded action.
        observed: PendingInputObservation,
    },
    /// Microtask queue or completed-checkpoint proof changed.
    MicrotasksChanged {
        /// Microtask evidence embedded in the token.
        expected: PendingMicrotaskObservation,
        /// Microtask evidence observed at the guarded action.
        observed: PendingMicrotaskObservation,
    },
    /// Producer-fence identity, watermarks, or two-checkpoint proof changed.
    ProducersChanged {
        /// Producer evidence embedded in the token.
        expected: Box<PendingProducerObservation>,
        /// Producer evidence observed at the guarded action.
        observed: Box<PendingProducerObservation>,
    },
}

/// One mechanical operation. None of these commands applies settlement policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentControlCommand {
    /// Observe authoritative pending state without running page work or advancing the clock.
    ///
    /// Observation invalidates any earlier token before collecting replacement authority. A lost
    /// response may therefore hide a replacement token and never makes the prior token reusable.
    Observe,
    /// Process exactly one ordinary event-loop turn and its normal checkpoint/rendering tail.
    ///
    /// Control messages do not count as the ordinary turn. Accepting this command invalidates any
    /// retained advance token before page work begins.
    DriveOneTurn,
    /// Admit the one initial async fetch-backed root pipeline identified by a preceding passive
    /// Observe readiness rejection.
    BootstrapInitialPipeline {
        /// Exact pending root pipeline which must still be the strictly qualified front event.
        pipeline_id: PipelineId,
    },
    /// Execute one bounded native operation against the exact target and state generation carried
    /// by the request.
    ///
    /// Read-only operations can fail definitively after execution. `Fill` and `Activate` can run
    /// page handlers, so a lost response or post-action observation is explicitly indeterminate.
    Automate(Box<DocumentAutomationRequest>),
    /// Conditionally advance to and activate the exact deadline bound by a fresh token.
    AdvanceTo(Box<DocumentAdvanceToken>),
}

/// Return whether `after` is the one exact Constellation transition authorized while admitting
/// the first async fetch-backed root document.
///
/// The pipeline remains the sole event-loop member. Only top-level navigation authority changes:
/// the pending row is removed and the same pipeline becomes active and fully active. The two
/// owner mutations each advance the checked navigation revision once, preventing an
/// identical-looking remove/reinsert sequence from being accepted as this handoff.
#[doc(hidden)]
pub fn is_exact_initial_pipeline_activation_transition(
    before: &PendingTargetObservation,
    after: &PendingTargetObservation,
    pipeline_id: PipelineId,
) -> bool {
    before.active_top_level.is_none()
        && before.pipelines() == [pipeline_id]
        && before.fully_active_pipelines().is_empty()
        && before.pending_top_level_pipelines() == [pipeline_id]
        && after.webview_id == before.webview_id
        && after.event_loop_id == before.event_loop_id
        && after
            .active_top_level
            .is_some_and(|active| active.pipeline_id == pipeline_id)
        && after.pipelines() == [pipeline_id]
        && after.fully_active_pipelines() == [pipeline_id]
        && after.pending_top_level_pipelines().is_empty()
        && after.pipeline_membership_revision == before.pipeline_membership_revision
        && after.unsupported_time_surface == before.unsupported_time_surface
        && before
            .navigation_revision
            .checked_next()
            .and_then(PendingNavigationRevision::checked_next)
            == Some(after.navigation_revision)
}

/// Mechanical action completed immediately before an authoritative observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentControlAction {
    /// No page work was requested.
    Observed,
    /// No page event was queued, so one no-op checkpoint turn ran.
    CheckpointTurnProcessed {
        /// Whether the turn completed a new microtask checkpoint.
        microtask_checkpoint_advanced: bool,
    },
    /// One ordinary event was processed through the event-loop path.
    TurnProcessed {
        /// Whether the turn completed a new microtask checkpoint.
        microtask_checkpoint_advanced: bool,
    },
    /// A sticky controlled-execution failure prevented the requested ordinary turn from starting.
    ExecutionTerminated,
    /// One bounded native automation operation completed before this observation.
    Automated(DocumentControlAutomationKind),
    /// Exactly one timer callback was activated; resulting page work has not run yet.
    TimerActivated(TimerDeadlineSnapshot),
}

/// Stable operation class used to bind automation requests, results, and indeterminate outcomes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentControlAutomationKind {
    QueryCount,
    TextContent,
    InnerHtml,
    Extract,
    Fill,
    Activate,
}

impl DocumentControlAutomationKind {
    /// Return the operation class carried by a target-bound request.
    pub const fn from_request(request: &DocumentAutomationRequest) -> Self {
        Self::from_operation(request.operation())
    }

    /// Return the operation class carried by a native automation operation.
    pub const fn from_operation(operation: &DocumentAutomationOperation) -> Self {
        match operation {
            DocumentAutomationOperation::QueryCount { .. } => Self::QueryCount,
            DocumentAutomationOperation::TextContent { .. } => Self::TextContent,
            DocumentAutomationOperation::InnerHtml { .. } => Self::InnerHtml,
            DocumentAutomationOperation::Extract(_) => Self::Extract,
            DocumentAutomationOperation::Fill { .. } => Self::Fill,
            DocumentAutomationOperation::Activate { .. } => Self::Activate,
        }
    }

    /// Whether execution may synchronously mutate the document or invoke page handlers.
    pub const fn is_mutating(self) -> bool {
        matches!(self, Self::Fill | Self::Activate)
    }

    fn matches_result(self, result: &DocumentAutomationResult) -> bool {
        matches!(
            (self, result),
            (
                Self::QueryCount,
                DocumentAutomationResult::QueryCount { .. }
            ) | (
                Self::TextContent,
                DocumentAutomationResult::TextContent { .. }
            ) | (Self::InnerHtml, DocumentAutomationResult::InnerHtml { .. })
                | (Self::Extract, DocumentAutomationResult::Extract { .. })
                | (Self::Fill, DocumentAutomationResult::Filled)
                | (Self::Activate, DocumentAutomationResult::Activated)
        )
    }
}

/// Authoritative post-command state from one controlled event loop.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DocumentControlObservation {
    action: DocumentControlAction,
    pending: Box<RawPendingSnapshot>,
    advance_token: Option<DocumentAdvanceToken>,
}

impl DocumentControlObservation {
    /// Construct and validate one same-build observation envelope.
    #[doc(hidden)]
    pub fn new_internal(
        action: DocumentControlAction,
        pending: Box<RawPendingSnapshot>,
        advance_token: Option<DocumentAdvanceToken>,
    ) -> Result<Self, DocumentControlObservationInvariantError> {
        let observation = Self {
            action,
            pending,
            advance_token,
        };
        observation.validate()?;
        Ok(observation)
    }

    /// Return the operation completed immediately before this observation.
    pub const fn action(&self) -> DocumentControlAction {
        self.action
    }

    /// Return the complete authoritative pending-state observation.
    pub fn pending(&self) -> &RawPendingSnapshot {
        &self.pending
    }

    /// Return the optional private precondition for the observed scheduler head.
    pub fn advance_token(&self) -> Option<&DocumentAdvanceToken> {
        self.advance_token.as_ref()
    }

    /// Revalidate raw evidence and any issued token after same-build deserialization.
    pub fn validate(&self) -> Result<(), DocumentControlObservationInvariantError> {
        self.pending
            .validate()
            .map_err(DocumentControlObservationInvariantError::PendingSnapshot)?;
        if self.action == DocumentControlAction::ExecutionTerminated
            && !self
                .pending
                .execution
                .is_some_and(|execution| execution.terminal.is_some())
        {
            return Err(DocumentControlObservationInvariantError::ExecutionTerminationMissing);
        }
        if let Some(token) = &self.advance_token {
            token
                .validate_against(&self.pending)
                .map_err(DocumentControlObservationInvariantError::AdvanceToken)?;
        }
        Ok(())
    }
}

/// A structural inconsistency in a document-control observation envelope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentControlObservationInvariantError {
    /// The contained pending-state observation was invalid.
    PendingSnapshot(PendingSnapshotInvariantError),
    /// An execution-terminated action lacked the sticky terminal which prevented admission.
    ExecutionTerminationMissing,
    /// The contained token did not bind the contained pending observation exactly.
    AdvanceToken(DocumentAdvanceTokenInvariantError),
}

/// Definitive or explicitly indeterminate completion of one control command.
///
/// A [`Self::Rejected`] response proves the requested page-work or guarded-clock mutation did not
/// begin. Once a drive or guarded advance may have crossed its linearization point, handlers must
/// return the corresponding indeterminate variant instead.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentControlOutcome {
    /// The command completed and returned an authoritative observation.
    Completed(Box<DocumentControlObservation>),
    /// A bounded native automation operation completed and returned authoritative post-action
    /// state. The observation action names the same operation class as `result`.
    AutomationCompleted {
        result: DocumentAutomationResult,
        observation: Box<DocumentControlObservation>,
    },
    /// The command was definitively rejected before its page-state mutation.
    Rejected(DocumentControlError),
    /// The runtime cannot determine whether one ordinary turn completed.
    DriveOneTurnOutcomeIndeterminate {
        /// Exact target bound when the command was routed.
        target: Box<PendingTargetObservation>,
    },
    /// The runtime cannot determine whether this guarded advance committed.
    AdvanceOutcomeIndeterminate {
        /// Single-use token whose outcome cannot be recovered.
        token_id: DocumentAdvanceTokenId,
        /// Exact target bound when the token was routed.
        target: Box<PendingTargetObservation>,
        /// Exact scheduler entry which may or may not have been activated.
        deadline: TimerDeadlineSnapshot,
    },
    /// The runtime cannot determine whether a mutating native automation operation completed.
    AutomationOutcomeIndeterminate {
        /// Exact target bound when the command was routed.
        target: Box<PendingTargetObservation>,
        /// Mutating operation which may or may not have committed.
        operation: DocumentControlAutomationKind,
    },
}

impl DocumentControlOutcome {
    /// Revalidate an observation, rejection, or indeterminate target after deserialization.
    pub fn validate(&self) -> Result<(), DocumentControlOutcomeInvariantError> {
        match self {
            Self::Completed(observation) => observation
                .validate()
                .map_err(DocumentControlOutcomeInvariantError::Observation),
            Self::AutomationCompleted {
                result,
                observation,
            } => {
                observation
                    .validate()
                    .map_err(DocumentControlOutcomeInvariantError::Observation)?;
                let DocumentControlAction::Automated(operation) = observation.action else {
                    return Err(
                        DocumentControlOutcomeInvariantError::CompletedActionMismatch {
                            command: DocumentControlCommandKind::Automate,
                            observed: observation.action,
                        },
                    );
                };
                if !operation.matches_result(result) {
                    return Err(
                        DocumentControlOutcomeInvariantError::AutomationResultMismatch {
                            expected: operation,
                        },
                    );
                }
                Ok(())
            },
            Self::Rejected(error) => error
                .validate()
                .map_err(DocumentControlOutcomeInvariantError::Rejection),
            Self::DriveOneTurnOutcomeIndeterminate { target }
            | Self::AdvanceOutcomeIndeterminate { target, .. } => target
                .validate()
                .map_err(DocumentControlOutcomeInvariantError::IndeterminateTarget),
            Self::AutomationOutcomeIndeterminate { target, operation } => {
                target
                    .validate()
                    .map_err(DocumentControlOutcomeInvariantError::IndeterminateTarget)?;
                if !operation.is_mutating() {
                    return Err(
                        DocumentControlOutcomeInvariantError::ReadOnlyAutomationIndeterminate {
                            operation: *operation,
                        },
                    );
                }
                Ok(())
            },
        }
    }

    /// Validate that this decoded outcome belongs to the submitted command.
    pub fn validate_for_command(
        &self,
        command: &DocumentControlCommand,
    ) -> Result<(), DocumentControlOutcomeInvariantError> {
        self.validate()?;
        match (command, self) {
            (
                command,
                Self::Rejected(DocumentControlError::InitialPipelineBootstrapRequired {
                    pipeline_id,
                }),
            ) if !matches!(command, DocumentControlCommand::Observe) => Err(
                DocumentControlOutcomeInvariantError::InitialPipelineBootstrapRejectionForCommand {
                    command: DocumentControlCommandKind::from_command(command),
                    pipeline_id: *pipeline_id,
                },
            ),
            (
                DocumentControlCommand::BootstrapInitialPipeline {
                    pipeline_id: expected,
                },
                Self::Rejected(DocumentControlError::InitialPipelineBootstrapUnavailable {
                    pipeline_id: observed,
                }),
            ) if expected == observed => Ok(()),
            (
                command,
                Self::Rejected(DocumentControlError::InitialPipelineBootstrapUnavailable {
                    pipeline_id: observed,
                }),
            ) => Err(
                DocumentControlOutcomeInvariantError::InitialPipelineBootstrapUnavailableForCommand {
                    command: DocumentControlCommandKind::from_command(command),
                    expected: match command {
                        DocumentControlCommand::BootstrapInitialPipeline { pipeline_id } => {
                            Some(*pipeline_id)
                        },
                        _ => None,
                    },
                    observed: *observed,
                },
            ),
            (DocumentControlCommand::DriveOneTurn, Self::Rejected(error))
                if error.is_pending_capture_failure() =>
            {
                Err(
                    DocumentControlOutcomeInvariantError::PendingCaptureRejectionForMutatingCommand {
                        command: DocumentControlCommandKind::DriveOneTurn,
                        error: Box::new(error.clone()),
                    },
                )
            },
            (DocumentControlCommand::BootstrapInitialPipeline { .. }, Self::Rejected(error))
                if error.is_pending_capture_failure() =>
            {
                Err(
                    DocumentControlOutcomeInvariantError::PendingCaptureRejectionForMutatingCommand {
                        command: DocumentControlCommandKind::BootstrapInitialPipeline,
                        error: Box::new(error.clone()),
                    },
                )
            },
            (DocumentControlCommand::AdvanceTo(_), Self::Rejected(error))
                if error.is_pending_capture_failure() =>
            {
                Err(
                    DocumentControlOutcomeInvariantError::PendingCaptureRejectionForMutatingCommand {
                        command: DocumentControlCommandKind::AdvanceTo,
                        error: Box::new(error.clone()),
                    },
                )
            },
            (DocumentControlCommand::Automate(request), Self::Rejected(error))
                if DocumentControlAutomationKind::from_request(request).is_mutating()
                    && error.is_pending_capture_failure() =>
            {
                Err(
                    DocumentControlOutcomeInvariantError::PendingCaptureRejectionForMutatingCommand {
                        command: DocumentControlCommandKind::Automate,
                        error: Box::new(error.clone()),
                    },
                )
            },
            (DocumentControlCommand::Automate(request), Self::Rejected(error))
                if automation_rejection_may_follow_mutation(request, error) =>
            {
                Err(
                    DocumentControlOutcomeInvariantError::AutomationMutationRejection {
                        operation: DocumentControlAutomationKind::from_request(request),
                        error: Box::new(error.clone()),
                    },
                )
            },
            (_, Self::Rejected(_)) => Ok(()),
            (DocumentControlCommand::Observe, Self::Completed(observation)) => {
                if observation.action == DocumentControlAction::Observed {
                    Ok(())
                } else {
                    Err(
                        DocumentControlOutcomeInvariantError::CompletedActionMismatch {
                            command: DocumentControlCommandKind::Observe,
                            observed: observation.action,
                        },
                    )
                }
            },
            (DocumentControlCommand::DriveOneTurn, Self::Completed(observation)) => {
                if matches!(
                    observation.action,
                    DocumentControlAction::CheckpointTurnProcessed { .. }
                        | DocumentControlAction::TurnProcessed { .. }
                        | DocumentControlAction::ExecutionTerminated
                ) {
                    Ok(())
                } else {
                    Err(
                        DocumentControlOutcomeInvariantError::CompletedActionMismatch {
                            command: DocumentControlCommandKind::DriveOneTurn,
                            observed: observation.action,
                        },
                    )
                }
            },
            (
                DocumentControlCommand::BootstrapInitialPipeline { .. },
                Self::Completed(observation),
            ) => {
                if matches!(
                    observation.action,
                    DocumentControlAction::TurnProcessed { .. }
                        | DocumentControlAction::ExecutionTerminated
                ) {
                    Ok(())
                } else {
                    Err(
                        DocumentControlOutcomeInvariantError::CompletedActionMismatch {
                            command: DocumentControlCommandKind::BootstrapInitialPipeline,
                            observed: observation.action,
                        },
                    )
                }
            },
            (
                DocumentControlCommand::Automate(request),
                Self::AutomationCompleted {
                    result,
                    observation,
                },
            ) => {
                let expected_kind = DocumentControlAutomationKind::from_request(request);
                let DocumentControlAction::Automated(observed_kind) = observation.action else {
                    return Err(
                        DocumentControlOutcomeInvariantError::CompletedActionMismatch {
                            command: DocumentControlCommandKind::Automate,
                            observed: observation.action,
                        },
                    );
                };
                if observed_kind != expected_kind {
                    return Err(
                        DocumentControlOutcomeInvariantError::AutomationOperationMismatch {
                            expected: expected_kind,
                            observed: observed_kind,
                        },
                    );
                }
                if !expected_kind.matches_result(result) {
                    return Err(DocumentControlOutcomeInvariantError::AutomationResultMismatch {
                        expected: expected_kind,
                    });
                }
                if &observation.pending.target != request.target() {
                    return Err(DocumentControlOutcomeInvariantError::AutomationTargetMismatch {
                        expected: Box::new(request.target().clone()),
                        observed: Box::new(observation.pending.target.clone()),
                    });
                }
                if observation.pending.state_generation < request.expected_generation() {
                    return Err(
                        DocumentControlOutcomeInvariantError::AutomationStateGenerationRegressed {
                            expected_at_least: request.expected_generation(),
                            observed: observation.pending.state_generation,
                        },
                    );
                }
                Ok(())
            },
            (DocumentControlCommand::AdvanceTo(token), Self::Completed(observation)) => {
                let DocumentControlAction::TimerActivated(observed_deadline) = observation.action
                else {
                    return Err(
                        DocumentControlOutcomeInvariantError::CompletedActionMismatch {
                            command: DocumentControlCommandKind::AdvanceTo,
                            observed: observation.action,
                        },
                    );
                };
                validate_advance_target(token, &observation.pending.target)?;
                if observation.pending.clock.clock_id != token.clock_id() {
                    return Err(
                        DocumentControlOutcomeInvariantError::AdvanceCompletedClockIdentityMismatch {
                            expected: token.clock_id(),
                            observed: observation.pending.clock.clock_id,
                        },
                    );
                }
                if observation.pending.scheduler.scheduler_id != token.scheduler_id() {
                    return Err(
                        DocumentControlOutcomeInvariantError::AdvanceCompletedSchedulerIdentityMismatch {
                            expected: token.scheduler_id(),
                            observed: observation.pending.scheduler.scheduler_id,
                        },
                    );
                }
                validate_advance_deadline(token, observed_deadline)?;
                if observation.pending.clock.now != token.deadline().deadline {
                    return Err(
                        DocumentControlOutcomeInvariantError::AdvanceCompletedClockMismatch {
                            expected: token.deadline().deadline,
                            observed: observation.pending.clock.now,
                        },
                    );
                }
                // Clock movement and removal or rescheduling of the activated head can each
                // normalize a distinct field. Requiring one exact successor would reject a valid
                // compound action, but no completed advance may retain its pre-action generation.
                if observation.pending.state_generation <= token.state_generation() {
                    return Err(
                        DocumentControlOutcomeInvariantError::AdvanceCompletedStateGenerationNotAdvanced {
                            expected_greater_than: token.state_generation(),
                            observed: observation.pending.state_generation,
                        },
                    );
                }
                if observation.pending.scheduler.next_deadline == Some(token.deadline()) {
                    return Err(
                        DocumentControlOutcomeInvariantError::AdvanceCompletedHeadNotConsumed {
                            deadline: token.deadline(),
                        },
                    );
                }
                Ok(())
            },
            (
                DocumentControlCommand::DriveOneTurn
                | DocumentControlCommand::BootstrapInitialPipeline { .. },
                Self::DriveOneTurnOutcomeIndeterminate { .. },
            ) => Ok(()),
            (
                DocumentControlCommand::AdvanceTo(token),
                Self::AdvanceOutcomeIndeterminate {
                    token_id,
                    target,
                    deadline,
                },
            ) => {
                if *token_id != token.id() {
                    return Err(
                        DocumentControlOutcomeInvariantError::AdvanceTokenIdentityMismatch {
                            expected: token.id(),
                            observed: *token_id,
                        },
                    );
                }
                validate_advance_target(token, target)?;
                validate_advance_deadline(token, *deadline)
            },
            (
                DocumentControlCommand::Automate(request),
                Self::AutomationOutcomeIndeterminate { target, operation },
            ) => {
                let expected = DocumentControlAutomationKind::from_request(request);
                if !expected.is_mutating() {
                    return Err(
                        DocumentControlOutcomeInvariantError::ReadOnlyAutomationIndeterminate {
                            operation: expected,
                        },
                    );
                }
                if *operation != expected {
                    return Err(
                        DocumentControlOutcomeInvariantError::AutomationOperationMismatch {
                            expected,
                            observed: *operation,
                        },
                    );
                }
                if &**target != request.target() {
                    return Err(DocumentControlOutcomeInvariantError::AutomationTargetMismatch {
                        expected: Box::new(request.target().clone()),
                        observed: target.clone(),
                    });
                }
                Ok(())
            },
            _ => Err(
                DocumentControlOutcomeInvariantError::OutcomeCommandMismatch {
                    command: DocumentControlCommandKind::from_command(command),
                    outcome: DocumentControlOutcomeKind::from_outcome(self),
                },
            ),
        }
    }
}

fn automation_rejection_may_follow_mutation(
    request: &DocumentAutomationRequest,
    error: &DocumentControlError,
) -> bool {
    matches!(
        (request.operation(), error),
        (
            DocumentAutomationOperation::Fill { .. },
            DocumentControlError::Automation(DocumentAutomationError::DomOperationFailed {
                operation: DocumentAutomationOperationKind::Fill,
            })
        )
    )
}

fn validate_advance_target(
    token: &DocumentAdvanceToken,
    observed: &PendingTargetObservation,
) -> Result<(), DocumentControlOutcomeInvariantError> {
    if token.target() == observed {
        return Ok(());
    }
    Err(
        DocumentControlOutcomeInvariantError::AdvanceTargetMismatch {
            expected: Box::new(token.target().clone()),
            observed: Box::new(observed.clone()),
        },
    )
}

fn validate_advance_deadline(
    token: &DocumentAdvanceToken,
    observed: TimerDeadlineSnapshot,
) -> Result<(), DocumentControlOutcomeInvariantError> {
    if token.deadline() == observed {
        return Ok(());
    }
    Err(
        DocumentControlOutcomeInvariantError::AdvanceDeadlineMismatch {
            expected: token.deadline(),
            observed,
        },
    )
}

/// Submitted command class used by command-aware outcome validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentControlCommandKind {
    /// Observe without page work.
    Observe,
    /// Drive one ordinary turn.
    DriveOneTurn,
    /// Admit the strictly qualified initial root pipeline.
    BootstrapInitialPipeline,
    /// Execute one bounded native automation operation.
    Automate,
    /// Activate one token-bound timer.
    AdvanceTo,
}

impl DocumentControlCommandKind {
    fn from_command(command: &DocumentControlCommand) -> Self {
        match command {
            DocumentControlCommand::Observe => Self::Observe,
            DocumentControlCommand::DriveOneTurn => Self::DriveOneTurn,
            DocumentControlCommand::BootstrapInitialPipeline { .. } => {
                Self::BootstrapInitialPipeline
            },
            DocumentControlCommand::Automate(_) => Self::Automate,
            DocumentControlCommand::AdvanceTo(_) => Self::AdvanceTo,
        }
    }
}

/// Decoded outcome class used by command-aware outcome validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentControlOutcomeKind {
    /// A completed observation.
    Completed,
    /// A completed native automation operation.
    AutomationCompleted,
    /// A definitive rejection.
    Rejected,
    /// An indeterminate driven turn.
    DriveOneTurnOutcomeIndeterminate,
    /// An indeterminate guarded advance.
    AdvanceOutcomeIndeterminate,
    /// An indeterminate mutating automation operation.
    AutomationOutcomeIndeterminate,
}

impl DocumentControlOutcomeKind {
    fn from_outcome(outcome: &DocumentControlOutcome) -> Self {
        match outcome {
            DocumentControlOutcome::Completed(_) => Self::Completed,
            DocumentControlOutcome::AutomationCompleted { .. } => Self::AutomationCompleted,
            DocumentControlOutcome::Rejected(_) => Self::Rejected,
            DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate { .. } => {
                Self::DriveOneTurnOutcomeIndeterminate
            },
            DocumentControlOutcome::AdvanceOutcomeIndeterminate { .. } => {
                Self::AdvanceOutcomeIndeterminate
            },
            DocumentControlOutcome::AutomationOutcomeIndeterminate { .. } => {
                Self::AutomationOutcomeIndeterminate
            },
        }
    }
}

/// A decoded command outcome which is structurally invalid or belongs to another command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocumentControlOutcomeInvariantError {
    /// A completed observation was structurally invalid.
    Observation(DocumentControlObservationInvariantError),
    /// A definitive rejection carried a structurally invalid error payload.
    Rejection(DocumentControlErrorInvariantError),
    /// An indeterminate outcome carried a malformed target authority.
    IndeterminateTarget(PendingSnapshotInvariantError),
    /// A pending-state capture failure cannot prove a mutating command never acted.
    PendingCaptureRejectionForMutatingCommand {
        /// Submitted mutating command class.
        command: DocumentControlCommandKind,
        /// Capture error which cannot be treated as definitive for that command.
        error: Box<DocumentControlError>,
    },
    /// Initial pipeline bootstrap is a passive readiness rejection emitted only for Observe.
    InitialPipelineBootstrapRejectionForCommand {
        /// Command which incorrectly carried the readiness rejection.
        command: DocumentControlCommandKind,
        /// Root pipeline awaiting its one explicit bootstrap turn.
        pipeline_id: PipelineId,
    },
    /// A bootstrap-unavailable rejection belonged to another command or pipeline.
    InitialPipelineBootstrapUnavailableForCommand {
        /// Submitted command class.
        command: DocumentControlCommandKind,
        /// Pipeline bound by a bootstrap command, or `None` for another command class.
        expected: Option<PipelineId>,
        /// Pipeline carried by the rejection.
        observed: PipelineId,
    },
    /// A definitive rejection named a native failure which can occur after a mutation began.
    AutomationMutationRejection {
        /// Mutating operation whose outcome can no longer be proven definitive.
        operation: DocumentControlAutomationKind,
        /// Rejection which crossed the mutation boundary.
        error: Box<DocumentControlError>,
    },
    /// A completed action did not match the submitted command.
    CompletedActionMismatch {
        /// Submitted command class.
        command: DocumentControlCommandKind,
        /// Action carried by the decoded observation.
        observed: DocumentControlAction,
    },
    /// An automation completion or indeterminate outcome named another operation class.
    AutomationOperationMismatch {
        expected: DocumentControlAutomationKind,
        observed: DocumentControlAutomationKind,
    },
    /// An automation completion carried a result belonging to another operation class.
    AutomationResultMismatch {
        expected: DocumentControlAutomationKind,
    },
    /// An automation completion or indeterminate outcome named another target.
    AutomationTargetMismatch {
        expected: Box<PendingTargetObservation>,
        observed: Box<PendingTargetObservation>,
    },
    /// Authoritative post-action state regressed behind the request precondition.
    AutomationStateGenerationRegressed {
        expected_at_least: RuntimeStateGeneration,
        observed: RuntimeStateGeneration,
    },
    /// A read-only native automation operation cannot have an indeterminate page mutation.
    ReadOnlyAutomationIndeterminate {
        operation: DocumentControlAutomationKind,
    },
    /// An indeterminate outcome belonged to a different command class.
    OutcomeCommandMismatch {
        /// Submitted command class.
        command: DocumentControlCommandKind,
        /// Decoded outcome class.
        outcome: DocumentControlOutcomeKind,
    },
    /// An indeterminate advance named a different single-use token.
    AdvanceTokenIdentityMismatch {
        /// Submitted token identity.
        expected: DocumentAdvanceTokenId,
        /// Decoded token identity.
        observed: DocumentAdvanceTokenId,
    },
    /// A completed or indeterminate advance named a different target authority.
    AdvanceTargetMismatch {
        /// Target embedded in the submitted token.
        expected: Box<PendingTargetObservation>,
        /// Target carried by the decoded outcome.
        observed: Box<PendingTargetObservation>,
    },
    /// A completed or indeterminate advance named a different scheduler entry.
    AdvanceDeadlineMismatch {
        /// Deadline embedded in the submitted token.
        expected: TimerDeadlineSnapshot,
        /// Deadline carried by the decoded outcome.
        observed: TimerDeadlineSnapshot,
    },
    /// A completed advance did not observe the activated deadline as current document time.
    AdvanceCompletedClockMismatch {
        /// Activated deadline.
        expected: DocumentTime,
        /// Post-action document time.
        observed: DocumentTime,
    },
    /// A completed advance observed a different controlled clock domain.
    AdvanceCompletedClockIdentityMismatch {
        /// Clock domain embedded in the submitted token.
        expected: DocumentClockId,
        /// Clock domain carried by the post-action observation.
        observed: DocumentClockId,
    },
    /// A completed advance observed a different outer scheduler.
    AdvanceCompletedSchedulerIdentityMismatch {
        /// Scheduler embedded in the submitted token.
        expected: TimerSchedulerId,
        /// Scheduler carried by the post-action observation.
        observed: TimerSchedulerId,
    },
    /// A completed advance retained or regressed its pre-action state generation.
    AdvanceCompletedStateGenerationNotAdvanced {
        /// Pre-action generation embedded in the token.
        expected_greater_than: RuntimeStateGeneration,
        /// Post-action generation carried by the observation.
        observed: RuntimeStateGeneration,
    },
    /// The exact scheduler head reported activated remained the post-action head.
    AdvanceCompletedHeadNotConsumed {
        /// Exact pre-action scheduler entry which should have been removed or rescheduled.
        deadline: TimerDeadlineSnapshot,
    },
}

/// Authoritative fact required to assemble one normalized pending-state observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub enum DocumentPendingFact {
    /// Exact WebView, event-loop, navigation, and pipeline-membership authority.
    TargetMembership,
    /// Monotonic generation covering observable runtime state.
    StateGeneration,
    /// Monotonic epoch covering DOM mutation.
    DomMutationEpoch,
    /// Controlled document-clock identity, mode, and current time.
    Clock,
    /// Exact finite scheduler head and scheduler identity.
    Scheduler,
    /// Ordinary input revision, readiness, and task inventory.
    Input,
    /// Main-event-loop microtask checkpoint coverage.
    MicrotaskCoverage,
    /// Asynchronous producer-fence watermarks and stability.
    Producers,
    /// Parser and navigation source inventory.
    Parser,
    /// Foreground and persistent network inventory.
    Network,
    /// Stable logical DOM timers and their exact coalesced outer-wake bindings.
    LogicalTimers,
    /// Rendering readiness and persistent rendering activity.
    Rendering,
    /// Canonical asynchronous-source inventory.
    Sources,
    /// Sticky terminal evidence for unsupported or exhausted runtime state.
    RuntimeTerminals,
}

/// Typed definitive rejection from routing, observation, turn driving, or guarded advance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentControlError {
    /// The WebView no longer exists.
    WebViewUnavailable,
    /// The WebView is not running with a controlled clock.
    NotControlled,
    /// No ScriptEventLoop is currently bound to the WebView.
    EventLoopUnavailable,
    /// The WebView spans more than one ScriptEventLoop.
    MultipleEventLoops,
    /// The selected ScriptEventLoop also owns a different WebView.
    SharedEventLoopWebView,
    /// The exact initial fetch-backed root SpawnPipeline is ready for one explicit bootstrap turn.
    InitialPipelineBootstrapRequired {
        /// Pending root pipeline which the bootstrap turn must admit.
        pipeline_id: PipelineId,
    },
    /// The requested initial root bootstrap is no longer the exact qualified front event.
    InitialPipelineBootstrapUnavailable {
        /// Pipeline named by the rejected one-shot bootstrap command.
        pipeline_id: PipelineId,
    },
    /// The authoritative owner cannot currently supply a required pending-state fact.
    PendingFactUnavailable(DocumentPendingFact),
    /// Authoritative facts could not be normalized into one internally consistent snapshot.
    PendingSnapshot(PendingSnapshotInvariantError),
    /// Target authority changed while the owner was capturing a definitive pre-action snapshot.
    TargetChanged {
        /// Authority established when the command was admitted.
        expected: Box<PendingTargetObservation>,
        /// Live authority observed before any requested page-state mutation began.
        observed: Box<PendingTargetObservation>,
    },
    /// Another control command is already in flight for this WebView.
    CommandAlreadyPending,
    /// The checked Constellation request sequence was exhausted.
    RequestSequenceOverflow,
    /// The checked ScriptThread token sequence was exhausted.
    AdvanceTokenSequenceOverflow,
    /// The checked ordinary-input revision was exhausted.
    InputRevisionOverflow,
    /// No issued token remained for the attempted advance, including token reuse.
    AdvanceTokenUnavailable {
        /// Token supplied by the caller.
        observed: DocumentAdvanceTokenId,
    },
    /// The supplied token was not the exact latest token retained by ScriptThread.
    StaleAdvanceToken {
        /// Latest retained token consumed by the failed attempt.
        expected: DocumentAdvanceTokenId,
        /// Token supplied by the caller.
        observed: DocumentAdvanceTokenId,
    },
    /// Fresh authoritative state rejected the token before clock or timer mutation.
    AdvancePrecondition(DocumentAdvanceTokenInvariantError),
    /// A controlled-time surface escaped this runtime slice.
    UnsupportedSurface(DocumentTimeSurface),
    /// A bounded native automation request was definitively rejected before an uncertain
    /// mutation, or a read-only operation failed without mutating the page.
    Automation(DocumentAutomationError),
    /// A checked document-clock operation failed.
    Clock(DocumentClockError),
    /// An exact finite-deadline operation failed.
    Timer(TimerControlError),
    /// Producer-fence observation failed.
    ProducerFence(DocumentProducerFenceError),
    /// A queue length could not be represented in the observation.
    QueueLengthOverflow,
    /// The selected ScriptThread channel closed before definitive execution.
    ChannelClosed,
    /// The checked per-WebView cancellation sequence was exhausted.
    CancellationSequenceOverflow,
}

impl DocumentControlError {
    /// Revalidate any authoritative payload carried by this error after deserialization.
    pub fn validate(&self) -> Result<(), DocumentControlErrorInvariantError> {
        let Self::TargetChanged { expected, observed } = self else {
            return Ok(());
        };
        expected
            .validate()
            .map_err(DocumentControlErrorInvariantError::TargetChangedExpected)?;
        observed
            .validate()
            .map_err(DocumentControlErrorInvariantError::TargetChangedObserved)?;
        if expected == observed {
            return Err(DocumentControlErrorInvariantError::TargetDidNotChange {
                target: expected.clone(),
            });
        }
        Ok(())
    }

    fn is_pending_capture_failure(&self) -> bool {
        matches!(
            self,
            Self::PendingFactUnavailable(_) | Self::PendingSnapshot(_) | Self::TargetChanged { .. }
        )
    }
}

/// A structurally invalid authoritative payload in a definitive control error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocumentControlErrorInvariantError {
    /// The target authority established at admission was malformed.
    TargetChangedExpected(PendingSnapshotInvariantError),
    /// The live target authority observed during capture was malformed.
    TargetChangedObserved(PendingSnapshotInvariantError),
    /// A target-drift error carried identical admission and live authority.
    TargetDidNotChange {
        /// Authority which was incorrectly reported as having changed.
        target: Box<PendingTargetObservation>,
    },
}

/// Why the embedding caller's local wait failed to deliver a command outcome.
///
/// This type is deliberately not serialized into the same-build engine protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocumentControlTransportFailure {
    /// The bounded wait elapsed while the callback might still complete later.
    TimedOut,
    /// The callback channel closed without an outcome.
    Disconnected,
    /// The callback payload could not be decoded.
    DeserializationFailed(String),
    /// The IPC transport returned an I/O failure.
    Io {
        /// Standard I/O category retained from the transport.
        kind: std::io::ErrorKind,
        /// Transport-provided diagnostic.
        message: String,
    },
    /// The embedding caller explicitly abandoned its only receiver.
    Cancelled,
    /// The callback decoded, but its payload was invalid for the submitted command.
    InvalidOutcome(Box<DocumentControlOutcomeInvariantError>),
}

impl From<TryReceiveError> for DocumentControlTransportFailure {
    fn from(error: TryReceiveError) -> Self {
        match error {
            TryReceiveError::Empty => Self::TimedOut,
            TryReceiveError::ReceiveError(ReceiveError::Disconnected) => Self::Disconnected,
            TryReceiveError::ReceiveError(ReceiveError::DeserializationFailed(message)) => {
                Self::DeserializationFailed(message)
            },
            TryReceiveError::ReceiveError(ReceiveError::Io(error)) => Self::Io {
                kind: error.kind(),
                message: error.to_string(),
            },
        }
    }
}

/// The sole result of consuming one local document-control receiver.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocumentControlReceiveOutcome {
    /// The engine delivered a typed same-build command outcome.
    CommandOutcome(DocumentControlOutcome),
    /// Observe ran no page turn and performed no guarded clock mutation, but no snapshot arrived.
    ObserveTransportFailure(DocumentControlTransportFailure),
    /// A read-only automation command cannot have mutated the page, but no typed result arrived.
    AutomationTransportFailure(DocumentControlTransportFailure),
    /// The caller cannot know whether the requested ordinary turn completed.
    DriveOneTurnOutcomeIndeterminate(DocumentControlTransportFailure),
}

/// Result of one consuming, nonblocking receive attempt.
pub enum DocumentControlTryReceiveOutcome {
    /// No callback result is ready; the receiver remains armed.
    Pending(DocumentControlReceiver),
    /// The callback or transport produced the receiver's only terminal result.
    Complete(DocumentControlReceiveOutcome),
}

/// One bounded, single-result receiver for a submitted document-control command.
///
/// Timeout, transport failure, explicit cancellation, and drop run the exact attached
/// abandonment action at most once. A drive or advance receiver never converts a missing response
/// into a definitive rejection. Cancellation abandons only the response; it neither aborts page
/// work nor rolls back a command which crossed its linearization point.
pub struct DocumentControlReceiver {
    receiver: GenericReceiver<DocumentControlOutcome>,
    command: DocumentControlCommand,
    cancellation: Option<Box<dyn FnOnce() + Send + 'static>>,
}

impl DocumentControlReceiver {
    /// Bind command-specific transport semantics before submitting the command.
    #[doc(hidden)]
    pub fn new(
        receiver: GenericReceiver<DocumentControlOutcome>,
        command: &DocumentControlCommand,
    ) -> Self {
        Self::new_internal(receiver, command, None)
    }

    /// Bind an exact abandonment action to timeout, transport failure, cancellation, or drop.
    #[doc(hidden)]
    pub fn new_cancellable(
        receiver: GenericReceiver<DocumentControlOutcome>,
        command: &DocumentControlCommand,
        cancellation: impl FnOnce() + Send + 'static,
    ) -> Self {
        Self::new_internal(receiver, command, Some(Box::new(cancellation)))
    }

    fn new_internal(
        receiver: GenericReceiver<DocumentControlOutcome>,
        command: &DocumentControlCommand,
        cancellation: Option<Box<dyn FnOnce() + Send + 'static>>,
    ) -> Self {
        Self {
            receiver,
            command: command.clone(),
            cancellation,
        }
    }

    /// Wait at most `timeout` for the command's only terminal result.
    pub fn recv_timeout(self, timeout: Duration) -> DocumentControlReceiveOutcome {
        let result = self.receiver.try_recv_timeout(timeout);
        self.resolve(result)
    }

    /// Attempt to consume the command result without making an empty channel terminal.
    pub fn try_recv(self) -> DocumentControlTryReceiveOutcome {
        let result = self.receiver.try_recv();
        match result {
            Err(TryReceiveError::Empty) => DocumentControlTryReceiveOutcome::Pending(self),
            result => DocumentControlTryReceiveOutcome::Complete(self.resolve(result)),
        }
    }

    /// Explicitly abandon this receiver and run its correlated cancellation action.
    pub fn cancel(mut self) -> DocumentControlReceiveOutcome {
        self.run_cancellation();
        self.resolve_failure(DocumentControlTransportFailure::Cancelled)
    }

    fn resolve(
        mut self,
        result: Result<DocumentControlOutcome, TryReceiveError>,
    ) -> DocumentControlReceiveOutcome {
        match result {
            Ok(outcome) => match outcome.validate_for_command(&self.command) {
                Ok(()) => {
                    self.cancellation.take();
                    DocumentControlReceiveOutcome::CommandOutcome(outcome)
                },
                Err(error) => {
                    self.run_cancellation();
                    self.resolve_failure(DocumentControlTransportFailure::InvalidOutcome(Box::new(
                        error,
                    )))
                },
            },
            Err(error) => {
                self.run_cancellation();
                self.resolve_failure(error.into())
            },
        }
    }

    fn resolve_failure(
        &self,
        failure: DocumentControlTransportFailure,
    ) -> DocumentControlReceiveOutcome {
        match &self.command {
            DocumentControlCommand::Observe => {
                DocumentControlReceiveOutcome::ObserveTransportFailure(failure)
            },
            DocumentControlCommand::DriveOneTurn => {
                DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(failure)
            },
            DocumentControlCommand::BootstrapInitialPipeline { .. } => {
                DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(failure)
            },
            DocumentControlCommand::Automate(request) => {
                let operation = DocumentControlAutomationKind::from_request(request);
                if operation.is_mutating() {
                    let _ = failure;
                    DocumentControlReceiveOutcome::CommandOutcome(
                        DocumentControlOutcome::AutomationOutcomeIndeterminate {
                            target: Box::new(request.target().clone()),
                            operation,
                        },
                    )
                } else {
                    DocumentControlReceiveOutcome::AutomationTransportFailure(failure)
                }
            },
            DocumentControlCommand::AdvanceTo(token) => {
                let _ = failure;
                DocumentControlReceiveOutcome::CommandOutcome(
                    DocumentControlOutcome::AdvanceOutcomeIndeterminate {
                        token_id: token.id(),
                        target: Box::new(token.target().clone()),
                        deadline: token.deadline(),
                    },
                )
            },
        }
    }

    fn run_cancellation(&mut self) {
        if let Some(cancellation) = self.cancellation.take() {
            cancellation();
        }
    }
}

impl Drop for DocumentControlReceiver {
    fn drop(&mut self) {
        self.run_cancellation();
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use servo_base::Epoch;
    use servo_base::generic_channel::GenericCallback;
    use servo_base::id::{
        Index, TEST_NAMESPACE, TEST_PIPELINE_ID, TEST_SCRIPT_EVENT_LOOP_ID, TEST_WEBVIEW_ID,
    };
    use timers::{
        DocumentClock, DocumentClockConfiguration, DocumentExecutionBudget,
        DocumentExecutionCounters, DocumentExecutionLimits, DocumentExecutionObservation,
        DocumentExecutionTerminal, DocumentProducerCheckpoint, DocumentProducerFence,
        DocumentUnixTime, TimerEventRequest, TimerScheduler,
    };

    use super::*;
    use crate::document_automation::DocumentAutomationLimits;
    use crate::document_pending::{
        DomEpoch, PendingActiveTopLevelPipeline, PendingAnimatedImageObservation,
        PendingCanvasObservation, PendingExternalIoEvidence, PendingExternalIoLoadBlocking,
        PendingExternalIoOwner, PendingLogicalTimerKind, PendingLogicalTimerSnapshot,
        PendingLogicalTimerStableId, PendingMicrotaskCheckpoint, PendingNavigationRevision,
        PendingNetworkKind, PendingNetworkObservation, PendingParserObservation,
        PendingParserSourceKind, PendingPipelineMembershipRevision,
        PendingPipelineRenderingObservation, PendingRenderingObservation,
        PendingRenderingPipelineActivity, PendingRuntimeTerminals, PendingSchedulerObservation,
        PendingSourceDisposition, PendingSourceEpoch, PendingSourceId, PendingSourceKind,
        PendingSourceObservation, PendingSourceSnapshot, PendingTaskObservation,
    };

    fn assert_postcard_round_trip<T>(value: T)
    where
        T: Debug + DeserializeOwned + Eq + Serialize,
    {
        let bytes = postcard::to_stdvec(&value).unwrap();
        let decoded = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(value, decoded);
    }

    fn stable_producers(
        current_checkpoint: u64,
    ) -> (PendingMicrotaskObservation, PendingProducerObservation) {
        assert!(current_checkpoint >= 2);
        let fence = DocumentProducerFence::default();
        let microtask_checkpoint = PendingMicrotaskCheckpoint::new(current_checkpoint);
        let producer_checkpoint = (0..current_checkpoint)
            .fold(DocumentProducerCheckpoint::ZERO, |checkpoint, _| {
                checkpoint.checked_next().unwrap()
            });
        let prior_microtask_checkpoint = PendingMicrotaskCheckpoint::new(current_checkpoint - 1);
        let prior_producer_checkpoint = (1..current_checkpoint)
            .fold(DocumentProducerCheckpoint::ZERO, |checkpoint, _| {
                checkpoint.checked_next().unwrap()
            });
        let snapshot = fence.snapshot();
        let producers = PendingProducerObservation::new(
            TEST_SCRIPT_EVENT_LOOP_ID,
            microtask_checkpoint,
            producer_checkpoint,
            snapshot,
            PendingProducerStability::StableEmpty,
            Some(
                crate::document_pending::PendingProducerPriorEmptyQualification {
                    microtask_checkpoint: prior_microtask_checkpoint,
                    checkpoint: prior_producer_checkpoint,
                    snapshot_revision: snapshot.revision(),
                },
            ),
        )
        .unwrap();
        let microtasks = PendingMicrotaskObservation {
            event_loop_id: TEST_SCRIPT_EVENT_LOOP_ID,
            queued: 0,
            completed_checkpoint: microtask_checkpoint,
            checkpoint_in_progress: false,
            terminal: None,
        };
        (microtasks, producers)
    }

    fn rendering() -> PendingRenderingObservation {
        PendingRenderingObservation::new(
            None,
            false,
            vec![PendingPipelineRenderingObservation {
                pipeline_id: TEST_PIPELINE_ID,
                activity: PendingRenderingPipelineActivity::FullyActive,
                retained_animation_frame_callbacks: 0,
                runnable_animation_frame_callbacks: 0,
                document_update_required: false,
                pending_animation_events: 0,
                finite_animations: 0,
                infinite_animations: 0,
                unsupported_animations: 0,
                animated_images: PendingAnimatedImageObservation::default(),
                canvas: PendingCanvasObservation::default(),
                pending_fonts: 0,
                pending_images: 0,
            }],
        )
        .unwrap()
    }

    fn eligible_pending() -> RawPendingSnapshot {
        let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 5,
            unix_time_origin_ns: DocumentUnixTime::from_nanos(1_000_000),
        });
        let mut scheduler = TimerScheduler::with_clock(clock.clone());
        scheduler.schedule_timer(TimerEventRequest {
            callback: Box::new(|| {}),
            duration: Duration::from_nanos(20),
        });
        let deadline = scheduler.finite_deadline_snapshot().unwrap().unwrap();
        let target = PendingTargetObservation::new(
            TEST_WEBVIEW_ID,
            TEST_SCRIPT_EVENT_LOOP_ID,
            Some(PendingActiveTopLevelPipeline {
                pipeline_id: TEST_PIPELINE_ID,
                epoch: Epoch(7),
            }),
            PendingNavigationRevision::new(3),
            vec![TEST_PIPELINE_ID],
            vec![TEST_PIPELINE_ID],
            Vec::new(),
        )
        .unwrap();
        let (microtasks, producers) = stable_producers(2);
        let pending = RawPendingSnapshot {
            target,
            state_generation: RuntimeStateGeneration::new(9),
            dom_epoch: DomEpoch::new(4),
            clock: crate::document_pending::PendingClockObservation {
                clock_id: clock.id(),
                mode: PendingClockMode::Controlled,
                now: clock.now(),
                unsupported_surface: None,
            },
            scheduler: PendingSchedulerObservation {
                scheduler_id: scheduler.id(),
                next_deadline: Some(deadline),
            },
            input: PendingInputObservation {
                revision: crate::document_pending::PendingInputRevision::new(11),
                ready_events: 0,
                intake_saturated: false,
                tasks: PendingTaskObservation::default(),
            },
            microtasks,
            execution: Some(DocumentExecutionObservation {
                clock_id: clock.id(),
                limits: DocumentExecutionLimits::CONTROLLED_WEBAPP_V1,
                counters: DocumentExecutionCounters::default(),
                terminal: None,
            }),
            producers,
            parser: PendingParserObservation::default(),
            network: PendingNetworkObservation::default(),
            logical_timers: crate::document_pending::PendingLogicalTimerSnapshot::default(),
            rendering: rendering(),
            sources: PendingSourceSnapshot::default(),
            terminals: PendingRuntimeTerminals::default(),
        };
        pending.validate().unwrap();
        pending
    }

    fn execution_terminated_pending() -> RawPendingSnapshot {
        let mut pending = eligible_pending();
        pending.execution = Some(DocumentExecutionObservation {
            clock_id: pending.clock.clock_id,
            limits: DocumentExecutionLimits {
                ordinary_tasks: 1,
                microtasks: 2,
                rendering_opportunities: 3,
                mutations: 4,
            },
            counters: DocumentExecutionCounters {
                ordinary_tasks: 1,
                ..DocumentExecutionCounters::default()
            },
            terminal: Some(DocumentExecutionTerminal::BudgetExceeded {
                budget: DocumentExecutionBudget::OrdinaryTasks,
                limit: 1,
                observed: 2,
            }),
        });
        pending.validate().unwrap();
        pending
    }

    fn token() -> DocumentAdvanceToken {
        DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(13), &eligible_pending())
            .unwrap()
    }

    fn automation_command(operation: DocumentAutomationOperation) -> DocumentControlCommand {
        let pending = eligible_pending();
        DocumentControlCommand::Automate(Box::new(
            DocumentAutomationRequest::new_internal(
                pending.target,
                pending.state_generation,
                operation,
                DocumentAutomationLimits::MVP,
            )
            .unwrap(),
        ))
    }

    #[test]
    fn raw_bound_token_command_and_outcomes_round_trip() {
        let pending = eligible_pending();
        let token =
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(13), &pending).unwrap();
        token.validate_against(&pending).unwrap();
        assert_eq!(token.input_revision(), pending.input.revision);
        assert!(!token.input_intake_saturated());
        assert_postcard_round_trip(token.clone());
        assert_postcard_round_trip(DocumentControlCommand::Observe);
        assert_postcard_round_trip(DocumentControlCommand::DriveOneTurn);
        assert_postcard_round_trip(DocumentControlCommand::BootstrapInitialPipeline {
            pipeline_id: TEST_PIPELINE_ID,
        });
        let text_command = automation_command(DocumentAutomationOperation::TextContent {
            selector: "#status".into(),
        });
        assert_postcard_round_trip(text_command.clone());
        assert_postcard_round_trip(DocumentControlCommand::AdvanceTo(Box::new(token.clone())));

        let observation = DocumentControlObservation::new_internal(
            DocumentControlAction::Observed,
            Box::new(pending),
            Some(token.clone()),
        )
        .unwrap();
        let completed = DocumentControlOutcome::Completed(Box::new(observation));
        completed.validate().unwrap();
        assert_postcard_round_trip(completed);
        assert_postcard_round_trip(DocumentControlOutcome::Rejected(
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Sources),
        ));
        assert_postcard_round_trip(DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate {
            target: Box::new(token.target().clone()),
        });
        assert_postcard_round_trip(DocumentControlOutcome::AdvanceOutcomeIndeterminate {
            token_id: token.id(),
            target: Box::new(token.target().clone()),
            deadline: token.deadline(),
        });

        let DocumentControlCommand::Automate(request) = text_command else {
            unreachable!();
        };
        let observation = DocumentControlObservation::new_internal(
            DocumentControlAction::Automated(DocumentControlAutomationKind::TextContent),
            Box::new(eligible_pending()),
            None,
        )
        .unwrap();
        let completed = DocumentControlOutcome::AutomationCompleted {
            result: DocumentAutomationResult::TextContent {
                value: "ready".into(),
            },
            observation: Box::new(observation),
        };
        completed
            .validate_for_command(&DocumentControlCommand::Automate(request))
            .unwrap();
        assert_postcard_round_trip(completed);
        assert_postcard_round_trip(DocumentControlOutcome::AutomationOutcomeIndeterminate {
            target: Box::new(token.target().clone()),
            operation: DocumentControlAutomationKind::Activate,
        });
    }

    #[test]
    fn pending_fact_and_capture_errors_round_trip() {
        for fact in [
            DocumentPendingFact::TargetMembership,
            DocumentPendingFact::StateGeneration,
            DocumentPendingFact::DomMutationEpoch,
            DocumentPendingFact::Clock,
            DocumentPendingFact::Scheduler,
            DocumentPendingFact::Input,
            DocumentPendingFact::MicrotaskCoverage,
            DocumentPendingFact::Producers,
            DocumentPendingFact::Parser,
            DocumentPendingFact::Network,
            DocumentPendingFact::LogicalTimers,
            DocumentPendingFact::Rendering,
            DocumentPendingFact::Sources,
            DocumentPendingFact::RuntimeTerminals,
        ] {
            assert_postcard_round_trip(fact);
            assert_postcard_round_trip(DocumentControlError::PendingFactUnavailable(fact));
        }

        assert_postcard_round_trip(DocumentControlError::PendingSnapshot(
            PendingSnapshotInvariantError::ProducerPriorEmptyMissing,
        ));

        let expected = eligible_pending().target;
        let mut observed = expected.clone();
        observed.navigation_revision = PendingNavigationRevision::new(4);
        observed.validate().unwrap();
        let error = DocumentControlError::TargetChanged {
            expected: Box::new(expected),
            observed: Box::new(observed),
        };
        error.validate().unwrap();
        assert_postcard_round_trip(error);
    }

    #[test]
    fn initial_pipeline_bootstrap_is_an_observe_only_definitive_rejection() {
        let rejection = DocumentControlOutcome::Rejected(
            DocumentControlError::InitialPipelineBootstrapRequired {
                pipeline_id: TEST_PIPELINE_ID,
            },
        );
        rejection
            .validate_for_command(&DocumentControlCommand::Observe)
            .unwrap();
        assert!(matches!(
            rejection.validate_for_command(&DocumentControlCommand::DriveOneTurn),
            Err(
                DocumentControlOutcomeInvariantError::InitialPipelineBootstrapRejectionForCommand {
                    command: DocumentControlCommandKind::DriveOneTurn,
                    pipeline_id: TEST_PIPELINE_ID,
                }
            )
        ));
        assert_postcard_round_trip(rejection);
    }

    #[test]
    fn execution_termination_is_an_authoritative_completed_drive_or_bootstrap() {
        let terminated = completed_outcome(
            DocumentControlAction::ExecutionTerminated,
            execution_terminated_pending(),
        );
        terminated
            .validate_for_command(&DocumentControlCommand::DriveOneTurn)
            .unwrap();
        terminated
            .validate_for_command(&DocumentControlCommand::BootstrapInitialPipeline {
                pipeline_id: TEST_PIPELINE_ID,
            })
            .unwrap();
        assert!(matches!(
            terminated.validate_for_command(&DocumentControlCommand::Observe),
            Err(
                DocumentControlOutcomeInvariantError::CompletedActionMismatch {
                    command: DocumentControlCommandKind::Observe,
                    observed: DocumentControlAction::ExecutionTerminated,
                }
            )
        ));
        assert_postcard_round_trip(terminated);

        assert_eq!(
            DocumentControlObservation::new_internal(
                DocumentControlAction::ExecutionTerminated,
                Box::new(eligible_pending()),
                None,
            ),
            Err(DocumentControlObservationInvariantError::ExecutionTerminationMissing)
        );
        assert_eq!(
            DocumentAdvanceToken::new_internal(
                DocumentAdvanceTokenId::new(71),
                &execution_terminated_pending(),
            ),
            Err(DocumentAdvanceTokenInvariantError::RuntimeTerminal)
        );
    }

    #[test]
    fn bootstrap_unavailable_is_bound_to_the_exact_bootstrap_command() {
        let command = DocumentControlCommand::BootstrapInitialPipeline {
            pipeline_id: TEST_PIPELINE_ID,
        };
        let rejection = DocumentControlOutcome::Rejected(
            DocumentControlError::InitialPipelineBootstrapUnavailable {
                pipeline_id: TEST_PIPELINE_ID,
            },
        );
        rejection.validate_for_command(&command).unwrap();
        assert!(matches!(
            rejection.validate_for_command(&DocumentControlCommand::DriveOneTurn),
            Err(
                DocumentControlOutcomeInvariantError::InitialPipelineBootstrapUnavailableForCommand {
                    command: DocumentControlCommandKind::DriveOneTurn,
                    expected: None,
                    observed: TEST_PIPELINE_ID,
                }
            )
        ));
        let other_pipeline_id = PipelineId {
            namespace_id: TEST_NAMESPACE,
            index: Index::new(TEST_PIPELINE_ID.index.0.get() + 1).unwrap(),
        };
        assert!(matches!(
            rejection.validate_for_command(&DocumentControlCommand::BootstrapInitialPipeline {
                pipeline_id: other_pipeline_id,
            }),
            Err(
                DocumentControlOutcomeInvariantError::InitialPipelineBootstrapUnavailableForCommand {
                    command: DocumentControlCommandKind::BootstrapInitialPipeline,
                    expected: Some(expected),
                    observed: TEST_PIPELINE_ID,
                }
            ) if expected == other_pipeline_id
        ));
        completed_outcome(
            DocumentControlAction::TurnProcessed {
                microtask_checkpoint_advanced: true,
            },
            eligible_pending(),
        )
        .validate_for_command(&command)
        .unwrap();
        assert!(matches!(
            completed_outcome(
                DocumentControlAction::CheckpointTurnProcessed {
                    microtask_checkpoint_advanced: true,
                },
                eligible_pending(),
            )
            .validate_for_command(&command),
            Err(
                DocumentControlOutcomeInvariantError::CompletedActionMismatch {
                    command: DocumentControlCommandKind::BootstrapInitialPipeline,
                    ..
                }
            )
        ));
        assert_postcard_round_trip(rejection);
    }

    #[test]
    fn initial_pipeline_activation_transition_is_exact_and_aba_safe() {
        let before = PendingTargetObservation::new_with_authority(
            TEST_WEBVIEW_ID,
            TEST_SCRIPT_EVENT_LOOP_ID,
            None,
            PendingNavigationRevision::new(3),
            PendingPipelineMembershipRevision::new(9),
            None,
            vec![TEST_PIPELINE_ID],
            Vec::new(),
            vec![TEST_PIPELINE_ID],
        )
        .unwrap();
        let after = PendingTargetObservation::new_with_authority(
            TEST_WEBVIEW_ID,
            TEST_SCRIPT_EVENT_LOOP_ID,
            Some(PendingActiveTopLevelPipeline {
                pipeline_id: TEST_PIPELINE_ID,
                epoch: Epoch(8),
            }),
            PendingNavigationRevision::new(5),
            PendingPipelineMembershipRevision::new(9),
            None,
            vec![TEST_PIPELINE_ID],
            vec![TEST_PIPELINE_ID],
            Vec::new(),
        )
        .unwrap();
        assert!(is_exact_initial_pipeline_activation_transition(
            &before,
            &after,
            TEST_PIPELINE_ID,
        ));

        let mut one_revision_only = after.clone();
        one_revision_only.navigation_revision = PendingNavigationRevision::new(4);
        assert!(!is_exact_initial_pipeline_activation_transition(
            &before,
            &one_revision_only,
            TEST_PIPELINE_ID,
        ));

        let mut membership_aba = after.clone();
        membership_aba.pipeline_membership_revision = PendingPipelineMembershipRevision::new(10);
        assert!(!is_exact_initial_pipeline_activation_transition(
            &before,
            &membership_aba,
            TEST_PIPELINE_ID,
        ));

        let mut unsupported_surface_changed = after;
        unsupported_surface_changed.unsupported_time_surface =
            Some(DocumentTimeSurface::SameEventLoopIframe);
        assert!(!is_exact_initial_pipeline_activation_transition(
            &before,
            &unsupported_surface_changed,
            TEST_PIPELINE_ID,
        ));
    }

    #[test]
    fn target_changed_error_revalidates_both_authorities_and_actual_drift() {
        let expected = eligible_pending().target;
        let mut observed = expected.clone();
        observed.navigation_revision = PendingNavigationRevision::new(4);

        let mut malformed_expected = expected.clone();
        malformed_expected.active_top_level = None;
        let malformed = DocumentControlOutcome::Rejected(DocumentControlError::TargetChanged {
            expected: Box::new(malformed_expected),
            observed: Box::new(observed),
        });
        assert!(matches!(
            malformed.validate(),
            Err(DocumentControlOutcomeInvariantError::Rejection(
                DocumentControlErrorInvariantError::TargetChangedExpected(
                    PendingSnapshotInvariantError::FullyActivePipelineWithoutActiveTopLevel(
                        TEST_PIPELINE_ID
                    )
                )
            ))
        ));

        let mut malformed_observed = expected.clone();
        malformed_observed.active_top_level = None;
        let malformed = DocumentControlOutcome::Rejected(DocumentControlError::TargetChanged {
            expected: Box::new(expected.clone()),
            observed: Box::new(malformed_observed),
        });
        assert!(matches!(
            malformed.validate(),
            Err(DocumentControlOutcomeInvariantError::Rejection(
                DocumentControlErrorInvariantError::TargetChangedObserved(
                    PendingSnapshotInvariantError::FullyActivePipelineWithoutActiveTopLevel(
                        TEST_PIPELINE_ID
                    )
                )
            ))
        ));

        let unchanged = DocumentControlOutcome::Rejected(DocumentControlError::TargetChanged {
            expected: Box::new(expected.clone()),
            observed: Box::new(expected.clone()),
        });
        assert!(matches!(
            unchanged.validate(),
            Err(DocumentControlOutcomeInvariantError::Rejection(
                DocumentControlErrorInvariantError::TargetDidNotChange { target }
            )) if *target == expected
        ));
    }

    #[test]
    fn token_rejects_target_clock_scheduler_and_deadline_changes() {
        let pending = eligible_pending();
        let token =
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(1), &pending).unwrap();

        let mut changed = pending.clone();
        changed.target.navigation_revision = PendingNavigationRevision::new(4);
        assert!(matches!(
            token.validate_against(&changed),
            Err(DocumentAdvanceTokenInvariantError::TargetChanged { .. })
        ));

        let mut changed = pending.clone();
        changed.state_generation = RuntimeStateGeneration::new(10);
        assert!(matches!(
            token.validate_against(&changed),
            Err(DocumentAdvanceTokenInvariantError::StateGenerationChanged { .. })
        ));

        let mut changed = pending.clone();
        changed.clock.now = DocumentTime::from_nanos(changed.clock.now.as_nanos() + 1);
        assert!(matches!(
            token.validate_against(&changed),
            Err(DocumentAdvanceTokenInvariantError::ClockChanged { .. })
        ));

        let other_clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 5,
            unix_time_origin_ns: DocumentUnixTime::default(),
        });
        let mut other_scheduler = TimerScheduler::with_clock(other_clock);
        other_scheduler.schedule_timer(TimerEventRequest {
            callback: Box::new(|| {}),
            duration: Duration::from_nanos(20),
        });
        let mut changed = pending.clone();
        changed.scheduler = PendingSchedulerObservation {
            scheduler_id: other_scheduler.id(),
            next_deadline: other_scheduler.finite_deadline_snapshot().unwrap(),
        };
        assert!(matches!(
            token.validate_against(&changed),
            Err(DocumentAdvanceTokenInvariantError::SchedulerChanged { .. })
        ));

        let mut changed = pending;
        changed.scheduler.next_deadline.as_mut().unwrap().deadline = DocumentTime::from_nanos(26);
        assert!(matches!(
            token.validate_against(&changed),
            Err(DocumentAdvanceTokenInvariantError::DeadlineChanged { .. })
        ));
    }

    #[test]
    fn token_rejects_changed_or_unqualified_input_microtasks_and_producers() {
        let pending = eligible_pending();
        let token =
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(2), &pending).unwrap();

        let mut changed = pending.clone();
        changed.input.revision = crate::document_pending::PendingInputRevision::new(12);
        assert!(matches!(
            token.validate_against(&changed),
            Err(DocumentAdvanceTokenInvariantError::InputChanged { .. })
        ));

        let mut changed = pending.clone();
        changed.input.intake_saturated = true;
        assert!(matches!(
            token.validate_against(&changed),
            Err(DocumentAdvanceTokenInvariantError::InputIntakeSaturated { .. })
        ));

        let mut changed = pending.clone();
        let (microtasks, producers) = stable_producers(3);
        changed.microtasks = microtasks;
        changed.producers = producers;
        assert!(matches!(
            token.validate_against(&changed),
            Err(DocumentAdvanceTokenInvariantError::MicrotasksChanged { .. })
        ));

        let mut changed = pending;
        let (_, producers) = stable_producers(2);
        changed.producers = producers;
        assert!(matches!(
            token.validate_against(&changed),
            Err(DocumentAdvanceTokenInvariantError::ProducersChanged { .. })
        ));
    }

    #[test]
    fn token_issuance_fails_closed_before_private_authority_exists() {
        let mut pending = eligible_pending();
        pending.clock.mode = PendingClockMode::Realtime;
        assert_eq!(
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(1), &pending),
            Err(DocumentAdvanceTokenInvariantError::ClockNotControlled(
                PendingClockMode::Realtime
            ))
        );

        let mut pending = eligible_pending();
        pending.scheduler.next_deadline = None;
        assert_eq!(
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(1), &pending),
            Err(DocumentAdvanceTokenInvariantError::NoFiniteDeadline)
        );

        let mut pending = eligible_pending();
        pending.input.ready_events = 1;
        assert!(matches!(
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(1), &pending),
            Err(DocumentAdvanceTokenInvariantError::ReadyInput(_))
        ));

        let mut pending = eligible_pending();
        pending.microtasks.queued = 1;
        assert!(matches!(
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(1), &pending),
            Err(DocumentAdvanceTokenInvariantError::MicrotasksNotDrained(_))
        ));
    }

    #[test]
    fn token_issuance_allows_exact_now_and_rejects_only_past_deadlines() {
        let mut pending = eligible_pending();
        let deadline = pending.scheduler.next_deadline.unwrap();
        pending.clock.now = deadline.deadline;
        let token =
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(1), &pending).unwrap();
        assert_eq!(token.now(), deadline.deadline);
        assert_eq!(token.deadline(), deadline);

        pending.clock.now = DocumentTime::from_nanos(deadline.deadline.as_nanos() + 1);
        assert_eq!(
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(2), &pending),
            Err(
                DocumentAdvanceTokenInvariantError::DeadlineBeforeCurrentTime {
                    now: pending.clock.now,
                    deadline,
                }
            )
        );
    }

    #[test]
    fn token_issuance_rejects_a_delivery_ready_logical_timer() {
        let mut pending = eligible_pending();
        let source_id = PendingSourceId::new(43);
        let timer = PendingLogicalTimerObservation {
            source_id,
            pipeline_id: TEST_PIPELINE_ID,
            stable_id: PendingLogicalTimerStableId::JavaScriptHandle(17),
            creation_sequence: 9,
            kind: PendingLogicalTimerKind::JavaScriptOneShot,
            logical_deadline: pending.clock.now,
            suspended: false,
            eligible_in_controlled_turn: true,
            is_ordering_head: true,
            delivery_ready: true,
            outer_wake: None,
        };
        pending.logical_timers = PendingLogicalTimerSnapshot::new(vec![timer]).unwrap();
        pending.sources = PendingSourceSnapshot::new(
            PendingSourceEpoch::new(1),
            vec![PendingSourceObservation {
                id: source_id,
                kind: PendingSourceKind::Timer,
                disposition: PendingSourceDisposition::Ready,
            }],
        )
        .unwrap();
        pending.validate().unwrap();

        assert_eq!(
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(1), &pending),
            Err(DocumentAdvanceTokenInvariantError::AuthoritativeReadyWork(
                DocumentAdvanceReadyWork::LogicalTimer(timer)
            ))
        );
    }

    #[test]
    fn completed_zero_delta_advance_accepts_consumed_head_and_advanced_state() {
        let mut pending = eligible_pending();
        let deadline = pending.scheduler.next_deadline.unwrap();
        pending.clock.now = deadline.deadline;
        let token =
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(7), &pending).unwrap();
        let command = DocumentControlCommand::AdvanceTo(Box::new(token.clone()));

        let mut completed = pending;
        completed.state_generation = RuntimeStateGeneration::new(10);
        completed.scheduler.next_deadline = None;
        completed_outcome(DocumentControlAction::TimerActivated(deadline), completed)
            .validate_for_command(&command)
            .unwrap();
    }

    #[test]
    fn automation_operation_kinds_and_mutability_are_exact() {
        for (operation, expected, mutating) in [
            (
                DocumentAutomationOperation::QueryCount {
                    selector: "div".into(),
                },
                DocumentControlAutomationKind::QueryCount,
                false,
            ),
            (
                DocumentAutomationOperation::TextContent {
                    selector: "div".into(),
                },
                DocumentControlAutomationKind::TextContent,
                false,
            ),
            (
                DocumentAutomationOperation::InnerHtml {
                    selector: "div".into(),
                },
                DocumentControlAutomationKind::InnerHtml,
                false,
            ),
            (
                DocumentAutomationOperation::Extract(
                    crate::document_automation::DocumentExtractionPlan::new_internal(
                        ".row".into(),
                        vec![
                            crate::document_automation::DocumentExtractionField::new_internal(
                                "name".into(),
                                ".name".into(),
                                crate::document_automation::DocumentExtractionRead::TextContent,
                            ),
                        ],
                    ),
                ),
                DocumentControlAutomationKind::Extract,
                false,
            ),
            (
                DocumentAutomationOperation::Fill {
                    selector: "input".into(),
                    value: "value".into(),
                },
                DocumentControlAutomationKind::Fill,
                true,
            ),
            (
                DocumentAutomationOperation::Activate {
                    selector: "button".into(),
                },
                DocumentControlAutomationKind::Activate,
                true,
            ),
        ] {
            assert_eq!(
                DocumentControlAutomationKind::from_operation(&operation),
                expected
            );
            assert_eq!(expected.is_mutating(), mutating);
            assert_eq!(operation.may_mutate_document(), mutating);
        }
    }

    #[test]
    fn token_issuance_rejects_ready_parser_and_navigation_phases() {
        for phase in [
            PendingParserPhase::Ready,
            PendingParserPhase::AwaitingCommit,
        ] {
            let mut pending = eligible_pending();
            let source_id = PendingSourceId::new(41);
            pending.parser = PendingParserObservation::new(vec![PendingParserSourceObservation {
                source_id,
                pipeline_id: TEST_PIPELINE_ID,
                kind: PendingParserSourceKind::DocumentParser,
                phase,
                disposition: PendingSourceDisposition::Ready,
            }])
            .unwrap();
            pending.sources = PendingSourceSnapshot::new(
                PendingSourceEpoch::new(1),
                vec![PendingSourceObservation {
                    id: source_id,
                    kind: PendingSourceKind::Parser,
                    disposition: PendingSourceDisposition::Ready,
                }],
            )
            .unwrap();
            assert!(matches!(
                DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(1), &pending),
                Err(DocumentAdvanceTokenInvariantError::AuthoritativeReadyWork(
                    DocumentAdvanceReadyWork::Parser(source)
                )) if source.phase == phase
            ));
        }
    }

    #[test]
    fn token_issuance_rejects_terminal_network_delivery_ready_to_run() {
        let mut pending = eligible_pending();
        let source_id = PendingSourceId::new(42);
        let evidence = PendingExternalIoEvidence {
            owner: PendingExternalIoOwner::Script,
            load_blocking: PendingExternalIoLoadBlocking::NonBlocking,
        };
        pending.network = PendingNetworkObservation::new(vec![PendingExternalIoObservation {
            source_id,
            pipeline_id: TEST_PIPELINE_ID,
            kind: PendingNetworkKind::Fetch,
            phase: PendingExternalIoPhase::TerminalTaskQueued,
            evidence,
            started_at: pending.clock.now,
        }])
        .unwrap();
        pending.sources = PendingSourceSnapshot::new(
            PendingSourceEpoch::new(1),
            vec![PendingSourceObservation {
                id: source_id,
                kind: PendingSourceKind::Network,
                disposition: PendingSourceDisposition::Ready,
            }],
        )
        .unwrap();
        assert!(matches!(
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(1), &pending),
            Err(DocumentAdvanceTokenInvariantError::AuthoritativeReadyWork(
                DocumentAdvanceReadyWork::Network(operation)
            )) if operation.source_id == source_id
        ));
    }

    #[test]
    fn token_issuance_rejects_ready_or_immediately_required_rendering() {
        let mut pending = eligible_pending();
        pending.rendering =
            PendingRenderingObservation::new(None, true, pending.rendering.pipelines().to_vec())
                .unwrap();
        assert_eq!(
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(1), &pending),
            Err(DocumentAdvanceTokenInvariantError::AuthoritativeReadyWork(
                DocumentAdvanceReadyWork::RenderingOpportunity
            ))
        );

        let mut pending = eligible_pending();
        let mut pipeline = pending.rendering.pipelines()[0];
        pipeline.document_update_required = true;
        pending.rendering = PendingRenderingObservation::new(None, false, vec![pipeline]).unwrap();
        assert!(matches!(
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(1), &pending),
            Err(DocumentAdvanceTokenInvariantError::AuthoritativeReadyWork(
                DocumentAdvanceReadyWork::RenderingRequired(observed)
            )) if observed.pipeline_id == TEST_PIPELINE_ID
        ));
    }

    #[test]
    fn token_issuance_allows_required_rendering_bound_to_future_scheduler_head() {
        let mut pending = eligible_pending();
        let deadline = pending.scheduler.next_deadline.unwrap();
        let mut pipeline = pending.rendering.pipelines()[0];
        pipeline.document_update_required = true;
        pending.rendering =
            PendingRenderingObservation::new(Some(deadline), false, vec![pipeline]).unwrap();

        let token =
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(1), &pending).unwrap();
        assert_eq!(token.deadline(), deadline);
    }

    #[test]
    fn token_issuance_allows_required_rendering_bound_to_exact_now_scheduler_head() {
        let mut pending = eligible_pending();
        let deadline = pending.scheduler.next_deadline.unwrap();
        pending.clock.now = deadline.deadline;
        let mut pipeline = pending.rendering.pipelines()[0];
        pipeline.document_update_required = true;
        pending.rendering =
            PendingRenderingObservation::new(Some(deadline), false, vec![pipeline]).unwrap();

        let token =
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(1), &pending).unwrap();
        assert_eq!(token.deadline(), deadline);
    }

    fn completed_outcome(
        action: DocumentControlAction,
        pending: RawPendingSnapshot,
    ) -> DocumentControlOutcome {
        DocumentControlOutcome::Completed(Box::new(
            DocumentControlObservation::new_internal(action, Box::new(pending), None).unwrap(),
        ))
    }

    fn automation_completed_outcome(
        operation: DocumentControlAutomationKind,
        result: DocumentAutomationResult,
        pending: RawPendingSnapshot,
    ) -> DocumentControlOutcome {
        DocumentControlOutcome::AutomationCompleted {
            result,
            observation: Box::new(
                DocumentControlObservation::new_internal(
                    DocumentControlAction::Automated(operation),
                    Box::new(pending),
                    None,
                )
                .unwrap(),
            ),
        }
    }

    #[test]
    fn automation_completion_binds_action_result_target_and_generation() {
        let command = automation_command(DocumentAutomationOperation::TextContent {
            selector: "#status".into(),
        });
        let completed = automation_completed_outcome(
            DocumentControlAutomationKind::TextContent,
            DocumentAutomationResult::TextContent {
                value: "ready".into(),
            },
            eligible_pending(),
        );
        completed.validate_for_command(&command).unwrap();

        let wrong_action = automation_completed_outcome(
            DocumentControlAutomationKind::QueryCount,
            DocumentAutomationResult::QueryCount { count: 1 },
            eligible_pending(),
        );
        assert!(matches!(
            wrong_action.validate_for_command(&command),
            Err(DocumentControlOutcomeInvariantError::AutomationOperationMismatch { .. })
        ));

        let wrong_result = automation_completed_outcome(
            DocumentControlAutomationKind::TextContent,
            DocumentAutomationResult::QueryCount { count: 1 },
            eligible_pending(),
        );
        assert!(matches!(
            wrong_result.validate_for_command(&command),
            Err(DocumentControlOutcomeInvariantError::AutomationResultMismatch { .. })
        ));

        let mut changed_target = eligible_pending();
        changed_target.target.navigation_revision = PendingNavigationRevision::new(4);
        let changed_target = automation_completed_outcome(
            DocumentControlAutomationKind::TextContent,
            DocumentAutomationResult::TextContent {
                value: "ready".into(),
            },
            changed_target,
        );
        assert!(matches!(
            changed_target.validate_for_command(&command),
            Err(DocumentControlOutcomeInvariantError::AutomationTargetMismatch { .. })
        ));

        let mut regressed = eligible_pending();
        regressed.state_generation = RuntimeStateGeneration::new(8);
        let regressed = automation_completed_outcome(
            DocumentControlAutomationKind::TextContent,
            DocumentAutomationResult::TextContent {
                value: "ready".into(),
            },
            regressed,
        );
        assert!(matches!(
            regressed.validate_for_command(&command),
            Err(DocumentControlOutcomeInvariantError::AutomationStateGenerationRegressed { .. })
        ));

        let DocumentControlCommand::Automate(request) = &command else {
            unreachable!();
        };
        let read_only_indeterminate = DocumentControlOutcome::AutomationOutcomeIndeterminate {
            target: Box::new(request.target().clone()),
            operation: DocumentControlAutomationKind::TextContent,
        };
        assert!(matches!(
            read_only_indeterminate.validate_for_command(&command),
            Err(
                DocumentControlOutcomeInvariantError::ReadOnlyAutomationIndeterminate {
                    operation: DocumentControlAutomationKind::TextContent,
                }
            )
        ));
    }

    #[test]
    fn standalone_automation_outcomes_validate_action_result_and_mutability() {
        let wrong_result = automation_completed_outcome(
            DocumentControlAutomationKind::TextContent,
            DocumentAutomationResult::QueryCount { count: 1 },
            eligible_pending(),
        );
        assert!(matches!(
            wrong_result.validate(),
            Err(
                DocumentControlOutcomeInvariantError::AutomationResultMismatch {
                    expected: DocumentControlAutomationKind::TextContent,
                }
            )
        ));

        let wrong_action = DocumentControlOutcome::AutomationCompleted {
            result: DocumentAutomationResult::TextContent {
                value: "ready".into(),
            },
            observation: Box::new(
                DocumentControlObservation::new_internal(
                    DocumentControlAction::Observed,
                    Box::new(eligible_pending()),
                    None,
                )
                .unwrap(),
            ),
        };
        assert!(matches!(
            wrong_action.validate(),
            Err(
                DocumentControlOutcomeInvariantError::CompletedActionMismatch {
                    command: DocumentControlCommandKind::Automate,
                    observed: DocumentControlAction::Observed,
                }
            )
        ));

        let read_only_indeterminate = DocumentControlOutcome::AutomationOutcomeIndeterminate {
            target: Box::new(eligible_pending().target),
            operation: DocumentControlAutomationKind::TextContent,
        };
        assert!(matches!(
            read_only_indeterminate.validate(),
            Err(
                DocumentControlOutcomeInvariantError::ReadOnlyAutomationIndeterminate {
                    operation: DocumentControlAutomationKind::TextContent,
                }
            )
        ));
    }

    #[test]
    fn decoded_indeterminate_outcomes_revalidate_target_authority() {
        let token = token();
        let mut invalid_target = token.target().clone();
        invalid_target.active_top_level = None;
        let outcome = DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate {
            target: Box::new(invalid_target),
        };
        let bytes = postcard::to_stdvec(&outcome).unwrap();
        let decoded: DocumentControlOutcome = postcard::from_bytes(&bytes).unwrap();

        assert!(matches!(
            decoded.validate(),
            Err(DocumentControlOutcomeInvariantError::IndeterminateTarget(
                PendingSnapshotInvariantError::FullyActivePipelineWithoutActiveTopLevel(
                    TEST_PIPELINE_ID
                )
            ))
        ));
    }

    #[test]
    fn command_aware_outcome_validation_binds_action_target_and_deadline() {
        let pending = eligible_pending();
        let token =
            DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(51), &pending).unwrap();
        let advance = DocumentControlCommand::AdvanceTo(Box::new(token.clone()));

        let observed = completed_outcome(DocumentControlAction::Observed, pending.clone());
        observed
            .validate_for_command(&DocumentControlCommand::Observe)
            .unwrap();
        assert!(matches!(
            observed.validate_for_command(&DocumentControlCommand::DriveOneTurn),
            Err(
                DocumentControlOutcomeInvariantError::CompletedActionMismatch {
                    command: DocumentControlCommandKind::DriveOneTurn,
                    observed: DocumentControlAction::Observed,
                }
            )
        ));

        let driven = completed_outcome(
            DocumentControlAction::TurnProcessed {
                microtask_checkpoint_advanced: true,
            },
            pending.clone(),
        );
        driven
            .validate_for_command(&DocumentControlCommand::DriveOneTurn)
            .unwrap();
        assert!(matches!(
            driven.validate_for_command(&DocumentControlCommand::Observe),
            Err(
                DocumentControlOutcomeInvariantError::CompletedActionMismatch {
                    command: DocumentControlCommandKind::Observe,
                    observed: DocumentControlAction::TurnProcessed { .. },
                }
            )
        ));

        let mut advanced_pending = pending.clone();
        advanced_pending.state_generation = RuntimeStateGeneration::new(10);
        advanced_pending.clock.now = token.deadline().deadline;
        advanced_pending.scheduler.next_deadline = None;
        let activated = completed_outcome(
            DocumentControlAction::TimerActivated(token.deadline()),
            advanced_pending.clone(),
        );
        activated.validate_for_command(&advance).unwrap();

        let foreign_clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: token.deadline().deadline.as_nanos(),
            unix_time_origin_ns: DocumentUnixTime::default(),
        });
        let mut wrong_clock_identity = advanced_pending.clone();
        wrong_clock_identity.clock.clock_id = foreign_clock.id();
        wrong_clock_identity.execution.as_mut().unwrap().clock_id = foreign_clock.id();
        let mismatched_clock_identity = completed_outcome(
            DocumentControlAction::TimerActivated(token.deadline()),
            wrong_clock_identity,
        );
        assert!(matches!(
            mismatched_clock_identity.validate_for_command(&advance),
            Err(DocumentControlOutcomeInvariantError::AdvanceCompletedClockIdentityMismatch { .. })
        ));

        let foreign_scheduler = TimerScheduler::with_clock(foreign_clock);
        let mut wrong_scheduler_identity = advanced_pending.clone();
        wrong_scheduler_identity.scheduler.scheduler_id = foreign_scheduler.id();
        let mismatched_scheduler_identity = completed_outcome(
            DocumentControlAction::TimerActivated(token.deadline()),
            wrong_scheduler_identity,
        );
        assert!(matches!(
            mismatched_scheduler_identity.validate_for_command(&advance),
            Err(
                DocumentControlOutcomeInvariantError::AdvanceCompletedSchedulerIdentityMismatch { .. }
            )
        ));

        let mut unchanged_generation = advanced_pending.clone();
        unchanged_generation.state_generation = token.state_generation();
        let stale_generation = completed_outcome(
            DocumentControlAction::TimerActivated(token.deadline()),
            unchanged_generation,
        );
        assert!(matches!(
            stale_generation.validate_for_command(&advance),
            Err(
                DocumentControlOutcomeInvariantError::AdvanceCompletedStateGenerationNotAdvanced { .. }
            )
        ));

        let mut unconsumed_head = advanced_pending.clone();
        unconsumed_head.scheduler.next_deadline = Some(token.deadline());
        let stale_head = completed_outcome(
            DocumentControlAction::TimerActivated(token.deadline()),
            unconsumed_head,
        );
        assert!(matches!(
            stale_head.validate_for_command(&advance),
            Err(DocumentControlOutcomeInvariantError::AdvanceCompletedHeadNotConsumed { .. })
        ));

        let mut wrong_deadline = token.deadline();
        wrong_deadline.deadline = DocumentTime::from_nanos(wrong_deadline.deadline.as_nanos() + 1);
        let mismatched_deadline = completed_outcome(
            DocumentControlAction::TimerActivated(wrong_deadline),
            advanced_pending.clone(),
        );
        assert!(matches!(
            mismatched_deadline.validate_for_command(&advance),
            Err(DocumentControlOutcomeInvariantError::AdvanceDeadlineMismatch { .. })
        ));

        let mut wrong_target = advanced_pending.clone();
        wrong_target.target.navigation_revision = PendingNavigationRevision::new(4);
        let mismatched_target = completed_outcome(
            DocumentControlAction::TimerActivated(token.deadline()),
            wrong_target,
        );
        assert!(matches!(
            mismatched_target.validate_for_command(&advance),
            Err(DocumentControlOutcomeInvariantError::AdvanceTargetMismatch { .. })
        ));

        advanced_pending.clock.now =
            DocumentTime::from_nanos(token.deadline().deadline.as_nanos() + 1);
        let mismatched_clock = completed_outcome(
            DocumentControlAction::TimerActivated(token.deadline()),
            advanced_pending,
        );
        assert!(matches!(
            mismatched_clock.validate_for_command(&advance),
            Err(DocumentControlOutcomeInvariantError::AdvanceCompletedClockMismatch { .. })
        ));

        let wrong_token = DocumentControlOutcome::AdvanceOutcomeIndeterminate {
            token_id: DocumentAdvanceTokenId::new(token.id().get() + 1),
            target: Box::new(token.target().clone()),
            deadline: token.deadline(),
        };
        assert!(matches!(
            wrong_token.validate_for_command(&advance),
            Err(DocumentControlOutcomeInvariantError::AdvanceTokenIdentityMismatch { .. })
        ));
    }

    fn cancellable_receiver(
        command: &DocumentControlCommand,
        cancellations: &Arc<AtomicUsize>,
    ) -> (
        servo_base::generic_channel::GenericCallback<DocumentControlOutcome>,
        DocumentControlReceiver,
    ) {
        let (response, receiver) = GenericCallback::new_blocking().unwrap();
        let cancellations = cancellations.clone();
        let receiver = DocumentControlReceiver::new_cancellable(receiver, command, move || {
            cancellations.fetch_add(1, Ordering::SeqCst);
        });
        (response, receiver)
    }

    #[test]
    fn receiver_preserves_observe_drive_and_advance_failure_semantics() {
        let cancellations = Arc::new(AtomicUsize::new(0));
        let (_response, receiver) =
            cancellable_receiver(&DocumentControlCommand::Observe, &cancellations);
        assert_eq!(
            receiver.recv_timeout(Duration::ZERO),
            DocumentControlReceiveOutcome::ObserveTransportFailure(
                DocumentControlTransportFailure::TimedOut
            )
        );

        let (_response, receiver) =
            cancellable_receiver(&DocumentControlCommand::DriveOneTurn, &cancellations);
        assert_eq!(
            receiver.recv_timeout(Duration::ZERO),
            DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(
                DocumentControlTransportFailure::TimedOut
            )
        );

        let bootstrap = DocumentControlCommand::BootstrapInitialPipeline {
            pipeline_id: TEST_PIPELINE_ID,
        };
        let (_response, receiver) = cancellable_receiver(&bootstrap, &cancellations);
        assert_eq!(
            receiver.recv_timeout(Duration::ZERO),
            DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(
                DocumentControlTransportFailure::TimedOut
            )
        );

        let token = token();
        let command = DocumentControlCommand::AdvanceTo(Box::new(token.clone()));
        let (_response, receiver) = cancellable_receiver(&command, &cancellations);
        assert!(matches!(
            receiver.recv_timeout(Duration::ZERO),
            DocumentControlReceiveOutcome::CommandOutcome(
                DocumentControlOutcome::AdvanceOutcomeIndeterminate {
                    token_id,
                    target,
                    deadline,
                }
            ) if token_id == token.id() && *target == *token.target() &&
                deadline == token.deadline()
        ));
        assert_eq!(cancellations.load(Ordering::SeqCst), 4);
    }

    #[test]
    fn receiver_distinguishes_read_only_and_mutating_automation_failure() {
        let cancellations = Arc::new(AtomicUsize::new(0));
        let read_only = automation_command(DocumentAutomationOperation::TextContent {
            selector: "#status".into(),
        });
        let (_response, receiver) = cancellable_receiver(&read_only, &cancellations);
        assert_eq!(
            receiver.recv_timeout(Duration::ZERO),
            DocumentControlReceiveOutcome::AutomationTransportFailure(
                DocumentControlTransportFailure::TimedOut
            )
        );

        let mutating = automation_command(DocumentAutomationOperation::Activate {
            selector: "#start".into(),
        });
        let (_response, receiver) = cancellable_receiver(&mutating, &cancellations);
        let DocumentControlCommand::Automate(request) = &mutating else {
            unreachable!();
        };
        assert!(matches!(
            receiver.recv_timeout(Duration::ZERO),
            DocumentControlReceiveOutcome::CommandOutcome(
                DocumentControlOutcome::AutomationOutcomeIndeterminate {
                    target,
                    operation: DocumentControlAutomationKind::Activate,
                }
            ) if *target == request.target().clone()
        ));
        assert_eq!(cancellations.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn mutating_automation_capture_and_post_mutation_errors_cannot_be_definitive() {
        let cancellations = Arc::new(AtomicUsize::new(0));
        let activate = automation_command(DocumentAutomationOperation::Activate {
            selector: "#start".into(),
        });
        let capture_rejection = DocumentControlOutcome::Rejected(
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Rendering),
        );
        assert!(matches!(
            capture_rejection.validate_for_command(&activate),
            Err(
                DocumentControlOutcomeInvariantError::PendingCaptureRejectionForMutatingCommand {
                    command: DocumentControlCommandKind::Automate,
                    ..
                }
            )
        ));
        let (response, receiver) = cancellable_receiver(&activate, &cancellations);
        response.send(capture_rejection).unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            DocumentControlReceiveOutcome::CommandOutcome(
                DocumentControlOutcome::AutomationOutcomeIndeterminate {
                    operation: DocumentControlAutomationKind::Activate,
                    ..
                }
            )
        ));

        let fill = automation_command(DocumentAutomationOperation::Fill {
            selector: "#name".into(),
            value: "Ada".into(),
        });
        let mutation_rejection = DocumentControlOutcome::Rejected(
            DocumentControlError::Automation(DocumentAutomationError::DomOperationFailed {
                operation: DocumentAutomationOperationKind::Fill,
            }),
        );
        assert!(matches!(
            mutation_rejection.validate_for_command(&fill),
            Err(
                DocumentControlOutcomeInvariantError::AutomationMutationRejection {
                    operation: DocumentControlAutomationKind::Fill,
                    ..
                }
            )
        ));
        let (response, receiver) = cancellable_receiver(&fill, &cancellations);
        response.send(mutation_rejection).unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            DocumentControlReceiveOutcome::CommandOutcome(
                DocumentControlOutcome::AutomationOutcomeIndeterminate {
                    operation: DocumentControlAutomationKind::Fill,
                    ..
                }
            )
        ));
        assert_eq!(cancellations.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn receiver_cancellation_is_exactly_once_and_success_disarms_it() {
        let cancellations = Arc::new(AtomicUsize::new(0));
        let (_response, receiver) =
            cancellable_receiver(&DocumentControlCommand::DriveOneTurn, &cancellations);
        assert_eq!(
            receiver.cancel(),
            DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(
                DocumentControlTransportFailure::Cancelled
            )
        );
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);

        let (response, receiver) =
            cancellable_receiver(&DocumentControlCommand::Observe, &cancellations);
        response
            .send(DocumentControlOutcome::Rejected(
                DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Rendering),
            ))
            .unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            DocumentControlReceiveOutcome::CommandOutcome(DocumentControlOutcome::Rejected(
                DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Rendering)
            ))
        ));
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);

        let (_response, receiver) =
            cancellable_receiver(&DocumentControlCommand::Observe, &cancellations);
        let receiver = match receiver.try_recv() {
            DocumentControlTryReceiveOutcome::Pending(receiver) => receiver,
            DocumentControlTryReceiveOutcome::Complete(_) => panic!("empty receiver completed"),
        };
        drop(receiver);
        assert_eq!(cancellations.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn receiver_preserves_capture_failure_semantics_by_command() {
        let cancellations = Arc::new(AtomicUsize::new(0));

        let (response, receiver) =
            cancellable_receiver(&DocumentControlCommand::Observe, &cancellations);
        response
            .send(DocumentControlOutcome::Rejected(
                DocumentControlError::PendingSnapshot(
                    PendingSnapshotInvariantError::ProducerPriorEmptyMissing,
                ),
            ))
            .unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            DocumentControlReceiveOutcome::CommandOutcome(DocumentControlOutcome::Rejected(
                DocumentControlError::PendingSnapshot(
                    PendingSnapshotInvariantError::ProducerPriorEmptyMissing
                )
            ))
        ));
        assert_eq!(cancellations.load(Ordering::SeqCst), 0);

        let (response, receiver) =
            cancellable_receiver(&DocumentControlCommand::Observe, &cancellations);
        response
            .send(completed_outcome(
                DocumentControlAction::TurnProcessed {
                    microtask_checkpoint_advanced: true,
                },
                eligible_pending(),
            ))
            .unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            DocumentControlReceiveOutcome::ObserveTransportFailure(
                DocumentControlTransportFailure::InvalidOutcome(error)
            ) if matches!(
                *error,
                DocumentControlOutcomeInvariantError::CompletedActionMismatch {
                    command: DocumentControlCommandKind::Observe,
                    ..
                }
            )
        ));
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);

        // Once a Drive or Advance handler may have acted, an invalid capture cannot be
        // represented as a definitive rejection. The receiver preserves that uncertainty.
        let (response, receiver) =
            cancellable_receiver(&DocumentControlCommand::DriveOneTurn, &cancellations);
        let capture_rejection = DocumentControlOutcome::Rejected(
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Network),
        );
        assert!(matches!(
            capture_rejection.validate_for_command(&DocumentControlCommand::DriveOneTurn),
            Err(
                DocumentControlOutcomeInvariantError::PendingCaptureRejectionForMutatingCommand {
                    command: DocumentControlCommandKind::DriveOneTurn,
                    ..
                }
            )
        ));
        response.send(capture_rejection).unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(
                DocumentControlTransportFailure::InvalidOutcome(error)
            ) if matches!(
                *error,
                DocumentControlOutcomeInvariantError::PendingCaptureRejectionForMutatingCommand {
                    command: DocumentControlCommandKind::DriveOneTurn,
                    ..
                }
            )
        ));
        assert_eq!(cancellations.load(Ordering::SeqCst), 2);

        let token = token();
        let command = DocumentControlCommand::AdvanceTo(Box::new(token.clone()));
        let (response, receiver) = cancellable_receiver(&command, &cancellations);
        let capture_rejection =
            DocumentControlOutcome::Rejected(DocumentControlError::PendingSnapshot(
                PendingSnapshotInvariantError::ProducerPriorEmptyMissing,
            ));
        assert!(matches!(
            capture_rejection.validate_for_command(&command),
            Err(
                DocumentControlOutcomeInvariantError::PendingCaptureRejectionForMutatingCommand {
                    command: DocumentControlCommandKind::AdvanceTo,
                    ..
                }
            )
        ));
        response.send(capture_rejection).unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            DocumentControlReceiveOutcome::CommandOutcome(
                DocumentControlOutcome::AdvanceOutcomeIndeterminate {
                    token_id,
                    target,
                    deadline,
                }
            ) if token_id == token.id() && *target == *token.target() &&
                deadline == token.deadline()
        ));
        assert_eq!(cancellations.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn disconnected_receiver_does_not_alias_timeout_or_rejection() {
        let (response, receiver) = GenericCallback::new_blocking().unwrap();
        let receiver =
            DocumentControlReceiver::new(receiver, &DocumentControlCommand::DriveOneTurn);
        drop(response);
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(1)),
            DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(
                DocumentControlTransportFailure::Disconnected
            )
        );
    }
}
