# Stasis support profiles

Support profiles name the exact browser subset for which Stasis can make controlled-settlement
claims. A profile is a versioned product contract, not a request to emulate every browser feature.

`controlled-webapp-v1.json` is the profile shipped by Stasis 0.1. It is frozen byte-for-byte and
remains the default selected by the legacy `Runtime.open()` API. Every later candidate must prove
that this profile's request shapes, result shapes, close behavior, and typed unsupported outcomes
have not changed.

`controlled-web-session-v1.json` is the stable contract shipped by Stasis 0.2. It is an explicitly
selected, additive profile for one terminal session per owned process, checked
top-level and same-document navigation, document-scoped state tokens, separately versioned
session state, practical bounded selectors and forms, declarative network fixtures, bounded
request/evidence streams, a fresh-process pool, and a reference crawler. Its
`stable_contract` status and frozen release digest prohibit silent changes; incompatible changes
require a newly named profile.

`controlled-web-session-v2.json` is a complete candidate contract targeting Stasis 0.3.0. It keeps
v1 as the `openSession()` default and adds bounded, same-global, untransferred `MessageChannel`
delivery; one narrow direct `HTMLImageElement.src` `data:image/svg+xml` cache/decode path; one
distinct inline `<svg>` path whose internally serialized data-SVG URL is the exact cached URL for
that SVG owner in the exact public non-auxiliary controlled top-level document; and suppression of page-driven, single-line
text InputMethod
presentation when no virtual keyboard is requested there. It also adds an exact v2 cookie-state
artifact with controlled in-memory expiry and bounded schemeful SameSite request selection. Image admission requires the canonical
data-URL parser, exact `image/svg+xml`, no `srcset`/`picture`/environment-change selection, a
serialized URL of at most 65,536 bytes, the same ScriptThread/ImageCache, and capacity within 512
total retained controlled image ownership records. Cache hits and finite asynchronous completion
remain producer-fenced, and one document-clock sample is shared by the engine-generated
`load`/`error`/`loadend` completion set. Cache-owned callback retirement is an owned cancellation
that completes its Image stream normally, as does a dequeued response whose closed-pipeline
tombstone proves that navigation retired its Window. A normal live handler rejection preserves the
pending owner or key, completes the scoped message guard, and settles as typed
`unsupported_rendering`; admission, enqueue, producer callback panic, actual handler unwind,
pre-handler authority, target-invariant or clock failure, and guarded transport loss explicitly
abandon the stream and stay
terminal. Completion and abandonment are accepted only for the exact live fence/sequence and
registered Image producer class. The inline path additionally requires an internal request,
the same canonical MIME/URL bound, an exact cache-ID owner join, and fenced decode/raster
completion; it creates no DOM load event. Excluded URLs and image consumers receive no new owned
authority; retained baseline work and observed host timestamps keep their existing typed rejection
paths.

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
externally routed ports remain typed `external_subscription` or worker boundaries. HTTP/HTTPS,
blob/file, non-SVG data, CSS/background/generated-content, favicon, video-poster, animated,
general or nested/external SVG resources, iframe/worker/worklet, and transferred image paths are
not promoted by the image slice. Web Animations API semantics, event ordering/cardinality,
`elapsedTime`, and CSS animation limits are unchanged. Scheduled pending animation-event batches
are finite rendering demand owned by guarded `AdvanceTo` at the exact retained scheduler head;
only an unscheduled batch is `Drive`-ready. This prevents a checkpoint spin without adding a task
source or limit. Baseline and v1 profile contracts are unchanged.

V2 persistent cookies are memory-owned by the controlled session and use its Unix-nanosecond
clock with origin zero. `Max-Age` precedes `Expires`, lifetime is clamped to 400 days, expiry at or
before controlled now deletes, and lazy purge runs before observation, request selection, and
export. SameSite uses captured schemeful site-for-cookies, the current redirect-hop method, and
the top-level-navigation bit: Strict is same-site only; Lax and unspecified also admit cross-site
top-level safe methods; Secure None cookies may cross site. Unknown or opaque context remains typed
unsupported before network start. Cross-site subresource responses store only valid Secure
SameSite=None cookies; Strict, Lax, and unspecified response cookies are ignored, while top-level
navigation responses admit all otherwise valid unpartitioned cookies. A post-open request at
controlled Unix time above u64 fails nonfatally as `unsupported_cookie_time_range` before network
start rather than wrapping; initial controlled open hardens the same code to fatal fail-stop.
Partitioned cookies and CookieStore read/getAll/delete remain unsupported. The schema remains 1,
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
