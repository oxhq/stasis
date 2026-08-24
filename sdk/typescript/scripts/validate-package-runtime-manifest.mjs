#!/usr/bin/env node

import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import {
  GENERATED_MODULE_PREFIX,
  GENERATED_MODULE_SUFFIX,
  parseStrictJson,
  renderRuntimeManifestModule,
} from "./generate-runtime-manifest.mjs";

const PACKAGE_NAME = "@oxhq/stasis";
const MAX_PACKAGE_JSON_BYTES = 64 * 1024;
const MAX_GENERATED_MANIFEST_BYTES = 256 * 1024;
const STABLE_VERSION = /^(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)$/u;

function fail(message) {
  throw new Error(`package runtime manifest: ${message}`);
}

function argumentsFrom(argv) {
  const packageRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..");
  if (argv.length === 0) {
    return {
      packageFile: resolve(packageRoot, "package.json"),
      manifestFile: resolve(packageRoot, "src/runtime-manifest.generated.ts"),
    };
  }
  if (
    argv.length !== 4 ||
    argv[0] !== "--package" ||
    argv[2] !== "--manifest"
  ) {
    fail("usage: validate-package-runtime-manifest.mjs [--package <package.json> --manifest <generated.ts>]");
  }
  return {
    packageFile: resolve(argv[1]),
    manifestFile: resolve(argv[3]),
  };
}

async function readBounded(path, maximumBytes, label) {
  const document = await readFile(path, "utf8");
  if (Buffer.byteLength(document, "utf8") > maximumBytes) {
    fail(`${label} exceeds its validation size bound`);
  }
  return document;
}

function packageVersionFrom(document) {
  let parsed;
  try {
    parsed = parseStrictJson(document);
  } catch (error) {
    fail(`package.json is not strict JSON: ${error instanceof Error ? error.message : String(error)}`);
  }
  if (parsed === null || typeof parsed !== "object" || Array.isArray(parsed)) {
    fail("package.json must contain an object");
  }
  if (parsed.name !== PACKAGE_NAME) {
    fail(`package name must be ${PACKAGE_NAME}`);
  }
  if (typeof parsed.version !== "string" || !STABLE_VERSION.test(parsed.version)) {
    fail("package version must be a stable three-part semantic version");
  }
  return parsed.version;
}

function manifestFromGeneratedModule(document) {
  if (
    !document.startsWith(GENERATED_MODULE_PREFIX) ||
    !document.endsWith(GENERATED_MODULE_SUFFIX)
  ) {
    fail("generated manifest does not have the canonical module envelope");
  }
  const literal = document.slice(
    GENERATED_MODULE_PREFIX.length,
    document.length - GENERATED_MODULE_SUFFIX.length,
  );
  let manifest;
  try {
    manifest = parseStrictJson(literal);
  } catch (error) {
    fail(`generated manifest is not strict JSON: ${error instanceof Error ? error.message : String(error)}`);
  }
  let canonical;
  try {
    canonical = renderRuntimeManifestModule(manifest);
  } catch (error) {
    fail(error instanceof Error ? error.message : String(error));
  }
  if (document !== canonical) {
    fail("generated manifest is not in canonical deterministic form");
  }
  return manifest;
}

async function main(argv) {
  const paths = argumentsFrom(argv);
  const [packageDocument, manifestDocument] = await Promise.all([
    readBounded(paths.packageFile, MAX_PACKAGE_JSON_BYTES, "package.json"),
    readBounded(paths.manifestFile, MAX_GENERATED_MANIFEST_BYTES, "generated manifest"),
  ]);
  const packageVersion = packageVersionFrom(packageDocument);
  const manifest = manifestFromGeneratedModule(manifestDocument);
  if (
    manifest.packageName !== PACKAGE_NAME ||
    manifest.sdkVersion !== packageVersion ||
    manifest.releaseTag !== `v${packageVersion}`
  ) {
    fail(`generated manifest must be bound exactly to ${PACKAGE_NAME}@${packageVersion}`);
  }
  process.stdout.write(`validated ${PACKAGE_NAME}@${packageVersion} runtime manifest\n`);
}

await main(process.argv.slice(2));
