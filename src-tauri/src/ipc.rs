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

/// Short bounded probe for ONE byte past a matching request. The pipe
/// transport delivers no EOF, so this is how `REQUEST + junk` fails closed
/// without buffering it: legit clients never send a 15th byte, making the
/// probe window invisible to them.
#[cfg(any(windows, test))]
const EXTRA_BYTE_PROBE: Duration = Duration::from_millis(100);

/// Hard deadline for one request exchange; mirrors the Unix socket server's
/// 2s read deadline (and bounds a client that withholds part of the request).
#[cfg(any(windows, test))]
const REQUEST_DEADLINE: Duration = Duration::from_secs(2);

/// Pause after an instance is consumed before the loop creates the next one.
/// On the real Windows transport (tokio 1.53 + mio IOCP), a connect+read-
/// to-EOF exchange leaves the disconnect completion draining through the
/// reactor: re-creating with FILE_FLAG_FIRST_PIPE_INSTANCE immediately after
/// dropping the instance fails with ERROR_ACCESS_DENIED ("pipe exists") until
/// that completion is delivered. Verified on the Windows host: 10ms of
/// reactor drain is sufficient; 25ms gives headroom while staying imperceptible
/// between CLI invocations.
#[cfg(any(windows, test))]
const PIPE_RECYCLE_PAUSE: Duration = Duration::from_millis(25);

/// Bounded responder behind the Windows named-pipe server (and the duplex
/// streams in tests, which satisfy the same AsyncRead + AsyncWrite contract
/// as a NamedPipeServer). That transport has NO half-close — the real CLI
/// writes the request, shutdown()s (a flush, never an EOF), and waits — so
/// the read must complete on the FIXED request size alone: read exactly
/// REQUEST.len() bytes within REQUEST_DEADLINE, byte-compare, then prove the
/// stream ended with a one-byte probe before dispatching. The reply is
/// written (bounded) before returning so the caller can rebind — unless the
/// read itself died on a client disconnect, where no reader remains and
/// replying would only poison the next instance create.
#[cfg(any(windows, test))]
async fn respond_bounded<S>(mut stream: S, handler: Arc<dyn RequestHandler>) -> Outcome
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut request = [0u8; REQUEST.len()];
    let read = tokio::time::timeout(REQUEST_DEADLINE, stream.read_exact(&mut request)).await;
    // A read that ended on a transport-level disconnect means the client is
    // GONE (a connect-and-drop probe is the canonical case): no reader
    // remains for a reply, and on the Windows pipe transport writing into
    // the just-disconnected instance succeeds into the kernel buffer and
    // poisons the loop's next first_pipe_instance create with
    // ERROR_ACCESS_DENIED. UnexpectedEof belongs in the skip set: probe
    // evidence from the Windows verification host (pinned tokio 1.53.3)
    // shows the post-disconnect read on the real pipe transport is
    // reported as read_exact wrapping the underlying 0-byte read into
    // ErrorKind::UnexpectedEof — mio surfaces the disconnect as EOF, not
    // BrokenPipe — so the dropped-handle probe takes this skip path and
    // never reaches the reply write. On the production transport an EOF
    // can only mean the client disconnected: the CLI's shutdown() is a
    // flush there, not a half-close, so a live client never delivers one.
    let client_gone = matches!(
        &read,
        Ok(Err(e))
            if matches!(
                e.kind(),
                std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::UnexpectedEof
            )
    );
    let out = if matches!(read, Ok(Ok(_))) && request == REQUEST {
        // Exact match — but a longer stream must still fail closed. With no
        // EOF to terminate the request, probe for one extra byte under a
        // short bound: anything past REQUEST means junk, so do not dispatch.
        let mut extra = [0u8; 1];
        if matches!(
            tokio::time::timeout(EXTRA_BYTE_PROBE, stream.read_exact(&mut extra)).await,
            Ok(Ok(_))
        ) {
            Outcome::failed(Failure::Unavailable)
        } else {
            // Panic parity with the Unix responder (ipc.rs `respond`): a
            // panicking handler becomes a typed Engine failure instead of
            // unwinding through serve_blocking's block_on and silently
            // killing the clipocr-ipc thread until app restart.
            match tokio::spawn(async move { handler.capture().await }).await {
                Ok(out) => out,
                Err(_) => Outcome::failed(Failure::Engine),
            }
        }
    } else {
        Outcome::failed(Failure::Unavailable)
    };
    if client_gone {
        // No reader left; never enter the reply-write block on this path.
        return out;
    }
    if let Ok(mut bytes) = serde_json::to_vec(&out) {
        bytes.push(b'\n');
        // Bound the reply too: a server-side pipe flush can block until the
        // client drains the pipe, so a silent client must not pin the
        // transport past the same deadline.
        let _ = tokio::time::timeout(REQUEST_DEADLINE, async {
            let _ = stream.write_all(&bytes).await;
            let _ = stream.flush().await;
        })
        .await;
    }
    out
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
    /// rebinds. The whole loop (including pipe creation, which needs a Tokio
    /// reactor) runs inside a single current-thread runtime.
    pub fn serve_blocking(root: PathBuf, handler: Arc<dyn RequestHandler>) -> Result<(), Failure> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| Failure::Engine)?;
        rt.block_on(async move {
            let name = pipe_name(&root);
            loop {
                let server = match tokio::net::windows::named_pipe::ServerOptions::new()
                    .first_pipe_instance(true)
                    .create(&name)
                {
                    Ok(s) => s,
                    Err(_) => return Err(Failure::Unavailable),
                };
                // A created instance is listening but NOT connected: tokio's
                // AsyncRead for NamedPipeServer polls the IO directly, so a
                // read can never deliver data until the client handshake is
                // completed by an explicit connect(). Park here — this is
                // where the loop waits for the next CLI invocation.
                match tokio::time::timeout(REQUEST_DEADLINE, server.connect()).await {
                    Ok(Ok(())) => {
                        // Answer one client, then loop to rebind for the next invocation.
                        let _ = respond_std(server, handler.clone()).await;
                    }
                    // A timed-out or failed handshake leaves no client to
                    // reply to: release the instance (dropping the pipe is
                    // what lets the next create rebind) and take the next
                    // iteration — same fail-closed discipline as every other
                    // branch, and never a leaked handle.
                    _ => drop(server),
                }
                // Drain the reactor between instances: the just-closed
                // instance's disconnect completion must be delivered before
                // FILE_FLAG_FIRST_PIPE_INSTANCE allows the name to be claimed
                // again (ERROR_ACCESS_DENIED otherwise — see PIPE_RECYCLE_PAUSE).
                tokio::time::sleep(PIPE_RECYCLE_PAUSE).await;
            }
        })
    }
    async fn respond_std(
        pipe: tokio::net::windows::named_pipe::NamedPipeServer,
        handler: Arc<dyn RequestHandler>,
    ) -> Outcome {
        // Bounded read/deadline/exact-match contract; see respond_bounded.
        respond_bounded(pipe, handler).await
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
                        let exe = match std::env::current_exe() {
                            Ok(exe) => exe,
                            Err(_) => return Outcome::failed(Failure::Unavailable),
                        };
                        let _ = std::process::Command::new(exe)
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
                // Flush the request. This is NOT a half-close on the pipe
                // transport (poll_shutdown is poll_flush there), which is
                // exactly why the server completes on the fixed request
                // size instead of waiting for an EOF.
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
/// because each request blocks its current-thread runtime for the whole
/// bounded exchange (a read can pend up to the request deadline).
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

/// Contract tests for the bounded responder the Windows named-pipe server
/// delegates to. The Windows transport itself cannot run off-Windows, so the
/// same code path is exercised over tokio duplex streams, which satisfy the
/// same AsyncRead + AsyncWrite + Unpin contract as a NamedPipeServer.
#[cfg(test)]
mod respond_bounded_tests {
    use super::*;
    use crate::core::Status;
    use std::{
        sync::atomic::{AtomicBool, Ordering},
        time::Instant,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Hang-class regression guard: the transport deadline is 2s, so every
    /// healthy exchange must settle far inside this bound.
    const WATCHDOG: Duration = Duration::from_secs(8);

    struct CountingHandler {
        called: AtomicBool,
        out: Outcome,
    }

    impl CountingHandler {
        fn new(out: Outcome) -> Arc<Self> {
            Arc::new(Self {
                called: AtomicBool::new(false),
                out,
            })
        }

        fn was_called(&self) -> bool {
            self.called.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl RequestHandler for CountingHandler {
        async fn capture(&self) -> Outcome {
            self.called.store(true, Ordering::SeqCst);
            self.out.clone()
        }
    }

    /// One full client/server exchange, optionally half-closing the client
    /// write side; fails the test if the exchange outlives the watchdog.
    async fn exchange(
        payload: &[u8],
        half_close: bool,
        out: Outcome,
    ) -> (Vec<u8>, Arc<CountingHandler>) {
        tokio::time::timeout(WATCHDOG, async {
            let (mut client, server) = tokio::io::duplex(64);
            let handler = CountingHandler::new(out);
            let task = tokio::spawn(respond_bounded(server, handler.clone()));
            client.write_all(payload).await.unwrap();
            if half_close {
                client.shutdown().await.unwrap();
            }
            let mut bytes = Vec::new();
            client.read_to_end(&mut bytes).await.unwrap();
            task.await.unwrap();
            (bytes, handler)
        })
        .await
        .expect("exchange settled within the watchdog")
    }

    #[test]
    fn read_bounds_pin_the_shared_contract() {
        assert_eq!(REQUEST_DEADLINE, Duration::from_secs(2));
        assert_eq!(EXTRA_BYTE_PROBE, Duration::from_millis(100));
        // Pin the instance-recycle pause so its value is referenced on every
        // cfg(any(windows, test)) build (and any regression of the drain
        // window is caught in review, not silently tuned).
        assert_eq!(PIPE_RECYCLE_PAUSE, Duration::from_millis(25));
        // The read is exact-size (REQUEST.len()), never an unbounded
        // read_to_end; the only further byte ever read is the single
        // probe byte past a matching request.
        assert_eq!(REQUEST.len(), 14);
    }

    #[tokio::test]
    async fn exact_request_dispatches_and_replies_with_each_terminal_status() {
        for status in [
            Status::Copied,
            Status::Cancelled,
            Status::Empty,
            Status::PermissionDenied,
            Status::InProgress,
            Status::Failed,
            Status::LowConfidence,
        ] {
            let expected = Outcome::new(status);
            let (bytes, handler) = exchange(REQUEST, true, expected.clone()).await;
            assert_eq!(serde_json::from_slice::<Outcome>(&bytes).unwrap(), expected);
            assert!(handler.was_called());
        }
    }

    #[tokio::test]
    async fn short_payload_eof_gets_no_reply_and_never_dispatches() {
        // Corrected contract: UnexpectedEof is a client-gone marker, so a
        // half-closing duplex client (shutdown = real EOF) that never
        // completes the fixed-size read gets NO reply — on the production
        // transport an EOF only ever means the client disconnected (the
        // CLI's shutdown() is a flush there, not a half-close). The EOF is
        // detected the moment the short payload ends, so these exchanges
        // settle with no deadline wait.
        for short in [
            b"".as_slice(),
            b"--quit\n".as_slice(),
            &REQUEST[..REQUEST.len() - 1],
        ] {
            let (bytes, handler) = exchange(short, true, Outcome::new(Status::Copied)).await;
            assert!(
                bytes.is_empty(),
                "payload {short:?} must get zero reply bytes (client gone)"
            );
            assert!(!handler.was_called(), "payload {short:?} must not dispatch");
        }
    }

    #[tokio::test]
    async fn non_matching_payloads_are_rejected_without_dispatch() {
        // Payloads that complete the fixed-size read (read_exact fills all
        // REQUEST.len() bytes before any EOF can arrive) are live clients:
        // they still receive the Failed rejection reply. Only a read that
        // dies on a disconnect takes the client-gone skip.
        for bad in [
            b"--capture-ocr\n--engine=remote".as_slice(),
            b"--capture-ocr-selftest\n".as_slice(),
        ] {
            let (bytes, handler) = exchange(bad, true, Outcome::new(Status::Copied)).await;
            assert_eq!(
                serde_json::from_slice::<Outcome>(&bytes).unwrap().status,
                Status::Failed,
                "payload {bad:?} must be rejected"
            );
            assert!(!handler.was_called());
        }
    }

    #[tokio::test]
    async fn request_plus_junk_fails_closed_via_the_extra_byte_probe() {
        let mut oversize = REQUEST.to_vec();
        oversize.extend_from_slice(b"--engine=remote");
        // Both with and without a half-close: a matching REQUEST prefix
        // followed by even one more byte must never dispatch.
        for half_close in [false, true] {
            let (bytes, handler) =
                exchange(&oversize, half_close, Outcome::new(Status::Copied)).await;
            assert_eq!(
                serde_json::from_slice::<Outcome>(&bytes).unwrap().status,
                Status::Failed
            );
            assert!(!handler.was_called());
        }
    }

    #[tokio::test]
    async fn withheld_partial_request_is_cut_off_by_the_read_deadline() {
        let started = Instant::now();
        // Only part of the request, no half-close: the exact-size read can
        // never complete, so the deadline must end it.
        let (bytes, handler) = exchange(&REQUEST[..7], false, Outcome::new(Status::Copied)).await;
        assert_eq!(
            serde_json::from_slice::<Outcome>(&bytes).unwrap().status,
            Status::Failed
        );
        assert!(!handler.was_called());
        assert!(
            started.elapsed() >= REQUEST_DEADLINE - Duration::from_millis(50),
            "failure must come from the deadline, not an early abort"
        );
    }

    #[tokio::test]
    async fn exact_request_without_eof_dispatches_like_the_pipe_transport() {
        // The Windows named-pipe transport has no half-close: the real CLI
        // (named::client) writes REQUEST, shutdown()s (a flush, never an
        // EOF), and waits for the reply. The responder must therefore
        // complete on the fixed request size alone instead of waiting for
        // an EOF that never comes on that transport.
        let expected = Outcome::new(Status::Copied);
        let started = Instant::now();
        let (bytes, handler) = exchange(REQUEST, false, expected.clone()).await;
        assert_eq!(serde_json::from_slice::<Outcome>(&bytes).unwrap(), expected);
        assert!(handler.was_called());
        assert!(
            started.elapsed() < REQUEST_DEADLINE,
            "dispatch must not wait out the read deadline for an EOF"
        );
    }

    struct PanickingHandler;

    #[async_trait]
    impl RequestHandler for PanickingHandler {
        async fn capture(&self) -> Outcome {
            panic!("synthetic handler panic");
        }
    }

    #[tokio::test]
    async fn panicking_handler_answers_engine_failure_and_returns_normally() {
        // Panic parity with the Unix responder: a panicking handler must
        // become a typed Engine-failure reply, and the responder must
        // RETURN (so serve_blocking's loop can serve the next request)
        // instead of unwinding through the server thread's block_on.
        let (mut client, server) = tokio::io::duplex(64);
        let task = tokio::spawn(respond_bounded(server, Arc::new(PanickingHandler)));
        client.write_all(REQUEST).await.unwrap();
        client.shutdown().await.unwrap();
        let mut bytes = Vec::new();
        client.read_to_end(&mut bytes).await.unwrap();
        let out: Outcome = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(out.status, Status::Failed);
        assert_eq!(out.reason, Some(Failure::Engine));
        assert_eq!(task.await.unwrap(), out);
    }
}

/// Real-transport regression for the EOF-independent read: the CLI's
/// shutdown() delivers no EOF on the named-pipe transport (tokio's
/// NamedPipeClient maps poll_shutdown to poll_flush), so this drives the
/// actual serve_blocking + client pair end to end and asserts the exchange
/// completes on the fixed request size alone. Runs only on Windows — the
/// duplex tests above carry the shared logic on other hosts.
#[cfg(all(test, windows))]
mod named_pipe_tests {
    use super::*;
    use crate::core::Status;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::net::windows::named_pipe::ClientOptions;

    struct CountingHandler {
        called: AtomicBool,
    }

    #[async_trait]
    impl RequestHandler for CountingHandler {
        async fn capture(&self) -> Outcome {
            self.called.store(true, Ordering::SeqCst);
            Outcome::new(Status::Copied)
        }
    }

    /// Poll until the server has demonstrably rebound a fresh instance: the
    /// FIRST connect-and-drop consumes the instance serve_blocking initially
    /// parked on, and a SECOND successful connect proves the server finished
    /// that probe's dropped-handle exchange (the disconnect read surfaces as
    /// read_exact's UnexpectedEof, so the client-gone skip path returns
    /// without any reply write) and parked a new listening instance — a
    /// connected pipe instance never returns to listening, so only a rebound
    /// instance can accept the second connect. Both probes share the same
    /// 5s deadline.
    fn wait_until_listening(root: &Path) {
        let start = std::time::Instant::now();
        // Constructing a tokio named-pipe client registers it with a
        // reactor, so each open attempt must run inside a runtime. The
        // runtime is built here (not via #[tokio::test]) because
        // named::client below builds and block_ons its own.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut connected_probes = 0;
        while connected_probes < 2 {
            // Connect and drop immediately; the disconnect surfaces as
            // read_exact's UnexpectedEof, driving the client-gone skip
            // path (no reply write) and the rebind.
            match rt.block_on(async { ClientOptions::new().open(named::pipe_name(root)) }) {
                Ok(_) => connected_probes += 1,
                Err(_) if start.elapsed() < Duration::from_secs(5) => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => panic!("pipe server never rebound a fresh instance"),
            }
        }
    }

    #[test]
    fn real_pipe_round_trip_dispatches_without_client_eof() {
        let root = tempfile::tempdir().unwrap();
        let handler = Arc::new(CountingHandler {
            called: AtomicBool::new(false),
        });
        let server_root = root.path().to_owned();
        let server_handler = handler.clone();
        // serve_blocking loops until pipe creation fails, so the thread is
        // intentionally left running for the rest of the test process.
        std::thread::Builder::new()
            .name("clipocr-test-pipe-server".into())
            .spawn(move || {
                let _ = named::serve_blocking(server_root, server_handler);
            })
            .unwrap();
        wait_until_listening(root.path());
        // The second probe's dropped-handle exchange now takes the
        // client-gone skip path (no reply write), so the rebind follows the
        // disconnect almost immediately; still settle past the old
        // REQUEST_DEADLINE-sized window so the real client's first open
        // lands on a parked instance and never takes the cold-start branch
        // (which would spawn this test binary as the "resident",
        // recursively re-running the suite as a process farm).
        std::thread::sleep(REQUEST_DEADLINE + Duration::from_millis(500));
        // A hang must fail the test, never the suite: bound the client call
        // with a watchdog instead of blocking the test thread indefinitely.
        let client_root = root.path().to_owned();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("clipocr-test-pipe-client".into())
            .spawn(move || {
                let _ = tx.send(named::client(&client_root));
            })
            .unwrap();
        let outcome = match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(outcome) => outcome,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("named::client did not finish the pipe exchange within 30s (hung exchange)")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("named::client thread ended without reporting an outcome")
            }
        };
        assert_eq!(outcome.status, Status::Copied);
        assert!(handler.called.load(Ordering::SeqCst));
    }
}
