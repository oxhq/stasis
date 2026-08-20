/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Same-build, engine-neutral observations of document work that can still affect page state.
//!
//! These types deliberately preserve exact scheduler and producer-fence evidence for trusted
//! Servo control messages. They are not a product protocol: embedders must project them into a
//! versioned wire representation rather than serialize them directly to clients.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use servo_base::Epoch;
use servo_base::id::{PipelineId, ScriptEventLoopId, WebViewId};
use timers::{
    DocumentClockError, DocumentClockId, DocumentProducerCheckpoint, DocumentProducerFenceError,
    DocumentProducerFenceId, DocumentProducerKind, DocumentProducerObservation,
    DocumentProducerSnapshot, DocumentTime, DocumentTimeSurface, DocumentUnixTime,
    TimerControlError, TimerDeadlineSnapshot, TimerSchedulerId,
};

/// Checked Constellation revision for active and pending top-level navigation membership.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct PendingNavigationRevision(u64);

impl PendingNavigationRevision {
    /// The revision before Constellation has recorded a navigation membership change.
    pub const ZERO: Self = Self(0);

    /// Construct a revision from a checked Constellation sequence.
    #[doc(hidden)]
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Return the underlying Constellation sequence.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advance without allowing a revision to wrap and alias older navigation state.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// Active top-level pipeline and the exact Constellation epoch that selected it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingActiveTopLevelPipeline {
    /// Active top-level pipeline identity.
    pub pipeline_id: PipelineId,
    /// Active-pipeline epoch observed atomically with `pipeline_id`.
    pub epoch: Epoch,
}

/// Immutable identity and pipeline membership bound to one pending-state observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingTargetObservation {
    /// WebView whose state is being observed.
    pub webview_id: WebViewId,
    /// Script event loop which owns every pipeline in this target.
    pub event_loop_id: ScriptEventLoopId,
    /// Active top-level pipeline, or `None` before the first activation or after closure.
    pub active_top_level: Option<PendingActiveTopLevelPipeline>,
    /// Checked revision covering active and pending top-level navigation membership.
    pub navigation_revision: PendingNavigationRevision,
    pipelines: Vec<PipelineId>,
    fully_active_pipelines: Vec<PipelineId>,
    pending_top_level_pipelines: Vec<PipelineId>,
}

impl PendingTargetObservation {
    /// Canonicalize pipeline membership and validate the active top-level authority.
    pub fn new(
        webview_id: WebViewId,
        event_loop_id: ScriptEventLoopId,
        active_top_level: Option<PendingActiveTopLevelPipeline>,
        navigation_revision: PendingNavigationRevision,
        mut pipelines: Vec<PipelineId>,
        mut fully_active_pipelines: Vec<PipelineId>,
        mut pending_top_level_pipelines: Vec<PipelineId>,
    ) -> Result<Self, PendingSnapshotInvariantError> {
        pipelines.sort_unstable();
        fully_active_pipelines.sort_unstable();
        pending_top_level_pipelines.sort_unstable();
        let target = Self {
            webview_id,
            event_loop_id,
            active_top_level,
            navigation_revision,
            pipelines,
            fully_active_pipelines,
            pending_top_level_pipelines,
        };
        target.validate()?;
        Ok(target)
    }

    /// Return all pipelines bound to this exact event-loop target in canonical order.
    pub fn pipelines(&self) -> &[PipelineId] {
        &self.pipelines
    }

    /// Return the fully-active subset in canonical order.
    pub fn fully_active_pipelines(&self) -> &[PipelineId] {
        &self.fully_active_pipelines
    }

    /// Return pending top-level navigation pipelines in canonical order.
    pub fn pending_top_level_pipelines(&self) -> &[PipelineId] {
        &self.pending_top_level_pipelines
    }

    /// Return whether a pipeline belongs to this immutable target.
    pub fn contains_pipeline(&self, pipeline_id: PipelineId) -> bool {
        self.pipelines.binary_search(&pipeline_id).is_ok()
    }

    fn validate(&self) -> Result<(), PendingSnapshotInvariantError> {
        validate_canonical_pipeline_membership(
            &self.pipelines,
            PendingSnapshotInvariantError::DuplicateTargetPipeline,
            PendingSnapshotInvariantError::NonCanonicalTargetPipelines,
        )?;
        validate_canonical_pipeline_membership(
            &self.fully_active_pipelines,
            PendingSnapshotInvariantError::DuplicateFullyActivePipeline,
            PendingSnapshotInvariantError::NonCanonicalFullyActivePipelines,
        )?;
        validate_canonical_pipeline_membership(
            &self.pending_top_level_pipelines,
            PendingSnapshotInvariantError::DuplicatePendingTopLevelPipeline,
            PendingSnapshotInvariantError::NonCanonicalPendingTopLevelPipelines,
        )?;
        match self.active_top_level {
            Some(active) => {
                if !self.contains_pipeline(active.pipeline_id) {
                    return Err(PendingSnapshotInvariantError::ActivePipelineOutsideTarget(
                        active.pipeline_id,
                    ));
                }
                if self
                    .fully_active_pipelines
                    .binary_search(&active.pipeline_id)
                    .is_err()
                {
                    return Err(PendingSnapshotInvariantError::ActivePipelineNotFullyActive(
                        active.pipeline_id,
                    ));
                }
            },
            None if !self.fully_active_pipelines.is_empty() => {
                return Err(
                    PendingSnapshotInvariantError::FullyActivePipelineWithoutActiveTopLevel(
                        self.fully_active_pipelines[0],
                    ),
                );
            },
            None => {},
        }
        if let Some(pipeline_id) = self
            .fully_active_pipelines
            .iter()
            .find(|pipeline_id| !self.contains_pipeline(**pipeline_id))
        {
            return Err(
                PendingSnapshotInvariantError::FullyActivePipelineOutsideTarget(*pipeline_id),
            );
        }
        if let Some(pipeline_id) = self
            .pending_top_level_pipelines
            .iter()
            .find(|pipeline_id| !self.contains_pipeline(**pipeline_id))
        {
            return Err(PendingSnapshotInvariantError::PendingPipelineOutsideTarget(
                *pipeline_id,
            ));
        }
        Ok(())
    }
}

fn validate_canonical_pipeline_membership(
    pipelines: &[PipelineId],
    duplicate_error: impl FnOnce(PipelineId) -> PendingSnapshotInvariantError,
    noncanonical_error: PendingSnapshotInvariantError,
) -> Result<(), PendingSnapshotInvariantError> {
    if pipelines.windows(2).all(|pair| pair[0] < pair[1]) {
        return Ok(());
    }
    if let Some(pipeline_id) = pipelines
        .windows(2)
        .find(|pair| pair[0] == pair[1])
        .map(|pair| pair[0])
    {
        return Err(duplicate_error(pipeline_id));
    }
    Err(noncanonical_error)
}

/// Monotonic identity for a complete pending-state observation.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct RuntimeStateGeneration(u64);

impl RuntimeStateGeneration {
    /// The generation before any observable state transition has been recorded.
    pub const ZERO: Self = Self(0);

    /// Construct a generation from a checked runtime sequence.
    #[doc(hidden)]
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Return the underlying runtime sequence.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advance without allowing a generation to wrap and alias older evidence.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// Monotonic generation of semantic DOM mutations.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct DomEpoch(u64);

impl DomEpoch {
    /// The epoch before any semantic DOM mutation has been recorded.
    pub const ZERO: Self = Self(0);

    /// Construct an epoch from a checked mutation sequence.
    #[doc(hidden)]
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Return the underlying mutation sequence.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advance without allowing an epoch to wrap and alias older DOM state.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// Monotonic revision of ordinary input admitted to one controlled event loop.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct PendingInputRevision(u64);

impl PendingInputRevision {
    /// The revision before any input has been admitted.
    pub const ZERO: Self = Self(0);

    /// Construct a revision from a checked input sequence.
    #[doc(hidden)]
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Return the underlying input sequence.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advance without allowing a revision to wrap and alias older input evidence.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// Monotonic identity of one canonical source inventory.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct PendingSourceEpoch(u64);

impl PendingSourceEpoch {
    /// The epoch before any source inventory has been recorded.
    pub const ZERO: Self = Self(0);

    /// Construct an epoch from a checked source-inventory sequence.
    #[doc(hidden)]
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Return the underlying source-inventory sequence.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advance without allowing an epoch to wrap and alias an older inventory.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// Whether a document clock advances from the host clock or only through explicit control.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u8)]
pub enum PendingClockMode {
    /// Time follows elapsed host monotonic time.
    Realtime = 0,
    /// Time advances only through the controlled scheduler.
    Controlled = 1,
}

/// Exact document-clock evidence captured with a pending-state observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingClockObservation {
    /// Process-local identity shared by every consumer of this clock domain.
    pub clock_id: DocumentClockId,
    /// Whether the clock is host-driven or controlled.
    pub mode: PendingClockMode,
    /// Current integer-nanosecond offset in the document clock.
    pub now: DocumentTime,
    /// First controlled-time surface touched without complete clock integration, if any.
    pub unsupported_surface: Option<DocumentTimeSurface>,
}

/// Exact scheduler-head evidence captured with a pending-state observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingSchedulerObservation {
    /// Process-local identity of the outer scheduler which produced this observation.
    ///
    /// A guarded advance must validate this together with clock and target even when the queue is
    /// empty and therefore has no deadline snapshot carrying the identity.
    pub scheduler_id: TimerSchedulerId,
    /// Next exact controlled scheduler entry within `scheduler_id`.
    ///
    /// [`TimerDeadlineSnapshot`] alone is not advance authority because timer IDs are only unique
    /// within one scheduler. Neither identity may be forwarded by a wire projection.
    pub next_deadline: Option<TimerDeadlineSnapshot>,
}

/// A failure which the controlled document clock itself retains permanently.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PendingClockTerminal {
    /// An integer nanosecond conversion or calculation overflowed.
    Overflow,
    /// SpiderMonkey's floating-point Date hook could not preserve an exact TimeClip result.
    JavaScriptDatePrecisionLoss {
        /// Exact signed wall time that could not be transported without rounding.
        unix_time: DocumentUnixTime,
        /// Millisecond produced by exact TimeClip truncation.
        expected_milliseconds: i128,
        /// Millisecond produced after floating-point callback transport and truncation.
        observed_milliseconds: i128,
    },
}

/// Sticky document-clock terminal bound to the clock that produced it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingClockTerminalObservation {
    /// Exact clock identity.
    pub clock_id: DocumentClockId,
    /// First sticky clock failure.
    pub error: PendingClockTerminal,
}

/// Sticky outer-scheduler terminal bound to one event loop and scheduler identity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingOuterSchedulerTerminalObservation {
    /// Event loop which owns this outer scheduler.
    pub event_loop_id: ScriptEventLoopId,
    /// Scheduler identity whose first failure is retained.
    pub scheduler_id: TimerSchedulerId,
    /// First sticky outer-scheduler failure.
    pub error: TimerControlError,
}

/// Sticky producer-fence terminal bound to its exact fence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingProducerTerminalObservation {
    /// Producer fence which latched the failure.
    pub fence_id: DocumentProducerFenceId,
    /// First sticky producer-lifecycle failure.
    pub error: DocumentProducerFenceError,
}

/// Sticky microtask terminal bound to its owning event loop.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingMicrotaskTerminalObservation {
    /// Event loop whose microtask queue cannot complete another checkpoint.
    pub event_loop_id: ScriptEventLoopId,
    /// First sticky checkpoint failure.
    pub error: PendingMicrotaskTerminal,
}

/// Sticky logical DOM-timer terminal for one pipeline-owned Window timer queue.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingLogicalTimerTerminalObservation {
    /// Pipeline whose Window timer queue latched the failure.
    pub pipeline_id: PipelineId,
    /// First checked document-clock failure retained by that queue.
    pub error: DocumentClockError,
}

/// Sticky animated-image scheduler terminal for one pipeline-owned document.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingImageTimerTerminalObservation {
    /// Pipeline whose image-animation manager latched the failure.
    pub pipeline_id: PipelineId,
    /// First checked outer-scheduler failure retained by that manager.
    pub error: TimerControlError,
}

/// Sticky failure of a checked state or DOM generation counter.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u8)]
pub enum PendingGenerationTerminal {
    /// The generation cannot represent another distinct value.
    Exhausted = 0,
}

/// Generation terminal bound to the WebView whose generation stopped advancing.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingGenerationTerminalObservation {
    /// WebView whose counter latched the failure.
    pub webview_id: WebViewId,
    /// First sticky generation failure.
    pub error: PendingGenerationTerminal,
}

/// Additive sticky failures retained by every independent runtime owner.
///
/// Fixed owner slots and canonical per-pipeline vectors prevent one later failure from erasing a
/// different terminal observed in the same snapshot.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingRuntimeTerminals {
    /// Shared document-clock terminal.
    pub clock: Option<PendingClockTerminalObservation>,
    /// ScriptThread outer-scheduler terminal.
    pub outer_scheduler: Option<PendingOuterSchedulerTerminalObservation>,
    /// Producer-fence terminal.
    pub producer: Option<PendingProducerTerminalObservation>,
    /// Event-loop microtask-checkpoint terminal.
    pub microtask: Option<PendingMicrotaskTerminalObservation>,
    logical_timers: Vec<PendingLogicalTimerTerminalObservation>,
    image_timers: Vec<PendingImageTimerTerminalObservation>,
    /// Semantic DOM-generation terminal.
    pub dom_generation: Option<PendingGenerationTerminalObservation>,
    /// Complete runtime-state-generation terminal.
    pub state_generation: Option<PendingGenerationTerminalObservation>,
    /// Constellation navigation-revision terminal.
    pub navigation_revision: Option<PendingGenerationTerminalObservation>,
}

impl PendingRuntimeTerminals {
    /// Canonicalize per-pipeline terminal evidence without coalescing independent owners.
    pub fn new(
        mut logical_timers: Vec<PendingLogicalTimerTerminalObservation>,
        mut image_timers: Vec<PendingImageTimerTerminalObservation>,
    ) -> Result<Self, PendingSnapshotInvariantError> {
        logical_timers.sort_unstable_by_key(|terminal| terminal.pipeline_id);
        image_timers.sort_unstable_by_key(|terminal| terminal.pipeline_id);
        validate_unique_terminal_pipelines(
            &logical_timers,
            |terminal| terminal.pipeline_id,
            PendingSnapshotInvariantError::DuplicateLogicalTimerTerminal,
        )?;
        validate_unique_terminal_pipelines(
            &image_timers,
            |terminal| terminal.pipeline_id,
            PendingSnapshotInvariantError::DuplicateImageTimerTerminal,
        )?;
        Ok(Self {
            logical_timers,
            image_timers,
            ..Self::default()
        })
    }

    /// Return logical-timer terminals in canonical pipeline order.
    pub fn logical_timers(&self) -> &[PendingLogicalTimerTerminalObservation] {
        &self.logical_timers
    }

    /// Return animated-image timer terminals in canonical pipeline order.
    pub fn image_timers(&self) -> &[PendingImageTimerTerminalObservation] {
        &self.image_timers
    }

    /// Return whether no independent runtime owner has latched a terminal.
    pub fn is_empty(&self) -> bool {
        self.clock.is_none()
            && self.outer_scheduler.is_none()
            && self.producer.is_none()
            && self.microtask.is_none()
            && self.logical_timers.is_empty()
            && self.image_timers.is_empty()
            && self.dom_generation.is_none()
            && self.state_generation.is_none()
            && self.navigation_revision.is_none()
    }

    fn validate(&self) -> Result<(), PendingSnapshotInvariantError> {
        validate_canonical_terminal_pipelines(
            &self.logical_timers,
            |terminal| terminal.pipeline_id,
            PendingSnapshotInvariantError::DuplicateLogicalTimerTerminal,
            PendingSnapshotInvariantError::NonCanonicalLogicalTimerTerminals,
        )?;
        validate_canonical_terminal_pipelines(
            &self.image_timers,
            |terminal| terminal.pipeline_id,
            PendingSnapshotInvariantError::DuplicateImageTimerTerminal,
            PendingSnapshotInvariantError::NonCanonicalImageTimerTerminals,
        )
    }
}

fn validate_unique_terminal_pipelines<T>(
    terminals: &[T],
    pipeline_id: impl Fn(&T) -> PipelineId,
    duplicate_error: impl FnOnce(PipelineId) -> PendingSnapshotInvariantError,
) -> Result<(), PendingSnapshotInvariantError> {
    if let Some(pipeline_id) = terminals
        .windows(2)
        .find(|pair| pipeline_id(&pair[0]) == pipeline_id(&pair[1]))
        .map(|pair| pipeline_id(&pair[0]))
    {
        return Err(duplicate_error(pipeline_id));
    }
    Ok(())
}

fn validate_canonical_terminal_pipelines<T>(
    terminals: &[T],
    pipeline_id: impl Fn(&T) -> PipelineId + Copy,
    duplicate_error: impl FnOnce(PipelineId) -> PendingSnapshotInvariantError,
    noncanonical_error: PendingSnapshotInvariantError,
) -> Result<(), PendingSnapshotInvariantError> {
    if terminals
        .windows(2)
        .all(|pair| pipeline_id(&pair[0]) < pipeline_id(&pair[1]))
    {
        return Ok(());
    }
    validate_unique_terminal_pipelines(terminals, pipeline_id, duplicate_error)?;
    Err(noncanonical_error)
}

/// Counts from the ordinary task queue, before settlement policy classifies them.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingTaskObservation {
    /// Tasks ready for an event-loop turn.
    pub ready: u64,
    /// Tasks retained by queue throttling.
    pub throttled: u64,
    /// Tasks retained for inactive documents.
    pub inactive: u64,
}

impl PendingTaskObservation {
    /// Return the sum of all queue classes without silently wrapping a diagnostic count.
    pub const fn checked_total(self) -> Option<u64> {
        match self.ready.checked_add(self.throttled) {
            Some(partial) => partial.checked_add(self.inactive),
            None => None,
        }
    }
}

/// Ordinary event-loop input and queue counts captured before a controlled turn.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingInputObservation {
    /// Revision changed whenever ordinary input is committed to the controlled queue.
    pub revision: PendingInputRevision,
    /// Ready ordinary events, excluding control requests themselves.
    pub ready_events: u64,
    /// Whether a bounded intake pass filled completely and therefore may have unseen input.
    pub intake_saturated: bool,
    /// Raw task-queue classes observed in the same event-loop state.
    pub tasks: PendingTaskObservation,
}

/// Monotonic generation of fully completed microtask checkpoints.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct PendingMicrotaskCheckpoint(u64);

impl PendingMicrotaskCheckpoint {
    /// The generation before any microtask checkpoint has fully completed.
    pub const ZERO: Self = Self(0);

    /// Construct a generation from the microtask queue's checked sequence.
    #[doc(hidden)]
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Return the underlying microtask checkpoint sequence.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advance without allowing a checkpoint generation to wrap.
    pub const fn checked_next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(next) => Some(Self(next)),
            None => None,
        }
    }
}

/// Sticky failure that prevents a microtask queue from completing more checkpoints.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u8)]
pub enum PendingMicrotaskTerminal {
    /// The completed-checkpoint generation cannot represent another checkpoint.
    CheckpointGenerationExhausted = 0,
}

/// Microtask queue and checkpoint evidence captured after an event-loop observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingMicrotaskObservation {
    /// Event loop which owns this main microtask queue.
    pub event_loop_id: ScriptEventLoopId,
    /// Jobs currently queued for a microtask checkpoint.
    pub queued: u64,
    /// Last fully completed checkpoint of this microtask queue.
    ///
    /// This generation is intentionally distinct from [`DocumentProducerCheckpoint`]. The
    /// integration layer explicitly advances producer qualification only after converting a
    /// completed microtask checkpoint at the event-loop boundary.
    pub completed_checkpoint: PendingMicrotaskCheckpoint,
    /// Whether a checkpoint is already executing and cannot be recursively entered.
    pub checkpoint_in_progress: bool,
    /// First sticky failure preventing another checkpoint, if any.
    pub terminal: Option<PendingMicrotaskTerminal>,
}

impl PendingMicrotaskObservation {
    /// Validate sticky-terminal evidence independently of settlement policy.
    pub fn validate(self) -> Result<(), PendingSnapshotInvariantError> {
        if self.terminal.is_none() {
            return Ok(());
        }
        if self.completed_checkpoint.get() != u64::MAX {
            return Err(PendingSnapshotInvariantError::MicrotaskTerminalBeforeExhaustion);
        }
        if self.checkpoint_in_progress {
            return Err(PendingSnapshotInvariantError::MicrotaskTerminalDuringCheckpoint);
        }
        Ok(())
    }
}

/// Mechanical qualification of one producer-fence snapshot.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u8)]
pub enum PendingProducerStability {
    /// No event-loop microtask checkpoint has completed yet.
    NotCheckpointed = 0,
    /// At least one producer ticket is live.
    Busy = 1,
    /// One fresh checkpoint observed this exact empty producer revision.
    FirstEmpty = 2,
    /// Two fresh checkpoints observed this exact empty producer revision.
    StableEmpty = 3,
}

/// Earlier empty qualification required to prove a second stable-empty boundary.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingProducerPriorEmptyQualification {
    /// Earlier completed microtask boundary which observed the same empty revision.
    pub microtask_checkpoint: PendingMicrotaskCheckpoint,
    /// Earlier producer-observer checkpoint.
    pub checkpoint: DocumentProducerCheckpoint,
    /// Exact empty producer-fence revision seen at that earlier boundary.
    pub snapshot_revision: u64,
}

/// Producer watermarks bound to the checkpoint that qualified them.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingProducerObservation {
    /// Event loop whose producer callbacks and terminal tasks this fence covers.
    pub event_loop_id: ScriptEventLoopId,
    /// Explicit producer-fence owner identity, duplicated for anti-splicing validation.
    pub fence_id: DocumentProducerFenceId,
    /// Completed microtask checkpoint boundary which yielded this producer qualification.
    pub microtask_checkpoint: PendingMicrotaskCheckpoint,
    /// Last fully completed event-loop checkpoint used for qualification.
    pub checkpoint: DocumentProducerCheckpoint,
    /// One mutex-consistent producer-fence snapshot.
    pub snapshot: DocumentProducerSnapshot,
    /// Mechanical stability classification for `snapshot`.
    pub stability: PendingProducerStability,
    /// Earlier same-revision empty boundary, present exactly for `StableEmpty`.
    pub prior_empty: Option<PendingProducerPriorEmptyQualification>,
}

impl PendingProducerObservation {
    /// Construct producer evidence after validating checkpoint and emptiness relationships.
    pub fn new(
        event_loop_id: ScriptEventLoopId,
        microtask_checkpoint: PendingMicrotaskCheckpoint,
        checkpoint: DocumentProducerCheckpoint,
        snapshot: DocumentProducerSnapshot,
        stability: PendingProducerStability,
        prior_empty: Option<PendingProducerPriorEmptyQualification>,
    ) -> Result<Self, PendingSnapshotInvariantError> {
        let observation = Self {
            event_loop_id,
            fence_id: snapshot.fence_id(),
            microtask_checkpoint,
            checkpoint,
            snapshot,
            stability,
            prior_empty,
        };
        observation.validate()?;
        Ok(observation)
    }

    /// Convert the timer crate's qualified observation without weakening its exact snapshot.
    pub fn from_observation(
        event_loop_id: ScriptEventLoopId,
        microtask_checkpoint: PendingMicrotaskCheckpoint,
        checkpoint: DocumentProducerCheckpoint,
        observation: DocumentProducerObservation,
        prior_empty: Option<PendingProducerPriorEmptyQualification>,
    ) -> Result<Self, PendingSnapshotInvariantError> {
        let (snapshot, stability) = match observation {
            DocumentProducerObservation::Busy(snapshot) => {
                (snapshot, PendingProducerStability::Busy)
            },
            DocumentProducerObservation::FirstEmpty(snapshot) => {
                (snapshot, PendingProducerStability::FirstEmpty)
            },
            DocumentProducerObservation::StableEmpty(snapshot) => {
                (snapshot, PendingProducerStability::StableEmpty)
            },
        };
        Self::new(
            event_loop_id,
            microtask_checkpoint,
            checkpoint,
            snapshot,
            stability,
            prior_empty,
        )
    }

    /// Revalidate evidence which may have crossed a same-build serialization boundary.
    pub fn validate(self) -> Result<(), PendingSnapshotInvariantError> {
        if self.fence_id != self.snapshot.fence_id() {
            return Err(
                PendingSnapshotInvariantError::ProducerFenceIdentityMismatch {
                    observed: self.fence_id,
                    snapshot: self.snapshot.fence_id(),
                },
            );
        }
        validate_producer_snapshot_conservation(self.snapshot)?;
        match (self.stability, self.prior_empty) {
            (PendingProducerStability::StableEmpty, None) => {
                return Err(PendingSnapshotInvariantError::ProducerPriorEmptyMissing);
            },
            (PendingProducerStability::StableEmpty, Some(prior)) => {
                if prior.microtask_checkpoint == PendingMicrotaskCheckpoint::ZERO
                    || prior.microtask_checkpoint >= self.microtask_checkpoint
                    || prior.checkpoint == DocumentProducerCheckpoint::ZERO
                    || prior.checkpoint >= self.checkpoint
                {
                    return Err(
                        PendingSnapshotInvariantError::ProducerPriorEmptyCheckpointMismatch,
                    );
                }
                if prior.snapshot_revision != self.snapshot.revision() {
                    return Err(
                        PendingSnapshotInvariantError::ProducerPriorEmptyRevisionMismatch {
                            prior: prior.snapshot_revision,
                            current: self.snapshot.revision(),
                        },
                    );
                }
            },
            (_, Some(_)) => {
                return Err(PendingSnapshotInvariantError::ProducerPriorEmptyUnexpected);
            },
            (_, None) => {},
        }
        match self.stability {
            PendingProducerStability::NotCheckpointed => {
                if self.checkpoint != DocumentProducerCheckpoint::ZERO
                    || self.microtask_checkpoint != PendingMicrotaskCheckpoint::ZERO
                {
                    return Err(PendingSnapshotInvariantError::ProducerCheckpointMismatch {
                        microtask_checkpoint: self.microtask_checkpoint,
                        checkpoint: self.checkpoint,
                        stability: self.stability,
                    });
                }
            },
            PendingProducerStability::Busy => {
                if self.checkpoint == DocumentProducerCheckpoint::ZERO
                    || self.microtask_checkpoint == PendingMicrotaskCheckpoint::ZERO
                {
                    return Err(PendingSnapshotInvariantError::ProducerCheckpointMismatch {
                        microtask_checkpoint: self.microtask_checkpoint,
                        checkpoint: self.checkpoint,
                        stability: self.stability,
                    });
                }
                if self.snapshot.is_empty() {
                    return Err(PendingSnapshotInvariantError::ProducerEmptinessMismatch {
                        stability: self.stability,
                        pending: self.snapshot.pending(),
                    });
                }
            },
            PendingProducerStability::FirstEmpty | PendingProducerStability::StableEmpty => {
                if self.checkpoint == DocumentProducerCheckpoint::ZERO
                    || self.microtask_checkpoint == PendingMicrotaskCheckpoint::ZERO
                {
                    return Err(PendingSnapshotInvariantError::ProducerCheckpointMismatch {
                        microtask_checkpoint: self.microtask_checkpoint,
                        checkpoint: self.checkpoint,
                        stability: self.stability,
                    });
                }
                if !self.snapshot.is_empty() {
                    return Err(PendingSnapshotInvariantError::ProducerEmptinessMismatch {
                        stability: self.stability,
                        pending: self.snapshot.pending(),
                    });
                }
            },
        }
        Ok(())
    }
}

fn validate_producer_snapshot_conservation(
    snapshot: DocumentProducerSnapshot,
) -> Result<(), PendingSnapshotInvariantError> {
    let mut summed_enqueued = 0_u64;
    let mut summed_completed = 0_u64;
    let mut summed_pending = 0_u64;
    for kind in [
        DocumentProducerKind::Task,
        DocumentProducerKind::Resource,
        DocumentProducerKind::Font,
        DocumentProducerKind::Image,
        DocumentProducerKind::ExternalCallback,
    ] {
        let watermark = snapshot.for_kind(kind);
        if watermark.completed().checked_add(watermark.pending()) != Some(watermark.enqueued()) {
            return Err(PendingSnapshotInvariantError::ProducerKindConservationMismatch { kind });
        }
        summed_enqueued = summed_enqueued
            .checked_add(watermark.enqueued())
            .ok_or(PendingSnapshotInvariantError::ProducerConservationOverflow)?;
        summed_completed = summed_completed
            .checked_add(watermark.completed())
            .ok_or(PendingSnapshotInvariantError::ProducerConservationOverflow)?;
        summed_pending = summed_pending
            .checked_add(watermark.pending())
            .ok_or(PendingSnapshotInvariantError::ProducerConservationOverflow)?;
    }
    if snapshot.completed().checked_add(snapshot.pending()) != Some(snapshot.enqueued()) {
        return Err(PendingSnapshotInvariantError::ProducerGlobalConservationMismatch);
    }
    if (summed_enqueued, summed_completed, summed_pending)
        != (
            snapshot.enqueued(),
            snapshot.completed(),
            snapshot.pending(),
        )
    {
        return Err(PendingSnapshotInvariantError::ProducerWatermarkSumMismatch);
    }
    let expected_revision = snapshot
        .enqueued()
        .checked_add(snapshot.completed())
        .ok_or(PendingSnapshotInvariantError::ProducerConservationOverflow)?;
    if snapshot.revision() != expected_revision {
        return Err(PendingSnapshotInvariantError::ProducerRevisionMismatch {
            observed: snapshot.revision(),
            expected: expected_revision,
        });
    }
    Ok(())
}

/// Parser or top-level navigation owner represented by one authoritative source entry.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(u8)]
pub enum PendingParserSourceKind {
    /// A live document parser which has neither stopped nor aborted.
    DocumentParser = 0,
    /// A deduped, target-filtered top-level navigation.
    TopLevelNavigation = 1,
}

/// Mechanical phase of authoritative parser or top-level navigation work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(u8)]
pub enum PendingParserPhase {
    /// Parser or navigation work is runnable on an event-loop turn.
    Ready = 0,
    /// Progress requires more externally delivered input.
    AwaitingExternalInput = 1,
    /// Response/parser work is complete and a navigation commit remains runnable.
    AwaitingCommit = 2,
}

/// One identity-bearing authoritative parser or top-level navigation source.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingParserSourceObservation {
    /// Stable identity shared with the canonical source inventory.
    pub source_id: PendingSourceId,
    /// Target-filtered owning pipeline.
    pub pipeline_id: PipelineId,
    /// Parser or navigation owner class.
    pub kind: PendingParserSourceKind,
    /// Mechanical lifecycle phase.
    pub phase: PendingParserPhase,
    /// Disposition duplicated exactly in the canonical source inventory.
    pub disposition: PendingSourceDisposition,
}

impl PendingParserSourceObservation {
    fn validate(self) -> Result<(), PendingSnapshotInvariantError> {
        let disposition_matches = match self.phase {
            PendingParserPhase::Ready | PendingParserPhase::AwaitingCommit => {
                self.disposition == PendingSourceDisposition::Ready
            },
            PendingParserPhase::AwaitingExternalInput => matches!(
                self.disposition,
                PendingSourceDisposition::AwaitingExternalIo(_)
            ),
        };
        if !disposition_matches {
            return Err(
                PendingSnapshotInvariantError::ParserPhaseDispositionMismatch(self.source_id),
            );
        }
        Ok(())
    }
}

/// Canonical authoritative parser and top-level navigation source inventory.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingParserObservation {
    sources: Vec<PendingParserSourceObservation>,
}

impl PendingParserObservation {
    /// Canonicalize parser/navigation sources and reject duplicate source identities.
    ///
    /// Integration must collect live document parsers and deduped, target-filtered Constellation
    /// navigation contexts. ScriptThread's `incomplete_loads` collection is deliberately not
    /// represented because it overlaps other facts and can retain stale entries.
    pub fn new(
        mut sources: Vec<PendingParserSourceObservation>,
    ) -> Result<Self, PendingSnapshotInvariantError> {
        sources.sort_unstable_by_key(|source| source.source_id);
        if let Some(source_id) = sources
            .windows(2)
            .find(|pair| pair[0].source_id == pair[1].source_id)
            .map(|pair| pair[0].source_id)
        {
            return Err(PendingSnapshotInvariantError::DuplicateParserSource(
                source_id,
            ));
        }
        for source in &sources {
            source.validate()?;
        }
        Ok(Self { sources })
    }

    /// Return authoritative parser/navigation sources in canonical identity order.
    pub fn sources(&self) -> &[PendingParserSourceObservation] {
        &self.sources
    }

    fn validate(&self) -> Result<(), PendingSnapshotInvariantError> {
        if !self
            .sources
            .windows(2)
            .all(|pair| pair[0].source_id < pair[1].source_id)
        {
            if let Some(source_id) = self
                .sources
                .windows(2)
                .find(|pair| pair[0].source_id == pair[1].source_id)
                .map(|pair| pair[0].source_id)
            {
                return Err(PendingSnapshotInvariantError::DuplicateParserSource(
                    source_id,
                ));
            }
            return Err(PendingSnapshotInvariantError::NonCanonicalParserSources);
        }
        for source in &self.sources {
            source.validate()?;
        }
        Ok(())
    }

    fn get(&self, source_id: PendingSourceId) -> Option<&PendingParserSourceObservation> {
        self.sources
            .binary_search_by_key(&source_id, |source| source.source_id)
            .ok()
            .map(|index| &self.sources[index])
    }
}

/// Stable, snapshot-local identity for a source that can produce more document work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct PendingSourceId(u64);

impl PendingSourceId {
    /// Construct an identity from a checked event-loop-owned sequence.
    #[doc(hidden)]
    pub const fn new(sequence: u64) -> Self {
        Self(sequence)
    }

    /// Return the event-loop-owned sequence.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Network operation class retained while external I/O can produce document work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(u8)]
pub enum PendingNetworkKind {
    /// Top-level or same-event-loop document navigation.
    Navigation = 0,
    /// Fetch API request.
    Fetch = 1,
    /// XMLHttpRequest request.
    XmlHttpRequest = 2,
    /// Image resource request.
    Image = 3,
    /// Web-font resource request.
    Font = 4,
    /// Stylesheet resource request.
    Stylesheet = 5,
    /// Script resource request.
    Script = 6,
    /// A resource class not yet distinguished by the observer.
    Other = 7,
}

/// Lifecycle phase of an active external network operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(u8)]
pub enum PendingExternalIoPhase {
    /// The operation is registered but has not completed dispatch.
    Queued = 0,
    /// The runtime is awaiting response headers or a terminal network error.
    AwaitingResponse = 1,
    /// A response body is still streaming.
    StreamingBody = 2,
    /// Terminal delivery is queued but has not yet executed on the event loop.
    TerminalTaskQueued = 3,
}

/// Mechanical owner that initiated an external-I/O operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(u8)]
pub enum PendingExternalIoOwner {
    /// Constellation initiated a top-level navigation.
    TopLevelNavigation = 0,
    /// A live document parser initiated the operation.
    DocumentParser = 1,
    /// Script initiated the operation directly.
    Script = 2,
    /// A document-owned subresource loader initiated the operation.
    DocumentSubresource = 3,
    /// A rendering-owned font, image, or graphics loader initiated the operation.
    RenderingResource = 4,
    /// The observer cannot yet assign a narrower mechanical owner.
    Other = 5,
}

/// Whether an operation mechanically participates in document load completion.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(u8)]
pub enum PendingExternalIoLoadBlocking {
    /// Document load completion is blocked while this operation remains active.
    Blocking = 0,
    /// Document load completion does not wait for this operation.
    NonBlocking = 1,
    /// The engine cannot yet prove either relationship.
    Unknown = 2,
}

/// Policy-neutral external-I/O provenance shared by network and source observations.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct PendingExternalIoEvidence {
    /// Mechanical owner observed at the operation's start site.
    pub owner: PendingExternalIoOwner,
    /// Mechanical document-load relationship.
    pub load_blocking: PendingExternalIoLoadBlocking,
}

/// One active network operation, retained through terminal event-loop handling.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct PendingExternalIoObservation {
    /// Stable source identity shared with the canonical source inventory.
    pub source_id: PendingSourceId,
    /// Pipeline which owns delivery of this operation's terminal event-loop work.
    pub pipeline_id: PipelineId,
    /// Network API or resource class.
    pub kind: PendingNetworkKind,
    /// Current delivery lifecycle phase.
    pub phase: PendingExternalIoPhase,
    /// Mechanical ownership and load-blocking evidence, before settlement policy is applied.
    pub evidence: PendingExternalIoEvidence,
    /// Document time at which this operation was registered.
    pub started_at: DocumentTime,
}

/// Canonical inventory of active network operations.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingNetworkObservation {
    active: Vec<PendingExternalIoObservation>,
}

impl PendingNetworkObservation {
    /// Canonicalize active network operations and reject duplicate source identities.
    pub fn new(
        mut active: Vec<PendingExternalIoObservation>,
    ) -> Result<Self, PendingSnapshotInvariantError> {
        active.sort_unstable_by_key(|observation| observation.source_id);
        if let Some(source_id) = duplicate_external_io_source_id(&active) {
            return Err(PendingSnapshotInvariantError::DuplicateExternalIoSource(
                source_id,
            ));
        }
        Ok(Self { active })
    }

    /// Return active operations in canonical source-identity order.
    pub fn active(&self) -> &[PendingExternalIoObservation] {
        &self.active
    }

    /// Return active operations in one mechanical document-load class.
    pub fn count_with_load_blocking(&self, load_blocking: PendingExternalIoLoadBlocking) -> u64 {
        u64::try_from(
            self.active
                .iter()
                .filter(|observation| observation.evidence.load_blocking == load_blocking)
                .count(),
        )
        .expect("an in-memory network inventory cannot exceed u64::MAX entries")
    }

    fn validate(&self) -> Result<(), PendingSnapshotInvariantError> {
        if !self
            .active
            .windows(2)
            .all(|pair| pair[0].source_id < pair[1].source_id)
        {
            if let Some(source_id) = duplicate_external_io_source_id(&self.active) {
                return Err(PendingSnapshotInvariantError::DuplicateExternalIoSource(
                    source_id,
                ));
            }
            return Err(PendingSnapshotInvariantError::NonCanonicalExternalIoInventory);
        }
        Ok(())
    }

    fn get(&self, source_id: PendingSourceId) -> Option<&PendingExternalIoObservation> {
        self.active
            .binary_search_by_key(&source_id, |observation| observation.source_id)
            .ok()
            .map(|index| &self.active[index])
    }
}

fn duplicate_external_io_source_id(
    observations: &[PendingExternalIoObservation],
) -> Option<PendingSourceId> {
    observations
        .windows(2)
        .find(|pair| pair[0].source_id == pair[1].source_id)
        .map(|pair| pair[0].source_id)
}

/// Why active animated images could not be split into finite and infinite loop classes.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u8)]
pub enum PendingAnimatedImageUnsupportedReason {
    /// Decoded image metadata did not expose an exact loop bound.
    LoopCountUnavailable = 0,
    /// Image animation advances from a timeline not yet bound to document time.
    TimelineUncontrolled = 1,
    /// The image update callback could not be joined to the exact outer scheduler queue.
    TimerBindingUnavailable = 2,
}

/// Per-reason counts for retained animated images that cannot be classified safely.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingAnimatedImageUnsupportedCounts {
    /// Images whose decoded metadata does not expose an exact loop bound.
    pub loop_count_unavailable: u64,
    /// Images which advance from a timeline not yet bound to document time.
    pub timeline_uncontrolled: u64,
    /// Images whose callback cannot be joined to an exact live scheduler entry.
    pub timer_binding_unavailable: u64,
}

impl PendingAnimatedImageUnsupportedCounts {
    /// Return the exact unsupported-image total without silently wrapping.
    pub const fn checked_total(self) -> Option<u64> {
        match self
            .loop_count_unavailable
            .checked_add(self.timeline_uncontrolled)
        {
            Some(partial) => partial.checked_add(self.timer_binding_unavailable),
            None => None,
        }
    }

    /// Return the count retained for one explicit unsupported reason.
    pub const fn count(self, reason: PendingAnimatedImageUnsupportedReason) -> u64 {
        match reason {
            PendingAnimatedImageUnsupportedReason::LoopCountUnavailable => {
                self.loop_count_unavailable
            },
            PendingAnimatedImageUnsupportedReason::TimelineUncontrolled => {
                self.timeline_uncontrolled
            },
            PendingAnimatedImageUnsupportedReason::TimerBindingUnavailable => {
                self.timer_binding_unavailable
            },
        }
    }
}

/// Exact retained animated-image classes for one document.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingAnimatedImageObservation {
    /// Images retained by the active animation manager.
    pub retained_images: u64,
    /// Retained, unfinished images with a finite remaining loop count.
    pub finite_images: u64,
    /// Retained, unfinished images with an infinite loop count.
    pub infinite_images: u64,
    /// Finished finite images which remain inertly retained by the manager.
    pub inert_images: u64,
    /// Per-reason counts for retained images not controlled by this observation.
    pub unsupported: PendingAnimatedImageUnsupportedCounts,
    /// Whether an animated-image update is ready for the next rendering turn.
    pub update_ready: bool,
    /// Exact manager callback joined to a live entry in the outer scheduler queue.
    ///
    /// The manager's retained timer ID is not enough: integration must serialize `Some` only
    /// after joining it to live scheduler membership and its exact deadline.
    pub scheduled_timer: Option<TimerDeadlineSnapshot>,
}

/// Why retained canvas or graphics state cannot be proven settled.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u8)]
pub enum PendingCanvasUnsupportedReason {
    /// No complete live-object inventory exists for document-owned graphics contexts.
    LiveSourceInventoryUnavailable = 0,
    /// An OffscreenCanvas or transferred context can mutate outside this event loop.
    OffscreenExecution = 1,
    /// Graphics mutations are not bound to the observed DOM and rendering generations.
    MutationGenerationUnbound = 2,
}

/// Per-reason counts for retained canvas/graphics contexts not controlled by this slice.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingCanvasUnsupportedCounts {
    /// Retained contexts omitted because no complete live-object inventory exists.
    pub live_source_inventory_unavailable: u64,
    /// Retained contexts which can execute through OffscreenCanvas or transfer.
    pub offscreen_execution: u64,
    /// Retained contexts not bound to DOM and rendering generations.
    pub mutation_generation_unbound: u64,
}

impl PendingCanvasUnsupportedCounts {
    /// Return the count retained for one explicit unsupported reason.
    pub const fn count(self, reason: PendingCanvasUnsupportedReason) -> u64 {
        match reason {
            PendingCanvasUnsupportedReason::LiveSourceInventoryUnavailable => {
                self.live_source_inventory_unavailable
            },
            PendingCanvasUnsupportedReason::OffscreenExecution => self.offscreen_execution,
            PendingCanvasUnsupportedReason::MutationGenerationUnbound => {
                self.mutation_generation_unbound
            },
        }
    }
}

/// Pending and unsupported canvas/graphics evidence for one document.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingCanvasObservation {
    /// Canvas contexts in the exact dirty-context list.
    pub dirty_contexts: u64,
    /// Whether an asynchronous canvas image upload is still awaited.
    pub awaiting_async_upload: bool,
    /// Per-reason fail-closed evidence for retained graphics state outside this runtime slice.
    pub unsupported: PendingCanvasUnsupportedCounts,
}

/// Mechanical lifecycle class of one pipeline's rendering facts.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u8)]
pub enum PendingRenderingPipelineActivity {
    /// The pipeline belongs to the target's fully-active set and can render normally.
    FullyActive = 0,
    /// The document is retained but inactive.
    Inactive = 1,
    /// The document is retained under an engine throttle.
    Throttled = 2,
}

/// Rendering facts for one pipeline-owned document.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingPipelineRenderingObservation {
    /// Pipeline which owns these document facts.
    pub pipeline_id: PipelineId,
    /// Mechanical target activity observed with these document facts.
    pub activity: PendingRenderingPipelineActivity,
    /// requestAnimationFrame slots retained in the document list, including canceled tombstones.
    pub retained_animation_frame_callbacks: u64,
    /// Retained requestAnimationFrame slots which still contain runnable callbacks.
    pub runnable_animation_frame_callbacks: u64,
    /// Whether this document's exact rendering predicate currently requires an update.
    pub document_update_required: bool,
    /// Animation and transition events queued for dispatch.
    pub pending_animation_events: u64,
    /// Tick-requiring CSS or Web Animations with a finite terminal opportunity.
    pub finite_animations: u64,
    /// Tick-requiring CSS or Web Animations with an infinite iteration bound.
    pub infinite_animations: u64,
    /// Tick-requiring animations which could not be classified safely.
    pub unsupported_animations: u64,
    /// Animated-image loop and callback evidence.
    pub animated_images: PendingAnimatedImageObservation,
    /// Canvas/graphics dirty and unsupported evidence.
    pub canvas: PendingCanvasObservation,
    /// Font loads which can still change layout or rendered output.
    pub pending_fonts: u64,
    /// Non-animated image updates which can still change rendered output.
    pub pending_images: u64,
}

impl PendingPipelineRenderingObservation {
    fn validate(self) -> Result<(), PendingSnapshotInvariantError> {
        if self.runnable_animation_frame_callbacks > self.retained_animation_frame_callbacks {
            return Err(
                PendingSnapshotInvariantError::RunnableAnimationFramesExceedRetained {
                    pipeline_id: self.pipeline_id,
                    retained: self.retained_animation_frame_callbacks,
                    runnable: self.runnable_animation_frame_callbacks,
                },
            );
        }
        let classified_images = self
            .animated_images
            .finite_images
            .checked_add(self.animated_images.infinite_images)
            .and_then(|count| count.checked_add(self.animated_images.inert_images))
            .and_then(|count| {
                self.animated_images
                    .unsupported
                    .checked_total()
                    .and_then(|unsupported| count.checked_add(unsupported))
            });
        if classified_images != Some(self.animated_images.retained_images) {
            return Err(PendingSnapshotInvariantError::AnimatedImageCountMismatch {
                pipeline_id: self.pipeline_id,
                retained: self.animated_images.retained_images,
                finite: self.animated_images.finite_images,
                infinite: self.animated_images.infinite_images,
                inert: self.animated_images.inert_images,
                unsupported: self
                    .animated_images
                    .unsupported
                    .checked_total()
                    .unwrap_or(u64::MAX),
            });
        }
        Ok(())
    }
}

/// Rendering opportunity and canonical per-document rendering facts for one event loop.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingRenderingObservation {
    /// Exact update-the-rendering callback joined to a live outer-scheduler entry.
    pub scheduled_opportunity: Option<TimerDeadlineSnapshot>,
    /// Whether the ScriptThread rendering-opportunity flag is ready for the next turn.
    pub opportunity_ready: bool,
    pipelines: Vec<PendingPipelineRenderingObservation>,
}

impl PendingRenderingObservation {
    /// Canonicalize per-pipeline rendering evidence.
    pub fn new(
        scheduled_opportunity: Option<TimerDeadlineSnapshot>,
        opportunity_ready: bool,
        mut pipelines: Vec<PendingPipelineRenderingObservation>,
    ) -> Result<Self, PendingSnapshotInvariantError> {
        pipelines.sort_unstable_by_key(|observation| observation.pipeline_id);
        if let Some(pipeline_id) = pipelines
            .windows(2)
            .find(|pair| pair[0].pipeline_id == pair[1].pipeline_id)
            .map(|pair| pair[0].pipeline_id)
        {
            return Err(PendingSnapshotInvariantError::DuplicateRenderingPipeline(
                pipeline_id,
            ));
        }
        for observation in &pipelines {
            observation.validate()?;
        }
        Ok(Self {
            scheduled_opportunity,
            opportunity_ready,
            pipelines,
        })
    }

    /// Return document rendering facts in canonical pipeline order.
    pub fn pipelines(&self) -> &[PendingPipelineRenderingObservation] {
        &self.pipelines
    }

    /// Return whether rendering facts require driving or fail-closed classification.
    ///
    /// Settlement must inspect this independently from [`PendingSourceSnapshot`]. Rendering facts
    /// are authoritative aggregates and are not required to have unstable one-to-one source IDs
    /// in this type-only foundation.
    pub fn has_observed_work(&self) -> bool {
        self.scheduled_opportunity.is_some()
            || self.opportunity_ready
            || self.pipelines.iter().any(|observation| {
                let images = observation.animated_images;
                let canvas = observation.canvas;
                observation.retained_animation_frame_callbacks != 0
                    || observation.runnable_animation_frame_callbacks != 0
                    || observation.document_update_required
                    || observation.pending_animation_events != 0
                    || observation.finite_animations != 0
                    || observation.infinite_animations != 0
                    || observation.unsupported_animations != 0
                    || images.finite_images != 0
                    || images.infinite_images != 0
                    || images.unsupported.checked_total() != Some(0)
                    || images.update_ready
                    || images.scheduled_timer.is_some()
                    || canvas.dirty_contexts != 0
                    || canvas.awaiting_async_upload
                    || canvas.unsupported.live_source_inventory_unavailable != 0
                    || canvas.unsupported.offscreen_execution != 0
                    || canvas.unsupported.mutation_generation_unbound != 0
                    || observation.pending_fonts != 0
                    || observation.pending_images != 0
            })
    }

    fn get(&self, pipeline_id: PipelineId) -> Option<&PendingPipelineRenderingObservation> {
        self.pipelines
            .binary_search_by_key(&pipeline_id, |observation| observation.pipeline_id)
            .ok()
            .map(|index| &self.pipelines[index])
    }

    fn validate(&self) -> Result<(), PendingSnapshotInvariantError> {
        if !self
            .pipelines
            .windows(2)
            .all(|pair| pair[0].pipeline_id < pair[1].pipeline_id)
        {
            if let Some(pipeline_id) = self
                .pipelines
                .windows(2)
                .find(|pair| pair[0].pipeline_id == pair[1].pipeline_id)
                .map(|pair| pair[0].pipeline_id)
            {
                return Err(PendingSnapshotInvariantError::DuplicateRenderingPipeline(
                    pipeline_id,
                ));
            }
            return Err(PendingSnapshotInvariantError::NonCanonicalRenderingPipelines);
        }
        for observation in &self.pipelines {
            observation.validate()?;
        }
        Ok(())
    }
}

/// Engine source class used only to interpret an identity-bearing source observation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[repr(u8)]
pub enum PendingSourceKind {
    /// An ordinary event-loop task.
    Task = 0,
    /// A microtask or mutation-observer delivery.
    Microtask = 1,
    /// A DOM or scheduler timer.
    Timer = 2,
    /// A requestAnimationFrame callback.
    AnimationFrame = 3,
    /// A CSS or Web Animation source.
    Animation = 4,
    /// An active network operation.
    Network = 5,
    /// A parser or navigation-load source.
    Parser = 6,
    /// A required rendering update independent of requestAnimationFrame.
    RenderingUpdate = 7,
    /// A retained object that can later produce work.
    TrackedPresence = 8,
    /// A source not yet assigned a narrower engine class.
    Other = 9,
}

/// Why a visible source can continue without a finite terminal opportunity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub enum PendingOpenEndedSourceReason {
    /// A JavaScript interval remains registered with its effective repeat period.
    Interval { period: Duration },
    /// A CSS or Web Animation has no finite iteration bound.
    InfiniteAnimation,
    /// A WebSocket can receive messages indefinitely.
    WebSocket,
    /// An EventSource can reconnect and receive messages indefinitely.
    EventSource,
    /// A BroadcastChannel can receive messages indefinitely.
    BroadcastChannel,
    /// A MessagePort can receive messages indefinitely.
    MessagePort,
    /// An embedder control awaits user or host input without a finite deadline.
    EmbedderControl,
    /// A media-session action handler can be invoked by external controls indefinitely.
    MediaSessionActionHandler,
    /// A storage-event listener can receive cross-document events indefinitely.
    StorageEventListener,
}

/// Source class for which this runtime slice cannot prove finite settlement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PendingUnsupportedSourceReason {
    /// A document-time surface is not yet controlled by the shared clock.
    TimeSurface(DocumentTimeSurface),
    /// Timer timing or callback identity could not be classified exactly.
    UnclassifiedTimer,
    /// A nested document executes on another event loop.
    CrossEventLoopDocument,
    /// A worker executes outside this event loop's execution ledger.
    Worker,
    /// A worklet executes outside this event loop's execution ledger.
    Worklet,
    /// A media element can advance from an independently owned media clock.
    MediaElement,
    /// A canvas or graphics source lacks a complete mutation/render generation binding.
    GraphicsSource,
    /// A storage backend callback lacks a complete producer lifecycle.
    StorageBackend,
    /// A service-worker callback or event loop is outside this execution surface.
    ServiceWorker,
    /// A peer, device, or subscription can produce untracked external callbacks.
    ExternalSubscription,
    /// A callback source exists but is not yet covered by the producer fence.
    UntrackedCallback,
}

/// Mechanical disposition of one exact source at the observation instant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PendingSourceDisposition {
    /// The retained source cannot currently produce work.
    Inert,
    /// Work is already eligible for an event-loop turn.
    Ready,
    /// Work becomes eligible at one exact future document-clock deadline.
    FiniteDeadline(DocumentTime),
    /// One update-the-rendering opportunity is required without an independent timer deadline.
    FiniteRenderingOpportunity,
    /// Work is waiting on an external operation whose wall duration cannot be virtualized.
    AwaitingExternalIo(PendingExternalIoEvidence),
    /// Work can continue indefinitely without a finite terminal deadline.
    OpenEnded(PendingOpenEndedSourceReason),
    /// The runtime cannot prove how this source settles.
    Unsupported(PendingUnsupportedSourceReason),
}

impl PendingSourceDisposition {
    /// Return the stable top-level classification while preserving typed evidence separately.
    pub const fn kind(self) -> PendingSourceDispositionKind {
        match self {
            Self::Inert => PendingSourceDispositionKind::Inert,
            Self::Ready => PendingSourceDispositionKind::Ready,
            Self::FiniteDeadline(_) => PendingSourceDispositionKind::FiniteDeadline,
            Self::FiniteRenderingOpportunity => {
                PendingSourceDispositionKind::FiniteRenderingOpportunity
            },
            Self::AwaitingExternalIo(_) => PendingSourceDispositionKind::AwaitingExternalIo,
            Self::OpenEnded(_) => PendingSourceDispositionKind::OpenEnded,
            Self::Unsupported(_) => PendingSourceDispositionKind::Unsupported,
        }
    }
}

/// Stable top-level source-disposition discriminant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u8)]
pub enum PendingSourceDispositionKind {
    /// Inert retained source.
    Inert = 0,
    /// Immediately eligible work.
    Ready = 1,
    /// Exact finite clock deadline.
    FiniteDeadline = 2,
    /// Required rendering opportunity.
    FiniteRenderingOpportunity = 3,
    /// External I/O wait.
    AwaitingExternalIo = 4,
    /// Source without a finite terminal opportunity.
    OpenEnded = 5,
    /// Source not controlled by this runtime slice.
    Unsupported = 6,
}

/// One identity-bearing source and its policy-neutral mechanical disposition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingSourceObservation {
    /// Stable event-loop-owned identity for this source.
    pub id: PendingSourceId,
    /// Engine source class.
    pub kind: PendingSourceKind,
    /// Mechanical state at the observation instant.
    pub disposition: PendingSourceDisposition,
}

/// Canonical identity-bearing auxiliary source inventory.
///
/// Absence from this inventory is not proof of settlement. In particular, rendering aggregates
/// in [`PendingRenderingObservation`] are independently authoritative and need not have unstable
/// one-to-one source identities in this type-only foundation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct PendingSourceSnapshot {
    epoch: PendingSourceEpoch,
    sources: Vec<PendingSourceObservation>,
}

impl PendingSourceSnapshot {
    /// Canonicalize sources and reject duplicate engine identities.
    pub fn new(
        epoch: PendingSourceEpoch,
        mut sources: Vec<PendingSourceObservation>,
    ) -> Result<Self, PendingSnapshotInvariantError> {
        sources.sort_unstable_by_key(|source| source.id);
        if let Some(source_id) = duplicate_source_id(&sources) {
            return Err(PendingSnapshotInvariantError::DuplicateSource(source_id));
        }
        Ok(Self { epoch, sources })
    }

    /// Return the monotonic identity of this source inventory.
    pub const fn epoch(&self) -> PendingSourceEpoch {
        self.epoch
    }

    /// Return sources in canonical identity order.
    pub fn sources(&self) -> &[PendingSourceObservation] {
        &self.sources
    }

    fn validate(&self) -> Result<(), PendingSnapshotInvariantError> {
        if !self.sources.windows(2).all(|pair| pair[0].id < pair[1].id) {
            if let Some(source_id) = duplicate_source_id(&self.sources) {
                return Err(PendingSnapshotInvariantError::DuplicateSource(source_id));
            }
            return Err(PendingSnapshotInvariantError::NonCanonicalSourceInventory);
        }
        Ok(())
    }

    fn get(&self, source_id: PendingSourceId) -> Option<&PendingSourceObservation> {
        self.sources
            .binary_search_by_key(&source_id, |source| source.id)
            .ok()
            .map(|index| &self.sources[index])
    }
}

fn duplicate_source_id(sources: &[PendingSourceObservation]) -> Option<PendingSourceId> {
    sources
        .windows(2)
        .find(|pair| pair[0].id == pair[1].id)
        .map(|pair| pair[0].id)
}

/// Complete policy-neutral pending evidence for one event-loop-owned document surface.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RawPendingSnapshot {
    /// Immutable WebView, event-loop, epoch, and pipeline authority for this evidence.
    pub target: PendingTargetObservation,
    /// Monotonic identity of the complete normalized runtime state.
    pub state_generation: RuntimeStateGeneration,
    /// Semantic DOM mutation generation, independent of paint or display-list epochs.
    pub dom_epoch: DomEpoch,
    /// Shared document-clock evidence.
    pub clock: PendingClockObservation,
    /// Exact scheduler-head evidence.
    pub scheduler: PendingSchedulerObservation,
    /// Ordinary event and task-queue evidence.
    pub input: PendingInputObservation,
    /// Microtask queue and checkpoint evidence.
    pub microtasks: PendingMicrotaskObservation,
    /// Producer-fence watermarks and mechanical stability.
    pub producers: PendingProducerObservation,
    /// Authoritative parser and top-level navigation evidence.
    pub parser: PendingParserObservation,
    /// Canonical external network-I/O evidence.
    pub network: PendingNetworkObservation,
    /// Independently authoritative rendering, animation-frame, font, and image evidence.
    pub rendering: PendingRenderingObservation,
    /// Auxiliary canonical source inventory; absence does not replace rendering inspection.
    pub sources: PendingSourceSnapshot,
    /// Additive sticky terminals retained independently by every runtime owner.
    pub terminals: PendingRuntimeTerminals,
}

impl RawPendingSnapshot {
    /// Revalidate canonical inventories and their cross-references after same-build deserialization.
    pub fn validate(&self) -> Result<(), PendingSnapshotInvariantError> {
        self.target.validate()?;
        if self.microtasks.event_loop_id != self.target.event_loop_id {
            return Err(PendingSnapshotInvariantError::MicrotaskEventLoopMismatch);
        }
        if self.producers.event_loop_id != self.target.event_loop_id {
            return Err(PendingSnapshotInvariantError::ProducerEventLoopMismatch);
        }
        self.microtasks.validate()?;
        self.producers.validate()?;
        self.parser.validate()?;
        self.network.validate()?;
        self.rendering.validate()?;
        self.sources.validate()?;
        self.terminals.validate()?;

        if let Some(deadline) = self.scheduler.next_deadline {
            if deadline.scheduler_id != self.scheduler.scheduler_id {
                return Err(
                    PendingSnapshotInvariantError::SchedulerDeadlineIdentityMismatch {
                        expected: self.scheduler.scheduler_id,
                        observed: deadline.scheduler_id,
                    },
                );
            }
        }

        if self.producers.microtask_checkpoint != self.microtasks.completed_checkpoint {
            return Err(
                PendingSnapshotInvariantError::ProducerMicrotaskCheckpointMismatch {
                    producer: self.producers.microtask_checkpoint,
                    microtask: self.microtasks.completed_checkpoint,
                },
            );
        }

        for parser in self.parser.sources() {
            if !self.target.contains_pipeline(parser.pipeline_id) {
                return Err(PendingSnapshotInvariantError::ParserPipelineOutsideTarget(
                    parser.pipeline_id,
                ));
            }
            if parser.kind == PendingParserSourceKind::TopLevelNavigation
                && self
                    .target
                    .pending_top_level_pipelines()
                    .binary_search(&parser.pipeline_id)
                    .is_err()
            {
                return Err(PendingSnapshotInvariantError::ParserNavigationNotPending(
                    parser.pipeline_id,
                ));
            }
            let Some(source) = self.sources.get(parser.source_id) else {
                return Err(PendingSnapshotInvariantError::MissingParserSource(
                    parser.source_id,
                ));
            };
            if source.kind != PendingSourceKind::Parser || source.disposition != parser.disposition
            {
                return Err(PendingSnapshotInvariantError::ParserSourceMismatch(
                    parser.source_id,
                ));
            }
        }

        for source in self.sources.sources() {
            if source.kind == PendingSourceKind::Parser && self.parser.get(source.id).is_none() {
                return Err(PendingSnapshotInvariantError::MissingParserObservation(
                    source.id,
                ));
            }
        }

        for pipeline_id in self.target.pending_top_level_pipelines() {
            match self
                .parser
                .sources()
                .iter()
                .filter(|source| {
                    source.kind == PendingParserSourceKind::TopLevelNavigation
                        && source.pipeline_id == *pipeline_id
                })
                .count()
            {
                1 => {},
                0 => {
                    return Err(
                        PendingSnapshotInvariantError::MissingPendingNavigationObservation(
                            *pipeline_id,
                        ),
                    );
                },
                _ => {
                    return Err(
                        PendingSnapshotInvariantError::DuplicatePendingNavigationObservation(
                            *pipeline_id,
                        ),
                    );
                },
            }
        }

        for rendering in self.rendering.pipelines() {
            if !self.target.contains_pipeline(rendering.pipeline_id) {
                return Err(
                    PendingSnapshotInvariantError::RenderingPipelineOutsideTarget(
                        rendering.pipeline_id,
                    ),
                );
            }
            let fully_active = self
                .target
                .fully_active_pipelines()
                .binary_search(&rendering.pipeline_id)
                .is_ok();
            if fully_active != (rendering.activity == PendingRenderingPipelineActivity::FullyActive)
            {
                return Err(
                    PendingSnapshotInvariantError::RenderingPipelineActivityMismatch(
                        rendering.pipeline_id,
                    ),
                );
            }
            if let Some(timer) = rendering.animated_images.scheduled_timer {
                if timer.scheduler_id != self.scheduler.scheduler_id {
                    return Err(
                        PendingSnapshotInvariantError::AnimatedImageSchedulerIdentityMismatch {
                            pipeline_id: rendering.pipeline_id,
                            expected: self.scheduler.scheduler_id,
                            observed: timer.scheduler_id,
                        },
                    );
                }
            }
        }
        if let Some(timer) = self.rendering.scheduled_opportunity {
            if timer.scheduler_id != self.scheduler.scheduler_id {
                return Err(
                    PendingSnapshotInvariantError::RenderingSchedulerIdentityMismatch {
                        expected: self.scheduler.scheduler_id,
                        observed: timer.scheduler_id,
                    },
                );
            }
        }
        for pipeline_id in self.target.fully_active_pipelines() {
            if self.rendering.get(*pipeline_id).is_none() {
                return Err(
                    PendingSnapshotInvariantError::MissingFullyActiveRenderingObservation(
                        *pipeline_id,
                    ),
                );
            }
        }

        for source in self.sources.sources() {
            if source.kind != PendingSourceKind::Network {
                continue;
            }
            let source_evidence = match source.disposition {
                PendingSourceDisposition::AwaitingExternalIo(evidence) => Some(evidence),
                PendingSourceDisposition::Ready => None,
                _ => continue,
            };
            let Some(network) = self.network.get(source.id) else {
                return Err(PendingSnapshotInvariantError::MissingExternalIoObservation(
                    source.id,
                ));
            };
            match (network.phase, source_evidence) {
                (PendingExternalIoPhase::TerminalTaskQueued, None) => {},
                (PendingExternalIoPhase::TerminalTaskQueued, Some(_)) | (_, None) => {
                    return Err(
                        PendingSnapshotInvariantError::NetworkSourceDispositionMismatch(source.id),
                    );
                },
                (_, Some(evidence)) if network.evidence != evidence => {
                    return Err(PendingSnapshotInvariantError::ExternalIoEvidenceMismatch {
                        source_id: source.id,
                        source: evidence,
                        network: network.evidence,
                    });
                },
                (_, Some(_)) => {},
            }
        }

        for network in self.network.active() {
            if !self.target.contains_pipeline(network.pipeline_id) {
                return Err(
                    PendingSnapshotInvariantError::NetworkPipelineOutsideTarget {
                        source_id: network.source_id,
                        pipeline_id: network.pipeline_id,
                    },
                );
            }
            let Some(source) = self.sources.get(network.source_id) else {
                return Err(PendingSnapshotInvariantError::MissingNetworkSource(
                    network.source_id,
                ));
            };
            if source.kind != PendingSourceKind::Network {
                return Err(PendingSnapshotInvariantError::NetworkSourceKindMismatch {
                    source_id: network.source_id,
                    observed: source.kind,
                });
            }
            if network.phase == PendingExternalIoPhase::TerminalTaskQueued {
                if source.disposition != PendingSourceDisposition::Ready {
                    return Err(
                        PendingSnapshotInvariantError::NetworkSourceDispositionMismatch(
                            network.source_id,
                        ),
                    );
                }
                continue;
            }
            let PendingSourceDisposition::AwaitingExternalIo(evidence) = source.disposition else {
                return Err(
                    PendingSnapshotInvariantError::NetworkSourceDispositionMismatch(
                        network.source_id,
                    ),
                );
            };
            if evidence != network.evidence {
                return Err(PendingSnapshotInvariantError::ExternalIoEvidenceMismatch {
                    source_id: network.source_id,
                    source: evidence,
                    network: network.evidence,
                });
            }
        }

        if self
            .terminals
            .clock
            .is_some_and(|terminal| terminal.clock_id != self.clock.clock_id)
        {
            return Err(PendingSnapshotInvariantError::ClockTerminalIdentityMismatch);
        }
        if self.terminals.outer_scheduler.is_some_and(|terminal| {
            terminal.event_loop_id != self.target.event_loop_id
                || terminal.scheduler_id != self.scheduler.scheduler_id
        }) {
            return Err(PendingSnapshotInvariantError::SchedulerTerminalIdentityMismatch);
        }
        let producer_terminal = self.producers.snapshot.terminal_error();
        match (self.terminals.producer, producer_terminal) {
            (Some(terminal), Some(error))
                if terminal.fence_id == self.producers.fence_id && terminal.error == error => {},
            (None, None) => {},
            _ => return Err(PendingSnapshotInvariantError::ProducerTerminalMismatch),
        }
        match (self.terminals.microtask, self.microtasks.terminal) {
            (Some(terminal), Some(error))
                if terminal.event_loop_id == self.target.event_loop_id
                    && terminal.error == error => {},
            (None, None) => {},
            _ => return Err(PendingSnapshotInvariantError::MicrotaskTerminalMismatch),
        }
        for terminal in self
            .terminals
            .logical_timers()
            .iter()
            .map(|terminal| terminal.pipeline_id)
            .chain(
                self.terminals
                    .image_timers()
                    .iter()
                    .map(|terminal| terminal.pipeline_id),
            )
        {
            if !self.target.contains_pipeline(terminal) {
                return Err(PendingSnapshotInvariantError::TerminalPipelineOutsideTarget(terminal));
            }
        }
        for terminal in [
            self.terminals.dom_generation,
            self.terminals.state_generation,
            self.terminals.navigation_revision,
        ]
        .into_iter()
        .flatten()
        {
            if terminal.webview_id != self.target.webview_id {
                return Err(PendingSnapshotInvariantError::GenerationTerminalIdentityMismatch);
            }
        }
        if self.terminals.dom_generation.is_some() && self.dom_epoch.get() != u64::MAX {
            return Err(PendingSnapshotInvariantError::GenerationTerminalBeforeExhaustion);
        }
        if self.terminals.state_generation.is_some() && self.state_generation.get() != u64::MAX {
            return Err(PendingSnapshotInvariantError::GenerationTerminalBeforeExhaustion);
        }
        if self.terminals.navigation_revision.is_some()
            && self.target.navigation_revision.get() != u64::MAX
        {
            return Err(PendingSnapshotInvariantError::GenerationTerminalBeforeExhaustion);
        }
        Ok(())
    }
}

/// A structural inconsistency in raw pending evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum PendingSnapshotInvariantError {
    /// The target pipeline list contained one identity more than once.
    DuplicateTargetPipeline(PipelineId),
    /// Target pipelines were not in canonical identity order.
    NonCanonicalTargetPipelines,
    /// The fully-active pipeline list contained one identity more than once.
    DuplicateFullyActivePipeline(PipelineId),
    /// Fully-active pipelines were not in canonical identity order.
    NonCanonicalFullyActivePipelines,
    /// The pending top-level list contained one pipeline more than once.
    DuplicatePendingTopLevelPipeline(PipelineId),
    /// Pending top-level pipelines were not in canonical identity order.
    NonCanonicalPendingTopLevelPipelines,
    /// The named active top-level pipeline did not belong to the target.
    ActivePipelineOutsideTarget(PipelineId),
    /// The named active top-level pipeline was absent from the fully-active subset.
    ActivePipelineNotFullyActive(PipelineId),
    /// A fully-active pipeline did not belong to the target.
    FullyActivePipelineOutsideTarget(PipelineId),
    /// Fully-active membership existed before an active top-level pipeline was selected.
    FullyActivePipelineWithoutActiveTopLevel(PipelineId),
    /// A pending top-level pipeline did not belong to the target.
    PendingPipelineOutsideTarget(PipelineId),
    /// The parser inventory contained one source identity more than once.
    DuplicateParserSource(PendingSourceId),
    /// Parser sources were not in canonical identity order.
    NonCanonicalParserSources,
    /// Parser phase and mechanical source disposition disagreed.
    ParserPhaseDispositionMismatch(PendingSourceId),
    /// Parser or navigation evidence named a pipeline outside the immutable target.
    ParserPipelineOutsideTarget(PipelineId),
    /// A top-level navigation parser source did not name a pending top-level pipeline.
    ParserNavigationNotPending(PipelineId),
    /// A pending top-level pipeline had no authoritative navigation source.
    MissingPendingNavigationObservation(PipelineId),
    /// A pending top-level pipeline had more than one authoritative navigation source.
    DuplicatePendingNavigationObservation(PipelineId),
    /// Parser evidence had no matching canonical source entry.
    MissingParserSource(PendingSourceId),
    /// Parser evidence and its canonical source entry disagreed.
    ParserSourceMismatch(PendingSourceId),
    /// A canonical parser source had no authoritative parser observation.
    MissingParserObservation(PendingSourceId),
    /// Producer stability used an impossible checkpoint class.
    ProducerCheckpointMismatch {
        /// Completed microtask boundary which yielded producer qualification.
        microtask_checkpoint: PendingMicrotaskCheckpoint,
        /// Checkpoint supplied by the observation.
        checkpoint: DocumentProducerCheckpoint,
        /// Stability supplied by the observation.
        stability: PendingProducerStability,
    },
    /// Producer stability disagreed with the exact pending watermark.
    ProducerEmptinessMismatch {
        /// Stability supplied by the observation.
        stability: PendingProducerStability,
        /// Exact number of live producer tickets.
        pending: u64,
    },
    /// Stable-empty evidence omitted the earlier qualifying empty boundary.
    ProducerPriorEmptyMissing,
    /// A non-stable producer classification carried stable-empty history.
    ProducerPriorEmptyUnexpected,
    /// Stable-empty history did not precede the current two checkpoint sequences.
    ProducerPriorEmptyCheckpointMismatch,
    /// Stable-empty history named a different producer-fence revision.
    ProducerPriorEmptyRevisionMismatch {
        /// Empty revision observed at the earlier boundary.
        prior: u64,
        /// Empty revision observed at the current boundary.
        current: u64,
    },
    /// Producer observation and snapshot named different fence identities.
    ProducerFenceIdentityMismatch {
        /// Explicit owner identity on the observation.
        observed: DocumentProducerFenceId,
        /// Identity embedded in the mutex-consistent snapshot.
        snapshot: DocumentProducerFenceId,
    },
    /// One producer-kind watermark violated `completed + pending == enqueued`.
    ProducerKindConservationMismatch {
        /// Producer class containing the contradictory watermark.
        kind: DocumentProducerKind,
    },
    /// Global producer watermarks violated `completed + pending == enqueued`.
    ProducerGlobalConservationMismatch,
    /// Per-kind watermark sums disagreed with global producer watermarks.
    ProducerWatermarkSumMismatch,
    /// Producer conservation arithmetic could not be represented exactly.
    ProducerConservationOverflow,
    /// Producer mutation revision disagreed with enqueue and completion totals.
    ProducerRevisionMismatch {
        /// Revision embedded in the snapshot.
        observed: u64,
        /// Exact `enqueued + completed` revision implied by the watermarks.
        expected: u64,
    },
    /// Main microtask evidence belonged to a different event loop than the target.
    MicrotaskEventLoopMismatch,
    /// Producer-fence evidence belonged to a different event loop than the target.
    ProducerEventLoopMismatch,
    /// A sticky microtask exhaustion appeared before the checked generation reached `u64::MAX`.
    MicrotaskTerminalBeforeExhaustion,
    /// A sticky microtask exhaustion appeared while a checkpoint was still executing.
    MicrotaskTerminalDuringCheckpoint,
    /// Producer qualification was not derived from the snapshot's completed microtask boundary.
    ProducerMicrotaskCheckpointMismatch {
        /// Microtask boundary retained with producer qualification.
        producer: PendingMicrotaskCheckpoint,
        /// Completed microtask boundary in the same raw snapshot.
        microtask: PendingMicrotaskCheckpoint,
    },
    /// Two source entries used the same stable identity.
    DuplicateSource(PendingSourceId),
    /// Source entries were not in canonical identity order.
    NonCanonicalSourceInventory,
    /// Two external-I/O entries used the same stable source identity.
    DuplicateExternalIoSource(PendingSourceId),
    /// External-I/O entries were not in canonical identity order.
    NonCanonicalExternalIoInventory,
    /// A network source awaited external I/O without an exact network observation.
    MissingExternalIoObservation(PendingSourceId),
    /// An active network observation had no source-inventory entry.
    MissingNetworkSource(PendingSourceId),
    /// A network operation named a pipeline outside the immutable target.
    NetworkPipelineOutsideTarget {
        /// Stable source identity of the network operation.
        source_id: PendingSourceId,
        /// Out-of-target owner pipeline.
        pipeline_id: PipelineId,
    },
    /// An active network observation referenced a non-network source class.
    NetworkSourceKindMismatch {
        /// Identity shared by the network and source inventories.
        source_id: PendingSourceId,
        /// Source class found in the canonical inventory.
        observed: PendingSourceKind,
    },
    /// An active network operation referenced a source not classified as external I/O.
    NetworkSourceDispositionMismatch(PendingSourceId),
    /// Source and network inventories disagreed about mechanical I/O evidence.
    ExternalIoEvidenceMismatch {
        /// Identity shared by both inventories.
        source_id: PendingSourceId,
        /// Mechanical evidence recorded by the source disposition.
        source: PendingExternalIoEvidence,
        /// Mechanical evidence recorded by the network observation.
        network: PendingExternalIoEvidence,
    },
    /// A document reported more runnable rAF callbacks than retained callback slots.
    RunnableAnimationFramesExceedRetained {
        /// Pipeline owning the callback list.
        pipeline_id: PipelineId,
        /// All retained slots, including canceled tombstones.
        retained: u64,
        /// Retained slots containing runnable callbacks.
        runnable: u64,
    },
    /// Animated-image finite, infinite, and unsupported classes did not cover retained images.
    AnimatedImageCountMismatch {
        /// Pipeline owning the animated-image manager.
        pipeline_id: PipelineId,
        /// All retained active images.
        retained: u64,
        /// Finite-loop images.
        finite: u64,
        /// Infinite-loop images.
        infinite: u64,
        /// Finished finite images retained inertly by the manager.
        inert: u64,
        /// Unsupported images.
        unsupported: u64,
    },
    /// Two rendering observations named the same pipeline.
    DuplicateRenderingPipeline(PipelineId),
    /// Rendering observations were not in canonical pipeline order.
    NonCanonicalRenderingPipelines,
    /// Rendering evidence named a pipeline outside the immutable target.
    RenderingPipelineOutsideTarget(PipelineId),
    /// Rendering activity disagreed with immutable fully-active membership.
    RenderingPipelineActivityMismatch(PipelineId),
    /// A fully-active target pipeline had no authoritative rendering observation.
    MissingFullyActiveRenderingObservation(PipelineId),
    /// The scheduler head carried a kernel scheduler identity different from its raw owner.
    SchedulerDeadlineIdentityMismatch {
        /// Scheduler identity on the raw scheduler observation.
        expected: TimerSchedulerId,
        /// Kernel scheduler identity embedded in the deadline snapshot.
        observed: TimerSchedulerId,
    },
    /// The rendering-opportunity timer belonged to a different outer scheduler.
    RenderingSchedulerIdentityMismatch {
        /// Scheduler observed on the raw snapshot.
        expected: TimerSchedulerId,
        /// Scheduler scoped into the rendering timer reference.
        observed: TimerSchedulerId,
    },
    /// An animated-image callback timer belonged to a different outer scheduler.
    AnimatedImageSchedulerIdentityMismatch {
        /// Pipeline owning the animated-image manager.
        pipeline_id: PipelineId,
        /// Scheduler observed on the raw snapshot.
        expected: TimerSchedulerId,
        /// Scheduler scoped into the image timer reference.
        observed: TimerSchedulerId,
    },
    /// Two logical-timer terminals named the same pipeline owner.
    DuplicateLogicalTimerTerminal(PipelineId),
    /// Logical-timer terminals were not in canonical pipeline order.
    NonCanonicalLogicalTimerTerminals,
    /// Two animated-image timer terminals named the same pipeline owner.
    DuplicateImageTimerTerminal(PipelineId),
    /// Animated-image timer terminals were not in canonical pipeline order.
    NonCanonicalImageTimerTerminals,
    /// Clock terminal identity did not match the observed clock.
    ClockTerminalIdentityMismatch,
    /// Outer-scheduler terminal identity did not match the observed event loop and scheduler.
    SchedulerTerminalIdentityMismatch,
    /// Producer terminal did not exactly match the producer-fence snapshot.
    ProducerTerminalMismatch,
    /// Microtask terminal did not exactly match the microtask observation and event loop.
    MicrotaskTerminalMismatch,
    /// A per-pipeline terminal named a pipeline outside the immutable target.
    TerminalPipelineOutsideTarget(PipelineId),
    /// A generation terminal named a different WebView.
    GenerationTerminalIdentityMismatch,
    /// A generation terminal was present before its checked counter reached exhaustion.
    GenerationTerminalBeforeExhaustion,
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;
    use std::time::Duration;

    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use servo_base::id::{
        BrowsingContextId, BrowsingContextIndex, Index, PipelineIndex, PipelineNamespaceId,
    };
    use timers::{
        DocumentClock, DocumentClockConfiguration, DocumentProducerFence, DocumentProducerKind,
        DocumentTimeSurface, DocumentUnixTime, TimerEventRequest, TimerScheduler,
    };

    use super::*;

    fn assert_postcard_round_trip<T>(value: T)
    where
        T: Debug + DeserializeOwned + Eq + Serialize,
    {
        let bytes = postcard::to_stdvec(&value).unwrap();
        let decoded = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(value, decoded);
    }

    #[derive(Clone, Copy, Serialize)]
    struct EncodedProducerWatermark {
        enqueued: u64,
        completed: u64,
        pending: u64,
    }

    #[derive(Serialize)]
    struct EncodedProducerSnapshot {
        fence_id: DocumentProducerFenceId,
        terminal_error: Option<DocumentProducerFenceError>,
        revision: u64,
        enqueued: u64,
        completed: u64,
        pending: u64,
        by_kind: [EncodedProducerWatermark; 5],
    }

    fn deserialize_forged_producer_snapshot(
        fence_id: DocumentProducerFenceId,
        revision: u64,
        global: EncodedProducerWatermark,
        by_kind: [EncodedProducerWatermark; 5],
    ) -> DocumentProducerSnapshot {
        let bytes = postcard::to_stdvec(&EncodedProducerSnapshot {
            fence_id,
            terminal_error: None,
            revision,
            enqueued: global.enqueued,
            completed: global.completed,
            pending: global.pending,
            by_kind,
        })
        .unwrap();
        postcard::from_bytes(&bytes).unwrap()
    }

    fn controlled_clock() -> DocumentClock {
        DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 5,
            unix_time_origin_ns: DocumentUnixTime::from_nanos(1_000_000),
        })
    }

    fn empty_scheduler_observation() -> PendingSchedulerObservation {
        let scheduler = TimerScheduler::with_clock(controlled_clock());
        PendingSchedulerObservation {
            scheduler_id: scheduler.id(),
            next_deadline: None,
        }
    }

    fn pipeline_id(index: u32) -> PipelineId {
        PipelineId {
            namespace_id: PipelineNamespaceId(41),
            index: Index::<PipelineIndex>::new(index).unwrap(),
        }
    }

    fn pending_target() -> PendingTargetObservation {
        let active_pipeline = pipeline_id(1);
        let pending_pipeline = pipeline_id(2);
        let browsing_context_id = BrowsingContextId {
            namespace_id: PipelineNamespaceId(42),
            index: Index::<BrowsingContextIndex>::new(1).unwrap(),
        };
        PendingTargetObservation::new(
            WebViewId::mock_for_testing(browsing_context_id),
            ScriptEventLoopId::new(),
            Some(PendingActiveTopLevelPipeline {
                pipeline_id: active_pipeline,
                epoch: Epoch(7),
            }),
            PendingNavigationRevision::new(3),
            vec![pending_pipeline, active_pipeline],
            vec![active_pipeline],
            vec![pending_pipeline],
        )
        .unwrap()
    }

    fn empty_producers(event_loop_id: ScriptEventLoopId) -> PendingProducerObservation {
        let fence = DocumentProducerFence::default();
        PendingProducerObservation::new(
            event_loop_id,
            PendingMicrotaskCheckpoint::new(1),
            DocumentProducerCheckpoint::ZERO.checked_next().unwrap(),
            fence.snapshot(),
            PendingProducerStability::FirstEmpty,
            None,
        )
        .unwrap()
    }

    fn top_level_navigation(
        source_id: PendingSourceId,
        pipeline_id: PipelineId,
    ) -> PendingParserSourceObservation {
        PendingParserSourceObservation {
            source_id,
            pipeline_id,
            kind: PendingParserSourceKind::TopLevelNavigation,
            phase: PendingParserPhase::Ready,
            disposition: PendingSourceDisposition::Ready,
        }
    }

    fn source(
        id: u64,
        kind: PendingSourceKind,
        disposition: PendingSourceDisposition,
    ) -> PendingSourceObservation {
        PendingSourceObservation {
            id: PendingSourceId::new(id),
            kind,
            disposition,
        }
    }

    fn external_io_evidence() -> PendingExternalIoEvidence {
        PendingExternalIoEvidence {
            owner: PendingExternalIoOwner::Script,
            load_blocking: PendingExternalIoLoadBlocking::NonBlocking,
        }
    }

    fn rendering_for(pipeline_id: PipelineId) -> PendingRenderingObservation {
        PendingRenderingObservation::new(
            None,
            true,
            vec![PendingPipelineRenderingObservation {
                pipeline_id,
                activity: PendingRenderingPipelineActivity::FullyActive,
                retained_animation_frame_callbacks: 2,
                runnable_animation_frame_callbacks: 1,
                document_update_required: true,
                pending_animation_events: 1,
                finite_animations: 2,
                infinite_animations: 1,
                unsupported_animations: 0,
                animated_images: PendingAnimatedImageObservation {
                    retained_images: 2,
                    finite_images: 1,
                    infinite_images: 1,
                    inert_images: 0,
                    unsupported: PendingAnimatedImageUnsupportedCounts::default(),
                    update_ready: true,
                    scheduled_timer: None,
                },
                canvas: PendingCanvasObservation {
                    dirty_contexts: 1,
                    awaiting_async_upload: true,
                    unsupported: PendingCanvasUnsupportedCounts {
                        live_source_inventory_unavailable: 1,
                        ..PendingCanvasUnsupportedCounts::default()
                    },
                },
                pending_fonts: 1,
                pending_images: 1,
            }],
        )
        .unwrap()
    }

    fn minimal_raw_snapshot() -> RawPendingSnapshot {
        let target = pending_target();
        let event_loop_id = target.event_loop_id;
        let active_pipeline = target.active_top_level.unwrap().pipeline_id;
        let pending_pipeline = target.pending_top_level_pipelines()[0];
        let navigation_id = PendingSourceId::new(100);
        let clock = controlled_clock();
        let snapshot = RawPendingSnapshot {
            target,
            state_generation: RuntimeStateGeneration::ZERO,
            dom_epoch: DomEpoch::ZERO,
            clock: PendingClockObservation {
                clock_id: clock.id(),
                mode: PendingClockMode::Controlled,
                now: clock.now(),
                unsupported_surface: None,
            },
            scheduler: empty_scheduler_observation(),
            input: PendingInputObservation::default(),
            microtasks: PendingMicrotaskObservation {
                event_loop_id,
                queued: 0,
                completed_checkpoint: PendingMicrotaskCheckpoint::new(1),
                checkpoint_in_progress: false,
                terminal: None,
            },
            producers: empty_producers(event_loop_id),
            parser: PendingParserObservation::new(vec![top_level_navigation(
                navigation_id,
                pending_pipeline,
            )])
            .unwrap(),
            network: PendingNetworkObservation::default(),
            rendering: rendering_for(active_pipeline),
            sources: PendingSourceSnapshot::new(
                PendingSourceEpoch::ZERO,
                vec![source(
                    navigation_id.get(),
                    PendingSourceKind::Parser,
                    PendingSourceDisposition::Ready,
                )],
            )
            .unwrap(),
            terminals: PendingRuntimeTerminals::default(),
        };
        snapshot.validate().unwrap();
        snapshot
    }

    #[test]
    fn raw_pending_snapshot_round_trips_without_a_wire_projection() {
        let clock = controlled_clock();
        let mut scheduler = TimerScheduler::with_clock(clock.clone());
        scheduler.schedule_timer(TimerEventRequest {
            callback: Box::new(|| {}),
            duration: Duration::from_nanos(20),
        });
        let next_deadline = scheduler.finite_deadline_snapshot().unwrap().unwrap();
        let network_id = PendingSourceId::new(20);
        let parser_id = PendingSourceId::new(40);
        let navigation_id = PendingSourceId::new(41);
        let evidence = external_io_evidence();
        let sources = PendingSourceSnapshot::new(
            PendingSourceEpoch::new(4),
            vec![
                source(
                    30,
                    PendingSourceKind::Timer,
                    PendingSourceDisposition::OpenEnded(PendingOpenEndedSourceReason::Interval {
                        period: Duration::from_secs(5),
                    }),
                ),
                source(
                    network_id.get(),
                    PendingSourceKind::Network,
                    PendingSourceDisposition::AwaitingExternalIo(evidence),
                ),
                source(
                    10,
                    PendingSourceKind::Timer,
                    PendingSourceDisposition::FiniteDeadline(next_deadline.deadline),
                ),
                source(
                    parser_id.get(),
                    PendingSourceKind::Parser,
                    PendingSourceDisposition::Ready,
                ),
                source(
                    navigation_id.get(),
                    PendingSourceKind::Parser,
                    PendingSourceDisposition::Ready,
                ),
            ],
        )
        .unwrap();
        let target = pending_target();
        let active_pipeline = target.active_top_level.unwrap().pipeline_id;
        let pending_pipeline = pipeline_id(2);
        let event_loop_id = target.event_loop_id;
        let network = PendingNetworkObservation::new(vec![PendingExternalIoObservation {
            source_id: network_id,
            pipeline_id: active_pipeline,
            kind: PendingNetworkKind::Fetch,
            phase: PendingExternalIoPhase::StreamingBody,
            evidence,
            started_at: DocumentTime::from_nanos(5),
        }])
        .unwrap();
        let parser = PendingParserObservation::new(vec![
            PendingParserSourceObservation {
                source_id: parser_id,
                pipeline_id: active_pipeline,
                kind: PendingParserSourceKind::DocumentParser,
                phase: PendingParserPhase::Ready,
                disposition: PendingSourceDisposition::Ready,
            },
            top_level_navigation(navigation_id, pending_pipeline),
        ])
        .unwrap();
        let scheduler_id = next_deadline.scheduler_id;
        let mut terminals = PendingRuntimeTerminals::new(
            vec![PendingLogicalTimerTerminalObservation {
                pipeline_id: active_pipeline,
                error: DocumentClockError::Overflow,
            }],
            vec![PendingImageTimerTerminalObservation {
                pipeline_id: pending_pipeline,
                error: TimerControlError::DeadlineOverflow,
            }],
        )
        .unwrap();
        terminals.clock = Some(PendingClockTerminalObservation {
            clock_id: clock.id(),
            error: PendingClockTerminal::Overflow,
        });
        terminals.outer_scheduler = Some(PendingOuterSchedulerTerminalObservation {
            event_loop_id: target.event_loop_id,
            scheduler_id,
            error: TimerControlError::SequenceExhausted,
        });
        let snapshot = RawPendingSnapshot {
            target,
            state_generation: RuntimeStateGeneration::new(8),
            dom_epoch: DomEpoch::new(3),
            clock: PendingClockObservation {
                clock_id: clock.id(),
                mode: PendingClockMode::Controlled,
                now: clock.now(),
                unsupported_surface: Some(DocumentTimeSurface::Worker),
            },
            scheduler: PendingSchedulerObservation {
                scheduler_id,
                next_deadline: Some(next_deadline),
            },
            input: PendingInputObservation {
                revision: PendingInputRevision::new(2),
                ready_events: 1,
                intake_saturated: false,
                tasks: PendingTaskObservation {
                    ready: 1,
                    throttled: 2,
                    inactive: 3,
                },
            },
            microtasks: PendingMicrotaskObservation {
                event_loop_id,
                queued: 2,
                completed_checkpoint: PendingMicrotaskCheckpoint::new(1),
                checkpoint_in_progress: false,
                terminal: None,
            },
            producers: empty_producers(event_loop_id),
            parser,
            network,
            rendering: rendering_for(active_pipeline),
            sources,
            terminals,
        };
        snapshot.validate().unwrap();
        assert_postcard_round_trip(snapshot);
    }

    #[test]
    fn public_classification_discriminants_are_explicit() {
        assert_eq!(PendingClockMode::Realtime as u8, 0);
        assert_eq!(PendingClockMode::Controlled as u8, 1);
        assert_eq!(PendingProducerStability::NotCheckpointed as u8, 0);
        assert_eq!(PendingProducerStability::Busy as u8, 1);
        assert_eq!(PendingProducerStability::FirstEmpty as u8, 2);
        assert_eq!(PendingProducerStability::StableEmpty as u8, 3);

        let dispositions = [
            PendingSourceDisposition::Inert,
            PendingSourceDisposition::Ready,
            PendingSourceDisposition::FiniteDeadline(DocumentTime::ZERO),
            PendingSourceDisposition::FiniteRenderingOpportunity,
            PendingSourceDisposition::AwaitingExternalIo(PendingExternalIoEvidence {
                owner: PendingExternalIoOwner::Other,
                load_blocking: PendingExternalIoLoadBlocking::Unknown,
            }),
            PendingSourceDisposition::OpenEnded(PendingOpenEndedSourceReason::WebSocket),
            PendingSourceDisposition::Unsupported(
                PendingUnsupportedSourceReason::UntrackedCallback,
            ),
        ];
        for (expected, disposition) in dispositions.into_iter().enumerate() {
            assert_eq!(disposition.kind() as usize, expected);
        }
        assert_eq!(PendingExternalIoOwner::TopLevelNavigation as u8, 0);
        assert_eq!(PendingExternalIoLoadBlocking::Blocking as u8, 0);
        assert_eq!(PendingParserSourceKind::DocumentParser as u8, 0);
        assert_eq!(PendingParserPhase::Ready as u8, 0);
        assert_eq!(PendingGenerationTerminal::Exhausted as u8, 0);
        assert_eq!(
            PendingAnimatedImageUnsupportedReason::LoopCountUnavailable as u8,
            0
        );
        assert_eq!(
            PendingCanvasUnsupportedReason::LiveSourceInventoryUnavailable as u8,
            0
        );
    }

    #[test]
    fn producer_observation_rejects_impossible_checkpoint_and_emptiness_pairs() {
        let empty_fence = DocumentProducerFence::default();
        let empty = empty_fence.snapshot();
        let event_loop_id = ScriptEventLoopId::new();
        assert_eq!(
            PendingProducerObservation::new(
                event_loop_id,
                PendingMicrotaskCheckpoint::ZERO,
                DocumentProducerCheckpoint::ZERO,
                empty,
                PendingProducerStability::FirstEmpty,
                None,
            ),
            Err(PendingSnapshotInvariantError::ProducerCheckpointMismatch {
                microtask_checkpoint: PendingMicrotaskCheckpoint::ZERO,
                checkpoint: DocumentProducerCheckpoint::ZERO,
                stability: PendingProducerStability::FirstEmpty,
            }),
        );

        let busy_fence = DocumentProducerFence::default();
        let _guard = busy_fence.begin(DocumentProducerKind::Resource).unwrap();
        let busy = busy_fence.snapshot();
        let checkpoint = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let microtask_checkpoint = PendingMicrotaskCheckpoint::new(1);
        assert_eq!(
            PendingProducerObservation::new(
                event_loop_id,
                microtask_checkpoint,
                checkpoint,
                busy,
                PendingProducerStability::FirstEmpty,
                None,
            ),
            Err(PendingSnapshotInvariantError::ProducerEmptinessMismatch {
                stability: PendingProducerStability::FirstEmpty,
                pending: 1,
            }),
        );
        assert_eq!(
            PendingProducerObservation::new(
                event_loop_id,
                microtask_checkpoint,
                checkpoint,
                empty,
                PendingProducerStability::Busy,
                None,
            ),
            Err(PendingSnapshotInvariantError::ProducerEmptinessMismatch {
                stability: PendingProducerStability::Busy,
                pending: 0,
            }),
        );

        let next_checkpoint = checkpoint.checked_next().unwrap();
        let next_microtask_checkpoint = microtask_checkpoint.checked_next().unwrap();
        assert_eq!(
            PendingProducerObservation::new(
                event_loop_id,
                next_microtask_checkpoint,
                next_checkpoint,
                empty,
                PendingProducerStability::StableEmpty,
                None,
            ),
            Err(PendingSnapshotInvariantError::ProducerPriorEmptyMissing),
        );
        let prior_empty = PendingProducerPriorEmptyQualification {
            microtask_checkpoint,
            checkpoint,
            snapshot_revision: empty.revision(),
        };
        PendingProducerObservation::new(
            event_loop_id,
            next_microtask_checkpoint,
            next_checkpoint,
            empty,
            PendingProducerStability::StableEmpty,
            Some(prior_empty),
        )
        .unwrap();
        assert_eq!(
            PendingProducerObservation::new(
                event_loop_id,
                next_microtask_checkpoint,
                next_checkpoint,
                empty,
                PendingProducerStability::StableEmpty,
                Some(PendingProducerPriorEmptyQualification {
                    snapshot_revision: empty.revision() + 1,
                    ..prior_empty
                }),
            ),
            Err(
                PendingSnapshotInvariantError::ProducerPriorEmptyRevisionMismatch {
                    prior: empty.revision() + 1,
                    current: empty.revision(),
                }
            ),
        );
    }

    #[test]
    fn producer_snapshot_deserialization_rejects_conservation_contradictions() {
        let event_loop_id = ScriptEventLoopId::new();
        let base = DocumentProducerFence::default().snapshot();
        let zero = EncodedProducerWatermark {
            enqueued: 0,
            completed: 0,
            pending: 0,
        };
        let validate = |snapshot: DocumentProducerSnapshot| {
            let mut observation = empty_producers(event_loop_id);
            observation.fence_id = snapshot.fence_id();
            observation.snapshot = snapshot;
            observation.validate()
        };

        let mut by_kind = [zero; 5];
        by_kind[0] = EncodedProducerWatermark {
            enqueued: 1,
            completed: 1,
            pending: 1,
        };
        let forged = deserialize_forged_producer_snapshot(
            base.fence_id(),
            2,
            EncodedProducerWatermark {
                enqueued: 1,
                completed: 1,
                pending: 0,
            },
            by_kind,
        );
        assert_eq!(
            validate(forged),
            Err(
                PendingSnapshotInvariantError::ProducerKindConservationMismatch {
                    kind: DocumentProducerKind::Task,
                }
            ),
        );

        let forged = deserialize_forged_producer_snapshot(
            base.fence_id(),
            2,
            EncodedProducerWatermark {
                enqueued: 1,
                completed: 1,
                pending: 1,
            },
            [zero; 5],
        );
        assert_eq!(
            validate(forged),
            Err(PendingSnapshotInvariantError::ProducerGlobalConservationMismatch),
        );

        let forged = deserialize_forged_producer_snapshot(
            base.fence_id(),
            1,
            EncodedProducerWatermark {
                enqueued: 1,
                completed: 0,
                pending: 1,
            },
            [zero; 5],
        );
        assert_eq!(
            validate(forged),
            Err(PendingSnapshotInvariantError::ProducerWatermarkSumMismatch),
        );

        let forged = deserialize_forged_producer_snapshot(base.fence_id(), 1, zero, [zero; 5]);
        assert_eq!(
            validate(forged),
            Err(PendingSnapshotInvariantError::ProducerRevisionMismatch {
                observed: 1,
                expected: 0,
            }),
        );

        let mut by_kind = [zero; 5];
        by_kind[0] = EncodedProducerWatermark {
            enqueued: u64::MAX,
            completed: u64::MAX,
            pending: 0,
        };
        by_kind[1] = EncodedProducerWatermark {
            enqueued: 1,
            completed: 1,
            pending: 0,
        };
        let forged = deserialize_forged_producer_snapshot(base.fence_id(), 0, zero, by_kind);
        assert_eq!(
            validate(forged),
            Err(PendingSnapshotInvariantError::ProducerConservationOverflow),
        );
    }

    #[test]
    fn target_observation_canonicalizes_and_binds_active_pipeline() {
        let target = pending_target();
        let active = pipeline_id(1);
        let pending = pipeline_id(2);
        assert_eq!(target.pipelines(), &[active, pending]);
        assert_eq!(target.fully_active_pipelines(), &[active]);
        assert_eq!(target.pending_top_level_pipelines(), &[pending]);

        assert_eq!(
            PendingTargetObservation::new(
                target.webview_id,
                target.event_loop_id,
                Some(PendingActiveTopLevelPipeline {
                    pipeline_id: active,
                    epoch: Epoch(7),
                }),
                target.navigation_revision,
                vec![pending],
                vec![active],
                Vec::new(),
            ),
            Err(PendingSnapshotInvariantError::ActivePipelineOutsideTarget(
                active,
            )),
        );
        assert_eq!(
            PendingTargetObservation::new(
                target.webview_id,
                target.event_loop_id,
                Some(PendingActiveTopLevelPipeline {
                    pipeline_id: active,
                    epoch: Epoch(7),
                }),
                target.navigation_revision,
                vec![active, pending],
                vec![active, pending, pending],
                vec![pending],
            ),
            Err(PendingSnapshotInvariantError::DuplicateFullyActivePipeline(
                pending,
            )),
        );

        let before_first_activation = PendingTargetObservation::new(
            target.webview_id,
            target.event_loop_id,
            None,
            PendingNavigationRevision::new(4),
            vec![pending],
            Vec::new(),
            vec![pending],
        )
        .unwrap();
        assert_eq!(before_first_activation.active_top_level, None);
    }

    #[test]
    fn raw_snapshot_requires_exactly_one_navigation_per_pending_pipeline() {
        let mut snapshot = minimal_raw_snapshot();
        let pending_pipeline = snapshot.target.pending_top_level_pipelines()[0];
        snapshot.parser = PendingParserObservation::default();
        snapshot.sources = PendingSourceSnapshot::default();
        assert_eq!(
            snapshot.validate(),
            Err(
                PendingSnapshotInvariantError::MissingPendingNavigationObservation(
                    pending_pipeline,
                ),
            ),
        );

        let first = PendingSourceId::new(101);
        let second = PendingSourceId::new(102);
        snapshot.parser = PendingParserObservation::new(vec![
            top_level_navigation(first, pending_pipeline),
            top_level_navigation(second, pending_pipeline),
        ])
        .unwrap();
        snapshot.sources = PendingSourceSnapshot::new(
            PendingSourceEpoch::new(1),
            vec![
                source(
                    first.get(),
                    PendingSourceKind::Parser,
                    PendingSourceDisposition::Ready,
                ),
                source(
                    second.get(),
                    PendingSourceKind::Parser,
                    PendingSourceDisposition::Ready,
                ),
            ],
        )
        .unwrap();
        assert_eq!(
            snapshot.validate(),
            Err(
                PendingSnapshotInvariantError::DuplicatePendingNavigationObservation(
                    pending_pipeline,
                ),
            ),
        );
    }

    #[test]
    fn microtask_and_producer_evidence_cannot_be_spliced_across_event_loops() {
        let event_loop_id = ScriptEventLoopId::new();
        assert_eq!(
            PendingMicrotaskObservation {
                event_loop_id,
                queued: 0,
                completed_checkpoint: PendingMicrotaskCheckpoint::new(u64::MAX - 1),
                checkpoint_in_progress: false,
                terminal: Some(PendingMicrotaskTerminal::CheckpointGenerationExhausted),
            }
            .validate(),
            Err(PendingSnapshotInvariantError::MicrotaskTerminalBeforeExhaustion),
        );
        assert_eq!(
            PendingMicrotaskObservation {
                event_loop_id,
                queued: 0,
                completed_checkpoint: PendingMicrotaskCheckpoint::new(u64::MAX),
                checkpoint_in_progress: true,
                terminal: Some(PendingMicrotaskTerminal::CheckpointGenerationExhausted),
            }
            .validate(),
            Err(PendingSnapshotInvariantError::MicrotaskTerminalDuringCheckpoint),
        );
        PendingMicrotaskObservation {
            event_loop_id,
            queued: 0,
            completed_checkpoint: PendingMicrotaskCheckpoint::new(u64::MAX),
            checkpoint_in_progress: false,
            terminal: Some(PendingMicrotaskTerminal::CheckpointGenerationExhausted),
        }
        .validate()
        .unwrap();

        let mut snapshot = minimal_raw_snapshot();
        snapshot.microtasks.event_loop_id = ScriptEventLoopId::new();
        assert_eq!(
            snapshot.validate(),
            Err(PendingSnapshotInvariantError::MicrotaskEventLoopMismatch),
        );
        snapshot = minimal_raw_snapshot();
        snapshot.producers.event_loop_id = ScriptEventLoopId::new();
        assert_eq!(
            snapshot.validate(),
            Err(PendingSnapshotInvariantError::ProducerEventLoopMismatch),
        );
        snapshot = minimal_raw_snapshot();
        snapshot.producers.fence_id = DocumentProducerFence::default().snapshot().fence_id();
        assert!(matches!(
            snapshot.validate(),
            Err(PendingSnapshotInvariantError::ProducerFenceIdentityMismatch { .. })
        ));
    }

    #[test]
    fn canonical_inventories_sort_and_reject_duplicate_identities() {
        let first = source(1, PendingSourceKind::Timer, PendingSourceDisposition::Ready);
        let second = source(
            2,
            PendingSourceKind::AnimationFrame,
            PendingSourceDisposition::FiniteRenderingOpportunity,
        );
        let sources =
            PendingSourceSnapshot::new(PendingSourceEpoch::new(1), vec![second, first]).unwrap();
        assert_eq!(sources.sources(), &[first, second]);
        assert_eq!(
            PendingSourceSnapshot::new(PendingSourceEpoch::new(2), vec![first, first]),
            Err(PendingSnapshotInvariantError::DuplicateSource(
                PendingSourceId::new(1),
            )),
        );

        let external = PendingExternalIoObservation {
            source_id: PendingSourceId::new(3),
            pipeline_id: pending_target().active_top_level.unwrap().pipeline_id,
            kind: PendingNetworkKind::Navigation,
            phase: PendingExternalIoPhase::AwaitingResponse,
            evidence: external_io_evidence(),
            started_at: DocumentTime::ZERO,
        };
        assert_eq!(
            PendingNetworkObservation::new(vec![external, external]),
            Err(PendingSnapshotInvariantError::DuplicateExternalIoSource(
                PendingSourceId::new(3),
            )),
        );

        let parser_id = PendingSourceId::new(4);
        assert_eq!(
            PendingParserObservation::new(vec![PendingParserSourceObservation {
                source_id: parser_id,
                pipeline_id: pending_target().active_top_level.unwrap().pipeline_id,
                kind: PendingParserSourceKind::DocumentParser,
                phase: PendingParserPhase::AwaitingExternalInput,
                disposition: PendingSourceDisposition::Ready,
            }]),
            Err(PendingSnapshotInvariantError::ParserPhaseDispositionMismatch(parser_id,)),
        );
    }

    #[test]
    fn rendering_observation_rejects_lossy_or_contradictory_counts() {
        let pipeline_id = pipeline_id(1);
        let mut rendering = rendering_for(pipeline_id).pipelines()[0];
        rendering.runnable_animation_frame_callbacks =
            rendering.retained_animation_frame_callbacks + 1;
        assert_eq!(
            PendingRenderingObservation::new(None, false, vec![rendering]),
            Err(
                PendingSnapshotInvariantError::RunnableAnimationFramesExceedRetained {
                    pipeline_id,
                    retained: 2,
                    runnable: 3,
                },
            ),
        );

        let mut rendering = rendering_for(pipeline_id).pipelines()[0];
        rendering.animated_images.inert_images = 1;
        assert_eq!(
            PendingRenderingObservation::new(None, false, vec![rendering]),
            Err(PendingSnapshotInvariantError::AnimatedImageCountMismatch {
                pipeline_id,
                retained: 2,
                finite: 1,
                infinite: 1,
                inert: 1,
                unsupported: 0,
            }),
        );

        let mut rendering = rendering_for(pipeline_id).pipelines()[0];
        rendering.animated_images.infinite_images = 0;
        rendering.animated_images.unsupported = PendingAnimatedImageUnsupportedCounts {
            loop_count_unavailable: 1,
            timeline_uncontrolled: 2,
            timer_binding_unavailable: 3,
        };
        rendering.animated_images.retained_images = 7;
        PendingRenderingObservation::new(None, false, vec![rendering]).unwrap();
        assert_eq!(
            rendering
                .animated_images
                .unsupported
                .count(PendingAnimatedImageUnsupportedReason::TimelineUncontrolled),
            2,
        );

        assert!(rendering_for(pipeline_id).has_observed_work());
        assert!(!PendingRenderingObservation::default().has_observed_work());
    }

    #[test]
    fn rendering_timer_references_are_bound_to_the_observed_scheduler() {
        let clock = controlled_clock();
        let mut scheduler = TimerScheduler::with_clock(clock);
        scheduler.schedule_timer(TimerEventRequest {
            callback: Box::new(|| {}),
            duration: Duration::from_nanos(1),
        });
        let deadline = scheduler.finite_deadline_snapshot().unwrap().unwrap();
        let actual = deadline.scheduler_id;
        let mut foreign_scheduler = TimerScheduler::with_clock(controlled_clock());
        foreign_scheduler.schedule_timer(TimerEventRequest {
            callback: Box::new(|| {}),
            duration: Duration::from_nanos(1),
        });
        let foreign_deadline = foreign_scheduler
            .finite_deadline_snapshot()
            .unwrap()
            .unwrap();
        let foreign = foreign_deadline.scheduler_id;
        let mut snapshot = minimal_raw_snapshot();
        snapshot.scheduler = PendingSchedulerObservation {
            scheduler_id: actual,
            next_deadline: Some(deadline),
        };
        snapshot.rendering.scheduled_opportunity = Some(foreign_deadline);
        assert_eq!(
            snapshot.validate(),
            Err(
                PendingSnapshotInvariantError::RenderingSchedulerIdentityMismatch {
                    expected: actual,
                    observed: foreign,
                }
            ),
        );

        snapshot.rendering.scheduled_opportunity = Some(deadline);
        snapshot.rendering.pipelines[0]
            .animated_images
            .scheduled_timer = Some(foreign_deadline);
        assert_eq!(
            snapshot.validate(),
            Err(
                PendingSnapshotInvariantError::AnimatedImageSchedulerIdentityMismatch {
                    pipeline_id: snapshot.rendering.pipelines[0].pipeline_id,
                    expected: actual,
                    observed: foreign,
                },
            ),
        );

        snapshot.rendering.pipelines[0]
            .animated_images
            .scheduled_timer = Some(deadline);
        snapshot.validate().unwrap();

        snapshot.scheduler.scheduler_id = foreign;
        assert_eq!(
            snapshot.validate(),
            Err(
                PendingSnapshotInvariantError::SchedulerDeadlineIdentityMismatch {
                    expected: foreign,
                    observed: actual,
                }
            ),
        );
    }

    #[test]
    fn raw_snapshot_rejects_network_source_mismatches() {
        let network_id = PendingSourceId::new(9);
        let navigation_id = PendingSourceId::new(10);
        let target = pending_target();
        let pipeline_id = target.active_top_level.unwrap().pipeline_id;
        let pending_pipeline_id = target.pending_top_level_pipelines()[0];
        let event_loop_id = target.event_loop_id;
        let evidence = external_io_evidence();
        let network = PendingNetworkObservation::new(vec![PendingExternalIoObservation {
            source_id: network_id,
            pipeline_id,
            kind: PendingNetworkKind::Fetch,
            phase: PendingExternalIoPhase::TerminalTaskQueued,
            evidence,
            started_at: DocumentTime::ZERO,
        }])
        .unwrap();
        let mut snapshot = RawPendingSnapshot {
            target,
            state_generation: RuntimeStateGeneration::ZERO,
            dom_epoch: DomEpoch::ZERO,
            clock: PendingClockObservation {
                clock_id: controlled_clock().id(),
                mode: PendingClockMode::Controlled,
                now: DocumentTime::ZERO,
                unsupported_surface: None,
            },
            scheduler: empty_scheduler_observation(),
            input: PendingInputObservation::default(),
            microtasks: PendingMicrotaskObservation {
                event_loop_id,
                queued: 0,
                completed_checkpoint: PendingMicrotaskCheckpoint::new(1),
                checkpoint_in_progress: false,
                terminal: None,
            },
            producers: empty_producers(event_loop_id),
            parser: PendingParserObservation::new(vec![top_level_navigation(
                navigation_id,
                pending_pipeline_id,
            )])
            .unwrap(),
            network,
            rendering: rendering_for(pipeline_id),
            sources: PendingSourceSnapshot::new(
                PendingSourceEpoch::ZERO,
                vec![source(
                    navigation_id.get(),
                    PendingSourceKind::Parser,
                    PendingSourceDisposition::Ready,
                )],
            )
            .unwrap(),
            terminals: PendingRuntimeTerminals::default(),
        };
        assert_eq!(
            snapshot.validate(),
            Err(PendingSnapshotInvariantError::MissingNetworkSource(
                network_id,
            )),
        );

        snapshot.sources = PendingSourceSnapshot::new(
            PendingSourceEpoch::new(1),
            vec![
                source(
                    network_id.get(),
                    PendingSourceKind::Network,
                    PendingSourceDisposition::AwaitingExternalIo(evidence),
                ),
                source(
                    navigation_id.get(),
                    PendingSourceKind::Parser,
                    PendingSourceDisposition::Ready,
                ),
            ],
        )
        .unwrap();
        assert_eq!(
            snapshot.validate(),
            Err(PendingSnapshotInvariantError::NetworkSourceDispositionMismatch(network_id,)),
        );

        snapshot.sources = PendingSourceSnapshot::new(
            PendingSourceEpoch::new(2),
            vec![
                source(
                    network_id.get(),
                    PendingSourceKind::Network,
                    PendingSourceDisposition::Ready,
                ),
                source(
                    navigation_id.get(),
                    PendingSourceKind::Parser,
                    PendingSourceDisposition::Ready,
                ),
            ],
        )
        .unwrap();
        snapshot.validate().unwrap();

        let other_evidence = PendingExternalIoEvidence {
            owner: PendingExternalIoOwner::DocumentSubresource,
            load_blocking: PendingExternalIoLoadBlocking::Blocking,
        };
        snapshot.network = PendingNetworkObservation::new(vec![PendingExternalIoObservation {
            source_id: network_id,
            pipeline_id,
            kind: PendingNetworkKind::Fetch,
            phase: PendingExternalIoPhase::StreamingBody,
            evidence,
            started_at: DocumentTime::ZERO,
        }])
        .unwrap();
        snapshot.sources = PendingSourceSnapshot::new(
            PendingSourceEpoch::new(3),
            vec![
                source(
                    network_id.get(),
                    PendingSourceKind::Network,
                    PendingSourceDisposition::AwaitingExternalIo(other_evidence),
                ),
                source(
                    navigation_id.get(),
                    PendingSourceKind::Parser,
                    PendingSourceDisposition::Ready,
                ),
            ],
        )
        .unwrap();
        assert_eq!(
            snapshot.validate(),
            Err(PendingSnapshotInvariantError::ExternalIoEvidenceMismatch {
                source_id: network_id,
                source: other_evidence,
                network: evidence,
            }),
        );

        snapshot.network = PendingNetworkObservation::default();
        assert_eq!(
            snapshot.validate(),
            Err(PendingSnapshotInvariantError::MissingExternalIoObservation(
                network_id,
            )),
        );
    }

    #[test]
    fn checked_sequences_and_task_totals_never_wrap() {
        assert_eq!(RuntimeStateGeneration::new(u64::MAX).checked_next(), None);
        assert_eq!(DomEpoch::new(u64::MAX).checked_next(), None);
        assert_eq!(PendingInputRevision::new(u64::MAX).checked_next(), None);
        assert_eq!(
            PendingNavigationRevision::new(u64::MAX).checked_next(),
            None,
        );
        assert_eq!(
            PendingMicrotaskCheckpoint::new(u64::MAX).checked_next(),
            None,
        );
        assert_eq!(PendingSourceEpoch::new(u64::MAX).checked_next(), None);
        assert_eq!(
            PendingTaskObservation {
                ready: u64::MAX,
                throttled: 1,
                inactive: 0,
            }
            .checked_total(),
            None,
        );
    }
}
