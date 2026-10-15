fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 2 && args[1] == "--help" {
        println!("ClipOCR: no args = resident tray utility; --capture-ocr = capture and wait for terminal JSON outcome.\nExit: 0 copied, 2 cancelled, 3 empty, 4 permission_denied, 5 in_progress, 6 failed, 7 low_confidence.\nDebug builds only: --capture-ocr-selftest (local interactive synthetic smoke test).");
        return;
    }
    let selftest = cfg!(debug_assertions) && args.len() == 2 && args[1] == "--capture-ocr-selftest";
    let capture = clipocr::ipc::wants_capture(&args);
    if args.len() != 1 && !capture && !selftest {
        finish(clipocr::core::Outcome::failed(
            clipocr::core::Failure::Unavailable,
        ));
    }
    let root = match clipocr::storage::data_dir() {
        Ok(root) => root,
        Err(e) => finish(clipocr::core::Outcome::failed(e)),
    };
    if clipocr::storage::assure_private_dir(&root).is_err() {
        finish(clipocr::core::Outcome::failed(
            clipocr::core::Failure::UnsafeStorage,
        ));
    }
    if capture {
        #[cfg(unix)]
        finish(clipocr::ipc::client(&root));
        #[cfg(not(unix))]
        finish(clipocr::core::Outcome::failed(
            clipocr::core::Failure::Unsupported,
        ));
    }
    if clipocr::desktop::run(root, selftest).is_err() {
        eprintln!("ClipOCR startup failed. Check private storage permissions and config.toml; see README troubleshooting.");
        std::process::exit(6);
    }
}
fn finish(out: clipocr::core::Outcome) -> ! {
    println!(
        "{}",
        serde_json::to_string(&out).expect("typed outcome serialization")
    );
    std::process::exit(out.exit_code());
}
