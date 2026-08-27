/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use content_security_policy::Destination;
use embedder_traits::{
    ControlledCookieContext, GenericEmbedderProxy, WebResourceCookiePolicyFailure, WebResourceKind,
    WebResourceLoadId, WebResourceLoadTerminal, WebResourceRequest, WebResourceResponseMsg,
};
use log::error;
use net_traits::http_status::HttpStatus;
use net_traits::request::{Request, RequestMode, RequestOriginatingApi};
use net_traits::response::{Response, ResponseBody};
use net_traits::{ControlledCookiePolicyError, NetworkError};

use crate::embedder::NetToEmbedderMsg;
use crate::fetch::methods::FetchContext;

#[derive(Clone)]
pub struct RequestInterceptor {
    embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>,
}

pub struct InterceptedRequest {
    pub load_id: WebResourceLoadId,
    pub controlled_cookie_context: Option<ControlledCookieContext>,
    pub fixture_response: bool,
}

impl RequestInterceptor {
    pub fn new(embedder_proxy: GenericEmbedderProxy<NetToEmbedderMsg>) -> RequestInterceptor {
        RequestInterceptor { embedder_proxy }
    }

    pub async fn intercept_request(
        &self,
        request: &mut Request,
        response: &mut Option<Response>,
        context: &FetchContext,
    ) -> InterceptedRequest {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let is_for_main_frame = matches!(request.destination, Destination::Document);
        let web_resource_request = WebResourceRequest {
            method: request.method.clone(),
            // Redirect recursion appends to the Fetch request URL list. Evidence and fixture
            // matching belong to this hop, not the original URL at the head of that list.
            url: request.current_url().into_url(),
            headers: request.headers.clone(),
            destination: request.destination,
            referrer_url: request.referrer.to_url().map(|url| url.as_url().clone()),
            is_for_main_frame,
            is_redirect: request.redirect_count > 0,
            controlled_load_id: controlled_load_id(request),
            controlled_body_bytes: request.body.as_ref().map_or(Some(0), |body| {
                body.len().and_then(|length| u64::try_from(length).ok())
            }),
            controlled_resource_kind: controlled_resource_kind(
                request.destination,
                request.originating_api,
            ),
        };

        let controlled_load_id = web_resource_request.controlled_load_id;

        self.embedder_proxy
            .send(NetToEmbedderMsg::WebResourceRequested(
                request.target_webview_id,
                web_resource_request,
                sender,
            ));

        // TODO: use done_chan and run in CoreResourceThreadPool.
        let mut accumulated_body = Vec::new();
        let mut controlled_cookie_context = None;
        let mut fixture_response = false;
        while let Some(message) = receiver.recv().await {
            match message {
                WebResourceResponseMsg::ControlledSession { cookie_context } => {
                    controlled_cookie_context = Some(cookie_context);
                },
                WebResourceResponseMsg::Start(webresource_response) => {
                    fixture_response = true;
                    let timing = context.timing.inner().clone();
                    let mut response_override =
                        Response::new(webresource_response.url.into(), timing);
                    response_override.headers = webresource_response.headers;
                    response_override.status = HttpStatus::new(
                        webresource_response.status_code,
                        webresource_response.status_message,
                    );
                    *response = Some(response_override);
                },
                WebResourceResponseMsg::SendBodyData(data) => {
                    accumulated_body.push(data);
                },
                WebResourceResponseMsg::FinishLoad => {
                    if accumulated_body.is_empty() {
                        break;
                    }
                    let Some(response) = response.as_mut() else {
                        error!("Received unexpected FinishLoad message");
                        break;
                    };
                    *response.body.lock() =
                        ResponseBody::Done(accumulated_body.into_iter().flatten().collect());
                    break;
                },
                WebResourceResponseMsg::CancelLoad => {
                    *response = Some(Response::network_error(NetworkError::LoadCancelled));
                    break;
                },
                WebResourceResponseMsg::DoNotIntercept => break,
            }
        }
        InterceptedRequest {
            load_id: controlled_load_id,
            controlled_cookie_context,
            fixture_response,
        }
    }

    /// Report only bounded terminal facts. The response body itself never crosses this boundary.
    pub fn notify_terminal(
        &self,
        target_webview_id: Option<servo_base::id::WebViewId>,
        load_id: WebResourceLoadId,
        response: &Response,
        controlled_cookie_failure: Option<ControlledCookiePolicyError>,
    ) {
        let terminal = if let Some(failure) = controlled_cookie_failure {
            WebResourceLoadTerminal::ControlledCookiePolicyRejected(match failure {
                ControlledCookiePolicyError::SameSiteContextUnsupported => {
                    WebResourceCookiePolicyFailure::SameSiteContextUnsupported
                },
                ControlledCookiePolicyError::PersistentCookieUnsupported => {
                    WebResourceCookiePolicyFailure::PersistentCookieUnsupported
                },
                ControlledCookiePolicyError::PartitionedCookieUnsupported => {
                    WebResourceCookiePolicyFailure::PartitionedCookieUnsupported
                },
                ControlledCookiePolicyError::TimeRangeUnsupported => {
                    WebResourceCookiePolicyFailure::TimeRangeUnsupported
                },
                ControlledCookiePolicyError::InvalidCookie => {
                    WebResourceCookiePolicyFailure::InvalidCookie
                },
            })
        } else if response.is_network_error() {
            WebResourceLoadTerminal::Failed
        } else {
            let actual = response.actual_response();
            let Some(status) = actual.status.try_code() else {
                return;
            };
            let response_bytes = match &*actual.body.lock() {
                ResponseBody::Empty => 0,
                ResponseBody::Receiving(bytes) | ResponseBody::Done(bytes) => {
                    u64::try_from(bytes.len()).expect("response body length must fit u64")
                },
            };
            WebResourceLoadTerminal::Completed {
                status: status.as_u16(),
                response_bytes,
            }
        };
        self.embedder_proxy
            .send(NetToEmbedderMsg::WebResourceFinished(
                target_webview_id,
                load_id,
                terminal,
            ));
    }

    /// Complete the current HTTP(S) hop before Fetch synchronously begins its successor hop.
    ///
    /// Ordinary Fetch/XHR redirects recurse inside the same Net fetch invocation. Without this
    /// boundary, interception for the successor retires the still-active predecessor before its
    /// 3xx response can reach controlled-network evidence. Manual redirects (including Servo's
    /// top-level navigation path) do not use this synchronous recursion and remain terminalized by
    /// the existing outer fetch boundary.
    pub fn notify_follow_redirect_terminal(
        &self,
        request: &Request,
        response: &Response,
        controlled_cookie_failure: Option<ControlledCookiePolicyError>,
    ) {
        if request.redirect_mode != net_traits::request::RedirectMode::Follow
            || request.mode == RequestMode::Navigate
            || response.is_network_error()
            || !response
                .actual_response()
                .status
                .try_code()
                .is_some_and(|status| status.is_redirection())
        {
            return;
        }
        self.notify_terminal(
            request.target_webview_id,
            controlled_load_id(request),
            response,
            controlled_cookie_failure,
        );
    }
}

fn controlled_load_id(request: &Request) -> WebResourceLoadId {
    WebResourceLoadId::new(
        controlled_request_identity(request),
        controlled_redirect_index(request),
    )
}

fn controlled_redirect_index(request: &Request) -> u32 {
    // Navigation redirects cross Script's asynchronous FetchRedirect boundary. The replayed
    // RequestBuilder already carries the predecessor response URL, then Net consumes ResponseInit
    // by incrementing redirect_count once more before the successor hop reaches interception.
    // Remove exactly that replay-only increment; later hops otherwise advance one-for-one.
    if request.mode == RequestMode::Navigate && request.redirect_count > 0 {
        request.redirect_count - 1
    } else {
        request.redirect_count
    }
}

fn controlled_request_identity(request: &Request) -> [u8; 16] {
    let Some(pipeline_id) = request
        .pipeline_id
        .filter(|_| request.mode == RequestMode::Navigate)
    else {
        return *request.id.0.as_bytes();
    };

    // Script rebuilds a navigation RequestBuilder for every asynchronous manual-redirect replay,
    // which gives each hop a fresh RequestId. The reserved pipeline remains stable for the entire
    // load, so namespace navigation hop identity by that owner instead. The fixed prefix has a
    // non-v4 version nibble (byte 6), making this namespace disjoint from RequestId's UUID v4
    // values while the globally unique PipelineId occupies the remaining eight bytes.
    let mut identity = *b"STASIS\0\x02\0\0\0\0\0\0\0\0";
    identity[8..].copy_from_slice(&u64::from(pipeline_id).to_be_bytes());
    identity
}

fn controlled_resource_kind(
    destination: Destination,
    originating_api: RequestOriginatingApi,
) -> WebResourceKind {
    match destination {
        Destination::Document => WebResourceKind::Navigation,
        Destination::Image => WebResourceKind::Image,
        Destination::Font => WebResourceKind::Font,
        Destination::Style => WebResourceKind::Stylesheet,
        Destination::Script => WebResourceKind::Script,
        Destination::None => match originating_api {
            RequestOriginatingApi::Fetch => WebResourceKind::Fetch,
            RequestOriginatingApi::XmlHttpRequest => WebResourceKind::XmlHttpRequest,
            RequestOriginatingApi::Unclassified => WebResourceKind::UnclassifiedProducerIo,
        },
        _ => WebResourceKind::Other,
    }
}

#[cfg(test)]
mod tests {
    use net_traits::blob_url_store::UrlWithBlobClaim;
    use net_traits::request::{Referrer, RequestBuilder, RequestId};
    use servo_base::id::TEST_PIPELINE_ID;
    use uuid::Uuid;

    use super::*;

    fn navigation_request(urls: &[&str]) -> Request {
        let urls = urls
            .iter()
            .map(|url| ServoUrl::parse(url).unwrap())
            .collect::<Vec<_>>();
        RequestBuilder::new(
            None,
            UrlWithBlobClaim::from_url_without_having_claimed_blob(urls[0].clone()),
            Referrer::NoReferrer,
        )
        .mode(RequestMode::Navigate)
        .pipeline_id(Some(TEST_PIPELINE_ID))
        .url_list(urls)
        .build()
    }

    #[test]
    fn rebuilt_navigation_redirects_share_pipeline_scoped_identity() {
        let mut first = navigation_request(&["https://example.test/start"]);
        // Script retained the predecessor response URL and Net appended the redirect location
        // while consuming ResponseInit, so the actual intercepted successor has count two.
        let mut successor = navigation_request(&[
            "https://example.test/start",
            "https://example.test/start",
            "https://example.test/next",
        ]);
        let later_successor = navigation_request(&[
            "https://example.test/start",
            "https://example.test/start",
            "https://example.test/next",
            "https://example.test/final",
        ]);
        first.id = RequestId(Uuid::from_u128(1));
        successor.id = RequestId(Uuid::from_u128(2));

        assert_ne!(first.id, successor.id);
        let first_id = controlled_load_id(&first);
        let successor_id = controlled_load_id(&successor);
        assert_eq!(successor_id.redirect_parent(), Some(first_id));
        assert_eq!(controlled_redirect_index(&first), 0);
        assert_eq!(controlled_redirect_index(&successor), 1);
        assert_eq!(controlled_redirect_index(&later_successor), 2);
        assert_eq!(controlled_request_identity(&first)[6] >> 4, 0);
    }

    #[test]
    fn non_navigation_requests_keep_their_uuid_identity() {
        let request = RequestBuilder::new(
            None,
            UrlWithBlobClaim::from_url_without_having_claimed_blob(
                ServoUrl::parse("https://example.test/data").unwrap(),
            ),
            Referrer::NoReferrer,
        )
        .pipeline_id(Some(TEST_PIPELINE_ID))
        .build();

        assert_eq!(
            controlled_request_identity(&request),
            *request.id.0.as_bytes()
        );
    }
}
