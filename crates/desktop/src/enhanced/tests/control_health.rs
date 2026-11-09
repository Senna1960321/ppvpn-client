//! Control health failures must not dismantle a working forwarder. The policy
//! controller uses its existing fake service, but its Disconnect shuts down a
//! real Sail-backed Engine. TCP/UDP traffic goes through that Engine's SOCKS
//! listener, so a mistaken recovery closes real established flows. No TUN,
//! system proxy, external network, administrator access or account is used.
use super::*;
use ppvpn_core::{
    ApplyRequest, Engine, EngineConfig, EngineState, Event, EventItem, EventKind, LocalProxyConfig,
    Platform, Role,
};
use std::collections::HashSet;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::AtomicU16;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::task::JoinHandle;

struct HttpFixture {
    address: SocketAddr,
    status: Arc<AtomicU16>,
    task: JoinHandle<()>,
}
impl HttpFixture {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let status = Arc::new(AtomicU16::new(200));
        let current = status.clone();
        let task = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let current = current.clone();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    loop {
                        let mut byte = [0];
                        if stream.read_exact(&mut byte).await.is_err() {
                            return;
                        }
                        head.push(byte[0]);
                        if head.ends_with(b"\r\n\r\n") {
                            break;
                        }
                        assert!(head.len() < 8192);
                    }
                    let status = current.load(Ordering::SeqCst);
                    let reason = if status == 200 {
                        "OK"
                    } else {
                        "Service Unavailable"
                    };
                    let response = format!("HTTP/1.1 {status} {reason}\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            address,
            status,
            task,
        }
    }
    fn url(&self) -> reqwest::Url {
        format!("http://{}{HEALTH_PATH}", self.address)
            .parse()
            .unwrap()
    }
}
impl Drop for HttpFixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[test]
fn server_responses_are_not_proxy_transport_failures() {
    let unavailable = || Ok(HealthEndpoint::Unavailable(503));
    for (direct, proxy) in [
        (unavailable(), unavailable()),
        (Ok(HealthEndpoint::Healthy), unavailable()),
        (unavailable(), Ok(HealthEndpoint::Healthy)),
    ] {
        let result = official_verdict(direct, proxy);
        assert!(is_api_unreachable(result.as_ref().unwrap_err()));
        assert_eq!(health_round(&result), Round::ApiUnreachable);
    }
    let failure = health_error("HEALTH_PROXY_PATH_FAILED");
    assert_eq!(
        official_verdict(unavailable(), Err(failure.clone())),
        Err(failure)
    );
    assert_eq!(
        proxied_health_verdict(&serde_json::json!({
            "success": false, "http_status":503, "error_code":"HTTP_STATUS"
        })),
        unavailable()
    );
    for status in [0, 404, 503] {
        assert!(proxied_health_verdict(&serde_json::json!({
            "success":false, "http_status":status, "error_code":"TLS_HANDSHAKE_FAILED"
        }))
        .is_err());
    }
    assert!(
        proxied_health_verdict(&serde_json::json!({
            "http_status":503, "error_code":"HTTP_STATUS"
        }))
        .is_err(),
        "incomplete result is not a completed HTTP exchange"
    );
    assert!(proxied_health_verdict(&serde_json::json!({
        "success":false, "http_status":404, "error_code":"HTTP_STATUS"
    }))
    .is_err());
}

#[tokio::test]
async fn a_server_503_still_requires_capture_and_fresh_counters() {
    let http = HttpFixture::new().await;
    http.status.store(503, Ordering::SeqCst);
    let response = direct_health_request(http.url()).await.unwrap();
    assert_eq!(response.endpoint, HealthEndpoint::Unavailable(503));
    let before = CoreTraffic {
        upload_bytes: 100,
        download_bytes: 0,
        measured_at: None,
    };
    let future = SystemTime::now() + Duration::from_secs(60);
    let unchanged = scripted_traffic(vec![
        (100, Some(future)),
        (100, Some(future)),
        (100, Some(future + ms(1))),
    ]);
    let error = checked_direct_health(
        unchanged.as_ref(),
        before,
        http.url(),
        &mut Steps::new("test", 1, true),
    )
    .await
    .unwrap_err();
    assert!(is_leak(&error));
    let stale = scripted_traffic(vec![(100, None)]);
    let error = checked_direct_health(
        stale.as_ref(),
        before,
        http.url(),
        &mut Steps::new("test", 1, true),
    )
    .await
    .unwrap_err();
    assert!(is_traffic_stale(&error));
    let captured = scripted_traffic(vec![(180, Some(future))]);
    assert_eq!(
        checked_direct_health(
            captured.as_ref(),
            before,
            http.url(),
            &mut Steps::new("test", 1, true)
        )
        .await
        .unwrap(),
        Ok(HealthEndpoint::Unavailable(503))
    );
}

async fn socks(
    credential: &ppvpn_core::LocalProxyCredential,
    command: u8,
    target: SocketAddr,
) -> (TcpStream, SocketAddr) {
    let mut stream = TcpStream::connect(("127.0.0.1", credential.port))
        .await
        .unwrap();
    stream.write_all(&[5, 1, 2]).await.unwrap();
    let mut reply = [0; 2];
    stream.read_exact(&mut reply).await.unwrap();
    assert_eq!(reply, [5, 2]);
    let mut auth = vec![1, credential.username.len() as u8];
    auth.extend_from_slice(credential.username.as_bytes());
    auth.push(credential.password.len() as u8);
    auth.extend_from_slice(credential.password.as_bytes());
    stream.write_all(&auth).await.unwrap();
    stream.read_exact(&mut reply).await.unwrap();
    assert!(reply == [1, 0], "ephemeral proxy authentication");
    let SocketAddr::V4(target) = target else {
        panic!("IPv4 loopback fixture")
    };
    let mut request = vec![5, command, 0, 1];
    request.extend_from_slice(&target.ip().octets());
    request.extend_from_slice(&target.port().to_be_bytes());
    stream.write_all(&request).await.unwrap();
    let mut response = [0; 10];
    stream.read_exact(&mut response).await.unwrap();
    assert_eq!(&response[..4], &[5, 0, 0, 1]);
    (
        stream,
        SocketAddr::from((
            Ipv4Addr::new(response[4], response[5], response[6], response[7]),
            u16::from_be_bytes([response[8], response[9]]),
        )),
    )
}

async fn tcp_roundtrip(stream: &mut TcpStream, payload: &[u8]) {
    stream
        .write_all(payload)
        .await
        .expect("held TCP survives control-health503");
    let mut got = vec![0; payload.len()];
    tokio::time::timeout(Duration::from_secs(2), stream.read_exact(&mut got))
        .await
        .expect("held TCP reply deadline")
        .expect("held TCP remains open");
    assert_eq!(got, payload);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dual_http503_keeps_established_tcp_udp_without_core_restart() {
    let h = harness(true);
    let platform = if cfg!(target_os = "windows") {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::Macos
    } else {
        Platform::Linux
    };
    let engine = Arc::new(
        Engine::new(
            EngineConfig::new(Role::Standard, platform, h.dir.join("forwarder"))
                .with_local_proxy(LocalProxyConfig::new().with_preferred_port(0)),
        )
        .await
        .unwrap(),
    );
    let mut profile: Value = serde_json::from_slice(include_bytes!(
        "../../../../../testdata/golden/contract/profiles/base.json"
    ))
    .unwrap();
    // Every route is loopback DIRECT; unused fixture node endpoints are also
    // loopback, so an incidental background check cannot contact the internet.
    let closed = tokio::net::TcpSocket::new_v4().unwrap();
    closed.bind("127.0.0.1:0".parse().unwrap()).unwrap();
    profile["nodes"].as_array_mut().unwrap().truncate(1);
    profile["nodes"][0]["ingresses"] = serde_json::json!([{"role":"primary","endpoint_key":"fixture","replica_ordinal":0,"protocol":"shadowsocks","endpoint":{"domain":"127.0.0.1","port":closed.local_addr().unwrap().port()},"credentials":{"shadowsocks":{"method":"2022-blake3-aes-128-gcm","user_key":"AAAAAAAAAAAAAAAAAAAAAA=="}},"capabilities":{"tcp":true,"udp":true}}]);
    profile["routing"] = serde_json::json!({"rules":[],"final":{"type":"direct"}});
    engine
        .apply(ApplyRequest::new(serde_json::to_vec(&profile).unwrap()))
        .await
        .unwrap();
    let mut lifecycle = engine.subscribe(&[EventKind::CoreStarted, EventKind::CoreStopped]);
    engine.start().await.unwrap();
    assert!(
        matches!(
            tokio::time::timeout(Duration::from_secs(2), lifecycle.recv())
                .await
                .unwrap(),
            Some(EventItem::Event {
                event: Event::CoreStarted { .. }
            })
        ),
        "the real lifecycle observer must see the initial core start"
    );
    *h.service.stop_engine_on_disconnect.lock().unwrap() = Some(engine.clone());
    assert!(matches!(
        engine.status().state,
        EngineState::Running | EngineState::Degraded { .. }
    ));
    let credential = engine.local_proxy_routed_credential().unwrap();
    let http = HttpFixture::new().await;
    let direct_url = http.url();
    let proxy_client = reqwest::Client::builder()
        .proxy(
            reqwest::Proxy::http(format!("http://127.0.0.1:{}", credential.port))
                .unwrap()
                .basic_auth(&credential.username, &credential.password),
        )
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    // Ensure localhost/NO_PROXY settings cannot silently bypass the real
    // listener in this fixture: the same proxy with wrong credentials refuses.
    let refused = reqwest::Client::builder()
        .proxy(
            reqwest::Proxy::http(format!("http://127.0.0.1:{}", credential.port))
                .unwrap()
                .basic_auth("invalid-fixture-user", "invalid-fixture-password"),
        )
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap()
        .get(direct_url.clone())
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status().as_u16(), 407);
    let dual_rounds = Arc::new(AtomicUsize::new(0));
    h.enhanced.set_official_steps(Arc::new({
        let dual_rounds = dual_rounds.clone();
        move || {
            let client = proxy_client.clone();
            let url = direct_url.clone();
            let dual_rounds = dual_rounds.clone();
            Box::pin(async move {
                let direct = direct_health_request(url.clone()).await.map(|r| r.endpoint);
                let proxy = match client.get(url).send().await {
                    Ok(response) => {
                        let status = response.status().as_u16();
                        let _ = response.bytes().await;
                        proxied_health_verdict(&serde_json::json!({
                            "success": (200..400).contains(&status),
                            "http_status": status,
                            "error_code": if (200..400).contains(&status) { "" } else { "HTTP_STATUS" }
                        }))
                    }
                    Err(_) => Err(health_error("HEALTH_PROXY_PATH_FAILED")),
                };
                if matches!(direct.as_ref(), Ok(HealthEndpoint::Unavailable(503)))
                    && matches!(proxy.as_ref(), Ok(HealthEndpoint::Unavailable(503))) {
                    dual_rounds.fetch_add(1, Ordering::SeqCst);
                }
                (direct, proxy)
            })
        }
    }));
    // Set the interval before the monitor reads its first sleep. Changing it
    // after network I/O cannot interrupt an already pending 30-second sleep.
    h.enhanced.set_health_interval(Duration::from_millis(20));
    h.enhanced.set_profile(PROFILE_R1, "r1").await.unwrap();
    h.enhanced.enable().await.unwrap();
    let owner = h.service.owner.lock().unwrap().clone();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let target = listener.local_addr().unwrap();
    let accepts = Arc::new(AtomicUsize::new(0));
    let accepted = accepts.clone();
    let tcp_task = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            accepted.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async move {
                let mut b = [0; 128];
                loop {
                    let n = stream.read(&mut b).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    if stream.write_all(&b[..n]).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    let (mut tcp, _) = socks(&credential, 1, target).await;
    let echo = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let udp_target = echo.local_addr().unwrap();
    let peers = Arc::new(Mutex::new(HashSet::new()));
    let seen = peers.clone();
    let udp_task = tokio::spawn(async move {
        let mut b = [0; 512];
        while let Ok((n, peer)) = echo.recv_from(&mut b).await {
            seen.lock().unwrap().insert(peer);
            echo.send_to(&b[..n], peer).await.unwrap();
        }
    });
    let (_udp_control, relay) = socks(&credential, 3, "0.0.0.0:0".parse().unwrap()).await;
    let udp = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let udp_packet = |payload: &[u8]| {
        let SocketAddr::V4(to) = udp_target else {
            unreachable!()
        };
        let mut packet = vec![0, 0, 0, 1];
        packet.extend_from_slice(&to.ip().octets());
        packet.extend_from_slice(&to.port().to_be_bytes());
        packet.extend_from_slice(payload);
        packet
    };
    // Establish both flows before the control endpoint becomes unavailable.
    tcp_roundtrip(&mut tcp, b"before").await;
    udp.send_to(&udp_packet(b"before"), relay).await.unwrap();
    let mut b = [0; 512];
    tokio::time::timeout(Duration::from_secs(2), udp.recv_from(&mut b))
        .await
        .unwrap()
        .unwrap();
    http.status.store(503, Ordering::SeqCst);
    let baseline = dual_rounds.load(Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(5);
    while dual_rounds.load(Ordering::SeqCst) < baseline + 4
        && h.service.disconnects.load(Ordering::SeqCst) == 0
    {
        assert!(Instant::now() < deadline, "four real dual503 rounds");
        tokio::time::sleep(ms(10)).await;
    }
    // A regressed policy may stop before it can complete four rounds. Wait
    // for its real stop, then require the held traffic itself to expose the
    // disruption (the mutation check must not pass on a setup timeout).
    if h.service.disconnects.load(Ordering::SeqCst) != 0 {
        let deadline = Instant::now() + Duration::from_secs(3);
        while engine.status().state != EngineState::Stopped {
            assert!(
                Instant::now() < deadline,
                "unexpected recovery stop settles"
            );
            tokio::time::sleep(ms(10)).await;
        }
    }
    tcp_roundtrip(&mut tcp, b"during503").await;
    let packet = udp_packet(b"during503");
    udp.send_to(&packet, relay).await.unwrap();
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), udp.recv_from(&mut b))
        .await
        .expect("held UDP survives503")
        .unwrap();
    assert_eq!(&b[..n], packet);
    assert_eq!(
        h.service.disconnects.load(Ordering::SeqCst),
        0,
        "API503 must not release the established session"
    );
    assert_eq!(h.service.owner.lock().unwrap().clone(), owner);
    assert_eq!(h.enhanced.state().phase, ConnectionPhase::On);
    assert!(is_api_unreachable(
        h.enhanced.state().reason.as_ref().unwrap()
    ));
    assert!(
        tokio::time::timeout(ms(20), lifecycle.recv())
            .await
            .is_err(),
        "control503 must not stop or restart the real core"
    );
    assert_eq!(accepts.load(Ordering::SeqCst), 1);
    assert_eq!(peers.lock().unwrap().len(), 1);
    let protected_rounds = dual_rounds.load(Ordering::SeqCst) - baseline;
    http.status.store(200, Ordering::SeqCst);
    wait_phase_reason(&h, |s| s.reason.is_none()).await;
    tcp_roundtrip(&mut tcp, b"after").await;
    let packet = udp_packet(b"after");
    udp.send_to(&packet, relay).await.unwrap();
    let (n, _) = tokio::time::timeout(Duration::from_secs(2), udp.recv_from(&mut b))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&b[..n], packet);
    assert_eq!(peers.lock().unwrap().len(), 1);
    assert_eq!(h.service.connects.load(Ordering::SeqCst), 1);
    println!("dual503 continuity: protected_rounds={} tcp_accepts={} udp_peers={} disconnects=0 core_restarts=0 api_recovered=true",
        protected_rounds, accepts.load(Ordering::SeqCst), peers.lock().unwrap().len());
    // Positive control: this fake service's explicit disconnect really ends
    // the same real runtime that carried the protected established traffic.
    h.enhanced.disable().await.unwrap();
    assert_eq!(engine.status().state, EngineState::Stopped);
    let mut end = [0];
    let closed = tokio::time::timeout(Duration::from_secs(2), tcp.read(&mut end))
        .await
        .unwrap();
    assert!(
        matches!(closed, Ok(0) | Err(_)),
        "explicit disconnect closes the held stream"
    );
    tcp_task.abort();
    udp_task.abort();
}
