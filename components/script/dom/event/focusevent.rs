/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use embedder_traits::{DocumentControlProfile, DocumentExecutionProfile};
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::reflector::reflect_dom_object_with_proto;
use style::Atom;

use crate::dom::bindings::codegen::Bindings::FocusEventBinding;
use crate::dom::bindings::codegen::Bindings::FocusEventBinding::FocusEventMethods;
use crate::dom::bindings::codegen::Bindings::UIEventBinding::UIEventMethods;
use crate::dom::bindings::codegen::Bindings::WindowBinding::WindowMethods;
use crate::dom::bindings::error::Fallible;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::event::{Event, EventBubbles, EventCancelable};
use crate::dom::eventtarget::EventTarget;
use crate::dom::performance::performanceentry::PerformanceEntryTime;
use crate::dom::uievent::UIEvent;
use crate::dom::window::Window;
use crate::event_loop::script_thread::ScriptThread;

/// The type of a [`FocusEvent`].
pub(crate) enum FocusEventType {
    /// Element gained focus. Doesn't bubble.
    Focus,
    /// Element lost focus. Doesn't bubble.
    Blur,
    /// Element gained focus. Bubbles.
    FocusIn,
    /// Element lost focus. Bubbles.
    FocusOut,
}

#[dom_struct]
pub(crate) struct FocusEvent {
    uievent: UIEvent,
}

impl FocusEvent {
    fn new_inherited() -> FocusEvent {
        FocusEvent {
            uievent: UIEvent::new_inherited(),
        }
    }

    pub(crate) fn new_uninitialized(cx: &mut JSContext, window: &Window) -> DomRoot<FocusEvent> {
        Self::new_uninitialized_with_proto(cx, window, None)
    }

    pub(crate) fn new_uninitialized_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> DomRoot<FocusEvent> {
        reflect_dom_object_with_proto(cx, Box::new(FocusEvent::new_inherited()), window, proto)
    }

    #[expect(clippy::too_many_arguments)]
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        event_type: Atom,
        can_bubble: EventBubbles,
        cancelable: EventCancelable,
        view: Option<&Window>,
        detail: i32,
        related_target: Option<&EventTarget>,
    ) -> DomRoot<FocusEvent> {
        let event = Self::new_with_proto(
            cx,
            window,
            None,
            event_type,
            can_bubble,
            cancelable,
            view,
            detail,
            related_target,
        );
        if ScriptThread::current_document_control_profile()
            == DocumentControlProfile::TopLevelSession
            && ScriptThread::current_document_execution_profile()
                == DocumentExecutionProfile::ControlledWebSessionV2
            && window.is_top_level()
            && let Ok(time_stamp @ PerformanceEntryTime::Document(_)) =
                window.Performance(cx).current_entry_time()
        {
            // Engine-generated focus transitions are same-event-loop work in the controlled
            // top-level document. Stamp them at creation from that document clock so React's
            // Event.timeStamp observation cannot import a host monotonic instant. Script-created
            // FocusEvent objects and every predecessor profile retain the explicit host boundary.
            event
                .upcast::<Event>()
                .set_creation_time_stamp(time_stamp);
        }
        event
    }

    #[expect(clippy::too_many_arguments)]
    fn new_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        event_type: Atom,
        can_bubble: EventBubbles,
        cancelable: EventCancelable,
        view: Option<&Window>,
        detail: i32,
        related_target: Option<&EventTarget>,
    ) -> DomRoot<FocusEvent> {
        let ev = FocusEvent::new_uninitialized_with_proto(cx, window, proto);
        ev.upcast::<UIEvent>().init_event(
            event_type,
            bool::from(can_bubble),
            bool::from(cancelable),
            view,
            detail,
        );
        ev.upcast::<Event>().set_related_target(related_target);
        ev
    }
}

impl FocusEventMethods<crate::DomTypeHolder> for FocusEvent {
    /// <https://w3c.github.io/uievents/#dom-focusevent-focusevent>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        event_type: DOMString,
        init: &FocusEventBinding::FocusEventInit,
    ) -> Fallible<DomRoot<FocusEvent>> {
        let bubbles = EventBubbles::from(init.parent.parent.bubbles);
        let cancelable = EventCancelable::from(init.parent.parent.cancelable);
        let event = FocusEvent::new_with_proto(
            cx,
            window,
            proto,
            event_type.into(),
            bubbles,
            cancelable,
            init.parent.view.as_deref(),
            init.parent.detail,
            init.relatedTarget.as_deref(),
        );
        event
            .upcast::<Event>()
            .set_composed(init.parent.parent.composed);
        Ok(event)
    }

    /// <https://w3c.github.io/uievents/#widl-FocusEvent-relatedTarget>
    fn GetRelatedTarget(&self) -> Option<DomRoot<EventTarget>> {
        self.upcast::<Event>().related_target()
    }

    /// <https://dom.spec.whatwg.org/#dom-event-istrusted>
    fn IsTrusted(&self) -> bool {
        self.uievent.IsTrusted()
    }
}
