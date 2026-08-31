import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  EXACT_V03_VERIFIER_SHA256,
  instrumentExactV03Verifier,
} from "./instrument_v03_registry_diagnostic.mjs";

const verifierUrl = new URL("./verify_registry_sdk.mjs", import.meta.url);

test("instruments only the two ambiguous request-5 settlements", async () => {
  const source = await readFile(verifierUrl, "utf8");
  const instrumented = instrumentExactV03Verifier(source);

  assert.equal(EXACT_V03_VERIFIER_SHA256.length, 64);
  assert.equal((instrumented.match(/"css-start-settle"/gu) ?? []).length, 3);
  assert.equal((instrumented.match(/"cookie-submit-settle"/gu) ?? []).length, 3);
  assert.equal(
    (instrumented.match(/const commandDeadline = \(\) => \(\{ signal: AbortSignal\.timeout\(30_000\) \}\);/gu) ?? [])
      .length,
    1,
  );
  assert.equal(
    (instrumented.match(/expectedRequestId: "5"/gu) ?? []).length,
    1,
  );
  assert.equal(
    (instrumented.match(/processOrdinal,?\n/gu) ?? []).length >= 1,
    true,
  );
});

test("rejects any verifier byte drift", () => {
  assert.throws(
    () => instrumentExactV03Verifier("// not the tagged verifier\n"),
    /not the immutable v0\.3\.0 registry verifier/u,
  );
});
