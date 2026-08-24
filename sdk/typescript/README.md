# `@oxhq/stasis`

TypeScript client for the Stasis controlled-time Servo runtime. The
`v0.1` API owns one Stasis process, opens one app, and serializes every
command through a single FIFO protocol lane.

## Install

```sh
pnpm add @oxhq/stasis
```

For a byte-for-byte reproducible dependency selection, install
`pnpm add @oxhq/stasis@0.1.0`.

The published pair is `@oxhq/stasis@0.1.0` and native implementation
`stasis-shell` version `0.1.0`, sourced from
`https://github.com/oxhq/stasis.git`. Node.js 20 or newer is required. The
SDK has no install lifecycle scripts or runtime dependencies. On the published
macOS Apple Silicon and Linux x86-64 targets, `launch()` downloads the exact release
archive on first use, verifies its declared size, archive SHA-256, complete
file inventory, and executable SHA-256, then installs it atomically in a
digest-keyed per-user cache. Unsupported hosts fail closed and can use an
explicit compatible executable. The SDK always starts it directly with
`shell: false`.

## Use

```ts
import { launch, settlementEvidence } from "@oxhq/stasis";

const runtime = await launch();
// Controlled time and controlled-webapp-v1 are the SDK defaults.
const app = await runtime.open("https://example.com");

try {
  const initial = await app.settle();
  if (initial.outcome !== "quiescent") {
    throw new Error(`app did not reach its initial controlled state: ${initial.outcome}`);
  }
  let generation = (
    await app.fill("#email", "gara@example.test", initial.stateGeneration)
  ).stateGeneration;
  generation = (
    await app.fill("#password", "correct horse battery staple", generation)
  ).stateGeneration;
  generation = (await app.activate("#start", generation)).stateGeneration;

  const settled = await app.settle({
    persistentWork: "report",
    wallIoTimeoutNs: 10_000_000_000n,
    maxVirtualTimeNs: 30_000_000_000n,
    maxControlTurns: 100_000n,
  });

  console.log(settled.outcome, settled.virtualTimeNs); // bigint, never a lossy number
  console.log(settlementEvidence(settled)); // bounded, allow-listed terminal evidence
  console.log(await app.text("#status", settled.stateGeneration));

  const cards = await app.query(".card", settled.stateGeneration);
  const extracted = await app.extract(
    {
      rootSelector: ".card",
      fields: [
        { name: "title", selector: ".title", read: "text" },
        { name: "details", selector: ".details", read: "html" },
      ],
    },
    cards.stateGeneration,
  );
  console.log(cards.count, extracted.rows);

  await app.advanceToNext();
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

Omitting `clock` selects Controlled mode and the frozen `controlled-webapp-v1`
profile. Select Real mode explicitly with `{ clock: { mode: "real" } }`;
`evaluate()` is available only there. Controlled sessions use the settlement
operations shown above. Stasis does not virtualize live network content,
ambient system state, or randomness.
`Runtime.close()` is an abrupt local termination for startup/error cleanup; `App.close()` is the
graceful protocol operation.

## Cancellation and failures

Every command accepts an `AbortSignal` and an optional `timeoutMs`. The launch-level
`commandTimeoutMs` defaults to 30 seconds and supervises every written native command. A queued
abort removes only that command with `stateEffect: "none"`. Once a command has been written, an
abort or timeout terminates the child and fail-stops the app; mutating commands report
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

The MVP intentionally has no persistent DOM handles or locators. `fill()`, `activate()`, `query()`,
`text()`, and `extract()` require an explicit fresh state generation obtained from `pending()`,
`settle()`, or the preceding automation result; stale generations fail instead of acting on a
changed document. `query()` returns a bounded match count, never handles. `extract()` returns
ordered rows whose fields remain in request order and currently reads raw `textContent` or
`innerHTML`. In a real-time session, `evaluate()` is a single-command escape hatch and does not
reinterpret decimal-looking strings in page values. Screenshot, journal, and artifact methods are
not part of the v0.1 automation surface.

## Development

`pnpm pack` fails closed unless the checked-in generated runtime manifest is
canonically bound to the exact stable package version, both supported native
platforms, and the ten-file stable archive inventory. Release automation must
generate that manifest from the gated native artifacts before packing.

```sh
pnpm install
pnpm typecheck
pnpm test
pnpm build
```
