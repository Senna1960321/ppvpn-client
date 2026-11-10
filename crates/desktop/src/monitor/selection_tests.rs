use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::oneshot;

use super::*;
use crate::auth::test_support::MemoryPlatform;
use crate::core_ipc::{BoxFuture, CoreCallError, CoreTransport};
use crate::cores::test_support::{FakeLauncher, NoService};
use crate::errors::ClientErrorInfo;
use crate::standard::{CoreLauncher, Launched};
use crate::test_backend::{serve, temp_dir, wait_until, Backend};
use crate::{ClientConfig, ClientListener, ClientSnapshot, StandardState, TrafficSample};

type Gate = (oneshot::Sender<()>, oneshot::Receiver<()>);

struct GatedTransport {
    inner: Arc<dyn CoreTransport>,
    next_call: Arc<Mutex<Option<(&'static str, Gate)>>>,
}

impl GatedTransport {
    fn gate_call(&self, path: &'static str) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (entered, waiting) = oneshot::channel();
        let (release, proceed) = oneshot::channel();
        *self.next_call.lock().unwrap() = Some((path, (entered, proceed)));
        (waiting, release)
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
            let gate = {
                let mut next = self.next_call.lock().unwrap();
                if next.as_ref().is_some_and(|(expected, _)| *expected == path) {
                    next.take().map(|(_, gate)| gate)
                } else {
                    None
                }
            };
            if let Some((entered, proceed)) = gate {
                let _ = entered.send(());
                proceed.await.unwrap();
            }
            self.inner.call(path, body, timeout).await
        })
    }
}

struct GatedLauncher {
    inner: Arc<FakeLauncher>,
    transport: Arc<GatedTransport>,
}

impl CoreLauncher for GatedLauncher {
    fn launch(&self) -> BoxFuture<'_, Result<Launched, ClientErrorInfo>> {
        Box::pin(async move {
            let mut launched = self.inner.launch().await?;
            // Each launch has a distinct transport identity, as a real engine does.
            launched.transport = Arc::new(GatedTransport {
                inner: self.transport.inner.clone(),
                next_call: self.transport.next_call.clone(),
            });
            Ok(launched)
        })
    }
}

struct Recorder;

impl ClientListener for Recorder {
    fn on_snapshot(&self, _: ClientSnapshot) {}
    fn on_probe_result(&self, _: ProbeResult) {}
    fn on_traffic(&self, _: TrafficSample) {}
}

async fn ready_client() -> (Arc<Client>, Arc<GatedTransport>, String) {
    let backend = Backend::new();
    *backend.profile.lock().unwrap() =
        Some(include_str!("../../../account/tests/fixtures/proxy-profile.json").to_string());
    let base = serve(backend);
    let dir = temp_dir("ppvpn-monitor-selection");
    let inner = FakeLauncher::new();
    let transport = Arc::new(GatedTransport {
        inner: inner.core.clone(),
        next_call: Arc::new(Mutex::new(None)),
    });
    let client = Client::with_parts(
        ClientConfig {
            api_base: base,
            data_dir: dir.clone(),
            log_dir: dir.clone(),
            platform: "macos".into(),
            app_version: "0.0.0-test".into(),
        },
        MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#),
        Arc::new(Recorder),
        Arc::new(NoService),
        Arc::new(GatedLauncher {
            inner: inner.clone(),
            transport: transport.clone(),
        }),
        inner.writer.clone(),
    );
    wait_until(|| matches!(client.snapshot().standard, StandardState::Ready { .. }));
    // Exercise one real monitor tick explicitly, without a competing timer.
    let monitor = client.session_state().monitor_task.take();
    if let Some(task) = monitor {
        task.abort();
        let _ = task.await;
    }
    (client, transport, dir)
}

async fn core_selected(transport: &dyn CoreTransport) -> Option<String> {
    core_ipc::get_status(transport)
        .await
        .unwrap()
        .selected_node_id
}

#[tokio::test]
async fn pending_status_does_not_undo_a_completed_user_selection() {
    let (client, transport, dir) = ready_client().await;
    let old = client.selected_node().unwrap();
    let new = client
        .nodes()
        .into_iter()
        .find(|node| node.id != old)
        .unwrap()
        .id;
    let session = client.current_session().unwrap();
    let (entered, release) = transport.gate_call("/v1/get-status");
    let monitoring = client.clone();
    let tick = tokio::spawn(async move { monitoring.monitor_tick(session).await });
    tokio::time::timeout(Duration::from_secs(3), entered)
        .await
        .unwrap()
        .unwrap();

    client.select_node(new.clone()).await.unwrap();
    assert_eq!(client.selected_node(), Some(new.clone()));
    assert_eq!(core_selected(transport.as_ref()).await, Some(new.clone()));
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), tick)
        .await
        .unwrap()
        .unwrap();
    let core_selection = core_selected(transport.as_ref()).await;
    let ui_selection = client.selected_node();
    tokio::task::spawn_blocking(move || {
        client.shutdown();
        drop(client);
    })
    .await
    .unwrap();
    let _ = std::fs::remove_dir_all(dir);
    assert_eq!(ui_selection, Some(new.clone()));
    assert_eq!(
        core_selection,
        Some(new),
        "an old monitor tick restored the previous node"
    );
}

#[tokio::test]
async fn unchanged_selection_still_repairs_a_drifted_core() {
    let (client, transport, dir) = ready_client().await;
    let selected = client.selected_node().unwrap();
    let other = client
        .nodes()
        .into_iter()
        .find(|node| node.id != selected)
        .unwrap()
        .id;
    core_ipc::select_node(transport.as_ref(), &other)
        .await
        .unwrap();
    assert_eq!(core_selected(transport.as_ref()).await, Some(other));
    client.monitor_tick(client.current_session().unwrap()).await;
    let actual = core_selected(transport.as_ref()).await;
    tokio::task::spawn_blocking(move || {
        client.shutdown();
        drop(client);
    })
    .await
    .unwrap();
    let _ = std::fs::remove_dir_all(dir);
    assert_eq!(actual, Some(selected));
}

#[tokio::test]
async fn pending_repair_finishes_before_a_new_user_selection() {
    use std::future::Future;
    use std::task::Poll;

    let (client, transport, dir) = ready_client().await;
    let old = client.selected_node().unwrap();
    let new = client
        .nodes()
        .into_iter()
        .find(|node| node.id != old)
        .unwrap()
        .id;
    // Force the ordinary self-heal path, then delay its actual select RPC.
    core_ipc::select_node(transport.as_ref(), &new)
        .await
        .unwrap();
    let (entered, release) = transport.gate_call("/v1/select-node");
    let session = client.current_session().unwrap();
    let monitoring = client.clone();
    let tick = tokio::spawn(async move { monitoring.monitor_tick(session).await });
    tokio::time::timeout(Duration::from_secs(3), entered)
        .await
        .unwrap()
        .unwrap();
    {
        let select = client.select_node(new.clone());
        tokio::pin!(select);
        // Poll the user action before releasing the old repair. The corrected
        // implementation queues it behind only that short mutation section.
        let early = std::future::poll_fn(|cx| Poll::Ready(select.as_mut().poll(cx))).await;
        release.send(()).unwrap();
        match early {
            Poll::Ready(result) => result.unwrap(),
            Poll::Pending => select.await.unwrap(),
        }
    }
    tokio::time::timeout(Duration::from_secs(3), tick)
        .await
        .unwrap()
        .unwrap();
    let actual = core_selected(transport.as_ref()).await;
    tokio::task::spawn_blocking(move || {
        client.shutdown();
        drop(client);
    })
    .await
    .unwrap();
    let _ = std::fs::remove_dir_all(dir);
    assert_eq!(actual, Some(new));
}

#[tokio::test]
async fn pending_latency_probe_does_not_block_user_selection() {
    let (client, transport, dir) = ready_client().await;
    let old = client.selected_node().unwrap();
    let new = client
        .nodes()
        .into_iter()
        .find(|node| node.id != old)
        .unwrap()
        .id;
    client.session_state().latency = None;
    let (entered, release) = transport.gate_call("/v1/probe-entrances");
    let session = client.current_session().unwrap();
    let monitoring = client.clone();
    let tick = tokio::spawn(async move { monitoring.monitor_tick(session).await });
    tokio::time::timeout(Duration::from_secs(3), entered)
        .await
        .unwrap()
        .unwrap();
    let selected =
        tokio::time::timeout(Duration::from_secs(1), client.select_node(new.clone())).await;
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), tick)
        .await
        .unwrap()
        .unwrap();
    let actual = core_selected(transport.as_ref()).await;
    tokio::task::spawn_blocking(move || {
        client.shutdown();
        drop(client);
    })
    .await
    .unwrap();
    let _ = std::fs::remove_dir_all(dir);
    selected
        .expect("user selection waited for the latency probe")
        .unwrap();
    assert_eq!(actual, Some(new));
}

#[tokio::test]
async fn a_status_from_a_replaced_core_cannot_repair_the_new_core() {
    let (client, transport, dir) = ready_client().await;
    let selected = client.selected_node().unwrap();
    let other = client
        .nodes()
        .into_iter()
        .find(|node| node.id != selected)
        .unwrap()
        .id;
    let old_transport = client.standard.transport().unwrap();
    let session = client.current_session().unwrap();
    let (entered, release) = transport.gate_call("/v1/get-status");
    let monitoring = client.clone();
    let tick = tokio::spawn(async move { monitoring.monitor_tick(session).await });
    tokio::time::timeout(Duration::from_secs(3), entered)
        .await
        .unwrap()
        .unwrap();
    {
        let _sync = client.cores.lock().await;
        let current = client.current_profile().unwrap();
        client.standard.stop().await;
        client
            .standard
            .apply_profile(&current.raw, &current.revision)
            .await
            .unwrap();
    }
    let replacement = client.standard.transport().unwrap();
    assert!(!Arc::ptr_eq(&old_transport, &replacement));
    core_ipc::select_node(replacement.as_ref(), &other)
        .await
        .unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), tick)
        .await
        .unwrap()
        .unwrap();
    let actual = core_selected(replacement.as_ref()).await;
    tokio::task::spawn_blocking(move || {
        client.shutdown();
        drop(client);
    })
    .await
    .unwrap();
    let _ = std::fs::remove_dir_all(dir);
    assert_eq!(
        actual,
        Some(other),
        "old-core status repaired the replacement"
    );
}

#[tokio::test]
async fn pending_status_is_ignored_after_sign_out() {
    let (client, transport, dir) = ready_client().await;
    let session = client.current_session().unwrap();
    let (entered, release) = transport.gate_call("/v1/get-status");
    let monitoring = client.clone();
    let tick = tokio::spawn(async move { monitoring.monitor_tick(session).await });
    tokio::time::timeout(Duration::from_secs(3), entered)
        .await
        .unwrap()
        .unwrap();
    client.logout().await.unwrap();
    release.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), tick)
        .await
        .unwrap()
        .unwrap();
    let snapshot = client.snapshot();
    tokio::task::spawn_blocking(move || {
        client.shutdown();
        drop(client);
    })
    .await
    .unwrap();
    let _ = std::fs::remove_dir_all(dir);
    assert_eq!(snapshot.auth, crate::AuthState::SignedOut);
    assert!(snapshot.selected_node_id.is_none());
    assert!(matches!(snapshot.standard, StandardState::Stopped));
    assert_eq!(snapshot.connection.detail, ConnectionDetail::default());
}
