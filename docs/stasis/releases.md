# Stasis 0.1 releases

`v0.1.0` is the first supported, non-prerelease Stasis release. It binds one
reviewed source commit to two native runtimes, one generated runtime distribution
manifest, and one byte-exact npm package.

The published `v0.1.0-alpha.0` tag, GitHub release, native assets, npm version,
and `alpha` dist-tag are immutable historical inputs. The stable train does not
replace, retarget, or rebuild them.

## Supported distribution surface

The stable native matrix is deliberately small:

| Release platform | GitHub runner | Compatibility contract |
| --- | --- | --- |
| `macos-aarch64` | `macos-15` | Native Apple Silicon; unsigned and not notarized |
| `linux-x86_64` | `ubuntu-22.04` | x86-64 GNU/Linux with glibc 2.35 or newer |

Windows, macOS Intel, signing/notarization, and cross-compiler byte
reproducibility are not 0.1 claims. Each release archive is deterministic in its
container metadata, but its executable is bound by digest and provenance rather
than by a claim that a second compiler invocation will reproduce identical
bytes.

Each archive contains one normalized root directory and exactly ten regular
files:

- `stasis`
- `INSTALL.txt`
- `LICENSE`
- `LICENSE_WHATWG_SPECS`
- `NATIVE-LIBRARIES.txt`
- `README.md`
- `SOURCE.txt`
- `STASIS_UPSTREAM.toml`
- `THIRD_PARTY_LICENSES.html`
- `VERSION.txt`

`etc/ci/stasis/release_archive.py` is the source-backed writer and verifier for
that contract. It accepts only version `0.1.0` and the two release-platform
labels above. Its self-test covers both platforms and rejects unknown versions,
platforms, archive members, metadata, checksums, and proof bindings.

## Exact GitHub release inventory

The stable GitHub release contains exactly nine assets:

- `stasis-0.1.0-macos-aarch64.tar.gz`
- `stasis-0.1.0-macos-aarch64.tar.gz.sha256`
- `stasis-0.1.0-macos-aarch64.binary.sha256`
- `stasis-0.1.0-macos-aarch64-act-settle-inspect.json`
- `stasis-0.1.0-linux-x86_64.tar.gz`
- `stasis-0.1.0-linux-x86_64.tar.gz.sha256`
- `stasis-0.1.0-linux-x86_64.binary.sha256`
- `stasis-0.1.0-linux-x86_64-act-settle-inspect.json`
- `stasis-0.1.0-runtime-manifest.json`

GitHub build-provenance attestations cover all nine release assets and the two
SDK package-run files. The npm tarball and its package-run proof remain Actions
artifacts; they do not become GitHub release assets.

The runtime manifest is generated only after both native archives and their gate
proofs pass. It binds:

- the exact tag, SDK version, implementation, and seven-key source identity;
- Node `darwin-arm64` to `macos-aarch64`;
- Node `linux-x64` to `linux-x86_64`;
- each release URL, compressed byte size, archive SHA-256, executable SHA-256,
  archive root, and exact ten-file inventory.

`sdk/typescript/scripts/generate-runtime-manifest.mjs` consumes that strict JSON
and emits the TypeScript module packed into the SDK. Release verification
regenerates the same module before comparing npm tarball bytes. This ordering
avoids a digest cycle: native bytes exist first, then the SDK is assembled around
their immutable identities.

## Package and native gates

`.github/workflows/stasis-package.yml` has four credential boundaries:

1. `archive-contract` checks the exact source version and runs the archive and
   npm self-tests.
2. `package-native` builds both native targets in a fail-fast matrix. The Linux
   job requires Ubuntu 22.04's exact glibc 2.35 build floor. Each job creates and
   re-verifies its archive, runs the automation and bounded-settlement negative
   fixture matrix, executes the extracted binary, produces a run/attempt-bound
   gate proof, and uploads attempt-qualified artifacts.
3. `package-sdk` waits for both native jobs, re-verifies both handoffs, generates
   the runtime manifest, builds and packs the SDK, and runs the real North Star
   fixture three times against the packed public entrypoint. It has no release or
   npm credential.
4. `attest-package` consumes the three exact producer attempts selected by
   `package-sdk`: one for macOS, one for Linux, and one for the SDK/manifest. It
   runs without a checkout, validates the ordered attempt handoff, exact
   inventories, bounded archive structure, checksums, source identities, native
   proofs, the SDK package/proof, and the runtime manifest. Only this job
   receives `id-token: write` and `attestations: write`.

The North Star is:

```text
open login
  -> fill credentials
  -> activate submit
  -> controlled POST
  -> promise and microtask
  -> DOM transition
  -> timer
  -> requestAnimationFrame
  -> settle
  -> query and extract
  -> bounded redacted evidence
  -> clean close and EOF
```

It contains no sleep or timeout-as-progress authority. Its structured proof
requires three independent executions to produce the same controlled result.

## Promotion

Promotion is a separate manual dispatch from `main`:

```sh
gh workflow run stasis-package.yml \
  --repo oxhq/stasis \
  --ref main \
  -f package_run_id=REPLACE_WITH_SUCCESSFUL_MAIN_PUSH_RUN_ID \
  -f release_tag=v0.1.0
```

The read-only verifier accepts only a completed successful `push` run from the
repository's current default branch. It requires the selected SHA to remain an
ancestor of that branch and resolves the latest exact macOS, Linux, and SDK
producer jobs independently from all run attempts. Each selected job must be
completed successfully and retain its unique, non-expired artifact pair; a
newer failed, incomplete, duplicated, or missing producer cannot fall back to
stale artifacts. The SDK attempt must not predate either native attempt. The
verifier then checks all bytes and attestations and stages only the release
inputs. It also requires the selected SHA and the current default-branch tip to
have the same `.github/workflows` Git tree. The protected mutation job resolves
both tips again and repeats that tree identity check immediately before release
creation.

The protected `release` environment receives only those staged files. Its job
does not check out or execute candidate source. It rechecks inventories, hashes,
proofs, source/run identities, runtime-manifest bindings, and provenance before
creating a lightweight `v0.1.0` tag and a draft stable release. It refuses to
mutate an existing release or a tag at another object.

Inspect the draft once, then publish that same draft as a non-prerelease. Do not
replace assets, retarget the tag, or recreate the release. Repository immutable
releases must already be enabled; the npm workflow requires the release API to
report `immutable: true`.

## npm stable publication

Publishing the stable GitHub release triggers
`.github/workflows/stasis-publish-npm.yml`. Its mutating path accepts only the
published, immutable, non-prerelease `v0.1.0` release in `oxhq/stasis`.

The workflow first runs without npm credentials. It verifies all nine release
assets and attestations and requires both native proofs to identify the same
successful package run. Their platform-specific attempts and the SDK attempt
are resolved independently from the latest exact successful producer jobs, with
the same no-stale-fallback and ordering checks used by promotion. It compares
the release runtime manifest with the SDK attempt's original attested manifest,
regenerates the SDK module, and reproduces the SDK attempt's exact npm tarball.
It then runs both the packed-SDK gate and the three-run North Star through
managed runtime acquisition. The staged proof binds the combined gate log.

Only the minimal `publish` job enters the protected `npm` environment and
receives `id-token: write`. It has no checkout and executes no package lifecycle
scripts. It rechecks the immutable release, exact staged inventory, package
metadata, tarball integrity, package-run proof, prepublication proof/log, North
Star record, and build provenance before using npm trusted publishing:

```sh
npm publish oxhq-stasis-0.1.0.tgz \
  --ignore-scripts \
  --access public \
  --tag latest \
  --provenance
```

No npm token is read by the stable workflow. Trusted publishing remains bound to
repository `oxhq/stasis`, workflow `stasis-publish-npm.yml`, environment `npm`,
and publish permission.

After publication, the exact expected dist-tag map is:

```json
{
  "alpha": "0.1.0-alpha.0",
  "latest": "0.1.0"
}
```

The workflow fails if `alpha` moves, `latest` does not point to stable, an
unexpected tag appears, or registry bytes differ from the staged tarball.

The final public verification matrix runs on `macos-15` and `ubuntu-22.04`. Each
job independently downloads and verifies its released native asset and runtime
manifest, verifies npm registry signature and SLSA provenance, installs the
public package in a clean consumer, runs the explicit-binary gate, and runs the
three-iteration North Star through `launch()` with no `executablePath`. The latter
proves HTTPS acquisition, size/hash/inventory verification, cache installation,
launch, controlled automation, evidence, and clean shutdown from public bytes.

## Retry and recovery rules

- Native and SDK artifact names include their producer attempt. Promotion and
  publication resolve the latest exact macOS, Linux, and SDK producer jobs
  separately, then require each selected job's unique artifact pair and the
  native-before-SDK ordering.
- **Re-run all jobs** creates fresh producers for every leg. **Re-run failed
  jobs** may reuse a platform leg whose latest exact job already succeeded, but
  only through verified run history and its attempt-qualified artifacts. A
  failed or incomplete latest producer never falls back to an older attempt.
- Existing version bytes are immutable. A publish retry may skip `npm publish`
  only when the registry's SHA-512 integrity equals the staged tarball exactly.
- A failure after npm accepts bytes is a verification incident, not permission to
  republish the version.
- Manual dispatch of the npm workflow is read-only recovery. It must run from the
  default branch and identify the original release-event run and attempt.
- Recovery never enters the `npm` environment and performs no registry mutation.

## Hosted controls required before promotion

- `main` and `v*` tags are protected against deletion and non-fast-forward
  movement; release creation uses the protected `release` environment.
- Immutable GitHub releases and artifact attestations are enabled.
- The repository Actions policy permits only the exact pinned action SHAs used by
  the two Stasis workflows, including nested official attestation actions.
- Default `GITHUB_TOKEN` permission is read-only. Workflow-created pull-request
  approval is disabled, and fork workflows receive no release/npm credentials.
- The `npm` environment is protected and npm trusted publishing is configured for
  the exact repository, workflow filename, environment, and publish permission.
- Only `stasis-package.yml` and `stasis-publish-npm.yml` are active release
  workflows. Before the final release commit, require zero diff under
  `.github/workflows` from the reviewed workflow tree, then repeat the active
  workflow inventory after the final push.

These hosted controls are release gates. If any live setting differs, stop before
creating `v0.1.0`; do not weaken source checks to compensate.
