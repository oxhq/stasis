# Execution journal contract 0.1

The journal is append-only execution evidence, not replay or time travel.

Every entry contains:

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

Automation actions are causal roots. Timers, producer tickets, network
lifecycle, task dispatch, individual microtasks, rendering/rAF, DOM mutations,
settlement outcomes, and typed failures record lifecycle entries. Missing
parentage is represented honestly as `null`; the journal never invents a
causal edge to make a diagnostic prettier.

Inputs and network metadata are redacted or hashed by default. The 0.1 journal
does not store full response bodies, full DOM snapshots, JS heap state, or a
screenshot per event. Size/event limits terminate recording with explicit
metadata rather than silently truncating a digestible journal.

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
