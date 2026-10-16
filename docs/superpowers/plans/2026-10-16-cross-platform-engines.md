# ClipOCR — Windows + Linux Engine Implementation Plan

## Goal
Replace the macOS-only stubs with real capture + OCR engines for Windows (room-6) and Linux (omarchy-laptop), then smoke-test on both machines.

## Current State (verified by reading source)
- `capture.rs`: `#[cfg(not(target_os = "macos"))]` returns `Err(CaptureError::Failed(Failure::Unsupported))`. Only macOS `screencapture -i -x` path exists.
- `engine.rs`: only `MacVision` (Apple Vision via objc2-vision) implemented. `Tesseract` and `WinRt` enum variants exist but `create()` returns `Err(Failure::Unsupported)` for them.
- `config.rs`: `validate()` hard-requires `engine == EngineId::MacVision && cfg!(macos)`. Default engine is `MacVision`.
- `desktop.rs`: hardcodes `EngineId::MacVision` in pipeline construction; menu shows "Engine: macVision"; permission item shows "Capture unsupported on this platform" on non-macOS.
- `geometry.rs`: already has `parse_slurp_geometry`, `capture_screen_region_png`, `OverlaySelector` trait + `UnsupportedOverlay` — the Linux slurp geometry parser and image-crop helper are READY.
- `storage.rs`: `assure_private_dir` on non-unix is a stub (no ACL enforcement) — comment says "MUST be implemented before enabling any Windows capture device".
- `ipc.rs`: unix-only (Unix socket). Windows has no CLI path (`#[cfg(not(unix))]` returns Unsupported).
- `core.rs`: `EngineId` enum has `MacVision`, `Tesseract`, `WinRt`. `Failure` has `Unsupported`. `PERMISSION_LINK` is macOS-only.

## Design Decisions
1. **Windows capture**: WinRT `GraphicsCapturePicker` (system interactive picker, like macOS `screencapture -i`). This is the closest analog — user drags a region, system returns a `GraphicsCaptureItem`. Requires the `windows` crate (WinRT bindings) + `windows-capture` crate (higher-level wrapper). Alternative: `mss` (screenshot library) but that's full-screen only, no region picker. **Use `windows-capture`** for the picker + frame capture.
2. **Windows OCR**: Windows.Media.Ocr (WinRT) via `windows` crate — `OcrEngine::try_create_from_language`. This is the native on-device engine, matches the `WinRt` enum variant.
3. **Linux capture**: `grim` + `slurp` (wlroots/wayland). `slurp` provides the interactive region picker (prints `x,y WxH`), `grim -g "x,y WxH"` captures that region to PNG. The `parse_slurp_geometry` helper already exists. For X11 fallback: `import` (ImageMagick) or `scrot`. **omarchy-laptop is Hyprland (wlroots)** so grim+slurp is correct.
4. **Linux OCR**: Tesseract via `tesseract` crate (Rust bindings to libtesseract). Matches the `Tesseract` enum variant. Requires `tesseract` system package + `eng.traineddata`.
5. **Config**: default engine becomes platform-appropriate (`WinRt` on Windows, `Tesseract` on Linux, `MacVision` on macOS). `validate()` checks the engine matches the platform.
6. **Storage**: Windows needs real private-dir enforcement (ACL) before enabling capture — implement a Windows ACL check or reuse the existing dirs-based path with a documented limitation.
7. **CLI (ipc.rs)**: Windows needs a named-pipe or TCP loopback client for `--capture-ocr`. Linux already has the unix socket path (works).

## Task Breakdown (TDD, each 2-5 min)
1. **Windows capture** — `capture.rs` `#[cfg(target_os = "windows")]`: WinRT GraphicsCapturePicker → region → PNG bytes. Tests: mock picker returns a known region.
2. **Windows OCR** — `engine.rs` `#[cfg(target_os = "windows")]`: Windows.Media.Ocr. Tests: synthetic image → known text.
3. **Linux capture** — `capture.rs` `#[cfg(target_os = "linux")]`: slurp → grim → PNG. Tests: parse slurp geometry (already exists), mock grim output.
4. **Linux OCR** — `engine.rs` `#[cfg(target_os = "linux")]`: Tesseract. Tests: synthetic image → known text.
5. **Config platform-default** — `config.rs`: default engine per-platform, validate() per-platform.
6. **Desktop wiring** — `desktop.rs`: pipeline uses platform engine, menu shows correct engine, permission item per-platform.
7. **Windows CLI** — `ipc.rs` `#[cfg(target_os = "windows")]`: named-pipe client.
8. **Windows storage** — `storage.rs`: Windows private-dir enforcement.
9. **CI** — add Windows/Linux test coverage (already in ci.yml, but ensure new code compiles on all 3).

## Verification
- `cargo test` on all 3 platforms (CI).
- Smoke test on room-6 (Windows): install .exe, run `--capture-ocr`, verify clipboard.
- Smoke test on omarchy-laptop (Linux): install AppImage, run `--capture-ocr`, verify clipboard.
- `cargo clippy` + `cargo fmt --check` clean.

## Risks
- WinRT GraphicsCapturePicker requires the app to be a packaged/foreground app with a window handle; may need `SetWindowLong`/foreground window tricks. This is the hardest part.
- Tesseract traineddata must be present on the Linux box (install `tesseract-ocr` + `tesseract-ocr-eng`).
- grim/slurp must be installed on omarchy-laptop.
- Windows named-pipe CLI is new code (unix socket already works).
