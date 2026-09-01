import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { summarizeDiagnosticEvidence } from "./summarize_v03_registry_diagnostic.mjs";

const schema = "stasis-v0.3.2-macos-release-event-diagnostic-v2";
const stackSchema = "stasis-v0.3.2-macos-stack-sample-v1";

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function stackArtifactNames(sample, phase, processOrdinal) {
  const prefix =
    `sample-${String(sample).padStart(3, "0")}-process-` +
    `${String(processOrdinal).padStart(3, "0")}-${phase}`;
  return {
    stackArtifact: `${prefix}.sample.txt`,
    stackStdoutArtifact: `${prefix}.sample.stdout.log`,
    stackStderrArtifact: `${prefix}.sample.stderr.log`,
    stackMetadataArtifact: `${prefix}.sample.json`,
  };
}

async function writeStackCapture(directory, sample, phase, processOrdinal, processPid) {
  const names = stackArtifactNames(sample, phase, processOrdinal);
  const stack = Buffer.from(`native stack for ${phase}\n`, "utf8");
  const stdout = Buffer.from("sample completed\n", "utf8");
  const stderr = Buffer.alloc(0);
  const metadata = {
    schema: stackSchema,
    sample: String(sample),
    phase,
    processOrdinal,
    processPid,
    delayMs: 10_000,
    durationSeconds: 5,
    intervalMilliseconds: 1,
    samplerTimeoutMs: 15_000,
    samplerExitCode: 0,
    samplerSignal: null,
    samplerErrorName: null,
    samplerErrorCode: null,
    stackArtifact: names.stackArtifact,
    stackArtifactBytes: stack.byteLength,
    stackArtifactSha256: sha256(stack),
    stdoutArtifact: names.stackStdoutArtifact,
    stdoutBytes: stdout.byteLength,
    stdoutSha256: sha256(stdout),
    stderrArtifact: names.stackStderrArtifact,
    stderrBytes: stderr.byteLength,
    stderrSha256: sha256(stderr),
    releaseGateAuthority: false,
  };
  await Promise.all([
    writeFile(join(directory, names.stackArtifact), stack),
    writeFile(join(directory, names.stackStdoutArtifact), stdout),
    writeFile(join(directory, names.stackStderrArtifact), stderr),
    writeFile(join(directory, names.stackMetadataArtifact), `${JSON.stringify(metadata)}\n`),
  ]);
  return {
    begin: { processPid, ...names },
    end: {
      processPid,
      ...names,
      samplerExitCode: 0,
      samplerSignal: null,
      samplerErrorName: null,
      samplerErrorCode: null,
      stackArtifactBytes: stack.byteLength,
      stackArtifactSha256: sha256(stack),
    },
  };
}

test("classifies completed and both request-5 timeout candidates without gate authority", async () => {
  const directory = await mkdtemp(join(tmpdir(), "stasis-v03-diagnostic-summary-"));
  try {
    const record = (sample, kind, phase, processOrdinal, extra = {}) =>
      JSON.stringify({
        schema,
        kind,
        phase,
        sample: String(sample),
        operation: "runtime.settle",
        processOrdinal,
        releaseGateAuthority: false,
        ...extra,
      });
    const timeout = (processPid) => ({
      processPid,
      expectedMethod: "runtime.settle",
      expectedRequestId: "5",
      code: "aborted",
      fatal: true,
      stateEffect: "indeterminate",
      method: "runtime.settle",
      requestId: "5",
      reasonName: "TimeoutError",
      stderrTailBytes: 0,
    });
    const cssCapture = await writeStackCapture(
      directory,
      2,
      "css-post-start-settle",
      3,
      302,
    );
    const cookieCapture = await writeStackCapture(
      directory,
      3,
      "cookie-post-submit-settle",
      4,
      403,
    );
    await Promise.all([
      writeFile(
        join(directory, "sample-001.log"),
        "",
      ),
      writeFile(join(directory, "sample-001.status"), "0\n"),
      writeFile(
        join(directory, "sample-002.log"),
        [
          record(2, "stack-sample-begin", "css-post-start-settle", 3, cssCapture.begin),
          record(2, "stack-sample-end", "css-post-start-settle", 3, cssCapture.end),
          record(
            2,
            "error",
            "css-post-start-settle",
            3,
            timeout(302),
          ),
        ].join("\n"),
      ),
      writeFile(join(directory, "sample-002.status"), "1\n"),
      writeFile(
        join(directory, "sample-003.log"),
        [
          record(3, "stack-sample-begin", "cookie-post-submit-settle", 4, cookieCapture.begin),
          record(3, "stack-sample-end", "cookie-post-submit-settle", 4, cookieCapture.end),
          record(
            3,
            "error",
            "cookie-post-submit-settle",
            4,
            timeout(403),
          ),
        ].join("\n"),
      ),
      writeFile(join(directory, "sample-003.status"), "1\n"),
    ]);
    const summary = await summarizeDiagnosticEvidence(directory, 3);
    assert.equal(summary.releaseGateAuthority, false);
    assert.deepEqual(summary.counts, {
      completed: 1,
      cssRequest5Timeout: 1,
      cookieRequest5Timeout: 1,
      otherFailure: 0,
    });
    assert.deepEqual(summary.stackSampleRecords, {
      begin: 2,
      end: 2,
      error: 0,
    });
    assert.equal(summary.validatedTimeoutStackCaptures, 2);
    assert.deepEqual(
      summary.samples.map(({ classification }) => classification),
      ["completed", "css_request_5_timeout", "cookie_request_5_timeout"],
    );
    assert.deepEqual(summary.samples[0].phaseRecords, []);
    assert.deepEqual(
      summary.samples.slice(1).map(({ phaseRecords }) =>
        phaseRecords.filter(({ kind }) => kind === "error").length,
      ),
      [1, 1],
    );
  } finally {
    await rm(directory, { recursive: true });
  }
});

test("refuses timeout attribution without exact successful hash-bound stack evidence", async () => {
  const directory = await mkdtemp(join(tmpdir(), "stasis-v03-diagnostic-negative-"));
  try {
    const record = (sample, kind, phase, processOrdinal, extra = {}) =>
      JSON.stringify({
        schema,
        kind,
        phase,
        sample: String(sample),
        operation: "runtime.settle",
        processOrdinal,
        releaseGateAuthority: false,
        ...extra,
      });
    const timeout = (processPid) => ({
      processPid,
      expectedMethod: "runtime.settle",
      expectedRequestId: "5",
      code: "aborted",
      fatal: true,
      stateEffect: "indeterminate",
      method: "runtime.settle",
      requestId: "5",
      reasonName: "TimeoutError",
      stderrTailBytes: 0,
    });
    const missingNames = stackArtifactNames(1, "cookie-post-submit-settle", 4);
    const validCapture = await writeStackCapture(
      directory,
      3,
      "cookie-post-submit-settle",
      4,
      703,
    );
    await Promise.all([
      writeFile(
        join(directory, "sample-001.log"),
        [
          record(1, "stack-sample-begin", "cookie-post-submit-settle", 4, {
            processPid: 701,
            ...missingNames,
          }),
          record(1, "stack-sample-end", "cookie-post-submit-settle", 4, {
            processPid: 701,
            ...missingNames,
            samplerExitCode: 0,
            samplerSignal: null,
            samplerErrorName: null,
            samplerErrorCode: null,
            stackArtifactBytes: 42,
            stackArtifactSha256: "a".repeat(64),
          }),
          record(1, "error", "cookie-post-submit-settle", 4, timeout(701)),
        ].join("\n"),
      ),
      writeFile(join(directory, "sample-001.status"), "1\n"),
      writeFile(
        join(directory, "sample-002.log"),
        [
          record(2, "stack-sample-begin", "cookie-post-submit-settle", 4, {
            processPid: 702,
          }),
          record(2, "stack-sample-error", "cookie-post-submit-settle", 4, {
            processPid: 702,
            samplerErrorName: "Error",
          }),
          record(2, "error", "cookie-post-submit-settle", 4, timeout(702)),
        ].join("\n"),
      ),
      writeFile(join(directory, "sample-002.status"), "1\n"),
      writeFile(
        join(directory, "sample-003.log"),
        [
          record(3, "stack-sample-begin", "cookie-post-submit-settle", 4, validCapture.begin),
          record(3, "stack-sample-end", "cookie-post-submit-settle", 4, validCapture.end),
          record(3, "error", "cookie-post-submit-settle", 4, timeout(703)),
        ].join("\n"),
      ),
      writeFile(join(directory, "sample-003.status"), "0\n"),
    ]);
    const summary = await summarizeDiagnosticEvidence(directory, 3);
    assert.deepEqual(summary.counts, {
      completed: 0,
      cssRequest5Timeout: 0,
      cookieRequest5Timeout: 0,
      otherFailure: 3,
    });
    assert.deepEqual(summary.stackSampleRecords, {
      begin: 3,
      end: 2,
      error: 1,
    });
    assert.equal(summary.validatedTimeoutStackCaptures, 0);
    assert.deepEqual(
      summary.samples.map(({ classification }) => classification),
      ["other_failure", "other_failure", "other_failure"],
    );
  } finally {
    await rm(directory, { recursive: true });
  }
});
