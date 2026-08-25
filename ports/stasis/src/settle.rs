/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Pure settlement policy and state transitions.
//!
//! This module never spins Servo, waits on a wall clock, polls a receiver, or serializes product
//! protocol. The shell owner supplies completed document-control outcomes and cumulative time
//! spent waiting for external I/O. In return, the coordinator requests exactly one mechanical
//! command, a lost-wake-safe host wait, or a terminal settlement result.

use std::time::Duration;

use embedder_traits::DocumentClockError;
use embedder_traits::document_control::{
    DocumentAdvanceToken, DocumentAdvanceTokenInvariantError, DocumentControlAction,
    DocumentControlCommand, DocumentControlError, DocumentControlOutcome,
    DocumentControlOutcomeInvariantError, DocumentControlReceiveOutcome,
    DocumentControlTransportFailure,
};
use embedder_traits::document_pending::{
    PendingClockMode, PendingExternalIoObservation, PendingExternalIoPhase,
    PendingLogicalTimerKind, PendingLogicalTimerObservation, PendingOpenEndedSourceReason,
    PendingParserPhase, PendingPipelineRenderingObservation, PendingProducerStability,
    PendingRenderingPipelineActivity, PendingRuntimeTerminals, PendingSourceDisposition,
    PendingSourceKind, PendingSourceObservation, PendingTargetObservation, PendingTaskObservation,
    RawPendingSnapshot,
};
use timers::{
    DocumentClockId, DocumentExecutionBudget, DocumentExecutionCounter, DocumentExecutionTerminal,
    TimerControlError,
};

/// Conservative limits for one settlement request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SettlePolicy {
    /// Maximum document-clock span reachable from the first authoritative observation.
    pub max_virtual_time: Duration,
    /// Maximum number of completed `DriveOneTurn` commands used as a final coordinator guard.
    /// Engine-owned task, microtask, rendering, and mutation budgets terminate first under the
    /// named v0.1 profile; callers may still select a lower explicit control-turn ceiling.
    pub max_control_turns: u64,
    /// Cumulative wall time which may be spent waiting for foreground external I/O.
    pub wall_io_timeout: Duration,
}

impl Default for SettlePolicy {
    fn default() -> Self {
        Self {
            max_virtual_time: Duration::from_secs(30),
            max_control_turns: 1_000_000,
            wall_io_timeout: Duration::from_secs(10),
        }
    }
}

/// The next action for the shell owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettleProgress {
    /// Submit exactly this same-build mechanical command once.
    Command(DocumentControlCommand),
    /// Run the owner turn and wait on the wake-generation predicate without polling.
    Wait(SettleWait),
    /// Settlement reached one typed terminal result.
    Complete(SettleCompletion),
}

/// A host wait requested by the coordinator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettleWait {
    /// Foreground network/parser I/O is active. Virtual time must remain frozen.
    ForegroundExternalIo {
        /// Latest authoritative observation.
        pending: Box<RawPendingSnapshot>,
        /// Canonical active network operations which explain at least part of this wait.
        network: Vec<PendingExternalIoObservation>,
        /// Cumulative wall-I/O budget remaining at this decision.
        remaining_wall_time: Duration,
    },
    /// A producer ticket is live but no foreground external operation identifies it.
    ProducerHandoff {
        /// Latest authoritative observation.
        pending: Box<RawPendingSnapshot>,
        /// Cumulative wall-I/O budget remaining at this decision.
        remaining_wall_time: Duration,
    },
}

/// Persistent work which does not itself prevent quiescence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PersistentWork {
    /// Identity-bearing open-ended source accepted by the 0.1 policy.
    Source(PendingSourceObservation),
    /// Independently authoritative infinite rendering work.
    InfiniteRendering(PendingPipelineRenderingObservation),
}

/// A typed terminal result with an authoritative final observation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettleCompletion {
    /// No relevant finite or persistent work remains.
    Quiescent {
        pending: Box<RawPendingSnapshot>,
        control_turns: u64,
    },
    /// Only explicitly accepted persistent work remains.
    QuiescentWithPersistentWork {
        pending: Box<RawPendingSnapshot>,
        persistent: Vec<PersistentWork>,
        control_turns: u64,
    },
    /// A final Observe after cumulative wall-I/O expiry still found foreground I/O.
    BlockedOnExternalIo {
        pending: Box<RawPendingSnapshot>,
        network: Vec<PendingExternalIoObservation>,
        control_turns: u64,
    },
    /// An accepted open-ended source owns the scheduler head ahead of finite work.
    BlockedOnOpenEndedWork {
        pending: Box<RawPendingSnapshot>,
        persistent: Vec<PersistentWork>,
        control_turns: u64,
    },
    /// The next finite deadline lies outside the allowed virtual span.
    VirtualTimeLimitExceeded {
        pending: Box<RawPendingSnapshot>,
        start_virtual_time_ns: u128,
        requested_virtual_time_ns: u128,
        limit: Duration,
        control_turns: u64,
    },
    /// Another mechanical turn is required but the honest control-turn limit is exhausted.
    ControlTurnLimitExceeded {
        pending: Box<RawPendingSnapshot>,
        limit: u64,
        control_turns: u64,
    },
    /// One engine-owned execution class reached its immutable session budget.
    ExecutionLimitExceeded {
        pending: Box<RawPendingSnapshot>,
        budget: DocumentExecutionBudget,
        limit: u64,
        observed: u64,
        control_turns: u64,
    },
    /// Authoritative runtime evidence failed closed without an indeterminate command outcome.
    RuntimeError {
        pending: Box<RawPendingSnapshot>,
        failure: SettleRuntimeFailure,
        control_turns: u64,
    },
}

impl SettleCompletion {
    /// Return the authoritative terminal observation without weakening the typed completion.
    /// Product profiles use this read-only seam to bind profile-specific opaque evidence before
    /// consuming the completion into a public projection.
    pub fn pending(&self) -> &RawPendingSnapshot {
        match self {
            Self::Quiescent { pending, .. }
            | Self::QuiescentWithPersistentWork { pending, .. }
            | Self::BlockedOnExternalIo { pending, .. }
            | Self::BlockedOnOpenEndedWork { pending, .. }
            | Self::VirtualTimeLimitExceeded { pending, .. }
            | Self::ControlTurnLimitExceeded { pending, .. }
            | Self::ExecutionLimitExceeded { pending, .. }
            | Self::RuntimeError { pending, .. } => pending,
        }
    }
}

/// Snapshot-backed runtime failures which remain valid settlement results.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettleRuntimeFailure {
    RuntimeTerminals(PendingRuntimeTerminals),
    ExecutionCounterOverflow(DocumentExecutionCounter),
    ClockNotControlled(PendingClockMode),
    UnsupportedClockSurface,
    ClockIdentityChanged,
    VirtualTimeRegressed,
    UnsupportedSource(PendingSourceObservation),
    UnsupportedOpenEndedSource(PendingSourceObservation),
    UnsupportedRendering(PendingPipelineRenderingObservation),
    UnsupportedRetainedTasks(PendingTaskObservation),
    IneligibleLogicalTimerHead(PendingLogicalTimerObservation),
    WebViewIdentityChanged,
    InconsistentPendingEvidence(&'static str),
    MissingFiniteSchedulerHead,
    UnclassifiedSchedulerHead,
    MissingAdvanceAuthority,
    MismatchedAdvanceAuthority,
    QuietCheckpointDidNotAdvance,
}

/// Fatal control/transport failure for which no trustworthy final snapshot can be returned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettleFailure {
    InvalidCoordinatorState(&'static str),
    InvalidControlOutcome(Box<DocumentControlOutcomeInvariantError>),
    ControlRejected(Box<DocumentControlError>),
    ObserveTransportFailure(DocumentControlTransportFailure),
    DriveOneTurnOutcomeIndeterminate(DocumentControlTransportFailure),
    DriveOutcomeIndeterminate(Box<DocumentControlOutcome>),
    AdvanceOutcomeIndeterminate(Box<DocumentControlOutcome>),
    ControlTurnCounterOverflow,
    ExternalIoWallTimeRegressed {
        previous: Duration,
        observed: Duration,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CoordinatorPhase {
    New,
    Ready,
    InFlight,
    Waiting,
    Complete,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WaitKind {
    ForegroundExternalIo,
    ProducerHandoff,
}

#[derive(Clone, Debug)]
struct InFlightCommand {
    command: DocumentControlCommand,
    quiet_candidate: Option<Box<RawPendingSnapshot>>,
}

#[derive(Default)]
struct SourceClassification {
    ready: bool,
    has_finite_deadline: bool,
    finite_rendering_opportunity: bool,
    foreground_network: Vec<PendingExternalIoObservation>,
    has_source_only_external_io: bool,
    persistent: Vec<PersistentWork>,
    unsupported: Option<SettleRuntimeFailure>,
}

/// Deterministic settlement state machine for one controlled document surface.
pub struct SettleCoordinator {
    policy: SettlePolicy,
    phase: CoordinatorPhase,
    in_flight: Option<InFlightCommand>,
    waiting: Option<WaitKind>,
    initial_target: Option<PendingTargetObservation>,
    last_target: Option<PendingTargetObservation>,
    initial_clock_id: Option<DocumentClockId>,
    initial_virtual_time_ns: Option<u128>,
    last_virtual_time_ns: Option<u128>,
    control_turns: u64,
    cumulative_external_io_wall_time: Duration,
    final_external_io_observation_required: bool,
    additional_foreground_external_io_active: bool,
}

impl SettleCoordinator {
    pub fn new(policy: SettlePolicy) -> Self {
        Self {
            policy,
            phase: CoordinatorPhase::New,
            in_flight: None,
            waiting: None,
            initial_target: None,
            last_target: None,
            initial_clock_id: None,
            initial_virtual_time_ns: None,
            last_virtual_time_ns: None,
            control_turns: 0,
            cumulative_external_io_wall_time: Duration::ZERO,
            final_external_io_observation_required: false,
            additional_foreground_external_io_active: false,
        }
    }

    /// Refresh embedder-owned finite foreground I/O before the next authoritative decision.
    ///
    /// This is an additional freeze gate only. It deliberately does not increment producer or
    /// network counts in the document snapshot, and an absent gate preserves v1 behavior.
    pub fn set_additional_foreground_external_io_active(&mut self, active: bool) {
        self.additional_foreground_external_io_active = active;
    }

    /// Begin with a non-mutating authoritative observation.
    pub fn start(&mut self) -> Result<SettleProgress, SettleFailure> {
        let result = self.start_inner();
        self.guard_result(result)
    }

    fn start_inner(&mut self) -> Result<SettleProgress, SettleFailure> {
        if self.phase != CoordinatorPhase::New {
            return Err(SettleFailure::InvalidCoordinatorState(
                "settlement has already started",
            ));
        }
        self.phase = CoordinatorPhase::Ready;
        self.issue_command(DocumentControlCommand::Observe, None)
    }

    /// Begin settlement from the exact completed Observe accepted by the shell's token-
    /// authorization bracket.
    ///
    /// The shell already owns this non-mutating outcome, so issuing a second Observe would create
    /// an unnecessary authority gap before the coordinator starts. Arm the ordinary initial
    /// Observe internally and consume the supplied outcome through the same validation,
    /// target/clock initialization, wall-I/O accounting, and decision path as a normally returned
    /// command. No control turn is counted.
    pub fn start_with_observe_outcome(
        &mut self,
        outcome: DocumentControlReceiveOutcome,
        cumulative_external_io_wall_time: Duration,
    ) -> Result<SettleProgress, SettleFailure> {
        let result =
            self.start_with_observe_outcome_inner(outcome, cumulative_external_io_wall_time);
        self.guard_result(result)
    }

    fn start_with_observe_outcome_inner(
        &mut self,
        outcome: DocumentControlReceiveOutcome,
        cumulative_external_io_wall_time: Duration,
    ) -> Result<SettleProgress, SettleFailure> {
        match self.start_inner()? {
            SettleProgress::Command(DocumentControlCommand::Observe) => {},
            _ => {
                return Err(SettleFailure::InvalidCoordinatorState(
                    "initial settlement did not arm its authoritative Observe",
                ));
            },
        }
        self.consume_receive_outcome_inner(outcome, cumulative_external_io_wall_time)
    }

    /// Begin settlement after the session owner has already attested one exact pending
    /// top-level replacement. The bootstrap is lifecycle reconciliation: it must run before an
    /// ordinary Observe can describe the captured target because ScriptThread has not yet
    /// consumed the replacement pipeline's sole eligible queued `SpawnPipeline` event.
    pub fn start_with_replacement_bootstrap(
        &mut self,
        command: DocumentControlCommand,
    ) -> Result<SettleProgress, SettleFailure> {
        let result = self.start_with_replacement_bootstrap_inner(command);
        self.guard_result(result)
    }

    fn start_with_replacement_bootstrap_inner(
        &mut self,
        command: DocumentControlCommand,
    ) -> Result<SettleProgress, SettleFailure> {
        if self.phase != CoordinatorPhase::New {
            return Err(SettleFailure::InvalidCoordinatorState(
                "settlement has already started",
            ));
        }
        if !matches!(
            command,
            DocumentControlCommand::BootstrapReplacementPipeline { .. }
        ) {
            return Err(SettleFailure::InvalidCoordinatorState(
                "replacement settlement requires BootstrapReplacementPipeline",
            ));
        }
        self.phase = CoordinatorPhase::Ready;
        self.issue_command(command, None)
    }

    /// Consume the sole terminal result from the command most recently requested by this state
    /// machine.
    pub fn consume_receive_outcome(
        &mut self,
        outcome: DocumentControlReceiveOutcome,
        cumulative_external_io_wall_time: Duration,
    ) -> Result<SettleProgress, SettleFailure> {
        let result = self.consume_receive_outcome_inner(outcome, cumulative_external_io_wall_time);
        self.guard_result(result)
    }

    /// Consume the exact typed loss boundary produced when an in-flight controlled turn admitted
    /// a top-level document replacement and invalidated its source target before a post-turn
    /// observation could be captured.
    ///
    /// The turn is never retried. It consumes one control-turn budget, discards any quiet
    /// candidate bound to the source document, preserves the original virtual-time and wall-I/O
    /// accounting, and requests only the exact replacement bootstrap supplied by the session
    /// owner. Callers must separately prove that this boundary is permitted by their session
    /// profile and operation phase.
    pub fn consume_drive_one_turn_replacement_boundary(
        &mut self,
        outcome: DocumentControlReceiveOutcome,
        cumulative_external_io_wall_time: Duration,
        bootstrap: DocumentControlCommand,
    ) -> Result<SettleProgress, SettleFailure> {
        let result = self.consume_drive_one_turn_replacement_boundary_inner(
            outcome,
            cumulative_external_io_wall_time,
            bootstrap,
        );
        self.guard_result(result)
    }

    fn consume_drive_one_turn_replacement_boundary_inner(
        &mut self,
        outcome: DocumentControlReceiveOutcome,
        cumulative_external_io_wall_time: Duration,
        bootstrap: DocumentControlCommand,
    ) -> Result<SettleProgress, SettleFailure> {
        self.observe_external_io_wall_time(cumulative_external_io_wall_time)?;
        if self.phase != CoordinatorPhase::InFlight {
            return Err(SettleFailure::InvalidCoordinatorState(
                "no document-control command is in flight",
            ));
        }
        let in_flight = self
            .in_flight
            .take()
            .ok_or(SettleFailure::InvalidCoordinatorState(
                "in-flight phase has no command",
            ))?;
        self.phase = CoordinatorPhase::Ready;

        if in_flight.command != DocumentControlCommand::DriveOneTurn {
            return Err(SettleFailure::InvalidCoordinatorState(
                "document replacement boundary requires an in-flight DriveOneTurn",
            ));
        }
        if !matches!(
            bootstrap,
            DocumentControlCommand::BootstrapReplacementPipeline { .. }
        ) {
            return Err(SettleFailure::InvalidCoordinatorState(
                "document replacement boundary requires BootstrapReplacementPipeline",
            ));
        }

        let runtime_outcome = match outcome {
            DocumentControlReceiveOutcome::CommandOutcome(
                outcome @ DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate { .. },
            ) => outcome,
            DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(failure) => {
                return Err(SettleFailure::DriveOneTurnOutcomeIndeterminate(failure));
            },
            _ => {
                return Err(SettleFailure::InvalidCoordinatorState(
                    "document replacement boundary requires an exact typed drive outcome",
                ));
            },
        };
        runtime_outcome
            .validate_for_command(&in_flight.command)
            .map_err(|error| SettleFailure::InvalidControlOutcome(Box::new(error)))?;
        let DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate { target } = runtime_outcome
        else {
            unreachable!("the exact typed drive outcome was matched above");
        };
        if self.last_target.as_ref() != Some(target.as_ref()) {
            return Err(SettleFailure::InvalidCoordinatorState(
                "document replacement boundary did not bind the last observed target",
            ));
        }

        // Admission ran the source turn even though the resulting target transition prevented a
        // post-turn observation. Count it once, never retry it, and deliberately drop the
        // in-flight quiet candidate before admitting only the owner-attested replacement
        // lifecycle event. An ordinary Observe is invalid until that event reconciles local
        // ScriptThread membership with the already-captured Constellation target.
        self.control_turns = self
            .control_turns
            .checked_add(1)
            .ok_or(SettleFailure::ControlTurnCounterOverflow)?;
        drop(in_flight.quiet_candidate);
        self.issue_command(bootstrap, None)
    }

    /// Replace a coordinator-issued `DriveOneTurn` which the shell has not submitted with the
    /// exact lifecycle bootstrap for an independently observed top-level replacement admission.
    ///
    /// No document turn ran, so this transition does not consume control-turn budget. Any quiet
    /// candidate belongs to the source document and is deliberately discarded. The bootstrap is
    /// installed as the coordinator's sole in-flight command and its eventual outcome must be
    /// consumed through [`Self::consume_receive_outcome`] like every other issued command.
    pub fn replace_unsubmitted_drive_with_replacement_bootstrap(
        &mut self,
        source: &PendingTargetObservation,
        bootstrap: DocumentControlCommand,
    ) -> Result<SettleProgress, SettleFailure> {
        let result =
            self.replace_unsubmitted_drive_with_replacement_bootstrap_inner(source, bootstrap);
        self.guard_result(result)
    }

    fn replace_unsubmitted_drive_with_replacement_bootstrap_inner(
        &mut self,
        source: &PendingTargetObservation,
        bootstrap: DocumentControlCommand,
    ) -> Result<SettleProgress, SettleFailure> {
        if self.phase != CoordinatorPhase::InFlight {
            return Err(SettleFailure::InvalidCoordinatorState(
                "no unsubmitted document-control command is in flight",
            ));
        }
        let in_flight = self
            .in_flight
            .as_ref()
            .ok_or(SettleFailure::InvalidCoordinatorState(
                "in-flight phase has no command",
            ))?;
        if in_flight.command != DocumentControlCommand::DriveOneTurn {
            return Err(SettleFailure::InvalidCoordinatorState(
                "replacement rearm requires an unsubmitted DriveOneTurn",
            ));
        }
        if self.last_target.as_ref() != Some(source) {
            return Err(SettleFailure::InvalidCoordinatorState(
                "replacement rearm did not bind the last observed target",
            ));
        }
        if !matches!(
            bootstrap,
            DocumentControlCommand::BootstrapReplacementPipeline { .. }
        ) {
            return Err(SettleFailure::InvalidCoordinatorState(
                "replacement rearm requires BootstrapReplacementPipeline",
            ));
        }

        let in_flight = self
            .in_flight
            .take()
            .expect("the in-flight command was checked above");
        drop(in_flight.quiet_candidate);
        self.phase = CoordinatorPhase::Ready;
        self.issue_command(bootstrap, None)
    }

    fn consume_receive_outcome_inner(
        &mut self,
        outcome: DocumentControlReceiveOutcome,
        cumulative_external_io_wall_time: Duration,
    ) -> Result<SettleProgress, SettleFailure> {
        self.observe_external_io_wall_time(cumulative_external_io_wall_time)?;
        if self.phase != CoordinatorPhase::InFlight {
            return Err(SettleFailure::InvalidCoordinatorState(
                "no document-control command is in flight",
            ));
        }
        let in_flight = self
            .in_flight
            .take()
            .ok_or(SettleFailure::InvalidCoordinatorState(
                "in-flight phase has no command",
            ))?;
        self.phase = CoordinatorPhase::Ready;

        match outcome {
            DocumentControlReceiveOutcome::ObserveTransportFailure(failure) => {
                Err(SettleFailure::ObserveTransportFailure(failure))
            },
            DocumentControlReceiveOutcome::AutomationTransportFailure(_) => {
                Err(SettleFailure::InvalidCoordinatorState(
                    "settlement received an automation transport outcome",
                ))
            },
            DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(failure) => {
                Err(SettleFailure::DriveOneTurnOutcomeIndeterminate(failure))
            },
            DocumentControlReceiveOutcome::CommandOutcome(outcome) => {
                outcome
                    .validate_for_command(&in_flight.command)
                    .map_err(|error| SettleFailure::InvalidControlOutcome(Box::new(error)))?;
                match outcome {
                    DocumentControlOutcome::Completed(observation) => {
                        let action = observation.action();
                        if matches!(
                            action,
                            DocumentControlAction::CheckpointTurnProcessed { .. }
                                | DocumentControlAction::TurnProcessed { .. }
                        ) {
                            self.control_turns = self
                                .control_turns
                                .checked_add(1)
                                .ok_or(SettleFailure::ControlTurnCounterOverflow)?;
                        }
                        let pending = Box::new(observation.pending().clone());
                        let token = observation.advance_token().cloned();
                        self.decide(pending, token, action, in_flight.quiet_candidate)
                    },
                    DocumentControlOutcome::Rejected(error)
                        if in_flight.command != DocumentControlCommand::Observe
                            && should_reobserve(&error) =>
                    {
                        self.issue_command(DocumentControlCommand::Observe, None)
                    },
                    DocumentControlOutcome::Rejected(error) => {
                        Err(SettleFailure::ControlRejected(Box::new(error)))
                    },
                    outcome @ DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate {
                        ..
                    } => Err(SettleFailure::DriveOutcomeIndeterminate(Box::new(outcome))),
                    outcome @ DocumentControlOutcome::AdvanceOutcomeIndeterminate { .. } => Err(
                        SettleFailure::AdvanceOutcomeIndeterminate(Box::new(outcome)),
                    ),
                    DocumentControlOutcome::AutomationCompleted { .. }
                    | DocumentControlOutcome::AutomationOutcomeIndeterminate { .. } => {
                        Err(SettleFailure::InvalidCoordinatorState(
                            "settlement received an automation command outcome",
                        ))
                    },
                }
            },
        }
    }

    /// Resume after a generation-observed host wake. The wake itself is not state evidence, so a
    /// fresh Observe is mandatory.
    pub fn resume_after_wake(
        &mut self,
        cumulative_external_io_wall_time: Duration,
    ) -> Result<SettleProgress, SettleFailure> {
        let result = self.resume_after_wake_inner(cumulative_external_io_wall_time);
        self.guard_result(result)
    }

    fn resume_after_wake_inner(
        &mut self,
        cumulative_external_io_wall_time: Duration,
    ) -> Result<SettleProgress, SettleFailure> {
        self.observe_external_io_wall_time(cumulative_external_io_wall_time)?;
        if self.phase != CoordinatorPhase::Waiting || self.waiting.take().is_none() {
            return Err(SettleFailure::InvalidCoordinatorState(
                "settlement is not waiting for a wake",
            ));
        }
        self.phase = CoordinatorPhase::Ready;
        self.issue_command(DocumentControlCommand::Observe, None)
    }

    /// Mark the cumulative external-I/O/producer-handoff wall budget expired and require one
    /// final Observe before returning `BlockedOnExternalIo`.
    pub fn external_io_wait_expired(
        &mut self,
        cumulative_external_io_wall_time: Duration,
    ) -> Result<SettleProgress, SettleFailure> {
        let result = self.external_io_wait_expired_inner(cumulative_external_io_wall_time);
        self.guard_result(result)
    }

    fn external_io_wait_expired_inner(
        &mut self,
        cumulative_external_io_wall_time: Duration,
    ) -> Result<SettleProgress, SettleFailure> {
        self.observe_external_io_wall_time(cumulative_external_io_wall_time)?;
        if self.phase != CoordinatorPhase::Waiting || self.waiting.is_none() {
            return Err(SettleFailure::InvalidCoordinatorState(
                "external I/O or a producer handoff is not awaiting a wake",
            ));
        }
        if self.cumulative_external_io_wall_time < self.policy.wall_io_timeout {
            return Err(SettleFailure::InvalidCoordinatorState(
                "foreground external-I/O wall budget has not expired",
            ));
        }
        self.waiting = None;
        self.phase = CoordinatorPhase::Ready;
        self.final_external_io_observation_required = true;
        self.issue_command(DocumentControlCommand::Observe, None)
    }

    fn decide(
        &mut self,
        pending: Box<RawPendingSnapshot>,
        advance_token: Option<DocumentAdvanceToken>,
        action: DocumentControlAction,
        quiet_candidate: Option<Box<RawPendingSnapshot>>,
    ) -> Result<SettleProgress, SettleFailure> {
        let final_external_io_observation = self.final_external_io_observation_required;
        if final_external_io_observation && action != DocumentControlAction::Observed {
            return Err(SettleFailure::InvalidCoordinatorState(
                "final external-I/O evidence did not come from Observe",
            ));
        }
        self.final_external_io_observation_required = false;

        match &self.initial_target {
            None => self.initial_target = Some(pending.target.clone()),
            Some(initial) if initial.webview_id != pending.target.webview_id => {
                return self
                    .complete_runtime(pending, SettleRuntimeFailure::WebViewIdentityChanged);
            },
            Some(_) => {},
        }
        self.last_target = Some(pending.target.clone());

        if let Some(terminal) = pending.execution.and_then(|execution| execution.terminal) {
            return match terminal {
                DocumentExecutionTerminal::BudgetExceeded {
                    budget,
                    limit,
                    observed,
                } => self.complete(SettleCompletion::ExecutionLimitExceeded {
                    pending,
                    budget,
                    limit,
                    observed,
                    control_turns: self.control_turns,
                }),
                DocumentExecutionTerminal::CounterOverflow(counter) => self.complete_runtime(
                    pending,
                    SettleRuntimeFailure::ExecutionCounterOverflow(counter),
                ),
            };
        }

        if !pending.terminals.is_empty() {
            if only_target_time_terminal(&pending.terminals)
                && pending.target.unsupported_time_surface.is_some()
            {
                return self
                    .complete_runtime(pending, SettleRuntimeFailure::UnsupportedClockSurface);
            }
            return self.complete_runtime(
                pending.clone(),
                SettleRuntimeFailure::RuntimeTerminals(pending.terminals.clone()),
            );
        }
        if pending.clock.unsupported_surface.is_some()
            || pending.target.unsupported_time_surface.is_some()
        {
            return self.complete_runtime(pending, SettleRuntimeFailure::UnsupportedClockSurface);
        }
        if pending.clock.mode != PendingClockMode::Controlled {
            let mode = pending.clock.mode;
            return self.complete_runtime(pending, SettleRuntimeFailure::ClockNotControlled(mode));
        }
        let clock_id = pending.clock.clock_id;
        match self.initial_clock_id {
            None => self.initial_clock_id = Some(clock_id),
            Some(initial) if initial != clock_id => {
                return self.complete_runtime(pending, SettleRuntimeFailure::ClockIdentityChanged);
            },
            Some(_) => {},
        }
        let now_ns = pending.clock.now.as_nanos();
        let start_ns = *self.initial_virtual_time_ns.get_or_insert(now_ns);
        if self
            .last_virtual_time_ns
            .replace(now_ns)
            .is_some_and(|previous| now_ns < previous)
        {
            return self.complete_runtime(pending, SettleRuntimeFailure::VirtualTimeRegressed);
        }
        if now_ns < start_ns {
            return self.complete_runtime(pending, SettleRuntimeFailure::VirtualTimeRegressed);
        }
        if now_ns - start_ns > self.policy.max_virtual_time.as_nanos() {
            return self.complete(SettleCompletion::VirtualTimeLimitExceeded {
                pending,
                start_virtual_time_ns: start_ns,
                requested_virtual_time_ns: now_ns,
                limit: self.policy.max_virtual_time,
                control_turns: self.control_turns,
            });
        }

        let classification = classify_sources(&pending);

        // Ready work always runs before external waiting or virtual advancement.
        if pending.input.intake_saturated
            || pending.input.ready_events != 0
            || pending.input.tasks.ready != 0
            || pending
                .logical_timers
                .timers()
                .iter()
                .any(|timer| timer.delivery_ready)
            || pending.microtasks.queued != 0
            || pending.microtasks.checkpoint_in_progress
            || pending.parser.sources().iter().any(|source| {
                matches!(
                    source.phase,
                    PendingParserPhase::Ready | PendingParserPhase::AwaitingCommit
                )
            })
            || pending
                .network
                .active()
                .iter()
                .any(|operation| operation.phase == PendingExternalIoPhase::TerminalTaskQueued)
            || classification.ready
            || rendering_ready(&pending)
        {
            return self.drive(pending, None);
        }

        // Real external I/O freezes virtual time. A wall expiry never trusts this observation: it
        // requests one final Observe and only that response may become BlockedOnExternalIo.
        if self.additional_foreground_external_io_active
            || !classification.foreground_network.is_empty()
            || classification.has_source_only_external_io
        {
            if final_external_io_observation {
                return self.complete(SettleCompletion::BlockedOnExternalIo {
                    pending,
                    network: classification.foreground_network,
                    control_turns: self.control_turns,
                });
            }
            if self.cumulative_external_io_wall_time >= self.policy.wall_io_timeout {
                self.final_external_io_observation_required = true;
                return self.issue_command(DocumentControlCommand::Observe, None);
            }
            let remaining_wall_time = self
                .policy
                .wall_io_timeout
                .saturating_sub(self.cumulative_external_io_wall_time);
            return self.wait(SettleWait::ForegroundExternalIo {
                pending,
                network: classification.foreground_network,
                remaining_wall_time,
            });
        }

        // Producer handoffs precede unsupported/source/rendering decisions because a live producer
        // can atomically replace itself with newly classified terminal work.
        if pending.producers.snapshot.pending() != 0 {
            if final_external_io_observation {
                return self.complete(SettleCompletion::BlockedOnExternalIo {
                    pending,
                    network: classification.foreground_network,
                    control_turns: self.control_turns,
                });
            }
            if self.cumulative_external_io_wall_time >= self.policy.wall_io_timeout {
                self.final_external_io_observation_required = true;
                return self.issue_command(DocumentControlCommand::Observe, None);
            }
            let remaining_wall_time = self
                .policy
                .wall_io_timeout
                .saturating_sub(self.cumulative_external_io_wall_time);
            return self.wait(SettleWait::ProducerHandoff {
                pending,
                remaining_wall_time,
            });
        }
        if pending.producers.stability != PendingProducerStability::StableEmpty {
            return self.drive(pending, None);
        }

        if pending.input.tasks.throttled != 0 || pending.input.tasks.inactive != 0 {
            let tasks = pending.input.tasks;
            return self.complete_runtime(
                pending,
                SettleRuntimeFailure::UnsupportedRetainedTasks(tasks),
            );
        }

        if let Some(failure) = classification.unsupported {
            return self.complete_runtime(pending, failure);
        }
        if let Some(failure) = unsupported_rendering(&pending) {
            return self.complete_runtime(pending, failure);
        }

        if rendering_needs_unscheduled_turn(&pending, classification.finite_rendering_opportunity) {
            return self.drive(pending, None);
        }

        let finite = finite_work(&pending, &classification);
        if finite.exists {
            let Some(head) = pending.scheduler.next_deadline else {
                return self
                    .complete_runtime(pending, SettleRuntimeFailure::MissingFiniteSchedulerHead);
            };
            let logical_head = logical_timer_owning_head(&pending, head);
            if let Some(timer) = logical_head {
                if !timer.eligible_in_controlled_turn {
                    return self.complete_runtime(
                        pending,
                        SettleRuntimeFailure::IneligibleLogicalTimerHead(timer),
                    );
                }
                if matches!(
                    timer.kind,
                    PendingLogicalTimerKind::JavaScriptInterval { .. }
                ) {
                    return self.complete(SettleCompletion::BlockedOnOpenEndedWork {
                        pending,
                        persistent: classification.persistent,
                        control_turns: self.control_turns,
                    });
                }
            }
            if finite.persistent_rendering_owns_head {
                return self.complete(SettleCompletion::BlockedOnOpenEndedWork {
                    pending,
                    persistent: classification.persistent,
                    control_turns: self.control_turns,
                });
            }
            let exact_logical_timer_head = logical_head.is_some_and(|timer| {
                !matches!(
                    timer.kind,
                    PendingLogicalTimerKind::JavaScriptInterval { .. }
                        | PendingLogicalTimerKind::EventSourceReconnect
                )
            });
            if !exact_logical_timer_head && !finite.exact_rendering_head {
                return self
                    .complete_runtime(pending, SettleRuntimeFailure::UnclassifiedSchedulerHead);
            }
            if head.deadline < pending.clock.now {
                return self.complete_runtime(
                    pending,
                    SettleRuntimeFailure::InconsistentPendingEvidence(
                        "the finite scheduler head precedes controlled time",
                    ),
                );
            }
            if head.deadline.as_nanos() - start_ns > self.policy.max_virtual_time.as_nanos() {
                return self.complete(SettleCompletion::VirtualTimeLimitExceeded {
                    pending,
                    start_virtual_time_ns: start_ns,
                    requested_virtual_time_ns: head.deadline.as_nanos(),
                    limit: self.policy.max_virtual_time,
                    control_turns: self.control_turns,
                });
            }
            let Some(token) = advance_token else {
                return self
                    .complete_runtime(pending, SettleRuntimeFailure::MissingAdvanceAuthority);
            };
            if token.deadline() != head || token.validate_against(&pending).is_err() {
                return self
                    .complete_runtime(pending, SettleRuntimeFailure::MismatchedAdvanceAuthority);
            }
            return self.issue_command(DocumentControlCommand::AdvanceTo(Box::new(token)), None);
        }

        if let Some(head) = pending.scheduler.next_deadline {
            let interval_owns_head =
                logical_timer_owning_head(&pending, head).is_some_and(|timer| {
                    matches!(
                        timer.kind,
                        PendingLogicalTimerKind::JavaScriptInterval { .. }
                    )
                });
            if !interval_owns_head && !persistent_rendering_owns_head(&pending, head) {
                return self
                    .complete_runtime(pending, SettleRuntimeFailure::UnclassifiedSchedulerHead);
            }
        }

        // Two fresh, identical mechanically qualified checkpoints are required across the whole
        // pending surface, not only the producer fence.
        if let Some(candidate) = quiet_candidate {
            match action {
                DocumentControlAction::CheckpointTurnProcessed {
                    microtask_checkpoint_advanced: true,
                } if quiet_snapshots_match(&candidate, &pending) => {
                    if classification.persistent.is_empty() {
                        return self.complete(SettleCompletion::Quiescent {
                            pending,
                            control_turns: self.control_turns,
                        });
                    }
                    return self.complete(SettleCompletion::QuiescentWithPersistentWork {
                        pending,
                        persistent: classification.persistent,
                        control_turns: self.control_turns,
                    });
                },
                DocumentControlAction::CheckpointTurnProcessed {
                    microtask_checkpoint_advanced: false,
                } => {
                    return self.complete_runtime(
                        pending,
                        SettleRuntimeFailure::QuietCheckpointDidNotAdvance,
                    );
                },
                _ => {},
            }
        }
        self.drive(pending.clone(), Some(pending))
    }

    fn drive(
        &mut self,
        pending: Box<RawPendingSnapshot>,
        quiet_candidate: Option<Box<RawPendingSnapshot>>,
    ) -> Result<SettleProgress, SettleFailure> {
        if self.control_turns >= self.policy.max_control_turns {
            return self.complete(SettleCompletion::ControlTurnLimitExceeded {
                pending,
                limit: self.policy.max_control_turns,
                control_turns: self.control_turns,
            });
        }
        self.issue_command(DocumentControlCommand::DriveOneTurn, quiet_candidate)
    }

    fn issue_command(
        &mut self,
        command: DocumentControlCommand,
        quiet_candidate: Option<Box<RawPendingSnapshot>>,
    ) -> Result<SettleProgress, SettleFailure> {
        if self.phase != CoordinatorPhase::Ready {
            return Err(SettleFailure::InvalidCoordinatorState(
                "cannot issue a command from the current phase",
            ));
        }
        self.waiting = None;
        self.in_flight = Some(InFlightCommand {
            command: command.clone(),
            quiet_candidate,
        });
        self.phase = CoordinatorPhase::InFlight;
        Ok(SettleProgress::Command(command))
    }

    fn wait(&mut self, wait: SettleWait) -> Result<SettleProgress, SettleFailure> {
        if self.phase != CoordinatorPhase::Ready {
            return Err(SettleFailure::InvalidCoordinatorState(
                "cannot wait from the current phase",
            ));
        }
        self.waiting = Some(match wait {
            SettleWait::ForegroundExternalIo { .. } => WaitKind::ForegroundExternalIo,
            SettleWait::ProducerHandoff { .. } => WaitKind::ProducerHandoff,
        });
        self.phase = CoordinatorPhase::Waiting;
        Ok(SettleProgress::Wait(wait))
    }

    fn complete_runtime(
        &mut self,
        pending: Box<RawPendingSnapshot>,
        failure: SettleRuntimeFailure,
    ) -> Result<SettleProgress, SettleFailure> {
        self.complete(SettleCompletion::RuntimeError {
            pending,
            failure,
            control_turns: self.control_turns,
        })
    }

    fn complete(&mut self, completion: SettleCompletion) -> Result<SettleProgress, SettleFailure> {
        self.phase = CoordinatorPhase::Complete;
        self.in_flight = None;
        self.waiting = None;
        Ok(SettleProgress::Complete(completion))
    }

    fn observe_external_io_wall_time(&mut self, observed: Duration) -> Result<(), SettleFailure> {
        if observed < self.cumulative_external_io_wall_time {
            return Err(SettleFailure::ExternalIoWallTimeRegressed {
                previous: self.cumulative_external_io_wall_time,
                observed,
            });
        }
        self.cumulative_external_io_wall_time = observed;
        Ok(())
    }

    fn guard_result(
        &mut self,
        result: Result<SettleProgress, SettleFailure>,
    ) -> Result<SettleProgress, SettleFailure> {
        if result.is_err() {
            self.phase = CoordinatorPhase::Failed;
            self.in_flight = None;
            self.waiting = None;
        }
        result
    }
}

fn only_target_time_terminal(terminals: &PendingRuntimeTerminals) -> bool {
    terminals.target_time.is_some()
        && terminals.clock.is_none()
        && terminals.outer_scheduler.is_none()
        && terminals.producer.is_none()
        && terminals.microtask.is_none()
        && terminals.input_revision.is_none()
        && terminals.source_id.is_none()
        && terminals.logical_timers().is_empty()
        && terminals.image_timers().is_empty()
        && terminals.dom_generation.is_none()
        && terminals.state_generation.is_none()
        && terminals.navigation_revision.is_none()
        && terminals.pipeline_membership_revision.is_none()
        && terminals.source_epoch.is_none()
}

fn classify_sources(pending: &RawPendingSnapshot) -> SourceClassification {
    let mut classification = SourceClassification::default();
    for operation in pending.network.active() {
        if operation.phase == PendingExternalIoPhase::TerminalTaskQueued {
            classification.ready = true;
        } else {
            classification.foreground_network.push(*operation);
        }
    }
    for source in pending.sources.sources() {
        match source.disposition {
            PendingSourceDisposition::Inert => {},
            PendingSourceDisposition::Ready => classification.ready = true,
            PendingSourceDisposition::FiniteDeadline(_) => {
                classification.has_finite_deadline = true;
            },
            PendingSourceDisposition::FiniteRenderingOpportunity => {
                classification.finite_rendering_opportunity = true;
            },
            PendingSourceDisposition::AwaitingExternalIo(_)
                if source.kind != PendingSourceKind::Network =>
            {
                classification.has_source_only_external_io = true;
            },
            PendingSourceDisposition::AwaitingExternalIo(_) => {},
            PendingSourceDisposition::OpenEnded(reason) => match reason {
                PendingOpenEndedSourceReason::Interval { .. } => {
                    classification
                        .persistent
                        .push(PersistentWork::Source(*source));
                },
                PendingOpenEndedSourceReason::InfiniteAnimation => classification
                    .persistent
                    .push(PersistentWork::Source(*source)),
                _ => {
                    classification.unsupported.get_or_insert_with(|| {
                        SettleRuntimeFailure::UnsupportedOpenEndedSource(*source)
                    });
                },
            },
            PendingSourceDisposition::Unsupported(reason) => {
                let _ = reason;
                classification
                    .unsupported
                    .get_or_insert_with(|| SettleRuntimeFailure::UnsupportedSource(*source));
            },
        }
    }
    for rendering in pending.rendering.pipelines() {
        if rendering.activity != PendingRenderingPipelineActivity::FullyActive {
            continue;
        }
        if rendering.infinite_animations != 0 || rendering.animated_images.infinite_images != 0 {
            classification
                .persistent
                .push(PersistentWork::InfiniteRendering(*rendering));
        }
    }
    classification
}

fn rendering_ready(pending: &RawPendingSnapshot) -> bool {
    // A retained scheduler entry is not ready work in Controlled mode, even when its deadline is
    // exactly `now`: controlled schedulers activate only through guarded `AdvanceTo`. Treating a
    // due retained entry as Drive-ready spins checkpoint turns forever because Drive cannot detach
    // that timer. Once activation dispatches its callback, capture publishes opportunity_ready and
    // no live scheduled_opportunity.
    let rendering_opportunity_is_unscheduled = pending.rendering.scheduled_opportunity.is_none();
    pending.rendering.opportunity_ready
        || pending.rendering.pipelines().iter().any(|rendering| {
            rendering.activity == PendingRenderingPipelineActivity::FullyActive
                && (rendering.pending_animation_events != 0
                    || rendering.animated_images.update_ready
                    || (rendering_opportunity_is_unscheduled
                        && (rendering.runnable_animation_frame_callbacks != 0
                            || rendering.document_update_required
                            || rendering.canvas.dirty_contexts != 0)))
        })
}

fn has_finite_rendering_demand(rendering: &PendingPipelineRenderingObservation) -> bool {
    rendering.activity == PendingRenderingPipelineActivity::FullyActive
        && (rendering.runnable_animation_frame_callbacks != 0
            || rendering.document_update_required
            || rendering.finite_animations != 0
            || rendering.canvas.dirty_contexts != 0)
}

fn unsupported_rendering(pending: &RawPendingSnapshot) -> Option<SettleRuntimeFailure> {
    pending.rendering.pipelines().iter().find_map(|rendering| {
        let unsupported_images = rendering.animated_images.unsupported.checked_total() != Some(0);
        let unsupported_canvas = rendering
            .canvas
            .unsupported
            .live_source_inventory_unavailable
            != 0
            || rendering.canvas.unsupported.offscreen_execution != 0
            || rendering.canvas.unsupported.mutation_generation_unbound != 0;
        let inactive_work = rendering.activity != PendingRenderingPipelineActivity::FullyActive
            && (rendering.runnable_animation_frame_callbacks != 0
                || rendering.document_update_required
                || rendering.pending_animation_events != 0
                || rendering.finite_animations != 0
                || rendering.infinite_animations != 0
                || rendering.animated_images.finite_images != 0
                || rendering.animated_images.infinite_images != 0
                || rendering.animated_images.update_ready
                || rendering.animated_images.scheduled_timer.is_some()
                || rendering.canvas.dirty_contexts != 0
                || rendering.pending_fonts != 0
                || rendering.pending_images != 0);
        if rendering.unsupported_animations != 0
            || unsupported_images
            || unsupported_canvas
            || inactive_work
            || rendering.canvas.awaiting_async_upload
            || rendering.pending_fonts != 0
            || rendering.pending_images != 0
        {
            Some(SettleRuntimeFailure::UnsupportedRendering(*rendering))
        } else {
            None
        }
    })
}

fn rendering_needs_unscheduled_turn(
    pending: &RawPendingSnapshot,
    finite_rendering_source: bool,
) -> bool {
    if finite_rendering_source && pending.rendering.scheduled_opportunity.is_none() {
        return true;
    }
    pending.rendering.pipelines().iter().any(|rendering| {
        rendering.activity == PendingRenderingPipelineActivity::FullyActive
            && ((rendering.finite_animations != 0
                && pending.rendering.scheduled_opportunity.is_none())
                || (rendering.animated_images.finite_images != 0
                    && rendering.animated_images.scheduled_timer.is_none()))
    })
}

fn logical_timer_owning_head(
    pending: &RawPendingSnapshot,
    head: timers::TimerDeadlineSnapshot,
) -> Option<PendingLogicalTimerObservation> {
    pending
        .logical_timers
        .timers()
        .iter()
        .copied()
        .find(|timer| timer.outer_wake == Some(head))
}

fn persistent_rendering_owns_head(
    pending: &RawPendingSnapshot,
    head: timers::TimerDeadlineSnapshot,
) -> bool {
    let scheduled_finite_rendering = pending.rendering.scheduled_opportunity.is_some()
        && pending
            .rendering
            .pipelines()
            .iter()
            .any(has_finite_rendering_demand);
    persistent_rendering_owns_head_with_finite(pending, head, scheduled_finite_rendering)
}

fn persistent_rendering_owns_head_with_finite(
    pending: &RawPendingSnapshot,
    head: timers::TimerDeadlineSnapshot,
    scheduled_finite_rendering: bool,
) -> bool {
    (pending.rendering.scheduled_opportunity == Some(head)
        && pending.rendering.pipelines().iter().any(|rendering| {
            rendering.activity == PendingRenderingPipelineActivity::FullyActive
                && rendering.infinite_animations != 0
        })
        && !scheduled_finite_rendering)
        || pending.rendering.pipelines().iter().any(|rendering| {
            rendering.activity == PendingRenderingPipelineActivity::FullyActive
                && rendering.animated_images.scheduled_timer == Some(head)
                && rendering.animated_images.infinite_images != 0
                && rendering.animated_images.finite_images == 0
        })
}

struct FiniteWork {
    exists: bool,
    exact_rendering_head: bool,
    persistent_rendering_owns_head: bool,
}

fn finite_work(pending: &RawPendingSnapshot, classification: &SourceClassification) -> FiniteWork {
    let head = pending.scheduler.next_deadline;
    let has_scheduled_finite_rendering = pending.rendering.scheduled_opportunity.is_some()
        && (classification.finite_rendering_opportunity
            || pending
                .rendering
                .pipelines()
                .iter()
                .any(has_finite_rendering_demand));
    let exact_rendering_deadline = head.is_some_and(|head| {
        (has_scheduled_finite_rendering && pending.rendering.scheduled_opportunity == Some(head))
            || pending.rendering.pipelines().iter().any(|rendering| {
                rendering.activity == PendingRenderingPipelineActivity::FullyActive
                    && rendering.animated_images.finite_images != 0
                    && rendering.animated_images.scheduled_timer == Some(head)
            })
    });
    let has_finite_rendering = has_scheduled_finite_rendering
        || pending.rendering.pipelines().iter().any(|rendering| {
            rendering.activity == PendingRenderingPipelineActivity::FullyActive
                && rendering.animated_images.finite_images != 0
                && rendering.animated_images.scheduled_timer.is_some()
        });
    let persistent_rendering_owns_head = head.is_some_and(|head| {
        persistent_rendering_owns_head_with_finite(pending, head, has_scheduled_finite_rendering)
    });
    FiniteWork {
        exists: classification.has_finite_deadline || has_finite_rendering,
        exact_rendering_head: exact_rendering_deadline,
        persistent_rendering_owns_head,
    }
}

fn quiet_snapshots_match(first: &RawPendingSnapshot, second: &RawPendingSnapshot) -> bool {
    second.state_generation >= first.state_generation
        && second.microtasks.completed_checkpoint > first.microtasks.completed_checkpoint
        && first.target == second.target
        && first.dom_epoch == second.dom_epoch
        && first.clock == second.clock
        && first.scheduler == second.scheduler
        && first.input == second.input
        && first.microtasks.event_loop_id == second.microtasks.event_loop_id
        && first.microtasks.queued == second.microtasks.queued
        && first.microtasks.checkpoint_in_progress == second.microtasks.checkpoint_in_progress
        && first.microtasks.terminal == second.microtasks.terminal
        && first.producers.fence_id == second.producers.fence_id
        && first.producers.snapshot == second.producers.snapshot
        && first.parser == second.parser
        && first.network == second.network
        && first.logical_timers == second.logical_timers
        && first.rendering == second.rendering
        && first.sources == second.sources
        && first.terminals == second.terminals
}

fn should_reobserve(error: &DocumentControlError) -> bool {
    match error {
        DocumentControlError::EventLoopUnavailable => true,
        DocumentControlError::AdvancePrecondition(error) => matches!(
            error,
            DocumentAdvanceTokenInvariantError::RuntimeTerminal
                | DocumentAdvanceTokenInvariantError::UnsupportedClockSurface(_)
                | DocumentAdvanceTokenInvariantError::AuthoritativeReadyWork(_)
                | DocumentAdvanceTokenInvariantError::NoFiniteDeadline
                | DocumentAdvanceTokenInvariantError::DeadlineBeforeCurrentTime { .. }
                | DocumentAdvanceTokenInvariantError::InputIntakeSaturated { .. }
                | DocumentAdvanceTokenInvariantError::ReadyInput(_)
                | DocumentAdvanceTokenInvariantError::MicrotasksNotDrained(_)
                | DocumentAdvanceTokenInvariantError::ProducerNotStableEmpty(_)
                | DocumentAdvanceTokenInvariantError::TargetChanged { .. }
                | DocumentAdvanceTokenInvariantError::StateGenerationChanged { .. }
                | DocumentAdvanceTokenInvariantError::ClockChanged { .. }
                | DocumentAdvanceTokenInvariantError::SchedulerChanged { .. }
                | DocumentAdvanceTokenInvariantError::DeadlineChanged { .. }
                | DocumentAdvanceTokenInvariantError::InputChanged { .. }
                | DocumentAdvanceTokenInvariantError::MicrotasksChanged { .. }
                | DocumentAdvanceTokenInvariantError::ProducersChanged { .. }
        ),
        DocumentControlError::UnsupportedSurface(_)
        | DocumentControlError::Clock(
            DocumentClockError::TimeChanged { .. } | DocumentClockError::UnsupportedSurface(_),
        ) => true,
        DocumentControlError::AdvanceTokenUnavailable { .. }
        | DocumentControlError::StaleAdvanceToken { .. } => true,
        DocumentControlError::Timer(
            TimerControlError::StaleDeadline { .. } | TimerControlError::TimerNotDue { .. },
        ) => true,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use embedder_traits::document_control::{DocumentAdvanceTokenId, DocumentControlObservation};
    use embedder_traits::document_pending::{
        DomEpoch, PendingActiveTopLevelPipeline, PendingAnimatedImageObservation,
        PendingCanvasObservation, PendingClockObservation, PendingExternalIoEvidence,
        PendingExternalIoLoadBlocking, PendingExternalIoOwner, PendingInputObservation,
        PendingLogicalTimerObservation, PendingLogicalTimerSnapshot, PendingLogicalTimerStableId,
        PendingMicrotaskCheckpoint, PendingMicrotaskObservation, PendingNavigationRevision,
        PendingNetworkKind, PendingNetworkObservation, PendingOpenEndedSourceReason,
        PendingParserObservation, PendingPipelineMembershipRevision, PendingProducerObservation,
        PendingProducerPriorEmptyQualification, PendingRenderingObservation,
        PendingSchedulerObservation, PendingSourceEpoch, PendingSourceId, PendingSourceSnapshot,
        PendingTargetObservation, PendingTargetTimeTerminalObservation, RuntimeStateGeneration,
    };
    use servo_base::Epoch;
    use servo_base::id::{TEST_PIPELINE_ID, TEST_SCRIPT_EVENT_LOOP_ID, TEST_WEBVIEW_ID};
    use timers::{
        DocumentClock, DocumentClockConfiguration, DocumentExecutionBudget,
        DocumentExecutionCounters, DocumentExecutionLimits, DocumentExecutionObservation,
        DocumentExecutionTerminal, DocumentProducerCheckpoint, DocumentProducerFence,
        DocumentProducerKind, DocumentTime, DocumentTimeSurface, DocumentUnixTime,
        TimerDeadlineSnapshot, TimerEventRequest, TimerScheduler,
    };

    use super::*;

    struct Fixture {
        clock: DocumentClock,
        scheduler: TimerScheduler,
        fence: DocumentProducerFence,
    }

    impl Fixture {
        fn new() -> Self {
            let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
                initial_time_ns: 5,
                unix_time_origin_ns: DocumentUnixTime::from_nanos(1_000_000),
            });
            Self {
                scheduler: TimerScheduler::with_clock(clock.clone()),
                clock,
                fence: DocumentProducerFence::default(),
            }
        }

        fn schedule(&mut self, duration: Duration) -> timers::TimerId {
            self.scheduler.schedule_timer(TimerEventRequest {
                callback: Box::new(|| {}),
                duration,
            })
        }

        fn snapshot(&self, checkpoint: u64, state_generation: u64) -> RawPendingSnapshot {
            let (microtasks, producers) = stable_producers(&self.fence, checkpoint);
            let pending = RawPendingSnapshot {
                target: target(),
                state_generation: RuntimeStateGeneration::new(state_generation),
                dom_epoch: DomEpoch::new(4),
                clock: PendingClockObservation {
                    clock_id: self.clock.id(),
                    mode: PendingClockMode::Controlled,
                    now: self.clock.now(),
                    unsupported_surface: None,
                },
                scheduler: PendingSchedulerObservation {
                    scheduler_id: self.scheduler.id(),
                    next_deadline: self.scheduler.finite_deadline_snapshot().unwrap(),
                },
                input: PendingInputObservation::default(),
                microtasks,
                execution: Some(DocumentExecutionObservation {
                    clock_id: self.clock.id(),
                    limits: DocumentExecutionLimits::CONTROLLED_WEBAPP_V1,
                    counters: DocumentExecutionCounters::default(),
                    terminal: None,
                }),
                producers,
                parser: PendingParserObservation::default(),
                network: PendingNetworkObservation::default(),
                logical_timers: PendingLogicalTimerSnapshot::default(),
                rendering: rendering(),
                sources: PendingSourceSnapshot::default(),
                terminals: PendingRuntimeTerminals::default(),
            };
            pending.validate().unwrap();
            pending
        }
    }

    fn target() -> PendingTargetObservation {
        PendingTargetObservation::new_with_authority(
            TEST_WEBVIEW_ID,
            TEST_SCRIPT_EVENT_LOOP_ID,
            Some(PendingActiveTopLevelPipeline {
                pipeline_id: TEST_PIPELINE_ID,
                epoch: Epoch(7),
            }),
            PendingNavigationRevision::new(3),
            PendingPipelineMembershipRevision::new(9),
            None,
            vec![TEST_PIPELINE_ID],
            vec![TEST_PIPELINE_ID],
            Vec::new(),
        )
        .unwrap()
    }

    fn checkpoint(sequence: u64) -> DocumentProducerCheckpoint {
        (0..sequence).fold(DocumentProducerCheckpoint::ZERO, |checkpoint, _| {
            checkpoint.checked_next().unwrap()
        })
    }

    fn stable_producers(
        fence: &DocumentProducerFence,
        current_checkpoint: u64,
    ) -> (PendingMicrotaskObservation, PendingProducerObservation) {
        assert!(current_checkpoint >= 2);
        let microtask_checkpoint = PendingMicrotaskCheckpoint::new(current_checkpoint);
        let snapshot = fence.snapshot();
        let producers = PendingProducerObservation::new(
            TEST_SCRIPT_EVENT_LOOP_ID,
            microtask_checkpoint,
            checkpoint(current_checkpoint),
            snapshot,
            PendingProducerStability::StableEmpty,
            Some(PendingProducerPriorEmptyQualification {
                microtask_checkpoint: PendingMicrotaskCheckpoint::new(current_checkpoint - 1),
                checkpoint: checkpoint(current_checkpoint - 1),
                snapshot_revision: snapshot.revision(),
            }),
        )
        .unwrap();
        (
            PendingMicrotaskObservation {
                event_loop_id: TEST_SCRIPT_EVENT_LOOP_ID,
                queued: 0,
                completed_checkpoint: microtask_checkpoint,
                checkpoint_in_progress: false,
                terminal: None,
            },
            producers,
        )
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

    fn timer_source(id: u64, disposition: PendingSourceDisposition) -> PendingSourceObservation {
        PendingSourceObservation {
            id: PendingSourceId::new(id),
            kind: PendingSourceKind::Timer,
            disposition,
        }
    }

    fn logical_timer(
        source_id: u64,
        creation_sequence: u64,
        kind: PendingLogicalTimerKind,
        logical_deadline: DocumentTime,
        outer_wake: Option<TimerDeadlineSnapshot>,
    ) -> PendingLogicalTimerObservation {
        let stable_id = match kind {
            PendingLogicalTimerKind::JavaScriptOneShot
            | PendingLogicalTimerKind::JavaScriptInterval { .. } => {
                PendingLogicalTimerStableId::JavaScriptHandle(i32::try_from(source_id).unwrap())
            },
            PendingLogicalTimerKind::XmlHttpRequestTimeout
            | PendingLogicalTimerKind::EventSourceReconnect
            | PendingLogicalTimerKind::RefreshRedirect
            | PendingLogicalTimerKind::RunStepsAfterTimeout
            | PendingLogicalTimerKind::TestBindingCallback => {
                PendingLogicalTimerStableId::EngineHandle(i32::try_from(source_id).unwrap())
            },
        };
        PendingLogicalTimerObservation {
            source_id: PendingSourceId::new(source_id),
            pipeline_id: TEST_PIPELINE_ID,
            stable_id,
            creation_sequence,
            kind,
            logical_deadline,
            suspended: false,
            eligible_in_controlled_turn: true,
            is_ordering_head: outer_wake.is_some(),
            delivery_ready: false,
            outer_wake,
        }
    }

    fn set_timer_sources(
        pending: &mut RawPendingSnapshot,
        sources: Vec<PendingSourceObservation>,
        logical_timers: Vec<PendingLogicalTimerObservation>,
    ) {
        pending.sources = PendingSourceSnapshot::new(PendingSourceEpoch::new(1), sources).unwrap();
        pending.logical_timers = PendingLogicalTimerSnapshot::new(logical_timers).unwrap();
        pending.validate().unwrap();
    }

    fn joined_deadline(scheduler: &TimerScheduler, id: timers::TimerId) -> TimerDeadlineSnapshot {
        let joined = scheduler
            .join_live_deadlines(scheduler.id(), &[id])
            .unwrap()[0];
        TimerDeadlineSnapshot {
            scheduler_id: joined.scheduler_id,
            id: joined.id,
            deadline: joined.deadline.unwrap(),
        }
    }

    fn token(pending: &RawPendingSnapshot) -> DocumentAdvanceToken {
        DocumentAdvanceToken::new_internal(DocumentAdvanceTokenId::new(17), pending).unwrap()
    }

    fn received(
        action: DocumentControlAction,
        pending: RawPendingSnapshot,
        advance_token: Option<DocumentAdvanceToken>,
    ) -> DocumentControlReceiveOutcome {
        let observation =
            DocumentControlObservation::new_internal(action, Box::new(pending), advance_token)
                .unwrap();
        DocumentControlReceiveOutcome::CommandOutcome(DocumentControlOutcome::Completed(Box::new(
            observation,
        )))
    }

    fn observe(
        coordinator: &mut SettleCoordinator,
        pending: RawPendingSnapshot,
        advance_token: Option<DocumentAdvanceToken>,
    ) -> Result<SettleProgress, SettleFailure> {
        coordinator.consume_receive_outcome(
            received(DocumentControlAction::Observed, pending, advance_token),
            Duration::ZERO,
        )
    }

    fn assert_observe(progress: SettleProgress) {
        assert!(matches!(
            progress,
            SettleProgress::Command(DocumentControlCommand::Observe)
        ));
    }

    fn replacement_bootstrap() -> DocumentControlCommand {
        DocumentControlCommand::BootstrapReplacementPipeline {
            source_pipeline_id: TEST_PIPELINE_ID,
            pipeline_id: TEST_PIPELINE_ID,
        }
    }

    fn execution_limit_observation(
        clock_id: DocumentClockId,
        budget: DocumentExecutionBudget,
        limit: u64,
    ) -> DocumentExecutionObservation {
        let observed = limit.checked_add(1).unwrap();
        let mut limits = DocumentExecutionLimits {
            ordinary_tasks: 100,
            microtasks: 100,
            rendering_opportunities: 100,
            mutations: 100,
        };
        let mut counters = DocumentExecutionCounters::default();
        match budget {
            DocumentExecutionBudget::OrdinaryTasks => {
                limits.ordinary_tasks = limit;
                counters.ordinary_tasks = limit;
            },
            DocumentExecutionBudget::Microtasks => {
                limits.microtasks = limit;
                counters.microtasks = limit;
            },
            DocumentExecutionBudget::RenderingOpportunities => {
                limits.rendering_opportunities = limit;
                counters.rendering_opportunities = limit;
            },
            DocumentExecutionBudget::MutationRecords => {
                limits.mutations = limit;
                counters.mutations = observed;
            },
        }
        DocumentExecutionObservation {
            clock_id,
            limits,
            counters,
            terminal: Some(DocumentExecutionTerminal::BudgetExceeded {
                budget,
                limit,
                observed,
            }),
        }
    }

    #[test]
    fn starts_with_observe_and_rejects_a_second_start() {
        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert_eq!(
            coordinator.start(),
            Err(SettleFailure::InvalidCoordinatorState(
                "settlement has already started"
            ))
        );
    }

    #[test]
    fn token_authorizing_observe_can_seed_the_same_coordinator_path_without_a_second_command() {
        let pending = Fixture::new().snapshot(2, 1);

        let mut ordinary = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(ordinary.start().unwrap());
        let ordinary_progress = ordinary
            .consume_receive_outcome(
                received(DocumentControlAction::Observed, pending.clone(), None),
                Duration::ZERO,
            )
            .unwrap();

        let mut seeded = SettleCoordinator::new(SettlePolicy::default());
        let seeded_progress = seeded
            .start_with_observe_outcome(
                received(DocumentControlAction::Observed, pending, None),
                Duration::ZERO,
            )
            .unwrap();

        assert_eq!(seeded_progress, ordinary_progress);
        assert_eq!(seeded.control_turns, 0);
        assert_eq!(seeded.initial_target, ordinary.initial_target);
        assert_eq!(seeded.last_target, ordinary.last_target);
        assert_eq!(seeded.initial_clock_id, ordinary.initial_clock_id);
        assert_eq!(seeded.phase, ordinary.phase);
        assert_eq!(
            seeded
                .in_flight
                .as_ref()
                .map(|in_flight| &in_flight.command),
            ordinary
                .in_flight
                .as_ref()
                .map(|in_flight| &in_flight.command),
        );
        assert_eq!(
            seeded.start_with_observe_outcome(
                received(
                    DocumentControlAction::Observed,
                    Fixture::new().snapshot(2, 1),
                    None,
                ),
                Duration::ZERO,
            ),
            Err(SettleFailure::InvalidCoordinatorState(
                "settlement has already started"
            ))
        );
    }

    #[test]
    fn every_engine_execution_budget_completes_with_its_typed_limit() {
        for budget in [
            DocumentExecutionBudget::OrdinaryTasks,
            DocumentExecutionBudget::Microtasks,
            DocumentExecutionBudget::RenderingOpportunities,
            DocumentExecutionBudget::MutationRecords,
        ] {
            let fixture = Fixture::new();
            let mut pending = fixture.snapshot(2, 1);
            pending.execution = Some(execution_limit_observation(
                pending.clock.clock_id,
                budget,
                3,
            ));
            pending.validate().unwrap();

            let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
            assert_observe(coordinator.start().unwrap());
            match observe(&mut coordinator, pending, None).unwrap() {
                SettleProgress::Complete(SettleCompletion::ExecutionLimitExceeded {
                    budget: observed_budget,
                    limit,
                    observed,
                    control_turns,
                    ..
                }) => {
                    assert_eq!(observed_budget, budget);
                    assert_eq!(limit, 3);
                    assert_eq!(observed, 4);
                    assert_eq!(control_turns, 0);
                },
                other => panic!("expected execution limit for {budget:?}, got {other:?}"),
            }
        }
    }

    #[test]
    fn exact_finite_timer_requests_guarded_advance() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_nanos(20));
        let mut pending = fixture.snapshot(2, 1);
        let head = pending.scheduler.next_deadline.unwrap();
        set_timer_sources(
            &mut pending,
            vec![timer_source(
                1,
                PendingSourceDisposition::FiniteDeadline(head.deadline),
            )],
            vec![logical_timer(
                1,
                1,
                PendingLogicalTimerKind::JavaScriptOneShot,
                head.deadline,
                Some(head),
            )],
        );
        let advance_token = token(&pending);

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        let progress = observe(&mut coordinator, pending, Some(advance_token.clone())).unwrap();
        match progress {
            SettleProgress::Command(DocumentControlCommand::AdvanceTo(observed)) => {
                assert_eq!(*observed, advance_token);
            },
            other => panic!("expected guarded advance, got {other:?}"),
        }
    }

    #[test]
    fn additional_foreground_io_freezes_and_then_releases_virtual_advance() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_nanos(20));
        let mut pending = fixture.snapshot(2, 1);
        let head = pending.scheduler.next_deadline.unwrap();
        set_timer_sources(
            &mut pending,
            vec![timer_source(
                1,
                PendingSourceDisposition::FiniteDeadline(head.deadline),
            )],
            vec![logical_timer(
                1,
                1,
                PendingLogicalTimerKind::JavaScriptOneShot,
                head.deadline,
                Some(head),
            )],
        );
        let advance_token = token(&pending);

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        coordinator.set_additional_foreground_external_io_active(true);
        assert_observe(coordinator.start().unwrap());
        match observe(
            &mut coordinator,
            pending.clone(),
            Some(advance_token.clone()),
        )
        .unwrap()
        {
            SettleProgress::Wait(SettleWait::ForegroundExternalIo { network, .. }) => {
                assert!(network.is_empty());
            },
            other => panic!("additional foreground I/O did not freeze advance: {other:?}"),
        }

        coordinator.set_additional_foreground_external_io_active(false);
        assert_observe(coordinator.resume_after_wake(Duration::ZERO).unwrap());
        match observe(&mut coordinator, pending, Some(advance_token.clone())).unwrap() {
            SettleProgress::Command(DocumentControlCommand::AdvanceTo(observed)) => {
                assert_eq!(*observed, advance_token);
            },
            other => panic!("clearing additional foreground I/O did not resume: {other:?}"),
        }
    }

    #[test]
    fn exact_now_finite_timer_requests_guarded_activation_not_a_drive() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::ZERO);
        let mut pending = fixture.snapshot(2, 1);
        let head = pending.scheduler.next_deadline.unwrap();
        assert_eq!(head.deadline, pending.clock.now);
        set_timer_sources(
            &mut pending,
            vec![timer_source(
                1,
                PendingSourceDisposition::FiniteDeadline(head.deadline),
            )],
            vec![logical_timer(
                1,
                1,
                PendingLogicalTimerKind::JavaScriptOneShot,
                head.deadline,
                Some(head),
            )],
        );
        let advance_token = token(&pending);

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending, Some(advance_token)).unwrap(),
            SettleProgress::Command(DocumentControlCommand::AdvanceTo(token))
                if token.deadline() == head
        ));
    }

    #[test]
    fn same_timestamp_without_exact_scheduler_identity_fails_closed() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_nanos(20));
        let finite_id = fixture.schedule(Duration::from_nanos(20));
        let finite_wake = joined_deadline(&fixture.scheduler, finite_id);
        let mut pending = fixture.snapshot(2, 1);
        assert_ne!(pending.scheduler.next_deadline, Some(finite_wake));
        assert_eq!(
            pending.scheduler.next_deadline.unwrap().deadline,
            finite_wake.deadline
        );
        set_timer_sources(
            &mut pending,
            vec![timer_source(
                1,
                PendingSourceDisposition::FiniteDeadline(finite_wake.deadline),
            )],
            vec![logical_timer(
                1,
                1,
                PendingLogicalTimerKind::JavaScriptOneShot,
                finite_wake.deadline,
                Some(finite_wake),
            )],
        );
        let advance_token = token(&pending);

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending, Some(advance_token)).unwrap(),
            SettleProgress::Complete(SettleCompletion::RuntimeError {
                failure: SettleRuntimeFailure::UnclassifiedSchedulerHead,
                ..
            })
        ));
    }

    #[test]
    fn queued_microtask_drives_before_future_timer() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_secs(10));
        let mut pending = fixture.snapshot(2, 1);
        pending.microtasks.queued = 1;

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending, None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
    }

    #[test]
    fn activated_logical_timer_delivery_drives_without_advancing_again() {
        let fixture = Fixture::new();
        let mut pending = fixture.snapshot(2, 1);
        let mut timer = logical_timer(
            1,
            1,
            PendingLogicalTimerKind::JavaScriptOneShot,
            pending.clock.now,
            None,
        );
        timer.is_ordering_head = true;
        timer.delivery_ready = true;
        set_timer_sources(
            &mut pending,
            vec![timer_source(1, PendingSourceDisposition::Ready)],
            vec![timer],
        );

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending, None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
    }

    #[test]
    fn external_io_freezes_time_and_expiry_requires_final_observe() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_secs(10));
        let mut pending = fixture.snapshot(2, 1);
        let timer_head = pending.scheduler.next_deadline.unwrap();
        let source_id = PendingSourceId::new(3);
        let evidence = PendingExternalIoEvidence {
            owner: PendingExternalIoOwner::Script,
            load_blocking: PendingExternalIoLoadBlocking::NonBlocking,
        };
        let operation = PendingExternalIoObservation {
            source_id,
            pipeline_id: TEST_PIPELINE_ID,
            kind: PendingNetworkKind::Fetch,
            phase: PendingExternalIoPhase::AwaitingResponse,
            evidence,
            started_at: pending.clock.now,
        };
        pending.network = PendingNetworkObservation::new(vec![operation]).unwrap();
        pending.sources = PendingSourceSnapshot::new(
            PendingSourceEpoch::new(1),
            vec![
                timer_source(
                    1,
                    PendingSourceDisposition::FiniteDeadline(timer_head.deadline),
                ),
                PendingSourceObservation {
                    id: source_id,
                    kind: PendingSourceKind::Network,
                    disposition: PendingSourceDisposition::AwaitingExternalIo(evidence),
                },
            ],
        )
        .unwrap();
        pending.logical_timers = PendingLogicalTimerSnapshot::new(vec![logical_timer(
            1,
            1,
            PendingLogicalTimerKind::JavaScriptOneShot,
            timer_head.deadline,
            Some(timer_head),
        )])
        .unwrap();
        pending.validate().unwrap();
        let advance_token = token(&pending);

        let policy = SettlePolicy {
            wall_io_timeout: Duration::from_millis(50),
            ..SettlePolicy::default()
        };
        let mut coordinator = SettleCoordinator::new(policy);
        assert_observe(coordinator.start().unwrap());
        match observe(&mut coordinator, pending.clone(), Some(advance_token)).unwrap() {
            SettleProgress::Wait(SettleWait::ForegroundExternalIo {
                network,
                remaining_wall_time,
                ..
            }) => {
                assert_eq!(network, vec![operation]);
                assert_eq!(remaining_wall_time, Duration::from_millis(50));
            },
            other => panic!("expected external-I/O wait, got {other:?}"),
        }

        assert_observe(
            coordinator
                .external_io_wait_expired(Duration::from_millis(50))
                .unwrap(),
        );
        match coordinator
            .consume_receive_outcome(
                received(DocumentControlAction::Observed, pending, None),
                Duration::from_millis(50),
            )
            .unwrap()
        {
            SettleProgress::Complete(SettleCompletion::BlockedOnExternalIo {
                network,
                control_turns,
                ..
            }) => {
                assert_eq!(network, vec![operation]);
                assert_eq!(control_turns, 0);
            },
            other => panic!("expected blocked external I/O, got {other:?}"),
        }
    }

    #[test]
    fn producer_handoff_is_bounded_and_requires_final_observation() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_secs(10));
        let mut pending = fixture.snapshot(2, 1);
        let timer_head = pending.scheduler.next_deadline.unwrap();
        set_timer_sources(
            &mut pending,
            vec![timer_source(
                1,
                PendingSourceDisposition::FiniteDeadline(timer_head.deadline),
            )],
            vec![logical_timer(
                1,
                1,
                PendingLogicalTimerKind::JavaScriptOneShot,
                timer_head.deadline,
                Some(timer_head),
            )],
        );
        let guard = fixture.fence.begin(DocumentProducerKind::Task).unwrap();
        pending.producers = PendingProducerObservation::new(
            TEST_SCRIPT_EVENT_LOOP_ID,
            pending.microtasks.completed_checkpoint,
            checkpoint(2),
            fixture.fence.snapshot(),
            PendingProducerStability::Busy,
            None,
        )
        .unwrap();
        pending.validate().unwrap();

        let policy = SettlePolicy {
            wall_io_timeout: Duration::from_millis(50),
            ..SettlePolicy::default()
        };
        let mut coordinator = SettleCoordinator::new(policy);
        assert_observe(coordinator.start().unwrap());
        match observe(&mut coordinator, pending.clone(), None).unwrap() {
            SettleProgress::Wait(SettleWait::ProducerHandoff {
                remaining_wall_time,
                ..
            }) => assert_eq!(remaining_wall_time, Duration::from_millis(50)),
            other => panic!("expected bounded producer handoff, got {other:?}"),
        }
        assert_observe(
            coordinator
                .external_io_wait_expired(Duration::from_millis(50))
                .unwrap(),
        );
        match coordinator
            .consume_receive_outcome(
                received(DocumentControlAction::Observed, pending, None),
                Duration::from_millis(50),
            )
            .unwrap()
        {
            SettleProgress::Complete(SettleCompletion::BlockedOnExternalIo {
                network,
                control_turns,
                ..
            }) => {
                assert!(network.is_empty());
                assert_eq!(control_turns, 0);
            },
            other => panic!("expected blocked producer handoff, got {other:?}"),
        }
        drop(guard);
    }

    #[test]
    fn interval_is_persistent_and_is_never_advanced() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_secs(5));
        let mut first = fixture.snapshot(2, 1);
        let head = first.scheduler.next_deadline.unwrap();
        set_timer_sources(
            &mut first,
            vec![timer_source(
                1,
                PendingSourceDisposition::OpenEnded(PendingOpenEndedSourceReason::Interval {
                    requested_period: Duration::from_secs(5),
                }),
            )],
            vec![logical_timer(
                1,
                1,
                PendingLogicalTimerKind::JavaScriptInterval {
                    requested_period: Duration::from_secs(5),
                },
                head.deadline,
                Some(head),
            )],
        );
        let mut second = fixture.snapshot(3, 2);
        second.sources = first.sources.clone();
        second.logical_timers = first.logical_timers.clone();
        second.validate().unwrap();

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, first, None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
        match coordinator
            .consume_receive_outcome(
                received(
                    DocumentControlAction::CheckpointTurnProcessed {
                        microtask_checkpoint_advanced: true,
                    },
                    second,
                    None,
                ),
                Duration::ZERO,
            )
            .unwrap()
        {
            SettleProgress::Complete(SettleCompletion::QuiescentWithPersistentWork {
                persistent,
                control_turns,
                ..
            }) => {
                assert!(matches!(
                    persistent.as_slice(),
                    [PersistentWork::Source(PendingSourceObservation {
                        disposition: PendingSourceDisposition::OpenEnded(
                            PendingOpenEndedSourceReason::Interval { .. }
                        ),
                        ..
                    })]
                ));
                assert_eq!(control_turns, 1);
            },
            other => panic!("expected persistent quiescence, got {other:?}"),
        }
    }

    #[test]
    fn exact_now_interval_is_checkpointed_without_guarded_activation() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::ZERO);
        let mut first = fixture.snapshot(2, 1);
        let head = first.scheduler.next_deadline.unwrap();
        assert_eq!(head.deadline, first.clock.now);
        set_timer_sources(
            &mut first,
            vec![timer_source(
                1,
                PendingSourceDisposition::OpenEnded(PendingOpenEndedSourceReason::Interval {
                    requested_period: Duration::ZERO,
                }),
            )],
            vec![logical_timer(
                1,
                1,
                PendingLogicalTimerKind::JavaScriptInterval {
                    requested_period: Duration::ZERO,
                },
                head.deadline,
                Some(head),
            )],
        );
        let mut second = fixture.snapshot(3, 2);
        second.sources = first.sources.clone();
        second.logical_timers = first.logical_timers.clone();
        second.validate().unwrap();

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, first, None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
        assert!(matches!(
            coordinator
                .consume_receive_outcome(
                    received(
                        DocumentControlAction::CheckpointTurnProcessed {
                            microtask_checkpoint_advanced: true,
                        },
                        second,
                        None,
                    ),
                    Duration::ZERO,
                )
                .unwrap(),
            SettleProgress::Complete(SettleCompletion::QuiescentWithPersistentWork {
                control_turns: 1,
                ..
            })
        ));
    }

    #[test]
    fn event_source_reconnect_head_is_unsupported_and_never_advanced() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_secs(5));
        let mut pending = fixture.snapshot(2, 1);
        let head = pending.scheduler.next_deadline.unwrap();
        set_timer_sources(
            &mut pending,
            vec![timer_source(
                1,
                PendingSourceDisposition::OpenEnded(PendingOpenEndedSourceReason::EventSource),
            )],
            vec![logical_timer(
                1,
                1,
                PendingLogicalTimerKind::EventSourceReconnect,
                head.deadline,
                Some(head),
            )],
        );

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending, None).unwrap(),
            SettleProgress::Complete(SettleCompletion::RuntimeError {
                failure: SettleRuntimeFailure::UnsupportedOpenEndedSource(
                    PendingSourceObservation {
                        disposition: PendingSourceDisposition::OpenEnded(
                            PendingOpenEndedSourceReason::EventSource
                        ),
                        ..
                    }
                ),
                ..
            })
        ));
    }

    #[test]
    fn interval_head_blocks_finite_work_behind_it() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_nanos(10));
        let finite_id = fixture.schedule(Duration::from_nanos(20));
        let finite_wake = joined_deadline(&fixture.scheduler, finite_id);
        let finite_deadline = finite_wake.deadline;
        let mut pending = fixture.snapshot(2, 1);
        let head = pending.scheduler.next_deadline.unwrap();
        set_timer_sources(
            &mut pending,
            vec![
                timer_source(
                    1,
                    PendingSourceDisposition::OpenEnded(PendingOpenEndedSourceReason::Interval {
                        requested_period: Duration::from_nanos(10),
                    }),
                ),
                timer_source(2, PendingSourceDisposition::FiniteDeadline(finite_deadline)),
            ],
            vec![
                logical_timer(
                    1,
                    1,
                    PendingLogicalTimerKind::JavaScriptInterval {
                        requested_period: Duration::from_nanos(10),
                    },
                    head.deadline,
                    Some(head),
                ),
                logical_timer(
                    2,
                    2,
                    PendingLogicalTimerKind::JavaScriptOneShot,
                    finite_deadline,
                    None,
                ),
            ],
        );

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending, None).unwrap(),
            SettleProgress::Complete(SettleCompletion::BlockedOnOpenEndedWork { .. })
        ));
    }

    #[test]
    fn finite_debounce_advances_before_later_interval_heartbeat() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_nanos(10));
        let interval_id = fixture.schedule(Duration::from_nanos(50));
        let interval_deadline = joined_deadline(&fixture.scheduler, interval_id).deadline;
        let mut pending = fixture.snapshot(2, 1);
        let head = pending.scheduler.next_deadline.unwrap();
        set_timer_sources(
            &mut pending,
            vec![
                timer_source(1, PendingSourceDisposition::FiniteDeadline(head.deadline)),
                timer_source(
                    2,
                    PendingSourceDisposition::OpenEnded(PendingOpenEndedSourceReason::Interval {
                        requested_period: Duration::from_nanos(50),
                    }),
                ),
            ],
            vec![
                logical_timer(
                    1,
                    1,
                    PendingLogicalTimerKind::JavaScriptOneShot,
                    head.deadline,
                    Some(head),
                ),
                logical_timer(
                    2,
                    2,
                    PendingLogicalTimerKind::JavaScriptInterval {
                        requested_period: Duration::from_nanos(50),
                    },
                    interval_deadline,
                    None,
                ),
            ],
        );
        let advance_token = token(&pending);

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending, Some(advance_token)).unwrap(),
            SettleProgress::Command(DocumentControlCommand::AdvanceTo(_))
        ));
    }

    #[test]
    fn interval_collision_at_finite_deadline_fails_closed() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_nanos(10));
        let mut pending = fixture.snapshot(2, 1);
        let head = pending.scheduler.next_deadline.unwrap();
        let deadline = head.deadline;
        set_timer_sources(
            &mut pending,
            vec![
                timer_source(
                    1,
                    PendingSourceDisposition::OpenEnded(PendingOpenEndedSourceReason::Interval {
                        requested_period: Duration::from_nanos(10),
                    }),
                ),
                timer_source(2, PendingSourceDisposition::FiniteDeadline(deadline)),
            ],
            vec![
                logical_timer(
                    1,
                    1,
                    PendingLogicalTimerKind::JavaScriptInterval {
                        requested_period: Duration::from_nanos(10),
                    },
                    deadline,
                    Some(head),
                ),
                logical_timer(
                    2,
                    2,
                    PendingLogicalTimerKind::JavaScriptOneShot,
                    deadline,
                    None,
                ),
            ],
        );
        let advance_token = token(&pending);

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending, Some(advance_token)).unwrap(),
            SettleProgress::Complete(SettleCompletion::BlockedOnOpenEndedWork { .. })
        ));
    }

    #[test]
    fn virtual_time_limit_is_checked_before_advance() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_nanos(20));
        let mut pending = fixture.snapshot(2, 1);
        let head = pending.scheduler.next_deadline.unwrap();
        set_timer_sources(
            &mut pending,
            vec![timer_source(
                1,
                PendingSourceDisposition::FiniteDeadline(head.deadline),
            )],
            vec![logical_timer(
                1,
                1,
                PendingLogicalTimerKind::JavaScriptOneShot,
                head.deadline,
                Some(head),
            )],
        );
        let advance_token = token(&pending);
        let policy = SettlePolicy {
            max_virtual_time: Duration::from_nanos(10),
            ..SettlePolicy::default()
        };

        let mut coordinator = SettleCoordinator::new(policy);
        assert_observe(coordinator.start().unwrap());
        match observe(&mut coordinator, pending, Some(advance_token)).unwrap() {
            SettleProgress::Complete(SettleCompletion::VirtualTimeLimitExceeded {
                start_virtual_time_ns,
                requested_virtual_time_ns,
                ..
            }) => {
                assert_eq!(start_virtual_time_ns, 5);
                assert_eq!(requested_virtual_time_ns, 25);
            },
            other => panic!("expected virtual-time limit, got {other:?}"),
        }
    }

    #[test]
    fn control_turn_limit_does_not_claim_event_counts() {
        let fixture = Fixture::new();
        let mut pending = fixture.snapshot(2, 1);
        pending.microtasks.queued = 1;
        let policy = SettlePolicy {
            max_control_turns: 0,
            ..SettlePolicy::default()
        };

        let mut coordinator = SettleCoordinator::new(policy);
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending, None).unwrap(),
            SettleProgress::Complete(SettleCompletion::ControlTurnLimitExceeded {
                limit: 0,
                control_turns: 0,
                ..
            })
        ));
    }

    #[test]
    fn stale_advance_race_reobserves_but_indeterminate_advance_is_fatal() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_nanos(20));
        let mut pending = fixture.snapshot(2, 1);
        let head = pending.scheduler.next_deadline.unwrap();
        set_timer_sources(
            &mut pending,
            vec![timer_source(
                1,
                PendingSourceDisposition::FiniteDeadline(head.deadline),
            )],
            vec![logical_timer(
                1,
                1,
                PendingLogicalTimerKind::JavaScriptOneShot,
                head.deadline,
                Some(head),
            )],
        );
        let advance_token = token(&pending);

        let mut raced = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(raced.start().unwrap());
        assert!(matches!(
            observe(&mut raced, pending.clone(), Some(advance_token.clone())).unwrap(),
            SettleProgress::Command(DocumentControlCommand::AdvanceTo(_))
        ));
        assert_observe(
            raced
                .consume_receive_outcome(
                    DocumentControlReceiveOutcome::CommandOutcome(
                        DocumentControlOutcome::Rejected(
                            DocumentControlError::AdvancePrecondition(
                                DocumentAdvanceTokenInvariantError::StateGenerationChanged {
                                    expected: pending.state_generation,
                                    observed: RuntimeStateGeneration::new(2),
                                },
                            ),
                        ),
                    ),
                    Duration::ZERO,
                )
                .unwrap(),
        );

        let mut terminal_race = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(terminal_race.start().unwrap());
        assert!(matches!(
            observe(
                &mut terminal_race,
                pending.clone(),
                Some(advance_token.clone())
            )
            .unwrap(),
            SettleProgress::Command(DocumentControlCommand::AdvanceTo(_))
        ));
        assert_observe(
            terminal_race
                .consume_receive_outcome(
                    DocumentControlReceiveOutcome::CommandOutcome(
                        DocumentControlOutcome::Rejected(
                            DocumentControlError::AdvancePrecondition(
                                DocumentAdvanceTokenInvariantError::RuntimeTerminal,
                            ),
                        ),
                    ),
                    Duration::ZERO,
                )
                .unwrap(),
        );

        let mut indeterminate = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(indeterminate.start().unwrap());
        assert!(matches!(
            observe(&mut indeterminate, pending, Some(advance_token.clone())).unwrap(),
            SettleProgress::Command(DocumentControlCommand::AdvanceTo(_))
        ));
        let outcome = DocumentControlOutcome::AdvanceOutcomeIndeterminate {
            token_id: advance_token.id(),
            target: Box::new(advance_token.target().clone()),
            deadline: advance_token.deadline(),
        };
        assert_eq!(
            indeterminate.consume_receive_outcome(
                DocumentControlReceiveOutcome::CommandOutcome(outcome.clone()),
                Duration::ZERO,
            ),
            Err(SettleFailure::AdvanceOutcomeIndeterminate(Box::new(
                outcome
            )))
        );
    }

    #[test]
    fn every_indeterminate_drive_outcome_is_fatal() {
        let fixture = Fixture::new();
        let mut pending = fixture.snapshot(2, 1);
        pending.microtasks.queued = 1;

        let mut transport = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(transport.start().unwrap());
        assert!(matches!(
            observe(&mut transport, pending.clone(), None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
        assert_eq!(
            transport.consume_receive_outcome(
                DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(
                    DocumentControlTransportFailure::TimedOut,
                ),
                Duration::ZERO,
            ),
            Err(SettleFailure::DriveOneTurnOutcomeIndeterminate(
                DocumentControlTransportFailure::TimedOut,
            ))
        );

        let mut runtime = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(runtime.start().unwrap());
        assert!(matches!(
            observe(&mut runtime, pending, None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
        let outcome = DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate {
            target: Box::new(target()),
        };
        assert_eq!(
            runtime.consume_receive_outcome(
                DocumentControlReceiveOutcome::CommandOutcome(outcome.clone()),
                Duration::ZERO,
            ),
            Err(SettleFailure::DriveOutcomeIndeterminate(Box::new(outcome)))
        );
    }

    #[test]
    fn typed_drive_replacement_boundary_counts_the_lost_turn_and_preserves_budget() {
        let fixture = Fixture::new();
        let pending = fixture.snapshot(2, 1);
        let mut coordinator = SettleCoordinator::new(SettlePolicy {
            max_control_turns: 1,
            ..SettlePolicy::default()
        });
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending.clone(), None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));

        assert_eq!(
            coordinator
                .consume_drive_one_turn_replacement_boundary(
                    DocumentControlReceiveOutcome::CommandOutcome(
                        DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate {
                            target: Box::new(pending.target.clone()),
                        },
                    ),
                    Duration::from_millis(7),
                    replacement_bootstrap(),
                )
                .unwrap(),
            SettleProgress::Command(replacement_bootstrap())
        );

        let observed = fixture.snapshot(3, 2);
        assert!(matches!(
            coordinator
                .consume_receive_outcome(
                    received(
                        DocumentControlAction::TurnProcessed {
                            microtask_checkpoint_advanced: false,
                        },
                        observed,
                        None,
                    ),
                    Duration::from_millis(7),
                )
                .unwrap(),
            SettleProgress::Complete(SettleCompletion::ControlTurnLimitExceeded {
                limit: 1,
                control_turns: 2,
                ..
            })
        ));
    }

    #[test]
    fn typed_drive_replacement_boundary_rejects_transport_and_target_drift() {
        let fixture = Fixture::new();
        let pending = fixture.snapshot(2, 1);

        let mut transport = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(transport.start().unwrap());
        assert!(matches!(
            observe(&mut transport, pending.clone(), None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
        assert_eq!(
            transport.consume_drive_one_turn_replacement_boundary(
                DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(
                    DocumentControlTransportFailure::Disconnected,
                ),
                Duration::ZERO,
                replacement_bootstrap(),
            ),
            Err(SettleFailure::DriveOneTurnOutcomeIndeterminate(
                DocumentControlTransportFailure::Disconnected,
            ))
        );

        let mut mismatched = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(mismatched.start().unwrap());
        assert!(matches!(
            observe(&mut mismatched, pending, None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
        let mut wrong_target = target();
        wrong_target.navigation_revision = wrong_target.navigation_revision.checked_next().unwrap();
        assert_eq!(
            mismatched.consume_drive_one_turn_replacement_boundary(
                DocumentControlReceiveOutcome::CommandOutcome(
                    DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate {
                        target: Box::new(wrong_target),
                    },
                ),
                Duration::ZERO,
                replacement_bootstrap(),
            ),
            Err(SettleFailure::InvalidCoordinatorState(
                "document replacement boundary did not bind the last observed target",
            ))
        );
    }

    #[test]
    fn typed_drive_replacement_boundary_preserves_cumulative_wall_io_time() {
        let fixture = Fixture::new();
        let pending = fixture.snapshot(2, 1);
        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending.clone(), None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
        assert_eq!(
            coordinator
                .consume_drive_one_turn_replacement_boundary(
                    DocumentControlReceiveOutcome::CommandOutcome(
                        DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate {
                            target: Box::new(pending.target.clone()),
                        },
                    ),
                    Duration::from_millis(7),
                    replacement_bootstrap(),
                )
                .unwrap(),
            SettleProgress::Command(replacement_bootstrap())
        );

        assert_eq!(
            coordinator.consume_receive_outcome(
                received(
                    DocumentControlAction::TurnProcessed {
                        microtask_checkpoint_advanced: false,
                    },
                    fixture.snapshot(3, 2),
                    None,
                ),
                Duration::from_millis(6),
            ),
            Err(SettleFailure::ExternalIoWallTimeRegressed {
                previous: Duration::from_millis(7),
                observed: Duration::from_millis(6),
            })
        );
    }

    #[test]
    fn unsubmitted_drive_rearm_drops_source_quiet_candidate_without_counting_the_drive() {
        let fixture = Fixture::new();
        let pending = fixture.snapshot(2, 1);
        let mut coordinator = SettleCoordinator::new(SettlePolicy {
            max_control_turns: 1,
            ..SettlePolicy::default()
        });
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending.clone(), None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));

        assert_eq!(
            coordinator
                .replace_unsubmitted_drive_with_replacement_bootstrap(
                    &pending.target,
                    replacement_bootstrap(),
                )
                .unwrap(),
            SettleProgress::Command(replacement_bootstrap())
        );

        // Only the bootstrap's completed turn consumes budget. The unsubmitted Drive contributes
        // zero, and its source-document quiet candidate cannot complete the replacement document.
        assert!(matches!(
            coordinator
                .consume_receive_outcome(
                    received(
                        DocumentControlAction::TurnProcessed {
                            microtask_checkpoint_advanced: false,
                        },
                        fixture.snapshot(3, 2),
                        None,
                    ),
                    Duration::ZERO,
                )
                .unwrap(),
            SettleProgress::Complete(SettleCompletion::ControlTurnLimitExceeded {
                limit: 1,
                control_turns: 1,
                ..
            })
        ));
    }

    #[test]
    fn unsubmitted_drive_rearm_rejects_phase_target_and_command_near_misses() {
        let fixture = Fixture::new();
        let pending = fixture.snapshot(2, 1);

        let mut observing = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(observing.start().unwrap());
        assert_eq!(
            observing.replace_unsubmitted_drive_with_replacement_bootstrap(
                &pending.target,
                replacement_bootstrap(),
            ),
            Err(SettleFailure::InvalidCoordinatorState(
                "replacement rearm requires an unsubmitted DriveOneTurn",
            ))
        );

        let mut wrong_target = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(wrong_target.start().unwrap());
        assert!(matches!(
            observe(&mut wrong_target, pending.clone(), None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
        let mut drifted = pending.target.clone();
        drifted.navigation_revision = drifted.navigation_revision.checked_next().unwrap();
        assert_eq!(
            wrong_target.replace_unsubmitted_drive_with_replacement_bootstrap(
                &drifted,
                replacement_bootstrap(),
            ),
            Err(SettleFailure::InvalidCoordinatorState(
                "replacement rearm did not bind the last observed target",
            ))
        );

        let mut wrong_command = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(wrong_command.start().unwrap());
        assert!(matches!(
            observe(&mut wrong_command, pending.clone(), None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
        assert_eq!(
            wrong_command.replace_unsubmitted_drive_with_replacement_bootstrap(
                &pending.target,
                DocumentControlCommand::Observe,
            ),
            Err(SettleFailure::InvalidCoordinatorState(
                "replacement rearm requires BootstrapReplacementPipeline",
            ))
        );
    }

    #[test]
    fn unavailable_observe_is_definitive_instead_of_a_polling_loop() {
        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert_eq!(
            coordinator.consume_receive_outcome(
                DocumentControlReceiveOutcome::CommandOutcome(DocumentControlOutcome::Rejected(
                    DocumentControlError::EventLoopUnavailable,
                )),
                Duration::ZERO,
            ),
            Err(SettleFailure::ControlRejected(Box::new(
                DocumentControlError::EventLoopUnavailable,
            )))
        );
    }

    #[test]
    fn quiet_candidate_must_survive_a_fresh_identical_checkpoint() {
        let fixture = Fixture::new();
        let first = fixture.snapshot(2, 1);
        let mut changed = fixture.snapshot(3, 2);
        changed.dom_epoch = DomEpoch::new(5);
        changed.validate().unwrap();
        let mut matching = fixture.snapshot(4, 3);
        matching.dom_epoch = DomEpoch::new(5);
        matching.validate().unwrap();

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, first, None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
        assert!(matches!(
            coordinator
                .consume_receive_outcome(
                    received(
                        DocumentControlAction::CheckpointTurnProcessed {
                            microtask_checkpoint_advanced: true,
                        },
                        changed,
                        None,
                    ),
                    Duration::ZERO,
                )
                .unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
        assert!(matches!(
            coordinator
                .consume_receive_outcome(
                    received(
                        DocumentControlAction::CheckpointTurnProcessed {
                            microtask_checkpoint_advanced: true,
                        },
                        matching,
                        None,
                    ),
                    Duration::ZERO,
                )
                .unwrap(),
            SettleProgress::Complete(SettleCompletion::Quiescent {
                control_turns: 2,
                ..
            })
        ));
    }

    #[test]
    fn rendering_ready_drives_before_quiet_qualification() {
        let fixture = Fixture::new();
        let mut pending = fixture.snapshot(2, 1);
        pending.rendering =
            PendingRenderingObservation::new(None, true, pending.rendering.pipelines().to_vec())
                .unwrap();
        pending.validate().unwrap();

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending, None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
    }

    #[test]
    fn unscheduled_dom_raf_and_canvas_demands_drive() {
        for demand in ["dom", "raf", "canvas"] {
            let fixture = Fixture::new();
            let mut pending = fixture.snapshot(2, 1);
            let mut rendering = pending.rendering.pipelines()[0];
            match demand {
                "dom" => rendering.document_update_required = true,
                "raf" => {
                    rendering.retained_animation_frame_callbacks = 1;
                    rendering.runnable_animation_frame_callbacks = 1;
                },
                "canvas" => rendering.canvas.dirty_contexts = 1,
                _ => unreachable!(),
            }
            pending.rendering =
                PendingRenderingObservation::new(None, false, vec![rendering]).unwrap();
            pending.validate().unwrap();

            let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
            assert_observe(coordinator.start().unwrap());
            assert!(
                matches!(
                    observe(&mut coordinator, pending, None).unwrap(),
                    SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
                ),
                "unscheduled {demand} demand must drive"
            );
        }
    }

    #[test]
    fn exact_now_scheduled_dom_raf_and_canvas_demands_advance() {
        for demand in ["dom", "raf", "canvas"] {
            let mut fixture = Fixture::new();
            fixture.schedule(Duration::ZERO);
            let mut pending = fixture.snapshot(2, 1);
            let head = pending.scheduler.next_deadline.unwrap();
            assert_eq!(head.deadline, pending.clock.now);
            let mut rendering = pending.rendering.pipelines()[0];
            match demand {
                "dom" => rendering.document_update_required = true,
                "raf" => {
                    rendering.retained_animation_frame_callbacks = 1;
                    rendering.runnable_animation_frame_callbacks = 1;
                },
                "canvas" => rendering.canvas.dirty_contexts = 1,
                _ => unreachable!(),
            }
            pending.rendering =
                PendingRenderingObservation::new(Some(head), false, vec![rendering]).unwrap();
            pending.validate().unwrap();
            let advance_token = token(&pending);

            let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
            assert_observe(coordinator.start().unwrap());
            assert!(
                matches!(
                    observe(&mut coordinator, pending, Some(advance_token)).unwrap(),
                    SettleProgress::Command(DocumentControlCommand::AdvanceTo(_))
                ),
                "scheduled exact-now {demand} demand must advance"
            );
        }
    }

    #[test]
    fn future_rendering_opportunity_advances_instead_of_spinning() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_nanos(20));
        let mut pending = fixture.snapshot(2, 1);
        let head = pending.scheduler.next_deadline.unwrap();
        let mut rendering = pending.rendering.pipelines()[0];
        rendering.retained_animation_frame_callbacks = 1;
        rendering.runnable_animation_frame_callbacks = 1;
        pending.rendering =
            PendingRenderingObservation::new(Some(head), false, vec![rendering]).unwrap();
        pending.validate().unwrap();
        let advance_token = token(&pending);

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending, Some(advance_token)).unwrap(),
            SettleProgress::Command(DocumentControlCommand::AdvanceTo(_))
        ));
    }

    #[test]
    fn exact_now_rendering_opportunity_advances_instead_of_spinning() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::ZERO);
        let mut pending = fixture.snapshot(2, 1);
        let head = pending.scheduler.next_deadline.unwrap();
        assert_eq!(head.deadline, pending.clock.now);
        let mut rendering = pending.rendering.pipelines()[0];
        rendering.document_update_required = true;
        pending.rendering =
            PendingRenderingObservation::new(Some(head), false, vec![rendering]).unwrap();
        pending.validate().unwrap();
        let advance_token = token(&pending);

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending.clone(), Some(advance_token)).unwrap(),
            SettleProgress::Command(DocumentControlCommand::AdvanceTo(_))
        ));

        pending.state_generation = RuntimeStateGeneration::new(2);
        pending.scheduler.next_deadline = None;
        pending.rendering = PendingRenderingObservation::new(None, true, vec![rendering]).unwrap();
        pending.validate().unwrap();
        assert!(matches!(
            coordinator
                .consume_receive_outcome(
                    received(DocumentControlAction::TimerActivated(head), pending, None),
                    Duration::ZERO,
                )
                .unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
    }

    #[test]
    fn infinite_rendering_head_blocks_finite_timer_behind_it() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_nanos(10));
        let finite_id = fixture.schedule(Duration::from_nanos(20));
        let finite_wake = joined_deadline(&fixture.scheduler, finite_id);
        let finite_deadline = finite_wake.deadline;
        let mut pending = fixture.snapshot(2, 1);
        let head = pending.scheduler.next_deadline.unwrap();
        let mut rendering = pending.rendering.pipelines()[0];
        rendering.infinite_animations = 1;
        pending.rendering =
            PendingRenderingObservation::new(Some(head), false, vec![rendering]).unwrap();
        set_timer_sources(
            &mut pending,
            vec![timer_source(
                1,
                PendingSourceDisposition::FiniteDeadline(finite_deadline),
            )],
            vec![logical_timer(
                1,
                1,
                PendingLogicalTimerKind::JavaScriptOneShot,
                finite_deadline,
                Some(finite_wake),
            )],
        );

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending, None).unwrap(),
            SettleProgress::Complete(SettleCompletion::BlockedOnOpenEndedWork { .. })
        ));
    }

    #[test]
    fn infinite_rendering_head_wins_same_deadline_collision() {
        let mut fixture = Fixture::new();
        fixture.schedule(Duration::from_nanos(10));
        let finite_id = fixture.schedule(Duration::from_nanos(10));
        let finite_wake = joined_deadline(&fixture.scheduler, finite_id);
        let mut pending = fixture.snapshot(2, 1);
        let head = pending.scheduler.next_deadline.unwrap();
        let mut rendering = pending.rendering.pipelines()[0];
        rendering.infinite_animations = 1;
        pending.rendering =
            PendingRenderingObservation::new(Some(head), false, vec![rendering]).unwrap();
        set_timer_sources(
            &mut pending,
            vec![timer_source(
                1,
                PendingSourceDisposition::FiniteDeadline(head.deadline),
            )],
            vec![logical_timer(
                1,
                1,
                PendingLogicalTimerKind::JavaScriptOneShot,
                finite_wake.deadline,
                Some(finite_wake),
            )],
        );

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending, None).unwrap(),
            SettleProgress::Complete(SettleCompletion::BlockedOnOpenEndedWork { .. })
        ));
    }

    #[test]
    fn wall_time_regression_is_fatal() {
        let fixture = Fixture::new();
        let pending = fixture.snapshot(2, 1);
        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        coordinator.cumulative_external_io_wall_time = Duration::from_millis(2);
        assert_eq!(
            coordinator.consume_receive_outcome(
                received(DocumentControlAction::Observed, pending, None),
                Duration::from_millis(1),
            ),
            Err(SettleFailure::ExternalIoWallTimeRegressed {
                previous: Duration::from_millis(2),
                observed: Duration::from_millis(1),
            })
        );
    }

    #[test]
    fn document_time_regression_fails_closed() {
        let fixture = Fixture::new();
        let mut first = fixture.snapshot(2, 1);
        first.microtasks.queued = 1;
        let mut regressed = fixture.snapshot(3, 2);
        regressed.clock.now = DocumentTime::from_nanos(4);
        regressed.validate().unwrap();

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, first, None).unwrap(),
            SettleProgress::Command(DocumentControlCommand::DriveOneTurn)
        ));
        assert!(matches!(
            coordinator
                .consume_receive_outcome(
                    received(
                        DocumentControlAction::TurnProcessed {
                            microtask_checkpoint_advanced: true,
                        },
                        regressed,
                        None,
                    ),
                    Duration::ZERO,
                )
                .unwrap(),
            SettleProgress::Complete(SettleCompletion::RuntimeError {
                failure: SettleRuntimeFailure::VirtualTimeRegressed,
                ..
            })
        ));
    }

    #[test]
    fn target_time_terminal_reports_the_specific_unsupported_surface() {
        let fixture = Fixture::new();
        let mut pending = fixture.snapshot(2, 1);
        pending.target.unsupported_time_surface = Some(DocumentTimeSurface::Worker);
        pending.terminals.target_time = Some(PendingTargetTimeTerminalObservation {
            webview_id: TEST_WEBVIEW_ID,
            unsupported_surface: DocumentTimeSurface::Worker,
        });
        pending.validate().unwrap();

        let mut coordinator = SettleCoordinator::new(SettlePolicy::default());
        assert_observe(coordinator.start().unwrap());
        assert!(matches!(
            observe(&mut coordinator, pending, None).unwrap(),
            SettleProgress::Complete(SettleCompletion::RuntimeError {
                failure: SettleRuntimeFailure::UnsupportedClockSurface,
                ..
            })
        ));
    }
}
