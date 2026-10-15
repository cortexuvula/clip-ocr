//! Desktop-independent orchestration. Only statuses/counts cross the UI/CLI boundary.
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::{
    io::Cursor,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

pub const MAX_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_DIMENSION: u32 = 16_384;
pub const MAX_PIXELS: u64 = 32_000_000;
pub const OCR_DEADLINE: Duration = Duration::from_secs(30);
pub const PERMISSION_LINK: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Copied,
    Cancelled,
    Empty,
    PermissionDenied,
    LowConfidence,
    InProgress,
    Failed,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub status: Status,
    pub chars: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<Failure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settings_url: Option<String>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Failure {
    Timeout,
    InvalidPng,
    Capture,
    Clipboard,
    Engine,
    Unsupported,
    UnsafeStorage,
    Unavailable,
}
impl Outcome {
    pub fn new(status: Status) -> Self {
        Self {
            status,
            chars: 0,
            reason: None,
            settings_url: (status == Status::PermissionDenied).then(|| PERMISSION_LINK.into()),
        }
    }
    pub fn failed(reason: Failure) -> Self {
        Self {
            reason: Some(reason),
            ..Self::new(Status::Failed)
        }
    }
    pub fn exit_code(&self) -> i32 {
        match self.status {
            Status::Copied => 0,
            Status::Cancelled => 2,
            Status::Empty => 3,
            Status::PermissionDenied => 4,
            Status::InProgress => 5,
            Status::Failed => 6,
            Status::LowConfidence => 7,
        }
    }
}
#[derive(Debug, Clone, Copy)]
pub enum CaptureError {
    Cancelled,
    PermissionDenied,
    Failed(Failure),
}
impl From<CaptureError> for Outcome {
    fn from(e: CaptureError) -> Self {
        match e {
            CaptureError::Cancelled => Self::new(Status::Cancelled),
            CaptureError::PermissionDenied => Self::new(Status::PermissionDenied),
            CaptureError::Failed(e) => Self::failed(e),
        }
    }
}
// The allowlist is encoded in the type: arbitrary providers cannot deserialize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EngineId {
    #[serde(rename = "macVision")]
    MacVision,
    #[serde(rename = "Tesseract")]
    Tesseract,
    #[serde(rename = "WinRT")]
    WinRt,
}
// Deliberately not Debug/Serialize: never emit recognized text into diagnostics.
pub struct OcrResult {
    pub text: String,
    pub confidence: Option<f64>,
}
pub trait OcrEngine: Send + Sync {
    fn recognize(&self, png: &[u8], language: &str) -> Result<OcrResult, Failure>;
    fn confidence_threshold(&self) -> Option<f64> {
        None
    }
}
#[async_trait]
pub trait CaptureDevice: Send + Sync {
    async fn capture(&self) -> Result<Vec<u8>, CaptureError>;
}
pub trait Clipboard: Send + Sync {
    fn write_text(&self, text: &str) -> Result<(), Failure>;
}
pub trait Progress: Send {}
impl Progress for () {}
pub trait Feedback: Send + Sync {
    fn recognizing(&self) -> Box<dyn Progress> {
        Box::new(())
    }
}
impl Feedback for () {}

/// Owned atomic guard survives async moves and releases on unwind/cancellation.
pub struct InFlightGuard(Arc<AtomicBool>);
impl InFlightGuard {
    pub fn acquire(flag: &Arc<AtomicBool>) -> Option<Self> {
        flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| Self(flag.clone()))
    }
}
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

pub struct Pipeline {
    capture: Arc<dyn CaptureDevice>,
    engine: Arc<dyn OcrEngine>,
    clipboard: Arc<dyn Clipboard>,
    pub in_flight: Arc<AtomicBool>,
    engine_live: Arc<AtomicBool>,
    deadline: Duration,
}
impl Pipeline {
    pub fn new(
        capture: Arc<dyn CaptureDevice>,
        engine: Arc<dyn OcrEngine>,
        clipboard: Arc<dyn Clipboard>,
    ) -> Self {
        Self {
            capture,
            engine,
            clipboard,
            in_flight: Arc::default(),
            engine_live: Arc::default(),
            deadline: OCR_DEADLINE,
        }
    }
    pub fn busy(&self) -> bool {
        self.in_flight.load(Ordering::SeqCst) || self.engine_live.load(Ordering::SeqCst)
    }
    pub async fn run(&self, language: String, collapse: bool, feedback: &dyn Feedback) -> Outcome {
        let Some(_flight) = InFlightGuard::acquire(&self.in_flight) else {
            return Outcome::new(Status::InProgress);
        };
        // A timed-out native call cannot be killed safely. Keep a SECOND lease in
        // that worker; release the request guard but reject further captures until
        // the worker exits. Never accumulate live engines or overlap selection.
        let Some(engine_lease) = InFlightGuard::acquire(&self.engine_live) else {
            return Outcome::new(Status::InProgress);
        };
        let png = match self.capture.capture().await {
            Ok(png) => png,
            Err(e) => return e.into(),
        };
        let _progress = feedback.recognizing();
        let engine = self.engine.clone();
        let threshold = engine.confidence_threshold();
        let task = tokio::task::spawn_blocking(move || {
            let _lease = engine_lease;
            validate_png(&png).map_err(|_| Failure::InvalidPng)?;
            engine.recognize(&png, &language)
        });
        let result = match tokio::time::timeout(self.deadline, task).await {
            Err(_) => return Outcome::failed(Failure::Timeout),
            Ok(Err(_)) => return Outcome::failed(Failure::Engine),
            Ok(Ok(Err(e))) => return Outcome::failed(e),
            Ok(Ok(Ok(result))) => result,
        };
        let text = if collapse {
            result.text.split_whitespace().collect::<Vec<_>>().join(" ")
        } else {
            result.text.trim().to_owned()
        };
        if text.is_empty() {
            return Outcome::new(Status::Empty);
        }
        if let (Some(confidence), Some(minimum)) = (result.confidence, threshold) {
            if !confidence.is_finite() || confidence < minimum {
                return Outcome::new(Status::LowConfidence);
            }
        }
        if self.clipboard.write_text(&text).is_err() {
            return Outcome::failed(Failure::Clipboard);
        }
        Outcome {
            chars: text.chars().count(),
            ..Outcome::new(Status::Copied)
        }
    }
}

pub fn validate_png(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.len() > MAX_BYTES || !crate::geometry::is_png(bytes) {
        return Err("invalid_png");
    }
    // Dimensions are checked BEFORE allocating a decoded image. Decoder limits also
    // bound its internal allocations; a successful signature alone proves nothing.
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DIMENSION);
    limits.max_image_height = Some(MAX_DIMENSION);
    limits.max_alloc = Some(MAX_PIXELS * 8);
    reader.limits(limits);
    let (width, height) =
        image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Png)
            .into_dimensions()
            .map_err(|_| "invalid_png")?;
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width) * u64::from(height) > MAX_PIXELS
    {
        return Err("invalid_png");
    }
    reader.decode().map_err(|_| "invalid_png")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{atomic::AtomicUsize, Mutex};
    pub fn png() -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(2, 2)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }
    #[test]
    fn truncated_png_is_rejected_before_engine() {
        assert!(validate_png(b"\x89PNG\r\n\x1a\n").is_err());
    }
    #[test]
    fn oversized_input_is_rejected_before_engine() {
        let mut bytes = vec![0; MAX_BYTES + 1];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        assert!(validate_png(&bytes).is_err());
    }
    #[test]
    fn valid_png_and_oversized_dimensions() {
        assert!(validate_png(&png()).is_ok());
        let mut buffer = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(MAX_DIMENSION + 1, 1)
            .write_to(&mut buffer, image::ImageFormat::Png)
            .unwrap();
        let bytes = buffer.into_inner();
        assert_eq!(
            image::ImageReader::with_format(Cursor::new(&bytes), image::ImageFormat::Png)
                .into_dimensions()
                .unwrap(),
            (MAX_DIMENSION + 1, 1)
        );
        assert!(validate_png(&bytes).is_err());
        let mut truncated = png();
        truncated.truncate(truncated.len() / 2);
        assert!(validate_png(&truncated).is_err());
    }
    struct Capture {
        bytes: Vec<u8>,
        count: Arc<AtomicUsize>,
        error: Option<CaptureError>,
    }
    #[async_trait]
    impl CaptureDevice for Capture {
        async fn capture(&self) -> Result<Vec<u8>, CaptureError> {
            self.count.fetch_add(1, Ordering::SeqCst);
            self.error.map_or_else(|| Ok(self.bytes.clone()), Err)
        }
    }
    struct Engine {
        text: &'static str,
        confidence: Option<f64>,
        delay: Duration,
        count: Arc<AtomicUsize>,
        panic: bool,
    }
    impl OcrEngine for Engine {
        fn recognize(&self, _: &[u8], _: &str) -> Result<OcrResult, Failure> {
            self.count.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            assert!(!self.panic, "synthetic engine panic");
            Ok(OcrResult {
                text: self.text.into(),
                confidence: self.confidence,
            })
        }
        fn confidence_threshold(&self) -> Option<f64> {
            Some(0.5)
        }
    }
    #[derive(Default)]
    struct Clip {
        text: Mutex<String>,
        fail: bool,
    }
    impl Clipboard for Clip {
        fn write_text(&self, text: &str) -> Result<(), Failure> {
            if self.fail {
                return Err(Failure::Clipboard);
            }
            *self.text.lock().unwrap() = text.into();
            Ok(())
        }
    }
    fn fixture(
        text: &'static str,
        confidence: Option<f64>,
        fail: bool,
    ) -> (Pipeline, Arc<Clip>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let captures = Arc::new(AtomicUsize::new(0));
        let engines = Arc::new(AtomicUsize::new(0));
        let clip = Arc::new(Clip {
            fail,
            ..Clip::default()
        });
        (
            Pipeline::new(
                Arc::new(Capture {
                    bytes: png(),
                    count: captures.clone(),
                    error: None,
                }),
                Arc::new(Engine {
                    text,
                    confidence,
                    delay: Duration::ZERO,
                    count: engines.clone(),
                    panic: false,
                }),
                clip.clone(),
            ),
            clip,
            captures,
            engines,
        )
    }
    #[tokio::test]
    async fn copies_text_only_and_collapses_lines() {
        let (p, clip, _, _) = fixture("hello\nworld", Some(0.9), false);
        let out = p.run("en-US".into(), true, &()).await;
        assert_eq!(out.status, Status::Copied);
        assert_eq!(out.chars, 11);
        assert_eq!(*clip.text.lock().unwrap(), "hello world");
    }
    #[tokio::test]
    async fn no_confidence_can_never_be_low_confidence() {
        let (p, _, _, _) = fixture("hello", None, false);
        assert_eq!(p.run("en".into(), false, &()).await.status, Status::Copied);
    }
    #[tokio::test]
    async fn empty_low_confidence_and_clipboard_failure_do_not_copy() {
        for (text, confidence, fail, expected) in [
            (" ", None, false, Status::Empty),
            ("text", Some(0.1), false, Status::LowConfidence),
            ("text", None, true, Status::Failed),
        ] {
            let (p, clip, _, _) = fixture(text, confidence, fail);
            let out = p.run("en".into(), false, &()).await;
            assert_eq!(out.status, expected);
            assert!(clip.text.lock().unwrap().is_empty());
            if fail {
                assert_eq!(out.reason, Some(Failure::Clipboard));
            }
        }
    }
    #[tokio::test]
    async fn malformed_capture_never_reaches_engine() {
        let (mut p, _, _, engines) = fixture("text", None, false);
        p.capture = Arc::new(Capture {
            bytes: b"\x89PNG\r\n\x1a\n".to_vec(),
            count: Arc::default(),
            error: None,
        });
        assert_eq!(
            p.run("en".into(), false, &()).await.reason,
            Some(Failure::InvalidPng)
        );
        assert_eq!(engines.load(Ordering::SeqCst), 0);
    }
    #[tokio::test]
    async fn capture_outcomes_are_typed() {
        for (error, status) in [
            (CaptureError::Cancelled, Status::Cancelled),
            (CaptureError::PermissionDenied, Status::PermissionDenied),
            (CaptureError::Failed(Failure::Capture), Status::Failed),
        ] {
            let (mut p, _, _, engines) = fixture("text", None, false);
            p.capture = Arc::new(Capture {
                bytes: vec![],
                count: Arc::default(),
                error: Some(error),
            });
            let out = p.run("en".into(), false, &()).await;
            assert_eq!(out.status, status);
            assert_eq!(engines.load(Ordering::SeqCst), 0);
            assert!(!p.busy());
            if status == Status::PermissionDenied {
                assert_eq!(out.settings_url.as_deref(), Some(PERMISSION_LINK));
            }
        }
    }
    #[tokio::test]
    async fn hung_engine_times_out_releases_request_but_prevents_overlap() {
        let (mut p, _, captures, engines) = fixture("text", None, false);
        p.deadline = Duration::from_millis(10);
        p.engine = Arc::new(Engine {
            text: "text",
            confidence: None,
            delay: Duration::from_millis(150),
            count: engines,
            panic: false,
        });
        assert_eq!(
            p.run("en".into(), false, &()).await.reason,
            Some(Failure::Timeout)
        );
        assert!(!p.in_flight.load(Ordering::SeqCst));
        assert!(p.busy());
        assert_eq!(
            p.run("en".into(), false, &()).await.status,
            Status::InProgress
        );
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        // The engine thread releases its lease asynchronously; poll for it to
        // clear rather than sleeping a fixed margin (a fixed sleep flaked on
        // loaded CI runners where the 150ms engine delay + scheduling could
        // exceed the 180ms sleep).
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while p.busy() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!p.busy());
    }
    #[tokio::test]
    async fn engine_panic_releases_both_leases() {
        let (mut p, _, _, engines) = fixture("text", None, false);
        p.engine = Arc::new(Engine {
            text: "",
            confidence: None,
            delay: Duration::ZERO,
            count: engines,
            panic: true,
        });
        assert_eq!(
            p.run("en".into(), false, &()).await.reason,
            Some(Failure::Engine)
        );
        assert!(!p.busy());
    }
    #[test]
    fn guard_releases_on_panic_and_rejects_second_acquire() {
        let flag = Arc::default();
        let _ = std::panic::catch_unwind(|| {
            let _guard = InFlightGuard::acquire(&flag).unwrap();
            assert!(InFlightGuard::acquire(&flag).is_none());
            panic!("synthetic");
        });
        assert!(!flag.load(Ordering::SeqCst));
    }
    #[test]
    fn outcome_contract_has_all_statuses_and_no_content() {
        for (status, name, code) in [
            (Status::Copied, "copied", 0),
            (Status::Cancelled, "cancelled", 2),
            (Status::Empty, "empty", 3),
            (Status::PermissionDenied, "permission_denied", 4),
            (Status::InProgress, "in_progress", 5),
            (Status::Failed, "failed", 6),
            (Status::LowConfidence, "low_confidence", 7),
        ] {
            let out = Outcome::new(status);
            let json = serde_json::to_value(&out).unwrap();
            assert_eq!(json["status"], name);
            assert_eq!(out.exit_code(), code);
            assert!(json.get("text").is_none());
        }
    }
    #[test]
    fn remote_engine_is_rejected() {
        assert!(serde_json::from_str::<EngineId>("\"https://remote.example\"").is_err());
    }
}
