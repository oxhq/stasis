#!/usr/bin/env node

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

const SCHEMA = "stasis-v0.3.2-macos-release-event-diagnostic-v2";
const TARGETS = Object.freeze({
  "css-post-start-settle": Object.freeze({
    processOrdinal: 3,
  }),
  "cookie-post-submit-settle": Object.freeze({
    processOrdinal: 4,
  }),
});
const PHASES = new Set(Object.keys(TARGETS));
const STACK_SAMPLE_SCHEMA = "stasis-v0.3.2-macos-stack-sample-v1";
const STACK_SAMPLE_DELAY_MS = 10_000;
const STACK_SAMPLE_DURATION_SECONDS = 5;
const STACK_SAMPLE_INTERVAL_MILLISECONDS = 1;
const STACK_SAMPLE_TIMEOUT_MS = 15_000;
const NATIVE_PHASES = Object.freeze([
  "script_paint_exit_marker_enqueued",
  "constellation_paint_exit_marker_enqueued",
  "paint_script_exit_marker_received",
  "paint_constellation_exit_marker_received",
  "paint_pipeline_retirement_checkpoint_received",
  "shell_servo_pump_suppressed_authority_bracket",
  "shell_servo_pump_suppressed_other",
  "paint_pipeline_retirement_owners_observed",
  "painter_webrender_retirement_send_begin",
  "painter_webrender_retirement_frame_built_queued",
  "painter_renderer_retirement_removal_consumed",
  "painter_webrender_retirement_transaction_failed",
  "constellation_paint_retirement_callback_observed",
  "controlled_replacement_reroute_begin",
]);
const NATIVE_PHASE_SET = new Set(NATIVE_PHASES);

function phaseRecords(log) {
  const records = [];
  for (const line of log.split(/\r?\n/u)) {
    let value;
    try {
      value = JSON.parse(line);
    } catch {
      continue;
    }
    if (value?.schema === SCHEMA) records.push(value);
  }
  return records;
}

function isCandidateTimeout(record) {
  return (
    record?.kind === "error" &&
    PHASES.has(record.phase) &&
    record.operation === "runtime.settle" &&
    record.expectedMethod === "runtime.settle" &&
    record.expectedRequestId === "5" &&
    record.code === "aborted" &&
    record.fatal === true &&
    record.stateEffect === "indeterminate" &&
    record.method === "runtime.settle" &&
    record.requestId === "5" &&
    record.reasonName === "TimeoutError"
  );
}

function hasValidNativePhaseEvidence(record, requireNonempty = false) {
  return (
    Array.isArray(record?.nativeLifecyclePhases) &&
    record.nativeLifecyclePhaseCount === record.nativeLifecyclePhases.length &&
    (!requireNonempty || record.nativeLifecyclePhases.length > 0) &&
    record.nativeLifecyclePhases.every((phase) => NATIVE_PHASE_SET.has(phase))
  );
}

function nativeBoundaryClassification(phases) {
  const has = (phase) => phases.includes(phase);
  const bothEnqueued =
    has("script_paint_exit_marker_enqueued") &&
    has("constellation_paint_exit_marker_enqueued");
  const scriptReceived = has("paint_script_exit_marker_received");
  const constellationReceived = has("paint_constellation_exit_marker_received");
  const ownersObserved = has("paint_pipeline_retirement_owners_observed");
  const checkpointReceived = has("paint_pipeline_retirement_checkpoint_received");
  if (
    bothEnqueued &&
    has("shell_servo_pump_suppressed_authority_bracket") &&
    !scriptReceived &&
    !constellationReceived &&
    !ownersObserved &&
    !checkpointReceived
  ) {
    return "paint_queue_starved_under_authority_suppression";
  }
  if (bothEnqueued && scriptReceived !== constellationReceived && !ownersObserved) {
    return "one_paint_owner_marker_received";
  }
  if (bothEnqueued && scriptReceived && constellationReceived && !ownersObserved) {
    return "both_markers_received_without_owner_retirement";
  }
  if (ownersObserved && !checkpointReceived) return "retirement_started_before_checkpoint";
  if (checkpointReceived) return "retirement_checkpoint_received";
  return "unclassified";
}

function validProcessPid(value) {
  return Number.isSafeInteger(value) && value > 0;
}

function isProcessIdentified(settleRecord) {
  const target = TARGETS[settleRecord.phase];
  if (target === undefined || settleRecord.processOrdinal !== target.processOrdinal) {
    return false;
  }
  return validProcessPid(settleRecord.processPid) && settleRecord.releaseGateAuthority === false;
}

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function hasExactRecordIdentity(record, sample, phase, processOrdinal, processPid) {
  return (
    record?.sample === String(sample) &&
    record.phase === phase &&
    record.operation === "runtime.settle" &&
    record.processOrdinal === processOrdinal &&
    record.processPid === processPid &&
    record.releaseGateAuthority === false
  );
}

async function hasValidatedStackCapture(directory, sample, records, settleError) {
  if (records.length !== 3 || !isProcessIdentified(settleError)) return false;
  const [begin, end, error] = records;
  if (
    error !== settleError ||
    begin.kind !== "stack-sample-begin" ||
    end.kind !== "stack-sample-end"
  ) {
    return false;
  }
  const { phase, processOrdinal, processPid } = settleError;
  if (
    !hasExactRecordIdentity(begin, sample, phase, processOrdinal, processPid) ||
    !hasExactRecordIdentity(end, sample, phase, processOrdinal, processPid) ||
    !hasExactRecordIdentity(error, sample, phase, processOrdinal, processPid)
  ) {
    return false;
  }
  const prefix =
    `sample-${String(sample).padStart(3, "0")}-process-` +
    `${String(processOrdinal).padStart(3, "0")}-${phase}`;
  const artifacts = {
    stack: `${prefix}.sample.txt`,
    stdout: `${prefix}.sample.stdout.log`,
    stderr: `${prefix}.sample.stderr.log`,
    metadata: `${prefix}.sample.json`,
  };
  if (
    begin.stackArtifact !== artifacts.stack ||
    begin.stackStdoutArtifact !== artifacts.stdout ||
    begin.stackStderrArtifact !== artifacts.stderr ||
    begin.stackMetadataArtifact !== artifacts.metadata ||
    end.stackArtifact !== artifacts.stack ||
    end.stackStdoutArtifact !== artifacts.stdout ||
    end.stackStderrArtifact !== artifacts.stderr ||
    end.stackMetadataArtifact !== artifacts.metadata ||
    end.samplerExitCode !== 0 ||
    end.samplerSignal !== null ||
    end.samplerErrorName !== null ||
    end.samplerErrorCode !== null ||
    !Number.isSafeInteger(end.stackArtifactBytes) ||
    end.stackArtifactBytes <= 0
  ) {
    return false;
  }
  let stack;
  let stdout;
  let stderr;
  let metadata;
  try {
    const [stackBytes, stdoutBytes, stderrBytes, metadataText] = await Promise.all([
      readFile(resolve(directory, artifacts.stack)),
      readFile(resolve(directory, artifacts.stdout)),
      readFile(resolve(directory, artifacts.stderr)),
      readFile(resolve(directory, artifacts.metadata), "utf8"),
    ]);
    stack = stackBytes;
    stdout = stdoutBytes;
    stderr = stderrBytes;
    metadata = JSON.parse(metadataText);
  } catch {
    return false;
  }
  const stackSha256 = sha256(stack);
  const stdoutSha256 = sha256(stdout);
  const stderrSha256 = sha256(stderr);
  return (
    metadata?.schema === STACK_SAMPLE_SCHEMA &&
    metadata.sample === String(sample) &&
    metadata.phase === phase &&
    metadata.processOrdinal === processOrdinal &&
    metadata.processPid === processPid &&
    metadata.delayMs === STACK_SAMPLE_DELAY_MS &&
    metadata.durationSeconds === STACK_SAMPLE_DURATION_SECONDS &&
    metadata.intervalMilliseconds === STACK_SAMPLE_INTERVAL_MILLISECONDS &&
    metadata.samplerTimeoutMs === STACK_SAMPLE_TIMEOUT_MS &&
    metadata.samplerExitCode === 0 &&
    metadata.samplerSignal === null &&
    metadata.samplerErrorName === null &&
    metadata.samplerErrorCode === null &&
    metadata.stackArtifact === artifacts.stack &&
    metadata.stackArtifactBytes === stack.byteLength &&
    metadata.stackArtifactSha256 === stackSha256 &&
    metadata.stdoutArtifact === artifacts.stdout &&
    metadata.stdoutBytes === stdout.byteLength &&
    metadata.stdoutSha256 === stdoutSha256 &&
    metadata.stderrArtifact === artifacts.stderr &&
    metadata.stderrBytes === stderr.byteLength &&
    metadata.stderrSha256 === stderrSha256 &&
    metadata.releaseGateAuthority === false &&
    end.stackArtifactBytes === stack.byteLength &&
    end.stackArtifactSha256 === stackSha256
  );
}

function compactRecord(record) {
  const compact = {
    kind: record.kind,
    phase: record.phase,
    operation: record.operation,
    processOrdinal: record.processOrdinal,
  };
  for (const key of [
    "processPid",
    "expectedMethod",
    "expectedRequestId",
    "code",
    "fatal",
    "stateEffect",
    "method",
    "requestId",
    "reasonName",
    "stderrTailBytes",
    "stackArtifact",
    "stackStdoutArtifact",
    "stackStderrArtifact",
    "stackMetadataArtifact",
    "samplerExitCode",
    "samplerSignal",
    "samplerErrorName",
    "samplerErrorCode",
    "stackArtifactBytes",
    "stackArtifactSha256",
    "nativeLifecyclePhases",
    "nativeLifecyclePhaseCount",
  ]) {
    if (Object.hasOwn(record, key)) compact[key] = record[key];
  }
  return compact;
}

export async function summarizeDiagnosticEvidence(directory, sampleCount) {
  assert.ok(Number.isSafeInteger(sampleCount) && sampleCount > 0);
  const samples = [];
  const counts = {
    completed: 0,
    cssRequest5Timeout: 0,
    cookieRequest5Timeout: 0,
    otherFailure: 0,
  };
  const stackSampleRecords = {
    begin: 0,
    end: 0,
    error: 0,
  };
  let validatedTimeoutStackCaptures = 0;
  const nativePhaseTotals = Object.fromEntries(NATIVE_PHASES.map((phase) => [phase, 0]));
  for (let sample = 1; sample <= sampleCount; sample += 1) {
    const sampleId = String(sample).padStart(3, "0");
    const [log, statusText] = await Promise.all([
      readFile(resolve(directory, `sample-${sampleId}.log`), "utf8"),
      readFile(resolve(directory, `sample-${sampleId}.status`), "utf8"),
    ]);
    const exitCode = Number(statusText.trim());
    assert.ok(Number.isSafeInteger(exitCode) && exitCode >= 0 && exitCode <= 255);
    const records = phaseRecords(log);
    for (const record of records) {
      if (!hasValidNativePhaseEvidence(record)) continue;
      for (const phase of record.nativeLifecyclePhases) nativePhaseTotals[phase] += 1;
    }
    const compactRecords = records.map(compactRecord);
    const stackSampleBegins = records.filter(
      (record) => record.kind === "stack-sample-begin",
    );
    const stackSampleEnds = records.filter(
      (record) => record.kind === "stack-sample-end",
    );
    const stackSampleErrors = records.filter(
      (record) => record.kind === "stack-sample-error",
    );
    assert.ok(stackSampleBegins.length <= 1, "sample launched more than one stack sampler");
    assert.ok(
      stackSampleEnds.length + stackSampleErrors.length <= 1,
      "sample recorded more than one stack sampler terminal",
    );
    stackSampleRecords.begin += stackSampleBegins.length;
    stackSampleRecords.end += stackSampleEnds.length;
    stackSampleRecords.error += stackSampleErrors.length;
    const error = records.find((record) => isCandidateTimeout(record));
    const targetRecords =
      error === undefined ? [] : records.filter((record) => record.phase === error.phase);
    const attributedTimeout =
      exitCode !== 0 &&
      error !== undefined &&
      hasValidNativePhaseEvidence(error, true) &&
      (await hasValidatedStackCapture(directory, sample, targetRecords, error));
    if (attributedTimeout) validatedTimeoutStackCaptures += 1;
    let classification;
    const completedSettles = records.filter((record) => record.kind === "settle-complete");
    const completedSample =
      exitCode === 0 &&
      records.length === 2 &&
      completedSettles.length === 2 &&
      completedSettles.every((record) =>
        isProcessIdentified(record) && hasValidNativePhaseEvidence(record)
      ) &&
      new Set(completedSettles.map((record) => record.phase)).size === 2;
    if (completedSample) {
      classification = "completed";
      counts.completed += 1;
    } else if (
      attributedTimeout &&
      error.phase === "css-post-start-settle"
    ) {
      classification = "css_request_5_timeout";
      counts.cssRequest5Timeout += 1;
    } else if (
      attributedTimeout &&
      error.phase === "cookie-post-submit-settle"
    ) {
      classification = "cookie_request_5_timeout";
      counts.cookieRequest5Timeout += 1;
    } else {
      classification = "other_failure";
      counts.otherFailure += 1;
    }
    const nativeRecord =
      error ?? completedSettles.find((record) => record.phase === "cookie-post-submit-settle");
    samples.push({
      sample,
      exitCode,
      classification,
      nativeBoundaryClassification:
        nativeRecord === undefined
          ? "unclassified"
          : nativeBoundaryClassification(nativeRecord.nativeLifecyclePhases),
      phaseRecords: compactRecords,
    });
  }
  return {
    schema: "stasis-v0.3.2-macos-release-event-diagnostic-summary-v2",
    releaseGateAuthority: false,
    predeclaredSampleCount: sampleCount,
    counts,
    stackSampleRecords,
    validatedTimeoutStackCaptures,
    nativePhaseTotals,
    samples,
  };
}

async function main() {
  const { values } = parseArgs({
    options: {
      directory: { type: "string" },
      samples: { type: "string" },
    },
    strict: true,
  });
  assert.ok(values.directory, "--directory is required");
  assert.match(values.samples ?? "", /^[1-9][0-9]*$/u, "--samples is invalid");
  const summary = await summarizeDiagnosticEvidence(
    resolve(values.directory),
    Number(values.samples),
  );
  process.stdout.write(`${JSON.stringify(summary)}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
