# Execution journal design contract 0.1 (future surface)

> **Status:** Design history and future direction. Execution journals are not
> advertised by `v0.1.0`; the stable release instead exposes bounded, redacted
> terminal settlement evidence in the TypeScript SDK. See `protocol-v1.md` for
> the exact shipped method surface.

The proposed journal is append-only execution evidence, not replay or time travel.

In this design, every entry contains:

```text
seq                 exact monotonic decimal string
atVirtualNs         exact document time decimal string
type                stable event kind
workId              exact string or null when no work owns the event
parentWorkId        exact string or null when causality is unproved
stateGeneration     engine-state generation
domEpoch            DOM mutation generation
attributes          bounded, event-specific metadata
```

Automation actions would be causal roots. Timers, producer tickets, network
lifecycle, task dispatch, individual microtasks, rendering/rAF, DOM mutations,
settlement outcomes, and typed failures would record lifecycle entries. Missing
parentage would be represented honestly as `null`; the journal would never
invent a causal edge to make a diagnostic prettier.

Inputs and network metadata would be redacted or hashed by default. The
proposed journal would not store full response bodies, full DOM snapshots, JS
heap state, or a screenshot per event. Size/event limits would terminate
recording with explicit metadata rather than silently truncating a digestible
journal.

`journalDigest` hashes a versioned canonical encoding of journal entries.
`domDigest` hashes a versioned canonical top-level DOM projection at the
settlement generation. Canonicalization version and effective settlement policy
are part of exported metadata. Equal digests are required only under the
controlled-input reproducibility profile described in `settlement-v0.1.md`.

The basic failure diagnostic is derived from the action-rooted journal slice:

```text
action activate(#save)
-> submit task
-> POST /api/save
-> HTTP 500
-> promise microtask
-> DOM mutation #error
```

It is useful evidence but not yet a complete causal explanation API.
