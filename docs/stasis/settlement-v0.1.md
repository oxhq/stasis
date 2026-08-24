# Settlement design contract 0.1

> **Status:** Stable v0.1 controlled-settlement and semantic-automation
> contract. `controlled-webapp-v1` is the one advertised support profile.
> Screenshots, causal journals, and the larger surfaces discussed below remain
> future direction unless the protocol capability list explicitly advertises them.

## Stable claim

For one fully active top-level document on one ScriptThread, under a declared
support profile, Stasis drives supported owned work without wall-clock polling
and returns either a stable settlement proof or a typed blocker or limit.

The `controlled-webapp-v1` profile includes:

- semantic `fill` and `activate`;
- tasks and individual microtasks;
- DOM one-shot timers;
- intercepted/local fetch and XHR;
- MutationObserver;
- one finite rendering opportunity and rAF callback;
- DOM inspection while controlled page turns remain paused.

Layout-backed click, hit testing, and screenshots are possible post-0.1
test-profile extensions. They are not part of the stable surface.

Workers, every child browsing context (including same-loop iframes), auxiliary
WebViews, WebSockets/SSE, media, and uncontrolled time surfaces are typed
unsupported work under this profile.

## Controlled-clock gate

Stasis must not advertise `clock: controlled` until one document clock governs:

- DOM timer deadlines;
- `Date.now()` through SpiderMonkey's realm-discriminated host callback;
- `performance.now()` and its origin;
- rAF callback timestamps;
- rendering updates and the document timeline;
- settlement and bounded-evidence timestamps.

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
control_turn_limit_exceeded
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

Every v0.1 automation method requires `expectedGeneration` and returns
`stale_generation` before acting or reading if the state changed. `query`,
`text`, and `extract` are read-only; the wire result records the observed
generation, while the TypeScript `text()` convenience API returns the string
directly. Standalone `html` is not public in v0.1; bounded `extract` fields may
read `innerHTML`. Arbitrary `evaluate` is explicitly mutating-capable and
invalidates a prior settlement proof unless it can be proven read-only.

Every settlement result contains distinct exact virtual time and measured wall
time, the effective policy, processed task/microtask/rendering/mutation counts,
the final raw pending snapshot, and structured persistent or unsupported work.

## Network rule

The deterministic release gate uses intercepted or local fixture network only.
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

Synchronous JavaScript that never returns cannot be interrupted by these
turn-level limits. The v0.1 SDK therefore applies a mandatory wall-clock
command supervisor and fail-stops the owned process with typed state-effect
evidence. A future SpiderMonkey interrupt may provide a finer native boundary.

## Supported profile and reproducibility

`controlled-webapp-v1` is frozen in
`profiles/controlled-webapp-v1.json`. It covers one top-level HTTP(S)
document/event loop, semantic fill and activation, bounded CSS-subset
inspection, tasks, microtasks, timers, finite rendering/rAF, MutationObserver,
and asynchronous fetch/XHR. Its immutable engine limits are 100,000 ordinary
tasks, 1,000,000 microtasks, 10,000 rendering opportunities, and 1,000,000
mutation records. Iframes, workers/worklets, auxiliary WebViews, external
subscriptions, media, and other named unaudited surfaces fail closed.

Digest reproducibility is promised only for controlled inputs: fixture network,
declared clock, identical actions, and controlled randomness/content. Live
network, ambient system state, and unvirtualized randomness are excluded.
