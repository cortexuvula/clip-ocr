# ClipOCR — Design Spec

- **Status:** DRAFT — under roster consultation (Argus, Repo Auditor)
- **Date:** 2026-10-15
- **Author:** Frikkie (orchestrator), with FerriScribe lineage

## Problem Statement

On-screen text that cannot be selected (video frames, PDFs without text layers, images,
remote desktops, locked documents) is a daily friction point. Existing solutions are
either paid SaaS (TextSniper, CleanShot X) or embedded inside a large app (FerriScribe's
screenshot-OCR feature). Andre wants a **standalone, cross-platform, menu-bar utility**
that does exactly one thing well: press a hotkey → drag a rectangle → the OCR'd text is
on the clipboard. Mac is the priority platform; Linux and Windows must work from the
same codebase. All OCR must be **on-device** — no network, no accounts, no PHI-style
privacy concerns.

## Solution

A Tauri 2 + Rust desktop app, single menu-bar/tray presence, one global hotkey.

Flow (mirrors FerriScribe v0.75, which is battle-tested):

1. **Trigger** — global hotkey (`CmdOrCtrl+Alt+O` default, configurable), or tray menu
   item "Capture". Also expose a CLI flag (`--capture-ocr`) for scripted/utility use that
   delegates to the running instance (single-instance plugin).
2. **Region select** — platform-native selection UI:
   - macOS: `screencapture -i -x <file>` (the one tool with no stdout mode; capture to
     private app dir, shred before OCR returns).
   - Linux Wayland: `slurp` (geometry) + `grim -g <geom> -` (PNG on stdout, no disk).
   - Linux X11 / Windows: transparent fullscreen overlay window + `xcap` screen API.
     Overlay reports the drag rect via a Tauri command; hide overlay, wait grace, capture.
3. **OCR** — on-device only. OCR engine is a **pluggable trait** with per-platform
   default implementations:
   - **macOS (priority, default):** Apple Vision `VNRecognizeTextRequest` — fast,
     accurate, free, on-device. Ship as a small Swift helper binary invoked per capture,
     or a Rust binding. This is the flagship experience.
   - **Linux (default):** Tesseract via `leptess`/`tesseract` crate (bundled traineddata
     for en; docs for adding languages).
   - **Windows (default):** Windows.Media.Ocr (WinRT) via `windows` crate, or Tesseract
     fallback — decide at implementation with a thin abstraction.
   - All engines return (text, confidence) so the app can reject empty/low-confidence
     results.
4. **Clipboard** — write text from Rust side (`tauri-plugin-clipboard-manager`), never
   pixels. Typed outcomes: `copied` / `cancelled` / `empty` / `in_progress` (single-flight
   guard rejects concurrent triggers, RAII guard releases on panic — FerriScribe pattern).
5. **Feedback** — completion toast/notification (best-effort, `osascript`/`notify-send`/
   PowerShell pattern from FerriScribe). Optional always-on-top progress pill on macOS.

**Non-goals:** no translation, no cloud OCR, no file saving, no OCR history, no settings
surface beyond hotkey + engine + language. No accounts. No analytics.

## User Stories

1. As a user on macOS, I press the hotkey, drag over a paused video frame, and the
   subtitle text is on my clipboard in <1s, OCR'd locally by Apple Vision.
2. As a Linux user on Wayland (Omarchy), I press the hotkey (compositor-bound) or tray
   item, drag with slurp, and grim captures exactly that region to OCR.
3. As a Windows user, I press the hotkey, drag on a locked PDF render, and get text.
4. As a power user, I run `clipocr --capture-ocr` from a script and text lands on the
   clipboard even if the app was cold-starting (single-instance delegation).
5. As a user, I can re-trigger mid-capture and get a clear "already running" signal
   rather than stacked overlays.
6. As a user on a Retina display, dragging on a secondary scaled monitor captures and
   OCRs the right pixels (scale-factor handling, FerriScribe's pure/unit-tested geometry
   helpers).

## Implementation Decisions

1. **Tauri 2 + Rust core** — matches FerriScribe/TailDrop GUI stack; one codebase, three
   desktop targets. `tauri-plugin-global-shortcut`, `tauri-plugin-single-instance`,
   `tauri-plugin-clipboard-manager`.
2. **Capture matrix borrowed from FerriScribe** (`screen_capture.rs`): macOS
   `screencapture -i -x`, Wayland `slurp`+`grim`, X11/Windows overlay+`xcap` with
   monitor-union geometry, physical→logical scale mapping, PNG signature check, 600s
   capture deadline, 120s overlay deadline, 250ms hide-grace. Port the module + its unit
   tests (they're platform-agnostic pure geometry) and trim FerriScribe-specific PHI
   stringency to plain hygiene (no `/tmp` shared writes, no logging of pixels — ever).
3. **OCR via trait `OcrEngine { fn recognize(&self, png: &[u8]) -> Result<OcrResult> }`**
   with per-platform default + config override. macVision is the shipped default on
   macOS; on Linux/Windows default to Tesseract/WinRT. Thread via `spawn_blocking`.
4. **Menu bar/tray only** — no main window. Tray menu: Capture, separator, hotkey
   display, Open Settings (hotkey/language/engine), Quit. On macOS use `TrayIcon` menu
   bar item; Windows/Linux system tray with the same items.
5. **No persistent storage** beyond a small config file (`confy` or raw TOML in app data
   dir): hotkey string, engine override, language, line-break-collapse toggle.
6. **Feedback without a window**: toast/notification best-effort (reuse FerriScribe
   `spawn_notification`), plus a tiny always-on-top progress pill on macOS while the
   engine runs.

## Testing Decisions

- **Unit:** port FerriScribe's geometry helpers (slurp geometry parse, clamp_rect,
  indicator_position, PNG sniff, in-flight guard) — they're pure and already proven.
- **Unit:** hotkey resolve/validate; outcome serialization; OCR trait contract with a
  fake engine.
- **Integration:** a `--capture-ocr-selftest` path that runs the full pipeline against a
  generated on-screen test image and asserts clipboard content (machine-local; meaningful
  on macOS, best-effort elsewhere). Validate manually on the Mac first.
- **Manual smoke matrix** (macOS priority, then Omarchy Wayland, then Windows):
  drag on video frame, secondary monitor at 2x scale, Esc-cancel, empty region,
  concurrent trigger, cold-start CLI flag.

## Out of Scope

- Cloud/API OCR, translation, screenshot saving/history (menu item may later offer
  "copy image" — not now).
- FerriScribe integration (this app is standalone; do not couple).
- Auto-update, code signing, distribution beyond `cargo tauri build` artifacts.

## Further Notes

- **Build rules (deferred AGENTS.md — pending Andre approval):** Rust 1.8x+, `cargo
  tauri build`; `cargo test` must pass before review; keep OCR engines behind the trait;
  never log pixel data; screenshots never touch the clipboard — text only.
- FerriScribe lineage files to mine: `screen_capture.rs`, `screenshot_ocr.rs` (hotkey,
  events, notification, single-flight), overlay Svelte view.
- macOS screen-recording permission: app must request Screen Recording (TCC) — document
  the prompt path; `screencapture` triggers it on first use.
