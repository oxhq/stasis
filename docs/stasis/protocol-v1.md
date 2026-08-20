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

`protocol.initialize` is the first request. `session.open` succeeds once.
`session.close` is terminal and its response is the last frame before EOF.
Unexpected process exit rejects all pending SDK operations.

The baseline implemented in `ports/stasis` currently supports tagged JavaScript
values and these methods:

```text
protocol.initialize
session.open
dom.evaluate
session.close
```

It intentionally advertises only those capabilities. The planned release-v1
surface is:

| Area | Methods |
| --- | --- |
| Lifecycle | `protocol.initialize`, `protocol.cancel`, `session.open`, `session.close` |
| Navigation | `document.navigate` |
| Runtime | `runtime.pending`, `runtime.settle`, `runtime.advance`, `runtime.advance_to_next` |
| DOM | `dom.evaluate`, `dom.query`, `dom.text`, `dom.html`, `dom.extract` |
| Actions | `action.fill`, `action.activate`, `action.click` |
| Artifacts | `artifact.screenshot`, `journal.export` |

The conditional advance token never crosses NDJSON. Each public advance asks
the engine to observe, mint, validate, and consume a fresh single-use token.

## Concurrency

Version 0.1 serializes ordinary engine commands and permits one active engine
request. A dedicated stdin/control lane must remain live while settlement waits
on external I/O. Cancellation is cooperative, never rolls back page effects,
and cannot preempt JavaScript already executing synchronously.

The planned v1 transport for large HTML, extraction results, screenshots, and
journals uses session-owned filesystem artifacts with media type, byte length,
and SHA-256 metadata instead of base64 frames.
