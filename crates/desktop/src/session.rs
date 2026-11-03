//! The signed-in session: restore at launch, device login, access-token
//! refresh, account/team/profile loading, node selection and sign-out.
//!
//! Everything here is `impl Client`; [`crate::auth`] owns the credential and
//! the backend device flow, [`crate::api`] the HTTP calls. Background work runs
//! on the client's runtime and holds the client only through [`ClientRef`].

use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, MutexGuard, Weak};
use std::time::Duration;

use ppvpn_account::session::{with_bearer, RetrySchedule, ACCESS_TOKEN_MARGIN};
use serde::{Deserialize, Serialize};

use crate::api::{self, ApiError, TeamEntry};
use crate::api::{ApiErrorExt, ToTeam};
use crate::auth::{AccessToken, AuthError, SignedIn};
use crate::auth::{AuthErrorExt, StoreErrorExt};
use crate::errors::{ClientError, ClientErrorInfo, ErrorCode};
use crate::saved_profile::SavedProfile;
use crate::{
    AuthState, Client, ClientSnapshot, DeviceCode, Node, ProfileStatus, StandardState, Team,
};

/// Profile refresh period while signed in.
const PROFILE_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
/// Retry period after a failed background refresh, e.g. offline at launch.
const RETRY_INTERVAL: Duration = Duration::from_secs(30);
/// Re-fetch delays while the profile status is `NodesNotReady`: 30 s, 60 s,
/// then the last one over and over; much shorter in tests.
const NODES_NOT_READY_RETRY: [Duration; 3] = if cfg!(test) {
    [
        Duration::from_millis(20),
        Duration::from_millis(40),
        Duration::from_millis(60),
    ]
} else {
    [
        Duration::from_secs(30),
        Duration::from_secs(60),
        Duration::from_secs(2 * 60),
    ]
};

/// The delay before the `attempt`-th (from 0) re-fetch in a row while the
/// nodes are not ready.
fn nodes_not_ready_delay(attempt: usize) -> Duration {
    NODES_NOT_READY_RETRY[attempt.min(NODES_NOT_READY_RETRY.len() - 1)]
}

/// Credential-store retry after a failed read at launch
/// ([`RetrySchedule::CREDENTIAL_STORE`]); much shorter in tests.
const CREDENTIAL_RETRY: RetrySchedule = if cfg!(test) {
    RetrySchedule {
        fast: Duration::from_millis(20),
        slow: Duration::from_millis(50),
        fast_window: RetrySchedule::CREDENTIAL_STORE.fast_window,
    }
} else {
    RetrySchedule::CREDENTIAL_STORE
};

/// The background re-read of a credential store that failed at launch
/// ([`Client::spawn_credential_retry`]), and the wake-up for an immediate
/// attempt (`Client::retry_credential_restore`).
#[derive(Default)]
pub(crate) struct CredentialRetry {
    /// Set while the retry loop runs.
    active: Arc<AtomicBool>,
    wake: Arc<tokio::sync::Notify>,
}

/// Clears [`CredentialRetry::active`] when the retry loop ends, however it ends.
struct RetryActive(Arc<AtomicBool>);

impl Drop for RetryActive {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

fn is_store_error(code: Option<ErrorCode>) -> bool {
    matches!(
        code,
        Some(ErrorCode::CredentialStoreFailed | ErrorCode::CredentialStoreLocked)
    )
}

/// Selected node, persisted in `ClientConfig::data_dir`.
const SELECTION_FILE: &str = "selection.json";

/// Private per-session state behind [`Client::session`](crate::Client).
///
/// Lock order: `session` before `snapshot`; never the reverse.
#[derive(Default)]
pub(crate) struct SessionState {
    /// Bumped on every sign-in and sign-out; results that belong to an older
    /// session are dropped.
    session: u64,
    /// A saved credential backs this session (it may still be unverified
    /// when the backend was unreachable at launch).
    signed_in: bool,
    /// `/users/me` and `/me/teams` have been loaded for this session.
    account_loaded: bool,
    access: Option<AccessToken>,
    /// Bumped on team switch; profile downloads for an older team are dropped.
    team_epoch: u64,
    profile_raw: Option<Arc<Vec<u8>>>,
    nodes: Vec<Node>,
    /// Last team list from `/me/teams`, served when the current team is
    /// disabled and the backend refuses the list.
    teams: Vec<Team>,
    /// A profile revision the core rejected, with why; keeps
    /// `ProfileStatus::Invalid` across refreshes of that same revision.
    core_rejected: Option<(String, ClientErrorInfo)>,
    /// The last profile a core accepted (this run, or saved by an earlier
    /// one): the session goes back to it when a newer revision is rejected,
    /// and the next launch starts from it (#532).
    accepted: Option<SavedProfile>,
    refresh_task: Option<tokio::task::JoinHandle<()>>,
    /// Wakes the refresh loop when the profile status moves into or out of
    /// `NodesNotReady` outside the loop (a manual refresh, a team switch), so
    /// it starts or ends the nodes-not-ready schedule.
    refresh_wake: Arc<tokio::sync::Notify>,
    /// Notification poll of this session.
    pub(crate) notifications: crate::notifications::NotificationState,
    /// Desktop push registration of this session.
    pub(crate) device: crate::device::DeviceState,
    pub(crate) android_push: crate::device::DeviceState,
    /// Once-per-second traffic sampler of this session.
    pub(crate) traffic_task: Option<tokio::task::JoinHandle<()>>,
    /// Connection monitor (replica and latency) of this session.
    pub(crate) monitor_task: Option<tokio::task::JoinHandle<()>>,
    /// Latest TCP latency to a node: `(node id, latency, measured at)`.
    pub(crate) latency: Option<(String, Option<u32>, std::time::Instant)>,
}

impl SessionState {
    pub(crate) fn active_session(&self) -> Option<u64> {
        self.signed_in.then_some(self.session)
    }

    /// `session` is the current, signed-in session.
    pub(crate) fn is_session(&self, session: u64) -> bool {
        self.signed_in && self.session == session
    }

    /// Forget everything tied to the current session and start a new one.
    fn reset(&mut self, signed_in: bool) -> u64 {
        self.session = self.session.wrapping_add(1);
        self.team_epoch = self.team_epoch.wrapping_add(1);
        self.signed_in = signed_in;
        self.account_loaded = false;
        self.access = None;
        self.profile_raw = None;
        self.nodes.clear();
        self.teams.clear();
        self.core_rejected = None;
        self.accepted = None;
        if let Some(task) = self.refresh_task.take() {
            task.abort();
        }
        // No wake-up left over from the previous session.
        self.refresh_wake = Arc::default();
        self.notifications.reset();
        self.device = crate::device::DeviceState::default();
        self.android_push = crate::device::DeviceState::default();
        if let Some(task) = self.traffic_task.take() {
            task.abort();
        }
        if let Some(task) = self.monitor_task.take() {
            task.abort();
        }
        self.latency = None;
        self.session
    }
}

/// The profile the cores should run.
pub(crate) struct CurrentProfile {
    pub(crate) raw: Arc<Vec<u8>>,
    pub(crate) revision: String,
    pub(crate) team_id: Option<String>,
}

/// A strong [`Client`] reference held by a background task.
///
/// The client owns its tokio runtime, and a runtime panics when dropped from
/// one of its own threads. When a task holds the last reference, dropping
/// this guard releases the client on a fresh thread instead.
pub(crate) struct ClientRef(Option<Arc<Client>>);

impl ClientRef {
    pub(crate) fn upgrade(weak: &Weak<Client>) -> Option<Self> {
        weak.upgrade().map(|client| Self(Some(client)))
    }
}

impl std::ops::Deref for ClientRef {
    type Target = Client;

    fn deref(&self) -> &Client {
        // Only `None` inside `drop`.
        self.0.as_deref().expect("ClientRef used after release")
    }
}

impl Drop for ClientRef {
    fn drop(&mut self) {
        if let Some(client) = self.0.take().and_then(Arc::into_inner) {
            let _ = std::thread::Builder::new()
                .name("ppvpn-client-release".into())
                .spawn(move || drop(client));
        }
    }
}

/// Codes a background account/profile success may clear; other agents'
/// errors (standard core, enhanced mode) stay until their own next success.
fn is_account_error(info: &ClientErrorInfo) -> bool {
    matches!(
        info.code,
        ErrorCode::NetworkUnreachable
            | ErrorCode::ServerUnavailable
            | ErrorCode::RateLimited
            | ErrorCode::AuthSessionInvalid
            | ErrorCode::CredentialStoreFailed
            | ErrorCode::CredentialStoreLocked
            | ErrorCode::NoSubscription
            | ErrorCode::SubscriptionExpired
            | ErrorCode::ProxyNodesNotReady
            | ErrorCode::ProfileFetchFailed
            | ErrorCode::ProfileInvalid
    )
}

fn clear_error(snapshot: &mut ClientSnapshot, user_action: bool) {
    if user_action || snapshot.last_error.as_ref().is_some_and(is_account_error) {
        snapshot.last_error = None;
    }
}

/// The persistent profile state an error stands for (no subscription,
/// expired, team disabled, nodes not ready); such errors never go into
/// `last_error`.
fn blocking_status(error: &ClientError) -> Option<ProfileStatus> {
    match error {
        ClientError::Failed { code, detail } => match code {
            ErrorCode::NoSubscription => Some(ProfileStatus::NoSubscription),
            ErrorCode::SubscriptionExpired => Some(ProfileStatus::SubscriptionExpired {
                expired_at: api::expired_at_from_detail(detail),
            }),
            ErrorCode::TeamDisabled => Some(ProfileStatus::TeamDisabled),
            ErrorCode::ProxyNodesNotReady => Some(ProfileStatus::NodesNotReady),
            _ => None,
        },
        _ => None,
    }
}

/// Failures the next scheduled refresh (not the fast retry) should revisit.
fn is_steady_failure(error: &ClientError) -> bool {
    blocking_status(error).is_some()
        || matches!(
            error,
            ClientError::Failed {
                code: ErrorCode::ProfileInvalid,
                ..
            }
        )
}

#[derive(Serialize, Deserialize)]
struct SelectionFile {
    node_id: String,
}

/// The selected node (`snapshot.selected_node_id`) as both cores read it
/// for every `apply-profile`; reads `None` when unset (tests).
#[derive(Clone, Default)]
pub(crate) struct SelectionSource(Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>);

impl SelectionSource {
    pub(crate) fn new(read: impl Fn() -> Option<String> + Send + Sync + 'static) -> Self {
        Self(Some(Arc::new(read)))
    }

    pub(crate) fn get(&self) -> Option<String> {
        self.0.as_ref().and_then(|read| read())
    }
}

impl std::fmt::Debug for SelectionSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SelectionSource")
    }
}

/// The node chosen in an earlier run, if any.
pub(crate) fn load_selection(data_dir: &str) -> Option<String> {
    let raw = std::fs::read(Path::new(data_dir).join(SELECTION_FILE)).ok()?;
    serde_json::from_slice::<SelectionFile>(&raw)
        .ok()
        .map(|file| file.node_id)
}

fn save_selection(data_dir: &str, node_id: Option<&str>) -> Result<(), String> {
    let path = Path::new(data_dir).join(SELECTION_FILE);
    let Some(node_id) = node_id else {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("remove {}: {error}", path.display())),
        };
    };
    std::fs::create_dir_all(data_dir).map_err(|error| format!("create {data_dir}: {error}"))?;
    let raw = serde_json::to_vec(&SelectionFile {
        node_id: node_id.to_string(),
    })
    .map_err(|error| error.to_string())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, raw).map_err(|error| format!("write {}: {error}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|error| format!("rename {}: {error}", path.display()))
}

impl Client {
    pub(crate) fn session_state(&self) -> MutexGuard<'_, SessionState> {
        self.session
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Raw bytes of the current proxy profile, exactly as downloaded, for
    /// handing to `ppvpn-core` (which validates them).
    pub(crate) fn profile_raw(&self) -> Option<Arc<Vec<u8>>> {
        self.session_state().profile_raw.clone()
    }

    /// The signed-in session's profile with its revision and team, read
    /// consistently (both are updated under the session lock).
    pub(crate) fn current_profile(&self) -> Option<CurrentProfile> {
        let state = self.session_state();
        if !state.signed_in {
            return None;
        }
        let raw = state.profile_raw.clone()?;
        let snapshot = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Some(CurrentProfile {
            raw,
            revision: snapshot.profile.as_ref()?.revision.clone(),
            team_id: snapshot.team.as_ref().map(|team| team.id.clone()),
        })
    }

    pub(crate) fn is_signed_in(&self) -> bool {
        self.session_state().signed_in
    }

    pub(crate) fn profile_nodes(&self) -> Vec<Node> {
        self.session_state().nodes.clone()
    }

    fn profile_status_is(&self, status: &ProfileStatus) -> bool {
        self.snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .profile_status
            == *status
    }

    pub(crate) fn session_is(&self, session: u64) -> bool {
        let state = self.session_state();
        state.signed_in && state.session == session
    }

    pub(crate) fn current_session(&self) -> Result<u64, ClientError> {
        let state = self.session_state();
        if state.signed_in {
            Ok(state.session)
        } else {
            Err(ClientError::NotSignedIn)
        }
    }

    /// Apply `change` only while `session` is current; it returns whether to
    /// publish. The listener runs after both locks are released.
    pub(crate) fn update_session(
        &self,
        session: u64,
        change: impl FnOnce(&mut SessionState, &mut ClientSnapshot) -> bool,
    ) -> bool {
        let snapshot = {
            let mut state = self.session_state();
            if state.session != session {
                return false;
            }
            let mut snapshot = self
                .snapshot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let was_waiting = snapshot.profile_status == ProfileStatus::NodesNotReady;
            if !change(&mut state, &mut snapshot) {
                return false;
            }
            if was_waiting != (snapshot.profile_status == ProfileStatus::NodesNotReady) {
                state.refresh_wake.notify_one();
            }
            snapshot.clone()
        };
        self.emit(snapshot);
        true
    }

    /// Record `error` as the latest user-facing failure and hand it back.
    pub(crate) fn report(&self, error: ClientError) -> ClientError {
        if let Some(info) = error.info() {
            // What the network did, for the logs (#521).
            if matches!(info.code, ErrorCode::NetworkUnreachable) {
                self.diagnose_reachability_after_failure();
            }
            self.update(|snapshot| snapshot.last_error = Some(info));
        }
        error
    }

    // --- sign-in -----------------------------------------------------------

    /// Launch-time restore; runs once on the client's runtime.
    pub(crate) async fn restore_session(&self) {
        let result = {
            let _guard = self.auth.lock().await;
            self.auth.restore().await
        };
        match result {
            Ok(Some(signed_in)) => {
                tracing::info!("session restore: signed in");
                self.begin_session(signed_in, true)
            }
            Ok(None) => {
                tracing::info!("session restore: signed out (no saved login)");
                self.update(|snapshot| {
                    if matches!(snapshot.auth, AuthState::Restoring) {
                        snapshot.auth = AuthState::SignedOut;
                    }
                })
            }
            Err(AuthError::Store(error)) => {
                // The store may simply not be readable yet (locked keychain
                // at login): stay signed out, keep the credential and retry.
                tracing::warn!(
                    "session restore: credential store unavailable ({:?}), retrying: {error}",
                    error.code()
                );
                let info = AuthError::Store(error).into_client_error().info();
                let mut shown = false;
                self.update(|snapshot| {
                    if matches!(snapshot.auth, AuthState::Restoring) {
                        snapshot.auth = AuthState::SignedOut;
                        snapshot.last_error = info;
                        shown = true;
                    }
                });
                if shown {
                    self.spawn_credential_retry();
                }
            }
            Err(error) if error.is_terminal() => {
                let info = error.into_client_error().info();
                tracing::info!(
                    "session restore: signed out, saved login rejected ({:?})",
                    info.as_ref().map(|info| info.code)
                );
                self.update(|snapshot| {
                    if matches!(snapshot.auth, AuthState::Restoring) {
                        snapshot.auth = AuthState::SignedOut;
                        snapshot.last_error = info;
                    }
                });
            }
            Err(error) => {
                // Offline or backend trouble: keep the saved login, show the
                // error and keep retrying in the background.
                let info = error.into_client_error().info();
                tracing::warn!(
                    "session restore: offline, signed in with the saved login, retrying ({:?})",
                    info.as_ref().map(|info| info.code)
                );
                let session = self.session_state().reset(true);
                self.update_session(session, |_, snapshot| {
                    snapshot.auth = AuthState::SignedIn;
                    snapshot.last_error = info;
                    true
                });
                self.load_saved_profile(session);
                self.spawn_refresh_loop(session, RETRY_INTERVAL);
            }
        }
    }

    /// Retries the launch restore until the credential store can be read.
    /// Abandoned as soon as a login starts or is cancelled (the auth
    /// generation moves) or a session begins or ends (the session counter
    /// moves), so a late retry never overrides a newer state.
    ///
    /// A locked store ([`ErrorCode::CredentialStoreLocked`]) is retried the
    /// same way; the platform must not prompt to unlock on every attempt (see
    /// [`crate::PlatformHooks::credential_load`]).
    fn spawn_credential_retry(&self) {
        let generation = self.auth.generation();
        let session = self.session_state().session;
        let weak = self.this.clone();
        let wake = self.credential_retry.wake.clone();
        self.credential_retry.active.store(true, Ordering::SeqCst);
        let active = RetryActive(self.credential_retry.active.clone());
        self.runtime.spawn(async move {
            let _active = active;
            let started = tokio::time::Instant::now();
            let mut attempt: u32 = 0;
            loop {
                attempt = attempt.saturating_add(1);
                let delay = CREDENTIAL_RETRY.delay(started.elapsed());
                tokio::select! {
                    () = tokio::time::sleep(delay) => {}
                    () = wake.notified() => {
                        tracing::info!("credential retry: attempt {attempt} requested by the app");
                    }
                }
                let Some(client) = ClientRef::upgrade(&weak) else {
                    return;
                };
                let current = |client: &Client| {
                    client.auth.generation_is_current(generation)
                        && client.session_state().session == session
                };
                if !current(&client) {
                    tracing::info!("credential retry superseded before attempt {attempt}");
                    return;
                }
                let result = {
                    let _guard = client.auth.lock().await;
                    if !current(&client) {
                        tracing::info!("credential retry superseded during attempt {attempt}");
                        return;
                    }
                    client.auth.restore().await
                };
                if !current(&client) {
                    tracing::info!("credential retry superseded after attempt {attempt}");
                    return;
                }
                match result {
                    Err(AuthError::Store(error)) => {
                        // Every attempt early on, then every tenth, so a
                        // store that never recovers stays visible without
                        // flooding the log.
                        if attempt <= 3 || attempt.is_multiple_of(10) {
                            tracing::info!(
                                "credential store still unavailable (attempt {attempt}): {error}"
                            );
                        }
                        // The store may go from failing to locked (or back).
                        let info = AuthError::Store(error).into_client_error().info();
                        client.update(|snapshot| {
                            if is_store_error(snapshot.last_error.as_ref().map(|info| info.code)) {
                                snapshot.last_error = info;
                            }
                        });
                    }
                    Ok(Some(signed_in)) => {
                        tracing::info!(
                            "credential store readable after {attempt} attempts; restoring"
                        );
                        return client.begin_session(signed_in, true);
                    }
                    // Readable now and nothing saved (or rejected for good).
                    Ok(None) => {
                        tracing::info!("session restore: signed out (store readable, no saved login)");
                        return client.update(|snapshot| {
                            if is_store_error(snapshot.last_error.as_ref().map(|info| info.code)) {
                                snapshot.last_error = None;
                            }
                        })
                    }
                    Err(error) if error.is_terminal() => {
                        let info = error.into_client_error().info();
                        tracing::info!(
                            "session restore: signed out, saved login rejected ({:?})",
                            info.as_ref().map(|info| info.code)
                        );
                        return client.update(|snapshot| snapshot.last_error = info);
                    }
                    Err(error) => {
                        // Readable, but the backend is unreachable: the
                        // normal signed-in retry loop takes over.
                        let info = error.into_client_error().info();
                        tracing::warn!(
                            "session restore: offline, signed in with the saved login, retrying ({:?})",
                            info.as_ref().map(|info| info.code)
                        );
                        let session = client.session_state().reset(true);
                        client.update_session(session, |_, snapshot| {
                            snapshot.auth = AuthState::SignedIn;
                            snapshot.last_error = info;
                            true
                        });
                        client.load_saved_profile(session);
                        return client.spawn_refresh_loop(session, RETRY_INTERVAL);
                    }
                }
            }
        });
    }

    /// Wake a pending credential retry for an immediate attempt; `false`
    /// when none is running.
    pub(crate) fn wake_credential_retry(&self) -> bool {
        if !self.credential_retry.active.load(Ordering::SeqCst) {
            tracing::info!("credential retry requested, but none is pending");
            return false;
        }
        self.credential_retry.wake.notify_waiters();
        true
    }

    pub(crate) async fn start_login(&self) -> Result<DeviceCode, ClientError> {
        let started = match self
            .auth
            .start(
                &crate::auth::device_name(&self.config.platform),
                &self.config.platform,
                &self.config.app_version,
            )
            .await
        {
            Ok(started) => started,
            // Superseded by a newer `auth_start` or `auth_cancel`.
            Err(AuthError::Cancelled) => {
                tracing::info!("device login: superseded before it started");
                return Err(AuthError::Cancelled.into_client_error());
            }
            Err(error) => {
                let error = error.into_client_error();
                let signed_in = self.session_state().signed_in;
                self.update(|snapshot| {
                    snapshot.auth = if signed_in {
                        AuthState::SignedIn
                    } else {
                        AuthState::SignedOut
                    };
                    snapshot.last_error = error.info();
                });
                return Err(error);
            }
        };
        let browser_opened = self.platform.open_url(started.verification_url.clone());
        let host = reqwest::Url::parse(&started.verification_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_string))
            .unwrap_or_default();
        tracing::info!(
            "device login: started, verification host {host}, browser opened {browser_opened}"
        );
        let code = DeviceCode {
            user_code: started.user_code,
            verification_url: started.verification_url,
            expires_in_secs: u32::try_from(started.expires_in.as_secs()).unwrap_or(u32::MAX),
            browser_opened,
        };
        let generation = started.generation;
        self.update(|snapshot| {
            if self.auth.generation_is_current(generation) {
                snapshot.auth = AuthState::AwaitingBrowser { code: code.clone() };
                snapshot.last_error = (!browser_opened).then(|| {
                    ClientErrorInfo::new(
                        ErrorCode::AuthBrowserOpenFailed,
                        "open_url returned false",
                    )
                });
            }
        });
        let weak = self.this.clone();
        self.runtime.spawn(async move {
            if let Some(client) = ClientRef::upgrade(&weak) {
                client.run_login(generation).await;
            }
        });
        Ok(code)
    }

    async fn run_login(&self, generation: u64) {
        let result = match self.auth.poll_until_done(generation).await {
            Ok(pending) => {
                let _guard = self.auth.lock().await;
                self.auth.activate(pending, Some(generation)).await
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(signed_in) => {
                tracing::info!("device login: finished, signed in");
                self.begin_session(signed_in, false)
            }
            Err(AuthError::Cancelled) => {
                tracing::info!("device login: cancelled or superseded");
            }
            Err(error) => {
                let info = error.into_client_error().info();
                tracing::info!(
                    "device login: finished without sign-in ({:?})",
                    info.as_ref().map(|info| info.code)
                );
                let signed_in = self.session_state().signed_in;
                self.update(|snapshot| {
                    if self.auth.generation_is_current(generation) {
                        snapshot.auth = if signed_in {
                            AuthState::SignedIn
                        } else {
                            AuthState::SignedOut
                        };
                        snapshot.last_error = info;
                    }
                });
            }
        }
    }

    pub(crate) fn cancel_login(&self) {
        tracing::info!("device login: cancel requested");
        self.auth.cancel();
        let signed_in = self.session_state().signed_in;
        self.update(|snapshot| {
            if matches!(snapshot.auth, AuthState::AwaitingBrowser { .. }) {
                snapshot.auth = if signed_in {
                    AuthState::SignedIn
                } else {
                    AuthState::SignedOut
                };
            }
        });
    }

    /// Start a new session from a completed sign-in (`restored`: the saved
    /// login at launch) and load its data in the background.
    fn begin_session(&self, signed_in: SignedIn, restored: bool) {
        let (session, replaced) = {
            let mut state = self.session_state();
            let replaced = state.signed_in;
            let session = state.reset(true);
            state.access = Some(signed_in.access);
            if replaced {
                crate::device::remove_agent_files(&self.config.data_dir);
            }
            (session, replaced)
        };
        if replaced {
            // Its files were removed under the session lock before publication.
            self.on_session_ended();
        }
        self.update_session(session, |_, snapshot| {
            snapshot.auth = AuthState::SignedIn;
            snapshot.account = signed_in.account.map(Into::into);
            snapshot.team = None;
            snapshot.profile = None;
            snapshot.profile_status = ProfileStatus::Loading;
            snapshot.last_error = None;
            true
        });
        if restored {
            self.load_saved_profile(session);
        } else {
            // A new sign-in, possibly another account's.
            crate::saved_profile::remove(&self.config.data_dir);
        }
        self.spawn_refresh_loop(session, Duration::ZERO);
    }

    /// Puts the profile an earlier run saved in place at launch, until the
    /// backend serves one the core accepts (#532).
    fn load_saved_profile(&self, session: u64) {
        let Some(saved) = crate::saved_profile::load(&self.config.data_dir, &self.config.api_base)
        else {
            return;
        };
        let parsed = match api::parse_profile(&saved.raw) {
            Ok(parsed) if !parsed.nodes.is_empty() && parsed.summary.revision == saved.revision => {
                parsed
            }
            _ => {
                tracing::warn!("saved profile: not a usable profile, removed");
                crate::saved_profile::remove(&self.config.data_dir);
                return;
            }
        };
        let applied = self.update_session(session, |state, snapshot| {
            if !state.signed_in || state.profile_raw.is_some() {
                // Signed out meanwhile, or the fresh download landed first.
                return false;
            }
            state.profile_raw = Some(saved.raw.clone());
            state.nodes = parsed.nodes;
            state.accepted = Some(saved.clone());
            self.keep_selection(snapshot, &state.nodes, parsed.default_node_id);
            snapshot.profile = Some(parsed.summary);
            snapshot.profile_status = ProfileStatus::Ready;
            true
        });
        if applied {
            tracing::info!(
                "saved profile: revision {} in use until the backend serves one",
                saved.revision
            );
            self.on_profile_changed();
        }
    }

    /// Keeps the selected node when `nodes` has it, else selects the
    /// profile's default (or the first node) and persists that. Called under
    /// the session lock, so it cannot overwrite a concurrent `select_node`.
    fn keep_selection(
        &self,
        snapshot: &mut ClientSnapshot,
        nodes: &[Node],
        default_node_id: Option<String>,
    ) {
        let selection_valid = snapshot
            .selected_node_id
            .as_ref()
            .is_some_and(|id| nodes.iter().any(|node| &node.id == id));
        if selection_valid {
            return;
        }
        let fallback = default_node_id
            .filter(|id| nodes.iter().any(|node| &node.id == id))
            .or_else(|| nodes.first().map(|node| node.id.clone()));
        if let Err(detail) = save_selection(&self.config.data_dir, fallback.as_deref()) {
            tracing::warn!("persist node selection: {detail}");
        }
        snapshot.selected_node_id = fallback;
    }

    /// The backend rejected the saved login for good (credentials are
    /// already cleared): sign out and say why.
    fn end_session(&self, session: u64, error: Option<ClientErrorInfo>) {
        self.drop_session(Some(session), error);
    }

    /// Forget the session, its node selection and everything the snapshot
    /// shows about it; with `only`, only while that session is current.
    fn drop_session(&self, only: Option<u64>, error: Option<ClientErrorInfo>) -> bool {
        let snapshot = {
            let mut state = self.session_state();
            if only.is_some_and(|session| session != state.session) {
                return false;
            }
            state.reset(false);
            if let Err(detail) = save_selection(&self.config.data_dir, None) {
                tracing::warn!("clear node selection: {detail}");
            }
            crate::device::remove_agent_files(&self.config.data_dir);
            crate::saved_profile::remove(&self.config.data_dir);
            let mut snapshot = self
                .snapshot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            snapshot.unread_notifications = 0;
            snapshot.rule_sets_unavailable.clear();
            snapshot.replaced_routes.clear();
            snapshot.auth = AuthState::SignedOut;
            snapshot.account = None;
            snapshot.team = None;
            snapshot.profile = None;
            snapshot.profile_status = ProfileStatus::Loading;
            snapshot.selected_node_id = None;
            snapshot.last_error = error;
            snapshot.clone()
        };
        self.emit(snapshot);
        self.set_connection_detail(crate::ConnectionDetail::default());
        self.on_session_ended();
        true
    }

    pub(crate) async fn sign_out(&self) -> Result<(), ClientError> {
        tracing::info!("logout");
        self.auth.cancel();
        // While the session can still authenticate.
        self.unregister_device().await;
        self.drop_session(None, None);
        self.stop_cores().await;
        let result = {
            let _guard = self.auth.lock().await;
            self.auth.logout().await
        };
        result.map_err(|error| self.report(error.into_client_error()))
    }

    // --- authorized calls ----------------------------------------------------

    fn fresh_access(&self, session: u64) -> Result<Option<String>, ClientError> {
        let state = self.session_state();
        if !state.signed_in || state.session != session {
            return Err(ClientError::NotSignedIn);
        }
        Ok(state
            .access
            .as_ref()
            .filter(|access| access.is_fresh(ACCESS_TOKEN_MARGIN))
            .map(|access| access.token.clone()))
    }

    /// A usable access token for `session`, refreshing it when needed. A
    /// terminal refresh failure ends the session.
    async fn ensure_access(&self, session: u64) -> Result<String, ClientError> {
        if let Some(token) = self.fresh_access(session)? {
            return Ok(token);
        }
        let _guard = self.auth.lock().await;
        if let Some(token) = self.fresh_access(session)? {
            return Ok(token);
        }
        match self.auth.refresh_or_clear().await {
            Ok(access) => {
                let token = access.token.clone();
                let mut state = self.session_state();
                if state.session != session {
                    return Err(ClientError::NotSignedIn);
                }
                state.access = Some(access);
                drop(state);
                self.on_access_refreshed(session);
                Ok(token)
            }
            Err(error) if error.is_terminal() => {
                let error = error.into_client_error();
                self.end_session(session, error.info());
                Err(error)
            }
            Err(error) => Err(error.into_client_error()),
        }
    }

    fn invalidate_access(&self, session: u64, token: &str) {
        let mut state = self.session_state();
        if state.session == session
            && state
                .access
                .as_ref()
                .is_some_and(|access| access.token == token)
        {
            state.access = None;
        }
    }

    /// Run a bearer call, refreshing and retrying once on 401.
    pub(crate) async fn bearer<T, F, Fut>(
        &self,
        session: u64,
        call: F,
        map: fn(ApiError) -> ClientError,
    ) -> Result<T, ClientError>
    where
        F: Fn(String) -> Fut,
        Fut: Future<Output = Result<T, ApiError>>,
    {
        let token = self.ensure_access(session).await?;
        with_bearer(
            token,
            call,
            |refused| async move {
                self.invalidate_access(session, &refused);
                self.ensure_access(session).await
            },
            map,
        )
        .await
    }

    // --- account, teams, profile -----------------------------------------------

    /// Account, current team and profile for a new (or not yet verified)
    /// session.
    async fn load_session_data(&self, session: u64) -> Result<(), ClientError> {
        let epoch = self.session_state().team_epoch;
        let has_account = self
            .snapshot
            .lock()
            .map(|s| s.account.is_some())
            .unwrap_or(false);
        if !has_account {
            let account = self
                .bearer(
                    session,
                    |token| async move { self.auth.api().account(&token).await },
                    ApiError::into_client_error,
                )
                .await
                .map_err(|error| self.report_profile_failure(session, epoch, error, false))?;
            self.update_session(session, |_, snapshot| {
                snapshot.account = Some(account.into());
                clear_error(snapshot, false);
                true
            });
        }
        self.fetch_teams(session, false).await?;
        self.session_state().account_loaded = true;
        self.fetch_profile(session, false).await
    }

    async fn fetch_teams(&self, session: u64, user_action: bool) -> Result<Vec<Team>, ClientError> {
        let epoch = self.session_state().team_epoch;
        let entries = match self
            .bearer(
                session,
                |token| async move { self.auth.api().teams(&token).await },
                ApiError::into_client_error,
            )
            .await
        {
            Ok(entries) => entries,
            Err(error) => {
                let disabled = matches!(
                    error,
                    ClientError::Failed {
                        code: ErrorCode::TeamDisabled,
                        ..
                    }
                );
                let error = self.report_profile_failure(session, epoch, error, user_action);
                // Keep the team picker usable from a disabled team.
                let cached = self.session_state().teams.clone();
                return if disabled && !cached.is_empty() {
                    Ok(cached)
                } else {
                    Err(error)
                };
            }
        };
        let teams: Vec<Team> = entries.iter().map(TeamEntry::to_team).collect();
        let current = entries
            .iter()
            .find(|entry| entry.is_default)
            .map(TeamEntry::to_team);
        self.update_session(session, |state, snapshot| {
            state.teams = teams.clone();
            if current.is_some() {
                snapshot.team = current;
            }
            clear_error(snapshot, user_action);
            true
        });
        Ok(teams)
    }

    pub(crate) async fn list_teams(&self) -> Result<Vec<Team>, ClientError> {
        let session = self.current_session()?;
        self.fetch_teams(session, true).await
    }

    pub(crate) async fn change_team(&self, team_id: String) -> Result<(), ClientError> {
        let session = self.current_session()?;
        let team_id = team_id.as_str();
        let response = self
            .bearer(
                session,
                |token| async move { self.auth.api().switch_team(&token, team_id).await },
                ApiError::into_client_error,
            )
            .await
            // A refused switch (e.g. into a disabled team) leaves the current
            // team as it was; it is a one-off failure, not a profile state.
            .map_err(|error| {
                tracing::info!(
                    "team switch: failed ({:?})",
                    error.info().map(|info| info.code)
                );
                self.report(error)
            })?;
        // The response carries a new desktop token bound to the new team.
        let access = self
            .auth
            .access_token_from(response.token, None)
            .map_err(|error| self.report(error.into_client_error()))?;
        let team = Team {
            id: response.id,
            name: response.name,
            personal: response.is_personal,
            active: true,
        };
        let applied = self.update_session(session, |state, snapshot| {
            state.access = Some(access);
            state.team_epoch = state.team_epoch.wrapping_add(1);
            state.profile_raw = None;
            state.nodes.clear();
            state.core_rejected = None;
            // Another team's profile.
            state.accepted = None;
            crate::saved_profile::remove(&self.config.data_dir);
            snapshot.team = Some(team);
            snapshot.profile = None;
            snapshot.profile_status = ProfileStatus::Loading;
            snapshot.last_error = None;
            true
        });
        if !applied {
            return Err(ClientError::NotSignedIn);
        }
        tracing::info!("team switch: switched to team {team_id}");
        self.fetch_profile(session, true).await
    }

    pub(crate) async fn refresh_profile_now(&self) -> Result<(), ClientError> {
        let session = self.current_session()?;
        self.fetch_profile(session, true).await
    }

    /// Record a failure of a team-scoped call. A persistent state (no
    /// subscription, expired, team disabled, nodes not ready) becomes
    /// `profile_status`, drops the profile and stops the cores, and stays out
    /// of `last_error`; any other failure is a one-off shown in `last_error`,
    /// leaving the status.
    fn report_profile_failure(
        &self,
        session: u64,
        epoch: u64,
        error: ClientError,
        user_action: bool,
    ) -> ClientError {
        let Some(status) = blocking_status(&error) else {
            return self.report(error);
        };
        let applied = self.update_session(session, |state, snapshot| {
            if state.team_epoch != epoch {
                return false;
            }
            state.profile_raw = None;
            state.nodes.clear();
            state.core_rejected = None;
            // Nothing to fall back to: the team has no usable profile now.
            state.accepted = None;
            crate::saved_profile::remove(&self.config.data_dir);
            snapshot.profile = None;
            snapshot.profile_status = status;
            clear_error(snapshot, user_action);
            true
        });
        if applied {
            // No profile any more: the cores stop.
            self.on_profile_changed();
        }
        error
    }

    /// The core refused `revision` of the current profile. With an earlier
    /// profile a core accepted, the session goes back to it and the cores
    /// run it again; returns whether it did (#532).
    pub(crate) fn mark_core_rejected(&self, revision: &str, info: ClientErrorInfo) -> bool {
        // The core's code and field lead the detail, for support (#532).
        tracing::warn!(
            "profile revision {revision} rejected by the core: {}",
            info.detail
        );
        let session = self.session_state().session;
        let mut previous = None;
        self.update_session(session, |state, snapshot| {
            let current = snapshot
                .profile
                .as_ref()
                .is_some_and(|profile| profile.revision == revision);
            if !current {
                return false;
            }
            state.core_rejected = Some((revision.to_string(), info.clone()));
            let fallback = state
                .accepted
                .clone()
                .filter(|accepted| accepted.revision != revision)
                .and_then(|accepted| {
                    let parsed = api::parse_profile(&accepted.raw).ok()?;
                    Some((accepted, parsed))
                });
            if let Some((accepted, parsed)) = fallback {
                state.profile_raw = Some(accepted.raw);
                state.nodes = parsed.nodes;
                self.keep_selection(snapshot, &state.nodes, parsed.default_node_id);
                snapshot.profile = Some(parsed.summary);
                previous = Some(accepted.revision);
            }
            snapshot.profile_status = ProfileStatus::Invalid {
                error: info.clone(),
                previous: previous.is_some(),
            };
            true
        });
        if let Some(previous) = &previous {
            tracing::info!("profile: back to revision {previous}, which the core accepted");
            self.on_profile_changed();
        }
        previous.is_some()
    }

    /// A core accepted `current`: the session falls back to it and the next
    /// launch starts from it (#532).
    pub(crate) fn mark_core_accepted(&self, current: &CurrentProfile) {
        let session = self.session_state().session;
        self.update_session(session, |state, _| {
            let known = state
                .accepted
                .as_ref()
                .is_some_and(|accepted| accepted.revision == current.revision);
            if state.signed_in && !known {
                let accepted = SavedProfile {
                    revision: current.revision.clone(),
                    raw: current.raw.clone(),
                };
                // Under the session lock: a sign-out cannot come between and
                // leave the file behind.
                crate::saved_profile::save(&self.config.data_dir, &self.config.api_base, &accepted);
                state.accepted = Some(accepted);
            }
            // Nothing the snapshot shows changed.
            false
        });
    }

    /// Download the profile, keep its raw bytes and publish the UI summary.
    async fn fetch_profile(&self, session: u64, user_action: bool) -> Result<(), ClientError> {
        let epoch = self.session_state().team_epoch;
        let raw = match self
            .bearer(
                session,
                |token| async move { self.auth.api().proxy_profile(&token).await },
                ApiError::into_profile_error,
            )
            .await
        {
            Ok(raw) => raw,
            Err(error) => {
                return Err(self.report_profile_failure(session, epoch, error, user_action))
            }
        };
        let parsed = match api::parse_profile(&raw) {
            Ok(parsed) => parsed,
            Err(error) => {
                // Persistent until the backend serves a valid profile; the
                // previously valid one (if any) stays in use.
                if let Some(info) = error.info() {
                    self.update_session(session, |state, snapshot| {
                        if state.team_epoch != epoch {
                            return false;
                        }
                        snapshot.profile_status = ProfileStatus::Invalid {
                            error: info,
                            previous: state.profile_raw.is_some(),
                        };
                        clear_error(snapshot, user_action);
                        true
                    });
                }
                return Err(error);
            }
        };
        if parsed.nodes.is_empty() {
            return Err(self.report_profile_failure(
                session,
                epoch,
                ClientError::failed(ErrorCode::NoSubscription, "profile has no nodes"),
                user_action,
            ));
        }
        let mut changed = false;
        let mut standard_failed = false;
        let applied = self.update_session(session, |state, snapshot| {
            if state.team_epoch != epoch {
                return false;
            }
            let known_rejected = state
                .core_rejected
                .as_ref()
                .is_some_and(|(revision, _)| *revision == parsed.summary.revision);
            let on_previous = snapshot
                .profile
                .as_ref()
                .is_some_and(|profile| profile.revision != parsed.summary.revision);
            if known_rejected && on_previous && state.profile_raw.is_some() {
                // The core refused this revision already: the earlier one it
                // accepted stays in use (#532).
                clear_error(snapshot, user_action);
                return true;
            }
            changed = state.profile_raw.is_none()
                || snapshot.profile.as_ref().map(|profile| &profile.revision)
                    != Some(&parsed.summary.revision);
            standard_failed = matches!(snapshot.standard, StandardState::Failed { .. });
            state.profile_raw = Some(Arc::new(raw));
            state.nodes = parsed.nodes;
            self.keep_selection(snapshot, &state.nodes, parsed.default_node_id);
            snapshot.profile_status = match &state.core_rejected {
                Some((revision, info)) if *revision == parsed.summary.revision => {
                    ProfileStatus::Invalid {
                        error: info.clone(),
                        previous: false,
                    }
                }
                _ => ProfileStatus::Ready,
            };
            snapshot.profile = Some(parsed.summary);
            clear_error(snapshot, user_action);
            true
        });
        if !applied {
            return Ok(());
        }
        // Before the cores get it: they drop such pins on their own too.
        let nodes = self.profile_nodes();
        self.prune_ingress_pins(&nodes);
        // A failed standard core gets another attempt on every refresh (manual
        // or the 5-minute loop), even when the profile is unchanged.
        if changed || standard_failed {
            self.on_profile_changed();
        }
        Ok(())
    }

    pub(crate) async fn choose_node(&self, node_id: String) -> Result<(), ClientError> {
        let session = self.current_session()?;
        if !self
            .session_state()
            .nodes
            .iter()
            .any(|node| node.id == node_id)
        {
            return Err(self.report(ClientError::failed(ErrorCode::NodeNotFound, node_id)));
        }
        let mut saved = Ok(());
        let applied = self.update_session(session, |_, snapshot| {
            saved = save_selection(&self.config.data_dir, Some(&node_id));
            if saved.is_err() {
                return false;
            }
            snapshot.selected_node_id = Some(node_id.clone());
            snapshot.last_error = None;
            true
        });
        if let Err(detail) = saved {
            return Err(self.report(ClientError::failed(ErrorCode::LocalStorageFailed, detail)));
        }
        if !applied {
            return Err(ClientError::NotSignedIn);
        }
        // Applied to the standard core, and live while enhanced mode is on;
        // every later apply and connect carries it.
        self.apply_selection(&node_id)
            .await
            .map_err(|error| self.report(error))
    }

    // --- background refresh ------------------------------------------------------

    /// Load session data now (after `first_delay`), then refresh the profile
    /// every 5 minutes; failures retry sooner.
    ///
    /// While the status is `NodesNotReady` the profile is fetched again after
    /// [`NODES_NOT_READY_RETRY`] (30 s, 60 s, then every 2 minutes) until the
    /// status changes. The schedule lives in this one loop, so it never piles
    /// up; it ends with the session (logout aborts the task) and restarts or
    /// ends through `refresh_wake` when another path moves the status into or
    /// out of `NodesNotReady`.
    fn spawn_refresh_loop(&self, session: u64, first_delay: Duration) {
        let weak = self.this.clone();
        let wake = self.session_state().refresh_wake.clone();
        let task = self.runtime.spawn(async move {
            let mut due = tokio::time::Instant::now() + first_delay;
            // Re-fetches in a row while the nodes are not ready.
            let mut not_ready = 0;
            loop {
                let woken = tokio::select! {
                    () = tokio::time::sleep_until(due) => false,
                    () = wake.notified() => true,
                };
                let Some(client) = ClientRef::upgrade(&weak) else {
                    return;
                };
                if !client.session_is(session) {
                    return;
                }
                if woken {
                    let waiting = client.profile_status_is(&ProfileStatus::NodesNotReady);
                    if waiting && not_ready == 0 {
                        due = tokio::time::Instant::now() + nodes_not_ready_delay(0);
                        not_ready = 1;
                    } else if !waiting && not_ready > 0 {
                        due = tokio::time::Instant::now() + PROFILE_REFRESH_INTERVAL;
                        not_ready = 0;
                    }
                    continue;
                }
                let loaded = client.session_state().account_loaded;
                let result = if loaded {
                    client.fetch_profile(session, false).await
                } else {
                    client.load_session_data(session).await
                };
                if client.session_state().account_loaded {
                    client.ensure_device_registered(session).await;
                }
                let delay = match result {
                    Err(ClientError::NotSignedIn) => return,
                    _ if client.profile_status_is(&ProfileStatus::NodesNotReady) => {
                        not_ready += 1;
                        nodes_not_ready_delay(not_ready - 1)
                    }
                    Ok(()) => PROFILE_REFRESH_INTERVAL,
                    Err(error) if is_steady_failure(&error) => PROFILE_REFRESH_INTERVAL,
                    Err(_) => RETRY_INTERVAL,
                };
                if !client.profile_status_is(&ProfileStatus::NodesNotReady) {
                    not_ready = 0;
                }
                due = tokio::time::Instant::now() + delay;
            }
        });
        {
            let mut state = self.session_state();
            if state.session == session && state.signed_in {
                if let Some(previous) = state.refresh_task.replace(task) {
                    previous.abort();
                }
            } else {
                task.abort();
                return;
            }
        }
        self.spawn_notification_loop(session);
        self.spawn_traffic_loop(session);
        self.spawn_monitor(session);
    }
}

#[cfg(test)]
mod tests {
    use super::load_selection;
    use crate::auth::test_support::{serve, serve_with, token_set, MemoryPlatform};
    use crate::{
        AuthState, Client, ClientConfig, ClientError, ClientListener, ClientSnapshot, ErrorCode,
        PlatformError, PlatformHooks, ProbeResult, ProfileStatus, TrafficSample,
    };
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    const FIXTURE: &str = include_str!("../../account/tests/fixtures/proxy-profile.json");
    const DEFAULT_NODE: &str = "7d7c34e4-7f38-4c0c-9a53-1f0c0c9e2b11-101";

    #[derive(Default)]
    struct Recorder(Mutex<Vec<ClientSnapshot>>);

    impl ClientListener for Recorder {
        fn on_snapshot(&self, snapshot: ClientSnapshot) {
            self.0.lock().unwrap().push(snapshot);
        }
        fn on_probe_result(&self, _: ProbeResult) {}
        fn on_traffic(&self, _: TrafficSample) {}
    }

    fn client(base: &str, platform: Arc<dyn PlatformHooks>) -> (Arc<Client>, String) {
        let data_dir = std::env::temp_dir()
            .join(format!("ppvpn-client-test-{}", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        let config = ClientConfig {
            api_base: base.to_string(),
            data_dir: data_dir.clone(),
            log_dir: data_dir.clone(),
            platform: "macos".into(),
            app_version: "0.0.0-test".into(),
        };
        (
            Client::with_parts(
                config,
                platform,
                Arc::new(Recorder::default()),
                Arc::new(crate::cores::test_support::NoService),
                crate::cores::test_support::FakeLauncher::new(),
                Arc::new(crate::sysproxy::tests::FakeWriter::default()),
            ),
            data_dir,
        )
    }

    fn wait_for(client: &Client, what: impl Fn(&ClientSnapshot) -> bool) -> ClientSnapshot {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = client.snapshot();
            if what(&snapshot) {
                return snapshot;
            }
            assert!(
                Instant::now() < deadline,
                "timed out; last snapshot {snapshot:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    fn signed_in_data() -> Vec<(&'static str, String)> {
        vec![
            ("200 OK", r#"{"id":"u1","name":"Jerry","avatar":""}"#.into()),
            (
                "200 OK",
                r#"{"items":[{"id":"t1","name":"Personal","is_personal":true,"is_default":true},{"id":"t2","name":"Work","is_personal":false,"is_default":false}]}"#.into(),
            ),
            ("200 OK", FIXTURE.into()),
        ]
    }

    #[test]
    fn restore_loads_session_then_select_and_logout() {
        let mut responses = vec![
            ("200 OK", r#"{"refresh_token":"jyr_prepared"}"#.to_string()),
            ("200 OK", token_set("jyr_active")),
        ];
        responses.extend(signed_in_data());
        responses.push(("200 OK", r#"{"revoked":true}"#.into()));
        let (base, paths) = serve(responses);
        let platform = MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#);
        let (client, data_dir) = client(&base, platform.clone());

        let snapshot = wait_for(&client, |s| s.profile.is_some());
        assert!(matches!(snapshot.auth, AuthState::SignedIn));
        assert_eq!(snapshot.account.as_ref().unwrap().name, "Jerry");
        assert_eq!(snapshot.account.as_ref().unwrap().avatar_url, None);
        assert_eq!(snapshot.team.as_ref().unwrap().id, "t1");
        assert_eq!(snapshot.profile.as_ref().unwrap().node_count, 3);
        assert_eq!(snapshot.selected_node_id.as_deref(), Some(DEFAULT_NODE));
        assert!(snapshot.last_error.is_none());
        assert_eq!(client.nodes().len(), 3);
        assert_eq!(client.profile_raw().unwrap().as_slice(), FIXTURE.as_bytes());
        assert_eq!(platform.stored().unwrap()["active_refresh"], "jyr_active");

        let error = block_on(client.select_node("missing".into())).unwrap_err();
        assert!(matches!(
            error,
            ClientError::Failed {
                code: ErrorCode::NodeNotFound,
                ..
            }
        ));
        let third = client.nodes()[2].id.clone();
        block_on(client.select_node(third.clone())).unwrap();
        assert_eq!(
            client.snapshot().selected_node_id.as_deref(),
            Some(third.as_str())
        );
        assert!(client.snapshot().last_error.is_none());
        assert_eq!(load_selection(&data_dir).as_deref(), Some(third.as_str()));

        block_on(client.logout()).unwrap();
        let snapshot = client.snapshot();
        assert!(matches!(snapshot.auth, AuthState::SignedOut));
        assert!(snapshot.account.is_none() && snapshot.profile.is_none());
        assert!(client.nodes().is_empty());
        assert!(platform.stored().is_none());
        assert_eq!(load_selection(&data_dir), None);
        assert_eq!(
            paths.lock().unwrap().as_slice(),
            [
                "POST /api/v1/auth/device/refresh",
                "POST /api/v1/auth/device/refresh/commit",
                "GET /api/v1/users/me",
                "GET /api/v1/me/teams",
                "GET /api/v1/me/proxy-profile",
                "POST /api/v1/auth/device/revoke",
            ]
        );
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn offline_restore_keeps_credentials_and_reports() {
        let platform = MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_keep"}"#);
        let (client, _) = client("http://127.0.0.1:9", platform.clone());
        let snapshot = wait_for(&client, |s| s.last_error.is_some());
        assert!(matches!(snapshot.auth, AuthState::SignedIn));
        assert_eq!(
            snapshot.last_error.unwrap().code,
            ErrorCode::NetworkUnreachable
        );
        assert_eq!(platform.stored().unwrap()["active_refresh"], "jyr_keep");
    }

    #[test]
    fn rejected_restore_signs_out_and_clears_credentials() {
        let (base, _) = serve(vec![(
            "401 Unauthorized",
            r#"{"code":"AUTH_DEVICE_CREDENTIAL_INVALID"}"#.into(),
        )]);
        let platform = MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#);
        let (client, _) = client(&base, platform.clone());
        let snapshot = wait_for(&client, |s| matches!(s.auth, AuthState::SignedOut));
        assert_eq!(
            snapshot.last_error.unwrap().code,
            ErrorCode::AuthSessionInvalid
        );
        assert!(platform.stored().is_none());
    }

    /// Fails `credential_load` until `failures` reaches zero (`usize::MAX`
    /// = forever), then behaves like the wrapped store. `locked` fails with
    /// `PlatformError::Locked` instead of `Failed`.
    struct LockedStore {
        inner: Arc<MemoryPlatform>,
        failures: Mutex<usize>,
        loads: Mutex<usize>,
        locked: bool,
    }

    impl LockedStore {
        fn new(inner: Arc<MemoryPlatform>, failures: usize) -> Arc<Self> {
            Arc::new(Self {
                inner,
                failures: Mutex::new(failures),
                loads: Mutex::new(0),
                locked: false,
            })
        }

        fn locked(inner: Arc<MemoryPlatform>, failures: usize) -> Arc<Self> {
            Arc::new(Self {
                inner,
                failures: Mutex::new(failures),
                loads: Mutex::new(0),
                locked: true,
            })
        }
    }

    impl PlatformHooks for LockedStore {
        fn credential_load(&self) -> Result<Option<Vec<u8>>, PlatformError> {
            *self.loads.lock().unwrap() += 1;
            let mut failures = self.failures.lock().unwrap();
            if *failures > 0 {
                *failures = failures.saturating_sub(1);
                let message = "keychain locked".to_string();
                return Err(if self.locked {
                    PlatformError::Locked { message }
                } else {
                    PlatformError::Failed { message }
                });
            }
            self.inner.credential_load()
        }
        fn credential_save(&self, blob: Vec<u8>) -> Result<(), PlatformError> {
            self.inner.credential_save(blob)
        }
        fn credential_delete(&self) -> Result<(), PlatformError> {
            self.inner.credential_delete()
        }
        fn open_url(&self, url: String) -> bool {
            self.inner.open_url(url)
        }
        fn privileged_service_installed(&self) -> bool {
            false
        }
        fn install_privileged_service(&self) -> Result<(), PlatformError> {
            Ok(())
        }
        fn uninstall_privileged_service(&self) -> Result<(), PlatformError> {
            Ok(())
        }
    }

    #[test]
    fn locked_store_is_retried_until_readable_then_restores() {
        let mut responses = vec![
            ("200 OK", r#"{"refresh_token":"jyr_prepared"}"#.to_string()),
            ("200 OK", token_set("jyr_active")),
        ];
        responses.extend(signed_in_data());
        let (base, _) = serve(responses);
        let inner = MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#);
        let store = LockedStore::new(inner.clone(), 3);
        let (client, _) = client(&base, store.clone());

        let snapshot = wait_for(&client, |s| s.profile.is_some());
        assert!(matches!(snapshot.auth, AuthState::SignedIn));
        assert!(snapshot.last_error.is_none());
        assert_eq!(*store.loads.lock().unwrap(), 4, "3 failures, then success");
        assert_eq!(inner.stored().unwrap()["active_refresh"], "jyr_active");
    }

    #[test]
    fn locked_store_keeps_the_login_and_restores_once_unlocked() {
        let mut responses = vec![
            ("200 OK", r#"{"refresh_token":"jyr_prepared"}"#.to_string()),
            ("200 OK", token_set("jyr_active")),
        ];
        responses.extend(signed_in_data());
        let (base, _) = serve(responses);
        let inner = MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#);
        let store = LockedStore::locked(inner.clone(), usize::MAX);
        let (client, _) = client(&base, store.clone());

        let snapshot = wait_for(&client, |s| {
            matches!(s.auth, AuthState::SignedOut) && *store.loads.lock().unwrap() >= 3
        });
        let error = snapshot.last_error.unwrap();
        assert_eq!(error.code, ErrorCode::CredentialStoreLocked);
        assert!(error.detail.contains("keychain locked"), "{}", error.detail);
        // The saved login is kept while the store is locked.
        assert_eq!(inner.stored().unwrap()["active_refresh"], "jyr_old");

        // "Retry" wakes the pending background read.
        assert!(client.retry_credential_restore());
        *store.failures.lock().unwrap() = 0;
        let snapshot = wait_for(&client, |s| s.profile.is_some());
        assert!(matches!(snapshot.auth, AuthState::SignedIn));
        assert!(snapshot.last_error.is_none());
        assert_eq!(inner.stored().unwrap()["active_refresh"], "jyr_active");
        // Restored: nothing left to retry.
        assert!(!client.retry_credential_restore());
    }

    #[test]
    fn retry_credential_restore_without_a_pending_retry_is_false() {
        let (client, _) = client("http://127.0.0.1:9", Arc::new(MemoryPlatform::default()));
        wait_for(&client, |s| matches!(s.auth, AuthState::SignedOut));
        assert!(!client.retry_credential_restore());
    }

    #[test]
    fn auth_start_cancels_the_credential_retry() {
        let (base, _) = serve_with(|base| {
            vec![(
                "200 OK",
                format!(
                    r#"{{"device_code":"{}","user_code":"ABCD-EFGH","verification_uri_complete":"{base}/dashboard/device/authorize?user_code=ABCD-EFGH","expires_in":600,"interval":5}}"#,
                    "d".repeat(43)
                ),
            )]
        });
        let inner = MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#);
        let store = LockedStore::new(inner, usize::MAX);
        let (client, _) = client(&base, store.clone());

        let snapshot = wait_for(&client, |s| {
            matches!(s.auth, AuthState::SignedOut) && *store.loads.lock().unwrap() >= 2
        });
        assert_eq!(
            snapshot.last_error.unwrap().code,
            ErrorCode::CredentialStoreFailed
        );
        block_on(client.auth_start()).unwrap();
        // Unlock the store: a surviving retry would now restore the session.
        *store.failures.lock().unwrap() = 0;
        let loads = *store.loads.lock().unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(*store.loads.lock().unwrap(), loads, "retry stopped");
        assert!(matches!(
            client.snapshot().auth,
            AuthState::AwaitingBrowser { .. }
        ));
        client.auth_cancel();
    }

    #[test]
    fn unchanged_snapshots_are_not_resent() {
        let recorder = Arc::new(Recorder::default());
        let data_dir = std::env::temp_dir()
            .join(format!("ppvpn-client-test-{}", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        let config = ClientConfig {
            api_base: "http://127.0.0.1:9".into(),
            data_dir: data_dir.clone(),
            log_dir: data_dir.clone(),
            platform: "macos".into(),
            app_version: "0.0.0-test".into(),
        };
        let client = Client::with_parts(
            config,
            Arc::new(MemoryPlatform::default()),
            recorder.clone(),
            Arc::new(crate::cores::test_support::NoService),
            crate::cores::test_support::FakeLauncher::new(),
            Arc::new(crate::sysproxy::tests::FakeWriter::default()),
        );
        wait_for(&client, |s| matches!(s.auth, AuthState::SignedOut));
        std::thread::sleep(Duration::from_millis(200));
        let before = recorder.0.lock().unwrap().len();
        client.update(|_| {});
        client.update(|_| {});
        assert_eq!(recorder.0.lock().unwrap().len(), before);
        let sent = recorder.0.lock().unwrap().clone();
        assert!(sent.windows(2).all(|pair| pair[0] != pair[1]));
    }

    #[test]
    fn empty_store_restores_to_signed_out() {
        let (client, _) = client("http://127.0.0.1:9", Arc::new(MemoryPlatform::default()));
        let snapshot = wait_for(&client, |s| matches!(s.auth, AuthState::SignedOut));
        assert!(snapshot.last_error.is_none());
        assert!(matches!(
            block_on(client.teams()),
            Err(ClientError::NotSignedIn)
        ));
    }

    #[test]
    fn device_login_opens_browser_and_signs_in() {
        let (base, paths) = serve_with(|base| {
            let mut responses = vec![
                (
                    "200 OK",
                    format!(
                        r#"{{"device_code":"{}","user_code":"ABCD-EFGH","verification_uri":"{base}/dashboard/device/authorize","verification_uri_complete":"{base}/dashboard/device/authorize?user_code=ABCD-EFGH","expires_in":600,"interval":1}}"#,
                        "d".repeat(43)
                    ),
                ),
                (
                    "200 OK",
                    format!(
                        r#"{{"status":"authorized","refresh_token":"jyr_{}"}}"#,
                        "p".repeat(48)
                    ),
                ),
                ("200 OK", token_set("jyr_active")),
            ];
            responses.extend(signed_in_data());
            responses
        });
        let platform = Arc::new(MemoryPlatform::default());
        let (client, _) = client(&base, platform.clone());
        wait_for(&client, |s| matches!(s.auth, AuthState::SignedOut));

        let code = block_on(client.auth_start()).unwrap();
        assert_eq!(code.user_code, "ABCD-EFGH");
        assert!(code.browser_opened);
        assert_eq!(
            platform.opened.lock().unwrap().as_slice(),
            std::slice::from_ref(&code.verification_url)
        );
        assert!(matches!(
            client.snapshot().auth,
            AuthState::AwaitingBrowser { .. }
        ));

        let snapshot = wait_for(&client, |s| s.profile.is_some());
        assert!(matches!(snapshot.auth, AuthState::SignedIn));
        assert_eq!(snapshot.account.unwrap().id, "u1");
        assert_eq!(platform.stored().unwrap()["active_refresh"], "jyr_active");
        assert_eq!(
            paths.lock().unwrap().as_slice(),
            [
                "POST /api/v1/auth/device/code",
                "POST /api/v1/auth/device/token",
                "POST /api/v1/auth/device/activate",
                "GET /api/v1/users/me",
                "GET /api/v1/me/teams",
                "GET /api/v1/me/proxy-profile",
            ]
        );
    }

    #[test]
    fn cancelled_login_returns_to_signed_out() {
        let (base, _) = serve_with(|base| {
            vec![(
                "200 OK",
                format!(
                    r#"{{"device_code":"{}","user_code":"ABCD-EFGH","verification_uri_complete":"{base}/dashboard/device/authorize?user_code=ABCD-EFGH","expires_in":600,"interval":5}}"#,
                    "d".repeat(43)
                ),
            )]
        });
        let (client, _) = client(&base, Arc::new(MemoryPlatform::default()));
        wait_for(&client, |s| matches!(s.auth, AuthState::SignedOut));
        block_on(client.auth_start()).unwrap();
        client.auth_cancel();
        assert!(matches!(client.snapshot().auth, AuthState::SignedOut));
        // Cancellation is not a failure: nothing to show.
        assert!(client.snapshot().last_error.is_none());
        assert!(ClientError::Cancelled.info().is_none());
    }

    /// The proxy-profile answer while the subscription's nodes are not
    /// published yet, in the backend's whole problem shape (root-level
    /// fields, no `data` wrapper); only `code` counts.
    fn nodes_not_ready() -> (&'static str, String) {
        (
            "404 Not Found",
            r#"{"type":"about:blank","title":"Not Found","status":404,"detail":"proxy nodes are not ready yet","code":"PROXY_NODES_NOT_READY"}"#.into(),
        )
    }

    /// A restore from a saved login up to the first profile download, which
    /// gets the first of `profiles`.
    fn restore_then(
        profiles: impl IntoIterator<Item = (&'static str, String)>,
    ) -> Vec<(&'static str, String)> {
        let mut responses = vec![
            ("200 OK", r#"{"refresh_token":"jyr_prepared"}"#.to_string()),
            ("200 OK", token_set("jyr_active")),
        ];
        responses.extend(signed_in_data());
        responses.pop();
        responses.extend(profiles);
        responses
    }

    fn profile_fetches(paths: &Mutex<Vec<String>>) -> usize {
        paths
            .lock()
            .unwrap()
            .iter()
            .filter(|path| *path == "GET /api/v1/me/proxy-profile")
            .count()
    }

    fn saved_login() -> Arc<MemoryPlatform> {
        MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#)
    }

    #[test]
    fn nodes_not_ready_is_fetched_again_until_ready() {
        let (base, paths) = serve(restore_then([
            nodes_not_ready(),
            nodes_not_ready(),
            ("200 OK", FIXTURE.into()),
            // Never asked for: Ready goes back to the 5-minute refresh.
            nodes_not_ready(),
        ]));
        let (client, data_dir) = client(&base, saved_login());

        let snapshot = wait_for(&client, |s| {
            s.profile_status == ProfileStatus::NodesNotReady
        });
        assert!(snapshot.profile.is_none());
        assert!(snapshot.last_error.is_none(), "{:?}", snapshot.last_error);
        assert!(client.nodes().is_empty());

        // No user action: the schedule fetches it again until the nodes arrive.
        let snapshot = wait_for(&client, |s| s.profile_status == ProfileStatus::Ready);
        assert_eq!(snapshot.profile.unwrap().node_count, 3);
        assert!(snapshot.last_error.is_none(), "{:?}", snapshot.last_error);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(profile_fetches(&paths), 3);
        assert_eq!(client.snapshot().profile_status, ProfileStatus::Ready);
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn nodes_not_ready_retries_stop_on_logout() {
        let (base, paths) = serve(restore_then(vec![nodes_not_ready(); 30]));
        let (client, data_dir) = client(&base, saved_login());

        wait_for(&client, |s| {
            s.profile_status == ProfileStatus::NodesNotReady
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while profile_fetches(&paths) < 2 {
            assert!(Instant::now() < deadline, "no automatic re-fetch");
            std::thread::sleep(Duration::from_millis(5));
        }
        // The revoke takes one of the scripted 404s; the session ends anyway.
        let _ = block_on(client.logout());
        assert!(matches!(client.snapshot().auth, AuthState::SignedOut));
        // The backend answers in order, so every fetch sent before the
        // logout is counted by now.
        let fetched = profile_fetches(&paths);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(profile_fetches(&paths), fetched);
        assert_eq!(client.snapshot().profile_status, ProfileStatus::Loading);
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn manual_refresh_into_nodes_not_ready_starts_the_schedule() {
        let (base, paths) = serve(restore_then([
            ("200 OK", FIXTURE.into()),
            nodes_not_ready(),
            ("200 OK", FIXTURE.into()),
            nodes_not_ready(),
        ]));
        let (client, data_dir) = client(&base, saved_login());
        wait_for(&client, |s| s.profile_status == ProfileStatus::Ready);

        let error = block_on(client.refresh_profile()).unwrap_err();
        assert_eq!(error.info().unwrap().code, ErrorCode::ProxyNodesNotReady);
        let snapshot = client.snapshot();
        assert!(matches!(
            snapshot.profile_status,
            ProfileStatus::NodesNotReady | ProfileStatus::Ready
        ));
        assert!(snapshot.last_error.is_none(), "{:?}", snapshot.last_error);

        // Long before the 5-minute refresh.
        wait_for(&client, |s| s.profile_status == ProfileStatus::Ready);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(profile_fetches(&paths), 3);
        assert_eq!(client.snapshot().profile_status, ProfileStatus::Ready);
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }
}
