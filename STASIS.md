# Stasis

Stasis is an experimental Servo port for executing a supported web application
as a controlled event system.

The 0.1 thesis is intentionally narrow:

> Under a declared support profile, the engine drives all owned work without
> wall-clock polling and returns either a stable settlement proof or a typed
> blocker or execution limit.

The product loop is:

```text
open -> act -> settle -> inspect
```

This branch starts from clean Servo. Pliego is an implementation donor for the
controlled document clock, guarded timer advancement, producer fences,
execution limits, and wake-driven owner loop. PDF, pagination, Paint capture,
retained Canvas, publication, and Pliego SDK code are outside the donor
boundary.

Current status:

- Servo base and Pliego donor revisions are pinned in `STASIS_UPSTREAM.toml`.
- `ports/stasis` provides the first wake-driven embedded baseline over NDJSON:
  `protocol.initialize`, `session.open`, `dom.evaluate`, and `session.close`.
  Its owner-loop progress contains no polling sleeps; this is not a claim about
  every shutdown path inherited from upstream Servo.
- Controlled time, pending-work snapshots, settlement, actions, and the
  TypeScript SDK are not claimed by this baseline yet.

See:

- `docs/stasis/architecture.md`
- `docs/stasis/pliego-donor-map.md`
- `docs/stasis/protocol-v1.md`
- `docs/stasis/settlement-v0.1.md`
- `docs/stasis/journal-v0.1.md`
