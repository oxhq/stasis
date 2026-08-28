# Stasis support profiles

Support profiles name the exact browser subset for which Stasis can make controlled-settlement
claims. A profile is a versioned product contract, not a request to emulate every browser feature.

`controlled-webapp-v1.json` is the profile shipped by Stasis 0.1. It is frozen byte-for-byte and
remains the default selected by the legacy `Runtime.open()` API. Every later profile must prove
that this profile's request shapes, result shapes, close behavior, and typed unsupported outcomes
have not changed.

`controlled-web-session-v1.json` is the stable contract shipped by Stasis 0.2. It is an explicitly
selected, additive profile for one terminal session per owned process, checked
top-level and same-document navigation, document-scoped state tokens, separately versioned
session state, practical bounded selectors and forms, declarative network fixtures, bounded
request/evidence streams, a fresh-process pool, and a reference crawler. Its
`stable_contract` status and frozen release digest prohibit silent changes; incompatible changes
require a newly named profile.

`controlled-web-session-v2.json` is the complete versioned contract for Stasis 0.3.0. It keeps
v1 as the `openSession()` default and adds bounded, same-global, untransferred `MessageChannel`
delivery; one bounded direct `HTMLImageElement.src` cache/decode path for canonical data SVG and
initial HTTP(S) selection; one
distinct inline `<svg>` path whose internally serialized data-SVG URL is the exact cached URL for
that SVG owner in the exact public non-auxiliary controlled top-level document; and suppression of page-driven, single-line
text InputMethod
presentation when no virtual keyboard is requested there. It also adds an exact v2 cookie-state
artifact with controlled in-memory expiry and bounded schemeful SameSite request selection. Image
admission requires no `srcset`/`picture`/environment-change selection, an initially selected
serialized URL of at most 65,536 bytes, the same ScriptThread/ImageCache, and capacity within 512
total retained controlled image ownership records. An admitted synchronous cache hit is owned in
the current Script turn and queues its existing ordinary DOM callback without inventing an
asynchronous `Image` producer lease. Finite asynchronous cache/decode completion is producer-fenced,
and one document-clock sample is shared by the engine-generated `load`/`error`/`loadend` completion
set. Cache-owned callback retirement is an owned cancellation
that completes its Image stream normally, as does a dequeued response whose closed-pipeline
tombstone proves that navigation retired its Window. A normal live handler rejection preserves the
pending owner or key, completes the scoped message guard, and settles as typed
`unsupported_rendering`; admission, enqueue, producer callback panic, actual handler unwind,
pre-handler authority, target-invariant or clock failure, and guarded transport loss explicitly
abandon the stream and stay
terminal. Completion and abandonment are accepted only for the exact live fence/sequence and
registered Image producer class. The inline path additionally requires an internal request,
the same canonical MIME/URL bound, an exact cache-ID owner join, and fenced decode/raster
completion. An identical current inline root may join an exact retained producer whether layout
reports `PendingResponse` or a stale/reentrant `Unrequested`, when its exact cache-key URL and
`PendingImageId` match an existing same-ID fenced layout record and every live callback retains
the exact same producer URL key. That callback-owned key may survive an earlier DOM owner's unbind,
but terminal callback removal revokes it; the old DOM identity is neither required nor trusted.
The callback set must be nonempty and uniformly fenced, with no baseline retained work. A new
current owner is retained once and an already-retained owner is idempotent. The join reuses the
existing listener and producer; it adds no listener, producer, or fetch. Controlled callbacks,
layout owners, DOM identities, raster keys, and raster owners all consume the shared 512-record
capacity through their exact lifetimes. A missing anchor/key, stale candidate, mismatch, mixed
provenance, or capacity terminal fails closed and cannot promote baseline work. The inline path creates no DOM
load event. Excluded URLs and image consumers receive no new owned
authority; retained baseline work and observed host timestamps keep their existing typed rejection
paths. Data URLs require the canonical parser and exact `image/svg+xml`; HTTP(S) is admitted by the
initially selected scheme before response format is known, with resource I/O separately owned and
finite decode failure retained as an owned error completion. Post-metadata
`multipart/x-mixed-replace` remains typed `unsupported_rendering` / `image_load` after separately
owned finite Resource I/O drains, without baseline callback fallback; an endless response remains
blocked on that external I/O. Public document replacement while HTTP image resource I/O is active
retains fatal `blocked_on_external_io`; v2 does not claim cross-document replacement through that
state.

Programmatic focus, including React `autoFocus`, is covered only in that exact public target; literal
HTML `autofocus` processing is not. Engine-generated focus transitions there expose document-clock `Event.timeStamp`. In addition, one
document Performance timestamp sampled before each public mutating automation action is shared by
every browser-created event constructed synchronously during that action. The fill, activation,
reset, check/uncheck, select, invalid, submit, and formdata corpus is representative, not an event-name
allowlist. Separately, one document Performance sample is taken before each nonempty owned CSS
animation pending-event queue is drained and is installed only on internal `AnimationEvent` or
`TransitionEvent` records owned by the exact public non-auxiliary controlled top-level
WebView/document. The `TransitionEvent` adapter is conditional on an existing owned transition record reaching that
queue; general transition settlement is not claimed. Auxiliary top-level WebViews remain
host-stamped. Script-created event
constructors and events
outside these bounded seams remain unsupported. DOM focus semantics remain; no native owner,
request, callback, or hidden
external work is created. MessageChannel construction requires the exact active public top-level
target and an incumbent that matches the owner global, pipeline, and WebView before either port is
published. `postMessage()` repeats the exact incumbent/owner check before structured cloning, so a
borrowed, missing, replaced, discarded, or auxiliary caller is rejected before serialization or
transfer detachment. Idle reciprocal local ports are inert;
queued deliveries consume the ordinary-task budget. Other InputMethod shapes and embedder controls
remain unsupported. Transferred, cross-global, cross-event-loop, worker, BroadcastChannel, and
externally routed ports remain typed `external_subscription` or worker boundaries. Blob/file,
non-SVG data, CSS/background/generated-content, favicon, video-poster, animated,
general or nested/external SVG resources, iframe/worker/worklet, and transferred image paths are
not promoted by the image slice. Web Animations API semantics, event ordering/cardinality,
`elapsedTime`, and CSS animation limits are unchanged. A nonempty document-owned pending CSS
animation-event queue is finite rendering demand and retains one later owned rendering opportunity
until dispatch drains it; an empty queue leaves no opportunity. A live scheduled batch uses guarded
`AdvanceTo` at the exact retained scheduler head, and only an unscheduled batch is `Drive`-ready.
This prevents a checkpoint spin without adding a task source or limit. Baseline and v1 profile
contracts are unchanged.

Every returned v2 `runtime.settle` result also carries a required owner-attested `url`. The shell
projects it from the exact final active top-level navigation authority after the passive
N1/document-pending-D/passive-N2 bracket succeeds, and that same authority binds the returned
`stateToken`. The field is present for every returned outcome and does not itself claim quiescence.
It does not turn the open-time `Session.url` into mutable state, add a polling operation, or change
action, navigation, or frozen v1 result shapes. Settlement evidence continues to omit URLs and
other sensitive application values.

V2 also resolves the scheduler-head case in which an owned persistent JavaScript interval sits
before eligible finite work. Only `persistentWork: "report"` may advance that exact eligible head, using the
same single-use complete-snapshot token as every finite advance. Each interval callback remains an
ordinary task under all existing execution and virtual-time limits. When finite work is gone, two
stable checkpoints return `quiescent_with_persistent_work` without advancing the interval again.
Finite timer and animated-image deadlines must remain strictly later. One finite rendering
opportunity may share the timestamp only as a distinct exact same-scheduler owner whose `TimerId`
sequence follows the interval head; same-entry, lower-or-equal-order, foreign-scheduler,
bare/unowned, equal finite-timer, and equal animated-image collisions remain blocked.
`strict`, `controlled-webapp-v1`, and `controlled-web-session-v1` retain the former
`blocked_on_open_ended_work` behavior.

V2 persistent cookies are memory-owned by the controlled session and use its Unix-nanosecond
clock with origin zero. `Max-Age` precedes `Expires`, lifetime is clamped to 400 days, expiry at or
before controlled now deletes, and lazy purge runs before observation, request selection, and
export. SameSite uses captured schemeful site-for-cookies, the current redirect-hop method, and
the top-level-navigation bit: Strict is same-site only; Lax and unspecified also admit cross-site
top-level safe methods; Secure None cookies may cross site. Unknown or opaque context remains typed
unsupported. After successful controlled parsing, cross-site subresource
responses store only valid Secure SameSite=None cookies; otherwise valid Strict, Lax, and
unspecified response cookies are ignored. Parse, normalization, and time-range failures retain
their existing typed outcomes. Top-level navigation responses admit all otherwise valid
unpartitioned cookies. A post-open request at controlled Unix time above u64 fails nonfatally as
`unsupported_cookie_time_range` rather than wrapping; initial controlled open hardens the same code
to fatal fail-stop. Either post-open typed rejection may retain bounded `request_started` and
`request_failed` evidence, but it occurs before `route_decided` or route selection, fixture or live
external I/O, and Cookie header construction. Partitioned cookies and CookieStore read/getAll/delete
remain unsupported. The schema remains 1,
but V2 exports and imports only a literal
`controlled-web-session-v2` state artifact; it never silently migrates v1 state or persists a host
cookie jar to disk. See
`docs/stasis/session-v0.3-candidate.md` for the concise boundary.

Each JSON file is canonical release source: automation, inspection, execution, network, state,
evidence, and unsupported-surface claims must be changed there before a profile can be advertised
by the native runtime. Incompatible changes require a new profile identifier; a stable release must
not silently broaden or narrow an existing profile. A new profile must be complete rather than
relying on implicit inheritance from an older profile.

For controlled sessions, the protocol request and response both carry the selected profile. Work
outside that profile must either be rejected before it starts or terminate settlement with a typed
unsupported/open-ended result. It must never be treated as quiescent merely because Stasis cannot
observe it.

All profiles deliberately exclude live-network reproducibility. Release proofs use local or
intercepted fixtures whose inputs are controlled by the test.

Document state and session state are separate authorities in the session profile. A document
`stateToken` binds the current top-level document target and complete runtime generation. A
`sessionStateToken` binds cookies and Web Storage, which survive document replacement. Neither can
substitute for the other, and exported session state is sensitive data that must not enter bounded
diagnostic evidence.

All document-targeting operations require exact current authority. `runtime.settle` has one
explicitly frozen continuation exception: the sole latest-issued token may drain monotonic
same-document work admitted after that token was returned, but only across a pump-suppressed
N1/document/N2 bracket with identical full navigation authority. It never authorizes a different
document. Valid nonterminal bracket drift transactionally latches that exact binding stale, while
typed navigation terminals keep their typed result and never start settlement. The next
successfully issued token revokes every earlier token.
