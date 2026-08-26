/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! This module contains implementations in script that are transferable as per
//! <https://html.spec.whatwg.org/multipage/#transferable-objects>. The implementations are here
//! instead of in script as they need to be passed through the Constellation.

use std::collections::VecDeque;

use malloc_size_of_derive::MallocSizeOf;
use serde::{Deserialize, Serialize};
use servo_base::id::MessagePortId;
use strum::EnumIter;

use crate::PortMessageTask;

#[derive(Debug, Deserialize, MallocSizeOf, Serialize)]
pub struct TransformStreamData {
    pub readable: (MessagePortId, MessagePortImpl),
    pub writable: (MessagePortId, MessagePortImpl),
}

/// All the DOM interfaces that can be transferred.
#[derive(Clone, Copy, Debug, EnumIter)]
pub enum Transferrable {
    /// The `ImageBitmap` interface.
    ImageBitmap,
    /// The `MessagePort` interface.
    MessagePort,
    /// The `OffscreenCanvas` interface.
    OffscreenCanvas,
    /// The `ReadableStream` interface.
    ReadableStream,
    /// The `WritableStream` interface.
    WritableStream,
    /// The `TransformStream` interface.
    TransformStream,
}

#[derive(Debug, Deserialize, MallocSizeOf, Serialize)]
enum MessagePortState {
    /// <https://html.spec.whatwg.org/multipage/#detached>
    Detached,
    /// <https://html.spec.whatwg.org/multipage/#port-message-queue>
    /// The message-queue of this port is enabled,
    /// the boolean represents awaiting completion of a transfer.
    Enabled(bool),
    /// <https://html.spec.whatwg.org/multipage/#port-message-queue>
    /// The message-queue of this port is disabled,
    /// the boolean represents awaiting completion of a transfer.
    Disabled(bool),
}

/// The disposition of a message admitted through the controlled-local routing path.
///
/// The ordinary API predates retained-message accounting and intentionally keeps its `Option`
/// result. This explicit result lets the local owner release a reservation on dispatch/drop while
/// retaining it for an unstarted port buffer.
#[derive(Debug)]
pub enum MessagePortIncomingResult {
    Dispatch(PortMessageTask),
    Buffered,
    Dropped,
}

#[derive(Debug, Deserialize, MallocSizeOf, Serialize)]
/// The data and logic backing the DOM managed MessagePort.
pub struct MessagePortImpl {
    /// The current state of the port.
    state: MessagePortState,

    /// <https://html.spec.whatwg.org/multipage/#entangle>
    entangled_port: Option<MessagePortId>,

    /// <https://html.spec.whatwg.org/multipage/#port-message-queue>
    message_buffer: Option<VecDeque<PortMessageTask>>,

    /// The UUID of this port.
    message_port_id: MessagePortId,
}

impl MessagePortImpl {
    /// Create a new messageport impl.
    pub fn new(port_id: MessagePortId) -> MessagePortImpl {
        MessagePortImpl {
            state: MessagePortState::Disabled(false),
            entangled_port: None,
            message_buffer: None,
            message_port_id: port_id,
        }
    }

    /// Get the Id.
    pub fn message_port_id(&self) -> &MessagePortId {
        &self.message_port_id
    }

    /// Maybe get the Id of the entangled port.
    pub fn entangled_port_id(&self) -> Option<MessagePortId> {
        self.entangled_port
    }

    /// Whether this port is waiting for Constellation to complete a transfer.
    ///
    /// Controlled local message-channel pairs never enter this state. Exposing the bit lets the
    /// script owner prove that an apparently local pair has no outstanding external owner before
    /// omitting it from pending-work inventory.
    pub fn transfer_in_progress(&self) -> bool {
        matches!(
            self.state,
            MessagePortState::Enabled(true) | MessagePortState::Disabled(true)
        )
    }

    /// Whether the port has been detached or explicitly closed.
    pub fn detached(&self) -> bool {
        matches!(self.state, MessagePortState::Detached)
    }

    /// Whether this port retains messages which have not yet been dispatched.
    ///
    /// In particular, posting to an unstarted port buffers the message here. Such a port is not an
    /// idle controlled-local source and must not be reported as quiescent.
    pub fn has_buffered_messages(&self) -> bool {
        self.message_buffer
            .as_ref()
            .is_some_and(|messages| !messages.is_empty())
    }

    /// Number of messages currently retained by an unstarted or transferring port.
    pub fn buffered_message_count(&self) -> usize {
        self.message_buffer.as_ref().map_or(0, VecDeque::len)
    }

    /// Drop messages retained by a controlled-local port which is being explicitly closed.
    pub fn discard_buffered_messages(&mut self) -> usize {
        self.message_buffer.take().map_or(0, |buffer| buffer.len())
    }

    /// <https://html.spec.whatwg.org/multipage/#disentangle>
    pub fn disentangle(&mut self) -> Option<MessagePortId> {
        self.entangled_port.take()
    }

    /// <https://html.spec.whatwg.org/multipage/#entangle>
    pub fn entangle(&mut self, other_id: MessagePortId) {
        self.entangled_port = Some(other_id);
    }

    /// Is this port enabled?
    pub fn enabled(&self) -> bool {
        matches!(self.state, MessagePortState::Enabled(_))
    }

    /// Mark this port as having been shipped.
    /// <https://html.spec.whatwg.org/multipage/#has-been-shipped>
    pub fn set_has_been_shipped(&mut self) {
        match self.state {
            MessagePortState::Detached => {
                panic!("Messageport set_has_been_shipped called in detached state")
            },
            MessagePortState::Enabled(_) => self.state = MessagePortState::Enabled(true),
            MessagePortState::Disabled(_) => self.state = MessagePortState::Disabled(true),
        }
    }

    /// Handle the completion of the transfer,
    /// this is data received from the constellation.
    pub fn complete_transfer(&mut self, mut tasks: VecDeque<PortMessageTask>) {
        match self.state {
            MessagePortState::Detached => return,
            MessagePortState::Enabled(_) => self.state = MessagePortState::Enabled(false),
            MessagePortState::Disabled(_) => self.state = MessagePortState::Disabled(false),
        }

        // Note: these are the tasks that were buffered while the transfer was ongoing,
        // hence they need to execute first.
        // The global will call `start` if we are enabled,
        // which will add tasks on the event-loop to dispatch incoming messages.
        match self.message_buffer {
            Some(ref mut incoming_buffer) => {
                while let Some(task) = tasks.pop_back() {
                    incoming_buffer.push_front(task);
                }
            },
            None => self.message_buffer = Some(tasks),
        }
    }

    /// A message was received from our entangled port,
    /// returns an optional task to be dispatched.
    pub fn handle_incoming(&mut self, task: PortMessageTask) -> Option<PortMessageTask> {
        let should_dispatch = match self.state {
            MessagePortState::Detached => return None,
            MessagePortState::Enabled(in_transfer) => !in_transfer,
            MessagePortState::Disabled(_) => false,
        };

        if should_dispatch {
            Some(task)
        } else {
            match self.message_buffer {
                Some(ref mut buffer) => {
                    buffer.push_back(task);
                },
                None => {
                    let mut queue = VecDeque::new();
                    queue.push_back(task);
                    self.message_buffer = Some(queue);
                },
            }
            None
        }
    }

    /// Handle an already-admitted controlled-local message while preserving its retained-message
    /// reservation until it either dispatches, is dropped, or leaves the native buffer.
    pub fn handle_controlled_local_incoming(
        &mut self,
        task: PortMessageTask,
    ) -> MessagePortIncomingResult {
        let should_dispatch = match self.state {
            MessagePortState::Detached => return MessagePortIncomingResult::Dropped,
            MessagePortState::Enabled(in_transfer) => !in_transfer,
            MessagePortState::Disabled(_) => false,
        };

        if should_dispatch {
            MessagePortIncomingResult::Dispatch(task)
        } else {
            self.message_buffer
                .get_or_insert_with(VecDeque::new)
                .push_back(task);
            MessagePortIncomingResult::Buffered
        }
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-messageport-start>
    /// returns an optional queue of tasks that were buffered while the port was disabled.
    pub fn start(&mut self) -> Option<VecDeque<PortMessageTask>> {
        match self.state {
            MessagePortState::Detached => return None,
            MessagePortState::Enabled(_) => {},
            MessagePortState::Disabled(in_transfer) => {
                self.state = MessagePortState::Enabled(in_transfer);
            },
        }
        if let MessagePortState::Enabled(true) = self.state {
            return None;
        }
        self.message_buffer.take()
    }

    /// <https://html.spec.whatwg.org/multipage/#dom-messageport-close>
    pub fn close(&mut self) {
        // Step 1
        self.state = MessagePortState::Detached;
    }
}

#[derive(Debug, Deserialize, MallocSizeOf, Serialize)]
/// A struct supporting the transfer of OffscreenCanvas, which loosely
/// corresponds to the dataHolder in
/// <https://html.spec.whatwg.org/multipage/#the-offscreencanvas-interface:offscreencanvas-16>
pub struct TransferableOffscreenCanvas {
    pub width: u64,
    pub height: u64,
}
