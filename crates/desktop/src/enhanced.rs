//! Enhanced mode: transparent routing through the privileged service, which
//! runs `ppvpn-core serve --tun --local-proxy=false`.
//!
//! Every connection attempt gets a fresh `session_id` and a monotonically
//! increasing `generation`; the service only accepts calls from that exact
//! owner, and every state transition names the generation it belongs to so a
//! stale background task can never overwrite a newer attempt. While on,
//! per-generation loops run: lease renewal (15 s; the service stops the core
//! 45 s after the last renewal), the data-plane health monitor (30 s), and
//! the service watch, a long-lived connection over which the service reports
//! at once that it is stopping or that the core stopped (and whose EOF means
//! the service is gone). A failed renewal or health check, or such a watch
//! event, triggers one automatic reconnect per episode, after which the
//! phase becomes `Contended` until the user retries. With a service that
//! predates the watch, only the renewal notices a lost core. TUN routing
//! another program changed (macOS, Windows) is reconnected at most
//! [`TUN_ROUTING_RECONNECTS`] times in [`TUN_ROUTING_WINDOW`]; after that
//! the phase is `Contended` (`TUN_ROUTING_TAKEN_OVER`) until the user
//! retries.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use ppvpn_engine_host::health::{
    Health, HealthTiming, RecoveryEpisode, Round, RoutingReconnects, Verdict,
    TUN_ROUTING_RECONNECTS, TUN_ROUTING_WINDOW,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::core_ipc::{self, BoxFuture, CoreTraffic, CoreTransport};
use crate::detect::ConflictReport;
use crate::errors::{ClientError, ClientErrorInfo, ErrorCode};
use crate::service::{ServiceApi, ServiceCoreTransport, ServiceError, SessionRef, WatchEnd};
use crate::standard::StandardCore;
use crate::{
    ClientConfig, ConnectionPhase, EnhancedState, PlatformError, PlatformHooks, RoutingMode,
    TrafficSample,
};

const LEASE_RENEW_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(15)
};
const HEALTH_INTERVAL: Duration = Duration::from_secs(30);
/// How long to wait for a freshly installed service to answer `GetVersion`.
const SERVICE_START_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(500)
} else {
    Duration::from_secs(10)
};
const SERVICE_POLL_INTERVAL: Duration = Duration::from_millis(200);
/// How long to wait for the service to confirm that a session's core is
/// gone after `Disconnect` did not answer: the stop itself (~17 s worst
/// case), or the 45 s lease lapsing if the request never arrived.
const DISCONNECT_CONFIRM_TIMEOUT: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(55)
};
const DISCONNECT_CONFIRM_POLL: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_millis(500)
};
/// First pause before reconnecting to a service that is stopping or not
/// running; doubled per attempt up to [`SERVICE_BACKOFF_MAX`].
const SERVICE_BACKOFF_BASE: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(2)
};
const SERVICE_BACKOFF_MAX: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(30)
};
/// Automatic reconnect attempts while the service is down (~2 minutes).
const SERVICE_BACKOFF_ATTEMPTS: u32 = 7;
/// How long releasing a session (before a reconnect, or before an
/// uninstall) waits for the service's answer. The service stops the core
/// before it answers, which can take ~17 s; nothing here depends on the
/// answer, the next Connect waits for the stop anyway.
const RELEASE_BUDGET: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(3)
};
/// Detail of the `Error` a disconnect ends in when the service never
/// confirmed that the core stopped.
const DISCONNECT_UNCONFIRMED: &str = "DISCONNECT_UNCONFIRMED";
/// The backend's unauthenticated health route.
const HEALTH_PATH: &str = "/api/v1/health";
/// Deadline for the direct health request (connect and whole response).
/// Right after the TUN starts its DNS and the node connection are cold
/// (a lookup through the node, then TLS to a CDN far away): 5 s timed out
/// where the repeat passed in 4 s. As the proxied-path probe: 8 s.
const HEALTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// Budget of the pre-connect conflict check (it runs OS commands). It starts
/// with the attempt, so it overlaps the service check; past it the connect
/// goes ahead unchecked, as before the check existed.
/// Pause before the one repeat of a failed first health check.
const HEALTH_SETTLE: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(2)
};
/// Pause after the OS reported a network change before the check it brings
/// forward: the new path (address, routes, DNS) takes a moment to settle.
const NETWORK_SETTLE: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(2)
};
/// After a network change, a failing path check is the network settling
/// (interface down, DHCP, Wi-Fi joining), not a broken tunnel: for this long
/// the core is kept and the path re-checked every [`NETWORK_RECHECK`];
/// restarting it would only fail the same way (measured: three reconnects and
/// 13 s back to On for a path that was up again after 7 s).
const NETWORK_GRACE: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(30)
};
/// Shortest reconnect pause a network change can cut short (see recovery).
const NETWORK_WAKE_MIN_PAUSE: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(10)
};
/// Entrance probe timeout of a health check; shorter while the network
/// settles after a change, where a probe right after the interface returned
/// took the full 5 s to fail (measured) and so delayed the next try.
const ENTRANCE_PROBE_TIMEOUT_MS: u64 = 5_000;
const SETTLING_ENTRANCE_PROBE_TIMEOUT_MS: u64 = 2_000;
const NETWORK_RECHECK: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(2)
};
/// Pause before the quick recheck of a periodic check that failed on the
/// network path: a lost probe on a lossy link is not a broken tunnel, so a
/// path failure is recovered from only when the recheck and the next round
/// fail too (#438).
const HEALTH_RECHECK: Duration = if cfg!(test) {
    Duration::from_millis(20)
} else {
    Duration::from_secs(5)
};
/// Detail of the health failure where the direct request to the API host
/// failed and the tunnel's own steps passed (see [`official_verdict`]), and
/// of the reason `On` carries meanwhile.
const API_UNREACHABLE: &str = "HEALTH_API_UNREACHABLE";
/// Detail of the `Contended` that ends those reconnects.
const TUN_ROUTING_TAKEN_OVER: &str = "TUN_ROUTING_TAKEN_OVER";
const PREFLIGHT_BUDGET: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_millis(1_500)
};

/// The official host's direct and proxied results, scripted (tests).
#[cfg(test)]
pub(crate) type OfficialSteps =
    Arc<dyn Fn() -> (Result<(), ClientErrorInfo>, Result<(), ClientErrorInfo>) + Send + Sync>;

/// Looks for other apps' tunnels (blocking; run off the async threads).
pub(crate) type Detector = Arc<dyn Fn() -> ConflictReport + Send + Sync>;

/// The machine's own detector; tests default to "nothing found" so they
/// never depend on the host's network.
pub(crate) fn default_detector() -> Detector {
    if cfg!(test) {
        Arc::new(ConflictReport::default)
    } else {
        Arc::new(crate::detect::detect_now)
    }
}

/// The time the TUN-routing reconnect budget counts by (tests move it).
pub(crate) type Clock = Arc<dyn Fn() -> Instant + Send + Sync>;

fn system_clock() -> Clock {
    Arc::new(Instant::now)
}

pub(crate) type StateSink = Arc<dyn Fn(EnhancedState) + Send + Sync>;
/// Background failures that are not part of the phase (e.g. persisting the
/// connection state).
pub(crate) type ErrorSink = Arc<dyn Fn(ClientErrorInfo) + Send + Sync>;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(crate) struct EnhancedConfig {
    pub api_base: String,
    /// `<data_dir>/enhanced-connection.json` (was `connection/state.json`).
    pub state_file: PathBuf,
    /// Build id of the service shipped with this client
    /// ([`crate::service::SERVICE_BUILD_ID`]). An installed service that
    /// reports another one, or none (older services), is reinstalled before
    /// the next connect. `None` (id unknown) never reinstalls.
    pub expected_service_build_id: Option<String>,
    /// An installed service that refuses this client is reinstalled once
    /// before a connect (macOS: a service from before the Developer ID
    /// signature pins the old certificate, #471).
    pub reinstall_rejecting_service: bool,
    /// Sent with every Connect / UpdateProfile (shared with the client).
    pub routing: crate::routing::RoutingModeCell,
    /// Ingress pins, sent with every Connect / UpdateProfile (shared with
    /// the client).
    pub ingress_pins: crate::ingress::PinsCell,
    /// The selected node, sent with every Connect / UpdateProfile (the
    /// client's).
    pub selection: crate::session::SelectionSource,
}

impl EnhancedConfig {
    pub(crate) fn from_client(config: &ClientConfig) -> Self {
        Self {
            api_base: config.api_base.clone(),
            state_file: PathBuf::from(&config.data_dir).join(crate::storage::ENHANCED_STATE_FILE),
            expected_service_build_id: crate::service::expected_service_build_id(),
            reinstall_rejecting_service: cfg!(target_os = "macos"),
            routing: Default::default(),
            ingress_pins: Default::default(),
            selection: Default::default(),
        }
    }
}

// ---------------------------------------------------------------------------
// Pure state machine
// ---------------------------------------------------------------------------

/// Connection bookkeeping: the current snapshot plus the desired state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Machine {
    pub phase: ConnectionPhase,
    pub session_id: Option<String>,
    pub generation: u64,
    pub revision: Option<String>,
    pub reason: Option<ClientErrorInfo>,
    pub retryable: bool,
    /// The user wants enhanced mode on (persisted across launches).
    pub desired_on: bool,
    /// Apps the pre-connect conflict check found owning the network (set
    /// with the `PREFLIGHT_CONFLICT` error; cleared by any transition out of
    /// `Error` / `Contended`).
    pub competitors: Vec<String>,
}

impl Machine {
    /// Starts a new attempt: fresh session, next generation, `Preparing`.
    pub(crate) fn begin_connect(&mut self, revision: &str) -> Result<SessionRef, ClientErrorInfo> {
        let generation = self.generation.checked_add(1).ok_or_else(|| {
            ClientErrorInfo::new(ErrorCode::Internal, "CONNECTION_GENERATION_EXHAUSTED")
        })?;
        let session_id = uuid::Uuid::new_v4().to_string();
        *self = Machine {
            phase: ConnectionPhase::Preparing,
            session_id: Some(session_id.clone()),
            generation,
            revision: Some(revision.to_string()),
            reason: None,
            retryable: false,
            desired_on: true,
            competitors: Vec::new(),
        };
        Ok(SessionRef {
            session_id,
            generation,
        })
    }

    /// Applies a transition only if `generation` is still current.
    pub(crate) fn transition(
        &mut self,
        generation: u64,
        phase: ConnectionPhase,
        reason: Option<ClientErrorInfo>,
        retryable: bool,
    ) -> bool {
        if self.generation != generation {
            return false;
        }
        self.phase = phase;
        self.reason = reason;
        self.retryable = retryable;
        if !matches!(phase, ConnectionPhase::Error | ConnectionPhase::Contended) {
            self.competitors.clear();
        }
        true
    }

    /// Ends `generation`: back to `Off`, not desired. The generation counter
    /// is kept so the next attempt is still newer.
    pub(crate) fn finish_disconnect(&mut self, generation: u64) -> bool {
        if self.generation != generation {
            return false;
        }
        *self = Machine {
            generation,
            ..Machine::default()
        };
        true
    }

    pub(crate) fn session(&self) -> Option<SessionRef> {
        let session_id = self.session_id.clone().filter(|id| !id.is_empty())?;
        (self.generation > 0).then_some(SessionRef {
            session_id,
            generation: self.generation,
        })
    }

    pub(crate) fn is(&self, generation: u64, phase: ConnectionPhase) -> bool {
        self.generation == generation && self.phase == phase
    }
}

/// Turns cumulative counters into rates (bytes per second).
#[derive(Debug, Default)]
pub(crate) struct TrafficMeter {
    last: Option<(u64, u64, Instant)>,
}

impl TrafficMeter {
    pub(crate) fn sample(&mut self, up_total: u64, down_total: u64, now: Instant) -> TrafficSample {
        let (up_bps, down_bps) = match self.last {
            // Counters reset when the core restarts: report 0 for that tick.
            Some((up, down, at)) if up_total >= up && down_total >= down && now > at => {
                let seconds = now.duration_since(at).as_secs_f64();
                (
                    ((up_total - up) as f64 / seconds).round() as u64,
                    ((down_total - down) as f64 / seconds).round() as u64,
                )
            }
            _ => (0, 0),
        };
        self.last = Some((up_total, down_total, now));
        TrafficSample {
            up_bps,
            down_bps,
            up_total,
            down_total,
        }
    }
}

/// The last disconnect was never confirmed: the session may still hold a
/// core, and "retry" means disconnecting again.
fn disconnect_unconfirmed(machine: &Machine) -> bool {
    machine.phase == ConnectionPhase::Error
        && machine
            .reason
            .as_ref()
            .is_some_and(|reason| reason.detail.starts_with(DISCONNECT_UNCONFIRMED))
}

/// Maps a connect-path service failure to `(info, retryable)`.
pub(crate) fn connect_failure(error: &ServiceError) -> (ClientErrorInfo, bool) {
    let info = error.info();
    let retryable = !matches!(
        info.code,
        ErrorCode::ServiceIncompatible
            | ErrorCode::ServiceClientRejected
            | ErrorCode::ServiceOwnedByAnotherUser
            | ErrorCode::ProfileInvalid
    );
    (info, retryable)
}

/// The service says the connection is no longer ours: another session of
/// this user took it over (`ServiceBusy`, can be taken back) or another user
/// owns it. Neither is a network-path problem.
pub(crate) fn ownership_lost(error: &ServiceError) -> Option<(ClientErrorInfo, bool)> {
    match error.reason_code()? {
        "STALE_OR_FOREIGN_SESSION" | "CONNECTION_OWNED_BY_ANOTHER_SESSION" => Some((
            ClientErrorInfo::new(ErrorCode::ServiceBusy, error.detail()),
            true,
        )),
        "CONNECTION_OWNED_BY_ANOTHER_USER" => Some((
            ClientErrorInfo::new(ErrorCode::ServiceOwnedByAnotherUser, error.detail()),
            false,
        )),
        _ => None,
    }
}

/// Detail of the health failure for broken TUN routing.
const TUN_ROUTING_BROKEN: &str = "HEALTH_TUN_ROUTING_BROKEN";

/// The TUN's routing is broken (see [`core_ipc::CoreStatus::tun_routing_broken`]).
fn is_tun_routing_broken(info: &ClientErrorInfo) -> bool {
    info.code == ErrorCode::ConnectHealthCheckFailed && info.detail.starts_with(TUN_ROUTING_BROKEN)
}

/// Out of automatic reconnects for TUN routing another program keeps
/// changing: `Contended`, retryable, until the user retries.
fn tun_routing_taken_over() -> Option<ClientErrorInfo> {
    contended(TUN_ROUTING_TAKEN_OVER)
}

fn health_error(detail: impl Into<String>) -> ClientErrorInfo {
    ClientErrorInfo::new(ErrorCode::ConnectFailed, detail)
}

/// The tunnel is up but the backend's health endpoint is not reachable on
/// the direct path, or the request bypassed TUN: typically DNS or the route
/// taken over by other software, not a node or route failure.
fn service_unreachable(detail: impl Into<String>) -> ClientErrorInfo {
    ClientErrorInfo::new(ErrorCode::ConnectHealthCheckFailed, detail)
}

fn contended(detail: &str) -> Option<ClientErrorInfo> {
    Some(ClientErrorInfo::new(
        ErrorCode::NetworkPathContended,
        detail,
    ))
}

/// Logs how long each step of a connect attempt (or health check) took,
/// so a slow connect shows where the time went.
struct Steps {
    what: &'static str,
    generation: u64,
    info: bool,
    started: Instant,
    last: Instant,
}

impl Steps {
    fn new(what: &'static str, generation: u64, info: bool) -> Self {
        let now = Instant::now();
        Self {
            what,
            generation,
            info,
            started: now,
            last: now,
        }
    }

    fn done(&mut self, step: &str) {
        let now = Instant::now();
        let millis = now.duration_since(self.last).as_millis() as u64;
        self.last = now;
        if self.info {
            tracing::info!(
                generation = self.generation,
                "{}: {step} took {millis} ms",
                self.what
            );
        } else {
            tracing::debug!(
                generation = self.generation,
                "{}: {step} took {millis} ms",
                self.what
            );
        }
    }

    fn finish(&self, outcome: &str) {
        tracing::info!(
            generation = self.generation,
            "{}: {outcome} after {} ms",
            self.what,
            self.started.elapsed().as_millis()
        );
    }
}

/// The pre-connect conflict check of one attempt.
struct Preflight {
    task: JoinHandle<ConflictReport>,
    deadline: Instant,
}

impl Preflight {
    /// The report when another app's tunnel owns the network (see
    /// [`crate::detect::tunnel_owns_network`]). `None` when nothing blocks,
    /// and when the check did not finish within its budget or failed: the
    /// connect then proceeds as it did before the check existed.
    async fn conflict(self) -> Option<ConflictReport> {
        let report = match tokio::time::timeout_at(self.deadline, self.task).await {
            Ok(Ok(report)) => report,
            Ok(Err(error)) => {
                tracing::warn!("preflight conflict check failed: {error}");
                return None;
            }
            Err(_) => {
                tracing::info!("preflight conflict check timed out; connecting unchecked");
                return None;
            }
        };
        if !crate::detect::tunnel_owns_network(&report) {
            return None;
        }
        tracing::warn!(
            "preflight: another tunnel owns the network (competitors {:?}, route {:?}, fake-ip dns {:?}); not starting TUN",
            report.competitors,
            report.foreign_default_route,
            report.fake_ip_dns
        );
        Some(report)
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum Desired {
    Disconnected,
    Connected,
}

/// What [`EnhancedConfig::state_file`] holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Persisted {
    generation: u64,
    desired: Desired,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    profile_revision: Option<String>,
}

impl Persisted {
    fn of(machine: &Machine) -> Self {
        Self {
            generation: machine.generation,
            desired: if machine.desired_on {
                Desired::Connected
            } else {
                Desired::Disconnected
            },
            session_id: machine.session_id.clone(),
            profile_revision: machine.revision.clone(),
        }
    }
}

async fn persist(path: PathBuf, state: Persisted) -> Result<(), ClientErrorInfo> {
    let result = tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let dir = path
            .parent()
            .ok_or_else(|| std::io::Error::other("no parent"))?;
        std::fs::create_dir_all(dir)?;
        let bytes = serde_json::to_vec(&state).map_err(std::io::Error::other)?;
        let temporary = dir.join(format!(".state-{}.tmp", uuid::Uuid::new_v4().simple()));
        std::fs::write(&temporary, bytes)?;
        std::fs::rename(&temporary, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&temporary);
        })
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(ClientErrorInfo::new(
            ErrorCode::LocalStorageFailed,
            format!("persist connection state: {error}"),
        )),
        Err(error) => Err(ClientErrorInfo::new(
            ErrorCode::Internal,
            format!("persist task: {error}"),
        )),
    }
}

async fn load_persisted(path: PathBuf) -> Option<Persisted> {
    tokio::task::spawn_blocking(move || {
        serde_json::from_slice::<Persisted>(&std::fs::read(path).ok()?).ok()
    })
    .await
    .ok()
    .flatten()
}

// ---------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------

struct Profile {
    value: Value,
    revision: String,
}

#[derive(Default)]
struct Data {
    machine: Machine,
    profile: Option<Arc<Profile>>,
    service_installed: bool,
    /// An outdated service was already reinstalled (or the attempt declined
    /// or failed) in this session: never again, whatever it reports now.
    stale_service_reinstall_tried: bool,
    /// An outdated service was seen while connected (logged once); it is
    /// replaced before the next connect.
    stale_service_noted: bool,
    recovery: RecoveryEpisode,
    routing_reconnects: RoutingReconnects,
    loops_generation: Option<u64>,
    loops: Vec<JoinHandle<()>>,
}

struct Inner {
    config: EnhancedConfig,
    service: Arc<dyn ServiceApi>,
    hooks: Arc<dyn PlatformHooks>,
    standard: Option<StandardCore>,
    on_state: StateSink,
    on_error: ErrorSink,
    detector: Mutex<Detector>,
    /// Pause between data-plane health checks ([`HEALTH_INTERVAL`]).
    health_interval: Mutex<Duration>,
    /// Replaces the official host's direct and proxied steps (tests).
    #[cfg(test)]
    official_steps: Mutex<Option<OfficialSteps>>,
    /// For [`RoutingReconnects`].
    clock: Mutex<Clock>,
    /// Woken by [`Enhanced::network_changed`]: brings the next health check
    /// forward, and cuts a reconnect pause short.
    network_changed: tokio::sync::Notify,
    /// Serialises enable / disable / retry / update / recovery.
    operation: tokio::sync::Mutex<()>,
    /// An uninstall holds `operation` through the admin prompt and the
    /// service's stop; enhanced mode is already off by then, so calls that
    /// only need it off (disable, a node selection) do not wait for it.
    uninstalling: AtomicBool,
    data: Mutex<Data>,
}

/// Enhanced-mode controller; cheap to clone.
#[derive(Clone)]
pub(crate) struct Enhanced {
    inner: Arc<Inner>,
}

impl Enhanced {
    /// `standard` serves the proxied-path step of the health check.
    pub(crate) fn new(
        config: EnhancedConfig,
        service: Arc<dyn ServiceApi>,
        hooks: Arc<dyn PlatformHooks>,
        standard: Option<StandardCore>,
        on_state: StateSink,
        on_error: ErrorSink,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                service,
                hooks,
                standard,
                on_state,
                on_error,
                detector: Mutex::new(default_detector()),
                health_interval: Mutex::new(HEALTH_INTERVAL),
                #[cfg(test)]
                official_steps: Mutex::new(None),
                clock: Mutex::new(system_clock()),
                network_changed: tokio::sync::Notify::new(),
                operation: tokio::sync::Mutex::new(()),
                uninstalling: AtomicBool::new(false),
                data: Mutex::new(Data::default()),
            }),
        }
    }

    /// Replaces the pre-connect conflict detector (tests).
    #[cfg(test)]
    pub(crate) fn set_detector(&self, detector: Detector) {
        *self
            .inner
            .detector
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = detector;
    }

    /// Checks on the system whether the privileged service is installed
    /// (an admin may have installed it outside the app) and updates the
    /// snapshot when that changed.
    pub(crate) async fn refresh_service_installed(&self) -> bool {
        self.inner.refresh_installed().await
    }

    /// The OS reported a network change (interface, address, default route):
    /// while on, the next health check runs after [`NETWORK_SETTLE`] instead
    /// of at the end of the interval; while reconnecting after a service
    /// failure, the pause ends now. Otherwise nothing (no check is pending).
    pub(crate) fn network_changed(&self) {
        let phase = self.inner.with_data(|data| data.machine.phase);
        if matches!(phase, ConnectionPhase::On | ConnectionPhase::Reconnecting) {
            tracing::info!(?phase, "network changed");
            self.inner.network_changed.notify_waiters();
        }
    }

    /// Replaces the pause between health checks (tests).
    #[cfg(test)]
    pub(crate) fn set_health_interval(&self, interval: Duration) {
        *self
            .inner
            .health_interval
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = interval;
    }

    /// Scripts the official host's steps of the health check (tests): each
    /// check calls `steps` for the direct and the proxied results.
    #[cfg(test)]
    pub(crate) fn set_official_steps(&self, steps: OfficialSteps) {
        *self
            .inner
            .official_steps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(steps);
    }

    /// Replaces the clock of the TUN-routing reconnect budget (tests).
    #[cfg(test)]
    pub(crate) fn set_clock(&self, clock: Clock) {
        *self
            .inner
            .clock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = clock;
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> EnhancedState {
        self.inner
            .data
            .lock()
            .map(|data| state_of(&data))
            .unwrap_or_default()
    }

    /// Restores the persisted generation and, if the app was killed while
    /// connected, re-attaches to the service session (or reports it lost /
    /// owned by someone else). Call once at launch.
    pub(crate) async fn restore(&self) {
        let inner = &self.inner;
        let _operation = inner.operation.lock().await;
        inner.refresh_installed().await;
        let Some(persisted) = load_persisted(inner.config.state_file.clone()).await else {
            inner.emit();
            return;
        };
        let generation = persisted.generation;
        inner.with_data(|data| {
            data.machine = Machine {
                phase: if persisted.desired == Desired::Connected {
                    ConnectionPhase::Preparing
                } else {
                    ConnectionPhase::Off
                },
                session_id: persisted.session_id.clone(),
                generation,
                revision: persisted.profile_revision.clone(),
                reason: None,
                retryable: false,
                desired_on: persisted.desired == Desired::Connected,
                competitors: Vec::new(),
            };
        });
        if persisted.desired != Desired::Connected {
            inner.emit();
            return;
        }
        let ours = inner.with_data(|data| data.machine.session());
        match inner.service.get_status().await {
            Ok(status)
                if status.running
                    && ours.as_ref().is_some_and(|session| {
                        status.session_id.as_deref() == Some(session.session_id.as_str())
                            && status.generation == session.generation
                    }) =>
            {
                inner.transition(generation, ConnectionPhase::On, None, false);
                inner.start_loops(generation);
            }
            // The service shows session details only to the owner's OS user:
            // none means another user holds the connection.
            Ok(status) if status.running && status.session_id.is_none() => {
                inner.transition(
                    generation,
                    ConnectionPhase::Contended,
                    Some(ClientErrorInfo::new(
                        ErrorCode::ServiceOwnedByAnotherUser,
                        "CONNECTION_OWNED_BY_ANOTHER_USER",
                    )),
                    false,
                );
            }
            Ok(status) if status.running => {
                inner.transition(
                    generation,
                    ConnectionPhase::Contended,
                    Some(ClientErrorInfo::new(
                        ErrorCode::ServiceBusy,
                        "CONNECTION_OWNED_BY_ANOTHER_SESSION",
                    )),
                    true,
                );
            }
            // The service runs but our session is gone (the machine or the
            // service restarted): enhanced mode is not resumed on launch, so
            // this is plain Off, not a failure ("the system service isn't
            // running" was shown on every launch after a reboot).
            Ok(_) => {
                tracing::info!(generation, "enhanced mode: the previous session ended; off");
                let persisted = inner.with_data(|data| {
                    data.machine.desired_on = false;
                    data.machine.session_id = None;
                    Persisted::of(&data.machine)
                });
                inner.transition(generation, ConnectionPhase::Off, None, false);
                inner.persist(persisted).await;
            }
            Err(error) => {
                inner.transition(generation, ConnectionPhase::Error, Some(error.info()), true);
            }
        }
    }

    /// Stores the profile used by the next connect. While on, a new revision
    /// is applied live through the service (rolled back by the service if the
    /// core rejects it).
    pub(crate) async fn set_profile(
        &self,
        profile: &[u8],
        revision: &str,
    ) -> Result<(), ClientError> {
        let value: Value = serde_json::from_slice(profile).map_err(|error| {
            ClientError::failed(
                ErrorCode::ProfileInvalid,
                format!("PROFILE_JSON_INVALID: {error}"),
            )
        })?;
        if revision.is_empty() {
            return Err(ClientError::failed(
                ErrorCode::ProfileInvalid,
                "PROFILE_REVISION_REQUIRED",
            ));
        }
        let profile = Arc::new(Profile {
            value,
            revision: revision.to_string(),
        });
        let inner = &self.inner;
        let _operation = inner.operation.lock().await;
        let machine = inner.with_data(|data| {
            data.profile = Some(profile.clone());
            data.machine.clone()
        });
        if machine.phase != ConnectionPhase::On || machine.revision.as_deref() == Some(revision) {
            return Ok(());
        }
        Self::push_profile(inner, &machine, &profile).await
    }

    /// Applies the current profile again, in the routing mode now set, to a
    /// connected core (no reconnect). A no-op unless `On`.
    pub(crate) async fn reapply(&self) -> Result<(), ClientError> {
        let inner = &self.inner;
        let _operation = inner.operation.lock().await;
        let (machine, profile) =
            inner.with_data(|data| (data.machine.clone(), data.profile.clone()));
        let Some(profile) = profile else {
            return Ok(());
        };
        if machine.phase != ConnectionPhase::On {
            return Ok(());
        }
        Self::push_profile(inner, &machine, &profile).await
    }

    /// UpdateProfile for the connected `machine`; caller holds `operation`.
    async fn push_profile(
        inner: &Arc<Inner>,
        machine: &Machine,
        profile: &Arc<Profile>,
    ) -> Result<(), ClientError> {
        let Some(session) = machine.session() else {
            return Ok(());
        };
        let generation = machine.generation;
        inner.with_data(|data| data.recovery.reset());
        // Stays On: the core (0.5.18+) applies a profile without dropping
        // open connections. Going through Reconnecting here also stopped the
        // health loop for good when it woke in that window.
        let routing_mode: RoutingMode = inner.config.routing.get();
        match inner
            .service
            .update_profile(&session, &profile.value, routing_mode, &inner.choices())
            .await
        {
            Ok(_) => {
                let persisted = inner.with_data(|data| {
                    (data.machine.generation == generation).then(|| {
                        data.machine.revision = Some(profile.revision.clone());
                        Persisted::of(&data.machine)
                    })
                });
                if let Some(persisted) = persisted {
                    inner.persist(persisted).await;
                }
                inner.transition(generation, ConnectionPhase::On, None, false);
                Ok(())
            }
            Err(error) => {
                let detail = error.detail();
                if detail.contains("PROFILE_UPDATE_ROLLED_BACK") {
                    let info = ClientErrorInfo::new(ErrorCode::ProfileInvalid, detail);
                    inner.transition(generation, ConnectionPhase::On, Some(info.clone()), true);
                    Err(info.into())
                } else {
                    let (mut info, _) = connect_failure(&error);
                    info.detail = format!("PROFILE_UPDATE_FAILED: {}", info.detail);
                    inner.transition(generation, ConnectionPhase::Error, Some(info.clone()), true);
                    Err(info.into())
                }
            }
        }
    }

    /// Turns enhanced mode on: installs the service through the platform hook
    /// when it is missing (phase `WaitingPermission`), waits until it answers,
    /// then connects and verifies the data plane.
    pub(crate) async fn enable(&self) -> Result<(), ClientError> {
        let inner = &self.inner;
        let _operation = inner.operation.lock().await;
        let machine = inner.with_data(|data| {
            data.recovery.reset();
            data.routing_reconnects.reset();
            data.machine.clone()
        });
        if machine.phase == ConnectionPhase::On {
            return Ok(());
        }
        inner.release_session(&machine).await;
        inner.clone().connect(true, false, false).await
    }

    /// Installs the privileged service without turning enhanced mode on, then
    /// waits until it answers. Already installed and answering: no-op. A
    /// cancelled admin prompt returns [`ClientError::Cancelled`] and leaves
    /// the phase untouched.
    pub(crate) async fn install_service(&self) -> Result<(), ClientError> {
        let inner = &self.inner;
        let _operation = inner.operation.lock().await;
        if inner.refresh_installed().await {
            if let Ok(Ok(_)) =
                tokio::time::timeout(Duration::from_secs(5), inner.service.get_version()).await
            {
                return Ok(());
            }
        }
        match inner.install().await {
            Ok(()) => {}
            Err(info) if info.code == ErrorCode::ServiceInstallCancelled => {
                tracing::info!("service install: cancelled at the admin prompt");
                return Err(ClientError::Cancelled);
            }
            Err(info) => return Err(info.into()),
        }
        inner.wait_for_service().await.map_err(ClientError::from)?;
        tracing::info!("service install: installed and answering");
        Ok(())
    }

    /// Takes the connection over from another session of this OS user
    /// ("Use on this device"); only while [`EnhancedState::can_take_over`].
    pub(crate) async fn take_over(&self) -> Result<(), ClientError> {
        let inner = &self.inner;
        let _operation = inner.operation.lock().await;
        let allowed = inner.with_data(|data| {
            data.recovery.reset();
            data.routing_reconnects.reset();
            can_take_over(&data.machine)
        });
        if !allowed {
            return Err(ClientError::failed(
                ErrorCode::ServiceBusy,
                "TAKE_OVER_NOT_AVAILABLE",
            ));
        }
        tracing::info!("enhanced mode: taking over from another session");
        inner.clone().connect(true, true, false).await
    }

    /// Reconnects from `Error` / `Contended` with a fresh session. Every
    /// service call already uses its own connection; the cached handshake is
    /// dropped too, since the failure may have been a service restart (e.g.
    /// a package upgrade) that invalidated it. After an unconfirmed
    /// disconnect, retries the disconnect instead.
    pub(crate) async fn retry(&self) -> Result<(), ClientError> {
        let inner = &self.inner;
        let _operation = inner.operation.lock().await;
        let machine = inner.with_data(|data| {
            data.recovery.reset();
            data.routing_reconnects.reset();
            data.machine.clone()
        });
        inner.service.reset_auth();
        if disconnect_unconfirmed(&machine) {
            return inner
                .disconnect_current(true)
                .await
                .map_err(ClientError::from);
        }
        inner.release_session(&machine).await;
        inner.clone().connect(true, false, false).await
    }

    /// Turns enhanced mode off: `Off` once the service confirmed that the
    /// session's core stopped (or the service is not running, which takes
    /// the core with it). Otherwise `Error` (`DISCONNECT_UNCONFIRMED`,
    /// retryable) with the session kept, so a retry disconnects again.
    pub(crate) async fn disable(&self) -> Result<(), ClientError> {
        let inner = &self.inner;
        if inner.off_while_uninstalling() {
            return Ok(());
        }
        let _operation = inner.operation.lock().await;
        inner
            .disconnect_current(true)
            .await
            .map_err(ClientError::from)
    }

    /// Selects the exit node live while on. Every connect and profile
    /// update carries the selection anyway ([`EnhancedConfig::selection`]).
    pub(crate) async fn select_node(&self, node_id: &str) -> Result<(), ClientError> {
        let inner = &self.inner;
        if inner.off_while_uninstalling() {
            return Ok(());
        }
        let _operation = inner.operation.lock().await;
        let machine = inner.with_data(|data| data.machine.clone());
        if machine.phase != ConnectionPhase::On {
            return Ok(());
        }
        let Some(session) = machine.session() else {
            return Ok(());
        };
        let core = ServiceCoreTransport {
            service: inner.service.clone(),
            session,
        };
        core_ipc::select_node(&core, node_id)
            .await
            .map_err(|error| error.into_client_error(ErrorCode::ConnectFailed))
    }

    /// Tells the enhanced-mode core, while it is on, that the system goes to
    /// sleep (`asleep`) or woke. Best effort and without the operation lock:
    /// the system does not wait. A service before 0.6.3 does not know it.
    pub(crate) async fn tell_sleep(&self, asleep: bool) {
        let machine = self.inner.with_data(|data| data.machine.clone());
        if machine.phase != ConnectionPhase::On {
            return;
        }
        let Some(session) = machine.session() else {
            return;
        };
        let core = ServiceCoreTransport {
            service: self.inner.service.clone(),
            session,
        };
        if let Err(error) = core_ipc::tell_sleep(&core, asleep).await {
            tracing::info!(asleep, "enhanced core: sleep not told ({})", error.detail());
        }
    }

    /// Pins (or unpins) one node on the enhanced-mode core while it is on.
    pub(crate) async fn pin_ingress_live(&self, node_id: &str, endpoint_key: Option<&str>) {
        let machine = self.inner.with_data(|data| data.machine.clone());
        if machine.phase != ConnectionPhase::On {
            return;
        }
        let Some(session) = machine.session() else {
            return;
        };
        let core = ServiceCoreTransport {
            service: self.inner.service.clone(),
            session,
        };
        crate::ingress::pin_one(&core, node_id, endpoint_key, "enhanced core").await;
    }

    /// The enhanced-mode core's status while it is on.
    pub(crate) async fn core_status(&self) -> Option<core_ipc::CoreStatus> {
        let machine = self.inner.with_data(|data| data.machine.clone());
        if machine.phase != ConnectionPhase::On {
            return None;
        }
        let core = ServiceCoreTransport {
            service: self.inner.service.clone(),
            session: machine.session()?,
        };
        core_ipc::get_status(&core).await.ok()
    }

    /// Cumulative `(upload, download)` bytes of the enhanced-mode core while
    /// it is on; `None` otherwise or when the service does not answer.
    pub(crate) async fn traffic_totals(&self) -> Option<(u64, u64)> {
        let machine = self.inner.with_data(|data| data.machine.clone());
        if machine.phase != ConnectionPhase::On {
            return None;
        }
        let core = ServiceCoreTransport {
            service: self.inner.service.clone(),
            session: machine.session()?,
        };
        core_ipc::get_traffic(&core)
            .await
            .ok()
            .map(|traffic| traffic.totals())
    }

    /// Releases the service session and stops background loops; call before
    /// the app quits. Enhanced mode is not resumed on the next launch.
    pub(crate) async fn shutdown(&self) {
        let inner = &self.inner;
        let _operation = inner.operation.lock().await;
        // Quitting: best effort, the lease stops the core otherwise.
        let _ = inner.disconnect_current(false).await;
        inner.stop_loops();
    }

    /// Turns enhanced mode off, then uninstalls the service through the
    /// platform hook.
    ///
    /// The service is told to stop our core (bounded by [`RELEASE_BUDGET`];
    /// the uninstall stops the service, which stops the core in order
    /// anyway), background loops are cancelled, and no confirmation is
    /// awaited: a service that no longer listens has stopped. While the hook
    /// runs (admin prompt, service stop), enhanced mode is off and
    /// [`Enhanced::disable`] does not wait for it, so switching to
    /// compatible mode and connecting proceed at once.
    pub(crate) async fn uninstall_service(&self) -> Result<(), ClientError> {
        let inner = &self.inner;
        let _operation = inner.operation.lock().await;
        struct Uninstalling<'a>(&'a AtomicBool);
        impl Drop for Uninstalling<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        // Set before going off: whoever sees `Off` from here on must not
        // queue behind the (slow, prompting) platform uninstall.
        inner.uninstalling.store(true, Ordering::SeqCst);
        let _uninstalling = Uninstalling(&inner.uninstalling);
        let started = Instant::now();
        let _ = inner.disconnect_current(false).await;
        tracing::info!(
            "service uninstall: enhanced mode off in {} ms",
            started.elapsed().as_millis()
        );
        let hooks = inner.hooks.clone();
        let result = tokio::task::spawn_blocking(move || hooks.uninstall_privileged_service())
            .await
            .map_err(|error| {
                ClientError::failed(ErrorCode::Internal, format!("uninstall task: {error}"))
            })?;
        tracing::info!(
            "service uninstall: platform hook returned after {} ms ({})",
            started.elapsed().as_millis(),
            if result.is_ok() { "ok" } else { "failed" }
        );
        inner.service.reset_auth();
        inner.refresh_installed().await;
        match result {
            Ok(()) => Ok(()),
            Err(PlatformError::Cancelled) => Err(ClientError::failed(
                ErrorCode::ServiceUninstallCancelled,
                "SERVICE_UNINSTALL_CANCELLED",
            )),
            Err(PlatformError::Failed { message } | PlatformError::Locked { message }) => Err(
                ClientError::failed(ErrorCode::ServiceInstallFailed, message),
            ),
        }
    }
}

fn state_of(data: &Data) -> EnhancedState {
    EnhancedState {
        phase: data.machine.phase,
        reason: data.machine.reason.clone(),
        retryable: data.machine.retryable,
        service_installed: data.service_installed,
        can_take_over: can_take_over(&data.machine),
        competitors: data.machine.competitors.clone(),
    }
}

/// "Use on this device" applies to a connection owned by another session of
/// this OS user, when the service can hand it over.
fn can_take_over(machine: &Machine) -> bool {
    machine.phase == ConnectionPhase::Contended
        && machine
            .reason
            .as_ref()
            .is_some_and(|reason| reason.code == ErrorCode::ServiceBusy)
}

impl Inner {
    /// Writes the connection state; a failure is reported, never fatal.
    async fn persist(&self, persisted: Persisted) {
        if let Err(info) = persist(self.config.state_file.clone(), persisted).await {
            tracing::warn!(?info, "persist enhanced-mode state failed");
            (self.on_error)(info);
        }
    }

    /// The selection and the pins every Connect / UpdateProfile carries.
    fn choices(&self) -> core_ipc::ApplyChoices {
        core_ipc::ApplyChoices::new(self.config.selection.get(), &self.config.ingress_pins.get())
    }

    /// Takes one automatic reconnect for broken TUN routing from the budget
    /// ([`RoutingReconnects`]); `false` when it is used up.
    fn claim_routing_reconnect(&self) -> bool {
        let clock = self
            .clock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let now = clock();
        self.with_data(|data| data.routing_reconnects.claim(now.into_std()))
    }

    fn with_data<T>(&self, change: impl FnOnce(&mut Data) -> T) -> T {
        let mut guard = match self.data.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        change(&mut guard)
    }

    fn emit(&self) {
        let state = self.with_data(|data| state_of(data));
        (self.on_state)(state);
    }

    fn transition(
        &self,
        generation: u64,
        phase: ConnectionPhase,
        reason: Option<ClientErrorInfo>,
        retryable: bool,
    ) -> bool {
        let applied = self.with_data(|data| {
            data.machine
                .transition(generation, phase, reason, retryable)
        });
        if applied {
            self.emit();
        }
        applied
    }

    /// Out of automatic reconnects for TUN routing another program keeps
    /// changing: `Contended` (`TUN_ROUTING_TAKEN_OVER`) until the user
    /// retries. The session is already released.
    fn routing_taken_over(&self, generation: u64) {
        tracing::warn!(
            generation,
            "enhanced mode: another program keeps changing the TUN routing \
             ({TUN_ROUTING_RECONNECTS} reconnects in {} min); not reconnecting",
            TUN_ROUTING_WINDOW.as_secs() / 60
        );
        self.transition(
            generation,
            ConnectionPhase::Contended,
            tun_routing_taken_over(),
            true,
        );
    }

    /// An uninstall is in progress and enhanced mode is off.
    fn off_while_uninstalling(&self) -> bool {
        self.uninstalling.load(Ordering::SeqCst)
            && self.with_data(|data| data.machine.phase == ConnectionPhase::Off)
    }

    fn is_current(&self, generation: u64, phase: ConnectionPhase) -> bool {
        self.with_data(|data| data.machine.is(generation, phase))
    }

    async fn refresh_installed(&self) -> bool {
        let hooks = self.hooks.clone();
        let installed = tokio::task::spawn_blocking(move || hooks.privileged_service_installed())
            .await
            .unwrap_or(false);
        let changed = self.with_data(|data| {
            let changed = data.service_installed != installed;
            data.service_installed = installed;
            changed
        });
        if changed {
            self.emit();
        }
        installed
    }

    async fn finish(&self, generation: u64) {
        let persisted = self.with_data(|data| {
            data.machine
                .finish_disconnect(generation)
                .then(|| Persisted::of(&data.machine))
        });
        if let Some(persisted) = persisted {
            self.persist(persisted).await;
            self.emit();
        }
    }

    /// Best-effort release of the service session held by `machine`.
    async fn release_session(&self, machine: &Machine) {
        if machine.phase == ConnectionPhase::Off {
            return;
        }
        if let Some(session) = machine.session() {
            match tokio::time::timeout(RELEASE_BUDGET, self.service.disconnect(&session)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::debug!(?error, "release enhanced session"),
                Err(_) => tracing::info!(
                    "release enhanced session: no answer within {} ms; going on",
                    RELEASE_BUDGET.as_millis()
                ),
            }
        }
    }

    /// Ends the current session. `confirm`: wait until the service confirms
    /// that its core stopped, and end in `Error` instead of `Off` when it
    /// does not; otherwise best effort (quit, uninstall).
    async fn disconnect_current(&self, confirm: bool) -> Result<(), ClientErrorInfo> {
        // A health check or lease renewal still in flight must not report
        // (or recover) a session that is going away.
        self.stop_loops();
        let machine = self.with_data(|data| data.machine.clone());
        if machine.phase != ConnectionPhase::Off {
            self.transition(
                machine.generation,
                ConnectionPhase::Disconnecting,
                None,
                false,
            );
            match machine.session() {
                Some(session) if confirm => {
                    if let Err(info) = self.stop_session(&session).await {
                        tracing::warn!(detail = %info.detail, "enhanced mode: disconnect not confirmed");
                        let persisted = self.with_data(|data| {
                            data.machine.desired_on = false;
                            data.machine
                                .transition(
                                    machine.generation,
                                    ConnectionPhase::Error,
                                    Some(info.clone()),
                                    true,
                                )
                                .then(|| Persisted::of(&data.machine))
                        });
                        if let Some(persisted) = persisted {
                            self.persist(persisted).await;
                        }
                        self.emit();
                        return Err(info);
                    }
                }
                _ => self.release_session(&machine).await,
            }
        }
        self.finish(machine.generation).await;
        Ok(())
    }

    /// Disconnects `session` and makes sure its core is gone. A definite
    /// answer from the service (done, or the session is not active) settles
    /// it; after a timeout or a broken call, the service's status is polled
    /// until it no longer runs a core for `session` or stops listening.
    async fn stop_session(&self, session: &SessionRef) -> Result<(), ClientErrorInfo> {
        let error = match self.service.disconnect(session).await {
            Ok(()) | Err(ServiceError::Failed(_)) => return Ok(()),
            Err(error) if error.not_listening() => return Ok(()),
            // The service refuses this executable before reading any request
            // (#543): it never ran a core for it, and one left by an earlier
            // build stops when its lease lapses, since nothing here can
            // renew it. Asking again only gets refused again.
            Err(error @ ServiceError::Rejected(_)) => {
                tracing::info!(
                    detail = %error.detail(),
                    "enhanced mode: the service refuses this client; nothing to stop"
                );
                return Ok(());
            }
            Err(error) => error,
        };
        tracing::info!(
            ?error,
            "disconnect unanswered; waiting for the service to confirm"
        );
        let deadline = Instant::now() + DISCONNECT_CONFIRM_TIMEOUT;
        loop {
            match tokio::time::timeout(Duration::from_secs(5), self.service.get_status()).await {
                Ok(Ok(status))
                    if !(status.running
                        && status.session_id.as_deref() == Some(&session.session_id)) =>
                {
                    return Ok(());
                }
                Ok(Err(error)) if error.not_listening() => return Ok(()),
                Ok(Err(ServiceError::Rejected(_))) => return Ok(()),
                _ => {}
            }
            if Instant::now() + DISCONNECT_CONFIRM_POLL >= deadline {
                return Err(ClientErrorInfo::new(
                    ErrorCode::ServiceUnavailable,
                    format!("{DISCONNECT_UNCONFIRMED}: {}", error.detail()),
                ));
            }
            tokio::time::sleep(DISCONNECT_CONFIRM_POLL).await;
        }
    }

    /// Makes sure a compatible service answers, installing it through the
    /// platform hook when allowed.
    async fn ensure_service(
        &self,
        generation: u64,
        allow_install: bool,
    ) -> Result<(), ClientErrorInfo> {
        let installed = self.refresh_installed().await;
        if installed {
            let answer =
                tokio::time::timeout(Duration::from_secs(5), self.service.get_version()).await;
            match answer {
                Ok(Ok(version)) => {
                    // Only here, before a connect: never while connected
                    // (recovery passes `allow_install = false`).
                    if allow_install && self.service_is_stale(version.build_id.as_deref()) {
                        return self.replace_stale_service(generation, &version).await;
                    }
                    return Ok(());
                }
                // The service refused this executable: one reinstall, where
                // the installed service may predate this client's signature.
                Ok(Err(error @ ServiceError::Rejected(_)))
                    if allow_install && self.config.reinstall_rejecting_service =>
                {
                    return self.replace_rejecting_service(generation, error).await;
                }
                // Otherwise reinstalling cannot help.
                Ok(Err(error @ (ServiceError::Rejected(_) | ServiceError::Protocol(_)))) => {
                    return Err(error.info());
                }
                Ok(Err(error)) if !allow_install => return Err(error.info()),
                Err(_) if !allow_install => {
                    return Err(ClientErrorInfo::new(
                        ErrorCode::ServiceUnavailable,
                        "SERVICE_VERSION_TIMEOUT",
                    ))
                }
                // Installed but not answering: repair by reinstalling.
                Ok(Err(error)) => tracing::warn!(
                    ?error,
                    "privileged service installed but its version query failed; reinstalling"
                ),
                Err(_) => tracing::warn!(
                    "privileged service installed but did not answer its version query within 5 s; reinstalling"
                ),
            }
        } else if !allow_install {
            return Err(ClientErrorInfo::new(
                ErrorCode::ServiceUnavailable,
                "SERVICE_NOT_INSTALLED",
            ));
        }

        if !installed {
            tracing::info!("privileged service not installed; installing");
        }
        self.transition(generation, ConnectionPhase::WaitingPermission, None, false);
        self.install().await?;
        self.transition(generation, ConnectionPhase::Preparing, None, false);
        self.wait_for_service().await
    }

    /// The service reports a build id other than the one this client ships
    /// with; a missing id (an older service) counts as different.
    fn service_is_stale(&self, reported: Option<&str>) -> bool {
        self.config
            .expected_service_build_id
            .as_deref()
            .is_some_and(|expected| reported != Some(expected))
    }

    /// Reinstalls an answering but outdated service through the platform
    /// hook (same flow and prompt as a first install), at most once per
    /// session. The old service still works, so a declined or failed
    /// reinstall, or one after which the service still reports another
    /// build, only logs a warning and the connect goes on with it.
    async fn replace_stale_service(
        &self,
        generation: u64,
        version: &crate::service::VersionInfo,
    ) -> Result<(), ClientErrorInfo> {
        let expected = self
            .config
            .expected_service_build_id
            .clone()
            .unwrap_or_default();
        let reported = version.build_id.as_deref().unwrap_or("none");
        if self.with_data(|data| data.stale_service_reinstall_tried) {
            tracing::warn!(service = %version.version, reported, expected,
                "privileged service differs from this build; already reinstalled once, using it as is");
            return Ok(());
        }
        // A core running for another session or user would be torn down by
        // the reinstall: leave it for a later connect.
        if let Ok(Ok(status)) =
            tokio::time::timeout(Duration::from_secs(5), self.service.get_status()).await
        {
            if status.running {
                tracing::info!(
                    "privileged service differs from this build but is in use; reinstall deferred"
                );
                return Ok(());
            }
        }
        self.with_data(|data| data.stale_service_reinstall_tried = true);
        tracing::info!(service = %version.version, reported, expected,
            "privileged service differs from this build; reinstalling");

        self.transition(generation, ConnectionPhase::WaitingPermission, None, false);
        let installed = self.install().await;
        self.transition(generation, ConnectionPhase::Preparing, None, false);
        let answering = self.wait_for_service().await;
        match (installed, answering) {
            (Ok(()), Err(info)) | (Err(info), Err(_)) => return Err(info),
            (Err(info), Ok(())) => {
                tracing::warn!(code = ?info.code, detail = %info.detail,
                    "reinstalling the outdated service did not complete; using it as is");
                return Ok(());
            }
            (Ok(()), Ok(())) => {}
        }
        match tokio::time::timeout(Duration::from_secs(5), self.service.get_version()).await {
            Ok(Ok(now)) if self.service_is_stale(now.build_id.as_deref()) => {
                tracing::warn!(service = %now.version, reported = now.build_id.as_deref().unwrap_or("none"),
                    expected, "privileged service still differs after reinstall; not retrying this session");
            }
            _ => {
                self.with_data(|data| data.stale_service_noted = false);
                tracing::info!("privileged service reinstalled");
            }
        }
        Ok(())
    }

    /// Reinstalls an installed service that refused this client, once per
    /// connect (each one a user's): a service installed before this build's
    /// signature, such as one pinning the self-signed certificate of the
    /// releases before the Developer ID (#471). The new installer checks
    /// the service it installs against this build's signature, so this
    /// widens nothing. A failed or declined reinstall fails the connect with
    /// its error; a service that still refuses, with the refusal.
    async fn replace_rejecting_service(
        &self,
        generation: u64,
        rejected: ServiceError,
    ) -> Result<(), ClientErrorInfo> {
        tracing::warn!(detail = %rejected.detail(),
            "privileged service refused this client; reinstalling it once");
        self.transition(generation, ConnectionPhase::WaitingPermission, None, false);
        let installed = self.install().await;
        self.transition(generation, ConnectionPhase::Preparing, None, false);
        installed?;
        self.wait_for_service().await?;
        tracing::info!("privileged service reinstalled; it accepts this client");
        Ok(())
    }

    /// Notes (once) that the service answering a connected session is
    /// outdated; it is replaced before the next connect, never mid-session.
    fn note_service_build(&self, status: &crate::service::ServiceStatus) {
        if !self.service_is_stale(status.service_build_id.as_deref()) {
            return;
        }
        let first = self.with_data(|data| !std::mem::replace(&mut data.stale_service_noted, true));
        if first {
            tracing::info!(
                reported = status.service_build_id.as_deref().unwrap_or("none"),
                "privileged service differs from this build; it is reinstalled before the next connect"
            );
        }
    }

    /// Runs the platform install hook (admin prompt) and refreshes the
    /// installed flag; does not wait for the service to answer.
    async fn install(&self) -> Result<(), ClientErrorInfo> {
        let hooks = self.hooks.clone();
        let installed = tokio::task::spawn_blocking(move || hooks.install_privileged_service())
            .await
            .map_err(|error| {
                ClientErrorInfo::new(ErrorCode::Internal, format!("install task: {error}"))
            })?;
        match installed {
            Ok(()) => {}
            Err(PlatformError::Cancelled) => {
                return Err(ClientErrorInfo::new(
                    ErrorCode::ServiceInstallCancelled,
                    "SERVICE_INSTALL_CANCELLED",
                ))
            }
            Err(PlatformError::Failed { message } | PlatformError::Locked { message }) => {
                return Err(ClientErrorInfo::new(
                    ErrorCode::ServiceInstallFailed,
                    message,
                ))
            }
        }
        self.service.reset_auth();
        self.refresh_installed().await;
        Ok(())
    }

    async fn wait_for_service(&self) -> Result<(), ClientErrorInfo> {
        let deadline = Instant::now() + SERVICE_START_TIMEOUT;
        let mut last;
        loop {
            match tokio::time::timeout(Duration::from_secs(2), self.service.get_version()).await {
                Ok(Ok(_)) => return Ok(()),
                // Answering, but not for this client: waiting cannot help.
                Ok(Err(error @ (ServiceError::Rejected(_) | ServiceError::Protocol(_)))) => {
                    return Err(error.info())
                }
                Ok(Err(error)) => last = error.detail(),
                Err(_) => last = "get_version timed out".to_string(),
            }
            if Instant::now() + SERVICE_POLL_INTERVAL >= deadline {
                return Err(ClientErrorInfo::new(
                    ErrorCode::ServiceUnavailable,
                    format!("SERVICE_START_TIMEOUT: {last}"),
                ));
            }
            tokio::time::sleep(SERVICE_POLL_INTERVAL).await;
        }
    }

    /// One connection attempt with a fresh session. Caller holds `operation`.
    /// Boxed with an explicit `Send` type: recovery calls this from a task it
    /// spawned, which would otherwise make the future type recursive.
    ///
    /// `retrying`: an automatic reconnect with attempts left; a failure
    /// because the service is down then leaves the phase `Reconnecting`
    /// (the caller retries after a pause) instead of `Error`.
    fn connect(
        self: Arc<Self>,
        allow_install: bool,
        take_over: bool,
        retrying: bool,
    ) -> BoxFuture<'static, Result<(), ClientError>> {
        Box::pin(async move {
            let Some(profile) = self.with_data(|data| data.profile.clone()) else {
                return Err(ClientError::failed(ErrorCode::Internal, "PROFILE_REQUIRED"));
            };
            // The previous generation's loops end here; recovery runs in its
            // own task, so this never cancels the caller.
            self.stop_loops();
            let begun = self.with_data(|data| {
                data.machine
                    .begin_connect(&profile.revision)
                    .map(|session| (session, Persisted::of(&data.machine)))
            });
            let (session, persisted) = begun?;
            let generation = session.generation;
            let mut steps = Steps::new("enhanced connect", generation, true);
            self.persist(persisted).await;
            self.emit();
            // First: with another tunnel owning the network there is no point
            // in installing or starting the service (an admin prompt, then a
            // failure that hides the real cause).
            let preflight = self.start_preflight();
            let conflict = preflight.conflict().await;
            steps.done("conflict preflight");
            if let Some(report) = conflict {
                // Another tunnel owns the route or DNS: starting ours on top
                // of it (and cleaning up after a failure) would break the
                // user's network. Nothing was asked of the service.
                let info = ClientErrorInfo::new(
                    ErrorCode::NetworkPathContended,
                    format!("PREFLIGHT_CONFLICT: {}", report.competitors.join(", ")),
                );
                let applied = self.with_data(|data| {
                    let applied = data.machine.transition(
                        generation,
                        ConnectionPhase::Error,
                        Some(info.clone()),
                        true,
                    );
                    if applied {
                        data.machine.competitors = report.competitors.clone();
                    }
                    applied
                });
                if applied {
                    self.emit();
                }
                return Err(info.into());
            }

            let ensured = self.ensure_service(generation, allow_install).await;
            steps.done("service check (install / version)");
            if let Err(info) = ensured {
                // A denied admin prompt is an Error the user can retry, like
                // any other failure; it is not remembered as "wanted on".
                let phase = if retrying && is_service_down(&info) {
                    ConnectionPhase::Reconnecting
                } else {
                    ConnectionPhase::Error
                };
                self.transition(generation, phase, Some(info.clone()), true);
                if info.code == ErrorCode::ServiceInstallCancelled {
                    let persisted = self.with_data(|data| {
                        (data.machine.generation == generation).then(|| {
                            data.machine.desired_on = false;
                            Persisted::of(&data.machine)
                        })
                    });
                    if let Some(persisted) = persisted {
                        self.persist(persisted).await;
                    }
                }
                return Err(info.into());
            }

            self.transition(generation, ConnectionPhase::Connecting, None, false);
            let started = steps.started;
            let failed = |info: ClientErrorInfo, retryable: bool| {
                let inner = self.clone();
                let session = session.clone();
                async move {
                    tracing::info!(
                        generation,
                        "enhanced connect: failed after {} ms ({:?})",
                        started.elapsed().as_millis(),
                        info.code
                    );
                    let _ = inner.service.disconnect(&session).await;
                    // Owned by someone else: "occupied", not a failure.
                    let phase = match info.code {
                        ErrorCode::ServiceBusy | ErrorCode::ServiceOwnedByAnotherUser => {
                            ConnectionPhase::Contended
                        }
                        // More attempts follow: stay "reconnecting" rather
                        // than flash "failed" between them.
                        _ if retrying && (is_service_down(&info) || is_path_cause(&info)) => {
                            ConnectionPhase::Reconnecting
                        }
                        _ => ConnectionPhase::Error,
                    };
                    inner.transition(generation, phase, Some(info.clone()), retryable);
                    Err::<(), ClientError>(info.into())
                }
            };

            // The selection and the pins go with the profile: the new core
            // needs no SelectNode / PinIngress afterwards.
            let connected = self
                .service
                .connect(
                    &session,
                    &profile.value,
                    take_over,
                    self.config.routing.get(),
                    &self.choices(),
                )
                .await;
            steps.done("service Connect (core spawn, profile, TUN start)");
            if let Err(error) = connected {
                let (info, retryable) = connect_failure(&error);
                return failed(info, retryable).await;
            }
            let status = self.service.get_status().await;
            steps.done("service status");
            match status {
                Ok(status)
                    if status.running
                        && status.session_id.as_deref() == Some(session.session_id.as_str())
                        && status.generation == generation => {}
                Ok(_) => {
                    return failed(
                        ClientErrorInfo::new(ErrorCode::ConnectFailed, "CORE_NOT_RUNNING"),
                        true,
                    )
                    .await
                }
                Err(error) => {
                    let info = ClientErrorInfo::new(
                        ErrorCode::ServiceUnavailable,
                        format!("SERVICE_STATUS_UNAVAILABLE: {}", error.detail()),
                    );
                    return failed(info, true).await;
                }
            }

            let core = ServiceCoreTransport {
                service: self.service.clone(),
                session: session.clone(),
            };
            let mut healthy = self
                .run_health(&core, &profile.revision, true, ENTRANCE_PROBE_TIMEOUT_MS)
                .await;
            if let Err(info) = &healthy {
                // Right after a core was killed the platform can take a few
                // seconds to settle (Windows: the old adapter, filters and
                // DNS cache go away), and the first check of a reconnect
                // failed where the next one passed: one more, after a pause.
                // So for the TUN core's counters not read again in time.
                if (is_path_cause(info) || is_traffic_stale(info))
                    && self.is_current(generation, ConnectionPhase::Connecting)
                {
                    tracing::info!(
                        generation,
                        detail = %info.detail,
                        "enhanced health failed; checking again after {} ms",
                        HEALTH_SETTLE.as_millis()
                    );
                    tokio::time::sleep(HEALTH_SETTLE).await;
                    healthy = self
                        .run_health(&core, &profile.revision, true, ENTRANCE_PROBE_TIMEOUT_MS)
                        .await;
                }
            }
            steps.done("health check");
            if let Err(info) = healthy {
                return failed(info, true).await;
            }
            steps.finish("on");
            self.with_data(|data| data.recovery.note_health_success());
            self.start_loops(generation);
            self.transition(generation, ConnectionPhase::On, None, false);
            Ok(())
        })
    }

    /// Starts the conflict check in the background (see [`Preflight`]).
    fn start_preflight(&self) -> Preflight {
        let detector = self
            .detector
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        Preflight {
            task: tokio::task::spawn_blocking(move || detector()),
            deadline: Instant::now() + PREFLIGHT_BUDGET,
        }
    }

    /// Data-plane health: core running the expected revision with a selected
    /// node whose entrance answers; for official API hosts additionally a
    /// DIRECT request that must be captured by TUN and a proxied request
    /// through the standard core's local proxy for the same node.
    /// `first`: the check of a connect attempt, whose step durations are
    /// logged at info level (debug for the periodic checks).
    async fn run_health(
        &self,
        core: &ServiceCoreTransport<dyn ServiceApi>,
        expected_revision: &str,
        first: bool,
        entrance_timeout_ms: u64,
    ) -> Result<(), ClientErrorInfo> {
        let mut steps = Steps::new("enhanced health", core.session.generation, first);
        // The capture check needs traffic counters read after this (the
        // status read below already gets the engine reading its runtime).
        let started = SystemTime::now();
        let status = core_ipc::get_status(core)
            .await
            .map_err(|error| health_error(format!("HEALTH_STATUS_FAILED: {}", error.detail())))?;
        // A degraded engine still forwards: engine-host reports it as
        // `running` (reasons kept), the engine itself as `degraded`. Its
        // reasons heal themselves, all but broken TUN routing below.
        if !status.is_serving() {
            return Err(health_error("HEALTH_CORE_NOT_RUNNING"));
        }
        if status.revision.as_deref() != Some(expected_revision) {
            return Err(health_error("HEALTH_PROFILE_REVISION_MISMATCH"));
        }
        if status.tun_routing_broken {
            // Traffic may bypass the TUN until the next start: a fresh core
            // installs the routes again (within the reconnect budget).
            return Err(service_unreachable(TUN_ROUTING_BROKEN));
        }
        let node_id = status
            .selected_node_id
            .ok_or_else(|| health_error("HEALTH_SELECTED_NODE_MISSING"))?;

        steps.done("core status");
        let entrances = core_ipc::probe_entrances(
            core,
            "tcp",
            std::slice::from_ref(&node_id),
            entrance_timeout_ms,
            4,
        )
        .await;
        steps.done("entrance probe");
        let entrances = entrances
            .map_err(|error| health_error(format!("HEALTH_ENTRANCE_FAILED: {}", error.detail())))?;
        if entrances
            .first()
            .and_then(|result| result.get("success"))
            .and_then(Value::as_bool)
            != Some(true)
        {
            return Err(health_error("HEALTH_ENTRANCE_FAILED"));
        }

        #[cfg(test)]
        if let Some(scripted) = self
            .official_steps
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            let (direct, proxied) = scripted();
            return official_verdict(direct, proxied);
        }
        let Some(target) = health_url(&self.config.api_base) else {
            return Ok(());
        };

        let host = target.host_str().unwrap_or("");
        let before = capture_baseline(core, started, host).await?;
        let direct = direct_health_request(target.clone()).await;
        let done = SystemTime::now();
        steps.done("direct request");
        // A failed direct request leaves nothing to judge the capture by;
        // the proxied path still runs and decides what the failure means.
        let direct = match direct {
            Ok(remote) => {
                capture_verdict(core, before, done, remote, host).await?;
                steps.done("capture check");
                Ok(())
            }
            Err(error) => Err(error),
        };
        let proxied = self.proxied_path(&node_id, &target).await;
        steps.done("proxied-path probe");
        official_verdict(direct, proxied)
    }

    /// The proxied-path step: the selected node reached through the standard
    /// core's local proxy. The privileged core runs TUN only
    /// (`--local-proxy=false`), so the path is exercised through the
    /// standard core.
    async fn proxied_path(
        &self,
        node_id: &str,
        target: &reqwest::Url,
    ) -> Result<(), ClientErrorInfo> {
        let standard = self
            .standard
            .as_ref()
            .ok_or_else(|| health_error("HEALTH_PROXY_PATH_FAILED: STANDARD_CORE_MISSING"))?
            .transport()
            .map_err(|_| health_error("HEALTH_PROXY_PATH_FAILED: STANDARD_NOT_READY"))?;
        let availability =
            core_ipc::probe_availability(standard.as_ref(), node_id, target.as_str(), 8_000)
                .await
                .map_err(|error| {
                    health_error(format!("HEALTH_PROXY_PATH_FAILED: {}", error.detail()))
                })?;
        if availability.get("success").and_then(Value::as_bool) != Some(true) {
            return Err(health_error("HEALTH_PROXY_PATH_FAILED"));
        }
        Ok(())
    }

    /// While on: `On` carries [`API_UNREACHABLE`] as its reason while only
    /// the API host is unreachable on the direct path, and none once a
    /// check passes. Nothing changes for another generation or phase.
    fn note_api_unreachable(&self, generation: u64, unreachable: bool) {
        let reason = unreachable.then(|| service_unreachable(API_UNREACHABLE));
        let applied = self.with_data(|data| {
            data.machine.is(generation, ConnectionPhase::On)
                && data.machine.reason != reason
                && data
                    .machine
                    .transition(generation, ConnectionPhase::On, reason, false)
        });
        if applied {
            self.emit();
        }
    }

    fn start_loops(self: &Arc<Self>, generation: u64) {
        let claimed = self.with_data(|data| {
            if data.loops_generation == Some(generation) {
                return false;
            }
            data.loops_generation = Some(generation);
            data.loops.retain(|task| !task.is_finished());
            true
        });
        if !claimed {
            return;
        }
        let Some(session) = self.with_data(|data| data.machine.session()) else {
            return;
        };
        let tasks = vec![
            tokio::spawn(lease_loop(self.clone(), session.clone())),
            tokio::spawn(health_loop(self.clone(), session.clone())),
            tokio::spawn(watch_loop(self.clone(), session)),
        ];
        self.with_data(|data| data.loops.extend(tasks));
    }

    /// Cancels the lease, health and watch loops (and a health check,
    /// renewal or watch connection they have in flight). Recovery is never part of a loop task, so this
    /// is safe to call from it.
    fn stop_loops(&self) {
        let loops = self.with_data(|data| {
            data.loops_generation = None;
            std::mem::take(&mut data.loops)
        });
        for task in loops {
            task.abort();
        }
    }

    /// Handles a finished health check of `generation`. A result for a
    /// generation that is no longer the current `On` one is discarded: it
    /// neither counts as a healthy check nor triggers recovery. A failure
    /// of the current generation starts recovery in its own task. Returns
    /// whether the health loop continues.
    fn on_health_result(
        self: &Arc<Self>,
        generation: u64,
        result: Result<(), ClientErrorInfo>,
    ) -> bool {
        let (current, on) = self.with_data(|data| {
            let current = data.machine.generation == generation;
            let on = current && data.machine.phase == ConnectionPhase::On;
            if on && result.is_ok() {
                data.recovery.note_health_success();
            }
            (current, on)
        });
        if !on {
            tracing::debug!(
                generation,
                healthy = result.is_ok(),
                "enhanced health result for a stale session discarded"
            );
            // Same generation, briefly not on (profile update): keep going.
            return current;
        }
        match result {
            Ok(()) => true,
            Err(error) => {
                tracing::warn!(?error, "enhanced health check failed");
                tokio::spawn(self.clone().recover(generation, error));
                false
            }
        }
    }

    /// Automatic recovery: one reconnect per episode; for broken TUN
    /// routing, within [`RoutingReconnects`].
    /// Never prompts for installation.
    ///
    /// `cause` is what failed while on. Only a network-path failure (see
    /// [`is_path_cause`]) ends in `Contended{NetworkPathContended}`; the
    /// service or core going away (a service restart kills the core: the
    /// lease renewal cannot reach the service, the core API does not answer)
    /// ends in `Error` with the actual reason, so the apps do not claim
    /// another app took the network over.
    fn recover(self: Arc<Self>, generation: u64, cause: ClientErrorInfo) -> BoxFuture<'static, ()> {
        Box::pin(async move {
            let _operation = self.operation.lock().await;
            let (machine, claimed, has_profile) = self.with_data(|data| {
                if !data.machine.is(generation, ConnectionPhase::On) {
                    return (data.machine.clone(), false, false);
                }
                (
                    data.machine.clone(),
                    data.recovery.claim(),
                    data.profile.is_some(),
                )
            });
            if !machine.is(generation, ConnectionPhase::On) {
                return;
            }
            let path = is_path_cause(&cause);
            let service_down = is_service_down(&cause);
            let routing_broken = is_tun_routing_broken(&cause);
            // With this episode's reconnect already used, still reconnect,
            // but only after the service backoff (bounded: a core that keeps
            // dying or a path that stays broken ends in Error / Contended).
            // Ending here instead left the core (and its TUN) running under
            // a failed state, deaf to what the service reported next.
            if !has_profile {
                self.stop_loops();
                self.release_session(&machine).await;
                if path {
                    self.transition(
                        generation,
                        ConnectionPhase::Contended,
                        contended("NETWORK_PATH_CONTENDED"),
                        true,
                    );
                } else {
                    self.transition(generation, ConnectionPhase::Error, Some(cause), true);
                }
                return;
            }
            // Another program changed the TUN's routing (another VPN, as a
            // rule). It stays so until the next start, but that program may
            // take it again at once: past the budget, leave the network to
            // it (our routes go with the session) and let the user decide.
            if routing_broken && !self.claim_routing_reconnect() {
                self.stop_loops();
                self.release_session(&machine).await;
                self.routing_taken_over(generation);
                return;
            }
            self.transition(
                generation,
                ConnectionPhase::Reconnecting,
                if path {
                    contended("NETWORK_PATH_RECOVERY")
                } else {
                    Some(cause)
                },
                true,
            );
            self.release_session(&machine).await;
            let mut attempt = 0;
            // The first reconnect is immediate, unless the service itself
            // is going away (stopping, restarting, gone): then, as after
            // every such failure, the next attempt waits (see
            // [`service_backoff`]) without holding the operation lock.
            // Broken TUN routing may leak traffic: no pause, the budget
            // bounds it.
            let mut wait = service_down || (!claimed && !routing_broken);
            let mut generation = generation;
            let mut operation = Some(_operation);
            loop {
                if wait {
                    let Some(delay) = service_backoff(attempt) else {
                        // Out of attempts: stop saying "reconnecting".
                        let reason = self.with_data(|data| data.machine.reason.clone());
                        self.transition(generation, ConnectionPhase::Error, reason, true);
                        return;
                    };
                    attempt += 1;
                    drop(operation.take());
                    tracing::info!(
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        "enhanced mode: reconnecting after a pause"
                    );
                    // A network change cuts the pause short, but not before
                    // NETWORK_WAKE_MIN_PAUSE: each attempt's own TUN coming
                    // and going is reported as a change too, and waking on it
                    // defeated the backoff (Linux: a core restart every 5 s
                    // while the API was unreachable).
                    let resume_at = Instant::now() + delay;
                    let earliest_wake = Instant::now() + NETWORK_WAKE_MIN_PAUSE.min(delay);
                    loop {
                        tokio::select! {
                            () = tokio::time::sleep_until(resume_at) => break,
                            () = self.network_changed.notified() => {
                                if Instant::now() < earliest_wake {
                                    continue;
                                }
                                tracing::info!("enhanced mode: network changed; reconnecting now");
                                tokio::time::sleep(NETWORK_SETTLE).await;
                                break;
                            }
                        }
                    }
                    let lock = self.operation.lock().await;
                    // A user action (disconnect, retry, mode switch,
                    // uninstall) took over while waiting.
                    if !self.is_current(generation, ConnectionPhase::Reconnecting) {
                        return;
                    }
                    operation = Some(lock);
                }
                // Attempts left after this one: a service-down failure then
                // stays `Reconnecting` instead of flashing `Error`.
                let retrying = service_backoff(attempt).is_some();
                if self.clone().connect(false, false, retrying).await.is_ok() {
                    return;
                }
                // The reconnect ended with its own reason. The service being
                // down is retried after a longer pause (the attempt left the
                // phase `Reconnecting`); someone else owning the connection
                // is reported as such; another app named as owning the
                // network path ends in Contended; any other path failure is
                // retried after the pause; anything else (core not running)
                // stays as it is.
                let (current, phase, reason, competitors) = self.with_data(|data| {
                    (
                        data.machine.generation,
                        data.machine.phase,
                        data.machine.reason.clone(),
                        data.machine.competitors.clone(),
                    )
                });
                // The new core's first check found the routing taken again.
                if phase == ConnectionPhase::Reconnecting
                    && reason.as_ref().is_some_and(is_tun_routing_broken)
                    && !self.claim_routing_reconnect()
                {
                    self.routing_taken_over(current);
                    return;
                }
                match reason {
                    Some(reason)
                        if phase == ConnectionPhase::Reconnecting && is_service_down(&reason) =>
                    {
                        generation = current;
                        wait = true;
                    }
                    // Nobody named as owning the path: the platform settling
                    // after a core or service restart, or a network hiccup.
                    Some(reason)
                        if is_path_cause(&reason)
                            && competitors.is_empty()
                            && service_backoff(attempt).is_some() =>
                    {
                        if !self.transition(
                            current,
                            ConnectionPhase::Reconnecting,
                            Some(reason),
                            true,
                        ) {
                            return;
                        }
                        generation = current;
                        wait = true;
                    }
                    // Another app named as owning the path. Nobody named
                    // and out of attempts: the failure stays an Error.
                    Some(reason) if is_path_cause(&reason) && !competitors.is_empty() => {
                        let reason = if reason.code == ErrorCode::NetworkPathContended {
                            Some(reason)
                        } else {
                            contended("NETWORK_PATH_CONTENDED")
                        };
                        self.transition(current, ConnectionPhase::Contended, reason, true);
                        return;
                    }
                    _ => return,
                }
            }
        })
    }
}

/// The service is stopping, restarting or not running (as opposed to
/// missing: an uninstalled service is not waited for).
fn is_service_down(info: &ClientErrorInfo) -> bool {
    info.code == ErrorCode::ServiceUnavailable && !info.detail.contains("SERVICE_NOT_INSTALLED")
}

/// Pause before automatic reconnect attempt `attempt` (0-based) while the
/// service is down: 2, 4, 8, 16, 30, 30, 30 s; `None` after the last one.
fn service_backoff(attempt: u32) -> Option<Duration> {
    (attempt < SERVICE_BACKOFF_ATTEMPTS).then(|| {
        SERVICE_BACKOFF_BASE
            .saturating_mul(1u32 << attempt.min(16))
            .min(SERVICE_BACKOFF_MAX)
    })
}

/// A failure caused by the network path (another app's tunnel or DNS, the
/// route to the node), not by the service or the core: the direct / capture
/// health steps, the entrance and proxied-path probes, and a detected
/// conflict. The core not answering (`HEALTH_STATUS_FAILED`, killed by a
/// service restart), the service being unreachable, or ownership are not.
fn is_path_cause(info: &ClientErrorInfo) -> bool {
    match info.code {
        ErrorCode::NetworkPathContended | ErrorCode::ConnectHealthCheckFailed => true,
        ErrorCode::ConnectFailed => ["HEALTH_ENTRANCE_FAILED", "HEALTH_PROXY_PATH_FAILED"]
            .iter()
            .any(|step| info.detail.starts_with(step)),
        _ => false,
    }
}

/// What the official host's direct request and proxied-path probe say
/// together. The proxied path failing is the tunnel's failure (with the
/// direct request's reason when that failed too, as before). The proxied
/// path working while the direct request failed means the API host is not
/// reachable on the direct path, e.g. a lossy route from the ISP to it
/// (the profile routes it DIRECT): the tunnel works (#438).
fn official_verdict(
    direct: Result<(), ClientErrorInfo>,
    proxied: Result<(), ClientErrorInfo>,
) -> Result<(), ClientErrorInfo> {
    match (direct, proxied) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(direct), Err(_)) => Err(direct),
        (Ok(()), Err(proxied)) => Err(proxied),
        (Err(direct), Ok(())) => Err(service_unreachable(format!(
            "{API_UNREACHABLE}: {}",
            direct.detail
        ))),
    }
}

/// Only the API host is unreachable on the direct path (see
/// [`official_verdict`]).
fn is_api_unreachable(info: &ClientErrorInfo) -> bool {
    info.code == ErrorCode::ConnectHealthCheckFailed && info.detail.starts_with(API_UNREACHABLE)
}

/// Traffic bypasses the TUN: the capture check failed, or its routing is
/// broken. Recovered from at once, never put down to the network.
fn is_leak(info: &ClientErrorInfo) -> bool {
    info.detail.contains("HEALTH_CAPTURE_PATH_FAILED") || is_tun_routing_broken(info)
}

/// The health policy's pauses: the production ones, scaled down in tests.
fn health_timing() -> HealthTiming {
    HealthTiming {
        recheck: HEALTH_RECHECK,
        network_recheck: NETWORK_RECHECK,
        network_grace: NETWORK_GRACE,
        ..HealthTiming::default()
    }
}

/// A round's result as the health policy takes it.
fn health_round(result: &Result<(), ClientErrorInfo>) -> Round {
    match result {
        Ok(()) => Round::Healthy,
        Err(error) if is_api_unreachable(error) => Round::ApiUnreachable,
        Err(error) if is_path_cause(error) && !is_leak(error) => Round::PathFailed,
        Err(_) => Round::Failed,
    }
}

/// `Some(url)` when the full health transaction (the direct request, the
/// capture check, the proxied path) applies to `api_base`: a backend on the
/// internet, its https API host a domain name, which is first-party by
/// definition (#517: the host comes from the build, none is written here).
/// The direct request proves that TUN captures the app's own traffic (the
/// core's counters grow), whichever outbound the profile routes it to
/// (DIRECT once the profile carries the first-party rule, the node until
/// then). Not for a local or test backend: an IP address, `localhost` or
/// plain http, which the tunnel does not carry.
fn health_target(api_base: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(api_base).ok()?;
    let host = url
        .host_str()?
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let public = url.scheme() == "https"
        && host.parse::<std::net::IpAddr>().is_err()
        && host.contains('.')
        && !host.ends_with(".localhost");
    public.then_some(url)
}

/// The backend's health endpoint (`GET /api/v1/health`: 200, `no-store`, no
/// auth) with a unique probe query, when the health transaction applies.
fn health_url(api_base: &str) -> Option<reqwest::Url> {
    let mut url = health_target(api_base)?;
    url.set_path(HEALTH_PATH);
    url.set_query(Some(&format!(
        "ppvpn_probe={}",
        uuid::Uuid::new_v4().simple()
    )));
    Some(url)
}

/// The DIRECT step of the health transaction: a bare GET (no cookies, no
/// credentials, no proxy, no redirects, no caches) that must answer 2xx.
async fn direct_health_request(
    target: reqwest::Url,
) -> Result<Option<std::net::SocketAddr>, ClientErrorInfo> {
    let client = reqwest::Client::builder()
        .timeout(HEALTH_REQUEST_TIMEOUT)
        .connect_timeout(HEALTH_REQUEST_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .map_err(|_| health_error("HEALTH_DIRECT_CLIENT_FAILED"))?;
    let response = client
        .get(target)
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .header(reqwest::header::PRAGMA, "no-cache")
        .send()
        .await
        .map_err(|_| service_unreachable("HEALTH_DIRECT_PATH_FAILED"))?;
    if !response.status().is_success() {
        return Err(service_unreachable(format!(
            "HEALTH_DIRECT_STATUS_FAILED: {}",
            response.status().as_u16()
        )));
    }
    // Where the request went: what to look at when the capture check fails.
    let remote = response.remote_addr();
    let _ = response.bytes().await;
    Ok(remote)
}

/// Detail of the health failure when the TUN core's traffic counters were
/// not read late enough to judge the capture check. Neither a path failure
/// nor a leak: the core is not doing its part, as when its traffic call
/// fails (`HEALTH_TRAFFIC_FAILED`), so recovery reconnects with this cause.
const TRAFFIC_STALE: &str = "HEALTH_TRAFFIC_STALE";
/// How long the capture check waits for a sample of the TUN core's
/// counters read late enough. The engine reads its runtime about once a
/// second while a host reads, and at once on the first read after it went
/// idle (docs/host-integration.md 4.4).
const TRAFFIC_FRESH_WAIT: Duration = if cfg!(test) {
    Duration::from_millis(300)
} else {
    Duration::from_secs(3)
};
/// Between reads while waiting for such a sample.
const TRAFFIC_POLL: Duration = if cfg!(test) {
    Duration::from_millis(5)
} else {
    Duration::from_millis(100)
};

fn is_traffic_stale(info: &ClientErrorInfo) -> bool {
    info.code == ErrorCode::ConnectFailed && info.detail.starts_with(TRAFFIC_STALE)
}

/// The TUN core's counters once the engine's sample of them is one `fresh`
/// accepts (by its `measured_at`), within [`TRAFFIC_FRESH_WAIT`];
/// `Ok(Err(last))` when none came. A sample counts once two reads in a row
/// return it: until the engine first reads its runtime it dates its zero
/// counters with the time of each call, which no two calls share.
async fn fresh_traffic(
    core: &dyn CoreTransport,
    fresh: impl Fn(SystemTime) -> bool,
) -> Result<Result<CoreTraffic, CoreTraffic>, ClientErrorInfo> {
    let deadline = Instant::now() + TRAFFIC_FRESH_WAIT;
    let mut previous: Option<CoreTraffic> = None;
    loop {
        let sample = core_ipc::get_traffic(core)
            .await
            .map_err(|error| health_error(format!("HEALTH_TRAFFIC_FAILED: {}", error.detail())))?;
        let repeated = previous == Some(sample);
        if repeated && sample.measured_at.is_some_and(&fresh) {
            return Ok(Ok(sample));
        }
        if Instant::now() >= deadline {
            return Ok(Err(sample));
        }
        previous = Some(sample);
        tokio::time::sleep(TRAFFIC_POLL).await;
    }
}

/// The capture check's baseline: the TUN core's counters as read no earlier
/// than `since`, when the health check started. An older sample (the first
/// read after the engine went idle, or the previous run's) can miss what
/// went through since, or not be this run's at all.
async fn capture_baseline(
    core: &dyn CoreTransport,
    since: SystemTime,
    host: &str,
) -> Result<CoreTraffic, ClientErrorInfo> {
    fresh_traffic(core, |at| at >= since)
        .await?
        .map_err(|last| traffic_stale("before", since, last, host))
}

/// The capture check's verdict: the direct request, done at `done`, must
/// show in the TUN core's counters as read after it.
async fn capture_verdict(
    core: &dyn CoreTransport,
    before: CoreTraffic,
    done: SystemTime,
    remote: Option<std::net::SocketAddr>,
    host: &str,
) -> Result<(), ClientErrorInfo> {
    let mut after = fresh_traffic(core, |at| at > done)
        .await?
        .map_err(|last| traffic_stale("after", done, last, host))?;
    if after.total() <= before.total() {
        // The engine dates a sample when its read of the runtime returns:
        // one dated just after `done` may have been read before the
        // request's last bytes. The next one was not.
        let first = after.measured_at;
        after = fresh_traffic(core, |at| Some(at) > first)
            .await?
            .map_err(|last| traffic_stale("after", done, last, host))?;
    }
    if after.total() <= before.total() {
        // The request did not show in the TUN's counters: it left another
        // way. Name where it went for the investigation.
        tracing::warn!(
            remote = ?remote,
            host,
            before = before.total(),
            after = after.total(),
            before_at = %measured(before.measured_at),
            after_at = %measured(after.measured_at),
            done_at = %rfc3339(done),
            "enhanced health: the direct request bypassed the TUN"
        );
        return Err(service_unreachable("HEALTH_CAPTURE_PATH_FAILED"));
    }
    Ok(())
}

/// No sample of the TUN core's counters read after `since` came in time:
/// the capture check cannot tell, which is not a leak.
fn traffic_stale(step: &str, since: SystemTime, last: CoreTraffic, host: &str) -> ClientErrorInfo {
    tracing::warn!(
        host,
        step,
        since = %rfc3339(since),
        last_at = %measured(last.measured_at),
        last = last.total(),
        "enhanced health: the TUN's traffic counters were not read again in {} ms",
        TRAFFIC_FRESH_WAIT.as_millis()
    );
    health_error(format!("{TRAFFIC_STALE}: {step}"))
}

fn measured(at: Option<SystemTime>) -> String {
    at.map_or_else(|| "unknown".to_string(), rfc3339)
}

/// RFC 3339 in UTC with milliseconds, for logs.
fn rfc3339(at: SystemTime) -> String {
    let since_epoch = at.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let seconds = since_epoch.as_secs() % 86_400;
    format!(
        "{}T{:02}:{:02}:{:02}.{:03}Z",
        crate::standard::utc_date(at),
        seconds / 3_600,
        seconds / 60 % 60,
        seconds % 60,
        since_epoch.subsec_millis()
    )
}

async fn lease_loop(inner: Arc<Inner>, session: SessionRef) {
    let generation = session.generation;
    loop {
        tokio::time::sleep(LEASE_RENEW_INTERVAL).await;
        if !inner.is_current(generation, ConnectionPhase::On) {
            break;
        }
        let renewed = inner.service.renew_lease(&session).await;
        if inner.with_data(|data| data.machine.generation != generation) {
            tracing::debug!(generation, "lease renewal for a stale session discarded");
            break;
        }
        if let Ok(status) = &renewed {
            inner.note_service_build(status);
        }
        if let Err(error) = renewed {
            if let Some((info, retryable)) = ownership_lost(&error) {
                // Taken over: no reconnect attempt, just "in use elsewhere".
                tracing::info!("enhanced mode: connection taken over ({:?})", info.code);
                inner.transition(
                    generation,
                    ConnectionPhase::Contended,
                    Some(info),
                    retryable,
                );
                break;
            }
            tracing::warn!(?error, "lease renewal failed");
            // Own task: the reconnect stops this loop.
            tokio::spawn(inner.clone().recover(generation, error.info()));
            break;
        }
    }
}

/// What a watch event means for the connection.
enum WatchVerdict {
    /// Nothing proven: lease renewal keeps deciding.
    Fallback,
    /// Reconnect (the recovery path), for this reason.
    Recover(ClientErrorInfo),
    /// Another session of this user took the connection over.
    TakenOver(ClientErrorInfo),
}

fn watch_verdict(end: WatchEnd) -> WatchVerdict {
    let stopping = || ClientErrorInfo::new(ErrorCode::ServiceUnavailable, "SERVICE_STOPPING");
    let core_stopped = |reason: &str| {
        ClientErrorInfo::new(ErrorCode::ConnectFailed, format!("CORE_STOPPED: {reason}"))
    };
    match end {
        WatchEnd::Unsupported | WatchEnd::Abandoned(_) | WatchEnd::Cancelled => {
            WatchVerdict::Fallback
        }
        WatchEnd::Stopping => WatchVerdict::Recover(stopping()),
        // EOF / broken pipe: the service is gone; reconnect after the pause
        // for a service that is down.
        WatchEnd::Lost(detail) => WatchVerdict::Recover(ClientErrorInfo::new(
            ErrorCode::ServiceUnavailable,
            format!("SERVICE_WATCH_LOST: {detail}"),
        )),
        WatchEnd::CoreStopped(reason) => match reason.as_str() {
            "taken_over" => WatchVerdict::TakenOver(ClientErrorInfo::new(
                ErrorCode::ServiceBusy,
                "CONNECTION_TAKEN_OVER",
            )),
            "service_stopping" => WatchVerdict::Recover(stopping()),
            reason => WatchVerdict::Recover(core_stopped(reason)),
        },
        WatchEnd::Refused(error) => {
            if let Some((info, _)) = ownership_lost(&error) {
                return WatchVerdict::TakenOver(info);
            }
            match error.reason_code() {
                Some("SERVICE_STOPPING") => WatchVerdict::Recover(stopping()),
                // The core is already gone.
                Some("CONNECTION_LEASE_EXPIRED" | "CONNECTION_NOT_ACTIVE") => {
                    WatchVerdict::Recover(core_stopped(&error.detail()))
                }
                _ => WatchVerdict::Fallback,
            }
        }
    }
}

/// Watches the session's core through the service (see
/// [`ServiceApi::watch`]) and, when the service reports it stopping or the
/// core gone, or the watch connection breaks, leaves `On` at once through
/// the usual recovery. Ends with the generation's loops (a stale watch
/// result is discarded).
async fn watch_loop(inner: Arc<Inner>, session: SessionRef) {
    let generation = session.generation;
    let end = inner.service.watch(&session).await;
    let verdict = match &end {
        WatchEnd::Unsupported => {
            tracing::info!(generation, "service watch unsupported; lease renewal only");
            WatchVerdict::Fallback
        }
        WatchEnd::Cancelled => return,
        end => {
            tracing::info!(generation, ?end, "service watch ended");
            watch_verdict(end.clone())
        }
    };
    if matches!(verdict, WatchVerdict::Fallback) {
        return;
    }
    // A profile update may have the generation briefly `Reconnecting`: act
    // once it is back `On`; anything else took over already.
    loop {
        let (current, phase) =
            inner.with_data(|data| (data.machine.generation == generation, data.machine.phase));
        if !current {
            tracing::debug!(
                generation,
                "service watch result for a stale session discarded"
            );
            return;
        }
        match phase {
            ConnectionPhase::On => break,
            ConnectionPhase::Reconnecting => tokio::time::sleep(Duration::from_millis(50)).await,
            _ => return,
        }
    }
    match verdict {
        WatchVerdict::Fallback => {}
        WatchVerdict::TakenOver(info) => {
            tracing::info!("enhanced mode: connection taken over ({:?})", info.code);
            inner.transition(generation, ConnectionPhase::Contended, Some(info), true);
        }
        WatchVerdict::Recover(cause) => {
            tracing::warn!(
                generation,
                ?cause,
                "enhanced mode: tunnel lost (service watch)"
            );
            // Own task: the reconnect stops this loop.
            tokio::spawn(inner.clone().recover(generation, cause));
        }
    }
}

async fn health_loop(inner: Arc<Inner>, session: SessionRef) {
    let generation = session.generation;
    let core = ServiceCoreTransport {
        service: inner.service.clone(),
        session,
    };
    let mut awaiting_verdict = false;
    let mut health = Health::new(health_timing());
    loop {
        let base = *inner
            .health_interval
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let interval = health.interval(base, Instant::now().into_std());
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = inner.network_changed.notified() => {
                health.network_changed(Instant::now().into_std());
                tokio::time::sleep(NETWORK_SETTLE).await;
                tracing::info!(generation, "enhanced health: network changed; checking now");
            }
        }
        if !inner.is_current(generation, ConnectionPhase::On) {
            break;
        }
        // A profile or routing-mode update holds the operation lock while the
        // core switches engines: skip this round rather than judge the path
        // mid-switch.
        if inner.operation.try_lock().is_err() {
            continue;
        }
        let Some(revision) = inner.with_data(|data| data.machine.revision.clone()) else {
            break;
        };
        let checked_at = Instant::now().into_std();
        let timeout = if health.settling(checked_at) {
            SETTLING_ENTRANCE_PROBE_TIMEOUT_MS
        } else {
            ENTRANCE_PROBE_TIMEOUT_MS
        };
        let result = inner.run_health(&core, &revision, false, timeout).await;
        let round = health_round(&result);
        // Traffic that bypasses the TUN (its routing rules gone, e.g.
        // systemd-networkd dropping foreign rules on a link change) is a
        // leak, not the network settling: no grace, recover now.
        if health.in_grace(round, checked_at) {
            if let Err(error) = &result {
                tracing::info!(
                    generation,
                    detail = %error.detail,
                    "enhanced health failed while the network settles; checking again in {} ms",
                    NETWORK_RECHECK.as_millis()
                );
            }
            continue;
        }
        // A path failure of a node the user pinned to a line that is down is
        // that choice, not a broken tunnel: stay on (the notice says so)
        // instead of reconnecting, which would only tear the TUN down again
        // and again. Once the line is back, or the user unpins, the checks
        // pass again. A check before the core's verdict gets one more round.
        // Broken TUN routing is no line's doing.
        if let Err(error) = &result {
            if is_path_cause(error) && !is_tun_routing_broken(error) {
                match pinned_ingress(&core, &inner.config.ingress_pins.get()).await {
                    PinnedIngress::Down => {
                        tracing::info!(
                            generation,
                            "enhanced health failed on the pinned line, which is down; staying on"
                        );
                        awaiting_verdict = false;
                        continue;
                    }
                    PinnedIngress::Unchecked if !awaiting_verdict => {
                        tracing::info!(
                            generation,
                            "enhanced health failed on a pinned line not checked yet; next round decides"
                        );
                        awaiting_verdict = true;
                        continue;
                    }
                    _ => {}
                }
            }
        }
        awaiting_verdict = false;
        match health.judge(round) {
            // The tunnel works; only the API host is unreachable on the
            // direct path. Restarting the core cannot fix that route: stay
            // on, say so, and check less often (#438).
            Verdict::ApiUnreachable { misses } => {
                tracing::info!(
                    generation,
                    misses,
                    detail = %result.as_ref().err().map_or("", |e| e.detail.as_str()),
                    "enhanced health: the tunnel works but the API host is unreachable on the direct path; staying on"
                );
                inner.note_api_unreachable(generation, true);
                // A healthy round for the tunnel (and the recovery episode).
                if !inner.on_health_result(generation, Ok(())) {
                    break;
                }
                continue;
            }
            // A path failure that is no leak: a lost probe on a lossy link
            // must not restart the core. Recheck shortly; recover only when
            // the recheck and the next round fail too (#438).
            Verdict::Recheck => {
                tracing::info!(
                    generation,
                    detail = %result.as_ref().err().map_or("", |e| e.detail.as_str()),
                    "enhanced health failed on the path; checking again before recovering"
                );
                continue;
            }
            Verdict::Healthy { api_cleared } => {
                if api_cleared {
                    inner.note_api_unreachable(generation, false);
                }
            }
            Verdict::Recover => {}
        }
        if !inner.on_health_result(generation, result) {
            break;
        }
    }
}

/// The selected node's pin, as the core in use sees it.
#[derive(Debug, PartialEq, Eq)]
enum PinnedIngress {
    /// Not pinned (or a single-line node, where a pin changes nothing).
    None,
    /// Pinned, and the core's checks say the line is down.
    Down,
    /// Pinned, not checked yet (the core has no verdict).
    Unchecked,
    /// Pinned and healthy: a failure has another cause.
    Up,
}

async fn pinned_ingress(core: &dyn CoreTransport, pins: &crate::ingress::Pins) -> PinnedIngress {
    if pins.is_empty() {
        return PinnedIngress::None;
    }
    let Ok(status) = core_ipc::get_status(core).await else {
        return PinnedIngress::None;
    };
    let Some(node_id) = status.selected_node_id.as_deref() else {
        return PinnedIngress::None;
    };
    let Some(pinned) = pins.get(node_id) else {
        return PinnedIngress::None;
    };
    let Some(node) = status
        .nodes
        .iter()
        .flatten()
        .find(|node| node.node_id == node_id)
    else {
        return PinnedIngress::None;
    };
    if node.ingresses.len() < 2 {
        return PinnedIngress::None;
    }
    match node
        .ingresses
        .iter()
        .find(|ingress| &ingress.endpoint_key == pinned)
        .map(|ingress| ingress.healthy)
    {
        Some(Some(false)) => PinnedIngress::Down,
        Some(Some(true)) => PinnedIngress::Up,
        Some(None) => PinnedIngress::Unchecked,
        None => PinnedIngress::None,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::{ServiceStatus, VersionInfo};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    // --- pure logic -------------------------------------------------------

    #[test]
    fn stale_generation_cannot_overwrite_new_session() {
        let mut machine = Machine::default();
        let first = machine.begin_connect("r1").unwrap();
        let second = machine.begin_connect("r1").unwrap();
        assert_eq!((first.generation, second.generation), (1, 2));
        assert_ne!(first.session_id, second.session_id);
        assert!(!machine.transition(first.generation, ConnectionPhase::On, None, false));
        assert_eq!(machine.phase, ConnectionPhase::Preparing);
        assert!(machine.transition(second.generation, ConnectionPhase::Connecting, None, false));
        assert!(!machine.finish_disconnect(first.generation));
        assert!(machine.finish_disconnect(second.generation));
        assert_eq!(machine.phase, ConnectionPhase::Off);
        assert_eq!(machine.generation, 2, "generation survives disconnect");
        assert!(!machine.desired_on);
        assert!(machine.session().is_none());
        assert_eq!(machine.begin_connect("r2").unwrap().generation, 3);
    }

    #[test]
    fn generation_overflow_is_reported() {
        let mut machine = Machine {
            generation: u64::MAX,
            ..Machine::default()
        };
        assert_eq!(
            machine.begin_connect("r").unwrap_err().code,
            ErrorCode::Internal
        );
    }

    #[test]
    fn traffic_meter_computes_rates_and_survives_counter_resets() {
        let start = Instant::now();
        let mut meter = TrafficMeter::default();
        let first = meter.sample(1_000, 5_000, start);
        assert_eq!(
            (first.up_bps, first.down_bps, first.up_total),
            (0, 0, 1_000)
        );
        let second = meter.sample(3_000, 9_000, start + Duration::from_secs(2));
        assert_eq!((second.up_bps, second.down_bps), (1_000, 2_000));
        let reset = meter.sample(10, 10, start + Duration::from_secs(3));
        assert_eq!((reset.up_bps, reset.down_bps, reset.down_total), (0, 0, 10));
        let after = meter.sample(110, 60, start + Duration::from_secs(4));
        assert_eq!((after.up_bps, after.down_bps), (100, 50));
    }

    #[test]
    fn connect_failures_map_to_codes_and_retryability() {
        let failed = |text: &str| connect_failure(&ServiceError::Failed(text.into()));
        assert_eq!(
            failed("CONNECTION_OWNED_BY_ANOTHER_SESSION").0.code,
            ErrorCode::ServiceBusy
        );
        assert!(failed("CONNECTION_OWNED_BY_ANOTHER_SESSION").1);
        let (info, retryable) = failed("PROFILE_SCHEMA_UNSUPPORTED");
        assert_eq!(
            (info.code, retryable),
            (ErrorCode::ServiceIncompatible, false)
        );
        let (info, retryable) = failed("PROFILE_REVISION_REQUIRED");
        assert_eq!((info.code, retryable), (ErrorCode::ProfileInvalid, false));
        let (info, retryable) = failed("ppvpn-core is not running");
        assert_eq!((info.code, retryable), (ErrorCode::ConnectFailed, true));
        assert_eq!(info.detail, "ppvpn-core is not running");
        let (info, _) = connect_failure(&ServiceError::Unavailable("connect: refused".into()));
        assert_eq!(info.code, ErrorCode::ServiceUnavailable);
    }

    #[test]
    fn health_transaction_only_for_a_backend_on_the_internet() {
        // Whatever backend the build names (#517).
        assert!(health_target("https://www.example.com").is_some());
        assert!(health_target("https://example.com").is_some());
        assert!(health_target("https://staging.example.net/").is_some());
        // A local or test backend.
        assert!(health_target("http://www.example.com").is_none());
        assert!(health_target("http://localhost:8080").is_none());
        assert!(health_target("https://localhost").is_none());
        assert!(health_target("https://api.localhost").is_none());
        assert!(health_target("http://127.0.0.1").is_none());
        assert!(health_target("https://127.0.0.1").is_none());
        assert!(health_target("https://[::1]").is_none());
        assert!(health_target("not a url").is_none());
    }

    #[test]
    fn health_url_is_the_api_health_route_with_a_unique_probe() {
        let first = health_url("https://www.example.com/some/base?x=1").unwrap();
        assert_eq!(first.host_str(), Some("www.example.com"));
        assert_eq!(first.path(), "/api/v1/health");
        assert!(first.query().unwrap().starts_with("ppvpn_probe="));
        let second = health_url("https://www.example.com").unwrap();
        assert_ne!(first.query(), second.query());
        assert!(health_url("http://localhost:8080").is_none());
    }

    #[tokio::test]
    async fn direct_health_request_is_bare_and_uncached() {
        let backend = crate::test_backend::Backend::new();
        let base = crate::test_backend::serve(backend.clone());
        let target = reqwest::Url::parse(&format!("{base}{HEALTH_PATH}?ppvpn_probe=1")).unwrap();
        direct_health_request(target).await.unwrap();
        let heads = backend.health_heads.lock().unwrap().clone();
        let head = heads.first().expect("health route was requested");
        assert!(head.starts_with("get /api/v1/health?ppvpn_probe=1 "));
        assert!(head.contains("cache-control: no-cache"));
        assert!(!head.contains("cookie:"));
        assert!(!head.contains("authorization:"));
    }

    #[tokio::test]
    async fn direct_health_failures_report_the_service_unreachable() {
        // A missing route (the old `/health` answered 404).
        let backend = crate::test_backend::Backend::new();
        let base = crate::test_backend::serve(backend);
        let missing = reqwest::Url::parse(&format!("{base}/health")).unwrap();
        let info = direct_health_request(missing).await.unwrap_err();
        assert_eq!(
            (info.code, info.detail.as_str()),
            (
                ErrorCode::ConnectHealthCheckFailed,
                "HEALTH_DIRECT_STATUS_FAILED: 404"
            )
        );

        // Nothing listening (as with DNS or the route taken over).
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let refused =
            reqwest::Url::parse(&format!("http://127.0.0.1:{port}/api/v1/health")).unwrap();
        let info = direct_health_request(refused).await.unwrap_err();
        assert_eq!(
            (info.code, info.detail.as_str()),
            (
                ErrorCode::ConnectHealthCheckFailed,
                "HEALTH_DIRECT_PATH_FAILED"
            )
        );
        assert_eq!(
            service_unreachable("HEALTH_CAPTURE_PATH_FAILED").code,
            ErrorCode::ConnectHealthCheckFailed
        );
        assert_eq!(
            health_error("HEALTH_ENTRANCE_FAILED").code,
            ErrorCode::ConnectFailed
        );
    }

    /// A TUN core whose `get-traffic` answers `samples` in turn (upload
    /// bytes, `measured_at`), the last one from then on.
    fn scripted_traffic(
        samples: Vec<(u64, Option<SystemTime>)>,
    ) -> Arc<crate::core_ipc::tests::FakeCore> {
        let calls = AtomicUsize::new(0);
        crate::core_ipc::tests::FakeCore::new(move |path, _| {
            assert_eq!(path, "/v1/get-traffic");
            let call = calls.fetch_add(1, Ordering::SeqCst);
            let (up, at) = samples[call.min(samples.len() - 1)];
            (Duration::ZERO, Ok(traffic_json(up, at)))
        })
    }

    fn traffic_json(up: u64, at: Option<SystemTime>) -> Value {
        serde_json::json!({
            "upload_bytes": up,
            "download_bytes": 0,
            "measured_at": at.map(rfc3339),
        })
    }

    /// Now, to the millisecond (what the fakes' RFC 3339 dates keep).
    fn whole_ms_now() -> SystemTime {
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap();
        std::time::UNIX_EPOCH + Duration::from_millis(now.as_millis() as u64)
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[tokio::test]
    async fn capture_check_waits_out_a_stale_first_sample() {
        // Right after connect: the first reads return what the engine read
        // before the check (or the previous run's counters), the same
        // sample twice in a row; then it reads its runtime again.
        let t0 = whole_ms_now();
        let stale = Some(t0 - Duration::from_secs(10));
        let core = scripted_traffic(vec![
            (0, stale),
            (0, stale),
            (100, Some(t0 + ms(1))),
            (100, Some(t0 + ms(1))),
            (150, Some(t0 + ms(10))),
        ]);
        let before = capture_baseline(core.as_ref(), t0, "api").await.unwrap();
        assert_eq!(before.total(), 100);
        capture_verdict(core.as_ref(), before, t0 + ms(5), None, "api")
            .await
            .unwrap();
        // Two reads of the stale sample, two of the fresh baseline, two of
        // the sample read after the request.
        assert_eq!(core.calls.lock().unwrap().len(), 6);
    }

    #[tokio::test]
    async fn capture_check_fails_when_fresh_counters_do_not_grow() {
        let t0 = whole_ms_now();
        let core = scripted_traffic(vec![
            (100, Some(t0 + ms(1))),
            (100, Some(t0 + ms(1))),
            (100, Some(t0 + ms(10))),
            (100, Some(t0 + ms(10))),
            (100, Some(t0 + ms(20))),
        ]);
        let before = capture_baseline(core.as_ref(), t0, "api").await.unwrap();
        let info = capture_verdict(core.as_ref(), before, t0 + ms(5), None, "api")
            .await
            .unwrap_err();
        assert_eq!(
            (info.code, info.detail.as_str()),
            (
                ErrorCode::ConnectHealthCheckFailed,
                "HEALTH_CAPTURE_PATH_FAILED"
            )
        );
        assert!(is_path_cause(&info));
        assert!(!is_traffic_stale(&info));
    }

    #[tokio::test]
    async fn capture_check_reads_once_more_before_calling_a_leak() {
        // The first sample dated after the request may have been read
        // before its last bytes: the next one decides.
        let t0 = whole_ms_now();
        let core = scripted_traffic(vec![
            (100, Some(t0 + ms(1))),
            (100, Some(t0 + ms(1))),
            (100, Some(t0 + ms(6))),
            (100, Some(t0 + ms(6))),
            (180, Some(t0 + ms(20))),
        ]);
        let before = capture_baseline(core.as_ref(), t0, "api").await.unwrap();
        capture_verdict(core.as_ref(), before, t0 + ms(5), None, "api")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn capture_check_without_fresh_counters_is_stale_not_a_leak() {
        let t0 = whole_ms_now();
        let stale = Some(t0 - Duration::from_secs(10));

        // Never read again after the check started.
        let core = scripted_traffic(vec![(0, stale)]);
        let info = capture_baseline(core.as_ref(), t0, "api")
            .await
            .unwrap_err();
        assert_eq!(
            (info.code, info.detail.as_str()),
            (ErrorCode::ConnectFailed, "HEALTH_TRAFFIC_STALE: before")
        );

        // Not read again after the request.
        let core = scripted_traffic(vec![(100, Some(t0 + ms(1)))]);
        let before = capture_baseline(core.as_ref(), t0, "api").await.unwrap();
        let info = capture_verdict(core.as_ref(), before, t0 + ms(5), None, "api")
            .await
            .unwrap_err();
        assert_eq!(
            (info.code, info.detail.as_str()),
            (ErrorCode::ConnectFailed, "HEALTH_TRAFFIC_STALE: after")
        );
        // Neither a path failure nor a leak for the health loop; the
        // connect's one repeat still covers it.
        assert!(!is_path_cause(&info));
        assert!(!info.detail.contains("HEALTH_CAPTURE_PATH_FAILED"));
        assert!(is_traffic_stale(&info));

        // An engine that has not read its runtime yet dates its zero
        // counters with the time of each call: never the same sample twice.
        let core = crate::core_ipc::tests::FakeCore::new(|_, _| {
            (
                Duration::ZERO,
                Ok(traffic_json(0, Some(SystemTime::now() + ms(1)))),
            )
        });
        let before = CoreTraffic {
            upload_bytes: 0,
            download_bytes: 0,
            measured_at: Some(t0),
        };
        let info = capture_verdict(core.as_ref(), before, t0, None, "api")
            .await
            .unwrap_err();
        assert_eq!(info.detail, "HEALTH_TRAFFIC_STALE: after");
    }

    #[test]
    fn rfc3339_logs_utc_to_the_millisecond() {
        let days = crate::logging::days_from_civil(2026, 10, 7) as u64;
        let at = std::time::UNIX_EPOCH
            + Duration::from_secs(days * 86_400 + 8 * 3_600 + 5 * 60 + 9)
            + Duration::from_micros(42_900);
        assert_eq!(rfc3339(at), "2026-10-07T08:05:09.042Z");
        assert_eq!(measured(None), "unknown");
    }

    #[test]
    fn persisted_v1_state_without_optional_fields_is_read() {
        let persisted: Persisted =
            serde_json::from_str(r#"{"generation":4,"desired":"connected"}"#).unwrap();
        assert_eq!(persisted.generation, 4);
        assert_eq!(persisted.desired, Desired::Connected);
        assert_eq!(persisted.session_id, None);
        let encoded = serde_json::to_value(Persisted::of(&Machine {
            generation: 3,
            session_id: Some("s".into()),
            revision: Some("r".into()),
            desired_on: true,
            ..Machine::default()
        }))
        .unwrap();
        assert_eq!(
            encoded,
            serde_json::json!({"generation":3,"desired":"connected","sessionId":"s","profileRevision":"r"})
        );
    }

    // --- controller with fakes --------------------------------------------

    #[derive(Default)]
    struct FakeService {
        available: AtomicBool,
        connect_error: Mutex<Option<String>>,
        update_error: Mutex<Option<String>>,
        /// Reject a specific requested mode, leaving other updates valid.
        reject_routing: Mutex<Option<RoutingMode>>,
        /// Hold the next profile update after recording its requested mode.
        update_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
        /// Another session owns the core (for restore tests).
        foreign: Mutex<Option<SessionRef>>,
        owner: Mutex<Option<(SessionRef, String)>>,
        disconnects: AtomicUsize,
        selected: Mutex<Option<String>>,
        /// `/v1/pin-ingress` bodies, in order.
        pins: Mutex<Vec<(String, Option<String>)>>,
        /// The foreign session belongs to another OS user.
        foreign_other_user: AtomicBool,
        auth_resets: AtomicUsize,
        /// `connect` calls (each asks the service to start the TUN core).
        connects: AtomicUsize,
        /// What `get_version` / `get_status` report as the build id.
        build_id: Mutex<Option<String>>,
        /// The build id a (re)install leaves behind.
        installed_build_id: Mutex<Option<String>>,
        /// `Disconnect` times out on the client side; `true`: the service
        /// still stops the core (late), `false`: it never does.
        disconnect_times_out: Mutex<Option<bool>>,
        /// The routing mode of every `connect` / `update_profile`, in order.
        routing_modes: Mutex<Vec<RoutingMode>>,
        /// The selection and pins of every `connect` / `update_profile`, in
        /// order.
        choices: Mutex<Vec<core_ipc::ApplyChoices>>,
        /// Every core API call first waits this long (a slow health check).
        core_api_delay: Mutex<Option<Duration>>,
        /// This many entrance probes fail before they succeed again.
        entrance_failures: AtomicUsize,
        /// `GetStatus.nodes`, once set (core 0.5.7+).
        status_nodes: Mutex<Option<Value>>,
        /// `tun_routing` in GetStatus (None: not reported). A connect starts
        /// a fresh core, which installs the routing again: back to None.
        tun_routing: Mutex<Option<String>>,
        /// Fields merged into GetStatus (e.g. a degraded state, reasons).
        status_extra: Mutex<Option<Value>>,
        /// `Disconnect` answers only after this long (a slow core stop).
        disconnect_delay: Mutex<Option<Duration>>,
        /// `RenewLease` fails with this service error.
        renew_error: Mutex<Option<String>>,
        /// When each `connect` call arrived.
        connect_times: Mutex<Vec<Instant>>,
        /// The service answers `Watch` (a service that predates it does not).
        watch_supported: AtomicBool,
        /// How the watch of each generation ends, once set.
        watch_ends: Mutex<std::collections::HashMap<u64, WatchEnd>>,
        /// Watches opened, and watches dropped by the client (cancelled).
        watches: AtomicUsize,
        watches_dropped: Arc<AtomicUsize>,
        /// The service refuses the client (its signature), and whether the
        /// one a (re)install leaves behind does.
        rejects: AtomicBool,
        rejects_after_install: AtomicBool,
    }

    impl FakeService {
        fn check(&self) -> Result<(), ServiceError> {
            if self.available.load(Ordering::SeqCst) && self.rejects.load(Ordering::SeqCst) {
                Err(ServiceError::Rejected(
                    "CLIENT_SIGNATURE_REJECTED:test".into(),
                ))
            } else if self.available.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err(ServiceError::Unavailable(
                    "connect: No such file or directory".into(),
                ))
            }
        }

        fn status(&self) -> ServiceStatus {
            ServiceStatus {
                service_build_id: self.build_id.lock().unwrap().clone(),
                ..self.session_status()
            }
        }

        fn session_status(&self) -> ServiceStatus {
            let owner = self.owner.lock().unwrap().clone();
            let foreign = self.foreign.lock().unwrap().clone();
            match (owner, foreign) {
                (Some((session, revision)), _) => ServiceStatus {
                    running: true,
                    session_id: Some(session.session_id),
                    generation: session.generation,
                    profile_revision: Some(revision),
                    ..ServiceStatus::default()
                },
                // Session details only for the owner's user.
                (None, Some(_)) if self.foreign_other_user.load(Ordering::SeqCst) => {
                    ServiceStatus {
                        running: true,
                        ..ServiceStatus::default()
                    }
                }
                (None, Some(session)) => ServiceStatus {
                    running: true,
                    session_id: Some(session.session_id),
                    generation: session.generation,
                    ..ServiceStatus::default()
                },
                _ => ServiceStatus::default(),
            }
        }
    }

    impl FakeService {
        /// The core applies the selection with the profile.
        fn applied(&self, choices: &core_ipc::ApplyChoices) {
            self.choices.lock().unwrap().push(choices.clone());
            if let Some(node_id) = &choices.selected_node_id {
                *self.selected.lock().unwrap() = Some(node_id.clone());
            }
        }
    }

    impl ServiceApi for FakeService {
        fn get_version(&self) -> BoxFuture<'_, Result<VersionInfo, ServiceError>> {
            Box::pin(async move {
                self.check()?;
                Ok(VersionInfo {
                    service: "PPVPN Service".into(),
                    version: "1.0.0".into(),
                    build_id: self.build_id.lock().unwrap().clone(),
                    ..VersionInfo::default()
                })
            })
        }

        fn get_status(&self) -> BoxFuture<'_, Result<ServiceStatus, ServiceError>> {
            Box::pin(async move {
                self.check()?;
                Ok(self.status())
            })
        }

        fn connect<'a>(
            &'a self,
            session: &'a SessionRef,
            profile: &'a Value,
            take_over: bool,
            routing_mode: RoutingMode,
            choices: &'a core_ipc::ApplyChoices,
        ) -> BoxFuture<'a, Result<u32, ServiceError>> {
            Box::pin(async move {
                self.connects.fetch_add(1, Ordering::SeqCst);
                self.routing_modes.lock().unwrap().push(routing_mode);
                self.connect_times.lock().unwrap().push(Instant::now());
                self.check()?;
                if let Some(error) = self.connect_error.lock().unwrap().clone() {
                    return Err(ServiceError::Failed(error));
                }
                // Mirrors service/src/core.rs `ownership`.
                let foreign = self.foreign.lock().unwrap().clone();
                if foreign.is_some_and(|owner| &owner != session) {
                    if self.foreign_other_user.load(Ordering::SeqCst) {
                        return Err(ServiceError::Failed(
                            "CONNECTION_OWNED_BY_ANOTHER_USER".into(),
                        ));
                    }
                    if !take_over {
                        return Err(ServiceError::Failed(
                            "CONNECTION_OWNED_BY_ANOTHER_SESSION".into(),
                        ));
                    }
                    *self.foreign.lock().unwrap() = None;
                }
                let revision = profile["revision"].as_str().unwrap_or_default().to_string();
                *self.owner.lock().unwrap() = Some((session.clone(), revision));
                *self.tun_routing.lock().unwrap() = None;
                self.applied(choices);
                Ok(4242)
            })
        }

        fn update_profile<'a>(
            &'a self,
            session: &'a SessionRef,
            profile: &'a Value,
            routing_mode: RoutingMode,
            choices: &'a core_ipc::ApplyChoices,
        ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
            Box::pin(async move {
                self.check()?;
                self.routing_modes.lock().unwrap().push(routing_mode);
                let gate = self.update_gate.lock().unwrap().take();
                if let Some(gate) = gate {
                    gate.notified().await;
                }
                if let Some(error) = self.update_error.lock().unwrap().clone() {
                    return Err(ServiceError::Failed(error));
                }
                if *self.reject_routing.lock().unwrap() == Some(routing_mode) {
                    return Err(ServiceError::Failed(
                        "PROFILE_UPDATE_ROLLED_BACK: invalid routing mode".into(),
                    ));
                }
                let revision = profile["revision"].as_str().unwrap_or_default().to_string();
                *self.owner.lock().unwrap() = Some((session.clone(), revision));
                self.applied(choices);
                Ok(self.status())
            })
        }

        fn renew_lease<'a>(
            &'a self,
            session: &'a SessionRef,
        ) -> BoxFuture<'a, Result<ServiceStatus, ServiceError>> {
            Box::pin(async move {
                self.check()?;
                if let Some(error) = self.renew_error.lock().unwrap().clone() {
                    return Err(ServiceError::Failed(error));
                }
                let owned = self
                    .owner
                    .lock()
                    .unwrap()
                    .as_ref()
                    .is_some_and(|(owner, _)| owner == session);
                if owned {
                    return Ok(self.status());
                }
                // Mirrors service/src/core.rs `access`: user first, then session.
                if self.foreign.lock().unwrap().is_some()
                    && self.foreign_other_user.load(Ordering::SeqCst)
                {
                    return Err(ServiceError::Failed(
                        "CONNECTION_OWNED_BY_ANOTHER_USER".into(),
                    ));
                }
                Err(ServiceError::Failed("STALE_OR_FOREIGN_SESSION".into()))
            })
        }

        fn disconnect<'a>(
            &'a self,
            session: &'a SessionRef,
        ) -> BoxFuture<'a, Result<(), ServiceError>> {
            Box::pin(async move {
                self.disconnects.fetch_add(1, Ordering::SeqCst);
                let delay = *self.disconnect_delay.lock().unwrap();
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                self.check()?;
                if let Some(stops_late) = *self.disconnect_times_out.lock().unwrap() {
                    if stops_late {
                        *self.owner.lock().unwrap() = None;
                    }
                    return Err(ServiceError::Unavailable("service call timed out".into()));
                }
                let mut owner = self.owner.lock().unwrap();
                if owner.as_ref().is_some_and(|(owned, _)| owned == session) {
                    *owner = None;
                    Ok(())
                } else {
                    Err(ServiceError::Failed("CONNECTION_NOT_ACTIVE".into()))
                }
            })
        }

        fn core_api<'a>(
            &'a self,
            session: &'a SessionRef,
            path: &'a str,
            body: Value,
            _: Duration,
        ) -> BoxFuture<'a, Result<Value, ServiceError>> {
            Box::pin(async move {
                let delay = *self.core_api_delay.lock().unwrap();
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                let owner = self.owner.lock().unwrap().clone();
                let Some((owned, revision)) = owner.filter(|(owned, _)| owned == session) else {
                    return Err(ServiceError::Failed("STALE_OR_FOREIGN_SESSION".into()));
                };
                let _ = owned;
                match path {
                    "/v1/get-status" => {
                        let mut status = serde_json::json!({
                            "state": "running", "revision": revision, "selected_node_id": "n1", "node_count": 2
                        });
                        if let Some(nodes) = self.status_nodes.lock().unwrap().clone() {
                            status["nodes"] = nodes;
                        }
                        if let Some(routing) = self.tun_routing.lock().unwrap().clone() {
                            status["tun_routing"] = routing.into();
                        }
                        if let Some(Value::Object(extra)) =
                            self.status_extra.lock().unwrap().clone()
                        {
                            for (key, value) in extra {
                                status[key] = value;
                            }
                        }
                        Ok(status)
                    }
                    "/v1/probe-entrances" => {
                        // One failure used up per probe (a compare-exchange loop: fetch_update is
                        // deprecated on newer toolchains, try_update missing on older ones).
                        let mut left = self.entrance_failures.load(Ordering::SeqCst);
                        let failing = loop {
                            if left == 0 {
                                break false;
                            }
                            match self.entrance_failures.compare_exchange(
                                left,
                                left - 1,
                                Ordering::SeqCst,
                                Ordering::SeqCst,
                            ) {
                                Ok(_) => break true,
                                Err(now) => left = now,
                            }
                        };
                        Ok(serde_json::json!([
                            {"node_id": "n1", "success": !failing, "latency_ms": 20, "endpoint_key": "k1"}
                        ]))
                    }
                    "/v1/pin-ingress" => {
                        self.pins.lock().unwrap().push((
                            body["node_id"].as_str().unwrap_or_default().to_string(),
                            body["endpoint_key"].as_str().map(str::to_string),
                        ));
                        Ok(serde_json::json!({}))
                    }
                    "/v1/select-node" => {
                        let node = body["node_id"].as_str().unwrap_or_default().to_string();
                        if node == "missing" {
                            return Err(ServiceError::Failed(
                                "NODE_NOT_FOUND: node not found".into(),
                            ));
                        }
                        *self.selected.lock().unwrap() = Some(node);
                        Ok(serde_json::json!({}))
                    }
                    "/v1/get-traffic" => {
                        Ok(serde_json::json!({"upload_bytes": 10, "download_bytes": 20}))
                    }
                    other => Err(ServiceError::Failed(format!("API_NOT_FOUND: {other}"))),
                }
            })
        }

        fn watch<'a>(&'a self, session: &'a SessionRef) -> BoxFuture<'a, WatchEnd> {
            struct Dropped(Arc<AtomicUsize>);
            impl Drop for Dropped {
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }
            Box::pin(async move {
                if !self.watch_supported.load(Ordering::SeqCst) {
                    return WatchEnd::Unsupported;
                }
                self.watches.fetch_add(1, Ordering::SeqCst);
                let dropped = Dropped(self.watches_dropped.clone());
                loop {
                    let end = self.watch_ends.lock().unwrap().remove(&session.generation);
                    if let Some(end) = end {
                        std::mem::forget(dropped);
                        return end;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
        }

        fn reset_auth(&self) {
            self.auth_resets.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct FakeHooks {
        installed: AtomicBool,
        install_result: Mutex<Result<(), PlatformError>>,
        installs: AtomicUsize,
        service: Arc<FakeService>,
        /// How long the uninstall hook takes (admin prompt, service stop).
        uninstall_delay: Mutex<Option<Duration>>,
        uninstalls: AtomicUsize,
    }

    impl PlatformHooks for FakeHooks {
        fn credential_load(&self) -> Result<Option<Vec<u8>>, PlatformError> {
            Ok(None)
        }
        fn credential_save(&self, _: Vec<u8>) -> Result<(), PlatformError> {
            Ok(())
        }
        fn credential_delete(&self) -> Result<(), PlatformError> {
            Ok(())
        }
        fn open_url(&self, _: String) -> bool {
            true
        }
        fn privileged_service_installed(&self) -> bool {
            self.installed.load(Ordering::SeqCst)
        }
        fn install_privileged_service(&self) -> Result<(), PlatformError> {
            self.installs.fetch_add(1, Ordering::SeqCst);
            let result = match &*self.install_result.lock().unwrap() {
                Ok(()) => Ok(()),
                Err(PlatformError::Cancelled) => Err(PlatformError::Cancelled),
                Err(PlatformError::Failed { message }) => Err(PlatformError::Failed {
                    message: message.clone(),
                }),
                Err(PlatformError::Locked { message }) => Err(PlatformError::Locked {
                    message: message.clone(),
                }),
            };
            if result.is_ok() {
                self.installed.store(true, Ordering::SeqCst);
                self.service.available.store(true, Ordering::SeqCst);
                *self.service.build_id.lock().unwrap() =
                    self.service.installed_build_id.lock().unwrap().clone();
                self.service.rejects.store(
                    self.service.rejects_after_install.load(Ordering::SeqCst),
                    Ordering::SeqCst,
                );
            }
            result
        }
        fn uninstall_privileged_service(&self) -> Result<(), PlatformError> {
            let delay = *self.uninstall_delay.lock().unwrap();
            if let Some(delay) = delay {
                std::thread::sleep(delay);
            }
            self.uninstalls.fetch_add(1, Ordering::SeqCst);
            self.installed.store(false, Ordering::SeqCst);
            self.service.available.store(false, Ordering::SeqCst);
            *self.service.owner.lock().unwrap() = None;
            Ok(())
        }
    }

    struct Harness {
        enhanced: Enhanced,
        service: Arc<FakeService>,
        hooks: Arc<FakeHooks>,
        phases: Arc<Mutex<Vec<ConnectionPhase>>>,
        /// The client's selected node ([`EnhancedConfig::selection`]).
        selection: Arc<Mutex<Option<String>>>,
        dir: PathBuf,
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn harness(installed: bool) -> Harness {
        harness_expecting(installed, Some(BUILD))
    }

    fn harness_expecting(installed: bool, expected_build: Option<&str>) -> Harness {
        harness_with(installed, expected_build, true)
    }

    fn harness_with(
        installed: bool,
        expected_build: Option<&str>,
        reinstall_rejecting_service: bool,
    ) -> Harness {
        let service = Arc::new(FakeService::default());
        service.available.store(installed, Ordering::SeqCst);
        *service.build_id.lock().unwrap() = Some(BUILD.into());
        *service.installed_build_id.lock().unwrap() = Some(BUILD.into());
        let hooks = Arc::new(FakeHooks {
            installed: AtomicBool::new(installed),
            install_result: Mutex::new(Ok(())),
            installs: AtomicUsize::new(0),
            service: service.clone(),
            uninstall_delay: Mutex::new(None),
            uninstalls: AtomicUsize::new(0),
        });
        let dir =
            std::env::temp_dir().join(format!("ppvpn-enh-test-{}", uuid::Uuid::new_v4().simple()));
        let phases = Arc::new(Mutex::new(Vec::new()));
        let sink = phases.clone();
        let selection: Arc<Mutex<Option<String>>> = Arc::default();
        let config = EnhancedConfig {
            api_base: "http://localhost:8080".into(),
            state_file: dir.join(crate::storage::ENHANCED_STATE_FILE),
            expected_service_build_id: expected_build.map(String::from),
            reinstall_rejecting_service,
            routing: Default::default(),
            ingress_pins: Default::default(),
            selection: crate::session::SelectionSource::new({
                let selection = selection.clone();
                move || selection.lock().unwrap().clone()
            }),
        };
        let enhanced = Enhanced::new(
            config,
            service.clone(),
            hooks.clone(),
            None,
            Arc::new(move |state: EnhancedState| {
                let mut phases = sink.lock().unwrap();
                if phases.last() != Some(&state.phase) {
                    phases.push(state.phase);
                }
            }),
            Arc::new(|_| {}),
        );
        Harness {
            enhanced,
            service,
            hooks,
            phases,
            selection,
            dir,
        }
    }

    /// The build id the harness's client expects.
    const BUILD: &str = "build-current";

    const PROFILE_R1: &[u8] = br#"{"revision":"r1","nodes":[]}"#;
    const PROFILE_R2: &[u8] = br#"{"revision":"r2","nodes":[]}"#;

    fn persisted(h: &Harness) -> Persisted {
        serde_json::from_slice(
            &std::fs::read(h.dir.join(crate::storage::ENHANCED_STATE_FILE)).unwrap(),
        )
        .unwrap()
    }

    fn report(
        competitors: &[&str],
        route: Option<&str>,
        fake_ip_dns: &[&str],
    ) -> crate::detect::ConflictReport {
        crate::detect::ConflictReport {
            competitors: competitors.iter().map(|name| name.to_string()).collect(),
            observed: competitors.iter().map(|name| name.to_string()).collect(),
            foreign_default_route: route.map(str::to_string),
            fake_ip_dns: fake_ip_dns.iter().map(|ip| ip.to_string()).collect(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_tunnel_owning_the_network_keeps_tun_from_starting() {
        let h = harness(true);
        // mihomo's TUN holds the split-default routes.
        h.enhanced
            .set_detector(Arc::new(|| report(&["Mihomo"], Some("Meta"), &[])));
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        let error = h.enhanced.enable().await.unwrap_err();
        assert!(matches!(
            error,
            ClientError::Failed {
                code: ErrorCode::NetworkPathContended,
                ..
            }
        ));
        // The service was never asked to start the core.
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 0);
        assert!(h.service.owner.lock().unwrap().is_none());
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::Error);
        let reason = state.reason.unwrap();
        assert_eq!(reason.code, ErrorCode::NetworkPathContended);
        assert_eq!(reason.detail, "PREFLIGHT_CONFLICT: Mihomo");
        assert!(state.retryable);
        assert_eq!(state.competitors, vec!["Mihomo"]);

        // Fake-IP DNS alone blocks too.
        h.enhanced
            .set_detector(Arc::new(|| report(&["Surge"], None, &["198.18.0.2"])));
        h.enhanced.retry().await.unwrap_err();
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 0);
        assert_eq!(h.enhanced.state().competitors, vec!["Surge"]);

        // The user quit it: retry connects, and the competitors are gone.
        h.enhanced
            .set_detector(Arc::new(crate::detect::ConflictReport::default));
        h.enhanced.retry().await.unwrap();
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::On);
        assert!(state.competitors.is_empty());
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_mesh_vpn_or_a_weak_signal_does_not_block() {
        let h = harness(true);
        // Tailscale running without the default route: not a competitor.
        h.enhanced
            .set_detector(Arc::new(|| crate::detect::ConflictReport {
                observed: vec!["Tailscale".into()],
                ..Default::default()
            }));
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        h.enhanced.disable().await.unwrap();

        // A named app with only a fake-IP interface (no route, no DNS).
        h.enhanced
            .set_detector(Arc::new(|| crate::detect::ConflictReport {
                fake_ip_interfaces: vec!["utun5 (198.18.0.1)".into()],
                ..report(&["Mihomo"], None, &[])
            }));
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_slow_conflict_check_is_skipped() {
        let h = harness(true);
        h.enhanced.set_detector(Arc::new(|| {
            std::thread::sleep(PREFLIGHT_BUDGET * 3);
            report(&["Mihomo"], Some("Meta"), &[])
        }));
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        let started = std::time::Instant::now();
        h.enhanced.enable().await.unwrap();
        assert!(started.elapsed() < PREFLIGHT_BUDGET * 2);
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1);
    }

    /// Waits until the service saw more than `n` connects. Recovery runs on
    /// timers: a fixed sleep before asserting it flaked on loaded runners.
    async fn wait_connects_above(h: &Harness, n: usize, what: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while h.service.connects.load(Ordering::SeqCst) <= n {
            assert!(std::time::Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Waits until the controller reaches `phase`.
    async fn wait_phase(h: &Harness, phase: ConnectionPhase) -> EnhancedState {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = h.enhanced.state();
            if state.phase == phase {
                return state;
            }
            assert!(std::time::Instant::now() < deadline, "stuck in {state:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    // --- health checks across disconnect / reconnect ---------------------------

    /// Captures WARN and above on this thread (the tests' runtime is
    /// current-thread, so spawned loops log here too).
    fn capture_warnings() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
        #[derive(Clone)]
        struct Sink(Arc<Mutex<Vec<u8>>>);
        impl std::io::Write for Sink {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let buffer = Arc::new(Mutex::new(Vec::new()));
        let sink = Sink(buffer.clone());
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .with_writer(move || sink.clone())
            .finish();
        (buffer, tracing::subscriber::set_default(subscriber))
    }

    fn captured(buffer: &Arc<Mutex<Vec<u8>>>) -> String {
        String::from_utf8_lossy(&buffer.lock().unwrap()).into_owned()
    }

    // Exercise the public client action as well as the enhanced controller:
    // the standard core may already have accepted a mode that the service rejects.
    struct RoutingHarness {
        client: Arc<crate::Client>,
        service: Arc<FakeService>,
        launcher: Arc<crate::cores::test_support::FakeLauncher>,
        dir: String,
    }

    impl Drop for RoutingHarness {
        fn drop(&mut self) {
            self.client.shutdown();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn routing_client() -> RoutingHarness {
        struct Listener;
        impl crate::ClientListener for Listener {
            fn on_snapshot(&self, _: crate::ClientSnapshot) {}
            fn on_probe_result(&self, _: crate::ProbeResult) {}
            fn on_traffic(&self, _: crate::TrafficSample) {}
        }
        let backend = crate::test_backend::Backend::new();
        *backend.profile.lock().unwrap() =
            Some(include_str!("../../account/tests/fixtures/proxy-profile.json").into());
        let base = crate::test_backend::serve(backend);
        let dir = crate::test_backend::temp_dir("ppvpn-routing-client");
        let service = Arc::new(FakeService::default());
        service.available.store(true, Ordering::SeqCst);
        let launcher = crate::cores::test_support::FakeLauncher::new();
        let client = crate::Client::with_parts(
            crate::ClientConfig {
                api_base: base,
                data_dir: dir.clone(),
                log_dir: dir.clone(),
                platform: "macos".into(),
                app_version: "0.0.0-test".into(),
            },
            crate::auth::test_support::MemoryPlatform::with_blob(
                r#"{"version":1,"active_refresh":"jyr_old"}"#,
            ),
            Arc::new(Listener),
            service.clone(),
            launcher.clone(),
            launcher.writer.clone(),
        );
        crate::test_backend::wait_until(|| {
            matches!(
                client.snapshot().standard,
                crate::StandardState::Ready { .. }
            )
        });
        client.runtime.block_on(client.connect_now()).unwrap();
        assert_eq!(client.snapshot().connection.phase, ConnectionPhase::On);
        RoutingHarness {
            client,
            service,
            launcher,
            dir,
        }
    }

    #[test]
    fn routing_failure_restores_client_and_standard_mode_and_allows_retry() {
        let h = routing_client();
        *h.service.reject_routing.lock().unwrap() = Some(RoutingMode::Global);
        h.client
            .runtime
            .block_on(h.client.set_routing_mode(RoutingMode::Global))
            .unwrap_err();
        assert_eq!(h.client.snapshot().routing_mode, RoutingMode::Rules);
        assert_eq!(h.client.routing.get(), RoutingMode::Rules);
        assert_eq!(
            crate::connection::load_routing_mode(&h.dir),
            RoutingMode::Rules
        );
        let standard_modes: Vec<_> = h
            .launcher
            .core
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/v1/apply-profile")
            .map(|(_, body)| body["routing_mode"].clone())
            .collect();
        assert_eq!(
            standard_modes,
            vec![
                serde_json::json!("rules"),
                serde_json::json!("global"),
                serde_json::json!("rules")
            ]
        );
        assert_eq!(h.client.snapshot().connection.phase, ConnectionPhase::On);
        *h.service.reject_routing.lock().unwrap() = None;
        h.client
            .runtime
            .block_on(h.client.set_routing_mode(RoutingMode::Global))
            .unwrap();
        assert_eq!(h.client.snapshot().routing_mode, RoutingMode::Global);
        assert_eq!(
            crate::connection::load_routing_mode(&h.dir),
            RoutingMode::Global
        );
        assert_eq!(
            h.service.routing_modes.lock().unwrap().last(),
            Some(&RoutingMode::Global)
        );
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1, "no reconnect");
    }

    #[test]
    fn routing_standard_failure_does_not_change_the_tunnel() {
        let h = routing_client();
        h.launcher.fail_apply.store(true, Ordering::SeqCst);
        h.client
            .runtime
            .block_on(h.client.set_routing_mode(RoutingMode::Global))
            .unwrap_err();
        assert_eq!(h.client.routing.get(), RoutingMode::Rules);
        assert_eq!(h.client.snapshot().routing_mode, RoutingMode::Rules);
        assert_eq!(
            crate::connection::load_routing_mode(&h.dir),
            RoutingMode::Rules
        );
        assert_eq!(
            *h.service.routing_modes.lock().unwrap(),
            [RoutingMode::Rules, RoutingMode::Rules]
        );
        h.launcher.fail_apply.store(false, Ordering::SeqCst);
        h.client
            .runtime
            .block_on(h.client.set_routing_mode(RoutingMode::Global))
            .unwrap();
        assert_eq!(
            *h.service.routing_modes.lock().unwrap(),
            [RoutingMode::Rules, RoutingMode::Rules, RoutingMode::Global]
        );
    }

    #[test]
    fn routing_save_failure_restores_both_running_cores() {
        let h = routing_client();
        // A directory at the destination makes atomic rename fail on every OS.
        let settings = PathBuf::from(&h.dir).join(crate::storage::CLIENT_SETTINGS_FILE);
        std::fs::create_dir(&settings).unwrap();
        let error = h
            .client
            .runtime
            .block_on(h.client.set_routing_mode(RoutingMode::Global))
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Failed {
                code: ErrorCode::LocalStorageFailed,
                ..
            }
        ));
        assert_eq!(h.client.routing.get(), RoutingMode::Rules);
        assert_eq!(h.client.snapshot().routing_mode, RoutingMode::Rules);
        assert_eq!(
            *h.service.routing_modes.lock().unwrap(),
            [RoutingMode::Rules, RoutingMode::Global, RoutingMode::Rules]
        );
        assert_eq!(h.client.snapshot().connection.phase, ConnectionPhase::On);
        std::fs::remove_dir(settings).unwrap();
        h.client
            .runtime
            .block_on(h.client.set_routing_mode(RoutingMode::Global))
            .unwrap();
        assert_eq!(
            crate::connection::load_routing_mode(&h.dir),
            RoutingMode::Global
        );
        // A confirmed mode is still a genuine no-op.
        let updates = h.service.routing_modes.lock().unwrap().len();
        h.client
            .runtime
            .block_on(h.client.set_routing_mode(RoutingMode::Global))
            .unwrap();
        assert_eq!(h.service.routing_modes.lock().unwrap().len(), updates);
    }

    #[test]
    fn routing_failed_compensation_stops_the_mismatched_cores() {
        let h = routing_client();
        *h.launcher.reject_routing.lock().unwrap() = Some(RoutingMode::Rules);
        *h.service.reject_routing.lock().unwrap() = Some(RoutingMode::Global);
        h.client
            .runtime
            .block_on(h.client.set_routing_mode(RoutingMode::Global))
            .unwrap_err();
        assert_eq!(h.client.routing.get(), RoutingMode::Rules);
        assert_eq!(h.client.snapshot().routing_mode, RoutingMode::Rules);
        assert_eq!(h.client.snapshot().connection.phase, ConnectionPhase::Off);
        assert!(
            h.client.standard.transport().is_err(),
            "standard core must not keep routing globally"
        );
        assert!(h.service.owner.lock().unwrap().is_none());
    }

    #[test]
    fn routing_fatal_tunnel_failure_is_not_mistaken_for_a_noop_rollback() {
        let h = routing_client();
        *h.service.update_error.lock().unwrap() =
            Some("PROFILE_UPDATE_AND_ROLLBACK_FAILED: runtime unavailable".into());
        h.client
            .runtime
            .block_on(h.client.set_routing_mode(RoutingMode::Global))
            .unwrap_err();
        assert_eq!(h.client.snapshot().routing_mode, RoutingMode::Rules);
        assert_eq!(h.client.snapshot().connection.phase, ConnectionPhase::Off);
        assert!(h.service.owner.lock().unwrap().is_none());
    }

    #[test]
    fn routing_waits_for_acceptance_and_survives_caller_cancellation() {
        let h = routing_client();
        let gate = Arc::new(tokio::sync::Notify::new());
        *h.service.update_gate.lock().unwrap() = Some(gate.clone());
        let client = h.client.clone();
        let request = h
            .client
            .runtime
            .spawn(async move { client.set_routing_mode(RoutingMode::Global).await });
        crate::test_backend::wait_until(|| h.service.routing_modes.lock().unwrap().len() == 2);
        assert_eq!(
            h.client.snapshot().routing_mode,
            RoutingMode::Rules,
            "unconfirmed mode is not published"
        );
        assert_eq!(
            crate::connection::load_routing_mode(&h.dir),
            RoutingMode::Rules,
            "unconfirmed mode is not saved"
        );
        request.abort();
        assert!(h
            .client
            .runtime
            .block_on(request)
            .unwrap_err()
            .is_cancelled());
        gate.notify_one();
        crate::test_backend::wait_until(|| h.client.snapshot().routing_mode == RoutingMode::Global);
        assert_eq!(
            crate::connection::load_routing_mode(&h.dir),
            RoutingMode::Global
        );
        assert_eq!(h.client.routing.get(), RoutingMode::Global);
    }

    #[test]
    fn routing_reverse_switch_restores_global_and_can_retry_rules() {
        let h = routing_client();
        h.client
            .runtime
            .block_on(h.client.set_routing_mode(RoutingMode::Global))
            .unwrap();
        *h.service.reject_routing.lock().unwrap() = Some(RoutingMode::Rules);
        h.client
            .runtime
            .block_on(h.client.set_routing_mode(RoutingMode::Rules))
            .unwrap_err();
        assert_eq!(h.client.snapshot().routing_mode, RoutingMode::Global);
        assert_eq!(h.client.routing.get(), RoutingMode::Global);
        assert_eq!(
            crate::connection::load_routing_mode(&h.dir),
            RoutingMode::Global
        );
        assert_eq!(h.client.snapshot().connection.phase, ConnectionPhase::On);
        *h.service.reject_routing.lock().unwrap() = None;
        h.client
            .runtime
            .block_on(h.client.set_routing_mode(RoutingMode::Rules))
            .unwrap();
        assert_eq!(h.client.snapshot().routing_mode, RoutingMode::Rules);
        assert_eq!(
            crate::connection::load_routing_mode(&h.dir),
            RoutingMode::Rules
        );
    }

    #[test]
    fn routing_tunnel_compensation_failure_stops_the_connection() {
        let h = routing_client();
        *h.service.update_error.lock().unwrap() =
            Some("PROFILE_UPDATE_ROLLED_BACK: all updates unavailable".into());
        h.client
            .runtime
            .block_on(h.client.set_routing_mode(RoutingMode::Global))
            .unwrap_err();
        assert_eq!(h.client.snapshot().routing_mode, RoutingMode::Rules);
        assert_eq!(h.client.snapshot().connection.phase, ConnectionPhase::Off);
        assert!(h.service.owner.lock().unwrap().is_none());
        assert!(h.client.standard.transport().is_err());
    }

    #[test]
    fn routing_lost_standard_reply_forces_the_old_mode_back_to_the_core() {
        let h = routing_client();
        *h.launcher.lose_apply_reply.lock().unwrap() = Some(RoutingMode::Global);
        h.client
            .runtime
            .block_on(h.client.set_routing_mode(RoutingMode::Global))
            .unwrap_err();
        let modes: Vec<_> = h
            .launcher
            .core
            .calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/v1/apply-profile")
            .map(|(_, body)| body["routing_mode"].clone())
            .collect();
        assert_eq!(
            modes,
            vec![
                serde_json::json!("rules"),
                serde_json::json!("global"),
                serde_json::json!("rules")
            ]
        );
        assert_eq!(h.client.snapshot().routing_mode, RoutingMode::Rules);
        assert_eq!(h.client.routing.get(), RoutingMode::Rules);
        assert_eq!(h.client.snapshot().connection.phase, ConnectionPhase::On);
    }

    #[tokio::test]
    async fn a_new_routing_mode_reaches_the_running_core_without_a_reconnect() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        // Off: nothing to push.
        h.enhanced.reapply().await.unwrap();
        h.enhanced.enable().await.unwrap();
        h.enhanced.inner.config.routing.set(RoutingMode::Global);
        let before = h.phases.lock().unwrap().len();
        h.enhanced.reapply().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        assert_eq!(
            h.phases.lock().unwrap().len(),
            before,
            "stayed On throughout"
        );
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1, "no reconnect");
        assert_eq!(
            *h.service.routing_modes.lock().unwrap(),
            vec![RoutingMode::Rules, RoutingMode::Global]
        );
    }

    fn nodes_with_k2(healthy: Option<bool>) -> Value {
        serde_json::json!([{"node_id": "n1", "pinned_endpoint_key": "k2", "ingresses": [
            {"endpoint_key": "k1", "role": "primary", "healthy": true, "active": false},
            {"endpoint_key": "k2", "role": "backup", "healthy": healthy, "active": true},
        ]}])
    }

    #[tokio::test]
    async fn a_pinned_line_that_is_down_keeps_the_connection_on() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        h.enhanced
            .inner
            .config
            .ingress_pins
            .set([("n1".to_string(), "k2".to_string())].into());
        // The pinned line goes down: every check fails on the path.
        *h.service.status_nodes.lock().unwrap() = Some(nodes_with_k2(Some(false)));
        h.service
            .entrance_failures
            .store(usize::MAX, Ordering::SeqCst);
        h.enhanced.set_health_interval(Duration::from_millis(20));
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On, "stays on");
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1, "no reconnect");

        // The core has no verdict yet: one round's grace, then recovery.
        *h.service.status_nodes.lock().unwrap() = Some(nodes_with_k2(None));
        wait_connects_above(&h, 1, "recovered after the grace round").await;
    }

    #[tokio::test]
    async fn a_network_change_brings_the_health_check_forward() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        h.enhanced.set_health_interval(Duration::from_secs(60));
        // The path breaks: nothing notices before the (long) interval ends.
        h.service
            .entrance_failures
            .store(usize::MAX, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1);

        // Short rounds from the change on: past the window, a path failure
        // still takes its recheck and one more round (#438).
        h.enhanced.set_health_interval(Duration::from_millis(20));
        h.enhanced.network_changed();
        // Checked at once, then re-checked while the network settles
        // (NETWORK_GRACE); still failing after that, recovery reconnects.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            h.service.connects.load(Ordering::SeqCst),
            1,
            "core kept while settling"
        );
        wait_connects_above(&h, 1, "recovery reconnected once the window passed").await;
    }

    #[tokio::test]
    async fn a_path_that_comes_back_after_a_network_change_keeps_the_core() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        h.enhanced.set_health_interval(Duration::from_secs(60));
        // The interface goes down: checks fail for a while, then pass again.
        h.service.entrance_failures.store(3, Ordering::SeqCst);
        // Let the health loop start waiting (a change before that is moot).
        tokio::time::sleep(Duration::from_millis(50)).await;
        h.enhanced.network_changed();
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        assert_eq!(
            h.service.connects.load(Ordering::SeqCst),
            1,
            "core kept, no reconnect"
        );
        assert_eq!(
            h.service.entrance_failures.load(Ordering::SeqCst),
            0,
            "rechecked until it passed"
        );
    }

    #[tokio::test]
    async fn broken_tun_routing_reconnects_even_while_the_network_settles() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        h.enhanced.set_health_interval(Duration::from_secs(60));
        // "unguarded" is not a reason to restart.
        *h.service.tun_routing.lock().unwrap() = Some("unguarded".into());
        tokio::time::sleep(Duration::from_millis(50)).await;
        h.enhanced.network_changed();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1);
        // Rules gone for good: traffic bypasses the TUN, no settling grace.
        *h.service.tun_routing.lock().unwrap() = Some("broken".into());
        // A change while a check is still running wakes nobody: repeat it
        // until the loop is waiting again.
        for _ in 0..40 {
            if h.service.connects.load(Ordering::SeqCst) > 1 {
                break;
            }
            h.enhanced.network_changed();
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        assert!(
            h.service.connects.load(Ordering::SeqCst) > 1,
            "a fresh core was started at once"
        );
    }

    #[tokio::test]
    async fn a_degraded_core_that_heals_itself_stays_on() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        h.enhanced.set_health_interval(Duration::from_millis(20));
        // As engine-host reports it: running, the reasons kept.
        *h.service.status_extra.lock().unwrap() = Some(serde_json::json!({
            "state": "running", "tun_routing": "unguarded",
            "reasons": [{"kind": "tun_routing_unguarded"}, {"kind": "no_default_interface"}]
        }));
        tokio::time::sleep(Duration::from_millis(200)).await;
        // As the engine itself names it.
        *h.service.status_extra.lock().unwrap() = Some(serde_json::json!({
            "state": "degraded",
            "reasons": [{"kind": "ingress_unavailable", "node_id": "n1"}]
        }));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1, "no reconnect");

        // Broken routing in a degraded state is the one that reconnects.
        *h.service.status_extra.lock().unwrap() = Some(serde_json::json!({
            "state": "degraded",
            "reasons": [{"kind": "tun_routing_broken", "missing": ["utun4: route"]}]
        }));
        wait_connects_above(&h, 1, "reconnected for broken routing").await;
    }

    /// A clock for the TUN-routing budget that the test moves by hand.
    fn manual_clock(h: &Harness) -> Arc<Mutex<Duration>> {
        let start = Instant::now();
        let offset = Arc::new(Mutex::new(Duration::ZERO));
        h.enhanced.set_clock(Arc::new({
            let offset = offset.clone();
            move || start + *offset.lock().unwrap()
        }));
        offset
    }

    /// Another program changes the TUN's routing; waits until the client
    /// reconnected (its `connects`-th connect) and is on again.
    async fn routing_taken_and_reconnected(h: &Harness, connects: usize) {
        *h.service.tun_routing.lock().unwrap() = Some("broken".into());
        wait_connects_above(h, connects - 1, "reconnected for broken routing").await;
        wait_phase(h, ConnectionPhase::On).await;
    }

    /// Another program changes the TUN's routing once more than the budget
    /// allows: no reconnect, `Contended` saying so, the session released.
    async fn routing_taken_once_too_often(h: &Harness, connects: usize) {
        *h.service.tun_routing.lock().unwrap() = Some("broken".into());
        let state = wait_phase(h, ConnectionPhase::Contended).await;
        let reason = state.reason.unwrap();
        assert_eq!(reason.code, ErrorCode::NetworkPathContended);
        assert_eq!(reason.detail, TUN_ROUTING_TAKEN_OVER);
        assert!(state.retryable);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            h.service.connects.load(Ordering::SeqCst),
            connects,
            "no reconnect past the budget"
        );
        assert!(
            h.service.owner.lock().unwrap().is_none(),
            "session released"
        );
        assert!(h.enhanced.inner.with_data(|d| d.loops.is_empty()));
    }

    #[tokio::test]
    async fn broken_tun_routing_reconnects_three_times_in_ten_minutes() {
        let h = harness(true);
        let clock = manual_clock(&h);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        h.enhanced.set_health_interval(Duration::from_millis(20));
        for connects in 2..=4 {
            routing_taken_and_reconnected(&h, connects).await;
            *clock.lock().unwrap() += Duration::from_secs(3 * 60);
        }
        // Reconnected at 0, 3 and 6 minutes: none at 9.
        routing_taken_once_too_often(&h, 4).await;
    }

    #[tokio::test]
    async fn the_tun_routing_budget_frees_up_with_time_and_on_retry() {
        let h = harness(true);
        let clock = manual_clock(&h);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        h.enhanced.set_health_interval(Duration::from_millis(20));
        for connects in 2..=4 {
            routing_taken_and_reconnected(&h, connects).await;
        }
        // Ten minutes on, the three are out of the window.
        *clock.lock().unwrap() += TUN_ROUTING_WINDOW;
        for connects in 5..=7 {
            routing_taken_and_reconnected(&h, connects).await;
        }
        routing_taken_once_too_often(&h, 7).await;

        // The user retries: connected, and a full budget again.
        h.enhanced.retry().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        for connects in 9..=11 {
            routing_taken_and_reconnected(&h, connects).await;
        }
        routing_taken_once_too_often(&h, 11).await;

        // So does connecting again.
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        routing_taken_and_reconnected(&h, 13).await;
    }

    #[tokio::test]
    async fn a_network_change_while_off_does_nothing() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.network_changed();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::Off);
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_pinned_line_that_is_up_does_not_hide_another_failure() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        h.enhanced
            .inner
            .config
            .ingress_pins
            .set([("n1".to_string(), "k2".to_string())].into());
        *h.service.status_nodes.lock().unwrap() = Some(nodes_with_k2(Some(true)));
        h.service
            .entrance_failures
            .store(usize::MAX, Ordering::SeqCst);
        h.enhanced.set_health_interval(Duration::from_millis(20));
        wait_connects_above(&h, 1, "recovered").await;
    }

    /// A probe lost now and then on a lossy link (#438): the entrance probe
    /// fails in one round out of four, for many rounds. The core stays,
    /// with no reconnect and no notice.
    #[tokio::test]
    async fn periodic_probe_loss_keeps_one_core() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        h.enhanced.set_health_interval(Duration::from_millis(20));
        for _ in 0..12 {
            h.service.entrance_failures.store(1, Ordering::SeqCst);
            // The failing round, its recheck and two passing rounds.
            while h.service.entrance_failures.load(Ordering::SeqCst) > 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        let state = h.enhanced.state();
        assert_eq!((state.phase, state.reason), (ConnectionPhase::On, None));
        assert_eq!(
            h.service.connects.load(Ordering::SeqCst),
            1,
            "one core throughout"
        );
    }

    /// A path failure that lasts: the round, its recheck and the next round
    /// fail, then exactly one recovery reconnects.
    #[tokio::test]
    async fn a_lasting_path_failure_recovers_after_the_recheck_and_the_next_round() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        h.enhanced.set_health_interval(Duration::from_millis(20));
        // Two failures (a round and its recheck): no recovery.
        h.service.entrance_failures.store(2, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            h.service.connects.load(Ordering::SeqCst),
            1,
            "two failures are not enough"
        );
        // Three in a row: recovered, once.
        h.service.entrance_failures.store(3, Ordering::SeqCst);
        wait_connects_above(&h, 1, "recovered after three failures").await;
        wait_phase(&h, ConnectionPhase::On).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 2, "one recovery");
    }

    /// Only the API host is unreachable on the direct path, the tunnel's
    /// steps pass: the core stays, `On` says so, and the notice goes once a
    /// direct request passes again (#438).
    #[tokio::test]
    async fn a_direct_only_failure_stays_on_with_a_notice() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        let direct_down = Arc::new(AtomicBool::new(true));
        let checks = Arc::new(AtomicUsize::new(0));
        h.enhanced.set_official_steps(Arc::new({
            let direct_down = direct_down.clone();
            let checks = checks.clone();
            move || {
                checks.fetch_add(1, Ordering::SeqCst);
                let direct = if direct_down.load(Ordering::SeqCst) {
                    Err(service_unreachable("HEALTH_DIRECT_PATH_FAILED"))
                } else {
                    Ok(())
                };
                (direct, Ok(()))
            }
        }));
        h.enhanced.set_health_interval(Duration::from_millis(20));
        let state = wait_phase_reason(&h, |state| state.reason.is_some()).await;
        assert_eq!(state.phase, ConnectionPhase::On);
        let reason = state.reason.unwrap();
        assert_eq!(
            (reason.code, reason.detail.as_str()),
            (ErrorCode::ConnectHealthCheckFailed, API_UNREACHABLE)
        );
        // Backing off: 20, 40, 80, 160, 320 ms between checks.
        let before = checks.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(600)).await;
        let during = checks.load(Ordering::SeqCst) - before;
        assert!(during <= 5, "backed off ({during} checks in 600 ms)");
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1, "no restart");

        direct_down.store(false, Ordering::SeqCst);
        let state = wait_phase_reason(&h, |state| state.reason.is_none()).await;
        assert_eq!(state.phase, ConnectionPhase::On);
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1);
    }

    /// The proxied path failing is the tunnel's failure, whatever the
    /// direct request did: recovered from as a path failure.
    #[tokio::test]
    async fn a_failing_proxied_path_still_recovers() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        h.enhanced.set_official_steps(Arc::new(|| {
            (
                Err(service_unreachable("HEALTH_DIRECT_PATH_FAILED")),
                Err(health_error("HEALTH_PROXY_PATH_FAILED")),
            )
        }));
        h.enhanced.set_health_interval(Duration::from_millis(20));
        wait_connects_above(&h, 1, "recovered").await;
    }

    #[test]
    fn official_verdicts() {
        let direct = || Err(service_unreachable("HEALTH_DIRECT_PATH_FAILED"));
        let proxied = || Err(health_error("HEALTH_PROXY_PATH_FAILED"));
        assert_eq!(official_verdict(Ok(()), Ok(())), Ok(()));
        let only_direct = official_verdict(direct(), Ok(())).unwrap_err();
        assert!(is_api_unreachable(&only_direct), "{only_direct:?}");
        assert_eq!(
            only_direct.detail,
            format!("{API_UNREACHABLE}: HEALTH_DIRECT_PATH_FAILED")
        );
        assert_eq!(official_verdict(direct(), proxied()), direct());
        assert_eq!(official_verdict(Ok(()), proxied()), proxied());
        assert!(!is_api_unreachable(&direct().unwrap_err()));
        assert!(is_leak(&service_unreachable(
            "HEALTH_CAPTURE_PATH_FAILED: x"
        )));
        assert!(is_leak(&service_unreachable(TUN_ROUTING_BROKEN)));
        assert!(!is_leak(&only_direct));
    }

    /// Waits until the controller's state matches `done`.
    async fn wait_phase_reason(
        h: &Harness,
        done: impl Fn(&EnhancedState) -> bool,
    ) -> EnhancedState {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let state = h.enhanced.state();
            if done(&state) {
                return state;
            }
            assert!(std::time::Instant::now() < deadline, "stuck in {state:?}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn a_failed_first_health_check_is_repeated_once() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        // The platform is still settling: the first check fails.
        h.service.entrance_failures.store(1, Ordering::SeqCst);
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1);

        // Failing twice is a failure.
        h.enhanced.disable().await.unwrap();
        h.service.entrance_failures.store(2, Ordering::SeqCst);
        assert!(h.enhanced.enable().await.is_err());
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::Error);
    }

    #[tokio::test]
    async fn disconnect_cancels_a_health_check_in_flight() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        let (logs, _guard) = capture_warnings();
        // The next check takes 300 ms and, once the session is gone, fails
        // (`HEALTH_STATUS_FAILED: STALE_OR_FOREIGN_SESSION`).
        *h.service.core_api_delay.lock().unwrap() = Some(Duration::from_millis(300));
        h.enhanced.set_health_interval(Duration::from_millis(20));
        tokio::time::sleep(Duration::from_millis(80)).await;

        h.enhanced.disable().await.unwrap();
        assert!(h.enhanced.inner.with_data(|d| d.loops.is_empty()));
        tokio::time::sleep(Duration::from_millis(500)).await;

        let logs = captured(&logs);
        assert!(!logs.contains("health check failed"), "{logs}");
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::Off);
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1, "no recovery");
        assert_eq!(h.enhanced.inner.with_data(|d| d.machine.generation), 1);
    }

    #[tokio::test]
    async fn a_stale_generation_health_result_is_ignored_after_a_reconnect() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        let (logs, _guard) = capture_warnings();
        *h.service.core_api_delay.lock().unwrap() = Some(Duration::from_millis(150));
        h.enhanced.set_health_interval(Duration::from_millis(20));
        tokio::time::sleep(Duration::from_millis(60)).await;

        // Generation 1's check is in flight across the reconnect.
        h.enhanced.disable().await.unwrap();
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.enhanced.inner.with_data(|d| d.machine.generation), 2);
        let before = h.enhanced.inner.with_data(|d| d.recovery);

        // A result of generation 1 arriving now: a failure recovers nothing,
        // a success does not count towards generation 2's stable checks.
        assert!(!h
            .enhanced
            .inner
            .on_health_result(1, Err(service_unreachable("HEALTH_DIRECT_PATH_FAILED"))));
        assert!(!h.enhanced.inner.on_health_result(1, Ok(())));
        assert_eq!(h.enhanced.inner.with_data(|d| d.recovery), before);
        tokio::time::sleep(Duration::from_millis(400)).await;

        let logs = captured(&logs);
        assert!(!logs.contains("health check failed"), "{logs}");
        let state = h.enhanced.state();
        assert_eq!((state.phase, state.reason), (ConnectionPhase::On, None));
        assert_eq!(h.enhanced.inner.with_data(|d| d.machine.generation), 2);
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 2, "no recovery");
    }

    // --- uninstall, service stop ------------------------------------------

    #[tokio::test]
    async fn uninstall_while_connected_finishes_quickly() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        // The service takes long to stop the core before it answers.
        *h.service.disconnect_delay.lock().unwrap() = Some(Duration::from_secs(10));

        let started = Instant::now();
        h.enhanced.uninstall_service().await.unwrap();
        let took = started.elapsed();
        assert!(took < Duration::from_secs(2), "took {took:?}");
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::Off);
        assert!(!state.service_installed);
        assert_eq!(h.hooks.uninstalls.load(Ordering::SeqCst), 1);
        // One Disconnect, no confirmation polling, no loops left behind.
        assert_eq!(h.service.disconnects.load(Ordering::SeqCst), 1);
        assert!(h.enhanced.inner.with_data(|d| d.loops.is_empty()));
        assert_eq!(persisted(&h).desired, Desired::Disconnected);
    }

    #[tokio::test]
    async fn an_uninstall_in_progress_does_not_hold_up_disable_or_a_reconnect() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        *h.hooks.uninstall_delay.lock().unwrap() = Some(Duration::from_millis(1_500));
        let uninstall = tokio::spawn({
            let enhanced = h.enhanced.clone();
            async move { enhanced.uninstall_service().await }
        });
        wait_phase(&h, ConnectionPhase::Off).await;
        tokio::time::sleep(Duration::from_millis(100)).await;

        // What switching to compatible mode and connecting there calls.
        let started = Instant::now();
        h.enhanced.disable().await.unwrap();
        *h.selection.lock().unwrap() = Some("n2".into());
        h.enhanced.select_node("n2").await.unwrap();
        let took = started.elapsed();
        assert!(took < Duration::from_millis(300), "waited {took:?}");
        assert!(!uninstall.is_finished());
        uninstall.await.unwrap().unwrap();

        // Enhanced again right after: straight to the install prompt.
        let started = Instant::now();
        h.enhanced.enable().await.unwrap();
        let took = started.elapsed();
        assert!(took < Duration::from_secs(1), "took {took:?}");
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        assert_eq!(h.service.selected.lock().unwrap().as_deref(), Some("n2"));
    }

    #[test]
    fn service_backoff_doubles_up_to_the_cap() {
        let delays: Vec<Duration> = (0..SERVICE_BACKOFF_ATTEMPTS)
            .map(|attempt| service_backoff(attempt).unwrap())
            .collect();
        let base = SERVICE_BACKOFF_BASE;
        assert_eq!(&delays[..4], &[base, base * 2, base * 4, base * 8]);
        assert!(delays.iter().all(|delay| *delay <= SERVICE_BACKOFF_MAX));
        assert_eq!(delays.last(), Some(&SERVICE_BACKOFF_MAX));
        assert_eq!(service_backoff(SERVICE_BACKOFF_ATTEMPTS), None);
        assert!(is_service_down(&ClientErrorInfo::new(
            ErrorCode::ServiceUnavailable,
            "connect /run/ppvpn/service.sock: Connection refused"
        )));
        assert!(!is_service_down(&ClientErrorInfo::new(
            ErrorCode::ServiceUnavailable,
            "SERVICE_NOT_INSTALLED"
        )));
        let (info, retryable) = connect_failure(&ServiceError::Failed("SERVICE_STOPPING".into()));
        assert_eq!(
            (info.code, retryable),
            (ErrorCode::ServiceUnavailable, true)
        );
        assert!(is_service_down(&info));
    }

    #[tokio::test]
    async fn a_stopping_service_is_reconnected_with_growing_pauses() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1);
        // The service shuts down: renewals and connects answer SERVICE_STOPPING.
        *h.service.connect_error.lock().unwrap() = Some("SERVICE_STOPPING".into());
        *h.service.renew_error.lock().unwrap() = Some("SERVICE_STOPPING".into());
        let failed_at = Instant::now();

        // Three refused attempts, then the (restarted) service accepts.
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.service.connects.load(Ordering::SeqCst) < 4 {
            assert!(Instant::now() < deadline, "no reconnect attempts");
            let phase = h.enhanced.state().phase;
            assert_ne!(phase, ConnectionPhase::Error, "gave up too early");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        *h.service.connect_error.lock().unwrap() = None;
        *h.service.renew_error.lock().unwrap() = None;
        let state = wait_phase(&h, ConnectionPhase::On).await;
        assert_eq!(state.reason, None);

        let times = h.service.connect_times.lock().unwrap().clone();
        let attempts = &times[1..];
        // Never right away: the first attempt waits a pause too.
        assert!(attempts[0] - failed_at >= SERVICE_BACKOFF_BASE);
        for (index, pair) in attempts.windows(2).enumerate() {
            let gap = pair[1] - pair[0];
            let expected = service_backoff(index as u32 + 1).unwrap();
            assert!(gap >= expected, "attempt {index}: {gap:?} < {expected:?}");
        }
        let phases = h.phases.lock().unwrap().clone();
        let after_on = &phases[phases
            .iter()
            .position(|p| *p == ConnectionPhase::On)
            .unwrap()..];
        assert!(
            !after_on.contains(&ConnectionPhase::Error),
            "no Error flashes while retrying: {after_on:?}"
        );
    }

    #[tokio::test]
    async fn a_disconnect_during_the_pause_cancels_the_reconnect() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        *h.service.renew_error.lock().unwrap() = Some("SERVICE_STOPPING".into());
        wait_phase(&h, ConnectionPhase::Reconnecting).await;
        // The reconnect keeps failing until the error is cleared below, so a
        // disable that waited for it would never return. A generous bound:
        // a wall-clock threshold near the pause flaked on loaded runners.
        tokio::time::timeout(Duration::from_secs(5), h.enhanced.disable())
            .await
            .expect("disable waited for the reconnect")
            .unwrap();
        *h.service.renew_error.lock().unwrap() = None;
        tokio::time::sleep(SERVICE_BACKOFF_BASE * 4).await;
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::Off);
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1, "no reconnect");
    }

    // --- service watch ------------------------------------------------------

    /// Enables with a service that answers `Watch`; returns once generation
    /// 1's watch is open.
    async fn on_with_watch(h: &Harness) {
        h.service.watch_supported.store(true, Ordering::SeqCst);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while h.service.watches.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "no watch opened");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        // Releasing the lost session takes a moment, so `Reconnecting` (and
        // its reason) lasts long enough to be observed.
        *h.service.disconnect_delay.lock().unwrap() = Some(Duration::from_millis(100));
    }

    fn end_watch(h: &Harness, generation: u64, end: WatchEnd) {
        h.service.watch_ends.lock().unwrap().insert(generation, end);
    }

    #[tokio::test]
    async fn a_broken_watch_leaves_on_at_once_and_reconnects_after_the_pause() {
        let h = harness(true);
        on_with_watch(&h).await;
        // `systemctl stop`: the service is gone, its connection hits EOF.
        // Lease renewals still succeed here: only the watch can tell.
        let lost_at = Instant::now();
        end_watch(
            &h,
            1,
            WatchEnd::Lost("the service closed the watch connection".into()),
        );
        let state = wait_phase(&h, ConnectionPhase::Reconnecting).await;
        let took = lost_at.elapsed();
        assert!(took < Duration::from_millis(200), "left On after {took:?}");
        let reason = state.reason.unwrap();
        assert_eq!(reason.code, ErrorCode::ServiceUnavailable);
        assert!(
            reason.detail.starts_with("SERVICE_WATCH_LOST"),
            "{}",
            reason.detail
        );

        // The service is back: reconnected after the service-down pause, and
        // the new generation is watched again.
        let state = wait_phase(&h, ConnectionPhase::On).await;
        assert_eq!(state.reason, None);
        let times = h.service.connect_times.lock().unwrap().clone();
        assert_eq!(times.len(), 2);
        assert!(times[1] - lost_at >= SERVICE_BACKOFF_BASE, "no pause");
        let deadline = Instant::now() + Duration::from_secs(2);
        while h.service.watches.load(Ordering::SeqCst) < 2 {
            assert!(Instant::now() < deadline, "generation 2 not watched");
            tokio::time::sleep(Duration::from_millis(2)).await;
        }

        // Disconnecting ends the watch.
        h.enhanced.disable().await.unwrap();
        tokio::task::yield_now().await;
        assert_eq!(h.service.watches_dropped.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn watch_events_map_to_the_recovery_path() {
        // Stopping: reconnect after the pause (#53), no Error flashes.
        let h = harness(true);
        on_with_watch(&h).await;
        let stopped_at = Instant::now();
        end_watch(&h, 1, WatchEnd::Stopping);
        let state = wait_phase(&h, ConnectionPhase::Reconnecting).await;
        assert!(stopped_at.elapsed() < Duration::from_millis(200));
        assert_eq!(
            state.reason.map(|reason| (reason.code, reason.detail)),
            Some((
                ErrorCode::ServiceUnavailable,
                "SERVICE_STOPPING".to_string()
            ))
        );
        wait_phase(&h, ConnectionPhase::On).await;
        let times = h.service.connect_times.lock().unwrap().clone();
        assert!(times[1] - stopped_at >= SERVICE_BACKOFF_BASE);

        // The core crashed while the service runs: reconnect right away.
        let h = harness(true);
        on_with_watch(&h).await;
        end_watch(&h, 1, WatchEnd::CoreStopped("exited".into()));
        let state = wait_phase(&h, ConnectionPhase::Reconnecting).await;
        assert_eq!(
            state.reason.map(|reason| (reason.code, reason.detail)),
            Some((ErrorCode::ConnectFailed, "CORE_STOPPED: exited".to_string()))
        );
        wait_phase(&h, ConnectionPhase::On).await;
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 2);
        // It dies again before the episode ended (no two healthy checks
        // yet): still reconnected, after the service backoff.
        let crashed_at = Instant::now();
        end_watch(&h, 2, WatchEnd::CoreStopped("exited".into()));
        wait_phase(&h, ConnectionPhase::Reconnecting).await;
        wait_phase(&h, ConnectionPhase::On).await;
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 3);
        assert!(crashed_at.elapsed() >= SERVICE_BACKOFF_BASE);

        // Taken over by another session: in use elsewhere, no reconnect.
        let h = harness(true);
        on_with_watch(&h).await;
        end_watch(&h, 1, WatchEnd::CoreStopped("taken_over".into()));
        let state = wait_phase(&h, ConnectionPhase::Contended).await;
        assert_eq!(state.reason.unwrap().code, ErrorCode::ServiceBusy);
        assert!(state.retryable);
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_service_without_watch_or_a_silent_watch_falls_back_to_renewal() {
        for end in [
            None,
            Some(WatchEnd::Abandoned("no keepalive for 16000 ms".into())),
        ] {
            let h = harness(true);
            h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
            if let Some(end) = end {
                h.service.watch_supported.store(true, Ordering::SeqCst);
                end_watch(&h, 1, end);
            }
            h.enhanced.enable().await.unwrap();
            // Many renewal periods (50 ms in tests): still on.
            tokio::time::sleep(Duration::from_millis(300)).await;
            let state = h.enhanced.state();
            assert_eq!((state.phase, state.reason), (ConnectionPhase::On, None));
            assert_eq!(h.service.connects.load(Ordering::SeqCst), 1);
            // Renewal alone still notices a stopping service.
            *h.service.renew_error.lock().unwrap() = Some("SERVICE_STOPPING".into());
            wait_phase(&h, ConnectionPhase::Reconnecting).await;
            *h.service.renew_error.lock().unwrap() = None;
            wait_phase(&h, ConnectionPhase::On).await;
        }
    }

    #[tokio::test]
    async fn a_stale_generations_watch_result_is_ignored() {
        let h = harness(true);
        on_with_watch(&h).await;
        h.enhanced.disable().await.unwrap();
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.enhanced.inner.with_data(|d| d.machine.generation), 2);
        // Generation 1's watch reporting now (as if its task had survived).
        end_watch(&h, 1, WatchEnd::Lost("eof".into()));
        watch_loop(
            h.enhanced.inner.clone(),
            SessionRef {
                session_id: "old".into(),
                generation: 1,
            },
        )
        .await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let state = h.enhanced.state();
        assert_eq!((state.phase, state.reason), (ConnectionPhase::On, None));
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 2, "no recovery");
    }

    #[test]
    fn watch_refusals_map_to_verdicts() {
        let verdict =
            |error: &str| watch_verdict(WatchEnd::Refused(ServiceError::Failed(error.into())));
        assert!(
            matches!(verdict("SERVICE_STOPPING"), WatchVerdict::Recover(info) if is_service_down(&info))
        );
        assert!(matches!(
            verdict("CONNECTION_LEASE_EXPIRED"),
            WatchVerdict::Recover(info) if info.code == ErrorCode::ConnectFailed
        ));
        assert!(matches!(
            verdict("STALE_OR_FOREIGN_SESSION"),
            WatchVerdict::TakenOver(_)
        ));
        assert!(matches!(
            verdict("WATCH_LIMIT_REACHED"),
            WatchVerdict::Fallback
        ));
        assert!(matches!(
            verdict("AUTH_SESSION_INVALID"),
            WatchVerdict::Fallback
        ));
        assert!(matches!(
            watch_verdict(WatchEnd::CoreStopped("service_stopping".into())),
            WatchVerdict::Recover(info) if is_service_down(&info)
        ));
        assert!(matches!(
            watch_verdict(WatchEnd::Lost("eof".into())),
            WatchVerdict::Recover(info) if is_service_down(&info)
        ));
    }

    #[tokio::test]
    async fn a_service_restart_is_not_a_contended_path() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        // The service restarts and its core dies: the lease renewal cannot
        // reach it, the one reconnect cannot either.
        h.service.available.store(false, Ordering::SeqCst);
        *h.service.owner.lock().unwrap() = None;
        let state = wait_phase(&h, ConnectionPhase::Error).await;
        let reason = state.reason.unwrap();
        assert_eq!(reason.code, ErrorCode::ServiceUnavailable);
        assert!(state.retryable);
        assert!(h
            .phases
            .lock()
            .unwrap()
            .contains(&ConnectionPhase::Reconnecting));
        // Back up: retry connects.
        h.service.available.store(true, Ordering::SeqCst);
        h.enhanced.retry().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
    }

    #[tokio::test]
    async fn only_path_failures_end_contended() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        let generation = h.enhanced.inner.with_data(|d| d.machine.generation);
        // The capture path broke and, on reconnect, another tunnel holds the
        // route: contended, with the conflict kept.
        h.enhanced
            .set_detector(Arc::new(|| report(&["Mihomo"], Some("Meta"), &[])));
        h.enhanced
            .inner
            .clone()
            .recover(
                generation,
                service_unreachable("HEALTH_CAPTURE_PATH_FAILED"),
            )
            .await;
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::Contended);
        let reason = state.reason.unwrap();
        assert_eq!(reason.code, ErrorCode::NetworkPathContended);
        assert!(reason.detail.starts_with("PREFLIGHT_CONFLICT"));
        assert_eq!(state.competitors, vec!["Mihomo"]);

        // The core stopped answering, and the episode's reconnect is used:
        // still reconnected, after the service backoff (it used to end in
        // Error with the core left running).
        h.enhanced
            .set_detector(Arc::new(crate::detect::ConflictReport::default));
        h.enhanced.retry().await.unwrap();
        let generation = h.enhanced.inner.with_data(|d| {
            d.recovery.claim();
            d.machine.generation
        });
        h.enhanced
            .inner
            .clone()
            .recover(generation, health_error("HEALTH_STATUS_FAILED: EOF"))
            .await;
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);

        // The same with a path failure and nobody named as owning the path:
        // reconnected, not contended.
        h.enhanced.retry().await.unwrap();
        let generation = h.enhanced.inner.with_data(|d| {
            d.recovery.claim();
            d.machine.generation
        });
        h.enhanced
            .inner
            .clone()
            .recover(generation, health_error("HEALTH_ENTRANCE_FAILED"))
            .await;
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);

        // A path that stays broken: the attempts run out and the phase
        // leaves Reconnecting.
        h.service
            .entrance_failures
            .store(usize::MAX, Ordering::SeqCst);
        let generation = h.enhanced.inner.with_data(|d| {
            d.recovery.claim();
            d.machine.generation
        });
        h.enhanced
            .inner
            .clone()
            .recover(generation, health_error("HEALTH_ENTRANCE_FAILED"))
            .await;
        let state = h.enhanced.state();
        assert!(
            matches!(
                state.phase,
                ConnectionPhase::Error | ConnectionPhase::Contended
            ),
            "{:?}",
            state.phase
        );
        assert!(h.service.owner.lock().unwrap().is_none(), "core released");
    }

    #[tokio::test]
    async fn enable_connects_through_an_installed_service() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        *h.selection.lock().unwrap() = Some("n1".into());
        h.enhanced.enable().await.unwrap();
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::On);
        assert!(state.service_installed);
        assert_eq!(state.reason, None);
        assert_eq!(
            *h.phases.lock().unwrap(),
            vec![
                ConnectionPhase::Preparing,
                ConnectionPhase::Connecting,
                ConnectionPhase::On
            ]
        );
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 0);
        assert_eq!(h.service.selected.lock().unwrap().as_deref(), Some("n1"));
        let saved = persisted(&h);
        assert_eq!((saved.generation, saved.desired), (1, Desired::Connected));

        // Idempotent while on.
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.enhanced.inner.with_data(|d| d.machine.generation), 1);

        h.enhanced.disable().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::Off);
        assert!(h
            .phases
            .lock()
            .unwrap()
            .ends_with(&[ConnectionPhase::Disconnecting, ConnectionPhase::Off]));
        assert!(h.service.owner.lock().unwrap().is_none());
        assert_eq!(persisted(&h).desired, Desired::Disconnected);
        assert_eq!(persisted(&h).generation, 1);
    }

    // --- outdated service ----------------------------------------------------

    fn report_build(h: &Harness, now: Option<&str>, after_install: Option<&str>) {
        *h.service.build_id.lock().unwrap() = now.map(String::from);
        *h.service.installed_build_id.lock().unwrap() = after_install.map(String::from);
    }

    async fn reconnect(h: &Harness) {
        h.enhanced.disable().await.unwrap();
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
    }

    #[tokio::test]
    async fn outdated_service_is_reinstalled_before_connect() {
        let h = harness(true);
        report_build(&h, Some("build-old"), Some(BUILD));
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
        assert!(h.service.auth_resets.load(Ordering::SeqCst) >= 1);
        let phases = h.phases.lock().unwrap().clone();
        assert_eq!(
            phases,
            vec![
                ConnectionPhase::Preparing,
                ConnectionPhase::WaitingPermission,
                ConnectionPhase::Preparing,
                ConnectionPhase::Connecting,
                ConnectionPhase::On
            ]
        );
        // Now current: nothing more to do.
        reconnect(&h).await;
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn matching_or_unknown_build_is_not_reinstalled() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 0);

        // Built without service/: the id is unknown, nothing is compared.
        let h = harness_expecting(true, None);
        report_build(&h, None, None);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn service_without_a_build_id_is_outdated() {
        let h = harness(true);
        report_build(&h, None, Some(BUILD));
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
    }

    #[tokio::test]
    async fn reinstall_happens_once_per_session_even_if_the_id_still_differs() {
        let h = harness(true);
        report_build(&h, Some("build-old"), Some("build-other"));
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        reconnect(&h).await;
        h.enhanced.retry().await.unwrap();
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn declined_reinstall_connects_through_the_old_service() {
        let h = harness(true);
        report_build(&h, Some("build-old"), Some(BUILD));
        *h.hooks.install_result.lock().unwrap() = Err(PlatformError::Cancelled);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::On);
        assert_eq!(state.reason, None);
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
        // Not asked again in this session.
        reconnect(&h).await;
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn outdated_service_in_use_is_left_alone() {
        let h = harness(true);
        report_build(&h, Some("build-old"), Some(BUILD));
        *h.service.foreign.lock().unwrap() = Some(SessionRef {
            session_id: uuid::Uuid::new_v4().to_string(),
            generation: 9,
        });
        h.service.foreign_other_user.store(true, Ordering::SeqCst);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        let _ = h.enhanced.enable().await;
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 0);
        // Once it is free, the next connect replaces it.
        *h.service.foreign.lock().unwrap() = None;
        h.enhanced.retry().await.unwrap();
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
    }

    // --- a service that refuses this client (#471) ---------------------------

    fn rejected(error: &ClientError) -> bool {
        matches!(
            error,
            ClientError::Failed {
                code: ErrorCode::ServiceClientRejected,
                ..
            }
        )
    }

    #[tokio::test]
    async fn a_refusing_service_is_reinstalled_before_connect() {
        let h = harness(true);
        h.service.rejects.store(true, Ordering::SeqCst);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
        assert_eq!(
            h.phases.lock().unwrap().clone(),
            vec![
                ConnectionPhase::Preparing,
                ConnectionPhase::WaitingPermission,
                ConnectionPhase::Preparing,
                ConnectionPhase::Connecting,
                ConnectionPhase::On
            ]
        );
        reconnect(&h).await;
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_service_still_refusing_after_the_reinstall_fails_with_the_refusal() {
        let h = harness(true);
        h.service.rejects.store(true, Ordering::SeqCst);
        h.service
            .rejects_after_install
            .store(true, Ordering::SeqCst);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        let started = Instant::now();
        let error = h.enhanced.enable().await.unwrap_err();
        assert!(rejected(&error), "{error:?}");
        // One reinstall, and no wait for a service that answers.
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
        assert!(started.elapsed() < SERVICE_START_TIMEOUT);
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 0);
        // The user's next connect tries once more.
        let _ = h.enhanced.retry().await;
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_declined_reinstall_of_a_refusing_service_fails_the_connect() {
        let h = harness(true);
        h.service.rejects.store(true, Ordering::SeqCst);
        *h.hooks.install_result.lock().unwrap() = Err(PlatformError::Cancelled);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        let error = h.enhanced.enable().await.unwrap_err();
        assert!(
            matches!(
                error,
                ClientError::Failed {
                    code: ErrorCode::ServiceInstallCancelled,
                    ..
                }
            ),
            "{error:?}"
        );
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
        assert_eq!(h.service.connects.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_refusing_service_is_final_where_reinstalling_is_off() {
        let h = harness_with(true, Some(BUILD), false);
        h.service.rejects.store(true, Ordering::SeqCst);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        let error = h.enhanced.enable().await.unwrap_err();
        assert!(rejected(&error), "{error:?}");
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn outdated_service_seen_while_connected_waits_for_the_next_connect() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        // The service changes under a live session (e.g. a package upgrade
        // restarted an old copy): noted by the lease loop, never replaced
        // mid-session.
        report_build(&h, Some("build-old"), Some(BUILD));
        tokio::time::sleep(LEASE_RENEW_INTERVAL * 4).await;
        assert!(h.enhanced.inner.with_data(|d| d.stale_service_noted));
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 0);

        reconnect(&h).await;
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
        assert!(!h.enhanced.inner.with_data(|d| d.stale_service_noted));
    }

    // --- disconnect confirmation ----------------------------------------------

    #[tokio::test]
    async fn unconfirmed_disconnect_is_an_error_not_off() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        *h.service.disconnect_times_out.lock().unwrap() = Some(false);

        let error = h.enhanced.disable().await.unwrap_err();
        assert!(
            matches!(&error, ClientError::Failed { code: ErrorCode::ServiceUnavailable, detail }
                if detail.starts_with(DISCONNECT_UNCONFIRMED)),
            "{error:?}"
        );
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::Error);
        assert!(state.retryable);
        assert!(h.service.owner.lock().unwrap().is_some(), "still running");
        assert_eq!(persisted(&h).desired, Desired::Disconnected);

        // Retry disconnects again (it does not reconnect).
        *h.service.disconnect_times_out.lock().unwrap() = None;
        h.enhanced.retry().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::Off);
        assert!(h.service.owner.lock().unwrap().is_none());
        assert_eq!(h.enhanced.inner.with_data(|d| d.machine.generation), 1);
    }

    #[tokio::test]
    async fn disconnecting_from_a_refusing_service_ends_off_at_once() {
        // A connect the service refused leaves the attempt's session behind;
        // turning off (e.g. switching to compatible mode) must not wait for
        // a confirmation the service will never give (#543).
        let h = harness_with(true, Some(BUILD), false);
        h.service.rejects.store(true, Ordering::SeqCst);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        let error = h.enhanced.enable().await.unwrap_err();
        assert!(rejected(&error), "{error:?}");
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::Error);
        let started = Instant::now();
        h.enhanced.disable().await.unwrap();
        assert!(started.elapsed() < DISCONNECT_CONFIRM_TIMEOUT);
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::Off);
        assert_eq!(persisted(&h).desired, Desired::Disconnected);

        // The same once connected: the app was replaced by a build the
        // service does not trust.
        h.service.rejects.store(false, Ordering::SeqCst);
        h.enhanced.enable().await.unwrap();
        h.service.rejects.store(true, Ordering::SeqCst);
        let started = Instant::now();
        h.enhanced.disable().await.unwrap();
        assert!(started.elapsed() < DISCONNECT_CONFIRM_TIMEOUT);
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::Off);
    }

    #[tokio::test]
    async fn late_disconnect_is_confirmed_through_the_status() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        *h.service.disconnect_times_out.lock().unwrap() = Some(true);
        h.enhanced.disable().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::Off);
    }

    #[tokio::test]
    async fn a_service_that_is_not_running_holds_no_core() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        h.service.available.store(false, Ordering::SeqCst);
        h.enhanced.disable().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::Off);
    }

    #[tokio::test]
    async fn missing_service_is_installed_then_awaited() {
        let h = harness(false);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 1);
        assert!(h.enhanced.state().service_installed);
        let phases = h.phases.lock().unwrap().clone();
        assert!(
            phases.contains(&ConnectionPhase::WaitingPermission),
            "{phases:?}"
        );
        assert_eq!(phases.last(), Some(&ConnectionPhase::On));
    }

    #[tokio::test]
    async fn cancelled_install_is_a_retryable_error() {
        let h = harness(false);
        *h.hooks.install_result.lock().unwrap() = Err(PlatformError::Cancelled);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        let error = h.enhanced.enable().await.unwrap_err();
        assert!(matches!(
            error,
            ClientError::Failed {
                code: ErrorCode::ServiceInstallCancelled,
                ..
            }
        ));
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::Error);
        assert_eq!(
            state.reason.unwrap().code,
            ErrorCode::ServiceInstallCancelled
        );
        assert!(state.retryable);
        assert_eq!(persisted(&h).desired, Desired::Disconnected);
    }

    #[tokio::test]
    async fn service_install_alone_waits_and_does_not_connect() {
        let h = harness(false);
        *h.hooks.install_result.lock().unwrap() = Err(PlatformError::Cancelled);
        assert!(matches!(
            h.enhanced.install_service().await,
            Err(ClientError::Cancelled)
        ));
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::Off);
        assert!(h.enhanced.state().reason.is_none());

        *h.hooks.install_result.lock().unwrap() = Ok(());
        h.enhanced.install_service().await.unwrap();
        let state = h.enhanced.state();
        assert!(state.service_installed);
        assert_eq!(state.phase, ConnectionPhase::Off);
        assert!(h.service.owner.lock().unwrap().is_none());
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 2);

        // Installed and answering: nothing to do.
        h.enhanced.install_service().await.unwrap();
        assert_eq!(h.hooks.installs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn failed_install_is_a_retryable_error() {
        let h = harness(false);
        *h.hooks.install_result.lock().unwrap() = Err(PlatformError::Failed {
            message: "helper missing".into(),
        });
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        assert!(h.enhanced.enable().await.is_err());
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::Error);
        assert!(state.retryable);
        assert_eq!(
            state.reason.unwrap(),
            ClientErrorInfo::new(ErrorCode::ServiceInstallFailed, "helper missing")
        );
    }

    #[tokio::test]
    async fn busy_service_reports_service_busy_and_releases_the_session() {
        let h = harness(true);
        *h.service.connect_error.lock().unwrap() =
            Some("CONNECTION_OWNED_BY_ANOTHER_SESSION".into());
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        assert!(h.enhanced.enable().await.is_err());
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::Contended);
        assert_eq!(state.reason.unwrap().code, ErrorCode::ServiceBusy);
        assert!(state.retryable);
        assert!(state.can_take_over, "same user, another session");
        assert_eq!(h.service.disconnects.load(Ordering::SeqCst), 1);

        // Retry with the contention gone uses a new generation.
        *h.service.connect_error.lock().unwrap() = None;
        h.enhanced.retry().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        assert_eq!(h.enhanced.inner.with_data(|d| d.machine.generation), 2);
    }

    fn foreign_session() -> SessionRef {
        SessionRef {
            session_id: uuid::Uuid::new_v4().to_string(),
            generation: 9,
        }
    }

    #[tokio::test]
    async fn same_user_takes_over_a_contended_connection() {
        let h = harness(true);
        *h.service.foreign.lock().unwrap() = Some(foreign_session());
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();

        assert!(h.enhanced.enable().await.is_err());
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::Contended);
        assert_eq!(state.reason.unwrap().code, ErrorCode::ServiceBusy);
        assert!(state.can_take_over);

        h.enhanced.take_over().await.unwrap();
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::On);
        assert!(!state.can_take_over);
        assert!(h.service.foreign.lock().unwrap().is_none());
        let owner = h.service.owner.lock().unwrap().clone().unwrap().0;
        assert_eq!(owner.generation, 2);

        // Not offered (and refused) once the connection is ours.
        assert!(h.enhanced.take_over().await.is_err());
    }

    #[tokio::test]
    async fn another_users_connection_is_never_offered() {
        let h = harness(true);
        h.service.foreign_other_user.store(true, Ordering::SeqCst);
        *h.service.foreign.lock().unwrap() = Some(foreign_session());
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();

        let error = h.enhanced.enable().await.unwrap_err();
        assert!(matches!(
            error,
            ClientError::Failed {
                code: ErrorCode::ServiceOwnedByAnotherUser,
                ..
            }
        ));
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::Contended);
        assert_eq!(
            state.reason.unwrap().code,
            ErrorCode::ServiceOwnedByAnotherUser
        );
        assert!(!state.retryable);
        assert!(!state.can_take_over);
        assert!(h.enhanced.take_over().await.is_err());
        assert!(h.service.foreign.lock().unwrap().is_some());
    }

    #[tokio::test]
    async fn a_taken_over_device_shows_busy_and_can_take_it_back() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();

        // Device B (same user) takes over: A's next renewal is refused.
        *h.service.owner.lock().unwrap() = None;
        *h.service.foreign.lock().unwrap() = Some(foreign_session());
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.enhanced.state().phase == ConnectionPhase::On {
            assert!(Instant::now() < deadline, "renewal did not notice");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::Contended);
        let reason = state.reason.unwrap();
        assert_eq!(reason.code, ErrorCode::ServiceBusy, "{reason:?}");
        assert!(reason.detail.contains("STALE_OR_FOREIGN_SESSION"));
        assert!(state.can_take_over);
        // No reconnect attempt was made (it would have been refused anyway).
        assert!(h.service.foreign.lock().unwrap().is_some());

        h.enhanced.take_over().await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
    }

    #[tokio::test]
    async fn a_takeover_whose_core_fails_is_a_retryable_error() {
        let h = harness(true);
        *h.service.foreign.lock().unwrap() = Some(foreign_session());
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        assert!(h.enhanced.enable().await.is_err());
        assert!(h.enhanced.state().can_take_over);
        // The service stopped the other session's core, then ours failed.
        *h.service.connect_error.lock().unwrap() = Some("ppvpn-core startup timed out".into());
        assert!(h.enhanced.take_over().await.is_err());
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::Error);
        assert_eq!(state.reason.unwrap().code, ErrorCode::ConnectFailed);
        assert!(state.retryable);
        assert!(!state.can_take_over);
    }

    #[tokio::test]
    async fn a_device_taken_over_by_another_user_is_told_so() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        *h.service.owner.lock().unwrap() = None;
        h.service.foreign_other_user.store(true, Ordering::SeqCst);
        *h.service.foreign.lock().unwrap() = Some(foreign_session());
        let deadline = Instant::now() + Duration::from_secs(5);
        while h.enhanced.state().phase == ConnectionPhase::On {
            assert!(Instant::now() < deadline, "renewal did not notice");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::Contended);
        assert_eq!(
            state.reason.unwrap().code,
            ErrorCode::ServiceOwnedByAnotherUser
        );
        assert!(!state.can_take_over);
    }

    #[tokio::test]
    async fn core_status_only_while_on() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        assert!(h.enhanced.core_status().await.is_none());
        h.enhanced.enable().await.unwrap();
        let status = h.enhanced.core_status().await.unwrap();
        assert_eq!(status.selected_node_id.as_deref(), Some("n1"));
        h.enhanced.disable().await.unwrap();
        assert!(h.enhanced.core_status().await.is_none());
    }

    #[tokio::test]
    async fn enable_without_profile_is_an_internal_error() {
        let h = harness(true);
        let error = h.enhanced.enable().await.unwrap_err();
        assert!(matches!(
            error,
            ClientError::Failed {
                code: ErrorCode::Internal,
                ..
            }
        ));
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::Off);
    }

    #[tokio::test]
    async fn live_profile_update_and_rollback() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();

        h.enhanced.set_profile(PROFILE_R2, "r2").await.unwrap();
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
        assert_eq!(h.service.owner.lock().unwrap().as_ref().unwrap().1, "r2");
        assert_eq!(persisted(&h).profile_revision.as_deref(), Some("r2"));

        *h.service.update_error.lock().unwrap() =
            Some("PROFILE_UPDATE_ROLLED_BACK: TLS_REQUIRED: bad".into());
        let error = h
            .enhanced
            .set_profile(br#"{"revision":"r3"}"#, "r3")
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ClientError::Failed {
                code: ErrorCode::ProfileInvalid,
                ..
            }
        ));
        let state = h.enhanced.state();
        assert_eq!(state.phase, ConnectionPhase::On, "rolled back: still on");
        assert_eq!(state.reason.unwrap().code, ErrorCode::ProfileInvalid);

        *h.service.update_error.lock().unwrap() =
            Some("PROFILE_UPDATE_AND_ROLLBACK_FAILED: x".into());
        assert!(h
            .enhanced
            .set_profile(br#"{"revision":"r4"}"#, "r4")
            .await
            .is_err());
        assert_eq!(h.enhanced.state().phase, ConnectionPhase::Error);
    }

    #[tokio::test]
    async fn every_connect_and_update_carries_the_selection_and_the_pins() {
        let h = harness(true);
        let pins = || crate::ingress::Pins::from([("n1".to_string(), "k2".to_string())]);
        *h.selection.lock().unwrap() = Some("n2".into());
        h.enhanced.inner.config.ingress_pins.set(pins());
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        assert_eq!(
            *h.service.choices.lock().unwrap(),
            vec![core_ipc::ApplyChoices::new(Some("n2".into()), &pins())]
        );
        assert_eq!(h.service.selected.lock().unwrap().as_deref(), Some("n2"));
        assert!(
            h.service.pins.lock().unwrap().is_empty(),
            "no pin-ingress after connect"
        );

        // Changed live since: the next update carries the current ones.
        *h.selection.lock().unwrap() = Some("n1".into());
        h.enhanced
            .inner
            .config
            .ingress_pins
            .set(crate::ingress::Pins::new());
        h.enhanced.set_profile(PROFILE_R2, "r2").await.unwrap();
        assert_eq!(
            h.service.choices.lock().unwrap().last(),
            Some(&core_ipc::ApplyChoices::new(
                Some("n1".into()),
                &crate::ingress::Pins::new()
            ))
        );
        assert!(h.service.pins.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn select_node_while_on_maps_unknown_nodes() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        h.enhanced.select_node("n2").await.unwrap();
        assert_eq!(h.service.selected.lock().unwrap().as_deref(), Some("n2"));
        let error = h.enhanced.select_node("missing").await.unwrap_err();
        assert!(matches!(
            error,
            ClientError::Failed {
                code: ErrorCode::NodeNotFound,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn restore_reattaches_to_our_session_or_reports_contention() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        let session = h.enhanced.inner.with_data(|d| d.machine.session()).unwrap();

        // A fresh controller (app relaunch) finds its own session running.
        let relaunched = Enhanced::new(
            h.enhanced.inner.config.clone(),
            h.service.clone(),
            h.hooks.clone(),
            None,
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        );
        relaunched.restore().await;
        assert_eq!(relaunched.state().phase, ConnectionPhase::On);
        assert_eq!(
            relaunched.inner.with_data(|d| d.machine.generation),
            session.generation
        );

        // Someone else owns the core now.
        *h.service.owner.lock().unwrap() = None;
        *h.service.foreign.lock().unwrap() = Some(SessionRef {
            session_id: uuid::Uuid::new_v4().to_string(),
            generation: 9,
        });
        let other = Enhanced::new(
            h.enhanced.inner.config.clone(),
            h.service.clone(),
            h.hooks.clone(),
            None,
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        );
        other.restore().await;
        let state = other.state();
        assert_eq!(state.phase, ConnectionPhase::Contended);
        assert_eq!(state.reason.unwrap().code, ErrorCode::ServiceBusy);

        // Nothing running at all: the session was lost.
        *h.service.foreign.lock().unwrap() = None;
        let lost = Enhanced::new(
            h.enhanced.inner.config.clone(),
            h.service.clone(),
            h.hooks.clone(),
            None,
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        );
        lost.restore().await;
        assert_eq!(lost.state().phase, ConnectionPhase::Off);
        assert_eq!(lost.state().reason, None);
        assert_eq!(persisted(&h).desired, Desired::Disconnected);
    }

    #[tokio::test]
    async fn shutdown_releases_the_session_and_is_not_resumed() {
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        h.enhanced.shutdown().await;
        assert!(h.service.owner.lock().unwrap().is_none());
        assert_eq!(persisted(&h).desired, Desired::Disconnected);
        assert!(h.enhanced.inner.with_data(|d| d.loops.is_empty()));
    }

    #[tokio::test]
    async fn retry_after_a_service_restart_and_a_failed_start_connects_a_fresh_session() {
        // Connected, then the package upgrade restarts the service: the core
        // and the session are gone.
        let h = harness(true);
        h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
        h.enhanced.enable().await.unwrap();
        let original = h.enhanced.inner.with_data(|d| d.machine.session()).unwrap();
        *h.service.owner.lock().unwrap() = None;

        // The app restores enhanced mode and finds the session lost.
        let app = Enhanced::new(
            h.enhanced.inner.config.clone(),
            h.service.clone(),
            h.hooks.clone(),
            None,
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        );
        app.set_profile(PROFILE_R1, "r1").await.unwrap();
        app.restore().await;
        assert_eq!(app.state().phase, ConnectionPhase::Off);

        // First connect: the service fails the start (what a broken core
        // channel used to report).
        *h.service.connect_error.lock().unwrap() =
            Some("Connection reset by peer (os error 104)".into());
        assert!(app.enable().await.is_err());
        let failed = app.inner.with_data(|d| d.machine.clone());
        assert_eq!(failed.phase, ConnectionPhase::Error);
        assert!(failed.retryable);
        assert_eq!(failed.reason.unwrap().code, ErrorCode::ConnectFailed);
        let failed_session = SessionRef {
            session_id: failed.session_id.clone().unwrap(),
            generation: failed.generation,
        };
        assert_ne!(failed_session, original);

        // Second retry: a new session and generation owns the new core, and
        // the Core API calls that follow carry it.
        *h.service.connect_error.lock().unwrap() = None;
        app.retry().await.unwrap();
        assert_eq!(app.state().phase, ConnectionPhase::On);
        let current = app.inner.with_data(|d| d.machine.session()).unwrap();
        assert_eq!(current.generation, failed_session.generation + 1);
        assert_ne!(current.session_id, failed_session.session_id);
        assert_eq!(
            h.service
                .owner
                .lock()
                .unwrap()
                .as_ref()
                .map(|(s, _)| s.clone()),
            Some(current.clone())
        );
        assert_eq!(
            app.core_status().await.map(|status| status.state),
            Some("running".to_string())
        );
        // The retry after the failed start began from a fresh handshake.
        assert_eq!(h.service.auth_resets.load(Ordering::SeqCst), 1);

        // A late release of an earlier session cannot touch the new core.
        for stale in [&original, &failed_session] {
            assert!(h.service.disconnect(stale).await.is_err());
        }
        assert_eq!(app.state().phase, ConnectionPhase::On);
        assert!(h.service.owner.lock().unwrap().is_some());
        app.shutdown().await;
        h.enhanced.shutdown().await;
    }
}
