# Fork Merge Blockers

Things this fork branch (`fork-update-system` / future `perforce-integration`) MUST resolve before merging back to its upstream-of-record or shipping to broader audiences. Loud, top-level so nobody forgets.

Each blocker has a stable id (so commits can reference it), a short reason, and a pointer to the deep context in `docs/agent-review-state.md`.

---

## OPEN

### BLOCKER-1 — Two upstream `client::telemetry` tests `#[ignore]`-marked

**Where:** `crates/client/src/telemetry.rs`
- `test_telemetry_flush_on_max_queue_size` (around line 728)
- `test_telemetry_flush_on_flush_interval` (around line 805)

**Why ignored:** Both fail in this fork because `assets/settings/default.json` ships `telemetry.metrics=false` per spec §7.2; `init_test(cx)` inherits that default; `report_event()` returns early at `telemetry.rs:520-522`; the queue never receives the single event the tests assert on.

**Verified pre-existing** on baseline `ddf4ff8259` via `git stash` snapshot-replace round-trip during Phase F review.

**Fix path** (when revisited):
1. Edit `init_test(cx)` (or just these two test bodies) to explicitly enable `telemetry.metrics=true` before constructing `Telemetry`.
2. Re-run `cargo test -p client --lib telemetry`. Confirm both pass + all other 10 still pass + 2 Fork-channel tests (`test_fork_channel_short_circuits_event_flush`, `test_release_channel_is_fork`) still pass.
3. Remove the `#[ignore]` attribute + the `FORK FIXME` comment block.
4. Update `docs/agent-review-state.md`:
   - §3 DD-F-pre-existing-telemetry-tests → FIXED
   - §4 landmine entry pruned (no longer a trap)
5. Update this file: move BLOCKER-1 to the bottom under "## RESOLVED" with the resolution commit SHA.

**Process gate:** CLAUDE.md Rule 2 forbids editing existing tests without explicit human approval. **Approval already given for the temporary `#[ignore]`** (user direction during Phase F orchestration). A second explicit approval is required before un-ignoring + editing the test bodies / `init_test`.

**Tracked since:** Phase F finished review.
**Ignored in commit:** `f550fc8361 fork: ignore 2 pre-existing telemetry tests pending plan completion`

---

## RESOLVED

(none yet)
