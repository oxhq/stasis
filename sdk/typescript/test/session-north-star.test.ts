import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import {
  createServer,
  type IncomingMessage,
  type Server,
  type ServerResponse,
} from "node:http";
import type { AddressInfo } from "node:net";
import test from "node:test";

import {
  CONTROLLED_WEB_SESSION_V1_PROFILE,
  StasisProtocolError,
  launch,
  type DocumentStateToken,
  type Runtime,
  type Session,
  type SessionEvidenceRecord,
  type SessionNetworkOptions,
  type SessionRequestRecord,
  type SessionState,
} from "../src/index.js";

const NATIVE_BINARY = process.env.STASIS_SESSION_NORTH_STAR_BINARY;
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
  persistentWork: "report" as const,
  maxVirtualTimeNs: 5_000_000_000n,
  maxControlTurns: 100_000n,
  wallIoTimeoutNs: 10_000_000_000n,
};
const SELECTED_REGIONS = ["north", "west"] as const;
const INTERACTION_FINGERPRINT = {
  activation: "activated=1",
  focus: "email=1",
  check: "input=2,change=2,checked=false",
  select: "input=1,change=1,values=north|west",
} as const;
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
] as const;
const FIXTURES = {
  login: await readFile(
    new URL("./fixtures/session-north-star/login.html", import.meta.url),
  ),
  dashboard: await readFile(
    new URL("./fixtures/session-north-star/dashboard.html", import.meta.url),
  ),
  second: await readFile(
    new URL("./fixtures/session-north-star/second.html", import.meta.url),
  ),
  restored: await readFile(
    new URL("./fixtures/session-north-star/restored.html", import.meta.url),
  ),
  automationAdversarial: await readFile(
    new URL(
      "./fixtures/session-north-star/automation-adversarial.html",
      import.meta.url,
    ),
  ),
  automationHandlerGrowth: await readFile(
    new URL(
      "./fixtures/session-north-star/automation-handler-growth.html",
      import.meta.url,
    ),
  ),
  replacementA: await readFile(
    new URL("./fixtures/session-north-star/replacement-a.html", import.meta.url),
  ),
  replacementB: await readFile(
    new URL("./fixtures/session-north-star/replacement-b.html", import.meta.url),
  ),
  replacementC: await readFile(
    new URL("./fixtures/session-north-star/replacement-c.html", import.meta.url),
  ),
  nativeFormSource: await readFile(
    new URL("./fixtures/session-north-star/native-form-source.html", import.meta.url),
  ),
  nativeFormDestination: await readFile(
    new URL(
      "./fixtures/session-north-star/native-form-destination.html",
      import.meta.url,
    ),
  ),
  syncActionSource: await readFile(
    new URL("./fixtures/session-north-star/sync-action-source.html", import.meta.url),
  ),
  syncActionDestination: await readFile(
    new URL(
      "./fixtures/session-north-star/sync-action-destination.html",
      import.meta.url,
    ),
  ),
  syncHistorySource: await readFile(
    new URL("./fixtures/session-north-star/sync-history-source.html", import.meta.url),
  ),
  crossOriginSource: await readFile(
    new URL("./fixtures/session-north-star/cross-origin-source.html", import.meta.url),
  ),
  crossOriginTarget: await readFile(
    new URL("./fixtures/session-north-star/cross-origin-target.html", import.meta.url),
  ),
  crossOriginReturn: await readFile(
    new URL("./fixtures/session-north-star/cross-origin-return.html", import.meta.url),
  ),
  networkAbort: await readFile(
    new URL("./fixtures/session-north-star/network-abort.html", import.meta.url),
  ),
  historyTraversalUnsupported: await readFile(
    new URL(
      "./fixtures/session-north-star/history-traversal-unsupported.html",
      import.meta.url,
    ),
  ),
};

interface RequestObservation {
  readonly method: string;
  readonly pathname: string;
  readonly search: string;
  readonly cookie: string;
}

interface LoginObservation {
  readonly method: "POST";
  readonly pathname: "/authenticate";
  readonly contentType: string;
  readonly email: string;
  readonly password: string;
  readonly terms: boolean;
  readonly regions: string[];
}

interface SessionNorthStarServer {
  readonly origin: string;
  readonly requests: RequestObservation[];
  readonly logins: LoginObservation[];
  readonly errors: unknown[];
  url(path: string): string;
  close(): Promise<void>;
}

function sessionNetwork(server: SessionNorthStarServer): SessionNetworkOptions {
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

test(
  "real native v0.2 session crosses documents, preserves state, and exposes redacted audit proof",
  {
    skip:
      NATIVE_BINARY === undefined
        ? "set STASIS_SESSION_NORTH_STAR_BINARY to the v0.2 stasis executable"
        : false,
    timeout: 240_000,
  },
  async () => {
    assert.ok(NATIVE_BINARY, "STASIS_SESSION_NORTH_STAR_BINARY must be non-empty");
    const server = await startSessionNorthStarServer();
    try {
      const exportedState = await executePrimarySession(server, NATIVE_BINARY);
      await executeRestoredSession(server, NATIVE_BINARY, exportedState);
      await proveOneSettleCanCrossRepeatedReplacements(server, NATIVE_BINARY);
      await proveNativeFormDefaultDefersNavigation(server, NATIVE_BINARY);
      await proveSynchronousActionReplacementCarriesAuthority(server, NATIVE_BINARY);
      await proveSynchronousHistoryActionCarriesAuthority(server, NATIVE_BINARY);
      await proveCrossOriginReplacementAndIsolation(server, NATIVE_BINARY);
      await proveHistoryTraversalIsTypedUnsupported(server, NATIVE_BINARY);
      await proveFixtureAbortIsObservable(server, NATIVE_BINARY);
      await proveFixtureOnlyMissIsSticky(server, NATIVE_BINARY);

      if (server.errors.length > 0) throw server.errors[0];
      assert.deepEqual(server.logins, [
        {
          method: "POST",
          pathname: "/authenticate",
          contentType: "application/json",
          email: SYNTHETIC_EMAIL,
          password: SYNTHETIC_PASSWORD,
          terms: false,
          regions: [...SELECTED_REGIONS],
        },
      ]);
      for (const pathname of ["/dashboard", "/second", "/api/details"]) {
        const observation = server.requests.find(
          (request) => request.pathname === pathname,
        );
        assert.ok(observation, `fixture server did not observe ${pathname}`);
        assert.match(
          observation.cookie,
          new RegExp(`(?:^|;\\s*)stasis-auth=${AUTH_COOKIE_VALUE}(?:;|$)`, "u"),
          `${pathname} did not receive the imported session cookie`,
        );
      }
      const restoredObservation = server.requests.find(
        (request) => request.pathname === "/restored",
      );
      assert.ok(restoredObservation, "fixture server did not observe /restored");
      assert.match(
        restoredObservation.cookie,
        new RegExp(
          `(?:^|;\\s*)stasis-auth=${MUTATED_AUTH_COOKIE_VALUE}(?:;|$)`,
          "u",
        ),
        "/restored did not receive the mutated imported session cookie",
      );
      assert.equal(
        server.requests.some((request) => request.pathname === "/api/profile"),
        false,
        "the deterministic /api/profile fixture unexpectedly reached the live server",
      );
    } finally {
      await server.close();
    }
  },
);

test(
  "real native v0.2 automation bounds attributes and select work across synchronous handlers",
  {
    skip:
      NATIVE_BINARY === undefined
        ? "set STASIS_SESSION_NORTH_STAR_BINARY to the v0.2 stasis executable"
        : false,
    timeout: 120_000,
  },
  async () => {
    assert.ok(NATIVE_BINARY, "STASIS_SESSION_NORTH_STAR_BINARY must be non-empty");
    const server = await startSessionNorthStarServer();
    try {
      const runtime = await launch({ executablePath: NATIVE_BINARY });
      try {
        const session = await runtime.openSession(server.url("/automation-adversarial"), {
          timeoutMs: 90_000,
        });
        const settled = await session.settle(session.stateToken, SETTLE_POLICY);
        assert.equal(settled.outcome, "quiescent");
        const nodeCount = await session.text("#document-node-count", settled.stateToken);
        assert.ok(
          Number(nodeCount.value) >= 10_000,
          `practical action fixture contained only ${nodeCount.value} DOM nodes`,
        );

        await assert.rejects(
          session.query("[data-huge*='needle']", settled.stateToken),
          assertNonMutatingAutomationRejection("unsupported_selector"),
        );
        await assert.rejects(
          session.query("[data-huge='short']", settled.stateToken),
          assertNonMutatingAutomationRejection(
            "automation_selector_evaluation_limit_exceeded",
          ),
        );
        const equality = await session.query("[data-small='short']", settled.stateToken);
        assert.equal(equality.count, 1n);
        const presence = await session.query("[data-huge]", equality.stateToken);
        assert.equal(presence.count, 1n);

        const bounded = await session.select(
          "#many-children",
          ["chosen"],
          presence.stateToken,
        );
        assert.deepEqual(bounded.values, ["chosen"]);

        const replaced = await session.select(
          "#replace-options",
          ["old-b"],
          bounded.stateToken,
        );
        assert.equal(replaced.changed, true);
        assert.deepEqual(
          replaced.values,
          ["replacement"],
          "select returned pre-event detached option values",
        );

        const largeForm = await session.select(
          "#large-form-select",
          ["large-selected"],
          replaced.stateToken,
        );
        assert.equal(largeForm.changed, true);
        assert.deepEqual(largeForm.values, ["large-selected"]);

        let preflightToken: DocumentStateToken = largeForm.stateToken;
        await assert.rejects(
          session.fill("#huge-old-fill", "replacement", preflightToken),
          assertNonMutatingAutomationRejection("automation_output_limit_exceeded"),
        );
        const fillStatus = await session.text("#fill-preflight-status", preflightToken);
        assert.equal(fillStatus.value, "untouched", "rejected fill dispatched input");
        preflightToken = fillStatus.stateToken;

        for (const [operation, statusSelector] of [
          [
            () => session.check("#large-radio", preflightToken),
            "#radio-preflight-status",
          ],
          [
            () => session.activate("#large-reset", preflightToken),
            "#reset-preflight-status",
          ],
        ] as const) {
          await assert.rejects(
            operation(),
            assertNonMutatingAutomationRejection(
              "automation_dom_traversal_limit_exceeded",
            ),
          );
          const status = await session.text(statusSelector, preflightToken);
          assert.equal(
            status.value,
            "untouched",
            `${statusSelector} observed an event after preflight rejection`,
          );
          preflightToken = status.stateToken;
        }

        await assert.rejects(
          session.submit("#large-reset-form", preflightToken),
          assertNonMutatingAutomationRejection(
            "automation_dom_traversal_limit_exceeded",
          ),
        );
        const submitWorkStatus = await session.text(
          "#submit-work-preflight-status",
          preflightToken,
        );
        assert.equal(
          submitWorkStatus.value,
          "untouched",
          "rejected shallow large-form submit dispatched submit",
        );
        const invalidWorkStatus = await session.text(
          "#invalid-work-preflight-status",
          submitWorkStatus.stateToken,
        );
        assert.equal(
          invalidWorkStatus.value,
          "untouched",
          "rejected shallow large-form submit dispatched invalid",
        );
        preflightToken = invalidWorkStatus.stateToken;

        await assert.rejects(
          session.submit("#large-form", preflightToken),
          assertNonMutatingAutomationRejection("automation_output_limit_exceeded"),
        );
        const submitStatus = await session.text(
          "#submit-preflight-status",
          preflightToken,
        );
        assert.equal(
          submitStatus.value,
          "untouched",
          "rejected submit dispatched submit",
        );
        preflightToken = submitStatus.stateToken;

        const focused = await session.focus("#large-focus", preflightToken);
        assert.equal(focused.focused, true);
        const focusStatus = await session.text(
          "#focus-preflight-status",
          focused.stateToken,
        );
        assert.equal(
          focusStatus.value,
          "mutated",
          "ordinary focus was coupled to unrelated page size",
        );

        const activated = await session.activate(
          "#large-activate",
          focusStatus.stateToken,
        );
        const activateStatus = await session.text(
          "#activate-preflight-status",
          activated.stateToken,
        );
        assert.equal(
          activateStatus.value,
          "mutated",
          "plain button activation was coupled to unrelated page size",
        );

        const linked = await session.activate("#large-link", activateStatus.stateToken);
        const linkStatus = await session.text(
          "#link-preflight-status",
          linked.stateToken,
        );
        assert.equal(
          linkStatus.value,
          "mutated",
          "plain link activation was coupled to unrelated page size",
        );
      } finally {
        await runtime.close().catch(() => undefined);
      }

      const fatalRuntime = await launch({ executablePath: NATIVE_BINARY });
      let terminal: StasisProtocolError | undefined;
      try {
        const session = await fatalRuntime.openSession(
          server.url("/automation-handler-growth"),
        );
        const settled = await session.settle(session.stateToken, SETTLE_POLICY);
        await assert.rejects(
          session.select("#grow-value", ["grow"], settled.stateToken),
          (error) => {
            assert.ok(error instanceof StasisProtocolError);
            terminal = error;
            assert.equal(error.code, "outcome_indeterminate");
            assert.equal(error.fatal, true);
            assert.equal(error.stateEffect, "indeterminate");
            return true;
          },
        );
        await assert.rejects(session.pending(), (error) => error === terminal);
      } finally {
        await fatalRuntime.close().catch(() => undefined);
      }

      if (server.errors.length > 0) throw server.errors[0];
    } finally {
      await server.close();
    }
  },
);

async function proveOneSettleCanCrossRepeatedReplacements(
  server: SessionNorthStarServer,
  executablePath: string,
): Promise<void> {
  const runtime = await launch({ executablePath });
  let closed = false;
  try {
    const session = await runtime.openSession(server.url("/replacement-a"), {
      network: { mode: "live", routes: [] },
    });
    const initial = await session.settle(session.stateToken, SETTLE_POLICY);
    assert.equal(initial.outcome, "quiescent");

    const activated = await session.activate("#to-replacement-b", initial.stateToken);
    assert.equal(
      await session
        .text("#to-replacement-b", activated.stateToken)
        .then(result => result.value),
      "Continue to B",
      "native anchor activation did not preserve its source-document action authority",
    );
    const final = await session.settle(activated.stateToken, SETTLE_POLICY);
    assert.equal(final.outcome, "quiescent");
    assert.equal(
      await session.text("#replacement-status", final.stateToken).then(result => result.value),
      "replacement-c-ready",
      "one settle did not cross both A-to-B and B-to-C replacements",
    );

    const evidence = await session.evidence({ limit: 128 });
    const started = evidence.records
      .filter((record) => record.kind === "navigation_started")
      .map((record) => record.navigationId);
    const committed = evidence.records
      .filter((record) => record.kind === "navigation_committed")
      .map((record) => record.navigationId);
    assert.deepEqual(started, [0n, 1n, 2n]);
    assert.deepEqual(committed, [0n, 1n, 2n]);

    await closeSessionAndAssertProcessExit(runtime, session);
    closed = true;
  } finally {
    if (!closed) await runtime.close();
  }
}

async function proveNativeFormDefaultDefersNavigation(
  server: SessionNorthStarServer,
  executablePath: string,
): Promise<void> {
  const runtime = await launch({ executablePath });
  let closed = false;
  try {
    const session = await runtime.openSession(server.url("/native-form-source"), {
      network: { mode: "live", routes: [] },
    });
    const initial = await session.settle(session.stateToken, SETTLE_POLICY);
    assert.equal(initial.outcome, "quiescent");

    const submitted = await session.submit("#native-form", initial.stateToken);
    assert.equal(submitted.submitted, true);
    assert.notEqual(
      submitted.stateToken,
      initial.stateToken,
      "native form submission did not return fresh source-document authority",
    );
    assert.equal(
      await session
        .text("#native-form-source-status", submitted.stateToken)
        .then(result => result.value),
      "native-form-source-ready",
      "native form submission did not preserve inspectable source-document authority",
    );

    const destination = await session.settle(submitted.stateToken, SETTLE_POLICY);
    assert.equal(destination.outcome, "quiescent");
    assert.equal(
      await session
        .text("#native-form-destination-status", destination.stateToken)
        .then(result => result.value),
      "native-form-destination-ready",
      "explicit settlement did not complete the native form navigation",
    );

    await closeSessionAndAssertProcessExit(runtime, session);
    closed = true;
  } finally {
    if (!closed) await runtime.close();
  }
}

async function proveSynchronousActionReplacementCarriesAuthority(
  server: SessionNorthStarServer,
  executablePath: string,
): Promise<void> {
  const runtime = await launch({ executablePath });
  let closed = false;
  const destinationRequestsBefore = server.requests.filter(
    request => request.pathname === "/sync-action-destination",
  ).length;
  try {
    const session = await runtime.openSession(server.url("/sync-action-source"), {
      network: { mode: "live", routes: [] },
    });
    const initial = await session.settle(session.stateToken, SETTLE_POLICY);
    assert.equal(initial.outcome, "quiescent");

    const activated = await session.activate("#sync-action", initial.stateToken);
    assert.deepEqual(Object.keys(activated).sort(), ["stateGeneration", "stateToken"]);
    assert.notEqual(
      activated.stateToken,
      initial.stateToken,
      "synchronous action replacement did not return fresh destination authority",
    );
    assert.equal(
      await session
        .text("#sync-action-destination-status", activated.stateToken)
        .then(result => result.value),
      "sync-action-destination-ready",
      "synchronous action replacement did not carry through controlled-ready",
    );
    await assert.rejects(
      session.text("#sync-action-source-status", initial.stateToken),
      assertStaleDocumentToken,
    );

    await closeSessionAndAssertProcessExit(runtime, session);
    closed = true;
  } finally {
    if (!closed) await runtime.close();
  }
  assert.equal(
    server.requests.filter(request => request.pathname === "/sync-action-destination").length,
    destinationRequestsBefore + 1,
    "synchronous navigation-producing action was replayed",
  );
}

async function proveSynchronousHistoryActionCarriesAuthority(
  server: SessionNorthStarServer,
  executablePath: string,
): Promise<void> {
  const runtime = await launch({ executablePath });
  let closed = false;
  try {
    const session = await runtime.openSession(server.url("/sync-history-source"), {
      network: { mode: "live", routes: [] },
    });
    const initial = await session.settle(session.stateToken, SETTLE_POLICY);
    assert.equal(initial.outcome, "quiescent");

    const activated = await session.activate("#sync-history-action", initial.stateToken);
    assert.deepEqual(Object.keys(activated).sort(), ["stateGeneration", "stateToken"]);
    assert.notEqual(
      activated.stateToken,
      initial.stateToken,
      "synchronous history action did not return fresh final-history authority",
    );
    assert.equal(
      await session
        .text("#sync-history-status", activated.stateToken)
        .then(result => result.value),
      "/sync-history-two|history-changes=2|activations=1",
      "synchronous history action did not return its final DOM/history state",
    );
    await assert.rejects(
      session.text("#sync-history-status", initial.stateToken),
      assertStaleDocumentToken,
    );
    const evidence = await session.evidence({ limit: 32 });
    assert.ok(
      evidence.records.some(record => record.kind === "same_document_history_changed"),
      "synchronous history action omitted its bounded history evidence",
    );

    await closeSessionAndAssertProcessExit(runtime, session);
    closed = true;
  } finally {
    if (!closed) await runtime.close();
  }
}

async function executePrimarySession(
  server: SessionNorthStarServer,
  executablePath: string,
): Promise<SessionState> {
  const runtime = await launch({ executablePath });
  let closed = false;
  try {
    assertV02Capabilities(runtime);
    const startUrl = server.url("/start");
    const loginUrl = server.url("/login?from=start");
    const session = await runtime.openSession(startUrl, {
      clock: {
        mode: "controlled",
        initialVirtualTimeNs: INITIAL_VIRTUAL_TIME_NS,
        unixTimeOriginNs: 0n,
      },
      network: sessionNetwork(server),
    });
    assert.equal(session.requestedUrl, startUrl);
    assert.equal(session.url, loginUrl);
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
    assert.equal(await session.text("#status", noAdvance.stateToken).then(result => result.value), "login-ready");

    const encodeStateBytes = (state: SessionState): Buffer => {
      const encoded = JSON.stringify(state, (_key, value: unknown) =>
        typeof value === "bigint" ? value.toString() : value,
      );
      if (encoded === undefined) throw new Error("session state did not encode");
      return Buffer.from(encoded, "utf8");
    };
    const beforeStorageLimit = await session.exportState();
    const beforeStorageLimitBytes = encodeStateBytes(beforeStorageLimit.state);
    const overflowAttempt = await session.activate(
      "#storage-overflow",
      noAdvance.stateToken,
    );
    assert.equal(
      await session
        .text("#storage-limit-result", overflowAttempt.stateToken)
        .then(result => result.value),
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
      await session
        .text("#storage-success-result", withinBudget.stateToken)
        .then(result => result.value),
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
      await session
        .text("#storage-success-result", cleanedStorage.stateToken)
        .then(result => result.value),
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
    await assert.rejects(
      session.text("#status", staleToken),
      assertStaleDocumentToken,
    );
    assert.equal(
      await session.text("#activation-events", activated.stateToken).then(result => result.value),
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
      await session.text("#focus-events", focused.stateToken).then(result => result.value),
      INTERACTION_FINGERPRINT.focus,
    );

    const nonfocused = await session.focus("#nonfocusable", focused.stateToken);
    assert.equal(nonfocused.focused, false);
    assert.equal(
      await session.text("#focus-events", nonfocused.stateToken).then(result => result.value),
      INTERACTION_FINGERPRINT.focus,
      "focusing a non-focusable element dispatched a focus event",
    );

    const emailFill = await session.fill("#email", SYNTHETIC_EMAIL, nonfocused.stateToken);
    assert.equal(
      await session.text("#input-events", emailFill.stateToken).then(result => result.value),
      "email=1,password=0",
    );

    const passwordFill = await session.fill(
      "#password",
      SYNTHETIC_PASSWORD,
      emailFill.stateToken,
    );
    assert.equal(
      await session.text("#input-events", passwordFill.stateToken).then(result => result.value),
      "email=1,password=1",
    );

    const checked = await session.check("#terms", passwordFill.stateToken);
    assert.deepEqual(
      { changed: checked.changed, checked: checked.checked },
      { changed: true, checked: true },
    );
    assert.equal(
      await session.text("#check-events", checked.stateToken).then(result => result.value),
      "input=1,change=1,checked=true",
    );
    const unchecked = await session.uncheck("#terms", checked.stateToken);
    assert.deepEqual(
      { changed: unchecked.changed, checked: unchecked.checked },
      { changed: true, checked: false },
    );
    assert.equal(
      await session.text("#check-events", unchecked.stateToken).then(result => result.value),
      INTERACTION_FINGERPRINT.check,
    );

    const initiallyCheckedRadio = await session.check("#plan-basic", unchecked.stateToken);
    assert.deepEqual(
      { changed: initiallyCheckedRadio.changed, checked: initiallyCheckedRadio.checked },
      { changed: false, checked: true },
    );
    assert.equal(
      await session
        .text("#radio-events", initiallyCheckedRadio.stateToken)
        .then(result => result.value),
      "basic-input=0,basic-change=0,pro-input=0,pro-change=0,basic=true,pro=false",
    );
    const proChecked = await session.check("#plan-pro", initiallyCheckedRadio.stateToken);
    assert.deepEqual(
      { changed: proChecked.changed, checked: proChecked.checked },
      { changed: true, checked: true },
    );
    assert.equal(
      await session.text("#radio-events", proChecked.stateToken).then(result => result.value),
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
      await session
        .text("#radio-events", proCheckedAgain.stateToken)
        .then(result => result.value),
      "basic-input=0,basic-change=0,pro-input=1,pro-change=1,basic=false,pro=true",
      "rejecting radio uncheck changed the group or dispatched events",
    );
    const basicChecked = await session.check("#plan-basic", proCheckedAgain.stateToken);
    assert.deepEqual(
      { changed: basicChecked.changed, checked: basicChecked.checked },
      { changed: true, checked: true },
    );
    assert.equal(
      await session.text("#radio-events", basicChecked.stateToken).then(result => result.value),
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
      await session
        .text("#single-select-events", basicChecked.stateToken)
        .then(result => result.value),
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
      await session
        .text("#single-select-events", primarySelected.stateToken)
        .then(result => result.value),
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
      await session
        .text("#single-select-events", primarySelectedAgain.stateToken)
        .then(result => result.value),
      "input=1,change=1,value=central,selected=1",
      "reselecting the single-select value dispatched duplicate events",
    );

    const selected = await session.select(
      "#region",
      SELECTED_REGIONS,
      primarySelectedAgain.stateToken,
    );
    assert.equal(selected.changed, true);
    assert.deepEqual(selected.values, [...SELECTED_REGIONS]);
    assert.equal(
      await session.text("#select-events", selected.stateToken).then(result => result.value),
      INTERACTION_FINGERPRINT.select,
    );

    const submitted = await session.submit("#login-form", selected.stateToken);
    assert.equal(submitted.submitted, true);

    const dashboard = await session.settle(submitted.stateToken, SETTLE_POLICY);
    assert.equal(dashboard.outcome, "quiescent");
    assert.ok(
      dashboard.virtualTimeNs - initial.virtualTimeNs >= 250_000_000n,
      "controlled settlement did not drive the dashboard timer and animation frame",
    );
    assert.equal(
      await session.text("#status", dashboard.stateToken).then(result => result.value),
      "dashboard-ready",
    );
    assert.equal(
      await session.text("#trace", dashboard.stateToken).then(result => result.value),
      "dashboard,history,fetch,promise,microtask,timer,raf",
    );

    const query = await session.query("#dashboard > a.next", dashboard.stateToken);
    assert.equal(query.count, 1n);
    assert.equal(query.stateToken, dashboard.stateToken);
    const dashboardExtraction = await session.extract(
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
    const nextUrl = server.url(
      "/redirect-next?secret=fixture-secret-navigation",
    );
    assert.deepEqual(dashboardExtraction.rows, [
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

    const navigated = await session.navigate(
      nextUrl,
      dashboardExtraction.stateToken,
    );
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
      dashboardExtraction.stateToken,
      "the replacement document reused the previous document authority token",
    );

    const second = await session.settle(navigated.stateToken, SETTLE_POLICY);
    assert.equal(second.outcome, "quiescent");
    assert.equal(
      await session.text("#second-status", second.stateToken).then(result => result.value),
      "second-ready",
    );
    assert.equal(
      await session.text("#second-email", second.stateToken).then(result => result.value),
      SYNTHETIC_EMAIL,
    );
    assert.equal(
      await session.text("#second-role", second.stateToken).then(result => result.value),
      ROLE,
    );
    assert.equal(
      await session.text("#second-flow", second.stateToken).then(result => result.value),
      "login",
    );
    assert.equal(
      await session.text("#detail-value", second.stateToken).then(result => result.value),
      DETAIL,
    );

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
    assertExportedState(exported.state, server.origin);

    const requests = await collectRequests(session, second.stateToken);
    assertRequestProof(requests, server.origin);
    const evidence = await collectEvidence(session, second.stateToken);
    assertEvidenceProof(evidence, requests);
    assertAuditRedaction(requests, evidence);

    await closeSessionAndAssertProcessExit(runtime, session);
    closed = true;
    return exported.state;
  } finally {
    if (!closed) await runtime.close().catch(() => undefined);
  }
}

async function executeRestoredSession(
  server: SessionNorthStarServer,
  executablePath: string,
  state: SessionState,
): Promise<void> {
  const runtime = await launch({ executablePath });
  let closed = false;
  try {
    assertV02Capabilities(runtime);
    const restoredUrl = server.url(
      "/restored?proof=fixture-secret-restored-query",
    );
    const session = await runtime.openSession(restoredUrl, {
      state,
      network: sessionNetwork(server),
    });
    assert.equal(session.url, restoredUrl);
    assert.equal(session.boundary, "controlled_ready");
    const settled = await session.settle(session.stateToken, SETTLE_POLICY);
    assert.equal(settled.outcome, "quiescent");
    assert.equal(
      await session.text("#restored-status", settled.stateToken).then(result => result.value),
      "restored-ready",
    );
    assert.equal(
      await session.text("#restored-email", settled.stateToken).then(result => result.value),
      SYNTHETIC_EMAIL,
    );
    assert.equal(
      await session.text("#restored-role", settled.stateToken).then(result => result.value),
      MUTATED_ROLE,
    );
    assert.equal(
      await session.text("#restored-flow", settled.stateToken).then(result => result.value),
      MUTATED_FLOW,
    );
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
    const restoredRequest = requests.find(
      (request) => request.url.path === "/restored",
    );
    assert.ok(restoredRequest, "restored initial request is absent from audit");
    assert.deepEqual(restoredRequest.url.queryKeys, ["proof"]);
    assertAuditRedaction(requests, []);
    await closeSessionAndAssertProcessExit(runtime, session);
    closed = true;
  } finally {
    if (!closed) await runtime.close().catch(() => undefined);
  }
}

async function proveFixtureOnlyMissIsSticky(
  server: SessionNorthStarServer,
  executablePath: string,
): Promise<void> {
  const forbiddenPath = "/fixture-miss-must-not-reach-live";
  const runtime = await launch({ executablePath });
  try {
    assertV02Capabilities(runtime);
    await assert.rejects(
      runtime.openSession(server.url(forbiddenPath), {
        network: { mode: "fixtures_only", routes: [] },
      }),
      (error: unknown) => {
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

async function proveCrossOriginReplacementAndIsolation(
  sourceServer: SessionNorthStarServer,
  executablePath: string,
): Promise<void> {
  const targetServer = await startSessionNorthStarServer("localhost");
  const runtime = await launch({ executablePath });
  let closed = false;
  try {
    const session = await runtime.openSession(
      sourceServer.url("/cross-origin-source"),
      { network: { mode: "live", routes: [] } },
    );
    const source = await session.settle(session.stateToken, SETTLE_POLICY);
    assert.equal(source.outcome, "quiescent");
    assert.equal(
      await session
        .text("#cross-origin-status", source.stateToken)
        .then((result) => result.value),
      "source-ready",
    );

    const target = await session.navigate(
      targetServer.url("/cross-origin-target"),
      source.stateToken,
    );
    assert.equal(new URL(target.url).origin, targetServer.origin);
    assert.equal(target.documentEpoch, 2n);
    assert.equal(target.navigationId, 1n);
    assert.notEqual(target.stateToken, source.stateToken);
    await assert.rejects(
      session.text("#cross-origin-status", source.stateToken),
      assertStaleDocumentToken,
    );

    const targetSettled = await session.settle(target.stateToken, SETTLE_POLICY);
    assert.equal(targetSettled.outcome, "quiescent");
    assert.equal(
      await session
        .text("#cross-origin-status", targetSettled.stateToken)
        .then((result) => result.value),
      "target-ready|source-cookie=false|local-before=null|session-before=null|probe=target-probe",
    );

    const returned = await session.navigate(
      sourceServer.url("/cross-origin-return"),
      targetSettled.stateToken,
    );
    assert.equal(new URL(returned.url).origin, sourceServer.origin);
    assert.equal(returned.documentEpoch, 3n);
    assert.equal(returned.navigationId, 2n);
    await assert.rejects(
      session.text("#cross-origin-status", targetSettled.stateToken),
      assertStaleDocumentToken,
    );

    const sourceReturned = await session.settle(returned.stateToken, SETTLE_POLICY);
    assert.equal(sourceReturned.outcome, "quiescent");
    assert.equal(
      await session
        .text("#cross-origin-status", sourceReturned.stateToken)
        .then((result) => result.value),
      "source-return|source-cookie=true|target-cookie=false|local=source-local|session=source-session|probe=source-probe",
    );

    const exported = await session.exportState();
    const sourceCookie = exported.state.cookies.find(
      (cookie) => cookie.name === "stasis-origin-source",
    );
    const targetCookie = exported.state.cookies.find(
      (cookie) => cookie.name === "stasis-origin-target",
    );
    assert.deepEqual(
      sourceCookie === undefined
        ? undefined
        : { domain: sourceCookie.domain, value: sourceCookie.value },
      { domain: "127.0.0.1", value: "source-cookie" },
    );
    assert.deepEqual(
      targetCookie === undefined
        ? undefined
        : { domain: targetCookie.domain, value: targetCookie.value },
      { domain: "localhost", value: "target-cookie" },
    );
    for (const [origin, localValue, sessionValue] of [
      [sourceServer.origin, "source-local", "source-session"],
      [targetServer.origin, "target-local", "target-session"],
    ] as const) {
      const originState = exported.state.origins.find((entry) => entry.origin === origin);
      assert.ok(originState, `state export omitted ${origin}`);
      assert.equal(
        originState.localStorage.find((entry) => entry.key === "stasis-origin-marker")
          ?.value,
        localValue,
      );
      assert.equal(
        originState.sessionStorage.find((entry) => entry.key === "stasis-origin-marker")
          ?.value,
        sessionValue,
      );
    }

    const targetInitial = targetServer.requests.find(
      (request) => request.pathname === "/cross-origin-target",
    );
    assert.ok(targetInitial);
    assert.doesNotMatch(targetInitial.cookie, /stasis-origin-source=/u);
    const targetProbe = targetServer.requests.find(
      (request) => request.pathname === "/cross-origin-target-probe",
    );
    assert.ok(targetProbe);
    assert.match(targetProbe.cookie, /(?:^|;\s*)stasis-origin-target=target-cookie(?:;|$)/u);
    assert.doesNotMatch(targetProbe.cookie, /stasis-origin-source=/u);
    const sourceReturn = sourceServer.requests.find(
      (request) => request.pathname === "/cross-origin-return",
    );
    assert.ok(sourceReturn);
    assert.match(sourceReturn.cookie, /(?:^|;\s*)stasis-origin-source=source-cookie(?:;|$)/u);
    assert.doesNotMatch(sourceReturn.cookie, /stasis-origin-target=/u);
    const sourceProbe = sourceServer.requests.find(
      (request) => request.pathname === "/cross-origin-source-probe",
    );
    assert.ok(sourceProbe);
    assert.match(sourceProbe.cookie, /(?:^|;\s*)stasis-origin-source=source-cookie(?:;|$)/u);
    assert.doesNotMatch(sourceProbe.cookie, /stasis-origin-target=/u);

    await closeSessionAndAssertProcessExit(runtime, session);
    closed = true;
  } finally {
    if (!closed) await runtime.close().catch(() => undefined);
    await targetServer.close();
  }
  if (targetServer.errors.length > 0) throw targetServer.errors[0];
}

async function proveHistoryTraversalIsTypedUnsupported(
  server: SessionNorthStarServer,
  executablePath: string,
): Promise<void> {
  const runtime = await launch({ executablePath });
  let closed = false;
  try {
    const session = await runtime.openSession(
      server.url("/history-traversal-unsupported"),
      { network: { mode: "live", routes: [] } },
    );
    const initial = await session.settle(session.stateToken, SETTLE_POLICY);
    assert.equal(initial.outcome, "quiescent");
    const activated = await session.activate("#history-back", initial.stateToken);
    assert.equal(
      await session
        .text("#history-traversal-status", activated.stateToken)
        .then((result) => result.value),
      "NotSupportedError",
    );
    const terminal = await session.settle(activated.stateToken, SETTLE_POLICY);
    assert.equal(terminal.outcome, "unsupported_work");
    if (terminal.outcome === "unsupported_work") {
      assert.equal(terminal.failure.code, "unsupported_clock_surface");
    }
    assert.ok(
      terminal.unsupportedWork.some(
        (work) => work.timeSurface === "history_traversal",
      ),
      "history traversal was silently treated as quiescent",
    );
    await closeSessionAndAssertProcessExit(runtime, session);
    closed = true;
  } finally {
    if (!closed) await runtime.close().catch(() => undefined);
  }
}

async function proveFixtureAbortIsObservable(
  server: SessionNorthStarServer,
  executablePath: string,
): Promise<void> {
  const forbiddenPath = "/fixture-abort-must-not-reach-live";
  const forbiddenUrl = server.url(forbiddenPath);
  const runtime = await launch({ executablePath });
  let closed = false;
  try {
    const session = await runtime.openSession(server.url("/network-abort"), {
      network: {
        mode: "mixed",
        routes: [
          {
            match: { method: "GET", url: { prefix: forbiddenUrl } },
            abort: { reason: "blocked_by_fixture" },
          },
        ],
      },
    });
    const settled = await session.settle(session.stateToken, SETTLE_POLICY);
    assert.equal(settled.outcome, "quiescent");
    assert.equal(
      await session
        .text("#network-abort-status", settled.stateToken)
        .then((result) => result.value),
      "caught-fetch|caught-xhr",
    );
    const requests = await collectRequests(session, settled.stateToken);
    const abortedRequests = requests.filter(
      (request) => request.url.path === forbiddenPath,
    );
    assert.equal(
      abortedRequests.length,
      2,
      "request audit did not retain both fixture-aborted fetch and XHR",
    );
    assert.deepEqual(
      abortedRequests.map((request) => request.url.queryKeys),
      [["transport"], ["transport"]],
    );
    assert.deepEqual(
      new Set(abortedRequests.map((request) => request.resourceKind)),
      new Set(["fetch", "xml_http_request"]),
    );
    const evidence = await collectEvidence(session, settled.stateToken);
    for (const abortedRequest of abortedRequests) {
      assert.ok(
        evidence.some(
          (record) =>
            record.kind === "route_decided" &&
            record.requestId === abortedRequest.requestId &&
            record.decision === "fixture_abort",
        ),
        `evidence omitted fixture-abort decision for ${abortedRequest.requestId}`,
      );
      assert.ok(
        evidence.some(
          (record) =>
            record.kind === "request_failed" &&
            record.requestId === abortedRequest.requestId &&
            record.reason === "blocked_by_fixture",
        ),
        `evidence omitted allow-listed abort failure for ${abortedRequest.requestId}`,
      );
    }
    await closeSessionAndAssertProcessExit(runtime, session);
    closed = true;
  } finally {
    if (!closed) await runtime.close().catch(() => undefined);
  }
  assert.equal(
    server.requests.some((request) => request.pathname === forbiddenPath),
    false,
    "fixture abort fell through to the live server",
  );
}

function assertV02Capabilities(runtime: Runtime): void {
  assert.ok(
    runtime.info.capabilities.profiles.includes(CONTROLLED_WEB_SESSION_V1_PROFILE),
    `runtime omitted ${CONTROLLED_WEB_SESSION_V1_PROFILE}`,
  );
  for (const method of REQUIRED_METHODS) {
    assert.ok(runtime.info.capabilities.methods.includes(method), `runtime omitted ${method}`);
  }
}

function assertStaleDocumentToken(error: unknown): boolean {
  assert.ok(error instanceof StasisProtocolError);
  assert.equal(error.code, "stale_state_token");
  assert.equal(error.fatal, false);
  assert.equal(error.stateEffect, "none");
  return true;
}

function assertStaleSessionStateToken(error: unknown): boolean {
  assert.ok(error instanceof StasisProtocolError);
  assert.equal(error.code, "stale_session_state_token");
  assert.equal(error.fatal, false);
  assert.equal(error.stateEffect, "none");
  return true;
}

function assertNonMutatingAutomationRejection(
  code:
    | "automation_dom_traversal_limit_exceeded"
    | "automation_output_limit_exceeded"
    | "invalid_select_multiplicity"
    | "automation_selector_evaluation_limit_exceeded"
    | "unsupported_selector"
    | "unsupported_uncheck_element",
): (error: unknown) => boolean {
  return (error: unknown): boolean => {
    assert.ok(error instanceof StasisProtocolError);
    assert.equal(error.code, code);
    assert.equal(error.fatal, false);
    assert.equal(error.stateEffect, "none");
    return true;
  };
}

function assertExportedState(state: SessionState, origin: string): void {
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
  assert.deepEqual(Object.fromEntries(originState.localStorage.map(({ key, value }) => [key, value])), {
    "stasis-email": SYNTHETIC_EMAIL,
    "stasis-role": MUTATED_ROLE,
  });
  assert.deepEqual(
    Object.fromEntries(originState.sessionStorage.map(({ key, value }) => [key, value])),
    { "stasis-flow": MUTATED_FLOW },
  );
}

function mutateStorageState(
  origins: SessionState["origins"],
  origin: string,
): SessionState["origins"] {
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

function assertMutatedStorage(
  origins: SessionState["origins"],
  origin: string,
): void {
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

async function collectRequests(
  session: Session,
  expectedStateToken: DocumentStateToken,
): Promise<SessionRequestRecord[]> {
  const records: SessionRequestRecord[] = [];
  let afterSeq = 0n;
  for (let pageIndex = 0; pageIndex < 64; pageIndex += 1) {
    const page = await session.requests({ afterSeq, limit: 32 });
    assert.equal(page.stateToken, expectedStateToken);
    assert.equal(page.complete, true, "request audit history was unexpectedly evicted");
    records.push(...page.records);
    if (!page.hasMore) return records;
    assert.ok(
      page.nextAfterSeq !== undefined && page.nextAfterSeq > afterSeq,
      "request audit pagination did not advance",
    );
    afterSeq = page.nextAfterSeq;
  }
  assert.fail("request audit exceeded the bounded pagination proof");
}

async function collectEvidence(
  session: Session,
  expectedStateToken: DocumentStateToken,
): Promise<SessionEvidenceRecord[]> {
  const records: SessionEvidenceRecord[] = [];
  let afterSeq = 0n;
  for (let pageIndex = 0; pageIndex < 64; pageIndex += 1) {
    const page = await session.evidence({ afterSeq, limit: 32 });
    assert.equal(page.schemaVersion, 2);
    assert.equal(page.stateToken, expectedStateToken);
    assert.equal(page.complete, true, "evidence history was unexpectedly evicted");
    records.push(...page.records);
    if (!page.hasMore) return records;
    assert.ok(
      page.nextAfterSeq !== undefined && page.nextAfterSeq > afterSeq,
      "evidence pagination did not advance",
    );
    afterSeq = page.nextAfterSeq;
  }
  assert.fail("evidence exceeded the bounded pagination proof");
}

function assertRequestProof(records: readonly SessionRequestRecord[], origin: string): void {
  const observed = new Set(
    records.map((record) => `${record.method} ${record.url.path}`),
  );
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
  assert.ok(loginRequest);
  assert.ok(loginRequest.bodyBytes > 0n);
}

function assertEvidenceProof(
  records: readonly SessionEvidenceRecord[],
  requests: readonly SessionRequestRecord[],
): void {
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
  ] as const) {
    assert.ok(kinds.has(kind), `session evidence omitted ${kind}`);
  }
  assert.ok(
    records.some(
      (record) => record.kind === "route_decided" && record.decision === "live",
    ),
    "session evidence did not classify loopback traffic as live",
  );
  const requestIds = new Set(requests.map((request) => request.requestId));
  assert.equal(
    requestIds.size,
    requests.length,
    "request audit reused an opaque request ID",
  );
  for (const record of records) {
    if (
      record.kind === "request_started" ||
      record.kind === "route_decided" ||
      record.kind === "response_headers" ||
      record.kind === "request_completed" ||
      record.kind === "request_failed"
    ) {
      assert.ok(
        requestIds.has(record.requestId),
        `evidence referenced unknown request ID ${record.requestId}`,
      );
    }
  }

  const request = (method: string, path: string): SessionRequestRecord => {
    const matches = requests.filter(
      (record) => record.method === method && record.url.path === path,
    );
    assert.equal(matches.length, 1, `expected exactly one ${method} ${path} request`);
    return matches[0] as SessionRequestRecord;
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
  ] as const;
  for (const [method, path, status] of expectedResponses) {
    const expectedRequest = request(method, path);
    assert.ok(
      records.some(
        (record) =>
          record.kind === "response_headers" &&
          record.requestId === expectedRequest.requestId &&
          record.status === status,
      ),
      `evidence omitted ${status} response headers for ${method} ${path}`,
    );
  }

  for (const [parentMethod, parentPath, childMethod, childPath] of [
    ["GET", "/start", "GET", "/login"],
    ["GET", "/handoff", "GET", "/dashboard"],
    ["GET", "/redirect-next", "GET", "/second"],
  ] as const) {
    const parent = request(parentMethod, parentPath);
    const child = request(childMethod, childPath);
    assert.equal(
      child.redirectParentId,
      parent.requestId,
      `${childMethod} ${childPath} did not retain its redirect parent`,
    );
    assert.ok(
      records.some(
        (record) =>
          record.kind === "redirect" &&
          record.requestId === parent.requestId &&
          record.nextRequestId === child.requestId,
      ),
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

  assert.deepEqual(
    new Set(
      records
        .filter((record) => record.kind === "navigation_started")
        .map((record) => record.navigationId),
    ),
    new Set([0n, 1n, 2n]),
    "navigation-start evidence did not cover the exact document sequence",
  );
  assert.deepEqual(
    new Set(
      records
        .filter((record) => record.kind === "navigation_committed")
        .map((record) => record.navigationId),
    ),
    new Set([0n, 1n, 2n]),
    "navigation-commit evidence did not cover the exact document sequence",
  );
  assert.deepEqual(
    new Set(
      records
        .filter((record) => record.kind === "same_document_history_changed")
        .map((record) => record.navigationId),
    ),
    new Set([1n, 2n]),
    "history evidence did not bind pushState and replaceState to their documents",
  );
  const settlementNavigationIds = new Set(
    records
      .filter((record) => record.kind === "settlement_terminal")
      .map((record) => record.navigationId),
  );
  for (const navigationId of [0n, 1n, 2n]) {
    assert.ok(
      settlementNavigationIds.has(navigationId),
      `session evidence omitted terminal settlement for navigation ${navigationId}`,
    );
  }
  assert.ok(
    records.filter((record) => record.kind === "settlement_terminal").length >= 3,
    "session evidence omitted a terminal settlement",
  );
}

function assertAuditRedaction(
  requests: readonly SessionRequestRecord[],
  evidence: readonly SessionEvidenceRecord[],
): void {
  const json = stringifyBigInts({ requests, evidence });
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
    assert.equal(json.includes(secret), false, `session audit leaked ${secret}`);
  }
}

async function closeSessionAndAssertProcessExit(
  runtime: Runtime,
  session: Session,
): Promise<void> {
  const pid = runtime.pid;
  assert.ok(pid !== undefined, "native runtime PID is unavailable");
  await session.close();
  assert.throws(
    () => process.kill(pid, 0),
    (error: unknown) =>
      typeof error === "object" &&
      error !== null &&
      "code" in error &&
      error.code === "ESRCH",
    "session.close returned before the native process exited",
  );
}

async function startSessionNorthStarServer(
  host = "127.0.0.1",
): Promise<SessionNorthStarServer> {
  const requests: RequestObservation[] = [];
  const logins: LoginObservation[] = [];
  const errors: unknown[] = [];
  const server = createServer((request, response) => {
    void serveSessionNorthStarRequest(request, response, requests, logins).catch(
      (error: unknown) => {
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
      },
    );
  });
  await listen(server, host);
  const address = server.address() as AddressInfo;
  const origin = `http://${host}:${address.port}`;
  return {
    origin,
    requests,
    logins,
    errors,
    url: (path) => new URL(path, origin).href,
    close: () => closeServer(server),
  };
}

async function serveSessionNorthStarRequest(
  request: IncomingMessage,
  response: ServerResponse,
  requests: RequestObservation[],
  logins: LoginObservation[],
): Promise<void> {
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
  if (method === "GET" && url.pathname === "/automation-adversarial") {
    writeHtml(response, FIXTURES.automationAdversarial);
    return;
  }
  if (method === "GET" && url.pathname === "/automation-handler-growth") {
    writeHtml(response, FIXTURES.automationHandlerGrowth);
    return;
  }
  if (method === "POST" && url.pathname === "/authenticate") {
    const contentType = request.headers["content-type"] ?? "";
    assert.equal(contentType, "application/json");
    const body = JSON.parse((await readBoundedBody(request, 16 * 1024)).toString("utf8")) as unknown;
    assertRecord(body, "authentication body");
    const { email, password, terms, regions } = body;
    assert.ok(typeof email === "string");
    assert.ok(typeof password === "string");
    assert.ok(typeof terms === "boolean");
    assert.ok(
      Array.isArray(regions) && regions.every((region) => typeof region === "string"),
    );
    logins.push({
      method: "POST",
      pathname: "/authenticate",
      contentType,
      email,
      password,
      terms,
      regions: [...regions],
    });
    writeResponse(
      response,
      200,
      "application/json",
      Buffer.from(JSON.stringify({ role: ROLE }), "utf8"),
      {
        "set-cookie": `stasis-auth=${AUTH_COOKIE_VALUE}; Path=/; HttpOnly; SameSite=Lax`,
      },
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
  if (method === "GET" && url.pathname === "/replacement-a") {
    writeHtml(response, FIXTURES.replacementA);
    return;
  }
  if (method === "GET" && url.pathname === "/replacement-b") {
    writeHtml(response, FIXTURES.replacementB);
    return;
  }
  if (method === "GET" && url.pathname === "/replacement-c") {
    writeHtml(response, FIXTURES.replacementC);
    return;
  }
  if (method === "GET" && url.pathname === "/native-form-source") {
    writeHtml(response, FIXTURES.nativeFormSource);
    return;
  }
  if (method === "GET" && url.pathname === "/native-form-destination") {
    writeHtml(response, FIXTURES.nativeFormDestination);
    return;
  }
  if (method === "GET" && url.pathname === "/sync-action-source") {
    writeHtml(response, FIXTURES.syncActionSource);
    return;
  }
  if (method === "GET" && url.pathname === "/sync-action-destination") {
    writeHtml(response, FIXTURES.syncActionDestination);
    return;
  }
  if (method === "GET" && url.pathname === "/sync-history-source") {
    writeHtml(response, FIXTURES.syncHistorySource);
    return;
  }
  if (method === "GET" && url.pathname === "/cross-origin-source") {
    writeHtml(response, FIXTURES.crossOriginSource);
    return;
  }
  if (method === "GET" && url.pathname === "/cross-origin-target") {
    writeHtml(response, FIXTURES.crossOriginTarget);
    return;
  }
  if (method === "GET" && url.pathname === "/cross-origin-return") {
    writeHtml(response, FIXTURES.crossOriginReturn);
    return;
  }
  if (method === "GET" && url.pathname === "/cross-origin-target-probe") {
    writeResponse(
      response,
      200,
      "text/plain; charset=utf-8",
      Buffer.from("target-probe", "utf8"),
    );
    return;
  }
  if (method === "GET" && url.pathname === "/cross-origin-source-probe") {
    writeResponse(
      response,
      200,
      "text/plain; charset=utf-8",
      Buffer.from("source-probe", "utf8"),
    );
    return;
  }
  if (method === "GET" && url.pathname === "/network-abort") {
    writeHtml(response, FIXTURES.networkAbort);
    return;
  }
  if (method === "GET" && url.pathname === "/history-traversal-unsupported") {
    writeHtml(response, FIXTURES.historyTraversalUnsupported);
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

function assertAuthenticated(
  cookie: string,
  pathname: string,
  expectedValue = AUTH_COOKIE_VALUE,
): void {
  assert.match(
    cookie,
    new RegExp(`(?:^|;\\s*)stasis-auth=${expectedValue}(?:;|$)`, "u"),
    `${pathname} requires the fixture session cookie`,
  );
}

function redirect(response: ServerResponse, location: string): void {
  response.writeHead(302, {
    location,
    "content-length": "0",
    "cache-control": "no-store",
    connection: "close",
  });
  response.end();
}

function writeHtml(response: ServerResponse, body: Uint8Array): void {
  writeResponse(response, 200, "text/html; charset=utf-8", body);
}

function writeResponse(
  response: ServerResponse,
  status: number,
  contentType: string,
  body: Uint8Array,
  extraHeaders: Readonly<Record<string, string>> = {},
): void {
  response.writeHead(status, {
    "content-type": contentType,
    "content-length": body.byteLength.toString(),
    "cache-control": "no-store",
    connection: "close",
    ...extraHeaders,
  });
  response.end(body);
}

async function readBoundedBody(
  request: IncomingMessage,
  maxBytes: number,
): Promise<Buffer> {
  const chunks: Buffer[] = [];
  let bytes = 0;
  for await (const chunk of request) {
    const buffer = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk as Uint8Array);
    bytes += buffer.length;
    assert.ok(bytes <= maxBytes, `request body exceeded ${maxBytes} bytes`);
    chunks.push(buffer);
  }
  return Buffer.concat(chunks, bytes);
}

function assertRecord(
  value: unknown,
  label: string,
): asserts value is Record<string, unknown> {
  assert.ok(
    typeof value === "object" && value !== null && !Array.isArray(value),
    `${label} must be an object`,
  );
}

function listen(server: Server, host: string): Promise<void> {
  return new Promise((resolve, reject) => {
    const onError = (error: Error): void => reject(error);
    server.once("error", onError);
    server.listen(0, host, () => {
      server.off("error", onError);
      resolve();
    });
  });
}

function closeServer(server: Server): Promise<void> {
  return new Promise((resolve, reject) => {
    server.close((error) => (error === undefined ? resolve() : reject(error)));
  });
}

function stringifyBigInts(value: unknown): string {
  return JSON.stringify(value, (_key, item: unknown) =>
    typeof item === "bigint" ? item.toString() : item,
  );
}
