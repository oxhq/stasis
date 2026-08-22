/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use std::cell::{Cell, RefCell};
use std::ptr;
use std::rc::Rc;

use dom_struct::dom_struct;
use js::context::JSContext;
use js::conversions::ToJSValConvertible;
use js::jsapi::{Heap, JSObject};
use js::jsval::UndefinedValue;
use js::rust::wrappers2::JS_NewObject;
use js::rust::{CustomAutoRooter, CustomAutoRooterGuard, HandleValue};
use rustc_hash::FxHashMap;
use script_bindings::reflector::reflect_weak_referenceable_dom_object;
use servo_base::id::{MessagePortId, MessagePortIndex};
use servo_constellation_traits::{MessagePortImpl, PortMessageTask, ScriptToConstellationMessage};

use crate::dom::bindings::codegen::Bindings::EventHandlerBinding::EventHandlerNonNull;
use crate::dom::bindings::codegen::Bindings::MessagePortBinding::{
    MessagePortMethods, StructuredSerializeOptions,
};
use crate::dom::bindings::conversions::root_from_object;
use crate::dom::bindings::error::{Error, ErrorResult, ErrorToJsval, Fallible};
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::structuredclone::{self, StructuredData};
use crate::dom::bindings::trace::RootedTraceableBox;
use crate::dom::bindings::transferable::Transferable;
use crate::dom::bindings::utils::set_dictionary_property;
use crate::dom::eventtarget::EventTarget;
use crate::dom::globalscope::GlobalScope;

#[dom_struct]
/// The MessagePort used in the DOM.
pub(crate) struct MessagePort {
    eventtarget: EventTarget,
    #[no_trace]
    message_port_id: MessagePortId,
    #[no_trace]
    entangled_port: RefCell<Option<MessagePortId>>,
    detached: Cell<bool>,
}

impl MessagePort {
    fn new_inherited(message_port_id: MessagePortId) -> MessagePort {
        MessagePort {
            eventtarget: EventTarget::new_inherited(),
            entangled_port: RefCell::new(None),
            detached: Cell::new(false),
            message_port_id,
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#create-a-new-messageport-object>
    pub(crate) fn new(cx: &mut JSContext, owner: &GlobalScope) -> DomRoot<MessagePort> {
        let port_id = MessagePortId::new();
        reflect_weak_referenceable_dom_object(
            cx,
            Rc::new(MessagePort::new_inherited(port_id)),
            owner,
        )
    }

    /// Create a new port for an incoming transfer-received one.
    pub(crate) fn new_transferred(
        cx: &mut JSContext,
        owner: &GlobalScope,
        transferred_port: MessagePortId,
        entangled_port: Option<MessagePortId>,
    ) -> DomRoot<MessagePort> {
        reflect_weak_referenceable_dom_object(
            cx,
            Rc::new(MessagePort {
                message_port_id: transferred_port,
                eventtarget: EventTarget::new_inherited(),
                detached: Cell::new(false),
                entangled_port: RefCell::new(entangled_port),
            }),
            owner,
        )
    }

    /// <https://html.spec.whatwg.org/multipage/#entangle>
    pub(crate) fn entangle(&self, other_id: MessagePortId) {
        *self.entangled_port.borrow_mut() = Some(other_id);
    }

    /// <https://html.spec.whatwg.org/multipage/#disentangle>
    pub(crate) fn disentangle(&self) -> Option<MessagePortId> {
        // Disentangle initiatorPort and otherPort, so that they are no longer entangled or associated with each other.
        // Note: called from `disentangle_port` in the global, where the rest happens.
        self.entangled_port.borrow_mut().take()
    }

    /// Has the port been disentangled?
    /// Used when starting the port to fire the `close` event,
    /// to cover the case of a disentanglement while in transfer.
    pub(crate) fn disentangled(&self) -> bool {
        self.entangled_port.borrow().is_none()
    }

    pub(crate) fn message_port_id(&self) -> &MessagePortId {
        &self.message_port_id
    }

    /// <https://html.spec.whatwg.org/multipage/#handler-messageport-onmessage>
    fn set_onmessage(&self, cx: &mut JSContext, listener: Option<Rc<EventHandlerNonNull>>) {
        let eventtarget = self.upcast::<EventTarget>();
        eventtarget.set_event_handler_common(cx, "message", listener);
    }

    /// <https://html.spec.whatwg.org/multipage/#message-port-post-message-steps>
    #[expect(unsafe_code)]
    fn post_message_impl(
        &self,
        cx: &mut JSContext,
        message: HandleValue,
        transfer: CustomAutoRooterGuard<Vec<*mut JSObject>>,
    ) -> ErrorResult {
        if self.detached.get() {
            return Ok(());
        }

        // Step 1 is the transfer argument.

        let target_port = self.entangled_port.borrow();

        // Step 3
        let mut doomed = false;

        let ports = transfer
            .iter()
            .filter_map(|&obj| unsafe { root_from_object::<MessagePort>(cx, obj).ok() });
        for port in ports {
            // Step 2
            if port.message_port_id() == self.message_port_id() {
                return Err(Error::DataClone(None));
            }

            // Step 4
            if let Some(target_id) = target_port.as_ref() &&
                port.message_port_id() == target_id
            {
                doomed = true;
            }
        }

        // Step 5
        let data = structuredclone::write(cx, message, Some(transfer))?;

        if doomed {
            // TODO: The spec says to optionally report such a case to a dev console.
            return Ok(());
        }

        // Step 6, done in MessagePortImpl.

        let incumbent = match GlobalScope::incumbent() {
            None => unreachable!("postMessage called with no incumbent global"),
            Some(incumbent) => incumbent,
        };

        // Step 7
        let task = PortMessageTask {
            origin: incumbent.origin().immutable().clone(),
            data,
        };

        // Have the global proxy this call to the corresponding MessagePortImpl.
        self.global()
            .post_messageport_msg(*self.message_port_id(), task);
        Ok(())
    }

    /// <https://streams.spec.whatwg.org/#abstract-opdef-crossrealmtransformsenderror>
    pub(crate) fn cross_realm_transform_send_error(&self, cx: &mut JSContext, error: HandleValue) {
        // Perform PackAndPostMessage(port, "error", error),
        // discarding the result.
        let _ = self.pack_and_post_message(cx, "error", error);
    }

    /// <https://streams.spec.whatwg.org/#abstract-opdef-packandpostmessagehandlingerror>
    pub(crate) fn pack_and_post_message_handling_error(
        &self,
        cx: &mut JSContext,
        type_: &str,
        value: HandleValue,
    ) -> ErrorResult {
        // Let result be PackAndPostMessage(port, type, value).
        let result = self.pack_and_post_message(cx, type_, value);

        // If result is an abrupt completion,
        if let Err(error) = result.as_ref() {
            // Perform ! CrossRealmTransformSendError(port, result.[[Value]]).
            rooted!(&in(cx) let mut rooted_error = UndefinedValue());
            error
                .clone()
                .to_jsval(cx, &self.global(), rooted_error.handle_mut());
            self.cross_realm_transform_send_error(cx, rooted_error.handle());
        }

        result
    }

    /// <https://streams.spec.whatwg.org/#abstract-opdef-packandpostmessage>
    #[expect(unsafe_code)]
    pub(crate) fn pack_and_post_message(
        &self,
        cx: &mut JSContext,
        type_: &str,
        value: HandleValue,
    ) -> ErrorResult {
        // Let message be OrdinaryObjectCreate(null).
        rooted!(&in(cx) let mut message = unsafe { JS_NewObject(cx, ptr::null()) });
        rooted!(&in(cx) let mut type_string = UndefinedValue());
        type_.safe_to_jsval(cx, type_string.handle_mut());

        // Perform ! CreateDataProperty(message, "type", type).
        set_dictionary_property(cx, message.handle(), c"type", type_string.handle())
            .expect("Setting the message type should not fail.");

        // Perform ! CreateDataProperty(message, "value", value).
        set_dictionary_property(cx, message.handle(), c"value", value)
            .expect("Setting the message value should not fail.");

        // Let targetPort be the port with which port is entangled, if any; otherwise let it be null.
        // Done in `global.post_messageport_msg`.

        // Let options be «[ "transfer" → « » ]».
        let mut rooted = CustomAutoRooter::new(vec![]);
        let transfer = unsafe { CustomAutoRooterGuard::new(cx.raw_cx(), &mut rooted) };

        // Run the message port post message steps providing targetPort, message, and options.
        rooted!(&in(cx) let mut message_val = UndefinedValue());
        message.safe_to_jsval(cx, message_val.handle_mut());
        self.post_message_impl(cx, message_val.handle(), transfer)
    }
}

impl Transferable for MessagePort {
    type Index = MessagePortIndex;
    type Data = MessagePortImpl;

    /// <https://html.spec.whatwg.org/multipage/#message-ports:transfer-steps>
    fn transfer(
        &self,
        _cx: &mut js::context::JSContext,
    ) -> Fallible<(MessagePortId, MessagePortImpl)> {
        // <https://html.spec.whatwg.org/multipage/#structuredserializewithtransfer>
        // Step 5.2. If transferable has a [[Detached]] internal slot and
        // transferable.[[Detached]] is true, then throw a "DataCloneError"
        // DOMException.
        if self.detached.get() {
            return Err(Error::DataClone(None));
        }

        self.global().require_external_subscription()?;

        self.detached.set(true);
        let id = self.message_port_id();

        // 1. Run local transfer logic, and return the object to be transferred.
        let transferred_port = self.global().mark_port_as_transferred(id);

        Ok((*id, transferred_port))
    }

    /// <https://html.spec.whatwg.org/multipage/#message-ports:transfer-receiving-steps>
    fn transfer_receive(
        cx: &mut js::context::JSContext,
        owner: &GlobalScope,
        id: MessagePortId,
        port_impl: MessagePortImpl,
    ) -> Fallible<DomRoot<Self>> {
        require_transfer_receive_admission(owner, [(id, &port_impl)])?;

        let transferred_port =
            MessagePort::new_transferred(cx, owner, id, port_impl.entangled_port_id());
        owner.track_message_port(&transferred_port, Some(port_impl));
        Ok(transferred_port)
    }

    fn serialized_storage<'a>(
        data: StructuredData<'a, '_>,
    ) -> &'a mut Option<FxHashMap<MessagePortId, Self::Data>> {
        match data {
            StructuredData::Reader(r) => &mut r.port_impls,
            StructuredData::Writer(w) => &mut w.ports,
        }
    }
}

/// Require admission for incoming transfer data that owns one or more in-flight ports.
///
/// The sender detached each port and marked it as transferred before the receiver reached these
/// steps. If this realm cannot admit the asynchronous subscription, every owned port must still be
/// disposed so the constellation cannot retain it in `TransferInProgress` indefinitely.
pub(crate) fn require_transfer_receive_admission<const N: usize>(
    owner: &GlobalScope,
    ports: [(MessagePortId, &MessagePortImpl); N],
) -> Fallible<()> {
    if let Err(error) = owner.require_external_subscription() {
        send_rejected_transfer_cleanup_messages(owner, rejected_transfer_cleanup_messages(ports));
        return Err(error);
    }
    Ok(())
}

pub(crate) fn send_rejected_transfer_cleanup_messages(
    owner: &GlobalScope,
    messages: impl IntoIterator<Item = ScriptToConstellationMessage>,
) {
    for message in messages {
        // Continue through the entire owned set even if the constellation channel is already gone.
        let _ = owner.script_to_constellation_chan().send(message);
    }
}

fn rejected_transfer_cleanup_messages<const N: usize>(
    ports: [(MessagePortId, &MessagePortImpl); N],
) -> [ScriptToConstellationMessage; N] {
    ports.map(|(id, port_impl)| rejected_transfer_cleanup_message(id, port_impl))
}

pub(crate) fn rejected_transfer_cleanup_message(
    id: MessagePortId,
    port_impl: &MessagePortImpl,
) -> ScriptToConstellationMessage {
    ScriptToConstellationMessage::DisentanglePorts(id, port_impl.entangled_port_id())
}

#[cfg(test)]
mod transfer_tests {
    use servo_base::id::{Index, MessagePortId, MessagePortIndex, PipelineNamespaceId};
    use servo_constellation_traits::{MessagePortImpl, ScriptToConstellationMessage};

    use super::{rejected_transfer_cleanup_message, rejected_transfer_cleanup_messages};

    fn message_port_id(index: u32) -> MessagePortId {
        MessagePortId {
            namespace_id: PipelineNamespaceId(7),
            index: Index::<MessagePortIndex>::new(index).unwrap(),
        }
    }

    #[test]
    fn rejected_incoming_transfer_disentangles_initiator_and_peer() {
        let transferred = message_port_id(1);
        let peer = message_port_id(2);
        let mut port_impl = MessagePortImpl::new(transferred);
        port_impl.entangle(peer);

        match rejected_transfer_cleanup_message(transferred, &port_impl) {
            ScriptToConstellationMessage::DisentanglePorts(actual, Some(actual_peer)) => {
                assert_eq!(actual, transferred);
                assert_eq!(actual_peer, peer);
            },
            _ => panic!("rejected transfer must dispose of the in-flight port and its peer"),
        }
    }

    #[test]
    fn rejected_transform_stream_transfer_cleans_both_embedded_ports() {
        let readable = message_port_id(1);
        let readable_peer = message_port_id(2);
        let writable = message_port_id(3);
        let writable_peer = message_port_id(4);

        let mut readable_impl = MessagePortImpl::new(readable);
        readable_impl.entangle(readable_peer);
        let mut writable_impl = MessagePortImpl::new(writable);
        writable_impl.entangle(writable_peer);

        let [readable_cleanup, writable_cleanup] = rejected_transfer_cleanup_messages([
            (readable, &readable_impl),
            (writable, &writable_impl),
        ]);

        assert_cleanup_message(readable_cleanup, readable, readable_peer);
        assert_cleanup_message(writable_cleanup, writable, writable_peer);
    }

    fn assert_cleanup_message(
        message: ScriptToConstellationMessage,
        expected_port: MessagePortId,
        expected_peer: MessagePortId,
    ) {
        match message {
            ScriptToConstellationMessage::DisentanglePorts(port, Some(peer)) => {
                assert_eq!(port, expected_port);
                assert_eq!(peer, expected_peer);
            },
            _ => panic!("rejected transfer must dispose of each in-flight port and its peer"),
        }
    }
}

impl MessagePortMethods<crate::DomTypeHolder> for MessagePort {
    /// <https://html.spec.whatwg.org/multipage/#dom-messageport-postmessage>
    fn PostMessage(
        &self,
        cx: &mut JSContext,
        message: HandleValue,
        transfer: CustomAutoRooterGuard<Vec<*mut JSObject>>,
    ) -> ErrorResult {
        if self.detached.get() {
            return Ok(());
        }
        self.post_message_impl(cx, message, transfer)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-messageport-postmessage>
    fn PostMessage_(
        &self,
        cx: &mut JSContext,
        message: HandleValue,
        options: RootedTraceableBox<StructuredSerializeOptions>,
    ) -> ErrorResult {
        if self.detached.get() {
            return Ok(());
        }
        let mut rooted = CustomAutoRooter::new(
            options
                .transfer
                .iter()
                .map(|js: &RootedTraceableBox<Heap<*mut JSObject>>| js.get())
                .collect(),
        );
        #[expect(unsafe_code)]
        let guard = unsafe { CustomAutoRooterGuard::new(cx.raw_cx(), &mut rooted) };
        self.post_message_impl(cx, message, guard)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-messageport-start>
    fn Start(&self, cx: &mut JSContext) {
        if self.detached.get() {
            return;
        }
        self.global().start_message_port(cx, self.message_port_id());
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-messageport-close>
    fn Close(&self, cx: &mut JSContext) {
        // Set this's [[Detached]] internal slot value to true.
        self.detached.set(true);

        let global = self.global();
        global.close_message_port(self.message_port_id());

        // If this is entangled, disentangle it.
        global.disentangle_port(cx, self);
    }

    /// <https://html.spec.whatwg.org/multipage/#handler-messageport-onmessage>
    fn GetOnmessage(&self, cx: &mut JSContext) -> Option<Rc<EventHandlerNonNull>> {
        if self.detached.get() {
            return None;
        }
        let eventtarget = self.upcast::<EventTarget>();
        eventtarget.get_event_handler_common(cx, "message")
    }

    /// <https://html.spec.whatwg.org/multipage/#handler-messageport-onmessage>
    fn SetOnmessage(&self, cx: &mut JSContext, listener: Option<Rc<EventHandlerNonNull>>) {
        if self.detached.get() {
            return;
        }
        self.set_onmessage(cx, listener);
        // Note: we cannot use the event_handler macro, due to the need to start the port.
        self.global().start_message_port(cx, self.message_port_id());
    }

    // <https://html.spec.whatwg.org/multipage/#handler-messageport-onmessageerror>
    event_handler!(messageerror, GetOnmessageerror, SetOnmessageerror);

    // <https://html.spec.whatwg.org/multipage/#handler-messageport-onclose>
    event_handler!(close, GetOnclose, SetOnclose);
}
