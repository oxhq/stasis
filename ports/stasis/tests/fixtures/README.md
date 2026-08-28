# Controlled MVP process fixtures

These pages are served only from the loopback integration-test server in
`controlled_mvp.rs`. They contain no readiness polling or public-network work.

- `static.html` proves that a controlled page can settle and be inspected
  without page-script evaluation.
- `timer_10s.html` proves that a ten-second application timeout advances in
  virtual time rather than sleeping for ten wall-clock seconds.
- `timer_microtask_order.html` distinguishes one callback per event-loop turn
  from incorrect timer batching.
- `raf_correlation.html` exposes the rAF, Performance, and Date surfaces for a
  single controlled rendering opportunity.
- `external_io.html` intentionally waits on the server's explicit release
  gate; no fixture timer simulates the transport delay.
- `interval.html` leaves a repeating timer ahead of a later one-shot timer so
  settlement must report the open-ended head, preserve the one-shot as deferred
  finite work, and execute neither callback.
- `controlled_v2_interval_before_finite.html` proves the v2/report-only expansion:
  two exact interval heads may run to reach a later finite one-shot, after which
  settlement reports the still-live interval without executing another cycle.
- `application_navigation.html` proves that an application-initiated top-level
  document replacement is rejected with typed unsupported-work evidence, even
  when a same-origin navigation could otherwise reuse the controlled event loop.
- `unsupported_websocket.html` proves a WebSocket is rejected before native
  dispatch and retained as typed `external_subscription` evidence.
- `xhr_mutation_observer.html` proves asynchronous XHR response delivery and a
  resulting MutationObserver checkpoint reach quiescence, while synchronous
  XHR is rejected before native dispatch without poisoning the session.
- `automation_surface.html` proves generation-bound semantic fill and activate,
  one input event per replacement, typed stale/unsupported rejections, a
  Promise-to-timer submit path, bounded query count, and ordered text/HTML
  extraction without page-script evaluation.
- `fill_profile.html` admits every text-control type named by the frozen profile
  and proves its one replacement `input` event contract.

The tests inspect DOM output only through advertised protocol methods. Missing
methods or Controlled clock support, protocol errors, shape mismatches,
timeouts, and assertion mismatches are all hard failures; development runs do
not capability-skip substantive scenarios.

`release_gate_published_binary_completes_act_settle_inspect` is the exception:
it never skips missing capabilities and only runs against the artifact named by
`STASIS_RELEASE_BINARY`. Invoke it after extracting or installing a release:

```sh
gate_log="$(mktemp)"
STASIS_RELEASE_REVISION="$(git rev-parse HEAD)" \
STASIS_RELEASE_BINARY=/absolute/path/to/stasis \
cargo test --locked -p stasis-shell --test controlled_mvp \
  release_gate_published_binary_completes_act_settle_inspect \
  -- --ignored --exact --test-threads=1 --show-output \
  2>&1 | tee "$gate_log"
grep -F 'test result: ok. 1 passed; 0 failed; 0 ignored;' "$gate_log"
```

When the publication channel provides an independent checksum for the
extracted binary (not only for its enclosing archive), prepend
`STASIS_RELEASE_SHA256=REPLACE_WITH_BINARY_SHA256` to assert it. The gate always
calculates and logs the exact binary digest. The mandatory release revision must
be the commit targeted by the published tag; run the gate from that exact
checkout or replace the command substitution with the verified tag commit. The
final check rejects libtest's otherwise successful `0 tests` result.
