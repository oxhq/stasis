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

async function sendError(request, stateEffect = "none") {
  wireSequence += 1n;
  await writeSplit(
    `${JSON.stringify({
      v: 1,
      type: "response",
      wireSeq: wireSequence.toString(),
      id: request.id,
      sessionId,
      event: null,
      error: {
        code: "evaluation_failed",
        message: "synthetic evaluation failure",
        fatal: false,
        stateEffect,
      },
    })}\n`,
  );
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
            "runtime.pending",
            "runtime.settle",
            "runtime.advance_to_next",
            "session.close",
          ],
          clockModes: ["real", "controlled"],
          profiles: ["controlled-webapp-v1"],
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
    await send(
      request,
      scenario === "invalid-fill-result"
        ? { stateGeneration: "01" }
        : { stateGeneration: "9007199254740997" },
    );
    return;
  }
  if (request.method === "dom.query") {
    lastQueryParams = request.params;
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
      request.params.expression === "indeterminate-error"
    ) {
      await sendError(
        request,
        request.params.expression === "indeterminate-error" ? "indeterminate" : "none",
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
    if (scenario === "command-hang-read") return;
    if (scenario === "malformed") {
      process.stdout.write('{"v":1\n');
      return;
    }
    if (scenario === "duplicate") {
      wireSequence += 1n;
      process.stdout.write(
        `{"v":1,"type":"response","wireSeq":"${wireSequence}","id":"${request.id}","id":"${request.id}","sessionId":"${sessionId}","result":{}}\n`,
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
    await send(request, pending());
    return;
  }
  if (request.method === "runtime.settle") {
    if (scenario === "abort-active") return;
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
    await send(request, result);
    return;
  }
  if (request.method === "runtime.advance_to_next") {
    const snapshot = pending();
    snapshot.virtualTimeNs = "18446744073709551626";
    snapshot.stateGeneration = "9007199254740997";
    if (scenario === "advance-summary-mismatch") snapshot.stateGeneration = "1";
    await send(request, {
      outcome: "advanced",
      fromVirtualTimeNs: "18446744073709551625",
      virtualTimeNs: "18446744073709551626",
      stateGeneration: "9007199254740997",
      snapshot,
    });
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
