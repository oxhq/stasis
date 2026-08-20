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
use timers::{
    DocumentClockError, DocumentClockId, DocumentProducerFenceError, DocumentTime,
    DocumentTimeSurface, TimerControlError, TimerDeadlineSnapshot, TimerSchedulerId,
};

use crate::document_pending::{
    PendingClockMode, PendingExternalIoObservation, PendingExternalIoPhase,
    PendingInputObservation, PendingMicrotaskObservation, PendingParserPhase,
    PendingParserSourceObservation, PendingPipelineRenderingObservation,
    PendingProducerObservation, PendingProducerStability, PendingSnapshotInvariantError,
    PendingTargetObservation, RawPendingSnapshot, RuntimeStateGeneration,
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
        if !pending.terminals.is_empty() {
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
        if deadline.deadline <= pending.clock.now {
            return Err(DocumentAdvanceTokenInvariantError::DeadlineNotInFuture {
                now: pending.clock.now,
                deadline,
            });
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
    let rendering_opportunity_is_future = pending
        .rendering
        .scheduled_opportunity
        .is_some_and(|opportunity| opportunity.deadline > pending.clock.now);
    if rendering_opportunity_is_future {
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
    /// A due timer must be processed as ready work instead of advancing time.
    DeadlineNotInFuture {
        /// Current document time.
        now: DocumentTime,
        /// Scheduler head which is already due.
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
    /// Conditionally advance to and activate the exact deadline bound by a fresh token.
    AdvanceTo(Box<DocumentAdvanceToken>),
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
    /// Exactly one timer callback was activated; resulting page work has not run yet.
    TimerActivated(TimerDeadlineSnapshot),
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
}

impl DocumentControlOutcome {
    /// Revalidate an observation and indeterminate target after same-build deserialization.
    pub fn validate(&self) -> Result<(), DocumentControlOutcomeInvariantError> {
        match self {
            Self::Completed(observation) => observation
                .validate()
                .map_err(DocumentControlOutcomeInvariantError::Observation),
            Self::Rejected(_) => Ok(()),
            Self::DriveOneTurnOutcomeIndeterminate { target }
            | Self::AdvanceOutcomeIndeterminate { target, .. } => target
                .validate()
                .map_err(DocumentControlOutcomeInvariantError::IndeterminateTarget),
        }
    }

    /// Validate that this decoded outcome belongs to the submitted command.
    pub fn validate_for_command(
        &self,
        command: &DocumentControlCommand,
    ) -> Result<(), DocumentControlOutcomeInvariantError> {
        self.validate()?;
        match (command, self) {
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
                DocumentControlCommand::DriveOneTurn,
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
            _ => Err(
                DocumentControlOutcomeInvariantError::OutcomeCommandMismatch {
                    command: DocumentControlCommandKind::from_command(command),
                    outcome: DocumentControlOutcomeKind::from_outcome(self),
                },
            ),
        }
    }
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
    /// Activate one token-bound timer.
    AdvanceTo,
}

impl DocumentControlCommandKind {
    fn from_command(command: &DocumentControlCommand) -> Self {
        match command {
            DocumentControlCommand::Observe => Self::Observe,
            DocumentControlCommand::DriveOneTurn => Self::DriveOneTurn,
            DocumentControlCommand::AdvanceTo(_) => Self::AdvanceTo,
        }
    }
}

/// Decoded outcome class used by command-aware outcome validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DocumentControlOutcomeKind {
    /// A completed observation.
    Completed,
    /// A definitive rejection.
    Rejected,
    /// An indeterminate driven turn.
    DriveOneTurnOutcomeIndeterminate,
    /// An indeterminate guarded advance.
    AdvanceOutcomeIndeterminate,
}

impl DocumentControlOutcomeKind {
    fn from_outcome(outcome: &DocumentControlOutcome) -> Self {
        match outcome {
            DocumentControlOutcome::Completed(_) => Self::Completed,
            DocumentControlOutcome::Rejected(_) => Self::Rejected,
            DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate { .. } => {
                Self::DriveOneTurnOutcomeIndeterminate
            },
            DocumentControlOutcome::AdvanceOutcomeIndeterminate { .. } => {
                Self::AdvanceOutcomeIndeterminate
            },
        }
    }
}

/// A decoded command outcome which is structurally invalid or belongs to another command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocumentControlOutcomeInvariantError {
    /// A completed observation was structurally invalid.
    Observation(DocumentControlObservationInvariantError),
    /// An indeterminate outcome carried a malformed target authority.
    IndeterminateTarget(PendingSnapshotInvariantError),
    /// A completed action did not match the submitted command.
    CompletedActionMismatch {
        /// Submitted command class.
        command: DocumentControlCommandKind,
        /// Action carried by the decoded observation.
        observed: DocumentControlAction,
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
    use servo_base::id::{TEST_PIPELINE_ID, TEST_SCRIPT_EVENT_LOOP_ID, TEST_WEBVIEW_ID};
    use timers::{
        DocumentClock, DocumentClockConfiguration, DocumentProducerCheckpoint,
        DocumentProducerFence, DocumentUnixTime, TimerEventRequest, TimerScheduler,
    };

    use super::*;
    use crate::document_pending::{
        DomEpoch, PendingActiveTopLevelPipeline, PendingAnimatedImageObservation,
        PendingCanvasObservation, PendingExternalIoEvidence, PendingExternalIoLoadBlocking,
        PendingExternalIoOwner, PendingMicrotaskCheckpoint, PendingNavigationRevision,
        PendingNetworkKind, PendingNetworkObservation, PendingParserObservation,
        PendingParserSourceKind, PendingPipelineRenderingObservation, PendingRenderingObservation,
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
            producers,
            parser: PendingParserObservation::default(),
            network: PendingNetworkObservation::default(),
            rendering: rendering(),
            sources: PendingSourceSnapshot::default(),
            terminals: PendingRuntimeTerminals::default(),
        };
        pending.validate().unwrap();
        pending
    }

    fn token() -> DocumentAdvanceToken {
        DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(13), &eligible_pending())
            .unwrap()
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
            DocumentControlError::ChannelClosed,
        ));
        assert_postcard_round_trip(DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate {
            target: Box::new(token.target().clone()),
        });
        assert_postcard_round_trip(DocumentControlOutcome::AdvanceOutcomeIndeterminate {
            token_id: token.id(),
            target: Box::new(token.target().clone()),
            deadline: token.deadline(),
        });
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

    fn completed_outcome(
        action: DocumentControlAction,
        pending: RawPendingSnapshot,
    ) -> DocumentControlOutcome {
        DocumentControlOutcome::Completed(Box::new(
            DocumentControlObservation::new_internal(action, Box::new(pending), None).unwrap(),
        ))
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
        assert_eq!(cancellations.load(Ordering::SeqCst), 3);
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
                DocumentControlError::ChannelClosed,
            ))
            .unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            DocumentControlReceiveOutcome::CommandOutcome(DocumentControlOutcome::Rejected(
                DocumentControlError::ChannelClosed
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
    fn receiver_rejects_invalid_decoded_outcomes_with_command_specific_semantics() {
        let cancellations = Arc::new(AtomicUsize::new(0));

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

        let (response, receiver) =
            cancellable_receiver(&DocumentControlCommand::DriveOneTurn, &cancellations);
        response
            .send(completed_outcome(
                DocumentControlAction::Observed,
                eligible_pending(),
            ))
            .unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(1)),
            DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(
                DocumentControlTransportFailure::InvalidOutcome(error)
            ) if matches!(
                *error,
                DocumentControlOutcomeInvariantError::CompletedActionMismatch {
                    command: DocumentControlCommandKind::DriveOneTurn,
                    ..
                }
            )
        ));
        assert_eq!(cancellations.load(Ordering::SeqCst), 2);

        let token = token();
        let command = DocumentControlCommand::AdvanceTo(Box::new(token.clone()));
        let (response, receiver) = cancellable_receiver(&command, &cancellations);
        response
            .send(completed_outcome(
                DocumentControlAction::Observed,
                eligible_pending(),
            ))
            .unwrap();
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
