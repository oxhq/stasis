# `@oxhq/stasis`

TypeScript client for the Stasis controlled-time Servo runtime. The
`v0.1.0-alpha.0` API owns one Stasis process, opens one app, and serializes every
command through a single FIFO protocol lane.

## Install

```sh
pnpm add @oxhq/stasis@alpha
```

For a byte-for-byte reproducible dependency selection, install the exact first
alpha with `pnpm add @oxhq/stasis@0.1.0-alpha.0`.

The published pair is `@oxhq/stasis@0.1.0-alpha.0` and native implementation
`stasis-shell` version `0.1.0-alpha.0`, sourced from
`https://github.com/oxhq/stasis.git`. Node.js 20 or newer is required. The
Stasis executable is distributed separately for this alpha; pass its path
explicitly. The SDK always starts it directly with `shell: false`.

## Use

```ts
import { launch } from "@oxhq/stasis";

const runtime = await launch({ executablePath: "/opt/stasis/bin/stasis" });
const app = await runtime.open("https://example.com", {
  clock: {
    mode: "controlled",
    initialVirtualTimeNs: 0n,
    unixTimeOriginNs: 0n,
  },
});

try {
  const before = await app.pending();
  await app.activate("#start", before.stateGeneration);

  const settled = await app.settle({
    persistentWork: "report",
    wallIoTimeoutNs: 10_000_000_000n,
    maxVirtualTimeNs: 30_000_000_000n,
    maxControlTurns: 100_000n,
  });

  const pending = await app.pending();
  console.log(settled.outcome, pending.virtualTimeNs); // bigint, never a lossy number
  console.log(await app.text("#status", pending.stateGeneration));

  await app.advanceToNext();
} finally {
  await app.close();
}
```

Omit `clock` to preserve the runtime's real-time mode. `evaluate()` is currently available only in
real-time sessions; controlled sessions use the controlled clock and settlement operations shown
above. This alpha does not virtualize live network content, ambient system state, or randomness.
`Runtime.close()` is an abrupt local termination for startup/error cleanup; `App.close()` is the
graceful protocol operation.

## Cancellation and failures

Every command accepts an `AbortSignal`. Aborting a command which is still queued removes only that
command. Once a command has been written, its state effect may be unknowable, so cancellation
terminates the child and fail-stops the app instead of retrying.

Protocol responses are strictly correlated by request ID, session ID, and monotonic `wireSeq`.
Malformed, duplicate-key, oversized, unmatched, or out-of-sequence output terminates the child.
Protocol-declared operation failures reject with `StasisProtocolError`; child/pipe failures reject
with `StasisProcessError` or `StasisTransportError`. Errors retain only a bounded tail of stderr
(64 KiB by default), while stdout is treated exclusively as NDJSON protocol data.

The MVP intentionally has no persistent DOM handles or locators. `activate()` and `text()` use a
native CSS selector plus a fresh state generation obtained from `pending()` or `settle()`; stale
generations fail instead of acting on a changed document. In a real-time session, `evaluate()` is a
single-command escape hatch and does not reinterpret decimal-looking strings in page values.
Fill, query/extract, screenshot, journal, and artifact methods are not part of
`v0.1.0-alpha.0`.

## Development

```sh
pnpm install
pnpm typecheck
pnpm test
pnpm build
```
