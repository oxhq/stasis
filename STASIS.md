# Stasis

Stasis is an experimental Servo port for executing a supported web application
as a controlled event system.

The 0.1 thesis established the intentionally narrow document boundary:

> Under a declared support profile, the engine drives all owned work without
> wall-clock polling and returns either a stable settlement proof or a typed
> blocker or execution limit.

The stable 0.2 release extends that proof from one controlled document to a
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

Current release status:

- Servo base and Pliego donor revisions are pinned in `STASIS_UPSTREAM.toml`.
- `ports/stasis` is versioned for the stable `v0.2.0` release and retains the
  frozen `controlled-webapp-v1` surface. The separately named
  `controlled-web-session-v1` profile adds document/navigation token authority,
  checked top-level replacement and history changes, semantic forms, practical
  selectors and URL/attribute extraction, bounded cookie and Web Storage state,
  immutable network fixtures, and redacted request/evidence projections.
- Controlled document time covers DOM timers, `Date`, Performance, rAF, and
  the document timeline inside one audited Script event loop. The frozen v1
  profile owns one top-level document; the v2 session profile admits checked
  replacement documents on that same event loop. Unsupported or open-ended
  work is reported as a typed outcome instead of silently falling back to
  uncontrolled progress.
- `sdk/typescript` provides the matching stable `@oxhq/stasis@0.2.0`
  client plus process-isolated session pooling and crawling helpers. The release
  workflows bind the SDK and both native archives to one source revision, retain
  the frozen v0.1 fixture gate, and add the multi-navigation/session-state North
  Star before promotion.
- This source version is not a publication claim. `v0.2.0` becomes released
  only after the macOS arm64 and Linux x86-64 provenance gates, immutable GitHub
  release, npm trusted publication, anonymous managed-runtime verification, and
  both public North Stars pass.
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
- `profiles/controlled-web-session-v1.json`
