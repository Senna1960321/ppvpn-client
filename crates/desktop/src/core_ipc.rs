//! Core API v1 calls: paths with JSON bodies, answered with data or an
//! error `{"code","field"?,"message","retryable"}`.
//!
//! Three transports implement [`CoreTransport`]: `engine::EngineTransport`
//! calls the in-process engine (standard mode),
//! `service::ServiceCoreTransport` forwards through the privileged service
//! (enhanced mode), and `tunnel::TunnelTransport` calls the tunnel host in
//! the phone's VPN process (#448). The typed helpers and the probe orchestration in this
//! module work over either.

use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use serde_json::{json, Value};
use tokio::time::Instant;

use crate::errors::{ClientError, ClientErrorInfo, ErrorCode};
use crate::{LocalProxy, ProbeMethod, ProbeResult};

pub(crate) type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Default per-call deadline for control calls.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Endpoint fetched through a node's local proxy by the Connect speed test.
/// Plain HTTP measures proxy RTT without adding a TLS handshake.
pub(crate) const CONNECT_PROBE_TARGET: &str = "http://www.gstatic.com/generate_204";

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Failure of one Core API call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CoreCallError {
    /// The core (or the service in front of it) could not be reached or spoke
    /// garbage. The string is for logs.
    Transport(String),
    /// The core answered `ok:false` (or the service rejected the forwarded
    /// call); `code` is the stable upstream code such as `NODE_NOT_FOUND`.
    Api {
        code: String,
        /// The profile field a validation error is about, e.g.
        /// `routing.rule_sets[0].url`.
        field: Option<String>,
        message: String,
        retryable: bool,
    },
}

impl CoreCallError {
    pub(crate) fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => Some(code),
            Self::Transport(_) => None,
        }
    }

    /// Log-safe detail: the upstream code (plus message) or transport reason.
    pub(crate) fn detail(&self) -> String {
        match self {
            Self::Transport(detail) => detail.clone(),
            Self::Api {
                code,
                field,
                message,
                ..
            } => {
                // `CODE field: message`: the code and the field lead, so a
                // rejected profile's log line names both (#532).
                let mut detail = code.clone();
                if let Some(field) = field {
                    detail.push(' ');
                    detail.push_str(field);
                }
                if !message.is_empty() {
                    detail.push_str(": ");
                    detail.push_str(message);
                }
                detail
            }
        }
    }

    /// Maps to a user-facing code, using `fallback` for anything that is not
    /// specifically recognised.
    pub(crate) fn info(&self, fallback: ErrorCode) -> ClientErrorInfo {
        let code = match self.code() {
            Some("NODE_NOT_FOUND") => ErrorCode::NodeNotFound,
            Some("PROFILE_EXPIRED") => ErrorCode::ProfileExpired,
            Some(code) if is_profile_error(code) => ErrorCode::ProfileInvalid,
            _ => fallback,
        };
        ClientErrorInfo::new(code, self.detail())
    }

    pub(crate) fn into_client_error(self, fallback: ErrorCode) -> ClientError {
        self.info(fallback).into()
    }
}

/// Core error codes that mean "the backend profile is not acceptable".
pub(crate) fn is_profile_error(code: &str) -> bool {
    ppvpn_core::codes::PROFILE_VALIDATION.contains(&code)
}

// ---------------------------------------------------------------------------
// Transport abstraction
// ---------------------------------------------------------------------------

/// One Core API round trip. `timeout` bounds the whole exchange.
pub(crate) trait CoreTransport: Send + Sync {
    fn call<'a>(
        &'a self,
        path: &'static str,
        body: Value,
        timeout: Duration,
    ) -> BoxFuture<'a, Result<Value, CoreCallError>>;
}

// ---------------------------------------------------------------------------
// Typed calls
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct CoreStatus {
    /// `stopped` | `configured` | `running` | `degraded` | `fatal`; see
    /// [`CoreStatus::is_serving`].
    pub state: String,
    pub revision: Option<String>,
    pub selected_node_id: Option<String>,
    pub node_count: u64,
    /// Replica the selected node's failover group currently uses, and the one
    /// before its latest switch. Proposed core addition (`selected_ingress`
    /// in GetStatus); `None` from cores that do not report it.
    pub selected_endpoint_key: Option<String>,
    /// Optional display name of that replica (`selected_ingress.label`).
    pub selected_endpoint_label: Option<String>,
    pub previous_endpoint_key: Option<String>,
    /// The unauthenticated loopback endpoint for the OS system proxy.
    pub system_proxy: Option<SystemProxyStatus>,
    /// `rule_sets` (core 0.5.0+), in profile order; empty when the profile
    /// declares none or the core does not report them.
    pub rule_sets: Vec<RuleSetStatus>,
    /// `nodes` (core 0.5.7+): each node's ingress pin and ingress health, in
    /// profile order; `None` from cores that do not report them.
    pub nodes: Option<Vec<crate::NodeIngresses>>,
    /// `tun_routing == "broken"`, or a `tun_routing_broken` degraded reason:
    /// on macOS and Windows another program changed the TUN's routes or
    /// address and nothing puts them back until the next start (the engine
    /// stays degraded); on Linux (Go core 0.5.20+) the rules could not be
    /// restored. Either way traffic may bypass the TUN. `ok`, `unguarded`
    /// (the guard did not start; nothing to gain from a restart) and absent
    /// are all false.
    pub tun_routing_broken: bool,
    /// `replaced_routes` (macOS TUN): another VPN's routes the TUN replaced
    /// for now; they are put back when the TUN stops. Empty when none.
    pub replaced_routes: Vec<String>,
}

impl CoreStatus {
    /// The core forwards traffic: `running`, or `degraded` (it keeps
    /// forwarding; its `reasons` say what is wrong).
    pub(crate) fn is_serving(&self) -> bool {
        matches!(self.state.as_str(), "running" | "degraded")
    }
}

/// One entry of `GetStatus.nodes`.
fn parse_node_ingresses(node: &Value) -> Option<crate::NodeIngresses> {
    Some(crate::NodeIngresses {
        node_id: opt_string(node, "node_id")?,
        pinned_endpoint_key: opt_string(node, "pinned_endpoint_key"),
        ingresses: node
            .get("ingresses")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|ingress| {
                Some(crate::IngressHealth {
                    endpoint_key: opt_string(ingress, "endpoint_key")?,
                    role: opt_string(ingress, "role").unwrap_or_default(),
                    label: opt_string(ingress, "label").filter(|label| !label.is_empty()),
                    healthy: ingress.get("healthy").and_then(Value::as_bool),
                    active: ingress
                        .get("active")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect(),
    })
}

/// One entry of `GetStatus.rule_sets`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RuleSetStatus {
    pub id: String,
    /// `ready` | `stale` | `unavailable`.
    pub state: String,
    pub updated_at: Option<String>,
    /// Stable code such as `RULE_SET_DOWNLOAD_FAILED`; only when not ready.
    pub error: Option<String>,
}

impl RuleSetStatus {
    fn parse(value: &Value) -> Option<Self> {
        Some(Self {
            id: opt_string(value, "id")?,
            state: opt_string(value, "state").unwrap_or_default(),
            updated_at: opt_string(value, "updated_at"),
            error: opt_string(value, "error"),
        })
    }
}

/// State a rule set has no usable copy in: the rules that reference it are
/// skipped, so that traffic goes through the proxy. `stale` still routes.
pub(crate) const RULE_SET_UNAVAILABLE: &str = "unavailable";

/// Ids of the rule sets without any usable copy, in profile order.
pub(crate) fn unavailable_rule_sets(rule_sets: &[RuleSetStatus]) -> Vec<String> {
    rule_sets
        .iter()
        .filter(|rule_set| rule_set.state == RULE_SET_UNAVAILABLE)
        .map(|rule_set| rule_set.id.clone())
        .collect()
}

/// Folds one Core API event into the unavailable rule-set ids. Only
/// `RuleSetChanged` (`rule_set_id`, new state in `message`) applies; returns
/// whether `unavailable` changed.
pub(crate) fn apply_rule_set_event(unavailable: &mut Vec<String>, event: &Value) -> bool {
    if event.get("type").and_then(Value::as_str) != Some("RuleSetChanged") {
        return false;
    }
    let Some(id) = opt_string(event, "rule_set_id") else {
        return false;
    };
    let now_unavailable =
        event.get("message").and_then(Value::as_str) == Some(RULE_SET_UNAVAILABLE);
    let listed = unavailable.iter().position(|known| *known == id);
    match (now_unavailable, listed) {
        (true, None) => {
            unavailable.push(id);
            true
        }
        (false, Some(index)) => {
            unavailable.remove(index);
            true
        }
        _ => false,
    }
}

/// `allowed_rule_set_hosts` for a profile fetched from `api_base`: its
/// authority, `host` or `host:port` (no scheme, path or userinfo). The port
/// is left out when it is 443, as the core treats `:443` and no port alike.
/// Empty when `api_base` has no host.
pub(crate) fn rule_set_hosts(api_base: &str) -> Vec<String> {
    let Ok(url) = reqwest::Url::parse(api_base.trim()) else {
        return Vec::new();
    };
    let Some(host) = url.host_str().filter(|host| !host.is_empty()) else {
        return Vec::new();
    };
    let host = host.to_ascii_lowercase();
    match url.port_or_known_default() {
        Some(443) | None => vec![host],
        Some(port) => vec![format!("{host}:{port}")],
    }
}

/// `SystemProxyStatus` of `set-system-proxy` / `get-status`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SystemProxyStatus {
    /// `false` on a core without the capability (the TUN core).
    pub available: bool,
    pub enabled: bool,
    pub listening: bool,
    pub listen: Option<String>,
    pub port: Option<u16>,
}

impl SystemProxyStatus {
    fn parse(value: &Value) -> Self {
        let flag = |key: &str| value.get(key).and_then(Value::as_bool).unwrap_or(false);
        Self {
            available: flag("available"),
            enabled: flag("enabled"),
            listening: flag("listening"),
            listen: opt_string(value, "listen"),
            port: value
                .get("port")
                .and_then(Value::as_u64)
                .and_then(|port| u16::try_from(port).ok())
                .filter(|port| *port > 0),
        }
    }

    /// `host:port` to write into the OS settings, once listening.
    pub(crate) fn endpoint(&self) -> Option<(String, u16)> {
        if !(self.enabled && self.listening) {
            return None;
        }
        Some((
            self.listen
                .clone()
                .unwrap_or_else(|| "127.0.0.1".to_string()),
            self.port?,
        ))
    }
}

/// Turns the core's loopback system-proxy endpoint on or off (idempotent).
pub(crate) async fn set_system_proxy(
    core: &dyn CoreTransport,
    enabled: bool,
) -> Result<SystemProxyStatus, CoreCallError> {
    let data = core
        .call(
            "/v1/set-system-proxy",
            json!({ "enabled": enabled }),
            CALL_TIMEOUT,
        )
        .await?;
    Ok(SystemProxyStatus::parse(&data))
}

fn opt_string(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

pub(crate) async fn get_status(core: &dyn CoreTransport) -> Result<CoreStatus, CoreCallError> {
    let data = core.call("/v1/get-status", json!({}), CALL_TIMEOUT).await?;
    Ok(parse_status(&data))
}

/// `GetStatus`'s data (also the tunnel host's `status` events).
pub(crate) fn parse_status(data: &Value) -> CoreStatus {
    // A status nested in a `core` object is accepted too.
    let status = data.get("core").unwrap_or(data);
    CoreStatus {
        state: opt_string(status, "state").unwrap_or_default(),
        revision: opt_string(status, "revision"),
        selected_node_id: opt_string(status, "selected_node_id"),
        node_count: status
            .get("node_count")
            .and_then(Value::as_u64)
            .unwrap_or(0),
        selected_endpoint_key: status
            .get("selected_ingress")
            .and_then(|ingress| opt_string(ingress, "endpoint_key")),
        system_proxy: status.get("system_proxy").map(SystemProxyStatus::parse),
        selected_endpoint_label: status
            .get("selected_ingress")
            .and_then(|ingress| opt_string(ingress, "label")),
        previous_endpoint_key: status
            .get("selected_ingress")
            .and_then(|ingress| opt_string(ingress, "previous_endpoint_key")),
        rule_sets: status
            .get("rule_sets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(RuleSetStatus::parse)
            .collect(),
        nodes: status
            .get("nodes")
            .and_then(Value::as_array)
            .map(|nodes| nodes.iter().filter_map(parse_node_ingresses).collect()),
        tun_routing_broken: opt_string(status, "tun_routing").as_deref() == Some("broken")
            || status
                .get("reasons")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|reason| {
                    reason.get("kind").and_then(Value::as_str) == Some("tun_routing_broken")
                }),
        replaced_routes: status
            .get("replaced_routes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
    }
}

/// The user's choices every `ApplyProfile` carries with the profile
/// (docs/host-integration.md 4.1): the engine applies them together, so a
/// new or recreated core needs no `SelectNode` / `PinIngress` afterwards.
/// Live changes still go through those calls.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub(crate) struct ApplyChoices {
    /// `None`: the profile's `default_node_id`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_node_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pins: Vec<crate::IngressPin>,
}

impl ApplyChoices {
    pub(crate) fn new(selected_node_id: Option<String>, pins: &crate::ingress::Pins) -> Self {
        Self {
            selected_node_id,
            pins: crate::ingress::records(pins),
        }
    }
}

/// Request body of `ApplyProfile` / `ValidateProfile`. `allowed_rule_set_hosts`
/// is sent only when given and non-empty: without it the core applies the
/// profile but downloads no rule set (`RULE_SET_HOST_NOT_PINNED`).
/// `selected_node_id` and `pins` are sent when set.
pub(crate) fn profile_request(
    profile: &Value,
    allowed_rule_set_hosts: Option<&[String]>,
    routing_mode: Option<crate::RoutingMode>,
    choices: &ApplyChoices,
) -> Value {
    let mut body = json!({ "profile": profile });
    if let Some(hosts) = allowed_rule_set_hosts.filter(|hosts| !hosts.is_empty()) {
        body["allowed_rule_set_hosts"] = json!(hosts);
    }
    if let Some(mode) = routing_mode {
        body["routing_mode"] = json!(crate::routing::wire_name(mode));
    }
    if let Some(node_id) = &choices.selected_node_id {
        body["selected_node_id"] = json!(node_id);
    }
    if !choices.pins.is_empty() {
        body["pins"] = json!(choices.pins);
    }
    body
}

/// `ApplyProfile`; returns the core's `applied` flag (false = same revision).
/// Rule-set downloads can hold the call
/// for up to 10 s, well inside [`CALL_TIMEOUT`].
pub(crate) async fn apply_profile(
    core: &dyn CoreTransport,
    profile: &Value,
    allowed_rule_set_hosts: Option<&[String]>,
    routing_mode: Option<crate::RoutingMode>,
    choices: &ApplyChoices,
) -> Result<bool, CoreCallError> {
    let data = core
        .call(
            "/v1/apply-profile",
            profile_request(profile, allowed_rule_set_hosts, routing_mode, choices),
            CALL_TIMEOUT,
        )
        .await?;
    Ok(data.get("applied").and_then(Value::as_bool).unwrap_or(true))
}

pub(crate) async fn start(core: &dyn CoreTransport) -> Result<(), CoreCallError> {
    core.call("/v1/start", json!({}), CALL_TIMEOUT)
        .await
        .map(|_| ())
}

pub(crate) async fn stop(core: &dyn CoreTransport, timeout: Duration) -> Result<(), CoreCallError> {
    core.call("/v1/stop", json!({}), timeout).await.map(|_| ())
}

/// Node ids known to the running profile, in profile order.
pub(crate) async fn list_node_ids(core: &dyn CoreTransport) -> Result<Vec<String>, CoreCallError> {
    let data = core.call("/v1/list-nodes", json!({}), CALL_TIMEOUT).await?;
    Ok(data
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|node| opt_string(node, "id"))
        .collect())
}

pub(crate) async fn select_node(
    core: &dyn CoreTransport,
    node_id: &str,
) -> Result<(), CoreCallError> {
    core.call(
        "/v1/select-node",
        json!({ "node_id": node_id }),
        CALL_TIMEOUT,
    )
    .await
    .map(|_| ())
}

/// How long a sleep or wake may take: the system does not wait for it.
const SLEEP_TIMEOUT: Duration = Duration::from_secs(3);

/// `Sleep` / `Wake` (core 0.6.3+, host-integration 4.7): the system goes
/// to sleep (`asleep`) or woke. An older core answers `CORE_API_UNSUPPORTED`.
pub(crate) async fn tell_sleep(
    core: &dyn CoreTransport,
    asleep: bool,
) -> Result<(), CoreCallError> {
    let path = if asleep { "/v1/sleep" } else { "/v1/wake" };
    core.call(path, json!({}), SLEEP_TIMEOUT).await.map(|_| ())
}

/// `PinIngress` (core 0.5.7+): pins `node_id` to `endpoint_key`, or back to
/// automatic failover with `None`. Applied live, no engine rebuild.
pub(crate) async fn pin_ingress(
    core: &dyn CoreTransport,
    node_id: &str,
    endpoint_key: Option<&str>,
) -> Result<(), CoreCallError> {
    core.call(
        "/v1/pin-ingress",
        json!({ "node_id": node_id, "endpoint_key": endpoint_key }),
        CALL_TIMEOUT,
    )
    .await
    .map(|_| ())
}

/// The running instance's cumulative counters, as the engine last read them
/// from its runtime (`Traffic`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct CoreTraffic {
    pub upload_bytes: u64,
    pub download_bytes: u64,
    /// When the engine read them (`Traffic::measured_at`). The engine reads
    /// its runtime about once a second while a host reads and stops 10 s
    /// after the last read, so the counters can be older than the call
    /// (docs/host-integration.md 4.4). `None` when missing or unreadable.
    pub measured_at: Option<SystemTime>,
}

impl CoreTraffic {
    fn parse(data: &Value) -> Self {
        let count = |key: &str| data.get(key).and_then(Value::as_u64).unwrap_or(0);
        CoreTraffic {
            upload_bytes: count("upload_bytes"),
            download_bytes: count("download_bytes"),
            measured_at: serde_json::from_value::<ppvpn_core::Traffic>(data.clone())
                .ok()
                .map(|traffic| SystemTime::from(traffic.measured_at)),
        }
    }

    /// `(upload_bytes, download_bytes)`.
    pub(crate) fn totals(&self) -> (u64, u64) {
        (self.upload_bytes, self.download_bytes)
    }

    /// Both directions together.
    pub(crate) fn total(&self) -> u64 {
        self.upload_bytes.saturating_add(self.download_bytes)
    }
}

/// Cumulative counters of the running instance, and when the engine read
/// them (see [`CoreTraffic::measured_at`]).
pub(crate) async fn get_traffic(core: &dyn CoreTransport) -> Result<CoreTraffic, CoreCallError> {
    let data = core
        .call("/v1/get-traffic", json!({}), Duration::from_secs(5))
        .await?;
    Ok(CoreTraffic::parse(&data))
}

/// Raw `ProbeEntrances` for the given nodes (`EntranceResult[]`).
pub(crate) async fn probe_entrances(
    core: &dyn CoreTransport,
    method: &str,
    node_ids: &[String],
    timeout_ms: u64,
    concurrency: u32,
) -> Result<Vec<Value>, CoreCallError> {
    let data = core
        .call(
            "/v1/probe-entrances",
            json!({
                "method": method,
                "timeout_ms": timeout_ms,
                "concurrency": concurrency,
                "node_ids": node_ids,
            }),
            Duration::from_millis(timeout_ms.saturating_mul(2)) + Duration::from_secs(5),
        )
        .await?;
    Ok(data.as_array().cloned().unwrap_or_default())
}

/// Raw `ProbeAvailability` (`AvailabilityResult`).
pub(crate) async fn probe_availability(
    core: &dyn CoreTransport,
    node_id: &str,
    target: &str,
    timeout_ms: u64,
) -> Result<Value, CoreCallError> {
    core.call(
        "/v1/probe-availability",
        json!({ "node_id": node_id, "target": target, "timeout_ms": timeout_ms }),
        Duration::from_millis(timeout_ms) + Duration::from_secs(5),
    )
    .await
}

/// Per-node loopback proxies with credentials. Metadata is read first; each
/// node's credential is then fetched individually (the bulk
/// `get-local-proxy-endpoints` route is deprecated in the core).
pub(crate) async fn local_proxies(
    core: &dyn CoreTransport,
) -> Result<Vec<LocalProxy>, CoreCallError> {
    let metadata = core
        .call("/v1/get-local-proxy-metadata", json!({}), CALL_TIMEOUT)
        .await?;
    let mut proxies = Vec::new();
    for entry in metadata.as_array().into_iter().flatten() {
        // 0.5.12+ lists the routed user too (`kind: routed`, no node).
        if opt_string(entry, "kind").is_some_and(|kind| kind != "node") {
            continue;
        }
        let Some(node_id) = opt_string(entry, "node_id") else {
            continue;
        };
        let credential = core
            .call(
                "/v1/get-local-proxy-credential",
                json!({ "node_id": node_id }),
                CALL_TIMEOUT,
            )
            .await?;
        if let Some(proxy) = parse_local_proxy(&credential, &node_id) {
            proxies.push(proxy);
        }
    }
    Ok(proxies)
}

/// The routed user of the shared local proxy: Profile rules, then the selected node, following
/// `routing_mode`. `node_id` is empty.
pub(crate) async fn routed_local_proxy(
    core: &dyn CoreTransport,
) -> Result<Option<LocalProxy>, CoreCallError> {
    let credential = core
        .call(
            "/v1/get-local-proxy-credential",
            json!({ "kind": "routed" }),
            CALL_TIMEOUT,
        )
        .await?;
    Ok(parse_local_proxy(&credential, ""))
}

/// Parses a `LocalProxyCredential`; `None` when port or secrets are missing.
pub(crate) fn parse_local_proxy(value: &Value, fallback_node: &str) -> Option<LocalProxy> {
    let port = value
        .get("port")
        .and_then(Value::as_u64)
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| *port > 0)?;
    Some(LocalProxy {
        node_id: opt_string(value, "node_id").unwrap_or_else(|| fallback_node.to_string()),
        host: opt_string(value, "listen")
            .or_else(|| opt_string(value, "host"))
            .unwrap_or_else(|| "127.0.0.1".to_string()),
        port,
        username: opt_string(value, "username")?,
        password: opt_string(value, "password")?,
    })
}

// ---------------------------------------------------------------------------
// Probe result mapping
// ---------------------------------------------------------------------------

/// Maps a core probe `error_code` (entrance or availability) to a user code.
pub(crate) fn probe_error_code(code: &str) -> ErrorCode {
    match code {
        "TIMEOUT" | "ICMP_TIMEOUT" => ErrorCode::Timeout,
        "CONNECT_FAILED" | "ICMP_UNREACHABLE" | "DNS_FAILED" | "PROXY_REQUEST_FAILED" => {
            ErrorCode::Unreachable
        }
        "ICMP_UNSUPPORTED" => ErrorCode::IcmpNotPermitted,
        "NODE_NOT_FOUND" => ErrorCode::NodeNotFound,
        _ => ErrorCode::ProbeFailed,
    }
}

fn latency(value: &Value, key: &str) -> Option<u32> {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|ms| u32::try_from(ms).unwrap_or(u32::MAX))
}

/// One `EntranceResult` → [`ProbeResult`]. `endpoint_key` is the replica the
/// node-level fields describe, reported only when it answered.
pub(crate) fn entrance_to_probe(value: &Value, method: ProbeMethod, node_id: &str) -> ProbeResult {
    let success = value.get("success").and_then(Value::as_bool) == Some(true);
    if success {
        return ProbeResult {
            node_id: node_id.to_string(),
            method,
            success: true,
            latency_ms: latency(value, "latency_ms").map(|ms| ms.max(1)),
            endpoint_key: opt_string(value, "endpoint_key"),
            error: None,
        };
    }
    let code = opt_string(value, "error_code").unwrap_or_else(|| "PROBE_FAILED".to_string());
    failed_probe(
        node_id,
        method,
        ClientErrorInfo::new(probe_error_code(&code), code),
    )
}

/// One `AvailabilityResult` → [`ProbeResult`].
pub(crate) fn availability_to_probe(value: &Value, node_id: &str) -> ProbeResult {
    let success = value.get("success").and_then(Value::as_bool) == Some(true);
    if success {
        return ProbeResult {
            node_id: node_id.to_string(),
            method: ProbeMethod::Connect,
            success: true,
            latency_ms: latency(value, "total_ms").map(|ms| ms.max(1)),
            endpoint_key: None,
            error: None,
        };
    }
    let code = opt_string(value, "error_code").unwrap_or_else(|| "PROBE_FAILED".to_string());
    let error_code = probe_error_code(&code);
    let detail = match value.get("http_status").and_then(Value::as_u64) {
        Some(status) if code == "HTTP_STATUS" => format!("HTTP_STATUS {status}"),
        _ => code,
    };
    failed_probe(
        node_id,
        ProbeMethod::Connect,
        ClientErrorInfo::new(error_code, detail),
    )
}

pub(crate) fn failed_probe(
    node_id: &str,
    method: ProbeMethod,
    error: ClientErrorInfo,
) -> ProbeResult {
    ProbeResult {
        node_id: node_id.to_string(),
        method,
        success: false,
        latency_ms: None,
        endpoint_key: None,
        error: Some(error),
    }
}

// ---------------------------------------------------------------------------
// Probe orchestration
// ---------------------------------------------------------------------------

/// Tuning for [`run_probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProbeOptions {
    /// Core-side timeout: per ingress for ICMP/TCP, per request for Connect.
    pub timeout_ms: u64,
    /// Nodes probed at once (one Core API call each).
    pub node_concurrency: usize,
    /// Ingresses of one node probed at once (ICMP/TCP only).
    pub ingress_concurrency: u32,
    /// Overall deadline; nodes without a result by then report `Timeout`.
    pub deadline: Duration,
}

impl ProbeOptions {
    pub(crate) fn for_method(method: ProbeMethod, node_count: usize) -> Self {
        let (timeout_ms, node_concurrency) = match method {
            ProbeMethod::Icmp | ProbeMethod::Tcp => (3_000u64, 16usize),
            ProbeMethod::Connect => (8_000, 6),
        };
        let waves = node_count.max(1).div_ceil(node_concurrency) as u64;
        // An ICMP/TCP wave may take two ingress timeouts (more replicas than
        // ingress concurrency); cap the whole test so the UI never hangs.
        let per_wave = match method {
            ProbeMethod::Connect => timeout_ms,
            ProbeMethod::Icmp | ProbeMethod::Tcp => timeout_ms * 2,
        };
        let deadline_ms = (waves * per_wave + 2_000).min(60_000);
        Self {
            timeout_ms,
            node_concurrency,
            ingress_concurrency: 4,
            deadline: Duration::from_millis(deadline_ms),
        }
    }
}

fn method_name(method: ProbeMethod) -> &'static str {
    match method {
        ProbeMethod::Icmp => "icmp",
        ProbeMethod::Tcp => "tcp",
        ProbeMethod::Connect => "connect",
    }
}

async fn probe_one(
    core: &dyn CoreTransport,
    method: ProbeMethod,
    node_id: &str,
    options: ProbeOptions,
) -> ProbeResult {
    match method {
        ProbeMethod::Connect => {
            match probe_availability(core, node_id, CONNECT_PROBE_TARGET, options.timeout_ms).await
            {
                Ok(value) => availability_to_probe(&value, node_id),
                Err(error) => failed_probe(node_id, method, error.info(ErrorCode::ProbeFailed)),
            }
        }
        ProbeMethod::Icmp | ProbeMethod::Tcp => {
            let ids = [node_id.to_string()];
            let results = probe_entrances(
                core,
                method_name(method),
                &ids,
                options.timeout_ms,
                options.ingress_concurrency,
            )
            .await;
            match results {
                Ok(results) => match results
                    .iter()
                    .find(|result| result.get("node_id").and_then(Value::as_str) == Some(node_id))
                    .or_else(|| results.first())
                {
                    Some(result) => entrance_to_probe(result, method, node_id),
                    None => failed_probe(
                        node_id,
                        method,
                        ClientErrorInfo::new(ErrorCode::ProbeFailed, "EMPTY_PROBE_RESULT"),
                    ),
                },
                Err(error) => failed_probe(node_id, method, error.info(ErrorCode::ProbeFailed)),
            }
        }
    }
}

/// Probes `node_ids` (empty = every node of the running profile) and delivers
/// exactly one [`ProbeResult`] per distinct requested node through
/// `on_result`, in completion order. Returns only after every node has been
/// delivered; nodes still pending at the deadline report
/// [`ErrorCode::Timeout`]. Fails only when the node list for an empty request
/// cannot be read.
/// Gap between the starts of a latency test's first nodes (#549): 16 nodes
/// at once sent every first handshake in one burst, which a slow uplink
/// queued and every sample then counted. Later nodes start as slots free.
fn start_gap(method: ProbeMethod) -> Duration {
    match method {
        ProbeMethod::Icmp | ProbeMethod::Tcp => Duration::from_millis(20),
        ProbeMethod::Connect => Duration::ZERO,
    }
}

pub(crate) async fn run_probe(
    core: Arc<dyn CoreTransport>,
    method: ProbeMethod,
    node_ids: Vec<String>,
    options: Option<ProbeOptions>,
    on_result: &(dyn Fn(ProbeResult) + Send + Sync),
) -> Result<(), ClientError> {
    let node_ids = if node_ids.is_empty() {
        list_node_ids(core.as_ref())
            .await
            .map_err(|error| error.into_client_error(ErrorCode::StandardCoreFailed))?
    } else {
        let mut seen = HashSet::new();
        node_ids
            .into_iter()
            .filter(|id| seen.insert(id.clone()))
            .collect()
    };
    if node_ids.is_empty() {
        return Ok(());
    }
    let options = options.unwrap_or_else(|| ProbeOptions::for_method(method, node_ids.len()));
    let deadline = Instant::now() + options.deadline;

    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<ProbeResult>();
    let semaphore = Arc::new(tokio::sync::Semaphore::new(options.node_concurrency.max(1)));
    let mut tasks = tokio::task::JoinSet::new();
    for (index, node_id) in node_ids.iter().enumerate() {
        let node_id = node_id.clone();
        let core = core.clone();
        let sender = sender.clone();
        let semaphore = semaphore.clone();
        let wait = start_gap(method) * index.min(options.node_concurrency) as u32;
        tasks.spawn(async move {
            tokio::time::sleep(wait).await;
            let Ok(_permit) = semaphore.acquire_owned().await else {
                return;
            };
            let result = probe_one(core.as_ref(), method, &node_id, options).await;
            let _ = sender.send(result);
        });
    }
    drop(sender);

    let mut pending: HashSet<String> = node_ids.iter().cloned().collect();
    while !pending.is_empty() {
        match tokio::time::timeout_at(deadline, receiver.recv()).await {
            Ok(Some(result)) => {
                if pending.remove(&result.node_id) {
                    on_result(result);
                }
            }
            // Every task ended (or panicked) without reporting some nodes, or
            // the deadline passed: the rest are filled below.
            Ok(None) | Err(_) => break,
        }
    }
    tasks.abort_all();
    for node_id in node_ids {
        if pending.remove(&node_id) {
            on_result(failed_probe(
                &node_id,
                method,
                ClientErrorInfo::new(ErrorCode::Timeout, "PROBE_DEADLINE"),
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    type Handler =
        Box<dyn Fn(&str, &Value) -> (Duration, Result<Value, CoreCallError>) + Send + Sync>;

    /// Scripted transport: `handler(path, body)` returns a delay and a result.
    pub(crate) struct FakeCore {
        handler: Handler,
        pub calls: Mutex<Vec<(String, Value)>>,
    }

    impl FakeCore {
        pub(crate) fn new(
            handler: impl Fn(&str, &Value) -> (Duration, Result<Value, CoreCallError>)
                + Send
                + Sync
                + 'static,
        ) -> Arc<Self> {
            Arc::new(Self {
                handler: Box::new(handler),
                calls: Mutex::new(Vec::new()),
            })
        }
    }

    impl CoreTransport for FakeCore {
        fn call<'a>(
            &'a self,
            path: &'static str,
            body: Value,
            _timeout: Duration,
        ) -> BoxFuture<'a, Result<Value, CoreCallError>> {
            self.calls
                .lock()
                .unwrap()
                .push((path.to_string(), body.clone()));
            let (delay, result) = (self.handler)(path, &body);
            Box::pin(async move {
                if !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                result
            })
        }
    }

    #[allow(clippy::type_complexity)]
    fn collect() -> (
        Arc<Mutex<Vec<ProbeResult>>>,
        Box<dyn Fn(ProbeResult) + Send + Sync>,
    ) {
        let results = Arc::new(Mutex::new(Vec::new()));
        let sink = results.clone();
        (
            results,
            Box::new(move |result| sink.lock().unwrap().push(result)),
        )
    }

    #[test]
    fn rule_set_hosts_is_the_api_base_authority() {
        let hosts = |base: &str| rule_set_hosts(base);
        assert_eq!(hosts("https://api.example.com"), ["api.example.com"]);
        assert_eq!(hosts("https://www.example.com/"), ["www.example.com"]);
        assert_eq!(
            hosts(" https://API.Example.com:443/api/v1?x=1 "),
            ["api.example.com"]
        );
        assert_eq!(
            hosts("https://api.example.com:8443/api"),
            ["api.example.com:8443"]
        );
        assert_eq!(
            hosts("https://user:pw@api.example.com"),
            ["api.example.com"]
        );
        assert_eq!(hosts("http://127.0.0.1:8080"), ["127.0.0.1:8080"]);
        assert_eq!(hosts("http://localhost"), ["localhost:80"]);
        assert_eq!(hosts("https://[2001:db8::1]:9443"), ["[2001:db8::1]:9443"]);
        assert!(hosts("not a url").is_empty());
    }

    #[test]
    fn rule_set_rejections_are_profile_errors() {
        let rejected = CoreCallError::Api {
            code: "RULE_SET_HOST_NOT_ALLOWED".into(),
            field: None,
            message: "rule set cn-ip host cdn.example.com is not allowed".into(),
            retryable: false,
        };
        assert_eq!(
            rejected.info(ErrorCode::StandardCoreFailed).code,
            ErrorCode::ProfileInvalid
        );
        assert!(is_profile_error("RULE_SET_HOSTS_INVALID"));
        assert!(is_profile_error("RULE_SET_NOT_FOUND"));
    }

    #[test]
    fn the_detail_names_the_code_and_the_field() {
        let rejected = CoreCallError::Api {
            code: "RULE_SET_HOST_NOT_ALLOWED".into(),
            field: Some("routing.rule_sets[0].url".into()),
            message: "rule set host must be the host the profile came from".into(),
            retryable: false,
        };
        assert_eq!(
            rejected.info(ErrorCode::StandardCoreFailed).detail,
            "RULE_SET_HOST_NOT_ALLOWED routing.rule_sets[0].url: \
             rule set host must be the host the profile came from"
        );
        let bare = CoreCallError::Api {
            code: "PROFILE_REQUIRED".into(),
            field: None,
            message: String::new(),
            retryable: false,
        };
        assert_eq!(bare.detail(), "PROFILE_REQUIRED");
    }

    fn proxy_core() -> Arc<FakeCore> {
        FakeCore::new(|path, body| {
            let credential = |node: &str, user: &str| {
                json!({"node_id": node, "listen": "127.0.0.1", "port": 7890,
                       "username": user, "password": "secret"})
            };
            let value = match path {
                "/v1/get-local-proxy-metadata" => json!([
                    {"kind": "node", "node_id": "n1"},
                    {"kind": "routed", "node_id": ""},
                ]),
                "/v1/get-local-proxy-credential" => match body["kind"].as_str() {
                    Some("routed") => {
                        let mut value = credential("", "abc");
                        value["kind"] = json!("routed");
                        value
                    }
                    _ => credential(body["node_id"].as_str().unwrap_or_default(), "abc-n1"),
                },
                _ => json!({}),
            };
            (Duration::ZERO, Ok(value))
        })
    }

    #[tokio::test]
    async fn per_node_proxies_leave_the_routed_user_out() {
        let core = proxy_core();
        let proxies = local_proxies(core.as_ref()).await.unwrap();
        assert_eq!(proxies.len(), 1);
        assert_eq!(proxies[0].node_id, "n1");
        assert_eq!(proxies[0].username, "abc-n1");
        let calls = core.calls.lock().unwrap();
        assert!(
            calls.iter().all(|(_, body)| body.get("kind").is_none()),
            "no kind sent for nodes"
        );
    }

    #[tokio::test]
    async fn the_routed_user_is_fetched_by_kind() {
        let core = proxy_core();
        let routed = routed_local_proxy(core.as_ref()).await.unwrap().unwrap();
        assert_eq!(
            (
                routed.node_id.as_str(),
                routed.username.as_str(),
                routed.port
            ),
            ("", "abc", 7890)
        );
        let calls = core.calls.lock().unwrap();
        assert_eq!(calls[0].1, json!({"kind": "routed"}));
    }

    #[tokio::test]
    async fn apply_body_carries_the_hosts_only_when_given() {
        let core = FakeCore::new(|_, _| (Duration::ZERO, Ok(json!({"applied": true}))));
        let profile = json!({"revision": "r1"});
        let hosts = rule_set_hosts("https://api.example.com:8443");
        let none = ApplyChoices::default();
        apply_profile(core.as_ref(), &profile, Some(&hosts), None, &none)
            .await
            .unwrap();
        apply_profile(core.as_ref(), &profile, None, None, &none)
            .await
            .unwrap();
        apply_profile(core.as_ref(), &profile, Some(&[]), None, &none)
            .await
            .unwrap();
        let bodies: Vec<Value> = core
            .calls
            .lock()
            .unwrap()
            .iter()
            .map(|(_, b)| b.clone())
            .collect();
        assert_eq!(
            bodies,
            vec![
                json!({"profile": {"revision": "r1"}, "allowed_rule_set_hosts": ["api.example.com:8443"]}),
                json!({"profile": {"revision": "r1"}}),
                json!({"profile": {"revision": "r1"}}),
            ]
        );
    }

    #[tokio::test]
    async fn apply_body_carries_the_selection_and_the_pins() {
        let core = FakeCore::new(|_, _| (Duration::ZERO, Ok(json!({"applied": true}))));
        let profile = json!({"revision": "r1"});
        let pins = crate::ingress::Pins::from([
            ("n2".to_string(), "k3".to_string()),
            ("n1".to_string(), "k2".to_string()),
        ]);
        let choices = ApplyChoices::new(Some("n2".into()), &pins);
        apply_profile(
            core.as_ref(),
            &profile,
            None,
            Some(crate::RoutingMode::Global),
            &choices,
        )
        .await
        .unwrap();
        assert_eq!(
            core.calls.lock().unwrap()[0].1,
            json!({"profile": {"revision": "r1"}, "routing_mode": "global",
                   "selected_node_id": "n2",
                   "pins": [{"node_id": "n1", "endpoint_key": "k2"},
                            {"node_id": "n2", "endpoint_key": "k3"}]})
        );
    }

    #[tokio::test]
    async fn status_parses_rule_sets_when_present() {
        let core = FakeCore::new(|_, _| {
            (
                Duration::ZERO,
                Ok(json!({"state": "running", "node_count": 3,
                "rule_sets": [
                    {"id": "cn-ip", "state": "ready", "updated_at": "2026-07-23T12:00:00Z"},
                    {"id": "cn-site", "state": "unavailable", "error": "RULE_SET_DOWNLOAD_FAILED"},
                    {"id": "ads", "state": "stale", "error": "RULE_SET_HTTP_STATUS"},
                    {"state": "unavailable"}
                ]})),
            )
        });
        let status = get_status(core.as_ref()).await.unwrap();
        assert_eq!(
            status.rule_sets.len(),
            3,
            "entries without an id are dropped"
        );
        assert_eq!(
            status.rule_sets[0],
            RuleSetStatus {
                id: "cn-ip".into(),
                state: "ready".into(),
                updated_at: Some("2026-07-23T12:00:00Z".into()),
                error: None,
            }
        );
        assert_eq!(
            status.rule_sets[1].error.as_deref(),
            Some("RULE_SET_DOWNLOAD_FAILED")
        );
        assert_eq!(unavailable_rule_sets(&status.rule_sets), ["cn-site"]);
    }

    #[tokio::test]
    async fn status_without_rule_sets_has_none() {
        let core = FakeCore::new(|_, _| {
            (
                Duration::ZERO,
                Ok(json!({"state": "running", "node_count": 3})),
            )
        });
        let status = get_status(core.as_ref()).await.unwrap();
        assert!(status.rule_sets.is_empty());
        assert!(unavailable_rule_sets(&status.rule_sets).is_empty());
        assert!(!status.tun_routing_broken);
        assert!(status.replaced_routes.is_empty());
    }

    async fn status_of(body: Value) -> CoreStatus {
        let core = FakeCore::new(move |_, _| (Duration::ZERO, Ok(body.clone())));
        get_status(core.as_ref()).await.unwrap()
    }

    #[tokio::test]
    async fn status_reads_broken_tun_routing_and_replaced_routes() {
        // engine-host reports a degraded engine as running, reasons kept.
        let status = status_of(json!({"state": "running", "tun_routing": "broken",
            "reasons": [{"kind": "tun_routing_broken", "missing": ["utun4: route"]}],
            "replaced_routes": ["route 128.0.0.0/1 via 192.0.2.1 on utun4"]}))
        .await;
        assert!(status.tun_routing_broken);
        assert_eq!(
            status.replaced_routes,
            ["route 128.0.0.0/1 via 192.0.2.1 on utun4"]
        );
        // The reason alone, in the engine's own `degraded` state.
        let status = status_of(json!({"state": "degraded",
            "reasons": [{"kind": "tun_routing_broken", "missing": []}]}))
        .await;
        assert!(status.tun_routing_broken);
        // Self-healing reasons are not broken routing.
        let status = status_of(json!({"state": "running", "tun_routing": "unguarded",
            "reasons": [{"kind": "tun_routing_unguarded"}, {"kind": "no_default_interface"}]}))
        .await;
        assert!(!status.tun_routing_broken);
    }

    #[test]
    fn rule_set_events_track_unavailable_ids() {
        let event = |id: &str, state: &str| {
            json!({"type": "RuleSetChanged", "at": "2026-07-23T12:00:00Z",
                   "rule_set_id": id, "message": state, "code": "RULE_SET_DOWNLOAD_FAILED"})
        };
        let mut unavailable = Vec::new();
        assert!(apply_rule_set_event(
            &mut unavailable,
            &event("cn-site", "unavailable")
        ));
        assert!(!apply_rule_set_event(
            &mut unavailable,
            &event("cn-site", "unavailable")
        ));
        assert!(apply_rule_set_event(
            &mut unavailable,
            &event("cn-ip", "unavailable")
        ));
        assert_eq!(unavailable, ["cn-site", "cn-ip"]);
        // Stale still routes: it counts as loaded.
        assert!(apply_rule_set_event(
            &mut unavailable,
            &event("cn-site", "stale")
        ));
        assert!(apply_rule_set_event(
            &mut unavailable,
            &event("cn-ip", "ready")
        ));
        assert!(unavailable.is_empty());
        // Other events and malformed ones change nothing.
        assert!(!apply_rule_set_event(
            &mut unavailable,
            &json!({"type": "NodeSelected", "rule_set_id": "x", "message": "unavailable"})
        ));
        assert!(!apply_rule_set_event(
            &mut unavailable,
            &json!({"type": "RuleSetChanged", "message": "unavailable"})
        ));
        assert!(unavailable.is_empty());
    }

    #[test]
    fn api_errors_keep_the_core_code() {
        let error = CoreCallError::Api {
            code: "NODE_NOT_FOUND".into(),
            field: None,
            message: "node not found".into(),
            retryable: false,
        };
        assert_eq!(error.code(), Some("NODE_NOT_FOUND"));
        assert_eq!(
            error.info(ErrorCode::ProbeFailed).code,
            ErrorCode::NodeNotFound
        );
        let profile = CoreCallError::Api {
            code: "ENDPOINT_KEY_DUPLICATE".into(),
            field: None,
            message: String::new(),
            retryable: false,
        };
        let info = profile.info(ErrorCode::StandardCoreFailed);
        assert_eq!(info.code, ErrorCode::ProfileInvalid);
        assert_eq!(info.detail, "ENDPOINT_KEY_DUPLICATE");
    }

    #[test]
    fn entrance_results_map_replica_and_codes() {
        let ok = json!({"node_id":"n1","success":true,"latency_ms":95,"endpoint_key":"9002","ingress_role":"backup"});
        let result = entrance_to_probe(&ok, ProbeMethod::Icmp, "n1");
        assert!(result.success);
        assert_eq!(result.latency_ms, Some(95));
        assert_eq!(result.endpoint_key.as_deref(), Some("9002"));

        let denied = json!({"node_id":"n1","success":false,"latency_ms":0,"error_code":"ICMP_UNSUPPORTED","endpoint_key":"9001"});
        let result = entrance_to_probe(&denied, ProbeMethod::Icmp, "n1");
        assert!(!result.success);
        assert_eq!(result.endpoint_key, None);
        assert_eq!(result.error.unwrap().code, ErrorCode::IcmpNotPermitted);

        for (code, expected) in [
            ("TIMEOUT", ErrorCode::Timeout),
            ("ICMP_TIMEOUT", ErrorCode::Timeout),
            ("CONNECT_FAILED", ErrorCode::Unreachable),
            ("ICMP_UNREACHABLE", ErrorCode::Unreachable),
            ("DNS_FAILED", ErrorCode::Unreachable),
            ("ICMP_FAILED", ErrorCode::ProbeFailed),
            ("CANCELED", ErrorCode::ProbeFailed),
        ] {
            assert_eq!(probe_error_code(code), expected, "{code}");
        }

        let http =
            json!({"node_id":"n1","success":false,"error_code":"HTTP_STATUS","http_status":503});
        let error = availability_to_probe(&http, "n1").error.unwrap();
        assert_eq!(error.code, ErrorCode::ProbeFailed);
        assert_eq!(error.detail, "HTTP_STATUS 503");
        let ok = json!({"node_id":"n1","success":true,"total_ms":241,"http_status":204});
        assert_eq!(availability_to_probe(&ok, "n1").latency_ms, Some(241));
    }

    #[test]
    fn local_proxy_requires_both_secrets() {
        let full =
            json!({"node_id":"a","listen":"127.0.0.1","port":32145,"username":"u","password":"p"});
        let proxy = parse_local_proxy(&full, "a").unwrap();
        assert_eq!((proxy.host.as_str(), proxy.port), ("127.0.0.1", 32145));
        let missing = json!({"node_id":"a","listen":"127.0.0.1","port":32145,"username":"u"});
        assert!(parse_local_proxy(&missing, "a").is_none());
        let bad_port = json!({"node_id":"a","port":0,"username":"u","password":"p"});
        assert!(parse_local_proxy(&bad_port, "a").is_none());
    }

    #[tokio::test]
    async fn probe_delivers_one_result_per_node_and_fills_timeouts() {
        let core = FakeCore::new(|path, body| {
            assert_eq!(path, "/v1/probe-entrances");
            assert_eq!(body["method"], "tcp");
            match body["node_ids"][0].as_str().unwrap() {
                "fast" => (
                    Duration::from_millis(5),
                    Ok(
                        json!([{"node_id":"fast","success":true,"latency_ms":12,"endpoint_key":"k1"}]),
                    ),
                ),
                "gone" => (
                    Duration::ZERO,
                    Err(CoreCallError::Api {
                        code: "NODE_NOT_FOUND".into(),
                        field: None,
                        message: String::new(),
                        retryable: false,
                    }),
                ),
                _ => (Duration::from_secs(30), Ok(json!([]))),
            }
        });
        let (results, sink) = collect();
        let options = ProbeOptions {
            timeout_ms: 1000,
            node_concurrency: 4,
            ingress_concurrency: 4,
            deadline: Duration::from_millis(200),
        };
        let ids = vec!["fast".into(), "slow".into(), "gone".into(), "fast".into()];
        run_probe(core, ProbeMethod::Tcp, ids, Some(options), sink.as_ref())
            .await
            .unwrap();

        let results = results.lock().unwrap();
        assert_eq!(results.len(), 3, "duplicates collapse to one result each");
        let by_id: HashMap<_, _> = results.iter().map(|r| (r.node_id.as_str(), r)).collect();
        assert!(by_id["fast"].success);
        assert_eq!(by_id["fast"].endpoint_key.as_deref(), Some("k1"));
        assert_eq!(
            by_id["gone"].error.as_ref().unwrap().code,
            ErrorCode::NodeNotFound
        );
        assert_eq!(
            by_id["slow"].error.as_ref().unwrap().code,
            ErrorCode::Timeout
        );
        assert_eq!(
            results.last().unwrap().node_id,
            "slow",
            "timeout filler comes last"
        );
    }

    #[tokio::test]
    async fn empty_probe_request_expands_to_every_node() {
        let core = FakeCore::new(|path, body| match path {
            "/v1/list-nodes" => (Duration::ZERO, Ok(json!([{"id":"a"},{"id":"b"}]))),
            "/v1/probe-availability" => {
                assert_eq!(body["target"], CONNECT_PROBE_TARGET);
                let node = body["node_id"].as_str().unwrap();
                (
                    Duration::ZERO,
                    Ok(json!({"node_id":node,"success":true,"total_ms":50})),
                )
            }
            other => panic!("unexpected {other}"),
        });
        let (results, sink) = collect();
        run_probe(core, ProbeMethod::Connect, Vec::new(), None, sink.as_ref())
            .await
            .unwrap();
        let mut ids: Vec<_> = results
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.node_id.clone())
            .collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn transport_failure_still_reports_every_node() {
        let core = FakeCore::new(|_, _| {
            (
                Duration::ZERO,
                Err(CoreCallError::Transport("connect: refused".into())),
            )
        });
        let (results, sink) = collect();
        run_probe(
            core,
            ProbeMethod::Icmp,
            vec!["a".into(), "b".into()],
            None,
            sink.as_ref(),
        )
        .await
        .unwrap();
        let results = results.lock().unwrap();
        assert_eq!(results.len(), 2);
        assert!(results
            .iter()
            .all(|r| r.error.as_ref().unwrap().code == ErrorCode::ProbeFailed));
    }

    #[test]
    fn default_deadline_scales_and_is_capped() {
        let small = ProbeOptions::for_method(ProbeMethod::Tcp, 3);
        assert_eq!(small.deadline, Duration::from_millis(8_000));
        let huge = ProbeOptions::for_method(ProbeMethod::Connect, 500);
        assert_eq!(huge.deadline, Duration::from_secs(60));
    }

    #[tokio::test]
    async fn traffic_carries_when_the_engine_read_it() {
        let core = FakeCore::new(|_, _| {
            (
                Duration::ZERO,
                Ok(json!({
                    "upload_bytes": 3,
                    "download_bytes": 4,
                    "measured_at": "2026-10-07T08:00:01.500Z"
                })),
            )
        });
        let traffic = get_traffic(core.as_ref()).await.unwrap();
        assert_eq!(traffic.totals(), (3, 4));
        assert_eq!(traffic.total(), 7);
        let days = crate::logging::days_from_civil(2026, 10, 7) as u64;
        assert_eq!(
            traffic.measured_at,
            Some(
                std::time::UNIX_EPOCH
                    + Duration::from_secs(days * 86_400 + 8 * 3_600 + 1)
                    + Duration::from_millis(500)
            )
        );
        assert_eq!(core.calls.lock().unwrap()[0].0, "/v1/get-traffic");

        // Undated (or not RFC 3339): the counters still count, the date is unknown.
        for measured_at in [Value::Null, json!("yesterday")] {
            let core = FakeCore::new(move |_, _| {
                (
                    Duration::ZERO,
                    Ok(
                        json!({"upload_bytes": 1, "download_bytes": 2, "measured_at": measured_at.clone()}),
                    ),
                )
            });
            let traffic = get_traffic(core.as_ref()).await.unwrap();
            assert_eq!((traffic.totals(), traffic.measured_at), ((1, 2), None));
        }
    }
}
