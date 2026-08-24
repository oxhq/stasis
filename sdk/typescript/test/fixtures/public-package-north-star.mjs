#!/usr/bin/env node

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { createServer } from "node:http";

import {
  CONTROLLED_WEBAPP_V1_PROFILE,
  launch,
  settlementEvidence,
} from "@oxhq/stasis";

const FIXTURE = await readFile(new URL("./north-star.html", import.meta.url));
const RUNS = 3;
const INITIAL_VIRTUAL_TIME_NS = 1_000_000_000n;
const SYNTHETIC_EMAIL = "north-star-user@example.invalid";
const SYNTHETIC_PASSWORD = "synthetic-password-never-log";
const SETTLE_POLICY = {
  persistentWork: "report",
  maxVirtualTimeNs: 5_000_000_000n,
  maxControlTurns: 100_000n,
  wallIoTimeoutNs: 10_000_000_000n,
};

const fixtureServer = await startFixtureServer();
const fingerprints = [];

try {
  for (let run = 0; run < RUNS; run += 1) {
    fingerprints.push(await executeNorthStar(fixtureServer.url));
  }
  if (fixtureServer.errors.length > 0) throw fixtureServer.errors[0];
  assert.equal(fixtureServer.logins.length, RUNS);
  for (const login of fixtureServer.logins) {
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
    "independent public-package runs produced different controlled semantic results",
  );

  process.stdout.write(
    `${JSON.stringify({
      proof: "stasis-v0.1-north-star",
      runs: RUNS,
      profile: CONTROLLED_WEBAPP_V1_PROFILE,
      outcome: fingerprints[0].outcome,
      virtualTimeDeltaNs: fingerprints[0].virtualTimeDeltaNs,
      domEpoch: fingerprints[0].domEpoch,
      cardCount: fingerprints[0].cardCount,
      evidenceShape: fingerprints[0].evidenceShape,
    })}\n`,
  );
} finally {
  await fixtureServer.close();
}

async function executeNorthStar(url) {
  const launchOptions = process.env.STASIS_NORTH_STAR_BINARY
    ? { executablePath: process.env.STASIS_NORTH_STAR_BINARY }
    : {};
  const runtime = await launch(launchOptions);
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

    const app = await runtime.open(url, {
      profile: CONTROLLED_WEBAPP_V1_PROFILE,
      clock: {
        mode: "controlled",
        initialVirtualTimeNs: INITIAL_VIRTUAL_TIME_NS,
        unixTimeOriginNs: 0n,
      },
    });
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
    assert.ok(settled.virtualTimeNs - initial.virtualTimeNs >= 250_000_000n);
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
    assert.deepEqual(extracted.rows, expectedRows());

    const evidence = settlementEvidence(settled);
    assert.equal(evidence.schemaVersion, 1);
    assert.equal(evidence.completeness, "terminal_snapshot");
    assert.equal(evidence.profile, CONTROLLED_WEBAPP_V1_PROFILE);
    assert.equal(evidence.reason.kind, "quiescent");
    assert.equal(evidence.bounds.maxItems, 32);
    assert.equal(evidence.virtualTimeNs, settled.virtualTimeNs);
    assert.equal(evidence.stateGeneration, settled.stateGeneration);
    assert.equal(evidence.domEpoch, settled.domEpoch);
    const evidenceJson = stringifyBigInts(evidence);
    assertEvidenceIsRedacted(evidenceJson);

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

function expectedRows() {
  return [
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
  ];
}

async function startFixtureServer() {
  const logins = [];
  const errors = [];
  const server = createServer((request, response) => {
    void serveRequest(request, response, logins).catch((error) => {
      errors.push(error);
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
  return {
    url: `http://127.0.0.1:${address.port}/`,
    logins,
    errors,
    close: () => new Promise((resolve, reject) => {
      server.close((error) => (error === undefined ? resolve() : reject(error)));
    }),
  };
}

async function serveRequest(request, response, logins) {
  const path = new URL(request.url ?? "/", "http://127.0.0.1").pathname;
  if (request.method === "GET" && path === "/") {
    writeResponse(response, 200, "text/html; charset=utf-8", FIXTURE);
    return;
  }
  if (request.method === "POST" && path === "/login") {
    const body = await readBoundedBody(request, 16 * 1024);
    const decoded = JSON.parse(body.toString("utf8"));
    assert.ok(decoded && typeof decoded === "object" && !Array.isArray(decoded));
    assert.equal(typeof decoded.email, "string");
    assert.equal(typeof decoded.password, "string");
    logins.push({
      method: request.method,
      path,
      contentType: request.headers["content-type"] ?? "",
      email: decoded.email,
      password: decoded.password,
    });
    writeResponse(
      response,
      200,
      "application/json",
      Buffer.from(JSON.stringify({ email: decoded.email, role: "Test Operator" })),
    );
    return;
  }
  writeResponse(response, 404, "text/plain; charset=utf-8", Buffer.from("not found\n"));
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

function writeResponse(response, status, contentType, body) {
  response.writeHead(status, {
    "content-type": contentType,
    "content-length": body.byteLength.toString(),
    connection: "close",
    "cache-control": "no-store",
  });
  response.end(body);
}

function stringifyBigInts(value) {
  return JSON.stringify(value, (_key, item) =>
    typeof item === "bigint" ? item.toString() : item,
  );
}

function assertEvidenceIsRedacted(evidenceJson) {
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
