//! What the Dart side calls; flutter_rust_bridge generates its Dart from
//! this module only. The state stays in the client (see `bridge`); the types
//! here are flat copies of what the two pages show, without enums that carry
//! data (those would need freezed on the Dart side).

use flutter_rust_bridge::frb;

use crate::bridge;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiAuth {
    Restoring,
    SignedOut,
    AwaitingBrowser,
    SignedIn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiProfile {
    Loading,
    Ready,
    NoSubscription,
    SubscriptionExpired,
    TeamDisabled,
    Invalid,
    NodesNotReady,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiPhase {
    Off,
    Preparing,
    WaitingPermission,
    Connecting,
    /// `ConnectionPhase::On` (Dart cannot name a constant `on`).
    Connected,
    Reconnecting,
    Contended,
    Disconnecting,
    Error,
}

/// Enhanced, `Contended`: what holds the connection or the network; picks
/// the copy (#438).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiContended {
    /// Another session of this OS user on this computer: "Use on this
    /// device" takes it over.
    AnotherSession,
    /// Another OS user on this computer.
    AnotherUser,
    /// Another VPN or proxy app has the network path.
    NetworkTaken,
    /// Another program kept changing the tunnel's routes.
    RoutesTaken,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiError {
    /// `ErrorCode` as its Rust name, e.g. `NetworkUnreachable`.
    pub code: String,
    /// For logs, not for display.
    pub detail: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiDeviceCode {
    pub user_code: String,
    pub verification_url: String,
    /// From when the code arrived.
    pub expires_in_secs: u32,
    pub browser_opened: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiSnapshot {
    pub auth: UiAuth,
    /// While `auth` is `AwaitingBrowser`.
    pub device_code: Option<UiDeviceCode>,
    pub account_name: Option<String>,
    pub team_name: Option<String>,
    /// The current team's id, one of [`teams`]' [`UiTeam::id`]s.
    pub team_id: Option<String>,
    pub profile: UiProfile,
    /// With `profile` [`UiProfile::Invalid`]: an earlier profile stays in use
    /// (#532). Without, nothing can connect until a valid one arrives.
    pub profile_previous: bool,
    /// Changes when the node list does: read [`nodes`] again.
    pub profile_revision: Option<String>,
    /// Service paid-until, RFC 3339.
    pub expires_at: Option<String>,
    pub phase: UiPhase,
    /// Line (replica) in use: its endpoint key, one of the current node's
    /// [`UiLine::key`]s.
    pub line_key: Option<String>,
    pub latency_ms: Option<u32>,
    /// The standard-mode core runs: local proxies and speed tests work.
    pub standard_ready: bool,
    pub selected_node_id: Option<String>,
    /// Why the connection failed, else the latest failure of any action.
    pub error: Option<UiError>,
    /// Unread backend messages (the sidebar badge); 0 while signed out.
    pub unread: u32,
    /// Rules or global (settings; [`set_routing_mode`]).
    pub routing_mode: UiRoutingMode,
    /// The privileged service (enhanced mode) is installed.
    pub service_installed: bool,
    /// The mode `phase` belongs to: one connection, either mode.
    pub connection_mode: UiConnectionMode,
    /// Enhanced, `Contended`: another session of this OS user has the
    /// connection and hands it over on [`enhanced_take_over`].
    pub can_take_over: bool,
    /// Enhanced failed in a way the system proxy avoids: offer it.
    pub suggest_compatible: bool,
    /// Enhanced, `Contended`: what holds it; `None` in any other phase.
    pub contended: Option<UiContended>,
    /// Enhanced, on: the tunnel works but the PPVPN API cannot be reached
    /// on the direct path; checks go on (#438).
    pub api_unreachable: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiNode {
    pub id: String,
    pub name: String,
    pub exit_region: Option<String>,
    pub exit_country_code: Option<String>,
    pub entry_label: Option<String>,
    /// Entry key, e.g. `cn-optimized`: picks the tier tag's colour.
    pub entry_key: String,
    pub udp: bool,
    /// Lines (replicas) in failover order.
    pub lines: Vec<UiLine>,
}

/// One of a node's lines. Keys are internal and never shown: a line
/// without a label is named by its place (线路 2), as in the native apps.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiLine {
    pub key: String,
    /// The backend's label; None when it gives none (or an empty one).
    pub label: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiProbeMethod {
    Icmp,
    Tcp,
    Connect,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiProbe {
    pub node_id: String,
    pub success: bool,
    pub latency_ms: Option<u32>,
    /// `Timeout`, when it failed by timing out.
    pub timed_out: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiLocalProxy {
    pub node_id: String,
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct UiTraffic {
    /// Bytes per second.
    pub up: i64,
    pub down: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiEvents {
    pub snapshot: Option<UiSnapshot>,
    pub traffic: Option<UiTraffic>,
    /// Speed-test results since the previous call, the latest per node.
    pub probes: Vec<UiProbe>,
}

/// Android and iOS: the app's own directory, before [`start`]; the data
/// and logs go under it (dirs.rs).
#[frb(sync)]
pub fn set_app_dir(path: String) {
    let _ = crate::dirs::APP_DIR.set(path);
}

/// Creates the client on the first call and returns the current state.
#[frb(sync)]
pub fn start() -> Result<UiSnapshot, String> {
    Ok(bridge::ui_snapshot(&bridge::start()?.client.snapshot()))
}

/// Waits for the next change and returns the latest snapshot and traffic
/// sample since the previous call (either may be absent).
pub async fn next_events() -> Result<UiEvents, String> {
    let events = bridge::get()?.events.clone();
    loop {
        {
            let mut pending = events.pending.lock().unwrap();
            if pending.snapshot.is_some() || pending.traffic.is_some() || !pending.probes.is_empty()
            {
                return Ok(std::mem::take(&mut *pending));
            }
        }
        events.ready.notified().await;
    }
}

#[frb(sync)]
pub fn nodes() -> Result<Vec<UiNode>, String> {
    Ok(bridge::get()?
        .client
        .nodes()
        .into_iter()
        .map(bridge::ui_node)
        .collect())
}

/// Downloads the node list again ("refresh nodes").
pub async fn refresh_nodes() -> Result<(), String> {
    bridge::get()?
        .call(|client| async move { client.refresh_profile().await })
        .await
}

/// Tests every node; results arrive in [`next_events`]. Fails until the
/// standard-mode core is ready.
pub async fn probe_all(method: UiProbeMethod) -> Result<(), String> {
    let method = bridge::probe_method(method);
    bridge::get()?
        .call(move |client| async move { client.probe(method, Vec::new()).await })
        .await
}

/// A team the account can switch to ([`teams`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiTeam {
    pub id: String,
    pub name: String,
    /// The account's personal space (shown as 个人).
    pub personal: bool,
    /// False for a disabled or dissolved team: listed, but switching to it
    /// is refused.
    pub active: bool,
}

/// Teams the account can switch between (the account menu, read each time
/// it opens); also refreshes the snapshot's team.
pub async fn teams() -> Result<Vec<UiTeam>, String> {
    let teams = bridge::get()?
        .fetch(|client| async move { client.teams().await })
        .await?;
    Ok(teams
        .into_iter()
        .map(|team| UiTeam {
            id: team.id,
            name: team.name,
            personal: team.personal,
            active: team.active,
        })
        .collect())
}

/// Switches team: the client fetches the team's profile and restarts
/// standard mode.
pub async fn switch_team(team_id: String) -> Result<(), String> {
    bridge::get()?
        .call(|client| async move { client.switch_team(team_id).await })
        .await
}

/// Each node's loopback proxy with its credentials (empty until the
/// standard-mode core is ready).
pub async fn local_proxies() -> Result<Vec<UiLocalProxy>, String> {
    bridge::get()?.local_proxies().await
}

/// Starts the device login; the code arrives in the snapshot
/// (`UiAuth::AwaitingBrowser`) and the browser opens on its own.
pub async fn sign_in() -> Result<(), String> {
    bridge::get()?
        .call(|client| async move { client.auth_start().await.map(|_| ()) })
        .await
}

/// `path` on the site root of the backend the app signs in to (#517): the
/// web pages the app links to follow the configured API base.
#[frb(sync)]
pub fn site_url(path: String) -> Result<String, String> {
    Ok(bridge::get()?.client.site_url(&path))
}

/// Network diagnostics (#521): the system resolver and the DoH resolvers
/// for the API host and the profile's node hosts, then TCP and TLS to each
/// address, as text for the page and for pasting into a ticket. Only on the
/// user's press: it asks third-party resolvers. Up to about 15 seconds.
pub async fn diagnose_reachability() -> Result<String, String> {
    bridge::get()?
        .fetch(|client| async move {
            Ok::<_, ppvpn_client::ClientError>(client.diagnose_reachability().await.text())
        })
        .await
}

/// Opens `url` in the default browser ("reopen the browser").
#[frb(sync)]
pub fn open_url(url: String) -> bool {
    crate::hooks::open_url(&url)
}

/// Hands over the APNs device token (iOS/macOS) or FCM registration
/// token (Android) and the app's bundle id / package name. Call it whenever
/// the OS gives one (every launch and on rotation); the core registers the
/// device while signed in and unregisters it at sign-out (#539).
#[frb(sync)]
pub fn set_push_token(token: String, bundle_id: Option<String>) -> Result<(), String> {
    bridge::get()?.client.set_push_token(token, bundle_id);
    Ok(())
}

/// Official-push hosts: poll the unread badge only while the app is in the foreground.
#[frb(sync)]
pub fn set_notification_foreground(foreground: bool) -> Result<(), String> {
    bridge::get()?
        .client
        .set_notification_foreground(foreground);
    Ok(())
}

#[frb(sync)]
pub fn cancel_sign_in() -> Result<(), String> {
    bridge::get()?.client.auth_cancel();
    Ok(())
}

pub async fn sign_out() -> Result<(), String> {
    bridge::get()?
        .call(|client| async move { client.logout().await })
        .await
}

pub async fn select_node(node_id: String) -> Result<(), String> {
    bridge::get()?
        .call(|client| async move { client.select_node(node_id).await })
        .await
}

pub async fn connect() -> Result<(), String> {
    bridge::get()?
        .call(|client| async move { client.connect().await })
        .await
}

pub async fn disconnect() -> Result<(), String> {
    bridge::get()?
        .call(|client| async move { client.disconnect().await })
        .await
}

pub async fn retry() -> Result<(), String> {
    bridge::get()?
        .call(|client| async move { client.retry().await })
        .await
}

// --- enhanced ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiConnectionMode {
    /// TUN through the privileged service.
    Enhanced,
    /// The system proxy on the standard-mode core.
    Compatible,
}

/// Chooses how [`connect`] connects (persisted). A connection that is not
/// off is disconnected and connected again in the new mode.
pub async fn set_connection_mode(mode: UiConnectionMode) -> Result<(), String> {
    let mode = bridge::connection_mode(mode);
    bridge::get()?
        .call(move |client| async move { client.set_connection_mode(mode).await })
        .await
}

/// "Use on this device": takes the enhanced-mode connection over from
/// another session (`UiSnapshot::can_take_over`).
pub async fn enhanced_take_over() -> Result<(), String> {
    bridge::get()?
        .call(|client| async move { client.enhanced_take_over().await })
        .await
}

/// Installs the privileged service (admin prompt) without connecting.
pub async fn service_install() -> Result<(), String> {
    bridge::get()?
        .call(|client| async move { client.service_install().await })
        .await
}

/// Before the app quits: puts the system proxy back and stops the core
/// (blocks for at most ~10 s).
#[frb(sync)]
pub fn shutdown() {
    if let Some(bridge) = bridge::running() {
        bridge.client.shutdown();
    }
}

// --- logs ---

/// The client's log directory: crates/desktop writes
/// `ppvpn-client.YYYY-MM-DD.log` and `ppvpn-core.YYYY-MM-DD.log` there, one
/// per UTC day. The logs page reads them itself.
#[frb(sync)]
pub fn log_dir() -> Result<String, String> {
    Ok(bridge::get()?.client.log_dir())
}

// --- messages ---

/// `MessageCategory`: what a backend message is about (its icon and type
/// label).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiMessageCategory {
    SubscriptionExpiring,
    SubscriptionExpired,
    Billing,
    Order,
    Route,
    Announcement,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiMessageSeverity {
    Critical,
    Important,
    Normal,
    /// Old messages without a severity.
    Unspecified,
}

/// A backend message (inbox entry).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiMessage {
    /// The backend's id (a `u64`; Dart ints are signed).
    pub id: i64,
    pub title: String,
    pub content: String,
    pub category: UiMessageCategory,
    pub severity: UiMessageSeverity,
    /// Absolute URL behind 「查看详情」.
    pub deep_link: Option<String>,
    pub read: bool,
    /// RFC 3339.
    pub created_at: String,
}

/// One page of messages, newest first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiMessagePage {
    pub items: Vec<UiMessage>,
    /// All messages of the account.
    pub total: u32,
}

/// Page `page` (1-based) of the messages, `page_size` to a page (1–100).
/// Also refreshes the snapshot's `unread`.
pub async fn notifications(page: u32, page_size: u32) -> Result<UiMessagePage, String> {
    let inbox = bridge::get()?
        .fetch(move |client| async move { client.notifications(page, page_size).await })
        .await?;
    Ok(crate::messages::ui_page(inbox))
}

pub async fn mark_notification_read(id: i64) -> Result<(), String> {
    let id = crate::messages::backend_id(id)?;
    bridge::get()?
        .call(move |client| async move { client.mark_notification_read(id).await })
        .await
}

pub async fn mark_notification_unread(id: i64) -> Result<(), String> {
    let id = crate::messages::backend_id(id)?;
    bridge::get()?
        .call(move |client| async move { client.mark_notification_unread(id).await })
        .await
}

pub async fn mark_all_notifications_read() -> Result<(), String> {
    bridge::get()?
        .call(|client| async move { client.mark_all_notifications_read().await })
        .await
}

// --- notification clicks, one instance ---

/// A push the push agent showed as an OS notification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiPush {
    /// The inbox message it is about; `None` for one without (a broadcast).
    pub message_id: Option<i64>,
    /// The push itself as a read-only message (its id is the push's), for
    /// when the inbox does not have it. `deep_link` only when http(s).
    pub message: UiMessage,
}

/// The push behind a clicked notification (`push_id`, the only thing the
/// notification carries), or `None` when it is unknown or too old. Reads
/// the push agent's record of what it showed; no network.
#[frb(sync)]
pub fn shown_push(push_id: i64) -> Result<Option<UiPush>, String> {
    let push_id = crate::messages::backend_id(push_id)?;
    Ok(bridge::get()?
        .client
        .shown_push(push_id)
        .map(crate::messages::ui_push))
}

/// A notification click, or another launch of the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiActivation {
    /// The clicked notification's push id ([`shown_push`]); `None`: only
    /// show the window.
    pub push_id: Option<i64>,
    /// The installer asks the app to quit (Windows): quit as the tray's
    /// Quit does, `push_id` is `None`.
    pub quit: bool,
    /// The update's installer runs (Windows, [`start_updater`]): hide the
    /// window and the tray icon and wait for `quit`, which comes from the
    /// installer or, 30 s later, from the bridge. `push_id` is `None`.
    pub update: bool,
}

/// Makes this process the app's one instance (Windows and Linux; on macOS
/// the system keeps one, and clicks come through the runner). False when
/// another one runs: it got this launch's arguments and shows its window,
/// so quit at once. Call once, before [`start`].
///
/// On Windows it first gives the process the app's AppUserModelID and
/// registers it for notifications, and a launch by the installer with
/// `--set-shortcut-aumid <.lnk>` only stamps that shortcut and exits here
/// (`aumid.rs`).
#[frb(sync)]
pub fn claim_instance() -> bool {
    crate::aumid::prepare();
    crate::instance::claim()
}

/// Waits for the next notification click or launch of the app (Windows
/// and Linux), the first launch's own click included.
pub async fn next_activation() -> UiActivation {
    crate::instance::next().await
}

// --- settings ---

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiAppearance {
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiLanguage {
    System,
    Chinese,
    English,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiRoutingMode {
    Rules,
    Global,
}

/// The app's own settings, kept in the data directory (`settings`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiSettings {
    pub appearance: UiAppearance,
    pub language: UiLanguage,
    /// The backend to sign in to; empty for the default. Read when the
    /// client starts: a change takes effect on the next launch.
    /// `PPVPN_API_BASE` wins over it. Only a debug build honours it
    /// ([`api_override_allowed`]).
    pub api_override: String,
}

/// Whether this build shows and honours the API override (#519): debug
/// builds only; a release build keeps a saved one but ignores it.
#[frb(sync)]
pub fn api_override_allowed() -> bool {
    crate::settings::API_OVERRIDE_ALLOWED
}

/// The saved settings, defaults when there are none. Works before
/// [`start`] (the API override is for it).
#[frb(sync)]
pub fn load_settings() -> Result<UiSettings, String> {
    crate::settings::load()
}

/// Saves all settings at once. Fails when `api_override` is neither empty
/// nor an http(s) URL; nothing is written then.
#[frb(sync)]
pub fn save_settings(settings: UiSettings) -> Result<(), String> {
    crate::settings::save(settings)
}

/// This build's version (a release's, else the workspace's), for the
/// settings page.
#[frb(sync)]
pub fn app_version() -> String {
    crate::update::version().into()
}

/// Rules or global routing (persisted by the client); the running core
/// applies it without a reconnect.
pub async fn set_routing_mode(mode: UiRoutingMode) -> Result<(), String> {
    let mode = bridge::routing_mode(mode);
    bridge::get()?
        .call(move |client| async move { client.set_routing_mode(mode).await })
        .await
}

// --- launch at login ---

/// Launch at login as the system has it (`autostart`): never kept in the
/// settings, off until turned on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UiLaunchAtLogin {
    pub enabled: bool,
    /// On, but held back by the system: macOS waits for approval in Login
    /// Items, Windows had it turned off in Settings › Apps › Startup. Only
    /// the system can let it run ([`open_launch_at_login_settings`]).
    pub needs_approval: bool,
}

/// Read again whenever it is shown: the system can change it. Windows and
/// Linux: on macOS lib/settings.dart asks the runner (ppvpn/login-item).
#[frb(sync)]
pub fn launch_at_login() -> Result<UiLaunchAtLogin, String> {
    crate::autostart::state()
}

/// Registers or removes the login entry (Windows and Linux start the app
/// with `--background`). Read [`launch_at_login`] again after either.
#[frb(sync)]
pub fn set_launch_at_login(enabled: bool) -> Result<(), String> {
    crate::autostart::set(enabled)
}

/// Where the system holds the entry back: Login Items (macOS), Startup
/// apps (Windows). Nothing on Linux.
#[frb(sync)]
pub fn open_launch_at_login_settings() -> Result<(), String> {
    crate::autostart::open_settings()
}

/// At start: points an enabled Windows entry at the current executable
/// (the app may have moved). Nothing elsewhere.
#[frb(sync)]
pub fn refresh_launch_at_login() {
    crate::autostart::refresh();
}

/// Started with `--background` (the login entry): the tray only, no
/// window.
#[frb(sync)]
pub fn started_in_background() -> bool {
    crate::autostart::started_in_background()
}

// --- updates ---

/// How this build updates itself (#401 step 5, `update.rs`). macOS: the
/// runner's Sparkle, nothing here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiUpdater {
    /// Windows: WinSparkle runs; the app shows 「检查更新…」
    /// ([`check_for_updates`]) and the automatic-check switch.
    pub installs: bool,
    /// Windows: WinSparkle checks on its own schedule
    /// ([`set_auto_check_updates`]).
    pub auto_check: bool,
    /// Linux: the repository's latest.json, to compare with `build` (a
    /// notice, the package manager upgrades); `None` in a development build.
    pub notice_feed: Option<String>,
    /// This build, `<version>.<build number>`, as latest.json's `build`.
    pub build: String,
    /// Linux: the upgrade command for this install's package format.
    pub upgrade_command: String,
}

/// Starts the updater (Windows: WinSparkle, with its scheduled check) and
/// says what this build can do. Call once the window exists.
#[frb(sync)]
pub fn start_updater() -> UiUpdater {
    crate::update::start()
}

/// A check the user asked for, with WinSparkle's own progress and result
/// windows (Windows; nothing elsewhere).
#[frb(sync)]
pub fn check_for_updates() {
    crate::update::check();
}

/// Turns WinSparkle's scheduled checks on or off and saves it as the native
/// app does (Windows).
#[frb(sync)]
pub fn set_auto_check_updates(enabled: bool) -> Result<(), String> {
    crate::update::set_auto_check(enabled)
}

/// The service's side of silent updates (#548), at launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiSilentUpdate {
    /// The service installs updates silently (macOS and Windows, a release
    /// build, a service of 0.7.5 or later).
    pub available: bool,
    /// The service is installing one now: the app says so and quits
    /// (docs/host-integration.md section 16.5).
    pub installing: bool,
    /// This build was just installed silently: 「已更新到 PPVPN {v}」, once.
    pub updated_to: Option<String>,
}

/// The channel's release, after [`silent_update_prepare`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UiSilentPrepared {
    UpToDate,
    /// Downloaded and checked: installed when the app quits or is idle.
    Ready,
    /// No silent update for it (no package, or three failures): Sparkle's
    /// or WinSparkle's prompt.
    Prompt,
    /// The pointer or the download failed: try again later.
    Later,
}

/// Reads the service's state, and handles its last result once (the
/// failures it counts, the notice).
pub async fn silent_update_state() -> Result<UiSilentUpdate, String> {
    bridge::get()?.run(crate::silent::state()).await
}

/// Checks the channel and downloads a newer package.
pub async fn silent_update_prepare() -> Result<UiSilentPrepared, String> {
    bridge::get()?.run(crate::silent::prepare()).await
}

/// Hands the downloaded package to the service: true when it accepted it,
/// and the app must now exit. With `relaunch` (idle, not at quit) the
/// service opens the app again, in the tray.
pub async fn silent_update_install(relaunch: bool) -> Result<bool, String> {
    bridge::get()?.run(crate::silent::install(relaunch)).await
}

/// A package downloaded and saved earlier that this launch may install
/// (docs/host-integration.md section 16.1): the switch on, newer, not given
/// up, not tried at a launch before. Reads the file only; before [`start`].
#[frb(sync)]
pub fn launch_install_pending() -> bool {
    crate::silent::launch_install_pending()
}

/// Installs that package at launch, before [`start`]: hashes the cached
/// copy again (never downloads), asks a service that supports silent
/// updates and is not installing, and sends it with `relaunch`; true when
/// the service accepted it and the app must now exit. With `hidden` (a
/// launch into the tray) the app it opens again starts in the tray. Each
/// build is tried at one launch only. Blocks a worker thread for at most a
/// minute in all; on a runtime of its own, the client's is not there yet,
/// and into the log file the client would otherwise open.
pub fn silent_update_install_at_launch(hidden: bool) -> bool {
    if let Ok((_, log_dir)) = crate::dirs::dirs() {
        let app_version = format!("{}-flutter", crate::update::version());
        ppvpn_client::install_logging(&log_dir, &app_version);
    }
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            tracing::warn!(%error, "silent update at launch: no runtime");
            return false;
        }
    };
    runtime.block_on(crate::silent::install_at_launch(hidden))
}

/// The 「自动更新」 switch; `None` before it was first set.
#[frb(sync)]
pub fn auto_update() -> Option<bool> {
    crate::silent::auto_update()
}

/// Saves the switch; false when it could not be saved. Off drops a
/// downloaded package.
#[frb(sync)]
pub fn set_auto_update(enabled: bool) -> bool {
    crate::silent::set_auto_update(enabled)
}

/// The service opened the app again after an idle install: it starts in
/// the tray. [`clear_start_hidden`] once it is known not to be installing.
#[frb(sync)]
pub fn start_hidden() -> bool {
    crate::silent::start_hidden()
}

#[frb(sync)]
pub fn clear_start_hidden() {
    crate::silent::clear_start_hidden();
}

/// The user quit while an idle install's request was out, and the service
/// accepted it: marked so that the app it opens again quits.
#[frb(sync)]
pub fn mark_user_quit() {
    crate::silent::mark_user_quit();
}

/// The service opened the app again after the user had quit it: it quits
/// at once. Clears the mark.
#[frb(sync)]
pub fn quit_after_relaunch() -> bool {
    crate::silent::quit_after_relaunch()
}
