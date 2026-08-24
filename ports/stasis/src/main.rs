/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

mod engine;
mod protocol;
mod wake;

use std::io;
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use embedder_traits::document_automation::{DocumentAutomationError, DocumentAutomationResult};
use serde::Deserialize;
use serde_json::{Value, json};
use servo::document_control::{
    DocumentControlAction, DocumentControlAutomationKind, DocumentControlCommand,
    DocumentControlError, DocumentControlOutcome, DocumentControlReceiveOutcome,
};
use stasis_shell::{settle, wire};
use url::Url;

use crate::engine::{ControlOutcomeDisposition, EngineClockMode, EngineControlPoll, EngineSession};
use crate::protocol::{
    DEFAULT_ORDINARY_LANE_CAPACITY, OrdinaryRequestRemoval, ProtocolError, ProtocolWriter,
    ReaderInbox, ReaderMessage, Request, reader_channel, spawn_reader,
};
use crate::wake::{ShellWaker, WakeGeneration, WakeWaitError};

const SOURCE_IDENTITIES: &str = include_str!("../../../STASIS_UPSTREAM.toml");
const SESSION_ID: &str = "s-1";
const CONTROLLED_WEBAPP_V1_PROFILE: &str = "controlled-webapp-v1";
const CONTROL_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const CONTROLLED_OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const OWNER_LOOP_SAFETY_TIMEOUT: Duration = Duration::from_secs(86_400);

fn main() {
    // Claim the protocol pipe before starting any helper or Servo-owned threads. Descriptor 1 is
    // diagnostic-only after this point; only `ProtocolWriter` retains the original stdout.
    let stdout = stasis_shell::stdio::claim_protocol_stdout()
        .expect("failed to claim protocol stdout before starting Servo");

    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    let waker = ShellWaker::default();
    let wake_cursor = waker
        .snapshot_checked()
        .expect("fresh shell wake generations are available");
    let servo_cursor = wake_cursor;
    let (sender, inbox) = reader_channel(DEFAULT_ORDINARY_LANE_CAPACITY);
    let _reader = spawn_reader(sender, waker.clone());
    let mut shell: Shell<_, EngineSession> = Shell {
        state: ShellState::Spawned,
        engine: None,
        inbox,
        waker,
        wake_cursor,
        servo_cursor,
        writer: ProtocolWriter::new(stdout),
        active: None,
        projection: wire::WireProjectionContext::new(),
    };

    if let Err(error) = shell.run() {
        eprintln!("stasis shell fatal error: {error}");
        std::process::exit(70);
    }
}

struct Shell<W, E = EngineSession> {
    state: ShellState,
    engine: Option<E>,
    inbox: ReaderInbox,
    waker: ShellWaker,
    wake_cursor: WakeGeneration,
    servo_cursor: WakeGeneration,
    writer: ProtocolWriter<W>,
    active: Option<ActiveRequest>,
    projection: wire::WireProjectionContext,
}

trait EnginePort: Sized {
    fn open_session(
        url: Url,
        waker: ShellWaker,
        clock_mode: EngineClockMode,
    ) -> Result<Self, ProtocolError>;
    fn pump(&mut self);
    fn url(&self) -> Option<Url>;
    fn clock_mode(&self) -> EngineClockMode;
    fn evaluate(&self, expression: &str) -> Result<Value, ProtocolError>;
    fn submit_document_control(
        &mut self,
        command: DocumentControlCommand,
        timeout: Duration,
    ) -> Result<(), ProtocolError>;
    fn poll_control_operation(&mut self) -> EnginePortPoll;
    fn cancel_control_operation(&mut self) -> Option<EnginePortCompletion>;
    fn close(&mut self);
}

struct EnginePortCompletion {
    disposition: ControlOutcomeDisposition,
    outcome: DocumentControlReceiveOutcome,
}

enum EnginePortPoll {
    Idle,
    Pending { deadline: Instant },
    Complete(EnginePortCompletion),
}

impl EnginePort for EngineSession {
    fn open_session(
        url: Url,
        waker: ShellWaker,
        clock_mode: EngineClockMode,
    ) -> Result<Self, ProtocolError> {
        match clock_mode {
            EngineClockMode::Real => Self::open(url, waker),
            EngineClockMode::Controlled { .. } => Self::start(url, waker, clock_mode),
        }
        .map_err(|error| error.to_protocol_error())
    }

    fn pump(&mut self) {
        Self::pump(self);
    }

    fn url(&self) -> Option<Url> {
        Self::url(self)
    }

    fn clock_mode(&self) -> EngineClockMode {
        Self::clock_mode(self)
    }

    fn evaluate(&self, expression: &str) -> Result<Value, ProtocolError> {
        Self::evaluate(self, expression).map_err(|error| error.to_protocol_error())
    }

    fn submit_document_control(
        &mut self,
        command: DocumentControlCommand,
        timeout: Duration,
    ) -> Result<(), ProtocolError> {
        Self::submit_document_control(self, command, timeout)
            .map_err(|error| error.to_protocol_error())
    }

    fn poll_control_operation(&mut self) -> EnginePortPoll {
        match Self::poll_control_operation(self) {
            EngineControlPoll::Idle => EnginePortPoll::Idle,
            EngineControlPoll::Pending { deadline } => EnginePortPoll::Pending { deadline },
            EngineControlPoll::Complete(completion) => {
                EnginePortPoll::Complete(EnginePortCompletion {
                    disposition: completion.disposition(),
                    outcome: completion.into_receive_outcome(),
                })
            },
        }
    }

    fn cancel_control_operation(&mut self) -> Option<EnginePortCompletion> {
        Self::cancel_control_operation(self).map(|completion| EnginePortCompletion {
            disposition: completion.disposition(),
            outcome: completion.into_receive_outcome(),
        })
    }

    fn close(&mut self) {
        Self::close(self);
    }
}

struct ActiveRequest {
    request: Request,
    operation: ActiveOperation,
    started_at: Instant,
    in_flight: Option<DocumentControlCommand>,
    needs_initial_pump: bool,
    state_effect: RequestStateEffect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RequestStateEffect {
    None,
    Partial,
}

impl RequestStateEffect {
    const fn as_protocol_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Partial => "partial",
        }
    }
}

enum ActiveOperation {
    ControlledOpen(ControlledOpenState),
    Pending,
    AdvanceToNext(AdvanceToNextState),
    Settle(SettleState),
    Automation(AutomationState),
}

struct ControlledOpenState {
    requested_url: Url,
    current_url: Url,
    deadline: Instant,
    waiting: Option<ControlledOpenWait>,
    bootstrap_attempted: bool,
}

struct ControlledOpenWait {
    observed: WakeGeneration,
    retry_at: Instant,
}

enum AdvanceToNextState {
    Observing,
    Advancing { from_virtual_time_ns: u128 },
}

struct SettleState {
    coordinator: settle::SettleCoordinator,
    effective_policy: wire::ResolvedSettlePolicy,
    cumulative_external_io_wall_time: Duration,
    waiting: Option<SettleHostWait>,
}

struct AutomationState {
    kind: wire::PublicAutomationKind,
    /// Present while the shell is obtaining fresh private target authority. The resolved public
    /// data is consumed exactly once when the Observe response is bound into an engine request.
    unresolved: Option<wire::ResolvedAutomationParams>,
}

struct SettleHostWait {
    observed: WakeGeneration,
    started_at: Instant,
    deadline: Option<Instant>,
}

enum ActiveTransition {
    Submit(DocumentControlCommand),
    WaitForControlledOpen,
    Wait(settle::SettleWait),
    Complete(Value),
    Fail(ActiveFailure),
}

struct ActiveFailure {
    error: ProtocolError,
    fail_stop: bool,
}

impl<W: io::Write, E: EnginePort> Shell<W, E> {
    fn run(&mut self) -> Result<(), String> {
        let mut input_closed = false;
        loop {
            let cycle_observed = self.checked_wake_snapshot()?;
            self.wake_cursor = cycle_observed;
            let mut progressed = false;
            let mut inbox_empty = false;

            match self.inbox.try_recv_sequenced() {
                Ok(message) => {
                    progressed = true;
                    match message.message {
                        ReaderMessage::Eof => input_closed = true,
                        message => {
                            if self.handle_reader_message(message)? {
                                return Ok(());
                            }
                        },
                    }
                },
                Err(TryRecvError::Disconnected) => {
                    input_closed = true;
                    inbox_empty = true;
                },
                Err(TryRecvError::Empty) => inbox_empty = true,
            }

            // A response can race a Servo wake. Poll the old response first so a transition into
            // host-waiting state cannot consume the wake which makes that observation stale.
            let (control_progress, mut control_deadline) = self.poll_active_control()?;
            progressed |= control_progress;

            let before_pump = self.checked_wake_snapshot()?;
            let force_initial_pump = self
                .active
                .as_ref()
                .is_some_and(|active| active.needs_initial_pump);
            if self.engine.is_some() &&
                (force_initial_pump || before_pump.servo_changed_since(self.servo_cursor))
            {
                self.engine
                    .as_mut()
                    .expect("engine presence was checked")
                    .pump();
                if let Some(active) = self.active.as_mut() {
                    active.needs_initial_pump = false;
                }
                // Every Servo generation present in this pre-pump snapshot is now consumed. A
                // wake created during the pump remains different and is handled next cycle.
                self.servo_cursor = before_pump;

                let (post_pump_progress, post_pump_deadline) = self.poll_active_control()?;
                progressed |= post_pump_progress;
                control_deadline = post_pump_deadline.or(control_deadline);
            }

            let after_pump = self.checked_wake_snapshot()?;
            if self.service_active_host_wait(after_pump, Instant::now())? {
                progressed = true;
                let (_, deadline) = self.poll_active_control()?;
                control_deadline = deadline.or(control_deadline);
            }

            if input_closed && inbox_empty && self.active.is_none() {
                self.abortive_close();
                return Ok(());
            }

            let final_snapshot = self.checked_wake_snapshot()?;
            let changed_during_cycle = final_snapshot != cycle_observed;
            self.wake_cursor = final_snapshot;
            if progressed || changed_during_cycle {
                continue;
            }

            let deadline = self.next_wait_deadline(control_deadline, Instant::now());
            match self
                .waker
                .wait_for_change_checked(self.wake_cursor, deadline)
            {
                Ok(_) | Err(WakeWaitError::DeadlineExceeded) => {},
                Err(WakeWaitError::GenerationExhausted(exhaustion)) => {
                    return Err(format!(
                        "shell wake generation exhausted: {:?}",
                        exhaustion.source
                    ));
                },
            }
        }
    }

    fn handle_reader_message(&mut self, message: ReaderMessage) -> Result<bool, String> {
        match message {
            ReaderMessage::Request(request) => self.handle(request),
            ReaderMessage::Fatal(error) => {
                self.writer
                    .error(None, self.session_id(), &error)
                    .map_err(|write_error| write_error.to_string())?;
                self.abortive_close();
                Err(error.message)
            },
            ReaderMessage::Eof => {
                // The owner loop handles clean EOF as a drain state so already accepted requests
                // still receive their terminal frames.
                Ok(false)
            },
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

        // Reject a self-targeting cancellation before duplicate-active-id enforcement. A cancel
        // frame cannot make its own id ambiguous with the target it names, even when that id also
        // happens to be active.
        if request.method == "protocol.cancel" &&
            parse_params::<CancelParams>(&request)
                .is_ok_and(|params| params.request_id == request.id)
        {
            self.write_method_result(
                &request,
                Err(ProtocolError::invalid_request(
                    "a cancellation request cannot target its own id",
                )),
            )?;
            return Ok(false);
        }

        if self
            .active
            .as_ref()
            .is_some_and(|active| active.request.id == request.id)
        {
            let error = fatal_operation(
                "duplicate_request_id",
                "request id is already active",
                "none",
            );
            self.writer
                .error(None, self.session_id(), &error)
                .map_err(|write_error| write_error.to_string())?;
            self.abortive_close();
            self.state = ShellState::Closed;
            return Err("duplicate active request id".into());
        }

        match request.method.as_str() {
            "protocol.cancel" => return self.cancel(request),
            "session.close" => return self.close(request),
            _ => {},
        }

        if self.active.is_some() {
            self.write_method_result(
                &request,
                Err(ProtocolError::operation(
                    "busy",
                    "another engine request is active",
                    "none",
                )),
            )?;
            return Ok(false);
        }

        let method = request.method.clone();
        if matches!(
            method.as_str(),
            "runtime.pending" | "runtime.settle" | "runtime.advance_to_next"
        ) {
            return self.begin_runtime_request(request).map(|()| false);
        }
        if matches!(
            method.as_str(),
            "action.activate" | "action.fill" | "dom.query" | "dom.text" | "dom.extract"
        ) {
            return self.begin_automation_request(request).map(|()| false);
        }
        if method == "session.open" {
            return self.begin_open(request).map(|()| false);
        }

        let result = match method.as_str() {
            "protocol.initialize" => self.initialize(&request),
            "dom.evaluate" => self.evaluate(&request),
            _ => Err(ProtocolError::invalid_request(format!(
                "unknown method {}",
                request.method
            ))),
        };
        self.write_method_result(&request, result)?;
        Ok(false)
    }

    fn begin_runtime_request(&mut self, request: Request) -> Result<(), String> {
        let validation = self.require_controlled_session(&request);
        if let Err(error) = validation {
            return self.write_method_result(&request, Err(error));
        }

        let started_at = Instant::now();
        let method = request.method.clone();
        let (operation, first_progress) = match method.as_str() {
            "runtime.pending" => {
                let params = parse_params::<wire::RuntimePendingParams>(&request);
                if let Err(error) = params {
                    return self.write_method_result(&request, Err(error));
                }
                (
                    ActiveOperation::Pending,
                    ActiveTransition::Submit(DocumentControlCommand::Observe),
                )
            },
            "runtime.advance_to_next" => {
                let params = parse_params::<wire::RuntimeAdvanceToNextParams>(&request);
                if let Err(error) = params {
                    return self.write_method_result(&request, Err(error));
                }
                (
                    ActiveOperation::AdvanceToNext(AdvanceToNextState::Observing),
                    ActiveTransition::Submit(DocumentControlCommand::Observe),
                )
            },
            "runtime.settle" => {
                let params = match parse_params::<wire::RuntimeSettleParams>(&request) {
                    Ok(params) => params,
                    Err(error) => return self.write_method_result(&request, Err(error)),
                };
                let effective_policy = match params.resolve(settle::SettlePolicy::default()) {
                    Ok(policy) => policy,
                    Err(error) => {
                        return self.write_method_result(
                            &request,
                            Err(ProtocolError::invalid_request(format!(
                                "invalid settlement policy: {error:?}"
                            ))),
                        );
                    },
                };
                let mut coordinator = settle::SettleCoordinator::new(effective_policy.engine);
                let progress = match coordinator.start() {
                    Ok(progress) => transition_from_settle_progress(progress),
                    Err(error) => ActiveTransition::Fail(settle_failure(
                        error,
                        RequestStateEffect::None,
                        None,
                    )),
                };
                (
                    ActiveOperation::Settle(SettleState {
                        coordinator,
                        effective_policy,
                        cumulative_external_io_wall_time: Duration::ZERO,
                        waiting: None,
                    }),
                    progress,
                )
            },
            _ => unreachable!("runtime method was filtered above"),
        };

        let active = ActiveRequest {
            request,
            operation,
            started_at,
            in_flight: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::None,
        };
        self.apply_active_transition(active, first_progress)
    }

    fn begin_automation_request(&mut self, request: Request) -> Result<(), String> {
        if let Err(error) = self.require_controlled_session(&request) {
            return self.write_method_result(&request, Err(error));
        }

        let resolved = match request.method.as_str() {
            "action.activate" => {
                parse_params::<wire::ActionActivateParams>(&request).and_then(|params| {
                    params.resolve().map_err(|error| {
                        ProtocolError::invalid_request(format!(
                            "invalid action.activate parameters: {error:?}"
                        ))
                    })
                })
            },
            "action.fill" => parse_params::<wire::ActionFillParams>(&request).and_then(|params| {
                params.resolve().map_err(|error| {
                    ProtocolError::invalid_request(format!(
                        "invalid action.fill parameters: {error:?}"
                    ))
                })
            }),
            "dom.query" => parse_params::<wire::DomQueryParams>(&request).and_then(|params| {
                params.resolve().map_err(|error| {
                    ProtocolError::invalid_request(format!(
                        "invalid dom.query parameters: {error:?}"
                    ))
                })
            }),
            "dom.text" => parse_params::<wire::DomTextParams>(&request).and_then(|params| {
                params.resolve().map_err(|error| {
                    ProtocolError::invalid_request(format!(
                        "invalid dom.text parameters: {error:?}"
                    ))
                })
            }),
            "dom.extract" => parse_params::<wire::DomExtractParams>(&request).and_then(|params| {
                params.resolve().map_err(|error| {
                    ProtocolError::invalid_request(format!(
                        "invalid dom.extract parameters: {error:?}"
                    ))
                })
            }),
            _ => unreachable!("automation method was filtered by the dispatcher"),
        };
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(error) => return self.write_method_result(&request, Err(error)),
        };
        let kind = resolved.kind();
        let active = ActiveRequest {
            request,
            operation: ActiveOperation::Automation(AutomationState {
                kind,
                unresolved: Some(resolved),
            }),
            started_at: Instant::now(),
            in_flight: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::None,
        };
        self.apply_active_transition(
            active,
            ActiveTransition::Submit(DocumentControlCommand::Observe),
        )
    }

    fn poll_active_control(&mut self) -> Result<(bool, Option<Instant>), String> {
        let Some(active) = self.active.as_ref() else {
            return Ok((false, None));
        };
        if active.in_flight.is_none() {
            return Ok((false, None));
        }
        let poll = self
            .engine
            .as_mut()
            .expect("an active runtime request has an engine")
            .poll_control_operation();
        match poll {
            EnginePortPoll::Pending { deadline } => Ok((false, Some(deadline))),
            EnginePortPoll::Idle => {
                let active = self.active.take().expect("active request was observed");
                self.apply_active_transition(
                    active,
                    ActiveTransition::Fail(ActiveFailure {
                        error: fatal_operation(
                            "internal_runtime_failure",
                            "engine lost an in-flight document-control receiver",
                            "indeterminate",
                        ),
                        fail_stop: true,
                    }),
                )?;
                Ok((true, None))
            },
            EnginePortPoll::Complete(completion) => {
                let mut active = self.active.take().expect("active request was observed");
                let command = active
                    .in_flight
                    .take()
                    .expect("active request tracked its in-flight command");
                active.needs_initial_pump = false;
                if let ActiveOperation::ControlledOpen(state) = &mut active.operation &&
                    let Some(url) = self.engine.as_ref().and_then(|engine| engine.url())
                {
                    state.current_url = url;
                }
                if completion.disposition == ControlOutcomeDisposition::Indeterminate {
                    self.apply_active_transition(
                        active,
                        ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "outcome_indeterminate",
                                "document-control mutation may have completed without a response",
                                "indeterminate",
                            ),
                            fail_stop: true,
                        }),
                    )?;
                    return Ok((true, None));
                }
                if completion.disposition == ControlOutcomeDisposition::Completed &&
                    command_is_mutating(&command)
                {
                    active.state_effect = RequestStateEffect::Partial;
                }
                let transition = transition_from_control_completion(
                    &mut active,
                    command,
                    completion.outcome,
                    &mut self.projection,
                );
                self.apply_active_transition(active, transition)?;
                Ok((true, None))
            },
        }
    }

    fn service_active_host_wait(
        &mut self,
        current: WakeGeneration,
        now: Instant,
    ) -> Result<bool, String> {
        let Some(active) = self.active.as_ref() else {
            return Ok(false);
        };
        if let ActiveOperation::ControlledOpen(state) = &active.operation {
            let Some(wait) = state.waiting.as_ref() else {
                return Ok(false);
            };
            let deadline_expired = now >= state.deadline;
            let retry_ready = now >= wait.retry_at;
            let servo_woke = current.servo_changed_since(wait.observed);
            if !deadline_expired && !retry_ready && !servo_woke {
                return Ok(false);
            }

            let mut active = self
                .active
                .take()
                .expect("controlled open wait was observed");
            let ActiveOperation::ControlledOpen(state) = &mut active.operation else {
                unreachable!("controlled open wait changed operation kind")
            };
            state.waiting = None;
            let transition = if deadline_expired {
                ActiveTransition::Fail(ActiveFailure {
                    error: ProtocolError::operation(
                        "controlled_open_timeout",
                        "the controlled document did not become ready before the wall deadline",
                        "none",
                    ),
                    fail_stop: false,
                })
            } else {
                ActiveTransition::Submit(DocumentControlCommand::Observe)
            };
            self.apply_active_transition(active, transition)?;
            return Ok(true);
        }
        let ActiveOperation::Settle(settle) = &active.operation else {
            return Ok(false);
        };
        let Some(wait) = settle.waiting.as_ref() else {
            return Ok(false);
        };
        let expired = wait.deadline.is_some_and(|deadline| now >= deadline);
        let servo_woke = current.servo_changed_since(wait.observed);
        if !expired && !servo_woke {
            return Ok(false);
        }

        let mut active = self.active.take().expect("settle wait was observed");
        let state_effect = active.state_effect;
        let started_at = active.started_at;
        let (wait, previous_cumulative) = {
            let ActiveOperation::Settle(state) = &mut active.operation else {
                unreachable!("settle wait changed operation kind")
            };
            (
                state.waiting.take().expect("settle wait was observed"),
                state.cumulative_external_io_wall_time,
            )
        };
        let elapsed = now.saturating_duration_since(wait.started_at);
        let Some(cumulative) = previous_cumulative.checked_add(elapsed) else {
            return self
                .apply_active_transition(
                    active,
                    ActiveTransition::Fail(ActiveFailure {
                        error: ProtocolError::operation(
                            "settlement_wall_time_overflow",
                            "external-I/O wall-time accounting overflowed",
                            state_effect.as_protocol_str(),
                        ),
                        fail_stop: false,
                    }),
                )
                .map(|()| true);
        };
        let ActiveOperation::Settle(state) = &mut active.operation else {
            unreachable!("settle wait changed operation kind")
        };
        state.cumulative_external_io_wall_time = cumulative;
        let progress = if expired {
            state.coordinator.external_io_wait_expired(cumulative)
        } else {
            state.coordinator.resume_after_wake(cumulative)
        };
        let transition = match progress {
            Ok(progress) => transition_from_settle_progress_for_active(
                state,
                started_at,
                progress,
                state_effect,
                &mut self.projection,
            ),
            Err(error) => ActiveTransition::Fail(settle_failure(error, state_effect, None)),
        };
        self.apply_active_transition(active, transition)?;
        Ok(true)
    }

    fn apply_active_transition(
        &mut self,
        mut active: ActiveRequest,
        transition: ActiveTransition,
    ) -> Result<(), String> {
        match transition {
            ActiveTransition::Submit(command) => {
                let controlled_open =
                    matches!(&active.operation, ActiveOperation::ControlledOpen(_));
                let timeout = if let ActiveOperation::ControlledOpen(state) = &active.operation {
                    let Some(remaining) = state.deadline.checked_duration_since(Instant::now())
                    else {
                        return self.apply_active_transition(
                            active,
                            ActiveTransition::Fail(ActiveFailure {
                                error: ProtocolError::operation(
                                    "controlled_open_timeout",
                                    "the controlled document did not become ready before the wall deadline",
                                    "none",
                                ),
                                fail_stop: false,
                            }),
                        );
                    };
                    remaining
                } else {
                    CONTROL_COMMAND_TIMEOUT
                };
                let submission = self
                    .engine
                    .as_mut()
                    .expect("runtime request has an engine")
                    .submit_document_control(command.clone(), timeout);
                match submission {
                    Ok(()) => {
                        active.in_flight = Some(command);
                        active.needs_initial_pump = true;
                        if let ActiveOperation::Settle(state) = &mut active.operation {
                            state.waiting = None;
                        }
                        self.active = Some(active);
                        Ok(())
                    },
                    Err(mut error) => {
                        error.state_effect = active.state_effect.as_protocol_str();
                        if controlled_open {
                            self.close_engine();
                            self.state = ShellState::Initialized;
                        }
                        self.write_method_result(&active.request, Err(error))
                    },
                }
            },
            ActiveTransition::WaitForControlledOpen => {
                let now = Instant::now();
                let ActiveOperation::ControlledOpen(state) = &mut active.operation else {
                    return self.apply_active_transition(
                        active,
                        ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "internal_runtime_failure",
                                "non-open request entered a controlled-open readiness wait",
                                "none",
                            ),
                            fail_stop: true,
                        }),
                    );
                };
                state.waiting = Some(ControlledOpenWait {
                    observed: self.servo_cursor,
                    retry_at: now
                        .checked_add(CONTROLLED_OPEN_RETRY_INTERVAL)
                        .unwrap_or(state.deadline),
                });
                self.active = Some(active);
                Ok(())
            },
            ActiveTransition::Wait(wait) => {
                let state_effect = active.state_effect;
                let now = Instant::now();
                let deadline = match wait {
                    settle::SettleWait::ForegroundExternalIo {
                        remaining_wall_time,
                        ..
                    } => {
                        let Some(deadline) = now.checked_add(remaining_wall_time) else {
                            return self.apply_active_transition(
                                active,
                                ActiveTransition::Fail(ActiveFailure {
                                    error: ProtocolError::operation(
                                        "settlement_deadline_overflow",
                                        "external-I/O wait deadline overflowed",
                                        state_effect.as_protocol_str(),
                                    ),
                                    fail_stop: false,
                                }),
                            );
                        };
                        Some(deadline)
                    },
                    settle::SettleWait::ProducerHandoff {
                        remaining_wall_time,
                        ..
                    } => {
                        let Some(deadline) = now.checked_add(remaining_wall_time) else {
                            return self.apply_active_transition(
                                active,
                                ActiveTransition::Fail(ActiveFailure {
                                    error: ProtocolError::operation(
                                        "settlement_deadline_overflow",
                                        "producer-handoff wait deadline overflowed",
                                        state_effect.as_protocol_str(),
                                    ),
                                    fail_stop: false,
                                }),
                            );
                        };
                        Some(deadline)
                    },
                };
                let ActiveOperation::Settle(state) = &mut active.operation else {
                    return self.apply_active_transition(
                        active,
                        ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "internal_runtime_failure",
                                "non-settlement request entered a settlement wait",
                                "none",
                            ),
                            fail_stop: true,
                        }),
                    );
                };
                state.waiting = Some(SettleHostWait {
                    observed: self.servo_cursor,
                    started_at: now,
                    deadline,
                });
                self.active = Some(active);
                Ok(())
            },
            ActiveTransition::Complete(value) => {
                self.write_method_result(&active.request, Ok(value))
            },
            ActiveTransition::Fail(failure) => {
                let controlled_open =
                    matches!(&active.operation, ActiveOperation::ControlledOpen(_));
                if controlled_open {
                    self.close_engine();
                    self.state = ShellState::Initialized;
                }
                self.write_method_result(&active.request, Err(failure.error))?;
                if failure.fail_stop {
                    self.abortive_close();
                    self.state = ShellState::Closed;
                    return Err("runtime outcome is indeterminate; session was fail-stopped".into());
                }
                Ok(())
            },
        }
    }

    fn cancel(&mut self, request: Request) -> Result<bool, String> {
        if let Err(error) = self.require_session(&request) {
            self.write_method_result(&request, Err(error))?;
            return Ok(false);
        }
        let params = match parse_params::<CancelParams>(&request) {
            Ok(params) => params,
            Err(error) => {
                self.write_method_result(&request, Err(error))?;
                return Ok(false);
            },
        };
        if request.id == params.request_id {
            self.write_method_result(
                &request,
                Err(ProtocolError::invalid_request(
                    "a cancellation request cannot target its own id",
                )),
            )?;
            return Ok(false);
        }

        if self
            .active
            .as_ref()
            .is_some_and(|active| active.request.id == params.request_id)
        {
            let active = self.active.take().expect("active target was observed");
            let controlled_open = matches!(&active.operation, ActiveOperation::ControlledOpen(_));
            let failure = self.cancel_active_failure(&active);
            self.writer
                .result(&request, self.session_id(), json!({"accepted": true}))
                .map_err(|error| error.to_string())?;
            self.write_method_result(&active.request, Err(failure.error))?;
            if controlled_open {
                self.close_engine();
                self.state = ShellState::Initialized;
            }
            if failure.fail_stop {
                self.abortive_close();
                self.state = ShellState::Closed;
                return Err("cancelled control outcome is indeterminate".into());
            }
            return Ok(false);
        }

        match self.inbox.remove_ordinary_request(&params.request_id) {
            OrdinaryRequestRemoval::Removed(removed) => {
                let ReaderMessage::Request(target) = removed.message else {
                    unreachable!("ordinary removal returned a transport message")
                };
                self.writer
                    .result(&request, self.session_id(), json!({"accepted": true}))
                    .map_err(|error| error.to_string())?;
                self.write_method_result(
                    &target,
                    Err(ProtocolError::operation(
                        "cancelled",
                        "request was cancelled before it started",
                        "none",
                    )),
                )?;
            },
            OrdinaryRequestRemoval::NotFound => {
                self.writer
                    .result(&request, self.session_id(), json!({"accepted": false}))
                    .map_err(|error| error.to_string())?;
            },
            OrdinaryRequestRemoval::Ambiguous => {
                let error = fatal_operation(
                    "duplicate_request_id",
                    "cancellation target matches more than one queued request",
                    "none",
                );
                self.write_method_result(&request, Err(error))?;
                self.abortive_close();
                self.state = ShellState::Closed;
                return Err("ambiguous queued cancellation target".into());
            },
        }
        Ok(false)
    }

    fn cancel_active_failure(&mut self, active: &ActiveRequest) -> ActiveFailure {
        let command_is_mutating = active.in_flight.as_ref().is_some_and(command_is_mutating);
        let completion = self
            .engine
            .as_mut()
            .and_then(|engine| engine.cancel_control_operation());
        if command_is_mutating ||
            completion.as_ref().is_some_and(|completion| {
                completion.disposition == ControlOutcomeDisposition::Indeterminate
            })
        {
            return ActiveFailure {
                error: fatal_operation(
                    "outcome_indeterminate",
                    "cancellation abandoned a mutating command response",
                    "indeterminate",
                ),
                fail_stop: true,
            };
        }
        ActiveFailure {
            error: ProtocolError::operation(
                "cancelled",
                "request was cancelled",
                active.state_effect.as_protocol_str(),
            ),
            fail_stop: false,
        }
    }

    fn close(&mut self, request: Request) -> Result<bool, String> {
        if let Err(error) = self.require_session(&request) {
            self.write_method_result(&request, Err(error))?;
            return Ok(false);
        }
        if let Err(error) = parse_params::<CloseParams>(&request) {
            self.write_method_result(&request, Err(error))?;
            return Ok(false);
        }

        if let Some(active) = self.active.take() {
            let failure = self.cancel_active_failure(&active);
            self.write_method_result(&active.request, Err(failure.error))?;
        }
        self.drain_queued_for_close()?;
        self.close_engine();
        self.state = ShellState::Closed;
        self.write_method_result(&request, Ok(json!({"state": "closed"})))?;
        Ok(true)
    }

    fn drain_queued_for_close(&mut self) -> Result<(), String> {
        loop {
            match self.inbox.try_recv_sequenced() {
                Ok(message) => match message.message {
                    ReaderMessage::Request(request) => self.write_method_result(
                        &request,
                        Err(ProtocolError::operation(
                            "session_closing",
                            "session closed before the request started",
                            "none",
                        )),
                    )?,
                    ReaderMessage::Fatal(error) => {
                        self.writer
                            .error(None, self.session_id(), &error)
                            .map_err(|write_error| write_error.to_string())?;
                        return Err(error.message);
                    },
                    // EOF is a drain marker, not proof that lower-priority lanes are empty: it
                    // can overtake ordinary requests which were accepted before session.close.
                    ReaderMessage::Eof => continue,
                },
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return Ok(()),
            }
        }
    }

    fn next_wait_deadline(&self, control_deadline: Option<Instant>, now: Instant) -> Instant {
        let safety = now.checked_add(OWNER_LOOP_SAFETY_TIMEOUT).unwrap_or(now);
        let host_wait = self
            .active
            .as_ref()
            .and_then(|active| match &active.operation {
                ActiveOperation::ControlledOpen(state) => {
                    Some(state.waiting.as_ref().map_or(state.deadline, |wait| {
                        Instant::min(wait.retry_at, state.deadline)
                    }))
                },
                ActiveOperation::Settle(state) => {
                    state.waiting.as_ref().and_then(|wait| wait.deadline)
                },
                ActiveOperation::Pending |
                ActiveOperation::AdvanceToNext(_) |
                ActiveOperation::Automation(_) => None,
            });
        [control_deadline, host_wait]
            .into_iter()
            .flatten()
            .fold(safety, Instant::min)
    }

    fn checked_wake_snapshot(&self) -> Result<WakeGeneration, String> {
        self.waker
            .snapshot_checked()
            .map_err(|exhaustion| format!("shell wake generation exhausted: {exhaustion:?}"))
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
        if let Some(client) = params.client &&
            (client.name.is_empty() || client.version.is_empty())
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
                    "runtime.pending",
                    "runtime.settle",
                    "runtime.advance_to_next",
                    "action.activate",
                    "action.fill",
                    "dom.query",
                    "dom.text",
                    "dom.extract",
                    "protocol.cancel",
                    "session.close"
                ],
                "clockModes": ["real", "controlled"],
                "profiles": [CONTROLLED_WEBAPP_V1_PROFILE],
                "settlement": true,
                "settlementLimits": [
                    "maxVirtualTimeNs",
                    "maxControlTurns",
                    "wallIoTimeoutNs"
                ],
            },
            "limits": {
                "maxInboundFrameBytes": protocol::MAX_FRAME_BYTES,
                "maxActiveEngineRequests": 1,
            }
        }))
    }

    fn begin_open(&mut self, request: Request) -> Result<(), String> {
        if self.state != ShellState::Initialized {
            return self.write_method_result(
                &request,
                Err(invalid_state("session.open requires an initialized shell")),
            );
        }
        if request.session_id.is_some() {
            return self.write_method_result(
                &request,
                Err(ProtocolError::invalid_request(
                    "session.open must not include sessionId",
                )),
            );
        }
        let params: OpenParams = match parse_params(&request) {
            Ok(params) => params,
            Err(error) => return self.write_method_result(&request, Err(error)),
        };
        let url = match Url::parse(&params.url) {
            Ok(url) => url,
            Err(error) => {
                return self.write_method_result(
                    &request,
                    Err(ProtocolError::invalid_request(format!(
                        "invalid URL: {error}"
                    ))),
                );
            },
        };
        let (clock_mode, boundary) = match params.clock_mode() {
            Ok(mode) => mode,
            Err(error) => return self.write_method_result(&request, Err(error)),
        };
        let mut engine = match E::open_session(url.clone(), self.waker.clone(), clock_mode) {
            Ok(engine) => engine,
            Err(error) => return self.write_method_result(&request, Err(error)),
        };
        let final_url = engine.url().unwrap_or_else(|| url.clone());

        if clock_mode.is_controlled() {
            let started_at = Instant::now();
            let Some(deadline) = started_at.checked_add(CONTROL_COMMAND_TIMEOUT) else {
                engine.close();
                return self.write_method_result(
                    &request,
                    Err(ProtocolError::operation(
                        "controlled_open_deadline_overflow",
                        "the controlled-open wall deadline overflowed",
                        "none",
                    )),
                );
            };
            if let Err(mut error) = engine.submit_document_control(
                DocumentControlCommand::Observe,
                deadline.saturating_duration_since(Instant::now()),
            ) {
                error.state_effect = "none";
                engine.close();
                return self.write_method_result(&request, Err(error));
            }
            self.projection = wire::WireProjectionContext::new();
            self.engine.replace(engine);
            self.state = ShellState::Open;
            self.active = Some(ActiveRequest {
                request,
                operation: ActiveOperation::ControlledOpen(ControlledOpenState {
                    requested_url: url,
                    current_url: final_url,
                    deadline,
                    waiting: None,
                    bootstrap_attempted: false,
                }),
                started_at,
                in_flight: Some(DocumentControlCommand::Observe),
                needs_initial_pump: true,
                state_effect: RequestStateEffect::None,
            });
            return Ok(());
        }

        self.engine.replace(engine);
        self.state = ShellState::Open;
        self.write_method_result(
            &request,
            Ok(json!({
                "sessionId": SESSION_ID,
                "requestedUrl": url,
                "url": final_url,
                "boundary": boundary,
                "clockMode": "real",
                "profile": null,
            })),
        )
    }

    fn evaluate(&self, request: &Request) -> Result<Value, ProtocolError> {
        self.require_session(request)?;
        let params: EvaluateParams = parse_params(request)?;
        let value = self
            .engine
            .as_ref()
            .expect("open state has an engine")
            .evaluate(&params.expression)?;
        Ok(json!({"value": value}))
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

    fn require_controlled_session(&self, request: &Request) -> Result<(), ProtocolError> {
        self.require_session(request)?;
        if !self
            .engine
            .as_ref()
            .expect("open state has an engine")
            .clock_mode()
            .is_controlled()
        {
            return Err(ProtocolError::operation(
                "controlled_clock_required",
                "this method requires a controlled session",
                "none",
            ));
        }
        Ok(())
    }

    fn close_engine(&mut self) {
        if let Some(mut engine) = self.engine.take() {
            engine.close();
        }
    }

    fn abortive_close(&mut self) {
        if let Some(mut engine) = self.engine.take() {
            let _ = engine.cancel_control_operation();
            engine.close();
        }
        self.active.take();
    }

    fn session_id(&self) -> Option<&'static str> {
        (self.state == ShellState::Open).then_some(SESSION_ID)
    }

    fn write_method_result(
        &mut self,
        request: &Request,
        result: Result<Value, ProtocolError>,
    ) -> Result<(), String> {
        let session_id = match (request.method.as_str(), result.is_ok()) {
            ("session.open", true) | ("session.close", true) => Some(SESSION_ID),
            ("session.open", false) => None,
            _ => self.session_id(),
        };
        match result {
            Ok(result) => self.writer.result(request, session_id, result),
            Err(error) => self.writer.error(Some(request), session_id, &error),
        }
        .map_err(|error| error.to_string())
    }
}

fn transition_from_control_completion(
    active: &mut ActiveRequest,
    command: DocumentControlCommand,
    outcome: DocumentControlReceiveOutcome,
    projection: &mut wire::WireProjectionContext,
) -> ActiveTransition {
    let state_effect = active.state_effect;
    match &mut active.operation {
        ActiveOperation::ControlledOpen(state) => {
            let observation = match command {
                DocumentControlCommand::Observe => {
                    if matches!(
                        &outcome,
                        DocumentControlReceiveOutcome::CommandOutcome(
                            DocumentControlOutcome::Rejected(
                                DocumentControlError::EventLoopUnavailable
                            )
                        )
                    ) {
                        return ActiveTransition::WaitForControlledOpen;
                    }
                    if let DocumentControlReceiveOutcome::CommandOutcome(
                        DocumentControlOutcome::Rejected(
                            DocumentControlError::InitialPipelineBootstrapRequired { pipeline_id },
                        ),
                    ) = &outcome
                    {
                        if state.bootstrap_attempted {
                            return ActiveTransition::Fail(ActiveFailure {
                                error: fatal_operation(
                                    "internal_runtime_failure",
                                    "controlled open requested more than one initial pipeline bootstrap",
                                    state_effect.as_protocol_str(),
                                ),
                                fail_stop: true,
                            });
                        }
                        state.bootstrap_attempted = true;
                        return ActiveTransition::Submit(
                            DocumentControlCommand::BootstrapInitialPipeline {
                                pipeline_id: *pipeline_id,
                            },
                        );
                    }
                    match completed_observation(
                        outcome,
                        &DocumentControlCommand::Observe,
                        state_effect,
                    ) {
                        Ok(observation) => observation,
                        Err(failure) => return ActiveTransition::Fail(failure),
                    }
                },
                bootstrap @ DocumentControlCommand::BootstrapInitialPipeline { .. }
                    if state.bootstrap_attempted =>
                {
                    let observation = match completed_observation(outcome, &bootstrap, state_effect)
                    {
                        Ok(observation) => observation,
                        Err(failure) => return ActiveTransition::Fail(failure),
                    };
                    if !matches!(
                        observation.action(),
                        DocumentControlAction::TurnProcessed { .. }
                    ) {
                        return ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "internal_runtime_failure",
                                "initial pipeline bootstrap did not process its exact lifecycle event",
                                state_effect.as_protocol_str(),
                            ),
                            fail_stop: true,
                        });
                    }
                    // This completion admitted only the exact root SpawnPipeline. Its pending
                    // target can still have no active Document; normal settlement later waits for
                    // and drives the correlated navigation-response headers.
                    observation
                },
                _ => {
                    return ActiveTransition::Fail(ActiveFailure {
                        error: fatal_operation(
                            "internal_runtime_failure",
                            "controlled-open readiness used an unauthorized command",
                            state_effect.as_protocol_str(),
                        ),
                        fail_stop: true,
                    });
                },
            };
            let _ = observation;
            ActiveTransition::Complete(json!({
                "sessionId": SESSION_ID,
                "requestedUrl": state.requested_url,
                "url": state.current_url,
                "boundary": "controlled_ready",
                "clockMode": "controlled",
                "profile": CONTROLLED_WEBAPP_V1_PROFILE,
            }))
        },
        ActiveOperation::Pending => {
            let observation = match completed_observation(outcome, &command, state_effect) {
                Ok(observation) => observation,
                Err(failure) => return ActiveTransition::Fail(failure),
            };
            serialize_result(
                wire::RuntimePendingResult::project(observation.pending(), projection),
                state_effect,
            )
        },
        ActiveOperation::AdvanceToNext(state) => {
            let observation = match completed_observation(outcome, &command, state_effect) {
                Ok(observation) => observation,
                Err(failure) => return ActiveTransition::Fail(failure),
            };
            match state {
                AdvanceToNextState::Observing => {
                    if observation.pending().scheduler.next_deadline.is_none() {
                        return serialize_result(
                            wire::RuntimeAdvanceToNextResult::project(
                                wire::RuntimeAdvanceToNextFacts::NoFiniteDeadline {
                                    final_snapshot: observation.pending(),
                                },
                                projection,
                            ),
                            state_effect,
                        );
                    }
                    let Some(token) = observation.advance_token().cloned() else {
                        return ActiveTransition::Fail(ActiveFailure {
                            error: ProtocolError::operation(
                                "advance_not_available",
                                "the finite scheduler head is not currently safe to advance",
                                state_effect.as_protocol_str(),
                            ),
                            fail_stop: false,
                        });
                    };
                    let from_virtual_time_ns = observation.pending().clock.now.as_nanos();
                    *state = AdvanceToNextState::Advancing {
                        from_virtual_time_ns,
                    };
                    ActiveTransition::Submit(DocumentControlCommand::AdvanceTo(Box::new(token)))
                },
                AdvanceToNextState::Advancing {
                    from_virtual_time_ns,
                } => serialize_result(
                    wire::RuntimeAdvanceToNextResult::project(
                        wire::RuntimeAdvanceToNextFacts::Advanced {
                            from_virtual_time_ns: *from_virtual_time_ns,
                            final_snapshot: observation.pending(),
                        },
                        projection,
                    ),
                    state_effect,
                ),
            }
        },
        ActiveOperation::Automation(state) => {
            if let Some(resolved) = state.unresolved.take() {
                if command != DocumentControlCommand::Observe {
                    return ActiveTransition::Fail(ActiveFailure {
                        error: fatal_operation(
                            "internal_runtime_failure",
                            "automation target binding did not use a fresh observation",
                            state_effect.as_protocol_str(),
                        ),
                        fail_stop: true,
                    });
                }
                let observation = match completed_observation(outcome, &command, state_effect) {
                    Ok(observation) => observation,
                    Err(failure) => return ActiveTransition::Fail(failure),
                };
                let request = match resolved.bind_to_target(observation.pending().target.clone()) {
                    Ok(request) => request,
                    Err(error) => {
                        return ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "internal_runtime_failure",
                                format!(
                                    "failed to bind validated automation data to fresh target authority: {error:?}"
                                ),
                                state_effect.as_protocol_str(),
                            ),
                            fail_stop: true,
                        });
                    },
                };
                ActiveTransition::Submit(DocumentControlCommand::Automate(Box::new(request)))
            } else {
                let (result, observation) =
                    match completed_automation(outcome, &command, state_effect) {
                        Ok(completion) => completion,
                        Err(failure) => return ActiveTransition::Fail(failure),
                    };
                let result = match wire::PublicAutomationResult::project(
                    state.kind,
                    result,
                    observation.pending(),
                ) {
                    Ok(result) => result,
                    Err(error) => {
                        return ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "internal_runtime_failure",
                                format!("failed to project automation result: {error:?}"),
                                state_effect.as_protocol_str(),
                            ),
                            fail_stop: true,
                        });
                    },
                };
                serialize_result(result, state_effect)
            }
        },
        ActiveOperation::Settle(state) => {
            let progress = state
                .coordinator
                .consume_receive_outcome(outcome, state.cumulative_external_io_wall_time);
            match progress {
                Ok(progress) => transition_from_settle_progress_for_active(
                    state,
                    active.started_at,
                    progress,
                    state_effect,
                    projection,
                ),
                Err(error) => {
                    ActiveTransition::Fail(settle_failure(error, state_effect, Some(&command)))
                },
            }
        },
    }
}

fn completed_observation(
    outcome: DocumentControlReceiveOutcome,
    command: &DocumentControlCommand,
    state_effect: RequestStateEffect,
) -> Result<Box<servo::document_control::DocumentControlObservation>, ActiveFailure> {
    match outcome {
        DocumentControlReceiveOutcome::CommandOutcome(outcome) => {
            if let Err(error) = outcome.validate_for_command(command) {
                let effect = if command_is_mutating(command) {
                    "indeterminate"
                } else {
                    state_effect.as_protocol_str()
                };
                return Err(ActiveFailure {
                    error: fatal_operation(
                        if command_is_mutating(command) {
                            "outcome_indeterminate"
                        } else {
                            "internal_runtime_failure"
                        },
                        format!("invalid document-control outcome: {error:?}"),
                        effect,
                    ),
                    fail_stop: true,
                });
            }
            match outcome {
                DocumentControlOutcome::Completed(observation) => Ok(observation),
                DocumentControlOutcome::AutomationCompleted { .. } => Err(ActiveFailure {
                    error: fatal_operation(
                        "internal_runtime_failure",
                        "an automation completion was delivered for a runtime-control command",
                        state_effect.as_protocol_str(),
                    ),
                    fail_stop: true,
                }),
                DocumentControlOutcome::Rejected(error) => Err(ActiveFailure {
                    error: ProtocolError::operation(
                        "document_control_rejected",
                        format!("document-control command was rejected: {error:?}"),
                        state_effect.as_protocol_str(),
                    ),
                    fail_stop: false,
                }),
                DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate { .. } |
                DocumentControlOutcome::AdvanceOutcomeIndeterminate { .. } |
                DocumentControlOutcome::AutomationOutcomeIndeterminate { .. } => {
                    Err(ActiveFailure {
                        error: fatal_operation(
                            "outcome_indeterminate",
                            "document-control mutation outcome is indeterminate",
                            "indeterminate",
                        ),
                        fail_stop: true,
                    })
                },
            }
        },
        DocumentControlReceiveOutcome::AutomationTransportFailure(error) => Err(ActiveFailure {
            error: fatal_operation(
                "internal_runtime_failure",
                format!(
                    "an automation transport failure was delivered for a runtime-control command: {error:?}"
                ),
                state_effect.as_protocol_str(),
            ),
            fail_stop: true,
        }),
        DocumentControlReceiveOutcome::ObserveTransportFailure(error) => Err(ActiveFailure {
            error: ProtocolError::operation(
                "document_control_transport_failed",
                format!("document-control observation failed: {error:?}"),
                state_effect.as_protocol_str(),
            ),
            fail_stop: false,
        }),
        DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(error) => {
            Err(ActiveFailure {
                error: fatal_operation(
                    "outcome_indeterminate",
                    format!("document-control turn outcome is indeterminate: {error:?}"),
                    "indeterminate",
                ),
                fail_stop: true,
            })
        },
    }
}

fn completed_automation(
    outcome: DocumentControlReceiveOutcome,
    command: &DocumentControlCommand,
    state_effect: RequestStateEffect,
) -> Result<
    (
        DocumentAutomationResult,
        Box<servo::document_control::DocumentControlObservation>,
    ),
    ActiveFailure,
> {
    if !matches!(command, DocumentControlCommand::Automate(_)) {
        return Err(ActiveFailure {
            error: fatal_operation(
                "internal_runtime_failure",
                "automation completion was paired with a non-automation command",
                state_effect.as_protocol_str(),
            ),
            fail_stop: true,
        });
    }

    match outcome {
        DocumentControlReceiveOutcome::CommandOutcome(outcome) => {
            if let Err(error) = outcome.validate_for_command(command) {
                let mutating = command_is_mutating(command);
                return Err(ActiveFailure {
                    error: fatal_operation(
                        if mutating {
                            "outcome_indeterminate"
                        } else {
                            "internal_runtime_failure"
                        },
                        format!("invalid document-automation outcome: {error:?}"),
                        if mutating {
                            "indeterminate"
                        } else {
                            state_effect.as_protocol_str()
                        },
                    ),
                    fail_stop: true,
                });
            }
            match outcome {
                DocumentControlOutcome::AutomationCompleted {
                    result,
                    observation,
                } => Ok((result, observation)),
                DocumentControlOutcome::Rejected(error) => Err(ActiveFailure {
                    error: automation_rejection(error, state_effect),
                    fail_stop: false,
                }),
                DocumentControlOutcome::AutomationOutcomeIndeterminate { .. } => {
                    Err(ActiveFailure {
                        error: fatal_operation(
                            "outcome_indeterminate",
                            "document-automation mutation outcome is indeterminate",
                            "indeterminate",
                        ),
                        fail_stop: true,
                    })
                },
                DocumentControlOutcome::Completed(_) |
                DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate { .. } |
                DocumentControlOutcome::AdvanceOutcomeIndeterminate { .. } => Err(ActiveFailure {
                    error: fatal_operation(
                        "internal_runtime_failure",
                        "a non-automation outcome was delivered for an automation command",
                        state_effect.as_protocol_str(),
                    ),
                    fail_stop: true,
                }),
            }
        },
        DocumentControlReceiveOutcome::AutomationTransportFailure(error) => Err(ActiveFailure {
            error: ProtocolError::operation(
                "document_automation_transport_failed",
                format!("document automation failed in transport: {error:?}"),
                state_effect.as_protocol_str(),
            ),
            fail_stop: false,
        }),
        DocumentControlReceiveOutcome::ObserveTransportFailure(error) => Err(ActiveFailure {
            error: fatal_operation(
                "internal_runtime_failure",
                format!("an Observe transport failure was delivered for automation: {error:?}"),
                state_effect.as_protocol_str(),
            ),
            fail_stop: true,
        }),
        DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(error) => {
            Err(ActiveFailure {
                error: fatal_operation(
                    "outcome_indeterminate",
                    format!(
                        "an indeterminate turn outcome was delivered for automation: {error:?}"
                    ),
                    "indeterminate",
                ),
                fail_stop: true,
            })
        },
    }
}

fn automation_rejection(
    error: DocumentControlError,
    state_effect: RequestStateEffect,
) -> ProtocolError {
    let code = match &error {
        DocumentControlError::Automation(error) => automation_error_code(error),
        _ => "document_automation_rejected",
    };
    ProtocolError::operation(
        code,
        format!("document automation was rejected: {error:?}"),
        state_effect.as_protocol_str(),
    )
}

fn automation_error_code(error: &DocumentAutomationError) -> &'static str {
    match error {
        DocumentAutomationError::InvalidRequest(_) => "invalid_automation_request",
        DocumentAutomationError::TargetChanged => "automation_target_changed",
        DocumentAutomationError::ExecutionTerminated => "execution_terminated",
        DocumentAutomationError::StaleStateGeneration { .. } => "stale_generation",
        DocumentAutomationError::InvalidSelector { .. } => "invalid_selector",
        DocumentAutomationError::UnsupportedSelector { .. } => "unsupported_selector",
        DocumentAutomationError::MatchLimitExceeded { .. } => "automation_match_limit_exceeded",
        DocumentAutomationError::DomTraversalLimitExceeded { .. } => {
            "automation_dom_traversal_limit_exceeded"
        },
        DocumentAutomationError::SelectorEvaluationLimitExceeded { .. } => {
            "automation_selector_evaluation_limit_exceeded"
        },
        DocumentAutomationError::ElementNotFound { .. } => "element_not_found",
        DocumentAutomationError::SelectorAmbiguous { .. } => "selector_ambiguous",
        DocumentAutomationError::ExtractionFieldNotFound { .. } => "extraction_field_not_found",
        DocumentAutomationError::ExtractionFieldAmbiguous { .. } => "extraction_field_ambiguous",
        DocumentAutomationError::UnsupportedFillElement { .. } => "unsupported_fill_element",
        DocumentAutomationError::ImmutableFillElement { .. } => "immutable_fill_element",
        DocumentAutomationError::UnsupportedActivationElement { .. } => {
            "unsupported_activation_element"
        },
        DocumentAutomationError::DisabledActivationElement { .. } => "disabled_activation_element",
        DocumentAutomationError::UnsupportedLazyAttributeSerialization { .. } => {
            "unsupported_dom_serialization"
        },
        DocumentAutomationError::DomOperationFailed { .. } => "document_automation_failed",
        DocumentAutomationError::OutputLimitExceeded { .. } => "automation_output_limit_exceeded",
    }
}

fn transition_from_settle_progress(progress: settle::SettleProgress) -> ActiveTransition {
    match progress {
        settle::SettleProgress::Command(command) => ActiveTransition::Submit(command),
        settle::SettleProgress::Wait(wait) => ActiveTransition::Wait(wait),
        settle::SettleProgress::Complete(_) => ActiveTransition::Fail(ActiveFailure {
            error: fatal_operation(
                "internal_runtime_failure",
                "settlement completed before its initial observation",
                "none",
            ),
            fail_stop: true,
        }),
    }
}

fn transition_from_settle_progress_for_active(
    state: &SettleState,
    started_at: Instant,
    progress: settle::SettleProgress,
    state_effect: RequestStateEffect,
    projection: &mut wire::WireProjectionContext,
) -> ActiveTransition {
    match progress {
        settle::SettleProgress::Command(command) => ActiveTransition::Submit(command),
        settle::SettleProgress::Wait(wait) => ActiveTransition::Wait(wait),
        settle::SettleProgress::Complete(completion) => serialize_result(
            wire::RuntimeSettleResult::project(
                completion,
                Instant::now().saturating_duration_since(started_at),
                state.effective_policy,
                projection,
            ),
            state_effect,
        ),
    }
}

fn serialize_result<T: serde::Serialize>(
    result: T,
    state_effect: RequestStateEffect,
) -> ActiveTransition {
    match serde_json::to_value(result) {
        Ok(value) => ActiveTransition::Complete(value),
        Err(error) => ActiveTransition::Fail(ActiveFailure {
            error: fatal_operation(
                "internal_runtime_failure",
                format!("failed to serialize runtime result: {error}"),
                state_effect.as_protocol_str(),
            ),
            fail_stop: true,
        }),
    }
}

fn settle_failure(
    error: settle::SettleFailure,
    state_effect: RequestStateEffect,
    command: Option<&DocumentControlCommand>,
) -> ActiveFailure {
    let mutating = command.is_some_and(command_is_mutating);
    let indeterminate = matches!(
        &error,
        settle::SettleFailure::DriveOneTurnOutcomeIndeterminate(_) |
            settle::SettleFailure::DriveOutcomeIndeterminate(_) |
            settle::SettleFailure::AdvanceOutcomeIndeterminate(_)
    ) || (mutating &&
        matches!(&error, settle::SettleFailure::InvalidControlOutcome(_)));
    let internal = matches!(
        &error,
        settle::SettleFailure::InvalidCoordinatorState(_) |
            settle::SettleFailure::InvalidControlOutcome(_) |
            settle::SettleFailure::ExternalIoWallTimeRegressed { .. }
    );
    ActiveFailure {
        error: if indeterminate {
            fatal_operation(
                "outcome_indeterminate",
                format!("settlement command outcome is indeterminate: {error:?}"),
                "indeterminate",
            )
        } else if internal {
            fatal_operation(
                "internal_runtime_failure",
                format!("settlement state machine failed: {error:?}"),
                state_effect.as_protocol_str(),
            )
        } else {
            ProtocolError::operation(
                "settlement_failed",
                format!("settlement could not continue: {error:?}"),
                state_effect.as_protocol_str(),
            )
        },
        fail_stop: indeterminate || internal,
    }
}

fn command_is_mutating(command: &DocumentControlCommand) -> bool {
    match command {
        DocumentControlCommand::Observe => false,
        DocumentControlCommand::DriveOneTurn |
        DocumentControlCommand::BootstrapInitialPipeline { .. } |
        DocumentControlCommand::AdvanceTo(_) => true,
        DocumentControlCommand::Automate(request) => {
            DocumentControlAutomationKind::from_request(request).is_mutating()
        },
    }
}

fn fatal_operation(
    code: &'static str,
    message: impl Into<String>,
    state_effect: &'static str,
) -> ProtocolError {
    ProtocolError {
        code,
        message: message.into(),
        fatal: true,
        state_effect,
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
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct OpenParams {
    url: String,
    #[serde(default)]
    clock_mode: OpenClockMode,
    #[serde(default)]
    initial_virtual_time_ns: Option<wire::DecimalU128>,
    #[serde(default)]
    unix_time_origin_ns: Option<wire::DecimalU128>,
    #[serde(default)]
    profile: Option<String>,
}

impl OpenParams {
    fn clock_mode(&self) -> Result<(EngineClockMode, &'static str), ProtocolError> {
        match self.clock_mode {
            OpenClockMode::Real => {
                if self.initial_virtual_time_ns.is_some() ||
                    self.unix_time_origin_ns.is_some() ||
                    self.profile.is_some()
                {
                    return Err(ProtocolError::invalid_request(
                        "controlled time fields and profile require clockMode controlled",
                    ));
                }
                Ok((EngineClockMode::Real, "load_complete"))
            },
            OpenClockMode::Controlled => {
                if self.profile.as_deref() != Some(CONTROLLED_WEBAPP_V1_PROFILE) {
                    return Err(ProtocolError::invalid_request(format!(
                        "controlled sessions require profile {CONTROLLED_WEBAPP_V1_PROFILE}",
                    )));
                }
                let unix_origin = self
                    .unix_time_origin_ns
                    .as_ref()
                    .map_or(0, wire::DecimalU128::get);
                if unix_origin != 0 {
                    return Err(ProtocolError::invalid_request(
                        "unixTimeOriginNs must be 0 in the controlled MVP",
                    ));
                }
                Ok((
                    EngineClockMode::Controlled {
                        initial_time_ns: self
                            .initial_virtual_time_ns
                            .as_ref()
                            .map_or(0, wire::DecimalU128::get),
                    },
                    "controlled_ready",
                ))
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum OpenClockMode {
    #[default]
    Real,
    Controlled,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluateParams {
    expression: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct CancelParams {
    request_id: String,
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
    // The release workflow injects the exact tagged commit without requiring a generated source
    // file. Local builds remain explicit about their non-release identity.
    identities.insert(
        "stasis_repository".into(),
        Value::String(
            option_env!("STASIS_REPOSITORY")
                .unwrap_or("https://github.com/oxhq/stasis.git")
                .into(),
        ),
    );
    identities.insert(
        "stasis_revision".into(),
        Value::String(
            option_env!("STASIS_REVISION")
                .unwrap_or("uncommitted")
                .into(),
        ),
    );
    Value::Object(identities)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use super::*;

    struct FakeEngine {
        clock_mode: EngineClockMode,
        pump_calls: usize,
        cancel_calls: usize,
        close_calls: usize,
        submitted: Vec<DocumentControlCommand>,
        polls: VecDeque<EnginePortPoll>,
    }

    impl FakeEngine {
        fn controlled() -> Self {
            Self {
                clock_mode: EngineClockMode::Controlled { initial_time_ns: 0 },
                pump_calls: 0,
                cancel_calls: 0,
                close_calls: 0,
                submitted: Vec::new(),
                polls: VecDeque::new(),
            }
        }
    }

    impl EnginePort for FakeEngine {
        fn open_session(
            _url: Url,
            _waker: ShellWaker,
            clock_mode: EngineClockMode,
        ) -> Result<Self, ProtocolError> {
            Ok(Self {
                clock_mode,
                pump_calls: 0,
                cancel_calls: 0,
                close_calls: 0,
                submitted: Vec::new(),
                polls: VecDeque::new(),
            })
        }

        fn pump(&mut self) {
            self.pump_calls += 1;
        }

        fn url(&self) -> Option<Url> {
            None
        }

        fn clock_mode(&self) -> EngineClockMode {
            self.clock_mode
        }

        fn evaluate(&self, _expression: &str) -> Result<Value, ProtocolError> {
            Ok(Value::Null)
        }

        fn submit_document_control(
            &mut self,
            command: DocumentControlCommand,
            _timeout: Duration,
        ) -> Result<(), ProtocolError> {
            self.submitted.push(command);
            Ok(())
        }

        fn poll_control_operation(&mut self) -> EnginePortPoll {
            self.polls.pop_front().unwrap_or(EnginePortPoll::Idle)
        }

        fn cancel_control_operation(&mut self) -> Option<EnginePortCompletion> {
            self.cancel_calls += 1;
            None
        }

        fn close(&mut self) {
            self.close_calls += 1;
        }
    }

    fn request_with_id(method: &str, id: &str, session_id: Option<&str>) -> Request {
        Request {
            v: 1,
            kind: "request".into(),
            id: id.into(),
            session_id: session_id.map(str::to_owned),
            method: method.into(),
            params: json!({}),
        }
    }

    fn request(method: &str, session_id: Option<&str>) -> Request {
        request_with_id(method, "test-1", session_id)
    }

    fn pipeline_id(index: u32) -> servo_base::id::PipelineId {
        servo_base::id::NamespaceIndex {
            namespace_id: servo_base::id::PipelineNamespaceId(9),
            index: servo_base::id::Index::new(index).unwrap(),
        }
    }

    fn shell<'a>(
        output: &'a mut Vec<u8>,
        state: ShellState,
        engine: Option<FakeEngine>,
    ) -> Shell<&'a mut Vec<u8>, FakeEngine> {
        let (_sender, inbox) = reader_channel(2);
        let waker = ShellWaker::default();
        let cursor = waker.snapshot_checked().unwrap();
        Shell {
            state,
            engine,
            inbox,
            waker,
            wake_cursor: cursor,
            servo_cursor: cursor,
            writer: ProtocolWriter::new(output),
            active: None,
            projection: wire::WireProjectionContext::new(),
        }
    }

    fn frames(bytes: &[u8]) -> Vec<Value> {
        bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect()
    }

    #[test]
    fn source_identity_manifest_contains_upstreams_and_stasis_build() {
        let identities = parse_source_identities();
        assert_eq!(identities["servo_revision"].as_str().unwrap().len(), 40);
        assert_eq!(identities["pliego_revision"].as_str().unwrap().len(), 40);
        assert_eq!(
            identities["stasis_repository"].as_str(),
            Some(option_env!("STASIS_REPOSITORY").unwrap_or("https://github.com/oxhq/stasis.git"))
        );
        assert_eq!(
            identities["stasis_revision"].as_str(),
            Some(option_env!("STASIS_REVISION").unwrap_or("uncommitted"))
        );
    }

    #[test]
    fn an_invalid_close_does_not_terminate_the_shell() {
        let mut bytes = Vec::new();
        let mut shell = shell(&mut bytes, ShellState::Initialized, None);

        assert!(!shell.handle(request("session.close", None)).unwrap());
        assert_eq!(shell.state, ShellState::Initialized);
    }

    #[test]
    fn a_valid_close_is_terminal_and_keeps_the_session_id() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(&mut bytes, ShellState::Open, Some(FakeEngine::controlled()));

            assert!(
                shell
                    .handle(request("session.close", Some(SESSION_ID)))
                    .unwrap()
            );
            assert_eq!(shell.state, ShellState::Closed);
        }

        let response = frames(&bytes).pop().unwrap();
        assert_eq!(response["sessionId"], SESSION_ID);
    }

    #[test]
    fn an_ordinary_request_is_busy_while_runtime_work_is_active() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(&mut bytes, ShellState::Open, Some(FakeEngine::controlled()));
            shell.active = Some(ActiveRequest {
                request: request_with_id("runtime.pending", "active", Some(SESSION_ID)),
                operation: ActiveOperation::Pending,
                started_at: Instant::now(),
                in_flight: None,
                needs_initial_pump: false,
                state_effect: RequestStateEffect::None,
            });

            assert!(
                !shell
                    .handle(request_with_id(
                        "runtime.pending",
                        "second",
                        Some(SESSION_ID),
                    ))
                    .unwrap()
            );
            assert_eq!(shell.active.as_ref().unwrap().request.id, "active");
        }

        assert_eq!(frames(&bytes)[0]["error"]["code"], "busy");
    }

    #[test]
    fn cancellation_cannot_target_its_own_request_id() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(&mut bytes, ShellState::Open, Some(FakeEngine::controlled()));
            shell.active = Some(ActiveRequest {
                request: request_with_id("runtime.pending", "same", Some(SESSION_ID)),
                operation: ActiveOperation::Pending,
                started_at: Instant::now(),
                in_flight: None,
                needs_initial_pump: false,
                state_effect: RequestStateEffect::None,
            });
            let mut cancel = request_with_id("protocol.cancel", "same", Some(SESSION_ID));
            cancel.params = json!({"requestId": "same"});

            assert!(!shell.handle(cancel).unwrap());
            assert_eq!(shell.engine.as_ref().unwrap().cancel_calls, 0);
            assert_eq!(shell.active.as_ref().unwrap().request.id, "same");
        }

        assert_eq!(frames(&bytes)[0]["error"]["code"], "invalid_request");
    }

    #[test]
    fn active_cancellation_acknowledges_before_terminalizing_the_target() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(&mut bytes, ShellState::Open, Some(FakeEngine::controlled()));
            shell.active = Some(ActiveRequest {
                request: request_with_id("runtime.settle", "active", Some(SESSION_ID)),
                operation: ActiveOperation::Pending,
                started_at: Instant::now(),
                in_flight: None,
                needs_initial_pump: false,
                state_effect: RequestStateEffect::Partial,
            });
            let mut cancel = request_with_id("protocol.cancel", "cancel", Some(SESSION_ID));
            cancel.params = json!({"requestId": "active"});

            assert!(!shell.handle(cancel).unwrap());
            assert!(shell.active.is_none());
            assert_eq!(shell.engine.as_ref().unwrap().cancel_calls, 1);
        }

        let frames = frames(&bytes);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["id"], "cancel");
        assert_eq!(frames[0]["result"]["accepted"], true);
        assert_eq!(frames[1]["id"], "active");
        assert_eq!(frames[1]["error"]["code"], "cancelled");
        assert_eq!(frames[1]["error"]["stateEffect"], "partial");
    }

    #[test]
    fn close_terminalizes_active_work_before_its_final_response() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(&mut bytes, ShellState::Open, Some(FakeEngine::controlled()));
            shell.active = Some(ActiveRequest {
                request: request_with_id("runtime.settle", "active", Some(SESSION_ID)),
                operation: ActiveOperation::Pending,
                started_at: Instant::now(),
                in_flight: None,
                needs_initial_pump: false,
                state_effect: RequestStateEffect::Partial,
            });

            assert!(
                shell
                    .handle(request_with_id("session.close", "close", Some(SESSION_ID),))
                    .unwrap()
            );
        }

        let frames = frames(&bytes);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["id"], "active");
        assert_eq!(frames[0]["error"]["stateEffect"], "partial");
        assert_eq!(frames[1]["id"], "close");
        assert_eq!(frames[1]["result"]["state"], "closed");
    }

    #[test]
    fn controlled_open_requires_the_named_profile_and_supported_unix_origin() {
        let controlled: OpenParams = serde_json::from_value(json!({
            "url": "about:blank",
            "clockMode": "controlled",
            "profile": CONTROLLED_WEBAPP_V1_PROFILE,
            "initialVirtualTimeNs": "42",
            "unixTimeOriginNs": "0"
        }))
        .unwrap();
        assert_eq!(
            controlled.clock_mode().unwrap(),
            (
                EngineClockMode::Controlled {
                    initial_time_ns: 42
                },
                "controlled_ready"
            )
        );

        let unsupported: OpenParams = serde_json::from_value(json!({
            "url": "about:blank",
            "clockMode": "controlled",
            "profile": CONTROLLED_WEBAPP_V1_PROFILE,
            "unixTimeOriginNs": "1"
        }))
        .unwrap();
        assert_eq!(
            unsupported.clock_mode().unwrap_err().code,
            "invalid_request"
        );

        for invalid in [
            json!({
                "url": "about:blank",
                "clockMode": "controlled",
            }),
            json!({
                "url": "about:blank",
                "clockMode": "controlled",
                "profile": "controlled-webapp-v2",
            }),
            json!({
                "url": "about:blank",
                "clockMode": "real",
                "profile": CONTROLLED_WEBAPP_V1_PROFILE,
            }),
        ] {
            let params: OpenParams = serde_json::from_value(invalid).unwrap();
            assert_eq!(params.clock_mode().unwrap_err().code, "invalid_request");
        }
    }

    #[test]
    fn failed_open_never_exposes_the_provisional_session_id() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(&mut bytes, ShellState::Open, Some(FakeEngine::controlled()));
            shell
                .write_method_result(
                    &request_with_id("session.open", "open", None),
                    Err(ProtocolError::operation(
                        "cancelled",
                        "controlled open was cancelled",
                        "none",
                    )),
                )
                .unwrap();
        }

        assert!(frames(&bytes)[0]["sessionId"].is_null());
    }

    #[test]
    fn controlled_open_retries_event_loop_unavailable_before_reporting_ready() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(&mut bytes, ShellState::Initialized, None);
            let mut open = request("session.open", None);
            open.params = json!({
                "url": "about:blank",
                "clockMode": "controlled",
                "profile": CONTROLLED_WEBAPP_V1_PROFILE,
            });

            assert!(!shell.handle(open).unwrap());
            assert_eq!(shell.engine.as_ref().unwrap().submitted.len(), 1);
            shell
                .engine
                .as_mut()
                .unwrap()
                .polls
                .push_back(EnginePortPoll::Complete(EnginePortCompletion {
                    disposition: ControlOutcomeDisposition::DefinitiveFailure,
                    outcome: DocumentControlReceiveOutcome::CommandOutcome(
                        DocumentControlOutcome::Rejected(
                            DocumentControlError::EventLoopUnavailable,
                        ),
                    ),
                }));

            assert_eq!(shell.poll_active_control().unwrap(), (true, None));
            assert!(matches!(
                shell.active.as_ref().map(|active| &active.operation),
                Some(ActiveOperation::ControlledOpen(ControlledOpenState {
                    waiting: Some(_),
                    ..
                }))
            ));

            servo::EventLoopWaker::wake(&shell.waker);
            let current = shell.checked_wake_snapshot().unwrap();
            assert!(
                shell
                    .service_active_host_wait(current, Instant::now())
                    .unwrap()
            );
            assert_eq!(shell.engine.as_ref().unwrap().submitted.len(), 2);
        }

        assert!(frames(&bytes).is_empty());
    }

    #[test]
    fn controlled_open_does_not_retry_missing_authoritative_pending_facts() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(&mut bytes, ShellState::Initialized, None);
            let mut open = request("session.open", None);
            open.params = json!({
                "url": "about:blank",
                "clockMode": "controlled",
                "profile": CONTROLLED_WEBAPP_V1_PROFILE,
            });

            assert!(!shell.handle(open).unwrap());
            shell
                .engine
                .as_mut()
                .unwrap()
                .polls
                .push_back(EnginePortPoll::Complete(EnginePortCompletion {
                    disposition: ControlOutcomeDisposition::DefinitiveFailure,
                    outcome: DocumentControlReceiveOutcome::CommandOutcome(
                        DocumentControlOutcome::Rejected(
                            DocumentControlError::PendingFactUnavailable(
                                servo::document_control::DocumentPendingFact::Rendering,
                            ),
                        ),
                    ),
                }));

            assert_eq!(shell.poll_active_control().unwrap(), (true, None));
            assert!(shell.active.is_none());
            assert!(shell.engine.is_none());
            assert_eq!(shell.state, ShellState::Initialized);
        }

        let response = frames(&bytes).pop().unwrap();
        assert_eq!(response["error"]["code"], "document_control_rejected");
        assert!(response["sessionId"].is_null());
    }

    #[test]
    fn controlled_open_allows_exactly_one_typed_initial_pipeline_bootstrap() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(&mut bytes, ShellState::Initialized, None);
            let mut open = request("session.open", None);
            open.params = json!({
                "url": "https://example.test/",
                "clockMode": "controlled",
                "profile": CONTROLLED_WEBAPP_V1_PROFILE,
            });

            assert!(!shell.handle(open).unwrap());
            let expected_pipeline_id = pipeline_id(1);
            let bootstrap_required = || {
                EnginePortPoll::Complete(EnginePortCompletion {
                    disposition: ControlOutcomeDisposition::DefinitiveFailure,
                    outcome: DocumentControlReceiveOutcome::CommandOutcome(
                        DocumentControlOutcome::Rejected(
                            DocumentControlError::InitialPipelineBootstrapRequired {
                                pipeline_id: expected_pipeline_id,
                            },
                        ),
                    ),
                })
            };
            shell
                .engine
                .as_mut()
                .unwrap()
                .polls
                .push_back(bootstrap_required());

            assert_eq!(shell.poll_active_control().unwrap(), (true, None));
            assert_eq!(
                shell.engine.as_ref().unwrap().submitted,
                vec![
                    DocumentControlCommand::Observe,
                    DocumentControlCommand::BootstrapInitialPipeline {
                        pipeline_id: expected_pipeline_id,
                    },
                ]
            );
            assert!(matches!(
                shell.active.as_ref().map(|active| &active.operation),
                Some(ActiveOperation::ControlledOpen(ControlledOpenState {
                    bootstrap_attempted: true,
                    ..
                }))
            ));

            // A definitive rejection of the dedicated bootstrap closes only the provisional
            // session. It must not loop into another bootstrap or expose a session id.
            shell
                .engine
                .as_mut()
                .unwrap()
                .polls
                .push_back(EnginePortPoll::Complete(EnginePortCompletion {
                    disposition: ControlOutcomeDisposition::DefinitiveFailure,
                    outcome: DocumentControlReceiveOutcome::CommandOutcome(
                        DocumentControlOutcome::Rejected(
                            DocumentControlError::InitialPipelineBootstrapUnavailable {
                                pipeline_id: expected_pipeline_id,
                            },
                        ),
                    ),
                }));
            assert_eq!(shell.poll_active_control().unwrap(), (true, None));
            assert!(shell.active.is_none());
            assert!(shell.engine.is_none());
            assert_eq!(shell.state, ShellState::Initialized);
        }

        let response = frames(&bytes).pop().unwrap();
        assert_eq!(response["error"]["code"], "document_control_rejected");
        assert!(response["sessionId"].is_null());
    }

    #[test]
    fn runtime_methods_are_rejected_before_submission_in_real_mode() {
        let mut bytes = Vec::new();
        {
            let real = FakeEngine {
                clock_mode: EngineClockMode::Real,
                pump_calls: 0,
                cancel_calls: 0,
                close_calls: 0,
                submitted: Vec::new(),
                polls: VecDeque::new(),
            };
            let mut shell = shell(&mut bytes, ShellState::Open, Some(real));

            assert!(
                !shell
                    .handle(request("runtime.pending", Some(SESSION_ID),))
                    .unwrap()
            );
            assert!(shell.engine.as_ref().unwrap().submitted.is_empty());
        }

        assert_eq!(
            frames(&bytes)[0]["error"]["code"],
            "controlled_clock_required"
        );
    }

    #[test]
    fn controlled_automation_observes_before_binding_private_target_authority() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(&mut bytes, ShellState::Open, Some(FakeEngine::controlled()));
            let mut activate = request("action.activate", Some(SESSION_ID));
            activate.params = json!({
                "selector": "#start",
                "expectedGeneration": "7",
            });

            assert!(!shell.handle(activate).unwrap());
            assert_eq!(
                shell.engine.as_ref().unwrap().submitted,
                vec![DocumentControlCommand::Observe]
            );
            assert!(matches!(
                shell.active.as_ref().map(|active| &active.operation),
                Some(ActiveOperation::Automation(AutomationState {
                    unresolved: Some(_),
                    ..
                }))
            ));
        }

        assert!(frames(&bytes).is_empty());
    }

    #[test]
    fn every_public_automation_method_enters_the_observe_then_bind_path() {
        for (method, params, expected_kind) in [
            (
                "action.fill",
                json!({
                    "selector": "#email",
                    "value": "person@example.test",
                    "expectedGeneration": "7",
                }),
                wire::PublicAutomationKind::Fill,
            ),
            (
                "dom.query",
                json!({"selector": ".row", "expectedGeneration": "7"}),
                wire::PublicAutomationKind::Query,
            ),
            (
                "dom.extract",
                json!({
                    "rootSelector": ".row",
                    "fields": [
                        {"name": "title", "selector": ".title", "read": "text"},
                    ],
                    "expectedGeneration": "7",
                }),
                wire::PublicAutomationKind::Extract,
            ),
        ] {
            let mut bytes = Vec::new();
            {
                let mut shell = shell(&mut bytes, ShellState::Open, Some(FakeEngine::controlled()));
                let mut request = request(method, Some(SESSION_ID));
                request.params = params;

                assert!(!shell.handle(request).unwrap());
                assert_eq!(
                    shell.engine.as_ref().unwrap().submitted,
                    vec![DocumentControlCommand::Observe],
                    "{method} did not begin with a passive observation",
                );
                assert!(matches!(
                    shell.active.as_ref().map(|active| &active.operation),
                    Some(ActiveOperation::Automation(AutomationState {
                        kind,
                        unresolved: Some(_),
                    })) if *kind == expected_kind
                ));
            }
            assert!(frames(&bytes).is_empty());
        }
    }

    #[test]
    fn automation_rejections_have_stable_public_codes() {
        use embedder_traits::document_pending::RuntimeStateGeneration;

        for (error, expected_code) in [
            (
                DocumentAutomationError::StaleStateGeneration {
                    expected: RuntimeStateGeneration::new(7),
                    observed: RuntimeStateGeneration::new(8),
                },
                "stale_generation",
            ),
            (
                DocumentAutomationError::UnsupportedFillElement {
                    selector: "#choice".into(),
                },
                "unsupported_fill_element",
            ),
            (
                DocumentAutomationError::SelectorAmbiguous {
                    selector: ".row".into(),
                    matches: 2,
                },
                "selector_ambiguous",
            ),
            (
                DocumentAutomationError::OutputLimitExceeded {
                    attempted: 131_073,
                    limit: 131_072,
                },
                "automation_output_limit_exceeded",
            ),
        ] {
            let projected = automation_rejection(
                DocumentControlError::Automation(error),
                RequestStateEffect::None,
            );
            assert_eq!(projected.code, expected_code);
            assert!(!projected.fatal);
            assert_eq!(projected.state_effect, "none");
        }
    }

    #[test]
    fn automation_rejects_generations_outside_the_runtime_u64_authority() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(&mut bytes, ShellState::Open, Some(FakeEngine::controlled()));
            let mut text = request("dom.text", Some(SESSION_ID));
            text.params = json!({
                "selector": "#state",
                "expectedGeneration": "18446744073709551616",
            });

            assert!(!shell.handle(text).unwrap());
            assert!(shell.engine.as_ref().unwrap().submitted.is_empty());
            assert!(shell.active.is_none());
        }

        assert_eq!(frames(&bytes)[0]["error"]["code"], "invalid_request");
    }
}
