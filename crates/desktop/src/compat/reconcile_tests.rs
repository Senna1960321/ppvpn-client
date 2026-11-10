use super::*;
use crate::core_ipc::tests::FakeCore;
use crate::core_ipc::BoxFuture;
use crate::standard::{CoreLauncher, Launched};
use crate::sysproxy::tests::FakeWriter;
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};

struct Launcher(Arc<dyn CoreTransport>);

impl CoreLauncher for Launcher {
    fn launch(&self) -> BoxFuture<'_, Result<Launched, ClientErrorInfo>> {
        Box::pin(async move {
            let (stop, stopped) = tokio::sync::oneshot::channel();
            Ok(Launched {
                transport: self.0.clone(),
                exited: Box::pin(async move {
                    let _ = stopped.await;
                    "stopped".into()
                }),
                stop,
                rule_set_hosts: vec![],
                credentials_reset: false,
            })
        })
    }
}

struct GatedCore {
    core: Arc<FakeCore>,
    pause_at: Mutex<Option<&'static str>>,
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

impl CoreTransport for GatedCore {
    fn call<'a>(
        &'a self,
        path: &'static str,
        body: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, CoreCallError>> {
        Box::pin(async move {
            let pause = {
                let mut pause_at = self.pause_at.lock().unwrap();
                if *pause_at == Some(path) {
                    pause_at.take();
                    true
                } else {
                    false
                }
            };
            let result = self.core.call(path, body, timeout).await;
            if pause {
                self.entered.notify_one();
                self.resume.notified().await;
            }
            result
        })
    }
}

#[derive(Default)]
struct Writer {
    fake: FakeWriter,
    fail_query: AtomicBool,
}

impl SystemProxyWriter for Writer {
    fn snapshot(&self) -> Result<Value, String> {
        self.fake.snapshot()
    }
    fn apply(&self, host: &str, port: u16) -> Result<(), String> {
        self.fake.apply(host, port)
    }
    fn restore(&self, saved: &Value) -> Result<(), String> {
        self.fake.restore(saved)
    }
    fn current_proxies(&self) -> Result<Vec<(String, u16)>, String> {
        if self.fail_query.load(Ordering::Relaxed) {
            Err("proxy settings temporarily unavailable".into())
        } else {
            self.fake.current_proxies()
        }
    }
}

struct Fixture {
    compat: Compat,
    writer: Arc<Writer>,
    core: Arc<FakeCore>,
    gate: Arc<GatedCore>,
    port: Arc<AtomicU16>,
    enabled: Arc<AtomicBool>,
    dir: PathBuf,
}

impl Fixture {
    async fn new() -> Self {
        let port = Arc::new(AtomicU16::new(7891));
        let enabled = Arc::new(AtomicBool::new(false));
        let (core_port, core_enabled) = (port.clone(), enabled.clone());
        let core = FakeCore::new(move |path, body| {
            if path == "/v1/set-system-proxy" {
                core_enabled.store(body["enabled"].as_bool().unwrap(), Ordering::Relaxed);
            }
            let status = serde_json::json!({
                "available": true,
                "enabled": core_enabled.load(Ordering::Relaxed),
                "listening": core_enabled.load(Ordering::Relaxed),
                "listen": "127.0.0.1", "port": core_port.load(Ordering::Relaxed),
            });
            let reply = match path {
                "/v1/get-status" => serde_json::json!({"system_proxy": status}),
                "/v1/set-system-proxy" => status,
                _ => serde_json::json!({"applied": true}),
            };
            (Duration::ZERO, Ok(reply))
        });
        let gate = Arc::new(GatedCore {
            core: core.clone(),
            pause_at: Mutex::new(None),
            entered: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        });
        let standard =
            StandardCore::with_launcher(Arc::new(Launcher(gate.clone())), Arc::new(|_| {}));
        standard
            .apply_profile(br#"{"revision":"r1"}"#, "r1")
            .await
            .unwrap();
        let writer = Arc::new(Writer::default());
        let dir = PathBuf::from(crate::test_backend::temp_dir("ppvpn-compat-reconcile"));
        let config = ClientConfig {
            api_base: "https://api.example.com".into(),
            data_dir: dir.to_string_lossy().into_owned(),
            log_dir: String::new(),
            platform: "test".into(),
            app_version: "test".into(),
        };
        let compat = Compat::new(&config, writer.clone(), standard, Arc::new(|_| {}));
        compat.connect().await.unwrap();
        Self {
            compat,
            writer,
            core,
            gate,
            port,
            enabled,
            dir,
        }
    }

    fn restart_at(&self, port: u16) {
        self.port.store(port, Ordering::Relaxed);
        self.enabled.store(false, Ordering::Relaxed);
    }

    fn apply_count(&self) -> usize {
        self.writer
            .fake
            .calls()
            .iter()
            .filter(|call| call.starts_with("apply "))
            .count()
    }

    fn assert_taken_over(&self, expected: &[(String, u16)]) {
        assert_eq!(self.writer.current_proxies().unwrap(), expected);
        let state = self.compat.inner.with_data(|data| data.state.clone());
        assert_eq!(state.phase, ConnectionPhase::Error);
        assert_eq!(state.reason.unwrap().code, ErrorCode::NetworkPathContended);
        assert!(state.retryable);
        assert!(self.compat.inner.with_data(|data| data.endpoint.is_none()));
        assert!(!self.compat.inner.backup_path.exists());
        assert!(!self.enabled.load(Ordering::Relaxed));
        assert_eq!(self.apply_count(), 1, "no OS rewrite after takeover");
    }

    async fn finish(&self) {
        self.compat.disconnect().await.unwrap();
        self.compat.inner.standard.stop().await;
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.compat.inner.standard.shutdown();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn a_moved_endpoint_leaves_another_apps_proxy_alone() {
    let fixture = Fixture::new().await;
    fixture.restart_at(7892);
    let other = vec![("127.0.0.1".into(), 6152)];
    *fixture.writer.fake.current.lock().unwrap() = Some(other.clone());
    fixture.compat.reconcile().await;
    fixture.assert_taken_over(&other);
    fixture.finish().await;
    assert_eq!(fixture.writer.current_proxies().unwrap(), other);
    assert!(!fixture
        .writer
        .fake
        .calls()
        .iter()
        .any(|call| call.starts_with("restore ")));
}

#[tokio::test]
async fn a_moved_endpoint_keeps_a_proxy_the_user_disabled_off() {
    let fixture = Fixture::new().await;
    fixture.restart_at(7892);
    *fixture.writer.fake.current.lock().unwrap() = Some(vec![]);
    fixture.compat.reconcile().await;
    fixture.assert_taken_over(&[]);
    fixture.finish().await;
    assert!(fixture.writer.current_proxies().unwrap().is_empty());
}

#[tokio::test]
async fn a_moved_endpoint_waits_until_ownership_can_be_checked() {
    let fixture = Fixture::new().await;
    fixture.restart_at(7892);
    fixture.writer.fail_query.store(true, Ordering::Relaxed);
    let calls_before = fixture.core.calls.lock().unwrap().len();
    fixture.compat.reconcile().await;
    assert_eq!(fixture.apply_count(), 1);
    assert_eq!(fixture.core.calls.lock().unwrap().len(), calls_before);
    assert!(!fixture.enabled.load(Ordering::Relaxed));
    assert!(fixture.compat.inner.backup_path.exists());
    assert_eq!(
        fixture.compat.inner.with_data(|data| data.endpoint.clone()),
        Some(("127.0.0.1".into(), 7891))
    );
    fixture.writer.fail_query.store(false, Ordering::Relaxed);
    fixture.compat.reconcile().await;
    assert_eq!(fixture.apply_count(), 2, "a later tick retries the move");
    fixture.finish().await;
}

#[tokio::test]
async fn our_proxy_moves_with_the_core_and_restores_on_disconnect() {
    let fixture = Fixture::new().await;
    fixture.restart_at(7892);
    fixture.compat.reconcile().await;
    assert_eq!(
        fixture.writer.current_proxies().unwrap(),
        vec![("127.0.0.1".into(), 7892)]
    );
    assert_eq!(
        fixture.compat.inner.with_data(|data| data.state.phase),
        ConnectionPhase::On
    );
    assert_eq!(fixture.apply_count(), 2);
    let backup: Backup =
        serde_json::from_slice(&std::fs::read(&fixture.compat.inner.backup_path).unwrap()).unwrap();
    assert_eq!(backup.endpoint, Some(("127.0.0.1".into(), 7892)));
    assert_eq!(backup.saved, serde_json::json!({"fake": "previous"}));
    fixture.finish().await;
    assert!(!fixture.compat.inner.backup_path.exists());
    assert!(!fixture.enabled.load(Ordering::Relaxed));
    assert!(fixture
        .writer
        .fake
        .calls()
        .iter()
        .any(|call| call == "restore {\"fake\":\"previous\"}"));
}

#[tokio::test]
async fn unchanged_endpoints_still_detect_takeover() {
    let fixture = Fixture::new().await;
    let other = vec![("127.0.0.1".into(), 6152)];
    *fixture.writer.fake.current.lock().unwrap() = Some(other.clone());
    fixture.compat.reconcile().await;
    fixture.assert_taken_over(&other);
    fixture.finish().await;
}

#[tokio::test]
async fn our_unchanged_endpoint_needs_no_os_rewrite() {
    let fixture = Fixture::new().await;
    fixture.compat.reconcile().await;
    assert_eq!(fixture.apply_count(), 1);
    assert!(fixture.enabled.load(Ordering::Relaxed));
    assert_eq!(
        fixture.compat.inner.with_data(|data| data.state.phase),
        ConnectionPhase::On
    );
    fixture.finish().await;
}

async fn takeover_during_core_call(path: &'static str) {
    let fixture = Fixture::new().await;
    fixture.restart_at(7892);
    *fixture.gate.pause_at.lock().unwrap() = Some(path);
    let other = vec![("127.0.0.1".into(), 6152)];
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(fixture.compat.reconcile(), async {
            fixture.gate.entered.notified().await;
            *fixture.writer.fake.current.lock().unwrap() = Some(other.clone());
            fixture.gate.resume.notify_one();
        });
    })
    .await
    .expect("the paused core response was released");
    fixture.assert_taken_over(&other);
    fixture.finish().await;
    assert_eq!(fixture.writer.current_proxies().unwrap(), other);
}

#[tokio::test]
async fn takeover_while_checking_core_status_prevents_migration() {
    takeover_during_core_call("/v1/get-status").await;
}

#[tokio::test]
async fn takeover_while_enabling_the_replacement_prevents_migration() {
    takeover_during_core_call("/v1/set-system-proxy").await;
}
