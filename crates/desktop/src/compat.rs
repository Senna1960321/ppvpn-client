//! Compatible mode: the OS system proxy points at the standard core's
//! unauthenticated loopback endpoint (`set-system-proxy`), which follows the
//! selected node. The endpoint exists only while compatible mode is
//! connected; enhanced mode never opens it.
//!
//! Before the OS settings are written, the previous ones are saved to
//! `<data_dir>/system-proxy-backup.json`; disconnect, sign-out, shutdown and
//! the next launch after a crash put them back and delete the file, unless
//! another app has pointed the OS proxy elsewhere since (then its settings
//! stay). While on, the OS settings are checked at every monitor tick: once
//! they no longer point at the endpoint, compatible mode ends in `Error`
//! (NetworkPathContended, naming the app when it can be told) and leaves
//! them alone.
//!
//! Settings that point at our own endpoint are never saved or restored as
//! the user's (#536): they are recorded and put back switched off. The
//! endpoint's port is kept in `<data_dir>/system-proxy-endpoint.json`, so
//! enhanced mode and the next launch can switch off an OS proxy still
//! pointing at that loopback port once nothing listens there.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core_ipc::{self, CoreCallError, CoreTransport};
use crate::errors::{ClientError, ClientErrorInfo, ErrorCode};
use crate::standard::StandardCore;
use crate::sysproxy::SystemProxyWriter;
use crate::{ClientConfig, ConnectionPhase, StandardState};

const BACKUP_FILE: &str = "system-proxy-backup.json";
const ENDPOINT_FILE: &str = "system-proxy-endpoint.json";
/// The standard core's preferred endpoint port, checked as well when no
/// port was recorded (clients before #536).
const PREFERRED_PORT: u16 = 7891;
/// How long connect waits for the standard core to become ready.
const READY_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(15)
};

/// Compatible mode as the snapshot shows it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CompatState {
    /// Off / Connecting / On / Disconnecting / Error.
    pub phase: ConnectionPhase,
    pub reason: Option<ClientErrorInfo>,
    pub retryable: bool,
    /// When connecting, the OS proxy already pointed somewhere else
    /// (another app); it is restored on disconnect.
    pub proxy_was_foreign: bool,
    /// The app that owned that proxy, when it could be told.
    pub proxy_owner: Option<String>,
}

/// The OS proxy that was active before compatible mode took over.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ForeignProxy {
    pub owner: Option<String>,
}

pub(crate) type StateSink = Arc<dyn Fn(CompatState) + Send + Sync>;

#[derive(Serialize, Deserialize)]
struct Backup {
    version: u32,
    saved: Value,
    /// The endpoint the OS settings were last pointed at (absent in backups
    /// written before this field: those are restored unconditionally).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<(String, u16)>,
}

struct Data {
    state: CompatState,
    /// Endpoint the OS settings currently point at.
    endpoint: Option<(String, u16)>,
    /// Another app's proxy found when connecting (kept while connected).
    foreign: Option<ForeignProxy>,
    /// Another app that pointed the OS proxy elsewhere while on (kept
    /// while in the resulting `Error`).
    taken_over_by: Option<ForeignProxy>,
}

struct Inner {
    writer: Arc<dyn SystemProxyWriter>,
    standard: StandardCore,
    backup_path: PathBuf,
    endpoint_path: PathBuf,
    on_state: StateSink,
    operation: tokio::sync::Mutex<()>,
    data: Mutex<Data>,
}

/// Compatible-mode controller; cheap to clone.
#[derive(Clone)]
pub(crate) struct Compat {
    inner: Arc<Inner>,
}

fn failed(detail: impl Into<String>, retryable: bool) -> (ClientErrorInfo, bool) {
    (
        ClientErrorInfo::new(ErrorCode::SystemProxyFailed, detail),
        retryable,
    )
}

/// Maps a failure of the OS proxy writer: a desktop without proxy settings
/// is `SystemProxyUnavailable`, anything else `SystemProxyFailed`.
fn os_failure(detail: String) -> ClientErrorInfo {
    let code = if detail.starts_with("NO_DESKTOP_PROXY_SETTINGS") {
        ErrorCode::SystemProxyUnavailable
    } else {
        ErrorCode::SystemProxyFailed
    };
    ClientErrorInfo::new(code, detail)
}

/// Maps a `set-system-proxy` failure.
fn endpoint_failure(error: &CoreCallError) -> (ClientErrorInfo, bool) {
    match error.code() {
        Some("SYSTEM_PROXY_UNAVAILABLE") => failed(error.detail(), false),
        _ => failed(error.detail(), true),
    }
}

impl Compat {
    pub(crate) fn new(
        config: &ClientConfig,
        writer: Arc<dyn SystemProxyWriter>,
        standard: StandardCore,
        on_state: StateSink,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                writer,
                standard,
                backup_path: PathBuf::from(&config.data_dir).join(BACKUP_FILE),
                endpoint_path: PathBuf::from(&config.data_dir).join(ENDPOINT_FILE),
                on_state,
                operation: tokio::sync::Mutex::new(()),
                data: Mutex::new(Data {
                    state: CompatState::default(),
                    endpoint: None,
                    foreign: None,
                    taken_over_by: None,
                }),
            }),
        }
    }

    /// Enables the loopback endpoint and points the OS proxy at it.
    pub(crate) async fn connect(&self) -> Result<(), ClientError> {
        let inner = &self.inner;
        let _operation = inner.operation.lock().await;
        if inner.with_data(|data| data.state.phase) == ConnectionPhase::On {
            return Ok(());
        }
        inner.with_data(|data| data.taken_over_by = None);
        inner.set_state(ConnectionPhase::Connecting, None, false);
        match inner.bring_up().await {
            Ok((endpoint, foreign)) => {
                tracing::info!(
                    "compatible mode: on, system proxy {}:{}",
                    endpoint.0,
                    endpoint.1
                );
                inner.with_data(|data| {
                    data.endpoint = Some(endpoint);
                    if let Some(foreign) = foreign {
                        data.foreign = Some(foreign);
                    }
                });
                inner.set_state(ConnectionPhase::On, None, false);
                Ok(())
            }
            Err((info, retryable)) => {
                tracing::info!("compatible mode: failed ({:?})", info.code);
                let _ = inner.tear_down().await;
                inner.set_state(ConnectionPhase::Error, Some(info.clone()), retryable);
                Err(info.into())
            }
        }
    }

    /// Restores the previous OS settings and disables the endpoint. Always
    /// ends `Off`; a failed restore is returned (and retried next launch).
    pub(crate) async fn disconnect(&self) -> Result<(), ClientError> {
        let inner = &self.inner;
        let _operation = inner.operation.lock().await;
        let phase = inner.with_data(|data| data.state.phase);
        if phase == ConnectionPhase::Off && !inner.has_backup().await {
            return Ok(());
        }
        if phase != ConnectionPhase::Off {
            inner.set_state(ConnectionPhase::Disconnecting, None, false);
        }
        let result = inner.tear_down().await;
        inner.with_data(|data| {
            data.endpoint = None;
            data.foreign = None;
            data.taken_over_by = None;
        });
        inner.set_state(ConnectionPhase::Off, None, false);
        tracing::info!("compatible mode: off");
        result.map_err(ClientError::from)
    }

    /// Launch-time cleanup after a crash: puts back OS settings a previous
    /// run left pointing at a dead endpoint.
    pub(crate) async fn restore_leftovers(&self) {
        let inner = &self.inner;
        let _operation = inner.operation.lock().await;
        if inner.has_backup().await {
            tracing::info!("compatible mode: restoring system proxy settings left by a crash");
            if let Err(info) = inner.restore_os().await {
                tracing::warn!("compatible mode: restore failed ({})", info.detail);
            }
        }
        inner.clear_dead_endpoint().await;
    }

    /// While off (enhanced mode connecting): switches off an OS proxy that
    /// still points at our loopback port with nothing listening there
    /// (#536). Another app's proxy, or one that answers, is left alone.
    pub(crate) async fn clear_stale_proxy(&self) {
        let inner = &self.inner;
        let _operation = inner.operation.lock().await;
        if inner.with_data(|data| data.state.phase) != ConnectionPhase::Off {
            return;
        }
        inner.clear_dead_endpoint().await;
    }

    /// While connected: after a standard-core restart the endpoint comes up
    /// disabled (and possibly on another port); re-enable it and rewrite the
    /// OS settings when the port moved, only while the previous endpoint is
    /// still ours.
    pub(crate) async fn reconcile(&self) {
        let inner = &self.inner;
        let Ok(_operation) = inner.operation.try_lock() else {
            return;
        };
        if inner.with_data(|data| data.state.phase) != ConnectionPhase::On {
            return;
        }
        let Some(previous) = inner.with_data(|data| data.endpoint.clone()) else {
            return;
        };
        // Check the endpoint we last wrote, before enabling its replacement.
        // A failed query also leaves the OS settings alone until a later tick.
        if !inner.check_ownership(&previous).await {
            return;
        }
        let Ok(transport) = inner.standard.transport() else {
            return; // Restarting; the next tick retries.
        };
        let status = core_ipc::get_status(transport.as_ref())
            .await
            .ok()
            .and_then(|status| status.system_proxy);
        let current = match status.as_ref().and_then(|status| status.endpoint()) {
            Some(endpoint) => endpoint,
            None => match core_ipc::set_system_proxy(transport.as_ref(), true).await {
                Ok(status) => match status.endpoint() {
                    Some(endpoint) => endpoint,
                    None => return,
                },
                Err(error) => {
                    tracing::warn!("compatible mode: re-enable failed ({})", error.detail());
                    return;
                }
            },
        };
        if previous == current {
            return;
        }
        // Core IPC can take time: another app may have taken over while
        // status or re-enable was pending. Recheck before moving the OS proxy.
        if !inner.check_ownership(&previous).await {
            return;
        }
        tracing::info!(
            "compatible mode: endpoint moved to {}:{}, rewriting system proxy",
            current.0,
            current.1
        );
        match inner.apply_os(&current).await {
            Ok(_) => inner.with_data(|data| data.endpoint = Some(current)),
            Err(info) => {
                inner.set_state(ConnectionPhase::Error, Some(info), true);
            }
        }
    }
}

impl Inner {
    fn with_data<T>(&self, change: impl FnOnce(&mut Data) -> T) -> T {
        let mut guard = self.data.lock().unwrap_or_else(|p| p.into_inner());
        change(&mut guard)
    }

    fn set_state(&self, phase: ConnectionPhase, reason: Option<ClientErrorInfo>, retryable: bool) {
        let (foreign, taken_over_by) = self.with_data(|data| match phase {
            ConnectionPhase::On => (data.foreign.clone(), None),
            ConnectionPhase::Error => (None, data.taken_over_by.clone()),
            _ => (None, None),
        });
        let state = CompatState {
            phase,
            reason,
            retryable,
            proxy_was_foreign: foreign.is_some(),
            proxy_owner: foreign.or(taken_over_by).and_then(|foreign| foreign.owner),
        };
        let changed = self.with_data(|data| {
            let changed = data.state != state;
            data.state = state.clone();
            changed
        });
        if changed {
            (self.on_state)(state);
        }
    }

    /// The ready standard core's transport. A core that refused the profile
    /// fails at once with that (#532): nothing would start it within the
    /// wait, and it is no proxy setting's fault.
    async fn ready_transport(&self) -> Result<Arc<dyn CoreTransport>, (ClientErrorInfo, bool)> {
        let deadline = tokio::time::Instant::now() + READY_WAIT;
        loop {
            if let Ok(transport) = self.standard.transport() {
                return Ok(transport);
            }
            if let StandardState::Failed { error } = self.standard.state() {
                if crate::cores::is_profile_rejection(&error) {
                    return Err((error, false));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(failed("STANDARD_CORE_NOT_READY", true));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn bring_up(
        &self,
    ) -> Result<((String, u16), Option<ForeignProxy>), (ClientErrorInfo, bool)> {
        let transport = self.ready_transport().await?;
        let status = core_ipc::set_system_proxy(transport.as_ref(), true)
            .await
            .map_err(|error| endpoint_failure(&error))?;
        if !status.available {
            return Err(failed("SYSTEM_PROXY_UNAVAILABLE", false));
        }
        let endpoint = status
            .endpoint()
            .ok_or_else(|| failed("SYSTEM_PROXY_NOT_LISTENING", true))?;
        let previous = self.apply_os(&endpoint).await.map_err(|info| {
            let retryable = info.code != ErrorCode::SystemProxyUnavailable;
            (info, retryable)
        })?;
        let foreign = match previous {
            Some((host, port))
                if !(crate::sysproxy::is_loopback_host(&host) && port == endpoint.1) =>
            {
                let owner = if crate::sysproxy::is_loopback_host(&host) {
                    lookup_listener(port).await
                } else {
                    None
                };
                tracing::info!(
                    "compatible mode: the OS proxy pointed at another proxy ({host}:{port}, owner {}); \
                     it is restored on disconnect",
                    owner.as_deref().unwrap_or("unknown")
                );
                Some(ForeignProxy { owner })
            }
            _ => None,
        };
        Ok((endpoint, foreign))
    }

    /// While on: when the OS settings no longer point at `endpoint`, another
    /// app took the system proxy over. Its settings stay (the backup is
    /// dropped), the endpoint is disabled and compatible mode ends in
    /// `Error` naming that app when it can be told. A failed check changes
    /// nothing (the next tick retries). Only confirmed ownership returns true.
    async fn check_ownership(&self, endpoint: &(String, u16)) -> bool {
        let writer = self.writer.clone();
        let Ok(Ok(proxies)) = tokio::task::spawn_blocking(move || writer.current_proxies()).await
        else {
            return false;
        };
        if crate::sysproxy::points_at(&proxies, &endpoint.0, endpoint.1) {
            return true;
        }
        let now = proxies.first().cloned();
        let owner = match &now {
            Some((host, port)) if crate::sysproxy::is_loopback_host(host) => {
                lookup_listener(*port).await
            }
            _ => None,
        };
        tracing::warn!(
            "compatible mode: the system proxy was changed by another app ({}, owner {}); leaving it",
            now.as_ref()
                .map_or_else(|| "off".to_string(), |(host, port)| format!("{host}:{port}")),
            owner.as_deref().unwrap_or("unknown")
        );
        let path = self.backup_path.clone();
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_file(path)).await;
        if let Ok(transport) = self.standard.transport() {
            let _ = core_ipc::set_system_proxy(transport.as_ref(), false).await;
        }
        self.with_data(|data| {
            data.endpoint = None;
            data.foreign = None;
            data.taken_over_by = Some(ForeignProxy { owner });
        });
        self.set_state(
            ConnectionPhase::Error,
            Some(ClientErrorInfo::new(
                ErrorCode::NetworkPathContended,
                "SYSTEM_PROXY_TAKEN_OVER",
            )),
            true,
        );
        false
    }

    /// [`Compat::clear_stale_proxy`] without the lock and phase check.
    async fn clear_dead_endpoint(&self) {
        let writer = self.writer.clone();
        let endpoint_path = self.endpoint_path.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<Option<u16>, String> {
            let mut ports = vec![PREFERRED_PORT];
            if let Some(port) = std::fs::read(&endpoint_path)
                .ok()
                .and_then(|raw| serde_json::from_slice::<u16>(&raw).ok())
            {
                ports.insert(0, port);
            }
            let proxies = writer.current_proxies()?;
            let Some(port) = ports.into_iter().find(|port| {
                crate::sysproxy::points_at(&proxies, "127.0.0.1", *port) && !listening(*port)
            }) else {
                return Ok(None);
            };
            let now = writer.snapshot()?;
            writer.restore(&crate::sysproxy::switch_off(&now, port))?;
            Ok(Some(port))
        })
        .await;
        match result {
            Ok(Ok(Some(port))) => tracing::warn!(
                "compatible mode: the system proxy still pointed at our loopback port {port} \
                 with nothing listening; switched it off"
            ),
            Ok(Ok(None)) => {}
            Ok(Err(detail)) => {
                tracing::warn!("compatible mode: clearing a stale system proxy failed ({detail})")
            }
            Err(error) => tracing::warn!("compatible mode: {error}"),
        }
    }

    /// Restores the OS settings, then disables the endpoint (best effort).
    async fn tear_down(&self) -> Result<(), ClientErrorInfo> {
        let restored = self.restore_os().await;
        if let Ok(transport) = self.standard.transport() {
            if let Err(error) = core_ipc::set_system_proxy(transport.as_ref(), false).await {
                tracing::info!(
                    "compatible mode: disable endpoint failed ({})",
                    error.detail()
                );
            }
        }
        restored
    }

    async fn has_backup(&self) -> bool {
        let path = self.backup_path.clone();
        tokio::task::spawn_blocking(move || path.is_file())
            .await
            .unwrap_or(false)
    }

    /// Saves the current OS settings (unless a backup already exists: those
    /// are the user's own), then points them at `endpoint`. Saved settings
    /// that already point at the endpoint's port are recorded switched off.
    /// Returns the proxy that was active in the settings saved now (`None`
    /// when a backup already existed or the OS proxy was off).
    async fn apply_os(
        &self,
        endpoint: &(String, u16),
    ) -> Result<Option<(String, u16)>, ClientErrorInfo> {
        let writer = self.writer.clone();
        let path = self.backup_path.clone();
        let endpoint_path = self.endpoint_path.clone();
        let (host, port) = endpoint.clone();
        tokio::task::spawn_blocking(move || -> Result<Option<(String, u16)>, String> {
            let mut previous = None;
            let endpoint = Some((host.clone(), port));
            let backup = match std::fs::read(&path) {
                // Those are the user's own settings: keep them, only note
                // where ours point now.
                Ok(raw) => serde_json::from_slice::<Backup>(&raw)
                    .ok()
                    .map(|backup| Backup { endpoint, ..backup }),
                Err(_) => {
                    let now = writer.snapshot()?;
                    let saved = crate::sysproxy::switch_off(&now, port);
                    if saved != now {
                        tracing::info!(
                            "compatible mode: the OS proxy already pointed at our port {port}; \
                             saved as off"
                        );
                    }
                    previous = crate::sysproxy::active_proxy(&saved);
                    Some(Backup {
                        version: 1,
                        saved,
                        endpoint,
                    })
                }
            };
            if let Some(backup) = backup {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
                }
                let raw = serde_json::to_vec(&backup).map_err(|error| error.to_string())?;
                let tmp = path.with_extension("json.tmp");
                std::fs::write(&tmp, raw).map_err(|error| format!("save backup: {error}"))?;
                std::fs::rename(&tmp, &path).map_err(|error| format!("save backup: {error}"))?;
            }
            if let Err(error) = std::fs::write(&endpoint_path, port.to_string()) {
                tracing::info!("compatible mode: record endpoint port: {error}");
            }
            writer.apply(&host, port).map(|()| previous)
        })
        .await
        .map_err(|error| ClientErrorInfo::new(ErrorCode::Internal, error.to_string()))?
        .map_err(os_failure)
    }

    /// Puts the saved OS settings back and deletes the backup.
    async fn restore_os(&self) -> Result<(), ClientErrorInfo> {
        let writer = self.writer.clone();
        let path = self.backup_path.clone();
        tokio::task::spawn_blocking(move || restore_backup(writer.as_ref(), &path))
            .await
            .map_err(|error| ClientErrorInfo::new(ErrorCode::Internal, error.to_string()))?
            .map_err(os_failure)
    }
}

/// [`Inner::restore_os`], blocking: puts the backup's settings back unless
/// another app pointed the OS proxy elsewhere since (its settings stay; a
/// failed check restores as before), then deletes the backup. Saved
/// settings that point at the backup's endpoint go back switched off.
fn restore_backup(writer: &dyn SystemProxyWriter, path: &std::path::Path) -> Result<(), String> {
    let Ok(raw) = std::fs::read(path) else {
        return Ok(());
    };
    match serde_json::from_slice::<Backup>(&raw) {
        Ok(backup) => {
            let replaced = backup.endpoint.as_ref().is_some_and(|(host, port)| {
                writer
                    .current_proxies()
                    .is_ok_and(|proxies| !crate::sysproxy::points_at(&proxies, host, *port))
            });
            if replaced {
                tracing::info!(
                    "compatible mode: system proxy changed by another app since connect; leaving it"
                );
            } else {
                let saved = match &backup.endpoint {
                    Some((_, port)) => crate::sysproxy::switch_off(&backup.saved, *port),
                    None => backup.saved.clone(),
                };
                if saved != backup.saved {
                    tracing::info!(
                        "compatible mode: the saved system proxy pointed at our endpoint; \
                         restored as off"
                    );
                }
                writer.restore(&saved)?;
            }
        }
        Err(error) => tracing::warn!("unreadable system proxy backup dropped: {error}"),
    }
    std::fs::remove_file(path).map_err(|error| format!("remove backup: {error}"))
}

/// Whether anything accepts connections on loopback `port`.
fn listening(port: u16) -> bool {
    let timeout = Duration::from_millis(300);
    [
        std::net::IpAddr::from([127, 0, 0, 1]),
        std::net::IpAddr::from(std::net::Ipv6Addr::LOCALHOST),
    ]
    .into_iter()
    .any(|ip| std::net::TcpStream::connect_timeout(&(ip, port).into(), timeout).is_ok())
}

/// The app listening on a loopback proxy port (best effort; never probes in
/// tests).
async fn lookup_listener(port: u16) -> Option<String> {
    if cfg!(test) {
        return None;
    }
    tokio::task::spawn_blocking(move || crate::detect::loopback_listener(port))
        .await
        .ok()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sysproxy::tests::FakeWriter;

    fn backup_at(endpoint: Option<(&str, u16)>) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("ppvpn-compat-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(BACKUP_FILE);
        let backup = Backup {
            version: 1,
            saved: serde_json::json!({"fake": "previous"}),
            endpoint: endpoint.map(|(host, port)| (host.to_string(), port)),
        };
        std::fs::write(&path, serde_json::to_vec(&backup).unwrap()).unwrap();
        path
    }

    fn restores(writer: &FakeWriter) -> usize {
        writer
            .calls()
            .iter()
            .filter(|call| call.starts_with("restore"))
            .count()
    }

    #[test]
    fn a_proxy_another_app_set_since_connect_is_left_alone() {
        let writer = FakeWriter::default();
        *writer.current.lock().unwrap() = Some(vec![("127.0.0.1".into(), 6152)]);
        let path = backup_at(Some(("127.0.0.1", 7891)));
        restore_backup(&writer, &path).unwrap();
        assert_eq!(restores(&writer), 0, "Surge's settings stay");
        assert!(!path.exists(), "backup dropped");

        // Still ours (any loopback name): restored.
        *writer.current.lock().unwrap() = Some(vec![("localhost".into(), 7891)]);
        let path = backup_at(Some(("127.0.0.1", 7891)));
        restore_backup(&writer, &path).unwrap();
        assert_eq!(restores(&writer), 1);
        assert!(!path.exists());

        // A backup from before the endpoint was recorded: restored as ever.
        *writer.current.lock().unwrap() = Some(vec![("127.0.0.1".into(), 6152)]);
        let path = backup_at(None);
        restore_backup(&writer, &path).unwrap();
        assert_eq!(restores(&writer), 2);
    }

    #[test]
    fn saved_settings_pointing_at_our_endpoint_go_back_off() {
        let writer = FakeWriter::default();
        let ours = serde_json::json!({"windows": {"ProxyEnable": "0x1",
            "ProxyServer": "127.0.0.1:7891", "ProxyOverride": "<local>"}});
        let path = backup_at(Some(("127.0.0.1", 7891)));
        let backup = Backup {
            version: 1,
            saved: ours,
            endpoint: Some(("127.0.0.1".into(), 7891)),
        };
        std::fs::write(&path, serde_json::to_vec(&backup).unwrap()).unwrap();
        *writer.current.lock().unwrap() = Some(vec![("127.0.0.1".into(), 7891)]);
        restore_backup(&writer, &path).unwrap();
        let off = serde_json::json!({"windows": {"ProxyEnable": "0x0",
            "ProxyServer": "127.0.0.1:7891", "ProxyOverride": "<local>"}});
        assert_eq!(writer.calls(), vec![format!("restore {off}")]);
        assert!(!path.exists());
    }

    #[test]
    fn a_loopback_port_is_listening_only_while_bound() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(listening(port));
        drop(listener);
        assert!(!listening(port));
    }

    #[test]
    fn old_backups_without_an_endpoint_still_parse() {
        let backup: Backup =
            serde_json::from_str(r#"{"version":1,"saved":{"fake":"previous"}}"#).unwrap();
        assert!(backup.endpoint.is_none());
    }
}

#[cfg(test)]
#[path = "compat/reconcile_tests.rs"]
mod reconcile_tests;
