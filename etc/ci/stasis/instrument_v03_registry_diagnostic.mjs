#!/usr/bin/env node

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile, realpath, writeFile } from "node:fs/promises";
import { basename, dirname, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

export const EXACT_V03_VERIFIER_SHA256 =
  "86aa09719204ad917471475bdc0d0d1adb28398d829f0e2d6eed36fc0e5f2add";
export const DIAGNOSTIC_SIGNAL_TIMEOUT_MS = 60_000;
export const DIAGNOSTIC_SDK_TIMEOUT_MS = 45_000;
const DIAGNOSTIC_SIGNAL_TIMEOUT_LITERAL = "60_000";
const DIAGNOSTIC_SDK_TIMEOUT_LITERAL = "45_000";

const DIAGNOSTIC_SUPPORT = `
const STASIS_V03_DIAGNOSTIC_SCHEMA = "stasis-v0.3-macos-public-diagnostic-v1";
const stasisV03DiagnosticSample = process.env.STASIS_V03_DIAGNOSTIC_SAMPLE;
assert.match(
  stasisV03DiagnosticSample ?? "",
  /^[1-9][0-9]*$/u,
  "STASIS_V03_DIAGNOSTIC_SAMPLE must be a canonical positive integer",
);

function stasisV03DiagnosticRecord(kind, phase, processOrdinal, error = undefined) {
  const record = {
    schema: STASIS_V03_DIAGNOSTIC_SCHEMA,
    kind,
    phase,
    sample: stasisV03DiagnosticSample,
    processOrdinal,
    expectedMethod: "runtime.settle",
    expectedRequestId: "5",
  };
  if (error !== undefined) {
    record.errorName = typeof error?.name === "string" ? error.name : null;
    record.code = typeof error?.code === "string" ? error.code : null;
    record.fatal = typeof error?.fatal === "boolean" ? error.fatal : null;
    record.stateEffect = typeof error?.stateEffect === "string" ? error.stateEffect : null;
    record.method = typeof error?.method === "string" ? error.method : null;
    record.requestId = typeof error?.requestId === "string" ? error.requestId : null;
    record.reasonName = typeof error?.reason?.name === "string" ? error.reason.name : null;
    record.stderrTailBytes =
      typeof error?.stderrTail === "string" ? Buffer.byteLength(error.stderrTail, "utf8") : null;
  }
  process.stderr.write(\`\${JSON.stringify(record)}\\n\`);
}
`;

const CSS_ANCHOR = `  const v2CssSettled = await v2CssSession.settle(
    v2CssStarted.stateToken,
    {},
    commandDeadline(),
  );`;

const CSS_REPLACEMENT = `  stasisV03DiagnosticRecord("begin", "css-start-settle", 3);
  let v2CssSettled;
  try {
    v2CssSettled = await v2CssSession.settle(
      v2CssStarted.stateToken,
      {},
      commandDeadline(),
    );
    stasisV03DiagnosticRecord("end", "css-start-settle", 3);
  } catch (error) {
    stasisV03DiagnosticRecord("error", "css-start-settle", 3, error);
    throw error;
  }`;

const COOKIE_ANCHOR = `  const cookieAuthenticated = await v2CookieSessionHandle.settle(
    cookieSubmitted.stateToken,
    {},
    commandDeadline(),
  );`;

const COOKIE_REPLACEMENT = `  stasisV03DiagnosticRecord("begin", "cookie-submit-settle", 4);
  let cookieAuthenticated;
  try {
    cookieAuthenticated = await v2CookieSessionHandle.settle(
      cookieSubmitted.stateToken,
      {},
      commandDeadline(),
    );
    stasisV03DiagnosticRecord("end", "cookie-submit-settle", 4);
  } catch (error) {
    stasisV03DiagnosticRecord("error", "cookie-submit-settle", 4, error);
    throw error;
  }`;

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function replaceExactlyOnce(source, anchor, replacement, label) {
  const first = source.indexOf(anchor);
  assert.notEqual(first, -1, `${label} anchor is absent from the exact v0.3.0 verifier`);
  assert.equal(
    source.indexOf(anchor, first + anchor.length),
    -1,
    `${label} anchor is not unique in the exact v0.3.0 verifier`,
  );
  return source.replace(anchor, replacement);
}

function replaceExactly(source, anchor, replacement, expectedCount, label) {
  const count = source.split(anchor).length - 1;
  assert.equal(
    count,
    expectedCount,
    `${label} anchor count changed in the exact v0.3.0 verifier`,
  );
  return source.replaceAll(anchor, replacement);
}

export function instrumentExactV03Verifier(source) {
  assert.equal(
    Number(DIAGNOSTIC_SIGNAL_TIMEOUT_LITERAL.replace("_", "")),
    DIAGNOSTIC_SIGNAL_TIMEOUT_MS,
  );
  assert.equal(
    Number(DIAGNOSTIC_SDK_TIMEOUT_LITERAL.replace("_", "")),
    DIAGNOSTIC_SDK_TIMEOUT_MS,
  );
  assert.equal(
    sha256(Buffer.from(source, "utf8")),
    EXACT_V03_VERIFIER_SHA256,
    "diagnostic input is not the immutable v0.3.0 registry verifier",
  );
  const deadlineAnchor =
    "const commandDeadline = () => ({ signal: AbortSignal.timeout(30_000) });";
  let instrumented = replaceExactlyOnce(
    source,
    deadlineAnchor,
    `const commandDeadline = () => ({ signal: AbortSignal.timeout(${DIAGNOSTIC_SIGNAL_TIMEOUT_LITERAL}) });\n${DIAGNOSTIC_SUPPORT}`,
    "diagnostic outer signal deadline",
  );
  const launchDeadlineAnchor = `    closeTimeoutMs: 30_000,
    ...commandDeadline(),`;
  instrumented = replaceExactly(
    instrumented,
    launchDeadlineAnchor,
    `    closeTimeoutMs: 30_000,
    commandTimeoutMs: ${DIAGNOSTIC_SDK_TIMEOUT_LITERAL},
    ...commandDeadline(),`,
    7,
    "diagnostic SDK command deadline",
  );
  const wrapperExecAnchor = `      'exec "$STASIS_EXPLICIT_OVERRIDE_BINARY" "$@"',`;
  instrumented = replaceExactlyOnce(
    instrumented,
    wrapperExecAnchor,
    `      'export STASIS_LIFECYCLE_TRACE_V1=1',\n${wrapperExecAnchor}`,
    "diagnostic lifecycle trace",
  );
  instrumented = replaceExactlyOnce(
    instrumented,
    CSS_ANCHOR,
    CSS_REPLACEMENT,
    "CSS request-5",
  );
  instrumented = replaceExactlyOnce(
    instrumented,
    COOKIE_ANCHOR,
    COOKIE_REPLACEMENT,
    "cookie request-5",
  );
  assert.equal(
    (instrumented.match(/stasisV03DiagnosticRecord\("begin",/gu) ?? []).length,
    2,
  );
  assert.equal(
    (instrumented.match(/stasisV03DiagnosticRecord\("error",/gu) ?? []).length,
    2,
  );
  return instrumented;
}

async function main() {
  const { values } = parseArgs({
    options: {
      input: { type: "string" },
      output: { type: "string" },
    },
    strict: true,
  });
  assert.ok(values.input, "--input is required");
  assert.ok(values.output, "--output is required");
  const inputPath = await realpath(resolve(values.input));
  const outputAbsolute = resolve(values.output);
  const outputDirectory = await realpath(dirname(outputAbsolute));
  const outputPath = resolve(outputDirectory, basename(outputAbsolute));
  assert.notEqual(inputPath, outputPath, "diagnostic output must not overwrite tagged source");
  const source = await readFile(inputPath, "utf8");
  const instrumented = instrumentExactV03Verifier(source);
  await writeFile(outputPath, instrumented, { encoding: "utf8", flag: "wx", mode: 0o700 });
  assert.equal(
    sha256(await readFile(inputPath)),
    EXACT_V03_VERIFIER_SHA256,
    "immutable verifier changed while creating diagnostic copy",
  );
  process.stdout.write(
    `${JSON.stringify({
      schema: "stasis-v0.3-macos-public-diagnostic-transform-v1",
      inputSha256: EXACT_V03_VERIFIER_SHA256,
      outputSha256: sha256(Buffer.from(instrumented, "utf8")),
      outerSignalTimeoutMs: DIAGNOSTIC_SIGNAL_TIMEOUT_MS,
      sdkCommandTimeoutMs: DIAGNOSTIC_SDK_TIMEOUT_MS,
      nativeCommandTimeoutMs: 30_000,
      nativeLifecycleTrace: true,
      releaseGateAuthority: false,
    })}\n`,
  );
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
