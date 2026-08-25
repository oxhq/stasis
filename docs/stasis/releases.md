# Stasis 0.2 release train

`v0.2.1` is the current controlled web-session release. It supersedes the
immutable `v0.2.0` bytes with one focused correction: redirect predecessors
remain pending until their terminal callback even when the successor request
starts first, so bounded evidence cannot omit the predecessor's response
headers or completion. It keeps the frozen
`controlled-webapp-v1` document contract available through the legacy API and
adds the separately named `controlled-web-session-v1` profile. The new profile
binds navigation and document authority, session cookies and Web Storage,
practical selectors and semantic forms, structured extraction, immutable
network fixtures, bounded request/evidence projections, and process-isolated
pool/crawler helpers. The stable product boundary is specified in
`docs/stasis/session-v0.2.md` and
`profiles/controlled-web-session-v1.json`.

The release identity is exact: native `stasis-shell` version `0.2.1`, SDK
`@oxhq/stasis@0.2.1`, and GitHub tag `v0.2.1`. The release workflows reject
every other stable or prerelease identity. The native matrix remains deliberately
bounded to Linux x86-64 and macOS arm64, with the same compatibility,
ten-member archive, checksum, source-identity, and provenance contracts used by
the first stable train. `etc/ci/stasis/release_archive.py` accepts only `0.2.1`
for newly produced release archives; this does not alter or re-authorize the
historical `v0.2.0` or `0.1.x` bytes.

The exact `v0.2.1` GitHub release inventory is nine assets:

- `stasis-0.2.1-macos-aarch64.tar.gz`
- `stasis-0.2.1-macos-aarch64.tar.gz.sha256`
- `stasis-0.2.1-macos-aarch64.binary.sha256`
- `stasis-0.2.1-macos-aarch64-act-settle-inspect.json`
- `stasis-0.2.1-linux-x86_64.tar.gz`
- `stasis-0.2.1-linux-x86_64.tar.gz.sha256`
- `stasis-0.2.1-linux-x86_64.binary.sha256`
- `stasis-0.2.1-linux-x86_64-act-settle-inspect.json`
- `stasis-0.2.1-runtime-manifest.json`

The npm tarball and its SDK proof remain attempt-qualified Actions artifacts,
not GitHub release assets. Build provenance covers all release assets and the
SDK package/proof. The SDK's checked-in generated runtime-manifest module is an
intentionally mismatched historical alpha placeholder: local `prepack` fails
closed until the credential-free package job generates the exact `v0.2.1`
manifest from both verified native archives.

## 0.2 package and product gates

The credential boundaries and attempt-specific producer selection remain the
same as the 0.1 train: native macOS and Linux producers are resolved
independently, the SDK attempt must not predate either native attempt, and a
newer failed, incomplete, missing, duplicated, or expired producer never falls
back to stale outputs. Only the final attestation job receives package build
attestation credentials; only the minimal npm publication job receives trusted
publishing credentials. Promotion and publication independently resolve the
latest successful attestation-job attempt after the selected native and SDK
producers, then require one verified eleven-subject statement whose signed
runner invocation and SLSA invocation both name that exact package run/attempt.

The SDK package job must pass both real public-entrypoint stories against the
extracted release binary:

1. The frozen `stasis-v0.1-north-star` executes three times against
   `controlled-webapp-v1`, proving backward compatibility of the original
   document contract.
2. The `stasis-v0.2-session-north-star` executes three fresh primary sessions
   and three fresh restored sessions. It covers the complete semantic form
   surface, mixed live/fixture network work, cookie and Web Storage replacement,
   session-state-token rotation and stale rejection, application and explicit
   redirects, history changes, controlled timer/rAF progress, structured
   extraction, bounded redacted request/evidence records, state export/import,
   document stale-token rejection, clean close/EOF, and equal semantic
   fingerprints without a host sleep. One additional fresh process proves a
   `fixtures_only` miss is sticky and never reaches ambient network. A bounded
   real-binary process pool then crawls three fixture-only pages with the public
   reference crawler, proving fresh-process disposal, canonical link extraction,
   and a concurrency bound of two.

The full v0.2 story also runs against each exact production-stripped native
binary on its own macOS arm64 or Linux x64 producer before any release artifact
is uploaded. This makes native platform correctness a prepublication gate,
rather than relying on post-publication registry verification to discover a
Linux-only failure.

Both fixture runners and the entire session fixture directory are copied beside
the installed tarball before execution. Credential-free package gating uses the
dedicated `STASIS_NORTH_STAR_BINARY` and
`STASIS_SESSION_NORTH_STAR_BINARY` overrides. Prepublication repeats both
stories, including the v2 session proof against the explicitly verified release
binary. Final public verification unsets both overrides and GitHub/npm tokens
for both stories, proving anonymous managed-runtime acquisition, archive
size/hash/inventory validation, atomic cache installation, and launch from
public bytes on `macos-15` and `ubuntu-22.04`.

Promotion remains a separate manual dispatch from `main` and accepts only a
successful main push package run:

```sh
gh workflow run stasis-package.yml \
  --repo oxhq/stasis \
  --ref main \
  -f package_run_id=REPLACE_WITH_SUCCESSFUL_MAIN_PUSH_RUN_ID \
  -f release_tag=v0.2.1
```

After the exact draft is inspected and published as an immutable,
non-prerelease GitHub release, the release event may publish only
`@oxhq/stasis@0.2.1` with npm trusted publishing and provenance. The exact
post-publication dist-tag map is:

```json
{
  "alpha": "0.1.0-alpha.0",
  "latest": "0.2.1"
}
```

The workflow fails if the immutable historical `alpha` tag moves, `latest`
does not point to `0.2.1`, any unexpected dist-tag appears, public registry
bytes differ from the staged tarball, provenance/signature verification fails,
or either anonymous North Star fails. Manual npm-workflow dispatch remains
read-only recovery tied to the original release-event run and attempt; it may
never publish or retag.

## Stasis v0.2.0 release history (immutable)

`v0.2.0` first published the `controlled-web-session-v1` surface. Its GitHub
release, native assets, npm package, provenance, and tag remain immutable. The
initial Linux post-publication run exposed a scheduler-dependent redirect
evidence ordering race; an exact read-only recovery run subsequently passed,
but that rerun did not erase the defect. `v0.2.1` is the corrective stable
package and the only 0.2 release that should remain on npm `latest`.

# Stasis 0.1 release history (immutable)

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

At the immutable `v0.1.0` release commit
(`0f3e0543f650a5c718ebc86919b16655080b4ace`),
`etc/ci/stasis/release_archive.py` was the source-backed writer and verifier for
that contract. That historical helper accepted only version `0.1.0` and the two
release-platform labels above; its self-test covered both platforms and rejected
unknown versions, platforms, archive members, metadata, checksums, and proof
bindings.

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
mutate a published or mismatched release, or a tag at another object. A retry
may resume only the exact matching draft by ID, remove its starter placeholders,
and upload any missing verified assets before revalidating the full inventory.

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
It then runs the packed-SDK gate against the explicitly verified extracted
binary and runs the three-run North Star through managed runtime acquisition.
The staged proof binds the combined gate log.

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
