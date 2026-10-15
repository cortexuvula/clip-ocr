# ClipOCR — Design Spec

- **Status:** REV 3 — Argus + Repo Auditor findings folded (2026-10-15), ready for implementation
- **Date:** 2026-10-15
- **Author:** Frikkie (orchestrator), with FerriScribe lineage

## Problem Statement

On-screen text that cannot be selected (video frames, PDFs without text layers, images,
remote desktops, locked documents) is a daily friction point. Existing solutions are
either paid SaaS (TextSniper, CleanShot X) or embedded inside a large app (FerriScribe's
screenshot-OCR feature). Andre wants a **standalone, cross-platform, menu-bar utility**
that does exactly one thing well: press a hotkey → drag a rectangle → the OCR'd text is
on the clipboard. Mac is the priority platform; Linux and Windows must work from the
same codebase. All OCR must be **on-device** — no network, no accounts.

## Solution

A Tauri 2 + Rust desktop app, single menu-bar/tray presence, one global hotkey.

Flow (mirrors FerriScribe v0.75, which is battle-tested):

1. **Trigger** — global hotkey (`CmdOrCtrl+Alt+O` default, configurable), tray menu
   "Capture", or CLI flag (`--capture-ocr`) delegating via single-instance.
2. **Region select** — platform-native selection UI (see Capture Matrix).
3. **OCR** — on-device only, behind a pluggable trait (see OCR Engines).
4. **Clipboard** — text written from Rust side (`tauri-plugin-clipboard-manager` /
   arboard), never pixels. Typed outcomes: `copied` / `cancelled` / `empty` /
   `permission_denied` / `low_confidence` / `in_progress` / `failed` (single-flight
   guard, RAII release on panic — FerriScribe pattern). **A clipboard write failure is
   a failure outcome, never a reported `copied`.**
5. **Feedback** — best-effort completion notification (`osascript` / `notify-send` /
   PowerShell toast; Windows toast shows generic identity without an AppUserModelID —
   documented best-effort). Always-on-top progress pill on macOS while the engine runs
   (proven FerriScribe code — keep). On `in_progress` from a headless trigger (hotkey/
   CLI), fire the notification fallback ("capture already running") — FerriScribe
   swallowed this for its webview; a tray-only app must surface it.

**Non-goals:** no translation, no cloud OCR, no file saving, no OCR history, no accounts,
no analytics. Screenshot saving/history may follow later — not in v1.

## User Stories

1. macOS: hotkey → drag over a paused video frame → subtitle text on clipboard in <1s,
   OCR'd locally by Apple Vision.
2. Linux Wayland (wlroots — Sway/Hyprland/Omarchy): hotkey (compositor-bound) or tray →
   slurp drag → grim captures exactly that region.
3. Windows: hotkey → drag on a locked PDF render → text on clipboard via
   Windows.Media.Ocr.
4. Power user: `clipocr --capture-ocr` from a script lands text on the clipboard even on
   cold start (single-instance delegation; first instance self-executes the capture once
   tray + plugins are ready — never in raw argv parsing, to avoid racing init). Exit
   status means **text actually copied** (see CLI Contract).
5. Re-trigger mid-capture → clear "already running" feedback (notification on headless
   triggers), never stacked overlays.
6. Secondary scaled monitor (2x) drag captures and OCRs the right pixels (scale-factor
   geometry; on Retina macOS `screencapture` emits physical pixels natively — story 6 is
   really about the overlay platforms). Cross-monitor drags are composited or visibly
   constrained, never silently truncated (see Capture Matrix).
7. **First-run permission flow (macOS):** before any capture, preflight Screen Recording
   with `CGPreflightScreenCaptureAccess()`; on denial the app shows a typed
   `permission_denied` outcome with a System Settings deep link
   (`x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture`),
   and the tray shows a warning state. **Critical:** without permission, `screencapture`
   succeeds but returns only the desktop wallpaper — a silent, wrong result. This flow
   prevents that. Tested from the PACKAGED app on a clean account (first grant, denial,
   re-grant, cold-start CLI delegation) — permission may attribute to a different
   executable or require an app restart after grant.
8. Launch at login (tauri-plugin-autostart) — v1 setting; a menu-bar utility that
   doesn't offer it feels broken.
9. OCR language setting: macOS enumerates the runtime-supported Vision language list and
   gates the setting to it; Linux ships eng traineddata with docs for more.

## Capture Matrix (port from FerriScribe `screen_capture.rs` — don't redesign)

| Platform | Mechanism | Notes |
|---|---|---|
| macOS | `screencapture -i -x <file>` | 0700 private dir + 0600 pre-created file + read-then-unlink BEFORE OCR; drop FerriScribe's shred (non-PHI) but keep the hygiene and ordering. Preflight TCC per story 7. |
| Linux Wayland (wlroots only) | `slurp` (geometry) + `grim -g <geom> -` (PNG on stdout) | wlr-screencopy. GNOME/KDE Wayland do NOT serve grim → explicit unsupported-state message (tray warning + notification), or XDG Screenshot portal fallback via ashpd (decision at implementation; document). Never claim general Wayland support beyond wlroots. |
| Linux X11 / Windows | transparent overlay + `xcap` | **One overlay window PER MONITOR**, each with its own scale factor and CSS→physical mapping — a single union overlay mis-maps drags on mixed-DPI setups. |

Port VERBATIM (Argus #11 — proven, unit-tested): `parse_slurp_geometry` + rejects,
`clamp_rect` (i64 edge math), `is_png`, `monitor_union`, `capture_screen_region_png`
alpha-force, deadlines (600s capture / 120s overlay), `OVERLAY_HIDE_GRACE` (named
constant; comment that slow compositors may need it raised), `InFlightGuard`,
`indicator_position`, typed outcomes.

**Fixes to the lineage on port (Repo Auditor F1/F2/F3/F4/F9 — do NOT copy these flaws):**

- **F1 child reaping:** a timed-out macOS capture must TERMINATE and reap the
  `screencapture` child before shredding/unlinking its file (dropping a Tokio handle
  does not kill the child). Cleanup on EVERY exit path — timeout, spawn failure, cancel,
  OCR failure. Verify: leave the picker open past a short test deadline; assert no child
  and no later-created PNG remains.
- **F2 private dir assurance:** validate an existing capture dir's ownership AND mode on
  every boot; refuse/migrate an unsafe pre-existing path; clean up the capture file
  despite read or OCR failure. "Shred" = best-effort deletion, not a guarantee of
  physical erasure on modern storage — say so.
- **F3/F4 cross-monitor overlay:** per-monitor overlays each map their OWN scale factor
  (fixes mixed-DPI mis-mapping). A drag crossing monitors must be EITHER composited from
  the intersected per-monitor captures OR visibly constrained to one monitor — the
  lineage's "capture the monitor at the selection's center, clamp to it" silently loses
  the other monitor's portion and is forbidden. Verify a rect spanning two screens in
  both drag directions, unequal-DPI monitors, negative origin.
- **F9 bounded input + engine lifetime:** the 8-byte PNG signature check is necessary but
  not sufficient — enforce bounded capture bytes/dimensions and treat decode failure as
  a capture failure BEFORE OCR. A single-flight guard cannot clear a genuinely stuck
  blocking engine merely by timing out its caller: give the OCR/helper call a bounded
  lifetime; on timeout the outcome is `failed(timeout)`, the guard releases, and the
  abandoned engine thread is a documented accepted resource cost (never allow a second
  live capture to overlap). Verify truncated PNGs, oversized dimensions, hung fake
  engine.

Known limitations (one line each in README): xcap/overlay cannot draw above elevated
(admin) Windows apps (UIPI); DRM-protected video captures black on every platform.

## OCR Engines (trait `OcrEngine { fn recognize(&self, png: &[u8]) -> Result<OcrResult> }`)

- **OcrResult carries `(text, confidence: Option<f64>)`** — confidence is OPTIONAL per
  engine. Windows.Media.Ocr exposes no confidence value; inventing one would corrupt
  cross-platform rejection (Repo Auditor F6). Engines without confidence report `None`
  and can never be `low_confidence`; engines with it use their own calibrated threshold
  constant (Vision per-candidate 0–1 vs Tesseract mean confidence are NOT comparable —
  Argus #4). `empty` / `low_confidence` are typed outcomes.
- **macOS (default):** Apple Vision IN-PROCESS via Rust `objc2-vision` /
  `objc2-foundation` bindings. `VNImageRequestHandler.performRequests` is SYNCHRONOUS —
  callable from `spawn_blocking`, read `request.results()` after. **A Swift helper
  binary is a second signed artifact** (its own cert, Contents/MacOS placement,
  notarization, per-capture spawn) — only a documented fallback if the binding fights
  the build. Either way, PNG via in-memory/stdin — no IPC file.
- **Linux (default):** Tesseract via `leptess` with bundled `eng.traineddata`; decide
  system-leptonica vs static build NOW (AppImage/bundled vs documented apt dependency)
  and set `TESSDATA_PREFIX` to the bundled dir.
- **Windows (default):** WinRT `Windows.Media.Ocr` via `windows` crate; detect
  missing-language-pack errors distinctly from OCR failure; documented fallback chain
  WinRT → Tesseract (bundled DLLs + traineddata — call out the distribution cost).
- **Offline boundary (Repo Auditor F7):** engine implementations are an ALLOWLIST
  (macVision, Tesseract, WinRT only) — no configurable provider stack, no remote
  "engine override" (do NOT port FerriScribe's provider resolution into the OCR call;
  it is not proof of offline operation). Helpers and language assets load from
  app-local paths only. Verify capture→clipboard end-to-end with network unavailable,
  and that a configured remote engine is rejected.

## Implementation Decisions (rev 3 — Argus + Repo Auditor folds)

1. **Tauri 2 + Rust core** — confirmed by both reviews: macOS quality comes from the OCR
   engine and capture mechanism, not the framework. Native Swift would abandon
   one-codebase; Electron/Flutter buy nothing. Plugins: `global-shortcut`,
   `single-instance`, `clipboard-manager`, `autostart`.
2. **Signing IS in v1 scope** (was wrongly out): stable dev signing identity + notarized
   release build. Ad-hoc rebuilds change cdhash every build → TCC re-prompts on EVERY
   dev build; Gatekeeper blocks unsigned downloads. **Not sandboxed, direct distribution
   only, no App Store** (sandbox forbids this capture model).
3. **Config:** raw TOML via the `toml` crate in the app data dir (confy pulls deprecated
   serde_yaml). Keys: hotkey string, engine override, language, line-break-collapse
   toggle, launch-at-login.
4. **Hotkey lifecycle (FerriScribe pattern, verbatim):** validate at save, idempotent
   re-register, warn-and-degrade to tray + documented compositor binding to
   `clipocr --capture-ocr`. global-shortcut is X11-only on Linux — Wayland `register()`
   fails by design. **Surface hotkey registration failure IN THE TRAY MENU** (inactive
   indicator + tooltip), not only in logs — a displayed-but-dead shortcut must never be
   silent (Repo Auditor improvement).
5. **Clipboard caveats:** arboard on Wayland needs ext-data-control (fine on
   Hyprland/KDE; GNOME only ≥48) — a failed write can look like success, so the smoke
   matrix does write-then-read per target compositor; `wl-copy` fallback acceptable. X11
   clipboard dies with the owning app — ClipOCR is resident so OK. Document that Linux
   clipboard managers (cliphist etc.) will retain OCR'd text by design.
6. **Tray menu:** Capture, separator, hotkey display (with inactive indicator),
   engine/language display, Open Settings (hotkey/language/engine/launch-at-login),
   About/version, Quit.
7. **Privacy (honest line):** pixels never touch the clipboard — text only. But macOS
   Universal Clipboard and Windows cloud clipboard DO sync text off-device when the user
   enables them; text inherits OS clipboard behavior by design. **Logging boundary:**
   never log recognized text, helper stdout, or screenshot-containing errors — logs and
   notifications carry statuses/counts only.
8. **CLI contract (Repo Auditor F10):** `--capture-ocr` exit status means **"text
   actually copied"**, not "request accepted": exit 0 = `copied`; nonzero typed codes
   for `cancelled`, `empty`, `permission_denied`, `in_progress`, `failed`. The second
   instance delegates to the resident instance and returns the terminal outcome; define
   how a script learns of in-progress/empty/clipboard-failure (nonzero + distinct codes;
   a `--wait` flag may block for the outcome). Accept ONLY `--capture-ocr` at the
   single-instance boundary — nothing else. Verify simultaneous CLI launches, cold
   start, empty OCR, clipboard failure.

## Testing Decisions

- **Unit:** port FerriScribe's pure geometry helpers (slurp geometry parse, clamp_rect,
  indicator_position, PNG sniff, in-flight guard) + hotkey resolve/validate, outcome
  serialization, OCR trait contract with a fake engine.
- **Unit/headless E2E (Repo Auditor improvement):** injectable `CaptureDevice` +
  `Clipboard` trait boundaries (alongside `OcrEngine`) so capture orchestration and
  clipboard effects are testable WITHOUT a real desktop; geometry transformations stay
  pure. `--capture-ocr-selftest` is an explicit LOCAL, INTERACTIVE smoke test (needs a
  display, permissions, a visible synthetic image, clipboard ownership) — NOT portable
  CI coverage; keep synthetic-only fixtures and separate headless unit tests. Dev-only
  marker — never ships a window in a tray-only app.
- **Manual smoke matrix** (macOS priority, then Omarchy Wayland, then Windows): video
  frame drag, secondary monitor at 2x, **cross-monitor drag both directions**, Esc-cancel,
  empty region, concurrent trigger, cold-start CLI, **first-run permission
  deny/grant/re-grant (packaged app, clean account)**, clipboard write-then-read per
  compositor, "selection UI not in the shot" check, **engine timeout**.
- **Verification before review:** `cargo test` green; `cargo tauri build` on macOS;
  `git status` clean.

## Out of Scope

- Cloud/API OCR, translation, screenshot saving/history, FerriScribe integration,
  auto-update.
- GNOME/KDE Wayland (v1) unless the ashpd portal fallback lands at implementation.

## Further Notes

- **Build rules (deferred AGENTS.md — pending Andre approval):** Rust 1.8x+, `cargo tauri
  build`; `cargo test` must pass before review; OCR engines stay behind the trait and
  the ALLOWLIST (macVision/Tesseract/WinRT); never log pixel data, recognized text, or
  helper stdout; screenshots never touch the clipboard — text only.
- FerriScribe lineage files to mine: `screen_capture.rs`, `screenshot_ocr.rs` (hotkey,
  events, notification, single-flight), overlay Svelte view. Apply the F1/F2/F3/F4/F9
  fixes on port — the lineage's versions of those behaviors are the flaws.
- Review trail: Argus design review 2026-10-15 (21 findings; critical = TCC
  silent-blank) — folded rev 2. Repo Auditor spec audit 2026-10-15 (10 findings: F1–F7
  major, F8–F10 minor; top criticals: none at spec stage; ready for macOS-first spike
  once privacy lifecycle + engine contract specified) — folded rev 3. Both report-only;
  repo verified untouched by each.
