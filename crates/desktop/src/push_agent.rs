//! Push agent: the core of the thin native helper that shows backend push
//! notifications while the main app is closed.
//!
//! It never signs in and never starts a proxy core. It reads what the main
//! app wrote to `<data_dir>/push-agent.json` (api base, device id, push
//! token), long-polls `GET /api/v1/push/pull`, hands each message to
//! [`PushAgentListener::on_push`] and acknowledges those that were shown.
//!
//! - Missing/unreadable file: `Idle`, re-checked every 10 s. A changed file
//!   (rotated token, new device) resets the in-memory state.
//! - 401/403: `Revoked` until the main app rewrites the file.
//! - Messages the OS could not show stay pending in memory, retried every
//!   5 min (or on [`PushAgent::retry_pending`]) and dropped 24 h after
//!   `created_at` (the server's TTL).
//! - The persisted cursor (`push-agent-cursor.json`, per device) is the lowest
//!   open (pending or unacknowledged) id − 1, else the highest acknowledged
//!   id; it is written only after a successful ack, so nothing that was not
//!   shown is ever skipped after a restart.
//! - `push-agent.heartbeat` is touched every 60 s while `run` is alive.
//!
//! Notification clicks: every push `on_push` accepted is recorded, with all
//! its [`PushMessage`] fields, in `<data_dir>/push-agent-shown.json` (at most
//! 50 entries, none older than 7 days, 0600 on Unix, written atomically; a
//! missing or corrupt file counts as empty). A native notification carries
//! only the push id (`PushMessage::id`, the push queue id, not an inbox
//! message id); the main app resolves a click with
//! [`crate::Client::shown_push`], which reads that file, and opens
//! `deep_link`, the inbox message `message_id`, or the push itself. Links
//! from anywhere else are never opened. Format: `push_shown.rs`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{watch, Notify};

use crate::device::{read_agent_file, AgentFile, AGENT_CURSOR_FILE};
use crate::notifications::{absolute_link, category, severity};
use crate::{MessageCategory, MessageSeverity};

const PATH_PUSH_PULL: &str = "/api/v1/push/pull";
const PATH_PUSH_ACK: &str = "/api/v1/push/ack";
const HEARTBEAT_FILE: &str = "push-agent.heartbeat";
/// Long-poll wait requested from the server (its maximum).
const PULL_WAIT_SECS: u64 = 25;
/// HTTP timeout: comfortably above the long-poll wait.
const HTTP_TIMEOUT: Duration = Duration::from_secs(40);
const ACK_BATCH: usize = 100;
/// Pending messages expire with the server-side TTL.
const PENDING_TTL_SECS: u64 = 24 * 60 * 60;

const IDLE_RECHECK: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(10)
};
const PENDING_RETRY: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(5 * 60)
};
const HEARTBEAT: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(60)
};
const RATE_LIMITED_WAIT: Duration = Duration::from_secs(60);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

#[derive(uniffi::Record, Clone, Debug)]
pub struct PushAgentConfig {
    /// The main app's `ClientConfig::data_dir` (holds `push-agent.json`).
    pub data_dir: String,
    /// Where `ppvpn-push-agent.YYYY-MM-DD.log` goes.
    pub log_dir: String,
    /// `macos` | `windows` | `linux`.
    pub platform: String,
    pub app_version: String,
}

/// A push to show as an OS notification.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushMessage {
    /// Push queue id: what the notification carries (launch arguments).
    pub id: u64,
    /// The inbox message this push is about; `None` when there is none
    /// (e.g. a broadcast).
    #[serde(default)]
    pub message_id: Option<u64>,
    pub title: String,
    pub body: String,
    pub severity: MessageSeverity,
    pub category: MessageCategory,
    pub event_key: String,
    /// Absolute http(s) URL (relative links resolved against the site root).
    pub deep_link: Option<String>,
    /// RFC 3339.
    pub created_at: String,
}

#[derive(uniffi::Enum, Clone, Debug, PartialEq, Eq)]
pub enum PushAgentState {
    /// Nothing to pull for (not signed in / not registered yet).
    Idle { reason: String },
    /// Pulling.
    Running,
    /// The push token was rejected; waiting for the main app to register
    /// again.
    Revoked,
}

/// Implemented by the native agent. Called from background threads.
#[uniffi::export(with_foreign)]
pub trait PushAgentListener: Send + Sync {
    /// Show `message`; `true` when it was handed to the OS notification
    /// system, `false` when it cannot be shown right now (retried later).
    fn on_push(&self, message: PushMessage) -> bool;
    fn on_state(&self, state: PushAgentState);
}

/// The push agent. `run` blocks until `stop`.
#[derive(uniffi::Object)]
pub struct PushAgent {
    config: PushAgentConfig,
    listener: Arc<dyn PushAgentListener>,
    runtime: tokio::runtime::Runtime,
    stop: watch::Sender<bool>,
    retry: Arc<Notify>,
}

#[uniffi::export]
impl PushAgent {
    #[uniffi::constructor]
    pub fn new(config: PushAgentConfig, listener: Arc<dyn PushAgentListener>) -> Arc<Self> {
        crate::logging::install_with_prefix(
            &config.log_dir,
            crate::logging::PUSH_AGENT_PREFIX,
            &config.app_version,
        );
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("ppvpn-push-agent")
            .build()
            .expect("tokio runtime");
        let (stop, _) = watch::channel(false);
        Arc::new(Self {
            config,
            listener,
            runtime,
            stop,
            retry: Arc::new(Notify::new()),
        })
    }

    /// Pulls and shows pushes until [`Self::stop`]. Blocks the calling thread
    /// (not a runtime thread). One run per agent: stop is latched even when
    /// called before the worker enters run; create a new agent to restart.
    pub fn run(&self) {
        tracing::info!(
            platform = %self.config.platform,
            version = %self.config.app_version,
            "push agent: run"
        );
        let engine = Engine::new(
            &self.config.data_dir,
            self.listener.clone(),
            self.stop.subscribe(),
            self.retry.clone(),
        );
        self.runtime.block_on(engine.run());
        tracing::info!("push agent: stopped");
    }

    /// Makes `run` return promptly, aborting an in-flight long poll.
    /// Thread-safe.
    pub fn stop(&self) {
        self.stop.send_replace(true);
    }

    /// Retries messages that could not be shown, now.
    pub fn retry_pending(&self) {
        self.retry.notify_one();
    }
}

// ---------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------

#[derive(Deserialize, Debug, Clone)]
struct PushItem {
    id: u64,
    #[serde(default)]
    message_id: Option<u64>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    deep_link: String,
    #[serde(default)]
    severity: String,
    #[serde(default)]
    event_key: String,
    #[serde(default)]
    created_at: String,
}

#[derive(Deserialize, Debug)]
struct PullResponse {
    #[serde(default)]
    items: Vec<PushItem>,
    #[serde(default)]
    cursor: u64,
}

fn to_push_message(item: &PushItem, api_base: &str) -> PushMessage {
    // Pushes carry no message type; the event key's first segment
    // (`invoice.issued` → `invoice`) stands in for it.
    let kind = item.event_key.split('.').next().unwrap_or_default();
    PushMessage {
        id: item.id,
        message_id: item.message_id,
        title: item.title.clone(),
        body: item.body.clone(),
        severity: severity(&item.severity),
        category: category(kind, &item.event_key),
        event_key: item.event_key.clone(),
        deep_link: absolute_link(api_base, &item.deep_link),
        created_at: item.created_at.clone(),
    }
}

/// Seconds since the epoch of an RFC 3339 timestamp (`YYYY-MM-DDTHH:MM:SS`
/// with optional fraction and `Z` / `±HH:MM`).
fn parse_rfc3339(value: &str) -> Option<u64> {
    let value = value.trim();
    let (date, rest) = value.split_once(['T', 't', ' '])?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    let time_end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == ':'))
        .unwrap_or(rest.len());
    let mut time = rest[..time_end].split(':');
    let hour: i64 = time.next()?.parse().ok()?;
    let minute: i64 = time.next()?.parse().ok()?;
    let second: i64 = time.next()?.parse().ok()?;
    let mut zone = &rest[time_end..];
    if let Some(fraction) = zone.strip_prefix('.') {
        let digits = fraction
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(fraction.len());
        zone = &fraction[digits..];
    }
    let offset = match zone {
        "Z" | "z" | "" => 0,
        _ => {
            let sign = if zone.starts_with('-') { -1 } else { 1 };
            let (h, m) = zone.get(1..)?.split_once(':')?;
            sign * (h.parse::<i64>().ok()? * 3600 + m.parse::<i64>().ok()? * 60)
        }
    };
    let days = crate::logging::days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second - offset;
    u64::try_from(seconds).ok()
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// ---------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
struct CursorFile {
    device_id: u64,
    cursor: u64,
}

fn load_cursor(data_dir: &Path, device_id: u64) -> u64 {
    std::fs::read(data_dir.join(AGENT_CURSOR_FILE))
        .ok()
        .and_then(|raw| serde_json::from_slice::<CursorFile>(&raw).ok())
        .filter(|file| file.device_id == device_id)
        .map_or(0, |file| file.cursor)
}

/// The cursor that is safe to resume from: nothing at or below it is still
/// waiting to be shown or acknowledged.
fn safe_cursor(open: impl IntoIterator<Item = u64>, acked_max: u64, start: u64) -> u64 {
    match open.into_iter().min() {
        Some(lowest) => lowest.saturating_sub(1),
        None => acked_max.max(start),
    }
}

// ---------------------------------------------------------------------------
// Engine
// ---------------------------------------------------------------------------

struct Pending {
    message: PushMessage,
    /// When the server-side TTL ends (from `created_at`, else first seen).
    expires_at: u64,
}

/// In-memory state for one agent file (device + token).
struct Session {
    file: AgentFile,
    /// Where the next pull starts: past everything received.
    cursor: u64,
    /// Cursor loaded at start (persisted value).
    start_cursor: u64,
    acked_max: u64,
    /// Shown, not yet acknowledged.
    unacked: BTreeSet<u64>,
    /// Not shown yet.
    pending: Vec<Pending>,
    next_retry: tokio::time::Instant,
    revoked: bool,
}

enum PullError {
    Unauthorized,
    RateLimited,
    Unprocessable,
    Other(String),
}

struct Engine {
    data_dir: PathBuf,
    listener: Arc<dyn PushAgentListener>,
    stop: watch::Receiver<bool>,
    retry: Arc<Notify>,
    http: reqwest::Client,
    state: Option<PushAgentState>,
    session: Option<Session>,
    backoff: Duration,
}

impl Engine {
    fn new(
        data_dir: &str,
        listener: Arc<dyn PushAgentListener>,
        stop: watch::Receiver<bool>,
        retry: Arc<Notify>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_default();
        Self {
            data_dir: PathBuf::from(data_dir),
            listener,
            stop,
            retry,
            http,
            state: None,
            session: None,
            backoff: BACKOFF_MIN,
        }
    }

    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    fn set_state(&mut self, state: PushAgentState) {
        if self.state.as_ref() != Some(&state) {
            tracing::info!("push agent: {state:?}");
            self.state = Some(state.clone());
            self.listener.on_state(state);
        }
    }

    /// Sleeps `duration`, waking early for `retry_pending` (returns false) or
    /// `stop` (returns true).
    async fn wait(&mut self, duration: Duration) -> bool {
        let mut stop = self.stop.clone();
        let retry = self.retry.clone();
        let woken = tokio::select! {
            _ = tokio::time::sleep(duration) => false,
            _ = stop.wait_for(|stopped| *stopped) => return true,
            _ = retry.notified() => true,
        };
        if woken {
            self.force_retry();
        }
        self.stopped()
    }

    fn force_retry(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.next_retry = tokio::time::Instant::now();
        }
    }

    async fn run(mut self) {
        let heartbeat = tokio::spawn(heartbeat(self.data_dir.join(HEARTBEAT_FILE)));
        while !self.stopped() {
            // Cancellation covers ACKs/response bodies as well as long polls;
            // the VPN service must never wait for a network timeout to stop.
            let mut stop = self.stop.clone();
            tokio::select! {
                _ = stop.wait_for(|stopped| *stopped) => break,
                done = self.tick() => if done { break; },
            }
        }
        heartbeat.abort();
    }

    /// One round; `true` when stopped.
    async fn tick(&mut self) -> bool {
        let data_dir = self.data_dir.to_string_lossy().into_owned();
        let Some(file) = read_agent_file(&data_dir) else {
            self.session = None;
            self.set_state(PushAgentState::Idle {
                reason: "not registered (no push-agent.json)".to_string(),
            });
            return self.wait(IDLE_RECHECK).await;
        };
        if self.session.as_ref().map(|session| &session.file) != Some(&file) {
            let start = load_cursor(&self.data_dir, file.device_id);
            tracing::info!("push agent: device {} (cursor {start})", file.device_id);
            self.session = Some(Session {
                file,
                cursor: start,
                start_cursor: start,
                acked_max: 0,
                unacked: BTreeSet::new(),
                pending: Vec::new(),
                next_retry: tokio::time::Instant::now() + PENDING_RETRY,
                revoked: false,
            });
            self.backoff = BACKOFF_MIN;
        }
        if self.session.as_ref().is_some_and(|session| session.revoked) {
            self.set_state(PushAgentState::Revoked);
            return self.wait(IDLE_RECHECK).await;
        }
        self.set_state(PushAgentState::Running);

        self.retry_due().await;
        self.flush_acks().await;

        let mut stop = self.stop.clone();
        let retry = self.retry.clone();
        let pulled = tokio::select! {
            result = self.pull() => Some(result),
            _ = stop.wait_for(|stopped| *stopped) => return true,
            _ = retry.notified() => None,
        };
        let Some(pulled) = pulled else {
            // retry_pending(): abandon this poll and retry now.
            self.force_retry();
            return false;
        };
        match pulled {
            Ok(response) => {
                self.backoff = BACKOFF_MIN;
                self.deliver(response).await;
                self.flush_acks().await;
                false
            }
            Err(PullError::Unauthorized) => {
                tracing::info!(
                    "push agent: push token rejected (401/403); waiting for re-registration"
                );
                if let Some(session) = self.session.as_mut() {
                    session.revoked = true;
                }
                self.set_state(PushAgentState::Revoked);
                false
            }
            Err(PullError::RateLimited) => {
                tracing::info!("push agent: rate limited (429)");
                self.wait(RATE_LIMITED_WAIT).await
            }
            Err(PullError::Unprocessable) => {
                tracing::error!("push agent: pull rejected as invalid (422) - a client bug");
                let backoff = self.next_backoff();
                self.wait(backoff).await
            }
            Err(PullError::Other(detail)) => {
                tracing::info!("push agent: pull failed: {detail}");
                let backoff = self.next_backoff();
                self.wait(backoff).await
            }
        }
    }

    fn next_backoff(&mut self) -> Duration {
        let current = self.backoff;
        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
        current
    }

    fn endpoint(&self, path: &str) -> Result<reqwest::Url, PullError> {
        let session = self.session.as_ref().ok_or(PullError::Unauthorized)?;
        crate::api::client_api(&session.file.api_base)
            .endpoint(path)
            .map_err(|_| PullError::Other("api base unusable".to_string()))
    }

    async fn pull(&self) -> Result<PullResponse, PullError> {
        let session = self.session.as_ref().ok_or(PullError::Unauthorized)?;
        let mut url = self.endpoint(PATH_PUSH_PULL)?;
        url.query_pairs_mut()
            .append_pair("after", &session.cursor.to_string())
            .append_pair("wait", &PULL_WAIT_SECS.to_string());
        let response = self
            .http
            .get(url)
            .header("X-Push-Token", &session.file.push_token)
            .send()
            .await
            .map_err(|error| PullError::Other(error.without_url().to_string()))?;
        match response.status().as_u16() {
            200 => response
                .json::<PullResponse>()
                .await
                .map_err(|error| PullError::Other(format!("decode pull: {error}"))),
            401 | 403 => Err(PullError::Unauthorized),
            429 => Err(PullError::RateLimited),
            422 => Err(PullError::Unprocessable),
            status => Err(PullError::Other(format!("HTTP {status}"))),
        }
    }

    /// Offers one message to the OS; `true` when shown. A shown message is
    /// recorded for notification clicks (best effort).
    async fn offer(&self, message: PushMessage) -> bool {
        let listener = self.listener.clone();
        let data_dir = self.data_dir.clone();
        tokio::task::spawn_blocking(move || {
            let shown = listener.on_push(message.clone());
            if shown {
                if let Err(error) = crate::push_shown::record(&data_dir, &message) {
                    tracing::warn!("push agent: record shown {}: {error}", message.id);
                }
            }
            shown
        })
        .await
        .unwrap_or(false)
    }

    async fn deliver(&mut self, response: PullResponse) {
        let api_base = match self.session.as_ref() {
            Some(session) => session.file.api_base.clone(),
            None => return,
        };
        for item in response.items {
            let message = to_push_message(&item, &api_base);
            let already = self.session.as_ref().is_some_and(|session| {
                session.unacked.contains(&item.id)
                    || session.pending.iter().any(|p| p.message.id == item.id)
            });
            if already {
                continue;
            }
            let shown = self.offer(message.clone()).await;
            let Some(session) = self.session.as_mut() else {
                return;
            };
            session.cursor = session.cursor.max(item.id);
            if shown {
                session.unacked.insert(item.id);
            } else {
                tracing::info!("push agent: message {} not shown now; pending", item.id);
                let expires_at = parse_rfc3339(&item.created_at)
                    .unwrap_or_else(now_secs)
                    .saturating_add(PENDING_TTL_SECS);
                session.pending.push(Pending {
                    message,
                    expires_at,
                });
            }
        }
        if let Some(session) = self.session.as_mut() {
            session.cursor = session.cursor.max(response.cursor);
        }
    }

    /// Offers pending messages again when due; drops expired ones.
    async fn retry_due(&mut self) {
        let due = self.session.as_ref().is_some_and(|session| {
            !session.pending.is_empty() && tokio::time::Instant::now() >= session.next_retry
        });
        if !due {
            return;
        }
        let pending = match self.session.as_mut() {
            Some(session) => {
                session.next_retry = tokio::time::Instant::now() + PENDING_RETRY;
                std::mem::take(&mut session.pending)
            }
            None => return,
        };
        let now = now_secs();
        let mut still = Vec::new();
        let mut shown_ids = Vec::new();
        for entry in pending {
            if now >= entry.expires_at {
                tracing::info!("push agent: message {} expired unshown", entry.message.id);
                continue;
            }
            if self.offer(entry.message.clone()).await {
                shown_ids.push(entry.message.id);
            } else {
                still.push(entry);
            }
        }
        if let Some(session) = self.session.as_mut() {
            session.unacked.extend(shown_ids);
            session.pending = still;
        }
        // Dropped messages no longer hold the cursor back.
        self.persist_cursor().await;
    }

    /// Acknowledges shown messages in batches; the cursor is persisted after
    /// each successful batch.
    async fn flush_acks(&mut self) {
        loop {
            let (ids, token) = match self.session.as_ref() {
                Some(session) if !session.unacked.is_empty() => (
                    session
                        .unacked
                        .iter()
                        .take(ACK_BATCH)
                        .copied()
                        .collect::<Vec<_>>(),
                    session.file.push_token.clone(),
                ),
                _ => return,
            };
            let Ok(url) = self.endpoint(PATH_PUSH_ACK) else {
                return;
            };
            let sent = self
                .http
                .post(url)
                .header("X-Push-Token", token)
                .json(&serde_json::json!({ "ids": ids }))
                .send()
                .await;
            match sent {
                Ok(response) if response.status().is_success() => {
                    let _ = response.json::<Value>().await;
                    if let Some(session) = self.session.as_mut() {
                        for id in &ids {
                            session.unacked.remove(id);
                        }
                        session.acked_max = ids.iter().copied().fold(session.acked_max, u64::max);
                    }
                    self.persist_cursor().await;
                }
                Ok(response) if matches!(response.status().as_u16(), 401 | 403) => {
                    if let Some(session) = self.session.as_mut() {
                        session.revoked = true;
                    }
                    return;
                }
                Ok(response) => {
                    tracing::info!(
                        "push agent: ack failed: HTTP {}",
                        response.status().as_u16()
                    );
                    return;
                }
                Err(error) => {
                    tracing::info!("push agent: ack failed: {}", error.without_url());
                    return;
                }
            }
        }
    }

    async fn persist_cursor(&self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let open = session
            .pending
            .iter()
            .map(|pending| pending.message.id)
            .chain(session.unacked.iter().copied());
        let cursor = safe_cursor(open, session.acked_max, session.start_cursor);
        let file = CursorFile {
            device_id: session.file.device_id,
            cursor,
        };
        let path = self.data_dir.join(AGENT_CURSOR_FILE);
        let raw = serde_json::to_vec(&file).unwrap_or_default();
        let written =
            tokio::task::spawn_blocking(move || crate::device::write_private(&path, &raw)).await;
        if !matches!(written, Ok(Ok(()))) {
            tracing::warn!("push agent: persist cursor failed");
        }
    }
}

async fn heartbeat(path: PathBuf) {
    loop {
        let stamp = now_secs().to_string();
        let target = path.clone();
        let _ = tokio::task::spawn_blocking(move || std::fs::write(target, stamp)).await;
        tokio::time::sleep(HEARTBEAT).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_backend::{serve, temp_dir, wait_until, Backend, PUSH_TOKEN};
    use std::collections::HashSet;
    use std::sync::Mutex;
    use std::time::Instant;

    #[derive(Default)]
    struct Recorder {
        shown: Mutex<Vec<PushMessage>>,
        offered: Mutex<Vec<u64>>,
        refuse: Mutex<HashSet<u64>>,
        states: Mutex<Vec<PushAgentState>>,
    }

    impl PushAgentListener for Recorder {
        fn on_push(&self, message: PushMessage) -> bool {
            self.offered.lock().unwrap().push(message.id);
            if self.refuse.lock().unwrap().contains(&message.id) {
                return false;
            }
            self.shown.lock().unwrap().push(message);
            true
        }
        fn on_state(&self, state: PushAgentState) {
            self.states.lock().unwrap().push(state);
        }
    }

    impl Recorder {
        fn shown_ids(&self) -> Vec<u64> {
            self.shown.lock().unwrap().iter().map(|m| m.id).collect()
        }
        fn last_state(&self) -> Option<PushAgentState> {
            self.states.lock().unwrap().last().cloned()
        }
    }

    fn write_file(dir: &str, base: &str, token: &str) {
        let file = AgentFile {
            api_base: base.to_string(),
            device_id: 42,
            push_token: token.to_string(),
            installation_id: "00000000-0000-4000-8000-000000000000".to_string(),
        };
        crate::device::write_private(
            &crate::device::agent_file_path(dir),
            &serde_json::to_vec(&file).unwrap(),
        )
        .unwrap();
    }

    fn cursor(dir: &str) -> Option<u64> {
        std::fs::read(Path::new(dir).join(AGENT_CURSOR_FILE))
            .ok()
            .and_then(|raw| serde_json::from_slice::<CursorFile>(&raw).ok())
            .map(|file| file.cursor)
    }

    struct Running {
        agent: Arc<PushAgent>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Running {
        fn stop(mut self) -> Duration {
            let started = Instant::now();
            self.agent.stop();
            self.thread.take().unwrap().join().unwrap();
            started.elapsed()
        }
    }

    fn start(dir: &str, recorder: Arc<Recorder>) -> Running {
        let agent = PushAgent::new(
            PushAgentConfig {
                data_dir: dir.to_string(),
                log_dir: dir.to_string(),
                platform: "macos".into(),
                app_version: "0.0.0-test".into(),
            },
            recorder,
        );
        let runner = agent.clone();
        let thread = std::thread::spawn(move || runner.run());
        Running {
            agent,
            thread: Some(thread),
        }
    }

    fn acked(backend: &Backend) -> Vec<u64> {
        let mut ids = backend.acked.lock().unwrap().clone();
        ids.sort_unstable();
        ids
    }

    #[test]
    fn pulled_messages_are_shown_and_acked() {
        let backend = Backend::new();
        backend.add_push(1, "2099-01-01T00:00:00Z");
        backend.add_push(2, "2099-01-01T00:00:00Z");
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-agent-test");
        write_file(&dir, &base, PUSH_TOKEN);
        let recorder = Arc::new(Recorder::default());
        let running = start(&dir, recorder.clone());

        wait_until(|| acked(&backend) == vec![1, 2]);
        assert_eq!(recorder.shown_ids(), vec![1, 2]);
        wait_until(|| cursor(&dir) == Some(2));
        let message = recorder.shown.lock().unwrap()[0].clone();
        assert_eq!(message.category, MessageCategory::Billing);
        assert_eq!(message.severity, MessageSeverity::Critical);
        assert_eq!(
            message.deep_link.as_deref(),
            Some(format!("{base}/app/subscriptions/88").as_str())
        );
        assert_eq!(recorder.last_state(), Some(PushAgentState::Running));
        assert!(Path::new(&dir).join(HEARTBEAT_FILE).exists());
        let pulls = backend.requests_matching("/push/pull?after=");
        assert!(pulls.iter().all(|line| line.contains("wait=25")));
        running.stop();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unshown_messages_stay_pending_and_hold_the_cursor() {
        let backend = Backend::new();
        backend.add_push(1, "2099-01-01T00:00:00Z");
        backend.add_push(2, "2099-01-01T00:00:00Z");
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-agent-test");
        write_file(&dir, &base, PUSH_TOKEN);
        let recorder = Arc::new(Recorder::default());
        recorder.refuse.lock().unwrap().insert(1);
        let running = start(&dir, recorder.clone());

        wait_until(|| acked(&backend) == vec![2]);
        // 1 is pending: the persisted cursor stays below it.
        wait_until(|| cursor(&dir) == Some(0));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(recorder.shown_ids(), vec![2]);

        // The OS can show it now: retried (and acked) on request.
        recorder.refuse.lock().unwrap().clear();
        running.agent.retry_pending();
        wait_until(|| acked(&backend) == vec![1, 2]);
        wait_until(|| cursor(&dir) == Some(2));
        assert_eq!(recorder.shown_ids(), vec![2, 1]);
        running.stop();
        let _ = std::fs::remove_dir_all(dir);
    }

    fn recorded(dir: &str) -> Vec<u64> {
        let raw = std::fs::read(crate::push_shown::shown_path(Path::new(dir))).unwrap_or_default();
        serde_json::from_slice::<Value>(&raw)
            .ok()
            .and_then(|file| {
                file["items"]
                    .as_array()
                    .map(|items| items.iter().filter_map(|i| i["id"].as_u64()).collect())
            })
            .unwrap_or_default()
    }

    #[test]
    fn only_shown_messages_are_recorded_for_clicks() {
        let backend = Backend::new();
        backend.add_push(1, "2099-01-01T00:00:00Z");
        backend.add_push_for_message(2, "2099-01-01T00:00:00Z", 4711);
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-agent-test");
        write_file(&dir, &base, PUSH_TOKEN);
        let recorder = Arc::new(Recorder::default());
        recorder.refuse.lock().unwrap().insert(1);
        let running = start(&dir, recorder.clone());

        wait_until(|| acked(&backend) == vec![2]);
        assert_eq!(recorder.offered.lock().unwrap().first(), Some(&1));
        assert_eq!(recorded(&dir), vec![2], "the refused push is not recorded");
        let shown = crate::push_shown::find(Path::new(&dir), 2).unwrap();
        assert_eq!(shown, recorder.shown.lock().unwrap()[0]);
        assert_eq!(shown.message_id, Some(4711));
        assert_eq!(
            shown.deep_link.as_deref(),
            Some(format!("{base}/app/subscriptions/88").as_str())
        );
        assert_eq!(crate::push_shown::find(Path::new(&dir), 1), None);

        // Shown on retry: recorded then.
        recorder.refuse.lock().unwrap().clear();
        running.agent.retry_pending();
        wait_until(|| acked(&backend) == vec![1, 2]);
        assert_eq!(recorded(&dir), vec![2, 1]);
        assert_eq!(
            crate::push_shown::find(Path::new(&dir), 1)
                .unwrap()
                .message_id,
            None
        );
        running.stop();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_restart_resumes_below_an_unshown_message() {
        let backend = Backend::new();
        backend.add_push(1, "2099-01-01T00:00:00Z");
        backend.add_push(2, "2099-01-01T00:00:00Z");
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-agent-test");
        write_file(&dir, &base, PUSH_TOKEN);

        let first = Arc::new(Recorder::default());
        first.refuse.lock().unwrap().insert(1);
        let running = start(&dir, first.clone());
        wait_until(|| acked(&backend) == vec![2] && cursor(&dir) == Some(0));
        running.stop();

        // A new agent resumes from 0 and gets 1 again (never skipped).
        let second = Arc::new(Recorder::default());
        let running = start(&dir, second.clone());
        wait_until(|| second.shown_ids() == vec![1]);
        wait_until(|| acked(&backend) == vec![1, 2]);
        assert!(backend.requests_matching("/push/pull?after=0&").len() >= 2);
        running.stop();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_rejected_token_waits_for_the_file_to_change() {
        rejected_token_waits_for_the_file_to_change("ppd_revoked"); // 401
    }

    #[test]
    fn a_forbidden_token_is_revoked_too() {
        rejected_token_waits_for_the_file_to_change("ppd_forbidden"); // 403
    }

    fn rejected_token_waits_for_the_file_to_change(rejected: &str) {
        let backend = Backend::new();
        backend.add_push(7, "2099-01-01T00:00:00Z");
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-agent-test");
        write_file(&dir, &base, rejected);
        let recorder = Arc::new(Recorder::default());
        let running = start(&dir, recorder.clone());

        wait_until(|| recorder.last_state() == Some(PushAgentState::Revoked));
        let pulls = backend.requests_matching("/push/pull").len();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            backend.requests_matching("/push/pull").len(),
            pulls,
            "no pulls while revoked"
        );

        // The main app registered again and rewrote the file.
        write_file(&dir, &base, PUSH_TOKEN);
        wait_until(|| recorder.shown_ids() == vec![7]);
        assert_eq!(recorder.last_state(), Some(PushAgentState::Running));
        running.stop();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn no_file_is_idle_until_registration() {
        let backend = Backend::new();
        backend.add_push(3, "2099-01-01T00:00:00Z");
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-agent-test");
        std::fs::create_dir_all(&dir).unwrap();
        let recorder = Arc::new(Recorder::default());
        let running = start(&dir, recorder.clone());

        wait_until(|| matches!(recorder.last_state(), Some(PushAgentState::Idle { .. })));
        assert!(backend.requests_matching("/push/").is_empty());
        // The heartbeat runs while idle too.
        wait_until(|| Path::new(&dir).join(HEARTBEAT_FILE).exists());

        write_file(&dir, &base, PUSH_TOKEN);
        wait_until(|| recorder.shown_ids() == vec![3]);
        running.stop();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stop_interrupts_a_long_poll() {
        let backend = Backend::new();
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-agent-test");
        write_file(&dir, &base, PUSH_TOKEN);
        let recorder = Arc::new(Recorder::default());
        let running = start(&dir, recorder.clone());
        wait_until(|| !backend.requests_matching("/push/pull").is_empty());
        std::thread::sleep(Duration::from_millis(100));
        let took = running.stop();
        assert!(took < Duration::from_secs(1), "stop took {took:?}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stop_interrupts_a_hanging_ack() {
        let backend = Backend::new();
        backend.add_push(1, "2099-01-01T00:00:00Z");
        *backend.ack_delay.lock().unwrap() = Some(Duration::from_secs(5));
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-agent-test");
        write_file(&dir, &base, PUSH_TOKEN);
        let running = start(&dir, Arc::new(Recorder::default()));
        wait_until(|| !backend.requests_matching("/push/ack").is_empty());
        let took = running.stop();
        assert!(
            took < Duration::from_secs(1),
            "stop waited for the ACK: {took:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stop_before_the_worker_enters_run_is_not_lost() {
        let dir = temp_dir("ppvpn-agent-test");
        let agent = PushAgent::new(
            PushAgentConfig {
                data_dir: dir.clone(),
                log_dir: dir.clone(),
                platform: "android".into(),
                app_version: "test".into(),
            },
            Arc::new(Recorder::default()),
        );
        agent.stop();
        let (sent, received) = std::sync::mpsc::channel();
        let worker = agent.clone();
        let thread = std::thread::spawn(move || {
            worker.run();
            let _ = sent.send(());
        });
        let finished = received.recv_timeout(Duration::from_secs(2)).is_ok();
        // Also cleanly stop the old implementation when used as a regression test.
        agent.stop();
        thread.join().unwrap();
        assert!(finished, "a stop before run must remain latched");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn messages_map_and_links_resolve() {
        let item = PushItem {
            id: 9,
            message_id: Some(4711),
            title: "t".into(),
            body: "b".into(),
            deep_link: "/app/orders/1".into(),
            severity: "important".into(),
            event_key: "order.paid".into(),
            created_at: "2026-09-29T12:00:00Z".into(),
        };
        let message = to_push_message(&item, "https://www.example.com/api/");
        assert_eq!(
            message.deep_link.as_deref(),
            Some("https://www.example.com/app/orders/1")
        );
        assert_eq!(message.category, MessageCategory::Order);
        assert_eq!(message.severity, MessageSeverity::Important);
        assert_eq!(message.message_id, Some(4711));
        let link = |deep_link: &str| {
            to_push_message(
                &PushItem {
                    deep_link: deep_link.into(),
                    ..item.clone()
                },
                "https://www.example.com",
            )
            .deep_link
        };
        assert_eq!(link(""), None);
        assert_eq!(link("javascript:alert(1)"), None);
        assert_eq!(
            link("https://example.com/x").as_deref(),
            Some("https://example.com/x")
        );
        let broadcast = to_push_message(
            &PushItem {
                event_key: "broadcast:12".into(),
                ..item.clone()
            },
            "https://x",
        );
        assert_eq!(broadcast.category, MessageCategory::Announcement);
    }

    #[test]
    fn cursor_rule_and_timestamps() {
        assert_eq!(safe_cursor([5, 3, 9], 20, 0), 2);
        assert_eq!(safe_cursor(Vec::<u64>::new(), 20, 4), 20);
        assert_eq!(safe_cursor(Vec::<u64>::new(), 0, 4), 4);
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-09-29T00:00:00Z"), Some(1_790_640_000));
        assert_eq!(
            parse_rfc3339("2026-09-29T08:00:00.123+08:00"),
            Some(1_790_640_000)
        );
        assert_eq!(parse_rfc3339("nonsense"), None);
    }
}
