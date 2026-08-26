/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::reflector::{Reflector, reflect_dom_object_with_proto};

use crate::dom::bindings::codegen::Bindings::MessageChannelBinding::MessageChannelMethods;
use crate::dom::bindings::error::Fallible;
use crate::dom::bindings::root::{Dom, DomRoot};
use crate::dom::globalscope::GlobalScope;
use crate::dom::messageport::MessagePort;

#[dom_struct]
pub(crate) struct MessageChannel {
    reflector_: Reflector,
    port1: Dom<MessagePort>,
    port2: Dom<MessagePort>,
}

impl MessageChannel {
    /// <https://html.spec.whatwg.org/multipage/#dom-messagechannel>
    fn new(
        cx: &mut JSContext,
        incumbent: &GlobalScope,
        proto: Option<HandleObject>,
        controlled_local: bool,
    ) -> DomRoot<MessageChannel> {
        // Step 1
        let port1 = MessagePort::new(cx, incumbent);

        // Step 2
        let port2 = MessagePort::new(cx, incumbent);

        if controlled_local {
            incumbent.track_controlled_local_message_port(&port1);
            incumbent.track_controlled_local_message_port(&port2);
        } else {
            incumbent.track_message_port(&port1, None);
            incumbent.track_message_port(&port2, None);
        }

        // Step 3
        incumbent.entangle_ports(*port1.message_port_id(), *port2.message_port_id());

        // Steps 4-6
        reflect_dom_object_with_proto(
            cx,
            Box::new(MessageChannel::new_inherited(&port1, &port2)),
            incumbent,
            proto,
        )
    }

    pub(crate) fn new_inherited(port1: &MessagePort, port2: &MessagePort) -> MessageChannel {
        MessageChannel {
            reflector_: Reflector::new(),
            port1: Dom::from_ref(port1),
            port2: Dom::from_ref(port2),
        }
    }
}

impl MessageChannelMethods<crate::DomTypeHolder> for MessageChannel {
    /// <https://html.spec.whatwg.org/multipage/#dom-messagechannel>
    fn Constructor(
        cx: &mut JSContext,
        global: &GlobalScope,
        proto: Option<HandleObject>,
    ) -> Fallible<DomRoot<MessageChannel>> {
        // Resolve the caller before publishing either port. A constructor borrowed from the
        // controlled Window by another realm must not acquire that Window's local authority.
        let incumbent = GlobalScope::incumbent();
        let controlled_local = global.admit_message_channel_constructor(incumbent.as_deref())?;
        Ok(MessageChannel::new(cx, global, proto, controlled_local))
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-messagechannel-port1>
    fn Port1(&self) -> DomRoot<MessagePort> {
        DomRoot::from_ref(&*self.port1)
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-messagechannel-port2>
    fn Port2(&self) -> DomRoot<MessagePort> {
        DomRoot::from_ref(&*self.port2)
    }
}
