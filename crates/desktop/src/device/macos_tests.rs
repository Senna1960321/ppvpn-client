use super::*;

use crate::auth::test_support::MemoryPlatform;
use crate::cores::test_support::{FakeLauncher, NoService};
use crate::test_backend::{serve, temp_dir, wait_until, Backend, PUSH_TOKEN};
use crate::{AuthState, ClientConfig, ClientListener, ClientSnapshot, ProbeResult, TrafficSample};
use ppvpn_account::device::INSTALLATION_FILE;
use serde_json::{json, Value};
use std::sync::{Arc, Barrier};
use std::time::Duration;

struct Quiet;

impl ClientListener for Quiet {
    fn on_snapshot(&self, _: ClientSnapshot) {}
    fn on_probe_result(&self, _: ProbeResult) {}
    fn on_traffic(&self, _: TrafficSample) {}
}

fn saved_login() -> Arc<MemoryPlatform> {
    MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#)
}

fn client(base: &str, dir: &str, platform: Arc<MemoryPlatform>) -> Arc<Client> {
    Client::with_parts(
        ClientConfig {
            api_base: base.into(),
            data_dir: dir.into(),
            log_dir: dir.into(),
            platform: "macos".into(),
            app_version: "1.2.3".into(),
        },
        platform,
        Arc::new(Quiet),
        Arc::new(NoService),
        FakeLauncher::new(),
        Arc::new(crate::sysproxy::tests::FakeWriter::default()),
    )
}

fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

fn legacy_device(dir: &str, base: &str) -> String {
    let installation = installation_id(dir).unwrap();
    let agent = AgentFile {
        api_base: base.into(),
        device_id: 42,
        push_token: PUSH_TOKEN.into(),
        installation_id: installation.clone(),
    };
    write_private(&agent_file_path(dir), &serde_json::to_vec(&agent).unwrap()).unwrap();
    installation
}

fn saved_apns_token(dir: &str, token: &str) {
    write_private(
        &Path::new(dir).join(PUSH_TOKEN_FILE),
        &serde_json::to_vec(&OsPushToken {
            token: token.into(),
            bundle_id: Some("com.peakpassvpn.ppvpn.desktop".into()),
        })
        .unwrap(),
    )
    .unwrap();
}

fn pending_path(dir: &str) -> PathBuf {
    Path::new(dir).join(LEGACY_MACOS_DEVICE_FILE)
}

fn device_bodies(backend: &Backend) -> Vec<Value> {
    backend.device_bodies.lock().unwrap().clone()
}

fn wait_registered(client: &Client) {
    wait_until(|| client.session_state().device == DeviceState::Registered);
}

fn finish(client: Arc<Client>, dir: &str) {
    client.shutdown();
    drop(client);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn staging_removes_legacy_credentials_and_keeps_the_installation_and_os_token() {
    let dir = temp_dir("ppvpn-macos-stage");
    let installation = legacy_device(&dir, "https://api.example.test/");
    saved_apns_token(&dir, "apns-saved");
    std::fs::write(Path::new(&dir).join(AGENT_CURSOR_FILE), "{}").unwrap();
    std::fs::write(Path::new(&dir).join(crate::push_shown::SHOWN_FILE), "{}").unwrap();

    stage_legacy_macos_device(&dir);

    let pending: Value =
        serde_json::from_slice(&std::fs::read(pending_path(&dir)).unwrap()).unwrap();
    assert_eq!(
        pending,
        json!({"api_base":"https://api.example.test/","device_id":42})
    );
    assert!(!agent_file_path(&dir).exists());
    assert!(!Path::new(&dir).join(AGENT_CURSOR_FILE).exists());
    assert!(!Path::new(&dir).join(crate::push_shown::SHOWN_FILE).exists());
    assert_eq!(installation_id(&dir).unwrap(), installation);
    assert_eq!(read_os_token(&dir).unwrap().token, "apns-saved");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(pending_path(&dir))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    // Another offline launch must not discard the only remaining device id.
    stage_legacy_macos_device(&dir);
    assert!(pending_path(&dir).exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn upgrading_retires_the_legacy_device_and_registers_apns_with_the_saved_login() {
    let backend = Backend::new();
    let base = serve(backend.clone());
    let dir = temp_dir("ppvpn-macos-upgrade");
    let installation = legacy_device(&dir, &format!("{base}/"));
    saved_apns_token(&dir, "apns-upgrade");
    let platform = saved_login();
    let client = client(&base, &dir, platform.clone());

    wait_registered(&client);
    assert_eq!(client.snapshot().auth, AuthState::SignedIn);
    assert!(
        platform.stored().is_some(),
        "migration must preserve the saved sign-in"
    );
    assert_eq!(
        backend.requests_matching("DELETE"),
        ["DELETE /api/v1/devices/42"]
    );
    assert!(!pending_path(&dir).exists());
    let bodies = device_bodies(&backend);
    assert_eq!(bodies.len(), 1);
    assert_eq!(bodies[0]["platform"], "macos");
    assert_eq!(bodies[0]["token"], "apns-upgrade");
    assert_eq!(bodies[0]["bundle_id"], "com.peakpassvpn.ppvpn.desktop");
    assert!(bodies[0].get("installation_id").is_none());
    assert!(!agent_file_path(&dir).exists());
    assert_eq!(installation_id(&dir).unwrap(), installation);
    assert!(Path::new(&dir).join(INSTALLATION_FILE).exists());
    finish(client, &dir);
}

#[test]
fn failed_retirement_does_not_block_apns_and_retries_after_registration() {
    let backend = Backend::new();
    *backend.delete_device_status.lock().unwrap() = Some("503 Service Unavailable");
    let base = serve(backend.clone());
    let dir = temp_dir("ppvpn-macos-retire-retry");
    legacy_device(&dir, &base);
    saved_apns_token(&dir, "apns-retry");
    let client = client(&base, &dir, saved_login());

    wait_registered(&client);
    assert!(pending_path(&dir).exists());
    assert_eq!(device_bodies(&backend).len(), 1);
    *backend.delete_device_status.lock().unwrap() = None;
    block_on(client.ensure_device_registered(client.current_session().unwrap()));
    assert!(!pending_path(&dir).exists());
    assert_eq!(backend.requests_matching("DELETE").len(), 2);
    assert_eq!(
        device_bodies(&backend).len(),
        1,
        "retirement retry must not re-register APNs"
    );
    finish(client, &dir);
}

#[test]
fn timed_out_retirement_keeps_the_marker_and_still_registers_apns() {
    let backend = Backend::new();
    *backend.delete_device_delay.lock().unwrap() = Some(Duration::from_secs(6));
    let base = serve(backend.clone());
    let dir = temp_dir("ppvpn-macos-retire-timeout");
    legacy_device(&dir, &base);
    saved_apns_token(&dir, "apns-timeout");
    let client = client(&base, &dir, saved_login());

    wait_registered(&client);
    assert!(pending_path(&dir).exists());
    assert_eq!(device_bodies(&backend).len(), 1);
    *backend.delete_device_delay.lock().unwrap() = None;
    block_on(client.ensure_device_registered(client.current_session().unwrap()));
    assert!(!pending_path(&dir).exists());
    assert_eq!(backend.requests_matching("DELETE").len(), 2);
    finish(client, &dir);
}

#[test]
fn a_later_launch_retries_the_legacy_marker_without_agent_credentials() {
    let backend = Backend::new();
    *backend.delete_device_status.lock().unwrap() = Some("503 Service Unavailable");
    let base = serve(backend.clone());
    let dir = temp_dir("ppvpn-macos-retire-relaunch");
    legacy_device(&dir, &base);
    saved_apns_token(&dir, "apns-relaunch");
    let platform = saved_login();
    let first = client(&base, &dir, platform.clone());
    wait_registered(&first);
    assert!(pending_path(&dir).exists());
    assert!(!agent_file_path(&dir).exists());
    first.shutdown();
    drop(first);

    *backend.delete_device_status.lock().unwrap() = None;
    let second = client(&base, &dir, platform);
    wait_registered(&second);
    assert!(!pending_path(&dir).exists());
    assert_eq!(backend.requests_matching("DELETE").len(), 2);
    assert_eq!(
        device_bodies(&backend).len(),
        2,
        "each launch registers the saved APNs token"
    );
    finish(second, &dir);
}

#[test]
fn changing_the_backend_never_deletes_a_device_on_the_new_backend() {
    let backend = Backend::new();
    let base = serve(backend.clone());
    let dir = temp_dir("ppvpn-macos-retire-other-backend");
    legacy_device(&dir, "https://old.example.test");
    saved_apns_token(&dir, "apns-other-backend");
    let client = client(&base, &dir, saved_login());

    wait_registered(&client);
    assert!(backend.requests_matching("DELETE").is_empty());
    assert!(!agent_file_path(&dir).exists());
    assert_eq!(device_bodies(&backend)[0]["token"], "apns-other-backend");
    finish(client, &dir);
}

#[test]
fn signed_out_launch_stages_migration_and_caches_apns_without_network_requests() {
    let backend = Backend::new();
    let base = serve(backend.clone());
    let dir = temp_dir("ppvpn-macos-retire-signed-out");
    let installation = legacy_device(&dir, &base);
    let client = client(&base, &dir, Arc::new(MemoryPlatform::default()));
    wait_until(|| client.snapshot().auth == AuthState::SignedOut);

    client.set_push_token("apns-signed-out".into(), None);
    wait_until(|| read_os_token(&dir).is_some_and(|token| token.token == "apns-signed-out"));
    // Wait for the asynchronous callback to finish, including any network work.
    block_on(async {
        let _registration = client.device_registration.lock().await;
    });
    assert!(backend.requests.lock().unwrap().is_empty());
    assert!(pending_path(&dir).exists());
    assert!(!agent_file_path(&dir).exists());
    assert_eq!(installation_id(&dir).unwrap(), installation);
    finish(client, &dir);
}

#[test]
fn token_rotation_during_registration_keeps_only_the_latest_apns_token() {
    let backend = Backend::new();
    let base = serve(backend.clone());
    let dir = temp_dir("ppvpn-macos-token-rotation");
    let client = client(&base, &dir, saved_login());
    wait_until(|| client.snapshot().auth == AuthState::SignedIn);
    let gate = Arc::new(Barrier::new(2));
    backend
        .register_gates
        .lock()
        .unwrap()
        .insert("apns-old".into(), gate.clone());
    client.set_push_token("apns-old".into(), None);
    wait_until(|| {
        device_bodies(&backend)
            .iter()
            .any(|body| body["token"] == "apns-old")
    });

    client.set_push_token("apns-superseded".into(), None);
    client.set_push_token(
        "apns-latest".into(),
        Some("com.peakpassvpn.ppvpn.desktop".into()),
    );
    gate.wait();
    wait_until(|| {
        device_bodies(&backend)
            .iter()
            .any(|body| body["platform"] == "macos" && body["token"] == "apns-latest")
    });
    wait_registered(&client);
    assert_eq!(read_os_token(&dir).unwrap().token, "apns-latest");
    let bodies = device_bodies(&backend);
    assert_eq!(bodies.len(), 3);
    assert_eq!(bodies[0]["token"], "apns-old");
    assert_eq!(bodies[1], json!({"token":"apns-old"}));
    assert_eq!(bodies[2]["token"], "apns-latest");
    assert!(bodies.iter().all(|body| body["token"] != "apns-superseded"));

    block_on(client.logout()).unwrap();
    assert_eq!(device_bodies(&backend)[3], json!({"token":"apns-latest"}));
    assert_eq!(client.snapshot().auth, AuthState::SignedOut);
    assert!(backend.requests_matching("DELETE").is_empty());
    finish(client, &dir);
}

#[test]
fn logout_waits_for_in_flight_registration_then_unregisters_the_token() {
    let backend = Backend::new();
    let base = serve(backend.clone());
    let dir = temp_dir("ppvpn-macos-token-logout");
    let client = client(&base, &dir, saved_login());
    wait_until(|| client.snapshot().auth == AuthState::SignedIn);
    let gate = Arc::new(Barrier::new(2));
    backend
        .register_gates
        .lock()
        .unwrap()
        .insert("apns-before-logout".into(), gate.clone());
    client.set_push_token("apns-before-logout".into(), None);
    wait_until(|| !device_bodies(&backend).is_empty());

    block_on(async {
        let mut logout = std::pin::pin!(client.logout());
        let early = tokio::time::timeout(Duration::from_millis(100), &mut logout).await;
        // Release even on failure, so the mock server never leaves a blocked request.
        gate.wait();
        assert!(
            early.is_err(),
            "logout must wait until registration can be unregistered"
        );
        logout.await.unwrap();
    });
    let bodies = device_bodies(&backend);
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["platform"], "macos");
    assert_eq!(bodies[0]["token"], "apns-before-logout");
    assert_eq!(bodies[1], json!({"token":"apns-before-logout"}));
    assert_eq!(client.snapshot().auth, AuthState::SignedOut);
    // A token refresh after sign-out is cached for the next account, never registered now.
    client.set_push_token("apns-after-logout".into(), None);
    wait_until(|| read_os_token(&dir).is_some_and(|token| token.token == "apns-after-logout"));
    block_on(async {
        let _registration = client.device_registration.lock().await;
    });
    assert_eq!(device_bodies(&backend).len(), 2);
    finish(client, &dir);
}
