//! PPVPN desktop client logic: the Flutter app's bridge and push agent
//! (apps/flutter) link it, the macOS app and push agent use it through UniFFI.
//!
//! The UI owns nothing but presentation: every state change arrives as a full
//! [`ClientSnapshot`] through [`ClientListener::on_snapshot`], and everything
//! platform-specific (credential storage, opening the browser, installing the
//! privileged service) is injected through [`PlatformHooks`].

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, Weak};

mod android_push;
mod api;
mod auth;
mod compat;
mod connection;
mod core_ipc;
mod cores;
mod detect;
mod device;
#[cfg(all(test, target_os = "linux", feature = "linux-e2e"))]
mod e2e_linux;
mod engine;
mod enhanced;
mod errors;
mod ingress;
mod logging;
mod monitor;
mod notifications;
mod push_agent;
mod push_shown;
mod reachability;
mod routing;
mod saved_profile;
mod service;
mod session;
pub mod silent_update;

/// The log file's subscriber before a [`Client`] exists (the app's install
/// of a silent update at launch, #548); [`Client::new`] then finds it there.
pub fn install_logging(log_dir: &str, app_version: &str) {
    logging::install(log_dir, app_version);
}
mod standard;
mod storage;
mod sysproxy;
#[cfg(test)]
mod test_backend;
mod testmode;
mod traffic;
mod tunnel;
mod tunnel_mode;

pub use detect::ConflictReport;
pub use errors::{ClientError, ClientErrorInfo, ErrorCode};
pub use push_agent::{PushAgent, PushAgentConfig, PushAgentListener, PushAgentState, PushMessage};

uniffi::setup_scaffolding!();

// ---------------------------------------------------------------------------
// Configuration and platform hooks
// ---------------------------------------------------------------------------

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct ClientConfig {
    /// Backend base URL, e.g. `https://www.example.com`.
    pub api_base: String,
    /// Writable per-user directory for core state (`core/`) and caches.
    pub data_dir: String,
    /// Directory for the daily-rotated logs (UTC dates, 7 days kept):
    /// `ppvpn-client.YYYY-MM-DD.log` written by this library and
    /// `ppvpn-core.YYYY-MM-DD.log` written by the standard-mode engine. Level
    /// `info`; override with the `PPVPN_LOG` environment variable.
    pub log_dir: String,
    /// `macos` | `windows` | `linux`; sent to the core and the device-login request.
    pub platform: String,
    /// App version shown on the web authorization page.
    pub app_version: String,
}

/// Implemented by the native app. Called from background threads.
#[uniffi::export(with_foreign)]
pub trait PlatformHooks: Send + Sync {
    /// The single credential blob (JSON bytes) or `None` when signed out.
    ///
    /// `None` means "nothing is saved" and signs the user out. A store that
    /// cannot be read right now must fail instead, so the saved login is kept:
    /// [`PlatformError::Locked`] when the store (keychain, keyring) is locked,
    /// [`PlatformError::Failed`] for any other failure. Both keep the login and
    /// are retried in the background (every 30 s, then every 5 min) until the
    /// store is readable; `Locked` only changes what the app shows
    /// ([`ErrorCode::CredentialStoreLocked`] instead of
    /// [`ErrorCode::CredentialStoreFailed`]).
    ///
    /// Because of those retries, an implementation must not show an unlock
    /// prompt on every call: after the user dismisses one, keep failing with
    /// `Locked` without prompting until the store is unlocked some other way
    /// (or the app's own "retry" action decides to allow one more prompt; see
    /// [`Client::retry_credential_restore`]).
    fn credential_load(&self) -> Result<Option<Vec<u8>>, PlatformError>;
    /// Replace the saved blob. Fails with [`PlatformError::Locked`] when the
    /// store is locked, [`PlatformError::Failed`] otherwise.
    fn credential_save(&self, blob: Vec<u8>) -> Result<(), PlatformError>;
    /// Remove the saved blob (a no-op when there is none). Same errors as
    /// `credential_save`.
    fn credential_delete(&self) -> Result<(), PlatformError>;
    /// Open the device-authorization page in the default browser.
    fn open_url(&self, url: String) -> bool;
    fn privileged_service_installed(&self) -> bool;
    /// Prompt for admin rights and install the enhanced-mode service.
    fn install_privileged_service(&self) -> Result<(), PlatformError>;
    fn uninstall_privileged_service(&self) -> Result<(), PlatformError>;
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum PlatformError {
    #[error("{message}")]
    Failed { message: String },
    #[error("cancelled by the user")]
    Cancelled,
    /// Credential hooks only: the credential store (keychain, keyring) is
    /// locked, so the saved login cannot be read or written until the user
    /// unlocks it. `message` is for logs (e.g. `LINUX_KEYRING_LOCKED: ...`).
    /// Other hooks report it like `Failed`.
    #[error("{message}")]
    Locked { message: String },
}

impl From<uniffi::UnexpectedUniFFICallbackError> for PlatformError {
    fn from(error: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Failed {
            message: error.reason,
        }
    }
}

/// Implemented by the native app. Called from background threads; hop to the
/// main thread before touching UI.
#[uniffi::export(with_foreign)]
pub trait ClientListener: Send + Sync {
    fn on_snapshot(&self, snapshot: ClientSnapshot);
    /// One result per requested node while a speed test runs; every node gets
    /// exactly one, before the `probe` call returns.
    fn on_probe_result(&self, result: ProbeResult);
    /// Once per second while signed in (zero rates while nothing runs).
    fn on_traffic(&self, sample: TrafficSample);
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

#[derive(uniffi::Record, Clone, Debug, Default, PartialEq)]
pub struct ClientSnapshot {
    pub auth: AuthState,
    pub account: Option<Account>,
    pub team: Option<Team>,
    pub profile: Option<ProfileSummary>,
    /// Why there is (or is not) a usable profile for the current team. A
    /// persistent state, independent of `last_error`.
    pub profile_status: ProfileStatus,
    /// Standard-mode core: local proxies and speed tests are available while
    /// it is `Ready`.
    pub standard: StandardState,
    /// How `Client::connect` connects (Settings → Advanced; persisted).
    pub connection_mode: ConnectionMode,
    /// Rules or global routing (`Client::set_routing_mode`; persisted).
    pub routing_mode: RoutingMode,
    /// The one "Connect" switch, in whichever mode is selected.
    pub connection: ConnectionState,
    /// The privileged service (enhanced mode) is installed.
    pub service_installed: bool,
    pub selected_node_id: Option<String>,
    /// Latest user-facing failure; cleared by the next successful action.
    pub last_error: Option<ClientErrorInfo>,
    /// Unread backend messages (badge); 0 while signed out.
    pub unread_notifications: u32,
    /// Routing rule sets the core in use has no copy of yet (core state
    /// `unavailable`), in profile order. The rules that reference them are
    /// skipped, so that traffic goes through the proxy until they load; show
    /// a low-key notice while non-empty. `stale` copies still route and are
    /// not listed. Refreshed every few seconds while signed in; empty while
    /// signed out or when the profile has no rule sets.
    pub rule_sets_unavailable: Vec<String>,
    /// Nodes pinned to one ingress on this device (`Client::pin_ingress`;
    /// persisted). A node not listed uses automatic failover.
    pub ingress_pins: Vec<IngressPin>,
    /// Per node, the ingress pin and each ingress's health as the core in
    /// use reports them (ppvpn-core 0.5.7+; the enhanced core's while it is
    /// on, otherwise the standard core's), in profile order. Refreshed every
    /// few seconds; empty while unknown. A pinned ingress with `healthy ==
    /// Some(false)` is down: the core does not switch away from it.
    pub node_ingresses: Vec<NodeIngresses>,
    /// Pins a profile refresh dropped because the node no longer has that
    /// ingress (the node is back on automatic failover). Shown once; cleared
    /// by `Client::dismiss_cleared_ingress_pins`.
    pub cleared_ingress_pins: Vec<IngressPin>,
    /// The standard-mode core rebuilt the local proxy credentials when it
    /// was created (its saved credentials were damaged, or other users
    /// could read them): apps that use the local proxy with the old user
    /// name and password (e.g. a browser extension) must copy them again.
    /// Shown once; cleared by `Client::dismiss_local_proxy_credentials_reset`.
    pub local_proxy_credentials_reset: bool,
    /// macOS, enhanced mode on: another VPN's routes the TUN replaced for
    /// now, as the core reports them (e.g. `route 128.0.0.0/1 via 192.0.2.1
    /// on utun4`); they are put back when enhanced mode disconnects. Show an
    /// informational notice while connected and non-empty (nothing to
    /// dismiss). Refreshed every few seconds; empty otherwise.
    pub replaced_routes: Vec<String>,
}

/// A node fixed to one of its ingresses (`Replica::endpoint_key`).
#[derive(uniffi::Record, serde::Serialize, Clone, Debug, PartialEq, Eq)]
pub struct IngressPin {
    pub node_id: String,
    pub endpoint_key: String,
}

/// One node's ingresses as the core reports them (`GetStatus.nodes`).
#[derive(uniffi::Record, Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeIngresses {
    pub node_id: String,
    /// `None`: automatic failover.
    pub pinned_endpoint_key: Option<String>,
    pub ingresses: Vec<IngressHealth>,
}

#[derive(uniffi::Record, Clone, Debug, Default, PartialEq, Eq)]
pub struct IngressHealth {
    pub endpoint_key: String,
    /// `primary` / `backup`.
    pub role: String,
    pub label: Option<String>,
    /// From the core's checks through this ingress; `None` for single-ingress
    /// nodes, while idle, or before the first check.
    pub healthy: Option<bool>,
    /// Carries the node's latest new connection.
    pub active: bool,
}

/// Which traffic goes through the proxy (Settings; persisted on this device,
/// never synced). Both keep the profile's baseline rules (private networks,
/// the official API) direct.
#[derive(uniffi::Enum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RoutingMode {
    /// The profile's routing rules (e.g. domestic sites direct).
    #[default]
    Rules,
    /// Everything but the baseline rules goes through the proxy.
    Global,
}

/// How the client connects. Mutually exclusive; the choice is persisted.
#[derive(uniffi::Enum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionMode {
    /// Transparent routing of all traffic (TUN) through the privileged
    /// service; installs it on first use (admin prompt).
    #[default]
    Enhanced,
    /// The OS system proxy points at a local endpoint that follows the
    /// selected node; apps that honour the system proxy use it. No admin
    /// rights needed.
    Compatible,
}

/// The connection in the selected mode.
#[derive(uniffi::Record, Clone, Debug, Default, PartialEq)]
pub struct ConnectionState {
    /// Enhanced uses every phase; Compatible only Off / Connecting / On /
    /// Disconnecting / Error.
    pub phase: ConnectionPhase,
    /// Why the phase is `Error`, `Contended` or `Reconnecting`. While
    /// enhanced mode is `On`, a problem that leaves it on:
    /// `ConnectHealthCheckFailed` / `HEALTH_API_UNREACHABLE`, the PPVPN API
    /// unreachable on the direct path while the tunnel works.
    pub reason: Option<ClientErrorInfo>,
    pub retryable: bool,
    /// Enhanced only: another session of this OS user owns the connection
    /// and the service can hand it over: offer "Use on this device"
    /// (`Client::enhanced_take_over`).
    pub can_take_over: bool,
    /// Enhanced failed in a way compatible mode avoids (admin prompt
    /// cancelled, service install failed, network path contended), or failed
    /// in any way while another app's tunnel was detected: offer "Use
    /// compatibility mode" (`Client::set_connection_mode`).
    pub suggest_compatible: bool,
    /// Display name of another proxy/VPN app competing for the network path:
    /// set while enhanced mode is contended or failed on a path problem
    /// (detected at that moment), and in compatible mode when the OS proxy
    /// already belonged to another app. `None` when it cannot be named.
    pub competitor: Option<String>,
    /// Compatible mode replaced another app's OS proxy setting (restored on
    /// disconnect).
    pub proxy_was_foreign: bool,
    /// Replica and latency of the current node.
    pub detail: ConnectionDetail,
}

/// The path to the current node: through the enhanced-mode core while it is
/// on, otherwise through the standard-mode core (system proxy / local proxy).
#[derive(uniffi::Record, Clone, Debug, Default, PartialEq, Eq)]
pub struct ConnectionDetail {
    /// Replica (endpoint key) in use, when the core reports it.
    pub endpoint_key: Option<String>,
    /// Display name of that replica, when the backend gives one; fall back
    /// to `endpoint_key`.
    pub endpoint_label: Option<String>,
    /// Replica used before the latest failover, when the core reports it.
    pub previous_endpoint_key: Option<String>,
    /// Latest TCP latency to the current node (refreshed every 60 s and on
    /// node changes; `None` when it did not answer or nothing runs).
    pub latency_ms: Option<u32>,
}

#[derive(uniffi::Enum, Clone, Debug, Default, PartialEq)]
pub enum AuthState {
    /// Restoring saved credentials at launch.
    #[default]
    Restoring,
    SignedOut,
    /// Waiting for the user to confirm `user_code` in the browser.
    AwaitingBrowser {
        code: DeviceCode,
    },
    SignedIn,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct DeviceCode {
    pub user_code: String,
    pub verification_url: String,
    pub expires_in_secs: u32,
    pub browser_opened: bool,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct Account {
    pub id: String,
    pub name: String,
    pub avatar_url: Option<String>,
    /// Sign-in email, when the backend reports one.
    pub email: Option<String>,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct Team {
    pub id: String,
    pub name: String,
    pub personal: bool,
    /// `false` for a disabled or dissolved team: list it, but switching to it
    /// is refused (`ErrorCode::TeamDisabled`).
    pub active: bool,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct ProfileSummary {
    pub revision: String,
    /// Service paid-until, RFC 3339.
    pub expires_at: String,
    pub node_count: u32,
}

/// A backend message (inbox entry). OS pop-ups come from the push agent
/// ([`PushAgent`]), not from the inbox.
#[derive(uniffi::Record, Clone, Debug, PartialEq, Eq)]
pub struct InboxMessage {
    pub id: u64,
    pub title: String,
    pub content: String,
    /// Backend message type, e.g. `subscription`.
    pub kind: String,
    /// Stable event, e.g. `subscription.expire_reminder_3d`; empty on old
    /// messages.
    pub event_key: String,
    /// What the message is about, derived from `kind` and `event_key` so
    /// every app groups and decorates messages the same way.
    pub category: MessageCategory,
    pub severity: MessageSeverity,
    /// Absolute URL to open when the notification is clicked.
    pub deep_link: Option<String>,
    /// The backend wants it shown as an OS notification.
    pub push: bool,
    pub read: bool,
    /// RFC 3339.
    pub created_at: String,
}

/// What a backend message is about.
#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageCategory {
    /// `subscription.expire_reminder_*`.
    SubscriptionExpiring,
    /// `subscription.expired` / `.suspended` / `.past_due`.
    SubscriptionExpired,
    /// Invoices, wallet, `proxy.auto_renew_failed`.
    Billing,
    Order,
    /// Route health (type `proxy`, e.g. `proxy.chain_unhealthy`).
    Route,
    /// Broadcasts, campaigns, marketing.
    Announcement,
    /// Tickets, identity checks and anything unknown.
    Other,
}

#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageSeverity {
    Critical,
    Important,
    Normal,
    /// Old messages without a severity.
    Unspecified,
}

/// One page of the message list, newest first.
#[derive(uniffi::Record, Clone, Debug)]
pub struct InboxPage {
    pub items: Vec<InboxMessage>,
    /// All messages of the account.
    pub total: u32,
}

/// State of the current team's proxy profile.
#[derive(uniffi::Enum, Clone, Debug, Default, PartialEq, Eq)]
pub enum ProfileStatus {
    /// Signed out, or signed in and the first download for this team (after
    /// sign-in or a team switch) has not finished.
    #[default]
    Loading,
    /// A valid profile is loaded (`ClientSnapshot::profile`).
    Ready,
    /// The team has no active subscription (the steady state for a new
    /// user); offer `Client::purchase_url`. No profile, both cores stopped.
    NoSubscription,
    /// The team's subscription expired (`expired_at`: RFC 3339, when the
    /// backend says). No profile, both cores stopped.
    SubscriptionExpired { expired_at: Option<String> },
    /// The team is disabled; offer switching teams. No profile, both cores
    /// stopped.
    TeamDisabled,
    /// The backend sent a profile this client or its core rejects. With
    /// `previous`, an earlier profile (one the core accepted, possibly saved
    /// by an earlier run) stays in use; without, there is nothing to connect
    /// with until the backend serves a valid one (#532).
    Invalid {
        error: ClientErrorInfo,
        previous: bool,
    },
    /// The team's subscription is live, but its nodes are not published yet.
    /// No profile, both cores stopped. The client fetches the profile again
    /// on its own (after 30 s, 60 s, then every 2 minutes) and moves to
    /// `Ready` when the nodes arrive; no user action is needed.
    NodesNotReady,
}

#[derive(uniffi::Enum, Clone, Debug, Default, PartialEq)]
pub enum StandardState {
    #[default]
    Stopped,
    Starting,
    /// Local proxies and speed tests are available for this revision.
    Ready {
        revision: String,
    },
    Failed {
        error: ClientErrorInfo,
    },
}

/// Enhanced mode as its controller reports it (folded into
/// [`ConnectionState`] for the apps).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct EnhancedState {
    pub phase: ConnectionPhase,
    pub reason: Option<ClientErrorInfo>,
    pub retryable: bool,
    pub service_installed: bool,
    pub can_take_over: bool,
    /// Apps the pre-connect conflict check found owning the network (with
    /// `NetworkPathContended` / `PREFLIGHT_CONFLICT`); empty otherwise.
    pub competitors: Vec<String>,
}

/// Phase of the connection.
#[derive(uniffi::Enum, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConnectionPhase {
    #[default]
    Off,
    Preparing,
    WaitingPermission,
    Connecting,
    On,
    Reconnecting,
    Contended,
    Disconnecting,
    Error,
}

// ---------------------------------------------------------------------------
// Nodes, speed tests, local proxies, traffic
// ---------------------------------------------------------------------------

/// One entry of one instance (instance × entry). Different entries of the
/// same instance are different nodes; the client never fails over between
/// nodes, only between a node's replicas.
#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct Node {
    pub id: String,
    pub name: String,
    /// Entry key, e.g. `cn-optimized`; for grouping only, never shown.
    pub entry_key: String,
    /// User-facing entry tier name, when the backend provides one.
    pub entry_label: Option<String>,
    pub exit_region: Option<String>,
    pub exit_country_code: Option<String>,
    pub udp: bool,
    /// Replicas in failover order (`replica_ordinal` ascending).
    pub replicas: Vec<Replica>,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct Replica {
    /// Stable endpoint id, unique within the profile.
    pub endpoint_key: String,
    /// 0 = preferred; higher values are tried in order on failure.
    pub replica_ordinal: u32,
    pub protocol: String,
    /// Display name, when the backend gives one; show `endpoint_key`
    /// otherwise.
    pub label: Option<String>,
}

#[derive(uniffi::Enum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeMethod {
    Icmp,
    Tcp,
    /// A real HTTP request through the node's local proxy.
    Connect,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct ProbeResult {
    pub node_id: String,
    pub method: ProbeMethod,
    pub success: bool,
    pub latency_ms: Option<u32>,
    /// Replica that answered (ICMP/TCP only).
    pub endpoint_key: Option<String>,
    pub error: Option<ClientErrorInfo>,
}

#[derive(uniffi::Record, Clone, Debug, PartialEq)]
pub struct LocalProxy {
    pub node_id: String,
    pub host: String,
    /// Same port serves HTTP and SOCKS5.
    pub port: u16,
    pub username: String,
    pub password: String,
}

/// Throughput of everything the client routes: system proxy and local
/// proxies (standard-mode core) plus enhanced mode when it is on. Despite the
/// `_bps` suffix (kept for source compatibility), rates are **bytes** per
/// second, not bits.
#[derive(uniffi::Record, Clone, Copy, Debug, PartialEq)]
pub struct TrafficSample {
    /// Upload rate in bytes per second over the last sample interval.
    pub up_bps: u64,
    /// Download rate in bytes per second over the last sample interval.
    pub down_bps: u64,
    /// Bytes uploaded since the running cores started (their sum; restarts
    /// reset it). Not shown by the current design.
    pub up_total: u64,
    /// Bytes downloaded since the running cores started (their sum).
    pub down_total: u64,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

#[derive(uniffi::Object)]
pub struct Client {
    config: ClientConfig,
    platform: Arc<dyn PlatformHooks>,
    listener: Arc<dyn ClientListener>,
    runtime: tokio::runtime::Runtime,
    snapshot: Mutex<ClientSnapshot>,
    /// Last snapshot handed to the listener; identical ones are not re-sent.
    last_sent: Mutex<Option<ClientSnapshot>>,
    /// Handed to background tasks (see `session::ClientRef`).
    this: Weak<Client>,
    /// Device login and the saved credential.
    auth: auth::Auth,
    /// Signed-in session: access token, raw profile, nodes.
    session: Mutex<session::SessionState>,
    /// Standard-mode core (local proxies, speed tests).
    standard: standard::StandardCore,
    /// Enhanced mode through the privileged service.
    enhanced: enhanced::Enhanced,
    /// Phones: tunnel mode in place of enhanced mode (#448), see
    /// [`cores::PHONE`].
    tunnel: Option<tunnel_mode::TunnelMode>,
    /// Compatible mode: OS system proxy on the standard core's endpoint.
    compat: compat::Compat,
    /// Rules / global, shared with both cores (`snapshot.routing_mode`).
    routing: routing::RoutingModeCell,
    /// Ingress pins, shared with both cores (`snapshot.ingress_pins`).
    ingress_pins: ingress::PinsCell,
    /// Both modes' states, folded into `snapshot.connection`.
    connection: Mutex<connection::ConnectionParts>,
    /// Serialises `set_connection_mode`.
    mode_switch: tokio::sync::Mutex<()>,
    /// Serialises core reconfiguration (see `cores`).
    cores: tokio::sync::Mutex<cores::CoreSync>,
    /// Set by `shutdown`; no core is started afterwards.
    shut_down: AtomicBool,
    /// Mobile hosts pause the unread poll while their UI is in the background.
    notification_foreground: AtomicBool,
    android_push_fallback: AtomicBool,
    android_push_lock: tokio::sync::Mutex<()>,
    /// Background re-read of a credential store that failed at launch.
    credential_retry: session::CredentialRetry,
}

#[uniffi::export(async_runtime = "tokio")]
impl Client {
    /// Starts restoring saved credentials immediately; watch `on_snapshot`.
    /// The initial state (`AuthState::Restoring`) is not pushed: read
    /// `snapshot()` right after construction. `on_snapshot` only reports
    /// changes; an unchanged snapshot is never sent twice.
    #[uniffi::constructor]
    pub fn new(
        config: ClientConfig,
        platform: Arc<dyn PlatformHooks>,
        listener: Arc<dyn ClientListener>,
    ) -> Arc<Self> {
        logging::install(&config.log_dir, &config.app_version);
        tracing::info!(app_version = %config.app_version, api = %config.api_base, "client created");
        let launcher: Arc<dyn standard::CoreLauncher> =
            Arc::new(engine::EngineLauncher::new(&config));
        let rule_set_hosts = core_ipc::rule_set_hosts(&config.api_base);
        Self::with_parts(
            config,
            platform,
            listener,
            Arc::new(service::ServiceClient::default().with_rule_set_hosts(rule_set_hosts)),
            launcher,
            sysproxy::platform_writer(),
        )
    }

    pub fn snapshot(&self) -> ClientSnapshot {
        self.snapshot.lock().unwrap().clone()
    }

    pub fn log_dir(&self) -> String {
        self.config.log_dir.clone()
    }

    // --- notifications ---------------------------------------------------

    /// One page (1-based) of backend messages, newest first; `page_size` is
    /// clamped to 1–100. Also refreshes `snapshot.unread_notifications`.
    pub async fn notifications(&self, page: u32, page_size: u32) -> Result<InboxPage, ClientError> {
        self.list_notifications(page, page_size).await
    }

    /// Mark one message read (idempotent); refreshes the unread count.
    pub async fn mark_notification_read(&self, id: u64) -> Result<(), ClientError> {
        self.read_notification(id).await
    }

    /// Mark one message unread again; refreshes the unread count.
    /// (`PUT /api/v1/messages/{id}/unread`; backend contract pending.)
    pub async fn mark_notification_unread(&self, id: u64) -> Result<(), ClientError> {
        self.unread_notification(id).await
    }

    /// Mark every message up to the newest one this client has seen as read;
    /// refreshes the unread count.
    pub async fn mark_all_notifications_read(&self) -> Result<(), ClientError> {
        self.read_all_notifications().await
    }

    /// The push the push agent showed as OS notification `push_id` (the
    /// only thing a notification carries), or `None` when it is unknown,
    /// older than 7 days or no longer among the 50 newest. Reads
    /// `<data_dir>/push-agent-shown.json` (see [`PushAgent`]); no network.
    pub fn shown_push(&self, push_id: u64) -> Option<PushMessage> {
        push_shown::find(std::path::Path::new(&self.config.data_dir), push_id)
    }

    /// Where to buy or renew a subscription: the site root of
    /// `ClientConfig::api_base` plus the products page path.
    pub fn purchase_url(&self) -> String {
        purchase_url_for(&self.config.api_base)
    }

    /// The dashboard page where the team's routing rules are edited (they
    /// apply to all its devices), on the API base's site root.
    pub fn routing_rules_url(&self) -> String {
        site_url_for(&self.config.api_base, ROUTING_RULES_PATH)
    }

    // --- auth ------------------------------------------------------------

    /// Start browser device login and open the verification page; polling
    /// runs internally and ends in `AuthState::SignedIn`, or in the previous
    /// state with `last_error` `AuthExpired` / `AuthDenied`. Starting again
    /// supersedes a running login.
    pub async fn auth_start(&self) -> Result<DeviceCode, ClientError> {
        self.start_login().await
    }

    /// Abandon a running device login. A late approval is revoked.
    pub fn auth_cancel(&self) {
        self.cancel_login();
    }

    /// Read the credential store again now instead of waiting for the next
    /// background retry, after a launch restore failed with
    /// `CredentialStoreLocked` / `CredentialStoreFailed` (the "Retry" button;
    /// the platform may show its unlock prompt once for this read). Returns
    /// `false` when no such retry is pending (e.g. the store failed while
    /// saving a new sign-in): the app then offers signing in again instead.
    pub fn retry_credential_restore(&self) -> bool {
        self.wake_credential_retry()
    }

    /// Stop both cores, revoke the session (best effort) and clear the saved
    /// credential and node selection. Fails only when the credential could
    /// not be deleted; the client is signed out either way.
    pub async fn logout(&self) -> Result<(), ClientError> {
        self.sign_out().await
    }

    // --- account ---------------------------------------------------------

    /// Teams the account can switch between; also refreshes `snapshot.team`.
    pub async fn teams(&self) -> Result<Vec<Team>, ClientError> {
        self.list_teams().await
    }

    /// Switch team; refetches the profile and restarts standard mode.
    pub async fn switch_team(&self, team_id: String) -> Result<(), ClientError> {
        self.change_team(team_id).await
    }

    // --- profile & nodes -------------------------------------------------

    /// Refetch the profile now (also runs every 5 minutes while signed in).
    /// A team without an active subscription fails with `NoSubscription`.
    pub async fn refresh_profile(&self) -> Result<(), ClientError> {
        self.refresh_profile_now().await
    }

    /// Nodes of the current profile, in profile order; empty until loaded.
    pub fn nodes(&self) -> Vec<Node> {
        self.profile_nodes()
    }

    /// Choose the exit node (remembered across launches); applies live while
    /// enhanced mode is on. Fails with `NodeNotFound` for an unknown id.
    pub async fn select_node(&self, node_id: String) -> Result<(), ClientError> {
        self.choose_node(node_id).await
    }

    /// Pin `node_id` to one of its ingresses (`endpoint_key`, from
    /// `Node::replicas`), or back to automatic failover with `None`
    /// (remembered across launches; applied live to the running cores). A
    /// pinned ingress that fails stays pinned: see `snapshot.node_ingresses`.
    /// Fails with `NodeNotFound` for an unknown node or endpoint key.
    pub async fn pin_ingress(
        &self,
        node_id: String,
        endpoint_key: Option<String>,
    ) -> Result<(), ClientError> {
        self.set_ingress_pin(node_id, endpoint_key).await
    }

    /// The user saw `snapshot.cleared_ingress_pins`: empty it.
    pub fn dismiss_cleared_ingress_pins(&self) {
        self.clear_cleared_ingress_pins();
    }

    /// The user saw `snapshot.local_proxy_credentials_reset`: clear it.
    pub fn dismiss_local_proxy_credentials_reset(&self) {
        self.clear_local_proxy_credentials_reset();
    }

    /// Run a speed test through the standard-mode core; results stream
    /// through `on_probe_result`. An empty list tests every node. Returns only
    /// after every requested node has reported exactly one result (nodes past
    /// the deadline report `Timeout`; ICMP refused by the OS reports
    /// `IcmpNotPermitted`). Fails with `StandardNotReady` until
    /// `snapshot.standard` is `Ready`.
    pub async fn probe(
        &self,
        method: ProbeMethod,
        node_ids: Vec<String>,
    ) -> Result<(), ClientError> {
        let listener = self.listener.clone();
        let weak = self.this.clone();
        let deliver = move |result: ProbeResult| {
            // A speed test of the current node also refreshes its latency.
            if let Some(client) = session::ClientRef::upgrade(&weak) {
                client.note_probe(&result);
            }
            listener.on_probe_result(result);
        };
        let result = self.standard.probe(method, node_ids, &deliver).await;
        self.finish_action(result)
    }

    /// Per-node loopback proxies (with credentials) served by the
    /// standard-mode core. Fails with `StandardNotReady` until it is ready.
    pub async fn local_proxies(&self) -> Result<Vec<LocalProxy>, ClientError> {
        let result = self.standard.local_proxies().await;
        self.finish_action(result)
    }

    /// The routed user of the same loopback proxy: traffic follows the
    /// profile's rules (and the routing mode), the rest goes through the
    /// selected node, like the system proxy. `node_id` is empty. `None`
    /// when the standard core predates it (0.5.12). Fails with
    /// `StandardNotReady` until the core is ready. Examples should use
    /// `http://` or `socks5h://` (SOCKS5 by IP misses domain rules).
    pub async fn routed_local_proxy(&self) -> Result<Option<LocalProxy>, ClientError> {
        let result = self.standard.routed_local_proxy().await;
        self.finish_action(result)
    }

    // --- connection --------------------------------------------------------

    /// Turn the connection on in `snapshot.connection_mode`, turning the
    /// other mode off first.
    ///
    /// - Enhanced: installs the privileged service when missing
    ///   (`WaitingPermission`, admin prompt), waits until it answers, then
    ///   connects. A denied prompt ends in `Error{ServiceInstallCancelled}`
    ///   with `suggest_compatible`. Before the service starts the TUN core,
    ///   conflict detection runs (about 1.5 s at most): when another app's
    ///   tunnel owns the route or DNS, TUN is not started and the connection
    ///   ends in `Error{NetworkPathContended, "PREFLIGHT_CONFLICT: …"}`,
    ///   retryable, with `competitor` and `suggest_compatible`.
    /// - Compatible: opens the standard core's local system-proxy endpoint and
    ///   points the OS proxy settings at it (previous settings are saved and
    ///   restored on disconnect, sign-out, shutdown or the next launch).
    pub async fn connect(&self) -> Result<(), ClientError> {
        let result = self.connect_now().await;
        self.finish_action(result)
    }

    /// Turn the connection off (both modes); always ends in `Off`.
    pub async fn disconnect(&self) -> Result<(), ClientError> {
        let result = self.disconnect_now().await;
        self.finish_action(result)
    }

    /// Reconnect from `Error` / `Contended` in the selected mode.
    pub async fn retry(&self) -> Result<(), ClientError> {
        let result = self.retry_now().await;
        self.finish_action(result)
    }

    /// Looks for other proxy/VPN apps competing for the network path
    /// (offline: processes, tunnel interfaces, routing table). A running app
    /// alone is not a conflict: `competitors` is filled only when a fake-IP
    /// tunnel or another app's route is present. Never blocks anything.
    pub async fn detect_conflicts(&self) -> ConflictReport {
        tokio::task::spawn_blocking(detect::detect_now)
            .await
            .unwrap_or_default()
    }

    /// Choose how `connect` connects (persisted). When the connection is not
    /// off it is disconnected and connected again in the new mode.
    pub async fn set_connection_mode(&self, mode: ConnectionMode) -> Result<(), ClientError> {
        let result = self.change_mode(mode).await;
        self.finish_action(result)
    }

    /// Choose rules or global routing (persisted on this device). The
    /// running cores apply it at once, without a reconnect; connections open
    /// at that moment are re-established.
    pub async fn set_routing_mode(&self, mode: RoutingMode) -> Result<(), ClientError> {
        let result = self.change_routing_mode(mode).await;
        self.finish_action(result)
    }

    /// Take over the enhanced-mode connection from another session of this OS
    /// user ("Use on this device"). Only valid while
    /// `ConnectionState::can_take_over`; a different user's connection fails
    /// with `ServiceOwnedByAnotherUser`.
    pub async fn enhanced_take_over(&self) -> Result<(), ClientError> {
        self.current_session()?;
        let result = match self.prime_enhanced().await {
            Ok(()) => self.enhanced.take_over().await,
            Err(error) => Err(error),
        };
        self.finish_action(result)
    }

    /// Install the privileged service (admin prompt) without turning
    /// enhanced mode on, and wait until it answers. No-op when it already
    /// answers. A cancelled prompt returns `ClientError::Cancelled` and
    /// changes no state.
    pub async fn service_install(&self) -> Result<(), ClientError> {
        let result = self.enhanced.install_service().await;
        self.finish_action(result)
    }

    /// Turn enhanced mode off and uninstall the privileged service through
    /// `PlatformHooks::uninstall_privileged_service` (for a Settings button).
    /// Cancelling the admin prompt fails with `ServiceUninstallCancelled`.
    pub async fn service_uninstall(&self) -> Result<(), ClientError> {
        let result = self.enhanced.uninstall_service().await;
        self.finish_action(result)
    }

    /// Check on the system whether the privileged service is installed and
    /// update `ClientSnapshot::service_installed` if that changed (e.g. an
    /// admin installed it outside the app). Apps call it before asking the
    /// user to install the service; cheap (a file check, no prompt).
    pub async fn refresh_service_installed(&self) -> bool {
        self.enhanced.refresh_service_installed().await
    }

    /// Tell the client the OS reported a network change (an interface came
    /// or went, an address or the default route changed; e.g. NWPathMonitor,
    /// NetworkChange.NetworkAddressChanged, GNetworkMonitor::network-changed).
    /// Enhanced mode then checks its data path within ~2 s instead of at the
    /// next 30 s check, and a reconnect waiting out a pause tries again now.
    /// Cheap and safe to call on every event, in any state.
    pub fn network_changed(&self) {
        self.enhanced.network_changed();
    }

    /// Tell the client the system is about to sleep
    /// (NSWorkspace.willSleepNotification, PBT_APMSUSPEND, logind
    /// PrepareForSleep(true)). The cores' timed checks and updates wait until
    /// [`Client::system_did_wake`]; a wake that never comes is assumed a
    /// minute after the system is awake again. Best effort, returns within a
    /// few seconds, safe in any state.
    pub async fn system_will_sleep(&self) {
        self.tell_cores_sleep(true).await;
    }

    /// Tell the client the system woke (NSWorkspace.didWakeNotification,
    /// PBT_APMRESUMEAUTOMATIC, logind PrepareForSleep(false)). After a sleep
    /// of 2 minutes or more the cores close their idle connections and test
    /// the lines again; connections in use stay. Best effort, safe in any
    /// state, also without a sleep before it.
    pub async fn system_did_wake(&self) {
        self.tell_cores_sleep(false).await;
    }

    /// Release the enhanced-mode session and stop the standard-mode core;
    /// call before the app quits. Blocks for at most ~10 seconds. Enhanced
    /// mode is not resumed on the next launch.
    pub fn shutdown(&self) {
        self.shutdown_cores();
    }
}

/// Path of the purchase page on the site root.
const PURCHASE_PATH: &str = "/dashboard/products";

/// Path of the team routing-rules page on the site root.
const ROUTING_RULES_PATH: &str = "/dashboard/routing-rules";

fn purchase_url_for(api_base: &str) -> String {
    site_url_for(api_base, PURCHASE_PATH)
}

/// `path` on the site root of `api_base` (its path, query and fragment
/// dropped).
fn site_url_for(api_base: &str, path: &str) -> String {
    match reqwest::Url::parse(api_base.trim()) {
        Ok(mut url) => {
            url.set_path(path);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        Err(_) => format!("{}{path}", api_base.trim().trim_end_matches('/')),
    }
}

impl Client {
    /// `path` on the site root of `ClientConfig::api_base`, for the pages
    /// the apps open (#517): the configured backend's own site, not a
    /// hardcoded host.
    pub fn site_url(&self, path: &str) -> String {
        site_url_for(&self.config.api_base, path)
    }

    /// [`Client::new`] with the privileged-service client and the core
    /// launcher injected (tests substitute fakes).
    pub(crate) fn with_parts(
        config: ClientConfig,
        platform: Arc<dyn PlatformHooks>,
        listener: Arc<dyn ClientListener>,
        service: Arc<dyn service::ServiceApi>,
        launcher: Arc<dyn standard::CoreLauncher>,
        writer: Arc<dyn sysproxy::SystemProxyWriter>,
    ) -> Arc<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .thread_name("ppvpn-client")
            .build()
            .expect("tokio runtime");
        testmode::log_startup(&config.api_base);
        storage::migrate(&config.data_dir);
        let auth = auth::client_auth(
            Arc::new(api::client_api(&config.api_base)),
            platform.clone(),
        );
        let snapshot = ClientSnapshot {
            selected_node_id: session::load_selection(&config.data_dir),
            // Phones have one mode, the system VPN, folded as enhanced.
            connection_mode: if cores::PHONE {
                ConnectionMode::Enhanced
            } else {
                connection::load_mode(&config.data_dir)
            },
            routing_mode: connection::load_routing_mode(&config.data_dir),
            ingress_pins: ingress::records(&ingress::load(&config.data_dir)),
            ..ClientSnapshot::default()
        };
        let routing = routing::RoutingModeCell::new(snapshot.routing_mode);
        let ingress_pins = ingress::PinsCell::new(ingress::load(&config.data_dir));
        // A delayed Android token worker may recreate the UI process while
        // :tunnel is still delivering fallback pushes. Restore that choice
        // before any session task can register or clean up device files.
        let android_push_fallback = config.platform == "android"
            && std::path::Path::new(&config.data_dir)
                .join(android_push::LEASE_FILE)
                .is_file();
        let client = Arc::new_cyclic(|this| {
            let (standard, enhanced, compat) = cores::build_cores(
                &config,
                platform.clone(),
                service,
                launcher,
                writer,
                routing.clone(),
                ingress_pins.clone(),
                this,
            );
            let tunnel = cores::build_tunnel(&config, routing.clone(), ingress_pins.clone(), this);
            Self {
                config,
                platform,
                listener,
                runtime,
                snapshot: Mutex::new(snapshot),
                last_sent: Mutex::new(None),
                this: this.clone(),
                auth,
                session: Mutex::new(session::SessionState::default()),
                standard,
                enhanced,
                tunnel,
                compat,
                routing,
                ingress_pins,
                connection: Mutex::new(connection::ConnectionParts::default()),
                mode_switch: tokio::sync::Mutex::new(()),
                cores: tokio::sync::Mutex::new(cores::CoreSync::default()),
                shut_down: AtomicBool::new(false),
                notification_foreground: AtomicBool::new(true),
                android_push_fallback: AtomicBool::new(android_push_fallback),
                android_push_lock: tokio::sync::Mutex::new(()),
                credential_retry: session::CredentialRetry::default(),
            }
        });
        let weak = Arc::downgrade(&client);
        client.runtime.spawn(async move {
            if let Some(client) = session::ClientRef::upgrade(&weak) {
                // OS proxy settings a crashed run left behind come back first.
                client.compat.restore_leftovers().await;
                client.restore_session().await;
                client.restore_enhanced().await;
            }
        });
        client
    }

    fn update(&self, change: impl FnOnce(&mut ClientSnapshot)) {
        let snapshot = {
            let mut guard = self.snapshot.lock().unwrap();
            change(&mut guard);
            guard.clone()
        };
        self.emit(snapshot);
    }

    /// Hands `snapshot` to the listener unless it equals the last one sent.
    /// The listener runs after the lock is released, so it may call back in.
    fn emit(&self, snapshot: ClientSnapshot) {
        {
            let mut last = self
                .last_sent
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if last.as_ref() == Some(&snapshot) {
                return;
            }
            *last = Some(snapshot.clone());
        }
        self.listener.on_snapshot(snapshot);
    }
}

#[cfg(test)]
mod tests {
    use super::{purchase_url_for, site_url_for, ROUTING_RULES_PATH};

    #[test]
    fn routing_rules_url_uses_the_site_root() {
        let url = |base| site_url_for(base, ROUTING_RULES_PATH);
        assert_eq!(
            url("https://www.example.com"),
            "https://www.example.com/dashboard/routing-rules"
        );
        assert_eq!(
            url("https://api.example.com/api/prefix/?x=1#f"),
            "https://api.example.com/dashboard/routing-rules"
        );
        assert_eq!(
            url("http://127.0.0.1:8080/"),
            "http://127.0.0.1:8080/dashboard/routing-rules"
        );
        assert_eq!(
            url("https://www.example.net"),
            "https://www.example.net/dashboard/routing-rules"
        );
        assert_eq!(
            site_url_for("https://www.example.net/api/", ""),
            "https://www.example.net/"
        );
    }

    #[test]
    fn purchase_url_uses_the_site_root() {
        assert_eq!(
            purchase_url_for("https://www.example.com"),
            "https://www.example.com/dashboard/products"
        );
        assert_eq!(
            purchase_url_for("https://api.example.com/api/prefix/?x=1#f"),
            "https://api.example.com/dashboard/products"
        );
        assert_eq!(
            purchase_url_for("http://127.0.0.1:8080/"),
            "http://127.0.0.1:8080/dashboard/products"
        );
    }
}
