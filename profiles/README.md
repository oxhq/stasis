# Stasis support profiles

Support profiles name the exact browser subset for which Stasis can make controlled-settlement
claims. A profile is a versioned product contract, not a request to emulate every browser feature.

`controlled-webapp-v1.json` is the profile shipped by Stasis 0.1. It is frozen byte-for-byte and
remains the default selected by the legacy `Runtime.open()` API. Stasis 0.2 development must prove
that this profile's request shapes, result shapes, close behavior, and typed unsupported outcomes
have not changed.

`controlled-web-session-v1.json` is the stable contract shipped by Stasis 0.2. It is an explicitly
selected, additive profile for one terminal session per owned process, checked
top-level and same-document navigation, document-scoped state tokens, separately versioned
session state, practical bounded selectors and forms, declarative network fixtures, bounded
request/evidence streams, a fresh-process pool, and a reference crawler. Its
`stable_contract` status and frozen release digest prohibit silent changes; incompatible changes
require a newly named profile.

Each JSON file is canonical release source: automation, inspection, execution, network, state,
evidence, and unsupported-surface claims must be changed there before a profile can be advertised
by the native runtime. Incompatible changes require a new profile identifier; a stable release must
not silently broaden or narrow an existing profile. A new profile must be complete rather than
relying on implicit inheritance from an older profile.

For controlled sessions, the protocol request and response both carry the selected profile. Work
outside that profile must either be rejected before it starts or terminate settlement with a typed
unsupported/open-ended result. It must never be treated as quiescent merely because Stasis cannot
observe it.

Both profiles deliberately exclude live-network reproducibility. Release proofs use local or
intercepted fixtures whose inputs are controlled by the test.

Document state and session state are separate authorities in the session profile. A document
`stateToken` binds the current top-level document target and complete runtime generation. A
`sessionStateToken` binds cookies and Web Storage, which survive document replacement. Neither can
substitute for the other, and exported session state is sensitive data that must not enter bounded
diagnostic evidence.
