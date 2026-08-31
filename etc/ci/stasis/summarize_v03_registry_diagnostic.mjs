#!/usr/bin/env node

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

const SCHEMA = "stasis-v0.3-macos-public-diagnostic-v1";
const PHASES = new Set(["css-start-settle", "cookie-submit-settle"]);

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
    record.code === "aborted" &&
    record.fatal === true &&
    record.stateEffect === "indeterminate" &&
    record.method === "runtime.settle" &&
    record.requestId === "5" &&
    record.reasonName === "TimeoutError"
  );
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
  for (let sample = 1; sample <= sampleCount; sample += 1) {
    const sampleId = String(sample).padStart(3, "0");
    const [log, statusText] = await Promise.all([
      readFile(resolve(directory, `sample-${sampleId}.log`), "utf8"),
      readFile(resolve(directory, `sample-${sampleId}.status`), "utf8"),
    ]);
    const exitCode = Number(statusText.trim());
    assert.ok(Number.isSafeInteger(exitCode) && exitCode >= 0 && exitCode <= 255);
    const records = phaseRecords(log);
    const compactRecords = records.map((record) => ({
      kind: record.kind,
      phase: record.phase,
      processOrdinal: record.processOrdinal,
      ...(record.kind === "error"
        ? {
            code: record.code,
            fatal: record.fatal,
            stateEffect: record.stateEffect,
            method: record.method,
            requestId: record.requestId,
            reasonName: record.reasonName,
            stderrTailBytes: record.stderrTailBytes,
          }
        : {}),
    }));
    const error = records.find((record) => record.kind === "error");
    let classification;
    if (
      exitCode === 0 &&
      JSON.stringify(compactRecords) ===
        JSON.stringify([
          { kind: "begin", phase: "css-start-settle", processOrdinal: 3 },
          { kind: "end", phase: "css-start-settle", processOrdinal: 3 },
          { kind: "begin", phase: "cookie-submit-settle", processOrdinal: 4 },
          { kind: "end", phase: "cookie-submit-settle", processOrdinal: 4 },
        ])
    ) {
      classification = "completed";
      counts.completed += 1;
    } else if (isCandidateTimeout(error) && error.phase === "css-start-settle") {
      classification = "css_request_5_timeout";
      counts.cssRequest5Timeout += 1;
    } else if (isCandidateTimeout(error) && error.phase === "cookie-submit-settle") {
      classification = "cookie_request_5_timeout";
      counts.cookieRequest5Timeout += 1;
    } else {
      classification = "other_failure";
      counts.otherFailure += 1;
    }
    samples.push({ sample, exitCode, classification, phaseRecords: compactRecords });
  }
  return {
    schema: "stasis-v0.3-macos-public-diagnostic-summary-v1",
    releaseGateAuthority: false,
    predeclaredSampleCount: sampleCount,
    counts,
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
