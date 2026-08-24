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
- `ports/stasis` ships the `v0.1.0` wake-driven NDJSON runtime. Real
  sessions support initialization, open, evaluation, and close. Controlled
  sessions add exact pending-work snapshots, bounded settlement and virtual
  advancement, generation-bound activation and text inspection, and
  cancellation, semantic fill, bounded queries, and structured extraction.
- Controlled document time covers DOM timers, `Date`, Performance, rAF, and
  the document timeline inside the stable profile's audited single-top-level-document
  support boundary. Unsupported or open-ended work is reported as a typed
  outcome instead of silently falling back to uncontrolled progress.
- `sdk/typescript` provides the matching `@oxhq/stasis` client, while the
  release workflows bind the SDK and native archive to one source revision and
  verify the act-settle-inspect fixture before promotion.
- The owner-loop progress path contains no polling sleeps; this is not a claim
  about every shutdown path inherited from upstream Servo. The exact shipped
  methods and exclusions are defined in `docs/stasis/protocol-v1.md`.

See:

- `docs/stasis/architecture.md`
- `docs/stasis/pliego-donor-map.md`
- `docs/stasis/protocol-v1.md`
- `docs/stasis/settlement-v0.1.md`
- `docs/stasis/journal-v0.1.md`
