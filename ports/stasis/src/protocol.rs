/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::io::{self, BufRead, Write};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value, json};

use crate::wake::ShellWaker;

pub const PROTOCOL_VERSION: u8 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_PROTOCOL_ERROR_DETAILS_BYTES: usize = 64 * 1024;
pub const MAX_PROTOCOL_ERROR_DETAILS_DEPTH: usize = 16;
pub const MAX_PROTOCOL_ERROR_DETAILS_VALUES: usize = 1024;
pub const DEFAULT_ORDINARY_LANE_CAPACITY: usize = 8;
const CONTROL_LANE_CAPACITY: usize = 8;
const MAX_EXACT_JSON_INTEGER: i64 = 9_007_199_254_740_991;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub v: u8,
    #[serde(rename = "type")]
    pub kind: String,
    pub id: String,
    #[serde(rename = "sessionId")]
    pub session_id: Option<String>,
    pub method: String,
    #[serde(default = "empty_object")]
    pub params: Value,
}

impl Request {
    fn is_control_lane(&self) -> bool {
        matches!(self.method.as_str(), "protocol.cancel" | "session.close")
    }
}

fn empty_object() -> Value {
    Value::Object(Map::new())
}

#[derive(Debug)]
pub enum ReaderMessage {
    Request(Request),
    CloseRequest {
        request: Request,
        barrier: ReaderCloseBarrier,
    },
    Fatal(ProtocolError),
    Eof,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReaderCloseDisposition {
    Resume,
    Stop,
}

#[derive(Debug)]
pub struct ReaderCloseBarrier {
    disposition: SyncSender<ReaderCloseDisposition>,
}

impl ReaderCloseBarrier {
    pub fn resolve(self, disposition: ReaderCloseDisposition) {
        // If the reader receiver is already gone, clean shutdown's join observes any panic. In
        // the inverse race, dropping an unresolved barrier is treated as Stop by the reader.
        self.disposition.send(disposition).ok();
    }
}

#[derive(Debug)]
pub struct SequencedReaderMessage {
    pub message: ReaderMessage,
    ingress_seq: Option<u128>,
}

impl SequencedReaderMessage {
    fn request(request: Request, ingress_seq: u128) -> Self {
        Self {
            message: ReaderMessage::Request(request),
            ingress_seq: Some(ingress_seq),
        }
    }

    fn close_request(
        request: Request,
        ingress_seq: u128,
        barrier: ReaderCloseBarrier,
    ) -> Self {
        Self {
            message: ReaderMessage::CloseRequest { request, barrier },
            ingress_seq: Some(ingress_seq),
        }
    }

    fn transport(message: ReaderMessage) -> Self {
        Self {
            message,
            ingress_seq: None,
        }
    }

    pub fn ingress_sequence(&self) -> Option<u128> {
        self.ingress_seq
    }

    fn is_terminal(&self) -> bool {
        self.message.is_terminal()
    }

    fn is_control_lane(&self) -> bool {
        self.message.is_control_lane()
    }
}

#[derive(Debug)]
pub enum OrdinaryRequestRemoval {
    Removed(SequencedReaderMessage),
    NotFound,
    Ambiguous,
}

impl ReaderMessage {
    fn is_terminal(&self) -> bool {
        matches!(self, Self::Fatal(_) | Self::Eof)
    }

    fn is_control_lane(&self) -> bool {
        match self {
            Self::Request(request) => request.is_control_lane(),
            Self::CloseRequest { .. } => true,
            Self::Fatal(_) | Self::Eof => false,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProtocolError {
    pub code: &'static str,
    pub message: String,
    pub fatal: bool,
    pub state_effect: &'static str,
    pub details: Option<Value>,
}

impl ProtocolError {
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_request",
            message: message.into(),
            fatal: false,
            state_effect: "none",
            details: None,
        }
    }

    pub fn operation(
        code: &'static str,
        message: impl Into<String>,
        state_effect: &'static str,
    ) -> Self {
        debug_assert!(matches!(state_effect, "none" | "partial" | "indeterminate"));
        Self {
            code,
            message: message.into(),
            fatal: false,
            state_effect,
            details: None,
        }
    }

    fn fatal(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            fatal: true,
            state_effect: "none",
            details: None,
        }
    }

    /// Attach a bounded JSON object whose values can be decoded exactly by the TypeScript SDK.
    pub fn with_details(mut self, details: Value) -> Result<Self, ProtocolErrorDetailsError> {
        if !details.is_object() {
            return Err(ProtocolErrorDetailsError::NotObject);
        }
        let mut values = 0usize;
        validate_protocol_error_details(&details, 0, &mut values)?;
        let bytes = serde_json::to_vec(&details)
            .expect("a validated serde_json::Value must always serialize")
            .len();
        if bytes > MAX_PROTOCOL_ERROR_DETAILS_BYTES {
            return Err(ProtocolErrorDetailsError::TooLarge {
                actual: bytes,
                limit: MAX_PROTOCOL_ERROR_DETAILS_BYTES,
            });
        }
        self.details = Some(details);
        Ok(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProtocolErrorDetailsError {
    NotObject,
    TooDeep { actual: usize, limit: usize },
    TooManyValues { actual: usize, limit: usize },
    IntegerNotExactlyRepresentable,
    TooLarge { actual: usize, limit: usize },
}

impl fmt::Display for ProtocolErrorDetailsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotObject => formatter.write_str("protocol error details must be a JSON object"),
            Self::TooDeep { actual, limit } => write!(
                formatter,
                "protocol error details depth {actual} exceeds {limit}",
            ),
            Self::TooManyValues { actual, limit } => write!(
                formatter,
                "protocol error details contain {actual} values, exceeding {limit}",
            ),
            Self::IntegerNotExactlyRepresentable => formatter.write_str(
                "protocol error details contain an integer that TypeScript cannot decode exactly",
            ),
            Self::TooLarge { actual, limit } => write!(
                formatter,
                "protocol error details encode to {actual} bytes, exceeding {limit}",
            ),
        }
    }
}

impl std::error::Error for ProtocolErrorDetailsError {}

fn validate_protocol_error_details(
    value: &Value,
    depth: usize,
    values: &mut usize,
) -> Result<(), ProtocolErrorDetailsError> {
    if depth > MAX_PROTOCOL_ERROR_DETAILS_DEPTH {
        return Err(ProtocolErrorDetailsError::TooDeep {
            actual: depth,
            limit: MAX_PROTOCOL_ERROR_DETAILS_DEPTH,
        });
    }
    *values = values.saturating_add(1);
    if *values > MAX_PROTOCOL_ERROR_DETAILS_VALUES {
        return Err(ProtocolErrorDetailsError::TooManyValues {
            actual: *values,
            limit: MAX_PROTOCOL_ERROR_DETAILS_VALUES,
        });
    }

    match value {
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                if !(-MAX_EXACT_JSON_INTEGER..=MAX_EXACT_JSON_INTEGER).contains(&integer) {
                    return Err(ProtocolErrorDetailsError::IntegerNotExactlyRepresentable);
                }
            } else if let Some(integer) = number.as_u64() {
                if integer > MAX_EXACT_JSON_INTEGER as u64 {
                    return Err(ProtocolErrorDetailsError::IntegerNotExactlyRepresentable);
                }
            } else if number.as_f64().is_none() ||
                !number
                    .to_string()
                    .chars()
                    .any(|character| matches!(character, '.' | 'e' | 'E'))
            {
                return Err(ProtocolErrorDetailsError::IntegerNotExactlyRepresentable);
            }
        },
        Value::Array(items) => {
            for item in items {
                validate_protocol_error_details(item, depth.saturating_add(1), values)?;
            }
        },
        Value::Object(object) => {
            for item in object.values() {
                validate_protocol_error_details(item, depth.saturating_add(1), values)?;
            }
        },
        Value::Null | Value::Bool(_) | Value::String(_) => {},
    }
    Ok(())
}

/// Sending half of the bounded protocol inbox.
///
/// Ordinary traffic, cooperative control requests, and terminal transport state never contend
/// for the same queue slots. All sends are non-blocking so the stdin reader remains able to decode
/// a cancellation immediately after the ordinary lane becomes full. A request beyond a lane's
/// capacity becomes a terminal `input_backpressure` error rather than creating unbounded memory.
pub struct ReaderSender {
    queue: Arc<Mutex<ReaderQueueState>>,
}

/// Receiving half of the bounded protocol inbox. Terminal transport state and cooperative
/// control always overtake queued ordinary engine requests.
pub struct ReaderInbox {
    queue: Arc<Mutex<ReaderQueueState>>,
}

struct ReaderQueueState {
    ordinary: VecDeque<SequencedReaderMessage>,
    control: VecDeque<SequencedReaderMessage>,
    terminal: Option<SequencedReaderMessage>,
    ordinary_capacity: usize,
    sender_alive: bool,
    receiver_alive: bool,
}

pub fn reader_channel(ordinary_capacity: usize) -> (ReaderSender, ReaderInbox) {
    assert!(
        ordinary_capacity > 0,
        "ordinary protocol lane must have capacity"
    );
    let queue = Arc::new(Mutex::new(ReaderQueueState {
        ordinary: VecDeque::with_capacity(ordinary_capacity),
        control: VecDeque::with_capacity(CONTROL_LANE_CAPACITY),
        terminal: None,
        ordinary_capacity,
        sender_alive: true,
        receiver_alive: true,
    }));
    (
        ReaderSender {
            queue: queue.clone(),
        },
        ReaderInbox { queue },
    )
}

impl ReaderInbox {
    pub fn try_recv(&self) -> Result<ReaderMessage, TryRecvError> {
        self.try_recv_sequenced().map(|sequenced| sequenced.message)
    }

    /// Remove a not-yet-admitted ordinary request by its opaque client identity.
    ///
    /// Cancellation uses the same mutex as enqueue and dequeue, so removal is linearized against
    /// owner admission. The returned envelope retains its original ingress sequence, and removing
    /// it does not renumber or reorder any remaining request. Duplicate queued identities are
    /// reported as ambiguous without mutating the queue.
    pub fn remove_ordinary_request(&self, request_id: &str) -> OrdinaryRequestRemoval {
        let mut queue = self.queue.lock().expect("protocol input queue poisoned");
        let position = {
            let mut matches =
                queue
                    .ordinary
                    .iter()
                    .enumerate()
                    .filter_map(|(position, sequenced)| match &sequenced.message {
                        ReaderMessage::Request(request) if request.id == request_id => {
                            Some(position)
                        },
                        _ => None,
                    });
            let Some(position) = matches.next() else {
                return OrdinaryRequestRemoval::NotFound;
            };
            if matches.next().is_some() {
                return OrdinaryRequestRemoval::Ambiguous;
            }
            position
        };
        OrdinaryRequestRemoval::Removed(
            queue
                .ordinary
                .remove(position)
                .expect("matched ordinary request disappeared while queue mutex was held"),
        )
    }

    pub fn try_recv_sequenced(&self) -> Result<SequencedReaderMessage, TryRecvError> {
        let mut queue = self.queue.lock().expect("protocol input queue poisoned");
        if let Some(message) = queue.terminal.take() {
            return Ok(message);
        }
        if let Some(message) = queue.control.pop_front() {
            return Ok(message);
        }
        if let Some(message) = queue.ordinary.pop_front() {
            return Ok(message);
        }
        if queue.sender_alive {
            Err(TryRecvError::Empty)
        } else {
            Err(TryRecvError::Disconnected)
        }
    }
}

impl Drop for ReaderInbox {
    fn drop(&mut self) {
        let mut queue = self.queue.lock().expect("protocol input queue poisoned");
        queue.receiver_alive = false;
        queue.ordinary.clear();
        queue.control.clear();
        queue.terminal.take();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SendOutcome {
    Continue,
    Stop,
    Disconnected,
}

impl ReaderSender {
    fn enqueue_overload(queue: &mut ReaderQueueState, lane: &'static str) -> SendOutcome {
        let error = ProtocolError::fatal(
            "input_backpressure",
            format!("{lane} protocol input lane is saturated"),
        );
        if queue.terminal.is_none() {
            queue.terminal = Some(SequencedReaderMessage::transport(ReaderMessage::Fatal(
                error,
            )));
            SendOutcome::Stop
        } else {
            SendOutcome::Disconnected
        }
    }
}

impl ReaderSender {
    fn enqueue(&self, message: SequencedReaderMessage) -> SendOutcome {
        let mut queue = self.queue.lock().expect("protocol input queue poisoned");
        if !queue.receiver_alive {
            return SendOutcome::Disconnected;
        }
        if message.is_terminal() {
            if queue.terminal.is_some() {
                return SendOutcome::Disconnected;
            }
            queue.terminal = Some(message);
            return SendOutcome::Stop;
        }

        if message.is_control_lane() {
            if queue.control.len() == CONTROL_LANE_CAPACITY {
                return Self::enqueue_overload(&mut queue, "control");
            }
            queue.control.push_back(message);
        } else {
            if queue.ordinary.len() == queue.ordinary_capacity {
                return Self::enqueue_overload(&mut queue, "ordinary");
            }
            queue.ordinary.push_back(message);
        }
        SendOutcome::Continue
    }
}

impl Drop for ReaderSender {
    fn drop(&mut self) {
        let mut queue = self.queue.lock().expect("protocol input queue poisoned");
        queue.sender_alive = false;
    }
}

pub(crate) fn spawn_reader(sender: ReaderSender, waker: ShellWaker) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("stasis-protocol-reader".into())
        .spawn(move || {
            let stdin = io::stdin();
            let input = stdin.lock();
            read_protocol_input(input, sender, waker);
        })
        .expect("failed to spawn protocol reader")
}

fn read_protocol_input(input: impl BufRead, sender: ReaderSender, waker: ShellWaker) {
    let mut reader = ProtocolReader::new(input);
    loop {
        let (message, close_disposition): (_, Option<Receiver<ReaderCloseDisposition>>) =
            match reader.next_request() {
                Ok(Some(request)) if request.method == "session.close" => {
                    let (disposition, receiver) = sync_channel(1);
                    (
                        SequencedReaderMessage::close_request(
                            request,
                            reader.ingress_seq,
                            ReaderCloseBarrier { disposition },
                        ),
                        Some(receiver),
                    )
                },
                Ok(Some(request)) => (
                    SequencedReaderMessage::request(request, reader.ingress_seq),
                    None,
                ),
                Ok(None) => (
                    SequencedReaderMessage::transport(ReaderMessage::Eof),
                    None,
                ),
                Err(error) => (
                    SequencedReaderMessage::transport(ReaderMessage::Fatal(error)),
                    None,
                ),
            };
        match sender.enqueue(message) {
            SendOutcome::Continue => {
                waker.notify_protocol_input();
                if let Some(disposition) = close_disposition {
                    match disposition.recv() {
                        Ok(ReaderCloseDisposition::Resume) => {},
                        Ok(ReaderCloseDisposition::Stop) | Err(_) => return,
                    }
                }
            },
            SendOutcome::Stop => {
                waker.notify_protocol_input();
                return;
            },
            SendOutcome::Disconnected => return,
        }
    }
}

pub fn read_request(input: &mut impl BufRead) -> Result<Option<Request>, ProtocolError> {
    ProtocolReader::new(input).next_request()
}

struct ProtocolReader<R> {
    input: R,
    ingress_seq: u128,
    sequence_exhausted: bool,
}

impl<R: BufRead> ProtocolReader<R> {
    fn new(input: R) -> Self {
        Self {
            input,
            ingress_seq: 0,
            sequence_exhausted: false,
        }
    }

    #[cfg(test)]
    fn with_ingress_sequence(input: R, ingress_seq: u128) -> Self {
        Self {
            input,
            ingress_seq,
            sequence_exhausted: false,
        }
    }

    fn next_request(&mut self) -> Result<Option<Request>, ProtocolError> {
        if self.sequence_exhausted {
            return Err(ProtocolError::fatal(
                "ingress_sequence_exhausted",
                "protocol ingress sequence is exhausted",
            ));
        }
        let Some(frame) = read_frame(&mut self.input)? else {
            return Ok(None);
        };
        let Some(ingress_seq) = self.ingress_seq.checked_add(1) else {
            self.sequence_exhausted = true;
            return Err(ProtocolError::fatal(
                "ingress_sequence_exhausted",
                "protocol ingress sequence is exhausted",
            ));
        };
        self.ingress_seq = ingress_seq;
        decode_request(frame).map(Some)
    }
}

fn read_frame(input: &mut impl BufRead) -> Result<Option<Vec<u8>>, ProtocolError> {
    // One extra byte is sufficient to distinguish an oversized LF frame and to retain the CR of
    // a maximum-sized CRLF frame. The decoder never allocates in proportion to an untrusted line.
    let mut frame = Vec::with_capacity(1024);
    loop {
        let available = input
            .fill_buf()
            .map_err(|error| ProtocolError::fatal("input_error", error.to_string()))?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Err(ProtocolError::fatal(
                    "incomplete_frame",
                    "stdin ended before the NDJSON frame terminator",
                ))
            };
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        let fragment_len = newline.unwrap_or(available.len());
        let fragment = &available[..fragment_len];
        let maximum_buffered = MAX_FRAME_BYTES + 1;
        if fragment.len() > maximum_buffered.saturating_sub(frame.len()) {
            return Err(frame_too_large());
        }
        frame.extend_from_slice(fragment);
        input.consume(consumed);

        if newline.is_some() {
            if frame.last() == Some(&b'\r') {
                frame.pop();
            }
            if frame.len() > MAX_FRAME_BYTES {
                return Err(frame_too_large());
            }
            return Ok(Some(frame));
        }

        if frame.len() == maximum_buffered && frame.last() != Some(&b'\r') {
            return Err(frame_too_large());
        }
    }
}

fn frame_too_large() -> ProtocolError {
    ProtocolError::fatal(
        "frame_too_large",
        format!("input frame exceeds {MAX_FRAME_BYTES} bytes"),
    )
}

fn decode_request(frame: Vec<u8>) -> Result<Request, ProtocolError> {
    if frame.is_empty() || frame.iter().all(u8::is_ascii_whitespace) {
        return Err(ProtocolError::fatal("empty_frame", "empty NDJSON frame"));
    }
    if frame.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(ProtocolError::fatal(
            "unexpected_bom",
            "UTF-8 BOM is not allowed",
        ));
    }
    let text = std::str::from_utf8(&frame)
        .map_err(|error| ProtocolError::fatal("invalid_utf8", error.to_string()))?;
    let mut decoder = serde_json::Deserializer::from_str(text);
    let StrictJson(value) = StrictJson::deserialize(&mut decoder)
        .map_err(|error| ProtocolError::fatal("invalid_json", error.to_string()))?;
    decoder
        .end()
        .map_err(|error| ProtocolError::fatal("invalid_json", error.to_string()))?;
    if !value.is_object() {
        return Err(ProtocolError::fatal(
            "invalid_envelope",
            "input frame must be a JSON object",
        ));
    }
    let request: Request = serde_json::from_value(value)
        .map_err(|error| ProtocolError::fatal("invalid_envelope", error.to_string()))?;
    validate_envelope(&request)?;
    Ok(request)
}

#[derive(Debug)]
struct StrictJson(Value);

impl<'de> Deserialize<'de> for StrictJson {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictJsonVisitor)
    }
}

struct StrictJsonVisitor;

impl<'de> Visitor<'de> for StrictJsonVisitor {
    type Value = StrictJson;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object members")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictJson(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictJson(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictJson(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(|number| StrictJson(Value::Number(number)))
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(StrictJson(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictJson(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictJson(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictJson(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(1024));
        while let Some(StrictJson(value)) = sequence.next_element::<StrictJson>()? {
            values.push(value);
        }
        Ok(StrictJson(Value::Array(values)))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut names = BTreeSet::new();
        let mut values = Map::with_capacity(object.size_hint().unwrap_or(0).min(1024));
        while let Some(name) = object.next_key::<String>()? {
            if !names.insert(name.clone()) {
                // This decoder runs before the method is known, so the member name can belong to
                // a sensitive payload such as imported cookies or Web Storage. Keep the fatal
                // diagnostic independent from all request bytes; serde may still append a safe
                // line/column location.
                return Err(de::Error::custom("duplicate JSON object member"));
            }
            let StrictJson(value) = object.next_value::<StrictJson>()?;
            values.insert(name, value);
        }
        Ok(StrictJson(Value::Object(values)))
    }
}

fn validate_envelope(request: &Request) -> Result<(), ProtocolError> {
    if request.v != PROTOCOL_VERSION {
        return Err(ProtocolError::fatal(
            "unsupported_protocol",
            format!("unsupported protocol version {}", request.v),
        ));
    }
    if request.kind != "request" {
        return Err(ProtocolError::fatal(
            "invalid_envelope",
            "input frame type must be request",
        ));
    }
    if request.id.is_empty() {
        return Err(ProtocolError::fatal(
            "invalid_envelope",
            "request id must not be empty",
        ));
    }
    Ok(())
}

pub struct ProtocolWriter<W> {
    output: W,
    wire_seq: u128,
    sequence_exhausted: bool,
    write_failed: bool,
}

impl<W: Write> ProtocolWriter<W> {
    pub fn new(output: W) -> Self {
        Self {
            output,
            wire_seq: 0,
            sequence_exhausted: false,
            write_failed: false,
        }
    }

    #[cfg(test)]
    fn with_wire_sequence(output: W, wire_seq: u128) -> Self {
        Self {
            output,
            wire_seq,
            sequence_exhausted: false,
            write_failed: false,
        }
    }

    pub fn result(
        &mut self,
        request: &Request,
        session_id: Option<&str>,
        result: Value,
    ) -> io::Result<()> {
        let wire_seq = self.next_wire_seq()?;
        self.write(json!({
            "v": PROTOCOL_VERSION,
            "type": "response",
            "wireSeq": wire_seq,
            "id": request.id,
            "sessionId": session_id,
            "result": result,
        }))
    }

    pub fn error(
        &mut self,
        request: Option<&Request>,
        session_id: Option<&str>,
        error: &ProtocolError,
    ) -> io::Result<()> {
        let wire_seq = self.next_wire_seq()?;
        let mut error_payload = json!({
            "code": error.code,
            "message": error.message,
            "fatal": error.fatal,
            "stateEffect": error.state_effect,
        });
        if let Some(details) = &error.details {
            error_payload
                .as_object_mut()
                .expect("protocol error payload is an object")
                .insert("details".to_owned(), details.clone());
        }
        self.write(json!({
            "v": PROTOCOL_VERSION,
            "type": if request.is_some() { "response" } else { "event" },
            "wireSeq": wire_seq,
            "id": request.map(|request| request.id.as_str()),
            "sessionId": session_id,
            "event": if request.is_none() { Some("protocol.fatal") } else { None::<&str> },
            "error": error_payload,
        }))
    }

    fn next_wire_seq(&mut self) -> io::Result<String> {
        if self.write_failed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "protocol output is unavailable after a prior write failure",
            ));
        }
        if self.sequence_exhausted {
            return Err(sequence_exhausted_error());
        }
        let Some(wire_seq) = self.wire_seq.checked_add(1) else {
            self.sequence_exhausted = true;
            return Err(sequence_exhausted_error());
        };
        self.wire_seq = wire_seq;
        Ok(wire_seq.to_string())
    }

    fn write(&mut self, frame: Value) -> io::Result<()> {
        let mut encoded = serde_json::to_vec(&frame)?;
        encoded.push(b'\n');
        let result = self
            .output
            .write_all(&encoded)
            .and_then(|()| self.output.flush());
        if result.is_err() {
            // Once any prefix may have reached the pipe, another frame could only make the stream
            // ambiguous. Keep failure sticky and let the shell terminate.
            self.write_failed = true;
        }
        result
    }
}

fn sequence_exhausted_error() -> io::Error {
    io::Error::other("protocol wire sequence is exhausted")
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor, Read};
    use std::time::{Duration, Instant};

    use super::*;

    const REQUEST_PREFIX: &[u8] =
        br#"{"v":1,"type":"request","id":"one","method":"x","params":{"padding":""#;
    const REQUEST_SUFFIX: &[u8] = br#""}}"#;

    fn parse(bytes: &[u8]) -> Result<Option<Request>, ProtocolError> {
        read_request(&mut Cursor::new(bytes))
    }

    fn request(method: &str) -> Request {
        let frame = format!(
            "{{\"v\":1,\"type\":\"request\",\"id\":\"{method}\",\"method\":\"{method}\",\"params\":{{}}}}\n"
        );
        parse(frame.as_bytes()).unwrap().unwrap()
    }

    fn sequenced(method: &str, ingress_seq: u128) -> SequencedReaderMessage {
        SequencedReaderMessage::request(request(method), ingress_seq)
    }

    fn padded_request(payload_bytes: usize, crlf: bool) -> Vec<u8> {
        assert!(payload_bytes >= REQUEST_PREFIX.len() + REQUEST_SUFFIX.len());
        let mut frame = Vec::with_capacity(payload_bytes + usize::from(crlf) + 1);
        frame.extend_from_slice(REQUEST_PREFIX);
        frame.resize(payload_bytes - REQUEST_SUFFIX.len(), b'a');
        frame.extend_from_slice(REQUEST_SUFFIX);
        if crlf {
            frame.push(b'\r');
        }
        frame.push(b'\n');
        frame
    }

    fn close_then_ordinary_input() -> Cursor<Vec<u8>> {
        Cursor::new(
            br#"{"v":1,"type":"request","id":"close","method":"session.close","params":{}}
{"v":1,"type":"request","id":"later","method":"dom.query","params":{}}
"#
            .to_vec(),
        )
    }

    fn wait_for_reader_wake(waker: &ShellWaker, observed: crate::wake::WakeGeneration) {
        waker
            .wait_for_change_checked(observed, Instant::now() + Duration::from_secs(1))
            .expect("protocol reader did not wake the owner");
    }

    struct ChunkedReader {
        bytes: Cursor<Vec<u8>>,
        maximum_chunk: usize,
    }

    impl Read for ChunkedReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            let length = output.len().min(self.maximum_chunk);
            self.bytes.read(&mut output[..length])
        }
    }

    #[derive(Default)]
    struct OneByteWriter {
        bytes: Vec<u8>,
        flushes: usize,
    }

    impl Write for OneByteWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let Some(byte) = bytes.first() else {
                return Ok(0);
            };
            self.bytes.push(*byte);
            Ok(1)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    struct FailingWriter {
        bytes: Vec<u8>,
        remaining: usize,
    }

    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"));
            }
            let written = bytes.len().min(self.remaining);
            self.bytes.extend_from_slice(&bytes[..written]);
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn accepts_one_complete_request() {
        let request = parse(
            br#"{"v":1,"type":"request","id":"one","method":"protocol.initialize","params":{}}
"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(request.id, "one");
    }

    #[test]
    fn incrementally_decodes_chunked_utf8_and_multiple_frames() {
        let bytes = concat!(
            "{\"v\":1,\"type\":\"request\",\"id\":\"café\",\"method\":\"x\",\"params\":{}}\r\n",
            "{\"v\":1,\"type\":\"request\",\"id\":\"雪\",\"method\":\"x\",\"params\":{}}\n"
        )
        .as_bytes()
        .to_vec();
        let input = ChunkedReader {
            bytes: Cursor::new(bytes),
            maximum_chunk: 1,
        };
        let mut reader = ProtocolReader::new(BufReader::with_capacity(3, input));

        let first = reader.next_request().unwrap().unwrap();
        assert_eq!(reader.ingress_seq, 1);
        let second = reader.next_request().unwrap().unwrap();
        assert_eq!(reader.ingress_seq, 2);

        assert_eq!(first.id, "café");
        assert_eq!(second.id, "雪");
        assert!(reader.next_request().unwrap().is_none());
    }

    #[test]
    fn accepts_exact_maximum_lf_and_crlf_payloads() {
        assert!(
            parse(&padded_request(MAX_FRAME_BYTES, false))
                .unwrap()
                .is_some()
        );
        assert!(
            parse(&padded_request(MAX_FRAME_BYTES, true))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn rejects_one_byte_over_the_payload_limit() {
        for crlf in [false, true] {
            let error = parse(&padded_request(MAX_FRAME_BYTES + 1, crlf)).unwrap_err();
            assert_eq!(error.code, "frame_too_large");
            assert!(error.fatal);
        }
    }

    #[test]
    fn preserves_an_id_when_params_need_correlated_validation() {
        let request = parse(
            br#"{"v":1,"type":"request","id":"one","method":"protocol.initialize","params":null}
"#,
        )
        .unwrap()
        .unwrap();
        assert_eq!(request.id, "one");
        assert!(request.params.is_null());
    }

    #[test]
    fn rejects_eof_mid_frame() {
        let error = parse(br#"{"v":1}"#).unwrap_err();
        assert_eq!(error.code, "incomplete_frame");
        assert!(error.fatal);
    }

    #[test]
    fn distinguishes_clean_eof_from_an_incomplete_frame() {
        assert!(parse(b"").unwrap().is_none());
        let error = parse(b" ").unwrap_err();
        assert_eq!(error.code, "incomplete_frame");
    }

    #[test]
    fn rejects_a_bom() {
        let mut frame = vec![0xef, 0xbb, 0xbf];
        frame.extend_from_slice(
            br#"{"v":1,"type":"request","id":"one","method":"x","params":{}}
"#,
        );
        let error = parse(&frame).unwrap_err();
        assert_eq!(error.code, "unexpected_bom");
    }

    #[test]
    fn rejects_invalid_utf8_before_json_decoding() {
        let error = parse(&[0xff, b'\n']).unwrap_err();
        assert_eq!(error.code, "invalid_utf8");
    }

    #[test]
    fn rejects_blank_and_non_object_frames() {
        assert_eq!(parse(b" \t\r\n").unwrap_err().code, "empty_frame");
        assert_eq!(parse(b"[]\n").unwrap_err().code, "invalid_envelope");
    }

    #[test]
    fn rejects_duplicate_members_at_every_object_depth() {
        let top = parse(
            br#"{"v":1,"type":"request","id":"one","id":"two","method":"x","params":{}}
"#,
        )
        .unwrap_err();
        assert_eq!(top.code, "invalid_json");
        assert!(top.message.contains("duplicate JSON object member"));
        assert!(!top.message.contains("\"id\""));

        let nested = parse(
            br#"{"v":1,"type":"request","id":"one","method":"x","params":{"a":1,"a":2}}
"#,
        )
        .unwrap_err();
        assert_eq!(nested.code, "invalid_json");
        assert!(nested.message.contains("duplicate JSON object member"));
        assert!(!nested.message.contains("\"a\""));
    }

    #[test]
    fn sensitive_duplicate_member_never_enters_fatal_diagnostics() {
        const SENTINEL: &str = "SECRET-SESSION-STATE-CANARY";
        let frame = format!(
            "{{\"v\":1,\"type\":\"request\",\"id\":\"one\",\"method\":\"session.open\",\"params\":{{\"state\":{{\"{SENTINEL}\":1,\"{SENTINEL}\":2}}}}}}\n"
        );
        let error = parse(frame.as_bytes()).unwrap_err();
        assert_eq!(error.code, "invalid_json");

        // Shell::handle_reader_message returns this message to main, which writes this exact
        // diagnostic shape to stderr.
        let stderr = format!("stasis shell fatal error: {}", error.message);
        assert!(!stderr.contains(SENTINEL));

        let mut output = Vec::new();
        ProtocolWriter::new(&mut output)
            .error(None, None, &error)
            .unwrap();
        let fatal = String::from_utf8(output).unwrap();
        assert!(!fatal.contains(SENTINEL));
    }

    #[test]
    fn rejects_unknown_envelope_members_and_trailing_json() {
        let unknown = parse(
            br#"{"v":1,"type":"request","id":"one","method":"x","params":{},"extra":true}
"#,
        )
        .unwrap_err();
        assert_eq!(unknown.code, "invalid_envelope");

        let trailing = parse(
            br#"{"v":1,"type":"request","id":"one","method":"x","params":{}} {}
"#,
        )
        .unwrap_err();
        assert_eq!(trailing.code, "invalid_json");
    }

    #[test]
    fn ingress_sequence_exhaustion_is_checked_and_sticky() {
        let frame = padded_request(REQUEST_PREFIX.len() + REQUEST_SUFFIX.len(), false);
        let mut bytes = frame.clone();
        bytes.extend_from_slice(&frame);
        let mut reader = ProtocolReader::with_ingress_sequence(Cursor::new(bytes), u128::MAX - 1);

        assert!(reader.next_request().unwrap().is_some());
        assert_eq!(reader.ingress_seq, u128::MAX);
        assert_eq!(
            reader.next_request().unwrap_err().code,
            "ingress_sequence_exhausted"
        );
        assert!(reader.sequence_exhausted);
        assert_eq!(
            reader.next_request().unwrap_err().code,
            "ingress_sequence_exhausted"
        );
    }

    #[test]
    fn accepted_close_stops_input_before_later_frames_and_allows_join() {
        let (sender, inbox) = reader_channel(2);
        let waker = ShellWaker::default();
        let observed = waker.snapshot_checked().unwrap();
        let reader_waker = waker.clone();
        let reader = std::thread::spawn(move || {
            read_protocol_input(close_then_ordinary_input(), sender, reader_waker)
        });

        wait_for_reader_wake(&waker, observed);
        let close = inbox.try_recv().unwrap();
        let ReaderMessage::CloseRequest { request, barrier } = close else {
            panic!("reader did not preserve the close barrier: {close:?}");
        };
        assert_eq!(request.method, "session.close");
        barrier.resolve(ReaderCloseDisposition::Stop);
        reader.join().expect("protocol reader did not stop cleanly");
        assert!(matches!(
            inbox.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn rejected_close_resumes_input_without_reordering_later_frames() {
        let (sender, inbox) = reader_channel(2);
        let waker = ShellWaker::default();
        let observed = waker.snapshot_checked().unwrap();
        let reader_waker = waker.clone();
        let reader = std::thread::spawn(move || {
            read_protocol_input(close_then_ordinary_input(), sender, reader_waker)
        });

        wait_for_reader_wake(&waker, observed);
        let close = inbox.try_recv().unwrap();
        let ReaderMessage::CloseRequest { request, barrier } = close else {
            panic!("reader did not preserve the close barrier: {close:?}");
        };
        assert_eq!(request.method, "session.close");
        let observed = waker.snapshot_checked().unwrap();
        barrier.resolve(ReaderCloseDisposition::Resume);
        wait_for_reader_wake(&waker, observed);
        reader.join().expect("protocol reader did not resume cleanly");

        assert!(matches!(inbox.try_recv().unwrap(), ReaderMessage::Eof));
        assert!(matches!(
            inbox.try_recv().unwrap(),
            ReaderMessage::Request(request) if request.id == "later"
        ));
        assert!(matches!(
            inbox.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn control_and_close_overtake_a_full_ordinary_lane() {
        let (sender, inbox) = reader_channel(1);
        assert_eq!(
            sender.enqueue(sequenced("dom.query", 1)),
            SendOutcome::Continue
        );
        assert_eq!(
            sender.enqueue(sequenced("protocol.cancel", 2)),
            SendOutcome::Continue
        );
        let control = inbox.try_recv_sequenced().unwrap();
        assert_eq!(control.ingress_sequence(), Some(2));
        assert!(matches!(
            control.message,
            ReaderMessage::Request(request) if request.method == "protocol.cancel"
        ));
        assert!(matches!(
            inbox.try_recv().unwrap(),
            ReaderMessage::Request(request) if request.method == "dom.query"
        ));

        assert_eq!(
            sender.enqueue(sequenced("dom.query", 3)),
            SendOutcome::Continue
        );
        assert_eq!(
            sender.enqueue(sequenced("session.close", 4)),
            SendOutcome::Continue
        );
        assert!(matches!(
            inbox.try_recv().unwrap(),
            ReaderMessage::Request(request) if request.method == "session.close"
        ));
    }

    #[test]
    fn queued_cancellation_removes_only_the_matching_ordinary_request() {
        let (sender, inbox) = reader_channel(4);
        assert_eq!(sender.enqueue(sequenced("one", 1)), SendOutcome::Continue);
        assert_eq!(sender.enqueue(sequenced("two", 2)), SendOutcome::Continue);
        assert_eq!(sender.enqueue(sequenced("three", 3)), SendOutcome::Continue);

        let OrdinaryRequestRemoval::Removed(removed) = inbox.remove_ordinary_request("two") else {
            panic!("matching ordinary request should be removed");
        };
        assert_eq!(removed.ingress_sequence(), Some(2));
        assert!(matches!(
            removed.message,
            ReaderMessage::Request(request) if request.id == "two"
        ));
        assert!(matches!(
            inbox.remove_ordinary_request("two"),
            OrdinaryRequestRemoval::NotFound
        ));
        assert!(matches!(
            inbox.remove_ordinary_request("stale"),
            OrdinaryRequestRemoval::NotFound
        ));

        assert!(matches!(
            inbox.try_recv().unwrap(),
            ReaderMessage::Request(request) if request.id == "one"
        ));
        let remaining = inbox.try_recv_sequenced().unwrap();
        assert_eq!(remaining.ingress_sequence(), Some(3));
        assert!(matches!(
            remaining.message,
            ReaderMessage::Request(request) if request.id == "three"
        ));
    }

    #[test]
    fn duplicate_queued_ids_are_reported_without_mutating_the_queue() {
        let (sender, inbox) = reader_channel(3);
        assert_eq!(sender.enqueue(sequenced("same", 1)), SendOutcome::Continue);
        assert_eq!(sender.enqueue(sequenced("same", 2)), SendOutcome::Continue);
        assert_eq!(sender.enqueue(sequenced("last", 3)), SendOutcome::Continue);

        assert!(matches!(
            inbox.remove_ordinary_request("same"),
            OrdinaryRequestRemoval::Ambiguous
        ));
        for expected_sequence in [1, 2, 3] {
            assert_eq!(
                inbox.try_recv_sequenced().unwrap().ingress_sequence(),
                Some(expected_sequence)
            );
        }
    }

    #[test]
    fn queued_removal_does_not_change_control_or_terminal_priority() {
        let (sender, inbox) = reader_channel(2);
        assert_eq!(
            sender.enqueue(sequenced("remove-me", 1)),
            SendOutcome::Continue
        );
        assert_eq!(
            sender.enqueue(sequenced("ordinary", 2)),
            SendOutcome::Continue
        );
        assert_eq!(
            sender.enqueue(sequenced("protocol.cancel", 3)),
            SendOutcome::Continue
        );
        assert_eq!(
            sender.enqueue(SequencedReaderMessage::transport(ReaderMessage::Eof)),
            SendOutcome::Stop
        );

        assert!(matches!(
            inbox.remove_ordinary_request("remove-me"),
            OrdinaryRequestRemoval::Removed(_)
        ));
        assert!(matches!(inbox.try_recv().unwrap(), ReaderMessage::Eof));
        assert!(matches!(
            inbox.try_recv().unwrap(),
            ReaderMessage::Request(request) if request.method == "protocol.cancel"
        ));
        assert!(matches!(
            inbox.try_recv().unwrap(),
            ReaderMessage::Request(request) if request.id == "ordinary"
        ));
    }

    #[test]
    fn terminal_state_overtakes_ordinary_backpressure() {
        let (sender, inbox) = reader_channel(1);
        assert_eq!(sender.enqueue(sequenced("one", 1)), SendOutcome::Continue);
        assert_eq!(sender.enqueue(sequenced("two", 2)), SendOutcome::Stop);
        match inbox.try_recv().unwrap() {
            ReaderMessage::Fatal(error) => assert_eq!(error.code, "input_backpressure"),
            message => panic!("expected terminal backpressure, got {message:?}"),
        }
        assert!(matches!(
            inbox.try_recv().unwrap(),
            ReaderMessage::Request(request) if request.method == "one"
        ));
    }

    #[test]
    fn eof_has_a_reserved_terminal_lane() {
        let (sender, inbox) = reader_channel(1);
        assert_eq!(sender.enqueue(sequenced("one", 1)), SendOutcome::Continue);
        assert_eq!(
            sender.enqueue(SequencedReaderMessage::transport(ReaderMessage::Eof)),
            SendOutcome::Stop
        );
        assert!(matches!(inbox.try_recv().unwrap(), ReaderMessage::Eof));
    }

    #[test]
    fn exact_counters_are_strings_on_output() {
        let request = parse(
            br#"{"v":1,"type":"request","id":"one","method":"x","params":{}}
"#,
        )
        .unwrap()
        .unwrap();
        let mut bytes = Vec::new();
        ProtocolWriter::new(&mut bytes)
            .result(&request, None, json!({"ok": true}))
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["wireSeq"], "1");
    }

    #[test]
    fn wire_sequence_is_checked_at_u128_exhaustion() {
        let request = request("x");
        let mut writer = ProtocolWriter::with_wire_sequence(Vec::new(), u128::MAX - 1);
        writer.result(&request, None, json!({})).unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&writer.output).unwrap()["wireSeq"],
            u128::MAX.to_string()
        );
        assert_eq!(
            writer.result(&request, None, json!({})).unwrap_err().kind(),
            io::ErrorKind::Other
        );
        assert!(writer.sequence_exhausted);
        assert!(writer.result(&request, None, json!({})).is_err());
    }

    #[test]
    fn output_is_fully_encoded_then_written_through_short_writes() {
        let request = request("x");
        let mut writer = ProtocolWriter::new(OneByteWriter::default());
        writer
            .result(&request, None, json!({"line": "one\ntwo"}))
            .unwrap();

        assert_eq!(writer.output.flushes, 1);
        assert_eq!(writer.output.bytes.last(), Some(&b'\n'));
        assert_eq!(
            writer
                .output
                .bytes
                .iter()
                .filter(|byte| **byte == b'\n')
                .count(),
            1
        );
        let value: Value = serde_json::from_slice(&writer.output.bytes).unwrap();
        assert_eq!(value["result"]["line"], "one\ntwo");
    }

    #[test]
    fn a_partial_output_failure_permanently_poisons_the_writer() {
        let request = request("x");
        let mut writer = ProtocolWriter::new(FailingWriter {
            bytes: Vec::new(),
            remaining: 7,
        });

        assert!(writer.result(&request, None, json!({})).is_err());
        let partial_length = writer.output.bytes.len();
        assert_eq!(partial_length, 7);
        assert_eq!(
            writer.result(&request, None, json!({})).unwrap_err().kind(),
            io::ErrorKind::BrokenPipe
        );
        assert_eq!(writer.output.bytes.len(), partial_length);
    }

    #[test]
    fn operation_errors_preserve_state_effect() {
        let request = parse(
            br#"{"v":1,"type":"request","id":"one","method":"x","params":{}}
"#,
        )
        .unwrap()
        .unwrap();
        let error = ProtocolError::operation("wall_time_limit_exceeded", "limit", "indeterminate");
        let mut bytes = Vec::new();
        ProtocolWriter::new(&mut bytes)
            .error(Some(&request), Some("s-1"), &error)
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["error"]["code"], "wall_time_limit_exceeded");
        assert_eq!(value["error"]["stateEffect"], "indeterminate");
    }

    #[test]
    fn legacy_error_envelope_is_byte_for_byte_unchanged_without_details() {
        let request = request("one");
        let error = ProtocolError::operation("evaluation_failed", "failure", "none");
        let mut bytes = Vec::new();
        ProtocolWriter::new(&mut bytes)
            .error(Some(&request), Some("s-1"), &error)
            .unwrap();

        assert_eq!(
            bytes,
            br#"{"v":1,"type":"response","wireSeq":"1","id":"one","sessionId":"s-1","event":null,"error":{"code":"evaluation_failed","message":"failure","fatal":false,"stateEffect":"none"}}
"#,
        );
    }

    #[test]
    fn structured_error_details_are_validated_bounded_and_emitted() {
        let request = request("one");
        let error = ProtocolError::operation("navigation_limit", "limit", "none")
            .with_details(json!({
                "actual": "21",
                "limit": 20,
                "reasons": ["replacement", null],
            }))
            .unwrap();
        let mut bytes = Vec::new();
        ProtocolWriter::new(&mut bytes)
            .error(Some(&request), Some("s-1"), &error)
            .unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();

        assert_eq!(value["error"]["details"]["actual"], "21");
        assert_eq!(value["error"]["details"]["limit"], 20);
        assert_eq!(value["error"]["details"]["reasons"][1], Value::Null);
        assert_eq!(
            ProtocolError::invalid_request("invalid")
                .with_details(json!(["not", "an", "object"]))
                .unwrap_err(),
            ProtocolErrorDetailsError::NotObject,
        );
        assert_eq!(
            ProtocolError::invalid_request("inexact")
                .with_details(json!({ "counter": 9_007_199_254_740_992u64 }))
                .unwrap_err(),
            ProtocolErrorDetailsError::IntegerNotExactlyRepresentable,
        );

        let oversized = "x".repeat(MAX_PROTOCOL_ERROR_DETAILS_BYTES);
        assert!(matches!(
            ProtocolError::invalid_request("large").with_details(json!({ "value": oversized })),
            Err(ProtocolErrorDetailsError::TooLarge { .. }),
        ));
    }
}
