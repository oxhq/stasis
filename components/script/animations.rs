/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! The set of animations for a document.

use std::cell::Cell;

use cssparser::ToCss;
use embedder_traits::{AnimationState as AnimationsPresentState, UntrustedNodeAddress};
use js::context::NoGC;
use libc::c_void;
use rustc_hash::{FxHashMap, FxHashSet};
use script_bindings::cell::DomRefCell;
use serde::{Deserialize, Serialize};
use servo_base::id::PipelineId;
use servo_constellation_traits::ScriptToConstellationMessage;
use style::animation::{
    Animation, AnimationSetKey, AnimationState, DocumentAnimationSet, ElementAnimationSet,
    KeyframesIterationState, Transition,
};
use style::dom::OpaqueNode;
use style::selector_parser::PseudoElement;

use crate::dom::animationevent::AnimationEvent;
use crate::dom::bindings::codegen::Bindings::AnimationEventBinding::AnimationEventInit;
use crate::dom::bindings::codegen::Bindings::EventBinding::EventInit;
use crate::dom::bindings::codegen::Bindings::TransitionEventBinding::TransitionEventInit;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::bindings::str::DOMString;
use crate::dom::bindings::trace::NoTrace;
use crate::dom::event::Event;
use crate::dom::node::{Node, NodeDamage, NodeTraits, from_untrusted_node_address};
use crate::dom::transitionevent::TransitionEvent;
use crate::dom::window::Window;

/// An active CSS animation or transition shape whose terminal behavior cannot be
/// classified from the retained style animation state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CssAnimationUnsupportedClass {
    /// A CSS animation has a non-finite start/duration or a negative duration.
    InvalidAnimationTiming,
    /// A CSS animation has a non-finite or internally inconsistent iteration state.
    InvalidAnimationIteration,
    /// A CSS transition has a non-finite start/duration or a negative duration.
    InvalidTransitionTiming,
}

/// Per-class counts of active CSS animations and transitions that cannot be
/// classified safely.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CssAnimationUnsupportedCounts {
    pub(crate) invalid_animation_timing: usize,
    pub(crate) invalid_animation_iteration: usize,
    pub(crate) invalid_transition_timing: usize,
}

impl CssAnimationUnsupportedCounts {
    fn increment(&mut self, class: CssAnimationUnsupportedClass) {
        let count = match class {
            CssAnimationUnsupportedClass::InvalidAnimationTiming => {
                &mut self.invalid_animation_timing
            },
            CssAnimationUnsupportedClass::InvalidAnimationIteration => {
                &mut self.invalid_animation_iteration
            },
            CssAnimationUnsupportedClass::InvalidTransitionTiming => {
                &mut self.invalid_transition_timing
            },
        };
        *count = count
            .checked_add(1)
            .expect("a retained animation collection cannot exceed usize::MAX entries");
    }

    pub(crate) fn count(self, class: CssAnimationUnsupportedClass) -> usize {
        match class {
            CssAnimationUnsupportedClass::InvalidAnimationTiming => {
                self.invalid_animation_timing
            },
            CssAnimationUnsupportedClass::InvalidAnimationIteration => {
                self.invalid_animation_iteration
            },
            CssAnimationUnsupportedClass::InvalidTransitionTiming => {
                self.invalid_transition_timing
            },
        }
    }

    pub(crate) fn checked_total(self) -> Option<usize> {
        self.invalid_animation_timing
            .checked_add(self.invalid_animation_iteration)?
            .checked_add(self.invalid_transition_timing)
    }
}

/// A policy-free, copied observation of the CSS animation state retained by one
/// document.
///
/// `infinite_inert` deliberately remains separate from tick-requiring infinite
/// animations: paused, finished, and canceled entries cannot run merely because a
/// rendering opportunity occurs.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CssAnimationPendingObservation {
    pub(crate) pending_event_count: usize,
    pub(crate) finite_pending_or_running: usize,
    pub(crate) infinite_pending_or_running: usize,
    pub(crate) infinite_inert: usize,
    pub(crate) unsupported_pending_or_running: CssAnimationUnsupportedCounts,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CssAnimationPendingClass {
    FinitePendingOrRunning,
    InfinitePendingOrRunning,
    InfiniteInert,
    Inert,
    Unsupported(CssAnimationUnsupportedClass),
}

impl CssAnimationPendingObservation {
    fn record(&mut self, class: CssAnimationPendingClass) {
        let count = match class {
            CssAnimationPendingClass::FinitePendingOrRunning => {
                &mut self.finite_pending_or_running
            },
            CssAnimationPendingClass::InfinitePendingOrRunning => {
                &mut self.infinite_pending_or_running
            },
            CssAnimationPendingClass::InfiniteInert => &mut self.infinite_inert,
            CssAnimationPendingClass::Inert => return,
            CssAnimationPendingClass::Unsupported(class) => {
                self.unsupported_pending_or_running.increment(class);
                return;
            },
        };
        *count = count
            .checked_add(1)
            .expect("a retained animation collection cannot exceed usize::MAX entries");
    }
}

fn animation_state_requires_ticks(state: &AnimationState) -> bool {
    matches!(state, AnimationState::Pending | AnimationState::Running)
}

fn valid_animation_timing(started_at: f64, duration: f64) -> bool {
    started_at.is_finite() && duration.is_finite() && duration >= 0.0
}

fn classify_css_animation(
    state: &AnimationState,
    iteration_state: &KeyframesIterationState,
    started_at: f64,
    duration: f64,
) -> CssAnimationPendingClass {
    let requires_ticks = animation_state_requires_ticks(state);
    match iteration_state {
        KeyframesIterationState::Infinite(_) if !requires_ticks => {
            CssAnimationPendingClass::InfiniteInert
        },
        KeyframesIterationState::Finite(_, _) if !requires_ticks => {
            CssAnimationPendingClass::Inert
        },
        _ if !valid_animation_timing(started_at, duration) => {
            CssAnimationPendingClass::Unsupported(
                CssAnimationUnsupportedClass::InvalidAnimationTiming,
            )
        },
        KeyframesIterationState::Infinite(current) if current.is_finite() && *current >= 0.0 => {
            CssAnimationPendingClass::InfinitePendingOrRunning
        },
        KeyframesIterationState::Infinite(_) => CssAnimationPendingClass::Unsupported(
            CssAnimationUnsupportedClass::InvalidAnimationIteration,
        ),
        KeyframesIterationState::Finite(current, maximum)
            if current.is_finite() &&
                maximum.is_finite() &&
                *current >= 0.0 &&
                *maximum >= 0.0 =>
        {
            CssAnimationPendingClass::FinitePendingOrRunning
        },
        KeyframesIterationState::Finite(_, _) => CssAnimationPendingClass::Unsupported(
            CssAnimationUnsupportedClass::InvalidAnimationIteration,
        ),
    }
}

fn classify_css_transition(
    state: &AnimationState,
    start_time: f64,
    duration: f64,
) -> CssAnimationPendingClass {
    if !animation_state_requires_ticks(state) {
        return CssAnimationPendingClass::Inert;
    }
    if start_time.is_finite() && duration.is_finite() && duration >= 0.0 {
        CssAnimationPendingClass::FinitePendingOrRunning
    } else {
        CssAnimationPendingClass::Unsupported(
            CssAnimationUnsupportedClass::InvalidTransitionTiming,
        )
    }
}

/// The set of animations for a document.
#[derive(Default, JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
pub(crate) struct Animations {
    /// The map of nodes to their animation states.
    #[no_trace]
    pub(crate) sets: DocumentAnimationSet,

    /// Whether or not we have animations that are running.
    has_running_animations: Cell<bool>,

    /// A list of nodes with in-progress CSS transitions or pending events.
    rooted_nodes: DomRefCell<FxHashMap<NoTrace<OpaqueNode>, Dom<Node>>>,

    /// A list of pending animation-related events.
    pending_events: DomRefCell<Vec<TransitionOrAnimationEvent>>,

    /// The timeline value at the last time all animations were marked dirty.
    /// This is used to prevent marking animations dirty when the timeline
    /// has not changed.
    timeline_value_at_last_dirty: Cell<f64>,
}

impl Animations {
    pub(crate) fn new() -> Self {
        Animations {
            sets: Default::default(),
            has_running_animations: Cell::new(false),
            rooted_nodes: Default::default(),
            pending_events: Default::default(),
            timeline_value_at_last_dirty: Cell::new(0.0),
        }
    }

    pub(crate) fn clear(&self) {
        self.sets.sets.write().clear();
        self.rooted_nodes.borrow_mut().clear();
        self.pending_events.borrow_mut().clear();
    }

    // Mark all animations dirty, if they haven't been marked dirty since the
    // specified `current_timeline_value`. Returns true if animations were marked
    // dirty or false otherwise.
    pub(crate) fn mark_animating_nodes_as_dirty(
        &self,
        no_gc: &NoGC,
        current_timeline_value: f64,
    ) -> bool {
        if current_timeline_value <= self.timeline_value_at_last_dirty.get() {
            return false;
        }
        self.timeline_value_at_last_dirty
            .set(current_timeline_value);

        let sets = self.sets.sets.read();
        let rooted_nodes = self.rooted_nodes.borrow();
        for node in sets
            .keys()
            .filter_map(|key| rooted_nodes.get(&NoTrace(key.node)))
        {
            node.dirty(no_gc, NodeDamage::Style);
        }

        true
    }

    pub(crate) fn update_for_new_timeline_value(&self, window: &Window, now: f64) {
        let pipeline_id = window.pipeline_id();
        let mut sets = self.sets.sets.write();

        for (key, set) in sets.iter_mut() {
            self.start_pending_animations(key, set, now, pipeline_id);

            // When necessary, iterate our running animations to the next iteration.
            for animation in set.animations.iter_mut() {
                if animation.iterate_if_necessary(now) {
                    self.add_animation_event(
                        key,
                        animation,
                        TransitionOrAnimationEventType::AnimationIteration,
                        now,
                        pipeline_id,
                    );
                }
            }

            self.finish_running_animations(key, set, now, pipeline_id);
        }

        self.unroot_unused_nodes(&sets);
    }

    /// Cancel animations for the given node, if any exist.
    pub(crate) fn cancel_animations_for_node(&self, node: &Node) {
        let mut animations = self.sets.sets.write();
        let mut cancel_animations_for = |key| {
            if let Some(set) = animations.get_mut(&key) {
                set.cancel_all_animations();
            }
        };

        let opaque_node = node.to_opaque();
        cancel_animations_for(AnimationSetKey::new_for_non_pseudo(opaque_node));
        cancel_animations_for(AnimationSetKey::new_for_pseudo(
            opaque_node,
            PseudoElement::Before,
        ));
        cancel_animations_for(AnimationSetKey::new_for_pseudo(
            opaque_node,
            PseudoElement::After,
        ));
    }

    /// This does three things:
    ///  - Cancel animations for any nodes that are no longer being rendered or delegating rendering.
    ///  - Process any new animations that were discovered after reflow.
    ///  - Collect pending events for any animations that changed state.
    pub(crate) fn do_post_reflow_update(&self, window: &Window, now: f64) {
        let mut sets = self.sets.sets.write();
        {
            let rooted_nodes = self.rooted_nodes.borrow();
            for (key, set) in sets.iter_mut() {
                if rooted_nodes.get(&NoTrace(key.node)).is_some_and(|node| {
                    !node.is_being_rendered_or_delegates_rendering(key.pseudo_element)
                }) {
                    set.cancel_all_animations();
                }
            }
        }

        let pipeline_id = window.pipeline_id();
        self.root_newly_animating_dom_nodes(&sets);

        for (key, set) in sets.iter_mut() {
            self.handle_canceled_animations(key, set, now, pipeline_id);
            self.handle_new_animations(key, set, now, pipeline_id);
        }

        // Remove empty states from our collection of states in order to free
        // up space as soon as we are no longer tracking any animations for
        // a node.
        sets.retain(|_, state| !state.is_empty());
        let have_running_animations = sets.values().any(|state| state.needs_animation_ticks());

        self.update_running_animations_presence(window, have_running_animations);
    }

    fn update_running_animations_presence(&self, window: &Window, new_value: bool) {
        let had_running_animations = self.has_running_animations.get();
        if new_value == had_running_animations {
            return;
        }

        self.has_running_animations.set(new_value);
        self.handle_animation_presence_or_pending_events_change(window);
    }

    fn handle_animation_presence_or_pending_events_change(&self, window: &Window) {
        let has_running_animations = self.has_running_animations.get();
        let has_pending_events = !self.pending_events.borrow().is_empty();

        // Do not send the AnimationCallbacksAbsent state until all pending
        // animation events are delivered.
        let state = match has_running_animations || has_pending_events {
            true => AnimationsPresentState::AnimationsPresent,
            false => AnimationsPresentState::NoAnimationsPresent,
        };
        window.send_to_constellation(ScriptToConstellationMessage::ChangeRunningAnimationsState(
            state,
        ));
    }

    pub(crate) fn running_animation_count(&self) -> usize {
        self.sets
            .sets
            .read()
            .values()
            .map(|state| state.running_animation_and_transition_count())
            .sum()
    }

    /// Copy the retained CSS animation facts without advancing the timeline,
    /// dispatching events, or holding either collection borrow after this call.
    pub(crate) fn pending_observation(&self) -> CssAnimationPendingObservation {
        let mut observation = CssAnimationPendingObservation::default();
        {
            let sets = self.sets.sets.read();
            for set in sets.values() {
                for animation in &set.animations {
                    observation.record(classify_css_animation(
                        &animation.state,
                        &animation.iteration_state,
                        animation.started_at,
                        animation.duration,
                    ));
                }
                for transition in &set.transitions {
                    observation.record(classify_css_transition(
                        &transition.state,
                        transition.start_time,
                        transition.property_animation.duration,
                    ));
                }
            }
        }
        observation.pending_event_count = self.pending_events.borrow().len();
        observation
    }

    /// Walk through the list of pending animations and start all of the ones that
    /// have left the delay phase.
    fn start_pending_animations(
        &self,
        key: &AnimationSetKey,
        set: &mut ElementAnimationSet,
        now: f64,
        pipeline_id: PipelineId,
    ) {
        for animation in set.animations.iter_mut() {
            if animation.state == AnimationState::Pending && animation.started_at <= now {
                animation.state = AnimationState::Running;
                self.add_animation_event(
                    key,
                    animation,
                    TransitionOrAnimationEventType::AnimationStart,
                    now,
                    pipeline_id,
                );
            }
        }

        for transition in set.transitions.iter_mut() {
            if transition.state == AnimationState::Pending && transition.start_time <= now {
                transition.state = AnimationState::Running;
                self.add_transition_event(
                    key,
                    transition,
                    TransitionOrAnimationEventType::TransitionStart,
                    now,
                    pipeline_id,
                );
            }
        }
    }

    /// Walk through the list of running animations and remove all of the ones that
    /// have ended.
    fn finish_running_animations(
        &self,
        key: &AnimationSetKey,
        set: &mut ElementAnimationSet,
        now: f64,
        pipeline_id: PipelineId,
    ) {
        for animation in set.animations.iter_mut() {
            if animation.state == AnimationState::Running && animation.has_ended(now) {
                animation.state = AnimationState::Finished;
                self.add_animation_event(
                    key,
                    animation,
                    TransitionOrAnimationEventType::AnimationEnd,
                    now,
                    pipeline_id,
                );
            }
        }

        for transition in set.transitions.iter_mut() {
            if transition.state == AnimationState::Running && transition.has_ended(now) {
                transition.state = AnimationState::Finished;
                self.add_transition_event(
                    key,
                    transition,
                    TransitionOrAnimationEventType::TransitionEnd,
                    now,
                    pipeline_id,
                );
            }
        }
    }

    /// Send events for canceled animations. Currently this only handles canceled
    /// transitions, but eventually this should handle canceled CSS animations as
    /// well.
    fn handle_canceled_animations(
        &self,
        key: &AnimationSetKey,
        set: &mut ElementAnimationSet,
        now: f64,
        pipeline_id: PipelineId,
    ) {
        for transition in &set.transitions {
            if transition.state == AnimationState::Canceled {
                self.add_transition_event(
                    key,
                    transition,
                    TransitionOrAnimationEventType::TransitionCancel,
                    now,
                    pipeline_id,
                );
            }
        }

        for animation in &set.animations {
            if animation.state == AnimationState::Canceled {
                self.add_animation_event(
                    key,
                    animation,
                    TransitionOrAnimationEventType::AnimationCancel,
                    now,
                    pipeline_id,
                );
            }
        }

        set.clear_canceled_animations();
    }

    fn handle_new_animations(
        &self,
        key: &AnimationSetKey,
        set: &mut ElementAnimationSet,
        now: f64,
        pipeline_id: PipelineId,
    ) {
        for animation in set.animations.iter_mut() {
            animation.is_new = false;
        }

        for transition in set.transitions.iter_mut() {
            if transition.is_new {
                self.add_transition_event(
                    key,
                    transition,
                    TransitionOrAnimationEventType::TransitionRun,
                    now,
                    pipeline_id,
                );
                transition.is_new = false;
            }
        }
    }

    /// Ensure that all nodes with new animations are rooted. This should be called
    /// immediately after a restyle, to ensure that these addresses are still valid.
    #[expect(unsafe_code)]
    fn root_newly_animating_dom_nodes(
        &self,
        sets: &FxHashMap<AnimationSetKey, ElementAnimationSet>,
    ) {
        let mut rooted_nodes = self.rooted_nodes.borrow_mut();
        for (key, set) in sets.iter() {
            let opaque_node = key.node;
            if rooted_nodes.contains_key(&NoTrace(opaque_node)) {
                continue;
            }

            if set.animations.iter().any(|animation| animation.is_new) ||
                set.transitions.iter().any(|transition| transition.is_new)
            {
                let address = UntrustedNodeAddress(opaque_node.0 as *const c_void);
                unsafe {
                    rooted_nodes.insert(
                        NoTrace(opaque_node),
                        Dom::from_ref(&*from_untrusted_node_address(address)),
                    )
                };
            }
        }
    }

    // Unroot any nodes that we have rooted but are no longer tracking animations for.
    fn unroot_unused_nodes(&self, sets: &FxHashMap<AnimationSetKey, ElementAnimationSet>) {
        let pending_events = self.pending_events.borrow();
        let nodes: FxHashSet<OpaqueNode> = sets.keys().map(|key| key.node).collect();
        self.rooted_nodes.borrow_mut().retain(|node, _| {
            nodes.contains(&node.0) || pending_events.iter().any(|event| event.node == node.0)
        });
    }

    fn add_transition_event(
        &self,
        key: &AnimationSetKey,
        transition: &Transition,
        event_type: TransitionOrAnimationEventType,
        now: f64,
        pipeline_id: PipelineId,
    ) {
        // Calculate the `elapsed-time` property of the event and take the absolute
        // value to prevent -0 values.
        let elapsed_time = match event_type {
            TransitionOrAnimationEventType::TransitionRun |
            TransitionOrAnimationEventType::TransitionStart => transition
                .property_animation
                .duration
                .min((-transition.delay).max(0.)),
            TransitionOrAnimationEventType::TransitionEnd => transition.property_animation.duration,
            TransitionOrAnimationEventType::TransitionCancel => {
                (now - transition.start_time).max(0.)
            },
            _ => unreachable!(),
        }
        .abs();

        self.pending_events
            .borrow_mut()
            .push(TransitionOrAnimationEvent {
                pipeline_id,
                event_type,
                node: key.node,
                pseudo_element: key.pseudo_element,
                property_or_animation_name: transition
                    .property_animation
                    .property_id()
                    .name()
                    .into(),
                elapsed_time,
            });
    }

    fn add_animation_event(
        &self,
        key: &AnimationSetKey,
        animation: &Animation,
        event_type: TransitionOrAnimationEventType,
        now: f64,
        pipeline_id: PipelineId,
    ) {
        let iteration_index = match animation.iteration_state {
            KeyframesIterationState::Finite(current, _) |
            KeyframesIterationState::Infinite(current) => current,
        };

        let active_duration = match animation.iteration_state {
            KeyframesIterationState::Finite(_, max) => max * animation.duration,
            KeyframesIterationState::Infinite(_) => f64::MAX,
        };

        // Calculate the `elapsed-time` property of the event and take the absolute
        // value to prevent -0 values.
        let elapsed_time = match event_type {
            TransitionOrAnimationEventType::AnimationStart => {
                (-animation.delay).max(0.).min(active_duration)
            },
            TransitionOrAnimationEventType::AnimationIteration => {
                iteration_index * animation.duration
            },
            TransitionOrAnimationEventType::AnimationEnd => {
                (iteration_index * animation.duration) + animation.current_iteration_duration()
            },
            TransitionOrAnimationEventType::AnimationCancel => {
                (iteration_index * animation.duration) + (now - animation.started_at).max(0.)
            },
            _ => unreachable!(),
        }
        .abs();

        self.pending_events
            .borrow_mut()
            .push(TransitionOrAnimationEvent {
                pipeline_id,
                event_type,
                node: key.node,
                pseudo_element: key.pseudo_element,
                property_or_animation_name: animation.name.to_string(),
                elapsed_time,
            });
    }

    /// An implementation of the final steps of
    /// <https://drafts.csswg.org/web-animations-1/#update-animations-and-send-events>.
    pub(crate) fn send_pending_events(&self, window: &Window, cx: &mut js::context::JSContext) {
        // > 4. Let events to dispatch be a copy of doc’s pending animation event queue.
        // > 5. Clear doc’s pending animation event queue.
        //
        // Take all of the events here, in case sending one of these events
        // triggers adding new events by forcing a layout.
        let events = std::mem::take(&mut *self.pending_events.safe_borrow_mut(cx.no_gc()));
        if events.is_empty() {
            return;
        }

        // > 6. Perform a stable sort of the animation events in events to dispatch as follows:
        // >    1. Sort the events by their scheduled event time such that events that were
        // >       scheduled to occur earlier sort before events scheduled to occur later, and
        // >       events whose scheduled event time is unresolved sort before events with a
        // >       resolved scheduled event time.
        // >    2. Within events with equal scheduled event times, sort by their composite
        // >       order.
        //
        // TODO: Sorting of animation events isn't done yet.

        // 7. Dispatch each of the events in events to dispatch at their corresponding
        // target using the order established in the previous step.
        for event in events.into_iter() {
            // We root the node here to ensure that sending this event doesn't
            // unroot it as a side-effect.
            let node = match self.rooted_nodes.borrow().get(&NoTrace(event.node)) {
                Some(node) => DomRoot::from_ref(&**node),
                None => {
                    warn!("Tried to send an event for an unrooted node");
                    continue;
                },
            };

            let event_atom = match event.event_type {
                TransitionOrAnimationEventType::AnimationEnd => atom!("animationend"),
                TransitionOrAnimationEventType::AnimationStart => atom!("animationstart"),
                TransitionOrAnimationEventType::AnimationCancel => atom!("animationcancel"),
                TransitionOrAnimationEventType::AnimationIteration => atom!("animationiteration"),
                TransitionOrAnimationEventType::TransitionCancel => atom!("transitioncancel"),
                TransitionOrAnimationEventType::TransitionEnd => atom!("transitionend"),
                TransitionOrAnimationEventType::TransitionRun => atom!("transitionrun"),
                TransitionOrAnimationEventType::TransitionStart => atom!("transitionstart"),
            };
            let parent = EventInit {
                bubbles: true,
                cancelable: false,
                composed: false,
            };

            let property_or_animation_name =
                DOMString::from(event.property_or_animation_name.clone());
            let pseudo_element = event
                .pseudo_element
                .map_or_else(DOMString::new, |pseudo_element| {
                    DOMString::from(pseudo_element.to_css_string())
                });
            let elapsed_time = Finite::new(event.elapsed_time as f32).unwrap();
            let window = node.owner_window();

            if event.event_type.is_transition_event() {
                let event_init = TransitionEventInit {
                    parent,
                    propertyName: property_or_animation_name,
                    elapsedTime: elapsed_time,
                    pseudoElement: pseudo_element,
                };
                TransitionEvent::new(cx, &window, event_atom, &event_init)
                    .upcast::<Event>()
                    .fire(cx, node.upcast());
            } else {
                let event_init = AnimationEventInit {
                    parent,
                    animationName: property_or_animation_name,
                    elapsedTime: elapsed_time,
                    pseudoElement: pseudo_element,
                };
                AnimationEvent::new(cx, &window, event_atom, &event_init)
                    .upcast::<Event>()
                    .fire(cx, node.upcast());
            }
        }

        if self.pending_events.borrow().is_empty() {
            self.handle_animation_presence_or_pending_events_change(window);
        }
    }
}

/// The type of transition event to trigger. These are defined by
/// CSS Transitions § 6.1 and CSS Animations § 4.2
#[derive(Clone, Debug, Deserialize, JSTraceable, MallocSizeOf, Serialize)]
pub(crate) enum TransitionOrAnimationEventType {
    /// "The transitionrun event occurs when a transition is created (i.e., when it
    /// is added to the set of running transitions)."
    TransitionRun,
    /// "The transitionstart event occurs when a transition’s delay phase ends."
    TransitionStart,
    /// "The transitionend event occurs at the completion of the transition. In the
    /// case where a transition is removed before completion, such as if the
    /// transition-property is removed, then the event will not fire."
    TransitionEnd,
    /// "The transitioncancel event occurs when a transition is canceled."
    TransitionCancel,
    /// "The animationstart event occurs at the start of the animation. If there is
    /// an animation-delay then this event will fire once the delay period has expired."
    AnimationStart,
    /// "The animationiteration event occurs at the end of each iteration of an
    /// animation, except when an animationend event would fire at the same time."
    AnimationIteration,
    /// "The animationend event occurs when the animation finishes"
    AnimationEnd,
    /// "The animationcancel event occurs when the animation stops running in a way
    /// that does not fire an animationend event..."
    AnimationCancel,
}

impl TransitionOrAnimationEventType {
    /// Whether or not this event is a transition-related event.
    pub(crate) fn is_transition_event(&self) -> bool {
        match *self {
            Self::TransitionRun |
            Self::TransitionEnd |
            Self::TransitionCancel |
            Self::TransitionStart => true,
            Self::AnimationEnd |
            Self::AnimationIteration |
            Self::AnimationStart |
            Self::AnimationCancel => false,
        }
    }
}

#[derive(Deserialize, JSTraceable, MallocSizeOf, Serialize)]
/// A transition or animation event.
pub(crate) struct TransitionOrAnimationEvent {
    /// The pipeline id of the layout task that sent this message.
    #[no_trace]
    pub(crate) pipeline_id: PipelineId,
    /// The type of transition event this should trigger.
    pub(crate) event_type: TransitionOrAnimationEventType,
    /// The address of the node which owns this transition.
    #[no_trace]
    pub(crate) node: OpaqueNode,
    /// The pseudo element for this transition or animation, if applicable.
    #[no_trace]
    pub(crate) pseudo_element: Option<PseudoElement>,
    /// The name of the property that is transitioning (in the case of a transition)
    /// or the name of the animation (in the case of an animation).
    pub(crate) property_or_animation_name: String,
    /// The elapsed time property to send with this transition event.
    pub(crate) elapsed_time: f64,
}

#[cfg(test)]
mod pending_observation_tests {
    use super::*;

    #[test]
    fn active_finite_and_infinite_animations_are_distinct_from_inert_infinite_entries() {
        assert_eq!(
            classify_css_animation(
                &AnimationState::Pending,
                &KeyframesIterationState::Finite(0.0, 2.5),
                -10.0,
                20.0,
            ),
            CssAnimationPendingClass::FinitePendingOrRunning
        );
        assert_eq!(
            classify_css_animation(
                &AnimationState::Running,
                &KeyframesIterationState::Infinite(4.0),
                10.0,
                20.0,
            ),
            CssAnimationPendingClass::InfinitePendingOrRunning
        );

        for state in [
            AnimationState::Paused(0.25),
            AnimationState::Finished,
            AnimationState::Canceled,
        ] {
            assert_eq!(
                classify_css_animation(
                    &state,
                    &KeyframesIterationState::Infinite(4.0),
                    10.0,
                    20.0,
                ),
                CssAnimationPendingClass::InfiniteInert
            );
        }
    }

    #[test]
    fn malformed_active_entries_keep_typed_unsupported_reasons() {
        let animation_timing = classify_css_animation(
            &AnimationState::Running,
            &KeyframesIterationState::Finite(0.0, 1.0),
            f64::NAN,
            20.0,
        );
        let animation_iteration = classify_css_animation(
            &AnimationState::Running,
            &KeyframesIterationState::Finite(-1.0, 1.0),
            10.0,
            20.0,
        );
        let transition_timing =
            classify_css_transition(&AnimationState::Pending, 10.0, f64::INFINITY);

        let mut observation = CssAnimationPendingObservation::default();
        observation.record(animation_timing);
        observation.record(animation_iteration);
        observation.record(transition_timing);

        assert_eq!(
            observation
                .unsupported_pending_or_running
                .count(CssAnimationUnsupportedClass::InvalidAnimationTiming),
            1
        );
        assert_eq!(
            observation
                .unsupported_pending_or_running
                .count(CssAnimationUnsupportedClass::InvalidAnimationIteration),
            1
        );
        assert_eq!(
            observation
                .unsupported_pending_or_running
                .count(CssAnimationUnsupportedClass::InvalidTransitionTiming),
            1
        );
        assert_eq!(
            observation
                .unsupported_pending_or_running
                .checked_total(),
            Some(3)
        );
    }

    #[test]
    fn paused_or_finished_finite_entries_and_transitions_are_inert() {
        assert_eq!(
            classify_css_animation(
                &AnimationState::Paused(0.5),
                &KeyframesIterationState::Finite(0.0, 1.0),
                f64::NAN,
                f64::NAN,
            ),
            CssAnimationPendingClass::Inert
        );
        assert_eq!(
            classify_css_transition(&AnimationState::Finished, f64::NAN, f64::NAN),
            CssAnimationPendingClass::Inert
        );
    }

    #[test]
    fn lowering_a_running_animation_iteration_limit_remains_finite() {
        // Stylo deliberately preserves the completed-iteration counter when a style
        // change lowers the maximum. The next tick deterministically finishes it.
        assert_eq!(
            classify_css_animation(
                &AnimationState::Running,
                &KeyframesIterationState::Finite(4.0, 1.0),
                10.0,
                20.0,
            ),
            CssAnimationPendingClass::FinitePendingOrRunning
        );
    }
}
