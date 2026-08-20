/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::io::{self, BufRead, Read, Write};
use std::sync::mpsc::SyncSender;

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::wake::ShellWaker;

pub const PROTOCOL_VERSION: u8 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

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

fn empty_object() -> Value {
    Value::Object(Map::new())
}

#[derive(Debug)]
pub enum ReaderMessage {
    Request(Request),
    Fatal(ProtocolError),
    Eof,
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

pub fn spawn_reader(
    sender: SyncSender<ReaderMessage>,
    waker: ShellWaker,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("stasis-protocol-reader".into())
        .spawn(move || {
            let stdin = io::stdin();
            let mut input = stdin.lock();
            loop {
                let message = match read_request(&mut input) {
                    Ok(Some(request)) => ReaderMessage::Request(request),
                    Ok(None) => ReaderMessage::Eof,
                    Err(error) => ReaderMessage::Fatal(error),
                };
                let terminal = matches!(message, ReaderMessage::Fatal(_) | ReaderMessage::Eof);
                if sender.send(message).is_err() {
                    return;
                }
                waker.notify_protocol_input();
                if terminal {
                    return;
                }
            }
        })
        .expect("failed to spawn protocol reader")
}

pub fn read_request(input: &mut impl BufRead) -> Result<Option<Request>, ProtocolError> {
    let mut frame = Vec::with_capacity(1024);
    let bytes_read = input
        .take((MAX_FRAME_BYTES + 1) as u64)
        .read_until(b'\n', &mut frame)
        .map_err(|error| ProtocolError::fatal("input_error", error.to_string()))?;
    if bytes_read == 0 {
        return Ok(None);
    }
    if frame.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::fatal(
            "frame_too_large",
            format!("input frame exceeds {MAX_FRAME_BYTES} bytes"),
        ));
    }
    if frame.last() != Some(&b'\n') {
        return Err(ProtocolError::fatal(
            "incomplete_frame",
            "stdin ended before the NDJSON frame terminator",
        ));
    }
    frame.pop();
    if frame.last() == Some(&b'\r') {
        frame.pop();
    }
    if frame.is_empty() {
        return Err(ProtocolError::fatal("empty_frame", "empty NDJSON frame"));
    }
    if frame.starts_with(&[0xef, 0xbb, 0xbf]) {
        return Err(ProtocolError::fatal(
            "unexpected_bom",
            "UTF-8 BOM is not allowed",
        ));
    }

    let request: Request = serde_json::from_slice(&frame)
        .map_err(|error| ProtocolError::fatal("invalid_json", error.to_string()))?;
    validate_envelope(&request)?;
    Ok(Some(request))
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
    wire_seq: u64,
}

impl<W: Write> ProtocolWriter<W> {
    pub fn new(output: W) -> Self {
        Self {
            output,
            wire_seq: 0,
        }
    }

    pub fn result(
        &mut self,
        request: &Request,
        session_id: Option<&str>,
        result: Value,
    ) -> io::Result<()> {
        let wire_seq = self.next_wire_seq();
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
        let wire_seq = self.next_wire_seq();
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

    fn next_wire_seq(&mut self) -> String {
        self.wire_seq = self
            .wire_seq
            .checked_add(1)
            .expect("wire sequence exhausted");
        self.wire_seq.to_string()
    }

    fn write(&mut self, frame: Value) -> io::Result<()> {
        let mut encoded = serde_json::to_vec(&frame)?;
        encoded.push(b'\n');
        self.output.write_all(&encoded)?;
        self.output.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(bytes: &[u8]) -> Result<Option<Request>, ProtocolError> {
        read_request(&mut Cursor::new(bytes))
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
