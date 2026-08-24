export interface RuntimeArtifactManifest {
  /** Node.js platform/architecture pair selecting this artifact. */
  readonly nodePlatform: NodeJS.Platform;
  readonly nodeArch: string;
  /** Stable release-platform label used in the native archive name. */
  readonly releasePlatform: string;
  readonly archiveUrl: string;
  readonly archiveSizeBytes: number;
  readonly archiveSha256: string;
  readonly archiveRoot: string;
  /** Exact regular-file inventory relative to archiveRoot. */
  readonly archiveFiles: readonly string[];
  readonly executablePath: string;
  readonly executableSha256: string;
}

export interface RuntimeDistributionManifest {
  readonly schema: 1;
  readonly packageName: "@oxhq/stasis";
  /** Must equal the installed SDK package version exactly. */
  readonly sdkVersion: string;
  readonly releaseTag: string;
  readonly implementation: {
    readonly name: "stasis-shell";
    readonly source: Readonly<Record<string, string>>;
  };
  readonly artifacts: Readonly<Record<string, RuntimeArtifactManifest>>;
}

export { RUNTIME_DISTRIBUTION_MANIFEST } from "./runtime-manifest.generated.js";

/**
 * Native bytes bound to this exact SDK release. Release automation updates this
 * table only after every listed archive and executable digest has been gated and
 * attested. Runtime acquisition fails closed when the installed SDK version or
 * host platform is absent from this manifest.
 */
export function runtimePlatformKey(
  platform: NodeJS.Platform = process.platform,
  architecture: string = process.arch,
): string {
  return `${platform}-${architecture}`;
}
