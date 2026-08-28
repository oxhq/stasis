/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Opaque controlled-session network authority shared by Servo and product embedders.
//!
//! The immutable fixture table is the only response authority. This state has no wall clock,
//! callback, filesystem, or network capability; Servo supplies checked request facts and reports
//! terminal lifecycle facts on its owner lane.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use embedder_traits::{
    ControlledCookiePolicy, DocumentExecutionProfile, SessionNavigationId, WebResourceLoadId,
};
use parking_lot::Mutex;
use serde::Serialize;
use url::Url;

use crate::network_evidence::{
    EvidenceLedgerBounds, EvidenceLedgerError, EvidenceResourceKind, EvidenceSequence,
    NavigationId, NetworkEvidenceEvent, NetworkEvidenceLedger, NetworkEvidencePage,
    NetworkFailureReason, NetworkRequestId, NetworkRequestsPage, RedactedRequest,
    RedactedRequestInput, RouteEvidenceDecision,
};
use crate::network_fixture::{
    FixtureDecision, FulfillResponse, NetworkFixtureError, NetworkFixtureTable,
};

pub const MAX_CONTROLLED_NETWORK_ACTIVE_OPERATIONS: usize = 512;

/// Completed redirect hops whose Servo navigation replay has not begun yet. Top-level navigation
/// redirects cross an asynchronous `FetchRedirect` boundary, but the successor request and the
/// predecessor terminal can arrive in either order. Retaining only this opaque hop identity keeps
/// that immediate lineage available without retaining request metadata or allowing unbounded
/// growth.
const MAX_RETIRED_REDIRECT_PREDECESSORS: usize = MAX_CONTROLLED_NETWORK_ACTIVE_OPERATIONS;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlledNetworkFailure {
    FixtureMiss,
    ActiveOperationLimitExceeded,
    UnknownRequestBodyLength,
    RequestMetadataRejected,
    EvidenceLedgerFailure,
    LifecycleInvariant,
    VirtualTimeRegressed,
    CookieSameSiteContextUnsupported,
    PersistentCookieUnsupported,
    PartitionedCookieUnsupported,
    CookieTimeRangeUnsupported,
    InvalidCookie,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlledNetworkTimeError {
    Regressed { current: u128, requested: u128 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlledNetworkSnapshot {
    pub active_operations: usize,
    pub maximum_active_operations: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sticky_failure: Option<ControlledNetworkFailure>,
    pub current_virtual_time_ns: String,
}

pub struct ControlledNetworkRequest<'a> {
    pub load_id: WebResourceLoadId,
    pub method: &'a str,
    pub url: &'a Url,
    pub resource_kind: EvidenceResourceKind,
    pub main_frame: bool,
    pub header_names: &'a [&'a str],
    /// `None` means a streaming body whose exact length is not proven.
    pub body_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlledNetworkRequestHandle {
    load_id: WebResourceLoadId,
    request_id: NetworkRequestId,
}

impl ControlledNetworkRequestHandle {
    pub const fn load_id(self) -> WebResourceLoadId {
        self.load_id
    }

    pub const fn request_id(self) -> NetworkRequestId {
        self.request_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlledNetworkAbortReason {
    FixtureAbort,
    FixtureMiss,
    LimitExceeded,
    UnsupportedRequestMetadata,
    CookiePolicyRejected,
    InternalEvidenceFailure,
}

/// A fixed decision. Fulfillment bytes deliberately remain absent from `Debug` surfaces.
pub enum ControlledNetworkAction {
    Fulfill {
        handle: ControlledNetworkRequestHandle,
        response: FulfillResponse,
    },
    Abort {
        handle: Option<ControlledNetworkRequestHandle>,
        reason: ControlledNetworkAbortReason,
    },
    Passthrough {
        handle: ControlledNetworkRequestHandle,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlledNetworkTerminal {
    Completed { status: u16, response_bytes: u64 },
    Failed,
    CookiePolicyRejected(ControlledNetworkCookieFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlledNetworkCookieFailure {
    SameSiteContextUnsupported,
    PersistentCookieUnsupported,
    PartitionedCookieUnsupported,
    TimeRangeUnsupported,
    InvalidCookie,
}

#[derive(Clone, Copy, Eq, PartialEq)]
struct ActiveRequest {
    request_id: NetworkRequestId,
    // The successor has consumed this request's lineage, but its terminal evidence is still due.
    redirect_successor_claimed: bool,
}

struct ControlledNetworkInner {
    fixtures: NetworkFixtureTable,
    evidence: NetworkEvidenceLedger,
    active: BTreeMap<WebResourceLoadId, ActiveRequest>,
    retired_redirect_predecessors: BTreeMap<WebResourceLoadId, NetworkRequestId>,
    retired_redirect_order: VecDeque<WebResourceLoadId>,
    virtual_time_ns: u128,
    controlled_cookie_v2: bool,
    sticky_failure: Option<ControlledNetworkFailure>,
}

/// Cloneable opaque session. All mutation is serialized on Servo's owner lane; the mutex makes
/// snapshots safe without allowing callers to mutate the immutable route table.
#[derive(Clone)]
pub struct ControlledNetworkSession(Arc<Mutex<ControlledNetworkInner>>);

impl ControlledNetworkSession {
    pub fn from_json(
        fixture_value: serde_json::Value,
        initial_virtual_time_ns: u128,
    ) -> Result<Self, NetworkFixtureError> {
        Self::from_json_with_execution_profile(
            fixture_value,
            initial_virtual_time_ns,
            DocumentExecutionProfile::Baseline,
        )
    }

    pub fn from_json_with_execution_profile(
        fixture_value: serde_json::Value,
        initial_virtual_time_ns: u128,
        execution_profile: DocumentExecutionProfile,
    ) -> Result<Self, NetworkFixtureError> {
        Ok(Self::new_with_execution_profile(
            NetworkFixtureTable::from_json(fixture_value)?,
            initial_virtual_time_ns,
            execution_profile,
        ))
    }

    pub fn new(fixtures: NetworkFixtureTable, initial_virtual_time_ns: u128) -> Self {
        Self::new_with_execution_profile(
            fixtures,
            initial_virtual_time_ns,
            DocumentExecutionProfile::Baseline,
        )
    }

    pub fn new_with_execution_profile(
        fixtures: NetworkFixtureTable,
        initial_virtual_time_ns: u128,
        execution_profile: DocumentExecutionProfile,
    ) -> Self {
        Self(Arc::new(Mutex::new(ControlledNetworkInner {
            fixtures,
            evidence: NetworkEvidenceLedger::new(EvidenceLedgerBounds::default()),
            active: BTreeMap::new(),
            retired_redirect_predecessors: BTreeMap::new(),
            retired_redirect_order: VecDeque::new(),
            virtual_time_ns: initial_virtual_time_ns,
            controlled_cookie_v2: execution_profile
                == DocumentExecutionProfile::ControlledWebSessionV2,
            sticky_failure: None,
        })))
    }

    pub fn set_virtual_time_ns(&self, requested: u128) -> Result<(), ControlledNetworkTimeError> {
        let mut inner = self.0.lock();
        if requested < inner.virtual_time_ns {
            let current = inner.virtual_time_ns;
            inner.sticky_failure = Some(ControlledNetworkFailure::VirtualTimeRegressed);
            return Err(ControlledNetworkTimeError::Regressed { current, requested });
        }
        inner.virtual_time_ns = requested;
        Ok(())
    }

    pub fn begin(&self, request: ControlledNetworkRequest<'_>) -> ControlledNetworkAction {
        let mut inner = self.0.lock();
        inner.begin(request, None)
    }

    /// Admit one request and capture the exact cookie clock from the same owner serialization
    /// point. V2 also proves the captured schemeful site and u64 cookie-clock domain before route
    /// selection. A preflight rejection records one bounded failed request and cannot reach either
    /// a fixture response or live passthrough. V1 deliberately ignores these added checks.
    pub fn begin_with_cookie_policy(
        &self,
        request: ControlledNetworkRequest<'_>,
        site_for_cookies: Option<&Url>,
    ) -> (ControlledNetworkAction, ControlledCookiePolicy) {
        let mut inner = self.0.lock();
        let policy = inner.cookie_policy();
        let preflight_failure = controlled_cookie_preflight_failure(policy, site_for_cookies);
        (inner.begin(request, preflight_failure), policy)
    }

    /// Return the current policy for a synchronous privileged state boundary.
    pub fn cookie_policy(&self) -> ControlledCookiePolicy {
        self.0.lock().cookie_policy()
    }

    /// Report a Net-owned terminal. A redirect successor and its predecessor terminal may arrive
    /// in either order; successor-first predecessors remain active until their terminal is known.
    pub fn live_terminal(&self, load_id: WebResourceLoadId, terminal: ControlledNetworkTerminal) {
        let mut inner = self.0.lock();
        let Some(active) = inner.active.remove(&load_id) else {
            return;
        };
        match terminal {
            ControlledNetworkTerminal::Completed {
                status,
                response_bytes: _,
            } => {
                inner.record(NetworkEvidenceEvent::ResponseHeaders {
                    request_id: active.request_id,
                    status,
                });
                inner.record(NetworkEvidenceEvent::RequestCompleted {
                    request_id: active.request_id,
                });
                if active.redirect_successor_claimed && !is_redirect_status(status) {
                    inner.latch(ControlledNetworkFailure::LifecycleInvariant);
                } else if is_redirect_status(status) && !active.redirect_successor_claimed {
                    inner.retain_redirect_predecessor(load_id, active.request_id);
                }
            },
            ControlledNetworkTerminal::Failed => {
                inner.record(NetworkEvidenceEvent::RequestFailed {
                    request_id: active.request_id,
                    reason: NetworkFailureReason::NetworkError,
                });
                if active.redirect_successor_claimed {
                    inner.latch(ControlledNetworkFailure::LifecycleInvariant);
                }
            },
            ControlledNetworkTerminal::CookiePolicyRejected(failure) => {
                inner.record(NetworkEvidenceEvent::RequestFailed {
                    request_id: active.request_id,
                    reason: NetworkFailureReason::NetworkError,
                });
                if active.redirect_successor_claimed {
                    inner.latch(ControlledNetworkFailure::LifecycleInvariant);
                }
                inner.latch(controlled_network_failure_for_cookie(failure));
            },
        }
    }

    pub fn snapshot(&self) -> ControlledNetworkSnapshot {
        let inner = self.0.lock();
        ControlledNetworkSnapshot {
            active_operations: inner.active.len(),
            maximum_active_operations: MAX_CONTROLLED_NETWORK_ACTIVE_OPERATIONS,
            sticky_failure: inner.sticky_failure,
            current_virtual_time_ns: inner.virtual_time_ns.to_string(),
        }
    }

    pub fn requests_page(
        &self,
        after: Option<EvidenceSequence>,
        limit: usize,
    ) -> Result<NetworkRequestsPage, EvidenceLedgerError> {
        self.0.lock().evidence.requests_page(after, limit)
    }

    pub fn evidence_page(
        &self,
        after: Option<EvidenceSequence>,
        limit: usize,
    ) -> Result<NetworkEvidencePage, EvidenceLedgerError> {
        self.0.lock().evidence.evidence_page(after, limit)
    }

    /// Append bounded evidence for an owner-authorized top-level navigation attempt. The ID is
    /// allocated by Constellation; this diagnostics ledger never creates a competing identity.
    pub fn record_navigation_started(&self, navigation_id: SessionNavigationId) {
        self.0
            .lock()
            .record(NetworkEvidenceEvent::NavigationStarted {
                navigation_id: NavigationId::from(navigation_id),
            });
    }

    pub fn record_navigation_committed(&self, navigation_id: SessionNavigationId) {
        self.0
            .lock()
            .record(NetworkEvidenceEvent::NavigationCommitted {
                navigation_id: NavigationId::from(navigation_id),
            });
    }

    pub fn record_navigation_failed(
        &self,
        navigation_id: SessionNavigationId,
        reason: NetworkFailureReason,
    ) {
        self.0
            .lock()
            .record(NetworkEvidenceEvent::NavigationFailed {
                navigation_id: NavigationId::from(navigation_id),
                reason,
            });
    }

    pub fn record_same_document_history_changed(&self, navigation_id: SessionNavigationId) {
        self.0
            .lock()
            .record(NetworkEvidenceEvent::SameDocumentHistoryChanged {
                navigation_id: NavigationId::from(navigation_id),
            });
    }

    pub fn record_settlement_terminal(&self, navigation_id: SessionNavigationId) {
        self.0
            .lock()
            .record(NetworkEvidenceEvent::SettlementTerminal {
                navigation_id: NavigationId::from(navigation_id),
            });
    }
}

impl ControlledNetworkInner {
    fn cookie_policy(&self) -> ControlledCookiePolicy {
        if self.controlled_cookie_v2 {
            ControlledCookiePolicy::SessionV2 {
                unix_time_ns: self.virtual_time_ns,
            }
        } else {
            ControlledCookiePolicy::SessionV1
        }
    }

    fn begin(
        &mut self,
        request: ControlledNetworkRequest<'_>,
        cookie_preflight_failure: Option<ControlledNetworkCookieFailure>,
    ) -> ControlledNetworkAction {
        if self.sticky_failure.is_some() {
            return ControlledNetworkAction::Abort {
                handle: None,
                reason: ControlledNetworkAbortReason::InternalEvidenceFailure,
            };
        }
        if self.active.contains_key(&request.load_id)
            || self
                .retired_redirect_predecessors
                .contains_key(&request.load_id)
        {
            self.latch(ControlledNetworkFailure::LifecycleInvariant);
            return ControlledNetworkAction::Abort {
                handle: None,
                reason: ControlledNetworkAbortReason::InternalEvidenceFailure,
            };
        }

        let redirect_parent_id = request
            .load_id
            .redirect_parent()
            .and_then(|parent| self.take_redirect_predecessor(parent));
        if self.active.len() >= MAX_CONTROLLED_NETWORK_ACTIVE_OPERATIONS {
            self.latch(ControlledNetworkFailure::ActiveOperationLimitExceeded);
            return ControlledNetworkAction::Abort {
                handle: None,
                reason: ControlledNetworkAbortReason::LimitExceeded,
            };
        }
        let Some(body_bytes) = request.body_bytes else {
            self.latch(ControlledNetworkFailure::UnknownRequestBodyLength);
            return ControlledNetworkAction::Abort {
                handle: None,
                reason: ControlledNetworkAbortReason::UnsupportedRequestMetadata,
            };
        };
        let request_id = match self.evidence.allocate_request_id() {
            Ok(request_id) => request_id,
            Err(_) => {
                self.latch(ControlledNetworkFailure::EvidenceLedgerFailure);
                return ControlledNetworkAction::Abort {
                    handle: None,
                    reason: ControlledNetworkAbortReason::InternalEvidenceFailure,
                };
            },
        };
        let redacted = match RedactedRequest::new(RedactedRequestInput {
            request_id,
            redirect_parent_id,
            method: request.method,
            url: request.url,
            resource_kind: request.resource_kind,
            main_frame: request.main_frame,
            header_names: request.header_names,
            body_bytes,
        }) {
            Ok(redacted) => redacted,
            Err(_) => {
                self.latch(ControlledNetworkFailure::RequestMetadataRejected);
                return ControlledNetworkAction::Abort {
                    handle: None,
                    reason: ControlledNetworkAbortReason::UnsupportedRequestMetadata,
                };
            },
        };
        if let Some(parent_id) = redirect_parent_id {
            self.record(NetworkEvidenceEvent::Redirect {
                request_id: parent_id,
                next_request_id: request_id,
            });
        }
        if self
            .evidence
            .record_request_started(self.virtual_time_ns, redacted)
            .is_err()
        {
            self.latch(ControlledNetworkFailure::EvidenceLedgerFailure);
            return ControlledNetworkAction::Abort {
                handle: None,
                reason: ControlledNetworkAbortReason::InternalEvidenceFailure,
            };
        }

        let handle = ControlledNetworkRequestHandle {
            load_id: request.load_id,
            request_id,
        };
        if let Some(failure) = cookie_preflight_failure {
            self.record(NetworkEvidenceEvent::RequestFailed {
                request_id,
                reason: NetworkFailureReason::NetworkError,
            });
            self.latch(controlled_network_failure_for_cookie(failure));
            return ControlledNetworkAction::Abort {
                handle: Some(handle),
                reason: ControlledNetworkAbortReason::CookiePolicyRejected,
            };
        }
        let decision = match self.fixtures.decide(request.method, request.url) {
            Ok(decision) => decision,
            Err(_) => {
                self.record(NetworkEvidenceEvent::RequestFailed {
                    request_id,
                    reason: NetworkFailureReason::NetworkError,
                });
                self.latch(ControlledNetworkFailure::RequestMetadataRejected);
                return ControlledNetworkAction::Abort {
                    handle: Some(handle),
                    reason: ControlledNetworkAbortReason::UnsupportedRequestMetadata,
                };
            },
        };
        match decision {
            FixtureDecision::Fulfill { response, .. } => {
                let response = response.clone();
                self.record(NetworkEvidenceEvent::RouteDecided {
                    request_id,
                    decision: RouteEvidenceDecision::FixtureFulfill,
                });
                self.active.insert(
                    request.load_id,
                    ActiveRequest {
                        request_id,
                        redirect_successor_claimed: false,
                    },
                );
                ControlledNetworkAction::Fulfill { handle, response }
            },
            FixtureDecision::Abort { abort, .. } => {
                let reason = match abort.reason() {
                    "blocked_by_fixture" => NetworkFailureReason::BlockedByFixture,
                    "connection_reset" => NetworkFailureReason::ConnectionReset,
                    "network_error" => NetworkFailureReason::NetworkError,
                    _ => unreachable!("fixture abort reason was validated before WebView creation"),
                };
                self.record(NetworkEvidenceEvent::RouteDecided {
                    request_id,
                    decision: RouteEvidenceDecision::FixtureAbort,
                });
                self.record(NetworkEvidenceEvent::RequestFailed { request_id, reason });
                ControlledNetworkAction::Abort {
                    handle: Some(handle),
                    reason: ControlledNetworkAbortReason::FixtureAbort,
                }
            },
            FixtureDecision::StrictMiss => {
                self.record(NetworkEvidenceEvent::RequestFailed {
                    request_id,
                    reason: NetworkFailureReason::FixtureMiss,
                });
                self.latch(ControlledNetworkFailure::FixtureMiss);
                ControlledNetworkAction::Abort {
                    handle: Some(handle),
                    reason: ControlledNetworkAbortReason::FixtureMiss,
                }
            },
            FixtureDecision::Passthrough => {
                self.record(NetworkEvidenceEvent::RouteDecided {
                    request_id,
                    decision: RouteEvidenceDecision::Live,
                });
                self.active.insert(
                    request.load_id,
                    ActiveRequest {
                        request_id,
                        redirect_successor_claimed: false,
                    },
                );
                ControlledNetworkAction::Passthrough { handle }
            },
        }
    }

    fn record(&mut self, event: NetworkEvidenceEvent) {
        if self
            .evidence
            .record_event(self.virtual_time_ns, event)
            .is_err()
        {
            self.latch(ControlledNetworkFailure::EvidenceLedgerFailure);
        }
    }

    fn take_redirect_predecessor(
        &mut self,
        load_id: WebResourceLoadId,
    ) -> Option<NetworkRequestId> {
        if let Some(active) = self.active.get_mut(&load_id) {
            active.redirect_successor_claimed = true;
            return Some(active.request_id);
        }
        let request_id = self.retired_redirect_predecessors.remove(&load_id)?;
        self.retired_redirect_order
            .retain(|retained| *retained != load_id);
        Some(request_id)
    }

    fn retain_redirect_predecessor(
        &mut self,
        load_id: WebResourceLoadId,
        request_id: NetworkRequestId,
    ) {
        while self.retired_redirect_predecessors.len() >= MAX_RETIRED_REDIRECT_PREDECESSORS {
            let Some(oldest) = self.retired_redirect_order.pop_front() else {
                self.latch(ControlledNetworkFailure::LifecycleInvariant);
                return;
            };
            self.retired_redirect_predecessors.remove(&oldest);
        }
        if self
            .retired_redirect_predecessors
            .insert(load_id, request_id)
            .is_some()
        {
            self.latch(ControlledNetworkFailure::LifecycleInvariant);
            return;
        }
        self.retired_redirect_order.push_back(load_id);
    }

    fn latch(&mut self, failure: ControlledNetworkFailure) {
        if self.sticky_failure.is_none() {
            self.sticky_failure = Some(failure);
        }
    }
}

const fn is_redirect_status(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

const fn controlled_network_failure_for_cookie(
    failure: ControlledNetworkCookieFailure,
) -> ControlledNetworkFailure {
    match failure {
        ControlledNetworkCookieFailure::SameSiteContextUnsupported => {
            ControlledNetworkFailure::CookieSameSiteContextUnsupported
        },
        ControlledNetworkCookieFailure::PersistentCookieUnsupported => {
            ControlledNetworkFailure::PersistentCookieUnsupported
        },
        ControlledNetworkCookieFailure::PartitionedCookieUnsupported => {
            ControlledNetworkFailure::PartitionedCookieUnsupported
        },
        ControlledNetworkCookieFailure::TimeRangeUnsupported => {
            ControlledNetworkFailure::CookieTimeRangeUnsupported
        },
        ControlledNetworkCookieFailure::InvalidCookie => ControlledNetworkFailure::InvalidCookie,
    }
}

fn controlled_cookie_preflight_failure(
    policy: ControlledCookiePolicy,
    site_for_cookies: Option<&Url>,
) -> Option<ControlledNetworkCookieFailure> {
    let ControlledCookiePolicy::SessionV2 { unix_time_ns } = policy else {
        return None;
    };
    // Preserve the cookie-selection validation order: an unprovable schemeful context wins over
    // a simultaneous clock-range failure. Both checks still happen before fixture route choice.
    if site_for_cookies
        .is_none_or(|site| !matches!(site.scheme(), "http" | "https") || !site.origin().is_tuple())
    {
        return Some(ControlledNetworkCookieFailure::SameSiteContextUnsupported);
    }
    if u64::try_from(unix_time_ns).is_err() {
        return Some(ControlledNetworkCookieFailure::TimeRangeUnsupported);
    }
    None
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::*;

    fn fixture_session(mode: &str, routes: Value) -> ControlledNetworkSession {
        ControlledNetworkSession::from_json(
            json!({
                "mode": mode,
                "routes": routes,
            }),
            7,
        )
        .unwrap()
    }

    fn request<'a>(
        identity: u8,
        redirect_index: u32,
        url: &'a Url,
        body_bytes: Option<u64>,
    ) -> ControlledNetworkRequest<'a> {
        ControlledNetworkRequest {
            load_id: WebResourceLoadId::new([identity; 16], redirect_index),
            method: "GET",
            url,
            resource_kind: EvidenceResourceKind::Fetch,
            main_frame: false,
            header_names: &["accept", "authorization"],
            body_bytes,
        }
    }

    #[test]
    fn fixtures_only_miss_is_sticky_and_never_passes_through() {
        let session = fixture_session("fixtures_only", json!([]));
        let url = Url::parse("https://example.test/private?token=secret").unwrap();

        assert!(matches!(
            session.begin(request(1, 0, &url, Some(0))),
            ControlledNetworkAction::Abort {
                handle: Some(_),
                reason: ControlledNetworkAbortReason::FixtureMiss,
            }
        ));
        assert_eq!(
            session.snapshot().sticky_failure,
            Some(ControlledNetworkFailure::FixtureMiss)
        );
        assert!(matches!(
            session.begin(request(2, 0, &url, Some(0))),
            ControlledNetworkAction::Abort {
                handle: None,
                reason: ControlledNetworkAbortReason::InternalEvidenceFailure,
            }
        ));

        let evidence = serde_json::to_value(session.evidence_page(None, 16).unwrap()).unwrap();
        let encoded = evidence.to_string();
        assert!(encoded.contains("fixture_miss"));
        assert!(!encoded.contains("secret"));
    }

    #[test]
    fn fixture_abort_preserves_its_allow_listed_public_reason() {
        for (identity, reason) in [
            (11, "blocked_by_fixture"),
            (12, "connection_reset"),
            (13, "network_error"),
        ] {
            let session = fixture_session(
                "fixtures_only",
                json!([{
                    "match": {"method": "GET", "url": {"exact": "https://example.test/abort"}},
                    "abort": {"reason": reason}
                }]),
            );
            let url = Url::parse("https://example.test/abort").unwrap();
            assert!(matches!(
                session.begin(request(identity, 0, &url, Some(0))),
                ControlledNetworkAction::Abort {
                    handle: Some(_),
                    reason: ControlledNetworkAbortReason::FixtureAbort,
                }
            ));
            let encoded = serde_json::to_string(&session.evidence_page(None, 16).unwrap()).unwrap();
            assert!(encoded.contains(reason));
        }
    }

    #[test]
    fn fixed_fulfillment_stays_active_until_net_reports_terminal() {
        let session = fixture_session(
            "fixtures_only",
            json!([{
                "match": {"method": "GET", "url": {"exact": "https://example.test/data"}},
                "fulfill": {"status": 201, "body": {"utf8": "fixed"}}
            }]),
        );
        let url = Url::parse("https://example.test/data").unwrap();
        let (load_id, handle) = match session.begin(request(3, 0, &url, Some(0))) {
            ControlledNetworkAction::Fulfill { handle, response } => {
                assert_eq!(response.status(), 201);
                assert_eq!(response.body(), b"fixed");
                (handle.load_id(), handle)
            },
            _ => panic!("expected fixed fulfillment"),
        };
        assert_eq!(session.snapshot().active_operations, 1);
        assert_eq!(handle.load_id(), load_id);

        session.live_terminal(
            load_id,
            ControlledNetworkTerminal::Completed {
                status: 201,
                response_bytes: 5,
            },
        );
        assert_eq!(session.snapshot().active_operations, 0);
        let encoded = serde_json::to_string(&session.evidence_page(None, 16).unwrap()).unwrap();
        assert!(encoded.contains("fixture_fulfill"));
        assert!(encoded.contains("response_headers"));
        assert!(encoded.contains("request_completed"));
    }

    #[test]
    fn mixed_passthrough_has_one_checked_live_lifecycle() {
        let session = fixture_session("mixed", json!([]));
        let url = Url::parse("https://example.test/live").unwrap();
        let handle = match session.begin(request(4, 0, &url, Some(0))) {
            ControlledNetworkAction::Passthrough { handle } => handle,
            _ => panic!("expected live passthrough"),
        };
        assert_eq!(session.snapshot().active_operations, 1);
        session.live_terminal(handle.load_id(), ControlledNetworkTerminal::Failed);
        assert_eq!(session.snapshot().active_operations, 0);
        let encoded = serde_json::to_string(&session.evidence_page(None, 16).unwrap()).unwrap();
        assert!(encoded.contains("\"decision\":\"live\""));
        assert!(encoded.contains("network_error"));
    }

    #[test]
    fn cookie_policy_rejection_is_terminal_and_secret_safe() {
        let session = fixture_session("mixed", json!([]));
        let url = Url::parse("https://example.test/live").unwrap();
        let handle = match session.begin(request(8, 0, &url, Some(0))) {
            ControlledNetworkAction::Passthrough { handle } => handle,
            _ => panic!("expected live passthrough"),
        };
        session.live_terminal(
            handle.load_id(),
            ControlledNetworkTerminal::CookiePolicyRejected(
                ControlledNetworkCookieFailure::PersistentCookieUnsupported,
            ),
        );

        assert_eq!(
            session.snapshot().sticky_failure,
            Some(ControlledNetworkFailure::PersistentCookieUnsupported)
        );
        assert_eq!(session.snapshot().active_operations, 0);
        let encoded = serde_json::to_string(&session.evidence_page(None, 16).unwrap()).unwrap();
        assert!(encoded.contains("network_error"));
        assert!(!encoded.contains("cookie"));
    }

    #[test]
    fn redirect_successor_before_terminal_retains_and_links_its_stable_parent() {
        let session = fixture_session("live", json!([]));
        let first_url = Url::parse("https://example.test/start").unwrap();
        let next_url = Url::parse("https://example.test/next").unwrap();
        let first = match session.begin(request(5, 0, &first_url, Some(0))) {
            ControlledNetworkAction::Passthrough { handle } => handle,
            _ => panic!("expected live predecessor"),
        };
        let successor = match session.begin(request(5, 1, &next_url, Some(0))) {
            ControlledNetworkAction::Passthrough { handle } => handle,
            _ => panic!("expected live successor"),
        };
        assert_eq!(session.snapshot().active_operations, 2);

        session.live_terminal(
            first.load_id(),
            ControlledNetworkTerminal::Completed {
                status: 302,
                response_bytes: 0,
            },
        );
        assert_eq!(session.snapshot().active_operations, 1);

        let requests = serde_json::to_value(session.requests_page(None, 16).unwrap()).unwrap();
        assert_eq!(requests["records"][1]["redirectParentId"], "1");
        let evidence = serde_json::to_value(session.evidence_page(None, 16).unwrap()).unwrap();
        let records = evidence["records"].as_array().unwrap();
        assert!(
            records
                .iter()
                .any(|record| { record["kind"] == "redirect" && record["nextRequestId"] == "2" })
        );
        assert!(records.iter().any(|record| {
            record["kind"] == "response_headers"
                && record["requestId"] == "1"
                && record["status"] == 302
        }));
        assert!(
            records.iter().any(|record| {
                record["kind"] == "request_completed" && record["requestId"] == "1"
            })
        );

        {
            let inner = session.0.lock();
            assert!(inner.retired_redirect_predecessors.is_empty());
            assert!(inner.retired_redirect_order.is_empty());
        }
        session.live_terminal(
            successor.load_id(),
            ControlledNetworkTerminal::Completed {
                status: 200,
                response_bytes: 0,
            },
        );
        assert_eq!(session.snapshot().active_operations, 0);
    }

    #[test]
    fn redirect_successor_before_non_redirect_terminal_fails_closed() {
        let session = fixture_session("live", json!([]));
        let first_url = Url::parse("https://example.test/start").unwrap();
        let next_url = Url::parse("https://example.test/next").unwrap();
        let first = match session.begin(request(13, 0, &first_url, Some(0))) {
            ControlledNetworkAction::Passthrough { handle } => handle,
            _ => panic!("expected live predecessor"),
        };
        assert!(matches!(
            session.begin(request(13, 1, &next_url, Some(0))),
            ControlledNetworkAction::Passthrough { .. }
        ));

        session.live_terminal(
            first.load_id(),
            ControlledNetworkTerminal::Completed {
                status: 200,
                response_bytes: 0,
            },
        );

        let snapshot = session.snapshot();
        assert_eq!(snapshot.active_operations, 1);
        assert_eq!(
            snapshot.sticky_failure,
            Some(ControlledNetworkFailure::LifecycleInvariant)
        );
    }

    #[test]
    fn navigation_redirect_terminal_remains_linkable_until_replay_begins() {
        let session = fixture_session("live", json!([]));
        let first_url = Url::parse("https://example.test/start?secret=one").unwrap();
        let next_url = Url::parse("https://example.test/next?secret=two").unwrap();
        let first = match session.begin(request(14, 0, &first_url, Some(0))) {
            ControlledNetworkAction::Passthrough { handle } => handle,
            _ => panic!("expected live predecessor"),
        };

        // Exercise the terminal-first half of the two accepted redirect notification orders.
        session.live_terminal(
            first.load_id(),
            ControlledNetworkTerminal::Completed {
                status: 302,
                response_bytes: 0,
            },
        );
        assert_eq!(session.snapshot().active_operations, 0);
        assert!(matches!(
            session.begin(request(14, 1, &next_url, Some(0))),
            ControlledNetworkAction::Passthrough { .. }
        ));

        let requests = serde_json::to_value(session.requests_page(None, 16).unwrap()).unwrap();
        assert_eq!(requests["records"][0]["url"]["path"], "/start");
        assert_eq!(requests["records"][1]["url"]["path"], "/next");
        assert_eq!(requests["records"][1]["redirectParentId"], "1");
        let encoded = serde_json::to_string(&session.evidence_page(None, 16).unwrap()).unwrap();
        assert!(encoded.contains("\"status\":302"));
        assert!(encoded.contains("\"kind\":\"redirect\""));
        assert!(encoded.contains("\"requestId\":\"1\",\"nextRequestId\":\"2\""));
        assert!(!encoded.contains("one"));
        assert!(!encoded.contains("two"));
    }

    #[test]
    fn non_redirect_terminal_cannot_be_claimed_as_redirect_lineage() {
        let session = fixture_session("live", json!([]));
        let first_url = Url::parse("https://example.test/start").unwrap();
        let next_url = Url::parse("https://example.test/next").unwrap();
        let first = match session.begin(request(15, 0, &first_url, Some(0))) {
            ControlledNetworkAction::Passthrough { handle } => handle,
            _ => panic!("expected live predecessor"),
        };
        session.live_terminal(
            first.load_id(),
            ControlledNetworkTerminal::Completed {
                status: 200,
                response_bytes: 0,
            },
        );
        assert!(matches!(
            session.begin(request(15, 1, &next_url, Some(0))),
            ControlledNetworkAction::Passthrough { .. }
        ));

        let requests = serde_json::to_value(session.requests_page(None, 16).unwrap()).unwrap();
        assert!(requests["records"][1].get("redirectParentId").is_none());
        let encoded = serde_json::to_string(&session.evidence_page(None, 16).unwrap()).unwrap();
        assert!(!encoded.contains("\"kind\":\"redirect\""));
    }

    #[test]
    fn retired_redirect_lineage_is_strictly_bounded() {
        let session = fixture_session("live", json!([]));
        let url = Url::parse("https://example.test/redirect").unwrap();
        for index in 0..=MAX_RETIRED_REDIRECT_PREDECESSORS {
            let identity = u64::try_from(index).unwrap().to_be_bytes();
            let mut bytes = [0_u8; 16];
            bytes[..8].copy_from_slice(&identity);
            let action = session.begin(ControlledNetworkRequest {
                load_id: WebResourceLoadId::new(bytes, 0),
                method: "GET",
                url: &url,
                resource_kind: EvidenceResourceKind::Navigation,
                main_frame: true,
                header_names: &[],
                body_bytes: Some(0),
            });
            let handle = match action {
                ControlledNetworkAction::Passthrough { handle } => handle,
                _ => panic!("expected live redirect predecessor"),
            };
            session.live_terminal(
                handle.load_id(),
                ControlledNetworkTerminal::Completed {
                    status: 307,
                    response_bytes: 0,
                },
            );
        }

        let inner = session.0.lock();
        assert_eq!(
            inner.retired_redirect_predecessors.len(),
            MAX_RETIRED_REDIRECT_PREDECESSORS
        );
        assert_eq!(
            inner.retired_redirect_order.len(),
            MAX_RETIRED_REDIRECT_PREDECESSORS
        );
        assert_eq!(inner.sticky_failure, None);
    }

    #[test]
    fn active_operation_cap_fails_closed_before_live_fallback() {
        let session = fixture_session("live", json!([]));
        let url = Url::parse("https://example.test/live").unwrap();
        for index in 0..MAX_CONTROLLED_NETWORK_ACTIVE_OPERATIONS {
            let identity = u64::try_from(index).unwrap().to_be_bytes();
            let mut bytes = [0_u8; 16];
            bytes[..8].copy_from_slice(&identity);
            let action = session.begin(ControlledNetworkRequest {
                load_id: WebResourceLoadId::new(bytes, 0),
                method: "GET",
                url: &url,
                resource_kind: EvidenceResourceKind::Fetch,
                main_frame: false,
                header_names: &[],
                body_bytes: Some(0),
            });
            assert!(matches!(
                action,
                ControlledNetworkAction::Passthrough { .. }
            ));
        }
        assert!(matches!(
            session.begin(request(255, 1, &url, Some(0))),
            ControlledNetworkAction::Abort {
                handle: None,
                reason: ControlledNetworkAbortReason::LimitExceeded,
            }
        ));
        assert_eq!(
            session.snapshot().sticky_failure,
            Some(ControlledNetworkFailure::ActiveOperationLimitExceeded)
        );
    }

    #[test]
    fn unknown_body_length_and_regressed_time_are_sticky_failures() {
        let session = fixture_session("live", json!([]));
        let url = Url::parse("https://example.test/upload").unwrap();
        assert!(matches!(
            session.begin(request(6, 0, &url, None)),
            ControlledNetworkAction::Abort {
                reason: ControlledNetworkAbortReason::UnsupportedRequestMetadata,
                ..
            }
        ));
        assert_eq!(
            session.snapshot().sticky_failure,
            Some(ControlledNetworkFailure::UnknownRequestBodyLength)
        );

        let time_session = fixture_session("live", json!([]));
        assert!(time_session.set_virtual_time_ns(8).is_ok());
        assert_eq!(
            time_session.set_virtual_time_ns(7),
            Err(ControlledNetworkTimeError::Regressed {
                current: 8,
                requested: 7,
            })
        );
        assert_eq!(
            time_session.snapshot().sticky_failure,
            Some(ControlledNetworkFailure::VirtualTimeRegressed)
        );
    }

    #[test]
    fn cookie_policy_captures_v2_time_at_request_admission_and_preserves_v1() {
        let url = Url::parse("https://example.test/data").unwrap();

        let v1 = fixture_session("live", json!([]));
        let (v1_action, v1_policy) =
            v1.begin_with_cookie_policy(request(31, 0, &url, Some(0)), None);
        assert_eq!(v1_policy, ControlledCookiePolicy::SessionV1);
        let v1_handle = match v1_action {
            ControlledNetworkAction::Passthrough { handle } => handle,
            _ => panic!("expected v1 passthrough"),
        };
        v1.live_terminal(v1_handle.load_id(), ControlledNetworkTerminal::Failed);

        let v2 = ControlledNetworkSession::from_json_with_execution_profile(
            json!({"mode": "live", "routes": []}),
            7,
            DocumentExecutionProfile::ControlledWebSessionV2,
        )
        .unwrap();
        assert_eq!(
            v2.cookie_policy(),
            ControlledCookiePolicy::SessionV2 { unix_time_ns: 7 },
        );
        let (v2_action, captured) =
            v2.begin_with_cookie_policy(request(32, 0, &url, Some(0)), Some(&url));
        let v2_handle = match v2_action {
            ControlledNetworkAction::Passthrough { handle } => handle,
            _ => panic!("expected v2 passthrough"),
        };
        v2.set_virtual_time_ns(11).unwrap();
        assert_eq!(
            captured,
            ControlledCookiePolicy::SessionV2 { unix_time_ns: 7 },
        );
        assert_eq!(
            v2.cookie_policy(),
            ControlledCookiePolicy::SessionV2 { unix_time_ns: 11 },
        );
        v2.live_terminal(v2_handle.load_id(), ControlledNetworkTerminal::Failed);
    }

    #[test]
    fn v2_unknown_cookie_site_fails_before_fixture_selection() {
        let request_url = Url::parse("https://example.test/data").unwrap();
        let session = ControlledNetworkSession::from_json_with_execution_profile(
            json!({
                "mode": "fixtures_only",
                "routes": [{
                    "match": {
                        "method": "GET",
                        "url": {"exact": "https://example.test/data"}
                    },
                    "fulfill": {"status": 200, "body": {"utf8": "must-not-start"}}
                }]
            }),
            7,
            DocumentExecutionProfile::ControlledWebSessionV2,
        )
        .unwrap();

        let (action, policy) =
            session.begin_with_cookie_policy(request(33, 0, &request_url, Some(0)), None);
        assert_eq!(
            policy,
            ControlledCookiePolicy::SessionV2 { unix_time_ns: 7 }
        );
        assert!(matches!(
            action,
            ControlledNetworkAction::Abort {
                handle: Some(_),
                reason: ControlledNetworkAbortReason::CookiePolicyRejected,
            }
        ));
        assert_eq!(session.snapshot().active_operations, 0);
        assert_eq!(
            session.snapshot().sticky_failure,
            Some(ControlledNetworkFailure::CookieSameSiteContextUnsupported)
        );

        let requests = serde_json::to_value(session.requests_page(None, 16).unwrap()).unwrap();
        assert_eq!(requests["records"].as_array().unwrap().len(), 1);
        let evidence = serde_json::to_value(session.evidence_page(None, 16).unwrap()).unwrap();
        let records = evidence["records"].as_array().unwrap();
        assert!(
            records
                .iter()
                .any(|record| record["kind"] == "request_started")
        );
        assert!(
            records
                .iter()
                .any(|record| record["kind"] == "request_failed")
        );
        assert!(
            !records
                .iter()
                .any(|record| record["kind"] == "route_decided")
        );
        assert!(
            !records
                .iter()
                .any(|record| record["kind"] == "request_completed")
        );

        let evidence_before_retry = evidence.clone();
        let requests_before_retry = requests.clone();
        assert!(matches!(
            session
                .begin_with_cookie_policy(
                    request(41, 0, &request_url, Some(0)),
                    Some(&request_url),
                )
                .0,
            ControlledNetworkAction::Abort {
                handle: None,
                reason: ControlledNetworkAbortReason::InternalEvidenceFailure,
            }
        ));
        assert_eq!(session.snapshot().active_operations, 0);
        assert_eq!(
            serde_json::to_value(session.evidence_page(None, 16).unwrap()).unwrap(),
            evidence_before_retry,
            "a sticky preflight failure must reject retries before allocating evidence"
        );
        assert_eq!(
            serde_json::to_value(session.requests_page(None, 16).unwrap()).unwrap(),
            requests_before_retry,
            "a sticky preflight failure must reject retries before allocating a request"
        );
    }

    #[test]
    fn v2_opaque_and_non_http_cookie_sites_fail_before_live_passthrough() {
        let request_url = Url::parse("https://example.test/live").unwrap();
        for (identity, site) in [
            (34, Url::parse("data:text/plain,opaque").unwrap()),
            (35, Url::parse("ftp://site.example/file").unwrap()),
        ] {
            let session = ControlledNetworkSession::from_json_with_execution_profile(
                json!({"mode": "live", "routes": []}),
                7,
                DocumentExecutionProfile::ControlledWebSessionV2,
            )
            .unwrap();

            let (action, _) = session
                .begin_with_cookie_policy(request(identity, 0, &request_url, Some(0)), Some(&site));
            assert!(matches!(
                action,
                ControlledNetworkAction::Abort {
                    handle: Some(_),
                    reason: ControlledNetworkAbortReason::CookiePolicyRejected,
                }
            ));
            assert_eq!(session.snapshot().active_operations, 0);
            assert_eq!(
                session.snapshot().sticky_failure,
                Some(ControlledNetworkFailure::CookieSameSiteContextUnsupported)
            );
            let evidence = serde_json::to_value(session.evidence_page(None, 16).unwrap()).unwrap();
            assert!(
                !evidence["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|record| record["kind"] == "route_decided")
            );
        }
    }

    #[test]
    fn v2_cookie_time_range_preflight_is_credentials_independent_with_explicit_precedence() {
        let request_url = Url::parse("https://example.test/live").unwrap();
        let valid_site = Url::parse("https://example.test/page").unwrap();
        let overflow = u128::from(u64::MAX) + 1;

        // Credentials are intentionally absent from this serialized admission seam. Therefore the
        // rejection is structural and applies equally to Include, SameOrigin, and Omit before Net
        // can make a Cookie-header or response-storage decision.
        for (identity, fixture_value) in [
            (36, json!({"mode": "live", "routes": []})),
            (
                39,
                json!({
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {
                            "method": "GET",
                            "url": {"exact": "https://example.test/live"}
                        },
                        "fulfill": {"status": 200, "body": {"utf8": "must-not-start"}}
                    }]
                }),
            ),
        ] {
            let overflow_session = ControlledNetworkSession::from_json_with_execution_profile(
                fixture_value,
                overflow,
                DocumentExecutionProfile::ControlledWebSessionV2,
            )
            .unwrap();
            let (action, _) = overflow_session.begin_with_cookie_policy(
                request(identity, 0, &request_url, Some(0)),
                Some(&valid_site),
            );
            assert!(matches!(
                action,
                ControlledNetworkAction::Abort {
                    handle: Some(_),
                    reason: ControlledNetworkAbortReason::CookiePolicyRejected,
                }
            ));
            assert_eq!(overflow_session.snapshot().active_operations, 0);
            assert_eq!(
                overflow_session.snapshot().sticky_failure,
                Some(ControlledNetworkFailure::CookieTimeRangeUnsupported)
            );
            let evidence =
                serde_json::to_value(overflow_session.evidence_page(None, 16).unwrap()).unwrap();
            assert!(
                !evidence["records"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|record| record["kind"] == "route_decided")
            );
        }

        let maximum = ControlledNetworkSession::from_json_with_execution_profile(
            json!({"mode": "live", "routes": []}),
            u128::from(u64::MAX),
            DocumentExecutionProfile::ControlledWebSessionV2,
        )
        .unwrap();
        assert!(matches!(
            maximum
                .begin_with_cookie_policy(request(40, 0, &request_url, Some(0)), Some(&valid_site),)
                .0,
            ControlledNetworkAction::Passthrough { .. }
        ));
        assert_eq!(maximum.snapshot().sticky_failure, None);

        let both_invalid = ControlledNetworkSession::from_json_with_execution_profile(
            json!({"mode": "live", "routes": []}),
            overflow,
            DocumentExecutionProfile::ControlledWebSessionV2,
        )
        .unwrap();
        assert!(matches!(
            both_invalid
                .begin_with_cookie_policy(request(37, 0, &request_url, Some(0)), None,)
                .0,
            ControlledNetworkAction::Abort {
                reason: ControlledNetworkAbortReason::CookiePolicyRejected,
                ..
            }
        ));
        assert_eq!(
            both_invalid.snapshot().sticky_failure,
            Some(ControlledNetworkFailure::CookieSameSiteContextUnsupported),
            "schemeful-context rejection preserves the existing selection precedence"
        );

        let v1 = ControlledNetworkSession::from_json_with_execution_profile(
            json!({"mode": "live", "routes": []}),
            overflow,
            DocumentExecutionProfile::Baseline,
        )
        .unwrap();
        assert!(matches!(
            v1.begin_with_cookie_policy(request(38, 0, &request_url, Some(0)), None)
                .0,
            ControlledNetworkAction::Passthrough { .. }
        ));
        assert_eq!(v1.snapshot().sticky_failure, None);
    }

    #[test]
    fn rejected_v2_redirect_successor_keeps_lineage_and_leaks_no_operation() {
        let first_url = Url::parse("https://example.test/start").unwrap();
        let next_url = Url::parse("https://example.test/next").unwrap();
        let site = Url::parse("https://example.test/page").unwrap();
        let session = ControlledNetworkSession::from_json_with_execution_profile(
            json!({"mode": "live", "routes": []}),
            7,
            DocumentExecutionProfile::ControlledWebSessionV2,
        )
        .unwrap();

        let first = match session
            .begin_with_cookie_policy(request(42, 0, &first_url, Some(0)), Some(&site))
            .0
        {
            ControlledNetworkAction::Passthrough { handle } => handle,
            _ => panic!("expected admitted predecessor"),
        };
        let successor = ControlledNetworkRequest {
            load_id: WebResourceLoadId::new([42; 16], 1),
            method: "POST",
            url: &next_url,
            resource_kind: EvidenceResourceKind::Fetch,
            main_frame: true,
            header_names: &[],
            body_bytes: Some(0),
        };
        assert!(matches!(
            session.begin_with_cookie_policy(successor, None).0,
            ControlledNetworkAction::Abort {
                handle: Some(_),
                reason: ControlledNetworkAbortReason::CookiePolicyRejected,
            }
        ));
        assert_eq!(session.snapshot().active_operations, 1);

        let requests = serde_json::to_value(session.requests_page(None, 16).unwrap()).unwrap();
        assert_eq!(requests["records"][1]["redirectParentId"], "1");
        assert_eq!(requests["records"][1]["method"], "POST");
        let evidence = serde_json::to_value(session.evidence_page(None, 32).unwrap()).unwrap();
        let records = evidence["records"].as_array().unwrap();
        assert!(records.iter().any(|record| {
            record["kind"] == "redirect"
                && record["requestId"] == "1"
                && record["nextRequestId"] == "2"
        }));
        assert!(
            records
                .iter()
                .any(|record| { record["kind"] == "request_failed" && record["requestId"] == "2" })
        );
        assert!(
            !records
                .iter()
                .any(|record| { record["kind"] == "route_decided" && record["requestId"] == "2" })
        );
        assert!(
            !records.iter().any(|record| {
                record["kind"] == "request_completed" && record["requestId"] == "2"
            })
        );

        session.live_terminal(
            first.load_id(),
            ControlledNetworkTerminal::Completed {
                status: 302,
                response_bytes: 0,
            },
        );
        assert_eq!(session.snapshot().active_operations, 0);
        let inner = session.0.lock();
        assert!(inner.retired_redirect_predecessors.is_empty());
        assert!(inner.retired_redirect_order.is_empty());
    }

    #[test]
    fn authoritative_navigation_ids_share_the_bounded_evidence_ledger() {
        let session = fixture_session("live", json!([]));
        let navigation_id = SessionNavigationId::new(41);
        session.record_navigation_started(navigation_id);
        session.record_navigation_committed(navigation_id);
        session.record_same_document_history_changed(navigation_id);
        session.record_settlement_terminal(navigation_id);
        session.record_navigation_failed(
            SessionNavigationId::new(42),
            NetworkFailureReason::RedirectLimitExceeded,
        );

        let page = serde_json::to_value(session.evidence_page(None, 16).unwrap()).unwrap();
        assert_eq!(page["records"][0]["navigationId"], "41");
        assert_eq!(page["records"][0]["kind"], "navigation_started");
        assert_eq!(page["records"][2]["kind"], "same_document_history_changed");
        assert_eq!(page["records"][3]["kind"], "settlement_terminal");
        assert_eq!(page["records"][4]["kind"], "navigation_failed");
        assert_eq!(page["records"][4]["reason"], "redirect_limit_exceeded");

        for _ in 0..crate::network_evidence::DEFAULT_EVIDENCE_MAX_RECORDS {
            session.record_same_document_history_changed(navigation_id);
        }
        let truncated = session
            .evidence_page(Some(EvidenceSequence::new(0)), 1)
            .unwrap();
        assert!(!truncated.complete);
        assert!(truncated.dropped_through_seq.is_some());
    }
}
