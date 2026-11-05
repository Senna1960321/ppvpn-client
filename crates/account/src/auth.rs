//! Browser device login (RFC 8628 style) and credential persistence.
//!
//! The browser receives only a short user code; the high-entropy device code
//! stays in memory and refresh credentials live in the host's credential
//! store ([`CredentialStore`]) as one opaque blob ([`CredentialBlob`]).
//!
//! Every step that writes a credential is ordered so a crash or a failed write
//! leaves something [`Auth::restore`] can finish or safely discard:
//! activation saves the pending credential before calling the backend, and a
//! refresh saves the prepared credential before committing it. A cancelled
//! login (generation changed) revokes whatever the backend already issued.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::{self, Api, ApiError, UserInfo};

/// Where the backend's browser authorization page may live: on the
/// configured API base (its scheme, host and port), the host the backend
/// returns `verification_uri` on (#517). No other host is trusted, and
/// none is built in.
#[derive(Clone, Debug)]
pub struct AuthConfig {
    /// The page's accepted paths; the URL must carry exactly one
    /// `user_code` query.
    pub verification_paths: Vec<String>,
}

/// The backend's browser authorization page, for every client. Only the
/// device page: the old `/dashboard/cli/authorize` is refused.
pub const VERIFICATION_PATHS: &[&str] = &["/dashboard/device/authorize"];

impl AuthConfig {
    /// The device page ([`VERIFICATION_PATHS`]) on the configured API base.
    pub fn production() -> Self {
        Self {
            verification_paths: VERIFICATION_PATHS.iter().map(|p| p.to_string()).collect(),
        }
    }
}

/// The host's secret store for the credential blob (Keychain, Credential
/// Manager, Secret Service, ...). Called from any thread; may block briefly.
pub trait CredentialStore: Send + Sync {
    fn load(&self) -> Result<Option<Vec<u8>>, StoreFailure>;
    fn save(&self, blob: Vec<u8>) -> Result<(), StoreFailure>;
    fn delete(&self) -> Result<(), StoreFailure>;
}

/// A failed [`CredentialStore`] call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreFailure {
    /// The store is locked (e.g. a keyring that needs unlocking); the user
    /// can fix it, so hosts usually say so instead of "failed".
    pub locked: bool,
    pub message: String,
}

impl std::fmt::Display for StoreFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
/// RFC 8628 `slow_down` increment.
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
/// Used when the backend omits both `expires_in` and the JWT `exp` claim.
const DEFAULT_ACCESS_TTL: Duration = Duration::from_secs(10 * 60);
const MIN_DEVICE_CODE_LEN: usize = 32;
const MIN_REFRESH_TOKEN_LEN: usize = 40;
/// Real refresh tokens are ~52 bytes. The cap keeps the three-slot blob well
/// under the smallest platform store limit (Windows Credential Manager, 2560).
const MAX_REFRESH_TOKEN_LEN: usize = 512;

// ---------------------------------------------------------------------------
// Credential blob
// ---------------------------------------------------------------------------

/// Everything persisted through [`CredentialStore::save`], as JSON. An
/// all-empty blob is never saved; the store entry is deleted instead.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct CredentialBlob {
    #[serde(default = "CredentialBlob::current_version")]
    pub version: u32,
    /// Refresh credential of the active device session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_refresh: Option<String>,
    /// Refresh credential returned by a successful poll, saved before
    /// `/auth/device/activate` so an interrupted activation can resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_activation: Option<String>,
    /// Prepared credential from `/auth/device/refresh`, saved before
    /// `/auth/device/refresh/commit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_refresh: Option<String>,
}

// Manual so a fresh blob carries the current version: the serde default only
// applies when deserialising.
impl Default for CredentialBlob {
    fn default() -> Self {
        Self {
            version: Self::current_version(),
            active_refresh: None,
            pending_activation: None,
            pending_refresh: None,
        }
    }
}

impl CredentialBlob {
    fn current_version() -> u32 {
        1
    }

    fn is_empty(&self) -> bool {
        self.active_refresh.is_none()
            && self.pending_activation.is_none()
            && self.pending_refresh.is_none()
    }

    fn slot(&mut self, slot: Slot) -> &mut Option<String> {
        match slot {
            Slot::Active => &mut self.active_refresh,
            Slot::PendingActivation => &mut self.pending_activation,
            Slot::PendingRefresh => &mut self.pending_refresh,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Active,
    PendingActivation,
    PendingRefresh,
}

/// Write-through cache over the credential store. The blob is read once per
/// process, so the OS prompts at most once.
pub struct Credentials {
    platform: Arc<dyn CredentialStore>,
    cache: Mutex<Option<CredentialBlob>>,
}

impl Credentials {
    pub fn new(platform: Arc<dyn CredentialStore>) -> Self {
        Self {
            platform,
            cache: Mutex::new(None),
        }
    }

    fn with<T>(
        &self,
        f: impl FnOnce(&mut CredentialBlob, &dyn CredentialStore) -> Result<T, StoreError>,
    ) -> Result<T, AuthError> {
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if cache.is_none() {
            let loaded = match self.platform.load() {
                // A corrupt blob is treated as signed out rather than blocking
                // launch, and removed so it is not re-read on every start. A
                // blob from a newer app version is left alone.
                Ok(Some(raw)) => match serde_json::from_slice::<CredentialBlob>(&raw) {
                    Ok(mut blob) => {
                        // Builds before the manual `Default` saved version 0
                        // for what is the version-1 layout.
                        if blob.version == 0 {
                            blob.version = CredentialBlob::current_version();
                        }
                        if blob.is_empty() && blob.version <= CredentialBlob::current_version() {
                            tracing::warn!("saved credential has no usable slot; removing it");
                            Self::discard(self.platform.as_ref());
                        }
                        blob
                    }
                    Err(error) => {
                        tracing::warn!("saved credential is unreadable ({error}); removing it");
                        Self::discard(self.platform.as_ref());
                        CredentialBlob::default()
                    }
                },
                Ok(None) => CredentialBlob::default(),
                Err(error) => return Err(AuthError::Store(StoreError::new("load", error))),
            };
            *cache = Some(loaded);
        }
        let blob = cache.get_or_insert_with(CredentialBlob::default);
        f(blob, self.platform.as_ref()).map_err(AuthError::Store)
    }

    fn discard(platform: &dyn CredentialStore) {
        if let Err(error) = platform.delete() {
            tracing::warn!("could not remove the unusable credential: {error}");
        }
    }

    fn persist(platform: &dyn CredentialStore, next: &CredentialBlob) -> Result<(), StoreError> {
        if next.is_empty() {
            return platform
                .delete()
                .map_err(|error| StoreError::new("delete", error));
        }
        let raw = serde_json::to_vec(next).map_err(|error| StoreError {
            detail: format!("encode: {error}"),
            locked: false,
        })?;
        platform
            .save(raw)
            .map_err(|error| StoreError::new("save", error))
    }

    pub fn get(&self, slot: Slot) -> Result<Option<String>, AuthError> {
        self.with(|blob, _| Ok(blob.slot(slot).clone()))
    }

    pub fn set(&self, slot: Slot, value: &str) -> Result<(), AuthError> {
        if value.len() > MAX_REFRESH_TOKEN_LEN {
            return Err(AuthError::Untrusted("credential exceeds the storable size"));
        }
        self.with(|blob, platform| {
            let mut next = blob.clone();
            *next.slot(slot) = Some(value.to_string());
            Self::persist(platform, &next)?;
            *blob = next;
            Ok(())
        })
    }

    pub fn delete(&self, slot: Slot) -> Result<(), AuthError> {
        self.with(|blob, platform| {
            if blob.slot(slot).is_none() {
                return Ok(());
            }
            let mut next = blob.clone();
            *next.slot(slot) = None;
            Self::persist(platform, &next)?;
            *blob = next;
            Ok(())
        })
    }

    /// Delete `slot` only while it still holds `expected`.
    fn delete_if(&self, slot: Slot, expected: &str) -> Result<(), AuthError> {
        if self.get(slot)?.as_deref() == Some(expected) {
            self.delete(slot)?;
        }
        Ok(())
    }

    /// Remove every slot and the platform entry.
    pub fn clear(&self) -> Result<(), AuthError> {
        self.with(|blob, platform| {
            platform
                .delete()
                .map_err(|error| StoreError::new("delete", error))?;
            *blob = CredentialBlob::default();
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// Errors and results
// ---------------------------------------------------------------------------

/// A failed credential-store operation.
#[derive(Debug)]
pub struct StoreError {
    /// `load: ...`, `save: ...`, `delete: ...` or `encode: ...`, for logs.
    pub detail: String,
    /// The store reported itself locked ([`StoreFailure::locked`]).
    pub locked: bool,
}

impl StoreError {
    fn new(operation: &str, error: StoreFailure) -> Self {
        Self {
            locked: error.locked,
            detail: format!("{operation}: {error}"),
        }
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

#[derive(Debug)]
pub enum AuthError {
    Api(ApiError),
    /// The platform credential store failed (or is locked).
    Store(StoreError),
    /// The login was cancelled or superseded (generation changed).
    Cancelled,
    /// The user rejected the authorization in the browser.
    Denied,
    /// The device code expired before the user confirmed it.
    Expired,
    /// The backend answered with something we refuse to trust.
    Untrusted(&'static str),
    /// No saved login to refresh.
    NoCredential,
}

impl AuthError {
    /// The saved login can never work again; it has been (or must be) cleared.
    pub fn is_terminal(&self) -> bool {
        match self {
            Self::Api(error) => error.is_terminal_credential(),
            Self::NoCredential => true,
            _ => false,
        }
    }
}

impl From<ApiError> for AuthError {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

/// A short-lived bearer token.
#[derive(Clone, Debug)]
pub struct AccessToken {
    pub token: String,
    pub expires_at: Instant,
}

impl AccessToken {
    /// Still usable for at least `margin`.
    pub fn is_fresh(&self, margin: Duration) -> bool {
        Instant::now() + margin < self.expires_at
    }
}

/// A completed sign-in: fresh token plus the account it belongs to.
#[derive(Clone, Debug)]
pub struct SignedIn {
    pub access: AccessToken,
    pub account: Option<UserInfo>,
}

/// Returned by [`Auth::start`] for the UI and the poller.
#[derive(Clone, Debug)]
pub struct Started {
    pub generation: u64,
    pub user_code: String,
    pub verification_url: String,
    pub expires_in: Duration,
}

// ---------------------------------------------------------------------------
// Wire models
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri_complete: String,
    expires_in: u64,
    interval: u64,
}

#[derive(Deserialize)]
struct DeviceTokenResponse {
    status: String,
    refresh_token: Option<String>,
}

#[derive(Deserialize)]
struct PendingRefreshResponse {
    refresh_token: String,
}

#[derive(Deserialize)]
struct ActiveTokenResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

// ---------------------------------------------------------------------------
// Coordinator
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FlowState {
    generation: u64,
    pending: Option<PendingAuthorization>,
}

#[derive(Clone)]
struct PendingAuthorization {
    device_code: String,
    expires_at: Instant,
    interval: Duration,
}

/// Device-login coordinator: one per signed-in host.
pub struct Auth {
    api: Arc<Api>,
    config: AuthConfig,
    credentials: Credentials,
    flow: Mutex<FlowState>,
    /// Serialises everything that rotates or clears the saved credential
    /// (activation, refresh, logout, restore).
    credential_lock: tokio::sync::Mutex<()>,
}

/// Remote cleanup after [`Auth::begin_logout`] attempts to remove credentials.
/// Owns only the old session's token; it never writes to the credential store.
pub struct PendingLogout {
    api: Arc<Api>,
    refresh: Option<String>,
    cleared: Result<(), AuthError>,
}

impl PendingLogout {
    /// Revoke the old token best effort, then return the saved local-clear result.
    pub async fn revoke(self) -> Result<(), AuthError> {
        if let Some(refresh) = self.refresh {
            if let Err(error) = self
                .api
                .post_public::<Value>(
                    api::PATH_DEVICE_REVOKE,
                    &json!({ "refresh_token": refresh }),
                )
                .await
            {
                tracing::warn!("device session revoke failed: {error:?}");
            }
        }
        self.cleared
    }
}

impl Auth {
    pub fn new(api: Arc<Api>, store: Arc<dyn CredentialStore>, config: AuthConfig) -> Self {
        Self {
            api,
            config,
            credentials: Credentials::new(store),
            flow: Mutex::new(FlowState::default()),
            credential_lock: tokio::sync::Mutex::new(()),
        }
    }

    pub fn api(&self) -> &Api {
        &self.api
    }

    /// [`access_token_from`] for this client's token audience (e.g. the
    /// token a team switch returns).
    pub fn access_token_from(
        &self,
        token: String,
        expires_in: Option<u64>,
    ) -> Result<AccessToken, AuthError> {
        access_token_from(token, expires_in, self.api.token_audience())
    }

    /// Held across credential rotation so a refresh, an activation and a
    /// logout never interleave their writes.
    pub async fn lock(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.credential_lock.lock().await
    }

    fn flow(&self) -> std::sync::MutexGuard<'_, FlowState> {
        self.flow
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The current login generation; bumped by every `start` and `cancel`.
    pub fn generation(&self) -> u64 {
        self.flow().generation
    }

    pub fn generation_is_current(&self, generation: u64) -> bool {
        self.flow().generation == generation
    }

    fn expect_generation(&self, expected: Option<u64>) -> bool {
        expected.is_none_or(|generation| self.generation_is_current(generation))
    }

    /// Invalidate any running login; its poller exits and a late activation
    /// is revoked.
    pub fn cancel(&self) {
        let mut flow = self.flow();
        flow.generation = flow.generation.wrapping_add(1);
        flow.pending = None;
    }

    /// Request a device code and remember it for [`Auth::poll_until_done`].
    /// `device_name` is shown in the user's device list; `platform` (e.g.
    /// `macos`) gets the CPU architecture appended.
    pub async fn start(
        &self,
        device_name: &str,
        platform: &str,
        app_version: &str,
    ) -> Result<Started, AuthError> {
        let generation = {
            let mut flow = self.flow();
            flow.generation = flow.generation.wrapping_add(1);
            flow.pending = None;
            flow.generation
        };
        let created: DeviceCodeResponse = self
            .api
            .post_public(
                api::PATH_DEVICE_CODE,
                &json!({
                    "device_name": device_name,
                    "platform": format!("{platform}/{}", std::env::consts::ARCH),
                    "cli_version": app_version,
                }),
            )
            .await?;
        // A local mock backend (test switch) may issue short codes.
        let min_device_code = if self.api.trusts_local_backend() {
            1
        } else {
            MIN_DEVICE_CODE_LEN
        };
        if created.device_code.len() < min_device_code
            || created.expires_in == 0
            || created.interval == 0
        {
            return Err(AuthError::Untrusted("invalid device transaction"));
        }
        let verification_url = validate_verification_url(
            &self.config,
            &created.verification_uri_complete,
            &created.user_code,
            self.api.base(),
            self.api.trusts_local_backend(),
        )?;
        let now = Instant::now();
        let mut flow = self.flow();
        if flow.generation != generation {
            return Err(AuthError::Cancelled);
        }
        flow.pending = Some(PendingAuthorization {
            device_code: created.device_code,
            expires_at: now + Duration::from_secs(created.expires_in),
            interval: Duration::from_secs(created.interval),
        });
        Ok(Started {
            generation,
            user_code: created.user_code,
            verification_url: verification_url.to_string(),
            expires_in: Duration::from_secs(created.expires_in),
        })
    }

    /// Poll `/auth/device/token` every `interval` (growing on `slow_down` or
    /// 429) until the user decides, the code expires or `generation` is
    /// cancelled. Returns the pending credential to pass to
    /// [`Auth::activate`]. Transient failures are retried.
    pub async fn poll_until_done(&self, generation: u64) -> Result<String, AuthError> {
        loop {
            let pending = {
                let flow = self.flow();
                match flow.pending.clone() {
                    Some(pending) if flow.generation == generation => pending,
                    _ => return Err(AuthError::Cancelled),
                }
            };
            let now = Instant::now();
            if now >= pending.expires_at {
                self.finish(generation);
                return Err(AuthError::Expired);
            }
            tokio::time::sleep(pending.interval.min(pending.expires_at - now)).await;
            if !self.generation_is_current(generation) {
                return Err(AuthError::Cancelled);
            }
            if Instant::now() >= pending.expires_at {
                self.finish(generation);
                return Err(AuthError::Expired);
            }

            let response: Result<DeviceTokenResponse, ApiError> = self
                .api
                .post_public(
                    api::PATH_DEVICE_TOKEN,
                    &json!({ "device_code": pending.device_code }),
                )
                .await;
            if !self.generation_is_current(generation) {
                return Err(AuthError::Cancelled);
            }
            let response = match response {
                Ok(response) => response,
                Err(error) if error.is_rate_limited() => {
                    self.slow_down(generation);
                    continue;
                }
                Err(error) if error.is_transient() => {
                    tracing::warn!("device login poll failed, retrying: {error:?}");
                    continue;
                }
                Err(error) => {
                    self.finish(generation);
                    return Err(error.into());
                }
            };
            match response.status.as_str() {
                "authorization_pending" => {}
                "slow_down" => self.slow_down(generation),
                "access_denied" => {
                    self.finish(generation);
                    return Err(AuthError::Denied);
                }
                "expired_token" => {
                    self.finish(generation);
                    return Err(AuthError::Expired);
                }
                "authorized" => {
                    self.finish(generation);
                    return response
                        .refresh_token
                        .filter(|value| value.len() >= MIN_REFRESH_TOKEN_LEN)
                        .ok_or(AuthError::Untrusted("authorization omitted the credential"));
                }
                _ => {
                    self.finish(generation);
                    return Err(AuthError::Untrusted("unknown device authorization status"));
                }
            }
        }
    }

    fn slow_down(&self, generation: u64) {
        let mut flow = self.flow();
        if flow.generation != generation {
            return;
        }
        if let Some(pending) = flow.pending.as_mut() {
            pending.interval += SLOW_DOWN_STEP;
        }
    }

    fn finish(&self, generation: u64) {
        let mut flow = self.flow();
        if flow.generation == generation {
            flow.pending = None;
        }
    }

    async fn cleanup_cancelled_activation(
        &self,
        pending: &str,
        active: Option<&str>,
    ) -> Result<(), AuthError> {
        if let Some(refresh) = active {
            let _ = self.revoke(refresh).await;
            self.credentials.delete_if(Slot::Active, refresh)?;
        }
        self.credentials.delete_if(Slot::PendingActivation, pending)
    }

    async fn revoke(&self, refresh: &str) -> Result<(), ApiError> {
        self.api
            .post_public::<Value>(
                api::PATH_DEVICE_REVOKE,
                &json!({ "refresh_token": refresh }),
            )
            .await
            .map(|_| ())
    }

    /// Exchange a pending credential for an active session. Hold
    /// [`Auth::lock`] while calling.
    pub async fn activate(
        &self,
        pending: String,
        expected_generation: Option<u64>,
    ) -> Result<SignedIn, AuthError> {
        self.credentials.set(Slot::PendingActivation, &pending)?;
        if !self.expect_generation(expected_generation) {
            self.cleanup_cancelled_activation(&pending, None).await?;
            return Err(AuthError::Cancelled);
        }
        let active = self.credentials.get(Slot::Active)?;
        let mut body = json!({ "refresh_token": pending });
        if let Some(previous) = active.as_deref().filter(|value| *value != pending) {
            body["replaced_refresh_token"] = Value::String(previous.to_string());
        }
        let tokens: ActiveTokenResponse = self
            .api
            .post_public(api::PATH_DEVICE_ACTIVATE, &body)
            .await?;
        if !self.expect_generation(expected_generation) {
            self.cleanup_cancelled_activation(&pending, Some(&tokens.refresh_token))
                .await?;
            return Err(AuthError::Cancelled);
        }
        let access = match access_token(&tokens, self.api.token_audience()) {
            Ok(access) => access,
            Err(error) => {
                self.cleanup_cancelled_activation(&pending, Some(&tokens.refresh_token))
                    .await?;
                return Err(error);
            }
        };
        self.credentials.set(Slot::Active, &tokens.refresh_token)?;
        if !self.expect_generation(expected_generation) {
            self.cleanup_cancelled_activation(&pending, Some(&tokens.refresh_token))
                .await?;
            return Err(AuthError::Cancelled);
        }
        self.credentials.delete(Slot::PendingActivation)?;
        let account = self.api.account(&access.token).await?;
        if !self.expect_generation(expected_generation) {
            self.cleanup_cancelled_activation(&pending, Some(&tokens.refresh_token))
                .await?;
            return Err(AuthError::Cancelled);
        }
        Ok(SignedIn {
            access,
            account: Some(account),
        })
    }

    /// Rotate the active refresh credential and mint a new access token.
    /// Hold [`Auth::lock`] while calling.
    pub async fn refresh(&self) -> Result<AccessToken, AuthError> {
        let current = self
            .credentials
            .get(Slot::Active)?
            .ok_or(AuthError::NoCredential)?;
        let prepared = match self.credentials.get(Slot::PendingRefresh)? {
            Some(value) => value,
            None => {
                let response: PendingRefreshResponse = self
                    .api
                    .post_public(
                        api::PATH_DEVICE_REFRESH,
                        &json!({ "refresh_token": current }),
                    )
                    .await?;
                self.credentials
                    .set(Slot::PendingRefresh, &response.refresh_token)?;
                response.refresh_token
            }
        };
        let tokens: ActiveTokenResponse = self
            .api
            .post_public(
                api::PATH_DEVICE_REFRESH_COMMIT,
                &json!({ "prepared_refresh_token": prepared }),
            )
            .await?;
        let access = access_token(&tokens, self.api.token_audience())?;
        self.credentials.set(Slot::Active, &tokens.refresh_token)?;
        self.credentials.delete(Slot::PendingRefresh)?;
        Ok(access)
    }

    /// [`Auth::refresh`], clearing every saved credential when the backend
    /// rejects it for good.
    pub async fn refresh_or_clear(&self) -> Result<AccessToken, AuthError> {
        match self.refresh().await {
            Err(error) if error.is_terminal() => {
                self.credentials.clear()?;
                Err(error)
            }
            other => other,
        }
    }

    /// Resume whatever the saved credentials allow: finish an interrupted
    /// activation, or refresh the active session. `Ok(None)` means signed
    /// out. Terminal rejections clear the credentials and are returned as
    /// errors; transient ones keep them.
    pub async fn restore(&self) -> Result<Option<SignedIn>, AuthError> {
        if let Some(pending) = self.credentials.get(Slot::PendingActivation)? {
            let active = self.credentials.get(Slot::Active)?;
            if active.as_deref() == Some(pending.as_str()) {
                self.credentials.delete(Slot::PendingActivation)?;
            } else {
                return match self.activate(pending, None).await {
                    Ok(signed_in) => Ok(Some(signed_in)),
                    Err(error) if error.is_terminal() => {
                        self.credentials.clear()?;
                        Err(error)
                    }
                    Err(error) => Err(error),
                };
            }
        }
        if self.credentials.get(Slot::Active)?.is_none() {
            return Ok(None);
        }
        let access = self.refresh_or_clear().await?;
        Ok(Some(SignedIn {
            access,
            account: None,
        }))
    }

    /// Cancel login and clear saved credentials before any network cleanup.
    /// Like rotation, the caller must hold [`Self::lock`] across this step.
    pub fn begin_logout(&self) -> PendingLogout {
        self.cancel();
        let active = self.credentials.get(Slot::Active);
        let cleared = self.credentials.clear();
        PendingLogout {
            api: self.api.clone(),
            refresh: active.ok().flatten(),
            cleared,
        }
    }

    /// Local logout is authoritative; remote revocation is best effort.
    pub async fn logout(&self) -> Result<(), AuthError> {
        self.begin_logout().revoke().await
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// `trust_local`: the test switch applies to the (loopback) configured base,
/// so its own `http` authorization page is trusted.
fn validate_verification_url(
    config: &AuthConfig,
    raw: &str,
    expected_user_code: &str,
    configured_api_base: &str,
    trust_local: bool,
) -> Result<Url, AuthError> {
    let untrusted = AuthError::Untrusted("untrusted verification URL");
    let Ok(url) = Url::parse(raw) else {
        return Err(untrusted);
    };
    // The configured backend is trusted for everything else already, so its
    // own authorization page is too, and only it. Outside debug builds it
    // must be HTTPS.
    let configured = Url::parse(configured_api_base.trim())
        .ok()
        .is_some_and(|base| {
            (url.scheme() == "https" || cfg!(debug_assertions) || trust_local)
                && url.scheme() == base.scheme()
                && url.host_str() == base.host_str()
                && url.port_or_known_default() == base.port_or_known_default()
        });
    let query = url.query_pairs().collect::<Vec<_>>();
    if !configured
        || !config
            .verification_paths
            .iter()
            .any(|path| url.path() == path)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || query.len() != 1
        || query[0].0 != "user_code"
        || query[0].1 != expected_user_code
    {
        return Err(untrusted);
    }
    Ok(url)
}

/// Decoded claims of an access token. Returns the `exp` claim (unix seconds)
/// when present. Rejects tokens not scoped to exactly `audience`.
pub fn validate_access_token(token: &str, audience: &str) -> Result<Option<u64>, AuthError> {
    let invalid = AuthError::Untrusted("invalid access token");
    let mut segments = token.split('.');
    let (Some(_header), Some(payload), Some(_signature), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(invalid);
    };
    let Ok(decoded) = URL_SAFE_NO_PAD.decode(payload) else {
        return Err(invalid);
    };
    let Ok(claims) = serde_json::from_slice::<Value>(&decoded) else {
        return Err(invalid);
    };
    let audience_matches = match claims.get("aud") {
        Some(Value::String(claimed)) => claimed == audience,
        Some(Value::Array(claimed)) => claimed.len() == 1 && claimed[0].as_str() == Some(audience),
        _ => false,
    };
    if !audience_matches {
        return Err(AuthError::Untrusted("restricted session"));
    }
    Ok(claims.get("exp").and_then(Value::as_u64))
}

/// Validate a bearer token and work out when to refresh it.
pub fn access_token_from(
    token: String,
    expires_in: Option<u64>,
    audience: &str,
) -> Result<AccessToken, AuthError> {
    let exp = validate_access_token(&token, audience)?;
    let ttl = expires_in
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
        .or_else(|| {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
            exp.map(|exp| Duration::from_secs(exp.saturating_sub(now)))
        })
        .unwrap_or(DEFAULT_ACCESS_TTL);
    Ok(AccessToken {
        token,
        expires_at: Instant::now() + ttl,
    })
}

fn access_token(tokens: &ActiveTokenResponse, audience: &str) -> Result<AccessToken, AuthError> {
    access_token_from(tokens.access_token.clone(), tokens.expires_in, audience)
}

// ---------------------------------------------------------------------------
// Test support
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_support {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    use super::{CredentialStore, StoreFailure};

    /// In-memory credential store; `fail_save_at(n)` fails the n-th save
    /// (0-based, counted from the call).
    #[derive(Default)]
    pub(crate) struct MemoryStore {
        pub(crate) blob: Mutex<Option<Vec<u8>>>,
        saves: Mutex<usize>,
        fail_save: Mutex<Option<usize>>,
    }

    impl MemoryStore {
        pub(crate) fn with_blob(json: &str) -> Arc<Self> {
            let store = Self::default();
            *store.blob.lock().unwrap() = Some(json.as_bytes().to_vec());
            Arc::new(store)
        }

        pub(crate) fn fail_save_at(&self, n: usize) {
            *self.fail_save.lock().unwrap() = Some(*self.saves.lock().unwrap() + n);
        }

        pub(crate) fn stored(&self) -> Option<serde_json::Value> {
            let blob = self.blob.lock().unwrap();
            blob.as_ref()
                .map(|raw| serde_json::from_slice(raw).unwrap())
        }
    }

    impl CredentialStore for MemoryStore {
        fn load(&self) -> Result<Option<Vec<u8>>, StoreFailure> {
            Ok(self.blob.lock().unwrap().clone())
        }
        fn save(&self, blob: Vec<u8>) -> Result<(), StoreFailure> {
            let mut saves = self.saves.lock().unwrap();
            let index = *saves;
            *saves += 1;
            if *self.fail_save.lock().unwrap() == Some(index) {
                return Err(StoreFailure {
                    locked: false,
                    message: "injected".into(),
                });
            }
            *self.blob.lock().unwrap() = Some(blob);
            Ok(())
        }
        fn delete(&self) -> Result<(), StoreFailure> {
            *self.blob.lock().unwrap() = None;
            Ok(())
        }
    }

    /// Serves `responses` (status line, JSON body) to successive connections
    /// on a background thread; returns the base URL and the captured
    /// `METHOD /path` lines.
    pub(crate) fn serve(
        responses: Vec<(&'static str, String)>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        serve_with(move |_| responses)
    }

    /// [`serve`] with responses that may embed the server's own base URL.
    pub(crate) fn serve_with(
        responses: impl FnOnce(&str) -> Vec<(&'static str, String)>,
    ) -> (String, Arc<Mutex<Vec<String>>>) {
        let (base, seen, _) = serve_recording(responses);
        (base, seen)
    }

    /// What the test server saw, shared with its thread.
    pub(crate) type Captured = Arc<Mutex<Vec<String>>>;

    /// [`serve_with`] that also returns each request's header block,
    /// lowercased.
    pub(crate) fn serve_recording(
        responses: impl FnOnce(&str) -> Vec<(&'static str, String)>,
    ) -> (String, Captured, Captured) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let responses = responses(&base);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let captured = seen.clone();
        let heads = Arc::new(Mutex::new(Vec::new()));
        let captured_heads = heads.clone();
        std::thread::spawn(move || {
            let mut responses = responses.into_iter();
            loop {
                let Ok((mut socket, _)) = listener.accept() else {
                    return;
                };
                let mut request = Vec::new();
                let mut chunk = [0_u8; 4096];
                loop {
                    let Ok(read) = socket.read(&mut chunk) else {
                        return;
                    };
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                    let length = headers
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
                let text = String::from_utf8_lossy(&request);
                let line = text.lines().next().unwrap_or_default().to_string();
                let mut parts = line.split(' ');
                let method = parts.next().unwrap_or_default().to_string();
                let path = parts.next().unwrap_or_default().to_string();
                if method.is_empty() {
                    continue;
                }
                let Some((status, body)) = responses.next() else {
                    return;
                };
                captured.lock().unwrap().push(format!("{method} {path}"));
                let head = text.split("\r\n\r\n").next().unwrap_or_default();
                captured_heads
                    .lock()
                    .unwrap()
                    .push(head.to_ascii_lowercase());
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes());
            }
        });
        (base, seen, heads)
    }

    /// A `/auth/device/activate` or `/refresh/commit` body for audience `app`.
    pub(crate) fn token_set(refresh: &str) -> String {
        format!(
            r#"{{"access_token":"{}","refresh_token":"{refresh}","expires_in":900}}"#,
            jwt(r#"{"aud":["app"]}"#)
        )
    }

    /// `header.payload.sig` with the given JSON claims.
    pub(crate) fn jwt(claims: &str) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        format!("e30.{}.sig", URL_SAFE_NO_PAD.encode(claims))
    }
}

#[cfg(test)]
mod tests {

    use super::test_support::{jwt, serve, serve_with, token_set, MemoryStore};
    use super::*;

    fn config() -> AuthConfig {
        AuthConfig {
            verification_paths: vec!["/device/authorize".into(), "/old/authorize".into()],
        }
    }

    fn api_with(base: &str, trust_local_backend: bool) -> Api {
        Api::new(api::ApiConfig {
            base: base.into(),
            header_audience: Some("app".into()),
            token_audience: "app".into(),
            accept_language: None,
            trust_local_backend,
        })
    }

    fn auth(base: &str, store: Arc<MemoryStore>) -> Auth {
        Auth::new(Arc::new(api_with(base, false)), store, config())
    }

    fn short_code_backend() -> String {
        serve_with(|base| {
            vec![(
                "200 OK",
                format!(
                    r#"{{"device_code":"short","user_code":"ABCD-EFGH","verification_uri_complete":"{base}/device/authorize?user_code=ABCD-EFGH","expires_in":600,"interval":5}}"#
                ),
            )]
        })
        .0
    }

    #[tokio::test]
    async fn test_switch_on_a_loopback_backend_relaxes_the_device_code() {
        let base = short_code_backend();
        let api = api_with(&base, true);
        assert!(api.trusts_local_backend());
        let auth = Auth::new(Arc::new(api), Arc::new(MemoryStore::default()), config());
        let started = auth
            .start("Test Device", "macos", "0.0.0-test")
            .await
            .unwrap();
        assert!(started.verification_url.starts_with("http://127.0.0.1"));
    }

    #[tokio::test]
    async fn without_the_switch_short_device_codes_are_refused() {
        let base = short_code_backend();
        let api = api_with(&base, false);
        assert!(!api.trusts_local_backend());
        let auth = Auth::new(Arc::new(api), Arc::new(MemoryStore::default()), config());
        assert!(matches!(
            auth.start("Test Device", "macos", "0.0.0-test").await,
            Err(AuthError::Untrusted("invalid device transaction"))
        ));
    }

    #[test]
    fn the_switch_never_relaxes_a_remote_backend() {
        let api = api_with("http://staging.example.com", true);
        assert!(!api.trusts_local_backend());
        assert!(api.endpoint("/api/v1/users/me").is_err(), "no plain http");
        assert!(!api_with("https://www.example.com", true).trusts_local_backend());
        assert!(!api_with("http://192.0.2.10", true).trusts_local_backend());
        // The loopback verification page is trusted only with the switch
        // (debug builds aside) and only for the loopback base itself.
        let url = "http://127.0.0.1:9/device/authorize?user_code=ABCD-EFGH";
        assert!(
            validate_verification_url(&config(), url, "ABCD-EFGH", "http://127.0.0.1:9", true)
                .is_ok()
        );
        assert!(validate_verification_url(
            &config(),
            url,
            "ABCD-EFGH",
            "http://staging.example.com",
            true
        )
        .is_err());
        // Under the switch plain http works in release builds too.
        assert!(api_with("http://127.0.0.1:9", true)
            .endpoint("/api/v1/users/me")
            .is_ok());
    }

    #[test]
    fn blob_round_trips_and_omits_empty_slots() {
        let blob = CredentialBlob {
            version: 1,
            active_refresh: Some("a".into()),
            pending_activation: None,
            pending_refresh: Some("p".into()),
        };
        let json = serde_json::to_string(&blob).unwrap();
        assert_eq!(
            json,
            r#"{"version":1,"active_refresh":"a","pending_refresh":"p"}"#
        );
        assert_eq!(serde_json::from_str::<CredentialBlob>(&json).unwrap(), blob);
    }

    #[test]
    fn unusable_blobs_are_removed_but_newer_ones_kept() {
        for (raw, removed) in [
            ("not json", true),
            (r#"{"corrupt":true}"#, true),
            (r#"{"version":1}"#, true),
            (r#"{"version":9,"future_slot":"x"}"#, false),
        ] {
            let platform = MemoryStore::with_blob(raw);
            let credentials = Credentials::new(platform.clone());
            assert_eq!(credentials.get(Slot::Active).unwrap(), None, "{raw}");
            assert_eq!(platform.stored().is_none(), removed, "{raw}");
        }
    }

    #[test]
    fn full_blob_fits_the_smallest_platform_store() {
        let platform = Arc::new(MemoryStore::default());
        let credentials = Credentials::new(platform.clone());
        let longest = "x".repeat(MAX_REFRESH_TOKEN_LEN);
        for slot in [Slot::Active, Slot::PendingActivation, Slot::PendingRefresh] {
            credentials.set(slot, &longest).unwrap();
        }
        assert!(platform.stored().unwrap().to_string().len() < 2560);
        assert!(matches!(
            credentials.set(Slot::Active, &"x".repeat(MAX_REFRESH_TOKEN_LEN + 1)),
            Err(AuthError::Untrusted(_))
        ));
    }

    #[test]
    fn new_blob_is_saved_with_the_current_version() {
        assert_eq!(CredentialBlob::default().version, 1);
        let platform = Arc::new(MemoryStore::default());
        let credentials = Credentials::new(platform.clone());
        credentials.set(Slot::Active, "rt_new").unwrap();
        assert_eq!(platform.stored().unwrap()["version"], 1);

        // A blob written with the old version 0 is upgraded on the next save.
        let platform = MemoryStore::with_blob(r#"{"version":0,"active_refresh":"a"}"#);
        let credentials = Credentials::new(platform.clone());
        credentials.set(Slot::PendingRefresh, "p").unwrap();
        assert_eq!(platform.stored().unwrap()["version"], 1);
        assert_eq!(platform.stored().unwrap()["active_refresh"], "a");
    }

    #[test]
    fn corrupt_blob_reads_as_signed_out() {
        let credentials = Credentials::new(MemoryStore::with_blob("not json"));
        assert_eq!(credentials.get(Slot::Active).unwrap(), None);
    }

    #[test]
    fn the_verification_url_is_exactly_allowlisted() {
        let base = "https://www.example.com";
        let valid = "https://www.example.com/device/authorize?user_code=ABCD-EFGH";
        assert!(validate_verification_url(&config(), valid, "ABCD-EFGH", base, false).is_ok());
        for bad in [
            "https://evil.example/device/authorize?user_code=ABCD-EFGH",
            "https://www.example.com/device/authorize-evil?user_code=ABCD-EFGH",
            "https://www.example.com/device/authorize?user_code=ABCD-EFGH&user_code=ABCD-EFGH",
            "https://u:p@www.example.com/device/authorize?user_code=ABCD-EFGH",
        ] {
            assert!(
                validate_verification_url(&config(), bad, "ABCD-EFGH", base, false).is_err(),
                "{bad}"
            );
        }
        assert!(validate_verification_url(&config(), valid, "ZZZZ-ZZZZ", base, false).is_err());
    }

    #[test]
    fn every_accepted_verification_path_is_trusted() {
        let base = "https://www.example.com";
        for valid in [
            "https://www.example.com/device/authorize?user_code=ABCD-EFGH",
            "https://www.example.com/old/authorize?user_code=ABCD-EFGH",
        ] {
            assert!(
                validate_verification_url(&config(), valid, "ABCD-EFGH", base, false).is_ok(),
                "{valid}"
            );
        }
        let other = "https://www.example.com/dashboard/authorize?user_code=ABCD-EFGH";
        assert!(validate_verification_url(&config(), other, "ABCD-EFGH", base, false).is_err());
    }

    #[test]
    fn only_the_device_page_is_accepted() {
        let config = AuthConfig {
            verification_paths: VERIFICATION_PATHS.iter().map(|p| p.to_string()).collect(),
        };
        let base = "https://www.example.com";
        let device = "https://www.example.com/dashboard/device/authorize?user_code=ABCD-EFGH";
        assert!(validate_verification_url(&config, device, "ABCD-EFGH", base, false).is_ok());
        for bad in [
            "https://www.example.com/dashboard/cli/authorize?user_code=ABCD-EFGH",
            "https://www.example.com/dashboard/authorize?user_code=ABCD-EFGH",
        ] {
            assert!(
                validate_verification_url(&config, bad, "ABCD-EFGH", base, false).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn configured_backend_verification_url_is_trusted() {
        let base = "https://staging.example.com";
        let dev = "https://staging.example.com/device/authorize?user_code=ABCD-EFGH";
        assert!(validate_verification_url(&config(), dev, "ABCD-EFGH", base, false).is_ok());
        // Another host, or a downgrade to plain HTTP on a non-debug path, is not.
        for bad in [
            "https://evil.example.com/device/authorize?user_code=ABCD-EFGH",
            "https://staging.example.com:8443/device/authorize?user_code=ABCD-EFGH",
        ] {
            assert!(
                validate_verification_url(&config(), bad, "ABCD-EFGH", base, false).is_err(),
                "{bad}"
            );
        }
        if !cfg!(debug_assertions) {
            let http = "http://staging.example.com/device/authorize?user_code=ABCD-EFGH";
            assert!(validate_verification_url(
                &config(),
                http,
                "ABCD-EFGH",
                "http://staging.example.com",
                false
            )
            .is_err());
        }
    }

    #[test]
    fn access_token_requires_exactly_the_audience() {
        assert!(validate_access_token(&jwt(r#"{"aud":"app"}"#), "app").is_ok());
        assert!(validate_access_token(&jwt(r#"{"aud":["app"]}"#), "app").is_ok());
        assert!(validate_access_token(&jwt(r#"{"aud":["cli"]}"#), "app").is_err());
        assert!(validate_access_token(&jwt(r#"{"aud":["app","cli"]}"#), "app").is_err());
        assert!(validate_access_token("not-a-jwt", "app").is_err());
        assert_eq!(
            validate_access_token(&jwt(r#"{"aud":"app","exp":42}"#), "app").unwrap(),
            Some(42)
        );
    }

    #[test]
    fn cancel_invalidates_generation() {
        let auth = auth("http://127.0.0.1:9", Arc::default());
        let before = auth.flow().generation;
        auth.cancel();
        assert!(!auth.generation_is_current(before));
    }

    #[tokio::test]
    async fn pending_storage_failure_stops_before_activation_request() {
        let platform = Arc::new(MemoryStore::default());
        platform.fail_save_at(0);
        let auth = auth("http://127.0.0.1:9", platform.clone());
        let error = auth
            .activate(format!("rt_{}", "x".repeat(48)), None)
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::Store(_)));
        assert!(platform.stored().is_none());
    }

    #[tokio::test]
    async fn active_storage_failure_leaves_recoverable_pending_credential() {
        let (base, paths) = serve(vec![("200 OK", token_set("rt_new_refresh"))]);
        let platform = Arc::new(MemoryStore::default());
        platform.fail_save_at(1);
        let auth = auth(&base, platform.clone());
        let pending = format!("rt_{}", "p".repeat(48));
        let error = auth.activate(pending.clone(), None).await.unwrap_err();
        assert!(matches!(error, AuthError::Store(_)));
        let stored = platform.stored().unwrap();
        assert_eq!(stored["pending_activation"], pending.as_str());
        assert!(stored.get("active_refresh").is_none());
        assert_eq!(
            paths.lock().unwrap().as_slice(),
            ["POST /api/v1/auth/device/activate"]
        );
    }

    #[tokio::test]
    async fn restricted_activation_is_revoked_and_never_persisted() {
        let restricted = format!(
            r#"{{"access_token":"{}","refresh_token":"rt_cli"}}"#,
            jwt(r#"{"aud":["cli"]}"#)
        );
        let (base, paths) = serve(vec![
            ("200 OK", restricted),
            ("200 OK", r#"{"revoked":true}"#.into()),
        ]);
        let platform = Arc::new(MemoryStore::default());
        let auth = auth(&base, platform.clone());
        let error = auth
            .activate(format!("rt_{}", "p".repeat(48)), None)
            .await
            .unwrap_err();
        assert!(matches!(error, AuthError::Untrusted("restricted session")));
        assert!(platform.stored().is_none());
        assert_eq!(
            paths.lock().unwrap().as_slice(),
            [
                "POST /api/v1/auth/device/activate",
                "POST /api/v1/auth/device/revoke"
            ]
        );
    }

    #[tokio::test]
    async fn cancel_before_activation_request_only_cleans_up_locally() {
        let (base, paths) = serve(vec![]);
        let platform = Arc::new(MemoryStore::default());
        let auth = auth(&base, platform.clone());
        let generation = auth.flow().generation;
        let task = auth.activate(format!("rt_{}", "p".repeat(48)), Some(generation));
        auth.cancel();
        assert!(matches!(task.await.unwrap_err(), AuthError::Cancelled));
        assert!(platform.stored().is_none());
        assert!(paths.lock().unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancel_during_activation_revokes_late_success() {
        use std::io::{Read, Write};
        use std::sync::mpsc;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (seen_tx, seen_rx) = mpsc::channel::<String>();
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let late = token_set("rt_late_refresh");
        std::thread::spawn(move || {
            let mut buffer = [0_u8; 8192];
            for (index, body) in [late, r#"{"revoked":true}"#.to_string()]
                .into_iter()
                .enumerate()
            {
                let (mut socket, _) = listener.accept().unwrap();
                let read = socket.read(&mut buffer).unwrap();
                let text = String::from_utf8_lossy(&buffer[..read]).to_string();
                seen_tx
                    .send(text.lines().next().unwrap_or_default().to_string())
                    .unwrap();
                if index == 0 {
                    release_rx.recv().unwrap();
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).unwrap();
            }
        });

        let platform = Arc::new(MemoryStore::default());
        let auth = Arc::new(auth(&base, platform.clone()));
        let generation = auth.flow().generation;
        let task = {
            let auth = auth.clone();
            tokio::spawn(async move {
                auth.activate(format!("rt_{}", "p".repeat(48)), Some(generation))
                    .await
            })
        };
        let first = tokio::task::spawn_blocking(move || {
            let first = seen_rx.recv().unwrap();
            (first, seen_rx)
        })
        .await
        .unwrap();
        auth.cancel();
        release_tx.send(()).unwrap();

        assert!(matches!(
            task.await.unwrap().unwrap_err(),
            AuthError::Cancelled
        ));
        assert!(platform.stored().is_none());
        assert_eq!(first.0, "POST /api/v1/auth/device/activate HTTP/1.1");
        assert_eq!(
            first.1.recv().unwrap(),
            "POST /api/v1/auth/device/revoke HTTP/1.1"
        );
    }

    #[tokio::test]
    async fn refresh_persists_prepared_token_before_commit_and_clears_it_afterward() {
        let (base, paths) = serve(vec![
            ("200 OK", r#"{"refresh_token":"rt_prepared"}"#.into()),
            ("200 OK", token_set("rt_active")),
        ]);
        let platform = MemoryStore::with_blob(r#"{"version":1,"active_refresh":"rt_old"}"#);
        let auth = auth(&base, platform.clone());
        let access = auth.refresh().await.unwrap();
        assert!(access.is_fresh(Duration::from_secs(60)));
        assert_eq!(
            platform.stored().unwrap(),
            serde_json::json!({"version": 1, "active_refresh": "rt_active"})
        );
        assert_eq!(
            paths.lock().unwrap().as_slice(),
            [
                "POST /api/v1/auth/device/refresh",
                "POST /api/v1/auth/device/refresh/commit"
            ]
        );
    }

    #[tokio::test]
    async fn interrupted_refresh_resumes_with_prepared_token() {
        let (base, paths) = serve(vec![("200 OK", token_set("rt_active"))]);
        let platform = MemoryStore::with_blob(
            r#"{"version":1,"active_refresh":"rt_old","pending_refresh":"rt_prepared"}"#,
        );
        let auth = auth(&base, platform.clone());
        auth.refresh().await.unwrap();
        assert_eq!(
            paths.lock().unwrap().as_slice(),
            ["POST /api/v1/auth/device/refresh/commit"]
        );
    }

    #[tokio::test]
    async fn terminal_pending_activation_is_cleared_during_restore() {
        let (base, paths) = serve(vec![(
            "401 Unauthorized",
            r#"{"code":"AUTH_DEVICE_CREDENTIAL_INVALID"}"#.into(),
        )]);
        let platform = MemoryStore::with_blob(r#"{"version":1,"pending_activation":"rt_expired"}"#);
        let auth = auth(&base, platform.clone());
        let error = auth.restore().await.unwrap_err();
        assert!(error.is_terminal());
        assert!(matches!(
            error,
            AuthError::Api(ApiError::Status { status, .. }) if status.as_u16() == 401
        ));
        assert!(platform.stored().is_none());
        assert_eq!(
            paths.lock().unwrap().as_slice(),
            ["POST /api/v1/auth/device/activate"]
        );
    }

    #[tokio::test]
    async fn network_failure_during_restore_keeps_credentials() {
        let platform = MemoryStore::with_blob(r#"{"version":1,"active_refresh":"rt_keep"}"#);
        let auth = auth("http://127.0.0.1:9", platform.clone());
        let error = auth.restore().await.unwrap_err();
        assert!(!error.is_terminal());
        assert_eq!(platform.stored().unwrap()["active_refresh"], "rt_keep");
    }

    #[tokio::test]
    async fn interrupted_logout_does_not_restore_credentials_while_revoke_is_pending() {
        use std::io::Read;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (requested, request_seen) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            let read = socket.read(&mut request).unwrap();
            assert!(String::from_utf8_lossy(&request[..read])
                .starts_with("POST /api/v1/auth/device/revoke "));
            let _ = requested.send(());
            let _ = released.recv_timeout(Duration::from_secs(5));
        });
        let platform = MemoryStore::with_blob(
            r#"{"version":1,"active_refresh":"rt_a","pending_refresh":"rt_p"}"#,
        );
        let account = auth(&base, platform.clone());
        let logout = tokio::spawn(async move { account.logout().await });
        tokio::time::timeout(Duration::from_secs(5), request_seen)
            .await
            .unwrap()
            .unwrap();
        let cleared_before_response = platform.stored().is_none();
        logout.abort();
        let _ = logout.await;
        let _ = release.send(());
        server.join().unwrap();
        assert!(
            cleared_before_response,
            "logout must clear credentials before awaiting revocation"
        );
        assert!(auth(&base, platform).restore().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn logout_reports_local_clear_failure() {
        struct CannotDelete(Arc<MemoryStore>);
        impl CredentialStore for CannotDelete {
            fn load(&self) -> Result<Option<Vec<u8>>, StoreFailure> {
                self.0.load()
            }
            fn save(&self, blob: Vec<u8>) -> Result<(), StoreFailure> {
                self.0.save(blob)
            }
            fn delete(&self) -> Result<(), StoreFailure> {
                Err(StoreFailure {
                    locked: true,
                    message: "locked test store".into(),
                })
            }
        }
        let platform = MemoryStore::with_blob(r#"{"version":1,"active_refresh":"rt_a"}"#);
        let (base, paths) = serve(vec![("200 OK", r#"{"revoked":true}"#.into())]);
        let account = Auth::new(
            Arc::new(api_with(&base, false)),
            Arc::new(CannotDelete(platform.clone())),
            config(),
        );
        let error = account.logout().await.unwrap_err();
        assert!(matches!(
            error,
            AuthError::Store(StoreError { locked: true, .. })
        ));
        assert_eq!(platform.stored().unwrap()["active_refresh"], "rt_a");
        assert_eq!(
            paths.lock().unwrap().as_slice(),
            ["POST /api/v1/auth/device/revoke"]
        );
    }

    #[tokio::test]
    async fn logout_clears_local_credentials_when_remote_revoke_fails() {
        let (base, _) = serve(vec![(
            "503 Service Unavailable",
            r#"{"code":"SERVICE_UNAVAILABLE"}"#.into(),
        )]);
        let platform = MemoryStore::with_blob(
            r#"{"version":1,"active_refresh":"rt_a","pending_refresh":"rt_p"}"#,
        );
        let auth = auth(&base, platform.clone());
        auth.logout().await.unwrap();
        assert!(platform.stored().is_none());
    }

    #[tokio::test]
    async fn poll_handles_pending_then_denied() {
        let (base, paths) = serve(vec![
            ("200 OK", r#"{"status":"authorization_pending"}"#.into()),
            ("200 OK", r#"{"status":"access_denied"}"#.into()),
        ]);
        let auth = auth(&base, Arc::default());
        let generation = {
            let mut flow = auth.flow();
            flow.pending = Some(PendingAuthorization {
                device_code: "d".repeat(43),
                expires_at: Instant::now() + Duration::from_secs(30),
                interval: Duration::from_millis(10),
            });
            flow.generation
        };
        let error = auth.poll_until_done(generation).await.unwrap_err();
        assert!(matches!(error, AuthError::Denied));
        assert_eq!(paths.lock().unwrap().len(), 2);
        assert!(auth.flow().pending.is_none());
    }

    #[tokio::test]
    async fn poll_expires_locally() {
        let auth = auth("http://127.0.0.1:9", Arc::default());
        let generation = {
            let mut flow = auth.flow();
            flow.pending = Some(PendingAuthorization {
                device_code: "d".repeat(43),
                expires_at: Instant::now() + Duration::from_millis(20),
                interval: Duration::from_secs(5),
            });
            flow.generation
        };
        assert!(matches!(
            auth.poll_until_done(generation).await,
            Err(AuthError::Expired)
        ));
    }
    fn device_code_body(base: &str) -> String {
        format!(
            r#"{{"device_code":"{}","user_code":"ABCD-EFGH","verification_uri_complete":"{base}/device/authorize?user_code=ABCD-EFGH","expires_in":600,"interval":5}}"#,
            "d".repeat(64)
        )
    }

    fn auth_with_audiences(base: &str, header: Option<&str>, token: &str) -> Auth {
        let api = Api::new(api::ApiConfig {
            base: base.into(),
            header_audience: header.map(str::to_string),
            token_audience: token.into(),
            accept_language: None,
            trust_local_backend: true,
        });
        Auth::new(Arc::new(api), Arc::new(MemoryStore::default()), config())
    }

    #[tokio::test]
    async fn device_calls_send_the_product_audience_only_when_configured() {
        // The header-less flow is how a CLI obtains `cli` tokens.
        let (base, _, heads) =
            test_support::serve_recording(|base| vec![("200 OK", device_code_body(base))]);
        auth_with_audiences(&base, None, "cli")
            .start("host", "linux", "1.0.0")
            .await
            .unwrap();
        assert!(
            !heads.lock().unwrap()[0].contains("x-product-aud"),
            "no header audience configured, but the header was sent"
        );

        let (base, _, heads) =
            test_support::serve_recording(|base| vec![("200 OK", device_code_body(base))]);
        auth_with_audiences(&base, Some("app"), "app")
            .start("host", "linux", "1.0.0")
            .await
            .unwrap();
        assert!(heads.lock().unwrap()[0].contains("x-product-aud: app"));
    }

    #[test]
    fn the_production_config_is_the_device_page_on_the_api_base_only() {
        let config = AuthConfig::production();
        assert_eq!(config.verification_paths, ["/dashboard/device/authorize"]);
        let base = "https://www.example.com";
        let url = "https://www.example.com/dashboard/device/authorize?user_code=ABCD-EFGH";
        assert!(validate_verification_url(&config, url, "ABCD-EFGH", base, false).is_ok());
        // No host is built in: a page anywhere but the API base is refused.
        for other in [
            "https://api.example.com/dashboard/device/authorize?user_code=ABCD-EFGH",
            "https://www.example.net/dashboard/device/authorize?user_code=ABCD-EFGH",
        ] {
            assert!(
                validate_verification_url(&config, other, "ABCD-EFGH", base, false).is_err(),
                "{other}"
            );
        }
    }

    #[test]
    fn auth_checks_tokens_against_its_api_audience() {
        let auth = auth_with_audiences("https://api.example.com", None, "cli");
        let cli = jwt(r#"{"aud":"cli"}"#);
        assert!(auth.access_token_from(cli, Some(60)).is_ok());
        let app = jwt(r#"{"aud":["app"]}"#);
        assert!(matches!(
            auth.access_token_from(app, None),
            Err(AuthError::Untrusted("restricted session"))
        ));
    }

    #[test]
    fn access_tokens_are_checked_against_the_token_audience() {
        let cli = jwt(r#"{"aud":"cli","exp":4102444800}"#);
        assert_eq!(
            validate_access_token(&cli, "cli").unwrap(),
            Some(4102444800)
        );
        assert!(matches!(
            validate_access_token(&cli, "app"),
            Err(AuthError::Untrusted(_))
        ));
        let api = Api::new(api::ApiConfig {
            base: "https://api.example.com".into(),
            header_audience: None,
            token_audience: "cli".into(),
            accept_language: None,
            trust_local_backend: false,
        });
        assert_eq!(api.token_audience(), "cli");
    }
}
