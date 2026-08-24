/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);
const RECURSIVE_TIMER_RESPONSE_TIMEOUT: Duration = Duration::from_secs(180);
const PROCESS_EXIT_TIMEOUT: Duration = Duration::from_secs(30);
const STALL_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(10);
const INITIAL_VIRTUAL_TIME_NS: &str = "1000000000";
const VIRTUAL_TEN_SECONDS_NS: u128 = 10_000_000_000;
const EXTERNAL_IO_TIMEOUT_NS: &str = "250000000";
const EXTERNAL_IO_TIMEOUT: Duration = Duration::from_millis(250);
const CANCEL_IO_TIMEOUT_NS: &str = "30000000000";
const RELEASE_GATE_MAX_VIRTUAL_TIME_NS: &str = "30000000000";
const RELEASE_GATE_MAX_CONTROL_TURNS: &str = "100000";
const RELEASE_GATE_WALL_IO_TIMEOUT_NS: &str = "10000000000";
const RELEASE_GATE_NAME: &str = "act-settle-inspect";
const RELEASE_GATE_TEST: &str = "release_gate_published_binary_completes_act_settle_inspect";
const RELEASE_GATE_RECORD_SCHEMA: u64 = 2;
const MAX_STDERR_TAIL_BYTES: usize = 64 * 1024;
const EXPECTED_SOURCE_IDENTITIES: &str = include_str!("../../../STASIS_UPSTREAM.toml");
const EXPECTED_STASIS_REPOSITORY: &str = "https://github.com/oxhq/stasis.git";
const CONTROLLED_WEBAPP_V1_PROFILE: &str = "controlled-webapp-v1";

const STATIC_FIXTURE: &[u8] = include_bytes!("fixtures/static.html");
const TIMER_10S_FIXTURE: &[u8] = include_bytes!("fixtures/timer_10s.html");
const TIMER_MICROTASK_FIXTURE: &[u8] = include_bytes!("fixtures/timer_microtask_order.html");
const RAF_FIXTURE: &[u8] = include_bytes!("fixtures/raf_correlation.html");
const EXTERNAL_IO_FIXTURE: &[u8] = include_bytes!("fixtures/external_io.html");
const INTERVAL_FIXTURE: &[u8] = include_bytes!("fixtures/interval.html");
const APPLICATION_NAVIGATION_FIXTURE: &[u8] =
    include_bytes!("fixtures/application_navigation.html");
const UNSUPPORTED_WEBSOCKET_FIXTURE: &[u8] = include_bytes!("fixtures/unsupported_websocket.html");
const XHR_MUTATION_OBSERVER_FIXTURE: &[u8] =
    include_bytes!("fixtures/xhr_mutation_observer.html");
const AUTOMATION_SURFACE_FIXTURE: &[u8] = include_bytes!("fixtures/automation_surface.html");
const FILL_PROFILE_FIXTURE: &[u8] = include_bytes!("fixtures/fill_profile.html");
const RECURSIVE_MICROTASK_FIXTURE: &[u8] = include_bytes!("fixtures/recursive_microtask.html");
const RECURSIVE_TIMER_FIXTURE: &[u8] = include_bytes!("fixtures/recursive_timer.html");
const MUTATION_STORM_FIXTURE: &[u8] = include_bytes!("fixtures/mutation_storm.html");
const RECURSIVE_RAF_FIXTURE: &[u8] = include_bytes!("fixtures/recursive_raf.html");

static PROCESS_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// The release gate intentionally does not use development capability skips. It also requires an
/// explicit binary and archive paths so a successful run cannot accidentally certify Cargo's
/// locally-built test binary or an archive other than the packaged release artifact.
#[test]
#[ignore = "release gate: set STASIS_RELEASE_BINARY, STASIS_RELEASE_ARCHIVE, and STASIS_RELEASE_REVISION"]
fn release_gate_published_binary_completes_act_settle_inspect() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(TIMER_10S_FIXTURE, false);
    let (mut shell, binary, archive) = TestShell::spawn_release_binary();
    let capabilities = shell.initialize();
    assert_release_identity(&capabilities, &binary, &archive);
    assert_capabilities(
        &capabilities,
        &[
            "runtime.pending",
            "runtime.settle",
            "action.activate",
            "dom.text",
        ],
        true,
    );

    shell.open_controlled(server.url());
    let initial = shell.settle_with(release_gate_settle_policy());
    assert_release_gate_effective_policy(&initial);
    assert_outcome(&initial, "quiescent");
    let initial_generation = state_generation(&initial);
    let initial_virtual_time = exact_decimal(&initial, "virtualTimeNs");
    assert_eq!(shell.text("#result", &initial_generation), "idle");

    let action_generation = shell.activate("#start", &initial_generation);
    let after_action = shell.pending();
    assert_eq!(state_generation(&after_action), action_generation);
    assert_eq!(after_action["timers"]["futureFinite"], "1");
    assert_eq!(
        exact_decimal(&after_action, "virtualTimeNs"),
        initial_virtual_time,
        "act/pending advanced the published runtime's controlled clock"
    );

    let wall_started = Instant::now();
    let settled = shell.settle_with(release_gate_settle_policy());
    assert_release_gate_effective_policy(&settled);
    let wall_elapsed = wall_started.elapsed();
    assert_outcome(&settled, "quiescent");
    assert!(
        wall_elapsed < Duration::from_secs(8),
        "published runtime slept {wall_elapsed:?} for a 10s virtual timer"
    );
    assert_eq!(
        exact_decimal(&settled, "virtualTimeNs") - initial_virtual_time,
        VIRTUAL_TEN_SECONDS_NS,
        "published runtime did not advance to the exact application deadline"
    );
    let settled_generation = state_generation(&settled);
    assert_eq!(shell.text("#result", &settled_generation), "timer complete");
    assert_eq!(shell.text("#date-elapsed", &settled_generation), "10000");
    assert_eq!(
        shell.text("#performance-elapsed", &settled_generation),
        "10000"
    );
    shell.close_cleanly();
}

#[test]
fn static_page_settles_and_is_inspected_with_protocol_only_stdout() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(STATIC_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(
        &capabilities,
        &["runtime.pending", "runtime.settle", "dom.text"],
        true,
    );

    shell.open_controlled(server.url());
    let pending = shell.pending();
    assert_eq!(pending["clock"]["mode"], "controlled");
    assert_eq!(
        exact_decimal(&pending, "virtualTimeNs"),
        INITIAL_VIRTUAL_TIME_NS.parse::<u128>().unwrap()
    );
    let settled = shell.settle_default();
    assert_outcome(&settled, "quiescent");
    let observed_after_settle = shell.pending();
    assert_eq!(
        state_generation(&observed_after_settle),
        state_generation(&settled),
        "a passive observation changed quiescent stateGeneration"
    );
    assert_eq!(
        observed_after_settle["producers"]["stability"],
        "stable_empty"
    );
    assert_eq!(shell.text("#result", &state_generation(&settled)), "ready");
    shell.close_cleanly();
}

#[test]
fn semantic_fill_activate_query_and_extract_are_generation_bound() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(AUTOMATION_SURFACE_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(
        &capabilities,
        &[
            "runtime.settle",
            "action.fill",
            "action.activate",
            "dom.query",
            "dom.text",
            "dom.extract",
        ],
        true,
    );

    shell.open_controlled(server.url());
    let initial = shell.settle_default();
    assert_outcome(&initial, "quiescent");
    let initial_generation = state_generation(&initial);

    let email_generation = shell.fill("#email", "person@example.test", &initial_generation);
    assert!(
        assert_canonical_decimal(&email_generation, "action.fill.stateGeneration")
            > assert_canonical_decimal(&initial_generation, "initial.stateGeneration"),
        "semantic fill did not advance stateGeneration",
    );

    let stale = Requests::text(
        shell.next_id("stale-text"),
        shell.session_id(),
        "#status",
        &initial_generation,
    );
    let stale = expect_error(shell.call(stale));
    assert_eq!(stale["code"], "stale_generation");
    assert_eq!(stale["fatal"], false);
    assert_eq!(stale["stateEffect"], "none");

    let unsupported = Requests::fill(
        shell.next_id("unsupported-fill"),
        shell.session_id(),
        "#remember",
        "true",
        &email_generation,
    );
    let unsupported = expect_error(shell.call(unsupported));
    assert_eq!(unsupported["code"], "unsupported_fill_element");
    assert_eq!(unsupported["stateEffect"], "none");

    let password_generation = shell.fill("#password", "correct horse", &email_generation);
    assert_eq!(
        shell.text("#input-events", &password_generation),
        "email=1,password=1",
    );
    let action_generation = shell.activate("#submit", &password_generation);

    let after_submit = shell.pending();
    assert_eq!(state_generation(&after_submit), action_generation);
    assert_eq!(after_submit["microtasks"]["queued"], "0");
    assert_eq!(after_submit["timers"]["futureFinite"], "1");

    let settled = shell.settle_default();
    assert_outcome(&settled, "quiescent");
    let settled_generation = state_generation(&settled);
    assert_eq!(
        shell.text("#status", &settled_generation),
        "signed in as person@example.test",
    );
    assert_eq!(shell.query(".dashboard-card", &settled_generation), 2);

    let extracted = shell.extract(
        ".dashboard-card",
        &[
            ("title", ".card-title", "text"),
            ("body", ".card-body", "html"),
        ],
        &settled_generation,
    );
    let rows = extracted
        .as_array()
        .expect("dom.extract result rows must be an array");
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0]["fields"][0],
        json!({"name": "title", "value": "Account"})
    );
    assert_eq!(
        rows[0]["fields"][1],
        json!({"name": "body", "value": "<strong>person@example.test</strong>"}),
    );
    assert_eq!(
        rows[1]["fields"][0],
        json!({"name": "title", "value": "Status"})
    );
    assert_eq!(
        rows[1]["fields"][1],
        json!({"name": "body", "value": "<strong>Ready</strong>"}),
    );
    shell.close_cleanly();
}

#[test]
fn controlled_fill_profile_admits_every_declared_text_control() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(FILL_PROFILE_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(
        &capabilities,
        &["runtime.settle", "action.fill", "dom.text"],
        true,
    );

    shell.open_controlled(server.url());
    let settled = shell.settle_default();
    assert_outcome(&settled, "quiescent");
    let mut generation = state_generation(&settled);
    for (selector, value) in [
        ("#text", "plain"),
        ("#search", "stasis"),
        ("#url", "https://example.test/"),
        ("#tel", "+1-555-0100"),
        ("#email", "person@example.test"),
        ("#password", "correct horse"),
        ("#textarea", "two\nlines"),
    ] {
        generation = shell.fill(selector, value, &generation);
    }
    assert_eq!(
        shell.text("#events", &generation),
        "text=1,search=1,url=1,tel=1,email=1,password=1,textarea=1",
    );
    shell.close_cleanly();
}

#[test]
fn ten_second_timeout_advances_without_ten_seconds_of_wall_time() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(TIMER_10S_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(
        &capabilities,
        &[
            "runtime.pending",
            "runtime.settle",
            "action.activate",
            "dom.text",
        ],
        true,
    );

    shell.open_controlled(server.url());
    let before = shell.settle_default();
    assert_outcome(&before, "quiescent");
    let before_virtual_time = exact_decimal(&before, "virtualTimeNs");
    let generation = state_generation(&before);

    let action_generation = shell.activate("#start", &generation);
    let pending = shell.pending();
    assert_eq!(state_generation(&pending), action_generation);
    assert_eq!(pending["timers"]["futureFinite"], "1");
    assert_eq!(
        exact_decimal(&pending, "virtualTimeNs"),
        before_virtual_time,
        "a passive pending observation advanced document time"
    );
    let wall_started = Instant::now();
    let after = shell.settle_default();
    let wall_elapsed = wall_started.elapsed();
    assert_outcome(&after, "quiescent");
    assert!(
        wall_elapsed < Duration::from_secs(8),
        "a 10s virtual timer slept for {wall_elapsed:?} of wall time"
    );
    assert_eq!(
        exact_decimal(&after, "virtualTimeNs") - before_virtual_time,
        VIRTUAL_TEN_SECONDS_NS,
        "settlement did not advance exactly to the application timeout"
    );

    let generation = state_generation(&after);
    assert_eq!(
        shell.text("#result", &generation),
        "timer complete",
        "timer callback did not update the DOM"
    );
    assert_eq!(shell.text("#date-elapsed", &generation), "10000");
    assert_eq!(shell.text("#performance-elapsed", &generation), "10000");
    shell.close_cleanly();
}

#[test]
fn timer_callback_microtasks_run_before_the_next_timer_turn() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(TIMER_MICROTASK_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(
        &capabilities,
        &["runtime.settle", "action.activate", "dom.text"],
        true,
    );

    shell.open_controlled(server.url());
    let initial = shell.settle_default();
    assert_outcome(&initial, "quiescent");
    let _ = shell.activate("#start", &state_generation(&initial));

    let settled = shell.settle_default();
    assert_outcome(&settled, "quiescent");
    assert_eq!(
        shell.text("#order", &state_generation(&settled)),
        "timer-1,microtask,timer-2"
    );
    shell.close_cleanly();
}

#[test]
fn recursive_microtasks_terminate_with_the_typed_engine_limit() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(RECURSIVE_MICROTASK_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(&capabilities, &["runtime.settle", "action.activate"], true);

    shell.open_controlled(server.url());
    let initial = shell.settle_default();
    assert_outcome(&initial, "quiescent");
    let _ = shell.activate("#start", &state_generation(&initial));

    let settled = shell.settle_default();
    assert_outcome(&settled, "microtask_limit_exceeded");
    assert_eq!(settled["processed"]["microtasks"], "1000000");
    assert_eq!(settled["limit"]["kind"], "microtasks");
    assert_eq!(settled["limit"]["limit"], "1000000");
    assert_eq!(settled["limit"]["observed"], "1000001");
    let terminal_generation = state_generation(&settled);
    let rejected = Requests::activate(
        shell.next_id("activate-after-terminal"),
        shell.session_id(),
        "#start",
        &terminal_generation,
    );
    let rejected = expect_error(shell.call(rejected));
    assert_eq!(rejected["code"], "execution_terminated");
    assert_eq!(rejected["fatal"], false);
    assert_eq!(rejected["stateEffect"], "none");
    assert_eq!(
        shell.text("#status", &terminal_generation),
        "running",
        "read-only inspection must remain available after an execution terminal",
    );
    shell.close_cleanly();
}

#[test]
fn recursive_timers_terminate_with_the_typed_engine_limit() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(RECURSIVE_TIMER_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(&capabilities, &["runtime.settle", "action.activate"], true);

    shell.open_controlled(server.url());
    let initial = shell.settle_default();
    assert_outcome(&initial, "quiescent");
    let _ = shell.activate("#start", &state_generation(&initial));

    // Timer nesting can advance virtual time. Widen both caller-owned limits so the engine's
    // ordinary-task budget is the terminating authority for this fixture.
    let settled = shell.settle_with_timeout(
        json!({
            "maxVirtualTimeNs": "1000000000000",
            "maxControlTurns": "1000000",
        }),
        RECURSIVE_TIMER_RESPONSE_TIMEOUT,
    );
    assert_outcome(&settled, "task_limit_exceeded");
    assert_eq!(settled["processed"]["tasks"], "100000");
    assert_eq!(settled["limit"]["kind"], "ordinary_tasks");
    assert_eq!(settled["limit"]["limit"], "100000");
    assert_eq!(settled["limit"]["observed"], "100001");
    shell.close_cleanly();
}

#[test]
fn mutation_storm_terminates_with_non_rejecting_limit_evidence() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(MUTATION_STORM_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(&capabilities, &["runtime.settle", "action.activate"], true);

    shell.open_controlled(server.url());
    let initial = shell.settle_default();
    assert_outcome(&initial, "quiescent");
    let _ = shell.activate("#start", &state_generation(&initial));

    let settled = shell.settle_default();
    assert_outcome(&settled, "mutation_limit_exceeded");
    assert_eq!(settled["processed"]["mutations"], "1000001");
    assert_eq!(settled["limit"]["kind"], "mutations");
    assert_eq!(settled["limit"]["limit"], "1000000");
    assert_eq!(settled["limit"]["observed"], "1000001");
    shell.close_cleanly();
}

#[test]
fn recursive_animation_frames_terminate_with_the_typed_rendering_limit() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(RECURSIVE_RAF_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(&capabilities, &["runtime.settle", "action.activate"], true);

    shell.open_controlled(server.url());
    let initial = shell.settle_default();
    assert_outcome(&initial, "quiescent");
    let _ = shell.activate("#start", &state_generation(&initial));

    // This fixture intentionally runs past the default 30s virtual-time horizon so the engine's
    // rendering-opportunity budget, rather than the caller policy, is the terminating authority.
    let settled = shell.settle_with(json!({
        "maxVirtualTimeNs": "400000000000",
    }));
    assert_outcome(&settled, "rendering_limit_exceeded");
    assert_eq!(settled["processed"]["renderingOpportunities"], "10000");
    assert_eq!(settled["limit"]["kind"], "rendering_opportunities");
    assert_eq!(settled["limit"]["limit"], "10000");
    assert_eq!(settled["limit"]["observed"], "10001");
    shell.close_cleanly();
}

#[test]
fn animation_frame_timestamp_matches_the_controlled_performance_clock() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(RAF_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(
        &capabilities,
        &["runtime.settle", "action.activate", "dom.text"],
        true,
    );

    shell.open_controlled(server.url());
    let initial = shell.settle_default();
    assert_outcome(&initial, "quiescent");
    let initial_virtual_time = exact_decimal(&initial, "virtualTimeNs");
    let _ = shell.activate("#start", &state_generation(&initial));

    let settled = shell.settle_default();
    assert_outcome(&settled, "quiescent");
    let generation = state_generation(&settled);
    let raf_timestamp = parse_finite_number(&shell.text("#raf-timestamp", &generation));
    let performance_now = parse_finite_number(&shell.text("#performance-now", &generation));
    let settled_virtual_milliseconds =
        (exact_decimal(&settled, "virtualTimeNs") - initial_virtual_time) as f64 / 1_000_000.0;
    assert!(
        (raf_timestamp - performance_now).abs() < 0.001,
        "rAF timestamp {raf_timestamp} and performance.now() {performance_now} diverged"
    );

    let date_elapsed = parse_finite_number(&shell.text("#date-elapsed", &generation));
    let performance_elapsed = parse_finite_number(&shell.text("#performance-elapsed", &generation));
    assert!(
        (performance_elapsed - settled_virtual_milliseconds).abs() <= 1.0,
        "Performance elapsed {performance_elapsed}ms and settled virtual elapsed {settled_virtual_milliseconds}ms diverged"
    );
    assert!(
        (date_elapsed - performance_elapsed).abs() <= 1.0,
        "Date elapsed {date_elapsed}ms and Performance elapsed {performance_elapsed}ms diverged"
    );
    shell.close_cleanly();
}

#[test]
fn foreground_fetch_blocks_settlement_without_advancing_virtual_time() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(EXTERNAL_IO_FIXTURE, true);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(
        &capabilities,
        &["runtime.settle", "action.activate", "dom.text"],
        true,
    );

    shell.open_controlled(server.url());
    let initial = shell.settle_default();
    assert_outcome(&initial, "quiescent");
    let initial_virtual_time = exact_decimal(&initial, "virtualTimeNs");
    let _ = shell.activate("#start", &state_generation(&initial));
    server.wait_until_stalled();

    let wall_started = Instant::now();
    let settle_request_id = shell.next_id("settle");
    let settle_request = Requests::settle(
        settle_request_id,
        shell.session_id(),
        json!({
            "persistentWork": "report",
            "wallIoTimeoutNs": EXTERNAL_IO_TIMEOUT_NS,
        }),
    );
    let request_id = shell.send(settle_request);
    let blocked = expect_result(shell.wait_for_response(&request_id));
    let wall_elapsed = wall_started.elapsed();

    assert_outcome(&blocked, "blocked_on_external_io");
    assert!(
        wall_elapsed >= EXTERNAL_IO_TIMEOUT,
        "external-I/O settlement returned before its wall budget elapsed: {wall_elapsed:?}"
    );
    assert!(
        wall_elapsed < Duration::from_secs(5),
        "external-I/O policy did not finish within its bounded wall budget: {wall_elapsed:?}"
    );
    assert_eq!(
        exact_decimal(&blocked, "virtualTimeNs"),
        initial_virtual_time,
        "virtual time advanced while foreground I/O was unresolved"
    );
    assert!(
        exact_decimal(&blocked, "wallTimeNs") >= EXTERNAL_IO_TIMEOUT.as_nanos(),
        "blocked result underreported its external-I/O wall wait"
    );
    assert_eq!(
        blocked["snapshot"]["virtualTimeNs"], blocked["virtualTimeNs"],
        "top-level and snapshot virtual time disagree"
    );
    assert_external_fetch_coverage(&blocked);

    server.release_stall();
    let completed = shell.settle_default();
    assert_outcome(&completed, "quiescent");
    assert_no_active_network(&completed);
    assert_eq!(
        shell.text("#status", &state_generation(&completed)),
        "done",
        "fetch Promise continuation did not update the DOM"
    );
    shell.close_cleanly();
}

#[test]
fn interval_head_blocks_deferred_finite_work_without_executing_either() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(INTERVAL_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(
        &capabilities,
        &["runtime.settle", "action.activate", "dom.text"],
        true,
    );

    shell.open_controlled(server.url());
    let initial = shell.settle_default();
    assert_outcome(&initial, "quiescent");
    let initial_virtual_time = exact_decimal(&initial, "virtualTimeNs");
    let _ = shell.activate("#start", &state_generation(&initial));

    let settled = shell.settle_with(json!({
        "persistentWork": "report",
    }));
    assert_outcome(&settled, "blocked_on_open_ended_work");
    // Activation may advance the microtask checkpoint, requiring one zero-time producer-proof
    // turn. It must never require a timer callback turn before classifying the interval head.
    let control_turns = settled["processed"]["controlTurns"]
        .as_str()
        .map(|value| assert_canonical_decimal(value, "processed.controlTurns"))
        .expect("processed.controlTurns must be an exact decimal string");
    assert!(
        control_turns <= 1,
        "settlement required more than one proof checkpoint before reporting the interval head: {control_turns}"
    );
    assert_eq!(
        exact_decimal(&settled, "virtualTimeNs"),
        initial_virtual_time,
        "settlement advanced virtual time before reporting the interval head"
    );

    let persistent = settled["persistentWork"]
        .as_array()
        .expect("settlement persistentWork must be an array");
    let interval = persistent
        .iter()
        .find(|work| {
            work["kind"] == "timer"
                && work["reason"] == "interval"
                && work["requestedPeriodNs"] == "5000000000"
        })
        .unwrap_or_else(|| {
            panic!("the 5s interval was not returned as persistent work: {persistent:?}")
        });
    assert_eq!(interval["count"], "1");
    let source_id = interval["sourceId"]
        .as_str()
        .expect("persistent interval must retain an opaque sourceId");
    assert_canonical_decimal(source_id, "persistentWork.sourceId");

    assert_eq!(
        settled["snapshot"]["timers"]["persistent"], "1",
        "the interval disappeared from the terminal pending snapshot"
    );
    assert_eq!(
        settled["snapshot"]["timers"]["futureFinite"], "1",
        "the finite timeout behind the interval head was not preserved as deferred work"
    );

    assert_eq!(
        shell.text("#heartbeat-count", &state_generation(&settled)),
        "0",
        "settlement executed an interval cycle while classifying persistent work"
    );
    assert_eq!(
        shell.text("#deferred-count", &state_generation(&settled)),
        "0",
        "settlement skipped the interval head and executed finite work behind it"
    );
    shell.close_cleanly();
}

#[test]
fn websocket_is_rejected_before_dispatch_and_settlement_reports_unsupported_work() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(UNSUPPORTED_WEBSOCKET_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(
        &capabilities,
        &["runtime.settle", "action.activate", "dom.text"],
        true,
    );

    shell.open_controlled(server.url());
    let initial = shell.settle_default();
    assert_outcome(&initial, "quiescent");
    let action_generation = shell.activate("#start", &state_generation(&initial));
    assert_eq!(
        shell.text("#result", &action_generation),
        "NotSupportedError",
        "the controlled profile did not reject WebSocket before native dispatch",
    );

    let settled = shell.settle_default();
    assert_outcome(&settled, "unsupported_work");
    assert_eq!(settled["failure"]["code"], "unsupported_clock_surface");
    let unsupported = settled["unsupportedWork"]
        .as_array()
        .expect("unsupported settlement must include bounded evidence");
    assert_eq!(unsupported.len(), 1);
    assert_eq!(unsupported[0]["kind"], "other");
    assert_eq!(unsupported[0]["count"], "1");
    assert_eq!(unsupported[0]["reason"], "time_surface");
    assert_eq!(unsupported[0]["timeSurface"], "external_subscription");
    shell.close_cleanly();
}

#[test]
fn asynchronous_xhr_and_mutation_observer_reach_quiescence() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(XHR_MUTATION_OBSERVER_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(
        &capabilities,
        &["runtime.settle", "action.activate", "dom.text"],
        true,
    );

    shell.open_controlled(server.url());
    let initial = shell.settle_default();
    assert_outcome(&initial, "quiescent");
    shell.activate("#async", &state_generation(&initial));

    let settled = shell.settle_default();
    assert_outcome(&settled, "quiescent");
    let generation = state_generation(&settled);
    assert_eq!(shell.text("#result", &generation), "xhr complete");
    assert_eq!(shell.text("#observed", &generation), "xhr complete");
    shell.close_cleanly();
}

#[test]
fn synchronous_xhr_is_rejected_before_start_without_poisoning_settlement() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(XHR_MUTATION_OBSERVER_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(
        &capabilities,
        &["runtime.settle", "action.activate", "dom.text"],
        true,
    );

    shell.open_controlled(server.url());
    let initial = shell.settle_default();
    assert_outcome(&initial, "quiescent");
    let generation = shell.activate("#sync", &state_generation(&initial));
    assert_eq!(shell.text("#result", &generation), "InvalidAccessError");

    let settled = shell.settle_default();
    assert_outcome(&settled, "quiescent");
    shell.close_cleanly();
}

#[test]
fn application_top_level_navigation_is_rejected_as_typed_unsupported_work() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(APPLICATION_NAVIGATION_FIXTURE, false);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(
        &capabilities,
        &["runtime.settle", "action.activate", "dom.text"],
        true,
    );

    shell.open_controlled(server.url());
    let initial = shell.settle_default();
    assert_outcome(&initial, "quiescent");
    assert_eq!(
        shell.text("#result", &state_generation(&initial)),
        "original"
    );

    shell.activate("#navigate", &state_generation(&initial));
    let settled = shell.settle_default();
    assert_outcome(&settled, "unsupported_work");
    assert_eq!(settled["failure"]["code"], "unsupported_clock_surface");
    let unsupported = settled["unsupportedWork"]
        .as_array()
        .expect("unsupported settlement must include bounded evidence");
    assert_eq!(unsupported.len(), 1);
    assert_eq!(unsupported[0]["kind"], "other");
    assert_eq!(unsupported[0]["count"], "1");
    assert_eq!(unsupported[0]["reason"], "time_surface");
    assert_eq!(
        unsupported[0]["timeSurface"],
        "cross_event_loop_navigation"
    );
    assert_eq!(
        shell.text("#result", &state_generation(&settled)),
        "original",
        "unsupported application navigation replaced the controlled document",
    );
    shell.close_cleanly();
}

#[test]
fn cancellation_interrupts_an_external_io_wait_and_keeps_the_session_usable() {
    let _serial = process_test_guard();
    let server = FixtureServer::start(EXTERNAL_IO_FIXTURE, true);
    let mut shell = TestShell::spawn();
    let capabilities = shell.initialize();
    assert_capabilities(
        &capabilities,
        &[
            "runtime.pending",
            "runtime.settle",
            "action.activate",
            "protocol.cancel",
            "dom.text",
        ],
        true,
    );

    shell.open_controlled(server.url());
    let initial = shell.settle_default();
    assert_outcome(&initial, "quiescent");
    let _ = shell.activate("#start", &state_generation(&initial));
    server.wait_until_stalled();

    let settle_request_id = shell.next_id("settle");
    let settle_request = Requests::settle(
        settle_request_id,
        shell.session_id(),
        json!({
            "persistentWork": "report",
            "wallIoTimeoutNs": CANCEL_IO_TIMEOUT_NS,
        }),
    );
    let settle_id = shell.send(settle_request);
    let probe_request = Requests::pending(shell.next_id("busy-probe"), shell.session_id());
    let probe_id = shell.send(probe_request);
    let busy = expect_error(shell.wait_for_response(&probe_id));
    assert_eq!(busy["code"], "busy");
    assert_eq!(busy["stateEffect"], "none");

    let cancel_request_id = shell.next_id("cancel");
    let cancel_request = Requests::cancel(cancel_request_id, shell.session_id(), &settle_id);
    let cancel_id = shell.send(cancel_request);
    let cancelled_ack = shell.wait_for_response(&cancel_id);
    let active_terminal = shell.wait_for_response(&settle_id);
    assert!(
        wire_sequence(&cancelled_ack) < wire_sequence(&active_terminal),
        "cancel acknowledgement must precede the active request's terminal response"
    );
    assert_eq!(expect_result(cancelled_ack)["accepted"], true);
    let cancellation = expect_error(active_terminal);
    assert_eq!(cancellation["code"], "cancelled");
    assert_eq!(cancellation["stateEffect"], "none");

    server.release_stall();
    let completed = shell.settle_default();
    assert_outcome(&completed, "quiescent");
    assert_no_active_network(&completed);
    assert_eq!(
        shell.text("#status", &state_generation(&completed)),
        "done",
        "session did not run the fetch continuation after cancellation recovery"
    );
    shell.close_cleanly();
}

fn process_test_guard() -> MutexGuard<'static, ()> {
    PROCESS_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn assert_capabilities(capabilities: &Capabilities, methods: &[&str], controlled_clock: bool) {
    let missing_methods = ["session.open", "session.close"]
        .into_iter()
        .chain(methods.iter().copied())
        .filter(|method| !capabilities.supports_method(method))
        .collect::<Vec<_>>();
    let missing_clock = controlled_clock && !capabilities.supports_clock("controlled");
    let missing_profile =
        controlled_clock && !capabilities.supports_profile(CONTROLLED_WEBAPP_V1_PROFILE);
    assert!(
        missing_methods.is_empty() && !missing_clock && !missing_profile,
        "release artifact is missing mandatory v0.1 capabilities: methods={missing_methods:?}, controlledClock={}, controlledProfile={}",
        !missing_clock,
        !missing_profile,
    );
}

fn release_gate_settle_policy() -> Value {
    json!({
        "persistentWork": "report",
        "maxVirtualTimeNs": RELEASE_GATE_MAX_VIRTUAL_TIME_NS,
        "maxControlTurns": RELEASE_GATE_MAX_CONTROL_TURNS,
        "wallIoTimeoutNs": RELEASE_GATE_WALL_IO_TIMEOUT_NS,
    })
}

fn assert_release_gate_effective_policy(settled: &Value) {
    assert_eq!(
        settled["effectivePolicy"],
        release_gate_settle_policy(),
        "release gate runtime did not honor the explicit settlement limits"
    );
}

fn assert_release_identity(capabilities: &Capabilities, binary: &Path, archive: &Path) {
    assert_eq!(capabilities.implementation_name, "stasis-shell");
    assert_eq!(
        capabilities.implementation_version,
        env!("CARGO_PKG_VERSION"),
        "release artifact version does not match the fixture source version"
    );
    let expected_revision = env::var("STASIS_RELEASE_REVISION")
        .expect("STASIS_RELEASE_REVISION must bind the gate to the full release commit");
    assert_git_commit(&expected_revision, "STASIS_RELEASE_REVISION");

    let mut expected_sources = parse_expected_source_identities();
    expected_sources.insert(
        "stasis_repository".to_owned(),
        EXPECTED_STASIS_REPOSITORY.to_owned(),
    );
    expected_sources.insert("stasis_revision".to_owned(), expected_revision.clone());
    assert_eq!(
        expected_sources.len(),
        7,
        "the release gate must bind exactly five upstream identities and two Stasis identities"
    );
    assert_eq!(
        capabilities.source_identities, expected_sources,
        "release artifact implementation.source must be the exact seven-key release identity map"
    );

    let binary_sha256 = release_artifact_sha256(binary);
    if let Some(expected) = env::var_os("STASIS_RELEASE_SHA256") {
        let expected = expected
            .into_string()
            .expect("STASIS_RELEASE_SHA256 must be valid UTF-8");
        assert_sha256(&expected, "STASIS_RELEASE_SHA256");
        assert_eq!(
            binary_sha256, expected,
            "release artifact SHA-256 does not match STASIS_RELEASE_SHA256"
        );
    }

    let archive_sha256 = release_artifact_sha256(archive);
    let archive_name = archive
        .file_name()
        .and_then(|name| name.to_str())
        .expect("STASIS_RELEASE_ARCHIVE must have a UTF-8 file name");
    let record = json!({
        "schema": RELEASE_GATE_RECORD_SCHEMA,
        "gate": RELEASE_GATE_NAME,
        "test": RELEASE_GATE_TEST,
        "version": capabilities.implementation_version,
        "archive": {
            "name": archive_name,
            "sha256": archive_sha256,
        },
        "binary": {
            "path": binary.display().to_string(),
            "sha256": binary_sha256,
        },
        "source": &capabilities.source_identities,
    });
    eprintln!(
        "[RELEASE ARTIFACT] {}",
        serde_json::to_string(&record).expect("release artifact record must encode as JSON")
    );
}

fn parse_expected_source_identities() -> BTreeMap<String, String> {
    EXPECTED_SOURCE_IDENTITIES
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(key, value)| {
            (
                key.trim().to_owned(),
                value.trim().trim_matches('"').to_owned(),
            )
        })
        .collect()
}

fn release_artifact_sha256(artifact: &Path) -> String {
    for (program, arguments) in [
        ("sha256sum", &[][..]),
        ("shasum", &["-a", "256"][..]),
        ("openssl", &["dgst", "-sha256"][..]),
    ] {
        let Ok(output) = Command::new(program).args(arguments).arg(artifact).output() else {
            continue;
        };
        if !output.status.success() {
            continue;
        }
        let stdout = String::from_utf8(output.stdout)
            .unwrap_or_else(|error| panic!("{program} emitted non-UTF-8 output: {error}"));
        if let Some(digest) = stdout
            .split_ascii_whitespace()
            .find(|field| field.len() == 64 && field.bytes().all(|byte| byte.is_ascii_hexdigit()))
        {
            let digest = digest.to_ascii_lowercase();
            assert_sha256(&digest, program);
            return digest;
        }
    }
    panic!(
        "could not calculate SHA-256 for {}; install sha256sum, shasum, or openssl",
        artifact.display()
    )
}

fn assert_sha256(digest: &str, field: &str) {
    assert!(
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{field} is not a canonical lowercase SHA-256 digest: {digest:?}"
    );
}

fn assert_git_commit(commit: &str, field: &str) {
    assert!(
        commit.len() == 40
            && commit
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "{field} is not a canonical lowercase full Git commit: {commit:?}"
    );
}

#[derive(Debug)]
struct Capabilities {
    methods: BTreeSet<String>,
    clock_modes: BTreeSet<String>,
    profiles: BTreeSet<String>,
    implementation_name: String,
    implementation_version: String,
    source_identities: BTreeMap<String, String>,
}

impl Capabilities {
    fn decode(result: &Value) -> Self {
        let capabilities = result
            .get("capabilities")
            .and_then(Value::as_object)
            .expect("initialize result is missing capabilities");
        let methods = capabilities
            .get("methods")
            .and_then(Value::as_array)
            .expect("capabilities.methods must be an array")
            .iter()
            .map(|method| {
                method
                    .as_str()
                    .expect("capability method must be a string")
                    .to_owned()
            })
            .collect();
        let clock_modes = capabilities
            .get("clockModes")
            .and_then(Value::as_array)
            .expect("capabilities.clockModes must be an array")
            .iter()
            .map(|mode| {
                mode.as_str()
                    .expect("clock mode must be a string")
                    .to_owned()
            })
            .collect();
        let profiles = capabilities
            .get("profiles")
            .and_then(Value::as_array)
            .expect("capabilities.profiles must be an array")
            .iter()
            .map(|profile| {
                profile
                    .as_str()
                    .expect("capability profile must be a string")
                    .to_owned()
            })
            .collect();
        let implementation = result
            .get("implementation")
            .and_then(Value::as_object)
            .expect("initialize result is missing implementation metadata");
        let implementation_name = implementation
            .get("name")
            .and_then(Value::as_str)
            .expect("implementation.name must be a string")
            .to_owned();
        let implementation_version = implementation
            .get("version")
            .and_then(Value::as_str)
            .expect("implementation.version must be a string")
            .to_owned();
        let source_identities = implementation
            .get("source")
            .and_then(Value::as_object)
            .expect("implementation.source must be an object")
            .iter()
            .map(|(key, value)| {
                (
                    key.clone(),
                    value
                        .as_str()
                        .unwrap_or_else(|| panic!("implementation.source.{key} must be a string"))
                        .to_owned(),
                )
            })
            .collect();
        Self {
            methods,
            clock_modes,
            profiles,
            implementation_name,
            implementation_version,
            source_identities,
        }
    }

    fn supports_method(&self, method: &str) -> bool {
        self.methods.contains(method)
    }

    fn supports_clock(&self, mode: &str) -> bool {
        self.clock_modes.contains(mode)
    }

    fn supports_profile(&self, profile: &str) -> bool {
        self.profiles.contains(profile)
    }
}

/// The only place in this suite which constructs public requests. Keeping all evolving wire
/// names here makes an intentional protocol change a one-site review rather than a fixture hunt.
struct Requests;

impl Requests {
    fn initialize(id: String) -> Value {
        Self::envelope(
            id,
            None,
            "protocol.initialize",
            json!({
                "client": {
                    "name": "controlled-mvp-test",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }),
        )
    }

    fn open_controlled(id: String, url: &str) -> Value {
        Self::envelope(
            id,
            None,
            "session.open",
            json!({
                "url": url,
                "clockMode": "controlled",
                "profile": CONTROLLED_WEBAPP_V1_PROFILE,
                "initialVirtualTimeNs": INITIAL_VIRTUAL_TIME_NS,
                "unixTimeOriginNs": "0",
            }),
        )
    }

    fn settle(id: String, session_id: &str, params: Value) -> Value {
        Self::envelope(id, Some(session_id), "runtime.settle", params)
    }

    fn pending(id: String, session_id: &str) -> Value {
        Self::envelope(id, Some(session_id), "runtime.pending", json!({}))
    }

    fn activate(id: String, session_id: &str, selector: &str, generation: &str) -> Value {
        Self::envelope(
            id,
            Some(session_id),
            "action.activate",
            json!({"selector": selector, "expectedGeneration": generation}),
        )
    }

    fn fill(id: String, session_id: &str, selector: &str, value: &str, generation: &str) -> Value {
        Self::envelope(
            id,
            Some(session_id),
            "action.fill",
            json!({
                "selector": selector,
                "value": value,
                "expectedGeneration": generation,
            }),
        )
    }

    fn query(id: String, session_id: &str, selector: &str, generation: &str) -> Value {
        Self::envelope(
            id,
            Some(session_id),
            "dom.query",
            json!({"selector": selector, "expectedGeneration": generation}),
        )
    }

    fn text(id: String, session_id: &str, selector: &str, generation: &str) -> Value {
        Self::envelope(
            id,
            Some(session_id),
            "dom.text",
            json!({"selector": selector, "expectedGeneration": generation}),
        )
    }

    fn extract(
        id: String,
        session_id: &str,
        root_selector: &str,
        fields: &[(&str, &str, &str)],
        generation: &str,
    ) -> Value {
        let fields: Vec<_> = fields
            .iter()
            .map(|(name, selector, read)| json!({"name": name, "selector": selector, "read": read}))
            .collect();
        Self::envelope(
            id,
            Some(session_id),
            "dom.extract",
            json!({
                "rootSelector": root_selector,
                "fields": fields,
                "expectedGeneration": generation,
            }),
        )
    }

    fn cancel(id: String, session_id: &str, request_id: &str) -> Value {
        Self::envelope(
            id,
            Some(session_id),
            "protocol.cancel",
            json!({"requestId": request_id}),
        )
    }

    fn close(id: String, session_id: &str) -> Value {
        Self::envelope(id, Some(session_id), "session.close", json!({}))
    }

    fn envelope(
        id: String,
        session_id: Option<&str>,
        method: &'static str,
        params: Value,
    ) -> Value {
        let mut request = json!({
            "v": 1,
            "type": "request",
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(session_id) = session_id {
            request["sessionId"] = Value::String(session_id.to_owned());
        }
        request
    }
}

enum OutputRead {
    Line(String),
    Error(String),
    Eof,
}

struct TestShell {
    child: Option<Child>,
    input: Option<ChildStdin>,
    output: Receiver<OutputRead>,
    stderr_tail: Receiver<Vec<u8>>,
    next_request_id: u64,
    session_id: Option<String>,
    last_wire_sequence: u128,
    outstanding_requests: BTreeSet<String>,
    response_backlog: BTreeMap<String, Value>,
    events: Vec<Value>,
    exited: bool,
}

impl TestShell {
    fn spawn() -> Self {
        Self::spawn_path(PathBuf::from(env!("CARGO_BIN_EXE_stasis")))
    }

    fn spawn_release_binary() -> (Self, PathBuf, PathBuf) {
        let binary = env::var_os("STASIS_RELEASE_BINARY")
            .map(PathBuf::from)
            .expect("STASIS_RELEASE_BINARY must name the extracted/installed release binary");
        assert!(
            binary.is_file(),
            "STASIS_RELEASE_BINARY is not a file: {}",
            binary.display()
        );
        let binary = binary.canonicalize().unwrap_or_else(|error| {
            panic!(
                "failed to resolve STASIS_RELEASE_BINARY {}: {error}",
                binary.display()
            )
        });
        let archive = env::var_os("STASIS_RELEASE_ARCHIVE")
            .map(PathBuf::from)
            .expect("STASIS_RELEASE_ARCHIVE must name the packaged release archive");
        assert!(
            archive.is_file(),
            "STASIS_RELEASE_ARCHIVE is not a file: {}",
            archive.display()
        );
        let archive = archive.canonicalize().unwrap_or_else(|error| {
            panic!(
                "failed to resolve STASIS_RELEASE_ARCHIVE {}: {error}",
                archive.display()
            )
        });
        (Self::spawn_path(binary.clone()), binary, archive)
    }

    fn spawn_path(binary: PathBuf) -> Self {
        let mut child = Command::new(&binary)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|error| {
                panic!(
                    "failed to spawn the stasis binary {}: {error}",
                    binary.display()
                )
            });
        let input = child.stdin.take().expect("stasis child has no stdin");
        let output = spawn_output_reader(child.stdout.take().expect("stasis child has no stdout"));
        let stderr_tail =
            spawn_stderr_reader(child.stderr.take().expect("stasis child has no stderr"));
        Self {
            child: Some(child),
            input: Some(input),
            output,
            stderr_tail,
            next_request_id: 0,
            session_id: None,
            last_wire_sequence: 0,
            outstanding_requests: BTreeSet::new(),
            response_backlog: BTreeMap::new(),
            events: Vec::new(),
            exited: false,
        }
    }

    fn initialize(&mut self) -> Capabilities {
        let id = self.next_id("initialize");
        let response = self.call(Requests::initialize(id));
        Capabilities::decode(&expect_result(response))
    }

    fn open_controlled(&mut self, url: &str) -> Value {
        let id = self.next_id("open");
        let response = self.call(Requests::open_controlled(id, url));
        let envelope_session_id = response
            .get("sessionId")
            .and_then(Value::as_str)
            .expect("successful session.open response must carry sessionId")
            .to_owned();
        let result = expect_result(response);
        let session_id = result["sessionId"]
            .as_str()
            .expect("successful session.open result must carry sessionId")
            .to_owned();
        assert_eq!(envelope_session_id, session_id);
        assert_eq!(result["clockMode"], "controlled");
        assert_eq!(result["boundary"], "controlled_ready");
        assert_eq!(result["profile"], CONTROLLED_WEBAPP_V1_PROFILE);
        self.session_id = Some(session_id);
        result
    }

    fn settle_default(&mut self) -> Value {
        self.settle_with(json!({}))
    }

    fn pending(&mut self) -> Value {
        let id = self.next_id("pending");
        let request = Requests::pending(id, self.session_id());
        expect_result(self.call(request))
    }

    fn settle_with(&mut self, params: Value) -> Value {
        let request = Requests::settle(self.next_id("settle"), self.session_id(), params);
        expect_result(self.call(request))
    }

    fn settle_with_timeout(&mut self, params: Value, timeout: Duration) -> Value {
        let request = Requests::settle(self.next_id("settle"), self.session_id(), params);
        let id = self.send(request);
        expect_result(self.wait_for_response_with_timeout(&id, timeout))
    }

    fn activate(&mut self, selector: &str, generation: &str) -> String {
        let request = Requests::activate(
            self.next_id("activate"),
            self.session_id(),
            selector,
            generation,
        );
        let result = expect_result(self.call(request));
        assert_object_keys(&result, &["stateGeneration"], "action.activate result");
        let observed = state_generation(&result);
        assert!(
            assert_canonical_decimal(&observed, "action.activate.stateGeneration")
                > assert_canonical_decimal(generation, "action.activate.expectedGeneration"),
            "fixture activation did not advance stateGeneration"
        );
        observed
    }

    fn fill(&mut self, selector: &str, value: &str, generation: &str) -> String {
        let request = Requests::fill(
            self.next_id("fill"),
            self.session_id(),
            selector,
            value,
            generation,
        );
        let result = expect_result(self.call(request));
        assert_object_keys(&result, &["stateGeneration"], "action.fill result");
        state_generation(&result)
    }

    fn query(&mut self, selector: &str, generation: &str) -> u128 {
        let request = Requests::query(
            self.next_id("query"),
            self.session_id(),
            selector,
            generation,
        );
        let result = expect_result(self.call(request));
        assert_object_keys(&result, &["count", "stateGeneration"], "dom.query result");
        assert_eq!(
            state_generation(&result),
            generation,
            "passive dom.query changed stateGeneration",
        );
        exact_decimal(&result, "count")
    }

    fn text(&mut self, selector: &str, generation: &str) -> String {
        let request = Requests::text(
            self.next_id("text"),
            self.session_id(),
            selector,
            generation,
        );
        let result = expect_result(self.call(request));
        assert_object_keys(&result, &["stateGeneration", "value"], "dom.text result");
        assert_eq!(
            state_generation(&result),
            generation,
            "passive dom.text changed stateGeneration"
        );
        result
            .get("value")
            .and_then(Value::as_str)
            .expect("dom.text result.value must be a string")
            .to_owned()
    }

    fn extract(
        &mut self,
        root_selector: &str,
        fields: &[(&str, &str, &str)],
        generation: &str,
    ) -> Value {
        let request = Requests::extract(
            self.next_id("extract"),
            self.session_id(),
            root_selector,
            fields,
            generation,
        );
        let result = expect_result(self.call(request));
        assert_object_keys(&result, &["rows", "stateGeneration"], "dom.extract result");
        assert_eq!(
            state_generation(&result),
            generation,
            "passive dom.extract changed stateGeneration",
        );
        result["rows"].clone()
    }

    fn close_cleanly(&mut self) {
        let id = self.next_id("close");
        let request = Requests::close(id, self.session_id());
        let response = self.call(request);
        assert_eq!(expect_result(response)["state"], "closed");
        assert!(
            self.outstanding_requests.is_empty() && self.response_backlog.is_empty(),
            "session closed with unconsumed protocol responses"
        );
        self.input.take();
        self.wait_for_process_exit(true);
        self.expect_protocol_eof();
        self.exited = true;
    }

    fn call(&mut self, request: Value) -> Value {
        let id = self.send(request);
        self.wait_for_response(&id)
    }

    fn send(&mut self, request: Value) -> String {
        let id = request
            .get("id")
            .and_then(Value::as_str)
            .expect("outgoing request has no ID")
            .to_owned();
        assert!(
            self.outstanding_requests.insert(id.clone()),
            "duplicate outgoing request ID {id}"
        );
        let input = self.input.as_mut().expect("stasis stdin is closed");
        serde_json::to_writer(&mut *input, &request).expect("failed to encode protocol request");
        input
            .write_all(b"\n")
            .expect("failed to frame protocol request");
        input.flush().expect("failed to flush protocol request");
        id
    }

    fn wait_for_response(&mut self, request_id: &str) -> Value {
        self.wait_for_response_with_timeout(request_id, RESPONSE_TIMEOUT)
    }

    fn wait_for_response_with_timeout(&mut self, request_id: &str, timeout: Duration) -> Value {
        if let Some(response) = self.response_backlog.remove(request_id) {
            return response;
        }
        assert!(
            self.outstanding_requests.contains(request_id),
            "cannot await unknown or already-consumed request {request_id}"
        );
        let deadline = Instant::now()
            .checked_add(timeout)
            .expect("response deadline overflowed");
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(
                !remaining.is_zero(),
                "timed out waiting for terminal response to request {request_id}"
            );
            let frame = self.receive_frame(remaining);
            match frame["type"].as_str() {
                Some("event") => self.events.push(frame),
                Some("response") => {
                    let id = frame["id"]
                        .as_str()
                        .expect("response ID must be a string")
                        .to_owned();
                    assert!(
                        self.outstanding_requests.remove(&id),
                        "unexpected or duplicate terminal response for request {id}"
                    );
                    if id == request_id {
                        return frame;
                    }
                    assert!(
                        self.response_backlog.insert(id.clone(), frame).is_none(),
                        "duplicate terminal response for request {id}"
                    );
                },
                other => panic!("unexpected protocol frame type {other:?}"),
            }
        }
    }

    fn receive_frame(&mut self, timeout: Duration) -> Value {
        let line = match self.output.recv_timeout(timeout) {
            Ok(OutputRead::Line(line)) => line,
            Ok(OutputRead::Error(error)) => panic!("failed to read protocol stdout: {error}"),
            Ok(OutputRead::Eof) => panic!("stasis stdout reached EOF before a response"),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                panic!("timed out waiting for stasis protocol output")
            },
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("stasis protocol output reader disconnected")
            },
        };
        assert!(!line.is_empty(), "protocol stdout contained a blank line");
        let frame: Value = serde_json::from_str(&line)
            .unwrap_or_else(|error| panic!("protocol stdout was not NDJSON: {error}: {line:?}"));
        assert_eq!(frame["v"], 1, "unexpected protocol version: {frame}");
        assert!(
            frame.is_object(),
            "protocol frame must be an object: {frame}"
        );
        let wire_sequence = wire_sequence(&frame);
        let expected = self
            .last_wire_sequence
            .checked_add(1)
            .expect("wireSeq exhausted u128");
        assert_eq!(
            wire_sequence, expected,
            "wireSeq was not contiguous after {}",
            self.last_wire_sequence
        );
        self.last_wire_sequence = wire_sequence;
        frame
    }

    fn next_id(&mut self, prefix: &str) -> String {
        self.next_request_id = self
            .next_request_id
            .checked_add(1)
            .expect("test request ID exhausted");
        format!("{prefix}-{}", self.next_request_id)
    }

    fn session_id(&self) -> &str {
        self.session_id
            .as_deref()
            .expect("test has not opened a session")
    }

    fn wait_for_process_exit(&mut self, require_success: bool) -> ExitStatus {
        let child = self.child.as_mut().expect("stasis child is missing");
        let deadline = Instant::now() + PROCESS_EXIT_TIMEOUT;
        let status = loop {
            match child.try_wait().expect("failed to query stasis process") {
                Some(status) => break status,
                None if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(10));
                },
                None => {
                    child.kill().ok();
                    let _ = child.wait();
                    panic!("stasis did not exit after its terminal input")
                },
            }
        };
        if require_success && !status.success() {
            panic!(
                "stasis exited unsuccessfully with {status}; stderr tail: {}",
                self.read_stderr_tail()
            );
        }
        status
    }

    fn expect_protocol_eof(&mut self) {
        match self.output.recv_timeout(RESPONSE_TIMEOUT) {
            Ok(OutputRead::Eof) => {},
            Ok(OutputRead::Line(line)) => {
                let _: Value = serde_json::from_str(&line).unwrap_or_else(|error| {
                    panic!("stdout after terminal response was not NDJSON: {error}: {line:?}")
                });
                panic!("session.close response was not the final protocol frame: {line}");
            },
            Ok(OutputRead::Error(error)) => panic!("failed to finish protocol stdout: {error}"),
            Err(error) => panic!("protocol stdout did not close cleanly: {error}"),
        }
    }

    fn read_stderr_tail(&mut self) -> String {
        match self.stderr_tail.recv_timeout(Duration::from_secs(1)) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(_) => "<stderr unavailable>".to_owned(),
        }
    }
}

impl Drop for TestShell {
    fn drop(&mut self) {
        if self.exited {
            return;
        }
        self.input.take();
        let Some(child) = self.child.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(Some(_)) => {},
            Ok(None) => {
                child.kill().ok();
                child.wait().ok();
            },
            Err(_) => {},
        }
    }
}

fn spawn_output_reader(output: impl Read + Send + 'static) -> Receiver<OutputRead> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut output = BufReader::new(output);
        loop {
            let mut frame = Vec::new();
            let message = match output.read_until(b'\n', &mut frame) {
                Ok(0) => {
                    sender.send(OutputRead::Eof).ok();
                    return;
                },
                Ok(_) if frame.last() != Some(&b'\n') => {
                    OutputRead::Error("protocol stdout ended with an unterminated frame".into())
                },
                Ok(_) => {
                    frame.pop();
                    if frame.last() == Some(&b'\r') {
                        OutputRead::Error(
                            "protocol stdout used CRLF instead of canonical LF framing".into(),
                        )
                    } else {
                        match String::from_utf8(frame) {
                            Ok(line) => OutputRead::Line(line),
                            Err(error) => OutputRead::Error(format!(
                                "protocol stdout frame was not valid UTF-8: {error}"
                            )),
                        }
                    }
                },
                Err(error) => OutputRead::Error(error.to_string()),
            };
            let terminal = matches!(message, OutputRead::Error(_));
            if sender.send(message).is_err() || terminal {
                return;
            }
        }
    });
    receiver
}

fn spawn_stderr_reader(mut stderr: impl Read + Send + 'static) -> Receiver<Vec<u8>> {
    let (sender, receiver) = sync_channel(1);
    thread::spawn(move || {
        let mut tail = Vec::new();
        let mut chunk = [0_u8; 8192];
        loop {
            match stderr.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    tail.extend_from_slice(&chunk[..read]);
                    if tail.len() > MAX_STDERR_TAIL_BYTES {
                        let excess = tail.len() - MAX_STDERR_TAIL_BYTES;
                        tail.drain(..excess);
                    }
                },
                Err(error) => {
                    tail.extend_from_slice(format!("\n<stderr read failed: {error}>").as_bytes());
                    break;
                },
            }
        }
        sender.send(tail).ok();
    });
    receiver
}

fn expect_result(response: Value) -> Value {
    if let Some(error) = response.get("error") {
        panic!("protocol request failed: {error}");
    }
    response
        .get("result")
        .cloned()
        .expect("response has neither result nor error")
}

fn expect_error(response: Value) -> Value {
    if let Some(result) = response.get("result") {
        panic!("protocol request unexpectedly succeeded: {result}");
    }
    response
        .get("error")
        .cloned()
        .expect("response has neither result nor error")
}

fn wire_sequence(frame: &Value) -> u128 {
    let encoded = frame["wireSeq"]
        .as_str()
        .expect("wireSeq must be an exact decimal string");
    assert_canonical_decimal(encoded, "wireSeq")
}

fn state_generation(result: &Value) -> String {
    let generation = result["stateGeneration"]
        .as_str()
        .expect("result stateGeneration must be an exact decimal string");
    assert_canonical_decimal(generation, "stateGeneration");
    generation.to_owned()
}

fn exact_decimal(result: &Value, field: &str) -> u128 {
    let encoded = result[field]
        .as_str()
        .unwrap_or_else(|| panic!("result {field} must be an exact decimal string"));
    assert_canonical_decimal(encoded, field)
}

fn assert_canonical_decimal(encoded: &str, field: &str) -> u128 {
    assert!(
        !encoded.is_empty()
            && !(encoded.len() > 1 && encoded.starts_with('0'))
            && encoded.bytes().all(|byte| byte.is_ascii_digit()),
        "{field} is not a canonical decimal string: {encoded:?}"
    );
    encoded
        .parse()
        .unwrap_or_else(|_| panic!("{field} exceeds u128: {encoded}"))
}

fn assert_object_keys(value: &Value, expected: &[&str], context: &str) {
    let actual = value
        .as_object()
        .unwrap_or_else(|| panic!("{context} must be an object"))
        .keys()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let expected = expected.iter().copied().collect::<BTreeSet<_>>();
    assert_eq!(actual, expected, "unexpected {context} fields");
}

fn assert_outcome(result: &Value, expected: &str) {
    assert_eq!(
        result["outcome"].as_str(),
        Some(expected),
        "unexpected settlement result: {result}"
    );
}

fn assert_external_fetch_coverage(result: &Value) {
    let external_io = result["externalIo"]
        .as_array()
        .expect("blocked settlement externalIo must be an array");
    let operation = external_io
        .iter()
        .find(|operation| {
            let precisely_classified = operation["kind"] == "fetch"
                && operation["owner"] == "script"
                && operation["loadBlocking"] == "non_blocking";
            let conservative_resource_coverage = operation["kind"]
                == "unclassified_producer_io"
                && operation["owner"] == "other"
                && operation["loadBlocking"] == "unknown";
            precisely_classified || conservative_resource_coverage
        })
        .unwrap_or_else(|| {
            panic!(
                "blocked settlement did not retain precise or conservative fetch coverage: {external_io:?}"
            )
        });
    assert_eq!(operation["phase"], "awaiting_response");
    let source_id = operation["sourceId"]
        .as_str()
        .expect("external-I/O coverage must retain a sourceId");
    assert_canonical_decimal(source_id, "externalIo.sourceId");

    let active = result["snapshot"]["network"]["active"]
        .as_array()
        .expect("snapshot.network.active must be an array");
    let active_operation = active
        .iter()
        .find(|candidate| candidate["sourceId"] == source_id)
        .expect("external-I/O coverage was absent from snapshot.network.active");
    assert_eq!(active_operation, operation);

    let sources = result["snapshot"]["sources"]
        .as_array()
        .expect("snapshot.sources must be an array");
    let source = sources
        .iter()
        .find(|candidate| candidate["sourceId"] == source_id)
        .expect("external-I/O coverage was absent from snapshot.sources");
    assert_eq!(source["kind"], "network");
    assert_eq!(source["state"], "awaiting_external_io");
    assert_eq!(source["owner"], operation["owner"]);
    assert_eq!(source["loadBlocking"], operation["loadBlocking"]);
}

fn assert_no_active_network(result: &Value) {
    assert!(
        result["snapshot"]["network"]["active"]
            .as_array()
            .expect("snapshot.network.active must be an array")
            .is_empty(),
        "settlement returned with active network work: {result}"
    );
}

fn parse_finite_number(value: &str) -> f64 {
    let value = value
        .parse::<f64>()
        .unwrap_or_else(|error| panic!("expected a finite number, got {value:?}: {error}"));
    assert!(value.is_finite(), "expected a finite number, got {value}");
    value
}

struct FixtureServer {
    url: String,
    address: std::net::SocketAddr,
    shutting_down: Arc<AtomicBool>,
    stall: Option<Arc<StallGate>>,
    stalled: Option<Receiver<()>>,
    thread: Option<JoinHandle<()>>,
}

impl FixtureServer {
    fn start(fixture: &'static [u8], stalls: bool) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .expect("failed to bind the controlled fixture server");
        let address = listener.local_addr().unwrap();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let (stall, stalled) = if stalls {
            let (sender, receiver) = sync_channel(1);
            (Some(Arc::new(StallGate::new(sender))), Some(receiver))
        } else {
            (None, None)
        };
        let thread_shutdown = shutting_down.clone();
        let thread_stall = stall.clone();
        let thread = thread::spawn(move || {
            let mut workers = Vec::new();
            loop {
                let (stream, _) = listener
                    .accept()
                    .expect("controlled fixture server accept failed");
                if thread_shutdown.load(Ordering::Acquire) {
                    break;
                }
                let stall = thread_stall.clone();
                workers.push(thread::spawn(move || {
                    serve_fixture_request(stream, fixture, stall.as_deref());
                }));
            }
            for worker in workers {
                worker.join().expect("controlled fixture worker panicked");
            }
        });
        Self {
            url: format!("http://{address}/"),
            address,
            shutting_down,
            stall,
            stalled,
            thread: Some(thread),
        }
    }

    fn url(&self) -> &str {
        &self.url
    }

    fn wait_until_stalled(&self) {
        self.stalled
            .as_ref()
            .expect("fixture server has no stall")
            .recv_timeout(STALL_OBSERVATION_TIMEOUT)
            .expect("page did not reach its foreground fetch");
    }

    fn release_stall(&self) {
        self.stall
            .as_ref()
            .expect("fixture server has no stall")
            .release();
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        if let Some(stall) = &self.stall {
            stall.release();
        }
        self.shutting_down.store(true, Ordering::Release);
        TcpStream::connect(self.address).ok();
        if let Some(thread) = self.thread.take() {
            let joined = thread.join();
            if !thread::panicking() {
                joined.expect("controlled fixture server panicked");
            }
        }
    }
}

struct StallGate {
    released: Mutex<bool>,
    condition: Condvar,
    observed: Mutex<Option<SyncSender<()>>>,
}

impl StallGate {
    fn new(observed: SyncSender<()>) -> Self {
        Self {
            released: Mutex::new(false),
            condition: Condvar::new(),
            observed: Mutex::new(Some(observed)),
        }
    }

    fn wait(&self) {
        if let Some(observed) = self
            .observed
            .lock()
            .expect("stall observation lock poisoned")
            .take()
        {
            observed.send(()).ok();
        }
        let mut released = self.released.lock().expect("stall gate lock poisoned");
        while !*released {
            released = self
                .condition
                .wait(released)
                .expect("stall gate lock poisoned while waiting");
        }
    }

    fn release(&self) {
        *self.released.lock().expect("stall gate lock poisoned") = true;
        self.condition.notify_all();
    }
}

fn serve_fixture_request(mut stream: TcpStream, fixture: &'static [u8], stall: Option<&StallGate>) {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("failed to set fixture request timeout");
    let mut request_line = String::new();
    {
        let mut request = BufReader::new(&mut stream);
        request
            .read_line(&mut request_line)
            .expect("failed to read fixture request line");
        loop {
            let mut header = String::new();
            request
                .read_line(&mut header)
                .expect("failed to read fixture request header");
            if header == "\r\n" || header == "\n" || header.is_empty() {
                break;
            }
        }
    }
    let path = request_line
        .split_whitespace()
        .nth(1)
        .and_then(|path| path.split('?').next())
        .expect("fixture request has no path");
    match path {
        "/" => write_http_response(&mut stream, "200 OK", "text/html; charset=utf-8", fixture),
        "/stall" => {
            stall
                .expect("received /stall on a fixture without a stall gate")
                .wait();
            write_http_response(
                &mut stream,
                "200 OK",
                "application/json",
                br#"{"status":"done"}"#,
            );
        },
        "/xhr" => write_http_response(
            &mut stream,
            "200 OK",
            "text/plain; charset=utf-8",
            b"xhr complete",
        ),
        _ => write_http_response(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

fn write_http_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .expect("failed to write fixture response headers");
    stream
        .write_all(body)
        .expect("failed to write fixture response body");
    stream.flush().expect("failed to flush fixture response");
}
