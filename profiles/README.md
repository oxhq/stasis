# Stasis support profiles

Support profiles name the exact browser subset for which Stasis can make controlled-settlement
claims. A profile is a versioned product contract, not a request to emulate every browser feature.

`controlled-webapp-v1.json` is the profile shipped by Stasis 0.1. Its JSON file is canonical release
source: automation, inspection, execution, network, and unsupported-surface claims must be changed
there before the profile can be advertised by the native runtime. Incompatible changes require a
new profile identifier; a stable release must not silently broaden or narrow an existing profile.

For controlled sessions, the protocol request and response both carry the selected profile. Work
outside that profile must either be rejected before it starts or terminate settlement with a typed
unsupported/open-ended result. It must never be treated as quiescent merely because Stasis cannot
observe it.

The profile deliberately excludes live-network reproducibility. Release proofs use local or
intercepted fixtures whose inputs are controlled by the test.
