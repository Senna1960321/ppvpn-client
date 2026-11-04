use super::*;
use crate::reachability::{AddressCheck, HostReport, Lookup as LookupReport, Step};
use rustls::pki_types::PrivatePkcs8KeyDer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

pub(super) struct Inputs {
    pub lookups: Arc<dyn Fn() -> Vec<Lookup> + Send + Sync>,
    pub tls: Arc<rustls::ClientConfig>,
}

struct Cancelled(Arc<std::sync::atomic::AtomicUsize>);
impl Drop for Cancelled {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn pending(counter: Arc<std::sync::atomic::AtomicUsize>) -> Lookup {
    let guard = Cancelled(counter);
    Box::pin(async move {
        let _guard = guard;
        std::future::pending().await
    })
}

#[tokio::test]
async fn reqwest_preserves_host_and_sni_sends_one_post_and_resolves_new_connections() {
    let key = rcgen::generate_simple_self_signed(vec!["api.example.com".to_string()]).unwrap();
    let cert = key.cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.clone()],
            PrivatePkcs8KeyDer::from(key.signing_key.serialize_der()).into(),
        )
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    let tls = Arc::new(
        rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let received = requests.clone();
    let server = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let (acceptor, received) = (acceptor.clone(), received.clone());
            tokio::spawn(async move {
                let Ok(mut stream) = acceptor.accept(stream).await else {
                    return;
                };
                let sni = stream.get_ref().1.server_name().map(str::to_string);
                let mut bytes = Vec::new();
                loop {
                    let mut buf = [0; 2048];
                    let Ok(Ok(n)) =
                        tokio::time::timeout(Duration::from_secs(2), stream.read(&mut buf)).await
                    else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                    if bytes.ends_with(b"payload") {
                        break;
                    }
                }
                received
                    .lock()
                    .unwrap()
                    .push((sni, String::from_utf8(bytes).unwrap()));
                let _ = stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK",
                    )
                    .await;
            });
        }
    });
    let changed = Arc::new(AtomicBool::new(false));
    let cancelled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut dns = RecoveryDns::new(&format!("https://api.example.com:{port}")).unwrap();
    let mut proof = report();
    proof.hosts[0].port = port;
    assert!(dns.enable_from(&proof));
    dns.test_inputs = Some(Inputs {
        tls: tls.clone(),
        lookups: Arc::new({
            let (changed, cancelled) = (changed.clone(), cancelled.clone());
            move || {
                vec![
                    pending(cancelled.clone()),
                    answer(if changed.load(Ordering::SeqCst) {
                        "127.0.0.2"
                    } else {
                        "127.0.0.1"
                    }),
                ]
            }
        }),
    });
    let client = reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(Arc::new(dns))
        .use_preconfigured_tls((*tls).clone())
        .timeout(Duration::from_secs(2))
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    let post = || {
        client
            .post(format!("https://api.example.com:{port}/device/token"))
            .bearer_auth("synthetic-test-token")
            .body("payload")
            .send()
    };
    assert!(post().await.unwrap().status().is_success());
    tokio::task::yield_now().await;
    assert_eq!(cancelled.load(Ordering::SeqCst), 1);
    changed.store(true, Ordering::SeqCst);
    assert!(post().await.is_err());
    assert_eq!(requests.lock().unwrap().len(), 1);
    changed.store(false, Ordering::SeqCst);
    assert!(post().await.unwrap().status().is_success());
    // Even a client that trusts this test certificate must send no HTTP
    // when the resolver's preflight does not trust it.
    let mut rejected = RecoveryDns::new(&format!("https://api.example.com:{port}")).unwrap();
    assert!(rejected.enable_from(&proof));
    rejected.test_inputs = Some(Inputs {
        tls: Arc::new(reachability::tls_config().unwrap()),
        lookups: Arc::new(|| vec![answer("127.0.0.1")]),
    });
    let rejected = reqwest::Client::builder()
        .no_proxy()
        .dns_resolver(Arc::new(rejected))
        .use_preconfigured_tls((*tls).clone())
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    assert!(rejected
        .post(format!("https://api.example.com:{port}/device/token"))
        .body("payload")
        .send()
        .await
        .is_err());
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for (sni, request) in requests.iter() {
        assert_eq!(sni.as_deref(), Some("api.example.com"));
        assert!(request.starts_with("POST /device/token HTTP/1.1\r\n"));
        assert!(request
            .to_ascii_lowercase()
            .contains(&format!("host: api.example.com:{port}\r\n")));
        assert_eq!(request.matches("synthetic-test-token").count(), 1);
    }
    server.abort();
}

#[tokio::test]
async fn resolver_deadline_cancels_unfinished_lookups() {
    let cancelled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut dns = RecoveryDns::new("https://api.example.com").unwrap();
    assert!(dns.enable_from(&report()));
    dns.test_inputs = Some(Inputs {
        tls: Arc::new(reachability::tls_config().unwrap()),
        lookups: Arc::new({
            let cancelled = cancelled.clone();
            move || vec![pending(cancelled.clone())]
        }),
    });
    let error = dns
        .resolve("api.example.com".parse().unwrap())
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("timed out"));
    tokio::task::yield_now().await;
    assert_eq!(cancelled.load(Ordering::SeqCst), 1);
}

fn target(port: u16) -> Target {
    reachability::api_target(&format!("https://api.example.com:{port}")).unwrap()
}

fn step(ok: bool) -> Step {
    Step {
        ok,
        error: (!ok).then(|| "failed".into()),
        ms: 0,
    }
}

fn report() -> Report {
    let system: IpAddr = "192.0.2.1".parse().unwrap();
    let alternate: IpAddr = "192.0.2.2".parse().unwrap();
    Report {
        mode: Mode::Compare,
        vpn_on: false,
        api_dns_recovery: false,
        hosts: vec![HostReport {
            label: "api".into(),
            host: "api.example.com".into(),
            port: 443,
            lookups: vec![LookupReport {
                resolver: SYSTEM.into(),
                addresses: vec![system],
                error: None,
                ms: 0,
            }],
            addresses: vec![
                AddressCheck {
                    address: system,
                    from: vec![SYSTEM.into()],
                    tcp: step(false),
                    tls: None,
                },
                AddressCheck {
                    address: alternate,
                    from: vec!["AliDNS".into()],
                    tcp: step(true),
                    tls: Some(step(true)),
                },
            ],
        }],
    }
}

#[test]
fn requires_explicit_comparison_with_vpn_off_and_verified_alternate() {
    assert!(proves_alternate(&report(), &target(443)));
    type Edit = Box<dyn Fn(&mut Report)>;
    let edits: Vec<Edit> = vec![
        Box::new(|r| r.mode = Mode::SystemOnly),
        Box::new(|r| r.vpn_on = true),
        Box::new(|r| r.hosts[0].label = "node".into()),
        Box::new(|r| r.hosts[0].host = "other.example.com".into()),
        Box::new(|r| r.hosts[0].port = 8443),
        Box::new(|r| r.hosts[0].lookups.clear()),
        Box::new(|r| r.hosts[0].addresses[1].tcp = step(false)),
        Box::new(|r| r.hosts[0].addresses[1].tls = None),
        Box::new(|r| r.hosts[0].addresses[1].tls = Some(step(false))),
        Box::new(|r| {
            r.hosts[0].addresses[1].tls.as_mut().unwrap().error =
                Some("certificate not checked".into())
        }),
        Box::new(|r| r.hosts[0].addresses[1].from = vec!["unknown".into()]),
        Box::new(|r| {
            r.hosts[0].lookups[0]
                .addresses
                .push("192.0.2.3".parse().unwrap())
        }),
        Box::new(|r| {
            r.hosts[0].addresses[0].tcp = step(true);
            r.hosts[0].addresses[0].tls = Some(step(true));
        }),
        Box::new(|r| {
            let ip = r.hosts[0].addresses[1].address;
            r.hosts[0].lookups[0].addresses.push(ip);
        }),
    ];
    for (index, edit) in edits.into_iter().enumerate() {
        let mut candidate = report();
        edit(&mut candidate);
        assert!(!proves_alternate(&candidate, &target(443)), "case {index}");
    }
}

#[test]
fn system_lookup_failure_can_be_recovered_and_healthy_report_disables_recovery() {
    let dns = RecoveryDns::new("https://api.example.com").unwrap();
    assert!(!dns.enabled.load(Ordering::Relaxed));
    let mut failed_lookup = report();
    failed_lookup.hosts[0].lookups[0].addresses.clear();
    failed_lookup.hosts[0].lookups[0].error = Some("NXDOMAIN".into());
    assert!(dns.enable_from(&failed_lookup));
    let mut healthy = report();
    healthy.hosts[0].addresses[0].tcp = step(true);
    healthy.hosts[0].addresses[0].tls = Some(step(true));
    assert!(!dns.enable_from(&healthy));
    assert!(!dns.enabled.load(Ordering::Relaxed));
    for base in [
        "http://api.example.com",
        "https://127.0.0.1",
        "https://[::1]",
        "https://localhost",
    ] {
        assert!(RecoveryDns::new(base).is_none(), "{base}");
    }
}

#[tokio::test]
async fn disabled_recovery_and_proxy_hosts_use_system_dns_without_tls() {
    let dns = RecoveryDns::new("https://api.example.com").unwrap();
    // No TLS server runs here: system resolution still works.
    for enabled in [false, true] {
        dns.enabled.store(enabled, Ordering::Relaxed);
        let addresses: Vec<_> = dns
            .resolve("localhost".parse().unwrap())
            .await
            .unwrap()
            .collect();
        assert!(addresses.iter().any(|a| a.ip().is_loopback()));
        assert!(addresses.iter().all(|a| a.port() == 0));
    }
}

async fn tls_server(name: &str) -> (u16, Arc<rustls::ClientConfig>, tokio::task::JoinHandle<()>) {
    let key = rcgen::generate_simple_self_signed(vec![name.to_string()]).unwrap();
    let cert = key.cert.der().clone();
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let server = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.clone()],
            PrivatePkcs8KeyDer::from(key.signing_key.serialize_der()).into(),
        )
        .unwrap();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    let client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(server));
    let task = tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let acceptor = acceptor.clone();
            tokio::spawn(async move {
                let _ = acceptor.accept(stream).await;
            });
        }
    });
    (port, Arc::new(client), task)
}

fn answer(address: &str) -> Lookup {
    let address: IpAddr = address.parse().unwrap();
    Box::pin(async move { vec![address] })
}

#[tokio::test]
async fn a_verified_alternate_wins_without_waiting_for_stalled_dns() {
    let (port, tls, server) = tls_server("api.example.com").await;
    let lookups: Vec<Lookup> = vec![
        Box::pin(std::future::pending()),
        answer("127.0.0.2"),
        answer("127.0.0.1"),
    ];
    let found = tokio::time::timeout(
        Duration::from_secs(2),
        first_verified(target(port), Ok(tls), lookups),
    )
    .await
    .unwrap();
    assert_eq!(found, Some(SocketAddr::from(([127, 0, 0, 1], port))));
    server.abort();
}

#[tokio::test]
async fn a_tcp_connection_with_the_wrong_certificate_is_not_usable() {
    let (port, tls, server) = tls_server("wrong.example.com").await;
    assert!(
        first_verified(target(port), Ok(tls), vec![answer("127.0.0.1")])
            .await
            .is_none()
    );
    server.abort();
}

#[tokio::test]
async fn an_untrusted_certificate_is_not_usable() {
    let (port, _, server) = tls_server("api.example.com").await;
    let tls = reachability::tls_config().map(Arc::new);
    assert!(first_verified(target(port), tls, vec![answer("127.0.0.1")])
        .await
        .is_none());
    server.abort();
}

#[tokio::test]
async fn changed_dns_answers_are_not_cached() {
    let (port, tls, server) = tls_server("api.example.com").await;
    assert!(
        first_verified(target(port), Ok(tls.clone()), vec![answer("127.0.0.1")])
            .await
            .is_some()
    );
    assert!(
        first_verified(target(port), Ok(tls), vec![answer("127.0.0.2")])
            .await
            .is_none()
    );
    server.abort();
}

#[tokio::test]
async fn no_addresses_or_unavailable_tls_fails_closed() {
    assert!(first_verified(
        target(443),
        reachability::tls_config().map(Arc::new),
        vec![]
    )
    .await
    .is_none());
    let (port, _, server) = tls_server("api.example.com").await;
    assert!(first_verified(
        target(port),
        Err("no TLS".into()),
        vec![answer("127.0.0.1")]
    )
    .await
    .is_none());
    server.abort();
}
