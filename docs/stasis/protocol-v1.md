# NDJSON protocol v1

The wire protocol is downstream-only. Servo internal command enums are a
same-build mechanism and are never serialized directly to clients.

## Framing

- UTF-8 NDJSON: input accepts LF or CRLF; output is one compact object followed
  by LF.
- `stdout` is protocol-only; logs use `stderr`.
- Input lines are bounded to 1 MiB before decoding.
- Requests, responses, and events have explicit envelope types.
- Request IDs are opaque non-empty strings.
- One stdout authority assigns a monotonic `wireSeq`.
- Exact nanoseconds, sequence numbers, generations, and work IDs are decimal
  strings on the wire; TypeScript derives ergonomic numeric milliseconds.

Example request:

```json
{"v":1,"type":"request","id":"cmd-18","sessionId":"s-1","method":"runtime.settle","params":{}}
```

Example response:

```json
{"v":1,"type":"response","wireSeq":"41","id":"cmd-18","sessionId":"s-1","result":{"outcome":"quiescent","virtualTimeNs":"1050000000","stateGeneration":"41"}}
```

Settlement blockers, `runtime_error`, and execution limits are successful domain results. Wire
errors mean that a command could not be validly performed. Mutating-command
errors include `stateEffect: none`, `partial`, or `indeterminate`; clients must
not automatically retry an indeterminate effect.

## Lifecycle

```text
spawned -> initialized -> session_open -> closing -> exited
```

`protocol.initialize` is the first request. For `v0.1.0-alpha.0`, it identifies
the native implementation as `stasis-shell` version `0.1.0-alpha.0`, binds the
exact Stasis source revision and the Servo/Pliego pins from
`STASIS_UPSTREAM.toml`, and names the source repository as
`https://github.com/oxhq/stasis.git`. The matching npm client is
`@oxhq/stasis@0.1.0-alpha.0`; the npm name is not the native implementation
name. `session.open` succeeds once.
`session.close` is terminal and its response is the last frame before EOF.
Unexpected process exit rejects all pending SDK operations.

The `v0.1.0-alpha.0` baseline implemented in `ports/stasis` advertises exactly
these methods:

```text
protocol.initialize
session.open
dom.evaluate
runtime.pending
runtime.settle
runtime.advance_to_next
action.activate
dom.text
protocol.cancel
session.close
```

The session clock mode is immutable:

- `session.open {"url": ...}` selects Real mode, blocks through ordinary load
  completion, and permits `dom.evaluate`.
- Controlled mode uses the flat open shape
  `{"url": ..., "clockMode":"controlled", "initialVirtualTimeNs":"0",
  "unixTimeOriginNs":"0"}`. Both time fields default to zero when omitted.
  It enables `runtime.pending`, `runtime.settle`,
  `runtime.advance_to_next`, `action.activate`, and `dom.text`.
- Controlled runtime, action, and native DOM methods reject Real sessions with
  `controlled_clock_required`. `dom.evaluate` rejects Controlled sessions
  because it is a blocking Real-mode helper.

The `v0.1.0-alpha.0` Controlled open bootstrap is limited to an audited,
fetch-backed top-level `http:` or `https:` navigation. The shell may submit
exactly one dedicated internal bootstrap command for the validated root
`SpawnPipeline` event before returning `controlled_ready`; ordinary runtime
drives cannot authorize that transition. This boundary establishes controlled
event-loop authority but does not promise an active DOM. `runtime.settle`
subsequently waits for resource input and drives the correlated navigation
response that activates the document; action or DOM methods issued earlier can
be definitively rejected as not yet actionable.
Synchronous `about:blank`, `srcdoc`, `javascript:` result documents, iframe
bootstrap, multiple candidate pipelines, and an already-active document are
not eligible for that bootstrap path; clients must not assume those inputs are
supported by Controlled `session.open` in this release.

`action.activate` and `dom.text` require a canonical decimal-string
`expectedGeneration`. The shell first performs a passive Observe, binds the
client data to that private target authority, and then submits one native
operation. Mutating activation is never retried when its outcome is
indeterminate. `dom.query`, `dom.extract`, fill, navigation, and artifact
methods are not advertised in this alpha baseline.

The conditional advance token never crosses NDJSON. Each public advance asks
the engine to observe, mint, validate, and consume a fresh single-use token.

## Concurrency

Protocol v1 in `v0.1.0-alpha.0` serializes ordinary engine commands and permits
one active engine request. A dedicated stdin/control lane must remain live while
settlement waits on external I/O. Cancellation is cooperative, never rolls back
page effects, and cannot preempt JavaScript already executing synchronously.

A future extension may transport large HTML, extraction results, screenshots,
and journals through session-owned filesystem artifacts with media type, byte
length, and SHA-256 metadata instead of base64 frames. Those methods are not
part of `v0.1.0-alpha.0`.
