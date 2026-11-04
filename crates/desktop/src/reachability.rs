//! Reachability diagnostics in the apps (#521 step A): ppvpn-account's
//! probe over the API host and the current profile's node hosts. The user's
//! "Network diagnostics" also asks the DoH resolvers; after a call fails to
//! reach the backend, a run with the system resolver only (at most one per
//! [`AUTOMATIC_INTERVAL`]) goes to the log, so the logs a user sends carry
//! it. The user's run can enable API-only DNS recovery for this session
//! when a TLS-verified alternate works and the system's answers do not.

use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ppvpn_account::reachability::{self, Mode, Report, Target};

use crate::session::ClientRef;
use crate::{Client, ConnectionPhase};

/// The automatic run's minimum spacing.
const AUTOMATIC_INTERVAL: Duration = Duration::from_secs(600);

/// When the automatic run last started, for the whole process.
static LAST_AUTOMATIC: Mutex<Option<Instant>> = Mutex::new(None);

impl Client {
    /// The user's run (#521): the system resolver and the DoH resolvers for
    /// the API host and the profile's node hosts, then TCP and TLS to each
    /// address. Logged as well. Takes up to about 15 seconds; never fails.
    pub async fn diagnose_reachability(&self) -> Report {
        let (revision, vpn_on) = self.reachability_connection();
        let mut report =
            reachability::run(self.reachability_targets(), Mode::Compare, vpn_on).await;
        // A tunnel used at any point invalidates the pre-VPN comparison.
        // The revision also catches an Off → On → Off round trip.
        let (after, vpn_on) = self.reachability_connection();
        report.vpn_on |= vpn_on || revision != after;
        report.api_dns_recovery = self.auth.api().recover_dns_from(&report);
        tracing::info!(report = %report.json(), "reachability, asked by the user");
        report
    }

    /// The automatic run after a call failed to reach the backend: the
    /// system resolver only (no third party learns which service the user
    /// uses without the user acting), into the log.
    pub(crate) fn diagnose_reachability_after_failure(&self) {
        if cfg!(test) {
            return;
        }
        {
            let mut last = LAST_AUTOMATIC
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if last.is_some_and(|at| at.elapsed() < AUTOMATIC_INTERVAL) {
                return;
            }
            *last = Some(Instant::now());
        }
        // The targets are read on the task: the caller may hold the
        // client's locks.
        let this = self.this.clone();
        self.runtime.spawn(async move {
            let Some(client) = ClientRef::upgrade(&this) else {
                return;
            };
            let (targets, vpn_on) = (client.reachability_targets(), client.vpn_on());
            let report = reachability::run(targets, Mode::SystemOnly, vpn_on).await;
            tracing::info!(report = %report.json(), "reachability after the backend was unreachable");
        });
    }

    fn reachability_targets(&self) -> Vec<Target> {
        let mut targets: Vec<Target> = reachability::api_target(&self.config.api_base)
            .into_iter()
            .collect();
        if let Some(profile) = self.profile_raw() {
            targets.extend(reachability::profile_targets(&profile));
        }
        targets
    }

    /// The tunnel may carry the probe's connections.
    fn vpn_on(&self) -> bool {
        self.reachability_connection().1
    }

    pub(crate) fn reachability_connection(&self) -> (u64, bool) {
        let snapshot = self.snapshot.lock().unwrap();
        let vpn_on = !matches!(
            snapshot.connection.phase,
            ConnectionPhase::Off | ConnectionPhase::Error
        );
        (self.connection_revision.load(Ordering::Relaxed), vpn_on)
    }
}
