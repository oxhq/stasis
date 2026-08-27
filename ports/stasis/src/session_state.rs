/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Strict, sensitive session-state wire types and the unpublished import boundary.
//!
//! Cookie and Web Storage live on separate Servo threads, so their ordinary replace APIs cannot
//! form a rollback-capable cross-thread transaction. Stasis instead validates the complete state
//! document first, applies it to an unpublished pre-navigation session, and publishes the session
//! only after both backend replaces succeed. Any backend failure consumes and abandons that
//! unpublished target.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::{self, Write};
use std::net::IpAddr;

use embedder_traits::ControlledCookiePolicy;
use net_traits::pub_domains::{is_pub_domain, reg_suffix};
use net_traits::{
    COOKIE_STATE_MAX_COOKIES_PER_REGISTRABLE_HOST_V1,
    COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1, COOKIE_STATE_SCHEMA_VERSION_V1,
    CookieStateRecordV1, CookieStateSameSite, CookieStateSnapshotV1, has_valid_cookie_state_prefix,
    is_canonical_cookie_state_domain, is_valid_cookie_state_name_and_value,
};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use servo::SiteDataManager;
use servo_base::id::WebViewId;
use storage_traits::webstorage_thread::{
    WEB_STORAGE_STATE_MAX_PUBLIC_JSON_BYTES_V1, WEB_STORAGE_STATE_SCHEMA_VERSION_V1,
    WebStorageOriginStateV1, WebStorageStateEntryV1, WebStorageStateSnapshotV1,
};
use url::Url;

use crate::token_namespace::{
    OpaqueTokenNamespace, format_namespaced_token, split_namespaced_token,
};

pub const SESSION_STATE_SCHEMA_VERSION_V1: u16 = 1;
pub const CONTROLLED_WEB_SESSION_V1_PROFILE: &str = "controlled-web-session-v1";
pub const CONTROLLED_WEB_SESSION_V2_PROFILE: &str = "controlled-web-session-v2";
pub const TOP_LEVEL_SESSION_STORAGE_SCOPE: &str = "top_level_browsing_context";
pub const MAX_SESSION_STATE_BYTES: usize = 512 * 1024;
pub const MAX_SESSION_COOKIES: usize = 512;
const MAX_CONTROLLED_COOKIE_LIFETIME_NS: u64 = 34_560_000 * 1_000_000_000;
pub const MAX_SESSION_COOKIES_PER_REGISTRABLE_HOST: usize =
    COOKIE_STATE_MAX_COOKIES_PER_REGISTRABLE_HOST_V1;
pub const MAX_SESSION_COOKIE_BYTES: usize = 4096;
pub const MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES: usize =
    COOKIE_STATE_MAX_ENCODED_PUBLIC_ARRAY_BYTES_V1;
pub const MAX_SESSION_STORAGE_ARRAY_ENCODED_BYTES: usize =
    WEB_STORAGE_STATE_MAX_PUBLIC_JSON_BYTES_V1;
pub const MAX_SESSION_STORAGE_ORIGINS: usize = 64;
pub const MAX_SESSION_STORAGE_ENTRIES_PER_AREA: usize = 1024;
pub const MAX_SESSION_STORAGE_KEY_BYTES: usize = 4096;
pub const MAX_SESSION_STORAGE_VALUE_BYTES: usize = 128 * 1024;
pub const MAX_SESSION_STORAGE_BYTES_PER_ORIGIN: usize = 512 * 1024;
pub const SESSION_STATE_TOKEN_PREFIX: &str = "session:";
const SESSION_STATE_TOKEN_MAX_BYTES: usize = 61;

/// A canonical u64 decimal string on the JSON wire.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct WireU64(u64);

impl WireU64 {
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Serialize for WireU64 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for WireU64 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CanonicalU64Visitor;

        impl Visitor<'_> for CanonicalU64Visitor {
            type Value = WireU64;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a canonical decimal u64 string")
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: de::Error,
            {
                if value.is_empty()
                    || (value.len() > 1 && value.starts_with('0'))
                    || !value.bytes().all(|byte| byte.is_ascii_digit())
                {
                    return Err(E::custom("canonical decimal u64 string required"));
                }
                value
                    .parse::<u64>()
                    .map(WireU64)
                    .map_err(|_| E::custom("decimal string exceeds u64"))
            }
        }

        deserializer.deserialize_str(CanonicalU64Visitor)
    }
}

/// Opaque session-local authority. Debug output deliberately withholds the token bytes.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct SessionStateToken(String);

impl SessionStateToken {
    fn from_alias(namespace: &OpaqueTokenNamespace, alias: u64) -> Result<Self, SessionStateError> {
        if alias == 0 {
            return Err(SessionStateError::TokenSpaceExhausted);
        }
        Ok(Self(format_namespaced_token(
            SESSION_STATE_TOKEN_PREFIX,
            namespace,
            alias,
        )))
    }

    fn from_wire(token: String) -> Result<Self, &'static str> {
        if token.len() > SESSION_STATE_TOKEN_MAX_BYTES {
            return Err("canonical session-state token required");
        }
        let (_, alias) = split_namespaced_token(&token, SESSION_STATE_TOKEN_PREFIX)
            .map_err(|_| "canonical session-state token required")?;
        alias
            .parse::<u64>()
            .map_err(|_| "session-state token alias exceeds u64")?;
        Ok(Self(token))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SessionStateToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionStateToken(<redacted>)")
    }
}

impl Serialize for SessionStateToken {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for SessionStateToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let token = String::deserialize(deserializer)?;
        Self::from_wire(token).map_err(de::Error::custom)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionCookieSameSite {
    Unspecified,
    Strict,
    Lax,
    None,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionCookieV1 {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub host_only: bool,
    pub secure: bool,
    pub http_only: bool,
    pub same_site: SessionCookieSameSite,
    pub expires_unix_time_ns: Option<WireU64>,
    pub partitioned: bool,
    pub creation_sequence: WireU64,
    pub last_access_sequence: WireU64,
}

impl fmt::Debug for SessionCookieV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionCookieV1")
            .field(
                "string_bytes",
                &(self.name.len() + self.value.len() + self.domain.len() + self.path.len()),
            )
            .field("host_only", &self.host_only)
            .field("secure", &self.secure)
            .field("http_only", &self.http_only)
            .field("same_site", &self.same_site)
            .field("persistent", &self.expires_unix_time_ns.is_some())
            .field("partitioned", &self.partitioned)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionStorageEntryV1 {
    pub key: String,
    pub value: String,
}

impl fmt::Debug for SessionStorageEntryV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionStorageEntryV1")
            .field("key_bytes", &self.key.len())
            .field("value_bytes", &self.value.len())
            .finish()
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionOriginStateV1 {
    pub origin: String,
    pub local_storage: Vec<SessionStorageEntryV1>,
    pub session_storage: Vec<SessionStorageEntryV1>,
}

impl fmt::Debug for SessionOriginStateV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionOriginStateV1")
            .field("origin_bytes", &self.origin.len())
            .field("local_storage_entries", &self.local_storage.len())
            .field("session_storage_entries", &self.session_storage.len())
            .finish()
    }
}

/// Complete portable public session state. This type must never enter diagnostics.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionStateV1 {
    pub schema_version: u16,
    pub profile: String,
    pub sensitive: bool,
    pub session_storage_scope: String,
    pub cookies: Vec<SessionCookieV1>,
    pub origins: Vec<SessionOriginStateV1>,
}

impl fmt::Debug for SessionStateV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionStateV1")
            .field("schema_version", &self.schema_version)
            .field("profile", &self.profile)
            .field("sensitive", &self.sensitive)
            .field("session_storage_scope", &self.session_storage_scope)
            .field("cookie_count", &self.cookies.len())
            .field("origin_count", &self.origins.len())
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionCookiesSetParamsV1 {
    pub cookies: Vec<SessionCookieV1>,
    pub expected_session_state_token: SessionStateToken,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionStorageSetParamsV1 {
    pub origins: Vec<SessionOriginStateV1>,
    pub expected_session_state_token: SessionStateToken,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct SessionStateImportParamsV1 {
    pub state: SessionStateV1,
    pub expected_session_state_token: SessionStateToken,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionCookiesResultV1 {
    pub cookies: Vec<SessionCookieV1>,
    pub session_state_token: SessionStateToken,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStorageResultV1 {
    pub origins: Vec<SessionOriginStateV1>,
    pub session_state_token: SessionStateToken,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStateExportResultV1 {
    pub state: SessionStateV1,
    pub session_state_token: SessionStateToken,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStateMutationResultV1 {
    pub session_state_token: SessionStateToken,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SessionStateRevisions {
    pub cookie: u64,
    pub web_storage: u64,
}

#[derive(Clone)]
struct SessionStateBinding {
    revisions: SessionStateRevisions,
    token: SessionStateToken,
}

/// Allocates checked, never-reused aliases for one session and retains only the current binding.
pub struct SessionStateAuthority {
    namespace: Option<OpaqueTokenNamespace>,
    next_alias: Option<u64>,
    current: Option<SessionStateBinding>,
}

impl Default for SessionStateAuthority {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionStateAuthority {
    pub const fn new() -> Self {
        Self {
            namespace: None,
            next_alias: Some(1),
            current: None,
        }
    }

    pub fn observe(
        &mut self,
        revisions: SessionStateRevisions,
    ) -> Result<SessionStateToken, SessionStateError> {
        if let Some(current) = &self.current {
            if revisions.cookie < current.revisions.cookie
                || revisions.web_storage < current.revisions.web_storage
            {
                return Err(SessionStateError::BackendRevisionRegressed);
            }
            if revisions == current.revisions {
                return Ok(current.token.clone());
            }
        }

        let namespace = match self.namespace.as_ref() {
            Some(namespace) => namespace,
            None => {
                let namespace = OpaqueTokenNamespace::generate()
                    .map_err(|_| SessionStateError::TokenEntropyUnavailable)?;
                self.namespace.insert(namespace)
            },
        };
        let alias = self
            .next_alias
            .ok_or(SessionStateError::TokenSpaceExhausted)?;
        let token = SessionStateToken::from_alias(namespace, alias)?;
        self.next_alias = alias.checked_add(1);
        self.current = Some(SessionStateBinding {
            revisions,
            token: token.clone(),
        });
        Ok(token)
    }

    pub fn authorize(
        &mut self,
        expected: &SessionStateToken,
        revisions: SessionStateRevisions,
    ) -> Result<(), SessionStateError> {
        if self.observe(revisions)? != *expected {
            return Err(SessionStateError::StaleSessionStateToken);
        }
        Ok(())
    }

    #[cfg(test)]
    fn with_next_alias(next_alias: u64) -> Self {
        Self {
            namespace: Some(OpaqueTokenNamespace::new_internal([0x51; 16])),
            next_alias: Some(next_alias),
            current: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionStateBackendStage {
    Observe,
    CookieRead,
    WebStorageRead,
    CookieReplace,
    WebStorageReplace,
    Export,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub enum SessionStateError {
    InvalidSchemaVersion,
    InvalidProfile,
    SensitiveMarkerRequired,
    InvalidSessionStorageScope,
    StateTooLarge,
    CookieArrayTooLarge,
    StorageArrayTooLarge,
    TooManyCookies,
    TooManyCookiesPerRegistrableHost,
    CookieTooLarge,
    InvalidCookie,
    DuplicateCookieIdentity,
    DuplicateCreationSequence,
    DuplicateLastAccessSequence,
    CookieTimeRangeUnsupported,
    PersistentCookieUnsupported,
    PartitionedCookieUnsupported,
    TooManyOrigins,
    InvalidOrigin,
    DuplicateOrigin,
    TooManyStorageEntries,
    StorageKeyTooLarge,
    StorageValueTooLarge,
    OriginStorageTooLarge,
    DuplicateStorageKey,
    StaleSessionStateToken,
    TokenEntropyUnavailable,
    TokenSpaceExhausted,
    BackendRevisionRegressed,
    BackendRevisionChanged,
    BackendRejected(SessionStateBackendStage),
}

impl SessionStateError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::InvalidSchemaVersion => "invalid_session_state_schema",
            Self::InvalidProfile => "invalid_session_state_profile",
            Self::SensitiveMarkerRequired => "invalid_session_state_sensitive_marker",
            Self::InvalidSessionStorageScope => "invalid_session_storage_scope",
            Self::StateTooLarge => "session_state_too_large",
            Self::CookieArrayTooLarge => "session_cookie_fragment_too_large",
            Self::StorageArrayTooLarge => "session_storage_fragment_too_large",
            Self::TooManyCookies => "too_many_session_cookies",
            Self::TooManyCookiesPerRegistrableHost => {
                "too_many_session_cookies_per_registrable_host"
            },
            Self::CookieTooLarge => "session_cookie_too_large",
            Self::InvalidCookie => "invalid_session_cookie",
            Self::DuplicateCookieIdentity => "duplicate_session_cookie",
            Self::DuplicateCreationSequence => "duplicate_cookie_creation_sequence",
            Self::DuplicateLastAccessSequence => "duplicate_cookie_access_sequence",
            Self::CookieTimeRangeUnsupported => "unsupported_cookie_time_range",
            Self::PersistentCookieUnsupported => "unsupported_persistent_cookie",
            Self::PartitionedCookieUnsupported => "unsupported_partitioned_cookie",
            Self::TooManyOrigins => "too_many_session_storage_origins",
            Self::InvalidOrigin => "invalid_session_storage_origin",
            Self::DuplicateOrigin => "duplicate_session_storage_origin",
            Self::TooManyStorageEntries => "too_many_session_storage_entries",
            Self::StorageKeyTooLarge => "session_storage_key_too_large",
            Self::StorageValueTooLarge => "session_storage_value_too_large",
            Self::OriginStorageTooLarge => "session_origin_storage_too_large",
            Self::DuplicateStorageKey => "duplicate_session_storage_key",
            Self::StaleSessionStateToken => "stale_session_state_token",
            Self::TokenEntropyUnavailable => "session_state_token_entropy_unavailable",
            Self::TokenSpaceExhausted => "session_state_token_space_exhausted",
            Self::BackendRevisionRegressed => "session_state_backend_revision_regressed",
            Self::BackendRevisionChanged => "session_state_backend_revision_changed",
            Self::BackendRejected(SessionStateBackendStage::Observe) => {
                "session_state_backend_observe_failed"
            },
            Self::BackendRejected(SessionStateBackendStage::CookieRead) => {
                "session_state_cookie_read_failed"
            },
            Self::BackendRejected(SessionStateBackendStage::WebStorageRead) => {
                "session_state_web_storage_read_failed"
            },
            Self::BackendRejected(SessionStateBackendStage::CookieReplace) => {
                "session_state_cookie_replace_failed"
            },
            Self::BackendRejected(SessionStateBackendStage::WebStorageReplace) => {
                "session_state_web_storage_replace_failed"
            },
            Self::BackendRejected(SessionStateBackendStage::Export) => {
                "session_state_export_failed"
            },
        }
    }

    /// Whether this error, when returned from a live session-state mutation method, must be
    /// treated as an indeterminate mutation outcome and fail-stop the owning process.
    ///
    /// `Observe` is deliberately conservative: the same lower error can be raised immediately
    /// before compare-replace or by the mandatory revision observation after compare-replace.
    /// The public shell cannot distinguish those phases once the backend call has been entered,
    /// so it must never claim `stateEffect: none` for this set.
    pub const fn requires_indeterminate_live_mutation_effect(self) -> bool {
        matches!(
            self,
            Self::TokenEntropyUnavailable
                | Self::TokenSpaceExhausted
                | Self::BackendRevisionRegressed
                | Self::BackendRejected(
                    SessionStateBackendStage::Observe
                        | SessionStateBackendStage::CookieReplace
                        | SessionStateBackendStage::WebStorageReplace
                )
        )
    }
}

impl fmt::Debug for SessionStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl fmt::Display for SessionStateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for SessionStateError {}

pub struct PreparedSessionStateImport {
    expected_revisions: SessionStateRevisions,
    cookies: CookieStateSnapshotV1,
    web_storage: WebStorageStateSnapshotV1,
}

impl fmt::Debug for PreparedSessionStateImport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PreparedSessionStateImport")
            .field("expected_revisions", &self.expected_revisions)
            .field("cookie_count", &self.cookies.cookies.len())
            .field("origin_count", &self.web_storage.origins.len())
            .finish()
    }
}

/// Backend proof supplied only after WebViewId allocation and before `NewWebView` publication.
///
/// Implementations must make `abandon` terminal for the unpublished session. This is what makes a
/// second-backend failure externally atomic even though the two Servo threads cannot roll back as
/// one transaction.
pub trait UnpublishedSessionStateBackend: Sized {
    type Error;

    fn revisions(&self) -> Result<SessionStateRevisions, Self::Error>;
    fn replace_cookie_state(
        &mut self,
        expected_revision: u64,
        snapshot: CookieStateSnapshotV1,
    ) -> Result<u64, Self::Error>;
    fn replace_web_storage_state(
        &mut self,
        expected_revision: u64,
        snapshot: WebStorageStateSnapshotV1,
    ) -> Result<u64, Self::Error>;
    fn abandon(self);
}

/// Live backend seam for public cookie/storage reads and single-backend compare-replace methods.
///
/// `revisions` must observe both lower backend revisions for the same shell command turn. Stasis
/// re-reads the pair immediately before each compare-replace and again after it, so a returned
/// token always describes a fresh pair rather than a projection guessed from the target backend.
pub trait LiveSessionStateBackend {
    type Error;

    fn controlled_cookie_policy(&self) -> ControlledCookiePolicy {
        ControlledCookiePolicy::SessionV1
    }
    fn revisions(&self) -> Result<SessionStateRevisions, Self::Error>;
    fn cookie_state(&self) -> Result<CookieStateSnapshotV1, Self::Error>;
    fn web_storage_state(&self) -> Result<WebStorageStateSnapshotV1, Self::Error>;
    fn replace_cookie_state(
        &mut self,
        expected_revision: u64,
        snapshot: CookieStateSnapshotV1,
    ) -> Result<u64, Self::Error>;
    fn replace_web_storage_state(
        &mut self,
        expected_revision: u64,
        snapshot: WebStorageStateSnapshotV1,
    ) -> Result<u64, Self::Error>;
}

/// Production lower-backend adapter for one public controlled-session WebView.
pub struct ServoSessionStateBackend<'a> {
    site_data: &'a SiteDataManager,
    webview_id: WebViewId,
    controlled_cookie_policy: ControlledCookiePolicy,
}

impl<'a> ServoSessionStateBackend<'a> {
    pub const fn new(
        site_data: &'a SiteDataManager,
        webview_id: WebViewId,
        controlled_cookie_policy: ControlledCookiePolicy,
    ) -> Self {
        Self {
            site_data,
            webview_id,
            controlled_cookie_policy,
        }
    }
}

/// Redacted production backend failure. Lower errors are intentionally not retained.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServoSessionStateBackendError {
    Cookie,
    WebStorage,
}

impl ServoSessionStateBackend<'_> {
    fn observed_revisions(&self) -> Result<SessionStateRevisions, ServoSessionStateBackendError> {
        let cookie = self
            .site_data
            .controlled_cookie_state(self.controlled_cookie_policy)
            .map_err(|_| ServoSessionStateBackendError::Cookie)?;
        let web_storage = self
            .site_data
            .webstorage_state(self.webview_id)
            .map_err(|_| ServoSessionStateBackendError::WebStorage)?;
        Ok(SessionStateRevisions {
            cookie: cookie.revision,
            web_storage: web_storage.revision,
        })
    }
}

impl LiveSessionStateBackend for ServoSessionStateBackend<'_> {
    type Error = ServoSessionStateBackendError;

    fn controlled_cookie_policy(&self) -> ControlledCookiePolicy {
        self.controlled_cookie_policy
    }

    fn revisions(&self) -> Result<SessionStateRevisions, Self::Error> {
        self.observed_revisions()
    }

    fn cookie_state(&self) -> Result<CookieStateSnapshotV1, Self::Error> {
        self.site_data
            .controlled_cookie_state(self.controlled_cookie_policy)
            .map_err(|_| ServoSessionStateBackendError::Cookie)
    }

    fn web_storage_state(&self) -> Result<WebStorageStateSnapshotV1, Self::Error> {
        self.site_data
            .webstorage_state(self.webview_id)
            .map_err(|_| ServoSessionStateBackendError::WebStorage)
    }

    fn replace_cookie_state(
        &mut self,
        expected_revision: u64,
        snapshot: CookieStateSnapshotV1,
    ) -> Result<u64, Self::Error> {
        self.site_data
            .replace_controlled_cookie_state(
                self.controlled_cookie_policy,
                expected_revision,
                snapshot,
            )
            .map_err(|_| ServoSessionStateBackendError::Cookie)
    }

    fn replace_web_storage_state(
        &mut self,
        expected_revision: u64,
        snapshot: WebStorageStateSnapshotV1,
    ) -> Result<u64, Self::Error> {
        self.site_data
            .replace_webstorage_state(self.webview_id, expected_revision, snapshot)
            .map_err(|_| ServoSessionStateBackendError::WebStorage)
    }
}

impl UnpublishedSessionStateBackend for ServoSessionStateBackend<'_> {
    type Error = ServoSessionStateBackendError;

    fn revisions(&self) -> Result<SessionStateRevisions, Self::Error> {
        self.observed_revisions()
    }

    fn replace_cookie_state(
        &mut self,
        expected_revision: u64,
        snapshot: CookieStateSnapshotV1,
    ) -> Result<u64, Self::Error> {
        self.site_data
            .replace_controlled_cookie_state(
                self.controlled_cookie_policy,
                expected_revision,
                snapshot,
            )
            .map_err(|_| ServoSessionStateBackendError::Cookie)
    }

    fn replace_web_storage_state(
        &mut self,
        expected_revision: u64,
        snapshot: WebStorageStateSnapshotV1,
    ) -> Result<u64, Self::Error> {
        self.site_data
            .replace_webstorage_state(self.webview_id, expected_revision, snapshot)
            .map_err(|_| ServoSessionStateBackendError::WebStorage)
    }

    fn abandon(self) {
        // The checked WebViewBuilder hook owns publication and drops this unpublished target when
        // initialization returns an error. One process owns one session, so no later target may
        // observe a partial first-backend replace.
    }
}

/// Initialize the public site-data boundary for `session.open` inside the checked WebViewBuilder
/// callback. The WebViewId-scoped sessionStorage backend does not exist before this point.
pub fn initialize_servo_session_state_before_publication(
    site_data: &SiteDataManager,
    webview_id: WebViewId,
    authority: &mut SessionStateAuthority,
    controlled_cookie_policy: ControlledCookiePolicy,
    state: Option<SessionStateV1>,
) -> Result<SessionStateToken, SessionStateError> {
    validate_controlled_cookie_policy_time(controlled_cookie_policy)?;
    let backend = ServoSessionStateBackend::new(site_data, webview_id, controlled_cookie_policy);
    let observed = UnpublishedSessionStateBackend::revisions(&backend)
        .map_err(|_| SessionStateError::BackendRejected(SessionStateBackendStage::Observe))?;
    let Some(state) = state else {
        return authority.observe(observed);
    };
    let prepared = prepare_open_state_import_with_policy(
        authority,
        observed,
        controlled_cookie_policy,
        state,
    )?;
    let initialized = apply_prepared_session_state_import(backend, authority, prepared)?;
    Ok(initialized.session_state_token().clone())
}

pub struct InitializedSessionStateBackend<B> {
    backend: B,
    revisions: SessionStateRevisions,
    token: SessionStateToken,
}

impl<B> InitializedSessionStateBackend<B> {
    pub fn revisions(&self) -> SessionStateRevisions {
        self.revisions
    }

    pub fn session_state_token(&self) -> &SessionStateToken {
        &self.token
    }

    /// Return the initialized target to the builder, which may now publish `NewWebView`.
    pub fn into_inner(self) -> B {
        self.backend
    }
}

/// Validate and authorize an explicit initialization-phase `session.state.import` request.
pub fn prepare_session_state_import(
    authority: &mut SessionStateAuthority,
    observed_revisions: SessionStateRevisions,
    params: SessionStateImportParamsV1,
) -> Result<PreparedSessionStateImport, SessionStateError> {
    prepare_session_state_import_with_policy(
        authority,
        observed_revisions,
        ControlledCookiePolicy::SessionV1,
        params,
    )
}

/// Validate and authorize an initialization-phase import under the selected session profile.
pub fn prepare_session_state_import_with_policy(
    authority: &mut SessionStateAuthority,
    observed_revisions: SessionStateRevisions,
    controlled_cookie_policy: ControlledCookiePolicy,
    params: SessionStateImportParamsV1,
) -> Result<PreparedSessionStateImport, SessionStateError> {
    authority.authorize(&params.expected_session_state_token, observed_revisions)?;
    prepare_state(observed_revisions, controlled_cookie_policy, params.state)
}

/// Validate the `session.open({ state })` sugar using the builder's current hidden token.
pub fn prepare_open_state_import(
    authority: &mut SessionStateAuthority,
    observed_revisions: SessionStateRevisions,
    state: SessionStateV1,
) -> Result<PreparedSessionStateImport, SessionStateError> {
    prepare_open_state_import_with_policy(
        authority,
        observed_revisions,
        ControlledCookiePolicy::SessionV1,
        state,
    )
}

/// Validate `session.open({ state })` under its immutable controlled-session profile.
pub fn prepare_open_state_import_with_policy(
    authority: &mut SessionStateAuthority,
    observed_revisions: SessionStateRevisions,
    controlled_cookie_policy: ControlledCookiePolicy,
    state: SessionStateV1,
) -> Result<PreparedSessionStateImport, SessionStateError> {
    authority.observe(observed_revisions)?;
    prepare_state(observed_revisions, controlled_cookie_policy, state)
}

/// Apply both backend replacements before publication or consume and abandon the target on error.
pub fn apply_prepared_session_state_import<B: UnpublishedSessionStateBackend>(
    mut backend: B,
    authority: &mut SessionStateAuthority,
    prepared: PreparedSessionStateImport,
) -> Result<InitializedSessionStateBackend<B>, SessionStateError> {
    let observed = match backend.revisions() {
        Ok(revisions) => revisions,
        Err(_) => {
            backend.abandon();
            return Err(SessionStateError::BackendRejected(
                SessionStateBackendStage::Observe,
            ));
        },
    };
    if observed != prepared.expected_revisions {
        backend.abandon();
        return Err(SessionStateError::BackendRevisionChanged);
    }

    let cookie_revision = match backend.replace_cookie_state(observed.cookie, prepared.cookies) {
        Ok(revision) => revision,
        Err(_) => {
            backend.abandon();
            return Err(SessionStateError::BackendRejected(
                SessionStateBackendStage::CookieReplace,
            ));
        },
    };
    let web_storage_revision =
        match backend.replace_web_storage_state(observed.web_storage, prepared.web_storage) {
            Ok(revision) => revision,
            Err(_) => {
                backend.abandon();
                return Err(SessionStateError::BackendRejected(
                    SessionStateBackendStage::WebStorageReplace,
                ));
            },
        };

    let revisions = SessionStateRevisions {
        cookie: cookie_revision,
        web_storage: web_storage_revision,
    };
    let token = match authority.observe(revisions) {
        Ok(token) => token,
        Err(error) => {
            backend.abandon();
            return Err(error);
        },
    };
    Ok(InitializedSessionStateBackend {
        backend,
        revisions,
        token,
    })
}

pub fn session_cookies_get<B: LiveSessionStateBackend>(
    backend: &B,
    authority: &mut SessionStateAuthority,
) -> Result<SessionCookiesResultV1, SessionStateError> {
    let controlled_cookie_policy = backend.controlled_cookie_policy();
    validate_controlled_cookie_policy_time(controlled_cookie_policy)?;
    let snapshot = backend
        .cookie_state()
        .map_err(|_| SessionStateError::BackendRejected(SessionStateBackendStage::CookieRead))?;
    let revisions = observe_live_revisions(backend)?;
    if snapshot.revision != revisions.cookie {
        return Err(SessionStateError::BackendRevisionChanged);
    }
    let cookies = wire_cookies(snapshot.cookies);
    validate_cookie_slice(&cookies, controlled_cookie_policy)?;
    Ok(SessionCookiesResultV1 {
        cookies,
        session_state_token: authority.observe(revisions)?,
    })
}

pub fn session_storage_get<B: LiveSessionStateBackend>(
    backend: &B,
    authority: &mut SessionStateAuthority,
) -> Result<SessionStorageResultV1, SessionStateError> {
    validate_controlled_cookie_policy_time(backend.controlled_cookie_policy())?;
    let snapshot = backend.web_storage_state().map_err(|_| {
        SessionStateError::BackendRejected(SessionStateBackendStage::WebStorageRead)
    })?;
    let revisions = observe_live_revisions(backend)?;
    if snapshot.revision != revisions.web_storage {
        return Err(SessionStateError::BackendRevisionChanged);
    }
    let origins = wire_origins(snapshot.origins);
    validate_origin_slice(&origins)?;
    Ok(SessionStorageResultV1 {
        origins,
        session_state_token: authority.observe(revisions)?,
    })
}

pub fn session_state_export<B: LiveSessionStateBackend>(
    backend: &B,
    authority: &mut SessionStateAuthority,
) -> Result<SessionStateExportResultV1, SessionStateError> {
    let controlled_cookie_policy = backend.controlled_cookie_policy();
    validate_controlled_cookie_policy_time(controlled_cookie_policy)?;
    let cookies = backend
        .cookie_state()
        .map_err(|_| SessionStateError::BackendRejected(SessionStateBackendStage::Export))?;
    let storage = backend
        .web_storage_state()
        .map_err(|_| SessionStateError::BackendRejected(SessionStateBackendStage::Export))?;
    let revisions = observe_live_revisions(backend)?;
    if cookies.revision != revisions.cookie || storage.revision != revisions.web_storage {
        return Err(SessionStateError::BackendRevisionChanged);
    }
    let state = wire_state(controlled_cookie_policy, cookies.cookies, storage.origins);
    validate_state_for_policy(&state, controlled_cookie_policy)?;
    Ok(SessionStateExportResultV1 {
        state,
        session_state_token: authority.observe(revisions)?,
    })
}

pub fn session_cookies_set<B: LiveSessionStateBackend>(
    backend: &mut B,
    authority: &mut SessionStateAuthority,
    params: SessionCookiesSetParamsV1,
) -> Result<SessionStateMutationResultV1, SessionStateError> {
    let controlled_cookie_policy = backend.controlled_cookie_policy();
    validate_controlled_cookie_policy_time(controlled_cookie_policy)?;
    let storage = backend.web_storage_state().map_err(|_| {
        SessionStateError::BackendRejected(SessionStateBackendStage::WebStorageRead)
    })?;
    let candidate = SessionStateV1 {
        schema_version: SESSION_STATE_SCHEMA_VERSION_V1,
        profile: controlled_cookie_policy_profile(controlled_cookie_policy).into(),
        sensitive: true,
        session_storage_scope: TOP_LEVEL_SESSION_STORAGE_SCOPE.into(),
        cookies: params.cookies,
        origins: wire_origins(storage.origins),
    };
    validate_state_for_policy(&candidate, controlled_cookie_policy)?;

    // This is the authorization linearization point immediately before compare-replace.
    let observed = observe_live_revisions(backend)?;
    if storage.revision != observed.web_storage {
        return Err(SessionStateError::BackendRevisionChanged);
    }
    authority.authorize(&params.expected_session_state_token, observed)?;
    let cookie_snapshot = CookieStateSnapshotV1 {
        schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
        revision: observed.cookie,
        cookies: backend_cookies(candidate.cookies),
    };
    let replaced_revision = backend
        .replace_cookie_state(observed.cookie, cookie_snapshot)
        .map_err(|_| SessionStateError::BackendRejected(SessionStateBackendStage::CookieReplace))?;
    finish_live_mutation(backend, authority, Some(replaced_revision), None)
}

pub fn session_storage_set<B: LiveSessionStateBackend>(
    backend: &mut B,
    authority: &mut SessionStateAuthority,
    params: SessionStorageSetParamsV1,
) -> Result<SessionStateMutationResultV1, SessionStateError> {
    let controlled_cookie_policy = backend.controlled_cookie_policy();
    validate_controlled_cookie_policy_time(controlled_cookie_policy)?;
    let cookies = backend
        .cookie_state()
        .map_err(|_| SessionStateError::BackendRejected(SessionStateBackendStage::CookieRead))?;
    let candidate = SessionStateV1 {
        schema_version: SESSION_STATE_SCHEMA_VERSION_V1,
        profile: controlled_cookie_policy_profile(controlled_cookie_policy).into(),
        sensitive: true,
        session_storage_scope: TOP_LEVEL_SESSION_STORAGE_SCOPE.into(),
        cookies: wire_cookies(cookies.cookies),
        origins: params.origins,
    };
    validate_state_for_policy(&candidate, controlled_cookie_policy)?;

    // This is the authorization linearization point immediately before compare-replace.
    let observed = observe_live_revisions(backend)?;
    if cookies.revision != observed.cookie {
        return Err(SessionStateError::BackendRevisionChanged);
    }
    authority.authorize(&params.expected_session_state_token, observed)?;
    let storage_snapshot = WebStorageStateSnapshotV1 {
        schema_version: WEB_STORAGE_STATE_SCHEMA_VERSION_V1,
        revision: observed.web_storage,
        origins: backend_origins(candidate.origins),
    };
    let replaced_revision = backend
        .replace_web_storage_state(observed.web_storage, storage_snapshot)
        .map_err(|_| {
            SessionStateError::BackendRejected(SessionStateBackendStage::WebStorageReplace)
        })?;
    finish_live_mutation(backend, authority, None, Some(replaced_revision))
}

fn observe_live_revisions<B: LiveSessionStateBackend>(
    backend: &B,
) -> Result<SessionStateRevisions, SessionStateError> {
    backend
        .revisions()
        .map_err(|_| SessionStateError::BackendRejected(SessionStateBackendStage::Observe))
}

fn finish_live_mutation<B: LiveSessionStateBackend>(
    backend: &B,
    authority: &mut SessionStateAuthority,
    cookie_revision: Option<u64>,
    web_storage_revision: Option<u64>,
) -> Result<SessionStateMutationResultV1, SessionStateError> {
    let revisions = observe_live_revisions(backend)?;
    if cookie_revision.is_some_and(|revision| revisions.cookie < revision)
        || web_storage_revision.is_some_and(|revision| revisions.web_storage < revision)
    {
        return Err(SessionStateError::BackendRevisionRegressed);
    }
    Ok(SessionStateMutationResultV1 {
        session_state_token: authority.observe(revisions)?,
    })
}

fn wire_state(
    controlled_cookie_policy: ControlledCookiePolicy,
    cookies: Vec<CookieStateRecordV1>,
    origins: Vec<WebStorageOriginStateV1>,
) -> SessionStateV1 {
    SessionStateV1 {
        schema_version: SESSION_STATE_SCHEMA_VERSION_V1,
        profile: controlled_cookie_policy_profile(controlled_cookie_policy).into(),
        sensitive: true,
        session_storage_scope: TOP_LEVEL_SESSION_STORAGE_SCOPE.into(),
        cookies: wire_cookies(cookies),
        origins: wire_origins(origins),
    }
}

fn wire_cookies(cookies: Vec<CookieStateRecordV1>) -> Vec<SessionCookieV1> {
    cookies
        .into_iter()
        .map(|cookie| SessionCookieV1 {
            name: cookie.name,
            value: cookie.value,
            domain: cookie.domain,
            path: cookie.path,
            host_only: cookie.host_only,
            secure: cookie.secure,
            http_only: cookie.http_only,
            same_site: match cookie.same_site {
                CookieStateSameSite::Unspecified => SessionCookieSameSite::Unspecified,
                CookieStateSameSite::Strict => SessionCookieSameSite::Strict,
                CookieStateSameSite::Lax => SessionCookieSameSite::Lax,
                CookieStateSameSite::None => SessionCookieSameSite::None,
            },
            expires_unix_time_ns: cookie.expires_unix_time_ns.map(WireU64::new),
            partitioned: cookie.partitioned,
            creation_sequence: WireU64::new(cookie.creation_sequence),
            last_access_sequence: WireU64::new(cookie.last_access_sequence),
        })
        .collect()
}

fn backend_cookies(cookies: Vec<SessionCookieV1>) -> Vec<CookieStateRecordV1> {
    cookies
        .into_iter()
        .map(|cookie| CookieStateRecordV1 {
            name: cookie.name,
            value: cookie.value,
            domain: cookie.domain,
            path: cookie.path,
            host_only: cookie.host_only,
            secure: cookie.secure,
            http_only: cookie.http_only,
            same_site: match cookie.same_site {
                SessionCookieSameSite::Unspecified => CookieStateSameSite::Unspecified,
                SessionCookieSameSite::Strict => CookieStateSameSite::Strict,
                SessionCookieSameSite::Lax => CookieStateSameSite::Lax,
                SessionCookieSameSite::None => CookieStateSameSite::None,
            },
            expires_unix_time_ns: cookie.expires_unix_time_ns.map(WireU64::get),
            partitioned: cookie.partitioned,
            creation_sequence: cookie.creation_sequence.get(),
            last_access_sequence: cookie.last_access_sequence.get(),
        })
        .collect()
}

fn wire_origins(origins: Vec<WebStorageOriginStateV1>) -> Vec<SessionOriginStateV1> {
    origins
        .into_iter()
        .map(|origin| SessionOriginStateV1 {
            origin: origin.origin,
            local_storage: origin
                .local_storage
                .into_iter()
                .map(|entry| SessionStorageEntryV1 {
                    key: entry.key,
                    value: entry.value,
                })
                .collect(),
            session_storage: origin
                .session_storage
                .into_iter()
                .map(|entry| SessionStorageEntryV1 {
                    key: entry.key,
                    value: entry.value,
                })
                .collect(),
        })
        .collect()
}

fn backend_origins(origins: Vec<SessionOriginStateV1>) -> Vec<WebStorageOriginStateV1> {
    origins
        .into_iter()
        .map(|origin| WebStorageOriginStateV1 {
            origin: origin.origin,
            local_storage: origin
                .local_storage
                .into_iter()
                .map(|entry| WebStorageStateEntryV1 {
                    key: entry.key,
                    value: entry.value,
                })
                .collect(),
            session_storage: origin
                .session_storage
                .into_iter()
                .map(|entry| WebStorageStateEntryV1 {
                    key: entry.key,
                    value: entry.value,
                })
                .collect(),
        })
        .collect()
}

fn prepare_state(
    revisions: SessionStateRevisions,
    controlled_cookie_policy: ControlledCookiePolicy,
    state: SessionStateV1,
) -> Result<PreparedSessionStateImport, SessionStateError> {
    validate_state_for_policy(&state, controlled_cookie_policy)?;
    let cookies = CookieStateSnapshotV1 {
        schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
        revision: revisions.cookie,
        cookies: state
            .cookies
            .into_iter()
            .map(|cookie| CookieStateRecordV1 {
                name: cookie.name,
                value: cookie.value,
                domain: cookie.domain,
                path: cookie.path,
                host_only: cookie.host_only,
                secure: cookie.secure,
                http_only: cookie.http_only,
                same_site: match cookie.same_site {
                    SessionCookieSameSite::Unspecified => CookieStateSameSite::Unspecified,
                    SessionCookieSameSite::Strict => CookieStateSameSite::Strict,
                    SessionCookieSameSite::Lax => CookieStateSameSite::Lax,
                    SessionCookieSameSite::None => CookieStateSameSite::None,
                },
                expires_unix_time_ns: cookie.expires_unix_time_ns.map(WireU64::get),
                partitioned: cookie.partitioned,
                creation_sequence: cookie.creation_sequence.get(),
                last_access_sequence: cookie.last_access_sequence.get(),
            })
            .collect(),
    };
    let web_storage = WebStorageStateSnapshotV1 {
        schema_version: WEB_STORAGE_STATE_SCHEMA_VERSION_V1,
        revision: revisions.web_storage,
        origins: state
            .origins
            .into_iter()
            .map(|origin| WebStorageOriginStateV1 {
                origin: origin.origin,
                local_storage: origin
                    .local_storage
                    .into_iter()
                    .map(|entry| WebStorageStateEntryV1 {
                        key: entry.key,
                        value: entry.value,
                    })
                    .collect(),
                session_storage: origin
                    .session_storage
                    .into_iter()
                    .map(|entry| WebStorageStateEntryV1 {
                        key: entry.key,
                        value: entry.value,
                    })
                    .collect(),
            })
            .collect(),
    };
    Ok(PreparedSessionStateImport {
        expected_revisions: revisions,
        cookies,
        web_storage,
    })
}

pub const fn controlled_cookie_policy_profile(
    controlled_cookie_policy: ControlledCookiePolicy,
) -> &'static str {
    match controlled_cookie_policy {
        ControlledCookiePolicy::SessionV1 => CONTROLLED_WEB_SESSION_V1_PROFILE,
        ControlledCookiePolicy::SessionV2 { .. } => CONTROLLED_WEB_SESSION_V2_PROFILE,
    }
}

/// Preserve the frozen v1 validator for existing callers and tests.
pub fn validate_state(state: &SessionStateV1) -> Result<(), SessionStateError> {
    validate_state_for_policy(state, ControlledCookiePolicy::SessionV1)
}

/// Validate one schema-v1 state document under its immutable controlled-session profile.
pub fn validate_state_for_policy(
    state: &SessionStateV1,
    controlled_cookie_policy: ControlledCookiePolicy,
) -> Result<(), SessionStateError> {
    if state.schema_version != SESSION_STATE_SCHEMA_VERSION_V1 {
        return Err(SessionStateError::InvalidSchemaVersion);
    }
    if state.profile != controlled_cookie_policy_profile(controlled_cookie_policy) {
        return Err(SessionStateError::InvalidProfile);
    }
    if !state.sensitive {
        return Err(SessionStateError::SensitiveMarkerRequired);
    }
    if state.session_storage_scope != TOP_LEVEL_SESSION_STORAGE_SCOPE {
        return Err(SessionStateError::InvalidSessionStorageScope);
    }
    validate_cookie_slice(&state.cookies, controlled_cookie_policy)?;
    validate_origin_slice(&state.origins)?;

    let mut counter = SizeCounter::new(MAX_SESSION_STATE_BYTES);
    serde_json::to_writer(&mut counter, state).map_err(|_| SessionStateError::StateTooLarge)?;
    Ok(())
}

fn validate_cookie_slice(
    cookies: &[SessionCookieV1],
    controlled_cookie_policy: ControlledCookiePolicy,
) -> Result<(), SessionStateError> {
    let controlled_now = validate_controlled_cookie_policy_time(controlled_cookie_policy)?;
    if cookies.len() > MAX_SESSION_COOKIES {
        return Err(SessionStateError::TooManyCookies);
    }

    let mut cookie_identities = HashSet::new();
    let mut creation_sequences = HashSet::new();
    let mut access_sequences = HashSet::new();
    let mut cookies_per_registrable_host = HashMap::new();
    for cookie in cookies {
        if matches!(controlled_cookie_policy, ControlledCookiePolicy::SessionV1)
            && cookie.expires_unix_time_ns.is_some()
        {
            return Err(SessionStateError::PersistentCookieUnsupported);
        }
        if cookie.partitioned {
            return Err(SessionStateError::PartitionedCookieUnsupported);
        }
        if let (Some(now), Some(expires_unix_time_ns)) =
            (controlled_now, cookie.expires_unix_time_ns)
        {
            let expiry = expires_unix_time_ns.get();
            if expiry > now {
                let maximum = now
                    .checked_add(MAX_CONTROLLED_COOKIE_LIFETIME_NS)
                    .ok_or(SessionStateError::CookieTimeRangeUnsupported)?;
                if expiry > maximum {
                    return Err(SessionStateError::InvalidCookie);
                }
            }
        }
        if !is_valid_cookie_state_name_and_value(&cookie.name, &cookie.value)
            || !is_canonical_cookie_state_domain(&cookie.domain)
            || !cookie.path.starts_with('/')
            || (cookie.same_site == SessionCookieSameSite::None && !cookie.secure)
            || (!cookie.host_only && is_pub_domain(&cookie.domain))
            || !has_valid_cookie_state_prefix(
                &cookie.name,
                cookie.secure,
                cookie.host_only,
                &cookie.path,
            )
        {
            return Err(SessionStateError::InvalidCookie);
        }
        let cookie_bytes =
            cookie.name.len() + cookie.value.len() + cookie.domain.len() + cookie.path.len();
        if cookie_bytes > MAX_SESSION_COOKIE_BYTES {
            return Err(SessionStateError::CookieTooLarge);
        }
        if !cookie_identities.insert((&cookie.domain, &cookie.path, &cookie.name)) {
            return Err(SessionStateError::DuplicateCookieIdentity);
        }
        if !creation_sequences.insert(cookie.creation_sequence) {
            return Err(SessionStateError::DuplicateCreationSequence);
        }
        if !access_sequences.insert(cookie.last_access_sequence) {
            return Err(SessionStateError::DuplicateLastAccessSequence);
        }
        let registrable_host = cookie_registrable_host(&cookie.domain);
        let count = cookies_per_registrable_host
            .entry(registrable_host)
            .or_insert(0usize);
        *count = count.saturating_add(1);
        if *count > MAX_SESSION_COOKIES_PER_REGISTRABLE_HOST {
            return Err(SessionStateError::TooManyCookiesPerRegistrableHost);
        }
    }
    let mut counter = SizeCounter::new(MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES);
    serde_json::to_writer(&mut counter, cookies)
        .map_err(|_| SessionStateError::CookieArrayTooLarge)?;
    Ok(())
}

/// Validate the controller-owned cookie clock before any backend observation or mutation.
///
/// The general document clock is u128, while the portable cookie artifact is deliberately bounded
/// to Unix nanoseconds representable by u64. V1 carries no cookie-clock authority.
pub fn validate_controlled_cookie_policy_time(
    controlled_cookie_policy: ControlledCookiePolicy,
) -> Result<Option<u64>, SessionStateError> {
    match controlled_cookie_policy {
        ControlledCookiePolicy::SessionV1 => Ok(None),
        ControlledCookiePolicy::SessionV2 { unix_time_ns } => u64::try_from(unix_time_ns)
            .map(Some)
            .map_err(|_| SessionStateError::CookieTimeRangeUnsupported),
    }
}

/// Return exactly the bucket key used by Servo's `CookieStorage` capacity check.
fn cookie_registrable_host(domain: &str) -> String {
    let host_for_ip_parse = domain
        .strip_prefix('[')
        .and_then(|domain| domain.strip_suffix(']'))
        .unwrap_or(domain);
    if let Ok(address) = host_for_ip_parse.parse::<IpAddr>() {
        return address.to_string().to_lowercase();
    }
    reg_suffix(domain).to_lowercase()
}

fn validate_origin_slice(origins_state: &[SessionOriginStateV1]) -> Result<(), SessionStateError> {
    if origins_state.len() > MAX_SESSION_STORAGE_ORIGINS {
        return Err(SessionStateError::TooManyOrigins);
    }
    let mut origins = HashSet::new();
    for origin in origins_state {
        let parsed = Url::parse(&origin.origin).map_err(|_| SessionStateError::InvalidOrigin)?;
        if !matches!(parsed.scheme(), "http" | "https")
            || parsed.origin().ascii_serialization() != origin.origin
        {
            return Err(SessionStateError::InvalidOrigin);
        }
        if !origins.insert(&origin.origin) {
            return Err(SessionStateError::DuplicateOrigin);
        }
        if origin.local_storage.len() > MAX_SESSION_STORAGE_ENTRIES_PER_AREA
            || origin.session_storage.len() > MAX_SESSION_STORAGE_ENTRIES_PER_AREA
        {
            return Err(SessionStateError::TooManyStorageEntries);
        }
        validate_storage_entries(&origin.local_storage)?;
        validate_storage_entries(&origin.session_storage)?;
        let origin_bytes = origin
            .local_storage
            .iter()
            .chain(&origin.session_storage)
            .try_fold(origin.origin.len(), |bytes, entry| {
                bytes
                    .checked_add(entry.key.len())
                    .and_then(|bytes| bytes.checked_add(entry.value.len()))
            })
            .ok_or(SessionStateError::OriginStorageTooLarge)?;
        if origin_bytes > MAX_SESSION_STORAGE_BYTES_PER_ORIGIN {
            return Err(SessionStateError::OriginStorageTooLarge);
        }
    }
    let mut counter = SizeCounter::new(MAX_SESSION_STORAGE_ARRAY_ENCODED_BYTES);
    serde_json::to_writer(&mut counter, origins_state)
        .map_err(|_| SessionStateError::StorageArrayTooLarge)?;
    Ok(())
}

fn validate_storage_entries(entries: &[SessionStorageEntryV1]) -> Result<(), SessionStateError> {
    let mut keys = HashSet::new();
    for entry in entries {
        if entry.key.len() > MAX_SESSION_STORAGE_KEY_BYTES {
            return Err(SessionStateError::StorageKeyTooLarge);
        }
        if entry.value.len() > MAX_SESSION_STORAGE_VALUE_BYTES {
            return Err(SessionStateError::StorageValueTooLarge);
        }
        if !keys.insert(&entry.key) {
            return Err(SessionStateError::DuplicateStorageKey);
        }
    }
    Ok(())
}

struct SizeCounter {
    bytes: usize,
    maximum: usize,
}

impl SizeCounter {
    const fn new(maximum: usize) -> Self {
        Self { bytes: 0, maximum }
    }
}

impl Write for SizeCounter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len())
            .ok_or_else(|| io::Error::other("session state exceeds limit"))?;
        if self.bytes > self.maximum {
            return Err(io::Error::other("session state exceeds limit"));
        }
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use serde_json::json;

    use super::*;

    const TEST_NAMESPACE_HEX: &str = "51515151515151515151515151515151";

    fn test_namespace() -> OpaqueTokenNamespace {
        OpaqueTokenNamespace::new_internal([0x51; 16])
    }

    fn test_authority() -> SessionStateAuthority {
        SessionStateAuthority {
            namespace: Some(test_namespace()),
            next_alias: Some(1),
            current: None,
        }
    }

    fn test_token(alias: u64) -> SessionStateToken {
        SessionStateToken::from_alias(&test_namespace(), alias).unwrap()
    }

    fn cookie() -> SessionCookieV1 {
        SessionCookieV1 {
            name: "auth-name".into(),
            value: "auth-secret".into(),
            domain: "example.com".into(),
            path: "/".into(),
            host_only: true,
            secure: true,
            http_only: true,
            same_site: SessionCookieSameSite::Lax,
            expires_unix_time_ns: None,
            partitioned: false,
            creation_sequence: WireU64::new(0),
            last_access_sequence: WireU64::new(0),
        }
    }

    fn cookies_in_one_registrable_host(count: usize) -> Vec<SessionCookieV1> {
        (0..count)
            .map(|index| {
                let mut record = cookie();
                record.name = format!("bucket-{index:03}");
                record.domain = format!("shard-{}.example.com", index % 3);
                record.creation_sequence = WireU64::new(index as u64);
                record.last_access_sequence = WireU64::new(index as u64);
                record
            })
            .collect()
    }

    fn state() -> SessionStateV1 {
        SessionStateV1 {
            schema_version: SESSION_STATE_SCHEMA_VERSION_V1,
            profile: CONTROLLED_WEB_SESSION_V1_PROFILE.into(),
            sensitive: true,
            session_storage_scope: TOP_LEVEL_SESSION_STORAGE_SCOPE.into(),
            cookies: vec![cookie()],
            origins: vec![SessionOriginStateV1 {
                origin: "https://example.com".into(),
                local_storage: vec![SessionStorageEntryV1 {
                    key: "private-key".into(),
                    value: "private-value".into(),
                }],
                session_storage: vec![],
            }],
        }
    }

    fn v2_state(expires_unix_time_ns: u64) -> SessionStateV1 {
        let mut state = state();
        state.profile = CONTROLLED_WEB_SESSION_V2_PROFILE.into();
        state.cookies[0].expires_unix_time_ns = Some(WireU64::new(expires_unix_time_ns));
        state
    }

    #[test]
    fn strict_wire_shape_matches_typescript_and_redacts_debug() {
        let state = state();
        let encoded = serde_json::to_value(&state).unwrap();
        assert_eq!(encoded["schemaVersion"], 1);
        assert_eq!(encoded["profile"], CONTROLLED_WEB_SESSION_V1_PROFILE);
        assert_eq!(encoded["sensitive"], true);
        assert_eq!(
            encoded["sessionStorageScope"],
            TOP_LEVEL_SESSION_STORAGE_SCOPE
        );
        assert_eq!(encoded["cookies"][0]["creationSequence"], "0");
        assert!(encoded["cookies"][0]["expiresUnixTimeNs"].is_null());
        assert_eq!(encoded["cookies"][0]["partitioned"], false);
        assert_eq!(
            encoded["origins"][0]["localStorage"][0]["key"],
            "private-key"
        );

        let debug = format!("{state:?}");
        assert!(!debug.contains("auth-secret"));
        assert!(!debug.contains("private-key"));
        assert!(!debug.contains("private-value"));

        let mut with_extra = encoded.clone();
        with_extra
            .as_object_mut()
            .unwrap()
            .insert("extra".into(), json!(true));
        assert!(serde_json::from_value::<SessionStateV1>(with_extra).is_err());
        assert!(serde_json::from_value::<WireU64>(json!("01")).is_err());
        assert!(serde_json::from_value::<WireU64>(json!(1)).is_err());
    }

    #[test]
    fn session_state_token_wire_is_domain_separated_and_canonical() {
        let wire = format!("session:{TEST_NAMESPACE_HEX}:1");
        let token: SessionStateToken = serde_json::from_value(json!(wire)).unwrap();
        assert_eq!(token.as_str(), format!("session:{TEST_NAMESPACE_HEX}:1"));
        assert_eq!(
            serde_json::to_value(&token).unwrap(),
            json!(format!("session:{TEST_NAMESPACE_HEX}:1"))
        );
        assert_eq!(format!("{token:?}"), "SessionStateToken(<redacted>)");

        let maximum = format!("session:{TEST_NAMESPACE_HEX}:{}", u64::MAX);
        assert_eq!(maximum.len(), SESSION_STATE_TOKEN_MAX_BYTES);
        let maximum_token: SessionStateToken =
            serde_json::from_value(json!(maximum.clone())).unwrap();
        assert_eq!(maximum_token.as_str(), maximum);

        for invalid in [
            "document:51515151515151515151515151515151:1",
            "1",
            "session:5151515151515151515151515151515:1",
            "session:515151515151515151515151515151511:1",
            "session:5151515151515151515151515151515g:1",
            "session:5151515151515151515151515151515A:1",
            "session:51515151515151515151515151515151:0",
            "session:51515151515151515151515151515151:00",
            "session:51515151515151515151515151515151:01",
            "session:",
            "session:51515151515151515151515151515151:+1",
            "session:51515151515151515151515151515151:-1",
            "session:51515151515151515151515151515151: 1",
            "session:51515151515151515151515151515151:1 ",
            " session:51515151515151515151515151515151:1",
            "session:51515151515151515151515151515151:18446744073709551616",
        ] {
            assert!(
                serde_json::from_value::<SessionStateToken>(json!(invalid)).is_err(),
                "accepted invalid token shape"
            );
        }
        assert!(serde_json::from_value::<SessionStateToken>(json!(1)).is_err());
        assert!(serde_json::from_value::<SessionStateToken>(json!(null)).is_err());
    }

    #[test]
    fn validation_rejects_sensitive_duplicates_and_combined_oversize() {
        let mut duplicate = state();
        duplicate.cookies.push(cookie());
        assert_eq!(
            validate_state(&duplicate),
            Err(SessionStateError::DuplicateCookieIdentity)
        );

        let mut persistent = state();
        persistent.cookies[0].expires_unix_time_ns = Some(WireU64::new(1));
        assert_eq!(
            validate_state(&persistent),
            Err(SessionStateError::PersistentCookieUnsupported)
        );

        let mut partitioned = state();
        partitioned.cookies[0].partitioned = true;
        assert_eq!(
            validate_state(&partitioned),
            Err(SessionStateError::PartitionedCookieUnsupported)
        );

        let mut oversized = state();
        oversized.origins[0].local_storage[0].value = "x".repeat(MAX_SESSION_STATE_BYTES);
        assert!(matches!(
            validate_state(&oversized),
            Err(SessionStateError::StorageValueTooLarge
                | SessionStateError::OriginStorageTooLarge
                | SessionStateError::StateTooLarge)
        ));
    }

    #[test]
    fn v1_is_frozen_while_v2_accepts_bounded_persistent_cookie_state() {
        let maximum_v2_expiry = 42 + MAX_CONTROLLED_COOKIE_LIFETIME_NS;
        let v2 = v2_state(maximum_v2_expiry);
        assert_eq!(validate_state(&v2), Err(SessionStateError::InvalidProfile),);

        let mut v1_with_expiry = v2.clone();
        v1_with_expiry.profile = CONTROLLED_WEB_SESSION_V1_PROFILE.into();
        assert_eq!(
            validate_state(&v1_with_expiry),
            Err(SessionStateError::PersistentCookieUnsupported),
        );

        let policy = ControlledCookiePolicy::SessionV2 { unix_time_ns: 42 };
        assert_eq!(validate_state_for_policy(&v2, policy), Ok(()));
        assert_eq!(
            validate_state_for_policy(&v2_state(maximum_v2_expiry + 1), policy),
            Err(SessionStateError::InvalidCookie),
        );
        assert_eq!(
            validate_state_for_policy(&state(), policy),
            Err(SessionStateError::InvalidProfile),
        );

        let above_u64 = ControlledCookiePolicy::SessionV2 {
            unix_time_ns: u128::from(u64::MAX) + 1,
        };
        let mut empty_v2 = v2_state(0);
        empty_v2.cookies.clear();
        assert_eq!(
            validate_state_for_policy(&empty_v2, above_u64),
            Err(SessionStateError::CookieTimeRangeUnsupported),
            "v2 time is bounded even when the state carries no persistent cookies",
        );

        let without_persistence_headroom = ControlledCookiePolicy::SessionV2 {
            unix_time_ns: u128::from(u64::MAX - 1),
        };
        assert_eq!(
            validate_state_for_policy(&v2_state(u64::MAX), without_persistence_headroom),
            Err(SessionStateError::CookieTimeRangeUnsupported),
        );
        assert_eq!(
            validate_state_for_policy(&v2_state(u64::MAX), ControlledCookiePolicy::SessionV2 {
                unix_time_ns: u128::from(u64::MAX),
            }),
            Ok(()),
            "expiry at controlled now remains a valid lazy-deletion record",
        );
        assert_eq!(v2.schema_version, SESSION_STATE_SCHEMA_VERSION_V1);
    }

    #[test]
    fn registrable_host_cookie_limit_is_exact_and_precedes_backend_mutation() {
        assert_eq!(cookie_registrable_host("a.example.com"), "example.com");
        assert_eq!(cookie_registrable_host("b.example.com"), "example.com");
        assert_eq!(cookie_registrable_host("2001:db8::1"), "2001:db8::1");

        let accepted = cookies_in_one_registrable_host(MAX_SESSION_COOKIES_PER_REGISTRABLE_HOST);
        let mut accepted_state = state();
        accepted_state.cookies = accepted.clone();
        assert_eq!(validate_state(&accepted_state), Ok(()));

        let mut accepted_backend = LiveFakeBackend::empty();
        let mut accepted_authority = test_authority();
        let accepted_token = accepted_authority
            .observe(accepted_backend.revisions_value())
            .unwrap();
        session_cookies_set(
            &mut accepted_backend,
            &mut accepted_authority,
            SessionCookiesSetParamsV1 {
                cookies: accepted,
                expected_session_state_token: accepted_token,
            },
        )
        .unwrap();
        assert_eq!(accepted_backend.cookie_replace_count, 1);
        assert_eq!(
            accepted_backend.cookies.len(),
            MAX_SESSION_COOKIES_PER_REGISTRABLE_HOST
        );

        let rejected =
            cookies_in_one_registrable_host(MAX_SESSION_COOKIES_PER_REGISTRABLE_HOST + 1);
        let mut rejected_state = state();
        rejected_state.cookies = rejected.clone();
        assert_eq!(
            validate_state(&rejected_state),
            Err(SessionStateError::TooManyCookiesPerRegistrableHost)
        );

        let mut rejected_backend = LiveFakeBackend::empty();
        let mut rejected_authority = test_authority();
        let rejected_token = rejected_authority
            .observe(rejected_backend.revisions_value())
            .unwrap();
        assert_eq!(
            session_cookies_set(
                &mut rejected_backend,
                &mut rejected_authority,
                SessionCookiesSetParamsV1 {
                    cookies: rejected,
                    expected_session_state_token: rejected_token,
                },
            ),
            Err(SessionStateError::TooManyCookiesPerRegistrableHost)
        );
        assert_eq!(rejected_backend.cookie_replace_count, 0);
        assert!(rejected_backend.cookies.is_empty());
    }

    #[test]
    fn validation_enforces_the_static_encoded_fragment_partition() {
        let mut cookie_fragment = state();
        cookie_fragment.cookies = (0..63)
            .map(|index| {
                let mut record = cookie();
                record.name = format!("budget-{index:03}");
                let fixed_bytes = record.name.len() + record.domain.len() + record.path.len();
                record.value = "x".repeat(MAX_SESSION_COOKIE_BYTES - fixed_bytes);
                record.creation_sequence = WireU64::new(index);
                record.last_access_sequence = WireU64::new(index);
                record
            })
            .collect();
        assert!(
            serde_json::to_vec(&cookie_fragment.cookies).unwrap().len()
                > MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES
        );
        assert_eq!(
            validate_state(&cookie_fragment),
            Err(SessionStateError::CookieArrayTooLarge)
        );

        let mut storage_fragment = state();
        storage_fragment.origins[0].local_storage = vec![
            SessionStorageEntryV1 {
                key: "first".into(),
                value: "x".repeat(127 * 1024),
            },
            SessionStorageEntryV1 {
                key: "second".into(),
                value: "y".repeat(127 * 1024),
            },
        ];
        assert!(
            serde_json::to_vec(&storage_fragment.origins).unwrap().len()
                > MAX_SESSION_STORAGE_ARRAY_ENCODED_BYTES
        );
        assert_eq!(
            validate_state(&storage_fragment),
            Err(SessionStateError::StorageArrayTooLarge)
        );

        assert_eq!(MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES, 256_000);
        assert_eq!(MAX_SESSION_STORAGE_ARRAY_ENCODED_BYTES, 256_000);
        assert_eq!(MAX_SESSION_STATE_BYTES, 524_288);
        assert_eq!(
            MAX_SESSION_STATE_BYTES
                - MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES
                - MAX_SESSION_STORAGE_ARRAY_ENCODED_BYTES,
            12_288
        );
    }

    #[test]
    fn near_maximum_escaped_fragments_fit_the_combined_state_budget() {
        let mut cookies = Vec::new();
        while cookies.len() < MAX_SESSION_COOKIES {
            let index = cookies.len() as u64;
            let mut record = cookie();
            record.name = format!("escaped-{index:03}");
            record.creation_sequence = WireU64::new(index);
            record.last_access_sequence = WireU64::new(index);
            let fixed = record.name.len() + record.domain.len() + record.path.len();
            let maximum_interior = MAX_SESSION_COOKIE_BYTES - fixed - 2;
            record.value = format!("\"{}\"", "x".repeat(maximum_interior));
            cookies.push(record);
            if serde_json::to_vec(&cookies).unwrap().len() <= MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES
            {
                continue;
            }

            let mut record = cookies.pop().unwrap();
            record.value = "\"\"".into();
            cookies.push(record);
            let minimum = serde_json::to_vec(&cookies).unwrap().len();
            assert!(minimum <= MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES);
            let fill = (MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES - minimum).min(maximum_interior);
            cookies.last_mut().unwrap().value = format!("\"{}\"", "x".repeat(fill));
            break;
        }
        let cookie_bytes = serde_json::to_vec(&cookies).unwrap().len();
        assert_eq!(cookie_bytes, MAX_SESSION_COOKIE_ARRAY_ENCODED_BYTES);

        let mut origins = vec![SessionOriginStateV1 {
            origin: "https://escaped.example.com".into(),
            local_storage: vec![SessionStorageEntryV1 {
                key: "escaped-key".into(),
                value: String::new(),
            }],
            session_storage: Vec::new(),
        }];
        let mut low = 0usize;
        let mut high = MAX_SESSION_STORAGE_VALUE_BYTES;
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            origins[0].local_storage[0].value = "\0".repeat(middle);
            if serde_json::to_vec(&origins).unwrap().len()
                <= MAX_SESSION_STORAGE_ARRAY_ENCODED_BYTES
            {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        origins[0].local_storage[0].value = "\0".repeat(low);
        let origin_bytes = serde_json::to_vec(&origins).unwrap().len();
        assert!(origin_bytes <= MAX_SESSION_STORAGE_ARRAY_ENCODED_BYTES);
        assert!(origin_bytes > MAX_SESSION_STORAGE_ARRAY_ENCODED_BYTES - 6);

        let state = SessionStateV1 {
            schema_version: SESSION_STATE_SCHEMA_VERSION_V1,
            profile: CONTROLLED_WEB_SESSION_V1_PROFILE.into(),
            sensitive: true,
            session_storage_scope: TOP_LEVEL_SESSION_STORAGE_SCOPE.into(),
            cookies,
            origins,
        };
        let full_bytes = serde_json::to_vec(&state).unwrap().len();
        assert_eq!(full_bytes, 147 + cookie_bytes + origin_bytes);
        assert!(full_bytes <= MAX_SESSION_STATE_BYTES);
        assert_eq!(validate_state(&state), Ok(()));
    }

    #[test]
    fn validation_rejects_cookie_request_wire_ambiguity() {
        for (name, value) in [
            ("bad name", "value"),
            ("bad=name", "value"),
            ("bad;name", "value"),
            ("bad\r\nname", "value"),
            ("valid", "bad;value"),
            ("valid", "bad,value"),
            ("valid", "bad\\value"),
            ("valid", "bad\r\nvalue"),
        ] {
            let mut invalid = state();
            invalid.cookies[0].name = name.into();
            invalid.cookies[0].value = value.into();
            assert_eq!(
                validate_state(&invalid),
                Err(SessionStateError::InvalidCookie),
                "accepted invalid cookie pair {name:?}={value:?}"
            );
        }

        for value in ["", "plain=value", "\"quoted=value\""] {
            let mut valid = state();
            valid.cookies[0].value = value.into();
            assert_eq!(validate_state(&valid), Ok(()));
        }

        for domain in [
            "EXAMPLE.COM",
            ".example.com",
            "example.com.",
            "user@example.com",
            "example.com:443",
            "[2001:db8::1]",
            "2001:0db8::1",
        ] {
            let mut invalid = state();
            invalid.cookies[0].domain = domain.into();
            assert_eq!(
                validate_state(&invalid),
                Err(SessionStateError::InvalidCookie),
                "accepted non-canonical cookie domain {domain:?}",
            );
        }

        let mut public_suffix_domain_cookie = state();
        public_suffix_domain_cookie.cookies[0].domain = "com".into();
        public_suffix_domain_cookie.cookies[0].host_only = false;
        assert_eq!(
            validate_state(&public_suffix_domain_cookie),
            Err(SessionStateError::InvalidCookie),
        );

        let mut ipv6 = state();
        ipv6.cookies[0].domain = "2001:db8::1".into();
        assert_eq!(validate_state(&ipv6), Ok(()));

        for (name, secure, host_only, path) in [
            ("__Secure-secret", false, true, "/"),
            ("__Host-secret", false, true, "/"),
            ("__Host-secret", true, false, "/"),
            ("__Host-secret", true, true, "/nested"),
        ] {
            let mut invalid = state();
            invalid.cookies[0].name = name.into();
            invalid.cookies[0].secure = secure;
            invalid.cookies[0].host_only = host_only;
            invalid.cookies[0].path = path.into();
            assert_eq!(
                validate_state(&invalid),
                Err(SessionStateError::InvalidCookie),
                "accepted invalid cookie prefix metadata",
            );
        }
    }

    #[test]
    fn authority_is_stable_for_one_revision_and_checked_against_aba() {
        let first = SessionStateRevisions {
            cookie: 0,
            web_storage: 0,
        };
        let second = SessionStateRevisions {
            cookie: 1,
            web_storage: 0,
        };
        let mut authority = test_authority();
        let first_token = authority.observe(first).unwrap();
        assert_eq!(
            first_token.as_str(),
            format!("session:{TEST_NAMESPACE_HEX}:1")
        );
        assert_eq!(authority.observe(first).unwrap(), first_token);
        let second_token = authority.observe(second).unwrap();
        assert_eq!(
            second_token.as_str(),
            format!("session:{TEST_NAMESPACE_HEX}:2")
        );
        assert_ne!(second_token, first_token);
        assert_eq!(
            authority.authorize(&first_token, second),
            Err(SessionStateError::StaleSessionStateToken)
        );
        assert_eq!(
            authority.observe(first),
            Err(SessionStateError::BackendRevisionRegressed)
        );

        let mut exhausted = SessionStateAuthority::with_next_alias(u64::MAX);
        exhausted.observe(first).unwrap();
        assert_eq!(
            exhausted.observe(second),
            Err(SessionStateError::TokenSpaceExhausted)
        );

        let mut invalid = SessionStateAuthority::with_next_alias(0);
        assert_eq!(
            invalid.observe(first),
            Err(SessionStateError::TokenSpaceExhausted)
        );
    }

    #[test]
    fn session_state_authority_rejects_a_token_from_another_fresh_session() {
        let revisions = SessionStateRevisions {
            cookie: 0,
            web_storage: 0,
        };
        let mut first = test_authority();
        let mut second = SessionStateAuthority {
            namespace: Some(OpaqueTokenNamespace::new_internal([0x52; 16])),
            next_alias: Some(1),
            current: None,
        };
        let foreign = first.observe(revisions).unwrap();
        let local = second.observe(revisions).unwrap();

        assert_ne!(foreign, local);
        assert_eq!(
            second.authorize(&foreign, revisions),
            Err(SessionStateError::StaleSessionStateToken)
        );
        assert!(!format!("{foreign:?}").contains(TEST_NAMESPACE_HEX));
    }

    struct FakeBackend {
        revisions: SessionStateRevisions,
        fail_storage: bool,
        abandoned: Arc<AtomicBool>,
    }

    impl UnpublishedSessionStateBackend for FakeBackend {
        type Error = ();

        fn revisions(&self) -> Result<SessionStateRevisions, Self::Error> {
            Ok(self.revisions)
        }

        fn replace_cookie_state(
            &mut self,
            expected_revision: u64,
            _: CookieStateSnapshotV1,
        ) -> Result<u64, Self::Error> {
            if expected_revision != self.revisions.cookie {
                return Err(());
            }
            self.revisions.cookie += 1;
            Ok(self.revisions.cookie)
        }

        fn replace_web_storage_state(
            &mut self,
            expected_revision: u64,
            _: WebStorageStateSnapshotV1,
        ) -> Result<u64, Self::Error> {
            if self.fail_storage || expected_revision != self.revisions.web_storage {
                return Err(());
            }
            self.revisions.web_storage += 1;
            Ok(self.revisions.web_storage)
        }

        fn abandon(self) {
            self.abandoned.store(true, Ordering::SeqCst);
        }
    }

    struct LiveFakeBackend {
        controlled_cookie_policy: ControlledCookiePolicy,
        cookie_revision: Cell<u64>,
        web_storage_revision: Cell<u64>,
        cookies: Vec<CookieStateRecordV1>,
        origins: Vec<WebStorageOriginStateV1>,
        cookie_replace_count: usize,
        web_storage_replace_count: usize,
        bump_cookie_before_next_observation: Cell<bool>,
        bump_web_storage_during_cookie_replace: bool,
        fail_observation_after_cookie_replace: bool,
        regress_cookie_revision_after_replace: bool,
        fail_next_revision_observation: Cell<bool>,
        fail_cookie_read: bool,
    }

    impl LiveFakeBackend {
        fn empty() -> Self {
            Self {
                controlled_cookie_policy: ControlledCookiePolicy::SessionV1,
                cookie_revision: Cell::new(0),
                web_storage_revision: Cell::new(0),
                cookies: Vec::new(),
                origins: Vec::new(),
                cookie_replace_count: 0,
                web_storage_replace_count: 0,
                bump_cookie_before_next_observation: Cell::new(false),
                bump_web_storage_during_cookie_replace: false,
                fail_observation_after_cookie_replace: false,
                regress_cookie_revision_after_replace: false,
                fail_next_revision_observation: Cell::new(false),
                fail_cookie_read: false,
            }
        }

        fn v2(unix_time_ns: u128) -> Self {
            Self {
                controlled_cookie_policy: ControlledCookiePolicy::SessionV2 { unix_time_ns },
                ..Self::empty()
            }
        }

        fn revisions_value(&self) -> SessionStateRevisions {
            SessionStateRevisions {
                cookie: self.cookie_revision.get(),
                web_storage: self.web_storage_revision.get(),
            }
        }
    }

    impl LiveSessionStateBackend for LiveFakeBackend {
        type Error = &'static str;

        fn controlled_cookie_policy(&self) -> ControlledCookiePolicy {
            self.controlled_cookie_policy
        }

        fn revisions(&self) -> Result<SessionStateRevisions, Self::Error> {
            if self.fail_next_revision_observation.replace(false) {
                return Err("secret post-replace observation detail");
            }
            if self.bump_cookie_before_next_observation.replace(false) {
                self.cookie_revision
                    .set(self.cookie_revision.get().checked_add(1).unwrap());
            }
            Ok(self.revisions_value())
        }

        fn cookie_state(&self) -> Result<CookieStateSnapshotV1, Self::Error> {
            if self.fail_cookie_read {
                return Err("secret backend detail");
            }
            Ok(CookieStateSnapshotV1 {
                schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
                revision: self.cookie_revision.get(),
                cookies: self.cookies.clone(),
            })
        }

        fn web_storage_state(&self) -> Result<WebStorageStateSnapshotV1, Self::Error> {
            Ok(WebStorageStateSnapshotV1 {
                schema_version: WEB_STORAGE_STATE_SCHEMA_VERSION_V1,
                revision: self.web_storage_revision.get(),
                origins: self.origins.clone(),
            })
        }

        fn replace_cookie_state(
            &mut self,
            expected_revision: u64,
            snapshot: CookieStateSnapshotV1,
        ) -> Result<u64, Self::Error> {
            self.cookie_replace_count += 1;
            if expected_revision != self.cookie_revision.get()
                || snapshot.revision != expected_revision
            {
                return Err("cookie compare failed");
            }
            self.cookies = snapshot.cookies;
            let revision = expected_revision
                .checked_add(1)
                .ok_or("cookie revision exhausted")?;
            self.cookie_revision
                .set(if self.regress_cookie_revision_after_replace {
                    expected_revision
                } else {
                    revision
                });
            if self.bump_web_storage_during_cookie_replace {
                self.web_storage_revision.set(
                    self.web_storage_revision
                        .get()
                        .checked_add(1)
                        .ok_or("storage revision exhausted")?,
                );
            }
            if self.fail_observation_after_cookie_replace {
                self.fail_next_revision_observation.set(true);
            }
            Ok(revision)
        }

        fn replace_web_storage_state(
            &mut self,
            expected_revision: u64,
            snapshot: WebStorageStateSnapshotV1,
        ) -> Result<u64, Self::Error> {
            self.web_storage_replace_count += 1;
            if expected_revision != self.web_storage_revision.get()
                || snapshot.revision != expected_revision
            {
                return Err("storage compare failed");
            }
            self.origins = snapshot.origins;
            let revision = expected_revision
                .checked_add(1)
                .ok_or("storage revision exhausted")?;
            self.web_storage_revision.set(revision);
            Ok(revision)
        }
    }

    #[test]
    fn import_authorizes_before_mutation_and_abandons_partial_backend() {
        let initial = SessionStateRevisions {
            cookie: 0,
            web_storage: 0,
        };
        let mut authority = test_authority();
        let token = authority.observe(initial).unwrap();
        let stale = test_token(999);
        assert_eq!(
            prepare_session_state_import(
                &mut authority,
                initial,
                SessionStateImportParamsV1 {
                    state: state(),
                    expected_session_state_token: stale,
                },
            )
            .unwrap_err(),
            SessionStateError::StaleSessionStateToken
        );

        let prepared = prepare_session_state_import(
            &mut authority,
            initial,
            SessionStateImportParamsV1 {
                state: state(),
                expected_session_state_token: token,
            },
        )
        .unwrap();
        let abandoned = Arc::new(AtomicBool::new(false));
        let result = apply_prepared_session_state_import(
            FakeBackend {
                revisions: initial,
                fail_storage: true,
                abandoned: abandoned.clone(),
            },
            &mut authority,
            prepared,
        );
        assert!(matches!(
            result,
            Err(SessionStateError::BackendRejected(
                SessionStateBackendStage::WebStorageReplace
            ))
        ));
        assert!(abandoned.load(Ordering::SeqCst));
        assert_eq!(
            format!(
                "{:?}",
                SessionStateError::BackendRejected(SessionStateBackendStage::WebStorageReplace)
            ),
            "session_state_web_storage_replace_failed"
        );
    }

    #[test]
    fn successful_import_rotates_token_before_builder_publication() {
        let initial = SessionStateRevisions {
            cookie: 0,
            web_storage: 0,
        };
        let mut authority = test_authority();
        let initial_token = authority.observe(initial).unwrap();
        let prepared = prepare_open_state_import(&mut authority, initial, state()).unwrap();
        let initialized = apply_prepared_session_state_import(
            FakeBackend {
                revisions: initial,
                fail_storage: false,
                abandoned: Arc::new(AtomicBool::new(false)),
            },
            &mut authority,
            prepared,
        )
        .unwrap();
        assert_eq!(
            initialized.revisions(),
            SessionStateRevisions {
                cookie: 1,
                web_storage: 1,
            }
        );
        assert_ne!(initialized.session_state_token(), &initial_token);
        assert_eq!(
            format!("{:?}", initialized.session_state_token()),
            "SessionStateToken(<redacted>)"
        );
    }

    #[test]
    fn v2_import_preserves_expiry_and_rotates_before_publication() {
        let initial = SessionStateRevisions {
            cookie: 0,
            web_storage: 0,
        };
        let policy = ControlledCookiePolicy::SessionV2 { unix_time_ns: 42 };
        let mut authority = test_authority();
        let initial_token = authority.observe(initial).unwrap();
        let prepared = prepare_open_state_import_with_policy(
            &mut authority,
            initial,
            policy,
            v2_state(42 + MAX_CONTROLLED_COOKIE_LIFETIME_NS),
        )
        .unwrap();
        assert_eq!(
            prepared.cookies.cookies[0].expires_unix_time_ns,
            Some(42 + MAX_CONTROLLED_COOKIE_LIFETIME_NS),
        );

        let initialized = apply_prepared_session_state_import(
            FakeBackend {
                revisions: initial,
                fail_storage: false,
                abandoned: Arc::new(AtomicBool::new(false)),
            },
            &mut authority,
            prepared,
        )
        .unwrap();
        assert_ne!(initialized.session_state_token(), &initial_token);
    }

    #[test]
    fn v2_live_state_round_trips_expiry_while_v1_export_stays_frozen() {
        let persistent_cookie = backend_cookies(v2_state(1_000).cookies)
            .into_iter()
            .next()
            .unwrap();

        let mut v1_backend = LiveFakeBackend::empty();
        v1_backend.cookies.push(persistent_cookie.clone());
        assert_eq!(
            session_state_export(&v1_backend, &mut test_authority()),
            Err(SessionStateError::PersistentCookieUnsupported),
        );

        let mut v2_backend = LiveFakeBackend::v2(42);
        v2_backend.cookies.push(persistent_cookie);
        let mut authority = test_authority();
        let exported = session_state_export(&v2_backend, &mut authority).unwrap();
        assert_eq!(
            exported.state.schema_version,
            SESSION_STATE_SCHEMA_VERSION_V1
        );
        assert_eq!(exported.state.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);
        assert_eq!(
            exported.state.cookies[0].expires_unix_time_ns,
            Some(WireU64::new(1_000)),
        );

        let mut replacement = cookie();
        replacement.expires_unix_time_ns = Some(WireU64::new(2_000));
        session_cookies_set(
            &mut v2_backend,
            &mut authority,
            SessionCookiesSetParamsV1 {
                cookies: vec![replacement],
                expected_session_state_token: exported.session_state_token,
            },
        )
        .unwrap();
        assert_eq!(v2_backend.cookies[0].expires_unix_time_ns, Some(2_000),);
    }

    #[test]
    fn v2_live_state_mutations_reject_cookie_time_range_before_backend_replace() {
        let policy_time = u128::from(u64::MAX) + 1;

        let mut cookie_backend = LiveFakeBackend::v2(policy_time);
        let mut cookie_authority = test_authority();
        let cookie_token = cookie_authority
            .observe(cookie_backend.revisions_value())
            .unwrap();
        assert_eq!(
            session_cookies_set(
                &mut cookie_backend,
                &mut cookie_authority,
                SessionCookiesSetParamsV1 {
                    cookies: Vec::new(),
                    expected_session_state_token: cookie_token,
                },
            ),
            Err(SessionStateError::CookieTimeRangeUnsupported),
        );
        assert_eq!(cookie_backend.cookie_replace_count, 0);
        assert_eq!(cookie_backend.web_storage_replace_count, 0);

        let mut storage_backend = LiveFakeBackend::v2(policy_time);
        let mut storage_authority = test_authority();
        let storage_token = storage_authority
            .observe(storage_backend.revisions_value())
            .unwrap();
        assert_eq!(
            session_storage_set(
                &mut storage_backend,
                &mut storage_authority,
                SessionStorageSetParamsV1 {
                    origins: Vec::new(),
                    expected_session_state_token: storage_token,
                },
            ),
            Err(SessionStateError::CookieTimeRangeUnsupported),
        );
        assert_eq!(storage_backend.cookie_replace_count, 0);
        assert_eq!(storage_backend.web_storage_replace_count, 0);
    }

    #[test]
    fn v2_lazy_expiry_revision_invalidates_the_pre_expiry_token() {
        struct ExpiringBackend {
            unix_time_ns: Cell<u128>,
            cookie_revision: Cell<u64>,
            expired: Cell<bool>,
            cookie: CookieStateRecordV1,
        }

        impl ExpiringBackend {
            fn purge_if_expired(&self) {
                if !self.expired.get()
                    && self
                        .cookie
                        .expires_unix_time_ns
                        .is_some_and(|expires| u128::from(expires) <= self.unix_time_ns.get())
                {
                    self.expired.set(true);
                    self.cookie_revision
                        .set(self.cookie_revision.get().checked_add(1).unwrap());
                }
            }
        }

        impl LiveSessionStateBackend for ExpiringBackend {
            type Error = ();

            fn controlled_cookie_policy(&self) -> ControlledCookiePolicy {
                ControlledCookiePolicy::SessionV2 {
                    unix_time_ns: self.unix_time_ns.get(),
                }
            }

            fn revisions(&self) -> Result<SessionStateRevisions, Self::Error> {
                self.purge_if_expired();
                Ok(SessionStateRevisions {
                    cookie: self.cookie_revision.get(),
                    web_storage: 0,
                })
            }

            fn cookie_state(&self) -> Result<CookieStateSnapshotV1, Self::Error> {
                self.purge_if_expired();
                Ok(CookieStateSnapshotV1 {
                    schema_version: COOKIE_STATE_SCHEMA_VERSION_V1,
                    revision: self.cookie_revision.get(),
                    cookies: (!self.expired.get())
                        .then(|| self.cookie.clone())
                        .into_iter()
                        .collect(),
                })
            }

            fn web_storage_state(&self) -> Result<WebStorageStateSnapshotV1, Self::Error> {
                Ok(WebStorageStateSnapshotV1 {
                    schema_version: WEB_STORAGE_STATE_SCHEMA_VERSION_V1,
                    revision: 0,
                    origins: Vec::new(),
                })
            }

            fn replace_cookie_state(
                &mut self,
                _: u64,
                _: CookieStateSnapshotV1,
            ) -> Result<u64, Self::Error> {
                Err(())
            }

            fn replace_web_storage_state(
                &mut self,
                _: u64,
                _: WebStorageStateSnapshotV1,
            ) -> Result<u64, Self::Error> {
                Err(())
            }
        }

        let cookie = backend_cookies(v2_state(10).cookies)
            .into_iter()
            .next()
            .unwrap();
        let backend = ExpiringBackend {
            unix_time_ns: Cell::new(9),
            cookie_revision: Cell::new(0),
            expired: Cell::new(false),
            cookie,
        };
        let mut authority = test_authority();
        let before_expiry = authority.observe(backend.revisions().unwrap()).unwrap();

        backend.unix_time_ns.set(10);
        let after_expiry = session_cookies_get(&backend, &mut authority).unwrap();
        assert!(after_expiry.cookies.is_empty());
        assert_ne!(after_expiry.session_state_token, before_expiry);
        assert_eq!(
            authority.authorize(&before_expiry, backend.revisions().unwrap()),
            Err(SessionStateError::StaleSessionStateToken),
        );
    }

    #[test]
    fn live_set_rejects_a_concurrent_other_backend_revision_before_replace() {
        let mut backend = LiveFakeBackend::empty();
        let mut authority = test_authority();
        let expected = authority.observe(backend.revisions_value()).unwrap();

        // Concurrent Web Storage work invalidates authority derived from the old pair.
        backend.web_storage_revision.set(1);
        assert_eq!(
            session_cookies_set(
                &mut backend,
                &mut authority,
                SessionCookiesSetParamsV1 {
                    cookies: vec![cookie()],
                    expected_session_state_token: expected,
                },
            ),
            Err(SessionStateError::StaleSessionStateToken)
        );
        assert_eq!(backend.cookie_replace_count, 0);
    }

    #[test]
    fn live_cookie_set_rejects_lower_invalid_metadata_before_replace() {
        for invalid_cookie in [
            SessionCookieV1 {
                domain: "EXAMPLE.COM".into(),
                ..cookie()
            },
            SessionCookieV1 {
                name: "__Secure-secret".into(),
                secure: false,
                ..cookie()
            },
            SessionCookieV1 {
                name: "__Host-secret".into(),
                host_only: false,
                ..cookie()
            },
            SessionCookieV1 {
                domain: "com".into(),
                host_only: false,
                ..cookie()
            },
        ] {
            let mut backend = LiveFakeBackend::empty();
            let mut authority = test_authority();
            let expected = authority.observe(backend.revisions_value()).unwrap();
            assert_eq!(
                session_cookies_set(
                    &mut backend,
                    &mut authority,
                    SessionCookiesSetParamsV1 {
                        cookies: vec![invalid_cookie],
                        expected_session_state_token: expected,
                    },
                ),
                Err(SessionStateError::InvalidCookie),
            );
            assert_eq!(backend.cookie_replace_count, 0);
            assert_eq!(backend.revisions_value().cookie, 0);
        }
    }

    #[test]
    fn live_set_returns_a_token_for_the_fresh_pair_after_concurrent_work() {
        let mut backend = LiveFakeBackend::empty();
        backend.bump_web_storage_during_cookie_replace = true;
        let mut authority = test_authority();
        let expected = authority.observe(backend.revisions_value()).unwrap();

        let result = session_cookies_set(
            &mut backend,
            &mut authority,
            SessionCookiesSetParamsV1 {
                cookies: vec![cookie()],
                expected_session_state_token: expected,
            },
        )
        .unwrap();
        assert_eq!(
            backend.revisions_value(),
            SessionStateRevisions {
                cookie: 1,
                web_storage: 1,
            }
        );
        assert_eq!(backend.cookie_replace_count, 1);
        assert_eq!(
            authority.observe(backend.revisions_value()).unwrap(),
            result.session_state_token
        );
    }

    #[test]
    fn post_replace_observe_failure_requires_indeterminate_effect() {
        let mut backend = LiveFakeBackend {
            fail_observation_after_cookie_replace: true,
            ..LiveFakeBackend::empty()
        };
        let mut authority = test_authority();
        let expected = authority.observe(backend.revisions_value()).unwrap();

        let error = session_cookies_set(
            &mut backend,
            &mut authority,
            SessionCookiesSetParamsV1 {
                cookies: vec![cookie()],
                expected_session_state_token: expected,
            },
        )
        .unwrap_err();

        assert_eq!(backend.cookie_replace_count, 1);
        assert_eq!(backend.cookies.len(), 1);
        assert_eq!(
            error,
            SessionStateError::BackendRejected(SessionStateBackendStage::Observe)
        );
        assert!(error.requires_indeterminate_live_mutation_effect());
        assert!(!format!("{error:?}").contains("secret post-replace observation detail"));
    }

    #[test]
    fn post_replace_revision_regression_requires_indeterminate_effect() {
        let mut backend = LiveFakeBackend {
            regress_cookie_revision_after_replace: true,
            ..LiveFakeBackend::empty()
        };
        let mut authority = test_authority();
        let expected = authority.observe(backend.revisions_value()).unwrap();

        let error = session_cookies_set(
            &mut backend,
            &mut authority,
            SessionCookiesSetParamsV1 {
                cookies: vec![cookie()],
                expected_session_state_token: expected,
            },
        )
        .unwrap_err();

        assert_eq!(backend.cookie_replace_count, 1);
        assert_eq!(backend.cookies.len(), 1);
        assert_eq!(error, SessionStateError::BackendRevisionRegressed);
        assert!(error.requires_indeterminate_live_mutation_effect());
    }

    #[test]
    fn post_replace_token_exhaustion_requires_indeterminate_effect() {
        let mut backend = LiveFakeBackend::empty();
        let mut authority = SessionStateAuthority::with_next_alias(u64::MAX);
        let expected = authority.observe(backend.revisions_value()).unwrap();

        let error = session_cookies_set(
            &mut backend,
            &mut authority,
            SessionCookiesSetParamsV1 {
                cookies: vec![cookie()],
                expected_session_state_token: expected,
            },
        )
        .unwrap_err();

        assert_eq!(backend.cookie_replace_count, 1);
        assert_eq!(backend.cookies.len(), 1);
        assert_eq!(error, SessionStateError::TokenSpaceExhausted);
        assert!(error.requires_indeterminate_live_mutation_effect());
        assert!(
            !SessionStateError::StaleSessionStateToken
                .requires_indeterminate_live_mutation_effect()
        );
    }

    #[test]
    fn live_read_rejects_a_snapshot_that_changed_before_pair_observation() {
        let backend = LiveFakeBackend {
            bump_cookie_before_next_observation: Cell::new(true),
            ..LiveFakeBackend::empty()
        };
        let mut authority = test_authority();
        assert_eq!(
            session_cookies_get(&backend, &mut authority),
            Err(SessionStateError::BackendRevisionChanged)
        );
    }

    #[test]
    fn live_storage_set_and_export_round_trip_the_canonical_state() {
        let expected_state = state();
        let mut backend = LiveFakeBackend::empty();
        backend.cookies = backend_cookies(expected_state.cookies.clone());
        let mut authority = test_authority();
        let expected = authority.observe(backend.revisions_value()).unwrap();

        let mutation = session_storage_set(
            &mut backend,
            &mut authority,
            SessionStorageSetParamsV1 {
                origins: expected_state.origins.clone(),
                expected_session_state_token: expected,
            },
        )
        .unwrap();
        assert_eq!(backend.web_storage_replace_count, 1);
        assert_eq!(
            authority.observe(backend.revisions_value()).unwrap(),
            mutation.session_state_token
        );

        let exported = session_state_export(&backend, &mut authority).unwrap();
        assert_eq!(exported.state, expected_state);
        assert_eq!(exported.session_state_token, mutation.session_state_token);
    }

    #[test]
    fn live_export_enforces_the_combined_cap_and_backend_errors_are_redacted() {
        let mut backend = LiveFakeBackend::empty();
        backend.origins = (0..5)
            .map(|index| WebStorageOriginStateV1 {
                origin: format!("https://{index}.example.com"),
                local_storage: vec![WebStorageStateEntryV1 {
                    key: "key".into(),
                    value: "x".repeat(110 * 1024),
                }],
                session_storage: Vec::new(),
            })
            .collect();
        let mut authority = test_authority();
        assert_eq!(
            session_state_export(&backend, &mut authority),
            Err(SessionStateError::StorageArrayTooLarge)
        );

        backend.fail_cookie_read = true;
        let error = session_cookies_get(&backend, &mut authority).unwrap_err();
        assert_eq!(error.code(), "session_state_cookie_read_failed");
        assert_eq!(format!("{error:?}"), "session_state_cookie_read_failed");
        assert!(!format!("{error:?}").contains("secret backend detail"));
    }
}
