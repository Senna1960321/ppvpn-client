//! Client for the privileged `ppvpn-service`. The wire types must stay
//! byte-identical to `service/src/protocol.rs`: requests and responses are
//! HMAC-signed over their canonical JSON (signature field empty), so any
//! drift in field order or naming fails verification.
//!
//! Wire format: `[u32 BE length][JSON]`, one request per connection (a
//! `Watch` keeps its connection open for events, see [`ServiceApi::watch`]).
//! An unsigned `Handshake` returns a short-lived token + HMAC key bound to
//! this process's PID; every later envelope is signed with that key.
//!
//! Installing and uninstalling the service is platform work and lives in
//! [`crate::PlatformHooks`]; this module only talks to a running service.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Sha256;

use crate::core_ipc::{ApplyChoices, BoxFuture, CoreCallError, CoreTransport};
use crate::errors::{ClientErrorInfo, ErrorCode};
use crate::RoutingMode;

/// Named pipe served by the Windows service.
#[cfg(windows)]
pub(crate) const SERVICE_ENDPOINT: &str = r"\\.\pipe\ppvpn-service";
/// Unix socket served by the macOS LaunchDaemon.
#[cfg(target_os = "macos")]
pub(crate) const SERVICE_ENDPOINT: &str = "/Library/Application Support/PPVPN/run/service.sock";
/// Unix socket served by the Linux systemd service (owned by the Linux
/// packaging team; keep in sync with their `service/` protocol constant).
#[cfg(all(unix, not(target_os = "macos")))]
pub(crate) const SERVICE_ENDPOINT: &str = "/run/ppvpn/service.sock";
#[cfg(not(any(unix, windows)))]
pub(crate) const SERVICE_ENDPOINT: &str = "";

/// Where the service expects the calling app (its identity check); quoted in
/// `ServiceClientRejected` details.
#[cfg(target_os = "macos")]
const EXPECTED_CLIENT_LOCATION: &str = "/Applications/PPVPN.app";
#[cfg(windows)]
const EXPECTED_CLIENT_LOCATION: &str = "ppvpn.exe in the service's install directory";
#[cfg(all(unix, not(target_os = "macos")))]
const EXPECTED_CLIENT_LOCATION: &str = "/usr/lib/ppvpn/ppvpn";
#[cfg(not(any(unix, windows)))]
const EXPECTED_CLIENT_LOCATION: &str = "the install location";

const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
/// Handshake sessions live 120 s on the service; renew 15 s early.
const AUTH_RENEW_MARGIN_SECS: u64 = 15;

// ---------------------------------------------------------------------------
// Protocol types (mirror service/src/protocol.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Command {
    Handshake,
    GetVersion,
    GetStatus,
    Connect,
    UpdateProfile,
    RenewLease,
    Disconnect,
    CoreApi,
    Watch,
    /// Silent desktop update (#548, docs/host-integration.md section 16).
    InstallUpdate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Request {
    id: String,
    timestamp: u64,
    command: Command,
    payload: Value,
    #[serde(default)]
    auth_token: Option<String>,
    signature: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Response {
    id: String,
    success: bool,
    data: Option<Value>,
    error: Option<String>,
    signature: String,
}

#[derive(Debug, Clone, Deserialize)]
struct HandshakeResponse {
    auth_token: String,
    auth_key: String,
    expires_at: u64,
}

/// Identifies one enhanced-mode connection attempt. The service only accepts
/// lease renewals, updates and Core API calls from the exact owner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SessionRef {
    pub session_id: String,
    pub generation: u64,
}

#[derive(Debug, Clone, Serialize)]
struct ProfilePayload<'a> {
    #[serde(flatten)]
    session: &'a SessionRef,
    profile: &'a Value,
    take_over: bool,
    /// Authority of the API the profile came from. The service passes it to
    /// the core's `apply-profile` when the core accepts it (0.5.0+); older
    /// services ignore the field.
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    allowed_rule_set_hosts: &'a [String],
    /// `rules` / `global`; the service passes it to cores that accept it
    /// (0.5.6+); older services ignore the field.
    routing_mode: &'static str,
    /// `selected_node_id` and `pins`, passed on with the profile to the
    /// core's `apply-profile`; older services ignore them.
    #[serde(flatten)]
    choices: &'a ApplyChoices,
}

#[derive(Debug, Clone, Serialize)]
struct CoreApiPayload<'a> {
    #[serde(flatten)]
    session: &'a SessionRef,
    path: &'a str,
    body: Value,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ServiceStatus {
    pub running: bool,
    pub pid: i64,
    pub bin_path: String,
    pub mode: String,
    pub service_version: String,
    /// The service's build id; `None` from older services.
    #[serde(default)]
    pub service_build_id: Option<String>,
    pub session_id: Option<String>,
    pub generation: u64,
    pub profile_revision: Option<String>,
    pub lease_expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct VersionInfo {
    pub service: String,
    pub version: String,
    /// The service's build id; `None` from older services.
    #[serde(default)]
    pub build_id: Option<String>,
    /// The release's build number baked into the service (0 in a build
    /// without one). This and the next two are absent from services that
    /// predate `InstallUpdate`: `None`, no silent updates.
    #[serde(default)]
    pub build: Option<u64>,
    /// The silent update being installed, if any.
    #[serde(default)]
    pub update: Option<UpdateProgress>,
    /// The last silent update's result, if any.
    #[serde(default)]
    pub last_update: Option<UpdateResult>,
}

/// `InstallUpdate`'s payload (section 16.2).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct InstallUpdatePayload {
    pub path: String,
    pub version: String,
    pub build: u64,
    pub length: u64,
    pub ed_signature: String,
    pub relaunch: bool,
}

/// A silent update the service is installing (`GetVersion`'s `update`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateProgress {
    pub version: String,
    pub build: u64,
    /// `waiting` (for the app to exit) or `installing`.
    pub phase: String,
}

/// The last silent update's result (`GetVersion`'s `last_update`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateResult {
    pub version: String,
    pub build: u64,
    /// `installed` or `failed`.
    pub outcome: String,
    /// The `UPDATE_*` code of a failure (`UPDATE_INSTALL_FAILED:<detail>`
    /// for the installer's); `None` when installed.
    #[serde(default)]
    pub error: Option<String>,
    pub finished_at_ms: u64,
}

/// Build id of the service shipped with this client (build.rs: SHA-256 of
/// `service/`'s sources), or `"unknown"` when the crate was built without
/// that tree.
pub(crate) const SERVICE_BUILD_ID: &str = env!("PPVPN_SERVICE_BUILD_ID");

/// [`SERVICE_BUILD_ID`] when known: the id an installed service must report.
pub(crate) fn expected_service_build_id() -> Option<String> {
    (SERVICE_BUILD_ID != "unknown").then(|| SERVICE_BUILD_ID.to_string())
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn sign_with(key: &[u8], message: &str) -> Option<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).ok()?;
    mac.update(message.as_bytes());
    Some(hex::encode(mac.finalize().into_bytes()))
}

fn verify_with(key: &[u8], message: &str, signature: &str) -> bool {
    let Ok(signature) = hex::decode(signature) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(key) else {
        return false;
    };
    mac.update(message.as_bytes());
    mac.verify_slice(&signature).is_ok()
}

impl Request {
    fn signed(
        command: Command,
        payload: Value,
        auth_token: String,
        key: &[u8],
    ) -> Result<Self, ServiceError> {
        let mut request = Request {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_epoch(),
            command,
            payload,
            auth_token: Some(auth_token),
            signature: String::new(),
        };
        let canonical = serde_json::to_string(&request)
            .map_err(|error| ServiceError::Protocol(format!("encode request: {error}")))?;
        request.signature = sign_with(key, &canonical)
            .ok_or_else(|| ServiceError::Protocol("HMAC init".to_string()))?;
        Ok(request)
    }

    fn handshake() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now_epoch(),
            command: Command::Handshake,
            payload: serde_json::json!({
                "client_nonce": uuid::Uuid::new_v4().simple().to_string(),
            }),
            auth_token: None,
            signature: String::new(),
        }
    }
}

impl Response {
    fn verify_with(&self, key: &[u8]) -> bool {
        let bare = Response {
            signature: String::new(),
            ..self.clone()
        };
        match serde_json::to_string(&bare) {
            Ok(canonical) => verify_with(key, &canonical, &self.signature),
            Err(_) => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure of one service call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ServiceError {
    /// Nothing listening (not installed, not started, wrong endpoint).
    Unavailable(String),
    /// The service closed the connection before answering the handshake —
    /// how it rejects a client whose executable identity it does not accept.
    Rejected(String),
    /// Malformed or unverifiable envelope, lifetime or key.
    Protocol(String),
    /// The service answered `success:false`; the string is its error, e.g.
    /// `CONNECTION_OWNED_BY_ANOTHER_SESSION` or a forwarded `CODE: message`.
    Failed(String),
}

impl ServiceError {
    pub(crate) fn detail(&self) -> String {
        match self {
            Self::Unavailable(detail)
            | Self::Rejected(detail)
            | Self::Protocol(detail)
            | Self::Failed(detail) => detail.clone(),
        }
    }

    /// Nothing accepted the connection: the service process is not running
    /// (as opposed to a call that timed out or broke off midway).
    pub(crate) fn not_listening(&self) -> bool {
        matches!(self, Self::Unavailable(detail)
            if detail.starts_with("connect") || detail.starts_with("open pipe"))
    }

    /// Stable reason code carried by a `Failed` error (`CODE` or `CODE: …`).
    pub(crate) fn reason_code(&self) -> Option<&str> {
        match self {
            Self::Failed(detail) => {
                let code = detail.split(':').next().unwrap_or_default().trim();
                let is_code = !code.is_empty()
                    && code
                        .bytes()
                        .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
                is_code.then_some(code)
            }
            _ => None,
        }
    }

    /// User-facing mapping for failures outside the connect flow.
    pub(crate) fn info(&self) -> ClientErrorInfo {
        let code = match self {
            Self::Unavailable(_) => ErrorCode::ServiceUnavailable,
            Self::Rejected(_) => ErrorCode::ServiceClientRejected,
            Self::Protocol(_) => ErrorCode::ServiceIncompatible,
            Self::Failed(_) => service_reason_error_code(self.reason_code().unwrap_or_default()),
        };
        ClientErrorInfo::new(code, self.detail())
    }
}

/// Maps a service/forwarded-core reason code to a user code.
pub(crate) fn service_reason_error_code(code: &str) -> ErrorCode {
    match code {
        "CONNECTION_OWNED_BY_ANOTHER_SESSION" | "STALE_OR_FOREIGN_SESSION" => {
            ErrorCode::ServiceBusy
        }
        "CONNECTION_OWNED_BY_ANOTHER_USER" => ErrorCode::ServiceOwnedByAnotherUser,
        "PROFILE_SCHEMA_UNSUPPORTED"
        | "CORE_SCHEMA_CAPABILITY_MISSING"
        | "CORE_API_UNSUPPORTED" => ErrorCode::ServiceIncompatible,
        "PROFILE_REVISION_REQUIRED" | "PROFILE_SCHEMA_REQUIRED" => ErrorCode::ProfileInvalid,
        // The service is shutting down: retried after a pause.
        "SERVICE_STOPPING" => ErrorCode::ServiceUnavailable,
        "NODE_NOT_FOUND" => ErrorCode::NodeNotFound,
        "PROFILE_EXPIRED" => ErrorCode::ProfileExpired,
        code if crate::core_ipc::is_profile_error(code) => ErrorCode::ProfileInvalid,
        _ => ErrorCode::ConnectFailed,
    }
}

// ---------------------------------------------------------------------------
// Watch
// ---------------------------------------------------------------------------

/// How a watch of the session's core ended (see [`ServiceApi::watch`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum WatchEnd {
    /// The service predates `Watch` (it closed the connection without an
    /// answer): only the lease renewal notices a lost core.
    Unsupported,
    /// The service answered the `Watch` with an error.
    Refused(ServiceError),
    /// The service is shutting down (its core stops next).
    Stopping,
    /// The session's core stopped, with the service's reason.
    CoreStopped(String),
    /// The watch connection broke (EOF, reset, broken pipe) or could not be
    /// opened: the service is gone.
    Lost(String),
    /// The watch went silent or sent something unverifiable; not proof of
    /// anything, so only the lease renewal decides.
    Abandoned(String),
    /// Stopped from this side (the watching task was cancelled).
    Cancelled,
}

/// How often a blocked watch read checks for cancellation and silence.
const WATCH_POLL: Duration = Duration::from_millis(100);
/// How long the service may take to answer a `Watch`.
const WATCH_ANSWER_TIMEOUT: Duration = Duration::from_secs(10);
/// Keepalive pause assumed when the answer names none.
const WATCH_DEFAULT_KEEPALIVE_MS: u64 = 5_000;

/// Silence after which a watch is abandoned: three missed keepalives.
fn watch_silence_limit(keepalive_ms: u64) -> Duration {
    Duration::from_millis(keepalive_ms.clamp(100, 60_000).saturating_mul(3))
        + Duration::from_secs(1)
}

/// Reads that return `Ok(None)` when nothing arrived within a poll interval.
trait PollRead {
    fn poll_read(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>>;
}

#[cfg(unix)]
impl PollRead for std::os::unix::net::UnixStream {
    /// With a read timeout of [`WATCH_POLL`] set.
    fn poll_read(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        use std::io::{ErrorKind, Read};
        match self.read(buf) {
            Ok(read) => Ok(Some(read)),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

/// A named pipe read only when `PeekNamedPipe` reports data, so the reader
/// never blocks and notices cancellation.
#[cfg(windows)]
struct PolledPipe(std::fs::File);

#[cfg(windows)]
impl PollRead for PolledPipe {
    fn poll_read(&mut self, buf: &mut [u8]) -> std::io::Result<Option<usize>> {
        use std::io::Read;
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::System::Pipes::PeekNamedPipe;
        const ERROR_BROKEN_PIPE: i32 = 109;
        const ERROR_PIPE_NOT_CONNECTED: i32 = 233;
        let mut available = 0u32;
        // SAFETY: a valid pipe handle; only the byte count is written.
        let peeked = unsafe {
            PeekNamedPipe(
                self.0.as_raw_handle() as _,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut available,
                std::ptr::null_mut(),
            )
        };
        if peeked == 0 {
            let error = std::io::Error::last_os_error();
            return match error.raw_os_error() {
                // The service closed its end: EOF.
                Some(ERROR_BROKEN_PIPE | ERROR_PIPE_NOT_CONNECTED) => Ok(Some(0)),
                _ => Err(error),
            };
        }
        if available == 0 {
            std::thread::sleep(WATCH_POLL);
            return Ok(None);
        }
        let wanted = buf.len().min(available as usize);
        match self.0.read(&mut buf[..wanted]) {
            Ok(read) => Ok(Some(read)),
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(Some(0)),
            Err(error) => Err(error),
        }
    }
}

#[cfg(windows)]
impl std::io::Write for PolledPipe {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// One frame of a watch connection.
enum Polled {
    Frame(Vec<u8>),
    /// Closed; `clean`: before any byte of a frame.
    Eof {
        clean: bool,
    },
    Silent,
    Cancelled,
}

fn fill<R: PollRead>(
    reader: &mut R,
    buf: &mut [u8],
    cancel: &AtomicBool,
    deadline: Instant,
) -> std::io::Result<Result<(), Polled>> {
    let mut filled = 0;
    while filled < buf.len() {
        if cancel.load(Ordering::SeqCst) {
            return Ok(Err(Polled::Cancelled));
        }
        match reader.poll_read(&mut buf[filled..])? {
            Some(0) => return Ok(Err(Polled::Eof { clean: false })),
            Some(read) => filled += read,
            None if Instant::now() >= deadline => return Ok(Err(Polled::Silent)),
            None => {}
        }
    }
    Ok(Ok(()))
}

/// Reads one `[u32 BE length][body]` frame before `deadline`.
fn poll_frame<R: PollRead>(
    reader: &mut R,
    cancel: &AtomicBool,
    deadline: Instant,
) -> std::io::Result<Polled> {
    let mut length = [0u8; 4];
    if let Err(polled) = fill(reader, &mut length[..1], cancel, deadline)? {
        return Ok(match polled {
            Polled::Eof { .. } => Polled::Eof { clean: true },
            other => other,
        });
    }
    if let Err(polled) = fill(reader, &mut length[1..], cancel, deadline)? {
        return Ok(polled);
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(std::io::Error::other(format!("frame too large: {length}")));
    }
    let mut body = vec![0u8; length];
    if let Err(polled) = fill(reader, &mut body, cancel, deadline)? {
        return Ok(polled);
    }
    Ok(Polled::Frame(body))
}

/// A frame's verified `data` (request id and signature checked).
fn watch_data(frame: &[u8], request_id: &str, key: &[u8]) -> Result<Value, ServiceError> {
    let response: Response = serde_json::from_slice(frame)
        .map_err(|error| ServiceError::Protocol(format!("parse watch frame: {error}")))?;
    verify_response(response, request_id, key)
}

/// Sends a `Watch` request on `stream` and follows the connection until it
/// ends. Blocking; returns within [`WATCH_POLL`] of `cancel` being set.
fn run_watch<S: PollRead + std::io::Write>(
    stream: &mut S,
    body: &[u8],
    request_id: &str,
    key: &[u8],
    cancel: &AtomicBool,
) -> WatchEnd {
    let sent = stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .and_then(|()| stream.write_all(body))
        .and_then(|()| stream.flush());
    if let Err(error) = sent {
        return WatchEnd::Lost(format!("write watch request: {error}"));
    }
    let answer = match poll_frame(stream, cancel, Instant::now() + WATCH_ANSWER_TIMEOUT) {
        Ok(Polled::Frame(frame)) => frame,
        // How a service without `Watch` answers it: it cannot parse the
        // request and drops the connection.
        Ok(Polled::Eof { clean: true }) => return WatchEnd::Unsupported,
        Ok(Polled::Eof { clean: false }) => {
            return WatchEnd::Lost("watch answer truncated".to_string())
        }
        Ok(Polled::Silent) => return WatchEnd::Abandoned("no answer to Watch".to_string()),
        Ok(Polled::Cancelled) => return WatchEnd::Cancelled,
        Err(error) => return WatchEnd::Lost(format!("read watch answer: {error}")),
    };
    let keepalive_ms = match watch_data(&answer, request_id, key) {
        Ok(data) => data
            .get("keepalive_ms")
            .and_then(Value::as_u64)
            .unwrap_or(WATCH_DEFAULT_KEEPALIVE_MS),
        Err(error @ ServiceError::Failed(_)) => return WatchEnd::Refused(error),
        Err(error) => return WatchEnd::Abandoned(error.detail()),
    };
    let silence = watch_silence_limit(keepalive_ms);
    loop {
        let frame = match poll_frame(stream, cancel, Instant::now() + silence) {
            Ok(Polled::Frame(frame)) => frame,
            Ok(Polled::Eof { .. }) => {
                return WatchEnd::Lost("the service closed the watch connection".to_string())
            }
            Ok(Polled::Silent) => {
                return WatchEnd::Abandoned(format!("no keepalive for {} ms", silence.as_millis()))
            }
            Ok(Polled::Cancelled) => return WatchEnd::Cancelled,
            Err(error) => return WatchEnd::Lost(format!("read watch event: {error}")),
        };
        let data = match watch_data(&frame, request_id, key) {
            Ok(data) => data,
            Err(error) => return WatchEnd::Abandoned(error.detail()),
        };
        match data.get("event").and_then(Value::as_str) {
            Some("stopping") => return WatchEnd::Stopping,
            Some("core_stopped") => {
                let reason = data.get("reason").and_then(Value::as_str).unwrap_or("");
                return WatchEnd::CoreStopped(reason.to_string());
            }
            // `keepalive`, and events a newer service may add.
            _ => {}
        }
    }
}

#[cfg(unix)]
fn blocking_watch(
    endpoint: &str,
    body: &[u8],
    request_id: &str,
    key: &[u8],
    cancel: &AtomicBool,
) -> WatchEnd {
    use std::os::unix::net::UnixStream;
    let mut stream = match UnixStream::connect(endpoint) {
        Ok(stream) => stream,
        Err(error) => return WatchEnd::Lost(format!("connect {endpoint}: {error}")),
    };
    stream.set_read_timeout(Some(WATCH_POLL)).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    run_watch(&mut stream, body, request_id, key, cancel)
}

#[cfg(windows)]
fn blocking_watch(
    endpoint: &str,
    body: &[u8],
    request_id: &str,
    key: &[u8],
    cancel: &AtomicBool,
) -> WatchEnd {
    match open_pipe(endpoint) {
        Ok(pipe) => run_watch(&mut PolledPipe(pipe), body, request_id, key, cancel),
        Err(error) => WatchEnd::Lost(error),
    }
}

#[cfg(not(any(unix, windows)))]
fn blocking_watch(_: &str, _: &[u8], _: &str, _: &[u8], _: &AtomicBool) -> WatchEnd {
    WatchEnd::Unsupported
}

/// Sets the flag when dropped: the watching future was dropped (its task
/// aborted), so the blocking reader stops and closes the connection.
struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Service API abstraction (faked in enhanced-mode tests)
// ---------------------------------------------------------------------------

/// The privileged service's command set.
pub(crate) trait ServiceApi: Send + Sync {
    fn get_version(&self) -> BoxFuture<'_, Result<VersionInfo, ServiceError>>;
    fn get_status(&self) -> BoxFuture<'_, Result<ServiceStatus, ServiceError>>;
    /// Start the TUN core for `session` with `profile` (applied with
    /// `choices`); returns its pid. `take_over` replaces a core owned by
    /// another session of the same OS user (service capability `take_over`).
    fn connect<'a>(
        &'a self,
        session: &'a SessionRef,
        profile: &'a Value,
        take_over: bool,
        routing_mode: RoutingMode,
        choices: &'a ApplyChoices,
    ) -> BoxFuture<'a, Result<u32, ServiceError>>;
    /// Apply `profile` (or the same profile in another `routing_mode`) to
    /// the running core without a reconnect, with `choices`.
    fn update_profile<'a>(
        &'a self,
        session: &'a SessionRef,
        profile: &'a Value,
        routing_mode: RoutingMode,
        choices: &'a ApplyChoices,
    ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>>;
    fn renew_lease<'a>(
        &'a self,
        session: &'a SessionRef,
    ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>>;
    fn disconnect<'a>(&'a self, session: &'a SessionRef)
        -> BoxFuture<'a, Result<(), ServiceError>>;
    /// Forward one Core API call to the session's core.
    fn core_api<'a>(
        &'a self,
        session: &'a SessionRef,
        path: &'a str,
        body: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, ServiceError>>;
    /// Watches `session`'s core over a long-lived connection; resolves when
    /// the service reports that the core stopped or that it is stopping,
    /// when the connection breaks, or when watching is not possible. Dropping
    /// the future ends the watch.
    fn watch<'a>(&'a self, _session: &'a SessionRef) -> BoxFuture<'a, WatchEnd> {
        Box::pin(async { WatchEnd::Unsupported })
    }
    /// Hands a downloaded update to the service (section 16.3): `Ok` once
    /// it answered `accepted`. A service that predates the command closes
    /// the connection without an answer.
    fn install_update<'a>(
        &'a self,
        _payload: &'a InstallUpdatePayload,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(async {
            Err(ServiceError::Unavailable(
                "InstallUpdate unsupported".to_string(),
            ))
        })
    }
    /// Drop cached handshake credentials (after a reinstall).
    fn reset_auth(&self) {}
}

#[derive(Clone)]
struct IpcAuth {
    token: String,
    key: Vec<u8>,
    expires_at: u64,
}

/// Real client over the OS endpoint in [`SERVICE_ENDPOINT`].
pub(crate) struct ServiceClient {
    endpoint: String,
    auth: Mutex<Option<IpcAuth>>,
    /// `allowed_rule_set_hosts` sent with every Connect / UpdateProfile.
    rule_set_hosts: Vec<String>,
}

impl Default for ServiceClient {
    fn default() -> Self {
        Self::new(SERVICE_ENDPOINT)
    }
}

impl ServiceClient {
    pub(crate) fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into(),
            auth: Mutex::new(None),
            rule_set_hosts: Vec::new(),
        }
    }

    /// Pins the rule-set downloads of every profile this client hands the
    /// service to `hosts` (see [`crate::core_ipc::rule_set_hosts`]).
    pub(crate) fn with_rule_set_hosts(mut self, hosts: Vec<String>) -> Self {
        self.rule_set_hosts = hosts;
        self
    }

    fn cached_auth(&self) -> Option<IpcAuth> {
        let now = now_epoch();
        self.auth
            .lock()
            .ok()?
            .as_ref()
            .filter(|auth| auth.expires_at > now.saturating_add(AUTH_RENEW_MARGIN_SECS))
            .cloned()
    }

    fn store_auth(&self, auth: Option<IpcAuth>) {
        if let Ok(mut slot) = self.auth.lock() {
            *slot = auth;
        }
    }

    async fn ipc_auth(&self) -> Result<IpcAuth, ServiceError> {
        if let Some(auth) = self.cached_auth() {
            return Ok(auth);
        }
        let request = Request::handshake();
        let request_id = request.id.clone();
        let response = self
            .transact(request, Duration::from_secs(10), true)
            .await?;
        let now = now_epoch();
        let auth = validate_handshake(&response, &request_id, now)?;
        self.store_auth(Some(auth.clone()));
        Ok(auth)
    }

    async fn call(
        &self,
        command: Command,
        payload: Value,
        timeout: Duration,
    ) -> Result<Value, ServiceError> {
        match self
            .call_once(command.clone(), payload.clone(), timeout)
            .await
        {
            Err(ServiceError::Failed(error)) if error == "AUTH_SESSION_INVALID" => {
                self.store_auth(None);
                self.call_once(command, payload, timeout).await
            }
            result => result,
        }
    }

    async fn call_once(
        &self,
        command: Command,
        payload: Value,
        timeout: Duration,
    ) -> Result<Value, ServiceError> {
        let auth = self.ipc_auth().await?;
        let request = Request::signed(command, payload, auth.token.clone(), &auth.key)?;
        let request_id = request.id.clone();
        let response = self.transact(request, timeout, false).await?;
        verify_response(response, &request_id, &auth.key)
    }

    /// One framed request/response. `handshake`: the first exchange of a
    /// session, where a close before any answer means the service refused
    /// this app's identity.
    async fn transact(
        &self,
        request: Request,
        timeout: Duration,
        handshake: bool,
    ) -> Result<Response, ServiceError> {
        let body = serde_json::to_vec(&request)
            .map_err(|error| ServiceError::Protocol(format!("encode request: {error}")))?;
        let endpoint = self.endpoint.clone();
        let task = tokio::task::spawn_blocking(move || {
            blocking_transact(&endpoint, &body, timeout, handshake)
        });
        let bytes = match tokio::time::timeout(timeout + Duration::from_secs(1), task).await {
            Err(_) => {
                return Err(ServiceError::Unavailable(
                    "service call timed out".to_string(),
                ))
            }
            Ok(Err(error)) => return Err(ServiceError::Protocol(format!("service task: {error}"))),
            Ok(Ok(result)) => result?,
        };
        serde_json::from_slice(&bytes)
            .map_err(|error| ServiceError::Protocol(format!("parse response: {error}")))
    }
}

fn validate_handshake(
    response: &Response,
    request_id: &str,
    now: u64,
) -> Result<IpcAuth, ServiceError> {
    if response.id != request_id || !response.signature.is_empty() {
        return Err(ServiceError::Protocol(
            "service handshake response invalid".to_string(),
        ));
    }
    if !response.success {
        return Err(ServiceError::Failed(
            response.error.clone().unwrap_or_default(),
        ));
    }
    let body: HandshakeResponse = response
        .data
        .clone()
        .ok_or_else(|| ServiceError::Protocol("service handshake response missing".to_string()))
        .and_then(|data| {
            serde_json::from_value(data)
                .map_err(|error| ServiceError::Protocol(format!("handshake body: {error}")))
        })?;
    if body.auth_token.is_empty()
        || body.expires_at <= now.saturating_add(AUTH_RENEW_MARGIN_SECS)
        || body.expires_at > now.saturating_add(10 * 60)
    {
        return Err(ServiceError::Protocol(
            "service handshake lifetime invalid".to_string(),
        ));
    }
    let key = hex::decode(&body.auth_key)
        .map_err(|_| ServiceError::Protocol("service session key invalid".to_string()))?;
    if key.len() != 32 {
        return Err(ServiceError::Protocol(
            "service session key length invalid".to_string(),
        ));
    }
    Ok(IpcAuth {
        token: body.auth_token,
        key,
        expires_at: body.expires_at,
    })
}

fn verify_response(
    response: Response,
    request_id: &str,
    key: &[u8],
) -> Result<Value, ServiceError> {
    if response.id != request_id {
        return Err(ServiceError::Protocol(
            "service response request id mismatch".to_string(),
        ));
    }
    // Pre-authentication rejections are unsigned by design.
    if !response.success && response.signature.is_empty() {
        return Err(ServiceError::Failed(response.error.unwrap_or_default()));
    }
    if !response.verify_with(key) {
        return Err(ServiceError::Protocol(
            "service response signature invalid".to_string(),
        ));
    }
    if !response.success {
        return Err(ServiceError::Failed(response.error.unwrap_or_default()));
    }
    Ok(response.data.unwrap_or_else(|| serde_json::json!({})))
}

/// Writes `[u32 BE length]` and the body as two writes (what the service
/// expects), then reads the framed response.
fn exchange<S: std::io::Read + std::io::Write>(
    stream: &mut S,
    body: &[u8],
    handshake: bool,
) -> Result<Vec<u8>, ServiceError> {
    let write =
        |error: std::io::Error| ServiceError::Unavailable(format!("write request: {error}"));
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .map_err(write)?;
    stream.write_all(body).map_err(write)?;
    stream.flush().ok();
    read_frame(stream, handshake)
}

/// Reads one `[u32 BE length][body]` response.
///
/// A close before any byte of the handshake answer is how the service
/// rejects an app it does not trust ([`ServiceError::Rejected`]); any other
/// short read is a protocol failure.
fn read_frame<R: std::io::Read>(reader: &mut R, handshake: bool) -> Result<Vec<u8>, ServiceError> {
    let mut length = [0u8; 4];
    let mut filled = 0;
    while filled < length.len() {
        match reader.read(&mut length[filled..]) {
            Ok(0) if filled == 0 && handshake => {
                return Err(ServiceError::Rejected(format!(
                    "SERVICE_CLIENT_REJECTED: the service closed the connection without answering; \
                     it only accepts the app installed at its expected location ({})",
                    EXPECTED_CLIENT_LOCATION
                )))
            }
            Ok(0) => {
                return Err(ServiceError::Protocol(format!(
                    "SERVICE_PROTOCOL: connection closed after {filled} of 4 length bytes"
                )))
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(ServiceError::Unavailable(format!(
                    "read response length: {error}"
                )))
            }
        }
    }
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(ServiceError::Protocol(format!(
            "SERVICE_PROTOCOL: response too large: {length}"
        )));
    }
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            ServiceError::Protocol(format!(
                "SERVICE_PROTOCOL: response body truncated ({length} bytes expected): {error}"
            ))
        } else {
            ServiceError::Unavailable(format!("read response body: {error}"))
        }
    })?;
    Ok(body)
}

#[cfg(unix)]
fn blocking_transact(
    endpoint: &str,
    body: &[u8],
    timeout: Duration,
    handshake: bool,
) -> Result<Vec<u8>, ServiceError> {
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(endpoint)
        .map_err(|error| ServiceError::Unavailable(format!("connect {endpoint}: {error}")))?;
    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(Duration::from_secs(5))).ok();
    exchange(&mut stream, body, handshake)
}

#[cfg(windows)]
fn blocking_transact(
    endpoint: &str,
    body: &[u8],
    _timeout: Duration,
    handshake: bool,
) -> Result<Vec<u8>, ServiceError> {
    let mut pipe = open_pipe(endpoint).map_err(ServiceError::Unavailable)?;
    exchange(&mut pipe, body, handshake)
}

#[cfg(not(any(unix, windows)))]
fn blocking_transact(_: &str, _: &[u8], _: Duration, _: bool) -> Result<Vec<u8>, ServiceError> {
    Err(ServiceError::Unavailable(
        "service IPC unsupported on this OS".to_string(),
    ))
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T, ServiceError> {
    serde_json::from_value(value)
        .map_err(|error| ServiceError::Protocol(format!("decode: {error}")))
}

fn encode(value: impl Serialize) -> Result<Value, ServiceError> {
    serde_json::to_value(value).map_err(|error| ServiceError::Protocol(format!("encode: {error}")))
}

const SHORT_CALL: Duration = Duration::from_secs(15);
/// Connect waits for the core to start (up to 8 s) plus apply + start.
const LONG_CALL: Duration = Duration::from_secs(45);
/// InstallUpdate copies and checks the package first (section 16.3).
const INSTALL_CALL: Duration = Duration::from_secs(60);

impl ServiceApi for ServiceClient {
    fn get_version(&self) -> BoxFuture<'_, Result<VersionInfo, ServiceError>> {
        Box::pin(async move {
            decode(
                self.call(Command::GetVersion, serde_json::json!({}), SHORT_CALL)
                    .await?,
            )
        })
    }

    fn get_status(&self) -> BoxFuture<'_, Result<ServiceStatus, ServiceError>> {
        Box::pin(async move {
            decode(
                self.call(Command::GetStatus, serde_json::json!({}), SHORT_CALL)
                    .await?,
            )
        })
    }

    fn connect<'a>(
        &'a self,
        session: &'a SessionRef,
        profile: &'a Value,
        take_over: bool,
        routing_mode: RoutingMode,
        choices: &'a ApplyChoices,
    ) -> BoxFuture<'a, Result<u32, ServiceError>> {
        Box::pin(async move {
            let payload = encode(ProfilePayload {
                session,
                profile,
                take_over,
                allowed_rule_set_hosts: &self.rule_set_hosts,
                routing_mode: crate::routing::wire_name(routing_mode),
                choices,
            })?;
            let data = self.call(Command::Connect, payload, LONG_CALL).await?;
            data.get("pid")
                .and_then(Value::as_u64)
                .and_then(|pid| u32::try_from(pid).ok())
                .ok_or_else(|| ServiceError::Protocol("service returned no pid".to_string()))
        })
    }

    fn update_profile<'a>(
        &'a self,
        session: &'a SessionRef,
        profile: &'a Value,
        routing_mode: RoutingMode,
        choices: &'a ApplyChoices,
    ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
        Box::pin(async move {
            let payload = encode(ProfilePayload {
                session,
                profile,
                take_over: false,
                allowed_rule_set_hosts: &self.rule_set_hosts,
                routing_mode: crate::routing::wire_name(routing_mode),
                choices,
            })?;
            decode(
                self.call(Command::UpdateProfile, payload, LONG_CALL)
                    .await?,
            )
        })
    }

    fn renew_lease<'a>(
        &'a self,
        session: &'a SessionRef,
    ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
        Box::pin(async move {
            decode(
                self.call(Command::RenewLease, encode(session)?, SHORT_CALL)
                    .await?,
            )
        })
    }

    fn disconnect<'a>(
        &'a self,
        session: &'a SessionRef,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(async move {
            self.call(Command::Disconnect, encode(session)?, SHORT_CALL)
                .await
                .map(|_| ())
        })
    }

    fn core_api<'a>(
        &'a self,
        session: &'a SessionRef,
        path: &'a str,
        body: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, ServiceError>> {
        Box::pin(async move {
            let payload = encode(CoreApiPayload {
                session,
                path,
                body,
            })?;
            self.call(Command::CoreApi, payload, timeout).await
        })
    }

    fn install_update<'a>(
        &'a self,
        payload: &'a InstallUpdatePayload,
    ) -> BoxFuture<'a, Result<(), ServiceError>> {
        Box::pin(async move {
            let data = self
                .call(Command::InstallUpdate, encode(payload)?, INSTALL_CALL)
                .await?;
            match data.get("accepted").and_then(Value::as_bool) {
                Some(true) => Ok(()),
                _ => Err(ServiceError::Protocol(
                    "InstallUpdate: no accepted".to_string(),
                )),
            }
        })
    }

    fn watch<'a>(&'a self, session: &'a SessionRef) -> BoxFuture<'a, WatchEnd> {
        Box::pin(async move {
            let payload = match encode(session) {
                Ok(payload) => payload,
                Err(error) => return WatchEnd::Abandoned(error.detail()),
            };
            for attempt in 0..2 {
                let auth = match self.ipc_auth().await {
                    Ok(auth) => auth,
                    Err(error @ ServiceError::Unavailable(_)) => {
                        return WatchEnd::Lost(error.detail())
                    }
                    Err(error) => return WatchEnd::Refused(error),
                };
                let request =
                    match Request::signed(Command::Watch, payload.clone(), auth.token, &auth.key) {
                        Ok(request) => request,
                        Err(error) => return WatchEnd::Abandoned(error.detail()),
                    };
                let body = match serde_json::to_vec(&request) {
                    Ok(body) => body,
                    Err(error) => return WatchEnd::Abandoned(format!("encode request: {error}")),
                };
                let cancel = Arc::new(AtomicBool::new(false));
                let _cancel_on_drop = CancelOnDrop(cancel.clone());
                let endpoint = self.endpoint.clone();
                let key = auth.key;
                let end = tokio::task::spawn_blocking(move || {
                    blocking_watch(&endpoint, &body, &request.id, &key, &cancel)
                })
                .await
                .unwrap_or_else(|error| WatchEnd::Abandoned(format!("watch task: {error}")));
                match end {
                    WatchEnd::Refused(ServiceError::Failed(error))
                        if error == "AUTH_SESSION_INVALID" && attempt == 0 =>
                    {
                        self.store_auth(None);
                    }
                    end => return end,
                }
            }
            WatchEnd::Refused(ServiceError::Failed("AUTH_SESSION_INVALID".to_string()))
        })
    }

    fn reset_auth(&self) {
        self.store_auth(None);
    }
}

/// Core API over the service's `CoreApi` command, bound to one session. Lets
/// the typed helpers in [`crate::core_ipc`] drive the enhanced-mode core.
pub(crate) struct ServiceCoreTransport<S: ServiceApi + ?Sized> {
    pub service: std::sync::Arc<S>,
    pub session: SessionRef,
}

impl<S: ServiceApi + ?Sized> CoreTransport for ServiceCoreTransport<S> {
    fn call<'a>(
        &'a self,
        path: &'static str,
        body: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, CoreCallError>> {
        Box::pin(async move {
            self.service
                .core_api(&self.session, path, body, timeout)
                .await
                .map_err(forwarded_core_error)
        })
    }
}

/// The service flattens forwarded core errors to `"CODE: message"` strings.
pub(crate) fn forwarded_core_error(error: ServiceError) -> CoreCallError {
    match &error {
        ServiceError::Failed(detail) => match error.reason_code() {
            Some(code) => CoreCallError::Api {
                code: code.to_string(),
                field: None,
                message: detail
                    .split_once(':')
                    .map(|(_, message)| message.trim().to_string())
                    .unwrap_or_default(),
                retryable: false,
            },
            None => CoreCallError::Api {
                code: "CORE_OPERATION_FAILED".to_string(),
                field: None,
                message: detail.clone(),
                retryable: false,
            },
        },
        _ => CoreCallError::Transport(error.detail()),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Opens a named pipe, retrying while every server instance is busy.
#[cfg(windows)]
fn open_pipe(name: &str) -> Result<std::fs::File, String> {
    const ERROR_PIPE_BUSY: i32 = 231;
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
        {
            Ok(pipe) => return Ok(pipe),
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY)
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(format!("open pipe {name}: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The service's own copy of the build-id algorithm (service/build.rs).
    #[cfg(ppvpn_service_tree)]
    mod service_build_id {
        include!("../../service/build_id.rs");

        pub(super) fn of_tree() -> String {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../service");
            service_build_id(&dir).unwrap()
        }
    }

    #[cfg(ppvpn_service_tree)]
    #[test]
    fn build_id_matches_what_the_service_computes() {
        // Same tree, same algorithm: service/build.rs bakes exactly this
        // value into the service that ships with this client.
        assert_eq!(SERVICE_BUILD_ID.len(), 64, "{SERVICE_BUILD_ID}");
        assert!(SERVICE_BUILD_ID
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(SERVICE_BUILD_ID, service_build_id::of_tree());
        assert_eq!(
            expected_service_build_id().as_deref(),
            Some(SERVICE_BUILD_ID)
        );
    }

    #[test]
    fn services_before_the_build_id_still_parse() {
        let version: VersionInfo = serde_json::from_value(
            serde_json::json!({"service": "PPVPN Service", "version": "0.3.0"}),
        )
        .unwrap();
        assert_eq!(version.build_id, None);
        let status: ServiceStatus = serde_json::from_value(serde_json::json!({
            "running": false, "pid": -1, "bin_path": "", "mode": "",
            "service_version": "0.3.0", "session_id": null, "generation": 0,
            "profile_revision": null, "lease_expires_at_ms": null
        }))
        .unwrap();
        assert_eq!(status.service_build_id, None);
        let current: VersionInfo = serde_json::from_value(serde_json::json!({
            "service": "PPVPN Service", "version": "0.4.0", "build_id": "ab"
        }))
        .unwrap();
        assert_eq!(current.build_id.as_deref(), Some("ab"));
        assert_eq!(current.build, None);
        assert_eq!(current.last_update, None);
    }

    #[test]
    fn version_carries_the_silent_update_fields() {
        let version: VersionInfo = serde_json::from_value(serde_json::json!({
            "service": "PPVPN Service", "version": "0.7.5", "build_id": "ab",
            "build": 412,
            "update": {"version": "0.7.6", "build": 420, "phase": "installing"},
            "last_update": {
                "version": "0.7.5", "build": 412, "outcome": "failed",
                "error": "UPDATE_INSTALL_FAILED:exit 1", "finished_at_ms": 1700000000000u64
            }
        }))
        .unwrap();
        assert_eq!(version.build, Some(412));
        assert_eq!(version.update.unwrap().phase, "installing");
        let last = version.last_update.unwrap();
        assert_eq!(last.error.as_deref(), Some("UPDATE_INSTALL_FAILED:exit 1"));
        assert_eq!(last.finished_at_ms, 1_700_000_000_000);
    }

    fn sign_response(mut response: Response, key: &[u8]) -> Response {
        response.signature = String::new();
        let canonical = serde_json::to_string(&response).unwrap();
        response.signature = sign_with(key, &canonical).unwrap();
        response
    }

    #[test]
    fn request_canonical_form_matches_the_service() {
        // The service recomputes the signature over the request with an empty
        // signature, serialised in declaration order.
        let key = [7u8; 32];
        let request = Request::signed(
            Command::RenewLease,
            encode(SessionRef {
                session_id: "s".into(),
                generation: 2,
            })
            .unwrap(),
            "token".into(),
            &key,
        )
        .unwrap();
        let bare = Request {
            signature: String::new(),
            ..request.clone()
        };
        let canonical = serde_json::to_string(&bare).unwrap();
        assert!(canonical.starts_with(r#"{"id":""#));
        assert!(canonical.contains(r#""command":"RenewLease","payload":{"generation":2,"session_id":"s"},"auth_token":"token","signature":""}"#));
        assert!(verify_with(&key, &canonical, &request.signature));
    }

    #[test]
    fn session_payloads_flatten_like_the_service() {
        let session = SessionRef {
            session_id: "abc".into(),
            generation: 3,
        };
        let profile = serde_json::json!({"revision":"r1"});
        let none = ApplyChoices::default();
        let value = encode(ProfilePayload {
            session: &session,
            profile: &profile,
            take_over: false,
            allowed_rule_set_hosts: &[],
            routing_mode: "rules",
            choices: &none,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"session_id":"abc","generation":3,"profile":{"revision":"r1"},"take_over":false,"routing_mode":"rules"})
        );
        let value = encode(ProfilePayload {
            session: &session,
            profile: &profile,
            take_over: true,
            allowed_rule_set_hosts: &[],
            routing_mode: "rules",
            choices: &none,
        })
        .unwrap();
        assert_eq!(value["take_over"], true);
        let hosts = crate::core_ipc::rule_set_hosts("https://api.example.com:8443/api");
        let value = encode(ProfilePayload {
            session: &session,
            profile: &profile,
            take_over: false,
            allowed_rule_set_hosts: &hosts,
            routing_mode: "global",
            choices: &none,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"session_id":"abc","generation":3,"profile":{"revision":"r1"},
                "take_over":false,"allowed_rule_set_hosts":["api.example.com:8443"],
                "routing_mode":"global"})
        );
        // The selection and the pins, as service/src/protocol.rs reads them.
        let choices = ApplyChoices::new(
            Some("n2".into()),
            &crate::ingress::Pins::from([("n1".to_string(), "k2".to_string())]),
        );
        let value = encode(ProfilePayload {
            session: &session,
            profile: &profile,
            take_over: false,
            allowed_rule_set_hosts: &[],
            routing_mode: "rules",
            choices: &choices,
        })
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({"session_id":"abc","generation":3,"profile":{"revision":"r1"},
                "take_over":false,"routing_mode":"rules","selected_node_id":"n2",
                "pins":[{"node_id":"n1","endpoint_key":"k2"}]})
        );
        assert_eq!(
            ServiceError::Failed("CONNECTION_OWNED_BY_ANOTHER_USER".into())
                .info()
                .code,
            ErrorCode::ServiceOwnedByAnotherUser
        );
        let value = encode(CoreApiPayload {
            session: &session,
            path: "/v1/get-traffic",
            body: serde_json::json!({}),
        })
        .unwrap();
        assert_eq!(value["path"], "/v1/get-traffic");
        assert_eq!(value["session_id"], "abc");
    }

    #[test]
    fn signed_responses_verify_and_tampering_is_rejected() {
        let key = [9u8; 32];
        let response = sign_response(
            Response {
                id: "r1".into(),
                success: true,
                data: Some(serde_json::json!({"pid": 42})),
                error: None,
                signature: String::new(),
            },
            &key,
        );
        assert_eq!(
            verify_response(response.clone(), "r1", &key).unwrap()["pid"],
            42
        );
        assert!(matches!(
            verify_response(response.clone(), "other", &key),
            Err(ServiceError::Protocol(_))
        ));
        let mut tampered = response;
        tampered.data = Some(serde_json::json!({"pid": 43}));
        assert!(matches!(
            verify_response(tampered, "r1", &key),
            Err(ServiceError::Protocol(_))
        ));

        let unsigned = Response {
            id: "r2".into(),
            success: false,
            data: None,
            error: Some("AUTH_SESSION_INVALID".into()),
            signature: String::new(),
        };
        assert_eq!(
            verify_response(unsigned, "r2", &key),
            Err(ServiceError::Failed("AUTH_SESSION_INVALID".into()))
        );
    }

    #[test]
    fn handshake_lifetime_and_key_are_checked() {
        let now = 1_000_000;
        let response = |expires_at: u64, key: &str| Response {
            id: "h".into(),
            success: true,
            data: Some(
                serde_json::json!({"auth_token":"t","auth_key":key,"expires_at":expires_at}),
            ),
            error: None,
            signature: String::new(),
        };
        let good_key = hex::encode([1u8; 32]);
        assert!(validate_handshake(&response(now + 120, &good_key), "h", now).is_ok());
        assert!(validate_handshake(&response(now + 10, &good_key), "h", now).is_err());
        assert!(validate_handshake(&response(now + 3600, &good_key), "h", now).is_err());
        assert!(validate_handshake(&response(now + 120, "abcd"), "h", now).is_err());
        assert!(validate_handshake(&response(now + 120, &good_key), "x", now).is_err());
    }

    #[test]
    fn reason_codes_map_to_error_codes() {
        let failed = |text: &str| ServiceError::Failed(text.to_string());
        assert_eq!(
            failed("CONNECTION_OWNED_BY_ANOTHER_SESSION").info().code,
            ErrorCode::ServiceBusy
        );
        assert_eq!(
            failed("PROFILE_SCHEMA_UNSUPPORTED").info().code,
            ErrorCode::ServiceIncompatible
        );
        assert_eq!(
            failed("NODE_NOT_FOUND: node not found").info().code,
            ErrorCode::NodeNotFound
        );
        assert_eq!(
            failed("failed to spawn ppvpn-core").info().code,
            ErrorCode::ConnectFailed
        );
        assert_eq!(failed("failed to spawn ppvpn-core").reason_code(), None);
        for (detail, expected) in [
            (
                "PROFILE_SCHEMA_UNSUPPORTED: startup failed [start_stage=profile]",
                ErrorCode::ServiceIncompatible,
            ),
            (
                "PROFILE_EXPIRED: expired [start_stage=profile]",
                ErrorCode::ProfileExpired,
            ),
            (
                "PERMISSION_DENIED: denied [start_stage=start]",
                ErrorCode::ConnectFailed,
            ),
        ] {
            assert_eq!(failed(detail).info().code, expected, "{detail}");
        }
        assert_eq!(
            ServiceError::Unavailable("connect: No such file".into())
                .info()
                .code,
            ErrorCode::ServiceUnavailable
        );
        assert_eq!(
            ServiceError::Rejected("eof".into()).info().code,
            ErrorCode::ServiceClientRejected
        );
        assert_eq!(
            failed("PROFILE_EXPIRED: profile has expired").info().code,
            ErrorCode::ProfileExpired
        );

        let forwarded = forwarded_core_error(failed("LOCAL_PROXY_DISABLED: this core was started"));
        assert_eq!(forwarded.code(), Some("LOCAL_PROXY_DISABLED"));
        assert!(matches!(
            forwarded_core_error(ServiceError::Unavailable("x".into())),
            CoreCallError::Transport(_)
        ));
    }

    /// Records each write separately and serves a scripted response.
    struct FakePipe {
        writes: Vec<Vec<u8>>,
        response: std::io::Cursor<Vec<u8>>,
    }

    impl std::io::Read for FakePipe {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            // One byte at a time, like a byte-mode pipe returning partial data.
            let one = buf.len().min(1);
            std::io::Read::read(&mut self.response, &mut buf[..one])
        }
    }

    impl std::io::Write for FakePipe {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.writes.push(buf.to_vec());
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn pipe(response: Vec<u8>) -> FakePipe {
        FakePipe {
            writes: Vec::new(),
            response: std::io::Cursor::new(response),
        }
    }

    #[test]
    fn request_length_and_body_are_separate_writes() {
        let mut fake = pipe(vec![0, 0, 0, 2, b'{', b'}']);
        let answer = exchange(&mut fake, b"{\"a\":1}", false).unwrap();
        assert_eq!(answer, b"{}".to_vec());
        assert_eq!(fake.writes, vec![vec![0, 0, 0, 7], b"{\"a\":1}".to_vec()]);
    }

    #[test]
    fn only_a_silent_close_of_the_handshake_is_a_rejection() {
        assert!(matches!(
            exchange(&mut pipe(Vec::new()), b"{}", true),
            Err(ServiceError::Rejected(_))
        ));
        // The same close on a later request, or a cut-off answer, is a
        // protocol failure (ServiceIncompatible), not an identity problem.
        for (response, handshake) in [
            (Vec::new(), false),
            (vec![0, 0], true),
            (vec![0, 0, 0, 9, b'{'], true),
            (vec![0, 0, 0, 9, b'{'], false),
        ] {
            let error = exchange(&mut pipe(response.clone()), b"{}", handshake).unwrap_err();
            assert!(
                matches!(error, ServiceError::Protocol(_)),
                "{response:?} {handshake}: {error:?}"
            );
            assert_eq!(error.info().code, ErrorCode::ServiceIncompatible);
        }
    }

    // --- watch over a real socket ------------------------------------------

    /// What the fake service does with a `Watch`.
    #[cfg(unix)]
    #[derive(Clone)]
    enum WatchScript {
        /// A service that predates `Watch`: it cannot parse the request.
        Old,
        /// Answers with this `keepalive_ms`, writes these frames' `data` (or
        /// `None`: a frame with a bad signature), then closes, or keeps the
        /// connection open until the client closes it (`hold`).
        Events {
            keepalive_ms: u64,
            frames: Vec<Option<Value>>,
            hold: bool,
        },
        /// Answers the Watch with this signed error.
        Refuse(&'static str),
    }

    #[cfg(unix)]
    struct FakeWatchService {
        dir: std::path::PathBuf,
        socket: String,
        /// When the client closed a held watch connection.
        client_closed: Arc<Mutex<Option<Instant>>>,
    }

    #[cfg(unix)]
    impl Drop for FakeWatchService {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// A fake service on a Unix socket: handshakes, then runs `script`
    /// for every `Watch`.
    #[cfg(unix)]
    fn fake_watch_service(script: WatchScript) -> FakeWatchService {
        use std::io::{Read, Write};
        use std::os::unix::net::UnixListener;

        let dir = std::env::temp_dir().join(format!(
            "pw-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("s.sock").to_string_lossy().to_string();
        let listener = UnixListener::bind(&socket).unwrap();
        let key = vec![7u8; 32];
        let client_closed = Arc::new(Mutex::new(None));
        let closed = client_closed.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut length = [0u8; 4];
                if stream.read_exact(&mut length).is_err() {
                    continue;
                }
                let mut body = vec![0u8; u32::from_be_bytes(length) as usize];
                stream.read_exact(&mut body).unwrap();
                let request: Value = serde_json::from_slice(&body).unwrap();
                let id = request["id"].as_str().unwrap().to_string();
                let write = |stream: &mut std::os::unix::net::UnixStream, response: &Response| {
                    let body = serde_json::to_vec(response).unwrap();
                    stream.write_all(&(body.len() as u32).to_be_bytes())?;
                    stream.write_all(&body)
                };
                let signed = |data: Option<Value>, error: Option<&str>| {
                    sign_response(
                        Response {
                            id: id.clone(),
                            success: error.is_none(),
                            data,
                            error: error.map(str::to_string),
                            signature: String::new(),
                        },
                        &key,
                    )
                };
                match request["command"].as_str() {
                    Some("Handshake") => {
                        let answer = Response {
                            id: id.clone(),
                            success: true,
                            data: Some(serde_json::json!({
                                "auth_token": "token",
                                "auth_key": hex::encode(&key),
                                "expires_at": now_epoch() + 120,
                            })),
                            error: None,
                            signature: String::new(),
                        };
                        write(&mut stream, &answer).unwrap();
                    }
                    Some("Watch") => match script.clone() {
                        WatchScript::Old => drop(stream),
                        WatchScript::Refuse(error) => {
                            write(&mut stream, &signed(None, Some(error))).unwrap();
                        }
                        WatchScript::Events {
                            keepalive_ms,
                            frames,
                            hold,
                        } => {
                            let watching = serde_json::json!({
                                "event": "watching", "keepalive_ms": keepalive_ms,
                            });
                            write(&mut stream, &signed(Some(watching), None)).unwrap();
                            for data in frames {
                                let frame = match data {
                                    Some(data) => signed(Some(data), None),
                                    None => Response {
                                        signature: "00".repeat(32),
                                        ..signed(
                                            Some(serde_json::json!({"event": "stopping"})),
                                            None,
                                        )
                                    },
                                };
                                write(&mut stream, &frame).unwrap();
                                std::thread::sleep(Duration::from_millis(20));
                            }
                            if hold {
                                let mut rest = Vec::new();
                                let _ = stream.read_to_end(&mut rest);
                                *closed.lock().unwrap() = Some(Instant::now());
                            }
                        }
                    },
                    other => panic!("unexpected command {other:?}"),
                }
            }
        });
        FakeWatchService {
            dir,
            socket,
            client_closed,
        }
    }

    #[cfg(unix)]
    fn watched_session() -> SessionRef {
        SessionRef {
            session_id: "s".into(),
            generation: 1,
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_service_without_watch_is_unsupported() {
        let service = fake_watch_service(WatchScript::Old);
        let client = ServiceClient::new(service.socket.clone());
        assert_eq!(
            client.watch(&watched_session()).await,
            WatchEnd::Unsupported
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn watch_events_end_the_watch() {
        let keepalive = || Some(serde_json::json!({"event": "keepalive", "seq": 1}));
        let cases = [
            (
                vec![
                    keepalive(),
                    Some(serde_json::json!({"event": "stopping", "seq": 2})),
                ],
                WatchEnd::Stopping,
            ),
            (
                vec![
                    keepalive(),
                    // Unknown events from a newer service are skipped.
                    Some(serde_json::json!({"event": "something_new", "seq": 2})),
                    Some(
                        serde_json::json!({"event": "core_stopped", "reason": "exited", "seq": 3}),
                    ),
                ],
                WatchEnd::CoreStopped("exited".into()),
            ),
            (
                vec![None],
                WatchEnd::Abandoned("service response signature invalid".into()),
            ),
        ];
        for (frames, expected) in cases {
            let service = fake_watch_service(WatchScript::Events {
                keepalive_ms: 5_000,
                frames,
                hold: true,
            });
            let client = ServiceClient::new(service.socket.clone());
            assert_eq!(client.watch(&watched_session()).await, expected);
        }
        let service = fake_watch_service(WatchScript::Refuse("SERVICE_STOPPING"));
        let client = ServiceClient::new(service.socket.clone());
        assert_eq!(
            client.watch(&watched_session()).await,
            WatchEnd::Refused(ServiceError::Failed("SERVICE_STOPPING".into()))
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn eof_on_the_watch_is_noticed_within_a_poll() {
        let service = fake_watch_service(WatchScript::Events {
            keepalive_ms: 5_000,
            frames: vec![Some(serde_json::json!({"event": "keepalive", "seq": 1}))],
            hold: false,
        });
        let client = ServiceClient::new(service.socket.clone());
        let started = Instant::now();
        let end = client.watch(&watched_session()).await;
        assert!(matches!(end, WatchEnd::Lost(_)), "{end:?}");
        // The keepalive's 20 ms, then EOF within a poll interval.
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "{:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_silent_watch_is_abandoned() {
        let service = fake_watch_service(WatchScript::Events {
            keepalive_ms: 100,
            frames: vec![],
            hold: true,
        });
        let client = ServiceClient::new(service.socket.clone());
        let started = Instant::now();
        let end = client.watch(&watched_session()).await;
        assert!(matches!(end, WatchEnd::Abandoned(_)), "{end:?}");
        let took = started.elapsed();
        assert!(
            took >= watch_silence_limit(100) && took < watch_silence_limit(100) * 2,
            "{took:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dropping_the_watch_closes_its_connection() {
        let service = fake_watch_service(WatchScript::Events {
            keepalive_ms: 5_000,
            frames: vec![],
            hold: true,
        });
        let client = ServiceClient::new(service.socket.clone());
        let session = watched_session();
        let watching = client.watch(&session);
        assert!(tokio::time::timeout(Duration::from_millis(100), watching)
            .await
            .is_err());
        let dropped = Instant::now();
        loop {
            if let Some(closed) = *service.client_closed.lock().unwrap() {
                assert!(closed - dropped < Duration::from_millis(500));
                break;
            }
            assert!(dropped.elapsed() < Duration::from_secs(2), "still open");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[test]
    fn endpoint_constants_follow_the_service() {
        #[cfg(target_os = "macos")]
        assert_eq!(
            SERVICE_ENDPOINT,
            "/Library/Application Support/PPVPN/run/service.sock"
        );
        #[cfg(target_os = "linux")]
        assert_eq!(SERVICE_ENDPOINT, "/run/ppvpn/service.sock");
        #[cfg(windows)]
        assert_eq!(SERVICE_ENDPOINT, r"\\.\pipe\ppvpn-service");
    }
}
