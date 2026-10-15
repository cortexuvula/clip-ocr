#[cfg(target_os = "macos")]
use crate::core::InFlightGuard;
#[cfg(any(target_os = "macos", test))]
use crate::core::MAX_BYTES;
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
    #[cfg(not(target_os = "macos"))]
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
                    cmd
                })
            })
            .await
            .map_err(|_| CaptureError::Failed(Failure::Capture))?
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(CaptureError::Failed(Failure::Unsupported))
        }
    }
}

/// Synchronous child ownership is deliberate: Drop kills AND reaps before file
/// cleanup, even on unwinding. A dropped Tokio handle alone cannot guarantee this.
#[cfg(any(target_os = "macos", test))]
#[cfg(unix)]
struct ChildGuard(std::process::Child);
#[cfg(any(target_os = "macos", test))]
#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[cfg(any(target_os = "macos", test))]
#[cfg(unix)]
fn capture_with_command(
    dir: &std::path::Path,
    deadline: Duration,
    command: impl FnOnce(&std::path::Path) -> std::process::Command,
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
    let mut cmd = command(&path);
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
            c
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
            cmd
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
            std::process::Command::new("/usr/bin/true")
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
            std::process::Command::new("/nonexistent-clipocr-test")
        })
        .is_err());
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 0);
        let out = capture_with_command(t.path(), Duration::from_secs(1), |_| {
            std::process::Command::new("/usr/bin/false")
        });
        assert!(matches!(out, Err(CaptureError::Cancelled)));
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 0);
        let bytes = capture_with_command(t.path(), Duration::from_secs(1), |p| {
            std::fs::write(p, b"synthetic bytes").unwrap();
            std::process::Command::new("/usr/bin/true")
        })
        .unwrap();
        assert_eq!(bytes, b"synthetic bytes");
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 0);
        assert!(capture_with_command(t.path(), Duration::from_secs(1), |p| {
            std::fs::remove_file(p).unwrap();
            std::os::unix::fs::symlink("/nonexistent", p).unwrap();
            std::process::Command::new("/usr/bin/true")
        })
        .is_err());
        assert_eq!(std::fs::read_dir(t.path()).unwrap().count(), 0);
    }
}
