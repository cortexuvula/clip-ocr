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
    use std::path::Path;

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

    pub fn languages() -> Result<Vec<String>, Failure> {
        // /usr/share/tessdata is the standard install location for
        // tesseract-ocr data files; also probe TESSDATA_PREFIX.
        let mut dirs = Vec::new();
        if let Ok(prefix) = std::env::var("TESSDATA_PREFIX") {
            dirs.push(Path::new(&prefix).to_path_buf());
        }
        dirs.push(Path::new("/usr/share/tessdata").to_path_buf());
        dirs.push(Path::new("/usr/local/share/tessdata").to_path_buf());
        let mut langs = Vec::new();
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if let Some(base) = name.strip_suffix(".traineddata") {
                    // osd is orientation data, not a usable OCR language.
                    if base != "osd" {
                        langs.push(base.to_owned());
                    }
                }
            }
        }
        langs.sort();
        langs.dedup();
        if langs.is_empty() {
            Err(Failure::Unsupported)
        } else {
            Ok(langs)
        }
    }

    pub fn recognize(png: &[u8], language: &str) -> Result<OcrResult, Failure> {
        let lang = normalize(language);
        // tesseract's API is builder-style: set_image_from_mem consumes self.
        let mut tess = tesseract::Tesseract::new(None, Some(&lang))
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
