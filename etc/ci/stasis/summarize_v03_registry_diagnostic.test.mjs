import assert from "node:assert/strict";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { summarizeDiagnosticEvidence } from "./summarize_v03_registry_diagnostic.mjs";

const schema = "stasis-v0.3-macos-public-diagnostic-v1";

test("classifies completed and both request-5 timeout candidates without gate authority", async () => {
  const directory = await mkdtemp(join(tmpdir(), "stasis-v03-diagnostic-summary-"));
  try {
    const record = (kind, phase, processOrdinal, extra = {}) =>
      JSON.stringify({ schema, kind, phase, processOrdinal, ...extra });
    await Promise.all([
      writeFile(
        join(directory, "sample-001.log"),
        [
          record("begin", "css-start-settle", 3),
          record("end", "css-start-settle", 3),
          record("begin", "cookie-submit-settle", 4),
          record("end", "cookie-submit-settle", 4),
        ].join("\n"),
      ),
      writeFile(join(directory, "sample-001.status"), "0\n"),
      writeFile(
        join(directory, "sample-002.log"),
        [
          record("begin", "css-start-settle", 3),
          record("error", "css-start-settle", 3, {
            code: "aborted",
            fatal: true,
            stateEffect: "indeterminate",
            method: "runtime.settle",
            requestId: "5",
            reasonName: "TimeoutError",
            stderrTailBytes: 0,
          }),
        ].join("\n"),
      ),
      writeFile(join(directory, "sample-002.status"), "1\n"),
      writeFile(
        join(directory, "sample-003.log"),
        [
          record("begin", "css-start-settle", 3),
          record("end", "css-start-settle", 3),
          record("begin", "cookie-submit-settle", 4),
          record("error", "cookie-submit-settle", 4, {
            code: "aborted",
            fatal: true,
            stateEffect: "indeterminate",
            method: "runtime.settle",
            requestId: "5",
            reasonName: "TimeoutError",
            stderrTailBytes: 0,
          }),
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
  } finally {
    await rm(directory, { recursive: true });
  }
});
