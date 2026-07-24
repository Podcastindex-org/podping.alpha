# Checkpoint: issue #61 / PR #85

Date: 2026-06-26
Branch: `fix/gossip-listener-mainline-memory`
Commit: `8a33f47 fix(dtt): bound mutable record collection`
PR: https://github.com/Podcastindex-org/podping.alpha/pull/85
Issue: https://github.com/Podcastindex-org/podping.alpha/issues/61

## Current State

- PR #85 is open and mergeable.
- Issue #61 has a status comment with the root-cause summary and soak validation:
  https://github.com/Podcastindex-org/podping.alpha/issues/61#issuecomment-4785748348
- No further work is pending on this branch unless PR review feedback arrives.

## Fix Summary

`dtt-fork/src/dht.rs` now bounds `mainline` mutable-record stream collection before allocating a `Vec`.

Before the fix, DTT used `collect::<Vec<_>>()` on the full `mainline` stream and only applied its existing `MAX_BOOTSTRAP_RECORDS = 100` cap later during filtering/publishing logic. That left the raw DHT query path able to grow memory before local bounds took effect.

The patch adds `collect_bounded(...)` using `stream.take(limit)` and sets the limit to `crate::MAX_BOOTSTRAP_RECORDS`.

## Validation Completed

- `cargo check --manifest-path gossip-listener/Cargo.toml` on `test-stax`: passed.
- `cargo test --manifest-path dtt-fork/Cargo.toml --lib --tests` on `test-stax`: passed.
- `cargo check --manifest-path dtt-fork/Cargo.toml --lib` on `test-stax`: passed.
- 60-minute debug soak on `test-stax` with `ARCHIVE_ENABLED=1 CATCHUP_ENABLED=1`: completed; RSS plateaued around 87-89 MB.
- 12-hour release soak on `test-stax` with `ARCHIVE_ENABLED=1 CATCHUP_ENABLED=1`: completed; RSS plateaued around 80-84 MB, no watchdog stalls, no panics.

12-hour soak details:

- Run dir: `/tmp/podping-soak-release-12h-20260622T221843Z`
- Samples: 720
- RSS first: 30 MB
- RSS max: 84.7 MB
- RSS final: 82.3 MB
- Archive messages: 16,814
- Catch-up stored: 9,178
- Live/new stored: 7,636
- Watchdog lines: 0
- Panic lines: 0
- `mainline::rpc` bootstrap errors: 1,458

## Known Limitation

Full `cargo test --manifest-path dtt-fork/Cargo.toml` is blocked by an unrelated existing example compile error in `dtt-fork/examples/secret_rotation.rs`, where `Endpoint::builder()` is missing the required iroh 0.97 preset argument.

That example fix is intentionally out of scope for PR #85.

## Resume Guidance

If resuming this thread:

1. Check PR #85 status and review comments.
2. Keep any follow-up changes scoped to bounded DHT collection unless the maintainer explicitly asks otherwise.
3. If the separate `secret_rotation` example fix has landed, rerun full `cargo test --manifest-path dtt-fork/Cargo.toml`.
4. Do not archive/close issue #61 unless the maintainer accepts the fix or explicitly asks.
