# Stasis WebRender provenance

This directory vendors the normalized crates.io contents of `webrender` 0.70.0.

- Original crates.io checksum: `ede9dbc3cfb3e2c073a68e20f8fd54d9f6be8587be1c7c948e1ac51112421ef6`
- Stasis delta: retain and join WebRender-owned backend and scene thread handles so a synchronous shutdown acknowledgement cannot be mistaken for physical thread termination.

The override is intentionally limited to the `webrender` crate. `webrender_api` and `wr_malloc_size_of` remain registry dependencies at their locked versions.
