//! Reachability diagnostics (#521 step A), for telling tampered DNS, SNI
//! blocking and node problems apart before the tunnel is up. For each host
//! (the API host, the profile's node hosts):
//! - what the system resolver answers;
//! - only when the user asks for it ([`Mode::Compare`]), what the domestic
//!   DoH resolvers in [`DOH_RESOLVERS`] answer: running it automatically
//!   would tell a third party which service the user uses;
//! - for each address any of them gave: whether TCP connects and, where the
//!   host speaks TLS, whether a handshake with SNI = the host's server name
//!   and the certificate verified against the web PKI completes.
//!
//! The probes themselves change no settings. A host may use a user-requested
//! report to enable API DNS recovery when it proves a usable alternative.
//! Every connection is direct, past the
//! OS proxy the API client honours; with the VPN on, the system routes it
//! through the tunnel, which [`Report::vpn_on`] records.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use hickory_proto::op::{Message, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::{Name, RData, RecordType};
use serde::Serialize;
use tokio::net::TcpStream;
use tokio_rustls::rustls;

/// A DoH resolver (RFC 8484) the diagnostics compare the system's answers
/// with.
#[derive(Debug, Clone, Copy)]
pub struct DohResolver {
    pub name: &'static str,
    /// Addressed by IP (its certificate covers it), so that reaching it
    /// needs no resolver.
    pub url: &'static str,
}

/// The DoH resolvers, in one place: domestic ones, which ISP hijacking and
/// on-path injection do not reach.
pub const DOH_RESOLVERS: &[DohResolver] = &[
    DohResolver {
        name: "AliDNS",
        url: "https://223.5.5.5/dns-query",
    },
    DohResolver {
        name: "DNSPod",
        url: "https://1.12.12.12/dns-query",
    },
];

/// The system resolver's name in a report.
pub const SYSTEM: &str = "system";

/// Deadline of each step: a lookup, a TCP connect, a TLS handshake.
const STEP_TIMEOUT: Duration = Duration::from_secs(5);
/// At most this many hosts per run (a profile can list many nodes).
const MAX_TARGETS: usize = 32;
/// At most this many addresses checked per host.
const MAX_ADDRESSES: usize = 8;
/// At most this many lookups or connections at once.
const PARALLEL: usize = 16;

/// Which resolvers a run asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// The system resolver only: for the automatic run after a failure.
    SystemOnly,
    /// The system resolver and [`DOH_RESOLVERS`]: only when the user asked.
    Compare,
}

/// A host to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// What it is, e.g. `api` or `node <name> (<endpoint key>)`.
    pub label: String,
    /// A host name or an IP address.
    pub host: String,
    pub port: u16,
    pub tls: Option<TlsTarget>,
}

/// The TLS handshake a target expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsTarget {
    /// The SNI, and the name the certificate must carry.
    pub server_name: String,
    /// False for an ingress the profile marks `insecure`: a certificate
    /// error then still shows the server answered TLS.
    pub verify: bool,
}

/// The API host of `api_base` (an http(s) URL), or None when it has none.
pub fn api_target(api_base: &str) -> Option<Target> {
    let url = reqwest::Url::parse(api_base.trim()).ok()?;
    let host = url.host_str()?.trim_matches(['[', ']']).to_string();
    let port = url.port_or_known_default()?;
    let tls = (url.scheme() == "https").then(|| TlsTarget {
        server_name: host.clone(),
        verify: true,
    });
    Some(Target {
        label: "api".into(),
        host,
        port,
        tls,
    })
}

/// The ingresses of a proxy profile (the backend's JSON), one target per
/// distinct host, port and TLS; an unreadable profile gives none.
pub fn profile_targets(profile: &[u8]) -> Vec<Target> {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(profile) else {
        return Vec::new();
    };
    let mut targets: Vec<Target> = Vec::new();
    let nodes = value["nodes"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    for node in nodes {
        let name = node["name"].as_str().unwrap_or_default();
        let ingresses = node["ingresses"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for ingress in ingresses {
            let endpoint = &ingress["endpoint"];
            let domain = endpoint["domain"].as_str().unwrap_or_default().trim();
            let ip = endpoint["ip"].as_str().unwrap_or_default().trim();
            let host = if domain.is_empty() { ip } else { domain };
            let port = endpoint["port"]
                .as_u64()
                .and_then(|p| u16::try_from(p).ok());
            let (false, Some(port)) = (host.is_empty(), port) else {
                continue;
            };
            let tls = ingress["tls"].as_object().map(|tls| {
                let server_name = tls
                    .get("server_name")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(host);
                TlsTarget {
                    server_name: server_name.to_string(),
                    verify: !tls
                        .get("insecure")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                }
            });
            let target = Target {
                label: format!(
                    "node {name} ({})",
                    ingress["endpoint_key"].as_str().unwrap_or_default()
                ),
                host: host.to_string(),
                port,
                tls,
            };
            let seen = targets
                .iter()
                .any(|t| t.host == target.host && t.port == target.port && t.tls == target.tls);
            if !seen {
                targets.push(target);
            }
        }
    }
    targets
}

/// One run's findings.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub mode: Mode,
    /// The VPN was active, or the host observed a connection transition
    /// during the run: probes may have used the tunnel.
    pub vpn_on: bool,
    pub hosts: Vec<HostReport>,
    /// The host enabled alternate API resolution for this app session.
    pub api_dns_recovery: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct HostReport {
    pub label: String,
    pub host: String,
    pub port: u16,
    /// The system resolver's, then each DoH resolver's (none for an IP).
    pub lookups: Vec<Lookup>,
    pub addresses: Vec<AddressCheck>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Lookup {
    /// [`SYSTEM`] or a [`DohResolver::name`].
    pub resolver: String,
    pub addresses: Vec<IpAddr>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub ms: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AddressCheck {
    pub address: IpAddr,
    /// The resolvers that gave it (empty for a target that is an IP).
    pub from: Vec<String>,
    pub tcp: Step,
    /// None without TLS, or when TCP failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls: Option<Step>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Step {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub ms: u64,
}

impl Report {
    /// One line of JSON, for the log.
    pub fn json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    /// The report as plain text, one block per host, for the apps' view
    /// and for pasting into a ticket.
    pub fn text(&self) -> String {
        let mut out = String::new();
        let resolvers = match self.mode {
            Mode::SystemOnly => "system resolver",
            Mode::Compare => "system resolver, AliDNS, DNSPod",
        };
        let _ = writeln!(out, "Resolvers: {resolvers}");
        if self.vpn_on {
            let _ = writeln!(
                out,
                "VPN active or connection state changed: probes may have used the tunnel"
            );
        }
        if self.api_dns_recovery {
            let _ = writeln!(out, "API DNS recovery enabled for this app session: system, AliDNS, DNSPod; TLS verified");
        }
        for host in &self.hosts {
            let _ = writeln!(out, "\n{} {}:{}", host.label, host.host, host.port);
            for lookup in &host.lookups {
                let answer = match &lookup.error {
                    Some(error) => error.clone(),
                    None if lookup.addresses.is_empty() => "no addresses".into(),
                    None => join(&lookup.addresses),
                };
                let _ = writeln!(out, "  {:<8} {answer} ({} ms)", lookup.resolver, lookup.ms);
            }
            for check in &host.addresses {
                let mut line = format!("  {:<40} tcp {}", check.address, step(&check.tcp));
                if let Some(tls) = &check.tls {
                    let _ = write!(line, ", tls {}", step(tls));
                }
                if !check.from.is_empty() && check.from.len() < host.lookups.len() {
                    let _ = write!(line, " [only {}]", check.from.join(", "));
                }
                let _ = writeln!(out, "{line}");
            }
        }
        out
    }
}

fn join(addresses: &[IpAddr]) -> String {
    addresses
        .iter()
        .map(IpAddr::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn step(step: &Step) -> String {
    match (&step.error, step.ok) {
        (_, true) => format!("ok {} ms", step.ms),
        (Some(error), false) => format!("{error} ({} ms)", step.ms),
        (None, false) => format!("failed ({} ms)", step.ms),
    }
}

/// Checks `targets` (at most [`MAX_TARGETS`]); takes up to a few times
/// [`STEP_TIMEOUT`] however many there are.
pub async fn run(targets: Vec<Target>, mode: Mode, vpn_on: bool) -> Report {
    let limit = Arc::new(tokio::sync::Semaphore::new(PARALLEL));
    let tls = tls_config().map(Arc::new);
    let doh = match mode {
        Mode::SystemOnly => None,
        Mode::Compare => doh_client(),
    };
    let mut tasks = tokio::task::JoinSet::new();
    for (index, target) in targets.into_iter().take(MAX_TARGETS).enumerate() {
        let (limit, tls, doh) = (limit.clone(), tls.clone(), doh.clone());
        tasks.spawn(async move { (index, check_host(target, limit, tls, doh).await) });
    }
    let mut hosts = Vec::new();
    while let Some(done) = tasks.join_next().await {
        if let Ok(host) = done {
            hosts.push(host);
        }
    }
    hosts.sort_by_key(|(index, _)| *index);
    Report {
        mode,
        vpn_on,
        hosts: hosts.into_iter().map(|(_, host)| host).collect(),
        api_dns_recovery: false,
    }
}

async fn check_host(
    target: Target,
    limit: Arc<tokio::sync::Semaphore>,
    tls: Result<Arc<rustls::ClientConfig>, String>,
    doh: Option<reqwest::Client>,
) -> HostReport {
    let mut lookups = Vec::new();
    // The resolvers each address came from, in the order first seen.
    let mut addresses: BTreeMap<IpAddr, (usize, Vec<String>)> = BTreeMap::new();
    if let Ok(ip) = target.host.parse::<IpAddr>() {
        addresses.insert(ip, (0, Vec::new()));
    } else {
        let mut asked = Vec::new();
        {
            let _permit = limit.acquire().await;
            asked.push(system_lookup(&target.host, target.port).await);
        }
        if let Some(client) = &doh {
            let mut queries = tokio::task::JoinSet::new();
            for (index, resolver) in DOH_RESOLVERS.iter().enumerate() {
                let (client, host, limit) = (client.clone(), target.host.clone(), limit.clone());
                queries.spawn(async move {
                    let _permit = limit.acquire().await;
                    (index, doh_lookup(&client, resolver, &host).await)
                });
            }
            let mut answers = Vec::new();
            while let Some(done) = queries.join_next().await {
                if let Ok(answer) = done {
                    answers.push(answer);
                }
            }
            answers.sort_by_key(|(index, _)| *index);
            asked.extend(answers.into_iter().map(|(_, lookup)| lookup));
        }
        for lookup in &asked {
            for ip in &lookup.addresses {
                let order = addresses.len();
                addresses
                    .entry(*ip)
                    .or_insert_with(|| (order, Vec::new()))
                    .1
                    .push(lookup.resolver.clone());
            }
        }
        lookups = asked;
    }
    let mut ordered: Vec<(IpAddr, usize, Vec<String>)> = addresses
        .into_iter()
        .map(|(ip, (order, from))| (ip, order, from))
        .collect();
    ordered.sort_by_key(|(_, order, _)| *order);
    let mut checks = tokio::task::JoinSet::new();
    for (index, (ip, _, from)) in ordered.into_iter().take(MAX_ADDRESSES).enumerate() {
        let (limit, tls, target) = (limit.clone(), tls.clone(), target.clone());
        checks.spawn(async move {
            let _permit = limit.acquire().await;
            (index, check_address(ip, from, &target, tls).await)
        });
    }
    let mut checked = Vec::new();
    while let Some(done) = checks.join_next().await {
        if let Ok(check) = done {
            checked.push(check);
        }
    }
    checked.sort_by_key(|(index, _)| *index);
    HostReport {
        label: target.label,
        host: target.host,
        port: target.port,
        lookups,
        addresses: checked.into_iter().map(|(_, check)| check).collect(),
    }
}

pub(crate) async fn system_lookup(host: &str, port: u16) -> Lookup {
    let started = Instant::now();
    let result = tokio::time::timeout(STEP_TIMEOUT, tokio::net::lookup_host((host, port))).await;
    let (addresses, error) = match result {
        Ok(Ok(found)) => {
            let mut addresses: Vec<IpAddr> = Vec::new();
            for address in found {
                if !addresses.contains(&address.ip()) {
                    addresses.push(address.ip());
                }
            }
            (addresses, None)
        }
        Ok(Err(error)) => (Vec::new(), Some(error.to_string())),
        Err(_) => (Vec::new(), Some("timed out".into())),
    };
    Lookup {
        resolver: SYSTEM.into(),
        addresses,
        error,
        ms: elapsed_ms(started),
    }
}

/// A client for the DoH queries: direct (no OS proxy), no redirects.
pub(crate) fn doh_client() -> Option<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(STEP_TIMEOUT)
        .connect_timeout(STEP_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .build()
        .ok()
}

/// A and AAAA from `resolver`, asked at once.
pub(crate) async fn doh_lookup(
    client: &reqwest::Client,
    resolver: &DohResolver,
    host: &str,
) -> Lookup {
    let started = Instant::now();
    let (a, aaaa) = tokio::join!(
        doh_query(client, resolver.url, host, RecordType::A),
        doh_query(client, resolver.url, host, RecordType::AAAA)
    );
    let mut addresses = Vec::new();
    let mut errors = Vec::new();
    for result in [a, aaaa] {
        match result {
            Ok(found) => addresses.extend(found),
            Err(error) => errors.push(error),
        }
    }
    // An error only when neither type got an answer.
    let error = (addresses.is_empty() && !errors.is_empty()).then(|| errors.join("; "));
    Lookup {
        resolver: resolver.name.into(),
        addresses,
        error,
        ms: elapsed_ms(started),
    }
}

async fn doh_query(
    client: &reqwest::Client,
    url: &str,
    host: &str,
    record_type: RecordType,
) -> Result<Vec<IpAddr>, String> {
    let name = Name::from_ascii(format!("{}.", host.trim_end_matches('.')))
        .map_err(|e| format!("bad name: {e:?}"))?;
    // ID 0, as RFC 8484 asks of GET queries.
    let mut query = Message::new(0, MessageType::Query, OpCode::Query);
    query.metadata.recursion_desired = true;
    query.add_query(Query::query(name, record_type));
    let wire = query.to_vec().map_err(|e| format!("{e:?}"))?;
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(wire);
    let response = client
        .get(url)
        .query(&[("dns", encoded)])
        .header(reqwest::header::ACCEPT, "application/dns-message")
        .send()
        .await
        .map_err(|e| request_error(&e))?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status().as_u16()));
    }
    let body = response.bytes().await.map_err(|e| request_error(&e))?;
    let answer = Message::from_vec(&body).map_err(|e| format!("bad answer: {e:?}"))?;
    if answer.metadata.response_code != ResponseCode::NoError {
        return Err(format!("{:?}", answer.metadata.response_code));
    }
    Ok(answer
        .answers
        .iter()
        .filter_map(|record| match &record.data {
            RData::A(a) => Some(IpAddr::V4(a.0)),
            RData::AAAA(aaaa) => Some(IpAddr::V6(aaaa.0)),
            _ => None,
        })
        .collect())
}

fn request_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timed out".into()
    } else if error.is_connect() {
        "connect failed".into()
    } else {
        "request failed".into()
    }
}

/// The web PKI's roots (as the API client's reqwest), ring as the provider.
pub(crate) fn tls_config() -> Result<rustls::ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    Ok(rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth())
}

pub(crate) async fn check_address(
    ip: IpAddr,
    from: Vec<String>,
    target: &Target,
    tls: Result<Arc<rustls::ClientConfig>, String>,
) -> AddressCheck {
    let started = Instant::now();
    let connected = tokio::time::timeout(
        STEP_TIMEOUT,
        TcpStream::connect(SocketAddr::new(ip, target.port)),
    )
    .await;
    let stream = match connected {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            return AddressCheck {
                address: ip,
                from,
                tcp: failed(io_error(&error), started),
                tls: None,
            }
        }
        Err(_) => {
            return AddressCheck {
                address: ip,
                from,
                tcp: failed("timed out".into(), started),
                tls: None,
            }
        }
    };
    let tcp = Step {
        ok: true,
        error: None,
        ms: elapsed_ms(started),
    };
    let tls = match &target.tls {
        None => None,
        Some(expect) => Some(handshake(stream, expect, tls).await),
    };
    AddressCheck {
        address: ip,
        from,
        tcp,
        tls,
    }
}

async fn handshake(
    stream: TcpStream,
    expect: &TlsTarget,
    config: Result<Arc<rustls::ClientConfig>, String>,
) -> Step {
    let started = Instant::now();
    let config = match config {
        Ok(config) => config,
        Err(error) => return failed(format!("no TLS: {error}"), started),
    };
    let server_name = match rustls::pki_types::ServerName::try_from(expect.server_name.clone()) {
        Ok(name) => name,
        Err(_) => return failed("bad server name".into(), started),
    };
    let connector = tokio_rustls::TlsConnector::from(config);
    match tokio::time::timeout(STEP_TIMEOUT, connector.connect(server_name, stream)).await {
        Ok(Ok(_)) => Step {
            ok: true,
            error: None,
            ms: elapsed_ms(started),
        },
        Ok(Err(error)) => {
            let certificate = error
                .get_ref()
                .and_then(|inner| inner.downcast_ref::<rustls::Error>())
                .and_then(|tls| match tls {
                    rustls::Error::InvalidCertificate(why) => Some(format!("{why:?}")),
                    _ => None,
                });
            match certificate {
                // The profile does not ask for a valid certificate: the
                // server answered TLS, which is what matters here.
                Some(_) if !expect.verify => Step {
                    ok: true,
                    error: Some("certificate not checked".into()),
                    ms: elapsed_ms(started),
                },
                Some(why) => failed(format!("certificate: {why}"), started),
                None => failed(io_error(&error), started),
            }
        }
        Err(_) => failed("timed out".into(), started),
    }
}

fn io_error(error: &std::io::Error) -> String {
    use std::io::ErrorKind;
    match error.kind() {
        ErrorKind::ConnectionRefused => "refused".into(),
        ErrorKind::ConnectionReset => "reset".into(),
        ErrorKind::ConnectionAborted => "aborted".into(),
        ErrorKind::TimedOut => "timed out".into(),
        ErrorKind::UnexpectedEof => "closed".into(),
        _ => error.to_string(),
    }
}

fn failed(error: String, started: Instant) -> Step {
    Step {
        ok: false,
        error: Some(error),
        ms: elapsed_ms(started),
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_api_target_is_its_host_with_tls_for_https() {
        let target = api_target("https://www.example.com/api/").unwrap();
        assert_eq!(
            (target.host.as_str(), target.port),
            ("www.example.com", 443)
        );
        assert_eq!(
            target.tls,
            Some(TlsTarget {
                server_name: "www.example.com".into(),
                verify: true
            })
        );
        let local = api_target("http://127.0.0.1:8080").unwrap();
        assert_eq!(
            (local.host.as_str(), local.port, local.tls),
            ("127.0.0.1", 8080, None)
        );
        assert!(api_target("not a url").is_none());
    }

    #[test]
    fn profile_targets_are_the_distinct_ingresses() {
        let profile = br#"{"nodes":[
            {"name":"Tokyo","ingresses":[
                {"endpoint_key":"t1","endpoint":{"domain":"t.example.com","port":443},
                 "tls":{"server_name":"cdn.example.com"}},
                {"endpoint_key":"t2","endpoint":{"ip":"203.0.113.7","port":8388}}]},
            {"name":"Osaka","ingresses":[
                {"endpoint_key":"o1","endpoint":{"domain":"t.example.com","port":443},
                 "tls":{"server_name":"cdn.example.com"}},
                {"endpoint_key":"o2","endpoint":{"domain":"o.example.com","port":443},
                 "tls":{"insecure":true}},
                {"endpoint_key":"bad","endpoint":{"port":443}}]}]}"#;
        let targets = profile_targets(profile);
        let hosts: Vec<_> = targets
            .iter()
            .map(|t| (t.label.as_str(), t.host.as_str(), t.port, t.tls.clone()))
            .collect();
        assert_eq!(
            hosts,
            [
                (
                    "node Tokyo (t1)",
                    "t.example.com",
                    443,
                    Some(TlsTarget {
                        server_name: "cdn.example.com".into(),
                        verify: true
                    })
                ),
                ("node Tokyo (t2)", "203.0.113.7", 8388, None),
                (
                    "node Osaka (o2)",
                    "o.example.com",
                    443,
                    Some(TlsTarget {
                        server_name: "o.example.com".into(),
                        verify: false
                    })
                ),
            ]
        );
        assert!(profile_targets(b"not json").is_empty());
    }

    #[test]
    fn the_doh_resolvers_are_addressed_by_ip() {
        for resolver in DOH_RESOLVERS {
            let url = reqwest::Url::parse(resolver.url).unwrap();
            assert_eq!(url.scheme(), "https");
            assert!(
                url.host_str().unwrap().parse::<IpAddr>().is_ok(),
                "{}",
                resolver.url
            );
        }
    }

    #[tokio::test]
    async fn an_ip_target_skips_the_resolvers_and_reports_tcp() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let closed = {
            let spare = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            spare.local_addr().unwrap().port()
        };
        let target = |port| Target {
            label: "t".into(),
            host: "127.0.0.1".into(),
            port,
            tls: None,
        };
        let report = run(vec![target(port), target(closed)], Mode::Compare, false).await;
        assert_eq!(report.hosts.len(), 2);
        let open = &report.hosts[0];
        assert!(open.lookups.is_empty());
        assert_eq!(open.addresses.len(), 1);
        assert!(open.addresses[0].tcp.ok);
        let refused = &report.hosts[1].addresses[0].tcp;
        assert!(!refused.ok);
        assert_eq!(refused.error.as_deref(), Some("refused"));
        let text = report.text();
        assert!(text.contains("t 127.0.0.1:"), "{text}");
        assert!(text.contains("refused"), "{text}");
        drop(listener);
    }

    #[tokio::test]
    async fn the_system_resolver_is_asked_for_a_host_name() {
        let target = Target {
            label: "local".into(),
            host: "localhost".into(),
            port: 9,
            tls: None,
        };
        let report = run(vec![target], Mode::SystemOnly, false).await;
        let lookups = &report.hosts[0].lookups;
        assert_eq!(lookups.len(), 1);
        assert_eq!(lookups[0].resolver, SYSTEM);
        assert!(
            lookups[0].addresses.iter().any(IpAddr::is_loopback),
            "{lookups:?}"
        );
    }
}
