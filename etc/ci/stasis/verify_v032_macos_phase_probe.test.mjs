import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  EXACT_BASE_REVISION,
  EXACT_PATCH_SHA256,
  validatePhaseProbeManifest,
} from "./verify_v032_macos_phase_probe.mjs";

const manifestUrl = new URL("./v032_macos_phase_probe_manifest.json", import.meta.url);
const patchUrl = new URL("./v032_macos_phase_probe.patch", import.meta.url);

test("binds the exact v0.3.2 source patch and fixed sanitized vocabulary", async () => {
  const [manifestText, patchBytes] = await Promise.all([
    readFile(manifestUrl, "utf8"),
    readFile(patchUrl),
  ]);
  const manifest = validatePhaseProbeManifest(JSON.parse(manifestText), patchBytes);
  assert.equal(manifest.baseRevision, EXACT_BASE_REVISION);
  assert.equal(manifest.patchSha256, EXACT_PATCH_SHA256);
  assert.equal(manifest.releaseGateAuthority, false);
  assert.equal(manifest.probePhases.length, 7);
});

test("rejects patch-byte or authority drift", async () => {
  const [manifestText, patchBytes] = await Promise.all([
    readFile(manifestUrl, "utf8"),
    readFile(patchUrl),
  ]);
  const manifest = JSON.parse(manifestText);
  assert.throws(
    () => validatePhaseProbeManifest(manifest, Buffer.concat([patchBytes, Buffer.from("x")])),
    /Expected values to be strictly equal/u,
  );
  assert.throws(
    () => validatePhaseProbeManifest({ ...manifest, releaseGateAuthority: true }, patchBytes),
    /Expected values to be strictly equal/u,
  );
});
