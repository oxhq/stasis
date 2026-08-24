# `@oxhq/stasis`

TypeScript client for the Stasis controlled-time Servo runtime. The
`v0.2` API owns controlled web sessions and serializes every command through a
single FIFO protocol lane.

## Install

```sh
pnpm add @oxhq/stasis
```

Select the stable release exactly with `pnpm add @oxhq/stasis@0.2.0`.

The release pairs `@oxhq/stasis@0.2.0` with native implementation
`stasis-shell` version `0.2.0`, sourced from
`https://github.com/oxhq/stasis.git`. Node.js 20 or newer is required. The
SDK has no install lifecycle scripts or runtime dependencies. On the release's
macOS Apple Silicon and Linux x86-64 targets, `launch()` downloads the exact release
archive on first use, verifies its declared size, archive SHA-256, complete
file inventory, and executable SHA-256, then installs it atomically in a
digest-keyed per-user cache. Unsupported hosts fail closed and can use an
explicit compatible executable. The SDK always starts it directly with
`shell: false`.

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

`openSession()` is always controlled and selects `controlled-web-session-v1`.
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
The legacy v1 API keeps its numeric state-generation contract. Screenshot,
replay, and time-travel methods are not part of the v0.2 surface.

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
