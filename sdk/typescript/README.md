# `@oxhq/stasis`

TypeScript client for the Stasis controlled-time Servo runtime. The
`v0.2` API owns controlled web sessions and serializes every command through a
single FIFO protocol lane.

## Install

```sh
# Last fully qualified public package while the 0.3.2 gates are pending:
pnpm add @oxhq/stasis@0.2.1

# Use only after the 0.3.2 registry and anonymous-consumer gates pass:
pnpm add @oxhq/stasis@0.3.2
```

This corrective source/package train is versioned `0.3.2`; install those immutable bytes explicitly
with `pnpm add @oxhq/stasis@0.3.2` after the registry, release, provenance, and anonymous-consumer
gates have published them. Untagged source alone does not prove that publication occurred.
`@oxhq/stasis@0.2.1` remains the last fully qualified predecessor; public `0.3.0` is immutable
disqualified release evidence after its macOS anonymous-consumer failure. The immutable `v0.3.1`
GitHub release is also disqualified because automatic npm prepublication failed in the packed
SDK's cookie-replacement settlement; `@oxhq/stasis@0.3.1` was never published.

An exact stable package pairs the TypeScript package with the same-version native
`stasis-shell`, sourced from `https://github.com/oxhq/stasis.git`. Node.js 20 or newer is required. The
SDK has no install lifecycle scripts or runtime dependencies. On the release's
macOS Apple Silicon and Linux x86-64 targets, `launch()` downloads the exact release
archive on first use, verifies its declared size, archive SHA-256, complete
file inventory, and executable SHA-256, then installs it atomically in a
digest-keyed per-user cache. Unsupported hosts fail closed and can use an
explicit compatible executable. The SDK always starts it directly with
`shell: false`.

The source identity is never a registry or release claim by itself. Before promotion, the exact
tag, package, provenance, publication, and anonymous-consumer gates must complete; after
promotion, verify the immutable tag and registry bytes rather than inferring status from this text.

## Use: controlled web sessions (v0.2)

```ts
import { launch } from "@oxhq/stasis";

const runtime = await launch();
const session = await runtime.openSession("https://app.example.test/login", {
  network: { mode: "live", routes: [] },
});

try {
  const initial = await session.settle(session.stateToken, {
    persistentWork: "report",
    wallIoTimeoutNs: 10_000_000_000n,
    maxVirtualTimeNs: 30_000_000_000n,
    maxControlTurns: 100_000n,
  });
  if (initial.outcome !== "quiescent") {
    throw new Error(`login did not settle: ${initial.outcome}`);
  }

  let token = (
    await session.fill("#email", "gara@example.test", initial.stateToken)
  ).stateToken;
  token = (await session.fill("#password", "fixture password", token)).stateToken;
  token = (await session.submit("#login-form", token)).stateToken;

  const dashboard = await session.settle(token);
  const links = await session.extract(
    {
      rootSelector: "main",
      fields: [
        { name: "next", selector: "a.next", read: "resolved_url", attribute: "href" },
      ],
    },
    dashboard.stateToken,
  );
  const next = links.rows[0]?.fields[0]?.value;
  if (typeof next !== "string") throw new Error("dashboard has no next link");

  const navigated = await session.navigate(next, links.stateToken);
  const detail = await session.settle(navigated.stateToken);

  console.log(await session.text("#status", detail.stateToken));
  console.log((await session.exportState()).state);
  console.log((await session.requests()).records);
  console.log((await session.evidence()).records);
} finally {
  await session.close();
}
```

`Runtime.openSession()` selects `controlled-web-session-v1`. Its document
operations consume and return opaque state tokens, so a token from before a DOM
mutation or navigation cannot authorize later work. The v0.2 surface includes
semantic form operations (`focus`, `fill`, `check`, `uncheck`, `select`, and
`submit`), query/text/structured extraction, checked navigation, cookies and
Web Storage export/import-at-open, immutable network routes, and bounded redacted
request/evidence pages. `createStasisSessionPool()` and `crawlWithStasis()` add
process-isolated concurrency and a same-origin crawler without weakening the
one-session-per-process boundary.

## Modern-web session profile (v0.3)

Version 0.3 exposes the explicitly selected `controlled-web-session-v2` profile. Checked-in source
is not a publication claim: the native runtime must advertise v2, and omitting `profile` still
selects v1.

```ts
import {
  CONTROLLED_WEB_SESSION_V2_PROFILE,
  launch,
} from "@oxhq/stasis";

const runtime = await launch({ executablePath: "/path/to/a/v2-capable/stasis" });
const session = await runtime.openSession("https://app.example.test/", {
  profile: CONTROLLED_WEB_SESSION_V2_PROFILE,
});
try {
  const settled = await session.settle(session.stateToken);
  console.log(new URL(settled.url).pathname);
  console.log(session.settlementEvidence(settled));
} finally {
  await session.close();
}
```

For an explicitly selected v2 session, every returned settle outcome has a required `url`. It is
the active top-level document URL from the same final owner authority that produced
`settled.stateToken`, after the passive N1/document-pending-D/passive-N2 bracket agrees. Its presence
does not imply quiescence. `session.url` remains the open-time URL; after same-document history
changes or document replacement, use `settled.url` instead of polling or treating `session.url` as
mutable. Frozen v1 settle results do not gain this field. Settlement evidence deliberately omits
the URL and retains its existing redaction contract.

V2 adds bounded same-global, untransferred `MessageChannel` delivery; a bounded direct
`HTMLImageElement.src` cache/decode completion path for canonical data SVG and direct HTTP(S)
selection; a distinct bounded inline
`<svg>` path for its exact cached internally serialized data-SVG request and cache-ID owner; and
suppression of only
page-driven, single-line text InputMethod presentation when no virtual keyboard is requested in the
exact public non-auxiliary controlled top-level document. It also adds controlled in-memory
persistent-cookie expiry and bounded schemeful SameSite request selection. The image path requires direct `src` selection without
`srcset`/`picture`/environment changes, an initial serialized URL no larger than 65,536 bytes, the
same ScriptThread/ImageCache, and room within the
 512-record retained controlled-ownership cap. Controlled callbacks, layout owners, DOM identities,
 raster keys, and raster owners each consume that shared capacity through their exact lifetimes.
 An admitted synchronous cache hit is owned in the
current Script turn and queues its existing ordinary DOM callback without inventing an asynchronous
`Image` producer lease. Finite asynchronous cache/decode completion is producer-fenced, and one
document-clock timestamp is shared across the engine-generated image completion events. Cache-owned
callback retirement completes its Image stream as owned cancellation,
as does a dequeued response whose closed-pipeline tombstone proves that navigation retired its
Window. A normal live handler rejection keeps the owner or key pending, completes the scoped
message guard, and settles as typed `unsupported_rendering`; admission, enqueue, producer callback
panic, actual handler unwind, pre-handler authority, target-invariant or clock failure, and guarded transport loss
explicitly abandon the stream and retain a terminal. Either terminal or ordinary completion must
match the exact live fence/sequence and registered Image class.
Canonical data URLs must be exact `image/svg+xml`. A direct HTTP(S) URL is admitted from its
initially selected scheme before response metadata or decoded format is known; network I/O remains
separately owned, and finite decode failure is an owned error completion. A post-metadata
`multipart/x-mixed-replace` response remains typed unsupported after separately owned finite
Resource I/O drains, without baseline callback fallback; an endless response remains blocked on
that external I/O. Cache reuse is proven within one pipeline's image-cache store and immutable fixture
routes, not as a deterministic claim for live or mutable HTTP content. Public document replacement
while HTTP image resource I/O remains active retains fatal `blocked_on_external_io`; v2 does not
claim cross-document replacement through that state.
The inline path additionally requires an internal request and the same
canonical MIME/URL bound; its decode/vector work is fenced, but it creates no DOM load event.
 An identical current inline root may join an already-pending response when its exact cache-key URL
 and `PendingImageId` match an existing same-ID fenced layout record and every live callback is
 fenced with that exact producer URL key. The callback owns this key through terminal removal, so
 an earlier DOM owner may already have unbound and its identity is neither required nor trusted.
 The current owner is retained once, no baseline work may coexist, and the peer reuses the existing
 listener and producer without adding a listener, producer, or fetch. Missing keys or anchors,
 stale candidates, mixed provenance, and retained-capacity exhaustion fail closed.
This does not promote baseline or v1 work, external or nested SVG resources, iframes, workers,
worklets, or cross-loop image work.

Local-channel authority is not borrowed through a same-origin wrapper. Construction requires the
exact active public top-level target and an incumbent matching the owner global, pipeline, and
WebView before either port is published. `postMessage()` performs the same check before structured
cloning, so a missing or foreign incumbent, replaced/discarded target, or auxiliary owner is rejected
before serialization or transfer detachment.

Programmatic focus, including React `autoFocus`, preserves DOM events, value, and selection in that
exact public target; its engine-generated focus transitions expose document-clock `Event.timeStamp`. Public mutating
automation also samples the document Performance clock once before each action and shares it with
every browser-created event constructed synchronously during that action. The fill, activation,
reset, check/uncheck, select, invalid, submit, and formdata proof is representative, not an event-name
allowlist. Each nonempty owned CSS animation pending-event dispatch batch also samples the document
Performance clock once and stamps only internal `AnimationEvent` or `TransitionEvent` records owned
by the exact public non-auxiliary controlled top-level WebView/document. The `TransitionEvent` adapter is conditional
on an existing owned transition record reaching that queue; general transition settlement is not
claimed. Auxiliary top-level WebViews remain host-stamped. Literal HTML `autofocus`, script-created event
timestamps including `new AnimationEvent(...)` and `new TransitionEvent(...)`, other InputMethod
shapes, and embedder controls remain unsupported, as do transferred, cross-global,
cross-event-loop, worker, BroadcastChannel, and external channels. Blob/file and non-SVG data image sources, responsive-image selection,
CSS/background/generated-content ownership, animated images, general or nested/external SVG
resource semantics, and cross-context image work receive no new v2 authority; baseline and v1 SVG
behavior, CSS animation semantics and limits, and predecessor rejection authorities remain in force.
A nonempty document-owned pending CSS animation-event queue is finite demand and retains one later
owned rendering opportunity until dispatch drains it; an empty queue leaves no opportunity. Live
scheduled work uses guarded `AdvanceTo` at the exact retained scheduler head, and only an
unscheduled batch is `Drive`-ready. This is a liveness correction, not another task source or
limit. With `persistentWork: "report"`, v2 may likewise advance an eligible exact JavaScript
interval scheduler head while finite work remains. Finite timer and animated-image deadlines must
be strictly later. One finite rendering opportunity may share the timestamp only as a distinct
exact same-scheduler owner whose `TimerId` sequence follows the interval head; same-entry,
lower-or-equal-order, foreign-scheduler, bare/unowned, equal finite-timer, and equal animated-image
collisions remain blocked. Each activation is bound by the ordinary single-use complete-snapshot
advance token, and each callback consumes the existing ordinary-task and downstream execution
budgets. Once finite work drains, settlement checkpoints without firing another interval cycle
and returns `quiescent_with_persistent_work`. Strict policy and both predecessor profiles retain
`blocked_on_open_ended_work` at that head.
V2 cookie expiry uses controlled Unix nanoseconds with origin zero. `Max-Age` precedes `Expires`,
lifetime is clamped to 400 days, and expired records are lazily purged before observation, request
selection, and export. SameSite uses the captured schemeful site-for-cookies, current redirect-hop
method, and top-level-navigation bit. Strict is same-site only; Lax and unspecified also admit
cross-site top-level safe methods; Secure None cookies may cross site. Unknown or opaque context
remains typed unsupported. After successful controlled parsing, cross-site
subresource responses retain only valid Secure SameSite=None cookies; otherwise valid
Strict/Lax/unspecified values are ignored, while parse, normalization, and time-range failures
retain their existing typed outcomes. Top-level-navigation responses admit all otherwise valid
unpartitioned cookies. A post-open request at controlled Unix time above u64 fails nonfatally as
`unsupported_cookie_time_range` instead of truncating; initial controlled open hardens the same
code to fatal fail-stop. Either post-open typed rejection may retain bounded `request_started` and
`request_failed` evidence, but it occurs before `route_decided` or route selection, fixture or live
external I/O, and Cookie header construction. Partitioned cookies
plus CookieStore read/getAll/delete remain unsupported.

The SDK keeps `SessionCookie` and `SessionState` as frozen v1-compatible names. Explicit v2 code
uses `SessionCookieV2` and `SessionStateV2`; `Session<Profile>` makes cookie reads, writes, imports,
and exports profile-specific. A v2 artifact keeps `schemaVersion: 1` but has literal
`profile: "controlled-web-session-v2"` and `expiresUnixTimeNs: bigint | null`. It is portable only
through explicit export and initial import; no v1 migration or disk-backed browser jar is implied.
`ReferenceCrawlerOptions` likewise defaults to v1. Its optional `profile` member remains source
compatible for existing v1 annotations. A standalone v2 annotation can therefore describe
an incompletely constructed value, but it cannot be passed to `crawlWithStasis()` until it carries
`profile: CONTROLLED_WEB_SESSION_V2_PROFILE`; at that call boundary the explicit profile is inferred
and a v1 state artifact is rejected.
Settlement evidence
carries the runtime-bound selected profile identity. One-argument
`settlementEvidence(settled)` preserves that binding for SDK-produced results, and contradictory
explicit profile claims are rejected. See the
[v2 contract](../../docs/stasis/session-v0.3-candidate.md).

## Legacy controlled document (v0.1)

`Runtime.open()` is the frozen legacy v1 document API. It remains available for
`controlled-webapp-v1` consumers and uses numeric state generations:

```ts
const runtime = await launch();
const app = await runtime.open("https://example.com");
try {
  const settled = await app.settle();
  console.log(await app.text("h1", settled.stateGeneration));
} finally {
  await app.close();
}
```

`executablePath` is the highest-priority override and performs no managed
download or cache access:

```ts
const runtime = await launch({ executablePath: "/opt/stasis/bin/stasis" });
```

Use `runtimeCacheDirectory` to relocate the managed cache. By default it is
stored under the operating system's per-user cache directory
(`~/Library/Caches/oxhq/stasis` on macOS). Acquisition requires HTTPS and an
exact SDK-version/platform manifest match; cache hits are rehashed before use.

`openSession()` is always controlled and defaults to `controlled-web-session-v1`; an explicit v2
session profile must be selected explicitly and advertised by the runtime.
For the legacy `open()` API, omitting `clock` selects Controlled mode and the
frozen `controlled-webapp-v1` profile; select Real mode explicitly with
`{ clock: { mode: "real" } }`. `evaluate()` is available only through that
legacy Real-mode API. Stasis does not virtualize live network content, ambient
system state, or randomness. `Runtime.close()` is an abrupt local termination
for startup/error cleanup; `Session.close()` and `App.close()` are the graceful
protocol operations.

## Cancellation and failures

Every command accepts an `AbortSignal` and an optional `timeoutMs`. The launch-level
`commandTimeoutMs` defaults to 30 seconds and supervises every written native command. A queued
abort removes only that command with `stateEffect: "none"`. Once a command has been written, an
abort or timeout terminates the child and fail-stops the session or app; mutating commands report
`stateEffect: "indeterminate"` instead of being retried. Command deadlines reject with
`StasisCommandTimeoutError`.
Aborting runtime acquisition removes its temporary download/extraction state
and never publishes a partial final cache entry.

Protocol responses are strictly correlated by request ID, session ID, and monotonic `wireSeq`.
Malformed, duplicate-key, oversized, unmatched, or out-of-sequence output terminates the child.
Protocol-declared operation failures reject with `StasisProtocolError`; child/pipe failures reject
with `StasisProcessError` or `StasisTransportError`. Abort and timeout errors carry `fatal`,
`stateEffect`, method, and request identity when a command was written. Errors retain only a bounded tail of stderr
(64 KiB by default), while stdout is treated exclusively as NDJSON protocol data.

Stasis intentionally has no persistent DOM handles or locators. The v0.2
session API requires a fresh document state token from `openSession()`,
`pending()`, `settle()`, or the preceding operation; stale tokens fail instead
of acting on a changed document. Cookie and storage mutations use a separate
session-state token. `query()` returns a bounded match count, never handles, and
`extract()` returns ordered text, HTML, raw-attribute, or resolved-URL fields.
The only relaxation is `settle()`: the sole latest-issued token may consume
monotonic work admitted on that exact same document after the preceding result,
which is what makes `submit()` followed immediately by `settle()` race-free.
Valid nonterminal bracket drift latches that exact token stale; a typed
navigation terminal keeps its typed outcome and settlement never starts.
Actions, inspection, advancement, and explicit navigation remain exact-token
operations, and a token can never cross into another document.
The legacy v1 API keeps its numeric state-generation contract. Screenshot,
replay, and time-travel methods are not part of the v0.2 surface.

## Development

`pnpm pack` fails closed unless the checked-in generated runtime manifest is
canonically bound to the exact source package version, both supported native
platforms, and the twelve-file release archive inventory. Release automation must
generate that manifest from the gated native artifacts before packing.

```sh
pnpm install
pnpm typecheck
pnpm test
pnpm build
```
