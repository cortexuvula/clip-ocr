//! Only native, on-device engines ship: Apple Vision (macOS), Tesseract (Linux)
//! and `Windows.Media.Ocr` (Windows). No provider resolution, remote overrides,
//! helpers or IPC image files exist here.
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

    /// Map a configured language tag onto the code tesseract's traineddata
    /// files are named after (ISO 639-2/T), so a config written for macOS
    /// Vision keeps working on Linux: `en-US`, `fr-CA` and `de` all have to
    /// find `eng`, `fra` and `deu`.
    ///
    /// Only the primary BCP-47 subtag is considered (`fr-CA` → `fr`), while a
    /// `traineddata` code keeps its underscore (`chi_sim` → itself).
    ///
    /// An unrecognised tag is returned with only its primary subtag (`xx-YY` →
    /// `xx`) so the engine fails loudly at init. It is never rewritten to
    /// English: copying text in the wrong language is worse than copying
    /// nothing, and a silent fallback would turn a missing language pack into
    /// plausible-looking garbage. Codes that name more than one model are
    /// deliberately not guessed — `zh` would have to pick chi_sim or chi_tra,
    /// and `sr` would have to pick srp (Cyrillic) or srp_latn (Latin), where
    /// guessing puts the wrong *script* on the clipboard with no error. Those
    /// must be configured as the traineddata code itself.
    pub(super) fn normalize(code: &str) -> String {
        let primary = code
            .split('-')
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase();
        let mapped = match primary.as_str() {
            "" | "en" | "us" => "eng",
            "af" => "afr",
            "ar" => "ara",
            "bg" => "bul",
            "bn" => "ben",
            "ca" => "cat",
            "cs" => "ces",
            "cy" => "cym",
            "da" => "dan",
            "de" | "deu" | "ger" => "deu",
            "el" => "ell",
            "es" => "spa",
            "et" => "est",
            "eu" => "eus",
            "fa" => "fas",
            "fi" => "fin",
            "fr" | "fra" | "fre" => "fra",
            "ga" => "gle",
            "gl" => "glg",
            "gu" => "guj",
            "he" => "heb",
            "hi" => "hin",
            "hr" => "hrv",
            "hu" => "hun",
            "hy" => "hye",
            "id" => "ind",
            "is" => "isl",
            "it" => "ita",
            "ja" => "jpn",
            "ka" => "kat",
            "kk" => "kaz",
            "km" => "khm",
            "kn" => "kan",
            "ko" => "kor",
            "ky" => "kir",
            "lo" => "lao",
            "lt" => "lit",
            "lv" => "lav",
            "mk" => "mkd",
            "ml" => "mal",
            "mn" => "mon",
            "mr" => "mar",
            "ms" => "msa",
            "mt" => "mlt",
            "my" => "mya",
            "ne" => "nep",
            "nl" => "nld",
            "no" => "nor",
            "pa" => "pan",
            "pl" => "pol",
            "pt" => "por",
            "ro" => "ron",
            "ru" => "rus",
            "si" => "sin",
            "sk" => "slk",
            "sl" => "slv",
            "sq" => "sqi",
            "sv" => "swe",
            "sw" => "swa",
            "ta" => "tam",
            "te" => "tel",
            "th" => "tha",
            "tl" => "fil",
            "tr" => "tur",
            "uk" => "ukr",
            "ur" => "urd",
            "uz" => "uzb",
            "vi" => "vie",
            other => return other.to_owned(),
        };
        mapped.to_owned()
    }

    /// `TESSDATA_PREFIX`, if the environment sets it.
    fn env_prefix() -> Option<std::path::PathBuf> {
        std::env::var_os("TESSDATA_PREFIX").map(std::path::PathBuf::from)
    }

    pub fn languages() -> Result<Vec<String>, Failure> {
        languages_in(&crate::tessdata::search_dirs(env_prefix().as_deref()))
    }

    pub(super) fn languages_in(dirs: &[std::path::PathBuf]) -> Result<Vec<String>, Failure> {
        let langs = crate::tessdata::languages_in(dirs);
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
    /// `linux_tests::an_unusable_tessdata_prefix_does_not_break_ocr`.
    pub(super) fn datapath_for(dirs: &[std::path::PathBuf], lang: &str) -> Option<String> {
        crate::tessdata::resolve_in(dirs, lang).map(|p| p.to_string_lossy().into_owned())
    }

    pub fn recognize(png: &[u8], language: &str) -> Result<OcrResult, Failure> {
        recognize_in(
            png,
            language,
            &crate::tessdata::search_dirs(env_prefix().as_deref()),
        )
    }

    /// The same initialization/recognition path, with host discovery supplied by
    /// the caller. No engine override: always the local Tesseract implementation.
    pub(super) fn recognize_in(
        png: &[u8],
        language: &str,
        dirs: &[std::path::PathBuf],
    ) -> Result<OcrResult, Failure> {
        let lang = normalize(language);
        let datapath = datapath_for(dirs, &lang);
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
#[path = "engine_linux_tests.rs"]
mod linux_tests;
