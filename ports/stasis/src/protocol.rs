/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::io::{self, BufRead, Write};
use std::sync::mpsc::TryRecvError;
use std::sync::{Arc, Mutex};

use serde::Deserialize;
use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value, json};

use crate::wake::ShellWaker;

pub const PROTOCOL_VERSION: u8 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
pub const DEFAULT_ORDINARY_LANE_CAPACITY: usize = 8;
const CONTROL_LANE_CAPACITY: usize = 8;

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
    Fatal(ProtocolError),
    Eof,
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
        matches!(self, Self::Request(request) if request.is_control_lane())
    }
}

#[derive(Clone, Debug)]
pub struct ProtocolError {
    pub code: &'static str,
    pub message: String,
    pub fatal: bool,
    pub state_effect: &'static str,
}

impl ProtocolError {
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: "invalid_request",
            message: message.into(),
            fatal: false,
            state_effect: "none",
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
        }
    }

    fn fatal(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            fatal: true,
            state_effect: "none",
        }
    }
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
            let mut reader = ProtocolReader::new(input);
            loop {
                let message = match reader.next_request() {
                    Ok(Some(request)) => {
                        SequencedReaderMessage::request(request, reader.ingress_seq)
                    },
                    Ok(None) => SequencedReaderMessage::transport(ReaderMessage::Eof),
                    Err(error) => SequencedReaderMessage::transport(ReaderMessage::Fatal(error)),
                };
                match sender.enqueue(message) {
                    SendOutcome::Continue => waker.notify_protocol_input(),
                    SendOutcome::Stop => {
                        waker.notify_protocol_input();
                        return;
                    },
                    SendOutcome::Disconnected => return,
                }
            }
        })
        .expect("failed to spawn protocol reader")
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
                return Err(de::Error::custom(format_args!(
                    "duplicate JSON object member {name:?}"
                )));
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
        self.write(json!({
            "v": PROTOCOL_VERSION,
            "type": if request.is_some() { "response" } else { "event" },
            "wireSeq": wire_seq,
            "id": request.map(|request| request.id.as_str()),
            "sessionId": session_id,
            "event": if request.is_none() { Some("protocol.fatal") } else { None::<&str> },
            "error": {
                "code": error.code,
                "message": error.message,
                "fatal": error.fatal,
                "stateEffect": error.state_effect,
            },
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
        assert!(top.message.contains("duplicate JSON object member \"id\""));

        let nested = parse(
            br#"{"v":1,"type":"request","id":"one","method":"x","params":{"a":1,"a":2}}
"#,
        )
        .unwrap_err();
        assert_eq!(nested.code, "invalid_json");
        assert!(
            nested
                .message
                .contains("duplicate JSON object member \"a\"")
        );
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
}
