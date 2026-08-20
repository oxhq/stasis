# Controlled-runtime fixtures

These fixtures are the deterministic contract for Stasis controlled execution.
They deliberately expose state through DOM attributes and text so tests can
inspect the result without sleeps, polling, or fixture-authored readiness
hooks.

The assertions below describe observable facts, not an exact journal event
count. Implementations may add instrumentation events, but the listed events
must remain an ordered causal subsequence.

## Network interception contract

Tests must intercept these requests. No fixture requires public network access.

| Request | Deterministic response |
| --- | --- |
| `GET /__stasis__/debounce-search?q=Garay` | `200`, `Content-Type: application/json`, body `{"results":[{"expediente":"GARAY-001","juzgado":"Court One"},{"expediente":"GARAY-002","juzgado":"Court Two"}]}` |
| `POST /__stasis__/save` | `500`, `Content-Type: application/json`, body `{"error":"fixture failure"}` |
| `GET /__stasis__/redirect/start` | `302`, `Location: /__stasis__/redirect/target`, empty body |
| `GET /__stasis__/redirect/target` | `200`, `Content-Type: text/html; charset=utf-8`, body from [`redirect/target.html`](redirect/target.html) |

Intercepted replies should be delivered as events, without wall-clock delays.
Tests of `BlockedOnExternalIo` belong in a separate transport fixture and must
not be simulated here with a timer.

## Fixture contracts

### `timer-10s`

Trigger: activate `#start`.

- Immediately after the action turn, `pending()` reports one future one-shot
  timer with a deadline 10,000 ms after its scheduling instant.
- `settle()` advances to that deadline without proportional wall time and
  returns `quiescent`.
- `#result` is `timer complete`; `data-date-elapsed` and
  `data-performance-elapsed` are both `10000`.
- Required journal subsequence:
  `action -> timer.scheduled -> timer.started -> dom.mutated -> timer.completed -> runtime.quiescent`.

### `debounce-fetch-raf`

Trigger: fill `#query` with `Garay`, then activate `#search`.

- After the action turn, `pending()` reports one finite 800 ms timer and no
  active request.
- Settlement advances the debounce timer, consumes the intercepted fetch and
  its promise continuations, performs the required animation frame/rendering
  update, and returns `quiescent`.
- Two `.result` rows exist with the exact `.case` and `.court` values in the
  interception contract; `#status` is `2 results`.
- Final pending work has no ready tasks, microtasks, foreground requests, due
  timers, rAF callbacks, or required rendering update.
- Required journal subsequence:
  `action -> timer.scheduled -> timer.started -> network.started -> network.finished -> microtask -> raf.scheduled -> raf.started -> dom.mutated -> runtime.quiescent`.

### `nested-microtasks`

Trigger: activate `#start`.

- Settlement performs all nested promise and `queueMicrotask` continuations in
  checkpoint order and returns `quiescent`.
- `#order` is exactly
  `outer,inner-promise,inner-microtask,tail-promise`.
- Final `pending().microtasks` is zero.
- The journal preserves four causally nested microtask records before the DOM
  mutation and quiescence event.

### `timer-microtask-order`

Trigger: activate `#start`.

- The action registers two one-shot timers at the same 1,000 ms deadline.
- Controlled settlement runs the first timer as one event-loop turn, performs
  its promise microtask checkpoint, and only then runs the second timer.
- `#order` is exactly `timer-1,microtask,timer-2`; `timer-1,timer-2,microtask`
  is a failure that reveals timer callbacks were incorrectly batched into one
  turn.
- Required journal subsequence:
  `timer-1.started -> timer-1.completed -> microtask -> timer-2.started -> runtime.quiescent`.

### `interval-heartbeat`

The interval is registered while parsing; no action is required.

- `pending()` reports one repeating timer with period 5,000 ms.
- Under the MVP `finite` timer policy, `settle()` returns
  `quiescent_with_persistent_work`, listing that interval.
- `#heartbeat-count` remains `0`; settlement must not execute interval cycles
  merely to discover that the work is persistent.
- Required journal subsequence:
  `timer.scheduled(repeating) -> runtime.quiescent(persistent)` with no
  `timer.started` in between.

### `mutation-observer`

Trigger: activate `#start`.

- The action queues a promise microtask that mutates `#target`; the observer is
  delivered before quiescence and disconnects itself.
- `#target[data-state]` is `changed`, `#observer-count` is `1`, and
  `#status` is `observer delivered`.
- Final pending work contains no microtasks or observer notification.
- Required journal subsequence:
  `action -> microtask -> dom.mutated(#target) -> mutation-observer -> dom.mutated(#status) -> runtime.quiescent`.

### `http-500`

Trigger: fill `#email`, then activate `#save`.

- While the intercepted response is unresolved, pending work identifies one
  foreground fetch. After delivery, final foreground network count is zero.
- HTTP 500 is an application-visible response, not a runtime failure:
  `settle()` returns `quiescent`.
- `#error` is `Unable to save`, `#status` is empty, and the document's
  `data-response-status` is `500`.
- Required journal subsequence:
  `action -> network.started -> network.finished(status=500) -> microtask -> dom.mutated(#error) -> runtime.quiescent`.

### `redirect`

Trigger: activate `#go`.

- During the redirect chain, pending work reports foreground navigation and/or
  an active parser. Settlement must not report quiescence between the 302 and
  the target document completing its initial parse.
- The final URL ends in `/__stasis__/redirect/target`,
  `#redirect-complete` is `Redirect complete`, and the root element has
  `data-redirect-target="true"`.
- Final pending work has no foreground navigation and an inactive parser.
- Required journal subsequence:
  `action -> navigation.started -> navigation.redirected -> navigation.started(target) -> parser.completed -> runtime.quiescent`.

## Global invariants

- Fixture code contains no `setTimeout` except the finite timers under test,
  no retry loop, no polling, and no authored `ready`/`settled` promise. The
  10,000 ms timer, 800 ms debounce, and same-deadline 1,000 ms timer pair are
  application work that the runtime must discover and execute; none is a
  client-side wait. The 5,000 ms interval is likewise intentional persistent
  application work.
- Assertions inspect only commands, snapshots, journal metadata, URL, and DOM.
- A controlled run uses one top-level document, no iframe, worker, service
  worker, WebSocket, SSE, animation loop, or live network resource.
- Exact sequence numbers and work IDs are intentionally unspecified; their
  monotonicity and parent-child relationships are part of the runtime contract.
