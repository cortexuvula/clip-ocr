//! Only the native, local Vision engine ships in the macOS spike. No provider
//! resolution, remote overrides, helpers or IPC image files exist here.
use crate::core::{EngineId, Failure, OcrEngine, OcrResult};
use std::sync::Arc;
// Vision candidate confidence is 0..1, NOT comparable to Tesseract's scale.
pub const VISION_MIN_CONFIDENCE: f64 = 0.5;
pub fn create(id: EngineId) -> Result<Arc<dyn OcrEngine>, Failure> {
    #[cfg(target_os = "macos")]
    if id == EngineId::MacVision {
        return Ok(Arc::new(MacVision));
    }
    #[cfg(target_os = "linux")]
    if id == EngineId::Tesseract {
        return Ok(Arc::new(TesseractEngine));
    }
    #[cfg(target_os = "windows")]
    if id == EngineId::WinRt {
        return Ok(Arc::new(WinRtEngine));
    }
    let _ = id;
    Err(Failure::Unsupported)
}
pub fn languages() -> Result<Vec<String>, Failure> {
    #[cfg(target_os = "macos")]
    {
        mac::languages()
    }
    #[cfg(target_os = "linux")]
    {
        linux::languages()
    }
    #[cfg(target_os = "windows")]
    {
        winrt::languages()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        Err(Failure::Unsupported)
    }
}
#[cfg(target_os = "macos")]
pub struct MacVision;
#[cfg(target_os = "macos")]
impl OcrEngine for MacVision {
    fn recognize(&self, png: &[u8], language: &str) -> Result<OcrResult, Failure> {
        mac::recognize(png, language)
    }
    fn confidence_threshold(&self) -> Option<f64> {
        Some(VISION_MIN_CONFIDENCE)
    }
}
#[cfg(target_os = "macos")]
mod mac {
    use super::*;
    use objc2::{rc::autoreleasepool, AnyThread, ClassType};
    use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
    };
    pub fn languages() -> Result<Vec<String>, Failure> {
        autoreleasepool(|_| {
            let request = VNRecognizeTextRequest::new();
            request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
            // SAFETY: a freshly initialized, thread-local request; framework returns
            // owned strings. No object is shared across worker threads.
            let languages = unsafe { request.supportedRecognitionLanguagesAndReturnError() }
                .map_err(|_| Failure::Engine)?;
            Ok(languages.iter().map(|s| s.to_string()).collect())
        })
    }
    pub fn recognize(png: &[u8], language: &str) -> Result<OcrResult, Failure> {
        autoreleasepool(|_| {
            let request = VNRecognizeTextRequest::new();
            request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
            request.setUsesLanguageCorrection(true);
            request.setRecognitionLanguages(&NSArray::from_retained_slice(&[NSString::from_str(
                language,
            )]));
            let data = NSData::with_bytes(png);
            let handler = VNImageRequestHandler::initWithData_options(
                VNImageRequestHandler::alloc(),
                &data,
                &NSDictionary::new(),
            );
            // Synchronous: perform, THEN inspect results. Caller is spawn_blocking.
            // Drop NSError without formatting it: it may contain image information.
            handler
                .performRequests_error(&NSArray::<VNRequest>::from_slice(&[request.as_super()]))
                .map_err(|_| Failure::Engine)?;
            let observations = request.results().ok_or(Failure::Engine)?;
            let mut lines = Vec::new();
            let mut confidences = Vec::new();
            for observation in observations.iter() {
                if let Some(candidate) = observation.topCandidates(1).firstObject() {
                    lines.push(candidate.string().to_string());
                    confidences.push(f64::from(candidate.confidence()));
                }
            }
            let confidence = (!confidences.is_empty())
                .then(|| confidences.iter().sum::<f64>() / confidences.len() as f64);
            Ok(OcrResult {
                text: lines.join("\n"),
                confidence,
            })
        })
    }
}
#[cfg(target_os = "linux")]
pub struct TesseractEngine;
#[cfg(target_os = "linux")]
impl OcrEngine for TesseractEngine {
    fn recognize(&self, png: &[u8], language: &str) -> Result<OcrResult, Failure> {
        linux::recognize(png, language)
    }
    fn confidence_threshold(&self) -> Option<f64> {
        // Tesseract exposes word-level confidences but the Rust binding does
        // not surface them per line; gate only on emptiness, not confidence.
        None
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;

    /// Tesseract's language codes are BCP-47-ish but NOT identical to the
    /// macOS Vision tags the config may carry (e.g. "en-US" vs "eng"). Map the
    /// common superset so a shared config.toml keeps working across platforms.
    fn normalize(code: &str) -> String {
        // tesseract-ocr uses "eng", "fra", "deu"... The first 3 chars of a tag
        // are the ISO 639-2/3 code tesseract expects; fall back to en.
        let base: String = code
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        let code = base.to_ascii_lowercase();
        match code.as_str() {
            "" => "eng".into(),
            "en" | "enus" | "us" => "eng".into(),
            "fra" | "fre" => "fra".into(),
            "deu" | "ger" => "deu".into(),
            _ => code,
        }
    }

    /// `TESSDATA_PREFIX`, if the environment sets it.
    fn env_prefix() -> Option<std::path::PathBuf> {
        std::env::var_os("TESSDATA_PREFIX").map(std::path::PathBuf::from)
    }

    pub fn languages() -> Result<Vec<String>, Failure> {
        let langs = crate::tessdata::languages(env_prefix().as_deref());
        if langs.is_empty() {
            Err(Failure::Unsupported)
        } else {
            Ok(langs)
        }
    }

    /// The data directory handed to libtesseract for `lang`, or `None` when
    /// nothing on this host holds `<lang>.traineddata` (the library's own
    /// resolution then applies).
    ///
    /// A bundled libtesseract carries its BUILD distro's compiled-in prefix —
    /// Debian's `/usr/share/tesseract-ocr/5/tessdata` — and that directory does
    /// not exist on Arch/Fedora, where the same files live in
    /// `/usr/share/tessdata`. With a NULL datapath the init therefore fails on
    /// those distributions and every capture reports `reason: engine` after the
    /// selection already succeeded. Always resolve explicitly; see
    /// `tests::an_unusable_tessdata_prefix_does_not_break_ocr`.
    pub(super) fn datapath_for(lang: &str) -> Option<String> {
        crate::tessdata::resolve(env_prefix().as_deref(), lang)
            .map(|p| p.to_string_lossy().into_owned())
    }

    pub fn recognize(png: &[u8], language: &str) -> Result<OcrResult, Failure> {
        let lang = normalize(language);
        let datapath: Option<String> = None; // TEMP: v0.4.1 bug, to prove the guard catches it
        let _ = datapath_for;
        // tesseract's API is builder-style: set_image_from_mem consumes self.
        let mut tess = tesseract::Tesseract::new(datapath.as_deref(), Some(&lang))
            .map_err(|_| Failure::Engine)?
            .set_image_from_mem(png)
            .map_err(|_| Failure::Engine)?;
        let text = tess.get_text().map_err(|_| Failure::Engine)?;
        Ok(OcrResult {
            text,
            confidence: None,
        })
    }
}

#[cfg(target_os = "windows")]
pub struct WinRtEngine;
#[cfg(target_os = "windows")]
impl OcrEngine for WinRtEngine {
    fn recognize(&self, png: &[u8], language: &str) -> Result<OcrResult, Failure> {
        winrt::recognize(png, language)
    }
    fn confidence_threshold(&self) -> Option<f64> {
        // This windows-rs binding does not surface per-word confidence, so gate
        // only on emptiness (same as Tesseract), never on a fabricated value.
        None
    }
}

#[cfg(target_os = "windows")]
mod winrt {
    use super::*;
    use windows::{
        Globalization::Language,
        Graphics::Imaging::{BitmapDecoder, SoftwareBitmap},
        Media::Ocr::OcrEngine,
        Storage::Streams::{DataWriter, InMemoryRandomAccessStream},
    };

    pub fn languages() -> Result<Vec<String>, Failure> {
        let languages = OcrEngine::AvailableRecognizerLanguages().map_err(|_| Failure::Engine)?;
        let mut out = Vec::new();
        for i in 0..languages.Size().map_err(|_| Failure::Engine)? {
            let lang = languages.GetAt(i).map_err(|_| Failure::Engine)?;
            out.push(lang.LanguageTag().map_err(|_| Failure::Engine)?.to_string());
        }
        Ok(out)
    }

    /// Decode a PNG into a SoftwareBitmap that Windows.Media.Ocr accepts.
    fn software_bitmap(png: &[u8]) -> Result<SoftwareBitmap, Failure> {
        let stream = InMemoryRandomAccessStream::new().map_err(|_| Failure::Engine)?;
        let writer = DataWriter::CreateDataWriter(&stream).map_err(|_| Failure::Engine)?;
        writer.WriteBytes(png).map_err(|_| Failure::Engine)?;
        writer
            .StoreAsync()
            .map_err(|_| Failure::Engine)?
            .join()
            .map_err(|_| Failure::Engine)?;
        writer
            .FlushAsync()
            .map_err(|_| Failure::Engine)?
            .join()
            .map_err(|_| Failure::Engine)?;
        stream.Seek(0).map_err(|_| Failure::Engine)?;
        let decoder = BitmapDecoder::CreateAsync(&stream)
            .map_err(|_| Failure::Engine)?
            .join()
            .map_err(|_| Failure::Engine)?;
        decoder
            .GetSoftwareBitmapAsync()
            .map_err(|_| Failure::Engine)?
            .join()
            .map_err(|_| Failure::Engine)
    }

    pub fn recognize(png: &[u8], language: &str) -> Result<OcrResult, Failure> {
        let bitmap = software_bitmap(png)?;
        // Prefer the requested language; fall back to user-profile languages so
        // a stale "en-US" config keeps working when the OS language differs.
        let engine = Language::CreateLanguage(&language.into())
            .ok()
            .and_then(|lang| OcrEngine::TryCreateFromLanguage(&lang).ok())
            .or_else(|| OcrEngine::TryCreateFromUserProfileLanguages().ok())
            .ok_or(Failure::Engine)?;
        let result = engine
            .RecognizeAsync(&bitmap)
            .map_err(|_| Failure::Engine)?
            .join()
            .map_err(|_| Failure::Engine)?;
        let lines = result.Lines().map_err(|_| Failure::Engine)?;
        let mut text = Vec::new();
        for i in 0..lines.Size().map_err(|_| Failure::Engine)? {
            let line = lines.GetAt(i).map_err(|_| Failure::Engine)?;
            if let Ok(t) = line.Text() {
                let t = t.to_string();
                if !t.trim().is_empty() {
                    text.push(t);
                }
            }
        }
        Ok(OcrResult {
            text: text.join("\n"),
            confidence: None,
        })
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    #[test]
    fn vision_enumerates_runtime_languages_and_recognizes_in_memory_blank_png() {
        let langs = languages().unwrap();
        assert!(langs.iter().any(|s| s == "en-US"));
        let image = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            500,
            120,
            image::Rgb([255, 255, 255]),
        ));
        let mut buffer = std::io::Cursor::new(Vec::new());
        image
            .write_to(&mut buffer, image::ImageFormat::Png)
            .unwrap();
        let engine = create(EngineId::MacVision).unwrap();
        let result = engine.recognize(buffer.get_ref(), "en-US").unwrap();
        assert!(result.text.is_empty());
        assert!(result.confidence.is_none());
    }
    #[test]
    fn vision_recognizes_synthetic_text_without_a_helper_or_capture_file() {
        // Tiny public-domain-style bitmap glyphs, expanded into a synthetic PNG.
        // Only synthetic pixels are used; never inspect a real desktop in tests.
        let glyphs: [[u8; 7]; 5] = [
            [17, 17, 17, 31, 17, 17, 17], // H
            [31, 16, 16, 30, 16, 16, 31], // E
            [16, 16, 16, 16, 16, 16, 31], // L
            [16, 16, 16, 16, 16, 16, 31], // L
            [14, 17, 17, 17, 17, 17, 14], // O
        ];
        let mut image = image::RgbImage::from_pixel(400, 120, image::Rgb([255, 255, 255]));
        for (g, glyph) in glyphs.iter().enumerate() {
            for (row, bits) in glyph.iter().enumerate() {
                for col in 0..5 {
                    if bits & (1 << (4 - col)) != 0 {
                        for dy in 0..8 {
                            for dx in 0..8 {
                                image.put_pixel(
                                    (40 + g * 56 + col * 8 + dx) as u32,
                                    (30 + row * 8 + dy) as u32,
                                    image::Rgb([0, 0, 0]),
                                );
                            }
                        }
                    }
                }
            }
        }
        let mut buffer = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut buffer, image::ImageFormat::Png)
            .unwrap();
        let result = create(EngineId::MacVision)
            .unwrap()
            .recognize(buffer.get_ref(), "en-US")
            .unwrap();
        assert!(
            result.text.to_uppercase().contains("HELLO"),
            "synthetic text recognition failed (content deliberately omitted)"
        );
        assert!(result.confidence.is_some_and(|v| (0.0..=1.0).contains(&v)));
    }
    #[test]
    fn non_shipped_allowlisted_engines_fail_explicitly() {
        assert!(matches!(
            create(EngineId::Tesseract),
            Err(Failure::Unsupported)
        ));
        assert!(matches!(create(EngineId::WinRt), Err(Failure::Unsupported)));
    }
}

#[cfg(all(test, target_os = "linux"))]
mod linux_tests {
    //! The Linux engine is the one place where the OCR library's own data-path
    //! resolution can silently disagree with the host. These tests run the REAL
    //! tesseract engine (CI installs `tesseract-ocr` + `tesseract-ocr-eng`) on
    //! synthetic pixels; no desktop, screen or traineddata of our own.
    use super::*;
    use crate::core::{CaptureDevice, CaptureError, Clipboard, Pipeline, Status};
    use std::sync::Mutex;
    use std::time::Duration;

    /// `TESSDATA_PREFIX` is process-global, so every test that reads or writes
    /// it takes this lock; `cargo test` runs these on parallel threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Assert the engine returned readable text.
    ///
    /// The fixture is a synthetic 5x7 bitmap, not a font, so tesseract reads it
    /// as roughly "HELLO" rather than exactly ("HELLDLDO" with the current
    /// glyphs). Exact spelling would be a change-detector on the engine's
    /// model, so the contract asserted here is the one that matters: the engine
    /// initialized with a resolved datapath and produced alphabetic text.
    fn assert_recognized_text(result: &OcrResult) {
        let letters: String = result.text.chars().filter(|c| c.is_alphabetic()).collect();
        assert!(
            letters.len() >= 5,
            "engine returned too little text to be a real recognition (content deliberately omitted)"
        );
        assert!(
            letters.contains('H') && letters.contains('L'),
            "engine text does not resemble the fixture glyphs (content deliberately omitted)"
        );
    }

    /// "HELLO" from synthetic bitmap glyphs, scaled so tesseract's layout
    /// analysis is comfortable. Only synthetic pixels — a test never captures
    /// or reads a real screen.
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

    /// Run `body` with `TESSDATA_PREFIX` pointed at an EMPTY directory for the
    /// duration, restoring whatever the host had afterwards.
    ///
    /// An empty prefix makes the library's own lookup fail on purpose, so a
    /// datapath that ClipOCR resolved itself is the only way OCR can succeed.
    fn with_unusable_tessdata_prefix<T>(body: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let empty = tempfile::tempdir().unwrap();
        let saved = std::env::var_os("TESSDATA_PREFIX");
        std::env::set_var("TESSDATA_PREFIX", empty.path());
        // The temp dir must outlive the closure; it is dropped after restore.
        let restored = std::panic::catch_unwind(std::panic::AssertUnwindSafe(body));
        match saved {
            Some(value) => std::env::set_var("TESSDATA_PREFIX", value),
            None => std::env::remove_var("TESSDATA_PREFIX"),
        }
        match restored {
            Ok(value) => value,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    #[test]
    fn tesseract_reads_synthetic_text_and_reports_no_fabricated_confidence() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let result = create(EngineId::Tesseract)
            .unwrap()
            .recognize(&synthetic_text_png(), "en-US")
            .expect("the Linux engine must initialize on a host with tessdata installed");
        assert_recognized_text(&result);
        // Same contract as WinRT: this binding surfaces no per-line confidence,
        // so the pipeline must never see a made-up number.
        assert_eq!(result.confidence, None);
    }

    /// Regression guard for v0.4.1 on Arch: the engine passed a NULL datapath,
    /// so libtesseract used the prefix compiled into its own build (Debian's
    /// `/usr/share/tesseract-ocr/5/tessdata`). On a distribution that keeps
    /// tessdata elsewhere the init failed and every capture reported
    /// `status: failed, reason: engine` after the drag had already worked.
    ///
    /// An unusable `TESSDATA_PREFIX` reproduces that condition on ANY host,
    /// including Debian: the library's own lookup cannot succeed, so this test
    /// only passes when ClipOCR resolved the directory itself and passed it in.
    #[test]
    fn an_unusable_tessdata_prefix_does_not_break_ocr() {
        let result = with_unusable_tessdata_prefix(|| {
            create(EngineId::Tesseract)
                .unwrap()
                .recognize(&synthetic_text_png(), "eng")
        });
        let result = result.expect(
            "OCR must not depend on the library's built-in data path: resolve tessdata explicitly",
        );
        assert_recognized_text(&result);
    }

    #[test]
    fn resolved_datapath_holds_the_requested_language() {
        let _guard = ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let datapath = linux::datapath_for("eng").expect("tessdata must be discoverable here");
        assert!(
            std::path::Path::new(&datapath)
                .join("eng.traineddata")
                .is_file(),
            "resolved directory must contain the language file, not merely exist"
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

    /// The shape the user reported: the capture succeeds, so the failure can
    /// only be in OCR. Runs the real engine through the real pipeline.
    #[tokio::test]
    async fn pipeline_copies_text_recognized_by_the_real_engine() {
        let clipboard = std::sync::Arc::new(Recorded::default());
        let pipeline = Pipeline::new(
            std::sync::Arc::new(Captured(synthetic_text_png())),
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
}
