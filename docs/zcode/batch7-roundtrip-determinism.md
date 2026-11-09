# Batch 7/7 — make the Windows round-trip test deterministic (test-only)

Build on the uncommitted batches 1-6 in this worktree. TEST-ONLY repair of `ipc::named_pipe_tests::real_pipe_round_trip_dispatches_without_client_eof` (src-tauri/src/ipc.rs). Do NOT touch production code (named::serve_blocking, named::client, respond_bounded stay as-is).

## Verified root cause (Windows host evidence)

The test's `wait_until_listening` connects a probe client and drops it, then sleeps 250ms before calling the REAL `named::client`. The server's reply-phase write after the probe is bounded by `REQUEST_DEADLINE` (2s) — a write/flush to the dropped probe connection can take up to 2s before `serve_blocking` rebinds a fresh instance. When the real client opens at `attempts == 0` before the rebind, `named::client` takes its COLD-START branch and spawns the current executable — in a test that is the test binary itself, which runs the whole suite again, hits the same test, and recursively spawns more copies (observed: 6+ stuck `clipocr-5aa15f324662382b.exe` processes on the Windows host; the suite hung for 37+ minutes and link steps failed with LNK1104 because the farm held the test exe).

## Fix (all inside the test module)

1. **Confirm the rebind, not just the first park:** after the first probe connects-and-drops, probe AGAIN (connect, then drop) so `wait_until_listening` only returns once the server has demonstrably rebound a fresh instance and parked on it. Loop both probes with the existing 5s deadline.
2. **Settle past the reply window:** after the probes, sleep `REQUEST_DEADLINE + Duration::from_millis(500)` (≥2.5s) before the real client call, so the server's bounded flush on the dropped probe guaranteeably finishes and the next open succeeds on `attempts == 0`.
3. **Watchdog the real client call:** run `named::client(root.path())` on a helper thread and join it with a bounded timeout (e.g. 30s). On timeout, panic with a clear message (and leave the farm-free assertion below meaningful). A hang must fail the test, never hang the suite.
4. **Assert no process farm:** after the exchange, assert `handler.called` and `Status::Copied` as today (no new production asserts).

Keep: the `#[cfg(all(test, windows))]` gating, the tempdir root, the dedicated server thread via `serve_blocking`, the CountingHandler.

## Gates

- `cd src-tauri && cargo fmt --all -- --check`
- `cd src-tauri && cargo clippy --all-targets -- -D warnings`
- `cd src-tauri && cargo test` on macOS (the test is Windows-gated; suite must stay green)
- The dispatcher runs the single test and the full suite on the Windows host — the acceptance is: single round-trip test passes in a few seconds; full suite finishes with no leftover `clipocr-*` test processes and no LNK1104.

## Report

File:line changed, how each of the four points addresses the observed root cause, gate results, and any deviation with source citation. Leave uncommitted.
