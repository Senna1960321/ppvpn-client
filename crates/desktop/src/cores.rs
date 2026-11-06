//! Glue between the signed-in session and the two cores: the standard-mode
//! core ([`crate::standard`]) and enhanced mode ([`crate::enhanced`]).
//!
//! The session code calls the integration hooks here without holding locks;
//! everything that touches a core runs on the client's runtime under
//! [`Client::cores`] so reconfigurations never interleave. Each run re-reads
//! the *current* profile, so the order in which hooks fire does not matter.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
use std::time::Duration;

use crate::enhanced::{Enhanced, EnhancedConfig};
use crate::errors::{ClientError, ClientErrorInfo, ErrorCode};
use crate::service::ServiceApi;
use crate::session::{ClientRef, SelectionSource};
use crate::standard::{CoreLauncher, StandardCore, StandardSettings};
use crate::tunnel_mode::{TunnelConfig, TunnelMode};
use crate::StandardState;
use crate::{Client, ClientConfig, PlatformHooks};

/// Android and iOS: the connection is the system VPN, whose process runs the
/// engine (tunnel mode, [`crate::tunnel_mode`]), in place of enhanced mode.
pub(crate) const PHONE: bool = cfg!(any(target_os = "android", target_os = "ios"));

/// State guarded by [`Client::cores`].
#[derive(Default)]
pub(crate) struct CoreSync {
    /// Team whose profile the standard core runs; a different team restarts
    /// the core instead of hot-applying.
    applied_team: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Trigger {
    ProfileChanged,
    SessionEnded,
}

/// Standard-core failures a later standard-core success clears.
fn is_standard_error(info: &ClientErrorInfo) -> bool {
    matches!(
        info.code,
        ErrorCode::StandardCoreFailed | ErrorCode::ProfileExpired
    )
}

/// A core refused the profile itself (as opposed to failing to run).
pub(crate) fn is_profile_rejection(info: &ClientErrorInfo) -> bool {
    matches!(
        info.code,
        ErrorCode::ProfileInvalid | ErrorCode::ProfileExpired
    )
}

/// Enhanced-mode failures a later successful connection clears.
pub(crate) fn is_enhanced_error(info: &ClientErrorInfo) -> bool {
    matches!(
        info.code,
        ErrorCode::ServiceInstallCancelled
            | ErrorCode::ServiceInstallFailed
            | ErrorCode::ServiceUnavailable
            | ErrorCode::ServiceIncompatible
            | ErrorCode::ServiceBusy
            | ErrorCode::ServiceOwnedByAnotherUser
            | ErrorCode::ServiceClientRejected
            | ErrorCode::ConnectFailed
            | ErrorCode::ConnectHealthCheckFailed
            | ErrorCode::NetworkPathContended
    )
}

/// Builds both core controllers with sinks that feed the client snapshot.
#[allow(clippy::too_many_arguments)] // one sink per core, plus the shared settings
pub(crate) fn build_cores(
    config: &ClientConfig,
    platform: Arc<dyn PlatformHooks>,
    service: Arc<dyn ServiceApi>,
    launcher: Arc<dyn CoreLauncher>,
    writer: Arc<dyn crate::sysproxy::SystemProxyWriter>,
    routing: crate::routing::RoutingModeCell,
    ingress_pins: crate::ingress::PinsCell,
    this: &Weak<Client>,
) -> (StandardCore, Enhanced, crate::compat::Compat) {
    // Both cores apply every profile with the current selection.
    let weak_selection = this.clone();
    let selection = SelectionSource::new(move || {
        ClientRef::upgrade(&weak_selection).and_then(|client| client.selected_node())
    });
    let weak = this.clone();
    let weak_reset = this.clone();
    let standard = StandardCore::with_settings(
        launcher,
        Arc::new(move |state| {
            if let Some(client) = ClientRef::upgrade(&weak) {
                client.on_standard_state(state);
            }
        }),
        StandardSettings {
            routing: routing.clone(),
            ingress_pins: ingress_pins.clone(),
            selection: selection.clone(),
            on_credentials_reset: Some(Arc::new(move || {
                if let Some(client) = ClientRef::upgrade(&weak_reset) {
                    client.on_local_proxy_credentials_reset();
                }
            })),
        },
    );
    let weak_state = this.clone();
    let weak_error = this.clone();
    let enhanced = Enhanced::new(
        EnhancedConfig {
            routing,
            ingress_pins,
            selection,
            ..EnhancedConfig::from_client(config)
        },
        service,
        platform,
        Some(standard.clone()),
        Arc::new(move |state| {
            if let Some(client) = ClientRef::upgrade(&weak_state) {
                client.on_enhanced_state(state);
            }
        }),
        Arc::new(move |info| {
            if let Some(client) = ClientRef::upgrade(&weak_error) {
                client.report_background(info);
            }
        }),
    );
    let weak_compat = this.clone();
    let compat = crate::compat::Compat::new(
        config,
        writer,
        standard.clone(),
        Arc::new(move |state| {
            if let Some(client) = ClientRef::upgrade(&weak_compat) {
                client.on_compat_state(state);
            }
        }),
    );
    (standard, enhanced, compat)
}

/// The phones' tunnel mode, with the host's socket in the data directory
/// (Android's `filesDir`, the App Group container on iOS); `None` elsewhere.
pub(crate) fn build_tunnel(
    config: &ClientConfig,
    routing: crate::routing::RoutingModeCell,
    ingress_pins: crate::ingress::PinsCell,
    this: &Weak<Client>,
) -> Option<TunnelMode> {
    if !PHONE {
        return None;
    }
    let weak_selection = this.clone();
    let weak_state = this.clone();
    Some(TunnelMode::new(
        std::path::Path::new(&config.data_dir),
        TunnelConfig {
            rule_set_hosts: crate::core_ipc::rule_set_hosts(&config.api_base),
            routing,
            ingress_pins,
            selection: SelectionSource::new(move || {
                ClientRef::upgrade(&weak_selection).and_then(|client| client.selected_node())
            }),
        },
        Arc::new(move |state| {
            if let Some(client) = ClientRef::upgrade(&weak_state) {
                client.on_enhanced_state(state);
            }
        }),
    ))
}

impl Client {
    // --- sinks ------------------------------------------------------------------

    fn on_standard_state(&self, state: StandardState) {
        self.update(|snapshot| {
            if snapshot.standard != state {
                match &state {
                    StandardState::Stopped => tracing::info!("standard core: stopped"),
                    StandardState::Starting => tracing::info!("standard core: starting"),
                    StandardState::Ready { revision } => {
                        tracing::info!("standard core: ready, revision {revision}")
                    }
                    StandardState::Failed { error } => {
                        tracing::info!("standard core: failed, {:?}", error.code)
                    }
                }
            }
            match &state {
                StandardState::Failed { error } => snapshot.last_error = Some(error.clone()),
                StandardState::Ready { .. } => {
                    if snapshot.last_error.as_ref().is_some_and(is_standard_error) {
                        snapshot.last_error = None;
                    }
                }
                StandardState::Stopped | StandardState::Starting => {}
            }
            snapshot.standard = state;
        });
    }

    /// A new standard core rebuilt the local proxy credentials: shown
    /// until dismissed, whatever later cores report.
    fn on_local_proxy_credentials_reset(&self) {
        tracing::info!("local proxy credentials reset: telling the user");
        self.update(|snapshot| snapshot.local_proxy_credentials_reset = true);
    }

    pub(crate) fn clear_local_proxy_credentials_reset(&self) {
        self.update(|snapshot| snapshot.local_proxy_credentials_reset = false);
    }

    /// A failure of background work: shown until the next success.
    pub(crate) fn report_background(&self, info: ClientErrorInfo) {
        self.update(|snapshot| snapshot.last_error = Some(info));
    }

    /// The outcome of a user action: success clears `last_error`, failure
    /// replaces it.
    pub(crate) fn finish_action<T>(
        &self,
        result: Result<T, ClientError>,
    ) -> Result<T, ClientError> {
        match result {
            Ok(value) => {
                let has_error = self
                    .snapshot
                    .lock()
                    .map(|snapshot| snapshot.last_error.is_some())
                    .unwrap_or(false);
                if has_error {
                    self.update(|snapshot| snapshot.last_error = None);
                }
                Ok(value)
            }
            Err(error) => {
                if let Some(info) = error.info() {
                    self.report_background(info);
                }
                Err(error)
            }
        }
    }

    // --- integration hooks (called by the session without locks) ----------------

    /// The session ended (sign-out, forced sign-out, or replaced by another
    /// sign-in): disconnect enhanced mode and stop the standard core.
    pub(crate) fn on_session_ended(&self) {
        self.spawn_sync(Trigger::SessionEnded);
    }

    /// A new profile revision (or the first one, or a new team's) is in
    /// [`Client::profile_raw`].
    pub(crate) fn on_profile_changed(&self) {
        self.spawn_sync(Trigger::ProfileChanged);
    }

    fn spawn_sync(&self, trigger: Trigger) {
        let weak = self.this.clone();
        self.runtime.spawn(async move {
            if let Some(client) = ClientRef::upgrade(&weak) {
                client.sync_cores(trigger).await;
            }
        });
    }

    /// Stops both cores now (sign-out awaits this).
    pub(crate) async fn stop_cores(&self) {
        self.sync_cores(Trigger::SessionEnded).await;
    }

    /// Brings both cores in line with the current session and profile.
    async fn sync_cores(&self, trigger: Trigger) {
        let mut sync = self.cores.lock().await;
        if trigger == Trigger::SessionEnded {
            self.disconnect_all("session end").await;
            self.standard.stop().await;
            sync.applied_team = None;
        }
        if self.shut_down.load(Ordering::SeqCst) {
            return;
        }
        let Some(current) = self.current_profile() else {
            // No usable profile (no subscription, team disabled, …): there is
            // nothing to route through.
            if trigger == Trigger::ProfileChanged {
                self.disconnect_all("no usable profile").await;
                self.standard.stop().await;
                sync.applied_team = None;
            }
            return;
        };
        if sync.applied_team.is_some() && sync.applied_team != current.team_id {
            // New team: start from a clean core rather than hot-applying.
            self.standard.stop().await;
        }
        sync.applied_team = current.team_id.clone();
        // Standard-core failures surface through its state sink; a rejected
        // update of a running core keeps it ready and is reported here. Both
        // cores get the selection and the pins with the profile.
        match self
            .standard
            .apply_profile(&current.raw, &current.revision)
            .await
        {
            Ok(()) => self.mark_core_accepted(&current),
            Err(error) => {
                if let Some(info) = error.info() {
                    if is_profile_rejection(&info) {
                        if self.mark_core_rejected(&current.revision, info) {
                            // Back on the earlier profile: the sync that
                            // follows hands it to both cores.
                            return;
                        }
                    } else if matches!(self.standard.state(), StandardState::Ready { .. }) {
                        self.report_background(info);
                    }
                }
            }
        }
        let enhanced = match &self.tunnel {
            Some(tunnel) => tunnel.set_profile(&current.raw, &current.revision).await,
            None => {
                self.enhanced
                    .set_profile(&current.raw, &current.revision)
                    .await
            }
        };
        if let Some(info) = enhanced.err().and_then(|error| error.info()) {
            if is_profile_rejection(&info) {
                self.mark_core_rejected(&current.revision, info);
            } else {
                self.report_background(info);
            }
        }
    }

    /// Launch-time enhanced-mode restore, after the saved sign-in restored.
    pub(crate) async fn restore_enhanced(&self) {
        if let Some(tunnel) = &self.tunnel {
            // The system VPN outlives the app; signed out, the app stops it
            // when the phase turns off.
            tunnel.restore().await;
            if !self.is_signed_in() {
                let _ = tunnel.disable().await;
            }
            return;
        }
        self.enhanced.restore().await;
        if self.connection_mode() == crate::ConnectionMode::Compatible {
            // Mutually exclusive: a session re-attached in compatible mode is
            // released.
            let _ = self.enhanced.disable().await;
        }
        if !self.is_signed_in() {
            // A connection left over from a signed-out session is released.
            let _ = self.enhanced.disable().await;
        }
    }

    /// Hands the latest profile to enhanced mode before a user action that
    /// needs it (the background sync may not have run yet).
    pub(crate) async fn prime_enhanced(&self) -> Result<(), ClientError> {
        // A disconnected controller retains its last profile. After a team
        // switch, that must not be used until the new team has a profile.
        let current = self.current_profile().ok_or_else(|| {
            ClientError::failed(ErrorCode::ProfileFetchFailed, "PROFILE_NOT_READY")
        })?;
        match &self.tunnel {
            Some(tunnel) => tunnel.set_profile(&current.raw, &current.revision).await?,
            None => {
                self.enhanced
                    .set_profile(&current.raw, &current.revision)
                    .await?
            }
        }
        Ok(())
    }

    pub(crate) fn selected_node(&self) -> Option<String> {
        self.snapshot
            .lock()
            .ok()
            .and_then(|snapshot| snapshot.selected_node_id.clone())
    }

    /// Selects `node_id` live on both cores (the user chose it): best effort
    /// on the standard core (its system-proxy endpoint follows the
    /// selection), and in enhanced mode while it is on (errors returned).
    /// The next apply or connect carries it anyway.
    pub(crate) async fn apply_selection(&self, node_id: &str) -> Result<(), ClientError> {
        self.select_standard_node(node_id).await;
        match &self.tunnel {
            Some(tunnel) => tunnel.select_node(node_id).await,
            None => self.enhanced.select_node(node_id).await,
        }
    }

    /// Best-effort `select-node` on the standard core (when it runs).
    pub(crate) async fn select_standard_node(&self, node_id: &str) {
        let Ok(transport) = self.standard.transport() else {
            return;
        };
        if let Err(error) = crate::core_ipc::select_node(transport.as_ref(), node_id).await {
            tracing::info!("standard core: select node failed ({})", error.detail());
        }
    }

    /// Tells both cores that the system goes to sleep (`asleep`) or woke,
    /// best effort: the standard core when it runs, the enhanced-mode core
    /// while it is on.
    pub(crate) async fn tell_cores_sleep(&self, asleep: bool) {
        let standard = async {
            let Ok(transport) = self.standard.transport() else {
                return;
            };
            if let Err(error) = crate::core_ipc::tell_sleep(transport.as_ref(), asleep).await {
                tracing::info!(asleep, "standard core: sleep not told ({})", error.detail());
            }
        };
        tokio::join!(standard, self.enhanced.tell_sleep(asleep));
    }

    /// Synchronous app-exit stop: releases the enhanced session and stops the
    /// standard core, waiting at most a few seconds.
    pub(crate) fn is_shut_down(&self) -> bool {
        self.shut_down.load(Ordering::SeqCst)
    }

    pub(crate) fn shutdown_cores(&self) {
        let started = std::time::Instant::now();
        tracing::info!("shutdown: start");
        self.shut_down.store(true, Ordering::SeqCst);
        {
            let mut state = self.session_state();
            if let Some(task) = state.notifications.task.take() {
                task.abort();
            }
            if let Some(task) = state.traffic_task.take() {
                task.abort();
            }
            if let Some(task) = state.monitor_task.take() {
                task.abort();
            }
        }
        if let Some(tunnel) = &self.tunnel {
            tunnel.shutdown();
        }
        let standard = self.standard.clone();
        let enhanced = self.enhanced.clone();
        let compat = self.compat.clone();
        let handle = self.runtime.handle().clone();
        // `block_on` must not run on a runtime thread; a helper thread is safe
        // wherever the app calls this from.
        let worker = std::thread::Builder::new()
            .name("ppvpn-client-shutdown".into())
            .spawn(move || {
                handle.block_on(async {
                    // OS proxy settings first: they must not outlive the app.
                    let _ = tokio::time::timeout(Duration::from_secs(5), compat.disconnect()).await;
                    let _ = tokio::time::timeout(Duration::from_secs(5), enhanced.shutdown()).await;
                    if tokio::time::timeout(Duration::from_secs(5), standard.stop())
                        .await
                        .is_err()
                    {
                        standard.shutdown();
                    }
                });
            });
        match worker {
            Ok(worker) => {
                let _ = worker.join();
            }
            Err(_) => self.standard.shutdown(),
        }
        tracing::info!("shutdown: done in {} ms", started.elapsed().as_millis());
    }
}

// ---------------------------------------------------------------------------
// Test support
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{json, Value};
    use tokio::sync::oneshot;

    use crate::core_ipc::tests::FakeCore;
    use crate::core_ipc::{BoxFuture, CoreCallError};
    use crate::errors::ClientErrorInfo;
    use crate::service::{ServiceApi, ServiceError, ServiceStatus, SessionRef, VersionInfo};
    use crate::standard::{CoreLauncher, Launched};

    /// A fake standard core: answers the Core API from memory and records
    /// every call. Probes succeed for known node ids.
    pub(crate) struct FakeLauncher {
        pub(crate) core: Arc<FakeCore>,
        pub(crate) launches: Mutex<usize>,
        /// OS proxy writer handed to the client in tests.
        pub(crate) writer: Arc<crate::sysproxy::tests::FakeWriter>,
        /// `(enabled, port)` of the core's system-proxy endpoint.
        pub(crate) system_proxy: Arc<Mutex<(bool, u16)>>,
        /// `apply-profile` fails while set (the core cannot run the profile).
        pub(crate) fail_apply: Arc<AtomicBool>,
        /// `apply-profile` refuses this revision as the engine refuses a rule
        /// set on another host (`RULE_SET_HOST_NOT_ALLOWED`).
        pub(crate) reject_revision: Arc<Mutex<Option<String>>>,
        /// `GetStatus.rule_sets`; omitted while `null`.
        pub(crate) rule_sets: Arc<Mutex<Value>>,
        /// Ingress pins the core holds (node id → endpoint key); reported in
        /// `GetStatus.nodes` like core 0.5.7. Replaced by every apply, like
        /// the engine.
        pub(crate) pins: Arc<Mutex<std::collections::BTreeMap<String, String>>>,
        /// The next launches report rebuilt local proxy credentials.
        pub(crate) credentials_reset: Arc<AtomicBool>,
    }

    impl FakeLauncher {
        pub(crate) fn new() -> Arc<Self> {
            let applied: Arc<Mutex<Option<Value>>> = Arc::default();
            // Cumulative counters that grow by 1 KiB up / 4 KiB down per read.
            let traffic: Arc<Mutex<(u64, u64)>> = Arc::default();
            let selected: Arc<Mutex<Option<String>>> = Arc::default();
            let system_proxy = Arc::new(Mutex::new((false, 7891u16)));
            let endpoint = system_proxy.clone();
            let proxy_status = move || {
                let (enabled, port) = *endpoint.lock().unwrap();
                if enabled {
                    json!({"available": true, "enabled": true, "listening": true,
                           "listen": "127.0.0.1", "port": port, "protocols": ["http", "socks5"]})
                } else {
                    json!({"available": true, "enabled": false, "listening": false})
                }
            };
            let toggle = system_proxy.clone();
            let fail_apply: Arc<AtomicBool> = Arc::default();
            let failing = fail_apply.clone();
            let reject_revision: Arc<Mutex<Option<String>>> = Arc::default();
            let rejected = reject_revision.clone();
            let rule_sets: Arc<Mutex<Value>> = Arc::default();
            let reported_rule_sets = rule_sets.clone();
            let pins: Arc<Mutex<std::collections::BTreeMap<String, String>>> = Arc::default();
            let core_pins = pins.clone();
            let status_profile = applied.clone();
            let core = FakeCore::new(move |path, body| {
                let ok = |value: Value| (Duration::ZERO, Ok(value));
                match path {
                    "/v1/set-system-proxy" => {
                        toggle.lock().unwrap().0 = body["enabled"].as_bool().unwrap_or(false);
                        ok(proxy_status())
                    }
                    "/v1/pin-ingress" => {
                        let node = body["node_id"].as_str().unwrap_or_default().to_string();
                        match body["endpoint_key"].as_str() {
                            Some(key) => core_pins.lock().unwrap().insert(node, key.to_string()),
                            None => core_pins.lock().unwrap().remove(&node),
                        };
                        ok(
                            json!({"node_id": body["node_id"], "endpoint_key": body["endpoint_key"]}),
                        )
                    }
                    "/v1/select-node" => {
                        *selected.lock().unwrap() = body["node_id"].as_str().map(str::to_string);
                        ok(json!({"node_id": body["node_id"]}))
                    }
                    "/v1/get-status" => {
                        let mut status = json!({
                            "state": "running",
                            "system_proxy": proxy_status(),
                            "selected_node_id": *selected.lock().unwrap(),
                            "selected_ingress": {"endpoint_key": "11", "previous_endpoint_key": "12", "label": "HKG-A"}
                        });
                        let rule_sets = reported_rule_sets.lock().unwrap().clone();
                        if !rule_sets.is_null() {
                            status["rule_sets"] = rule_sets;
                        }
                        let pins = core_pins.lock().unwrap().clone();
                        let nodes = status_profile
                            .lock()
                            .unwrap()
                            .as_ref()
                            .and_then(|profile| profile["nodes"].as_array().cloned())
                            .unwrap_or_default()
                            .iter()
                            .map(|node| {
                                let id = node["id"].as_str().unwrap_or_default();
                                let pinned = pins.get(id);
                                let ingresses = node["ingresses"]
                                    .as_array()
                                    .cloned()
                                    .unwrap_or_default()
                                    .iter()
                                    .map(|ingress| {
                                        let key = ingress["endpoint_key"].as_str().unwrap_or_default();
                                        // A pinned ingress is down in this fake.
                                        json!({"endpoint_key": key, "role": ingress["role"],
                                               "healthy": pinned.map(String::as_str) != Some(key),
                                               "active": pinned.map(String::as_str) == Some(key)})
                                    })
                                    .collect::<Vec<_>>();
                                json!({"node_id": id, "pinned_endpoint_key": pinned, "ingresses": ingresses})
                            })
                            .collect::<Vec<_>>();
                        status["nodes"] = Value::Array(nodes);
                        ok(status)
                    }
                    "/v1/get-traffic" => {
                        let mut counters = traffic.lock().unwrap();
                        counters.0 += 1_024;
                        counters.1 += 4_096;
                        ok(json!({"upload_bytes": counters.0, "download_bytes": counters.1}))
                    }
                    "/v1/apply-profile" if failing.load(Ordering::SeqCst) => (
                        Duration::ZERO,
                        Err(CoreCallError::Api {
                            code: "INTERNAL".into(),
                            field: None,
                            message: "runtime failed to start".into(),
                            retryable: false,
                        }),
                    ),
                    "/v1/apply-profile"
                        if rejected.lock().unwrap().as_deref()
                            == body["profile"]["revision"].as_str() =>
                    {
                        (
                            Duration::ZERO,
                            Err(CoreCallError::Api {
                                code: "RULE_SET_HOST_NOT_ALLOWED".into(),
                                field: Some("routing.rule_sets[0].url".into()),
                                message: "rule set host must be the host the profile came from"
                                    .into(),
                                retryable: false,
                            }),
                        )
                    }
                    "/v1/apply-profile" => {
                        *applied.lock().unwrap() = Some(body["profile"].clone());
                        // The engine takes the selection (default: the
                        // profile's) and the pins with the profile.
                        *selected.lock().unwrap() = body["selected_node_id"]
                            .as_str()
                            .or_else(|| body["profile"]["selection"]["default_node_id"].as_str())
                            .map(str::to_string);
                        *core_pins.lock().unwrap() = body["pins"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|pin| {
                                Some((
                                    pin["node_id"].as_str()?.to_string(),
                                    pin["endpoint_key"].as_str()?.to_string(),
                                ))
                            })
                            .collect();
                        ok(json!({"applied": true}))
                    }
                    "/v1/list-nodes" => {
                        let nodes = applied
                            .lock()
                            .unwrap()
                            .as_ref()
                            .and_then(|profile| profile["nodes"].as_array().cloned())
                            .unwrap_or_default()
                            .iter()
                            .map(|node| json!({"id": node["id"]}))
                            .collect::<Vec<_>>();
                        ok(Value::Array(nodes))
                    }
                    "/v1/probe-entrances" => {
                        let node = body["node_ids"][0].clone();
                        ok(
                            json!([{"node_id": node, "success": true, "latency_ms": 42, "endpoint_key": "11"}]),
                        )
                    }
                    _ => ok(json!({})),
                }
            });
            Arc::new(Self {
                core,
                launches: Mutex::new(0),
                writer: Arc::default(),
                system_proxy,
                fail_apply,
                reject_revision,
                rule_sets,
                pins,
                credentials_reset: Arc::default(),
            })
        }

        pub(crate) fn calls(&self, path: &str) -> usize {
            self.core
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(called, _)| called == path)
                .count()
        }
    }

    impl CoreLauncher for FakeLauncher {
        fn launch(&self) -> BoxFuture<'_, Result<Launched, ClientErrorInfo>> {
            Box::pin(async move {
                *self.launches.lock().unwrap() += 1;
                let (stop, stopped) = oneshot::channel::<()>();
                Ok(Launched {
                    transport: self.core.clone(),
                    exited: Box::pin(async move {
                        let _ = stopped.await;
                        "stopped".to_string()
                    }),
                    stop,
                    rule_set_hosts: vec!["127.0.0.1".into()],
                    credentials_reset: self.credentials_reset.load(Ordering::SeqCst),
                })
            })
        }
    }

    /// A privileged service that is not installed / not running.
    pub(crate) struct NoService;

    fn unavailable<T>() -> Result<T, ServiceError> {
        Err(ServiceError::Unavailable(
            "connect /run/ppvpn/service.sock: not installed".into(),
        ))
    }

    impl ServiceApi for NoService {
        fn get_version(&self) -> BoxFuture<'_, Result<VersionInfo, ServiceError>> {
            Box::pin(async { unavailable() })
        }
        fn get_status(&self) -> BoxFuture<'_, Result<ServiceStatus, ServiceError>> {
            Box::pin(async { unavailable() })
        }
        fn connect<'a>(
            &'a self,
            _: &'a SessionRef,
            _: &'a Value,
            _: bool,
            _: crate::RoutingMode,
            _: &'a crate::core_ipc::ApplyChoices,
        ) -> BoxFuture<'a, Result<u32, ServiceError>> {
            Box::pin(async { unavailable() })
        }
        fn update_profile<'a>(
            &'a self,
            _: &'a SessionRef,
            _: &'a Value,
            _: crate::RoutingMode,
            _: &'a crate::core_ipc::ApplyChoices,
        ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
            Box::pin(async { unavailable() })
        }
        fn renew_lease<'a>(
            &'a self,
            _: &'a SessionRef,
        ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
            Box::pin(async { unavailable() })
        }
        fn disconnect<'a>(&'a self, _: &'a SessionRef) -> BoxFuture<'a, Result<(), ServiceError>> {
            Box::pin(async { unavailable() })
        }
        fn core_api<'a>(
            &'a self,
            _: &'a SessionRef,
            _: &'a str,
            _: Value,
            _: Duration,
        ) -> BoxFuture<'a, Result<Value, ServiceError>> {
            Box::pin(async { unavailable() })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::test_support::{FakeLauncher, NoService};
    use crate::auth::test_support::{jwt, serve, token_set, MemoryPlatform};
    use crate::core_ipc::{ApplyChoices, BoxFuture};
    use crate::service::{ServiceApi, ServiceError, ServiceStatus, SessionRef, VersionInfo};
    use crate::{
        AuthState, Client, ClientConfig, ClientListener, ClientSnapshot, ConnectionPhase,
        ProbeMethod, ProbeResult, StandardState, TrafficSample,
    };
    use crate::{ErrorCode, ProfileStatus};
    use serde_json::{json, Value};

    const FIXTURE: &str = include_str!("../../account/tests/fixtures/proxy-profile.json");
    const REVISION: &str = "49e28b20a5732f48c1f9411e072d97691b4cfc8e1f1e79485f5869ad45915ff5";

    #[derive(Default)]
    struct Recorder {
        probes: Mutex<Vec<ProbeResult>>,
        traffic: Mutex<Vec<TrafficSample>>,
    }

    impl ClientListener for Recorder {
        fn on_snapshot(&self, _: ClientSnapshot) {}
        fn on_probe_result(&self, result: ProbeResult) {
            self.probes.lock().unwrap().push(result);
        }
        fn on_traffic(&self, sample: TrafficSample) {
            self.traffic.lock().unwrap().push(sample);
        }
    }

    fn wait_for(client: &Client, what: impl Fn(&ClientSnapshot) -> bool) -> ClientSnapshot {
        // Generous: a loaded CI runner (Windows, the tests in parallel) can
        // take seconds to sign in and start the fake core; a passing wait
        // returns as soon as the state is there.
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let snapshot = client.snapshot();
            if what(&snapshot) {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "timed out; last {snapshot:?}");
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

    /// A client restored from a saved login against a scripted backend;
    /// `later` answers the requests after the first profile download.
    fn signed_in_client_then(
        later: Vec<(&'static str, String)>,
    ) -> (Arc<Client>, Arc<Recorder>, Arc<FakeLauncher>, String) {
        signed_in_client_with(later, FakeLauncher::new())
    }

    /// [`signed_in_client_then`] with a prepared standard core.
    fn signed_in_client_with(
        later: Vec<(&'static str, String)>,
        launcher: Arc<FakeLauncher>,
    ) -> (Arc<Client>, Arc<Recorder>, Arc<FakeLauncher>, String) {
        signed_in_client_on(
            later,
            launcher,
            MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#),
        )
    }

    /// [`signed_in_client_with`] on a given platform.
    fn signed_in_client_on(
        later: Vec<(&'static str, String)>,
        launcher: Arc<FakeLauncher>,
        platform: Arc<MemoryPlatform>,
    ) -> (Arc<Client>, Arc<Recorder>, Arc<FakeLauncher>, String) {
        signed_in_client_using(later, launcher, platform, Arc::new(NoService))
    }

    fn signed_in_client_using(
        later: Vec<(&'static str, String)>,
        launcher: Arc<FakeLauncher>,
        platform: Arc<MemoryPlatform>,
        service: Arc<dyn ServiceApi>,
    ) -> (Arc<Client>, Arc<Recorder>, Arc<FakeLauncher>, String) {
        let mut responses = vec![
            ("200 OK", r#"{"refresh_token":"jyr_prepared"}"#.to_string()),
            ("200 OK", token_set("jyr_active")),
            ("200 OK", r#"{"id":"u1","name":"Jerry","avatar":""}"#.into()),
            (
                "200 OK",
                r#"{"items":[{"id":"t1","name":"Personal","is_personal":true,"is_default":true},{"id":"t2","name":"Work","is_personal":false,"is_default":false}]}"#
                    .into(),
            ),
            ("200 OK", FIXTURE.into()),
        ];
        responses.extend(later);
        let (base, _) = serve(responses);
        let data_dir = std::env::temp_dir()
            .join(format!("ppvpn-cores-test-{}", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        let config = ClientConfig {
            api_base: base,
            data_dir: data_dir.clone(),
            log_dir: data_dir.clone(),
            platform: "macos".into(),
            app_version: "0.0.0-test".into(),
        };
        let recorder = Arc::new(Recorder::default());
        let client = Client::with_parts(
            config,
            platform,
            recorder.clone(),
            service,
            launcher.clone(),
            launcher.writer.clone(),
        );
        (client, recorder, launcher, data_dir)
    }

    fn signed_in_client() -> (Arc<Client>, Arc<Recorder>, Arc<FakeLauncher>, String) {
        signed_in_client_then(vec![("200 OK", r#"{"revoked":true}"#.into())])
    }

    fn switch_response(id: &str, name: &str) -> (&'static str, String) {
        (
            "200 OK",
            format!(
                r#"{{"id":"{id}","name":"{name}","is_personal":false,"token":"{}"}}"#,
                jwt(r#"{"aud":["ppvpn"]}"#)
            ),
        )
    }

    fn not_subscribed() -> (&'static str, String) {
        (
            "404 Not Found",
            r#"{"code":"SUBSCRIPTION_NOT_FOUND"}"#.into(),
        )
    }

    fn team_disabled() -> (&'static str, String) {
        (
            "403 Forbidden",
            r#"{"code":"403012","message":"team is not active"}"#.into(),
        )
    }

    fn is_ready(snapshot: &ClientSnapshot) -> bool {
        snapshot.profile_status == ProfileStatus::Ready
            && matches!(snapshot.standard, StandardState::Ready { .. })
    }

    #[test]
    fn new_user_without_subscription_is_a_steady_state() {
        let (base, _) = serve(vec![
            ("200 OK", r#"{"refresh_token":"jyr_prepared"}"#.to_string()),
            ("200 OK", token_set("jyr_active")),
            ("200 OK", r#"{"id":"u1","name":"New","avatar":""}"#.into()),
            (
                "200 OK",
                r#"{"items":[{"id":"t1","name":"Personal","is_personal":true,"is_default":true}]}"#
                    .into(),
            ),
            not_subscribed(),
        ]);
        let data_dir = std::env::temp_dir()
            .join(format!("ppvpn-cores-test-{}", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        let launcher = FakeLauncher::new();
        let client = Client::with_parts(
            ClientConfig {
                api_base: base,
                data_dir: data_dir.clone(),
                log_dir: data_dir.clone(),
                platform: "macos".into(),
                app_version: "0.0.0-test".into(),
            },
            MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#),
            Arc::new(Recorder::default()),
            Arc::new(NoService),
            launcher.clone(),
            launcher.writer.clone(),
        );
        let snapshot = wait_for(&client, |s| {
            s.profile_status == ProfileStatus::NoSubscription
        });
        assert!(matches!(snapshot.auth, AuthState::SignedIn));
        assert!(snapshot.profile.is_none());
        assert!(snapshot.last_error.is_none(), "{:?}", snapshot.last_error);
        assert!(matches!(snapshot.standard, StandardState::Stopped));
        assert_eq!(*launcher.launches.lock().unwrap(), 0);
        assert!(client.purchase_url().ends_with("/dashboard/products"));
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn switching_to_an_unsubscribed_team_and_back() {
        let (client, _, launcher, data_dir) = signed_in_client_then(vec![
            switch_response("t2", "Work"),
            not_subscribed(),
            switch_response("t1", "Personal"),
            ("200 OK", FIXTURE.into()),
        ]);
        wait_for(&client, is_ready);

        let error = block_on(client.switch_team("t2".into())).unwrap_err();
        assert_eq!(error.info().unwrap().code, ErrorCode::NoSubscription);
        let snapshot = client.snapshot();
        assert_eq!(snapshot.team.as_ref().unwrap().id, "t2");
        assert_eq!(snapshot.profile_status, ProfileStatus::NoSubscription);
        assert!(snapshot.profile.is_none());
        assert!(snapshot.last_error.is_none());
        assert!(client.nodes().is_empty());
        wait_for(&client, |s| matches!(s.standard, StandardState::Stopped));

        block_on(client.switch_team("t1".into())).unwrap();
        let snapshot = wait_for(&client, is_ready);
        assert_eq!(snapshot.team.unwrap().id, "t1");
        assert_eq!(*launcher.launches.lock().unwrap(), 2);
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    fn failed_team_profile_stops_previous_core(
        response: Option<(&'static str, String)>,
        expected: ErrorCode,
    ) {
        let can_retry = response.is_some();
        let mut responses = vec![switch_response("t2", "Work")];
        if let Some(response) = response {
            responses.push(response);
            responses.push(("200 OK", FIXTURE.replace(REVISION, "team-two")));
        }
        // Without a profile response the server closes the next connection.
        let (client, _, launcher, data_dir) = signed_in_client_then(responses);
        wait_for(&client, is_ready);
        block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
        block_on(client.connect()).unwrap();
        assert_eq!(client.snapshot().connection.phase, ConnectionPhase::On);

        let error = block_on(client.switch_team("t2".into())).unwrap_err();
        assert_eq!(error.info().unwrap().code, expected);
        let snapshot = client.snapshot();
        assert_eq!(snapshot.team.as_ref().unwrap().id, "t2");
        assert!(snapshot.profile.is_none());
        assert!(client.nodes().is_empty());
        assert!(
            matches!(snapshot.standard, StandardState::Stopped),
            "the previous team's standard core is still running: {:?}",
            snapshot.standard
        );
        assert_eq!(snapshot.connection.phase, ConnectionPhase::Off);
        assert_eq!(launcher.calls("/v1/stop"), 1);
        assert_eq!(proxy_toggles(&launcher), vec![true, false]);
        assert!(!backup_exists(&data_dir), "the OS proxy was not restored");
        // Enhanced controllers keep their last profile across disconnects.
        // Neither connect nor retry may revive it for a team with no profile.
        block_on(client.set_connection_mode(crate::ConnectionMode::Enhanced)).unwrap();
        for result in [
            block_on(client.connect()),
            block_on(client.retry()),
            block_on(client.enhanced_take_over()),
        ] {
            let info = result.unwrap_err().info().unwrap();
            assert_eq!(info.code, ErrorCode::ProfileFetchFailed);
            assert_eq!(info.detail, "PROFILE_NOT_READY");
        }

        if can_retry {
            block_on(client.refresh_profile()).unwrap();
            let snapshot = wait_for(&client, is_ready);
            assert_eq!(snapshot.team.unwrap().id, "t2");
            assert_eq!(snapshot.profile.unwrap().revision, "team-two");
            assert_eq!(*launcher.launches.lock().unwrap(), 2);
            assert_eq!(snapshot.connection.phase, ConnectionPhase::Off);
        }
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn team_switch_profile_server_error_stops_previous_core() {
        failed_team_profile_stops_previous_core(
            Some(("503 Service Unavailable", "{}".into())),
            ErrorCode::ServerUnavailable,
        );
    }

    #[test]
    fn team_switch_profile_transport_error_stops_previous_core() {
        failed_team_profile_stops_previous_core(None, ErrorCode::NetworkUnreachable);
    }

    #[test]
    fn team_switch_profile_parse_error_stops_previous_core() {
        failed_team_profile_stops_previous_core(
            Some((
                "200 OK",
                r#"{"schema_version":1,"revision":"bad","expires_at":"2099-01-01T00:00:00Z","nodes":[{"id":"a-1","name":"x","exit":{"region":"Tokyo"}}]}"#.into(),
            )),
            ErrorCode::ProfileInvalid,
        );
    }

    /// An existing privileged-service session, reattached through the real
    /// enhanced controller so team switching exercises its disconnect path.
    struct RestoredService {
        running: AtomicBool,
        disconnects: AtomicUsize,
    }

    impl RestoredService {
        fn status(&self) -> ServiceStatus {
            ServiceStatus {
                running: self.running.load(Ordering::SeqCst),
                session_id: Some("team-one-session".into()),
                generation: 1,
                profile_revision: Some(REVISION.into()),
                ..ServiceStatus::default()
            }
        }
    }

    impl ServiceApi for RestoredService {
        fn get_version(&self) -> BoxFuture<'_, Result<VersionInfo, ServiceError>> {
            Box::pin(async { Ok(VersionInfo::default()) })
        }
        fn get_status(&self) -> BoxFuture<'_, Result<ServiceStatus, ServiceError>> {
            Box::pin(async { Ok(self.status()) })
        }
        fn connect<'a>(
            &'a self,
            session: &'a SessionRef,
            profile: &'a Value,
            take_over: bool,
            routing: crate::RoutingMode,
            choices: &'a ApplyChoices,
        ) -> BoxFuture<'a, Result<u32, ServiceError>> {
            NoService.connect(session, profile, take_over, routing, choices)
        }
        fn update_profile<'a>(
            &'a self,
            _: &'a SessionRef,
            _: &'a Value,
            _: crate::RoutingMode,
            _: &'a ApplyChoices,
        ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
            Box::pin(async { Ok(self.status()) })
        }
        fn renew_lease<'a>(
            &'a self,
            _: &'a SessionRef,
        ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
            Box::pin(async { Ok(self.status()) })
        }
        fn disconnect<'a>(
            &'a self,
            session: &'a SessionRef,
        ) -> BoxFuture<'a, Result<(), ServiceError>> {
            Box::pin(async move {
                assert_eq!(session.session_id, "team-one-session");
                assert_eq!(session.generation, 1);
                self.running.store(false, Ordering::SeqCst);
                self.disconnects.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        }
        fn core_api<'a>(
            &'a self,
            session: &'a SessionRef,
            path: &'a str,
            body: Value,
            timeout: Duration,
        ) -> BoxFuture<'a, Result<Value, ServiceError>> {
            NoService.core_api(session, path, body, timeout)
        }
    }

    #[test]
    fn team_switch_profile_failure_disconnects_an_active_enhanced_session() {
        let service = Arc::new(RestoredService {
            running: AtomicBool::new(true),
            disconnects: AtomicUsize::new(0),
        });
        let (client, _, launcher, data_dir) = signed_in_client_using(
            vec![
                switch_response("t2", "Work"),
                ("503 Service Unavailable", "{}".into()),
            ],
            FakeLauncher::new(),
            MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#),
            service.clone(),
        );
        wait_for(&client, is_ready);
        client.enhanced.set_health_interval(Duration::from_secs(60));
        std::fs::write(
            std::path::Path::new(&data_dir).join(crate::storage::ENHANCED_STATE_FILE),
            json!({
                "generation": 1,
                "desired": "connected",
                "sessionId": "team-one-session",
                "profileRevision": REVISION,
            })
            .to_string(),
        )
        .unwrap();
        block_on(async {
            client.enhanced.restore().await;
            assert_eq!(client.snapshot().connection.phase, ConnectionPhase::On);
            let error = client.switch_team("t2".into()).await.unwrap_err();
            assert_eq!(error.info().unwrap().code, ErrorCode::ServerUnavailable);
            assert!(!service.running.load(Ordering::SeqCst));
            assert_eq!(service.disconnects.load(Ordering::SeqCst), 1);
            let snapshot = client.snapshot();
            assert_eq!(snapshot.team.unwrap().id, "t2");
            assert!(snapshot.profile.is_none());
            assert!(matches!(snapshot.standard, StandardState::Stopped));
            assert_eq!(snapshot.connection.phase, ConnectionPhase::Off);
            assert_eq!(launcher.calls("/v1/stop"), 1);
            assert_eq!(
                client.connect().await.unwrap_err().info().unwrap().detail,
                "PROFILE_NOT_READY"
            );
        });
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn team_switch_success_starts_the_new_profile_after_stopping_the_old_core() {
        let (client, _, launcher, data_dir) = signed_in_client_then(vec![
            switch_response("t2", "Work"),
            ("200 OK", FIXTURE.replace(REVISION, "team-two")),
        ]);
        wait_for(&client, is_ready);
        block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
        block_on(client.connect()).unwrap();
        block_on(client.switch_team("t2".into())).unwrap();
        let snapshot = wait_for(&client, is_ready);
        assert_eq!(snapshot.team.unwrap().id, "t2");
        assert_eq!(snapshot.profile.unwrap().revision, "team-two");
        assert_eq!(snapshot.connection.phase, ConnectionPhase::Off);
        assert_eq!(launcher.calls("/v1/stop"), 1);
        assert_eq!(*launcher.launches.lock().unwrap(), 2);
        assert_eq!(proxy_toggles(&launcher), vec![true, false]);
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn team_switch_refusal_keeps_previous_core_and_connection() {
        let (client, _, launcher, data_dir) = signed_in_client_then(vec![team_disabled()]);
        wait_for(&client, is_ready);
        block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
        block_on(client.connect()).unwrap();

        let error = block_on(client.switch_team("t2".into())).unwrap_err();
        assert_eq!(error.info().unwrap().code, ErrorCode::TeamDisabled);
        let snapshot = client.snapshot();
        assert_eq!(snapshot.team.as_ref().unwrap().id, "t1");
        assert_eq!(snapshot.profile.as_ref().unwrap().revision, REVISION);
        assert!(is_ready(&snapshot));
        assert_eq!(snapshot.connection.phase, ConnectionPhase::On);
        assert_eq!(launcher.calls("/v1/stop"), 0);
        assert_eq!(proxy_toggles(&launcher), vec![true]);
        assert!(backup_exists(&data_dir));
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn disabled_team_keeps_the_session_and_the_team_picker() {
        let (client, _, _, data_dir) = signed_in_client_then(vec![
            team_disabled(), // refresh_profile
            team_disabled(), // teams
            team_disabled(), // switch_team refused from a disabled team
            switch_response("t2", "Work"),
            ("200 OK", FIXTURE.into()),
        ]);
        wait_for(&client, is_ready);

        let error = block_on(client.refresh_profile()).unwrap_err();
        assert_eq!(error.info().unwrap().code, ErrorCode::TeamDisabled);
        let snapshot = client.snapshot();
        assert_eq!(snapshot.profile_status, ProfileStatus::TeamDisabled);
        assert_eq!(
            snapshot.team.as_ref().map(|team| team.id.as_str()),
            Some("t1"),
            "the disabled team stays in the snapshot"
        );
        assert!(snapshot.profile.is_none());
        assert!(snapshot.last_error.is_none());
        assert!(matches!(snapshot.auth, AuthState::SignedIn));
        wait_for(&client, |s| matches!(s.standard, StandardState::Stopped));

        // The picker keeps the last known list.
        let teams = block_on(client.teams()).unwrap();
        assert_eq!(teams.len(), 2);

        let error = block_on(client.switch_team("t2".into())).unwrap_err();
        let info = error.info().unwrap();
        assert_eq!(info.code, ErrorCode::TeamDisabled);
        assert_eq!(
            info.detail,
            "POST /api/v1/me/switch-team -> HTTP 403 403012"
        );
        assert!(matches!(client.snapshot().auth, AuthState::SignedIn));
        assert_eq!(
            client.snapshot().profile_status,
            ProfileStatus::TeamDisabled
        );
        assert_eq!(
            client.snapshot().last_error.map(|info| info.code),
            Some(ErrorCode::TeamDisabled)
        );

        block_on(client.switch_team("t2".into())).unwrap();
        let snapshot = wait_for(&client, is_ready);
        assert_eq!(snapshot.team.unwrap().id, "t2");
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn malformed_profile_keeps_the_previous_one_and_never_reaches_the_cores() {
        let old_format = r#"{"schema_version":1,"revision":"r","expires_at":"2099-01-01T00:00:00Z","nodes":[{"id":"a-1","name":"x","exit":{"region":"东京"}}]}"#;
        let (client, _, launcher, data_dir) =
            signed_in_client_then(vec![("200 OK", old_format.to_string())]);
        wait_for(&client, |s| {
            matches!(s.standard, StandardState::Ready { .. })
        });

        let error = block_on(client.refresh_profile()).unwrap_err();
        let info = error.info().unwrap();
        assert_eq!(info.code, crate::ErrorCode::ProfileInvalid);
        assert_eq!(info.detail, "PROFILE_SHAPE: nodes[0].entry_key missing");
        let snapshot = client.snapshot();
        assert_eq!(snapshot.profile.unwrap().revision, REVISION);
        assert_eq!(
            snapshot.profile_status,
            ProfileStatus::Invalid {
                error: info,
                previous: true
            }
        );
        assert!(snapshot.last_error.is_none(), "{:?}", snapshot.last_error);
        assert_eq!(client.nodes().len(), 3);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(launcher.calls("/v1/apply-profile"), 1);

        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    fn saved_revision(data_dir: &str, api_base: &str) -> Option<String> {
        crate::saved_profile::load(data_dir, api_base).map(|saved| saved.revision)
    }

    fn rejected_detail(snapshot: &ClientSnapshot) -> Option<(String, bool)> {
        match &snapshot.profile_status {
            ProfileStatus::Invalid { error, previous } => Some((error.detail.clone(), *previous)),
            _ => None,
        }
    }

    #[test]
    fn a_rejected_profile_with_nothing_before_it_says_so_and_fails_compatible_at_once() {
        let launcher = FakeLauncher::new();
        *launcher.reject_revision.lock().unwrap() = Some(REVISION.into());
        let (client, _, launcher, data_dir) =
            signed_in_client_with(vec![("200 OK", r#"{"revoked":true}"#.into())], launcher);
        let snapshot = wait_for(&client, |s| rejected_detail(s).is_some());
        let (detail, previous) = rejected_detail(&snapshot).unwrap();
        assert!(
            detail.starts_with("RULE_SET_HOST_NOT_ALLOWED routing.rule_sets[0].url"),
            "{detail}"
        );
        assert!(!previous, "nothing was accepted before");
        assert!(matches!(snapshot.standard, StandardState::Failed { .. }));

        block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
        let error = block_on(client.connect()).unwrap_err();
        assert_eq!(error.info().unwrap().code, ErrorCode::ProfileInvalid);
        let snapshot = client.snapshot();
        assert_eq!(snapshot.connection.phase, ConnectionPhase::Error);
        assert_eq!(
            snapshot.connection.reason.map(|reason| reason.code),
            Some(ErrorCode::ProfileInvalid)
        );
        assert!(
            !launcher
                .writer
                .calls()
                .iter()
                .any(|call| call.starts_with("apply")),
            "the OS proxy was never touched"
        );
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn a_rejected_revision_falls_back_to_the_accepted_one_and_sign_out_drops_it() {
        let rejected = FIXTURE.replace(REVISION, "r2");
        let (client, _, launcher, data_dir) = signed_in_client_then(vec![
            ("200 OK", rejected.clone()),
            ("200 OK", rejected),
            ("200 OK", r#"{"revoked":true}"#.into()),
        ]);
        let api_base = client.config.api_base.clone();
        wait_for(&client, is_ready);
        let deadline = Instant::now() + Duration::from_secs(10);
        while saved_revision(&data_dir, &api_base).as_deref() != Some(REVISION) {
            assert!(
                Instant::now() < deadline,
                "the accepted profile was not saved"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        *launcher.reject_revision.lock().unwrap() = Some("r2".into());
        block_on(client.refresh_profile()).unwrap();
        let snapshot = wait_for(&client, |s| rejected_detail(s).is_some());
        assert!(
            rejected_detail(&snapshot).unwrap().1,
            "the accepted one is in use"
        );
        assert_eq!(snapshot.profile.unwrap().revision, REVISION);
        assert_eq!(
            client.current_profile().unwrap().raw.as_slice(),
            FIXTURE.as_bytes()
        );
        assert!(matches!(
            client.snapshot().standard,
            StandardState::Ready { revision } if revision == REVISION
        ));
        assert_eq!(
            saved_revision(&data_dir, &api_base).as_deref(),
            Some(REVISION)
        );

        // The same rejected revision again: the accepted one stays.
        let applies = launcher.calls("/v1/apply-profile");
        block_on(client.refresh_profile()).unwrap();
        assert_eq!(client.snapshot().profile.unwrap().revision, REVISION);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(launcher.calls("/v1/apply-profile"), applies);

        block_on(client.sign_out()).unwrap();
        assert_eq!(saved_revision(&data_dir, &api_base), None);
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn the_saved_profile_serves_a_launch_whose_fresh_profile_is_rejected() {
        let rejected = FIXTURE.replace(REVISION, "r2");
        let (base, _) = serve(vec![
            ("200 OK", r#"{"refresh_token":"jyr_prepared"}"#.to_string()),
            ("200 OK", token_set("jyr_active")),
            ("200 OK", r#"{"id":"u1","name":"Jerry","avatar":""}"#.into()),
            (
                "200 OK",
                r#"{"items":[{"id":"t1","name":"Personal","is_personal":true,"is_default":true}]}"#
                    .into(),
            ),
            ("200 OK", rejected),
        ]);
        let data_dir = crate::test_backend::temp_dir("ppvpn-cores-test");
        crate::saved_profile::save(
            &data_dir,
            &base,
            &crate::saved_profile::SavedProfile {
                revision: REVISION.into(),
                raw: Arc::new(FIXTURE.as_bytes().to_vec()),
            },
        );
        let launcher = FakeLauncher::new();
        *launcher.reject_revision.lock().unwrap() = Some("r2".into());
        let client = Client::with_parts(
            ClientConfig {
                api_base: base.clone(),
                data_dir: data_dir.clone(),
                log_dir: data_dir.clone(),
                platform: "macos".into(),
                app_version: "0.0.0-test".into(),
            },
            MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#),
            Arc::new(Recorder::default()),
            Arc::new(NoService),
            launcher.clone(),
            launcher.writer.clone(),
        );
        let snapshot = wait_for(&client, |s| {
            rejected_detail(s).is_some_and(|(_, previous)| previous)
                && matches!(s.standard, StandardState::Ready { .. })
        });
        assert_eq!(snapshot.profile.unwrap().revision, REVISION);
        assert!(matches!(
            snapshot.standard,
            StandardState::Ready { revision } if revision == REVISION
        ));
        assert_eq!(saved_revision(&data_dir, &base).as_deref(), Some(REVISION));
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn refresh_retries_a_failed_standard_core_with_the_same_profile() {
        let launcher = FakeLauncher::new();
        launcher.fail_apply.store(true, Ordering::SeqCst);
        let later = vec![("200 OK", FIXTURE.to_string()); 3];
        let (client, _, launcher, data_dir) = signed_in_client_with(later, launcher);
        let snapshot = wait_for(&client, |s| {
            matches!(s.standard, StandardState::Failed { .. })
        });
        assert_eq!(snapshot.profile_status, ProfileStatus::Ready);
        assert_eq!(launcher.calls("/v1/apply-profile"), 1);

        // Unchanged profile, core still broken: one more attempt per refresh.
        block_on(client.refresh_profile()).unwrap();
        wait_for(&client, |_| launcher.calls("/v1/apply-profile") == 2);
        std::thread::sleep(Duration::from_millis(100));
        assert!(matches!(
            client.snapshot().standard,
            StandardState::Failed { .. }
        ));
        assert_eq!(launcher.calls("/v1/apply-profile"), 2);
        assert_eq!(launcher.calls("/v1/start"), 0);

        // The core can run it now: the refresh re-applies and starts it.
        launcher.fail_apply.store(false, Ordering::SeqCst);
        block_on(client.refresh_profile()).unwrap();
        wait_for(
            &client,
            |s| matches!(&s.standard, StandardState::Ready { revision } if revision == REVISION),
        );
        assert_eq!(launcher.calls("/v1/apply-profile"), 3);
        assert_eq!(launcher.calls("/v1/start"), 1);
        assert!(client.snapshot().last_error.is_none());

        // Running with the same profile: a refresh applies nothing.
        block_on(client.refresh_profile()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(launcher.calls("/v1/apply-profile"), 3);
        assert_eq!(launcher.calls("/v1/start"), 1);
        assert_eq!(*launcher.launches.lock().unwrap(), 1);
        assert!(matches!(
            client.snapshot().standard,
            StandardState::Ready { .. }
        ));

        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn standard_core_follows_the_selection_and_reports_its_path() {
        let (client, _, launcher, data_dir) = signed_in_client();
        let snapshot = wait_for(&client, |s| s.connection.detail.latency_ms.is_some());
        assert_eq!(snapshot.connection.detail.latency_ms, Some(42));
        assert_eq!(
            snapshot.connection.detail.endpoint_key.as_deref(),
            Some("11")
        );
        assert_eq!(
            snapshot.connection.detail.endpoint_label.as_deref(),
            Some("HKG-A")
        );
        let selected = snapshot.selected_node_id.clone().unwrap();
        // The selection goes with the profile, not after it.
        let applied = launcher
            .core
            .calls
            .lock()
            .unwrap()
            .iter()
            .find(|(path, _)| path == "/v1/apply-profile")
            .map(|(_, body)| body.clone())
            .unwrap();
        assert_eq!(applied["selected_node_id"], json!(selected));
        let selects = |launcher: &FakeLauncher| {
            launcher
                .core
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(path, _)| path == "/v1/select-node")
                .map(|(_, body)| body["node_id"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert!(selects(&launcher).is_empty(), "no select-node after apply");

        let other = client
            .nodes()
            .into_iter()
            .find(|node| node.id != selected)
            .unwrap()
            .id;
        block_on(client.select_node(other.clone())).unwrap();
        assert_eq!(selects(&launcher).last(), Some(&other));
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn an_ingress_pin_reaches_the_core_persists_and_is_restored() {
        let node = "7d7c34e4-7f38-4c0c-9a53-1f0c0c9e2b11-101".to_string();
        // The next profile no longer has ingress 12.
        let mut later: Value = serde_json::from_str(FIXTURE).unwrap();
        later["revision"] = json!("r-without-12");
        later["nodes"][0]["ingresses"]
            .as_array_mut()
            .unwrap()
            .retain(|ingress| ingress["endpoint_key"] != "12");
        let (client, _, launcher, data_dir) =
            signed_in_client_then(vec![("200 OK", later.to_string())]);
        wait_for(&client, |s| {
            matches!(s.standard, StandardState::Ready { .. })
        });

        let error = block_on(client.pin_ingress(node.clone(), Some("99".into()))).unwrap_err();
        assert_eq!(error.info().unwrap().code, crate::ErrorCode::NodeNotFound);
        block_on(client.pin_ingress(node.clone(), Some("12".into()))).unwrap();
        assert_eq!(
            client.snapshot().ingress_pins,
            vec![crate::IngressPin {
                node_id: node.clone(),
                endpoint_key: "12".into()
            }]
        );
        assert_eq!(
            launcher.pins.lock().unwrap().get(&node).map(String::as_str),
            Some("12")
        );
        assert_eq!(
            crate::ingress::load(&data_dir)
                .get(&node)
                .map(String::as_str),
            Some("12"),
            "persisted"
        );
        // The core reports the pin and its health.
        let snapshot = wait_for(&client, |s| {
            s.node_ingresses
                .iter()
                .any(|n| n.node_id == node && n.pinned_endpoint_key.as_deref() == Some("12"))
        });
        let reported = snapshot
            .node_ingresses
            .iter()
            .find(|n| n.node_id == node)
            .unwrap();
        let pinned = reported
            .ingresses
            .iter()
            .find(|ingress| ingress.endpoint_key == "12")
            .unwrap();
        assert_eq!(pinned.healthy, Some(false));
        assert!(pinned.active);

        // Back to automatic.
        block_on(client.pin_ingress(node.clone(), None)).unwrap();
        assert!(client.snapshot().ingress_pins.is_empty());
        assert!(launcher.pins.lock().unwrap().is_empty());

        // Pinned again, another node too, then the profile drops the first
        // pin's ingress: that pin goes, the other comes with the new profile.
        let other = "7d7c34e4-7f38-4c0c-9a53-1f0c0c9e2b11-102".to_string();
        block_on(client.pin_ingress(node.clone(), Some("12".into()))).unwrap();
        block_on(client.pin_ingress(other.clone(), Some("22".into()))).unwrap();
        let live_pins = launcher.calls("/v1/pin-ingress");
        block_on(client.refresh_profile()).unwrap();
        let snapshot = wait_for(&client, |s| !s.cleared_ingress_pins.is_empty());
        assert_eq!(
            snapshot.ingress_pins,
            vec![crate::IngressPin {
                node_id: other.clone(),
                endpoint_key: "22".into()
            }]
        );
        assert_eq!(snapshot.cleared_ingress_pins[0].endpoint_key, "12");
        assert_eq!(crate::ingress::load(&data_dir).len(), 1);
        let last_apply = || {
            launcher
                .core
                .calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(path, _)| path == "/v1/apply-profile")
                .map(|(_, body)| body.clone())
                .next_back()
                .unwrap()
        };
        wait_for(&client, |_| {
            last_apply()["profile"]["revision"] == json!("r-without-12")
        });
        let applied = last_apply();
        assert_eq!(
            applied["pins"],
            json!([{"node_id": other, "endpoint_key": "22"}])
        );
        assert_eq!(
            launcher.calls("/v1/pin-ingress"),
            live_pins,
            "no pin-ingress after an apply"
        );
        client.dismiss_cleared_ingress_pins();
        assert!(client.snapshot().cleared_ingress_pins.is_empty());

        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn rebuilt_local_proxy_credentials_show_until_dismissed() {
        let launcher = FakeLauncher::new();
        launcher.credentials_reset.store(true, Ordering::SeqCst);
        let (client, _, _launcher, data_dir) =
            signed_in_client_with(vec![("200 OK", r#"{"revoked":true}"#.into())], launcher);
        // The reset is reported as the core starts; Ready may come a moment later.
        let snapshot = wait_for(&client, |s| {
            s.local_proxy_credentials_reset && matches!(s.standard, StandardState::Ready { .. })
        });
        assert!(snapshot.local_proxy_credentials_reset);

        client.dismiss_local_proxy_credentials_reset();
        assert!(!client.snapshot().local_proxy_credentials_reset);

        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn unavailable_rule_sets_show_until_they_load() {
        let (client, _, launcher, data_dir) = signed_in_client();
        wait_for(&client, |s| s.connection.detail.endpoint_key.is_some());
        assert!(client.snapshot().rule_sets_unavailable.is_empty());

        // The standard core applied with the pinned host.
        let body = launcher
            .core
            .calls
            .lock()
            .unwrap()
            .iter()
            .find(|(path, _)| path == "/v1/apply-profile")
            .map(|(_, body)| body.clone())
            .unwrap();
        assert_eq!(body["allowed_rule_set_hosts"], json!(["127.0.0.1"]));

        *launcher.rule_sets.lock().unwrap() = json!([
            {"id": "cn-ip", "state": "ready", "updated_at": "2026-07-23T12:00:00Z"},
            {"id": "cn-site", "state": "unavailable", "error": "RULE_SET_DOWNLOAD_FAILED"},
            {"id": "ads", "state": "stale", "error": "RULE_SET_HTTP_STATUS"}
        ]);
        let snapshot = wait_for(&client, |s| !s.rule_sets_unavailable.is_empty());
        assert_eq!(snapshot.rule_sets_unavailable, ["cn-site"]);

        // Loaded (stale still routes): the notice clears.
        *launcher.rule_sets.lock().unwrap() = json!([
            {"id": "cn-ip", "state": "ready"},
            {"id": "cn-site", "state": "stale", "error": "RULE_SET_DOWNLOAD_FAILED"}
        ]);
        wait_for(&client, |s| s.rule_sets_unavailable.is_empty());

        // A profile without rule sets reports none.
        *launcher.rule_sets.lock().unwrap() = json!([{"id": "cn-ip", "state": "unavailable"}]);
        wait_for(&client, |s| !s.rule_sets_unavailable.is_empty());
        *launcher.rule_sets.lock().unwrap() = Value::Null;
        wait_for(&client, |s| s.rule_sets_unavailable.is_empty());

        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn rule_set_changed_events_update_the_snapshot() {
        let (client, _, _launcher, data_dir) = signed_in_client();
        wait_for(&client, is_ready);
        // Stop the status poll from overwriting the event-driven state.
        if let Some(task) = client.session_state().monitor_task.take() {
            task.abort();
        }
        client.on_core_event(&json!({"type": "RuleSetChanged", "at": "2026-07-23T12:00:00Z",
            "rule_set_id": "cn-site", "message": "unavailable", "code": "RULE_SET_DOWNLOAD_FAILED"}));
        assert_eq!(client.snapshot().rule_sets_unavailable, ["cn-site"]);
        client.on_core_event(&json!({"type": "NodeSelected", "node_id": "hk-001"}));
        assert_eq!(client.snapshot().rule_sets_unavailable, ["cn-site"]);
        client.on_core_event(
            &json!({"type": "RuleSetChanged", "at": "2026-07-23T12:00:05Z",
            "rule_set_id": "cn-site", "message": "ready"}),
        );
        assert!(client.snapshot().rule_sets_unavailable.is_empty());

        // Signing out clears it too.
        client.on_core_event(&json!({"type": "RuleSetChanged",
            "rule_set_id": "cn-ip", "message": "unavailable"}));
        block_on(client.sign_out()).unwrap();
        assert!(client.snapshot().rule_sets_unavailable.is_empty());
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    fn proxy_toggles(launcher: &FakeLauncher) -> Vec<bool> {
        launcher
            .core
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/v1/set-system-proxy")
            .map(|(_, body)| body["enabled"].as_bool().unwrap())
            .collect()
    }

    fn backup_exists(data_dir: &str) -> bool {
        std::path::Path::new(data_dir)
            .join("system-proxy-backup.json")
            .is_file()
    }

    #[test]
    fn compatible_mode_writes_and_restores_the_os_proxy() {
        let (client, _, launcher, data_dir) = signed_in_client();
        wait_for(&client, |s| {
            matches!(s.standard, StandardState::Ready { .. })
        });
        block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
        assert_eq!(
            client.snapshot().connection_mode,
            crate::ConnectionMode::Compatible
        );
        assert_eq!(client.snapshot().connection.phase, ConnectionPhase::Off);
        assert!(
            proxy_toggles(&launcher).is_empty(),
            "switching alone opens nothing"
        );

        block_on(client.connect()).unwrap();
        let snapshot = client.snapshot();
        assert_eq!(snapshot.connection.phase, ConnectionPhase::On);
        assert!(!snapshot.connection.can_take_over);
        assert_eq!(
            launcher.writer.calls(),
            vec!["snapshot", "apply 127.0.0.1:7891"]
        );
        assert!(backup_exists(&data_dir));
        assert_eq!(proxy_toggles(&launcher), vec![true]);

        // The core restarted: endpoint disabled and on another port.
        *launcher.system_proxy.lock().unwrap() = (false, 7900);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !launcher
            .writer
            .calls()
            .contains(&"apply 127.0.0.1:7900".to_string())
        {
            assert!(Instant::now() < deadline, "not rewritten");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(proxy_toggles(&launcher), vec![true, true]);

        block_on(client.disconnect()).unwrap();
        assert_eq!(client.snapshot().connection.phase, ConnectionPhase::Off);
        let calls = launcher.writer.calls();
        assert_eq!(calls.last().unwrap(), r#"restore {"fake":"previous"}"#);
        assert_eq!(calls.iter().filter(|call| *call == "snapshot").count(), 1);
        assert!(!backup_exists(&data_dir));
        assert_eq!(proxy_toggles(&launcher).last(), Some(&false));

        // The choice persists.
        assert_eq!(
            crate::connection::load_mode(&data_dir),
            crate::ConnectionMode::Compatible
        );
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn another_app_taking_the_system_proxy_over_ends_compatible_mode() {
        let (client, _, launcher, data_dir) = signed_in_client();
        wait_for(&client, |s| {
            matches!(s.standard, StandardState::Ready { .. })
        });
        block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
        block_on(client.connect()).unwrap();
        assert_eq!(client.snapshot().connection.phase, ConnectionPhase::On);
        assert!(backup_exists(&data_dir));

        // Surge turns its system proxy on over ours.
        *launcher.writer.current.lock().unwrap() = Some(vec![("127.0.0.1".into(), 6152)]);
        wait_for(&client, |s| s.connection.phase == ConnectionPhase::Error);
        let connection = client.snapshot().connection;
        assert_eq!(
            connection.reason.map(|reason| (reason.code, reason.detail)),
            Some((
                crate::ErrorCode::NetworkPathContended,
                "SYSTEM_PROXY_TAKEN_OVER".to_string()
            ))
        );
        assert!(connection.retryable);
        assert!(
            !backup_exists(&data_dir),
            "its settings are not ours to restore"
        );
        assert_eq!(
            proxy_toggles(&launcher).last(),
            Some(&false),
            "endpoint closed"
        );

        // Disconnecting from there touches nothing.
        block_on(client.disconnect()).unwrap();
        assert_eq!(client.snapshot().connection.phase, ConnectionPhase::Off);
        assert!(!launcher
            .writer
            .calls()
            .iter()
            .any(|call| call.starts_with("restore")));
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn compatible_mode_does_not_wait_for_a_service_uninstall() {
        let platform = MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#);
        // The admin prompt and the service's stop take a while.
        *platform.uninstall_delay.lock().unwrap() = Some(Duration::from_secs(3));
        let (client, _, _launcher, data_dir) = signed_in_client_on(
            vec![("200 OK", r#"{"revoked":true}"#.into())],
            FakeLauncher::new(),
            platform,
        );
        wait_for(&client, |s| {
            matches!(s.standard, StandardState::Ready { .. })
        });
        let uninstalling = {
            let client = client.clone();
            std::thread::spawn(move || block_on(client.service_uninstall()))
        };
        std::thread::sleep(Duration::from_millis(300));

        let started = Instant::now();
        block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
        block_on(client.connect()).unwrap();
        let took = started.elapsed();
        // Well under the uninstall's 3 s, with room for a slow runner.
        assert!(took < Duration::from_secs(2), "waited {took:?}");
        assert_eq!(client.snapshot().connection.phase, ConnectionPhase::On);
        assert!(
            !uninstalling.is_finished(),
            "the uninstall is still running"
        );

        uninstalling.join().unwrap().unwrap();
        block_on(client.disconnect()).unwrap();
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn a_desktop_without_proxy_settings_is_system_proxy_unavailable() {
        let (client, _, launcher, data_dir) = signed_in_client();
        wait_for(&client, |s| {
            matches!(s.standard, StandardState::Ready { .. })
        });
        block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
        *launcher.writer.fail_apply.lock().unwrap() =
            Some("NO_DESKTOP_PROXY_SETTINGS: neither gsettings nor kwriteconfig found".into());
        let error = block_on(client.connect()).unwrap_err();
        assert_eq!(
            error.info().unwrap().code,
            crate::ErrorCode::SystemProxyUnavailable
        );
        let connection = client.snapshot().connection;
        assert_eq!(connection.phase, ConnectionPhase::Error);
        assert_eq!(
            connection.reason.map(|reason| reason.code),
            Some(crate::ErrorCode::SystemProxyUnavailable)
        );
        // Retrying cannot conjure proxy settings up.
        assert!(!connection.retryable);

        // Any other writer failure stays SystemProxyFailed (retryable).
        *launcher.writer.fail_apply.lock().unwrap() = Some("gsettings: exit status 1".into());
        let error = block_on(client.retry()).unwrap_err();
        assert_eq!(
            error.info().unwrap().code,
            crate::ErrorCode::SystemProxyFailed
        );
        assert!(client.snapshot().connection.retryable);
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn another_apps_proxy_is_reported_and_restored() {
        let (client, _, launcher, data_dir) = signed_in_client();
        wait_for(&client, |s| {
            matches!(s.standard, StandardState::Ready { .. })
        });
        // Surge-like: the OS proxy pointed at a loopback port that is not ours.
        let previous = serde_json::json!({"windows": {
            "ProxyEnable": "0x1", "ProxyServer": "127.0.0.1:6152", "ProxyOverride": "<local>"}});
        *launcher.writer.previous.lock().unwrap() = Some(previous.clone());
        block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
        block_on(client.connect()).unwrap();
        let connection = client.snapshot().connection;
        assert_eq!(connection.phase, ConnectionPhase::On);
        assert!(connection.proxy_was_foreign);

        block_on(client.disconnect()).unwrap();
        let connection = client.snapshot().connection;
        assert!(!connection.proxy_was_foreign);
        assert_eq!(connection.competitor, None);
        // The other app's proxy comes back exactly.
        assert_eq!(
            launcher.writer.calls().last().unwrap(),
            &format!("restore {previous}")
        );

        // Our own endpoint left behind is not someone else's.
        *launcher.writer.previous.lock().unwrap() = Some(serde_json::json!({"windows": {
            "ProxyEnable": "0x1", "ProxyServer": "127.0.0.1:7891"}}));
        block_on(client.connect()).unwrap();
        assert!(!client.snapshot().connection.proxy_was_foreign);
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn switching_compatible_and_enhanced_back_and_forth_never_strands_the_os_proxy() {
        let (client, _, launcher, data_dir) = signed_in_client();
        wait_for(&client, |s| {
            matches!(s.standard, StandardState::Ready { .. })
        });
        // A loopback port nothing listens on, as our endpoint is once
        // compatible mode is off.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        launcher.system_proxy.lock().unwrap().1 = port;
        let macos = |port: u16| {
            let proxy = serde_json::json!({"enabled": true, "server": "127.0.0.1", "port": port.to_string()});
            serde_json::json!({"macos": {"Wi-Fi": {
                "web": proxy, "secure": proxy, "socks": proxy, "bypass": []}}})
        };
        let set_os = |port: u16| *launcher.writer.os.lock().unwrap() = Some(macos(port));
        let os_proxies = || {
            crate::sysproxy::active_proxies(launcher.writer.os.lock().unwrap().as_ref().unwrap())
        };
        // Surge's proxy is on.
        set_os(6152);

        // Compatible: Surge's settings are saved and ours applied.
        block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
        block_on(client.connect()).unwrap();
        assert!(client.snapshot().connection.proxy_was_foreign);
        assert_eq!(os_proxies(), vec![("127.0.0.1".to_string(), port); 3]);

        // Enhanced (it fails here, without a service): Surge's come back.
        let _ = block_on(client.set_connection_mode(crate::ConnectionMode::Enhanced));
        assert_eq!(os_proxies(), vec![("127.0.0.1".to_string(), 6152); 3]);
        // Surge quits and writes back the proxy it last saw: ours.
        set_os(port);

        // Compatible again: that proxy is neither Surge's nor the user's.
        block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
        assert_eq!(client.snapshot().connection.phase, ConnectionPhase::On);
        assert!(!client.snapshot().connection.proxy_was_foreign);

        // Enhanced again: the OS proxy ends off, not on a dead port.
        let _ = block_on(client.set_connection_mode(crate::ConnectionMode::Enhanced));
        assert_eq!(os_proxies(), Vec::new());
        assert!(!backup_exists(&data_dir));

        // Something put our dead port back: enhanced switches it off.
        set_os(port);
        let _ = block_on(client.connect());
        assert_eq!(os_proxies(), Vec::new());

        // A proxy that answers on that port is somebody's: it stays.
        let listener = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        set_os(port);
        let _ = block_on(client.connect());
        assert_eq!(os_proxies(), vec![("127.0.0.1".to_string(), port); 3]);
        drop(listener);
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn sign_out_restores_the_os_proxy() {
        let (client, _, launcher, data_dir) = signed_in_client();
        wait_for(&client, |s| {
            matches!(s.standard, StandardState::Ready { .. })
        });
        block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
        block_on(client.connect()).unwrap();
        block_on(client.logout()).unwrap();
        assert!(launcher
            .writer
            .calls()
            .iter()
            .any(|call| call.starts_with("restore")));
        assert!(!backup_exists(&data_dir));
        assert_eq!(client.snapshot().connection.phase, ConnectionPhase::Off);
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn switching_mode_reconnects_and_enhanced_never_opens_the_endpoint() {
        let (client, _, launcher, data_dir) = signed_in_client();
        wait_for(&client, |s| {
            matches!(s.standard, StandardState::Ready { .. })
        });
        assert_eq!(
            client.snapshot().connection_mode,
            crate::ConnectionMode::Enhanced
        );
        // No reachable service: enhanced fails, but it never touches the
        // system-proxy endpoint or the OS settings.
        assert!(block_on(client.connect()).is_err());
        let snapshot = client.snapshot();
        assert_ne!(snapshot.connection.phase, ConnectionPhase::Off);
        assert!(proxy_toggles(&launcher).iter().all(|enabled| !enabled));
        assert!(launcher.writer.calls().is_empty());

        // Switching while not off reconnects in the new mode.
        block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
        let snapshot = client.snapshot();
        assert_eq!(snapshot.connection_mode, crate::ConnectionMode::Compatible);
        assert_eq!(snapshot.connection.phase, ConnectionPhase::On);
        assert_eq!(proxy_toggles(&launcher).last(), Some(&true));

        // And back: compatible is torn down, the OS settings restored.
        let _ = block_on(client.set_connection_mode(crate::ConnectionMode::Enhanced));
        assert_eq!(
            client.snapshot().connection_mode,
            crate::ConnectionMode::Enhanced
        );
        assert_eq!(proxy_toggles(&launcher).last(), Some(&false));
        assert!(launcher
            .writer
            .calls()
            .last()
            .unwrap()
            .starts_with("restore"));
        assert!(!backup_exists(&data_dir));
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn os_proxy_left_by_a_crash_is_restored_at_launch() {
        let data_dir = std::env::temp_dir()
            .join(format!("ppvpn-crash-test-{}", uuid::Uuid::new_v4()))
            .to_string_lossy()
            .into_owned();
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(
            std::path::Path::new(&data_dir).join("system-proxy-backup.json"),
            r#"{"version":1,"saved":{"fake":"before crash"}}"#,
        )
        .unwrap();
        let launcher = FakeLauncher::new();
        let client = Client::with_parts(
            ClientConfig {
                api_base: "http://127.0.0.1:9".into(),
                data_dir: data_dir.clone(),
                log_dir: data_dir.clone(),
                platform: "macos".into(),
                app_version: "0.0.0-test".into(),
            },
            Arc::new(MemoryPlatform::default()),
            Arc::new(Recorder::default()),
            Arc::new(NoService),
            launcher.clone(),
            launcher.writer.clone(),
        );
        wait_for(&client, |s| matches!(s.auth, AuthState::SignedOut));
        assert_eq!(
            launcher.writer.calls(),
            vec![r#"restore {"fake":"before crash"}"#]
        );
        assert!(!backup_exists(&data_dir));
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn standard_core_traffic_is_reported_while_signed_in() {
        let (client, recorder, _, data_dir) = signed_in_client();
        wait_for(&client, |s| {
            matches!(s.standard, StandardState::Ready { .. })
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !recorder
            .traffic
            .lock()
            .unwrap()
            .iter()
            .any(|sample| sample.up_bps > 0 && sample.down_bps > sample.up_bps)
        {
            assert!(Instant::now() < deadline, "no traffic sample");
            std::thread::sleep(Duration::from_millis(10));
        }

        block_on(client.logout()).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let count = recorder.traffic.lock().unwrap().len();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(
            recorder.traffic.lock().unwrap().len(),
            count,
            "stops at sign-out"
        );
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn profile_starts_standard_probe_reports_and_logout_stops() {
        let (client, recorder, launcher, data_dir) = signed_in_client();

        // Profile change → standard core launched, profile applied, started.
        let snapshot = wait_for(
            &client,
            |s| matches!(&s.standard, StandardState::Ready { revision } if revision == REVISION),
        );
        assert!(matches!(snapshot.auth, AuthState::SignedIn));
        assert!(snapshot.last_error.is_none(), "{:?}", snapshot.last_error);
        assert_eq!(*launcher.launches.lock().unwrap(), 1);
        assert_eq!(launcher.calls("/v1/apply-profile"), 1);
        assert_eq!(launcher.calls("/v1/start"), 1);
        assert_eq!(snapshot.connection.phase, ConnectionPhase::Off);
        assert!(!snapshot.service_installed);

        // Probe with an empty list → one result per profile node.
        block_on(client.probe(ProbeMethod::Tcp, Vec::new())).unwrap();
        let probes = recorder.probes.lock().unwrap().clone();
        assert_eq!(probes.len(), 3);
        assert!(probes.iter().all(|p| p.success && p.latency_ms == Some(42)));
        let mut ids: Vec<_> = probes.iter().map(|p| p.node_id.clone()).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3);

        // Enhanced mode without a reachable service reports an error.
        let error = block_on(client.connect()).unwrap_err();
        assert_eq!(
            error.info().unwrap().code,
            crate::ErrorCode::ServiceUnavailable
        );
        assert_eq!(client.snapshot().connection.phase, ConnectionPhase::Error);
        assert!(client.snapshot().last_error.is_some());

        // Logout → both cores stopped.
        block_on(client.logout()).unwrap();
        let snapshot = client.snapshot();
        assert!(matches!(snapshot.auth, AuthState::SignedOut));
        assert!(matches!(snapshot.standard, StandardState::Stopped));
        assert_eq!(snapshot.connection.phase, ConnectionPhase::Off);
        assert_eq!(launcher.calls("/v1/stop"), 1);
        assert!(matches!(
            block_on(client.probe(ProbeMethod::Icmp, Vec::new())),
            Err(crate::ClientError::StandardNotReady)
        ));

        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn a_preflight_conflict_names_the_app_and_suggests_compatible() {
        let (client, _, _, data_dir) = signed_in_client();
        wait_for(&client, |s| {
            matches!(s.standard, StandardState::Ready { .. })
        });
        // What enhanced mode reports when the pre-connect check blocked TUN.
        client.on_enhanced_state(crate::EnhancedState {
            phase: ConnectionPhase::Error,
            reason: Some(crate::ClientErrorInfo::new(
                crate::ErrorCode::NetworkPathContended,
                "PREFLIGHT_CONFLICT: Mihomo, Clash Verge",
            )),
            retryable: true,
            competitors: vec!["Mihomo".into(), "Clash Verge".into()],
            ..crate::EnhancedState::default()
        });
        let connection = client.snapshot().connection;
        assert_eq!(connection.phase, ConnectionPhase::Error);
        assert_eq!(
            connection.reason.map(|reason| reason.code),
            Some(crate::ErrorCode::NetworkPathContended)
        );
        assert!(connection.retryable);
        assert!(connection.suggest_compatible);
        assert_eq!(connection.competitor.as_deref(), Some("Mihomo"));
        client.shutdown();
        let _ = std::fs::remove_dir_all(data_dir);
    }

    #[test]
    fn shutdown_stops_the_standard_core_and_blocks_restarts() {
        let (client, _, launcher, data_dir) = signed_in_client();
        wait_for(&client, |s| {
            matches!(s.standard, StandardState::Ready { .. })
        });
        client.shutdown();
        assert!(matches!(client.snapshot().standard, StandardState::Stopped));
        assert_eq!(launcher.calls("/v1/stop"), 1);
        // A late profile change must not start a new core.
        client.on_profile_changed();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(*launcher.launches.lock().unwrap(), 1);
        let _ = std::fs::remove_dir_all(data_dir);
    }

    /// The real Linux writer against gsettings (dconf) and, when installed,
    /// kwriteconfig6/5. Off unless `PPVPN_SYSPROXY_IT=1`: the tests rewrite the
    /// desktop's proxy settings, so run them only in a throwaway session, e.g.
    ///
    /// ```sh
    /// HOME=$(mktemp -d) PPVPN_SYSPROXY_IT=1 dbus-run-session -- \
    ///     cargo test --lib linux_os_proxy -- --test-threads=1
    /// ```
    ///
    /// Needs gsettings with a dconf backend (dconf-gsettings-backend and
    /// dconf-service) and the org.gnome.system.proxy schemas.
    #[cfg(target_os = "linux")]
    mod linux_os_proxy {
        use super::*;
        use std::process::Command;

        const G: &[(&str, &str)] = &[
            ("org.gnome.system.proxy", "mode"),
            ("org.gnome.system.proxy", "ignore-hosts"),
            ("org.gnome.system.proxy.http", "host"),
            ("org.gnome.system.proxy.http", "port"),
            ("org.gnome.system.proxy.https", "host"),
            ("org.gnome.system.proxy.https", "port"),
            ("org.gnome.system.proxy.socks", "host"),
            ("org.gnome.system.proxy.socks", "port"),
        ];
        const K: &[&str] = &[
            "ProxyType",
            "httpProxy",
            "httpsProxy",
            "socksProxy",
            "NoProxyFor",
        ];
        /// A non-default user value that has to survive the round trip verbatim.
        const WEIRD: &str = r#"['weird host', "it's", 'a,b', '*.corp.example', '10.1.0.0/16', 'ü.example', 'back\\slash', 'semi;colon']"#;
        const CHILD_DIR: &str = "PPVPN_SYSPROXY_IT_CHILD_DIR";

        /// They share the desktop settings: one at a time.
        static SERIAL: Mutex<()> = Mutex::new(());

        fn enabled() -> bool {
            std::env::var("PPVPN_SYSPROXY_IT").as_deref() == Ok("1")
        }

        fn out(program: &str, args: &[&str]) -> String {
            let output = Command::new(program).args(args).output().unwrap();
            assert!(
                output.status.success(),
                "{program} {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }

        /// kreadconfig/kwriteconfig as the writer picks them (6 before 5).
        fn kde() -> Option<(&'static str, &'static str)> {
            [
                ("kreadconfig6", "kwriteconfig6"),
                ("kreadconfig5", "kwriteconfig5"),
            ]
            .into_iter()
            .find(|(_, write)| Command::new(write).arg("--help").output().is_ok())
        }

        fn kde_args(key: &str) -> Vec<&str> {
            vec![
                "--file",
                "kioslaverc",
                "--group",
                "Proxy Settings",
                "--key",
                key,
            ]
        }

        fn state() -> Vec<(String, String)> {
            let mut all: Vec<(String, String)> = G
                .iter()
                .map(|(schema, key)| {
                    (
                        format!("{schema} {key}"),
                        out("gsettings", &["get", schema, key]),
                    )
                })
                .collect();
            if let Some((read, _)) = kde() {
                all.extend(
                    K.iter()
                        .map(|key| (format!("kde {key}"), out(read, &kde_args(key)))),
                );
            }
            all
        }

        fn get(state: &[(String, String)], key: &str) -> String {
            state
                .iter()
                .find(|(name, _)| name == key)
                .unwrap()
                .1
                .clone()
        }

        fn print(label: &str, state: &[(String, String)]) {
            eprintln!("--- {label}");
            for (key, value) in state {
                eprintln!("  {key} = {value}");
            }
        }

        /// Non-default values (and some left at their defaults / unset).
        fn preset() -> Vec<(String, String)> {
            let gset = |schema: &str, key: &str, value: &str| {
                out("gsettings", &["set", schema, key, value]);
            };
            gset("org.gnome.system.proxy", "mode", "'auto'");
            gset("org.gnome.system.proxy", "ignore-hosts", WEIRD);
            gset("org.gnome.system.proxy.http", "host", "'orig.example'");
            gset("org.gnome.system.proxy.http", "port", "3128");
            gset("org.gnome.system.proxy.socks", "host", "'socks.orig'");
            gset("org.gnome.system.proxy.socks", "port", "1080");
            // https: host unset, port a user value equal to the default.
            out(
                "gsettings",
                &["reset", "org.gnome.system.proxy.https", "host"],
            );
            gset("org.gnome.system.proxy.https", "port", "0");
            if let Some((_, write)) = kde() {
                for (key, value) in [
                    ("ProxyType", "2"),
                    ("httpProxy", "http://orig.example 3128"),
                    ("NoProxyFor", "weird host,it's"),
                ] {
                    let mut args = kde_args(key);
                    args.push(value);
                    out(write, &args);
                }
            }
            let before = state();
            print("before", &before);
            before
        }

        fn assert_applied(state: &[(String, String)]) {
            assert_eq!(get(state, "org.gnome.system.proxy mode"), "'manual'");
            // Both desktops bypass the same hosts and private ranges.
            let hosts: Vec<&str> = crate::sysproxy::BYPASS
                .iter()
                .chain(crate::sysproxy::PRIVATE_RANGES)
                .copied()
                .collect();
            let quoted: Vec<String> = hosts.iter().map(|host| format!("'{host}'")).collect();
            assert_eq!(
                get(state, "org.gnome.system.proxy ignore-hosts"),
                format!("[{}]", quoted.join(", "))
            );
            for scheme in ["http", "https", "socks"] {
                assert_eq!(
                    get(state, &format!("org.gnome.system.proxy.{scheme} host")),
                    "'127.0.0.1'"
                );
                assert_eq!(
                    get(state, &format!("org.gnome.system.proxy.{scheme} port")),
                    "7891"
                );
            }
            if kde().is_some() {
                assert_eq!(get(state, "kde ProxyType"), "1");
                assert_eq!(get(state, "kde httpProxy"), "http://127.0.0.1 7891");
                assert_eq!(get(state, "kde httpsProxy"), "http://127.0.0.1 7891");
                assert_eq!(get(state, "kde socksProxy"), "socks://127.0.0.1 7891");
                assert_eq!(get(state, "kde NoProxyFor"), hosts.join(","));
            }
        }

        /// After a restore the https keys (default in `preset`) are back
        /// to the schema default, not user values equal to it. Needs the
        /// `dconf` CLI (dconf-cli); skipped without it.
        fn assert_defaults_not_written() {
            if Command::new("dconf").arg("help").output().is_err() {
                eprintln!("dconf not installed: user-value check skipped");
                return;
            }
            for path in ["/system/proxy/https/host", "/system/proxy/https/port"] {
                assert_eq!(out("dconf", &["read", path]), "", "{path} left in dconf");
            }
        }

        fn has_backup(data_dir: &str) -> bool {
            std::path::Path::new(data_dir)
                .join("system-proxy-backup.json")
                .is_file()
        }

        fn new_dir() -> String {
            let dir =
                std::env::temp_dir().join(format!("ppvpn-sysproxy-it-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            dir.to_string_lossy().into_owned()
        }

        fn config(api_base: String, data_dir: &str) -> ClientConfig {
            ClientConfig {
                api_base,
                data_dir: data_dir.into(),
                log_dir: data_dir.into(),
                platform: "linux".into(),
                app_version: "0.0.0-test".into(),
            }
        }

        /// Signed in against a scripted backend, standard core ready.
        fn signed_in(data_dir: &str) -> Arc<Client> {
            let (base, _) = serve(vec![
                ("200 OK", r#"{"refresh_token":"jyr_prepared"}"#.to_string()),
                ("200 OK", token_set("jyr_active")),
                ("200 OK", r#"{"id":"u1","name":"Jerry","avatar":""}"#.into()),
                (
                    "200 OK",
                    r#"{"items":[{"id":"t1","name":"Personal","is_personal":true,"is_default":true}]}"#.into(),
                ),
                ("200 OK", FIXTURE.into()),
                ("200 OK", r#"{"revoked":true}"#.into()),
            ]);
            let client = Client::with_parts(
                config(base, data_dir),
                MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#),
                Arc::new(Recorder::default()),
                Arc::new(NoService),
                FakeLauncher::new(),
                crate::sysproxy::platform_writer(),
            );
            wait_for(&client, |s| {
                matches!(s.standard, StandardState::Ready { .. })
            });
            client
        }

        fn connect_compatible(client: &Client) {
            block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
            block_on(client.connect()).unwrap();
        }

        #[test]
        fn linux_os_proxy_round_trip() {
            if !enabled() {
                return;
            }
            let _serial = SERIAL
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let before = preset();
            let data_dir = new_dir();
            let client = signed_in(&data_dir);
            connect_compatible(&client);
            let applied = state();
            print("connected (compatible)", &applied);
            assert_applied(&applied);
            assert!(has_backup(&data_dir));

            block_on(client.disconnect()).unwrap();
            let after = state();
            print("disconnected", &after);
            assert_eq!(after, before);
            assert!(!has_backup(&data_dir));
            assert_defaults_not_written();
            client.shutdown();
            let _ = std::fs::remove_dir_all(data_dir);
        }

        /// The process `linux_os_proxy_restored_after_a_kill` spawns: connects
        /// in compatible mode, then dies without any cleanup.
        #[test]
        fn linux_os_proxy_killed_child() {
            let Ok(data_dir) = std::env::var(CHILD_DIR) else {
                return;
            };
            let client = signed_in(&data_dir);
            connect_compatible(&client);
            std::process::abort();
        }

        #[test]
        fn linux_os_proxy_restored_after_a_kill() {
            if !enabled() {
                return;
            }
            let _serial = SERIAL
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let before = preset();
            let data_dir = new_dir();
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "cores::tests::linux_os_proxy::linux_os_proxy_killed_child",
                    "--nocapture",
                ])
                .env(CHILD_DIR, &data_dir)
                .status()
                .unwrap();
            assert!(!status.success(), "the child was meant to die");
            let left = state();
            print("after the kill", &left);
            assert_applied(&left);
            assert!(has_backup(&data_dir));

            // The next launch restores the leftovers, signed in or not.
            let client = Client::with_parts(
                config("http://127.0.0.1:9".into(), &data_dir),
                Arc::new(MemoryPlatform::default()),
                Arc::new(Recorder::default()),
                Arc::new(NoService),
                FakeLauncher::new(),
                crate::sysproxy::platform_writer(),
            );
            wait_for(&client, |s| matches!(s.auth, AuthState::SignedOut));
            let deadline = Instant::now() + Duration::from_secs(10);
            while has_backup(&data_dir) {
                assert!(Instant::now() < deadline, "the backup was not restored");
                std::thread::sleep(Duration::from_millis(50));
            }
            let after = state();
            print("after the next launch", &after);
            assert_eq!(after, before);
            assert_defaults_not_written();
            client.shutdown();
            let _ = std::fs::remove_dir_all(data_dir);
        }

        #[test]
        fn linux_os_proxy_switching_methods_while_connected() {
            if !enabled() {
                return;
            }
            let _serial = SERIAL
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let before = preset();
            let data_dir = new_dir();
            let client = signed_in(&data_dir);
            connect_compatible(&client);
            assert_applied(&state());

            // To enhanced: there is no service here, so enhanced fails, but
            // compatible is torn down and the settings come back.
            let enhanced = block_on(client.set_connection_mode(crate::ConnectionMode::Enhanced));
            eprintln!("switch to enhanced: {:?}", enhanced.map(|_| ()));
            let after_switch = state();
            print("switched to enhanced", &after_switch);
            assert_eq!(after_switch, before);
            assert!(!has_backup(&data_dir));

            // Back to compatible while not off: it reconnects there.
            block_on(client.set_connection_mode(crate::ConnectionMode::Compatible)).unwrap();
            assert_eq!(client.snapshot().connection.phase, ConnectionPhase::On);
            assert_applied(&state());
            block_on(client.disconnect()).unwrap();
            assert_eq!(state(), before);
            assert_defaults_not_written();
            client.shutdown();
            let _ = std::fs::remove_dir_all(data_dir);
        }
    }
}
