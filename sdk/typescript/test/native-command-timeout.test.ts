import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createServer, type Server } from "node:http";
import type { AddressInfo } from "node:net";
import test from "node:test";

import {
  CONTROLLED_WEBAPP_V1_PROFILE,
  StasisCommandTimeoutError,
  StasisStateError,
  launch,
  type Runtime,
} from "../src/index.js";

const NATIVE_BINARY = process.env.STASIS_NORTH_STAR_BINARY;
const FIXTURE = await readFile(
  new URL("./fixtures/synchronous-infinite-click.html", import.meta.url),
);
const ACTIVATE_TIMEOUT_MS = 250;

test(
  "a native synchronous click hang fail-stops and reaps the runtime",
  {
    skip:
      NATIVE_BINARY === undefined
        ? "set STASIS_NORTH_STAR_BINARY to the stasis executable for the native fail-stop proof"
        : false,
    timeout: 60_000,
  },
  async () => {
    assert.ok(NATIVE_BINARY, "STASIS_NORTH_STAR_BINARY must be non-empty");
    const fixture = await startFixtureServer();
    let runtime: Runtime | undefined;

    try {
      runtime = await launch({
        executablePath: NATIVE_BINARY,
        // Bootstrap and initial settlement can share a loaded CI host with the North Star's
        // independent native runtimes. Only the hanging mutation uses the deliberately short
        // per-command deadline below.
        commandTimeoutMs: 20_000,
        closeTimeoutMs: 2_000,
      });
      const pid = runtime.pid;
      assert.ok(pid !== undefined, "spawned runtime did not expose its process ID");

      const app = await runtime.open(fixture.url, {
        profile: CONTROLLED_WEBAPP_V1_PROFILE,
        clock: {
          mode: "controlled",
          initialVirtualTimeNs: 1_000_000_000n,
          unixTimeOriginNs: 0n,
        },
      });
      const initial = await app.settle({
        persistentWork: "report",
        maxVirtualTimeNs: 1_000_000_000n,
        maxControlTurns: 10_000n,
        wallIoTimeoutNs: 5_000_000_000n,
      });
      assert.equal(initial.outcome, "quiescent");
      assert.equal(await app.text("#status", initial.stateGeneration), "ready");

      let terminal: StasisCommandTimeoutError | undefined;
      await assert.rejects(
        app.activate("#spin", initial.stateGeneration, { timeoutMs: ACTIVATE_TIMEOUT_MS }),
        (error: unknown) => {
          assert.ok(error instanceof StasisCommandTimeoutError);
          terminal = error;
          assert.equal(error.code, "command_timeout");
          assert.equal(error.fatal, true);
          assert.equal(error.stateEffect, "indeterminate");
          assert.equal(error.method, "action.activate");
          assert.equal(error.timeoutMs, ACTIVATE_TIMEOUT_MS);
          assert.match(error.requestId, /^[1-9][0-9]*$/u);
          return true;
        },
      );
      assert.ok(terminal !== undefined, "activate did not expose its terminal timeout");

      await assert.rejects(
        app.pending(),
        (error: unknown) => error === terminal,
        "the poisoned app accepted another command",
      );
      await assert.rejects(
        app.close(),
        (error: unknown) => error === terminal,
        "the poisoned app attempted a graceful protocol close",
      );
      await assert.rejects(
        runtime.open(fixture.url, {
          profile: CONTROLLED_WEBAPP_V1_PROFILE,
          clock: { mode: "controlled" },
        }),
        StasisStateError,
        "the runtime accepted a second app after fail-stop",
      );

      // terminate() resolves only after the owned child emits exit/close (with SIGKILL escalation
      // bounded by closeTimeoutMs). A single signal-0 observation then proves it was reaped; it is
      // not a polling loop and is not used as progress authority.
      await runtime.close();
      await runtime.close();
      assertProcessIsGone(pid);
    } finally {
      await runtime?.close().catch(() => undefined);
      await fixture.close();
    }
  },
);

async function startFixtureServer(): Promise<{ url: string; close(): Promise<void> }> {
  const server = createServer((request, response) => {
    const path = new URL(request.url ?? "/", "http://127.0.0.1").pathname;
    if (request.method === "GET" && path === "/") {
      response.writeHead(200, {
        "content-type": "text/html; charset=utf-8",
        "content-length": FIXTURE.byteLength.toString(),
        connection: "close",
        "cache-control": "no-store",
      });
      response.end(FIXTURE);
      return;
    }
    const body = Buffer.from("not found\n", "utf8");
    response.writeHead(404, {
      "content-type": "text/plain; charset=utf-8",
      "content-length": body.byteLength.toString(),
      connection: "close",
    });
    response.end(body);
  });
  await listen(server);
  const address = server.address();
  assert.ok(address !== null && typeof address === "object");
  return {
    url: `http://127.0.0.1:${(address as AddressInfo).port}/`,
    close: () => closeServer(server),
  };
}

function listen(server: Server): Promise<void> {
  return new Promise((resolve, reject) => {
    const onError = (error: Error): void => reject(error);
    server.once("error", onError);
    server.listen(0, "127.0.0.1", () => {
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

function assertProcessIsGone(pid: number): void {
  try {
    process.kill(pid, 0);
  } catch (error) {
    assert.ok(error instanceof Error);
    assert.equal((error as NodeJS.ErrnoException).code, "ESRCH");
    return;
  }
  assert.fail(`Stasis process ${pid} still exists after Runtime.close()`);
}
