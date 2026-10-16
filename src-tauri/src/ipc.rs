//! Local request/reply supplements Tauri's single-instance plugin: that plugin's
//! fire-and-forget callback cannot provide the CLI's terminal exit contract.
use crate::core::{Failure, Outcome};
use async_trait::async_trait;
#[cfg(windows)]
use std::path::PathBuf;
use std::{path::Path, sync::Arc, time::Duration};
pub const REQUEST: &[u8] = b"--capture-ocr\n";
pub fn wants_capture(args: &[String]) -> bool {
    args.len() == 2 && args[1] == "--capture-ocr"
}
#[async_trait]
pub trait RequestHandler: Send + Sync {
    async fn capture(&self) -> Outcome;
}

#[cfg(unix)]
pub fn bind(root: &Path) -> Result<std::os::unix::net::UnixListener, Failure> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let path = root.join("cli.sock");
    if let Ok(meta) = std::fs::symlink_metadata(&path) {
        if !meta.file_type().is_socket()
            || meta.uid() != unsafe { libc::geteuid() }
            || meta.mode() & 0o777 != 0o600
        {
            return Err(Failure::UnsafeStorage);
        }
        std::fs::remove_file(&path).map_err(|_| Failure::Unavailable)?;
    }
    let listener =
        std::os::unix::net::UnixListener::bind(&path).map_err(|_| Failure::Unavailable)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
        .map_err(|_| Failure::UnsafeStorage)?;
    listener
        .set_nonblocking(true)
        .map_err(|_| Failure::Unavailable)?;
    Ok(listener)
}
#[cfg(unix)]
pub async fn serve(listener: std::os::unix::net::UnixListener, handler: Arc<dyn RequestHandler>) {
    let Ok(listener) = tokio::net::UnixListener::from_std(listener) else {
        return;
    };
    let slots = Arc::new(tokio::sync::Semaphore::new(16));
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            drop(stream);
            continue;
        };
        let handler = handler.clone();
        tokio::spawn(async move {
            let _permit = permit;
            respond(stream, handler).await;
        });
    }
}
#[cfg(unix)]
async fn respond(mut stream: tokio::net::UnixStream, handler: Arc<dyn RequestHandler>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut request = Vec::new();
    let read = tokio::time::timeout(
        Duration::from_secs(2),
        (&mut stream).take(64).read_to_end(&mut request),
    )
    .await;
    let out = if matches!(read, Ok(Ok(_))) && request == REQUEST {
        // Catch unexpected orchestration panic as a failed terminal response.
        match tokio::spawn(async move { handler.capture().await }).await {
            Ok(out) => out,
            Err(_) => Outcome::failed(Failure::Engine),
        }
    } else {
        Outcome::failed(Failure::Unavailable)
    };
    if let Ok(mut bytes) = serde_json::to_vec(&out) {
        bytes.push(b'\n');
        let _ = stream.write_all(&bytes).await;
    }
}
#[cfg(unix)]
fn connect_or_launch(
    root: &Path,
    launch: impl FnOnce() -> std::io::Result<()>,
) -> std::io::Result<std::os::unix::net::UnixStream> {
    use fs2::FileExt;
    use std::{
        os::unix::{fs::OpenOptionsExt, net::UnixStream},
        time::Instant,
    };
    let path = root.join("cli.sock");
    if let Ok(stream) = UnixStream::connect(&path) {
        return Ok(stream);
    }
    // Serialize cold-start launchers, NOT captures. Concurrent captures must
    // receive in_progress, not queue another picker behind a long-held file lock.
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(root.join("launch.lock"))?;
    lock.lock_exclusive()?;
    if let Ok(stream) = UnixStream::connect(&path) {
        return Ok(stream);
    }
    launch()?;
    let start = Instant::now();
    loop {
        if let Ok(stream) = UnixStream::connect(&path) {
            return Ok(stream);
        }
        if start.elapsed() > Duration::from_secs(20) {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
#[cfg(unix)]
pub fn client(root: &Path) -> Outcome {
    use std::{
        io::{Read, Write},
        os::unix::process::CommandExt,
        process::{Command, Stdio},
    };
    let fail = || Outcome::failed(Failure::Unavailable);
    let connection = connect_or_launch(root, || {
        Command::new(std::env::current_exe()?)
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(())
    });
    let Ok(mut stream) = connection else {
        return fail();
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(640)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    if stream.write_all(REQUEST).is_err() || stream.shutdown(std::net::Shutdown::Write).is_err() {
        return fail();
    }
    let mut bytes = Vec::new();
    if stream.take(4096).read_to_end(&mut bytes).is_err() {
        return fail();
    }
    // Never retry a sent request: a lost response must not cause a second capture.
    serde_json::from_slice(&bytes).unwrap_or_else(|_| fail())
}

#[cfg(windows)]
mod named {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub fn pipe_name(root: &Path) -> String {
        // Windows named pipes are global; derive a per-profile name so two
        // users on one machine never collide. Hashing the private root gives a
        // stable, guess-resistant suffix without exposing the username.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        root.hash(&mut hasher);
        format!(r"\\.\pipe\clipocr-{:016x}", hasher.finish())
    }

    /// Resident (server) side: run a dedicated thread that creates a fresh
    /// pipe instance per request, answers exactly one CLI invocation, then
    /// rebinds. A per-server tokio runtime handles the async capture pipeline.
    pub fn serve_blocking(root: PathBuf, handler: Arc<dyn RequestHandler>) -> Result<(), Failure> {
        let name = pipe_name(&root);
        loop {
            let server = match tokio::net::windows::named_pipe::ServerOptions::new()
                .first_pipe_instance(true)
                .create(&name)
            {
                Ok(s) => s,
                Err(_) => return Err(Failure::Unavailable),
            };
            // Answer one client, then loop to rebind for the next invocation.
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(_) => return Err(Failure::Engine),
            };
            let _ = rt.block_on(respond_std(server, handler.clone()));
        }
    }

    async fn respond_std(
        mut pipe: tokio::net::windows::named_pipe::NamedPipeServer,
        handler: Arc<dyn RequestHandler>,
    ) -> Outcome {
        let mut request = Vec::new();
        let read = pipe.read_to_end(&mut request).await;
        let out = if read.is_ok() && request == REQUEST {
            handler.capture().await
        } else {
            Outcome::failed(Failure::Unavailable)
        };
        if let Ok(mut bytes) = serde_json::to_vec(&out) {
            bytes.push(b'\n');
            let _ = pipe.write_all(&bytes).await;
        }
        let _ = pipe.flush().await;
        out
    }

    /// CLI side: connect to the resident (cold-starting it if needed), send the
    /// request, and return the parsed terminal outcome without retrying.
    pub fn client(root: &Path) -> Outcome {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(_) => return Outcome::failed(Failure::Unavailable),
        };
        rt.block_on(async move {
            let mut attempts = 0;
            loop {
                let mut pipe = match tokio::net::windows::named_pipe::ClientOptions::new()
                    .open(pipe_name(root))
                {
                    Ok(c) => c,
                    Err(_) if attempts == 0 => {
                        // Cold start: spawn the resident, then retry once.
                        let _ = std::process::Command::new(std::env::current_exe())
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .spawn();
                        attempts += 1;
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        continue;
                    }
                    Err(_) => return Outcome::failed(Failure::Unavailable),
                };
                if pipe.write_all(REQUEST).await.is_err() {
                    return Outcome::failed(Failure::Unavailable);
                }
                // Half-close the write side so the server's read_to_end returns.
                let _ = pipe.shutdown().await;
                let mut bytes = Vec::new();
                if pipe.read_to_end(&mut bytes).await.is_err() {
                    return Outcome::failed(Failure::Unavailable);
                }
                return serde_json::from_slice(&bytes)
                    .unwrap_or_else(|_| Outcome::failed(Failure::Unavailable));
            }
        })
    }
}

/// Windows: spawn the named-pipe server on a dedicated thread. Unix uses
/// `bind`/`serve` (async, tokio-native); the pipe server needs its own thread
/// because it may block a whole `read_to_end` per request.
#[cfg(windows)]
pub fn spawn_server(root: PathBuf, handler: Arc<dyn RequestHandler>) {
    std::thread::Builder::new()
        .name("clipocr-ipc".into())
        .spawn(move || {
            let _ = named::serve_blocking(root, handler);
        })
        .ok();
}

/// Windows CLI entry: connect (cold-starting the resident if needed), send the
/// capture request, and return the parsed terminal outcome.
#[cfg(windows)]
pub fn named_client(root: &Path) -> Outcome {
    named::client(root)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::core::Status;
    struct Handler(Outcome);
    #[async_trait]
    impl RequestHandler for Handler {
        async fn capture(&self) -> Outcome {
            self.0.clone()
        }
    }
    #[test]
    fn only_exact_capture_argv_is_accepted() {
        assert!(wants_capture(&["app".into(), "--capture-ocr".into()]));
        for args in [
            vec![],
            vec!["app".into()],
            vec![
                "app".into(),
                "--capture-ocr".into(),
                "--engine=remote".into(),
            ],
            vec!["app".into(), "--capture-ocr-selftest".into()],
        ] {
            assert!(!wants_capture(&args));
        }
    }
    #[tokio::test]
    async fn delegation_returns_terminal_outcomes_and_rejects_other_messages() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for status in [
            Status::Copied,
            Status::Cancelled,
            Status::Empty,
            Status::PermissionDenied,
            Status::InProgress,
            Status::Failed,
            Status::LowConfidence,
        ] {
            let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
            let expected = Outcome::new(status);
            let task = tokio::spawn(respond(server, Arc::new(Handler(expected.clone()))));
            client.write_all(REQUEST).await.unwrap();
            client.shutdown().await.unwrap();
            let mut bytes = Vec::new();
            client.read_to_end(&mut bytes).await.unwrap();
            assert_eq!(serde_json::from_slice::<Outcome>(&bytes).unwrap(), expected);
            task.await.unwrap();
        }
        for bad in [
            b"--capture-ocr\n--engine=remote".as_slice(),
            b"--quit\n",
            b"",
            b"--capture-ocr-selftest\n",
        ] {
            let (mut client, server) = tokio::net::UnixStream::pair().unwrap();
            let task = tokio::spawn(respond(
                server,
                Arc::new(Handler(Outcome::new(Status::Copied))),
            ));
            client.write_all(bad).await.unwrap();
            client.shutdown().await.unwrap();
            let mut bytes = Vec::new();
            client.read_to_end(&mut bytes).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<Outcome>(&bytes).unwrap().status,
                Status::Failed
            );
            task.await.unwrap();
        }
    }
    #[test]
    fn simultaneous_cold_clients_launch_one_resident() {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        };
        let t = tempfile::tempdir().unwrap();
        let launches = Arc::new(AtomicUsize::new(0));
        let listener = Arc::new(Mutex::new(None));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let root = t.path().to_owned();
                let launches = launches.clone();
                let listener = listener.clone();
                std::thread::spawn(move || {
                    connect_or_launch(&root, || {
                        launches.fetch_add(1, Ordering::SeqCst);
                        *listener.lock().unwrap() = Some(bind(&root).unwrap());
                        Ok(())
                    })
                    .unwrap()
                })
            })
            .collect();
        for thread in threads {
            drop(thread.join().unwrap());
        }
        assert_eq!(launches.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn socket_refuses_symlink_or_non_socket() {
        let t = tempfile::tempdir().unwrap();
        std::fs::write(t.path().join("cli.sock"), b"synthetic").unwrap();
        assert!(bind(t.path()).is_err());
    }
}
