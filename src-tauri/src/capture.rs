#[cfg(target_os = "macos")]
use crate::core::InFlightGuard;
#[cfg(any(target_os = "macos", target_os = "linux", all(test, unix)))]
use crate::core::MAX_BYTES;
#[cfg(target_os = "windows")]
use crate::core::MAX_PIXELS;
use crate::core::{CaptureDevice, CaptureError, Failure};
use async_trait::async_trait;
#[cfg(target_os = "macos")]
use std::sync::{atomic::AtomicBool, Arc};
use std::{path::PathBuf, time::Duration};
pub const CAPTURE_DEADLINE: Duration = Duration::from_secs(600);
pub const OVERLAY_SELECT_TIMEOUT: Duration = Duration::from_secs(120);
/// Slow compositors may need this raised when overlay platforms are implemented.
pub const OVERLAY_HIDE_GRACE: Duration = Duration::from_millis(250);

#[cfg(target_os = "macos")]
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
}
/// Explicit user gesture only; unlike preflight this may display a TCC prompt.
pub fn request_permission() {
    #[cfg(target_os = "macos")]
    unsafe {
        CGRequestScreenCaptureAccess();
    }
}
pub fn permission_granted() -> bool {
    #[cfg(target_os = "macos")]
    {
        unsafe { CGPreflightScreenCaptureAccess() }
    }
    #[cfg(target_os = "linux")]
    {
        // Wayland (wlroots/omarchy) does not gate screen capture behind a
        // permission grant the way macOS TCC does: grim/slurp capture what the
        // compositor shows. Report granted; the spawn itself fails loudly if
        // grim/slurp are missing or the session is X11 (grim exits non-zero).
        true
    }
    #[cfg(target_os = "windows")]
    {
        // Windows region capture draws its own overlay (monitor capture
        // needs no consent UI on 1903+). No separate permission gate
        // exists, so report granted; failures surface as capture errors.
        true
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        false
    }
}
#[cfg(target_os = "macos")]
trait ScreenPermission: Send + Sync {
    fn granted(&self) -> bool;
}
#[cfg(target_os = "macos")]
struct NativePermission;
#[cfg(target_os = "macos")]
impl ScreenPermission for NativePermission {
    fn granted(&self) -> bool {
        permission_granted()
    }
}
pub struct PlatformCapture {
    pub root: PathBuf,
    #[cfg(target_os = "macos")]
    live: Arc<AtomicBool>,
    #[cfg(target_os = "macos")]
    permission: Arc<dyn ScreenPermission>,
}
impl PlatformCapture {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            #[cfg(target_os = "macos")]
            live: Arc::default(),
            #[cfg(target_os = "macos")]
            permission: Arc::new(NativePermission),
        }
    }
}
#[async_trait]
impl CaptureDevice for PlatformCapture {
    async fn capture(&self) -> Result<Vec<u8>, CaptureError> {
        #[cfg(target_os = "macos")]
        {
            // Check EACH time. Without TCC, screencapture can return wallpaper!
            if !self.permission.granted() {
                return Err(CaptureError::PermissionDenied);
            }
            let lease = InFlightGuard::acquire(&self.live)
                .ok_or(CaptureError::Failed(Failure::Unavailable))?;
            let root = self.root.clone();
            tokio::task::spawn_blocking(move || {
                let _lease = lease;
                let dir = crate::storage::capture_dir(&root).map_err(CaptureError::Failed)?;
                capture_with_command(&dir, CAPTURE_DEADLINE, |path| {
                    let mut cmd = std::process::Command::new("/usr/sbin/screencapture");
                    cmd.args(["-i", "-x", "-t", "png"]).arg(path);
                    Ok(cmd)
                })
            })
            .await
            .map_err(|_| CaptureError::Failed(Failure::Capture))?
        }
        #[cfg(target_os = "linux")]
        {
            let root = self.root.clone();
            tokio::task::spawn_blocking(move || {
                let dir = crate::storage::capture_dir(&root).map_err(CaptureError::Failed)?;
                capture_with_command(&dir, CAPTURE_DEADLINE, |path| {
                    // slurp prints the selected region as "x,y WxH" on stdout.
                    // grim -g captures exactly that region to the target path.
                    let geometry = slurp_geometry()?;
                    let mut cmd = std::process::Command::new("grim");
                    cmd.args(["-g", &geometry, "-t", "png"]).arg(path);
                    Ok::<std::process::Command, CaptureError>(cmd)
                })
            })
            .await
            .map_err(|_| CaptureError::Failed(Failure::Capture))?
        }
        #[cfg(target_os = "windows")]
        {
            #[cfg(target_os = "windows")]
            use std::sync::mpsc;
            #[cfg(target_os = "windows")]
            use windows::Win32::Foundation::POINT;
            #[cfg(target_os = "windows")]
            use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONULL};
            #[cfg(target_os = "windows")]
            use windows_capture::{
                capture::GraphicsCaptureApiHandler,
                frame::Frame,
                graphics_capture_api::InternalCaptureControl,
                monitor::Monitor,
                settings::{
                    ColorFormat, CursorCaptureSettings, DirtyRegionSettings, DrawBorderSettings,
                    MinimumUpdateIntervalSettings, SecondaryWindowSettings, Settings,
                },
            };

            // Region selection (Snipping-Tool style crosshair overlay, see
            // overlay_windows.rs) replaces the WinRT window picker. The
            // overlay pumps messages; spawn_blocking gives us a thread.
            tokio::task::spawn_blocking(move || {
                let selector = crate::overlay_windows::Win32RegionSelector;
                let selection = match selector.select_region() {
                    Ok(Some(rect)) => rect,
                    // Esc / right-click / click without drag.
                    Ok(None) => return Err(CaptureError::Cancelled),
                    Err(Failure::Timeout) => return Err(CaptureError::Failed(Failure::Timeout)),
                    Err(e) => return Err(CaptureError::Failed(e)),
                };
                // Map the desktop-coord selection to its origin monitor and
                // a monitor-LOCAL crop rect (rejects cross-monitor spill).
                let monitors =
                    crate::overlay_windows::enumerate_monitors().map_err(CaptureError::Failed)?;
                let rects: Vec<crate::geometry::CaptureRect> =
                    monitors.iter().map(|(_, r)| *r).collect();
                let local = crate::geometry::monitor_local_crop(&rects, selection)
                    .ok_or(CaptureError::Failed(Failure::Capture))?
                    .1;
                // The monitor that owns the selection origin.
                let hmon = unsafe {
                    MonitorFromPoint(
                        POINT {
                            x: selection.x,
                            y: selection.y,
                        },
                        MONITOR_DEFAULTTONULL,
                    )
                };
                if hmon.is_invalid() {
                    return Err(CaptureError::Failed(Failure::Capture));
                }
                let item = Monitor::from_raw_hmonitor(hmon.0);
                let (tx, rx) = mpsc::channel::<Result<(u32, u32, Vec<u8>), Failure>>();
                let settings = Settings::new(
                    item,
                    CursorCaptureSettings::Default,
                    DrawBorderSettings::Default,
                    SecondaryWindowSettings::Default,
                    MinimumUpdateIntervalSettings::Default,
                    DirtyRegionSettings::Default,
                    ColorFormat::Rgba8,
                    tx,
                );
                // `start` (blocking) takes control of this spawn_blocking
                // thread until the handler stops after the first frame; the
                // handler sends the RGBA frame through the channel.
                struct FirstFrame {
                    tx: mpsc::Sender<Result<(u32, u32, Vec<u8>), Failure>>,
                }
                impl GraphicsCaptureApiHandler for FirstFrame {
                    type Flags = mpsc::Sender<Result<(u32, u32, Vec<u8>), Failure>>;
                    type Error = Box<dyn std::error::Error + Send + Sync>;
                    fn new(
                        ctx: windows_capture::capture::Context<Self::Flags>,
                    ) -> Result<Self, Self::Error> {
                        Ok(FirstFrame { tx: ctx.flags })
                    }
                    fn on_frame_arrived(
                        &mut self,
                        frame: &mut Frame<'_>,
                        capture_control: InternalCaptureControl,
                    ) -> Result<(), Self::Error> {
                        let buffer = frame.buffer()?;
                        let width = buffer.width();
                        let height = buffer.height();
                        let mut scratch = Vec::new();
                        let pixels = buffer.as_nopadding_buffer(&mut scratch).to_vec();
                        let _ = self.tx.send(Ok((width, height, pixels)));
                        capture_control.stop();
                        Ok(())
                    }
                }
                let _ = FirstFrame::start(settings);
                match rx.recv_timeout(CAPTURE_DEADLINE) {
                    Ok(Ok((w, h, rgba))) => {
                        // Whole-monitor frame; guard the size before the
                        // image allocation, then crop the selected region.
                        if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
                            return Err(CaptureError::Failed(Failure::Capture));
                        }
                        let img = image::RgbaImage::from_raw(w, h, rgba)
                            .ok_or(CaptureError::Failed(Failure::Capture))?;
                        crate::geometry::capture_screen_region_png(&img, local)
                            .map_err(CaptureError::Failed)
                    }
                    Ok(Err(_)) => Err(CaptureError::Failed(Failure::Capture)),
                    Err(_) => Err(CaptureError::Failed(Failure::Timeout)),
                }
            })
            .await
            .map_err(|_| CaptureError::Failed(Failure::Capture))?
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            Err(CaptureError::Failed(Failure::Unsupported))
        }
    }
}

/// Run `slurp` for interactive region selection on Wayland (wlroots). Prints
/// the selected geometry as `x,y WxH` used by `grim -g`. A cancelled or empty
/// selection maps to [`CaptureError::Cancelled`]; a slurp that is not present
/// or exits abnormally maps to [`CaptureError::Failed`].
#[cfg(target_os = "linux")]
fn slurp_geometry() -> Result<String, CaptureError> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("slurp")
        .args(["-f", "%x,%y %wx%h"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| CaptureError::Failed(Failure::Capture))?;
    let start = std::time::Instant::now();
    // slurp is interactive; wait for it to finish but bound the wait.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Err(_) => return Err(CaptureError::Failed(Failure::Capture)),
            Ok(None) if start.elapsed() >= OVERLAY_SELECT_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(CaptureError::Failed(Failure::Timeout));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    let mut out = String::new();
    use std::io::Read;
    child
        .stdout
        .take()
        .ok_or(CaptureError::Failed(Failure::Capture))?
        .read_to_string(&mut out)
        .map_err(|_| CaptureError::Failed(Failure::Capture))?;
    if !status.success() {
        return Err(CaptureError::Cancelled);
    }
    let geometry = out.trim().to_owned();
    if geometry.is_empty() || crate::geometry::parse_slurp_geometry(&geometry).is_none() {
        return Err(CaptureError::Cancelled);
    }
    // Re-serialize through the canonical formatter so grim receives exactly the
    // grammar it accepts, regardless of slurp's output quirks.
    let rect = crate::geometry::parse_slurp_geometry(&geometry).expect("checked above");
    Ok(crate::geometry::slurp_geometry_string(rect))
}

/// Synchronous child ownership is deliberate: Drop kills AND reaps before file
/// cleanup, even on unwinding. A dropped Tokio handle alone cannot guarantee this.
#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[cfg(unix)]
struct ChildGuard(std::process::Child);
#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[cfg(any(target_os = "macos", target_os = "linux", test))]
#[cfg(unix)]
fn capture_with_command(
    dir: &std::path::Path,
    deadline: Duration,
    command: impl FnOnce(&std::path::Path) -> Result<std::process::Command, CaptureError>,
) -> Result<Vec<u8>, CaptureError> {
    use std::{
        fs::OpenOptions,
        io::Read,
        os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        process::Stdio,
        time::Instant,
    };
    let path = dir.join(format!("capture-{}.png", uuid::Uuid::new_v4()));
    let cleanup = crate::storage::RemoveOnDrop(path.clone());
    let prepared = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
        .map_err(|_| CaptureError::Failed(Failure::Capture))?;
    drop(prepared);
    // Declare after cleanup so child Drop always runs first.
    let mut cmd = match command(&path) {
        Ok(cmd) => cmd,
        Err(e) => {
            cleanup.remove().map_err(CaptureError::Failed)?;
            return Err(e);
        }
    };
    let mut child = ChildGuard(
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| CaptureError::Failed(Failure::Capture))?,
    );
    let start = Instant::now();
    let status = loop {
        match child.0.try_wait() {
            Ok(Some(status)) => break status,
            Err(_) => return Err(CaptureError::Failed(Failure::Capture)),
            Ok(None) if start.elapsed() >= deadline => {
                // Terminate + wait before the cleanup guard can unlink the path.
                let _ = child.0.kill();
                child
                    .0
                    .wait()
                    .map_err(|_| CaptureError::Failed(Failure::Capture))?;
                return Err(CaptureError::Failed(Failure::Timeout));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    drop(child);
    if !status.success() {
        cleanup.remove().map_err(CaptureError::Failed)?;
        return Err(CaptureError::Cancelled);
    }
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            cleanup.remove().map_err(CaptureError::Failed)?;
            return Err(CaptureError::Cancelled);
        }
        Err(_) => return Err(CaptureError::Failed(Failure::Capture)),
    };
    // screencapture can replace the pre-created 0600 inode with a new 0644
    // file. The private 0700 directory confines it while the child runs; restore
    // owner-only permissions before validation/read. Use the O_NOFOLLOW-opened
    // descriptor, not a path chmod that could follow a replacement symlink.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
        .map_err(|_| CaptureError::Failed(Failure::Capture))?;
    let meta = file
        .metadata()
        .map_err(|_| CaptureError::Failed(Failure::Capture))?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o777 != 0o600
        || meta.len() > MAX_BYTES as u64
    {
        return Err(CaptureError::Failed(Failure::Capture));
    }
    let mut bytes = Vec::new();
    let read_result = file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes);
    // Unlink even on read failure; insist on successful removal BEFORE any OCR.
    cleanup.remove().map_err(CaptureError::Failed)?;
    read_result.map_err(|_| CaptureError::Failed(Failure::Capture))?;
    if bytes.is_empty() {
        return Err(CaptureError::Cancelled);
    }
    if bytes.len() > MAX_BYTES {
        return Err(CaptureError::Failed(Failure::Capture));
    }
    Ok(bytes)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn denied_permission_is_preflighted_each_time_without_creating_a_file() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Denied(Arc<AtomicUsize>);
        impl ScreenPermission for Denied {
            fn granted(&self) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst);
                false
            }
        }
        let root = tempfile::tempdir().unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let device = PlatformCapture {
            root: root.path().to_owned(),
            live: Arc::default(),
            permission: Arc::new(Denied(calls.clone())),
        };
        for _ in 0..2 {
            assert!(matches!(
                device.capture().await,
                Err(CaptureError::PermissionDenied)
            ));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }
    #[test]
    fn timed_out_child_is_reaped_before_file_cleanup() {
        let t = tempfile::tempdir().unwrap();
        let dir = t.path().join("capture");
        std::fs::create_dir(&dir).unwrap();
        let pid_file = t.path().join("pid");
        // One fake capture process: records its PID, waits, then tries to create
        // the screenshot. No subprocesses that could outlive the process we kill.
        let result = capture_with_command(&dir, Duration::from_millis(80), |path| {
            let mut c = std::process::Command::new("/usr/bin/perl");
            c.arg("-e").arg("open(my $pid, '>', $ARGV[1]) or die; print $pid $$; close $pid; select(undef,undef,undef,0.3); open(my $out, '>', $ARGV[0]) or die; print $out 'late synthetic screenshot'; close $out;")
                .arg(path).arg(&pid_file);
            Ok(c)
        });
        assert!(matches!(
            result,
            Err(CaptureError::Failed(Failure::Timeout))
        ));
        let pid: i32 = std::fs::read_to_string(pid_file).unwrap().parse().unwrap();
        assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
        std::thread::sleep(Duration::from_millis(350));
        assert_eq!(
            std::fs::read_dir(&dir).unwrap().count(),
            0,
            "killed child must never recreate a PNG later"
        );
    }
    #[test]
    fn replaced_capture_file_restores_private_mode_before_read_and_unlinks() {
        use std::os::unix::fs::MetadataExt;
        let root = tempfile::tempdir().unwrap();
        let mut original = None;
        let result = capture_with_command(root.path(), Duration::from_secs(1), |path| {
            // Keep the original inode alive so the replacement cannot reuse it.
            let file = std::fs::File::open(path).unwrap();
            let inode = file.metadata().unwrap().ino();
            original = Some(file);
            let mut cmd = std::process::Command::new("/usr/bin/perl");
            // Match real screencapture: replace, rather than truncate, then leave
            // a fresh 0644 file regardless of the test process's current umask.
            cmd.arg("-e")
                .arg("unlink $ARGV[0] or die; open(my $out, '>', $ARGV[0]) or die; print $out 'synthetic'; close $out or die; chmod 0644, $ARGV[0] or die; my @st = stat $ARGV[0]; die if $st[1] == $ARGV[1] || ($st[2] & 0777) != 0644;")
                .arg(path)
                .arg(inode.to_string());
            Ok(cmd)
        });
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        assert_eq!(original.unwrap().metadata().unwrap().len(), 0);
        assert_eq!(result.unwrap(), b"synthetic");
    }

    #[test]
    fn replacement_symlink_is_rejected_without_changing_target_permissions() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("capture");
        std::fs::create_dir(&dir).unwrap();
        let target = root.path().join("synthetic-target");
        std::fs::write(&target, b"synthetic").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let result = capture_with_command(&dir, Duration::from_secs(1), |path| {
            std::fs::remove_file(path).unwrap();
            std::os::unix::fs::symlink(&target, path).unwrap();
            Ok(std::process::Command::new("/usr/bin/true"))
        });
        assert!(matches!(
            result,
            Err(CaptureError::Failed(Failure::Capture))
        ));
        assert_eq!(std::fs::metadata(&target).unwrap().mode() & 0o777, 0o644);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    }

    #[test]
    fn spawn_cancel_read_failures_cleanup_and_read_then_unlink() {
        let t = tempfile::tempdir().unwrap();
        assert!(capture_with_command(t.path(), Duration::from_secs(1), |_| {
            Ok(std::process::Command::new("/nonexistent-clipocr-test"))
        })
        .is_err());
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 0);
        let out = capture_with_command(t.path(), Duration::from_secs(1), |_| {
            Ok(std::process::Command::new("/usr/bin/false"))
        });
        assert!(matches!(out, Err(CaptureError::Cancelled)));
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 0);
        let bytes = capture_with_command(t.path(), Duration::from_secs(1), |p| {
            std::fs::write(p, b"synthetic bytes").unwrap();
            Ok(std::process::Command::new("/usr/bin/true"))
        })
        .unwrap();
        assert_eq!(bytes, b"synthetic bytes");
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 0);
        assert!(capture_with_command(t.path(), Duration::from_secs(1), |p| {
            std::fs::remove_file(p).unwrap();
            std::os::unix::fs::symlink("/nonexistent", p).unwrap();
            Ok(std::process::Command::new("/usr/bin/true"))
        })
        .is_err());
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 0);
    }
}
