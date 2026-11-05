//! Standard mode: the in-process Rust engine ([`crate::engine`],
//! `Role::Standard`) owned by this app.
//!
//! It serves the per-node local HTTP/SOCKS5 proxies and every speed test
//! (ICMP/TCP entrance probes and Connect availability probes). It starts when
//! the first profile is applied and lives until sign-out or exit, independent
//! of enhanced mode: the privileged service runs the TUN instance only, so
//! local proxy ports stay stable while enhanced mode is toggled.
//!
//! The engine is supervised: an unexpected end (a Fatal state) recreates it
//! with backoff (at most [`RestartBudget::MAX_RESTARTS`] times per minute)
//! and re-applies the last profile; state changes go to the `on_state` sink.
//! Every apply carries the routing mode, the selected node and the ingress
//! pins ([`StandardSettings`]), so a recreated engine starts with the user's
//! choices.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::core_ipc::{self, ApplyChoices, BoxFuture, CoreTransport};
use crate::errors::{ClientError, ClientErrorInfo, ErrorCode};
use crate::ingress::PinsCell;
use crate::routing::RoutingModeCell;
use crate::session::SelectionSource;
use crate::{LocalProxy, ProbeMethod, ProbeResult, RoutingMode, StandardState};

const STOP_GRACE: Duration = Duration::from_secs(3);

/// Receives every standard-mode state change.
pub(crate) type StateSink = Arc<dyn Fn(StandardState) + Send + Sync>;

// ---------------------------------------------------------------------------
// Restart policy
// ---------------------------------------------------------------------------

/// Bounded crash-restart policy: exponential backoff, at most
/// `MAX_RESTARTS` restarts inside a sliding `WINDOW`.
#[derive(Debug, Default)]
pub(crate) struct RestartBudget {
    recent: VecDeque<Instant>,
}

impl RestartBudget {
    pub(crate) const MAX_RESTARTS: usize = 3;
    const WINDOW: Duration = Duration::from_secs(60);
    const BASE_DELAY: Duration = Duration::from_millis(500);

    /// Delay before the next restart, or `None` when the budget is spent.
    pub(crate) fn next_delay(&mut self, now: Instant) -> Option<Duration> {
        while self
            .recent
            .front()
            .is_some_and(|at| now.duration_since(*at) >= Self::WINDOW)
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= Self::MAX_RESTARTS {
            return None;
        }
        let delay = Self::BASE_DELAY * 2u32.pow(self.recent.len() as u32);
        self.recent.push_back(now);
        Some(delay)
    }

    pub(crate) fn reset(&mut self) {
        self.recent.clear();
    }

    /// Spends the whole budget at `now`: no automatic restart for a window.
    pub(crate) fn exhaust(&mut self, now: Instant) {
        self.recent = std::iter::repeat_n(now, Self::MAX_RESTARTS).collect();
    }
}

// ---------------------------------------------------------------------------
// Standard core
// ---------------------------------------------------------------------------

/// A launched core as seen by the controller.
pub(crate) struct Launched {
    pub transport: Arc<dyn CoreTransport>,
    /// Resolves with a status text once the core has ended, for any reason.
    pub exited: BoxFuture<'static, String>,
    /// Asks the core to shut down. Dropping it has the same effect.
    pub stop: oneshot::Sender<()>,
    /// `allowed_rule_set_hosts` for every `apply-profile` on this instance.
    pub rule_set_hosts: Vec<String>,
    /// The engine rebuilt the local proxy credentials when it was created
    /// (`status.local_proxy.credentials_reset`): apps holding the old ones
    /// must copy them again.
    pub credentials_reset: bool,
}

/// Starts one core instance. The production implementation creates the
/// engine ([`crate::engine::EngineLauncher`]); tests substitute a fake.
pub(crate) trait CoreLauncher: Send + Sync {
    fn launch(&self) -> BoxFuture<'_, Result<Launched, ClientErrorInfo>>;
}

/// Shared with the client.
#[derive(Clone, Default)]
pub(crate) struct StandardSettings {
    /// Sent with every `apply-profile`.
    pub routing: RoutingModeCell,
    /// Sent with every `apply-profile`.
    pub ingress_pins: PinsCell,
    /// Sent with every `apply-profile`.
    pub selection: SelectionSource,
    /// Told when a new engine reports [`Launched::credentials_reset`].
    pub on_credentials_reset: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[derive(Clone)]
struct Desired {
    profile: Arc<Value>,
    revision: String,
}

struct Running {
    instance: u64,
    transport: Arc<dyn CoreTransport>,
    rule_set_hosts: Vec<String>,
    applied_revision: Option<String>,
    /// `routing_mode` `applied_revision` was applied with.
    applied_mode: Option<RoutingMode>,
    started: bool,
    stop: Option<oneshot::Sender<()>>,
    supervisor: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct Slot {
    next_instance: u64,
    running: Option<Running>,
    desired: Option<Desired>,
    restarts: RestartBudget,
    state: StandardState,
}

struct Inner {
    launcher: Arc<dyn CoreLauncher>,
    on_state: StateSink,
    /// Serialises spawn / apply / stop so concurrent refreshes cannot start
    /// two cores.
    operation: tokio::sync::Mutex<()>,
    slot: Mutex<Slot>,
    settings: StandardSettings,
}

/// Handle to the standard-mode core; cheap to clone.
#[derive(Clone)]
pub(crate) struct StandardCore {
    inner: Arc<Inner>,
}

impl StandardCore {
    /// Creates the controller; nothing is launched until
    /// [`Self::apply_profile`].
    #[cfg(test)]
    pub(crate) fn with_launcher(launcher: Arc<dyn CoreLauncher>, on_state: StateSink) -> Self {
        Self::with_settings(launcher, on_state, StandardSettings::default())
    }

    /// [`Self::with_launcher`], applying profiles with `settings`.
    pub(crate) fn with_settings(
        launcher: Arc<dyn CoreLauncher>,
        on_state: StateSink,
        settings: StandardSettings,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                launcher,
                on_state,
                operation: tokio::sync::Mutex::new(()),
                slot: Mutex::new(Slot::default()),
                settings,
            }),
        }
    }

    /// Applies the desired profile again when the routing mode changed
    /// since it was applied (no respawn: the core swaps its runtime). A
    /// no-op without a profile or a running core.
    pub(crate) async fn reapply(&self) -> Result<(), ClientError> {
        let _operation = self.inner.operation.lock().await;
        let ready = self
            .inner
            .slot
            .lock()
            .map(|slot| slot.desired.is_some() && slot.running.is_some())
            .unwrap_or(false);
        if !ready {
            return Ok(());
        }
        self.inner.ensure_applied().await
    }

    pub(crate) fn state(&self) -> StandardState {
        self.inner
            .slot
            .lock()
            .map(|slot| slot.state.clone())
            .unwrap_or_default()
    }

    /// Ensures the core runs `profile` (JSON bytes) at `revision`, spawning it
    /// first when needed. Same revision on a live core is a no-op. The profile
    /// is remembered and re-applied after a crash restart.
    ///
    /// A new revision (or a first start) refills the crash-restart budget. The
    /// same revision after a failure is one more attempt only: it spawns or
    /// applies once, and a crash right after it fails again instead of
    /// restarting, so periodic retries of a broken core stay one per call.
    pub(crate) async fn apply_profile(
        &self,
        profile: &[u8],
        revision: &str,
    ) -> Result<(), ClientError> {
        if revision.is_empty() {
            return Err(ClientError::failed(
                ErrorCode::ProfileInvalid,
                "PROFILE_REVISION_REQUIRED",
            ));
        }
        let profile: Value = serde_json::from_slice(profile).map_err(|error| {
            ClientError::failed(
                ErrorCode::ProfileInvalid,
                format!("PROFILE_JSON_INVALID: {error}"),
            )
        })?;
        let _operation = self.inner.operation.lock().await;
        if let Ok(mut slot) = self.inner.slot.lock() {
            let retry = matches!(slot.state, StandardState::Failed { .. })
                && slot
                    .desired
                    .as_ref()
                    .is_some_and(|desired| desired.revision == revision);
            slot.desired = Some(Desired {
                profile: Arc::new(profile),
                revision: revision.to_string(),
            });
            if retry {
                slot.restarts.exhaust(Instant::now());
            } else {
                slot.restarts.reset();
            }
        }
        self.inner.ensure_applied().await
    }

    /// Stops the core gracefully and forgets the profile.
    pub(crate) async fn stop(&self) {
        let _operation = self.inner.operation.lock().await;
        let running = self.inner.slot.lock().ok().and_then(|mut slot| {
            slot.desired = None;
            slot.running.take()
        });
        let was_running = running.is_some();
        if let Some(running) = running {
            stop_instance(running).await;
        }
        if was_running || !matches!(self.state(), StandardState::Stopped) {
            self.inner.publish(StandardState::Stopped);
        }
    }

    /// Non-blocking stop for app exit: asks the core to shut down. Does not
    /// wait.
    pub(crate) fn shutdown(&self) {
        let running = self.inner.slot.lock().ok().and_then(|mut slot| {
            slot.desired = None;
            slot.running.take()
        });
        if let Some(mut running) = running {
            if let Some(stop) = running.stop.take() {
                let _ = stop.send(());
            }
        }
        self.inner.publish(StandardState::Stopped);
    }

    /// Core API transport of the ready core.
    pub(crate) fn transport(&self) -> Result<Arc<dyn CoreTransport>, ClientError> {
        let slot = self
            .inner
            .slot
            .lock()
            .map_err(|_| ClientError::StandardNotReady)?;
        match slot.running.as_ref() {
            Some(running) if running.started => Ok(running.transport.clone()),
            _ => Err(ClientError::StandardNotReady),
        }
    }

    /// Speed test through this core; see [`core_ipc::run_probe`]. Exactly one
    /// result per requested node reaches `on_result` before this returns.
    pub(crate) async fn probe(
        &self,
        method: ProbeMethod,
        node_ids: Vec<String>,
        on_result: &(dyn Fn(ProbeResult) + Send + Sync),
    ) -> Result<(), ClientError> {
        let transport = self.transport()?;
        core_ipc::run_probe(transport, method, node_ids, None, on_result).await
    }

    /// Per-node loopback proxies with their credentials.
    pub(crate) async fn local_proxies(&self) -> Result<Vec<LocalProxy>, ClientError> {
        let transport = self.transport()?;
        core_ipc::local_proxies(transport.as_ref())
            .await
            .map_err(|error| error.into_client_error(ErrorCode::StandardCoreFailed))
    }

    /// The routed local-proxy user (Profile rules, then the selected node).
    pub(crate) async fn routed_local_proxy(&self) -> Result<Option<LocalProxy>, ClientError> {
        let transport = self.transport()?;
        core_ipc::routed_local_proxy(transport.as_ref())
            .await
            .map_err(|error| error.into_client_error(ErrorCode::StandardCoreFailed))
    }
}

impl Inner {
    fn publish(&self, state: StandardState) {
        if let Ok(mut slot) = self.slot.lock() {
            slot.state = state.clone();
        }
        (self.on_state)(state);
    }

    fn fail(&self, error: ClientErrorInfo) -> ClientError {
        self.publish(StandardState::Failed {
            error: error.clone(),
        });
        error.into()
    }

    /// Spawns (if needed) and applies the desired profile. Caller holds
    /// `operation`.
    async fn ensure_applied(self: &Arc<Self>) -> Result<(), ClientError> {
        let Some(desired) = self.slot.lock().ok().and_then(|slot| slot.desired.clone()) else {
            return Err(ClientError::StandardNotReady);
        };
        let needs_spawn = self
            .slot
            .lock()
            .map(|slot| slot.running.is_none())
            .unwrap_or(true);
        if needs_spawn {
            self.publish(StandardState::Starting);
            let running = match self.spawn_instance().await {
                Ok(running) => running,
                Err(error) => return Err(self.fail(error)),
            };
            if let Ok(mut slot) = self.slot.lock() {
                slot.running = Some(running);
            }
        }
        let Some((transport, hosts, applied, applied_mode, mode, started)) =
            self.slot.lock().ok().and_then(|slot| {
                slot.running.as_ref().map(|r| {
                    (
                        r.transport.clone(),
                        r.rule_set_hosts.clone(),
                        r.applied_revision.clone(),
                        r.applied_mode,
                        Some(self.settings.routing.get()),
                        r.started,
                    )
                })
            })
        else {
            return Err(self.fail(ClientErrorInfo::new(
                ErrorCode::StandardCoreFailed,
                "STANDARD_CORE_EXITED",
            )));
        };
        if started && applied.as_deref() == Some(desired.revision.as_str()) && applied_mode == mode
        {
            self.publish(StandardState::Ready {
                revision: desired.revision,
            });
            return Ok(());
        }

        // ApplyProfile swaps a running runtime atomically and rolls back on
        // failure, so only the first revision needs an explicit start. The
        // selection and pins go with it: a new engine needs no SelectNode or
        // PinIngress afterwards.
        let choices = ApplyChoices::new(
            self.settings.selection.get(),
            &self.settings.ingress_pins.get(),
        );
        if let Err(error) = core_ipc::apply_profile(
            transport.as_ref(),
            &desired.profile,
            Some(hosts.as_slice()),
            mode,
            &choices,
        )
        .await
        {
            // A lost response can follow a successful remote apply. Do not
            // let the old cached mode turn compensation into a no-op.
            if matches!(&error, core_ipc::CoreCallError::Transport(_)) {
                if let Ok(mut slot) = self.slot.lock() {
                    if let Some(running) = slot.running.as_mut() {
                        running.applied_mode = None;
                    }
                }
            }
            let info = error.info(ErrorCode::StandardCoreFailed);
            return Err(match (started, applied) {
                // The previous revision keeps serving; report without
                // leaving the ready state.
                (true, Some(previous)) => {
                    self.publish(StandardState::Ready { revision: previous });
                    info.into()
                }
                _ => self.fail(info),
            });
        }
        if !started {
            if let Err(error) = core_ipc::start(transport.as_ref()).await {
                return Err(self.fail(error.info(ErrorCode::StandardCoreFailed)));
            }
        }
        if let Ok(mut slot) = self.slot.lock() {
            if let Some(running) = slot.running.as_mut() {
                running.started = true;
                running.applied_revision = Some(desired.revision.clone());
                running.applied_mode = mode;
            }
        }
        self.publish(StandardState::Ready {
            revision: desired.revision,
        });
        Ok(())
    }

    async fn spawn_instance(self: &Arc<Self>) -> Result<Running, ClientErrorInfo> {
        let launched = self.launcher.launch().await?;
        if launched.credentials_reset {
            if let Some(notify) = &self.settings.on_credentials_reset {
                notify();
            }
        }
        let instance = self
            .slot
            .lock()
            .map(|mut slot| {
                slot.next_instance += 1;
                slot.next_instance
            })
            .unwrap_or(0);
        let inner = Arc::downgrade(self);
        let exited = launched.exited;
        let supervisor = tokio::spawn(async move {
            let status = exited.await;
            if let Some(inner) = Weak::upgrade(&inner) {
                inner.on_exit(instance, status);
            }
        });
        Ok(Running {
            instance,
            transport: launched.transport,
            rule_set_hosts: launched.rule_set_hosts,
            applied_revision: None,
            applied_mode: None,
            started: false,
            stop: Some(launched.stop),
            supervisor: Some(supervisor),
        })
    }

    /// Called by the supervisor when an instance exited. Deliberate stops
    /// have already removed the instance from the slot and are ignored.
    fn on_exit(self: &Arc<Self>, instance: u64, status: String) {
        let delay = {
            let Ok(mut slot) = self.slot.lock() else {
                return;
            };
            if slot.running.as_ref().map(|r| r.instance) != Some(instance) {
                return;
            }
            slot.running = None;
            if slot.desired.is_some() {
                slot.restarts.next_delay(Instant::now())
            } else {
                None
            }
        };
        tracing::warn!(%status, "standard core ended unexpectedly");
        let Some(delay) = delay else {
            self.publish(StandardState::Failed {
                error: ClientErrorInfo::new(
                    ErrorCode::StandardCoreFailed,
                    format!("STANDARD_CORE_EXITED: {status}"),
                ),
            });
            return;
        };
        self.publish(StandardState::Starting);
        let inner = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let _operation = inner.operation.lock().await;
            let still_wanted = inner
                .slot
                .lock()
                .map(|slot| slot.running.is_none() && slot.desired.is_some())
                .unwrap_or(false);
            if still_wanted {
                if let Err(error) = inner.ensure_applied().await {
                    tracing::warn!(?error, "standard core restart failed");
                }
            }
        });
    }
}

async fn stop_instance(mut running: Running) {
    let _ = core_ipc::stop(running.transport.as_ref(), Duration::from_secs(2)).await;
    if let Some(stop) = running.stop.take() {
        let _ = stop.send(());
    }
    if let Some(supervisor) = running.supervisor.take() {
        let _ = tokio::time::timeout(STOP_GRACE + Duration::from_secs(2), supervisor).await;
    }
}

/// `YYYY-MM-DD` in UTC (log file names; no date crate needed).
pub(crate) fn utc_date(now: SystemTime) -> String {
    let days = now.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() / 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_budget_backs_off_and_refills_after_the_window() {
        let start = Instant::now();
        let mut budget = RestartBudget::default();
        assert_eq!(budget.next_delay(start), Some(Duration::from_millis(500)));
        assert_eq!(budget.next_delay(start), Some(Duration::from_millis(1000)));
        assert_eq!(budget.next_delay(start), Some(Duration::from_millis(2000)));
        assert_eq!(budget.next_delay(start), None);
        assert_eq!(
            budget.next_delay(start + Duration::from_secs(61)),
            Some(Duration::from_millis(500))
        );
        budget.reset();
        assert_eq!(budget.next_delay(start), Some(Duration::from_millis(500)));
    }

    #[test]
    fn utc_date_formats_known_days() {
        assert_eq!(utc_date(UNIX_EPOCH), "1970-01-01");
        assert_eq!(
            utc_date(UNIX_EPOCH + Duration::from_secs(951_782_400)),
            "2000-02-29"
        );
        assert_eq!(
            utc_date(UNIX_EPOCH + Duration::from_secs(1_790_640_000)),
            "2026-09-29"
        );
    }

    /// Launches fake cores whose exit can be triggered to simulate a crash.
    struct CrashyLauncher {
        core: Arc<crate::core_ipc::tests::FakeCore>,
        crash: Mutex<Vec<oneshot::Sender<()>>>,
        /// The next launches fail.
        fail: std::sync::atomic::AtomicBool,
        /// The next launches report rebuilt local proxy credentials.
        reset: std::sync::atomic::AtomicBool,
        launches: std::sync::atomic::AtomicUsize,
    }

    impl CrashyLauncher {
        fn new(core: Arc<crate::core_ipc::tests::FakeCore>) -> Arc<Self> {
            Arc::new(Self {
                core,
                crash: Mutex::new(Vec::new()),
                fail: Default::default(),
                reset: Default::default(),
                launches: Default::default(),
            })
        }

        fn launches(&self) -> usize {
            self.launches.load(std::sync::atomic::Ordering::SeqCst)
        }

        /// Crashes the newest instance.
        fn crash_last(&self) {
            if let Some(crash) = self.crash.lock().unwrap().pop() {
                let _ = crash.send(());
            }
        }
    }

    impl CoreLauncher for CrashyLauncher {
        fn launch(&self) -> BoxFuture<'_, Result<Launched, ClientErrorInfo>> {
            Box::pin(async move {
                self.launches
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                    return Err(ClientErrorInfo::new(
                        ErrorCode::StandardCoreFailed,
                        "test launch failure",
                    ));
                }
                let (stop, stopped) = oneshot::channel::<()>();
                let (crash, crashed) = oneshot::channel::<()>();
                self.crash.lock().unwrap().push(crash);
                Ok(Launched {
                    transport: self.core.clone(),
                    exited: Box::pin(async move {
                        tokio::select! {
                            _ = stopped => "stopped".to_string(),
                            _ = crashed => "signal: 9".to_string(),
                        }
                    }),
                    stop,
                    rule_set_hosts: vec!["api.example.com".to_string()],
                    credentials_reset: self.reset.load(std::sync::atomic::Ordering::SeqCst),
                })
            })
        }
    }

    #[tokio::test]
    async fn the_routed_proxy_comes_from_the_ready_core() {
        let core = crate::core_ipc::tests::FakeCore::new(|path, _| {
            let value = match path {
                "/v1/get-local-proxy-credential" => serde_json::json!({
                    "kind": "routed", "node_id": "", "listen": "127.0.0.1", "port": 7890,
                    "username": "abc", "password": "secret"
                }),
                _ => serde_json::json!({"applied": true}),
            };
            (Duration::ZERO, Ok(value))
        });
        let standard =
            StandardCore::with_launcher(CrashyLauncher::new(core.clone()), Arc::new(|_| {}));
        assert!(matches!(
            standard.routed_local_proxy().await,
            Err(ClientError::StandardNotReady)
        ));
        standard
            .apply_profile(br#"{"revision":"r1"}"#, "r1")
            .await
            .unwrap();
        let routed = standard.routed_local_proxy().await.unwrap().unwrap();
        assert_eq!((routed.username.as_str(), routed.port), ("abc", 7890));
        standard.stop().await;
    }

    #[tokio::test]
    async fn crash_restarts_and_reapplies_the_profile() {
        let core = crate::core_ipc::tests::FakeCore::new(|_, _| {
            (Duration::ZERO, Ok(serde_json::json!({"applied": true})))
        });
        let launcher = CrashyLauncher::new(core.clone());
        let standard = StandardCore::with_launcher(launcher.clone(), Arc::new(|_| {}));
        standard
            .apply_profile(br#"{"revision":"r1"}"#, "r1")
            .await
            .unwrap();
        assert!(matches!(standard.state(), StandardState::Ready { .. }));
        // Same revision again: no second apply.
        standard
            .apply_profile(br#"{"revision":"r1"}"#, "r1")
            .await
            .unwrap();
        let applies = |core: &crate::core_ipc::tests::FakeCore| {
            core.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|(path, _)| path == "/v1/apply-profile")
                .count()
        };
        assert_eq!(applies(&core), 1);

        let crash = launcher.crash.lock().unwrap().remove(0);
        let _ = crash.send(());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(matches!(standard.state(), StandardState::Starting));
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert!(
            matches!(standard.state(), StandardState::Ready { ref revision } if revision == "r1")
        );
        assert_eq!(applies(&core), 2, "profile re-applied after restart");
        assert_eq!(launcher.crash.lock().unwrap().len(), 1, "one new instance");

        standard.stop().await;
        assert!(matches!(standard.state(), StandardState::Stopped));
        assert!(matches!(
            standard.transport(),
            Err(ClientError::StandardNotReady)
        ));
    }

    #[tokio::test]
    async fn retrying_a_failed_core_makes_one_attempt_per_call() {
        let core = crate::core_ipc::tests::FakeCore::new(|_, _| {
            (Duration::ZERO, Ok(serde_json::json!({"applied": true})))
        });
        let launcher = CrashyLauncher::new(core.clone());
        let standard = StandardCore::with_launcher(launcher.clone(), Arc::new(|_| {}));
        let profile = br#"{"revision":"r1"}"#;

        // The first start fails.
        launcher
            .fail
            .store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(standard.apply_profile(profile, "r1").await.is_err());
        assert!(matches!(standard.state(), StandardState::Failed { .. }));
        assert_eq!(launcher.launches(), 1);

        // Still broken: each retry of the same revision launches once.
        assert!(standard.apply_profile(profile, "r1").await.is_err());
        assert_eq!(launcher.launches(), 2);

        // Fixed: the retry starts it.
        launcher
            .fail
            .store(false, std::sync::atomic::Ordering::SeqCst);
        standard.apply_profile(profile, "r1").await.unwrap();
        assert!(matches!(standard.state(), StandardState::Ready { .. }));
        assert_eq!(launcher.launches(), 3);

        // A crash right after a retry is not restarted automatically.
        launcher.crash_last();
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(matches!(standard.state(), StandardState::Failed { .. }));
        assert_eq!(launcher.launches(), 3, "no restart storm");

        // The next retry gets exactly one more launch.
        standard.apply_profile(profile, "r1").await.unwrap();
        assert!(matches!(standard.state(), StandardState::Ready { .. }));
        assert_eq!(launcher.launches(), 4);

        // A new revision refills the budget: a crash is restarted again.
        standard
            .apply_profile(br#"{"revision":"r2"}"#, "r2")
            .await
            .unwrap();
        launcher.crash_last();
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(
            matches!(standard.state(), StandardState::Ready { ref revision } if revision == "r2")
        );
        assert_eq!(launcher.launches(), 5);

        standard.stop().await;
    }

    fn apply_bodies(core: &crate::core_ipc::tests::FakeCore) -> Vec<serde_json::Value> {
        core.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|(path, _)| path == "/v1/apply-profile")
            .map(|(_, body)| body.clone())
            .collect()
    }

    #[tokio::test]
    async fn apply_pins_the_api_host() {
        let core = crate::core_ipc::tests::FakeCore::new(|_, _| {
            (Duration::ZERO, Ok(serde_json::json!({"applied": true})))
        });
        let standard =
            StandardCore::with_launcher(CrashyLauncher::new(core.clone()), Arc::new(|_| {}));
        standard
            .apply_profile(br#"{"revision":"r1"}"#, "r1")
            .await
            .unwrap();
        standard
            .apply_profile(br#"{"revision":"r2"}"#, "r2")
            .await
            .unwrap();
        let expected = |revision: &str| {
            serde_json::json!({
                "profile": {"revision": revision},
                "allowed_rule_set_hosts": ["api.example.com"],
                "routing_mode": "rules",
            })
        };
        assert_eq!(apply_bodies(&core), vec![expected("r1"), expected("r2")]);
        standard.stop().await;
    }

    #[tokio::test]
    async fn a_new_routing_mode_reapplies_the_same_revision() {
        let core = crate::core_ipc::tests::FakeCore::new(|_, _| {
            (Duration::ZERO, Ok(serde_json::json!({"applied": true})))
        });
        let launcher = CrashyLauncher::new(core.clone());
        let routing = RoutingModeCell::default();
        let settings = StandardSettings {
            routing: routing.clone(),
            ..StandardSettings::default()
        };
        let standard = StandardCore::with_settings(launcher, Arc::new(|_| {}), settings);
        // Nothing running yet: nothing to re-apply.
        standard.reapply().await.unwrap();
        assert!(apply_bodies(&core).is_empty());
        standard
            .apply_profile(br#"{"revision":"r1"}"#, "r1")
            .await
            .unwrap();
        routing.set(RoutingMode::Global);
        standard.reapply().await.unwrap();
        // Same revision, same mode: no call.
        standard.reapply().await.unwrap();
        standard
            .apply_profile(br#"{"revision":"r1"}"#, "r1")
            .await
            .unwrap();
        let modes: Vec<Value> = apply_bodies(&core)
            .into_iter()
            .map(|body| body["routing_mode"].clone())
            .collect();
        assert_eq!(
            modes,
            vec![serde_json::json!("rules"), serde_json::json!("global")]
        );
        standard.stop().await;
    }

    #[tokio::test]
    async fn every_apply_carries_the_selection_and_the_pins() {
        let core = crate::core_ipc::tests::FakeCore::new(|_, _| {
            (Duration::ZERO, Ok(serde_json::json!({"applied": true})))
        });
        let launcher = CrashyLauncher::new(core.clone());
        let selected = Arc::new(Mutex::new(Some("n1".to_string())));
        let pins = PinsCell::new([("n2".to_string(), "k3".to_string())].into());
        let settings = StandardSettings {
            ingress_pins: pins.clone(),
            selection: SelectionSource::new({
                let selected = selected.clone();
                move || selected.lock().unwrap().clone()
            }),
            ..StandardSettings::default()
        };
        let standard = StandardCore::with_settings(launcher.clone(), Arc::new(|_| {}), settings);
        standard
            .apply_profile(br#"{"revision":"r1"}"#, "r1")
            .await
            .unwrap();
        // Changed live since (SelectNode / PinIngress): a recreated engine
        // gets the current ones with the profile.
        *selected.lock().unwrap() = Some("n2".to_string());
        pins.set(Default::default());
        launcher.crash_last();
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(matches!(standard.state(), StandardState::Ready { .. }));

        let bodies = apply_bodies(&core);
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["selected_node_id"], "n1");
        assert_eq!(
            bodies[0]["pins"],
            serde_json::json!([{"node_id": "n2", "endpoint_key": "k3"}])
        );
        assert_eq!(bodies[1]["selected_node_id"], "n2");
        assert!(bodies[1].get("pins").is_none(), "no pins left");
        let calls = core.calls.lock().unwrap().clone();
        assert!(
            calls
                .iter()
                .all(|(path, _)| path != "/v1/select-node" && path != "/v1/pin-ingress"),
            "nothing re-sent after an apply: {calls:?}"
        );
        standard.stop().await;
    }

    #[tokio::test]
    async fn an_engine_that_rebuilt_the_credentials_says_so() {
        let core = crate::core_ipc::tests::FakeCore::new(|_, _| {
            (Duration::ZERO, Ok(serde_json::json!({"applied": true})))
        });
        let launcher = CrashyLauncher::new(core);
        let resets = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let settings = StandardSettings {
            on_credentials_reset: Some(Arc::new({
                let resets = resets.clone();
                move || {
                    resets.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            })),
            ..StandardSettings::default()
        };
        let standard = StandardCore::with_settings(launcher.clone(), Arc::new(|_| {}), settings);
        standard
            .apply_profile(br#"{"revision":"r1"}"#, "r1")
            .await
            .unwrap();
        assert_eq!(resets.load(std::sync::atomic::Ordering::SeqCst), 0);

        // The recreated engine rebuilt them.
        launcher
            .reset
            .store(true, std::sync::atomic::Ordering::SeqCst);
        launcher.crash_last();
        tokio::time::sleep(Duration::from_millis(800)).await;
        assert!(matches!(standard.state(), StandardState::Ready { .. }));
        assert_eq!(resets.load(std::sync::atomic::Ordering::SeqCst), 1);
        standard.stop().await;
    }

    #[tokio::test]
    async fn invalid_profile_json_is_rejected_before_launching() {
        let fake = crate::core_ipc::tests::FakeCore::new(|_, _| {
            (Duration::ZERO, Ok(serde_json::json!({"applied": true})))
        });
        let launcher = CrashyLauncher::new(fake);
        let core = StandardCore::with_launcher(launcher.clone(), Arc::new(|_| {}));
        let error = core.apply_profile(b"not json", "r1").await.unwrap_err();
        assert!(matches!(
            error,
            ClientError::Failed {
                code: ErrorCode::ProfileInvalid,
                ..
            }
        ));
        assert!(matches!(core.state(), StandardState::Stopped));
        assert_eq!(launcher.launches(), 0);
    }
}
