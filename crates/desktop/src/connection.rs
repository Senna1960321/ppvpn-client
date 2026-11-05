//! The single "Connect" switch: dispatches to enhanced mode (TUN via the
//! privileged service) or compatible mode (OS system proxy), which are
//! mutually exclusive, and folds both into [`ConnectionState`].
//!
//! The mode is persisted in `<data_dir>/client-settings.json` (migrated once
//! from the apps' `settings.json`, see [`load_mode`]).

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::compat::CompatState;
use crate::cores::is_enhanced_error;
use crate::errors::{ClientError, ClientErrorInfo, ErrorCode};
use crate::{
    Client, ConnectionDetail, ConnectionMode, ConnectionPhase, ConnectionState, EnhancedState,
};

use crate::storage::{CLIENT_SETTINGS_FILE, LEGACY_SETTINGS_FILE};

#[derive(Serialize, Deserialize, Default)]
struct Settings {
    #[serde(default)]
    connection_mode: Option<String>,
    #[serde(default)]
    routing_mode: Option<String>,
}

fn mode_name(mode: ConnectionMode) -> &'static str {
    match mode {
        ConnectionMode::Enhanced => "enhanced",
        ConnectionMode::Compatible => "compatible",
    }
}

fn read_settings(path: &Path) -> Option<Settings> {
    std::fs::read(path)
        .ok()
        .and_then(|raw| serde_json::from_slice::<Settings>(&raw).ok())
}

fn parse_mode(name: Option<&str>) -> ConnectionMode {
    match name {
        Some("compatible") => ConnectionMode::Compatible,
        _ => ConnectionMode::Enhanced,
    }
}

/// The persisted mode; Enhanced when unset or unreadable.
///
/// Earlier builds wrote it into `settings.json`, which the apps own: when
/// `client-settings.json` does not exist yet and `settings.json` carries a
/// `connection_mode`, it is copied over once (`settings.json` is left as is).
pub(crate) fn load_mode(data_dir: &str) -> ConnectionMode {
    let dir = Path::new(data_dir);
    if let Some(settings) = read_settings(&dir.join(CLIENT_SETTINGS_FILE)) {
        return parse_mode(settings.connection_mode.as_deref());
    }
    let legacy = read_settings(&dir.join(LEGACY_SETTINGS_FILE))
        .and_then(|settings| settings.connection_mode);
    let mode = parse_mode(legacy.as_deref());
    if legacy.is_some() {
        match save_mode(data_dir, mode) {
            Ok(()) => tracing::info!("storage: connection mode moved to {CLIENT_SETTINGS_FILE}"),
            Err(detail) => tracing::warn!("storage: migrate connection mode: {detail}"),
        }
    }
    mode
}

fn save_mode(data_dir: &str, mode: ConnectionMode) -> Result<(), String> {
    save_setting(data_dir, "connection_mode", mode_name(mode))
}

/// The persisted routing mode; Rules when unset or unreadable.
pub(crate) fn load_routing_mode(data_dir: &str) -> crate::RoutingMode {
    let settings = read_settings(&Path::new(data_dir).join(CLIENT_SETTINGS_FILE));
    crate::routing::parse(settings.and_then(|s| s.routing_mode).as_deref())
}

pub(crate) fn save_routing_mode(data_dir: &str, mode: crate::RoutingMode) -> Result<(), String> {
    save_setting(data_dir, "routing_mode", crate::routing::wire_name(mode))
}

/// Writes one key of `client-settings.json`, keeping the others.
fn save_setting(data_dir: &str, key: &str, value: &str) -> Result<(), String> {
    save_setting_value(data_dir, key, serde_json::json!(value))
}

/// [`save_setting`] with any JSON value.
pub(crate) fn save_setting_value(
    data_dir: &str,
    key: &str,
    value: serde_json::Value,
) -> Result<(), String> {
    let path = Path::new(data_dir).join(CLIENT_SETTINGS_FILE);
    let mut settings: serde_json::Value = std::fs::read(&path)
        .ok()
        .and_then(|raw| serde_json::from_slice(&raw).ok())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    settings[key] = value;
    std::fs::create_dir_all(data_dir).map_err(|error| format!("create {data_dir}: {error}"))?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&settings).unwrap_or_default())
        .map_err(|error| format!("write {}: {error}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|error| format!("rename {}: {error}", path.display()))
}

/// Both modes' latest states plus the path detail from the monitor.
#[derive(Default)]
pub(crate) struct ConnectionParts {
    enhanced: EnhancedState,
    compat: CompatState,
    detail: ConnectionDetail,
    /// Competitor named for the current enhanced failure: by the pre-connect
    /// check, else by detection run at the failure. Shown as `competitor`
    /// for path problems only; any failure with one suggests compatible mode.
    enhanced_competitor: Option<String>,
}

/// Enhanced is contended or failed because of the network path (not the
/// service, the profile or ownership): worth naming a competitor. That is
/// `NetworkPathContended`, `ConnectHealthCheckFailed` (the direct path or
/// the TUN capture failed: DNS or the route taken over, e.g. Surge's
/// enhanced mode with fake-IP DNS), and the other `HEALTH_*` steps of
/// `ConnectFailed` (a foreign tunnel can break those too), including
/// `HEALTH_ENTRANCE_FAILED` (the core's entrance probe: a foreign TUN
/// swallows it). Detection only names an app when it sees a tunnel signal,
/// so a node failure stays unnamed.
pub(crate) fn is_path_problem(state: &EnhancedState) -> bool {
    matches!(
        state.phase,
        ConnectionPhase::Error | ConnectionPhase::Contended
    ) && state
        .reason
        .as_ref()
        .is_some_and(|reason| match reason.code {
            ErrorCode::NetworkPathContended | ErrorCode::ConnectHealthCheckFailed => true,
            ErrorCode::ConnectFailed => {
                reason.detail.starts_with("HEALTH_")
                    || reason.detail.contains("HEALTH_ENTRANCE_FAILED")
            }
            _ => false,
        })
}

/// Enhanced failed or is contended for any reason but another session or
/// user owning the connection: conflict detection runs, and a competitor it
/// finds makes compatible mode worth suggesting.
pub(crate) fn is_failure(state: &EnhancedState) -> bool {
    matches!(
        state.phase,
        ConnectionPhase::Error | ConnectionPhase::Contended
    ) && !state.reason.as_ref().is_some_and(|reason| {
        matches!(
            reason.code,
            ErrorCode::ServiceBusy | ErrorCode::ServiceOwnedByAnotherUser
        )
    })
}

/// Enhanced failures that compatible mode would avoid.
pub(crate) fn suggests_compatible(state: &EnhancedState) -> bool {
    matches!(
        state.phase,
        ConnectionPhase::Error | ConnectionPhase::Contended
    ) && state.reason.as_ref().is_some_and(|reason| {
        matches!(
            reason.code,
            ErrorCode::ServiceInstallCancelled
                | ErrorCode::ServiceInstallFailed
                | ErrorCode::NetworkPathContended
                | ErrorCode::ConnectHealthCheckFailed
        )
    })
}

/// The connection as the apps see it, for the selected mode.
pub(crate) fn compose(mode: ConnectionMode, parts: &ConnectionParts) -> ConnectionState {
    match mode {
        ConnectionMode::Enhanced => ConnectionState {
            phase: parts.enhanced.phase,
            reason: parts.enhanced.reason.clone(),
            retryable: parts.enhanced.retryable,
            can_take_over: parts.enhanced.can_take_over,
            // Another tunnel owns the path: compatible mode does not fight
            // it, whatever the failure was.
            suggest_compatible: suggests_compatible(&parts.enhanced)
                || (is_failure(&parts.enhanced) && parts.enhanced_competitor.is_some()),
            competitor: is_path_problem(&parts.enhanced)
                .then(|| parts.enhanced_competitor.clone())
                .flatten(),
            proxy_was_foreign: false,
            detail: parts.detail.clone(),
        },
        ConnectionMode::Compatible => ConnectionState {
            phase: parts.compat.phase,
            reason: parts.compat.reason.clone(),
            retryable: parts.compat.retryable,
            can_take_over: false,
            suggest_compatible: false,
            competitor: parts.compat.proxy_owner.clone(),
            proxy_was_foreign: parts.compat.proxy_was_foreign,
            detail: parts.detail.clone(),
        },
    }
}

fn is_compat_error(info: &ClientErrorInfo) -> bool {
    matches!(
        info.code,
        ErrorCode::SystemProxyFailed | ErrorCode::SystemProxyUnavailable
    )
}

impl Client {
    fn parts(&self) -> std::sync::MutexGuard<'_, ConnectionParts> {
        self.connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn connection_mode(&self) -> ConnectionMode {
        self.snapshot
            .lock()
            .map(|snapshot| snapshot.connection_mode)
            .unwrap_or_default()
    }

    /// Recomputes `snapshot.connection` / `service_installed`, with
    /// `change` applied to the snapshot in the same update.
    fn publish_connection(&self, change: impl FnOnce(&mut crate::ClientSnapshot)) {
        // Copied out first: the parts lock is never held with the snapshot's.
        let parts = {
            let parts = self.parts();
            ConnectionParts {
                enhanced: parts.enhanced.clone(),
                compat: parts.compat.clone(),
                detail: parts.detail.clone(),
                enhanced_competitor: parts.enhanced_competitor.clone(),
            }
        };
        self.update(|snapshot| {
            change(snapshot);
            snapshot.connection = compose(snapshot.connection_mode, &parts);
            snapshot.service_installed = parts.enhanced.service_installed;
        });
    }

    // --- sinks ----------------------------------------------------------------

    pub(crate) fn on_enhanced_state(&self, state: EnhancedState) {
        let preflight = state.competitors.first().cloned();
        let previous = {
            let mut parts = self.parts();
            if preflight.is_some() {
                // The pre-connect check already looked.
                parts.enhanced_competitor = preflight.clone();
            } else if !is_failure(&state) {
                parts.enhanced_competitor = None;
            }
            std::mem::replace(&mut parts.enhanced, state.clone())
        };
        if preflight.is_none()
            && is_failure(&state)
            && (!is_failure(&previous) || previous.reason != state.reason)
        {
            self.spawn_competitor_detection();
        }
        if previous.phase != state.phase {
            // Entering Error / Contended is logged as a warning: the Logs
            // page's "warnings and errors" filter must show it.
            let failed = matches!(
                state.phase,
                ConnectionPhase::Error | ConnectionPhase::Contended
            );
            let reason = state.reason.as_ref().map(|reason| reason.code);
            match (failed, reason) {
                (true, Some(code)) => tracing::warn!(
                    "enhanced mode: {:?} -> {:?} ({:?})",
                    previous.phase,
                    state.phase,
                    code
                ),
                (true, None) => {
                    tracing::warn!("enhanced mode: {:?} -> {:?}", previous.phase, state.phase)
                }
                (false, Some(code)) => tracing::info!(
                    "enhanced mode: {:?} -> {:?} ({:?})",
                    previous.phase,
                    state.phase,
                    code
                ),
                (false, None) => {
                    tracing::info!("enhanced mode: {:?} -> {:?}", previous.phase, state.phase)
                }
            }
        }
        self.publish_connection(|snapshot| match state.phase {
            ConnectionPhase::Error | ConnectionPhase::Contended => {
                if let Some(reason) = &state.reason {
                    snapshot.last_error = Some(reason.clone());
                }
            }
            ConnectionPhase::On
                if state.reason.is_none()
                    && snapshot.last_error.as_ref().is_some_and(is_enhanced_error) =>
            {
                snapshot.last_error = None;
            }
            _ => {}
        });
    }

    pub(crate) fn on_compat_state(&self, state: CompatState) {
        let previous = std::mem::replace(&mut self.parts().compat, state.clone());
        if previous.phase != state.phase {
            if state.phase == ConnectionPhase::Error {
                tracing::warn!(
                    "compatible mode: {:?} -> {:?} ({:?})",
                    previous.phase,
                    state.phase,
                    state.reason.as_ref().map(|reason| reason.code)
                );
            } else {
                tracing::info!("compatible mode: {:?} -> {:?}", previous.phase, state.phase);
            }
        }
        self.publish_connection(|snapshot| match state.phase {
            ConnectionPhase::Error => {
                if let Some(reason) = &state.reason {
                    snapshot.last_error = Some(reason.clone());
                }
            }
            ConnectionPhase::On if snapshot.last_error.as_ref().is_some_and(is_compat_error) => {
                snapshot.last_error = None;
            }
            _ => {}
        });
    }

    /// Names the app competing for the path, at the moment enhanced mode
    /// failed.
    fn spawn_competitor_detection(&self) {
        let weak = self.this.clone();
        self.runtime.spawn(async move {
            let detector = crate::enhanced::default_detector();
            let report = tokio::task::spawn_blocking(move || detector())
                .await
                .unwrap_or_default();
            let Some(client) = crate::session::ClientRef::upgrade(&weak) else {
                return;
            };
            let competitor = crate::detect::display_competitor(&report);
            tracing::info!(
                "conflict detection: competitors {:?}, fake-ip {:?}, route {:?}",
                report.competitors,
                report.fake_ip_interfaces,
                report.foreign_default_route
            );
            let changed = {
                let mut parts = client.parts();
                if is_failure(&parts.enhanced)
                    && parts.enhanced.competitors.is_empty()
                    && parts.enhanced_competitor != competitor
                {
                    parts.enhanced_competitor = competitor;
                    true
                } else {
                    false
                }
            };
            if changed {
                client.publish_connection(|_| {});
            }
        });
    }

    /// The monitor's latest replica/latency for the current node.
    pub(crate) fn set_connection_detail(&self, detail: ConnectionDetail) {
        let changed = {
            let mut parts = self.parts();
            let changed = parts.detail != detail;
            // The line in use changed (automatic failover or a pin): one
            // line in the log, so a switch can be timed without screenshots.
            if let (Some(from), Some(to)) = (
                parts.detail.endpoint_key.as_deref(),
                detail.endpoint_key.as_deref(),
            ) {
                if from != to {
                    tracing::info!(
                        "ingress switched {from} -> {to} ({})",
                        detail.endpoint_label.as_deref().unwrap_or("no label")
                    );
                }
            }
            parts.detail = detail;
            changed
        };
        if changed {
            self.publish_connection(|_| {});
        }
    }

    // --- actions ----------------------------------------------------------------

    /// Turns the connection on in the selected mode, making sure the other
    /// mode is off.
    pub(crate) async fn connect_now(&self) -> Result<(), ClientError> {
        self.current_session()?;
        if let Some(tunnel) = &self.tunnel {
            self.prime_enhanced().await?;
            return tunnel.enable().await;
        }
        match self.connection_mode() {
            ConnectionMode::Enhanced => {
                if let Err(error) = self.compat.disconnect().await {
                    tracing::warn!("compatible mode: disconnect before enhanced failed: {error}");
                }
                // Enhanced mode never opens the endpoint: an OS proxy still
                // pointing at it would cut every app that uses it (#536).
                self.compat.clear_stale_proxy().await;
                self.prime_enhanced().await?;
                self.enhanced.enable().await
            }
            ConnectionMode::Compatible => {
                self.enhanced.disable().await?;
                self.compat.connect().await
            }
        }
    }

    /// Turns both modes off; the first failure is returned.
    pub(crate) async fn disconnect_now(&self) -> Result<(), ClientError> {
        if let Some(tunnel) = &self.tunnel {
            return tunnel.disable().await;
        }
        let enhanced = self.enhanced.disable().await;
        let compat = self.compat.disconnect().await;
        enhanced.and(compat)
    }

    /// Best-effort disconnect of both modes (session end, no profile).
    pub(crate) async fn disconnect_all(&self, why: &str) {
        if let Err(error) = self.disconnect_now().await {
            tracing::warn!("disconnect at {why} failed: {error}");
        }
    }

    pub(crate) async fn retry_now(&self) -> Result<(), ClientError> {
        self.current_session()?;
        if let Some(tunnel) = &self.tunnel {
            self.prime_enhanced().await?;
            return tunnel.enable().await;
        }
        match self.connection_mode() {
            ConnectionMode::Enhanced => {
                self.prime_enhanced().await?;
                self.enhanced.retry().await
            }
            ConnectionMode::Compatible => {
                self.compat.disconnect().await?;
                self.compat.connect().await
            }
        }
    }

    /// Commit a routing change only after the running cores accepted it.
    /// Keep the transaction alive if its UI caller drops the request midway.
    pub(crate) async fn change_routing_mode(
        &self,
        mode: crate::RoutingMode,
    ) -> Result<(), ClientError> {
        let client =
            crate::session::ClientRef::upgrade(&self.this).ok_or(ClientError::Cancelled)?;
        self.runtime
            .spawn(async move { client.apply_routing_mode(mode).await })
            .await
            .map_err(|error| ClientError::failed(ErrorCode::Internal, error.to_string()))?
    }

    async fn apply_routing_mode(&self, mode: crate::RoutingMode) -> Result<(), ClientError> {
        let _switch = self.mode_switch.lock().await;
        // A profile refresh must not apply the tentative mode during rollback.
        let _cores = self.cores.lock().await;
        let current = self.routing.get();
        if current == mode {
            return Ok(());
        }
        self.routing.set(mode);
        let result = async {
            self.standard.reapply().await?;
            self.reapply_tunnel_routing().await?;
            save_routing_mode(&self.config.data_dir, mode)
                .map_err(|detail| ClientError::failed(ErrorCode::LocalStorageFailed, detail))
        }
        .await;
        if let Err(error) = result {
            self.routing.set(current);
            // Either core may have accepted the tentative mode, including a
            // connect/recovery racing this action or an update whose reply
            // was lost. Restore both; never infer rollback from an error alone.
            let standard = self.standard.reapply().await;
            let tunnel = self.reapply_tunnel_routing().await;
            let tunnel_failed =
                self.connection.lock().unwrap().enhanced.phase == ConnectionPhase::Error;
            if standard.is_err() || tunnel.is_err() || tunnel_failed {
                // A failed compensation must never leave traffic using a
                // mode different from the selection shown to the user.
                tracing::warn!(?standard, ?tunnel, "routing mode rollback: stopping cores");
                self.disconnect_all("routing mode rollback").await;
                self.standard.stop().await;
            }
            return Err(error);
        }
        tracing::info!(
            "routing mode: {} -> {}",
            crate::routing::wire_name(current),
            crate::routing::wire_name(mode)
        );
        self.publish_connection(|snapshot| snapshot.routing_mode = mode);
        Ok(())
    }

    async fn reapply_tunnel_routing(&self) -> Result<(), ClientError> {
        match &self.tunnel {
            Some(tunnel) => tunnel.reapply().await,
            None => self.enhanced.reapply().await,
        }
    }

    /// Persists `mode`; when the connection is not off, disconnects and
    /// connects again in the new mode.
    pub(crate) async fn change_mode(&self, mode: ConnectionMode) -> Result<(), ClientError> {
        let _switch = self.mode_switch.lock().await;
        let (current, phase) = self
            .snapshot
            .lock()
            .map(|snapshot| (snapshot.connection_mode, snapshot.connection.phase))
            .unwrap_or_default();
        if current == mode {
            return Ok(());
        }
        save_mode(&self.config.data_dir, mode)
            .map_err(|detail| ClientError::failed(ErrorCode::LocalStorageFailed, detail))?;
        tracing::info!(
            "connection mode: {} -> {}",
            mode_name(current),
            mode_name(mode)
        );
        let reconnect = phase != ConnectionPhase::Off;
        if reconnect {
            if let Err(error) = self.disconnect_now().await {
                tracing::warn!("disconnect before switching mode failed: {error}");
            }
        }
        self.publish_connection(|snapshot| snapshot.connection_mode = mode);
        if reconnect && self.is_signed_in() {
            self.connect_now().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enhanced(phase: ConnectionPhase, code: Option<ErrorCode>) -> EnhancedState {
        EnhancedState {
            phase,
            reason: code.map(|code| ClientErrorInfo::new(code, "x")),
            ..EnhancedState::default()
        }
    }

    #[test]
    fn suggest_compatible_only_for_failures_it_avoids() {
        for code in [
            ErrorCode::ServiceInstallCancelled,
            ErrorCode::ServiceInstallFailed,
            ErrorCode::NetworkPathContended,
            ErrorCode::ConnectHealthCheckFailed,
        ] {
            assert!(suggests_compatible(&enhanced(
                ConnectionPhase::Error,
                Some(code)
            )));
        }
        assert!(suggests_compatible(&enhanced(
            ConnectionPhase::Contended,
            Some(ErrorCode::NetworkPathContended)
        )));
        assert!(!suggests_compatible(&enhanced(
            ConnectionPhase::Contended,
            Some(ErrorCode::ServiceBusy)
        )));
        assert!(!suggests_compatible(&enhanced(
            ConnectionPhase::Error,
            Some(ErrorCode::ConnectFailed)
        )));
        assert!(!suggests_compatible(&enhanced(ConnectionPhase::Off, None)));
    }

    #[test]
    fn compose_follows_the_selected_mode() {
        let parts = ConnectionParts {
            enhanced: EnhancedState {
                phase: ConnectionPhase::Error,
                reason: Some(ClientErrorInfo::new(
                    ErrorCode::ServiceInstallCancelled,
                    "x",
                )),
                retryable: true,
                service_installed: false,
                can_take_over: false,
                competitors: Vec::new(),
            },
            compat: CompatState {
                phase: ConnectionPhase::On,
                reason: None,
                retryable: false,
                proxy_was_foreign: true,
                proxy_owner: Some("Surge".into()),
            },
            detail: ConnectionDetail {
                latency_ms: Some(38),
                ..ConnectionDetail::default()
            },
            enhanced_competitor: Some("Clash Verge".into()),
        };
        let enhanced = compose(ConnectionMode::Enhanced, &parts);
        assert_eq!(enhanced.phase, ConnectionPhase::Error);
        assert!(enhanced.suggest_compatible);
        let compatible = compose(ConnectionMode::Compatible, &parts);
        assert_eq!(compatible.phase, ConnectionPhase::On);
        assert!(!compatible.suggest_compatible);
        assert!(compatible.proxy_was_foreign);
        assert_eq!(compatible.competitor.as_deref(), Some("Surge"));
        // Enhanced failed on the service install, not the path: no name.
        assert_eq!(enhanced.competitor, None);
        let mut parts = parts;
        parts.enhanced.reason = Some(ClientErrorInfo::new(
            ErrorCode::NetworkPathContended,
            "NETWORK_PATH_CONTENDED",
        ));
        parts.enhanced.phase = ConnectionPhase::Contended;
        assert_eq!(
            compose(ConnectionMode::Enhanced, &parts)
                .competitor
                .as_deref(),
            Some("Clash Verge")
        );
        // Surge's enhanced mode on macOS: the health check fails on the
        // direct path / TUN capture (ConnectHealthCheckFailed); still named.
        parts.enhanced.reason = Some(ClientErrorInfo::new(
            ErrorCode::ConnectHealthCheckFailed,
            "HEALTH_CAPTURE_PATH_FAILED",
        ));
        parts.enhanced.phase = ConnectionPhase::Error;
        assert!(is_path_problem(&parts.enhanced));
        let failed = compose(ConnectionMode::Enhanced, &parts);
        assert_eq!(failed.competitor.as_deref(), Some("Clash Verge"));
        assert!(failed.suggest_compatible);
        parts.enhanced.reason = Some(ClientErrorInfo::new(
            ErrorCode::ConnectFailed,
            "HEALTH_PROXY_PATH_FAILED",
        ));
        assert!(is_path_problem(&parts.enhanced));
        // A node / tunnel failure that is not a health step: no name.
        parts.enhanced.reason = Some(ClientErrorInfo::new(ErrorCode::ConnectFailed, "x"));
        assert!(!is_path_problem(&parts.enhanced));
        assert_eq!(compose(ConnectionMode::Enhanced, &parts).competitor, None);
        parts.enhanced.phase = ConnectionPhase::On;
        parts.enhanced.reason = None;
        assert_eq!(compose(ConnectionMode::Enhanced, &parts).competitor, None);
        assert_eq!(compatible.detail.latency_ms, Some(38));
    }

    #[test]
    fn a_detected_competitor_suggests_compatible_for_any_failure() {
        let mut parts = ConnectionParts {
            enhanced: EnhancedState {
                phase: ConnectionPhase::Error,
                reason: Some(ClientErrorInfo::new(
                    ErrorCode::ConnectFailed,
                    "HEALTH_ENTRANCE_FAILED: probe timed out",
                )),
                retryable: true,
                ..EnhancedState::default()
            },
            enhanced_competitor: Some("Mihomo".into()),
            ..ConnectionParts::default()
        };
        // The core's entrance probe failing is a path problem: named.
        assert!(is_path_problem(&parts.enhanced));
        let state = compose(ConnectionMode::Enhanced, &parts);
        assert!(state.suggest_compatible);
        assert_eq!(state.competitor.as_deref(), Some("Mihomo"));
        parts.enhanced.reason = Some(ClientErrorInfo::new(
            ErrorCode::ConnectFailed,
            "PROFILE_UPDATE_FAILED: HEALTH_ENTRANCE_FAILED",
        ));
        assert!(is_path_problem(&parts.enhanced));

        // Any other enhanced failure: compatible suggested, not named.
        parts.enhanced.reason = Some(ClientErrorInfo::new(
            ErrorCode::ConnectFailed,
            "CORE_NOT_RUNNING",
        ));
        let state = compose(ConnectionMode::Enhanced, &parts);
        assert!(state.suggest_compatible);
        assert_eq!(state.competitor, None);
        // Without a competitor that failure suggests nothing.
        parts.enhanced_competitor = None;
        assert!(!compose(ConnectionMode::Enhanced, &parts).suggest_compatible);

        // Owned by another session: not a failure, never suggested.
        parts.enhanced_competitor = Some("Mihomo".into());
        parts.enhanced.phase = ConnectionPhase::Contended;
        parts.enhanced.reason = Some(ClientErrorInfo::new(ErrorCode::ServiceBusy, "x"));
        assert!(!is_failure(&parts.enhanced));
        assert!(!compose(ConnectionMode::Enhanced, &parts).suggest_compatible);
        // Not failed at all.
        parts.enhanced.phase = ConnectionPhase::On;
        parts.enhanced.reason = None;
        assert!(!compose(ConnectionMode::Enhanced, &parts).suggest_compatible);
    }

    #[test]
    fn mode_moves_out_of_the_apps_settings_file_once() {
        let dir = crate::test_backend::temp_dir("ppvpn-mode");
        std::fs::create_dir_all(&dir).unwrap();
        let app_file = Path::new(&dir).join(LEGACY_SETTINGS_FILE);
        let app_settings = r#"{"theme":"dark","connection_mode":"compatible"}"#;
        std::fs::write(&app_file, app_settings).unwrap();

        assert_eq!(load_mode(&dir), ConnectionMode::Compatible);
        assert!(Path::new(&dir).join(CLIENT_SETTINGS_FILE).is_file());
        // The app's file is untouched, then and on later saves.
        save_mode(&dir, ConnectionMode::Enhanced).unwrap();
        assert_eq!(std::fs::read_to_string(&app_file).unwrap(), app_settings);
        assert_eq!(load_mode(&dir), ConnectionMode::Enhanced);

        // An app file without the key migrates nothing.
        let other = crate::test_backend::temp_dir("ppvpn-mode");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(
            Path::new(&other).join(LEGACY_SETTINGS_FILE),
            r#"{"theme":"dark"}"#,
        )
        .unwrap();
        assert_eq!(load_mode(&other), ConnectionMode::Enhanced);
        assert!(!Path::new(&other).join(CLIENT_SETTINGS_FILE).exists());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(other);
    }

    #[test]
    fn mode_persists_and_defaults_to_enhanced() {
        let dir = std::env::temp_dir()
            .join(format!("ppvpn-mode-{}", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        assert_eq!(load_mode(&dir), ConnectionMode::Enhanced);
        save_mode(&dir, ConnectionMode::Compatible).unwrap();
        assert_eq!(load_mode(&dir), ConnectionMode::Compatible);
        save_mode(&dir, ConnectionMode::Enhanced).unwrap();
        assert_eq!(load_mode(&dir), ConnectionMode::Enhanced);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn routing_mode_persists_next_to_the_connection_mode() {
        let dir = std::env::temp_dir()
            .join(format!("ppvpn-routing-{}", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        assert_eq!(load_routing_mode(&dir), crate::RoutingMode::Rules);
        save_mode(&dir, ConnectionMode::Compatible).unwrap();
        save_routing_mode(&dir, crate::RoutingMode::Global).unwrap();
        assert_eq!(load_routing_mode(&dir), crate::RoutingMode::Global);
        assert_eq!(load_mode(&dir), ConnectionMode::Compatible, "kept");
        save_routing_mode(&dir, crate::RoutingMode::Rules).unwrap();
        assert_eq!(load_routing_mode(&dir), crate::RoutingMode::Rules);
        let _ = std::fs::remove_dir_all(dir);
    }
}
