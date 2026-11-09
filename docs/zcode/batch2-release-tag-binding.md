# Batch 2/4 — Bind manual-release tags to the built commit

Read the full spec first: docs/zcode/fix-security-audit-2026-11-08.md (Hard facts §2, Design decisions §2, Deliverable 2, gates).

Work in .github/workflows/release.yml (create-release job script only; leave checkout/build/publish jobs and the tag-push flow intact).

Implement in the github-script block:
- `createRelease({ ..., tag_name: tag, target_commitish: context.sha })` so a new tag points exactly at the commit this run checked out (on workflow_dispatch, `github.sha` = the selected ref's commit that every job builds).
- Before creating/replacing anything, resolve an EXISTING tag for `tag` to its commit and fail the job loudly when it differs from `context.sha`. Preserve the existing draft-replacement and published-release refusal semantics (release.yml:63-84) exactly.

If you believe `context.sha` is not the commit built on dispatch, cite the official GitHub Actions `github.sha` documentation in your report before changing the approach — do not silently diverge.

No workflow can be executed here; state that limitation in your report. Static edit with clear failure paths; no tests beyond YAML sanity (yq/actionlint if available, else visual review).
