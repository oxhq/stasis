/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Canonical, redacted, bounded controlled-network evidence.
//!
//! Callers can provide URL and header-name metadata, but this API has no parameter for header
//! values or body bytes. Credentials, fragments, query values, Cookie/Authorization values, and
//! bodies therefore cannot enter retained evidence by construction.

use std::collections::{BTreeSet, VecDeque};

use embedder_traits::{SessionNavigationId, WebResourceKind};
use serde::{Serialize, Serializer};
use url::Url;

pub const MAX_EVIDENCE_METHOD_BYTES: usize = 32;
pub const MAX_EVIDENCE_ORIGIN_BYTES: usize = 512;
pub const MAX_EVIDENCE_PATH_BYTES: usize = 2 * 1024;
pub const MAX_EVIDENCE_QUERY_BYTES: usize = 16 * 1024;
pub const MAX_EVIDENCE_QUERY_KEYS: usize = 64;
pub const MAX_EVIDENCE_QUERY_KEY_BYTES: usize = 128;
pub const MAX_EVIDENCE_HEADER_NAMES: usize = 64;
pub const MAX_EVIDENCE_HEADER_NAME_BYTES: usize = 256;
pub const DEFAULT_EVIDENCE_MAX_RECORDS: usize = 1024;
pub const DEFAULT_EVIDENCE_MAX_METADATA_BYTES: usize = 1024 * 1024;
pub const DEFAULT_EVIDENCE_MAX_PAGE_ITEMS: usize = 256;
pub const HARD_EVIDENCE_MAX_RECORDS: usize = 4096;
pub const HARD_EVIDENCE_MAX_METADATA_BYTES: usize = 8 * 1024 * 1024;
pub const HARD_EVIDENCE_MAX_PAGE_ITEMS: usize = 1024;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct NetworkRequestId(u64);

impl NetworkRequestId {
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Serialize for NetworkRequestId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct EvidenceSequence(u64);

impl EvidenceSequence {
    /// Construct an exclusive pagination cursor. Zero means before the first record.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Serialize for EvidenceSequence {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct NavigationId(SessionNavigationId);

impl NavigationId {
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

impl From<SessionNavigationId> for NavigationId {
    fn from(value: SessionNavigationId) -> Self {
        Self(value)
    }
}

impl Serialize for NavigationId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.get().to_string())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceResourceKind {
    Navigation,
    Fetch,
    XmlHttpRequest,
    Image,
    Font,
    Stylesheet,
    Script,
    UnclassifiedProducerIo,
    Other,
}

impl From<WebResourceKind> for EvidenceResourceKind {
    fn from(kind: WebResourceKind) -> Self {
        match kind {
            WebResourceKind::Navigation => Self::Navigation,
            WebResourceKind::Fetch => Self::Fetch,
            WebResourceKind::XmlHttpRequest => Self::XmlHttpRequest,
            WebResourceKind::Image => Self::Image,
            WebResourceKind::Font => Self::Font,
            WebResourceKind::Stylesheet => Self::Stylesheet,
            WebResourceKind::Script => Self::Script,
            WebResourceKind::UnclassifiedProducerIo => Self::UnclassifiedProducerIo,
            WebResourceKind::Other => Self::Other,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedactedUrl {
    origin: String,
    path: String,
    query_keys: Vec<String>,
}

impl RedactedUrl {
    pub fn origin(&self) -> &str {
        &self.origin
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn query_keys(&self) -> &[String] {
        &self.query_keys
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RedactedRequest {
    request_id: NetworkRequestId,
    #[serde(skip_serializing_if = "Option::is_none")]
    redirect_parent_id: Option<NetworkRequestId>,
    method: String,
    url: RedactedUrl,
    resource_kind: EvidenceResourceKind,
    main_frame: bool,
    header_names: Vec<String>,
    #[serde(serialize_with = "serialize_u64_decimal")]
    body_bytes: u64,
}

/// Input boundary deliberately incapable of carrying header values or body content.
pub struct RedactedRequestInput<'a> {
    pub request_id: NetworkRequestId,
    pub redirect_parent_id: Option<NetworkRequestId>,
    pub method: &'a str,
    pub url: &'a Url,
    pub resource_kind: EvidenceResourceKind,
    pub main_frame: bool,
    pub header_names: &'a [&'a str],
    pub body_bytes: u64,
}

impl RedactedRequest {
    /// Build evidence from URL and header names only. Header values and body bytes are deliberately
    /// not accepted; only the already-counted body length can cross this boundary.
    pub fn new(input: RedactedRequestInput<'_>) -> Result<Self, EvidenceMetadataError> {
        let RedactedRequestInput {
            request_id,
            redirect_parent_id,
            method,
            url,
            resource_kind,
            main_frame,
            header_names,
            body_bytes,
        } = input;
        if method.is_empty()
            || method.len() > MAX_EVIDENCE_METHOD_BYTES
            || !is_http_token(method.as_bytes())
        {
            return Err(EvidenceMetadataError::InvalidMethod);
        }

        if !matches!(url.scheme(), "http" | "https") {
            return Err(EvidenceMetadataError::UnsupportedUrl);
        }
        let Some(host) = url.host_str() else {
            return Err(EvidenceMetadataError::UnsupportedUrl);
        };
        // Bound the only page-controlled variable-width origin component before serialization.
        if host.len() > MAX_EVIDENCE_ORIGIN_BYTES {
            return Err(EvidenceMetadataError::OriginTooLong {
                observed: host.len(),
                limit: MAX_EVIDENCE_ORIGIN_BYTES,
            });
        }
        let origin = url.origin().ascii_serialization();
        if origin == "null" {
            return Err(EvidenceMetadataError::UnsupportedUrl);
        }
        if origin.len() > MAX_EVIDENCE_ORIGIN_BYTES {
            return Err(EvidenceMetadataError::OriginTooLong {
                observed: origin.len(),
                limit: MAX_EVIDENCE_ORIGIN_BYTES,
            });
        }
        let path = url.path();
        if path.len() > MAX_EVIDENCE_PATH_BYTES {
            return Err(EvidenceMetadataError::PathTooLong {
                observed: path.len(),
                limit: MAX_EVIDENCE_PATH_BYTES,
            });
        }
        let path = path.to_owned();

        let mut query_keys = BTreeSet::new();
        if let Some(query) = url.query() {
            if query.len() > MAX_EVIDENCE_QUERY_BYTES {
                return Err(EvidenceMetadataError::QueryTooLong {
                    observed: query.len(),
                    limit: MAX_EVIDENCE_QUERY_BYTES,
                });
            }
            let mut raw_key_count = 0usize;
            for component in query.split('&') {
                let key = component.split_once('=').map_or(component, |(key, _)| key);
                if key.is_empty() {
                    continue;
                }
                raw_key_count += 1;
                if raw_key_count > MAX_EVIDENCE_QUERY_KEYS {
                    return Err(EvidenceMetadataError::TooManyQueryKeys {
                        observed: raw_key_count,
                        limit: MAX_EVIDENCE_QUERY_KEYS,
                    });
                }
                if key.len() > MAX_EVIDENCE_QUERY_KEY_BYTES {
                    return Err(EvidenceMetadataError::QueryKeyTooLong {
                        observed: key.len(),
                        limit: MAX_EVIDENCE_QUERY_KEY_BYTES,
                    });
                }
                query_keys.insert(key.to_owned());
                if query_keys.len() > MAX_EVIDENCE_QUERY_KEYS {
                    return Err(EvidenceMetadataError::TooManyQueryKeys {
                        observed: query_keys.len(),
                        limit: MAX_EVIDENCE_QUERY_KEYS,
                    });
                }
            }
        }
        let query_keys = query_keys.into_iter().collect();

        if header_names.len() > MAX_EVIDENCE_HEADER_NAMES {
            return Err(EvidenceMetadataError::TooManyHeaderNames {
                observed: header_names.len(),
                limit: MAX_EVIDENCE_HEADER_NAMES,
            });
        }
        let mut canonical_header_names = BTreeSet::new();
        for name in header_names {
            if name.len() > MAX_EVIDENCE_HEADER_NAME_BYTES || !is_http_token(name.as_bytes()) {
                return Err(EvidenceMetadataError::InvalidHeaderName);
            }
            canonical_header_names.insert(name.to_ascii_lowercase());
            if canonical_header_names.len() > MAX_EVIDENCE_HEADER_NAMES {
                return Err(EvidenceMetadataError::TooManyHeaderNames {
                    observed: canonical_header_names.len(),
                    limit: MAX_EVIDENCE_HEADER_NAMES,
                });
            }
        }
        let canonical_header_names = canonical_header_names.into_iter().collect();

        Ok(Self {
            request_id,
            redirect_parent_id,
            method: method.to_ascii_uppercase(),
            url: RedactedUrl {
                origin,
                path,
                query_keys,
            },
            resource_kind,
            main_frame,
            header_names: canonical_header_names,
            body_bytes,
        })
    }

    pub const fn request_id(&self) -> NetworkRequestId {
        self.request_id
    }

    pub fn url(&self) -> &RedactedUrl {
        &self.url
    }

    pub fn header_names(&self) -> &[String] {
        &self.header_names
    }

    pub const fn body_bytes(&self) -> u64 {
        self.body_bytes
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RouteEvidenceDecision {
    FixtureFulfill,
    FixtureAbort,
    Live,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkFailureReason {
    BlockedByFixture,
    FixtureMiss,
    Cancelled,
    ConnectionReset,
    NetworkError,
    NavigationError,
    DocumentTransitionLimitExceeded,
    RedirectLimitExceeded,
    HistoryLimitExceeded,
}

/// Allow-listed event metadata. Unknown causal relationships are represented by absent IDs.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum NetworkEvidenceEvent {
    RequestStarted {
        request_id: NetworkRequestId,
    },
    RouteDecided {
        request_id: NetworkRequestId,
        decision: RouteEvidenceDecision,
    },
    ResponseHeaders {
        request_id: NetworkRequestId,
        status: u16,
    },
    Redirect {
        request_id: NetworkRequestId,
        next_request_id: NetworkRequestId,
    },
    RequestCompleted {
        request_id: NetworkRequestId,
    },
    RequestFailed {
        request_id: NetworkRequestId,
        reason: NetworkFailureReason,
    },
    NavigationStarted {
        navigation_id: NavigationId,
    },
    NavigationCommitted {
        navigation_id: NavigationId,
    },
    NavigationFailed {
        navigation_id: NavigationId,
        reason: NetworkFailureReason,
    },
    SameDocumentHistoryChanged {
        navigation_id: NavigationId,
    },
    SettlementTerminal {
        navigation_id: NavigationId,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkEvidenceEntry {
    seq: EvidenceSequence,
    #[serde(serialize_with = "serialize_u128_decimal")]
    at_virtual_ns: u128,
    #[serde(flatten)]
    event: NetworkEvidenceEvent,
}

/// Public request metadata projected from the request-start entry that owns this sequence.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkRequestRecord {
    seq: EvidenceSequence,
    #[serde(flatten)]
    request: RedactedRequest,
}

impl NetworkRequestRecord {
    pub const fn sequence(&self) -> EvidenceSequence {
        self.seq
    }

    pub fn request(&self) -> &RedactedRequest {
        &self.request
    }
}

impl NetworkEvidenceEntry {
    pub const fn sequence(&self) -> EvidenceSequence {
        self.seq
    }

    pub const fn at_virtual_ns(&self) -> u128 {
        self.at_virtual_ns
    }

    pub fn event(&self) -> &NetworkEvidenceEvent {
        &self.event
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EvidenceLedgerBounds {
    max_records: usize,
    max_metadata_bytes: usize,
    max_page_items: usize,
}

impl EvidenceLedgerBounds {
    pub fn new(
        max_records: usize,
        max_metadata_bytes: usize,
        max_page_items: usize,
    ) -> Result<Self, EvidenceLedgerError> {
        if max_records == 0 || max_records > HARD_EVIDENCE_MAX_RECORDS {
            return Err(EvidenceLedgerError::InvalidBounds);
        }
        if max_metadata_bytes == 0 || max_metadata_bytes > HARD_EVIDENCE_MAX_METADATA_BYTES {
            return Err(EvidenceLedgerError::InvalidBounds);
        }
        if max_page_items == 0 || max_page_items > HARD_EVIDENCE_MAX_PAGE_ITEMS {
            return Err(EvidenceLedgerError::InvalidBounds);
        }
        Ok(Self {
            max_records,
            max_metadata_bytes,
            max_page_items,
        })
    }

    pub const fn max_records(self) -> usize {
        self.max_records
    }

    pub const fn max_metadata_bytes(self) -> usize {
        self.max_metadata_bytes
    }

    pub const fn max_page_items(self) -> usize {
        self.max_page_items
    }
}

impl Default for EvidenceLedgerBounds {
    fn default() -> Self {
        Self {
            max_records: DEFAULT_EVIDENCE_MAX_RECORDS,
            max_metadata_bytes: DEFAULT_EVIDENCE_MAX_METADATA_BYTES,
            max_page_items: DEFAULT_EVIDENCE_MAX_PAGE_ITEMS,
        }
    }
}

struct StoredEntry {
    entry: NetworkEvidenceEntry,
    request: Option<NetworkRequestRecord>,
    metadata_bytes: usize,
}

/// Bounded append-only diagnostic ledger. It is never a settlement correctness authority.
pub struct NetworkEvidenceLedger {
    bounds: EvidenceLedgerBounds,
    entries: VecDeque<StoredEntry>,
    retained_metadata_bytes: usize,
    last_sequence: u64,
    last_request_id: u64,
    dropped_through_seq: Option<EvidenceSequence>,
}

impl NetworkEvidenceLedger {
    pub fn new(bounds: EvidenceLedgerBounds) -> Self {
        Self {
            bounds,
            entries: VecDeque::new(),
            retained_metadata_bytes: 0,
            last_sequence: 0,
            last_request_id: 0,
            dropped_through_seq: None,
        }
    }

    pub fn allocate_request_id(&mut self) -> Result<NetworkRequestId, EvidenceLedgerError> {
        self.last_request_id = self
            .last_request_id
            .checked_add(1)
            .ok_or(EvidenceLedgerError::RequestIdExhausted)?;
        Ok(NetworkRequestId(self.last_request_id))
    }

    /// Record the one canonical request-start event and its redacted public request projection.
    pub fn record_request_started(
        &mut self,
        at_virtual_ns: u128,
        request: RedactedRequest,
    ) -> Result<EvidenceSequence, EvidenceLedgerError> {
        let request_id = request.request_id();
        self.append_internal(
            at_virtual_ns,
            NetworkEvidenceEvent::RequestStarted { request_id },
            Some(request),
        )
    }

    /// Record non-start evidence. Request starts must use `record_request_started` so
    /// `session.requests` and `session.evidence` cannot diverge.
    pub fn record_event(
        &mut self,
        at_virtual_ns: u128,
        event: NetworkEvidenceEvent,
    ) -> Result<EvidenceSequence, EvidenceLedgerError> {
        if matches!(event, NetworkEvidenceEvent::RequestStarted { .. }) {
            return Err(EvidenceLedgerError::RequestMetadataRequired);
        }
        self.append_internal(at_virtual_ns, event, None)
    }

    fn append_internal(
        &mut self,
        at_virtual_ns: u128,
        event: NetworkEvidenceEvent,
        request: Option<RedactedRequest>,
    ) -> Result<EvidenceSequence, EvidenceLedgerError> {
        let sequence = EvidenceSequence(
            self.last_sequence
                .checked_add(1)
                .ok_or(EvidenceLedgerError::SequenceExhausted)?,
        );
        let entry = NetworkEvidenceEntry {
            seq: sequence,
            at_virtual_ns,
            event,
        };
        let mut metadata_bytes = serde_json::to_vec(&entry)
            .map_err(|_| EvidenceLedgerError::SerializationFailed)?
            .len();
        let request = request.map(|request| NetworkRequestRecord {
            seq: sequence,
            request,
        });
        if let Some(request) = &request {
            metadata_bytes = metadata_bytes
                .checked_add(
                    serde_json::to_vec(request)
                        .map_err(|_| EvidenceLedgerError::SerializationFailed)?
                        .len(),
                )
                .ok_or(EvidenceLedgerError::MetadataCounterOverflow)?;
        }
        if metadata_bytes > self.bounds.max_metadata_bytes {
            return Err(EvidenceLedgerError::EntryTooLarge {
                observed: metadata_bytes,
                limit: self.bounds.max_metadata_bytes,
            });
        }
        let retained = self
            .retained_metadata_bytes
            .checked_add(metadata_bytes)
            .ok_or(EvidenceLedgerError::MetadataCounterOverflow)?;

        self.entries.push_back(StoredEntry {
            entry,
            request,
            metadata_bytes,
        });
        self.retained_metadata_bytes = retained;
        self.last_sequence = sequence.0;
        while self.entries.len() > self.bounds.max_records
            || self.retained_metadata_bytes > self.bounds.max_metadata_bytes
        {
            let removed = self
                .entries
                .pop_front()
                .expect("an over-limit ledger must contain an entry");
            self.retained_metadata_bytes -= removed.metadata_bytes;
            self.dropped_through_seq = Some(removed.entry.seq);
        }
        Ok(sequence)
    }

    /// Return evidence after `after`, plus explicit loss and pagination metadata.
    pub fn evidence_page(
        &self,
        after: Option<EvidenceSequence>,
        limit: usize,
    ) -> Result<NetworkEvidencePage, EvidenceLedgerError> {
        self.validate_page_limit(limit)?;
        let cursor = after.map_or(0, EvidenceSequence::get);
        let cursor_was_truncated = self
            .dropped_through_seq
            .is_some_and(|dropped| cursor < dropped.get());
        let mut matching = self
            .entries
            .iter()
            .filter(|stored| stored.entry.seq.get() > cursor);
        let entries = matching
            .by_ref()
            .take(limit)
            .map(|stored| stored.entry.clone())
            .collect::<Vec<_>>();
        let has_more = matching.next().is_some();
        let next_after_seq = entries.last().map_or(after, |entry| Some(entry.seq));

        Ok(NetworkEvidencePage {
            schema_version: 2,
            records: entries,
            first_retained_seq: self.entries.front().map(|stored| stored.entry.seq),
            next_after_seq,
            latest_seq: (self.last_sequence != 0).then_some(EvidenceSequence(self.last_sequence)),
            complete: !cursor_was_truncated,
            has_more,
            dropped_through_seq: self.dropped_through_seq,
            bounds: self.bounds,
        })
    }

    /// Return request-start records from the same sequence and retention domain as evidence.
    pub fn requests_page(
        &self,
        after: Option<EvidenceSequence>,
        limit: usize,
    ) -> Result<NetworkRequestsPage, EvidenceLedgerError> {
        self.validate_page_limit(limit)?;
        let cursor = after.map_or(0, EvidenceSequence::get);
        let cursor_was_truncated = self
            .dropped_through_seq
            .is_some_and(|dropped| cursor < dropped.get());
        let mut matching = self
            .entries
            .iter()
            .filter(|stored| stored.entry.seq.get() > cursor)
            .filter_map(|stored| stored.request.as_ref());
        let records = matching.by_ref().take(limit).cloned().collect::<Vec<_>>();
        let has_more = matching.next().is_some();
        let next_after_seq = records.last().map_or(after, |record| Some(record.seq));
        Ok(NetworkRequestsPage {
            records,
            first_retained_seq: self.entries.front().map(|stored| stored.entry.seq),
            next_after_seq,
            latest_seq: (self.last_sequence != 0).then_some(EvidenceSequence(self.last_sequence)),
            complete: !cursor_was_truncated,
            has_more,
            dropped_through_seq: self.dropped_through_seq,
            bounds: self.bounds,
        })
    }

    fn validate_page_limit(&self, limit: usize) -> Result<(), EvidenceLedgerError> {
        if limit == 0 || limit > self.bounds.max_page_items {
            return Err(EvidenceLedgerError::InvalidPageLimit {
                observed: limit,
                limit: self.bounds.max_page_items,
            });
        }
        Ok(())
    }

    pub const fn retained_metadata_bytes(&self) -> usize {
        self.retained_metadata_bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkEvidencePage {
    pub schema_version: u8,
    pub records: Vec<NetworkEvidenceEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_retained_seq: Option<EvidenceSequence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after_seq: Option<EvidenceSequence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_seq: Option<EvidenceSequence>,
    /// False only when records after the requested cursor were already evicted.
    pub complete: bool,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_through_seq: Option<EvidenceSequence>,
    pub bounds: EvidenceLedgerBounds,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NetworkRequestsPage {
    pub records: Vec<NetworkRequestRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_retained_seq: Option<EvidenceSequence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after_seq: Option<EvidenceSequence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latest_seq: Option<EvidenceSequence>,
    pub complete: bool,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dropped_through_seq: Option<EvidenceSequence>,
    pub bounds: EvidenceLedgerBounds,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvidenceMetadataError {
    InvalidMethod,
    UnsupportedUrl,
    OriginTooLong { observed: usize, limit: usize },
    PathTooLong { observed: usize, limit: usize },
    QueryTooLong { observed: usize, limit: usize },
    QueryKeyTooLong { observed: usize, limit: usize },
    TooManyQueryKeys { observed: usize, limit: usize },
    InvalidHeaderName,
    TooManyHeaderNames { observed: usize, limit: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EvidenceLedgerError {
    InvalidBounds,
    RequestIdExhausted,
    SequenceExhausted,
    RequestMetadataRequired,
    SerializationFailed,
    MetadataCounterOverflow,
    EntryTooLarge { observed: usize, limit: usize },
    InvalidPageLimit { observed: usize, limit: usize },
}

fn serialize_u64_decimal<S>(value: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

fn serialize_u128_decimal<S>(value: &u128, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

fn is_http_token(bytes: &[u8]) -> bool {
    !bytes.is_empty()
        && bytes.iter().copied().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        id: NetworkRequestId,
        url: &str,
        header_names: &[&str],
        body_bytes: u64,
    ) -> RedactedRequest {
        let url = Url::parse(url).unwrap();
        RedactedRequest::new(RedactedRequestInput {
            request_id: id,
            redirect_parent_id: None,
            method: "post",
            url: &url,
            resource_kind: EvidenceResourceKind::Fetch,
            main_frame: false,
            header_names,
            body_bytes,
        })
        .unwrap()
    }

    #[test]
    fn retained_json_has_no_credentials_query_values_header_values_or_body() {
        let mut ledger = NetworkEvidenceLedger::new(EvidenceLedgerBounds::default());
        let id = ledger.allocate_request_id().unwrap();
        let cookie_value = "session=private-cookie-value";
        let authorization_value = "Bearer private-authorization-value";
        let body = "private-body-value";
        let request = request(
            id,
            "https://alice:private-password@example.test/path?token=private-token&mode=full#private-fragment",
            &["Cookie", "Authorization", "Content-Type"],
            body.len() as u64,
        );
        ledger.record_request_started(17, request).unwrap();
        let evidence_json =
            serde_json::to_string(&ledger.evidence_page(None, 10).unwrap()).unwrap();
        let requests_json =
            serde_json::to_string(&ledger.requests_page(None, 10).unwrap()).unwrap();
        let json = format!("{evidence_json}\n{requests_json}");

        for secret in [
            "alice",
            "private-password",
            "private-token",
            "private-fragment",
            cookie_value,
            authorization_value,
            body,
        ] {
            assert!(!json.contains(secret), "evidence leaked {secret:?}: {json}");
        }
        assert!(json.contains("example.test"));
        assert!(json.contains("token"));
        assert!(json.contains("authorization"));
        assert!(json.contains("cookie"));
    }

    #[test]
    fn redaction_is_canonical_for_query_keys_and_header_names() {
        let id = NetworkRequestId(1);
        let request = request(
            id,
            "https://example.test/path?z=last&a=first&z=duplicate",
            &["X-Z", "authorization", "x-z", "Cookie"],
            0,
        );
        assert_eq!(request.url().query_keys(), &["a", "z"]);
        assert_eq!(request.header_names(), &["authorization", "cookie", "x-z"]);
    }

    #[test]
    fn record_bound_eviction_is_explicit_and_cursor_aware() {
        let bounds = EvidenceLedgerBounds::new(2, 32 * 1024, 2).unwrap();
        let mut ledger = NetworkEvidenceLedger::new(bounds);
        let id = ledger.allocate_request_id().unwrap();
        for status in [200, 201, 202] {
            ledger
                .record_event(
                    0,
                    NetworkEvidenceEvent::ResponseHeaders {
                        request_id: id,
                        status,
                    },
                )
                .unwrap();
        }

        let lost = ledger.evidence_page(None, 2).unwrap();
        assert!(!lost.complete);
        assert_eq!(lost.dropped_through_seq, Some(EvidenceSequence(1)));
        assert_eq!(lost.first_retained_seq, Some(EvidenceSequence(2)));
        assert_eq!(lost.records.len(), 2);

        let resumed = ledger.evidence_page(Some(EvidenceSequence(1)), 2).unwrap();
        assert!(resumed.complete);
        assert!(!resumed.has_more);
        assert_eq!(resumed.next_after_seq, Some(EvidenceSequence(3)));
    }

    #[test]
    fn page_limit_and_single_entry_metadata_limit_are_enforced() {
        let bounds = EvidenceLedgerBounds::new(4, 1, 2).unwrap();
        let mut ledger = NetworkEvidenceLedger::new(bounds);
        let id = ledger.allocate_request_id().unwrap();
        assert!(matches!(
            ledger.record_event(
                0,
                NetworkEvidenceEvent::RequestFailed {
                    request_id: id,
                    reason: NetworkFailureReason::FixtureMiss,
                },
            ),
            Err(EvidenceLedgerError::EntryTooLarge { limit: 1, .. })
        ));
        assert_eq!(ledger.retained_metadata_bytes(), 0);
        assert_eq!(
            ledger.evidence_page(None, 0).unwrap_err(),
            EvidenceLedgerError::InvalidPageLimit {
                observed: 0,
                limit: 2
            }
        );
    }

    #[test]
    fn url_and_metadata_dimensions_are_independently_bounded() {
        let id = NetworkRequestId(1);
        let path = format!("/{}", "x".repeat(MAX_EVIDENCE_PATH_BYTES));
        assert!(matches!(
            RedactedRequest::new(RedactedRequestInput {
                request_id: id,
                redirect_parent_id: None,
                method: "GET",
                url: &Url::parse(&format!("https://example.test{path}")).unwrap(),
                resource_kind: EvidenceResourceKind::Fetch,
                main_frame: false,
                header_names: &[],
                body_bytes: 0,
            }),
            Err(EvidenceMetadataError::PathTooLong { .. })
        ));

        let query = (0..=MAX_EVIDENCE_QUERY_KEYS)
            .map(|index| format!("k{index}=secret"))
            .collect::<Vec<_>>()
            .join("&");
        assert!(matches!(
            RedactedRequest::new(RedactedRequestInput {
                request_id: id,
                redirect_parent_id: None,
                method: "GET",
                url: &Url::parse(&format!("https://example.test/?{query}")).unwrap(),
                resource_kind: EvidenceResourceKind::Fetch,
                main_frame: false,
                header_names: &[],
                body_bytes: 0,
            }),
            Err(EvidenceMetadataError::TooManyQueryKeys { .. })
        ));

        let header_names = (0..=MAX_EVIDENCE_HEADER_NAMES)
            .map(|index| format!("x-{index}"))
            .collect::<Vec<_>>();
        let borrowed = header_names.iter().map(String::as_str).collect::<Vec<_>>();
        assert!(matches!(
            RedactedRequest::new(RedactedRequestInput {
                request_id: id,
                redirect_parent_id: None,
                method: "GET",
                url: &Url::parse("https://example.test/").unwrap(),
                resource_kind: EvidenceResourceKind::Fetch,
                main_frame: false,
                header_names: &borrowed,
                body_bytes: 0,
            }),
            Err(EvidenceMetadataError::TooManyHeaderNames { .. })
        ));
    }

    #[test]
    fn metadata_cardinality_rejection_is_early_and_duplicate_safe() {
        let id = NetworkRequestId(1);
        let oversized_path = format!("/{}", "x".repeat(128 * 1024));
        assert_eq!(
            RedactedRequest::new(RedactedRequestInput {
                request_id: id,
                redirect_parent_id: None,
                method: "GET",
                url: &Url::parse(&format!("https://example.test{oversized_path}")).unwrap(),
                resource_kind: EvidenceResourceKind::Fetch,
                main_frame: false,
                header_names: &[],
                body_bytes: 0,
            }),
            Err(EvidenceMetadataError::PathTooLong {
                observed: oversized_path.len(),
                limit: MAX_EVIDENCE_PATH_BYTES,
            })
        );

        let oversized_query = format!("key={}", "x".repeat(MAX_EVIDENCE_QUERY_BYTES));
        assert_eq!(
            RedactedRequest::new(RedactedRequestInput {
                request_id: id,
                redirect_parent_id: None,
                method: "GET",
                url: &Url::parse(&format!("https://example.test/?{oversized_query}")).unwrap(),
                resource_kind: EvidenceResourceKind::Fetch,
                main_frame: false,
                header_names: &[],
                body_bytes: 0,
            }),
            Err(EvidenceMetadataError::QueryTooLong {
                observed: oversized_query.len(),
                limit: MAX_EVIDENCE_QUERY_BYTES,
            })
        );

        let duplicate_query = std::iter::repeat_n("same=secret", MAX_EVIDENCE_QUERY_KEYS + 1)
            .collect::<Vec<_>>()
            .join("&");
        assert_eq!(
            RedactedRequest::new(RedactedRequestInput {
                request_id: id,
                redirect_parent_id: None,
                method: "GET",
                url: &Url::parse(&format!("https://example.test/?{duplicate_query}")).unwrap(),
                resource_kind: EvidenceResourceKind::Fetch,
                main_frame: false,
                header_names: &[],
                body_bytes: 0,
            }),
            Err(EvidenceMetadataError::TooManyQueryKeys {
                observed: MAX_EVIDENCE_QUERY_KEYS + 1,
                limit: MAX_EVIDENCE_QUERY_KEYS,
            })
        );

        let header_names = (0..4096)
            .map(|index| format!("x-{index}"))
            .collect::<Vec<_>>();
        let borrowed = header_names.iter().map(String::as_str).collect::<Vec<_>>();
        assert_eq!(
            RedactedRequest::new(RedactedRequestInput {
                request_id: id,
                redirect_parent_id: None,
                method: "GET",
                url: &Url::parse("https://example.test/").unwrap(),
                resource_kind: EvidenceResourceKind::Fetch,
                main_frame: false,
                header_names: &borrowed,
                body_bytes: 0,
            }),
            Err(EvidenceMetadataError::TooManyHeaderNames {
                observed: header_names.len(),
                limit: MAX_EVIDENCE_HEADER_NAMES,
            })
        );

        let duplicate_query = std::iter::repeat_n("same=secret", MAX_EVIDENCE_QUERY_KEYS)
            .collect::<Vec<_>>()
            .join("&");
        let duplicate_headers = vec!["X-Duplicate"; MAX_EVIDENCE_HEADER_NAMES];
        let request = RedactedRequest::new(RedactedRequestInput {
            request_id: id,
            redirect_parent_id: None,
            method: "GET",
            url: &Url::parse(&format!("https://example.test/?{duplicate_query}")).unwrap(),
            resource_kind: EvidenceResourceKind::Fetch,
            main_frame: false,
            header_names: &duplicate_headers,
            body_bytes: 0,
        })
        .unwrap();
        assert_eq!(request.url().query_keys(), &["same"]);
        assert_eq!(request.header_names(), &["x-duplicate"]);
    }

    #[test]
    fn pagination_reports_more_without_claiming_history_loss() {
        let bounds = EvidenceLedgerBounds::new(8, 32 * 1024, 2).unwrap();
        let mut ledger = NetworkEvidenceLedger::new(bounds);
        let id = ledger.allocate_request_id().unwrap();
        for status in [200, 201, 202] {
            ledger
                .record_event(
                    0,
                    NetworkEvidenceEvent::ResponseHeaders {
                        request_id: id,
                        status,
                    },
                )
                .unwrap();
        }
        let first = ledger.evidence_page(None, 2).unwrap();
        assert!(first.complete);
        assert!(first.has_more);
        let second = ledger.evidence_page(first.next_after_seq, 2).unwrap();
        assert!(second.complete);
        assert!(!second.has_more);
        assert_eq!(second.records[0].sequence(), EvidenceSequence(3));
    }

    #[test]
    fn request_and_evidence_pages_share_sequences_and_decimal_wire_encoding() {
        let mut ledger = NetworkEvidenceLedger::new(EvidenceLedgerBounds::default());
        let id = ledger.allocate_request_id().unwrap();
        let request = request(
            id,
            "https://example.test/path?token=secret",
            &["Authorization"],
            12,
        );
        let started = ledger.record_request_started(u128::MAX, request).unwrap();
        ledger
            .record_event(
                0,
                NetworkEvidenceEvent::RouteDecided {
                    request_id: id,
                    decision: RouteEvidenceDecision::FixtureFulfill,
                },
            )
            .unwrap();

        let requests = ledger.requests_page(None, 10).unwrap();
        let evidence = ledger.evidence_page(None, 10).unwrap();
        assert_eq!(requests.records[0].sequence(), started);
        assert_eq!(evidence.records[0].sequence(), started);
        assert_eq!(requests.latest_seq, evidence.latest_seq);

        let request_json = serde_json::to_value(requests).unwrap();
        assert_eq!(request_json["records"][0]["seq"], "1");
        assert_eq!(request_json["records"][0]["requestId"], "1");
        assert_eq!(request_json["records"][0]["bodyBytes"], "12");
        let evidence_json = serde_json::to_value(evidence).unwrap();
        assert_eq!(evidence_json["schemaVersion"], 2);
        assert_eq!(evidence_json["records"][0]["seq"], "1");
        assert_eq!(
            evidence_json["records"][0]["atVirtualNs"],
            u128::MAX.to_string()
        );
        assert_eq!(evidence_json["records"][1]["decision"], "fixture_fulfill");
    }

    #[test]
    fn request_projection_has_shared_truncation_and_filters_non_request_events() {
        let bounds = EvidenceLedgerBounds::new(2, 32 * 1024, 2).unwrap();
        let mut ledger = NetworkEvidenceLedger::new(bounds);
        let first_id = ledger.allocate_request_id().unwrap();
        ledger
            .record_request_started(0, request(first_id, "https://example.test/one", &[], 0))
            .unwrap();
        ledger
            .record_event(
                0,
                NetworkEvidenceEvent::RequestCompleted {
                    request_id: first_id,
                },
            )
            .unwrap();
        let second_id = ledger.allocate_request_id().unwrap();
        ledger
            .record_request_started(0, request(second_id, "https://example.test/two", &[], 0))
            .unwrap();

        let page = ledger.requests_page(None, 2).unwrap();
        assert!(!page.complete);
        assert_eq!(page.dropped_through_seq, Some(EvidenceSequence(1)));
        assert_eq!(page.records.len(), 1);
        assert_eq!(page.records[0].sequence(), EvidenceSequence(3));
    }

    #[test]
    fn request_started_cannot_be_recorded_without_request_metadata() {
        let mut ledger = NetworkEvidenceLedger::new(EvidenceLedgerBounds::default());
        let id = ledger.allocate_request_id().unwrap();
        assert_eq!(
            ledger
                .record_event(0, NetworkEvidenceEvent::RequestStarted { request_id: id },)
                .unwrap_err(),
            EvidenceLedgerError::RequestMetadataRequired
        );
    }

    #[test]
    fn navigation_events_are_minimal_and_redacted() {
        let mut ledger = NetworkEvidenceLedger::new(EvidenceLedgerBounds::default());
        let navigation_id = NavigationId::from(SessionNavigationId::new(7));
        ledger
            .record_event(4, NetworkEvidenceEvent::NavigationStarted { navigation_id })
            .unwrap();
        ledger
            .record_event(
                5,
                NetworkEvidenceEvent::NavigationCommitted { navigation_id },
            )
            .unwrap();
        ledger
            .record_event(
                6,
                NetworkEvidenceEvent::SameDocumentHistoryChanged { navigation_id },
            )
            .unwrap();
        ledger
            .record_event(
                7,
                NetworkEvidenceEvent::SettlementTerminal { navigation_id },
            )
            .unwrap();
        let json = serde_json::to_value(ledger.evidence_page(None, 10).unwrap()).unwrap();
        assert_eq!(json["records"][0]["kind"], "navigation_started");
        assert_eq!(json["records"][0]["navigationId"], "7");
        assert_eq!(json["records"][1]["kind"], "navigation_committed");
        assert_eq!(json["records"][2]["kind"], "same_document_history_changed");
        assert_eq!(json["records"][3]["kind"], "settlement_terminal");
        assert!(!json.to_string().contains("url"));
    }
}
