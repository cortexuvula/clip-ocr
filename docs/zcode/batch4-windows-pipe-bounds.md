# Batch 4/4 — Bound and timeout Windows named-pipe reads

Read the full spec first: docs/zcode/fix-security-audit-2026-11-08.md (Hard facts §4, Design decisions §4, Deliverable 4, gates).

Work in src-tauri/src/ipc.rs (Windows `named` module only — `respond_std`, `serve_blocking`; DO NOT touch the Unix socket path or its tests).

Implement:
- `respond_std` (ipc.rs:194-211): read at most `REQUEST.len() + 1` bytes (take), enforce a 2s read deadline (tokio::time::timeout, matching the Unix 2s at ipc.rs:64-67), and reject any payload not byte-equal to `REQUEST` with a typed failure without dispatching. On timeout/oversize/mismatch, write the failure JSON and drop the client so the loop rebinds promptly.
- No unbounded allocation on any branch.
- Optional, only if feasible with the ALREADY-LOCKED `windows 0.62` features (Cargo.toml:37-40) and no new dependencies: an explicit owner-only security descriptor for the pipe. If infeasible, state so in your report — do not add crate features.

Tests: extend the existing exact-request test contract (the Unix-side equivalent at ipc.rs:310-353) with a Windows-compatible helper where runnable; otherwise unit-test the shared request-size/timeout decisions and document the Windows coverage gap in your report. Red-before-green where runnable.

Gates: cargo fmt --check, clippy -D warnings, full cargo test. Leave changes uncommitted.
