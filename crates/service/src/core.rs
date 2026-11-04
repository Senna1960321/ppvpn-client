//! The privileged TUN instance: the Rust ppvpn-core engine
//! (`Role::Tun`, docs/host-integration.md) in this process, and the
//! client's Core API calls on it.
//!
//! One instance at a time, owned by one client session: the session that
//! connected, under the OS user that connected it, for as long as its lease
//! is renewed. Two rules keep an old instance from hurting its successor:
//!
//! - **Keyed to the instance.** A Core API call is bound to the instance it
//!   was authorized for (it holds that instance, not the slot), so a call
//!   meant for an older instance never reaches a newer one.
//! - **Stopped means shut down.** [`CoreManager::stop_because`] returns only
//!   once the engine's `shutdown` did (bounded, 10 s): its TUN, routes and
//!   rules are gone before the next instance starts.
//!
//! An instance that reaches `Fatal` is stopped like one whose lease lapsed
//! (the watchdog's [`CoreManager::reap_exited`]); its session's watchers
//! hear `exited`, and the client connects again. On Windows a shutdown that
//! leaves WFP filters (or anything it cannot classify) behind ends the
//! process: the filters belong to the process, and the service manager
//! restarts the service.

use crate::logfile::RotatingLog;
use crate::protocol::{ApplyChoices, ConnectPayload, SessionRef, Status, UpdateProfilePayload};
use anyhow::{anyhow, Result};
use log::info;
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use ppvpn_core::{
    Engine, EngineConfig, EngineState, Event, EventItem, EventKind, LogConfig, LogLevel, LogSink,
    Platform, Role,
};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

pub static CORE: Lazy<Mutex<CoreManager>> = Lazy::new(|| {
    Mutex::new(CoreManager {
        watchers: crate::watch::HUB.clone(),
        ..CoreManager::default()
    })
});

/// The engine's tasks run here; the service's own threads call into it with
/// `block_on` (the IPC handlers run on blocking threads, the watchdog on a
/// plain one).
static ENGINE_RT: Lazy<tokio::runtime::Runtime> = Lazy::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .thread_name("ppvpn-engine")
        .enable_all()
        .build()
        .expect("engine runtime")
});

/// Why an instance stopped, as told to the session's watchers
/// (`core_stopped`).
pub mod stop_reason {
    /// Its session disconnected.
    pub const DISCONNECTED: &str = "disconnected";
    /// Its lease was not renewed in time.
    pub const LEASE_EXPIRED: &str = "lease_expired";
    /// Another session of the same OS user took the connection over.
    pub const TAKEN_OVER: &str = "taken_over";
    /// A new instance replaced it (a new Connect).
    pub const REPLACED: &str = "replaced";
    /// It failed to start.
    pub const START_FAILED: &str = "start_failed";
    /// A profile update and its rollback both failed.
    pub const PROFILE_FAILED: &str = "profile_failed";
    /// It failed on its own (the engine reached `Fatal`).
    pub const EXITED: &str = "exited";
    /// The service is shutting down.
    pub const SERVICE_STOPPING: &str = "service_stopping";
}
pub const LEASE_DURATION: Duration = Duration::from_secs(45);
/// The process exit code that asks the service manager for a restart.
#[cfg(windows)]
const RESTART_EXIT_CODE: i32 = 3;

#[cfg(target_os = "macos")]
const CORE_STATE: &str = "/Library/Application Support/PPVPN/core/state";
#[cfg(windows)]
const CORE_STATE: &str = r"C:\ProgramData\PPVPN\core\state";
// /var/lib/ppvpn is created by the systemd unit (StateDirectory=, see
// install.rs).
#[cfg(not(any(windows, target_os = "macos")))]
const CORE_STATE: &str = "/var/lib/ppvpn/core/state";

// ---------------------------------------------------------------------------
// Instances
// ---------------------------------------------------------------------------

/// What a shutdown could not undo (`ShutdownReport.leftovers`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leftover {
    /// `runtime`, `task`, `tun`, `route`, `dns`, `wfp`, `rule`, `steps`.
    pub kind: String,
    pub name: String,
    pub detail: String,
}

/// One TUN instance as the manager drives it. Every call blocks the calling
/// thread; none is made while holding [`CORE`] except at bring-up and stop.
pub trait Instance: Send + Sync {
    /// One Core API v1 call; errors read `"CODE: message"`.
    fn call(&self, path: &str, body: Value, timeout: Duration) -> Result<Value>;
    /// The reason (JSON) once the instance is `Fatal`.
    fn fatal(&self) -> Option<String>;
    /// Stops and releases everything; what could not be undone.
    fn shutdown(&self) -> Vec<Leftover>;
}

/// Creates instances.
pub trait Launcher: Send {
    /// A new instance keeping its state in `state`; `debug` turns on the
    /// debug log (visited domains included).
    fn launch(&self, state: &Path, debug: bool) -> Result<Arc<dyn Instance>>;
}

/// [`Instance`] on the engine.
struct EngineInstance {
    engine: Engine,
    /// Set once the engine reached `Fatal` (see [`watch_state`]).
    fatal: Arc<Mutex<Option<String>>>,
}

impl Instance for EngineInstance {
    fn call(&self, path: &str, body: Value, timeout: Duration) -> Result<Value> {
        ENGINE_RT.block_on(async {
            match tokio::time::timeout(
                timeout,
                ppvpn_engine_host::dispatch(&self.engine, path, body),
            )
            .await
            {
                Ok(Ok(data)) => Ok(data),
                Ok(Err(error)) => Err(anyhow!("{}: {}", error.code, error.message)),
                Err(_) => Err(anyhow!("CORE_IPC_TIMEOUT: {path} after {timeout:?}")),
            }
        })
    }

    fn fatal(&self) -> Option<String> {
        self.fatal.lock().clone()
    }

    fn shutdown(&self) -> Vec<Leftover> {
        match ENGINE_RT.block_on(self.engine.shutdown()) {
            Ok(report) => report
                .leftovers
                .into_iter()
                .map(|leftover| Leftover {
                    kind: serde_json::to_value(leftover.kind)
                        .ok()
                        .and_then(|kind| kind.as_str().map(str::to_string))
                        .unwrap_or_default(),
                    name: leftover.name,
                    detail: leftover.detail,
                })
                .collect(),
            // Already shut down (by an earlier call).
            Err(error) => {
                log::warn!("engine shutdown: {}: {}", error.code, error.message);
                Vec::new()
            }
        }
    }
}

/// Creates engines, writing their log lines into `ppvpn-core.log` beside
/// the service log (capped and rolled over like it).
#[derive(Default)]
struct EngineLauncher {
    /// Opened on first use and shared by every instance.
    log: Mutex<Option<Arc<Mutex<RotatingLog>>>>,
}

impl EngineLauncher {
    fn log(&self) -> Option<Arc<Mutex<RotatingLog>>> {
        let mut slot = self.log.lock();
        if slot.is_none() {
            let path = match crate::logfile::prepare_log_dir() {
                Ok(dir) => dir.join(crate::logfile::CORE_LOG),
                Err(error) => {
                    log::warn!("core log directory unavailable: {error}");
                    return None;
                }
            };
            match RotatingLog::open(&path, crate::logfile::MAX_BYTES, crate::logfile::KEEP_FILES) {
                Ok(opened) => *slot = Some(Arc::new(Mutex::new(opened))),
                Err(error) => {
                    log::warn!("cannot open core log {}: {error}", path.display());
                    return None;
                }
            }
        }
        slot.clone()
    }
}

impl Launcher for EngineLauncher {
    fn launch(&self, state: &Path, debug: bool) -> Result<Arc<dyn Instance>> {
        prepare_state_dir(state)?;
        let level = if debug {
            LogLevel::Debug
        } else {
            LogLevel::Info
        };
        let sink = if self.log().is_some() {
            LogSink::Channel
        } else {
            LogSink::None
        };
        let config = EngineConfig::new(Role::Tun, platform(), state.to_path_buf())
            .with_log(LogConfig::new(level, sink));
        // The wintun.dll the installer puts beside the service.
        #[cfg(windows)]
        let config = config.with_tun(
            ppvpn_core::TunConfig::new()
                .with_wintun_dll(service_exe_dir().unwrap_or_default().join("wintun.dll")),
        );
        let engine = ENGINE_RT
            .block_on(Engine::new(config))
            .map_err(|error| anyhow!("{}: {}", error.code, error.message))?;
        if let Some(log) = self.log() {
            let mut lines = engine.logs();
            ENGINE_RT.spawn(async move {
                while let Some(mut line) = lines.recv().await {
                    if !line.ends_with('\n') {
                        line.push('\n');
                    }
                    // A full disk must not stall the engine: drop the line.
                    let _ = log.lock().write(line.as_bytes());
                }
            });
        }
        let fatal = Arc::new(Mutex::new(None));
        ENGINE_RT.spawn(watch_state(engine.clone(), fatal.clone()));
        info!(
            "ppvpn-core {} created (TUN instance, log level {level:?})",
            Engine::version().core_version
        );
        Ok(Arc::new(EngineInstance { engine, fatal }))
    }
}

/// Follows the engine's state until it shuts down: records the reason of a
/// `Fatal` in `fatal` (the watchdog stops the instance) and logs `Degraded`
/// (the engine heals that itself). Reading the state from the events keeps
/// the engine's status refresher idle: `status()` would wake it.
async fn watch_state(engine: Engine, fatal: Arc<Mutex<Option<String>>>) {
    let mut states = engine.subscribe(&[EventKind::StateChanged]);
    // A Fatal between Engine::new and the subscription has no event.
    let mut state = Some(engine.status().state);
    loop {
        match state.take() {
            Some(EngineState::Fatal { reason }) => {
                let reason = serde_json::to_string(&reason).unwrap_or_default();
                log::warn!("ppvpn-core fatal: {reason}");
                *fatal.lock() = Some(reason);
                return;
            }
            Some(EngineState::Degraded { reasons }) => {
                log::info!(
                    "ppvpn-core degraded: {}",
                    serde_json::to_string(&reasons).unwrap_or_default()
                );
            }
            _ => {}
        }
        state = match states.recv().await {
            Some(EventItem::Event {
                event: Event::StateChanged { state, .. },
            }) => Some(state),
            // Fell behind: the current state is what counts.
            Some(EventItem::Lagged { .. }) => Some(engine.status().state),
            Some(_) => None,
            // Shut down.
            None => return,
        };
    }
}

fn platform() -> Platform {
    if cfg!(windows) {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::Macos
    } else {
        Platform::Linux
    }
}

/// `kind/name: detail; …` for the log.
fn describe_leftovers(leftovers: &[Leftover]) -> String {
    leftovers
        .iter()
        .map(|leftover| format!("{}/{}: {}", leftover.kind, leftover.name, leftover.detail))
        .collect::<Vec<_>>()
        .join("; ")
}

/// The leftovers after which only ending the process cleans up: WFP filters
/// (strict route), or a runtime leftover that could be one. Windows only;
/// elsewhere the next instance's sweep takes care of what is left.
fn needs_process_restart(leftovers: &[Leftover], windows: bool) -> bool {
    windows
        && leftovers
            .iter()
            .any(|leftover| leftover.kind == "wfp" || leftover.kind == "runtime")
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

pub struct CoreManager {
    launcher: Box<dyn Launcher>,
    state_dir: PathBuf,
    state: Option<RunningCore>,
    /// Numbers instances, so a call can tell whether its instance is still
    /// the current one.
    launched: u64,
    /// Watch connections, told when a session's instance stops.
    watchers: Arc<crate::watch::Hub>,
}

impl Default for CoreManager {
    fn default() -> Self {
        Self {
            launcher: Box::<EngineLauncher>::default(),
            state_dir: PathBuf::from(CORE_STATE),
            state: None,
            launched: 0,
            watchers: Default::default(),
        }
    }
}

struct RunningCore {
    id: u64,
    instance: Arc<dyn Instance>,
    session: SessionRef,
    /// OS user of the client that started this instance (uid on Unix, user
    /// SID on Windows); `None` when it could not be resolved.
    owner_user: Option<String>,
    profile_revision: String,
    profile: Value,
    /// `allowed_rule_set_hosts` `profile` was applied with, reused for a
    /// rollback.
    rule_set_hosts: Vec<String>,
    /// `routing_mode` `profile` was applied with, reused for a rollback.
    routing_mode: Option<String>,
    lease_deadline: Instant,
}

impl RunningCore {
    fn alive(&self) -> bool {
        self.instance.fatal().is_none()
    }
}

impl CoreManager {
    /// Whether a core runs, for any session (a silent update waits for
    /// none).
    pub fn has_core(&self) -> bool {
        self.state.is_some()
    }

    /// Service status for `client_pid`. Session details (id, generation,
    /// revision, lease) are shown only to the owner's OS user; anyone else
    /// learns only that the connection is in use.
    pub fn status(&mut self, client_pid: u32) -> Status {
        let caller_user = process_user(client_pid);
        self.status_for(caller_user.as_deref())
    }

    fn status_for(&mut self, caller_user: Option<&str>) -> Status {
        let live = self
            .state
            .as_ref()
            .is_some_and(|core| core.lease_deadline > Instant::now() && core.alive());
        let view = self.state.as_ref().filter(|_| live).map(|core| CoreView {
            pid: std::process::id(),
            session: &core.session,
            owner_user: core.owner_user.as_deref(),
            profile_revision: &core.profile_revision,
            lease_remaining: core
                .lease_deadline
                .saturating_duration_since(Instant::now()),
        });
        describe(view, caller_user)
    }

    /// Start (or keep) the TUN instance for `body.session`. `client_pid` is
    /// the authenticated caller, used for the same-user takeover check.
    /// Returns the service's pid (the instance runs inside it).
    pub fn connect(&mut self, body: ConnectPayload, client_pid: u32) -> Result<u32> {
        validate_session(&body.session)?;
        let caller_user = process_user(client_pid);
        let decision = match self.state.as_ref() {
            None => Ownership::Free,
            Some(current) => {
                let live = current.lease_deadline > Instant::now() && current.alive();
                ownership(
                    &Owner {
                        session: &current.session,
                        user: current.owner_user.as_deref(),
                        live,
                    },
                    &body.session,
                    body.take_over,
                    caller_user.as_deref(),
                )
            }
        };
        let taking_over = decision == Ownership::TakeOver;
        match decision {
            Ownership::Free => {}
            Ownership::Expired => {
                self.stop_because(stop_reason::LEASE_EXPIRED).ok();
            }
            Ownership::Same => {
                if let Some(current) = self.state.as_mut() {
                    current.lease_deadline = Instant::now() + LEASE_DURATION;
                    return Ok(std::process::id());
                }
            }
            Ownership::TakeOver => {
                if let Some(current) = self.state.as_ref() {
                    // No session ids in the log (see ipc.rs, watch).
                    info!(
                        "generation {} takes over from generation {}",
                        body.session.generation, current.session.generation
                    );
                }
                self.stop_because(stop_reason::TAKEN_OVER).ok();
            }
            Ownership::Refused(code) => return Err(anyhow!(code)),
        }
        let started = self.start_core(body, caller_user);
        if let Err(error) = &started {
            if taking_over {
                // Two TUN instances cannot coexist, so the previous session
                // was stopped first; now nobody is connected.
                log::error!(
                    "takeover: the new session's instance failed after the previous session was stopped: {error:#}"
                );
            }
        }
        started
    }

    /// Creates a new instance and brings it up. Any failure after the
    /// creation stops the instance again, so a failed attempt leaves nothing
    /// behind.
    fn start_core(&mut self, body: ConnectPayload, caller_user: Option<String>) -> Result<u32> {
        let profile_revision = body
            .profile
            .get("revision")
            .and_then(Value::as_str)
            .filter(|revision| !revision.is_empty())
            .ok_or_else(|| anyhow!("PROFILE_REVISION_REQUIRED"))?
            .to_string();
        let requested_schema = body
            .profile
            .get("schema_version")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow!("PROFILE_SCHEMA_REQUIRED"))?;
        check_routing_mode(body.routing_mode.as_deref())?;
        // Whatever ran before must be gone: it shares our state directory.
        self.stop_because(stop_reason::REPLACED).ok();

        let debug = debug_log_flag(&self.state_dir);
        if let Some(flag) = &debug {
            // One line per routed connection, with the domain: for
            // troubleshooting only, never left on.
            log::warn!(
                "{} exists: ppvpn-core logs at debug level (includes visited domains)",
                flag.display()
            );
        }
        let created = Instant::now();
        let instance = self
            .launcher
            .launch(&self.state_dir, debug.is_some())
            .map_err(|error| start_error("create", error))?;
        info!(
            "ppvpn-core start: create took {} ms",
            created.elapsed().as_millis()
        );
        self.launched += 1;
        self.state = Some(RunningCore {
            id: self.launched,
            instance: instance.clone(),
            session: body.session.clone(),
            owner_user: caller_user,
            profile_revision,
            profile: body.profile.clone(),
            rule_set_hosts: body.allowed_rule_set_hosts.clone(),
            routing_mode: body.routing_mode.clone(),
            lease_deadline: Instant::now() + LEASE_DURATION,
        });
        let brought_up = bring_up(
            instance.as_ref(),
            &body.profile,
            requested_schema,
            &body.allowed_rule_set_hosts,
            body.routing_mode.as_deref(),
            &body.choices,
        );
        if let Err(error) = brought_up {
            log::error!("ppvpn-core failed to start: {error:#}");
            self.stop_because(stop_reason::START_FAILED).ok();
            return Err(error);
        }
        info!("ppvpn-core TUN started");
        Ok(std::process::id())
    }

    /// At service start (Linux): sweeps what the instance of a killed
    /// service left (policy rules that would misroute or block traffic until
    /// the next connection) by creating an instance and shutting it down
    /// again: the engine sweeps when it is created. macOS and Windows remove
    /// a dead process's TUN with everything on it.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn sweep_leftovers(&mut self) {
        if self.state.is_some() {
            return;
        }
        match self.launcher.launch(&self.state_dir, false) {
            Ok(instance) => {
                let leftovers = instance.shutdown();
                if !leftovers.is_empty() {
                    log::warn!(
                        "startup sweep left behind: {}",
                        describe_leftovers(&leftovers)
                    );
                }
            }
            Err(error) => log::warn!("startup sweep failed: {error:#}"),
        }
    }

    pub fn renew_lease(&mut self, session: &SessionRef, client_pid: u32) -> Result<Status> {
        let caller_user = process_user(client_pid);
        let core = self.authorize(session, caller_user.as_deref())?;
        core.lease_deadline = Instant::now() + LEASE_DURATION;
        Ok(self.status_for(caller_user.as_deref()))
    }

    pub fn disconnect(&mut self, session: &SessionRef, client_pid: u32) -> Result<()> {
        let caller_user = process_user(client_pid);
        self.authorize(session, caller_user.as_deref())?;
        self.stop_because(stop_reason::DISCONNECTED)
    }

    /// Registers a watch connection for the owner's session (see
    /// [`crate::watch`]). Under `self` only for the check and the
    /// registration: a stop can then never slip in between unnoticed.
    pub fn watch(
        &mut self,
        session: &SessionRef,
        client_pid: u32,
        closer: Option<crate::watch::Closer>,
    ) -> Result<crate::watch::Registration> {
        let caller_user = process_user(client_pid);
        self.authorize(session, caller_user.as_deref())?;
        self.watchers
            .register(session.clone(), closer)
            .map_err(|code| anyhow!(code))
    }

    pub fn expire_lease(&mut self) {
        let expired = self
            .state
            .as_ref()
            .is_some_and(|core| core.lease_deadline <= Instant::now());
        if expired {
            info!("connection lease expired; stopping the TUN instance");
            self.stop_because(stop_reason::LEASE_EXPIRED).ok();
        }
    }

    // Only ipc's unix tests use it.
    #[cfg(all(test, unix))]
    pub fn watchers(&self) -> Arc<crate::watch::Hub> {
        self.watchers.clone()
    }

    /// An instance that failed on its own (`Fatal`) is stopped and its
    /// session's watchers are told right away, instead of when the client
    /// next renews its lease.
    pub fn reap_exited(&mut self) {
        let fatal = self.state.as_ref().and_then(|core| core.instance.fatal());
        if let Some(reason) = fatal {
            log::warn!("ppvpn-core failed on its own (fatal: {reason}); stopping it");
            self.stop_because(stop_reason::EXITED).ok();
        }
    }

    /// One Core API call for the owner's session. Holds `self` for the
    /// whole exchange; the IPC path uses [`call_api`], which does not.
    #[cfg(test)]
    pub fn call_api(
        &mut self,
        session: &SessionRef,
        path: &str,
        body: Value,
        client_pid: u32,
    ) -> Result<Value> {
        let instance = self.prepare_call(session, path, client_pid)?;
        let timeout = call_timeout(path, &body);
        instance.call(path, body, timeout)
    }

    /// Authorizes a Core API call for the owner's session (renewing its
    /// lease) and returns the instance it is bound to.
    fn prepare_call(
        &mut self,
        session: &SessionRef,
        path: &str,
        client_pid: u32,
    ) -> Result<Arc<dyn Instance>> {
        if !ALLOWED_PATHS.contains(&path) {
            return Err(anyhow!("Core API path is not allowed"));
        }
        let caller_user = process_user(client_pid);
        let core = self.authorize(session, caller_user.as_deref())?;
        core.lease_deadline = Instant::now() + LEASE_DURATION;
        Ok(core.instance.clone())
    }

    /// The caller must run as the owner's OS user *and* present the owner's
    /// session: a session id alone never grants access.
    fn authorize(
        &mut self,
        session: &SessionRef,
        caller_user: Option<&str>,
    ) -> Result<&mut RunningCore> {
        validate_session(session)?;
        let core = self
            .state
            .as_mut()
            .ok_or_else(|| anyhow!("CONNECTION_NOT_ACTIVE"))?;
        access(
            &core.session,
            core.owner_user.as_deref(),
            session,
            caller_user,
        )
        .map_err(|code| anyhow!(code))?;
        if core.lease_deadline <= Instant::now() || !core.alive() {
            return Err(anyhow!("CONNECTION_LEASE_EXPIRED"));
        }
        Ok(core)
    }

    /// The current instance is `id`.
    fn is_current(&self, id: u64) -> bool {
        self.state.as_ref().is_some_and(|core| core.id == id)
    }

    /// Stops the current instance and returns once the engine's shutdown
    /// did (bounded): the TUN, its routes and rules are gone.
    #[cfg(test)]
    pub fn stop(&mut self) -> Result<()> {
        self.stop_because("stopped")
    }

    /// [`Self::stop`], telling the session's watchers `reason` (see
    /// [`stop_reason`]) before the instance is shut down.
    pub fn stop_because(&mut self, reason: &str) -> Result<()> {
        if let Some(core) = self.state.take() {
            self.watchers.core_stopped(&core.session, reason);
            let started = Instant::now();
            info!("stopping ppvpn-core ({reason})");
            let leftovers = core.instance.shutdown();
            info!("ppvpn-core stopped in {} ms", started.elapsed().as_millis());
            if !leftovers.is_empty() {
                log::warn!(
                    "ppvpn-core shutdown left behind: {}",
                    describe_leftovers(&leftovers)
                );
            }
            if reason != stop_reason::SERVICE_STOPPING
                && needs_process_restart(&leftovers, cfg!(windows))
            {
                restart_process();
            }
        }
        Ok(())
    }
}

/// Ends the service so the service manager restarts it: the WFP filters a
/// shutdown left go with the process.
#[cfg(windows)]
fn restart_process() {
    log::error!("leftovers only a process exit removes; exiting for a restart");
    log::logger().flush();
    std::process::exit(RESTART_EXIT_CODE)
}

#[cfg(not(windows))]
fn restart_process() {}

/// Checks the schema, applies the profile and starts the data plane.
/// Keep the leading stable code for older clients; append a small, safe
/// phase marker while retaining the complete cause chain in local logs.
fn start_error(stage: &str, error: anyhow::Error) -> anyhow::Error {
    let detail = format!("{error:#}");
    let bare_code = !detail.is_empty()
        && detail
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_');
    let separator = if bare_code { ": startup failed" } else { "" };
    anyhow!("{detail}{separator} [start_stage={stage}]")
}

fn bring_up(
    instance: &dyn Instance,
    profile: &Value,
    requested_schema: u64,
    hosts: &[String],
    mode: Option<&str>,
    choices: &ApplyChoices,
) -> Result<()> {
    // Per-phase durations at info level: a slow first connect (TUN
    // creation, rule-set downloads) shows which step took the time.
    let mut phase = Instant::now();
    let mut lap = |name: &str| {
        info!(
            "ppvpn-core start: {name} took {} ms",
            phase.elapsed().as_millis()
        );
        phase = Instant::now();
    };
    let none = serde_json::json!({});
    let version = instance
        .call(
            "/v1/get-version",
            none.clone(),
            call_timeout("/v1/get-version", &none),
        )
        .map_err(|error| start_error("version", error))?;
    let supported_schema = version
        .get("profile_schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| start_error("version", anyhow!("CORE_SCHEMA_CAPABILITY_MISSING")))?;
    if requested_schema != supported_schema {
        return Err(start_error(
            "profile",
            anyhow!("PROFILE_SCHEMA_UNSUPPORTED"),
        ));
    }
    let body = apply_body(profile, hosts, mode, choices);
    let timeout = call_timeout("/v1/apply-profile", &body);
    let applied = instance.call("/v1/apply-profile", body, timeout);
    lap("apply-profile");
    applied.map_err(|error| start_error("profile", error))?;
    let started = instance.call("/v1/start", none.clone(), call_timeout("/v1/start", &none));
    lap("start (TUN, routes)");
    started.map_err(|error| start_error("start", error))?;
    Ok(())
}

/// The first steps of the service's shutdown, once `ipc::begin_stopping`
/// refuses new work: every watch connection is told `stopping` and closed
/// (within about `grace`, whatever its client does), and only then is the
/// core lock taken for the instance's stop, which the caller does with the
/// returned guard.
pub fn stop_sequence<'a>(
    core: &'a Mutex<CoreManager>,
    watchers: &crate::watch::Hub,
    grace: Duration,
) -> parking_lot::MutexGuard<'a, CoreManager> {
    watchers.close_all(grace);
    core.lock()
}

/// Core API paths a client may call (the service also uses these itself).
const ALLOWED_PATHS: &[&str] = &[
    "/v1/get-version",
    "/v1/validate-profile",
    "/v1/apply-profile",
    "/v1/start",
    "/v1/stop",
    "/v1/get-status",
    "/v1/list-nodes",
    "/v1/select-node",
    "/v1/get-selected-node",
    // A node fixed to one ingress (live, no engine rebuild).
    "/v1/pin-ingress",
    "/v1/probe-entrances",
    "/v1/probe-availability",
    "/v1/get-local-proxy-metadata",
    "/v1/get-local-proxy-credential",
    "/v1/get-traffic",
    "/v1/get-connections",
    // The system's sleep and wake, as the app hears them (4.7).
    "/v1/sleep",
    "/v1/wake",
];

/// Upper bound for one Core API call. Status-type calls are answered from
/// memory; probes run for the `timeout_ms` the client asked for (per
/// ingress, up to two waves, as the client's own deadline in
/// crates/desktop/src/core_ipc.rs).
fn call_timeout(path: &str, body: &Value) -> Duration {
    match path {
        "/v1/get-version"
        | "/v1/get-status"
        | "/v1/get-traffic"
        | "/v1/get-selected-node"
        | "/v1/sleep"
        | "/v1/wake" => Duration::from_secs(2),
        "/v1/list-nodes"
        | "/v1/select-node"
        | "/v1/get-connections"
        | "/v1/get-local-proxy-metadata"
        | "/v1/get-local-proxy-credential" => Duration::from_secs(5),
        "/v1/probe-entrances" | "/v1/probe-availability" => {
            let asked = body
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(10_000)
                .min(60_000);
            Duration::from_millis(asked.saturating_mul(2)) + Duration::from_secs(5)
        }
        "/v1/stop" => Duration::from_secs(10),
        // validate / apply profile, start.
        _ => Duration::from_secs(30),
    }
}

/// `CoreApi`: authorizes under `core`, then calls the instance without it,
/// so a slow call (a probe) never delays Disconnect, a lease renewal or a
/// status query.
pub fn call_api(
    core: &Mutex<CoreManager>,
    session: &SessionRef,
    path: &str,
    body: Value,
    client_pid: u32,
) -> Result<Value> {
    let instance = core.lock().prepare_call(session, path, client_pid)?;
    let timeout = call_timeout(path, &body);
    instance.call(path, body, timeout)
}

/// `UpdateProfile`: checks the schema and applies the new profile (rolling
/// back on failure) without holding `core` during the calls, then records
/// the new revision if the same instance is still the session's.
pub fn update_profile(
    core: &Mutex<CoreManager>,
    body: UpdateProfilePayload,
    client_pid: u32,
) -> Result<Status> {
    let caller_user = process_user(client_pid);
    let revision = body
        .profile
        .get("revision")
        .and_then(Value::as_str)
        .filter(|revision| !revision.is_empty())
        .ok_or_else(|| anyhow!("PROFILE_REVISION_REQUIRED"))?
        .to_string();
    let requested_schema = body
        .profile
        .get("schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("PROFILE_SCHEMA_REQUIRED"))?;
    check_routing_mode(body.routing_mode.as_deref())?;
    let (id, instance, previous, previous_hosts, previous_mode) = {
        let mut manager = core.lock();
        let current = manager.authorize(&body.session, caller_user.as_deref())?;
        (
            current.id,
            current.instance.clone(),
            current.profile.clone(),
            current.rule_set_hosts.clone(),
            current.routing_mode.clone(),
        )
    };

    let none = serde_json::json!({});
    let version = instance.call(
        "/v1/get-version",
        none.clone(),
        call_timeout("/v1/get-version", &none),
    )?;
    let supported_schema = version
        .get("profile_schema_version")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow!("CORE_SCHEMA_CAPABILITY_MISSING"))?;
    if requested_schema != supported_schema {
        return Err(anyhow!("PROFILE_SCHEMA_UNSUPPORTED"));
    }
    let apply = |profile: &Value, hosts: &[String], mode: Option<&str>| {
        let request = apply_body(profile, hosts, mode, &body.choices);
        let timeout = call_timeout("/v1/apply-profile", &request);
        instance.call("/v1/apply-profile", request, timeout)
    };
    // ApplyProfile is transactional while the instance runs: it replaces
    // the live runtime and rolls the previous one back if the candidate
    // cannot start. Both carry the client's current selection and pins.
    let applied = apply(
        &body.profile,
        &body.allowed_rule_set_hosts,
        body.routing_mode.as_deref(),
    );
    if let Err(error) = applied {
        // Re-apply the previous revision once, in case the candidate was
        // committed after all. If ApplyProfile itself rejected the
        // candidate, this is an idempotent no-op.
        let rollback = apply(&previous, &previous_hosts, previous_mode.as_deref());
        return match rollback {
            Ok(_) => Err(anyhow!("PROFILE_UPDATE_ROLLED_BACK: {error}")),
            Err(rollback_error) => {
                let mut manager = core.lock();
                if manager.is_current(id) {
                    manager.stop_because(stop_reason::PROFILE_FAILED).ok();
                }
                Err(anyhow!(
                    "PROFILE_UPDATE_AND_ROLLBACK_FAILED: {error}; {rollback_error}"
                ))
            }
        };
    }
    let mut manager = core.lock();
    let current = manager.authorize(&body.session, caller_user.as_deref())?;
    if current.id != id {
        return Err(anyhow!("CONNECTION_NOT_ACTIVE"));
    }
    current.profile = body.profile;
    current.rule_set_hosts = body.allowed_rule_set_hosts;
    current.routing_mode = body.routing_mode;
    current.profile_revision = revision;
    current.lease_deadline = Instant::now() + LEASE_DURATION;
    Ok(manager.status_for(caller_user.as_deref()))
}

/// Rejects a `routing_mode` other than `rules` / `global` before anything
/// starts.
fn check_routing_mode(requested: Option<&str>) -> Result<()> {
    match requested {
        None | Some("rules" | "global") => Ok(()),
        Some(_) => Err(anyhow!("ROUTING_MODE_INVALID")),
    }
}

/// `apply-profile` request body.
fn apply_body(
    profile: &Value,
    allowed_rule_set_hosts: &[String],
    routing_mode: Option<&str>,
    choices: &ApplyChoices,
) -> Value {
    let mut body = serde_json::json!({ "profile": profile });
    if !allowed_rule_set_hosts.is_empty() {
        body["allowed_rule_set_hosts"] = serde_json::json!(allowed_rule_set_hosts);
    }
    if let Some(mode) = routing_mode {
        body["routing_mode"] = serde_json::json!(mode);
    }
    if let Some(node_id) = &choices.selected_node_id {
        body["selected_node_id"] = serde_json::json!(node_id);
    }
    if !choices.pins.is_empty() {
        body["pins"] = serde_json::json!(choices.pins);
    }
    body
}

fn validate_session(session: &SessionRef) -> Result<()> {
    if session.session_id.is_empty() || uuid::Uuid::parse_str(&session.session_id).is_err() {
        return Err(anyhow!("INVALID_SESSION_ID"));
    }
    if session.generation == 0 {
        return Err(anyhow!("INVALID_GENERATION"));
    }
    Ok(())
}

/// `debug-log` next to the state directory (only an administrator can
/// create it there) turns on the debug log for the next connection.
fn debug_log_flag(state_dir: &Path) -> Option<PathBuf> {
    let flag = state_dir.parent()?.join("debug-log");
    flag.is_file().then_some(flag)
}

/// Creates the state directory, its parent private to the administrators.
fn prepare_state_dir(state: &Path) -> Result<()> {
    let parent = state
        .parent()
        .ok_or_else(|| anyhow!("invalid core state path"))?;
    std::fs::create_dir_all(state)?;
    harden_permissions(parent)?;
    Ok(())
}

#[cfg(windows)]
pub(crate) fn harden_permissions(path: &Path) -> Result<()> {
    use anyhow::Context;
    let status = std::process::Command::new("icacls.exe")
        .arg(path)
        .args([
            "/inheritance:r",
            "/grant:r",
            "*S-1-5-18:(OI)(CI)F",
            "*S-1-5-32-544:(OI)(CI)F",
        ])
        .status()
        .context("apply private core state ACL")?;
    if !status.success() {
        return Err(anyhow!(
            "CORE_STATE_ACL_FAILED:{}",
            status.code().unwrap_or(-1)
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn harden_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(path, permissions)?;
    Ok(())
}

#[cfg(windows)]
fn service_exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

/// The core's current owner, as seen by a new `Connect`.
struct Owner<'a> {
    session: &'a SessionRef,
    user: Option<&'a str>,
    /// Lease valid and instance not failed.
    live: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum Ownership {
    /// No core yet.
    Free,
    /// The previous owner's lease lapsed or its instance failed: replace it.
    Expired,
    /// The caller already owns the core: renew and reuse it.
    Same,
    /// Same OS user asked to take over: stop the other session's instance.
    TakeOver,
    Refused(&'static str),
}

/// A live instance, as `GetStatus` may describe it.
struct CoreView<'a> {
    pid: u32,
    session: &'a SessionRef,
    owner_user: Option<&'a str>,
    profile_revision: &'a str,
    lease_remaining: Duration,
}

/// `GetStatus` for `caller_user`: session details only for the owner's user.
fn describe(core: Option<CoreView<'_>>, caller_user: Option<&str>) -> Status {
    let idle = Status {
        running: false,
        pid: -1,
        bin_path: String::new(),
        mode: String::new(),
        service_version: env!("CARGO_PKG_VERSION").to_string(),
        service_build_id: crate::protocol::SERVICE_BUILD_ID.to_string(),
        session_id: None,
        generation: 0,
        profile_revision: None,
        lease_expires_at_ms: None,
    };
    let Some(core) = core else {
        return idle;
    };
    if !same_user(core.owner_user, caller_user) {
        // In use by someone else: no session, no pid, nothing to replay.
        return Status {
            running: true,
            mode: "transparent".to_string(),
            ..idle
        };
    }
    Status {
        running: true,
        pid: core.pid as i64,
        mode: "transparent".to_string(),
        session_id: Some(core.session.session_id.clone()),
        generation: core.session.generation,
        profile_revision: Some(core.profile_revision.to_string()),
        lease_expires_at_ms: Some(
            crate::protocol::now_epoch()
                .saturating_mul(1000)
                .saturating_add(core.lease_remaining.as_millis() as u64),
        ),
        ..idle
    }
}

/// Both identities known and equal. An unresolvable user matches nobody.
fn same_user(owner: Option<&str>, caller: Option<&str>) -> bool {
    matches!((owner, caller), (Some(owner), Some(caller)) if owner == caller)
}

/// Access to an existing session: same OS user first, then same session.
fn access(
    owner_session: &SessionRef,
    owner_user: Option<&str>,
    session: &SessionRef,
    caller_user: Option<&str>,
) -> std::result::Result<(), &'static str> {
    if !same_user(owner_user, caller_user) {
        return Err("CONNECTION_OWNED_BY_ANOTHER_USER");
    }
    if owner_session != session {
        return Err("STALE_OR_FOREIGN_SESSION");
    }
    Ok(())
}

fn ownership(
    current: &Owner<'_>,
    requested: &SessionRef,
    take_over: bool,
    caller_user: Option<&str>,
) -> Ownership {
    if !current.live {
        return Ownership::Expired;
    }
    if !same_user(current.user, caller_user) {
        // Never offered, never allowed: another OS user's connection.
        return Ownership::Refused("CONNECTION_OWNED_BY_ANOTHER_USER");
    }
    if current.session == requested {
        return Ownership::Same;
    }
    if take_over {
        Ownership::TakeOver
    } else {
        Ownership::Refused("CONNECTION_OWNED_BY_ANOTHER_SESSION")
    }
}

/// OS user of `pid`: uid on Unix, user SID on Windows.
fn process_user(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let mut sys = System::new();
    sys.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[Pid::from_u32(pid)]),
        true,
        ProcessRefreshKind::new().with_user(sysinfo::UpdateKind::Always),
    );
    sys.process(Pid::from_u32(pid))
        .and_then(|process| process.user_id())
        .map(|user| user.to_string())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn session(id: &str, generation: u64) -> SessionRef {
        SessionRef {
            session_id: id.to_string(),
            generation,
        }
    }

    fn owner<'a>(session: &'a SessionRef, user: Option<&'a str>, live: bool) -> Owner<'a> {
        Owner {
            session,
            user,
            live,
        }
    }

    #[test]
    fn same_user_can_take_over() {
        let current = session("a", 1);
        let caller = session("b", 7);
        assert_eq!(
            ownership(
                &owner(&current, Some("501"), true),
                &caller,
                true,
                Some("501")
            ),
            Ownership::TakeOver
        );
        // Without the flag the same user is told it is another session.
        assert_eq!(
            ownership(
                &owner(&current, Some("501"), true),
                &caller,
                false,
                Some("501")
            ),
            Ownership::Refused("CONNECTION_OWNED_BY_ANOTHER_SESSION")
        );
        assert_eq!(
            ownership(
                &owner(&current, Some("501"), true),
                &current,
                false,
                Some("501")
            ),
            Ownership::Same
        );
    }

    #[test]
    fn another_user_is_refused_even_with_the_owners_session() {
        let current = session("a", 1);
        let caller = session("b", 7);
        for take_over in [false, true] {
            assert_eq!(
                ownership(
                    &owner(&current, Some("501"), true),
                    &caller,
                    take_over,
                    Some("502")
                ),
                Ownership::Refused("CONNECTION_OWNED_BY_ANOTHER_USER")
            );
        }
        // Knowing the session id does not make another user the owner.
        assert_eq!(
            ownership(
                &owner(&current, Some("501"), true),
                &current,
                false,
                Some("502")
            ),
            Ownership::Refused("CONNECTION_OWNED_BY_ANOTHER_USER")
        );
        // Unresolvable identities match nobody.
        assert_eq!(
            ownership(&owner(&current, None, true), &caller, true, Some("501")),
            Ownership::Refused("CONNECTION_OWNED_BY_ANOTHER_USER")
        );
        assert_eq!(
            ownership(&owner(&current, Some("501"), true), &caller, true, None),
            Ownership::Refused("CONNECTION_OWNED_BY_ANOTHER_USER")
        );
        // An expired owner is replaced whoever asks.
        assert_eq!(
            ownership(
                &owner(&current, Some("501"), false),
                &caller,
                false,
                Some("502")
            ),
            Ownership::Expired
        );
    }

    #[test]
    fn session_calls_need_the_owners_user_and_session() {
        let owned = session("a", 1);
        assert_eq!(access(&owned, Some("501"), &owned, Some("501")), Ok(()));
        assert_eq!(
            access(&owned, Some("501"), &owned, Some("502")),
            Err("CONNECTION_OWNED_BY_ANOTHER_USER")
        );
        assert_eq!(
            access(&owned, Some("501"), &session("b", 2), Some("501")),
            Err("STALE_OR_FOREIGN_SESSION")
        );
        assert_eq!(
            access(&owned, None, &owned, None),
            Err("CONNECTION_OWNED_BY_ANOTHER_USER")
        );
    }

    #[test]
    fn take_over_is_required_in_the_payload() {
        let without: Result<ConnectPayload, _> = serde_json::from_value(serde_json::json!({
            "session_id": "a", "generation": 1, "profile": {}
        }));
        assert!(without.is_err());
        let with: ConnectPayload = serde_json::from_value(serde_json::json!({
            "session_id": "a", "generation": 1, "profile": {}, "take_over": true
        }))
        .unwrap();
        assert!(with.take_over);
    }

    #[test]
    fn rule_set_hosts_are_optional_in_the_payloads() {
        let old: ConnectPayload = serde_json::from_value(serde_json::json!({
            "session_id": "a", "generation": 1, "profile": {}, "take_over": false
        }))
        .unwrap();
        assert!(old.allowed_rule_set_hosts.is_empty());
        let new: ConnectPayload = serde_json::from_value(serde_json::json!({
            "session_id": "a", "generation": 1, "profile": {}, "take_over": false,
            "allowed_rule_set_hosts": ["api.example.com"]
        }))
        .unwrap();
        assert_eq!(new.allowed_rule_set_hosts, ["api.example.com"]);
        let update: UpdateProfilePayload = serde_json::from_value(serde_json::json!({
            "session_id": "a", "generation": 1, "profile": {},
            "allowed_rule_set_hosts": ["api.example.com:8443"]
        }))
        .unwrap();
        assert_eq!(update.allowed_rule_set_hosts, ["api.example.com:8443"]);
    }

    #[test]
    fn apply_bodies_carry_the_hosts_and_the_routing_mode_when_given() {
        let hosts = || vec!["api.example.com".to_string()];
        let profile = serde_json::json!({ "revision": "r1" });
        let none = ApplyChoices::default();
        assert_eq!(
            apply_body(&profile, &hosts(), None, &none),
            serde_json::json!({ "profile": { "revision": "r1" },
                "allowed_rule_set_hosts": ["api.example.com"] })
        );
        assert_eq!(
            apply_body(&profile, &[], None, &none),
            serde_json::json!({ "profile": { "revision": "r1" } })
        );
        assert_eq!(
            apply_body(&profile, &[], Some("global"), &none),
            serde_json::json!({ "profile": { "revision": "r1" }, "routing_mode": "global" })
        );
        assert!(check_routing_mode(None).is_ok());
        assert!(check_routing_mode(Some("rules")).is_ok());
        assert!(check_routing_mode(Some("global")).is_ok());
        assert_eq!(
            check_routing_mode(Some("direct")).unwrap_err().to_string(),
            "ROUTING_MODE_INVALID"
        );
        let old: UpdateProfilePayload = serde_json::from_value(serde_json::json!({
            "session_id": "a", "generation": 1, "profile": {}
        }))
        .unwrap();
        assert_eq!(old.routing_mode, None);
        let new: UpdateProfilePayload = serde_json::from_value(serde_json::json!({
            "session_id": "a", "generation": 1, "profile": {}, "routing_mode": "global"
        }))
        .unwrap();
        assert_eq!(new.routing_mode.as_deref(), Some("global"));
    }

    #[test]
    fn apply_bodies_carry_the_clients_selection_and_pins() {
        let profile = serde_json::json!({ "revision": "r1" });
        let choices = ApplyChoices {
            selected_node_id: Some("n2".into()),
            pins: vec![crate::protocol::IngressPin {
                node_id: "n1".into(),
                endpoint_key: "k2".into(),
            }],
        };
        assert_eq!(
            apply_body(&profile, &[], Some("rules"), &choices),
            serde_json::json!({ "profile": { "revision": "r1" }, "routing_mode": "rules",
                "selected_node_id": "n2", "pins": [{ "node_id": "n1", "endpoint_key": "k2" }] })
        );

        // Both payloads read them, and default to none (older clients).
        let connect: ConnectPayload = serde_json::from_value(serde_json::json!({
            "session_id": "a", "generation": 1, "profile": {}, "take_over": false,
            "selected_node_id": "n2", "pins": [{ "node_id": "n1", "endpoint_key": "k2" }]
        }))
        .unwrap();
        assert_eq!(connect.choices, choices);
        assert_eq!(connect.session.generation, 1);
        let update: UpdateProfilePayload = serde_json::from_value(serde_json::json!({
            "session_id": "a", "generation": 1, "profile": {},
            "selected_node_id": "n2", "pins": [{ "node_id": "n1", "endpoint_key": "k2" }]
        }))
        .unwrap();
        assert_eq!(update.choices, choices);
        let old: UpdateProfilePayload = serde_json::from_value(serde_json::json!({
            "session_id": "a", "generation": 1, "profile": {}
        }))
        .unwrap();
        assert_eq!(old.choices, ApplyChoices::default());
    }

    #[test]
    fn status_hides_the_session_from_other_users() {
        let owned = session("a", 3);
        let view = || CoreView {
            pid: 42,
            session: &owned,
            owner_user: Some("501"),
            profile_revision: "r1",
            lease_remaining: Duration::from_secs(30),
        };
        let own = describe(Some(view()), Some("501"));
        assert!(own.running);
        assert_eq!(own.session_id.as_deref(), Some("a"));
        assert_eq!(own.generation, 3);
        assert_eq!(own.pid, 42);

        let other = describe(Some(view()), Some("502"));
        assert!(other.running);
        assert_eq!(other.session_id, None);
        assert_eq!(other.generation, 0);
        assert_eq!(other.pid, -1);
        assert_eq!(other.profile_revision, None);
        assert_eq!(other.lease_expires_at_ms, None);

        let unknown = describe(Some(view()), None);
        assert_eq!(unknown.session_id, None);
        assert!(!describe(None, Some("501")).running);

        // Every status names this build, idle, owned or foreign.
        for status in [own, other, unknown, describe(None, Some("501"))] {
            assert_eq!(status.service_build_id, crate::protocol::SERVICE_BUILD_ID);
        }
    }

    #[test]
    fn build_id_is_a_sha256_hex_digest() {
        let id = crate::protocol::SERVICE_BUILD_ID;
        assert_eq!(id.len(), 64, "{id}");
        assert!(id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    }

    #[test]
    fn a_debug_log_flag_file_turns_on_the_debug_log() {
        let dir = std::env::temp_dir().join(format!(
            "pc-{}",
            &uuid::Uuid::new_v4().simple().to_string()[..8]
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("state");
        assert_eq!(debug_log_flag(&state), None);
        std::fs::write(dir.join("debug-log"), "").unwrap();
        assert_eq!(debug_log_flag(&state), Some(dir.join("debug-log")));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn only_wfp_or_unclassified_leftovers_on_windows_end_the_process() {
        let leftover = |kind: &str| Leftover {
            kind: kind.to_string(),
            name: "x".to_string(),
            detail: String::new(),
        };
        assert!(needs_process_restart(&[leftover("wfp")], true));
        assert!(needs_process_restart(
            &[leftover("route"), leftover("runtime")],
            true
        ));
        assert!(!needs_process_restart(&[leftover("route")], true));
        assert!(!needs_process_restart(&[], true));
        assert!(!needs_process_restart(&[leftover("wfp")], false));
    }

    // --- Lifecycle, on fake instances ---------------------------------------

    /// How each fake instance behaves, by launch order.
    #[derive(Clone, Copy, Default)]
    pub(crate) struct FakePlan {
        /// `/v1/start` fails.
        pub(crate) fail_start: bool,
        pub(crate) fail_path: &'static str,
        pub(crate) omit_schema: bool,
        /// This Core API path answers only after `slow_ms` (or once the
        /// instance shut down, or at the call's timeout).
        pub(crate) slow_path: &'static str,
        pub(crate) slow_ms: u64,
    }

    #[derive(Default)]
    struct FakeShared {
        plans: Vec<FakePlan>,
        events: Vec<String>,
        instances: Vec<Arc<FakeInstance>>,
    }

    /// The instances a [`fake_manager`] launched, and what they were asked.
    #[derive(Clone, Default)]
    pub(crate) struct FakeEngines(Arc<Mutex<FakeShared>>);

    impl FakeEngines {
        pub(crate) fn events(&self) -> Vec<String> {
            self.0.lock().events.clone()
        }

        /// The newest instance reaches `Fatal`.
        pub(crate) fn fail_last(&self) {
            if let Some(instance) = self.0.lock().instances.last() {
                *instance.fatal.lock() = Some("\"kernel_unrecoverable\"".to_string());
            }
        }

        fn record(&self, event: String) {
            self.0.lock().events.push(event);
        }
    }

    struct FakeInstance {
        index: usize,
        plan: FakePlan,
        engines: FakeEngines,
        fatal: Mutex<Option<String>>,
        shut_down: std::sync::atomic::AtomicBool,
    }

    impl FakeInstance {
        fn is_shut_down(&self) -> bool {
            self.shut_down.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Instance for FakeInstance {
        fn call(&self, path: &str, _body: Value, timeout: Duration) -> Result<Value> {
            let index = self.index;
            if self.is_shut_down() {
                return Err(anyhow!("ENGINE_SHUT_DOWN: engine is shut down"));
            }
            if path == self.plan.slow_path {
                self.engines.record(format!("{index} {path} begun"));
                let started = Instant::now();
                let wait = Duration::from_millis(self.plan.slow_ms);
                while started.elapsed() < wait {
                    if self.is_shut_down() {
                        return Err(anyhow!("ENGINE_SHUT_DOWN: engine is shut down"));
                    }
                    if started.elapsed() >= timeout {
                        return Err(anyhow!("CORE_IPC_TIMEOUT: {path} after {timeout:?}"));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            self.engines.record(format!("{index} {path}"));
            if path == self.plan.fail_path {
                return Err(anyhow!("PERMISSION_DENIED: denied by OS"));
            }
            match path {
                "/v1/get-version" if self.plan.omit_schema => Ok(serde_json::json!({})),
                "/v1/get-version" => Ok(serde_json::json!({
                    "core_version": "fake", "core_api_version": 1, "profile_schema_version": 1
                })),
                "/v1/start" if self.plan.fail_start => {
                    Err(anyhow!("CORE_OPERATION_FAILED: core operation failed"))
                }
                "/v1/get-status" => Ok(serde_json::json!({ "instance": index.to_string() })),
                _ => Ok(serde_json::json!({})),
            }
        }

        fn fatal(&self) -> Option<String> {
            self.fatal.lock().clone()
        }

        fn shutdown(&self) -> Vec<Leftover> {
            self.shut_down
                .store(true, std::sync::atomic::Ordering::SeqCst);
            self.engines.record(format!("{} shutdown", self.index));
            Vec::new()
        }
    }

    struct FakeLauncher(FakeEngines);

    impl Launcher for FakeLauncher {
        fn launch(&self, _state: &Path, _debug: bool) -> Result<Arc<dyn Instance>> {
            let mut shared = self.0 .0.lock();
            let index = shared.instances.len() + 1;
            let plan = shared.plans.get(index - 1).copied().unwrap_or_default();
            if plan.fail_path == "create" {
                return Err(anyhow!("WINTUN_UNAVAILABLE: driver unavailable"));
            }
            let instance = Arc::new(FakeInstance {
                index,
                plan,
                engines: self.0.clone(),
                fatal: Mutex::new(None),
                shut_down: Default::default(),
            });
            shared.instances.push(instance.clone());
            shared.events.push(format!("{index} created"));
            Ok(instance)
        }
    }

    /// A manager whose instances are fakes that follow `plans`.
    pub(crate) fn fake_manager(plans: Vec<FakePlan>) -> (CoreManager, FakeEngines) {
        let engines = FakeEngines::default();
        engines.0.lock().plans = plans;
        let manager = CoreManager {
            launcher: Box::new(FakeLauncher(engines.clone())),
            state_dir: std::env::temp_dir().join("ppvpn-service-test-unused"),
            ..CoreManager::default()
        };
        (manager, engines)
    }

    pub(crate) fn connect_payload(session: &SessionRef) -> ConnectPayload {
        ConnectPayload {
            session: session.clone(),
            profile: serde_json::json!({ "revision": "r1", "schema_version": 1 }),
            take_over: false,
            allowed_rule_set_hosts: vec!["api.example.com".into()],
            routing_mode: Some("rules".into()),
            choices: ApplyChoices::default(),
        }
    }

    pub(crate) fn new_session(generation: u64) -> SessionRef {
        session(&uuid::Uuid::new_v4().to_string(), generation)
    }

    #[test]
    fn startup_failures_keep_the_code_and_identify_the_stage() {
        for (path, stage) in [
            ("create", "create"),
            ("/v1/get-version", "version"),
            ("/v1/apply-profile", "profile"),
            ("/v1/start", "start"),
        ] {
            let (mut manager, engines) = fake_manager(vec![FakePlan {
                fail_path: path,
                ..Default::default()
            }]);
            let error = manager
                .connect(connect_payload(&new_session(1)), std::process::id())
                .unwrap_err()
                .to_string();
            let code = if stage == "create" {
                "WINTUN_UNAVAILABLE"
            } else {
                "PERMISSION_DENIED"
            };
            assert!(error.starts_with(&format!("{code}:")), "{error}");
            assert!(
                error.ends_with(&format!("[start_stage={stage}]")),
                "{error}"
            );
            assert!(manager.state.is_none());
            if stage != "create" {
                assert!(engines.events().contains(&"1 shutdown".to_string()));
            }
        }
        let error = start_error("profile", anyhow!("PROFILE_SCHEMA_UNSUPPORTED")).to_string();
        assert!(error.starts_with("PROFILE_SCHEMA_UNSUPPORTED:"));
    }

    #[test]
    fn schema_handshake_failures_keep_their_stage_and_stop_the_instance() {
        for missing in [false, true] {
            let (mut manager, engines) = fake_manager(vec![FakePlan {
                omit_schema: missing,
                ..Default::default()
            }]);
            let mut body = connect_payload(&new_session(1));
            body.profile["schema_version"] = serde_json::json!(2);
            let error = manager
                .connect(body, std::process::id())
                .unwrap_err()
                .to_string();
            let (code, stage) = if missing {
                ("CORE_SCHEMA_CAPABILITY_MISSING", "version")
            } else {
                ("PROFILE_SCHEMA_UNSUPPORTED", "profile")
            };
            assert!(error.starts_with(&format!("{code}:")), "{error}");
            assert!(
                error.ends_with(&format!("[start_stage={stage}]")),
                "{error}"
            );
            assert!(manager.state.is_none());
            assert!(engines.events().contains(&"1 shutdown".to_string()));
            assert!(!engines.events().contains(&"1 /v1/start".to_string()));
        }
    }

    #[test]
    fn a_failed_start_leaves_nothing_and_the_retry_gets_its_own_instance() {
        let failing = FakePlan {
            fail_start: true,
            ..FakePlan::default()
        };
        let (mut manager, engines) = fake_manager(vec![failing]);
        let me = std::process::id();
        let first = new_session(1);
        let error = manager
            .connect(connect_payload(&first), me)
            .unwrap_err()
            .to_string();
        assert_eq!(
            error,
            "CORE_OPERATION_FAILED: core operation failed [start_stage=start]"
        );
        assert!(manager.state.is_none(), "the failed instance is gone");
        assert!(engines.events().contains(&"1 shutdown".to_string()));

        let retry = new_session(2);
        manager.connect(connect_payload(&retry), me).unwrap();
        let status = manager
            .call_api(&retry, "/v1/get-status", serde_json::json!({}), me)
            .unwrap();
        assert_eq!(status["instance"], "2", "calls reach the new instance");

        // The failed attempt's session can neither use nor stop it.
        assert_eq!(
            manager
                .call_api(&first, "/v1/get-status", serde_json::json!({}), me)
                .unwrap_err()
                .to_string(),
            "STALE_OR_FOREIGN_SESSION"
        );
        assert!(manager.disconnect(&first, me).is_err());
        assert!(!engines.events().contains(&"2 shutdown".to_string()));

        manager.disconnect(&retry, me).unwrap();
        let events = engines.events();
        assert_eq!(
            &events[events.len() - 5..],
            [
                "2 /v1/get-version",
                "2 /v1/apply-profile",
                "2 /v1/start",
                "2 /v1/get-status",
                "2 shutdown"
            ]
        );
    }

    #[test]
    fn a_failed_instance_reads_as_expired_and_is_replaced() {
        let (mut manager, engines) = fake_manager(vec![]);
        let me = std::process::id();
        let first = new_session(1);
        manager.connect(connect_payload(&first), me).unwrap();
        engines.fail_last();
        assert!(!manager.status(me).running);
        assert_eq!(
            manager.renew_lease(&first, me).unwrap_err().to_string(),
            "CONNECTION_LEASE_EXPIRED"
        );
        // Another session (even without take_over) replaces it.
        let second = new_session(2);
        manager.connect(connect_payload(&second), me).unwrap();
        assert!(engines.events().contains(&"1 shutdown".to_string()));
        assert!(manager.status(me).running);
        manager.stop().unwrap();
    }

    #[test]
    fn disconnect_does_not_wait_for_a_slow_core_call() {
        // A speed test in flight when the user turns enhanced mode off.
        let slow = FakePlan {
            slow_path: "/v1/probe-entrances",
            slow_ms: 20_000,
            ..FakePlan::default()
        };
        let (manager, engines) = fake_manager(vec![slow]);
        let core = Mutex::new(manager);
        let me = std::process::id();
        let session = new_session(1);
        core.lock().connect(connect_payload(&session), me).unwrap();

        std::thread::scope(|scope| {
            let probe = scope.spawn(|| {
                call_api(
                    &core,
                    &session,
                    "/v1/probe-entrances",
                    serde_json::json!({ "node_id": "n1", "timeout_ms": 10_000 }),
                    me,
                )
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while !engines
                .events()
                .contains(&"1 /v1/probe-entrances begun".to_string())
            {
                assert!(Instant::now() < deadline, "the probe never began");
                std::thread::sleep(Duration::from_millis(10));
            }

            // Status and lease renewal are not held up either.
            let started = Instant::now();
            assert!(core.lock().renew_lease(&session, me).is_ok());
            assert!(core.lock().status(me).running);
            core.lock().disconnect(&session, me).unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "disconnect took {:?}",
                started.elapsed()
            );
            assert!(engines.events().contains(&"1 shutdown".to_string()));
            // The probe fails with its instance gone instead of hanging.
            assert!(probe.join().unwrap().is_err());
        });
    }

    #[test]
    fn a_profile_update_reaches_only_the_instance_it_was_authorized_for() {
        let (manager, engines) = fake_manager(vec![]);
        let core = Mutex::new(manager);
        let me = std::process::id();
        let session = new_session(1);
        core.lock().connect(connect_payload(&session), me).unwrap();
        let update = UpdateProfilePayload {
            session: session.clone(),
            profile: serde_json::json!({ "revision": "r2", "schema_version": 1 }),
            allowed_rule_set_hosts: vec!["api.example.com".into()],
            routing_mode: Some("global".into()),
            choices: ApplyChoices::default(),
        };
        let status = update_profile(&core, update, me).unwrap();
        assert_eq!(status.profile_revision.as_deref(), Some("r2"));
        let applies = engines
            .events()
            .iter()
            .filter(|event| *event == "1 /v1/apply-profile")
            .count();
        assert_eq!(applies, 2, "connect and update");
        core.lock().stop().unwrap();
    }

    #[test]
    fn core_call_timeouts_are_bounded() {
        let none = serde_json::json!({});
        assert_eq!(
            call_timeout("/v1/get-status", &none),
            Duration::from_secs(2)
        );
        assert_eq!(
            call_timeout("/v1/get-traffic", &none),
            Duration::from_secs(2)
        );
        assert_eq!(
            call_timeout("/v1/list-nodes", &none),
            Duration::from_secs(5)
        );
        assert_eq!(
            call_timeout("/v1/apply-profile", &none),
            Duration::from_secs(30)
        );
        assert_eq!(
            call_timeout(
                "/v1/probe-entrances",
                &serde_json::json!({ "timeout_ms": 3000 })
            ),
            Duration::from_secs(11)
        );
        assert_eq!(
            call_timeout(
                "/v1/probe-availability",
                &serde_json::json!({ "timeout_ms": u64::MAX })
            ),
            Duration::from_secs(125)
        );
        for path in ALLOWED_PATHS {
            assert!(call_timeout(path, &none) <= Duration::from_secs(30));
        }
    }

    #[test]
    fn clients_may_pin_an_ingress_but_not_reach_the_rest_of_the_core_api() {
        assert!(ALLOWED_PATHS.contains(&"/v1/pin-ingress"));
        assert!(ALLOWED_PATHS.contains(&"/v1/sleep") && ALLOWED_PATHS.contains(&"/v1/wake"));
        assert!(!ALLOWED_PATHS.contains(&"/v1/set-system-proxy"));
    }
}
