# Pliego donor map

Pinned donor: `556c774242b272b11bc60999449c5debff1ad20f`.

Pinned clean target: `0d579bd5aab6df3764fad805427254751632a6e4`.

The shared Servo ancestor is `313b6d5ecc113b08010ce434140db3ca5abcc71c`.
Current Servo has moved ScriptThread and task modules and changed font,
microtask, worker-rAF, and Constellation state ownership, so this table is a
semantic port order rather than a cherry-pick recipe.

| Donor commits | Port | Adaptation rule |
| --- | --- | --- |
| `f06508f382de`, `1813db637bb7`, `6ef1ee109766` | `DocumentClock`, `u128` monotonic time, signed Unix time, controlled timer scheduler, stable ordering and exact deadline snapshots | Preserve real mode and current `Select::select_deadline` behavior; never restore the older timer wait-channel architecture |
| `ffa610484dae` | Window Date and Performance clock routing | Reuse SpiderMonkey's host callback and realm discrimination; no init-script Date replacement |
| `bb3b8b947314`, `66d68bf99071` | Rendering, rAF, document timeline, and performance provenance | Fail typed on unsupported clock surfaces rather than leaking host time |
| `6501a024c36c` | RAII producer fence, enqueue/completion watermarks, two-checkpoint stability | Extend coverage on current source paths; do not treat it as a causal journal |
| `501d4809abfe` | Clock configuration before initial navigation | Expose only through WebView construction/open options in 0.1 |
| `f9fcd692ae5c`, `4f9bdd944927` | Observe, DriveOneTurn, and guarded single-use AdvanceTo | Keep the token internal to product commands |
| `715661aca7b8`, `9984d57869ea` | Typed shutdown and definitive versus indeterminate transport results | Preserve `stateEffect: none/partial/indeterminate` at the wire boundary |
| `4db43e7a5b95` | Task, individual microtask, rendering, mutation, and virtual-span limits | Keep distinct counters; a generic `maxEvents` is only an SDK convenience |
| `84b866014f13`, `580abd76eb51` | Lost-wake-safe generation/condition-variable transport | Add a separate protocol-input generation |
| `6ba6be8cdaa2`, `877cac369b39` | Finite, open-ended, and unsupported source taxonomy | Extract from generation capture; reject Paint/capture preconditions |
| `9f5c0270fcbe` and `ports/pliego/src/controlled_settlement.rs` | Wake-driven observe/drive/advance coordinator | Replace capture readiness with raw snapshots and Stasis policy; report intervals as persistent work rather than a PDF capture failure |

Useful lifecycle shape only:

- `ports/pliego/src/document_session.rs`: software rendering context,
  ServoBuilder, WebViewBuilder, owner-thread drop order.
- `ports/pliego/src/event_loop_waker.rs`: wake generation and condition
  variable.
- `ports/pliego/src/api2.rs`: bounded strict JSON concepts and executable
  identity. Its one-shot EOF framing is not reusable as NDJSON.

Explicit exclusions:

- generation-bound Paint capture and cached layout serialization;
- PDF/scene/pagination and retained Canvas;
- Pliego resource allowlists and offline document policy;
- `window.pliego.defer/ready/fail`;
- publication, recovery, supervisor worker schema, and PHP/Laravel clients.

New Stasis work begins at the general pending projection, persistent-work
policy, external-I/O classification, journal causal metadata, semantic DOM
actions, NDJSON session protocol, and TypeScript SDK.
