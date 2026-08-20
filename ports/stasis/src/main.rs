/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

mod engine;
mod protocol;
mod wake;

use std::io;
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use url::Url;

use crate::engine::EngineSession;
use crate::protocol::{ProtocolError, ProtocolWriter, ReaderMessage, Request, spawn_reader};
use crate::wake::{ShellWaker, WaitError};

const SOURCE_IDENTITIES: &str = include_str!("../../../STASIS_UPSTREAM.toml");
const SESSION_ID: &str = "s-1";

fn main() {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let waker = ShellWaker::default();
    let (sender, receiver) = sync_channel(8);
    let _reader = spawn_reader(sender, waker.clone());
    let stdout = io::stdout();
    let mut shell = Shell {
        state: ShellState::Spawned,
        engine: None,
        receiver,
        waker,
        writer: ProtocolWriter::new(stdout.lock()),
    };

    if let Err(error) = shell.run() {
        eprintln!("stasis shell fatal error: {error}");
        std::process::exit(70);
    }
}

struct Shell<W> {
    state: ShellState,
    engine: Option<EngineSession>,
    receiver: Receiver<ReaderMessage>,
    waker: ShellWaker,
    writer: ProtocolWriter<W>,
}

impl<W: io::Write> Shell<W> {
    fn run(&mut self) -> Result<(), String> {
        loop {
            let observed = self.waker.snapshot();
            match self.receiver.try_recv() {
                Ok(ReaderMessage::Request(request)) => {
                    if self.handle(request)? {
                        return Ok(());
                    }
                },
                Ok(ReaderMessage::Fatal(error)) => {
                    self.writer
                        .error(None, self.session_id(), &error)
                        .map_err(|write_error| write_error.to_string())?;
                    return Err(error.message);
                },
                Ok(ReaderMessage::Eof) => {
                    self.close_engine();
                    return Ok(());
                },
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.close_engine();
                    return Ok(());
                },
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    let current = self
                        .waker
                        .wait_for_change(observed, Instant::now() + Duration::from_secs(86_400))
                        .map_err(|WaitError::DeadlineExceeded| {
                            "protocol owner loop safety deadline exceeded".to_string()
                        })?;
                    if current.servo_changed_since(observed)
                        && let Some(engine) = self.engine.as_ref()
                    {
                        engine.pump();
                    }
                },
            }
        }
    }

    fn handle(&mut self, request: Request) -> Result<bool, String> {
        if !request.params.is_object() {
            self.write_method_result(
                &request,
                Err(ProtocolError::invalid_request("params must be an object")),
            )?;
            return Ok(false);
        }
        let result = match request.method.as_str() {
            "protocol.initialize" => self.initialize(&request),
            "session.open" => self.open(&request),
            "dom.evaluate" => self.evaluate(&request),
            "session.close" => {
                let result = self.close(&request);
                let should_exit = result.is_ok();
                self.write_method_result(&request, result)?;
                return Ok(should_exit);
            },
            _ => Err(ProtocolError::invalid_request(format!(
                "unknown method {}",
                request.method
            ))),
        };
        self.write_method_result(&request, result)?;
        Ok(false)
    }

    fn initialize(&mut self, request: &Request) -> Result<Value, ProtocolError> {
        if self.state != ShellState::Spawned {
            return Err(invalid_state("protocol.initialize is only valid once"));
        }
        if request.session_id.is_some() {
            return Err(ProtocolError::invalid_request(
                "initialize must not include sessionId",
            ));
        }
        let params: InitializeParams = parse_params(request)?;
        if let Some(client) = params.client
            && (client.name.is_empty() || client.version.is_empty())
        {
            return Err(ProtocolError::invalid_request(
                "client name and version must not be empty",
            ));
        }
        self.state = ShellState::Initialized;
        Ok(json!({
            "protocolVersion": 1,
            "implementation": {
                "name": "stasis-shell",
                "version": env!("CARGO_PKG_VERSION"),
                "source": parse_source_identities(),
            },
            "capabilities": {
                "methods": [
                    "protocol.initialize",
                    "session.open",
                    "dom.evaluate",
                    "session.close"
                ],
                "clockModes": ["real"],
                "profiles": [],
                "settlement": false,
            },
            "limits": {
                "maxInboundFrameBytes": protocol::MAX_FRAME_BYTES,
                "maxActiveEngineRequests": 1,
            }
        }))
    }

    fn open(&mut self, request: &Request) -> Result<Value, ProtocolError> {
        if self.state != ShellState::Initialized {
            return Err(invalid_state("session.open requires an initialized shell"));
        }
        if request.session_id.is_some() {
            return Err(ProtocolError::invalid_request(
                "session.open must not include sessionId",
            ));
        }
        let params: OpenParams = parse_params(request)?;
        let url = Url::parse(&params.url)
            .map_err(|error| ProtocolError::invalid_request(format!("invalid URL: {error}")))?;
        let engine = EngineSession::open(url.clone(), self.waker.clone())
            .map_err(|error| error.to_protocol_error())?;
        let final_url = engine.url().unwrap_or_else(|| url.clone());
        self.engine.replace(engine);
        self.state = ShellState::Open;
        Ok(json!({
            "sessionId": SESSION_ID,
            "requestedUrl": url,
            "url": final_url,
            "boundary": "load_complete",
        }))
    }

    fn evaluate(&self, request: &Request) -> Result<Value, ProtocolError> {
        self.require_session(request)?;
        let params: EvaluateParams = parse_params(request)?;
        let value = self
            .engine
            .as_ref()
            .expect("open state has an engine")
            .evaluate(&params.expression)
            .map_err(|error| error.to_protocol_error())?;
        Ok(json!({"value": value}))
    }

    fn close(&mut self, request: &Request) -> Result<Value, ProtocolError> {
        self.require_session(request)?;
        let _: CloseParams = parse_params(request)?;
        self.close_engine();
        self.state = ShellState::Closed;
        Ok(json!({"state": "closed"}))
    }

    fn require_session(&self, request: &Request) -> Result<(), ProtocolError> {
        if self.state != ShellState::Open {
            return Err(invalid_state("method requires an open session"));
        }
        if request.session_id.as_deref() != Some(SESSION_ID) {
            return Err(ProtocolError::invalid_request(
                "request has a missing or stale sessionId",
            ));
        }
        Ok(())
    }

    fn close_engine(&mut self) {
        if let Some(mut engine) = self.engine.take() {
            engine.close();
        }
    }

    fn session_id(&self) -> Option<&'static str> {
        (self.state == ShellState::Open).then_some(SESSION_ID)
    }

    fn write_method_result(
        &mut self,
        request: &Request,
        result: Result<Value, ProtocolError>,
    ) -> Result<(), String> {
        let terminal_session_response =
            matches!(request.method.as_str(), "session.open" | "session.close") && result.is_ok();
        let session_id = if terminal_session_response {
            Some(SESSION_ID)
        } else {
            self.session_id()
        };
        match result {
            Ok(result) => self.writer.result(request, session_id, result),
            Err(error) => self.writer.error(Some(request), session_id, &error),
        }
        .map_err(|error| error.to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShellState {
    Spawned,
    Initialized,
    Open,
    Closed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InitializeParams {
    #[serde(default)]
    client: Option<ClientIdentity>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientIdentity {
    name: String,
    version: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenParams {
    url: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluateParams {
    expression: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CloseParams {}

fn parse_params<T: for<'de> Deserialize<'de>>(request: &Request) -> Result<T, ProtocolError> {
    serde_json::from_value(request.params.clone())
        .map_err(|error| ProtocolError::invalid_request(error.to_string()))
}

fn invalid_state(message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: "invalid_state",
        message: message.into(),
        fatal: false,
        state_effect: "none",
    }
}

fn parse_source_identities() -> Value {
    let mut identities = serde_json::Map::new();
    for line in SOURCE_IDENTITIES.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        identities.insert(
            key.trim().to_string(),
            Value::String(value.trim().trim_matches('"').to_string()),
        );
    }
    Value::Object(identities)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(method: &str, session_id: Option<&str>) -> Request {
        Request {
            v: 1,
            kind: "request".into(),
            id: "test-1".into(),
            session_id: session_id.map(str::to_owned),
            method: method.into(),
            params: json!({}),
        }
    }

    #[test]
    fn source_identity_manifest_contains_both_sources() {
        let identities = parse_source_identities();
        assert_eq!(identities["servo_revision"].as_str().unwrap().len(), 40);
        assert_eq!(identities["pliego_revision"].as_str().unwrap().len(), 40);
    }

    #[test]
    fn an_invalid_close_does_not_terminate_the_shell() {
        let (_sender, receiver) = sync_channel(1);
        let mut bytes = Vec::new();
        let mut shell = Shell {
            state: ShellState::Initialized,
            engine: None,
            receiver,
            waker: ShellWaker::default(),
            writer: ProtocolWriter::new(&mut bytes),
        };

        assert!(!shell.handle(request("session.close", None)).unwrap());
        assert_eq!(shell.state, ShellState::Initialized);
    }

    #[test]
    fn a_valid_close_is_terminal_and_keeps_the_session_id() {
        let (_sender, receiver) = sync_channel(1);
        let mut bytes = Vec::new();
        {
            let mut shell = Shell {
                state: ShellState::Open,
                engine: None,
                receiver,
                waker: ShellWaker::default(),
                writer: ProtocolWriter::new(&mut bytes),
            };

            assert!(
                shell
                    .handle(request("session.close", Some(SESSION_ID)))
                    .unwrap()
            );
            assert_eq!(shell.state, ShellState::Closed);
        }

        let response: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(response["sessionId"], SESSION_ID);
    }
}
