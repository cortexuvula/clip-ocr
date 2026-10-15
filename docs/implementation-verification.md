# macOS-first implementation verification

## Scope

Implemented from the complete rev-3 spec, starting at `e423873` (documentation-only repository). FerriScribe's two source files were read-only references. Implementation was isolated on `implement/macos-spike` before a local fast-forward of `main`. No PR, push, notarization submission, production probe, real desktop screenshot, current clipboard read, or external OCR service was used in verification.

This is the macOS-first cut requested by the dispatcher. Linux/Windows capture, engine and overlay implementations are explicitly unsupported; their actual desktop behavior and build matrices have not been verified on this machine.

## Requirements and evidence

| Requirement / review finding | Implementation | Verification |
| --- | --- | --- |
| Typed terminal outcomes; clipboard failure never copied | `core::Outcome`, `Status`, `Failure`, `Pipeline` | Outcome/exit serialization tests; failed fake clipboard; real socket terminal replies |
| RAII single-flight and panic release | `core::InFlightGuard`, separate request and worker leases | Guard unwind, engine panic, hung engine, simultaneous request tests |
| F1 kill/reap before cleanup | `capture::ChildGuard`, `capture_with_command` | Actual fake capture process records PID, times out, is gone/reaped, cannot recreate its delayed output |
| F2 existing directory owner/mode/type assurance | `storage::assure_private_dir`, `capture_dir`, guarded unlink and sweep | Repeat validation after chmod; wrong-owner metadata; symlink/file rejection; unwind/read/spawn/cancel cleanup; packaged directory modes |
| TCC preflight and warning/deep link | `capture::ScreenPermission`, native CoreGraphics calls, `desktop::refresh_menu` | Injectable denied preflight called on every attempt with no file creation; first-run packaged permission grant/denial still human |
| In-process synchronous Vision | `engine::mac` on `spawn_blocking`; results inspected after perform | Real Vision supported-language enumeration, blank PNG and synthetic text PNG tests; no helper executable |
| F6 optional confidence | `OcrResult { text, confidence: Option<f64> }`; Vision-specific threshold | None confidence never low-confidence; low-confidence text never copied |
| F7 offline allowlist | `EngineId`, engine factory, strict TOML deserialization | Remote/unknown provider/engine rejection; actual Vision tests with outbound networking denied |
| F9 bounded PNG and engine lifetime | byte/dimension/pixel/decode/allocation validation; 30s caller deadline; worker lease survives timeout | Truncated/signature-only/oversized/valid PNG tests; hung worker releases request guard but blocks new captures until worker exits |
| F3/F4 no flawed union overlay | Native picker on macOS; other platforms fail explicitly; pure constrained per-monitor mapping and crop helper | Negative-origin, unequal-scale and both-direction geometry tests; partial crop rejection; opaque alpha test; actual multi-monitor desktop behavior still human |
| Raw TOML and save validation | `config.rs`; atomic rename; shipped engine/language/hotkey validation | TOML round-trip, remote field rejection, hotkey parser tests |
| Tray, progress, Settings, autostart, no main window | `desktop.rs`, bundled `ui/`; plugin adapters | Packaged tray/plugin readiness; LSUIElement; no configured startup window; last-window exit-policy test; interactive settings/hotkey/login/progress appearance still human |
| F10 terminal CLI delegation | Exact private socket request/reply plus Tauri single-instance plugin; cold launcher lock | All outcomes over IPC; actual concurrent clients with copied/empty/clipboard failure; one cold resident launch; packaged duplicate GUI exits while resident lives |
| Privacy boundary | No text-bearing result Serialize/Debug; closed failure enums; capture helper streams null; text-only clipboard adapter | Code inspection; serialized replies omit text; smoke does not access screen/clipboard |
| Stable signing/release process | Bundle identifier and README signing/notary checklist | Signed app build and codesign verification; no notarization submitted |
| Development selftest | Debug-only flag and synthetic sample window | Release flag rejection; actual interactive selection left for a human |

## Checks actually run

- Initial signature-only implementation was exercised against truncated/oversized regression tests: both failed. Bounded decode implementation made them pass.
- `cargo test --offline --locked`: **42 unit tests and 1 integration test passed**, no failures or ignored tests. Native Vision tests ran on this macOS host, not as mocks.
- `cargo clippy --offline --locked --all-targets -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `node --check ui/app.js`: passed.
- `sandbox-exec -p '(version 1) (allow default) (deny network-outbound)' cargo test --offline --locked --lib engine::tests`: all 3 native-engine/allowlist tests passed with outbound networking denied.
- `cargo build --offline`: passed during implementation; subsequent release bundle builds also compiled the application.
- `CARGO_NET_OFFLINE=true cargo tauri build --bundles app`, with notarization credential variable names explicitly unset on the final build: passed; produced `target/release/bundle/macos/ClipOCR.app`. The installed signing setup signed the bundle. Tauri explicitly skipped notarization.
- `codesign --verify --deep --strict --verbose=2 target/release/bundle/macos/ClipOCR.app`: valid on disk; satisfies its designated requirement.
- Bundle metadata checked: identifier `ca.clipocr.desktop`, version `0.1.0`, `LSUIElement=true`.
- `python3 scripts/smoke_resident.py target/release/bundle/macos/ClipOCR.app`: passed release help/dev-only/extra-argument rejection, real packaged startup, private socket invalid-request handling, owner-only directories/empty capture storage, duplicate-instance handling. The harness stopped only the resident it launched and never requested a capture.

One inherited build-environment warning is unrelated to the macOS implementation: Tauri reports deprecated `STATIC_VCRUNTIME`; this session does not change shared machine environment settings.

## Explicit limitations / remaining human acceptance

Not claimed as verified: TCC first denial/grant/re-grant from a clean account; actual region selection/cancellation; video/DRM behavior; cross-monitor/Retina real captures; visible Settings/progress rendering; global shortcut conflict UI; real OS clipboard failure/write-read; installed login launch; disconnected-desktop capture-to-clipboard; older macOS compatibility; Gatekeeper/notarization; Linux/Windows integration. The README contains the interactive synthetic test and full smoke checklist. Engine timeout and capture timeout were exercised with controlled fakes, not by leaving a real user's picker open for 600 seconds or wedging Apple Vision.

A stuck native thread is intentionally not replaced by another live capture: `failed(timeout)` releases the request guard, while the separate worker lease holds off new captures until that native call returns (or the app is restarted). Unlink is not guaranteed physical erasure, and hard process kills/power loss are outside destructor guarantees.

Independent review was requested from Codie against the implementation. A passing local suite/build is not a substitute for that review or the human release checks above.
