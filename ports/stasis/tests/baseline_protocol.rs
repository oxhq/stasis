/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::mpsc::{Receiver, sync_channel};
use std::thread;
use std::time::Duration;

use serde_json::{Value, json};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const FIXTURE: &[u8] = include_bytes!("../../../fixtures/baseline/index.html");
const GLOBAL_SCOPE_MESSAGE_PORT_SOURCE: &str =
    include_str!("../../../components/script/dom/globalscope/globalscope.rs");
const MESSAGE_CHANNEL_SOURCE: &str =
    include_str!("../../../components/script/dom/globalscope/messagechannel.rs");
const MESSAGE_PORT_SOURCE: &str =
    include_str!("../../../components/script/dom/globalscope/messageport.rs");
const HTML_IMAGE_ELEMENT_SOURCE: &str =
    include_str!("../../../components/script/dom/html/embedded_content/htmlimageelement.rs");
const SVG_SVG_ELEMENT_SOURCE: &str =
    include_str!("../../../components/script/dom/svg/svgsvgelement.rs");
const WINDOW_SOURCE: &str = include_str!("../../../components/script/dom/window/window.rs");
const SCRIPT_THREAD_SOURCE: &str =
    include_str!("../../../components/script/event_loop/script_thread.rs");
const MESSAGE_CHANNEL_FIXTURE: &[u8] = include_bytes!("fixtures/message_channel.html");
const BUFFERED_MESSAGE_CHANNEL_FIXTURE: &[u8] =
    include_bytes!("fixtures/message_channel_buffered.html");
const MULTI_PAIR_MESSAGE_CHANNEL_FIXTURE: &[u8] =
    include_bytes!("fixtures/message_channel_multi_pair.html");
const RECURSIVE_MESSAGE_CHANNEL_FIXTURE: &[u8] =
    include_bytes!("fixtures/message_channel_recursive.html");
const MESSAGE_CHANNEL_TRANSFER_FIXTURE: &[u8] =
    include_bytes!("fixtures/message_channel_transfer.html");
const MESSAGE_CHANNEL_CLOSED_CAPACITY_FIXTURE: &[u8] =
    include_bytes!("fixtures/message_channel_closed_capacity.html");
const MESSAGE_CHANNEL_DOUBLE_CLOSE_FIXTURE: &[u8] =
    include_bytes!("fixtures/message_channel_double_close.html");
const CONTROLLED_TRANSFER_STREAMS_FIXTURE: &[u8] =
    include_bytes!("fixtures/controlled_transfer_streams.html");
const CONTROLLED_TRANSFER_VALIDATION_PRECEDENCE_FIXTURE: &[u8] =
    include_bytes!("fixtures/controlled_transfer_validation_precedence.html");
const CONTROLLED_LOCKED_TRANSFORM_V1_PRECEDENCE_FIXTURE: &[u8] =
    include_bytes!("fixtures/controlled_locked_transform_v1_precedence.html");
const MESSAGE_CHANNEL_RETAINED_LIMIT_FIXTURE: &[u8] =
    include_bytes!("fixtures/message_channel_retained_limit.html");
const MESSAGE_CHANNEL_PAYLOAD_BOUNDARY_FIXTURE: &[u8] =
    include_bytes!("fixtures/message_channel_payload_boundary.html");
const MESSAGE_CHANNEL_RETAINED_REUSE_FIXTURE: &[u8] =
    include_bytes!("fixtures/message_channel_retained_reuse.html");
const MESSAGE_CHANNEL_SIDECAR_FIXTURE: &[u8] =
    include_bytes!("fixtures/message_channel_sidecar.html");
const MESSAGE_CHANNEL_NAVIGATION_FIXTURE: &[u8] =
    include_bytes!("fixtures/message_channel_navigation.html");
const INPUT_METHOD_AUTOFOCUS_FIXTURE: &[u8] =
    include_bytes!("fixtures/input_method_autofocus.html");
const INPUT_METHOD_FOCUS_TIMESTAMP_FIXTURE: &[u8] =
    include_bytes!("fixtures/input_method_focus_timestamp.html");
const INPUT_METHOD_FOCUS_TIMESTAMP_ADVANCED_FIXTURE: &[u8] =
    include_bytes!("fixtures/input_method_focus_timestamp_advanced.html");
const CONTROLLED_V2_FORM_EVENT_TIMESTAMP_FIXTURE: &[u8] =
    include_bytes!("fixtures/controlled_v2_form_event_timestamp.html");
const CONTROLLED_V2_CSS_ANIMATION_EVENT_TIMESTAMP_FIXTURE: &[u8] =
    include_bytes!("fixtures/controlled_v2_css_animation_event_timestamp.html");
const CONTROLLED_V2_EVENT_TIMESTAMP_BOOTSTRAP_FIXTURE: &[u8] =
    b"<!doctype html><meta charset=utf-8><title>bootstrap-safe event clock</title>";
const INPUT_METHOD_NUMBER_AUTOFOCUS_FIXTURE: &[u8] =
    include_bytes!("fixtures/input_method_number_autofocus.html");
const INPUT_METHOD_TEXTAREA_FOCUS_FIXTURE: &[u8] =
    include_bytes!("fixtures/input_method_textarea_focus.html");
const CONTROLLED_V2_IMAGE_DATA_SVG_FIXTURE: &[u8] =
    include_bytes!("fixtures/controlled_v2_image_data_svg.html");
const CONTROLLED_V2_IMAGE_DATA_SVG_ADVANCED_FIXTURE: &[u8] =
    include_bytes!("fixtures/controlled_v2_image_data_svg_advanced.html");
const CONTROLLED_V2_IMAGE_DATA_SVG_ERROR_FIXTURE: &[u8] =
    include_bytes!("fixtures/controlled_v2_image_data_svg_error.html");
const CONTROLLED_V2_IMAGE_DATA_SVG_CACHE_HIT_FIXTURE: &[u8] =
    include_bytes!("fixtures/controlled_v2_image_data_svg_cache_hit.html");
const CONTROLLED_V2_IMAGE_IDENTITY_REUSE_FIXTURE: &[u8] =
    include_bytes!("fixtures/controlled_v2_image_identity_reuse.html");
const CONTROLLED_V2_IMAGE_HTTP_FIXTURE: &[u8] =
    include_bytes!("fixtures/controlled_v2_image_http.html");
const CONTROLLED_V2_HTTP_IMAGE: &[u8] = include_bytes!("fixtures/controlled_v2_http_image.svg");
const CONTROLLED_V2_INLINE_SVG_FIXTURE: &[u8] =
    include_bytes!("fixtures/controlled_v2_inline_svg.html");
const CONTROLLED_V2_INLINE_SVG_ADVANCED_FIXTURE: &[u8] =
    include_bytes!("fixtures/controlled_v2_inline_svg_advanced.html");
const CONTROLLED_COOKIE_FIXTURE: &[u8] = br#"<!doctype html>
<meta charset="utf-8">
<script>
document.cookie = "page-session=valid; Path=/; SameSite=Lax";
const deterministicReadWorked = document.cookie === "page-session=valid";

let maxAgeRejected = false;
try {
  document.cookie = "persistent=blocked; Max-Age=60; Path=/";
} catch (error) {
  maxAgeRejected = error.name === "NotSupportedError" &&
    error.message.includes("unsupported_persistent_cookie");
}

let partitionedRejected = false;
try {
  document.cookie = "partitioned=blocked; Partitioned; Secure; SameSite=None; Path=/";
} catch (error) {
  partitionedRejected = error.name === "NotSupportedError" &&
    error.message.includes("unsupported_partitioned_cookie");
}

if (deterministicReadWorked && maxAgeRejected && partitionedRejected) {
  document.cookie = "page-session=updated; Path=/; SameSite=Lax";
} else {
  document.cookie = "controlled-cookie-regression=failed; Path=/";
}

const typedUnsupported = (error) => error.name === "NotSupportedError" &&
  error.message.includes("controlled_cookie_store_read_delete_unsupported");
const typedPersistent = (error) => error.name === "NotSupportedError" &&
  error.message.includes("unsupported_persistent_cookie");
const typedInvalid = (error) => error.name === "TypeError" &&
  error.message.includes("invalid_controlled_cookie");
Promise.all([
  cookieStore.set("store-session", "valid").then(() => true, () => false),
  cookieStore.set({
    name: "persistent-store",
    value: "blocked",
    expires: Date.now() + 60000,
  }).then(() => false, typedPersistent),
  cookieStore.set("bad name", "blocked").then(() => false, typedInvalid),
  cookieStore.get("page-session").then(() => false, typedUnsupported),
  cookieStore.getAll("page-session").then(() => false, typedUnsupported),
  cookieStore.delete("page-session").then(() => false, typedUnsupported),
]).then((results) => {
  const exactContract = deterministicReadWorked && maxAgeRejected &&
    partitionedRejected && results.every(Boolean);
  document.cookie = `controlled-cookie-contract=${exactContract ? "exact" : "wrong"}; Path=/`;
});
</script>"#;

#[test]
fn embedded_baseline_survives_a_bad_close_and_reports_the_final_url() {
    let (url, fixture_thread) = start_redirect_fixture();
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-1",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    let initialized = receive(&responses);
    assert_eq!(initialized["id"], "init-1");
    assert_eq!(initialized["result"]["capabilities"]["settlement"], true);
    assert_eq!(
        initialized["result"]["capabilities"]["profiles"],
        json!([
            "controlled-webapp-v1",
            "controlled-web-session-v1",
            "controlled-web-session-v2"
        ])
    );
    assert_eq!(
        initialized["result"]["capabilities"]["clockModes"],
        json!(["real", "controlled"])
    );
    let methods = initialized["result"]["capabilities"]["methods"]
        .as_array()
        .expect("capabilities.methods must be an array");
    for required in [
        "protocol.initialize",
        "session.open",
        "runtime.pending",
        "runtime.settle",
        "runtime.advance_to_next",
        "protocol.cancel",
        "session.close",
    ] {
        assert!(
            methods
                .iter()
                .any(|method| method.as_str() == Some(required)),
            "initialize did not advertise {required}: {methods:?}"
        );
    }

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-1",
            "method": "session.open",
            "params": {"url": url},
        }),
    );
    let opened = receive(&responses);
    assert_eq!(opened["id"], "open-1");
    assert_eq!(opened["sessionId"], "s-1");
    assert_eq!(opened["result"]["requestedUrl"], url);
    assert_eq!(opened["result"]["url"], format!("{url}final"));

    assert_title(&mut input, &responses, "eval-1");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "bad-close",
            "sessionId": "stale",
            "method": "session.close",
            "params": {},
        }),
    );
    let rejected_close = receive(&responses);
    assert_eq!(rejected_close["error"]["code"], "invalid_request");
    assert_eq!(rejected_close["error"]["stateEffect"], "none");

    assert_title(&mut input, &responses, "eval-2");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "close-1",
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    let closed = receive(&responses);
    assert_eq!(closed["id"], "close-1");
    assert_eq!(closed["sessionId"], "s-1");
    assert_eq!(closed["result"]["state"], "closed");
    drop(input);

    let (status_sender, status_receiver) = sync_channel(1);
    thread::spawn(move || {
        status_sender.send(child.wait()).ok();
    });
    let status = status_receiver
        .recv_timeout(RESPONSE_TIMEOUT)
        .expect("stasis did not exit after session.close")
        .expect("failed to wait for stasis");
    assert!(status.success());
    fixture_thread.join().expect("fixture server panicked");
}

#[test]
fn controlled_page_cookie_mutations_remain_atomic_and_exportable() {
    let url = "https://cookie.example.test/".to_owned();
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-cookie",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    assert_eq!(receive(&responses)["id"], "init-cookie");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-cookie",
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v1",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {"utf8": std::str::from_utf8(CONTROLLED_COOKIE_FIXTURE).unwrap()},
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(opened["id"], "open-cookie", "{opened:#}");
    assert_eq!(opened["sessionId"], "s-1", "{opened:#}");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "export-cookie",
            "sessionId": "s-1",
            "method": "session.state.export",
            "params": {},
        }),
    );
    let exported = receive(&responses);
    assert_eq!(exported["id"], "export-cookie", "{exported:#}");
    let cookies = exported["result"]["state"]["cookies"]
        .as_array()
        .expect("session.state.export cookies must be an array");
    assert_eq!(cookies.len(), 3, "{exported:#}");
    let names: Vec<_> = cookies
        .iter()
        .map(|cookie| cookie["name"].as_str().unwrap())
        .collect();
    assert_eq!(
        names,
        [
            "controlled-cookie-contract",
            "page-session",
            "store-session"
        ],
        "{exported:#}",
    );
    assert_eq!(cookies[0]["value"], "exact", "{exported:#}");
    assert_eq!(cookies[1]["value"], "updated", "{exported:#}");
    assert_eq!(
        cookies[1]["domain"],
        url::Url::parse(&url).unwrap().host_str().unwrap(),
        "{exported:#}",
    );
    assert_eq!(cookies[1]["path"], "/", "{exported:#}");
    assert_eq!(cookies[1]["hostOnly"], true, "{exported:#}");
    assert_eq!(cookies[1]["secure"], false, "{exported:#}");
    assert_eq!(cookies[1]["httpOnly"], false, "{exported:#}");
    assert_eq!(cookies[1]["sameSite"], "lax", "{exported:#}");
    assert_eq!(cookies[1]["expiresUnixTimeNs"], Value::Null, "{exported:#}");
    assert_eq!(cookies[1]["partitioned"], false, "{exported:#}");
    assert_eq!(cookies[2]["value"], "valid", "{exported:#}");
    assert_eq!(cookies[2]["sameSite"], "strict", "{exported:#}");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "close-cookie",
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    assert_eq!(receive(&responses)["id"], "close-cookie");
    drop(input);

    let (status_sender, status_receiver) = sync_channel(1);
    thread::spawn(move || {
        status_sender.send(child.wait()).ok();
    });
    let status = status_receiver
        .recv_timeout(RESPONSE_TIMEOUT)
        .expect("stasis did not exit after session.close")
        .expect("failed to wait for stasis");
    assert!(status.success());
}

#[test]
fn controlled_local_message_channel_is_additive_to_session_v2() {
    exercise_message_channel_profile("controlled-web-session-v1", false);
    exercise_message_channel_profile("controlled-web-session-v2", true);
}

#[test]
fn controlled_local_message_port_source_requires_exact_target_and_preclone_incumbent() {
    assert!(
        GLOBAL_SCOPE_MESSAGE_PORT_SOURCE
            .contains(".is_some_and(ScriptThread::current_controlled_top_level_target_matches)",),
        "controlled-local admission must reuse the exact public target predicate",
    );
    assert!(
        GLOBAL_SCOPE_MESSAGE_PORT_SOURCE.contains("controlled_local_caller_matches_owner"),
        "controlled-local admission must bind the caller to the exact owning global",
    );

    let constructor_incumbent = MESSAGE_CHANNEL_SOURCE
        .find("let incumbent = GlobalScope::incumbent();")
        .expect("MessageChannel must resolve its incumbent before publication");
    let constructor_admission = MESSAGE_CHANNEL_SOURCE
        .find("admit_message_channel_constructor(incumbent.as_deref())")
        .expect("MessageChannel must submit the exact incumbent to admission");
    let constructor_publication = MESSAGE_CHANNEL_SOURCE
        .find("Ok(MessageChannel::new")
        .expect("MessageChannel publication seam must remain explicit");
    assert!(constructor_incumbent < constructor_admission);
    assert!(constructor_admission < constructor_publication);

    let post_incumbent = MESSAGE_PORT_SOURCE
        .find("let incumbent = GlobalScope::incumbent();")
        .expect("MessagePort must resolve its incumbent before clone work");
    let post_admission = MESSAGE_PORT_SOURCE
        .find("require_message_port_post(")
        .expect("MessagePort must preflight the exact incumbent");
    let post_serialization = MESSAGE_PORT_SOURCE
        .find("structuredclone::write")
        .expect("MessagePort serialization seam must remain explicit");
    assert!(post_incumbent < post_admission);
    assert!(post_admission < post_serialization);
    assert!(
        MESSAGE_PORT_SOURCE.contains("incumbent.as_deref()"),
        "MessagePort post admission must receive the exact resolved incumbent",
    );
}

#[test]
fn controlled_v2_images_require_the_exact_nonauxiliary_target() {
    for (surface, source) in [
        ("direct HTML image", HTML_IMAGE_ELEMENT_SOURCE),
        ("inline SVG", SVG_SVG_ELEMENT_SOURCE),
        ("image listener", WINDOW_SOURCE),
    ] {
        assert!(
            source.contains("ScriptThread::current_controlled_top_level_target_matches"),
            "{surface} admission must reuse the exact controlled target predicate",
        );
    }
    assert!(
        SCRIPT_THREAD_SOURCE
            .contains("!Self::current_controlled_top_level_target_matches(&window)"),
        "controlled image delivery must revalidate the exact target",
    );
    assert!(
        SCRIPT_THREAD_SOURCE.contains("window_proxy.is_auxiliary()"),
        "the shared exact-target predicate must reject auxiliary WebViews",
    );
}

#[test]
fn controlled_session_v2_autofocus_input_method_does_not_promote_v1() {
    exercise_input_method_autofocus_profile(
        "controlled-web-session-v2",
        "text-v2",
        INPUT_METHOD_AUTOFOCUS_FIXTURE,
        true,
        "embedder_control",
        "focused|1|rwa-value|2:5",
    );
    exercise_input_method_autofocus_profile(
        "controlled-web-session-v1",
        "text-v1",
        INPUT_METHOD_AUTOFOCUS_FIXTURE,
        false,
        "embedder_control",
        "focused|1|rwa-value|2:5",
    );
}

#[test]
fn controlled_session_v2_focus_event_timestamp_uses_document_clock_without_v1_promotion() {
    exercise_input_method_autofocus_profile(
        "controlled-web-session-v2",
        "focus-timestamp-v2",
        INPUT_METHOD_FOCUS_TIMESTAMP_FIXTURE,
        true,
        "host_timestamp",
        "blurred|4|focus:trusted:0>focusin:trusted:0>blur:trusted:0>focusout:trusted:0|rwa-value|2:5",
    );
    exercise_input_method_autofocus_profile(
        "controlled-web-session-v1",
        "focus-timestamp-v1",
        INPUT_METHOD_FOCUS_TIMESTAMP_FIXTURE,
        false,
        "host_timestamp",
        "blurred|4|focus:trusted:0>focusin:trusted:0>blur:trusted:0>focusout:trusted:0|rwa-value|2:5",
    );
}

#[test]
fn controlled_session_v2_direct_data_svg_is_owned_without_v1_promotion() {
    exercise_controlled_data_svg_profile(
        "controlled-web-session-v2",
        "direct-v2",
        CONTROLLED_V2_IMAGE_DATA_SVG_FIXTURE,
        Some("load:0>loadend:0|now:0"),
    );
    exercise_controlled_data_svg_profile(
        "controlled-web-session-v1",
        "direct-v1",
        CONTROLLED_V2_IMAGE_DATA_SVG_FIXTURE,
        None,
    );
}

#[test]
fn controlled_session_v2_inline_svg_rendering_is_owned_without_v1_promotion() {
    exercise_controlled_data_svg_profile(
        "controlled-web-session-v2",
        "inline-svg-v2",
        CONTROLLED_V2_INLINE_SVG_FIXTURE,
        Some("inline-svg:4x3|events:0|now:0"),
    );
    exercise_controlled_data_svg_profile(
        "controlled-web-session-v1",
        "inline-svg-v1",
        CONTROLLED_V2_INLINE_SVG_FIXTURE,
        None,
    );
}

#[test]
fn controlled_session_v2_data_svg_cache_hit_keeps_exact_generation_time() {
    exercise_controlled_data_svg_profile(
        "controlled-web-session-v2",
        "cache-hit-v2",
        CONTROLLED_V2_IMAGE_DATA_SVG_CACHE_HIT_FIXTURE,
        Some("first:0|second:0|now:0"),
    );
}

#[test]
fn controlled_session_v2_data_svg_decode_error_uses_owned_completion_time() {
    exercise_controlled_data_svg_profile(
        "controlled-web-session-v2",
        "decode-error-v2",
        CONTROLLED_V2_IMAGE_DATA_SVG_ERROR_FIXTURE,
        Some("error:0>loadend:0|now:0"),
    );
}

#[test]
fn controlled_session_v2_reuses_image_identity_capacity_across_520_requests() {
    exercise_controlled_data_svg_profile(
        "controlled-web-session-v2",
        "identity-reuse-v2",
        CONTROLLED_V2_IMAGE_IDENTITY_REUSE_FIXTURE,
        Some("completed:520|exact-time:true"),
    );
}

#[test]
fn controlled_session_v2_http_image_remains_outside_owned_slice() {
    let document_url = "https://controlled-image-http.example.test/";
    let image_url = "https://controlled-image-http.example.test/controlled-v2-http-image.svg";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-controlled-image-http",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    assert_eq!(receive(&responses)["id"], "init-controlled-image-http");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-controlled-image-http",
            "method": "session.open",
            "params": {
                "url": document_url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v2",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [
                        {
                            "match": {"method": "GET", "url": {"exact": document_url}},
                            "fulfill": {
                                "status": 200,
                                "headers": [["content-type", "text/html; charset=utf-8"]],
                                "body": {
                                    "utf8": std::str::from_utf8(CONTROLLED_V2_IMAGE_HTTP_FIXTURE)
                                        .unwrap()
                                },
                            },
                        },
                        {
                            "match": {"method": "GET", "url": {"exact": image_url}},
                            "fulfill": {
                                "status": 200,
                                "headers": [["content-type", "image/svg+xml"]],
                                "body": {
                                    "utf8": std::str::from_utf8(CONTROLLED_V2_HTTP_IMAGE).unwrap()
                                },
                            },
                        },
                    ],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(opened["id"], "open-controlled-image-http", "{opened:#}");
    assert_eq!(opened["error"]["code"], "unsupported_work", "{opened:#}");
    assert_eq!(opened["error"]["fatal"], true, "{opened:#}");
    let failure_code = opened["error"]["details"]["failure"]["code"]
        .as_str()
        .expect("HTTP image rejection must carry a typed failure code");
    assert!(
        matches!(
            failure_code,
            "unsupported_rendering" | "unsupported_clock_surface"
        ),
        "HTTP image must fail through an unchanged typed baseline authority: {opened:#}",
    );

    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for rejected HTTP image process");
    assert_eq!(
        status.code(),
        Some(70),
        "fatal HTTP image rejection must use the documented exit code: {status}",
    );
}

#[test]
fn controlled_session_v2_data_svg_completion_tracks_advanced_document_time() {
    let url = "https://controlled-image-advanced.example.test/";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-controlled-image-advanced",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    assert_eq!(receive(&responses)["id"], "init-controlled-image-advanced");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-controlled-image-advanced",
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v2",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(
                                    CONTROLLED_V2_IMAGE_DATA_SVG_ADVANCED_FIXTURE,
                                )
                                .unwrap(),
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(opened["id"], "open-controlled-image-advanced", "{opened:#}");
    let open_token = opened["result"]["stateToken"]
        .as_str()
        .expect("advanced image open must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "schedule-controlled-image-advanced",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#start", "expectedStateToken": open_token},
        }),
    );
    let scheduled = receive(&responses);
    assert_eq!(
        scheduled["id"], "schedule-controlled-image-advanced",
        "{scheduled:#}"
    );
    let scheduled_token = scheduled["result"]["stateToken"]
        .as_str()
        .expect("scheduled image timer must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "qualify-controlled-image-advance",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": scheduled_token, "maxVirtualTimeNs": "0"},
        }),
    );
    let qualified = receive(&responses);
    assert_eq!(
        qualified["id"], "qualify-controlled-image-advance",
        "{qualified:#}"
    );
    assert_eq!(
        qualified["result"]["outcome"], "virtual_time_limit_exceeded",
        "{qualified:#}"
    );
    assert_eq!(qualified["result"]["virtualTimeNs"], "0", "{qualified:#}");
    assert_eq!(
        qualified["result"]["limit"]["requestedVirtualTimeNs"], "5000000",
        "{qualified:#}"
    );
    let qualified_token = qualified["result"]["stateToken"]
        .as_str()
        .expect("qualified image timer must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "advance-controlled-image",
            "sessionId": "s-1",
            "method": "runtime.advance_to_next",
            "params": {"expectedStateToken": qualified_token},
        }),
    );
    let advanced = receive(&responses);
    assert_eq!(advanced["id"], "advance-controlled-image", "{advanced:#}");
    assert_eq!(advanced["result"]["outcome"], "advanced", "{advanced:#}");
    assert_eq!(
        advanced["result"]["virtualTimeNs"], "5000000",
        "{advanced:#}"
    );
    let advanced_token = advanced["result"]["stateToken"]
        .as_str()
        .expect("advanced image timer must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "settle-controlled-image-advanced",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": advanced_token},
        }),
    );
    let settled = receive(&responses);
    assert_eq!(
        settled["id"], "settle-controlled-image-advanced",
        "{settled:#}"
    );
    assert_eq!(settled["result"]["outcome"], "quiescent", "{settled:#}");
    assert_eq!(
        settled["result"]["unsupportedWork"],
        json!([]),
        "{settled:#}"
    );
    assert_eq!(
        settled["result"]["snapshot"]["producers"]["pending"], "0",
        "{settled:#}"
    );
    let settled_token = settled["result"]["stateToken"]
        .as_str()
        .expect("settled advanced image must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-controlled-image-advanced",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {"selector": "#result", "expectedStateToken": settled_token},
        }),
    );
    let text = receive(&responses);
    assert_eq!(text["id"], "text-controlled-image-advanced", "{text:#}");
    assert_eq!(
        text["result"]["value"], "load:5>loadend:5|now:5",
        "both terminal image events must retain the 5 ms document-clock sample: {text:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "close-controlled-image-advanced",
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    assert_eq!(receive(&responses)["id"], "close-controlled-image-advanced");
    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for advanced image process");
    assert!(
        status.success(),
        "advanced image process exited with {status}"
    );
}

#[test]
fn controlled_session_v2_inline_svg_raster_completes_after_advanced_document_time() {
    exercise_controlled_inline_svg_advanced();
}

#[test]
fn controlled_session_v2_focus_event_timestamp_tracks_advanced_document_time() {
    let url = "https://input-method-focus-timestamp-advanced.example.test/";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-focus-timestamp-advanced",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    assert_eq!(receive(&responses)["id"], "init-focus-timestamp-advanced");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-focus-timestamp-advanced",
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v2",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(
                                    INPUT_METHOD_FOCUS_TIMESTAMP_ADVANCED_FIXTURE,
                                )
                                .unwrap(),
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(opened["id"], "open-focus-timestamp-advanced", "{opened:#}");
    let open_token = opened["result"]["stateToken"]
        .as_str()
        .expect("advanced focus fixture must carry open stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "schedule-focus-timestamp-advanced",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#start", "expectedStateToken": open_token},
        }),
    );
    let scheduled = receive(&responses);
    assert_eq!(
        scheduled["id"], "schedule-focus-timestamp-advanced",
        "{scheduled:#}",
    );
    let scheduled_token = scheduled["result"]["stateToken"]
        .as_str()
        .expect("scheduled focus action must carry stateToken")
        .to_owned();
    assert_ne!(
        scheduled_token, open_token,
        "scheduling the controlled timer must rotate document authority: {scheduled:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "qualify-focus-timestamp-advance",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {
                "expectedStateToken": scheduled_token,
                "maxVirtualTimeNs": "0",
            },
        }),
    );
    let qualified = receive(&responses);
    assert_eq!(
        qualified["id"], "qualify-focus-timestamp-advance",
        "{qualified:#}",
    );
    assert_eq!(
        qualified["result"]["outcome"], "virtual_time_limit_exceeded",
        "the zero-width settle must drain ready work without consuming the 5 ms timer: {qualified:#}",
    );
    assert_eq!(qualified["result"]["virtualTimeNs"], "0", "{qualified:#}");
    assert_eq!(
        qualified["result"]["limit"],
        json!({
            "kind": "virtual_time",
            "limit": "0",
            "startVirtualTimeNs": "0",
            "requestedVirtualTimeNs": "5000000",
        }),
        "{qualified:#}",
    );
    let qualified_token = qualified["result"]["stateToken"]
        .as_str()
        .expect("qualified focus timer must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "pending-focus-timestamp-advance",
            "sessionId": "s-1",
            "method": "runtime.pending",
            "params": {},
        }),
    );
    let pending = receive(&responses);
    assert_eq!(
        pending["id"], "pending-focus-timestamp-advance",
        "{pending:#}"
    );
    assert_eq!(pending["result"]["virtualTimeNs"], "0", "{pending:#}");
    assert_eq!(pending["result"]["timers"]["ready"], "0", "{pending:#}");
    assert_eq!(
        pending["result"]["timers"]["futureFinite"], "1",
        "{pending:#}",
    );
    assert_eq!(
        pending["result"]["timers"]["nextDeadlineNs"], "5000000",
        "{pending:#}",
    );
    assert_eq!(
        pending["result"]["producers"]["pending"], "0",
        "{pending:#}"
    );
    assert_eq!(
        pending["result"]["producers"]["stability"], "stable_empty",
        "{pending:#}",
    );
    assert_eq!(
        pending["result"]["stateToken"], qualified_token,
        "passive pending observation must preserve the qualified timer authority: {pending:#}",
    );
    let pending_token = pending["result"]["stateToken"]
        .as_str()
        .expect("pending focus timer must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "advance-focus-timestamp",
            "sessionId": "s-1",
            "method": "runtime.advance_to_next",
            "params": {"expectedStateToken": pending_token},
        }),
    );
    let advanced = receive(&responses);
    assert_eq!(advanced["id"], "advance-focus-timestamp", "{advanced:#}");
    assert_eq!(advanced["result"]["outcome"], "advanced", "{advanced:#}");
    assert_eq!(advanced["result"]["fromVirtualTimeNs"], "0", "{advanced:#}");
    assert_eq!(
        advanced["result"]["virtualTimeNs"], "5000000",
        "{advanced:#}"
    );
    assert_eq!(
        advanced["result"]["snapshot"]["timers"]["ready"], "1",
        "{advanced:#}"
    );
    assert_eq!(
        advanced["result"]["snapshot"]["timers"]["futureFinite"], "0",
        "{advanced:#}",
    );
    let advanced_token = advanced["result"]["stateToken"]
        .as_str()
        .expect("advanced focus result must carry stateToken")
        .to_owned();
    assert_ne!(
        advanced_token, pending_token,
        "advancing to the 5 ms timer must rotate document authority: {advanced:#}",
    );
    assert_eq!(
        advanced["result"]["snapshot"]["stateToken"], advanced_token,
        "advance summary and snapshot must carry one authority token: {advanced:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "dispatch-focus-timestamp",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {
                "expectedStateToken": advanced_token,
                "maxVirtualTimeNs": "0",
            },
        }),
    );
    let dispatched = receive(&responses);
    assert_eq!(
        dispatched["id"], "dispatch-focus-timestamp",
        "{dispatched:#}"
    );
    assert_eq!(
        dispatched["result"]["outcome"], "virtual_time_limit_exceeded",
        "the zero-width dispatch settle must execute 5 ms work but not consume the 20 ms rendering head: {dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["virtualTimeNs"], "5000000",
        "{dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["limit"],
        json!({
            "kind": "virtual_time",
            "limit": "0",
            "startVirtualTimeNs": "5000000",
            "requestedVirtualTimeNs": "20000000",
        }),
        "{dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["snapshot"]["timers"]["ready"], "0",
        "{dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["snapshot"]["timers"]["futureFinite"], "0",
        "{dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["snapshot"]["producers"]["stability"], "stable_empty",
        "{dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["snapshot"]["producers"]["pending"], "0",
        "{dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["snapshot"]["rendering"]["opportunityReady"], false,
        "{dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["snapshot"]["rendering"]["nextOpportunityNs"], "20000000",
        "{dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["snapshot"]["rendering"]["updateRequired"], true,
        "{dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["snapshot"]["sources"],
        json!([]),
        "{dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["snapshot"]["runtimeFailures"],
        json!([]),
        "{dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["persistentWork"],
        json!([]),
        "{dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["externalIo"],
        json!([]),
        "{dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["unsupportedWork"],
        json!([]),
        "{dispatched:#}",
    );
    let dispatched_token = dispatched["result"]["stateToken"]
        .as_str()
        .expect("dispatched focus timer must carry stateToken")
        .to_owned();
    assert_ne!(
        dispatched_token, advanced_token,
        "executing the 5 ms callback must rotate document authority: {dispatched:#}",
    );
    assert_eq!(
        dispatched["result"]["snapshot"]["stateToken"], dispatched_token,
        "dispatch summary and snapshot must carry one authority token: {dispatched:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "pending-focus-timestamp-five-ms",
            "sessionId": "s-1",
            "method": "runtime.pending",
            "params": {},
        }),
    );
    let pending_at_five = receive(&responses);
    assert_eq!(
        pending_at_five["id"], "pending-focus-timestamp-five-ms",
        "{pending_at_five:#}",
    );
    assert_eq!(
        pending_at_five["result"]["virtualTimeNs"], "5000000",
        "{pending_at_five:#}",
    );
    assert_eq!(
        pending_at_five["result"]["timers"]["ready"], "0",
        "{pending_at_five:#}",
    );
    assert_eq!(
        pending_at_five["result"]["timers"]["futureFinite"], "0",
        "{pending_at_five:#}",
    );
    assert_eq!(
        pending_at_five["result"]["timers"]["persistent"], "0",
        "{pending_at_five:#}",
    );
    assert_eq!(
        pending_at_five["result"]["timers"]["unsupported"], "0",
        "{pending_at_five:#}",
    );
    assert!(
        pending_at_five["result"]["timers"]["nextDeadlineNs"].is_null(),
        "no timer head may remain after the 5 ms callback: {pending_at_five:#}",
    );
    assert_eq!(
        pending_at_five["result"]["producers"]["pending"], "0",
        "{pending_at_five:#}",
    );
    assert_eq!(
        pending_at_five["result"]["producers"]["stability"], "stable_empty",
        "{pending_at_five:#}",
    );
    assert_eq!(
        pending_at_five["result"]["producers"]["terminal"], false,
        "{pending_at_five:#}",
    );
    assert_eq!(
        pending_at_five["result"]["rendering"]["opportunityReady"], false,
        "{pending_at_five:#}",
    );
    assert_eq!(
        pending_at_five["result"]["rendering"]["nextOpportunityNs"], "20000000",
        "{pending_at_five:#}",
    );
    assert_eq!(
        pending_at_five["result"]["rendering"]["updateRequired"], true,
        "{pending_at_five:#}",
    );
    assert_eq!(
        pending_at_five["result"]["sources"],
        json!([]),
        "{pending_at_five:#}",
    );
    assert_eq!(
        pending_at_five["result"]["runtimeFailures"],
        json!([]),
        "{pending_at_five:#}",
    );
    assert_eq!(
        pending_at_five["result"]["stateToken"], dispatched_token,
        "passive pending observation at 5 ms must preserve authority: {pending_at_five:#}",
    );
    let pending_at_five_token = pending_at_five["result"]["stateToken"]
        .as_str()
        .expect("5 ms pending snapshot must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-focus-timestamp-advanced",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {"selector": "#result", "expectedStateToken": pending_at_five_token},
        }),
    );
    let text = receive(&responses);
    assert_eq!(text["id"], "text-focus-timestamp-advanced", "{text:#}");
    assert_eq!(
        text["result"]["value"],
        "blurred|4|focus:trusted:5>focusin:trusted:5>blur:trusted:5>focusout:trusted:5|5|rwa-value|2:5",
        "all engine focus-transition timeStamps must equal the advanced document performance clock: {text:#}",
    );
    let text_token = text["result"]["stateToken"]
        .as_str()
        .expect("advanced focus inspection must carry stateToken")
        .to_owned();
    assert_eq!(
        text_token, pending_at_five_token,
        "passive DOM inspection must preserve 5 ms document authority: {text:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "advance-rendering-focus-timestamp",
            "sessionId": "s-1",
            "method": "runtime.advance_to_next",
            "params": {"expectedStateToken": text_token},
        }),
    );
    let rendering_advanced = receive(&responses);
    assert_eq!(
        rendering_advanced["id"], "advance-rendering-focus-timestamp",
        "{rendering_advanced:#}",
    );
    assert_eq!(
        rendering_advanced["result"]["outcome"], "advanced",
        "{rendering_advanced:#}",
    );
    assert_eq!(
        rendering_advanced["result"]["fromVirtualTimeNs"], "5000000",
        "{rendering_advanced:#}",
    );
    assert_eq!(
        rendering_advanced["result"]["virtualTimeNs"], "20000000",
        "{rendering_advanced:#}",
    );
    assert_eq!(
        rendering_advanced["result"]["snapshot"]["rendering"]["opportunityReady"], true,
        "{rendering_advanced:#}",
    );
    let rendering_advanced_token = rendering_advanced["result"]["stateToken"]
        .as_str()
        .expect("rendering advance must carry stateToken")
        .to_owned();
    assert_ne!(
        rendering_advanced_token, text_token,
        "advancing to the 20 ms rendering opportunity must rotate authority: {rendering_advanced:#}",
    );
    assert_eq!(
        rendering_advanced["result"]["snapshot"]["stateToken"], rendering_advanced_token,
        "rendering advance summary and snapshot must carry one authority token: {rendering_advanced:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "settle-focus-timestamp-advanced",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {
                "expectedStateToken": rendering_advanced_token,
                "maxVirtualTimeNs": "0",
            },
        }),
    );
    let settled = receive(&responses);
    assert_eq!(
        settled["id"], "settle-focus-timestamp-advanced",
        "{settled:#}",
    );
    assert_eq!(settled["result"]["outcome"], "quiescent", "{settled:#}");
    assert_eq!(
        settled["result"]["virtualTimeNs"], "20000000",
        "{settled:#}"
    );
    assert_eq!(
        settled["result"]["snapshot"]["timers"]["ready"], "0",
        "{settled:#}"
    );
    assert_eq!(
        settled["result"]["snapshot"]["timers"]["futureFinite"], "0",
        "{settled:#}",
    );
    assert_eq!(
        settled["result"]["snapshot"]["producers"]["pending"], "0",
        "{settled:#}",
    );
    assert_eq!(
        settled["result"]["snapshot"]["producers"]["stability"], "stable_empty",
        "{settled:#}",
    );
    assert_eq!(
        settled["result"]["snapshot"]["rendering"]["opportunityReady"], false,
        "{settled:#}",
    );
    assert!(
        settled["result"]["snapshot"]["rendering"]["nextOpportunityNs"].is_null(),
        "no rendering head may remain after the exact 20 ms settle: {settled:#}",
    );
    assert_eq!(
        settled["result"]["snapshot"]["rendering"]["updateRequired"], false,
        "{settled:#}",
    );
    assert_eq!(
        settled["result"]["snapshot"]["sources"],
        json!([]),
        "{settled:#}"
    );
    assert_eq!(
        settled["result"]["snapshot"]["runtimeFailures"],
        json!([]),
        "{settled:#}",
    );
    assert_eq!(
        settled["result"]["persistentWork"],
        json!([]),
        "{settled:#}"
    );
    assert_eq!(settled["result"]["externalIo"], json!([]), "{settled:#}");
    assert_eq!(
        settled["result"]["unsupportedWork"],
        json!([]),
        "{settled:#}"
    );
    let settled_token = settled["result"]["stateToken"]
        .as_str()
        .expect("advanced focus settlement must carry stateToken")
        .to_owned();
    assert_ne!(
        settled_token, rendering_advanced_token,
        "executing the 20 ms rendering opportunity must rotate authority: {settled:#}",
    );
    assert_eq!(
        settled["result"]["snapshot"]["stateToken"], settled_token,
        "final settle summary and snapshot must carry one authority token: {settled:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "read-synthetic-focus-timestamp",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#synthetic", "expectedStateToken": settled_token},
        }),
    );
    let synthetic = receive(&responses);
    assert_eq!(
        synthetic["id"], "read-synthetic-focus-timestamp",
        "{synthetic:#}"
    );
    let synthetic_token = synthetic["result"]["stateToken"]
        .as_str()
        .expect("synthetic focus action must carry stateToken")
        .to_owned();
    assert_ne!(
        synthetic_token, settled_token,
        "dispatching the synthetic FocusEvent must rotate authority: {synthetic:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-synthetic-focus-timestamp",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {"selector": "#result", "expectedStateToken": synthetic_token},
        }),
    );
    let synthetic_text = receive(&responses);
    assert_eq!(
        synthetic_text["id"], "text-synthetic-focus-timestamp",
        "{synthetic_text:#}",
    );
    assert_eq!(
        synthetic_text["result"]["value"], "synthetic:0",
        "script-created FocusEvent must retain its suppressed host-time value: {synthetic_text:#}",
    );
    let synthetic_text_token = synthetic_text["result"]["stateToken"]
        .as_str()
        .expect("synthetic focus inspection must carry stateToken")
        .to_owned();
    assert_eq!(
        synthetic_text_token, synthetic_token,
        "passive synthetic DOM inspection must preserve authority: {synthetic_text:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "settle-synthetic-focus-timestamp",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": synthetic_text_token},
        }),
    );
    let rejected = receive(&responses);
    assert_eq!(
        rejected["id"], "settle-synthetic-focus-timestamp",
        "{rejected:#}"
    );
    assert_eq!(
        rejected["result"]["outcome"], "unsupported_work",
        "{rejected:#}"
    );
    assert_eq!(
        rejected["result"]["failure"]["code"], "unsupported_clock_surface",
        "{rejected:#}",
    );
    assert_eq!(
        rejected["result"]["unsupportedWork"],
        json!([{
            "kind": "other",
            "count": "1",
            "reason": "time_surface",
            "timeSurface": "host_timestamp",
        }]),
        "only engine-generated focus transitions receive the v2 document timestamp: {rejected:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "close-focus-timestamp-advanced",
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    assert_eq!(receive(&responses)["id"], "close-focus-timestamp-advanced");
    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for advanced focus process");
    assert!(
        status.success(),
        "advanced focus process exited with {status}"
    );
}

#[test]
fn controlled_session_v2_event_timestamp_scope_is_safe_during_document_bootstrap() {
    let url = "https://event-timestamp-bootstrap.example.test/";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-event-timestamp-bootstrap",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    assert_eq!(receive(&responses)["id"], "init-event-timestamp-bootstrap");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-event-timestamp-bootstrap",
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v2",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(
                                    CONTROLLED_V2_EVENT_TIMESTAMP_BOOTSTRAP_FIXTURE,
                                )
                                .unwrap(),
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(opened["id"], "open-event-timestamp-bootstrap", "{opened:#}");
    assert_eq!(
        opened["result"]["profile"], "controlled-web-session-v2",
        "{opened:#}",
    );

    call_session(
        &mut input,
        &responses,
        "close-event-timestamp-bootstrap",
        "session.close",
        json!({}),
    );
    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for event timestamp bootstrap process");
    assert!(
        status.success(),
        "event timestamp bootstrap process exited with {status}"
    );
}

#[test]
fn controlled_session_v2_form_automation_events_share_the_advanced_document_time() {
    let url = "https://form-event-timestamp.example.test/";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-form-event-timestamp",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    assert_eq!(receive(&responses)["id"], "init-form-event-timestamp");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-form-event-timestamp",
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v2",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(
                                    CONTROLLED_V2_FORM_EVENT_TIMESTAMP_FIXTURE,
                                )
                                .unwrap(),
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(opened["id"], "open-form-event-timestamp", "{opened:#}");
    assert_eq!(
        opened["result"]["profile"], "controlled-web-session-v2",
        "{opened:#}",
    );
    let mut token = state_token(&opened, "form timestamp open");

    let (_, scheduled_token) = call_controlled_action(
        &mut input,
        &responses,
        "schedule-form-event-advance",
        "action.activate",
        json!({"selector": "#start"}),
        &token,
    );
    token = scheduled_token;

    let qualified = call_session(
        &mut input,
        &responses,
        "qualify-form-event-advance",
        "runtime.settle",
        json!({
            "expectedStateToken": token,
            "maxVirtualTimeNs": "0",
        }),
    );
    assert_eq!(qualified["result"]["virtualTimeNs"], "0", "{qualified:#}");
    token = state_token(&qualified, "qualified form timestamp timer");

    let advanced = call_session(
        &mut input,
        &responses,
        "advance-form-event-clock",
        "runtime.advance_to_next",
        json!({"expectedStateToken": token}),
    );
    assert_eq!(advanced["result"]["outcome"], "advanced", "{advanced:#}");
    assert_eq!(
        advanced["result"]["virtualTimeNs"], "5000000",
        "{advanced:#}",
    );
    token = state_token(&advanced, "advanced form timestamp clock");

    let dispatched = call_session(
        &mut input,
        &responses,
        "dispatch-form-event-timer",
        "runtime.settle",
        json!({
            "expectedStateToken": token,
            "maxVirtualTimeNs": "0",
        }),
    );
    assert_eq!(
        dispatched["result"]["virtualTimeNs"], "5000000",
        "{dispatched:#}",
    );
    token = state_token(&dispatched, "dispatched form timestamp timer");

    for (id, method, params) in [
        (
            "fill-form-event",
            "action.fill",
            json!({"selector": "#fill", "value": "replacement"}),
        ),
        (
            "activate-form-event",
            "action.activate",
            json!({"selector": "#activate"}),
        ),
        (
            "reset-form-event",
            "action.activate",
            json!({"selector": "#reset"}),
        ),
        (
            "check-form-event",
            "action.check",
            json!({"selector": "#check"}),
        ),
        (
            "select-form-event",
            "action.select",
            json!({"selector": "#select", "values": ["two"]}),
        ),
        (
            "invalid-form-event",
            "action.submit",
            json!({"selector": "#invalid-form"}),
        ),
        (
            "submit-form-event",
            "action.submit",
            json!({"selector": "#valid-form"}),
        ),
    ] {
        let (_, next_token) =
            call_controlled_action(&mut input, &responses, id, method, params, &token);
        token = next_token;
    }

    let browser_events = call_session(
        &mut input,
        &responses,
        "read-browser-form-event-timestamps",
        "dom.text",
        json!({"selector": "#result", "expectedStateToken": token}),
    );
    assert_eq!(
        browser_events["result"]["value"],
        "5|fill:input:5>activate:click:5>reset:reset:5>check:click:5>check:input:5>check:change:5>select:input:5>select:change:5>invalid:invalid:5>submit:submit:5>submit:formdata:5|not-read|0",
        "representative engine-created events, including a derived reset beyond the basic form-event seams, must share the owning action's sampled document Performance timestamp: {browser_events:#}",
    );
    assert_eq!(
        state_token(&browser_events, "browser form timestamp inspection"),
        token,
        "passive inspection must preserve form timestamp authority: {browser_events:#}",
    );

    let (_, script_token) = call_controlled_action(
        &mut input,
        &responses,
        "read-script-created-form-event-timestamps",
        "action.activate",
        json!({"selector": "#script-created"}),
        &token,
    );
    token = script_token;
    let script_events = call_session(
        &mut input,
        &responses,
        "inspect-script-created-form-event-timestamps",
        "dom.text",
        json!({"selector": "#result", "expectedStateToken": token}),
    );
    assert_eq!(
        script_events["result"]["value"],
        "5|fill:input:5>activate:click:5>reset:reset:5>check:click:5>check:input:5>check:change:5>select:input:5>select:change:5>invalid:invalid:5>submit:submit:5>submit:formdata:5>script-trigger:click:5|0,0,0,0,0|0",
        "script-created Event constructors must retain their rejected host timestamps: {script_events:#}",
    );
    token = state_token(&script_events, "script-created form timestamp inspection");

    let rejected = call_session(
        &mut input,
        &responses,
        "settle-script-created-form-event-timestamps",
        "runtime.settle",
        json!({"expectedStateToken": token}),
    );
    assert_eq!(
        rejected["result"]["outcome"], "unsupported_work",
        "{rejected:#}"
    );
    assert_eq!(
        rejected["result"]["failure"]["code"], "unsupported_clock_surface",
        "{rejected:#}",
    );
    assert_eq!(
        rejected["result"]["unsupportedWork"],
        json!([{
            "kind": "other",
            "count": "1",
            "reason": "time_surface",
            "timeSurface": "host_timestamp",
        }]),
        "script-created events must remain outside the synchronous automation scope: {rejected:#}",
    );

    let closed = call_session(
        &mut input,
        &responses,
        "close-form-event-timestamp",
        "session.close",
        json!({}),
    );
    assert_eq!(closed["result"]["state"], "closed", "{closed:#}");
    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for form timestamp process");
    assert!(
        status.success(),
        "form timestamp process exited with {status}"
    );
}

#[test]
fn controlled_session_v2_css_animation_events_use_owned_dispatch_time() {
    let trace =
        exercise_css_animation_event_profile("controlled-web-session-v2", "css-events-v2", true)
            .expect("v2 CSS events must produce an owned trace");
    assert_eq!(
        trace, "armed:5|animationstart:trusted:20:20:owned>animationend:trusted:20:20:owned",
        "the RWA-proven internal animation events must use the owned dispatch batch time",
    );

    assert_eq!(
        exercise_css_animation_event_profile("controlled-web-session-v1", "css-events-v1", false,),
        None,
        "the frozen v1 profile must not inherit v2 CSS event timestamps",
    );
}

#[test]
fn controlled_session_v2_script_created_animation_and_transition_events_remain_host_stamped() {
    exercise_script_created_css_event_timestamp_boundary();
}

#[test]
fn controlled_session_v2_non_text_autofocus_remains_unsupported() {
    exercise_input_method_autofocus_profile(
        "controlled-web-session-v2",
        "number-v2",
        INPUT_METHOD_NUMBER_AUTOFOCUS_FIXTURE,
        false,
        "embedder_control",
        "focused-number",
    );
}

#[test]
fn controlled_session_v2_multiline_textarea_focus_remains_unsupported_without_v1_promotion() {
    exercise_input_method_autofocus_profile(
        "controlled-web-session-v2",
        "textarea-v2",
        INPUT_METHOD_TEXTAREA_FOCUS_FIXTURE,
        false,
        "embedder_control",
        "focused-textarea",
    );
    exercise_input_method_autofocus_profile(
        "controlled-web-session-v1",
        "textarea-v1",
        INPUT_METHOD_TEXTAREA_FOCUS_FIXTURE,
        false,
        "embedder_control",
        "focused-textarea",
    );
}

#[test]
fn controlled_session_execution_profile_survives_replacement_aba_without_v1_promotion() {
    exercise_replacement_message_channel_profile("controlled-web-session-v2", true);
    exercise_replacement_message_channel_profile("controlled-web-session-v1", false);
}

#[test]
fn controlled_buffered_messages_reenter_the_task_source_one_at_a_time() {
    let url = "https://buffered-message-channel.example.test/";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-buffered-message-channel",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    let initialized = receive(&responses);
    assert_eq!(
        initialized["id"], "init-buffered-message-channel",
        "{initialized:#}"
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-buffered-message-channel",
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v2",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(BUFFERED_MESSAGE_CHANNEL_FIXTURE)
                                    .unwrap()
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(opened["id"], "open-buffered-message-channel", "{opened:#}");
    assert_eq!(opened["sessionId"], "s-1", "{opened:#}");
    let open_token = opened["result"]["stateToken"]
        .as_str()
        .expect("buffered-message open result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "buffer-message-channel",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#buffer", "expectedStateToken": open_token},
        }),
    );
    let buffered = receive(&responses);
    assert_eq!(buffered["id"], "buffer-message-channel", "{buffered:#}");
    let buffered_token = buffered["result"]["stateToken"]
        .as_str()
        .expect("buffer action must carry stateToken")
        .to_owned();
    assert_ne!(
        buffered_token, open_token,
        "buffering controlled-local work must rotate document authority: {buffered:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "pending-buffered-message-channel",
            "sessionId": "s-1",
            "method": "runtime.pending",
            "params": {},
        }),
    );
    let pending = receive(&responses);
    assert_eq!(
        pending["id"], "pending-buffered-message-channel",
        "{pending:#}"
    );
    let message_port_sources = pending["result"]["sources"]
        .as_array()
        .expect("pending sources must be an array")
        .iter()
        .filter(|source| {
            source["kind"] == "tracked_presence" &&
                source["state"] == "open_ended" &&
                source["openEnded"]["reason"] == "message_port"
        })
        .count();
    assert_eq!(
        message_port_sources, 1,
        "one retained target port must project exactly one MessagePort source: {pending:#}",
    );
    let pending_token = pending["result"]["stateToken"]
        .as_str()
        .expect("session pending result must carry stateToken")
        .to_owned();
    assert_eq!(
        pending_token, buffered_token,
        "passive pending observation must preserve buffered document authority: {pending:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "start-buffered-message-channel",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#start", "expectedStateToken": pending_token},
        }),
    );
    let activated = receive(&responses);
    assert_eq!(
        activated["id"], "start-buffered-message-channel",
        "{activated:#}"
    );
    let action_token = activated["result"]["stateToken"]
        .as_str()
        .expect("buffered-message action result must carry stateToken")
        .to_owned();
    assert_ne!(
        action_token, pending_token,
        "starting buffered delivery must rotate document authority: {activated:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "settle-buffered-message-channel",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": action_token},
        }),
    );
    let settled = receive(&responses);
    assert_eq!(
        settled["id"], "settle-buffered-message-channel",
        "{settled:#}"
    );
    assert_eq!(settled["result"]["outcome"], "quiescent", "{settled:#}");
    let processed_tasks = settled["result"]["processed"]["tasks"]
        .as_str()
        .expect("settlement must expose the aggregate public task count")
        .parse::<u128>()
        .expect("public task count must be canonical decimal");
    assert!(
        processed_tasks >= 2,
        "two retained messages must re-enter ordinary task accounting: {settled:#}",
    );
    let settled_token = settled["result"]["stateToken"]
        .as_str()
        .expect("buffered-message settle result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-buffered-message-channel",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {
                "selector": "#result",
                "expectedStateToken": settled_token,
            },
        }),
    );
    let text = receive(&responses);
    assert_eq!(text["id"], "text-buffered-message-channel", "{text:#}");
    assert_eq!(
        text["result"]["value"], "callback1>microtask1>callback2>microtask2",
        "buffered callbacks must be separate PortMessage tasks with intervening checkpoints: {text:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "close-buffered-message-channel",
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    let closed = receive(&responses);
    assert_eq!(closed["id"], "close-buffered-message-channel", "{closed:#}");
    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for buffered-message process");
    assert!(
        status.success(),
        "buffered-message process exited with {status}"
    );
}

#[test]
fn controlled_multi_pair_pending_distinguishes_queued_and_buffered_owners() {
    let url = "https://multi-pair-message-channel.example.test/";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-multi-pair-message-channel",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    let initialized = receive(&responses);
    assert_eq!(
        initialized["id"], "init-multi-pair-message-channel",
        "{initialized:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-multi-pair-message-channel",
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v2",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(MULTI_PAIR_MESSAGE_CHANNEL_FIXTURE)
                                    .unwrap()
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(
        opened["id"], "open-multi-pair-message-channel",
        "{opened:#}"
    );
    assert_eq!(opened["sessionId"], "s-1", "{opened:#}");
    let open_token = opened["result"]["stateToken"]
        .as_str()
        .expect("multi-pair open result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "prime-multi-pair-message-channel",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#prime", "expectedStateToken": open_token},
        }),
    );
    let primed = receive(&responses);
    assert_eq!(
        primed["id"], "prime-multi-pair-message-channel",
        "{primed:#}",
    );
    let primed_token = primed["result"]["stateToken"]
        .as_str()
        .expect("multi-pair prime result must carry stateToken")
        .to_owned();
    assert_ne!(primed_token, open_token, "{primed:#}");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "pending-multi-pair-message-channel",
            "sessionId": "s-1",
            "method": "runtime.pending",
            "params": {},
        }),
    );
    let pending = receive(&responses);
    assert_eq!(
        pending["id"], "pending-multi-pair-message-channel",
        "{pending:#}",
    );
    let message_port_sources = pending["result"]["sources"]
        .as_array()
        .expect("multi-pair pending sources must be an array")
        .iter()
        .filter(|source| {
            source["kind"] == "tracked_presence" &&
                source["state"] == "open_ended" &&
                source["openEnded"]["reason"] == "message_port"
        })
        .count();
    assert_eq!(
        message_port_sources, 2,
        "one queued pair plus one buffered pair must project two distinct MessagePort owners: {pending:#}",
    );
    assert_eq!(
        pending["result"]["stateToken"], primed_token,
        "passive multi-pair observation must preserve document authority: {pending:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "start-buffered-multi-pair-message-channel",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {
                "selector": "#start-buffered",
                "expectedStateToken": primed_token,
            },
        }),
    );
    let started = receive(&responses);
    assert_eq!(
        started["id"], "start-buffered-multi-pair-message-channel",
        "{started:#}",
    );
    let started_token = started["result"]["stateToken"]
        .as_str()
        .expect("multi-pair start result must carry stateToken")
        .to_owned();
    assert_ne!(started_token, primed_token, "{started:#}");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "settle-multi-pair-message-channel",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": started_token},
        }),
    );
    let settled = receive(&responses);
    assert_eq!(
        settled["id"], "settle-multi-pair-message-channel",
        "{settled:#}",
    );
    assert_eq!(settled["result"]["outcome"], "quiescent", "{settled:#}");
    assert_eq!(
        settled["result"]["snapshot"]["sources"],
        json!([]),
        "both pair reservations must drain exactly once: {settled:#}",
    );
    let processed_tasks = settled["result"]["processed"]["tasks"]
        .as_str()
        .expect("multi-pair settlement must expose aggregate task accounting")
        .parse::<u128>()
        .expect("multi-pair task count must be canonical decimal");
    assert!(
        processed_tasks >= 1,
        "the buffered pair must re-enter ordinary task accounting after start: {settled:#}",
    );
    let settled_token = settled["result"]["stateToken"]
        .as_str()
        .expect("multi-pair settle result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-multi-pair-message-channel",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {
                "selector": "#result",
                "expectedStateToken": settled_token,
            },
        }),
    );
    let text = receive(&responses);
    assert_eq!(text["id"], "text-multi-pair-message-channel", "{text:#}");
    assert_eq!(
        text["result"]["value"], "queued:one>buffered:two",
        "queued and buffered pairs must each dispatch once in task-source order: {text:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "close-multi-pair-message-channel",
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    let closed = receive(&responses);
    assert_eq!(
        closed["id"], "close-multi-pair-message-channel",
        "{closed:#}"
    );
    drop(input);
    let status = child.wait().expect("failed to wait for multi-pair process");
    assert!(status.success(), "multi-pair process exited with {status}");
}

#[test]
fn controlled_local_message_channel_recursion_uses_the_shared_control_turn_budget() {
    let url = "https://recursive-message-channel.example.test/";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-recursive-message-channel",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    let initialized = receive(&responses);
    assert_eq!(
        initialized["id"], "init-recursive-message-channel",
        "{initialized:#}"
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-recursive-message-channel",
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v2",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(RECURSIVE_MESSAGE_CHANNEL_FIXTURE)
                                    .unwrap()
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(opened["id"], "open-recursive-message-channel", "{opened:#}");
    let open_token = opened["result"]["stateToken"]
        .as_str()
        .expect("v2 open result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "start-recursive-message-channel",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#start", "expectedStateToken": open_token},
        }),
    );
    let activated = receive(&responses);
    assert_eq!(
        activated["id"], "start-recursive-message-channel",
        "{activated:#}"
    );
    let action_token = activated["result"]["stateToken"]
        .as_str()
        .expect("v2 action result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "settle-recursive-message-channel",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {
                "expectedStateToken": action_token,
                "maxControlTurns": "32",
            },
        }),
    );
    let settled = receive(&responses);
    assert_eq!(
        settled["id"], "settle-recursive-message-channel",
        "{settled:#}"
    );
    assert_eq!(
        settled["result"]["outcome"], "control_turn_limit_exceeded",
        "{settled:#}"
    );
    assert_eq!(
        settled["result"]["limit"]["kind"], "control_turns",
        "{settled:#}"
    );
    assert_eq!(settled["result"]["limit"]["limit"], "32", "{settled:#}");
    assert_eq!(
        settled["result"]["processed"]["controlTurns"], "32",
        "{settled:#}"
    );
    let processed_tasks = settled["result"]["processed"]["tasks"]
        .as_str()
        .expect("limited settlement must expose the aggregate public task count")
        .parse::<u128>()
        .expect("public task count must be canonical decimal");
    assert!(
        processed_tasks >= 32,
        "32 controlled MessagePort callbacks require at least 32 ordinary tasks: {settled:#}",
    );
    let settled_token = settled["result"]["stateToken"]
        .as_str()
        .expect("limited settlement must carry a stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "count-recursive-message-channel",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {"selector": "#count", "expectedStateToken": settled_token},
        }),
    );
    let count = receive(&responses);
    assert_eq!(count["id"], "count-recursive-message-channel", "{count:#}");
    assert_eq!(
        count["result"]["value"], "32",
        "the control-turn limit must stop after exactly 32 MessagePort callbacks: {count:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "close-recursive-message-channel",
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    let closed = receive(&responses);
    assert_eq!(
        closed["id"], "close-recursive-message-channel",
        "{closed:#}"
    );
    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for recursive v2 process");
    assert!(
        status.success(),
        "recursive v2 process exited with {status}"
    );
}

#[test]
fn controlled_local_message_port_in_a_mixed_transfer_list_is_rejected() {
    let url = "https://message-channel-transfer.example.test/";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-message-channel-mixed-transfer",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    let initialized = receive(&responses);
    assert_eq!(
        initialized["id"], "init-message-channel-mixed-transfer",
        "{initialized:#}"
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-message-channel-mixed-transfer",
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v2",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(MESSAGE_CHANNEL_TRANSFER_FIXTURE)
                                    .unwrap()
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(
        opened["id"], "open-message-channel-mixed-transfer",
        "{opened:#}"
    );
    assert_eq!(opened["sessionId"], "s-1", "{opened:#}");
    let open_token = opened["result"]["stateToken"]
        .as_str()
        .expect("mixed-transfer open result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "start-message-channel-mixed-transfer",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#start", "expectedStateToken": open_token},
        }),
    );
    let activated = receive(&responses);
    assert_eq!(
        activated["id"], "start-message-channel-mixed-transfer",
        "{activated:#}"
    );
    let _action_token = activated["result"]["stateToken"]
        .as_str()
        .expect("mixed-transfer action result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "pending-message-channel-mixed-transfer",
            "sessionId": "s-1",
            "method": "runtime.pending",
            "params": {},
        }),
    );
    let pending = receive(&responses);
    assert_eq!(
        pending["id"], "pending-message-channel-mixed-transfer",
        "{pending:#}",
    );
    assert_eq!(
        pending["result"]["clock"]["unsupportedSurfaces"],
        json!(["external_subscription"]),
        "both transfer-list rejections must retain their exact typed boundary: {pending:#}",
    );
    assert!(
        pending["result"]["sources"]
            .as_array()
            .expect("mixed-transfer pending sources must be an array")
            .iter()
            .all(|source| source["openEnded"]["reason"] != "message_port"),
        "a rejected postMessage transfer list must not queue MessagePort work: {pending:#}",
    );
    let pending_token = pending["result"]["stateToken"]
        .as_str()
        .expect("mixed-transfer pending result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-message-channel-mixed-transfer",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {
                "selector": "#result",
                "expectedStateToken": pending_token,
            },
        }),
    );
    let text = receive(&responses);
    assert_eq!(
        text["id"], "text-message-channel-mixed-transfer",
        "{text:#}"
    );
    assert_eq!(
        text["result"]["value"],
        "clone:NotSupportedError:buffer:16|post:NotSupportedError:buffer:16|deliveries:0",
        "the JS action must prove typed rejection, no earlier detachment, and no delivery: {text:#}",
    );
    let text_token = text["result"]["stateToken"]
        .as_str()
        .expect("mixed-transfer DOM inspection must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "settle-message-channel-mixed-transfer",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": text_token},
        }),
    );
    let settled = receive(&responses);
    assert_eq!(
        settled["id"], "settle-message-channel-mixed-transfer",
        "{settled:#}"
    );
    assert_eq!(
        settled["result"]["outcome"], "unsupported_work",
        "{settled:#}"
    );
    assert_eq!(
        settled["result"]["failure"]["code"], "unsupported_clock_surface",
        "{settled:#}"
    );
    let unsupported = settled["result"]["unsupportedWork"]
        .as_array()
        .expect("mixed-transfer settlement must carry unsupported evidence");
    assert_eq!(unsupported.len(), 1, "{settled:#}");
    assert_eq!(unsupported[0]["reason"], "time_surface", "{settled:#}");
    assert_eq!(
        unsupported[0]["timeSurface"], "external_subscription",
        "{settled:#}"
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "close-message-channel-mixed-transfer",
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    let closed = receive(&responses);
    assert_eq!(
        closed["id"], "close-message-channel-mixed-transfer",
        "{closed:#}"
    );
    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for mixed-transfer process");
    assert!(
        status.success(),
        "post-open mixed-transfer rejection should remain closable: {status}",
    );
}

#[test]
fn explicitly_closed_ports_still_consume_native_capacity_until_collected() {
    exercise_post_open_rejected_message_channel_fixture(
        "closed-capacity",
        "https://message-channel-closed-capacity.example.test/",
        MESSAGE_CHANNEL_CLOSED_CAPACITY_FIXTURE,
        "controlled-web-session-v2",
        "created:16|NotSupportedError",
    );
}

#[test]
fn controlled_port_backed_transferables_reject_before_array_buffer_detachment() {
    exercise_post_open_rejected_message_channel_fixture(
        "stream-transfer-preflight",
        "https://controlled-stream-transfer.example.test/",
        CONTROLLED_TRANSFER_STREAMS_FIXTURE,
        "controlled-web-session-v2",
        "readable:NotSupportedError:buffer:16|writable:NotSupportedError:buffer:16|transform:NotSupportedError:buffer:16",
    );
}

#[test]
fn predecessor_profile_keeps_pre_v2_stream_transfer_side_effect_order() {
    exercise_post_open_rejected_message_channel_fixture(
        "stream-transfer-v1-side-effects",
        "https://controlled-stream-transfer-v1.example.test/",
        CONTROLLED_TRANSFER_STREAMS_FIXTURE,
        "controlled-web-session-v1",
        "readable:NotSupportedError:buffer:0|writable:NotSupportedError:buffer:0|transform:NotSupportedError:buffer:0",
    );
}

#[test]
fn invalid_port_backed_transferables_preserve_platform_error_precedence() {
    exercise_transfer_validation_precedence();
}

#[test]
fn predecessor_profile_keeps_locked_transform_transfer_error_precedence() {
    exercise_post_open_rejected_message_channel_fixture(
        "locked-transform-v1-precedence",
        "https://controlled-locked-transform-v1.example.test/",
        CONTROLLED_LOCKED_TRANSFORM_V1_PRECEDENCE_FIXTURE,
        "controlled-web-session-v1",
        "NotSupportedError:buffer:0",
    );
}

fn exercise_transfer_validation_precedence() {
    let url = "https://controlled-transfer-validation-precedence.example.test/";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-transfer-validation-precedence",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    let initialized = receive(&responses);
    assert_eq!(
        initialized["id"], "init-transfer-validation-precedence",
        "{initialized:#}"
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-transfer-validation-precedence",
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v2",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(
                                    CONTROLLED_TRANSFER_VALIDATION_PRECEDENCE_FIXTURE,
                                )
                                .unwrap()
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(
        opened["id"], "open-transfer-validation-precedence",
        "{opened:#}"
    );
    assert_eq!(opened["sessionId"], "s-1", "{opened:#}");
    let open_token = opened["result"]["stateToken"]
        .as_str()
        .expect("transfer-validation open result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "start-transfer-validation-precedence",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#start", "expectedStateToken": open_token},
        }),
    );
    let activated = receive(&responses);
    assert_eq!(
        activated["id"], "start-transfer-validation-precedence",
        "{activated:#}"
    );
    let action_token = activated["result"]["stateToken"]
        .as_str()
        .expect("transfer-validation action must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-transfer-validation-precedence",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {
                "selector": "#result",
                "expectedStateToken": action_token,
            },
        }),
    );
    let text = receive(&responses);
    assert_eq!(
        text["id"], "text-transfer-validation-precedence",
        "{text:#}"
    );
    assert_eq!(
        text["result"]["value"],
        "message-port:DataCloneError:buffer:16|readable:DataCloneError:buffer:16|writable:DataCloneError:buffer:16|transform:DataCloneError:buffer:16",
        "transfer validation must preserve the selected profile's exact error and detachment precedence: {text:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "settle-transfer-validation-precedence",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": action_token},
        }),
    );
    let settled = receive(&responses);
    assert_eq!(
        settled["id"], "settle-transfer-validation-precedence",
        "{settled:#}"
    );
    assert_eq!(settled["result"]["outcome"], "quiescent", "{settled:#}");
    assert!(
        settled["result"]["unsupportedWork"]
            .as_array()
            .expect("quiescent settlement must carry unsupportedWork")
            .is_empty(),
        "v2 platform validation must not latch external-subscription evidence: {settled:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "close-transfer-validation-precedence",
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    let closed = receive(&responses);
    assert_eq!(
        closed["id"], "close-transfer-validation-precedence",
        "{closed:#}"
    );

    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for transfer-validation process");
    assert!(
        status.success(),
        "transfer-validation process exited with {status}",
    );
}

#[test]
fn controlled_local_message_retention_limit_is_exact_and_sticky() {
    exercise_post_open_rejected_message_channel_fixture(
        "retained-message-limit",
        "https://message-channel-retained-limit.example.test/",
        MESSAGE_CHANNEL_RETAINED_LIMIT_FIXTURE,
        "controlled-web-session-v2",
        "posted:1024|NotSupportedError",
    );
}

#[test]
fn controlled_local_serialized_payload_limit_is_exact_and_sticky() {
    exercise_post_open_rejected_message_channel_fixture(
        "payload-boundary",
        "https://message-channel-payload-boundary.example.test/",
        MESSAGE_CHANNEL_PAYLOAD_BOUNDARY_FIXTURE,
        "controlled-web-session-v2",
        "serialized:65536=true|serialized:65544=NotSupportedError",
    );
}

#[test]
fn controlled_local_message_retention_capacity_is_released_after_delivery() {
    let url = "https://message-channel-retained-reuse.example.test/";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-message-channel-retained-reuse",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    let initialized = receive(&responses);
    assert_eq!(
        initialized["id"], "init-message-channel-retained-reuse",
        "{initialized:#}"
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-message-channel-retained-reuse",
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v2",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(
                                    MESSAGE_CHANNEL_RETAINED_REUSE_FIXTURE,
                                )
                                .unwrap()
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(
        opened["id"], "open-message-channel-retained-reuse",
        "{opened:#}"
    );
    assert_eq!(opened["sessionId"], "s-1", "{opened:#}");
    let open_token = opened["result"]["stateToken"]
        .as_str()
        .expect("retained-reuse open result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "buffer-message-channel-retained-reuse",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#buffer", "expectedStateToken": open_token},
        }),
    );
    let buffered = receive(&responses);
    assert_eq!(
        buffered["id"], "buffer-message-channel-retained-reuse",
        "{buffered:#}"
    );
    let _buffered_token = buffered["result"]["stateToken"]
        .as_str()
        .expect("retained-reuse buffer action must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "pending-message-channel-retained-reuse",
            "sessionId": "s-1",
            "method": "runtime.pending",
            "params": {},
        }),
    );
    let pending = receive(&responses);
    assert_eq!(
        pending["id"], "pending-message-channel-retained-reuse",
        "{pending:#}"
    );
    let retained_message_port_sources = pending["result"]["sources"]
        .as_array()
        .expect("retained-reuse pending sources must be an array")
        .iter()
        .filter(|source| {
            source["kind"] == "tracked_presence" &&
                source["state"] == "open_ended" &&
                source["openEnded"]["reason"] == "message_port"
        })
        .count();
    assert_eq!(
        retained_message_port_sources, 1,
        "1,024 messages retained by one target port must project exactly one MessagePort source: {pending:#}",
    );
    let pending_token = pending["result"]["stateToken"]
        .as_str()
        .expect("retained-reuse pending result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-buffered-message-channel-retained-reuse",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {
                "selector": "#result",
                "expectedStateToken": pending_token,
            },
        }),
    );
    let text = receive(&responses);
    assert_eq!(
        text["id"], "text-buffered-message-channel-retained-reuse",
        "{text:#}"
    );
    assert_eq!(
        text["result"]["value"], "buffered:1024|no-error",
        "the non-terminal flow must fill, but not exceed, retained capacity: {text:#}",
    );
    let inspected_token = text["result"]["stateToken"]
        .as_str()
        .expect("retained-reuse inspection must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "start-message-channel-retained-reuse",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#start", "expectedStateToken": inspected_token},
        }),
    );
    let started = receive(&responses);
    assert_eq!(
        started["id"], "start-message-channel-retained-reuse",
        "{started:#}"
    );
    let started_token = started["result"]["stateToken"]
        .as_str()
        .expect("retained-reuse start action must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "settle-message-channel-retained-reuse",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {
                "expectedStateToken": started_token,
                "maxControlTurns": "2048",
            },
        }),
    );
    let settled = receive(&responses);
    assert_eq!(
        settled["id"], "settle-message-channel-retained-reuse",
        "{settled:#}"
    );
    assert_eq!(settled["result"]["outcome"], "quiescent", "{settled:#}");
    assert!(
        settled["result"]["unsupportedWork"]
            .as_array()
            .expect("retained-reuse settlement must carry unsupportedWork")
            .is_empty(),
        "the bounded retained-message drain must stay controlled: {settled:#}",
    );
    let settled_token = settled["result"]["stateToken"]
        .as_str()
        .expect("retained-reuse settlement must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-drained-message-channel-retained-reuse",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {
                "selector": "#result",
                "expectedStateToken": settled_token,
            },
        }),
    );
    let drained = receive(&responses);
    assert_eq!(
        drained["id"], "text-drained-message-channel-retained-reuse",
        "{drained:#}"
    );
    assert_eq!(
        drained["result"]["value"], "delivered:1024",
        "the first bounded drain must deliver exactly 1,024 callbacks: {drained:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "reuse-message-channel-retained-capacity",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#reuse", "expectedStateToken": settled_token},
        }),
    );
    let reused = receive(&responses);
    assert_eq!(
        reused["id"], "reuse-message-channel-retained-capacity",
        "{reused:#}"
    );
    let reused_token = reused["result"]["stateToken"]
        .as_str()
        .expect("retained-capacity reuse action must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "settle-message-channel-retained-capacity-reuse",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {
                "expectedStateToken": reused_token,
                "maxControlTurns": "16",
            },
        }),
    );
    let reused_settlement = receive(&responses);
    assert_eq!(
        reused_settlement["id"], "settle-message-channel-retained-capacity-reuse",
        "{reused_settlement:#}"
    );
    assert_eq!(
        reused_settlement["result"]["outcome"], "quiescent",
        "{reused_settlement:#}"
    );
    assert!(
        reused_settlement["result"]["unsupportedWork"]
            .as_array()
            .expect("reuse settlement must carry unsupportedWork")
            .is_empty(),
        "reusing released capacity must stay controlled: {reused_settlement:#}",
    );
    let reused_settlement_token = reused_settlement["result"]["stateToken"]
        .as_str()
        .expect("reuse settlement must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-message-channel-retained-capacity-reuse",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {
                "selector": "#result",
                "expectedStateToken": reused_settlement_token,
            },
        }),
    );
    let reuse_text = receive(&responses);
    assert_eq!(
        reuse_text["id"], "text-message-channel-retained-capacity-reuse",
        "{reuse_text:#}"
    );
    assert_eq!(
        reuse_text["result"]["value"], "delivered:1025",
        "one post-drain message must reserve and deliver after capacity is released: {reuse_text:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "close-message-channel-retained-reuse",
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    let closed = receive(&responses);
    assert_eq!(
        closed["id"], "close-message-channel-retained-reuse",
        "{closed:#}"
    );

    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for retained-capacity reuse process");
    assert!(
        status.success(),
        "retained-capacity reuse process exited with {status}",
    );
}

#[test]
fn controlled_local_message_clone_sidecars_are_typed_and_sticky() {
    exercise_post_open_rejected_message_channel_fixture(
        "clone-sidecar",
        "https://message-channel-sidecar.example.test/",
        MESSAGE_CHANNEL_SIDECAR_FIXTURE,
        "controlled-web-session-v2",
        "NotSupportedError",
    );
}

#[test]
fn controlled_local_message_port_close_is_idempotent_after_gc_checkpoint() {
    let url = "https://message-channel-double-close.example.test/";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-message-channel-double-close",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    assert_eq!(
        receive(&responses)["id"],
        "init-message-channel-double-close"
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-message-channel-double-close",
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v2",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(MESSAGE_CHANNEL_DOUBLE_CLOSE_FIXTURE)
                                    .unwrap()
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(
        opened["id"], "open-message-channel-double-close",
        "{opened:#}"
    );
    let open_token = opened["result"]["stateToken"]
        .as_str()
        .expect("double-close open result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "first-message-channel-close",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#first", "expectedStateToken": open_token},
        }),
    );
    let first = receive(&responses);
    assert_eq!(first["id"], "first-message-channel-close", "{first:#}");
    let first_token = first["result"]["stateToken"]
        .as_str()
        .expect("first close must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-after-closed-peer-post",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {"selector": "#result", "expectedStateToken": first_token},
        }),
    );
    let closed_peer_text = receive(&responses);
    assert_eq!(
        closed_peer_text["id"], "text-after-closed-peer-post",
        "{closed_peer_text:#}"
    );
    assert_eq!(
        closed_peer_text["result"]["value"], "closed-peer-post:no-op",
        "a surviving locally-disentangled peer must accept postMessage as a no-op: {closed_peer_text:#}",
    );
    let closed_peer_token = closed_peer_text["result"]["stateToken"]
        .as_str()
        .expect("closed-peer inspection must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "settle-after-first-message-channel-close",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": closed_peer_token},
        }),
    );
    let settled = receive(&responses);
    assert_eq!(
        settled["id"], "settle-after-first-message-channel-close",
        "{settled:#}"
    );
    assert_eq!(
        settled["result"]["outcome"], "quiescent",
        "the closed-peer no-op must not latch external_subscription: {settled:#}",
    );
    let settled_token = settled["result"]["stateToken"]
        .as_str()
        .expect("settlement after first close must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "second-message-channel-close",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#second", "expectedStateToken": settled_token},
        }),
    );
    let second = receive(&responses);
    assert_eq!(second["id"], "second-message-channel-close", "{second:#}");
    let second_token = second["result"]["stateToken"]
        .as_str()
        .expect("idempotent second close must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-after-second-message-channel-close",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {"selector": "#result", "expectedStateToken": second_token},
        }),
    );
    let text = receive(&responses);
    assert_eq!(
        text["id"], "text-after-second-message-channel-close",
        "{text:#}"
    );
    assert_eq!(text["result"]["value"], "closed:second", "{text:#}");
    let second_text_token = text["result"]["stateToken"]
        .as_str()
        .expect("second-close inspection must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "post-and-close-both-message-ports",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {
                "selector": "#post-close-both",
                "expectedStateToken": second_text_token,
            },
        }),
    );
    let posted = receive(&responses);
    assert_eq!(
        posted["id"], "post-and-close-both-message-ports",
        "{posted:#}"
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "pending-post-and-close-both-message-ports",
            "sessionId": "s-1",
            "method": "runtime.pending",
            "params": {},
        }),
    );
    let pending = receive(&responses);
    assert_eq!(
        pending["id"], "pending-post-and-close-both-message-ports",
        "{pending:#}",
    );
    let closed_tombstone_message_port_sources = pending["result"]["sources"]
        .as_array()
        .expect("post-close pending sources must be an array")
        .iter()
        .filter(|source| {
            source["kind"] == "tracked_presence" &&
                source["state"] == "open_ended" &&
                source["openEnded"]["reason"] == "message_port"
        })
        .count();
    assert_eq!(
        closed_tombstone_message_port_sources, 1,
        "one reserved closed target must retain exactly one MessagePort identity: {pending:#}",
    );
    let pending_token = pending["result"]["stateToken"]
        .as_str()
        .expect("post-close pending result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "settle-post-and-close-both-message-ports",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": pending_token},
        }),
    );
    let drained = receive(&responses);
    assert_eq!(
        drained["id"], "settle-post-and-close-both-message-ports",
        "{drained:#}",
    );
    assert_eq!(drained["result"]["outcome"], "quiescent", "{drained:#}");
    let drained_token = drained["result"]["stateToken"]
        .as_str()
        .expect("post-close settlement must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "allocate-after-post-close-drain",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {
                "selector": "#allocate-after-drain",
                "expectedStateToken": drained_token,
            },
        }),
    );
    let allocated = receive(&responses);
    assert_eq!(
        allocated["id"], "allocate-after-post-close-drain",
        "{allocated:#}"
    );
    let allocated_token = allocated["result"]["stateToken"]
        .as_str()
        .expect("post-drain allocation must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-after-post-close-drain",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {
                "selector": "#result",
                "expectedStateToken": allocated_token,
            },
        }),
    );
    let allocation_text = receive(&responses);
    assert_eq!(
        allocation_text["id"], "text-after-post-close-drain",
        "{allocation_text:#}",
    );
    assert_eq!(
        allocation_text["result"]["value"], "allocated:16",
        "drained closed tombstones must release all 32 native entries: {allocation_text:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "close-message-channel-double-close",
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    assert_eq!(
        receive(&responses)["id"],
        "close-message-channel-double-close"
    );
    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for double-close process");
    assert!(
        status.success(),
        "double-close process exited with {status}"
    );
}

fn exercise_post_open_rejected_message_channel_fixture(
    case_id: &str,
    url: &str,
    fixture: &[u8],
    profile: &str,
    expected_text: &str,
) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("init-message-channel-{case_id}"),
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    let initialized = receive(&responses);
    assert_eq!(
        initialized["id"],
        format!("init-message-channel-{case_id}"),
        "{initialized:#}"
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("open-message-channel-{case_id}"),
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": profile,
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(fixture).unwrap()
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(
        opened["id"],
        format!("open-message-channel-{case_id}"),
        "{opened:#}"
    );
    assert_eq!(opened["sessionId"], "s-1", "{opened:#}");
    let open_token = opened["result"]["stateToken"]
        .as_str()
        .expect("post-open rejection fixture must carry an open stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("start-message-channel-{case_id}"),
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#start", "expectedStateToken": open_token},
        }),
    );
    let activated = receive(&responses);
    assert_eq!(
        activated["id"],
        format!("start-message-channel-{case_id}"),
        "{activated:#}"
    );
    let action_token = activated["result"]["stateToken"]
        .as_str()
        .expect("post-open rejection action must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("text-message-channel-{case_id}"),
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {
                "selector": "#result",
                "expectedStateToken": action_token,
            },
        }),
    );
    let text = receive(&responses);
    assert_eq!(
        text["id"],
        format!("text-message-channel-{case_id}"),
        "{text:#}"
    );
    assert_eq!(
        text["result"]["value"], expected_text,
        "the page did not observe the exact pre-mutation rejection boundary: {text:#}",
    );
    let text_token = text["result"]["stateToken"]
        .as_str()
        .expect("post-open rejection inspection must carry stateToken")
        .to_owned();

    let settle_token = if case_id == "clone-sidecar" {
        send(
            &mut input,
            json!({
                "v": 1,
                "type": "request",
                "id": "pending-message-channel-clone-sidecar",
                "sessionId": "s-1",
                "method": "runtime.pending",
                "params": {},
            }),
        );
        let pending = receive(&responses);
        assert_eq!(
            pending["id"], "pending-message-channel-clone-sidecar",
            "{pending:#}",
        );
        assert_eq!(
            pending["result"]["clock"]["unsupportedSurfaces"],
            json!(["external_subscription"]),
            "the rejected sidecar must retain its exact typed boundary: {pending:#}",
        );
        let sources = pending["result"]["sources"]
            .as_array()
            .expect("clone-sidecar pending sources must be an array");
        assert!(
            sources
                .iter()
                .all(|source| source["openEnded"]["reason"] != "message_port"),
            "a rejected sidecar post must not retain or queue MessagePort work: {pending:#}",
        );
        pending["result"]["stateToken"]
            .as_str()
            .expect("clone-sidecar pending result must carry stateToken")
            .to_owned()
    } else {
        text_token
    };

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("settle-message-channel-{case_id}"),
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": settle_token},
        }),
    );
    let settled = receive(&responses);
    assert_eq!(
        settled["id"],
        format!("settle-message-channel-{case_id}"),
        "{settled:#}"
    );
    assert_eq!(
        settled["result"]["outcome"], "unsupported_work",
        "{settled:#}"
    );
    assert_eq!(
        settled["result"]["failure"]["code"], "unsupported_clock_surface",
        "{settled:#}"
    );
    let unsupported = settled["result"]["unsupportedWork"]
        .as_array()
        .expect("post-open rejection must carry unsupported evidence");
    assert_eq!(unsupported.len(), 1, "{settled:#}");
    assert_eq!(unsupported[0]["reason"], "time_surface", "{settled:#}");
    assert_eq!(
        unsupported[0]["timeSurface"], "external_subscription",
        "{settled:#}"
    );
    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("close-message-channel-{case_id}"),
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    let closed = receive(&responses);
    assert_eq!(
        closed["id"],
        format!("close-message-channel-{case_id}"),
        "{closed:#}"
    );

    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for rejected MessageChannel process");
    assert!(
        status.success(),
        "post-open rejection fixture should remain closable: {status}",
    );
}

fn exercise_replacement_message_channel_profile(profile: &str, supported: bool) {
    let case_id = if supported { "v2" } else { "v1" };
    let source_url = "https://message-channel-navigation.example.test/a";
    let bridge_url = "https://message-channel-navigation.example.test/b";
    let fixture = std::str::from_utf8(MESSAGE_CHANNEL_NAVIGATION_FIXTURE).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("init-message-channel-navigation-{case_id}"),
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    let initialized = receive(&responses);
    assert_eq!(
        initialized["id"],
        format!("init-message-channel-navigation-{case_id}"),
        "{initialized:#}"
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("open-message-channel-navigation-{case_id}"),
            "method": "session.open",
            "params": {
                "url": source_url,
                "clockMode": "controlled",
                "profile": profile,
                "network": {
                    "mode": "fixtures_only",
                    "routes": [
                        {
                            "match": {"method": "GET", "url": {"exact": source_url}},
                            "fulfill": {
                                "status": 200,
                                "headers": [["content-type", "text/html; charset=utf-8"]],
                                "body": {"utf8": fixture},
                            },
                        },
                        {
                            "match": {"method": "GET", "url": {"exact": bridge_url}},
                            "fulfill": {
                                "status": 200,
                                "headers": [["content-type", "text/html; charset=utf-8"]],
                                "body": {"utf8": fixture},
                            },
                        },
                    ],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(
        opened["id"],
        format!("open-message-channel-navigation-{case_id}"),
        "{opened:#}"
    );
    assert_eq!(opened["result"]["profile"], profile, "{opened:#}");
    assert_eq!(opened["result"]["url"], source_url, "{opened:#}");
    let opened_token = opened["result"]["stateToken"]
        .as_str()
        .expect("controlled open must carry a document state token")
        .to_owned();
    let source_token = if supported {
        send(
            &mut input,
            json!({
                "v": 1,
                "type": "request",
                "id": "prime-message-channel-before-v2-replacement",
                "sessionId": "s-1",
                "method": "action.activate",
                "params": {
                    "selector": "#prime",
                    "expectedStateToken": opened_token.as_str(),
                },
            }),
        );
        let primed = receive(&responses);
        assert_eq!(
            primed["id"], "prime-message-channel-before-v2-replacement",
            "{primed:#}"
        );
        let primed_token = primed["result"]["stateToken"]
            .as_str()
            .expect("v2 priming action must carry a fresh document state token")
            .to_owned();
        assert_ne!(primed_token, opened_token, "{primed:#}");

        send(
            &mut input,
            json!({
                "v": 1,
                "type": "request",
                "id": "text-message-channel-before-v2-replacement",
                "sessionId": "s-1",
                "method": "dom.text",
                "params": {
                    "selector": "#result",
                    "expectedStateToken": primed_token.as_str(),
                },
            }),
        );
        let text = receive(&responses);
        assert_eq!(
            text["id"], "text-message-channel-before-v2-replacement",
            "{text:#}"
        );
        assert_eq!(
            text["result"]["value"], "primed",
            "v2 did not retain one buffered local MessagePort message before replacement: {text:#}",
        );
        assert_eq!(
            text["result"]["stateToken"], primed_token,
            "passive DOM inspection must preserve primed document authority: {text:#}",
        );

        send(
            &mut input,
            json!({
                "v": 1,
                "type": "request",
                "id": "pending-message-channel-before-v2-replacement",
                "sessionId": "s-1",
                "method": "runtime.pending",
                "params": {},
            }),
        );
        let pending = receive(&responses);
        assert_eq!(
            pending["id"], "pending-message-channel-before-v2-replacement",
            "{pending:#}",
        );
        let message_port_sources = pending["result"]["sources"]
            .as_array()
            .expect("pre-replacement pending sources must be an array")
            .iter()
            .filter(|source| {
                source["kind"] == "tracked_presence" &&
                    source["state"] == "open_ended" &&
                    source["openEnded"]["reason"] == "message_port"
            })
            .count();
        assert_eq!(
            message_port_sources, 1,
            "document A must own exactly one buffered MessagePort source before replacement: {pending:#}",
        );
        assert_eq!(
            pending["result"]["stateToken"], primed_token,
            "passive pending observation must preserve primed authority: {pending:#}",
        );

        primed_token
    } else {
        opened_token
    };

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("navigate-message-channel-bridge-{case_id}"),
            "sessionId": "s-1",
            "method": "session.navigate",
            "params": {
                "url": bridge_url,
                "expectedStateToken": source_token,
            },
        }),
    );
    let navigated_to_bridge = receive(&responses);
    assert_eq!(
        navigated_to_bridge["id"],
        format!("navigate-message-channel-bridge-{case_id}"),
        "{navigated_to_bridge:#}"
    );
    assert_eq!(
        navigated_to_bridge["result"]["boundary"], "controlled_ready",
        "{navigated_to_bridge:#}"
    );
    assert_eq!(
        navigated_to_bridge["result"]["requestedUrl"], bridge_url,
        "{navigated_to_bridge:#}"
    );
    assert_eq!(
        navigated_to_bridge["result"]["url"], bridge_url,
        "{navigated_to_bridge:#}"
    );
    assert_eq!(
        navigated_to_bridge["result"]["documentEpoch"], "2",
        "{navigated_to_bridge:#}"
    );
    assert_eq!(
        navigated_to_bridge["result"]["navigationId"], "1",
        "{navigated_to_bridge:#}"
    );
    let bridge_token = navigated_to_bridge["result"]["stateToken"]
        .as_str()
        .expect("first replacement must carry a fresh document state token")
        .to_owned();
    assert_ne!(bridge_token, source_token, "{navigated_to_bridge:#}");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("navigate-message-channel-return-{case_id}"),
            "sessionId": "s-1",
            "method": "session.navigate",
            "params": {
                "url": source_url,
                "expectedStateToken": bridge_token,
            },
        }),
    );
    let returned = receive(&responses);
    assert_eq!(
        returned["id"],
        format!("navigate-message-channel-return-{case_id}"),
        "{returned:#}"
    );
    assert_eq!(
        returned["result"]["boundary"], "controlled_ready",
        "{returned:#}"
    );
    assert_eq!(
        returned["result"]["requestedUrl"], source_url,
        "{returned:#}"
    );
    assert_eq!(returned["result"]["url"], source_url, "{returned:#}");
    assert_eq!(returned["result"]["documentEpoch"], "3", "{returned:#}");
    assert_eq!(returned["result"]["navigationId"], "2", "{returned:#}");
    let returned_token = returned["result"]["stateToken"]
        .as_str()
        .expect("ABA return must carry a fresh document state token")
        .to_owned();
    assert_ne!(returned_token, source_token, "{returned:#}");
    assert_ne!(returned_token, bridge_token, "{returned:#}");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("pending-message-channel-after-return-{case_id}"),
            "sessionId": "s-1",
            "method": "runtime.pending",
            "params": {},
        }),
    );
    let returned_pending = receive(&responses);
    assert_eq!(
        returned_pending["id"],
        format!("pending-message-channel-after-return-{case_id}"),
        "{returned_pending:#}",
    );
    let returned_message_port_sources = returned_pending["result"]["sources"]
        .as_array()
        .expect("post-return pending sources must be an array")
        .iter()
        .filter(|source| {
            source["kind"] == "tracked_presence" &&
                source["state"] == "open_ended" &&
                source["openEnded"]["reason"] == "message_port"
        })
        .count();
    assert_eq!(
        returned_message_port_sources, 0,
        "A -> B -> A replacement must retire every old-document MessagePort reservation: {returned_pending:#}",
    );
    assert_eq!(
        returned_pending["result"]["stateToken"], returned_token,
        "post-return pending observation must preserve returned authority: {returned_pending:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("stale-message-channel-source-{case_id}"),
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {
                "selector": "#result",
                "expectedStateToken": source_token,
            },
        }),
    );
    let stale = receive(&responses);
    assert_eq!(
        stale["id"],
        format!("stale-message-channel-source-{case_id}"),
        "{stale:#}"
    );
    assert_eq!(stale["error"]["code"], "stale_state_token", "{stale:#}");
    assert_eq!(stale["error"]["fatal"], false, "{stale:#}");
    assert_eq!(stale["error"]["stateEffect"], "none", "{stale:#}");

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("start-message-channel-after-return-{case_id}"),
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {
                "selector": "#start",
                "expectedStateToken": returned_token,
            },
        }),
    );
    let activated = receive(&responses);
    assert_eq!(
        activated["id"],
        format!("start-message-channel-after-return-{case_id}"),
        "{activated:#}"
    );
    let action_token = activated["result"]["stateToken"]
        .as_str()
        .expect("post-replacement action must carry a fresh document state token")
        .to_owned();
    assert_ne!(action_token, returned_token, "{activated:#}");

    if supported {
        send(
            &mut input,
            json!({
                "v": 1,
                "type": "request",
                "id": "settle-message-channel-after-v2-return",
                "sessionId": "s-1",
                "method": "runtime.settle",
                "params": {"expectedStateToken": action_token},
            }),
        );
        let settled = receive(&responses);
        assert_eq!(
            settled["id"], "settle-message-channel-after-v2-return",
            "{settled:#}"
        );
        assert_eq!(settled["result"]["outcome"], "quiescent", "{settled:#}");
        let processed_tasks = settled["result"]["processed"]["tasks"]
            .as_str()
            .expect("settlement must expose the aggregate public task count")
            .parse::<u128>()
            .expect("public task count must be canonical decimal");
        assert!(
            processed_tasks >= 1,
            "the post-replacement MessagePort callback must consume an ordinary task: {settled:#}",
        );
        let settled_token = settled["result"]["stateToken"]
            .as_str()
            .expect("v2 settlement must carry a document state token")
            .to_owned();

        send(
            &mut input,
            json!({
                "v": 1,
                "type": "request",
                "id": "text-message-channel-after-v2-return",
                "sessionId": "s-1",
                "method": "dom.text",
                "params": {
                    "selector": "#result",
                    "expectedStateToken": settled_token,
                },
            }),
        );
        let text = receive(&responses);
        assert_eq!(
            text["id"], "text-message-channel-after-v2-return",
            "{text:#}"
        );
        assert_eq!(
            text["result"]["value"], "message:after-replacement",
            "v2 lost controlled-local MessageChannel authority across A->B->A: {text:#}",
        );
    } else {
        send(
            &mut input,
            json!({
                "v": 1,
                "type": "request",
                "id": "text-message-channel-after-v1-return",
                "sessionId": "s-1",
                "method": "dom.text",
                "params": {
                    "selector": "#result",
                    "expectedStateToken": action_token,
                },
            }),
        );
        let text = receive(&responses);
        assert_eq!(
            text["id"], "text-message-channel-after-v1-return",
            "{text:#}"
        );
        assert_eq!(
            text["result"]["value"], "NotSupportedError",
            "v1 was silently promoted after A->B->A replacement: {text:#}",
        );

        send(
            &mut input,
            json!({
                "v": 1,
                "type": "request",
                "id": "settle-message-channel-after-v1-return",
                "sessionId": "s-1",
                "method": "runtime.settle",
                "params": {"expectedStateToken": action_token},
            }),
        );
        let settled = receive(&responses);
        assert_eq!(
            settled["id"], "settle-message-channel-after-v1-return",
            "{settled:#}"
        );
        assert_eq!(
            settled["result"]["outcome"], "unsupported_work",
            "{settled:#}"
        );
        assert_eq!(
            settled["result"]["failure"]["code"], "unsupported_clock_surface",
            "{settled:#}"
        );
        let unsupported = settled["result"]["unsupportedWork"]
            .as_array()
            .expect("v1 rejection must remain sticky settlement evidence");
        assert_eq!(unsupported.len(), 1, "{settled:#}");
        assert_eq!(unsupported[0]["reason"], "time_surface", "{settled:#}");
        assert_eq!(
            unsupported[0]["timeSurface"], "external_subscription",
            "{settled:#}"
        );
    }

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("close-message-channel-navigation-{case_id}"),
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    let closed = receive(&responses);
    assert_eq!(
        closed["id"],
        format!("close-message-channel-navigation-{case_id}"),
        "{closed:#}"
    );
    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for replacement MessageChannel process");
    assert!(
        status.success(),
        "post-replacement {profile} process exited with {status}",
    );
}

fn exercise_controlled_inline_svg_advanced() {
    let url = "https://controlled-inline-svg-advanced.example.test/";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-controlled-inline-svg-advanced",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    assert_eq!(
        receive(&responses)["id"],
        "init-controlled-inline-svg-advanced",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-controlled-inline-svg-advanced",
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v2",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(
                                    CONTROLLED_V2_INLINE_SVG_ADVANCED_FIXTURE,
                                )
                                .unwrap(),
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(
        opened["id"], "open-controlled-inline-svg-advanced",
        "{opened:#}",
    );
    let open_token = opened["result"]["stateToken"]
        .as_str()
        .expect("advanced inline SVG open must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "schedule-controlled-inline-svg-advanced",
            "sessionId": "s-1",
            "method": "action.activate",
            "params": {"selector": "#start", "expectedStateToken": open_token},
        }),
    );
    let scheduled = receive(&responses);
    assert_eq!(
        scheduled["id"], "schedule-controlled-inline-svg-advanced",
        "{scheduled:#}",
    );
    let scheduled_token = scheduled["result"]["stateToken"]
        .as_str()
        .expect("advanced inline SVG timer must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "qualify-controlled-inline-svg-advance",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": scheduled_token, "maxVirtualTimeNs": "0"},
        }),
    );
    let qualified = receive(&responses);
    assert_eq!(
        qualified["id"], "qualify-controlled-inline-svg-advance",
        "{qualified:#}",
    );
    assert_eq!(
        qualified["result"]["outcome"], "virtual_time_limit_exceeded",
        "{qualified:#}",
    );
    assert_eq!(qualified["result"]["virtualTimeNs"], "0", "{qualified:#}");
    assert_eq!(
        qualified["result"]["limit"]["requestedVirtualTimeNs"], "5000000",
        "{qualified:#}",
    );
    let qualified_token = qualified["result"]["stateToken"]
        .as_str()
        .expect("qualified inline SVG timer must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "advance-controlled-inline-svg",
            "sessionId": "s-1",
            "method": "runtime.advance_to_next",
            "params": {"expectedStateToken": qualified_token},
        }),
    );
    let advanced = receive(&responses);
    assert_eq!(
        advanced["id"], "advance-controlled-inline-svg",
        "{advanced:#}"
    );
    assert_eq!(advanced["result"]["outcome"], "advanced", "{advanced:#}");
    assert_eq!(
        advanced["result"]["virtualTimeNs"], "5000000",
        "{advanced:#}"
    );
    let advanced_token = advanced["result"]["stateToken"]
        .as_str()
        .expect("advanced inline SVG timer must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "settle-controlled-inline-svg-advanced",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": advanced_token},
        }),
    );
    let settled = receive(&responses);
    assert_eq!(
        settled["id"], "settle-controlled-inline-svg-advanced",
        "{settled:#}",
    );
    assert_eq!(settled["result"]["outcome"], "quiescent", "{settled:#}");
    assert_eq!(
        settled["result"]["unsupportedWork"],
        json!([]),
        "{settled:#}"
    );
    assert_eq!(
        settled["result"]["snapshot"]["producers"]["pending"], "0",
        "{settled:#}",
    );
    assert_eq!(
        settled["result"]["snapshot"]["rendering"]["updateRequired"], false,
        "{settled:#}",
    );
    assert!(
        settled["result"]["snapshot"]["rendering"]["nextOpportunityNs"].is_null(),
        "{settled:#}",
    );
    let settled_token = settled["result"]["stateToken"]
        .as_str()
        .expect("settled advanced inline SVG must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-controlled-inline-svg-advanced",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {"selector": "#result", "expectedStateToken": settled_token},
        }),
    );
    let text = receive(&responses);
    assert_eq!(
        text["id"], "text-controlled-inline-svg-advanced",
        "{text:#}",
    );
    assert_eq!(
        text["result"]["value"], "inline-svg:5|load-events:0",
        "inline SVG raster completion must settle at the advanced document time without a DOM load event: {text:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "close-controlled-inline-svg-advanced",
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    assert_eq!(
        receive(&responses)["id"],
        "close-controlled-inline-svg-advanced",
    );
    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for advanced inline SVG process");
    assert!(
        status.success(),
        "advanced inline SVG process exited with {status}",
    );
}

fn exercise_css_animation_event_profile(
    profile: &str,
    case_id: &str,
    expect_owned: bool,
) -> Option<String> {
    let url = format!("https://controlled-css-{case_id}.example.test/");
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("init-{case_id}"),
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    assert_eq!(receive(&responses)["id"], format!("init-{case_id}"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("open-{case_id}"),
            "method": "session.open",
            "params": {
                "url": url.as_str(),
                "clockMode": "controlled",
                "profile": profile,
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url.as_str()}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(
                                    CONTROLLED_V2_CSS_ANIMATION_EVENT_TIMESTAMP_FIXTURE,
                                )
                                .unwrap(),
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(opened["id"], format!("open-{case_id}"), "{opened:#}");
    assert_eq!(opened["result"]["profile"], profile, "{opened:#}");
    let open_token = state_token(&opened, "CSS event fixture open");

    let qualified = call_session(
        &mut input,
        &responses,
        &format!("qualify-{case_id}"),
        "runtime.settle",
        json!({"expectedStateToken": open_token, "maxVirtualTimeNs": "0"}),
    );
    assert_eq!(qualified["result"]["outcome"], "quiescent", "{qualified:#}");
    assert_eq!(
        qualified["result"]["virtualTimeNs"], "5000000",
        "session.open must have advanced and consumed the exact 5 ms fixture timer: {qualified:#}",
    );

    let (_, action_token) = call_controlled_action(
        &mut input,
        &responses,
        &format!("start-{case_id}"),
        "action.activate",
        json!({"selector": "#start"}),
        &state_token(&qualified, "settled 5 ms CSS event timer"),
    );
    let settled = call_session(
        &mut input,
        &responses,
        &format!("settle-{case_id}"),
        "runtime.settle",
        json!({"expectedStateToken": action_token}),
    );

    let trace = if expect_owned {
        assert_eq!(settled["result"]["outcome"], "quiescent", "{settled:#}");
        assert_eq!(
            settled["result"]["unsupportedWork"],
            json!([]),
            "{settled:#}"
        );
        let text = call_session(
            &mut input,
            &responses,
            &format!("text-{case_id}"),
            "dom.text",
            json!({
                "selector": "#result",
                "expectedStateToken": state_token(&settled, "settled CSS events"),
            }),
        );
        Some(
            text["result"]["value"]
                .as_str()
                .expect("CSS event trace must be text")
                .to_owned(),
        )
    } else {
        assert_eq!(
            settled["result"]["outcome"], "unsupported_work",
            "{settled:#}"
        );
        assert_eq!(
            settled["result"]["failure"]["code"], "unsupported_clock_surface",
            "{settled:#}",
        );
        assert_eq!(
            settled["result"]["unsupportedWork"],
            json!([{
                "kind": "other",
                "count": "1",
                "reason": "time_surface",
                "timeSurface": "host_timestamp",
            }]),
            "v1 CSS events must preserve the host timestamp boundary: {settled:#}",
        );
        None
    };

    let closed = call_session(
        &mut input,
        &responses,
        &format!("close-{case_id}"),
        "session.close",
        json!({}),
    );
    assert_eq!(closed["result"]["state"], "closed", "{closed:#}");
    drop(input);
    let status = child.wait().expect("failed to wait for CSS event process");
    assert!(status.success(), "CSS event process exited with {status}");
    trace
}

fn exercise_script_created_css_event_timestamp_boundary() {
    let case_id = "script-css-events-v2";
    let url = "https://controlled-css-script-events-v2.example.test/";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("init-{case_id}"),
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    assert_eq!(receive(&responses)["id"], format!("init-{case_id}"));
    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("open-{case_id}"),
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": "controlled-web-session-v2",
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(
                                    CONTROLLED_V2_CSS_ANIMATION_EVENT_TIMESTAMP_FIXTURE,
                                )
                                .unwrap(),
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(opened["id"], format!("open-{case_id}"), "{opened:#}");
    let open_token = state_token(&opened, "script CSS event fixture open");
    let qualified = call_session(
        &mut input,
        &responses,
        &format!("qualify-{case_id}"),
        "runtime.settle",
        json!({"expectedStateToken": open_token, "maxVirtualTimeNs": "0"}),
    );
    assert_eq!(qualified["result"]["outcome"], "quiescent", "{qualified:#}");
    assert_eq!(
        qualified["result"]["virtualTimeNs"], "5000000",
        "{qualified:#}",
    );
    let (_, script_token) = call_controlled_action(
        &mut input,
        &responses,
        &format!("create-{case_id}"),
        "action.activate",
        json!({"selector": "#script-created"}),
        &state_token(&qualified, "settled 5 ms script CSS event timer"),
    );
    let text = call_session(
        &mut input,
        &responses,
        &format!("text-{case_id}"),
        "dom.text",
        json!({"selector": "#result", "expectedStateToken": script_token}),
    );
    assert_eq!(
        text["result"]["value"], "armed:-1|script:0,0",
        "WebIDL constructors must not inherit the internal CSS dispatch timestamp: {text:#}",
    );
    let rejected = call_session(
        &mut input,
        &responses,
        &format!("settle-{case_id}"),
        "runtime.settle",
        json!({
            "expectedStateToken": state_token(&text, "script CSS event trace"),
        }),
    );
    assert_eq!(
        rejected["result"]["outcome"], "unsupported_work",
        "{rejected:#}"
    );
    assert_eq!(
        rejected["result"]["failure"]["code"], "unsupported_clock_surface",
        "{rejected:#}",
    );
    assert_eq!(
        rejected["result"]["unsupportedWork"],
        json!([{
            "kind": "other",
            "count": "1",
            "reason": "time_surface",
            "timeSurface": "host_timestamp",
        }]),
        "script-created AnimationEvent and TransitionEvent must stay host-stamped: {rejected:#}",
    );

    let closed = call_session(
        &mut input,
        &responses,
        &format!("close-{case_id}"),
        "session.close",
        json!({}),
    );
    assert_eq!(closed["result"]["state"], "closed", "{closed:#}");
    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for script CSS event process");
    assert!(
        status.success(),
        "script CSS event process exited with {status}",
    );
}

fn exercise_controlled_data_svg_profile(
    profile: &str,
    case_id: &str,
    fixture: &[u8],
    expected_text: Option<&str>,
) {
    let url = format!("https://controlled-image-{case_id}.example.test/");
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("init-controlled-image-{case_id}"),
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    assert_eq!(
        receive(&responses)["id"],
        format!("init-controlled-image-{case_id}"),
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("open-controlled-image-{case_id}"),
            "method": "session.open",
            "params": {
                "url": url.as_str(),
                "clockMode": "controlled",
                "profile": profile,
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url.as_str()}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {"utf8": std::str::from_utf8(fixture).unwrap()},
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(
        opened["id"],
        format!("open-controlled-image-{case_id}"),
        "{opened:#}",
    );

    let Some(expected_text) = expected_text else {
        assert_eq!(opened["error"]["code"], "unsupported_work", "{opened:#}");
        assert_eq!(opened["error"]["fatal"], true, "{opened:#}");
        let failure_code = opened["error"]["details"]["failure"]["code"]
            .as_str()
            .expect("v1 image rejection must carry a typed failure code");
        assert!(
            matches!(
                failure_code,
                "unsupported_rendering" | "unsupported_clock_surface"
            ),
            "frozen v1 must retain its image/time boundary: {opened:#}",
        );
        drop(input);
        let status = child
            .wait()
            .expect("failed to wait for rejected v1 image process");
        assert_eq!(
            status.code(),
            Some(70),
            "fatal v1 image rejection must use the documented exit code: {status}",
        );
        return;
    };

    assert_eq!(opened["sessionId"], "s-1", "{opened:#}");
    assert_eq!(opened["result"]["profile"], profile, "{opened:#}");
    let open_token = opened["result"]["stateToken"]
        .as_str()
        .expect("controlled image open result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("settle-controlled-image-{case_id}"),
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": open_token},
        }),
    );
    let settled = receive(&responses);
    assert_eq!(
        settled["id"],
        format!("settle-controlled-image-{case_id}"),
        "{settled:#}",
    );
    assert_eq!(settled["result"]["outcome"], "quiescent", "{settled:#}");
    assert_eq!(
        settled["result"]["unsupportedWork"],
        json!([]),
        "{settled:#}"
    );
    assert_eq!(settled["result"]["externalIo"], json!([]), "{settled:#}");
    assert_eq!(
        settled["result"]["snapshot"]["producers"]["pending"], "0",
        "{settled:#}"
    );
    assert_eq!(
        settled["result"]["snapshot"]["producers"]["terminal"], false,
        "{settled:#}"
    );
    let settled_token = settled["result"]["stateToken"]
        .as_str()
        .expect("controlled image settlement must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("text-controlled-image-{case_id}"),
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {"selector": "#result", "expectedStateToken": settled_token},
        }),
    );
    let text = receive(&responses);
    assert_eq!(
        text["id"],
        format!("text-controlled-image-{case_id}"),
        "{text:#}",
    );
    assert_eq!(
        text["result"]["value"], expected_text,
        "controlled image events must use the exact document completion time: {text:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("close-controlled-image-{case_id}"),
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    assert_eq!(
        receive(&responses)["id"],
        format!("close-controlled-image-{case_id}"),
    );
    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for controlled image process");
    assert!(
        status.success(),
        "controlled image process exited with {status}"
    );
}

fn exercise_input_method_autofocus_profile(
    profile: &str,
    case_id: &str,
    fixture: &[u8],
    supported: bool,
    unsupported_time_surface: &str,
    expected_text: &str,
) {
    let url = format!("https://input-method-autofocus-{case_id}.example.test/");
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("init-input-method-autofocus-{case_id}"),
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    let initialized = receive(&responses);
    assert_eq!(
        initialized["id"],
        format!("init-input-method-autofocus-{case_id}"),
        "{initialized:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("open-input-method-autofocus-{case_id}"),
            "method": "session.open",
            "params": {
                "url": url.as_str(),
                "clockMode": "controlled",
                "profile": profile,
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url.as_str()}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {
                                "utf8": std::str::from_utf8(fixture).unwrap()
                            },
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(
        opened["id"],
        format!("open-input-method-autofocus-{case_id}"),
        "{opened:#}",
    );

    if !supported {
        assert_eq!(opened["error"]["code"], "unsupported_work", "{opened:#}");
        assert_eq!(opened["error"]["fatal"], true, "{opened:#}");
        assert_eq!(opened["error"]["stateEffect"], "partial", "{opened:#}");
        assert_eq!(
            opened["error"]["details"]["failure"]["code"], "unsupported_clock_surface",
            "{opened:#}",
        );
        assert_eq!(
            opened["error"]["details"]["unsupportedWork"],
            json!([{
                "kind": "other",
                "count": "1",
                "reason": "time_surface",
                "timeSurface": unsupported_time_surface,
            }]),
            "unsupported autofocus must retain its exact InputMethod boundary: {opened:#}",
        );
        drop(input);
        let status = child
            .wait()
            .expect("failed to wait for rejected autofocus process");
        assert_eq!(
            status.code(),
            Some(70),
            "fatal autofocus rejection must use the documented exit code: {status}",
        );
        return;
    }

    assert_eq!(opened["sessionId"], "s-1", "{opened:#}");
    assert_eq!(opened["result"]["profile"], profile, "{opened:#}");
    let open_token = opened["result"]["stateToken"]
        .as_str()
        .expect("v2 autofocus open result must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("settle-input-method-autofocus-{case_id}"),
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": open_token},
        }),
    );
    let settled = receive(&responses);
    assert_eq!(
        settled["id"],
        format!("settle-input-method-autofocus-{case_id}"),
        "{settled:#}",
    );
    assert_eq!(settled["result"]["outcome"], "quiescent", "{settled:#}");
    assert_eq!(
        settled["result"]["unsupportedWork"],
        json!([]),
        "{settled:#}"
    );
    let settled_token = settled["result"]["stateToken"]
        .as_str()
        .expect("v2 autofocus settlement must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("text-input-method-autofocus-{case_id}"),
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {
                "selector": "#result",
                "expectedStateToken": settled_token,
            },
        }),
    );
    let text = receive(&responses);
    assert_eq!(
        text["id"],
        format!("text-input-method-autofocus-{case_id}"),
        "{text:#}",
    );
    assert_eq!(
        text["result"]["value"], expected_text,
        "v2 suppression must preserve autofocus, its event, value, and selection: {text:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": format!("close-input-method-autofocus-{case_id}"),
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    let closed = receive(&responses);
    assert_eq!(
        closed["id"],
        format!("close-input-method-autofocus-{case_id}"),
        "{closed:#}",
    );
    drop(input);
    let status = child
        .wait()
        .expect("failed to wait for v2 autofocus process");
    assert!(
        status.success(),
        "v2 autofocus process exited with {status}"
    );
}

fn exercise_message_channel_profile(profile: &str, supported: bool) {
    let url = "https://message-channel.example.test/";
    let mut child = Command::new(env!("CARGO_BIN_EXE_stasis"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn stasis");
    let mut input = child.stdin.take().expect("missing child stdin");
    let responses = spawn_response_reader(child.stdout.take().expect("missing child stdout"));

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "init-message-channel",
            "method": "protocol.initialize",
            "params": {"client": {"name": "integration-test", "version": "0.0.0"}},
        }),
    );
    let initialized = receive(&responses);
    assert_eq!(initialized["id"], "init-message-channel", "{initialized:#}");
    assert!(
        initialized["result"]["capabilities"]["profiles"]
            .as_array()
            .is_some_and(|profiles| profiles.iter().any(|candidate| candidate == profile)),
        "runtime did not advertise {profile}: {initialized:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "open-message-channel",
            "method": "session.open",
            "params": {
                "url": url,
                "clockMode": "controlled",
                "profile": profile,
                "network": {
                    "mode": "fixtures_only",
                    "routes": [{
                        "match": {"method": "GET", "url": {"exact": url}},
                        "fulfill": {
                            "status": 200,
                            "headers": [["content-type", "text/html; charset=utf-8"]],
                            "body": {"utf8": std::str::from_utf8(MESSAGE_CHANNEL_FIXTURE).unwrap()},
                        },
                    }],
                },
            },
        }),
    );
    let opened = receive(&responses);
    assert_eq!(opened["id"], "open-message-channel", "{opened:#}");

    if !supported {
        assert_eq!(opened["error"]["code"], "unsupported_work", "{opened:#}");
        assert_eq!(opened["error"]["fatal"], true, "{opened:#}");
        assert_eq!(opened["error"]["stateEffect"], "partial", "{opened:#}");
        assert_eq!(
            opened["error"]["details"]["failure"]["code"], "unsupported_clock_surface",
            "{opened:#}",
        );
        assert_eq!(
            opened["error"]["details"]["unsupportedWork"],
            json!([{
                "kind": "other",
                "count": "1",
                "reason": "time_surface",
                "timeSurface": "external_subscription",
            }]),
            "the frozen v1 parser-script MessageChannel boundary must remain exact: {opened:#}",
        );
        drop(input);
        let status = child
            .wait()
            .expect("failed to wait for rejected v1 process");
        assert_eq!(
            status.code(),
            Some(70),
            "fatal v1 rejection must use the shell's documented fatal exit code: {status}",
        );
        return;
    }

    assert_eq!(opened["sessionId"], "s-1", "{opened:#}");
    assert_eq!(opened["result"]["profile"], profile, "{opened:#}");
    let state_token = opened["result"]["stateToken"]
        .as_str()
        .expect("v2 open result must include a document state token")
        .to_owned();
    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "pending-idle-message-channel",
            "sessionId": "s-1",
            "method": "runtime.pending",
            "params": {},
        }),
    );
    let idle_pending = receive(&responses);
    assert_eq!(
        idle_pending["id"], "pending-idle-message-channel",
        "{idle_pending:#}",
    );
    assert_eq!(
        idle_pending["result"]["sources"],
        json!([]),
        "an open but empty controlled-local pair must not project a pending source: {idle_pending:#}",
    );
    assert_eq!(
        idle_pending["result"]["producers"]["pending"], "0",
        "{idle_pending:#}",
    );
    assert_eq!(
        idle_pending["result"]["producers"]["stability"], "stable_empty",
        "{idle_pending:#}",
    );
    assert_eq!(
        idle_pending["result"]["stateToken"], state_token,
        "passive idle-pair observation must preserve authority: {idle_pending:#}",
    );
    let idle_pending_token = idle_pending["result"]["stateToken"]
        .as_str()
        .expect("idle MessageChannel pending snapshot must carry stateToken")
        .to_owned();

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "settle-idle-message-channel",
            "sessionId": "s-1",
            "method": "runtime.settle",
            "params": {"expectedStateToken": idle_pending_token},
        }),
    );
    let idle_settled = receive(&responses);
    assert_eq!(
        idle_settled["id"], "settle-idle-message-channel",
        "{idle_settled:#}",
    );
    assert_eq!(
        idle_settled["result"]["outcome"], "quiescent",
        "an open but empty controlled-local pair must permit settlement: {idle_settled:#}",
    );
    assert_eq!(
        idle_settled["result"]["snapshot"]["sources"],
        json!([]),
        "idle settlement must not hide MessagePort work: {idle_settled:#}",
    );
    assert_eq!(
        idle_settled["result"]["persistentWork"],
        json!([]),
        "{idle_settled:#}",
    );
    assert_eq!(
        idle_settled["result"]["externalIo"],
        json!([]),
        "{idle_settled:#}",
    );
    assert_eq!(
        idle_settled["result"]["unsupportedWork"],
        json!([]),
        "{idle_settled:#}",
    );
    let idle_settled_token = idle_settled["result"]["stateToken"]
        .as_str()
        .expect("idle MessageChannel settlement must carry stateToken")
        .to_owned();
    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "text-message-channel",
            "sessionId": "s-1",
            "method": "dom.text",
            "params": {
                "selector": "#result",
                "expectedStateToken": idle_settled_token,
            },
        }),
    );
    let text = receive(&responses);
    assert_eq!(text["id"], "text-message-channel", "{text:#}");
    assert_eq!(
        text["result"]["value"], "script>message:1>message:2>message:3>microtask",
        "local MessageChannel delivery was not FIFO and microtask-complete: {text:#}",
    );

    send(
        &mut input,
        json!({
            "v": 1,
            "type": "request",
            "id": "close-message-channel",
            "sessionId": "s-1",
            "method": "session.close",
            "params": {},
        }),
    );
    let closed = receive(&responses);
    assert_eq!(closed["id"], "close-message-channel", "{closed:#}");
    drop(input);
    let status = child.wait().expect("failed to wait for v2 process");
    assert!(status.success(), "v2 process exited with {status}");
}

fn assert_title(input: &mut ChildStdin, responses: &Receiver<String>, id: &str) {
    send(
        input,
        json!({
            "v": 1,
            "type": "request",
            "id": id,
            "sessionId": "s-1",
            "method": "dom.evaluate",
            "params": {"expression": "document.title"},
        }),
    );
    let evaluated = receive(responses);
    assert_eq!(evaluated["id"], id);
    assert_eq!(evaluated["result"]["value"]["kind"], "string");
    assert_eq!(
        evaluated["result"]["value"]["value"],
        "Stasis Automation Fixture"
    );
}

fn call_session(
    input: &mut ChildStdin,
    responses: &Receiver<String>,
    id: &str,
    method: &str,
    params: Value,
) -> Value {
    send(
        input,
        json!({
            "v": 1,
            "type": "request",
            "id": id,
            "sessionId": "s-1",
            "method": method,
            "params": params,
        }),
    );
    let response = receive(responses);
    assert_eq!(response["id"], id, "{response:#}");
    response
}

fn state_token(response: &Value, context: &str) -> String {
    response["result"]["stateToken"]
        .as_str()
        .unwrap_or_else(|| panic!("{context} must carry stateToken: {response:#}"))
        .to_owned()
}

fn call_controlled_action(
    input: &mut ChildStdin,
    responses: &Receiver<String>,
    id: &str,
    method: &str,
    mut params: Value,
    expected_state_token: &str,
) -> (Value, String) {
    params["expectedStateToken"] = Value::String(expected_state_token.to_owned());
    let response = call_session(input, responses, id, method, params);
    let token = state_token(&response, id);
    assert_ne!(
        token, expected_state_token,
        "{method} must rotate document authority: {response:#}",
    );
    (response, token)
}

fn send(input: &mut ChildStdin, frame: Value) {
    serde_json::to_writer(&mut *input, &frame).expect("failed to encode request");
    input.write_all(b"\n").expect("failed to frame request");
    input.flush().expect("failed to flush request");
}

fn spawn_response_reader(stdout: impl Read + Send + 'static) -> Receiver<String> {
    let (sender, receiver) = sync_channel(8);
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if sender
                .send(line.expect("failed to read protocol output"))
                .is_err()
            {
                return;
            }
        }
    });
    receiver
}

fn receive(responses: &Receiver<String>) -> Value {
    let line = responses
        .recv_timeout(RESPONSE_TIMEOUT)
        .expect("timed out waiting for protocol response");
    serde_json::from_str(&line).expect("stdout contained a non-JSON protocol line")
}

fn start_redirect_fixture() -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind fixture server");
    let address = listener.local_addr().unwrap();
    let base_url = format!("http://{address}/");
    let thread_url = base_url.clone();
    let handle = thread::spawn(move || {
        for _ in 0..2 {
            let (stream, _) = listener.accept().expect("failed to accept fixture request");
            serve_fixture_request(stream, &thread_url);
        }
    });
    (base_url, handle)
}

fn serve_fixture_request(mut stream: TcpStream, base_url: &str) {
    let mut request = [0_u8; 4096];
    let read = stream
        .read(&mut request)
        .expect("failed to read fixture request");
    let request = String::from_utf8_lossy(&request[..read]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("missing fixture request path");

    if path == "/" {
        write!(
            stream,
            "HTTP/1.1 302 Found\r\nLocation: {base_url}final\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
    } else {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            FIXTURE.len()
        )
        .unwrap();
        stream.write_all(FIXTURE).unwrap();
    }
    stream.flush().unwrap();
}
