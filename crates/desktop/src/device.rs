//! Push registration, on [`ppvpn_account::device`].
//!
//! Phones (#539) register the APNs or FCM token the host hands over with
//! [`Client::set_push_token`]. It is device-wide, so it is kept in
//! `<data_dir>/push-token.json` across sign-outs; the device is registered
//! on sign-in, on every launch and whenever the token changes, and
//! unregistered by token at sign-out (best effort).
//!
//! The desktop registers for the self-hosted channel (main-app side of the
//! push agent):
//!
//! Each install has a stable `installation_id` (`<data_dir>/installation.json`,
//! kept across sign-outs). While signed in, the device is registered with the
//! backend on sign-in and on every launch; the returned push token goes to
//! `<data_dir>/push-agent.json`, which the separate push agent
//! ([`crate::PushAgent`]) watches. Sign-out deletes the device (best effort)
//! and the agent files (including the pushes recorded for notification
//! clicks).

use std::path::{Path, PathBuf};

use ppvpn_account::device::{
    installation_id, official_push, push_platform, DeviceRegistrationRequest,
};
pub(crate) use ppvpn_account::device::{write_private, DeviceState};
use serde::{Deserialize, Serialize};

use crate::api::ApiError;
use crate::api::ApiErrorExt;
use crate::errors::ClientError;
use crate::session::ClientRef;
use crate::Client;

pub(crate) const AGENT_FILE: &str = "push-agent.json";
pub(crate) const AGENT_CURSOR_FILE: &str = "push-agent-cursor.json";
pub(crate) const PUSH_TOKEN_FILE: &str = "push-token.json";

/// The OS push token (APNs, FCM) the host handed over.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct OsPushToken {
    pub token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle_id: Option<String>,
}

pub(crate) fn read_os_token(data_dir: &str) -> Option<OsPushToken> {
    let raw = std::fs::read(Path::new(data_dir).join(PUSH_TOKEN_FILE)).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// What the push agent needs; written by the main app only.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentFile {
    pub api_base: String,
    pub device_id: u64,
    pub push_token: String,
    pub installation_id: String,
}

pub(crate) fn agent_file_path(data_dir: &str) -> PathBuf {
    Path::new(data_dir).join(AGENT_FILE)
}

pub(crate) fn read_agent_file(data_dir: &str) -> Option<AgentFile> {
    let raw = std::fs::read(agent_file_path(data_dir)).ok()?;
    serde_json::from_slice(&raw).ok()
}

/// Sign-out: the agent must stop pulling for this account, and its pushes
/// no longer resolve notification clicks.
pub(crate) fn remove_agent_files(data_dir: &str) {
    for name in [
        AGENT_FILE,
        AGENT_CURSOR_FILE,
        crate::push_shown::SHOWN_FILE,
        crate::android_push::LEASE_FILE,
    ] {
        let path = Path::new(data_dir).join(name);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => tracing::warn!("remove {}: {error}", path.display()),
        }
    }
}

pub(crate) fn locale() -> String {
    sys_locale::get_locale()
        .map(|locale| locale.replace('_', "-"))
        .filter(|locale| !locale.is_empty())
        .unwrap_or_else(|| "en-US".to_string())
}

pub(crate) fn timezone() -> String {
    iana_time_zone::get_timezone()
        .ok()
        .filter(|zone| !zone.is_empty())
        .unwrap_or_else(|| "UTC".to_string())
}

/// 403 (missing `device:register` scope, or not a desktop session).
pub(crate) fn needs_new_token(error: &ClientError) -> bool {
    matches!(error, ClientError::Failed { detail, .. } if detail.contains("-> HTTP 403"))
}

impl Client {
    /// Phones: the APNs device token (iOS) or FCM registration token
    /// (Android) the OS gave the app, and the app's bundle id / package name
    /// when known. Call it whenever the OS hands one over (every launch, and
    /// on rotation). While signed in the device is registered with it; a
    /// replaced token is unregistered first. Ignored on other platforms.
    pub fn set_push_token(&self, token: String, bundle_id: Option<String>) {
        if !official_push(&self.config.platform) {
            tracing::warn!("push registration: OS push tokens are for the phones only");
            return;
        }
        let token = token.trim().to_string();
        if token.is_empty() {
            return;
        }
        let new = OsPushToken {
            token,
            bundle_id: bundle_id.filter(|id| !id.trim().is_empty()),
        };
        let old = read_os_token(&self.config.data_dir);
        if old.as_ref() != Some(&new) {
            let raw = serde_json::to_vec(&new).unwrap_or_default();
            let path = Path::new(&self.config.data_dir).join(PUSH_TOKEN_FILE);
            if let Err(detail) = write_private(&path, &raw) {
                tracing::warn!("push registration: {detail}");
                return;
            }
        }
        let Ok(session) = self.current_session() else {
            return;
        };
        let replaced = old.filter(|old| old.token != new.token);
        let was_registered = {
            let mut state = self.session_state();
            let registered = state.is_session(session) && state.device == DeviceState::Registered;
            if registered && replaced.is_some() {
                state.device = DeviceState::Pending;
            }
            registered
        };
        let weak = self.this.clone();
        self.runtime.spawn(async move {
            let Some(client) = ClientRef::upgrade(&weak) else {
                return;
            };
            if let (Some(old), true) = (replaced, was_registered) {
                client.unregister_os_token(session, &old.token).await;
            }
            client.ensure_device_registered(session).await;
        });
    }

    /// Whether the current token registration finished; used by Android's
    /// bounded background worker. Signed-out is handled separately by it.
    pub fn push_token_registered(&self, token: &str) -> bool {
        self.session_state().device == DeviceState::Registered
            && read_os_token(&self.config.data_dir).is_some_and(|saved| saved.token == token)
    }

    /// Registers when the session wants it; called by the refresh loop.
    pub(crate) async fn ensure_device_registered(&self, session: u64) {
        self.ensure_android_push(session).await;
        let wanted = {
            let state = self.session_state();
            state.is_session(session) && state.device == DeviceState::Pending
        };
        if wanted {
            self.register_device(session).await;
        }
    }

    /// After an access-token refresh: retry a registration the old token was
    /// not allowed to make.
    pub(crate) fn on_access_refreshed(&self, session: u64) {
        let retry = {
            let mut state = self.session_state();
            let retry = state.is_session(session) && state.device == DeviceState::AwaitingRefresh;
            if retry {
                state.device = DeviceState::Pending;
            }
            let fallback_retry =
                state.is_session(session) && state.android_push == DeviceState::AwaitingRefresh;
            if fallback_retry {
                state.android_push = DeviceState::Pending;
            }
            retry || fallback_retry
        };
        if retry {
            let weak = self.this.clone();
            self.runtime.spawn(async move {
                if let Some(client) = ClientRef::upgrade(&weak) {
                    client.ensure_device_registered(session).await;
                }
            });
        }
    }

    async fn register_device(&self, session: u64) {
        let official = official_push(&self.config.platform);
        let (installation, os_token) = if official {
            match read_os_token(&self.config.data_dir) {
                Some(os_token) => (String::new(), Some(os_token)),
                // Registered once the host hands the token over.
                None => return,
            }
        } else {
            match installation_id(&self.config.data_dir) {
                Ok(id) => (id, None),
                Err(detail) => {
                    tracing::warn!("push registration: installation id unavailable: {detail}");
                    return;
                }
            }
        };
        let body = DeviceRegistrationRequest {
            platform: push_platform(&self.config.platform),
            installation_id: (!official).then(|| installation.clone()),
            token: os_token.as_ref().map(|os_token| os_token.token.clone()),
            bundle_id: os_token.and_then(|os_token| os_token.bundle_id),
            app_version: self.config.app_version.clone(),
            locale: locale(),
            timezone: timezone(),
        };
        let body = &body;
        let result = self
            .bearer(
                session,
                |token| async move { self.auth.api().register_device(&token, body).await },
                ApiError::into_client_error,
            )
            .await;
        let registration = match result {
            Ok(registration) => registration,
            Err(error) if needs_new_token(&error) => {
                tracing::info!(
                    "push registration refused ({error}); retrying after the next token refresh"
                );
                self.set_device_state(session, DeviceState::AwaitingRefresh);
                return;
            }
            Err(error) => {
                tracing::info!("push registration failed, retrying later: {error}");
                return;
            }
        };
        if official {
            if self.session_is(session)
                && read_os_token(&self.config.data_dir).map(|saved| saved.token) == body.token
            {
                tracing::info!("push registration: device {} registered", registration.id);
                self.set_device_state(session, DeviceState::Registered);
            }
            return;
        }
        let file = AgentFile {
            api_base: self.config.api_base.clone(),
            device_id: registration.id,
            push_token: registration.push_token,
            installation_id: installation,
        };
        if !self.session_is(session) {
            return;
        }
        let raw = serde_json::to_vec_pretty(&file).unwrap_or_default();
        match write_private(&agent_file_path(&self.config.data_dir), &raw) {
            Ok(()) => {
                tracing::info!("push registration: device {} registered", file.device_id);
                self.set_device_state(session, DeviceState::Registered);
            }
            Err(detail) => tracing::warn!("push registration: {detail}"),
        }
    }

    fn set_device_state(&self, session: u64, device: DeviceState) {
        let mut state = self.session_state();
        if state.is_session(session) {
            state.device = device;
        }
    }

    /// Sign-out: deletes the device on the backend (best effort, while the
    /// session can still authenticate).
    pub(crate) async fn unregister_device(&self) {
        if let Ok(session) = self.current_session() {
            self.unregister_android_push(session).await;
        }
        if official_push(&self.config.platform) {
            let Ok(session) = self.current_session() else {
                return;
            };
            let registered = self.session_state().device == DeviceState::Registered;
            if let (true, Some(os_token)) = (registered, read_os_token(&self.config.data_dir)) {
                self.unregister_os_token(session, &os_token.token).await;
            }
            return;
        }
        let Some(file) = read_agent_file(&self.config.data_dir) else {
            return;
        };
        let Ok(session) = self.current_session() else {
            return;
        };
        let id = file.device_id;
        let deleted = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.bearer(
                session,
                |token| async move { self.auth.api().delete_device(&token, id).await },
                ApiError::into_client_error,
            ),
        )
        .await;
        match deleted {
            Ok(Ok(())) => tracing::info!("push registration: device {id} deleted"),
            Ok(Err(error)) => tracing::info!("push registration: delete failed: {error}"),
            Err(_) => tracing::info!("push registration: delete timed out"),
        }
    }

    /// Disables the device holding the OS push token `push_token` (best
    /// effort).
    async fn unregister_os_token(&self, session: u64, push_token: &str) {
        let unregistered = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.bearer(
                session,
                |token| async move {
                    self.auth
                        .api()
                        .unregister_push_token(&token, push_token)
                        .await
                },
                ApiError::into_client_error,
            ),
        )
        .await;
        match unregistered {
            Ok(Ok(())) => tracing::info!("push registration: device unregistered"),
            Ok(Err(error)) => tracing::info!("push registration: unregister failed: {error}"),
            Err(_) => tracing::info!("push registration: unregister timed out"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ppvpn_account::device::INSTALLATION_FILE;

    #[test]
    fn installation_id_survives_sign_out() {
        let dir = crate::test_backend::temp_dir("ppvpn-install-test");
        let first = installation_id(&dir).unwrap();
        // Survives removing the agent files (sign-out).
        remove_agent_files(&dir);
        assert_eq!(installation_id(&dir).unwrap(), first);
        let _ = std::fs::remove_dir_all(dir);
    }

    use crate::auth::test_support::MemoryPlatform;
    use crate::cores::test_support::{FakeLauncher, NoService};
    use crate::test_backend::{serve, temp_dir, wait_until, Backend, PUSH_TOKEN};
    use crate::{ClientConfig, ClientListener, ClientSnapshot, ProbeResult, TrafficSample};
    use std::sync::Arc;

    struct Quiet;

    impl ClientListener for Quiet {
        fn on_snapshot(&self, _: ClientSnapshot) {}
        fn on_probe_result(&self, _: ProbeResult) {}
        fn on_traffic(&self, _: TrafficSample) {}
    }

    fn client(base: &str, data_dir: &str) -> Arc<Client> {
        client_on(base, data_dir, "windows")
    }

    fn client_on(base: &str, data_dir: &str, platform: &str) -> Arc<Client> {
        Client::with_parts(
            ClientConfig {
                api_base: base.to_string(),
                data_dir: data_dir.to_string(),
                log_dir: data_dir.to_string(),
                platform: platform.into(),
                app_version: "1.2.3".into(),
            },
            MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#),
            Arc::new(Quiet),
            Arc::new(NoService),
            FakeLauncher::new(),
            Arc::new(crate::sysproxy::tests::FakeWriter::default()),
        )
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn sign_in_registers_and_sign_out_unregisters() {
        let backend = Backend::new();
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-device-test");
        let client = client(&base, &dir);

        wait_until(|| read_agent_file(&dir).is_some());
        let file = read_agent_file(&dir).unwrap();
        assert_eq!(file.device_id, 42);
        assert_eq!(file.push_token, PUSH_TOKEN);
        assert_eq!(file.api_base, base);
        assert_eq!(file.installation_id, installation_id(&dir).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(agent_file_path(&dir))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        assert_eq!(
            backend.requests_matching("/devices/register"),
            vec!["POST /api/v1/devices/register"]
        );
        std::fs::write(Path::new(&dir).join(AGENT_CURSOR_FILE), "{}").unwrap();
        // What the push agent recorded resolves a notification click.
        let push = crate::PushMessage {
            id: 17,
            message_id: None,
            title: "t".into(),
            body: "b".into(),
            severity: crate::MessageSeverity::Normal,
            category: crate::MessageCategory::Announcement,
            event_key: "broadcast:3".into(),
            deep_link: None,
            created_at: "2026-09-29T00:00:00Z".into(),
        };
        crate::push_shown::record(Path::new(&dir), &push).unwrap();
        assert_eq!(client.shown_push(17), Some(push));
        assert_eq!(client.shown_push(18), None);

        block_on(client.logout()).unwrap();
        assert_eq!(
            backend.requests_matching("DELETE"),
            vec!["DELETE /api/v1/devices/42"]
        );
        assert!(read_agent_file(&dir).is_none());
        assert!(!Path::new(&dir).join(AGENT_CURSOR_FILE).exists());
        assert!(!Path::new(&dir).join(crate::push_shown::SHOWN_FILE).exists());
        assert_eq!(client.shown_push(17), None);
        assert!(Path::new(&dir).join(INSTALLATION_FILE).exists());
        client.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_phone_registers_its_os_token_and_unregisters_it_at_sign_out() {
        let backend = Backend::new();
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-device-test");
        let client = client_on(&base, &dir, "ios");
        let bodies = || backend.device_bodies.lock().unwrap().clone();

        // Signed in, but no OS token yet: nothing to register.
        wait_until(|| client.snapshot().auth == crate::AuthState::SignedIn);
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(backend.requests_matching("/devices/").is_empty());

        client.set_push_token("apns-1".into(), Some("com.example.app".into()));
        wait_until(|| !bodies().is_empty());
        let body = &bodies()[0];
        assert_eq!(body["platform"], "ios");
        assert_eq!(body["token"], "apns-1");
        assert_eq!(body["bundle_id"], "com.example.app");
        assert!(body.get("installation_id").is_none());
        assert!(read_agent_file(&dir).is_none(), "no push agent on a phone");
        wait_until(|| client.session_state().device == DeviceState::Registered);

        // The same token again changes nothing.
        client.set_push_token("apns-1".into(), Some("com.example.app".into()));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert_eq!(bodies().len(), 1);

        // A new token: the old one is unregistered, the new one registered.
        client.set_push_token("apns-2".into(), None);
        wait_until(|| bodies().len() == 3);
        assert_eq!(bodies()[1], serde_json::json!({"token": "apns-1"}));
        assert_eq!(bodies()[2]["token"], "apns-2");
        assert!(bodies()[2].get("bundle_id").is_none());
        wait_until(|| client.session_state().device == DeviceState::Registered);

        block_on(client.logout()).unwrap();
        assert_eq!(bodies()[3], serde_json::json!({"token": "apns-2"}));
        assert!(backend.requests_matching("DELETE").is_empty());
        // The token is the device's: it stays for the next sign-in.
        assert_eq!(read_os_token(&dir).unwrap().token, "apns-2");
        client.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_token_without_the_scope_waits_for_the_next_refresh() {
        let backend = Backend::new();
        *backend.register_status.lock().unwrap() = Some("403 Forbidden");
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-device-test");
        let client = client(&base, &dir);
        wait_until(|| !backend.requests_matching("/devices/register").is_empty());
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(backend.requests_matching("/devices/register").len(), 1);
        assert!(read_agent_file(&dir).is_none());
        assert!(client.snapshot().last_error.is_none());
        client.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn android_fallback_registers_without_fcm_and_stops_on_headless_recovery() {
        let backend = Backend::new();
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-fallback-test");
        let client = client_on(&base, &dir, "android");
        wait_until(|| client.snapshot().auth == crate::AuthState::SignedIn);
        client.set_android_push_fallback(true);
        let lease = Path::new(&dir).join(crate::android_push::LEASE_FILE);
        wait_until(|| lease.exists());
        let bodies = backend.device_bodies.lock().unwrap().clone();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["platform"], "desktop-android");
        assert!(bodies[0].get("token").is_none());
        assert!(bodies[0]["installation_id"].as_str().is_some());
        client.set_push_token("fcm-1".into(), None);
        client.set_android_push_fallback(false);
        assert!(!lease.exists(), "local fallback stops synchronously");
        wait_until(|| read_agent_file(&dir).is_none());
        wait_until(|| client.session_state().device == DeviceState::Registered);
        assert_eq!(backend.requests_matching("DELETE").len(), 1);
        assert!(backend
            .device_bodies
            .lock()
            .unwrap()
            .iter()
            .any(|body| body["platform"] == "android" && body["token"] == "fcm-1"));
        block_on(client.logout()).unwrap();
        assert!(!lease.exists());
        assert!(backend
            .device_bodies
            .lock()
            .unwrap()
            .iter()
            .any(|body| *body == serde_json::json!({"token":"fcm-1"})));
        client.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn android_fallback_stops_at_sign_out_and_retries_after_scope_refresh() {
        let backend = Backend::new();
        *backend.register_status.lock().unwrap() = Some("403 Forbidden");
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-fallback-test");
        let client = client_on(&base, &dir, "android");
        client.set_android_push_fallback(true);
        wait_until(|| client.session_state().android_push == DeviceState::AwaitingRefresh);
        assert!(read_agent_file(&dir).is_none());
        *backend.register_status.lock().unwrap() = None;
        let session = client.current_session().unwrap();
        client.on_access_refreshed(session);
        let lease = Path::new(&dir).join(crate::android_push::LEASE_FILE);
        wait_until(|| lease.exists());
        block_on(client.logout()).unwrap();
        assert!(!lease.exists());
        assert!(read_agent_file(&dir).is_none());
        assert_eq!(backend.requests_matching("DELETE").len(), 1);
        client.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn android_fallback_is_ignored_on_ios_and_desktop() {
        for platform in ["ios", "windows"] {
            let backend = Backend::new();
            let base = serve(backend.clone());
            let dir = temp_dir("ppvpn-fallback-test");
            let client = client_on(&base, &dir, platform);
            wait_until(|| client.snapshot().auth == crate::AuthState::SignedIn);
            client.set_android_push_fallback(true);
            assert!(!client
                .android_push_fallback
                .load(std::sync::atomic::Ordering::SeqCst));
            client.shutdown();
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn platform_and_environment_values() {
        assert!(!locale().is_empty());
        assert!(!timezone().is_empty());
    }
}
