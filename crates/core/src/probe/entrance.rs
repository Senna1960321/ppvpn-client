//! Entrance probes: every ingress of every node, directly. `tcp` times a
//! TCP handshake with the ingress (no proxy handshake), `icmp` an echo. The
//! literal ingress IP is used when there is one, so the probe never depends
//! on DNS; otherwise the domain is resolved first, not timed.
//!
//! Each ingress is sampled [`ENTRANCE_SAMPLES`] times in a row and reports
//! the fastest sample (#549): one sample landed anywhere in the path's
//! jitter, on average near its mean, and a lost packet or a queue behind a
//! speed test's other probes counted as the node's distance. Probe starts
//! are spread by [`ENTRANCE_STAGGER`] so a request does not send every
//! first packet at once.

use std::collections::HashSet;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures_util::future::join_all;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use super::result_codes as rc;
use super::{icmp, require_online, timeout_of, Cancel, DefaultInterface};
use crate::config::SocketProtect;
use crate::error::{codes, Error};
use crate::event::Event;
use crate::profile::{Endpoint, Ingress, Profile};
use crate::types::{EntranceResult, IngressProbeResult, ProbeEntrancesRequest, ProbeMethod};

/// A request's timeout when it gives none.
pub(crate) const ENTRANCE_TIMEOUT: Duration = Duration::from_secs(5);
/// Ingress probes in flight when a request gives no bound.
pub(crate) const ENTRANCE_CONCURRENCY: usize = 4;
/// Samples per ingress, one after another within its timeout; the result is
/// the fastest.
pub(crate) const ENTRANCE_SAMPLES: usize = 3;
/// Gap between the starts of a request's first ingress probes (the later
/// ones start as slots free up).
pub(crate) const ENTRANCE_STAGGER: Duration = Duration::from_millis(10);

/// The host's network as entrance probes use it; tests replace it.
#[async_trait]
pub(crate) trait Net: Send + Sync {
    /// A TCP handshake with `to`; the connection is closed at once.
    async fn connect(&self, to: SocketAddr) -> io::Result<()>;
    /// One ICMP echo round trip, or a `result_codes` ICMP_* code.
    async fn ping(&self, to: IpAddr, timeout: Duration) -> Result<Duration, &'static str>;
    async fn resolve(&self, host: &str) -> io::Result<Vec<IpAddr>>;
}

/// The operating system's sockets and resolver.
#[derive(Default)]
pub(crate) struct SystemNet {
    /// A mobile host's socket protection: the probes' sockets stay out of
    /// its VPN, whose TUN would otherwise answer a TCP handshake itself.
    protect: Option<SocketProtect>,
}

impl SystemNet {
    pub(crate) fn protected(protect: Option<SocketProtect>) -> Self {
        Self { protect }
    }
}

#[async_trait]
impl Net for SystemNet {
    async fn connect(&self, to: SocketAddr) -> io::Result<()> {
        match &self.protect {
            None => tokio::net::TcpStream::connect(to).await.map(drop),
            Some(protect) => connect_protected(to, protect).await,
        }
    }

    async fn ping(&self, to: IpAddr, timeout: Duration) -> Result<Duration, &'static str> {
        match &self.protect {
            None => icmp::ping(to, timeout).await,
            Some(protect) => icmp::ping_protected(to, timeout, protect).await,
        }
    }

    async fn resolve(&self, host: &str) -> io::Result<Vec<IpAddr>> {
        Ok(tokio::net::lookup_host((host, 0))
            .await?
            .map(|a| a.ip())
            .collect())
    }
}

/// A TCP connect whose socket the host protects before it connects.
#[cfg(unix)]
async fn connect_protected(to: SocketAddr, protect: &SocketProtect) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let socket = if to.is_ipv4() {
        tokio::net::TcpSocket::new_v4()?
    } else {
        tokio::net::TcpSocket::new_v6()?
    };
    protect.protect(socket.as_raw_fd())?;
    socket.connect(to).await.map(drop)
}

/// Only mobile hosts protect sockets.
#[cfg(not(unix))]
async fn connect_protected(to: SocketAddr, _protect: &SocketProtect) -> io::Result<()> {
    tokio::net::TcpStream::connect(to).await.map(drop)
}

/// Measures every ingress of the nodes `request` names (all when none), at
/// most `concurrency` at a time. Offline it fails at once and probes nothing.
/// Returns the results in profile order, and one EntranceProbed per node.
pub(crate) async fn probe_entrances(
    profile: &Profile,
    request: &ProbeEntrancesRequest,
    network: DefaultInterface,
    net: &dyn Net,
    cancel: &Cancel,
) -> Result<(Vec<EntranceResult>, Vec<Event>), Error> {
    require_online(network)?;
    let nodes: Vec<_> = if request.node_ids.is_empty() {
        profile.nodes.iter().collect()
    } else {
        let wanted: HashSet<&str> = request.node_ids.iter().map(String::as_str).collect();
        let nodes: Vec<_> = profile
            .nodes
            .iter()
            .filter(|n| wanted.contains(n.id.as_str()))
            .collect();
        if nodes.len() != wanted.len() {
            return Err(Error::invalid(
                codes::NODE_NOT_FOUND,
                "node_id",
                "one or more probe nodes were not found",
            ));
        }
        nodes
    };
    let method = request.method;
    let timeout = timeout_of(request.timeout_ms, ENTRANCE_TIMEOUT);
    let concurrency = match request.concurrency {
        0 => ENTRANCE_CONCURRENCY,
        n => n as usize,
    };
    let slots = Semaphore::new(concurrency);
    let mut started = 0usize;
    let probes = nodes.iter().map(|node| {
        join_all(node.ingresses.iter().map(|ingress| {
            let wait = ENTRANCE_STAGGER * started.min(concurrency) as u32;
            started += 1;
            probe_one(ingress, method, timeout, net, &slots, wait, cancel)
        }))
    });
    // The starts above are counted before anything runs.
    let probes: Vec<_> = probes.collect();
    let measured = join_all(probes).await;
    let measured_at = Utc::now();
    let results: Vec<EntranceResult> = nodes
        .iter()
        .zip(measured)
        .map(|(node, ingresses)| summarize(&node.id, method, ingresses, measured_at))
        .collect();
    let events = results
        .iter()
        .map(|r| Event::EntranceProbed {
            at: r.measured_at,
            revision: profile.revision.clone(),
            node_id: r.node_id.clone(),
            message: if r.success {
                "success".to_owned()
            } else {
                r.error_code.clone()
            },
        })
        .collect();
    Ok((results, events))
}

async fn probe_one(
    ingress: &Ingress,
    method: ProbeMethod,
    timeout: Duration,
    net: &dyn Net,
    slots: &Semaphore,
    wait: Duration,
    cancel: &Cancel,
) -> IngressProbeResult {
    let mut result = IngressProbeResult {
        endpoint_key: ingress.endpoint_key.clone(),
        label: ingress.label.clone().unwrap_or_default(),
        replica_ordinal: ingress.replica_ordinal,
        role: ingress.role.clone(),
        success: false,
        latency_ms: 0,
        error_code: String::new(),
    };
    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(rc::CANCELED),
        outcome = async {
            tokio::time::sleep(wait).await;
            let _slot = slots.acquire().await.expect("never closed");
            if cancel.is_cancelled() {
                return Err(rc::CANCELED);
            }
            probe_ingress(&ingress.endpoint, method, timeout, net).await
        } => outcome,
    };
    match outcome {
        Ok(latency) => {
            result.success = true;
            // Rounded to the nearest millisecond; a success is never 0.
            let ms = (latency + Duration::from_micros(500)).as_millis();
            result.latency_ms = i64::try_from(ms).unwrap_or(i64::MAX).max(1);
        }
        Err(code) => result.error_code = code.to_owned(),
    }
    result
}

/// The ingress's fastest of [`ENTRANCE_SAMPLES`] samples within
/// `timeout`, the resolution included. Samples run while time is left; a
/// sample still out at the deadline is dropped, and the ingress fails only
/// when no sample succeeded (with the last sample's failure).
async fn probe_ingress(
    endpoint: &Endpoint,
    method: ProbeMethod,
    timeout: Duration,
    net: &dyn Net,
) -> Result<Duration, &'static str> {
    let timed_out = match method {
        ProbeMethod::Icmp => rc::ICMP_TIMEOUT,
        ProbeMethod::Tcp => rc::TIMEOUT,
    };
    let deadline = Instant::now() + timeout;
    let addr = match tokio::time::timeout_at(deadline, target_address(endpoint, net)).await {
        Ok(addr) => addr?,
        Err(_) => return Err(timed_out),
    };
    let mut fastest: Option<Duration> = None;
    let mut failure = timed_out;
    for _ in 0..ENTRANCE_SAMPLES {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        let sample = sample(addr, endpoint.port, method, remaining, net);
        match tokio::time::timeout_at(deadline, sample).await {
            Ok(Ok(rtt)) => fastest = Some(fastest.map_or(rtt, |best| best.min(rtt))),
            Ok(Err(code)) => failure = code,
            Err(_) => {
                failure = timed_out;
                break;
            }
        }
    }
    fastest.ok_or(failure)
}

/// One handshake or echo.
async fn sample(
    addr: IpAddr,
    port: u16,
    method: ProbeMethod,
    remaining: Duration,
    net: &dyn Net,
) -> Result<Duration, &'static str> {
    match method {
        ProbeMethod::Icmp => net.ping(addr, remaining).await,
        ProbeMethod::Tcp => {
            let started = Instant::now();
            match net.connect(SocketAddr::new(addr, port)).await {
                Ok(()) => Ok(started.elapsed()),
                Err(e) if e.kind() == io::ErrorKind::TimedOut => Err(rc::TIMEOUT),
                Err(_) => Err(rc::CONNECT_FAILED),
            }
        }
    }
}

/// The literal IP when there is one; otherwise the domain's first IPv4
/// address (the most widely routable), else its first address.
async fn target_address(endpoint: &Endpoint, net: &dyn Net) -> Result<IpAddr, &'static str> {
    if !endpoint.ip.is_empty() {
        return endpoint
            .ip
            .parse::<IpAddr>()
            .map(|ip| ip.to_canonical())
            .map_err(|_| rc::DNS_FAILED);
    }
    let addrs = net
        .resolve(&endpoint.domain)
        .await
        .map_err(|_| rc::DNS_FAILED)?;
    let addrs: Vec<IpAddr> = addrs.into_iter().map(|ip| ip.to_canonical()).collect();
    addrs
        .iter()
        .find(|ip| ip.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
        .ok_or(rc::DNS_FAILED)
}

/// The node reports its primary when that succeeded, else its fastest
/// successful backup, else the primary's failure.
fn summarize(
    node_id: &str,
    method: ProbeMethod,
    ingresses: Vec<IngressProbeResult>,
    measured_at: chrono::DateTime<Utc>,
) -> EntranceResult {
    let mut chosen = 0;
    if !ingresses.first().is_some_and(|i| i.success) {
        for (j, ingress) in ingresses.iter().enumerate().skip(1) {
            let best = &ingresses[chosen];
            if ingress.success && (!best.success || ingress.latency_ms < best.latency_ms) {
                chosen = j;
            }
        }
    }
    let (success, latency_ms, error_code, endpoint_key, ingress_role) = match ingresses.get(chosen)
    {
        Some(i) => (
            i.success,
            i.latency_ms,
            i.error_code.clone(),
            i.endpoint_key.clone(),
            i.role.clone(),
        ),
        None => Default::default(),
    };
    EntranceResult {
        node_id: node_id.to_owned(),
        method,
        success,
        latency_ms,
        error_code,
        endpoint_key,
        ingress_role,
        ingresses,
        measured_at,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use tokio::net::TcpListener;

    use super::*;
    use crate::profile::Node;

    type Connect = Box<dyn Fn(SocketAddr) -> io::Result<()> + Send + Sync>;
    type Ping = Box<dyn Fn(IpAddr) -> Result<Duration, &'static str> + Send + Sync>;
    type Resolve = Box<dyn Fn(&str) -> io::Result<Vec<IpAddr>> + Send + Sync>;

    /// The system's TCP unless replaced; ping and resolve as each test says
    /// (absent: the test fails if they are used). Records every call.
    #[derive(Default)]
    struct TestNet {
        connect: Option<Connect>,
        hang_connect: bool,
        ping: Option<Ping>,
        resolve: Option<Resolve>,
        calls: Mutex<Vec<String>>,
        in_flight: AtomicUsize,
        most_in_flight: AtomicUsize,
        delay: Duration,
    }

    #[async_trait]
    impl Net for TestNet {
        async fn connect(&self, to: SocketAddr) -> io::Result<()> {
            self.calls.lock().unwrap().push(format!("connect {to}"));
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.most_in_flight.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(self.delay).await;
            if self.hang_connect {
                std::future::pending::<()>().await;
            }
            let outcome = match &self.connect {
                Some(connect) => connect(to),
                None => SystemNet::default().connect(to).await,
            };
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            outcome
        }

        async fn ping(&self, to: IpAddr, _timeout: Duration) -> Result<Duration, &'static str> {
            self.calls.lock().unwrap().push(format!("ping {to}"));
            (self.ping.as_ref().expect("unexpected ping"))(to)
        }

        async fn resolve(&self, host: &str) -> io::Result<Vec<IpAddr>> {
            self.calls.lock().unwrap().push(format!("resolve {host}"));
            (self.resolve.as_ref().expect("unexpected resolve"))(host)
        }
    }

    fn ingress(role: &str, key: &str, domain: &str, ip: &str, port: u16) -> Ingress {
        Ingress {
            role: role.into(),
            endpoint_key: key.into(),
            protocol: "shadowsocks".into(),
            endpoint: Endpoint {
                domain: domain.into(),
                ip: ip.into(),
                port,
            },
            ..Default::default()
        }
    }

    fn profile(mut ingresses: Vec<Ingress>) -> Profile {
        for (i, ingress) in ingresses.iter_mut().enumerate() {
            ingress.replica_ordinal = i as i64;
        }
        Profile {
            schema_version: 1,
            revision: "r".into(),
            nodes: vec![Node {
                id: "node".into(),
                entry_key: "cn-optimized".into(),
                ingresses,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn request(method: ProbeMethod, timeout_ms: u64, concurrency: u32) -> ProbeEntrancesRequest {
        ProbeEntrancesRequest::new(method, timeout_ms, concurrency)
    }

    async fn probe(
        p: &Profile,
        r: &ProbeEntrancesRequest,
        net: &TestNet,
    ) -> Result<(Vec<EntranceResult>, Vec<Event>), Error> {
        probe_entrances(p, r, DefaultInterface::Present, net, &Cancel::never()).await
    }

    async fn listener() -> (TcpListener, u16) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    // Go: TestEntranceUsesLiteralIP.
    #[tokio::test]
    async fn entrance_uses_literal_ip() {
        let (_listener, port) = listener().await;
        let p = profile(vec![ingress(
            "primary",
            "a",
            "must-not-resolve.invalid",
            "127.0.0.1",
            port,
        )]);
        let net = TestNet::default();
        let (got, _) = probe(&p, &request(ProbeMethod::Tcp, 1000, 1), &net)
            .await
            .unwrap();
        let r = &got[0];
        assert!(r.success, "{r:?}");
        assert_eq!(
            (r.method, r.ingress_role.as_str(), r.ingresses.len()),
            (ProbeMethod::Tcp, "primary", 1)
        );
        assert!(r.latency_ms >= 1);
        assert_eq!(
            *net.calls.lock().unwrap(),
            vec![format!("connect 127.0.0.1:{port}"); ENTRANCE_SAMPLES]
        );
    }

    // Go: TestEntranceResolvesDomainWithoutIP.
    #[tokio::test]
    async fn entrance_resolves_domain_without_ip() {
        let (_listener, port) = listener().await;
        let p = profile(vec![ingress("primary", "a", "edge.example.com", "", port)]);
        let net = TestNet {
            resolve: Some(Box::new(|host| {
                assert_eq!(host, "edge.example.com");
                Ok(vec![
                    "2001:db8::8888".parse().unwrap(),
                    "127.0.0.1".parse().unwrap(),
                ])
            })),
            ..Default::default()
        };
        let (got, _) = probe(&p, &request(ProbeMethod::Tcp, 0, 0), &net)
            .await
            .unwrap();
        assert!(got[0].success, "{got:?}");
        // Resolved once for all samples.
        let mut want = vec!["resolve edge.example.com".to_owned()];
        want.extend(vec![format!("connect 127.0.0.1:{port}"); ENTRANCE_SAMPLES]);
        assert_eq!(*net.calls.lock().unwrap(), want);
    }

    // Go: TestEntranceDNSFailure.
    #[tokio::test]
    async fn entrance_dns_failure() {
        let p = profile(vec![ingress("primary", "a", "edge.example.com", "", 443)]);
        let net = TestNet {
            resolve: Some(Box::new(|_| Err(io::Error::other("nxdomain")))),
            ..Default::default()
        };
        let (got, events) = probe(&p, &request(ProbeMethod::Icmp, 0, 0), &net)
            .await
            .unwrap();
        assert!(!got[0].success);
        assert_eq!(got[0].error_code, rc::DNS_FAILED);
        assert!(matches!(
            &events[0],
            Event::EntranceProbed { message, .. } if message == rc::DNS_FAILED
        ));
    }

    // Go: TestEntranceFallsBackToBestBackup.
    #[tokio::test]
    async fn entrance_falls_back_to_best_backup() {
        let mut labeled = ingress("backup", "c.example.com", "c.example.com", "192.0.2.3", 443);
        labeled.label = Some("Relay C".into());
        let p = profile(vec![
            ingress(
                "primary",
                "a.example.com",
                "a.example.com",
                "192.0.2.1",
                443,
            ),
            ingress("backup", "b.example.com", "b.example.com", "192.0.2.2", 443),
            labeled,
        ]);
        let net = TestNet {
            ping: Some(Box::new(|ip| match ip.to_string().as_str() {
                "192.0.2.1" => Err(rc::ICMP_TIMEOUT),
                "192.0.2.2" => Ok(Duration::from_millis(80)),
                _ => Ok(Duration::from_millis(30)),
            })),
            ..Default::default()
        };
        let (got, events) = probe(&p, &request(ProbeMethod::Icmp, 0, 0), &net)
            .await
            .unwrap();
        let r = &got[0];
        assert!(r.success && r.error_code.is_empty(), "{r:?}");
        assert_eq!(r.latency_ms, 30);
        assert_eq!(
            (r.ingress_role.as_str(), r.endpoint_key.as_str(), r.method),
            ("backup", "c.example.com", ProbeMethod::Icmp)
        );
        let (a, c) = (&r.ingresses[0], &r.ingresses[2]);
        assert!(!a.success && a.error_code == rc::ICMP_TIMEOUT && a.role == "primary");
        assert_eq!(
            (a.endpoint_key.as_str(), a.label.as_str()),
            ("a.example.com", "")
        );
        assert!(c.success);
        assert_eq!(
            (c.endpoint_key.as_str(), c.replica_ordinal, c.label.as_str()),
            ("c.example.com", 2, "Relay C")
        );
        match &events[..] {
            [Event::EntranceProbed {
                at,
                revision,
                node_id,
                message,
            }] => {
                assert_eq!(*at, r.measured_at);
                assert_eq!(
                    (revision.as_str(), node_id.as_str(), message.as_str()),
                    ("r", "node", "success")
                );
            }
            other => panic!("{other:?}"),
        }
    }

    // Go: TestEntrancePrimaryWinsWhenHealthy.
    #[tokio::test]
    async fn entrance_primary_wins_when_healthy() {
        let p = profile(vec![
            ingress(
                "primary",
                "a.example.com",
                "a.example.com",
                "192.0.2.1",
                443,
            ),
            ingress("backup", "b.example.com", "b.example.com", "192.0.2.2", 443),
        ]);
        let net = TestNet {
            ping: Some(Box::new(|ip| {
                Ok(Duration::from_millis(if ip.to_string() == "192.0.2.1" {
                    90
                } else {
                    10
                }))
            })),
            ..Default::default()
        };
        let (got, _) = probe(&p, &request(ProbeMethod::Icmp, 0, 0), &net)
            .await
            .unwrap();
        assert_eq!(
            (
                got[0].ingress_role.as_str(),
                got[0].endpoint_key.as_str(),
                got[0].latency_ms
            ),
            ("primary", "a.example.com", 90)
        );
    }

    // Go: TestEntranceAllFailedReportsPrimary.
    #[tokio::test]
    async fn entrance_all_failed_reports_primary() {
        let p = profile(vec![
            ingress(
                "primary",
                "a.example.com",
                "a.example.com",
                "192.0.2.1",
                443,
            ),
            ingress("backup", "b.example.com", "b.example.com", "192.0.2.2", 443),
        ]);
        let net = TestNet {
            connect: Some(Box::new(|_| {
                Err(io::Error::from(io::ErrorKind::ConnectionRefused))
            })),
            ..Default::default()
        };
        let (got, _) = probe(&p, &request(ProbeMethod::Tcp, 0, 0), &net)
            .await
            .unwrap();
        let r = &got[0];
        assert!(!r.success);
        assert_eq!(
            (r.error_code.as_str(), r.ingress_role.as_str()),
            (rc::CONNECT_FAILED, "primary")
        );
    }

    /// Samples one after another: the fastest counts, a failed sample is
    /// outweighed by any success, and only all failing fails (#549).
    #[tokio::test]
    async fn entrance_reports_the_fastest_sample() {
        let p = profile(vec![ingress("primary", "a", "", "192.0.2.1", 443)]);
        let script = |samples: Vec<Result<u64, &'static str>>| {
            let next = AtomicUsize::new(0);
            TestNet {
                ping: Some(Box::new(move |_| {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    samples[i.min(samples.len() - 1)].map(Duration::from_millis)
                })),
                ..Default::default()
            }
        };
        for (samples, want) in [
            (vec![Ok(250), Ok(160), Ok(300)], Ok(160)),
            (vec![Err(rc::ICMP_TIMEOUT), Ok(210), Ok(190)], Ok(190)),
            (
                vec![Ok(170), Err(rc::ICMP_FAILED), Err(rc::ICMP_FAILED)],
                Ok(170),
            ),
            (vec![Err(rc::ICMP_FAILED); 3], Err(rc::ICMP_FAILED)),
        ] {
            let net = script(samples.clone());
            let (got, _) = probe(&p, &request(ProbeMethod::Icmp, 1000, 0), &net)
                .await
                .unwrap();
            let r = &got[0];
            let outcome = if r.success {
                Ok(r.latency_ms as u64)
            } else {
                Err(r.error_code.as_str())
            };
            assert_eq!(outcome, want, "{samples:?}");
            assert_eq!(net.calls.lock().unwrap().len(), ENTRANCE_SAMPLES);
        }
    }

    /// A sample still out at the deadline is dropped; the ones before it
    /// count.
    #[tokio::test]
    async fn entrance_keeps_the_samples_before_a_timeout() {
        let p = profile(vec![ingress("primary", "a", "", "192.0.2.1", 443)]);
        let next = Arc::new(AtomicUsize::new(0));
        let counter = next.clone();
        let net = TestNet {
            connect: Some(Box::new(move |_| {
                counter.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })),
            ..Default::default()
        };
        struct HangsAfterFirst(TestNet, Arc<AtomicUsize>);
        #[async_trait]
        impl Net for HangsAfterFirst {
            async fn connect(&self, to: SocketAddr) -> io::Result<()> {
                if self.1.load(Ordering::SeqCst) >= 1 {
                    std::future::pending::<()>().await;
                }
                self.0.connect(to).await
            }
            async fn ping(&self, to: IpAddr, t: Duration) -> Result<Duration, &'static str> {
                self.0.ping(to, t).await
            }
            async fn resolve(&self, host: &str) -> io::Result<Vec<IpAddr>> {
                self.0.resolve(host).await
            }
        }
        let net = HangsAfterFirst(net, next.clone());
        let r = request(ProbeMethod::Tcp, 100, 0);
        let (got, _) = probe_entrances(&p, &r, DefaultInterface::Present, &net, &Cancel::never())
            .await
            .unwrap();
        assert!(got[0].success, "{got:?}");
        assert_eq!(next.load(Ordering::SeqCst), 1);
    }

    /// The first probes of a request start apart, not all at once.
    #[tokio::test]
    async fn entrance_probe_starts_are_spread() {
        let p = profile(
            (0..3)
                .map(|i| ingress("backup", &format!("k{i}"), "", "192.0.2.1", 443))
                .collect(),
        );
        let starts = Arc::new(Mutex::new(Vec::new()));
        let record = starts.clone();
        let net = TestNet {
            connect: Some(Box::new(move |_| {
                record.lock().unwrap().push(std::time::Instant::now());
                Ok(())
            })),
            ..Default::default()
        };
        let began = std::time::Instant::now();
        probe(&p, &request(ProbeMethod::Tcp, 1000, 0), &net)
            .await
            .unwrap();
        let mut firsts: Vec<Duration> = starts
            .lock()
            .unwrap()
            .iter()
            .map(|at| at.duration_since(began))
            .collect();
        firsts.sort();
        // Three ingresses, three samples each; the last ingress waited two
        // gaps before its first sample.
        assert_eq!(firsts.len(), 3 * ENTRANCE_SAMPLES);
        assert!(
            firsts[firsts.len() - 1] >= ENTRANCE_STAGGER * 2,
            "{firsts:?}"
        );
    }

    // Go: TestEntranceTimeout.
    #[tokio::test]
    async fn entrance_timeout() {
        let p = profile(vec![ingress(
            "primary",
            "a",
            "a.example.com",
            "192.0.2.1",
            443,
        )]);
        let net = TestNet {
            hang_connect: true,
            ..Default::default()
        };
        let (got, _) = probe(&p, &request(ProbeMethod::Tcp, 1, 1), &net)
            .await
            .unwrap();
        assert_eq!(got[0].error_code, rc::TIMEOUT);
    }

    // Go: TestEntranceCanceled.
    #[tokio::test]
    async fn entrance_canceled() {
        let p = profile(vec![ingress(
            "primary",
            "a",
            "a.example.com",
            "192.0.2.1",
            443,
        )]);
        let net = TestNet::default();
        let (canceller, cancel) = Cancel::new();
        canceller.cancel();
        let (got, events) = probe_entrances(
            &p,
            &request(ProbeMethod::Tcp, 1000, 1),
            DefaultInterface::Present,
            &net,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(got[0].error_code, rc::CANCELED);
        assert!(net.calls.lock().unwrap().is_empty());
        assert!(matches!(
            &events[0],
            Event::EntranceProbed { message, .. } if message == rc::CANCELED
        ));
    }

    #[tokio::test]
    async fn entrance_cancel_ends_probes_in_flight() {
        let p = profile(vec![ingress(
            "primary",
            "a",
            "a.example.com",
            "192.0.2.1",
            443,
        )]);
        let net = TestNet {
            hang_connect: true,
            ..Default::default()
        };
        let (canceller, cancel) = Cancel::new();
        let r = request(ProbeMethod::Tcp, 60_000, 1);
        let probing = probe_entrances(&p, &r, DefaultInterface::Present, &net, &cancel);
        let cancelling = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            canceller.cancel();
        };
        let ((got, _), ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(async { probing.await.unwrap() }, cancelling)
        })
        .await
        .expect("cancel not honoured");
        assert_eq!(got[0].error_code, rc::CANCELED);
    }

    #[tokio::test]
    async fn entrance_concurrency_is_bounded() {
        let p = profile(
            (0..6)
                .map(|i| {
                    ingress(
                        "backup",
                        &format!("k{i}"),
                        "x.example.com",
                        "192.0.2.1",
                        443,
                    )
                })
                .collect(),
        );
        let net = TestNet {
            connect: Some(Box::new(|_| Ok(()))),
            delay: Duration::from_millis(20),
            ..Default::default()
        };
        let (got, _) = probe(&p, &request(ProbeMethod::Tcp, 0, 2), &net)
            .await
            .unwrap();
        assert!(got[0].ingresses.iter().all(|i| i.success));
        assert_eq!(net.most_in_flight.load(Ordering::SeqCst), 2);
    }

    // Go: api TestProbeEntrancesMethodAndShape (the node filter, the
    // result's JSON).
    #[tokio::test]
    async fn entrance_node_filter_and_shape() {
        let mut tokyo = ingress("primary", "9001", "tokyo.example.com", "192.0.2.1", 443);
        tokyo.label = Some("Tokyo A".into());
        let mut p = profile(vec![tokyo]);
        let mut other = p.nodes[0].clone();
        other.id = "other".into();
        p.nodes.push(other);
        let net = TestNet {
            connect: Some(Box::new(|_| Ok(()))),
            ping: Some(Box::new(|_| Ok(Duration::from_millis(3)))),
            ..Default::default()
        };
        for (method, name) in [(ProbeMethod::Tcp, "tcp"), (ProbeMethod::Icmp, "icmp")] {
            let r = request(method, 50, 0).with_node_ids(vec!["node".into()]);
            let (got, events) = probe(&p, &r, &net).await.unwrap();
            assert_eq!((got.len(), events.len()), (1, 1));
            let body = serde_json::to_string(&got).unwrap();
            for want in [
                format!(r#""method":"{name}""#),
                r#""endpoint_key":"9001","ingress_role":"primary""#.to_owned(),
                r#""ingresses":[{"endpoint_key":"9001","label":"Tokyo A","replica_ordinal":0,"role":"primary""#.to_owned(),
                r#""latency_ms""#.to_owned(),
            ] {
                assert!(body.contains(&want), "{body} lacks {want}");
            }
            assert!(!body.contains("connect_ms"), "{body}");
        }
        let missing = request(ProbeMethod::Tcp, 50, 0).with_node_ids(vec!["missing".into()]);
        let error = probe(&p, &missing, &net).await.unwrap_err();
        assert_eq!(
            (error.code, error.field.as_deref(), error.retryable),
            (codes::NODE_NOT_FOUND, Some("node_id"), false)
        );
        let (all, _) = probe(&p, &request(ProbeMethod::Tcp, 50, 0), &net)
            .await
            .unwrap();
        assert_eq!(
            all.iter().map(|r| r.node_id.as_str()).collect::<Vec<_>>(),
            ["node", "other"]
        );
    }

    // Go: internal/runtime TestProbesFailFastWithoutDefaultInterface (the
    // entrance half; the Engine passes sail's view of the interface).
    #[tokio::test]
    async fn entrance_offline_fails_fast_and_probes_nothing() {
        let p = profile(vec![ingress(
            "primary",
            "a",
            "a.example.com",
            "192.0.2.1",
            443,
        )]);
        let net = TestNet {
            connect: Some(Box::new(|_| Ok(()))),
            ..Default::default()
        };
        let r = request(ProbeMethod::Tcp, 5000, 1);
        let started = std::time::Instant::now();
        let error = probe_entrances(&p, &r, DefaultInterface::Absent, &net, &Cancel::never())
            .await
            .unwrap_err();
        assert_eq!(
            (error.code, error.retryable),
            (codes::NO_DEFAULT_INTERFACE, true)
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(net.calls.lock().unwrap().is_empty(), "probed while offline");
        for network in [DefaultInterface::Present, DefaultInterface::Unknown] {
            let (got, _) = probe_entrances(&p, &r, network, &net, &Cancel::never())
                .await
                .unwrap();
            assert!(got[0].success, "{network:?}");
        }
    }

    // Go: TestParseMethod. The method is an enum: tcp when absent (or
    // empty, as Go's), and nothing else decodes.
    #[test]
    fn parse_method() {
        for (json, want) in [
            (r#"{"timeout_ms":1,"concurrency":1}"#, ProbeMethod::Tcp),
            (
                r#"{"method":"","timeout_ms":1,"concurrency":1}"#,
                ProbeMethod::Tcp,
            ),
            (
                r#"{"method":"tcp","timeout_ms":1,"concurrency":1}"#,
                ProbeMethod::Tcp,
            ),
            (
                r#"{"method":"icmp","timeout_ms":1,"concurrency":1}"#,
                ProbeMethod::Icmp,
            ),
        ] {
            let got: ProbeEntrancesRequest = serde_json::from_str(json).unwrap();
            assert_eq!(got.method, want, "{json}");
        }
        for method in ["udp", "http"] {
            let json = format!(r#"{{"method":"{method}","timeout_ms":1,"concurrency":1}}"#);
            assert!(serde_json::from_str::<ProbeEntrancesRequest>(&json).is_err());
        }
        assert_eq!(
            serde_json::to_string(&ProbeMethod::Tcp).unwrap(),
            r#""tcp""#
        );
    }

    #[allow(dead_code)]
    fn _assert_send(p: &Profile, r: &ProbeEntrancesRequest, net: Arc<dyn Net>, c: Cancel) {
        fn send<T: Send>(_: T) {}
        send(async move { probe_entrances(p, r, DefaultInterface::Present, &*net, &c).await });
    }

    /// A mobile host's protection sees each probe socket before it
    /// connects, and a socket it refuses never connects.
    #[cfg(unix)]
    #[tokio::test]
    async fn protected_probes_go_through_the_host() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let to = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        let net = SystemNet::protected(Some(SocketProtect::new(move |fd| {
            record.lock().unwrap().push(fd);
            Ok(())
        })));
        net.connect(to).await.unwrap();
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert!(seen.lock().unwrap()[0] >= 0);

        let refusing = SystemNet::protected(Some(SocketProtect::new(|_| {
            Err(io::Error::other("refused"))
        })));
        let err = refusing.connect(to).await.unwrap_err();
        assert_eq!(err.to_string(), "refused");
    }
}
