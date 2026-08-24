import { createHash, randomUUID } from "node:crypto";
import { constants, createReadStream } from "node:fs";
import {
  chmod,
  lstat,
  link,
  mkdir,
  mkdtemp,
  open,
  rename,
  rm,
  unlink,
  writeFile,
  type FileHandle,
} from "node:fs/promises";
import type { IncomingMessage } from "node:http";
import { get as httpsGet } from "node:https";
import { homedir } from "node:os";
import { dirname, isAbsolute, join, posix, resolve } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import { createGunzip } from "node:zlib";

import { StasisAbortError, StasisError } from "./errors.js";
import {
  RUNTIME_DISTRIBUTION_MANIFEST,
  runtimePlatformKey,
  type RuntimeArtifactManifest,
  type RuntimeDistributionManifest,
} from "./runtime-manifest.js";
import type { RuntimeInfo } from "./types.js";

const MAX_COMPRESSED_ARCHIVE_BYTES = 512 * 1024 * 1024;
const MAX_UNCOMPRESSED_ARCHIVE_BYTES = 600 * 1024 * 1024;
const MAX_ARCHIVE_MEMBER_BYTES = 512 * 1024 * 1024;
const MAX_ARCHIVE_MEMBERS = 256;
const DOWNLOAD_IDLE_TIMEOUT_MS = 30_000;
const DOWNLOAD_TOTAL_TIMEOUT_MS = 10 * 60_000;
const MAX_DOWNLOAD_TOTAL_TIMEOUT_MS = 24 * 60 * 60_000;
const MAX_REDIRECTS = 5;
const LOCK_WAIT_MS = 10 * 60_000;
const LOCK_POLL_MS = 100;
const LOCK_STALE_MS = 2 * 60_000;
const IO_CHUNK_BYTES = 1024 * 1024;
const TAR_BLOCK_BYTES = 512;
const CACHE_SCHEMA_DIRECTORY = "runtime-v1";
const CACHE_MARKER = ".stasis-runtime.json";
const SHA256_PATTERN = /^[0-9a-f]{64}$/u;
const SAFE_COMPONENT_PATTERN = /^[A-Za-z0-9][A-Za-z0-9._-]*$/u;
const WINDOWS_FORBIDDEN_PATTERN = /[<>:"\\|?*\u0000-\u001f]/u;
const WINDOWS_RESERVED_NAMES = new Set([
  "con",
  "prn",
  "aux",
  "nul",
  ...Array.from({ length: 9 }, (_value, index) => `com${index + 1}`),
  ...Array.from({ length: 9 }, (_value, index) => `lpt${index + 1}`),
]);

export class RuntimeResolutionError extends StasisError {
  constructor(message: string, options?: ErrorOptions) {
    super(message, "", options);
    this.name = "RuntimeResolutionError";
  }
}

export interface RuntimeResolutionOptions {
  /** Override the per-user runtime cache. Relative paths are resolved from cwd. */
  cacheDirectory?: string;
  signal?: AbortSignal;
}

/** @internal */
export interface RuntimeResolverDependencies {
  manifest?: RuntimeDistributionManifest;
  platform?: NodeJS.Platform;
  architecture?: string;
  downloadArchive?: (
    artifact: RuntimeArtifactManifest,
    destination: string,
    signal: AbortSignal | undefined,
  ) => Promise<void>;
  lockWaitMs?: number;
  lockPollMs?: number;
  maxUncompressedArchiveBytes?: number;
  maxArchiveMembers?: number;
  staleLockMs?: number;
  /** @internal Test/release override for the absolute archive-transfer deadline. */
  downloadTotalTimeoutMs?: number;
  /** @internal Test seam immediately before a fully written primary lock is published. */
  beforePrimaryLockPublish?: (candidate: string, destination: string) => Promise<void>;
}

interface SelectedRuntime {
  manifest: RuntimeDistributionManifest;
  artifact: RuntimeArtifactManifest;
  platformKey: string;
}

interface FileDigest {
  sha256: string;
  size: number;
}

interface CacheLock {
  filename: string;
  owner: string;
}

interface CacheLockOwner {
  schema: 1;
  pid: number;
  createdAtMs: number;
  nonce: string;
}

interface TarEntry {
  path: string;
  type: "directory" | "file";
  size: number;
}

/**
 * Resolve, install, and verify the native runtime bound to an exact SDK
 * version. This function performs network I/O only when the digest-keyed cache
 * does not already contain a verified executable.
 */
export async function resolveRuntimeExecutable(
  sdkVersion: string,
  options: RuntimeResolutionOptions = {},
): Promise<string> {
  return resolveRuntimeExecutableForTesting(sdkVersion, options);
}

/** @internal */
export async function resolveRuntimeExecutableForTesting(
  sdkVersion: string,
  options: RuntimeResolutionOptions = {},
  dependencies: RuntimeResolverDependencies = {},
): Promise<string> {
  if (typeof sdkVersion !== "string" || sdkVersion.length === 0) {
    throw new TypeError("sdkVersion must be a non-empty string");
  }
  throwIfAborted(options.signal);

  const selected = selectRuntime(
    sdkVersion,
    dependencies.manifest ?? RUNTIME_DISTRIBUTION_MANIFEST,
    dependencies.platform ?? process.platform,
    dependencies.architecture ?? process.arch,
  );
  const cacheDirectory = resolveCacheDirectory(options.cacheDirectory, selected.platformKey);
  const cacheEntry = join(
    cacheDirectory,
    CACHE_SCHEMA_DIRECTORY,
    sdkVersion,
    selected.platformKey,
    selected.artifact.archiveSha256,
  );
  const executable = cacheExecutable(cacheEntry, selected.artifact);
  const expectedMarker = cacheMarker(selected);

  try {
    if (await cacheEntryIsValid(cacheEntry, executable, selected.artifact, expectedMarker, options.signal)) {
      return executable;
    }

    const lockDirectory = join(cacheDirectory, CACHE_SCHEMA_DIRECTORY, ".locks");
    await mkdir(lockDirectory, { recursive: true, mode: 0o700 });
    const lockFilename = join(
      lockDirectory,
      `${sdkVersion}-${selected.platformKey}-${selected.artifact.archiveSha256}.lock`,
    );
    const lock = await acquireCacheLock(
      lockFilename,
      async () => cacheEntryIsValid(cacheEntry, executable, selected.artifact, expectedMarker, options.signal),
      options.signal,
      boundedPositiveInteger(dependencies.lockWaitMs ?? LOCK_WAIT_MS, "lockWaitMs", LOCK_WAIT_MS),
      boundedPositiveInteger(dependencies.lockPollMs ?? LOCK_POLL_MS, "lockPollMs", LOCK_POLL_MS),
      boundedPositiveInteger(
        dependencies.staleLockMs ?? LOCK_STALE_MS,
        "staleLockMs",
        LOCK_WAIT_MS,
      ),
      dependencies.beforePrimaryLockPublish,
    );
    if (lock === null) return executable;

    try {
      if (await cacheEntryIsValid(cacheEntry, executable, selected.artifact, expectedMarker, options.signal)) {
        return executable;
      }
      return await installRuntime(
        selected,
        cacheDirectory,
        cacheEntry,
        expectedMarker,
        options.signal,
        dependencies,
      );
    } finally {
      await releaseCacheLock(lock);
    }
  } catch (error) {
    if (options.signal?.aborted === true) {
      throw new StasisAbortError(options.signal.reason);
    }
    throw error;
  }
}

/** @internal */
export function assertManagedRuntimeIdentity(
  sdkVersion: string,
  runtime: RuntimeInfo,
  manifest: RuntimeDistributionManifest = RUNTIME_DISTRIBUTION_MANIFEST,
): void {
  validateManifestIdentity(sdkVersion, manifest);
  const expectedSource = manifest.implementation.source;
  const actualSource = runtime.implementation.source;
  const expectedKeys = Object.keys(expectedSource).sort();
  const actualKeys = Object.keys(actualSource).sort();
  if (
    runtime.implementation.name !== manifest.implementation.name ||
    runtime.implementation.version !== sdkVersion ||
    expectedKeys.length !== actualKeys.length ||
    expectedKeys.some((key, index) => key !== actualKeys[index] || actualSource[key] !== expectedSource[key])
  ) {
    throw new RuntimeResolutionError(
      `The managed native runtime identity does not match @oxhq/stasis@${sdkVersion}`,
    );
  }
}

function selectRuntime(
  sdkVersion: string,
  manifest: RuntimeDistributionManifest,
  platform: NodeJS.Platform,
  architecture: string,
): SelectedRuntime {
  validateManifestIdentity(sdkVersion, manifest);
  requireSafeComponent(sdkVersion, "SDK version");
  const platformKey = runtimePlatformKey(platform, architecture);
  requireSafeComponent(platformKey, "runtime platform");
  const artifact = manifest.artifacts[platformKey];
  if (artifact === undefined) {
    throw new RuntimeResolutionError(
      `@oxhq/stasis@${sdkVersion} has no native runtime for ${platformKey}; pass executablePath to use an explicit compatible runtime`,
    );
  }
  validateArtifact(artifact, platform, architecture);
  return { manifest, artifact, platformKey };
}

function validateManifestIdentity(
  sdkVersion: string,
  manifest: RuntimeDistributionManifest,
): void {
  const source = manifest.implementation?.source;
  if (
    manifest.schema !== 1 ||
    manifest.packageName !== "@oxhq/stasis" ||
    manifest.sdkVersion !== sdkVersion ||
    manifest.releaseTag !== `v${sdkVersion}` ||
    manifest.implementation?.name !== "stasis-shell" ||
    typeof source !== "object" ||
    source === null ||
    Array.isArray(source) ||
    Object.keys(source).length < 1 ||
    Object.entries(source).some(
      ([key, value]) => !SAFE_COMPONENT_PATTERN.test(key) || typeof value !== "string" || value.length === 0,
    )
  ) {
    throw new RuntimeResolutionError(
      `The native runtime manifest does not exactly match @oxhq/stasis@${sdkVersion}`,
    );
  }
}

function validateArtifact(
  artifact: RuntimeArtifactManifest,
  platform: NodeJS.Platform,
  architecture: string,
): void {
  if (artifact.nodePlatform !== platform || artifact.nodeArch !== architecture) {
    throw new RuntimeResolutionError("The selected native runtime manifest has a platform mismatch");
  }
  requireSafeComponent(artifact.releasePlatform, "release platform");
  validateHttpsUrl(new URL(artifact.archiveUrl), "native runtime archive URL");
  if (
    !Number.isSafeInteger(artifact.archiveSizeBytes) ||
    artifact.archiveSizeBytes < 1 ||
    artifact.archiveSizeBytes > MAX_COMPRESSED_ARCHIVE_BYTES
  ) {
    throw new RuntimeResolutionError("The native runtime archive size is outside the release bound");
  }
  requireSha256(artifact.archiveSha256, "native runtime archive SHA-256");
  requireSha256(artifact.executableSha256, "native runtime executable SHA-256");
  validateArchivePath(artifact.archiveRoot, "archive root");
  validateArchivePath(artifact.executablePath, "runtime executable path");
  if (artifact.archiveRoot.includes("/") || artifact.executablePath === artifact.archiveRoot) {
    throw new RuntimeResolutionError("The native runtime manifest has an invalid archive layout");
  }
  if (!Array.isArray(artifact.archiveFiles) || artifact.archiveFiles.length < 1) {
    throw new RuntimeResolutionError("The native runtime manifest has an empty archive inventory");
  }
  const archiveFiles = new Set<string>();
  for (const filename of artifact.archiveFiles) {
    validateArchivePath(filename, "manifest archive file");
    const collisionKey = portableCollisionKey(filename);
    if (archiveFiles.has(collisionKey)) {
      throw new RuntimeResolutionError("The native runtime manifest has colliding archive files");
    }
    archiveFiles.add(collisionKey);
  }
  for (const filename of archiveFiles) {
    for (const other of archiveFiles) {
      if (other.startsWith(`${filename}/`)) {
        throw new RuntimeResolutionError("The native runtime manifest uses a file as a parent directory");
      }
    }
  }
  if (!archiveFiles.has(portableCollisionKey(artifact.executablePath))) {
    throw new RuntimeResolutionError("The native runtime executable is absent from the archive inventory");
  }
}

function resolveCacheDirectory(configured: string | undefined, platformKey: string): string {
  if (configured !== undefined) {
    if (typeof configured !== "string" || configured.length === 0 || configured.includes("\0")) {
      throw new TypeError("cacheDirectory must be a non-empty path");
    }
    return resolve(configured);
  }
  if (platformKey.startsWith("darwin-")) {
    return join(homedir(), "Library", "Caches", "oxhq", "stasis");
  }
  if (platformKey.startsWith("win32-")) {
    const localAppData = process.env.LOCALAPPDATA;
    return join(
      typeof localAppData === "string" && localAppData.length > 0
        ? localAppData
        : join(homedir(), "AppData", "Local"),
      "OxHQ",
      "Stasis",
    );
  }
  const xdgCache = process.env.XDG_CACHE_HOME;
  return join(
    typeof xdgCache === "string" && isAbsolute(xdgCache) ? xdgCache : join(homedir(), ".cache"),
    "oxhq",
    "stasis",
  );
}

function cacheExecutable(cacheEntry: string, artifact: RuntimeArtifactManifest): string {
  return join(cacheEntry, ...artifact.executablePath.split("/"));
}

function cacheMarker(selected: SelectedRuntime): string {
  return `${JSON.stringify({
    schema: 1,
    packageName: selected.manifest.packageName,
    sdkVersion: selected.manifest.sdkVersion,
    releaseTag: selected.manifest.releaseTag,
    implementation: selected.manifest.implementation,
    platform: selected.platformKey,
    releasePlatform: selected.artifact.releasePlatform,
    archiveUrl: selected.artifact.archiveUrl,
    archiveSizeBytes: selected.artifact.archiveSizeBytes,
    archiveSha256: selected.artifact.archiveSha256,
    archiveFiles: [...selected.artifact.archiveFiles],
    executablePath: selected.artifact.executablePath,
    executableSha256: selected.artifact.executableSha256,
  })}\n`;
}

async function cacheEntryIsValid(
  cacheEntry: string,
  executable: string,
  artifact: RuntimeArtifactManifest,
  expectedMarker: string,
  signal?: AbortSignal,
): Promise<boolean> {
  try {
    throwIfAborted(signal);
    const directory = await lstat(cacheEntry);
    if (directory.isSymbolicLink() || !directory.isDirectory()) return false;
    const markerFilename = join(cacheEntry, CACHE_MARKER);
    if (!(await exactRegularFileMatches(markerFilename, Buffer.from(expectedMarker, "utf8")))) return false;
    const binary = await sha256RegularFile(executable, MAX_ARCHIVE_MEMBER_BYTES, signal);
    if (binary.size < 1 || binary.sha256 !== artifact.executableSha256) return false;
    const binaryMetadata = await lstat(executable);
    return artifact.nodePlatform === "win32" || (binaryMetadata.mode & 0o111) !== 0;
  } catch (error) {
    if (isMissing(error) || isNotDirectory(error) || isSymbolicLoop(error)) return false;
    throw error;
  }
}

async function acquireCacheLock(
  filename: string,
  cacheBecameValid: () => Promise<boolean>,
  signal: AbortSignal | undefined,
  waitMs: number,
  pollMs: number,
  staleMs: number,
  beforePublish?: (candidate: string, destination: string) => Promise<void>,
): Promise<CacheLock | null> {
  const deadline = Date.now() + waitMs;
  while (true) {
    throwIfAborted(signal);
    const owner = `${JSON.stringify({
      schema: 1,
      pid: process.pid,
      createdAtMs: Date.now(),
      nonce: randomUUID(),
    } satisfies CacheLockOwner)}\n`;
    try {
      if (await publishCompleteLock(filename, owner, beforePublish)) {
        return { filename, owner };
      }
    } catch (error) {
      throw error;
    }
    if (await cacheBecameValid()) return null;
    if (await reclaimDeadCacheLock(filename, staleMs)) continue;
    if (Date.now() >= deadline) {
      throw new RuntimeResolutionError(
        `Timed out waiting for the native runtime cache lock at ${filename}`,
      );
    }
    await delay(pollMs, undefined, signal === undefined ? undefined : { signal });
  }
}

async function publishCompleteLock(
  filename: string,
  owner: string,
  beforePublish?: (candidate: string, destination: string) => Promise<void>,
): Promise<boolean> {
  const candidate = `${filename}.${randomUUID()}.candidate`;
  let handle: FileHandle | undefined;
  try {
    handle = await open(candidate, constants.O_CREAT | constants.O_EXCL | constants.O_WRONLY, 0o600);
    await handle.writeFile(owner, "utf8");
    await handle.sync();
    await handle.close();
    handle = undefined;
    if (beforePublish !== undefined) await beforePublish(candidate, filename);
    try {
      await link(candidate, filename);
      return true;
    } catch (error) {
      if (isAlreadyExists(error)) return false;
      throw error;
    }
  } finally {
    await handle?.close().catch(() => undefined);
    await unlink(candidate).catch((error: unknown) => {
      if (!isMissing(error)) throw error;
    });
  }
}

async function releaseCacheLock(lock: CacheLock): Promise<void> {
  let owner: string;
  try {
    owner = await readBoundedRegularUtf8(lock.filename, 1_024);
  } catch (error) {
    if (isMissing(error)) return;
    throw error;
  }
  if (owner !== lock.owner) {
    throw new RuntimeResolutionError(
      `Native runtime cache lock ownership changed unexpectedly at ${lock.filename}`,
    );
  }
  await unlink(lock.filename).catch((error: unknown) => {
    if (!isMissing(error)) throw error;
  });
}

async function reclaimDeadCacheLock(filename: string, staleMs: number): Promise<boolean> {
  let metadata;
  try {
    metadata = await lstat(filename);
    if (metadata.isSymbolicLink() || !metadata.isFile()) {
      throw new RuntimeResolutionError(
        `Refusing to replace an unsafe native runtime cache lock at ${filename}`,
      );
    }
  } catch (error) {
    if (isMissing(error)) return true;
    throw error;
  }
  const ageMs = Date.now() - metadata.mtimeMs;
  if (metadata.size < 1 || metadata.size > 1_024) {
    if (ageMs < staleMs) return false;
    throw new RuntimeResolutionError(
      `Refusing to remove stale native runtime cache lock with invalid size at ${filename}`,
    );
  }
  let document: string;
  try {
    document = await readBoundedRegularUtf8(filename, 1_024);
  } catch (error) {
    if (isMissing(error)) return true;
    throw error;
  }
  if (ageMs < staleMs) return false;
  const owner = parseCacheLockOwner(document);
  if (owner === null) {
    throw new RuntimeResolutionError(
      `Refusing to remove stale native runtime cache lock with invalid owner metadata at ${filename}`,
    );
  }
  if (Date.now() - owner.createdAtMs < staleMs) return false;
  if (processIsAlive(owner.pid)) return false;

  const reclaimFilename = `${filename}.reclaim`;
  const reclaimOwner = `${JSON.stringify({
    schema: 1,
    pid: process.pid,
    createdAtMs: Date.now(),
    nonce: randomUUID(),
  } satisfies CacheLockOwner)}\n`;
  const reclaimCandidate = `${reclaimFilename}.${randomUUID()}.candidate`;
  try {
    await writeFile(reclaimCandidate, reclaimOwner, { flag: "wx", mode: 0o600 });
    await link(reclaimCandidate, reclaimFilename);
  } catch (error) {
    if (isAlreadyExists(error)) {
      if (!(await reclaimOrphanedReclaimLock(reclaimFilename, staleMs))) return false;
      return reclaimDeadCacheLock(filename, staleMs);
    }
    throw error;
  } finally {
    await unlink(reclaimCandidate).catch((error: unknown) => {
      if (!isMissing(error)) throw error;
    });
  }
  try {
    let current: string;
    let currentMetadata;
    try {
      current = await readBoundedRegularUtf8(filename, 1_024);
      currentMetadata = await lstat(filename);
    } catch (error) {
      if (isMissing(error)) return true;
      throw error;
    }
    if (
      current !== document ||
      currentMetadata.isSymbolicLink() ||
      !currentMetadata.isFile() ||
      Date.now() - currentMetadata.mtimeMs < staleMs
    ) {
      return false;
    }
    const currentOwner = parseCacheLockOwner(current);
    if (
      currentOwner === null ||
      Date.now() - currentOwner.createdAtMs < staleMs ||
      processIsAlive(currentOwner.pid)
    ) {
      return false;
    }
    await unlink(filename);
    return true;
  } finally {
    const currentOwner = await readBoundedRegularUtf8(reclaimFilename, 1_024).catch((error: unknown) => {
      if (isMissing(error)) return null;
      throw error;
    });
    if (currentOwner === reclaimOwner) {
      await unlink(reclaimFilename).catch((error: unknown) => {
        if (!isMissing(error)) throw error;
      });
    }
  }
}

async function reclaimOrphanedReclaimLock(filename: string, staleMs: number): Promise<boolean> {
  let metadata;
  let document: string;
  try {
    metadata = await lstat(filename);
    if (metadata.isSymbolicLink() || !metadata.isFile() || metadata.size < 1 || metadata.size > 1_024) {
      throw new RuntimeResolutionError(`Refusing to replace an unsafe cache reclaim lock at ${filename}`);
    }
    if (Date.now() - metadata.mtimeMs < staleMs) return false;
    document = await readBoundedRegularUtf8(filename, 1_024);
  } catch (error) {
    if (isMissing(error)) return true;
    throw error;
  }
  const owner = parseCacheLockOwner(document);
  if (owner === null || Date.now() - owner.createdAtMs < staleMs) {
    throw new RuntimeResolutionError(`Refusing to remove cache reclaim lock with invalid owner metadata at ${filename}`);
  }
  if (processIsAlive(owner.pid)) return false;
  const current = await readBoundedRegularUtf8(filename, 1_024).catch((error: unknown) => {
    if (isMissing(error)) return null;
    throw error;
  });
  if (current === null) return true;
  if (current !== document) return false;
  await unlink(filename).catch((error: unknown) => {
    if (!isMissing(error)) throw error;
  });
  return true;
}

function parseCacheLockOwner(document: string): CacheLockOwner | null {
  let value: unknown;
  try {
    value = JSON.parse(document) as unknown;
  } catch {
    return null;
  }
  if (typeof value !== "object" || value === null || Array.isArray(value)) return null;
  const owner = value as Record<string, unknown>;
  if (
    Object.keys(owner).sort().join(",") !== "createdAtMs,nonce,pid,schema" ||
    owner.schema !== 1 ||
    !Number.isSafeInteger(owner.pid) ||
    (owner.pid as number) < 1 ||
    !Number.isSafeInteger(owner.createdAtMs) ||
    (owner.createdAtMs as number) < 0 ||
    typeof owner.nonce !== "string" ||
    !/^[0-9a-f-]{36}$/u.test(owner.nonce)
  ) {
    return null;
  }
  return owner as unknown as CacheLockOwner;
}

function processIsAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return errorCode(error) !== "ESRCH";
  }
}

async function installRuntime(
  selected: SelectedRuntime,
  cacheDirectory: string,
  cacheEntry: string,
  expectedMarker: string,
  signal: AbortSignal | undefined,
  dependencies: RuntimeResolverDependencies,
): Promise<string> {
  const temporaryParent = join(cacheDirectory, CACHE_SCHEMA_DIRECTORY, ".tmp");
  await mkdir(temporaryParent, { recursive: true, mode: 0o700 });
  const temporary = await mkdtemp(join(temporaryParent, `${selected.artifact.archiveSha256}.`));
  const archive = join(temporary, "runtime.tar.gz");
  const extracted = join(temporary, "extracted");
  const extractedRoot = join(extracted, selected.artifact.archiveRoot);
  let quarantine: string | undefined;
  try {
    throwIfAborted(signal);
    const download = dependencies.downloadArchive ?? downloadRuntimeArchive;
    await downloadWithDeadline(
      download,
      selected.artifact,
      archive,
      signal,
      boundedPositiveInteger(
        dependencies.downloadTotalTimeoutMs ?? DOWNLOAD_TOTAL_TIMEOUT_MS,
        "downloadTotalTimeoutMs",
        MAX_DOWNLOAD_TOTAL_TIMEOUT_MS,
      ),
    );
    const archiveDigest = await sha256RegularFile(archive, MAX_COMPRESSED_ARCHIVE_BYTES, signal);
    if (
      archiveDigest.size !== selected.artifact.archiveSizeBytes ||
      archiveDigest.sha256 !== selected.artifact.archiveSha256
    ) {
      throw new RuntimeResolutionError(
        "Downloaded native runtime archive does not match its exact size and SHA-256 manifest binding",
      );
    }

    await mkdir(extracted, { mode: 0o700 });
    await extractTarGz(archive, extracted, selected.artifact, {
      maxUncompressedBytes: boundedPositiveInteger(
        dependencies.maxUncompressedArchiveBytes ?? MAX_UNCOMPRESSED_ARCHIVE_BYTES,
        "maxUncompressedArchiveBytes",
        MAX_UNCOMPRESSED_ARCHIVE_BYTES,
      ),
      maxMembers: boundedPositiveInteger(
        dependencies.maxArchiveMembers ?? MAX_ARCHIVE_MEMBERS,
        "maxArchiveMembers",
        MAX_ARCHIVE_MEMBERS,
      ),
    }, signal);
    const extractedExecutable = cacheExecutable(extractedRoot, selected.artifact);
    const binaryDigest = await sha256RegularFile(extractedExecutable, MAX_ARCHIVE_MEMBER_BYTES, signal);
    if (binaryDigest.size < 1 || binaryDigest.sha256 !== selected.artifact.executableSha256) {
      throw new RuntimeResolutionError(
        "Extracted native runtime executable does not match its SHA-256 manifest binding",
      );
    }
    throwIfAborted(signal);
    if (process.platform !== "win32") await chmod(extractedExecutable, 0o755);
    await writeFile(join(extractedRoot, CACHE_MARKER), expectedMarker, {
      encoding: "utf8",
      flag: "wx",
      mode: 0o600,
    });

    throwIfAborted(signal);
    await mkdir(dirname(cacheEntry), { recursive: true, mode: 0o700 });
    try {
      await lstat(cacheEntry);
      quarantine = `${cacheEntry}.invalid-${randomUUID()}`;
      await rename(cacheEntry, quarantine);
    } catch (error) {
      if (!isMissing(error)) throw error;
    }
    try {
      throwIfAborted(signal);
      await rename(extractedRoot, cacheEntry);
    } catch (error) {
      if (quarantine !== undefined) {
        await rename(quarantine, cacheEntry).catch(() => undefined);
        quarantine = undefined;
      }
      throw error;
    }
    if (quarantine !== undefined) {
      await rm(quarantine, { recursive: true, force: true });
      quarantine = undefined;
    }

    const executable = cacheExecutable(cacheEntry, selected.artifact);
    if (!(await cacheEntryIsValid(cacheEntry, executable, selected.artifact, expectedMarker, signal))) {
      throw new RuntimeResolutionError("The atomically installed native runtime failed cache verification");
    }
    return executable;
  } finally {
    await rm(temporary, { recursive: true, force: true });
    if (quarantine !== undefined) {
      await rm(quarantine, { recursive: true, force: true }).catch(() => undefined);
    }
  }
}

async function downloadWithDeadline(
  download: NonNullable<RuntimeResolverDependencies["downloadArchive"]>,
  artifact: RuntimeArtifactManifest,
  destination: string,
  signal: AbortSignal | undefined,
  timeoutMs: number,
): Promise<void> {
  throwIfAborted(signal);
  const controller = new AbortController();
  const deadlineError = new RuntimeResolutionError(
    `Native runtime download exceeded its ${String(timeoutMs)} ms total deadline`,
  );
  let deadlineExpired = false;
  const forwardAbort = (): void => controller.abort(signal?.reason);
  signal?.addEventListener("abort", forwardAbort, { once: true });
  if (signal?.aborted === true) forwardAbort();
  const deadline = globalThis.setTimeout(() => {
    deadlineExpired = true;
    controller.abort(deadlineError);
  }, timeoutMs);
  try {
    await download(artifact, destination, controller.signal);
    if (deadlineExpired) throw deadlineError;
    throwIfAborted(signal);
  } catch (error) {
    if (deadlineExpired) throw deadlineError;
    throw error;
  } finally {
    globalThis.clearTimeout(deadline);
    signal?.removeEventListener("abort", forwardAbort);
  }
}

async function downloadRuntimeArchive(
  artifact: RuntimeArtifactManifest,
  destination: string,
  signal: AbortSignal | undefined,
): Promise<void> {
  const initialUrl = new URL(artifact.archiveUrl);
  validateHttpsUrl(initialUrl, "native runtime archive URL");
  let response: IncomingMessage | undefined;
  let output: FileHandle | undefined;
  let completed = false;
  let size = 0;
  try {
    response = await requestWithRedirects(initialUrl, signal, 0);
    const contentLength = response.headers["content-length"];
    if (contentLength !== undefined) {
      if (!/^(0|[1-9][0-9]*)$/u.test(contentLength)) {
        throw new RuntimeResolutionError("Native runtime response has an invalid Content-Length");
      }
      const declaredLength = Number(contentLength);
      if (
        !Number.isSafeInteger(declaredLength) ||
        declaredLength !== artifact.archiveSizeBytes ||
        declaredLength > MAX_COMPRESSED_ARCHIVE_BYTES
      ) {
        throw new RuntimeResolutionError(
          "Native runtime response Content-Length does not match the exact manifest size",
        );
      }
    }

    throwIfAborted(signal);
    output = await open(
      destination,
      constants.O_CREAT | constants.O_EXCL | constants.O_WRONLY,
      0o600,
    );
    const activeResponse = response;
    activeResponse.setTimeout(DOWNLOAD_IDLE_TIMEOUT_MS, () => {
      activeResponse.destroy(new RuntimeResolutionError("Native runtime download became idle"));
    });
    for await (const value of activeResponse) {
      throwIfAborted(signal);
      const chunk = Buffer.isBuffer(value) ? value : Buffer.from(value as Uint8Array);
      size += chunk.byteLength;
      if (size > artifact.archiveSizeBytes || size > MAX_COMPRESSED_ARCHIVE_BYTES) {
        activeResponse.destroy();
        throw new RuntimeResolutionError("Native runtime download exceeds the exact manifest size");
      }
      await writeAll(output, chunk);
    }
    if (size !== artifact.archiveSizeBytes) {
      throw new RuntimeResolutionError("Native runtime download ended before its exact manifest size");
    }
    await output.sync();
    throwIfAborted(signal);
    await output.close();
    output = undefined;
    completed = true;
  } finally {
    response?.destroy();
    await output?.close().catch(() => undefined);
    if (!completed) {
      await unlink(destination).catch((error: unknown) => {
        if (!isMissing(error)) throw error;
      });
    }
  }
}

async function requestWithRedirects(
  url: URL,
  signal: AbortSignal | undefined,
  redirects: number,
): Promise<IncomingMessage> {
  validateHttpsUrl(url, "native runtime download URL");
  const response = await request(url, signal);
  if ([301, 302, 303, 307, 308].includes(response.statusCode ?? 0)) {
    const location = response.headers.location;
    response.destroy();
    if (typeof location !== "string" || redirects >= MAX_REDIRECTS) {
      throw new RuntimeResolutionError("Native runtime download has an invalid redirect chain");
    }
    const redirected = new URL(location, url);
    validateHttpsUrl(redirected, "native runtime redirect URL");
    return requestWithRedirects(redirected, signal, redirects + 1);
  }
  if (response.statusCode !== 200) {
    const statusCode = response.statusCode;
    response.destroy();
    throw new RuntimeResolutionError(
      `Native runtime download failed with HTTP status ${String(statusCode)}`,
    );
  }
  return response;
}

function request(url: URL, signal: AbortSignal | undefined): Promise<IncomingMessage> {
  return new Promise((resolveRequest, rejectRequest) => {
    const request = httpsGet(
      url,
      {
        headers: {
          Accept: "application/octet-stream",
          "User-Agent": `@oxhq/stasis runtime resolver`,
        },
        ...(signal === undefined ? {} : { signal }),
      },
      (response) => {
        globalThis.clearTimeout(deadline);
        resolveRequest(response);
      },
    );
    const deadline = globalThis.setTimeout(() => {
      request.destroy(new RuntimeResolutionError("Native runtime request did not receive headers before its deadline"));
    }, DOWNLOAD_IDLE_TIMEOUT_MS);
    request.once("error", (error) => {
      globalThis.clearTimeout(deadline);
      rejectRequest(error);
    });
  });
}

function validateHttpsUrl(url: URL, label: string): void {
  if (
    url.protocol !== "https:" ||
    url.username.length > 0 ||
    url.password.length > 0 ||
    url.hash.length > 0 ||
    (url.port.length > 0 && url.port !== "443")
  ) {
    throw new RuntimeResolutionError(`${label} must be an HTTPS URL without credentials or a fragment`);
  }
}

async function extractTarGz(
  archive: string,
  destination: string,
  artifact: RuntimeArtifactManifest,
  limits: { maxUncompressedBytes: number; maxMembers: number },
  signal?: AbortSignal,
): Promise<void> {
  const source = createReadStream(archive);
  const gunzip = createGunzip();
  source.once("error", (error) => gunzip.destroy(error));
  source.pipe(gunzip);
  const reader = new BoundedByteReader(
    gunzip[Symbol.asyncIterator](),
    Math.min(limits.maxUncompressedBytes, MAX_UNCOMPRESSED_ARCHIVE_BYTES),
    signal,
  );
  const paths = new Map<string, "directory" | "file">();
  const expectedEntries = expectedArchiveEntries(artifact);
  const missingEntries = new Set(expectedEntries.keys());
  let members = 0;
  let totalMemberBytes = 0;
  let foundExecutable = false;
  try {
    while (true) {
      const header = await reader.readExact(TAR_BLOCK_BYTES);
      if (header === null) {
        throw new RuntimeResolutionError("Native runtime tar stream ended before its end marker");
      }
      if (isZeroBlock(header)) {
        const secondEndBlock = await reader.readExact(TAR_BLOCK_BYTES);
        if (secondEndBlock === null || !isZeroBlock(secondEndBlock)) {
          throw new RuntimeResolutionError("Native runtime tar stream has an incomplete end marker");
        }
        await reader.requireOnlyZeroBytesRemain();
        break;
      }
      members += 1;
      if (members > Math.min(limits.maxMembers, MAX_ARCHIVE_MEMBERS)) {
        throw new RuntimeResolutionError("Native runtime archive exceeds the member-count limit");
      }
      const entry = parseTarHeader(header, artifact.archiveRoot);
      registerTarEntry(paths, entry);
      const entryKey = portableCollisionKey(entry.path);
      if (expectedEntries.get(entryKey) !== entry.type) {
        throw new RuntimeResolutionError(
          `Native runtime archive contains an unexpected ${entry.type}: ${entry.path}`,
        );
      }
      missingEntries.delete(entryKey);
      if (entry.type === "directory") {
        await mkdir(join(destination, ...entry.path.split("/")), { recursive: true, mode: 0o700 });
        continue;
      }
      totalMemberBytes += entry.size;
      if (
        entry.size > MAX_ARCHIVE_MEMBER_BYTES ||
        totalMemberBytes > Math.min(limits.maxUncompressedBytes, MAX_UNCOMPRESSED_ARCHIVE_BYTES)
      ) {
        throw new RuntimeResolutionError("Native runtime archive exceeds the member-size limit");
      }
      const filename = join(destination, ...entry.path.split("/"));
      await mkdir(dirname(filename), { recursive: true, mode: 0o700 });
      const output = await open(filename, constants.O_CREAT | constants.O_EXCL | constants.O_WRONLY, 0o600);
      try {
        let remaining = entry.size;
        while (remaining > 0) {
          const chunk = await reader.readUpTo(Math.min(remaining, IO_CHUNK_BYTES));
          if (chunk === null) {
            throw new RuntimeResolutionError("Native runtime archive contains a truncated member");
          }
          await writeAll(output, chunk);
          throwIfAborted(signal);
          remaining -= chunk.byteLength;
        }
        await output.sync();
      } finally {
        await output.close();
      }
      const padding = (TAR_BLOCK_BYTES - (entry.size % TAR_BLOCK_BYTES)) % TAR_BLOCK_BYTES;
      if (padding > 0) {
        const paddingBytes = await reader.readExact(padding);
        if (paddingBytes === null || !isZeroBlock(paddingBytes)) {
          throw new RuntimeResolutionError("Native runtime tar member has invalid padding");
        }
      }
      if (entry.path === `${artifact.archiveRoot}/${artifact.executablePath}`) {
        foundExecutable = true;
      }
    }
  } finally {
    source.destroy();
    gunzip.destroy();
  }
  if (missingEntries.size > 0) {
    throw new RuntimeResolutionError("Native runtime archive is missing manifest inventory entries");
  }
  if (!foundExecutable) {
    throw new RuntimeResolutionError("Native runtime archive is missing its manifest executable");
  }
}

class BoundedByteReader {
  readonly #iterator: AsyncIterator<unknown>;
  readonly #maximumBytes: number;
  readonly #signal: AbortSignal | undefined;
  #buffer = Buffer.alloc(0);
  #offset = 0;
  #observedBytes = 0;
  #done = false;

  constructor(iterator: AsyncIterator<unknown>, maximumBytes: number, signal?: AbortSignal) {
    this.#iterator = iterator;
    this.#maximumBytes = maximumBytes;
    this.#signal = signal;
  }

  async readExact(size: number): Promise<Buffer | null> {
    if (size === 0) return Buffer.alloc(0);
    const output = Buffer.allocUnsafe(size);
    let written = 0;
    while (written < size) {
      const chunk = await this.readUpTo(size - written);
      if (chunk === null) return written === 0 ? null : null;
      chunk.copy(output, written);
      written += chunk.byteLength;
    }
    return output;
  }

  async readUpTo(size: number): Promise<Buffer | null> {
    if (!Number.isSafeInteger(size) || size < 1) throw new RangeError("read size must be positive");
    while (this.#offset >= this.#buffer.byteLength) {
      if (this.#done) return null;
      throwIfAborted(this.#signal);
      const next = await this.#iterator.next();
      if (next.done === true) {
        this.#done = true;
        return null;
      }
      const chunk = Buffer.isBuffer(next.value)
        ? next.value
        : Buffer.from(next.value as Uint8Array);
      if (chunk.byteLength === 0) continue;
      this.#observedBytes += chunk.byteLength;
      if (this.#observedBytes > this.#maximumBytes) {
        throw new RuntimeResolutionError("Native runtime archive exceeds the decompression limit");
      }
      this.#buffer = Buffer.from(chunk);
      this.#offset = 0;
    }
    const length = Math.min(size, this.#buffer.byteLength - this.#offset);
    const result = this.#buffer.subarray(this.#offset, this.#offset + length);
    this.#offset += length;
    return result;
  }

  async requireOnlyZeroBytesRemain(): Promise<void> {
    while (true) {
      const chunk = await this.readUpTo(IO_CHUNK_BYTES);
      if (chunk === null) return;
      if (!isZeroBlock(chunk)) {
        throw new RuntimeResolutionError("Native runtime tar stream has data after its end marker");
      }
    }
  }
}

function parseTarHeader(header: Buffer, expectedRoot: string): TarEntry {
  if (header.byteLength !== TAR_BLOCK_BYTES) {
    throw new RuntimeResolutionError("Native runtime tar header has an invalid size");
  }
  const storedChecksum = parseTarOctal(header.subarray(148, 156), "header checksum");
  let computedChecksum = 0;
  for (let index = 0; index < header.byteLength; index += 1) {
    computedChecksum += index >= 148 && index < 156 ? 0x20 : (header[index] ?? 0);
  }
  if (storedChecksum !== computedChecksum) {
    throw new RuntimeResolutionError("Native runtime tar header checksum is invalid");
  }
  const magic = header.subarray(257, 263);
  if (!magic.equals(Buffer.from("ustar\0", "ascii"))) {
    throw new RuntimeResolutionError("Native runtime archive must use the ustar format");
  }
  const name = parseTarString(header.subarray(0, 100), "member name");
  const prefix = parseTarString(header.subarray(345, 500), "member prefix", true);
  const combined = prefix.length === 0 ? name : `${prefix}/${name}`;
  const typeByte = header[156] ?? 0;
  const type = typeByte === 0 || typeByte === 0x30 ? "file" : typeByte === 0x35 ? "directory" : null;
  if (type === null) {
    throw new RuntimeResolutionError("Native runtime archive contains a link or unsupported entry");
  }
  const path = type === "directory" && combined.endsWith("/") ? combined.slice(0, -1) : combined;
  validateArchivePath(path, "archive member path");
  if (path !== expectedRoot && !path.startsWith(`${expectedRoot}/`)) {
    throw new RuntimeResolutionError("Native runtime archive contains a member outside its exact root");
  }
  const size = parseTarOctal(header.subarray(124, 136), "member size");
  if (!Number.isSafeInteger(size) || size < 0 || (type === "directory" && size !== 0)) {
    throw new RuntimeResolutionError("Native runtime archive member has an invalid size");
  }
  return { path, type, size };
}

function registerTarEntry(
  entries: Map<string, "directory" | "file">,
  entry: TarEntry,
): void {
  const collisionKey = portableCollisionKey(entry.path);
  if (entries.has(collisionKey)) {
    throw new RuntimeResolutionError("Native runtime archive contains duplicate member paths");
  }
  const components = collisionKey.split("/");
  for (let index = 1; index < components.length; index += 1) {
    if (entries.get(components.slice(0, index).join("/")) === "file") {
      throw new RuntimeResolutionError("Native runtime archive uses a file as a parent directory");
    }
  }
  if (entry.type === "file") {
    for (const existing of entries.keys()) {
      if (existing.startsWith(`${collisionKey}/`)) {
        throw new RuntimeResolutionError("Native runtime archive uses a file as a parent directory");
      }
    }
  }
  entries.set(collisionKey, entry.type);
}

function expectedArchiveEntries(
  artifact: RuntimeArtifactManifest,
): Map<string, "directory" | "file"> {
  const entries = new Map<string, "directory" | "file">();
  entries.set(portableCollisionKey(artifact.archiveRoot), "directory");
  for (const relativeFilename of artifact.archiveFiles) {
    const components = relativeFilename.split("/");
    for (let index = 1; index < components.length; index += 1) {
      const directory = `${artifact.archiveRoot}/${components.slice(0, index).join("/")}`;
      entries.set(portableCollisionKey(directory), "directory");
    }
    entries.set(
      portableCollisionKey(`${artifact.archiveRoot}/${relativeFilename}`),
      "file",
    );
  }
  return entries;
}

function portableCollisionKey(value: string): string {
  return value.normalize("NFC").toLocaleLowerCase("en-US");
}

function parseTarString(field: Buffer, label: string, allowEmpty = false): string {
  const terminator = field.indexOf(0);
  const valueBytes = terminator === -1 ? field : field.subarray(0, terminator);
  if (terminator !== -1 && field.subarray(terminator).some((byte) => byte !== 0)) {
    throw new RuntimeResolutionError(`Native runtime tar ${label} is not canonically terminated`);
  }
  if (valueBytes.some((byte) => byte < 0x20 || byte > 0x7e)) {
    throw new RuntimeResolutionError(`Native runtime tar ${label} is not printable ASCII`);
  }
  const value = valueBytes.toString("ascii");
  if (!allowEmpty && value.length === 0) {
    throw new RuntimeResolutionError(`Native runtime tar ${label} is empty`);
  }
  return value;
}

function parseTarOctal(field: Buffer, label: string): number {
  if (field.some((byte) => byte > 0x7f)) {
    throw new RuntimeResolutionError(`Native runtime tar ${label} uses an unsupported encoding`);
  }
  const value = field.toString("ascii").replace(/[\u0000 ]+$/u, "").replace(/^ +/u, "");
  if (!/^[0-7]+$/u.test(value)) {
    throw new RuntimeResolutionError(`Native runtime tar ${label} is not canonical octal`);
  }
  const parsed = Number.parseInt(value, 8);
  if (!Number.isSafeInteger(parsed)) {
    throw new RuntimeResolutionError(`Native runtime tar ${label} exceeds the safe integer range`);
  }
  return parsed;
}

function validateArchivePath(value: string, label: string): void {
  if (
    typeof value !== "string" ||
    value.length === 0 ||
    value.includes("\\") ||
    value.startsWith("/") ||
    posix.isAbsolute(value) ||
    posix.normalize(value) !== value
  ) {
    throw new RuntimeResolutionError(`The ${label} is not a canonical relative path`);
  }
  const components = value.split("/");
  if (components.some((component) => component.length === 0 || component === "." || component === "..")) {
    throw new RuntimeResolutionError(`The ${label} contains an unsafe component`);
  }
  for (const component of components) {
    if (
      WINDOWS_FORBIDDEN_PATTERN.test(component) ||
      component.endsWith(".") ||
      component.endsWith(" ") ||
      WINDOWS_RESERVED_NAMES.has((component.split(".")[0] ?? "").toLocaleLowerCase("en-US"))
    ) {
      throw new RuntimeResolutionError(`The ${label} is not portable across supported filesystems`);
    }
  }
}

async function sha256RegularFile(
  filename: string,
  maximumBytes: number,
  signal?: AbortSignal,
): Promise<FileDigest> {
  const handle = await open(filename, secureCacheReadFlags());
  try {
    const metadata = await handle.stat();
    if (!metadata.isFile() || metadata.size < 0 || metadata.size > maximumBytes) {
      throw new RuntimeResolutionError(`Expected a bounded regular file at ${filename}`);
    }
    const hash = createHash("sha256");
    const buffer = Buffer.allocUnsafe(IO_CHUNK_BYTES);
    let position = 0;
    while (position < metadata.size) {
      throwIfAborted(signal);
      const { bytesRead } = await handle.read(
        buffer,
        0,
        Math.min(buffer.byteLength, metadata.size - position),
        position,
      );
      if (bytesRead === 0) {
        throw new RuntimeResolutionError(`File ended while hashing ${filename}`);
      }
      hash.update(buffer.subarray(0, bytesRead));
      position += bytesRead;
    }
    const after = await handle.stat();
    if (!sameOpenedFile(metadata, after)) {
      throw new RuntimeResolutionError(`File changed while hashing ${filename}`);
    }
    return { sha256: hash.digest("hex"), size: metadata.size };
  } finally {
    await handle.close();
  }
}

async function exactRegularFileMatches(filename: string, expected: Buffer): Promise<boolean> {
  const handle = await open(filename, secureCacheReadFlags());
  try {
    const before = await handle.stat();
    if (!before.isFile() || before.size !== expected.byteLength) return false;
    const observed = await readAtMost(handle, expected.byteLength + 1);
    const after = await handle.stat();
    return (
      sameOpenedFile(before, after) &&
      observed.byteLength === expected.byteLength &&
      observed.equals(expected)
    );
  } finally {
    await handle.close();
  }
}

async function readBoundedRegularUtf8(filename: string, maximumBytes: number): Promise<string> {
  const handle = await open(filename, secureCacheReadFlags());
  try {
    const before = await handle.stat();
    if (!before.isFile() || before.size < 0 || before.size > maximumBytes) {
      throw new RuntimeResolutionError(`Expected a bounded regular file at ${filename}`);
    }
    const observed = await readAtMost(handle, before.size + 1);
    const after = await handle.stat();
    if (!sameOpenedFile(before, after) || observed.byteLength !== before.size) {
      throw new RuntimeResolutionError(`File changed while reading ${filename}`);
    }
    return observed.toString("utf8");
  } finally {
    await handle.close();
  }
}

async function readAtMost(handle: FileHandle, maximumBytes: number): Promise<Buffer> {
  const output = Buffer.allocUnsafe(maximumBytes);
  let offset = 0;
  while (offset < maximumBytes) {
    const { bytesRead } = await handle.read(output, offset, maximumBytes - offset, offset);
    if (bytesRead === 0) break;
    offset += bytesRead;
  }
  return output.subarray(0, offset);
}

function sameOpenedFile(before: Awaited<ReturnType<FileHandle["stat"]>>, after: Awaited<ReturnType<FileHandle["stat"]>>): boolean {
  return (
    after.isFile() &&
    after.dev === before.dev &&
    after.ino === before.ino &&
    after.size === before.size &&
    after.mtimeMs === before.mtimeMs &&
    after.ctimeMs === before.ctimeMs
  );
}

function secureCacheReadFlags(): number {
  if (typeof constants.O_NOFOLLOW !== "number" || typeof constants.O_NONBLOCK !== "number") {
    throw new RuntimeResolutionError(
      "This platform cannot safely open managed-runtime cache files without following links or blocking",
    );
  }
  return constants.O_RDONLY | constants.O_NOFOLLOW | constants.O_NONBLOCK;
}

async function writeAll(handle: FileHandle, buffer: Buffer): Promise<void> {
  let offset = 0;
  while (offset < buffer.byteLength) {
    const { bytesWritten } = await handle.write(buffer, offset, buffer.byteLength - offset, null);
    if (bytesWritten === 0) throw new RuntimeResolutionError("Could not write native runtime data");
    offset += bytesWritten;
  }
}

function isZeroBlock(buffer: Buffer): boolean {
  return buffer.every((byte) => byte === 0);
}

function requireSafeComponent(value: string, label: string): void {
  if (!SAFE_COMPONENT_PATTERN.test(value)) {
    throw new RuntimeResolutionError(`${label} is not a safe cache component`);
  }
}

function requireSha256(value: string, label: string): void {
  if (!SHA256_PATTERN.test(value)) throw new RuntimeResolutionError(`${label} is invalid`);
}

function boundedPositiveInteger(value: number, label: string, maximum: number): number {
  if (!Number.isSafeInteger(value) || value < 1 || value > maximum) {
    throw new RangeError(`${label} must be a positive safe integer no greater than ${maximum}`);
  }
  return value;
}

function throwIfAborted(signal: AbortSignal | undefined): void {
  if (signal?.aborted === true) throw new StasisAbortError(signal.reason);
}

function errorCode(error: unknown): string | undefined {
  return typeof error === "object" && error !== null && "code" in error
    ? String((error as NodeJS.ErrnoException).code)
    : undefined;
}

function isMissing(error: unknown): boolean {
  return errorCode(error) === "ENOENT";
}

function isAlreadyExists(error: unknown): boolean {
  return errorCode(error) === "EEXIST";
}

function isNotDirectory(error: unknown): boolean {
  return errorCode(error) === "ENOTDIR";
}

function isSymbolicLoop(error: unknown): boolean {
  return errorCode(error) === "ELOOP";
}
