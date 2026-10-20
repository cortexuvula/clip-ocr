//! Consented update CHECK — not an updater.
//!
//! One HTTPS GET to a fixed GitHub endpoint, triggered by the tray action
//! or (opt-in, default OFF) at most once per 24h in the background. No
//! download, no install, no restart automation: on a newer version the app
//! shows it and, on the user's click, opens the release page in a browser.
//!
//! Hard constraints encoded here:
//! - Fixed endpoint, never runtime-configurable.
//! - Redirects are handled MANUALLY and each hop must stay on the
//!   allowlisted hosts over HTTPS (GitHub serves release assets via signed
//!   CDN URLs — that is why the CDN hosts are on the list at all).
//! - Bounded response body, hard timeout — the transport enforces both.
//! - Every failure maps to a fixed typed reason; a failure is NEVER
//!   reported as "up to date".
//! - The request carries nothing but the GET: no User-Agent identifying
//!   the version, no version/platform/install-id in query or headers.
//!
//! Network access lives only in [`UreqTransport`] (Rust side, called from
//! the resident). The webview/UI never gains HTTP capability; it only
//! reads passive state and sets a "please check" flag.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::core::InFlightGuard;

pub const ENDPOINT: &str =
    "https://github.com/cortexuvula/clip-ocr/releases/latest/download/latest.json";
/// Hard cap on the manifest body. The real payload is < 200 bytes.
pub const MAX_BODY_BYTES: usize = 8 * 1024;
/// Per-request hard timeout (connect + read, whole GET).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Automatic checks: at most one per 24h of elapsed time.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// Never check sooner than this after launch.
pub const MIN_DELAY_AFTER_LAUNCH: Duration = Duration::from_secs(120);
/// Maximum added jitter on the automatic schedule (never subtracts).
pub const MAX_JITTER: Duration = Duration::from_secs(300);
/// Failure backoff ladder for automatic checks.
pub const BACKOFF_LADDER: [Duration; 3] = [
    Duration::from_secs(6 * 60 * 60),
    Duration::from_secs(12 * 60 * 60),
    Duration::from_secs(24 * 60 * 60),
];
/// Manual redirect hops before giving up.
const MAX_REDIRECT_HOPS: usize = 3;

/// Redirect allowlist: github.com itself plus the hosts GitHub redirects
/// release-asset downloads to (signed, expiring CDN URLs). Anything else —
/// including http:// variants — is a typed failure, never followed.
const REDIRECT_HOSTS: [&str; 3] = [
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
];

/// Fixed failure reasons (UI copy, exactly these labels).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckReason {
    Offline,
    Connection,
    Tls,
    Unavailable,
    RateLimited,
}

impl CheckReason {
    pub fn label(&self) -> &'static str {
        match self {
            CheckReason::Offline => "Offline",
            CheckReason::Connection => "Connection or proxy error",
            CheckReason::Tls => "Secure connection could not be verified",
            CheckReason::Unavailable => "Update service unavailable",
            CheckReason::RateLimited => "Too many requests",
        }
    }
}

/// Strict `X.Y.Z` (three numeric components, nothing else). A prerelease
/// suffix (`0.4.0-beta`) or any other shape does not parse — the manifest
/// contract is exact.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.split('.');
    let x = parts.next()?.parse().ok()?;
    let y = parts.next()?.parse().ok()?;
    let z = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((x, y, z))
}

/// `Some(true)` iff `latest` is strictly newer than `current` (both must
/// parse). Unparseable input is `None` — callers treat that as a failure,
/// never as "equal".
pub fn is_newer(latest: &str, current: &str) -> Option<bool> {
    Some(parse_version(latest)? > parse_version(current)?)
}

/// One GET's raw outcome (transport layer; redirects NOT followed).
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub location: Option<String>,
    /// `Retry-After` in seconds, when the service sent one.
    pub retry_after: Option<Duration>,
    pub body: Vec<u8>,
}

/// Low-level transport failure classification. The production ureq mapping
/// is heuristic (offline vs connection vs TLS); the CHECK layer never turns
/// any of these into success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportError {
    Offline,
    Connection,
    Tls,
}

/// Injected transport seam: exactly ONE request per call. Tests script it;
/// production uses [`UreqTransport`].
pub trait UpdateTransport: Send + Sync {
    fn get(
        &self,
        url: &str,
        timeout: Duration,
        max_body: usize,
    ) -> Result<HttpResponse, TransportError>;
}

/// The exact URL shape `release.yml` writes into `latest.json`
/// (`releases/tag/v` + the validated tag). The open-browser path validates
/// AGAIN, at the point of use, and only ever launches this shape.
const TAG_URL_PREFIX: &str = "https://github.com/cortexuvula/clip-ocr/releases/tag/v";

/// Is this exactly the canonical release-tag URL: the repo tag prefix
/// followed by `[0-9A-Za-z.-]+` and nothing else? Rejects quotes, spaces,
/// `&`, `|`, `%`, `;`, backslashes, `..` segments, query strings and
/// fragments — so a crafted manifest can never put shell or URL
/// metacharacters in front of the OS launcher.
pub fn is_canonical_release_url(url: &str) -> bool {
    let Some(rest) = url.strip_prefix(TAG_URL_PREFIX) else {
        return false;
    };
    !rest.is_empty()
        && !rest.contains("..")
        && rest
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
}

/// Parsed-and-validated manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOutcome {
    UpToDate { latest: String },
    Available { latest: String, notes_url: String },
}

/// Validate a redirect target: absolute HTTPS URL whose host is on the
/// allowlist. Relative locations are rejected (the endpoint never sends
/// them; if it ever does, failing closed is correct).
fn validate_redirect(location: &str) -> Result<String, CheckReason> {
    if let Some(rest) = location.strip_prefix("https://") {
        let host = rest.split('/').next().unwrap_or("");
        if REDIRECT_HOSTS.contains(&host) {
            return Ok(location.to_owned());
        }
    }
    Err(CheckReason::Unavailable)
}

/// Parse and validate the manifest body. Any deviation — size, JSON shape,
/// version grammar, notes_url host — is the same typed failure. The
/// notes_url must be HTTPS on github.com under this repository.
fn parse_manifest(body: &[u8], current_version: &str) -> Result<CheckOutcome, CheckReason> {
    if body.len() > MAX_BODY_BYTES {
        return Err(CheckReason::Unavailable);
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Manifest {
        version: String,
        notes_url: String,
    }
    let manifest: Manifest = serde_json::from_slice(body).map_err(|_| CheckReason::Unavailable)?;
    if parse_version(&manifest.version).is_none() {
        return Err(CheckReason::Unavailable);
    }
    // notes_url: strict string form — exactly this repository on github.com
    // over HTTPS, with an optional path suffix. No percent/host tricks.
    let prefix = "https://github.com/cortexuvula/clip-ocr";
    let ok = manifest
        .notes_url
        .strip_prefix(prefix)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'));
    if !ok {
        return Err(CheckReason::Unavailable);
    }
    if is_newer(&manifest.version, current_version) == Some(true) {
        Ok(CheckOutcome::Available {
            latest: manifest.version,
            notes_url: manifest.notes_url,
        })
    } else {
        Ok(CheckOutcome::UpToDate {
            latest: manifest.version,
        })
    }
}

/// One complete check: manual redirect handling across allowlisted hosts,
/// bounded body, typed failure for everything else. `Retry-After` (429) is
/// surfaced for the backoff override.
pub fn check_once(
    transport: &dyn UpdateTransport,
    current_version: &str,
) -> Result<CheckOutcome, (CheckReason, Option<Duration>)> {
    let mut url = ENDPOINT.to_owned();
    for _ in 0..MAX_REDIRECT_HOPS {
        let resp = transport
            .get(&url, REQUEST_TIMEOUT, MAX_BODY_BYTES)
            .map_err(|e| (from_transport(e), None))?;
        match resp.status {
            200 => {
                return parse_manifest(&resp.body, current_version).map_err(|r| (r, None));
            }
            301 | 302 | 303 | 307 | 308 => {
                let Some(location) = resp.location else {
                    return Err((CheckReason::Unavailable, None));
                };
                url = validate_redirect(&location).map_err(|r| (r, None))?;
            }
            429 => return Err((CheckReason::RateLimited, resp.retry_after)),
            404 | 500..=599 => return Err((CheckReason::Unavailable, None)),
            _ => return Err((CheckReason::Unavailable, None)),
        }
    }
    Err((CheckReason::Unavailable, None))
}

fn from_transport(e: TransportError) -> CheckReason {
    match e {
        TransportError::Offline => CheckReason::Offline,
        TransportError::Connection => CheckReason::Connection,
        TransportError::Tls => CheckReason::Tls,
    }
}

/// Pure schedule for automatic checks. All instants are injected so the
/// rules are unit-testable.
#[derive(Debug, Clone)]
pub struct Cadence {
    pub launch: Instant,
    pub last_attempt: Option<Instant>,
    pub last_success: Option<Instant>,
    pub failures: u32,
    /// A `Retry-After` longer than the standard backoff extends the wait.
    pub retry_after_override: Option<Duration>,
}

impl Cadence {
    /// Standard backoff after `failures` consecutive failures: 6h, 12h,
    /// 24h — extended by a longer `Retry-After` when the service sent one.
    pub fn effective_backoff(&self) -> Duration {
        let idx = usize::try_from(self.failures.saturating_sub(1))
            .unwrap_or(BACKOFF_LADDER.len() - 1)
            .min(BACKOFF_LADDER.len() - 1);
        let mut backoff = BACKOFF_LADDER[idx];
        if let Some(ra) = self.retry_after_override {
            backoff = backoff.max(ra);
        }
        backoff
    }

    /// Whether an AUTOMATIC check is due NOW. Never while a check is in
    /// flight or a capture/OCR lease is held (defer, do not catch up).
    /// Jitter only ever ADDS delay, so the 24h minimum holds.
    pub fn due(
        &self,
        now: Instant,
        jitter: Duration,
        check_in_flight: bool,
        capture_busy: bool,
    ) -> bool {
        if check_in_flight || capture_busy {
            return false;
        }
        if now < self.launch + MIN_DELAY_AFTER_LAUNCH + jitter {
            return false;
        }
        let next = match (self.failures, self.last_success, self.last_attempt) {
            // Healthy: once per 24h since the last SUCCESS.
            (0, Some(t), _) => Some(t + CHECK_INTERVAL),
            // Failure ladder: since the last ATTEMPT, 6h/12h/24h
            // (or a longer Retry-After).
            (f, _, Some(t)) if f > 0 => Some(t + self.effective_backoff()),
            // Never attempted: gated only by the launch delay above.
            _ => None,
        };
        match next {
            Some(n) => now >= n + jitter,
            None => true,
        }
    }
}

/// Tray/Settings-visible update state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateStatus {
    /// Opt-out and no check has ever been made.
    Off,
    /// Automatic checks enabled, none run yet (or none since enabling).
    NotChecked,
    Checking,
    UpToDate {
        latest: String,
    },
    Available {
        latest: String,
    },
    Failed {
        reason: CheckReason,
    },
}

impl UpdateStatus {
    /// Exact tray row strings (ui-consultant contract).
    pub fn tray_label(&self) -> String {
        match self {
            UpdateStatus::Off => "Updates: Off".into(),
            UpdateStatus::NotChecked => "Updates: Not checked".into(),
            UpdateStatus::Checking => "Updates: Checking…".into(),
            UpdateStatus::UpToDate { .. } => "Updates: Up to date".into(),
            UpdateStatus::Available { latest } => format!("Update available: v{latest}"),
            UpdateStatus::Failed { .. } => "Updates: Check failed".into(),
        }
    }
}

struct ManagerInner {
    enabled: bool,
    status: UpdateStatus,
    notes_url: Option<String>,
    last_success: Option<String>,
    cadence: Cadence,
    /// Set by the tray action / Settings button; consumed by the next tick.
    pending_manual: bool,
    /// When the last check STARTED. Re-arming a manual request inside
    /// this window is ignored, so a webview tight loop becomes a trickle.
    last_started: Option<Instant>,
}

/// A manual check request inside this window of the last started check is
/// ignored (anti-spam for repeated UI requests).
pub const MANUAL_REQUEST_WINDOW: Duration = Duration::from_secs(30);

/// Resident-side manager. The check itself is single-flighted through the
/// SAME [`InFlightGuard`] machinery the capture pipeline uses.
pub struct UpdateManager {
    inner: Mutex<ManagerInner>,
    /// In-flight flag for the CHECK (not the capture).
    check_flag: Arc<AtomicBool>,
    transport: Arc<dyn UpdateTransport>,
    current_version: &'static str,
}

impl UpdateManager {
    pub fn new(transport: Arc<dyn UpdateTransport>, enabled: bool) -> Self {
        Self::with_version(transport, enabled, env!("CARGO_PKG_VERSION"))
    }

    pub fn with_version(
        transport: Arc<dyn UpdateTransport>,
        enabled: bool,
        current_version: &'static str,
    ) -> Self {
        Self {
            inner: Mutex::new(ManagerInner {
                enabled,
                status: if enabled {
                    UpdateStatus::NotChecked
                } else {
                    UpdateStatus::Off
                },
                notes_url: None,
                last_success: None,
                cadence: Cadence {
                    launch: Instant::now(),
                    last_attempt: None,
                    last_success: None,
                    failures: 0,
                    retry_after_override: None,
                },
                pending_manual: false,
                last_started: None,
            }),
            check_flag: Arc::new(AtomicBool::new(false)),
            transport,
            current_version,
        }
    }

    pub fn set_enabled(&self, enabled: bool) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.enabled = enabled;
        // Only move toward NotChecked when nothing has been observed yet;
        // a known result stays visible (it is still true information).
        if enabled && inner.status == UpdateStatus::Off {
            inner.status = UpdateStatus::NotChecked;
        }
    }

    /// User asked for a check (tray / Settings). The network request is
    /// still made only by the resident tick — never from the UI command.
    /// Re-requests within [`MANUAL_REQUEST_WINDOW`] of the last started
    /// check are ignored so a UI loop cannot hammer the endpoint.
    pub fn request_manual(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = inner.last_started {
            if t.elapsed() < MANUAL_REQUEST_WINDOW {
                return; // a check just ran; ignore the re-arm
            }
        }
        inner.pending_manual = true;
    }

    pub fn status(&self) -> UpdateStatus {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .status
            .clone()
    }

    pub fn notes_url(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .notes_url
            .clone()
    }

    pub fn last_success(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_success
            .clone()
    }

    /// Resident tick: decide whether to START a check now. Returns the
    /// guard holder when a check should run on a blocking thread.
    pub fn tick(&self, now: Instant, jitter: Duration, capture_busy: bool) -> Option<CheckLease> {
        // ANY check (manual or automatic) defers while a capture is
        // active or another check is in flight — never compete with OCR.
        let in_flight = self.check_flag.load(Ordering::SeqCst);
        if capture_busy || in_flight {
            return None; // pending flags survive the deferral
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let manual = inner.pending_manual;
        let auto = inner.enabled && inner.cadence.due(now, jitter, in_flight, capture_busy);
        if !manual && !auto {
            return None;
        }
        let Some(guard) = InFlightGuard::acquire(&self.check_flag) else {
            return None; // one check in flight
        };
        inner.pending_manual = false;
        inner.last_started = Some(now);
        inner.status = UpdateStatus::Checking;
        Some(CheckLease { _guard: guard })
    }

    /// Run the check (blocking thread, holding the [`CheckLease`]).
    /// Records the outcome; a failure is never recorded as up to date.
    pub fn run_check(&self, _lease: &CheckLease, now_utc: SystemTime) {
        let result = check_once(self.transport.as_ref(), self.current_version);
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let attempt = Instant::now();
        match result {
            Ok(CheckOutcome::UpToDate { latest }) => {
                inner.status = UpdateStatus::UpToDate { latest };
                inner.notes_url = None;
                inner.last_success = Some(format_utc(now_utc));
                inner.cadence.last_success = Some(attempt);
                inner.cadence.last_attempt = Some(attempt);
                inner.cadence.failures = 0;
                inner.cadence.retry_after_override = None;
            }
            Ok(CheckOutcome::Available { latest, notes_url }) => {
                inner.status = UpdateStatus::Available { latest };
                inner.notes_url = Some(notes_url);
                inner.last_success = Some(format_utc(now_utc));
                inner.cadence.last_success = Some(attempt);
                inner.cadence.last_attempt = Some(attempt);
                inner.cadence.failures = 0;
                inner.cadence.retry_after_override = None;
            }
            Err((reason, retry_after)) => {
                inner.status = UpdateStatus::Failed { reason };
                inner.notes_url = None;
                inner.cadence.last_attempt = Some(attempt);
                inner.cadence.failures = inner.cadence.failures.saturating_add(1);
                inner.cadence.retry_after_override = retry_after.filter(|ra| *ra > Duration::ZERO);
            }
        }
    }
}

/// Ownership token proving the single-flight guard is held; dropped when
/// the blocking check finishes.
pub struct CheckLease {
    _guard: InFlightGuard,
}

/// Small deterministic jitter in `[0, MAX_JITTER)` from wall-clock entropy
/// (no new dependency). Only ever ADDS delay.
pub fn current_jitter() -> Duration {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    Duration::from_nanos(u64::from(nanos) % MAX_JITTER.as_nanos() as u64)
}

/// Format epoch seconds as UTC `YYYY-MM-DD HH:MM UTC` (no chrono dep).
pub fn format_utc(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02} UTC",
        rem / 3600,
        (rem % 3600) / 60
    )
}

/// Howard Hinnant's `civil_from_days` (public-domain algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Production transport: ureq + rustls, blocking, redirects disabled
/// (the checker validates hops itself), no User-Agent that identifies the
/// version, bounded body read, hard timeout. A proxy/TLS policy failure is
/// a typed failure — never a downgrade to plain HTTP.
pub struct UreqTransport;

impl UpdateTransport for UreqTransport {
    fn get(
        &self,
        url: &str,
        timeout: Duration,
        max_body: usize,
    ) -> Result<HttpResponse, TransportError> {
        let agent = ureq::AgentBuilder::new()
            .timeout(timeout)
            .redirects(0) // manual, allowlist-checked hops only
            .build();
        let resp = match agent.get(url).call() {
            Ok(r) => r,
            Err(ureq::Error::Status(_, r)) => r, // non-2xx: read headers, map by status
            Err(ureq::Error::Transport(t)) => return Err(map_ureq_transport(&t)),
        };
        let status = resp.status();
        if status != 200 {
            let retry_after = resp
                .header("retry-after")
                .and_then(|v| v.parse::<u64>().ok());
            return Ok(HttpResponse {
                status,
                location: resp.header("location").map(str::to_owned),
                retry_after: retry_after.map(Duration::from_secs),
                body: Vec::new(),
            });
        }
        // Bounded read: take one extra byte so oversize is detectable.
        let mut body = Vec::new();
        use std::io::Read;
        resp.into_reader()
            .take(max_body as u64 + 1)
            .read_to_end(&mut body)
            .map_err(|_| TransportError::Connection)?;
        Ok(HttpResponse {
            status,
            location: None,
            retry_after: None,
            body,
        })
    }
}

/// Heuristic classification of ureq transport errors: DNS resolution is
/// "offline", certificate/TLS strings are "TLS", everything else
/// (connection refused, proxy failures, timeouts) is "connection".
fn map_ureq_transport(t: &ureq::Transport) -> TransportError {
    if t.kind() == ureq::ErrorKind::Dns {
        return TransportError::Offline;
    }
    let msg = t.message().unwrap_or_default().to_lowercase();
    if msg.contains("certificate") || msg.contains("tls") || msg.contains("rustls") {
        return TransportError::Tls;
    }
    TransportError::Connection
}

/// Counting, scripted transport for tests.
#[cfg(test)]
pub struct FakeTransport {
    pub calls: std::sync::atomic::AtomicUsize,
    pub script: Mutex<std::collections::VecDeque<Result<HttpResponse, TransportError>>>,
}

#[cfg(test)]
impl FakeTransport {
    pub fn new(script: Vec<Result<HttpResponse, TransportError>>) -> Arc<Self> {
        Arc::new(Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
            script: Mutex::new(script.into()),
        })
    }
}

#[cfg(test)]
impl UpdateTransport for FakeTransport {
    fn get(
        &self,
        _url: &str,
        _timeout: Duration,
        _max_body: usize,
    ) -> Result<HttpResponse, TransportError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or(Err(TransportError::Connection))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_body(version: &str, notes_url: &str) -> Vec<u8> {
        format!(r#"{{"version":"{version}","notes_url":"{notes_url}"}}"#).into_bytes()
    }

    const NOTES: &str = "https://github.com/cortexuvula/clip-ocr/releases/tag/v9.9.9";
    const CURRENT: &str = "0.3.0";

    fn resp(status: u16, body: Vec<u8>) -> Result<HttpResponse, TransportError> {
        Ok(HttpResponse {
            status,
            location: None,
            retry_after: None,
            body,
        })
    }

    // ---- version comparison ----

    #[test]
    fn version_comparison_newer_equal_older() {
        assert_eq!(is_newer("0.4.0", CURRENT), Some(true));
        assert_eq!(is_newer("1.0.0", "0.99.99"), Some(true));
        assert_eq!(is_newer("0.3.0", CURRENT), Some(false));
        assert_eq!(is_newer("0.2.9", CURRENT), Some(false));
        assert_eq!(is_newer("0.3.1", CURRENT), Some(true));
        // Minor/major rollovers compare numerically, not lexically.
        assert_eq!(is_newer("0.10.0", "0.9.0"), Some(true));
    }

    #[test]
    fn version_prerelease_suffix_and_unparseable_rejected() {
        assert_eq!(parse_version("0.4.0-beta"), None);
        assert_eq!(parse_version("v0.4.0"), None);
        assert_eq!(parse_version("0.4"), None);
        assert_eq!(parse_version("0.4.0.1"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(is_newer("0.4.0-beta", CURRENT), None);
        assert_eq!(is_newer("banana", CURRENT), None);
        assert_eq!(parse_version("0.4.0"), Some((0, 4, 0)));
    }

    #[test]
    fn manifest_with_prerelease_version_is_typed_failure_not_update() {
        let t = FakeTransport::new(vec![resp(200, ok_body("0.4.0-beta", NOTES))]);
        assert_eq!(
            check_once(t.as_ref(), CURRENT),
            Err((CheckReason::Unavailable, None))
        );
    }

    // ---- failed is never reported as up to date ----

    #[test]
    fn failure_404_is_typed_not_uptodate() {
        let t = FakeTransport::new(vec![resp(404, Vec::new())]);
        assert_eq!(
            check_once(t.as_ref(), CURRENT),
            Err((CheckReason::Unavailable, None))
        );
    }

    #[test]
    fn failure_429_carries_retry_after() {
        let t = FakeTransport::new(vec![Ok(HttpResponse {
            status: 429,
            location: None,
            retry_after: Some(Duration::from_secs(7200)),
            body: Vec::new(),
        })]);
        assert_eq!(
            check_once(t.as_ref(), CURRENT),
            Err((CheckReason::RateLimited, Some(Duration::from_secs(7200))))
        );
    }

    #[test]
    fn failure_tls_is_typed_not_uptodate() {
        let t = FakeTransport::new(vec![Err(TransportError::Tls)]);
        assert_eq!(
            check_once(t.as_ref(), CURRENT),
            Err((CheckReason::Tls, None))
        );
    }

    #[test]
    fn failure_offline_and_connection_are_typed() {
        for (e, reason) in [
            (TransportError::Offline, CheckReason::Offline),
            (TransportError::Connection, CheckReason::Connection),
        ] {
            let t = FakeTransport::new(vec![Err(e)]);
            assert_eq!(check_once(t.as_ref(), CURRENT), Err((reason, None)));
        }
    }

    #[test]
    fn failure_oversized_body_is_typed_not_uptodate() {
        let huge = vec![b'x'; MAX_BODY_BYTES + 1];
        let t = FakeTransport::new(vec![resp(200, huge)]);
        assert_eq!(
            check_once(t.as_ref(), CURRENT),
            Err((CheckReason::Unavailable, None))
        );
    }

    #[test]
    fn failure_malformed_json_is_typed_not_uptodate() {
        for body in [
            b"not json".to_vec(),
            b"{}".to_vec(),
            br#"{"version":"0.4.0"}"#.to_vec(),
            // Extra fields: the manifest contract is exact.
            br#"{"version":"0.4.0","notes_url":"https://github.com/cortexuvula/clip-ocr/releases/tag/v0.4.0","extra":1}"#.to_vec(),
        ] {
            let t = FakeTransport::new(vec![resp(200, body)]);
            assert_eq!(
                check_once(t.as_ref(), CURRENT),
                Err((CheckReason::Unavailable, None)),
                "body must fail closed"
            );
        }
    }

    #[test]
    fn failure_foreign_notes_url_is_typed_not_uptodate() {
        for url in [
            "http://github.com/cortexuvula/clip-ocr/releases/tag/v0.4.0",
            "https://evil.test/cortexuvula/clip-ocr",
            "https://github.com/other/repo/releases/tag/v0.4.0",
            "https://github.com.evil.test/cortexuvula/clip-ocr",
        ] {
            let t = FakeTransport::new(vec![resp(200, ok_body("0.4.0", url))]);
            assert_eq!(
                check_once(t.as_ref(), CURRENT),
                Err((CheckReason::Unavailable, None)),
                "{url} must fail closed"
            );
        }
    }

    // ---- request discipline ----

    #[test]
    fn success_path_makes_exactly_one_request_and_reports_available() {
        let t = FakeTransport::new(vec![resp(200, ok_body("0.4.0", NOTES))]);
        assert_eq!(
            check_once(t.as_ref(), CURRENT),
            Ok(CheckOutcome::Available {
                latest: "0.4.0".into(),
                notes_url: NOTES.into()
            })
        );
        assert_eq!(t.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn equal_version_reports_up_to_date_not_available() {
        let t = FakeTransport::new(vec![resp(200, ok_body(CURRENT, NOTES))]);
        assert_eq!(
            check_once(t.as_ref(), CURRENT),
            Ok(CheckOutcome::UpToDate {
                latest: CURRENT.into()
            })
        );
    }

    #[test]
    fn allowlisted_redirect_hops_are_followed_exactly_once_each() {
        let t = FakeTransport::new(vec![
            Ok(HttpResponse {
                status: 302,
                location: Some("https://objects.githubusercontent.com/signed/asset?x=1".into()),
                retry_after: None,
                body: Vec::new(),
            }),
            resp(200, ok_body("0.4.0", NOTES)),
        ]);
        assert!(check_once(t.as_ref(), CURRENT).is_ok());
        assert_eq!(t.calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn redirect_to_non_allowlisted_host_is_rejected() {
        for host in [
            "evil.test",
            "http://github.com",
            "objects.githubusercontent.com.evil.test",
        ] {
            let location = format!("https://{host}/asset");
            let t = FakeTransport::new(vec![Ok(HttpResponse {
                status: 302,
                location: Some(location),
                retry_after: None,
                body: Vec::new(),
            })]);
            assert_eq!(
                check_once(t.as_ref(), CURRENT),
                Err((CheckReason::Unavailable, None))
            );
            // One request only: the redirect was never followed.
            assert_eq!(t.calls.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn redirect_loop_terminates_typed() {
        let hop = || {
            Ok(HttpResponse {
                status: 302,
                location: Some("https://github.com/again".into()),
                retry_after: None,
                body: Vec::new(),
            })
        };
        let t = FakeTransport::new(vec![hop(), hop(), hop(), hop()]);
        assert_eq!(
            check_once(t.as_ref(), CURRENT),
            Err((CheckReason::Unavailable, None))
        );
        assert!(t.calls.load(Ordering::SeqCst) <= MAX_REDIRECT_HOPS + 1);
    }

    #[test]
    fn missing_location_on_redirect_fails_closed() {
        let t = FakeTransport::new(vec![Ok(HttpResponse {
            status: 302,
            location: None,
            retry_after: None,
            body: Vec::new(),
        })]);
        assert_eq!(
            check_once(t.as_ref(), CURRENT),
            Err((CheckReason::Unavailable, None))
        );
    }

    // ---- cadence ----

    #[test]
    fn cadence_not_due_before_launch_delay_or_24h() {
        let launch = Instant::now();
        let c = Cadence {
            launch,
            last_attempt: None,
            last_success: Some(launch),
            failures: 0,
            retry_after_override: None,
        };
        // Healthy cadence: exactly 24h after the last success, no sooner.
        assert!(!c.due(
            launch + MIN_DELAY_AFTER_LAUNCH,
            Duration::ZERO,
            false,
            false
        ));
        assert!(!c.due(
            launch + CHECK_INTERVAL - Duration::from_secs(1),
            Duration::ZERO,
            false,
            false
        ));
        assert!(c.due(launch + CHECK_INTERVAL, Duration::ZERO, false, false));
    }

    #[test]
    fn cadence_first_check_waits_for_launch_delay_plus_jitter() {
        let launch = Instant::now();
        let c = Cadence {
            launch,
            last_attempt: None,
            last_success: None,
            failures: 0,
            retry_after_override: None,
        };
        assert!(!c.due(
            launch + Duration::from_secs(10),
            Duration::ZERO,
            false,
            false
        ));
        assert!(!c.due(
            launch + MIN_DELAY_AFTER_LAUNCH - Duration::from_secs(1),
            Duration::ZERO,
            false,
            false
        ));
        // Jitter only ever ADDS delay.
        assert!(!c.due(
            launch + MIN_DELAY_AFTER_LAUNCH,
            Duration::from_secs(60),
            false,
            false
        ));
        assert!(c.due(
            launch + MIN_DELAY_AFTER_LAUNCH + Duration::from_secs(60),
            Duration::from_secs(60),
            false,
            false
        ));
    }

    #[test]
    fn cadence_jitter_bounds() {
        assert!(current_jitter() < MAX_JITTER);
    }

    #[test]
    fn cadence_deferred_while_busy_or_in_flight() {
        let launch = Instant::now();
        let mut c = Cadence {
            launch,
            last_attempt: None,
            last_success: None,
            failures: 0,
            retry_after_override: None,
        };
        // Overdue by any measure (virtual now = 24h past the last
        // success), but a capture/OCR lease or a check in flight defers
        // it (no catch-up stacking).
        c.last_success = Some(launch);
        let now = launch + CHECK_INTERVAL;
        assert!(!c.due(now, Duration::ZERO, true, false));
        assert!(!c.due(now, Duration::ZERO, false, true));
        assert!(c.due(now, Duration::ZERO, false, false));
    }

    #[test]
    fn cadence_backoff_ladder_and_retry_after_override() {
        let launch = Instant::now();
        let mut c = Cadence {
            launch,
            last_attempt: Some(launch),
            last_success: None,
            failures: 1,
            retry_after_override: None,
        };
        assert_eq!(c.effective_backoff(), BACKOFF_LADDER[0]); // 6h
        assert!(!c.due(
            launch + BACKOFF_LADDER[0] - Duration::from_secs(1),
            Duration::ZERO,
            false,
            false
        ));
        assert!(c.due(launch + BACKOFF_LADDER[0], Duration::ZERO, false, false));

        c.failures = 2;
        assert_eq!(c.effective_backoff(), BACKOFF_LADDER[1]); // 12h
        c.failures = 5; // beyond the ladder: capped at 24h
        assert_eq!(c.effective_backoff(), BACKOFF_LADDER[2]);

        // A longer Retry-After extends the wait; a shorter one does not.
        c.retry_after_override = Some(Duration::from_secs(48 * 60 * 60));
        assert_eq!(c.effective_backoff(), Duration::from_secs(48 * 60 * 60));
        c.retry_after_override = Some(Duration::from_secs(60));
        assert_eq!(c.effective_backoff(), BACKOFF_LADDER[2]);
    }

    // ---- manager state machine ----

    #[test]
    fn manager_records_failure_never_as_uptodate_and_backs_off() {
        let t = FakeTransport::new(vec![resp(404, Vec::new())]);
        let m = UpdateManager::with_version(t.clone(), true, CURRENT);
        m.request_manual();
        let lease = m
            .tick(Instant::now(), Duration::ZERO, false)
            .expect("manual check due");
        assert_eq!(m.status(), UpdateStatus::Checking);
        m.run_check(&lease, SystemTime::UNIX_EPOCH);
        assert_eq!(
            m.status(),
            UpdateStatus::Failed {
                reason: CheckReason::Unavailable
            }
        );
        assert!(m.last_success().is_none());
        // Not due again until the 6h backoff elapses (failures = 1).
        let now = Instant::now();
        assert!(
            !m.tick(now + Duration::from_secs(60), Duration::ZERO, false)
                .is_some(),
            "automatic re-check must respect the backoff ladder"
        );
    }

    #[test]
    fn manager_single_flight_and_manual_pending() {
        let t = FakeTransport::new(vec![resp(200, ok_body(CURRENT, NOTES))]);
        let m = UpdateManager::with_version(t.clone(), false, CURRENT);
        // Manual request while not busy starts exactly one check.
        m.request_manual();
        let lease = m
            .tick(Instant::now(), Duration::ZERO, false)
            .expect("manual check starts");
        // While in flight: a second manual request defers (no second lease).
        m.request_manual();
        assert!(m.tick(Instant::now(), Duration::ZERO, false).is_none());
        m.run_check(&lease, SystemTime::UNIX_EPOCH);
        drop(lease); // the resident drops the guard when the check returns
        assert_eq!(
            m.status(),
            UpdateStatus::UpToDate {
                latest: CURRENT.into()
            }
        );
        // The in-window re-arm is now IGNORED (anti-spam, 30s window):
        // no lease, no second request. The re-arm policy is covered in
        // manager_manual_rearm_inside_window_is_ignored.
        assert!(
            m.tick(Instant::now(), Duration::ZERO, false).is_none(),
            "in-window manual re-arm must be ignored"
        );
        assert_eq!(t.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn manager_manual_rearm_inside_window_is_ignored() {
        let t = FakeTransport::new(vec![
            resp(200, ok_body(CURRENT, NOTES)),
            resp(200, ok_body(CURRENT, NOTES)),
        ]);
        let m = UpdateManager::with_version(t.clone(), false, CURRENT);
        m.request_manual();
        let lease = m.tick(Instant::now(), Duration::ZERO, false).unwrap();
        m.run_check(&lease, SystemTime::UNIX_EPOCH);
        drop(lease);
        assert_eq!(t.calls.load(Ordering::SeqCst), 1);
        // A tight re-arm loop inside the window: requests are IGNORED (the
        // flag never becomes pending, so no second network call happens).
        for _ in 0..5 {
            m.request_manual();
            assert!(
                m.tick(Instant::now(), Duration::ZERO, false).is_none(),
                "no second check inside the manual window"
            );
        }
        assert_eq!(t.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn manager_deferred_while_capture_busy() {
        let t = FakeTransport::new(vec![resp(200, ok_body(CURRENT, NOTES))]);
        let m = UpdateManager::with_version(t.clone(), true, CURRENT);
        m.request_manual();
        assert!(m.tick(Instant::now(), Duration::ZERO, true).is_none());
        assert_eq!(t.calls.load(Ordering::SeqCst), 0);
        // The pending flag survives the deferral.
        assert!(m.tick(Instant::now(), Duration::ZERO, false).is_some());
    }

    #[test]
    fn manager_available_keeps_notes_url_and_label() {
        let t = FakeTransport::new(vec![resp(200, ok_body("0.4.0", NOTES))]);
        let m = UpdateManager::with_version(t, true, CURRENT);
        m.request_manual();
        let lease = m.tick(Instant::now(), Duration::ZERO, false).unwrap();
        m.run_check(&lease, SystemTime::UNIX_EPOCH);
        assert_eq!(m.status().tray_label(), "Update available: v0.4.0");
        assert_eq!(m.notes_url().as_deref(), Some(NOTES));
    }

    #[test]
    fn manager_initial_states_off_vs_not_checked() {
        let off = UpdateManager::with_version(FakeTransport::new(vec![]), false, CURRENT);
        assert_eq!(off.status().tray_label(), "Updates: Off");
        let on = UpdateManager::with_version(FakeTransport::new(vec![]), true, CURRENT);
        assert_eq!(on.status().tray_label(), "Updates: Not checked");
    }

    // ---- time formatting ----

    #[test]
    fn format_utc_known_timestamps() {
        assert_eq!(format_utc(SystemTime::UNIX_EPOCH), "1970-01-01 00:00 UTC");
        // 2026-08-14 00:00:00 UTC = 1786684800.
        let t = UNIX_EPOCH + Duration::from_secs(1_786_665_600);
        assert_eq!(format_utc(t), "2026-08-14 00:00 UTC");
        // End-of-month and leap-day boundaries.
        let t = UNIX_EPOCH + Duration::from_secs(1_782_624_000); // 2026-06-28? computed below
        let s = format_utc(t);
        assert!(s.ends_with("UTC") && s.len() == "YYYY-MM-DD HH:MM UTC".len());
    }
}

#[cfg(test)]
mod hostile_url_tests {
    use super::*;

    #[test]
    fn canonical_release_url_rejects_hostile_inputs() {
        // Exact shapes the launcher accepts: the tag URLs release.yml
        // generates.
        assert!(is_canonical_release_url(
            "https://github.com/cortexuvula/clip-ocr/releases/tag/v0.4.0"
        ));
        assert!(is_canonical_release_url(
            "https://github.com/cortexuvula/clip-ocr/releases/tag/v1.2.3-rc.1"
        ));
        // Nothing hostile reaches the launch path: quotes (cmd metachars
        // via `start`), ampersands, pipes, semicolons, percent-encoding,
        // dot-dot segments, backslashes, spaces, queries, fragments, other
        // hosts, and empty/degenerate tails are all rejected.
        for hostile in [
            r#"https://github.com/cortexuvula/clip-ocr/releases/tag/v0.4.0\"&calc"#,
            "https://github.com/cortexuvula/clip-ocr/releases/tag/v0.4.0&calc",
            "https://github.com/cortexuvula/clip-ocr/releases/tag/v0.4.0|calc",
            "https://github.com/cortexuvula/clip-ocr/releases/tag/v0.4.0;calc",
            "https://github.com/cortexuvula/clip-ocr/releases/tag/v0.4.0%00",
            "https://github.com/cortexuvula/clip-ocr/releases/tag/v0.4.0%41",
            "https://github.com/cortexuvula/clip-ocr/releases/tag/v..",
            "https://github.com/cortexuvula/clip-ocr/releases/tag/v0.4.0/../evil",
            "https://github.com/cortexuvula/clip-ocr/releases/tag/v0.4.0\\evil",
            "https://github.com/cortexuvula/clip-ocr/releases/tag/v 0.4.0",
            "https://github.com/cortexuvula/clip-ocr/releases/tag/v0.4.0?x=1",
            "https://github.com/cortexuvula/clip-ocr/releases/tag/v0.4.0#frag",
            "https://evil.test/cortexuvula/clip-ocr/releases/tag/v0.4.0",
            "https://github.com/cortexuvula/clip-ocr/releases/tag/",
            "https://github.com/cortexuvula/clip-ocr/releases/tag/v#",
            "",
        ] {
            assert!(
                !is_canonical_release_url(hostile),
                "{hostile:?} must never reach the launch path"
            );
        }
    }
}
