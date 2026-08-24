/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! Pure, bounded network-fixture matching for controlled Servo sessions.
//!
//! This module deliberately has no callback, clock, filesystem, or networking surface. A table is
//! fully decoded, validated, and compiled before a WebView is created; request handling is then a
//! side-effect-free first-match lookup returning only fixed bytes or a fixed abort reason.

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

pub const MAX_FIXTURE_ROUTES: usize = 256;
pub const MAX_FIXTURE_METHOD_BYTES: usize = 32;
pub const MAX_FIXTURE_URL_BYTES: usize = 4 * 1024;
pub const MAX_FIXTURE_RESPONSE_HEADERS: usize = 64;
pub const MAX_FIXTURE_RESPONSE_HEADER_BYTES: usize = 16 * 1024;
pub const MAX_FIXTURE_RESPONSE_BODY_BYTES: usize = 256 * 1024;
pub const MAX_FIXTURE_AGGREGATE_BYTES: usize = 320 * 1024;
/// Maximum canonical JSON size of the immutable route table inside `session.open`.
///
/// This leaves bounded room in the frozen 1 MiB request frame for the URL, clock, optional
/// 512 KiB session-state artifact, and the NDJSON envelope.
pub const MAX_FIXTURE_ENCODED_TABLE_BYTES: usize = 384 * 1024;

const GLOB_SENTINEL: &str = "stasisfixturewildcard";

/// Strict serde input for one immutable fixture table.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct NetworkFixtureSpec {
    mode: NetworkFixtureMode,
    routes: Vec<RouteInput>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum NetworkFixtureMode {
    FixturesOnly,
    Mixed,
    Live,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteInput {
    #[serde(rename = "match")]
    matcher: RouteMatchInput,
    fulfill: Option<FulfillInput>,
    abort: Option<AbortInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RouteMatchInput {
    method: String,
    url: UrlMatcherInput,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UrlMatcherInput {
    exact: Option<String>,
    prefix: Option<String>,
    glob: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FulfillInput {
    status: u16,
    #[serde(default)]
    headers: Vec<(String, String)>,
    body: Option<FixtureBodyInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureBodyInput {
    utf8: Option<String>,
    base64: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AbortInput {
    reason: String,
}

#[derive(Clone)]
enum CompiledUrlMatcher {
    Exact(String),
    Prefix(String),
    SimpleGlob(String),
}

impl CompiledUrlMatcher {
    fn matches(&self, url: &str) -> bool {
        match self {
            Self::Exact(pattern) => url == pattern,
            Self::Prefix(pattern) => url.starts_with(pattern),
            Self::SimpleGlob(pattern) => simple_glob_matches(pattern.as_bytes(), url.as_bytes()),
        }
    }
}

#[derive(Clone)]
struct CompiledRoute {
    method: String,
    url: CompiledUrlMatcher,
    action: FixtureAction,
}

/// Validated response header. Values are intentionally omitted from `Debug` surfaces.
#[derive(Clone, Eq, PartialEq)]
pub struct FixtureHeader {
    name: String,
    value: String,
}

impl FixtureHeader {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn value(&self) -> &str {
        &self.value
    }
}

/// Fixed response bytes. This type intentionally does not implement `Debug`.
#[derive(Clone, Eq, PartialEq)]
pub struct FulfillResponse {
    status: u16,
    headers: Arc<[FixtureHeader]>,
    body: Arc<[u8]>,
}

impl FulfillResponse {
    pub const fn status(&self) -> u16 {
        self.status
    }

    pub fn headers(&self) -> &[FixtureHeader] {
        &self.headers
    }

    pub fn body(&self) -> &[u8] {
        &self.body
    }
}

#[derive(Clone)]
enum FixtureAction {
    Fulfill(FulfillResponse),
    Abort(FixtureAbort),
}

/// Validated stable abort reason. It intentionally does not implement `Debug`.
#[derive(Clone, Eq, PartialEq)]
pub struct FixtureAbort {
    reason: String,
}

impl FixtureAbort {
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

/// One pure route lookup result. A miss is strict: callers must not fall through to live network.
pub enum FixtureDecision<'a> {
    Fulfill {
        route_index: usize,
        response: &'a FulfillResponse,
    },
    Abort {
        route_index: usize,
        abort: &'a FixtureAbort,
    },
    StrictMiss,
    Passthrough,
}

/// Immutable, shareable, fully validated route table.
#[derive(Clone)]
pub struct NetworkFixtureTable {
    mode: NetworkFixtureMode,
    routes: Arc<[CompiledRoute]>,
    aggregate_bytes: usize,
}

impl NetworkFixtureTable {
    /// Decode a strict DTO. Serde diagnostics are deliberately collapsed so invalid secret-bearing
    /// input can never be copied into a long-lived validation error.
    pub fn from_json(value: Value) -> Result<Self, NetworkFixtureError> {
        let encoded_bytes = serde_json::to_vec(&value)
            .map_err(|_| NetworkFixtureError::InvalidJson)?
            .len();
        if encoded_bytes > MAX_FIXTURE_ENCODED_TABLE_BYTES {
            return Err(NetworkFixtureError::EncodedTableBytesExceeded {
                observed: encoded_bytes,
                limit: MAX_FIXTURE_ENCODED_TABLE_BYTES,
            });
        }
        let spec = serde_json::from_value(value).map_err(|_| NetworkFixtureError::InvalidJson)?;
        Self::compile(spec)
    }

    pub fn compile(spec: NetworkFixtureSpec) -> Result<Self, NetworkFixtureError> {
        if spec.routes.len() > MAX_FIXTURE_ROUTES {
            return Err(NetworkFixtureError::TooManyRoutes {
                observed: spec.routes.len(),
                limit: MAX_FIXTURE_ROUTES,
            });
        }

        let mut aggregate_bytes = 0usize;
        let mut routes = Vec::with_capacity(spec.routes.len());
        for (route_index, route) in spec.routes.into_iter().enumerate() {
            let method = validate_method(route_index, route.matcher.method)?;
            add_aggregate(&mut aggregate_bytes, method.len())?;
            let url = compile_url_matcher(route_index, route.matcher.url, &mut aggregate_bytes)?;
            let action = compile_action(
                route_index,
                route.fulfill,
                route.abort,
                &mut aggregate_bytes,
            )?;
            routes.push(CompiledRoute {
                method,
                url,
                action,
            });
        }

        Ok(Self {
            mode: spec.mode,
            routes: routes.into(),
            aggregate_bytes,
        })
    }

    pub fn route_count(&self) -> usize {
        self.routes.len()
    }

    pub const fn mode(&self) -> NetworkFixtureMode {
        self.mode
    }

    pub fn aggregate_bytes(&self) -> usize {
        self.aggregate_bytes
    }

    /// Match a canonical Servo URL. This never consumes a rule and never invokes host work.
    pub fn decide<'a>(
        &'a self,
        method: &str,
        url: &Url,
    ) -> Result<FixtureDecision<'a>, FixtureRequestError> {
        if !is_http_token(method.as_bytes()) || method.len() > MAX_FIXTURE_METHOD_BYTES {
            return Err(FixtureRequestError::InvalidMethod);
        }
        let url = url.as_str();
        if url.len() > MAX_FIXTURE_URL_BYTES {
            return Err(FixtureRequestError::UrlTooLong {
                observed: url.len(),
                limit: MAX_FIXTURE_URL_BYTES,
            });
        }
        if !matches!(url.get(..5), Some("http:")) && !matches!(url.get(..6), Some("https:")) {
            return Err(FixtureRequestError::UnsupportedScheme);
        }

        for (route_index, route) in self.routes.iter().enumerate() {
            if route.method.eq_ignore_ascii_case(method) && route.url.matches(url) {
                return Ok(match &route.action {
                    FixtureAction::Fulfill(response) => FixtureDecision::Fulfill {
                        route_index,
                        response,
                    },
                    FixtureAction::Abort(abort) => FixtureDecision::Abort { route_index, abort },
                });
            }
        }
        Ok(match self.mode {
            NetworkFixtureMode::FixturesOnly => FixtureDecision::StrictMiss,
            NetworkFixtureMode::Mixed | NetworkFixtureMode::Live => FixtureDecision::Passthrough,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NetworkFixtureError {
    InvalidJson,
    EncodedTableBytesExceeded {
        observed: usize,
        limit: usize,
    },
    TooManyRoutes {
        observed: usize,
        limit: usize,
    },
    EmptyMethod {
        route_index: usize,
    },
    MethodTooLong {
        route_index: usize,
        observed: usize,
        limit: usize,
    },
    InvalidMethod {
        route_index: usize,
    },
    UrlPatternTooLong {
        route_index: usize,
        observed: usize,
        limit: usize,
    },
    InvalidUrlPattern {
        route_index: usize,
    },
    UrlMatcherArity {
        route_index: usize,
    },
    UnsupportedUrlScheme {
        route_index: usize,
    },
    UrlCredentialsForbidden {
        route_index: usize,
    },
    UrlFragmentForbidden {
        route_index: usize,
    },
    InvalidStatus {
        route_index: usize,
        status: u16,
    },
    ActionArity {
        route_index: usize,
    },
    InvalidAbortReason {
        route_index: usize,
    },
    TooManyHeaders {
        route_index: usize,
        observed: usize,
        limit: usize,
    },
    InvalidHeaderName {
        route_index: usize,
        header_index: usize,
    },
    ForbiddenResponseHeader {
        route_index: usize,
        header_index: usize,
    },
    InvalidHeaderValue {
        route_index: usize,
        header_index: usize,
    },
    HeaderBytesExceeded {
        route_index: usize,
        observed: usize,
        limit: usize,
    },
    BodyBytesExceeded {
        route_index: usize,
        observed: usize,
        limit: usize,
    },
    BodyEncodingArity {
        route_index: usize,
    },
    InvalidBase64Body {
        route_index: usize,
    },
    AggregateBytesExceeded {
        limit: usize,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FixtureRequestError {
    InvalidMethod,
    UrlTooLong { observed: usize, limit: usize },
    UnsupportedScheme,
}

fn validate_method(route_index: usize, method: String) -> Result<String, NetworkFixtureError> {
    if method.is_empty() {
        return Err(NetworkFixtureError::EmptyMethod { route_index });
    }
    if method.len() > MAX_FIXTURE_METHOD_BYTES {
        return Err(NetworkFixtureError::MethodTooLong {
            route_index,
            observed: method.len(),
            limit: MAX_FIXTURE_METHOD_BYTES,
        });
    }
    if !is_http_token(method.as_bytes()) {
        return Err(NetworkFixtureError::InvalidMethod { route_index });
    }
    Ok(method.to_ascii_uppercase())
}

fn compile_url_matcher(
    route_index: usize,
    matcher: UrlMatcherInput,
    aggregate_bytes: &mut usize,
) -> Result<CompiledUrlMatcher, NetworkFixtureError> {
    let populated = usize::from(matcher.exact.is_some())
        + usize::from(matcher.prefix.is_some())
        + usize::from(matcher.glob.is_some());
    if populated != 1 {
        return Err(NetworkFixtureError::UrlMatcherArity { route_index });
    }
    let (value, kind) = match (matcher.exact, matcher.prefix, matcher.glob) {
        (Some(value), None, None) => (value, 0),
        (None, Some(value), None) => (value, 1),
        (None, None, Some(value)) => (value, 2),
        _ => unreachable!("validated exactly one URL matcher"),
    };
    if value.len() > MAX_FIXTURE_URL_BYTES {
        return Err(NetworkFixtureError::UrlPatternTooLong {
            route_index,
            observed: value.len(),
            limit: MAX_FIXTURE_URL_BYTES,
        });
    }
    if value.is_empty() || value.to_ascii_lowercase().contains(GLOB_SENTINEL) {
        return Err(NetworkFixtureError::InvalidUrlPattern { route_index });
    }

    let probe = if kind == 2 {
        value.replace('*', GLOB_SENTINEL)
    } else {
        value.clone()
    };
    let parsed =
        Url::parse(&probe).map_err(|_| NetworkFixtureError::InvalidUrlPattern { route_index })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(NetworkFixtureError::UnsupportedUrlScheme { route_index });
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(NetworkFixtureError::UrlCredentialsForbidden { route_index });
    }
    if parsed.fragment().is_some() {
        return Err(NetworkFixtureError::UrlFragmentForbidden { route_index });
    }

    let canonical = if kind == 2 {
        parsed.as_str().replace(GLOB_SENTINEL, "*")
    } else {
        parsed.as_str().to_owned()
    };
    add_aggregate(aggregate_bytes, canonical.len())?;
    Ok(match kind {
        0 => CompiledUrlMatcher::Exact(canonical),
        1 => CompiledUrlMatcher::Prefix(canonical),
        _ => CompiledUrlMatcher::SimpleGlob(canonical),
    })
}

fn compile_action(
    route_index: usize,
    fulfill: Option<FulfillInput>,
    abort: Option<AbortInput>,
    aggregate_bytes: &mut usize,
) -> Result<FixtureAction, NetworkFixtureError> {
    match (fulfill, abort) {
        (None, None) | (Some(_), Some(_)) => Err(NetworkFixtureError::ActionArity { route_index }),
        (None, Some(abort)) => {
            if !matches!(
                abort.reason.as_str(),
                "blocked_by_fixture" | "connection_reset" | "network_error"
            ) {
                return Err(NetworkFixtureError::InvalidAbortReason { route_index });
            }
            add_aggregate(aggregate_bytes, abort.reason.len())?;
            Ok(FixtureAction::Abort(FixtureAbort {
                reason: abort.reason,
            }))
        },
        (
            Some(FulfillInput {
                status,
                headers,
                body,
            }),
            None,
        ) => {
            if !(200..=599).contains(&status) {
                return Err(NetworkFixtureError::InvalidStatus {
                    route_index,
                    status,
                });
            }
            if headers.len() > MAX_FIXTURE_RESPONSE_HEADERS {
                return Err(NetworkFixtureError::TooManyHeaders {
                    route_index,
                    observed: headers.len(),
                    limit: MAX_FIXTURE_RESPONSE_HEADERS,
                });
            }

            let mut header_bytes = 0usize;
            let mut compiled_headers = Vec::with_capacity(headers.len());
            for (header_index, (header_name, header_value)) in headers.into_iter().enumerate() {
                if !is_http_token(header_name.as_bytes()) {
                    return Err(NetworkFixtureError::InvalidHeaderName {
                        route_index,
                        header_index,
                    });
                }
                let name = header_name.to_ascii_lowercase();
                if forbidden_response_header(&name) {
                    return Err(NetworkFixtureError::ForbiddenResponseHeader {
                        route_index,
                        header_index,
                    });
                }
                if !valid_header_value(&header_value) {
                    return Err(NetworkFixtureError::InvalidHeaderValue {
                        route_index,
                        header_index,
                    });
                }
                header_bytes = header_bytes
                    .checked_add(name.len())
                    .and_then(|bytes| bytes.checked_add(header_value.len()))
                    .ok_or(NetworkFixtureError::HeaderBytesExceeded {
                        route_index,
                        observed: usize::MAX,
                        limit: MAX_FIXTURE_RESPONSE_HEADER_BYTES,
                    })?;
                if header_bytes > MAX_FIXTURE_RESPONSE_HEADER_BYTES {
                    return Err(NetworkFixtureError::HeaderBytesExceeded {
                        route_index,
                        observed: header_bytes,
                        limit: MAX_FIXTURE_RESPONSE_HEADER_BYTES,
                    });
                }
                compiled_headers.push(FixtureHeader {
                    name,
                    value: header_value,
                });
            }

            let body = match body {
                None => Vec::new(),
                Some(FixtureBodyInput { utf8, base64 }) => match (utf8, base64) {
                    (Some(value), None) => value.into_bytes(),
                    (None, Some(value)) => BASE64_STANDARD
                        .decode(value)
                        .map_err(|_| NetworkFixtureError::InvalidBase64Body { route_index })?,
                    _ => return Err(NetworkFixtureError::BodyEncodingArity { route_index }),
                },
            };
            if body.len() > MAX_FIXTURE_RESPONSE_BODY_BYTES {
                return Err(NetworkFixtureError::BodyBytesExceeded {
                    route_index,
                    observed: body.len(),
                    limit: MAX_FIXTURE_RESPONSE_BODY_BYTES,
                });
            }
            add_aggregate(aggregate_bytes, header_bytes)?;
            add_aggregate(aggregate_bytes, body.len())?;
            Ok(FixtureAction::Fulfill(FulfillResponse {
                status,
                headers: compiled_headers.into(),
                body: body.into(),
            }))
        },
    }
}

fn add_aggregate(total: &mut usize, amount: usize) -> Result<(), NetworkFixtureError> {
    *total = total
        .checked_add(amount)
        .ok_or(NetworkFixtureError::AggregateBytesExceeded {
            limit: MAX_FIXTURE_AGGREGATE_BYTES,
        })?;
    if *total > MAX_FIXTURE_AGGREGATE_BYTES {
        return Err(NetworkFixtureError::AggregateBytesExceeded {
            limit: MAX_FIXTURE_AGGREGATE_BYTES,
        });
    }
    Ok(())
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

fn valid_header_value(value: &str) -> bool {
    value
        .as_bytes()
        .iter()
        .copied()
        .all(|byte| byte == b'\t' || (byte >= 0x20 && byte != 0x7f))
}

fn forbidden_response_header(name: &str) -> bool {
    matches!(
        name,
        "connection"
            | "content-length"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "authorization"
            | "cookie"
    )
}

/// Linear wildcard matcher where `*` is the only metacharacter.
fn simple_glob_matches(pattern: &[u8], value: &[u8]) -> bool {
    let (mut pattern_index, mut value_index) = (0usize, 0usize);
    let mut star = None;
    let mut star_value_index = 0usize;

    while value_index < value.len() {
        if pattern.get(pattern_index) == value.get(value_index)
            && pattern.get(pattern_index) != Some(&b'*')
        {
            pattern_index += 1;
            value_index += 1;
        } else if pattern.get(pattern_index) == Some(&b'*') {
            star = Some(pattern_index);
            pattern_index += 1;
            star_value_index = value_index;
        } else if let Some(star_index) = star {
            pattern_index = star_index + 1;
            star_value_index += 1;
            value_index = star_value_index;
        } else {
            return false;
        }
    }
    while pattern.get(pattern_index) == Some(&b'*') {
        pattern_index += 1;
    }
    pattern_index == pattern.len()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn table(routes: Value) -> Result<NetworkFixtureTable, NetworkFixtureError> {
        NetworkFixtureTable::from_json(json!({ "mode": "fixtures_only", "routes": routes }))
    }

    fn fixture_error(
        result: Result<NetworkFixtureTable, NetworkFixtureError>,
    ) -> NetworkFixtureError {
        match result {
            Ok(_) => panic!("expected fixture validation to fail"),
            Err(error) => error,
        }
    }

    fn fulfill(method: &str, url: Value, body: &str) -> Value {
        json!({
            "match": { "method": method, "url": url },
            "fulfill": {
                "status": 200,
                "headers": [["content-type", "text/plain"]],
                "body": {"utf8": body},
            }
        })
    }

    #[test]
    fn exact_prefix_and_simple_glob_use_first_match_without_consuming_rules() {
        let table = table(json!([
            fulfill(
                "GET",
                json!({"exact": "https://example.test/exact"}),
                "exact"
            ),
            fulfill(
                "GET",
                json!({"prefix": "https://example.test/api/"}),
                "prefix"
            ),
            fulfill(
                "POST",
                json!({"glob": "https://*.example.test/items/*"}),
                "glob"
            ),
        ]))
        .unwrap();

        for (method, url, expected_index, expected_body) in [
            ("GET", "https://example.test/exact", 0, b"exact".as_slice()),
            (
                "GET",
                "https://example.test/api/42",
                1,
                b"prefix".as_slice(),
            ),
            (
                "POST",
                "https://shop.example.test/items/42",
                2,
                b"glob".as_slice(),
            ),
        ] {
            for _ in 0..2 {
                let FixtureDecision::Fulfill {
                    route_index,
                    response,
                } = table.decide(method, &Url::parse(url).unwrap()).unwrap()
                else {
                    panic!("expected a fixture fulfillment")
                };
                assert_eq!(route_index, expected_index);
                assert_eq!(response.body(), expected_body);
            }
        }
    }

    #[test]
    fn abort_and_strict_miss_never_imply_live_fallback() {
        let table = table(json!([{
            "match": {
                "method": "GET",
                "url": {"exact": "https://example.test/abort"}
            },
            "abort": {"reason": "connection_reset"}
        }]))
        .unwrap();

        assert!(matches!(
            table
                .decide("GET", &Url::parse("https://example.test/abort").unwrap())
                .unwrap(),
            FixtureDecision::Abort { route_index: 0, abort }
                if abort.reason() == "connection_reset"
        ));
        assert!(matches!(
            table
                .decide("GET", &Url::parse("https://example.test/live").unwrap())
                .unwrap(),
            FixtureDecision::StrictMiss
        ));
    }

    #[test]
    fn abort_reason_is_a_frozen_allowlist() {
        for reason in ["", "timeout", "CONNECTION_RESET", "network error"] {
            assert!(matches!(
                table(json!([{
                    "match": {
                        "method": "GET",
                        "url": {"exact": "https://example.test/abort"}
                    },
                    "abort": {"reason": reason}
                }])),
                Err(NetworkFixtureError::InvalidAbortReason { route_index: 0 })
            ));
        }
    }

    #[test]
    fn serde_rejects_unknown_fields_and_status_is_bounded() {
        assert_eq!(
            fixture_error(NetworkFixtureTable::from_json(
                json!({"mode": "fixtures_only", "routes": [], "callback": "host"}),
            )),
            NetworkFixtureError::InvalidJson,
        );
        let route = json!([{
            "match": {"method": "GET", "url": {"exact": "https://example.test/"}},
            "fulfill": {"status": 101, "headers": [], "body": {"utf8": ""}}
        }]);
        assert_eq!(
            fixture_error(table(route)),
            NetworkFixtureError::InvalidStatus {
                route_index: 0,
                status: 101
            }
        );
    }

    #[test]
    fn route_and_url_limits_are_enforced() {
        let routes = (0..=MAX_FIXTURE_ROUTES)
            .map(|index| {
                fulfill(
                    "GET",
                    json!({"exact": format!("https://example.test/{index}")}),
                    "",
                )
            })
            .collect::<Vec<_>>();
        assert!(matches!(
            table(Value::Array(routes)),
            Err(NetworkFixtureError::TooManyRoutes { observed, limit })
                if observed == MAX_FIXTURE_ROUTES + 1 && limit == MAX_FIXTURE_ROUTES
        ));

        let too_long = format!("https://example.test/{}", "x".repeat(MAX_FIXTURE_URL_BYTES));
        assert!(matches!(
            table(json!([fulfill("GET", json!({"exact": too_long}), "")])),
            Err(NetworkFixtureError::UrlPatternTooLong { route_index: 0, .. })
        ));
    }

    #[test]
    fn headers_are_bounded_and_hop_by_hop_or_secret_request_headers_are_rejected() {
        let headers = (0..=MAX_FIXTURE_RESPONSE_HEADERS)
            .map(|index| json!([format!("x-{index}"), "v"]))
            .collect::<Vec<_>>();
        let route = json!([{
            "match": {"method": "GET", "url": {"exact": "https://example.test/"}},
            "fulfill": {"status": 200, "headers": headers, "body": {"utf8": ""}}
        }]);
        assert!(matches!(
            table(route),
            Err(NetworkFixtureError::TooManyHeaders { route_index: 0, .. })
        ));

        for name in [
            "connection",
            "transfer-encoding",
            "content-length",
            "authorization",
            "cookie",
        ] {
            let route = json!([{
                "match": {"method": "GET", "url": {"exact": "https://example.test/"}},
                "fulfill": {"status": 200, "headers": [[name, "secret"]], "body": {"utf8": ""}}
            }]);
            assert!(matches!(
                table(route),
                Err(NetworkFixtureError::ForbiddenResponseHeader {
                    route_index: 0,
                    header_index: 0
                })
            ));
        }

        let injection = json!([{
            "match": {"method": "GET", "url": {"exact": "https://example.test/"}},
            "fulfill": {"status": 200, "headers": [["x-safe", "ok\r\ninjected: yes"]], "body": {"utf8": ""}}
        }]);
        assert!(matches!(
            table(injection),
            Err(NetworkFixtureError::InvalidHeaderValue {
                route_index: 0,
                header_index: 0
            })
        ));
    }

    #[test]
    fn header_body_and_aggregate_byte_limits_are_enforced() {
        let oversized_headers = json!([{
            "match": {"method": "GET", "url": {"exact": "https://example.test/"}},
            "fulfill": {"status": 200, "headers": [["x-large", "v".repeat(MAX_FIXTURE_RESPONSE_HEADER_BYTES)]], "body": {"utf8": ""}}
        }]);
        assert!(matches!(
            table(oversized_headers),
            Err(NetworkFixtureError::HeaderBytesExceeded { route_index: 0, .. })
        ));

        let oversized_body = "x".repeat(MAX_FIXTURE_RESPONSE_BODY_BYTES + 1);
        assert!(matches!(
            table(json!([fulfill(
                "GET",
                json!({"exact": "https://example.test/"}),
                &oversized_body
            )])),
            Err(NetworkFixtureError::BodyBytesExceeded { route_index: 0, .. })
        ));

        let body = "x".repeat(MAX_FIXTURE_AGGREGATE_BYTES / 2);
        let routes = (0..2)
            .map(|index| {
                fulfill(
                    "GET",
                    json!({"exact": format!("https://example.test/{index}")}),
                    &body,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            fixture_error(table(Value::Array(routes))),
            NetworkFixtureError::AggregateBytesExceeded {
                limit: MAX_FIXTURE_AGGREGATE_BYTES,
            }
        );
    }

    #[test]
    fn encoded_table_limit_keeps_the_public_fixture_inside_the_request_frame() {
        let route = fulfill(
            "GET",
            json!({"exact": "https://example.test/encoded"}),
            &"x".repeat(MAX_FIXTURE_ENCODED_TABLE_BYTES),
        );
        assert!(matches!(
            table(json!([route])),
            Err(NetworkFixtureError::EncodedTableBytesExceeded { observed, limit })
                if observed > limit && limit == MAX_FIXTURE_ENCODED_TABLE_BYTES
        ));

        let maximum_binary_body = vec![0u8; MAX_FIXTURE_RESPONSE_BODY_BYTES];
        let canonical_base64 = BASE64_STANDARD.encode(maximum_binary_body);
        let table = NetworkFixtureTable::from_json(json!({
            "mode": "fixtures_only",
            "routes": [{
                "match": {
                    "method": "GET",
                    "url": {"exact": "https://example.test/maximum-binary"}
                },
                "fulfill": {"status": 200, "body": {"base64": canonical_base64}}
            }]
        }))
        .expect("one maximum binary response must fit the encoded route-table budget");
        assert_eq!(
            table.aggregate_bytes(),
            MAX_FIXTURE_RESPONSE_BODY_BYTES + 38
        );
    }

    #[test]
    fn patterns_cannot_embed_credentials_or_fragments() {
        for value in [
            "https://alice:secret@example.test/",
            "https://example.test/#secret",
        ] {
            assert!(matches!(
                table(json!([fulfill("GET", json!({"exact": value}), "")])),
                Err(NetworkFixtureError::UrlCredentialsForbidden { route_index: 0 })
                    | Err(NetworkFixtureError::UrlFragmentForbidden { route_index: 0 })
            ));
        }
    }

    #[test]
    fn public_shape_requires_exactly_one_matcher_and_action() {
        for url in [
            json!({}),
            json!({"exact": "https://example.test/", "prefix": "https://example.test/"}),
        ] {
            assert!(matches!(
                table(json!([{
                    "match": {"method": "GET", "url": url},
                    "abort": {"reason": "network_error"}
                }])),
                Err(NetworkFixtureError::UrlMatcherArity { route_index: 0 })
            ));
        }

        assert!(matches!(
            table(json!([{
                "match": {"method": "GET", "url": {"exact": "https://example.test/"}},
                "fulfill": {"status": 200},
                "abort": {"reason": "network_error"}
            }])),
            Err(NetworkFixtureError::ActionArity { route_index: 0 })
        ));
    }

    #[test]
    fn base64_body_is_decoded_once_during_compilation() {
        let table = table(json!([{
            "match": {"method": "GET", "url": {"exact": "https://example.test/data"}},
            "fulfill": {"status": 200, "body": {"base64": "AAEC/w=="}}
        }]))
        .unwrap();
        let FixtureDecision::Fulfill { response, .. } = table
            .decide("GET", &Url::parse("https://example.test/data").unwrap())
            .unwrap()
        else {
            panic!("expected fixture fulfillment")
        };
        assert_eq!(response.body(), &[0, 1, 2, 255]);
    }

    #[test]
    fn body_requires_exactly_one_valid_encoding() {
        for body in [json!({}), json!({"utf8": "a", "base64": "Yg=="})] {
            assert!(matches!(
                table(json!([{
                    "match": {"method": "GET", "url": {"exact": "https://example.test/"}},
                    "fulfill": {"status": 200, "body": body}
                }])),
                Err(NetworkFixtureError::BodyEncodingArity { route_index: 0 })
            ));
        }
        assert!(matches!(
            table(json!([{
                "match": {"method": "GET", "url": {"exact": "https://example.test/"}},
                "fulfill": {"status": 200, "body": {"base64": "not base64"}}
            }])),
            Err(NetworkFixtureError::InvalidBase64Body { route_index: 0 })
        ));
    }

    #[test]
    fn mixed_and_live_misses_are_explicit_passthrough() {
        for mode in ["mixed", "live"] {
            let table =
                NetworkFixtureTable::from_json(json!({"mode": mode, "routes": []})).unwrap();
            assert!(matches!(
                table
                    .decide("GET", &Url::parse("https://example.test/").unwrap())
                    .unwrap(),
                FixtureDecision::Passthrough
            ));
        }
    }
}
