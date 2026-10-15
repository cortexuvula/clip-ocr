#![cfg(unix)]
//! Real private sockets + CLI client + injected capture/engine/clipboard. No
//! desktop, permissions, live screenshots or network required.
use async_trait::async_trait;
use clipocr::{core::*, ipc};
use std::sync::{mpsc, Arc, Mutex};
struct Capture;
#[async_trait]
impl CaptureDevice for Capture {
    async fn capture(&self) -> Result<Vec<u8>, CaptureError> {
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(8, 8)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        Ok(png.into_inner())
    }
}
struct Engine {
    started: Arc<tokio::sync::Notify>,
    release: Mutex<mpsc::Receiver<()>>,
    text: &'static str,
}
impl OcrEngine for Engine {
    fn recognize(&self, _: &[u8], _: &str) -> Result<OcrResult, Failure> {
        self.started.notify_one();
        self.release.lock().unwrap().recv().unwrap();
        Ok(OcrResult {
            text: self.text.into(),
            confidence: None,
        })
    }
}
struct Clip {
    fail: bool,
    writes: Mutex<Vec<String>>,
}
impl Clipboard for Clip {
    fn write_text(&self, text: &str) -> Result<(), Failure> {
        if self.fail {
            return Err(Failure::Clipboard);
        }
        self.writes.lock().unwrap().push(text.into());
        Ok(())
    }
}
struct Handler(Arc<Pipeline>);
#[async_trait]
impl ipc::RequestHandler for Handler {
    async fn capture(&self) -> Outcome {
        self.0.run("en-US".into(), false, &()).await
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn simultaneous_cli_requests_report_real_terminal_clipboard_outcome() {
    for (text, fail, expected) in [
        ("synthetic text", false, Status::Copied),
        ("", false, Status::Empty),
        ("synthetic text", true, Status::Failed),
    ] {
        let root = tempfile::tempdir().unwrap();
        let listener = ipc::bind(root.path()).unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let (release_tx, release) = mpsc::channel();
        let clip = Arc::new(Clip {
            fail,
            writes: Mutex::new(vec![]),
        });
        let pipeline = Arc::new(Pipeline::new(
            Arc::new(Capture),
            Arc::new(Engine {
                started: started.clone(),
                release: Mutex::new(release),
                text,
            }),
            clip.clone(),
        ));
        let server = tokio::spawn(ipc::serve(listener, Arc::new(Handler(pipeline))));
        let path = root.path().to_owned();
        let first = tokio::task::spawn_blocking(move || ipc::client(&path));
        tokio::time::timeout(std::time::Duration::from_secs(2), started.notified())
            .await
            .unwrap();
        let path = root.path().to_owned();
        let second = tokio::task::spawn_blocking(move || ipc::client(&path))
            .await
            .unwrap();
        assert_eq!(second.status, Status::InProgress);
        assert_eq!(second.exit_code(), 5);
        release_tx.send(()).unwrap();
        let result = first.await.unwrap();
        assert_eq!(result.status, expected);
        assert_eq!(
            clip.writes.lock().unwrap().len(),
            usize::from(expected == Status::Copied)
        );
        if fail {
            assert_eq!(result.reason, Some(Failure::Clipboard));
            assert_ne!(result.exit_code(), 0);
        }
        server.abort();
        let _ = server.await;
    }
}
