/*
 * Fail-closed source-tree placeholder: these are the immutable historical
 * v0.1.0-alpha.0 bytes, not a v0.3.1 distribution manifest. The gated release
 * workflow must replace this module with the exact generated v0.3.1 manifest
 * before packing; local prepack intentionally rejects this placeholder.
 */
import type { RuntimeDistributionManifest } from "./runtime-manifest.js";

// The first alpha archive predates the stable ten-member archive contract.
export const RUNTIME_DISTRIBUTION_MANIFEST = {
  schema: 1,
  packageName: "@oxhq/stasis",
  sdkVersion: "0.1.0-alpha.0",
  releaseTag: "v0.1.0-alpha.0",
  implementation: {
    name: "stasis-shell",
    source: {
      servo_repository: "https://github.com/servo/servo.git",
      servo_revision: "0d579bd5aab6df3764fad805427254751632a6e4",
      pliego_repository: "https://github.com/oxhq/pliego.git",
      pliego_revision: "556c774242b272b11bc60999449c5debff1ad20f",
      pliego_servo_merge_base: "313b6d5ecc113b08010ce434140db3ca5abcc71c",
      stasis_repository: "https://github.com/oxhq/stasis.git",
      stasis_revision: "c5b8591d4d7dbe1b2da2f916409867cb18256470"
    }
  },
  artifacts: {
    "darwin-arm64": {
      nodePlatform: "darwin",
      nodeArch: "arm64",
      releasePlatform: "macos-aarch64",
      archiveUrl: "https://github.com/oxhq/stasis/releases/download/v0.1.0-alpha.0/stasis-0.1.0-alpha.0-macos-aarch64.tar.gz",
      archiveSizeBytes: 30077840,
      archiveSha256: "399fa14e506332c816292be5ff3423455c0947547259186c1e91bd70fc5070d9",
      archiveRoot: "stasis-0.1.0-alpha.0-macos-aarch64",
      archiveFiles: [
        "INSTALL.txt",
        "LICENSE",
        "LICENSE_WHATWG_SPECS",
        "SOURCE.txt",
        "STASIS_UPSTREAM.toml",
        "THIRD_PARTY_LICENSES.html",
        "VERSION.txt",
        "stasis"
      ],
      executablePath: "stasis",
      executableSha256: "3cd7a759d46b42e8191a17c8bb9191af9672babb7dd08512d8ccd29a13e1c567"
    }
  }
} as const satisfies RuntimeDistributionManifest;
