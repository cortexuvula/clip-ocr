# ClipOCR

A standalone menu-bar utility: **CmdOrCtrl+Alt+X → drag-select → on-device OCR → text on the clipboard**. No main window, accounts, remote OCR, history, or analytics. Recognized text never leaves the device: the ONLY network request ClipOCR ever makes is the optional update check (see [Updates](#updates)), which contacts github.com and GitHub's release-asset hosts.

**Website and downloads: <https://clipocr.ca>** — platform packages, privacy behaviour and the exact caveats for each OS. The [latest release](https://github.com/cortexuvula/clip-ocr/releases/latest) holds the installers.

## Status and platform scope

Cross-platform single codebase (Tauri 2 + Rust). Engines are all on-device and never call the network (the optional update check is not part of OCR and is off by default):

| Platform | Capture | OCR engine |
|----------|---------|------------|
| macOS | system region picker (`screencapture`) | Apple Vision, in-process via objc2 (no Swift helper, no image IPC file) |
| Linux (wlroots/Wayland) | `slurp` + `grim` | Tesseract (`tesseract-rs`) |
| Windows | crosshair drag-to-select region overlay (Snipping Tool style, per-monitor) | `Windows.Media.Ocr` (WinRT) |

macOS 12+ is the deployment target. Linux has been exercised on Hyprland/wlroots (Omarchy) and Windows on Windows 11.

**Wayland hotkeys:** the global-shortcut backend is X11-only (`global-hotkey` 0.8), and X11 key grabs do not receive events while a native Wayland client has focus. On a Wayland session, bind the command in your compositor instead — the app reports the shortcut as a compositor bind in the tray. See [Linux (Wayland)](#linux-wayland).

## Updates

ClipOCR checks for updates only when YOU ask it to (tray → **Check for updates…**), or — only if you enable it in Settings — automatically at most once a day. It is a CHECK, not an updater: nothing is downloaded, installed, or restarted. When a newer version exists, ClipOCR shows it and, on your click, opens the GitHub release page in your browser.

- Endpoint: `https://github.com/cortexuvula/clip-ocr/releases/latest/download/latest.json` (fixed; redirects are followed only within github.com and its release-asset CDN hosts, over HTTPS).
- The request carries nothing but the GET — no version, platform, or install identifier. GitHub sees your IP address; that is unavoidable.
- Automatic checks: opt-in in Settings (**Check automatically (daily)**), default OFF, at most once per 24h, never during a capture, with 6h/12h/24h backoff after failures. Turn it off at any time in Settings.
- Every failure (offline, proxy/TLS, 404, rate limit, malformed manifest) is reported as a typed status — never as "up to date" — and never affects OCR.
- The tray menu shows an `Updates:` status row; the action says `Check for updates…` and becomes `Update ClipOCR…` when a newer version is known (it opens the release page — ClipOCR never installs anything).

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

The tray contains Capture, hotkey status (explicit **INACTIVE** on registration failure, or **compositor bind** on Wayland), engine/language, capture backend, Settings, About/version, and Quit. No normal window is created at startup. Settings and the OCR progress pill are created only as needed; closing the last such window does not terminate the resident. Quit during an active capture asks you to finish/Esc-cancel first so the child can be reaped and the temporary file removed.

### Linux (Wayland)

Dependencies: `tesseract` with the language data you need (e.g. `tesseract-data-eng`), plus `grim` and `slurp` for wlroots compositors (Hyprland, Sway, River).

ClipOCR finds the tessdata directory itself and passes it to the OCR engine, because the `libtesseract` shipped in the bundle carries the path compiled into the build host's package (Debian's `/usr/share/tesseract-ocr/5/tessdata`) rather than the path your distribution uses. Resolution order: `TESSDATA_PREFIX` (either the tessdata directory or its parent), `/usr/share/tessdata`, `/usr/share/tesseract-ocr/{5,4.00}/tessdata`, `/usr/local/share/tessdata`, then any `/usr/share/tesseract-ocr/*/tessdata` found at runtime. If nothing matches, the engine falls back to the library's own resolution.

The resident is started from `~/.config/autostart/clipocr.desktop`. Include `DISPLAY=:0` in `Exec=` so the X11-backed shortcut registration has a display, and keep the WebKit workarounds on NVIDIA/wlroots:

```ini
Exec=env DISPLAY=:0 WEBKIT_DISABLE_DMABUF_RENDERER=1 WEBKIT_DISABLE_COMPOSITING_MODE=1 /home/USER/.local/bin/clipocr
```

**Bind the hotkey in the compositor** — the in-app shortcut cannot fire on Wayland. Hyprland (`~/.config/hypr/bindings.lua` with Omarchy; use `bind = CTRL ALT, X, exec, …` in plain `hyprland.conf`):

```lua
o.bind("CTRL + ALT + X", "ClipOCR capture", "/home/USER/.local/bin/clipocr --capture-ocr")
```

Then `hyprctl reload` and verify the binding appears in `hyprctl binds`. Sway: `bindsym Ctrl+Alt+x exec clipocr --capture-ocr`. GNOME/KDE Wayland are not claimed (no region-picker path wired).

### Windows

Capture is a native Win32 overlay (Snipping Tool style): every monitor is dimmed ~47%, a crosshair cursor (`IDC_CROSS`) marks a left-drag rectangle with a live `W × H` pixel readout, and the mouse-up region — clamped to the monitor where the drag began — is captured via Windows Graphics Capture and OCR'd with `Windows.Media.Ocr`. Esc, right-click, or a click without a drag cancels (exit 2); the selection is bounded to 120 s. The drag cannot span monitors by design: the selection is constrained to its origin monitor (mixed-DPI safe — coordinates are physical pixels and the mapping runs through the unit-tested geometry seam). Limitation: the overlay cannot draw above an elevated/admin application (UIPI); Windows blocks that for every non-admin overlay, and ClipOCR does not try to defeat it. The only runtime prerequisite is the WebView2 runtime that Tauri already needs. Launch at login is registry-based (`HKCU\Software\Microsoft\Windows\CurrentVersion\Run`) and starts the resident directly, so the resident detaches its own console at startup. Without that, Windows would leave a console/Windows Terminal window open for the whole session because the binary is console-subsystem.

The binary stays console-subsystem on purpose: `--capture-ocr` waits and returns its exit code to the calling shell. A console inherited from such a caller is never detached — only a console this process owns (the login-autostart launch) is. A brief flash of a terminal window at login is possible, since Windows creates the console before the process runs.

### Screen Recording permission

Every capture checks `CGPreflightScreenCaptureAccess()` **before** invoking `screencapture`. A denied check yields `permission_denied`, not a wallpaper-only OCR result. The tray warns until permission is granted, refreshing periodically. Click its Screen Recording item to explicitly request access and open:

`x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture`

Allow ClipOCR in System Settings → Privacy & Security → Screen & System Audio Recording (wording varies by OS). Restart the app if macOS requests it. Permission can be attributed to a different executable when running unbundled binaries from a terminal; use the packaged application for acceptance testing. Denial/grant/re-grant on a clean account is a required **human** release check, not covered by the headless suite.

### Settings

Settings are raw TOML in the platform application data directory — `~/Library/Application Support/ca.clipocr.desktop/config.toml` (macOS) or `~/.local/share/ca.clipocr.desktop/config.toml` (Linux) — not confy. Save validates the shortcut, the engine shipped on this platform, and the runtime-enumerated language list (Vision languages on macOS, installed Tesseract languages on Linux). A malformed configuration blocks capture until repaired in Settings, rather than silently accepting a remote provider. A shortcut conflict is nonfatal: Capture remains available in the tray, and the menu/tooltip/settings visibly report an inactive shortcut. Saving unregisters and re-registers idempotently. Launch-at-login uses `tauri-plugin-autostart` and should be enabled only from the installed bundle.

Default keys:

```toml
hotkey = "CmdOrCtrl+Alt+X"
engine = "macVision"   # Tesseract on Linux, WinRT on Windows
language = "en-US"     # "eng" on Linux
collapse_line_breaks = false
launch_at_login = false
```

The engine type is an offline allowlist (`macVision`, `Tesseract`, `WinRT`); only the engine shipped on the current platform is selectable (Apple Vision on macOS, Tesseract on Linux, WinRT on Windows). Unknown engine/provider fields and remote overrides are rejected, not routed through a configurable provider stack.

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

No arguments starts the tray resident. `--help` documents the contract. Other flags are rejected. On a Wayland session, bind `clipocr --capture-ocr` from your compositor, because the in-app global shortcut cannot receive events there; the capture itself runs through `slurp` and `grim`.

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

Known platform limitations: DRM-protected video can capture black on every platform. The Windows region overlay cannot draw above elevated/admin applications (UIPI) — over an admin window the dimming/selection is not visible (the capture itself still works; cancel and select on a non-elevated area instead). Notifications are best-effort; statuses remain available through the CLI/tray. Linux/Windows desktop integration and compositor clipboard ownership need their own future smoke matrix.

## Source layout and lineage

- `core.rs`: injectable `CaptureDevice`, `OcrEngine`, `Clipboard`, typed outcomes, bounded orchestration and RAII leases.
- `capture.rs` / `storage.rs`: isolated macOS picker/TCC/child lifecycle/private-file handling, the Linux `slurp`+`grim` selection-and-capture path, the Windows overlay capture path, and explicit unsupported capture elsewhere.
- `engine.rs` / `tessdata.rs`: on-device engines only — Apple Vision (macOS, in-process), Tesseract (Linux), `Windows.Media.Ocr` (Windows) — offline allowlist dispatch, runtime language enumeration and Linux tessdata resolution.
- `geometry.rs`: FerriScribe pure parsers/clamp/PNG/indicator ports and regression tests, hardened monitor union, per-monitor constraint mapping, alpha-force PNG crop, future overlay trait.
- `config.rs` / `ipc.rs`: raw TOML validation/atomic save, private CLI terminal request/reply protocol.
- `desktop.rs` / `ui/`: Tauri tray/plugins, text-only clipboard adapter, on-demand Settings/progress and dev-only interactive selftest.

Born from FerriScribe's screenshot-OCR feature. The original repository was used read-only; its unsafe timeout/directory/signature-only/provider/overlay behaviors were not carried over. No files in FerriScribe are modified.
