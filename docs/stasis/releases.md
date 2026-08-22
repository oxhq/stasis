# Stasis alpha releases

The first planned Stasis release is `v0.1.0-alpha.0`. Its release train binds one
source commit to one tested native archive and one byte-exact npm package. It does
not invoke or dispatch Servo's inherited `release.yml`.

## Supported release surface

This alpha is a controlled-time Servo runtime: it controls the Stasis clock and
explicit settlement boundary. It does not virtualize live network content,
ambient operating-system state, or randomness, and it does not claim general
reproducibility for those inputs. Broader deterministic-input support remains a
future design goal rather than an alpha release guarantee.

The first native matrix contains only `macos-aarch64`, built on GitHub's
Apple-Silicon macOS runner. The executable is unsigned and is not Apple-notarized.
This release makes no Linux, Windows, macOS Intel, Developer ID, notarization, or
cross-platform byte-reproducibility claim.

Both the native build and the postpublication fixture fail before using the
`macos-aarch64` label unless `uname -m` reports `arm64` and macOS reports native
Arm64 support. A future runner-label change therefore cannot silently mislabel an
x86_64 artifact.

Before bootstrapping Servo, the native job records the selected Xcode/compiler and
disk state, removes only disposable simulator devices and Homebrew leftovers on
the ephemeral hosted runner, and requires at least 10 GiB free. It retains the
selected Xcode toolchain. After the production build it records disk state again
and requires at least 4 GiB for archive creation and the native/SDK gates. These
are capacity fail-fast checks, not proof that GitHub's runner image will always be
large enough; adjust them only after measuring a clean hosted production build.

The archive writer normalizes its file order, ownership, modes, timestamps, and
gzip header. The executable still comes from the selected hosted build, so the
release guarantee is an immutable source/digest/provenance binding, not a claim
that an independent compiler run produces the same executable bytes.

The native bundle is named
`stasis-0.1.0-alpha.0-macos-aarch64`. Its archive contains the normalized root
directory `stasis-0.1.0-alpha.0-macos-aarch64/` and exactly eight regular files
beneath it:

- `stasis`
- `INSTALL.txt`
- `LICENSE`
- `LICENSE_WHATWG_SPECS`
- `SOURCE.txt`
- `STASIS_UPSTREAM.toml`
- `THIRD_PARTY_LICENSES.html`
- `VERSION.txt`

`THIRD_PARTY_LICENSES.html` is the repository's generated Servo third-party
license inventory. The GitHub release must contain exactly four assets:

- `stasis-0.1.0-alpha.0-macos-aarch64.tar.gz`
- `stasis-0.1.0-alpha.0-macos-aarch64.tar.gz.sha256`
- `stasis-0.1.0-alpha.0-macos-aarch64.binary.sha256`
- `stasis-0.1.0-alpha.0-macos-aarch64-act-settle-inspect.json`

GitHub build-provenance attestations must cover all four release files. The
package run must also attest the exact npm tarball and its SDK act-settle-inspect
proof; those two files remain Actions artifacts and are not added to the native
GitHub release.

## Native package and immutable promotion

1. Put both `ports/stasis/Cargo.toml` and `sdk/typescript/package.json` at
   `0.1.0-alpha.0`, complete review, and merge the exact release commit to the
   protected default branch. Promotion never accepts an unmerged topic-branch run.
2. The default-branch push runs `stasis-package.yml`. It checks out exactly
   `github.sha`, exports `STASIS_REVISION=github.sha`, builds the native executable
   with the locked production profile, creates the strict archive, re-verifies its
   metadata and exact inventory, and extracts it into a new directory.
3. The one ignored native release test runs with `STASIS_RELEASE_BINARY` pointing
   to that extracted executable and `STASIS_RELEASE_ARCHIVE` pointing to the exact
   archive that produced it. It must report exactly one passing
   `release_gate_published_binary_completes_act_settle_inspect` test. A capability
   skip, zero-test filter, revision mismatch, binary digest mismatch, fixture
   failure, source-identity mismatch, or eight-second wall-guard failure prevents
   the package run succeeding. The reported source object must contain exactly the
   five identities from `STASIS_UPSTREAM.toml`, the exact
   `https://github.com/oxhq/stasis.git` repository, and the full selected revision,
   with no extra keys. Its structured gate record binds the selected archive name
   and SHA-256, so the native proof cannot be replayed against different archive
   bytes.
4. In the same credential-free build context, the workflow uses Corepack-managed
   pnpm `9.12.3` and a frozen install to typecheck, test, build, and pack the SDK.
   Installing that tarball in a clean consumer and importing the public
   `@oxhq/stasis` entrypoint must complete the real act-settle-inspect fixture
   against the extracted native executable, including close response and protocol
   stdout EOF. `verify_registry_sdk.mjs` requires `--package` naming that exact
   tarball and records its name, SHA-256, and SHA-512 integrity in the gate log.
5. The selected successful package attempt contains exactly three expected
   Actions artifacts: the three native release files, the native gate proof, and
   the two-file pre-gated SDK artifact. Each artifact name ends in
   `-attempt-<producer_attempt>`, so rerunning the same run ID cannot make download
   selection ambiguous; older partial or complete attempt artifacts are never
   substituted for the newest complete producer attempt.
   Both native and SDK proofs use schema 2. They bind the full source revision, run
   ID and attempt, exact archive or tarball bytes, native binary digest, exact source
   map, and gate-log digest. All attempt-qualified artifacts and approval handoffs
   are retained for 90 days. The build job has no OIDC or attestation-write
   permission. A separate no-checkout job downloads
   only this attempt's immutable artifacts, rechecks all six regular files, exact
   inventories, cross-file digests, metadata, and proof schemas with trusted inline
   logic, then receives OIDC solely to generate GitHub build provenance for every
   file. The package workflow cannot succeed unless that job succeeds.
6. From the repository's default branch, dispatch the same workflow with that
   successful push run ID and exact tag:

   ```sh
   gh workflow run stasis-package.yml \
     --repo oxhq/stasis \
     --ref main \
     -f package_run_id=REPLACE_WITH_SUCCESSFUL_RUN_ID \
     -f release_tag=v0.1.0-alpha.0
   ```

7. The read-only promotion verifier accepts only a completed successful
   default-branch push run in `oxhq/stasis`. The selected SHA must still be an
   ancestor of the current protected default branch. It derives the highest
   complete producer attempt from the attempt-qualified artifact inventory (the
   overall workflow run attempt can be newer after “re-run failed jobs”), then
   requires exactly the three expected artifacts for that attempt. It validates
   every inventory, checksum, version, revision, exact seven-key source map, and
   gate proof, and verifies every attestation against `stasis-package.yml`, that
   SHA, the default-branch ref, and a GitHub-hosted runner.
8. Only the three native files and native proof cross into the credentialed
   promotion job. That job executes no selected source code; trusted inline checks
   rebind their identities and provenance before any mutation. It may then create
   a new lightweight tag at the exact package-run SHA and a draft prerelease with
   the exact four native release assets. It refuses to mutate an existing release
   or a tag at another object. An interrupted attempt is resumable only when the
   existing tag is already the expected lightweight tag at that SHA.
9. Inspect the draft assets and notes, then publish that same draft as a prerelease.
   Do not replace assets, retarget the tag, or recreate the release. Repository
   release immutability must already be enabled. Publication freezes the tag and
   assets and produces GitHub's release attestation; the npm workflow refuses to
   proceed unless the live release API reports `immutable: true`.

No failed native or SDK gate can reach tag or draft creation. Promotion creates a
draft only; maintainer publication is the deliberate boundary after inspection.

## npm alpha publication

Publishing the GitHub draft triggers `stasis-publish-npm.yml`. Published-release
events trigger the workflow broadly, but its verification and publication jobs
proceed only for the published `v0.1.0-alpha.0` prerelease in `oxhq/stasis`.
A manual dispatch is a read-only recovery path, not a promotion path. It must run
from the default branch and name the exact immutable tag plus the original
release-event publication run ID and attempt. Recovery verifies that historical
run, skips the protected npm environment and every publication step, and repeats
the public-byte, signature, provenance, install, and fixture checks.

The workflow has three credential boundaries:

Every artifact handoff from read-only verification into a protected environment is
retained for 90 days.

1. A read-only verification job has no protected environment and no OIDC token.
   It requires the live release to be immutable and the lightweight tag to equal
   the release-event SHA during publication, or the checked-out tag SHA during a
   read-only recovery,
   then verifies the exact four native release assets, native gate proof, and
   provenance. The native proof identifies the exact package-run ID and attempt;
   the job downloads that run's two-file SDK artifact and verifies its proof and
   provenance. It then uses Node 24, npm `11.19.0`, and
   Corepack-managed pnpm `9.12.3` to run:

   ```sh
   pnpm install --frozen-lockfile
   pnpm typecheck
   pnpm test
   pnpm build
   pnpm pack
   ```

   The rebuilt tarball must be byte-identical to the attested, pre-gated package-run
   tarball. A clean consumer must import the bare `@oxhq/stasis` package entrypoint
   and complete act-settle-inspect against the verified GitHub release executable,
   with exact SDK/runtime version, full revision, advertised capabilities, exact
   ten-second virtual-time advance, graceful close, protocol stdout EOF, and the
   same exact seven-key source map. The ten-second virtual settlement phase, not
   the whole fixture lifecycle, must finish in less than eight seconds. Only after
   this non-skippable gate succeeds is the exact tarball staged for publication.
   Its prepublish proof records the exact producing verification-job run attempt
   and exports that attempt as a job output, so “re-run failed jobs” can safely run
   publication in a later attempt without rewriting the proof identity.
2. A minimal job enters the protected `npm` environment and receives
   `id-token:write`. It does not check out the repository or execute package code.
   Trusted inline logic rechecks the exact four-file inventory—the tarball, original
   package-run proof, prepublish proof, and prepublish log—plus tarball SHA-256 and
   SHA-512 integrity, package metadata and public entrypoint map, native digest,
   package-run identity, both proof identities, gate-log binding, and original
   build provenance. It validates the staged proof against the producing attempt,
   while retaining the current publication attempt separately for provenance and
   safe retries. Immediately before mutation it re-fetches the immutable release by
   both ID and tag and rechecks the lightweight tag SHA. Publication is:

   ```sh
   npm publish oxhq-stasis-0.1.0-alpha.0.tgz \
     --ignore-scripts --access public --tag alpha --provenance
   ```

   Lifecycle scripts are disabled in the credentialed job. The first package
   creation uses the one-time protected-environment seed token described below and
   still emits npm provenance from this GitHub Actions build. Immediately afterward,
   configure npm trusted publishing, delete the environment secret, and revoke the
   seed token; subsequent releases must exchange the GitHub OIDC identity instead.
   A retry skips publication only if npm already holds the exact same SHA-512
   integrity.
3. A final read-only macOS job has neither the `npm` environment nor OIDC write
   permission. It downloads the registry tarball, requires byte identity with the
   staged tarball, pins every request to `https://registry.npmjs.org/`, requires the
   complete dist-tag map to contain exactly `alpha` and npm's mandatory `latest`
   alias, with both pointing to `0.1.0-alpha.0`,
   and requires npm's JSON signature audit to contain exactly one
   verified `@oxhq/stasis@0.1.0-alpha.0` entry and no invalid or missing entries.
   It decodes the signature-verified DSSE payload and binds its in-toto subject
   digest, GitHub workflow/repository/tag ref, source commit, hosted builder, and
   invocation to the selected release-event publication run. During a release, a
   provenance attempt from an earlier attempt of the same run is accepted for safe
   postpublication retries; recovery requires the exact explicitly selected run
   and attempt. It then installs the public registry package in a
   clean consumer. That bare package import must repeat the real act-settle-inspect
   fixture against the verified released binary, including graceful close and EOF.

No prepublication gate failure can reach `npm publish`. A failure after npm accepts
immutable bytes is a postpublication verification incident; npm does not permit
replacing an existing name/version pair.

## One-time hosted-service setup

These controls cannot be created by a source change and must exist before the
first promotion.

### GitHub

- Create the empty public repository as exactly `oxhq/stasis` with repository
  Actions disabled. npm provenance for a public package is not generated from a
  private repository. While Actions remain disabled, push a bootstrap commit to
  `main` that contains the two Stasis workflows and their release tooling, along
  with the inherited workflows that must be inventoried. This bootstrap commit must
  **not** be the separately validated final release SHA.
- Enumerate every workflow record from the bootstrap commit and disable every
  inherited workflow by ID, not just Servo's `release.yml`:

  ```sh
  gh api --paginate 'repos/oxhq/stasis/actions/workflows?per_page=100' \
    --jq '.workflows[] | [.id, .path, .state] | @tsv'
  gh workflow disable REPLACE_WITH_INHERITED_WORKFLOW_ID --repo oxhq/stasis
  ```

  Repeat the disable command for every non-Stasis workflow. Keep repository Actions
  disabled throughout this inventory and policy setup.
- Audit the organization Actions policy as a read-only ceiling before configuring
  the repository. Do not narrow OxHQ's organization-wide allowed-actions or
  enabled-repositories policy solely for Stasis: those settings also affect every
  other organization repository and require a separately reviewed migration.
  Provided the organization ceiling permits the required official actions,
  configure only the `oxhq/stasis` repository policy to allow selected actions at
  the exact full commit SHAs recorded in the two Stasis workflows, including its
  nested official action
  `actions/attest@508db95dd578ae2727ebd6217d5ba78e4fbda05d` (v4.2.1). Require
  SHA pinning at the repository, set the default `GITHUB_TOKEN` permission to
  read-only, forbid workflow-created pull request approvals, require the strictest
  applicable approval for workflows from forks, and never send write tokens or
  secrets to fork pull requests. Enable artifact attestations. If the organization
  ceiling blocks any required setting, stop and coordinate an organization-wide
  policy change instead of weakening the repository.
- Enable repository Actions, explicitly enable only `stasis-package.yml` and
  `stasis-publish-npm.yml`, and verify that their paths are the complete active
  workflow inventory:

  ```sh
  gh workflow enable stasis-package.yml --repo oxhq/stasis
  gh workflow enable stasis-publish-npm.yml --repo oxhq/stasis
  active_workflows=$(
    gh api --paginate 'repos/oxhq/stasis/actions/workflows?per_page=100' \
      --jq '.workflows[] | select(.state == "active") | .path' | LC_ALL=C sort
  )
  test "$active_workflows" = "$(printf '%s\n' \
    '.github/workflows/stasis-package.yml' \
    '.github/workflows/stasis-publish-npm.yml')"
  ```

  Set the default branch to exactly `main`, but do not push the final release commit
  until every remaining hosted-service control below is configured and verified.
- From a local administrator-authenticated bootstrap session, enable immutable
  releases before promotion. The repository endpoint requires Administration
  write permission; its GET check requires Administration read permission, neither
  of which the workflow `GITHUB_TOKEN` can request:

  ```sh
  gh api --method PUT \
    -H 'Accept: application/vnd.github+json' \
    -H 'X-GitHub-Api-Version: 2026-03-10' \
    repos/oxhq/stasis/immutable-releases
  gh api \
    -H 'Accept: application/vnd.github+json' \
    -H 'X-GitHub-Api-Version: 2026-03-10' \
    repos/oxhq/stasis/immutable-releases | \
    jq -e 'select(.enabled == true) | {enabled, enforced_by_owner}'
  ```

  Immutability applies only to future releases and takes effect when a draft is
  published. Do not add this administration endpoint to an Actions job; the npm
  workflow instead checks `immutable: true` on the ordinary live release objects.
  See GitHub's [immutable-release setup](https://docs.github.com/en/code-security/how-tos/secure-your-supply-chain/establish-provenance-and-integrity/prevent-release-changes)
  and [repository REST endpoints](https://docs.github.com/en/rest/repos/repos?apiVersion=2026-03-10#enable-immutable-releases).
- Create protected `release` and `npm` environments. The workflow and npm trusted
  publisher both use the exact environment name `npm`. Required reviewers are
  recommended for both environments. Configure selected deployment refs explicitly:
  `release` allows the `main` branch, while `npm` allows the exact
  `v0.1.0-alpha.0` tag (or an intentional `v*` tag pattern for later releases).
  Do not select `Protected branches only` for `npm`; the release event runs from a
  tag ref and that setting blocks the publish job before its own identity checks.
- Protect the default branch and protect `v*` tags from update or deletion. The
  promotion job has only its `GITHUB_TOKEN`, so do **not** enable `Restrict
  creations` for the `v*` ruleset; doing so deterministically blocks first promotion
  with HTTP 403. If organization policy requires restricted creation, an authorized
  maintainer must pre-create the exact lightweight tag at the selected package SHA
  before dispatch, which the workflow will accept only at that exact commit, or the
  workflow must first be redesigned to use a separately installed GitHub App token.
  The no-checkout package-attestation job has `contents:read`, `id-token:write`, and
  `attestations:write` solely to attest already revalidated files. The protected
  native promotion job has `contents:write` plus read-only Actions/attestation
  access and no OIDC permission. Of the protected-environment jobs, only the
  minimal npm publish job declares `id-token:write`; repository and package code
  run outside both protected environments and without that permission.
- After every control above is live, push the separately validated final release
  commit as a fast-forward update to `main`. This must be a different commit from
  the bootstrap commit. The package workflow intentionally has no path filter: every
  pull request and every `main` push runs the release gate so a change to an indirect
  Servo/bootstrap input cannot bypass it. The final push is required: the package
  producer must have `event == push` on the default branch, and a manual
  `workflow_dispatch` run is not an eligible substitute for promotion. The
  bootstrap commit must
  already contain the final reviewed `.github/workflows` tree; require zero workflow
  diff between the bootstrap and final release SHAs, then repeat the exact active
  workflow-inventory assertion immediately after the final push. If any other
  workflow is active or starts, stop promotion and investigate it.

### npm: seed the first alpha, then require OIDC

npm requires a package to exist before a trusted-publisher relationship can be
created. Do not publish a dummy setup version. npm assigns its required `latest`
alias to the first real version, so the first-alpha verifier requires both `alpha`
and `latest` to identify the same exact release bytes. Use this one-time sequence
for `0.1.0-alpha.0`:

1. From an OxHQ npm owner account authorized to create public packages in the
   `@oxhq` scope, create a minimum-expiry granular token. Select only the `@oxhq`
   package scope with package/scopes permission `read and write`, grant no
   organization-management permission, and enable bypass-2FA only because this
   non-interactive first publish requires it.
2. Store it only as the `NPM_TOKEN` secret on the protected GitHub environment named
   `npm`. Do not create a repository or organization secret and do not print the
   token.
3. Publish the inspected GitHub prerelease. After every credential-free gate passes,
   the minimal publish job exposes the secret as `NODE_AUTH_TOKEN` only to its
   lifecycle-disabled publish process. The version is created directly under the
   `alpha` dist-tag with provenance. npm also assigns its mandatory `latest` alias
   to this same first real version; do not move either alias to different bytes.
4. Immediately after registry verification succeeds, open a local interactive
   shell as an OxHQ package owner with account-level 2FA. Do not authenticate this
   operation with the bypass-2FA seed token; npm trust endpoints reject that token
   class. With npm `11.15.0` or newer, configure trusted publishing:

   ```sh
   npm trust github @oxhq/stasis \
     --repo oxhq/stasis \
     --file stasis-publish-npm.yml \
     --env npm \
     --allow-publish
   ```

   The equivalent npmjs.com settings are organization `oxhq`, repository `stasis`,
   workflow filename `stasis-publish-npm.yml`, environment `npm`, and allowed action
   `npm publish`. The filename is the basename, not `.github/workflows/...`.
   Verify the stored binding before revoking anything:

   ```sh
   npm trust list @oxhq/stasis --json --registry=https://registry.npmjs.org/ | \
     jq -e '
       .type == "github"
       and .repository == "oxhq/stasis"
       and .file == "stasis-publish-npm.yml"
       and .environment == "npm"
       and (.permissions | sort) == ["createPackage"]
     '
   ```

5. Delete the GitHub environment secret and revoke the granular token. Confirm
   subsequent alpha publishes use OIDC with no `NPM_TOKEN`. Then enable npm's setting
   that requires 2FA and disallows token-based publication.

See npm's official documentation for [trusted
publishing](https://docs.npmjs.com/trusted-publishers/), [provenance
statements](https://docs.npmjs.com/generating-provenance-statements/), and [access
tokens](https://docs.npmjs.com/creating-and-viewing-access-tokens/).

## Consumer verification

For the native release:

```sh
gh release view v0.1.0-alpha.0 --repo oxhq/stasis \
  --json tagName,isImmutable \
  --jq 'select(.tagName == "v0.1.0-alpha.0" and .isImmutable == true)'
gh release verify v0.1.0-alpha.0 --repo oxhq/stasis
gh release download v0.1.0-alpha.0 --repo oxhq/stasis --dir stasis-release
cd stasis-release
for asset in \
  stasis-0.1.0-alpha.0-macos-aarch64.tar.gz \
  stasis-0.1.0-alpha.0-macos-aarch64.tar.gz.sha256 \
  stasis-0.1.0-alpha.0-macos-aarch64.binary.sha256 \
  stasis-0.1.0-alpha.0-macos-aarch64-act-settle-inspect.json
do
  gh release verify-asset v0.1.0-alpha.0 "$asset" --repo oxhq/stasis
done
shasum -a 256 -c stasis-0.1.0-alpha.0-macos-aarch64.tar.gz.sha256
gh attestation verify stasis-0.1.0-alpha.0-macos-aarch64.tar.gz \
  --repo oxhq/stasis \
  --signer-workflow oxhq/stasis/.github/workflows/stasis-package.yml
tar -xzf stasis-0.1.0-alpha.0-macos-aarch64.tar.gz
shasum -a 256 -c stasis-0.1.0-alpha.0-macos-aarch64.binary.sha256
```

For npm:

```sh
npm view @oxhq/stasis@0.1.0-alpha.0 version dist.integrity
npm view @oxhq/stasis dist-tags --json
mkdir stasis-audit && cd stasis-audit
npm install --ignore-scripts @oxhq/stasis@0.1.0-alpha.0
npm audit signatures --include-attestations
```
