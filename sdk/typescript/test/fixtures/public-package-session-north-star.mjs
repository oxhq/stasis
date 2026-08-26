#!/usr/bin/env node

import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { constants as fsConstants, createReadStream } from "node:fs";
import { access, lstat, readdir, readFile, realpath } from "node:fs/promises";
import { createServer } from "node:http";
import { dirname, isAbsolute, join, relative, sep } from "node:path";
import { promisify } from "node:util";

import {
  CONTROLLED_WEB_SESSION_V1_PROFILE,
  StasisProtocolError,
  createStasisSessionPool,
  crawlWithStasis,
  launch,
} from "@oxhq/stasis";

const RUNS = 3;
const INITIAL_VIRTUAL_TIME_NS = 1_000_000_000n;
const SYNTHETIC_EMAIL = "session-user@example.invalid";
const SYNTHETIC_PASSWORD = "fixture-secret-password";
const AUTH_COOKIE_VALUE = "fixture-secret-session-cookie";
const MUTATED_AUTH_COOKIE_VALUE = "fixture-secret-mutated-session-cookie";
const ROLE = "Session Test Operator";
const MUTATED_ROLE = "Session State Mutated Operator";
const MUTATED_FLOW = "mutated-login";
const DETAIL = "controlled-detail-ready";
const SETTLE_POLICY = {
  persistentWork: "report",
  maxVirtualTimeNs: 5_000_000_000n,
  maxControlTurns: 100_000n,
  wallIoTimeoutNs: 10_000_000_000n,
};
const SELECTED_REGIONS = ["north", "west"];
const INTERACTION_FINGERPRINT = {
  activation: "activated=1",
  focus: "email=1",
  check: "input=2,change=2,checked=false",
  select: "input=1,change=1,values=north|west",
};
const REQUIRED_METHODS = [
  "session.open",
  "session.close",
  "session.navigate",
  "session.cookies.get",
  "session.cookies.set",
  "session.storage.get",
  "session.storage.set",
  "session.state.export",
  "session.state.import",
  "session.requests",
  "session.evidence",
  "runtime.pending",
  "runtime.settle",
  "runtime.advance_to_next",
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
];
const FIXTURES = {
  login: await readFile(new URL("./session-north-star/login.html", import.meta.url)),
  dashboard: await readFile(
    new URL("./session-north-star/dashboard.html", import.meta.url),
  ),
  second: await readFile(new URL("./session-north-star/second.html", import.meta.url)),
  restored: await readFile(
    new URL("./session-north-star/restored.html", import.meta.url),
  ),
};
const explicitBinary = process.env.STASIS_SESSION_NORTH_STAR_BINARY;
const configuredRuntimeCache = process.env.STASIS_SESSION_NORTH_STAR_RUNTIME_CACHE;
const runtimeCacheProof = await prepareRuntimeCacheProof(
  explicitBinary,
  configuredRuntimeCache,
  process.env.STASIS_SESSION_NORTH_STAR_EXPECTED_ARCHIVE_SHA256,
  process.env.STASIS_SESSION_NORTH_STAR_EXPECTED_BINARY_SHA256,
  process.env.STASIS_SESSION_NORTH_STAR_EXPECTED_REVISION,
);
let managedChildExecutable;
const launchOptions = {
  ...(explicitBinary ? { executablePath: explicitBinary } : {}),
  ...(configuredRuntimeCache
    ? { runtimeCacheDirectory: runtimeCacheProof.runtimeCacheDirectory }
    : {}),
};
const execFileAsync = promisify(execFile);

async function launchNorthStarRuntime() {
  const runtime = await launch(launchOptions);
  try {
    if (
      runtimeCacheProof.mode === "managed-empty-cache-installed" &&
      managedChildExecutable === undefined
    ) {
      const executable = await inspectManagedRuntimeCacheProof(runtimeCacheProof);
      await assertManagedChildExecutable(runtime.pid, executable);
      managedChildExecutable = executable;
    }
    return runtime;
  } catch (error) {
    await runtime.close().catch(() => undefined);
    throw error;
  }
}

async function assertManagedChildExecutable(pid, executable) {
  assert.ok(Number.isSafeInteger(pid) && pid > 0, "managed runtime did not expose a live PID");
  const expectedExecutable = await realpath(executable);
  if (process.platform === "linux") {
    assert.equal(
      await realpath(`/proc/${pid}/exe`),
      expectedExecutable,
      "managed runtime child is not executing the digest-keyed cache binary",
    );
    return;
  }
  assert.equal(process.platform, "darwin", "managed child proof ran on an unsupported host");
  const { stdout } = await execFileAsync(
    "/bin/ps",
    ["-ww", "-p", String(pid), "-o", "comm="],
    { encoding: "utf8" },
  );
  assert.equal(
    await realpath(stdout.trim()),
    expectedExecutable,
    "managed runtime child command is not the digest-keyed cache binary",
  );
}

function sessionNetwork(server) {
  return {
    mode: "mixed",
    routes: [
      {
        match: {
          method: "GET",
          url: {
            exact: server.url("/api/profile?nonce=fixture-secret-profile"),
          },
        },
        fulfill: {
          status: 200,
          headers: [["content-type", "application/json"]],
          body: { utf8: JSON.stringify({ role: ROLE }) },
        },
      },
    ],
  };
}

assertRequestEvidenceLifecycleRegression();

const server = await startSessionNorthStarServer();
const fingerprints = [];
try {
  for (let run = 0; run < RUNS; run += 1) {
    fingerprints.push(await executeRun(server));
  }
  await proveFixtureOnlyMissIsSticky(server);
  if (server.errors.length > 0) throw server.errors[0];
  assert.equal(server.logins.length, RUNS);
  for (const login of server.logins) {
    assert.deepEqual(login, {
      method: "POST",
      pathname: "/authenticate",
      contentType: "application/json",
      email: SYNTHETIC_EMAIL,
      password: SYNTHETIC_PASSWORD,
      terms: false,
      regions: SELECTED_REGIONS,
    });
  }
  for (const pathname of ["/dashboard", "/second", "/api/details"]) {
    const observations = server.requests.filter((request) => request.pathname === pathname);
    assert.equal(observations.length, RUNS, `fixture server did not observe every ${pathname}`);
    for (const observation of observations) {
      assert.match(
        observation.cookie,
        new RegExp(`(?:^|;\\s*)stasis-auth=${AUTH_COOKIE_VALUE}(?:;|$)`, "u"),
        `${pathname} did not receive the imported session cookie`,
      );
    }
  }
  const restoredObservations = server.requests.filter(
    (request) => request.pathname === "/restored",
  );
  assert.equal(restoredObservations.length, RUNS, "fixture server omitted a restored request");
  for (const observation of restoredObservations) {
    assert.match(
      observation.cookie,
      new RegExp(
        `(?:^|;\\s*)stasis-auth=${MUTATED_AUTH_COOKIE_VALUE}(?:;|$)`,
        "u",
      ),
      "/restored did not receive the mutated imported session cookie",
    );
  }
  assert.equal(
    server.requests.some((request) => request.pathname === "/api/profile"),
    false,
    "the deterministic /api/profile fixture unexpectedly reached the live server",
  );
  assert.deepEqual(
    fingerprints.slice(1).map((run) => run.semantic),
    Array.from({ length: RUNS - 1 }, () => fingerprints[0].semantic),
    "fresh controlled-session runs produced different semantic fingerprints",
  );
  const crawler = await proveReferenceCrawler(server);
  const runtimeAcquisition = await verifyRuntimeCacheProof(runtimeCacheProof);

  const first = fingerprints[0];
  assert.ok(first);
  const proof = {
    proof: "stasis-v0.2-session-north-star",
    runs: RUNS,
    sessions: RUNS * 2,
    negativeSessions: 1,
    profile: CONTROLLED_WEB_SESSION_V1_PROFILE,
    boundary: "controlled_ready",
    outcome: first.semantic.outcome,
    restoredOutcome: first.semantic.restoredOutcome,
    staleTokenError: "stale_state_token",
    fixtureMissError: "network_fixture_miss",
    cleanClose: true,
    runtimeAcquisition,
    crawler,
    interaction: first.semantic.interaction,
    navigation: first.semantic.navigation,
    audit: {
      schemaVersion: 2,
      requestCount: first.audit.requestCount,
      evidenceCount: first.audit.evidenceCount,
      redirectEvents: first.audit.evidenceKindCounts.redirect ?? 0,
      sameDocumentHistoryEvents:
        first.audit.evidenceKindCounts.same_document_history_changed ?? 0,
      settlementEvents: first.audit.evidenceKindCounts.settlement_terminal ?? 0,
      redacted: true,
    },
    state: { ...first.semantic.stateShape, restored: true },
  };
  assertProofRedacted(JSON.stringify(proof));
  process.stdout.write(`${JSON.stringify(proof)}\n`);
} finally {
  await server.close();
}

async function prepareRuntimeCacheProof(
  binary,
  cacheDirectory,
  expectedArchiveSha256,
  expectedBinarySha256,
  expectedRevision,
) {
  if (cacheDirectory === undefined) {
    return {
      mode: binary ? "explicit-executable-override" : "managed-default-cache",
      runtimeCacheDirectory: undefined,
      expectedArchiveSha256,
      expectedBinarySha256,
      expectedRevision,
    };
  }
  assert.ok(
    isAbsolute(cacheDirectory),
    "STASIS_SESSION_NORTH_STAR_RUNTIME_CACHE must be an absolute path",
  );
  const runtimeCacheDirectory = await realpath(cacheDirectory);
  assert.deepEqual(
    await readdir(runtimeCacheDirectory),
    [],
    "the isolated runtime cache must start empty",
  );
  if (!binary) {
    assert.match(
      expectedArchiveSha256 ?? "",
      /^[0-9a-f]{64}$/u,
      "managed cache proof requires the verified public archive SHA-256",
    );
    assert.match(
      expectedBinarySha256 ?? "",
      /^[0-9a-f]{64}$/u,
      "managed cache proof requires the verified public binary SHA-256",
    );
    assert.match(
      expectedRevision ?? "",
      /^[0-9a-f]{40}$/u,
      "managed cache proof requires the verified release revision",
    );
  }
  return {
    mode: binary
      ? "explicit-executable-override-cache-bypassed"
      : "managed-empty-cache-installed",
    runtimeCacheDirectory,
    expectedArchiveSha256,
    expectedBinarySha256,
    expectedRevision,
  };
}

async function verifyRuntimeCacheProof(proof) {
  if (proof.runtimeCacheDirectory === undefined) return proof.mode;
  if (proof.mode === "explicit-executable-override-cache-bypassed") {
    assert.deepEqual(
      await readdir(proof.runtimeCacheDirectory),
      [],
      "executablePath unexpectedly accessed the managed runtime cache",
    );
    return proof.mode;
  }

  const executable = await inspectManagedRuntimeCacheProof(proof);
  assert.equal(
    managedChildExecutable,
    executable,
    "managed cache proof did not bind a live child to the installed executable",
  );
  return proof.mode;
}

async function inspectManagedRuntimeCacheProof(proof) {
  const entries = await collectRuntimeCacheEntries(proof.runtimeCacheDirectory);
  const markers = entries.filter((entry) => entry.relativePath.endsWith("/.stasis-runtime.json"));
  assert.equal(markers.length, 1, "managed launch did not publish exactly one runtime cache marker");
  const markerEntry = markers[0];
  assert.ok(markerEntry);
  const marker = JSON.parse(await readFile(markerEntry.absolutePath, "utf8"));
  assert.equal(marker.schema, 1);
  assert.equal(marker.packageName, "@oxhq/stasis");
  assert.equal(marker.sdkVersion, "0.3.0");
  assert.equal(marker.releaseTag, "v0.3.0");
  assert.equal(marker.implementation?.name, "stasis-shell");
  assert.equal(marker.executablePath, "stasis");
  const expectedReleasePlatform =
    process.platform === "darwin" && process.arch === "arm64"
      ? "macos-aarch64"
      : process.platform === "linux" && process.arch === "x64"
        ? "linux-x86_64"
        : undefined;
  assert.ok(expectedReleasePlatform, "managed runtime proof ran on an unsupported release host");
  assert.equal(marker.platform, `${process.platform}-${process.arch}`);
  assert.equal(marker.releasePlatform, expectedReleasePlatform);
  assert.equal(
    marker.archiveSha256,
    proof.expectedArchiveSha256,
    "managed runtime marker is not bound to the verified public archive",
  );
  assert.equal(
    marker.executableSha256,
    proof.expectedBinarySha256,
    "managed runtime marker is not bound to the verified public executable",
  );
  assert.equal(
    marker.implementation?.source?.stasis_revision,
    proof.expectedRevision,
    "managed runtime marker is not bound to the verified release revision",
  );
  assert.equal(
    markerEntry.relativePath,
    join(
      "runtime-v1",
      "0.3.0",
      `${process.platform}-${process.arch}`,
      marker.archiveSha256,
      ".stasis-runtime.json",
    ),
    "managed runtime marker was not published at its digest-keyed cache identity",
  );
  const archiveUrl = new URL(marker.archiveUrl);
  assert.equal(
    archiveUrl.href,
    `https://github.com/oxhq/stasis/releases/download/v0.3.0/stasis-0.3.0-${expectedReleasePlatform}.tar.gz`,
  );

  const executable = join(dirname(markerEntry.absolutePath), marker.executablePath);
  const executableMetadata = await lstat(executable);
  assert.ok(
    executableMetadata.isFile() && !executableMetadata.isSymbolicLink(),
    "managed runtime cache executable is not a regular file",
  );
  await access(executable, fsConstants.X_OK);
  assert.equal(
    await sha256(executable),
    marker.executableSha256,
    "managed runtime cache executable does not match its install marker",
  );
  return executable;
}

async function collectRuntimeCacheEntries(root) {
  const entries = [];
  const visit = async (directory) => {
    const children = await readdir(directory, { withFileTypes: true });
    assert.ok(children.length <= 256, "managed runtime cache directory is unexpectedly large");
    for (const child of children) {
      const absolutePath = join(directory, child.name);
      const relativePath = relative(root, absolutePath);
      assert.ok(
        relativePath !== "" && relativePath !== ".." && !relativePath.startsWith(`..${sep}`),
        "managed runtime cache entry escaped its root",
      );
      assert.equal(child.isSymbolicLink(), false, "managed runtime cache contains a symbolic link");
      entries.push({ absolutePath, relativePath });
      if (child.isDirectory()) await visit(absolutePath);
    }
  };
  await visit(root);
  assert.ok(entries.length > 0 && entries.length <= 512, "managed runtime cache inventory is invalid");
  return entries;
}

async function sha256(filename) {
  const digest = createHash("sha256");
  for await (const chunk of createReadStream(filename)) digest.update(chunk);
  return digest.digest("hex");
}

async function executeRun(server) {
  const primary = await executePrimarySession(server);
  const restored = await executeRestoredSession(server, primary.state);
  return {
    semantic: {
      outcome: primary.outcome,
      restoredOutcome: restored.outcome,
      navigation: primary.navigation,
      interaction: primary.interaction,
      dashboardTrace: primary.dashboardTrace,
      secondStatus: primary.secondStatus,
      restoredStatus: restored.status,
      stateShape: primary.stateShape,
    },
    audit: {
      requestCount: primary.requests.length,
      restoredRequestCount: restored.requests.length,
      evidenceCount: primary.evidence.length,
      evidenceKindCounts: countEvidenceKinds(primary.evidence),
    },
  };
}

async function executePrimarySession(server) {
  const runtime = await launchNorthStarRuntime();
  let closed = false;
  try {
    assertV02Capabilities(runtime);
    const startUrl = server.url("/start");
    const session = await runtime.openSession(startUrl, {
      clock: {
        mode: "controlled",
        initialVirtualTimeNs: INITIAL_VIRTUAL_TIME_NS,
        unixTimeOriginNs: 0n,
      },
      network: sessionNetwork(server),
    });
    assert.equal(session.requestedUrl, startUrl);
    assert.equal(session.url, server.url("/login?from=start"));
    assert.equal(session.boundary, "controlled_ready");
    assert.equal(session.profile, CONTROLLED_WEB_SESSION_V1_PROFILE);

    const initial = await session.settle(session.stateToken, SETTLE_POLICY);
    assert.equal(initial.outcome, "quiescent");
    assert.equal(initial.virtualTimeNs, INITIAL_VIRTUAL_TIME_NS);
    const pending = await session.pending();
    assert.equal(pending.stateToken, initial.stateToken);
    assert.equal(pending.timers.nextDeadlineNs, undefined);
    const noAdvance = await session.advanceToNext(pending.stateToken);
    assert.equal(noAdvance.outcome, "no_finite_deadline");
    assert.equal(noAdvance.virtualTimeNs, INITIAL_VIRTUAL_TIME_NS);
    assert.equal(noAdvance.snapshot.stateToken, noAdvance.stateToken);
    assert.equal((await session.text("#status", noAdvance.stateToken)).value, "login-ready");

    const encodeStateBytes = (state) =>
      Buffer.from(
        JSON.stringify(state, (_key, value) =>
          typeof value === "bigint" ? value.toString() : value,
        ),
        "utf8",
      );
    const beforeStorageLimit = await session.exportState();
    const beforeStorageLimitBytes = encodeStateBytes(beforeStorageLimit.state);
    const overflowAttempt = await session.activate(
      "#storage-overflow",
      noAdvance.stateToken,
    );
    assert.equal(
      (await session.text("#storage-limit-result", overflowAttempt.stateToken)).value,
      "QuotaExceededError",
      "controlled page Web Storage did not expose the typed quota boundary",
    );
    const afterRejectedStorage = await session.exportState();
    assert.equal(
      afterRejectedStorage.sessionStateToken,
      beforeStorageLimit.sessionStateToken,
      "rejected controlled page Web Storage changed the session-state revision",
    );
    assert.deepEqual(
      encodeStateBytes(afterRejectedStorage.state),
      beforeStorageLimitBytes,
      "rejected controlled page Web Storage changed exported state bytes",
    );

    const withinBudget = await session.activate(
      "#storage-within-budget",
      overflowAttempt.stateToken,
    );
    assert.equal(
      (await session.text("#storage-success-result", withinBudget.stateToken)).value,
      "stored",
    );
    const afterSuccessfulStorage = await session.exportState();
    assert.notEqual(
      afterSuccessfulStorage.sessionStateToken,
      afterRejectedStorage.sessionStateToken,
      "successful controlled page Web Storage did not rotate session authority",
    );
    const budgetOrigin = afterSuccessfulStorage.state.origins.find(
      (entry) => entry.origin === server.origin,
    );
    assert.equal(
      budgetOrigin?.localStorage.find((entry) => entry.key === "stasis-budget-proof")?.value,
      "bounded-value",
      "successful below-bound page write was absent from session.state.export",
    );

    const cleanedStorage = await session.activate(
      "#storage-cleanup",
      withinBudget.stateToken,
    );
    assert.equal(
      (await session.text("#storage-success-result", cleanedStorage.stateToken)).value,
      "removed",
    );
    const afterStorageCleanup = await session.exportState();
    assert.notEqual(
      afterStorageCleanup.sessionStateToken,
      afterSuccessfulStorage.sessionStateToken,
    );
    assert.deepEqual(
      encodeStateBytes(afterStorageCleanup.state),
      beforeStorageLimitBytes,
      "successful controlled storage cleanup did not restore the fixture state",
    );

    const staleToken = cleanedStorage.stateToken;
    const activated = await session.activate("#prepare", staleToken);
    assert.notEqual(activated.stateToken, staleToken);
    await assert.rejects(session.text("#status", staleToken), assertStaleDocumentToken);
    assert.equal(
      (await session.text("#activation-events", activated.stateToken)).value,
      INTERACTION_FINGERPRINT.activation,
    );

    const focused = await session.focus("#email", activated.stateToken);
    assert.equal(focused.focused, true);
    const focusedPending = await session.pending();
    assert.equal(focusedPending.stateToken, focused.stateToken);
    assert.deepEqual(
      focusedPending.clock.unsupportedSurfaces,
      [],
      "semantic focus leaked a native embedder-control surface into controlled authority",
    );
    assert.equal(
      (await session.text("#focus-events", focused.stateToken)).value,
      INTERACTION_FINGERPRINT.focus,
    );

    const nonfocused = await session.focus("#nonfocusable", focused.stateToken);
    assert.equal(nonfocused.focused, false);
    assert.equal(
      (await session.text("#focus-events", nonfocused.stateToken)).value,
      INTERACTION_FINGERPRINT.focus,
      "focusing a non-focusable element dispatched a focus event",
    );

    const emailFill = await session.fill("#email", SYNTHETIC_EMAIL, nonfocused.stateToken);
    assert.equal(
      (await session.text("#input-events", emailFill.stateToken)).value,
      "email=1,password=0",
    );
    const passwordFill = await session.fill(
      "#password",
      SYNTHETIC_PASSWORD,
      emailFill.stateToken,
    );
    assert.equal(
      (await session.text("#input-events", passwordFill.stateToken)).value,
      "email=1,password=1",
    );

    const checked = await session.check("#terms", passwordFill.stateToken);
    assert.deepEqual(
      { changed: checked.changed, checked: checked.checked },
      { changed: true, checked: true },
    );
    assert.equal(
      (await session.text("#check-events", checked.stateToken)).value,
      "input=1,change=1,checked=true",
    );
    const unchecked = await session.uncheck("#terms", checked.stateToken);
    assert.deepEqual(
      { changed: unchecked.changed, checked: unchecked.checked },
      { changed: true, checked: false },
    );
    assert.equal(
      (await session.text("#check-events", unchecked.stateToken)).value,
      INTERACTION_FINGERPRINT.check,
    );

    const initiallyCheckedRadio = await session.check("#plan-basic", unchecked.stateToken);
    assert.deepEqual(
      { changed: initiallyCheckedRadio.changed, checked: initiallyCheckedRadio.checked },
      { changed: false, checked: true },
    );
    assert.equal(
      (await session.text("#radio-events", initiallyCheckedRadio.stateToken)).value,
      "basic-input=0,basic-change=0,pro-input=0,pro-change=0,basic=true,pro=false",
    );
    const proChecked = await session.check("#plan-pro", initiallyCheckedRadio.stateToken);
    assert.deepEqual(
      { changed: proChecked.changed, checked: proChecked.checked },
      { changed: true, checked: true },
    );
    assert.equal(
      (await session.text("#radio-events", proChecked.stateToken)).value,
      "basic-input=0,basic-change=0,pro-input=1,pro-change=1,basic=false,pro=true",
    );
    const proCheckedAgain = await session.check("#plan-pro", proChecked.stateToken);
    assert.deepEqual(
      { changed: proCheckedAgain.changed, checked: proCheckedAgain.checked },
      { changed: false, checked: true },
    );
    await assert.rejects(
      session.uncheck("#plan-pro", proCheckedAgain.stateToken),
      assertNonMutatingAutomationRejection("unsupported_uncheck_element"),
    );
    assert.equal(
      (await session.text("#radio-events", proCheckedAgain.stateToken)).value,
      "basic-input=0,basic-change=0,pro-input=1,pro-change=1,basic=false,pro=true",
      "rejecting radio uncheck changed the group or dispatched events",
    );
    const basicChecked = await session.check("#plan-basic", proCheckedAgain.stateToken);
    assert.deepEqual(
      { changed: basicChecked.changed, checked: basicChecked.checked },
      { changed: true, checked: true },
    );
    assert.equal(
      (await session.text("#radio-events", basicChecked.stateToken)).value,
      "basic-input=1,basic-change=1,pro-input=1,pro-change=1,basic=true,pro=false",
    );

    await assert.rejects(
      session.select("#primary-region", [], basicChecked.stateToken),
      assertNonMutatingAutomationRejection("invalid_select_multiplicity"),
    );
    await assert.rejects(
      session.select("#primary-region", ["east", "central"], basicChecked.stateToken),
      assertNonMutatingAutomationRejection("invalid_select_multiplicity"),
    );
    assert.equal(
      (await session.text("#single-select-events", basicChecked.stateToken)).value,
      "input=0,change=0,value=east,selected=1",
      "invalid single-select cardinality changed selection or dispatched events",
    );
    const primarySelected = await session.select(
      "#primary-region",
      ["central"],
      basicChecked.stateToken,
    );
    assert.deepEqual(
      { changed: primarySelected.changed, values: primarySelected.values },
      { changed: true, values: ["central"] },
    );
    assert.equal(
      (await session.text("#single-select-events", primarySelected.stateToken)).value,
      "input=1,change=1,value=central,selected=1",
    );
    const primarySelectedAgain = await session.select(
      "#primary-region",
      ["central"],
      primarySelected.stateToken,
    );
    assert.deepEqual(
      { changed: primarySelectedAgain.changed, values: primarySelectedAgain.values },
      { changed: false, values: ["central"] },
    );
    assert.equal(
      (await session.text("#single-select-events", primarySelectedAgain.stateToken)).value,
      "input=1,change=1,value=central,selected=1",
      "reselecting the single-select value dispatched duplicate events",
    );

    const selected = await session.select(
      "#region",
      SELECTED_REGIONS,
      primarySelectedAgain.stateToken,
    );
    assert.equal(selected.changed, true);
    assert.deepEqual(selected.values, SELECTED_REGIONS);
    assert.equal(
      (await session.text("#select-events", selected.stateToken)).value,
      INTERACTION_FINGERPRINT.select,
    );

    const submitted = await session.submit("#login-form", selected.stateToken);
    assert.equal(submitted.submitted, true);

    const dashboard = await session.settle(submitted.stateToken, SETTLE_POLICY);
    assert.equal(dashboard.outcome, "quiescent");
    assert.ok(dashboard.virtualTimeNs - initial.virtualTimeNs >= 250_000_000n);
    assert.equal((await session.text("#status", dashboard.stateToken)).value, "dashboard-ready");
    const dashboardTrace = (await session.text("#trace", dashboard.stateToken)).value;
    assert.equal(dashboardTrace, "dashboard,history,fetch,promise,microtask,timer,raf");
    const query = await session.query("#dashboard > a.next", dashboard.stateToken);
    assert.equal(query.count, 1n);
    assert.equal(query.stateToken, dashboard.stateToken);

    const extraction = await session.extract(
      {
        rootSelector: "#dashboard",
        fields: [
          { name: "email", selector: "#account-email", read: "text" },
          { name: "role", selector: "#account-role", read: "text" },
          { name: "flow", selector: "#flow-state", read: "text" },
          {
            name: "next",
            selector: "a.next",
            read: "resolved_url",
            attribute: "href",
          },
          {
            name: "missing",
            selector: "a.next",
            read: "attribute",
            attribute: "data-missing",
          },
        ],
      },
      query.stateToken,
    );
    const nextUrl = server.url("/redirect-next?secret=fixture-secret-navigation");
    assert.deepEqual(extraction.rows, [
      {
        fields: [
          { name: "email", value: SYNTHETIC_EMAIL },
          { name: "role", value: ROLE },
          { name: "flow", value: "login" },
          { name: "next", value: nextUrl },
          { name: "missing", value: null },
        ],
      },
    ]);

    const navigated = await session.navigate(nextUrl, extraction.stateToken);
    assert.equal(navigated.boundary, "controlled_ready");
    assert.equal(navigated.requestedUrl, nextUrl);
    const finalNavigationUrl = new URL(navigated.url);
    assert.equal(finalNavigationUrl.origin, server.origin);
    assert.equal(finalNavigationUrl.pathname, "/second");
    assert.equal(finalNavigationUrl.search, "?stage=2&secret=fixture-secret-final");
    assert.equal(navigated.documentEpoch, 3n);
    assert.equal(navigated.navigationId, 2n);
    assert.equal(navigated.historyRevision, 2n);
    assert.notEqual(
      navigated.stateToken,
      extraction.stateToken,
      "the replacement document reused the previous document authority token",
    );

    const second = await session.settle(navigated.stateToken, SETTLE_POLICY);
    assert.equal(second.outcome, "quiescent");
    const secondStatus = (await session.text("#second-status", second.stateToken)).value;
    assert.equal(secondStatus, "second-ready");
    assert.equal((await session.text("#second-email", second.stateToken)).value, SYNTHETIC_EMAIL);
    assert.equal((await session.text("#second-role", second.stateToken)).value, ROLE);
    assert.equal((await session.text("#second-flow", second.stateToken)).value, "login");
    assert.equal((await session.text("#detail-value", second.stateToken)).value, DETAIL);

    const cookies = await session.getCookies();
    const storage = await session.getStorage();
    assert.equal(storage.sessionStateToken, cookies.sessionStateToken);
    const staleSessionStateToken = cookies.sessionStateToken;
    assert.ok(
      cookies.cookies.some(
        (cookie) => cookie.name === "stasis-auth" && cookie.value === AUTH_COOKIE_VALUE,
      ),
      "cookie snapshot omitted the pre-mutation authentication value",
    );
    const mutatedCookies = cookies.cookies.map((cookie) =>
      cookie.name === "stasis-auth"
        ? { ...cookie, value: MUTATED_AUTH_COOKIE_VALUE }
        : cookie,
    );
    const cookiesReplaced = await session.setCookies(
      mutatedCookies,
      staleSessionStateToken,
    );
    assert.notEqual(cookiesReplaced.sessionStateToken, staleSessionStateToken);
    const mutatedCookieSnapshot = await session.getCookies();
    assert.equal(
      mutatedCookieSnapshot.sessionStateToken,
      cookiesReplaced.sessionStateToken,
    );
    assert.ok(
      mutatedCookieSnapshot.cookies.some(
        (cookie) =>
          cookie.name === "stasis-auth" && cookie.value === MUTATED_AUTH_COOKIE_VALUE,
      ),
      "session.cookies.set did not persist the mutated authentication value",
    );
    await assert.rejects(
      session.setStorage(storage.origins, staleSessionStateToken),
      assertStaleSessionStateToken,
    );
    const mutatedOrigins = mutateStorageState(storage.origins, server.origin);
    const storageReplaced = await session.setStorage(
      mutatedOrigins,
      cookiesReplaced.sessionStateToken,
    );
    assert.notEqual(
      storageReplaced.sessionStateToken,
      cookiesReplaced.sessionStateToken,
    );
    const mutatedStorageSnapshot = await session.getStorage();
    assert.equal(
      mutatedStorageSnapshot.sessionStateToken,
      storageReplaced.sessionStateToken,
    );
    assertMutatedStorage(mutatedStorageSnapshot.origins, server.origin);
    const exported = await session.exportState();
    assert.equal(exported.sessionStateToken, storageReplaced.sessionStateToken);
    const stateShape = assertExportedState(exported.state, server.origin);

    const requests = await collectRequests(session, second.stateToken);
    assertRequestProof(requests, server.origin);
    const evidence = await collectEvidence(session, second.stateToken);
    assertEvidenceProof(evidence, requests);
    assertAuditRedaction(requests, evidence);

    await closeSessionAndAssertProcessExit(runtime, session);
    closed = true;
    return {
      state: exported.state,
      stateShape,
      outcome: second.outcome,
      interaction: INTERACTION_FINGERPRINT,
      dashboardTrace,
      secondStatus,
      navigation: {
        documentEpoch: navigated.documentEpoch.toString(),
        navigationId: navigated.navigationId.toString(),
        historyRevision: navigated.historyRevision.toString(),
      },
      requests,
      evidence,
    };
  } finally {
    if (!closed) await runtime.close().catch(() => undefined);
  }
}

async function executeRestoredSession(server, state) {
  const runtime = await launchNorthStarRuntime();
  let closed = false;
  try {
    assertV02Capabilities(runtime);
    const restoredUrl = server.url("/restored?proof=fixture-secret-restored-query");
    const session = await runtime.openSession(restoredUrl, {
      state,
      network: sessionNetwork(server),
    });
    assert.equal(session.url, restoredUrl);
    assert.equal(session.boundary, "controlled_ready");
    const settled = await session.settle(session.stateToken, SETTLE_POLICY);
    assert.equal(settled.outcome, "quiescent");
    const status = (await session.text("#restored-status", settled.stateToken)).value;
    assert.equal(status, "restored-ready");
    assert.equal((await session.text("#restored-email", settled.stateToken)).value, SYNTHETIC_EMAIL);
    assert.equal((await session.text("#restored-role", settled.stateToken)).value, MUTATED_ROLE);
    assert.equal((await session.text("#restored-flow", settled.stateToken)).value, MUTATED_FLOW);
    const cookies = await session.getCookies();
    assert.ok(
      cookies.cookies.some(
        (cookie) =>
          cookie.name === "stasis-auth" && cookie.value === MUTATED_AUTH_COOKIE_VALUE,
      ),
      "restored session did not import the mutated authentication cookie",
    );
    const storage = await session.getStorage();
    assert.equal(storage.sessionStateToken, cookies.sessionStateToken);
    assertMutatedStorage(storage.origins, server.origin);
    const restoredExport = await session.exportState();
    assert.equal(restoredExport.sessionStateToken, storage.sessionStateToken);
    assertExportedState(restoredExport.state, server.origin);
    const requests = await collectRequests(session, settled.stateToken);
    assert.equal(requests.length, 1, "restored session recorded an unexpected request");
    const restoredRequest = requests.find((request) => request.url.path === "/restored");
    assert.ok(restoredRequest);
    assert.deepEqual(restoredRequest.url.queryKeys, ["proof"]);
    assertAuditRedaction(requests, []);
    await closeSessionAndAssertProcessExit(runtime, session);
    closed = true;
    return { outcome: settled.outcome, status, requests };
  } finally {
    if (!closed) await runtime.close().catch(() => undefined);
  }
}

async function proveFixtureOnlyMissIsSticky(server) {
  const forbiddenPath = "/fixture-miss-must-not-reach-live";
  const runtime = await launchNorthStarRuntime();
  try {
    assertV02Capabilities(runtime);
    await assert.rejects(
      runtime.openSession(server.url(forbiddenPath), {
        network: { mode: "fixtures_only", routes: [] },
      }),
      (error) => {
        assert.ok(error instanceof StasisProtocolError);
        assert.equal(error.code, "network_fixture_miss");
        assert.equal(error.stateEffect, "partial");
        return true;
      },
    );
  } finally {
    await runtime.close().catch(() => undefined);
  }
  assert.equal(
    server.requests.some((request) => request.pathname === forbiddenPath),
    false,
    "fixtures_only miss fell through to ambient network",
  );
}

async function proveReferenceCrawler(server) {
  const rootUrl = server.url("/crawl-root");
  const firstUrl = server.url("/crawl-first");
  const secondUrl = server.url("/crawl-second");
  const contentType = [["content-type", "text/html; charset=utf-8"]];
  const route = (url, body) => ({
    match: { method: "GET", url: { exact: url } },
    fulfill: {
      status: 200,
      headers: contentType,
      body: { utf8: body },
    },
  });
  const network = {
    mode: "fixtures_only",
    routes: [
      route(
        rootUrl,
        '<!doctype html><main><a href="/crawl-first#discarded">first</a><a href="/crawl-second">second</a></main>',
      ),
      route(firstUrl, "<!doctype html><main>first</main>"),
      route(secondUrl, "<!doctype html><main>second</main>"),
      {
        match: { method: "GET", url: { exact: server.url("/favicon.ico") } },
        fulfill: {
          status: 200,
          headers: [["content-type", "image/x-icon"]],
          body: { utf8: "" },
        },
      },
    ],
  };
  const pool = createStasisSessionPool({
    maxProcesses: 2,
    maxQueue: 4,
    launch: launchOptions,
  });
  try {
    const result = await crawlWithStasis(pool, {
      start: `${rootUrl}#discarded`,
      maxPages: 3,
      maxDepth: 1,
      concurrency: 2,
      network,
      settle: SETTLE_POLICY,
    });
    assert.deepEqual(result.scheduledUrls, [rootUrl, firstUrl, secondUrl]);
    assert.deepEqual(
      result.pages.map(({ requestedUrl, depth, status, settleOutcome }) => ({
        requestedUrl,
        depth,
        status,
        settleOutcome,
      })),
      [
        { requestedUrl: rootUrl, depth: 0, status: "crawled", settleOutcome: "quiescent" },
        { requestedUrl: firstUrl, depth: 1, status: "crawled", settleOutcome: "quiescent" },
        { requestedUrl: secondUrl, depth: 1, status: "crawled", settleOutcome: "quiescent" },
      ],
    );
    for (const pathname of ["/crawl-root", "/crawl-first", "/crawl-second"]) {
      assert.equal(
        server.requests.some((request) => request.pathname === pathname),
        false,
        `crawler fixture unexpectedly reached ambient network: ${pathname}`,
      );
    }
    return {
      pages: result.pages.length,
      freshSessions: result.pages.length,
      concurrency: 2,
      maxDepth: 1,
      network: "fixtures_only",
      allCrawled: result.pages.every((page) => page.status === "crawled"),
    };
  } finally {
    await pool.close();
  }
}

function assertV02Capabilities(runtime) {
  assert.equal(runtime.info.implementation.name, "stasis-shell");
  assert.equal(runtime.info.implementation.version, "0.3.0");
  if (runtimeCacheProof.expectedRevision !== undefined) {
    assert.equal(
      runtime.info.implementation.source?.stasis_revision,
      runtimeCacheProof.expectedRevision,
      "runtime source identity differs from the selected release revision",
    );
  }
  assert.ok(runtime.info.capabilities.profiles.includes(CONTROLLED_WEB_SESSION_V1_PROFILE));
  for (const method of REQUIRED_METHODS) {
    assert.ok(runtime.info.capabilities.methods.includes(method), `runtime omitted ${method}`);
  }
}

function assertStaleDocumentToken(error) {
  assert.ok(error instanceof StasisProtocolError);
  assert.equal(error.code, "stale_state_token");
  assert.equal(error.fatal, false);
  assert.equal(error.stateEffect, "none");
  return true;
}

function assertStaleSessionStateToken(error) {
  assert.ok(error instanceof StasisProtocolError);
  assert.equal(error.code, "stale_session_state_token");
  assert.equal(error.fatal, false);
  assert.equal(error.stateEffect, "none");
  return true;
}

function assertNonMutatingAutomationRejection(code) {
  return (error) => {
    assert.ok(error instanceof StasisProtocolError);
    assert.equal(error.code, code);
    assert.equal(error.fatal, false);
    assert.equal(error.stateEffect, "none");
    return true;
  };
}

function assertExportedState(state, origin) {
  assert.equal(state.schemaVersion, 1);
  assert.equal(state.profile, CONTROLLED_WEB_SESSION_V1_PROFILE);
  assert.equal(state.sensitive, true);
  assert.equal(state.sessionStorageScope, "top_level_browsing_context");
  const authCookie = state.cookies.find((cookie) => cookie.name === "stasis-auth");
  assert.ok(authCookie, "state export omitted the authentication cookie");
  assert.equal(authCookie.value, MUTATED_AUTH_COOKIE_VALUE);
  assert.equal(authCookie.expiresUnixTimeNs, null);
  assert.equal(authCookie.partitioned, false);
  const originState = state.origins.find((entry) => entry.origin === origin);
  assert.ok(originState, `state export omitted ${origin}`);
  const localStorage = Object.fromEntries(
    originState.localStorage.map(({ key, value }) => [key, value]),
  );
  const sessionStorage = Object.fromEntries(
    originState.sessionStorage.map(({ key, value }) => [key, value]),
  );
  assert.deepEqual(localStorage, {
    "stasis-email": SYNTHETIC_EMAIL,
    "stasis-role": MUTATED_ROLE,
  });
  assert.deepEqual(sessionStorage, { "stasis-flow": MUTATED_FLOW });
  return {
    schemaVersion: state.schemaVersion,
    cookieCount: state.cookies.length,
    originCount: state.origins.length,
    localStorageKeys: Object.keys(localStorage),
    sessionStorageKeys: Object.keys(sessionStorage),
    sessionStorageScope: state.sessionStorageScope,
  };
}

function mutateStorageState(origins, origin) {
  let mutated = false;
  const result = origins.map((entry) => {
    if (entry.origin !== origin) return entry;
    mutated = true;
    return {
      ...entry,
      localStorage: entry.localStorage.map((item) =>
        item.key === "stasis-role" ? { ...item, value: MUTATED_ROLE } : item,
      ),
      sessionStorage: entry.sessionStorage.map((item) =>
        item.key === "stasis-flow" ? { ...item, value: MUTATED_FLOW } : item,
      ),
    };
  });
  assert.equal(mutated, true, `storage snapshot omitted ${origin}`);
  assertMutatedStorage(result, origin);
  return result;
}

function assertMutatedStorage(origins, origin) {
  const originState = origins.find((entry) => entry.origin === origin);
  assert.ok(originState, `storage snapshot omitted ${origin}`);
  assert.equal(
    originState.localStorage.find((entry) => entry.key === "stasis-role")?.value,
    MUTATED_ROLE,
  );
  assert.equal(
    originState.sessionStorage.find((entry) => entry.key === "stasis-flow")?.value,
    MUTATED_FLOW,
  );
}

async function collectRequests(session, expectedStateToken) {
  const records = [];
  let afterSeq = 0n;
  for (let pageIndex = 0; pageIndex < 64; pageIndex += 1) {
    const page = await session.requests({ afterSeq, limit: 32 });
    assert.equal(page.stateToken, expectedStateToken);
    assert.equal(page.complete, true);
    records.push(...page.records);
    if (!page.hasMore) return records;
    assert.ok(page.nextAfterSeq !== undefined && page.nextAfterSeq > afterSeq);
    afterSeq = page.nextAfterSeq;
  }
  assert.fail("request audit exceeded its bounded pagination proof");
}

async function collectEvidence(session, expectedStateToken) {
  const records = [];
  let afterSeq = 0n;
  for (let pageIndex = 0; pageIndex < 64; pageIndex += 1) {
    const page = await session.evidence({ afterSeq, limit: 32 });
    assert.equal(page.schemaVersion, 2);
    assert.equal(page.stateToken, expectedStateToken);
    assert.equal(page.complete, true);
    records.push(...page.records);
    if (!page.hasMore) return records;
    assert.ok(page.nextAfterSeq !== undefined && page.nextAfterSeq > afterSeq);
    afterSeq = page.nextAfterSeq;
  }
  assert.fail("evidence exceeded its bounded pagination proof");
}

function assertRequestProof(records, origin) {
  const observed = new Set(records.map((record) => `${record.method} ${record.url.path}`));
  for (const expected of [
    "GET /start",
    "GET /login",
    "POST /authenticate",
    "GET /handoff",
    "GET /dashboard",
    "GET /api/profile",
    "GET /redirect-next",
    "GET /second",
    "GET /api/details",
  ]) {
    assert.ok(observed.has(expected), `request audit omitted ${expected}`);
  }
  for (const record of records) assert.equal(record.url.origin, origin);
  const profileRequest = records.find((record) => record.url.path === "/api/profile");
  assert.ok(profileRequest);
  assert.deepEqual(profileRequest.url.queryKeys, ["nonce"]);
  const loginRequest = records.find(
    (record) => record.method === "POST" && record.url.path === "/authenticate",
  );
  assert.ok(loginRequest && loginRequest.bodyBytes > 0n);
}

function assertEvidenceProof(records, requests) {
  assertRequestEvidenceLifecycles(records, requests);

  const kinds = new Set(records.map((record) => record.kind));
  for (const kind of [
    "request_started",
    "route_decided",
    "response_headers",
    "redirect",
    "request_completed",
    "navigation_started",
    "navigation_committed",
    "same_document_history_changed",
    "settlement_terminal",
  ]) {
    assert.ok(kinds.has(kind), `session evidence omitted ${kind}`);
  }
  assert.ok(
    records.some(
      (record) => record.kind === "route_decided" && record.decision === "live",
    ),
    "session evidence did not classify loopback traffic as live",
  );

  const request = (method, path) => {
    const matches = requests.filter(
      (record) => record.method === method && record.url.path === path,
    );
    assert.equal(matches.length, 1, `expected exactly one ${method} ${path} request`);
    return matches[0];
  };
  const expectedResponses = [
    ["GET", "/start", 302],
    ["GET", "/login", 200],
    ["POST", "/authenticate", 200],
    ["GET", "/handoff", 302],
    ["GET", "/dashboard", 200],
    ["GET", "/api/profile", 200],
    ["GET", "/redirect-next", 302],
    ["GET", "/second", 200],
    ["GET", "/api/details", 200],
  ];
  assert.equal(
    requests.length,
    expectedResponses.length,
    "request audit recorded an unexpected primary-session request",
  );
  for (const [method, path, status] of expectedResponses) {
    const expectedRequest = request(method, path);
    const responseHeaders = records.filter(
      (record) =>
        record.kind === "response_headers" && record.requestId === expectedRequest.requestId,
    );
    assert.equal(
      responseHeaders.length,
      1,
      `evidence did not contain exactly one response for ${method} ${path}`,
    );
    assert.equal(
      responseHeaders[0].status,
      status,
      `evidence recorded the wrong response status for ${method} ${path}`,
    );
    assert.equal(
      records.filter(
        (record) =>
          record.kind === "request_completed" && record.requestId === expectedRequest.requestId,
      ).length,
      1,
      `evidence did not complete ${method} ${path} exactly once`,
    );
  }

  const expectedRedirects = [
    ["GET", "/start", "GET", "/login"],
    ["GET", "/handoff", "GET", "/dashboard"],
    ["GET", "/redirect-next", "GET", "/second"],
  ];
  assert.equal(
    requests.filter((record) => record.redirectParentId !== undefined).length,
    expectedRedirects.length,
    "request audit recorded an unexpected redirect-parent cardinality",
  );
  assert.equal(
    records.filter((record) => record.kind === "redirect").length,
    expectedRedirects.length,
    "session evidence recorded an unexpected redirect cardinality",
  );
  for (const [parentMethod, parentPath, childMethod, childPath] of expectedRedirects) {
    const parent = request(parentMethod, parentPath);
    const child = request(childMethod, childPath);
    assert.equal(
      child.redirectParentId,
      parent.requestId,
      `${childMethod} ${childPath} did not retain its redirect parent`,
    );
    assert.equal(
      records.filter(
        (record) =>
          record.kind === "redirect" &&
          record.requestId === parent.requestId &&
          record.nextRequestId === child.requestId,
      ).length,
      1,
      `evidence did not correlate ${parentPath} -> ${childPath}`,
    );
  }

  const profileRequest = request("GET", "/api/profile");
  assert.ok(
    records.some(
      (record) =>
        record.kind === "route_decided" &&
        record.requestId === profileRequest.requestId &&
        record.decision === "fixture_fulfill",
    ),
    "session evidence did not bind deterministic fixture fulfillment to /api/profile",
  );

  const expectedNavigationIds = [0n, 1n, 2n];
  assert.deepEqual(
    new Set(
      records
        .filter((record) => record.kind === "settlement_terminal")
        .map((record) => record.navigationId),
    ),
    new Set(expectedNavigationIds),
    "settlement evidence referenced an unexpected navigation",
  );
  assert.equal(
    records.filter((record) => record.kind === "navigation_failed").length,
    0,
    "session evidence recorded an unexpected navigation failure",
  );
  assert.deepEqual(
    new Set(
      records
        .filter((record) => record.kind === "navigation_started")
        .map((record) => record.navigationId),
    ),
    new Set(expectedNavigationIds),
    "navigation-start evidence did not cover the exact document sequence",
  );
  assert.equal(
    records.filter((record) => record.kind === "navigation_committed").length,
    expectedNavigationIds.length,
    "navigation-commit evidence had the wrong cardinality",
  );
  for (const navigationId of expectedNavigationIds) {
    const started = records.filter(
      (record) =>
        record.kind === "navigation_started" && record.navigationId === navigationId,
    );
    const committed = records.filter(
      (record) =>
        record.kind === "navigation_committed" && record.navigationId === navigationId,
    );
    const settlements = records.filter(
      (record) =>
        record.kind === "settlement_terminal" && record.navigationId === navigationId,
    );
    assert.ok(
      started.length >= 1,
      `navigation ${navigationId} did not start`,
    );
    assert.equal(
      committed.length,
      1,
      `navigation ${navigationId} did not commit exactly once`,
    );
    assert.ok(
      started.every((record) => record.seq < committed[0].seq),
      `navigation ${navigationId} committed before every start was observed`,
    );
    assert.ok(
      settlements.length >= 1,
      `session evidence omitted terminal settlement for navigation ${navigationId}`,
    );
    assert.ok(
      settlements.every((record) => committed[0].seq < record.seq),
      `navigation ${navigationId} settled before it committed`,
    );
  }
  for (const navigationId of [1n, 2n]) {
    const history = records.filter(
      (record) =>
        record.kind === "same_document_history_changed" &&
        record.navigationId === navigationId,
    );
    assert.equal(
      history.length,
      1,
      `history evidence did not bind exactly one change to navigation ${navigationId}`,
    );
    const committed = records.find(
      (record) =>
        record.kind === "navigation_committed" && record.navigationId === navigationId,
    );
    assert.ok(
      committed.seq < history[0].seq,
      `navigation ${navigationId} changed history before it committed`,
    );
    assert.ok(
      records.some(
        (record) =>
          record.kind === "settlement_terminal" &&
          record.navigationId === navigationId &&
          history[0].seq < record.seq,
      ),
      `navigation ${navigationId} did not settle after its history change`,
    );
  }
  assert.equal(
    records.filter((record) => record.kind === "same_document_history_changed").length,
    2,
    "history evidence had the wrong cardinality",
  );
  assert.ok(
    records.filter((record) => record.kind === "settlement_terminal").length >= 3,
    "session evidence omitted a terminal settlement",
  );
}

function assertRequestEvidenceLifecycles(records, requests) {
  const requestById = new Map(requests.map((request) => [request.requestId, request]));
  assert.equal(requestById.size, requests.length, "request audit reused an opaque request ID");

  for (let index = 1; index < requests.length; index += 1) {
    assert.ok(
      requests[index].seq > requests[index - 1].seq,
      "request audit sequence was not strictly monotonic",
    );
  }

  for (let index = 1; index < records.length; index += 1) {
    assert.ok(
      records[index].seq > records[index - 1].seq,
      "session evidence sequence was not strictly monotonic",
    );
  }

  for (const record of records) {
    if ("requestId" in record) {
      assert.ok(
        requestById.has(record.requestId),
        `evidence referenced unknown request ID ${record.requestId}`,
      );
    }
    if ("nextRequestId" in record) {
      assert.ok(
        requestById.has(record.nextRequestId),
        `redirect evidence referenced unknown successor request ID ${record.nextRequestId}`,
      );
    }
  }

  for (const request of requests) {
    const one = (kind) => {
      const matches = records.filter(
        (record) => record.kind === kind && record.requestId === request.requestId,
      );
      assert.equal(
        matches.length,
        1,
        `request ${request.requestId} did not have exactly one ${kind} event`,
      );
      return matches[0];
    };
    const started = one("request_started");
    const routed = one("route_decided");
    const headers = one("response_headers");
    const completed = one("request_completed");
    assert.equal(
      request.seq,
      started.seq,
      `request ${request.requestId} did not retain its request_started sequence`,
    );
    assert.ok(
      started.seq < routed.seq && routed.seq < headers.seq && headers.seq < completed.seq,
      `request ${request.requestId} had a non-monotonic lifecycle`,
    );
  }
  assert.equal(
    records.filter((record) => record.kind === "request_failed").length,
    0,
    "session evidence recorded an unexpected request failure",
  );

  const expectedRedirects = requests
    .filter((request) => request.redirectParentId !== undefined)
    .map((request) => ({
      requestId: request.redirectParentId,
      nextRequestId: request.requestId,
    }));
  for (const redirect of expectedRedirects)
    assert.ok(requestById.has(redirect.requestId), "request audit used an unknown redirect parent");
  const observedRedirects = records.filter((record) => record.kind === "redirect");
  assert.equal(
    observedRedirects.length,
    expectedRedirects.length,
    "redirect evidence cardinality did not match request parentage",
  );
  for (const expected of expectedRedirects) {
    const matches = observedRedirects.filter(
      (record) =>
        record.requestId === expected.requestId &&
        record.nextRequestId === expected.nextRequestId,
    );
    assert.equal(
      matches.length,
      1,
      `redirect evidence did not link ${expected.requestId} -> ${expected.nextRequestId} exactly once`,
    );
    const parentRoute = records.find(
      (record) => record.kind === "route_decided" && record.requestId === expected.requestId,
    );
    const successorStart = records.find(
      (record) => record.kind === "request_started" && record.requestId === expected.nextRequestId,
    );
    assert.ok(
      parentRoute.seq < matches[0].seq && matches[0].seq < successorStart.seq,
      `redirect ${expected.requestId} -> ${expected.nextRequestId} violated route-before-successor order`,
    );
  }
}

function countEvidenceKinds(records) {
  const counts = {};
  for (const record of records) counts[record.kind] = (counts[record.kind] ?? 0) + 1;
  return counts;
}

function assertRequestEvidenceLifecycleRegression() {
  const requestsForChildStart = (childSeq) => [
    { seq: 1n, requestId: "parent" },
    { seq: childSeq, requestId: "child", redirectParentId: "parent" },
  ];
  const terminalFirstRequests = requestsForChildStart(6n);
  const successorFirstRequests = requestsForChildStart(4n);
  const evidence = (seq, event) => ({ seq: BigInt(seq), ...event });
  const terminalFirst = [
    evidence(1, { kind: "request_started", requestId: "parent" }),
    evidence(2, { kind: "route_decided", requestId: "parent", decision: "live" }),
    evidence(3, { kind: "response_headers", requestId: "parent", status: 302 }),
    evidence(4, { kind: "request_completed", requestId: "parent" }),
    evidence(5, { kind: "redirect", requestId: "parent", nextRequestId: "child" }),
    evidence(6, { kind: "request_started", requestId: "child" }),
    evidence(7, { kind: "route_decided", requestId: "child", decision: "live" }),
    evidence(8, { kind: "response_headers", requestId: "child", status: 200 }),
    evidence(9, { kind: "request_completed", requestId: "child" }),
  ];
  const successorFirst = [
    evidence(1, { kind: "request_started", requestId: "parent" }),
    evidence(2, { kind: "route_decided", requestId: "parent", decision: "live" }),
    evidence(3, { kind: "redirect", requestId: "parent", nextRequestId: "child" }),
    evidence(4, { kind: "request_started", requestId: "child" }),
    evidence(5, { kind: "route_decided", requestId: "child", decision: "live" }),
    evidence(6, { kind: "response_headers", requestId: "child", status: 200 }),
    evidence(7, { kind: "request_completed", requestId: "child" }),
    evidence(8, { kind: "response_headers", requestId: "parent", status: 302 }),
    evidence(9, { kind: "request_completed", requestId: "parent" }),
  ];

  assert.doesNotThrow(
    () => assertRequestEvidenceLifecycles(terminalFirst, terminalFirstRequests),
    "request evidence rejected a legal predecessor-terminal-first redirect",
  );
  assert.doesNotThrow(
    () => assertRequestEvidenceLifecycles(successorFirst, successorFirstRequests),
    "request evidence rejected a legal successor-first redirect",
  );
  assert.throws(
    () => assertRequestEvidenceLifecycles(terminalFirst, requestsForChildStart(4n)),
    /did not retain its request_started sequence/u,
    "request evidence accepted a request record with the wrong start sequence",
  );
  const redirectBeforeRoute = [
    terminalFirst[0],
    { ...terminalFirst[4], seq: 2n },
    { ...terminalFirst[1], seq: 3n },
    { ...terminalFirst[2], seq: 4n },
    { ...terminalFirst[3], seq: 5n },
    ...terminalFirst.slice(5),
  ];
  assert.throws(
    () => assertRequestEvidenceLifecycles(redirectBeforeRoute, terminalFirstRequests),
    /violated route-before-successor order/u,
    "request evidence accepted a redirect before its parent route decision",
  );
}

function assertAuditRedaction(requests, evidence) {
  assertProofRedacted(stringifyBigInts({ requests, evidence }));
}

function assertProofRedacted(json) {
  for (const secret of [
    SYNTHETIC_EMAIL,
    SYNTHETIC_PASSWORD,
    AUTH_COOKIE_VALUE,
    MUTATED_AUTH_COOKIE_VALUE,
    "fixture-secret-ticket",
    "fixture-secret-profile",
    "fixture-secret-navigation",
    "fixture-secret-final",
    "fixture-secret-detail",
    "fixture-secret-restored-query",
  ]) {
    assert.equal(json.includes(secret), false, `public session proof leaked ${secret}`);
  }
}

async function closeSessionAndAssertProcessExit(runtime, session) {
  const pid = runtime.pid;
  assert.ok(pid !== undefined);
  await session.close();
  assert.throws(
    () => process.kill(pid, 0),
    (error) =>
      typeof error === "object" &&
      error !== null &&
      "code" in error &&
      error.code === "ESRCH",
  );
}

async function startSessionNorthStarServer() {
  const requests = [];
  const logins = [];
  const errors = [];
  const server = createServer((request, response) => {
    void serveRequest(request, response, requests, logins).catch((error) => {
      errors.push(error);
      if (!response.headersSent) {
        writeResponse(
          response,
          500,
          "text/plain; charset=utf-8",
          Buffer.from("fixture server error\n", "utf8"),
        );
        return;
      }
      response.destroy(error instanceof Error ? error : undefined);
    });
  });
  await new Promise((resolve, reject) => {
    const onError = (error) => reject(error);
    server.once("error", onError);
    server.listen(0, "127.0.0.1", () => {
      server.off("error", onError);
      resolve();
    });
  });
  const address = server.address();
  assert.ok(address && typeof address === "object");
  const origin = `http://127.0.0.1:${address.port}`;
  return {
    origin,
    requests,
    logins,
    errors,
    url: (path) => new URL(path, origin).href,
    close: () =>
      new Promise((resolve, reject) => {
        server.close((error) => (error === undefined ? resolve() : reject(error)));
      }),
  };
}

async function serveRequest(request, response, requests, logins) {
  const url = new URL(request.url ?? "/", "http://127.0.0.1");
  const method = request.method ?? "";
  const cookie = request.headers.cookie ?? "";
  requests.push({ method, pathname: url.pathname, search: url.search, cookie });
  if (method === "GET" && url.pathname === "/start") {
    redirect(response, "/login?from=start");
    return;
  }
  if (method === "GET" && url.pathname === "/login") {
    writeHtml(response, FIXTURES.login);
    return;
  }
  if (method === "POST" && url.pathname === "/authenticate") {
    const contentType = request.headers["content-type"] ?? "";
    assert.equal(contentType, "application/json");
    const body = JSON.parse((await readBoundedBody(request, 16 * 1024)).toString("utf8"));
    assert.ok(body && typeof body === "object" && !Array.isArray(body));
    assert.equal(typeof body.email, "string");
    assert.equal(typeof body.password, "string");
    assert.equal(typeof body.terms, "boolean");
    assert.ok(
      Array.isArray(body.regions) &&
        body.regions.every((region) => typeof region === "string"),
    );
    logins.push({
      method: "POST",
      pathname: "/authenticate",
      contentType,
      email: body.email,
      password: body.password,
      terms: body.terms,
      regions: [...body.regions],
    });
    writeResponse(
      response,
      200,
      "application/json",
      Buffer.from(JSON.stringify({ role: ROLE }), "utf8"),
      { "set-cookie": `stasis-auth=${AUTH_COOKIE_VALUE}; Path=/; HttpOnly; SameSite=Lax` },
    );
    return;
  }
  if (method === "GET" && url.pathname === "/handoff") {
    assertAuthenticated(cookie, "/handoff");
    redirect(response, "/dashboard?source=handoff");
    return;
  }
  if (method === "GET" && url.pathname === "/dashboard") {
    assertAuthenticated(cookie, "/dashboard");
    writeHtml(response, FIXTURES.dashboard);
    return;
  }
  if (method === "GET" && url.pathname === "/api/profile") {
    assertAuthenticated(cookie, "/api/profile");
    writeResponse(
      response,
      200,
      "application/json",
      Buffer.from(JSON.stringify({ role: ROLE }), "utf8"),
    );
    return;
  }
  if (method === "GET" && url.pathname === "/redirect-next") {
    assertAuthenticated(cookie, "/redirect-next");
    redirect(response, "/second?stage=2&secret=fixture-secret-final");
    return;
  }
  if (method === "GET" && url.pathname === "/second") {
    assertAuthenticated(cookie, "/second");
    writeHtml(response, FIXTURES.second);
    return;
  }
  if (method === "GET" && url.pathname === "/api/details") {
    assertAuthenticated(cookie, "/api/details");
    writeResponse(
      response,
      200,
      "application/json",
      Buffer.from(JSON.stringify({ value: DETAIL }), "utf8"),
    );
    return;
  }
  if (method === "GET" && url.pathname === "/restored") {
    assertAuthenticated(cookie, "/restored", MUTATED_AUTH_COOKIE_VALUE);
    writeHtml(response, FIXTURES.restored);
    return;
  }
  if (method === "GET" && url.pathname === "/favicon.ico") {
    writeResponse(response, 204, "image/x-icon", Buffer.alloc(0));
    return;
  }
  writeResponse(
    response,
    404,
    "text/plain; charset=utf-8",
    Buffer.from("not found\n", "utf8"),
  );
}

function assertAuthenticated(cookie, pathname, expectedValue = AUTH_COOKIE_VALUE) {
  assert.match(
    cookie,
    new RegExp(`(?:^|;\\s*)stasis-auth=${expectedValue}(?:;|$)`, "u"),
    `${pathname} requires the fixture session cookie`,
  );
}

function redirect(response, location) {
  response.writeHead(302, {
    location,
    "content-length": "0",
    "cache-control": "no-store",
    connection: "close",
  });
  response.end();
}

function writeHtml(response, body) {
  writeResponse(response, 200, "text/html; charset=utf-8", body);
}

function writeResponse(response, status, contentType, body, extraHeaders = {}) {
  response.writeHead(status, {
    "content-type": contentType,
    "content-length": body.byteLength.toString(),
    "cache-control": "no-store",
    connection: "close",
    ...extraHeaders,
  });
  response.end(body);
}

async function readBoundedBody(request, maxBytes) {
  const chunks = [];
  let bytes = 0;
  for await (const chunk of request) {
    const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    bytes += buffer.length;
    assert.ok(bytes <= maxBytes, `request body exceeded ${maxBytes} bytes`);
    chunks.push(buffer);
  }
  return Buffer.concat(chunks, bytes);
}

function stringifyBigInts(value) {
  return JSON.stringify(value, (_key, item) =>
    typeof item === "bigint" ? item.toString() : item,
  );
}
