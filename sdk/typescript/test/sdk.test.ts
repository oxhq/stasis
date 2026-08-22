import assert from "node:assert/strict";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  StasisAbortError,
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
} as const satisfies Record<TimeSurface, true>;
const allTimeSurfaces = Object.keys(allTimeSurfaceSet) as TimeSurface[];

async function openFake(
  context: { after(callback: () => void | Promise<void>): void },
  scenario = "normal",
  options: { maxStderrBytes?: number; closeTimeoutMs?: number } = {},
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

test("launch, open, native DOM operations, runtime control, and close use the linear API", async (context) => {
  const { runtime, app } = await openFake(context);
  assert.equal(runtime.info.protocolVersion, 1);
  assert.equal(runtime.info.limits.maxActiveEngineRequests, 1);
  assert.ok(runtime.pid !== undefined);
  assert.equal(app.url, "https://example.test/");
  assert.equal(app.clockMode, "controlled");

  const initializeParams = (await app.evaluate("__initializeParams")) as {
    client: { name: string; version: string };
  };
  assert.deepEqual(initializeParams.client, {
    name: "@oxhq/stasis",
    version: "0.1.0-alpha.0",
  });

  const openParams = (await app.evaluate("__openParams")) as {
    url: string;
    clockMode: string;
    initialVirtualTimeNs: string;
    unixTimeOriginNs: string;
  };
  assert.equal(openParams.clockMode, "controlled");
  assert.equal(openParams.initialVirtualTimeNs, "7");
  assert.equal(openParams.unixTimeOriginNs, "0");

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
  await app.activate("#start", expectedGeneration);
  assert.equal(await app.text("#status", expectedGeneration), "ready");
  const activateParams = (await app.evaluate("__activateParams")) as Record<string, unknown>;
  const textParams = (await app.evaluate("__textParams")) as Record<string, unknown>;
  assert.deepEqual(activateParams, {
    selector: "#start",
    expectedGeneration: "18446744073709551615",
  });
  assert.deepEqual(textParams, {
    selector: "#status",
    expectedGeneration: "18446744073709551615",
  });

  const advanced = await app.advanceToNext();
  assert.equal(advanced.outcome, "advanced");
  if (advanced.outcome === "advanced") {
    assert.equal(advanced.fromVirtualTimeNs, 18446744073709551625n);
  }
  await app.close();
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
  await unavailable.app.close();

  const { app } = await openFake(context);
  await assert.rejects(app.activate("#start", -1n), RangeError);
  await assert.rejects(app.activate("#start", 1n << 64n), RangeError);
  await assert.rejects(app.text("#status", 1 as unknown as bigint), RangeError);
  await assert.rejects(app.text(7 as unknown as string, 0n), TypeError);
  assert.equal(await app.text("#status", 0n), "ready");
  await app.close();
});

for (const [scenario, operation] of [
  ["invalid-activation-result", "activate"],
  ["invalid-text-result", "text"],
] as const) {
  test(`strict native result decoding rejects ${operation} shape drift`, async (context) => {
    const { app } = await openFake(context, scenario);
    const command =
      operation === "activate" ? app.activate("#start", 0n) : app.text("#status", 0n);
    await assert.rejects(command, (error) => {
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
    return true;
  });
  setImmediate(() => controller.abort("stop"));
  await assertion;
  await assert.rejects(app.pending(), StasisAbortError);
});

for (const [scenario, code] of [
  ["malformed", "invalid_json"],
  ["duplicate", "duplicate_member"],
  ["unmatched", "unmatched_response"],
  ["bad-sequence", "wire_sequence_mismatch"],
] as const) {
  test(`strict protocol rejects ${scenario} output`, async (context) => {
    const { app } = await openFake(context, scenario);
    await assert.rejects(app.pending(), (error) => {
      assert.ok(error instanceof StasisTransportError);
      assert.equal(error.code, code);
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

test("unexpected stdout EOF fail-stops even while the child remains alive", async (context) => {
  const { app } = await openFake(context, "stdout-eof-linger");
  await assert.rejects(app.pending(), (error) => {
    assert.ok(error instanceof StasisTransportError);
    assert.equal(error.code, "unexpected_stdout_eof");
    return true;
  });
});

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
    return true;
  });
  const alive = (await app.evaluate("alive")) as { expression: string };
  assert.equal(alive.expression, "alive");
  await app.close();
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
