/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use embedder_traits::{
    ControlledCookieContext, WebResourceCookiePolicyFailure, WebResourceLoadTerminal,
    WebResourceResponse,
};
use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use net_traits::controlled_network::{
    ControlledNetworkAction, ControlledNetworkCookieFailure, ControlledNetworkRequest,
    ControlledNetworkSession, ControlledNetworkTerminal,
};
use net_traits::network_evidence::{EvidenceResourceKind, MAX_EVIDENCE_HEADER_NAMES};

use crate::webview_delegate::WebResourceLoad;

pub(crate) fn handle_request(
    session: &ControlledNetworkSession,
    mut load: WebResourceLoad,
    site_for_cookies: Option<url::Url>,
) {
    let request = load.request();
    let header_names = request
        .headers
        .keys()
        .take(MAX_EVIDENCE_HEADER_NAMES + 1)
        .map(HeaderName::as_str)
        .collect::<Vec<_>>();
    let resource_kind = EvidenceResourceKind::from(request.controlled_resource_kind);
    let top_level_navigation = request.is_for_main_frame;
    let (action, policy) = session.begin_with_cookie_policy(ControlledNetworkRequest {
        load_id: request.controlled_load_id,
        method: request.method.as_str(),
        url: &request.url,
        resource_kind,
        main_frame: request.is_for_main_frame,
        header_names: &header_names,
        body_bytes: request.controlled_body_bytes,
    });
    load.mark_controlled_session(ControlledCookieContext {
        policy,
        site_for_cookies,
        top_level_navigation,
    });
    match action {
        ControlledNetworkAction::Fulfill { handle, response } => {
            let mut headers = HeaderMap::new();
            for header in response.headers() {
                let name = HeaderName::from_bytes(header.name().as_bytes())
                    .expect("fixture header names were validated before WebView creation");
                let value = HeaderValue::from_bytes(header.value().as_bytes())
                    .expect("fixture header values were validated before WebView creation");
                headers.append(name, value);
            }
            let status = StatusCode::from_u16(response.status())
                .expect("fixture status was validated before WebView creation");
            let status_message = status
                .canonical_reason()
                .unwrap_or_default()
                .as_bytes()
                .to_vec();
            let response_head = WebResourceResponse::new(load.request().url.clone())
                .headers(headers)
                .status_code(status)
                .status_message(status_message);
            let mut intercepted = load.intercept(response_head);
            if !response.body().is_empty() {
                intercepted.send_body_data(response.body().to_vec());
            }
            intercepted.finish();
            // Net owns the terminal boundary for fixture and live responses alike. In
            // particular, controlled-session cookie validation runs there before this request
            // may be reported complete.
            let _ = handle;
        },
        ControlledNetworkAction::Abort { .. } => load.cancel(),
        ControlledNetworkAction::Passthrough { .. } => {
            // Dropping an unanswered load preserves Servo's existing DoNotIntercept path.
            drop(load);
        },
    }
}

pub(crate) fn handle_terminal(
    session: &ControlledNetworkSession,
    load_id: embedder_traits::WebResourceLoadId,
    terminal: WebResourceLoadTerminal,
) {
    let terminal = match terminal {
        WebResourceLoadTerminal::Completed {
            status,
            response_bytes,
        } => ControlledNetworkTerminal::Completed {
            status,
            response_bytes,
        },
        WebResourceLoadTerminal::Failed => ControlledNetworkTerminal::Failed,
        WebResourceLoadTerminal::ControlledCookiePolicyRejected(failure) => {
            ControlledNetworkTerminal::CookiePolicyRejected(match failure {
                WebResourceCookiePolicyFailure::SameSiteContextUnsupported => {
                    ControlledNetworkCookieFailure::SameSiteContextUnsupported
                },
                WebResourceCookiePolicyFailure::PersistentCookieUnsupported => {
                    ControlledNetworkCookieFailure::PersistentCookieUnsupported
                },
                WebResourceCookiePolicyFailure::PartitionedCookieUnsupported => {
                    ControlledNetworkCookieFailure::PartitionedCookieUnsupported
                },
                WebResourceCookiePolicyFailure::TimeRangeUnsupported => {
                    ControlledNetworkCookieFailure::TimeRangeUnsupported
                },
                WebResourceCookiePolicyFailure::InvalidCookie => {
                    ControlledNetworkCookieFailure::InvalidCookie
                },
            })
        },
    };
    session.live_terminal(load_id, terminal);
}
