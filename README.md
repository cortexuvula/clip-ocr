# ClipOCR

A standalone menu-bar utility: **CmdOrCtrl+Alt+O → drag-select → on-device OCR → text on the clipboard**. No main window, accounts, remote OCR, history, analytics, or runtime network requests.

## Status and platform scope

This is the **macOS-first 0.1 spike** of the [rev-3 design](docs/superpowers/specs/2026-10-15-clip-ocr-design.md), not a claim of finished Linux/Windows support. macOS uses the system region picker and **in-process Apple Vision** through objc2 bindings; there is no Swift OCR executable or image IPC file. macOS 12+ is the configured deployment target; this build has been exercised only on the implementation machine, not an older-OS compatibility matrix.

Linux and Windows capture/OCR/overlay devices explicitly return `failed(unsupported)` and the tray displays an unsupported-platform warning. They do not implement the flawed desktop-union/selection-center overlay from the lineage. Pure per-monitor mapping, visible-constraint geometry, union, crop/opaque-alpha encoding, and the trait boundaries are present for later implementations. Linux wlroots, X11 and WinRT/Tesseract packaging are not shipped in this cut. GNOME/KDE Wayland capture is not claimed.

## Build and run

Prerequisites: a current stable Rust toolchain, Xcode Command Line Tools, and the Tauri 2 CLI (`cargo install tauri-cli --version '^2' --locked`). The dependency lockfile is committed. Building dependencies initially needs a populated Cargo cache or a package download; the application itself does not need network access. There is no Node build step: the small Settings/progress UI is bundled HTML/CSS/JavaScript.

From the repository root:

```sh
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
cargo tauri build --bundles app
open target/release/bundle/macos/ClipOCR.app
```

For a fully cached build, add `--offline` to Cargo commands and use `CARGO_NET_OFFLINE=true cargo tauri build --bundles app`. The application bundle is `target/release/bundle/macos/ClipOCR.app`; the executable is `ClipOCR.app/Contents/MacOS/clipocr`. Installing the same signed bundle at a stable path (normally `/Applications/ClipOCR.app`) is recommended before granting permissions or enabling launch at login. Do not move it afterwards without rechecking those settings.

The tray contains Capture, hotkey status (explicit **INACTIVE** on registration failure), engine/language, Screen Recording permission, Settings, About/version, and Quit. No normal window is created at startup. Settings and the OCR progress pill are created only as needed; closing the last such window does not terminate the resident. Quit during an active capture asks you to finish/Esc-cancel first so the child can be reaped and the temporary file removed.

### Screen Recording permission

Every capture checks `CGPreflightScreenCaptureAccess()` **before** invoking `screencapture`. A denied check yields `permission_denied`, not a wallpaper-only OCR result. The tray warns until permission is granted, refreshing periodically. Click its Screen Recording item to explicitly request access and open:

`x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture`

Allow ClipOCR in System Settings → Privacy & Security → Screen & System Audio Recording (wording varies by OS). Restart the app if macOS requests it. Permission can be attributed to a different executable when running unbundled binaries from a terminal; use the packaged application for acceptance testing. Denial/grant/re-grant on a clean account is a required **human** release check, not covered by the headless suite.

### Settings

Settings are raw TOML at `~/Library/Application Support/ca.clipocr.desktop/config.toml` (the application data directory), not confy. Save validates the shortcut, shipped engine, and the runtime-enumerated Vision language list. A malformed configuration blocks capture until repaired in Settings, rather than silently accepting a remote provider. A shortcut conflict is nonfatal: Capture remains available in the tray, and the menu/tooltip/settings visibly report an inactive shortcut. Saving unregisters and re-registers idempotently. Launch-at-login uses `tauri-plugin-autostart` and should be enabled only from the installed bundle.

Default keys:

```toml
hotkey = "CmdOrCtrl+Alt+O"
engine = "macVision"
language = "en-US"
collapse_line_breaks = false
launch_at_login = false
```

The engine type is an offline allowlist (`macVision`, `Tesseract`, `WinRT`); only `macVision` is implemented and selectable in this release. Unknown engine/provider fields and remote overrides are rejected, not routed through a configurable provider stack.

## CLI: terminal outcome, not request acceptance

```sh
"/Applications/ClipOCR.app/Contents/MacOS/clipocr" --capture-ocr
```

`--capture-ocr` waits by default; no `--wait` flag is necessary. It prints one JSON object containing status, character count, optional typed failure reason, and a permission Settings URL when applicable. It **never prints the recognized text**. Exit status is:

| Code | Terminal result |
| --- | --- |
| 0 | `copied`: text was actually written to the clipboard |
| 2 | `cancelled` |
| 3 | `empty` |
| 4 | `permission_denied` |
| 5 | `in_progress` |
| 6 | `failed` (including `reason: "clipboard"`, `"timeout"`, `"invalid_png"`, `"unavailable"`, `"unsupported"`) |
| 7 | `low_confidence` |

A failed clipboard write **can never report copied**. Empty/low-confidence/cancelled runs leave the clipboard unchanged. Vision uses mean top-candidate confidence with its own 0.5 threshold. Engines without confidence return `None` and can never yield `low_confidence`.

A private owner-only Unix-domain socket supplements Tauri's single-instance plugin, whose fire-and-forget callback cannot return a terminal result. The resident accepts **only the exact `--capture-ocr` request**, not arbitrary arguments/settings/paths. Each requester gets its own terminal reply. On cold start the CLI serializes launchers, starts the resident without capture arguments, waits for plugins/tray/config to finish setup, then submits exactly one request. Concurrent triggers return `in_progress`, not queued or stacked pickers. Lost responses are failed; a sent request is never silently retried. Startup readiness is bounded to 20 seconds and a request to 640 seconds. Transport failure is nonzero, never an accepted-as-copied success.

No arguments starts the tray resident. `--help` documents the contract. Other flags are rejected. Future Wayland users can bind `clipocr --capture-ocr` from their compositor when global shortcuts are unavailable, but actual Wayland capture is **not yet shipped**.

## Privacy, storage, bounds, and lifetime

- Pixels never touch the clipboard; only text is copied. macOS Universal Clipboard and Windows cloud clipboard can sync that text if the user enables them. Linux clipboard managers such as cliphist retain copied text by design; ClipOCR does not override OS clipboard behavior.
- `screencapture -i -x -t png` writes into an owner-only **0700** capture directory under application data, with a pre-created **0600** file. Existing directory ownership/mode/type are validated on every boot and capture; unsafe existing directories, symlinks or other-owner paths are refused, not silently chmodded or trusted. Stop the app and inspect/move an unsafe pre-existing path before retrying; do not blindly repair a path owned by someone else.
- A child guard terminates **and reaps** `screencapture` on its 600-second deadline or unwind **before** file cleanup. Spawn/cancel/read/error paths all have cleanup guards. The file is read with a byte cap and unlinked **before OCR**; unlink failure prevents OCR. Stale regular capture files are swept only after the resident instance lock is acquired. Unexpected symlinks are refused. Deletion is **best-effort unlink, not guaranteed physical erasure** on APFS/SSDs. Forced process termination/power loss cannot guarantee destructor cleanup.
- Before Vision, PNG signature, maximum **32 MiB** encoded bytes, maximum **16,384** pixels per dimension, maximum **32 million** pixels, decoder allocation limits and full decode validity are enforced. Signature-only/truncated/oversized PNGs cannot reach OCR.
- Request single-flight uses an RAII guard, including panic release. Vision executes synchronously on `spawn_blocking`, with a 30-second caller deadline. Rust cannot safely kill a stuck native thread: on timeout the request is `failed(timeout)` and its request guard releases, but a **separate worker lease** rejects all new captures until that worker exits. At most one abandoned OCR worker (and its bounded image) can remain; this is the documented accepted resource cost. A genuinely stuck worker requires app restart. No second live capture/engine is overlapped to disguise a timeout.
- Framework errors/helper stderr/stdout, screenshot bytes, and recognized text are never formatted into logs, notifications or CLI replies. Only fixed statuses, safe configuration errors and counts are surfaced. There is no OCR history or screenshot export.

## Stable signing and notarization (release checklist)

Direct distribution only: **not App Sandbox / not Mac App Store**. This capture model is not compatible with App Sandbox. Keep the bundle identifier `ca.clipocr.desktop`, install location, and signing team/identity stable. Ad-hoc unsigned rebuilds can change the designated code requirement/cdhash and cause TCC re-prompts; use a stable Apple Development signing identity for local packaged builds and a Developer ID Application identity for public releases. Merely setting `LSUIElement` does not satisfy signing, TCC or Gatekeeper.

Release signing must use a legitimate certificate/private key already installed by its owner. Do not put certificates, passwords, API keys or signing credentials in this repository. For example (substitute an identity, not a credential):

```sh
APPLE_SIGNING_IDENTITY="Developer ID Application: Your Name (TEAMID)" \
  cargo tauri build --bundles app
codesign --verify --deep --strict --verbose=2 target/release/bundle/macos/ClipOCR.app
```

Notarization is a **separate owner-operated release step** using a pre-provisioned owner-managed keychain profile. These commands are instructions only; require an Accepted submission before stapling:

```sh
APP="target/release/bundle/macos/ClipOCR.app"
ZIP="${TMPDIR:?}/ClipOCR-notary.zip"
ditto -c -k --keepParent "$APP" "$ZIP"
xcrun notarytool submit "$ZIP" --keychain-profile CLIPOCR_NOTARY --wait
# Continue ONLY after status Accepted:
xcrun stapler staple "$APP"
xcrun stapler validate "$APP"
spctl --assess --type execute --verbose=2 "$APP"
```

Also test the downloaded/quarantined distributed artifact on a clean account. Tauri can notarize when explicitly supplied owner-managed signing/notarization environment variables; omit those variables for local builds. **No notarization submission is part of this implementation session.** Signing alone is not a Gatekeeper/notarization pass.

## Verification

`cargo test` exercises the ported geometry/PNG/hotkey helpers; typed outcomes/exit codes; optional confidence; fake capture, engine and clipboard orchestration; panic/timeout leases; unsafe directory rejection; a real child killed/reaped before a delayed screenshot write; exact-request IPC; cold-start launch serialization; and real local sockets with simultaneous clients returning copied/empty/clipboard-failure/in-progress outcomes. macOS tests also invoke real Vision in memory against synthetic blank and bitmap-text PNGs, and enumerate actual supported languages. Tests do not read the desktop or current clipboard and do not need network access.

A non-capturing packaged smoke test is also available (quit any resident first):

```sh
python3 scripts/smoke_resident.py target/release/bundle/macos/ClipOCR.app
```

It checks release CLI argument rejection, resident/plugin readiness, private storage/socket setup, invalid IPC rejection, and duplicate-instance handling, then stops only its own child. It never requests a screen capture or reads the clipboard. Synthetic in-memory Vision tests were additionally run with outbound networking denied using macOS `sandbox-exec`; this is not a substitute for the human disconnected-desktop capture check.

For the development-only, local **interactive** smoke window (not a portable CI test), first quit any resident:

```sh
cargo run -- --capture-ocr-selftest
```

Select only the displayed synthetic sample. Confirm status and clipboard contents, then close the sample and confirm the tray remains resident. Release builds reject this flag and never create the sample window.

Human acceptance checklist, **not claimed as automated coverage**:

- From the installed packaged app on a clean account: deny, grant, restart if needed, revoke, re-grant Screen Recording; confirm tray warning and CLI exit 4 on denial.
- Hotkey/tray/CLI capture of synthetic text and a video frame; Escape cancellation; empty region; clipboard write-then-read; line-break collapse and alternate runtime-supported language.
- Secondary Retina monitor; negative-origin screen arrangement; cross-monitor drag in both directions; verify no picker/progress UI appears in captured pixels.
- Concurrent hotkey/tray/CLI; cold-start CLI; real clipboard failure; hotkey conflict visibly INACTIVE; save/restart persistence; login launch from installed location; close Settings/progress and verify residency.
- Network disconnected capture-to-clipboard; release Gatekeeper/notarization; supported older macOS versions.

Known platform limitations: DRM-protected video can capture black on every platform. Future Windows xcap/overlay capture cannot draw above elevated/admin applications (UIPI). Notifications are best-effort; statuses remain available through the CLI/tray. Linux/Windows desktop integration and compositor clipboard ownership need their own future smoke matrix.

## Source layout and lineage

- `core.rs`: injectable `CaptureDevice`, `OcrEngine`, `Clipboard`, typed outcomes, bounded orchestration and RAII leases.
- `capture.rs` / `storage.rs`: isolated macOS picker/TCC/child lifecycle/private-file handling; explicit unsupported capture elsewhere.
- `engine.rs`: in-process Vision only, offline allowlist dispatch and runtime languages.
- `geometry.rs`: FerriScribe pure parsers/clamp/PNG/indicator ports and regression tests, hardened monitor union, per-monitor constraint mapping, alpha-force PNG crop, future overlay trait.
- `config.rs` / `ipc.rs`: raw TOML validation/atomic save, private CLI terminal request/reply protocol.
- `desktop.rs` / `ui/`: Tauri tray/plugins, text-only clipboard adapter, on-demand Settings/progress and dev-only interactive selftest.

Born from FerriScribe's screenshot-OCR feature. The original repository was used read-only; its unsafe timeout/directory/signature-only/provider/overlay behaviors were not carried over. No files in FerriScribe are modified.
