import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { gzipSync } from "node:zlib";
import {
  access,
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  unlink,
  utimes,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { setTimeout as delay } from "node:timers/promises";
import test, { type TestContext } from "node:test";

import { StasisAbortError } from "../src/errors.js";
import {
  RuntimeResolutionError,
  assertManagedRuntimeIdentity,
  resolveRuntimeExecutableForTesting as resolveRuntimeExecutable,
  type RuntimeResolverDependencies,
} from "../src/runtime-resolver.js";
import type {
  RuntimeArtifactManifest,
  RuntimeDistributionManifest,
} from "../src/runtime-manifest.js";
import type { RuntimeInfo } from "../src/types.js";

const VERSION = "0.1.0-test.0";
const PLATFORM_KEY = "darwin-arm64";
const ROOT = `stasis-${VERSION}-macos-aarch64`;
const BINARY = Buffer.from("#!/bin/sh\nprintf 'fixture runtime\\n'\n", "utf8");
const MANAGED_RUNTIME_CACHE_TEST_OPTIONS = {
  skip:
    process.platform === "win32"
      ? "Windows supports only explicit executablePath; managed-runtime cache installation is unsupported"
      : false,
};

function managedRuntimeCacheTest(
  title: string,
  body: (context: TestContext) => void | Promise<void>,
): Promise<void> {
  return test(title, MANAGED_RUNTIME_CACHE_TEST_OPTIONS, body);
}

interface TarFixtureMember {
  name: string;
  type?: "file" | "directory" | "symlink";
  content?: Buffer;
}

interface ResolverFixture {
  root: string;
  cache: string;
  archive: string;
  manifest: RuntimeDistributionManifest;
  dependencies: RuntimeResolverDependencies;
  downloads: { count: number };
}

async function createResolverFixture(
  context: { after(callback: () => void | Promise<void>): void },
  members: TarFixtureMember[] = [
    { name: ROOT, type: "directory" },
    { name: `${ROOT}/stasis`, content: BINARY },
    { name: `${ROOT}/LICENSE`, content: Buffer.from("fixture license\n", "utf8") },
  ],
): Promise<ResolverFixture> {
  const root = await mkdtemp(join(tmpdir(), "stasis-runtime-resolver-test-"));
  context.after(() => rm(root, { recursive: true, force: true }));
  const cache = join(root, "cache");
  const archive = join(root, "runtime.tar.gz");
  const archiveBytes = createTarGz(members);
  await writeFile(archive, archiveBytes, { flag: "wx", mode: 0o600 });
  const artifact: RuntimeArtifactManifest = {
    nodePlatform: "darwin",
    nodeArch: "arm64",
    releasePlatform: "macos-aarch64",
    archiveUrl: "https://example.test/stasis-runtime.tar.gz",
    archiveSizeBytes: archiveBytes.byteLength,
    archiveSha256: sha256(archiveBytes),
    archiveRoot: ROOT,
    archiveFiles: ["LICENSE", "stasis"],
    executablePath: "stasis",
    executableSha256: sha256(BINARY),
  };
  const manifest: RuntimeDistributionManifest = {
    schema: 1,
    packageName: "@oxhq/stasis",
    sdkVersion: VERSION,
    releaseTag: `v${VERSION}`,
    implementation: {
      name: "stasis-shell",
      source: {
        stasis_repository: "https://github.com/oxhq/stasis.git",
        stasis_revision: "1".repeat(40),
      },
    },
    artifacts: { [PLATFORM_KEY]: artifact },
  };
  const downloads = { count: 0 };
  const dependencies: RuntimeResolverDependencies = {
    manifest,
    platform: "darwin",
    architecture: "arm64",
    lockWaitMs: 5_000,
    lockPollMs: 10,
    downloadArchive: async (_selected, destination) => {
      downloads.count += 1;
      await copyFile(archive, destination);
    },
  };
  return { root, cache, archive, manifest, dependencies, downloads };
}

managedRuntimeCacheTest("resolver installs exact bytes atomically, reuses cache, and repairs corruption", async (context) => {
  const fixture = await createResolverFixture(context);
  const executable = await resolveRuntimeExecutable(
    VERSION,
    { cacheDirectory: fixture.cache },
    fixture.dependencies,
  );
  assert.match(executable, new RegExp(fixture.manifest.artifacts[PLATFORM_KEY]!.archiveSha256, "u"));
  assert.deepEqual(await readFile(executable), BINARY);
  assert.equal(fixture.downloads.count, 1);

  assert.equal(
    await resolveRuntimeExecutable(
      VERSION,
      { cacheDirectory: fixture.cache },
      fixture.dependencies,
    ),
    executable,
  );
  assert.equal(fixture.downloads.count, 1);

  await writeFile(executable, "corrupt", "utf8");
  assert.equal(
    await resolveRuntimeExecutable(
      VERSION,
      { cacheDirectory: fixture.cache },
      fixture.dependencies,
    ),
    executable,
  );
  assert.deepEqual(await readFile(executable), BINARY);
  assert.equal(fixture.downloads.count, 2);
});

managedRuntimeCacheTest("concurrent resolvers share one exclusive cache installation", async (context) => {
  const fixture = await createResolverFixture(context);
  const originalDownload = fixture.dependencies.downloadArchive!;
  fixture.dependencies.downloadArchive = async (...arguments_) => {
    await delay(50);
    await originalDownload(...arguments_);
  };
  const executables = await Promise.all(
    Array.from({ length: 4 }, () =>
      resolveRuntimeExecutable(
        VERSION,
        { cacheDirectory: fixture.cache },
        fixture.dependencies,
      ),
    ),
  );
  assert.equal(new Set(executables).size, 1);
  assert.equal(fixture.downloads.count, 1);
  assert.deepEqual(await readFile(executables[0]!), BINARY);
});

managedRuntimeCacheTest("primary cache lock is published only after complete owner metadata", async (context) => {
  const fixture = await createResolverFixture(context);
  let candidateOwner: unknown;
  let lockFilename = "";
  await assert.rejects(
    resolveRuntimeExecutable(
      VERSION,
      { cacheDirectory: fixture.cache },
      {
        ...fixture.dependencies,
        beforePrimaryLockPublish: async (candidate, destination) => {
          lockFilename = destination;
          await assert.rejects(access(destination));
          candidateOwner = JSON.parse(await readFile(candidate, "utf8")) as unknown;
          throw new Error("simulated crash before atomic lock publication");
        },
      },
    ),
    /simulated crash/u,
  );
  assert.deepEqual(Object.keys(candidateOwner as object).sort(), [
    "createdAtMs",
    "nonce",
    "pid",
    "schema",
  ]);
  await assert.rejects(access(lockFilename));

  const executable = await resolveRuntimeExecutable(
    VERSION,
    { cacheDirectory: fixture.cache },
    fixture.dependencies,
  );
  assert.deepEqual(await readFile(executable), BINARY);
});

test(
  "cache FIFOs fail closed without blocking marker or executable validation",
  { ...MANAGED_RUNTIME_CACHE_TEST_OPTIONS, timeout: 5_000 },
  async (context) => {
    const fixture = await createResolverFixture(context);
    let executable = await resolveRuntimeExecutable(
      VERSION,
      { cacheDirectory: fixture.cache },
      fixture.dependencies,
    );
    const cacheEntry = dirname(executable);

    const marker = join(cacheEntry, ".stasis-runtime.json");
    await unlink(marker);
    execFileSync("mkfifo", [marker]);
    executable = await resolveRuntimeExecutable(
      VERSION,
      { cacheDirectory: fixture.cache },
      fixture.dependencies,
    );
    assert.deepEqual(await readFile(executable), BINARY);

    await unlink(executable);
    execFileSync("mkfifo", [executable]);
    await assert.rejects(
      resolveRuntimeExecutable(
        VERSION,
        { cacheDirectory: fixture.cache },
        fixture.dependencies,
      ),
      /Expected a bounded regular file/u,
    );
    assert.equal(fixture.downloads.count, 2);
  },
);

managedRuntimeCacheTest("absolute archive deadline aborts the transfer and leaves no cache entry", async (context) => {
  const fixture = await createResolverFixture(context);
  let observedAbort = false;
  await assert.rejects(
    resolveRuntimeExecutable(
      VERSION,
      { cacheDirectory: fixture.cache },
      {
        ...fixture.dependencies,
        downloadTotalTimeoutMs: 25,
        downloadArchive: async (_artifact, _destination, signal) => {
          await new Promise<never>((_resolve, reject) => {
            const abort = (): void => {
              observedAbort = true;
              reject(signal?.reason);
            };
            signal?.addEventListener("abort", abort, { once: true });
            if (signal?.aborted === true) abort();
          });
        },
      },
    ),
    /exceeded its 25 ms total deadline/u,
  );
  assert.equal(observedAbort, true);
  const artifact = fixture.manifest.artifacts[PLATFORM_KEY]!;
  const cacheEntry = join(
    fixture.cache,
    "runtime-v1",
    VERSION,
    PLATFORM_KEY,
    artifact.archiveSha256,
  );
  await assert.rejects(access(cacheEntry));
});

managedRuntimeCacheTest("resolver safely reclaims a stale cache lock owned by a dead process", async (context) => {
  const fixture = await createResolverFixture(context);
  const artifact = fixture.manifest.artifacts[PLATFORM_KEY]!;
  const lockDirectory = join(fixture.cache, "runtime-v1", ".locks");
  const lockFilename = join(
    lockDirectory,
    `${VERSION}-${PLATFORM_KEY}-${artifact.archiveSha256}.lock`,
  );
  await mkdir(lockDirectory, { recursive: true });
  await writeFile(
    lockFilename,
    `${JSON.stringify({
      schema: 1,
      pid: 99_999_999,
      createdAtMs: Date.now() - 10_000,
      nonce: "00000000-0000-4000-8000-000000000000",
    })}\n`,
    { flag: "wx", mode: 0o600 },
  );
  const old = new Date(Date.now() - 10_000);
  await utimes(lockFilename, old, old);

  const executable = await resolveRuntimeExecutable(
    VERSION,
    { cacheDirectory: fixture.cache },
    { ...fixture.dependencies, staleLockMs: 1 },
  );
  assert.deepEqual(await readFile(executable), BINARY);
  assert.equal(fixture.downloads.count, 1);
  await assert.rejects(access(lockFilename));
});

managedRuntimeCacheTest("resolver safely reclaims an orphaned stale reclaim lock", async (context) => {
  const fixture = await createResolverFixture(context);
  const artifact = fixture.manifest.artifacts[PLATFORM_KEY]!;
  const lockDirectory = join(fixture.cache, "runtime-v1", ".locks");
  const lockFilename = join(
    lockDirectory,
    `${VERSION}-${PLATFORM_KEY}-${artifact.archiveSha256}.lock`,
  );
  const deadOwner = `${JSON.stringify({
    schema: 1,
    pid: 99_999_999,
    createdAtMs: Date.now() - 10_000,
    nonce: "00000000-0000-4000-8000-000000000000",
  })}\n`;
  await mkdir(lockDirectory, { recursive: true });
  await writeFile(lockFilename, deadOwner, { flag: "wx", mode: 0o600 });
  await writeFile(`${lockFilename}.reclaim`, deadOwner, { flag: "wx", mode: 0o600 });
  const old = new Date(Date.now() - 10_000);
  await utimes(lockFilename, old, old);
  await utimes(`${lockFilename}.reclaim`, old, old);

  const executable = await resolveRuntimeExecutable(
    VERSION,
    { cacheDirectory: fixture.cache },
    { ...fixture.dependencies, staleLockMs: 1 },
  );
  assert.deepEqual(await readFile(executable), BINARY);
  await assert.rejects(access(lockFilename));
  await assert.rejects(access(`${lockFilename}.reclaim`));
});

managedRuntimeCacheTest("resolver rejects archive and executable digest mismatches", async (context) => {
  const archiveMismatch = await createResolverFixture(context);
  const artifact = archiveMismatch.manifest.artifacts[PLATFORM_KEY]!;
  const badArchiveManifest: RuntimeDistributionManifest = {
    ...archiveMismatch.manifest,
    artifacts: {
      [PLATFORM_KEY]: { ...artifact, archiveSha256: "0".repeat(64) },
    },
  };
  await assert.rejects(
    resolveRuntimeExecutable(
      VERSION,
      { cacheDirectory: archiveMismatch.cache },
      { ...archiveMismatch.dependencies, manifest: badArchiveManifest },
    ),
    /archive does not match its exact size and SHA-256/u,
  );

  const binaryMismatch = await createResolverFixture(context);
  const binaryArtifact = binaryMismatch.manifest.artifacts[PLATFORM_KEY]!;
  const badBinaryManifest: RuntimeDistributionManifest = {
    ...binaryMismatch.manifest,
    artifacts: {
      [PLATFORM_KEY]: { ...binaryArtifact, executableSha256: "f".repeat(64) },
    },
  };
  await assert.rejects(
    resolveRuntimeExecutable(
      VERSION,
      { cacheDirectory: binaryMismatch.cache },
      { ...binaryMismatch.dependencies, manifest: badBinaryManifest },
    ),
    /executable does not match its SHA-256/u,
  );
});

managedRuntimeCacheTest("resolver rejects traversal and link entries before they escape extraction", async (context) => {
  const traversal = await createResolverFixture(context, [
    { name: ROOT, type: "directory" },
    { name: `${ROOT}/../escape`, content: Buffer.from("escape", "utf8") },
    { name: `${ROOT}/stasis`, content: BINARY },
  ]);
  await assert.rejects(
    resolveRuntimeExecutable(
      VERSION,
      { cacheDirectory: traversal.cache },
      traversal.dependencies,
    ),
    RuntimeResolutionError,
  );
  await assert.rejects(access(join(traversal.cache, "escape")));

  const link = await createResolverFixture(context, [
    { name: ROOT, type: "directory" },
    { name: `${ROOT}/stasis`, type: "symlink", content: Buffer.alloc(0) },
  ]);
  await assert.rejects(
    resolveRuntimeExecutable(VERSION, { cacheDirectory: link.cache }, link.dependencies),
    /link or unsupported entry/u,
  );
});

managedRuntimeCacheTest("resolver enforces bounded decompression and member counts", async (context) => {
  const fixture = await createResolverFixture(context);
  await assert.rejects(
    resolveRuntimeExecutable(
      VERSION,
      { cacheDirectory: fixture.cache },
      { ...fixture.dependencies, maxUncompressedArchiveBytes: 1_024 },
    ),
    /decompression limit|member-size limit/u,
  );

  const memberFixture = await createResolverFixture(context);
  await assert.rejects(
    resolveRuntimeExecutable(
      VERSION,
      { cacheDirectory: memberFixture.cache },
      { ...memberFixture.dependencies, maxArchiveMembers: 1 },
    ),
    /member-count limit/u,
  );
});

test(
  "Windows managed acquisition stays typed unsupported and names the explicit override",
  async (context) => {
    const fixture = await createResolverFixture(context);
    await assert.rejects(
      resolveRuntimeExecutable(
        VERSION,
        { cacheDirectory: fixture.cache },
        { ...fixture.dependencies, platform: "win32", architecture: "x64" },
      ),
      (error: unknown) => {
        assert.ok(error instanceof RuntimeResolutionError);
        assert.equal(
          error.message,
          `@oxhq/stasis@${VERSION} has no native runtime for win32-x64; pass executablePath to use an explicit compatible runtime`,
        );
        return true;
      },
    );
    assert.equal(fixture.downloads.count, 0);
  },
);

test("resolver fails closed on manifest, platform, URL, and abort mismatches", async (context) => {
  const fixture = await createResolverFixture(context);
  await assert.rejects(
    resolveRuntimeExecutable(
      "0.1.0-other.0",
      { cacheDirectory: fixture.cache },
      fixture.dependencies,
    ),
    /does not exactly match/u,
  );
  await assert.rejects(
    resolveRuntimeExecutable(
      VERSION,
      { cacheDirectory: fixture.cache },
      { ...fixture.dependencies, architecture: "x64" },
    ),
    /has no native runtime/u,
  );

  const artifact = fixture.manifest.artifacts[PLATFORM_KEY]!;
  const insecureManifest: RuntimeDistributionManifest = {
    ...fixture.manifest,
    artifacts: {
      [PLATFORM_KEY]: { ...artifact, archiveUrl: "http://example.test/runtime.tar.gz" },
    },
  };
  await assert.rejects(
    resolveRuntimeExecutable(
      VERSION,
      { cacheDirectory: fixture.cache },
      { ...fixture.dependencies, manifest: insecureManifest },
    ),
    /must be an HTTPS URL/u,
  );

  const controller = new AbortController();
  controller.abort("not now");
  await assert.rejects(
    resolveRuntimeExecutable(
      VERSION,
      { cacheDirectory: fixture.cache, signal: controller.signal },
      fixture.dependencies,
    ),
    StasisAbortError,
  );
  assert.equal(fixture.downloads.count, 0);
});

test("managed runtime identity is bound to the exact SDK source manifest", async (context) => {
  const fixture = await createResolverFixture(context);
  const source = fixture.manifest.implementation.source;
  const runtime: RuntimeInfo = {
    protocolVersion: 1,
    implementation: { name: "stasis-shell", version: VERSION, source: { ...source } },
    capabilities: {
      methods: [],
      clockModes: [],
      profiles: [],
      settlement: true,
      settlementLimits: [],
    },
    limits: { maxInboundFrameBytes: 1, maxActiveEngineRequests: 1 },
  };
  assert.doesNotThrow(() => assertManagedRuntimeIdentity(VERSION, runtime, fixture.manifest));
  assert.throws(
    () =>
      assertManagedRuntimeIdentity(
        VERSION,
        {
          ...runtime,
          implementation: {
            ...runtime.implementation,
            source: { ...source, stasis_revision: "2".repeat(40) },
          },
        },
        fixture.manifest,
      ),
    RuntimeResolutionError,
  );
});

function createTarGz(members: readonly TarFixtureMember[]): Buffer {
  const blocks: Buffer[] = [];
  for (const member of members) {
    const type = member.type ?? "file";
    const content = member.content ?? Buffer.alloc(0);
    const header = Buffer.alloc(512);
    writeTarString(header, 0, 100, member.name);
    writeTarOctal(header, 100, 8, type === "directory" ? 0o755 : 0o644);
    writeTarOctal(header, 108, 8, 0);
    writeTarOctal(header, 116, 8, 0);
    writeTarOctal(header, 124, 12, type === "file" ? content.byteLength : 0);
    writeTarOctal(header, 136, 12, 0);
    header.fill(0x20, 148, 156);
    header[156] = type === "directory" ? 0x35 : type === "symlink" ? 0x32 : 0x30;
    Buffer.from("ustar\0", "ascii").copy(header, 257);
    Buffer.from("00", "ascii").copy(header, 263);
    const checksum = header.reduce((sum, byte) => sum + byte, 0);
    const checksumText = `${checksum.toString(8).padStart(6, "0")}\0 `;
    Buffer.from(checksumText, "ascii").copy(header, 148);
    blocks.push(header);
    if (type === "file") {
      blocks.push(content);
      const padding = (512 - (content.byteLength % 512)) % 512;
      if (padding > 0) blocks.push(Buffer.alloc(padding));
    }
  }
  blocks.push(Buffer.alloc(1024));
  return gzipSync(Buffer.concat(blocks), { level: 9 });
}

function writeTarString(buffer: Buffer, offset: number, length: number, value: string): void {
  const encoded = Buffer.from(value, "ascii");
  if (encoded.byteLength >= length) throw new RangeError(`tar value is too long: ${value}`);
  encoded.copy(buffer, offset);
}

function writeTarOctal(
  buffer: Buffer,
  offset: number,
  length: number,
  value: number,
): void {
  const encoded = `${value.toString(8).padStart(length - 1, "0")}\0`;
  Buffer.from(encoded, "ascii").copy(buffer, offset);
}

function sha256(value: Buffer): string {
  return createHash("sha256").update(value).digest("hex");
}
