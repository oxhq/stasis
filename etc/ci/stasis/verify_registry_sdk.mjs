#!/usr/bin/env node

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { lookup } from "node:dns/promises";
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
import { createServer, get as httpGet } from "node:http";
import { isIP } from "node:net";
import { basename, dirname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { pathToFileURL } from "node:url";
import { isDeepStrictEqual, parseArgs } from "node:util";
import { performance } from "node:perf_hooks";

const REQUIRED_METHODS = [
  "session.open",
  "session.close",
  "session.navigate",
  "runtime.pending",
  "runtime.settle",
  "runtime.advance_to_next",
  "action.fill",
  "action.activate",
  "action.check",
  "action.select",
  "action.submit",
  "dom.query",
  "dom.text",
  "dom.extract",
];
const REQUIRED_REGISTRY_SDK_EXPORTS = Object.freeze([
  "CONTROLLED_WEB_SESSION_V2_PROFILE",
  "StasisProtocolError",
  "launch",
  "settlementEvidence",
]);
const CONTROLLED_WEBAPP_V1_PROFILE = "controlled-webapp-v1";
const CONTROLLED_WEB_SESSION_V2_PROFILE = "controlled-web-session-v2";
const INITIAL_VIRTUAL_TIME_NS = 1_000_000_000n;
const TEN_SECONDS_NS = 10_000_000_000n;
const MAX_U64_VIRTUAL_TIME_NS = (1n << 64n) - 1n;
const commandDeadline = () => ({ signal: AbortSignal.timeout(30_000) });

// The release subject receives only runtime environment needed by the packaged GUI/runtime.
// In particular, workflow credentials, package-manager authentication, cloud credentials,
// agent sockets, preload hooks, and unrelated STASIS_* controls are not inherited by the exact
// binary under test. XAUTHORITY is deliberately retained because xvfb-run may require it.
const EXACT_BINARY_INHERITED_ENVIRONMENT = Object.freeze([
  "APPDATA",
  "COMSPEC",
  "DISPLAY",
  "DYLD_FALLBACK_LIBRARY_PATH",
  "DYLD_LIBRARY_PATH",
  "FONTCONFIG_FILE",
  "FONTCONFIG_PATH",
  "GDK_PIXBUF_MODULE_FILE",
  "GST_PLUGIN_PATH",
  "GST_PLUGIN_SYSTEM_PATH",
  "GST_REGISTRY",
  "HOME",
  "LANG",
  "LD_LIBRARY_PATH",
  "LOCALAPPDATA",
  "LOGNAME",
  "PATH",
  "PATHEXT",
  "RUST_BACKTRACE",
  "SYSTEMROOT",
  "TEMP",
  "TMP",
  "TMPDIR",
  "USER",
  "USERPROFILE",
  "WAYLAND_DISPLAY",
  "WINDIR",
  "XAUTHORITY",
  "XDG_RUNTIME_DIR",
]);
const EXACT_BINARY_OVERRIDE_ENVIRONMENT = Object.freeze([
  "STASIS_EXPLICIT_OVERRIDE_BINARY",
  "STASIS_EXPLICIT_OVERRIDE_MARKER",
  "STASIS_EXPLICIT_OVERRIDE_PROOF",
]);
const EXACT_BINARY_LOCALE_ENVIRONMENT = Object.freeze([
  "LC_ADDRESS",
  "LC_ALL",
  "LC_COLLATE",
  "LC_CTYPE",
  "LC_IDENTIFICATION",
  "LC_MEASUREMENT",
  "LC_MESSAGES",
  "LC_MONETARY",
  "LC_NAME",
  "LC_NUMERIC",
  "LC_PAPER",
  "LC_TELEPHONE",
  "LC_TIME",
]);

function exactBinaryChildEnvironment(overrides) {
  const environment = {};
  for (const name of [
    ...EXACT_BINARY_INHERITED_ENVIRONMENT,
    ...EXACT_BINARY_LOCALE_ENVIRONMENT,
  ]) {
    const value = process.env[name];
    if (value !== undefined) environment[name] = value;
  }
  assert.deepEqual(
    Object.keys(overrides).sort(),
    [...EXACT_BINARY_OVERRIDE_ENVIRONMENT].sort(),
    "exact-binary environment overrides escaped the release-gate allowlist",
  );
  for (const [name, value] of Object.entries(overrides)) {
    assert.equal(typeof value, "string", `${name} must be a string`);
    environment[name] = value;
  }
  const allowedNames = new Set([
    ...EXACT_BINARY_INHERITED_ENVIRONMENT,
    ...EXACT_BINARY_LOCALE_ENVIRONMENT,
    ...EXACT_BINARY_OVERRIDE_ENVIRONMENT,
  ]);
  assert.deepEqual(
    Object.keys(environment).filter((name) => !allowedNames.has(name)),
    [],
    "exact-binary child environment contains a non-allowlisted name",
  );
  return environment;
}

function countMessagePortSources(snapshot) {
  return snapshot.sources.filter(
    (source) =>
      source.kind === "tracked_presence" &&
      source.state === "open_ended" &&
      source.openEnded.reason === "message_port",
  ).length;
}

function cookieStateWithoutLastAccessSequence(cookie) {
  assert.ok(cookie && typeof cookie === "object", "cookie state record must be an object");
  const { lastAccessSequence: _lastAccessSequence, ...stableState } = cookie;
  return stableState;
}

function cookieStatesWithoutLastAccessSequence(cookies) {
  assert.ok(Array.isArray(cookies), "cookie state must be an array");
  return cookies.map(cookieStateWithoutLastAccessSequence);
}

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

function writeServerText(response, status, contentType, body, headers = {}) {
  const bytes = Buffer.from(body, "utf8");
  response.writeHead(status, {
    "cache-control": "no-store",
    "content-length": bytes.length,
    "content-type": contentType,
    ...headers,
  });
  response.end(bytes);
}

async function listenLoopback(server, host, port = 0) {
  const addressFamily = isIP(host);
  assert.ok(
    host === "127.0.0.1" || host === "::1",
    "release proof server must use an exact local-only loopback literal",
  );
  assert.ok(addressFamily === 4 || addressFamily === 6, "loopback host must be an IP literal");
  await new Promise((resolveListen, rejectListen) => {
    server.once("error", rejectListen);
    const options = { host, port };
    if (addressFamily === 6) options.ipv6Only = true;
    server.listen(options, resolveListen);
  });
  const address = server.address();
  assert.ok(address && typeof address === "object");
  assert.equal(address.address, host, "server did not bind the requested loopback address");
  if (port !== 0) {
    assert.equal(address.port, port, "loopback server did not retain the requested port");
  }
  const urlHost = addressFamily === 6 ? `[${host}]` : host;
  return new URL(`http://${urlHost}:${address.port}`).origin;
}

async function readLocalHttpText(url) {
  return await new Promise((resolveRead, rejectRead) => {
    const request = httpGet(url, { headers: { connection: "close" } }, (response) => {
      if (response.statusCode !== 200) {
        response.resume();
        rejectRead(new Error(`localhost preflight returned HTTP ${response.statusCode ?? 0}`));
        return;
      }
      const chunks = [];
      let bytes = 0;
      response.on("data", (chunk) => {
        bytes += chunk.length;
        if (bytes > 1024) {
          request.destroy(new Error("localhost preflight response exceeded 1024 bytes"));
          return;
        }
        chunks.push(chunk);
      });
      response.once("error", rejectRead);
      response.once("end", () => resolveRead(Buffer.concat(chunks).toString("utf8")));
    });
    request.setTimeout(5_000, () => {
      request.destroy(new Error("localhost preflight timed out"));
    });
    request.once("error", rejectRead);
  });
}

async function closeServer(server) {
  if (!server.listening) return;
  await new Promise((resolveClose) => {
    server.close(resolveClose);
    server.closeAllConnections?.();
  });
}

const { values } = parseArgs({
  options: {
    binary: { type: "string" },
    fixture: { type: "string" },
    "consumer-root": { type: "string" },
    package: { type: "string" },
    revision: { type: "string" },
    "session-v2-fixture": { type: "string" },
    "session-v2-image-fixture": { type: "string" },
    "session-v2-http-image-fixture": { type: "string" },
    "session-v2-inline-svg-fixture": { type: "string" },
    "session-v2-inline-svg-shared-pending-fixture": { type: "string" },
    "session-v2-settlement-url-fixture": { type: "string" },
    "session-v2-interval-before-finite-fixture": { type: "string" },
    "session-v2-focus-fixture": { type: "string" },
    "session-v2-automation-event-fixture": { type: "string" },
    "session-v2-css-animation-event-fixture": { type: "string" },
    "session-v2-cookie-fixture": { type: "string" },
    version: { type: "string" },
  },
  strict: true,
});

for (const field of [
  "binary",
  "fixture",
  "consumer-root",
  "package",
  "revision",
  "session-v2-fixture",
  "session-v2-image-fixture",
  "session-v2-http-image-fixture",
  "session-v2-inline-svg-fixture",
  "session-v2-inline-svg-shared-pending-fixture",
  "session-v2-settlement-url-fixture",
  "session-v2-interval-before-finite-fixture",
  "session-v2-focus-fixture",
  "session-v2-automation-event-fixture",
  "session-v2-css-animation-event-fixture",
  "session-v2-cookie-fixture",
  "version",
]) {
  if (typeof values[field] !== "string" || values[field].length === 0) {
    throw new TypeError(`--${field} is required`);
  }
}

const binary = resolve(values.binary);
const fixture = resolve(values.fixture);
const sessionV2Fixture = resolve(values["session-v2-fixture"]);
const sessionV2ImageFixture = resolve(values["session-v2-image-fixture"]);
const sessionV2HttpImageFixture = resolve(values["session-v2-http-image-fixture"]);
const sessionV2InlineSvgFixture = resolve(values["session-v2-inline-svg-fixture"]);
const sessionV2InlineSvgSharedPendingFixture = resolve(
  values["session-v2-inline-svg-shared-pending-fixture"],
);
const sessionV2SettlementUrlFixture = resolve(values["session-v2-settlement-url-fixture"]);
const sessionV2IntervalBeforeFiniteFixture = resolve(
  values["session-v2-interval-before-finite-fixture"],
);
const sessionV2FocusFixture = resolve(values["session-v2-focus-fixture"]);
const sessionV2AutomationEventFixture = resolve(values["session-v2-automation-event-fixture"]);
const sessionV2CssAnimationEventFixture = resolve(
  values["session-v2-css-animation-event-fixture"],
);
const sessionV2CookieFixture = resolve(values["session-v2-cookie-fixture"]);
const consumerRoot = resolve(values["consumer-root"]);
const packageTarball = resolve(values.package);
assert.ok(isAbsolute(fixture), "--fixture must resolve to an absolute path before launch");
assert.ok(
  isAbsolute(sessionV2Fixture),
  "--session-v2-fixture must resolve to an absolute path before launch",
);
assert.ok(
  isAbsolute(sessionV2ImageFixture),
  "--session-v2-image-fixture must resolve to an absolute path before launch",
);
assert.ok(
  isAbsolute(sessionV2HttpImageFixture),
  "--session-v2-http-image-fixture must resolve to an absolute path before launch",
);
assert.ok(
  isAbsolute(sessionV2InlineSvgFixture),
  "--session-v2-inline-svg-fixture must resolve to an absolute path before launch",
);
assert.ok(
  isAbsolute(sessionV2InlineSvgSharedPendingFixture),
  "--session-v2-inline-svg-shared-pending-fixture must resolve to an absolute path before launch",
);
assert.ok(
  isAbsolute(sessionV2SettlementUrlFixture),
  "--session-v2-settlement-url-fixture must resolve to an absolute path before launch",
);
assert.ok(
  isAbsolute(sessionV2IntervalBeforeFiniteFixture),
  "--session-v2-interval-before-finite-fixture must resolve to an absolute path before launch",
);
assert.ok(
  isAbsolute(sessionV2FocusFixture),
  "--session-v2-focus-fixture must resolve to an absolute path before launch",
);
assert.ok(
  isAbsolute(sessionV2AutomationEventFixture),
  "--session-v2-automation-event-fixture must resolve to an absolute path before launch",
);
assert.ok(
  isAbsolute(sessionV2CssAnimationEventFixture),
  "--session-v2-css-animation-event-fixture must resolve to an absolute path before launch",
);
assert.ok(
  isAbsolute(sessionV2CookieFixture),
  "--session-v2-cookie-fixture must resolve to an absolute path before launch",
);
const packageRoot = join(consumerRoot, "node_modules", "@oxhq", "stasis");
const expectedRevision = values.revision.toLowerCase();
const expectedVersion = values.version;
assert.match(expectedRevision, /^[0-9a-f]{40}$/, "--revision must be a full Git commit");
assert.equal(expectedVersion, "0.3.3", "--version must name the exact release candidate");
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
await writeFile(
  importProbe,
  `export { ${REQUIRED_REGISTRY_SDK_EXPORTS.join(", ")} } from "@oxhq/stasis";\n`,
  {
    encoding: "utf8",
    flag: "wx",
  },
);
let sdk;
try {
  sdk = await import(`${pathToFileURL(importProbe).href}?release-gate=${Date.now()}`);
} finally {
  await unlink(importProbe);
}
assert.equal(typeof sdk.launch, "function", "registry SDK does not export launch()");
assert.equal(
  typeof sdk.settlementEvidence,
  "function",
  "registry SDK does not export settlementEvidence()",
);
assert.equal(
  sdk.CONTROLLED_WEB_SESSION_V2_PROFILE,
  CONTROLLED_WEB_SESSION_V2_PROFILE,
  "registry SDK does not export the exact candidate v2 profile identity",
);
const fixtureBody = await readFile(fixture);
const sessionV2FixtureStatus = await lstat(sessionV2Fixture);
assert.ok(
  sessionV2FixtureStatus.isFile() && !sessionV2FixtureStatus.isSymbolicLink(),
  "--session-v2-fixture must be a regular file",
);
const sessionV2FixtureBody = await readFile(sessionV2Fixture, "utf8");
const sessionV2ImageFixtureStatus = await lstat(sessionV2ImageFixture);
assert.ok(
  sessionV2ImageFixtureStatus.isFile() && !sessionV2ImageFixtureStatus.isSymbolicLink(),
  "--session-v2-image-fixture must be a regular file",
);
const sessionV2ImageFixtureBody = await readFile(sessionV2ImageFixture, "utf8");
const sessionV2HttpImageFixtureStatus = await lstat(sessionV2HttpImageFixture);
assert.ok(
  sessionV2HttpImageFixtureStatus.isFile() &&
    !sessionV2HttpImageFixtureStatus.isSymbolicLink(),
  "--session-v2-http-image-fixture must be a regular file",
);
const sessionV2HttpImageFixtureBody = await readFile(sessionV2HttpImageFixture, "utf8");
const sessionV2InlineSvgFixtureStatus = await lstat(sessionV2InlineSvgFixture);
assert.ok(
  sessionV2InlineSvgFixtureStatus.isFile() &&
    !sessionV2InlineSvgFixtureStatus.isSymbolicLink(),
  "--session-v2-inline-svg-fixture must be a regular file",
);
const sessionV2InlineSvgFixtureBody = await readFile(sessionV2InlineSvgFixture, "utf8");
const sessionV2InlineSvgSharedPendingFixtureStatus = await lstat(
  sessionV2InlineSvgSharedPendingFixture,
);
assert.ok(
  sessionV2InlineSvgSharedPendingFixtureStatus.isFile() &&
    !sessionV2InlineSvgSharedPendingFixtureStatus.isSymbolicLink(),
  "--session-v2-inline-svg-shared-pending-fixture must be a regular file",
);
const sessionV2InlineSvgSharedPendingFixtureBody = await readFile(
  sessionV2InlineSvgSharedPendingFixture,
  "utf8",
);
const sessionV2SettlementUrlFixtureStatus = await lstat(sessionV2SettlementUrlFixture);
assert.ok(
  sessionV2SettlementUrlFixtureStatus.isFile() &&
    !sessionV2SettlementUrlFixtureStatus.isSymbolicLink(),
  "--session-v2-settlement-url-fixture must be a regular file",
);
const sessionV2SettlementUrlFixtureBody = await readFile(
  sessionV2SettlementUrlFixture,
  "utf8",
);
const sessionV2IntervalBeforeFiniteFixtureStatus = await lstat(
  sessionV2IntervalBeforeFiniteFixture,
);
assert.ok(
  sessionV2IntervalBeforeFiniteFixtureStatus.isFile() &&
    !sessionV2IntervalBeforeFiniteFixtureStatus.isSymbolicLink(),
  "--session-v2-interval-before-finite-fixture must be a regular file",
);
const sessionV2IntervalBeforeFiniteFixtureBody = await readFile(
  sessionV2IntervalBeforeFiniteFixture,
  "utf8",
);
const sessionV2FocusFixtureStatus = await lstat(sessionV2FocusFixture);
assert.ok(
  sessionV2FocusFixtureStatus.isFile() && !sessionV2FocusFixtureStatus.isSymbolicLink(),
  "--session-v2-focus-fixture must be a regular file",
);
const sessionV2FocusFixtureBody = await readFile(sessionV2FocusFixture, "utf8");
const sessionV2AutomationEventFixtureStatus = await lstat(sessionV2AutomationEventFixture);
assert.ok(
  sessionV2AutomationEventFixtureStatus.isFile() &&
    !sessionV2AutomationEventFixtureStatus.isSymbolicLink(),
  "--session-v2-automation-event-fixture must be a regular file",
);
const sessionV2AutomationEventFixtureBody = await readFile(
  sessionV2AutomationEventFixture,
  "utf8",
);
const sessionV2CssAnimationEventFixtureStatus = await lstat(
  sessionV2CssAnimationEventFixture,
);
assert.ok(
  sessionV2CssAnimationEventFixtureStatus.isFile() &&
    !sessionV2CssAnimationEventFixtureStatus.isSymbolicLink(),
  "--session-v2-css-animation-event-fixture must be a regular file",
);
const sessionV2CssAnimationEventFixtureBody = await readFile(
  sessionV2CssAnimationEventFixture,
  "utf8",
);
const sessionV2CookieFixtureStatus = await lstat(sessionV2CookieFixture);
assert.ok(
  sessionV2CookieFixtureStatus.isFile() && !sessionV2CookieFixtureStatus.isSymbolicLink(),
  "--session-v2-cookie-fixture must be a regular file",
);
const sessionV2CookieFixtureBody = await readFile(sessionV2CookieFixture, "utf8");
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

const cookieMainRequests = [];
const cookieCrossRequests = [];
let cookieCrossUrl;
const cookieCrossPreflightPath = "/__stasis-localhost-preflight__";
const cookieCrossPreflightBody = "stasis-localhost-loopback\n";
const handleCookieCrossRequest = (request, response) => {
  if (request.method === "GET" && request.url === cookieCrossPreflightPath) {
    writeServerText(response, 200, "text/plain; charset=utf-8", cookieCrossPreflightBody);
    return;
  }
  cookieCrossRequests.push({
    method: request.method ?? "",
    url: request.url ?? "",
    host: request.headers.host ?? "",
    cookie: request.headers.cookie ?? "",
  });
  if (request.method === "GET" && request.url === "/probe.js") {
    writeServerText(
      response,
      200,
      "text/javascript; charset=utf-8",
      'globalThis.__stasisCrossSiteProbe = "loaded";\n',
    );
    return;
  }
  writeServerText(response, 404, "text/plain; charset=utf-8", "not found\n");
};
const cookieCrossIpv4Server = createServer(handleCookieCrossRequest);
const cookieCrossIpv6Server = createServer(handleCookieCrossRequest);
const cookieMainServer = createServer((request, response) => {
  cookieMainRequests.push({
    method: request.method ?? "",
    url: request.url ?? "",
    cookie: request.headers.cookie ?? "",
  });
  if (request.method === "GET" && request.url === "/login") {
    writeServerText(response, 200, "text/html; charset=utf-8", sessionV2CookieFixtureBody);
    return;
  }
  if (request.method === "POST" && request.url === "/login") {
    request.resume();
    writeServerText(
      response,
      200,
      "text/html; charset=utf-8",
      '<!doctype html><html><body><output id="authenticated">remembered</output></body></html>',
      {
        "set-cookie": [
          "remember_me=controlled; Path=/; HttpOnly; SameSite=Lax; " +
            "Expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=2592000",
        ],
      },
    );
    return;
  }
  if (request.method === "GET" && request.url === "/fresh-no-import") {
    writeServerText(
      response,
      200,
      "text/html; charset=utf-8",
      '<!doctype html><html><body><output id="fresh-no-import">fresh</output></body></html>',
    );
    return;
  }
  if (request.method === "GET" && request.url === "/time-range-arm") {
    writeServerText(
      response,
      200,
      "text/html; charset=utf-8",
      `<!doctype html><html><body>
        <output id="time-range-arm">pending</output>
        <button id="time-range-advance" type="button">advance</button>
        <script>
          document.querySelector("#time-range-advance").addEventListener("click", () => {
            setTimeout(() => {
              document.querySelector("#time-range-arm").textContent = "advanced";
            }, 1);
          });
        </script>
      </body></html>`,
    );
    return;
  }
  if (request.method === "GET" && request.url === "/restored") {
    writeServerText(
      response,
      200,
      "text/html; charset=utf-8",
      `<!doctype html><html><body>
        <script src="${cookieCrossUrl}/probe.js"></script>
        <output id="cross-site-result">pending</output>
        <script>
          document.querySelector("#cross-site-result").textContent =
            globalThis.__stasisCrossSiteProbe === "loaded" ? "loaded-without-cookie" : "missing";
        </script>
      </body></html>`,
    );
    return;
  }
  writeServerText(response, 404, "text/plain; charset=utf-8", "not found\n");
});

let runtime;
let app;
let closedCleanly = false;
let v2Runtime;
let v2Session;
let v2ClosedCleanly = false;
let v2MessageChannel;
let v2DirectDataSvg;
let v2InlineSvgRendering;
let v2SettlementUrl;
let v2PersistentIntervalProgression;
let v2InputMethodFocus;
let v2AutomationEventTimestamps;
let v2CssAnimationEventTimestamps;
let v2CookieSession;
let v2CssRuntime;
let v2CssSession;
let v2CssClosedCleanly = false;
let v2CookieRuntime;
let v2CookieSessionHandle;
let v2CookieClosedCleanly = false;
let v2CookieRestoreRuntime;
let v2CookieRestoreSession;
let v2CookieRestoreClosedCleanly = false;
let v2CookieNoImportRuntime;
let v2CookieNoImportSession;
let v2CookieNoImportClosedCleanly = false;
let v2CookieTimeRangeRuntime;
let v2CookieTimeRangeSession;
let v2CookieTimeRangeClosed = false;
let explicitOverrideCacheDirectory;
let explicitOverrideProbeDirectory;
let runtimeWorkingDirectory;
let v2RuntimeWorkingDirectory;
let v2CssRuntimeWorkingDirectory;
let v2CookieRuntimeWorkingDirectory;
let v2CookieRestoreRuntimeWorkingDirectory;
let v2CookieNoImportRuntimeWorkingDirectory;
let v2CookieTimeRangeRuntimeWorkingDirectory;
try {
  // localhost is a distinct schemeful site from the main 127.0.0.1 document. Both exact loopback
  // families serve one port so resolver ordering cannot create a same-site or external fallback.
  const cookieCrossIpv4Url = await listenLoopback(cookieCrossIpv4Server, "127.0.0.1");
  const cookieCrossPort = Number.parseInt(new URL(cookieCrossIpv4Url).port, 10);
  assert.ok(Number.isInteger(cookieCrossPort) && cookieCrossPort > 0);
  const cookieCrossIpv6Url = await listenLoopback(
    cookieCrossIpv6Server,
    "::1",
    cookieCrossPort,
  );
  assert.equal(Number.parseInt(new URL(cookieCrossIpv6Url).port, 10), cookieCrossPort);
  const localhostAddresses = await lookup("localhost", { all: true, verbatim: true });
  assert.ok(localhostAddresses.length > 0, "localhost did not resolve");
  assert.equal(
    localhostAddresses.every(
      ({ address: resolvedAddress, family }) =>
        (resolvedAddress === "127.0.0.1" || resolvedAddress === "::1") &&
        isIP(resolvedAddress) === family,
    ),
    true,
    "localhost resolved outside the exact IPv4/IPv6 loopback listeners",
  );
  cookieCrossUrl = new URL(`http://localhost:${cookieCrossPort}`).origin;
  assert.equal(
    await readLocalHttpText(`${cookieCrossUrl}${cookieCrossPreflightPath}`),
    cookieCrossPreflightBody,
    "localhost HTTP preflight did not reach the shared loopback handler",
  );
  assert.deepEqual(cookieCrossRequests, [], "localhost preflight contaminated proof requests");
  const cookieMainUrl = await listenLoopback(cookieMainServer, "127.0.0.1");
  const cookieMainSite = new URL(cookieMainUrl);
  const cookieCrossSite = new URL(cookieCrossUrl);
  assert.equal(cookieMainSite.protocol, "http:");
  assert.equal(cookieCrossSite.protocol, "http:");
  assert.equal(cookieMainSite.hostname, "127.0.0.1");
  assert.equal(cookieCrossSite.hostname, "localhost");
  assert.notEqual(
    cookieMainSite.hostname,
    cookieCrossSite.hostname,
    "cookie proof requires distinct schemeful sites",
  );
  explicitOverrideCacheDirectory = await mkdtemp(
    join(consumerRoot, ".stasis-explicit-override-cache-"),
  );
  assert.deepEqual(
    await readdir(explicitOverrideCacheDirectory),
    [],
    "explicit executable override trap cache must start empty",
  );
  explicitOverrideProbeDirectory = await mkdtemp(
    join(consumerRoot, ".stasis-explicit-override-probe-"),
  );
  const explicitOverrideMarker = join(explicitOverrideProbeDirectory, "invoked.txt");
  const explicitOverrideWrapper = join(explicitOverrideProbeDirectory, "stasis-wrapper");
  const explicitOverrideProof = `stasis-explicit-override-${process.pid}`;
  await writeFile(
    explicitOverrideWrapper,
    [
      "#!/bin/sh",
      "set -eu",
      "umask 077",
      'printf "%s\\n" "$STASIS_EXPLICIT_OVERRIDE_PROOF" >> "$STASIS_EXPLICIT_OVERRIDE_MARKER"',
      'exec "$STASIS_EXPLICIT_OVERRIDE_BINARY" "$@"',
      "",
    ].join("\n"),
    { encoding: "utf8", flag: "wx", mode: 0o700 },
  );
  await access(explicitOverrideWrapper, fsConstants.X_OK);
  const exactBinaryEnvironment = exactBinaryChildEnvironment({
    STASIS_EXPLICIT_OVERRIDE_BINARY: binary,
    STASIS_EXPLICIT_OVERRIDE_MARKER: explicitOverrideMarker,
    STASIS_EXPLICIT_OVERRIDE_PROOF: explicitOverrideProof,
  });
  assert.ok(
    typeof exactBinaryEnvironment.HOME === "string" && exactBinaryEnvironment.HOME.length > 0,
    "exact-binary cookie controls require one retained host HOME context",
  );
  await assert.rejects(
    access(explicitOverrideMarker, fsConstants.F_OK),
    (error) => error?.code === "ENOENT",
    "explicit executable override marker exists before launch",
  );
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
    executablePath: explicitOverrideWrapper,
    runtimeCacheDirectory: explicitOverrideCacheDirectory,
    env: exactBinaryEnvironment,
    cwd: runtimeWorkingDirectoryRealPath,
    closeTimeoutMs: 30_000,
    ...commandDeadline(),
  });
  const childPid = runtime.pid;
  assert.ok(Number.isSafeInteger(childPid) && childPid > 0, "SDK did not expose the child PID");
  assert.equal(
    await readFile(explicitOverrideMarker, "utf8"),
    `${explicitOverrideProof}\n`,
    "the supplied executablePath wrapper did not launch the native runtime",
  );
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
  assert.deepEqual(
    await readdir(explicitOverrideCacheDirectory),
    [],
    "executablePath unexpectedly accessed the managed runtime cache",
  );
  closedCleanly = true;

  v2RuntimeWorkingDirectory = await mkdtemp(
    join(consumerRoot, ".stasis-v2-runtime-cwd-"),
  );
  const v2RuntimeWorkingDirectoryRealPath = await realpath(v2RuntimeWorkingDirectory);
  assert.equal(
    dirname(v2RuntimeWorkingDirectoryRealPath),
    consumerRootRealPath,
    "v2 runtime cwd escaped the clean consumer root",
  );
  assert.notEqual(
    v2RuntimeWorkingDirectoryRealPath,
    runtimeWorkingDirectoryRealPath,
    "v1 and v2 runtime processes reused the same working directory",
  );
  assert.deepEqual(
    await readdir(v2RuntimeWorkingDirectoryRealPath),
    [],
    "v2 runtime cwd must start empty",
  );

  v2Runtime = await sdk.launch({
    executablePath: explicitOverrideWrapper,
    runtimeCacheDirectory: explicitOverrideCacheDirectory,
    env: exactBinaryEnvironment,
    cwd: v2RuntimeWorkingDirectoryRealPath,
    closeTimeoutMs: 30_000,
    ...commandDeadline(),
  });
  const v2ChildPid = v2Runtime.pid;
  assert.ok(
    Number.isSafeInteger(v2ChildPid) && v2ChildPid > 0,
    "SDK did not expose the v2 child PID",
  );
  assert.equal(v2Runtime.info.implementation.version, expectedVersion);
  assert.deepEqual(v2Runtime.info.implementation.source, expectedSource);
  assert.ok(
    v2Runtime.info.capabilities.profiles.includes(CONTROLLED_WEB_SESSION_V2_PROFILE),
    "exact native runtime did not advertise controlled-web-session-v2",
  );
  assert.equal(
    await readFile(explicitOverrideMarker, "utf8"),
    `${explicitOverrideProof}\n${explicitOverrideProof}\n`,
    "the packed-SDK v1 and v2 processes did not both launch through the exact-binary wrapper",
  );

  const v2FixtureUrl = "https://packed-sdk-message-channel-v2.example.test/";
  const v2ImageFixtureUrl = "https://packed-sdk-direct-data-svg-v2.example.test/";
  const v2HttpImageFixtureUrl = "https://packed-sdk-direct-http-image-v2.example.test/";
  const v2HttpImageAssetUrl =
    "https://controlled-image-assets.example.test/controlled-v2-http-image.svg";
  const v2HttpInvalidImageAssetUrl =
    "https://controlled-image-assets.example.test/controlled-v2-http-image.invalid";
  const v2HttpImageSvgBody =
    '<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><rect width="2" height="2" fill="green"/></svg>';
  const v2InlineSvgFixtureUrl = "https://packed-sdk-inline-svg-v2.example.test/";
  const v2InlineSvgSharedPendingFixtureUrl =
    "https://packed-sdk-inline-svg-shared-pending-v2.example.test/";
  const v2SettlementUrlFixtureUrl =
    "https://packed-sdk-settlement-url-v2.example.test/settlement-url/start?proof=open";
  const v2SettlementInitialUrl =
    "https://packed-sdk-settlement-url-v2.example.test/settlement-url/replaced?proof=initial#attested";
  const v2SettlementHistoryUrl =
    "https://packed-sdk-settlement-url-v2.example.test/settlement-url/pushed?proof=history#attested";
  const v2SettlementReplacementUrl =
    "https://packed-sdk-settlement-url-v2.example.test/settlement-url/final?proof=replacement#attested";
  const v2IntervalBeforeFiniteFixtureUrl =
    "https://packed-sdk-interval-before-finite-v2.example.test/";
  const v2FocusFixtureUrl = "https://packed-sdk-input-method-focus-v2.example.test/";
  const v2AutomationEventFixtureUrl =
    "https://packed-sdk-automation-event-timestamps-v2.example.test/";
  v2Session = await v2Runtime.openSession(v2FixtureUrl, {
    profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
    clock: {
      mode: "controlled",
      initialVirtualTimeNs: 0n,
      unixTimeOriginNs: 0n,
    },
    network: {
      mode: "fixtures_only",
      routes: [
        {
          match: { method: "GET", url: { exact: v2FixtureUrl } },
          fulfill: {
            status: 200,
            headers: [["content-type", "text/html; charset=utf-8"]],
            body: { utf8: sessionV2FixtureBody },
          },
        },
        {
          match: { method: "GET", url: { exact: v2ImageFixtureUrl } },
          fulfill: {
            status: 200,
            headers: [["content-type", "text/html; charset=utf-8"]],
            body: { utf8: sessionV2ImageFixtureBody },
          },
        },
        {
          match: { method: "GET", url: { exact: v2HttpImageFixtureUrl } },
          fulfill: {
            status: 200,
            headers: [["content-type", "text/html; charset=utf-8"]],
            body: { utf8: sessionV2HttpImageFixtureBody },
          },
        },
        {
          match: { method: "GET", url: { exact: v2HttpImageAssetUrl } },
          fulfill: {
            status: 200,
            headers: [["content-type", "image/svg+xml"]],
            body: { utf8: v2HttpImageSvgBody },
          },
        },
        {
          match: { method: "GET", url: { exact: v2HttpInvalidImageAssetUrl } },
          fulfill: {
            status: 200,
            headers: [["content-type", "image/png"]],
            body: { utf8: "not an image" },
          },
        },
        {
          match: { method: "GET", url: { exact: v2InlineSvgFixtureUrl } },
          fulfill: {
            status: 200,
            headers: [["content-type", "text/html; charset=utf-8"]],
            body: { utf8: sessionV2InlineSvgFixtureBody },
          },
        },
        {
          match: {
            method: "GET",
            url: { exact: v2InlineSvgSharedPendingFixtureUrl },
          },
          fulfill: {
            status: 200,
            headers: [["content-type", "text/html; charset=utf-8"]],
            body: { utf8: sessionV2InlineSvgSharedPendingFixtureBody },
          },
        },
        {
          match: { method: "GET", url: { exact: v2SettlementUrlFixtureUrl } },
          fulfill: {
            status: 200,
            headers: [["content-type", "text/html; charset=utf-8"]],
            body: { utf8: sessionV2SettlementUrlFixtureBody },
          },
        },
        {
          match: { method: "GET", url: { exact: v2IntervalBeforeFiniteFixtureUrl } },
          fulfill: {
            status: 200,
            headers: [["content-type", "text/html; charset=utf-8"]],
            body: { utf8: sessionV2IntervalBeforeFiniteFixtureBody },
          },
        },
        {
          match: { method: "GET", url: { exact: v2FocusFixtureUrl } },
          fulfill: {
            status: 200,
            headers: [["content-type", "text/html; charset=utf-8"]],
            body: { utf8: sessionV2FocusFixtureBody },
          },
        },
        {
          match: { method: "GET", url: { exact: v2AutomationEventFixtureUrl } },
          fulfill: {
            status: 200,
            headers: [["content-type", "text/html; charset=utf-8"]],
            body: { utf8: sessionV2AutomationEventFixtureBody },
          },
        },
      ],
    },
  });
  assert.equal(v2Session.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);

  const v2Idle = await v2Session.settle(v2Session.stateToken);
  assert.equal(v2Idle.outcome, "quiescent", "an idle open MessageChannel pair blocked settlement");
  const idleMessagePortSources = countMessagePortSources(v2Idle.snapshot);
  assert.equal(idleMessagePortSources, 0, "an idle MessageChannel reported pending port work");
  assert.deepEqual(v2Idle.snapshot.runtimeFailures, []);
  const buffered = await v2Session.activate("#buffer", v2Idle.stateToken);
  const bufferActionRotatedStateToken = buffered.stateToken !== v2Idle.stateToken;
  assert.equal(
    bufferActionRotatedStateToken,
    true,
    "buffer action did not rotate the document state token",
  );
  const v2Pending = await v2Session.pending();
  const pendingPreservedBufferedStateToken = v2Pending.stateToken === buffered.stateToken;
  assert.equal(
    pendingPreservedBufferedStateToken,
    true,
    "pending observation did not preserve the buffered document state token",
  );
  const pendingMessagePortSources = countMessagePortSources(v2Pending);
  assert.equal(
    pendingMessagePortSources,
    1,
    "buffered controlled-local MessagePort work did not project as exactly one source",
  );
  assert.deepEqual(v2Pending.runtimeFailures, []);
  const started = await v2Session.activate("#start", v2Pending.stateToken);
  const startActionRotatedStateToken = started.stateToken !== v2Pending.stateToken;
  assert.equal(
    startActionRotatedStateToken,
    true,
    "start action did not rotate the document state token",
  );
  const v2Drained = await v2Session.settle(started.stateToken);
  assert.equal(v2Drained.outcome, "quiescent");
  assert.ok(v2Drained.processed.tasks >= 2n);
  assert.deepEqual(v2Drained.unsupportedWork, []);
  assert.deepEqual(v2Drained.snapshot.runtimeFailures, []);
  const drainedMessagePortSources = countMessagePortSources(v2Drained.snapshot);
  assert.equal(
    drainedMessagePortSources,
    0,
    "drained controlled-local MessagePort work remained pending",
  );
  const aggregateProcessedOrdinaryTasks = v2Drained.processed.tasks.toString();
  assert.match(aggregateProcessedOrdinaryTasks, /^(0|[1-9][0-9]*)$/);
  const v2TraceResult = await v2Session.text("#result", v2Drained.stateToken);
  const v2Trace = v2TraceResult.value;
  assert.equal(v2Trace, "callback1>microtask1>callback2>microtask2");
  const v2Evidence = v2Session.settlementEvidence(v2Drained);
  assert.equal(v2Evidence.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);

  const v2ImageNavigation = await v2Session.navigate(
    v2ImageFixtureUrl,
    v2TraceResult.stateToken,
    commandDeadline(),
  );
  assert.equal(v2ImageNavigation.boundary, "controlled_ready");
  const v2ImageSettled = await v2Session.settle(
    v2ImageNavigation.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(v2ImageSettled.outcome, "quiescent");
  assert.deepEqual(v2ImageSettled.unsupportedWork, []);
  assert.deepEqual(v2ImageSettled.externalIo, []);
  assert.equal(v2ImageSettled.snapshot.producers.pending, 0n);
  assert.equal(v2ImageSettled.snapshot.producers.terminal, false);
  assert.equal(v2ImageSettled.snapshot.rendering.pendingImages, 0n);
  assert.deepEqual(v2ImageSettled.snapshot.runtimeFailures, []);
  const v2ImageTraceResult = await v2Session.text(
    "#result",
    v2ImageSettled.stateToken,
    commandDeadline(),
  );
  assert.equal(v2ImageTraceResult.value, "load:0>loadend:0|now:0");
  const v2ImageEvidence = v2Session.settlementEvidence(v2ImageSettled);
  assert.equal(v2ImageEvidence.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);

  const v2HttpImageNavigation = await v2Session.navigate(
    v2HttpImageFixtureUrl,
    v2ImageTraceResult.stateToken,
    commandDeadline(),
  );
  assert.equal(v2HttpImageNavigation.boundary, "controlled_ready");
  const v2HttpImageSettled = await v2Session.settle(
    v2HttpImageNavigation.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(v2HttpImageSettled.outcome, "quiescent");
  assert.deepEqual(v2HttpImageSettled.unsupportedWork, []);
  assert.deepEqual(v2HttpImageSettled.externalIo, []);
  assert.equal(v2HttpImageSettled.snapshot.producers.pending, 0n);
  assert.equal(v2HttpImageSettled.snapshot.producers.terminal, false);
  assert.equal(v2HttpImageSettled.snapshot.rendering.pendingImages, 0n);
  assert.deepEqual(v2HttpImageSettled.snapshot.runtimeFailures, []);
  const v2HttpImageTraceResult = await v2Session.text(
    "#result",
    v2HttpImageSettled.stateToken,
    commandDeadline(),
  );
  assert.equal(
    v2HttpImageTraceResult.value,
    "loaded:load:0>loadend:0|failed:error:0>loadend:0|cached:load:0|now:0",
  );
  const v2HttpImageEvidence = v2Session.settlementEvidence(v2HttpImageSettled);
  assert.equal(v2HttpImageEvidence.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);

  const v2InlineSvgNavigation = await v2Session.navigate(
    v2InlineSvgFixtureUrl,
    v2HttpImageTraceResult.stateToken,
    commandDeadline(),
  );
  assert.equal(v2InlineSvgNavigation.boundary, "controlled_ready");
  const v2InlineSvgSettled = await v2Session.settle(
    v2InlineSvgNavigation.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(v2InlineSvgSettled.outcome, "quiescent");
  assert.deepEqual(v2InlineSvgSettled.unsupportedWork, []);
  assert.deepEqual(v2InlineSvgSettled.externalIo, []);
  assert.equal(v2InlineSvgSettled.snapshot.producers.pending, 0n);
  assert.equal(v2InlineSvgSettled.snapshot.producers.terminal, false);
  assert.equal(v2InlineSvgSettled.snapshot.rendering.pendingImages, 0n);
  assert.deepEqual(v2InlineSvgSettled.snapshot.runtimeFailures, []);
  const v2InlineSvgTraceResult = await v2Session.text(
    "#result",
    v2InlineSvgSettled.stateToken,
    commandDeadline(),
  );
  assert.equal(v2InlineSvgTraceResult.value, "inline-svg:4x3|events:0|now:0");
  const inlineSvgTraceMatch =
    /^inline-svg:4x3\|events:([0-9]+)\|now:0$/u.exec(v2InlineSvgTraceResult.value);
  assert.ok(inlineSvgTraceMatch, "inline SVG fixture trace is not canonical");
  const v2InlineSvgDomCompletionEvents = inlineSvgTraceMatch[1];
  assert.equal(v2InlineSvgDomCompletionEvents, "0");
  const v2InlineSvgEvidence = v2Session.settlementEvidence(v2InlineSvgSettled);
  assert.equal(v2InlineSvgEvidence.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);

  const v2InlineSvgSharedPendingNavigation = await v2Session.navigate(
    v2InlineSvgSharedPendingFixtureUrl,
    v2InlineSvgTraceResult.stateToken,
    commandDeadline(),
  );
  assert.equal(v2InlineSvgSharedPendingNavigation.boundary, "controlled_ready");
  const v2InlineSvgSharedPendingSettled = await v2Session.settle(
    v2InlineSvgSharedPendingNavigation.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(v2InlineSvgSharedPendingSettled.outcome, "quiescent");
  assert.deepEqual(v2InlineSvgSharedPendingSettled.unsupportedWork, []);
  assert.deepEqual(v2InlineSvgSharedPendingSettled.externalIo, []);
  assert.equal(v2InlineSvgSharedPendingSettled.snapshot.producers.pending, 0n);
  assert.equal(v2InlineSvgSharedPendingSettled.snapshot.producers.terminal, false);
  assert.equal(v2InlineSvgSharedPendingSettled.snapshot.rendering.pendingImages, 0n);
  assert.deepEqual(v2InlineSvgSharedPendingSettled.snapshot.runtimeFailures, []);
  const v2InlineSvgSharedPendingTraceResult = await v2Session.text(
    "#result",
    v2InlineSvgSharedPendingSettled.stateToken,
    commandDeadline(),
  );
  assert.equal(
    v2InlineSvgSharedPendingTraceResult.value,
    "shared-inline-svg:12|now:0",
  );
  const v2InlineSvgSharedPendingEvidence = v2Session.settlementEvidence(
    v2InlineSvgSharedPendingSettled,
  );
  assert.equal(
    v2InlineSvgSharedPendingEvidence.profile,
    CONTROLLED_WEB_SESSION_V2_PROFILE,
  );

  const v2SettlementUrlNavigation = await v2Session.navigate(
    v2SettlementUrlFixtureUrl,
    v2InlineSvgSharedPendingTraceResult.stateToken,
    commandDeadline(),
  );
  assert.equal(v2SettlementUrlNavigation.boundary, "controlled_ready");
  assert.equal(v2Session.url, v2FixtureUrl, "Session.url mutated after navigation");
  const v2SettlementInitial = await v2Session.settle(
    v2SettlementUrlNavigation.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(v2SettlementInitial.outcome, "quiescent");
  assert.equal(v2SettlementInitial.url, v2SettlementInitialUrl);
  assert.equal(v2Session.url, v2FixtureUrl, "Session.url mutated after settlement");
  const v2SettlementInitialSessionEvidence =
    v2Session.settlementEvidence(v2SettlementInitial);
  const v2SettlementInitialStandaloneEvidence = sdk.settlementEvidence(
    v2SettlementInitial,
  );
  assert.equal(Object.hasOwn(v2SettlementInitialSessionEvidence, "url"), false);
  assert.equal(Object.hasOwn(v2SettlementInitialStandaloneEvidence, "url"), false);

  const v2SettlementHistoryAction = await v2Session.activate(
    "#push",
    v2SettlementInitial.stateToken,
    commandDeadline(),
  );
  const v2SettlementHistory = await v2Session.settle(
    v2SettlementHistoryAction.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(v2SettlementHistory.outcome, "quiescent");
  assert.equal(v2SettlementHistory.url, v2SettlementHistoryUrl);
  assert.equal(v2Session.url, v2FixtureUrl, "Session.url mutated after pushState");
  assert.equal(
    Object.hasOwn(v2Session.settlementEvidence(v2SettlementHistory), "url"),
    false,
  );

  const v2SettlementReplacementAction = await v2Session.activate(
    "#replace",
    v2SettlementHistory.stateToken,
    commandDeadline(),
  );
  const v2SettlementReplacement = await v2Session.settle(
    v2SettlementReplacementAction.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(v2SettlementReplacement.outcome, "quiescent");
  assert.equal(v2SettlementReplacement.url, v2SettlementReplacementUrl);
  assert.equal(v2Session.url, v2FixtureUrl, "Session.url mutated after replaceState");
  const v2SettlementReplacementEvidence = v2Session.settlementEvidence(
    v2SettlementReplacement,
  );
  assert.equal(Object.hasOwn(v2SettlementReplacementEvidence, "url"), false);
  const v2SettlementTraceResult = await v2Session.text(
    "#result",
    v2SettlementReplacement.stateToken,
    commandDeadline(),
  );
  assert.equal(v2SettlementTraceResult.value, "replaced");

  const v2FocusNavigation = await v2Session.navigate(
    v2FocusFixtureUrl,
    v2SettlementTraceResult.stateToken,
    commandDeadline(),
  );
  assert.equal(v2FocusNavigation.boundary, "controlled_ready");
  const v2FocusSettled = await v2Session.settle(
    v2FocusNavigation.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(v2FocusSettled.outcome, "quiescent");
  assert.deepEqual(v2FocusSettled.unsupportedWork, []);
  assert.deepEqual(v2FocusSettled.externalIo, []);
  assert.equal(v2FocusSettled.snapshot.producers.pending, 0n);
  assert.equal(v2FocusSettled.snapshot.producers.terminal, false);
  assert.deepEqual(v2FocusSettled.snapshot.runtimeFailures, []);
  const v2FocusTraceResult = await v2Session.text(
    "#result",
    v2FocusSettled.stateToken,
    commandDeadline(),
  );
  assert.equal(
    v2FocusTraceResult.value,
    "blurred|4|focus:trusted:0>focusin:trusted:0>blur:trusted:0>focusout:trusted:0|rwa-value|2:5",
  );
  const v2FocusEvidence = v2Session.settlementEvidence(v2FocusSettled);
  assert.equal(v2FocusEvidence.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);

  const v2PersistentIntervalSessionBaselineVirtualTimeNs = v2FocusSettled.virtualTimeNs;
  assert.equal(v2PersistentIntervalSessionBaselineVirtualTimeNs, 260_000_000n);
  const v2PersistentIntervalNavigation = await v2Session.navigate(
    v2IntervalBeforeFiniteFixtureUrl,
    v2FocusTraceResult.stateToken,
    commandDeadline(),
  );
  assert.equal(v2PersistentIntervalNavigation.boundary, "controlled_ready");
  const v2PersistentIntervalImplicitPending = await v2Session.pending(commandDeadline());
  const v2PersistentIntervalDocumentElapsedTimeNs =
    v2PersistentIntervalImplicitPending.virtualTimeNs -
    v2PersistentIntervalSessionBaselineVirtualTimeNs;
  assert.equal(v2PersistentIntervalDocumentElapsedTimeNs, 12_000_000_000n);
  assert.equal(v2PersistentIntervalImplicitPending.timers.persistent, 1n);
  assert.equal(v2PersistentIntervalImplicitPending.timers.futureFinite, 0n);
  assert.deepEqual(v2PersistentIntervalImplicitPending.runtimeFailures, []);
  const v2PersistentIntervalImplicitTrace = await v2Session.text(
    "#trace",
    v2PersistentIntervalImplicitPending.stateToken,
    commandDeadline(),
  );
  assert.equal(
    v2PersistentIntervalImplicitTrace.value,
    "interval:1@5000|interval:2@10000|finite@12000",
  );
  const v2PersistentIntervalStrict = await v2Session.settle(
    v2PersistentIntervalImplicitTrace.stateToken,
    { persistentWork: "strict" },
    commandDeadline(),
  );
  assert.equal(v2PersistentIntervalStrict.outcome, "blocked_on_open_ended_work");
  assert.equal(
    v2PersistentIntervalStrict.virtualTimeNs,
    v2PersistentIntervalImplicitPending.virtualTimeNs,
  );
  assert.equal(v2PersistentIntervalStrict.snapshot.timers.persistent, 1n);
  assert.equal(v2PersistentIntervalStrict.snapshot.timers.futureFinite, 0n);
  assert.deepEqual(v2PersistentIntervalStrict.snapshot.runtimeFailures, []);
  assert.deepEqual(v2PersistentIntervalStrict.unsupportedWork, []);
  assert.deepEqual(v2PersistentIntervalStrict.externalIo, []);
  const v2PersistentIntervalStrictTrace = await v2Session.text(
    "#trace",
    v2PersistentIntervalStrict.stateToken,
    commandDeadline(),
  );
  assert.equal(
    v2PersistentIntervalStrictTrace.value,
    v2PersistentIntervalImplicitTrace.value,
    "strict classification executed another interval callback",
  );
  const v2PersistentIntervalReported = await v2Session.settle(
    v2PersistentIntervalStrictTrace.stateToken,
    { persistentWork: "report" },
    commandDeadline(),
  );
  assert.equal(v2PersistentIntervalReported.outcome, "quiescent_with_persistent_work");
  assert.equal(
    v2PersistentIntervalReported.virtualTimeNs,
    v2PersistentIntervalImplicitPending.virtualTimeNs,
  );
  assert.equal(v2PersistentIntervalReported.snapshot.timers.persistent, 1n);
  assert.equal(v2PersistentIntervalReported.snapshot.timers.futureFinite, 0n);
  assert.deepEqual(v2PersistentIntervalReported.snapshot.runtimeFailures, []);
  assert.deepEqual(v2PersistentIntervalReported.unsupportedWork, []);
  assert.deepEqual(v2PersistentIntervalReported.externalIo, []);
  assert.equal(v2PersistentIntervalReported.persistentWork.length, 1);
  const [v2PersistentIntervalWork] = v2PersistentIntervalReported.persistentWork;
  assert.equal(v2PersistentIntervalWork.kind, "timer");
  assert.equal(v2PersistentIntervalWork.reason, "interval");
  assert.equal(v2PersistentIntervalWork.count, 1n);
  assert.equal(v2PersistentIntervalWork.requestedPeriodNs, 5_000_000_000n);
  const v2PersistentIntervalReportTrace = await v2Session.text(
    "#trace",
    v2PersistentIntervalReported.stateToken,
    commandDeadline(),
  );
  assert.equal(
    v2PersistentIntervalReportTrace.value,
    v2PersistentIntervalStrictTrace.value,
    "report-mode checkpoint executed another interval callback",
  );
  const v2PersistentIntervalEvidence = v2Session.settlementEvidence(
    v2PersistentIntervalReported,
  );
  assert.equal(
    v2PersistentIntervalEvidence.profile,
    CONTROLLED_WEB_SESSION_V2_PROFILE,
  );

  const v2AutomationNavigation = await v2Session.navigate(
    v2AutomationEventFixtureUrl,
    v2PersistentIntervalReportTrace.stateToken,
    commandDeadline(),
  );
  assert.equal(v2AutomationNavigation.boundary, "controlled_ready");
  const v2AutomationInitial = await v2Session.settle(
    v2AutomationNavigation.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(v2AutomationInitial.outcome, "quiescent");
  assert.deepEqual(v2AutomationInitial.unsupportedWork, []);
  assert.deepEqual(v2AutomationInitial.externalIo, []);
  assert.deepEqual(v2AutomationInitial.snapshot.runtimeFailures, []);
  const v2AutomationBaselineVirtualTimeNs = v2AutomationInitial.virtualTimeNs;

  const v2AutomationScheduled = await v2Session.activate(
    "#start",
    v2AutomationInitial.stateToken,
    commandDeadline(),
  );
  const v2AutomationQualified = await v2Session.settle(
    v2AutomationScheduled.stateToken,
    { maxVirtualTimeNs: 0n },
    commandDeadline(),
  );
  assert.equal(
    v2AutomationQualified.virtualTimeNs,
    v2AutomationBaselineVirtualTimeNs,
  );
  const v2AutomationAdvanced = await v2Session.advanceToNext(
    v2AutomationQualified.stateToken,
    commandDeadline(),
  );
  assert.equal(v2AutomationAdvanced.outcome, "advanced");
  assert.equal(
    v2AutomationAdvanced.virtualTimeNs,
    v2AutomationBaselineVirtualTimeNs + 5_000_000n,
  );
  const v2AutomationDispatched = await v2Session.settle(
    v2AutomationAdvanced.stateToken,
    { maxVirtualTimeNs: 0n },
    commandDeadline(),
  );
  assert.equal(
    v2AutomationDispatched.virtualTimeNs,
    v2AutomationBaselineVirtualTimeNs + 5_000_000n,
  );

  const v2AutomationFilled = await v2Session.fill(
    "#fill",
    "replacement",
    v2AutomationDispatched.stateToken,
    commandDeadline(),
  );
  const v2AutomationActivated = await v2Session.activate(
    "#activate",
    v2AutomationFilled.stateToken,
    commandDeadline(),
  );
  const v2AutomationReset = await v2Session.activate(
    "#reset",
    v2AutomationActivated.stateToken,
    commandDeadline(),
  );
  const v2AutomationChecked = await v2Session.check(
    "#check",
    v2AutomationReset.stateToken,
    commandDeadline(),
  );
  const v2AutomationSelected = await v2Session.select(
    "#select",
    ["two"],
    v2AutomationChecked.stateToken,
    commandDeadline(),
  );
  const v2AutomationInvalidSubmitted = await v2Session.submit(
    "#invalid-form",
    v2AutomationSelected.stateToken,
    commandDeadline(),
  );
  const v2AutomationValidSubmitted = await v2Session.submit(
    "#valid-form",
    v2AutomationInvalidSubmitted.stateToken,
    commandDeadline(),
  );
  const v2AutomationControlledTraceResult = await v2Session.text(
    "#result",
    v2AutomationValidSubmitted.stateToken,
    commandDeadline(),
  );
  const v2AutomationControlledTraceParts =
    v2AutomationControlledTraceResult.value.split("|");
  assert.equal(v2AutomationControlledTraceParts.length, 4);
  const [v2AutomationControlledSampleMs, , , v2AutomationBaselineSampleMs] =
    v2AutomationControlledTraceParts;
  assert.match(v2AutomationControlledSampleMs, /^(?:0|[1-9][0-9]*)$/u);
  assert.match(v2AutomationBaselineSampleMs, /^(?:0|[1-9][0-9]*)$/u);
  const v2AutomationControlledSampleNs = BigInt(v2AutomationControlledSampleMs) * 1_000_000n;
  const v2AutomationBaselineSampleNs = BigInt(v2AutomationBaselineSampleMs) * 1_000_000n;
  assert.equal(
    v2AutomationControlledSampleNs,
    v2AutomationBaselineSampleNs + 5_000_000n,
    "the document clock did not advance exactly five milliseconds",
  );
  assert.ok(
    v2AutomationBaselineSampleNs < v2AutomationBaselineVirtualTimeNs,
    "the reused document clock was conflated with the session-global clock",
  );
  const v2AutomationControlledEventKinds = [
    "fill:input",
    "activate:click",
    "reset:reset",
    "check:click",
    "check:input",
    "check:change",
    "select:input",
    "select:change",
    "invalid:invalid",
    "submit:submit",
    "submit:formdata",
  ];
  const expectedV2AutomationControlledEvents = v2AutomationControlledEventKinds.map(
    (entry) => `${entry}:${v2AutomationControlledSampleMs}`,
  );
  const expectedV2AutomationControlledTrace =
    `${v2AutomationControlledSampleMs}|${expectedV2AutomationControlledEvents.join(">")}|not-read|${v2AutomationBaselineSampleMs}`;
  assert.equal(v2AutomationControlledTraceResult.value, expectedV2AutomationControlledTrace);
  const v2AutomationControlledEvents = v2AutomationControlledTraceParts[1]?.split(">") ?? [];
  assert.equal(v2AutomationControlledEvents.length, 11);
  assert.ok(
    v2AutomationControlledEvents.every((entry) =>
      entry.endsWith(`:${v2AutomationControlledSampleMs}`),
    ),
    "an engine-generated synchronous automation event escaped the document-clock sample",
  );

  const v2AutomationScriptTriggered = await v2Session.activate(
    "#script-created",
    v2AutomationControlledTraceResult.stateToken,
    commandDeadline(),
  );
  const v2AutomationScriptTraceResult = await v2Session.text(
    "#result",
    v2AutomationScriptTriggered.stateToken,
    commandDeadline(),
  );
  const expectedV2AutomationScriptTrace =
    `${v2AutomationControlledSampleMs}|${[
      ...expectedV2AutomationControlledEvents,
      `script-trigger:click:${v2AutomationControlledSampleMs}`,
    ].join(">")}|0,0,0,0,0|${v2AutomationBaselineSampleMs}`;
  assert.equal(v2AutomationScriptTraceResult.value, expectedV2AutomationScriptTrace);
  const v2AutomationScriptTraceParts = v2AutomationScriptTraceResult.value.split("|");
  assert.equal(v2AutomationScriptTraceParts[1]?.split(">").length, 12);
  assert.equal(v2AutomationScriptTraceParts[2], "0,0,0,0,0");

  const v2AutomationRejected = await v2Session.settle(
    v2AutomationScriptTraceResult.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(v2AutomationRejected.outcome, "unsupported_work");
  assert.equal(v2AutomationRejected.failure?.code, "unsupported_clock_surface");
  assert.equal(v2AutomationRejected.unsupportedWork.length, 1);
  const [v2AutomationUnsupported] = v2AutomationRejected.unsupportedWork;
  assert.equal(v2AutomationUnsupported.kind, "other");
  assert.equal(v2AutomationUnsupported.count, 1n);
  assert.equal(v2AutomationUnsupported.reason, "time_surface");
  assert.equal(v2AutomationUnsupported.timeSurface, "host_timestamp");
  assert.equal(v2AutomationRejected.url, v2AutomationEventFixtureUrl);
  assert.equal(
    v2Session.url,
    v2FixtureUrl,
    "Session.url mutated after an unsupported terminal settlement",
  );
  const v2AutomationEvidence = v2Session.settlementEvidence(v2AutomationRejected);
  assert.equal(v2AutomationEvidence.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);
  assert.equal(Object.hasOwn(v2AutomationEvidence, "url"), false);
  assert.equal(
    Object.hasOwn(sdk.settlementEvidence(v2AutomationRejected), "url"),
    false,
  );

  await v2Session.close(commandDeadline());
  await new Promise((resolveImmediate) => setImmediate(resolveImmediate));
  let v2ProcessStillExists = true;
  try {
    process.kill(v2ChildPid, 0);
  } catch (error) {
    if (error?.code !== "ESRCH") throw error;
    v2ProcessStillExists = false;
  }
  assert.equal(v2ProcessStillExists, false, "v2 Stasis child still exists after close and EOF");
  assert.deepEqual(
    await readdir(explicitOverrideCacheDirectory),
    [],
    "the v2 explicit executable override unexpectedly accessed the managed runtime cache",
  );
  v2ClosedCleanly = true;
  v2MessageChannel = {
    profile: v2Session.profile,
    idleOutcome: v2Idle.outcome,
    idleMessagePortSources: String(idleMessagePortSources),
    idleRuntimeFailures: String(v2Idle.snapshot.runtimeFailures.length),
    bufferActionRotatedStateToken,
    pendingPreservedBufferedStateToken,
    pendingMessagePortSources: String(pendingMessagePortSources),
    pendingRuntimeFailures: String(v2Pending.runtimeFailures.length),
    startActionRotatedStateToken,
    drainedOutcome: v2Drained.outcome,
    drainedMessagePortSources: String(drainedMessagePortSources),
    drainedRuntimeFailures: String(v2Drained.snapshot.runtimeFailures.length),
    aggregateProcessedOrdinaryTasks,
    trace: v2Trace,
    evidenceProfile: v2Evidence.profile,
    unsupportedWork: String(v2Drained.unsupportedWork.length),
    exactBinaryLaunch: true,
    closeResponseAndEof: true,
  };
  v2DirectDataSvg = {
    profile: v2Session.profile,
    navigationBoundary: v2ImageNavigation.boundary,
    outcome: v2ImageSettled.outcome,
    producerPending: String(v2ImageSettled.snapshot.producers.pending),
    producerTerminal: v2ImageSettled.snapshot.producers.terminal,
    pendingImages: String(v2ImageSettled.snapshot.rendering.pendingImages),
    runtimeFailures: String(v2ImageSettled.snapshot.runtimeFailures.length),
    unsupportedWork: String(v2ImageSettled.unsupportedWork.length),
    externalIo: String(v2ImageSettled.externalIo.length),
    completionTrace: v2ImageTraceResult.value,
    evidenceProfile: v2ImageEvidence.profile,
    httpNavigationBoundary: v2HttpImageNavigation.boundary,
    httpOutcome: v2HttpImageSettled.outcome,
    httpProducerPending: String(v2HttpImageSettled.snapshot.producers.pending),
    httpProducerTerminal: v2HttpImageSettled.snapshot.producers.terminal,
    httpPendingImages: String(v2HttpImageSettled.snapshot.rendering.pendingImages),
    httpRuntimeFailures: String(v2HttpImageSettled.snapshot.runtimeFailures.length),
    httpUnsupportedWork: String(v2HttpImageSettled.unsupportedWork.length),
    httpExternalIo: String(v2HttpImageSettled.externalIo.length),
    httpCompletionTrace: v2HttpImageTraceResult.value,
    httpEvidenceProfile: v2HttpImageEvidence.profile,
    sameControlledSession: true,
    exactBinaryLaunch: true,
    closeResponseAndEof: true,
  };
  v2InlineSvgRendering = {
    profile: v2Session.profile,
    navigationBoundary: v2InlineSvgNavigation.boundary,
    outcome: v2InlineSvgSettled.outcome,
    producerPending: String(v2InlineSvgSettled.snapshot.producers.pending),
    producerTerminal: v2InlineSvgSettled.snapshot.producers.terminal,
    pendingImages: String(v2InlineSvgSettled.snapshot.rendering.pendingImages),
    runtimeFailures: String(v2InlineSvgSettled.snapshot.runtimeFailures.length),
    unsupportedWork: String(v2InlineSvgSettled.unsupportedWork.length),
    externalIo: String(v2InlineSvgSettled.externalIo.length),
    fixtureTrace: v2InlineSvgTraceResult.value,
    domCompletionEvents: v2InlineSvgDomCompletionEvents,
    evidenceProfile: v2InlineSvgEvidence.profile,
    sharedNavigationBoundary: v2InlineSvgSharedPendingNavigation.boundary,
    sharedOutcome: v2InlineSvgSharedPendingSettled.outcome,
    sharedProducerPending: String(
      v2InlineSvgSharedPendingSettled.snapshot.producers.pending,
    ),
    sharedProducerTerminal: v2InlineSvgSharedPendingSettled.snapshot.producers.terminal,
    sharedPendingImages: String(
      v2InlineSvgSharedPendingSettled.snapshot.rendering.pendingImages,
    ),
    sharedRuntimeFailures: String(
      v2InlineSvgSharedPendingSettled.snapshot.runtimeFailures.length,
    ),
    sharedUnsupportedWork: String(v2InlineSvgSharedPendingSettled.unsupportedWork.length),
    sharedExternalIo: String(v2InlineSvgSharedPendingSettled.externalIo.length),
    sharedFixtureTrace: v2InlineSvgSharedPendingTraceResult.value,
    sharedEvidenceProfile: v2InlineSvgSharedPendingEvidence.profile,
    sameControlledSession: true,
    exactBinaryLaunch: true,
    closeResponseAndEof: true,
  };
  v2SettlementUrl = {
    profile: v2Session.profile,
    navigationBoundary: v2SettlementUrlNavigation.boundary,
    controlledOpenUrl: v2Session.url,
    initialOutcome: v2SettlementInitial.outcome,
    initialUrl: v2SettlementInitial.url,
    historyOutcome: v2SettlementHistory.outcome,
    historyUrl: v2SettlementHistory.url,
    replacementOutcome: v2SettlementReplacement.outcome,
    replacementUrl: v2SettlementReplacement.url,
    replacementTrace: v2SettlementTraceResult.value,
    sessionUrlStayedAtControlledOpen: v2Session.url === v2FixtureUrl,
    sessionEvidenceExcludesUrl:
      !Object.hasOwn(v2SettlementInitialSessionEvidence, "url") &&
      !Object.hasOwn(v2SettlementReplacementEvidence, "url") &&
      !Object.hasOwn(v2AutomationEvidence, "url"),
    standaloneEvidenceExcludesUrl:
      !Object.hasOwn(v2SettlementInitialStandaloneEvidence, "url") &&
      !Object.hasOwn(sdk.settlementEvidence(v2AutomationRejected), "url"),
    unsupportedOutcome: v2AutomationRejected.outcome,
    unsupportedFailureCode: v2AutomationRejected.failure?.code,
    unsupportedUrl: v2AutomationRejected.url,
    exactBinaryLaunch: true,
    closeResponseAndEof: true,
  };
  v2PersistentIntervalProgression = {
    profile: v2Session.profile,
    navigationBoundary: v2PersistentIntervalNavigation.boundary,
    sessionBaselineVirtualTimeNs: String(
      v2PersistentIntervalSessionBaselineVirtualTimeNs,
    ),
    documentElapsedTimeNs: String(v2PersistentIntervalDocumentElapsedTimeNs),
    implicitVirtualTimeNs: String(v2PersistentIntervalImplicitPending.virtualTimeNs),
    implicitPersistentTimers: String(v2PersistentIntervalImplicitPending.timers.persistent),
    implicitFutureFinite: String(v2PersistentIntervalImplicitPending.timers.futureFinite),
    implicitTrace: v2PersistentIntervalImplicitTrace.value,
    strictOutcome: v2PersistentIntervalStrict.outcome,
    strictVirtualTimeNs: String(v2PersistentIntervalStrict.virtualTimeNs),
    strictTrace: v2PersistentIntervalStrictTrace.value,
    reportOutcome: v2PersistentIntervalReported.outcome,
    reportVirtualTimeNs: String(v2PersistentIntervalReported.virtualTimeNs),
    reportTrace: v2PersistentIntervalReportTrace.value,
    persistentTimers: String(v2PersistentIntervalReported.snapshot.timers.persistent),
    futureFinite: String(v2PersistentIntervalReported.snapshot.timers.futureFinite),
    persistentKind: v2PersistentIntervalWork.kind,
    persistentReason: v2PersistentIntervalWork.reason,
    persistentCount: String(v2PersistentIntervalWork.count),
    requestedPeriodNs: String(v2PersistentIntervalWork.requestedPeriodNs),
    runtimeFailures: String(v2PersistentIntervalReported.snapshot.runtimeFailures.length),
    unsupportedWork: String(v2PersistentIntervalReported.unsupportedWork.length),
    externalIo: String(v2PersistentIntervalReported.externalIo.length),
    evidenceProfile: v2PersistentIntervalEvidence.profile,
    sameControlledSession: true,
    exactBinaryLaunch: true,
    closeResponseAndEof: true,
  };
  v2InputMethodFocus = {
    profile: v2Session.profile,
    navigationBoundary: v2FocusNavigation.boundary,
    outcome: v2FocusSettled.outcome,
    producerPending: String(v2FocusSettled.snapshot.producers.pending),
    producerTerminal: v2FocusSettled.snapshot.producers.terminal,
    runtimeFailures: String(v2FocusSettled.snapshot.runtimeFailures.length),
    unsupportedWork: String(v2FocusSettled.unsupportedWork.length),
    externalIo: String(v2FocusSettled.externalIo.length),
    completionTrace: v2FocusTraceResult.value,
    evidenceProfile: v2FocusEvidence.profile,
    sameControlledSession: true,
    exactBinaryLaunch: true,
    closeResponseAndEof: true,
  };
  v2AutomationEventTimestamps = {
    profile: v2Session.profile,
    navigationBoundary: v2AutomationNavigation.boundary,
    initialOutcome: v2AutomationInitial.outcome,
    initialVirtualTimeNs: String(v2AutomationInitial.virtualTimeNs),
    advancedVirtualTimeNs: String(v2AutomationAdvanced.virtualTimeNs),
    dispatchedVirtualTimeNs: String(v2AutomationDispatched.virtualTimeNs),
    controlledEventCount: String(v2AutomationControlledEvents.length),
    controlledTrace: v2AutomationControlledTraceResult.value,
    browserEventCountAfterScriptProbe: String(
      v2AutomationScriptTraceParts[1]?.split(">").length,
    ),
    scriptCreatedConstructorCount: String(v2AutomationScriptTraceParts[2]?.split(",").length),
    scriptCreatedTrace: v2AutomationScriptTraceParts[2],
    rejectedOutcome: v2AutomationRejected.outcome,
    failureCode: v2AutomationRejected.failure?.code,
    unsupportedKind: v2AutomationUnsupported.kind,
    unsupportedCount: String(v2AutomationUnsupported.count),
    unsupportedReason: v2AutomationUnsupported.reason,
    unsupportedTimeSurface: v2AutomationUnsupported.timeSurface,
    evidenceProfile: v2AutomationEvidence.profile,
    sameControlledSession: true,
    exactBinaryLaunch: true,
    closeResponseAndEof: true,
  };

  // A controlled clock terminal is process-wide and sticky. The automation proof above
  // deliberately observes five script-created host timestamps, so the CSS constructor boundary
  // must use its own fresh exact-binary process. Its positive internal events and negative WebIDL
  // constructors still run in one CSS session.
  v2CssRuntimeWorkingDirectory = await mkdtemp(
    join(consumerRoot, ".stasis-v2-css-runtime-cwd-"),
  );
  const v2CssRuntimeWorkingDirectoryRealPath = await realpath(v2CssRuntimeWorkingDirectory);
  assert.equal(
    dirname(v2CssRuntimeWorkingDirectoryRealPath),
    consumerRootRealPath,
    "CSS v2 runtime cwd escaped the clean consumer root",
  );
  assert.notEqual(v2CssRuntimeWorkingDirectoryRealPath, runtimeWorkingDirectoryRealPath);
  assert.notEqual(v2CssRuntimeWorkingDirectoryRealPath, v2RuntimeWorkingDirectoryRealPath);
  assert.deepEqual(
    await readdir(v2CssRuntimeWorkingDirectoryRealPath),
    [],
    "CSS v2 runtime cwd must start empty",
  );
  v2CssRuntime = await sdk.launch({
    executablePath: explicitOverrideWrapper,
    runtimeCacheDirectory: explicitOverrideCacheDirectory,
    env: exactBinaryEnvironment,
    cwd: v2CssRuntimeWorkingDirectoryRealPath,
    closeTimeoutMs: 30_000,
    ...commandDeadline(),
  });
  const v2CssChildPid = v2CssRuntime.pid;
  assert.ok(
    Number.isSafeInteger(v2CssChildPid) && v2CssChildPid > 0,
    "SDK did not expose the CSS v2 child PID",
  );
  assert.equal(v2CssRuntime.info.implementation.version, expectedVersion);
  assert.deepEqual(v2CssRuntime.info.implementation.source, expectedSource);
  assert.ok(
    v2CssRuntime.info.capabilities.profiles.includes(CONTROLLED_WEB_SESSION_V2_PROFILE),
    "exact CSS proof runtime did not advertise controlled-web-session-v2",
  );
  assert.equal(
    await readFile(explicitOverrideMarker, "utf8"),
    `${explicitOverrideProof}\n${explicitOverrideProof}\n${explicitOverrideProof}\n`,
    "the packed-SDK CSS process did not launch through the exact-binary wrapper",
  );

  const v2CssFixtureUrl =
    "https://packed-sdk-css-animation-event-timestamps-v2.example.test/";
  v2CssSession = await v2CssRuntime.openSession(v2CssFixtureUrl, {
    profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
    clock: {
      mode: "controlled",
      initialVirtualTimeNs: 0n,
      unixTimeOriginNs: 0n,
    },
    network: {
      mode: "fixtures_only",
      routes: [
        {
          match: { method: "GET", url: { exact: v2CssFixtureUrl } },
          fulfill: {
            status: 200,
            headers: [["content-type", "text/html; charset=utf-8"]],
            body: { utf8: sessionV2CssAnimationEventFixtureBody },
          },
        },
      ],
    },
  });
  assert.equal(v2CssSession.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);
  const v2CssQualified = await v2CssSession.settle(
    v2CssSession.stateToken,
    { maxVirtualTimeNs: 0n },
    commandDeadline(),
  );
  assert.equal(v2CssQualified.outcome, "quiescent");
  assert.equal(v2CssQualified.virtualTimeNs, 5_000_000n);
  assert.deepEqual(v2CssQualified.unsupportedWork, []);
  assert.deepEqual(v2CssQualified.externalIo, []);
  assert.deepEqual(v2CssQualified.snapshot.runtimeFailures, []);
  const v2CssStarted = await v2CssSession.activate(
    "#start",
    v2CssQualified.stateToken,
    commandDeadline(),
  );
  const v2CssSettled = await v2CssSession.settle(
    v2CssStarted.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(v2CssSettled.outcome, "quiescent");
  assert.deepEqual(v2CssSettled.unsupportedWork, []);
  assert.deepEqual(v2CssSettled.externalIo, []);
  assert.deepEqual(v2CssSettled.snapshot.runtimeFailures, []);
  assert.equal(v2CssSettled.snapshot.producers.pending, 0n);
  assert.equal(v2CssSettled.snapshot.producers.terminal, false);
  assert.equal(v2CssSettled.snapshot.rendering.pendingAnimationEvents, 0n);
  assert.equal(v2CssSettled.snapshot.rendering.finiteAnimations, 0n);
  assert.equal(v2CssSettled.snapshot.rendering.persistentAnimations, 0n);
  assert.equal(v2CssSettled.snapshot.rendering.unsupportedAnimations, 0n);
  assert.ok(v2CssSettled.processed.renderingOpportunities > 0n);
  const v2CssControlledTraceResult = await v2CssSession.text(
    "#result",
    v2CssSettled.stateToken,
    commandDeadline(),
  );
  const [v2CssArmedTrace, v2CssControlledEventsTrace] =
    v2CssControlledTraceResult.value.split("|");
  assert.equal(v2CssArmedTrace, "armed:5");
  const v2CssControlledEvents = v2CssControlledEventsTrace?.split(">") ?? [];
  assert.equal(v2CssControlledEvents.length, 2);
  const v2CssAllowedKinds = new Set([
    "animationcancel",
    "animationend",
    "animationiteration",
    "animationstart",
  ]);
  const v2CssControlledKinds = [];
  const v2CssControlledDispatchTimes = new Set();
  for (const entry of v2CssControlledEvents) {
    const match =
      /^(animationcancel|animationend|animationiteration|animationstart):trusted:([^:]+):([^:]+):owned$/u.exec(
        entry,
      );
    assert.ok(match, `CSS event trace escaped the exact owned shape: ${entry}`);
    assert.equal(match[2], match[3], `CSS event did not use its dispatch document time: ${entry}`);
    assert.ok(Number(match[2]) >= 5, `CSS event preceded the admitted action: ${entry}`);
    v2CssControlledKinds.push(match[1]);
    v2CssControlledDispatchTimes.add(match[2]);
  }
  v2CssControlledKinds.sort();
  assert.equal(
    new Set(v2CssControlledKinds).size,
    v2CssControlledKinds.length,
    "CSS fixture emitted a duplicate internal event kind",
  );
  assert.deepEqual(v2CssControlledKinds, ["animationend", "animationstart"]);
  assert.ok(
    v2CssControlledKinds.every((kind) => v2CssAllowedKinds.has(kind)),
    "CSS fixture emitted an event outside the internal CSS event boundary",
  );
  assert.ok(
    v2CssControlledKinds.includes("animationstart") &&
      v2CssControlledKinds.includes("animationend"),
    "CSS fixture must prove both start and finite completion dispatch",
  );
  assert.ok(
    v2CssControlledDispatchTimes.size >= 1 &&
      v2CssControlledDispatchTimes.size <= v2CssControlledEvents.length,
    "CSS dispatch-time cardinality escaped the retained event set",
  );
  const v2CssEvidence = v2CssSession.settlementEvidence(v2CssSettled);
  assert.equal(v2CssEvidence.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);

  const v2CssPostReflowSessionBaselineVirtualTimeNs = v2CssSettled.virtualTimeNs;
  assert.equal(v2CssPostReflowSessionBaselineVirtualTimeNs, 20_000_000n);
  const v2CssPostReflowStarted = await v2CssSession.activate(
    "#post-reflow",
    v2CssControlledTraceResult.stateToken,
    commandDeadline(),
  );
  const v2CssPostReflowSettled = await v2CssSession.settle(
    v2CssPostReflowStarted.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(v2CssPostReflowSettled.outcome, "quiescent");
  const v2CssPostReflowElapsedVirtualTimeNs =
    v2CssPostReflowSettled.virtualTimeNs - v2CssPostReflowSessionBaselineVirtualTimeNs;
  assert.equal(v2CssPostReflowElapsedVirtualTimeNs, 70_000_000n);
  assert.equal(v2CssPostReflowSettled.virtualTimeNs, 90_000_000n);
  assert.deepEqual(v2CssPostReflowSettled.unsupportedWork, []);
  assert.deepEqual(v2CssPostReflowSettled.externalIo, []);
  assert.deepEqual(v2CssPostReflowSettled.snapshot.runtimeFailures, []);
  assert.equal(v2CssPostReflowSettled.snapshot.rendering.pendingAnimationEvents, 0n);
  assert.equal(v2CssPostReflowSettled.snapshot.rendering.nextOpportunityNs, undefined);
  assert.ok(v2CssPostReflowSettled.processed.renderingOpportunities > 0n);
  const v2CssPostReflowPending = await v2CssSession.pending(commandDeadline());
  assert.equal(
    v2CssPostReflowPending.virtualTimeNs,
    v2CssPostReflowSettled.virtualTimeNs,
  );
  assert.equal(v2CssPostReflowPending.rendering.pendingAnimationEvents, 0n);
  assert.equal(v2CssPostReflowPending.rendering.nextOpportunityNs, undefined);
  assert.deepEqual(v2CssPostReflowPending.runtimeFailures, []);
  assert.equal(
    v2CssPostReflowPending.stateToken,
    v2CssPostReflowSettled.stateToken,
    "passive pending observation changed the drained post-reflow CSS authority",
  );
  const v2CssPostReflowTraceResult = await v2CssSession.text(
    "#post-reflow-result",
    v2CssPostReflowSettled.stateToken,
    commandDeadline(),
  );
  assert.equal(
    v2CssPostReflowTraceResult.value,
    "armed:20|animationstart:trusted:70:70>animationcancel:trusted:90:90",
  );
  const v2CssPostReflowEvents =
    v2CssPostReflowTraceResult.value.split("|")[1]?.split(">") ?? [];
  assert.equal(v2CssPostReflowEvents.length, 2);
  const v2CssPostReflowKinds = [];
  for (const entry of v2CssPostReflowEvents) {
    const match = /^(animationcancel|animationstart):trusted:([^:]+):([^:]+)$/u.exec(entry);
    assert.ok(match, `post-reflow CSS event escaped the exact owned shape: ${entry}`);
    assert.equal(match[2], match[3], `post-reflow CSS event escaped document time: ${entry}`);
    v2CssPostReflowKinds.push(match[1]);
  }
  v2CssPostReflowKinds.sort();
  assert.deepEqual(v2CssPostReflowKinds, ["animationcancel", "animationstart"]);

  const v2CssScriptCreated = await v2CssSession.activate(
    "#script-created",
    v2CssPostReflowTraceResult.stateToken,
    commandDeadline(),
  );
  const v2CssScriptTraceResult = await v2CssSession.text(
    "#result",
    v2CssScriptCreated.stateToken,
    commandDeadline(),
  );
  const v2CssScriptEntries = v2CssScriptTraceResult.value.split("|")[1]?.split(">") ?? [];
  assert.equal(v2CssScriptEntries.length, v2CssControlledEvents.length + 1);
  assert.equal(v2CssScriptEntries.at(-1), "script:0,0");
  const v2CssRejected = await v2CssSession.settle(
    v2CssScriptTraceResult.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(v2CssRejected.outcome, "unsupported_work");
  assert.equal(v2CssRejected.failure?.code, "unsupported_clock_surface");
  assert.equal(v2CssRejected.unsupportedWork.length, 1);
  const [v2CssUnsupported] = v2CssRejected.unsupportedWork;
  assert.equal(v2CssUnsupported.kind, "other");
  assert.equal(v2CssUnsupported.count, 1n);
  assert.equal(v2CssUnsupported.reason, "time_surface");
  assert.equal(v2CssUnsupported.timeSurface, "host_timestamp");
  const v2CssRejectedEvidence = v2CssSession.settlementEvidence(v2CssRejected);
  assert.equal(v2CssRejectedEvidence.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);

  await v2CssSession.close(commandDeadline());
  await new Promise((resolveImmediate) => setImmediate(resolveImmediate));
  let v2CssProcessStillExists = true;
  try {
    process.kill(v2CssChildPid, 0);
  } catch (error) {
    if (error?.code !== "ESRCH") throw error;
    v2CssProcessStillExists = false;
  }
  assert.equal(
    v2CssProcessStillExists,
    false,
    "CSS v2 Stasis child still exists after close and EOF",
  );
  assert.deepEqual(
    await readdir(explicitOverrideCacheDirectory),
    [],
    "the CSS v2 explicit executable override unexpectedly accessed the managed runtime cache",
  );
  v2CssClosedCleanly = true;
  v2CssAnimationEventTimestamps = {
    profile: v2CssSession.profile,
    initialOutcome: v2CssQualified.outcome,
    settledVirtualTimeNs: String(v2CssQualified.virtualTimeNs),
    controlledOutcome: v2CssSettled.outcome,
    controlledEventCount: String(v2CssControlledEvents.length),
    controlledEventKinds: v2CssControlledKinds.join(","),
    controlledOwnedEventCount: String(
      v2CssControlledEvents.filter((entry) => entry.endsWith(":owned")).length,
    ),
    controlledDispatchTimeCount: String(v2CssControlledDispatchTimes.size),
    controlledRuntimeFailures: String(v2CssSettled.snapshot.runtimeFailures.length),
    controlledUnsupportedWork: String(v2CssSettled.unsupportedWork.length),
    controlledExternalIo: String(v2CssSettled.externalIo.length),
    pendingAnimationEvents: String(v2CssSettled.snapshot.rendering.pendingAnimationEvents),
    finiteAnimations: String(v2CssSettled.snapshot.rendering.finiteAnimations),
    infiniteAnimations: String(v2CssSettled.snapshot.rendering.persistentAnimations),
    unsupportedAnimations: String(v2CssSettled.snapshot.rendering.unsupportedAnimations),
    producerPending: String(v2CssSettled.snapshot.producers.pending),
    producerTerminal: v2CssSettled.snapshot.producers.terminal,
    processedRenderingOpportunities: String(v2CssSettled.processed.renderingOpportunities),
    postReflowOutcome: v2CssPostReflowSettled.outcome,
    postReflowVirtualTimeNs: String(v2CssPostReflowSettled.virtualTimeNs),
    postReflowTrace: v2CssPostReflowTraceResult.value,
    postReflowEventCount: String(v2CssPostReflowEvents.length),
    postReflowEventKinds: v2CssPostReflowKinds.join(","),
    postReflowRuntimeFailures: String(v2CssPostReflowSettled.snapshot.runtimeFailures.length),
    postReflowUnsupportedWork: String(v2CssPostReflowSettled.unsupportedWork.length),
    postReflowExternalIo: String(v2CssPostReflowSettled.externalIo.length),
    postReflowPendingAnimationEvents: String(
      v2CssPostReflowPending.rendering.pendingAnimationEvents,
    ),
    postReflowNextOpportunityNs:
      v2CssPostReflowPending.rendering.nextOpportunityNs === undefined
        ? "none"
        : String(v2CssPostReflowPending.rendering.nextOpportunityNs),
    postReflowProcessedRenderingOpportunities: String(
      v2CssPostReflowSettled.processed.renderingOpportunities,
    ),
    postReflowStateTokenPreserved:
      v2CssPostReflowPending.stateToken === v2CssPostReflowSettled.stateToken,
    postReflowOwnedQueueDrain: true,
    scriptCreatedConstructorCount: "2",
    scriptCreatedTrace: v2CssScriptEntries.at(-1),
    rejectedOutcome: v2CssRejected.outcome,
    failureCode: v2CssRejected.failure?.code,
    unsupportedKind: v2CssUnsupported.kind,
    unsupportedCount: String(v2CssUnsupported.count),
    unsupportedReason: v2CssUnsupported.reason,
    unsupportedTimeSurface: v2CssUnsupported.timeSurface,
    evidenceProfile: v2CssRejectedEvidence.profile,
    publicNonAuxiliaryControlledTarget: true,
    sameControlledSession: true,
    freshExactBinaryProcess: true,
    managedRuntimeFallbackAccesses: "0",
    exactBinaryLaunch: true,
    closeResponseAndEof: true,
  };

  const wrapperInvocationLinesBeforeCookieProof = (
    await readFile(explicitOverrideMarker, "utf8")
  )
    .trimEnd()
    .split("\n");
  assert.equal(
    wrapperInvocationLinesBeforeCookieProof.length,
    3,
    "pre-cookie exact-binary launch count changed",
  );

  v2CookieRuntimeWorkingDirectory = await mkdtemp(
    join(consumerRoot, ".stasis-v2-cookie-runtime-cwd-"),
  );
  const v2CookieRuntimeWorkingDirectoryRealPath = await realpath(
    v2CookieRuntimeWorkingDirectory,
  );
  assert.equal(
    dirname(v2CookieRuntimeWorkingDirectoryRealPath),
    consumerRootRealPath,
    "cookie v2 runtime cwd escaped the clean consumer root",
  );
  assert.deepEqual(
    await readdir(v2CookieRuntimeWorkingDirectoryRealPath),
    [],
    "cookie v2 runtime cwd must start empty",
  );
  v2CookieRuntime = await sdk.launch({
    executablePath: explicitOverrideWrapper,
    runtimeCacheDirectory: explicitOverrideCacheDirectory,
    env: exactBinaryEnvironment,
    cwd: v2CookieRuntimeWorkingDirectoryRealPath,
    closeTimeoutMs: 30_000,
    ...commandDeadline(),
  });
  const v2CookieChildPid = v2CookieRuntime.pid;
  assert.ok(Number.isSafeInteger(v2CookieChildPid) && v2CookieChildPid > 0);
  assert.equal(v2CookieRuntime.info.implementation.version, expectedVersion);
  assert.deepEqual(v2CookieRuntime.info.implementation.source, expectedSource);
  assert.ok(
    v2CookieRuntime.info.capabilities.profiles.includes(CONTROLLED_WEB_SESSION_V2_PROFILE),
    "exact cookie proof runtime did not advertise controlled-web-session-v2",
  );
  assert.equal(
    await readFile(explicitOverrideMarker, "utf8"),
    `${explicitOverrideProof}\n${explicitOverrideProof}\n${explicitOverrideProof}\n${explicitOverrideProof}\n`,
    "the packed-SDK cookie process did not launch through the exact-binary wrapper",
  );

  const loginUrl = `${cookieMainUrl}/login`;
  v2CookieSessionHandle = await v2CookieRuntime.openSession(loginUrl, {
    profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
    clock: {
      mode: "controlled",
      initialVirtualTimeNs: 0n,
      unixTimeOriginNs: 0n,
    },
    network: { mode: "live", routes: [] },
    ...commandDeadline(),
  });
  const cookieLoginReady = await v2CookieSessionHandle.settle(
    v2CookieSessionHandle.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(cookieLoginReady.outcome, "quiescent");
  const cookieSubmitted = await v2CookieSessionHandle.submit(
    "#login-form",
    cookieLoginReady.stateToken,
    commandDeadline(),
  );
  assert.equal(cookieSubmitted.submitted, true);
  const cookieAuthenticated = await v2CookieSessionHandle.settle(
    cookieSubmitted.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(cookieAuthenticated.outcome, "quiescent");
  assert.equal(
    (await v2CookieSessionHandle.text(
      "#authenticated",
      cookieAuthenticated.stateToken,
      commandDeadline(),
    )).value,
    "remembered",
  );
  const cookieSnapshot = await v2CookieSessionHandle.getCookies(commandDeadline());
  const rememberedCookie = cookieSnapshot.cookies.find(
    (cookie) => cookie.name === "remember_me",
  );
  assert.ok(rememberedCookie, "v2 cookie snapshot omitted the persistent response cookie");
  const expectedRememberedExpiryNs = 2_592_000_000_000_000n;
  assert.equal(rememberedCookie.expiresUnixTimeNs, expectedRememberedExpiryNs);
  assert.equal(rememberedCookie.sameSite, "lax");
  assert.equal(rememberedCookie.httpOnly, true);

  const crossSiteCookieExpiryNs = 34_560_000_000_000_000n;
  const cookieMutation = await v2CookieSessionHandle.setCookies(
    [
      ...cookieSnapshot.cookies,
      {
        name: "cross_lax",
        value: "must-not-cross",
        domain: "localhost",
        path: "/",
        hostOnly: true,
        secure: false,
        httpOnly: true,
        sameSite: "lax",
        expiresUnixTimeNs: crossSiteCookieExpiryNs,
        partitioned: false,
        creationSequence: 100n,
        lastAccessSequence: 101n,
      },
    ],
    cookieSnapshot.sessionStateToken,
    commandDeadline(),
  );
  assert.notEqual(cookieMutation.sessionStateToken, cookieSnapshot.sessionStateToken);
  const portableV2State = await v2CookieSessionHandle.exportState(commandDeadline());
  assert.equal(portableV2State.state.schemaVersion, 1);
  assert.equal(portableV2State.state.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);
  const portableRememberedCookie = portableV2State.state.cookies.find(
    (cookie) => cookie.name === "remember_me",
  );
  assert.ok(portableRememberedCookie, "portable v2 state omitted the response cookie");
  assert.equal(portableRememberedCookie.expiresUnixTimeNs, expectedRememberedExpiryNs);
  const portableCrossSiteCookie = portableV2State.state.cookies.find(
    (cookie) => cookie.name === "cross_lax",
  );
  assert.ok(portableCrossSiteCookie, "portable v2 state omitted the cross-site Lax control");
  assert.equal(
    portableCrossSiteCookie.domain,
    "localhost",
    "portable v2 state must retain the canonical localhost cookie domain",
  );
  assert.equal(portableCrossSiteCookie.expiresUnixTimeNs, crossSiteCookieExpiryNs);
  assert.equal(
    portableRememberedCookie.lastAccessSequence,
    0n,
    "portable response cookie must precede the newly inserted cross-site control",
  );
  assert.equal(
    portableCrossSiteCookie.lastAccessSequence,
    1n,
    "portable cross-site control must begin as the most recently accessed cookie",
  );

  await v2CookieSessionHandle.close(commandDeadline());
  await new Promise((resolveImmediate) => setImmediate(resolveImmediate));
  let v2CookieProcessStillExists = true;
  try {
    process.kill(v2CookieChildPid, 0);
  } catch (error) {
    if (error?.code !== "ESRCH") throw error;
    v2CookieProcessStillExists = false;
  }
  assert.equal(v2CookieProcessStillExists, false, "v2 cookie child still exists after close");
  assert.deepEqual(
    await readdir(v2CookieRuntimeWorkingDirectoryRealPath),
    [],
    "cookie v2 runtime cwd must remain empty after the controlled session closes",
  );
  v2CookieClosedCleanly = true;

  v2CookieRestoreRuntimeWorkingDirectory = await mkdtemp(
    join(consumerRoot, ".stasis-v2-cookie-restore-runtime-cwd-"),
  );
  const v2CookieRestoreRuntimeWorkingDirectoryRealPath = await realpath(
    v2CookieRestoreRuntimeWorkingDirectory,
  );
  assert.equal(
    dirname(v2CookieRestoreRuntimeWorkingDirectoryRealPath),
    consumerRootRealPath,
    "restored cookie v2 runtime cwd escaped the clean consumer root",
  );
  assert.deepEqual(
    await readdir(v2CookieRestoreRuntimeWorkingDirectoryRealPath),
    [],
    "restored cookie v2 runtime cwd must start empty",
  );
  v2CookieRestoreRuntime = await sdk.launch({
    executablePath: explicitOverrideWrapper,
    runtimeCacheDirectory: explicitOverrideCacheDirectory,
    env: exactBinaryEnvironment,
    cwd: v2CookieRestoreRuntimeWorkingDirectoryRealPath,
    closeTimeoutMs: 30_000,
    ...commandDeadline(),
  });
  const v2CookieRestoreChildPid = v2CookieRestoreRuntime.pid;
  assert.ok(Number.isSafeInteger(v2CookieRestoreChildPid) && v2CookieRestoreChildPid > 0);
  assert.equal(v2CookieRestoreRuntime.info.implementation.version, expectedVersion);
  assert.deepEqual(v2CookieRestoreRuntime.info.implementation.source, expectedSource);
  assert.equal(
    await readFile(explicitOverrideMarker, "utf8"),
    `${explicitOverrideProof}\n${explicitOverrideProof}\n${explicitOverrideProof}\n${explicitOverrideProof}\n${explicitOverrideProof}\n`,
    "the packed-SDK restored-cookie process did not launch through the exact-binary wrapper",
  );
  v2CookieRestoreSession = await v2CookieRestoreRuntime.openSession(
    `${cookieMainUrl}/restored`,
    {
      profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
      state: portableV2State.state,
      clock: {
        mode: "controlled",
        initialVirtualTimeNs: 0n,
        unixTimeOriginNs: 0n,
      },
      network: { mode: "live", routes: [] },
      ...commandDeadline(),
    },
  );
  const restoredCookieSettled = await v2CookieRestoreSession.settle(
    v2CookieRestoreSession.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(restoredCookieSettled.outcome, "quiescent");
  assert.equal(
    (await v2CookieRestoreSession.text(
      "#cross-site-result",
      restoredCookieSettled.stateToken,
      commandDeadline(),
    )).value,
    "loaded-without-cookie",
  );
  const restoredMainRequest = cookieMainRequests.find(
    (request) => request.method === "GET" && request.url === "/restored",
  );
  assert.ok(restoredMainRequest, "restored v2 request did not reach the same-site server");
  const restoredSameSiteCookieSent =
    /(?:^|;\s*)remember_me=controlled(?:;|$)/u.test(restoredMainRequest.cookie);
  assert.equal(restoredSameSiteCookieSent, true);
  assert.equal(cookieCrossRequests.length, 1);
  assert.deepEqual(cookieCrossRequests[0], {
    method: "GET",
    url: "/probe.js",
    host: new URL(cookieCrossUrl).host,
    cookie: "",
  });
  const restoredCookieState = await v2CookieRestoreSession.exportState(commandDeadline());
  assert.equal(restoredCookieState.state.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);
  const restoredRememberedCookie = restoredCookieState.state.cookies.find(
    (cookie) => cookie.name === "remember_me",
  );
  assert.ok(restoredRememberedCookie, "restored v2 state omitted the response cookie");
  assert.equal(restoredRememberedCookie.expiresUnixTimeNs, expectedRememberedExpiryNs);
  const restoredCrossSiteCookie = restoredCookieState.state.cookies.find(
    (cookie) => cookie.name === "cross_lax",
  );
  assert.ok(restoredCrossSiteCookie, "restored v2 state omitted the cross-site Lax control");
  assert.equal(restoredCrossSiteCookie.domain, "localhost");
  const stableCookieStateRetained = isDeepStrictEqual(
    cookieStatesWithoutLastAccessSequence(restoredCookieState.state.cookies),
    cookieStatesWithoutLastAccessSequence(portableV2State.state.cookies),
  );
  assert.equal(
    stableCookieStateRetained,
    true,
    "request selection must retain every imported cookie field except operational access order",
  );
  assert.equal(
    restoredCrossSiteCookie.lastAccessSequence,
    0n,
    "the filtered cross-site control must remain older after dense access-order export",
  );
  assert.equal(
    restoredRememberedCookie.lastAccessSequence,
    1n,
    "the same-site response cookie must become newest after request selection",
  );
  const crossSiteLaxCookieFiltered =
    cookieCrossRequests[0].cookie === "" &&
    stableCookieStateRetained &&
    restoredCrossSiteCookie.lastAccessSequence < restoredRememberedCookie.lastAccessSequence;
  assert.equal(crossSiteLaxCookieFiltered, true);
  const v2CookieEvidence = v2CookieRestoreSession.settlementEvidence(
    restoredCookieSettled,
  );
  assert.equal(v2CookieEvidence.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);

  await v2CookieRestoreSession.close(commandDeadline());
  await new Promise((resolveImmediate) => setImmediate(resolveImmediate));
  let v2CookieRestoreProcessStillExists = true;
  try {
    process.kill(v2CookieRestoreChildPid, 0);
  } catch (error) {
    if (error?.code !== "ESRCH") throw error;
    v2CookieRestoreProcessStillExists = false;
  }
  assert.equal(
    v2CookieRestoreProcessStillExists,
    false,
    "restored v2 cookie child still exists after close",
  );
  assert.deepEqual(
    await readdir(explicitOverrideCacheDirectory),
    [],
    "the v2 cookie proof unexpectedly accessed the managed runtime cache",
  );
  assert.deepEqual(
    await readdir(v2CookieRestoreRuntimeWorkingDirectoryRealPath),
    [],
    "restored cookie v2 runtime cwd must remain empty after the controlled session closes",
  );
  v2CookieRestoreClosedCleanly = true;

  v2CookieNoImportRuntimeWorkingDirectory = await mkdtemp(
    join(consumerRoot, ".stasis-v2-cookie-no-import-runtime-cwd-"),
  );
  const v2CookieNoImportRuntimeWorkingDirectoryRealPath = await realpath(
    v2CookieNoImportRuntimeWorkingDirectory,
  );
  assert.equal(
    dirname(v2CookieNoImportRuntimeWorkingDirectoryRealPath),
    consumerRootRealPath,
    "no-import cookie v2 runtime cwd escaped the clean consumer root",
  );
  assert.notEqual(
    v2CookieNoImportRuntimeWorkingDirectoryRealPath,
    v2CookieRuntimeWorkingDirectoryRealPath,
  );
  assert.notEqual(
    v2CookieNoImportRuntimeWorkingDirectoryRealPath,
    v2CookieRestoreRuntimeWorkingDirectoryRealPath,
  );
  assert.deepEqual(
    await readdir(v2CookieNoImportRuntimeWorkingDirectoryRealPath),
    [],
    "no-import cookie v2 runtime cwd must start empty",
  );
  v2CookieNoImportRuntime = await sdk.launch({
    executablePath: explicitOverrideWrapper,
    runtimeCacheDirectory: explicitOverrideCacheDirectory,
    env: exactBinaryEnvironment,
    cwd: v2CookieNoImportRuntimeWorkingDirectoryRealPath,
    closeTimeoutMs: 30_000,
    ...commandDeadline(),
  });
  const v2CookieNoImportChildPid = v2CookieNoImportRuntime.pid;
  assert.ok(Number.isSafeInteger(v2CookieNoImportChildPid) && v2CookieNoImportChildPid > 0);
  assert.equal(v2CookieNoImportRuntime.info.implementation.version, expectedVersion);
  assert.deepEqual(v2CookieNoImportRuntime.info.implementation.source, expectedSource);
  assert.equal(
    await readFile(explicitOverrideMarker, "utf8"),
    `${explicitOverrideProof}\n`.repeat(6),
    "the packed-SDK no-import cookie process did not launch through the exact-binary wrapper",
  );
  v2CookieNoImportSession = await v2CookieNoImportRuntime.openSession(
    `${cookieMainUrl}/fresh-no-import`,
    {
      profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
      clock: {
        mode: "controlled",
        initialVirtualTimeNs: 0n,
        unixTimeOriginNs: 0n,
      },
      network: { mode: "live", routes: [] },
      ...commandDeadline(),
    },
  );
  const noImportSettled = await v2CookieNoImportSession.settle(
    v2CookieNoImportSession.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(noImportSettled.outcome, "quiescent");
  assert.equal(
    (await v2CookieNoImportSession.text(
      "#fresh-no-import",
      noImportSettled.stateToken,
      commandDeadline(),
    )).value,
    "fresh",
  );
  const noImportMainRequests = cookieMainRequests.filter(
    (request) => request.method === "GET" && request.url === "/fresh-no-import",
  );
  assert.deepEqual(noImportMainRequests, [
    { method: "GET", url: "/fresh-no-import", cookie: "" },
  ]);
  const noImportCookieSnapshot = await v2CookieNoImportSession.getCookies(commandDeadline());
  assert.deepEqual(noImportCookieSnapshot.cookies, []);
  const noImportState = await v2CookieNoImportSession.exportState(commandDeadline());
  assert.equal(noImportState.state.profile, CONTROLLED_WEB_SESSION_V2_PROFILE);
  assert.deepEqual(noImportState.state.cookies, []);

  await v2CookieNoImportSession.close(commandDeadline());
  await new Promise((resolveImmediate) => setImmediate(resolveImmediate));
  let v2CookieNoImportProcessStillExists = true;
  try {
    process.kill(v2CookieNoImportChildPid, 0);
  } catch (error) {
    if (error?.code !== "ESRCH") throw error;
    v2CookieNoImportProcessStillExists = false;
  }
  assert.equal(
    v2CookieNoImportProcessStillExists,
    false,
    "no-import v2 cookie child still exists after close",
  );
  assert.deepEqual(
    await readdir(v2CookieNoImportRuntimeWorkingDirectoryRealPath),
    [],
    "no-import cookie v2 runtime cwd must remain empty after the controlled session closes",
  );
  v2CookieNoImportClosedCleanly = true;

  v2CookieTimeRangeRuntimeWorkingDirectory = await mkdtemp(
    join(consumerRoot, ".stasis-v2-cookie-time-range-runtime-cwd-"),
  );
  const v2CookieTimeRangeRuntimeWorkingDirectoryRealPath = await realpath(
    v2CookieTimeRangeRuntimeWorkingDirectory,
  );
  assert.equal(
    dirname(v2CookieTimeRangeRuntimeWorkingDirectoryRealPath),
    consumerRootRealPath,
    "cookie time-range runtime cwd escaped the clean consumer root",
  );
  assert.deepEqual(
    await readdir(v2CookieTimeRangeRuntimeWorkingDirectoryRealPath),
    [],
    "cookie time-range runtime cwd must start empty",
  );
  v2CookieTimeRangeRuntime = await sdk.launch({
    executablePath: explicitOverrideWrapper,
    runtimeCacheDirectory: explicitOverrideCacheDirectory,
    env: exactBinaryEnvironment,
    cwd: v2CookieTimeRangeRuntimeWorkingDirectoryRealPath,
    closeTimeoutMs: 30_000,
    ...commandDeadline(),
  });
  const v2CookieTimeRangeChildPid = v2CookieTimeRangeRuntime.pid;
  assert.ok(Number.isSafeInteger(v2CookieTimeRangeChildPid) && v2CookieTimeRangeChildPid > 0);
  assert.equal(v2CookieTimeRangeRuntime.info.implementation.version, expectedVersion);
  assert.deepEqual(v2CookieTimeRangeRuntime.info.implementation.source, expectedSource);
  assert.equal(
    await readFile(explicitOverrideMarker, "utf8"),
    `${explicitOverrideProof}\n`.repeat(7),
    "the packed-SDK cookie time-range process did not launch through the exact-binary wrapper",
  );
  const requestsBeforeTimeRangeArm = cookieMainRequests.length;
  let cookieTimeRangeError;
  v2CookieTimeRangeSession = await v2CookieTimeRangeRuntime.openSession(
    `${cookieMainUrl}/time-range-arm`,
    {
      profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
      clock: {
        mode: "controlled",
        initialVirtualTimeNs: MAX_U64_VIRTUAL_TIME_NS - 1n,
        unixTimeOriginNs: 0n,
      },
      network: { mode: "live", routes: [] },
      ...commandDeadline(),
    },
  );
  assert.equal(cookieMainRequests.length, requestsBeforeTimeRangeArm + 1);
  assert.deepEqual(cookieMainRequests.at(-1), {
    method: "GET",
    url: "/time-range-arm",
    cookie: "",
  });
  const timeRangeScheduled = await v2CookieTimeRangeSession.activate(
    "#time-range-advance",
    v2CookieTimeRangeSession.stateToken,
    commandDeadline(),
  );
  const timeRangeArmed = await v2CookieTimeRangeSession.settle(
    timeRangeScheduled.stateToken,
    {},
    commandDeadline(),
  );
  assert.equal(timeRangeArmed.outcome, "quiescent");
  assert.ok(timeRangeArmed.virtualTimeNs > MAX_U64_VIRTUAL_TIME_NS);
  assert.equal(
    (await v2CookieTimeRangeSession.text(
      "#time-range-arm",
      timeRangeArmed.stateToken,
      commandDeadline(),
    )).value,
    "advanced",
  );
  const requestsBeforeTimeRangeNavigation = cookieMainRequests.length;
  await assert.rejects(
    v2CookieTimeRangeSession.navigate(
      `${cookieMainUrl}/time-range`,
      timeRangeArmed.stateToken,
      commandDeadline(),
    ),
    (error) => {
      cookieTimeRangeError = error;
      assert.ok(error instanceof sdk.StasisProtocolError);
      assert.equal(error.code, "unsupported_cookie_time_range");
      assert.equal(error.fatal, false);
      assert.equal(error.stateEffect, "partial");
      return true;
    },
  );
  assert.equal(
    cookieMainRequests.length,
    requestsBeforeTimeRangeNavigation,
    "post-open cookie time-range terminal must occur before the request reaches the server",
  );
  await v2CookieTimeRangeSession.close(commandDeadline());
  await new Promise((resolveImmediate) => setImmediate(resolveImmediate));
  let v2CookieTimeRangeProcessStillExists = true;
  try {
    process.kill(v2CookieTimeRangeChildPid, 0);
  } catch (error) {
    if (error?.code !== "ESRCH") throw error;
    v2CookieTimeRangeProcessStillExists = false;
  }
  assert.equal(
    v2CookieTimeRangeProcessStillExists,
    false,
    "cookie time-range child still exists after session close",
  );
  assert.deepEqual(
    await readdir(v2CookieTimeRangeRuntimeWorkingDirectoryRealPath),
    [],
    "cookie time-range runtime cwd must remain empty after the controlled session closes",
  );
  v2CookieTimeRangeClosed = true;
  const finalManagedRuntimeCacheEntries = await readdir(explicitOverrideCacheDirectory);
  assert.deepEqual(
    finalManagedRuntimeCacheEntries,
    [],
    "the complete v2 cookie proof unexpectedly accessed the managed runtime cache",
  );
  const cookieWorkingDirectoriesRemainEmpty = (
    await Promise.all([
      readdir(v2CookieRuntimeWorkingDirectoryRealPath),
      readdir(v2CookieRestoreRuntimeWorkingDirectoryRealPath),
      readdir(v2CookieNoImportRuntimeWorkingDirectoryRealPath),
    ])
  ).every((entries) => entries.length === 0);
  const noImportControlSameHostContext =
    exactBinaryEnvironment.HOME === process.env.HOME;
  const memoryOnlyExplicitStatePortability =
    restoredSameSiteCookieSent &&
    noImportState.state.cookies.length === 0 &&
    noImportMainRequests[0].cookie === "" &&
    noImportControlSameHostContext &&
    cookieWorkingDirectoriesRemainEmpty;
  assert.equal(memoryOnlyExplicitStatePortability, true);
  const exactBinaryEnvironmentIsAllowlisted = Object.keys(exactBinaryEnvironment).every(
    (name) =>
      EXACT_BINARY_INHERITED_ENVIRONMENT.includes(name) ||
      EXACT_BINARY_LOCALE_ENVIRONMENT.includes(name) ||
      EXACT_BINARY_OVERRIDE_ENVIRONMENT.includes(name),
  );
  assert.equal(exactBinaryEnvironmentIsAllowlisted, true);
  const wrapperInvocationLines = (await readFile(explicitOverrideMarker, "utf8"))
    .trimEnd()
    .split("\n");
  const exactBinaryLaunch =
    wrapperInvocationLines.length === 7 &&
    wrapperInvocationLines.every((line) => line === explicitOverrideProof);
  assert.equal(exactBinaryLaunch, true);
  const gracefulCookieSessionProcesses = [
    v2CookieClosedCleanly,
    v2CookieRestoreClosedCleanly,
    v2CookieNoImportClosedCleanly,
    v2CookieTimeRangeClosed,
  ].filter(Boolean).length;
  const cookieCloseResponseAndEof = gracefulCookieSessionProcesses === 4;
  assert.equal(cookieCloseResponseAndEof, true);
  v2CookieSession = {
    profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
    stateSchemaVersion: "1",
    stateProfile: portableV2State.state.profile,
    responseCookieName: rememberedCookie.name,
    responseCookieExpiryUnixTimeNs: String(rememberedCookie.expiresUnixTimeNs),
    maxAgePrecedenceOverPastExpires:
      rememberedCookie.expiresUnixTimeNs === expectedRememberedExpiryNs,
    restoredSameSiteCookieSent,
    crossSiteResourceReachedServer: cookieCrossRequests.length === 1,
    crossSiteLaxCookieFiltered,
    crossSiteRequestMethod: cookieCrossRequests[0].method,
    crossSiteRequestPath: cookieCrossRequests[0].url,
    evidenceProfile: v2CookieEvidence.profile,
    memoryOnlyExplicitStatePortability,
    noImportControlCookieCount: String(noImportState.state.cookies.length),
    noImportControlRequestCookieHeaderEmpty: noImportMainRequests[0].cookie === "",
    noImportControlSameHostContext,
    cookieTimeRangeFailureCode: cookieTimeRangeError.code,
    cookieTimeRangeFatal: cookieTimeRangeError.fatal,
    cookieTimeRangeStateEffect: cookieTimeRangeError.stateEffect,
    cookieTimeRangeRequestReachedServer:
      cookieMainRequests.length !== requestsBeforeTimeRangeNavigation,
    credentialEnvironmentMode: exactBinaryEnvironmentIsAllowlisted
      ? "explicit_allowlist"
      : "invalid",
    freshExactBinaryProcesses: String(
      wrapperInvocationLines.length - wrapperInvocationLinesBeforeCookieProof.length,
    ),
    gracefulCookieSessionProcesses: String(gracefulCookieSessionProcesses),
    managedRuntimeFallbackAccesses: String(finalManagedRuntimeCacheEntries.length),
    exactBinaryLaunch,
    closeResponseAndEof: cookieCloseResponseAndEof,
  };

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
      v2MessageChannel,
      v2DirectDataSvg,
      v2InlineSvgRendering,
      v2SettlementUrl,
      v2PersistentIntervalProgression,
      v2InputMethodFocus,
      v2AutomationEventTimestamps,
      v2CssAnimationEventTimestamps,
      v2CookieSession,
    })}\n`,
  );
} finally {
  if (!closedCleanly && runtime !== undefined) {
    await runtime.close().catch(() => undefined);
  }
  if (!v2ClosedCleanly && v2Session !== undefined) {
    await v2Session.close().catch(() => undefined);
  }
  if (!v2ClosedCleanly && v2Runtime !== undefined) {
    await v2Runtime.close().catch(() => undefined);
  }
  if (!v2CssClosedCleanly && v2CssSession !== undefined) {
    await v2CssSession.close().catch(() => undefined);
  }
  if (!v2CssClosedCleanly && v2CssRuntime !== undefined) {
    await v2CssRuntime.close().catch(() => undefined);
  }
  if (!v2CookieClosedCleanly && v2CookieSessionHandle !== undefined) {
    await v2CookieSessionHandle.close().catch(() => undefined);
  }
  if (!v2CookieClosedCleanly && v2CookieRuntime !== undefined) {
    await v2CookieRuntime.close().catch(() => undefined);
  }
  if (!v2CookieRestoreClosedCleanly && v2CookieRestoreSession !== undefined) {
    await v2CookieRestoreSession.close().catch(() => undefined);
  }
  if (!v2CookieRestoreClosedCleanly && v2CookieRestoreRuntime !== undefined) {
    await v2CookieRestoreRuntime.close().catch(() => undefined);
  }
  if (!v2CookieNoImportClosedCleanly && v2CookieNoImportSession !== undefined) {
    await v2CookieNoImportSession.close().catch(() => undefined);
  }
  if (!v2CookieNoImportClosedCleanly && v2CookieNoImportRuntime !== undefined) {
    await v2CookieNoImportRuntime.close().catch(() => undefined);
  }
  if (!v2CookieTimeRangeClosed && v2CookieTimeRangeSession !== undefined) {
    await v2CookieTimeRangeSession.close().catch(() => undefined);
  }
  if (!v2CookieTimeRangeClosed && v2CookieTimeRangeRuntime !== undefined) {
    await v2CookieTimeRangeRuntime.close().catch(() => undefined);
  }
  await closeServer(server);
  await closeServer(cookieMainServer);
  await closeServer(cookieCrossIpv4Server);
  await closeServer(cookieCrossIpv6Server);
  if (runtimeWorkingDirectory !== undefined) {
    await rm(runtimeWorkingDirectory, { recursive: true });
  }
  if (v2RuntimeWorkingDirectory !== undefined) {
    await rm(v2RuntimeWorkingDirectory, { recursive: true });
  }
  if (v2CssRuntimeWorkingDirectory !== undefined) {
    await rm(v2CssRuntimeWorkingDirectory, { recursive: true });
  }
  if (v2CookieRuntimeWorkingDirectory !== undefined) {
    await rm(v2CookieRuntimeWorkingDirectory, { recursive: true });
  }
  if (v2CookieRestoreRuntimeWorkingDirectory !== undefined) {
    await rm(v2CookieRestoreRuntimeWorkingDirectory, { recursive: true });
  }
  if (v2CookieNoImportRuntimeWorkingDirectory !== undefined) {
    await rm(v2CookieNoImportRuntimeWorkingDirectory, { recursive: true });
  }
  if (v2CookieTimeRangeRuntimeWorkingDirectory !== undefined) {
    await rm(v2CookieTimeRangeRuntimeWorkingDirectory, { recursive: true });
  }
  if (explicitOverrideCacheDirectory !== undefined) {
    await rm(explicitOverrideCacheDirectory, { recursive: true });
  }
  if (explicitOverrideProbeDirectory !== undefined) {
    await rm(explicitOverrideProbeDirectory, { recursive: true });
  }
}
