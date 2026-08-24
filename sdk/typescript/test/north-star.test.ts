import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import test from "node:test";

import {
  CONTROLLED_WEBAPP_V1_PROFILE,
  launch,
  settlementEvidence,
  type OpenOptions,
} from "../src/index.js";

const NATIVE_BINARY = process.env.STASIS_NORTH_STAR_BINARY;
const FIXTURE = await readFile(new URL("./fixtures/north-star.html", import.meta.url));
const RUNS = 3;
const INITIAL_VIRTUAL_TIME_NS = 1_000_000_000n;
const SYNTHETIC_EMAIL = "north-star-user@example.invalid";
const SYNTHETIC_PASSWORD = "synthetic-password-never-log";
const SETTLE_POLICY = {
  persistentWork: "report" as const,
  maxVirtualTimeNs: 5_000_000_000n,
  maxControlTurns: 100_000n,
  wallIoTimeoutNs: 10_000_000_000n,
};

interface LoginRequest {
  method: string;
  path: string;
  contentType: string;
  email: string;
  password: string;
}

interface NorthStarServer {
  url: string;
  logins: LoginRequest[];
  errors: unknown[];
  close(): Promise<void>;
}

interface NorthStarFingerprint {
  outcome: string;
  virtualTimeDeltaNs: string;
  domEpoch: string;
  trace: string;
  cardCount: string;
  rows: unknown;
  evidenceShape: {
    schemaVersion: number;
    completeness: string;
    profile: string;
    outcome: string;
    reasonKind: string;
    maxItems: number;
  };
}

test(
  "public API completes the v0.1 North Star deterministically without sleeps",
  {
    skip:
      NATIVE_BINARY === undefined
        ? "set STASIS_NORTH_STAR_BINARY to the stasis executable for the native proof"
        : false,
    timeout: 120_000,
  },
  async () => {
    assert.ok(NATIVE_BINARY, "STASIS_NORTH_STAR_BINARY must be non-empty");
    const server = await startNorthStarServer();
    const fingerprints: NorthStarFingerprint[] = [];

    try {
      for (let run = 0; run < RUNS; run += 1) {
        fingerprints.push(await executeNorthStar(server.url, NATIVE_BINARY));
      }

      if (server.errors.length > 0) throw server.errors[0];
      assert.equal(server.logins.length, RUNS, "each independent runtime must perform one login");
      for (const login of server.logins) {
        assert.deepEqual(login, {
          method: "POST",
          path: "/login",
          contentType: "application/json",
          email: SYNTHETIC_EMAIL,
          password: SYNTHETIC_PASSWORD,
        });
      }

      const semanticFingerprints = fingerprints.map(({ domEpoch: _domEpoch, ...fingerprint }) =>
        fingerprint
      );
      assert.deepEqual(
        semanticFingerprints.slice(1),
        Array.from({ length: RUNS - 1 }, () => semanticFingerprints[0]),
        "independent runs did not produce the same controlled semantic result and evidence",
      );
    } finally {
      await server.close();
    }
  },
);

async function executeNorthStar(
  url: string,
  executablePath: string,
): Promise<NorthStarFingerprint> {
  const runtime = await launch({ executablePath });
  let closed = false;

  try {
    for (const method of [
      "action.fill",
      "action.activate",
      "dom.query",
      "dom.text",
      "dom.extract",
      "runtime.settle",
    ]) {
      assert.ok(runtime.info.capabilities.methods.includes(method), `runtime omitted ${method}`);
    }
    assert.ok(
      runtime.info.capabilities.profiles.includes(CONTROLLED_WEBAPP_V1_PROFILE),
      `runtime omitted ${CONTROLLED_WEBAPP_V1_PROFILE}`,
    );

    // The intersection keeps this fixture source-compatible while profile negotiation lands in
    // OpenOptions; at runtime the named profile is always sent and advertised above.
    const openOptions = {
      profile: CONTROLLED_WEBAPP_V1_PROFILE,
      clock: {
        mode: "controlled" as const,
        initialVirtualTimeNs: INITIAL_VIRTUAL_TIME_NS,
        unixTimeOriginNs: 0n as const,
      },
    } as OpenOptions & { profile: typeof CONTROLLED_WEBAPP_V1_PROFILE };
    const app = await runtime.open(url, openOptions);

    const initial = await app.settle(SETTLE_POLICY);
    assert.equal(initial.outcome, "quiescent");
    assert.equal(initial.virtualTimeNs, INITIAL_VIRTUAL_TIME_NS);

    let generation = initial.stateGeneration;
    const emailFill = await app.fill("#email", SYNTHETIC_EMAIL, generation);
    assert.ok(emailFill.stateGeneration > generation);
    generation = emailFill.stateGeneration;

    const passwordFill = await app.fill("#password", SYNTHETIC_PASSWORD, generation);
    assert.ok(passwordFill.stateGeneration > generation);
    generation = passwordFill.stateGeneration;
    assert.equal(await app.text("#input-events", generation), "email=1,password=1");

    const activation = await app.activate("#submit", generation);
    assert.ok(activation.stateGeneration > generation);

    const settled = await app.settle(SETTLE_POLICY);
    assert.equal(settled.outcome, "quiescent");
    assert.ok(
      settled.virtualTimeNs - initial.virtualTimeNs >= 250_000_000n,
      "settlement did not advance through the fixture timer",
    );

    generation = settled.stateGeneration;
    assert.equal(await app.text("#status", generation), "ready");
    const trace = await app.text("#trace", generation);
    assert.equal(trace, "boot,submit,fetch,promise,microtask,dom,timer,raf");

    const query = await app.query(".dashboard-card", generation);
    assert.equal(query.stateGeneration, generation);
    assert.equal(query.count, 3n);

    const extracted = await app.extract(
      {
        rootSelector: ".dashboard-card",
        fields: [
          { name: "title", selector: ".card-title", read: "text" },
          { name: "body", selector: ".card-body", read: "html" },
        ],
      },
      generation,
    );
    assert.equal(extracted.stateGeneration, generation);
    assert.deepEqual(extracted.rows, [
      {
        fields: [
          { name: "title", value: "Account" },
          {
            name: "body",
            value: `<strong id="account-email">${SYNTHETIC_EMAIL}</strong>`,
          },
        ],
      },
      {
        fields: [
          { name: "title", value: "Role" },
          { name: "body", value: '<strong id="account-role">Test Operator</strong>' },
        ],
      },
      {
        fields: [
          { name: "title", value: "Settlement" },
          { name: "body", value: '<strong id="settlement-state">Ready</strong>' },
        ],
      },
    ]);

    const evidence = settlementEvidence(settled);
    assert.equal(evidence.schemaVersion, 1);
    assert.equal(evidence.completeness, "terminal_snapshot");
    assert.equal(evidence.profile, CONTROLLED_WEBAPP_V1_PROFILE);
    assert.equal(evidence.outcome, "quiescent");
    assert.equal(evidence.reason.kind, "quiescent");
    assert.equal(evidence.bounds.maxItems, 32);
    assert.equal(evidence.virtualTimeNs, settled.virtualTimeNs);
    assert.equal(evidence.stateGeneration, settled.stateGeneration);
    assert.equal(evidence.domEpoch, settled.domEpoch);
    const evidenceJson = stringifyBigInts(evidence);
    assertRedacted(evidenceJson);

    await app.close();
    closed = true;

    return {
      outcome: settled.outcome,
      virtualTimeDeltaNs: (settled.virtualTimeNs - initial.virtualTimeNs).toString(),
      domEpoch: settled.domEpoch.toString(),
      trace,
      cardCount: query.count.toString(),
      rows: extracted.rows,
      evidenceShape: {
        schemaVersion: evidence.schemaVersion,
        completeness: evidence.completeness,
        profile: evidence.profile,
        outcome: evidence.outcome,
        reasonKind: evidence.reason.kind,
        maxItems: evidence.bounds.maxItems,
      },
    };
  } finally {
    if (!closed) await runtime.close().catch(() => undefined);
  }
}

async function startNorthStarServer(): Promise<NorthStarServer> {
  const logins: LoginRequest[] = [];
  const errors: unknown[] = [];
  const server = createServer((request, response) => {
    void serveNorthStarRequest(request, response, logins).catch((error: unknown) => {
      errors.push(error);
      if (!response.headersSent) {
        const body = Buffer.from("fixture server error\n", "utf8");
        response.writeHead(500, {
          "content-type": "text/plain; charset=utf-8",
          "content-length": body.byteLength.toString(),
          connection: "close",
        });
        response.end(body);
        return;
      }
      response.destroy(error instanceof Error ? error : undefined);
    });
  });
  await listen(server);
  const address = server.address() as AddressInfo;

  return {
    url: `http://127.0.0.1:${address.port}/`,
    logins,
    errors,
    close: () => closeServer(server),
  };
}

async function serveNorthStarRequest(
  request: IncomingMessage,
  response: ServerResponse,
  logins: LoginRequest[],
): Promise<void> {
  const path = new URL(request.url ?? "/", "http://127.0.0.1").pathname;
  if (request.method === "GET" && path === "/") {
    writeResponse(response, 200, "text/html; charset=utf-8", FIXTURE);
    return;
  }
  if (request.method === "POST" && path === "/login") {
    const contentType = request.headers["content-type"] ?? "";
    const body = await readBoundedBody(request, 16 * 1024);
    const decoded = JSON.parse(body.toString("utf8")) as unknown;
    assertRecord(decoded, "login body");
    const email = decoded.email;
    const password = decoded.password;
    assert.ok(typeof email === "string", "login email must be a string");
    assert.ok(typeof password === "string", "login password must be a string");
    logins.push({
      method: request.method,
      path,
      contentType,
      email,
      password,
    });
    const responseBody = Buffer.from(
      JSON.stringify({ email, role: "Test Operator" }),
      "utf8",
    );
    writeResponse(response, 200, "application/json", responseBody);
    return;
  }
  writeResponse(response, 404, "text/plain; charset=utf-8", Buffer.from("not found\n"));
}

async function readBoundedBody(request: IncomingMessage, maxBytes: number): Promise<Buffer> {
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

function writeResponse(
  response: ServerResponse,
  status: number,
  contentType: string,
  body: Uint8Array,
): void {
  response.writeHead(status, {
    "content-type": contentType,
    "content-length": body.byteLength.toString(),
    connection: "close",
    "cache-control": "no-store",
  });
  response.end(body);
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

function assertRecord(value: unknown, label: string): asserts value is Record<string, string> {
  assert.ok(typeof value === "object" && value !== null && !Array.isArray(value), `${label} invalid`);
}

function stringifyBigInts(value: unknown): string {
  return JSON.stringify(value, (_key, item: unknown) =>
    typeof item === "bigint" ? item.toString() : item,
  );
}

function assertRedacted(evidenceJson: string): void {
  for (const sensitive of [
    SYNTHETIC_EMAIL,
    SYNTHETIC_PASSWORD,
    "/login",
    "#email",
    "#password",
    "authorization",
  ]) {
    assert.equal(
      evidenceJson.includes(sensitive),
      false,
      `terminal evidence leaked sensitive fixture data: ${sensitive}`,
    );
  }
}
