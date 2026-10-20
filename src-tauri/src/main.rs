/// Detach this process from the console Windows created for a console-subsystem
/// launch, which closes the terminal window the login autostart entry would
/// otherwise leave sitting on the desktop for the whole session.
///
/// Detaching is deliberate rather than hiding a window: Windows 11 hosts the
/// console in Windows Terminal (`PseudoConsoleWindow` inside a separate
/// `CASCADIA_HOSTING_WINDOW_CLASS` window), so hiding the pseudo-console leaves
/// the terminal window visible. FreeConsole tears the whole host down without
/// touching a window this process does not own.
///
/// An inherited console is left alone: a `--capture-ocr` caller is waiting for
/// JSON on it, and its cold-start spawn shares that console rather than owning
/// one. Only the resident (no arguments) reaches this.
#[cfg(windows)]
fn detach_owned_console() {
    use windows::Win32::System::Console::{FreeConsole, GetConsoleProcessList};
    // SAFETY: both calls take no caller pointers beyond the fixed local array
    // and affect only this process's own console attachment.
    unsafe {
        // A single attached process means this console belongs to us alone.
        // More than one means it was inherited and is still serving the CLI.
        let mut attached = [0u32; 2];
        if GetConsoleProcessList(&mut attached) != 1 {
            return;
        }
        let _ = FreeConsole();
    }
}

#[cfg(not(windows))]
fn detach_owned_console() {}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 1 {
        // Resident launch (login autostart, or the cold-start spawn below).
        detach_owned_console();
    }
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
        #[cfg(windows)]
        finish(clipocr::ipc::named_client(&root));
        #[cfg(not(any(unix, windows)))]
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
