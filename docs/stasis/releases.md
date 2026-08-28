# Stasis 0.3 release train

The source tree, native `stasis-shell` crate, TypeScript package metadata, and
release validators are aligned to exact version `0.3.0`. Source version is not
a publication claim. Before promotion, `v0.2.1` and `@oxhq/stasis@0.2.1` remain
the immutable stable predecessor until an exact `v0.3.0` tag is promoted,
published immutably, published to npm with provenance, and verified from
anonymous managed-runtime bytes. After promotion, verify those public artifacts;
do not infer current registry state from this checked-in text.

Version 0.3 adds the explicitly selected `controlled-web-session-v2` profile
for bounded same-global, untransferred `MessageChannel` work, a bounded direct top-level
`HTMLImageElement.src` cache/decode completion path for canonical data SVG and direct HTTP(S)
selection, a distinct bounded inline
`<svg>` path in the controlled top-level document for its exact cached internally serialized
data-SVG request and cache-ID owner, and the narrow exact public non-auxiliary controlled
top-level single-line `InputMethodType::Text` presentation boundary when `multiline = false` and no
virtual keyboard is requested. The image slice admits only direct-source work within its
65,536-byte initial selected-URL and 512 retained-ownership-record bounds on the same
ScriptThread/ImageCache. Data URLs require canonical exact `image/svg+xml`; HTTP(S) is admitted by
the initially selected scheme before response format is known, and finite decode failure remains
owned. Post-metadata `multipart/x-mixed-replace` is retained as typed
`unsupported_rendering` / `image_load` after separately owned finite Resource I/O drains, without
baseline delivery fallback; an endless response remains blocked on that external I/O. A public document
replacement while HTTP image resource I/O is active retains fatal `blocked_on_external_io`; this
slice does not claim cross-document replacement through in-flight external I/O. The inline slice additionally requires an internal request and exact
cached-URL/cache-ID join, fences decode and raster completion, and emits no new DOM load event;
an identical current inline root may join an exact retained producer whether layout reports
`PendingResponse` or a stale/reentrant `Unrequested`, with its exact cache-key URL and ID anchored
by an existing same-ID fenced layout record and a nonempty uniformly fenced callback set whose
callback-owned producer keys all equal that URL. The key may survive an earlier DOM owner's unbind
only until terminal callback removal; the earlier DOM identity is not authority. A new current
owner is retained once and an already-retained owner is idempotent. The join reuses the existing
listener and producer; it adds no listener, producer, or fetch. Every controlled callback, layout
owner, DOM identity, raster key, and raster owner is charged to the shared 512-record capacity. A
missing anchor/key, stale candidate, mismatch, mixed provenance, or capacity terminal fails closed
and cannot promote baseline work.
Excluded image paths receive no new authority.
Cache-owned callback retirement and a dequeued
response whose closed-pipeline tombstone proves that navigation retired its Window complete
normally as owned cancellation. A normal live handler rejection retains the pending owner or key,
completes the scoped message guard, and settles as typed `unsupported_rendering`; admission,
enqueue, producer callback panic, actual handler unwind, pre-handler authority, target-invariant or clock failure,
and guarded transport loss explicitly abandon the stream and remain terminal. Completion and
abandonment require the exact live fence/sequence and registered Image producer class before any
terminal or watermark mutation. One document-clock sample is shared by an admitted
engine-generated HTML image completion set, and engine-generated exact-public-target top-level
focus transitions in v2 also receive a document-clock `Event.timeStamp`. Each public mutating
automation action samples the document Performance clock once before mutation and shares that value
with every browser-created event constructed synchronously during the action; the fill, activation,
reset, check/uncheck, select, invalid, submit, and formdata corpus is representative, not an event-name
allowlist. MessageChannel construction and posting require the exact active public top-level target
and an incumbent matching the owner global, pipeline, and WebView before pair publication or
structured cloning. A nonempty owned CSS animation pending-event dispatch batch likewise samples the document
Performance clock once and shares it only with internal `AnimationEvent` or `TransitionEvent`
records owned by the exact public non-auxiliary controlled top-level WebView/document. The `TransitionEvent` adapter is
conditional on an existing owned transition record reaching that queue; general transition
settlement is not claimed. A nonempty document-owned pending CSS animation-event queue is finite
rendering demand and retains one later owned rendering opportunity until dispatch drains it; an
empty queue leaves no opportunity. Live scheduled work uses guarded `AdvanceTo` at the exact
retained scheduler head, including an exact-`now` deadline; only an unscheduled batch is
`Drive`-ready. This corrects liveness without adding a task source or limit. Auxiliary top-level WebViews remain host-stamped. Script-created events,
including the WebIDL animation
and transition event constructors, excluded image sources, general SVG/resource, and other
unlisted host-stamped paths remain unsupported or retain predecessor behavior. Baseline and v1 SVG
behavior and the existing CSS animation authority, semantics, and limits are unchanged. It keeps
`controlled-web-session-v1` frozen and as the default. Candidate package runs
must execute the complete `baseline_protocol` integration target on macOS
arm64, Linux x86-64 under Xvfb, and the Windows x86-64 CI job, in addition to
the existing controlled-session and release-artifact gates. Windows remains a
CI-only bundle and is not admitted to the managed release manifest. Each 0.3
macOS/Linux archive has twelve files: the historical ten-file
runtime/source inventory plus `controlled-web-session-v2.json` and
`session-v0.3-candidate.md`, both byte-bound to the exact source revision.

The seventh slice owns persistent cookies in memory for the exact v2 session. Expiry uses
controlled Unix nanoseconds with origin zero, valid `Max-Age` takes precedence over `Expires`,
lifetime is clamped to 400 days, and lazy purge precedes observation, request selection, and
export. SameSite selection uses captured schemeful site-for-cookies, the current redirect-hop
method, and the top-level-navigation bit; ineligible cookies are filtered, while unknown or opaque
context remains typed unsupported. V2 state retains schema 1 but carries the
literal v2 profile and is portable only through explicit v2 export and initial import. V1 state is
not migrated. After successful controlled parsing, cross-site subresource responses retain only
valid Secure SameSite=None cookies; otherwise valid Strict/Lax/unspecified values are ignored,
while parse, normalization, and time-range failures retain their existing typed outcomes.
Top-level-navigation responses admit all otherwise valid unpartitioned cookies. A post-open
request above u64 is nonfatal `unsupported_cookie_time_range`; initial open hardens the same code
to fatal fail-stop. Either post-open typed rejection may retain bounded `request_started` and
`request_failed` evidence, but it occurs before `route_decided` or route selection, fixture or live
external I/O, and Cookie header construction. Partitioned cookies and
CookieStore read/getAll/delete remain unsupported.

The eighth slice adds a required owner-attested `url` to every returned v2 settle outcome. It is
projected from the exact final active top-level navigation authority after the passive
N1/document-pending-D/passive-N2 bracket succeeds, and the same authority binds the returned
`stateToken`. Its presence does not imply quiescence. `Session.url` remains the open-time value;
there is no new poll or mutable session property, frozen v1 result shapes remain byte-compatible,
and bounded redacted settlement evidence does not include the URL.

The ninth slice admits bounded progress through an eligible JavaScript interval scheduler head
only for v2 settlement with `persistentWork: "report"`. Every observed finite timer and
animated-image deadline must be strictly later. One finite rendering opportunity may share the
timestamp only as a distinct exact same-scheduler owner whose `TimerId` sequence follows the
interval head; same-entry, lower-or-equal-order, foreign-scheduler, bare/unowned, equal
finite-timer, and equal animated-image collisions remain blocked.
Every exact head uses the existing single-use advance token; each callback is an ordinary task and
all task, microtask, rendering, mutation, control-turn, and virtual-time limits remain authoritative.
After finite work drains, settlement does not fire another interval cycle and returns
`quiescent_with_persistent_work`. Strict policy and both frozen predecessor profiles still stop at
the interval head as `blocked_on_open_ended_work`.

The checked-out release and npm workflows accept only exact `v0.3.0` for new
promotion/publication work. They do not authorize rebuilding or replacing any published `0.2.x`
bytes. Checked-in version text is not release evidence: verify the immutable tag, hosted promotion,
npm provenance, and anonymous public-consumer result.

# Stasis 0.2 release history (immutable predecessor)

`v0.2.1` is the immutable corrective controlled web-session predecessor. It supersedes the
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

The immutable release identity is exact: native `stasis-shell` version `0.2.1`, SDK
`@oxhq/stasis@0.2.1`, and GitHub tag `v0.2.1`. Its tagged release workflows rejected
every other stable or prerelease identity. The native matrix remains deliberately
bounded to Linux x86-64 and macOS arm64, with the same compatibility,
ten-member archive, checksum, source-identity, and provenance contracts used by
the first stable train. The helper at the `v0.2.1` tag accepted only `0.2.1`;
the current main helper accepts only `0.3.0` and cannot alter or
re-authorize historical `v0.2.1`, `v0.2.0`, or `0.1.x` bytes.

## Windows x86-64 CI-only proof artifact

Package-mode runs also execute a separate `package-windows-ci` job on the
GitHub-hosted `windows-2022` x86-64 runner. This job is deliberately outside the
stable macOS/Linux native release matrix. It builds the exact event SHA with
`mach.ps1 exec -- cargo build --locked -p stasis-shell --profile
production-stripped`, runs the frozen v0.2 TypeScript session North Star through
an explicit `stasis.exe`, and runs the complete `baseline_protocol` target, the
native `controlled_mvp` and controlled-network redirect-order gates, plus the
Windows stdio protocol-isolation tests.

The job creates an unsigned, attempt-qualified Windows CI ZIP. The ZIP has one
root directory containing `stasis.exe`, the ANGLE `libEGL.dll` and
`libGLESv2.dll` rendering runtime, the source and license documents, the
canonical v2 profile and versioned contract, and the
app-local x86-64 MSVC runtime DLL closure derived from the native files' PE
imports. The job writes archive and executable SHA-256 sidecars, extracts the
ZIP into a fresh directory, verifies every extracted member, and runs the
ignored `release_gate_published_binary_completes_act_settle_inspect` fixture
against that extracted executable. The bundle and its logs are retained as
separate Actions artifacts for the producing run attempt.

Before invoking Cargo, the job binds both uv and Mozilla's `PYTHON3` override to
the absolute interpreter emitted by the pinned `actions/setup-python` step,
rejects a `WindowsApps` Store alias,
checks the `.python-version` minor both directly and inside `mach.ps1 exec`, and
requires that `python.exe` on `PATH` resolves to that same setup-python
installation. The locked `aws-lc-sys 0.44.0` build has an explicit
`AWS_LC_SYS_PREBUILT_NASM=1` fallback: a usable runner `nasm.exe` is version-
probed when present; otherwise aws-lc uses the crate's Cargo-checksummed prebuilt
x86-64 objects. This is an intentional build input, not an ambient-tool guess.

This proves only that the checked-out revision builds, bundles, and passes the
declared native and explicit-SDK gates on that Windows runner. The ZIP is not a
GitHub release asset, is not covered by the stable eleven-subject provenance
statement, and is not admitted to promotion, the checked-in/generated runtime
manifest, npm package inventory, or managed runtime acquisition. Windows users
must provide the extracted executable explicitly through `executablePath`.
The stable supported distribution surface remains macOS arm64 and Linux x86-64.

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
closed until the credential-free package job generates the exact 0.3
`v0.3.0` manifest from both verified native archives.

## 0.3 package and product gates

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

The packed-SDK exact-binary gate also opens `controlled-web-session-v2` explicitly. In one fresh
controlled session it proves idle and buffered local MessageChannel ownership, navigates to a
direct data-SVG fixture and observes the exact document-clock `load`/`loadend` completion trace,
navigates to a cross-origin direct HTTP fixture and proves owned success, owned decode failure, and
a same-pipeline cache hit with zero residual Image producers or pending images, then navigates to an
inline-SVG fixture and proves quiescence with no invented DOM load event before
navigating to the programmatic-focus fixture and observing the exact trusted focus-transition
trace. Finally it advances document time to 5 ms, proves the exact synchronous browser-event trace
for fill, activate, reset, check, select, invalid and valid submission, then proves that five
script-created event interfaces remain host-stamped and settle as `unsupported_clock_surface`.
A second fresh exact-binary process proves controlled internal `animationstart` and `animationend`
dispatch reaches quiescence through finite rendering demand, then proves script-created
`AnimationEvent` and `TransitionEvent` values remain host-stamped and typed unsupported. The durable
SDK gate additionally proves one same-site login response can retain and export a controlled
persistent cookie and that cross-site subresource selection filters an imported Lax
cookie while retaining its exact state identity. A third same-host exact-binary process opens
without an import and proves neither an ambient request cookie nor cookie state appears; a fourth
process opens at u64 max, advances beyond it, and proves a post-open request fails as
`unsupported_cookie_time_range` before reaching the server. Every exact-binary child receives an
explicit runtime-only environment
allowlist rather than workflow or registry credentials. Every returned v2 settle in that gate must
also carry the owner-attested active top-level URL from the same authority as its state token, while
its settlement evidence omits the URL. Its schema-10 proof binds all nine v2 slices to the same package, native
digest, source revision, clean close responses, and protocol EOFs.

For direct HTTP(S) images, the 65,536-byte admission limit applies to the initially selected
canonical URL. A final redirect URL is not rechecked against that initial limit: its fetch remains
separately owned Resource I/O and the immutable session network policy remains authoritative. Cache
reuse proof is limited to one pipeline's image-cache store under immutable fixture routes; it is not
a deterministic claim for cross-pipeline, live, or mutable HTTP content.

The frozen v0.2 story and complete native `baseline_protocol` target also run
against each exact production-stripped native
binary on its own macOS arm64 or Linux x64 producer before any release artifact
is uploaded. This makes native platform correctness a prepublication gate,
rather than relying on post-publication registry verification to discover a
Linux-only failure.

On package runs, the independent Windows CI-only job must also succeed before
the main-push package outputs can be attested. Its ZIP and logs remain
diagnostic CI artifacts and are not inputs to the stable release attestation.

Two independent native Ubuntu 22.04 lanes build the exact event revision and
each require 150 fresh single-close Stasis processes without retries. One lane
enables the fixed-vocabulary lifecycle trace and the other explicitly removes
it, so tracing cannot mask or create the result. Before stressing the product,
both lanes run the deterministic physical-ownership regressions and the real
source-binary ordering oracle: WebRender's synchronous shutdown acknowledgement
must precede joins of its backend/scene threads and custom Rayon workers, and
those joins must precede renderer deinitialization and rendering-context
destruction. A successful rerun cannot substitute for these first-attempt gates.
Package-mode invocation itself rejects `GITHUB_RUN_ATTEMPT` values other than
`1`; after any package-run failure, qualification requires a fresh push run.

Both fixture runners and the entire session fixture directory are copied beside
the installed tarball before execution. Credential-free package gating uses the
dedicated `STASIS_NORTH_STAR_BINARY` and
`STASIS_SESSION_NORTH_STAR_BINARY` overrides. Prepublication repeats both
stories, including the frozen v0.2/v1 session proof against the explicitly verified release
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
  -f release_tag=v0.3.0
```

Run that promotion only after the exact source's hosted package gates pass. After
the exact draft is inspected and published as an immutable,
non-prerelease GitHub release, the release event may publish only
`@oxhq/stasis@0.3.0` with npm trusted publishing and provenance. The expected
post-publication dist-tag map would then be:

```json
{
  "alpha": "0.1.0-alpha.0",
  "latest": "0.3.0"
}
```

The workflow fails if the immutable historical `alpha` tag moves, `latest`
does not point to `0.3.0` after publication, any unexpected dist-tag appears, public registry
bytes differ from the staged tarball, provenance/signature verification fails,
or either anonymous North Star fails. Manual npm-workflow dispatch remains
read-only recovery tied to the original release-event run and attempt; it may
never publish or retag.

## Stasis v0.2.0 release history (immutable)

`v0.2.0` first published the `controlled-web-session-v1` surface. Its GitHub
release, native assets, npm package, provenance, and tag remain immutable. The
initial Linux post-publication run exposed a scheduler-dependent redirect
evidence ordering race; an exact read-only recovery run subsequently passed,
but that rerun did not erase the defect. `v0.2.1` is the corrective 0.2 stable
package. It remains immutable when a later stable release moves npm `latest`.

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

1. `archive-contract` checks the exact source version, runs the archive and npm
   self-tests, and gates the SDK under exact Node 20.0.0. The Node-floor lane
   typechecks and builds the source, emits the same typed test sources as plain
   ESM with `tsc`, and executes those emitted files with the native `node --test`
   runner. It does not use `tsx`'s custom ESM loader because that loader's worker
   initialization fails before tests start on Node 20.0.0. Every SDK test file
   is supplied to the native runner under that exact Node; each test's own
   explicit native-binary skip condition remains visible as a skip.
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
does not check out or execute source code. It rechecks inventories, hashes,
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
`.github/workflows/stasis-publish-npm.yml`. At the immutable `v0.1.0` tag, its
mutating path accepted only the published, immutable, non-prerelease `v0.1.0`
release in `oxhq/stasis`; the current main workflow accepts only `v0.3.0` and
cannot mutate that historical package.

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

Immediately after the historical 0.1 publication, the exact expected dist-tag
map was:

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
- Historical 0.1/0.2 package trains could use **Re-run all jobs** or **Re-run
  failed jobs** with attempt-qualified producer selection. Stasis 0.3 package
  mode rejects every rerun attempt; it requires a fresh push run at attempt 1.
  Promotion still rejects a failed, incomplete, missing, duplicated, or expired
  producer and never falls back to an older run.
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
