# Batch 9/9 — respond_bounded: don't reply into a dead pipe (validated fix)

Build on the uncommitted batches 1-8 in this worktree. Production change to `respond_bounded` (src-tauri/src/ipc.rs) only.

## Validated root cause (real-Windows probe, pinned tokio 1.53.3)

`respond_bounded` currently writes a `Failed(Unavailable)` reply for EVERY non-dispatch outcome — including when the read phase ended because the client disconnected (EOF). On Windows, writing into a just-disconnected byte-mode pipe can SUCCEED into the kernel buffer (before the disconnect fully propagates), and an instance that received such a write no longer releases cleanly: the loop's NEXT `ServerOptions::first_pipe_instance(true).create(...)` fails with ERROR_ACCESS_DENIED (os error 5) — the server thread dies, and every later CLI invocation gets "no such pipe". Reproduced deterministically in a scratch crate on the Windows host: connect→drop (no data)→read EOF→reply write→drop→next create FAILS; with the reply write SKIPPED on read-EOF, each rebind succeeds, and the full legitimate exchange (client sends REQUEST, server replies, client receives reply, rebind) completes and rebinds cleanly.

## Fix (respond_bounded only)

Only skip the reply write when the READ failed because the client is gone:

- Track the read outcome. When it is `Ok(Err(e))` with a disconnect-marker IO error — `UnexpectedEof`, `BrokenPipe`, `ConnectionAborted`, `ConnectionReset` (include all four) — return the outcome WITHOUT writing anything: there is no reader, and writing is what poisons the next instance create. Do not even enter the reply-write block.
- EVERY other path keeps writing the single JSON reply as today: read timeout (`Elapsed` — client still connected), byte-mismatch/oversize (probe saw an extra byte — client alive), handler panic → `Failed(Engine)`, and the legitimate dispatch result.
- Nothing else changes: exact-size read_exact, extra-byte probe, REQUEST_DEADLINE bounds, panic-parity spawn, bounded reply write, fail-closed outcomes. `serve_blocking`, `named::client`, tests (duplex + round-trip) untouched.

## Regression logic

The Windows round-trip test (`real_pipe_round_trip_dispatches_without_client_eof`) currently FAILS at its second probe ("pipe server never rebound a fresh instance") and must go GREEN with this change. The probe path (connect+drop without data) is exactly the read-EOF case; the real client path is the dispatch case. Both are covered by the existing test. No new test needed, but the report must state both cases.

## Gates

- `cd src-tauri && cargo fmt --all -- --check`
- `cd src-tauri && cargo clippy --all-targets -- -D warnings`
- `cd src-tauri && cargo test` (macOS full suite green; Windows-gated test not run here)
- Dispatcher runs on the Windows host: single `ipc::named_pipe_tests` run (pin test must pass in seconds), then the full Windows suite (no LNK1104, no leftover processes, no hangs).

## Report

File:line of the change, the exact read-outcome → skip-write rule, gate results, and any deviation with source citation. Leave uncommitted.
