/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::cmp::{Ord, Ordering};
use std::collections::VecDeque;
use std::default::Default;
use std::rc::Rc;
use std::time::Duration;

use deny_public_fields::DenyPublicFields;
use js::context::JSContext;
use js::jsapi::Heap;
use js::jsval::{JSVal, UndefinedValue};
use js::rust::wrappers2::JS_GetScriptedCallerPrivate;
use js::rust::{HandleValue, IntoHandle};
use net_traits::request::ParserMetadata;
use rustc_hash::FxHashMap;
use script_bindings::cell::DomRefCell;
use serde::{Deserialize, Serialize};
use servo_base::id::PipelineId;
use servo_config::pref;
use servo_url::ServoUrl;
use timers::{
    BoxedTimerCallback, DocumentClock, DocumentClockError, DocumentTime, DocumentTimeSurface,
    TimerControlError, TimerEventRequest, TimerId,
};

use crate::dom::bindings::callback::ExceptionHandling::Report;
use crate::dom::bindings::codegen::Bindings::FunctionBinding::Function;
use crate::dom::bindings::codegen::UnionTypes::TrustedScriptOrString;
use crate::dom::bindings::error::Fallible;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::root::{AsHandleValue, Dom};
use crate::dom::bindings::str::DOMString;
use crate::dom::csp::CspReporting;
use crate::dom::document::RefreshRedirectDue;
use crate::dom::eventsource::EventSourceTimeoutCallback;
use crate::dom::globalscope::GlobalScope;
use crate::dom::globalscope::script_execution::{ErrorReporting, RethrowErrors};
#[cfg(feature = "testbinding")]
use crate::dom::testbinding::TestBindingCallback;
use crate::dom::trustedtypes::trustedscript::TrustedScript;
use crate::dom::types::{Window, WorkerGlobalScope};
use crate::dom::xmlhttprequest::XHRTimeoutCallback;
use crate::event_loop::script_thread::ScriptThread;
use crate::modules::script_module::{ScriptFetchOptions, module_script_from_reference_private};
use crate::script_runtime::IntroductionType;
use crate::tasks::task_source::SendableTaskSource;

type TimerKey = i32;
type RunStepsDeadline = DocumentTime;
type CompletionStep = Box<dyn FnOnce(&mut JSContext, &GlobalScope) + 'static>;

/// <https://html.spec.whatwg.org/multipage/#run-steps-after-a-timeout>
/// OrderingIdentifier per spec ("orderingIdentifier")
type OrderingIdentifier = DOMString;

#[derive(JSTraceable, MallocSizeOf)]
struct OrderingEntry {
    milliseconds: u64,
    start_seq: u64,
    handle: OneshotTimerHandle,
}

// Per-ordering queues map
type OrderingQueues = FxHashMap<OrderingIdentifier, Vec<OrderingEntry>>;

// Active timers map for Run Steps After A Timeout
type RunStepsActiveMap = FxHashMap<TimerKey, RunStepsDeadline>;

#[derive(Clone, Copy, Debug, Eq, Hash, JSTraceable, MallocSizeOf, Ord, PartialEq, PartialOrd)]
pub(crate) struct OneshotTimerHandle(i32);

impl OneshotTimerHandle {
    pub(crate) const fn sequence(self) -> i32 {
        self.0
    }
}

#[derive(DenyPublicFields, JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct OneshotTimers {
    global_scope: Dom<GlobalScope>,
    js_timers: JsTimers,
    next_timer_handle: Cell<OneshotTimerHandle>,
    timers: DomRefCell<VecDeque<OneshotTimer>>,
    #[no_trace]
    document_clock: DocumentClock,
    #[no_trace]
    timebase: Cell<TimerTimebase>,
    /// The first checked failure in this logical timer layer. Once set, no more callbacks are
    /// scheduled or invoked; the controlled runtime can report the failure instead of exposing a
    /// wrapped deadline or falling back to host time.
    #[no_trace]
    #[ignore_malloc_size_of = "Copy-only checked failure state"]
    terminal_error: Cell<Option<DocumentClockError>>,
    /// Calls to `fire_timer` with a different argument than this get ignored.
    /// They were previously scheduled and got invalidated when
    ///  - timers were suspended,
    ///  - the timer it was scheduled for got canceled or
    ///  - a timer was added with an earlier callback time. In this case the
    ///    original timer is rescheduled when it is the next one to get called.
    #[no_trace]
    expected_event_id: Cell<TimerEventId>,
    /// The low-level scheduler event currently representing the earliest logical DOM timer.
    #[no_trace]
    scheduled_timer_id: Cell<Option<TimerId>>,
    /// <https://html.spec.whatwg.org/multipage/#map-of-active-timers>
    /// TODO this should also be used for the other timers
    /// as per <html.spec.whatwg.org/multipage/#map-of-settimeout-and-setinterval-ids>Z.
    #[no_trace]
    map_of_active_timers: DomRefCell<RunStepsActiveMap>,

    /// <https://html.spec.whatwg.org/multipage/#run-steps-after-a-timeout>
    /// Step 4.2 Wait until any invocations of this algorithm that had the same global and orderingIdentifier,
    /// that started before this one, and whose milliseconds is less than or equal to this one's, have completed.
    runsteps_queues: DomRefCell<OrderingQueues>,

    /// <html.spec.whatwg.org/multipage/#timers:unique-internal-value-5>
    next_runsteps_key: Cell<TimerKey>,

    /// <https://html.spec.whatwg.org/multipage/#run-steps-after-a-timeout>
    /// Start order sequence to break ties for Step 4.2.
    runsteps_start_seq: Cell<u64>,

    /// Stable creation order for logical timers that share a deadline.
    creation_sequence: Cell<u64>,
}

#[derive(DenyPublicFields, JSTraceable, MallocSizeOf)]
struct OneshotTimer {
    handle: OneshotTimerHandle,
    #[no_trace]
    source: TimerSource,
    callback: OneshotTimerCallback,
    #[no_trace]
    scheduled_for: DocumentTime,
    creation_sequence: u64,
}

#[derive(Clone, Copy, Debug, Default, Eq, MallocSizeOf, PartialEq)]
struct TimerTimebase {
    suspended_at: Option<DocumentTime>,
    suspension_offset: Duration,
}

impl TimerTimebase {
    fn now(self, clock: &DocumentClock) -> Result<DocumentTime, DocumentClockError> {
        if let Some(error) = clock.terminal_error() {
            return Err(error);
        }
        let now = match self.suspended_at {
            Some(suspended_at) => suspended_at,
            None => clock.try_now()?,
        };
        now.checked_sub(self.suspension_offset)
    }

    fn suspend(&mut self, clock: &DocumentClock) -> Result<bool, DocumentClockError> {
        if self.suspended_at.is_some() {
            return Ok(false);
        }
        if let Some(error) = clock.terminal_error() {
            return Err(error);
        }
        self.suspended_at = Some(clock.try_now()?);
        Ok(true)
    }

    fn resume(&mut self, clock: &DocumentClock) -> Result<bool, DocumentClockError> {
        let Some(suspended_at) = self.suspended_at else {
            return Ok(false);
        };
        if let Some(error) = clock.terminal_error() {
            return Err(error);
        }
        let paused_for = clock.try_now()?.checked_duration_since(suspended_at)?;
        self.suspension_offset = self
            .suspension_offset
            .checked_add(paused_for)
            .ok_or(DocumentClockError::Overflow)?;
        self.suspended_at = None;
        Ok(true)
    }
}

/// The semantic source and recurrence class of a pending logical DOM timer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DomTimerKind {
    JsOneShot,
    JsInterval { requested_period: Duration },
    XhrTimeout,
    EventSourceReconnect,
    RefreshRedirect,
    RunStepsAfterTimeout,
    #[cfg(feature = "testbinding")]
    TestBindingCallback,
}

/// Stable metadata for a pending logical DOM timer, ordered by deadline then creation sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DomTimerMetadata {
    pub(crate) handle: OneshotTimerHandle,
    pub(crate) javascript_handle: Option<i32>,
    pub(crate) creation_sequence: u64,
    pub(crate) deadline: DocumentTime,
    pub(crate) suspended: bool,
    /// Whether this timer may be selected in the next controlled turn. A run-steps timer can be
    /// blocked by an earlier entry in its ordering-identifier queue even when its deadline is due.
    pub(crate) eligible_in_controlled_turn: bool,
    pub(crate) kind: DomTimerKind,
}

// This enum is required to work around the fact that trait objects do not support generic methods.
// A replacement trait would have a method such as
//     `invoke<T: DomObject>(self: Box<Self>, this: &T, js_timers: &JsTimers);`.
#[derive(JSTraceable, MallocSizeOf)]
pub(crate) enum OneshotTimerCallback {
    XhrTimeout(XHRTimeoutCallback),
    EventSourceTimeout(EventSourceTimeoutCallback),
    JsTimer(JsTimerTask),
    #[cfg(feature = "testbinding")]
    TestBindingCallback(TestBindingCallback),
    RefreshRedirectDue(RefreshRedirectDue),
    /// <https://html.spec.whatwg.org/multipage/#run-steps-after-a-timeout>
    RunStepsAfterTimeout {
        /// Step 1. timerKey
        timer_key: i32,
        /// Step 4. orderingIdentifier
        ordering_id: DOMString,
        /// Spec: milliseconds (the algorithm input)
        milliseconds: u64,
        /// Perform completionSteps.
        #[no_trace]
        #[ignore_malloc_size_of = "Closure"]
        completion: CompletionStep,
    },
}

impl OneshotTimerCallback {
    fn invoke(self, cx: &mut JSContext, global: &GlobalScope, js_timers: &JsTimers) {
        match self {
            OneshotTimerCallback::XhrTimeout(callback) => callback.invoke(cx),
            OneshotTimerCallback::EventSourceTimeout(callback) => callback.invoke(),
            OneshotTimerCallback::JsTimer(task) => task.invoke(cx, global, js_timers),
            #[cfg(feature = "testbinding")]
            OneshotTimerCallback::TestBindingCallback(callback) => callback.invoke(cx),
            OneshotTimerCallback::RefreshRedirectDue(callback) => callback.invoke(cx, global),
            OneshotTimerCallback::RunStepsAfterTimeout { completion, .. } => {
                // <https://html.spec.whatwg.org/multipage/#run-steps-after-a-timeout>
                // Step 4.4 Perform completionSteps.
                completion(cx, global);
            },
        }
    }
}

impl Ord for OneshotTimer {
    fn cmp(&self, other: &OneshotTimer) -> Ordering {
        compare_timer_order(
            self.scheduled_for,
            self.creation_sequence,
            other.scheduled_for,
            other.creation_sequence,
        )
    }
}

fn compare_timer_order(
    left_deadline: DocumentTime,
    left_sequence: u64,
    right_deadline: DocumentTime,
    right_sequence: u64,
) -> Ordering {
    match left_deadline.cmp(&right_deadline).reverse() {
        Ordering::Equal => left_sequence.cmp(&right_sequence).reverse(),
        ordering => ordering,
    }
}

impl PartialOrd for OneshotTimer {
    fn partial_cmp(&self, other: &OneshotTimer) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Eq for OneshotTimer {}
impl PartialEq for OneshotTimer {
    fn eq(&self, other: &OneshotTimer) -> bool {
        std::ptr::eq(self, other)
    }
}

impl OneshotTimer {
    fn metadata(
        &self,
        timebase: TimerTimebase,
        eligible_in_controlled_turn: bool,
    ) -> Result<DomTimerMetadata, DocumentClockError> {
        let (javascript_handle, kind) = match &self.callback {
            OneshotTimerCallback::JsTimer(task) => (
                Some(task.handle.0),
                match task.is_interval {
                    IsInterval::Interval => DomTimerKind::JsInterval {
                        requested_period: task.duration,
                    },
                    IsInterval::NonInterval => DomTimerKind::JsOneShot,
                },
            ),
            OneshotTimerCallback::XhrTimeout(_) => (None, DomTimerKind::XhrTimeout),
            OneshotTimerCallback::EventSourceTimeout(_) => {
                (None, DomTimerKind::EventSourceReconnect)
            },
            OneshotTimerCallback::RefreshRedirectDue(_) => {
                (None, DomTimerKind::RefreshRedirect)
            },
            OneshotTimerCallback::RunStepsAfterTimeout { .. } => {
                (None, DomTimerKind::RunStepsAfterTimeout)
            },
            #[cfg(feature = "testbinding")]
            OneshotTimerCallback::TestBindingCallback(_) => {
                (None, DomTimerKind::TestBindingCallback)
            },
        };
        Ok(DomTimerMetadata {
            handle: self.handle,
            javascript_handle,
            creation_sequence: self.creation_sequence,
            deadline: self.scheduled_for.checked_add(timebase.suspension_offset)?,
            suspended: timebase.suspended_at.is_some(),
            eligible_in_controlled_turn,
            kind,
        })
    }
}

fn insert_timer(timers: &mut VecDeque<OneshotTimer>, timer: OneshotTimer) {
    let insertion_index = timers.binary_search(&timer).err().unwrap();
    timers.insert(insertion_index, timer);
}

fn runsteps_timer_is_eligible(timer: &OneshotTimer, queues: &OrderingQueues) -> bool {
    let OneshotTimerCallback::RunStepsAfterTimeout { ordering_id, .. } = &timer.callback else {
        return true;
    };
    queues
        .get(ordering_id)
        .and_then(|queue| queue.first())
        .is_none_or(|head| head.handle == timer.handle)
}

fn select_timer_for_outer<'a>(
    timers: &'a VecDeque<OneshotTimer>,
    runsteps_queues: &OrderingQueues,
    controlled: bool,
) -> Option<&'a OneshotTimer> {
    if controlled {
        timers
            .iter()
            .rev()
            .find(|timer| runsteps_timer_is_eligible(timer, runsteps_queues))
    } else {
        timers.back()
    }
}

fn take_due_timers_for_turn(
    timers: &mut VecDeque<OneshotTimer>,
    runsteps_queues: &OrderingQueues,
    now: DocumentTime,
    controlled: bool,
) -> Vec<OneshotTimer> {
    if controlled {
        let selected = timers
            .iter()
            .enumerate()
            .rev()
            .find_map(|(index, timer)| {
                (timer.scheduled_for <= now &&
                    runsteps_timer_is_eligible(timer, runsteps_queues))
                .then_some(index)
            });
        return selected
            .and_then(|index| timers.remove(index))
            .into_iter()
            .collect();
    }

    // Preserve Servo's realtime batching behavior. Controlled mode deliberately selects one
    // eligible logical timer so the normal task boundary performs a microtask checkpoint.
    let mut due = Vec::new();
    while timers
        .back()
        .is_some_and(|timer| timer.scheduled_for <= now)
    {
        due.push(timers.pop_back().unwrap());
    }
    due
}

fn map_timer_control_error(error: TimerControlError) -> DocumentClockError {
    match error {
        TimerControlError::Clock(error) => error,
        TimerControlError::DeadlineOverflow | TimerControlError::SequenceExhausted => {
            DocumentClockError::Overflow
        },
        TimerControlError::RealtimeScheduler |
        TimerControlError::StaleDeadline { .. } |
        TimerControlError::TimerNotDue { .. } => DocumentClockError::Overflow,
    }
}

fn replace_scheduled_timer_id(
    slot: &Cell<Option<TimerId>>,
    replacement: Option<TimerId>,
) -> Option<TimerId> {
    slot.replace(replacement)
}

impl OneshotTimers {
    pub(crate) fn new(global_scope: &GlobalScope) -> OneshotTimers {
        let document_clock = global_scope.document_clock();
        let surface = if global_scope.is::<Window>() {
            DocumentTimeSurface::WindowTimers
        } else {
            DocumentTimeSurface::Worker
        };
        let terminal_error = document_clock.require_surface(surface).err();
        OneshotTimers {
            global_scope: Dom::from_ref(global_scope),
            document_clock,
            js_timers: JsTimers::default(),
            next_timer_handle: Cell::new(OneshotTimerHandle(1)),
            timers: DomRefCell::new(VecDeque::new()),
            timebase: Cell::new(TimerTimebase::default()),
            terminal_error: Cell::new(terminal_error),
            expected_event_id: Cell::new(TimerEventId(0)),
            scheduled_timer_id: Cell::new(None),
            map_of_active_timers: Default::default(),
            runsteps_queues: Default::default(),
            next_runsteps_key: Cell::new(1),
            runsteps_start_seq: Cell::new(0),
            creation_sequence: Cell::new(0),
        }
    }

    fn terminal_error(&self) -> Option<DocumentClockError> {
        self.terminal_error
            .get()
            .or_else(|| self.document_clock.terminal_error())
    }

    fn latch_terminal(&self, error: DocumentClockError) {
        if self.terminal_error.get().is_none() {
            self.terminal_error.set(Some(error));
        }
        self.cancel_scheduled_timer();
    }

    pub(crate) fn latch_timer_error(&self, error: DocumentClockError) {
        self.latch_terminal(error);
    }

    /// Return the first checked logical-timer failure. A terminal timer layer never falls back to
    /// host time and never invokes another callback.
    pub(crate) fn checked_terminal_error(&self) -> Option<DocumentClockError> {
        self.terminal_error()
    }

    /// Return stable metadata for pending logical timers in execution order.
    pub(crate) fn pending_timer_metadata(
        &self,
    ) -> Result<Vec<DomTimerMetadata>, DocumentClockError> {
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        let timebase = self.timebase.get();
        let runsteps_queues = self.runsteps_queues.borrow();
        let metadata = self
            .timers
            .borrow()
            .iter()
            .rev()
            .map(|timer| {
                timer.metadata(
                    timebase,
                    runsteps_timer_is_eligible(timer, &runsteps_queues),
                )
            })
            .collect::<Result<Vec<_>, _>>();
        if let Err(error) = &metadata {
            self.latch_terminal(*error);
        }
        metadata
    }

    /// <https://html.spec.whatwg.org/multipage/#run-steps-after-a-timeout>
    #[inline]
    pub(crate) fn now_for_runsteps(&self) -> Result<DocumentTime, DocumentClockError> {
        // Step 2. Let startTime be the current high resolution time given global.
        let now = self.base_time();
        if let Err(error) = now {
            self.latch_terminal(error);
        }
        now
    }

    /// <https://html.spec.whatwg.org/multipage/#run-steps-after-a-timeout>
    /// Step 1. Let timerKey be a new unique internal value.
    pub(crate) fn fresh_runsteps_key(&self) -> Result<TimerKey, DocumentClockError> {
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        let k = self.next_runsteps_key.get();
        let Some(next) = k.checked_add(1) else {
            self.latch_terminal(DocumentClockError::Overflow);
            return Err(DocumentClockError::Overflow);
        };
        self.next_runsteps_key.set(next);
        Ok(k)
    }

    /// <https://html.spec.whatwg.org/multipage/#run-steps-after-a-timeout>
    /// Step 3. Set global's map of active timers[timerKey] to startTime plus milliseconds.
    pub(crate) fn runsteps_set_active(&self, timer_key: TimerKey, deadline: RunStepsDeadline) {
        self.map_of_active_timers
            .borrow_mut()
            .insert(timer_key, deadline);
    }

    /// <https://html.spec.whatwg.org/multipage/#run-steps-after-a-timeout>
    /// Helper for Step 4.2: maintain per-ordering sorted queue by (milliseconds, startSeq, handle).
    fn runsteps_enqueue_sorted(
        &self,
        ordering_id: &DOMString,
        handle: OneshotTimerHandle,
        milliseconds: u64,
    ) -> Result<(), DocumentClockError> {
        let mut map = self.runsteps_queues.borrow_mut();
        let q = map.entry(ordering_id.clone()).or_default();

        let seq = {
            let cur = self.runsteps_start_seq.get();
            self.runsteps_start_seq
                .set(cur.checked_add(1).ok_or(DocumentClockError::Overflow)?);
            cur
        };

        let key = OrderingEntry {
            milliseconds,
            start_seq: seq,
            handle,
        };

        let idx = q
            .binary_search_by(|ordering_entry| {
                match ordering_entry.milliseconds.cmp(&milliseconds) {
                    Ordering::Less => Ordering::Less,
                    Ordering::Greater => Ordering::Greater,
                    Ordering::Equal => ordering_entry.start_seq.cmp(&seq),
                }
            })
            .unwrap_or_else(|i| i);

        q.insert(idx, key);
        Ok(())
    }

    pub(crate) fn schedule_callback(
        &self,
        callback: OneshotTimerCallback,
        duration: Duration,
        source: TimerSource,
    ) -> OneshotTimerHandle {
        if self.terminal_error().is_some() {
            return OneshotTimerHandle(0);
        }
        let new_handle = self.next_timer_handle.get();
        let Some(next_handle) = new_handle.0.checked_add(1) else {
            self.latch_terminal(DocumentClockError::Overflow);
            return OneshotTimerHandle(0);
        };
        let creation_sequence = self.creation_sequence.get();
        let Some(next_creation_sequence) = creation_sequence.checked_add(1) else {
            self.latch_terminal(DocumentClockError::Overflow);
            return OneshotTimerHandle(0);
        };
        let scheduled_for = match self
            .base_time()
            .and_then(|now| now.checked_add(duration))
        {
            Ok(deadline) => deadline,
            Err(error) => {
                self.latch_terminal(error);
                return OneshotTimerHandle(0);
            },
        };
        self.next_timer_handle
            .set(OneshotTimerHandle(next_handle));
        self.creation_sequence.set(next_creation_sequence);

        let timer = OneshotTimer {
            handle: new_handle,
            source,
            callback,
            scheduled_for,
            creation_sequence,
        };

        // https://html.spec.whatwg.org/multipage/#run-steps-after-a-timeout
        // Step 4.2: maintain per-orderingIdentifier order by milliseconds (and start order for ties).
        if let OneshotTimerCallback::RunStepsAfterTimeout {
            ordering_id,
            milliseconds,
            ..
        } = &timer.callback
        {
            if let Err(error) =
                self.runsteps_enqueue_sorted(ordering_id, new_handle, *milliseconds)
            {
                self.latch_terminal(error);
                return OneshotTimerHandle(0);
            }
        }

        {
            let mut timers = self.timers.borrow_mut();
            insert_timer(&mut timers, timer);
        }

        if self.document_clock.is_controlled() || self.is_next_timer(new_handle) {
            self.schedule_timer_call();
        }

        new_handle
    }

    pub(crate) fn unschedule_callback(&self, handle: OneshotTimerHandle) {
        let was_next = self.is_next_timer(handle);

        self.timers.borrow_mut().retain(|t| t.handle != handle);

        if self.document_clock.is_controlled() || was_next {
            self.schedule_timer_call();
        }
    }

    fn is_next_timer(&self, handle: OneshotTimerHandle) -> bool {
        match self.timers.borrow().back() {
            None => false,
            Some(max_timer) => max_timer.handle == handle,
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#timer-initialisation-steps>
    pub(crate) fn fire_timer(&self, id: TimerEventId, cx: &mut JSContext) {
        // Step 9.2. If id does not exist in global's map of setTimeout and setInterval IDs, then abort these steps.
        let expected_id = self.expected_event_id.get();
        if expected_id != id {
            debug!(
                "ignoring timer fire event {:?} (expected {:?})",
                id, expected_id
            );
            return;
        }

        // The matching outer scheduler event has already been consumed. Clearing its identity
        // before any callback can schedule a replacement prevents us from retaining or canceling
        // an obsolete TimerId.
        replace_scheduled_timer_id(&self.scheduled_timer_id, None);

        if self.terminal_error().is_some() {
            return;
        }
        if self.timebase.get().suspended_at.is_some() {
            warn!("Ignoring a DOM timer task while its timebase is suspended.");
            return;
        }

        let base_time = match self.base_time() {
            Ok(base_time) => base_time,
            Err(error) => {
                self.latch_terminal(error);
                return;
            },
        };

        let Some(next_deadline) = self
            .timers
            .borrow()
            .back()
            .map(|timer| timer.scheduled_for)
        else {
            warn!("A DOM timer task fired after its logical timer was removed.");
            return;
        };
        if base_time < next_deadline {
            warn!("Unexpected timing!");
            // A low-level wake can be delivered before its queued task runs or after a timebase
            // transition. Re-arm the still-pending logical timer instead of stranding it.
            self.schedule_timer_call();
            return;
        }

        // Controlled mode pops exactly one eligible timer. Realtime mode preserves Servo's
        // existing due-timer batch semantics.
        let timers_to_run = {
            let runsteps_queues = self.runsteps_queues.borrow();
            let mut timers = self.timers.borrow_mut();
            take_due_timers_for_turn(
                &mut timers,
                &runsteps_queues,
                base_time,
                self.document_clock.is_controlled(),
            )
        };
        if timers_to_run.is_empty() {
            warn!("A DOM timer task fired without an eligible logical timer.");
            self.schedule_timer_call();
            return;
        }

        for timer in timers_to_run {
            // Since timers can be coalesced together inside a task,
            // this loop can keep running, including after an interrupt of the JS,
            // and prevent a clean-shutdown of a JS-running thread.
            // This check prevents such a situation.
            if !self.global_scope.can_continue_running() {
                return;
            }
            match &timer.callback {
                // TODO: https://github.com/servo/servo/issues/40060
                OneshotTimerCallback::RunStepsAfterTimeout { ordering_id, .. } => {
                    // Step 4.2 Wait until any invocations of this algorithm that had the same global and orderingIdentifier,
                    // that started before this one, and whose milliseconds is less than or equal to this one's, have completed.
                    let head_handle_opt = {
                        let queues_ref = self.runsteps_queues.borrow();
                        queues_ref
                            .get(ordering_id)
                            .and_then(|v| v.first().map(|t| t.handle))
                    };
                    let is_head = head_handle_opt.is_none_or(|head| head == timer.handle);

                    if !is_head {
                        // TODO: this re queuing would go away when we revisit timers implementation.
                        let rein = OneshotTimer {
                            handle: timer.handle,
                            source: timer.source,
                            callback: timer.callback,
                            scheduled_for: base_time,
                            creation_sequence: timer.creation_sequence,
                        };
                        let mut timers = self.timers.borrow_mut();
                        insert_timer(&mut timers, rein);
                        continue;
                    }

                    let (timer_key, ordering_id_owned, completion) = match timer.callback {
                        OneshotTimerCallback::RunStepsAfterTimeout {
                            timer_key,
                            ordering_id,
                            milliseconds: _,
                            completion,
                        } => (timer_key, ordering_id, completion),
                        _ => unreachable!(),
                    };

                    // Step 4.3 Optionally, wait a further implementation-defined length of time.
                    // (No additional delay applied.)

                    // Step 4.4 Perform completionSteps.
                    (completion)(cx, &self.global_scope);

                    // Step 4.5 Remove global's map of active timers[timerKey].
                    self.map_of_active_timers.borrow_mut().remove(&timer_key);

                    {
                        let mut queues_mut = self.runsteps_queues.borrow_mut();
                        if let Some(q) = queues_mut.get_mut(&ordering_id_owned) {
                            if !q.is_empty() {
                                q.remove(0);
                            }
                            if q.is_empty() {
                                queues_mut.remove(&ordering_id_owned);
                            }
                        }
                    }
                },
                _ => {
                    let cb = timer.callback;
                    cb.invoke(cx, &self.global_scope, &self.js_timers);
                },
            }
        }

        self.schedule_timer_call();
    }

    fn base_time(&self) -> Result<DocumentTime, DocumentClockError> {
        if let Some(error) = self.terminal_error() {
            return Err(error);
        }
        self.timebase.get().now(&self.document_clock)
    }

    pub(crate) fn slow_down(&self) {
        let min_duration_ms = pref!(js_timers_minimum_duration) as u64;
        self.js_timers
            .set_min_duration(Duration::from_millis(min_duration_ms));
    }

    pub(crate) fn speed_up(&self) {
        self.js_timers.remove_min_duration();
    }

    pub(crate) fn suspend(&self) {
        // Suspend is idempotent: do nothing if the timers are already suspended.
        let mut timebase = self.timebase.get();
        let suspended = match timebase.suspend(&self.document_clock) {
            Ok(suspended) => suspended,
            Err(error) => {
                self.latch_terminal(error);
                return;
            },
        };
        if !suspended {
            return warn!("Suspending an already suspended timer.");
        }

        debug!("Suspending timers.");
        self.timebase.set(timebase);
        let _ = self.invalidate_expected_event_id();
    }

    pub(crate) fn resume(&self) {
        // Resume is idempotent: do nothing if the timers are already resumed.
        let mut timebase = self.timebase.get();
        let resumed = match timebase.resume(&self.document_clock) {
            Ok(resumed) => resumed,
            Err(error) => {
                self.latch_terminal(error);
                return;
            },
        };
        if !resumed {
            return warn!("Resuming an already resumed timer.");
        }

        debug!("Resuming timers.");
        self.timebase.set(timebase);

        self.schedule_timer_call();
    }

    /// <https://html.spec.whatwg.org/multipage/#timer-initialisation-steps>
    fn schedule_timer_call(&self) {
        // Invalidate first even when the queue becomes empty: a low-level callback may already
        // have queued its task and can no longer be canceled by TimerId alone.
        let Some(expected_event_id) = self.invalidate_expected_event_id() else {
            return;
        };
        if self.terminal_error().is_some() {
            return;
        }
        if self.timebase.get().suspended_at.is_some() {
            // The timer will be scheduled when the pipeline is fully activated.
            return;
        }

        let selected = {
            let timers = self.timers.borrow();
            let runsteps_queues = self.runsteps_queues.borrow();
            select_timer_for_outer(
                &timers,
                &runsteps_queues,
                self.document_clock.is_controlled(),
            )
            .map(|timer| (timer.scheduled_for, timer.source))
        };
        let Some((scheduled_for, source)) = selected else {
            return;
        };

        // Step 12. Let completionStep be an algorithm step which queues a global
        // task on the timer task source given global to run task.
        let callback = TimerListener {
            context: Trusted::new(&*self.global_scope),
            task_source: self
                .global_scope
                .task_manager()
                .timer_task_source()
                .to_sendable(),
            source,
            id: expected_event_id,
        }
        .into_callback();

        let base_time = match self.base_time() {
            Ok(base_time) => base_time,
            Err(error) => {
                self.latch_terminal(error);
                return;
            },
        };
        let duration = if scheduled_for <= base_time {
            Duration::ZERO
        } else {
            match scheduled_for.checked_duration_since(base_time) {
                Ok(duration) => duration,
                Err(error) => {
                    self.latch_terminal(error);
                    return;
                },
            }
        };
        let event_request = TimerEventRequest {
            callback,
            duration,
        };

        match self.global_scope.try_schedule_timer(event_request) {
            Ok(timer_id) => {
                debug_assert!(
                    replace_scheduled_timer_id(&self.scheduled_timer_id, Some(timer_id)).is_none()
                );
            },
            Err(error) => self.latch_terminal(map_timer_control_error(error)),
        }
    }

    fn cancel_scheduled_timer(&self) {
        if let Some(timer_id) = replace_scheduled_timer_id(&self.scheduled_timer_id, None) {
            self.global_scope.cancel_timer(timer_id);
        }
    }

    fn invalidate_expected_event_id(&self) -> Option<TimerEventId> {
        self.cancel_scheduled_timer();
        let TimerEventId(currently_expected) = self.expected_event_id.get();
        let Some(next) = currently_expected.checked_add(1) else {
            self.latch_terminal(DocumentClockError::Overflow);
            return None;
        };
        let next_id = TimerEventId(next);
        debug!(
            "invalidating expected timer (was {:?}, now {:?}",
            currently_expected, next_id
        );
        self.expected_event_id.set(next_id);
        Some(next_id)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn set_timeout_or_interval(
        &self,
        cx: &mut JSContext,
        global: &GlobalScope,
        callback: TimerCallback,
        arguments: Vec<HandleValue>,
        timeout: Duration,
        is_interval: IsInterval,
        source: TimerSource,
    ) -> Fallible<i32> {
        self.js_timers.set_timeout_or_interval(
            cx,
            global,
            callback,
            arguments,
            timeout,
            is_interval,
            source,
        )
    }

    pub(crate) fn clear_timeout_or_interval(&self, global: &GlobalScope, handle: i32) {
        self.js_timers.clear_timeout_or_interval(global, handle)
    }
}

#[derive(Clone, Copy, Eq, Hash, JSTraceable, MallocSizeOf, Ord, PartialEq, PartialOrd)]
pub(crate) struct JsTimerHandle(i32);

#[derive(DenyPublicFields, JSTraceable, MallocSizeOf)]
pub(crate) struct JsTimers {
    next_timer_handle: Cell<JsTimerHandle>,
    /// <https://html.spec.whatwg.org/multipage/#list-of-active-timers>
    active_timers: DomRefCell<FxHashMap<JsTimerHandle, JsTimerEntry>>,
    /// The nesting level of the currently executing timer task or 0.
    nesting_level: Cell<u32>,
    /// Used to introduce a minimum delay in event intervals
    min_duration: Cell<Option<Duration>>,
}

#[derive(JSTraceable, MallocSizeOf)]
struct JsTimerEntry {
    oneshot_handle: OneshotTimerHandle,
}

// Holder for the various JS values associated with setTimeout
// (ie. function value to invoke and all arguments to pass
//      to the function when calling it)
// TODO: Handle rooting during invocation when movable GC is turned on
#[derive(JSTraceable, MallocSizeOf)]
pub(crate) struct JsTimerTask {
    handle: JsTimerHandle,
    #[no_trace]
    source: TimerSource,
    callback: InternalTimerCallback,
    is_interval: IsInterval,
    nesting_level: u32,
    duration: Duration,
    is_user_interacting: bool,
}

// Enum allowing more descriptive values for the is_interval field
#[derive(Clone, Copy, JSTraceable, MallocSizeOf, PartialEq)]
pub(crate) enum IsInterval {
    Interval,
    NonInterval,
}

pub(crate) enum TimerCallback {
    StringTimerCallback(TrustedScriptOrString),
    FunctionTimerCallback(Rc<Function>),
}

#[derive(Clone, JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, expect(crown::unrooted_must_root))]
enum InternalTimerCallback {
    StringTimerCallback(DOMString, InitiatingScriptFetchInfo),
    FunctionTimerCallback(
        #[conditional_malloc_size_of] Rc<Function>,
        #[ignore_malloc_size_of = "mozjs"] Rc<Box<[Heap<JSVal>]>>,
    ),
}

impl Default for JsTimers {
    fn default() -> Self {
        JsTimers {
            next_timer_handle: Cell::new(JsTimerHandle(1)),
            active_timers: DomRefCell::new(FxHashMap::default()),
            nesting_level: Cell::new(0),
            min_duration: Cell::new(None),
        }
    }
}

impl JsTimers {
    /// <https://html.spec.whatwg.org/multipage/#timer-initialisation-steps>
    #[allow(clippy::too_many_arguments)]
    #[cfg_attr(crown, expect(crown::unrooted_must_root))]
    pub(crate) fn set_timeout_or_interval(
        &self,
        cx: &mut JSContext,
        global: &GlobalScope,
        callback: TimerCallback,
        arguments: Vec<HandleValue>,
        timeout: Duration,
        is_interval: IsInterval,
        source: TimerSource,
    ) -> Fallible<i32> {
        let callback = match callback {
            TimerCallback::StringTimerCallback(trusted_script_or_string) => {
                // Step 9.6.1.1. Let globalName be "Window" if global is a Window object; "WorkerGlobalScope" otherwise.
                let global_name = if global.is::<Window>() {
                    "Window"
                } else {
                    "WorkerGlobalScope"
                };
                // Step 9.6.1.2. Let methodName be "setInterval" if repeat is true; "setTimeout" otherwise.
                let method_name = if is_interval == IsInterval::Interval {
                    "setInterval"
                } else {
                    "setTimeout"
                };
                // Step 9.6.1.3. Let sink be a concatenation of globalName, U+0020 SPACE, and methodName.
                let sink = format!("{} {}", global_name, method_name);
                // Step 9.6.1.4. Set handler to the result of invoking the
                // Get Trusted Type compliant string algorithm with TrustedScript, global, handler, sink, and "script".
                let code_str = TrustedScript::get_trusted_type_compliant_string(
                    cx,
                    global,
                    trusted_script_or_string,
                    &sink,
                )?;

                let initiating_script_fetch_info = active_script_fetch_info(cx, global);

                // Step 9.6.3. Perform EnsureCSPDoesNotBlockStringCompilation(realm, « », handler, handler, timer, « », handler).
                // If this throws an exception, catch it, report it for global, and abort these steps.
                if global
                    .get_csp_list()
                    .is_js_evaluation_allowed(cx, global, &code_str.str())
                {
                    // Step 9.6.2. Assert: handler is a string.
                    InternalTimerCallback::StringTimerCallback(
                        code_str,
                        initiating_script_fetch_info,
                    )
                } else {
                    return Ok(0);
                }
            },
            TimerCallback::FunctionTimerCallback(function) => {
                // This is a bit complicated, but this ensures that the vector's
                // buffer isn't reallocated (and moved) after setting the Heap values
                let mut args = Vec::with_capacity(arguments.len());
                for _ in 0..arguments.len() {
                    args.push(Heap::default());
                }
                for (i, item) in arguments.iter().enumerate() {
                    args.get_mut(i).unwrap().set(item.get());
                }
                // Step 9.5. If handler is a Function, then invoke handler given arguments and "report",
                // and with callback this value set to thisArg.
                InternalTimerCallback::FunctionTimerCallback(
                    function,
                    Rc::new(args.into_boxed_slice()),
                )
            },
        };

        // Step 2. If previousId was given, let id be previousId; otherwise,
        // let id be an implementation-defined integer that is greater than zero
        // and does not already exist in global's map of setTimeout and setInterval IDs.
        let JsTimerHandle(new_handle) = self.next_timer_handle.get();
        let Some(next_handle) = new_handle.checked_add(1) else {
            global.latch_timer_error(DocumentClockError::Overflow);
            return Ok(0);
        };
        self.next_timer_handle.set(JsTimerHandle(next_handle));

        // Step 3. If the surrounding agent's event loop's currently running task
        // is a task that was created by this algorithm, then let nesting level
        // be the task's timer nesting level. Otherwise, let nesting level be 0.
        let mut task = JsTimerTask {
            handle: JsTimerHandle(new_handle),
            source,
            callback,
            is_interval,
            is_user_interacting: ScriptThread::is_user_interacting(),
            nesting_level: 0,
            duration: Duration::ZERO,
        };

        // Step 4. If timeout is less than 0, then set timeout to 0.
        task.duration = timeout.max(Duration::ZERO);

        self.initialize_and_schedule(global, task);

        // Step 15. Return id.
        Ok(new_handle)
    }

    pub(crate) fn clear_timeout_or_interval(&self, global: &GlobalScope, handle: i32) {
        let mut active_timers = self.active_timers.borrow_mut();

        if let Some(entry) = active_timers.remove(&JsTimerHandle(handle)) {
            global.unschedule_callback(entry.oneshot_handle);
        }
    }

    pub(crate) fn set_min_duration(&self, duration: Duration) {
        self.min_duration.set(Some(duration));
    }

    pub(crate) fn remove_min_duration(&self) {
        self.min_duration.set(None);
    }

    // see step 13 of https://html.spec.whatwg.org/multipage/#timer-initialisation-steps
    fn user_agent_pad(&self, current_duration: Duration) -> Duration {
        match self.min_duration.get() {
            Some(min_duration) => min_duration.max(current_duration),
            None => current_duration,
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#timer-initialisation-steps>
    fn initialize_and_schedule(&self, global: &GlobalScope, mut task: JsTimerTask) {
        let handle = task.handle;
        let mut active_timers = self.active_timers.borrow_mut();

        // Step 3. If the surrounding agent's event loop's currently running task
        // is a task that was created by this algorithm, then let nesting level be
        // the task's timer nesting level. Otherwise, let nesting level be 0.
        let nesting_level = self.nesting_level.get();

        let duration = self.user_agent_pad(clamp_duration(nesting_level, task.duration));
        // Step 10. Increment nesting level by one.
        // Step 11. Set task's timer nesting level to nesting level.
        task.nesting_level = nesting_level + 1;

        // Step 13. Set uniqueHandle to the result of running steps after a timeout given global,
        // "setTimeout/setInterval", timeout, and completionStep.
        let callback = OneshotTimerCallback::JsTimer(task);
        let oneshot_handle = global.schedule_callback(callback, duration);

        // Step 14. Set global's map of setTimeout and setInterval IDs[id] to uniqueHandle.
        let entry = active_timers
            .entry(handle)
            .or_insert(JsTimerEntry { oneshot_handle });
        entry.oneshot_handle = oneshot_handle;
    }
}

/// Step 5 of <https://html.spec.whatwg.org/multipage/#timer-initialisation-steps>
fn clamp_duration(nesting_level: u32, unclamped: Duration) -> Duration {
    // Step 5. If nesting level is greater than 5, and timeout is less than 4, then set timeout to 4.
    let lower_bound_ms = if nesting_level > 5 { 4 } else { 0 };
    let lower_bound = Duration::from_millis(lower_bound_ms);
    lower_bound.max(unclamped)
}

impl JsTimerTask {
    // see https://html.spec.whatwg.org/multipage/#timer-initialisation-steps
    fn invoke(self, cx: &mut JSContext, global: &GlobalScope, timers: &JsTimers) {
        // step 9.2 can be ignored, because we proactively prevent execution
        // of this task when its scheduled execution is canceled.

        // prep for step ? in nested set_timeout_or_interval calls
        timers.nesting_level.set(self.nesting_level);

        let _guard = ScriptThread::user_interacting_guard();
        match self.callback {
            InternalTimerCallback::StringTimerCallback(ref code_str, ref fetch_info) => {
                // Step 6.4. Let settings object be global's relevant settings object.
                // Step 6. Let realm be global's relevant realm.

                // Note: the steps to retrieve *fetch options* and *base URL* are performed in
                // `active_script_fetch_info`.
                let InitiatingScriptFetchInfo {
                    fetch_options,
                    base_url,
                } = fetch_info.clone();

                // Step 9.6.8. Let script be the result of creating a classic script given handler,
                // settings object, base URL, and fetch options.
                let script = global.create_a_classic_script(
                    cx,
                    (*code_str.str()).into(),
                    base_url,
                    fetch_options,
                    ErrorReporting::Unmuted,
                    Some(IntroductionType::DOM_TIMER),
                    1,
                    false,
                );

                // Step 9.6.9. Run the classic script script.
                _ = global.run_a_classic_script(cx, script, RethrowErrors::No);
            },
            // Step 9.5. If handler is a Function, then invoke handler given arguments and
            // "report", and with callback this value set to thisArg.
            InternalTimerCallback::FunctionTimerCallback(ref function, ref arguments) => {
                let arguments = self.collect_heap_args(arguments);
                rooted!(&in(cx) let mut value: JSVal);
                let _ = function.Call_(cx, global, arguments, value.handle_mut(), Report);
            },
        };

        // reset nesting level (see above)
        timers.nesting_level.set(0);

        // Step 9.9. If repeat is true, then perform the timer initialization steps again,
        // given global, handler, timeout, arguments, true, and id.
        //
        // Since we choose proactively prevent execution (see 4.1 above), we must only
        // reschedule repeating timers when they were not canceled as part of step 4.2.
        if self.is_interval == IsInterval::Interval &&
            timers.active_timers.borrow().contains_key(&self.handle)
        {
            timers.initialize_and_schedule(global, self);
        }
    }

    fn collect_heap_args<'b>(&self, args: &'b [Heap<JSVal>]) -> Vec<HandleValue<'b>> {
        args.iter().map(|arg| arg.as_handle_value()).collect()
    }
}

/// Describes the source that requested the [`TimerEvent`].
#[derive(Clone, Copy, Debug, Deserialize, MallocSizeOf, Serialize)]
pub enum TimerSource {
    /// The event was requested from a window (`ScriptThread`).
    FromWindow(PipelineId),
    /// The event was requested from a worker (`DedicatedGlobalWorkerScope`).
    FromWorker,
}

/// The id to be used for a [`TimerEvent`] is defined by the corresponding [`TimerEventRequest`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, MallocSizeOf, PartialEq, Serialize)]
pub struct TimerEventId(pub u32);

/// A notification that a timer has fired. [`TimerSource`] must be `FromWindow` when
/// dispatched to `ScriptThread` and must be `FromWorker` when dispatched to a
/// `DedicatedGlobalWorkerScope`
#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct TimerEvent(pub TimerSource, pub TimerEventId);

/// A wrapper between timer events coming in over IPC, and the event-loop.
#[derive(Clone)]
struct TimerListener {
    task_source: SendableTaskSource,
    context: Trusted<GlobalScope>,
    source: TimerSource,
    id: TimerEventId,
}

impl TimerListener {
    /// Handle a timer-event coming from the [`timers::TimerScheduler`]
    /// by queuing the appropriate task on the relevant event-loop.
    /// <https://html.spec.whatwg.org/multipage/#timer-initialisation-steps>
    fn handle(&self, event: TimerEvent) {
        let context = self.context.clone();
        // Step 9. Let task be a task that runs the following substeps:
        self.task_source.queue(task!(timer_event: move |cx| {
                let global = context.root();
                let TimerEvent(source, id) = event;
                match source {
                    TimerSource::FromWorker => {
                        global.downcast::<WorkerGlobalScope>().expect("Window timer delivered to worker");
                    },
                    TimerSource::FromWindow(pipeline) => {
                        assert_eq!(pipeline, global.pipeline_id());
                        global.downcast::<Window>().expect("Worker timer delivered to window");
                    },
                };
                global.fire_timer(id, cx);
            })
        );
    }

    fn into_callback(self) -> BoxedTimerCallback {
        let timer_event = TimerEvent(self.source, self.id);
        Box::new(move || self.handle(timer_event))
    }
}

#[derive(Clone, JSTraceable, MallocSizeOf)]
struct InitiatingScriptFetchInfo {
    fetch_options: ScriptFetchOptions,
    #[no_trace]
    base_url: ServoUrl,
}

#[expect(unsafe_code)]
/// <https://html.spec.whatwg.org/multipage/#timer-initialisation-steps>
fn active_script_fetch_info(cx: &mut JSContext, global: &GlobalScope) -> InitiatingScriptFetchInfo {
    rooted!(&in(cx) let mut value = UndefinedValue());
    unsafe { JS_GetScriptedCallerPrivate(cx, value.handle_mut()) };

    let reference_private = value.handle().into_handle();

    // Step 7. Let initiating script be the active script.
    let initiating_script = unsafe { module_script_from_reference_private(&reference_private) };

    let (fetch_options, base_url) = match initiating_script {
        // Step 9.6.7. If initiating script is not null, then:
        Some(script) => (
            // Step 9.6.7.1. Set fetch options to a script fetch options whose
            ScriptFetchOptions {
                // cryptographic nonce is initiating script's fetch options's cryptographic nonce,
                cryptographic_nonce: script.options.cryptographic_nonce.clone(),
                // integrity metadata is the empty string,
                integrity_metadata: String::new(),
                // parser metadata is "not-parser-inserted",
                parser_metadata: ParserMetadata::NotParserInserted,
                // credentials mode is initiating script's fetch options's credentials mode,
                credentials_mode: script.options.credentials_mode,
                // referrer policy is initiating script's fetch options's referrer policy,
                referrer_policy: script.options.referrer_policy,
                // TODO and fetch priority is "auto".
                render_blocking: false,
            },
            // Step 9.6.7.2. Set base URL to initiating script's base URL.
            script.base_url.clone(),
        ),
        None => (
            // Step 9.6.5. Let fetch options be the default script fetch options.
            ScriptFetchOptions::default_classic_script(),
            // Step 9.6.6. Let base URL be settings object's API base URL.
            global.api_base_url(),
        ),
    };

    InitiatingScriptFetchInfo {
        fetch_options,
        base_url,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};

    use timers::{
        DocumentClockConfiguration, DocumentUnixTime, TimerEventRequest, TimerScheduler,
    };

    use super::*;

    fn controlled_clock(initial_time_ns: u128) -> DocumentClock {
        DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns,
            unix_time_origin_ns: DocumentUnixTime::default(),
        })
    }

    fn test_timer(
        handle: i32,
        deadline: DocumentTime,
        creation_sequence: u64,
    ) -> OneshotTimer {
        OneshotTimer {
            handle: OneshotTimerHandle(handle),
            source: TimerSource::FromWorker,
            callback: OneshotTimerCallback::RunStepsAfterTimeout {
                timer_key: handle,
                ordering_id: DOMString::new(),
                milliseconds: 0,
                completion: Box::new(|_, _| {}),
            },
            scheduled_for: deadline,
            creation_sequence,
        }
    }

    #[test]
    fn controlled_ten_second_dom_deadline_shares_the_outer_scheduler_clock() {
        let clock = controlled_clock(25);
        let timebase = TimerTimebase::default();
        let start = timebase.now(&clock).unwrap();
        let deadline = start.checked_add(Duration::from_secs(10)).unwrap();
        let fired = Arc::new(AtomicBool::new(false));
        let callback_fired = fired.clone();
        let mut scheduler = TimerScheduler::with_clock(clock.clone());
        let id = scheduler
            .try_schedule_timer(TimerEventRequest {
                callback: Box::new(move || {
                    callback_fired.store(true, AtomicOrdering::Relaxed);
                }),
                duration: deadline.checked_duration_since(start).unwrap(),
            })
            .unwrap();
        let snapshot = scheduler.finite_deadline_snapshot().unwrap().unwrap();

        assert_eq!(snapshot.id, id);
        assert_eq!(snapshot.deadline, deadline);
        scheduler.advance_to_and_activate(snapshot).unwrap();
        assert!(fired.load(AtomicOrdering::Relaxed));
        assert_eq!(timebase.now(&clock).unwrap(), deadline);
    }

    #[test]
    fn equal_deadline_timers_run_in_creation_order() {
        let deadline = DocumentTime::from_nanos(10);
        let mut timers = VecDeque::new();
        insert_timer(&mut timers, test_timer(1, deadline, 0));
        insert_timer(&mut timers, test_timer(2, deadline, 1));
        let queues = OrderingQueues::default();

        assert_eq!(
            take_due_timers_for_turn(&mut timers, &queues, deadline, true)
                .pop()
                .unwrap()
                .handle
                .sequence(),
            1
        );
        assert_eq!(
            take_due_timers_for_turn(&mut timers, &queues, deadline, true)
                .pop()
                .unwrap()
                .handle
                .sequence(),
            2
        );
    }

    #[test]
    fn replacing_an_outer_registration_cancels_the_obsolete_timer_id() {
        let clock = controlled_clock(0);
        let mut scheduler = TimerScheduler::with_clock(clock);
        let old_id = scheduler
            .try_schedule_timer(TimerEventRequest {
                callback: Box::new(|| {}),
                duration: Duration::from_secs(10),
            })
            .unwrap();
        let replacement_id = scheduler
            .try_schedule_timer(TimerEventRequest {
                callback: Box::new(|| {}),
                duration: Duration::from_secs(5),
            })
            .unwrap();
        let registered = Cell::new(Some(old_id));

        let obsolete = replace_scheduled_timer_id(&registered, Some(replacement_id)).unwrap();
        scheduler.cancel_timer(obsolete);
        let replacement = scheduler.finite_deadline_snapshot().unwrap().unwrap();
        assert_eq!(replacement.id, replacement_id);
        scheduler.advance_to_and_activate(replacement).unwrap();
        assert_eq!(scheduler.finite_deadline_snapshot().unwrap(), None);
    }

    #[test]
    fn one_due_logical_timer_is_popped_per_turn() {
        let deadline = DocumentTime::from_nanos(10);
        let mut timers = VecDeque::new();
        insert_timer(&mut timers, test_timer(1, deadline, 0));
        insert_timer(&mut timers, test_timer(2, deadline, 1));
        let queues = OrderingQueues::default();

        let first = take_due_timers_for_turn(&mut timers, &queues, deadline, true)
            .pop()
            .unwrap();
        assert_eq!(first.handle.sequence(), 1);
        assert_eq!(timers.len(), 1);
        assert_eq!(timers.back().unwrap().handle.sequence(), 2);
    }

    #[test]
    fn controlled_runsteps_selection_cannot_starve_the_ordering_queue_head() {
        let deadline = DocumentTime::from_nanos(100);
        let ordering_id = DOMString::new();
        let mut timers = VecDeque::new();
        insert_timer(&mut timers, test_timer(1, deadline, 0));
        insert_timer(&mut timers, test_timer(2, deadline, 1));
        let mut queues = OrderingQueues::default();
        queues.insert(
            ordering_id,
            vec![
                OrderingEntry {
                    milliseconds: 10,
                    start_seq: 1,
                    handle: OneshotTimerHandle(2),
                },
                OrderingEntry {
                    milliseconds: 100,
                    start_seq: 0,
                    handle: OneshotTimerHandle(1),
                },
            ],
        );

        let selected = take_due_timers_for_turn(&mut timers, &queues, deadline, true);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].handle.sequence(), 2);
        assert_eq!(timers.back().unwrap().handle.sequence(), 1);
    }

    #[test]
    fn realtime_turn_preserves_the_existing_due_timer_batch() {
        let deadline = DocumentTime::from_nanos(10);
        let mut timers = VecDeque::new();
        insert_timer(&mut timers, test_timer(1, deadline, 0));
        insert_timer(&mut timers, test_timer(2, deadline, 1));

        let due = take_due_timers_for_turn(
            &mut timers,
            &OrderingQueues::default(),
            deadline,
            false,
        );
        assert_eq!(
            due.iter()
                .map(|timer| timer.handle.sequence())
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(timers.is_empty());
    }

    #[test]
    fn metadata_uses_the_checked_physical_deadline_and_typed_callback_kind() {
        let timer = test_timer(1, DocumentTime::from_nanos(20), 7);
        let metadata = timer
            .metadata(
                TimerTimebase {
                    suspended_at: Some(DocumentTime::from_nanos(10)),
                    suspension_offset: Duration::from_nanos(5),
                },
                true,
            )
            .unwrap();

        assert_eq!(metadata.handle.sequence(), 1);
        assert_eq!(metadata.creation_sequence, 7);
        assert_eq!(metadata.deadline, DocumentTime::from_nanos(25));
        assert!(metadata.suspended);
        assert!(metadata.eligible_in_controlled_turn);
        assert_eq!(metadata.kind, DomTimerKind::RunStepsAfterTimeout);

        let overflowing = test_timer(2, DocumentTime::from_nanos(u128::MAX), 8);
        assert_eq!(
            overflowing.metadata(
                TimerTimebase {
                    suspended_at: None,
                    suspension_offset: Duration::from_nanos(1),
                },
                true,
            ),
            Err(DocumentClockError::Overflow)
        );
    }

    #[test]
    fn metadata_marks_an_earlier_blocked_runsteps_timer_before_the_eligible_outer_target() {
        let ordering_id = DOMString::new();
        let mut timers = VecDeque::new();
        insert_timer(
            &mut timers,
            test_timer(1, DocumentTime::from_nanos(100), 0),
        );
        insert_timer(
            &mut timers,
            test_timer(2, DocumentTime::from_nanos(110), 1),
        );
        let mut queues = OrderingQueues::default();
        queues.insert(
            ordering_id,
            vec![
                OrderingEntry {
                    milliseconds: 10,
                    start_seq: 1,
                    handle: OneshotTimerHandle(2),
                },
                OrderingEntry {
                    milliseconds: 100,
                    start_seq: 0,
                    handle: OneshotTimerHandle(1),
                },
            ],
        );

        let metadata = timers
            .iter()
            .rev()
            .map(|timer| {
                timer
                    .metadata(
                        TimerTimebase::default(),
                        runsteps_timer_is_eligible(timer, &queues),
                    )
                    .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(metadata[0].handle.sequence(), 1);
        assert!(!metadata[0].eligible_in_controlled_turn);
        assert_eq!(metadata[1].handle.sequence(), 2);
        assert!(metadata[1].eligible_in_controlled_turn);
        assert_eq!(
            select_timer_for_outer(&timers, &queues, true)
                .unwrap()
                .handle
                .sequence(),
            2
        );
    }

    #[test]
    fn realtime_timebase_preserves_the_shared_host_clock_ordering() {
        let clock = DocumentClock::default();
        let before = clock.try_now().unwrap();
        let observed = TimerTimebase::default().now(&clock).unwrap();
        let after = clock.try_now().unwrap();

        assert!(before <= observed);
        assert!(observed <= after);
        assert_eq!(
            observed
                .checked_add(Duration::from_secs(10))
                .unwrap()
                .checked_duration_since(observed)
                .unwrap(),
            Duration::from_secs(10)
        );
    }

    #[test]
    fn suspension_freezes_logical_dom_time_and_checked_resume_excludes_the_pause() {
        let clock = controlled_clock(10);
        let mut timebase = TimerTimebase::default();
        assert!(timebase.suspend(&clock).unwrap());
        clock.advance_to(DocumentTime::from_nanos(20)).unwrap();
        assert_eq!(timebase.now(&clock).unwrap(), DocumentTime::from_nanos(10));
        assert!(timebase.resume(&clock).unwrap());
        clock.advance_to(DocumentTime::from_nanos(25)).unwrap();
        assert_eq!(timebase.now(&clock).unwrap(), DocumentTime::from_nanos(15));
    }
}
