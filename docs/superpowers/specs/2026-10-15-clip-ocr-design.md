# ClipOCR — Design Spec

- **Status:** REV 2 — Argus findings folded (2026-10-15); Repo Auditor audit pending fold
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
   `permission_denied` / `low_confidence` / `in_progress` (single-flight guard, RAII
   release on panic — FerriScribe pattern).
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
   tray + plugins are ready — never in raw argv parsing, to avoid racing init).
5. Re-trigger mid-capture → clear "already running" feedback (notification on headless
   triggers), never stacked overlays.
6. Secondary scaled monitor (2x) drag captures and OCRs the right pixels (scale-factor
   geometry; on Retina macOS `screencapture` emits physical pixels natively — story 6 is
   really about the overlay platforms).
7. **First-run permission flow (macOS):** before any capture, preflight Screen Recording
   with `CGPreflightScreenCaptureAccess()`; on denial the app shows a typed
   `permission_denied` outcome with a System Settings deep link
   (`x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture`),
   and the tray shows a warning state. **Critical:** without permission, `screencapture`
   succeeds but returns only the desktop wallpaper — a silent, wrong result. This flow
   prevents that.
8. Launch at login (tauri-plugin-autostart) — v1 setting; a menu-bar utility that
   doesn't offer it feels broken.
9. OCR language setting: macOS enumerates the runtime-supported Vision language list and
   gates the setting to it; Linux ships eng traineddata with docs for more.

## Capture Matrix (port from FerriScribe `screen_capture.rs` — don't redesign)

| Platform | Mechanism | Notes |
|---|---|---|
| macOS | `screencapture -i -x <file>` | 0700 private dir + 0600 pre-created file + read-then-unlink BEFORE OCR; drop FerriScribe's shred (non-PHI) but keep the hygiene and ordering. Preflight TCC per story 7. |
| Linux Wayland (wlroots only) | `slurp` (geometry) + `grim -g <geom> -` (PNG on stdout) | wlr-screencopy. GNOME/KDE Wayland do NOT serve grim → v1 non-goal, or XDG Screenshot portal fallback via ashpd (decision at implementation; document). |
| Linux X11 / Windows | transparent overlay + `xcap` | **One overlay window PER MONITOR**, each with its own scale factor and CSS→physical mapping — a single union overlay mis-maps drags on mixed-DPI setups. Alternatively explicitly document mixed-DPI as a known v1 limitation. |

Port VERBATIM (Argus #11 — proven, unit-tested): `parse_slurp_geometry` + rejects,
`clamp_rect` (i64 edge math), `is_png`, `monitor_union`, `capture_screen_region_png`
alpha-force, deadlines (600s capture / 120s overlay), `OVERLAY_HIDE_GRACE` (make it a
named constant with a comment that slow compositors may need it raised), `InFlightGuard`,
`indicator_position`, typed outcomes.

Known limitations (one line each in README): xcap/overlay cannot draw above elevated
(admin) Windows apps (UIPI); DRM-protected video captures black on every platform.

## OCR Engines (trait `OcrEngine { fn recognize(&self, png: &[u8]) -> Result<OcrResult> }`)

- **OcrResult carries (text, per-engine confidence)** — thresholds are per-engine
  constants, NOT one global cut: Vision confidence (per-candidate 0–1, calibrated per
  revision) and Tesseract mean confidence are not comparable. `empty` / `low_confidence`
  are typed outcomes.
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

## Implementation Decisions (rev 2 — Argus folds)

1. **Tauri 2 + Rust core** — confirmed by review: macOS quality comes from the OCR engine
   and capture mechanism, not the framework. Native Swift would abandon one-codebase;
   Electron/Flutter buy nothing. Plugins: `global-shortcut`, `single-instance`,
   `clipboard-manager`, `autostart`.
2. **Signing IS in v1 scope** (was wrongly out): stable dev signing identity + notarized
   release build. Ad-hoc rebuilds change cdhash every build → TCC re-prompts on EVERY dev
   build; Gatekeeper blocks unsigned downloads. **Not sandboxed, direct distribution
   only, no App Store** (sandbox forbids this capture model).
3. **Config:** raw TOML via the `toml` crate in the app data dir (confy pulls deprecated
   serde_yaml). Keys: hotkey string, engine override, language, line-break-collapse
   toggle, launch-at-login.
4. **Hotkey lifecycle (FerriScribe pattern, verbatim):** validate at save, idempotent
   re-register, warn-and-degrade to tray + documented compositor binding to
   `clipocr --capture-ocr`. global-shortcut is X11-only on Linux — Wayland `register()`
   fails by design. **Add a tray-menu indicator when the hotkey is inactive** so the
   failure is visible, not silent.
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
   enables them; text inherits OS clipboard behavior by design.

## Testing Decisions

- **Unit:** port FerriScribe's pure geometry helpers (slurp geometry parse, clamp_rect,
  indicator_position, PNG sniff, in-flight guard) + hotkey resolve/validate, outcome
  serialization, OCR trait contract with a fake engine.
- **Integration:** `--capture-ocr-selftest` (dev-only marker — never ships a window in a
  tray-only app) runs the pipeline against a generated on-screen test image and asserts
  clipboard content. Meaningful on macOS; best-effort elsewhere. Manual smoke first.
- **Manual smoke matrix** (macOS priority, then Omarchy Wayland, then Windows): video
  frame drag, secondary monitor at 2x, Esc-cancel, empty region, concurrent trigger,
  cold-start CLI, **first-run permission denial**, clipboard write-then-read on each
  target compositor, "selection UI not in the shot" check.
- **Verification before review:** `cargo test` green; `cargo tauri build` on macOS;
  `git status` clean.

## Out of Scope

- Cloud/API OCR, translation, screenshot saving/history, FerriScribe integration,
  auto-update.
- GNOME/KDE Wayland (v1) unless the ashpd portal fallback lands at implementation.
- Mixed-DPI multi-monitor on X11/Windows (v1) unless per-monitor overlays land.

## Further Notes

- **Build rules (deferred AGENTS.md — pending Andre approval):** Rust 1.8x+, `cargo tauri
  build`; `cargo test` must pass before review; OCR engines stay behind the trait; never
  log pixel data; screenshots never touch the clipboard — text only.
- FerriScribe lineage files to mine: `screen_capture.rs`, `screenshot_ocr.rs` (hotkey,
  events, notification, single-flight), overlay Svelte view.
- Review trail: Argus design review 2026-10-15 (21 findings: 1 critical — TCC
  silent-blank #8; 6 major — signing scope, per-monitor overlays, wlroots scoping,
  in-process Vision, confidence-trap, WinRT language packs). Repo Auditor spec audit
  pending.
