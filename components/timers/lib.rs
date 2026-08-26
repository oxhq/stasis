/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A generic timer scheduler module that can be integrated into a crossbeam based event
//! loop or used to launch a background timer thread.

#![deny(unsafe_code)]

use std::cmp::{self, Ord};
use std::collections::{BTreeMap, BinaryHeap};
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, after, never};
use malloc_size_of_derive::MallocSizeOf;
use serde::{Deserialize, Serialize};

/// Immutable execution limits for one controlled document-clock domain.
///
/// These counters observe work at engine-owned hooks. Pre-work classes can be rejected at
/// admission; mutation-record accounting is non-rejecting. CPU and host wall time require an
/// interrupt/watchdog boundary and are intentionally not represented here.
#[derive(Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub struct DocumentExecutionLimits {
    /// Ordinary controlled event-loop turns that may begin.
    pub ordinary_tasks: u64,
    /// Individual microtask jobs that may begin.
    pub microtasks: u64,
    /// Rendering opportunities that may begin.
    pub rendering_opportunities: u64,
    /// Calls to Servo's central DOM mutation-record hook.
    pub mutations: u64,
}

impl DocumentExecutionLimits {
    /// Version-1 defaults for a controlled single-document web application.
    pub const CONTROLLED_WEBAPP_V1: Self = Self {
        ordinary_tasks: 100_000,
        microtasks: 1_000_000,
        rendering_opportunities: 10_000,
        mutations: 1_000_000,
    };
}

/// A controlled execution counter with a policy limit.
#[derive(Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub enum DocumentExecutionBudget {
    /// Ordinary controlled event-loop turns.
    OrdinaryTasks,
    /// Individual jobs removed from a microtask queue.
    Microtasks,
    /// Invocations of the HTML update-the-rendering algorithm.
    RenderingOpportunities,
    /// Calls to the central DOM mutation-record hook.
    MutationRecords,
}

/// A controlled execution counter retained as exact evidence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub enum DocumentExecutionCounter {
    /// Ordinary controlled event-loop turns.
    OrdinaryTasks,
    /// Individual jobs removed from a microtask queue.
    Microtasks,
    /// Invocations of the HTML update-the-rendering algorithm.
    RenderingOpportunities,
    /// Calls to the central DOM mutation-record hook.
    MutationRecords,
}

/// The first terminal execution failure latched for one controlled session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub enum DocumentExecutionTerminal {
    /// Starting or observing another unit would exceed its configured limit.
    BudgetExceeded {
        /// Work class whose policy boundary was reached.
        budget: DocumentExecutionBudget,
        /// Configured maximum.
        limit: u64,
        /// One-based unit rejected before work, or non-rejecting record observed beyond its limit.
        observed: u64,
    },
    /// An evidence counter exhausted its exact integer representation.
    CounterOverflow(DocumentExecutionCounter),
}

/// Exact counters retained by one controlled execution ledger.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub struct DocumentExecutionCounters {
    /// Ordinary event-loop turns admitted before work.
    pub ordinary_tasks: u64,
    /// Individual microtask jobs admitted before work.
    pub microtasks: u64,
    /// Rendering opportunities admitted before work.
    pub rendering_opportunities: u64,
    /// Invocations of the central DOM mutation-record hook.
    pub mutations: u64,
}

/// One mutex-consistent observation of controlled execution policy and use.
#[derive(Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub struct DocumentExecutionObservation {
    /// Stable owner identity for the document-clock/session domain whose work was counted.
    pub clock_id: DocumentClockId,
    /// Immutable limits installed before navigation.
    pub limits: DocumentExecutionLimits,
    /// Counters frozen when the first terminal failure is latched.
    pub counters: DocumentExecutionCounters,
    /// Sticky first failure, if a budget or representation boundary was reached.
    pub terminal: Option<DocumentExecutionTerminal>,
}

#[derive(Debug)]
struct DocumentExecutionLedgerState {
    clock_id: DocumentClockId,
    limits: DocumentExecutionLimits,
    counters: DocumentExecutionCounters,
    terminal: Option<DocumentExecutionTerminal>,
}

/// A clonable, session-scoped ledger shared by controlled ScriptThread work hooks.
#[derive(Clone, Debug, MallocSizeOf)]
pub struct DocumentExecutionLedger {
    #[ignore_malloc_size_of = "The execution ledger is shared and measured by its owner"]
    inner: Arc<Mutex<DocumentExecutionLedgerState>>,
}

/// Proof that one execution ledger was nonterminal when this guard was acquired.
///
/// The ledger lock remains held for the guard's lifetime, so a concurrent execution hook cannot
/// latch a terminal between a guarded precondition check and its engine-owned mutation.
#[derive(Debug)]
pub struct DocumentExecutionActiveGuard<'a> {
    _state: MutexGuard<'a, DocumentExecutionLedgerState>,
}

impl DocumentExecutionLedger {
    /// Create an empty ledger with immutable limits.
    pub fn new(clock_id: DocumentClockId, limits: DocumentExecutionLimits) -> Self {
        Self {
            inner: Arc::new(Mutex::new(DocumentExecutionLedgerState {
                clock_id,
                limits,
                counters: DocumentExecutionCounters::default(),
                terminal: None,
            })),
        }
    }

    /// Capture policy, counters, and the sticky first terminal failure under one lock.
    pub fn observation(&self) -> DocumentExecutionObservation {
        let state = self
            .inner
            .lock()
            .expect("document execution ledger poisoned");
        DocumentExecutionObservation {
            clock_id: state.clock_id,
            limits: state.limits,
            counters: state.counters,
            terminal: state.terminal,
        }
    }

    /// Lock this ledger only if it has not reached a terminal boundary.
    ///
    /// Keep the returned guard alive across the engine mutation that the terminal check protects.
    pub fn active_guard(
        &self,
    ) -> Result<DocumentExecutionActiveGuard<'_>, DocumentExecutionTerminal> {
        let state = self
            .inner
            .lock()
            .expect("document execution ledger poisoned");
        if let Some(terminal) = state.terminal {
            return Err(terminal);
        }
        Ok(DocumentExecutionActiveGuard { _state: state })
    }

    /// Admit one ordinary event-loop turn before any of its work runs.
    pub fn begin_ordinary_task(&self) -> Result<(), DocumentExecutionTerminal> {
        self.begin_budgeted(DocumentExecutionBudget::OrdinaryTasks)
    }

    /// Admit one microtask before invoking its job.
    pub fn begin_microtask(&self) -> Result<(), DocumentExecutionTerminal> {
        self.begin_budgeted(DocumentExecutionBudget::Microtasks)
    }

    /// Admit one rendering opportunity before invoking update-the-rendering.
    pub fn begin_rendering_opportunity(&self) -> Result<(), DocumentExecutionTerminal> {
        self.begin_budgeted(DocumentExecutionBudget::RenderingOpportunities)
    }

    /// Record one invocation of Servo's central DOM mutation-record hook.
    ///
    /// This is deliberately non-rejecting accounting: the observed record is included before the
    /// limit comparison, and a breach latches failure without suppressing or rolling back the DOM
    /// algorithm. Servo call sites do not all place this record hook on the same side of the
    /// underlying write, so this layer does not claim a universal post-write mutation generation.
    pub fn record_mutation_record(&self) {
        let mut state = self
            .inner
            .lock()
            .expect("document execution ledger poisoned");
        if state.terminal.is_some() {
            return;
        }
        let Some(observed) = state.counters.mutations.checked_add(1) else {
            state.terminal = Some(DocumentExecutionTerminal::CounterOverflow(
                DocumentExecutionCounter::MutationRecords,
            ));
            return;
        };
        state.counters.mutations = observed;
        let limit = state.limits.mutations;
        if observed > limit {
            state.terminal = Some(DocumentExecutionTerminal::BudgetExceeded {
                budget: DocumentExecutionBudget::MutationRecords,
                limit,
                observed,
            });
        }
    }

    fn begin_budgeted(
        &self,
        budget: DocumentExecutionBudget,
    ) -> Result<(), DocumentExecutionTerminal> {
        let mut state = self
            .inner
            .lock()
            .expect("document execution ledger poisoned");
        if let Some(terminal) = state.terminal {
            return Err(terminal);
        }
        let (current, limit, counter_kind) = match budget {
            DocumentExecutionBudget::OrdinaryTasks => (
                state.counters.ordinary_tasks,
                state.limits.ordinary_tasks,
                DocumentExecutionCounter::OrdinaryTasks,
            ),
            DocumentExecutionBudget::Microtasks => (
                state.counters.microtasks,
                state.limits.microtasks,
                DocumentExecutionCounter::Microtasks,
            ),
            DocumentExecutionBudget::RenderingOpportunities => (
                state.counters.rendering_opportunities,
                state.limits.rendering_opportunities,
                DocumentExecutionCounter::RenderingOpportunities,
            ),
            DocumentExecutionBudget::MutationRecords => unreachable!(
                "mutation records use non-rejecting accounting rather than pre-work admission"
            ),
        };
        let Some(observed) = current.checked_add(1) else {
            let terminal = DocumentExecutionTerminal::CounterOverflow(counter_kind);
            state.terminal = Some(terminal);
            return Err(terminal);
        };
        if observed > limit {
            let terminal = DocumentExecutionTerminal::BudgetExceeded {
                budget,
                limit,
                observed,
            };
            state.terminal = Some(terminal);
            return Err(terminal);
        }
        match budget {
            DocumentExecutionBudget::OrdinaryTasks => {
                state.counters.ordinary_tasks = observed;
            },
            DocumentExecutionBudget::Microtasks => {
                state.counters.microtasks = observed;
            },
            DocumentExecutionBudget::RenderingOpportunities => {
                state.counters.rendering_opportunities = observed;
            },
            DocumentExecutionBudget::MutationRecords => unreachable!(
                "mutation records use non-rejecting accounting rather than pre-work admission"
            ),
        }
        Ok(())
    }
}

/// A callback to pass to the [`TimerScheduler`] to be called when the timer is
/// dispatched.
pub type BoxedTimerCallback = Box<dyn Fn() + Send + 'static>;

/// Requests a TimerEvent-Message be sent after the given duration.
#[derive(MallocSizeOf)]
pub struct TimerEventRequest {
    #[ignore_malloc_size_of = "Size of a boxed function"]
    pub callback: BoxedTimerCallback,
    pub duration: Duration,
}

impl TimerEventRequest {
    fn dispatch(self) {
        (self.callback)()
    }
}

/// Configuration used when constructing a document-observable monotonic clock.
///
/// Interactive Servo uses [`Self::Realtime`]. The controlled mode does not advance by itself and
/// never creates a host-clock wake-up; its owner must advance and activate deadlines explicitly.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentClockConfiguration {
    /// Use elapsed host monotonic time.
    #[default]
    Realtime,
    /// Start a controlled clock at the supplied integer nanosecond offset.
    Controlled {
        /// Initial monotonic time in this document-clock domain.
        initial_time_ns: u128,
        /// Unix time, in nanoseconds, corresponding to [`DocumentTime::ZERO`].
        ///
        /// This is deliberately configuration rather than host time so JavaScript wall time and
        /// monotonic DOM time advance together without consulting the host clock.
        #[serde(default)]
        unix_time_origin_ns: DocumentUnixTime,
    },
}

/// An integer nanosecond offset in one document clock domain.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct DocumentTime(u128);

impl DocumentTime {
    /// The zero offset.
    pub const ZERO: Self = Self(0);

    /// Construct an offset from an integer number of nanoseconds.
    pub const fn from_nanos(nanos: u128) -> Self {
        Self(nanos)
    }

    /// Return this offset as integer nanoseconds.
    pub const fn as_nanos(self) -> u128 {
        self.0
    }

    /// Convert a duration without truncating or wrapping it.
    pub fn checked_from_duration(duration: Duration) -> Result<Self, DocumentClockError> {
        Ok(Self(duration.as_nanos()))
    }

    /// Convert this offset into a standard duration without truncating or wrapping it.
    pub fn checked_to_duration(self) -> Result<Duration, DocumentClockError> {
        let seconds =
            u64::try_from(self.0 / 1_000_000_000).map_err(|_| DocumentClockError::Overflow)?;
        let nanoseconds =
            u32::try_from(self.0 % 1_000_000_000).map_err(|_| DocumentClockError::Overflow)?;
        Ok(Duration::new(seconds, nanoseconds))
    }

    /// Add a duration without truncating or wrapping it.
    pub fn checked_add(self, duration: Duration) -> Result<Self, DocumentClockError> {
        let duration = Self::checked_from_duration(duration)?;
        self.0
            .checked_add(duration.0)
            .map(Self)
            .ok_or(DocumentClockError::Overflow)
    }

    /// Subtract a duration without wrapping it.
    pub fn checked_sub(self, duration: Duration) -> Result<Self, DocumentClockError> {
        let duration = Self::checked_from_duration(duration)?;
        self.0
            .checked_sub(duration.0)
            .map(Self)
            .ok_or(DocumentClockError::Overflow)
    }

    /// Return the duration since an earlier offset without hiding a mismatched or future origin.
    pub fn checked_duration_since(self, earlier: Self) -> Result<Duration, DocumentClockError> {
        let elapsed =
            self.0
                .checked_sub(earlier.0)
                .ok_or(DocumentClockError::TimeMovedBackwards {
                    current: earlier,
                    requested: self,
                })?;
        Self(elapsed).checked_to_duration()
    }
}

/// A signed Unix timestamp in integer nanoseconds.
///
/// This is deliberately distinct from [`DocumentTime`]: wall time can precede the Unix epoch,
/// while the monotonic document-clock offset cannot be negative.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Deserialize,
    Eq,
    MallocSizeOf,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
)]
pub struct DocumentUnixTime(i128);

impl DocumentUnixTime {
    /// Construct a signed Unix timestamp from an integer number of nanoseconds.
    pub const fn from_nanos(nanos: i128) -> Self {
        Self(nanos)
    }

    /// Return this Unix timestamp as signed integer nanoseconds.
    pub const fn as_nanos(self) -> i128 {
        self.0
    }

    fn checked_add_document_time(self, time: DocumentTime) -> Result<Self, DocumentClockError> {
        let time = i128::try_from(time.as_nanos()).map_err(|_| DocumentClockError::Overflow)?;
        self.0
            .checked_add(time)
            .map(Self)
            .ok_or(DocumentClockError::Overflow)
    }
}

const TIME_CLIP_LIMIT_MILLISECONDS: i128 = 8_640_000_000_000_000;
const NANOSECONDS_PER_MILLISECOND: i128 = 1_000_000;

fn checked_javascript_date_time_microseconds(
    unix_time: DocumentUnixTime,
) -> Result<f64, DocumentClockError> {
    let nanoseconds = unix_time.as_nanos();
    let whole_microseconds = nanoseconds.div_euclid(1000);
    let sub_microsecond_nanoseconds = nanoseconds.rem_euclid(1000);
    let exact_candidate_microseconds =
        whole_microseconds as f64 + sub_microsecond_nanoseconds as f64 / 1000.0;
    let time_clip_limit_nanoseconds = TIME_CLIP_LIMIT_MILLISECONDS * NANOSECONDS_PER_MILLISECOND;
    if nanoseconds < -time_clip_limit_nanoseconds || nanoseconds > time_clip_limit_nanoseconds {
        // Return the spec result directly: rounding this exact out-of-range value through f64
        // microseconds could otherwise collapse it back onto a finite TimeClip boundary.
        return Ok(f64::NAN);
    }

    let expected_milliseconds = nanoseconds / NANOSECONDS_PER_MILLISECOND;
    let millisecond_anchor = expected_milliseconds as f64 * 1000.0;
    let mut best = None;
    for anchor in [exact_candidate_microseconds, millisecond_anchor] {
        for candidate in [anchor, f64_next_down(anchor), f64_next_up(anchor)] {
            let observed_time_clip = simulated_javascript_date_time_clip(candidate);
            if !observed_time_clip.is_finite() ||
                observed_time_clip as i128 != expected_milliseconds
            {
                continue;
            }
            let distance = (candidate - exact_candidate_microseconds).abs();
            if best.is_none_or(|(_, best_distance)| distance < best_distance) {
                best = Some((candidate, distance));
            }
        }
    }
    if let Some((candidate, _)) = best {
        return Ok(candidate);
    }

    Err(DocumentClockError::JavaScriptDatePrecisionLoss {
        unix_time,
        expected_milliseconds,
        observed_milliseconds: (exact_candidate_microseconds / 1000.0).trunc() as i128,
    })
}

fn simulated_javascript_date_time_clip(candidate_microseconds: f64) -> f64 {
    let milliseconds = candidate_microseconds / 1000.0;
    if !milliseconds.is_finite() || milliseconds.abs() > TIME_CLIP_LIMIT_MILLISECONDS as f64 {
        return f64::NAN;
    }
    milliseconds.trunc() + 0.0
}

fn f64_next_up(value: f64) -> f64 {
    if value.is_nan() || value == f64::INFINITY {
        return value;
    }
    if value == 0.0 {
        return f64::from_bits(1);
    }
    let bits = value.to_bits();
    f64::from_bits(if value > 0.0 { bits + 1 } else { bits - 1 })
}

fn f64_next_down(value: f64) -> f64 {
    if value.is_nan() || value == f64::NEG_INFINITY {
        return value;
    }
    if value == 0.0 {
        return f64::from_bits((1_u64 << 63) | 1);
    }
    let bits = value.to_bits();
    f64::from_bits(if value > 0.0 { bits - 1 } else { bits + 1 })
}

/// One immutable timestamp captured for an "update the rendering" invocation.
///
/// The token prevents animation timelines and animation-frame callbacks from independently
/// sampling the clock while one rendering update is in progress.
#[derive(Clone, Copy, Debug, Eq, MallocSizeOf, PartialEq)]
pub struct DocumentRenderingTime(DocumentTime);

impl DocumentRenderingTime {
    /// Return the timestamp in the underlying document-clock domain.
    pub const fn document_time(self) -> DocumentTime {
        self.0
    }
}

/// Document-observable surfaces that eventually need to share one controlled clock.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u8)]
pub enum DocumentTimeSurface {
    /// Window timers on one script event loop.
    WindowTimers,
    /// A same-event-loop nested browsing context.
    SameEventLoopIframe,
    /// JavaScript `Date` in a Window realm.
    JavaScriptDate,
    /// `performance.now()` and `performance.timeOrigin` in a Window realm.
    Performance,
    /// A DOM high-resolution timestamp supplied by a host or another process.
    HostTimestamp,
    /// The timestamp captured once for an "update the rendering" invocation.
    UpdateRendering,
    /// Window `requestAnimationFrame` timestamps and scheduling.
    AnimationFrame,
    /// CSS and Web Animations document timelines.
    DocumentTimeline,
    /// Dedicated, shared, or service workers.
    Worker,
    /// A worklet running on another event loop.
    Worklet,
    /// A nested browsing context hosted by another script event loop.
    CrossEventLoopIframe,
    /// A navigation that would replace the controlled WebView's one event-loop authority.
    CrossEventLoopNavigation,
    /// An auxiliary WebView that would share or replace a controlled event-loop authority.
    AuxiliaryWebView,
    /// Resource-thread I/O whose callback or blocking lifecycle is not yet controlled.
    ResourceThreadIo,
    /// An externally-driven subscription whose lifecycle is not yet controlled.
    ExternalSubscription,
    /// A native media backend or callback lifecycle that is not yet controlled.
    NativeMedia,
    /// An embedder-owned control or dialog lifecycle that is not yet controlled.
    EmbedderControl,
    /// Script-requested session-history traversal (`back`, `forward`, or `go`).
    HistoryTraversal,
}

impl DocumentTimeSurface {
    const fn latch_value(self) -> u8 {
        self as u8 + 1
    }

    const fn from_latch_value(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::WindowTimers),
            2 => Some(Self::SameEventLoopIframe),
            3 => Some(Self::JavaScriptDate),
            4 => Some(Self::Performance),
            5 => Some(Self::HostTimestamp),
            6 => Some(Self::UpdateRendering),
            7 => Some(Self::AnimationFrame),
            8 => Some(Self::DocumentTimeline),
            9 => Some(Self::Worker),
            10 => Some(Self::Worklet),
            11 => Some(Self::CrossEventLoopIframe),
            12 => Some(Self::CrossEventLoopNavigation),
            13 => Some(Self::AuxiliaryWebView),
            14 => Some(Self::ResourceThreadIo),
            15 => Some(Self::ExternalSubscription),
            16 => Some(Self::NativeMedia),
            17 => Some(Self::EmbedderControl),
            18 => Some(Self::HistoryTraversal),
            _ => None,
        }
    }
}

/// A checked document-clock failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentClockError {
    /// The requested operation requires a controlled clock.
    RealtimeClock,
    /// Controlled document time cannot move backwards.
    TimeMovedBackwards {
        /// The current clock offset.
        current: DocumentTime,
        /// The rejected new clock offset.
        requested: DocumentTime,
    },
    /// A conditional advance observed a different offset than its scheduling precondition.
    TimeChanged {
        /// Offset bound by the conditional scheduling precondition.
        expected: DocumentTime,
        /// Offset observed at the controlled clock's linearization point.
        observed: DocumentTime,
    },
    /// An integer nanosecond conversion or calculation overflowed.
    Overflow,
    /// The current clock slices do not yet control the requested observable surface.
    UnsupportedSurface(DocumentTimeSurface),
    /// SpiderMonkey's f64-microsecond Date hook cannot preserve this otherwise-valid TimeClip
    /// millisecond.
    JavaScriptDatePrecisionLoss {
        /// Exact signed wall time that could not be transported without rounding to another
        /// page-visible millisecond.
        unix_time: DocumentUnixTime,
        /// Millisecond that exact TimeClip truncation would produce.
        expected_milliseconds: i128,
        /// Millisecond produced after the f64-microsecond callback transport and truncation.
        observed_milliseconds: i128,
    },
}

impl fmt::Display for DocumentClockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RealtimeClock => formatter.write_str("document clock is using host time"),
            Self::TimeMovedBackwards { current, requested } => write!(
                formatter,
                "document time cannot move backwards from {}ns to {}ns",
                current.as_nanos(),
                requested.as_nanos()
            ),
            Self::TimeChanged { expected, observed } => write!(
                formatter,
                "document time changed from expected {}ns to {}ns",
                expected.as_nanos(),
                observed.as_nanos()
            ),
            Self::Overflow => {
                formatter.write_str("document time conversion or calculation overflowed")
            },
            Self::UnsupportedSurface(surface) => {
                write!(
                    formatter,
                    "controlled document time does not yet support {surface:?}"
                )
            },
            Self::JavaScriptDatePrecisionLoss {
                unix_time,
                expected_milliseconds,
                observed_milliseconds,
            } => write!(
                formatter,
                "controlled JavaScript Date wall time {}ns would round from exact {}ms to {}ms",
                unix_time.as_nanos(),
                expected_milliseconds,
                observed_milliseconds
            ),
        }
    }
}

impl std::error::Error for DocumentClockError {}

static NEXT_DOCUMENT_CLOCK_ID: AtomicU64 = AtomicU64::new(1);

/// Stable process-local identity for one shared document clock domain.
#[derive(Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub struct DocumentClockId(u64);

impl DocumentClockId {
    /// Return the process-local clock identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug)]
struct ControlledClockState {
    now: DocumentTime,
    terminal: Option<DocumentClockError>,
}

enum DocumentClockInner {
    Realtime {
        origin: Instant,
    },
    Controlled {
        state: Mutex<ControlledClockState>,
        unix_time_origin_ns: DocumentUnixTime,
        unsupported_surface: AtomicU8,
    },
}

/// One clonable clock shared by the timer scheduler and every same-event-loop Window realm.
#[derive(Clone, MallocSizeOf)]
pub struct DocumentClock {
    id: DocumentClockId,
    #[ignore_malloc_size_of = "The clock state is shared and has no owned heap allocations"]
    inner: Arc<DocumentClockInner>,
}

impl Default for DocumentClock {
    fn default() -> Self {
        Self::new(DocumentClockConfiguration::Realtime)
    }
}

impl DocumentClock {
    /// Validate every controlled-time representation boundary without constructing a clock or
    /// consuming a process-local clock identifier.
    ///
    /// # Errors
    ///
    /// Returns [`DocumentClockError::Overflow`] when the configured initial monotonic offset and
    /// Unix-time origin cannot be combined exactly.
    #[doc(hidden)]
    pub fn validate_configuration(
        configuration: DocumentClockConfiguration,
    ) -> Result<(), DocumentClockError> {
        if let DocumentClockConfiguration::Controlled {
            initial_time_ns,
            unix_time_origin_ns,
        } = configuration
        {
            unix_time_origin_ns
                .checked_add_document_time(DocumentTime::from_nanos(initial_time_ns))?;
        }
        Ok(())
    }

    /// Construct a clock from its immutable mode configuration.
    pub fn new(configuration: DocumentClockConfiguration) -> Self {
        Self::try_new(configuration).expect("invalid document clock configuration")
    }

    /// Construct a clock after validating every controlled-time representation boundary.
    pub fn try_new(configuration: DocumentClockConfiguration) -> Result<Self, DocumentClockError> {
        Self::validate_configuration(configuration)?;
        let inner = match configuration {
            DocumentClockConfiguration::Realtime => DocumentClockInner::Realtime {
                origin: Instant::now(),
            },
            DocumentClockConfiguration::Controlled {
                initial_time_ns,
                unix_time_origin_ns,
            } => {
                let initial_time = DocumentTime::from_nanos(initial_time_ns);
                DocumentClockInner::Controlled {
                    state: Mutex::new(ControlledClockState {
                        now: initial_time,
                        terminal: None,
                    }),
                    unix_time_origin_ns,
                    unsupported_surface: AtomicU8::new(0),
                }
            },
        };
        let id = DocumentClockId(
            NEXT_DOCUMENT_CLOCK_ID
                .fetch_update(
                    AtomicOrdering::Relaxed,
                    AtomicOrdering::Relaxed,
                    |current| current.checked_add(1),
                )
                .expect("document clock identifier exhausted"),
        );
        Ok(Self {
            id,
            inner: Arc::new(inner),
        })
    }

    /// Return the identity shared by every clone in this clock domain.
    pub const fn id(&self) -> DocumentClockId {
        self.id
    }

    /// Return whether this clock is explicitly controlled rather than host-driven.
    pub fn is_controlled(&self) -> bool {
        matches!(&*self.inner, DocumentClockInner::Controlled { .. })
    }

    /// Return the first sticky controlled-clock terminal, if one has been latched.
    pub fn terminal_error(&self) -> Option<DocumentClockError> {
        match &*self.inner {
            DocumentClockInner::Realtime { .. } => None,
            DocumentClockInner::Controlled { state, .. } => {
                state
                    .lock()
                    .expect("controlled document clock poisoned")
                    .terminal
            },
        }
    }

    /// Latch a checked controlled-clock failure discovered by a document-surface adapter.
    ///
    /// Some surface conversions (for example, subtracting a replacement Document's time origin)
    /// occur outside the core clock mutex. They must retain their exact first failure in the same
    /// terminal domain before suppressing page-visible work. Realtime callers receive
    /// [`DocumentClockError::RealtimeClock`] and cannot manufacture a controlled terminal.
    pub fn latch_terminal_error(
        &self,
        error: DocumentClockError,
    ) -> Result<DocumentClockError, DocumentClockError> {
        let DocumentClockInner::Controlled { state, .. } = &*self.inner else {
            return Err(DocumentClockError::RealtimeClock);
        };
        let mut state = state.lock().expect("controlled document clock poisoned");
        if let Some(terminal) = state.terminal {
            return Ok(terminal);
        }
        state.terminal = Some(error);
        Ok(error)
    }

    /// Return the current offset, checking the host-duration conversion.
    pub fn try_now(&self) -> Result<DocumentTime, DocumentClockError> {
        match &*self.inner {
            DocumentClockInner::Realtime { origin } => {
                DocumentTime::checked_from_duration(origin.elapsed())
            },
            DocumentClockInner::Controlled { state, .. } => Ok(state
                .lock()
                .expect("controlled document clock poisoned")
                .now),
        }
    }

    /// Return the current offset.
    ///
    /// Realtime conversion remains checked even though the document-time representation is wider
    /// than a standard duration.
    pub fn now(&self) -> DocumentTime {
        self.try_now()
            .expect("document clock exceeded its checked integer nanosecond range")
    }

    /// Return the current offset after checking that the observable surface is controlled.
    pub fn now_for_surface(
        &self,
        surface: DocumentTimeSurface,
    ) -> Result<DocumentTime, DocumentClockError> {
        self.require_surface(surface)?;
        self.try_now()
    }

    /// Capture the one timestamp shared by all consumers in a rendering update.
    pub fn rendering_time(&self) -> Result<DocumentRenderingTime, DocumentClockError> {
        self.now_for_surface(DocumentTimeSurface::UpdateRendering)
            .map(DocumentRenderingTime)
    }

    /// Return a checked elapsed duration in the requested observable surface.
    pub fn duration_since_for_surface(
        &self,
        surface: DocumentTimeSurface,
        origin: DocumentTime,
        observed: DocumentTime,
    ) -> Result<Duration, DocumentClockError> {
        self.require_surface(surface)?;
        observed.checked_duration_since(origin)
    }

    /// Advance a controlled clock monotonically without sleeping.
    pub fn advance_to(&self, requested: DocumentTime) -> Result<(), DocumentClockError> {
        let DocumentClockInner::Controlled {
            state,
            unix_time_origin_ns,
            ..
        } = &*self.inner
        else {
            return Err(DocumentClockError::RealtimeClock);
        };

        let mut state = state.lock().expect("controlled document clock poisoned");
        if let Some(terminal) = state.terminal {
            return Err(terminal);
        }
        if requested < state.now {
            return Err(DocumentClockError::TimeMovedBackwards {
                current: state.now,
                requested,
            });
        }
        if let Err(error) = unix_time_origin_ns.checked_add_document_time(requested) {
            state.terminal = Some(error);
            return Err(error);
        }
        state.now = requested;
        Ok(())
    }

    /// Atomically advance only if the clock still equals an observed scheduling precondition.
    pub fn advance_from_to(
        &self,
        expected: DocumentTime,
        requested: DocumentTime,
    ) -> Result<(), DocumentClockError> {
        let DocumentClockInner::Controlled {
            state,
            unix_time_origin_ns,
            ..
        } = &*self.inner
        else {
            return Err(DocumentClockError::RealtimeClock);
        };
        let mut state = state.lock().expect("controlled document clock poisoned");
        if let Some(terminal) = state.terminal {
            return Err(terminal);
        }
        if requested < expected {
            return Err(DocumentClockError::TimeMovedBackwards {
                current: expected,
                requested,
            });
        }
        if state.now != expected {
            return Err(DocumentClockError::TimeChanged {
                expected,
                observed: state.now,
            });
        }
        if let Err(error) = unix_time_origin_ns.checked_add_document_time(requested) {
            state.terminal = Some(error);
            return Err(error);
        }
        state.now = requested;
        Ok(())
    }

    /// Return Unix time at a point in a controlled clock domain, checking integer overflow.
    pub fn unix_time_ns_at(
        &self,
        time: DocumentTime,
    ) -> Result<DocumentUnixTime, DocumentClockError> {
        let DocumentClockInner::Controlled {
            unix_time_origin_ns,
            ..
        } = &*self.inner
        else {
            return Err(DocumentClockError::RealtimeClock);
        };
        unix_time_origin_ns.checked_add_document_time(time)
    }

    /// Return current Unix time in a controlled clock domain, checking integer overflow.
    pub fn unix_time_ns(&self) -> Result<DocumentUnixTime, DocumentClockError> {
        self.unix_time_ns_at(self.try_now()?)
    }

    /// Return the f64-microsecond value consumed by SpiderMonkey's JavaScript Date hook.
    ///
    /// A value inside ECMAScript's TimeClip domain is returned only when the hook's later
    /// millisecond conversion preserves exact truncation. Otherwise this method latches and
    /// returns a typed terminal before rounded wall time can reach the page. An exact value outside
    /// TimeClip returns NaN directly without latching: that is the specified page-visible result,
    /// not a representation failure.
    pub fn javascript_date_time_microseconds(&self) -> Result<f64, DocumentClockError> {
        self.require_surface(DocumentTimeSurface::JavaScriptDate)?;
        let DocumentClockInner::Controlled {
            state,
            unix_time_origin_ns,
            ..
        } = &*self.inner
        else {
            return Err(DocumentClockError::RealtimeClock);
        };
        let mut state = state.lock().expect("controlled document clock poisoned");
        if let Some(terminal) = state.terminal {
            return Err(terminal);
        }
        let unix_time = unix_time_origin_ns.checked_add_document_time(state.now)?;
        match checked_javascript_date_time_microseconds(unix_time) {
            Ok(microseconds) => Ok(microseconds),
            Err(error) => {
                state.terminal = Some(error);
                Err(error)
            },
        }
    }

    /// Check whether the currently integrated slice controls an observable surface.
    ///
    /// This allow-list deliberately grows only in the same commit that routes a browser surface to
    /// this clock. Declaring a future surface early would let a controlled page leak host time
    /// without leaving fail-closed evidence.
    pub fn require_surface(&self, surface: DocumentTimeSurface) -> Result<(), DocumentClockError> {
        if !self.is_controlled() ||
            matches!(
                surface,
                DocumentTimeSurface::WindowTimers |
                    DocumentTimeSurface::JavaScriptDate |
                    DocumentTimeSurface::Performance |
                    DocumentTimeSurface::UpdateRendering |
                    DocumentTimeSurface::AnimationFrame |
                    DocumentTimeSurface::DocumentTimeline
            )
        {
            Ok(())
        } else {
            if let DocumentClockInner::Controlled {
                unsupported_surface,
                ..
            } = &*self.inner
            {
                let _ = unsupported_surface.compare_exchange(
                    0,
                    surface.latch_value(),
                    AtomicOrdering::AcqRel,
                    AtomicOrdering::Acquire,
                );
            }
            Err(DocumentClockError::UnsupportedSurface(surface))
        }
    }

    /// Return the first unsupported observable surface touched by this controlled clock.
    ///
    /// The latch is monotonic: once an unsupported surface is observed, later control-plane
    /// observations keep failing closed instead of allowing later activity to hide it.
    pub fn unsupported_surface(&self) -> Option<DocumentTimeSurface> {
        match &*self.inner {
            DocumentClockInner::Realtime { .. } => None,
            DocumentClockInner::Controlled {
                unsupported_surface,
                ..
            } => DocumentTimeSurface::from_latch_value(
                unsupported_surface.load(AtomicOrdering::Acquire),
            ),
        }
    }

    fn realtime_deadline(
        &self,
        deadline: DocumentTime,
    ) -> Result<Option<Instant>, DocumentClockError> {
        let DocumentClockInner::Realtime { origin } = &*self.inner else {
            return Ok(None);
        };
        origin
            .checked_add(deadline.checked_to_duration()?)
            .map(Some)
            .ok_or(DocumentClockError::Overflow)
    }
}

const DOCUMENT_PRODUCER_KIND_COUNT: usize = 5;
static NEXT_DOCUMENT_PRODUCER_FENCE_ID: AtomicU64 = AtomicU64::new(1);

/// A class of asynchronous work that can later affect a document's observable result.
///
/// The fence deliberately contains only producers owned by a single script event loop. Workers,
/// worklets, and cross-event-loop documents must be handled as separate execution surfaces rather
/// than being silently treated as idle.
#[derive(Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
#[repr(usize)]
pub enum DocumentProducerKind {
    /// A task queued through a Window's task manager.
    Task = 0,
    /// A document or navigation resource fetch.
    Resource = 1,
    /// A logical web-font load, including source fallback.
    Font = 2,
    /// An image-cache or vector-rasterization completion listener.
    Image = 3,
    /// An outbound one-shot callback whose reply will be handed off to a Window task.
    ExternalCallback = 4,
}

impl DocumentProducerKind {
    const fn index(self) -> usize {
        self as usize
    }
}

/// The stable enqueue identity assigned to one producer ticket.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct DocumentProducerSequence(u64);

impl DocumentProducerSequence {
    /// Return the global enqueue sequence within this fence.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Stable identity of one event-loop-owned producer fence.
#[derive(Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub struct DocumentProducerFenceId(u64);

impl DocumentProducerFenceId {
    /// Return the process-local fence identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Serializable identity for an explicitly acknowledged producer lease.
///
/// The ID carries no synchronization state. Its owning [`DocumentProducerFence`] validates that
/// the sequence is still live and belongs to the expected producer class before completing it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub struct DocumentProducerLeaseId {
    fence_id: DocumentProducerFenceId,
    sequence: DocumentProducerSequence,
    kind: DocumentProducerKind,
}

impl DocumentProducerLeaseId {
    /// Return the event-loop fence that issued this lease.
    pub const fn fence_id(self) -> DocumentProducerFenceId {
        self.fence_id
    }

    /// Return the stable global enqueue sequence for this lease.
    pub const fn sequence(self) -> DocumentProducerSequence {
        self.sequence
    }

    /// Return the producer class registered for this lease.
    pub const fn kind(self) -> DocumentProducerKind {
        self.kind
    }
}

/// Enqueue, completion, and pending watermarks for one producer class.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub struct DocumentProducerWatermark {
    enqueued: u64,
    completed: u64,
    pending: u64,
}

impl DocumentProducerWatermark {
    /// Number of tickets ever enqueued for this producer class.
    pub const fn enqueued(self) -> u64 {
        self.enqueued
    }

    /// Number of tickets whose producer callback or task has completed.
    pub const fn completed(self) -> u64 {
        self.completed
    }

    /// Number of currently live producer tickets.
    pub const fn pending(self) -> u64 {
        self.pending
    }
}

/// One mutex-consistent snapshot of all participating document producers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub struct DocumentProducerSnapshot {
    fence_id: DocumentProducerFenceId,
    terminal_error: Option<DocumentProducerFenceError>,
    revision: u64,
    enqueued: u64,
    completed: u64,
    pending: u64,
    by_kind: [DocumentProducerWatermark; DOCUMENT_PRODUCER_KIND_COUNT],
}

impl DocumentProducerSnapshot {
    /// Return the identity of the fence that produced this snapshot.
    pub const fn fence_id(self) -> DocumentProducerFenceId {
        self.fence_id
    }

    /// Return the first terminal producer-lifecycle failure without clearing it.
    pub const fn terminal_error(self) -> Option<DocumentProducerFenceError> {
        self.terminal_error
    }

    /// A mutation sequence incremented for every enqueue and completion.
    pub const fn revision(self) -> u64 {
        self.revision
    }

    /// Global number of producer tickets ever enqueued.
    pub const fn enqueued(self) -> u64 {
        self.enqueued
    }

    /// Global number of producer tickets completed.
    pub const fn completed(self) -> u64 {
        self.completed
    }

    /// Global number of live producer tickets.
    pub const fn pending(self) -> u64 {
        self.pending
    }

    /// Return whether no participating producer ticket is live.
    pub const fn is_empty(self) -> bool {
        self.pending == 0
    }

    /// Return watermarks for one producer class.
    pub const fn for_kind(self, kind: DocumentProducerKind) -> DocumentProducerWatermark {
        self.by_kind[kind.index()]
    }
}

#[derive(Default)]
struct DocumentProducerFenceState {
    terminal_error: Option<DocumentProducerFenceError>,
    revision: u64,
    enqueued: u64,
    completed: u64,
    pending: u64,
    by_kind: [DocumentProducerWatermark; DOCUMENT_PRODUCER_KIND_COUNT],
    active_leases: BTreeMap<DocumentProducerSequence, DocumentProducerKind>,
}

impl DocumentProducerFenceState {
    fn snapshot(&self, fence_id: DocumentProducerFenceId) -> DocumentProducerSnapshot {
        DocumentProducerSnapshot {
            fence_id,
            terminal_error: self.terminal_error,
            revision: self.revision,
            enqueued: self.enqueued,
            completed: self.completed,
            pending: self.pending,
            by_kind: self.by_kind,
        }
    }
}

/// A checked producer-fence failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub enum DocumentProducerFenceError {
    /// A sequence or watermark would exceed the `u64` representation.
    CounterOverflow,
    /// A bounded producer registration was rejected before it could create a lease.
    AdmissionLimitExceeded {
        /// Producer class whose retained-registration boundary was reached.
        kind: DocumentProducerKind,
        /// Configured maximum number of retained registrations.
        limit: u64,
        /// One-based registration rejected at the boundary.
        observed: u64,
    },
    /// No event-loop microtask checkpoint has completed yet.
    CheckpointNotCompleted,
    /// An observation reused or moved backwards from an already observed checkpoint.
    StaleCheckpoint {
        /// The last checkpoint accepted by this observer.
        previous: DocumentProducerCheckpoint,
        /// The rejected checkpoint.
        observed: DocumentProducerCheckpoint,
    },
    /// A lease acknowledgement did not name a live lease on this fence.
    UnknownLease(DocumentProducerLeaseId),
    /// A producer lost its completion channel before delivering its terminal handoff.
    ProducerAbandoned(DocumentProducerLeaseId),
}

impl fmt::Display for DocumentProducerFenceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for DocumentProducerFenceError {}

/// A producer snapshot changed before a conditional action reached its linearization point.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentProducerSnapshotMismatch {
    observed: Box<DocumentProducerSnapshot>,
}

impl DocumentProducerSnapshotMismatch {
    /// Return the producer state observed under the fence lock.
    pub fn observed(&self) -> DocumentProducerSnapshot {
        *self.observed
    }
}

/// A clonable, linearizable fence shared by all participating producers on one event loop.
///
/// Enqueue and completion mutate one locked state so a snapshot cannot combine watermarks from
/// different instants. Every successful enqueue reserves enough revision space for its eventual
/// RAII completion, making overflow a checked enqueue failure rather than a fallible destructor.
#[derive(Clone, MallocSizeOf)]
pub struct DocumentProducerFence {
    fence_id: DocumentProducerFenceId,
    #[ignore_malloc_size_of = "The producer state is shared and measured by its owner"]
    inner: Arc<Mutex<DocumentProducerFenceState>>,
    #[ignore_malloc_size_of = "The optional host notifier is shared embedding state"]
    state_change_notifier: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for DocumentProducerFence {
    fn default() -> Self {
        Self::with_notifier(None)
    }
}

impl DocumentProducerFence {
    /// Construct a producer fence which optionally wakes its host after every committed change.
    ///
    /// Notification occurs after releasing the fence lock and is independent of any execution
    /// accounting or settlement policy.
    pub fn with_notifier(state_change_notifier: Option<Arc<dyn Fn() + Send + Sync>>) -> Self {
        let fence_id = DocumentProducerFenceId(
            NEXT_DOCUMENT_PRODUCER_FENCE_ID
                .fetch_update(
                    AtomicOrdering::Relaxed,
                    AtomicOrdering::Relaxed,
                    |current| current.checked_add(1),
                )
                .expect("document producer fence identifier exhausted"),
        );
        Self {
            fence_id,
            inner: Arc::new(Mutex::new(DocumentProducerFenceState::default())),
            state_change_notifier,
        }
    }

    /// Begin one producer operation and return its stable RAII ticket.
    pub fn begin(
        &self,
        kind: DocumentProducerKind,
    ) -> Result<DocumentProducerGuard, DocumentProducerFenceError> {
        let mut state = self.inner.lock().expect("document producer fence poisoned");
        if let Some(error) = state.terminal_error {
            return Err(error);
        }
        let index = kind.index();

        macro_rules! checked_or_latch_overflow {
            ($value:expr) => {
                match $value {
                    Some(value) => value,
                    None => {
                        let error = DocumentProducerFenceError::CounterOverflow;
                        state.terminal_error = Some(error);
                        drop(state);
                        self.notify_state_change();
                        return Err(error);
                    },
                }
            };
        }

        let revision = checked_or_latch_overflow!(state.revision.checked_add(1));
        let enqueued = checked_or_latch_overflow!(state.enqueued.checked_add(1));
        let pending = checked_or_latch_overflow!(state.pending.checked_add(1));
        let kind_enqueued =
            checked_or_latch_overflow!(state.by_kind[index].enqueued.checked_add(1));
        let kind_pending = checked_or_latch_overflow!(state.by_kind[index].pending.checked_add(1));

        // Each live ticket will consume one more revision when it completes. Reserve all of those
        // future increments before admitting another ticket so guard destruction stays infallible.
        checked_or_latch_overflow!(revision.checked_add(pending));

        state.revision = revision;
        state.enqueued = enqueued;
        state.pending = pending;
        state.by_kind[index].enqueued = kind_enqueued;
        state.by_kind[index].pending = kind_pending;

        let lease_id = DocumentProducerLeaseId {
            fence_id: self.fence_id,
            sequence: DocumentProducerSequence(enqueued),
            kind,
        };
        let previous = state.active_leases.insert(lease_id.sequence, kind);
        debug_assert!(previous.is_none());

        drop(state);
        self.notify_state_change();

        Ok(DocumentProducerGuard {
            fence: self.clone(),
            lease_id: Some(lease_id),
        })
    }

    /// Latch a bounded producer-registration failure without inventing a producer lease.
    ///
    /// Registration capacity is owned by the adapter retaining the callback, rather than by the
    /// fence's message watermarks. This operation records that checked admission failure in the
    /// same sticky terminal domain while leaving enqueue, completion, and pending counts exact.
    /// The terminal field is part of snapshot identity, so an already captured snapshot cannot
    /// remain authoritative across the rejected admission. Revision and watermarks stay unchanged
    /// to preserve the fence conservation law `revision == enqueued + completed`.
    pub fn latch_admission_limit_exceeded(
        &self,
        kind: DocumentProducerKind,
        limit: u64,
        observed: u64,
    ) -> DocumentProducerFenceError {
        debug_assert!(observed > limit);
        let mut state = self.inner.lock().expect("document producer fence poisoned");
        if let Some(error) = state.terminal_error {
            return error;
        }

        let error = DocumentProducerFenceError::AdmissionLimitExceeded {
            kind,
            limit,
            observed,
        };
        state.terminal_error = Some(error);
        drop(state);
        self.notify_state_change();
        error
    }

    /// Capture all producer watermarks under one lock acquisition.
    pub fn snapshot(&self) -> DocumentProducerSnapshot {
        self.inner
            .lock()
            .expect("document producer fence poisoned")
            .snapshot(self.fence_id)
    }

    /// Wake the host after externally committing work already represented by a live ticket.
    ///
    /// This does not mutate producer state or its revision. It closes the handoff window where
    /// `begin` must notify before a producer can commit its task to an external queue: the queue
    /// owner receives a second notification after that commit becomes observable.
    #[doc(hidden)]
    pub fn notify_observer_after_commit(&self) {
        self.notify_state_change();
    }

    /// Run an action only while the producer snapshot still exactly matches `expected`.
    ///
    /// Producer enqueue and completion remain blocked until `action` returns. The action must be
    /// small and non-reentrant: calling `begin`, `complete_lease`, `snapshot`,
    /// `with_matching_snapshot`, or any other operation on this same fence will deadlock. It must
    /// not dispatch callbacks that can reach the same fence either.
    pub fn with_matching_snapshot<T>(
        &self,
        expected: DocumentProducerSnapshot,
        action: impl FnOnce() -> T,
    ) -> Result<T, DocumentProducerSnapshotMismatch> {
        let state = self.inner.lock().expect("document producer fence poisoned");
        let observed = state.snapshot(self.fence_id);
        if observed != expected {
            return Err(DocumentProducerSnapshotMismatch {
                observed: Box::new(observed),
            });
        }
        let result = catch_unwind(AssertUnwindSafe(action));
        drop(state);
        match result {
            Ok(result) => Ok(result),
            Err(payload) => resume_unwind(payload),
        }
    }

    /// Return the stable identity bound to this event-loop fence.
    pub const fn id(&self) -> DocumentProducerFenceId {
        self.fence_id
    }

    /// Complete a live lease after its terminal message has been handled.
    pub fn complete_lease(
        &self,
        lease_id: DocumentProducerLeaseId,
    ) -> Result<(), DocumentProducerFenceError> {
        self.finish_lease(lease_id, None)
    }

    /// Complete a live lease while atomically latching that its producer was abandoned.
    ///
    /// Abandonment consumes the same completion watermark and revision reserved by [`Self::begin`]
    /// as an ordinary completion. The first producer terminal remains sticky, so a later
    /// abandonment cannot hide an earlier failure.
    fn abandon_lease(
        &self,
        lease_id: DocumentProducerLeaseId,
    ) -> Result<(), DocumentProducerFenceError> {
        self.finish_lease(
            lease_id,
            Some(DocumentProducerFenceError::ProducerAbandoned(lease_id)),
        )
    }

    fn finish_lease(
        &self,
        lease_id: DocumentProducerLeaseId,
        terminal_error: Option<DocumentProducerFenceError>,
    ) -> Result<(), DocumentProducerFenceError> {
        if lease_id.fence_id != self.fence_id {
            return Err(DocumentProducerFenceError::UnknownLease(lease_id));
        }
        let mut state = self.inner.lock().expect("document producer fence poisoned");
        if state.active_leases.get(&lease_id.sequence) != Some(&lease_id.kind) {
            return Err(DocumentProducerFenceError::UnknownLease(lease_id));
        }
        if state.terminal_error.is_none() {
            state.terminal_error = terminal_error;
        }
        state.active_leases.remove(&lease_id.sequence);
        let index = lease_id.kind.index();

        // `begin` reserves this revision and every completion count is bounded by its matching
        // checked enqueue count, so these failures indicate an internal fence invariant bug.
        state.revision = state
            .revision
            .checked_add(1)
            .expect("reserved document producer completion revision exhausted");
        state.completed = state
            .completed
            .checked_add(1)
            .expect("document producer completion exceeded enqueue watermark");
        state.pending = state
            .pending
            .checked_sub(1)
            .expect("document producer ticket completed twice");
        state.by_kind[index].completed = state.by_kind[index]
            .completed
            .checked_add(1)
            .expect("document producer kind completion exceeded enqueue watermark");
        state.by_kind[index].pending = state.by_kind[index]
            .pending
            .checked_sub(1)
            .expect("document producer kind ticket completed twice");
        drop(state);
        self.notify_state_change();
        Ok(())
    }

    fn notify_state_change(&self) {
        if let Some(notify) = &self.state_change_notifier {
            // Host wake notification is best effort. In particular, a notifier panic must not
            // strand a just-admitted lease before `begin` can return its guard, nor may it escape
            // from the RAII guard's destructor after a valid completion was committed.
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| notify())) {
                // A custom panic payload may itself panic from `Drop`. This is a host-waker
                // failure path, so absolute containment is more important than reclaiming that
                // exceptional allocation.
                std::mem::forget(payload);
            }
        }
    }
}

/// A live producer ticket. Dropping it records completion after the guarded callback or task.
pub struct DocumentProducerGuard {
    fence: DocumentProducerFence,
    lease_id: Option<DocumentProducerLeaseId>,
}

impl DocumentProducerGuard {
    /// Return this ticket's stable global enqueue sequence.
    pub const fn sequence(&self) -> DocumentProducerSequence {
        self.lease_id
            .expect("a detached document producer guard has no sequence")
            .sequence
    }

    /// Transfer completion responsibility to a serializable lease ID.
    pub fn into_lease_id(mut self) -> DocumentProducerLeaseId {
        self.lease_id
            .take()
            .expect("document producer guard detached twice")
    }

    /// Consume this guard and mark its producer as abandoned instead of normally completed.
    ///
    /// This is reserved for adapters that lose a response channel before its protocol terminal.
    /// The lease is still completed, but the fence retains a sticky terminal fact so an observer
    /// cannot mistake the now-empty producer set for successful quiescence.
    pub fn abandon(mut self) -> Result<(), DocumentProducerFenceError> {
        let lease_id = self
            .lease_id
            .take()
            .expect("document producer guard abandoned twice");
        self.fence.abandon_lease(lease_id)
    }
}

impl Drop for DocumentProducerGuard {
    fn drop(&mut self) {
        if let Some(lease_id) = self.lease_id.take() {
            self.fence
                .complete_lease(lease_id)
                .expect("live document producer guard named an unknown lease");
        }
    }
}

/// A monotonically increasing token created only after an event-loop microtask checkpoint.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Deserialize,
    Eq,
    MallocSizeOf,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
)]
pub struct DocumentProducerCheckpoint(u64);

impl DocumentProducerCheckpoint {
    /// The initial token before any checkpoint has completed.
    pub const ZERO: Self = Self(0);

    /// Return the underlying checkpoint sequence.
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Advance to the next checkpoint without wrapping.
    pub fn checked_next(self) -> Result<Self, DocumentProducerFenceError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(DocumentProducerFenceError::CounterOverflow)
    }
}

/// Mechanical result of one fenced observation after a microtask checkpoint.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum DocumentProducerObservation {
    /// At least one producer is live.
    Busy(DocumentProducerSnapshot),
    /// This is the first empty snapshot at this revision.
    FirstEmpty(DocumentProducerSnapshot),
    /// Two fresh checkpoints observed the same empty producer revision.
    StableEmpty(DocumentProducerSnapshot),
}

/// Per-driver state that qualifies an empty producer fence only across two fresh checkpoints.
#[derive(Default)]
pub struct DocumentProducerObserver {
    last_checkpoint: Option<DocumentProducerCheckpoint>,
    last_empty: Option<DocumentProducerSnapshot>,
}

impl DocumentProducerObserver {
    /// Observe the fence after `checkpoint` and mechanically qualify stable emptiness.
    pub fn observe(
        &mut self,
        fence: &DocumentProducerFence,
        checkpoint: DocumentProducerCheckpoint,
    ) -> Result<DocumentProducerObservation, DocumentProducerFenceError> {
        if checkpoint == DocumentProducerCheckpoint::ZERO {
            return Err(DocumentProducerFenceError::CheckpointNotCompleted);
        }
        if let Some(previous) = self.last_checkpoint &&
            checkpoint <= previous
        {
            return Err(DocumentProducerFenceError::StaleCheckpoint {
                previous,
                observed: checkpoint,
            });
        }
        self.last_checkpoint = Some(checkpoint);

        let snapshot = fence.snapshot();
        if !snapshot.is_empty() {
            self.last_empty = None;
            return Ok(DocumentProducerObservation::Busy(snapshot));
        }

        let stable = self.last_empty == Some(snapshot);
        self.last_empty = Some(snapshot);
        if stable {
            Ok(DocumentProducerObservation::StableEmpty(snapshot))
        } else {
            Ok(DocumentProducerObservation::FirstEmpty(snapshot))
        }
    }
}

#[derive(MallocSizeOf)]
struct ScheduledEvent {
    id: TimerId,
    request: TimerEventRequest,
    deadline: DocumentTime,
}

impl Ord for ScheduledEvent {
    fn cmp(&self, other: &ScheduledEvent) -> cmp::Ordering {
        match self.deadline.cmp(&other.deadline).reverse() {
            cmp::Ordering::Equal => self.id.cmp(&other.id).reverse(),
            ordering => ordering,
        }
    }
}

impl PartialOrd for ScheduledEvent {
    fn partial_cmp(&self, other: &ScheduledEvent) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for ScheduledEvent {}
impl PartialEq for ScheduledEvent {
    fn eq(&self, other: &ScheduledEvent) -> bool {
        self.id == other.id
    }
}

/// A stable timer identity whose value is also its scheduler insertion order.
#[derive(
    Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, Ord, PartialEq, PartialOrd, Serialize,
)]
pub struct TimerId(u64);

impl TimerId {
    /// Return the stable scheduler insertion sequence.
    pub const fn sequence(self) -> u64 {
        self.0
    }
}

static NEXT_TIMER_SCHEDULER_ID: AtomicU64 = AtomicU64::new(1);

/// Stable process-local identity for one timer scheduler.
#[derive(Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub struct TimerSchedulerId(u64);

impl TimerSchedulerId {
    /// Return the process-local scheduler identifier.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// The finite deadline exposed by a controlled scheduler.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TimerDeadlineSnapshot {
    /// Identity of the scheduler that issued this snapshot.
    pub scheduler_id: TimerSchedulerId,
    /// Stable identity and insertion order for this event.
    pub id: TimerId,
    /// Absolute integer-nanosecond offset in the document clock.
    pub deadline: DocumentTime,
}

/// The result of joining one scheduler-local timer identity to its live deadline.
///
/// Results from [`TimerScheduler::join_live_deadlines`] are aligned with the requested timer IDs.
/// A missing deadline means that identity is no longer pending in the named scheduler; it is not
/// silently omitted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub struct TimerDeadlineJoin {
    /// Identity of the scheduler against which this timer ID was resolved.
    pub scheduler_id: TimerSchedulerId,
    /// Scheduler-local timer identity supplied by the caller.
    pub id: TimerId,
    /// Absolute controlled-clock deadline when this timer is still live.
    pub deadline: Option<DocumentTime>,
}

/// A checked failure while joining scheduler-local IDs to controlled deadlines.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TimerDeadlineJoinError {
    /// The lookup was requested from a realtime scheduler.
    RealtimeScheduler,
    /// The timer IDs were observed in another scheduler's scope.
    SchedulerMismatch {
        /// Identity of the scheduler receiving the lookup.
        expected: TimerSchedulerId,
        /// Scheduler identity supplied by the caller.
        observed: TimerSchedulerId,
    },
}

impl fmt::Display for TimerDeadlineJoinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for TimerDeadlineJoinError {}

/// One timer callback detached from its scheduler but not yet dispatched.
///
/// This value deliberately exposes no callback accessor, cloning, or serialization. A controller
/// can detach it while holding a producer exclusion, return it from that exclusion, and consume it
/// only after the exclusion has been released.
#[derive(MallocSizeOf)]
#[must_use = "a detached timer event must be explicitly dispatched or deliberately dropped"]
pub struct DetachedTimerEvent {
    request: TimerEventRequest,
}

impl DetachedTimerEvent {
    /// Dispatch the detached callback exactly once.
    pub fn dispatch(self) {
        self.request.dispatch();
    }
}

/// A checked scheduler/control failure.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum TimerControlError {
    /// The underlying document clock rejected an operation.
    Clock(DocumentClockError),
    /// Adding a request duration exceeded the document-time range.
    DeadlineOverflow,
    /// The stable timer insertion sequence was exhausted.
    SequenceExhausted,
    /// A finite-deadline operation was requested from a realtime scheduler.
    RealtimeScheduler,
    /// The caller supplied a snapshot that is not current for this scheduler.
    StaleDeadline {
        /// Snapshot supplied by the caller.
        expected: TimerDeadlineSnapshot,
        /// Current next snapshot, if any.
        observed: Option<TimerDeadlineSnapshot>,
    },
    /// The selected timer is still in the future.
    TimerNotDue {
        /// Selected timer deadline.
        deadline: DocumentTime,
        /// Current controlled time.
        now: DocumentTime,
    },
}

impl From<DocumentClockError> for TimerControlError {
    fn from(error: DocumentClockError) -> Self {
        Self::Clock(error)
    }
}

impl fmt::Display for TimerControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl std::error::Error for TimerControlError {}

/// A queue of [`TimerEventRequest`]s that are stored in order of next-to-fire.
#[derive(MallocSizeOf)]
pub struct TimerScheduler {
    /// Stable process-local identity for snapshots issued by this scheduler.
    id: TimerSchedulerId,
    /// A priority queue of future events, sorted by due time and insertion sequence.
    queue: BinaryHeap<ScheduledEvent>,
    /// The next stable timer insertion sequence.
    next_id: u64,
    /// The same clock used by the DOM timer layer for this event loop.
    clock: DocumentClock,
}

impl Default for TimerScheduler {
    fn default() -> Self {
        Self::with_clock(DocumentClock::default())
    }
}

impl TimerScheduler {
    /// Create a scheduler driven by the supplied document clock.
    pub fn with_clock(clock: DocumentClock) -> Self {
        let id = TimerSchedulerId(
            NEXT_TIMER_SCHEDULER_ID
                .fetch_update(
                    AtomicOrdering::Relaxed,
                    AtomicOrdering::Relaxed,
                    |current| current.checked_add(1),
                )
                .expect("timer scheduler identifier exhausted"),
        );
        Self {
            id,
            queue: BinaryHeap::new(),
            next_id: 0,
            clock,
        }
    }

    /// Return this scheduler's stable process-local identity.
    pub const fn id(&self) -> TimerSchedulerId {
        self.id
    }

    /// Return the scheduler's shared document clock.
    pub fn clock(&self) -> DocumentClock {
        self.clock.clone()
    }

    /// Schedule a timer, returning a typed failure instead of truncating time or sequence values.
    pub fn try_schedule_timer(
        &mut self,
        request: TimerEventRequest,
    ) -> Result<TimerId, TimerControlError> {
        if let Some(terminal) = self.clock.terminal_error() {
            return Err(terminal.into());
        }
        let deadline = self
            .clock
            .try_now()?
            .checked_add(request.duration)
            .map_err(|_| TimerControlError::DeadlineOverflow)?;
        if !self.clock.is_controlled() {
            self.clock
                .realtime_deadline(deadline)
                .map_err(|_| TimerControlError::DeadlineOverflow)?;
        }
        let next_id = self
            .next_id
            .checked_add(1)
            .ok_or(TimerControlError::SequenceExhausted)?;
        let id = TimerId(self.next_id);
        self.next_id = next_id;
        self.queue.push(ScheduledEvent {
            id,
            request,
            deadline,
        });
        Ok(id)
    }

    /// Schedule a timer for an interactive caller whose bounded durations are already validated.
    pub fn schedule_timer(&mut self, request: TimerEventRequest) -> TimerId {
        self.try_schedule_timer(request)
            .expect("validated timer request exceeded the checked scheduler range")
    }

    /// Cancel a timer with the given [`TimerId`]. If it is no longer pending, do nothing.
    pub fn cancel_timer(&mut self, id: TimerId) {
        self.queue.retain(|event| event.id != id);
    }

    /// Get a receiver that wakes for the next realtime timer.
    ///
    /// Controlled schedulers never wake from host time.
    pub fn wait_channel(&self) -> Receiver<Instant> {
        self.next_deadline()
            .map(|deadline| {
                let now = Instant::now();
                after(deadline.saturating_duration_since(now))
            })
            .unwrap_or_else(never)
    }

    /// The host deadline of the next realtime timer.
    ///
    /// Returns `None` for controlled schedulers.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.queue.peek().and_then(|event| {
            self.clock
                .realtime_deadline(event.deadline)
                .expect("a queued realtime deadline must have been validated before insertion")
        })
    }

    /// Return the next finite controlled deadline.
    pub fn finite_deadline_snapshot(
        &self,
    ) -> Result<Option<TimerDeadlineSnapshot>, TimerControlError> {
        if !self.clock.is_controlled() {
            return Err(TimerControlError::RealtimeScheduler);
        }
        Ok(self.queue.peek().map(|event| TimerDeadlineSnapshot {
            scheduler_id: self.id,
            id: event.id,
            deadline: event.deadline,
        }))
    }

    /// Join arbitrary scheduler-local timer IDs to their currently live controlled deadlines.
    ///
    /// The returned vector has exactly the same length and order as `timer_ids`, including
    /// duplicate and missing IDs. Every row carries this scheduler's identity so downstream
    /// observations cannot accidentally treat bare [`TimerId`] values as process-global. The
    /// caller must supply the scheduler identity observed alongside those IDs; a foreign scope is
    /// rejected before any values are joined.
    pub fn join_live_deadlines(
        &self,
        expected_scheduler_id: TimerSchedulerId,
        timer_ids: &[TimerId],
    ) -> Result<Vec<TimerDeadlineJoin>, TimerDeadlineJoinError> {
        if expected_scheduler_id != self.id {
            return Err(TimerDeadlineJoinError::SchedulerMismatch {
                expected: self.id,
                observed: expected_scheduler_id,
            });
        }
        if !self.clock.is_controlled() {
            return Err(TimerDeadlineJoinError::RealtimeScheduler);
        }

        let mut live_deadlines = timer_ids
            .iter()
            .copied()
            .map(|id| (id, None))
            .collect::<BTreeMap<_, _>>();
        for event in &self.queue {
            if let Some(deadline) = live_deadlines.get_mut(&event.id) {
                *deadline = Some(event.deadline);
            }
        }

        Ok(timer_ids
            .iter()
            .copied()
            .map(|id| TimerDeadlineJoin {
                scheduler_id: self.id,
                id,
                deadline: live_deadlines[&id],
            })
            .collect())
    }

    /// Require one exact finite deadline snapshot without mutating the scheduler or clock.
    pub fn validate_deadline_snapshot(
        &self,
        expected: TimerDeadlineSnapshot,
    ) -> Result<(), TimerControlError> {
        let observed = self.finite_deadline_snapshot()?;
        if observed != Some(expected) {
            return Err(TimerControlError::StaleDeadline { expected, observed });
        }
        Ok(())
    }

    /// Advance the shared controlled clock monotonically without activating a timer.
    pub fn advance_controlled_time_to(&self, now: DocumentTime) -> Result<(), TimerControlError> {
        self.clock.advance_to(now).map_err(Into::into)
    }

    /// Validate one fresh finite deadline, advance to it, and activate exactly that event.
    ///
    /// Validation happens before the clock mutates so a canceled or replaced snapshot cannot move
    /// controlled time and then fail stale.
    pub fn advance_to_and_activate(
        &mut self,
        expected: TimerDeadlineSnapshot,
    ) -> Result<(), TimerControlError> {
        let expected_now = self.clock.try_now()?;
        let detached = self.validate_advance_and_detach(expected_now, expected)?;
        detached.dispatch();
        Ok(())
    }

    /// Validate one exact controlled deadline and observed clock offset, advance, and detach the
    /// selected event without dispatching its callback.
    ///
    /// Snapshot validation precedes clock mutation. Conditional clock advancement precedes queue
    /// mutation. Therefore a foreign or stale snapshot, clock drift, backwards time, or checked
    /// wall-time overflow leaves the selected event attached and never invokes its callback.
    ///
    /// The returned event should leave any producer exclusion and mutable scheduler borrow before
    /// [`DetachedTimerEvent::dispatch`] is called.
    pub fn validate_advance_and_detach(
        &mut self,
        expected_now: DocumentTime,
        expected: TimerDeadlineSnapshot,
    ) -> Result<DetachedTimerEvent, TimerControlError> {
        self.validate_deadline_snapshot(expected)?;
        self.clock
            .advance_from_to(expected_now, expected.deadline)?;
        Ok(self.pop_validated_head(expected))
    }

    /// Validate one fresh finite deadline and advance to it without dispatching its callback.
    ///
    /// This seam lets a controller linearize validation and clock mutation separately from
    /// callback activation.
    pub fn validate_and_advance_to(
        &self,
        expected: TimerDeadlineSnapshot,
    ) -> Result<(), TimerControlError> {
        self.validate_deadline_snapshot(expected)?;
        self.clock.advance_to(expected.deadline).map_err(Into::into)
    }

    /// Validate one deadline and require the previously observed clock offset at one linearization
    /// point.
    pub fn validate_and_advance_from(
        &self,
        expected_now: DocumentTime,
        expected: TimerDeadlineSnapshot,
    ) -> Result<(), TimerControlError> {
        self.validate_deadline_snapshot(expected)?;
        self.clock
            .advance_from_to(expected_now, expected.deadline)
            .map_err(Into::into)
    }

    /// Activate exactly one due event selected from a fresh finite-deadline snapshot.
    pub fn activate_due_timer(
        &mut self,
        expected: TimerDeadlineSnapshot,
    ) -> Result<(), TimerControlError> {
        let detached = self.validate_and_detach_due(expected)?;
        detached.dispatch();
        Ok(())
    }

    fn validate_and_detach_due(
        &mut self,
        expected: TimerDeadlineSnapshot,
    ) -> Result<DetachedTimerEvent, TimerControlError> {
        if let Some(terminal) = self.clock.terminal_error() {
            return Err(terminal.into());
        }
        let observed = self.finite_deadline_snapshot()?;
        if observed != Some(expected) {
            return Err(TimerControlError::StaleDeadline { expected, observed });
        }
        let now = self.clock.try_now()?;
        if expected.deadline > now {
            return Err(TimerControlError::TimerNotDue {
                deadline: expected.deadline,
                now,
            });
        }
        Ok(self.pop_validated_head(expected))
    }

    fn pop_validated_head(&mut self, expected: TimerDeadlineSnapshot) -> DetachedTimerEvent {
        let event = self
            .queue
            .pop()
            .expect("a matching finite-deadline snapshot must still have an event");
        debug_assert_eq!(event.id, expected.id);
        debug_assert_eq!(event.deadline, expected.deadline);
        DetachedTimerEvent {
            request: event.request,
        }
    }

    /// Dispatch all timers due on the host clock.
    ///
    /// This preserves Servo's interactive behavior. Controlled schedulers are activated only via
    /// [`Self::activate_due_timer`] so a host event-loop batch cannot coalesce virtual timers.
    pub fn dispatch_completed_timers(&mut self) {
        if self.clock.is_controlled() {
            return;
        }
        let Ok(now) = self.clock.try_now() else {
            return;
        };
        while matches!(self.queue.peek(), Some(event) if event.deadline <= now) {
            self.queue
                .pop()
                .expect("a due timer must still be queued")
                .request
                .dispatch();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt::Debug;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;

    use serde::Serialize;
    use serde::de::DeserializeOwned;

    use super::*;

    fn assert_postcard_round_trip<T>(value: T)
    where
        T: Debug + DeserializeOwned + Eq + Serialize,
    {
        let encoded = postcard::to_stdvec(&value).unwrap();
        let decoded: T = postcard::from_bytes(&encoded).unwrap();
        assert_eq!(decoded, value);
    }

    fn controlled_clock(initial_time_ns: u128) -> DocumentClock {
        DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns,
            unix_time_origin_ns: DocumentUnixTime::default(),
        })
    }

    fn execution_limits() -> DocumentExecutionLimits {
        DocumentExecutionLimits {
            ordinary_tasks: 2,
            microtasks: 3,
            rendering_opportunities: 1,
            mutations: 1,
        }
    }

    fn execution_ledger() -> DocumentExecutionLedger {
        let clock = controlled_clock(0);
        DocumentExecutionLedger::new(clock.id(), execution_limits())
    }

    #[test]
    fn execution_ledger_starts_empty_and_round_trips_its_observation() {
        let clock = controlled_clock(0);
        let ledger = DocumentExecutionLedger::new(clock.id(), execution_limits());
        let observation = DocumentExecutionObservation {
            clock_id: clock.id(),
            limits: execution_limits(),
            counters: DocumentExecutionCounters::default(),
            terminal: None,
        };

        assert_eq!(ledger.observation(), observation);
        assert_postcard_round_trip(observation);
    }

    #[test]
    fn prework_budget_latches_first_breach_without_counting_the_rejected_unit() {
        let ledger = execution_ledger();
        assert!(ledger.begin_ordinary_task().is_ok());
        assert!(ledger.begin_ordinary_task().is_ok());

        let first = ledger.begin_ordinary_task().unwrap_err();
        assert_eq!(
            first,
            DocumentExecutionTerminal::BudgetExceeded {
                budget: DocumentExecutionBudget::OrdinaryTasks,
                limit: 2,
                observed: 3,
            }
        );
        assert_eq!(ledger.begin_microtask(), Err(first));
        assert_eq!(ledger.begin_rendering_opportunity(), Err(first));
        assert_eq!(ledger.active_guard().unwrap_err(), first);
        assert_eq!(
            ledger.observation().counters,
            DocumentExecutionCounters {
                ordinary_tasks: 2,
                ..DocumentExecutionCounters::default()
            }
        );
        assert_eq!(ledger.observation().terminal, Some(first));
    }

    #[test]
    fn each_prework_class_counts_only_admitted_units() {
        let ledger = execution_ledger();
        for _ in 0..execution_limits().microtasks {
            ledger.begin_microtask().unwrap();
        }
        assert_eq!(
            ledger.begin_microtask(),
            Err(DocumentExecutionTerminal::BudgetExceeded {
                budget: DocumentExecutionBudget::Microtasks,
                limit: 3,
                observed: 4,
            })
        );
        assert_eq!(ledger.observation().counters.microtasks, 3);

        let ledger = execution_ledger();
        ledger.begin_rendering_opportunity().unwrap();
        assert_eq!(
            ledger.begin_rendering_opportunity(),
            Err(DocumentExecutionTerminal::BudgetExceeded {
                budget: DocumentExecutionBudget::RenderingOpportunities,
                limit: 1,
                observed: 2,
            })
        );
        assert_eq!(ledger.observation().counters.rendering_opportunities, 1);
    }

    #[test]
    fn mutation_breach_counts_the_nonrejecting_over_limit_record() {
        let ledger = execution_ledger();
        ledger.record_mutation_record();
        assert_eq!(ledger.observation().terminal, None);

        ledger.record_mutation_record();
        assert_eq!(ledger.observation().counters.mutations, 2);
        assert_eq!(
            ledger.observation().terminal,
            Some(DocumentExecutionTerminal::BudgetExceeded {
                budget: DocumentExecutionBudget::MutationRecords,
                limit: 1,
                observed: 2,
            })
        );

        // The first terminal freezes every counter, including non-rejecting evidence.
        ledger.record_mutation_record();
        assert_eq!(ledger.observation().counters.mutations, 2);
    }

    fn recording_request(
        events: &Arc<Mutex<Vec<usize>>>,
        value: usize,
        duration: Duration,
    ) -> TimerEventRequest {
        let events = events.clone();
        TimerEventRequest {
            callback: Box::new(move || events.lock().unwrap().push(value)),
            duration,
        }
    }

    type RecordedEvents = Arc<Mutex<Vec<usize>>>;

    fn same_shaped_scheduler_pair(
        duration: Duration,
    ) -> (
        TimerScheduler,
        TimerDeadlineSnapshot,
        RecordedEvents,
        TimerScheduler,
        TimerDeadlineSnapshot,
        RecordedEvents,
    ) {
        let local_events = Arc::new(Mutex::new(Vec::new()));
        let foreign_events = Arc::new(Mutex::new(Vec::new()));
        let mut local = TimerScheduler::with_clock(controlled_clock(0));
        let mut foreign = TimerScheduler::with_clock(controlled_clock(0));
        local.schedule_timer(recording_request(&local_events, 1, duration));
        foreign.schedule_timer(recording_request(&foreign_events, 2, duration));
        let local_snapshot = local.finite_deadline_snapshot().unwrap().unwrap();
        let foreign_snapshot = foreign.finite_deadline_snapshot().unwrap().unwrap();
        assert_ne!(local_snapshot.scheduler_id, foreign_snapshot.scheduler_id);
        assert_eq!(local_snapshot.id, foreign_snapshot.id);
        assert_eq!(local_snapshot.deadline, foreign_snapshot.deadline);
        (
            local,
            local_snapshot,
            local_events,
            foreign,
            foreign_snapshot,
            foreign_events,
        )
    }

    fn assert_scheduler_state_unchanged(
        scheduler: &TimerScheduler,
        snapshot: TimerDeadlineSnapshot,
        now: DocumentTime,
        events: &RecordedEvents,
    ) {
        assert_eq!(scheduler.clock().now(), now);
        assert_eq!(
            scheduler.finite_deadline_snapshot().unwrap(),
            Some(snapshot)
        );
        assert!(events.lock().unwrap().is_empty());
    }

    fn assert_foreign_snapshot_rejected(
        local_now: DocumentTime,
        operation: impl FnOnce(
            &mut TimerScheduler,
            TimerDeadlineSnapshot,
        ) -> Result<(), TimerControlError>,
    ) {
        let (mut local, local_snapshot, local_events, foreign, foreign_snapshot, foreign_events) =
            same_shaped_scheduler_pair(Duration::from_nanos(10));
        if local_now != DocumentTime::ZERO {
            local.advance_controlled_time_to(local_now).unwrap();
        }

        assert_eq!(
            operation(&mut local, foreign_snapshot),
            Err(TimerControlError::StaleDeadline {
                expected: foreign_snapshot,
                observed: Some(local_snapshot),
            })
        );
        assert_scheduler_state_unchanged(&local, local_snapshot, local_now, &local_events);
        assert_scheduler_state_unchanged(
            &foreign,
            foreign_snapshot,
            DocumentTime::ZERO,
            &foreign_events,
        );
    }

    #[test]
    fn producer_fence_requires_two_fresh_unchanged_empty_checkpoints() {
        let fence = DocumentProducerFence::default();
        let mut observer = DocumentProducerObserver::default();
        let first = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let second = first.checked_next().unwrap();

        assert_eq!(
            observer.observe(&fence, DocumentProducerCheckpoint::ZERO),
            Err(DocumentProducerFenceError::CheckpointNotCompleted)
        );
        assert!(matches!(
            observer.observe(&fence, first),
            Ok(DocumentProducerObservation::FirstEmpty(snapshot)) if snapshot.is_empty()
        ));
        assert_eq!(
            observer.observe(&fence, first),
            Err(DocumentProducerFenceError::StaleCheckpoint {
                previous: first,
                observed: first,
            })
        );
        assert!(matches!(
            observer.observe(&fence, second),
            Ok(DocumentProducerObservation::StableEmpty(snapshot)) if snapshot.is_empty()
        ));
    }

    #[test]
    fn observer_switching_fences_requires_a_new_first_empty_observation() {
        let first_fence = DocumentProducerFence::default();
        let second_fence = DocumentProducerFence::default();
        let mut observer = DocumentProducerObserver::default();
        let first = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let second = first.checked_next().unwrap();
        let third = second.checked_next().unwrap();

        assert!(matches!(
            observer.observe(&first_fence, first),
            Ok(DocumentProducerObservation::FirstEmpty(snapshot))
                if snapshot.fence_id() == first_fence.id()
        ));
        assert!(matches!(
            observer.observe(&second_fence, second),
            Ok(DocumentProducerObservation::FirstEmpty(snapshot))
                if snapshot.fence_id() == second_fence.id()
        ));
        assert!(matches!(
            observer.observe(&second_fence, third),
            Ok(DocumentProducerObservation::StableEmpty(snapshot))
                if snapshot.fence_id() == second_fence.id()
        ));
    }

    #[test]
    fn producer_activity_between_empty_checkpoints_restarts_qualification() {
        let fence = DocumentProducerFence::default();
        let mut observer = DocumentProducerObserver::default();
        let first = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let second = first.checked_next().unwrap();
        let third = second.checked_next().unwrap();

        assert!(matches!(
            observer.observe(&fence, first),
            Ok(DocumentProducerObservation::FirstEmpty(_))
        ));
        let guard = fence.begin(DocumentProducerKind::Resource).unwrap();
        drop(guard);
        assert!(matches!(
            observer.observe(&fence, second),
            Ok(DocumentProducerObservation::FirstEmpty(snapshot))
                if snapshot.revision() == 2 && snapshot.enqueued() == 1 && snapshot.completed() == 1
        ));
        assert!(matches!(
            observer.observe(&fence, third),
            Ok(DocumentProducerObservation::StableEmpty(_))
        ));
    }

    #[test]
    fn busy_observation_clears_the_previous_empty_candidate() {
        let fence = DocumentProducerFence::default();
        let mut observer = DocumentProducerObserver::default();
        let first = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let second = first.checked_next().unwrap();
        let third = second.checked_next().unwrap();
        let fourth = third.checked_next().unwrap();

        assert!(matches!(
            observer.observe(&fence, first),
            Ok(DocumentProducerObservation::FirstEmpty(_))
        ));
        let guard = fence.begin(DocumentProducerKind::Font).unwrap();
        assert!(matches!(
            observer.observe(&fence, second),
            Ok(DocumentProducerObservation::Busy(snapshot))
                if snapshot.for_kind(DocumentProducerKind::Font).pending() == 1
        ));
        drop(guard);
        assert!(matches!(
            observer.observe(&fence, third),
            Ok(DocumentProducerObservation::FirstEmpty(_))
        ));
        assert!(matches!(
            observer.observe(&fence, fourth),
            Ok(DocumentProducerObservation::StableEmpty(_))
        ));
    }

    #[test]
    fn producer_watermarks_are_stable_when_tickets_complete_out_of_order() {
        let fence = DocumentProducerFence::default();
        let first = fence.begin(DocumentProducerKind::Task).unwrap();
        let second = fence.begin(DocumentProducerKind::Task).unwrap();
        let image = fence.begin(DocumentProducerKind::Image).unwrap();

        assert_eq!(first.sequence().get(), 1);
        assert_eq!(second.sequence().get(), 2);
        assert_eq!(image.sequence().get(), 3);
        assert_eq!(fence.snapshot().pending(), 3);

        drop(second);
        let middle = fence.snapshot();
        assert_eq!(middle.revision(), 4);
        assert_eq!(middle.pending(), 2);
        assert_eq!(
            middle.for_kind(DocumentProducerKind::Task),
            DocumentProducerWatermark {
                enqueued: 2,
                completed: 1,
                pending: 1,
            }
        );

        drop(first);
        drop(image);
        let complete = fence.snapshot();
        assert!(complete.is_empty());
        assert_eq!(complete.revision(), 6);
        assert_eq!(complete.enqueued(), complete.completed());
        for kind in [
            DocumentProducerKind::Task,
            DocumentProducerKind::Resource,
            DocumentProducerKind::Font,
            DocumentProducerKind::Image,
            DocumentProducerKind::ExternalCallback,
        ] {
            let watermark = complete.for_kind(kind);
            assert_eq!(watermark.enqueued(), watermark.completed());
            assert_eq!(watermark.pending(), 0);
        }
    }

    #[test]
    fn external_callback_handoff_never_exposes_false_empty_state() {
        let fence = DocumentProducerFence::default();

        let failed_callback = fence.begin(DocumentProducerKind::ExternalCallback).unwrap();
        assert_eq!(fence.snapshot().pending(), 1);
        drop(failed_callback);
        assert!(fence.snapshot().is_empty());

        let callback = fence.begin(DocumentProducerKind::ExternalCallback).unwrap();
        let task = fence.begin(DocumentProducerKind::Task).unwrap();
        let during_handoff = fence.snapshot();
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
        assert_eq!(fence.snapshot().pending(), 1);
        drop(task);
        assert!(fence.snapshot().is_empty());
    }

    #[test]
    fn explicit_leases_reject_double_and_foreign_completion() {
        let first_fence = DocumentProducerFence::default();
        let second_fence = DocumentProducerFence::default();
        let first_lease = first_fence
            .begin(DocumentProducerKind::Resource)
            .unwrap()
            .into_lease_id();
        let second_lease = second_fence
            .begin(DocumentProducerKind::Resource)
            .unwrap()
            .into_lease_id();

        assert_eq!(first_lease.sequence(), second_lease.sequence());
        assert_eq!(first_lease.kind(), DocumentProducerKind::Resource);
        assert_eq!(
            first_fence.complete_lease(second_lease),
            Err(DocumentProducerFenceError::UnknownLease(second_lease))
        );
        assert_eq!(first_fence.snapshot().pending(), 1);
        assert_eq!(second_fence.snapshot().pending(), 1);

        first_fence.complete_lease(first_lease).unwrap();
        assert_eq!(
            first_fence.complete_lease(first_lease),
            Err(DocumentProducerFenceError::UnknownLease(first_lease))
        );
        second_fence.complete_lease(second_lease).unwrap();
        assert!(first_fence.snapshot().is_empty());
        assert!(second_fence.snapshot().is_empty());
    }

    #[test]
    fn abandoned_guard_completes_its_lease_and_latches_the_exact_terminal() {
        let fence = DocumentProducerFence::default();
        let guard = fence.begin(DocumentProducerKind::Resource).unwrap();
        let lease_id = DocumentProducerLeaseId {
            fence_id: fence.id(),
            sequence: guard.sequence(),
            kind: DocumentProducerKind::Resource,
        };

        guard.abandon().unwrap();

        let snapshot = fence.snapshot();
        assert_eq!(
            snapshot.terminal_error(),
            Some(DocumentProducerFenceError::ProducerAbandoned(lease_id))
        );
        assert_eq!(snapshot.revision(), 2);
        assert_eq!(snapshot.enqueued(), 1);
        assert_eq!(snapshot.completed(), 1);
        assert_eq!(snapshot.pending(), 0);
        assert_eq!(
            snapshot.for_kind(DocumentProducerKind::Resource),
            DocumentProducerWatermark {
                enqueued: 1,
                completed: 1,
                pending: 0,
            }
        );
    }

    #[test]
    fn abandonment_does_not_replace_an_earlier_producer_terminal() {
        let fence = DocumentProducerFence::default();
        let guard = fence.begin(DocumentProducerKind::Resource).unwrap();
        fence.inner.lock().unwrap().terminal_error =
            Some(DocumentProducerFenceError::CounterOverflow);

        guard.abandon().unwrap();

        let snapshot = fence.snapshot();
        assert_eq!(
            snapshot.terminal_error(),
            Some(DocumentProducerFenceError::CounterOverflow)
        );
        assert!(snapshot.is_empty());
        assert_eq!(snapshot.enqueued(), snapshot.completed());
    }

    #[test]
    fn admission_limit_terminal_invalidates_snapshot_without_changing_conserved_watermarks() {
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed_notifications = notifications.clone();
        let fence = DocumentProducerFence::with_notifier(Some(Arc::new(move || {
            observed_notifications.fetch_add(1, Ordering::SeqCst);
        })));
        let before = fence.snapshot();

        let error = fence.latch_admission_limit_exceeded(DocumentProducerKind::Image, 512, 513);

        assert_eq!(
            error,
            DocumentProducerFenceError::AdmissionLimitExceeded {
                kind: DocumentProducerKind::Image,
                limit: 512,
                observed: 513,
            }
        );
        let terminal = fence.snapshot();
        assert_eq!(terminal.terminal_error(), Some(error));
        assert_eq!(terminal.revision(), before.revision());
        assert_eq!(terminal.enqueued(), before.enqueued());
        assert_eq!(terminal.completed(), before.completed());
        assert_eq!(terminal.pending(), before.pending());
        assert_eq!(
            terminal.for_kind(DocumentProducerKind::Image),
            before.for_kind(DocumentProducerKind::Image)
        );
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert!(matches!(
            fence.with_matching_snapshot(before, || ()),
            Err(mismatch) if mismatch.observed() == terminal
        ));

        assert_eq!(
            fence.latch_admission_limit_exceeded(DocumentProducerKind::Task, 7, 8),
            error
        );
        assert_eq!(fence.snapshot(), terminal);
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn enqueue_reserves_capacity_for_infallible_guard_completion() {
        let state = DocumentProducerFenceState {
            revision: u64::MAX - 2,
            ..DocumentProducerFenceState::default()
        };
        let fence = DocumentProducerFence {
            fence_id: DocumentProducerFenceId(0),
            inner: Arc::new(Mutex::new(state)),
            state_change_notifier: None,
        };

        let guard = fence.begin(DocumentProducerKind::Task).unwrap();
        assert_eq!(fence.snapshot().revision(), u64::MAX - 1);
        drop(guard);
        assert_eq!(fence.snapshot().revision(), u64::MAX);

        let before = fence.snapshot();
        assert!(matches!(
            fence.begin(DocumentProducerKind::Task),
            Err(DocumentProducerFenceError::CounterOverflow)
        ));
        let terminal = fence.snapshot();
        assert_eq!(
            terminal.terminal_error(),
            Some(DocumentProducerFenceError::CounterOverflow)
        );
        assert_eq!(terminal.revision(), before.revision());
        assert_eq!(terminal.enqueued(), before.enqueued());
        assert_eq!(terminal.completed(), before.completed());
        assert_eq!(terminal.pending(), before.pending());
        assert_eq!(
            DocumentProducerCheckpoint(u64::MAX).checked_next(),
            Err(DocumentProducerFenceError::CounterOverflow)
        );
    }

    #[test]
    fn producer_overflow_latches_once_without_mutating_watermarks_and_notifies() {
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed_notifications = notifications.clone();
        let state = DocumentProducerFenceState {
            revision: u64::MAX - 1,
            ..DocumentProducerFenceState::default()
        };
        let fence = DocumentProducerFence {
            fence_id: DocumentProducerFenceId(0),
            inner: Arc::new(Mutex::new(state)),
            state_change_notifier: Some(Arc::new(move || {
                observed_notifications.fetch_add(1, Ordering::SeqCst);
            })),
        };
        let before = fence.snapshot();

        assert_eq!(
            fence.begin(DocumentProducerKind::Task).err(),
            Some(DocumentProducerFenceError::CounterOverflow)
        );
        let terminal = fence.snapshot();
        assert_eq!(
            terminal.terminal_error(),
            Some(DocumentProducerFenceError::CounterOverflow)
        );
        assert_eq!(terminal.revision(), before.revision());
        assert_eq!(terminal.enqueued(), before.enqueued());
        assert_eq!(terminal.completed(), before.completed());
        assert_eq!(terminal.pending(), before.pending());
        assert_eq!(notifications.load(Ordering::SeqCst), 1);

        assert_eq!(
            fence.begin(DocumentProducerKind::Resource).err(),
            Some(DocumentProducerFenceError::CounterOverflow)
        );
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn producer_notifier_runs_after_unlock_for_each_committed_change() {
        let holder = Arc::new(Mutex::new(None::<DocumentProducerFence>));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let callback_holder = holder.clone();
        let callback_observed = observed.clone();
        let fence = DocumentProducerFence::with_notifier(Some(Arc::new(move || {
            let fence = callback_holder
                .lock()
                .unwrap()
                .as_ref()
                .expect("fence installed before its first mutation")
                .clone();
            callback_observed
                .lock()
                .unwrap()
                .push(fence.snapshot().pending());
        })));
        *holder.lock().unwrap() = Some(fence.clone());

        let guard = fence.begin(DocumentProducerKind::Task).unwrap();
        drop(guard);

        assert_eq!(*observed.lock().unwrap(), vec![1, 0]);
        assert!(fence.snapshot().is_empty());
    }

    #[test]
    fn observer_notification_after_external_commit_does_not_mutate_producer_state() {
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed_notifications = notifications.clone();
        let fence = DocumentProducerFence::with_notifier(Some(Arc::new(move || {
            observed_notifications.fetch_add(1, Ordering::SeqCst);
        })));
        let before = fence.snapshot();

        fence.notify_observer_after_commit();

        assert_eq!(fence.snapshot(), before);
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn panicking_notifier_does_not_interrupt_begin_or_leak_its_guard() {
        let fence = DocumentProducerFence::with_notifier(Some(Arc::new(|| {
            panic!("host notifier failed");
        })));

        let guard = fence
            .begin(DocumentProducerKind::ExternalCallback)
            .expect("notifier panic must not replace a successful admission");
        assert_eq!(fence.snapshot().pending(), 1);

        drop(guard);
        assert!(fence.snapshot().is_empty());
    }

    #[test]
    fn panicking_notifier_never_escapes_raii_completion() {
        let fence = DocumentProducerFence::with_notifier(Some(Arc::new(|| {
            panic!("host notifier failed");
        })));
        let guard = fence.begin(DocumentProducerKind::Task).unwrap();

        let completion = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(guard)));

        assert!(completion.is_ok());
        assert!(fence.snapshot().is_empty());
    }

    #[test]
    fn rejected_lease_completion_does_not_notify() {
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed_notifications = notifications.clone();
        let fence = DocumentProducerFence::with_notifier(Some(Arc::new(move || {
            observed_notifications.fetch_add(1, Ordering::SeqCst);
        })));
        let foreign_fence = DocumentProducerFence::default();
        let foreign_lease = foreign_fence
            .begin(DocumentProducerKind::Font)
            .unwrap()
            .into_lease_id();

        assert_eq!(
            fence.complete_lease(foreign_lease),
            Err(DocumentProducerFenceError::UnknownLease(foreign_lease))
        );
        assert_eq!(notifications.load(Ordering::SeqCst), 0);
        foreign_fence.complete_lease(foreign_lease).unwrap();
    }

    #[test]
    fn matching_snapshot_reports_exact_observed_state_on_mismatch() {
        let fence = DocumentProducerFence::default();
        let expected = fence.snapshot();
        let guard = fence.begin(DocumentProducerKind::Resource).unwrap();
        let observed = fence.snapshot();

        let mismatch = fence
            .with_matching_snapshot(expected, || panic!("stale action must not run"))
            .unwrap_err();
        assert_eq!(mismatch.observed(), observed);
        drop(guard);
    }

    #[test]
    fn matching_snapshot_rejects_an_identical_snapshot_from_another_fence() {
        let first_fence = DocumentProducerFence::default();
        let second_fence = DocumentProducerFence::default();
        let foreign = first_fence.snapshot();
        let local = second_fence.snapshot();

        assert_ne!(foreign.fence_id(), local.fence_id());
        let mismatch = second_fence
            .with_matching_snapshot(foreign, || panic!("foreign snapshot action must not run"))
            .unwrap_err();
        assert_eq!(mismatch.observed(), local);
    }

    #[test]
    fn panicking_matching_snapshot_action_releases_lock_without_poisoning() {
        let fence = DocumentProducerFence::default();
        let expected = fence.snapshot();

        let action = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fence
                .with_matching_snapshot(expected, || panic!("guarded action failed"))
                .unwrap();
        }));
        assert!(action.is_err());

        let guard = fence
            .begin(DocumentProducerKind::Resource)
            .expect("guarded action panic must not poison the fence");
        drop(guard);
        assert!(fence.snapshot().is_empty());
    }

    #[test]
    fn matching_snapshot_lock_excludes_new_producers_until_action_finishes() {
        let fence = DocumentProducerFence::default();
        let expected = fence.snapshot();
        let action_fence = fence.clone();
        let producer_fence = fence.clone();
        let (locked_sender, locked_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let (attempt_sender, attempt_receiver) = mpsc::channel();
        let (producer_sender, producer_receiver) = mpsc::channel();

        let action = thread::spawn(move || {
            action_fence
                .with_matching_snapshot(expected, || {
                    locked_sender.send(()).unwrap();
                    release_receiver.recv().unwrap();
                })
                .unwrap();
        });
        locked_receiver.recv().unwrap();
        let producer = thread::spawn(move || {
            attempt_sender.send(()).unwrap();
            let guard = producer_fence
                .begin(DocumentProducerKind::Resource)
                .unwrap();
            producer_sender.send(()).unwrap();
            drop(guard);
        });

        attempt_receiver.recv().unwrap();
        assert!(
            producer_receiver
                .recv_timeout(Duration::from_millis(50))
                .is_err()
        );
        release_sender.send(()).unwrap();
        action.join().unwrap();
        producer_receiver.recv().unwrap();
        producer.join().unwrap();
        assert_eq!(fence.snapshot().revision(), 2);
    }

    #[test]
    fn controlled_deadlines_activate_one_at_a_time_in_stable_creation_order() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut scheduler = TimerScheduler::with_clock(controlled_clock(0));

        scheduler.schedule_timer(recording_request(&events, 1, Duration::from_nanos(10)));
        scheduler.schedule_timer(recording_request(&events, 2, Duration::from_nanos(10)));
        scheduler.schedule_timer(recording_request(&events, 0, Duration::from_nanos(5)));

        let first = scheduler.finite_deadline_snapshot().unwrap().unwrap();
        assert_eq!(first.deadline, DocumentTime::from_nanos(5));
        assert!(matches!(
            scheduler.activate_due_timer(first),
            Err(TimerControlError::TimerNotDue { .. })
        ));

        scheduler.advance_to_and_activate(first).unwrap();
        assert_eq!(*events.lock().unwrap(), vec![0]);

        let second = scheduler.finite_deadline_snapshot().unwrap().unwrap();
        assert_eq!(second.id.sequence(), 0);
        scheduler.advance_to_and_activate(second).unwrap();
        let third = scheduler.finite_deadline_snapshot().unwrap().unwrap();
        assert_eq!(third.id.sequence(), 1);
        scheduler.activate_due_timer(third).unwrap();
        assert_eq!(*events.lock().unwrap(), vec![0, 1, 2]);
        assert_eq!(scheduler.finite_deadline_snapshot().unwrap(), None);
    }

    #[test]
    fn scheduler_identity_is_stable_unique_and_bound_to_its_snapshots() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut scheduler = TimerScheduler::with_clock(controlled_clock(0));
        let other = TimerScheduler::with_clock(controlled_clock(0));
        let id = scheduler.id();

        assert_ne!(id.get(), 0);
        assert_eq!(scheduler.id(), id);
        assert_ne!(other.id(), id);
        scheduler.schedule_timer(recording_request(&events, 1, Duration::from_nanos(1)));
        assert_eq!(
            scheduler
                .finite_deadline_snapshot()
                .unwrap()
                .unwrap()
                .scheduler_id,
            id
        );
        assert_eq!(scheduler.id(), id);
    }

    #[test]
    fn bulk_deadline_join_is_aligned_live_and_scheduler_scoped() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut scheduler = TimerScheduler::with_clock(controlled_clock(5));
        let later =
            scheduler.schedule_timer(recording_request(&events, 1, Duration::from_nanos(30)));
        let canceled =
            scheduler.schedule_timer(recording_request(&events, 2, Duration::from_nanos(10)));
        let earlier =
            scheduler.schedule_timer(recording_request(&events, 3, Duration::from_nanos(20)));
        scheduler.cancel_timer(canceled);

        assert_eq!(
            scheduler.join_live_deadlines(scheduler.id(), &[]),
            Ok(Vec::new())
        );
        assert_eq!(
            scheduler.join_live_deadlines(scheduler.id(), &[earlier, canceled, later, earlier],),
            Ok(vec![
                TimerDeadlineJoin {
                    scheduler_id: scheduler.id(),
                    id: earlier,
                    deadline: Some(DocumentTime::from_nanos(25)),
                },
                TimerDeadlineJoin {
                    scheduler_id: scheduler.id(),
                    id: canceled,
                    deadline: None,
                },
                TimerDeadlineJoin {
                    scheduler_id: scheduler.id(),
                    id: later,
                    deadline: Some(DocumentTime::from_nanos(35)),
                },
                TimerDeadlineJoin {
                    scheduler_id: scheduler.id(),
                    id: earlier,
                    deadline: Some(DocumentTime::from_nanos(25)),
                },
            ])
        );
        assert!(events.lock().unwrap().is_empty());

        let foreign_events = Arc::new(Mutex::new(Vec::new()));
        let mut foreign = TimerScheduler::with_clock(controlled_clock(5));
        let colliding_foreign_id = foreign.schedule_timer(recording_request(
            &foreign_events,
            4,
            Duration::from_nanos(1),
        ));
        assert_eq!(colliding_foreign_id, later);
        assert_eq!(
            scheduler.join_live_deadlines(foreign.id(), &[colliding_foreign_id]),
            Err(TimerDeadlineJoinError::SchedulerMismatch {
                expected: scheduler.id(),
                observed: foreign.id(),
            })
        );
        assert!(events.lock().unwrap().is_empty());
        assert!(foreign_events.lock().unwrap().is_empty());

        let realtime = TimerScheduler::default();
        assert_eq!(
            realtime.join_live_deadlines(realtime.id(), &[later]),
            Err(TimerDeadlineJoinError::RealtimeScheduler)
        );
    }

    #[test]
    fn foreign_scheduler_snapshots_cannot_validate_advance_or_activate() {
        assert_foreign_snapshot_rejected(DocumentTime::ZERO, |scheduler, snapshot| {
            scheduler.validate_deadline_snapshot(snapshot)
        });
        assert_foreign_snapshot_rejected(DocumentTime::ZERO, |scheduler, snapshot| {
            scheduler.advance_to_and_activate(snapshot)
        });
        assert_foreign_snapshot_rejected(DocumentTime::ZERO, |scheduler, snapshot| {
            scheduler.validate_and_advance_to(snapshot)
        });
        assert_foreign_snapshot_rejected(DocumentTime::ZERO, |scheduler, snapshot| {
            scheduler.validate_and_advance_from(DocumentTime::ZERO, snapshot)
        });
        assert_foreign_snapshot_rejected(
            DocumentTime::from_nanos(10),
            TimerScheduler::activate_due_timer,
        );
    }

    #[test]
    fn detach_rejects_foreign_and_stale_snapshots_without_time_or_callback_mutation() {
        let (mut local, local_snapshot, local_events, foreign, foreign_snapshot, foreign_events) =
            same_shaped_scheduler_pair(Duration::from_nanos(10));

        assert!(matches!(
            local.validate_advance_and_detach(DocumentTime::ZERO, foreign_snapshot),
            Err(TimerControlError::StaleDeadline {
                expected,
                observed: Some(observed),
            }) if expected == foreign_snapshot && observed == local_snapshot
        ));
        assert_scheduler_state_unchanged(&local, local_snapshot, DocumentTime::ZERO, &local_events);
        assert_scheduler_state_unchanged(
            &foreign,
            foreign_snapshot,
            DocumentTime::ZERO,
            &foreign_events,
        );

        local.cancel_timer(local_snapshot.id);
        assert!(matches!(
            local.validate_advance_and_detach(DocumentTime::ZERO, local_snapshot),
            Err(TimerControlError::StaleDeadline {
                expected,
                observed: None,
            }) if expected == local_snapshot
        ));
        assert_eq!(local.clock().now(), DocumentTime::ZERO);
        assert_eq!(local.finite_deadline_snapshot().unwrap(), None);
        assert!(local_events.lock().unwrap().is_empty());
    }

    #[test]
    fn detached_callback_runs_only_after_explicit_dispatch_and_exact_head_is_removed() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut scheduler = TimerScheduler::with_clock(controlled_clock(0));
        scheduler.schedule_timer(recording_request(&events, 1, Duration::from_nanos(10)));
        scheduler.schedule_timer(recording_request(&events, 2, Duration::from_nanos(10)));
        scheduler.schedule_timer(recording_request(&events, 3, Duration::from_nanos(20)));
        let selected = scheduler.finite_deadline_snapshot().unwrap().unwrap();

        let detached = scheduler
            .validate_advance_and_detach(DocumentTime::ZERO, selected)
            .unwrap();

        assert_eq!(scheduler.clock().now(), selected.deadline);
        assert!(events.lock().unwrap().is_empty());
        let next = scheduler.finite_deadline_snapshot().unwrap().unwrap();
        assert_eq!(next.deadline, selected.deadline);
        assert_ne!(next.id, selected.id);

        detached.dispatch();
        assert_eq!(*events.lock().unwrap(), vec![1]);
        assert_eq!(scheduler.finite_deadline_snapshot().unwrap(), Some(next));
    }

    #[test]
    fn detached_event_can_leave_producer_exclusion_before_callback_dispatch() {
        let fence = DocumentProducerFence::default();
        let producer_snapshot = fence.snapshot();
        let callback_fence = fence.clone();
        let events = Arc::new(Mutex::new(Vec::new()));
        let callback_events = events.clone();
        let mut scheduler = TimerScheduler::with_clock(controlled_clock(0));
        scheduler.schedule_timer(TimerEventRequest {
            callback: Box::new(move || {
                let guard = callback_fence.begin(DocumentProducerKind::Task).unwrap();
                callback_events.lock().unwrap().push(1);
                drop(guard);
            }),
            duration: Duration::from_nanos(10),
        });
        let selected = scheduler.finite_deadline_snapshot().unwrap().unwrap();

        let detached = fence
            .with_matching_snapshot(producer_snapshot, || {
                scheduler.validate_advance_and_detach(DocumentTime::ZERO, selected)
            })
            .unwrap()
            .unwrap();

        assert_eq!(fence.snapshot(), producer_snapshot);
        assert!(events.lock().unwrap().is_empty());
        detached.dispatch();
        assert_eq!(*events.lock().unwrap(), vec![1]);
        assert_eq!(fence.snapshot().revision(), 2);
    }

    #[test]
    fn detach_rejects_clock_drift_and_wall_overflow_without_consuming_event() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let clock = controlled_clock(0);
        let mut scheduler = TimerScheduler::with_clock(clock.clone());
        scheduler.schedule_timer(recording_request(&events, 1, Duration::from_nanos(10)));
        let snapshot = scheduler.finite_deadline_snapshot().unwrap().unwrap();
        clock.advance_to(DocumentTime::from_nanos(1)).unwrap();

        assert!(matches!(
            scheduler.validate_advance_and_detach(DocumentTime::ZERO, snapshot),
            Err(TimerControlError::Clock(DocumentClockError::TimeChanged {
                expected: DocumentTime::ZERO,
                observed,
            })) if observed == DocumentTime::from_nanos(1)
        ));
        assert_scheduler_state_unchanged(
            &scheduler,
            snapshot,
            DocumentTime::from_nanos(1),
            &events,
        );

        let last_representable_wall_offset = u128::try_from(i128::MAX).unwrap();
        let overflow_events = Arc::new(Mutex::new(Vec::new()));
        let mut overflow_scheduler =
            TimerScheduler::with_clock(controlled_clock(last_representable_wall_offset));
        overflow_scheduler.schedule_timer(recording_request(
            &overflow_events,
            2,
            Duration::from_nanos(1),
        ));
        let overflow_snapshot = overflow_scheduler
            .finite_deadline_snapshot()
            .unwrap()
            .unwrap();

        assert!(matches!(
            overflow_scheduler.validate_advance_and_detach(
                DocumentTime::from_nanos(last_representable_wall_offset),
                overflow_snapshot,
            ),
            Err(TimerControlError::Clock(DocumentClockError::Overflow))
        ));
        assert_scheduler_state_unchanged(
            &overflow_scheduler,
            overflow_snapshot,
            DocumentTime::from_nanos(last_representable_wall_offset),
            &overflow_events,
        );
    }

    #[test]
    fn cancellation_invalidates_snapshot_without_moving_time_or_running_callback() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut scheduler = TimerScheduler::with_clock(controlled_clock(3));
        let id = scheduler.schedule_timer(recording_request(&events, 1, Duration::from_nanos(7)));
        let snapshot = scheduler.finite_deadline_snapshot().unwrap().unwrap();

        scheduler.cancel_timer(id);
        assert!(matches!(
            scheduler.advance_to_and_activate(snapshot),
            Err(TimerControlError::StaleDeadline { observed: None, .. })
        ));
        assert_eq!(scheduler.clock().now(), DocumentTime::from_nanos(3));
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn replacement_deadline_rejects_stale_identity_even_when_old_timer_was_due() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut scheduler = TimerScheduler::with_clock(controlled_clock(0));
        let old_id =
            scheduler.schedule_timer(recording_request(&events, 1, Duration::from_nanos(10)));
        let stale = scheduler.finite_deadline_snapshot().unwrap().unwrap();

        scheduler.cancel_timer(old_id);
        scheduler.schedule_timer(recording_request(&events, 2, Duration::from_nanos(5)));
        scheduler
            .advance_controlled_time_to(DocumentTime::from_nanos(10))
            .unwrap();
        let replacement = scheduler.finite_deadline_snapshot().unwrap().unwrap();

        assert!(matches!(
            scheduler.activate_due_timer(stale),
            Err(TimerControlError::StaleDeadline {
                observed: Some(observed),
                ..
            }) if observed == replacement
        ));
        assert!(events.lock().unwrap().is_empty());
        scheduler.activate_due_timer(replacement).unwrap();
        assert_eq!(*events.lock().unwrap(), vec![2]);
    }

    #[test]
    fn conditional_advance_rejects_clock_drift_without_consuming_timer() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let clock = controlled_clock(0);
        let mut scheduler = TimerScheduler::with_clock(clock.clone());
        scheduler.schedule_timer(recording_request(&events, 1, Duration::from_nanos(10)));
        let snapshot = scheduler.finite_deadline_snapshot().unwrap().unwrap();
        clock.advance_to(DocumentTime::from_nanos(1)).unwrap();

        assert_eq!(
            scheduler.validate_and_advance_from(DocumentTime::ZERO, snapshot),
            Err(TimerControlError::Clock(DocumentClockError::TimeChanged {
                expected: DocumentTime::ZERO,
                observed: DocumentTime::from_nanos(1),
            }))
        );
        assert_eq!(clock.now(), DocumentTime::from_nanos(1));
        assert_eq!(
            scheduler.finite_deadline_snapshot().unwrap(),
            Some(snapshot)
        );
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn checked_wall_deadline_and_sequence_boundaries_do_not_run_callbacks() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let last_representable_wall_offset = u128::try_from(i128::MAX).unwrap();
        let mut scheduler =
            TimerScheduler::with_clock(controlled_clock(last_representable_wall_offset));
        scheduler
            .try_schedule_timer(recording_request(&events, 1, Duration::from_nanos(1)))
            .unwrap();
        let out_of_wall_range = scheduler.finite_deadline_snapshot().unwrap().unwrap();
        assert_eq!(
            scheduler.advance_to_and_activate(out_of_wall_range),
            Err(TimerControlError::Clock(DocumentClockError::Overflow))
        );
        assert_eq!(
            scheduler.clock().now(),
            DocumentTime::from_nanos(last_representable_wall_offset)
        );
        assert_eq!(
            scheduler.finite_deadline_snapshot().unwrap(),
            Some(out_of_wall_range)
        );
        assert!(events.lock().unwrap().is_empty());

        let mut scheduler = TimerScheduler::with_clock(controlled_clock(0));
        scheduler.next_id = u64::MAX;
        assert_eq!(
            scheduler.try_schedule_timer(recording_request(&events, 2, Duration::ZERO)),
            Err(TimerControlError::SequenceExhausted)
        );
        assert_eq!(scheduler.finite_deadline_snapshot().unwrap(), None);
    }

    #[test]
    fn widened_time_rejects_backwards_motion_and_duration_overflow_exactly() {
        let beyond_duration = DocumentTime::from_nanos(Duration::MAX.as_nanos() + 1);
        assert_eq!(
            beyond_duration.checked_to_duration(),
            Err(DocumentClockError::Overflow)
        );

        let beyond_u64 = u128::from(u64::MAX) + 1;
        let clock = controlled_clock(beyond_u64);
        assert_eq!(
            clock.advance_to(DocumentTime::from_nanos(beyond_u64 - 1)),
            Err(DocumentClockError::TimeMovedBackwards {
                current: DocumentTime::from_nanos(beyond_u64),
                requested: DocumentTime::from_nanos(beyond_u64 - 1),
            })
        );
        assert_eq!(clock.now(), DocumentTime::from_nanos(beyond_u64));
        assert_eq!(
            DocumentTime::from_nanos(u128::MAX).checked_add(Duration::from_nanos(1)),
            Err(DocumentClockError::Overflow)
        );
    }

    #[test]
    fn controlled_scheduler_never_uses_host_wake_or_realtime_dispatch_path() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut scheduler = TimerScheduler::with_clock(controlled_clock(0));
        scheduler.schedule_timer(recording_request(&events, 1, Duration::ZERO));

        assert_eq!(scheduler.next_deadline(), None);
        scheduler.dispatch_completed_timers();
        assert!(events.lock().unwrap().is_empty());

        let snapshot = scheduler.finite_deadline_snapshot().unwrap().unwrap();
        scheduler.activate_due_timer(snapshot).unwrap();
        assert_eq!(*events.lock().unwrap(), vec![1]);
    }

    #[test]
    fn realtime_scheduler_keeps_existing_host_deadline_and_dispatch_path() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut scheduler = TimerScheduler::default();
        scheduler.schedule_timer(recording_request(&events, 1, Duration::ZERO));

        assert!(scheduler.next_deadline().is_some());
        assert_eq!(
            scheduler.finite_deadline_snapshot(),
            Err(TimerControlError::RealtimeScheduler)
        );
        scheduler.dispatch_completed_timers();
        assert_eq!(*events.lock().unwrap(), vec![1]);
    }

    #[test]
    fn realtime_deadline_overflow_is_rejected_before_queue_mutation() {
        let mut scheduler = TimerScheduler::default();
        let recorded = Arc::new(Mutex::new(Vec::new()));

        assert_eq!(
            scheduler.try_schedule_timer(recording_request(&recorded, 1, Duration::MAX,)),
            Err(TimerControlError::DeadlineOverflow),
        );
        assert!(scheduler.next_deadline().is_none());
        assert!(recorded.lock().unwrap().is_empty());
    }

    #[test]
    fn controlled_wall_and_monotonic_time_advance_together_exactly() {
        let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 7_000_000,
            unix_time_origin_ns: DocumentUnixTime::from_nanos(1_700_000_000_000_000_000),
        });
        let initial = clock.now();
        assert_eq!(
            clock.unix_time_ns(),
            Ok(DocumentUnixTime::from_nanos(1_700_000_000_007_000_000))
        );

        clock
            .advance_to(DocumentTime::from_nanos(12_000_000))
            .unwrap();
        assert_eq!(
            clock.now().checked_duration_since(initial),
            Ok(Duration::from_millis(5))
        );
        assert_eq!(
            clock.unix_time_ns(),
            Ok(DocumentUnixTime::from_nanos(1_700_000_000_012_000_000))
        );
    }

    #[test]
    fn signed_unix_origin_preserves_pre_epoch_wall_time() {
        let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 500,
            unix_time_origin_ns: DocumentUnixTime::from_nanos(-1_000),
        });
        assert_eq!(clock.unix_time_ns(), Ok(DocumentUnixTime::from_nanos(-500)));
        clock.advance_to(DocumentTime::from_nanos(1_500)).unwrap();
        assert_eq!(clock.unix_time_ns(), Ok(DocumentUnixTime::from_nanos(500)));
    }

    #[test]
    fn javascript_date_timeclip_boundaries_are_exact_or_nan() {
        const LIMIT_NANOSECONDS: i128 = TIME_CLIP_LIMIT_MILLISECONDS * NANOSECONDS_PER_MILLISECOND;

        for nanoseconds in [
            -LIMIT_NANOSECONDS,
            -LIMIT_NANOSECONDS + 1,
            -1_000_001,
            -999_999,
            -1,
            0,
            1,
            999_999,
            1_000_001,
            LIMIT_NANOSECONDS - 1,
            LIMIT_NANOSECONDS,
        ] {
            let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
                initial_time_ns: 0,
                unix_time_origin_ns: DocumentUnixTime::from_nanos(nanoseconds),
            });
            let candidate = clock.javascript_date_time_microseconds().unwrap();
            let clipped = simulated_javascript_date_time_clip(candidate);
            assert_eq!(
                clipped as i128,
                nanoseconds / NANOSECONDS_PER_MILLISECOND,
                "exact TimeClip mismatch for {nanoseconds}ns",
            );
            assert_eq!(clock.terminal_error(), None);
        }

        for nanoseconds in [-LIMIT_NANOSECONDS - 1, LIMIT_NANOSECONDS + 1] {
            let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
                initial_time_ns: 0,
                unix_time_origin_ns: DocumentUnixTime::from_nanos(nanoseconds),
            });
            assert!(clock.javascript_date_time_microseconds().unwrap().is_nan());
            assert_eq!(clock.terminal_error(), None);
        }
    }

    #[test]
    fn javascript_date_precision_loss_latches_before_page_visible_rounding() {
        const ADVERSARIAL_MILLISECONDS: i128 = 8_639_999_999_999_979;

        for milliseconds in [ADVERSARIAL_MILLISECONDS, -ADVERSARIAL_MILLISECONDS] {
            let unix_time =
                DocumentUnixTime::from_nanos(milliseconds * NANOSECONDS_PER_MILLISECOND);
            let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
                initial_time_ns: 0,
                unix_time_origin_ns: unix_time,
            });
            let error = clock
                .javascript_date_time_microseconds()
                .expect_err("unrepresentable TimeClip millisecond must fail closed");
            assert!(matches!(
                error,
                DocumentClockError::JavaScriptDatePrecisionLoss {
                    unix_time: observed_time,
                    expected_milliseconds,
                    observed_milliseconds,
                } if observed_time == unix_time &&
                    expected_milliseconds == milliseconds &&
                    observed_milliseconds != expected_milliseconds
            ));
            assert_eq!(clock.terminal_error(), Some(error));
            assert_eq!(clock.advance_to(DocumentTime::from_nanos(1)), Err(error));
            assert_eq!(clock.now(), DocumentTime::ZERO);
        }
    }

    #[test]
    fn configuration_validation_is_pure_and_rejects_initial_wall_time_overflow() {
        assert_eq!(
            DocumentClock::validate_configuration(DocumentClockConfiguration::Realtime),
            Ok(()),
        );
        assert_eq!(
            DocumentClock::validate_configuration(DocumentClockConfiguration::Controlled {
                initial_time_ns: 1,
                unix_time_origin_ns: DocumentUnixTime::from_nanos(i128::MAX),
            }),
            Err(DocumentClockError::Overflow),
        );
    }

    #[test]
    fn wall_time_overflow_is_sticky_and_precedes_monotonic_mutation() {
        assert!(matches!(
            DocumentClock::try_new(DocumentClockConfiguration::Controlled {
                initial_time_ns: 1,
                unix_time_origin_ns: DocumentUnixTime::from_nanos(i128::MAX),
            }),
            Err(DocumentClockError::Overflow)
        ));

        let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 0,
            unix_time_origin_ns: DocumentUnixTime::from_nanos(i128::MAX),
        });
        assert_eq!(
            clock.advance_to(DocumentTime::from_nanos(1)),
            Err(DocumentClockError::Overflow)
        );
        assert_eq!(clock.now(), DocumentTime::ZERO);
        assert_eq!(clock.terminal_error(), Some(DocumentClockError::Overflow));
        assert_eq!(
            clock.advance_to(DocumentTime::ZERO),
            Err(DocumentClockError::Overflow)
        );
    }

    #[test]
    fn surface_adapter_can_latch_its_exact_first_clock_terminal() {
        let clock = controlled_clock(7);
        let first = DocumentClockError::TimeMovedBackwards {
            current: DocumentTime::from_nanos(9),
            requested: DocumentTime::from_nanos(7),
        };
        assert_eq!(clock.latch_terminal_error(first), Ok(first));
        assert_eq!(
            clock.latch_terminal_error(DocumentClockError::Overflow),
            Ok(first)
        );
        assert_eq!(
            DocumentClock::default().latch_terminal_error(DocumentClockError::Overflow),
            Err(DocumentClockError::RealtimeClock)
        );
    }

    #[test]
    fn first_unsupported_surface_is_sticky_fail_closed_evidence() {
        let clock = controlled_clock(0);
        assert_eq!(clock.unsupported_surface(), None);
        assert_eq!(
            clock.require_surface(DocumentTimeSurface::HostTimestamp),
            Err(DocumentClockError::UnsupportedSurface(
                DocumentTimeSurface::HostTimestamp,
            ))
        );
        assert_eq!(
            clock.require_surface(DocumentTimeSurface::Worker),
            Err(DocumentClockError::UnsupportedSurface(
                DocumentTimeSurface::Worker,
            ))
        );
        assert_eq!(
            clock.unsupported_surface(),
            Some(DocumentTimeSurface::HostTimestamp)
        );
    }

    #[test]
    fn controlled_same_event_loop_iframe_is_unsupported_but_realtime_is_unchanged() {
        let controlled = controlled_clock(0);
        assert_eq!(
            controlled.require_surface(DocumentTimeSurface::SameEventLoopIframe),
            Err(DocumentClockError::UnsupportedSurface(
                DocumentTimeSurface::SameEventLoopIframe,
            ))
        );
        assert_eq!(
            controlled.unsupported_surface(),
            Some(DocumentTimeSurface::SameEventLoopIframe)
        );

        let realtime = DocumentClock::default();
        assert_eq!(
            realtime.require_surface(DocumentTimeSurface::SameEventLoopIframe),
            Ok(())
        );
        assert_eq!(realtime.unsupported_surface(), None);
    }

    #[test]
    fn controlled_resource_thread_io_is_sticky_and_realtime_is_unchanged() {
        let controlled = controlled_clock(0);
        assert_eq!(
            controlled.require_surface(DocumentTimeSurface::ResourceThreadIo),
            Err(DocumentClockError::UnsupportedSurface(
                DocumentTimeSurface::ResourceThreadIo,
            ))
        );
        assert_eq!(
            controlled.unsupported_surface(),
            Some(DocumentTimeSurface::ResourceThreadIo)
        );

        let realtime = DocumentClock::default();
        assert_eq!(
            realtime.require_surface(DocumentTimeSurface::ResourceThreadIo),
            Ok(())
        );
        assert_eq!(realtime.unsupported_surface(), None);
    }

    #[test]
    fn sticky_clock_terminal_rejects_new_and_already_due_timer_callbacks() {
        let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 0,
            unix_time_origin_ns: DocumentUnixTime::from_nanos(i128::MAX),
        });
        let mut scheduler = TimerScheduler::with_clock(clock.clone());
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let due = scheduler
            .try_schedule_timer(recording_request(&recorded, 1, Duration::ZERO))
            .unwrap();
        let due = TimerDeadlineSnapshot {
            scheduler_id: scheduler.id(),
            id: due,
            deadline: DocumentTime::ZERO,
        };

        assert_eq!(
            clock.advance_to(DocumentTime::from_nanos(1)),
            Err(DocumentClockError::Overflow),
        );
        assert_eq!(
            scheduler.try_schedule_timer(recording_request(&recorded, 2, Duration::ZERO)),
            Err(TimerControlError::Clock(DocumentClockError::Overflow)),
        );
        assert_eq!(
            scheduler.activate_due_timer(due),
            Err(TimerControlError::Clock(DocumentClockError::Overflow)),
        );
        assert_eq!(scheduler.finite_deadline_snapshot(), Ok(Some(due)));
        assert!(recorded.lock().unwrap().is_empty());
    }

    #[test]
    fn one_rendering_snapshot_drives_raf_and_document_timeline() {
        let clock = controlled_clock(5_000_000);
        let origin = clock.now();
        clock
            .advance_to(DocumentTime::from_nanos(12_000_000))
            .unwrap();
        let frame = clock.rendering_time().unwrap();
        let raf = clock
            .duration_since_for_surface(
                DocumentTimeSurface::AnimationFrame,
                origin,
                frame.document_time(),
            )
            .unwrap();
        let timeline = clock
            .duration_since_for_surface(
                DocumentTimeSurface::DocumentTimeline,
                origin,
                frame.document_time(),
            )
            .unwrap();

        assert_eq!(raf, Duration::from_millis(7));
        assert_eq!(timeline, raf);
        clock
            .advance_to(DocumentTime::from_nanos(20_000_000))
            .unwrap();
        assert_eq!(frame.document_time(), DocumentTime::from_nanos(12_000_000));
    }

    #[test]
    fn clock_identity_is_shared_only_across_clones() {
        let clock = controlled_clock(0);
        assert_eq!(clock.id(), clock.clone().id());
        assert_ne!(clock.id(), controlled_clock(0).id());
    }

    #[test]
    fn postcard_round_trips_widened_controlled_time_endpoints() {
        const WIDE_SPAN_NS: u128 = 9_007_199_254_740_991_000_000;
        const NEGATIVE_EPOCH_NS: i128 = -8_640_000_000_000_000_000_000;

        assert_postcard_round_trip(DocumentClockConfiguration::Controlled {
            initial_time_ns: WIDE_SPAN_NS,
            unix_time_origin_ns: DocumentUnixTime::from_nanos(NEGATIVE_EPOCH_NS),
        });
        assert_postcard_round_trip(TimerSchedulerId(u64::MAX));
        assert_postcard_round_trip(TimerDeadlineSnapshot {
            scheduler_id: TimerSchedulerId(u64::MAX),
            id: TimerId(u64::MAX),
            deadline: DocumentTime::from_nanos(u128::MAX),
        });
        assert_postcard_round_trip(TimerDeadlineJoin {
            scheduler_id: TimerSchedulerId(u64::MAX),
            id: TimerId(u64::MAX),
            deadline: Some(DocumentTime::from_nanos(u128::MAX)),
        });
        assert_postcard_round_trip(DocumentClockError::JavaScriptDatePrecisionLoss {
            unix_time: DocumentUnixTime::from_nanos(i128::MIN),
            expected_milliseconds: i128::MAX,
            observed_milliseconds: i128::MIN,
        });
        assert_postcard_round_trip(TimerControlError::StaleDeadline {
            expected: TimerDeadlineSnapshot {
                scheduler_id: TimerSchedulerId(1),
                id: TimerId(1),
                deadline: DocumentTime::from_nanos(2),
            },
            observed: Some(TimerDeadlineSnapshot {
                scheduler_id: TimerSchedulerId(1),
                id: TimerId(3),
                deadline: DocumentTime::from_nanos(4),
            }),
        });
        assert_postcard_round_trip(TimerDeadlineJoinError::SchedulerMismatch {
            expected: TimerSchedulerId(1),
            observed: TimerSchedulerId(2),
        });
    }

    #[test]
    fn postcard_preserves_controlled_time_enum_discriminants() {
        let controlled = DocumentClockConfiguration::Controlled {
            initial_time_ns: 0,
            unix_time_origin_ns: DocumentUnixTime::default(),
        };
        assert_eq!(
            postcard::to_stdvec(&DocumentClockConfiguration::Realtime).unwrap()[0],
            0,
        );
        assert_eq!(postcard::to_stdvec(&controlled).unwrap()[0], 1);

        for (index, surface) in [
            DocumentTimeSurface::WindowTimers,
            DocumentTimeSurface::SameEventLoopIframe,
            DocumentTimeSurface::JavaScriptDate,
            DocumentTimeSurface::Performance,
            DocumentTimeSurface::HostTimestamp,
            DocumentTimeSurface::UpdateRendering,
            DocumentTimeSurface::AnimationFrame,
            DocumentTimeSurface::DocumentTimeline,
            DocumentTimeSurface::Worker,
            DocumentTimeSurface::Worklet,
            DocumentTimeSurface::CrossEventLoopIframe,
            DocumentTimeSurface::CrossEventLoopNavigation,
            DocumentTimeSurface::AuxiliaryWebView,
            DocumentTimeSurface::ResourceThreadIo,
            DocumentTimeSurface::ExternalSubscription,
            DocumentTimeSurface::NativeMedia,
            DocumentTimeSurface::EmbedderControl,
        ]
        .into_iter()
        .enumerate()
        {
            assert_eq!(postcard::to_stdvec(&surface).unwrap()[0], index as u8);
        }

        let clock_errors = [
            DocumentClockError::RealtimeClock,
            DocumentClockError::TimeMovedBackwards {
                current: DocumentTime::ZERO,
                requested: DocumentTime::ZERO,
            },
            DocumentClockError::TimeChanged {
                expected: DocumentTime::ZERO,
                observed: DocumentTime::ZERO,
            },
            DocumentClockError::Overflow,
            DocumentClockError::UnsupportedSurface(DocumentTimeSurface::Worker),
            DocumentClockError::JavaScriptDatePrecisionLoss {
                unix_time: DocumentUnixTime::default(),
                expected_milliseconds: 0,
                observed_milliseconds: 1,
            },
        ];
        for (index, error) in clock_errors.into_iter().enumerate() {
            assert_eq!(postcard::to_stdvec(&error).unwrap()[0], index as u8);
        }

        let deadline = TimerDeadlineSnapshot {
            scheduler_id: TimerSchedulerId(0),
            id: TimerId(0),
            deadline: DocumentTime::ZERO,
        };
        let timer_errors = [
            TimerControlError::Clock(DocumentClockError::Overflow),
            TimerControlError::DeadlineOverflow,
            TimerControlError::SequenceExhausted,
            TimerControlError::RealtimeScheduler,
            TimerControlError::StaleDeadline {
                expected: deadline,
                observed: None,
            },
            TimerControlError::TimerNotDue {
                deadline: DocumentTime::ZERO,
                now: DocumentTime::ZERO,
            },
        ];
        for (index, error) in timer_errors.into_iter().enumerate() {
            assert_eq!(postcard::to_stdvec(&error).unwrap()[0], index as u8);
        }

        let join_errors = [
            TimerDeadlineJoinError::RealtimeScheduler,
            TimerDeadlineJoinError::SchedulerMismatch {
                expected: TimerSchedulerId(0),
                observed: TimerSchedulerId(1),
            },
        ];
        for (index, error) in join_errors.into_iter().enumerate() {
            assert_eq!(postcard::to_stdvec(&error).unwrap()[0], index as u8);
        }
    }
}
