import assert from "node:assert/strict";
import { getEventListeners } from "node:events";
import test from "node:test";

import { StasisAbortError } from "../src/errors.js";
import {
  FreshSessionPool,
  SessionPoolClosedError,
  SessionPoolQueueFullError,
  combineAbortSignals,
  type OwnedSessionProcess,
} from "../src/session-pool.js";

interface FakeSession {
  readonly processId: number;
  readonly request: string;
  readonly stateToken: string;
}

interface FactoryHarness {
  readonly starts: string[];
  readonly closes: number[];
  readonly terminations: number[];
  readonly create: (request: string) => Promise<OwnedSessionProcess<FakeSession>>;
}

function factoryHarness(): FactoryHarness {
  let nextProcessId = 1;
  const starts: string[] = [];
  const closes: number[] = [];
  const terminations: number[] = [];
  return {
    starts,
    closes,
    terminations,
    create: async (request) => {
      starts.push(request);
      const processId = nextProcessId++;
      return {
        session: {
          processId,
          request,
          stateToken: `process-${processId}-token`,
        },
        close: async () => {
          closes.push(processId);
        },
        terminate: async () => {
          terminations.push(processId);
        },
      };
    },
  };
}

test("pool is FIFO and replaces every terminal session with a fresh process", async () => {
  const harness = factoryHarness();
  const pool = new FreshSessionPool({
    maxProcesses: 1,
    maxQueue: 2,
    create: harness.create,
  });

  const first = await pool.acquire("first");
  const secondPromise = pool.acquire("second");
  const thirdPromise = pool.acquire("third");
  assert.deepEqual(harness.starts, ["first"]);
  assert.equal(pool.queuedAcquisitions, 2);

  await first.release();
  const second = await secondPromise;
  assert.deepEqual(harness.starts, ["first", "second"]);
  assert.notEqual(second.session.processId, first.session.processId);
  assert.notEqual(second.session.stateToken, first.session.stateToken);

  await second.release();
  const third = await thirdPromise;
  assert.deepEqual(harness.starts, ["first", "second", "third"]);
  assert.notEqual(third.session.processId, second.session.processId);
  await third.release();

  assert.deepEqual(harness.closes, [1, 2, 3]);
  assert.deepEqual(harness.terminations, []);
  assert.equal(pool.activeProcesses, 0);
  await pool.close();
});

test("queued acquisition is abort-aware and queue overflow rejects before enqueue", async () => {
  const harness = factoryHarness();
  const pool = new FreshSessionPool({
    maxProcesses: 1,
    maxQueue: 1,
    create: harness.create,
  });
  const first = await pool.acquire("first");
  const controller = new AbortController();
  const queued = pool.acquire("queued", { signal: controller.signal });
  await assert.rejects(
    pool.acquire("overflow"),
    (error) => error instanceof SessionPoolQueueFullError,
  );

  controller.abort("not-needed");
  await assert.rejects(queued, (error) => {
    assert.ok(error instanceof StasisAbortError);
    assert.equal(error.reason, "not-needed");
    return true;
  });
  assert.equal(pool.queuedAcquisitions, 0);

  const replacementPromise = pool.acquire("replacement");
  await first.release();
  const replacement = await replacementPromise;
  assert.deepEqual(harness.starts, ["first", "replacement"]);
  await replacement.release();
  await pool.close();
});

test("run poisons a failed lease, never retries it, and admits a fresh replacement", async () => {
  const harness = factoryHarness();
  const pool = new FreshSessionPool({
    maxProcesses: 1,
    maxQueue: 1,
    create: harness.create,
  });

  const expected = new Error("written command may have happened");
  await assert.rejects(
    pool.run("failed", async () => {
      throw expected;
    }),
    (error) => error === expected,
  );
  assert.deepEqual(harness.starts, ["failed"]);
  assert.deepEqual(harness.closes, []);
  assert.deepEqual(harness.terminations, [1]);

  const processId = await pool.run("next", (session) => session.processId);
  assert.equal(processId, 2);
  assert.deepEqual(harness.starts, ["failed", "next"]);
  assert.deepEqual(harness.closes, [2]);
  await pool.close();
});

test("concurrency is bounded while close rejects queued admission and drains leases", async () => {
  const harness = factoryHarness();
  const pool = new FreshSessionPool({
    maxProcesses: 2,
    maxQueue: 2,
    create: harness.create,
  });
  const first = await pool.acquire("first");
  const second = await pool.acquire("second");
  const queued = pool.acquire("queued");
  assert.equal(pool.activeProcesses, 2);
  assert.equal(pool.queuedAcquisitions, 1);

  const closePromise = pool.close();
  await assert.rejects(queued, (error) => error instanceof SessionPoolClosedError);
  await assert.rejects(
    pool.acquire("late"),
    (error) => error instanceof SessionPoolClosedError,
  );
  await first.release();
  await second.release();
  await closePromise;
  assert.equal(pool.activeProcesses, 0);
});

test("a synchronous factory failure frees its process slot and does not block close", async () => {
  const expected = new Error("factory failed before returning a promise");
  const pool = new FreshSessionPool<string, FakeSession>({
    maxProcesses: 1,
    maxQueue: 0,
    create: (): Promise<OwnedSessionProcess<FakeSession>> => {
      throw expected;
    },
  });

  await assert.rejects(pool.acquire("failing"), (error) => error === expected);
  assert.equal(pool.activeProcesses, 0);
  await pool.close();
});

test("pool bounds must be finite safe integers", () => {
  const harness = factoryHarness();
  for (const maxProcesses of [0, -1, Number.POSITIVE_INFINITY, 1.5]) {
    assert.throws(
      () => new FreshSessionPool({ maxProcesses, maxQueue: 0, create: harness.create }),
      RangeError,
    );
  }
  for (const maxQueue of [-1, Number.POSITIVE_INFINITY, 1.5]) {
    assert.throws(
      () => new FreshSessionPool({ maxProcesses: 1, maxQueue, create: harness.create }),
      RangeError,
    );
  }
});

test("signal fan-in works without the Node 20.3 AbortSignal.any API", () => {
  const anyDescriptor = Object.getOwnPropertyDescriptor(AbortSignal, "any");
  Object.defineProperty(AbortSignal, "any", {
    configurable: true,
    value: undefined,
    writable: true,
  });

  try {
    const absent = combineAbortSignals();
    assert.equal(absent.signal, undefined);
    absent.dispose();

    const first = new AbortController();
    const second = new AbortController();
    const single = combineAbortSignals(undefined, first.signal);
    assert.equal(single.signal, first.signal);
    single.dispose();

    const combined = combineAbortSignals(first.signal, second.signal);
    assert.ok(combined.signal);
    assert.equal(combined.signal.aborted, false);

    const reason = { source: "second" };
    second.abort(reason);
    assert.equal(combined.signal.aborted, true);
    assert.equal(combined.signal.reason, reason);
    assert.equal(getEventListeners(first.signal, "abort").length, 0);
    assert.equal(getEventListeners(second.signal, "abort").length, 0);

    first.abort("later");
    assert.equal(combined.signal.reason, reason);
    combined.dispose();

    const alreadyAborted = new AbortController();
    alreadyAborted.abort("already-aborted");
    const immediate = combineAbortSignals(first.signal, alreadyAborted.signal);
    assert.ok(immediate.signal);
    assert.equal(immediate.signal.aborted, true);
    assert.equal(immediate.signal.reason, "later");
    immediate.dispose();
  } finally {
    if (anyDescriptor === undefined) {
      delete (AbortSignal as { any?: unknown }).any;
    } else {
      Object.defineProperty(AbortSignal, "any", anyDescriptor);
    }
  }
});

test("signal fan-in deduplicates sources and dispose removes retained listeners", () => {
  const shared = new AbortController();
  const other = new AbortController();
  const combined = combineAbortSignals(shared.signal, shared.signal, other.signal);

  assert.ok(combined.signal);
  assert.notEqual(combined.signal, shared.signal);
  assert.equal(getEventListeners(shared.signal, "abort").length, 1);
  assert.equal(getEventListeners(other.signal, "abort").length, 1);

  combined.dispose();
  assert.equal(getEventListeners(shared.signal, "abort").length, 0);
  assert.equal(getEventListeners(other.signal, "abort").length, 0);
  assert.equal(combined.signal.aborted, false);
});
