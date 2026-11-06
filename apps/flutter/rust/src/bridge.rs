//! The client behind [`crate::api`]: one per process, its listener and the
//! conversions to the flat types Dart sees. Kept out of `api` so that the
//! codegen, which reads only that module, sees no client type.
//!
//! Events are not queued: the listener keeps the latest snapshot and the
//! latest traffic sample, and `api::next_events` hands over whatever is
//! there. A UI that falls behind (window hidden, isolate busy) gets the
//! newest state, and memory stays the same however long it waits. Traffic
//! comes at most once a second (the client's own rate).

use std::sync::{Arc, Mutex, OnceLock};

use ppvpn_client::{
    AuthState, Client, ClientConfig, ClientError, ClientErrorInfo, ClientListener, ClientSnapshot,
    ConnectionMode, ConnectionPhase, ConnectionState, ErrorCode, Node, ProbeMethod, ProbeResult,
    ProfileStatus, RoutingMode, StandardState, TrafficSample,
};
use tokio::sync::Notify;

use crate::api::{
    UiAuth, UiConnectionMode, UiContended, UiDeviceCode, UiError, UiEvents, UiLine, UiLocalProxy,
    UiNode, UiPhase, UiProbe, UiProbeMethod, UiProfile, UiRoutingMode, UiSnapshot, UiTraffic,
};
use crate::hooks::Hooks;

/// The backend the app signs in to, compiled in from
/// `PPVPN_API_BASE_DEFAULT`: ci.yml and release.yml pass the GitHub
/// variable `PPVPN_API_BASE`, a dev release the secret of that name (#517), and
/// scripts/ci-build.sh refuses to build without it. No address is written
/// here. `PPVPN_API_BASE` overrides it, else the settings page's API
/// override (`settings.json`; debug builds only, #519).
const DEFAULT_API_BASE: Option<&str> = match option_env!("PPVPN_API_BASE_DEFAULT") {
    Some(base) if !base.is_empty() => Some(base),
    _ => None,
};

pub(crate) struct Bridge {
    pub(crate) client: Arc<Client>,
    pub(crate) events: Arc<Events>,
    /// Runs the client's async calls: the api awaits them from
    /// flutter_rust_bridge's executor, which is not tokio.
    runtime: tokio::runtime::Runtime,
}

static BRIDGE: OnceLock<Bridge> = OnceLock::new();

#[derive(Default)]
pub(crate) struct Events {
    pub(crate) pending: Mutex<UiEvents>,
    pub(crate) ready: Notify,
    proxies: Mutex<ProxyGeneration>,
}

#[derive(Default)]
struct ProxyGeneration {
    source: Option<ProxySource>,
    generation: u32,
}

#[derive(PartialEq, Eq)]
struct ProxySource {
    account: Option<String>,
    team: Option<String>,
    revision: String,
}

impl ProxyGeneration {
    fn update(&mut self, snapshot: &ClientSnapshot) -> Option<u32> {
        let source = match (&snapshot.auth, &snapshot.standard) {
            (AuthState::SignedIn, StandardState::Ready { revision }) => Some(ProxySource {
                account: snapshot.account.as_ref().map(|account| account.id.clone()),
                team: snapshot.team.as_ref().map(|team| team.id.clone()),
                revision: revision.clone(),
            }),
            _ => None,
        };
        if source != self.source {
            self.generation = self.generation.wrapping_add(1);
            self.source = source;
        }
        self.current()
    }

    fn current(&self) -> Option<u32> {
        self.source.as_ref().map(|_| self.generation)
    }
}

impl Events {
    pub(crate) fn initial_snapshot(&self, read: impl FnOnce() -> ClientSnapshot) -> UiSnapshot {
        // Serialize the initial read with the listener. If an update already
        // arrived, use its token instead of rewinding the tracker with an
        // earlier client snapshot captured by start().
        let pending = self.pending.lock().unwrap();
        pending
            .snapshot
            .clone()
            .unwrap_or_else(|| self.snapshot(&read()))
    }

    fn snapshot(&self, snapshot: &ClientSnapshot) -> UiSnapshot {
        // Track every transition before pending events are coalesced. A core
        // restart can otherwise look like Ready(R1) -> Ready(R1) to Dart.
        let generation = self.proxies.lock().unwrap().update(snapshot);
        ui_snapshot(snapshot, generation)
    }

    fn check_proxy_generation(&self, generation: u32) -> Result<(), String> {
        if self.proxies.lock().unwrap().current() == Some(generation) {
            Ok(())
        } else {
            Err("local proxy configuration changed".into())
        }
    }
}

struct Listener(Arc<Events>);

impl ClientListener for Listener {
    fn on_snapshot(&self, snapshot: ClientSnapshot) {
        let mut pending = self.0.pending.lock().unwrap();
        pending.snapshot = Some(self.0.snapshot(&snapshot));
        drop(pending);
        self.0.ready.notify_one();
    }

    fn on_probe_result(&self, result: ProbeResult) {
        let probe = UiProbe {
            timed_out: result
                .error
                .as_ref()
                .is_some_and(|error| error.code == ErrorCode::Timeout),
            node_id: result.node_id,
            success: result.success,
            latency_ms: result.latency_ms,
        };
        let mut pending = self.0.pending.lock().unwrap();
        pending.probes.retain(|old| old.node_id != probe.node_id);
        pending.probes.push(probe);
        drop(pending);
        self.0.ready.notify_one();
    }

    fn on_traffic(&self, sample: TrafficSample) {
        self.0.pending.lock().unwrap().traffic = Some(UiTraffic {
            up: i64::try_from(sample.up_bps).unwrap_or(i64::MAX),
            down: i64::try_from(sample.down_bps).unwrap_or(i64::MAX),
        });
        self.0.ready.notify_one();
    }
}

pub(crate) fn get() -> Result<&'static Bridge, String> {
    BRIDGE
        .get()
        .ok_or_else(|| "start() was not called".to_string())
}

pub(crate) fn running() -> Option<&'static Bridge> {
    BRIDGE.get()
}

/// Creates the client once. The connection mode is the client's persisted
/// one: the overview's two switches choose it (`api::set_connection_mode`).
pub(crate) fn start() -> Result<&'static Bridge, String> {
    if let Some(bridge) = BRIDGE.get() {
        return Ok(bridge);
    }
    let (data_dir, log_dir) = crate::dirs::dirs()?;
    // What the backend knows the device by: device sign-in, the device
    // name, push registration (#535).
    let platform = if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "ios") {
        "ios"
    } else if cfg!(target_os = "android") {
        "android"
    } else {
        "linux"
    };
    let api_base = std::env::var("PPVPN_API_BASE")
        .ok()
        .filter(|base| !base.is_empty())
        .or_else(|| crate::settings::api_override(&data_dir))
        .or_else(|| DEFAULT_API_BASE.map(str::to_string))
        .ok_or("this build has no backend: build it with PPVPN_API_BASE_DEFAULT")?;
    let config = ClientConfig {
        api_base,
        data_dir: data_dir.clone(),
        log_dir,
        platform: platform.into(),
        app_version: format!("{}-flutter", crate::update::version()),
    };
    let events = Arc::new(Events::default());
    let client = Client::new(
        config,
        Arc::new(Hooks::new(&data_dir)),
        Arc::new(Listener(events.clone())),
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .thread_name("ppvpn-flutter")
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    if std::env::var("PPVPN_FLUTTER_SYNTHETIC_TRAFFIC").is_ok_and(|value| value == "1") {
        runtime.spawn(synthetic_traffic(events.clone()));
    }
    // Two first calls at once: the loser's client is dropped unused.
    let agent_client = client.clone();
    if BRIDGE
        .set(Bridge {
            client,
            events,
            runtime,
        })
        .is_ok()
    {
        crate::push_agent::start(&agent_client, &data_dir);
    }
    get()
}

/// compare.py's memory curve (#387): a traffic sample every second, through
/// the listener's path, as a signed-in client's would come. Measurement
/// only; off unless `PPVPN_FLUTTER_SYNTHETIC_TRAFFIC=1`.
async fn synthetic_traffic(events: Arc<Events>) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    let mut n: i64 = 0;
    loop {
        tick.tick().await;
        n += 1;
        events.pending.lock().unwrap().traffic = Some(UiTraffic {
            up: 200_000 + (n * 7_919) % 1_000_000,
            down: 2_000_000 + (n * 104_729) % 8_000_000,
        });
        events.ready.notify_one();
    }
}

impl Bridge {
    /// Runs one of the client's async calls on the bridge's runtime.
    pub(crate) async fn call<F, Fut>(&self, f: F) -> Result<(), String>
    where
        F: FnOnce(Arc<Client>) -> Fut,
        Fut: std::future::Future<Output = Result<(), ClientError>> + Send + 'static,
    {
        self.fetch(f).await
    }

    /// Runs `future` on the bridge's runtime (the HTTP client needs one).
    pub(crate) async fn run<T: Send + 'static>(
        &self,
        future: impl std::future::Future<Output = T> + Send + 'static,
    ) -> Result<T, String> {
        self.runtime
            .spawn(future)
            .await
            .map_err(|error| error.to_string())
    }

    /// [`Bridge::call`] for a call that returns a value.
    pub(crate) async fn fetch<T, F, Fut>(&self, f: F) -> Result<T, String>
    where
        T: Send + 'static,
        F: FnOnce(Arc<Client>) -> Fut,
        Fut: std::future::Future<Output = Result<T, ClientError>> + Send + 'static,
    {
        self.runtime
            .spawn(f(self.client.clone()))
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
    }
}

impl Bridge {
    pub(crate) async fn local_proxies(&self, generation: u32) -> Result<Vec<UiLocalProxy>, String> {
        self.events.check_proxy_generation(generation)?;
        let client = self.client.clone();
        let proxies = self
            .runtime
            .spawn(async move { client.local_proxies().await })
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        self.events.check_proxy_generation(generation)?;
        Ok(proxies
            .into_iter()
            .map(|proxy| UiLocalProxy {
                node_id: proxy.node_id,
                host: proxy.host,
                port: proxy.port,
                username: proxy.username,
                password: proxy.password,
            })
            .collect())
    }
}

pub(crate) fn connection_mode(mode: UiConnectionMode) -> ConnectionMode {
    match mode {
        UiConnectionMode::Enhanced => ConnectionMode::Enhanced,
        UiConnectionMode::Compatible => ConnectionMode::Compatible,
    }
}

pub(crate) fn probe_method(method: UiProbeMethod) -> ProbeMethod {
    match method {
        UiProbeMethod::Icmp => ProbeMethod::Icmp,
        UiProbeMethod::Tcp => ProbeMethod::Tcp,
        UiProbeMethod::Connect => ProbeMethod::Connect,
    }
}

pub(crate) fn routing_mode(mode: UiRoutingMode) -> RoutingMode {
    match mode {
        UiRoutingMode::Rules => RoutingMode::Rules,
        UiRoutingMode::Global => RoutingMode::Global,
    }
}

fn ui_error(info: &ClientErrorInfo) -> UiError {
    UiError {
        code: format!("{:?}", info.code),
        detail: info.detail.clone(),
    }
}

/// What holds enhanced mode while it is contended (`ppvpn_client`'s
/// `ServiceBusy`, `ServiceOwnedByAnotherUser`, or `NetworkPathContended`
/// with `TUN_ROUTING_TAKEN_OVER` or another detail).
fn ui_contended(connection: &ConnectionState) -> Option<UiContended> {
    if connection.phase != ConnectionPhase::Contended {
        return None;
    }
    let reason = connection.reason.as_ref();
    Some(match reason.map(|reason| reason.code) {
        Some(ErrorCode::ServiceBusy) => UiContended::AnotherSession,
        Some(ErrorCode::ServiceOwnedByAnotherUser) => UiContended::AnotherUser,
        _ if reason.is_some_and(|reason| reason.detail == "TUN_ROUTING_TAKEN_OVER") => {
            UiContended::RoutesTaken
        }
        _ => UiContended::NetworkTaken,
    })
}

/// Enhanced is on and its reason says only the API host is unreachable on
/// the direct path (`HEALTH_API_UNREACHABLE`).
fn api_unreachable(connection: &ConnectionState) -> bool {
    connection.phase == ConnectionPhase::On
        && connection
            .reason
            .as_ref()
            .is_some_and(|reason| reason.detail == "HEALTH_API_UNREACHABLE")
}

fn ui_snapshot(snapshot: &ClientSnapshot, local_proxy_generation: Option<u32>) -> UiSnapshot {
    let (auth, device_code) = match &snapshot.auth {
        AuthState::Restoring => (UiAuth::Restoring, None),
        AuthState::SignedOut => (UiAuth::SignedOut, None),
        AuthState::AwaitingBrowser { code } => (
            UiAuth::AwaitingBrowser,
            Some(UiDeviceCode {
                user_code: code.user_code.clone(),
                verification_url: code.verification_url.clone(),
                expires_in_secs: code.expires_in_secs,
                browser_opened: code.browser_opened,
            }),
        ),
        AuthState::SignedIn => (UiAuth::SignedIn, None),
    };
    let profile = match &snapshot.profile_status {
        ProfileStatus::Loading => UiProfile::Loading,
        ProfileStatus::Ready => UiProfile::Ready,
        ProfileStatus::NoSubscription => UiProfile::NoSubscription,
        ProfileStatus::SubscriptionExpired { .. } => UiProfile::SubscriptionExpired,
        ProfileStatus::TeamDisabled => UiProfile::TeamDisabled,
        ProfileStatus::Invalid { .. } => UiProfile::Invalid,
        ProfileStatus::NodesNotReady => UiProfile::NodesNotReady,
    };
    let connection = &snapshot.connection;
    UiSnapshot {
        auth,
        device_code,
        account_name: snapshot
            .account
            .as_ref()
            .map(|account| account.name.clone()),
        team_name: snapshot.team.as_ref().map(|team| team.name.clone()),
        team_id: snapshot.team.as_ref().map(|team| team.id.clone()),
        profile,
        profile_previous: matches!(
            snapshot.profile_status,
            ProfileStatus::Invalid { previous: true, .. }
        ),
        profile_revision: snapshot.profile.as_ref().map(|p| p.revision.clone()),
        expires_at: snapshot.profile.as_ref().map(|p| p.expires_at.clone()),
        phase: match connection.phase {
            ConnectionPhase::Off => UiPhase::Off,
            ConnectionPhase::Preparing => UiPhase::Preparing,
            ConnectionPhase::WaitingPermission => UiPhase::WaitingPermission,
            ConnectionPhase::Connecting => UiPhase::Connecting,
            ConnectionPhase::On => UiPhase::Connected,
            ConnectionPhase::Reconnecting => UiPhase::Reconnecting,
            ConnectionPhase::Contended => UiPhase::Contended,
            ConnectionPhase::Disconnecting => UiPhase::Disconnecting,
            ConnectionPhase::Error => UiPhase::Error,
        },
        line_key: connection.detail.endpoint_key.clone(),
        latency_ms: connection.detail.latency_ms,
        standard_ready: matches!(snapshot.standard, StandardState::Ready { .. }),
        local_proxy_generation,
        selected_node_id: snapshot.selected_node_id.clone(),
        error: connection
            .reason
            .as_ref()
            .or(snapshot.last_error.as_ref())
            .map(ui_error),
        unread: snapshot.unread_notifications,
        routing_mode: match snapshot.routing_mode {
            RoutingMode::Rules => UiRoutingMode::Rules,
            RoutingMode::Global => UiRoutingMode::Global,
        },
        connection_mode: match snapshot.connection_mode {
            ConnectionMode::Enhanced => UiConnectionMode::Enhanced,
            ConnectionMode::Compatible => UiConnectionMode::Compatible,
        },
        service_installed: snapshot.service_installed,
        can_take_over: connection.can_take_over,
        suggest_compatible: connection.suggest_compatible,
        contended: ui_contended(connection),
        api_unreachable: api_unreachable(connection),
    }
}

pub(crate) fn ui_node(node: Node) -> UiNode {
    UiNode {
        id: node.id,
        name: node.name,
        exit_region: node.exit_region,
        exit_country_code: node.exit_country_code,
        entry_label: node.entry_label,
        entry_key: node.entry_key,
        udp: node.udp,
        lines: node
            .replicas
            .into_iter()
            .map(|replica| UiLine {
                key: replica.endpoint_key,
                label: replica.label.filter(|label| !label.is_empty()),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ppvpn_client::{Account, ProfileSummary, Team};

    fn ready(revision: &str) -> ClientSnapshot {
        ClientSnapshot {
            auth: AuthState::SignedIn,
            account: Some(Account {
                id: "account-a".into(),
                name: "Example".into(),
                avatar_url: None,
                email: None,
            }),
            team: Some(Team {
                id: "team-a".into(),
                name: "Example".into(),
                personal: true,
                active: true,
            }),
            profile: Some(ProfileSummary {
                revision: revision.into(),
                expires_at: String::new(),
                node_count: 1,
            }),
            standard: StandardState::Ready {
                revision: revision.into(),
            },
            ..ClientSnapshot::default()
        }
    }

    #[test]
    fn proxy_generation_follows_applied_revision_not_profile_arrival() {
        let events = Events::default();
        let mut snapshot = ready("r1");
        let initial = events.snapshot(&snapshot).local_proxy_generation.unwrap();
        snapshot.profile.as_mut().unwrap().revision = "r2".into();
        assert_eq!(
            events.snapshot(&snapshot).local_proxy_generation,
            Some(initial)
        );
        // A failed application keeps Ready(r1) and must not label its ports r2.
        snapshot.profile_status = ProfileStatus::Invalid {
            previous: true,
            error: ClientErrorInfo::new(ErrorCode::StandardCoreFailed, "apply failed"),
        };
        assert_eq!(
            events.snapshot(&snapshot).local_proxy_generation,
            Some(initial)
        );
        snapshot.standard = StandardState::Ready {
            revision: "r2".into(),
        };
        let applied = events.snapshot(&snapshot).local_proxy_generation.unwrap();
        assert_ne!(applied, initial);
        assert!(events.check_proxy_generation(initial).is_err());
        assert!(events.check_proxy_generation(applied).is_ok());
    }

    #[test]
    fn proxy_generation_survives_coalesced_restart_events() {
        let events = Arc::new(Events::default());
        let listener = Listener(events.clone());
        let mut snapshot = ready("r1");
        let initial = events.snapshot(&snapshot).local_proxy_generation.unwrap();
        listener.on_snapshot(snapshot.clone());
        assert_eq!(
            events
                .pending
                .lock()
                .unwrap()
                .snapshot
                .as_ref()
                .unwrap()
                .local_proxy_generation,
            Some(initial)
        );
        snapshot.standard = StandardState::Stopped;
        listener.on_snapshot(snapshot.clone());
        assert!(events.check_proxy_generation(initial).is_err());
        snapshot.standard = StandardState::Starting;
        listener.on_snapshot(snapshot.clone());
        snapshot.standard = StandardState::Ready {
            revision: "r1".into(),
        };
        listener.on_snapshot(snapshot);
        let latest = events.pending.lock().unwrap().snapshot.take().unwrap();
        assert!(latest.standard_ready);
        assert_ne!(latest.local_proxy_generation, Some(initial));
    }

    #[test]
    fn initial_snapshot_uses_an_already_published_generation() {
        let events = Arc::new(Events::default());
        let listener = Listener(events.clone());
        let first = events.initial_snapshot(|| ready("r1"));
        listener.on_snapshot(ready("r2"));
        let latest = events.initial_snapshot(|| panic!("must use the published snapshot"));
        assert_eq!(latest.profile_revision.as_deref(), Some("r2"));
        assert_ne!(latest.local_proxy_generation, first.local_proxy_generation);
        assert!(events
            .check_proxy_generation(latest.local_proxy_generation.unwrap())
            .is_ok());
        assert_eq!(
            events.pending.lock().unwrap().snapshot.as_ref(),
            Some(&latest)
        );
    }

    #[test]
    fn proxy_generation_invalidates_account_team_and_sign_out() {
        let events = Events::default();
        let mut snapshot = ready("r1");
        let first = events.snapshot(&snapshot).local_proxy_generation;
        snapshot.account.as_mut().unwrap().id = "account-b".into();
        let second = events.snapshot(&snapshot).local_proxy_generation;
        assert_ne!(first, second);
        snapshot.team.as_mut().unwrap().id = "team-b".into();
        let third = events.snapshot(&snapshot).local_proxy_generation;
        assert_ne!(second, third);
        snapshot.auth = AuthState::SignedOut;
        assert_eq!(events.snapshot(&snapshot).local_proxy_generation, None);
        assert!(events.check_proxy_generation(third.unwrap()).is_err());
        snapshot.auth = AuthState::SignedIn;
        assert_ne!(events.snapshot(&snapshot).local_proxy_generation, third);
    }
}
