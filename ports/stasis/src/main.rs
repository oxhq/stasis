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
use embedder_traits::document_session::{
    SameDocumentSessionTransition, SessionNavigationCounter, SessionNavigationTerminal,
    classify_same_document_session_transition,
};
use net_traits::controlled_network::{ControlledNetworkFailure, ControlledNetworkSnapshot};
use net_traits::network_evidence::{
    EvidenceLedgerError, EvidenceSequence, NetworkEvidencePage, NetworkFailureReason,
    NetworkRequestsPage,
};
use serde::Deserialize;
use serde_json::{Value, json};
use servo::document_control::{
    DocumentControlAction, DocumentControlAutomationKind, DocumentControlCommand,
    DocumentControlError, DocumentControlOutcome, DocumentControlReceiveOutcome,
};
use stasis_shell::session_state::{
    SessionCookiesResultV1, SessionCookiesSetParamsV1, SessionStateError,
    SessionStateExportResultV1, SessionStateMutationResultV1, SessionStateToken, SessionStateV1,
    SessionStorageResultV1, SessionStorageSetParamsV1, WireU64,
};
use stasis_shell::{settle, wire};
use url::Url;

use crate::engine::{
    ControlOutcomeDisposition, DocumentControlProfile, DocumentExecutionProfile, EngineClockMode,
    EngineControlPoll, EngineNavigationPoll, EngineSession, EngineSessionOpenOptions,
    NavigationOperationCompletion, NavigationOperationKind, SessionNavigationAuthority,
    SessionNavigationError, SessionNavigationId,
};
use crate::protocol::{
    DEFAULT_ORDINARY_LANE_CAPACITY, OrdinaryRequestRemoval, ProtocolError, ProtocolWriter,
    ReaderCloseDisposition, ReaderInbox, ReaderMessage, Request, reader_channel, spawn_reader,
};
use crate::wake::{ShellWaker, WakeGeneration, WakeWaitError};

const SOURCE_IDENTITIES: &str = include_str!("../../../STASIS_UPSTREAM.toml");
const SESSION_ID: &str = "s-1";
const CONTROLLED_WEBAPP_V1_PROFILE: &str = "controlled-webapp-v1";
const CONTROLLED_WEB_SESSION_V1_PROFILE: &str = "controlled-web-session-v1";
const CONTROLLED_WEB_SESSION_V2_PROFILE: &str = "controlled-web-session-v2";
const CONTROL_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
// Stay below the public SDK's 30-second command timeout so a native controlled-open failure can
// retain its typed protocol identity instead of being masked by the caller's generic timeout.
const CONTROLLED_OPEN_WALL_TIMEOUT: Duration = Duration::from_secs(25);
const CONTROLLED_OPEN_RETRY_INTERVAL: Duration = Duration::from_millis(10);
const OWNER_LOOP_SAFETY_TIMEOUT: Duration = Duration::from_secs(86_400);
const DEFAULT_SESSION_AUDIT_PAGE_ITEMS: usize = 256;
const HARD_SESSION_AUDIT_PAGE_ITEMS: usize = 1024;

fn earliest_deadline(left: Option<Instant>, right: Option<Instant>) -> Option<Instant> {
    [left, right].into_iter().flatten().min()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionProfile {
    ControlledWebappV1,
    ControlledWebSessionV1,
    ControlledWebSessionV2,
}

impl SessionProfile {
    const fn id(self) -> &'static str {
        match self {
            Self::ControlledWebappV1 => CONTROLLED_WEBAPP_V1_PROFILE,
            Self::ControlledWebSessionV1 => CONTROLLED_WEB_SESSION_V1_PROFILE,
            Self::ControlledWebSessionV2 => CONTROLLED_WEB_SESSION_V2_PROFILE,
        }
    }

    const fn document_control_profile(self) -> DocumentControlProfile {
        match self {
            Self::ControlledWebappV1 => DocumentControlProfile::SingleDocument,
            Self::ControlledWebSessionV1 | Self::ControlledWebSessionV2 => {
                DocumentControlProfile::TopLevelSession
            },
        }
    }

    const fn document_execution_profile(self) -> DocumentExecutionProfile {
        match self {
            Self::ControlledWebappV1 | Self::ControlledWebSessionV1 => {
                DocumentExecutionProfile::Baseline
            },
            Self::ControlledWebSessionV2 => DocumentExecutionProfile::ControlledWebSessionV2,
        }
    }

    const fn supports_session_api(self) -> bool {
        matches!(
            self,
            Self::ControlledWebSessionV1 | Self::ControlledWebSessionV2
        )
    }
}

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
    let reader = spawn_reader(sender, waker.clone());
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
        profile: None,
        last_navigation_authority: None,
    };

    match shell.run() {
        Ok(()) => {
            if reader.join().is_err() {
                eprintln!("stasis shell fatal error: protocol reader panicked during shutdown");
                std::process::exit(70);
            }
        },
        Err(error) => {
            eprintln!("stasis shell fatal error: {error}");
            std::process::exit(70);
        },
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
    profile: Option<SessionProfile>,
    last_navigation_authority: Option<SessionNavigationAuthority>,
}

trait EnginePort: Sized {
    fn open_session(
        url: Url,
        waker: ShellWaker,
        options: EngineSessionOpenOptions,
    ) -> Result<Self, ProtocolError>;
    fn pump(&mut self);
    fn url(&self) -> Option<Url>;
    fn clock_mode(&self) -> EngineClockMode;
    fn document_control_profile(&self) -> DocumentControlProfile;
    fn document_execution_profile(&self) -> DocumentExecutionProfile;
    fn evaluate(&self, expression: &str) -> Result<Value, ProtocolError>;
    fn submit_document_control(
        &mut self,
        command: DocumentControlCommand,
        timeout: Duration,
    ) -> Result<(), ProtocolError>;
    fn poll_control_operation(&mut self) -> EnginePortPoll;
    fn cancel_control_operation(&mut self) -> Option<EnginePortCompletion>;
    fn submit_session_navigation_observation(
        &mut self,
        timeout: Duration,
    ) -> Result<(), ProtocolError>;
    fn submit_session_navigation(
        &mut self,
        expected: SessionNavigationAuthority,
        url: Url,
        timeout: Duration,
    ) -> Result<(), ProtocolError>;
    fn poll_session_navigation(&mut self) -> EnginePortNavigationPoll;
    fn cancel_session_navigation(&mut self) -> Option<NavigationOperationCompletion>;
    fn session_state_token(&self) -> Result<SessionStateToken, ProtocolError> {
        Err(ProtocolError::operation(
            "session_state_backend_unavailable",
            "session-state backend is unavailable",
            "none",
        ))
    }
    fn session_cookies_get(&self) -> Result<SessionCookiesResultV1, ProtocolError> {
        Err(ProtocolError::operation(
            "session_state_backend_unavailable",
            "session-state backend is unavailable",
            "none",
        ))
    }
    fn session_storage_get(&self) -> Result<SessionStorageResultV1, ProtocolError> {
        Err(ProtocolError::operation(
            "session_state_backend_unavailable",
            "session-state backend is unavailable",
            "none",
        ))
    }
    fn session_state_export(&self) -> Result<SessionStateExportResultV1, ProtocolError> {
        Err(ProtocolError::operation(
            "session_state_backend_unavailable",
            "session-state backend is unavailable",
            "none",
        ))
    }
    fn session_cookies_set(
        &self,
        params: SessionCookiesSetParamsV1,
    ) -> Result<SessionStateMutationResultV1, ProtocolError> {
        Err(ProtocolError::operation(
            "session_state_backend_unavailable",
            "session-state backend is unavailable",
            "none",
        ))
    }
    fn session_storage_set(
        &self,
        params: SessionStorageSetParamsV1,
    ) -> Result<SessionStateMutationResultV1, ProtocolError> {
        Err(ProtocolError::operation(
            "session_state_backend_unavailable",
            "session-state backend is unavailable",
            "none",
        ))
    }
    fn controlled_network_snapshot(&self) -> Option<ControlledNetworkSnapshot> {
        None
    }
    fn set_controlled_network_virtual_time_ns(
        &self,
        virtual_time_ns: u128,
    ) -> Result<(), ProtocolError> {
        Ok(())
    }
    fn network_requests_page(
        &self,
        after: Option<EvidenceSequence>,
        limit: usize,
    ) -> Result<NetworkRequestsPage, ProtocolError> {
        Err(ProtocolError::operation(
            "controlled_network_unavailable",
            "controlled-network ledger is unavailable",
            "none",
        ))
    }
    fn network_evidence_page(
        &self,
        after: Option<EvidenceSequence>,
        limit: usize,
    ) -> Result<NetworkEvidencePage, ProtocolError> {
        Err(ProtocolError::operation(
            "controlled_network_unavailable",
            "controlled-network ledger is unavailable",
            "none",
        ))
    }
    fn record_navigation_started(&self, _authority: &SessionNavigationAuthority) {}
    fn record_navigation_started_id(&self, _navigation_id: SessionNavigationId) {}
    fn record_navigation_committed(&self, _authority: &SessionNavigationAuthority) {}
    fn record_navigation_failed(
        &self,
        authority: &SessionNavigationAuthority,
        reason: NetworkFailureReason,
    ) {
    }
    fn record_navigation_failed_id(
        &self,
        _navigation_id: SessionNavigationId,
        _reason: NetworkFailureReason,
    ) {
    }
    fn record_same_document_history_changed(&self, _authority: &SessionNavigationAuthority) {}
    fn record_settlement_terminal(&self, _authority: &SessionNavigationAuthority) {}
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

enum EnginePortNavigationPoll {
    Idle,
    Pending { deadline: Instant },
    Complete(NavigationOperationCompletion),
}

impl EnginePort for EngineSession {
    fn open_session(
        url: Url,
        waker: ShellWaker,
        options: EngineSessionOpenOptions,
    ) -> Result<Self, ProtocolError> {
        match options.clock_mode {
            EngineClockMode::Real => Self::open(url, waker),
            EngineClockMode::Controlled { .. } => Self::start_with_options(url, waker, options),
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

    fn document_control_profile(&self) -> DocumentControlProfile {
        Self::document_control_profile(self)
    }

    fn document_execution_profile(&self) -> DocumentExecutionProfile {
        Self::document_execution_profile(self)
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

    fn submit_session_navigation_observation(
        &mut self,
        timeout: Duration,
    ) -> Result<(), ProtocolError> {
        Self::submit_session_navigation_observation(self, timeout)
            .map_err(|error| error.to_protocol_error())
    }

    fn submit_session_navigation(
        &mut self,
        expected: SessionNavigationAuthority,
        url: Url,
        timeout: Duration,
    ) -> Result<(), ProtocolError> {
        Self::submit_session_navigation(self, expected, url, timeout)
            .map_err(|error| error.to_protocol_error())
    }

    fn poll_session_navigation(&mut self) -> EnginePortNavigationPoll {
        match Self::poll_session_navigation(self) {
            EngineNavigationPoll::Idle => EnginePortNavigationPoll::Idle,
            EngineNavigationPoll::Pending { deadline } => {
                EnginePortNavigationPoll::Pending { deadline }
            },
            EngineNavigationPoll::Complete(completion) => {
                EnginePortNavigationPoll::Complete(completion)
            },
        }
    }

    fn cancel_session_navigation(&mut self) -> Option<NavigationOperationCompletion> {
        Self::cancel_session_navigation(self)
    }

    fn session_state_token(&self) -> Result<SessionStateToken, ProtocolError> {
        Self::session_state_token(self).map_err(session_state_protocol_error)
    }

    fn session_cookies_get(&self) -> Result<SessionCookiesResultV1, ProtocolError> {
        Self::session_cookies_get(self).map_err(session_state_protocol_error)
    }

    fn session_storage_get(&self) -> Result<SessionStorageResultV1, ProtocolError> {
        Self::session_storage_get(self).map_err(session_state_protocol_error)
    }

    fn session_state_export(&self) -> Result<SessionStateExportResultV1, ProtocolError> {
        Self::session_state_export(self).map_err(session_state_protocol_error)
    }

    fn session_cookies_set(
        &self,
        params: SessionCookiesSetParamsV1,
    ) -> Result<SessionStateMutationResultV1, ProtocolError> {
        Self::session_cookies_set(self, params).map_err(session_state_protocol_error)
    }

    fn session_storage_set(
        &self,
        params: SessionStorageSetParamsV1,
    ) -> Result<SessionStateMutationResultV1, ProtocolError> {
        Self::session_storage_set(self, params).map_err(session_state_protocol_error)
    }

    fn controlled_network_snapshot(&self) -> Option<ControlledNetworkSnapshot> {
        Self::controlled_network_snapshot(self)
    }

    fn set_controlled_network_virtual_time_ns(
        &self,
        virtual_time_ns: u128,
    ) -> Result<(), ProtocolError> {
        Self::set_controlled_network_virtual_time_ns(self, virtual_time_ns).map_err(|error| {
            fatal_operation(
                "internal_runtime_failure",
                format!("controlled-network virtual time regressed: {error:?}"),
                "indeterminate",
            )
        })
    }

    fn network_requests_page(
        &self,
        after: Option<EvidenceSequence>,
        limit: usize,
    ) -> Result<NetworkRequestsPage, ProtocolError> {
        Self::network_requests_page(self, after, limit).map_err(network_evidence_protocol_error)
    }

    fn network_evidence_page(
        &self,
        after: Option<EvidenceSequence>,
        limit: usize,
    ) -> Result<NetworkEvidencePage, ProtocolError> {
        Self::network_evidence_page(self, after, limit).map_err(network_evidence_protocol_error)
    }

    fn record_navigation_started(&self, authority: &SessionNavigationAuthority) {
        Self::record_navigation_started(self, authority);
    }

    fn record_navigation_started_id(&self, navigation_id: SessionNavigationId) {
        Self::record_navigation_started_id(self, navigation_id);
    }

    fn record_navigation_committed(&self, authority: &SessionNavigationAuthority) {
        Self::record_navigation_committed(self, authority);
    }

    fn record_navigation_failed(
        &self,
        authority: &SessionNavigationAuthority,
        reason: NetworkFailureReason,
    ) {
        Self::record_navigation_failed(self, authority, reason);
    }

    fn record_navigation_failed_id(
        &self,
        navigation_id: SessionNavigationId,
        reason: NetworkFailureReason,
    ) {
        Self::record_navigation_failed_id(self, navigation_id, reason);
    }

    fn record_same_document_history_changed(&self, authority: &SessionNavigationAuthority) {
        Self::record_same_document_history_changed(self, authority);
    }

    fn record_settlement_terminal(&self, authority: &SessionNavigationAuthority) {
        Self::record_settlement_terminal(self, authority);
    }

    fn close(&mut self) {
        Self::close(self);
    }
}

struct ActiveRequest {
    request: Request,
    profile: Option<SessionProfile>,
    operation: ActiveOperation,
    started_at: Instant,
    in_flight: Option<DocumentControlCommand>,
    /// Servo generation immediately before the in-flight control command is submitted. Later Servo
    /// pumps may consume producer notifications before the typed response is pollable, so
    /// settlement waits must retain this older edge instead of the global cursor.
    control_turn_observed: Option<WakeGeneration>,
    needs_initial_pump: bool,
    state_effect: RequestStateEffect,
}

impl ActiveRequest {
    fn settle_host_wait(
        &mut self,
        fallback: WakeGeneration,
        started_at: Instant,
        deadline: Option<Instant>,
    ) -> SettleHostWait {
        SettleHostWait {
            observed: self.control_turn_observed.take().unwrap_or(fallback),
            started_at,
            deadline,
        }
    }
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
    Audit(AuditState),
    Navigate(NavigateState),
    SessionProjection(SessionProjectionState),
}

impl ActiveOperation {
    fn controlled_open_wall_authority(&self) -> Option<(SessionProfile, Instant)> {
        match self {
            Self::ControlledOpen(state) => Some((state.profile, state.deadline)),
            Self::Settle(SettleState {
                response:
                    SettleResponse::ControlledOpen {
                        profile, deadline, ..
                    },
                ..
            }) => Some((*profile, *deadline)),
            Self::SessionProjection(SessionProjectionState {
                kind:
                    SessionProjectionKind::ControlledOpen {
                        profile, deadline, ..
                    },
                ..
            }) => Some((*profile, *deadline)),
            _ => None,
        }
    }
}

struct AuditState {
    value: Option<Value>,
}

struct ControlledOpenState {
    requested_url: Url,
    current_url: Url,
    profile: SessionProfile,
    deadline: Instant,
    readiness_waiting: Option<ControlledOpenWait>,
    bootstrap_attempted: bool,
    settlement: Option<ControlledOpenSettlement>,
}

struct ControlledOpenSettlement {
    coordinator: settle::SettleCoordinator,
    cumulative_external_io_wall_time: Duration,
    waiting: Option<SettleHostWait>,
}

struct ControlledOpenWait {
    observed: WakeGeneration,
    retry_at: Instant,
}

enum AdvanceToNextState {
    Observing {
        expected_state_token: Option<wire::DocumentStateToken>,
        navigation: Option<SessionNavigationAuthority>,
    },
    Advancing {
        from_virtual_time_ns: u128,
    },
}

struct SettleState {
    profile: SessionProfile,
    /// Present only while `runtime.settle` brackets the exact latest public capability against a
    /// fresh owner navigation and document observation. Unlike strict action authorization, this
    /// permits monotonic internal generation progress on the identical document authority.
    authorizing_document_state: Option<wire::CurrentDocumentStateAuthority>,
    /// The exact completed document Observe is held until a passive no-pump N2 proves the same
    /// navigation authority as N1. Only then can it seed the settlement coordinator.
    authorizing_observation: Option<Box<servo::document_control::DocumentControlObservation>>,
    authorizing_navigation: Option<SessionNavigationAuthority>,
    replacement: Option<SettleReplacementPhase>,
    /// A coordinator command is held until a passive no-pump session observation binds the exact
    /// current document authority which may be replaced by a later mutating turn.
    authority_bound_command: Option<DocumentControlCommand>,
    latest_pending_target: Option<Box<servo::document_pending::PendingTargetObservation>>,
    response: SettleResponse,
    coordinator: settle::SettleCoordinator,
    effective_policy: wire::ResolvedSettlePolicy,
    cumulative_external_io_wall_time: Duration,
    waiting: Option<SettleHostWait>,
}

#[derive(Clone)]
enum SettleResponse {
    Runtime,
    /// A read-only `runtime.pending` projection which must be recomputed if document authority
    /// changes while its final N1/document/N2 bracket is in flight.
    Pending,
    Automation {
        kind: wire::PublicAutomationKind,
        result: DocumentAutomationResult,
    },
    ControlledOpen {
        requested_url: Url,
        current_url: Url,
        profile: SessionProfile,
        deadline: Instant,
        bootstrap_attempted: bool,
    },
    Navigate {
        requested_url: Url,
        source: SessionNavigationAuthority,
        admitted: SessionNavigationAuthority,
    },
}

enum SettleReplacementPhase {
    /// A typed in-process Drive result was bound to the token-authorizing document. No recovery
    /// is permitted until a passive, no-pump session observation proves one exact replacement
    /// admission.
    AwaitingAdmission {
        source: SessionNavigationAuthority,
        drive_outcome: DocumentControlReceiveOutcome,
    },
    /// The exact owner-attested pending pipeline is being admitted. Retaining both authorities and
    /// the command keeps a second or mismatched handoff from being mistaken for this one.
    Bootstrapping {
        source: SessionNavigationAuthority,
        admitted: SessionNavigationAuthority,
        command: DocumentControlCommand,
    },
    /// Bootstrap was consumed by the settlement coordinator. Subsequent controlled observations
    /// are bracketed with passive Constellation authority until the admitted pipeline becomes the
    /// sole controlled-ready document.
    Activating {
        source: SessionNavigationAuthority,
        admitted: SessionNavigationAuthority,
    },
    /// One post-bootstrap document-control completion is held exactly once while Constellation
    /// independently reports either the same admitted shape or its successful activation.
    AwaitingActivation {
        source: SessionNavigationAuthority,
        admitted: SessionNavigationAuthority,
        command: DocumentControlCommand,
        control_outcome: DocumentControlReceiveOutcome,
        controlled_network_active: bool,
    },
}

struct AutomationState {
    profile: SessionProfile,
    kind: wire::PublicAutomationKind,
    /// Present while the shell is obtaining fresh private target authority. The resolved public
    /// data is consumed exactly once when the Observe response is bound into an engine request.
    unresolved: Option<UnresolvedAutomationParams>,
    authorizing_navigation: Option<SessionNavigationAuthority>,
    /// A mutating v2 result already consumed from Script exactly once. Before exposing it, the
    /// shell passively brackets session authority so native activation/submission navigation can
    /// either be proven absent or carried through controlled-ready settlement.
    completed: Option<CompletedAutomation>,
}

struct CompletedAutomation {
    result: DocumentAutomationResult,
    pending: Box<servo::document_pending::RawPendingSnapshot>,
    synchronous_navigation_emitted: bool,
}

enum UnresolvedAutomationParams {
    Legacy(wire::ResolvedAutomationParams),
    Session(wire::ResolvedSessionAutomationParams),
}

struct NavigateState {
    requested_url: Url,
    phase: NavigatePhase,
    coordinator: settle::SettleCoordinator,
    cumulative_external_io_wall_time: Duration,
    waiting: Option<SettleHostWait>,
}

enum NavigatePhase {
    AwaitingAuthority {
        expected_state_token: wire::DocumentStateToken,
    },
    Authorizing {
        expected_state_token: wire::DocumentStateToken,
        navigation: SessionNavigationAuthority,
    },
    AwaitingAdmission {
        source: SessionNavigationAuthority,
        source_external_io_active_at_authorization: bool,
    },
    Settling {
        source: SessionNavigationAuthority,
        admitted: SessionNavigationAuthority,
    },
}

struct SessionProjectionState {
    pending: Box<servo::document_pending::RawPendingSnapshot>,
    kind: SessionProjectionKind,
    phase: SessionProjectionPhase,
}

enum SessionProjectionPhase {
    AwaitingInitialNavigation,
    AwaitingPendingObservation {
        navigation: SessionNavigationAuthority,
    },
    /// The document Observe raced an exact pending replacement after N1. Re-observe the owner
    /// authority and admit only the two pipeline identities carried by the typed rejection.
    AwaitingReplacementAdmission {
        source: SessionNavigationAuthority,
        source_pipeline_id: servo_base::id::PipelineId,
        pipeline_id: servo_base::id::PipelineId,
    },
    AwaitingStableNavigation {
        navigation: SessionNavigationAuthority,
    },
}

enum SessionProjectionKind {
    /// A mutating automation result is retained in its native action shape until a fresh pending
    /// observation is bracketed by two passive owner authorities. This prevents a pre-action
    /// pending snapshot from leaking its generation into the public result.
    Automation {
        settle_resume: SettleProjectionResume,
        replacement_rearm: bool,
    },
    Value {
        value: Value,
        snapshot_token: bool,
        settle_resume: Option<SettleProjectionResume>,
    },
    Navigate {
        requested_url: Url,
        source: SessionNavigationAuthority,
        admitted: SessionNavigationAuthority,
        cumulative_external_io_wall_time: Duration,
        settle_resume: Option<SettleProjectionResume>,
    },
    ControlledOpen {
        requested_url: Url,
        current_url: Url,
        profile: SessionProfile,
        deadline: Instant,
        bootstrap_attempted: bool,
        cumulative_external_io_wall_time: Duration,
        session_state_token: Option<SessionStateToken>,
        settle_resume: Option<SettleProjectionResume>,
    },
}

#[derive(Clone)]
struct SettleProjectionResume {
    profile: SessionProfile,
    effective_policy: wire::ResolvedSettlePolicy,
    cumulative_external_io_wall_time: Duration,
    authorizing_navigation: Option<SessionNavigationAuthority>,
    response: SettleResponse,
}

struct SettleHostWait {
    observed: WakeGeneration,
    started_at: Instant,
    deadline: Option<Instant>,
}

enum ActiveTransition {
    Submit(DocumentControlCommand),
    SubmitSessionNavigationObservation {
        allow_servo_pump: bool,
    },
    SubmitSessionNavigation {
        expected: SessionNavigationAuthority,
        url: Url,
    },
    ProjectSession(SessionProjectionState),
    WaitForControlledOpen,
    Wait(settle::SettleWait),
    Complete(Value),
    /// Reject a known-stale or freshly invalidated v2 settlement capability only after the shared
    /// active-transition preflight has given any sticky controlled-network terminal outcome
    /// precedence. This must never submit further engine work or enable Servo pumping.
    RejectStaleStateToken,
    Fail(ActiveFailure),
}

struct ActiveFailure {
    error: ProtocolError,
    fail_stop: bool,
}

fn controlled_open_timeout_failure(profile: SessionProfile) -> ActiveFailure {
    if profile.supports_session_api() {
        ActiveFailure {
            error: fatal_operation(
                "controlled_open_timeout",
                "the controlled session did not become ready before the wall deadline",
                "indeterminate",
            ),
            fail_stop: true,
        }
    } else {
        ActiveFailure {
            error: ProtocolError::operation(
                "controlled_open_timeout",
                "the controlled document did not become ready before the wall deadline",
                "none",
            ),
            fail_stop: false,
        }
    }
}

fn active_control_command_timeout(
    operation: &ActiveOperation,
    now: Instant,
) -> Result<Duration, ActiveFailure> {
    let Some((profile, deadline)) = operation.controlled_open_wall_authority() else {
        return Ok(CONTROL_COMMAND_TIMEOUT);
    };
    controlled_open_command_timeout(profile, deadline, now)
}

fn controlled_open_command_timeout(
    profile: SessionProfile,
    deadline: Instant,
    now: Instant,
) -> Result<Duration, ActiveFailure> {
    if now >= deadline {
        return Err(controlled_open_timeout_failure(profile));
    }
    Ok(CONTROL_COMMAND_TIMEOUT.min(deadline.duration_since(now)))
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
            let (navigation_progress, navigation_deadline) = self.poll_active_navigation()?;
            progressed |= navigation_progress;
            control_deadline = earliest_deadline(control_deadline, navigation_deadline);

            // Poll both typed response lanes before consulting the overall open deadline. A
            // response which was already ready at the boundary is therefore consumed exactly
            // once and may complete the open; only unfinished continuation work times out.
            if self.service_controlled_open_deadline(Instant::now())? {
                continue;
            }

            let before_pump = self.checked_wake_snapshot()?;
            let force_initial_pump = self
                .active
                .as_ref()
                .is_some_and(|active| active.needs_initial_pump);
            if self.engine.is_some() &&
                should_pump_servo(
                    self.active.as_ref(),
                    force_initial_pump,
                    before_pump.servo_changed_since(self.servo_cursor),
                )
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
                control_deadline = earliest_deadline(control_deadline, post_pump_deadline);
                let (post_nav_progress, post_nav_deadline) = self.poll_active_navigation()?;
                progressed |= post_nav_progress;
                control_deadline = earliest_deadline(control_deadline, post_nav_deadline);

                if self.service_controlled_open_deadline(Instant::now())? {
                    continue;
                }
            }

            let after_pump = self.checked_wake_snapshot()?;
            if self.service_active_host_wait(after_pump, Instant::now())? {
                progressed = true;
                let (_, deadline) = self.poll_active_control()?;
                control_deadline = earliest_deadline(control_deadline, deadline);
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
            ReaderMessage::CloseRequest { request, barrier } => {
                let result = self.handle(request);
                let disposition = match &result {
                    Ok(true) | Err(_) => ReaderCloseDisposition::Stop,
                    Ok(false) => ReaderCloseDisposition::Resume,
                };
                // `handle` has already flushed the accepted or rejected close response. The
                // reader may now either continue decoding or exit before the owner joins it.
                barrier.resolve(disposition);
                result
            },
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
            "action.activate" |
                "action.fill" |
                "action.focus" |
                "action.check" |
                "action.uncheck" |
                "action.select" |
                "action.submit" |
                "dom.query" |
                "dom.text" |
                "dom.extract"
        ) {
            return self.begin_automation_request(request).map(|()| false);
        }
        if method == "session.navigate" {
            return self.begin_session_navigate(request).map(|()| false);
        }
        if matches!(
            method.as_str(),
            "session.cookies.get" |
                "session.cookies.set" |
                "session.storage.get" |
                "session.storage.set" |
                "session.state.export" |
                "session.state.import"
        ) {
            return self.handle_session_state_request(request);
        }
        if matches!(method.as_str(), "session.requests" | "session.evidence") {
            return self.begin_session_audit(request).map(|()| false);
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

        let profile = self
            .profile
            .expect("a controlled session has a validated support profile");
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
                let expected_state_token = match profile {
                    SessionProfile::ControlledWebappV1 => {
                        if let Err(error) =
                            parse_params::<wire::RuntimeAdvanceToNextParams>(&request)
                        {
                            return self.write_method_result(&request, Err(error));
                        }
                        None
                    },
                    SessionProfile::ControlledWebSessionV1 |
                    SessionProfile::ControlledWebSessionV2 => {
                        match parse_params::<wire::SessionRuntimeAdvanceToNextParams>(&request) {
                            Ok(params) => Some(params.expected_state_token),
                            Err(error) => return self.write_method_result(&request, Err(error)),
                        }
                    },
                };
                (
                    ActiveOperation::AdvanceToNext(AdvanceToNextState::Observing {
                        expected_state_token,
                        navigation: None,
                    }),
                    if profile.supports_session_api() {
                        ActiveTransition::SubmitSessionNavigationObservation {
                            allow_servo_pump: true,
                        }
                    } else {
                        ActiveTransition::Submit(DocumentControlCommand::Observe)
                    },
                )
            },
            "runtime.settle" => {
                let resolved = match profile {
                    SessionProfile::ControlledWebappV1 => {
                        parse_params::<wire::RuntimeSettleParams>(&request).and_then(|params| {
                            params
                                .resolve(settle::SettlePolicy::default())
                                .map(|policy| (None, policy))
                                .map_err(|error| {
                                    ProtocolError::invalid_request(format!(
                                        "invalid settlement policy: {error:?}"
                                    ))
                                })
                        })
                    },
                    SessionProfile::ControlledWebSessionV1 |
                    SessionProfile::ControlledWebSessionV2 => parse_params::<
                        wire::SessionRuntimeSettleParams,
                    >(&request)
                    .and_then(|params| {
                        params
                            .resolve(settle::SettlePolicy::default())
                            .map(|(token, policy)| (Some(token), policy))
                            .map_err(|error| {
                                ProtocolError::invalid_request(format!(
                                    "invalid settlement policy: {error:?}"
                                ))
                            })
                    }),
                };
                let (expected_state_token, effective_policy) = match resolved {
                    Ok(resolved) => resolved,
                    Err(error) => {
                        return self.write_method_result(&request, Err(error));
                    },
                };
                let (authorizing_document_state, authorization_is_stale) =
                    match expected_state_token.as_ref() {
                        Some(expected) => {
                            match self.projection.current_document_state_authority(expected) {
                                Some(authority) => (Some(authority), false),
                                None => (None, true),
                            }
                        },
                        None => (None, false),
                    };
                let mut coordinator = settle::SettleCoordinator::new(effective_policy.engine);
                let progress = if authorization_is_stale {
                    ActiveTransition::RejectStaleStateToken
                } else if authorizing_document_state.is_some() {
                    ActiveTransition::SubmitSessionNavigationObservation {
                        allow_servo_pump: false,
                    }
                } else {
                    match coordinator.start() {
                        Ok(progress) => transition_from_settle_progress(progress),
                        Err(error) => ActiveTransition::Fail(settle_failure(
                            error,
                            RequestStateEffect::None,
                            None,
                        )),
                    }
                };
                (
                    ActiveOperation::Settle(SettleState {
                        profile,
                        authorizing_document_state,
                        authorizing_observation: None,
                        authorizing_navigation: None,
                        replacement: None,
                        authority_bound_command: None,
                        latest_pending_target: None,
                        response: SettleResponse::Runtime,
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
            profile: Some(profile),
            operation,
            started_at,
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::None,
        };
        self.apply_active_transition(active, first_progress)
    }

    fn begin_automation_request(&mut self, request: Request) -> Result<(), String> {
        if let Err(error) = self.require_controlled_session(&request) {
            return self.write_method_result(&request, Err(error));
        }

        let profile = self
            .profile
            .expect("a controlled session has a validated support profile");
        let invalid =
            |_| ProtocolError::invalid_request(format!("invalid {} parameters", request.method));
        let resolved: Result<UnresolvedAutomationParams, ProtocolError> = match profile {
            SessionProfile::ControlledWebappV1 => match request.method.as_str() {
                "action.activate" => parse_params::<wire::ActionActivateParams>(&request)
                    .and_then(|params| params.resolve().map_err(invalid))
                    .map(UnresolvedAutomationParams::Legacy),
                "action.fill" => parse_params::<wire::ActionFillParams>(&request)
                    .and_then(|params| params.resolve().map_err(invalid))
                    .map(UnresolvedAutomationParams::Legacy),
                "dom.query" => parse_params::<wire::DomQueryParams>(&request)
                    .and_then(|params| params.resolve().map_err(invalid))
                    .map(UnresolvedAutomationParams::Legacy),
                "dom.text" => parse_params::<wire::DomTextParams>(&request)
                    .and_then(|params| params.resolve().map_err(invalid))
                    .map(UnresolvedAutomationParams::Legacy),
                "dom.extract" => parse_params::<wire::DomExtractParams>(&request)
                    .and_then(|params| params.resolve().map_err(invalid))
                    .map(UnresolvedAutomationParams::Legacy),
                _ => Err(ProtocolError::operation(
                    "unsupported_profile_method",
                    format!(
                        "{} is not supported by {CONTROLLED_WEBAPP_V1_PROFILE}",
                        request.method
                    ),
                    "none",
                )),
            },
            SessionProfile::ControlledWebSessionV1 | SessionProfile::ControlledWebSessionV2 => {
                match request.method.as_str() {
                    "action.activate" => {
                        parse_params::<wire::SessionActionActivateParams>(&request)
                            .and_then(|params| params.resolve().map_err(invalid))
                            .map(UnresolvedAutomationParams::Session)
                    },
                    "action.fill" => parse_params::<wire::SessionActionFillParams>(&request)
                        .and_then(|params| params.resolve().map_err(invalid))
                        .map(UnresolvedAutomationParams::Session),
                    "action.focus" => parse_params::<wire::SessionActionFocusParams>(&request)
                        .and_then(|params| params.resolve().map_err(invalid))
                        .map(UnresolvedAutomationParams::Session),
                    "action.check" => parse_params::<wire::SessionActionCheckParams>(&request)
                        .and_then(|params| params.resolve().map_err(invalid))
                        .map(UnresolvedAutomationParams::Session),
                    "action.uncheck" => parse_params::<wire::SessionActionUncheckParams>(&request)
                        .and_then(|params| params.resolve().map_err(invalid))
                        .map(UnresolvedAutomationParams::Session),
                    "action.select" => parse_params::<wire::SessionActionSelectParams>(&request)
                        .and_then(|params| params.resolve().map_err(invalid))
                        .map(UnresolvedAutomationParams::Session),
                    "action.submit" => parse_params::<wire::SessionActionSubmitParams>(&request)
                        .and_then(|params| params.resolve().map_err(invalid))
                        .map(UnresolvedAutomationParams::Session),
                    "dom.query" => parse_params::<wire::SessionDomQueryParams>(&request)
                        .and_then(|params| params.resolve().map_err(invalid))
                        .map(UnresolvedAutomationParams::Session),
                    "dom.text" => parse_params::<wire::SessionDomTextParams>(&request)
                        .and_then(|params| params.resolve().map_err(invalid))
                        .map(UnresolvedAutomationParams::Session),
                    "dom.extract" => parse_params::<wire::SessionDomExtractParams>(&request)
                        .and_then(|params| params.resolve().map_err(invalid))
                        .map(UnresolvedAutomationParams::Session),
                    _ => unreachable!("automation method was filtered by the dispatcher"),
                }
            },
        };
        let resolved = match resolved {
            Ok(resolved) => resolved,
            Err(error) => return self.write_method_result(&request, Err(error)),
        };
        let kind = match &resolved {
            UnresolvedAutomationParams::Legacy(resolved) => resolved.kind(),
            UnresolvedAutomationParams::Session(resolved) => resolved.kind(),
        };
        let active = ActiveRequest {
            request,
            profile: Some(profile),
            operation: ActiveOperation::Automation(AutomationState {
                profile,
                kind,
                unresolved: Some(resolved),
                authorizing_navigation: None,
                completed: None,
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::None,
        };
        self.apply_active_transition(
            active,
            if profile.supports_session_api() {
                ActiveTransition::SubmitSessionNavigationObservation {
                    allow_servo_pump: true,
                }
            } else {
                ActiveTransition::Submit(DocumentControlCommand::Observe)
            },
        )
    }

    fn begin_session_navigate(&mut self, request: Request) -> Result<(), String> {
        if let Err(error) = self.require_controlled_session(&request) {
            return self.write_method_result(&request, Err(error));
        }
        if !self
            .profile
            .is_some_and(SessionProfile::supports_session_api)
        {
            return self.write_method_result(
                &request,
                Err(ProtocolError::operation(
                    "unsupported_profile_method",
                    format!(
                        "session.navigate requires {CONTROLLED_WEB_SESSION_V1_PROFILE} or {CONTROLLED_WEB_SESSION_V2_PROFILE}"
                    ),
                    "none",
                )),
            );
        }
        let params = match parse_params::<wire::SessionNavigateParams>(&request) {
            Ok(params) => params,
            Err(error) => return self.write_method_result(&request, Err(error)),
        };
        let requested_url = match Url::parse(&params.url) {
            Ok(url) if matches!(url.scheme(), "http" | "https") => url,
            Ok(_) => {
                return self.write_method_result(
                    &request,
                    Err(ProtocolError::operation(
                        "unsupported_navigation_scheme",
                        "session navigation supports only HTTP(S) URLs",
                        "none",
                    )),
                );
            },
            Err(error) => {
                return self.write_method_result(
                    &request,
                    Err(ProtocolError::invalid_request(format!(
                        "invalid navigation URL: {error}"
                    ))),
                );
            },
        };
        let effective_policy = wire::RuntimeSettleParams::default()
            .resolve(settle::SettlePolicy::default())
            .expect("the product default settlement policy is valid");
        let profile = self
            .profile
            .expect("a session navigation has a validated session profile");
        let active = ActiveRequest {
            request,
            profile: Some(profile),
            operation: ActiveOperation::Navigate(NavigateState {
                requested_url,
                phase: NavigatePhase::AwaitingAuthority {
                    expected_state_token: params.expected_state_token,
                },
                coordinator: settle::SettleCoordinator::new(effective_policy.engine),
                cumulative_external_io_wall_time: Duration::ZERO,
                waiting: None,
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::None,
        };
        self.apply_active_transition(
            active,
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: true,
            },
        )
    }

    fn handle_session_state_request(&mut self, request: Request) -> Result<bool, String> {
        if let Err(error) = self.require_controlled_web_session(&request) {
            self.write_method_result(&request, Err(error))?;
            return Ok(false);
        }
        let engine = self
            .engine
            .as_ref()
            .expect("a validated controlled session has an engine");
        let result = match request.method.as_str() {
            "session.cookies.get" => parse_params::<EmptyParams>(&request)
                .and_then(|_| engine.session_cookies_get())
                .and_then(serialize_immediate_result),
            "session.cookies.set" => parse_sensitive_params::<SessionCookiesSetParamsV1>(
                &request,
                "invalid session cookie parameters",
            )
            .and_then(|params| engine.session_cookies_set(params))
            .and_then(serialize_immediate_result),
            "session.storage.get" => parse_params::<EmptyParams>(&request)
                .and_then(|_| engine.session_storage_get())
                .and_then(serialize_immediate_result),
            "session.storage.set" => parse_sensitive_params::<SessionStorageSetParamsV1>(
                &request,
                "invalid session storage parameters",
            )
            .and_then(|params| engine.session_storage_set(params))
            .and_then(serialize_immediate_result),
            "session.state.export" => parse_params::<EmptyParams>(&request)
                .and_then(|_| engine.session_state_export())
                .and_then(serialize_immediate_result),
            "session.state.import" => Err(ProtocolError::operation(
                "session_state_import_phase_closed",
                "session.state.import is available only before session publication; use session.open state",
                "none",
            )),
            _ => unreachable!("session-state method was filtered by the dispatcher"),
        };
        let result = result
            .map_err(|error| harden_session_state_mutation_error(request.method.as_str(), error));
        let fatal = result.as_ref().err().is_some_and(|error| error.fatal);
        self.write_method_result(&request, result)?;
        if fatal {
            self.abortive_close();
            self.state = ShellState::Closed;
            return Err(
                "session-state authority failed terminally; session was fail-stopped".into(),
            );
        }
        Ok(false)
    }

    fn begin_session_audit(&mut self, request: Request) -> Result<(), String> {
        if let Err(error) = self.require_controlled_web_session(&request) {
            return self.write_method_result(&request, Err(error));
        }
        let params = match parse_params::<SessionAuditParams>(&request) {
            Ok(params) => params,
            Err(error) => return self.write_method_result(&request, Err(error)),
        };
        let after = params
            .after_seq
            .map(|sequence| EvidenceSequence::new(sequence.get()));
        let limit = params.limit.unwrap_or(DEFAULT_SESSION_AUDIT_PAGE_ITEMS);
        if limit == 0 || limit > HARD_SESSION_AUDIT_PAGE_ITEMS {
            return self.write_method_result(
                &request,
                Err(with_error_details(
                    ProtocolError::invalid_request(
                        "session audit limit is outside the public bound",
                    ),
                    json!({
                        "observed": limit.to_string(),
                        "limit": HARD_SESSION_AUDIT_PAGE_ITEMS,
                    }),
                )),
            );
        }
        let engine = self
            .engine
            .as_ref()
            .expect("a validated controlled session has an engine");
        let value = match request.method.as_str() {
            "session.requests" => engine
                .network_requests_page(after, limit)
                .and_then(serialize_immediate_result),
            "session.evidence" => engine
                .network_evidence_page(after, limit)
                .and_then(serialize_immediate_result),
            _ => unreachable!("session audit method was filtered by the dispatcher"),
        };
        let value = match value {
            Ok(value) => value,
            Err(error) => return self.write_method_result(&request, Err(error)),
        };
        let profile = self
            .profile
            .expect("a session audit has a validated session profile");
        let active = ActiveRequest {
            request,
            profile: Some(profile),
            operation: ActiveOperation::Audit(AuditState { value: Some(value) }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::None,
        };
        self.apply_active_transition(
            active,
            ActiveTransition::Submit(DocumentControlCommand::Observe),
        )
    }

    fn require_controlled_web_session(&self, request: &Request) -> Result<(), ProtocolError> {
        self.require_controlled_session(request)?;
        if !self
            .profile
            .is_some_and(SessionProfile::supports_session_api)
        {
            return Err(ProtocolError::operation(
                "unsupported_profile_method",
                format!(
                    "{} requires {CONTROLLED_WEB_SESSION_V1_PROFILE} or {CONTROLLED_WEB_SESSION_V2_PROFILE}",
                    request.method
                ),
                "none",
            ));
        }
        Ok(())
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
                if let ActiveOperation::Settle(state) = &mut active.operation &&
                    let Some(target) = receive_outcome_pending_target(&completion.outcome)
                {
                    state.latest_pending_target = Some(Box::new(target.clone()));
                }
                let replacement_source = if completion.disposition ==
                    ControlOutcomeDisposition::Indeterminate &&
                    command == DocumentControlCommand::DriveOneTurn &&
                    let DocumentControlReceiveOutcome::CommandOutcome(
                        DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate { target },
                    ) = &completion.outcome &&
                    let ActiveOperation::Settle(SettleState {
                        profile,
                        authorizing_document_state: None,
                        authorizing_observation: None,
                        authorizing_navigation: Some(source),
                        replacement: None,
                        ..
                    }) = &active.operation &&
                    profile.supports_session_api() &&
                    source.target() == target.as_ref()
                {
                    Some(source.clone())
                } else {
                    None
                };
                if let Some(source) = replacement_source {
                    let ActiveOperation::Settle(state) = &mut active.operation else {
                        unreachable!("the replacement source was taken only from settlement")
                    };
                    state.replacement = Some(SettleReplacementPhase::AwaitingAdmission {
                        source,
                        drive_outcome: completion.outcome,
                    });
                    // The source turn crossed its linearization point. It will be counted exactly
                    // once only after the passive session authority proves the navigation shape.
                    active.state_effect = RequestStateEffect::Partial;
                    self.apply_active_transition(
                        active,
                        ActiveTransition::SubmitSessionNavigationObservation {
                            allow_servo_pump: false,
                        },
                    )?;
                    return Ok((true, None));
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
                if active
                    .profile
                    .is_some_and(SessionProfile::supports_session_api) &&
                    matches!(
                        &completion.outcome,
                        DocumentControlReceiveOutcome::ObserveTransportFailure(_) |
                            DocumentControlReceiveOutcome::AutomationTransportFailure(_) |
                            DocumentControlReceiveOutcome::DriveOneTurnOutcomeIndeterminate(_)
                    )
                {
                    self.apply_active_transition(
                        active,
                        ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "session_command_transport_failure",
                                "an active controlled-session command lost its typed response",
                                "indeterminate",
                            ),
                            fail_stop: true,
                        }),
                    )?;
                    return Ok((true, None));
                }
                if active
                    .profile
                    .is_some_and(SessionProfile::supports_session_api) &&
                    completion.disposition == ControlOutcomeDisposition::Completed &&
                    let DocumentControlCommand::AdvanceTo(token) = &command &&
                    receive_outcome_virtual_time_ns(&completion.outcome) !=
                        Some(token.deadline().deadline.as_nanos())
                {
                    self.apply_active_transition(
                        active,
                        ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "controlled_network_time_authority_diverged",
                                "the completed document advance did not attest the virtual time authorized for its network evidence",
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
                // Never stage v2 network time from command intent. A rejected advance must
                // reobserve at the old clock; a completed advance reaches this point with its
                // exact deadline attested, before a later controlled Drive can run page work.
                if let Some(virtual_time_ns) = receive_outcome_virtual_time_ns(&completion.outcome) &&
                    let Err(mut error) = self
                        .engine
                        .as_ref()
                        .expect("a completed control operation has an engine")
                        .set_controlled_network_virtual_time_ns(virtual_time_ns)
                {
                    error.fatal = true;
                    self.apply_active_transition(
                        active,
                        ActiveTransition::Fail(ActiveFailure {
                            error,
                            fail_stop: true,
                        }),
                    )?;
                    return Ok((true, None));
                }
                let transition = transition_from_control_completion(
                    &mut active,
                    command,
                    completion.outcome,
                    &mut self.projection,
                    self.engine
                        .as_ref()
                        .and_then(EnginePort::controlled_network_snapshot)
                        .map_or(0, |snapshot| snapshot.active_operations),
                );
                self.apply_active_transition(active, transition)?;
                Ok((true, None))
            },
        }
    }

    fn poll_active_navigation(&mut self) -> Result<(bool, Option<Instant>), String> {
        let Some(active) = self.active.as_ref() else {
            return Ok((false, None));
        };
        if !active_expects_navigation_response(&active.operation) {
            return Ok((false, None));
        }
        let poll = self
            .engine
            .as_mut()
            .expect("an active controlled-session request has an engine")
            .poll_session_navigation();
        match poll {
            EnginePortNavigationPoll::Pending { deadline } => Ok((false, Some(deadline))),
            EnginePortNavigationPoll::Idle => {
                let active = self.active.take().expect("active request was observed");
                self.apply_active_transition(
                    active,
                    ActiveTransition::Fail(ActiveFailure {
                        error: fatal_operation(
                            "internal_runtime_failure",
                            "engine lost an in-flight session-navigation receiver",
                            "indeterminate",
                        ),
                        fail_stop: true,
                    }),
                )?;
                Ok((true, None))
            },
            EnginePortNavigationPoll::Complete(completion) => {
                let mut active = self.active.take().expect("active request was observed");
                active.needs_initial_pump = false;
                let controlled_network_active_operations = self
                    .engine
                    .as_ref()
                    .and_then(EnginePort::controlled_network_snapshot)
                    .map_or(0, |snapshot| snapshot.active_operations);
                if let Ok(navigation) = completion.outcome() {
                    let navigation = navigation.clone();
                    let engine = self
                        .engine
                        .as_ref()
                        .expect("an active controlled-session request has an engine");
                    if let Some(terminal) = navigation.terminal() {
                        let (navigation_id, reason) =
                            terminal_navigation_evidence(terminal, navigation.navigation_id());
                        if matches!(
                            &active.operation,
                            ActiveOperation::Navigate(NavigateState {
                                phase: NavigatePhase::AwaitingAdmission { .. },
                                ..
                            })
                        ) && completion.kind() == NavigationOperationKind::Navigate
                        {
                            engine.record_navigation_started_id(navigation_id);
                        }
                        engine.record_navigation_failed_id(navigation_id, reason);
                    } else {
                        match &mut active.operation {
                            ActiveOperation::Navigate(NavigateState {
                                phase: NavigatePhase::AwaitingAdmission { .. },
                                ..
                            }) if completion.kind() == NavigationOperationKind::Navigate => {
                                engine.record_navigation_started(&navigation);
                            },
                            ActiveOperation::Settle(SettleState {
                                replacement:
                                    Some(SettleReplacementPhase::AwaitingActivation {
                                        source,
                                        admitted,
                                        ..
                                    }),
                                ..
                            }) if completion.kind() == NavigationOperationKind::Observe &&
                                explicit_navigation_reached_controlled_ready(
                                    source,
                                    admitted,
                                    &navigation,
                                ) =>
                            {
                                if self
                                    .last_navigation_authority
                                    .as_ref()
                                    .is_none_or(|previous| {
                                        previous.navigation_id() != navigation.navigation_id()
                                    })
                                {
                                    engine.record_navigation_started(&navigation);
                                    engine.record_navigation_committed(&navigation);
                                }
                                if self
                                    .last_navigation_authority
                                    .as_ref()
                                    .is_some_and(|previous| {
                                        previous.history_revision() != navigation.history_revision()
                                    })
                                {
                                    engine.record_same_document_history_changed(&navigation);
                                }
                                self.last_navigation_authority = Some(navigation.clone());
                            },
                            ActiveOperation::SessionProjection(state)
                                if completion.kind() == NavigationOperationKind::Observe =>
                            {
                                let stable = matches!(
                                    &state.phase,
                                    SessionProjectionPhase::AwaitingStableNavigation {
                                        navigation: before,
                                    } if before == &navigation
                                );
                                if !stable {
                                    // N1 establishes the authority to be bracketed around the final
                                    // document Observe. Only the matching no-pump N2 is terminal.
                                    let transition = transition_from_navigation_completion(
                                        &mut active,
                                        completion,
                                        &mut self.projection,
                                        controlled_network_active_operations,
                                    );
                                    self.apply_active_transition(active, transition)?;
                                    return Ok((true, None));
                                }
                                match &mut state.kind {
                                    SessionProjectionKind::ControlledOpen {
                                        session_state_token,
                                        settle_resume,
                                        ..
                                    } => {
                                        let controlled_ready = if settle_resume.is_some() {
                                            session_navigation_reached_controlled_ready(&navigation)
                                        } else {
                                            initial_navigation_reached_controlled_ready(&navigation)
                                        };
                                        if !controlled_ready {
                                            let transition = navigation_activation_failure(
                                                active.state_effect,
                                                true,
                                            );
                                            self.apply_active_transition(active, transition)?;
                                            return Ok((true, None));
                                        }
                                        match engine.session_state_token() {
                                            Ok(token) => *session_state_token = Some(token),
                                            Err(mut error) => {
                                                error.fatal = true;
                                                self.apply_active_transition(
                                                    active,
                                                    ActiveTransition::Fail(ActiveFailure {
                                                        error,
                                                        fail_stop: true,
                                                    }),
                                                )?;
                                                return Ok((true, None));
                                            },
                                        }
                                        if self.last_navigation_authority.as_ref().is_none_or(
                                            |previous| {
                                                previous.navigation_id() !=
                                                    navigation.navigation_id()
                                            },
                                        ) {
                                            engine.record_navigation_started(&navigation);
                                            engine.record_navigation_committed(&navigation);
                                        }
                                        engine.record_settlement_terminal(&navigation);
                                    },
                                    SessionProjectionKind::Navigate {
                                        source,
                                        admitted,
                                        settle_resume,
                                        ..
                                    } => {
                                        let controlled_ready = if settle_resume.is_some() {
                                            explicit_navigation_chain_reached_controlled_ready(
                                                source,
                                                &navigation,
                                            )
                                        } else {
                                            explicit_navigation_reached_controlled_ready(
                                                source,
                                                admitted,
                                                &navigation,
                                            )
                                        };
                                        if !controlled_ready {
                                            let transition = navigation_activation_failure(
                                                active.state_effect,
                                                true,
                                            );
                                            self.apply_active_transition(active, transition)?;
                                            return Ok((true, None));
                                        }
                                        if settle_resume.is_none() ||
                                            self.last_navigation_authority.as_ref().is_none_or(
                                                |previous| {
                                                    previous.navigation_id() !=
                                                        navigation.navigation_id()
                                                },
                                            )
                                        {
                                            engine.record_navigation_committed(&navigation);
                                        }
                                        if admitted.history_revision() !=
                                            navigation.history_revision()
                                        {
                                            engine
                                                .record_same_document_history_changed(&navigation);
                                        }
                                        engine.record_settlement_terminal(&navigation);
                                    },
                                    SessionProjectionKind::Automation { .. } |
                                    SessionProjectionKind::Value { .. } => {
                                        if let Some(previous) =
                                            self.last_navigation_authority.as_ref()
                                        {
                                            if previous.navigation_id() !=
                                                navigation.navigation_id()
                                            {
                                                engine.record_navigation_started(&navigation);
                                                engine.record_navigation_committed(&navigation);
                                            }
                                            if previous.history_revision() !=
                                                navigation.history_revision()
                                            {
                                                engine.record_same_document_history_changed(
                                                    &navigation,
                                                );
                                            }
                                        }
                                        if active.request.method == "runtime.settle" {
                                            engine.record_settlement_terminal(&navigation);
                                        }
                                    },
                                }
                                self.last_navigation_authority = Some(navigation);
                            },
                            _ => {},
                        }
                    }
                } else {
                    let engine = self
                        .engine
                        .as_ref()
                        .expect("navigation failure has an engine");
                    match completion.outcome() {
                        Err(SessionNavigationError::NavigationStartFailed { observed }) => {
                            engine.record_navigation_started(observed);
                            engine.record_navigation_failed(
                                observed,
                                NetworkFailureReason::NetworkError,
                            );
                        },
                        Err(SessionNavigationError::Terminal(terminal)) => {
                            let fallback = self
                                .last_navigation_authority
                                .as_ref()
                                .map_or(SessionNavigationId::new(0), |authority| {
                                    authority.navigation_id()
                                });
                            let (navigation_id, reason) =
                                terminal_navigation_evidence(*terminal, fallback);
                            engine.record_navigation_failed_id(navigation_id, reason);
                        },
                        _ => {},
                    }
                }
                let transition = transition_from_navigation_completion(
                    &mut active,
                    completion,
                    &mut self.projection,
                    controlled_network_active_operations,
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
        if let ActiveOperation::ControlledOpen(state) = &active.operation &&
            let Some(wait) = state.readiness_waiting.as_ref()
        {
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
            state.readiness_waiting = None;
            let transition = if deadline_expired {
                ActiveTransition::Fail(controlled_open_timeout_failure(state.profile))
            } else {
                ActiveTransition::Submit(DocumentControlCommand::Observe)
            };
            self.apply_active_transition(active, transition)?;
            return Ok(true);
        }
        let wait = match &active.operation {
            ActiveOperation::ControlledOpen(state) => state
                .settlement
                .as_ref()
                .and_then(|settlement| settlement.waiting.as_ref()),
            ActiveOperation::Settle(state) => state.waiting.as_ref(),
            ActiveOperation::Navigate(state)
                if matches!(state.phase, NavigatePhase::Settling { .. }) =>
            {
                state.waiting.as_ref()
            },
            _ => None,
        };
        let Some(wait) = wait else {
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
        let controlled_network_active = self
            .engine
            .as_ref()
            .and_then(EnginePort::controlled_network_snapshot)
            .is_some_and(|snapshot| snapshot.active_operations != 0);
        let (wait, previous_cumulative) = match &mut active.operation {
            ActiveOperation::ControlledOpen(state) => {
                let settlement = state
                    .settlement
                    .as_mut()
                    .expect("controlled-open settlement wait has coordinator state");
                (
                    settlement
                        .waiting
                        .take()
                        .expect("controlled-open settle wait was observed"),
                    settlement.cumulative_external_io_wall_time,
                )
            },
            ActiveOperation::Settle(state) => (
                state.waiting.take().expect("settle wait was observed"),
                state.cumulative_external_io_wall_time,
            ),
            ActiveOperation::Navigate(state) => (
                state.waiting.take().expect("navigate wait was observed"),
                state.cumulative_external_io_wall_time,
            ),
            _ => unreachable!("settlement wait changed operation kind"),
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
        let transition = match &mut active.operation {
            ActiveOperation::ControlledOpen(state) => {
                let settlement = state
                    .settlement
                    .as_mut()
                    .expect("controlled-open settlement is active");
                settlement.cumulative_external_io_wall_time = cumulative;
                settlement
                    .coordinator
                    .set_additional_foreground_external_io_active(controlled_network_active);
                let progress = if expired {
                    settlement.coordinator.external_io_wait_expired(cumulative)
                } else {
                    settlement.coordinator.resume_after_wake(cumulative)
                };
                match progress {
                    Ok(progress) => transition_from_controlled_open_settle_progress(
                        state,
                        progress,
                        state_effect,
                    ),
                    Err(error) => ActiveTransition::Fail(settle_failure(error, state_effect, None)),
                }
            },
            ActiveOperation::Settle(state) => {
                state.cumulative_external_io_wall_time = cumulative;
                state
                    .coordinator
                    .set_additional_foreground_external_io_active(
                        controlled_network_active || state.replacement.is_some(),
                    );
                let progress = if expired {
                    state.coordinator.external_io_wait_expired(cumulative)
                } else {
                    state.coordinator.resume_after_wake(cumulative)
                };
                match progress {
                    Ok(progress) => transition_from_settle_progress_for_active(
                        state,
                        started_at,
                        progress,
                        state_effect,
                        &mut self.projection,
                    ),
                    Err(error) => ActiveTransition::Fail(settle_failure_for_response(
                        error,
                        state_effect,
                        None,
                        &state.response,
                    )),
                }
            },
            ActiveOperation::Navigate(state) => {
                state.cumulative_external_io_wall_time = cumulative;
                state
                    .coordinator
                    .set_additional_foreground_external_io_active(controlled_network_active);
                let progress = if expired {
                    state.coordinator.external_io_wait_expired(cumulative)
                } else {
                    state.coordinator.resume_after_wake(cumulative)
                };
                match progress {
                    Ok(progress) => {
                        transition_from_navigate_settle_progress(state, progress, state_effect)
                    },
                    Err(error) => ActiveTransition::Fail(settle_failure(error, state_effect, None)),
                }
            },
            _ => unreachable!("settlement wait changed operation kind"),
        };
        self.apply_active_transition(active, transition)?;
        Ok(true)
    }

    fn service_controlled_open_deadline(&mut self, now: Instant) -> Result<bool, String> {
        let Some((profile, deadline)) = self
            .active
            .as_ref()
            .and_then(|active| active.operation.controlled_open_wall_authority())
        else {
            return Ok(false);
        };
        if now < deadline {
            return Ok(false);
        }

        let active = self
            .active
            .take()
            .expect("controlled-open wall authority was observed");
        self.apply_active_transition(
            active,
            ActiveTransition::Fail(controlled_open_timeout_failure(profile)),
        )?;
        Ok(true)
    }

    fn apply_active_transition(
        &mut self,
        mut active: ActiveRequest,
        transition: ActiveTransition,
    ) -> Result<(), String> {
        if !matches!(&transition, ActiveTransition::Fail(_)) &&
            !matches!(&active.operation, ActiveOperation::Audit(_)) &&
            let Some(snapshot) = self
                .engine
                .as_ref()
                .and_then(EnginePort::controlled_network_snapshot) &&
            let Some(mut failure) = controlled_network_failure(snapshot, active.state_effect)
        {
            if matches!(&active.operation, ActiveOperation::ControlledOpen(_)) {
                failure.error.fatal = true;
                failure.fail_stop = true;
            }
            return self.apply_active_transition(active, ActiveTransition::Fail(failure));
        }
        match transition {
            ActiveTransition::Submit(command) => {
                let controlled_open = active.operation.controlled_open_wall_authority().is_some();
                let timeout =
                    match active_control_command_timeout(&active.operation, Instant::now()) {
                        Ok(timeout) => timeout,
                        Err(failure) => {
                            return self
                                .apply_active_transition(active, ActiveTransition::Fail(failure));
                        },
                    };
                let control_turn_observed = self.checked_wake_snapshot()?;
                let submission = self
                    .engine
                    .as_mut()
                    .expect("runtime request has an engine")
                    .submit_document_control(command.clone(), timeout);
                match submission {
                    Ok(()) => {
                        active.in_flight = Some(command);
                        active.control_turn_observed = Some(control_turn_observed);
                        active.needs_initial_pump = true;
                        if let ActiveOperation::Settle(state) = &mut active.operation {
                            state.waiting = None;
                        }
                        if let ActiveOperation::ControlledOpen(state) = &mut active.operation &&
                            let Some(settlement) = state.settlement.as_mut()
                        {
                            settlement.waiting = None;
                        }
                        if let ActiveOperation::Navigate(state) = &mut active.operation {
                            state.waiting = None;
                        }
                        self.active = Some(active);
                        Ok(())
                    },
                    Err(mut error) => {
                        error.state_effect = active.state_effect.as_protocol_str();
                        let v2 = active
                            .profile
                            .is_some_and(SessionProfile::supports_session_api);
                        if v2 {
                            error.fatal = true;
                        }
                        if controlled_open {
                            self.close_engine();
                            self.state = if v2 {
                                ShellState::Closed
                            } else {
                                ShellState::Initialized
                            };
                        }
                        self.write_method_result(&active.request, Err(error))?;
                        if v2 {
                            self.abortive_close();
                            self.state = ShellState::Closed;
                            return Err(
                                "controlled-session command submission failed terminally".into()
                            );
                        }
                        Ok(())
                    },
                }
            },
            ActiveTransition::SubmitSessionNavigationObservation { allow_servo_pump } => {
                let timeout =
                    match active_control_command_timeout(&active.operation, Instant::now()) {
                        Ok(timeout) => timeout,
                        Err(failure) => {
                            return self
                                .apply_active_transition(active, ActiveTransition::Fail(failure));
                        },
                    };
                let submission = self
                    .engine
                    .as_mut()
                    .expect("controlled-session request has an engine")
                    .submit_session_navigation_observation(timeout);
                match submission {
                    Ok(()) => {
                        active.needs_initial_pump = allow_servo_pump;
                        self.active = Some(active);
                        Ok(())
                    },
                    Err(mut error) => {
                        error.state_effect = active.state_effect.as_protocol_str();
                        let v2 = active
                            .profile
                            .is_some_and(SessionProfile::supports_session_api);
                        if v2 {
                            error.fatal = true;
                        }
                        self.write_method_result(&active.request, Err(error))?;
                        if v2 {
                            self.abortive_close();
                            self.state = ShellState::Closed;
                            return Err(
                                "session-navigation observation submission failed terminally"
                                    .into(),
                            );
                        }
                        Ok(())
                    },
                }
            },
            ActiveTransition::SubmitSessionNavigation { expected, url } => {
                let timeout =
                    match active_control_command_timeout(&active.operation, Instant::now()) {
                        Ok(timeout) => timeout,
                        Err(failure) => {
                            return self
                                .apply_active_transition(active, ActiveTransition::Fail(failure));
                        },
                    };
                let submission = self
                    .engine
                    .as_mut()
                    .expect("controlled-session request has an engine")
                    .submit_session_navigation(expected, url, timeout);
                match submission {
                    Ok(()) => {
                        active.needs_initial_pump = true;
                        self.active = Some(active);
                        Ok(())
                    },
                    Err(mut error) => {
                        error.state_effect = active.state_effect.as_protocol_str();
                        self.write_method_result(&active.request, Err(error))
                    },
                }
            },
            ActiveTransition::ProjectSession(state) => {
                let allow_servo_pump = !session_projection_suppresses_initial_pump(&state.kind);
                active.operation = ActiveOperation::SessionProjection(state);
                self.apply_active_transition(
                    active,
                    ActiveTransition::SubmitSessionNavigationObservation { allow_servo_pump },
                )
            },
            ActiveTransition::WaitForControlledOpen => {
                let now = Instant::now();
                if let Err(failure) = active_control_command_timeout(&active.operation, now) {
                    return self.apply_active_transition(active, ActiveTransition::Fail(failure));
                }
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
                state.readiness_waiting = Some(ControlledOpenWait {
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
                let controlled_open_deadline =
                    match active.operation.controlled_open_wall_authority() {
                        Some((profile, deadline)) if now >= deadline => {
                            return self.apply_active_transition(
                                active,
                                ActiveTransition::Fail(controlled_open_timeout_failure(profile)),
                            );
                        },
                        Some((_, deadline)) => Some(deadline),
                        None => None,
                    };
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
                        Some(controlled_open_deadline.map_or(deadline, |open_deadline| {
                            Instant::min(deadline, open_deadline)
                        }))
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
                        Some(controlled_open_deadline.map_or(deadline, |open_deadline| {
                            Instant::min(deadline, open_deadline)
                        }))
                    },
                };
                let host_wait = active.settle_host_wait(self.servo_cursor, now, deadline);
                match &mut active.operation {
                    ActiveOperation::ControlledOpen(state) => {
                        let Some(settlement) = state.settlement.as_mut() else {
                            return self.apply_active_transition(
                                active,
                                ActiveTransition::Fail(ActiveFailure {
                                    error: fatal_operation(
                                        "internal_runtime_failure",
                                        "controlled open entered settlement wait before settlement started",
                                        "none",
                                    ),
                                    fail_stop: true,
                                }),
                            );
                        };
                        settlement.waiting = Some(host_wait)
                    },
                    ActiveOperation::Settle(state) => state.waiting = Some(host_wait),
                    ActiveOperation::Navigate(state)
                        if matches!(state.phase, NavigatePhase::Settling { .. }) =>
                    {
                        state.waiting = Some(host_wait)
                    },
                    _ => {
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
                    },
                }
                self.active = Some(active);
                Ok(())
            },
            ActiveTransition::Complete(value) => {
                self.write_method_result(&active.request, Ok(value))
            },
            ActiveTransition::RejectStaleStateToken => self.apply_active_transition(
                active,
                ActiveTransition::Fail(ActiveFailure {
                    error: stale_state_token_error(),
                    fail_stop: false,
                }),
            ),
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
        let navigation_completion = self
            .engine
            .as_mut()
            .and_then(|engine| engine.cancel_session_navigation());
        if active
            .profile
            .is_some_and(SessionProfile::supports_session_api)
        {
            return ActiveFailure {
                error: fatal_operation(
                    "outcome_indeterminate",
                    "cancellation abandoned an active controlled-session command sequence",
                    "indeterminate",
                ),
                fail_stop: true,
            };
        }
        if command_is_mutating ||
            completion.as_ref().is_some_and(|completion| {
                completion.disposition == ControlOutcomeDisposition::Indeterminate
            }) ||
            navigation_completion.as_ref().is_some_and(|completion| {
                completion.kind() == NavigationOperationKind::Navigate &&
                    !completion.response_received()
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
                    ReaderMessage::CloseRequest { request, barrier } => {
                        self.write_method_result(
                            &request,
                            Err(ProtocolError::operation(
                                "session_closing",
                                "session closed before the request started",
                                "none",
                            )),
                        )?;
                        barrier.resolve(ReaderCloseDisposition::Stop);
                    },
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
        let active = self.active.as_ref();
        let host_wait = active.and_then(|active| match &active.operation {
            ActiveOperation::ControlledOpen(state) => state.readiness_waiting.as_ref().map_or_else(
                || {
                    state
                        .settlement
                        .as_ref()
                        .and_then(|settlement| settlement.waiting.as_ref())
                        .and_then(|wait| wait.deadline)
                },
                |wait| Some(Instant::min(wait.retry_at, state.deadline)),
            ),
            ActiveOperation::Settle(state) => state.waiting.as_ref().and_then(|wait| wait.deadline),
            ActiveOperation::Navigate(state) => {
                state.waiting.as_ref().and_then(|wait| wait.deadline)
            },
            ActiveOperation::Pending |
            ActiveOperation::AdvanceToNext(_) |
            ActiveOperation::Automation(_) |
            ActiveOperation::Audit(_) |
            ActiveOperation::SessionProjection(_) => None,
        });
        let controlled_open_deadline = active.and_then(|active| {
            active
                .operation
                .controlled_open_wall_authority()
                .map(|(_, deadline)| deadline)
        });
        [control_deadline, host_wait, controlled_open_deadline]
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
                    "session.navigate",
                    "action.activate",
                    "action.fill",
                    "action.focus",
                    "action.check",
                    "action.uncheck",
                    "action.select",
                    "action.submit",
                    "dom.query",
                    "dom.text",
                    "dom.extract",
                    "session.cookies.get",
                    "session.cookies.set",
                    "session.storage.get",
                    "session.storage.set",
                    "session.state.export",
                    "session.state.import",
                    "session.requests",
                    "session.evidence",
                    "protocol.cancel",
                    "session.close"
                ],
                "clockModes": ["real", "controlled"],
                "profiles": [
                    CONTROLLED_WEBAPP_V1_PROFILE,
                    CONTROLLED_WEB_SESSION_V1_PROFILE,
                    CONTROLLED_WEB_SESSION_V2_PROFILE
                ],
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
        let params: OpenParams =
            match parse_sensitive_params(&request, "invalid session.open parameters") {
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
        let configuration = match params.configuration() {
            Ok(configuration) => configuration,
            Err(error) => return self.write_method_result(&request, Err(error)),
        };
        if configuration
            .profile
            .is_some_and(SessionProfile::supports_session_api) &&
            !matches!(url.scheme(), "http" | "https")
        {
            return self.write_method_result(
                &request,
                Err(ProtocolError::operation(
                    "unsupported_navigation_scheme",
                    "initial session navigation supports only HTTP(S) URLs",
                    "none",
                )),
            );
        }
        let document_control_profile = configuration.profile.map_or(
            DocumentControlProfile::SingleDocument,
            SessionProfile::document_control_profile,
        );
        let document_execution_profile = configuration.profile.map_or(
            DocumentExecutionProfile::Baseline,
            SessionProfile::document_execution_profile,
        );
        let controlled_open_timing = if configuration.clock_mode.is_controlled() {
            let started_at = Instant::now();
            let Some(deadline) = started_at.checked_add(CONTROLLED_OPEN_WALL_TIMEOUT) else {
                return self.write_method_result(
                    &request,
                    Err(ProtocolError::operation(
                        "controlled_open_deadline_overflow",
                        "the controlled-open wall deadline overflowed",
                        "none",
                    )),
                );
            };
            Some((started_at, deadline))
        } else {
            None
        };
        let mut engine = match E::open_session(
            url.clone(),
            self.waker.clone(),
            EngineSessionOpenOptions {
                clock_mode: configuration.clock_mode,
                document_control_profile,
                document_execution_profile,
                state: configuration.state,
                network: configuration.network,
            },
        ) {
            Ok(engine) => engine,
            Err(error) => {
                if let Some((_, deadline)) = controlled_open_timing &&
                    Instant::now() >= deadline
                {
                    let profile = configuration
                        .profile
                        .expect("a controlled open has a validated support profile");
                    let failure = controlled_open_timeout_failure(profile);
                    self.write_method_result(&request, Err(failure.error))?;
                    if failure.fail_stop {
                        self.state = ShellState::Closed;
                        return Err(
                            "runtime outcome is indeterminate; session was fail-stopped".into()
                        );
                    }
                    return Ok(());
                }
                let fatal = error.fatal;
                self.write_method_result(&request, Err(error))?;
                if fatal {
                    self.state = ShellState::Closed;
                    return Err(
                        "session construction failed at a terminal unpublished boundary".into(),
                    );
                }
                return Ok(());
            },
        };
        let final_url = engine.url().unwrap_or_else(|| url.clone());

        if configuration.clock_mode.is_controlled() {
            let profile = configuration
                .profile
                .expect("a controlled open has a validated support profile");
            let (started_at, deadline) = controlled_open_timing
                .expect("a controlled open established its wall deadline before engine creation");
            if Instant::now() >= deadline {
                engine.close();
                let failure = controlled_open_timeout_failure(profile);
                self.write_method_result(&request, Err(failure.error))?;
                if failure.fail_stop {
                    self.state = ShellState::Closed;
                    return Err("runtime outcome is indeterminate; session was fail-stopped".into());
                }
                return Ok(());
            }
            if engine.document_control_profile() != profile.document_control_profile() {
                engine.close();
                return self.write_method_result(
                    &request,
                    Err(fatal_operation(
                        "internal_runtime_failure",
                        "engine opened with a different document-control profile",
                        "none",
                    )),
                );
            }
            if engine.document_execution_profile() != profile.document_execution_profile() {
                engine.close();
                return self.write_method_result(
                    &request,
                    Err(fatal_operation(
                        "internal_runtime_failure",
                        "engine opened with a different document-execution profile",
                        "none",
                    )),
                );
            }
            let initial_timeout =
                match controlled_open_command_timeout(profile, deadline, Instant::now()) {
                    Ok(timeout) => timeout,
                    Err(failure) => {
                        engine.close();
                        self.write_method_result(&request, Err(failure.error))?;
                        if failure.fail_stop {
                            self.state = ShellState::Closed;
                            return Err(
                                "runtime outcome is indeterminate; session was fail-stopped".into(),
                            );
                        }
                        return Ok(());
                    },
                };
            let control_turn_observed = self.checked_wake_snapshot()?;
            if let Err(mut error) =
                engine.submit_document_control(DocumentControlCommand::Observe, initial_timeout)
            {
                error.state_effect = "none";
                engine.close();
                return self.write_method_result(&request, Err(error));
            }
            self.projection = wire::WireProjectionContext::new();
            self.last_navigation_authority = None;
            self.engine.replace(engine);
            self.profile = Some(profile);
            self.state = ShellState::Open;
            self.active = Some(ActiveRequest {
                request,
                profile: Some(profile),
                operation: ActiveOperation::ControlledOpen(ControlledOpenState {
                    requested_url: url,
                    current_url: final_url,
                    profile,
                    deadline,
                    readiness_waiting: None,
                    bootstrap_attempted: false,
                    settlement: None,
                }),
                started_at,
                in_flight: Some(DocumentControlCommand::Observe),
                control_turn_observed: Some(control_turn_observed),
                needs_initial_pump: true,
                state_effect: RequestStateEffect::None,
            });
            return Ok(());
        }

        self.engine.replace(engine);
        self.profile = None;
        self.state = ShellState::Open;
        self.write_method_result(
            &request,
            Ok(json!({
                "sessionId": SESSION_ID,
                "requestedUrl": url,
                "url": final_url,
                "boundary": configuration.boundary,
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
    controlled_network_active_operations: usize,
) -> ActiveTransition {
    let state_effect = active.state_effect;
    let active_profile = active.profile;
    match &mut active.operation {
        ActiveOperation::ControlledOpen(state) => {
            if let Some(settlement) = state.settlement.as_mut() {
                settlement
                    .coordinator
                    .set_additional_foreground_external_io_active(
                        controlled_network_active_operations != 0,
                    );
                let progress = settlement
                    .coordinator
                    .consume_receive_outcome(outcome, settlement.cumulative_external_io_wall_time);
                return match progress {
                    Ok(progress) => transition_from_controlled_open_settle_progress(
                        state,
                        progress,
                        state_effect,
                    ),
                    Err(error) => {
                        ActiveTransition::Fail(settle_failure(error, state_effect, Some(&command)))
                    },
                };
            }
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
            if state.profile.supports_session_api() {
                let response = SettleResponse::ControlledOpen {
                    requested_url: state.requested_url.clone(),
                    current_url: state.current_url.clone(),
                    profile: state.profile,
                    deadline: state.deadline,
                    bootstrap_attempted: state.bootstrap_attempted,
                };
                let effective_policy = default_resolved_settle_policy();
                let mut coordinator = settle::SettleCoordinator::new(effective_policy.engine);
                let command = match coordinator.start() {
                    Ok(settle::SettleProgress::Command(command)) => command,
                    Ok(_) => {
                        return ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "internal_runtime_failure",
                                "controlled open settlement did not request its initial observation",
                                state_effect.as_protocol_str(),
                            ),
                            fail_stop: true,
                        });
                    },
                    Err(error) => {
                        return ActiveTransition::Fail(harden_continuation_failure(
                            settle_failure(error, state_effect, None),
                            state_effect,
                        ));
                    },
                };
                active.operation = ActiveOperation::Settle(SettleState {
                    profile: state.profile,
                    authorizing_document_state: None,
                    authorizing_observation: None,
                    authorizing_navigation: None,
                    replacement: None,
                    authority_bound_command: Some(command),
                    latest_pending_target: Some(Box::new(observation.pending().target.clone())),
                    response,
                    coordinator,
                    effective_policy,
                    cumulative_external_io_wall_time: Duration::ZERO,
                    waiting: None,
                });
                ActiveTransition::SubmitSessionNavigationObservation {
                    allow_servo_pump: false,
                }
            } else {
                ActiveTransition::Complete(json!({
                    "sessionId": SESSION_ID,
                    "requestedUrl": state.requested_url,
                    "url": state.current_url,
                    "boundary": "controlled_ready",
                    "clockMode": "controlled",
                    "profile": state.profile.id(),
                }))
            }
        },
        ActiveOperation::Pending => {
            let observation = match completed_observation(outcome, &command, state_effect) {
                Ok(observation) => observation,
                Err(failure) => return ActiveTransition::Fail(failure),
            };
            let result = wire::RuntimePendingResult::project(observation.pending(), projection);
            match active_profile {
                Some(profile) if profile.supports_session_api() => {
                    project_session_pending(profile, result, observation.pending(), state_effect)
                },
                _ => serialize_result(result, state_effect),
            }
        },
        ActiveOperation::AdvanceToNext(state) => {
            let observation = match completed_observation(outcome, &command, state_effect) {
                Ok(observation) => observation,
                Err(failure) => return ActiveTransition::Fail(failure),
            };
            match state {
                AdvanceToNextState::Observing {
                    expected_state_token,
                    navigation,
                } => {
                    if let Some(expected_state_token) = expected_state_token.as_ref() {
                        let Some(navigation) = navigation.as_ref() else {
                            return missing_navigation_authority("runtime.advance_to_next");
                        };
                        match projection.authorizes_document_state(
                            observation.pending(),
                            navigation,
                            expected_state_token,
                        ) {
                            Ok(true) => {},
                            Ok(false) => return stale_state_token(),
                            Err(error) => return document_authority_authorization_failure(error),
                        }
                    }
                    if controlled_network_blocks_virtual_advance(
                        active_profile,
                        controlled_network_active_operations,
                    ) {
                        return ActiveTransition::Fail(ActiveFailure {
                            error: ProtocolError::operation(
                                "advance_not_available",
                                "virtual time is frozen while controlled network operations are active",
                                "none",
                            ),
                            fail_stop: false,
                        });
                    }
                    if observation.pending().scheduler.next_deadline.is_none() {
                        let result = wire::RuntimeAdvanceToNextResult::project(
                            wire::RuntimeAdvanceToNextFacts::NoFiniteDeadline {
                                final_snapshot: observation.pending(),
                            },
                            projection,
                        );
                        return match active_profile {
                            Some(profile) if profile.supports_session_api() => {
                                project_session_value(
                                    result,
                                    observation.pending(),
                                    true,
                                    state_effect,
                                )
                            },
                            _ => serialize_result(result, state_effect),
                        };
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
                } => {
                    let result = wire::RuntimeAdvanceToNextResult::project(
                        wire::RuntimeAdvanceToNextFacts::Advanced {
                            from_virtual_time_ns: *from_virtual_time_ns,
                            final_snapshot: observation.pending(),
                        },
                        projection,
                    );
                    match active_profile {
                        Some(profile) if profile.supports_session_api() => {
                            project_session_value(result, observation.pending(), true, state_effect)
                        },
                        _ => serialize_result(result, state_effect),
                    }
                },
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
                let request = match resolved {
                    UnresolvedAutomationParams::Legacy(resolved) => resolved
                        .bind_to_target(observation.pending().target.clone())
                        .map_err(wire::SessionAutomationBindError::InvalidRequest),
                    UnresolvedAutomationParams::Session(resolved) => {
                        let Some(navigation) = state.authorizing_navigation.as_ref() else {
                            return missing_navigation_authority("session automation");
                        };
                        resolved.authorize_and_bind(observation.pending(), navigation, projection)
                    },
                };
                let request = match request {
                    Ok(request) => request,
                    Err(wire::SessionAutomationBindError::StaleStateToken) => {
                        return stale_state_token();
                    },
                    Err(wire::SessionAutomationBindError::Authority(error)) => {
                        return document_authority_authorization_failure(error);
                    },
                    Err(wire::SessionAutomationBindError::InvalidRequest(error)) => {
                        let _ = error;
                        return ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "internal_runtime_failure",
                                "validated automation data could not be bound to fresh target authority",
                                state_effect.as_protocol_str(),
                            ),
                            fail_stop: true,
                        });
                    },
                };
                ActiveTransition::Submit(DocumentControlCommand::Automate(Box::new(request)))
            } else {
                let (result, observation, synchronous_navigation_emitted) =
                    match completed_automation(outcome, &command, state_effect) {
                        Ok(completion) => completion,
                        Err(failure) => return ActiveTransition::Fail(failure),
                    };
                if state.profile.supports_session_api() && public_automation_is_mutating(state.kind)
                {
                    if state.completed.is_some() {
                        return ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "internal_runtime_failure",
                                "a completed automation result was consumed more than once",
                                state_effect.as_protocol_str(),
                            ),
                            fail_stop: true,
                        });
                    }
                    state.completed = Some(CompletedAutomation {
                        result,
                        pending: Box::new(observation.pending().clone()),
                        synchronous_navigation_emitted,
                    });
                    return ActiveTransition::SubmitSessionNavigationObservation {
                        // The action completion and this passive bracket are one ordered owner
                        // sequence. Do not pump unrelated Servo work between them.
                        allow_servo_pump: false,
                    };
                }
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
                match state.profile {
                    SessionProfile::ControlledWebappV1 => serialize_result(result, state_effect),
                    SessionProfile::ControlledWebSessionV1 |
                    SessionProfile::ControlledWebSessionV2 => {
                        project_session_value(result, observation.pending(), false, state_effect)
                    },
                }
            }
        },
        ActiveOperation::Audit(state) => {
            let observation = match completed_observation(outcome, &command, state_effect) {
                Ok(observation) => observation,
                Err(failure) => return ActiveTransition::Fail(failure),
            };
            let Some(value) = state.value.take() else {
                return ActiveTransition::Fail(ActiveFailure {
                    error: fatal_operation(
                        "internal_runtime_failure",
                        "session audit value was consumed more than once",
                        state_effect.as_protocol_str(),
                    ),
                    fail_stop: true,
                });
            };
            project_session_value(value, observation.pending(), false, state_effect)
        },
        ActiveOperation::Settle(state) => {
            if state.authority_bound_command.is_some() && command == DocumentControlCommand::Observe
            {
                let observation = match completed_observation(outcome, &command, state_effect) {
                    Ok(observation) => observation,
                    Err(failure) => {
                        return ActiveTransition::Fail(harden_continuation_failure(
                            failure,
                            state_effect,
                        ));
                    },
                };
                state.latest_pending_target = Some(Box::new(observation.pending().target.clone()));
                return ActiveTransition::SubmitSessionNavigationObservation {
                    allow_servo_pump: false,
                };
            }
            if matches!(
                state.replacement,
                Some(SettleReplacementPhase::Bootstrapping { .. })
            ) {
                let Some(SettleReplacementPhase::Bootstrapping {
                    source,
                    admitted,
                    command: expected,
                }) = state.replacement.take()
                else {
                    unreachable!("the settlement bootstrap phase was matched above")
                };
                if command != expected {
                    return ActiveTransition::Fail(ActiveFailure {
                        error: fatal_operation(
                            "internal_runtime_failure",
                            "settlement replacement bootstrap completed under a different command",
                            state_effect.as_protocol_str(),
                        ),
                        fail_stop: true,
                    });
                }
                state
                    .coordinator
                    .set_additional_foreground_external_io_active(
                        controlled_network_active_operations != 0,
                    );
                state.replacement = Some(SettleReplacementPhase::AwaitingActivation {
                    source,
                    admitted,
                    command,
                    control_outcome: outcome,
                    controlled_network_active: controlled_network_active_operations != 0,
                });
                return ActiveTransition::SubmitSessionNavigationObservation {
                    allow_servo_pump: false,
                };
            }
            if matches!(
                state.replacement,
                Some(SettleReplacementPhase::Activating { .. })
            ) {
                let Some(SettleReplacementPhase::Activating { source, admitted }) =
                    state.replacement.take()
                else {
                    unreachable!("the settlement activation phase was matched above")
                };
                state
                    .coordinator
                    .set_additional_foreground_external_io_active(
                        controlled_network_active_operations != 0,
                    );
                state.replacement = Some(SettleReplacementPhase::AwaitingActivation {
                    source,
                    admitted,
                    command,
                    control_outcome: outcome,
                    controlled_network_active: controlled_network_active_operations != 0,
                });
                return ActiveTransition::SubmitSessionNavigationObservation {
                    allow_servo_pump: false,
                };
            }
            if let Some(expected_authority) = state.authorizing_document_state.as_ref() {
                if state.authorizing_observation.is_some() {
                    return ActiveTransition::Fail(ActiveFailure {
                        error: fatal_operation(
                            "internal_runtime_failure",
                            "settlement token authorization consumed more than one document observation",
                            state_effect.as_protocol_str(),
                        ),
                        fail_stop: true,
                    });
                }
                if command != DocumentControlCommand::Observe {
                    return ActiveTransition::Fail(ActiveFailure {
                        error: fatal_operation(
                            "internal_runtime_failure",
                            "settlement token authorization did not use a fresh observation",
                            "none",
                        ),
                        fail_stop: true,
                    });
                }
                if let DocumentControlReceiveOutcome::CommandOutcome(
                    command_outcome @ DocumentControlOutcome::Rejected(
                        DocumentControlError::TargetChanged { .. } |
                        DocumentControlError::ReplacementPipelineBootstrapRequired { .. },
                    ),
                ) = &outcome &&
                    command_outcome.validate_for_command(&command).is_ok()
                {
                    return latch_and_reject_stale_settle_authority(
                        projection,
                        expected_authority,
                        state_effect,
                    );
                }
                let observation = match completed_observation(outcome, &command, state_effect) {
                    Ok(observation) => observation,
                    Err(failure) => return ActiveTransition::Fail(failure),
                };
                let Some(navigation) = state.authorizing_navigation.as_ref() else {
                    return missing_navigation_authority("runtime.settle");
                };
                if !expected_authority.matches_navigation(navigation) ||
                    observation.pending().target != *expected_authority.target()
                {
                    return latch_and_reject_stale_settle_authority(
                        projection,
                        expected_authority,
                        state_effect,
                    );
                }
                if observation.pending().state_generation < expected_authority.state_generation() {
                    return ActiveTransition::Fail(ActiveFailure {
                        error: fatal_operation(
                            "internal_runtime_failure",
                            "settlement token authority observed a regressed runtime generation",
                            state_effect.as_protocol_str(),
                        ),
                        fail_stop: true,
                    });
                }
                state.authorizing_observation = Some(observation);
                return ActiveTransition::SubmitSessionNavigationObservation {
                    allow_servo_pump: false,
                };
            }
            state
                .coordinator
                .set_additional_foreground_external_io_active(
                    controlled_network_active_operations != 0,
                );
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
                Err(error) => ActiveTransition::Fail(settle_failure_for_response(
                    error,
                    state_effect,
                    Some(&command),
                    &state.response,
                )),
            }
        },
        ActiveOperation::Navigate(state) => {
            if !matches!(
                state.phase,
                NavigatePhase::Authorizing { .. } | NavigatePhase::Settling { .. }
            ) {
                return ActiveTransition::Fail(ActiveFailure {
                    error: fatal_operation(
                        "internal_runtime_failure",
                        "session.navigate received a document-control result in the wrong phase",
                        state_effect.as_protocol_str(),
                    ),
                    fail_stop: true,
                });
            }
            if let NavigatePhase::Authorizing {
                expected_state_token,
                navigation,
            } = &state.phase
            {
                let observation = match completed_observation(outcome, &command, state_effect) {
                    Ok(observation) => observation,
                    Err(failure) => return ActiveTransition::Fail(failure),
                };
                match projection.authorizes_document_state(
                    observation.pending(),
                    navigation,
                    expected_state_token,
                ) {
                    Ok(true) => {},
                    Ok(false) => return stale_state_token(),
                    Err(error) => return document_authority_authorization_failure(error),
                }
                let expected = navigation.clone();
                let url = state.requested_url.clone();
                state.phase = NavigatePhase::AwaitingAdmission {
                    source: expected.clone(),
                    source_external_io_active_at_authorization:
                        controlled_network_blocks_document_replacement(
                            active_profile,
                            controlled_network_active_operations,
                        ),
                };
                return ActiveTransition::SubmitSessionNavigation { expected, url };
            }
            state
                .coordinator
                .set_additional_foreground_external_io_active(
                    controlled_network_active_operations != 0,
                );
            let progress = state
                .coordinator
                .consume_receive_outcome(outcome, state.cumulative_external_io_wall_time);
            match progress {
                Ok(progress) => {
                    transition_from_navigate_settle_progress(state, progress, state_effect)
                },
                Err(error) => {
                    ActiveTransition::Fail(settle_failure(error, state_effect, Some(&command)))
                },
            }
        },
        ActiveOperation::SessionProjection(state) => {
            let SessionProjectionPhase::AwaitingPendingObservation { navigation } = &state.phase
            else {
                return ActiveTransition::Fail(ActiveFailure {
                    error: fatal_operation(
                        "internal_runtime_failure",
                        "session projection received a document-control result in the wrong phase",
                        state_effect.as_protocol_str(),
                    ),
                    fail_stop: true,
                });
            };
            let navigation = navigation.clone();
            if command == DocumentControlCommand::Observe &&
                session_projection_allows_replacement_rearm(&state.kind) &&
                session_projection_settle_resume(&state.kind).is_some() &&
                let DocumentControlReceiveOutcome::CommandOutcome(
                    DocumentControlOutcome::Rejected(
                        DocumentControlError::ReplacementPipelineBootstrapRequired {
                            source_pipeline_id,
                            pipeline_id,
                        },
                    ),
                ) = &outcome
            {
                let source_shape_is_exact = state.pending.target == *navigation.target() &&
                    navigation
                        .target()
                        .active_top_level
                        .is_some_and(|active| active.pipeline_id == *source_pipeline_id) &&
                    navigation.target().pipelines() == [*source_pipeline_id] &&
                    navigation.target().fully_active_pipelines() == [*source_pipeline_id] &&
                    navigation.target().pending_top_level_pipelines().is_empty() &&
                    source_pipeline_id != pipeline_id;
                if !source_shape_is_exact {
                    return navigation_activation_failure(state_effect, true);
                }
                if let Some(resume) = session_projection_settle_resume_mut(&mut state.kind) {
                    resume.authorizing_navigation = Some(navigation.clone());
                }
                state.phase = SessionProjectionPhase::AwaitingReplacementAdmission {
                    source: navigation,
                    source_pipeline_id: *source_pipeline_id,
                    pipeline_id: *pipeline_id,
                };
                return ActiveTransition::SubmitSessionNavigationObservation {
                    allow_servo_pump: false,
                };
            }
            let observation = match completed_observation(outcome, &command, state_effect) {
                Ok(observation) => observation,
                Err(failure) => {
                    let failure = if session_projection_settle_resume(&state.kind).is_some() {
                        harden_continuation_failure(failure, state_effect)
                    } else {
                        failure
                    };
                    return ActiveTransition::Fail(failure);
                },
            };
            let pending = Box::new(observation.pending().clone());
            let raw_automation_projection =
                matches!(&state.kind, SessionProjectionKind::Automation { .. });
            let exact_action_refresh = raw_automation_projection &&
                session_projection_settle_resume(&state.kind)
                    .and_then(|resume| resume.authorizing_navigation.as_ref())
                    .is_some_and(|source| {
                        classify_same_document_session_transition(source, &navigation).is_some()
                    }) &&
                navigation.target() == &pending.target;
            if pending.as_ref() != state.pending.as_ref() && !exact_action_refresh {
                let restart = restart_session_projection_after_drift(&state.kind, state_effect);
                return match restart {
                    Ok((operation, transition)) => {
                        active.operation = operation;
                        transition
                    },
                    Err(failure) => ActiveTransition::Fail(failure),
                };
            }
            if navigation.target() != &pending.target {
                let restart = restart_session_projection_after_drift(&state.kind, state_effect);
                return match restart {
                    Ok((operation, transition)) => {
                        active.operation = operation;
                        transition
                    },
                    Err(failure) => ActiveTransition::Fail(failure),
                };
            }
            if let Some(resume) = session_projection_settle_resume_mut(&mut state.kind) {
                resume.authorizing_navigation = Some(navigation.clone());
            }
            state.pending = pending;
            state.phase = SessionProjectionPhase::AwaitingStableNavigation { navigation };
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false,
            }
        },
    }
}

fn controlled_network_blocks_virtual_advance(
    profile: Option<SessionProfile>,
    active_operations: usize,
) -> bool {
    profile.is_some_and(SessionProfile::supports_session_api) && active_operations != 0
}

fn controlled_network_blocks_document_replacement(
    profile: Option<SessionProfile>,
    active_operations: usize,
) -> bool {
    profile == Some(SessionProfile::ControlledWebSessionV2) && active_operations != 0
}

fn public_automation_is_mutating(kind: wire::PublicAutomationKind) -> bool {
    matches!(
        kind,
        wire::PublicAutomationKind::Activate |
            wire::PublicAutomationKind::Fill |
            wire::PublicAutomationKind::Focus |
            wire::PublicAutomationKind::Check |
            wire::PublicAutomationKind::Uncheck |
            wire::PublicAutomationKind::Select |
            wire::PublicAutomationKind::Submit
    )
}

fn default_resolved_settle_policy() -> wire::ResolvedSettlePolicy {
    wire::RuntimeSettleParams::default()
        .resolve(settle::SettlePolicy::default())
        .expect("the product default settlement policy is valid")
}

fn active_expects_navigation_response(operation: &ActiveOperation) -> bool {
    match operation {
        ActiveOperation::AdvanceToNext(AdvanceToNextState::Observing {
            expected_state_token: Some(_),
            navigation: None,
        }) => true,
        ActiveOperation::Settle(SettleState {
            authorizing_document_state: Some(_),
            authorizing_navigation: None,
            ..
        }) => true,
        ActiveOperation::Settle(SettleState {
            authorizing_document_state: Some(_),
            authorizing_observation: Some(_),
            ..
        }) => true,
        ActiveOperation::Settle(SettleState {
            replacement:
                Some(
                    SettleReplacementPhase::AwaitingAdmission { .. } |
                    SettleReplacementPhase::AwaitingActivation { .. },
                ),
            ..
        }) => true,
        ActiveOperation::Settle(SettleState {
            authority_bound_command: Some(_),
            ..
        }) => true,
        ActiveOperation::Automation(AutomationState {
            profile,
            unresolved: Some(_),
            authorizing_navigation: None,
            ..
        }) if profile.supports_session_api() => true,
        ActiveOperation::Automation(AutomationState {
            profile,
            completed: Some(_),
            ..
        }) if profile.supports_session_api() => true,
        ActiveOperation::Navigate(NavigateState {
            phase: NavigatePhase::AwaitingAuthority { .. } | NavigatePhase::AwaitingAdmission { .. },
            ..
        }) => true,
        ActiveOperation::SessionProjection(SessionProjectionState {
            phase:
                SessionProjectionPhase::AwaitingInitialNavigation |
                SessionProjectionPhase::AwaitingReplacementAdmission { .. } |
                SessionProjectionPhase::AwaitingStableNavigation { .. },
            ..
        }) => true,
        _ => false,
    }
}

fn transition_from_navigation_completion(
    active: &mut ActiveRequest,
    completion: NavigationOperationCompletion,
    projection: &mut wire::WireProjectionContext,
    controlled_network_active_operations: usize,
) -> ActiveTransition {
    let kind = completion.kind();
    if !completion.response_received() {
        return ActiveTransition::Fail(ActiveFailure {
            error: if kind == NavigationOperationKind::Navigate {
                fatal_operation(
                    "outcome_indeterminate",
                    "a written navigation lost its admission response",
                    "indeterminate",
                )
            } else {
                fatal_operation(
                    "navigation_transport_failure",
                    "the engine lost a passive session-navigation observation",
                    active.state_effect.as_protocol_str(),
                )
            },
            fail_stop: true,
        });
    }
    let navigation = match completion.into_outcome() {
        Ok(navigation) => navigation,
        Err(error @ SessionNavigationError::TargetChanged { .. })
            if kind == NavigationOperationKind::Observe =>
        {
            let expected_authority = match &active.operation {
                ActiveOperation::Settle(SettleState {
                    authorizing_document_state: Some(expected),
                    authorizing_navigation: None,
                    authorizing_observation: None,
                    ..
                }) |
                ActiveOperation::Settle(SettleState {
                    authorizing_document_state: Some(expected),
                    authorizing_navigation: Some(_),
                    authorizing_observation: Some(_),
                    ..
                }) => Some(expected),
                _ => None,
            };
            if let Some(expected_authority) = expected_authority {
                return latch_and_reject_stale_settle_authority(
                    projection,
                    expected_authority,
                    active.state_effect,
                );
            }
            return session_navigation_failure(error, kind, active.state_effect);
        },
        Err(error) => return session_navigation_failure(error, kind, active.state_effect),
    };
    if let Some(terminal) = navigation.terminal() {
        return session_navigation_terminal_failure(terminal, active.state_effect);
    }

    match &mut active.operation {
        ActiveOperation::AdvanceToNext(AdvanceToNextState::Observing {
            expected_state_token: Some(_),
            navigation: slot @ None,
        }) if kind == NavigationOperationKind::Observe => {
            *slot = Some(navigation);
            ActiveTransition::Submit(DocumentControlCommand::Observe)
        },
        ActiveOperation::Settle(state)
            if kind == NavigationOperationKind::Observe &&
                state.authority_bound_command.is_some() =>
        {
            let Some(expected_target) = state.latest_pending_target.as_ref() else {
                return ActiveTransition::Fail(ActiveFailure {
                    error: fatal_operation(
                        "internal_runtime_failure",
                        "a held settlement Drive lost its pending target",
                        active.state_effect.as_protocol_str(),
                    ),
                    fail_stop: true,
                });
            };
            let initial_open_bind = state.authorizing_navigation.is_none() &&
                matches!(state.response, SettleResponse::ControlledOpen { .. });
            if initial_open_bind {
                if navigation.terminal().is_some() ||
                    !matches!(navigation.url().scheme(), "http" | "https") ||
                    navigation.target().webview_id != expected_target.webview_id ||
                    navigation.target().event_loop_id != expected_target.event_loop_id
                {
                    return navigation_activation_failure(active.state_effect, true);
                }
                // Initial bootstrap only proves the root lifecycle event. Navigation-response
                // headers can advance its pending membership before the first owner observation;
                // bind the coordinator's initial Observe to that fresh owner target.
                state.latest_pending_target = Some(Box::new(navigation.target().clone()));
            } else if navigation.target() != expected_target.as_ref() {
                let queued_replacement_admission = state
                    .authorizing_navigation
                    .as_ref()
                    .filter(|source| {
                        state.replacement.is_none() && source.target() == expected_target.as_ref()
                    })
                    .and_then(|source| {
                        exact_replacement_admission(source, &navigation)
                            .map(|admission| (source.clone(), admission))
                    });
                if let Some((source, admission)) = queued_replacement_admission {
                    if state.authority_bound_command.as_ref() !=
                        Some(&DocumentControlCommand::DriveOneTurn)
                    {
                        return ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "internal_runtime_failure",
                                "replacement admission did not interrupt one unsubmitted settlement Drive",
                                active.state_effect.as_protocol_str(),
                            ),
                            fail_stop: true,
                        });
                    }
                    let bootstrap = DocumentControlCommand::BootstrapReplacementPipeline {
                        source_pipeline_id: admission.source_pipeline_id,
                        pipeline_id: admission.pipeline_id,
                    };
                    let progress = state
                        .coordinator
                        .replace_unsubmitted_drive_with_replacement_bootstrap(
                            source.target(),
                            bootstrap.clone(),
                        );
                    return match progress {
                        Ok(settle::SettleProgress::Command(command)) if command == bootstrap => {
                            // The coordinator selected the source Drive, but the shell had not
                            // submitted it. Replace that in-flight intent atomically with the
                            // independently owner-attested lifecycle bootstrap; never run or
                            // replay the stale source turn.
                            state.authority_bound_command = None;
                            state.replacement = Some(SettleReplacementPhase::Bootstrapping {
                                source,
                                admitted: navigation,
                                command: command.clone(),
                            });
                            active.state_effect = RequestStateEffect::Partial;
                            ActiveTransition::Submit(command)
                        },
                        Ok(_) => ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "internal_runtime_failure",
                                "replacement rearm did not issue its exact bootstrap command",
                                active.state_effect.as_protocol_str(),
                            ),
                            fail_stop: true,
                        }),
                        Err(error) => ActiveTransition::Fail(settle_failure_for_response(
                            error,
                            active.state_effect,
                            Some(&DocumentControlCommand::DriveOneTurn),
                            &state.response,
                        )),
                    };
                }
                let exact_replacement_progress = match state.replacement.as_ref() {
                    Some(SettleReplacementPhase::Activating { source, admitted }) => {
                        held_drive_replacement_target_progressed(
                            source,
                            admitted,
                            expected_target,
                            &navigation,
                        )
                    },
                    _ => false,
                };
                if !exact_replacement_progress {
                    return navigation_activation_failure(active.state_effect, true);
                }
                // The held Drive was bound to a pending snapshot immediately before the same
                // admitted replacement advanced. Refresh document authority and bracket it once
                // more; the coordinator command remains retained and is never replayed.
                state.latest_pending_target = Some(Box::new(navigation.target().clone()));
                return ActiveTransition::Submit(DocumentControlCommand::Observe);
            }
            let mut replacement_became_ready = false;
            if let Some(SettleReplacementPhase::Activating { source, admitted }) =
                state.replacement.as_ref()
            {
                match classify_replacement_activation_observation(source, admitted, &navigation) {
                    ReplacementActivationObservation::ControlledReady => {
                        state.replacement = None;
                        replacement_became_ready = true;
                    },
                    ReplacementActivationObservation::Pending |
                    ReplacementActivationObservation::ActivatedAwaitingSourceExit => {},
                    ReplacementActivationObservation::Invalid => {
                        return navigation_activation_failure(active.state_effect, true);
                    },
                }
            }
            if !replacement_became_ready &&
                state.replacement.is_none() &&
                state.authorizing_navigation.as_ref().is_some_and(|source| {
                    session_navigation_reached_controlled_ready(source) &&
                        classify_same_document_session_transition(source, &navigation).is_none()
                })
            {
                return navigation_activation_failure(active.state_effect, true);
            }
            let Some(command) = state.authority_bound_command.take() else {
                unreachable!("the pending Drive phase was matched above")
            };
            state.authorizing_navigation = Some(navigation);
            ActiveTransition::Submit(command)
        },
        ActiveOperation::Settle(state)
            if kind == NavigationOperationKind::Observe &&
                matches!(
                    state.replacement.as_ref(),
                    Some(SettleReplacementPhase::AwaitingAdmission { .. })
                ) =>
        {
            let Some(SettleReplacementPhase::AwaitingAdmission {
                source,
                drive_outcome,
            }) = state.replacement.take()
            else {
                unreachable!("the replacement admission phase was matched above")
            };
            if classify_same_document_session_transition(&source, &navigation).is_some() {
                let progress = state
                    .coordinator
                    .consume_drive_one_turn_stable_authority_boundary(
                        drive_outcome,
                        state.cumulative_external_io_wall_time,
                    );
                return match progress {
                    Ok(settle::SettleProgress::Command(DocumentControlCommand::Observe)) => {
                        state.latest_pending_target = Some(Box::new(navigation.target().clone()));
                        state.authorizing_navigation = Some(navigation);
                        ActiveTransition::Submit(DocumentControlCommand::Observe)
                    },
                    Ok(_) => ActiveTransition::Fail(ActiveFailure {
                        error: fatal_operation(
                            "internal_runtime_failure",
                            "stable authority boundary did not issue an Observe command",
                            active.state_effect.as_protocol_str(),
                        ),
                        fail_stop: true,
                    }),
                    Err(error) => ActiveTransition::Fail(settle_failure_for_response(
                        error,
                        active.state_effect,
                        None,
                        &state.response,
                    )),
                };
            }
            let Some(admission) = exact_replacement_admission(&source, &navigation) else {
                return navigation_activation_failure(active.state_effect, true);
            };
            let bootstrap = DocumentControlCommand::BootstrapReplacementPipeline {
                source_pipeline_id: admission.source_pipeline_id,
                pipeline_id: admission.pipeline_id,
            };
            let progress = state
                .coordinator
                .consume_drive_one_turn_replacement_boundary(
                    drive_outcome,
                    state.cumulative_external_io_wall_time,
                    bootstrap.clone(),
                );
            match progress {
                Ok(settle::SettleProgress::Command(command)) if command == bootstrap => {
                    state.replacement = Some(SettleReplacementPhase::Bootstrapping {
                        source,
                        admitted: navigation,
                        command: command.clone(),
                    });
                    ActiveTransition::Submit(command)
                },
                Ok(_) => ActiveTransition::Fail(ActiveFailure {
                    error: fatal_operation(
                        "internal_runtime_failure",
                        "replacement boundary did not issue its exact bootstrap command",
                        active.state_effect.as_protocol_str(),
                    ),
                    fail_stop: true,
                }),
                Err(error) => ActiveTransition::Fail(settle_failure_for_response(
                    error,
                    active.state_effect,
                    None,
                    &state.response,
                )),
            }
        },
        ActiveOperation::Settle(state)
            if kind == NavigationOperationKind::Observe &&
                matches!(
                    state.replacement.as_ref(),
                    Some(SettleReplacementPhase::AwaitingActivation { .. })
                ) =>
        {
            let Some(SettleReplacementPhase::AwaitingActivation {
                source,
                admitted,
                command,
                control_outcome,
                controlled_network_active,
            }) = state.replacement.take()
            else {
                unreachable!("the replacement activation phase was matched above")
            };
            let observation =
                classify_replacement_activation_observation(&source, &admitted, &navigation);
            match observation {
                ReplacementActivationObservation::ControlledReady => {
                    let document_target_matches = receive_outcome_pending_target(&control_outcome)
                        .is_some_and(|target| target == navigation.target());
                    if document_target_matches {
                        state.latest_pending_target = Some(Box::new(navigation.target().clone()));
                        state.authorizing_navigation = Some(navigation);
                        state
                            .coordinator
                            .set_additional_foreground_external_io_active(
                                controlled_network_active,
                            );
                        // `None` is the armed state: a later Drive boundary must bind to the newly
                        // activated document authority and may cross another independently verified
                        // replacement.
                        state.replacement = None;
                    } else {
                        // Constellation is ready but the held document outcome still describes an
                        // earlier admitted/retained membership. Keep the exact replacement phase
                        // until the coordinator's next command and passive bracket agree on ready.
                        state
                            .coordinator
                            .set_additional_foreground_external_io_active(true);
                        state.replacement =
                            Some(SettleReplacementPhase::Activating { source, admitted });
                    }
                },
                ReplacementActivationObservation::Pending |
                ReplacementActivationObservation::ActivatedAwaitingSourceExit => {
                    // An incomplete replacement is settlement foreground work. Consume the exact
                    // held outcome through the existing coordinator so all subsequent drives,
                    // waits, wall-I/O expiry, and control-turn limits remain on one ledger.
                    state
                        .coordinator
                        .set_additional_foreground_external_io_active(true);
                    state.replacement =
                        Some(SettleReplacementPhase::Activating { source, admitted });
                },
                ReplacementActivationObservation::Invalid => {
                    return navigation_activation_failure(active.state_effect, true);
                },
            }
            match state
                .coordinator
                .consume_receive_outcome(control_outcome, state.cumulative_external_io_wall_time)
            {
                Ok(progress) => transition_from_settle_progress_for_active(
                    state,
                    active.started_at,
                    progress,
                    active.state_effect,
                    projection,
                ),
                Err(error) => ActiveTransition::Fail(settle_failure_for_response(
                    error,
                    active.state_effect,
                    Some(&command),
                    &state.response,
                )),
            }
        },
        ActiveOperation::Settle(state)
            if kind == NavigationOperationKind::Observe &&
                state.authorizing_document_state.is_some() &&
                state.authorizing_observation.is_some() &&
                state.authorizing_navigation.is_some() =>
        {
            let expected_authority = state
                .authorizing_document_state
                .take()
                .expect("the settlement N2 phase retained document authority");
            let observation = state
                .authorizing_observation
                .take()
                .expect("the settlement N2 phase retained its document observation");
            let n1 = state
                .authorizing_navigation
                .as_ref()
                .expect("the settlement N2 phase retained N1");
            if &navigation != n1 ||
                !expected_authority.matches_navigation(&navigation) ||
                observation.pending().target != *expected_authority.target() ||
                navigation.target() != &observation.pending().target
            {
                return latch_and_reject_stale_settle_authority(
                    projection,
                    &expected_authority,
                    active.state_effect,
                );
            }
            if observation.pending().state_generation < expected_authority.state_generation() {
                return ActiveTransition::Fail(ActiveFailure {
                    error: fatal_operation(
                        "internal_runtime_failure",
                        "settlement token authority observed a regressed runtime generation",
                        active.state_effect.as_protocol_str(),
                    ),
                    fail_stop: true,
                });
            }
            state.latest_pending_target = Some(Box::new(observation.pending().target.clone()));
            state.authorizing_navigation = Some(navigation);
            state
                .coordinator
                .set_additional_foreground_external_io_active(
                    controlled_network_active_operations != 0,
                );
            let seeded_outcome = DocumentControlReceiveOutcome::CommandOutcome(
                DocumentControlOutcome::Completed(observation),
            );
            match state
                .coordinator
                .start_with_observe_outcome(seeded_outcome, state.cumulative_external_io_wall_time)
            {
                Ok(progress) => transition_from_settle_progress_for_active(
                    state,
                    active.started_at,
                    progress,
                    active.state_effect,
                    projection,
                ),
                Err(error) => ActiveTransition::Fail(settle_failure_for_response(
                    error,
                    active.state_effect,
                    Some(&DocumentControlCommand::Observe),
                    &state.response,
                )),
            }
        },
        ActiveOperation::Settle(SettleState {
            authorizing_document_state: Some(expected),
            authorizing_navigation: slot @ None,
            ..
        }) if kind == NavigationOperationKind::Observe => {
            if !expected.matches_navigation(&navigation) {
                return latch_and_reject_stale_settle_authority(
                    projection,
                    expected,
                    active.state_effect,
                );
            }
            *slot = Some(navigation);
            ActiveTransition::Submit(DocumentControlCommand::Observe)
        },
        ActiveOperation::Automation(AutomationState {
            profile,
            unresolved: Some(_),
            authorizing_navigation: slot @ None,
            ..
        }) if profile.supports_session_api() && kind == NavigationOperationKind::Observe => {
            *slot = Some(navigation);
            ActiveTransition::Submit(DocumentControlCommand::Observe)
        },
        ActiveOperation::Automation(state)
            if state.profile.supports_session_api() &&
                state.completed.is_some() &&
                kind == NavigationOperationKind::Observe =>
        {
            let profile = state.profile;
            let Some(source) = state.authorizing_navigation.clone() else {
                return missing_navigation_authority("completed session automation");
            };
            let Some(completed) = state.completed.take() else {
                unreachable!("the completed automation phase was matched above")
            };
            let action_kind = state.kind;
            if completed.pending.target != *source.target() {
                return navigation_activation_failure(active.state_effect, true);
            }
            let synchronous_navigation_emitted = completed.synchronous_navigation_emitted;
            let response = SettleResponse::Automation {
                kind: action_kind,
                result: completed.result,
            };
            if synchronous_navigation_emitted &&
                let Some(admission) = exact_replacement_admission(&source, &navigation)
            {
                let bootstrap = DocumentControlCommand::BootstrapReplacementPipeline {
                    source_pipeline_id: admission.source_pipeline_id,
                    pipeline_id: admission.pipeline_id,
                };
                let effective_policy = default_resolved_settle_policy();
                let mut coordinator = settle::SettleCoordinator::new(effective_policy.engine);
                let progress = coordinator.start_with_replacement_bootstrap(bootstrap.clone());
                let command = match progress {
                    Ok(settle::SettleProgress::Command(command)) if command == bootstrap => command,
                    Ok(_) => {
                        return ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "internal_runtime_failure",
                                "automation replacement did not issue its exact bootstrap command",
                                active.state_effect.as_protocol_str(),
                            ),
                            fail_stop: true,
                        });
                    },
                    Err(error) => {
                        return ActiveTransition::Fail(harden_continuation_failure(
                            settle_failure(error, active.state_effect, None),
                            active.state_effect,
                        ));
                    },
                };
                active.operation = ActiveOperation::Settle(SettleState {
                    profile,
                    authorizing_document_state: None,
                    authorizing_observation: None,
                    authorizing_navigation: Some(source.clone()),
                    replacement: Some(SettleReplacementPhase::Bootstrapping {
                        source,
                        admitted: navigation,
                        command: command.clone(),
                    }),
                    authority_bound_command: None,
                    latest_pending_target: Some(Box::new(completed.pending.target.clone())),
                    response,
                    coordinator,
                    effective_policy,
                    cumulative_external_io_wall_time: Duration::ZERO,
                    waiting: None,
                });
                ActiveTransition::Submit(command)
            } else if classify_same_document_session_transition(&source, &navigation).is_some_and(
                |transition| {
                    synchronous_navigation_emitted ||
                        transition == SameDocumentSessionTransition::Unchanged
                },
            ) {
                active.operation = ActiveOperation::SessionProjection(SessionProjectionState {
                    // This pending snapshot proves the action completed on `source`, but is not
                    // projected. The following document Observe supplies the fresh generation
                    // bracketed by N1 and N2.
                    pending: completed.pending,
                    kind: SessionProjectionKind::Automation {
                        settle_resume: SettleProjectionResume {
                            profile,
                            effective_policy: default_resolved_settle_policy(),
                            cumulative_external_io_wall_time: Duration::ZERO,
                            authorizing_navigation: Some(navigation.clone()),
                            response,
                        },
                        replacement_rearm: synchronous_navigation_emitted,
                    },
                    phase: SessionProjectionPhase::AwaitingPendingObservation { navigation },
                });
                ActiveTransition::Submit(DocumentControlCommand::Observe)
            } else {
                navigation_activation_failure(active.state_effect, true)
            }
        },
        ActiveOperation::Navigate(state) => match &mut state.phase {
            NavigatePhase::AwaitingAuthority {
                expected_state_token,
            } if kind == NavigationOperationKind::Observe => {
                state.phase = NavigatePhase::Authorizing {
                    expected_state_token: expected_state_token.clone(),
                    navigation,
                };
                ActiveTransition::Submit(DocumentControlCommand::Observe)
            },
            NavigatePhase::AwaitingAdmission {
                source,
                source_external_io_active_at_authorization,
            }
                if kind == NavigationOperationKind::Navigate =>
            {
                let profile = active
                    .profile
                    .expect("session navigation retains its selected session profile");
                active.state_effect = RequestStateEffect::Partial;
                let Some(admission) = exact_replacement_admission(source, &navigation) else {
                    return navigation_activation_failure(active.state_effect, true);
                };
                let requested_url = state.requested_url.clone();
                let source = source.clone();
                let admitted = navigation;
                let bootstrap = DocumentControlCommand::BootstrapReplacementPipeline {
                    source_pipeline_id: admission.source_pipeline_id,
                    pipeline_id: admission.pipeline_id,
                };
                let effective_policy = default_resolved_settle_policy();
                let mut coordinator = settle::SettleCoordinator::new(effective_policy.engine);
                coordinator.latch_additional_foreground_external_io_active(
                    *source_external_io_active_at_authorization,
                );
                let command = match coordinator.start_with_replacement_bootstrap(bootstrap.clone())
                {
                    Ok(settle::SettleProgress::Command(command)) if command == bootstrap => command,
                    Ok(_) => {
                        return ActiveTransition::Fail(ActiveFailure {
                            error: fatal_operation(
                                "internal_runtime_failure",
                                "explicit navigation did not issue its exact bootstrap command",
                                active.state_effect.as_protocol_str(),
                            ),
                            fail_stop: true,
                        });
                    },
                    Err(error) => {
                        return ActiveTransition::Fail(harden_continuation_failure(
                            settle_failure(error, active.state_effect, None),
                            active.state_effect,
                        ));
                    },
                };
                active.operation = ActiveOperation::Settle(SettleState {
                    profile,
                    authorizing_document_state: None,
                    authorizing_observation: None,
                    authorizing_navigation: Some(source.clone()),
                    replacement: Some(SettleReplacementPhase::Bootstrapping {
                        source: source.clone(),
                        admitted: admitted.clone(),
                        command: command.clone(),
                    }),
                    authority_bound_command: None,
                    latest_pending_target: Some(Box::new(admitted.target().clone())),
                    response: SettleResponse::Navigate {
                        requested_url,
                        source,
                        admitted,
                    },
                    coordinator,
                    effective_policy,
                    cumulative_external_io_wall_time: Duration::ZERO,
                    waiting: None,
                });
                ActiveTransition::Submit(command)
            },
            _ => unexpected_navigation_completion(active.state_effect),
        },
        ActiveOperation::SessionProjection(state) if kind == NavigationOperationKind::Observe => {
            let resume = session_projection_settle_resume(&state.kind).cloned();
            let replacement_source = match &state.phase {
                SessionProjectionPhase::AwaitingInitialNavigation => resume
                    .as_ref()
                    .and_then(|resume| resume.authorizing_navigation.clone()),
                SessionProjectionPhase::AwaitingStableNavigation { navigation } => {
                    Some(navigation.clone())
                },
                SessionProjectionPhase::AwaitingPendingObservation { .. } |
                SessionProjectionPhase::AwaitingReplacementAdmission { .. } => None,
            };
            if let SessionProjectionPhase::AwaitingReplacementAdmission {
                source,
                source_pipeline_id,
                pipeline_id,
            } = &state.phase
            {
                let Some(admission) = exact_replacement_admission(source, &navigation) else {
                    return navigation_activation_failure(active.state_effect, true);
                };
                if admission.source_pipeline_id != *source_pipeline_id ||
                    admission.pipeline_id != *pipeline_id
                {
                    return navigation_activation_failure(active.state_effect, true);
                }
                let Some(resume) = resume.as_ref() else {
                    return projection_shape_failure(
                        "replacement admission lost its projection continuation",
                        active.state_effect,
                    );
                };
                let restart = resume_session_projection_at_replacement(
                    resume,
                    source.clone(),
                    navigation,
                    Box::new(state.pending.target.clone()),
                    active.state_effect,
                );
                return match restart {
                    Ok((operation, transition)) => {
                        active.operation = operation;
                        transition
                    },
                    Err(failure) => ActiveTransition::Fail(failure),
                };
            }
            if let (Some(resume), Some(source)) = (resume.as_ref(), replacement_source) &&
                session_projection_allows_replacement_rearm(&state.kind) &&
                exact_replacement_admission(&source, &navigation).is_some()
            {
                let restart = resume_session_projection_at_replacement(
                    resume,
                    source,
                    navigation,
                    Box::new(state.pending.target.clone()),
                    active.state_effect,
                );
                return match restart {
                    Ok((operation, transition)) => {
                        active.operation = operation;
                        transition
                    },
                    Err(failure) => ActiveTransition::Fail(failure),
                };
            }
            match &state.phase {
                SessionProjectionPhase::AwaitingInitialNavigation => {
                    state.phase = SessionProjectionPhase::AwaitingPendingObservation { navigation };
                    return ActiveTransition::Submit(DocumentControlCommand::Observe);
                },
                SessionProjectionPhase::AwaitingPendingObservation { .. } => {
                    return unexpected_navigation_completion(active.state_effect);
                },
                SessionProjectionPhase::AwaitingReplacementAdmission { .. } => {
                    unreachable!("replacement admission was handled before projection bracketing")
                },
                SessionProjectionPhase::AwaitingStableNavigation { navigation: before }
                    if before != &navigation =>
                {
                    if session_projection_allows_replacement_rearm(&state.kind) &&
                        classify_same_document_session_transition(before, &navigation).is_some()
                    {
                        if let Some(resume) = session_projection_settle_resume_mut(&mut state.kind)
                        {
                            resume.authorizing_navigation = Some(navigation.clone());
                        }
                        // A bounded group of same-document messages can land between N1 and N2.
                        // Refresh the pending observation and bracket again without driving any
                        // ordinary page work or converting an ordinary action into settlement.
                        state.phase =
                            SessionProjectionPhase::AwaitingPendingObservation { navigation };
                        return ActiveTransition::Submit(DocumentControlCommand::Observe);
                    }
                    let restart =
                        restart_session_projection_after_drift(&state.kind, active.state_effect);
                    return match restart {
                        Ok((operation, transition)) => {
                            active.operation = operation;
                            transition
                        },
                        Err(failure) => ActiveTransition::Fail(failure),
                    };
                },
                SessionProjectionPhase::AwaitingStableNavigation { .. } => {},
            }
            let token = match projection.document_state_token(&state.pending, &navigation) {
                Ok(token) => token,
                Err(error) => return mismatched_navigation_authority(error, active.state_effect),
            };
            match &mut state.kind {
                SessionProjectionKind::Automation { settle_resume, .. } => {
                    let SettleResponse::Automation { kind, result } = &settle_resume.response
                    else {
                        return projection_shape_failure(
                            "an automation projection lost its original action-shaped result",
                            active.state_effect,
                        );
                    };
                    let public = match wire::PublicAutomationResult::project(
                        *kind,
                        result.clone(),
                        state.pending.as_ref(),
                    ) {
                        Ok(result) => result,
                        Err(error) => {
                            return ActiveTransition::Fail(ActiveFailure {
                                error: fatal_operation(
                                    "internal_runtime_failure",
                                    format!("failed to project automation result: {error:?}"),
                                    active.state_effect.as_protocol_str(),
                                ),
                                fail_stop: true,
                            });
                        },
                    };
                    let mut value = match serde_json::to_value(public) {
                        Ok(value) => value,
                        Err(error) => {
                            return ActiveTransition::Fail(ActiveFailure {
                                error: fatal_operation(
                                    "internal_runtime_failure",
                                    format!("failed to serialize automation result: {error}"),
                                    active.state_effect.as_protocol_str(),
                                ),
                                fail_stop: true,
                            });
                        },
                    };
                    let Some(object) = value.as_object_mut() else {
                        return projection_shape_failure(
                            "automation result is not an object",
                            active.state_effect,
                        );
                    };
                    object.insert(
                        "stateToken".into(),
                        serde_json::to_value(&token).expect("opaque token serializes"),
                    );
                    ActiveTransition::Complete(value)
                },
                SessionProjectionKind::Value {
                    value,
                    snapshot_token,
                    ..
                } => {
                    let Some(object) = value.as_object_mut() else {
                        return projection_shape_failure(
                            "session result is not an object",
                            active.state_effect,
                        );
                    };
                    object.insert(
                        "stateToken".into(),
                        serde_json::to_value(&token).expect("opaque token serializes"),
                    );
                    if *snapshot_token {
                        let Some(snapshot) =
                            object.get_mut("snapshot").and_then(Value::as_object_mut)
                        else {
                            return projection_shape_failure(
                                "session result snapshot is not an object",
                                active.state_effect,
                            );
                        };
                        snapshot.insert(
                            "stateToken".into(),
                            serde_json::to_value(&token).expect("opaque token serializes"),
                        );
                    }
                    ActiveTransition::Complete(std::mem::take(value))
                },
                SessionProjectionKind::Navigate {
                    requested_url,
                    source,
                    admitted,
                    cumulative_external_io_wall_time: _,
                    settle_resume,
                } => {
                    let controlled_ready = if settle_resume.is_some() {
                        explicit_navigation_chain_reached_controlled_ready(source, &navigation)
                    } else {
                        explicit_navigation_reached_controlled_ready(source, admitted, &navigation)
                    };
                    if !controlled_ready {
                        return navigation_activation_failure(active.state_effect, true);
                    }
                    match wire::SessionNavigateResult::project(
                        requested_url.to_string(),
                        &state.pending,
                        &navigation,
                        projection,
                    ) {
                        Ok(result) => serialize_result(result, active.state_effect),
                        Err(error) => mismatched_navigation_authority(error, active.state_effect),
                    }
                },
                SessionProjectionKind::ControlledOpen {
                    requested_url,
                    current_url,
                    profile,
                    deadline: _,
                    bootstrap_attempted: _,
                    cumulative_external_io_wall_time: _,
                    session_state_token,
                    settle_resume,
                } => {
                    let _ = current_url;
                    let controlled_ready = if settle_resume.is_some() {
                        session_navigation_reached_controlled_ready(&navigation)
                    } else {
                        initial_navigation_reached_controlled_ready(&navigation)
                    };
                    if !controlled_ready {
                        return navigation_activation_failure(active.state_effect, true);
                    }
                    let Some(session_state_token) = session_state_token.take() else {
                        return projection_shape_failure(
                            "controlled open did not refresh session-state authority",
                            active.state_effect,
                        );
                    };
                    ActiveTransition::Complete(json!({
                        "sessionId": SESSION_ID,
                        "requestedUrl": requested_url,
                        "url": navigation.url().to_string(),
                        "boundary": "controlled_ready",
                        "clockMode": "controlled",
                        "profile": profile.id(),
                        "stateToken": token,
                        "sessionStateToken": session_state_token,
                    }))
                },
            }
        },
        _ => unexpected_navigation_completion(active.state_effect),
    }
}

fn terminal_navigation_evidence(
    terminal: SessionNavigationTerminal,
    fallback: SessionNavigationId,
) -> (SessionNavigationId, NetworkFailureReason) {
    match terminal {
        SessionNavigationTerminal::DocumentTransitionLimitExceeded {
            next_navigation_id, ..
        } => (
            next_navigation_id,
            NetworkFailureReason::DocumentTransitionLimitExceeded,
        ),
        SessionNavigationTerminal::HistoryLimitExceeded { navigation_id, .. } => {
            (navigation_id, NetworkFailureReason::HistoryLimitExceeded)
        },
        SessionNavigationTerminal::RedirectLimitExceeded { navigation_id, .. } => {
            (navigation_id, NetworkFailureReason::RedirectLimitExceeded)
        },
        SessionNavigationTerminal::CounterOverflow { .. } => {
            (fallback, NetworkFailureReason::NavigationError)
        },
    }
}

fn session_navigation_failure(
    error: SessionNavigationError,
    kind: NavigationOperationKind,
    state_effect: RequestStateEffect,
) -> ActiveTransition {
    match error {
        SessionNavigationError::TargetChanged { .. }
            if state_effect == RequestStateEffect::Partial =>
        {
            navigation_activation_failure(state_effect, true)
        },
        SessionNavigationError::TargetChanged { .. } => stale_state_token(),
        SessionNavigationError::NavigationInProgress => continuation_navigation_rejection(
            "navigation_in_progress",
            "another top-level navigation is already pending",
            state_effect,
        ),
        SessionNavigationError::SourceInactive => continuation_navigation_rejection(
            "navigation_source_inactive",
            "the current top-level source is no longer active",
            state_effect,
        ),
        SessionNavigationError::NavigationStartFailed { observed } => {
            ActiveTransition::Fail(ActiveFailure {
                error: with_error_details(
                    fatal_operation(
                        "navigation_start_failed",
                        "navigation identity was reserved but the fetch pipeline did not start",
                        "partial",
                    ),
                    json!({
                        "navigationId": observed.navigation_id().get().to_string(),
                    }),
                ),
                fail_stop: true,
            })
        },
        SessionNavigationError::UnsupportedScheme { scheme } => {
            let fail_stop = state_effect == RequestStateEffect::Partial;
            let mut error = ProtocolError::operation(
                "unsupported_navigation_scheme",
                "session navigation supports only HTTP(S) URLs",
                state_effect.as_protocol_str(),
            );
            let _ = scheme;
            error.fatal = fail_stop;
            ActiveTransition::Fail(ActiveFailure { error, fail_stop })
        },
        SessionNavigationError::Terminal(terminal) => {
            session_navigation_terminal_failure(terminal, state_effect)
        },
        SessionNavigationError::ChannelClosed => ActiveTransition::Fail(ActiveFailure {
            error: fatal_operation(
                "navigation_transport_failure",
                "the engine session-navigation channel closed",
                if kind == NavigationOperationKind::Navigate {
                    "indeterminate"
                } else {
                    state_effect.as_protocol_str()
                },
            ),
            fail_stop: true,
        }),
        SessionNavigationError::NotTopLevelSession |
        SessionNavigationError::TargetUnavailable(_) => ActiveTransition::Fail(ActiveFailure {
            error: fatal_operation(
                "session_navigation_authority_unavailable",
                format!("engine could not capture controlled-session authority: {error:?}"),
                state_effect.as_protocol_str(),
            ),
            fail_stop: true,
        }),
    }
}

fn continuation_navigation_rejection(
    code: &'static str,
    message: &'static str,
    state_effect: RequestStateEffect,
) -> ActiveTransition {
    let fail_stop = state_effect == RequestStateEffect::Partial;
    let mut error = ProtocolError::operation(code, message, state_effect.as_protocol_str());
    error.fatal = fail_stop;
    ActiveTransition::Fail(ActiveFailure { error, fail_stop })
}

fn session_navigation_terminal_failure(
    terminal: SessionNavigationTerminal,
    state_effect: RequestStateEffect,
) -> ActiveTransition {
    let completed_other_work = state_effect == RequestStateEffect::Partial;
    let (error, fail_stop) = match terminal {
        SessionNavigationTerminal::DocumentTransitionLimitExceeded {
            limit,
            observed,
            next_navigation_id,
        } => (
            with_error_details(
                ProtocolError::operation(
                    "document_transition_limit_exceeded",
                    "document transition limit was exceeded",
                    state_effect.as_protocol_str(),
                ),
                json!({
                    "limit": limit.to_string(),
                    "observed": observed.to_string(),
                    "nextNavigationId": next_navigation_id.get().to_string(),
                }),
            ),
            completed_other_work,
        ),
        SessionNavigationTerminal::HistoryLimitExceeded {
            limit,
            observed,
            navigation_id,
            history_revision,
        } => (
            with_error_details(
                ProtocolError::operation(
                    "history_limit_exceeded",
                    "same-document history limit was exceeded",
                    state_effect.as_protocol_str(),
                ),
                json!({
                    "limit": limit.to_string(),
                    "observed": observed.to_string(),
                    "navigationId": navigation_id.get().to_string(),
                    "historyRevision": history_revision.get().to_string(),
                }),
            ),
            completed_other_work,
        ),
        SessionNavigationTerminal::RedirectLimitExceeded {
            limit,
            observed,
            navigation_id,
        } => (
            with_error_details(
                fatal_operation(
                    "redirect_limit_exceeded",
                    "redirect limit was exceeded after network work began",
                    "partial",
                ),
                json!({
                    "limit": limit.to_string(),
                    "observed": observed.to_string(),
                    "navigationId": navigation_id.get().to_string(),
                }),
            ),
            true,
        ),
        SessionNavigationTerminal::CounterOverflow { counter } => (
            fatal_operation(
                "runtime_error",
                format!(
                    "controlled-session {} counter overflowed",
                    match counter {
                        SessionNavigationCounter::DocumentEpoch => "document epoch",
                        SessionNavigationCounter::NavigationId => "navigation id",
                        SessionNavigationCounter::HistoryRevision => "history revision",
                        SessionNavigationCounter::SuccessfulDocumentReplacements => {
                            "successful document replacement"
                        },
                    }
                ),
                state_effect.as_protocol_str(),
            ),
            true,
        ),
    };
    let mut error = error;
    error.fatal |= fail_stop;
    ActiveTransition::Fail(ActiveFailure { error, fail_stop })
}

fn unexpected_navigation_completion(state_effect: RequestStateEffect) -> ActiveTransition {
    ActiveTransition::Fail(ActiveFailure {
        error: fatal_operation(
            "internal_runtime_failure",
            "session-navigation completion did not match the active request phase",
            state_effect.as_protocol_str(),
        ),
        fail_stop: true,
    })
}

fn projection_shape_failure(
    message: &'static str,
    state_effect: RequestStateEffect,
) -> ActiveTransition {
    ActiveTransition::Fail(ActiveFailure {
        error: fatal_operation(
            "internal_runtime_failure",
            message,
            state_effect.as_protocol_str(),
        ),
        fail_stop: true,
    })
}

fn stale_state_token() -> ActiveTransition {
    ActiveTransition::Fail(ActiveFailure {
        error: stale_state_token_error(),
        fail_stop: false,
    })
}

fn stale_state_token_error() -> ProtocolError {
    ProtocolError::operation(
        "stale_state_token",
        "expectedStateToken does not authorize the current document state",
        "none",
    )
}

fn latch_and_reject_stale_settle_authority(
    projection: &mut wire::WireProjectionContext,
    expected: &wire::CurrentDocumentStateAuthority,
    state_effect: RequestStateEffect,
) -> ActiveTransition {
    if projection.latch_current_document_state_strictly_invalidated(expected) {
        return ActiveTransition::RejectStaleStateToken;
    }
    ActiveTransition::Fail(ActiveFailure {
        error: fatal_operation(
            "internal_runtime_failure",
            "settlement document authority changed while latching a stale continuation",
            state_effect.as_protocol_str(),
        ),
        fail_stop: true,
    })
}

fn missing_navigation_authority(operation: &'static str) -> ActiveTransition {
    ActiveTransition::Fail(ActiveFailure {
        error: fatal_operation(
            "internal_runtime_failure",
            format!("{operation} did not retain checked session-navigation authority"),
            "none",
        ),
        fail_stop: true,
    })
}

fn mismatched_navigation_authority(
    error: wire::DocumentStateAuthorityError,
    state_effect: RequestStateEffect,
) -> ActiveTransition {
    match error {
        wire::DocumentStateAuthorityError::NavigationTargetDoesNotMatchPending => {
            ActiveTransition::Fail(ActiveFailure {
                error: if state_effect == RequestStateEffect::Partial {
                    fatal_operation(
                        "document_authority_changed",
                        "document authority changed after request work was admitted",
                        "partial",
                    )
                } else {
                    ProtocolError::operation(
                        "document_authority_changed",
                        "document authority changed while projecting the result",
                        "none",
                    )
                },
                fail_stop: state_effect == RequestStateEffect::Partial,
            })
        },
        wire::DocumentStateAuthorityError::TokenSpaceExhausted => {
            document_authority_token_space_exhausted(state_effect)
        },
        wire::DocumentStateAuthorityError::TokenEntropyUnavailable => {
            document_authority_token_entropy_unavailable(state_effect)
        },
    }
}

fn document_authority_authorization_failure(
    error: wire::DocumentStateAuthorityError,
) -> ActiveTransition {
    match error {
        wire::DocumentStateAuthorityError::NavigationTargetDoesNotMatchPending => {
            stale_state_token()
        },
        wire::DocumentStateAuthorityError::TokenSpaceExhausted => {
            document_authority_token_space_exhausted(RequestStateEffect::None)
        },
        wire::DocumentStateAuthorityError::TokenEntropyUnavailable => {
            document_authority_token_entropy_unavailable(RequestStateEffect::None)
        },
    }
}

fn document_authority_token_entropy_unavailable(
    state_effect: RequestStateEffect,
) -> ActiveTransition {
    ActiveTransition::Fail(ActiveFailure {
        error: fatal_operation(
            "runtime_error",
            "document-state token entropy is unavailable",
            state_effect.as_protocol_str(),
        ),
        fail_stop: true,
    })
}

fn document_authority_token_space_exhausted(state_effect: RequestStateEffect) -> ActiveTransition {
    ActiveTransition::Fail(ActiveFailure {
        error: fatal_operation(
            "runtime_error",
            "document-state token allocator exhausted",
            state_effect.as_protocol_str(),
        ),
        fail_stop: true,
    })
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
        bool,
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
                    synchronous_navigation_emitted,
                } => Ok((result, observation, synchronous_navigation_emitted)),
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
        "document automation request was rejected",
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
        DocumentAutomationError::UnsupportedCheckElement { .. } => "unsupported_check_element",
        DocumentAutomationError::ImmutableCheckElement { .. } => "immutable_check_element",
        DocumentAutomationError::UnsupportedUncheckElement { .. } => "unsupported_uncheck_element",
        DocumentAutomationError::ImmutableUncheckElement { .. } => "immutable_uncheck_element",
        DocumentAutomationError::UnsupportedSelectElement { .. } => "unsupported_select_element",
        DocumentAutomationError::ImmutableSelectElement { .. } => "immutable_select_element",
        DocumentAutomationError::InvalidSelectMultiplicity { .. } => "invalid_select_multiplicity",
        DocumentAutomationError::SelectValueNotFound { .. } => "select_value_not_found",
        DocumentAutomationError::SelectValueDisabled { .. } => "select_value_disabled",
        DocumentAutomationError::UnsupportedFocusElement { .. } => "unsupported_focus_element",
        DocumentAutomationError::UnsupportedSubmitElement { .. } => "unsupported_submit_element",
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

fn transition_from_controlled_open_settle_progress(
    state: &ControlledOpenState,
    progress: settle::SettleProgress,
    state_effect: RequestStateEffect,
) -> ActiveTransition {
    match progress {
        settle::SettleProgress::Command(command) => ActiveTransition::Submit(command),
        settle::SettleProgress::Wait(wait) => ActiveTransition::Wait(wait),
        settle::SettleProgress::Complete(completion) => {
            match controlled_ready_pending(completion, state_effect, true) {
                Ok(pending) => ActiveTransition::ProjectSession(SessionProjectionState {
                    pending,
                    kind: SessionProjectionKind::ControlledOpen {
                        requested_url: state.requested_url.clone(),
                        current_url: state.current_url.clone(),
                        profile: state.profile,
                        deadline: state.deadline,
                        bootstrap_attempted: state.bootstrap_attempted,
                        cumulative_external_io_wall_time: state
                            .settlement
                            .as_ref()
                            .map_or(Duration::ZERO, |settlement| {
                                settlement.cumulative_external_io_wall_time
                            }),
                        session_state_token: None,
                        settle_resume: None,
                    },
                    phase: SessionProjectionPhase::AwaitingInitialNavigation,
                }),
                Err(failure) => ActiveTransition::Fail(failure),
            }
        },
    }
}

fn transition_from_settle_progress_for_active(
    state: &mut SettleState,
    started_at: Instant,
    progress: settle::SettleProgress,
    state_effect: RequestStateEffect,
    projection: &mut wire::WireProjectionContext,
) -> ActiveTransition {
    match progress {
        settle::SettleProgress::Complete(completion) if state.replacement.is_some() => {
            match controlled_ready_pending(completion, state_effect, true) {
                Err(failure) => ActiveTransition::Fail(failure),
                Ok(_) => ActiveTransition::Fail(ActiveFailure {
                    error: fatal_operation(
                        "internal_runtime_failure",
                        "settlement reached quiescence before replacement authority was controlled-ready",
                        state_effect.as_protocol_str(),
                    ),
                    fail_stop: true,
                }),
            }
        },
        settle::SettleProgress::Command(command)
            if state.profile.supports_session_api() &&
                command == DocumentControlCommand::DriveOneTurn =>
        {
            if state.authority_bound_command.replace(command).is_some() ||
                state.latest_pending_target.is_none()
            {
                return ActiveTransition::Fail(ActiveFailure {
                    error: fatal_operation(
                        "internal_runtime_failure",
                        "settlement could not bind one pending Drive to fresh document authority",
                        state_effect.as_protocol_str(),
                    ),
                    fail_stop: true,
                });
            }
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false,
            }
        },
        settle::SettleProgress::Command(command) => ActiveTransition::Submit(command),
        settle::SettleProgress::Wait(wait) => ActiveTransition::Wait(wait),
        settle::SettleProgress::Complete(completion) => match &state.response {
            SettleResponse::Runtime => {
                let pending = state
                    .profile
                    .supports_session_api()
                    .then(|| completion.pending().clone());
                let result = wire::RuntimeSettleResult::project(
                    completion,
                    Instant::now().saturating_duration_since(started_at),
                    state.effective_policy,
                    projection,
                );
                match pending {
                    Some(pending) => project_session_value_with_resume(
                        result,
                        &pending,
                        true,
                        state_effect,
                        Some(SettleProjectionResume {
                            profile: state.profile,
                            effective_policy: state.effective_policy,
                            cumulative_external_io_wall_time: state
                                .cumulative_external_io_wall_time,
                            authorizing_navigation: state.authorizing_navigation.clone(),
                            response: state.response.clone(),
                        }),
                    ),
                    None => serialize_result(result, state_effect),
                }
            },
            SettleResponse::Pending => {
                let pending = match controlled_ready_pending(completion, state_effect, true) {
                    Ok(pending) => pending,
                    Err(failure) => return ActiveTransition::Fail(failure),
                };
                let result = wire::RuntimePendingResult::project(&pending, projection);
                project_session_value_with_resume(
                    result,
                    &pending,
                    false,
                    state_effect,
                    Some(SettleProjectionResume {
                        profile: state.profile,
                        effective_policy: state.effective_policy,
                        cumulative_external_io_wall_time: state.cumulative_external_io_wall_time,
                        authorizing_navigation: state.authorizing_navigation.clone(),
                        response: SettleResponse::Pending,
                    }),
                )
            },
            SettleResponse::Automation { kind, result } => {
                let pending = match controlled_ready_pending(completion, state_effect, true) {
                    Ok(pending) => pending,
                    Err(failure) => return ActiveTransition::Fail(failure),
                };
                let response = SettleResponse::Automation {
                    kind: *kind,
                    result: result.clone(),
                };
                ActiveTransition::ProjectSession(SessionProjectionState {
                    pending,
                    kind: SessionProjectionKind::Automation {
                        settle_resume: SettleProjectionResume {
                            profile: state.profile,
                            effective_policy: state.effective_policy,
                            cumulative_external_io_wall_time: state
                                .cumulative_external_io_wall_time,
                            authorizing_navigation: state.authorizing_navigation.clone(),
                            response,
                        },
                        replacement_rearm: true,
                    },
                    phase: SessionProjectionPhase::AwaitingInitialNavigation,
                })
            },
            SettleResponse::ControlledOpen {
                requested_url,
                current_url,
                profile,
                deadline,
                bootstrap_attempted,
            } => match controlled_ready_pending(completion, state_effect, true) {
                Ok(pending) => ActiveTransition::ProjectSession(SessionProjectionState {
                    pending,
                    kind: SessionProjectionKind::ControlledOpen {
                        requested_url: requested_url.clone(),
                        current_url: current_url.clone(),
                        profile: *profile,
                        deadline: *deadline,
                        bootstrap_attempted: *bootstrap_attempted,
                        cumulative_external_io_wall_time: state.cumulative_external_io_wall_time,
                        session_state_token: None,
                        settle_resume: Some(SettleProjectionResume {
                            profile: state.profile,
                            effective_policy: state.effective_policy,
                            cumulative_external_io_wall_time: state
                                .cumulative_external_io_wall_time,
                            authorizing_navigation: state.authorizing_navigation.clone(),
                            response: state.response.clone(),
                        }),
                    },
                    phase: SessionProjectionPhase::AwaitingInitialNavigation,
                }),
                Err(failure) => ActiveTransition::Fail(failure),
            },
            SettleResponse::Navigate {
                requested_url,
                source,
                admitted,
            } => match controlled_ready_pending(completion, state_effect, true) {
                Ok(pending) => ActiveTransition::ProjectSession(SessionProjectionState {
                    pending,
                    kind: SessionProjectionKind::Navigate {
                        requested_url: requested_url.clone(),
                        source: source.clone(),
                        admitted: admitted.clone(),
                        cumulative_external_io_wall_time: state.cumulative_external_io_wall_time,
                        settle_resume: Some(SettleProjectionResume {
                            profile: state.profile,
                            effective_policy: state.effective_policy,
                            cumulative_external_io_wall_time: state
                                .cumulative_external_io_wall_time,
                            authorizing_navigation: state.authorizing_navigation.clone(),
                            response: state.response.clone(),
                        }),
                    },
                    phase: SessionProjectionPhase::AwaitingInitialNavigation,
                }),
                Err(failure) => ActiveTransition::Fail(failure),
            },
        },
    }
}

fn transition_from_navigate_settle_progress(
    state: &mut NavigateState,
    progress: settle::SettleProgress,
    state_effect: RequestStateEffect,
) -> ActiveTransition {
    match progress {
        settle::SettleProgress::Command(command) => ActiveTransition::Submit(command),
        settle::SettleProgress::Wait(wait) => ActiveTransition::Wait(wait),
        settle::SettleProgress::Complete(completion) => {
            let NavigatePhase::Settling { source, admitted } = &state.phase else {
                return unexpected_navigation_completion(state_effect);
            };
            match controlled_ready_pending(completion, state_effect, true) {
                Ok(pending) => ActiveTransition::ProjectSession(SessionProjectionState {
                    pending,
                    kind: SessionProjectionKind::Navigate {
                        requested_url: state.requested_url.clone(),
                        source: source.clone(),
                        admitted: admitted.clone(),
                        cumulative_external_io_wall_time: state.cumulative_external_io_wall_time,
                        settle_resume: None,
                    },
                    phase: SessionProjectionPhase::AwaitingInitialNavigation,
                }),
                Err(failure) => ActiveTransition::Fail(failure),
            }
        },
    }
}

fn controlled_ready_pending(
    completion: settle::SettleCompletion,
    state_effect: RequestStateEffect,
    fail_stop: bool,
) -> Result<Box<servo::document_pending::RawPendingSnapshot>, ActiveFailure> {
    let (pending, code, message, details) = match completion {
        settle::SettleCompletion::Quiescent { pending, .. } |
        settle::SettleCompletion::QuiescentWithPersistentWork { pending, .. } => {
            return Ok(pending);
        },
        settle::SettleCompletion::BlockedOnExternalIo {
            pending, network, ..
        } => (
            pending,
            "blocked_on_external_io",
            "navigation could not reach controlled_ready while external I/O remained active",
            Some(json!({ "activeOperations": network.len().to_string() })),
        ),
        settle::SettleCompletion::BlockedOnOpenEndedWork {
            pending,
            persistent,
            ..
        } => (
            pending,
            "blocked_on_open_ended_work",
            "navigation could not reach controlled_ready while open-ended work blocked progress",
            Some(json!({ "persistentWork": persistent.len().to_string() })),
        ),
        settle::SettleCompletion::VirtualTimeLimitExceeded {
            pending,
            start_virtual_time_ns,
            requested_virtual_time_ns,
            limit,
            ..
        } => (
            pending,
            "virtual_time_limit_exceeded",
            "navigation could not reach controlled_ready within the virtual-time limit",
            Some(json!({
                "limit": limit.as_nanos().to_string(),
                "startVirtualTimeNs": start_virtual_time_ns.to_string(),
                "requestedVirtualTimeNs": requested_virtual_time_ns.to_string(),
            })),
        ),
        settle::SettleCompletion::ControlTurnLimitExceeded {
            pending,
            limit,
            control_turns,
        } => (
            pending,
            "control_turn_limit_exceeded",
            "navigation could not reach controlled_ready within the control-turn limit",
            Some(json!({
                "limit": limit.to_string(),
                "observed": control_turns.to_string(),
            })),
        ),
        settle::SettleCompletion::ExecutionLimitExceeded {
            pending,
            budget,
            limit,
            observed,
            ..
        } => {
            let (code, kind) = match budget {
                timers::DocumentExecutionBudget::OrdinaryTasks => {
                    ("task_limit_exceeded", "ordinary_tasks")
                },
                timers::DocumentExecutionBudget::Microtasks => {
                    ("microtask_limit_exceeded", "microtasks")
                },
                timers::DocumentExecutionBudget::RenderingOpportunities => {
                    ("rendering_limit_exceeded", "rendering_opportunities")
                },
                timers::DocumentExecutionBudget::MutationRecords => {
                    ("mutation_limit_exceeded", "mutations")
                },
            };
            (
                pending,
                code,
                "navigation could not reach controlled_ready within an execution limit",
                Some(json!({
                    "kind": kind,
                    "limit": limit.to_string(),
                    "observed": observed.to_string(),
                })),
            )
        },
        settle::SettleCompletion::RuntimeError {
            pending, failure, ..
        } => {
            let (outcome, details) =
                wire::project_controlled_ready_failure_details(&failure, &pending);
            let unsupported = outcome == wire::SettleOutcome::UnsupportedWork;
            (
                pending,
                if unsupported {
                    "unsupported_work"
                } else {
                    "runtime_error"
                },
                if unsupported {
                    "navigation encountered work outside the controlled-session support profile"
                } else {
                    "navigation settlement reached a runtime error"
                },
                Some(
                    serde_json::to_value(details)
                        .expect("bounded controlled-ready failure details serialize"),
                ),
            )
        },
    };
    let mut error = ProtocolError::operation(code, message, state_effect.as_protocol_str());
    if let Some(details) = details {
        error = with_error_details(error, details);
    }
    error.fatal = fail_stop;
    let _ = pending;
    Err(ActiveFailure { error, fail_stop })
}

fn restart_session_projection_after_drift(
    kind: &SessionProjectionKind,
    state_effect: RequestStateEffect,
) -> Result<(ActiveOperation, ActiveTransition), ActiveFailure> {
    if matches!(kind, SessionProjectionKind::Automation { .. }) {
        return Err(ActiveFailure {
            error: fatal_operation(
                "document_authority_changed",
                "document authority changed while the action result was being stabilized",
                state_effect.as_protocol_str(),
            ),
            fail_stop: true,
        });
    }
    let resume = session_projection_settle_resume(kind);
    let policy = resume.map_or_else(settle::SettlePolicy::default, |resume| {
        resume.effective_policy.engine
    });
    let mut coordinator = settle::SettleCoordinator::new(policy);
    let progress = coordinator.start().map_err(|error| match resume {
        Some(resume) => settle_failure_for_response(error, state_effect, None, &resume.response),
        None => settle_failure(error, state_effect, None),
    })?;
    let transition = match progress {
        settle::SettleProgress::Command(command) => ActiveTransition::Submit(command),
        settle::SettleProgress::Wait(_) | settle::SettleProgress::Complete(_) => {
            return Err(ActiveFailure {
                error: fatal_operation(
                    "internal_runtime_failure",
                    "a fresh settlement coordinator did not request its initial observation",
                    state_effect.as_protocol_str(),
                ),
                fail_stop: true,
            });
        },
    };
    let operation = if let Some(resume) = resume {
        ActiveOperation::Settle(SettleState {
            profile: resume.profile,
            authorizing_document_state: None,
            authorizing_observation: None,
            authorizing_navigation: resume.authorizing_navigation.clone(),
            replacement: None,
            authority_bound_command: None,
            latest_pending_target: None,
            response: resume.response.clone(),
            coordinator,
            effective_policy: resume.effective_policy,
            cumulative_external_io_wall_time: resume.cumulative_external_io_wall_time,
            waiting: None,
        })
    } else {
        match kind {
            SessionProjectionKind::ControlledOpen {
                requested_url,
                current_url,
                profile,
                deadline,
                bootstrap_attempted,
                cumulative_external_io_wall_time,
                settle_resume: None,
                ..
            } => ActiveOperation::ControlledOpen(ControlledOpenState {
                requested_url: requested_url.clone(),
                current_url: current_url.clone(),
                profile: *profile,
                deadline: *deadline,
                readiness_waiting: None,
                bootstrap_attempted: *bootstrap_attempted,
                settlement: Some(ControlledOpenSettlement {
                    coordinator,
                    cumulative_external_io_wall_time: *cumulative_external_io_wall_time,
                    waiting: None,
                }),
            }),
            SessionProjectionKind::Navigate {
                requested_url,
                source,
                admitted,
                cumulative_external_io_wall_time,
                settle_resume: None,
            } => ActiveOperation::Navigate(NavigateState {
                requested_url: requested_url.clone(),
                phase: NavigatePhase::Settling {
                    source: source.clone(),
                    admitted: admitted.clone(),
                },
                coordinator,
                cumulative_external_io_wall_time: *cumulative_external_io_wall_time,
                waiting: None,
            }),
            SessionProjectionKind::Value {
                settle_resume: None,
                ..
            } => {
                return Err(ActiveFailure {
                    error: ProtocolError::operation(
                        "document_authority_changed",
                        "document state changed while the session result was being stabilized",
                        state_effect.as_protocol_str(),
                    ),
                    fail_stop: false,
                });
            },
            SessionProjectionKind::ControlledOpen {
                settle_resume: Some(_),
                ..
            } |
            SessionProjectionKind::Navigate {
                settle_resume: Some(_),
                ..
            } |
            SessionProjectionKind::Value {
                settle_resume: Some(_),
                ..
            } |
            SessionProjectionKind::Automation { .. } => {
                unreachable!("projection resume was handled before legacy restart")
            },
        }
    };
    Ok((operation, transition))
}

fn session_projection_settle_resume(
    kind: &SessionProjectionKind,
) -> Option<&SettleProjectionResume> {
    match kind {
        SessionProjectionKind::Automation { settle_resume, .. } => Some(settle_resume),
        SessionProjectionKind::Value { settle_resume, .. } |
        SessionProjectionKind::Navigate { settle_resume, .. } |
        SessionProjectionKind::ControlledOpen { settle_resume, .. } => settle_resume.as_ref(),
    }
}

fn session_projection_settle_resume_mut(
    kind: &mut SessionProjectionKind,
) -> Option<&mut SettleProjectionResume> {
    match kind {
        SessionProjectionKind::Automation { settle_resume, .. } => Some(settle_resume),
        SessionProjectionKind::Value { settle_resume, .. } |
        SessionProjectionKind::Navigate { settle_resume, .. } |
        SessionProjectionKind::ControlledOpen { settle_resume, .. } => settle_resume.as_mut(),
    }
}

fn session_projection_allows_replacement_rearm(kind: &SessionProjectionKind) -> bool {
    !matches!(
        kind,
        SessionProjectionKind::Automation {
            replacement_rearm: false,
            ..
        }
    )
}

fn should_pump_servo(
    active: Option<&ActiveRequest>,
    force_initial_pump: bool,
    servo_changed: bool,
) -> bool {
    !active.is_some_and(|active| active_operation_suppresses_servo_pump(&active.operation)) &&
        (force_initial_pump || servo_changed)
}

fn active_operation_suppresses_servo_pump(operation: &ActiveOperation) -> bool {
    matches!(
        operation,
        ActiveOperation::SessionProjection(SessionProjectionState {
            phase: SessionProjectionPhase::AwaitingStableNavigation { .. } |
                SessionProjectionPhase::AwaitingReplacementAdmission { .. },
            ..
        }) | ActiveOperation::SessionProjection(SessionProjectionState {
            phase: SessionProjectionPhase::AwaitingInitialNavigation,
            kind: SessionProjectionKind::Value {
                settle_resume: Some(SettleProjectionResume {
                    response: SettleResponse::Automation { .. },
                    ..
                }),
                ..
            },
            ..
        }) | ActiveOperation::SessionProjection(SessionProjectionState {
            phase: SessionProjectionPhase::AwaitingInitialNavigation,
            kind: SessionProjectionKind::Automation { .. },
            ..
        }) | ActiveOperation::Settle(SettleState {
            replacement: Some(
                SettleReplacementPhase::AwaitingAdmission { .. } |
                    SettleReplacementPhase::AwaitingActivation { .. }
            ),
            ..
        }) | ActiveOperation::Settle(SettleState {
            authority_bound_command: Some(_),
            ..
        }) | ActiveOperation::Settle(SettleState {
            authorizing_document_state: Some(_),
            ..
        }) | ActiveOperation::Automation(AutomationState {
            completed: Some(_),
            ..
        })
    )
}

fn session_projection_suppresses_initial_pump(kind: &SessionProjectionKind) -> bool {
    session_projection_settle_resume(kind)
        .is_some_and(|resume| matches!(resume.response, SettleResponse::Automation { .. }))
}

fn resume_session_projection_at_replacement(
    resume: &SettleProjectionResume,
    source: SessionNavigationAuthority,
    admitted: SessionNavigationAuthority,
    pending_target: Box<servo::document_pending::PendingTargetObservation>,
    state_effect: RequestStateEffect,
) -> Result<(ActiveOperation, ActiveTransition), ActiveFailure> {
    let Some(admission) = exact_replacement_admission(&source, &admitted) else {
        return Err(ActiveFailure {
            error: fatal_operation(
                "navigation_authority_changed",
                "session projection did not observe one exact replacement admission",
                state_effect.as_protocol_str(),
            ),
            fail_stop: true,
        });
    };
    let bootstrap = DocumentControlCommand::BootstrapReplacementPipeline {
        source_pipeline_id: admission.source_pipeline_id,
        pipeline_id: admission.pipeline_id,
    };
    let mut coordinator = settle::SettleCoordinator::new(resume.effective_policy.engine);
    let command = match coordinator.start_with_replacement_bootstrap(bootstrap.clone()) {
        Ok(settle::SettleProgress::Command(command)) if command == bootstrap => command,
        Ok(_) => {
            return Err(ActiveFailure {
                error: fatal_operation(
                    "internal_runtime_failure",
                    "projection replacement did not issue its exact bootstrap command",
                    state_effect.as_protocol_str(),
                ),
                fail_stop: true,
            });
        },
        Err(error) => {
            return Err(settle_failure_for_response(
                error,
                state_effect,
                None,
                &resume.response,
            ));
        },
    };
    Ok((
        ActiveOperation::Settle(SettleState {
            profile: resume.profile,
            authorizing_document_state: None,
            authorizing_observation: None,
            authorizing_navigation: Some(source.clone()),
            replacement: Some(SettleReplacementPhase::Bootstrapping {
                source,
                admitted,
                command: command.clone(),
            }),
            authority_bound_command: None,
            latest_pending_target: Some(pending_target),
            response: resume.response.clone(),
            coordinator,
            effective_policy: resume.effective_policy,
            cumulative_external_io_wall_time: resume.cumulative_external_io_wall_time,
            waiting: None,
        }),
        ActiveTransition::Submit(command),
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReplacementAdmission {
    source_pipeline_id: servo_base::id::PipelineId,
    pipeline_id: servo_base::id::PipelineId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReplacementActivationObservation {
    Pending,
    ActivatedAwaitingSourceExit,
    ControlledReady,
    Invalid,
}

/// Prove the only pending-document shape which may bridge a typed in-process Drive boundary or
/// start explicit session settlement. This compares two independently owner-captured session
/// authorities; neither the shell nor ScriptThread fabricates the replacement membership.
fn exact_replacement_admission(
    source: &SessionNavigationAuthority,
    admitted: &SessionNavigationAuthority,
) -> Option<ReplacementAdmission> {
    if source.terminal().is_some() ||
        admitted.terminal().is_some() ||
        admitted.url().scheme() != "http" && admitted.url().scheme() != "https" ||
        admitted.document_epoch() != source.document_epoch() ||
        admitted.successful_document_replacements() != source.successful_document_replacements() ||
        admitted.history_revision() < source.history_revision() ||
        admitted.navigation_id().get() != source.navigation_id().get().checked_add(1)?
    {
        return None;
    }

    let before = source.target();
    let after = admitted.target();
    let source_active = before.active_top_level?;
    let [source_pipeline_id] = before.pipelines() else {
        return None;
    };
    let [fully_active_source] = before.fully_active_pipelines() else {
        return None;
    };
    let [pipeline_id] = after.pending_top_level_pipelines() else {
        return None;
    };
    if source_active.pipeline_id != *source_pipeline_id ||
        *fully_active_source != *source_pipeline_id ||
        !before.pending_top_level_pipelines().is_empty() ||
        *pipeline_id == *source_pipeline_id ||
        after.webview_id != before.webview_id ||
        after.event_loop_id != before.event_loop_id ||
        after.unsupported_time_surface != before.unsupported_time_surface ||
        after.active_top_level != Some(source_active) ||
        after.fully_active_pipelines() != [*source_pipeline_id] ||
        after.pipelines().len() != 2 ||
        !after.contains_pipeline(*source_pipeline_id) ||
        !after.contains_pipeline(*pipeline_id) ||
        before.navigation_revision.checked_next()? != after.navigation_revision ||
        before.pipeline_membership_revision.checked_next()? != after.pipeline_membership_revision
    {
        return None;
    }

    Some(ReplacementAdmission {
        source_pipeline_id: *source_pipeline_id,
        pipeline_id: *pipeline_id,
    })
}

fn initial_navigation_reached_controlled_ready(navigation: &SessionNavigationAuthority) -> bool {
    navigation.navigation_id().get() == 0 &&
        navigation.document_epoch().get() == 1 &&
        navigation.successful_document_replacements() == 0 &&
        navigation.terminal().is_none()
}

fn session_navigation_reached_controlled_ready(navigation: &SessionNavigationAuthority) -> bool {
    let target = navigation.target();
    navigation.document_epoch().get() >= 1 &&
        navigation.terminal().is_none() &&
        matches!(navigation.url().scheme(), "http" | "https") &&
        target.active_top_level.is_some() &&
        target.pipelines().len() == 1 &&
        target.fully_active_pipelines() == target.pipelines() &&
        target.pending_top_level_pipelines().is_empty()
}

fn explicit_navigation_chain_reached_controlled_ready(
    source: &SessionNavigationAuthority,
    navigation: &SessionNavigationAuthority,
) -> bool {
    session_navigation_reached_controlled_ready(navigation) &&
        navigation.target().webview_id == source.target().webview_id &&
        navigation.target().event_loop_id == source.target().event_loop_id &&
        navigation.document_epoch() > source.document_epoch() &&
        navigation.successful_document_replacements() > source.successful_document_replacements()
}

fn explicit_navigation_reached_controlled_ready(
    source: &SessionNavigationAuthority,
    admitted: &SessionNavigationAuthority,
    observed: &SessionNavigationAuthority,
) -> bool {
    let Some(admission) = exact_replacement_admission(source, admitted) else {
        return false;
    };
    let Some(expected_document_epoch) = source.document_epoch().get().checked_add(1) else {
        return false;
    };
    let Some(expected_replacements) = source.successful_document_replacements().checked_add(1)
    else {
        return false;
    };
    let before = admitted.target();
    let after = observed.target();
    observed.navigation_id() == admitted.navigation_id() &&
        observed.document_epoch().get() == expected_document_epoch &&
        observed.successful_document_replacements() == expected_replacements &&
        observed.history_revision() >= admitted.history_revision() &&
        matches!(observed.url().scheme(), "http" | "https") &&
        observed.terminal().is_none() &&
        after.webview_id == before.webview_id &&
        after.event_loop_id == before.event_loop_id &&
        after.unsupported_time_surface == before.unsupported_time_surface &&
        after.active_top_level.is_some_and(|active| {
            active.pipeline_id == admission.pipeline_id &&
                active.pipeline_id != admission.source_pipeline_id
        }) &&
        after.pipelines() == [admission.pipeline_id] &&
        after.fully_active_pipelines() == [admission.pipeline_id] &&
        after.pending_top_level_pipelines().is_empty() &&
        before
            .navigation_revision
            .checked_next()
            .and_then(|revision| revision.checked_next()) ==
            Some(after.navigation_revision) &&
        before.pipeline_membership_revision.checked_next() ==
            Some(after.pipeline_membership_revision)
}

/// Recognize the sole valid activation state before the asynchronously exiting source pipeline
/// has left Constellation's pipeline map. This is not controlled-ready and cannot authorize a
/// public token; settlement must keep pumping until the source membership is removed.
fn explicit_navigation_activation_target_awaiting_source_exit(
    source: &SessionNavigationAuthority,
    admitted: &SessionNavigationAuthority,
    after: &servo::document_pending::PendingTargetObservation,
) -> bool {
    let Some(admission) = exact_replacement_admission(source, admitted) else {
        return false;
    };
    let before = admitted.target();
    after.webview_id == before.webview_id &&
        after.event_loop_id == before.event_loop_id &&
        after.unsupported_time_surface == before.unsupported_time_surface &&
        after.active_top_level.is_some_and(|active| {
            active.pipeline_id == admission.pipeline_id &&
                active.pipeline_id != admission.source_pipeline_id
        }) &&
        after.pipelines().len() == 2 &&
        after.contains_pipeline(admission.source_pipeline_id) &&
        after.contains_pipeline(admission.pipeline_id) &&
        after.fully_active_pipelines() == [admission.pipeline_id] &&
        after.pending_top_level_pipelines().is_empty() &&
        before
            .navigation_revision
            .checked_next()
            .and_then(|revision| revision.checked_next()) ==
            Some(after.navigation_revision) &&
        before.pipeline_membership_revision == after.pipeline_membership_revision
}

fn explicit_navigation_activated_awaiting_source_exit(
    source: &SessionNavigationAuthority,
    admitted: &SessionNavigationAuthority,
    observed: &SessionNavigationAuthority,
) -> bool {
    let Some(expected_document_epoch) = source.document_epoch().get().checked_add(1) else {
        return false;
    };
    let Some(expected_replacements) = source.successful_document_replacements().checked_add(1)
    else {
        return false;
    };
    observed.navigation_id() == admitted.navigation_id() &&
        observed.document_epoch().get() == expected_document_epoch &&
        observed.successful_document_replacements() == expected_replacements &&
        observed.history_revision() >= admitted.history_revision() &&
        matches!(observed.url().scheme(), "http" | "https") &&
        observed.terminal().is_none() &&
        explicit_navigation_activation_target_awaiting_source_exit(
            source,
            admitted,
            observed.target(),
        )
}

fn classify_replacement_activation_observation(
    source: &SessionNavigationAuthority,
    admitted: &SessionNavigationAuthority,
    observed: &SessionNavigationAuthority,
) -> ReplacementActivationObservation {
    if explicit_navigation_reached_controlled_ready(source, admitted, observed) {
        ReplacementActivationObservation::ControlledReady
    } else if observed == admitted {
        ReplacementActivationObservation::Pending
    } else if explicit_navigation_activated_awaiting_source_exit(source, admitted, observed) {
        ReplacementActivationObservation::ActivatedAwaitingSourceExit
    } else {
        ReplacementActivationObservation::Invalid
    }
}

fn held_drive_replacement_target_progressed(
    source: &SessionNavigationAuthority,
    admitted: &SessionNavigationAuthority,
    before: &servo::document_pending::PendingTargetObservation,
    observed: &SessionNavigationAuthority,
) -> bool {
    match classify_replacement_activation_observation(source, admitted, observed) {
        ReplacementActivationObservation::ActivatedAwaitingSourceExit => {
            before == admitted.target()
        },
        ReplacementActivationObservation::ControlledReady => {
            before == admitted.target() ||
                explicit_navigation_activation_target_awaiting_source_exit(
                    source, admitted, before,
                )
        },
        ReplacementActivationObservation::Pending | ReplacementActivationObservation::Invalid => {
            false
        },
    }
}

fn navigation_activation_failure(
    state_effect: RequestStateEffect,
    fail_stop: bool,
) -> ActiveTransition {
    let mut error = ProtocolError::operation(
        "navigation_authority_changed",
        "the admitted navigation did not activate the exact controlled document",
        state_effect.as_protocol_str(),
    );
    error.fatal = fail_stop;
    ActiveTransition::Fail(ActiveFailure { error, fail_stop })
}

fn project_session_value<T: serde::Serialize>(
    result: T,
    pending: &servo::document_pending::RawPendingSnapshot,
    snapshot_token: bool,
    state_effect: RequestStateEffect,
) -> ActiveTransition {
    project_session_value_with_resume(result, pending, snapshot_token, state_effect, None)
}

fn project_session_pending(
    profile: SessionProfile,
    result: wire::RuntimePendingResult,
    pending: &servo::document_pending::RawPendingSnapshot,
    state_effect: RequestStateEffect,
) -> ActiveTransition {
    let effective_policy = default_resolved_settle_policy();
    project_session_value_with_resume(
        result,
        pending,
        false,
        state_effect,
        Some(SettleProjectionResume {
            profile,
            effective_policy,
            cumulative_external_io_wall_time: Duration::ZERO,
            authorizing_navigation: None,
            response: SettleResponse::Pending,
        }),
    )
}

fn project_session_value_with_resume<T: serde::Serialize>(
    result: T,
    pending: &servo::document_pending::RawPendingSnapshot,
    snapshot_token: bool,
    state_effect: RequestStateEffect,
    settle_resume: Option<SettleProjectionResume>,
) -> ActiveTransition {
    match serde_json::to_value(result) {
        Ok(value) => ActiveTransition::ProjectSession(SessionProjectionState {
            pending: Box::new(pending.clone()),
            kind: SessionProjectionKind::Value {
                value,
                snapshot_token,
                settle_resume,
            },
            phase: SessionProjectionPhase::AwaitingInitialNavigation,
        }),
        Err(error) => ActiveTransition::Fail(ActiveFailure {
            error: fatal_operation(
                "internal_runtime_failure",
                format!("failed to serialize controlled-session result: {error}"),
                state_effect.as_protocol_str(),
            ),
            fail_stop: true,
        }),
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

fn settle_failure_for_response(
    error: settle::SettleFailure,
    state_effect: RequestStateEffect,
    command: Option<&DocumentControlCommand>,
    response: &SettleResponse,
) -> ActiveFailure {
    let failure = settle_failure(error, state_effect, command);
    if matches!(response, SettleResponse::Runtime) {
        failure
    } else {
        harden_continuation_failure(failure, state_effect)
    }
}

fn harden_continuation_failure(
    mut failure: ActiveFailure,
    state_effect: RequestStateEffect,
) -> ActiveFailure {
    if state_effect == RequestStateEffect::Partial && !failure.fail_stop {
        failure.error.fatal = true;
        failure.error.state_effect = "partial";
        failure.fail_stop = true;
    }
    failure
}

fn command_is_mutating(command: &DocumentControlCommand) -> bool {
    match command {
        DocumentControlCommand::Observe => false,
        DocumentControlCommand::DriveOneTurn |
        DocumentControlCommand::BootstrapInitialPipeline { .. } |
        DocumentControlCommand::BootstrapReplacementPipeline { .. } |
        DocumentControlCommand::AdvanceTo(_) => true,
        DocumentControlCommand::Automate(request) => {
            DocumentControlAutomationKind::from_request(request).is_mutating()
        },
    }
}

fn receive_outcome_virtual_time_ns(outcome: &DocumentControlReceiveOutcome) -> Option<u128> {
    match outcome {
        DocumentControlReceiveOutcome::CommandOutcome(DocumentControlOutcome::Completed(
            observation,
        )) |
        DocumentControlReceiveOutcome::CommandOutcome(
            DocumentControlOutcome::AutomationCompleted { observation, .. },
        ) => Some(observation.pending().clock.now.as_nanos()),
        _ => None,
    }
}

fn receive_outcome_pending_target(
    outcome: &DocumentControlReceiveOutcome,
) -> Option<&servo::document_pending::PendingTargetObservation> {
    match outcome {
        DocumentControlReceiveOutcome::CommandOutcome(DocumentControlOutcome::Completed(
            observation,
        )) |
        DocumentControlReceiveOutcome::CommandOutcome(
            DocumentControlOutcome::AutomationCompleted { observation, .. },
        ) => Some(&observation.pending().target),
        _ => None,
    }
}

fn session_state_protocol_error(error: SessionStateError) -> ProtocolError {
    let code = match error {
        SessionStateError::InvalidCookie => "invalid_controlled_cookie",
        _ => error.code(),
    };
    let mut protocol = ProtocolError::operation(
        code,
        "session-state operation was rejected",
        match error {
            SessionStateError::BackendRejected(
                stasis_shell::session_state::SessionStateBackendStage::CookieReplace |
                stasis_shell::session_state::SessionStateBackendStage::WebStorageReplace,
            ) => "indeterminate",
            _ => "none",
        },
    );
    if matches!(
        error,
        SessionStateError::TokenEntropyUnavailable |
            SessionStateError::TokenSpaceExhausted |
            SessionStateError::BackendRevisionRegressed |
            SessionStateError::BackendRejected(
                stasis_shell::session_state::SessionStateBackendStage::CookieReplace |
                    stasis_shell::session_state::SessionStateBackendStage::WebStorageReplace
            )
    ) {
        protocol.fatal = true;
    }
    protocol
}

fn harden_session_state_mutation_error(method: &str, mut error: ProtocolError) -> ProtocolError {
    if matches!(method, "session.cookies.set" | "session.storage.set") &&
        matches!(
            error.code,
            "session_state_backend_observe_failed" |
                "session_state_cookie_replace_failed" |
                "session_state_web_storage_replace_failed" |
                "session_state_backend_revision_regressed" |
                "session_state_token_entropy_unavailable" |
                "session_state_token_space_exhausted"
        )
    {
        error.fatal = true;
        error.state_effect = "indeterminate";
    }
    error
}

fn network_evidence_protocol_error(error: EvidenceLedgerError) -> ProtocolError {
    match error {
        EvidenceLedgerError::InvalidPageLimit { observed, limit } => with_error_details(
            ProtocolError::invalid_request("invalid session audit page limit"),
            json!({
                "observed": observed,
                "limit": limit,
            }),
        ),
        error => fatal_operation(
            "internal_runtime_failure",
            format!("controlled-network evidence ledger failed: {error:?}"),
            "none",
        ),
    }
}

fn controlled_network_failure(
    snapshot: ControlledNetworkSnapshot,
    state_effect: RequestStateEffect,
) -> Option<ActiveFailure> {
    let failure = snapshot.sticky_failure?;
    let effect = if state_effect == RequestStateEffect::Partial {
        "partial"
    } else {
        "partial"
    };
    let (error, fail_stop) = match failure {
        ControlledNetworkFailure::FixtureMiss => (
            ProtocolError::operation(
                "network_fixture_miss",
                "fixtures_only rejected a request without an immutable matching route",
                effect,
            ),
            false,
        ),
        ControlledNetworkFailure::ActiveOperationLimitExceeded => (
            with_error_details(
                ProtocolError::operation(
                    "controlled_network_active_operation_limit_exceeded",
                    "controlled-network active operation limit was exceeded",
                    effect,
                ),
                json!({
                    "limit": snapshot.maximum_active_operations,
                    "observed": snapshot.maximum_active_operations.saturating_add(1),
                }),
            ),
            false,
        ),
        ControlledNetworkFailure::UnknownRequestBodyLength => (
            ProtocolError::operation(
                "unsupported_network_request_body_length",
                "controlled network requires a bounded request body length",
                effect,
            ),
            false,
        ),
        ControlledNetworkFailure::RequestMetadataRejected => (
            ProtocolError::operation(
                "unsupported_network_request_metadata",
                "controlled network rejected request metadata outside the support profile",
                effect,
            ),
            false,
        ),
        ControlledNetworkFailure::CookieSameSiteContextUnsupported => (
            ProtocolError::operation(
                "unsupported_cookie_same_site_context",
                "controlled cookie SameSite context is outside the support profile",
                effect,
            ),
            false,
        ),
        ControlledNetworkFailure::PersistentCookieUnsupported => (
            ProtocolError::operation(
                "unsupported_persistent_cookie",
                "controlled sessions support only session cookies",
                effect,
            ),
            false,
        ),
        ControlledNetworkFailure::PartitionedCookieUnsupported => (
            ProtocolError::operation(
                "unsupported_partitioned_cookie",
                "controlled sessions do not support partitioned cookies",
                effect,
            ),
            false,
        ),
        ControlledNetworkFailure::CookieTimeRangeUnsupported => (
            ProtocolError::operation(
                "unsupported_cookie_time_range",
                "controlled cookie time is outside the bounded persistence range",
                effect,
            ),
            false,
        ),
        ControlledNetworkFailure::InvalidCookie => (
            ProtocolError::operation(
                "invalid_controlled_cookie",
                "controlled Set-Cookie metadata was invalid",
                effect,
            ),
            false,
        ),
        ControlledNetworkFailure::EvidenceLedgerFailure |
        ControlledNetworkFailure::LifecycleInvariant |
        ControlledNetworkFailure::VirtualTimeRegressed => (
            fatal_operation(
                "internal_runtime_failure",
                format!("controlled-network authority failed: {failure:?}"),
                if failure == ControlledNetworkFailure::VirtualTimeRegressed {
                    "indeterminate"
                } else {
                    state_effect.as_protocol_str()
                },
            ),
            true,
        ),
    };
    Some(ActiveFailure { error, fail_stop })
}

fn with_error_details(error: ProtocolError, details: Value) -> ProtocolError {
    match error.with_details(details) {
        Ok(error) => error,
        Err(details_error) => fatal_operation(
            "internal_runtime_failure",
            format!("failed to construct bounded protocol error details: {details_error}"),
            "none",
        ),
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
        details: None,
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
    #[serde(default)]
    state: Option<SessionStateV1>,
    #[serde(default)]
    network: Option<Value>,
}

impl OpenParams {
    fn configuration(self) -> Result<OpenConfiguration, ProtocolError> {
        match self.clock_mode {
            OpenClockMode::Real => {
                if self.initial_virtual_time_ns.is_some() ||
                    self.unix_time_origin_ns.is_some() ||
                    self.profile.is_some() ||
                    self.state.is_some() ||
                    self.network.is_some()
                {
                    return Err(ProtocolError::invalid_request(
                        "controlled time, profile, state, and network fields require clockMode controlled",
                    ));
                }
                Ok(OpenConfiguration {
                    clock_mode: EngineClockMode::Real,
                    boundary: "load_complete",
                    profile: None,
                    state: None,
                    network: None,
                })
            },
            OpenClockMode::Controlled => {
                let profile = match self.profile.as_deref() {
                    Some(CONTROLLED_WEBAPP_V1_PROFILE) => SessionProfile::ControlledWebappV1,
                    Some(CONTROLLED_WEB_SESSION_V1_PROFILE) => {
                        SessionProfile::ControlledWebSessionV1
                    },
                    Some(CONTROLLED_WEB_SESSION_V2_PROFILE) => {
                        SessionProfile::ControlledWebSessionV2
                    },
                    _ => {
                        return Err(ProtocolError::invalid_request(format!(
                            "controlled sessions require profile {CONTROLLED_WEBAPP_V1_PROFILE}, {CONTROLLED_WEB_SESSION_V1_PROFILE}, or {CONTROLLED_WEB_SESSION_V2_PROFILE}",
                        )));
                    },
                };
                let unix_origin = self
                    .unix_time_origin_ns
                    .as_ref()
                    .map_or(0, wire::DecimalU128::get);
                if unix_origin != 0 {
                    return Err(ProtocolError::invalid_request(
                        "unixTimeOriginNs must be 0 in the controlled MVP",
                    ));
                }
                if !profile.supports_session_api() &&
                    (self.state.is_some() || self.network.is_some())
                {
                    return Err(ProtocolError::invalid_request(format!(
                        "state and network require profile {CONTROLLED_WEB_SESSION_V1_PROFILE} or {CONTROLLED_WEB_SESSION_V2_PROFILE}",
                    )));
                }
                Ok(OpenConfiguration {
                    clock_mode: EngineClockMode::Controlled {
                        initial_time_ns: self
                            .initial_virtual_time_ns
                            .as_ref()
                            .map_or(0, wire::DecimalU128::get),
                    },
                    boundary: "controlled_ready",
                    profile: Some(profile),
                    state: self.state,
                    network: self.network,
                })
            },
        }
    }
}

struct OpenConfiguration {
    clock_mode: EngineClockMode,
    boundary: &'static str,
    profile: Option<SessionProfile>,
    state: Option<SessionStateV1>,
    network: Option<Value>,
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SessionAuditParams {
    #[serde(default)]
    after_seq: Option<WireU64>,
    #[serde(default)]
    limit: Option<usize>,
}

fn parse_params<T: for<'de> Deserialize<'de>>(request: &Request) -> Result<T, ProtocolError> {
    if let Some(error) = cross_domain_token_error(&request.params) {
        return Err(error);
    }
    serde_json::from_value(request.params.clone()).map_err(|error| {
        if request.method.starts_with("action.") || request.method.starts_with("dom.") {
            ProtocolError::invalid_request(format!("invalid {} parameters", request.method))
        } else {
            ProtocolError::invalid_request(error.to_string())
        }
    })
}

fn parse_sensitive_params<T: for<'de> Deserialize<'de>>(
    request: &Request,
    redacted_message: &'static str,
) -> Result<T, ProtocolError> {
    if let Some(error) = cross_domain_token_error(&request.params) {
        return Err(error);
    }
    serde_json::from_value(request.params.clone())
        .map_err(|_| ProtocolError::invalid_request(redacted_message))
}

fn cross_domain_token_error(params: &Value) -> Option<ProtocolError> {
    let object = params.as_object()?;
    if object
        .get("expectedStateToken")
        .is_some_and(|token| serde_json::from_value::<SessionStateToken>(token.clone()).is_ok())
    {
        return Some(ProtocolError::operation(
            "stale_state_token",
            "expectedStateToken does not authorize the current document state",
            "none",
        ));
    }
    if object
        .get("expectedSessionStateToken")
        .is_some_and(|token| {
            serde_json::from_value::<wire::DocumentStateToken>(token.clone()).is_ok()
        })
    {
        return Some(ProtocolError::operation(
            "stale_session_state_token",
            "expectedSessionStateToken does not authorize the current session state",
            "none",
        ));
    }
    None
}

fn serialize_immediate_result<T: serde::Serialize>(result: T) -> Result<Value, ProtocolError> {
    serde_json::to_value(result).map_err(|error| {
        fatal_operation(
            "internal_runtime_failure",
            format!("failed to serialize public session result: {error}"),
            "none",
        )
    })
}

fn invalid_state(message: impl Into<String>) -> ProtocolError {
    ProtocolError {
        code: "invalid_state",
        message: message.into(),
        fatal: false,
        state_effect: "none",
        details: None,
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
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use super::*;

    const TEST_TOKEN_NAMESPACE_HEX: &str = "71717171717171717171717171717171";

    fn test_document_token(alias: u128) -> String {
        format!("document:{TEST_TOKEN_NAMESPACE_HEX}:{alias}")
    }

    fn test_session_state_token(alias: u64) -> String {
        format!("session:{TEST_TOKEN_NAMESPACE_HEX}:{alias}")
    }

    #[derive(Debug, Eq, PartialEq)]
    enum FakeControlEvent {
        Submit(DocumentControlCommand),
        NetworkTime(u128),
    }

    struct FakeEngine {
        clock_mode: EngineClockMode,
        document_control_profile: DocumentControlProfile,
        document_execution_profile: DocumentExecutionProfile,
        pump_calls: usize,
        cancel_calls: usize,
        navigation_cancel_calls: usize,
        close_calls: usize,
        submitted: Vec<DocumentControlCommand>,
        polls: VecDeque<EnginePortPoll>,
        navigation_submitted: Vec<NavigationOperationKind>,
        navigation_polls: VecDeque<EnginePortNavigationPoll>,
        network_snapshot: Option<ControlledNetworkSnapshot>,
        network_virtual_times: RefCell<Vec<u128>>,
        control_events: Rc<RefCell<Vec<FakeControlEvent>>>,
        evidence: Rc<RefCell<Vec<(&'static str, u64)>>>,
    }

    impl FakeEngine {
        fn controlled() -> Self {
            Self {
                clock_mode: EngineClockMode::Controlled { initial_time_ns: 0 },
                document_control_profile: DocumentControlProfile::SingleDocument,
                document_execution_profile: DocumentExecutionProfile::Baseline,
                pump_calls: 0,
                cancel_calls: 0,
                navigation_cancel_calls: 0,
                close_calls: 0,
                submitted: Vec::new(),
                polls: VecDeque::new(),
                navigation_submitted: Vec::new(),
                navigation_polls: VecDeque::new(),
                network_snapshot: None,
                network_virtual_times: RefCell::new(Vec::new()),
                control_events: Rc::new(RefCell::new(Vec::new())),
                evidence: Rc::new(RefCell::new(Vec::new())),
            }
        }

        fn controlled_session() -> Self {
            let mut engine = Self::controlled();
            engine.document_control_profile = DocumentControlProfile::TopLevelSession;
            engine
        }

        fn controlled_session_v2() -> Self {
            let mut engine = Self::controlled_session();
            engine.document_execution_profile = DocumentExecutionProfile::ControlledWebSessionV2;
            engine
        }
    }

    impl EnginePort for FakeEngine {
        fn open_session(
            _url: Url,
            _waker: ShellWaker,
            options: EngineSessionOpenOptions,
        ) -> Result<Self, ProtocolError> {
            Ok(Self {
                clock_mode: options.clock_mode,
                document_control_profile: options.document_control_profile,
                document_execution_profile: options.document_execution_profile,
                pump_calls: 0,
                cancel_calls: 0,
                navigation_cancel_calls: 0,
                close_calls: 0,
                submitted: Vec::new(),
                polls: VecDeque::new(),
                navigation_submitted: Vec::new(),
                navigation_polls: VecDeque::new(),
                network_snapshot: None,
                network_virtual_times: RefCell::new(Vec::new()),
                control_events: Rc::new(RefCell::new(Vec::new())),
                evidence: Rc::new(RefCell::new(Vec::new())),
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

        fn document_control_profile(&self) -> DocumentControlProfile {
            self.document_control_profile
        }

        fn document_execution_profile(&self) -> DocumentExecutionProfile {
            self.document_execution_profile
        }

        fn evaluate(&self, _expression: &str) -> Result<Value, ProtocolError> {
            Ok(Value::Null)
        }

        fn submit_document_control(
            &mut self,
            command: DocumentControlCommand,
            _timeout: Duration,
        ) -> Result<(), ProtocolError> {
            self.control_events
                .borrow_mut()
                .push(FakeControlEvent::Submit(command.clone()));
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

        fn submit_session_navigation_observation(
            &mut self,
            _timeout: Duration,
        ) -> Result<(), ProtocolError> {
            self.navigation_submitted
                .push(NavigationOperationKind::Observe);
            Ok(())
        }

        fn submit_session_navigation(
            &mut self,
            _expected: SessionNavigationAuthority,
            _url: Url,
            _timeout: Duration,
        ) -> Result<(), ProtocolError> {
            self.navigation_submitted
                .push(NavigationOperationKind::Navigate);
            Ok(())
        }

        fn poll_session_navigation(&mut self) -> EnginePortNavigationPoll {
            self.navigation_polls
                .pop_front()
                .unwrap_or(EnginePortNavigationPoll::Idle)
        }

        fn cancel_session_navigation(&mut self) -> Option<NavigationOperationCompletion> {
            self.navigation_cancel_calls += 1;
            None
        }

        fn controlled_network_snapshot(&self) -> Option<ControlledNetworkSnapshot> {
            self.network_snapshot.clone()
        }

        fn set_controlled_network_virtual_time_ns(
            &self,
            virtual_time_ns: u128,
        ) -> Result<(), ProtocolError> {
            self.control_events
                .borrow_mut()
                .push(FakeControlEvent::NetworkTime(virtual_time_ns));
            self.network_virtual_times
                .borrow_mut()
                .push(virtual_time_ns);
            Ok(())
        }

        fn record_navigation_started(&self, authority: &SessionNavigationAuthority) {
            self.record_navigation_started_id(authority.navigation_id());
        }

        fn record_navigation_started_id(&self, navigation_id: SessionNavigationId) {
            self.evidence
                .borrow_mut()
                .push(("started", navigation_id.get()));
        }

        fn record_navigation_committed(&self, authority: &SessionNavigationAuthority) {
            self.evidence
                .borrow_mut()
                .push(("committed", authority.navigation_id().get()));
        }

        fn record_navigation_failed_id(
            &self,
            navigation_id: SessionNavigationId,
            _reason: NetworkFailureReason,
        ) {
            self.evidence
                .borrow_mut()
                .push(("failed", navigation_id.get()));
        }

        fn record_same_document_history_changed(&self, authority: &SessionNavigationAuthority) {
            self.evidence
                .borrow_mut()
                .push(("history", authority.navigation_id().get()));
        }

        fn record_settlement_terminal(&self, authority: &SessionNavigationAuthority) {
            self.evidence
                .borrow_mut()
                .push(("settled", authority.navigation_id().get()));
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

    fn session_authority(
        document_epoch: u64,
        navigation_id: u64,
        history_revision: u64,
        successful_document_replacements: u64,
    ) -> SessionNavigationAuthority {
        use embedder_traits::document_pending::{
            PendingActiveTopLevelPipeline, PendingNavigationRevision,
            PendingPipelineMembershipRevision, PendingTargetObservation,
        };
        use embedder_traits::document_session::{DocumentEpoch, HistoryRevision};
        use servo_base::Epoch;
        use servo_base::id::{TEST_PIPELINE_ID, TEST_SCRIPT_EVENT_LOOP_ID, TEST_WEBVIEW_ID};

        let target = PendingTargetObservation::new_with_authority(
            TEST_WEBVIEW_ID,
            TEST_SCRIPT_EVENT_LOOP_ID,
            Some(PendingActiveTopLevelPipeline {
                pipeline_id: TEST_PIPELINE_ID,
                epoch: Epoch(1),
            }),
            PendingNavigationRevision::new(navigation_id),
            PendingPipelineMembershipRevision::new(document_epoch),
            None,
            vec![TEST_PIPELINE_ID],
            vec![TEST_PIPELINE_ID],
            Vec::new(),
        )
        .unwrap();
        SessionNavigationAuthority::new_internal(
            Box::new(target),
            DocumentEpoch::new(document_epoch),
            SessionNavigationId::new(navigation_id),
            HistoryRevision::new(history_revision),
            successful_document_replacements,
            servo::ServoUrl::parse("https://example.test/").unwrap(),
            None,
        )
    }

    fn replacement_admission_authority(
        source: &SessionNavigationAuthority,
    ) -> SessionNavigationAuthority {
        use embedder_traits::document_pending::PendingTargetObservation;
        use embedder_traits::document_session::{DocumentEpoch, HistoryRevision};

        let source_pipeline_id = source.target().active_top_level.unwrap().pipeline_id;
        let pipeline_id = pipeline_id(source_pipeline_id.index.0.get().checked_add(1).unwrap());
        let target = PendingTargetObservation::new_with_authority(
            source.target().webview_id,
            source.target().event_loop_id,
            source.target().active_top_level,
            source.target().navigation_revision.checked_next().unwrap(),
            source
                .target()
                .pipeline_membership_revision
                .checked_next()
                .unwrap(),
            source.target().unsupported_time_surface,
            vec![source_pipeline_id, pipeline_id],
            vec![source_pipeline_id],
            vec![pipeline_id],
        )
        .unwrap();
        SessionNavigationAuthority::new_internal(
            Box::new(target),
            DocumentEpoch::new(source.document_epoch().get()),
            SessionNavigationId::new(source.navigation_id().get().checked_add(1).unwrap()),
            HistoryRevision::new(source.history_revision().get()),
            source.successful_document_replacements(),
            servo::ServoUrl::parse("https://example.test/next").unwrap(),
            None,
        )
    }

    fn terminal_session_authority(
        source: &SessionNavigationAuthority,
        terminal: SessionNavigationTerminal,
    ) -> SessionNavigationAuthority {
        SessionNavigationAuthority::new_internal(
            Box::new(source.target().clone()),
            source.document_epoch(),
            source.navigation_id(),
            source.history_revision(),
            source.successful_document_replacements(),
            source.url().clone(),
            Some(terminal),
        )
    }

    fn activated_replacement_authority(
        source: &SessionNavigationAuthority,
        admitted: &SessionNavigationAuthority,
    ) -> SessionNavigationAuthority {
        use embedder_traits::document_pending::{
            PendingActiveTopLevelPipeline, PendingTargetObservation,
        };
        use embedder_traits::document_session::{DocumentEpoch, HistoryRevision};
        use servo_base::Epoch;

        let pipeline_id = admitted.target().pending_top_level_pipelines()[0];
        let target = PendingTargetObservation::new_with_authority(
            admitted.target().webview_id,
            admitted.target().event_loop_id,
            Some(PendingActiveTopLevelPipeline {
                pipeline_id,
                epoch: Epoch(2),
            }),
            admitted
                .target()
                .navigation_revision
                .checked_next()
                .and_then(|revision| revision.checked_next())
                .unwrap(),
            admitted
                .target()
                .pipeline_membership_revision
                .checked_next()
                .unwrap(),
            admitted.target().unsupported_time_surface,
            vec![pipeline_id],
            vec![pipeline_id],
            Vec::new(),
        )
        .unwrap();
        SessionNavigationAuthority::new_internal(
            Box::new(target),
            DocumentEpoch::new(source.document_epoch().get().checked_add(1).unwrap()),
            admitted.navigation_id(),
            HistoryRevision::new(admitted.history_revision().get()),
            source
                .successful_document_replacements()
                .checked_add(1)
                .unwrap(),
            servo::ServoUrl::parse("https://example.test/final").unwrap(),
            None,
        )
    }

    fn activated_replacement_source_retained_authority(
        source: &SessionNavigationAuthority,
        admitted: &SessionNavigationAuthority,
    ) -> SessionNavigationAuthority {
        use embedder_traits::document_pending::{
            PendingActiveTopLevelPipeline, PendingTargetObservation,
        };
        use embedder_traits::document_session::{DocumentEpoch, HistoryRevision};
        use servo_base::Epoch;

        let source_pipeline_id = source.target().active_top_level.unwrap().pipeline_id;
        let pipeline_id = admitted.target().pending_top_level_pipelines()[0];
        let target = PendingTargetObservation::new_with_authority(
            admitted.target().webview_id,
            admitted.target().event_loop_id,
            Some(PendingActiveTopLevelPipeline {
                pipeline_id,
                epoch: Epoch(2),
            }),
            admitted
                .target()
                .navigation_revision
                .checked_next()
                .and_then(|revision| revision.checked_next())
                .unwrap(),
            admitted.target().pipeline_membership_revision,
            admitted.target().unsupported_time_surface,
            vec![source_pipeline_id, pipeline_id],
            vec![pipeline_id],
            Vec::new(),
        )
        .unwrap();
        SessionNavigationAuthority::new_internal(
            Box::new(target),
            DocumentEpoch::new(source.document_epoch().get().checked_add(1).unwrap()),
            admitted.navigation_id(),
            HistoryRevision::new(admitted.history_revision().get()),
            source
                .successful_document_replacements()
                .checked_add(1)
                .unwrap(),
            servo::ServoUrl::parse("https://example.test/final").unwrap(),
            None,
        )
    }

    fn pending_for_authority(
        authority: &SessionNavigationAuthority,
        state_generation: u64,
    ) -> servo::document_pending::RawPendingSnapshot {
        use embedder_traits::document_pending::{
            DomEpoch, PendingAnimatedImageObservation, PendingCanvasObservation, PendingClockMode,
            PendingClockObservation, PendingInputObservation, PendingLogicalTimerSnapshot,
            PendingMicrotaskCheckpoint, PendingMicrotaskObservation, PendingNetworkObservation,
            PendingParserObservation, PendingPipelineRenderingObservation,
            PendingProducerObservation, PendingProducerStability, PendingRenderingObservation,
            PendingRenderingPipelineActivity, PendingRuntimeTerminals, PendingSchedulerObservation,
            PendingSourceSnapshot, RawPendingSnapshot, RuntimeStateGeneration,
        };
        use timers::{
            DocumentClock, DocumentClockConfiguration, DocumentExecutionCounters,
            DocumentExecutionLimits, DocumentExecutionObservation, DocumentProducerCheckpoint,
            DocumentProducerFence, DocumentUnixTime, TimerScheduler,
        };

        let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 5,
            unix_time_origin_ns: DocumentUnixTime::from_nanos(1_000_000),
        });
        let scheduler = TimerScheduler::with_clock(clock.clone());
        let fence = DocumentProducerFence::default();
        let microtask_checkpoint = PendingMicrotaskCheckpoint::new(1);
        let producer_checkpoint = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let rendering = PendingRenderingObservation::new(
            None,
            false,
            authority
                .target()
                .fully_active_pipelines()
                .iter()
                .map(|pipeline_id| PendingPipelineRenderingObservation {
                    pipeline_id: *pipeline_id,
                    activity: PendingRenderingPipelineActivity::FullyActive,
                    render_blocking_elements: 0,
                    retained_animation_frame_callbacks: 0,
                    runnable_animation_frame_callbacks: 0,
                    document_update_required: false,
                    pending_animation_events: 0,
                    finite_animations: 0,
                    infinite_animations: 0,
                    unsupported_animations: 0,
                    animated_images: PendingAnimatedImageObservation::default(),
                    canvas: PendingCanvasObservation::default(),
                    pending_fonts: 0,
                    pending_images: 0,
                })
                .collect(),
        )
        .unwrap();
        let pending = RawPendingSnapshot {
            target: authority.target().clone(),
            state_generation: RuntimeStateGeneration::new(state_generation),
            dom_epoch: DomEpoch::new(state_generation),
            clock: PendingClockObservation {
                clock_id: clock.id(),
                mode: PendingClockMode::Controlled,
                now: clock.now(),
                unsupported_surface: None,
            },
            scheduler: PendingSchedulerObservation {
                scheduler_id: scheduler.id(),
                next_deadline: None,
            },
            input: PendingInputObservation::default(),
            microtasks: PendingMicrotaskObservation {
                event_loop_id: authority.target().event_loop_id,
                queued: 0,
                completed_checkpoint: microtask_checkpoint,
                checkpoint_in_progress: false,
                terminal: None,
            },
            execution: Some(DocumentExecutionObservation {
                clock_id: clock.id(),
                limits: DocumentExecutionLimits::CONTROLLED_WEBAPP_V1,
                counters: DocumentExecutionCounters::default(),
                terminal: None,
            }),
            producers: PendingProducerObservation::new(
                authority.target().event_loop_id,
                microtask_checkpoint,
                producer_checkpoint,
                fence.snapshot(),
                PendingProducerStability::FirstEmpty,
                None,
            )
            .unwrap(),
            parser: PendingParserObservation::default(),
            network: PendingNetworkObservation::default(),
            logical_timers: PendingLogicalTimerSnapshot::default(),
            rendering,
            sources: PendingSourceSnapshot::default(),
            terminals: PendingRuntimeTerminals::default(),
        };
        pending.validate().unwrap();
        pending
    }

    fn observed_outcome(
        pending: servo::document_pending::RawPendingSnapshot,
    ) -> DocumentControlReceiveOutcome {
        observed_outcome_with_advance_token(pending, None)
    }

    fn observed_outcome_with_advance_token(
        pending: servo::document_pending::RawPendingSnapshot,
        advance_token: Option<servo::document_control::DocumentAdvanceToken>,
    ) -> DocumentControlReceiveOutcome {
        DocumentControlReceiveOutcome::CommandOutcome(DocumentControlOutcome::Completed(Box::new(
            servo::document_control::DocumentControlObservation::new_internal(
                DocumentControlAction::Observed,
                Box::new(pending),
                advance_token,
            )
            .unwrap(),
        )))
    }

    fn pending_with_finite_timer_for_authority(
        authority: &SessionNavigationAuthority,
        state_generation: u64,
    ) -> (
        servo::document_pending::RawPendingSnapshot,
        servo::document_control::DocumentAdvanceToken,
    ) {
        use embedder_traits::document_control::DocumentAdvanceTokenId;
        use embedder_traits::document_pending::{
            PendingClockMode, PendingClockObservation, PendingLogicalTimerKind,
            PendingLogicalTimerObservation, PendingLogicalTimerSnapshot,
            PendingLogicalTimerStableId, PendingMicrotaskCheckpoint, PendingProducerObservation,
            PendingProducerPriorEmptyQualification, PendingProducerStability,
            PendingSchedulerObservation, PendingSourceDisposition, PendingSourceEpoch,
            PendingSourceId, PendingSourceKind, PendingSourceObservation, PendingSourceSnapshot,
        };
        use timers::{
            DocumentClock, DocumentClockConfiguration, DocumentProducerCheckpoint,
            DocumentProducerFence, DocumentUnixTime, TimerEventRequest, TimerScheduler,
        };

        let mut pending = pending_for_authority(authority, state_generation);
        let clock = DocumentClock::new(DocumentClockConfiguration::Controlled {
            initial_time_ns: 5,
            unix_time_origin_ns: DocumentUnixTime::from_nanos(1_000_000),
        });
        let mut scheduler = TimerScheduler::with_clock(clock.clone());
        scheduler.schedule_timer(TimerEventRequest {
            callback: Box::new(|| {}),
            duration: Duration::from_nanos(20),
        });
        let deadline = scheduler
            .finite_deadline_snapshot()
            .unwrap()
            .expect("the test scheduler has one finite deadline");
        pending.clock = PendingClockObservation {
            clock_id: clock.id(),
            mode: PendingClockMode::Controlled,
            now: clock.now(),
            unsupported_surface: None,
        };
        pending.scheduler = PendingSchedulerObservation {
            scheduler_id: scheduler.id(),
            next_deadline: Some(deadline),
        };
        pending
            .execution
            .as_mut()
            .expect("the test pending snapshot has execution authority")
            .clock_id = clock.id();

        let fence = DocumentProducerFence::default();
        let fence_snapshot = fence.snapshot();
        let prior_checkpoint = DocumentProducerCheckpoint::ZERO.checked_next().unwrap();
        let checkpoint = prior_checkpoint.checked_next().unwrap();
        let microtask_checkpoint = PendingMicrotaskCheckpoint::new(2);
        pending.microtasks.completed_checkpoint = microtask_checkpoint;
        pending.producers = PendingProducerObservation::new(
            authority.target().event_loop_id,
            microtask_checkpoint,
            checkpoint,
            fence_snapshot,
            PendingProducerStability::StableEmpty,
            Some(PendingProducerPriorEmptyQualification {
                microtask_checkpoint: PendingMicrotaskCheckpoint::new(1),
                checkpoint: prior_checkpoint,
                snapshot_revision: fence_snapshot.revision(),
            }),
        )
        .unwrap();

        let source_id = PendingSourceId::new(1);
        let pipeline_id = authority
            .target()
            .active_top_level
            .expect("the test authority has an active document")
            .pipeline_id;
        pending.sources = PendingSourceSnapshot::new(
            PendingSourceEpoch::new(1),
            vec![PendingSourceObservation {
                id: source_id,
                kind: PendingSourceKind::Timer,
                disposition: PendingSourceDisposition::FiniteDeadline(deadline.deadline),
            }],
        )
        .unwrap();
        pending.logical_timers =
            PendingLogicalTimerSnapshot::new(vec![PendingLogicalTimerObservation {
                source_id,
                pipeline_id,
                stable_id: PendingLogicalTimerStableId::JavaScriptHandle(1),
                creation_sequence: 1,
                kind: PendingLogicalTimerKind::JavaScriptOneShot,
                logical_deadline: deadline.deadline,
                suspended: false,
                eligible_in_controlled_turn: true,
                is_ordering_head: true,
                delivery_ready: false,
                outer_wake: Some(deadline),
            }])
            .unwrap();
        pending.validate().unwrap();
        let token = servo::document_control::DocumentAdvanceToken::new_internal(
            DocumentAdvanceTokenId::new(17),
            &pending,
        )
        .unwrap();
        (pending, token)
    }

    fn advance_settle_active(
        source: &SessionNavigationAuthority,
    ) -> (
        ActiveRequest,
        servo::document_pending::RawPendingSnapshot,
        servo::document_control::DocumentAdvanceToken,
    ) {
        let effective_policy = default_resolved_settle_policy();
        let (pending, token) = pending_with_finite_timer_for_authority(source, 7);
        let mut coordinator = settle::SettleCoordinator::new(effective_policy.engine);
        assert_eq!(
            coordinator.start(),
            Ok(settle::SettleProgress::Command(
                DocumentControlCommand::Observe
            ))
        );
        assert_eq!(
            coordinator.consume_receive_outcome(
                observed_outcome_with_advance_token(pending.clone(), Some(token.clone())),
                Duration::ZERO,
            ),
            Ok(settle::SettleProgress::Command(
                DocumentControlCommand::AdvanceTo(Box::new(token.clone()))
            ))
        );
        let active = ActiveRequest {
            request: request("runtime.settle", Some(SESSION_ID)),
            profile: Some(SessionProfile::ControlledWebSessionV1),
            operation: ActiveOperation::Settle(SettleState {
                profile: SessionProfile::ControlledWebSessionV1,
                authorizing_document_state: None,
                authorizing_observation: None,
                authorizing_navigation: Some(source.clone()),
                replacement: None,
                authority_bound_command: None,
                latest_pending_target: Some(Box::new(source.target().clone())),
                response: SettleResponse::Runtime,
                coordinator,
                effective_policy,
                cumulative_external_io_wall_time: Duration::ZERO,
                waiting: None,
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::None,
        };
        (active, pending, token)
    }

    fn token_authorizing_settle_active(
        authority: &SessionNavigationAuthority,
        pending: &servo::document_pending::RawPendingSnapshot,
        projection: &mut wire::WireProjectionContext,
    ) -> ActiveRequest {
        let token = projection.document_state_token(pending, authority).unwrap();
        let current = projection
            .current_document_state_authority(&token)
            .expect("the just-issued token has current private authority");
        let effective_policy = default_resolved_settle_policy();
        ActiveRequest {
            request: request("runtime.settle", Some(SESSION_ID)),
            profile: Some(SessionProfile::ControlledWebSessionV1),
            operation: ActiveOperation::Settle(SettleState {
                profile: SessionProfile::ControlledWebSessionV1,
                authorizing_document_state: Some(current),
                authorizing_observation: None,
                authorizing_navigation: None,
                replacement: None,
                authority_bound_command: None,
                latest_pending_target: None,
                response: SettleResponse::Runtime,
                coordinator: settle::SettleCoordinator::new(effective_policy.engine),
                effective_policy,
                cumulative_external_io_wall_time: Duration::ZERO,
                waiting: None,
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::None,
        }
    }

    fn indeterminate_drive_settle_active(source: &SessionNavigationAuthority) -> ActiveRequest {
        let effective_policy = default_resolved_settle_policy();
        let mut coordinator = settle::SettleCoordinator::new(effective_policy.engine);
        assert_eq!(
            coordinator.start(),
            Ok(settle::SettleProgress::Command(
                DocumentControlCommand::Observe
            ))
        );
        assert_eq!(
            coordinator.consume_receive_outcome(
                observed_outcome(pending_for_authority(source, 7)),
                Duration::ZERO,
            ),
            Ok(settle::SettleProgress::Command(
                DocumentControlCommand::DriveOneTurn
            ))
        );
        ActiveRequest {
            request: request("runtime.settle", Some(SESSION_ID)),
            profile: Some(SessionProfile::ControlledWebSessionV1),
            operation: ActiveOperation::Settle(SettleState {
                profile: SessionProfile::ControlledWebSessionV1,
                authorizing_document_state: None,
                authorizing_observation: None,
                authorizing_navigation: Some(source.clone()),
                replacement: Some(SettleReplacementPhase::AwaitingAdmission {
                    source: source.clone(),
                    drive_outcome: DocumentControlReceiveOutcome::CommandOutcome(
                        DocumentControlOutcome::DriveOneTurnOutcomeIndeterminate {
                            target: Box::new(source.target().clone()),
                        },
                    ),
                }),
                authority_bound_command: None,
                latest_pending_target: Some(Box::new(source.target().clone())),
                response: SettleResponse::Runtime,
                coordinator,
                effective_policy,
                cumulative_external_io_wall_time: Duration::ZERO,
                waiting: None,
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::Partial,
        }
    }

    fn turn_processed_outcome(
        pending: servo::document_pending::RawPendingSnapshot,
    ) -> DocumentControlReceiveOutcome {
        DocumentControlReceiveOutcome::CommandOutcome(DocumentControlOutcome::Completed(Box::new(
            servo::document_control::DocumentControlObservation::new_internal(
                DocumentControlAction::TurnProcessed {
                    microtask_checkpoint_advanced: false,
                },
                Box::new(pending),
                None,
            )
            .unwrap(),
        )))
    }

    fn shell<'a>(
        output: &'a mut Vec<u8>,
        state: ShellState,
        engine: Option<FakeEngine>,
    ) -> Shell<&'a mut Vec<u8>, FakeEngine> {
        let (_sender, inbox) = reader_channel(2);
        let waker = ShellWaker::default();
        let cursor = waker.snapshot_checked().unwrap();
        let profile = engine.as_ref().and_then(|engine| {
            match (
                engine.document_control_profile,
                engine.document_execution_profile,
            ) {
                (
                    DocumentControlProfile::TopLevelSession,
                    DocumentExecutionProfile::ControlledWebSessionV2,
                ) => Some(SessionProfile::ControlledWebSessionV2),
                (DocumentControlProfile::TopLevelSession, DocumentExecutionProfile::Baseline) => {
                    Some(SessionProfile::ControlledWebSessionV1)
                },
                _ if engine.clock_mode.is_controlled() => Some(SessionProfile::ControlledWebappV1),
                _ => None,
            }
        });
        Shell {
            state,
            engine,
            inbox,
            waker,
            wake_cursor: cursor,
            servo_cursor: cursor,
            writer: ProtocolWriter::new(output),
            active: None,
            projection: wire::WireProjectionContext::new_with_namespace_internal(
                stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x71; 16]),
            ),
            profile,
            last_navigation_authority: None,
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
    fn initialize_advertises_profiles_in_append_only_order_and_session_methods() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(&mut bytes, ShellState::Spawned, None);
            assert!(!shell.handle(request("protocol.initialize", None)).unwrap());
        }
        let response = frames(&bytes).pop().unwrap();
        assert_eq!(
            response["result"]["capabilities"]["profiles"],
            json!([
                CONTROLLED_WEBAPP_V1_PROFILE,
                CONTROLLED_WEB_SESSION_V1_PROFILE,
                CONTROLLED_WEB_SESSION_V2_PROFILE,
            ])
        );
        let methods = response["result"]["capabilities"]["methods"]
            .as_array()
            .unwrap();
        for method in [
            "session.navigate",
            "session.cookies.get",
            "session.cookies.set",
            "session.storage.get",
            "session.storage.set",
            "session.state.export",
            "session.state.import",
            "session.requests",
            "session.evidence",
        ] {
            assert!(
                methods.iter().any(|value| value.as_str() == Some(method)),
                "initialize omitted {method}",
            );
        }
    }

    #[test]
    fn named_session_profiles_select_exact_native_execution_policy() {
        assert_eq!(
            SessionProfile::ControlledWebSessionV1.document_control_profile(),
            DocumentControlProfile::TopLevelSession,
        );
        assert_eq!(
            SessionProfile::ControlledWebSessionV1.document_execution_profile(),
            DocumentExecutionProfile::Baseline,
        );
        assert_eq!(
            SessionProfile::ControlledWebSessionV2.document_control_profile(),
            DocumentControlProfile::TopLevelSession,
        );
        assert_eq!(
            SessionProfile::ControlledWebSessionV2.document_execution_profile(),
            DocumentExecutionProfile::ControlledWebSessionV2,
        );

        let mut output = Vec::new();
        let shell = shell(
            &mut output,
            ShellState::Open,
            Some(FakeEngine::controlled_session_v2()),
        );
        assert_eq!(shell.profile, Some(SessionProfile::ControlledWebSessionV2));
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
                profile: Some(SessionProfile::ControlledWebappV1),
                operation: ActiveOperation::Pending,
                started_at: Instant::now(),
                in_flight: None,
                control_turn_observed: None,
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
                profile: Some(SessionProfile::ControlledWebappV1),
                operation: ActiveOperation::Pending,
                started_at: Instant::now(),
                in_flight: None,
                control_turn_observed: None,
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
                profile: Some(SessionProfile::ControlledWebappV1),
                operation: ActiveOperation::Pending,
                started_at: Instant::now(),
                in_flight: None,
                control_turn_observed: None,
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
    fn controlled_session_cancellation_is_indeterminate_and_fail_stop() {
        for state_effect in [RequestStateEffect::None, RequestStateEffect::Partial] {
            let mut bytes = Vec::new();
            let mut shell = shell(
                &mut bytes,
                ShellState::Open,
                Some(FakeEngine::controlled_session()),
            );
            let active = ActiveRequest {
                request: request("runtime.settle", Some(SESSION_ID)),
                profile: Some(SessionProfile::ControlledWebSessionV1),
                operation: ActiveOperation::Pending,
                started_at: Instant::now(),
                in_flight: None,
                control_turn_observed: None,
                needs_initial_pump: false,
                state_effect,
            };

            let failure = shell.cancel_active_failure(&active);
            assert!(failure.fail_stop);
            assert!(failure.error.fatal);
            assert_eq!(failure.error.code, "outcome_indeterminate");
            assert_eq!(failure.error.state_effect, "indeterminate");
        }
    }

    #[test]
    fn close_terminalizes_active_work_before_its_final_response() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(&mut bytes, ShellState::Open, Some(FakeEngine::controlled()));
            shell.active = Some(ActiveRequest {
                request: request_with_id("runtime.settle", "active", Some(SESSION_ID)),
                profile: Some(SessionProfile::ControlledWebappV1),
                operation: ActiveOperation::Pending,
                started_at: Instant::now(),
                in_flight: None,
                control_turn_observed: None,
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
        let controlled = controlled.configuration().unwrap();
        assert_eq!(
            controlled.clock_mode,
            EngineClockMode::Controlled {
                initial_time_ns: 42
            }
        );
        assert_eq!(controlled.boundary, "controlled_ready");
        assert_eq!(controlled.profile, Some(SessionProfile::ControlledWebappV1));
        assert!(controlled.state.is_none());
        assert!(controlled.network.is_none());

        let session: OpenParams = serde_json::from_value(json!({
            "url": "about:blank",
            "clockMode": "controlled",
            "profile": CONTROLLED_WEB_SESSION_V1_PROFILE,
            "initialVirtualTimeNs": "42",
            "unixTimeOriginNs": "0"
        }))
        .unwrap();
        assert_eq!(
            session.configuration().unwrap().profile,
            Some(SessionProfile::ControlledWebSessionV1)
        );

        let session_v2: OpenParams = serde_json::from_value(json!({
            "url": "about:blank",
            "clockMode": "controlled",
            "profile": CONTROLLED_WEB_SESSION_V2_PROFILE,
            "initialVirtualTimeNs": "42",
            "unixTimeOriginNs": "0"
        }))
        .unwrap();
        assert_eq!(
            session_v2.configuration().unwrap().profile,
            Some(SessionProfile::ControlledWebSessionV2)
        );

        let unsupported: OpenParams = serde_json::from_value(json!({
            "url": "about:blank",
            "clockMode": "controlled",
            "profile": CONTROLLED_WEBAPP_V1_PROFILE,
            "unixTimeOriginNs": "1"
        }))
        .unwrap();
        assert_eq!(
            unsupported.configuration().err().unwrap().code,
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
            assert_eq!(
                params.configuration().err().unwrap().code,
                "invalid_request"
            );
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
                    readiness_waiting: Some(_),
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
    fn controlled_open_wall_deadline_survives_the_settle_phase() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(
                &mut bytes,
                ShellState::Open,
                Some(FakeEngine::controlled_session_v2()),
            );
            let profile = SessionProfile::ControlledWebSessionV2;
            let deadline = Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap();
            let effective_policy = default_resolved_settle_policy();
            shell.active = Some(ActiveRequest {
                request: request("session.open", None),
                profile: Some(profile),
                operation: ActiveOperation::Settle(SettleState {
                    profile,
                    authorizing_document_state: None,
                    authorizing_observation: None,
                    authorizing_navigation: None,
                    replacement: None,
                    authority_bound_command: None,
                    latest_pending_target: None,
                    response: SettleResponse::ControlledOpen {
                        requested_url: Url::parse("https://example.test/").unwrap(),
                        current_url: Url::parse("https://example.test/").unwrap(),
                        profile,
                        deadline,
                        bootstrap_attempted: false,
                    },
                    coordinator: settle::SettleCoordinator::new(effective_policy.engine),
                    effective_policy,
                    cumulative_external_io_wall_time: Duration::ZERO,
                    waiting: None,
                }),
                started_at: deadline,
                in_flight: None,
                control_turn_observed: None,
                needs_initial_pump: false,
                state_effect: RequestStateEffect::None,
            });

            assert_eq!(shell.next_wait_deadline(None, Instant::now()), deadline,);
            assert!(
                shell
                    .service_controlled_open_deadline(Instant::now())
                    .unwrap_err()
                    .contains("fail-stopped")
            );
            assert!(shell.active.is_none());
            assert!(shell.engine.is_none());
            assert_eq!(shell.state, ShellState::Closed);
        }

        let response = frames(&bytes).pop().unwrap();
        assert_eq!(response["error"]["code"], "controlled_open_timeout");
        assert_eq!(response["error"]["fatal"], true);
        assert_eq!(response["error"]["stateEffect"], "indeterminate");
        assert!(response["sessionId"].is_null());
    }

    #[test]
    fn controlled_open_wall_deadline_survives_session_projection() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(
                &mut bytes,
                ShellState::Open,
                Some(FakeEngine::controlled_session_v2()),
            );
            let profile = SessionProfile::ControlledWebSessionV2;
            let deadline = Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap();
            let navigation = session_authority(1, 1, 0, 0);
            shell.active = Some(ActiveRequest {
                request: request("session.open", None),
                profile: Some(profile),
                operation: ActiveOperation::SessionProjection(SessionProjectionState {
                    pending: Box::new(pending_for_authority(&navigation, 1)),
                    kind: SessionProjectionKind::ControlledOpen {
                        requested_url: Url::parse("https://example.test/").unwrap(),
                        current_url: Url::parse("https://example.test/").unwrap(),
                        profile,
                        deadline,
                        bootstrap_attempted: false,
                        cumulative_external_io_wall_time: Duration::ZERO,
                        session_state_token: None,
                        settle_resume: None,
                    },
                    phase: SessionProjectionPhase::AwaitingInitialNavigation,
                }),
                started_at: deadline,
                in_flight: None,
                control_turn_observed: None,
                needs_initial_pump: false,
                state_effect: RequestStateEffect::None,
            });

            assert_eq!(shell.next_wait_deadline(None, Instant::now()), deadline,);
            assert!(
                shell
                    .service_controlled_open_deadline(Instant::now())
                    .unwrap_err()
                    .contains("fail-stopped")
            );
            assert!(shell.active.is_none());
            assert!(shell.engine.is_none());
            assert_eq!(shell.state, ShellState::Closed);
        }

        let response = frames(&bytes).pop().unwrap();
        assert_eq!(response["error"]["code"], "controlled_open_timeout");
        assert_eq!(response["error"]["fatal"], true);
        assert_eq!(response["error"]["stateEffect"], "indeterminate");
        assert!(response["sessionId"].is_null());
    }

    #[test]
    fn settlement_wait_retains_wake_from_in_flight_control_turn() {
        let mut bytes = Vec::new();
        let mut shell = shell(&mut bytes, ShellState::Open, Some(FakeEngine::controlled()));
        let mut active = ActiveRequest {
            request: request("runtime.settle", Some(SESSION_ID)),
            profile: Some(SessionProfile::ControlledWebappV1),
            operation: ActiveOperation::Pending,
            started_at: Instant::now(),
            in_flight: Some(DocumentControlCommand::Observe),
            control_turn_observed: Some(shell.checked_wake_snapshot().unwrap()),
            needs_initial_pump: true,
            state_effect: RequestStateEffect::None,
        };
        let before_submit = active.control_turn_observed.unwrap();

        // A producer commits after submission but before the forced control pump. That pump may
        // advance the global cursor before the typed Observe response becomes pollable.
        servo::EventLoopWaker::wake(&shell.waker);
        let producer_wake = shell.checked_wake_snapshot().unwrap();
        shell.servo_cursor = producer_wake;
        active.needs_initial_pump = false;

        let wait = active.settle_host_wait(shell.servo_cursor, Instant::now(), None);
        assert_eq!(wait.observed, before_submit);
        assert!(producer_wake.servo_changed_since(wait.observed));

        // The per-command baseline is single-use. A later wait without a new control turn starts
        // from the current cursor and cannot replay this old producer wake.
        let next_wait = active.settle_host_wait(shell.servo_cursor, Instant::now(), None);
        assert_eq!(next_wait.observed, producer_wake);
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
                document_control_profile: DocumentControlProfile::SingleDocument,
                document_execution_profile: DocumentExecutionProfile::Baseline,
                pump_calls: 0,
                cancel_calls: 0,
                navigation_cancel_calls: 0,
                close_calls: 0,
                submitted: Vec::new(),
                polls: VecDeque::new(),
                navigation_submitted: Vec::new(),
                navigation_polls: VecDeque::new(),
                network_snapshot: None,
                network_virtual_times: RefCell::new(Vec::new()),
                control_events: Rc::new(RefCell::new(Vec::new())),
                evidence: Rc::new(RefCell::new(Vec::new())),
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
    fn stale_settle_token_is_preflighted_without_submitting_or_pumping_engine_work() {
        fn stale_settle_request() -> Request {
            let mut request = request("runtime.settle", Some(SESSION_ID));
            request.params = json!({
                "expectedStateToken": test_document_token(99),
            });
            request
        }

        let mut clean_bytes = Vec::new();
        {
            let mut shell = shell(
                &mut clean_bytes,
                ShellState::Open,
                Some(FakeEngine::controlled_session()),
            );
            assert!(!shell.handle(stale_settle_request()).unwrap());
            assert!(shell.active.is_none());
            let engine = shell.engine.as_ref().unwrap();
            assert!(engine.submitted.is_empty());
            assert!(engine.navigation_submitted.is_empty());
            assert_eq!(engine.pump_calls, 0);
        }
        let clean = frames(&clean_bytes).pop().unwrap();
        assert_eq!(clean["error"]["code"], "stale_state_token");
        assert_eq!(clean["error"]["stateEffect"], "none");
        assert_eq!(clean["error"]["fatal"], false);

        let mut sticky_bytes = Vec::new();
        {
            let mut engine = FakeEngine::controlled_session();
            engine.network_snapshot = Some(ControlledNetworkSnapshot {
                active_operations: 0,
                maximum_active_operations: 8,
                sticky_failure: Some(ControlledNetworkFailure::FixtureMiss),
                current_virtual_time_ns: "0".into(),
            });
            let mut shell = shell(&mut sticky_bytes, ShellState::Open, Some(engine));
            assert!(!shell.handle(stale_settle_request()).unwrap());
            assert!(shell.active.is_none());
            let engine = shell.engine.as_ref().unwrap();
            assert!(engine.submitted.is_empty());
            assert!(engine.navigation_submitted.is_empty());
            assert_eq!(engine.pump_calls, 0);
        }
        let sticky = frames(&sticky_bytes).pop().unwrap();
        assert_eq!(sticky["error"]["code"], "network_fixture_miss");
        assert_eq!(sticky["error"]["stateEffect"], "partial");
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
    fn controlled_session_document_mutations_observe_navigation_authority_first() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(
                &mut bytes,
                ShellState::Open,
                Some(FakeEngine::controlled_session()),
            );
            let mut fill = request("action.fill", Some(SESSION_ID));
            fill.params = json!({
                "selector": "#email",
                "value": "person@example.test",
                "expectedStateToken": test_document_token(1),
            });

            assert!(!shell.handle(fill).unwrap());
            let engine = shell.engine.as_ref().unwrap();
            assert!(engine.submitted.is_empty());
            assert_eq!(
                engine.navigation_submitted,
                vec![NavigationOperationKind::Observe]
            );
        }
        assert!(frames(&bytes).is_empty());
    }

    #[test]
    fn session_navigate_validates_http_and_starts_with_passive_authority() {
        let mut bytes = Vec::new();
        {
            let mut shell = shell(
                &mut bytes,
                ShellState::Open,
                Some(FakeEngine::controlled_session()),
            );
            let mut navigate = request("session.navigate", Some(SESSION_ID));
            navigate.params = json!({
                "url": "https://example.test/next",
                "expectedStateToken": test_document_token(1),
            });
            assert!(!shell.handle(navigate).unwrap());
            assert_eq!(
                shell.engine.as_ref().unwrap().navigation_submitted,
                vec![NavigationOperationKind::Observe]
            );
            assert!(matches!(
                shell.active.as_ref().map(|active| &active.operation),
                Some(ActiveOperation::Navigate(NavigateState {
                    phase: NavigatePhase::AwaitingAuthority { .. },
                    ..
                }))
            ));
        }
        assert!(frames(&bytes).is_empty());

        let mut invalid_bytes = Vec::new();
        {
            let mut shell = shell(
                &mut invalid_bytes,
                ShellState::Open,
                Some(FakeEngine::controlled_session()),
            );
            let mut navigate = request("session.navigate", Some(SESSION_ID));
            navigate.params = json!({
                "url": "file:///tmp/not-supported",
                "expectedStateToken": test_document_token(1),
            });
            assert!(!shell.handle(navigate).unwrap());
            assert!(
                shell
                    .engine
                    .as_ref()
                    .unwrap()
                    .navigation_submitted
                    .is_empty()
            );
        }
        assert_eq!(
            frames(&invalid_bytes)[0]["error"]["code"],
            "unsupported_navigation_scheme"
        );
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
                        ..
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
    fn automation_rejections_never_echo_selector_or_select_values() {
        let secret_selector = "#SECRET-SELECTOR-CANARY";
        let secret_value = "SECRET-OPTION-CANARY";
        for error in [
            DocumentAutomationError::InvalidSelector {
                selector: secret_selector.into(),
            },
            DocumentAutomationError::SelectValueNotFound {
                selector: secret_selector.into(),
                value: secret_value.into(),
            },
        ] {
            let protocol = automation_rejection(
                DocumentControlError::Automation(error),
                RequestStateEffect::None,
            );
            assert!(!protocol.message.contains(secret_selector));
            assert!(!protocol.message.contains(secret_value));
            assert_eq!(protocol.message, "document automation request was rejected");
        }

        let mut malformed = request("dom.extract", Some(SESSION_ID));
        malformed.params = json!({
            "rootSelector": secret_selector,
            "fields": [{
                "name": "value",
                "selector": secret_selector,
                "read": secret_value,
            }],
            "expectedGeneration": "1",
        });
        let parse_error = parse_params::<wire::DomExtractParams>(&malformed)
            .err()
            .unwrap();
        assert!(!parse_error.message.contains(secret_selector));
        assert!(!parse_error.message.contains(secret_value));
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

    #[test]
    fn sensitive_parameter_errors_are_fixed_and_redacted() {
        let secret = "SECRET-CANARY-DO-NOT-ECHO";
        let mut open = request("session.open", None);
        open.params = json!({
            "url": "https://example.test/",
            "clockMode": secret,
        });
        let open_error =
            parse_sensitive_params::<OpenParams>(&open, "invalid session.open parameters")
                .err()
                .unwrap();
        assert_eq!(open_error.message, "invalid session.open parameters");
        assert!(!open_error.message.contains(secret));

        let mut cookies = request("session.cookies.set", Some(SESSION_ID));
        cookies.params = json!({
            "expectedSessionStateToken": test_session_state_token(1),
            "cookies": [{"sameSite": secret}],
        });
        let cookie_error = parse_sensitive_params::<SessionCookiesSetParamsV1>(
            &cookies,
            "invalid session cookie parameters",
        )
        .err()
        .unwrap();
        assert_eq!(cookie_error.message, "invalid session cookie parameters");
        assert!(!cookie_error.message.contains(secret));

        let mut storage = request("session.storage.set", Some(SESSION_ID));
        storage.params = json!({
            "expectedSessionStateToken": secret,
            "origins": [],
        });
        let storage_error = parse_sensitive_params::<SessionStorageSetParamsV1>(
            &storage,
            "invalid session storage parameters",
        )
        .err()
        .unwrap();
        assert_eq!(storage_error.message, "invalid session storage parameters");
        assert!(!storage_error.message.contains(secret));
    }

    #[test]
    fn token_domains_cannot_substitute_at_public_endpoints() {
        let mut document_request = request("runtime.pending", Some(SESSION_ID));
        document_request.params = json!({"expectedStateToken": test_session_state_token(1)});
        let document_error =
            parse_params::<wire::SessionRuntimeAdvanceToNextParams>(&document_request)
                .err()
                .unwrap();
        assert_eq!(document_error.code, "stale_state_token");
        assert_eq!(document_error.state_effect, "none");

        let mut session_request = request("session.cookies.set", Some(SESSION_ID));
        session_request.params = json!({
            "expectedSessionStateToken": test_document_token(1),
            "cookies": [],
        });
        let session_error = parse_sensitive_params::<SessionCookiesSetParamsV1>(
            &session_request,
            "invalid session cookie parameters",
        )
        .err()
        .unwrap();
        assert_eq!(session_error.code, "stale_session_state_token");
        assert_eq!(session_error.state_effect, "none");

        let mut malformed_document = request("runtime.pending", Some(SESSION_ID));
        malformed_document.params = json!({"expectedStateToken": "session:not-canonical"});
        let malformed_document_error =
            parse_params::<wire::SessionRuntimeAdvanceToNextParams>(&malformed_document)
                .err()
                .unwrap();
        assert_eq!(malformed_document_error.code, "invalid_request");

        let mut malformed_session = request("session.cookies.set", Some(SESSION_ID));
        malformed_session.params = json!({
            "expectedSessionStateToken": "document:not-canonical",
            "cookies": [],
        });
        let malformed_session_error = parse_sensitive_params::<SessionCookiesSetParamsV1>(
            &malformed_session,
            "invalid session cookie parameters",
        )
        .err()
        .unwrap();
        assert_eq!(malformed_session_error.code, "invalid_request");
    }

    #[test]
    fn published_state_import_is_phase_closed_before_secret_inspection() {
        let secret = "SECRET-IMPORT-CANARY";
        let mut bytes = Vec::new();
        {
            let mut shell = shell(
                &mut bytes,
                ShellState::Open,
                Some(FakeEngine::controlled_session()),
            );
            let mut import = request("session.state.import", Some(SESSION_ID));
            import.params = json!({
                "expectedSessionStateToken": test_document_token(1),
                "state": secret,
            });
            assert!(!shell.handle(import).unwrap());
        }
        let response = frames(&bytes).pop().unwrap();
        assert_eq!(
            response["error"]["code"],
            "session_state_import_phase_closed"
        );
        assert!(!response.to_string().contains(secret));
    }

    #[test]
    fn app_navigation_limits_preserve_effect_and_fail_stop_after_other_work() {
        let terminal = SessionNavigationTerminal::HistoryLimitExceeded {
            limit: 10_000,
            observed: 10_000,
            navigation_id: SessionNavigationId::new(7),
            history_revision: embedder_traits::document_session::HistoryRevision::new(10_000),
        };
        let ActiveTransition::Fail(pre_admission) =
            session_navigation_terminal_failure(terminal, RequestStateEffect::None)
        else {
            panic!("terminal navigation must fail");
        };
        assert!(!pre_admission.fail_stop);
        assert!(!pre_admission.error.fatal);
        assert_eq!(pre_admission.error.state_effect, "none");
        assert!(pre_admission.error.details.is_some());

        let ActiveTransition::Fail(post_action) =
            session_navigation_terminal_failure(terminal, RequestStateEffect::Partial)
        else {
            panic!("terminal navigation must fail");
        };
        assert!(post_action.fail_stop);
        assert!(post_action.error.fatal);
        assert_eq!(post_action.error.state_effect, "partial");
    }

    #[test]
    fn projection_authority_mismatch_fail_stops_after_partial_work_only() {
        let mismatch = wire::DocumentStateAuthorityError::NavigationTargetDoesNotMatchPending;
        let ActiveTransition::Fail(before_work) =
            mismatched_navigation_authority(mismatch.clone(), RequestStateEffect::None)
        else {
            panic!("authority mismatch must fail");
        };
        assert!(!before_work.fail_stop);
        assert!(!before_work.error.fatal);
        assert_eq!(before_work.error.state_effect, "none");

        let ActiveTransition::Fail(after_work) =
            mismatched_navigation_authority(mismatch, RequestStateEffect::Partial)
        else {
            panic!("authority mismatch must fail");
        };
        assert!(after_work.fail_stop);
        assert!(after_work.error.fatal);
        assert_eq!(after_work.error.state_effect, "partial");
    }

    #[test]
    fn token_entropy_failures_are_secret_free_fatal_runtime_errors() {
        let ActiveTransition::Fail(document) = document_authority_authorization_failure(
            wire::DocumentStateAuthorityError::TokenEntropyUnavailable,
        ) else {
            panic!("document token entropy failure must fail");
        };
        assert!(document.fail_stop);
        assert!(document.error.fatal);
        assert_eq!(document.error.code, "runtime_error");
        assert_eq!(document.error.state_effect, "none");
        assert!(document.error.details.is_none());

        let session = session_state_protocol_error(SessionStateError::TokenEntropyUnavailable);
        assert!(session.fatal);
        assert_eq!(session.code, "session_state_token_entropy_unavailable");
        assert_eq!(session.state_effect, "none");
        assert!(session.details.is_none());

        let hardened = harden_session_state_mutation_error("session.storage.set", session);
        assert!(hardened.fatal);
        assert_eq!(hardened.state_effect, "indeterminate");
        assert!(hardened.details.is_none());
    }

    #[test]
    fn post_replace_session_state_failures_are_indeterminate_and_fatal() {
        for code in [
            "session_state_backend_observe_failed",
            "session_state_cookie_replace_failed",
            "session_state_web_storage_replace_failed",
            "session_state_backend_revision_regressed",
            "session_state_token_entropy_unavailable",
            "session_state_token_space_exhausted",
        ] {
            let hardened = harden_session_state_mutation_error(
                "session.cookies.set",
                ProtocolError::operation(code, "redacted", "none"),
            );
            assert!(hardened.fatal, "{code} was not fail-stopped");
            assert_eq!(hardened.state_effect, "indeterminate", "{code}");
        }
        let stale = harden_session_state_mutation_error(
            "session.storage.set",
            ProtocolError::operation("stale_session_state_token", "stale", "none"),
        );
        assert!(!stale.fatal);
        assert_eq!(stale.state_effect, "none");
    }

    #[test]
    fn registrable_host_cookie_overflow_has_a_distinct_pre_mutation_code() {
        let error =
            session_state_protocol_error(SessionStateError::TooManyCookiesPerRegistrableHost);
        assert_eq!(error.code, "too_many_session_cookies_per_registrable_host");
        assert!(!error.fatal);
        assert_eq!(error.state_effect, "none");
        assert!(error.details.is_none());
    }

    #[test]
    fn cookie_time_range_has_a_distinct_nonfatal_live_mutation_code() {
        let error =
            session_state_protocol_error(SessionStateError::CookieTimeRangeUnsupported);
        assert_eq!(error.code, "unsupported_cookie_time_range");
        assert!(!error.fatal);
        assert_eq!(error.state_effect, "none");
        assert!(error.details.is_none());

        let hardened = harden_session_state_mutation_error("session.cookies.set", error);
        assert!(!hardened.fatal);
        assert_eq!(hardened.state_effect, "none");
    }

    #[test]
    fn active_controlled_network_freezes_v2_virtual_advance() {
        assert!(controlled_network_blocks_virtual_advance(
            Some(SessionProfile::ControlledWebSessionV1),
            1,
        ));
        assert!(!controlled_network_blocks_virtual_advance(
            Some(SessionProfile::ControlledWebSessionV1),
            0,
        ));
        assert!(!controlled_network_blocks_virtual_advance(
            Some(SessionProfile::ControlledWebappV1),
            1,
        ));
    }

    #[test]
    fn active_controlled_network_latches_v2_document_replacement_only() {
        assert!(controlled_network_blocks_document_replacement(
            Some(SessionProfile::ControlledWebSessionV2),
            1,
        ));
        assert!(!controlled_network_blocks_document_replacement(
            Some(SessionProfile::ControlledWebSessionV2),
            0,
        ));
        assert!(!controlled_network_blocks_document_replacement(
            Some(SessionProfile::ControlledWebSessionV1),
            1,
        ));
        assert!(!controlled_network_blocks_document_replacement(
            Some(SessionProfile::ControlledWebappV1),
            1,
        ));
    }

    #[test]
    fn rejected_v2_advance_reobserves_without_staging_network_time() {
        let source = session_authority(1, 0, 0, 0);
        let (active, pending, token) = advance_settle_active(&source);
        let advance = DocumentControlCommand::AdvanceTo(Box::new(token));
        let mut bytes = Vec::new();
        let mut shell = shell(
            &mut bytes,
            ShellState::Open,
            Some(FakeEngine::controlled_session()),
        );

        shell
            .apply_active_transition(active, ActiveTransition::Submit(advance.clone()))
            .unwrap();
        let engine = shell.engine.as_ref().unwrap();
        assert_eq!(engine.submitted, vec![advance.clone()]);
        assert!(engine.network_virtual_times.borrow().is_empty());
        assert_eq!(
            *engine.control_events.borrow(),
            vec![FakeControlEvent::Submit(advance.clone())]
        );

        shell
            .engine
            .as_mut()
            .unwrap()
            .polls
            .push_back(EnginePortPoll::Complete(EnginePortCompletion {
                disposition: ControlOutcomeDisposition::DefinitiveFailure,
                outcome: DocumentControlReceiveOutcome::CommandOutcome(
                    DocumentControlOutcome::Rejected(DocumentControlError::AdvancePrecondition(
                        servo::document_control::DocumentAdvanceTokenInvariantError::StateGenerationChanged {
                            expected: pending.state_generation,
                            observed: servo::document_pending::RuntimeStateGeneration::new(8),
                        },
                    )),
                ),
            }));

        assert_eq!(shell.poll_active_control().unwrap(), (true, None));
        let engine = shell.engine.as_ref().unwrap();
        assert_eq!(
            engine.submitted,
            vec![advance.clone(), DocumentControlCommand::Observe]
        );
        assert!(engine.network_virtual_times.borrow().is_empty());
        assert_eq!(
            *engine.control_events.borrow(),
            vec![
                FakeControlEvent::Submit(advance),
                FakeControlEvent::Submit(DocumentControlCommand::Observe),
            ]
        );
        assert!(matches!(
            shell
                .active
                .as_ref()
                .and_then(|active| active.in_flight.as_ref()),
            Some(DocumentControlCommand::Observe)
        ));
    }

    #[test]
    fn completed_v2_advance_syncs_network_time_from_authoritative_observation_once() {
        let source = session_authority(1, 0, 0, 0);
        let (active, mut completed_pending, token) = advance_settle_active(&source);
        let deadline = token.deadline();
        let advance = DocumentControlCommand::AdvanceTo(Box::new(token));
        completed_pending.state_generation =
            servo::document_pending::RuntimeStateGeneration::new(8);
        completed_pending.clock.now = deadline.deadline;
        completed_pending.scheduler.next_deadline = None;
        completed_pending.logical_timers =
            servo::document_pending::PendingLogicalTimerSnapshot::default();
        completed_pending.sources = servo::document_pending::PendingSourceSnapshot::default();
        completed_pending.validate().unwrap();
        let outcome = DocumentControlReceiveOutcome::CommandOutcome(
            DocumentControlOutcome::Completed(Box::new(
                servo::document_control::DocumentControlObservation::new_internal(
                    DocumentControlAction::TimerActivated(deadline),
                    Box::new(completed_pending),
                    None,
                )
                .unwrap(),
            )),
        );
        let mut bytes = Vec::new();
        let mut shell = shell(
            &mut bytes,
            ShellState::Open,
            Some(FakeEngine::controlled_session()),
        );

        shell
            .apply_active_transition(active, ActiveTransition::Submit(advance.clone()))
            .unwrap();
        assert!(
            shell
                .engine
                .as_ref()
                .unwrap()
                .network_virtual_times
                .borrow()
                .is_empty()
        );
        shell
            .engine
            .as_mut()
            .unwrap()
            .polls
            .push_back(EnginePortPoll::Complete(EnginePortCompletion {
                disposition: ControlOutcomeDisposition::Completed,
                outcome,
            }));

        assert_eq!(shell.poll_active_control().unwrap(), (true, None));
        assert_eq!(
            *shell
                .engine
                .as_ref()
                .unwrap()
                .network_virtual_times
                .borrow(),
            vec![deadline.deadline.as_nanos()]
        );
        assert_eq!(
            *shell.engine.as_ref().unwrap().control_events.borrow(),
            vec![
                FakeControlEvent::Submit(advance.clone()),
                FakeControlEvent::NetworkTime(deadline.deadline.as_nanos()),
            ]
        );

        shell.engine.as_mut().unwrap().navigation_polls.push_back(
            EnginePortNavigationPoll::Complete(NavigationOperationCompletion::test_response(
                NavigationOperationKind::Observe,
                Ok(source),
            )),
        );
        assert_eq!(shell.poll_active_navigation().unwrap(), (true, None));
        assert_eq!(
            *shell.engine.as_ref().unwrap().control_events.borrow(),
            vec![
                FakeControlEvent::Submit(advance),
                FakeControlEvent::NetworkTime(deadline.deadline.as_nanos()),
                FakeControlEvent::Submit(DocumentControlCommand::DriveOneTurn),
            ]
        );
    }

    #[test]
    fn malformed_completed_v2_advance_fails_before_network_time_sync() {
        let source = session_authority(1, 0, 0, 0);
        let (active, mut malformed_pending, token) = advance_settle_active(&source);
        let deadline = token.deadline();
        let advance = DocumentControlCommand::AdvanceTo(Box::new(token));
        malformed_pending.state_generation =
            servo::document_pending::RuntimeStateGeneration::new(8);
        malformed_pending.scheduler.next_deadline = None;
        malformed_pending.logical_timers =
            servo::document_pending::PendingLogicalTimerSnapshot::default();
        malformed_pending.sources = servo::document_pending::PendingSourceSnapshot::default();
        malformed_pending.validate().unwrap();
        let outcome = DocumentControlReceiveOutcome::CommandOutcome(
            DocumentControlOutcome::Completed(Box::new(
                servo::document_control::DocumentControlObservation::new_internal(
                    DocumentControlAction::TimerActivated(deadline),
                    Box::new(malformed_pending),
                    None,
                )
                .unwrap(),
            )),
        );
        let mut bytes = Vec::new();
        let control_events;
        {
            let mut shell = shell(
                &mut bytes,
                ShellState::Open,
                Some(FakeEngine::controlled_session()),
            );
            shell
                .apply_active_transition(active, ActiveTransition::Submit(advance.clone()))
                .unwrap();
            control_events = shell.engine.as_ref().unwrap().control_events.clone();
            shell
                .engine
                .as_mut()
                .unwrap()
                .polls
                .push_back(EnginePortPoll::Complete(EnginePortCompletion {
                    disposition: ControlOutcomeDisposition::Completed,
                    outcome,
                }));

            assert_eq!(
                shell.poll_active_control().unwrap_err(),
                "runtime outcome is indeterminate; session was fail-stopped"
            );
            assert!(shell.active.is_none());
            assert!(shell.engine.is_none());
        }
        assert_eq!(
            *control_events.borrow(),
            vec![FakeControlEvent::Submit(advance)]
        );
        let response = frames(&bytes).pop().unwrap();
        assert_eq!(
            response["error"]["code"],
            "controlled_network_time_authority_diverged"
        );
        assert_eq!(response["error"]["stateEffect"], "indeterminate");
        assert_eq!(response["error"]["fatal"], true);
    }

    #[test]
    fn indeterminate_v2_advance_remains_fatal_without_sync_or_replay() {
        let source = session_authority(1, 0, 0, 0);
        let (active, _, token) = advance_settle_active(&source);
        let token_id = token.id();
        let target = Box::new(token.target().clone());
        let deadline = token.deadline();
        let advance = DocumentControlCommand::AdvanceTo(Box::new(token));
        let mut bytes = Vec::new();
        let control_events;
        {
            let mut shell = shell(
                &mut bytes,
                ShellState::Open,
                Some(FakeEngine::controlled_session()),
            );
            shell
                .apply_active_transition(active, ActiveTransition::Submit(advance.clone()))
                .unwrap();
            control_events = shell.engine.as_ref().unwrap().control_events.clone();
            shell
                .engine
                .as_mut()
                .unwrap()
                .polls
                .push_back(EnginePortPoll::Complete(EnginePortCompletion {
                    disposition: ControlOutcomeDisposition::Indeterminate,
                    outcome: DocumentControlReceiveOutcome::CommandOutcome(
                        DocumentControlOutcome::AdvanceOutcomeIndeterminate {
                            token_id,
                            target,
                            deadline,
                        },
                    ),
                }));

            assert_eq!(
                shell.poll_active_control().unwrap_err(),
                "runtime outcome is indeterminate; session was fail-stopped"
            );
            assert!(shell.active.is_none());
            assert!(shell.engine.is_none());
        }
        assert_eq!(
            *control_events.borrow(),
            vec![FakeControlEvent::Submit(advance)]
        );
        let response = frames(&bytes).pop().unwrap();
        assert_eq!(response["error"]["code"], "outcome_indeterminate");
        assert_eq!(response["error"]["stateEffect"], "indeterminate");
        assert_eq!(response["error"]["fatal"], true);
    }

    #[test]
    fn latest_settle_token_accepts_monotonic_generation_only_after_exact_n1_d_n2_bracket() {
        let source = session_authority(1, 0, 0, 0);
        let source_pending = pending_for_authority(&source, 7);
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x71; 16]),
        );
        let mut active = token_authorizing_settle_active(&source, &source_pending, &mut projection);

        assert!(active_operation_suppresses_servo_pump(&active.operation));
        assert!(!should_pump_servo(Some(&active), true, true));

        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(source.clone()),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::Submit(DocumentControlCommand::Observe)
        ));

        let mut progressed = source_pending.clone();
        progressed.state_generation = servo::document_pending::RuntimeStateGeneration::new(9);
        assert!(matches!(
            transition_from_control_completion(
                &mut active,
                DocumentControlCommand::Observe,
                observed_outcome(progressed),
                &mut projection,
                0,
            ),
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false
            }
        ));
        assert!(matches!(
            &active.operation,
            ActiveOperation::Settle(SettleState {
                authorizing_document_state: Some(_),
                authorizing_observation: Some(_),
                authorizing_navigation: Some(n1),
                ..
            }) if n1 == &source
        ));
        assert!(active_operation_suppresses_servo_pump(&active.operation));
        assert!(!should_pump_servo(Some(&active), true, true));
        let mut engine = FakeEngine::controlled_session();
        if should_pump_servo(Some(&active), true, true) {
            engine.pump();
        }
        assert_eq!(
            engine.pump_calls, 0,
            "an ambient wake pumped while D awaited N2"
        );

        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(source.clone()),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false
            }
        ));
        let ActiveOperation::Settle(state) = &active.operation else {
            panic!("token authorization lost settlement state");
        };
        assert!(state.authorizing_document_state.is_none());
        assert!(state.authorizing_observation.is_none());
        assert_eq!(state.authorizing_navigation.as_ref(), Some(&source));
        assert_eq!(
            state.authority_bound_command,
            Some(DocumentControlCommand::DriveOneTurn),
        );
        assert_eq!(
            state.latest_pending_target.as_deref(),
            Some(source.target())
        );
    }

    #[test]
    fn settle_token_bracket_rejects_n1_document_and_n2_authority_near_misses() {
        use embedder_traits::document_session::HistoryRevision;

        let source = session_authority(1, 0, 0, 0);
        let source_pending = pending_for_authority(&source, 7);
        let changed_url = SessionNavigationAuthority::new_internal(
            Box::new(source.target().clone()),
            source.document_epoch(),
            source.navigation_id(),
            source.history_revision(),
            source.successful_document_replacements(),
            servo::ServoUrl::parse("https://example.test/contradictory").unwrap(),
            None,
        );

        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x71; 16]),
        );
        let token = projection
            .document_state_token(&source_pending, &source)
            .unwrap();
        let mut active = token_authorizing_settle_active(&source, &source_pending, &mut projection);
        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(changed_url),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::RejectStaleStateToken
        ));
        assert!(
            !projection
                .authorizes_document_state(&source_pending, &source, &token)
                .unwrap(),
            "an N1 near miss must latch the resolved token strict-stale",
        );

        let admitted = replacement_admission_authority(&source);
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x72; 16]),
        );
        let token = projection
            .document_state_token(&source_pending, &source)
            .unwrap();
        let mut active = token_authorizing_settle_active(&source, &source_pending, &mut projection);
        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(source.clone()),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::Submit(DocumentControlCommand::Observe)
        ));
        assert!(matches!(
            transition_from_control_completion(
                &mut active,
                DocumentControlCommand::Observe,
                observed_outcome(pending_for_authority(&admitted, 8)),
                &mut projection,
                0,
            ),
            ActiveTransition::RejectStaleStateToken
        ));
        assert!(
            !projection
                .authorizes_document_state(&source_pending, &source, &token)
                .unwrap(),
            "a D target near miss must latch the resolved token strict-stale",
        );

        for changed_n2 in [
            SessionNavigationAuthority::new_internal(
                Box::new(source.target().clone()),
                source.document_epoch(),
                source.navigation_id(),
                HistoryRevision::new(source.history_revision().get() + 1),
                source.successful_document_replacements(),
                source.url().clone(),
                None,
            ),
            admitted.clone(),
        ] {
            let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
                stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x73; 16]),
            );
            let token = projection
                .document_state_token(&source_pending, &source)
                .unwrap();
            let mut active =
                token_authorizing_settle_active(&source, &source_pending, &mut projection);
            assert!(matches!(
                transition_from_navigation_completion(
                    &mut active,
                    NavigationOperationCompletion::test_response(
                        NavigationOperationKind::Observe,
                        Ok(source.clone()),
                    ),
                    &mut projection,
                    0,
                ),
                ActiveTransition::Submit(DocumentControlCommand::Observe)
            ));
            let mut progressed = source_pending.clone();
            progressed.state_generation = servo::document_pending::RuntimeStateGeneration::new(8);
            assert!(matches!(
                transition_from_control_completion(
                    &mut active,
                    DocumentControlCommand::Observe,
                    observed_outcome(progressed),
                    &mut projection,
                    0,
                ),
                ActiveTransition::SubmitSessionNavigationObservation {
                    allow_servo_pump: false
                }
            ));
            assert!(matches!(
                transition_from_navigation_completion(
                    &mut active,
                    NavigationOperationCompletion::test_response(
                        NavigationOperationKind::Observe,
                        Ok(changed_n2),
                    ),
                    &mut projection,
                    0,
                ),
                ActiveTransition::RejectStaleStateToken
            ));
            assert!(
                !projection
                    .authorizes_document_state(&source_pending, &source, &token)
                    .unwrap(),
                "an N2 near miss must latch the resolved token strict-stale",
            );
        }
    }

    #[test]
    fn settle_token_document_rejections_are_validation_first_and_do_not_start_settlement() {
        let source = session_authority(1, 0, 0, 0);
        let admitted = replacement_admission_authority(&source);
        let source_pending = pending_for_authority(&source, 7);
        let source_pipeline_id = source.target().active_top_level.unwrap().pipeline_id;
        let replacement_pipeline_id = admitted.target().pending_top_level_pipelines()[0];

        for rejection in [
            DocumentControlError::TargetChanged {
                expected: Box::new(source.target().clone()),
                observed: Box::new(admitted.target().clone()),
            },
            DocumentControlError::ReplacementPipelineBootstrapRequired {
                source_pipeline_id,
                pipeline_id: replacement_pipeline_id,
            },
        ] {
            let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
                stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x76; 16]),
            );
            let token = projection
                .document_state_token(&source_pending, &source)
                .unwrap();
            let mut active =
                token_authorizing_settle_active(&source, &source_pending, &mut projection);
            assert!(matches!(
                transition_from_navigation_completion(
                    &mut active,
                    NavigationOperationCompletion::test_response(
                        NavigationOperationKind::Observe,
                        Ok(source.clone()),
                    ),
                    &mut projection,
                    0,
                ),
                ActiveTransition::Submit(DocumentControlCommand::Observe)
            ));
            assert!(matches!(
                transition_from_control_completion(
                    &mut active,
                    DocumentControlCommand::Observe,
                    DocumentControlReceiveOutcome::CommandOutcome(
                        DocumentControlOutcome::Rejected(rejection),
                    ),
                    &mut projection,
                    0,
                ),
                ActiveTransition::RejectStaleStateToken
            ));
            assert!(
                !projection
                    .authorizes_document_state(&source_pending, &source, &token)
                    .unwrap(),
                "a definitive D authority rejection must latch the token strict-stale",
            );
            let ActiveOperation::Settle(state) = &mut active.operation else {
                panic!("a D authority rejection lost settlement state");
            };
            assert!(state.authorizing_observation.is_none());
            assert!(state.replacement.is_none());
            assert!(matches!(
                state.coordinator.start(),
                Ok(settle::SettleProgress::Command(
                    DocumentControlCommand::Observe
                ))
            ));
        }

        for malformed in [
            DocumentControlError::TargetChanged {
                expected: Box::new(source.target().clone()),
                observed: Box::new(source.target().clone()),
            },
            DocumentControlError::ReplacementPipelineBootstrapRequired {
                source_pipeline_id,
                pipeline_id: source_pipeline_id,
            },
        ] {
            let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
                stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x77; 16]),
            );
            let mut active =
                token_authorizing_settle_active(&source, &source_pending, &mut projection);
            assert!(matches!(
                transition_from_navigation_completion(
                    &mut active,
                    NavigationOperationCompletion::test_response(
                        NavigationOperationKind::Observe,
                        Ok(source.clone()),
                    ),
                    &mut projection,
                    0,
                ),
                ActiveTransition::Submit(DocumentControlCommand::Observe)
            ));
            let ActiveTransition::Fail(failure) = transition_from_control_completion(
                &mut active,
                DocumentControlCommand::Observe,
                DocumentControlReceiveOutcome::CommandOutcome(DocumentControlOutcome::Rejected(
                    malformed,
                )),
                &mut projection,
                0,
            ) else {
                panic!("a malformed D rejection was mistaken for stale authority");
            };
            assert!(failure.fail_stop);
            assert!(failure.error.fatal);
            assert_eq!(failure.error.code, "internal_runtime_failure");
        }

        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x78; 16]),
        );
        let mut active = token_authorizing_settle_active(&source, &source_pending, &mut projection);
        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(source),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::Submit(DocumentControlCommand::Observe)
        ));
        let ActiveTransition::Fail(failure) = transition_from_control_completion(
            &mut active,
            DocumentControlCommand::Observe,
            DocumentControlReceiveOutcome::CommandOutcome(DocumentControlOutcome::Rejected(
                DocumentControlError::EventLoopUnavailable,
            )),
            &mut projection,
            0,
        ) else {
            panic!("an unrelated D rejection did not preserve its normal mapping");
        };
        assert!(!failure.fail_stop);
        assert!(!failure.error.fatal);
        assert_eq!(failure.error.code, "document_control_rejected");
    }

    #[test]
    fn settle_token_navigation_target_errors_latch_at_n1_and_n2() {
        let source = session_authority(1, 0, 0, 0);
        let admitted = replacement_admission_authority(&source);
        let source_pending = pending_for_authority(&source, 7);
        let target_changed = || SessionNavigationError::TargetChanged {
            expected: Box::new(source.clone()),
            observed: Box::new(admitted.clone()),
        };

        for at_n2 in [false, true] {
            let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
                stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x79; 16]),
            );
            let token = projection
                .document_state_token(&source_pending, &source)
                .unwrap();
            let mut active =
                token_authorizing_settle_active(&source, &source_pending, &mut projection);
            if at_n2 {
                assert!(matches!(
                    transition_from_navigation_completion(
                        &mut active,
                        NavigationOperationCompletion::test_response(
                            NavigationOperationKind::Observe,
                            Ok(source.clone()),
                        ),
                        &mut projection,
                        0,
                    ),
                    ActiveTransition::Submit(DocumentControlCommand::Observe)
                ));
                assert!(matches!(
                    transition_from_control_completion(
                        &mut active,
                        DocumentControlCommand::Observe,
                        observed_outcome(source_pending.clone()),
                        &mut projection,
                        0,
                    ),
                    ActiveTransition::SubmitSessionNavigationObservation {
                        allow_servo_pump: false
                    }
                ));
            }
            assert!(matches!(
                transition_from_navigation_completion(
                    &mut active,
                    NavigationOperationCompletion::test_response(
                        NavigationOperationKind::Observe,
                        Err(target_changed()),
                    ),
                    &mut projection,
                    0,
                ),
                ActiveTransition::RejectStaleStateToken
            ));
            assert!(
                !projection
                    .authorizes_document_state(&source_pending, &source, &token)
                    .unwrap(),
                "a navigation TargetChanged at N1/N2 must latch the token strict-stale",
            );
            let ActiveOperation::Settle(state) = &mut active.operation else {
                panic!("a navigation TargetChanged lost settlement state");
            };
            assert!(matches!(
                state.coordinator.start(),
                Ok(settle::SettleProgress::Command(
                    DocumentControlCommand::Observe
                ))
            ));
        }
    }

    #[test]
    fn settle_token_navigation_terminals_precede_stale_matching_without_starting_coordinator() {
        let source = session_authority(1, 7, 3, 0);
        let source_pending = pending_for_authority(&source, 7);

        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x7a; 16]),
        );
        let token = projection
            .document_state_token(&source_pending, &source)
            .unwrap();
        let mut active = token_authorizing_settle_active(&source, &source_pending, &mut projection);
        let terminal = terminal_session_authority(
            &source,
            SessionNavigationTerminal::HistoryLimitExceeded {
                limit: 10,
                observed: 10,
                navigation_id: source.navigation_id(),
                history_revision: source.history_revision(),
            },
        );
        let ActiveTransition::Fail(history) = transition_from_navigation_completion(
            &mut active,
            NavigationOperationCompletion::test_response(
                NavigationOperationKind::Observe,
                Ok(terminal),
            ),
            &mut projection,
            0,
        ) else {
            panic!("an N1 history terminal was mistaken for stale authority");
        };
        assert_eq!(history.error.code, "history_limit_exceeded");
        assert!(!history.fail_stop);
        assert!(!history.error.fatal);
        assert!(
            projection
                .authorizes_document_state(&source_pending, &source, &token)
                .unwrap(),
            "a typed navigation terminal must not stale-latch the document token",
        );
        let ActiveOperation::Settle(state) = &mut active.operation else {
            panic!("an N1 terminal lost settlement state");
        };
        assert!(matches!(
            state.coordinator.start(),
            Ok(settle::SettleProgress::Command(
                DocumentControlCommand::Observe
            ))
        ));

        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x7b; 16]),
        );
        let token = projection
            .document_state_token(&source_pending, &source)
            .unwrap();
        let mut active = token_authorizing_settle_active(&source, &source_pending, &mut projection);
        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(source.clone()),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::Submit(DocumentControlCommand::Observe)
        ));
        assert!(matches!(
            transition_from_control_completion(
                &mut active,
                DocumentControlCommand::Observe,
                observed_outcome(source_pending.clone()),
                &mut projection,
                0,
            ),
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false
            }
        ));
        let terminal = terminal_session_authority(
            &source,
            SessionNavigationTerminal::DocumentTransitionLimitExceeded {
                limit: 4,
                observed: 4,
                next_navigation_id: SessionNavigationId::new(8),
            },
        );
        let ActiveTransition::Fail(document_limit) = transition_from_navigation_completion(
            &mut active,
            NavigationOperationCompletion::test_response(
                NavigationOperationKind::Observe,
                Ok(terminal),
            ),
            &mut projection,
            0,
        ) else {
            panic!("an N2 document terminal was mistaken for stale authority");
        };
        assert_eq!(
            document_limit.error.code,
            "document_transition_limit_exceeded"
        );
        assert!(!document_limit.fail_stop);
        assert!(
            projection
                .authorizes_document_state(&source_pending, &source, &token)
                .unwrap()
        );
        let ActiveOperation::Settle(state) = &mut active.operation else {
            panic!("an N2 terminal lost settlement state");
        };
        assert!(matches!(
            state.coordinator.start(),
            Ok(settle::SettleProgress::Command(
                DocumentControlCommand::Observe
            ))
        ));

        for (terminal, expected_code, expected_effect) in [
            (
                SessionNavigationTerminal::RedirectLimitExceeded {
                    limit: 20,
                    observed: 21,
                    navigation_id: source.navigation_id(),
                },
                "redirect_limit_exceeded",
                "partial",
            ),
            (
                SessionNavigationTerminal::CounterOverflow {
                    counter: SessionNavigationCounter::HistoryRevision,
                },
                "runtime_error",
                "none",
            ),
        ] {
            let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
                stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x7c; 16]),
            );
            let token = projection
                .document_state_token(&source_pending, &source)
                .unwrap();
            let mut active =
                token_authorizing_settle_active(&source, &source_pending, &mut projection);
            let ActiveTransition::Fail(failure) = transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Err(SessionNavigationError::Terminal(terminal)),
                ),
                &mut projection,
                0,
            ) else {
                panic!("a fail-stop navigation terminal was mistaken for stale authority");
            };
            assert_eq!(failure.error.code, expected_code);
            assert_eq!(failure.error.state_effect, expected_effect);
            assert!(failure.fail_stop);
            assert!(failure.error.fatal);
            assert!(
                projection
                    .authorizes_document_state(&source_pending, &source, &token)
                    .unwrap()
            );
            let ActiveOperation::Settle(state) = &mut active.operation else {
                panic!("a fail-stop terminal lost settlement state");
            };
            assert!(matches!(
                state.coordinator.start(),
                Ok(settle::SettleProgress::Command(
                    DocumentControlCommand::Observe
                ))
            ));
        }
    }

    #[test]
    fn settle_token_n2_seeds_the_current_controlled_network_gate_before_deciding() {
        let source = session_authority(1, 0, 0, 0);
        let source_pending = pending_for_authority(&source, 7);
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x75; 16]),
        );
        let mut active = token_authorizing_settle_active(&source, &source_pending, &mut projection);
        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(source.clone()),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::Submit(DocumentControlCommand::Observe)
        ));
        let mut progressed = source_pending;
        progressed.state_generation = servo::document_pending::RuntimeStateGeneration::new(8);
        assert!(matches!(
            transition_from_control_completion(
                &mut active,
                DocumentControlCommand::Observe,
                observed_outcome(progressed),
                &mut projection,
                1,
            ),
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false
            }
        ));
        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(source),
                ),
                &mut projection,
                1,
            ),
            ActiveTransition::Wait(settle::SettleWait::ForegroundExternalIo { .. })
        ));
        let ActiveOperation::Settle(state) = &active.operation else {
            panic!("network-gated token seed lost settlement state");
        };
        assert!(state.authorizing_document_state.is_none());
        assert!(state.authorizing_observation.is_none());
        assert!(state.authority_bound_command.is_none());
    }

    #[test]
    fn settle_token_generation_regression_is_fatal_before_coordinator_start() {
        let source = session_authority(1, 0, 0, 0);
        let source_pending = pending_for_authority(&source, 7);
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x74; 16]),
        );
        let mut active = token_authorizing_settle_active(&source, &source_pending, &mut projection);
        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(source.clone()),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::Submit(DocumentControlCommand::Observe)
        ));
        let mut regressed = source_pending;
        regressed.state_generation = servo::document_pending::RuntimeStateGeneration::new(6);
        let ActiveTransition::Fail(failure) = transition_from_control_completion(
            &mut active,
            DocumentControlCommand::Observe,
            observed_outcome(regressed),
            &mut projection,
            0,
        ) else {
            panic!("a regressed generation authorized settlement");
        };
        assert!(failure.fail_stop);
        assert!(failure.error.fatal);
        assert_eq!(failure.error.code, "internal_runtime_failure");
    }

    #[test]
    fn held_drive_refresh_accepts_coalesced_same_document_authority() {
        let source = session_authority(1, 0, 0, 0);
        let observed = SessionNavigationAuthority::new_internal(
            Box::new(source.target().clone()),
            source.document_epoch(),
            source.navigation_id(),
            embedder_traits::document_session::HistoryRevision::new(3),
            source.successful_document_replacements(),
            source.url().clone(),
            None,
        );
        let effective_policy = default_resolved_settle_policy();
        let mut active = ActiveRequest {
            request: request("runtime.settle", Some(SESSION_ID)),
            profile: Some(SessionProfile::ControlledWebSessionV1),
            operation: ActiveOperation::Settle(SettleState {
                profile: SessionProfile::ControlledWebSessionV1,
                authorizing_document_state: None,
                authorizing_observation: None,
                authorizing_navigation: Some(source.clone()),
                replacement: None,
                authority_bound_command: Some(DocumentControlCommand::DriveOneTurn),
                latest_pending_target: Some(Box::new(source.target().clone())),
                response: SettleResponse::Runtime,
                coordinator: settle::SettleCoordinator::new(effective_policy.engine),
                effective_policy,
                cumulative_external_io_wall_time: Duration::ZERO,
                waiting: None,
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::None,
        };
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x71; 16]),
        );

        let transition = transition_from_navigation_completion(
            &mut active,
            NavigationOperationCompletion::test_response(
                NavigationOperationKind::Observe,
                Ok(observed.clone()),
            ),
            &mut projection,
            0,
        );
        assert!(matches!(
            transition,
            ActiveTransition::Submit(DocumentControlCommand::DriveOneTurn)
        ));
        let ActiveOperation::Settle(state) = &active.operation else {
            panic!("held Drive lost settlement state");
        };
        assert_eq!(state.authorizing_navigation.as_ref(), Some(&observed));
        assert!(state.authority_bound_command.is_none());
    }

    #[test]
    fn indeterminate_drive_with_stable_authority_is_counted_once_and_reobserved() {
        let source = session_authority(1, 0, 0, 0);
        let mut active = indeterminate_drive_settle_active(&source);
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x72; 16]),
        );

        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(source.clone()),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::Submit(DocumentControlCommand::Observe)
        ));
        let ActiveOperation::Settle(state) = &active.operation else {
            panic!("stable authority recovery lost settlement state");
        };
        assert!(state.replacement.is_none());
        assert_eq!(state.authorizing_navigation.as_ref(), Some(&source));
        assert_eq!(
            state.latest_pending_target.as_deref(),
            Some(source.target())
        );
    }

    #[test]
    fn indeterminate_drive_with_same_document_history_change_is_reobserved() {
        let source = session_authority(1, 0, 0, 0);
        let changed = SessionNavigationAuthority::new_internal(
            Box::new(source.target().clone()),
            source.document_epoch(),
            source.navigation_id(),
            embedder_traits::document_session::HistoryRevision::new(1),
            source.successful_document_replacements(),
            servo::ServoUrl::parse("https://example.test/#changed").unwrap(),
            None,
        );
        let mut active = indeterminate_drive_settle_active(&source);
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x75; 16]),
        );

        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(changed.clone()),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::Submit(DocumentControlCommand::Observe)
        ));
        let ActiveOperation::Settle(state) = &active.operation else {
            panic!("same-document recovery lost settlement state");
        };
        assert_eq!(state.authorizing_navigation.as_ref(), Some(&changed));
    }

    #[test]
    fn indeterminate_drive_with_exact_replacement_still_bootstraps() {
        let source = session_authority(1, 0, 0, 0);
        let admitted = replacement_admission_authority(&source);
        let mut active = indeterminate_drive_settle_active(&source);
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x73; 16]),
        );
        let bootstrap = DocumentControlCommand::BootstrapReplacementPipeline {
            source_pipeline_id: source.target().active_top_level.unwrap().pipeline_id,
            pipeline_id: admitted.target().pending_top_level_pipelines()[0],
        };

        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(admitted.clone()),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::Submit(command) if command == bootstrap
        ));
        let ActiveOperation::Settle(state) = &active.operation else {
            panic!("replacement recovery lost settlement state");
        };
        assert!(matches!(
            state.replacement.as_ref(),
            Some(SettleReplacementPhase::Bootstrapping {
                source: actual_source,
                admitted: actual_admitted,
                command,
            }) if actual_source == &source && actual_admitted == &admitted && command == &bootstrap
        ));
    }

    #[test]
    fn indeterminate_drive_authority_near_miss_remains_fatal() {
        let source = session_authority(1, 0, 0, 0);
        let admitted = replacement_admission_authority(&source);
        let near_miss = SessionNavigationAuthority::new_internal(
            Box::new(admitted.target().clone()),
            admitted.document_epoch(),
            SessionNavigationId::new(source.navigation_id().get() + 2),
            admitted.history_revision(),
            admitted.successful_document_replacements(),
            admitted.url().clone(),
            None,
        );
        let mut active = indeterminate_drive_settle_active(&source);
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x74; 16]),
        );

        let ActiveTransition::Fail(failure) = transition_from_navigation_completion(
            &mut active,
            NavigationOperationCompletion::test_response(
                NavigationOperationKind::Observe,
                Ok(near_miss),
            ),
            &mut projection,
            0,
        ) else {
            panic!("an owner-authority near miss recovered an indeterminate Drive");
        };
        assert!(failure.fail_stop);
        assert!(failure.error.fatal);
        assert_eq!(failure.error.code, "navigation_authority_changed");
    }

    #[test]
    fn queued_replacement_rearms_the_unsubmitted_coordinator_drive_exactly_once() {
        let source = session_authority(1, 0, 0, 0);
        let admitted = replacement_admission_authority(&source);
        let activated = activated_replacement_authority(&source, &admitted);
        let effective_policy = default_resolved_settle_policy();
        let mut coordinator = settle::SettleCoordinator::new(effective_policy.engine);
        assert_eq!(
            coordinator.start(),
            Ok(settle::SettleProgress::Command(
                DocumentControlCommand::Observe
            )),
        );
        let source_pending = pending_for_authority(&source, 7);
        assert_eq!(
            coordinator
                .consume_receive_outcome(observed_outcome(source_pending.clone()), Duration::ZERO,),
            Ok(settle::SettleProgress::Command(
                DocumentControlCommand::DriveOneTurn
            )),
        );
        let mut active = ActiveRequest {
            request: request("runtime.settle", Some(SESSION_ID)),
            profile: Some(SessionProfile::ControlledWebSessionV1),
            operation: ActiveOperation::Settle(SettleState {
                profile: SessionProfile::ControlledWebSessionV1,
                authorizing_document_state: None,
                authorizing_observation: None,
                authorizing_navigation: Some(source.clone()),
                replacement: None,
                authority_bound_command: Some(DocumentControlCommand::DriveOneTurn),
                latest_pending_target: Some(Box::new(source.target().clone())),
                response: SettleResponse::Runtime,
                coordinator,
                effective_policy,
                cumulative_external_io_wall_time: Duration::ZERO,
                waiting: None,
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::None,
        };
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x71; 16]),
        );

        let bootstrap = DocumentControlCommand::BootstrapReplacementPipeline {
            source_pipeline_id: source.target().active_top_level.unwrap().pipeline_id,
            pipeline_id: admitted.target().pending_top_level_pipelines()[0],
        };
        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(admitted.clone()),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::Submit(command) if command == bootstrap
        ));
        assert_eq!(active.state_effect, RequestStateEffect::Partial);
        let ActiveOperation::Settle(state) = &active.operation else {
            panic!("replacement rearm lost settlement state");
        };
        assert!(state.authority_bound_command.is_none());
        assert!(matches!(
            state.replacement,
            Some(SettleReplacementPhase::Bootstrapping { .. })
        ));

        let mut activated_pending = pending_for_authority(&activated, 8);
        activated_pending.clock = source_pending.clock;
        activated_pending.scheduler = source_pending.scheduler;
        activated_pending
            .execution
            .as_mut()
            .expect("test pending has execution authority")
            .clock_id = activated_pending.clock.clock_id;
        activated_pending.validate().unwrap();
        assert!(matches!(
            transition_from_control_completion(
                &mut active,
                bootstrap,
                turn_processed_outcome(activated_pending),
                &mut projection,
                0,
            ),
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false
            }
        ));
        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(activated.clone()),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false
            }
        ));
        let ActiveOperation::Settle(state) = &active.operation else {
            panic!("activated replacement lost settlement state");
        };
        assert!(state.replacement.is_none());
        assert_eq!(
            state.authority_bound_command,
            Some(DocumentControlCommand::DriveOneTurn),
        );
        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(activated),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::Submit(DocumentControlCommand::DriveOneTurn)
        ));
    }

    #[test]
    fn queued_replacement_rearm_rejects_owner_authority_near_misses() {
        let source = session_authority(1, 0, 0, 0);
        let admitted = replacement_admission_authority(&source);
        let revision_gap = SessionNavigationAuthority::new_internal(
            Box::new(admitted.target().clone()),
            admitted.document_epoch(),
            SessionNavigationId::new(source.navigation_id().get() + 2),
            admitted.history_revision(),
            admitted.successful_document_replacements(),
            admitted.url().clone(),
            None,
        );
        assert!(exact_replacement_admission(&source, &revision_gap).is_none());

        let effective_policy = default_resolved_settle_policy();
        let mut coordinator = settle::SettleCoordinator::new(effective_policy.engine);
        assert!(matches!(
            coordinator.start(),
            Ok(settle::SettleProgress::Command(
                DocumentControlCommand::Observe
            ))
        ));
        assert!(matches!(
            coordinator.consume_receive_outcome(
                observed_outcome(pending_for_authority(&source, 7)),
                Duration::ZERO,
            ),
            Ok(settle::SettleProgress::Command(
                DocumentControlCommand::DriveOneTurn
            ))
        ));
        let mut active = ActiveRequest {
            request: request("runtime.settle", Some(SESSION_ID)),
            profile: Some(SessionProfile::ControlledWebSessionV1),
            operation: ActiveOperation::Settle(SettleState {
                profile: SessionProfile::ControlledWebSessionV1,
                authorizing_document_state: None,
                authorizing_observation: None,
                authorizing_navigation: Some(source.clone()),
                replacement: None,
                authority_bound_command: Some(DocumentControlCommand::DriveOneTurn),
                latest_pending_target: Some(Box::new(source.target().clone())),
                response: SettleResponse::Runtime,
                coordinator,
                effective_policy,
                cumulative_external_io_wall_time: Duration::ZERO,
                waiting: None,
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::None,
        };
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x71; 16]),
        );

        let ActiveTransition::Fail(failure) = transition_from_navigation_completion(
            &mut active,
            NavigationOperationCompletion::test_response(
                NavigationOperationKind::Observe,
                Ok(revision_gap),
            ),
            &mut projection,
            0,
        ) else {
            panic!("a near-miss owner admission rearmed the coordinator Drive");
        };
        assert!(failure.fail_stop);
        assert!(failure.error.fatal);
        assert_eq!(failure.error.code, "navigation_authority_changed");
    }

    #[test]
    fn replacement_control_outcome_is_held_until_sole_pipeline_controlled_ready() {
        let source = session_authority(1, 0, 0, 0);
        let admitted = replacement_admission_authority(&source);
        let activated = activated_replacement_authority(&source, &admitted);
        let effective_policy = default_resolved_settle_policy();
        let source_pipeline_id = source.target().active_top_level.unwrap().pipeline_id;
        let pipeline_id = admitted.target().pending_top_level_pipelines()[0];
        let bootstrap = DocumentControlCommand::BootstrapReplacementPipeline {
            source_pipeline_id,
            pipeline_id,
        };
        let mut coordinator = settle::SettleCoordinator::new(effective_policy.engine);
        assert_eq!(
            coordinator.start_with_replacement_bootstrap(bootstrap.clone()),
            Ok(settle::SettleProgress::Command(bootstrap.clone())),
        );
        let mut active = ActiveRequest {
            request: request("runtime.settle", Some(SESSION_ID)),
            profile: Some(SessionProfile::ControlledWebSessionV1),
            operation: ActiveOperation::Settle(SettleState {
                profile: SessionProfile::ControlledWebSessionV1,
                authorizing_document_state: None,
                authorizing_observation: None,
                authorizing_navigation: Some(source.clone()),
                replacement: Some(SettleReplacementPhase::Bootstrapping {
                    source: source.clone(),
                    admitted: admitted.clone(),
                    command: bootstrap.clone(),
                }),
                authority_bound_command: None,
                latest_pending_target: Some(Box::new(admitted.target().clone())),
                response: SettleResponse::Runtime,
                coordinator,
                effective_policy,
                cumulative_external_io_wall_time: Duration::ZERO,
                waiting: None,
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::Partial,
        };
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x71; 16]),
        );

        let bootstrap_outcome = turn_processed_outcome(pending_for_authority(&activated, 9));
        let passive = transition_from_control_completion(
            &mut active,
            bootstrap,
            bootstrap_outcome,
            &mut projection,
            0,
        );
        assert!(matches!(
            passive,
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false
            }
        ));

        let ready = transition_from_navigation_completion(
            &mut active,
            NavigationOperationCompletion::test_response(
                NavigationOperationKind::Observe,
                Ok(activated.clone()),
            ),
            &mut projection,
            0,
        );
        assert!(matches!(
            ready,
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false
            }
        ));
        let ActiveOperation::Settle(state) = &active.operation else {
            panic!("ready replacement lost settlement state");
        };
        assert!(state.replacement.is_none());
        assert_eq!(
            state.latest_pending_target.as_deref(),
            Some(activated.target())
        );
        assert_eq!(
            state.authority_bound_command,
            Some(DocumentControlCommand::DriveOneTurn),
        );

        let release = transition_from_navigation_completion(
            &mut active,
            NavigationOperationCompletion::test_response(
                NavigationOperationKind::Observe,
                Ok(activated),
            ),
            &mut projection,
            0,
        );
        assert!(matches!(
            release,
            ActiveTransition::Submit(DocumentControlCommand::DriveOneTurn)
        ));
        let ActiveOperation::Settle(state) = &active.operation else {
            panic!("ready Drive release lost settlement state");
        };
        assert!(state.authority_bound_command.is_none());
    }

    #[test]
    fn source_retained_activation_stays_on_the_coordinator_wait_and_drive_ledger() {
        let source = session_authority(1, 0, 0, 0);
        let admitted = replacement_admission_authority(&source);
        let retained = activated_replacement_source_retained_authority(&source, &admitted);
        let activated = activated_replacement_authority(&source, &admitted);
        assert!(held_drive_replacement_target_progressed(
            &source,
            &admitted,
            admitted.target(),
            &retained,
        ));
        assert!(held_drive_replacement_target_progressed(
            &source,
            &admitted,
            retained.target(),
            &activated,
        ));
        assert!(!held_drive_replacement_target_progressed(
            &source,
            &admitted,
            retained.target(),
            &admitted,
        ));

        let effective_policy = default_resolved_settle_policy();
        let bootstrap = DocumentControlCommand::BootstrapReplacementPipeline {
            source_pipeline_id: source.target().active_top_level.unwrap().pipeline_id,
            pipeline_id: admitted.target().pending_top_level_pipelines()[0],
        };
        let mut coordinator = settle::SettleCoordinator::new(effective_policy.engine);
        assert_eq!(
            coordinator.start_with_replacement_bootstrap(bootstrap.clone()),
            Ok(settle::SettleProgress::Command(bootstrap.clone())),
        );
        let mut active = ActiveRequest {
            request: request("runtime.settle", Some(SESSION_ID)),
            profile: Some(SessionProfile::ControlledWebSessionV1),
            operation: ActiveOperation::Settle(SettleState {
                profile: SessionProfile::ControlledWebSessionV1,
                authorizing_document_state: None,
                authorizing_observation: None,
                authorizing_navigation: Some(source.clone()),
                replacement: Some(SettleReplacementPhase::Bootstrapping {
                    source,
                    admitted: admitted.clone(),
                    command: bootstrap.clone(),
                }),
                authority_bound_command: None,
                latest_pending_target: Some(Box::new(admitted.target().clone())),
                response: SettleResponse::Runtime,
                coordinator,
                effective_policy,
                cumulative_external_io_wall_time: Duration::ZERO,
                waiting: None,
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::Partial,
        };
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x71; 16]),
        );

        let bootstrap_pending = pending_for_authority(&admitted, 9);
        assert!(matches!(
            transition_from_control_completion(
                &mut active,
                bootstrap,
                turn_processed_outcome(bootstrap_pending.clone()),
                &mut projection,
                0,
            ),
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false
            }
        ));
        let wait = transition_from_navigation_completion(
            &mut active,
            NavigationOperationCompletion::test_response(
                NavigationOperationKind::Observe,
                Ok(retained.clone()),
            ),
            &mut projection,
            0,
        );
        assert!(matches!(wait, ActiveTransition::Wait(_)));
        let ActiveOperation::Settle(state) = &mut active.operation else {
            panic!("retained activation lost settlement state");
        };
        assert!(matches!(
            state.replacement,
            Some(SettleReplacementPhase::Activating { .. })
        ));

        let resumed = state.coordinator.resume_after_wake(Duration::ZERO).unwrap();
        assert!(matches!(
            transition_from_settle_progress_for_active(
                state,
                active.started_at,
                resumed,
                active.state_effect,
                &mut projection,
            ),
            ActiveTransition::Submit(DocumentControlCommand::Observe)
        ));
        let mut ready_pending = pending_for_authority(&activated, 10);
        ready_pending.clock = bootstrap_pending.clock;
        ready_pending.scheduler = bootstrap_pending.scheduler;
        ready_pending
            .execution
            .as_mut()
            .expect("test pending has execution authority")
            .clock_id = ready_pending.clock.clock_id;
        ready_pending.validate().unwrap();
        assert!(matches!(
            transition_from_control_completion(
                &mut active,
                DocumentControlCommand::Observe,
                observed_outcome(ready_pending),
                &mut projection,
                0,
            ),
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false
            }
        ));
        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(activated.clone()),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false
            }
        ));
        let ActiveOperation::Settle(state) = &active.operation else {
            panic!("ready activation lost settlement state");
        };
        assert!(state.replacement.is_none());
        assert_eq!(state.authorizing_navigation.as_ref(), Some(&activated));
        assert_eq!(
            state.latest_pending_target.as_deref(),
            Some(activated.target())
        );
        assert_eq!(
            state.authority_bound_command,
            Some(DocumentControlCommand::DriveOneTurn),
        );

        assert!(matches!(
            transition_from_navigation_completion(
                &mut active,
                NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Ok(activated),
                ),
                &mut projection,
                0,
            ),
            ActiveTransition::Submit(DocumentControlCommand::DriveOneTurn)
        ));
    }

    #[test]
    fn replacement_completion_never_projects_before_controlled_ready() {
        let source = session_authority(1, 0, 0, 0);
        let admitted = replacement_admission_authority(&source);
        let effective_policy = default_resolved_settle_policy();
        let mut state = SettleState {
            profile: SessionProfile::ControlledWebSessionV1,
            authorizing_document_state: None,
            authorizing_observation: None,
            authorizing_navigation: Some(source.clone()),
            replacement: Some(SettleReplacementPhase::Activating { source, admitted }),
            authority_bound_command: None,
            latest_pending_target: None,
            response: SettleResponse::Runtime,
            coordinator: settle::SettleCoordinator::new(effective_policy.engine),
            effective_policy,
            cumulative_external_io_wall_time: Duration::ZERO,
            waiting: None,
        };
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x71; 16]),
        );
        let terminal_pending = Box::new(pending_for_authority(
            state.authorizing_navigation.as_ref().unwrap(),
            11,
        ));
        let ActiveTransition::Fail(blocked) = transition_from_settle_progress_for_active(
            &mut state,
            Instant::now(),
            settle::SettleProgress::Complete(settle::SettleCompletion::BlockedOnExternalIo {
                pending: terminal_pending,
                network: Vec::new(),
                control_turns: 1,
            }),
            RequestStateEffect::Partial,
            &mut projection,
        ) else {
            panic!("an incomplete replacement projected a terminal settlement result");
        };
        assert!(blocked.fail_stop);
        assert!(blocked.error.fatal);
        assert_eq!(blocked.error.code, "blocked_on_external_io");
        assert_eq!(blocked.error.state_effect, "partial");
    }

    #[test]
    fn pending_projection_rearms_when_replacement_admission_races_its_document_observe() {
        let source = session_authority(1, 0, 0, 0);
        let admitted = replacement_admission_authority(&source);
        let source_pipeline_id = source.target().active_top_level.unwrap().pipeline_id;
        let pipeline_id = admitted.target().pending_top_level_pipelines()[0];
        let effective_policy = default_resolved_settle_policy();
        let mut active = ActiveRequest {
            request: request("runtime.pending", Some(SESSION_ID)),
            profile: Some(SessionProfile::ControlledWebSessionV1),
            operation: ActiveOperation::SessionProjection(SessionProjectionState {
                pending: Box::new(pending_for_authority(&source, 7)),
                kind: SessionProjectionKind::Value {
                    value: json!({ "stateGeneration": "7" }),
                    snapshot_token: false,
                    settle_resume: Some(SettleProjectionResume {
                        profile: SessionProfile::ControlledWebSessionV1,
                        effective_policy,
                        cumulative_external_io_wall_time: Duration::ZERO,
                        authorizing_navigation: None,
                        response: SettleResponse::Pending,
                    }),
                },
                phase: SessionProjectionPhase::AwaitingPendingObservation {
                    navigation: source.clone(),
                },
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::None,
        };
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x71; 16]),
        );
        let rejected =
            DocumentControlReceiveOutcome::CommandOutcome(DocumentControlOutcome::Rejected(
                DocumentControlError::ReplacementPipelineBootstrapRequired {
                    source_pipeline_id,
                    pipeline_id,
                },
            ));

        let retry = transition_from_control_completion(
            &mut active,
            DocumentControlCommand::Observe,
            rejected,
            &mut projection,
            0,
        );
        assert!(matches!(
            retry,
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false
            }
        ));
        assert!(matches!(
            &active.operation,
            ActiveOperation::SessionProjection(SessionProjectionState {
                phase: SessionProjectionPhase::AwaitingReplacementAdmission {
                    source: observed_source,
                    source_pipeline_id: observed_source_pipeline_id,
                    pipeline_id: observed_pipeline_id,
                },
                ..
            }) if observed_source == &source
                && *observed_source_pipeline_id == source_pipeline_id
                && *observed_pipeline_id == pipeline_id
        ));

        let rearmed = transition_from_navigation_completion(
            &mut active,
            NavigationOperationCompletion::test_response(
                NavigationOperationKind::Observe,
                Ok(admitted),
            ),
            &mut projection,
            0,
        );
        assert!(matches!(
            rearmed,
            ActiveTransition::Submit(DocumentControlCommand::BootstrapReplacementPipeline {
                source_pipeline_id: observed_source,
                pipeline_id: observed_pipeline,
            }) if observed_source == source_pipeline_id && observed_pipeline == pipeline_id
        ));
        assert!(matches!(
            &active.operation,
            ActiveOperation::Settle(SettleState {
                response: SettleResponse::Pending,
                replacement: Some(SettleReplacementPhase::Bootstrapping { .. }),
                ..
            })
        ));
    }

    #[test]
    fn raw_action_projection_preserves_shape_and_uses_fresh_bracketed_generation() {
        let source = session_authority(1, 0, 0, 0);
        let action_pending = pending_for_authority(&source, 7);
        let fresh_pending = pending_for_authority(&source, 9);
        let mut active = ActiveRequest {
            request: request("action.activate", Some(SESSION_ID)),
            profile: Some(SessionProfile::ControlledWebSessionV1),
            operation: ActiveOperation::Automation(AutomationState {
                profile: SessionProfile::ControlledWebSessionV1,
                kind: wire::PublicAutomationKind::Activate,
                unresolved: None,
                authorizing_navigation: Some(source.clone()),
                completed: Some(CompletedAutomation {
                    result: DocumentAutomationResult::Activated,
                    pending: Box::new(action_pending),
                    synchronous_navigation_emitted: false,
                }),
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::Partial,
        };
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x71; 16]),
        );

        let first = transition_from_navigation_completion(
            &mut active,
            NavigationOperationCompletion::test_response(
                NavigationOperationKind::Observe,
                Ok(source.clone()),
            ),
            &mut projection,
            0,
        );
        assert!(matches!(
            first,
            ActiveTransition::Submit(DocumentControlCommand::Observe)
        ));

        let second = transition_from_control_completion(
            &mut active,
            DocumentControlCommand::Observe,
            observed_outcome(fresh_pending),
            &mut projection,
            0,
        );
        assert!(matches!(
            second,
            ActiveTransition::SubmitSessionNavigationObservation {
                allow_servo_pump: false
            }
        ));

        let final_transition = transition_from_navigation_completion(
            &mut active,
            NavigationOperationCompletion::test_response(
                NavigationOperationKind::Observe,
                Ok(source),
            ),
            &mut projection,
            0,
        );
        let ActiveTransition::Complete(value) = final_transition else {
            panic!("raw action did not complete after its N1/pending/N2 bracket");
        };
        assert_eq!(value["stateGeneration"], "9");
        assert_eq!(value["stateToken"], test_document_token(1));
        assert!(value.get("outcome").is_none());
        assert!(value.get("snapshot").is_none());
    }

    #[test]
    fn synchronous_action_replacement_enters_common_settlement_without_replay() {
        let source = session_authority(1, 0, 0, 0);
        let admitted = replacement_admission_authority(&source);
        let action_pending = pending_for_authority(&source, 7);
        let mut active = ActiveRequest {
            request: request("action.activate", Some(SESSION_ID)),
            profile: Some(SessionProfile::ControlledWebSessionV1),
            operation: ActiveOperation::Automation(AutomationState {
                profile: SessionProfile::ControlledWebSessionV1,
                kind: wire::PublicAutomationKind::Activate,
                unresolved: None,
                authorizing_navigation: Some(source),
                completed: Some(CompletedAutomation {
                    result: DocumentAutomationResult::Activated,
                    pending: Box::new(action_pending),
                    synchronous_navigation_emitted: true,
                }),
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::Partial,
        };
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x71; 16]),
        );

        let transition = transition_from_navigation_completion(
            &mut active,
            NavigationOperationCompletion::test_response(
                NavigationOperationKind::Observe,
                Ok(admitted),
            ),
            &mut projection,
            0,
        );
        assert!(matches!(
            transition,
            ActiveTransition::Submit(DocumentControlCommand::BootstrapReplacementPipeline { .. })
        ));
        let ActiveOperation::Settle(SettleState {
            response:
                SettleResponse::Automation {
                    kind: wire::PublicAutomationKind::Activate,
                    result: DocumentAutomationResult::Activated,
                },
            replacement: Some(SettleReplacementPhase::Bootstrapping { .. }),
            ..
        }) = &active.operation
        else {
            panic!("synchronous action replacement did not enter common settlement");
        };
        assert!(
            active.in_flight.is_none(),
            "the action must not be replayed"
        );
    }

    #[test]
    fn synchronous_action_same_document_change_enters_fresh_projection() {
        let source = session_authority(1, 0, 0, 0);
        let changed = SessionNavigationAuthority::new_internal(
            Box::new(source.target().clone()),
            source.document_epoch(),
            source.navigation_id(),
            embedder_traits::document_session::HistoryRevision::new(2),
            source.successful_document_replacements(),
            servo::ServoUrl::parse("https://example.test/#changed").unwrap(),
            None,
        );
        let mut active = ActiveRequest {
            request: request("action.activate", Some(SESSION_ID)),
            profile: Some(SessionProfile::ControlledWebSessionV1),
            operation: ActiveOperation::Automation(AutomationState {
                profile: SessionProfile::ControlledWebSessionV1,
                kind: wire::PublicAutomationKind::Activate,
                unresolved: None,
                authorizing_navigation: Some(source.clone()),
                completed: Some(CompletedAutomation {
                    result: DocumentAutomationResult::Activated,
                    pending: Box::new(pending_for_authority(&source, 7)),
                    synchronous_navigation_emitted: true,
                }),
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::Partial,
        };
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x71; 16]),
        );

        let transition = transition_from_navigation_completion(
            &mut active,
            NavigationOperationCompletion::test_response(
                NavigationOperationKind::Observe,
                Ok(changed.clone()),
            ),
            &mut projection,
            0,
        );
        assert!(matches!(
            transition,
            ActiveTransition::Submit(DocumentControlCommand::Observe)
        ));
        assert!(matches!(
            &active.operation,
            ActiveOperation::SessionProjection(SessionProjectionState {
                kind: SessionProjectionKind::Automation {
                    replacement_rearm: true,
                    ..
                },
                phase: SessionProjectionPhase::AwaitingPendingObservation { navigation },
                ..
            }) if navigation == &changed
        ));
    }

    #[test]
    fn open_navigate_and_synchronous_action_share_bounded_replacement_rearm() {
        let source = session_authority(1, 0, 0, 0);
        let admitted = replacement_admission_authority(&source);
        let activated = activated_replacement_authority(&source, &admitted);
        let admitted_again = replacement_admission_authority(&activated);
        let activated_again = activated_replacement_authority(&activated, &admitted_again);
        let requested_url = Url::parse("https://example.test/next").unwrap();
        let effective_policy = default_resolved_settle_policy();
        let cumulative_external_io_wall_time = Duration::from_millis(37);

        let action_resume = SettleProjectionResume {
            profile: SessionProfile::ControlledWebSessionV1,
            effective_policy,
            cumulative_external_io_wall_time,
            authorizing_navigation: Some(source.clone()),
            response: SettleResponse::Automation {
                kind: wire::PublicAutomationKind::Activate,
                result: DocumentAutomationResult::Activated,
            },
        };
        assert!(!session_projection_allows_replacement_rearm(
            &SessionProjectionKind::Automation {
                settle_resume: action_resume.clone(),
                replacement_rearm: false,
            }
        ));
        assert!(session_projection_allows_replacement_rearm(
            &SessionProjectionKind::Automation {
                settle_resume: action_resume.clone(),
                replacement_rearm: true,
            }
        ));
        assert!(session_projection_allows_replacement_rearm(
            &SessionProjectionKind::ControlledOpen {
                requested_url: requested_url.clone(),
                current_url: Url::parse(source.url().as_str()).unwrap(),
                profile: SessionProfile::ControlledWebSessionV1,
                deadline: Instant::now(),
                bootstrap_attempted: true,
                cumulative_external_io_wall_time,
                session_state_token: None,
                settle_resume: Some(action_resume.clone()),
            }
        ));
        assert!(session_projection_allows_replacement_rearm(
            &SessionProjectionKind::Navigate {
                requested_url: requested_url.clone(),
                source: source.clone(),
                admitted: admitted.clone(),
                cumulative_external_io_wall_time,
                settle_resume: Some(action_resume.clone()),
            }
        ));

        let responses = [
            SettleResponse::Automation {
                kind: wire::PublicAutomationKind::Activate,
                result: DocumentAutomationResult::Activated,
            },
            SettleResponse::ControlledOpen {
                requested_url: requested_url.clone(),
                current_url: Url::parse(source.url().as_str()).unwrap(),
                profile: SessionProfile::ControlledWebSessionV1,
                deadline: Instant::now(),
                bootstrap_attempted: true,
            },
            SettleResponse::Navigate {
                requested_url: requested_url.clone(),
                source: source.clone(),
                admitted: admitted.clone(),
            },
        ];
        for response in responses {
            let expected_response = std::mem::discriminant(&response);
            let resume = SettleProjectionResume {
                profile: SessionProfile::ControlledWebSessionV1,
                effective_policy,
                cumulative_external_io_wall_time,
                authorizing_navigation: Some(source.clone()),
                response,
            };
            let Ok((operation, transition)) = resume_session_projection_at_replacement(
                &resume,
                source.clone(),
                admitted.clone(),
                Box::new(source.target().clone()),
                RequestStateEffect::Partial,
            ) else {
                panic!("exact replacement failed to resume common settlement");
            };
            assert!(matches!(
                transition,
                ActiveTransition::Submit(
                    DocumentControlCommand::BootstrapReplacementPipeline { .. }
                )
            ));
            let ActiveOperation::Settle(state) = operation else {
                panic!("replacement carry did not resume common settlement");
            };
            assert_eq!(std::mem::discriminant(&state.response), expected_response);
            assert_eq!(
                state.cumulative_external_io_wall_time,
                cumulative_external_io_wall_time,
            );
            assert!(matches!(
                state.replacement,
                Some(SettleReplacementPhase::Bootstrapping { .. })
            ));
        }

        assert!(explicit_navigation_reached_controlled_ready(
            &source, &admitted, &activated,
        ));
        assert_eq!(
            exact_replacement_admission(&activated, &admitted_again)
                .map(|admission| admission.pipeline_id),
            Some(admitted_again.target().pending_top_level_pipelines()[0]),
        );
        assert!(explicit_navigation_reached_controlled_ready(
            &activated,
            &admitted_again,
            &activated_again,
        ));
        assert!(explicit_navigation_chain_reached_controlled_ready(
            &source,
            &activated_again,
        ));
        let second_resume = SettleProjectionResume {
            profile: SessionProfile::ControlledWebSessionV1,
            effective_policy,
            cumulative_external_io_wall_time,
            authorizing_navigation: Some(activated.clone()),
            response: SettleResponse::Automation {
                kind: wire::PublicAutomationKind::Activate,
                result: DocumentAutomationResult::Activated,
            },
        };
        assert!(
            resume_session_projection_at_replacement(
                &second_resume,
                activated.clone(),
                admitted_again,
                Box::new(activated.target().clone()),
                RequestStateEffect::Partial,
            )
            .is_ok()
        );
    }

    #[test]
    fn post_action_navigation_drift_and_continuation_rejections_fail_stop() {
        let source = session_authority(1, 0, 0, 0);
        let admitted = replacement_admission_authority(&source);
        for error in [
            SessionNavigationError::NavigationInProgress,
            SessionNavigationError::SourceInactive,
            SessionNavigationError::TargetChanged {
                expected: Box::new(source.clone()),
                observed: Box::new(admitted.clone()),
            },
        ] {
            let ActiveTransition::Fail(failure) = session_navigation_failure(
                error,
                NavigationOperationKind::Observe,
                RequestStateEffect::Partial,
            ) else {
                panic!("post-action navigation rejection must fail");
            };
            assert!(failure.fail_stop);
            assert!(failure.error.fatal);
            assert_eq!(failure.error.state_effect, "partial");
        }

        for response in [
            SettleResponse::Automation {
                kind: wire::PublicAutomationKind::Activate,
                result: DocumentAutomationResult::Activated,
            },
            SettleResponse::ControlledOpen {
                requested_url: Url::parse("https://example.test/").unwrap(),
                current_url: Url::parse(source.url().as_str()).unwrap(),
                profile: SessionProfile::ControlledWebSessionV1,
                deadline: Instant::now(),
                bootstrap_attempted: true,
            },
            SettleResponse::Navigate {
                requested_url: Url::parse("https://example.test/next").unwrap(),
                source: source.clone(),
                admitted: admitted.clone(),
            },
        ] {
            let failure = settle_failure_for_response(
                settle::SettleFailure::ControlRejected(Box::new(
                    DocumentControlError::EventLoopUnavailable,
                )),
                RequestStateEffect::Partial,
                None,
                &response,
            );
            assert!(failure.fail_stop);
            assert!(failure.error.fatal);
            assert_eq!(failure.error.state_effect, "partial");
        }

        let mut transport_active = ActiveRequest {
            request: request("action.activate", Some(SESSION_ID)),
            profile: Some(SessionProfile::ControlledWebSessionV1),
            operation: ActiveOperation::Automation(AutomationState {
                profile: SessionProfile::ControlledWebSessionV1,
                kind: wire::PublicAutomationKind::Activate,
                unresolved: None,
                authorizing_navigation: Some(source),
                completed: Some(CompletedAutomation {
                    result: DocumentAutomationResult::Activated,
                    pending: Box::new(pending_for_authority(&admitted, 7)),
                    synchronous_navigation_emitted: true,
                }),
            }),
            started_at: Instant::now(),
            in_flight: None,
            control_turn_observed: None,
            needs_initial_pump: false,
            state_effect: RequestStateEffect::Partial,
        };
        let mut projection = wire::WireProjectionContext::new_with_namespace_internal(
            stasis_shell::token_namespace::OpaqueTokenNamespace::new_internal([0x71; 16]),
        );
        let ActiveTransition::Fail(transport) = transition_from_navigation_completion(
            &mut transport_active,
            NavigationOperationCompletion::test_transport_failure(NavigationOperationKind::Observe),
            &mut projection,
            0,
        ) else {
            panic!("lost continuation observation must fail");
        };
        assert!(transport.fail_stop);
        assert!(transport.error.fatal);
        assert_eq!(transport.error.state_effect, "partial");
    }

    #[test]
    fn explicit_navigation_requires_the_admitted_replacement_activation() {
        let source = session_authority(1, 0, 0, 0);
        let admitted = replacement_admission_authority(&source);
        let activated_source_retained =
            activated_replacement_source_retained_authority(&source, &admitted);
        let activated = activated_replacement_authority(&source, &admitted);
        assert_eq!(
            exact_replacement_admission(&source, &admitted),
            Some(ReplacementAdmission {
                source_pipeline_id: source.target().active_top_level.unwrap().pipeline_id,
                pipeline_id: admitted.target().pending_top_level_pipelines()[0],
            })
        );
        assert!(explicit_navigation_reached_controlled_ready(
            &source, &admitted, &activated,
        ));
        assert_eq!(
            classify_replacement_activation_observation(&source, &admitted, &admitted),
            ReplacementActivationObservation::Pending,
        );
        assert_eq!(
            classify_replacement_activation_observation(
                &source,
                &admitted,
                &activated_source_retained,
            ),
            ReplacementActivationObservation::ActivatedAwaitingSourceExit,
        );
        assert_eq!(
            classify_replacement_activation_observation(&source, &admitted, &activated),
            ReplacementActivationObservation::ControlledReady,
        );
        assert!(!explicit_navigation_reached_controlled_ready(
            &source, &admitted, &source,
        ));
        assert_eq!(
            classify_replacement_activation_observation(&source, &admitted, &source),
            ReplacementActivationObservation::Invalid,
        );
        assert!(initial_navigation_reached_controlled_ready(&source));
        assert!(!initial_navigation_reached_controlled_ready(&activated));

        let revision_gap = SessionNavigationAuthority::new_internal(
            Box::new(admitted.target().clone()),
            admitted.document_epoch(),
            SessionNavigationId::new(source.navigation_id().get() + 2),
            admitted.history_revision(),
            admitted.successful_document_replacements(),
            admitted.url().clone(),
            None,
        );
        assert_eq!(exact_replacement_admission(&source, &revision_gap), None);
    }

    #[test]
    fn reserved_navigation_start_failure_is_fatal_and_does_not_echo_schemes() {
        let observed = session_authority(1, 1, 0, 0);
        let ActiveTransition::Fail(start_failed) = session_navigation_failure(
            SessionNavigationError::NavigationStartFailed {
                observed: Box::new(observed),
            },
            NavigationOperationKind::Navigate,
            RequestStateEffect::None,
        ) else {
            panic!("navigation start failure must fail");
        };
        assert!(start_failed.fail_stop);
        assert!(start_failed.error.fatal);
        assert_eq!(start_failed.error.state_effect, "partial");
        assert_eq!(start_failed.error.details.unwrap()["navigationId"], "1");

        let secret_scheme = "SECRET-SCHEME-CANARY";
        let ActiveTransition::Fail(unsupported) = session_navigation_failure(
            SessionNavigationError::UnsupportedScheme {
                scheme: secret_scheme.into(),
            },
            NavigationOperationKind::Observe,
            RequestStateEffect::Partial,
        ) else {
            panic!("unsupported scheme must fail");
        };
        assert!(unsupported.fail_stop);
        assert!(unsupported.error.fatal);
        assert!(!unsupported.error.message.contains(secret_scheme));
    }

    #[test]
    fn app_terminal_evidence_never_records_false_commit_or_settlement() {
        let mut bytes = Vec::new();
        let evidence;
        {
            let mut shell = shell(
                &mut bytes,
                ShellState::Open,
                Some(FakeEngine::controlled_session()),
            );
            let mut fill = request("action.fill", Some(SESSION_ID));
            fill.params = json!({
                "selector": "#email",
                "value": "person@example.test",
                "expectedStateToken": test_document_token(1),
            });
            assert!(!shell.handle(fill).unwrap());
            shell.active.as_mut().unwrap().state_effect = RequestStateEffect::Partial;
            evidence = shell.engine.as_ref().unwrap().evidence.clone();
            shell.engine.as_mut().unwrap().navigation_polls.push_back(
                EnginePortNavigationPoll::Complete(NavigationOperationCompletion::test_response(
                    NavigationOperationKind::Observe,
                    Err(SessionNavigationError::Terminal(
                        SessionNavigationTerminal::HistoryLimitExceeded {
                            limit: 10_000,
                            observed: 10_000,
                            navigation_id: SessionNavigationId::new(7),
                            history_revision:
                                embedder_traits::document_session::HistoryRevision::new(10_000),
                        },
                    )),
                )),
            );
            assert!(shell.poll_active_navigation().is_err());
            assert_eq!(shell.state, ShellState::Closed);
        }
        assert_eq!(&*evidence.borrow(), &[("failed", 7)]);
        let response = frames(&bytes).pop().unwrap();
        assert_eq!(response["error"]["code"], "history_limit_exceeded");
        assert_eq!(response["error"]["stateEffect"], "partial");
        assert_eq!(response["error"]["fatal"], true);
    }
}
