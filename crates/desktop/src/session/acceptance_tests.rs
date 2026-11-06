//! Delayed successful ApplyProfile replies must retain their session provenance.

use super::*;
use crate::auth::test_support::{jwt, serve, token_set, MemoryPlatform};
use crate::core_ipc::{BoxFuture, CoreCallError, CoreTransport};
use crate::cores::test_support::{FakeLauncher, NoService};
use crate::standard::{CoreLauncher, Launched};
use crate::{ClientListener, ProbeResult, TrafficSample};
use serde_json::Value;
use std::sync::{mpsc, Mutex};
use std::time::Instant;
use tokio::sync::oneshot;

const PROFILE: &str = include_str!("../../../account/tests/fixtures/proxy-profile.json");
const REVISION: &str = "49e28b20a5732f48c1f9411e072d97691b4cfc8e1f1e79485f5869ad45915ff5";

struct Gate {
    entered: mpsc::Sender<()>,
    release: oneshot::Receiver<()>,
}

struct GatedLauncher {
    inner: Arc<FakeLauncher>,
    gate: Arc<Mutex<Option<Gate>>>,
}

struct GatedTransport {
    inner: Arc<dyn CoreTransport>,
    gate: Arc<Mutex<Option<Gate>>>,
}

impl CoreLauncher for GatedLauncher {
    fn launch(&self) -> BoxFuture<'_, Result<Launched, ClientErrorInfo>> {
        Box::pin(async move {
            let mut launched = self.inner.launch().await?;
            launched.transport = Arc::new(GatedTransport {
                inner: launched.transport,
                gate: self.gate.clone(),
            });
            Ok(launched)
        })
    }
}

impl CoreTransport for GatedTransport {
    fn call<'a>(
        &'a self,
        path: &'static str,
        body: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, CoreCallError>> {
        Box::pin(async move {
            let gate = (path == "/v1/apply-profile")
                .then(|| self.gate.lock().unwrap().take())
                .flatten();
            let result = self.inner.call(path, body, timeout).await;
            if let Some(gate) = gate {
                // The engine accepted the profile, but its successful reply
                // has not reached the controller yet.
                gate.entered.send(()).unwrap();
                let _ = tokio::time::timeout(Duration::from_secs(10), gate.release)
                    .await
                    .expect("test did not release the delayed apply");
            }
            result
        })
    }
}

struct Listener;

impl ClientListener for Listener {
    fn on_snapshot(&self, _: ClientSnapshot) {}
    fn on_probe_result(&self, _: ProbeResult) {}
    fn on_traffic(&self, _: TrafficSample) {}
}

struct Fixture {
    client: Arc<Client>,
    launcher: Arc<FakeLauncher>,
    data_dir: String,
    release: Option<oneshot::Sender<()>>,
}

impl Fixture {
    fn new(later: Vec<(&'static str, String)>) -> Self {
        let mut responses = vec![
            ("200 OK", r#"{"refresh_token":"jyr_prepared"}"#.into()),
            ("200 OK", token_set("jyr_active")),
            ("200 OK", r#"{"id":"u1","name":"Example"}"#.into()),
            ("200 OK", teams()),
            ("200 OK", PROFILE.into()),
        ];
        responses.extend(later);
        let (base, _) = serve(responses);
        let data_dir = crate::test_backend::temp_dir("ppvpn-accepted-profile");
        let launcher = FakeLauncher::new();
        let (entered, wait) = mpsc::channel();
        let (release, released) = oneshot::channel();
        let gated = Arc::new(GatedLauncher {
            inner: launcher.clone(),
            gate: Arc::new(Mutex::new(Some(Gate {
                entered,
                release: released,
            }))),
        });
        let client = Client::with_parts(
            crate::ClientConfig {
                api_base: base,
                data_dir: data_dir.clone(),
                log_dir: data_dir.clone(),
                platform: "macos".into(),
                app_version: "0.0.0-test".into(),
            },
            MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#),
            Arc::new(Listener),
            Arc::new(NoService),
            gated,
            launcher.writer.clone(),
        );
        wait.recv_timeout(Duration::from_secs(10)).unwrap();
        Self {
            client,
            launcher,
            data_dir,
            release: Some(release),
        }
    }

    fn finish_apply(&mut self) {
        self.release.take().unwrap().send(()).unwrap();
        // Wait for the real sync_cores transaction, including acceptance,
        // to leave its serialization lock; no timing sleeps for the race.
        block_on(async {
            let _guard = tokio::time::timeout(Duration::from_secs(10), self.client.cores.lock())
                .await
                .unwrap();
        });
    }

    fn saved(&self) -> Option<SavedProfile> {
        crate::saved_profile::load(&self.data_dir, &self.client.config.api_base)
    }

    fn assert_no_accepted_profile(&self) {
        assert!(
            self.client.session_state().accepted.is_none(),
            "old profile became the new context's fallback"
        );
        assert!(
            self.saved().is_none(),
            "old profile was persisted after invalidation"
        );
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.release.take());
        self.client.shutdown();
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn wait_for(client: &Client, what: impl Fn(&ClientSnapshot) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !what(&client.snapshot()) {
        assert!(
            Instant::now() < deadline,
            "timed out: {:?}",
            client.snapshot()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn teams() -> String {
    r#"{"items":[{"id":"t1","name":"Example","is_personal":true,"is_default":true}]}"#.into()
}

fn unavailable() -> (&'static str, String) {
    (
        "503 Service Unavailable",
        r#"{"code":"UNAVAILABLE"}"#.into(),
    )
}

fn no_subscription() -> (&'static str, String) {
    (
        "404 Not Found",
        r#"{"code":"SUBSCRIPTION_NOT_FOUND"}"#.into(),
    )
}

#[test]
fn late_acceptance_does_not_restore_a_switched_teams_cache() {
    // Even selecting the same ID again is a new team epoch.
    for team in ["t2", "t1"] {
        let response = format!(
            r#"{{"id":"{team}","name":"Example","is_personal":false,"token":"{}"}}"#,
            jwt(r#"{"aud":["ppvpn"]}"#)
        );
        let mut f = Fixture::new(vec![("200 OK", response), unavailable()]);
        let client = f.client.clone();
        let switched = std::thread::spawn(move || block_on(client.switch_team(team.into())));
        wait_for(&f.client, |snapshot| snapshot.profile.is_none());
        f.finish_apply();
        assert!(switched.join().unwrap().is_err());
        f.assert_no_accepted_profile();
    }
}

#[test]
fn late_acceptance_does_not_restore_a_replaced_accounts_cache() {
    let mut f = Fixture::new(vec![
        ("200 OK", r#"{"id":"u2","name":"Another"}"#.into()),
        ("200 OK", teams()),
        unavailable(),
    ]);
    let access = f.client.session_state().access.clone().unwrap();
    f.client.begin_session(
        SignedIn {
            access,
            account: None,
        },
        false,
    );
    wait_for(&f.client, |snapshot| {
        snapshot
            .account
            .as_ref()
            .is_some_and(|account| account.id == "u2")
            && snapshot.last_error.is_some()
    });
    f.finish_apply();
    f.assert_no_accepted_profile();
}

#[test]
fn late_acceptance_does_not_restore_a_revoked_profiles_cache() {
    let mut f = Fixture::new(vec![no_subscription()]);
    assert!(block_on(f.client.refresh_profile()).is_err());
    assert_eq!(
        f.client.snapshot().profile_status,
        ProfileStatus::NoSubscription
    );
    f.finish_apply();
    f.assert_no_accepted_profile();
}

fn superseded_first_apply(invalidate: bool) {
    let mut responses = Vec::new();
    if invalidate {
        responses.push(no_subscription());
    }
    responses.push(("200 OK", PROFILE.replace(REVISION, "r2")));
    let mut f = Fixture::new(responses);
    *f.launcher.reject_revision.lock().unwrap() = Some("r2".into());
    if invalidate {
        assert!(block_on(f.client.refresh_profile()).is_err());
    }
    block_on(f.client.refresh_profile()).unwrap();
    assert_eq!(f.client.snapshot().profile.unwrap().revision, "r2");
    f.finish_apply();
    wait_for(&f.client, |snapshot| {
        matches!(snapshot.profile_status, ProfileStatus::Invalid { .. })
    });
    assert!(
        matches!(f.client.snapshot().profile_status, ProfileStatus::Invalid { previous, .. } if previous != invalidate)
    );
    if invalidate {
        f.assert_no_accepted_profile();
    } else {
        assert_eq!(f.saved().unwrap().revision, REVISION);
        assert_eq!(
            f.client.current_profile().unwrap().raw.as_slice(),
            PROFILE.as_bytes()
        );
        // Reapplying this accepted revision for a routing-mode change
        // must keep the last-good cache available too.
        block_on(f.client.set_routing_mode(crate::RoutingMode::Global)).unwrap();
        assert_eq!(f.saved().unwrap().revision, REVISION);
    }
}

#[test]
fn an_invalidated_apply_cannot_become_fallback_after_profile_recovery() {
    superseded_first_apply(true);
}

#[test]
fn a_same_team_apply_remains_fallback_while_a_newer_revision_is_pending() {
    superseded_first_apply(false);
}

#[test]
fn refreshing_the_same_revision_does_not_discard_its_delayed_acceptance() {
    let mut f = Fixture::new(vec![("200 OK", PROFILE.into())]);
    block_on(f.client.refresh_profile()).unwrap();
    f.finish_apply();
    assert_eq!(f.saved().unwrap().raw.as_slice(), PROFILE.as_bytes());
}
