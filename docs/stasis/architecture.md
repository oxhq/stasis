# Architecture decision: clean Servo base, semantic Pliego transplant

## Decision

Stasis is based directly on the Servo revision pinned in
`STASIS_UPSTREAM.toml`. The Pliego fork is a donor of audited generic engine
mechanisms, not the base branch and not a subtree to copy wholesale.

The two source lines have diverged far enough that blind cherry-picks would mix
product code with engine code and silently discard newer Servo work. Every
donor slice must therefore be replayed semantically against current Servo and
retain its focused tests and provenance.

## Ownership boundary

Generic Servo-facing mechanisms:

```text
clock configuration before navigation
document clock and exact time types
timer deadlines and guarded advancement
Date / Performance / rAF / document timeline clock routing
task, microtask, rendering, and mutation execution limits
cross-thread producer fence and stable checkpoints
raw pending-source observations
typed internal control commands
```

Stasis-owned policy and product surfaces:

```text
settlement scopes and persistent-work policy
PendingWorkSnapshot wire projection
execution journal and best-effort causal IDs
network fixture policy and external-I/O outcomes
semantic actions and DOM extraction
NDJSON shell, artifacts, cancellation, and process lifecycle
TypeScript SDK and binary distribution
```

Rejected Pliego dependencies:

```text
paged layout and PDF/scene generation
Paint presentation/capture tickets
retained Canvas capture and freeze
authored window.pliego readiness
publication and recovery transactions
render supervision contracts tied to one PDF render
PHP and Laravel SDKs
```

The polling/readiness branches in Pliego's `DocumentSession` are also rejected.
Only its controlled driver, wake transport, builder sequence, and drop-order
lessons are donors.

## Runtime configuration

The clock is immutable for a WebView and is selected before the first
navigation. The external convenience modes map to orthogonal engine settings:

| External mode | Clock | Instrumentation |
| --- | --- | --- |
| `real` | real | off or minimal |
| `observe` | real | counters/journal |
| `controlled` | controlled | counters/journal |

There is no 0.1 command that changes an existing WebView from real to
controlled time. Doing so after realms or deadlines exist would split the page
timeline.

## Correctness authority

Settlement is decided from authoritative scheduler, queue, producer-fence, and
source-inventory facts. The future `WorkRegistry` adds journal identity and
best-effort parent links, but it is not the sole correctness authority and may
record `parentWorkId: null` when the engine cannot prove causality.

Every timer advance is conditional. The engine first observes an exact target,
input revision, producer snapshot, and timer deadline, issues a single-use
token, then consumes that token while advancing. A new input, producer event,
navigation, cancellation, or timer change makes the token stale without moving
the clock.

## Alpha owner loop

The shell's main thread exclusively owns Servo and the WebView. A dedicated
stdin reader and Servo's `EventLoopWaker` increment separate wake generations
on one condition variable. The owner thread drives Servo and waits for a
generation change; it never uses `sleep()` to discover progress.

The alpha uses this owner loop as the authority for controlled bootstrap,
pending observations, bounded event-loop turns, conditional virtual-time
advancement, settlement, and generation-bound actions and inspection. Those
claims apply only to the exact shipped method and support boundary in
`protocol-v1.md`; the loop by itself is not evidence for future profiles,
journals, artifacts, or broader browsing-context support.

## Upstream boundary

Candidate Servo contributions are the clock types and injection points,
scheduler/source observations, producer lifecycle hooks, execution budgets,
and typed internal control messages. Stasis keeps settlement policy, journal
schema, network-fixture rules, protocol, CLI, profiles, actions, and SDK
downstream.
