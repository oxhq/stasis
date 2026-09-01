import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { inspect } from "node:util";

import {
  CONTROLLED_WEBAPP_V1_PROFILE,
  StasisAbortError,
  StasisCommandTimeoutError,
  StasisProcessError,
  StasisProtocolError,
  StasisStateError,
  StasisTransportError,
  launch,
  type App,
  type ClockOptions,
  type Runtime,
  type SettleOutcome,
  type TimeSurface,
} from "../src/index.js";

const fixture = fileURLToPath(new URL("./fixtures/fake-shell.mjs", import.meta.url));
const allTimeSurfaceSet = {
  window_timers: true,
  same_event_loop_iframe: true,
  java_script_date: true,
  performance: true,
  host_timestamp: true,
  update_rendering: true,
  animation_frame: true,
  document_timeline: true,
  worker: true,
  worklet: true,
  cross_event_loop_iframe: true,
  cross_event_loop_navigation: true,
  auxiliary_web_view: true,
  resource_thread_io: true,
  external_subscription: true,
  native_media: true,
  embedder_control: true,
  history_traversal: true,
} as const satisfies Record<TimeSurface, true>;
const allTimeSurfaces = Object.keys(allTimeSurfaceSet) as TimeSurface[];

async function openFake(
  context: { after(callback: () => void | Promise<void>): void },
  scenario = "normal",
  options: {
    maxStderrBytes?: number;
    closeTimeoutMs?: number;
    commandTimeoutMs?: number;
  } = {},
): Promise<{ runtime: Runtime; app: App }> {
  const runtime = await launch({
    executablePath: process.execPath,
    args: [fixture, scenario],
    ...options,
  });
  context.after(() => runtime.close());
  const app = await runtime.open("https://example.test/", {
    clock: {
      mode: "controlled",
      initialVirtualTimeNs: 7n,
      unixTimeOriginNs: 0n,
    },
  });
  return { runtime, app };
}

function recursiveErrorDiagnostics(error: unknown, seen = new Set<unknown>()): string {
  if (seen.has(error)) return "<cycle>";
  seen.add(error);
  const diagnostics = [String(error), inspect(error, { depth: 10 })];
  if (error instanceof Error) {
    diagnostics.push(error.message, error.stack ?? "");
    if (error.cause !== undefined) {
      diagnostics.push(recursiveErrorDiagnostics(error.cause, seen));
    }
  }
  return diagnostics.join("\n");
}

test("launch, open, native DOM operations, runtime control, and close use the linear API", async (context) => {
  const { runtime, app } = await openFake(context);
  assert.equal(runtime.info.protocolVersion, 1);
  assert.equal(runtime.info.limits.maxActiveEngineRequests, 1);
  assert.ok(runtime.pid !== undefined);
  assert.equal(app.url, "https://example.test/");
  assert.equal(app.clockMode, "controlled");
  assert.equal(app.profile, CONTROLLED_WEBAPP_V1_PROFILE);

  const initializeParams = (await app.evaluate("__initializeParams")) as {
    client: { name: string; version: string };
  };
  assert.deepEqual(initializeParams.client, {
    name: "@oxhq/stasis",
    version: "0.3.3",
  });

  const openParams = (await app.evaluate("__openParams")) as {
    url: string;
    clockMode: string;
    initialVirtualTimeNs: string;
    unixTimeOriginNs: string;
    profile: string;
  };
  assert.equal(openParams.clockMode, "controlled");
  assert.equal(openParams.initialVirtualTimeNs, "7");
  assert.equal(openParams.unixTimeOriginNs, "0");
  assert.equal(openParams.profile, CONTROLLED_WEBAPP_V1_PROFILE);

  const snapshot = await app.pending();
  assert.equal(snapshot.stateGeneration, 9007199254740993n);
  assert.equal(snapshot.virtualTimeNs, 18446744073709551625n);
  assert.equal(snapshot.sources[0]?.sourceId, "1");
  assert.equal(snapshot.sources[0]?.state, "open_ended");
  if (snapshot.sources[0]?.state === "open_ended") {
    assert.equal(snapshot.sources[0].openEnded.requestedPeriodNs, 5_000_000_000n);
  }
  assert.equal(snapshot.sources[1]?.state, "unsupported");
  if (snapshot.sources[1]?.state === "unsupported") {
    assert.equal(snapshot.sources[1].unsupported.reason, "throttled_rendering");
  }
  assert.deepEqual(snapshot.clock.unsupportedSurfaces, []);
  assert.equal(snapshot.network.counts.unclassifiedProducerIo, 1n);
  assert.equal(snapshot.network.active[0]?.sourceId, "3");
  assert.equal(snapshot.network.active[0]?.kind, "unclassified_producer_io");

  const expectedGeneration = (1n << 64n) - 1n;
  const activation = await app.activate("#start", expectedGeneration);
  assert.equal(activation.stateGeneration, 9007199254740996n);
  const fill = await app.fill(
    "#email",
    'gara+stasis@example.test & "friends"',
    expectedGeneration,
  );
  assert.equal(fill.stateGeneration, 9007199254740997n);
  const query = await app.query(".result", expectedGeneration);
  assert.equal(query.count, 18446744073709551616n);
  assert.equal(query.stateGeneration, 9007199254740998n);
  assert.equal(await app.text("#status", expectedGeneration), "ready");
  const extraction = await app.extract(
    {
      rootSelector: ".result",
      fields: [
        { name: "second", selector: ".detail", read: "html" },
        { name: "first", selector: ".title", read: "text" },
      ],
    },
    expectedGeneration,
  );
  assert.equal(extraction.stateGeneration, 9007199254740999n);
  assert.deepEqual(extraction.rows, [
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
  ]);
  const activateParams = (await app.evaluate("__activateParams")) as Record<string, unknown>;
  const fillParams = (await app.evaluate("__fillParams")) as Record<string, unknown>;
  const queryParams = (await app.evaluate("__queryParams")) as Record<string, unknown>;
  const textParams = (await app.evaluate("__textParams")) as Record<string, unknown>;
  const extractParams = (await app.evaluate("__extractParams")) as Record<string, unknown>;
  assert.deepEqual(activateParams, {
    selector: "#start",
    expectedGeneration: "18446744073709551615",
  });
  assert.deepEqual(textParams, {
    selector: "#status",
    expectedGeneration: "18446744073709551615",
  });
  assert.deepEqual(fillParams, {
    selector: "#email",
    expectedGeneration: "18446744073709551615",
    value: 'gara+stasis@example.test & "friends"',
  });
  assert.deepEqual(queryParams, {
    selector: ".result",
    expectedGeneration: "18446744073709551615",
  });
  assert.deepEqual(extractParams, {
    rootSelector: ".result",
    fields: [
      { name: "second", selector: ".detail", read: "html" },
      { name: "first", selector: ".title", read: "text" },
    ],
    expectedGeneration: "18446744073709551615",
  });

  const advanced = await app.advanceToNext();
  assert.equal(advanced.outcome, "advanced");
  if (advanced.outcome === "advanced") {
    assert.equal(advanced.fromVirtualTimeNs, 18446744073709551625n);
  }
  await app.close();
});

test("open defaults to the named controlled profile and Real mode is explicit", async (context) => {
  const controlledRuntime = await launch({
    executablePath: process.execPath,
    args: [fixture, "normal"],
  });
  context.after(() => controlledRuntime.close());
  const controlled = await controlledRuntime.open("https://example.test/");
  assert.equal(controlled.clockMode, "controlled");
  assert.equal(controlled.boundary, "controlled_ready");
  assert.equal(controlled.profile, CONTROLLED_WEBAPP_V1_PROFILE);
  const controlledParams = (await controlled.evaluate("__openParams")) as Record<
    string,
    unknown
  >;
  assert.deepEqual(controlledParams, {
    url: "https://example.test/",
    clockMode: "controlled",
    profile: CONTROLLED_WEBAPP_V1_PROFILE,
    initialVirtualTimeNs: "0",
    unixTimeOriginNs: "0",
  });
  await controlled.close();

  const realRuntime = await launch({
    executablePath: process.execPath,
    args: [fixture, "normal"],
  });
  context.after(() => realRuntime.close());
  const real = await realRuntime.open("https://example.test/", { clock: { mode: "real" } });
  assert.equal(real.clockMode, "real");
  assert.equal(real.boundary, "load_complete");
  assert.equal(real.profile, null);
  await real.close();
});

test("pending decodes every fail-closed async surface", async (context) => {
  const { app } = await openFake(context, "unsupported-async-surfaces");
  const snapshot = await app.pending();
  assert.deepEqual(snapshot.clock.unsupportedSurfaces, [
    "external_subscription",
    "native_media",
    "embedder_control",
  ]);
});

test("pending decodes every native time-surface wire spelling", async (context) => {
  const { app } = await openFake(context, "all-time-surfaces");
  const snapshot = await app.pending();
  assert.deepEqual(snapshot.clock.unsupportedSurfaces, allTimeSurfaces);
});

test("native DOM methods require advertised capabilities and exact u64 generations", async (context) => {
  const unavailable = await openFake(context, "no-actions");
  await assert.rejects(
    unavailable.app.activate("#start", 0n),
    (error) => error instanceof StasisStateError && /action\.activate/u.test(error.message),
  );
  await assert.rejects(
    unavailable.app.text("#status", 0n),
    (error) => error instanceof StasisStateError && /dom\.text/u.test(error.message),
  );
  await assert.rejects(
    unavailable.app.fill("#email", "a@example.test", 0n),
    (error) => error instanceof StasisStateError && /action\.fill/u.test(error.message),
  );
  await assert.rejects(
    unavailable.app.query(".result", 0n),
    (error) => error instanceof StasisStateError && /dom\.query/u.test(error.message),
  );
  await assert.rejects(
    unavailable.app.extract({ rootSelector: ".result", fields: [] }, 0n),
    (error) => error instanceof StasisStateError && /dom\.extract/u.test(error.message),
  );
  await unavailable.app.close();

  const { app } = await openFake(context);
  await assert.rejects(app.activate("#start", -1n), RangeError);
  await assert.rejects(app.activate("#start", 1n << 64n), RangeError);
  await assert.rejects(app.text("#status", 1 as unknown as bigint), RangeError);
  await assert.rejects(app.text(7 as unknown as string, 0n), TypeError);
  await assert.rejects(app.fill("#email", 7 as unknown as string, 0n), TypeError);
  await assert.rejects(app.fill("#email", "value", undefined as unknown as bigint), RangeError);
  await assert.rejects(app.query(".result", undefined as unknown as bigint), RangeError);
  await assert.rejects(
    app.extract(
      {
        rootSelector: ".result",
        fields: [{ name: "title", selector: ".title", read: "attribute" }],
      } as never,
      0n,
    ),
    TypeError,
  );
  await assert.rejects(
    app.extract({ rootSelector: ".result", fields: [] }, 1n << 64n),
    RangeError,
  );
  assert.equal(await app.text("#status", 0n), "ready");
  await app.close();
});

const invalidNativeResults: ReadonlyArray<
  readonly [string, string, (app: App) => Promise<unknown>]
> = [
  ["invalid-activation-result", "activate", (app) => app.activate("#start", 0n)],
  ["invalid-fill-result", "fill", (app) => app.fill("#email", "value", 0n)],
  ["invalid-query-result", "query", (app) => app.query(".result", 0n)],
  ["invalid-text-result", "text", (app) => app.text("#status", 0n)],
  [
    "invalid-extract-result",
    "extract",
    (app) => app.extract({ rootSelector: ".result", fields: [] }, 0n),
  ],
];

for (const [scenario, operation, command] of invalidNativeResults) {
  test(`strict native result decoding rejects ${operation} shape drift`, async (context) => {
    const { app } = await openFake(context, scenario);
    await assert.rejects(command(app), (error) => {
      assert.ok(error instanceof StasisTransportError);
      assert.equal(error.code, "invalid_result");
      return true;
    });
  });
}

test("a pre-aborted launch does not spawn the executable", async () => {
  const controller = new AbortController();
  controller.abort("already done");
  await assert.rejects(
    launch({
      executablePath: "/definitely/not/a/stasis/executable",
      signal: controller.signal,
    }),
    StasisAbortError,
  );
});

test("an explicit executablePath bypasses managed runtime acquisition", async (context) => {
  const runtime = await launch({
    executablePath: process.execPath,
    args: [fixture, "normal"],
    // A NUL path would fail immediately if the managed resolver touched it.
    runtimeCacheDirectory: "\0",
  });
  context.after(() => runtime.close());
  assert.equal(runtime.info.implementation.name, "fake-stasis");
  await runtime.close();
});

test("a spawn error rejects promptly without waiting for termination timeout", async () => {
  let timer: ReturnType<typeof setTimeout> | undefined;
  try {
    await Promise.race([
      assert.rejects(
        launch({
          executablePath: "/definitely/not/a/stasis/executable",
          closeTimeoutMs: 5_000,
        }),
        StasisProcessError,
      ),
      new Promise<never>((_resolve, reject) => {
        timer = setTimeout(() => reject(new Error("spawn error did not reject promptly")), 500);
      }),
    ]);
  } finally {
    if (timer !== undefined) clearTimeout(timer);
  }
});

test("open rejects invalid clock input and a mismatched runtime clock", async (context) => {
  const runtime = await launch({ executablePath: process.execPath, args: [fixture, "normal"] });
  context.after(() => runtime.close());
  await assert.rejects(
    runtime.open("https://example.test/", {
      clock: { mode: "warp" } as unknown as ClockOptions,
    }),
    TypeError,
  );

  const mismatched = await launch({
    executablePath: process.execPath,
    args: [fixture, "clock-mismatch"],
  });
  context.after(() => mismatched.close());
  await assert.rejects(
    mismatched.open("https://example.test/", { clock: { mode: "controlled" } }),
    (error) => {
      assert.ok(error instanceof StasisTransportError);
      assert.equal(error.code, "invalid_result");
      return true;
    },
  );
});

test("one in-flight request preserves FIFO and queued abort removes only that command", async (context) => {
  const { app } = await openFake(context);
  const first = app.evaluate("hold") as Promise<{ ordinal: number }>;
  const controller = new AbortController();
  const queued = app.pending({ signal: controller.signal });
  const queuedAssertion = assert.rejects(queued, StasisAbortError);
  const third = app.evaluate("after") as Promise<{ ordinal: number }>;
  controller.abort("not needed");

  assert.equal((await first).ordinal, 1);
  await queuedAssertion;
  assert.equal((await third).ordinal, 2);
  await app.close();
});

test("aborting a written command fail-stops the process and the app", async (context) => {
  const { app } = await openFake(context, "abort-active");
  const controller = new AbortController();
  const settlement = app.settle({}, { signal: controller.signal });
  const assertion = assert.rejects(settlement, (error) => {
    assert.ok(error instanceof StasisAbortError);
    assert.equal(error.name, "AbortError");
    assert.equal(error.fatal, true);
    assert.equal(error.stateEffect, "indeterminate");
    assert.equal(error.method, "runtime.settle");
    assert.match(error.requestId ?? "", /^[1-9][0-9]*$/u);
    return true;
  });
  setImmediate(() => controller.abort("stop"));
  await assertion;
  await assert.rejects(app.pending(), StasisAbortError);
});

test("a non-mutating command timeout fail-stops with known state effect", async (context) => {
  const { app } = await openFake(context, "command-hang-read", {
    commandTimeoutMs: 1_000,
  });
  let terminal: StasisCommandTimeoutError | undefined;
  await assert.rejects(app.pending({ timeoutMs: 25 }), (error) => {
    assert.ok(error instanceof StasisCommandTimeoutError);
    terminal = error;
    assert.equal(error.code, "command_timeout");
    assert.equal(error.fatal, true);
    assert.equal(error.stateEffect, "none");
    assert.equal(error.method, "runtime.pending");
    assert.equal(error.timeoutMs, 25);
    return true;
  });
  await assert.rejects(app.pending(), (error) => error === terminal);
});

test("a mutating command timeout is fatal with indeterminate state effect", async (context) => {
  const { app } = await openFake(context, "command-hang-mutate", {
    commandTimeoutMs: 5_000,
  });
  await assert.rejects(app.activate("#submit", 1n, { timeoutMs: 25 }), (error) => {
    assert.ok(error instanceof StasisCommandTimeoutError);
    assert.equal(error.code, "command_timeout");
    assert.equal(error.fatal, true);
    assert.equal(error.stateEffect, "indeterminate");
    assert.equal(error.method, "action.activate");
    assert.equal(error.timeoutMs, 25);
    return true;
  });
  await assert.rejects(app.pending(), StasisCommandTimeoutError);
});

for (const [scenario, code] of [
  ["malformed", "invalid_json"],
  ["invalid-utf8", "invalid_utf8"],
  ["duplicate", "duplicate_member"],
  ["unmatched", "unmatched_response"],
  ["bad-sequence", "wire_sequence_mismatch"],
] as const) {
  test(`strict protocol rejects ${scenario} output`, async (context) => {
    const { app } = await openFake(context, scenario);
    await assert.rejects(app.pending(), (error) => {
      assert.ok(error instanceof StasisTransportError);
      assert.equal(error.code, code);
      if (scenario === "malformed") {
        assert.equal(recursiveErrorDiagnostics(error).includes("sensitive-invalid-json-canary"), false);
      }
      if (scenario === "invalid-utf8") {
        assert.equal(recursiveErrorDiagnostics(error).includes("sensitive-invalid-utf8-canary"), false);
      }
      if (scenario === "duplicate") {
        assert.equal(error.message.includes("sensitive-duplicate-canary"), false);
      }
      return true;
    });
  });
}

test("nested pending shape mismatches fail-stop instead of escaping the declared type", async (context) => {
  const { app } = await openFake(context, "invalid-pending");
  await assert.rejects(app.pending(), (error) => {
    assert.ok(error instanceof StasisTransportError);
    assert.equal(error.code, "invalid_result");
    return true;
  });
  await assert.rejects(app.pending(), StasisTransportError);
});

test("source aliases stay opaque strings and reject non-canonical decimals", async (context) => {
  const { app } = await openFake(context, "invalid-source-id");
  await assert.rejects(app.pending(), (error) => {
    assert.ok(error instanceof StasisTransportError);
    assert.equal(error.code, "invalid_result");
    return true;
  });
});

test("settlement outcome payload invariants are enforced", async (context) => {
  const { app } = await openFake(context, "invalid-settle");
  await assert.rejects(app.settle(), (error) => {
    assert.ok(error instanceof StasisTransportError);
    assert.equal(error.code, "invalid_result");
    return true;
  });
});

test("child death includes exit status and only the bounded stderr tail", async (context) => {
  const { app } = await openFake(context, "process-death", { maxStderrBytes: 16 });
  await assert.rejects(app.pending(), (error) => {
    assert.ok(error instanceof StasisProcessError);
    assert.equal(error.exitCode, 17);
    assert.ok(Buffer.byteLength(error.stderrTail) <= 16);
    assert.ok(error.stderrTail.endsWith("-TAIL-END"));
    return true;
  });
});

test(
  "unexpected stdout EOF fail-stops even while the child remains alive",
  {
    skip:
      process.platform === "win32"
        ? "Node keeps the child stdout pipe open on Windows until process exit, so a live-child EOF fixture is not representable"
        : false,
  },
  async (context) => {
    const { app } = await openFake(context, "stdout-eof-linger");
    await assert.rejects(app.pending(), (error) => {
      assert.ok(error instanceof StasisTransportError);
      assert.equal(error.code, "unexpected_stdout_eof");
      return true;
    });
  },
);

test("graceful close waits for and validates the process exit", async (context) => {
  const { app } = await openFake(context, "close-nonzero");
  await assert.rejects(app.close(), (error) => {
    assert.ok(error instanceof StasisProcessError);
    assert.equal(error.exitCode, 17);
    return true;
  });
});

test("graceful close fail-stops a child that does not exit", async (context) => {
  const { app } = await openFake(context, "close-linger", { closeTimeoutMs: 25 });
  await assert.rejects(app.close(), (error) => {
    assert.ok(error instanceof StasisTransportError);
    assert.equal(error.code, "close_timeout");
    return true;
  });
});

test("Runtime.close interrupts an in-progress graceful close immediately", async (context) => {
  const { runtime, app } = await openFake(context, "close-linger", { closeTimeoutMs: 5_000 });
  const closing = app.close();
  const assertion = assert.rejects(closing, StasisStateError);
  await new Promise((resolve) => setTimeout(resolve, 20));
  await runtime.close();
  await assertion;
});

test("Runtime.close waits for exit and escalates past an ignored SIGTERM", async (context) => {
  const { runtime } = await openFake(context, "ignore-sigterm");
  const pid = runtime.pid;
  assert.ok(pid !== undefined);
  await runtime.close();
  assert.throws(
    () => process.kill(pid, 0),
    (error: unknown) =>
      typeof error === "object" &&
      error !== null &&
      "code" in error &&
      error.code === "ESRCH",
  );
});

test("nonfatal protocol errors stay typed and do not poison the FIFO", async (context) => {
  const { app } = await openFake(context);
  await assert.rejects(app.evaluate("protocol-error"), (error) => {
    assert.ok(error instanceof StasisProtocolError);
    assert.equal(error.code, "evaluation_failed");
    assert.equal(error.stateEffect, "none");
    assert.equal(error.fatal, false);
    assert.equal(error.details, undefined);
    return true;
  });
  const alive = (await app.evaluate("alive")) as { expression: string };
  assert.equal(alive.expression, "alive");
  await app.close();
});

test("structured protocol error details are decoded exactly and exposed read-only", async (context) => {
  const { app } = await openFake(context);
  await assert.rejects(app.evaluate("protocol-error-details"), (error) => {
    assert.ok(error instanceof StasisProtocolError);
    assert.deepEqual(error.details, {
      actual: "21",
      limit: 20,
      reasons: ["replacement", null],
      retryable: false,
    });
    assert.equal(Object.isFrozen(error.details), true);
    assert.equal(Object.isFrozen(error.details?.reasons), true);
    return true;
  });
  await app.close();
});

test("a non-object protocol error details member is a fatal wire violation", async (context) => {
  const { app } = await openFake(context);
  await assert.rejects(app.evaluate("protocol-error-invalid-details"), (error) => {
    assert.ok(error instanceof StasisTransportError);
    assert.equal(error.code, "invalid_envelope");
    assert.match(error.message, /details must be an object/iu);
    return true;
  });
});

test("an indeterminate protocol outcome fail-stops even when the server marks it nonfatal", async (context) => {
  const { app } = await openFake(context);
  await assert.rejects(app.evaluate("indeterminate-error"), (error) => {
    assert.ok(error instanceof StasisProtocolError);
    assert.equal(error.stateEffect, "indeterminate");
    return true;
  });
  await assert.rejects(app.pending(), StasisProtocolError);
});

test("every settlement discriminant is decoded without losing exact counters", async (context) => {
  const { app } = await openFake(context, "settle-outcomes");
  const expected: SettleOutcome[] = [
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

  for (const outcome of expected) {
    const result = await app.settle({
      persistentWork: "report",
      wallIoTimeoutNs: 20n,
      maxVirtualTimeNs: 30n,
      maxControlTurns: 40n,
    });
    assert.equal(result.outcome, outcome);
    assert.equal(result.wallTimeNs, 25n);
    assert.equal(result.processed.controlTurns, 3n);
    assert.equal(result.processed.tasks, 4n);
    assert.equal(result.processed.microtasks, 5n);
    assert.equal(result.processed.renderingOpportunities, 6n);
    assert.equal(result.processed.mutations, 7n);
    assert.equal(result.persistentWork[0]?.count, 2n);
    assert.equal(result.persistentWork[0]?.requestedPeriodNs, 5_000_000_000n);
    assert.equal(result.snapshot.domEpoch, 9007199254740994n);
  }
  const settleParams = (await app.evaluate("__settleParams")) as Record<string, unknown>;
  assert.deepEqual(settleParams, {
    persistentWork: "report",
    maxVirtualTimeNs: "30",
    maxControlTurns: "40",
    wallIoTimeoutNs: "20",
  });
  await app.close();
});

for (const [scenario, operation] of [
  ["settle-summary-mismatch", "settle"],
  ["advance-summary-mismatch", "advance"],
] as const) {
  test(`${operation} summary fields must correlate with its snapshot`, async (context) => {
    const { app } = await openFake(context, scenario);
    const command = operation === "settle" ? app.settle() : app.advanceToNext();
    await assert.rejects(command, (error) => {
      assert.ok(error instanceof StasisTransportError);
      assert.equal(error.code, "invalid_result");
      return true;
    });
  });
}
