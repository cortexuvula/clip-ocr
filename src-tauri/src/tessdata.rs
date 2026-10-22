//! Tesseract language-data (tessdata) discovery.
//!
//! The tesseract C library resolves its data directory from `TESSDATA_PREFIX`
//! or from a path compiled into the library itself. A library built for one
//! distro carries that distro's path — Debian/Ubuntu builds hardcode
//! `/usr/share/tesseract-ocr/5/tessdata`, while Arch/Fedora keep the same files
//! in `/usr/share/tessdata`. A bundled AppImage ships the build host's
//! `libtesseract.so`, so `TessBaseAPIInit3(NULL, "eng")` looks in a directory
//! that does not exist on the user's distro and OCR fails with "Failed loading
//! language" even though the system tessdata is right there.
//!
//! Callers therefore resolve an explicit data directory at runtime and pass it
//! to `Tesseract::new(Some(dir), …)`. Discovery is deliberately filesystem-only
//! (no liblept/libtesseract call) so it stays testable on every platform.

use std::path::{Path, PathBuf};

/// Distro/prefix layouts seen in the wild, most common first. Every entry is
/// the tessdata directory itself, matching `TessBaseAPIInit3`'s datapath.
pub const CANDIDATE_DIRS: &[&str] = &[
    // Arch, Fedora, openSUSE, most rolling distributions.
    "/usr/share/tessdata",
    // Debian/Ubuntu — also the prefix compiled into the bundled library, so
    // listing it keeps Debian/Ubuntu hosts working unchanged.
    "/usr/share/tesseract-ocr/5/tessdata",
    "/usr/share/tesseract-ocr/4.00/tessdata",
    "/usr/share/tesseract-ocr/tessdata",
    // Local/source installs and package managers that do not use /usr.
    "/usr/local/share/tessdata",
    "/opt/homebrew/share/tessdata",
    "/run/current-system/sw/share/tessdata",
];

/// Parent of per-version tessdata trees (`/usr/share/tesseract-ocr/*/tessdata`).
/// Scanned at runtime so a distribution that bumps its version directory keeps
/// working without a release.
const VERSIONED_PARENT: &str = "/usr/share/tesseract-ocr";

/// Tesseract's orientation data is not a usable OCR language.
const NON_LANGUAGE: &str = "osd";

/// A language code reaches a file path, so refuse anything that is not a plain
/// file name component.
fn is_plain_language(lang: &str) -> bool {
    !lang.is_empty()
        && !lang.starts_with('.')
        && lang
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Does `dir` hold the traineddata for `lang`?
pub fn has_language(dir: &Path, lang: &str) -> bool {
    is_plain_language(lang) && dir.join(format!("{lang}.traineddata")).is_file()
}

/// Parse `TESSDATA_PREFIX` the way tesseract does, accepting both the tessdata
/// directory itself and a parent that contains a `tessdata` child.
fn prefix_candidates(prefix: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![prefix.to_path_buf()];
    let nested = prefix.join("tessdata");
    if nested != prefix {
        dirs.push(nested);
    }
    dirs
}

/// Every directory worth searching, in priority order: the environment
/// override (if any), then the static candidates, then version directories
/// discovered under `VERSIONED_PARENT`.
pub fn search_dirs(env_prefix: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(prefix) = env_prefix {
        dirs.extend(prefix_candidates(prefix));
    }
    dirs.extend(CANDIDATE_DIRS.iter().map(PathBuf::from));
    if let Ok(entries) = std::fs::read_dir(VERSIONED_PARENT) {
        for entry in entries.flatten() {
            let dir = entry.path().join("tessdata");
            if dir.is_dir() {
                dirs.push(dir);
            }
        }
    }
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

/// First directory that actually contains `lang`, if any.
pub fn resolve_in(dirs: &[PathBuf], lang: &str) -> Option<PathBuf> {
    dirs.iter().find(|d| has_language(d, lang)).cloned()
}

/// Runtime data directory for `lang`.
pub fn resolve(env_prefix: Option<&Path>, lang: &str) -> Option<PathBuf> {
    resolve_in(&search_dirs(env_prefix), lang)
}

/// Languages installed in the first directory that has any, so the Settings
/// list reflects what the engine can actually load rather than a union of
/// half-populated trees.
pub fn languages_in(dirs: &[PathBuf]) -> Vec<String> {
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut langs: Vec<String> = entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                let base = name.strip_suffix(".traineddata")?.to_owned();
                (base != NON_LANGUAGE && is_plain_language(&base)).then_some(base)
            })
            .collect();
        if !langs.is_empty() {
            langs.sort();
            langs.dedup();
            return langs;
        }
    }
    Vec::new()
}

/// Languages available to the engine at runtime.
pub fn languages(env_prefix: Option<&Path>) -> Vec<String> {
    languages_in(&search_dirs(env_prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(dir: &Path, names: &[&str]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        for name in names {
            std::fs::write(dir.join(format!("{name}.traineddata")), b"synthetic").unwrap();
        }
        dir.to_path_buf()
    }

    #[test]
    fn resolves_the_directory_that_holds_the_language_not_merely_the_first() {
        let t = tempfile::tempdir().unwrap();
        let empty = dir_with(&t.path().join("empty"), &[]);
        let other = dir_with(&t.path().join("other"), &["deu"]);
        let want = dir_with(&t.path().join("want"), &["eng", "osd"]);
        let dirs = vec![empty, other, want.clone()];
        assert_eq!(resolve_in(&dirs, "eng").as_deref(), Some(want.as_path()));
        // A language nobody has resolves to nothing, never to a wrong path.
        assert_eq!(resolve_in(&dirs, "fra"), None);
    }

    #[test]
    fn environment_prefix_may_be_the_tessdata_dir_or_its_parent() {
        let t = tempfile::tempdir().unwrap();
        let inner = dir_with(&t.path().join("prefix").join("tessdata"), &["eng"]);

        // Parent form: <prefix>/tessdata.
        assert_eq!(
            resolve(Some(&t.path().join("prefix")), "eng").as_deref(),
            Some(inner.as_path())
        );
        // Directory form: <prefix> is already the tessdata dir.
        assert_eq!(
            resolve(Some(&inner), "eng").as_deref(),
            Some(inner.as_path())
        );
    }

    #[test]
    fn unresolvable_language_leaves_the_library_default_in_place() {
        let t = tempfile::tempdir().unwrap();
        // No traineddata anywhere under this prefix, so callers must fall back
        // to the library's own resolution rather than inventing a path.
        let dirs = prefix_candidates(&t.path().join("nothing"));
        assert_eq!(resolve_in(&dirs, "eng"), None);
    }

    #[test]
    fn language_names_cannot_escape_the_tessdata_directory() {
        for bad in ["", "..", "../eng", "eng/../../etc", ".hidden", "e.g"] {
            assert!(
                !has_language(Path::new("/usr/share/tessdata"), bad),
                "{bad}"
            );
        }
    }

    #[test]
    fn languages_skip_orientation_data_and_report_the_first_populated_tree() {
        let t = tempfile::tempdir().unwrap();
        let first = dir_with(&t.path().join("first"), &["eng", "osd"]);
        let second = dir_with(&t.path().join("second"), &["fra", "deu"]);
        let langs = languages_in(&[first, second]);
        assert_eq!(langs, vec!["eng".to_owned()]);
        assert_eq!(languages_in(&[]), Vec::<String>::new());
    }
}
