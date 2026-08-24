# Controlled web session contract 0.2

> **Status:** Stable release contract for Stasis 0.2. The canonical, frozen profile is
> `profiles/controlled-web-session-v1.json`. `controlled-webapp-v1` remains frozen and is still
> the legacy default.

## Product boundary

Stasis 0.2 extends the controlled engine into a bounded web-session product. It
does not try to become a complete Playwright replacement. One owned native
process runs exactly one session. That session owns one WebView, one controlled
Script event loop, and one active top-level document at a time, while allowing
checked replacement documents on the same event loop. `session.close` is
terminal: its response is followed by process exit. There is no sequential
session reuse, return-to-initialized state, or public `runtime.close` contract.

The TypeScript SDK may coordinate multiple fresh processes through a bounded
pool. The normal session story is:

```text
spawn fresh owned process
  -> create controlled-web-session-v1 with immutable network policy
  -> optionally import bounded state before the first request
  -> open and settle the initial document
  -> inspect and use semantic form actions with its state token
  -> observe checked hash/history changes or replace the top document
  -> reject tokens from every earlier authority
  -> read bounded requests and diagnostic evidence
  -> export session state if requested
  -> session.close
  -> process exits and the pool discards it
```

A fatal transport error, command timeout, abort of a written command, or
indeterminate state effect fail-stops the process. The pool discards that
process and never retries work whose effects may have occurred.

## Backward compatibility

Protocol envelope version 1 remains sufficient because the new methods and
result fields are selected by an explicit profile. The 0.2 native runtime
advertises both profile IDs. The legacy TypeScript `Runtime.open()` API keeps
selecting `controlled-webapp-v1`, keeps using `expectedGeneration`, and keeps
every v0.1 request, result, typed outcome, and terminal-close shape. A separate
session API selects `controlled-web-session-v1`; it never accepts a legacy
generation as document authority.

Each process has one process-local session ID. There is no later session ID to
allocate in that process. After the close response, EOF is the only valid
transport state.

## Document-scoped authority

Every new-profile operation that targets or may advance a document consumes an
opaque `expectedStateToken`. Successful results expose the authoritative
`stateToken`. The token is session-local, non-persistent, and binds all of:

- the owned WebView and controlled Script event loop;
- the active top-level document and checked `documentEpoch`;
- checked `navigationId` and `historyRevision`;
- navigation and pipeline-membership revisions; and
- the complete runtime state generation.

The wire spelling is canonical and tightly bounded:
`document:<32 lowercase hexadecimal namespace characters>:<nonzero u128 decimal
alias>` (at most 81 bytes). The 128-bit namespace comes from the operating
system CSPRNG and entropy failure fail-stops before a token can authorize work.
Consumers must still treat the complete value as opaque. A well-formed token
from another Stasis process parses normally but is stale, and token debug/error
paths redact the namespace.

The SDK applies one forward-compatible opaque-token boundary of 256 UTF-8 bytes
on both outbound and inbound values before request serialization or use. It does
not parse token contents. The native v1 canonical spellings remain stricter at
81 bytes for document authority and 61 bytes for session-state authority; the
SDK cap is allocation and protocol hardening, not permission for alternate
native syntax.

The shell retains only the current binding. Equivalent passive observations
reuse it. Navigation admission, document activation, same-document history
change, target-authority change, or complete-state generation change rotates
it. A token from an earlier state or document fails before DOM access with
`stale_state_token`, `fatal: false`, and `stateEffect: none`.

`runtime.pending` is the read-only recovery operation: it reports the current
snapshot and token without requiring an expected token. `runtime.settle`,
`runtime.advance_to_next`, `session.navigate`, all semantic actions, `query`,
`text`, and `extract` require the current token. Mutations return a replacement
token; passive operations echo the still-current binding. The Script owner
revalidates the private document identity and generation at the operation's
execution linearization point.

## Session-state authority

Cookies and Web Storage outlive a document, so they cannot be bound to the
document token or DOM generation. The session owns a distinct opaque
`sessionStateToken`. Native cookie and Web Storage backends keep separate
monotonic cookie and aggregate Web Storage revisions; the shell combines them
into this one opaque token, never exposes the components, and revalidates both
immediately before a mutation linearizes.

Its canonical spelling is
`session:<32 lowercase hexadecimal namespace characters>:<nonzero u64 decimal
alias>` (at most 61 bytes). It uses an independently generated 128-bit OS-CSPRNG
namespace, fails closed if entropy is unavailable, is redacted from debug and
errors, and never authorizes a different process even when both aliases have
the same ordinal.

The public method names are fixed:

- `session.cookies.get` and `session.cookies.set`;
- `session.storage.get` and `session.storage.set`;
- `session.state.export`; and
- `session.state.import`, retained only as the typed post-publication rejection
  endpoint described below.

Every successful cookie or storage set consumes `expectedSessionStateToken` and
returns a fresh `sessionStateToken`. A stale value fails with
`stale_session_state_token` and no state effect. Cookie and storage set methods
are individually atomic at their backend boundary.

Full state import has exactly one successful public entry point:
`session.open({ state })`. While the fresh session exists in its unpublished
pre-navigation builder, with navigation and network-start revision zero, the
builder authorizes the import with its hidden current session-state token. The
caller neither possesses nor supplies that token. The builder validates the
complete artifact before any backend mutation, installs cookie and Web Storage
state through the construction-before-publication seam, and returns the rotated
token in the successful open result. There is no public standalone-import
window. The public Session object exposes `session.state.import` only after
publication, where it unconditionally fails `session_state_import_phase_closed`
with no state effect before inspecting its sensitive request payload.

Exported state is sensitive. Cookie and storage names and values are excluded
from settlement evidence, the diagnostic event stream, logs, release proofs,
and thrown error messages. Compact encoded state is capped at 512 KiB so it and
its NDJSON envelope remain below the 1 MiB frame boundary. The frozen static
partition admits at most 256,000 compact JSON bytes for the complete `cookies`
array and 256,000 bytes for the complete `origins` array. Each fragment includes
its brackets, commas, field names, escaped string bytes, and decimal sequence
strings. That leaves 12,288 bytes for the public envelope and slack: the fixed
v1 envelope excluding both arrays is exactly 147 bytes, leaving 12,141 bytes of
slack. The shell and both backends enforce their side of this partition before
mutation. Controlled page Web Storage writes stage the complete candidate and
enforce the key, value, entry, origin, per-origin, and exact encoded-fragment
bounds before data or its revision changes. A rejected page candidate throws a
secret-safe `QuotaExceededError`; every successful controlled page cookie or
Web Storage mutation therefore preserves later `session.state.export`.
Ordinary Servo profiles retain their existing Web Storage quota. The remaining
bounds are 512 aggregate cookies, 150 cookies per Servo registrable host, 4 KiB
of combined UTF-8 string data per cookie, 512 KiB
of combined cookie string data, 64 origins, 1,024 entries per storage kind and
origin, 4 KiB keys, 128 KiB values, and 512 KiB total storage per origin.
IndexedDB, Cache Storage, and Service Worker registrations remain unsupported.

### Cookie boundary

The state contract supports unpartitioned session cookies only. A `Set-Cookie`
with `Expires`, `Max-Age`, or `Partitioned` is rejected before jar mutation as
`unsupported_persistent_cookie` or `unsupported_partitioned_cookie`; imports
with non-null expiry or `partitioned: true` fail by the same typed validation.
Malformed controlled `Set-Cookie` metadata fails as `invalid_controlled_cookie`
at that same pre-mutation boundary.

Portable cookie domains use the canonical host spelling; IPv6 is unbracketed.
Leading or trailing dots, ports, credentials, non-canonical aliases, and
non-host-only public-suffix domains are invalid. Servo's native public-suffix data is
authoritative for that final classification. `__Secure-` and `__Host-`
matching is ASCII case-insensitive;
`__Secure-` requires `secure: true`, while `__Host-` additionally requires a
host-only cookie at path `/`. These checks run in shell preflight as well as the
lower jar so a direct protocol request cannot enter compare-replace and then
fail with an indeterminate effect. Invalid raw protocol state is rejected before
operation entry and jar mutation with `stateEffect: none`.

The exact per-host capacity is 150 cookies keyed by Servo's native
public-suffix registrable suffix, or by the canonical IP address for IP hosts.
Shell preflight rejects the 151st final-jar cookie for one such key as
`too_many_session_cookies_per_registrable_host` before unpublished backend
replacement or live compare-replace; the lower jar enforces the same bound
atomically. Replacing an existing cookie does not consume a new slot. The SDK
does not duplicate public-suffix classification: native admission remains the
authority and surfaces the typed, secret-safe error with `stateEffect: none`.

The page-cookie API is frozen narrowly for `controlled-web-session-v1`:

- `document.cookie` get and set are supported. Reads use controller-owned
  ordering and access stamps; writes apply the same controlled cookie policy as
  response headers.
- `cookieStore.set()` is supported through that atomic controlled boundary.
  Invalid input rejects with `TypeError` carrying `invalid_controlled_cookie`;
  persistent or partitioned input uses the same typed unsupported codes as
  response cookies.
- `cookieStore.get()`, `cookieStore.getAll()`, and `cookieStore.delete()` are
  deferred. They reject with `NotSupportedError` carrying the secret-safe code
  `controlled_cookie_store_read_delete_unsupported` before ordinary resource
  callbacks, wall-time ordering, or mutation can begin.

Every successful controlled write is staged against a private copy of the jar,
then re-exported and validated against the per-record, 512-cookie, and 512 KiB
raw aggregate bounds plus the 256,000-byte encoded cookie-array partition before
commit. Replacement and eviction are evaluated on the projected final jar. A
single raw controlled cookie string is capped at 8 KiB before parsing, and one
atomic response batch is capped at 512 values before allocation. A rejected page
or response mutation changes neither jar state nor controller ordering stamps,
so it cannot make a later state export fail. None of these page APIs can fall
through to Servo's ordinary cookie paths while the session profile is active.
Each exported/imported `creationSequence` and `lastAccessSequence` is a
canonical decimal u64 string (`0` through `18446744073709551615`); creation
sequences are unique within the cookie array, and last-access sequences are
independently unique within that array.

SameSite metadata round-trips, but requests are claimed deterministic only at
the audited boundary: `None` requires `Secure`; `Strict` requires a
schemeful-same-site request; `Lax` uses that same deliberately narrow v0.2
boundary. A main-frame navigation establishes its target URL as the new cookie
context. Any other controlled HTTP(S) request whose target is cross-site with
the current top-level URL, or whose context is indeterminate, fails
`unsupported_cookie_same_site_context` before network start or state mutation.

## Navigation contract

Initial and explicit navigation are fetch-backed HTTP(S) top-level operations.
`session.navigate` consumes the current document token and returns requested
and final URLs, `stateGeneration`, `domEpoch`, `documentEpoch`, `navigationId`,
`historyRevision`, and a fresh token at the `controlled_ready` boundary. A
written navigation whose response is lost has an indeterminate effect and
fail-stops the process.

Same-origin and cross-origin HTTP(S) replacement are supported only while the
WebView preserves its already-bound controlled event loop. Form submission and
application-triggered replacement use the same rule. A non-HTTP(S) or
cross-event-loop replacement is rejected before uncontrolled authority becomes
active.

History traversal remains a typed boundary rather than a silent no-op.
`history.back()`, `history.forward()`, and nonzero `history.go()` under
`controlled-web-session-v1` throw `NotSupportedError` before their script
request reaches Constellation and latch the `history_traversal` time surface.
Every alternate Constellation traversal ingress also latches that surface on
the target controlled session before declining the traversal, so the next
`settle()` terminates as `unsupported_work`; it cannot report quiescence.
`history.go(0)` retains reload semantics. Frozen `controlled-webapp-v1` and
realtime profiles keep their prior behavior.

When a controlled turn itself admits a replacement, Stasis never replays that
turn. It counts the turn once, freezes the original settlement budgets, obtains
a passive no-pump session-authority snapshot, and accepts only the exact
checked transition from one active pipeline to that same pipeline plus one
pending HTTP(S) replacement. It then consumes the single qualified
`SpawnPipeline` lifecycle event. Once the replacement independently reaches
controlled-ready, settlement binds the new document authority and rearms the
same operation; one `settle()` may therefore cross multiple replacements while
retaining its original budgets. A transport loss, revision gap, extra pipeline,
or different target is fatal and remains indeterminate.

The profile owns one live top-level document, not a BFCache. On replacement,
the old pipeline is converted to a reloadable history entry and closed before
the new document can return completed control authority. The exact exit
lifecycle is prioritized so inactive author script cannot remain silently live;
`pagehide`/`unload` fidelity during that discard is not claimed in 0.2. History
traversal and BFCache restoration remain unsupported.

The navigation counters are checked unsigned 64-bit values:

- Initial successful document activation sets `documentEpoch` to 1. Each
  successful replacement activation increments it. The initial document does
  not count against the 1,000-replacement budget.
- Initial navigation has `navigationId` 0. Replacement IDs 1 and above are
  reserved monotonically when replacement is admitted and are never reused,
  even if fetch or activation fails. Redirects retain the same navigation ID.
- `historyRevision` starts at 0 and increments for every admitted fragment,
  `history.pushState`, and `history.replaceState` authority change. Even
  `replaceState` increments it, preventing ABA authorization. It is
  session-monotonic and never resets when a document is replaced.

The next replacement after 1,000 successful replacements fails before admission
or network start with `document_transition_limit_exceeded`; it does not consume
an ID and reports `limit`, `observed`, and `nextNavigationId`. The 10,001st
same-document change fails before mutation with `history_limit_exceeded` and
reports `limit`, `observed`, `navigationId`, and the current revision. Redirect
failure reports `limit`, `observed`, and `navigationId`. A navigation follows at
most 20 redirects. Hop 21 terminates
with `redirect_limit_exceeded` and a partial state effect because network work
has already occurred, then fail-stops the process. Arithmetic overflow is a
distinct `runtime_error` and fail-stop. History traversal, downloads, auxiliary
tabs, and child browsing contexts remain unsupported; reload is explicit
navigation.

For `unsupported_navigation_scheme` and the two pre-admission limit errors,
an explicit navigation (or an observation made before any enclosing request
work) is nonfatal with `stateEffect: none`. When an application-triggered
failure is discovered after an enclosing activation, submission, or settlement
has already performed work, that earlier effect cannot be rolled back: the
public error is conservatively `stateEffect: partial`, fatal, and fail-stops the
process. In both cases the rejected navigation itself starts no pipeline or
network work and does not mutate navigation authority.

## Selectors, automation, and extraction

`practical_selector_v2` retains type and universal selectors, IDs, classes,
no-namespace attribute presence/equality (`[name]` and `[name="value"]`), and
comma-separated lists, and adds descendant and child combinators. Exact
equality precharges the safe serialized byte upper bound of every relevant
candidate/ancestor value before Stylo matching. A targeted lazy inline-style
declaration whose serialization does not already exist is rejected before
matching because Stylo cannot yet serialize it through a bounded writer. Token,
dash, prefix, substring, and suffix operators are rejected because they can
scan an arbitrarily large page-controlled value; namespace-generic attribute
selectors are rejected because they can scan multiple values.
Sibling/column combinators, named namespace prefixes, pseudo-elements, and
every pseudo-class remain rejected in the first profile. A later component may
be advertised only when it is explicitly enumerated and its hidden traversal
has a proven bounded cost; `:has`, `:lang`, and dynamic-state pseudos are not
implicit surface.

For each candidate, selector work is conservatively charged as:

```text
(local units + attribute entries * attribute components
             + relevant value bytes * equality components)
            * (child combinators + 1)
            * (ancestor depth + 1) ^ descendant combinators
```

The attribute terms include every entry and relevant equality value on the
candidate and structurally considered ancestors because Servo's no-namespace
lookup is vector-backed and equality may compare the full value. The traversal
and selector-unit counters each retain the one-million-unit bound. This makes
the added structural selectors practical without allowing Servo's matcher to
walk uncharged trees or values.

`fill` retains v0.1 semantic replacement for mutable `textarea` and input types
`text`, `search`, `url`, `tel`, `email`, and `password`: replace the value and
dispatch one bubbling, composed, non-cancelable `input` event with `inputType`
`insertReplacementText`. It does not synthesize focus, keyboard, or change
events. `activate` retains `HTMLElement.click()` semantics.

Every mutating semantic action preflights Servo's native hidden work before it
fires an event or changes the DOM. Work is frozen against the action's actual
native path, not the size of an unrelated page. For an event target, `P` is the
exact composed node path (shadow-root host, otherwise assigned slot, otherwise
parent), `R` is the sum of shadow-including ancestor depths over `P`, `D` is the
target depth, and `H` is the number of shadow-root retarget hops. One event
reserves `(1 + H_related) * R_target + H_related * D_target + 4 * P_target +
32`; a focus transition reserves two old-to-new and two new-to-old events.
Delegated focus and editing-host initialization additionally charge only the
relevant shadow/content subtree, its attribute entries, ancestor steps, and
text bytes.

Validity work uses the target control's owned-form count `C`, light-DOM
ancestor count `A`, and nearest-fieldset subtree count `F`. Radio work uses the
actual light-tree root `Rroot` and exact same-owner/same-name group size `G`,
reserving `6 * (Rroot + G * Rroot + sum(C_i + A_i + F_i))` plus its click,
input, change, and fixed work. Submit/reset inspect only controls owned by the
actual form, their possible invalid-event paths, dataset ancestor paths,
options/files/custom data, and relevant validity/radio work. Generic activate
classifies the first activatable element on the exact click path; a label pays
only its real control-resolution tree and the resolved control's nested click.
Consequently, a shallow link, plain button, or ordinary focus remains usable on
a 10,000-node page containing unrelated large forms, while a genuinely large
radio group or owned form is rejected before mutation. All reservations share
the cumulative one-million-unit DOM-work ceiling; fixed action work is 256
units. A reservation failure is nonfatal with `stateEffect: none`.

Submit validity reserves `sum(C_i + A_i + F_i)` across every owned control,
the complete target-specific radio root/group reservation for each radio, and
two inspected-option-tree passes for each select (placeholder plus selected
option validation). Because every owned control has the same owning-form count,
the exact `C*C` part is reserved before entry output; the target-specific
ancestor, fieldset, radio, and option terms remain preflighted before
`requestSubmit`. Form-dataset cardinality is bounded by
`E = 2*C + sum(max(fileEntries_i - 1, 0)) +
sum(max(selectedOptions_i - 1, 0)) +
sum(max(elementInternalsEntries_i - 1, 0))`. The conservative `2*C` base
covers one ordinary datum and one possible `dirname` datum per owned control
and is reserved before control content is inspected. Each file-list,
multi-select, or `ElementInternals` fan-out excess is reserved immediately
after its O(1) or already-bounded count and before its entry values are
iterated. Thus empty entries cannot evade either work or output limits.

Each of those `E` entries consumes one DOM-work unit, one raw-cardinality byte,
and a 135-byte derived-output envelope; the final multipart closing boundary
consumes 53 bytes. The 135-byte constant covers a maximum 47-byte generated
boundary plus the largest fixed file disposition/content-type/trailer form,
including the `text/plain` fallback. Page-controlled strings additionally use
the six-times encoding envelope and file bodies use their exact byte length.
Selected-file and selected-option fan-out repeats the owning control `name`,
so every copy beyond the one already charged by the element attribute scan is
also charged. File strings include filename and `Blob.type`; custom FormData
charges every datum name, string or filename, and file type.

The preflight also bounds page-controlled scratch strings before an IDL getter
or form encoder can materialize them: target attributes/private state and only
the action-relevant form controls, options, files, contenteditable/delegated
focus subtree text, and form-associated custom-element data. Raw scratch and a
conservative six-times derived encoding envelope each fit the 128 KiB output
ceiling. Synchronous page handlers can change the DOM after this snapshot; that
explicitly separate work is governed by the command wall-time boundary, and a
timeout or unprovable post-action observation is fatal `outcome_indeterminate`,
never a definitive rejection.

An action result may carry top-level or same-document navigation authority only
when script emitted it synchronously during that controlled turn. A queued
native link/form default and navigation from later timer, fetch, or other async
work remain for an explicit `settle()`. Any post-action authority drift that
cannot be proven from the synchronized result is fail-stop with partial or
indeterminate state effect, never definitive `none`.

The additional state-token-bound form actions are:

- `action.focus`: call focus with `preventScroll: true`, returning `focused`.
  A non-focusable HTML element producing `focused: false` is a successful
  observation, not a typed operation failure.
- `action.check`: for a mutable checkbox or radio, click only when needed and
  return observed `changed` and `checked`.
- `action.uncheck`: the same for mutable checkboxes only; radios cannot be
  unchecked through this operation.
- `action.select`: atomically prevalidate unique requested values against
  enabled options. A single select requires exactly one value. Every direct
  select child and optgroup child inspected by the option-list algorithm is
  charged, as are every option attribute entry and option-text node inspected
  for value/label reads. Selectedness is updated in bulk, and validity is
  refreshed once.
  Before mutation the engine also reserves one document walk plus three
  conservative full-document passes for form controls, fieldset discovery, and
  the fieldset subtree, along with 128 fixed units for the implementation-owned
  UA shadow tree's creation, `SetData` observer walk, and version-ancestor
  walks.
  Only a change dispatches one bubbling/composed `input` followed by one
  bubbling `change` event. Before selectedness changes, both dispatches reserve
  the exact composed target path (`2 * event(target, null)`) and its path
  attributes share the operation's cumulative raw scratch budget. Because
  those handlers run synchronously, the
  selected values are then rescanned from the current option tree and returned
  in current DOM order. A work/output failure during that post-event
  observation is fatal `outcome_indeterminate` with `stateEffect:
  indeterminate`; the already-observed mutation cannot be reported as a
  definitive rejection.
- `action.submit`: target a form and call `requestSubmit(null)`, not `submit()`,
  returning `submitted`. This means invocation succeeded; it does not guarantee
  that validation passed or navigation occurred. Any resulting navigation
  follows the navigation contract.

`query`, `text`, and ordered `extract` remain bounded and handle-free.
Extraction reads are `text`, `html`, `attribute`, and `resolved_url`. Raw
content-attribute reads return the source string or `null` when missing.
Resolved URL reads use the current document base URL, including the first valid
`<base href>`, and return `null` when the attribute is missing or unresolvable.
The engine, not the crawler client, performs this resolution. Attribute names
are nonempty, contain no ASCII whitespace/control, and are at most 256 bytes.
Every attribute entry on the target element is charged before Servo's
vector-backed content-attribute lookup.
URL/base/raw temporaries and the final optional result all consume the output
budget. Inside a v0.2 extraction field, an empty selector explicitly means the
current row root itself; a nonempty selector retains exact-one descendant
matching. The self form is v0.2-only, charges an explicit row-root visit, and
lets crawler workloads read each matched link's own `href` without handles or
an unbounded pseudo-class. Frozen v0.1 continues to reject an empty selector.

The other limits remain 4 KiB selectors, 128 KiB fill values and logical
output, 256-byte field names, 16 extraction fields, 128 matches, and one million
visited nodes. Select accepts at most 16 requested values and 128 KiB total
requested-value bytes.

## Settlement and declarative network fixtures

The v0.1 settlement outcomes and execution ceilings remain intact: 100,000
ordinary tasks, 1,000,000 microtasks, 10,000 rendering opportunities, and
1,000,000 mutations. Tasks, microtasks, MutationObserver, one-shot timers,
finite rendering, rAF, Date, and Performance stay controlled. Intervals remain
persistent work.

Asynchronous fetch and XHR remain supported; synchronous XHR is rejected before
start. A session may supply an immutable route table in `session.open` before
the first request. Routes are evaluated in order, first match wins, and a match
is never consumed. A route matches an HTTP method plus an exact URL, prefix, or
simple glob where `*` is the only metacharacter. It has one fixed action:
bounded fulfill data or a typed abort. No callback, regular expression, host
delay, filesystem read, or later route mutation is admitted.

`fixtures_only` is the reproducible mode. A miss becomes sticky
`network_fixture_miss` and never falls through to ambient network. `mixed` and
`live` are observable but explicitly nondeterministic; neither may claim
cross-run digest equality.

A matched abort is observable without live fallback: `route_decided` records
`fixture_abort`, followed by `request_failed` for the same request ID and one
of the fixed allow-listed reasons. The page-facing fetch/XHR rejects while the
session may still reach quiescence after its handler completes.

Three additional request-admission failures are part of the public profile.
The 513th active operation fails
`controlled_network_active_operation_limit_exceeded` with bounded `limit` and
`observed` details; an indeterminate request-body length fails
`unsupported_network_request_body_length` before reading the body or falling
back to a route; rejected unbounded or unredactable metadata fails
`unsupported_network_request_metadata` before retention. All are sticky with
`stateEffect: partial` after request admission. During initial open they are
fatal and fail-stop; after publication they are nonfatal protocol errors whose
sticky network terminal remains authoritative for settlement.

The table admits at most 256 routes, 32-byte methods, 4 KiB URL patterns, 64
response headers and 16 KiB combined header bytes per route, 256 KiB response
bodies, 320 KiB aggregate decoded fixture data, a 384 KiB canonical encoded
route table, and 512 active network operations. Individual field maxima remain
subordinate to the encoded-table cap. The route table, optional 512 KiB state
artifact, URL, clock, and envelope must together fit the frozen 1 MiB request frame.
Credentialed or fragmented matcher URLs, CR/LF header values, hop-by-hop and
`Content-Length` response headers, and request-secret response headers such as
`Authorization` or `Cookie` are rejected during immutable validation.

## Public requests and diagnostic evidence

`session.requests({ afterSeq?, limit })` exposes a bounded request projection.
`session.evidence({ afterSeq?, limit })` exposes the shared navigation/network
schema-2 event stream. Both use one checked, monotonic, session-local sequence allocator
and retention watermark, so a filtered requests page may contain sequence gaps.
Each page reports `firstRetainedSeq`, `nextAfterSeq`, `latestSeq`, `complete`,
`hasMore`, and `droppedThroughSeq`. `afterSeq` is exclusive. `complete` is false
only when requested history has been evicted; `hasMore` represents ordinary
pagination.

The diagnostic event types include request start, route decision, response
headers, redirect, request completion/failure, navigation start,
navigation commit/failure, same-document history change, and settlement terminal. Each carries
exact virtual time plus only its explicit request, redirect, or navigation IDs. Unknown
relationships are omitted instead of receiving a fabricated parent ID. The ledger is for
explanation, never settlement authority.

Application navigation evidence is emitted from the final stable authority
observation. If application script performs multiple same-document changes
before Stasis regains that boundary, one record reports the resulting
`navigationId`/`historyRevision` delta; Stasis does not invent intermediate
events it did not observe. Consumers must therefore treat the ledger as a
bounded terminal-observation journal, not a complete browser event trace.

`session.requests` owns the redacted URL projection: origin, path, and sorted
unique query-key names. Evidence events, including `request_started`,
cross-reference its request ID rather than duplicating the projection.
Admission caps every observed request method at 32 bytes, then caps the
borrowed raw query at 16 KiB and 64 non-empty components
before retaining at most 64 sorted unique keys of at most 128 bytes each. It
also rejects more than 64 raw header names before lowercasing and deduplication,
so repeated names cannot create unbounded preprocessing work.
Same-document-history and settlement-terminal evidence require the current
`navigationId`. Neither API retains credentials, fragments, query values,
header values, request/response bodies, cookie/auth values, or the session-state
token. Default retention is 1,024 records, 1 MiB serialized metadata, and 256
items per page; hard caps are 4,096 records, 8 MiB, and 1,024 page items.

Settlement evidence remains a separate schema-2 terminal snapshot. It carries
the selected profile, document token binding, at most 32 allow-listed blocker
items, and exact time/state/DOM/outcome/limit metadata. It excludes URLs and all
session secrets. The event ledger does not turn this terminal snapshot into
replay or a complete causal journal.

## Process pool and reference crawler

The SDK pool is a bounded FIFO lease manager over owned Stasis processes. It
requires finite positive process and finite non-negative queue limits, rejects
overflow before enqueue, and honors cancellation while a job waits. One lease
owns one fresh process and its one session. Healthy release performs terminal
`session.close`, observes process exit, and discards the process; the next lease
spawns a replacement. Poisoned processes are terminated and discarded. The
pool never retries a mutation or replays a callback after a written command.

The reference crawler is deliberately smaller than a crawler platform. It uses
first-class `resolved_url` extraction for `href`, accepts only HTTP(S), removes
fragments for canonical deduplication, and rechecks redirect final URLs against
its origin policy. Same-origin is the default; crossing origins requires an
explicit HTTP(S) allowlist. Every crawl requires finite depth, page, and
concurrency limits, concurrency cannot exceed the process limit, and the URL
queue cannot exceed the page limit. Each active page owns a fresh process and
session; optional state is imported before its first request. It neither sleeps
for progress nor retries a written navigation.

Robots policy, authentication strategy, proxy pools, distributed work, cloud
orchestration, stealth, and anti-bot behavior are application concerns, not
Stasis 0.2 engine claims.

## Deferred surface

The following do not block the session profile and must not be implied by its
name: iframes, workers/worklets, multiple tabs, dialogs, permissions,
geolocation, downloads, upload, WebSocket/SSE settlement, Service Workers,
cross-event-loop document control, sibling selector combinators,
pseudo-classes/elements, history traversal, layout-backed click, hit testing,
screenshots, a complete causal journal, replay, time travel, a persistent
daemon, distributed crawling, or Playwright API compatibility. Work outside the
declared subset is rejected before start or terminates with an honest typed
unsupported/open-ended result; it is never silently called quiescent.

## Release proof

The 0.2 release gate must run both contracts from the packed public package.
The unchanged v0.1 North Star proves legacy open, generation authority, and
terminal close.

The packed-package session North Star launches a fresh process with strict immutable routes,
imports bounded state before its first request, opens and settles a form page,
uses focus/fill/check/select/submit, observes a same-document history change,
extracts a resolved link, navigates, rejects the prior token, settles and
inspects the replacement, reads redacted requests/evidence, exports state, and
closes to process exit. It additionally proves cross-origin replacement with
document-token invalidation and cookie/local/session-storage isolation, typed
unsupported history traversal that cannot quiesce, and a fixture abort whose
bounded decision/failure evidence shares one request ID without reaching the
live server. A second fresh process imports that export and proves
the handoff before its first request. Native boundary tests prove the configured 1,000
document-transition, 20-redirect, and 10,000-history limits, including the final admitted value
and first rejected value, without exhausting or wrapping their checked counters. The packed
artifact story exercises representative below-limit navigation rather than claiming an endurance
run to every numeric boundary.

A bounded pool then crawls a local multi-page fixture repeatedly without
sleeps. Cross-run comparison excludes opaque token bytes but validates every
within-run token transition, request/evidence sequence, and strict fixture
decision.
