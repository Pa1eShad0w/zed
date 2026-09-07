# Fork Merge Blockers

Items this fork must resolve before upstream submission or broader distribution. Releases are made from `main`.

Keep the reason, resolution commit and verification evidence together here. The former `docs/agent-review-state.md` is no longer present in this repository.

---

## OPEN

None currently recorded.

## RESOLVED

### Telemetry queue and timer tests restored — 2026-09-07

**Where:** `crates/client/src/telemetry.rs`
- `test_telemetry_flush_on_max_queue_size`
- `test_telemetry_flush_on_flush_interval`

**Reason:** Both tests inherited the fork's intentional `telemetry.metrics=false` default. `report_event()` returned before queueing an event, so the tests could not exercise their intended opt-in queue/timer behavior.

**Resolution commit:** `d97d8321cf5b7faa0786827a4af2ec49bb495b82`.

**Change:** With explicit user approval, enable metrics only inside each of the two test fixtures before constructing `Telemetry`; remove their ignore attributes and obsolete comments. Preserve every original assertion, shared initialization, production defaults and fork upload isolation.

**Verification:** Rust 1.97.1 on Windows; `cargo nextest run --locked -p client --lib telemetry` passed all 13 matching tests, including both restored tests, `test_fork_channel_short_circuits_event_flush` and `test_release_channel_is_fork`. The other 19 client tests were excluded by the name filter, not counted as passes. `script/clippy --locked -p client` passed with release/all-targets/all-features and deny-warnings settings; rustfmt and diff checks passed. Optional cargo-shear/typos/buf checks were not run because the tools were unavailable.

**Compatibility review:** Production telemetry code and all original assertions are unchanged. Metrics and diagnostics remain disabled by default; the fork transport guard still drains queued events without an upstream HTTP request.

**History:** The failures were pre-existing on `ddf4ff8259`; temporary ignores were introduced in `f550fc8361`. This record closes that release blocker; it does not assert that testing proves the absence of every possible release issue.
