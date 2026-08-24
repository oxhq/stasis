/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Side-effect-free capture of facts owned by the controlled ScriptThread barrier.

use embedder_traits::document_control::{DocumentControlError, DocumentPendingFact};
use embedder_traits::document_pending::{
    PendingClockMode, PendingClockObservation, PendingClockTerminal,
    PendingClockTerminalObservation, PendingEventLoopGenerationTerminalObservation,
    PendingGenerationTerminal, PendingInputObservation, PendingMicrotaskCheckpoint,
    PendingMicrotaskObservation, PendingMicrotaskTerminal, PendingMicrotaskTerminalObservation,
    PendingOuterSchedulerTerminalObservation, PendingSchedulerObservation, PendingTaskObservation,
};
use servo_base::id::ScriptEventLoopId;
use timers::{
    DocumentClock, DocumentClockError, DocumentExecutionObservation, TimerControlError,
    TimerScheduler,
};

use super::pending_state::{
    PendingBuildError, PendingCountKind, PendingFactKind, PendingNormalizeError,
    PendingOwnedTerminalSlot, PendingStateError,
};
use crate::microtask::{MicrotaskCheckpointError, MicrotaskQueueObservation};
use crate::tasks::task_queue::TaskQueueObservation;

/// Barrier-owned copied facts and their independently retained terminals.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingBarrierObservation {
    pub(crate) clock: PendingClockObservation,
    pub(crate) clock_terminal: Option<PendingClockTerminalObservation>,
    pub(crate) scheduler: PendingSchedulerObservation,
    pub(crate) scheduler_terminal: Option<PendingOuterSchedulerTerminalObservation>,
    pub(crate) input: PendingInputObservation,
    pub(crate) input_terminal: Option<PendingEventLoopGenerationTerminalObservation>,
    pub(crate) microtasks: PendingMicrotaskObservation,
    pub(crate) microtask_terminal: Option<PendingMicrotaskTerminalObservation>,
    pub(crate) execution: DocumentExecutionObservation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PendingInputBarrierFacts {
    pub(crate) revision: embedder_traits::document_pending::PendingInputRevision,
    pub(crate) revision_exhausted: bool,
    pub(crate) ready_events: usize,
    pub(crate) intake_saturated: bool,
    pub(crate) tasks: TaskQueueObservation,
}

pub(crate) fn capture_barrier_observation(
    event_loop_id: ScriptEventLoopId,
    clock: &DocumentClock,
    scheduler: &TimerScheduler,
    scheduler_terminal: Option<TimerControlError>,
    input: PendingInputBarrierFacts,
    microtasks: MicrotaskQueueObservation,
    execution: DocumentExecutionObservation,
) -> Result<PendingBarrierObservation, DocumentControlError> {
    if !clock.is_controlled() {
        return Err(DocumentControlError::NotControlled);
    }

    let clock_observation = PendingClockObservation {
        clock_id: clock.id(),
        mode: PendingClockMode::Controlled,
        now: clock.try_now().map_err(DocumentControlError::Clock)?,
        unsupported_surface: clock.unsupported_surface(),
    };
    let clock_terminal = clock
        .terminal_error()
        .map(|error| pending_clock_terminal(clock, error))
        .transpose()?;

    let scheduler_observation = PendingSchedulerObservation {
        scheduler_id: scheduler.id(),
        next_deadline: scheduler
            .finite_deadline_snapshot()
            .map_err(DocumentControlError::Timer)?,
    };
    let scheduler_terminal =
        scheduler_terminal.map(|error| PendingOuterSchedulerTerminalObservation {
            event_loop_id,
            scheduler_id: scheduler.id(),
            error,
        });

    let input_observation = PendingInputObservation {
        revision: input.revision,
        ready_events: checked_count(input.ready_events)?,
        intake_saturated: input.intake_saturated,
        tasks: PendingTaskObservation {
            ready: checked_count(input.tasks.ready)?,
            throttled: checked_count(input.tasks.throttled)?,
            inactive: checked_count(input.tasks.inactive)?,
        },
    };
    let input_terminal =
        input
            .revision_exhausted
            .then_some(PendingEventLoopGenerationTerminalObservation {
                event_loop_id,
                error: PendingGenerationTerminal::Exhausted,
            });

    let microtask_terminal = microtasks.terminal_error.map(|error| match error {
        MicrotaskCheckpointError::GenerationExhausted => {
            PendingMicrotaskTerminal::CheckpointGenerationExhausted
        },
    });
    let microtask_observation = PendingMicrotaskObservation {
        event_loop_id,
        queued: checked_count(microtasks.queued_count)?,
        completed_checkpoint: PendingMicrotaskCheckpoint::new(
            microtasks.completed_checkpoint_generation,
        ),
        checkpoint_in_progress: microtasks.checkpoint_in_progress,
        terminal: microtask_terminal,
    };
    let microtask_terminal = microtask_terminal.map(|error| PendingMicrotaskTerminalObservation {
        event_loop_id,
        error,
    });

    Ok(PendingBarrierObservation {
        clock: clock_observation,
        clock_terminal,
        scheduler: scheduler_observation,
        scheduler_terminal,
        input: input_observation,
        input_terminal,
        microtasks: microtask_observation,
        microtask_terminal,
        execution,
    })
}

fn pending_clock_terminal(
    clock: &DocumentClock,
    error: DocumentClockError,
) -> Result<PendingClockTerminalObservation, DocumentControlError> {
    let error = match error {
        DocumentClockError::Overflow => PendingClockTerminal::Overflow,
        DocumentClockError::JavaScriptDatePrecisionLoss {
            unix_time,
            expected_milliseconds,
            observed_milliseconds,
        } => PendingClockTerminal::JavaScriptDatePrecisionLoss {
            unix_time,
            expected_milliseconds,
            observed_milliseconds,
        },
        error => return Err(DocumentControlError::Clock(error)),
    };
    Ok(PendingClockTerminalObservation {
        clock_id: clock.id(),
        error,
    })
}

fn checked_count(count: usize) -> Result<u64, DocumentControlError> {
    u64::try_from(count).map_err(|_| DocumentControlError::QueueLengthOverflow)
}

/// Collapse internal owner/build failures into the public typed pending-observation boundary.
pub(crate) fn map_pending_normalize_error(error: PendingNormalizeError) -> DocumentControlError {
    match error {
        PendingNormalizeError::Build(error) => map_pending_build_error(error),
        PendingNormalizeError::State(error) => map_pending_state_error(error),
        PendingNormalizeError::StaleOwnerFacts
        | PendingNormalizeError::NonMonotonicStateGeneration { .. } => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::StateGeneration)
        },
        PendingNormalizeError::ResourceFallbackTargetUnavailable(_) => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::TargetMembership)
        },
        PendingNormalizeError::ResourceFenceAuthorityMismatch { .. } => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Producers)
        },
        PendingNormalizeError::PersistentSourceOutsideTarget(_) => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Sources)
        },
    }
}

fn map_pending_build_error(error: PendingBuildError) -> DocumentControlError {
    match error {
        PendingBuildError::MissingFact(fact) => {
            DocumentControlError::PendingFactUnavailable(match fact {
                PendingFactKind::Sources => DocumentPendingFact::Sources,
                PendingFactKind::LogicalTimers => DocumentPendingFact::LogicalTimers,
                PendingFactKind::Parser => DocumentPendingFact::Parser,
                PendingFactKind::Network => DocumentPendingFact::Network,
                PendingFactKind::Rendering => DocumentPendingFact::Rendering,
            })
        },
        PendingBuildError::WebViewOwnerMismatch { .. }
        | PendingBuildError::EventLoopOwnerMismatch { .. } => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::TargetMembership)
        },
        PendingBuildError::OwnedTerminalConflict(slot) => {
            DocumentControlError::PendingFactUnavailable(match slot {
                PendingOwnedTerminalSlot::Clock => DocumentPendingFact::Clock,
                PendingOwnedTerminalSlot::OuterScheduler => DocumentPendingFact::Scheduler,
                PendingOwnedTerminalSlot::Producer => DocumentPendingFact::Producers,
                PendingOwnedTerminalSlot::Microtask => DocumentPendingFact::MicrotaskCoverage,
                PendingOwnedTerminalSlot::InputRevision => DocumentPendingFact::Input,
                PendingOwnedTerminalSlot::SourceId | PendingOwnedTerminalSlot::SourceEpoch => {
                    DocumentPendingFact::Sources
                },
                PendingOwnedTerminalSlot::StateGeneration => DocumentPendingFact::StateGeneration,
            })
        },
        PendingBuildError::CountOverflow(count) => match count {
            PendingCountKind::ReadyEvents
            | PendingCountKind::ReadyTasks
            | PendingCountKind::ThrottledTasks
            | PendingCountKind::InactiveTasks
            | PendingCountKind::Microtasks => DocumentControlError::QueueLengthOverflow,
        },
        PendingBuildError::InputRevisionTerminalBeforeExhaustion => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Input)
        },
        PendingBuildError::UnrepresentedResourceProducer => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Network)
        },
        PendingBuildError::Invariant(error) => DocumentControlError::PendingSnapshot(error),
    }
}

fn map_pending_state_error(error: PendingStateError) -> DocumentControlError {
    match error {
        PendingStateError::DuplicateWebView(_) | PendingStateError::UnknownWebView(_) => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::TargetMembership)
        },
        PendingStateError::StateGenerationExhausted(_) => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::StateGeneration)
        },
        PendingStateError::UnknownSource(_)
        | PendingStateError::SourceEpochExhausted(_)
        | PendingStateError::SourceIdExhausted => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Sources)
        },
        PendingStateError::UnknownParser(_)
        | PendingStateError::DuplicateParserOwner(_)
        | PendingStateError::MissingNetworkParent(_)
        | PendingStateError::NetworkParentKindMismatch { .. }
        | PendingStateError::NetworkParentPipelineMismatch { .. } => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Parser)
        },
        PendingStateError::Network(_) | PendingStateError::NetworkOperationIdExhausted => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Network)
        },
        PendingStateError::DuplicatePersistentSource(_)
        | PendingStateError::InvalidPersistentSource(_) => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Sources)
        },
        PendingStateError::ResourceFenceAlreadyBound { .. } => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Producers)
        },
        PendingStateError::ProducerPriorEmptyMissing => {
            DocumentControlError::PendingFactUnavailable(DocumentPendingFact::Producers)
        },
        PendingStateError::SnapshotInvariant(error) => DocumentControlError::PendingSnapshot(error),
    }
}

#[cfg(test)]
mod tests {
    use servo_base::id::ScriptEventLoopId;
    use timers::{
        DocumentClock, DocumentClockConfiguration, DocumentExecutionCounters,
        DocumentExecutionLimits, DocumentExecutionObservation, DocumentTime, DocumentUnixTime,
        TimerEventRequest, TimerScheduler,
    };

    use super::*;

    #[test]
    fn capture_preserves_exact_barrier_counts_and_deadline() {
        let event_loop_id = ScriptEventLoopId::new();
        let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 17,
            unix_time_origin_ns: DocumentUnixTime::default(),
        });
        let mut scheduler = TimerScheduler::with_clock(clock.clone());
        scheduler
            .try_schedule_timer(TimerEventRequest {
                callback: Box::new(|| {}),
                duration: std::time::Duration::from_nanos(23),
            })
            .unwrap();

        let execution = DocumentExecutionObservation {
            clock_id: clock.id(),
            limits: DocumentExecutionLimits::CONTROLLED_WEBAPP_V1,
            counters: DocumentExecutionCounters::default(),
            terminal: None,
        };
        let observation = capture_barrier_observation(
            event_loop_id,
            &clock,
            &scheduler,
            None,
            PendingInputBarrierFacts {
                revision: embedder_traits::document_pending::PendingInputRevision::new(9),
                revision_exhausted: false,
                ready_events: 2,
                intake_saturated: true,
                tasks: TaskQueueObservation {
                    ready: 3,
                    throttled: 4,
                    inactive: 5,
                },
            },
            MicrotaskQueueObservation {
                queued_count: 6,
                checkpoint_in_progress: false,
                completed_checkpoint_generation: 7,
                terminal_error: None,
            },
            execution,
        )
        .unwrap();

        assert_eq!(observation.clock.now, DocumentTime::from_nanos(17));
        assert_eq!(
            observation.scheduler.next_deadline.unwrap().deadline,
            DocumentTime::from_nanos(40)
        );
        assert_eq!(observation.input.ready_events, 2);
        assert_eq!(observation.input.tasks.ready, 3);
        assert_eq!(observation.input.tasks.throttled, 4);
        assert_eq!(observation.input.tasks.inactive, 5);
        assert!(observation.input.intake_saturated);
        assert_eq!(observation.microtasks.queued, 6);
        assert_eq!(observation.microtasks.completed_checkpoint.get(), 7);
        assert_eq!(observation.execution, execution);
    }

    #[test]
    fn realtime_capture_fails_closed() {
        let event_loop_id = ScriptEventLoopId::new();
        let clock = DocumentClock::default();
        let scheduler = TimerScheduler::with_clock(clock.clone());
        assert_eq!(
            capture_barrier_observation(
                event_loop_id,
                &clock,
                &scheduler,
                None,
                PendingInputBarrierFacts {
                    revision: embedder_traits::document_pending::PendingInputRevision::ZERO,
                    revision_exhausted: false,
                    ready_events: 0,
                    intake_saturated: false,
                    tasks: TaskQueueObservation::default(),
                },
                MicrotaskQueueObservation {
                    queued_count: 0,
                    checkpoint_in_progress: false,
                    completed_checkpoint_generation: 0,
                    terminal_error: None,
                },
                DocumentExecutionObservation {
                    clock_id: clock.id(),
                    limits: DocumentExecutionLimits::CONTROLLED_WEBAPP_V1,
                    counters: DocumentExecutionCounters::default(),
                    terminal: None,
                },
            ),
            Err(DocumentControlError::NotControlled)
        );
    }
}
