//! Tauri shell: no main window; settings and progress exist only on demand.
use crate::{
    capture,
    config::{self, Config},
    core::{self, Clipboard, Failure, Feedback, Outcome, Pipeline, Progress, Status},
    engine, ipc, storage, update,
};
use std::{
    fs::File,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::TrayIconBuilder,
    Manager,
};
use tauri_plugin_global_shortcut::GlobalShortcutExt;

pub struct AppState {
    pub pipeline: Pipeline,
    config: Mutex<Config>,
    config_error: Mutex<Option<String>>,
    config_blocked: AtomicBool,
    settings_save: tokio::sync::Mutex<()>,
    languages: Vec<String>,
    root: PathBuf,
    hotkey_active: AtomicBool,
    hotkey_item: MenuItem<tauri::Wry>,
    engine_item: MenuItem<tauri::Wry>,
    permission_item: MenuItem<tauri::Wry>,
    update_item: MenuItem<tauri::Wry>,
    update_action: MenuItem<tauri::Wry>,
    update: Arc<update::UpdateManager>,
    _instance_lock: File,
}
struct DesktopClipboard(tauri::AppHandle);
impl Clipboard for DesktopClipboard {
    fn write_text(&self, text: &str) -> Result<(), Failure> {
        use tauri_plugin_clipboard_manager::ClipboardExt;
        self.0
            .clipboard()
            .write_text(text)
            .map_err(|_| Failure::Clipboard)
    }
}
struct DesktopFeedback(tauri::AppHandle);
struct ProgressIndicator(tauri::AppHandle);
impl Progress for ProgressIndicator {}
impl Drop for ProgressIndicator {
    fn drop(&mut self) {
        if let Some(w) = self.0.get_webview_window("progress") {
            let _ = w.destroy();
        }
    }
}
impl Feedback for DesktopFeedback {
    fn recognizing(&self) -> Box<dyn Progress> {
        let app = &self.0;
        if let Some(w) = app.get_webview_window("progress") {
            let _ = w.destroy();
        }
        let mut builder = tauri::WebviewWindowBuilder::new(
            app,
            "progress",
            tauri::WebviewUrl::App("index.html#progress".into()),
        )
        .title("ClipOCR — recognizing")
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .resizable(false)
        .shadow(false)
        .focused(false)
        .inner_size(220.0, 44.0)
        .background_color(tauri::window::Color(20, 20, 24, 255));
        if let Ok(Some(monitor)) = app.primary_monitor() {
            let p = monitor.position();
            let (x, y) = crate::geometry::indicator_position(
                p.x,
                p.y,
                monitor.size().width,
                monitor.scale_factor(),
            );
            builder = builder.position(x, y);
        }
        if let Ok(w) = builder.build() {
            let _ = w.set_ignore_cursor_events(true);
        }
        Box::new(ProgressIndicator(app.clone()))
    }
}
struct UnsupportedEngine;
impl core::OcrEngine for UnsupportedEngine {
    fn recognize(&self, _: &[u8], _: &str) -> Result<core::OcrResult, Failure> {
        Err(Failure::Unsupported)
    }
}

pub async fn run_capture(app: &tauri::AppHandle) -> Outcome {
    let state = app.state::<AppState>();
    let config = state
        .config
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let out = if state.config_blocked.load(Ordering::SeqCst)
        || config.validate(&state.languages).is_err()
    {
        Outcome::failed(Failure::Unsupported)
    } else {
        state
            .pipeline
            .run(
                config.language,
                config.collapse_line_breaks,
                &DesktopFeedback(app.clone()),
            )
            .await
    };
    refresh_menu(app);
    notify(&out);
    out
}
pub fn trigger(app: &tauri::AppHandle) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        run_capture(&app).await;
    });
}
struct AppRequest(tauri::AppHandle);
#[async_trait::async_trait]
impl ipc::RequestHandler for AppRequest {
    async fn capture(&self) -> Outcome {
        run_capture(&self.0).await
    }
}

fn notify(out: &Outcome) {
    // Fixed statuses/counts only. No recognized text or framework/helper errors.
    let message = match out.status {
        Status::Copied => format!("Copied {} characters", out.chars), Status::Cancelled => "Capture cancelled".into(),
        Status::Empty => "No text found".into(), Status::PermissionDenied => "Screen Recording permission required. Use the ClipOCR tray menu to open System Settings, then restart ClipOCR if needed.".into(),
        Status::LowConfidence => "Low confidence — clipboard unchanged".into(), Status::InProgress => "Capture already running".into(),
        Status::Failed => "Capture failed — clipboard unchanged. Check ClipOCR settings.".into(),
    };
    tauri::async_runtime::spawn_blocking(move || {
        #[cfg(target_os = "macos")]
        {
            use std::process::{Command, Stdio};
            if let Ok(mut child) = Command::new("/usr/bin/osascript")
                .args([
                    "-e",
                    &format!("display notification \"{message}\" with title \"ClipOCR\""),
                ])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                let start = std::time::Instant::now();
                while matches!(child.try_wait(), Ok(None))
                    && start.elapsed() < Duration::from_secs(3)
                {
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = child.kill();
                let _ = child.wait();
            }
        }
        #[cfg(not(target_os = "macos"))]
        let _ = message;
    });
}
/// Open the canonical release page in the system browser (Rust side).
/// The URL is validated at the POINT OF USE against the exact shape
/// release.yml generates (`is_canonical_release_url`): a crafted manifest
/// must never put quotes or shell metacharacters in front of an OS
/// launcher. Windows deliberately does NOT shell out through cmd
/// (`std::process::Command` cannot use ShellExecute for a bare URL); the
/// no-shell launcher is `explorer.exe <url>`, which opens the default
/// browser via the shell URL association with plain CreateProcessW argv —
/// no cmd parsing anywhere in the path.
fn open_release_page(url: &str) {
    if !update::is_canonical_release_url(url) {
        return; // typed rejection: never open a non-canonical URL
    }
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("explorer.exe");
        c.arg(url);
        c
    };
    #[cfg(not(target_os = "windows"))]
    let mut cmd = {
        let mut c = std::process::Command::new(if cfg!(target_os = "macos") {
            "/usr/bin/open"
        } else {
            "xdg-open"
        });
        c.arg(url);
        c
    };
    let _ = cmd.spawn();
}

/// Resident tick: run the update manager's cadence (deferred while a
/// capture/OCR lease is held), then refresh the tray. No network in the
/// startup critical path — this only ever runs from the periodic loop.
fn update_tick(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let manager = state.update.clone();
    let busy = state.pipeline.busy();
    if let Some(lease) = manager.tick(Instant::now(), update::current_jitter(), busy) {
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let manager = manager.clone();
            tauri::async_runtime::spawn_blocking(move || {
                manager.run_check(&lease, std::time::SystemTime::now());
            })
            .await
            .ok();
            refresh_menu(&app);
        });
    } else {
        // Still refresh the row (e.g. Checking… finished elsewhere).
        let _ = state.update_item.set_text(manager.status().tray_label());
    }
}

fn refresh_menu(app: &tauri::AppHandle) {
    let state = app.state::<AppState>();
    let config = state
        .config
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let config_warning = state
        .config_error
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some();
    // Never hold a state mutex while calling a main-thread menu API: the main
    // thread can simultaneously be servicing a Settings command needing it.
    let active = state.hotkey_active.load(Ordering::SeqCst);
    let wayland = cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some();
    let _ = state.hotkey_item.set_text(format!(
        "Hotkey: {}{}",
        config::display_hotkey(&config.hotkey),
        if wayland {
            " (compositor bind)"
        } else if active {
            ""
        } else {
            " — INACTIVE (use Capture)"
        }
    ));
    let _ = state.engine_item.set_text(format!(
        "Engine: {}{}",
        engine_label(),
        if config_warning {
            " — SETTINGS REQUIRED"
        } else {
            ""
        }
    ));
    // Update rows (ui-consultant contract). The action NEVER says
    // "Install" — it opens the release page in a browser.
    let update_status = state.update.status();
    let _ = state.update_item.set_text(update_status.tray_label());
    let _ = state.update_action.set_text(match &update_status {
        update::UpdateStatus::Available { .. } => "Update ClipOCR…",
        _ => "Check for updates…",
    });
    let permission = capture::permission_granted();
    let _ = state
        .permission_item
        .set_text(if cfg!(target_os = "macos") {
            if permission {
                "Screen Recording: granted (Open Settings)"
            } else {
                "⚠ Screen Recording REQUIRED — Open Settings"
            }
        } else if cfg!(target_os = "linux") {
            "Capture: grim + slurp (Wayland)"
        } else if cfg!(target_os = "windows") {
            "Capture: crosshair region"
        } else {
            "⚠ Capture unsupported on this platform"
        });
    if let Some(tray) = app.tray_by_id("clipocr") {
        let base = if cfg!(not(any(
            target_os = "macos",
            target_os = "linux",
            target_os = "windows"
        ))) {
            "ClipOCR — unsupported platform"
        } else if !permission && cfg!(target_os = "macos") {
            "ClipOCR — Screen Recording permission required"
        } else if cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some() {
            "ClipOCR — Wayland: bind Ctrl+Alt+X to clipocr --capture-ocr in your compositor"
        } else if !active {
            "ClipOCR — hotkey INACTIVE; use tray Capture"
        } else {
            "ClipOCR — on-device OCR"
        };
        // Update text only ever APPENDS in the no-warning branch; it
        // never displaces a capture warning.
        let tooltip =
            if let (update::UpdateStatus::Available { latest }, "ClipOCR — on-device OCR") =
                (&update_status, base)
            {
                format!("{base} — update v{latest} available")
            } else {
                base.to_owned()
            };
        let _ = tray.set_tooltip(Some(&tooltip));
    }
}
fn engine_label() -> &'static str {
    if cfg!(target_os = "windows") {
        "WinRT"
    } else if cfg!(target_os = "linux") {
        "Tesseract"
    } else {
        "macVision"
    }
}
fn sync_hotkey(app: &tauri::AppHandle, config: &Config) {
    let shortcuts = app.global_shortcut();
    let unreg_ok = shortcuts.unregister_all();
    let reg = unreg_ok.and_then(|_| shortcuts.register(config::resolve_hotkey(&config.hotkey)));
    // On Wayland the X11 grab registers but never receives key events (the
    // compositor routes keys to native Wayland clients), so it is effectively
    // inactive: users bind `clipocr --capture-ocr` in their compositor instead.
    let wayland = cfg!(target_os = "linux") && std::env::var_os("WAYLAND_DISPLAY").is_some();
    let active = reg.is_ok() && !wayland;
    eprintln!(
        "[clipocr] hotkey register {:?}: active={} wayland={} (DISPLAY={:?}, WAYLAND={:?})",
        reg.as_ref().map(|_| ()).map_err(|e| format!("{:?}", e)),
        active,
        wayland,
        std::env::var("DISPLAY").ok(),
        std::env::var("WAYLAND_DISPLAY").ok()
    );
    app.state::<AppState>()
        .hotkey_active
        .store(active, Ordering::SeqCst);
}
fn open_settings(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let _ = tauri::WebviewWindowBuilder::new(
        app,
        "settings",
        tauri::WebviewUrl::App("index.html#settings".into()),
    )
    .title("ClipOCR Settings")
    .inner_size(480.0, 570.0)
    .resizable(false)
    .build();
}
#[derive(serde::Serialize)]
pub struct SettingsView {
    config: Config,
    /// Platform-form shortcut label (Cmd+Alt+X on macOS, Ctrl+Alt+X elsewhere).
    hotkey_display: String,
    languages: Vec<String>,
    hotkey_active: bool,
    warning: Option<String>,
    version: &'static str,
    /// Passive update-check state for the Updates section (no network
    /// happens through this view — the resident performs any request).
    update: UpdateView,
}

#[derive(serde::Serialize, Clone)]
pub struct UpdateView {
    /// One of: off | not_checked | checking | up_to_date | available | failed.
    pub state: String,
    pub latest: Option<String>,
    /// Fixed reason label (never a framework error string).
    pub reason: Option<&'static str>,
    pub last_checked: Option<String>,
    pub notes_url: Option<String>,
}

fn update_view(manager: &update::UpdateManager) -> UpdateView {
    use update::UpdateStatus as S;
    let (state, latest, reason) = match manager.status() {
        S::Off => ("off", None, None),
        S::NotChecked => ("not_checked", None, None),
        S::Checking => ("checking", None, None),
        S::UpToDate { latest } => ("up_to_date", Some(latest), None),
        S::Available { latest } => ("available", Some(latest), None),
        S::Failed { reason } => ("failed", None, Some(reason.label())),
    };
    UpdateView {
        state: state.to_owned(),
        latest,
        reason,
        last_checked: manager.last_success(),
        notes_url: manager.notes_url(),
    }
}
#[tauri::command]
fn load_settings(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<SettingsView, String> {
    if window.label() != "settings" {
        return Err("Settings window required".into());
    }
    let config = state
        .config
        .lock()
        .map_err(|_| "Settings unavailable")?
        .clone();
    Ok(SettingsView {
        hotkey_display: config::display_hotkey(&config.hotkey),
        config,
        languages: state.languages.clone(),
        hotkey_active: state.hotkey_active.load(Ordering::SeqCst),
        warning: state
            .config_error
            .lock()
            .map_err(|_| "Settings unavailable")?
            .clone(),
        version: env!("CARGO_PKG_VERSION"),
        update: update_view(&state.update),
    })
}
#[tauri::command]
async fn save_settings(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    mut config: Config,
) -> Result<SettingsView, String> {
    use tauri_plugin_autostart::ManagerExt;
    if window.label() != "settings" {
        return Err("Settings window required".into());
    }
    eprintln!(
        "[clipocr] save_settings arrived: hotkey={:?} engine={:?} language={:?}",
        config.hotkey, config.engine, config.language
    );
    config.validate(&state.languages).map_err(|e| {
        eprintln!("[clipocr] save_settings validate failed: {e}");
        e
    })?;
    let save_guard = state.settings_save.lock().await;
    if state.pipeline.busy() {
        return Err("Wait for the current capture to finish before saving".into());
    }
    config.hotkey = config::resolve_hotkey(&config.hotkey).to_owned();
    {
        let autostart = app.autolaunch();
        let was_enabled = autostart
            .is_enabled()
            .map_err(|_| "Cannot query launch-at-login")?;
        if was_enabled != config.launch_at_login {
            if config.launch_at_login {
                autostart.enable()
            } else {
                autostart.disable()
            }
            .map_err(|_| "Cannot change launch-at-login")?;
        }
        if let Err(e) = config.save(&state.root) {
            if was_enabled != config.launch_at_login {
                let _ = if was_enabled {
                    autostart.enable()
                } else {
                    autostart.disable()
                };
            }
            return Err(e);
        }
        *state.config.lock().map_err(|_| "Settings unavailable")? = config.clone();
        *state
            .config_error
            .lock()
            .map_err(|_| "Settings unavailable")? = None;
        state.config_blocked.store(false, Ordering::SeqCst);
    }
    // Update opt-in follows the saved config (default OFF).
    state.update.set_enabled(config.update_checks);
    // Main-thread APIs must never execute while holding the config mutex.
    sync_hotkey(&app, &config);
    refresh_menu(&app);
    drop(save_guard);
    load_settings(window, state)
}
#[tauri::command]
async fn selftest_capture(
    window: tauri::WebviewWindow,
    app: tauri::AppHandle,
) -> Result<Outcome, String> {
    if !cfg!(debug_assertions) || window.label() != "selftest" {
        return Err("Development selftest only".into());
    }
    // Keep the synthetic text window visible. A human selects ONLY that text.
    Ok(run_capture(&app).await)
}

/// Settings button "Check for updates": sets a REQUEST flag only. The
/// network request is performed by the resident tick when idle — no
/// network, transport, or update logic is exposed to the webview.
#[tauri::command]
fn request_update_check(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    if window.label() != "settings" {
        return Err("Settings window required".into());
    }
    state.update.request_manual();
    Ok(())
}

/// Settings button "Review update…": opens the validated release page in
/// the system browser. No download, no install.
#[tauri::command]
fn open_update_page(
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<(), String> {
    if window.label() != "settings" {
        return Err("Settings window required".into());
    }
    let Some(url) = state.update.notes_url() else {
        return Ok(());
    };
    if !update::is_canonical_release_url(&url) {
        // Typed failure (fixed label — no URL contents in the reply).
        return Err("Update link unavailable".into());
    }
    open_release_page(&url);
    Ok(())
}

pub fn run(root: PathBuf, selftest: bool) -> Result<(), Box<dyn std::error::Error>> {
    tauri::Builder::default()
        // Secondary GUI launches never accept arbitrary argv. CLI uses the private
        // request/reply server; it does NOT initialize Tauri in the second process.
        .plugin(tauri_plugin_single_instance::init(|app, argv, _| {
            if ipc::wants_capture(&argv) {
                trigger(app);
            }
        }))
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_autostart::Builder::new().build())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, _, event| {
                    if event.state() == tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        trigger(app);
                    }
                })
                .build(),
        )
        .invoke_handler(tauri::generate_handler![
            load_settings,
            save_settings,
            selftest_capture,
            request_update_check,
            open_update_page
        ])
        .setup(move |app| {
            use fs2::FileExt;
            storage::assure_private_dir(&root)
                .map_err(|_| "Unsafe application data directory (requires owner-only 0700)")?;
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
            }
            let lock = options.open(root.join("instance.lock"))?;
            lock.try_lock_exclusive()
                .map_err(|_| "Another ClipOCR instance owns this profile")?;
            #[cfg(target_os = "macos")]
            {
                let dir = storage::capture_dir(&root)
                    .map_err(|_| "Unsafe capture directory; move it aside and restart ClipOCR")?;
                storage::sweep(&dir).map_err(|_| "Cannot clean stale captures")?;
            }
            #[cfg(target_os = "macos")]
            app.set_activation_policy(tauri::ActivationPolicy::Regular);
            let languages = engine::languages().unwrap_or_default();
            let loaded = Config::load(&root);
            let config_blocked = loaded.is_err();
            let (config, warning) = match loaded {
                Ok(c) => {
                    let warning = c.validate(&languages).err();
                    (c, warning)
                }
                Err(e) => (Config::default(), Some(e)),
            };
            let menu = Menu::new(app)?;
            let capture_item = MenuItem::with_id(app, "capture", "Capture", true, None::<&str>)?;
            let hotkey_item =
                MenuItem::with_id(app, "hotkey", "Hotkey: INACTIVE", false, None::<&str>)?;
            let engine_item =
                MenuItem::with_id(app, "engine", "Engine: INACTIVE", false, None::<&str>)?;
            let permission_item = MenuItem::with_id(
                app,
                "permission",
                "Screen Recording",
                cfg!(target_os = "macos"),
                None::<&str>,
            )?;
            let update_item =
                MenuItem::with_id(app, "updates", "Updates: Off", false, None::<&str>)?;
            let update_action = MenuItem::with_id(
                app,
                "update_action",
                "Check for updates…",
                true,
                None::<&str>,
            )?;
            menu.append_items(&[
                &capture_item,
                &PredefinedMenuItem::separator(app)?,
                &hotkey_item,
                &engine_item,
                &permission_item,
                &update_item,
                &update_action,
                &MenuItem::with_id(app, "settings", "Open Settings…", true, None::<&str>)?,
                &MenuItem::with_id(
                    app,
                    "about",
                    format!("About ClipOCR {}", env!("CARGO_PKG_VERSION")),
                    true,
                    None::<&str>,
                )?,
                &PredefinedMenuItem::separator(app)?,
                &MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?,
            ])?;
            let pipeline = Pipeline::new(
                Arc::new(capture::PlatformCapture::new(root.clone())),
                engine::create(config.engine).unwrap_or_else(|_| Arc::new(UnsupportedEngine)),
                Arc::new(DesktopClipboard(app.handle().clone())),
            );
            let update_manager = Arc::new(update::UpdateManager::new(
                Arc::new(update::UreqTransport),
                config.update_checks,
            ));
            app.manage(AppState {
                pipeline,
                config: Mutex::new(config.clone()),
                config_error: Mutex::new(warning),
                config_blocked: AtomicBool::new(config_blocked),
                settings_save: tokio::sync::Mutex::new(()),
                languages,
                root: root.clone(),
                hotkey_active: AtomicBool::new(false),
                hotkey_item,
                engine_item,
                permission_item,
                update_item,
                update_action,
                update: update_manager,
                _instance_lock: lock,
            });
            // Small generated icon; no external assets needed.
            // A filled square with two text lines — reads clearly as "OCR" at
            // menu-bar size. Rendered in "hacker" green (#00FF41), so it is not
            // a template image (macOS would otherwise force it monochrome).
            const GREEN: [u8; 3] = [0x00, 0xFF, 0x41];
            let mut pixels = vec![0u8; 22 * 22 * 4];
            for y in 0..22 {
                for x in 0..22 {
                    let border = !(2..20).contains(&x) || !(2..20).contains(&y);
                    let textline = (y == 8 || y == 13) && (4..18).contains(&x);
                    if border || textline {
                        let i = (y * 22 + x) * 4;
                        pixels[i] = GREEN[0];
                        pixels[i + 1] = GREEN[1];
                        pixels[i + 2] = GREEN[2];
                        pixels[i + 3] = 255;
                    }
                }
            }
            TrayIconBuilder::with_id("clipocr")
                .icon(tauri::image::Image::new_owned(pixels, 22, 22))
                .icon_as_template(false)
                .menu(&menu)
                .show_menu_on_left_click(true)
                .on_menu_event(|app, event| {
                    match event.id.as_ref() {
                        "capture" => trigger(app),
                        "settings" => open_settings(app),
                        "permission" => {
                            #[cfg(target_os = "macos")]
                            {
                                tauri::async_runtime::spawn_blocking(|| {
                                    if !capture::permission_granted() {
                                        capture::request_permission();
                                    }
                                    let _ = std::process::Command::new("/usr/bin/open")
                                        .arg(core::PERMISSION_LINK)
                                        .status();
                                });
                            }
                        }
                        "update_action" => {
                            let state = app.state::<AppState>();
                            if let update::UpdateStatus::Available { .. } = state.update.status() {
                                if let Some(url) = state.update.notes_url() {
                                    open_release_page(&url);
                                }
                            } else {
                                // Flag only — the network request is made
                                // by the resident tick when idle, never
                                // from the menu callback itself.
                                state.update.request_manual();
                                update_tick(app);
                            }
                        }
                        "about" => {
                            open_settings(app);
                        }
                        "quit" => {
                            if app
                                .state::<AppState>()
                                .pipeline
                                .in_flight
                                .load(Ordering::SeqCst)
                            {
                                // Let the picker finish/cancel and reap its child before
                                // orderly exit. Force-kill remains outside RAII guarantees.
                                notify(&Outcome::new(Status::InProgress));
                            } else {
                                app.exit(0);
                            }
                        }
                        _ => {}
                    }
                    refresh_menu(app);
                })
                .build(app)?;
            sync_hotkey(app.handle(), &config);
            // Reconcile stored login preference at boot; report failures in Settings.
            use tauri_plugin_autostart::ManagerExt;
            let login = app.autolaunch();
            let login_result = if config.launch_at_login {
                login.enable()
            } else {
                login.disable()
            };
            if login_result.is_err() {
                app.state::<AppState>()
                    .config_error
                    .lock()
                    .unwrap()
                    .get_or_insert_with(|| {
                        "Launch-at-login could not be applied; save Settings to retry".into()
                    });
            }
            refresh_menu(app.handle());
            #[cfg(unix)]
            {
                let listener = ipc::bind(&root).map_err(|_| "Cannot bind private CLI socket")?;
                let app_handle = app.handle().clone();
                tauri::async_runtime::spawn(ipc::serve(listener, Arc::new(AppRequest(app_handle))));
            }
            #[cfg(windows)]
            {
                let app_handle = app.handle().clone();
                ipc::spawn_server(root.clone(), Arc::new(AppRequest(app_handle)));
            }
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    update_tick(&handle);
                    refresh_menu(&handle);
                }
            });
            if selftest && cfg!(debug_assertions) {
                tauri::WebviewWindowBuilder::new(
                    app,
                    "selftest",
                    tauri::WebviewUrl::App("index.html#selftest".into()),
                )
                .title("ClipOCR — synthetic interactive selftest")
                .inner_size(700.0, 420.0)
                .build()?;
            }
            Ok(())
        })
        .build(tauri::generate_context!())?
        .run(|app, event| {
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                let active = app
                    .state::<AppState>()
                    .pipeline
                    .in_flight
                    .load(Ordering::SeqCst);
                if prevent_resident_exit(code, active) {
                    api.prevent_exit();
                    if code.is_some() {
                        notify(&Outcome::new(Status::InProgress));
                    }
                }
            }
        });
    Ok(())
}

fn prevent_resident_exit(code: Option<i32>, capture_active: bool) -> bool {
    // Destroying the last Settings/progress/selftest window requests code=None.
    // This is a resident tray utility: only explicit Quit may stop it, and not
    // before the active picker has terminated/reaped and its file is gone.
    code.is_none() || capture_active
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn last_window_closing_never_exits_the_resident() {
        assert!(prevent_resident_exit(None, false));
        assert!(prevent_resident_exit(None, true));
        assert!(prevent_resident_exit(Some(0), true));
        assert!(!prevent_resident_exit(Some(0), false));
    }
}
