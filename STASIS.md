# Stasis

Stasis is an experimental Servo port for executing a supported web application
as a controlled event system.

The 0.1 thesis established the intentionally narrow document boundary:

> Under a declared support profile, the engine drives all owned work without
> wall-clock polling and returns either a stable settlement proof or a typed
> blocker or execution limit.

The published 0.2 release extends that proof from one controlled document to a
bounded controlled web session without broadening into a complete browser-test
API. The product loop becomes:

```text
open -> act -> settle -> inspect -> navigate -> preserve session state -> repeat
```

This branch starts from clean Servo. Pliego is an implementation donor for the
controlled document clock, guarded timer advancement, producer fences,
execution limits, and wake-driven owner loop. PDF, pagination, Paint capture,
retained Canvas, publication, and Pliego SDK code are outside the donor
boundary.

Release identity and immutable boundaries:

- Servo base and Pliego donor revisions are pinned in `STASIS_UPSTREAM.toml`.
- `v0.2.1` is the immutable stable predecessor. The source tree's native crate
  and TypeScript package are versioned `0.3.0` for the next exact release train
  and retain the frozen `controlled-webapp-v1` surface. The separately named
  `controlled-web-session-v1` profile adds document/navigation token authority,
  checked top-level replacement and history changes, semantic forms, practical
  selectors and URL/attribute extraction, bounded cookie and Web Storage state,
  immutable network fixtures, and redacted request/evidence projections.
- Controlled document time covers DOM timers, `Date`, Performance, rAF, and
  the document timeline inside one audited Script event loop. The frozen
  `controlled-webapp-v1` profile owns one top-level document;
  `controlled-web-session-v1` already admits checked replacement documents on
  that same event loop. `controlled-web-session-v2` preserves that session and
  navigation authority and expands only declared execution, headless-presentation, and controlled
  cookie-state surfaces. Unsupported or
  open-ended work is reported as a typed outcome instead of silently falling
  back to uncontrolled progress.
- The published `@oxhq/stasis@0.2.1` remains immutable release history. The
  `sdk/typescript` source is the matching `0.3.0` train plus process-isolated
  session pooling and crawling helpers. The release
  workflows bind the SDK and both native archives to one source revision, retain
  the frozen v0.1 fixture gate, and add the multi-navigation/session-state North
  Star before promotion.
- Source version `0.3.0` is not a publication claim. Before promotion, `v0.3.0`
  becomes released only after the macOS arm64 and Linux x86-64 provenance gates,
  immutable GitHub release, npm trusted publication, anonymous managed-runtime
  verification, and all public and candidate protocol gates pass. After promotion,
  status is established by those immutable public artifacts rather than this source copy.
- The immutable `v0.2.0` and `v0.2.1` artifacts remain release history. `v0.2.1` corrects
  a redirect-evidence ordering race in which a successor request could begin
  before its predecessor's terminal callback and omit that predecessor's
  response evidence.
- `profiles/controlled-web-session-v2.json` and the TypeScript selector define the versioned 0.3.0
  contract's bounded same-global, untransferred `MessageChannel` delivery; direct top-level
  `HTMLImageElement.src` completion for canonical `data:image/svg+xml` or an initially selected
  canonical HTTP(S) URL no larger than 65,536 bytes; a distinct bounded inline
  `<svg>` path whose internal serialized data-SVG request must exactly match that element's cached
  URL and join its exact cache-ID owner. An identical current inline root may join an exact
  retained producer whether layout reports `PendingResponse` or a stale/reentrant `Unrequested`:
  its exact cache-key URL and `PendingImageId` must match an existing same-ID fenced layout record,
  while every live callback must be fenced and retain that exact producer URL key. The producer
  key outlives an earlier DOM owner's unbind only until terminal callback removal; no earlier live
  DOM identity is required or trusted. No baseline work may be retained. A new current owner is
  retained once and an already-retained owner is idempotent. The join reuses the existing listener
  and producer; it adds no listener, producer, or fetch. Every controlled callback, layout owner,
  DOM identity, raster key, and raster owner consumes the shared 512-record capacity until its
  exact lifetime ends. A missing anchor/key, stale candidate, mismatch, mixed provenance, or
  capacity terminal fails closed and cannot promote baseline work. Baseline, v1, external,
  nested-SVG, iframe, worker, worklet, and cross-loop authority remains unchanged. V2 also adds the
  narrow exact public non-auxiliary controlled top-level, single-line
  `InputMethodType::Text` presentation suppression only when `multiline = false` and no virtual
  keyboard is requested. Admitted HTML image completion events and engine-generated focus
  transitions expose document-clock `Event.timeStamp` values. Every browser-created event
  constructed synchronously during one public mutating automation action shares one pre-mutation
  document-clock sample; the form-event corpus is representative, not an event-name allowlist.
  Cache-owned image callback retirement and delivery whose closed-pipeline tombstone proves that
  navigation retired the target Window are owned cancellations. A normal live handler rejection
  preserves the pending owner or key, completes its scoped message guard, and becomes typed
  `unsupported_rendering`; admission, enqueue, producer callback panic, actual handler unwind,
  pre-handler authority, target-invariant or clock failure, and guarded transport loss remain sticky
  abandonment terminals.
  Completion and abandonment must match the exact live lease and registered Image producer class.
  An admitted synchronous image-cache hit is owned in the current Script turn and queues its
  existing ordinary DOM callback without inventing an asynchronous `Image` producer lease. Finite
  asynchronous cache/decode completion is fenced by an `Image` producer through ScriptThread
  handoff. HTTP(S) Resource I/O remains separately owned. A final redirect URL is not rechecked
  against the initial 65,536-byte selection bound; the redirected fetch and immutable session
  network policy remain authoritative. Deterministic cache reuse is proven only within one
  pipeline's image-cache store under immutable fixture routes, not across pipelines or for live or
  mutable HTTP content. A finite `multipart/x-mixed-replace` response becomes typed
  `unsupported_rendering` / `image_load` after its separately owned Resource I/O drains, without a
  baseline callback or fake quiescence; an endless response remains blocked on external I/O.
  Public document replacement while HTTP image Resource I/O is active remains fatal
  `blocked_on_external_io` before successor-document authority.
  Local-channel construction and posting require the exact active public top-level target plus an
  incumbent matching the owner global, pipeline, and WebView, before pair publication or structured
  cloning respectively. Each nonempty owned CSS animation pending-event dispatch batch also receives one document-clock
  sample, shared only by internal `AnimationEvent` or `TransitionEvent` records in the exact public non-auxiliary
  controlled top-level WebView/document. The implemented `TransitionEvent` adapter is conditional
  on an existing owned transition record reaching that queue; general transition settlement is not
  claimed. A nonempty document-owned pending CSS animation-event queue is finite demand and
  retains one later owned rendering opportunity until dispatch drains it; an empty queue leaves no
  rendering opportunity. A live scheduled batch uses guarded `AdvanceTo` at the exact retained
  scheduler head, while only an unscheduled batch is `Drive`-ready. This closes a checkpoint-spin
  liveness bug without adding a task source or limit. Auxiliary WebViews remain host-stamped. Inline SVG gains no DOM completion-event claim,
  and script-created animation/transition constructors, excluded
  image, animated/decode-timeline, decoder-resource-budget, general SVG/resource, and other
  unlisted host-stamped paths remain unsupported or retain
  their frozen predecessor authority. V2 also owns persistent cookies in memory against controlled
  Unix time, with Max-Age precedence, a 400-day clamp, lazy expiry purge, and exact v2
  export/import identity. SameSite request selection uses captured schemeful site-for-cookies, the
  current redirect-hop method, and the top-level-navigation bit; ineligible cookies are filtered,
  while unknown or opaque context stays typed unsupported. Cross-site
  subresource responses, after successful controlled parsing, retain only valid Secure
  SameSite=None cookies; otherwise valid Strict/Lax/unspecified values are ignored. Parse,
  normalization, and time-range failures retain their existing typed outcomes. Top-level
  responses admit every otherwise valid unpartitioned cookie. Controlled Unix time above u64 is a
  nonfatal `unsupported_cookie_time_range` boundary for a post-open request, never a truncation;
  initial controlled open hardens the same code to fatal fail-stop. Either post-open typed
  rejection may retain bounded `request_started` and `request_failed` evidence, but it occurs before
  `route_decided` or route selection, fixture or live external I/O, and Cookie header construction.
  Partitioned
  cookies and the deferred CookieStore read/getAll/delete methods receive no new authority.
  Every returned v2 settle result additionally includes the active top-level document `url` from
  the same final passive-N1/document-pending-D/passive-N2 owner authority that binds its
  `stateToken`. The field is required on every returned outcome and does not imply quiescence.
  `Session.url` remains the open-time value; this projection adds no poll or mutable session
  property, leaves action/navigation and frozen v1 result shapes unchanged, and remains excluded
  from bounded redacted settlement evidence.
  With `persistentWork: "report"`, v2 may also advance an eligible exact JavaScript interval only
  while every observed finite timer and animated-image deadline is strictly later. One finite
  rendering opportunity may share its timestamp only as a distinct exact same-scheduler owner
  whose `TimerId` sequence follows the interval head; Stasis dispatches the interval and reobserves
  before that rendering entry. Same-entry, lower-or-equal-order, foreign-scheduler, bare/unowned,
  equal finite-timer, and equal animated-image collisions remain blocked. Each activation uses the ordinary
  single-use advance token and its callback consumes the existing task/microtask/rendering,
  mutation, control-turn, and virtual-time budgets. Once finite work drains, an interval-only
  document is checkpointed and returned as `quiescent_with_persistent_work` without another
  interval cycle. `strict` and both predecessor profiles retain their stop-at-interval behavior.
  It is explicit-only,
  leaves the v1 session default and v1 behavior unchanged, and is not a public release claim
  until exact v0.3 artifacts pass the hosted promotion and public-consumer gates.
- The owner-loop progress path contains no polling sleeps; this is not a claim
  about every shutdown path inherited from upstream Servo. The frozen v1 wire
  methods and exclusions are defined in `docs/stasis/protocol-v1.md`; the v0.2
  controlled-session contract is defined in `docs/stasis/session-v0.2.md` and
  `profiles/controlled-web-session-v1.json`.

See:

- `docs/stasis/architecture.md`
- `docs/stasis/pliego-donor-map.md`
- `docs/stasis/protocol-v1.md`
- `docs/stasis/settlement-v0.1.md`
- `docs/stasis/journal-v0.1.md`
- `docs/stasis/session-v0.2.md`
- `docs/stasis/session-v0.3-candidate.md`
- `profiles/controlled-web-session-v1.json`
- `profiles/controlled-web-session-v2.json`
