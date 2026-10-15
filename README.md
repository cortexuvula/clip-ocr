# ClipOCR

Standalone cross-platform menu bar / tray utility: **global hotkey → drag-select a screen region → screenshot → on-device OCR → text copied to the clipboard.**

Born from the screenshot-OCR feature proven inside FerriScribe (`src-tauri/src/screen_capture.rs`, `src-tauri/src/commands/screenshot_ocr.rs`). This project extracts that flow into a lightweight, focused utility — no medical app, no network, no account.

**Platform priority:** macOS first (menu bar status item, Apple Vision OCR), then Linux (tray, Wayland/X11), then Windows (tray).

## Design spec

The living design document lives at [docs/superpowers/specs/2026-10-15-clip-ocr-design.md](docs/superpowers/specs/2026-10-15-clip-ocr-design.md).
