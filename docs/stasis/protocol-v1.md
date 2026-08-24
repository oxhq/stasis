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

`protocol.initialize` is the first request. For `v0.1.0`, it identifies
the native implementation as `stasis-shell` version `0.1.0`, binds the
exact Stasis source revision and the Servo/Pliego pins from
`STASIS_UPSTREAM.toml`, and names the source repository as
`https://github.com/oxhq/stasis.git`. The matching npm client is
`@oxhq/stasis@0.1.0`; the npm name is not the native implementation
name. `session.open` succeeds once.
`session.close` is terminal and its response is the last frame before EOF.
Unexpected process exit rejects all pending SDK operations.

The `v0.1.0` runtime implemented in `ports/stasis` advertises exactly
these methods:

```text
protocol.initialize
session.open
dom.evaluate
runtime.pending
runtime.settle
runtime.advance_to_next
action.activate
action.fill
dom.query
dom.text
dom.extract
protocol.cancel
session.close
```

The session clock mode is immutable:

- `session.open {"url": ...}` selects Real mode, blocks through ordinary load
  completion, and permits `dom.evaluate`.
- Controlled mode uses the flat open shape
  `{"url": ..., "clockMode":"controlled", "initialVirtualTimeNs":"0",
  "unixTimeOriginNs":"0", "profile":"controlled-webapp-v1"}`. Both time
  fields default to zero when omitted, but the exact named profile is mandatory.
  It enables `runtime.pending`, `runtime.settle`,
  `runtime.advance_to_next`, `action.activate`, `action.fill`, `dom.query`,
  `dom.text`, and `dom.extract`.
- Every successful open result echoes `profile`; it is
  `"controlled-webapp-v1"` for Controlled mode and `null` for Real mode.
- Controlled runtime, action, and native DOM methods reject Real sessions with
  `controlled_clock_required`. `dom.evaluate` rejects Controlled sessions
  because it is a blocking Real-mode helper.

The `v0.1.0` Controlled open bootstrap is limited to an audited,
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
Application-initiated top-level navigation after the initial document is also
outside `controlled-webapp-v1`, including same-origin `http:` or `https:`
navigation that could reuse the same script event loop. The engine rejects the
replacement pipeline before it can become active, latches
`cross_event_loop_navigation`, and the next settlement reports typed
`unsupported_work` with bounded evidence.

## Generation-aware automation in v0.1

The v0.1 automation extension adds `action.fill`, `dom.query`, and
`dom.extract` to the baseline `action.activate` and `dom.text` surface. It does
not introduce persistent DOM handles. Every automation request carries a
mandatory canonical decimal-string `expectedGeneration`; the shell passively
observes the document, binds the request to that private target authority, and
rejects `stale_generation` before touching a different state.

The strict request and result bodies are:

| Method | Parameters | Result |
| --- | --- | --- |
| `action.activate` | `selector`, `expectedGeneration` | `stateGeneration` |
| `action.fill` | `selector`, `value`, `expectedGeneration` | `stateGeneration` |
| `dom.query` | `selector`, `expectedGeneration` | `count`, `stateGeneration` |
| `dom.text` | `selector`, `expectedGeneration` | `value`, `stateGeneration` |
| `dom.extract` | `rootSelector`, ordered `fields`, `expectedGeneration` | ordered `rows`, `stateGeneration` |

All exact integers in that table are canonical decimal strings on the wire.
`activate` and `fill` return the authoritative post-operation generation so
clients can chain mutations without an extra observation. Read operations
return their observed generation on the wire. The TypeScript `text()` helper
intentionally returns the string directly; `pending()`, `settle()`, mutation
results, and structured inspection results provide generations for further
chaining.

`dom.query` returns only a bounded count. `dom.text` reads raw `textContent` and
requires exactly one match. Each `dom.extract` field is
`{"name":...,"selector":...,"read":"text"|"html"}`. Roots remain in
document order, fields remain in request order, and every field must match
exactly one descendant of its root. `text` means raw `textContent`; `html`
means bounded `innerHTML`. Attribute extraction and standalone `dom.html` are
not in this surface.

Selectors are the CSS subset whose decision is local to one candidate:
type/universal, ID, class, and attribute components, including comma-separated
selector lists. Named namespace prefixes, combinators, and pseudo-classes are
outside the profile; unsupported syntax is rejected as `unsupported_selector`
or `invalid_selector` before traversal.

`action.fill` replaces the value of one mutable `textarea` or one mutable
`input` of type `text`, `search`, `url`, `tel`, `email`, or `password`. It then
dispatches exactly one bubbling, composed, non-cancelable `input` event with
the complete replacement in `data` and `inputType` equal to
`insertReplacementText`. It does not synthesize focus, keyboard, or `change`
events. Other controls return `unsupported_fill_element`; read-only or
otherwise immutable controls return `immutable_fill_element`.

The public automation limits are intentionally below the same-build engine
ceilings:

| Resource | v0.1 limit |
| --- | ---: |
| selector UTF-8 bytes | 4 KiB |
| fill value UTF-8 bytes | 128 KiB |
| extraction field-name UTF-8 bytes | 256 B |
| extraction fields | 16 |
| selector/root matches | 128 |
| DOM nodes visited | 1,000,000 |
| logical text/HTML extraction output | 128 KiB |

These bounds keep even adversarial JSON escaping and the maximum extraction
row/field structure within the product's 1 MiB NDJSON frame budget. An
over-budget operation is rejected before an oversized public frame is emitted.

Definitive automation rejections use stable operation-error codes:

```text
invalid_automation_request
automation_target_changed
execution_terminated
stale_generation
invalid_selector
unsupported_selector
automation_match_limit_exceeded
automation_dom_traversal_limit_exceeded
automation_selector_evaluation_limit_exceeded
element_not_found
selector_ambiguous
extraction_field_not_found
extraction_field_ambiguous
unsupported_fill_element
immutable_fill_element
unsupported_activation_element
disabled_activation_element
unsupported_dom_serialization
document_automation_failed
automation_output_limit_exceeded
```

Definitive validation and element failures have `stateEffect: none`. If a
mutating operation loses its authoritative result after it may have run, the
shell reports `stateEffect: indeterminate` and fail-stops the session. Clients
must not retry that mutation automatically.

The conditional advance token never crosses NDJSON. Each public advance asks
the engine to observe, mint, validate, and consume a fresh single-use token.

## Concurrency

Protocol v1 in `v0.1.0` serializes ordinary engine commands and permits
one active engine request. A dedicated stdin/control lane must remain live while
settlement waits on external I/O. Cancellation is cooperative and never rolls
back page effects. The TypeScript SDK additionally gives every written native
command a mandatory wall-clock deadline; expiry fail-stops the child with typed
`stateEffect` evidence, so synchronous JavaScript cannot hang the consumer
indefinitely.

A future extension may transport large HTML, extraction results, screenshots,
and journals through session-owned filesystem artifacts with media type, byte
length, and SHA-256 metadata instead of base64 frames. Those methods are not
part of `v0.1.0`.
