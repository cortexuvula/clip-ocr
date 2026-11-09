# Batch 8/8 — production fix: NamedPipeServer must connect() before reads

Build on the uncommitted batches 1-7 in this worktree. THIS IS A PRODUCTION CHANGE (the one remaining part of the transport fix). All prior batches stay untouched.

## Verified root cause (Windows host evidence + pinned tokio source)

`real_pipe_round_trip_dispatches_without_client_eof` now runs cleanly (no reactor panic, no process farm, 30s watchdog) but the real exchange still returns `Failed`. Evidence chain:

1. tokio 1.53.3 `named_pipe.rs:891` — `impl AsyncRead for NamedPipeServer` calls `self.io.poll_read(cx, buf)` DIRECTLY. There is no implicit connect.
2. `NamedPipeServer::connect()` (named_pipe.rs:192) is the explicit operation that completes the client-connection handshake; mio's windows NamedPipe returns WouldBlock for reads on a server that has not gone through connect.
3. Production `serve_blocking` (src-tauri/src/ipc.rs `mod named`) does `ServerOptions::new().first_pipe_instance(true).create(&name)` and immediately calls `respond_std(server, ...)` → `respond_bounded` → `stream.read_exact(...)` — the read can never deliver data because nobody called connect. The read then hits the 2s `REQUEST_DEADLINE` and replies `Failed(Unavailable)` for EVERY connection. This is the pre-existing "transport never supported the legitimate flow" breakage (client-side: shutdown()=flush, fixed in batch 5; server-side: missing connect, THIS batch).

## Fix (src-tauri/src/ipc.rs, `mod named` only)

In `serve_blocking`'s loop, between `create` and `respond_std`:

- `let connected = tokio::time::timeout(REQUEST_DEADLINE, server.connect()).await;` — the connect completes as soon as a client connects (the loop is designed to park waiting for the next CLI invocation).
- Map the result consistently with everything else: on `Ok(Ok(()))` → proceed to `respond_std(server, handler)`; on timeout or error → reply nothing (there is no client to reply to on a failed handshake) and REBIND (continue the loop), same fail-closed discipline as every other branch. Do NOT leak the pipe: drop it by scoping.
- Keep `first_pipe_instance(true)` and the existing rebind/response flow.

Nothing else changes: `respond_bounded` (exact-size read, extra-byte probe, panic-parity spawn, bounded reply), `named::client` (write, flush-only shutdown, read_to_end), and the deterministic round-trip test (double-probe + 2.5s settle + 30s watchdog) all stay as-is — the test already waits for a parked instance and should now pass end-to-end.

## Regression logic

The Windows round-trip test (`real_pipe_round_trip_dispatches_without_client_eof`) is red on the Windows host today (client got `Failed` at ipc.rs:743) and must go green with this change — that is the acceptance pin. The production behavior it proves is exactly the CLI contract: `clipocr --capture-ocr` on Windows connects, dispatches, and returns the typed outcome.

## Gates

- `cd src-tauri && cargo fmt --all -- --check`
- `cd src-tauri && cargo clippy --all-targets -- -D warnings`
- `cd src-tauri && cargo test` (macOS full suite stays green; the Windows-gated test compiles off-Windows? It is `#[cfg(all(test, windows))]` — not run here; macOS must show no collateral damage)
- Dispatcher runs: single round-trip test on the Windows host (must pass), then the full Windows suite (no LNK1104, no leftover processes).

## Report

File:line of the change, the connect mapping (timeout/error handling), gate results, and any deviation with source citation. Leave uncommitted.
