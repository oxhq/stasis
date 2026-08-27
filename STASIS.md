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
- `profiles/controlled-web-session-v2.json` and the TypeScript selector define the 0.3.0
  candidate's bounded same-global, untransferred `MessageChannel` delivery; direct top-level
  `HTMLImageElement.src` `data:image/svg+xml` cache/decode completion; a distinct bounded inline
  `<svg>` path whose internal serialized data-SVG request must exactly match that element's cached
  URL and join its exact cache-ID owner; and narrow exact public non-auxiliary controlled top-level, single-line
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
  Local-channel construction and posting require the exact active public top-level target plus an
  incumbent matching the owner global, pipeline, and WebView, before pair publication or structured
  cloning respectively. Each nonempty owned CSS animation pending-event dispatch batch also receives one document-clock
  sample, shared only by internal `AnimationEvent` or `TransitionEvent` records in the exact public non-auxiliary
  controlled top-level WebView/document. The implemented `TransitionEvent` adapter is conditional
  on an existing owned transition record reaching that queue; general transition settlement is not
  claimed. A scheduled pending animation-event batch is finite demand for guarded `AdvanceTo` at
  the exact retained scheduler head, while only an unscheduled batch is `Drive`-ready; this closes a
  checkpoint-spin liveness bug without adding a task source or limit. Auxiliary WebViews remain host-stamped. Inline SVG gains no DOM completion-event claim,
  and script-created animation/transition constructors, excluded
  image, general SVG/resource, and other unlisted host-stamped paths remain unsupported or retain
  their frozen predecessor authority. V2 also owns persistent cookies in memory against controlled
  Unix time, with Max-Age precedence, a 400-day clamp, lazy expiry purge, and exact v2
  export/import identity. SameSite request selection uses captured schemeful site-for-cookies, the
  current redirect-hop method, and the top-level-navigation bit; ineligible cookies are filtered,
  while unknown or opaque context stays typed unsupported before network start. Cross-site
  subresource responses retain only valid Secure SameSite=None cookies; top-level responses admit
  every otherwise valid unpartitioned cookie. Controlled Unix time above u64 is a nonfatal
  `unsupported_cookie_time_range` boundary for a post-open request before network start, never a
  truncation; initial controlled open hardens the same code to fatal fail-stop. Partitioned
  cookies and the deferred CookieStore read/getAll/delete methods receive no new authority.
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
