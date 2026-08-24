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
        json!(["controlled-webapp-v1", "controlled-web-session-v1"])
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
