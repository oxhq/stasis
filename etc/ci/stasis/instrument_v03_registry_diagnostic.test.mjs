import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  DIAGNOSTIC_STACK_SAMPLE_DELAY_MS,
  DIAGNOSTIC_STACK_SAMPLE_DURATION_SECONDS,
  DIAGNOSTIC_STACK_SAMPLE_TIMEOUT_MS,
  EXACT_V03_VERIFIER_SHA256,
  instrumentExactV03Verifier,
} from "./instrument_v03_registry_diagnostic.mjs";

const verifierUrl = new URL("./verify_registry_sdk.mjs", import.meta.url);

test("binds v0.3.2 without perturbing the successful 30-second command path", async () => {
  const source = await readFile(verifierUrl, "utf8");
  const instrumented = instrumentExactV03Verifier(source);

  assert.equal(EXACT_V03_VERIFIER_SHA256.length, 64);
  assert.equal((instrumented.match(/"css-runtime-launch"/gu) ?? []).length, 0);
  assert.equal((instrumented.match(/"cookie-runtime-launch"/gu) ?? []).length, 0);
  assert.equal((instrumented.match(/"css-post-start-settle"/gu) ?? []).length, 1);
  assert.equal((instrumented.match(/"cookie-post-submit-settle"/gu) ?? []).length, 1);
  assert.equal(
    (instrumented.match(/const commandDeadline = \(\) => \(\{ signal: AbortSignal\.timeout\(30_000\) \}\);/gu) ?? [])
      .length,
    1,
  );
  assert.equal(DIAGNOSTIC_STACK_SAMPLE_DELAY_MS, 10_000);
  assert.equal(DIAGNOSTIC_STACK_SAMPLE_DURATION_SECONDS, 5);
  assert.equal(DIAGNOSTIC_STACK_SAMPLE_TIMEOUT_MS, 15_000);
  assert.equal(
    (instrumented.match(/commandTimeoutMs:/gu) ?? []).length,
    0,
  );
  assert.equal(
    (instrumented.match(/export STASIS_LIFECYCLE_TRACE_V1=1/gu) ?? []).length,
    1,
  );
  assert.equal(instrumented.includes("AbortSignal.timeout(60_000)"), false);
  assert.equal(
    (instrumented.match(/expectedRequestId: "5"/gu) ?? []).length,
    2,
  );
  assert.equal(
    (instrumented.match(/stasisV03DiagnosticLaunch\(/gu) ?? []).length,
    0,
  );
  assert.equal(
    (instrumented.match(/stasisV03DiagnosticSettle\(/gu) ?? []).length,
    3,
  );
  assert.equal(
    (instrumented.match(/stasisV03DiagnosticLifecycleEvidence\(/gu) ?? []).length,
    3,
  );
  assert.equal(
    (instrumented.match(/"settle-complete"/gu) ?? []).length,
    1,
  );
  for (const phase of [
    "script_paint_exit_marker_enqueued",
    "constellation_paint_exit_marker_enqueued",
    "paint_script_exit_marker_received",
    "paint_constellation_exit_marker_received",
    "paint_pipeline_retirement_checkpoint_received",
    "shell_servo_pump_suppressed_authority_bracket",
    "shell_servo_pump_suppressed_other",
  ]) {
    assert.equal((instrumented.match(new RegExp(`"${phase}"`, "gu")) ?? []).length, 1);
  }
  assert.equal(
    (instrumented.match(/\/usr\/bin\/sample/gu) ?? []).length,
    1,
  );
  assert.equal(
    (instrumented.match(/stackSampleTimer\.unref\(\)/gu) ?? []).length,
    1,
  );
  assert.equal(
    (instrumented.match(/if \(stackSamplePromise !== undefined\) await stackSamplePromise;/gu) ?? [])
      .length,
    2,
  );
  assert.equal(
    (instrumented.match(/STASIS_V03_DIAGNOSTIC_EVIDENCE/gu) ?? []).length >= 2,
    true,
  );
  assert.equal(
    (instrumented.match(/stasisV03DiagnosticRecord\("(?:begin|end)"/gu) ?? []).length,
    0,
  );
  assert.equal((instrumented.match(/v2CssRuntime,\n/gu) ?? []).length, 1);
  assert.equal((instrumented.match(/v2CookieRuntime,\n/gu) ?? []).length, 1);
});

test("rejects any verifier byte drift", () => {
  assert.throws(
    () => instrumentExactV03Verifier("// not the tagged verifier\n"),
    /not the immutable v0\.3\.2 release verifier/u,
  );
});
