# Batch 6/6 — fix the Windows round-trip test harness (no production change)

Build on the uncommitted batches 1-5 in this worktree. This is a TEST-ONLY repair; do not touch production code.

## Verified diagnosis (from the Windows verification host)

`ipc::named_pipe_tests::real_pipe_round_trip_dispatches_without_client_eof` fails with:

```
panicked at ...tokio-1.53.3/src/net/windows/named_pipe.rs:1005:17:
there is no reactor running, must be called from the context of a Tokio 1.x runtime
```

Line 1005 is `io: PollEvented::new(named_pipe)?` — creating ANY tokio named-pipe object requires an active runtime. The production paths are unaffected (named::client builds its own `current_thread` runtime; serve_blocking runs inside `rt.block_on`). The bug is in the test helper `wait_until_listening` (src-tauri/src/ipc.rs, `mod named_pipe_tests`): it calls `ClientOptions::new().open(...)` directly on the plain `#[test]` thread, outside any tokio runtime.

## Fix

Wrap the `ClientOptions::new().open(...)` attempt in `wait_until_listening` inside a small Tokio current-thread runtime:

- Build `tokio::runtime::Builder::new_current_thread().enable_all()` once before the loop (or per attempt — your choice, keep it simple and obvious), `block_on` the `open`, and evaluate the result as today (Ok → listening; Err → sleep 10ms and retry up to 5s, then panic "pipe server never started listening").
- Do NOT convert the test to `#[tokio::test]`: `named::client` builds its OWN runtime and `block_on`s it, which would panic ("Cannot start a runtime from within a runtime") if called from inside a tokio context. The test fn itself must stay a plain `#[test]`; only the open attempts need a runtime.
- Keep: the 250ms post-listen settle sleep, the cold-start avoidance rationale, the handler assertion, and the `Status::Copied` assertion — unchanged.

## Regression logic

This test is the pin for the batch-5 Critical: it FAILED on the Windows host pre-fix-wiring (this exact panic) and must PASS after this repair. No production code changes; the duplex responder tests and all other tests must stay green.

## Gates

- `cd src-tauri && cargo fmt --all -- --check`
- `cd src-tauri && cargo clippy --all-targets -- -D warnings`
- `cd src-tauri && cargo test` (macOS host must stay green — note this test is `#[cfg(all(test, windows))]` so it will not run here)
- The dispatcher re-runs the full Windows suite on the Windows host; your report should state that this test now compiles under `--cfg windows` and explain the runtime wrapper.

## Report

File:line changed, why the wrapper is correct (reactor requirement), confirmation that no production code moved, and any deviation with source citation. Leave uncommitted.
