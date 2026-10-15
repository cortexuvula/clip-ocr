//! Private storage: refuse unsafe existing directories rather than chmodding a
//! possibly attacker-controlled path. Deletion is not physical erasure on SSD/APFS.
use crate::core::Failure;
use std::{
    fs,
    path::{Path, PathBuf},
};

pub fn data_dir() -> Result<PathBuf, Failure> {
    dirs::data_local_dir()
        .map(|p| p.join("ca.clipocr.desktop"))
        .ok_or(Failure::UnsafeStorage)
}
#[cfg(unix)]
pub fn assure_private_dir(path: &Path) -> Result<(), Failure> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(Failure::UnsafeStorage),
    }
    let m = fs::symlink_metadata(path).map_err(|_| Failure::UnsafeStorage)?;
    validate_metadata(m.is_dir(), m.file_type().is_symlink(), m.uid(), m.mode())
}
#[cfg(unix)]
fn validate_metadata(is_dir: bool, symlink: bool, uid: u32, mode: u32) -> Result<(), Failure> {
    // SAFETY: geteuid takes no arguments and has no memory preconditions.
    if !is_dir || symlink || uid != unsafe { libc::geteuid() } || mode & 0o7777 != 0o700 {
        return Err(Failure::UnsafeStorage);
    }
    Ok(())
}
#[cfg(not(unix))]
pub fn assure_private_dir(path: &Path) -> Result<(), Failure> {
    // Non-Unix builds store only preferences and display an unsupported-capture
    // tray warning. Capture storage/ACLs MUST be implemented before enabling any
    // Windows capture device; the Unix private-directory contract is not claimed.
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(Failure::UnsafeStorage),
    }
    let m = fs::symlink_metadata(path).map_err(|_| Failure::UnsafeStorage)?;
    if m.is_dir() && !m.file_type().is_symlink() {
        Ok(())
    } else {
        Err(Failure::UnsafeStorage)
    }
}

pub fn capture_dir(root: &Path) -> Result<PathBuf, Failure> {
    assure_private_dir(root)?;
    let dir = root.join("capture-tmp");
    assure_private_dir(&dir)?;
    Ok(dir)
}
pub struct RemoveOnDrop(pub PathBuf);
impl RemoveOnDrop {
    pub fn remove(&self) -> Result<(), Failure> {
        match fs::remove_file(&self.0) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(_) => Err(Failure::UnsafeStorage),
        }
    }
}
impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        let _ = self.remove();
    }
}

/// Sweep only our own old regular capture files, after acquiring the instance
/// lock (no live child from another instance can be writing them).
pub fn sweep(dir: &Path) -> Result<(), Failure> {
    for entry in fs::read_dir(dir).map_err(|_| Failure::UnsafeStorage)? {
        let entry = entry.map_err(|_| Failure::UnsafeStorage)?;
        if entry.file_name().to_string_lossy().starts_with("capture-") {
            if !entry
                .file_type()
                .map_err(|_| Failure::UnsafeStorage)?
                .is_file()
            {
                return Err(Failure::UnsafeStorage);
            }
            fs::remove_file(entry.path()).map_err(|_| Failure::UnsafeStorage)?;
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    #[test]
    fn private_directory_revalidated_on_every_call() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("private");
        assure_private_dir(&p).unwrap();
        assure_private_dir(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(assure_private_dir(&p), Err(Failure::UnsafeStorage));
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
    #[test]
    fn symlinks_files_and_other_owners_rejected() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("link");
        symlink(t.path(), &p).unwrap();
        assert!(assure_private_dir(&p).is_err());
        fs::remove_file(&p).unwrap();
        fs::write(&p, []).unwrap();
        assert!(assure_private_dir(&p).is_err());
        assert!(validate_metadata(
            true,
            false,
            unsafe { libc::geteuid() }.wrapping_add(1),
            0o700
        )
        .is_err());
    }
    #[test]
    fn capture_file_removed_on_unwind() {
        let t = tempfile::tempdir().unwrap();
        let p = t.path().join("capture-test");
        fs::write(&p, b"synthetic").unwrap();
        let _ = std::panic::catch_unwind(|| {
            let _cleanup = RemoveOnDrop(p.clone());
            panic!("synthetic");
        });
        assert!(!p.exists());
    }
}
