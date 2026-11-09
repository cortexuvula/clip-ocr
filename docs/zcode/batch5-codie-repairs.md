# Batch 5/5 — Codie review repairs (ship-blocker + confirmed warnings)

Read the full spec first: docs/zcode/fix-security-audit-2026-11-08.md. Prior batches (1-4) are implemented and uncommitted in this worktree — build on them, never revert them. This batch fixes the reviewer's CONFIRMED findings in the existing code. Leave everything uncommitted.

Verified ground truth: in the pinned tokio 1.53.3 (Cargo.lock), `NamedPipeClient::poll_shutdown` calls `poll_flush` (src/net/windows/named_pipe.rs:1711) — client shutdown() delivers NO EOF and no half-close. On the real Windows transport, EOF only arrives when a handle is dropped.

## 1. CRITICAL — ipc.rs: Windows responder must not depend on EOF (ipc.rs:177-198)

The legitimate CLI flow (named::client → write REQUEST → shutdown → wait reply; ipc.rs:284-290) never sends EOF. `respond_bounded`'s `take(15).read_to_end` therefore always runs to the 2s deadline and returns `Failed(Unavailable)` for every REAL Windows capture; the duplex contract tests pass because tokio::io::duplex shutdown IS a real half-close (a capability the transport lacks).

Fix: replace the EOF-dependent read with a fixed-size read of exactly `REQUEST.len()` bytes that completes as soon as those bytes arrive (byte-mode pipe): e.g. `stream.read_exact(&mut buf[..REQUEST.len()])` under the existing `REQUEST_DEADLINE` timeout. Then:
- byte-compare against REQUEST; dispatch only on exact equality; mismatch → `Failed(Unavailable)` (as today).
- After an exact match, reject an oversized stream without buffering it: attempt ONE more read of a single byte with a SHORT bounded probe (e.g. `tokio::time::timeout(Duration::from_millis(100), read_exact 1 byte)`) — if a 15th byte arrives, respond `Failed(Unavailable)` and do NOT dispatch. This keeps the exact-match discipline without EOF and without unbounded buffering. (Legit clients never send a 15th byte; the 100ms probe is invisible to them and makes `REQUEST + junk` fail closed.)
- Keep: 2s read deadline (withheld-partial case), bounded reply write+flush with its own deadline, single JSON reply, drop-and-rebind.
- The duplex contract tests must keep passing and remain platform-agnostic.

Regression test (Codie's pin): add a `#[cfg(windows)]` test driving the REAL pair — start `named::serve_blocking` on a background thread against a `CountingHandler`-style handler, run `named::client(root)` against the same root, assert `Status::Copied` and the handler was invoked. This test FAILS today (red pin) and passes after the fix. It will run on the Windows verification host, not on macOS.

## 2. Warning — ipc.rs:185: panic-parity with the Unix sibling

Unix `respond` wraps `handler.capture()` in `tokio::spawn` and maps a JoinError to `Failed(Engine)` (ipc.rs:71-74). `respond_bounded` awaits the handler bare, so a handler panic unwinds through `serve_blocking`'s `block_on` and silently kills the `clipocr-ipc` thread until app restart. Mirror the Unix shape: spawn the handler, map join failure to `Outcome::failed(Failure::Engine)`, reply that outcome, keep the loop alive.

Regression test: `respond_bounded` with a panicking handler → reply is `Failed`/`Engine` AND the responder returns normally (loop can serve the next request). Platform-agnostic (duplex stream).

## 3. Warning — capture.rs:457-462: grandchild can pin `reader.join()` forever

In `capture_via_pipe`, the timeout branch does `kill; wait; reader.join()` — if a grandchild inherited the stdout write end and outlives the killed child, the pipe never closes and `join()` blocks forever (pins the spawn_blocking thread, holds `Pipeline::in_flight` → all later captures report InProgress until restart). Fix: after kill+wait, poll `reader.is_finished()` for a short bounded window (e.g. up to ~500ms); if still not finished, DETACH (leak the thread) and return the timeout failure — the leaked reader holds only a pipe handle and bounded scratch. Do not join unconditionally.

Regression test: a shell child that backgrounds a pipe-inheriting sleeper (e.g. `/bin/sh -c 'sleep 30 & exec sleep 1'` with stdout inherited) — `capture_via_pipe` must return `Timeout` within a bounded time asserted with an 8s watchdog. Platform: unix (runs on macOS and Linux).

## 4. Warning — release.yml:98-100: the 404 catch is wider than the getRef it guards

The `try { getRef ... getTag ... } catch (e) { if (e.status !== 404) throw; }` treats a 404 from EITHER call as "tag absent". A 404 from `getTag` (pruned/missing tag object) would silently downgrade the provenance check and proceed unverified. Narrow the 404-skip to getRef only: restructure so only `git.getRef`'s 404 means "new tag"; a failure of `getTag` after a successful getRef must fail the job loudly (rethrow). Keep the annotated-tag peeling logic.

## 5. Hygiene — .mimosa residue can't ship

Add `.mimosa/` to .gitignore (repo root; pattern matches at any depth). Leave the untracked scratch dirs as-is on disk; they must simply never enter a commit.

## Verification gates (run individually; report each)

- `cd src-tauri && cargo fmt --all -- --check`
- `cd src-tauri && cargo clippy --all-targets -- -D warnings`
- `cd src-tauri && cargo test` (full suite; report the complete count)
- Red-before-green evidence for findings 1-3 (paste failing → passing outputs; for #1 the Windows round-trip test must be shown failing against the pre-fix code — note it cannot be executed on this host; run it on the Windows host, or if you can produce the red by compiling the test against the current code via a cfg(windows) check, state exactly what was and wasn't executed).
- `git status --short` + `git diff --stat`: only the 5 expected files (+ .gitignore); nothing staged; no lockfile/Cargo.toml/ui changes.

## Report

Per finding: what changed (file:line), test names, gate results, red evidence, and any deviation with source citation. State clearly which tests ran on which platform and which remain Windows-only (to be executed on the Windows verification host by the dispatcher). Leave uncommitted.
