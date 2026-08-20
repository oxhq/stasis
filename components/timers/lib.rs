/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A generic timer scheduler module that can be integrated into a crossbeam based event
//! loop or used to launch a background timer thread.

#![deny(unsafe_code)]

use std::cmp::{self, Ord};
use std::collections::BinaryHeap;
use std::fmt;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, after, never};
use malloc_size_of_derive::MallocSizeOf;
use serde::{Deserialize, Serialize};

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
            if !observed_time_clip.is_finite()
                || observed_time_clip as i128 != expected_milliseconds
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
    /// Construct a clock from its immutable mode configuration.
    pub fn new(configuration: DocumentClockConfiguration) -> Self {
        Self::try_new(configuration).expect("invalid document clock configuration")
    }

    /// Construct a clock after validating every controlled-time representation boundary.
    pub fn try_new(configuration: DocumentClockConfiguration) -> Result<Self, DocumentClockError> {
        let inner = match configuration {
            DocumentClockConfiguration::Realtime => DocumentClockInner::Realtime {
                origin: Instant::now(),
            },
            DocumentClockConfiguration::Controlled {
                initial_time_ns,
                unix_time_origin_ns,
            } => {
                let initial_time = DocumentTime::from_nanos(initial_time_ns);
                unix_time_origin_ns.checked_add_document_time(initial_time)?;
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
        if !self.is_controlled()
            || matches!(
                surface,
                DocumentTimeSurface::WindowTimers | DocumentTimeSurface::JavaScriptDate
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

/// The finite deadline exposed by a controlled scheduler.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TimerDeadlineSnapshot {
    /// Stable identity and insertion order for this event.
    pub id: TimerId,
    /// Absolute integer-nanosecond offset in the document clock.
    pub deadline: DocumentTime,
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
    /// The caller tried to activate a snapshot that is no longer current.
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
        Self {
            queue: BinaryHeap::new(),
            next_id: 0,
            clock,
        }
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
            id: event.id,
            deadline: event.deadline,
        }))
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
        self.validate_and_advance_to(expected)?;
        self.activate_due_timer(expected)
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
        self.queue
            .pop()
            .expect("a matching finite-deadline snapshot must still have an event")
            .request
            .dispatch();
        Ok(())
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
    use std::sync::{Arc, Mutex};

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
    fn rendering_surfaces_remain_fail_closed_until_their_integration_slice() {
        let clock = controlled_clock(5_000_000);
        assert_eq!(
            clock.rendering_time(),
            Err(DocumentClockError::UnsupportedSurface(
                DocumentTimeSurface::UpdateRendering,
            )),
        );
        for surface in [
            DocumentTimeSurface::AnimationFrame,
            DocumentTimeSurface::DocumentTimeline,
        ] {
            assert_eq!(
                clock.duration_since_for_surface(surface, DocumentTime::ZERO, DocumentTime::ZERO,),
                Err(DocumentClockError::UnsupportedSurface(surface)),
            );
        }
        assert_eq!(
            clock.unsupported_surface(),
            Some(DocumentTimeSurface::UpdateRendering),
        );
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
        assert_postcard_round_trip(TimerDeadlineSnapshot {
            id: TimerId(u64::MAX),
            deadline: DocumentTime::from_nanos(u128::MAX),
        });
        assert_postcard_round_trip(DocumentClockError::JavaScriptDatePrecisionLoss {
            unix_time: DocumentUnixTime::from_nanos(i128::MIN),
            expected_milliseconds: i128::MAX,
            observed_milliseconds: i128::MIN,
        });
        assert_postcard_round_trip(TimerControlError::StaleDeadline {
            expected: TimerDeadlineSnapshot {
                id: TimerId(1),
                deadline: DocumentTime::from_nanos(2),
            },
            observed: Some(TimerDeadlineSnapshot {
                id: TimerId(3),
                deadline: DocumentTime::from_nanos(4),
            }),
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
    }
}
