# Controlled session v2 contract

> **Status:** Versioned contract for Stasis 0.3.0. The canonical profile is
> `profiles/controlled-web-session-v2.json`. Checked-in source is not a publication or
> native-availability claim; verify the immutable tag, release, registry package, and provenance.
> `Runtime.openSession()` continues to default to `controlled-web-session-v1`; v2 must be selected
> explicitly and the native runtime must advertise it.

`controlled-web-session-v2` preserves the complete v1 session contract and adds seven
benchmark-driven compatibility slices: controlled-local messaging, bounded direct image
completion, inline-SVG rendering completion, single-line text-focus presentation and focus-event
time, synchronous public-automation event time, internal CSS animation-event time, and controlled
persistent-cookie expiry plus SameSite request selection. The owned
source added by this profile is a constructor-created
`MessageChannel` whose two ports remain in the active controlled top-level document global, and a
bounded direct `HTMLImageElement.src` completion path for canonical data SVG and direct HTTP(S)
selection in the same controlled ScriptThread and ImageCache. The focus boundary suppresses only native InputMethod UI for
controlled top-level, single-line `InputMethodType::Text` focus with `multiline = false` and no
virtual keyboard requested. Engine-generated focus transitions and admitted image completion
events receive document-clock event timestamps. A separate synchronous automation scope gives the
browser-created form events emitted by one admitted public mutating action one shared document-clock
sample. Engine-generated CSS `AnimationEvent` objects on the bounded instant finite-animation path
drain from an already-owned pending-event batch in the exact public non-auxiliary controlled
top-level WebView and receive one shared document-clock sample. These additions
preserve DOM behavior without admitting a general cross-realm messaging, image-source,
host-timestamp, event-constructor, animation, or embedder-control system.

## Controlled single-line text focus boundary

Page-driven programmatic focus, including React's `autoFocus` mount behavior, can synchronously
focus a single-line text input without requiring input from a native IME. Literal HTML
`autofocus` candidate processing is not claimed. Under v2 in the exact public non-auxiliary
controlled top-level WebView/document, a page-driven `InputMethodType::Text` request with `multiline = false` and
`allow_virtual_keyboard = false` therefore preserves DOM focus events, value, and selection but
returns before time-surface admission, visible-owner publication, embedder dispatch, or callback
creation. No external work is created or hidden from pending authority. The pre-existing semantic
automation focus guard remains profile-independent and unchanged.

For an engine-generated `focus`, `blur`, `focusin`, or `focusout` transition in that same exact v2
public non-auxiliary top-level document, `Event.timeStamp` is sampled at event creation from the document Performance
clock. It equals the document-relative controlled time, including after virtual advance, so no
host-derived timestamp value is observable to React's event normalization. The current internal
constructor may still sample and overwrite an implementation timestamp; this is an observable
clock-authority claim, not a claim that no host-clock function executes internally. Script-created
`new FocusEvent(...)` objects, events outside the separately enumerated synchronous automation and
image-completion and CSS pending-event seams, v1, nested, and realtime paths retain their existing
behavior; a
controlled read of an otherwise host-stamped value remains `host_timestamp`.

The new page-driven exception is limited to the exact `EmbedderControlRequest::InputMethod`
envelope above. Other InputMethod types, multiline or virtual-keyboard requests, and SelectElement,
ColorPicker, FilePicker, ContextMenu retain `embedder_control`; every non-top-level, realtime, or
non-session path retains its previous behavior. The frozen v1 profiles retain their exact
page-driven rejection. This is not keyboard-input synthesis, IME composition support, or a generic
embedder-control fallback.

## Controlled synchronous automation-event timestamp boundary

Immediately before executing one public mutating automation action in the active controlled
top-level document, v2 samples the document Performance clock exactly once. The sample is installed
in a synchronous RAII scope for that action and restored before the action response is produced.
Sampling failure rejects the action before mutation; an admitted action never substitutes a host
timestamp. Every browser-created event constructed synchronously during that admitted action sees
the scope and exposes the action's single document-relative `Event.timeStamp`, including after
controlled virtual advance. This is not an event-name allowlist. The implementation injects the
sample at the internal fill `InputEvent`, internal `PointerEvent`, generic browser-created `Event`,
internal `SubmitEvent`, and internal `FormDataEvent` construction seams. The frozen proof's eleven
fill-input, activation-click, reset, check-click/input/change, select-input/change, invalid, submit, and
formdata events are representative observations of the generic rule.

This is not a general rewrite of DOM event construction. `Event::new_inherited` remains unchanged,
and the WebIDL construction paths for script-created `Event`, `InputEvent`, `PointerEvent`,
`SubmitEvent`, and `FormDataEvent` do not consult the automation scope. Reading those host-stamped
objects under a controlled clock therefore still produces the existing `host_timestamp` evidence
and `unsupported_clock_surface` settlement outcome. V1, realtime, nested-document, asynchronous
event production, real user input, keyboard synthesis, IME composition, and any engine event not
created synchronously inside the public action scope retain their predecessor authority unless a
separate bounded rule says otherwise. The independent focus-transition rule above still governs
engine-generated `FocusEvent` objects, and the CSS pending-event rule below governs its exact
internal animation and transition event seam.

## Controlled CSS animation-event timestamp boundary

The existing rendering authority already retains each document's CSS animation and transition
states, pending animation-event count, finite/infinite/unsupported classification, and the 10,000
rendering-opportunity execution limit. A scheduled opportunity with pending animation events is
finite rendering demand and must use guarded `AdvanceTo` at the exact retained scheduler head,
including when that deadline equals `now`; only an event batch with no live scheduled opportunity is
`Drive`-ready. `Drive` cannot detach a controlled scheduler entry, so treating a scheduled batch as
ready would spin checkpoint turns instead of reaching the callback. This is a settlement-liveness
correction, not a new producer, task source, or execution limit.

V2 adds one timestamp adapter at the owning `Animations::send_pending_events` seam. For a nonempty pending-event
dispatch batch, `ScriptThread::current_controlled_top_level_target_matches(window)` must
conservatively reconstruct the dispatch Window as the sole retained fully-active public
non-auxiliary controlled top-level WebView/document: the WebView must have an owner snapshot, its
WindowProxy must be undiscarded and non-auxiliary, no foreign-WebView load may be incomplete, and
the sole retained Document must match its pipeline, WebView, and Window pointer. This is not a
claim that Stasis retains a separate Constellation target identity. Only then, Stasis
samples that document's
Performance clock exactly once before taking the queue. Sampling failure latches the controlled
clock terminal and leaves the batch undispatched; it never falls through to a host timestamp.

The sample is eligible only for a retained record whose queued pipeline and rooted node owner both
match that dispatch Window and document. Immediately before firing each eligible internal event,
Stasis overwrites its creation timestamp with the batch sample. The rule covers the eight event
kinds represented by the owned queue: `animationstart`, `animationiteration`, `animationend`,
`animationcancel`, `transitionrun`, `transitionstart`, `transitionend`, and `transitioncancel`.
Every eligible event in one batch therefore exposes the same document-relative `Event.timeStamp`.
A stale or mismatched record is not promoted by this adapter.

The executable compatibility proof is deliberately narrower than the adapter mapping: it exercises
one already-complete finite animation and observes owned `animationstart` and `animationend` events.
The `TransitionEvent` mapping applies only if the existing rendering pipeline has already produced
an owned transition record that reaches this pending dispatch queue. V0.3 does not claim general
CSS transition execution or settlement compatibility.

This is only a timestamp-authority expansion for browser-created `AnimationEvent` and
`TransitionEvent` objects at that exact dispatch seam. It does not expand CSS/Web Animations API
semantics, event ordering or cardinality, `elapsedTime`, animation ownership, or execution limits.
The WebIDL `new_with_proto` paths remain unchanged, so script-created `new AnimationEvent(...)` and
`new TransitionEvent(...)` objects retain host timestamps and still produce the existing typed
`host_timestamp` evidence when observed. Auxiliary top-level WebViews are deliberately excluded
even if they inherit the selected profile. V1, auxiliary, nested, realtime, stale, and owner- or
pipeline-mismatched records retain predecessor behavior.

## Controlled direct image boundary

The image slice is deliberately an ownership contract, not a declaration that every Servo image
path is deterministic. It applies only to an active top-level document selected with
`controlled-web-session-v2` that is the exact public non-auxiliary target, and only when an
`HTMLImageElement` selects its direct `src` without `srcset`, `picture`, or environment-change
selection. The initially selected serialized `ServoUrl` must be at most 65,536 bytes. A `data:`
URL is admitted only when the canonical `DataUrl` parser accepts it and its parsed MIME type is
exactly `image/svg+xml`; a direct `http:` or `https:` URL is admitted by scheme before response
metadata or decoded format is known. Resource I/O remains independently owned by the existing
network authority. A public document replacement attempted while HTTP image resource I/O remains
active retains the existing fatal `blocked_on_external_io` boundary; v2 does not claim successful
cross-document replacement through that state. The selection decision is captured on the image request and carried with that
request's generation. It is never reconstructed later from the element's current URL, response
content type, or DOM position. A final redirect URL is not rechecked against the initial 65,536-byte
selection bound; the redirected fetch remains separately owned Resource I/O and the immutable
session network policy remains authoritative. A request stores a
controlled cache ID only after its asynchronous callback registration or synchronous exact-owner
retain succeeds; URL equality alone never creates that authority.

A synchronous admitted cache hit is already executing in an owned controlled turn, so it requires
no invented asynchronous producer lease. Its request provenance and completion time are still
captured before the existing DOM-manipulation task is queued and remain bound to the same request
generation. A finite asynchronous cache decode owns an Image producer stream from callback
registration through terminal enqueue. Every cache message has its own producer guard, the guarded
envelope survives the ScriptThread handoff, and only then may the HTML element queue its ordinary
DOM callback. Loaded and failed-to-load-or-decode responses terminate that stream, so an HTTP
response that is not a decodable image remains an owned finite error completion. Message
admission failure, enqueue rejection, producer callback panic, or guarded transport loss becomes a sticky
producer terminal. After dequeue, a missing untombstoned target, a live tombstoned target, a live
Window whose profile, execution mode, or exact public top-level target does not match, or a Window
whose controlled document clock cannot be sampled explicitly abandons the message guard and latches
the same terminal. Admitted work never retries via the baseline image sender.

The image cache owns the callback's lifetime. If it authoritatively retires that callback before a
protocol terminal, dropping the callback proves that this stream can no longer enqueue document
work and completes the stream lease as owned cancellation without a producer terminal. This is
distinct from losing an expected handoff. A dequeued response completes normally as retired only
when pipeline teardown independently installed its permanent tombstone before removing the Window:
the owning
event loop received it, but no mutation target remains. Once a response reaches its live owning
Window, a handler `Err` likewise completes the scoped message guard normally. The rejected key or
owner stays in the Window's pending collections and settlement reports it as typed
`unsupported_rendering` / `image_load`; it is not producer loss. `ControlledImageMessageCompletion`
spans the live handler call: every normal return, including that retained-state `Err`, completes,
while a Rust unwind abandons before propagation. Explicit abandonment is reserved for admission,
enqueue, producer-callback panic, ScriptThread-handler unwind, pre-handler authority, clock sampling,
and guarded-transport failure. Both ordinary completion and explicit
abandonment require the exact producer-fence identity, live lease sequence, and registered
`DocumentProducerKind::Image` class before either a terminal or any completion watermark can
change; a class-mismatched lease is rejected without consuming another producer's work.

Vector rasterization is controlled only when its exact `(PendingImageId, requested size)` owner is
created by joining a retained exact `(PendingImageId, DOM owner)` identity that came from an
admitted direct request. A synchronous vector cache hit retains that identity for a later layout
join without inventing a decode producer. A layout listener may likewise join only for its exact
retained DOM owner. Each layout owner captures its provenance when that exact `(cache ID, DOM
owner)` first joins post-reflow; delivery never re-derives it from a later identity map. While
callbacks remain active, an image ID is controlled only when every callback is controlled and no
retained layout owner is baseline. A missing owner, a baseline callback or layout owner, or mixed
provenance prevents that image ID or raster key from being claimed. This describes ownership when
the cache exposes an `Image::Vector`; it is not a promise that every admitted SVG takes that
representation or that general vector-rendering semantics are deterministic.

Layout may initiate the vector-raster task before returning its pending key to ScriptThread. Once a
controlled reflow reports that work, the post-reflow handler synchronously classifies its exact
`(PendingImageId, DOM owner)`, reserves the exact `(PendingImageId, requested size)` key, and
installs the fenced completion listener before ScriptThread can publish or observe another pending
snapshot. Capacity failure latches the sticky Image-producer terminal with no baseline fallback,
but the contract does not claim that the raster task had not already started.

Pending-to-current promotion moves the retained cache identity instead of reconstructing it.
Abort, replacement, or a different-ID completion releases the exact owner idempotently. Stale
generation cleanup releases an identity only when neither the current nor pending request slot
still owns that exact cache ID, preventing same-ID ABA cleanup from deleting live authority. Any
unadmitted synchronous vector hit removes all controlled owners for its shared cache ID and
downgrades a live raster key to baseline. Favicon raster callbacks are baseline, and joining one to
a controlled raster key performs the same downgrade so a later guarded mismatch fails closed. This
is the direct HTML image slice's CSS/layout adjacency: CSS background, list-style, and
generated-content image requests do not independently gain controlled authority. The separately
declared inline-SVG slice below must establish its own exact cached-URL and DOM-owner identity; it
does not inherit authority merely because its decoded bytes are SVG. Likewise, when a baseline
layout owner joins an ID, the Window globally downgrades that cache ID and its live raster keys.
Any delivery whose provenance disagrees with any retained layout owner is rejected before
image callbacks or layout invalidation run. The owner or raster key remains retained, the delivered
message guard completes normally, and pending observation classifies that work as
`unsupported_rendering` / `image_load`.

`multipart/x-mixed-replace` is discovered only after HTTP response metadata, when the request has
already been admitted. V2 therefore does not silently relabel that callback as baseline. It marks
the exact retained image provenance unsupported, releases its controlled ownership reservations,
and retires the finite Image producer while retaining an explicit unsupported image fact. The typed
`unsupported_rendering` / `image_load` terminal becomes observable after the separately owned
Resource I/O drains; an endless response remains blocked on that external I/O. Neither baseline nor
controlled callback delivery can invoke the retained unsupported callback. This exception is
limited to an admitted v2 direct HTTP(S) request. Baseline and v1 multipart behavior is unchanged.

At most 512 controlled image ownership records may be retained by one Window. Each pending
controlled callback, each exact `(cache ID, DOM owner)` identity, and each controlled
vector-rasterization key owns one non-cloneable capacity reservation. Multiple DOM owners sharing a
cache ID therefore consume distinct records. Rejection of record 513 latches a bounded
Image-producer admission terminal, and there is no baseline fallback. On the HTML decode
`ReadyForRequest` path, callback and identity reservations succeed before the cache request is
issued. On the post-reflow raster path, record 513 can be observed after layout initiated the task;
the terminal still exists before another pending snapshot can be published or observed. A
cached-vector identity retain checks that terminal before even an idempotent success. Normal
terminal removal releases the corresponding record. Navigation or script-runtime teardown clears
callback, identity, layout, and raster maps together, releasing every retained reservation;
ImageCache teardown drops the corresponding producer callbacks.

Pending observation treats the union of callback and per-owner layout `PendingImageId` values as
logical image identities and treats each `(PendingImageId, requested size)` raster key as separate
work. An image ID is controlled only when every retained callback for that ID is controlled and no
retained callback or layout owner is baseline or explicitly unsupported. Live record count must
equal retained controlled callbacks plus
exact `(cache ID, DOM owner)` identities plus controlled raster keys, and, absent a producer
terminal, the Image producer's pending count must be at least the number of controlled logical work
items. Controlled items are represented by the producer fence and are removed from the generic
pending-rendering image count; baseline, missing, mixed, or otherwise unowned items
remain `unsupported_rendering` / `image_load`. Ready response messages are represented by their
guarded envelopes during the handoff rather than guessed from Window collections.

For one admitted completion, the document Performance clock is sampled exactly once. The same
`PerformanceEntryTime::Document` value is carried to every engine-generated image completion event
that Servo emits for that completion. The ordinary terminal path therefore gives `load` and
`loadend`, or `error` and `loadend`, the same observable `Event.timeStamp`. Servo's existing
synchronous-cache-hit path emits only `load`; v2 preserves that cardinality while stamping the
event from the same controlled boundary. A host-domain value is never substituted for an admitted
image completion. Script-created events and unadmitted image paths retain predecessor behavior.

Blob, file, non-SVG data URLs, over-size initial URLs, over-cap registrations, `srcset`, `picture`,
environment changes, favicon, video poster, `ImageBitmap`, canvas upload, animated image semantics,
iframe, worker, worklet, and cross-event-loop paths receive no new authority from the direct HTML
image slice. HTTP(S) response formats are not pre-admission allowlisted; finite cache/decode success
or failure is owned, while the post-metadata multipart case above remains explicitly unsupported.
Except for the explicit over-cap terminal, this is not a universal eager-rejection promise:
unadmitted paths retain baseline behavior, retained asynchronous work remains rejected by existing
pending-rendering authority, and an observed host timestamp remains rejected by existing clock
authority. The gate does not inspect SVG payload contents. Nested or external SVG resource
semantics are not proven or admitted by this slice; separately surfaced external work remains under
the existing resource-I/O authority. The initial selected-URL and retained-owner limits bound
admission and callback scheduling; they do not claim a separate deterministic CPU or allocation
budget for image parsing, decoding, or rasterization. Cache reuse is proven only within one
pipeline's image-cache store and the immutable fixture-route model; live or mutable HTTP content is
not deterministic proof. Existing wall, ordinary-task, rendering, and resource-I/O limits remain
authoritative.

## Controlled inline SVG rendering boundary

The second image slice is independent of `HTMLImageElement`. It applies only to an inline root
`SVGSVGElement` in the active top-level document selected with `controlled-web-session-v2` when
that document is the exact public non-auxiliary target. Servo
must have internally serialized that exact element subtree into its cached data URL, the layout
request must be marked `InternalRequest::Yes`, and the request URL must exactly equal the cached
serialized URL for that same DOM owner. The serialized URL is admitted only when it is at most
65,536 bytes and the canonical `DataUrl` parser reports exactly `image/svg+xml`. An arbitrary
script-, CSS-, layout-, or embedder-supplied data URL cannot impersonate this authority.

The inline owner must join the exact `PendingImageId`/DOM-owner cache identity. Finite decode and
vector-raster completion then use the same bounded retained-record inventory, Image producer
fence, guarded ScriptThread handoff, mixed-owner downgrade, and exact raster-key reconciliation as
the direct image slice. A missing identity, mismatched cached URL, non-internal request, capacity
failure, provenance mismatch, abandoned callback, or guarded transport failure never falls back to
the baseline sender. A delivery-time provenance mismatch remains retained typed unsupported work;
it is not converted into producer abandonment. Controlled work is represented by the Image producer
fence rather than hidden in generic pending rendering.

This is a rendering-completion claim, not an event-surface expansion. The internal inline-SVG
completion creates no DOM `load`, `error`, or `loadend` event and therefore makes no new timestamp
claim. Baseline and v1 behavior, general SVG rendering semantics, nested SVG trees, `<use>` or other
resource expansion, external SVG resources, CSS image consumers, iframe/worker/worklet/cross-loop
paths, animation, decoder CPU/allocation bounds, and unrelated resource I/O retain their existing
authority. In particular, successful settlement of this narrow path is not evidence that arbitrary
SVG documents or subresources are deterministic.

## Controlled local channel boundary

- Construction is admitted only while `ScriptThread` can reconstruct the receiver as the exact
  fully-active public controlled top-level target and the incumbent global exists and matches the
  receiver's global, pipeline, and WebView identities. Those checks happen before either port is
  published. A borrowed constructor, missing incumbent, replaced or discarded target, or auxiliary
  owner throws `NotSupportedError` and latches `external_subscription` before a pair can exist.
- `MessagePort.postMessage()` resolves and validates that same exact incumbent/owner identity before
  structured cloning. Rejection therefore occurs before serialization, transfer detachment,
  retained-message reservation, or dispatch; a foreign realm cannot borrow the owning Window's
  controlled task authority.
- At most 32 controlled-local native port entries may be retained in one global. Starting from a
  global with no one-ended terminal identities, that entry bound admits at most 16 complete pairs.
  Each retained one-ended terminal identity consumes one of the 32 entries and therefore reduces
  the remaining complete-pair capacity. Explicitly closed entries remain capacity-bearing until the
  DOM garbage-collection checkpoint prunes them; while any queued controlled-local message
  reservation remains, closed entries stay as bounded identity tombstones until that reservation
  drains and a later checkpoint prunes them. Closure cannot hide owned work or bypass the entry
  bound within one script task.
- At most 1,024 messages may be retained across queued port-message tasks and unstarted-port
  buffers in that global.
- One post may contain at most 65,536 serialized bytes. Its transfer list and structured-clone
  sidecar maps must be empty.
- Each dispatched `message` event is one ordinary controlled task and consumes the existing
  cumulative `executionLimits.ordinaryTasks` budget. Recursive posts receive no private drain or
  additional budget. A microtask checkpoint follows each dispatched message task.
- A reciprocal local pair is idle only when both DOM and implementation peer identities agree,
  neither port is pending, detached, or in transfer, and neither queued nor buffered messages
  remain. A terminal port is also idle when Stasis has synchronously proven and recorded the local
  disentanglement and both its DOM and implementation peer identities are absent. Such an idle pair
  or proven terminal port is inert and does not prevent quiescence. Missing or inconsistent peer
  evidence without that local terminal proof remains pending and cannot prove quiescence.

The `port_message` task-source provenance is enforced internally. In the prospective 0.3 proof
surface, retained queued or buffered port-message work appears as `sources.kind = tracked_presence`
with `openEnded.reason = message_port`; dispatched callbacks contribute to the aggregate
`processed.tasks` count. Every admitted reservation acquires its exact destination-port identity
before retention in either the ordinary task queue or the destination's native disabled-port
buffer. Pending observation succeeds only when the global retained count equals the sum of exact
per-destination queued counts plus the sum of native buffered counts. An invalid destination
identity, a zero queued association, or a missing association exposed by that reconciliation fails
pending observation closed; there is no global nonzero fallback identity. A well-formed reciprocal
pair with owned work projects exactly one deterministic minimum port identity, so independent pairs
with work remain independently visible; malformed or nonreciprocal identities remain individually
pending. A zero retained count does not make an otherwise-idle open pair pending. V2
does not claim a public per-source dispatch counter, and the frozen/common wire schema is not
expanded for one.

An otherwise-valid port-transfer attempt, an otherwise-reached cross-global or cross-event-loop
port, BroadcastChannel construction, Constellation-routed ports, external ingress, and
sidecar-bearing structured clones remain unsupported. A non-empty `MessagePort.postMessage()` transfer list
throws `NotSupportedError` and latches the existing `external_subscription` surface
before serialization, detachment, or dispatch. An already-detached port instead preserves the
platform boundary: transfer validation throws `DataCloneError`, while `postMessage()` is a no-op
before Stasis admission. Serialized-byte and sidecar limits are necessarily checked after
serialization, but still throw and latch before the message can be retained or dispatched.

The ordinary controlled child-context path rejects nested-window/iframe creation earlier with
`same_event_loop_iframe`, before a nested global or port can exist. The profile retains
`cross_event_loop_iframe` as a conservative alternate-ingress fence, not as the demonstrated
ordinary iframe-creation outcome. Worker creation likewise latches `worker` before a worker global
or worker-owned port exists; worker-global `MessageChannel` is therefore unreachable through the
selected profile and is not claimed as independently admitted or evidenced.

Controlled-local managed state owns no MessagePort router identifier and never registers a
Constellation MessagePort router. In the normal production `GlobalScope` path, an impossible
mixed-provenance registration or router/external callback latches `external_subscription`, grants
no ingress authority, and installs or dispatches nothing. This is a product execution boundary,
not a hostile or forged renderer-IPC security claim. BroadcastChannel is prevented at construction;
v2 does not claim a separate impossible-callback backstop for it.

For an arbitrary structured-clone transfer list in the selected controlled global, Stasis scans in
list order for an otherwise-valid `MessagePort`, `ReadableStream`, `WritableStream`, or
`TransformStream` before any JavaScript transfer step and stops at the first failing entry. Rejection
at this controlled boundary throws `NotSupportedError`, latches `external_subscription`, and leaves
an earlier transferable such as an `ArrayBuffer` attached. For each checked entry, an already
detached `MessagePort` or locked stream retains its platform `DataCloneError` precedence before
Stasis applies its boundary to that entry. This is a typed rejection boundary, not
transferable-stream support or general transactional rollback for other transfer failures.

## Controlled cookie/session-state boundary

V2 owns persistent cookies in memory for the lifetime of its single controlled session. It never
loads or writes a host browser profile, cookie database, or other disk-backed jar. Persistence
across processes is available only through an explicit `controlled-web-session-v2` state export
and initial import. The state envelope remains `schemaVersion: 1`, but its literal `state.profile`
is `controlled-web-session-v2`; a v1 artifact is not silently migrated or accepted as v2.

Cookie expiry is measured against the session's controlled Unix clock with origin zero. A valid
`Max-Age` takes precedence over `Expires`; retained expiry is clamped to at most 400 days from the
controlled receipt time. Expiry at or before controlled now deletes the cookie, and expired
records are lazily purged before cookie observation, request selection, and state export. The
portable wire field remains the canonical decimal-u64 `expiresUnixTimeNs` string, decoded by the
TypeScript SDK as `bigint | null`. Session cookies retain `null`.

Cookie authority is bounded by that same u64 Unix-nanosecond domain even though the general
controlled clock accepts u128 values. When a post-open request observes controlled Unix time above
`18446744073709551615`, it fails nonfatally as `unsupported_cookie_time_range` with partial request
state effect before network start or Cookie header construction. During initial controlled open,
the shell hardens that same code to a fatal fail-stop because no usable session can be returned. A
persistent expiry that has no u64 headroom uses the same typed boundary. Page-authored cookie APIs
surface the equivalent `NotSupportedError`; they do not truncate or wrap time.

SameSite selection uses the schemeful site-for-cookies captured from the request client, the
current redirect-hop method, and whether the request is a top-level navigation. `Strict` cookies
are selected only same-site. `Lax` and unspecified cookies are also eligible on cross-site
top-level `GET`, `HEAD`, `OPTIONS`, or `TRACE`; `None` requires `Secure` and may be selected
cross-site. Ineligible cookies are filtered before Cookie header construction without creating a
settlement terminal. An unknown or opaque request context still fails closed as
`unsupported_cookie_same_site_context` before network start.

Response-cookie storage uses the same captured schemeful context. A same-site response or any
top-level-navigation response may store every otherwise valid unpartitioned cookie. For a
cross-site subresource response, only a valid Secure `SameSite=None` cookie is eligible; Strict,
Lax, and unspecified cookies are ignored without a terminal. The redirect-hop method is not an
input to this response-storage admission rule.

Partitioned cookies remain `unsupported_partitioned_cookie`. `cookieStore.get()`, `getAll()`, and
`delete()` retain `controlled_cookie_store_read_delete_unsupported`; this slice does not create a
general Cookie Store API or durable browser profile.

## Identity and compatibility

The protocol envelope remains version 1. Open requests and responses carry the explicitly selected
v2 profile, and the TypeScript client rejects an unadvertised or mismatched response. SDK-produced
settle results are privately bound to that selected profile, so both `settlementEvidence(result)`
and `session.settlementEvidence(result)` preserve it; a contradictory explicit profile is rejected.
Unbound manually constructed results retain the legacy v1 default.

V2 pins its predecessor profile by SHA-256
`9b62b9245b2c6a6f9620b117da6787a18df9298be1115cbce2e6c3d5439cc41a`. Candidate validation
re-hashes that frozen `controlled-web-session-v1` file independently; naming the predecessor is not
accepted as evidence that its bytes stayed unchanged.

V2 expands only the declared execution, headless-presentation, and controlled cookie-state
surfaces. Web Storage retains its predecessor behavior inside a profile-matched v2 artifact.
Import and export preserve that exact v2 identity and there is no implicit state migration.
