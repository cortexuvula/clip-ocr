//! Only the native, local Vision engine ships in the macOS spike. No provider
//! resolution, remote overrides, helpers or IPC image files exist here.
#[cfg(target_os = "macos")]
use crate::core::OcrResult;
use crate::core::{EngineId, Failure, OcrEngine};
use std::sync::Arc;
// Vision candidate confidence is 0..1, NOT comparable to Tesseract's scale.
pub const VISION_MIN_CONFIDENCE: f64 = 0.5;
pub fn create(id: EngineId) -> Result<Arc<dyn OcrEngine>, Failure> {
    #[cfg(target_os = "macos")]
    if id == EngineId::MacVision {
        return Ok(Arc::new(MacVision));
    }
    let _ = id;
    Err(Failure::Unsupported)
}
pub fn languages() -> Result<Vec<String>, Failure> {
    #[cfg(target_os = "macos")]
    {
        mac::languages()
    }
    #[cfg(not(target_os = "macos"))]
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
