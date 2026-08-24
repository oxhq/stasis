#!/usr/bin/env node

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { createReadStream } from "node:fs";
import {
  access,
  lstat,
  mkdtemp,
  readdir,
  readFile,
  realpath,
  rm,
  unlink,
  writeFile,
} from "node:fs/promises";
import { constants as fsConstants } from "node:fs";
import { createServer } from "node:http";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";
import { performance } from "node:perf_hooks";

const REQUIRED_METHODS = [
  "session.open",
  "session.close",
  "runtime.pending",
  "runtime.settle",
  "action.fill",
  "action.activate",
  "dom.query",
  "dom.text",
  "dom.extract",
];
const CONTROLLED_WEBAPP_V1_PROFILE = "controlled-webapp-v1";
const INITIAL_VIRTUAL_TIME_NS = 1_000_000_000n;
const TEN_SECONDS_NS = 10_000_000_000n;
const commandDeadline = () => ({ signal: AbortSignal.timeout(30_000) });

function parseStrictJson(source, label) {
  let cursor = 0;
  const fail = (message) => {
    throw new SyntaxError(`${label} is invalid JSON at offset ${cursor}: ${message}`);
  };
  const skipWhitespace = () => {
    while (cursor < source.length && /[\t\n\r ]/.test(source[cursor])) cursor += 1;
  };
  const scanString = () => {
    if (source[cursor] !== '"') fail("expected string");
    const start = cursor;
    cursor += 1;
    while (cursor < source.length) {
      const character = source[cursor];
      if (character === '"') {
        cursor += 1;
        try {
          return JSON.parse(source.slice(start, cursor));
        } catch (error) {
          fail(error.message);
        }
      }
      if (character.charCodeAt(0) <= 0x1f) fail("unescaped control character");
      if (character === "\\") {
        cursor += 1;
        if (cursor >= source.length) fail("unterminated escape");
      }
      cursor += 1;
    }
    fail("unterminated string");
  };
  const scanValue = () => {
    skipWhitespace();
    const character = source[cursor];
    if (character === "{") {
      cursor += 1;
      skipWhitespace();
      const keys = new Set();
      if (source[cursor] === "}") {
        cursor += 1;
        return;
      }
      while (true) {
        skipWhitespace();
        const key = scanString();
        if (keys.has(key)) fail(`duplicate object key ${JSON.stringify(key)}`);
        keys.add(key);
        skipWhitespace();
        if (source[cursor] !== ":") fail("expected colon");
        cursor += 1;
        scanValue();
        skipWhitespace();
        if (source[cursor] === "}") {
          cursor += 1;
          return;
        }
        if (source[cursor] !== ",") fail("expected comma or closing brace");
        cursor += 1;
      }
    }
    if (character === "[") {
      cursor += 1;
      skipWhitespace();
      if (source[cursor] === "]") {
        cursor += 1;
        return;
      }
      while (true) {
        scanValue();
        skipWhitespace();
        if (source[cursor] === "]") {
          cursor += 1;
          return;
        }
        if (source[cursor] !== ",") fail("expected comma or closing bracket");
        cursor += 1;
      }
    }
    if (character === '"') {
      scanString();
      return;
    }
    for (const literal of ["true", "false", "null"]) {
      if (source.startsWith(literal, cursor)) {
        cursor += literal.length;
        return;
      }
    }
    const number = source.slice(cursor).match(/^-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?/u)?.[0];
    if (number === undefined) fail("expected value");
    cursor += number.length;
    if (!Number.isFinite(Number(number))) fail(`non-finite number ${number}`);
  };

  if (typeof source !== "string") throw new TypeError(`${label} must be text`);
  scanValue();
  skipWhitespace();
  if (cursor !== source.length) fail("unexpected trailing input");
  try {
    return JSON.parse(source);
  } catch (error) {
    throw new SyntaxError(`${label} is invalid JSON: ${error.message}`, { cause: error });
  }
}

const { values } = parseArgs({
  options: {
    binary: { type: "string" },
    fixture: { type: "string" },
    "consumer-root": { type: "string" },
    package: { type: "string" },
    revision: { type: "string" },
    version: { type: "string" },
  },
  strict: true,
});

for (const field of ["binary", "fixture", "consumer-root", "package", "revision", "version"]) {
  if (typeof values[field] !== "string" || values[field].length === 0) {
    throw new TypeError(`--${field} is required`);
  }
}

const binary = resolve(values.binary);
const fixture = resolve(values.fixture);
const consumerRoot = resolve(values["consumer-root"]);
const packageTarball = resolve(values.package);
assert.ok(isAbsolute(fixture), "--fixture must resolve to an absolute path before launch");
const packageRoot = join(consumerRoot, "node_modules", "@oxhq", "stasis");
const expectedRevision = values.revision.toLowerCase();
const expectedVersion = values.version;
assert.match(expectedRevision, /^[0-9a-f]{40}$/, "--revision must be a full Git commit");
assert.equal(expectedVersion, "0.1.0", "--version must name the exact stable release");
const expectedTarballName = `oxhq-stasis-${expectedVersion}.tgz`;
const packageStatus = await lstat(packageTarball);
assert.ok(packageStatus.isFile() && !packageStatus.isSymbolicLink(), "--package must be a regular file");
assert.equal(basename(packageTarball), expectedTarballName, "--package has an unexpected name");
const expectedSource = {
  servo_repository: "https://github.com/servo/servo.git",
  servo_revision: "0d579bd5aab6df3764fad805427254751632a6e4",
  pliego_repository: "https://github.com/oxhq/pliego.git",
  pliego_revision: "556c774242b272b11bc60999449c5debff1ad20f",
  pliego_servo_merge_base: "313b6d5ecc113b08010ce434140db3ca5abcc71c",
  stasis_repository: "https://github.com/oxhq/stasis.git",
  stasis_revision: expectedRevision,
};
await access(binary, fsConstants.X_OK);
const binarySha256 = await (async () => {
  const digest = createHash("sha256");
  for await (const chunk of createReadStream(binary)) {
    digest.update(chunk);
  }
  return digest.digest("hex");
})();
const tarball = await (async () => {
  const sha256 = createHash("sha256");
  const sha512 = createHash("sha512");
  for await (const chunk of createReadStream(packageTarball)) {
    sha256.update(chunk);
    sha512.update(chunk);
  }
  return {
    name: expectedTarballName,
    sha256: sha256.digest("hex"),
    integrity: `sha512-${sha512.digest("base64")}`,
  };
})();

const packageMetadata = parseStrictJson(
  await readFile(join(packageRoot, "package.json"), "utf8"),
  "installed SDK package metadata",
);
assert.equal(packageMetadata.name, "@oxhq/stasis");
assert.equal(packageMetadata.version, expectedVersion);
assert.equal(packageMetadata.repository?.url, "https://github.com/oxhq/stasis.git");
assert.equal(packageMetadata.type, "module");
assert.equal(packageMetadata.main, "./dist/index.js");
assert.equal(packageMetadata.types, "./dist/index.d.ts");
assert.deepEqual(packageMetadata.exports, {
  ".": {
    types: "./dist/index.d.ts",
    import: "./dist/index.js",
  },
});

// Resolve the real public package specifier from a consumer module. Importing dist/index.js
// directly would let a broken or missing package export map pass the release gate.
const importProbe = join(consumerRoot, `.stasis-release-import-${process.pid}.mjs`);
await writeFile(importProbe, 'export { launch } from "@oxhq/stasis";\n', {
  encoding: "utf8",
  flag: "wx",
});
let sdk;
try {
  sdk = await import(`${pathToFileURL(importProbe).href}?release-gate=${Date.now()}`);
} finally {
  await unlink(importProbe);
}
assert.equal(typeof sdk.launch, "function", "registry SDK does not export launch()");
const fixtureBody = await readFile(fixture);
const invocationRoot = await realpath(process.cwd());
const fixtureRealPath = await realpath(fixture);
const checkoutRelativeFixture = relative(invocationRoot, fixtureRealPath);
const fixtureIsInsideInvocationRoot =
  checkoutRelativeFixture !== "" &&
  checkoutRelativeFixture !== ".." &&
  !checkoutRelativeFixture.startsWith(`..${sep}`) &&
  !isAbsolute(checkoutRelativeFixture);
const server = createServer((request, response) => {
  if (request.url === "/" || request.url === "/index.html") {
    response.writeHead(200, {
      "cache-control": "no-store",
      "content-length": fixtureBody.length,
      "content-type": "text/html; charset=utf-8",
    });
    response.end(fixtureBody);
    return;
  }
  response.writeHead(404, { "content-length": "0" });
  response.end();
});

await new Promise((resolveListen, rejectListen) => {
  server.once("error", rejectListen);
  server.listen(0, "127.0.0.1", resolveListen);
});
const address = server.address();
assert.ok(address && typeof address === "object");
const fixtureUrl = `http://127.0.0.1:${address.port}/`;

let runtime;
let app;
let closedCleanly = false;
let runtimeWorkingDirectory;
try {
  runtimeWorkingDirectory = await mkdtemp(join(consumerRoot, ".stasis-runtime-cwd-"));
  const [consumerRootRealPath, runtimeWorkingDirectoryRealPath] = await Promise.all([
    realpath(consumerRoot),
    realpath(runtimeWorkingDirectory),
  ]);
  assert.equal(
    dirname(runtimeWorkingDirectoryRealPath),
    consumerRootRealPath,
    "runtime cwd escaped the clean consumer root",
  );
  assert.deepEqual(
    await readdir(runtimeWorkingDirectoryRealPath),
    [],
    "runtime cwd must start empty",
  );

  // The release workflow supplies a checkout-relative fixture. Prove that the same relative
  // resource spelling cannot resolve from the native child's empty clean-consumer cwd; the
  // verifier serves only the fixture bytes read through the absolute path above.
  if (fixtureIsInsideInvocationRoot) {
    const checkoutRelativeProbe = resolve(
      runtimeWorkingDirectoryRealPath,
      checkoutRelativeFixture,
    );
    assert.notEqual(
      checkoutRelativeProbe,
      fixtureRealPath,
      "isolated runtime cwd still resolves the checkout fixture",
    );
    await assert.rejects(
      access(checkoutRelativeProbe, fsConstants.F_OK),
      (error) => error?.code === "ENOENT",
      "checkout-relative fixture unexpectedly exists beneath the clean runtime cwd",
    );
  }

  runtime = await sdk.launch({
    executablePath: binary,
    cwd: runtimeWorkingDirectoryRealPath,
    closeTimeoutMs: 30_000,
    ...commandDeadline(),
  });
  const childPid = runtime.pid;
  assert.ok(Number.isSafeInteger(childPid) && childPid > 0, "SDK did not expose the child PID");
  assert.equal(runtime.info.protocolVersion, 1);
  assert.equal(runtime.info.implementation.name, "stasis-shell");
  assert.equal(runtime.info.implementation.version, expectedVersion);
  assert.deepEqual(runtime.info.implementation.source, expectedSource);
  assert.equal(runtime.info.capabilities.settlement, true);
  assert.ok(runtime.info.capabilities.clockModes.includes("controlled"));
  assert.ok(runtime.info.capabilities.profiles.includes(CONTROLLED_WEBAPP_V1_PROFILE));
  for (const method of REQUIRED_METHODS) {
    assert.ok(runtime.info.capabilities.methods.includes(method), `runtime did not advertise ${method}`);
  }

  app = await runtime.open(fixtureUrl, {
    clock: {
      mode: "controlled",
      initialVirtualTimeNs: INITIAL_VIRTUAL_TIME_NS,
      unixTimeOriginNs: 0n,
    },
    ...commandDeadline(),
  });
  assert.equal(app.clockMode, "controlled");
  assert.equal(app.boundary, "controlled_ready");
  assert.equal(app.profile, CONTROLLED_WEBAPP_V1_PROFILE);

  const policy = {
    persistentWork: "report",
    maxVirtualTimeNs: 30_000_000_000n,
    maxControlTurns: 100_000n,
    wallIoTimeoutNs: 30_000_000_000n,
  };
  const initial = await app.settle(policy, commandDeadline());
  assert.equal(initial.outcome, "quiescent");
  assert.equal(initial.virtualTimeNs, INITIAL_VIRTUAL_TIME_NS);
  assert.equal(await app.text("#result", initial.stateGeneration, commandDeadline()), "idle");

  await app.activate("#start", initial.stateGeneration, commandDeadline());
  const acted = await app.pending(commandDeadline());
  assert.ok(acted.stateGeneration > initial.stateGeneration);
  assert.equal(acted.virtualTimeNs, initial.virtualTimeNs, "action advanced virtual time");
  assert.equal(acted.timers.futureFinite, 1n);

  const wallStarted = performance.now();
  const settled = await app.settle(policy, commandDeadline());
  const wallElapsedMs = performance.now() - wallStarted;
  assert.ok(Number.isFinite(wallElapsedMs), "SDK gate wall duration is not finite");
  assert.equal(settled.outcome, "quiescent");
  assert.ok(wallElapsedMs < 8_000, `ten-second virtual timer took ${wallElapsedMs}ms of wall time`);
  assert.equal(settled.virtualTimeNs - initial.virtualTimeNs, TEN_SECONDS_NS);
  assert.equal(
    await app.text("#result", settled.stateGeneration, commandDeadline()),
    "timer complete",
  );
  assert.equal(
    await app.text("#date-elapsed", settled.stateGeneration, commandDeadline()),
    "10000",
  );
  assert.equal(
    await app.text("#performance-elapsed", settled.stateGeneration, commandDeadline()),
    "10000",
  );

  await app.close(commandDeadline());
  await app.close(commandDeadline());
  closedCleanly = true;

  // App.close() waits for the close response, protocol-only stdout EOF, and a clean child close.
  // This extra process check makes accidental early resolution visible in the registry proof.
  await new Promise((resolveImmediate) => setImmediate(resolveImmediate));
  let processStillExists = true;
  try {
    process.kill(childPid, 0);
  } catch (error) {
    if (error?.code !== "ESRCH") throw error;
    processStillExists = false;
  }
  assert.equal(processStillExists, false, "Stasis child still exists after graceful close and EOF");

  process.stdout.write(
    `${JSON.stringify({
      gate: "sdk-act-settle-inspect",
      package: `@oxhq/stasis@${expectedVersion}`,
      revision: expectedRevision,
      source: expectedSource,
      tarball,
      binary,
      binarySha256,
      virtualElapsedNs: TEN_SECONDS_NS.toString(),
      wallElapsedMs,
      closeResponseAndEof: true,
    })}\n`,
  );
} finally {
  if (!closedCleanly && runtime !== undefined) {
    await runtime.close().catch(() => undefined);
  }
  await new Promise((resolveClose) => {
    server.close(resolveClose);
    server.closeAllConnections?.();
  });
  if (runtimeWorkingDirectory !== undefined) {
    await rm(runtimeWorkingDirectory, { recursive: true });
  }
}
