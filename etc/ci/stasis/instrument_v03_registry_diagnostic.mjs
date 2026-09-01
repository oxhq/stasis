#!/usr/bin/env node

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile, realpath, writeFile } from "node:fs/promises";
import { basename, dirname, resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

export const EXACT_V03_VERIFIER_SHA256 =
  "cea4d810fca0ad2e0e44009ca245a439831ae3e7d39710a851e346a892e54df9";
export const DIAGNOSTIC_STACK_SAMPLE_DELAY_MS = 10_000;
export const DIAGNOSTIC_STACK_SAMPLE_DURATION_SECONDS = 5;
export const DIAGNOSTIC_STACK_SAMPLE_TIMEOUT_MS = 15_000;
const DIAGNOSTIC_STACK_SAMPLE_DELAY_LITERAL = "10_000";
const DIAGNOSTIC_STACK_SAMPLE_DURATION_LITERAL = "5";
const DIAGNOSTIC_STACK_SAMPLE_TIMEOUT_LITERAL = "15_000";

const DIAGNOSTIC_SUPPORT = `
const STASIS_V03_DIAGNOSTIC_SCHEMA = "stasis-v0.3.2-macos-release-event-diagnostic-v2";
const stasisV03DiagnosticSample = process.env.STASIS_V03_DIAGNOSTIC_SAMPLE;
assert.match(
  stasisV03DiagnosticSample ?? "",
  /^[1-9][0-9]*$/u,
  "STASIS_V03_DIAGNOSTIC_SAMPLE must be a canonical positive integer",
);
const stasisV03DiagnosticSampleId = stasisV03DiagnosticSample.padStart(3, "0");
const stasisV03DiagnosticEvidenceDirectory =
  process.env.STASIS_V03_DIAGNOSTIC_EVIDENCE;
assert.ok(
  stasisV03DiagnosticEvidenceDirectory !== undefined &&
    isAbsolute(stasisV03DiagnosticEvidenceDirectory) &&
    resolve(stasisV03DiagnosticEvidenceDirectory) === stasisV03DiagnosticEvidenceDirectory,
  "STASIS_V03_DIAGNOSTIC_EVIDENCE must be a normalized absolute path",
);
const STASIS_V03_DIAGNOSTIC_STACK_SAMPLE_DELAY_MS =
  ${DIAGNOSTIC_STACK_SAMPLE_DELAY_LITERAL};
const STASIS_V03_DIAGNOSTIC_STACK_SAMPLE_DURATION_SECONDS =
  ${DIAGNOSTIC_STACK_SAMPLE_DURATION_LITERAL};
const STASIS_V03_DIAGNOSTIC_STACK_SAMPLE_TIMEOUT_MS =
  ${DIAGNOSTIC_STACK_SAMPLE_TIMEOUT_LITERAL};

function stasisV03DiagnosticRecord(
  kind,
  phase,
  processOrdinal,
  details = {},
  error = undefined,
) {
  const record = {
    schema: STASIS_V03_DIAGNOSTIC_SCHEMA,
    kind,
    phase,
    sample: stasisV03DiagnosticSample,
    processOrdinal,
    operation: "runtime.settle",
    releaseGateAuthority: false,
    ...details,
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
  process.stderr.write(JSON.stringify(record) + "\\n");
}

function stasisV03DiagnosticSampleArtifacts(phase, processOrdinal) {
  assert.match(phase, /^(?:css-post-start|cookie-post-submit)-settle$/u);
  assert.ok(processOrdinal === 3 || processOrdinal === 4);
  const prefix =
    "sample-" +
    stasisV03DiagnosticSampleId +
    "-process-" +
    String(processOrdinal).padStart(3, "0") +
    "-" +
    phase;
  const artifact = (suffix) => resolve(stasisV03DiagnosticEvidenceDirectory, prefix + suffix);
  const artifacts = {
    stack: artifact(".sample.txt"),
    stdout: artifact(".sample.stdout.log"),
    stderr: artifact(".sample.stderr.log"),
    metadata: artifact(".sample.json"),
  };
  for (const path of Object.values(artifacts)) {
    assert.equal(dirname(path), stasisV03DiagnosticEvidenceDirectory);
  }
  return artifacts;
}

async function stasisV03DiagnosticCaptureStackSample(phase, processOrdinal, processPid) {
  const artifacts = stasisV03DiagnosticSampleArtifacts(phase, processOrdinal);
  const artifactNames = {
    stackArtifact: basename(artifacts.stack),
    stackStdoutArtifact: basename(artifacts.stdout),
    stackStderrArtifact: basename(artifacts.stderr),
    stackMetadataArtifact: basename(artifacts.metadata),
  };
  stasisV03DiagnosticRecord("stack-sample-begin", phase, processOrdinal, {
    processPid,
    ...artifactNames,
  });
  try {
    const stdout = [];
    const stderr = [];
    const sampler = stasisV03DiagnosticSpawn(
      "/usr/bin/sample",
      [
        String(processPid),
        String(STASIS_V03_DIAGNOSTIC_STACK_SAMPLE_DURATION_SECONDS),
        "1",
        "-file",
        artifacts.stack,
      ],
      {
        env: { PATH: "/usr/bin:/bin" },
        stdio: ["ignore", "pipe", "pipe"],
        timeout: STASIS_V03_DIAGNOSTIC_STACK_SAMPLE_TIMEOUT_MS,
      },
    );
    sampler.stdout.on("data", (chunk) => stdout.push(chunk));
    sampler.stderr.on("data", (chunk) => stderr.push(chunk));
    const samplerResult = await new Promise((resolveSampler) => {
      let resolved = false;
      const finish = (result) => {
        if (resolved) return;
        resolved = true;
        resolveSampler(result);
      };
      sampler.once("error", (error) => {
        finish({ exitCode: null, signal: null, spawnError: error });
      });
      sampler.once("close", (exitCode, signal) => {
        finish({ exitCode, signal, spawnError: undefined });
      });
    });
    const stdoutBytes = Buffer.concat(stdout);
    const stderrBytes = Buffer.concat(stderr);
    await Promise.all([
      writeFile(artifacts.stdout, stdoutBytes, { flag: "wx" }),
      writeFile(artifacts.stderr, stderrBytes, { flag: "wx" }),
    ]);
    let stackBytes;
    try {
      stackBytes = await readFile(artifacts.stack);
    } catch (error) {
      if (error?.code !== "ENOENT") throw error;
    }
    const metadata = {
      schema: "stasis-v0.3.2-macos-stack-sample-v1",
      sample: stasisV03DiagnosticSample,
      phase,
      processOrdinal,
      processPid,
      delayMs: STASIS_V03_DIAGNOSTIC_STACK_SAMPLE_DELAY_MS,
      durationSeconds: STASIS_V03_DIAGNOSTIC_STACK_SAMPLE_DURATION_SECONDS,
      intervalMilliseconds: 1,
      samplerTimeoutMs: STASIS_V03_DIAGNOSTIC_STACK_SAMPLE_TIMEOUT_MS,
      samplerExitCode: samplerResult.exitCode,
      samplerSignal: samplerResult.signal,
      samplerErrorName:
        typeof samplerResult.spawnError?.name === "string"
          ? samplerResult.spawnError.name
          : null,
      samplerErrorCode:
        typeof samplerResult.spawnError?.code === "string"
          ? samplerResult.spawnError.code
          : null,
      stackArtifact: artifactNames.stackArtifact,
      stackArtifactBytes: stackBytes?.byteLength ?? null,
      stackArtifactSha256:
        stackBytes === undefined ? null : createHash("sha256").update(stackBytes).digest("hex"),
      stdoutArtifact: artifactNames.stackStdoutArtifact,
      stdoutBytes: stdoutBytes.byteLength,
      stdoutSha256: createHash("sha256").update(stdoutBytes).digest("hex"),
      stderrArtifact: artifactNames.stackStderrArtifact,
      stderrBytes: stderrBytes.byteLength,
      stderrSha256: createHash("sha256").update(stderrBytes).digest("hex"),
      releaseGateAuthority: false,
    };
    await writeFile(artifacts.metadata, JSON.stringify(metadata) + "\\n", { flag: "wx" });
    const captureSucceeded =
      metadata.samplerExitCode === 0 &&
      metadata.samplerSignal === null &&
      metadata.samplerErrorName === null &&
      metadata.stackArtifactBytes > 0;
    stasisV03DiagnosticRecord(
      captureSucceeded ? "stack-sample-end" : "stack-sample-error",
      phase,
      processOrdinal,
      {
        processPid,
        ...artifactNames,
        samplerExitCode: metadata.samplerExitCode,
        samplerSignal: metadata.samplerSignal,
        samplerErrorName: metadata.samplerErrorName,
        samplerErrorCode: metadata.samplerErrorCode,
        stackArtifactBytes: metadata.stackArtifactBytes,
        stackArtifactSha256: metadata.stackArtifactSha256,
      },
    );
    return metadata;
  } catch (error) {
    stasisV03DiagnosticRecord("stack-sample-error", phase, processOrdinal, {
      processPid,
      ...artifactNames,
      samplerErrorName: typeof error?.name === "string" ? error.name : null,
      samplerErrorCode: typeof error?.code === "string" ? error.code : null,
    });
    return undefined;
  }
}

async function stasisV03DiagnosticSettle(
  phase,
  processOrdinal,
  runtime,
  settle,
) {
  const processPid = runtime.pid;
  assert.ok(Number.isSafeInteger(processPid) && processPid > 0);
  let stackSamplePromise;
  const stackSampleTimer = setTimeout(() => {
    stackSamplePromise = stasisV03DiagnosticCaptureStackSample(
      phase,
      processOrdinal,
      processPid,
    );
  }, STASIS_V03_DIAGNOSTIC_STACK_SAMPLE_DELAY_MS);
  stackSampleTimer.unref();
  try {
    const result = await settle();
    clearTimeout(stackSampleTimer);
    if (stackSamplePromise !== undefined) await stackSamplePromise;
    return result;
  } catch (error) {
    clearTimeout(stackSampleTimer);
    if (stackSamplePromise !== undefined) await stackSamplePromise;
    stasisV03DiagnosticRecord(
      "error",
      phase,
      processOrdinal,
      {
        processPid,
        expectedMethod: "runtime.settle",
        expectedRequestId: "5",
      },
      error,
    );
    throw error;
  }
}
`;

const IMPORT_ANCHOR = `import assert from "node:assert/strict";`;

const IMPORT_REPLACEMENT = `${IMPORT_ANCHOR}
import { spawn as stasisV03DiagnosticSpawn } from "node:child_process";`;

const CSS_ANCHOR = `  const v2CssSettled = await v2CssSession.settle(
    v2CssStarted.stateToken,
    {},
    commandDeadline(),
  );`;

const CSS_REPLACEMENT = `  const v2CssSettled = await stasisV03DiagnosticSettle(
    "css-post-start-settle",
    3,
    v2CssRuntime,
    () =>
      v2CssSession.settle(
        v2CssStarted.stateToken,
        {},
        commandDeadline(),
      ),
  );`;

const COOKIE_ANCHOR = `  const cookieAuthenticated = await v2CookieSessionHandle.settle(
    cookieSubmitted.stateToken,
    {},
    commandDeadline(),
  );`;

const COOKIE_REPLACEMENT = `  const cookieAuthenticated = await stasisV03DiagnosticSettle(
    "cookie-post-submit-settle",
    4,
    v2CookieRuntime,
    () =>
      v2CookieSessionHandle.settle(
        cookieSubmitted.stateToken,
        {},
        commandDeadline(),
      ),
  );`;

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function replaceExactlyOnce(source, anchor, replacement, label) {
  const first = source.indexOf(anchor);
  assert.notEqual(first, -1, `${label} anchor is absent from the exact v0.3.2 verifier`);
  assert.equal(
    source.indexOf(anchor, first + anchor.length),
    -1,
    `${label} anchor is not unique in the exact v0.3.2 verifier`,
  );
  return source.replace(anchor, replacement);
}

export function instrumentExactV03Verifier(source) {
  assert.equal(
    Number(DIAGNOSTIC_STACK_SAMPLE_DELAY_LITERAL.replace("_", "")),
    DIAGNOSTIC_STACK_SAMPLE_DELAY_MS,
  );
  assert.equal(
    Number(DIAGNOSTIC_STACK_SAMPLE_DURATION_LITERAL),
    DIAGNOSTIC_STACK_SAMPLE_DURATION_SECONDS,
  );
  assert.equal(
    Number(DIAGNOSTIC_STACK_SAMPLE_TIMEOUT_LITERAL.replace("_", "")),
    DIAGNOSTIC_STACK_SAMPLE_TIMEOUT_MS,
  );
  assert.equal(
    sha256(Buffer.from(source, "utf8")),
    EXACT_V03_VERIFIER_SHA256,
    "diagnostic input is not the immutable v0.3.2 release verifier",
  );
  const deadlineAnchor =
    "const commandDeadline = () => ({ signal: AbortSignal.timeout(30_000) });";
  let instrumented = replaceExactlyOnce(
    source,
    IMPORT_ANCHOR,
    IMPORT_REPLACEMENT,
    "diagnostic child-process import",
  );
  instrumented = replaceExactlyOnce(
    instrumented,
    deadlineAnchor,
    `${deadlineAnchor}\n${DIAGNOSTIC_SUPPORT}`,
    "exact command deadline and diagnostic support",
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
    (instrumented.match(/stasisV03DiagnosticSettle\(/gu) ?? []).length,
    3,
  );
  assert.equal(
    (instrumented.match(/"css-post-start-settle"/gu) ?? []).length,
    1,
  );
  assert.equal(
    (instrumented.match(/"cookie-post-submit-settle"/gu) ?? []).length,
    1,
  );
  assert.equal(
    (instrumented.match(/\/usr\/bin\/sample/gu) ?? []).length,
    1,
  );
  assert.equal(
    (instrumented.match(/stack-sample-begin/gu) ?? []).length,
    1,
  );
  assert.equal(
    (instrumented.match(/AbortSignal\.timeout\(30_000\)/gu) ?? []).length,
    1,
  );
  assert.equal((instrumented.match(/commandTimeoutMs:/gu) ?? []).length, 0);
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
      schema: "stasis-v0.3.2-macos-release-event-diagnostic-transform-v2",
      inputSha256: EXACT_V03_VERIFIER_SHA256,
      outputSha256: sha256(Buffer.from(instrumented, "utf8")),
      commandDeadlineMs: 30_000,
      sdkCommandTimeoutOverride: false,
      closeTimeoutMs: 30_000,
      exactVerifierTimeoutsPreserved: true,
      nativeLifecycleTrace: true,
      stackSampleDelayMs: DIAGNOSTIC_STACK_SAMPLE_DELAY_MS,
      stackSampleDurationSeconds: DIAGNOSTIC_STACK_SAMPLE_DURATION_SECONDS,
      stackSampleIntervalMilliseconds: 1,
      stackSampleTimeoutMs: DIAGNOSTIC_STACK_SAMPLE_TIMEOUT_MS,
      releaseGateAuthority: false,
    })}\n`,
  );
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
