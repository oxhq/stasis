# Settlement design contract 0.1 (future surface)

> **Status:** Design history and future direction. This is not the shipped
> contract for `v0.1.0-alpha.0`. The alpha's implemented public surface is the
> exact method and mode list in `protocol-v1.md`; in particular, it does not
> advertise fill, query/extract, screenshots, journals, or named support
> profiles.

## Historical target claim

For one fully active top-level document on one ScriptThread, under a declared
support profile, Stasis drives supported owned work without wall-clock polling
and returns either a stable settlement proof or a typed blocker or limit.

The PR-7 core proof profile includes:

- semantic `fill` and `activate`;
- tasks and individual microtasks;
- DOM one-shot timers;
- intercepted/local fetch and XHR;
- MutationObserver;
- one finite rendering opportunity and rAF callback;
- DOM inspection while controlled page turns remain paused.

The proposed contract-0.1 envelope then adds the `test` profile's layout-backed
click, hit testing, and basic screenshot. Those features consume the same
settlement model but are not prerequisites for proving the controlled
event-loop kernel.

Workers, every child browsing context (including same-loop iframes), auxiliary
WebViews, WebSockets/SSE, media, and uncontrolled time surfaces are typed
unsupported work under this proposed contract.

## Proposed controlled-clock gate

Stasis must not advertise `clock: controlled` until one document clock governs:

- DOM timer deadlines;
- `Date.now()` through SpiderMonkey's realm-discriminated host callback;
- `performance.now()` and its origin;
- rAF callback timestamps;
- rendering updates and the document timeline;
- execution-journal timestamps.

A supported surface must never silently fall back to host time. Workers, other
realms/event loops, and remaining unaudited host-time surfaces are either out of
scope or produce a typed `unsupported_work` result.

## Facts versus policy

Servo exposes a raw observation; Stasis applies policy:

```text
RawPendingSnapshot -> SettlementAssessment -> SettleResult
```

The raw snapshot binds at least:

- WebView, event-loop, pipeline, and navigation epoch;
- clock identity and exact virtual time;
- ordinary-input revision and whether bounded intake saturated;
- task/microtask checkpoints and producer watermarks;
- exact next finite deadline and single-use advance precondition;
- parser, network, rendering, and persistent/unsupported source inventory;
- task, microtask, rendering, mutation, and virtual-span counters;
- state generation and DOM epoch, whose meanings remain distinct.

## Outcomes

```text
quiescent
quiescent_with_persistent_work
blocked_on_external_io
blocked_on_open_ended_work
unsupported_work
virtual_time_limit_exceeded
task_limit_exceeded
microtask_limit_exceeded
rendering_limit_exceeded
mutation_limit_exceeded
runtime_error
```

`quiescent_with_persistent_work` means settled under the selected policy, not
that the page can never change again. The result lists every ignored persistent
source. Strict policy instead returns `blocked_on_open_ended_work`.

Eligible one-shot timers are advanced. Intervals are never auto-executed in an
unbounded loop: each is reported with stable identity, period, and source as
persistent work, or becomes a strict-policy blocker.

Quiescence is a proof at one linearization point. The result carries a state
generation, and inspection is consistent only while the controlled event loop
remains paused at that generation.

Read-only inspection methods (`query`, `text`, `html`, and `extract`) accept an
optional `expectedGeneration` and return `stale_generation` before reading if
the state changed. Arbitrary `evaluate` is explicitly mutating-capable and
invalidates a prior settlement proof unless it can be proven read-only.

Every settlement result contains distinct exact virtual time and measured wall
time, the effective policy, processed task/microtask/rendering/mutation counts,
the final raw pending snapshot, and structured persistent or unsupported work.

## Network rule for the design proof

The proposed deterministic gate uses intercepted or local fixture network only.
Live network is observable but does not receive a determinism claim. A later
live-network policy must explicitly choose whether virtual timers freeze while
I/O is pending or race against it; silently freezing can prevent an application
timeout from aborting its request.

While foreground external I/O is pending, the owner waits on the network/control
wake set, never on a polling delay. If no relevant response arrives before
`wallIoTimeout`, settlement returns the successful domain outcome
`blocked_on_external_io` with the final snapshot and wall/virtual time; it is
not rewritten as the wire error `wall_time_limit_exceeded`.

## Limit rule

A one-shot timer is not proof of finite computation: callbacks may recursively
schedule timeouts. rAF may reschedule itself and a microtask may requeue itself
inside one turn. Distinct execution limits must terminate all three cases with
typed outcomes.

Synchronous JavaScript that never returns cannot yet be interrupted by these
turn-level limits. The contract-0.1 design uses an outer process supervisor
until a SpiderMonkey interrupt/watchdog is implemented.

## Proposed profiles and reproducibility

`crawl` enables DOM, JavaScript, storage, network fixtures, semantic actions,
and layout only on demand. `test` adds continuous layout where required, hit
testing, pointer/focus input, rendering ticks, and screenshots. Both use the
same pending snapshot and settlement state machine.

Digest reproducibility is promised only for controlled inputs: fixture network,
declared clock, identical actions, and controlled randomness/content. Live
network, ambient system state, and unvirtualized randomness are excluded.
