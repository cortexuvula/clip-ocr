# Batch 10/10 — correction: include UnexpectedEof in the client-gone skip set

Build on the uncommitted batches 1-9 in this worktree. Correct batch 9's deviation; do not revert anything else.

## Why batch 9's exclusion is wrong (evidence from the Windows verification host)

The scratch-probe output on room-6 (pinned tokio 1.53.3) shows the disconnect read on the REAL pipe transport is reported as `read -> Ok(Err(Custom { kind: UnexpectedEof, error: "early eof" }))` — tokio's `read_exact` wraps an underlying 0-byte read into `ErrorKind::UnexpectedEof`. The batch-9 claim "the pipe transport never yields an EOF at all" is factually wrong for this stack: mio surfaces the post-disconnect read as EOF, not as BrokenPipe. Consequently the current `client_gone` set (BrokenPipe | ConnectionAborted | ConnectionReset) does NOT catch the canonical connect-and-drop probe, the reply write is still attempted into the dead instance, and the following `first_pipe_instance(true)` create fails with ERROR_ACCESS_DENIED — the round-trip test still fails at its second probe, exactly as before batch 9.

## Required changes (src-tauri/src/ipc.rs)

1. **Add `std::io::ErrorKind::UnexpectedEof` to the `client_gone` match set** in `respond_bounded`. Keep the other three. Update the comment to cite the probe evidence (read_exact wraps the post-disconnect 0-byte read as UnexpectedEof on the Windows pipe transport).
2. **Align the duplex rejection tests with the corrected contract.** With UnexpectedEof in the skip set, a client that half-closes (duplex shutdown = real EOF) before sending a complete valid REQUEST now gets NO reply — the server treats EOF as client-gone (which is correct: on the production transport EOF only means the client is gone, since shutdown() there is a flush, not a half-close). Update `respond_bounded_tests`:
   - The short-payload rejection fixtures (`b""`, `b"--quit\n"`, `b"--capture-ocr-selftest\n"`, truncated REQUEST) that currently half-close and assert a `Failed` JSON reply must now assert: **zero reply bytes** (the server returned without writing) AND the handler was NOT called. Keep them fast (no timeout waits).
   - The valid-REQUEST-with-half-close dispatch test (`exact_request_dispatches_and_replies_with_each_terminal_status`) is unaffected: a FULL valid REQUEST before half-close still dispatches and replies (read_exact completes on the 14 bytes first). Verify it still passes.
   - The no-half-close oversize test and the withheld-EOF (deadline) test are unaffected (their reads end via the byte cap / REQUEST_DEADLINE, not EOF).
3. Fix the now-stale probe comment in the Windows round-trip test (ipc.rs ~line 713) to reflect that the dropped-handle probe drives the client-gone skip path, not "the bounded reply phase".

## Regression logic

The Windows round-trip test is the pin: its second probe (connect-and-drop) must produce `UnexpectedEof` → skip write → clean rebind → test passes, then `named::client` dispatches `Status::Copied`. The duplex suite must stay green on macOS after the assertion changes.

## Gates

- `cd src-tauri && cargo fmt --all -- --check`
- `cd src-tauri && cargo clippy --all-targets -- -D warnings`
- `cd src-tauri && cargo test` (macOS — full suite green with the updated duplex assertions)
- Dispatcher runs on the Windows host: single `ipc::named_pipe_tests` run (pin goes green), then the full Windows suite.

## Report

File:line of the skip-set and test changes, duplex assertion updates and why they encode the corrected contract, gate results, any deviation with source citation. Leave uncommitted.
