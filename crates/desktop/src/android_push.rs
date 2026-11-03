//! Android's self-hosted fallback registration. The native VPN foreground
//! service owns the agent; the authenticated UI only provides its lease.
use std::path::Path;
use std::sync::atomic::Ordering;

use ppvpn_account::device::{installation_id, DeviceRegistrationRequest};

use crate::api::{ApiError, ApiErrorExt};
use crate::device::{self, AgentFile, DeviceState};
use crate::session::ClientRef;
use crate::Client;

/// Present only while fallback is enabled and this account is registered.
/// The service watches it together with push-agent.json, and stops at once
/// when either disappears (FCM recovered or sign-out).
pub(crate) const LEASE_FILE: &str = "android-push-fallback";

impl Client {
    /// Android host: no Google Play services, no Firebase configuration, or
    /// FCM token acquisition failed/timed out. This never starts a service.
    pub fn set_android_push_fallback(&self, enabled: bool) {
        if self.config.platform != "android" {
            return;
        }
        {
            let mut state = self.session_state();
            let changed = self.android_push_fallback.swap(enabled, Ordering::SeqCst) != enabled;
            if changed {
                state.android_push = DeviceState::Pending;
            }
            if !enabled {
                let _ = std::fs::remove_file(Path::new(&self.config.data_dir).join(LEASE_FILE));
            }
        }
        let Ok(session) = self.current_session() else {
            return;
        };
        let weak = self.this.clone();
        self.runtime.spawn(async move {
            if let Some(client) = ClientRef::upgrade(&weak) {
                client.ensure_android_push(session).await;
            }
        });
    }

    pub(crate) async fn ensure_android_push(&self, session: u64) {
        if self.config.platform != "android" {
            return;
        }
        let _guard = self.android_push_lock.lock().await;
        if !self.session_is(session) {
            return;
        }
        if !self.android_push_fallback.load(Ordering::SeqCst) {
            self.remove_android_push(session).await;
            return;
        }
        if self.session_state().android_push != DeviceState::Pending {
            return;
        }
        let Ok(installation) = installation_id(&self.config.data_dir) else {
            return;
        };
        // #539 explicitly reserves desktop-* for the existing self-hosted
        // protocol. Keep the phone's FCM registration separate.
        let body = DeviceRegistrationRequest {
            platform: "desktop-android".into(),
            installation_id: Some(installation.clone()),
            token: None,
            bundle_id: None,
            app_version: self.config.app_version.clone(),
            locale: device::locale(),
            timezone: device::timezone(),
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
            Err(error) => {
                if device::needs_new_token(&error) {
                    let mut state = self.session_state();
                    if state.is_session(session) {
                        state.android_push = DeviceState::AwaitingRefresh;
                    }
                }
                tracing::info!("Android push fallback registration failed: {error}");
                return;
            }
        };
        let file = AgentFile {
            api_base: self.config.api_base.clone(),
            device_id: registration.id,
            push_token: registration.push_token,
            installation_id: installation,
        };
        // Hold the session lock while publishing files: sign-out/new login
        // cannot remove them and then have this old response recreate them.
        let mut state = self.session_state();
        if !state.is_session(session) {
            return;
        }
        let raw = serde_json::to_vec_pretty(&file).unwrap_or_default();
        if let Err(detail) =
            device::write_private(&device::agent_file_path(&self.config.data_dir), &raw)
        {
            tracing::warn!("Android push fallback: {detail}");
            return;
        }
        if self.android_push_fallback.load(Ordering::SeqCst) {
            let lease = Path::new(&self.config.data_dir).join(LEASE_FILE);
            if let Err(detail) = device::write_private(&lease, b"enabled") {
                tracing::warn!("Android push fallback: {detail}");
                return;
            }
        }
        state.android_push = DeviceState::Registered;
    }

    pub(crate) async fn unregister_android_push(&self, session: u64) {
        if self.config.platform != "android" {
            return;
        }
        let _guard = self.android_push_lock.lock().await;
        self.remove_android_push(session).await;
    }

    /// The caller holds android_push_lock. Local delivery stops before the
    /// best-effort backend request, including when the network is offline.
    async fn remove_android_push(&self, session: u64) {
        let file = device::read_agent_file(&self.config.data_dir);
        device::remove_agent_files(&self.config.data_dir);
        if let Some(file) = file {
            let id = file.device_id;
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                self.bearer(
                    session,
                    |token| async move { self.auth.api().delete_device(&token, id).await },
                    ApiError::into_client_error,
                ),
            )
            .await;
        }
        let mut state = self.session_state();
        if state.is_session(session) {
            state.android_push = DeviceState::Pending;
        }
    }
}
