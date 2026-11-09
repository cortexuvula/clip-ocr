# Batch 1/4 — Bound Retry-After and make cadence arithmetic overflow-proof

Read the full spec first: docs/zcode/fix-security-audit-2026-11-08.md (Hard facts §1, Design decisions §1, Deliverable 1, gates).

Work in src-tauri/src/update.rs (+ tests). Implement:
- Clamp the parsed 429 `Retry-After` in the transport (update.rs:582-590) to a documented max of 7 days (604800s) before `Duration::from_secs`.
- Make `Cadence::due` overflow-proof (update.rs:293-305): use checked/saturating Instant arithmetic; an unrepresentable deadline defers the check (returns false) instead of panicking.
- Keep ordinary backoff ladder behavior (6h/12h/24h) and Retry-After-overrides-above-ladder behavior; keep failure-never-up-to-date semantics; keep `run_check` clearing the override on success.

Tests (red-before-green): a mocked 429 with an extreme valid u64 Retry-After must never panic and must yield a ≤ 7-day bounded override; `due` must never panic even with that override; ordinary 429 backoff still works. Prove the new test fails against the unmodified baseline.

Gates: cargo fmt --check, clippy -D warnings, full cargo test. Leave changes uncommitted. Report per the spec's Report section, item 1.
