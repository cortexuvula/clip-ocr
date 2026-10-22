//! Linux-only tests use the real Tesseract engine, never a desktop or live pixels.
//! Environment-dependent tests re-exec exactly themselves in a private process.
//! A module mutex cannot protect the library or unrelated spawn_blocking readers.
use super::*;
use crate::core::{CaptureDevice, CaptureError, Clipboard, Pipeline, Status};
use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

const PREREQUISITE: &str = "Linux OCR tests require eng.traineddata: install tesseract-ocr and tesseract-ocr-eng (and libtesseract-dev/libleptonica-dev to build); tests must not skip when traineddata is missing";
const CHILD: &str = "CLIPOCR_TESSDATA_TEST_CHILD";
const MODEL_DIR: &str = "CLIPOCR_TESSDATA_TEST_MODEL_DIR";
// An English model under a synthetic language filename exercises selection of a
// non-default language without requiring/downloading another language package.
const ALIAS: &str = "clipocrtest";

fn require_english_data() -> PathBuf {
    crate::tessdata::resolve(None, "eng").expect(PREREQUISITE)
}

/// Returns true only in the parent, AFTER a successful, non-vacuous child run.
/// Command::env configures only the child; this test executable NEVER set_var's
/// or remove_var's its process environment, including on panic/async workers.
fn run_isolated(name: &str, prefix: &Path, models: Option<&Path>) -> bool {
    let full_name = format!("engine::linux_tests::{name}");
    let handshake = format!("isolated tessdata test entered: {full_name}");
    if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new(&full_name)) {
        println!("{handshake}");
        return false;
    }
    let mut child = Command::new(std::env::current_exe().expect("test executable path"));
    child
        .args(["--exact", &full_name, "--nocapture"])
        .env(CHILD, &full_name)
        .env("TESSDATA_PREFIX", prefix);
    if let Some(models) = models {
        child.env(MODEL_DIR, models);
    } else {
        child.env_remove(MODEL_DIR);
    }
    let output = child.output().expect("isolated Linux OCR test must launch");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    // Only test names and synthetic-file diagnostics. Recognized text is never
    // included in assert messages or output, even when recognition fails.
    print!("{stdout}");
    eprint!("{stderr}");
    assert!(
        stdout.contains(&handshake),
        "child filter must execute {full_name}, not silently run zero tests"
    );
    assert!(
        output.status.success(),
        "isolated Linux OCR test {full_name} failed"
    );
    true
}

fn assert_recognized_text(result: &OcrResult) {
    let letters: String = result.text.chars().filter(|c| c.is_alphabetic()).collect();
    assert!(
        letters.len() >= 5,
        "engine returned too little text (content deliberately omitted)"
    );
    assert!(
        letters.contains('H') && letters.contains('L'),
        "engine text does not resemble fixture glyphs (content deliberately omitted)"
    );
}

fn synthetic_text_png() -> Vec<u8> {
    const GLYPHS: [[u8; 7]; 5] = [
        [17, 17, 17, 31, 17, 17, 17], // H
        [31, 16, 16, 30, 16, 16, 31], // E
        [16, 16, 16, 16, 16, 16, 31], // L
        [16, 16, 16, 16, 16, 16, 31], // L
        [14, 17, 17, 17, 17, 17, 14], // O
    ];
    const SCALE: u32 = 16;
    let mut image = image::RgbImage::from_pixel(900, 220, image::Rgb([255, 255, 255]));
    for (glyph_index, glyph) in GLYPHS.iter().enumerate() {
        for (row, bits) in glyph.iter().enumerate() {
            for col in 0..5u32 {
                if bits & (1 << (4 - col)) == 0 {
                    continue;
                }
                for dy in 0..SCALE {
                    for dx in 0..SCALE {
                        image.put_pixel(
                            40 + glyph_index as u32 * 140 + col * SCALE + dx,
                            40 + row as u32 * SCALE + dy,
                            image::Rgb([0, 0, 0]),
                        );
                    }
                }
            }
        }
    }
    let mut buffer = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut buffer, image::ImageFormat::Png)
        .unwrap();
    buffer.into_inner()
}

#[test]
fn tesseract_reads_synthetic_text_and_reports_no_fabricated_confidence() {
    let data = require_english_data();
    if run_isolated(
        "tesseract_reads_synthetic_text_and_reports_no_fabricated_confidence",
        &data,
        None,
    ) {
        return;
    }
    let result = create(EngineId::Tesseract)
        .unwrap()
        .recognize(&synthetic_text_png(), "en-US")
        .expect("the Linux engine must initialize with the installed English model");
    assert_recognized_text(&result);
    assert_eq!(result.confidence, None);
}

#[test]
fn an_unusable_tessdata_prefix_does_not_break_ocr() {
    require_english_data();
    let empty = tempfile::tempdir().unwrap();
    if run_isolated(
        "an_unusable_tessdata_prefix_does_not_break_ocr",
        empty.path(),
        None,
    ) {
        return;
    }
    // Negative control: do NOT assume a library version honors an empty prefix.
    // Prove the implicit lookup is actually broken on this runner first.
    assert!(
        tesseract::Tesseract::new(None, Some("eng")).is_err(),
        "negative control: library default must fail for this empty-prefix fixture"
    );
    let result = create(EngineId::Tesseract)
        .unwrap()
        .recognize(&synthetic_text_png(), "eng")
        .expect("OCR must resolve tessdata explicitly when the library default is unusable");
    assert_recognized_text(&result);
}

#[test]
fn resolved_datapath_holds_the_requested_language() {
    let data = require_english_data();
    if run_isolated(
        "resolved_datapath_holds_the_requested_language",
        &data,
        None,
    ) {
        return;
    }
    let dirs = crate::tessdata::search_dirs(
        std::env::var_os("TESSDATA_PREFIX")
            .as_deref()
            .map(Path::new),
    );
    let datapath = linux::datapath_for(&dirs, "eng").expect(PREREQUISITE);
    assert!(
        Path::new(&datapath).join("eng.traineddata").is_file(),
        "resolved directory must contain the requested language, not merely exist"
    );
}

struct Captured(Vec<u8>);
#[async_trait::async_trait]
impl CaptureDevice for Captured {
    async fn capture(&self) -> Result<Vec<u8>, CaptureError> {
        Ok(self.0.clone())
    }
}
#[derive(Default)]
struct Recorded(std::sync::Mutex<String>);
impl Clipboard for Recorded {
    fn write_text(&self, text: &str) -> Result<(), Failure> {
        *self.0.lock().unwrap() = text.to_owned();
        Ok(())
    }
}

#[tokio::test]
async fn pipeline_copies_text_recognized_by_the_real_engine() {
    let data = require_english_data();
    if run_isolated(
        "pipeline_copies_text_recognized_by_the_real_engine",
        &data,
        None,
    ) {
        return;
    }
    let clipboard = Arc::new(Recorded::default());
    let pipeline = Pipeline::new(
        Arc::new(Captured(synthetic_text_png())),
        create(EngineId::Tesseract).unwrap(),
        clipboard.clone(),
    );
    let outcome = tokio::time::timeout(
        Duration::from_secs(30),
        pipeline.run("eng".into(), false, &()),
    )
    .await
    .expect("real OCR must finish inside the pipeline deadline");
    assert_eq!(
        outcome.status,
        Status::Copied,
        "expected copied, got {:?}",
        outcome.reason
    );
    assert!(outcome.chars > 0);
    let copied = clipboard.0.lock().unwrap().clone();
    assert!(!copied.trim().is_empty());
    assert!(copied.chars().filter(|c| c.is_alphabetic()).count() >= 5);
}

fn relocated_model(root: &Path) -> PathBuf {
    let source = require_english_data().join("eng.traineddata");
    let data = root.join("tessdata");
    std::fs::create_dir(&data).unwrap();
    std::fs::copy(source, data.join(format!("{ALIAS}.traineddata"))).unwrap();
    data
}
fn child_model_dir() -> PathBuf {
    std::env::var_os(MODEL_DIR)
        .map(PathBuf::from)
        .expect("isolated test model directory must be supplied")
}

#[test]
fn relocated_language_uses_explicit_datapath_even_when_prefix_is_a_file() {
    require_english_data();
    let root = tempfile::tempdir().unwrap();
    let data = relocated_model(root.path());
    let file = root.path().join("not-a-directory");
    std::fs::write(&file, b"synthetic").unwrap();
    if run_isolated(
        "relocated_language_uses_explicit_datapath_even_when_prefix_is_a_file",
        &file,
        Some(&data),
    ) {
        return;
    }
    let data = child_model_dir();
    assert!(
        tesseract::Tesseract::new(None, Some(ALIAS)).is_err(),
        "negative control: the library default must not load the relocated synthetic language"
    );
    // Model exists ONLY in <prefix>/tessdata; neither the static host layout nor
    // libtesseract's TESSDATA_PREFIX (a file) can find this alias.
    let dirs =
        crate::tessdata::search_dirs_in(data.parent(), &[], &root.path().join("missing-versions"));
    let result = linux::recognize_in(&synthetic_text_png(), ALIAS, &dirs)
        .expect("the resolved relocated model must reach Tesseract initialization");
    assert_recognized_text(&result);
}

#[test]
fn unresolved_discovery_preserves_the_working_library_default() {
    require_english_data();
    let root = tempfile::tempdir().unwrap();
    let data = relocated_model(root.path());
    if run_isolated(
        "unresolved_discovery_preserves_the_working_library_default",
        &data,
        Some(&data),
    ) {
        return;
    }
    // Simulate a layout outside ClipOCR's known directories. Tesseract can still
    // load a local model through its own lookup, so None fallback is intentional.
    assert!(linux::datapath_for(&[], ALIAS).is_none());
    let result = linux::recognize_in(&synthetic_text_png(), ALIAS, &[])
        .expect("no discovered match must leave the library's own valid lookup available");
    assert_recognized_text(&result);
}

#[test]
fn languages_reject_empty_trees_and_accept_only_the_first_usable_partial_tree() {
    let t = tempfile::tempdir().unwrap();
    let first = t.path().join("first");
    std::fs::create_dir(&first).unwrap();
    std::fs::write(first.join("osd.traineddata"), b"synthetic").unwrap();
    std::fs::create_dir(first.join("eng.traineddata")).unwrap();
    assert!(matches!(
        linux::languages_in(std::slice::from_ref(&first)),
        Err(Failure::Unsupported)
    ));
    let second = t.path().join("second");
    std::fs::create_dir(&second).unwrap();
    std::fs::write(second.join("fra.traineddata"), b"synthetic").unwrap();
    assert_eq!(
        linux::languages_in(&[first, second]),
        Ok(vec!["fra".into()])
    );
}

/// A config written for macOS Vision (`en-US`, `fr-CA`) or by hand (`de`) must
/// resolve to the traineddata file names tesseract actually ships. Before this
/// was pinned, only `en`/`fra`/`deu` were translated, so any other language
/// configured by its short code failed init and reported `status: failed,
/// reason: engine` — the exact symptom this suite exists to prevent.
#[test]
fn normalize_maps_configured_tags_to_traineddata_codes() {
    for (configured, expected) in [
        ("", "eng"),
        ("en", "eng"),
        ("en-US", "eng"),
        ("eng", "eng"),
        ("de", "deu"),
        ("de-DE", "deu"),
        ("deu", "deu"),
        ("ger", "deu"),
        ("fr", "fra"),
        ("fr-CA", "fra"),
        ("fre", "fra"),
        ("es", "spa"),
        ("es-419", "spa"),
        ("pt-BR", "por"),
        ("it", "ita"),
        ("nl", "nld"),
        ("ja", "jpn"),
        ("ko", "kor"),
        ("ru", "rus"),
        ("uk", "ukr"),
        ("el", "ell"),
        ("cs", "ces"),
        ("zh-Hans", "zh"),
        // Ambiguous-script codes are not guessed: sr (Cyrillic srp vs Latin
        // srp_latn) and zh must be configured as the traineddata code.
        ("sr", "sr"),
        ("sr-Latn", "sr"),
        // traineddata codes keep their underscore instead of being truncated
        ("chi_sim", "chi_sim"),
        ("chi_tra", "chi_tra"),
        ("srp_latn", "srp_latn"),
        // South Asian and other models, verified present in tessdata_best
        ("pa", "pan"),
        ("bn", "ben"),
        ("ta", "tam"),
        ("te", "tel"),
        ("kn", "kan"),
        ("ml", "mal"),
        ("mr", "mar"),
        ("gu", "guj"),
        ("ne", "nep"),
        ("si", "sin"),
        ("ur", "urd"),
        ("hy", "hye"),
        ("ka", "kat"),
        ("kk", "kaz"),
        ("uz", "uzb"),
        ("ms", "msa"),
        ("sw", "swa"),
        ("xx-YY", "xx"),
    ] {
        assert_eq!(
            linux::normalize(configured),
            expected,
            "configured language {configured:?}"
        );
    }
}

/// An unknown tag must fail loudly at init, never become English. Silently
/// OCRing in the wrong language would turn a missing language pack into
/// plausible-looking garbage on the clipboard.
#[test]
fn normalize_never_falls_back_to_english_for_an_unknown_tag() {
    for tag in ["xx", "tlh", "zh", "sr", "chi_sim", "srp_latn"] {
        let mapped = linux::normalize(tag);
        assert_ne!(
            mapped, "eng",
            "{tag:?} must not be rewritten to English as a silent fall-back"
        );
    }
}
