#!/usr/bin/env node

import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { execFileSync } from "node:child_process";
import { readFile, realpath } from "node:fs/promises";
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { parseArgs } from "node:util";

export const SCHEMA = "stasis-v0.3.2-macos-phase-probe-patch-v1";
export const EXACT_BASE_REVISION = "b3d1ac949d341dc6bbe1244162441d9bb8adb00a";
export const EXACT_PATCH_SHA256 =
  "d0a60c71c4a714f0d533f251c4ea1134f938c8cdf1e7e3d4e44c70a3dc93530a";

const EXACT_PHASES = Object.freeze([
  "script_paint_exit_marker_enqueued",
  "constellation_paint_exit_marker_enqueued",
  "paint_script_exit_marker_received",
  "paint_constellation_exit_marker_received",
  "paint_pipeline_retirement_checkpoint_received",
  "shell_servo_pump_suppressed_authority_bracket",
  "shell_servo_pump_suppressed_other",
]);

function sha256(bytes) {
  return createHash("sha256").update(bytes).digest("hex");
}

function git(source, ...args) {
  return execFileSync("git", args, {
    cwd: source,
    encoding: "utf8",
    env: { ...process.env, LC_ALL: "C" },
  }).trim();
}

export function validatePhaseProbeManifest(manifest, patchBytes) {
  assert.equal(manifest?.schema, SCHEMA);
  assert.equal(manifest.baseRevision, EXACT_BASE_REVISION);
  assert.equal(manifest.patchSha256, EXACT_PATCH_SHA256);
  assert.equal(sha256(patchBytes), EXACT_PATCH_SHA256);
  assert.match(manifest.baseTree, /^[0-9a-f]{40}$/u);
  assert.match(manifest.patchedTree, /^[0-9a-f]{40}$/u);
  assert.match(manifest.rustToolchainBlob, /^[0-9a-f]{40}$/u);
  assert.match(manifest.cargoLockBlob, /^[0-9a-f]{40}$/u);
  assert.equal(manifest.releaseGateAuthority, false);
  assert.deepEqual(manifest.probePhases, EXACT_PHASES);
  assert.ok(Array.isArray(manifest.files));
  assert.equal(manifest.files.length, 5);
  const paths = manifest.files.map(({ path }) => path);
  assert.deepEqual(paths, [...paths].sort());
  assert.equal(new Set(paths).size, paths.length);
  for (const file of manifest.files) {
    assert.match(file.path, /^(?:components|ports)\/[a-z0-9_./-]+\.rs$/u);
    assert.match(file.baseBlob, /^[0-9a-f]{40}$/u);
    assert.match(file.patchedBlob, /^[0-9a-f]{40}$/u);
    assert.notEqual(file.baseBlob, file.patchedBlob);
  }
  return manifest;
}

export async function verifyPhaseProbe({ source, patch, manifest: manifestPath, state }) {
  assert.ok(state === "base" || state === "patched", "--state must be base or patched");
  const [sourceReal, patchReal, manifestReal] = await Promise.all([
    realpath(resolve(source)),
    realpath(resolve(patch)),
    realpath(resolve(manifestPath)),
  ]);
  const [patchBytes, manifestText] = await Promise.all([
    readFile(patchReal),
    readFile(manifestReal, "utf8"),
  ]);
  const manifest = validatePhaseProbeManifest(JSON.parse(manifestText), patchBytes);
  assert.equal(git(sourceReal, "rev-parse", "HEAD"), manifest.baseRevision);
  assert.equal(git(sourceReal, "rev-parse", "HEAD^{tree}"), manifest.baseTree);
  assert.equal(
    git(sourceReal, "rev-parse", "HEAD:rust-toolchain.toml"),
    manifest.rustToolchainBlob,
  );
  assert.equal(git(sourceReal, "rev-parse", "HEAD:Cargo.lock"), manifest.cargoLockBlob);

  for (const file of manifest.files) {
    assert.equal(git(sourceReal, "rev-parse", `HEAD:${file.path}`), file.baseBlob);
    const expected = state === "base" ? file.baseBlob : file.patchedBlob;
    assert.equal(git(sourceReal, "hash-object", "--", file.path), expected);
  }

  if (state === "base") {
    assert.equal(git(sourceReal, "status", "--porcelain=v1", "--untracked-files=all"), "");
    git(sourceReal, "apply", "--check", "--whitespace=error-all", patchReal);
  } else {
    assert.equal(git(sourceReal, "write-tree"), manifest.patchedTree);
    git(sourceReal, "apply", "--check", "--reverse", patchReal);
    git(sourceReal, "diff", "--cached", "--check");
    const status = git(
      sourceReal,
      "status",
      "--porcelain=v1",
      "--untracked-files=all",
      "--ignore-submodules=none",
    )
      .split("\n")
      .filter(Boolean);
    assert.deepEqual(
      status,
      manifest.files.map(({ path }) => `M  ${path}`),
    );
  }
  return {
    schema: "stasis-v0.3.2-macos-phase-probe-source-verification-v1",
    baseRevision: manifest.baseRevision,
    baseTree: manifest.baseTree,
    patchedTree: manifest.patchedTree,
    patchSha256: manifest.patchSha256,
    state,
    releaseGateAuthority: false,
  };
}

async function main() {
  const { values } = parseArgs({
    options: {
      source: { type: "string" },
      patch: { type: "string" },
      manifest: { type: "string" },
      state: { type: "string" },
    },
    strict: true,
  });
  for (const name of ["source", "patch", "manifest", "state"]) {
    assert.ok(values[name], `--${name} is required`);
  }
  const result = await verifyPhaseProbe(values);
  process.stdout.write(`${JSON.stringify(result)}\n`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  await main();
}
