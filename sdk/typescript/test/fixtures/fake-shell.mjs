import readline from "node:readline";

const scenario = process.argv[2] ?? "normal";
if (scenario === "ignore-sigterm") {
  process.on("SIGTERM", () => {});
  setInterval(() => {}, 1_000);
}
let wireSequence = 0n;
let sessionId = null;
let initializeParams = null;
let openParams = null;
let lastSettleParams = null;
let lastActivateParams = null;
let lastFillParams = null;
let lastQueryParams = null;
let lastTextParams = null;
let lastExtractParams = null;
let applicationOrdinal = 0;
let settleOrdinal = 0;
let documentTokenOrdinal = 1;
let sessionStateTokenOrdinal = 1;
const documentTokenNamespace = "11111111111111111111111111111111";
const sessionStateTokenNamespace = "22222222222222222222222222222222";
const documentToken = (ordinal) => `document:${documentTokenNamespace}:${ordinal}`;
const sessionToken = (ordinal) => `session:${sessionStateTokenNamespace}:${ordinal}`;
let stateToken = documentToken(1);
let sessionStateToken = sessionToken(1);
let sessionCookies = [];
let sessionOrigins = [];

const isSessionScenario = scenario.startsWith("session-");
const sessionV1Profile = "controlled-web-session-v1";
const sessionV2Profile = "controlled-web-session-v2";
const sessionProfiles = new Set([sessionV1Profile, sessionV2Profile]);
const isSessionProfile = (profile) => sessionProfiles.has(profile);

const rotateDocumentToken = () => {
  documentTokenOrdinal += 1;
  stateToken = documentToken(documentTokenOrdinal);
  return stateToken;
};

const rotateSessionStateToken = () => {
  sessionStateTokenOrdinal += 1;
  sessionStateToken = sessionToken(sessionStateTokenOrdinal);
  return sessionStateToken;
};

const sessionState = () => ({
  schemaVersion: 1,
  profile: sessionV1Profile,
  sensitive: true,
  sessionStorageScope: "top_level_browsing_context",
  cookies: sessionCookies,
  origins: sessionOrigins,
});

const settleOutcomes = [
  "quiescent",
  "quiescent_with_persistent_work",
  "blocked_on_external_io",
  "blocked_on_open_ended_work",
  "unsupported_work",
  "virtual_time_limit_exceeded",
  "task_limit_exceeded",
  "microtask_limit_exceeded",
  "rendering_limit_exceeded",
  "mutation_limit_exceeded",
  "control_turn_limit_exceeded",
  "runtime_error",
];

const allTimeSurfaces = [
  "window_timers",
  "same_event_loop_iframe",
  "java_script_date",
  "performance",
  "host_timestamp",
  "update_rendering",
  "animation_frame",
  "document_timeline",
  "worker",
  "worklet",
  "cross_event_loop_iframe",
  "cross_event_loop_navigation",
  "auxiliary_web_view",
  "resource_thread_io",
  "external_subscription",
  "native_media",
  "embedder_control",
  "history_traversal",
];

const pending = () => ({
  stateGeneration: "9007199254740993",
  domEpoch: "9007199254740994",
  virtualTimeNs: "18446744073709551625",
  clock: {
    mode: "controlled",
    unsupportedSurfaces:
      scenario === "unsupported-async-surfaces"
        ? ["external_subscription", "native_media", "embedder_control"]
        : scenario === "all-time-surfaces"
          ? allTimeSurfaces
          : [],
  },
  input: {
    readyEvents: "0",
    intakeSaturated: false,
    tasks: { ready: "0", throttled: "0", inactive: "0" },
  },
  microtasks: { queued: "0", checkpointInProgress: false, terminal: false },
  producers: { pending: "0", stability: "stable_empty", terminal: false },
  timers: {
    ready: "0",
    futureFinite: "1",
    persistent: "1",
    unsupported: "0",
    nextDeadlineNs: "18446744073709551626",
  },
  parser: {
    total: "0",
    ready: "0",
    awaitingExternalIo: "0",
    awaitingCommit: "0",
    awaitingScriptInput: "0",
    suspended: "0",
  },
  network: {
    counts: {
      navigation: "0",
      fetch: "0",
      xmlHttpRequest: "0",
      image: "0",
      font: "0",
      stylesheet: "0",
      script: "0",
      unclassifiedProducerIo: "1",
      other: "0",
    },
    active: [
      {
        sourceId: "3",
        kind: "unclassified_producer_io",
        phase: "awaiting_response",
        owner: "script",
        loadBlocking: "unknown",
        startedAtNs: "18446744073709551624",
      },
    ],
  },
  rendering: {
    opportunityReady: false,
    retainedAnimationFrames: "0",
    runnableAnimationFrames: "0",
    updateRequired: false,
    pendingAnimationEvents: "0",
    finiteAnimations: "0",
    persistentAnimations: "0",
    unsupportedAnimations: "0",
    finiteAnimatedImages: "0",
    persistentAnimatedImages: "0",
    unsupportedAnimatedImages: "0",
    imageUpdateReady: false,
    dirtyCanvases: "0",
    canvasUploadPending: false,
    unsupportedCanvases: "0",
    pendingFonts: "0",
    pendingImages: "0",
  },
  sourceEpoch: "9007199254740995",
  sources: [
    {
      sourceId: "1",
      kind: "timer",
      state: "open_ended",
      openEnded: { reason: "interval", requestedPeriodNs: "5000000000" },
    },
    {
      sourceId: "2",
      kind: "rendering_update",
      state: "unsupported",
      unsupported: { reason: "throttled_rendering" },
    },
    {
      sourceId: "3",
      kind: "network",
      state: "awaiting_external_io",
      owner: "script",
      loadBlocking: "unknown",
    },
  ],
  runtimeFailures: [{ component: "source_epoch", occurrences: "2" }],
});

const settleResult = (outcome) => {
  const result = {
    outcome,
    virtualTimeNs: "18446744073709551625",
    wallTimeNs: "25",
    stateGeneration: "9007199254740993",
    domEpoch: "9007199254740994",
    effectivePolicy: {
      persistentWork: "report",
      maxVirtualTimeNs: "30000000000",
      maxControlTurns: "100000",
      wallIoTimeoutNs: "10000000000",
    },
    processed: {
      controlTurns: "3",
      tasks: "4",
      microtasks: "5",
      renderingOpportunities: "6",
      mutations: "7",
    },
    snapshot: pending(),
    persistentWork: [
      {
        kind: "timer",
        count: "2",
        reason: "interval",
        requestedPeriodNs: "5000000000",
      },
    ],
    externalIo: [],
    unsupportedWork: [],
  };
  if (outcome === "virtual_time_limit_exceeded") {
    result.limit = {
      kind: "virtual_time",
      limit: "30000000000",
      startVirtualTimeNs: "1",
      requestedVirtualTimeNs: "30000000001",
    };
  }
  if (outcome === "control_turn_limit_exceeded") {
    result.limit = { kind: "control_turns", limit: "100000" };
  }
  const executionLimits = {
    task_limit_exceeded: ["ordinary_tasks", "100000"],
    microtask_limit_exceeded: ["microtasks", "1000000"],
    rendering_limit_exceeded: ["rendering_opportunities", "10000"],
    mutation_limit_exceeded: ["mutations", "1000000"],
  };
  if (Object.hasOwn(executionLimits, outcome)) {
    const [kind, limit] = executionLimits[outcome];
    result.limit = { kind, limit, observed: (BigInt(limit) + 1n).toString() };
  }
  if (outcome === "unsupported_work") {
    result.failure = { code: "unsupported_source" };
  }
  if (outcome === "runtime_error") {
    result.failure = { code: "runtime_terminals" };
  }
  return result;
};

async function writeSplit(frame) {
  const bytes = Buffer.from(frame, "utf8");
  const cuts = [1, Math.min(7, bytes.length), Math.min(19, bytes.length), bytes.length];
  let offset = 0;
  for (const cut of cuts) {
    if (cut <= offset) continue;
    await new Promise((resolve) => process.stdout.write(bytes.subarray(offset, cut), resolve));
    offset = cut;
    await new Promise((resolve) => setTimeout(resolve, 1));
  }
}

async function send(request, result, responseSession = sessionId) {
  wireSequence += 1n;
  await writeSplit(
    `${JSON.stringify({
      v: 1,
      type: "response",
      wireSeq: wireSequence.toString(),
      id: request.id,
      sessionId: responseSession,
      result,
    })}\r\n`,
  );
}

async function sendProtocolError(request, error) {
  wireSequence += 1n;
  await writeSplit(
    `${JSON.stringify({
      v: 1,
      type: "response",
      wireSeq: wireSequence.toString(),
      id: request.id,
      sessionId,
      event: null,
      error,
    })}\n`,
  );
}

async function sendError(request, stateEffect = "none", details = undefined) {
  await sendProtocolError(request, {
    code: "evaluation_failed",
    message: "synthetic evaluation failure",
    fatal: false,
    stateEffect,
    ...(details === undefined ? {} : { details }),
  });
}

async function handle(request) {
  if (request.method === "protocol.initialize") {
    initializeParams = request.params;
    await send(
      request,
      {
        protocolVersion: 1,
        implementation: { name: "fake-stasis", version: "0.0.0", source: {} },
        capabilities: {
          methods: [
            "protocol.initialize",
            "session.open",
            "dom.evaluate",
            ...(scenario === "no-actions"
              ? []
              : ["dom.query", "dom.text", "dom.extract", "action.fill", "action.activate"]),
            ...(scenario === "session-no-runtime-methods"
              ? []
              : ["runtime.pending", "runtime.settle", "runtime.advance_to_next"]),
            ...(isSessionScenario
              ? [
                  "action.focus",
                  "action.check",
                  "action.uncheck",
                  "action.select",
                  "action.submit",
                  "session.navigate",
                  "session.state.export",
                  "session.state.import",
                  "session.cookies.get",
                  "session.cookies.set",
                  "session.storage.get",
                  "session.storage.set",
                  "session.requests",
                  "session.evidence",
                ]
              : []),
            "session.close",
          ],
          clockModes: ["real", "controlled"],
          profiles: [
            "controlled-webapp-v1",
            ...(isSessionScenario
              ? scenario === "session-v2-unadvertised"
                ? [sessionV1Profile]
                : [sessionV1Profile, sessionV2Profile]
              : []),
          ],
          settlement: true,
          settlementLimits: ["maxVirtualTimeNs", "maxControlTurns", "wallIoTimeoutNs"],
        },
        limits: { maxInboundFrameBytes: 1048576, maxActiveEngineRequests: 1 },
      },
      null,
    );
    return;
  }
  if (request.method === "session.open") {
    openParams = request.params;
    sessionId = "fake-session";
    if (isSessionProfile(request.params.profile)) {
      stateToken = documentToken(1);
      sessionStateToken = sessionToken(1);
      documentTokenOrdinal = 1;
      sessionStateTokenOrdinal = 1;
      sessionCookies = request.params.state?.cookies ?? [];
      sessionOrigins = request.params.state?.origins ?? [];
      if (scenario === "session-future-opaque-tokens") {
        stateToken = "future-document-authority/v9";
        sessionStateToken = "future-session-authority/v9";
      }
      await send(request, {
        sessionId,
        requestedUrl: request.params.url,
        url: request.params.url,
        boundary: "controlled_ready",
        clockMode: "controlled",
        profile:
          scenario === "session-v2-response-mismatch"
            ? sessionV1Profile
            : request.params.profile,
        stateToken,
        sessionStateToken:
          scenario === "session-invalid-session-state-token"
            ? "x".repeat(257)
            : sessionStateToken,
      });
      return;
    }
    const responseClock =
      scenario === "clock-mismatch"
        ? request.params.clockMode === "controlled"
          ? "real"
          : "controlled"
        : request.params.clockMode ?? "real";
    await send(request, {
      sessionId,
      requestedUrl: request.params.url,
      url: request.params.url,
      boundary: responseClock === "controlled" ? "controlled_ready" : "load_complete",
      clockMode: responseClock,
      profile:
        scenario === "clock-mismatch"
          ? request.params.clockMode === "controlled"
            ? null
            : "controlled-webapp-v1"
          : request.params.profile ?? null,
    });
    return;
  }

  applicationOrdinal += 1;
  if (request.method === "action.activate") {
    lastActivateParams = request.params;
    if (scenario === "command-hang-mutate") return;
    if (isSessionProfile(openParams?.profile)) {
      if (request.params.expectedStateToken !== stateToken) {
        throw new Error("action.activate expectedStateToken mismatch");
      }
      await send(request, {
        stateGeneration: "9007199254740996",
        stateToken: rotateDocumentToken(),
      });
      return;
    }
    await send(
      request,
      scenario === "invalid-activation-result"
        ? { stateGeneration: "01" }
        : { stateGeneration: "9007199254740996" },
    );
    return;
  }
  if (request.method === "action.fill") {
    lastFillParams = request.params;
    if (isSessionProfile(openParams?.profile)) {
      if (request.params.expectedStateToken !== stateToken) {
        throw new Error("action.fill expectedStateToken mismatch");
      }
      await send(request, {
        stateGeneration: "9007199254740997",
        stateToken: rotateDocumentToken(),
      });
      return;
    }
    await send(
      request,
      scenario === "invalid-fill-result"
        ? { stateGeneration: "01" }
        : { stateGeneration: "9007199254740997" },
    );
    return;
  }
  if (
    request.method === "action.focus" ||
    request.method === "action.check" ||
    request.method === "action.uncheck" ||
    request.method === "action.select" ||
    request.method === "action.submit"
  ) {
    if (request.params.expectedStateToken !== stateToken) {
      throw new Error(`${request.method} expectedStateToken mismatch`);
    }
    const replacement = rotateDocumentToken();
    if (request.method === "action.focus") {
      await send(request, {
        focused: true,
        stateGeneration: "9007199254741002",
        stateToken: replacement,
      });
      return;
    }
    if (request.method === "action.check" || request.method === "action.uncheck") {
      await send(request, {
        changed: true,
        checked: request.method === "action.check",
        stateGeneration: "9007199254741003",
        stateToken: replacement,
      });
      return;
    }
    if (request.method === "action.select") {
      if (!Array.isArray(request.params.values)) throw new Error("action.select values missing");
      await send(request, {
        changed: true,
        values: request.params.values,
        stateGeneration: "9007199254741004",
        stateToken: replacement,
      });
      return;
    }
    await send(request, {
      submitted: true,
      stateGeneration: "9007199254741005",
      stateToken: replacement,
    });
    return;
  }
  if (request.method === "dom.query") {
    lastQueryParams = request.params;
    if (isSessionProfile(openParams?.profile)) {
      if (request.params.expectedStateToken !== stateToken) {
        throw new Error("dom.query expectedStateToken mismatch");
      }
      await send(request, {
        count: "2",
        stateGeneration: "9007199254740998",
        stateToken,
      });
      return;
    }
    await send(
      request,
      scenario === "invalid-query-result"
        ? { count: "01", stateGeneration: "9007199254740998" }
        : { count: "18446744073709551616", stateGeneration: "9007199254740998" },
    );
    return;
  }
  if (request.method === "dom.text") {
    lastTextParams = request.params;
    if (isSessionProfile(openParams?.profile)) {
      if (request.params.expectedStateToken !== stateToken) {
        throw new Error("dom.text expectedStateToken mismatch");
      }
      await send(request, {
        value: "ready",
        stateGeneration: "9007199254740998",
        stateToken,
      });
      return;
    }
    await send(
      request,
      scenario === "invalid-text-result"
        ? { value: "ready", stateGeneration: "01" }
        : { value: "ready", stateGeneration: "9007199254740996" },
    );
    return;
  }
  if (request.method === "dom.extract") {
    lastExtractParams = request.params;
    if (isSessionProfile(openParams?.profile)) {
      if (request.params.expectedStateToken !== stateToken) {
        throw new Error("dom.extract expectedStateToken mismatch");
      }
      const attributeField = request.params.fields.find(
        (field) => field.read === "attribute" || field.read === "resolved_url",
      );
      if (attributeField?.attribute !== "href") {
        throw new Error("dom.extract attribute field mismatch");
      }
      await send(request, {
        rows: [
          {
            fields: request.params.fields.map((field) => ({
              name: field.name,
              value:
                field.attribute === "data-missing"
                  ? null
                  : field.read === "resolved_url"
                    ? "https://example.test/next"
                    : "/next",
            })),
          },
        ],
        stateGeneration: "9007199254740998",
        stateToken,
      });
      return;
    }
    await send(
      request,
      scenario === "invalid-extract-result"
        ? {
            rows: [{ fields: { first: "one" } }],
            stateGeneration: "9007199254740999",
          }
        : {
            rows: [
              {
                fields: [
                  { name: "second", value: "<strong>two</strong>" },
                  { name: "first", value: "one" },
                ],
              },
              {
                fields: [
                  { name: "second", value: "<strong>four</strong>" },
                  { name: "first", value: "three" },
                ],
              },
            ],
            stateGeneration: "9007199254740999",
          },
    );
    return;
  }
  if (request.method === "dom.evaluate") {
    if (request.params.expression === "hold") {
      await new Promise((resolve) => setTimeout(resolve, 80));
    }
    if (
      request.params.expression === "protocol-error" ||
      request.params.expression === "indeterminate-error" ||
      request.params.expression === "protocol-error-details" ||
      request.params.expression === "protocol-error-invalid-details"
    ) {
      await sendError(
        request,
        request.params.expression === "indeterminate-error" ? "indeterminate" : "none",
        request.params.expression === "protocol-error-details"
          ? {
              actual: "21",
              limit: 20,
              reasons: ["replacement", null],
              retryable: false,
            }
          : request.params.expression === "protocol-error-invalid-details"
            ? ["not", "an", "object"]
            : undefined,
      );
      return;
    }
    const value =
      request.params.expression === "__openParams"
        ? openParams
        : request.params.expression === "__initializeParams"
          ? initializeParams
        : request.params.expression === "__settleParams"
          ? lastSettleParams
          : request.params.expression === "__activateParams"
            ? lastActivateParams
            : request.params.expression === "__fillParams"
              ? lastFillParams
              : request.params.expression === "__queryParams"
                ? lastQueryParams
            : request.params.expression === "__textParams"
              ? lastTextParams
              : request.params.expression === "__extractParams"
                ? lastExtractParams
        : { expression: request.params.expression, ordinal: applicationOrdinal };
    await send(request, { value });
    return;
  }
  if (request.method === "runtime.pending") {
    if (scenario === "session-abort-active-pending") return;
    if (scenario === "command-hang-read") return;
    if (scenario === "malformed") {
      process.stdout.write('{"value":sensitive-invalid-json-canary}\n');
      return;
    }
    if (scenario === "invalid-utf8") {
      process.stdout.write(
        Buffer.concat([
          Buffer.from('{"value":"sensitive-invalid-utf8-canary'),
          Buffer.from([0xff]),
          Buffer.from('"}\n'),
        ]),
      );
      return;
    }
    if (scenario === "duplicate") {
      wireSequence += 1n;
      process.stdout.write(
        `{"v":1,"type":"response","wireSeq":"${wireSequence}","id":"${request.id}","sessionId":"${sessionId}","sensitive-duplicate-canary":"one","sensitive-duplicate-canary":"two","result":{}}\n`,
      );
      return;
    }
    if (scenario === "unmatched") {
      wireSequence += 1n;
      await writeSplit(
        `${JSON.stringify({
          v: 1,
          type: "response",
          wireSeq: wireSequence.toString(),
          id: "not-the-request",
          sessionId,
          result: pending(),
        })}\n`,
      );
      return;
    }
    if (scenario === "bad-sequence") wireSequence += 1n;
    if (scenario === "process-death") {
      process.stderr.write(`diagnostic-${"x".repeat(128)}-TAIL-END`, () => process.exit(17));
      return;
    }
    if (scenario === "stdout-eof-linger") {
      process.stdout.end();
      setInterval(() => {}, 1_000);
      return;
    }
    if (scenario === "invalid-pending") {
      const invalid = pending();
      invalid.input.intakeSaturated = "false";
      await send(request, invalid);
      return;
    }
    if (scenario === "invalid-source-id") {
      const invalid = pending();
      invalid.sources[0].sourceId = "01";
      await send(request, invalid);
      return;
    }
    if (isSessionProfile(openParams?.profile)) {
      await send(request, {
        ...pending(),
        stateToken:
          scenario === "session-invalid-token"
            ? "\ud800"
            : stateToken,
      });
      return;
    }
    await send(request, pending());
    return;
  }
  if (request.method === "runtime.settle") {
    if (scenario === "abort-active" || scenario === "session-abort-active-settle") return;
    lastSettleParams = request.params;
    const outcome =
      scenario === "settle-outcomes"
        ? settleOutcomes[settleOrdinal++ % settleOutcomes.length]
        : "quiescent";
    const result = settleResult(outcome);
    if (scenario === "invalid-settle") {
      result.outcome = "virtual_time_limit_exceeded";
      delete result.limit;
    }
    if (scenario === "settle-summary-mismatch") result.stateGeneration = "1";
    if (isSessionProfile(openParams?.profile)) {
      if (request.params.expectedStateToken !== stateToken) {
        throw new Error("runtime.settle expectedStateToken mismatch");
      }
      result.stateToken = rotateDocumentToken();
      result.snapshot.stateToken = result.stateToken;
    }
    await send(request, result);
    return;
  }
  if (request.method === "runtime.advance_to_next") {
    const snapshot = pending();
    snapshot.virtualTimeNs = "18446744073709551626";
    snapshot.stateGeneration = "9007199254740997";
    if (scenario === "advance-summary-mismatch") snapshot.stateGeneration = "1";
    const result = {
      outcome: "advanced",
      fromVirtualTimeNs: "18446744073709551625",
      virtualTimeNs: "18446744073709551626",
      stateGeneration: "9007199254740997",
      snapshot,
    };
    if (isSessionProfile(openParams?.profile)) {
      if (request.params.expectedStateToken !== stateToken) {
        throw new Error("runtime.advance_to_next expectedStateToken mismatch");
      }
      result.stateToken = rotateDocumentToken();
      result.snapshot.stateToken = result.stateToken;
    }
    await send(request, result);
    return;
  }
  if (request.method === "session.navigate") {
    if (request.params.expectedStateToken !== stateToken) {
      throw new Error("session.navigate expectedStateToken mismatch");
    }
    await send(request, {
      requestedUrl: request.params.url,
      url: request.params.url,
      boundary: "controlled_ready",
      stateGeneration: "9007199254741000",
      domEpoch: "9007199254741001",
      documentEpoch: "3",
      navigationId: "2",
      historyRevision: "4",
      stateToken: rotateDocumentToken(),
    });
    return;
  }
  if (request.method === "session.cookies.get") {
    const cookies =
      scenario === "session-invalid-cookie-state"
        ? [sessionCookies[0], sessionCookies[0]]
        : scenario === "session-secret-cookie-field"
          ? [
              {
                ...sessionCookies[0],
                "sensitive-cookie-field-canary": "sensitive-cookie-value-canary",
              },
            ]
        : scenario === "session-oversized-cookie-state"
          ? Array.from({ length: 70 }, (_unused, index) => ({
              ...sessionCookies[0],
              name: `cookie-${index}`,
              value: "x",
              path: `/${"p".repeat(3800)}`,
              creationSequence: String(index + 1),
              lastAccessSequence: String(index + 1000),
            }))
        : sessionCookies;
    await send(request, { cookies, sessionStateToken });
    return;
  }
  if (request.method === "session.cookies.set") {
    if (request.params.expectedSessionStateToken !== sessionStateToken) {
      throw new Error("session.cookies.set expectedSessionStateToken mismatch");
    }
    sessionCookies = request.params.cookies;
    await send(request, { sessionStateToken: rotateSessionStateToken() });
    return;
  }
  if (request.method === "session.storage.get") {
    const origins =
      scenario === "session-invalid-storage-state"
        ? [
            {
              ...sessionOrigins[0],
              localStorage: [
                sessionOrigins[0].localStorage[0],
                sessionOrigins[0].localStorage[0],
              ],
            },
          ]
        : scenario === "session-oversized-storage-state"
          ? [
              {
                ...sessionOrigins[0],
                localStorage: [
                  { key: "a", value: "x".repeat(128_000) },
                  { key: "b", value: "x".repeat(128_000) },
                ],
                sessionStorage: [],
              },
            ]
        : sessionOrigins;
    await send(request, { origins, sessionStateToken });
    return;
  }
  if (request.method === "session.storage.set") {
    if (request.params.expectedSessionStateToken !== sessionStateToken) {
      throw new Error("session.storage.set expectedSessionStateToken mismatch");
    }
    sessionOrigins = request.params.origins;
    await send(request, { sessionStateToken: rotateSessionStateToken() });
    return;
  }
  if (request.method === "session.state.export") {
    const state = sessionState();
    if (scenario === "session-invalid-export-state") {
      state.origins = [{ ...state.origins[0], origin: "HTTPS://example.test" }];
    }
    await send(request, { state, sessionStateToken });
    return;
  }
  if (request.method === "session.state.import") {
    if (Object.keys(request.params).length !== 0) {
      throw new Error("session.state.import must not serialize sensitive closed-phase inputs");
    }
    if (scenario === "session-import-unexpected-success") {
      await send(request, { sessionStateToken: rotateSessionStateToken() });
      return;
    }
    await sendProtocolError(request, {
      code: "session_state_import_phase_closed",
      message:
        "Session state import is closed after session publication; pass state to session.open instead",
      fatal: false,
      stateEffect: "none",
    });
    return;
  }
  if (request.method === "session.requests") {
    const fixtureUrl =
      openParams.network?.routes?.[0]?.match?.url?.exact ?? "https://example.test/path?token=redacted";
    const parsedUrl = new URL(fixtureUrl);
    const result = {
      records: [
        {
          seq: "1",
          requestId: "1",
          method: "GET",
          url: {
            origin: parsedUrl.origin,
            path: parsedUrl.pathname,
            queryKeys:
              scenario === "session-unsorted-query-keys"
                ? ["z-last", "a-first"]
                : scenario === "session-duplicate-query-keys"
                  ? ["duplicate", "duplicate"]
                : [...parsedUrl.searchParams.keys()].sort(),
          },
          resourceKind: "navigation",
          mainFrame: true,
          headerNames: ["accept"],
          bodyBytes: "0",
        },
      ],
      firstRetainedSeq: "1",
      nextAfterSeq: "1",
      latestSeq: "1",
      complete: true,
      hasMore: false,
      bounds: { maxRecords: 128, maxMetadataBytes: 65536, maxPageItems: 32 },
      stateToken,
    };
    if (scenario === "session-audit-future-cursor") {
      if (request.params.afterSeq !== "100") {
        throw new Error("session.requests future cursor was not serialized exactly");
      }
      result.records = [];
      result.nextAfterSeq = request.params.afterSeq;
    }
    await send(request, result);
    return;
  }
  if (request.method === "session.evidence") {
    const result = {
      schemaVersion: 2,
      records:
        scenario === "session-invalid-evidence-reason"
          ? [
              {
                seq: "1",
                atVirtualNs: "7",
                kind: "request_failed",
                requestId: "1",
                reason: "invented_failure_reason",
              },
            ]
          : [
              { seq: "1", atVirtualNs: "7", kind: "request_started", requestId: "1" },
              {
                seq: "2",
                atVirtualNs: "7",
                kind: "route_decided",
                requestId: "1",
                decision: "fixture_fulfill",
              },
              { seq: "3", atVirtualNs: "8", kind: "navigation_started", navigationId: "2" },
            ],
      firstRetainedSeq: "1",
      nextAfterSeq: "3",
      latestSeq: "3",
      complete: true,
      hasMore: false,
      bounds: { maxRecords: 256, maxMetadataBytes: 131072, maxPageItems: 32 },
      stateToken,
    };
    if (scenario === "session-audit-incomplete-without-drop") {
      result.complete = false;
    }
    if (scenario === "session-audit-has-more-without-records") {
      result.records = [];
      result.hasMore = true;
    }
    if (scenario === "session-audit-next-cursor-mismatch") {
      result.nextAfterSeq = "2";
    }
    if (scenario === "session-audit-nonincreasing-records") {
      result.records[1].seq = "1";
    }
    if (scenario === "session-audit-invalid-retention-order") {
      result.droppedThroughSeq = "1";
      result.firstRetainedSeq = "1";
    }
    if (scenario === "session-audit-missing-first-retained") {
      delete result.firstRetainedSeq;
    }
    if (scenario === "session-audit-record-before-retention") {
      result.firstRetainedSeq = "2";
    }
    if (scenario === "session-audit-latest-before-record") {
      result.latestSeq = "2";
    }
    if (scenario === "session-audit-future-cursor") {
      if (request.params.afterSeq !== "100") {
        throw new Error("session.evidence future cursor was not serialized exactly");
      }
      result.records = [];
      result.nextAfterSeq = request.params.afterSeq;
    }
    await send(request, result);
    return;
  }
  if (request.method === "session.close") {
    await send(request, { state: "closed" });
    process.stdin.pause();
    if (scenario === "close-linger") {
      setInterval(() => {}, 1_000);
      return;
    }
    setTimeout(() => process.exit(scenario === "close-nonzero" ? 17 : 0), 5);
  }
}

const input = readline.createInterface({ input: process.stdin, crlfDelay: Infinity });
input.on("line", (line) => {
  void handle(JSON.parse(line)).catch((error) => {
    console.error(error);
    process.exit(70);
  });
});
