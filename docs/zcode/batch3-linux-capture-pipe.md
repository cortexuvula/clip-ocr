# Batch 3/4 — Linux capture via stdin/stdout pipe + sweep on all Unix

Read the full spec first: docs/zcode/fix-security-audit-2026-11-08.md (Hard facts §3, Design decisions §3, Deliverable 3, gates).

Work in src-tauri/src/capture.rs and src-tauri/src/desktop.rs (+ tests). MacOS and Windows capture paths MUST stay unchanged.

Implement:
- Linux `PlatformCapture::capture` (capture.rs:108-124): run `grim -g <canonical-geometry> -t png -` with stdout piped; read bounded (MAX_BYTES + 1), deadline-bounded (CAPTURE_DEADLINE), kill+reap on timeout like today's ChildGuard; `validate_png` (core.rs:223-249) on the bytes BEFORE they reach the engine. NO file is created on Linux — no capture-tmp write, no `capture_with_command` call on the Linux path. Keep slurp geometry parsing/re-serialization as-is.
- desktop.rs:591-596: change the sweep gate from `cfg(target_os = "macos")` to `cfg(any(target_os = "macos", target_os = "linux"))` so Linux also sweeps stale pre-fix remnants (storage::sweep semantics unchanged: only `capture-*` regular files).

Tests (red-before-green): synthetic grim wrapper (a test command writing PNG bytes to stdout) — capture succeeds, capture-tmp has zero entries; oversized/hung stdout fails with typed Capture failure and child reaped; `validate_png` rejection blocks OCR. Prove the new tests fail against the unmodified baseline.

Gates: cargo fmt --check, clippy -D warnings, full cargo test. Leave changes uncommitted.
