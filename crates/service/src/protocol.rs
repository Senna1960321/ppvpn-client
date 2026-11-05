//! IPC protocol shared by main app and service. Kept byte-identical on both
//! sides — if you change this, mirror the change in
//! `crates/desktop/src/service.rs`.
//!
//! Wire format:
//!   [u32 BE: length of JSON payload] [JSON bytes]
//!   After the named-pipe client identity is accepted, an unsigned one-shot
//!   handshake returns a short-lived HMAC key bound to that client process.
//!   All later request and response envelopes are signed with that key.

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(windows)]
pub const IPC_PIPE_NAME: &str = r"\\.\pipe\ppvpn-service";

#[cfg(target_os = "macos")]
pub const IPC_SOCKET_PATH: &str = "/Library/Application Support/PPVPN/run/service.sock";

#[cfg(all(unix, not(target_os = "macos")))]
pub const IPC_SOCKET_PATH: &str = "/run/ppvpn/service.sock";

#[cfg(target_os = "linux")]
pub const LINUX_LOG_DIR: &str = "/var/log/ppvpn";

/// Messages older than this are rejected as replay attempts.
pub const MESSAGE_EXPIRY_SECONDS: u64 = 30;

pub fn sign_with(key: &[u8], message: &str) -> Result<String> {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(key).context("HMAC init failed")?;
    mac.update(message.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

fn verify_with(key: &[u8], message: &str, signature: &str) -> Result<bool> {
    type HmacSha256 = Hmac<Sha256>;
    let signature = match hex::decode(signature) {
        Ok(value) => value,
        Err(_) => return Ok(false),
    };
    let mut mac = HmacSha256::new_from_slice(key).context("HMAC init failed")?;
    mac.update(message.as_bytes());
    Ok(mac.verify_slice(&signature).is_ok())
}

pub fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Commands accepted by the service. Keep the enum tag names stable once
/// shipped — changing them breaks old clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Command {
    Handshake,
    GetVersion,
    GetStatus,
    Connect,
    UpdateProfile,
    RenewLease,
    Disconnect,
    CoreApi,
    /// Payload: a [`SessionRef`]. Keeps the connection open: the service
    /// answers once (`{"event":"watching","keepalive_ms":…}`), then writes
    /// one signed [`Response`] frame per event, each with the request's id
    /// and `data` `{"event":…,"seq":n}`: `keepalive` every
    /// `keepalive_ms`, and finally `stopping` (the service is shutting
    /// down) or `core_stopped` (with `reason`), after which it closes the
    /// connection. EOF or a broken pipe at any time: the service is gone.
    /// A service that predates this command closes the connection without
    /// an answer.
    Watch,
    /// Payload: an [`InstallUpdatePayload`]. Silent desktop update
    /// (docs/host-integration.md section 16): the service copies and checks
    /// the package, answers `{"accepted":true}` (or an `UPDATE_*` error),
    /// then installs it once the client process has exited. The result
    /// shows in [`VersionInfo::last_update`]. A service that predates this
    /// command closes the connection without an answer.
    InstallUpdate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: String,
    pub timestamp: u64,
    pub command: Command,
    pub payload: serde_json::Value,
    #[serde(default)]
    pub auth_token: Option<String>,
    pub signature: String,
}

impl Request {
    /// Compute signature excluding the signature field itself.
    fn canonical(&self) -> Result<String> {
        let bare = Request {
            id: self.id.clone(),
            timestamp: self.timestamp,
            command: self.command.clone(),
            payload: self.payload.clone(),
            auth_token: self.auth_token.clone(),
            signature: String::new(),
        };
        Ok(serde_json::to_string(&bare)?)
    }

    #[allow(dead_code)]
    pub fn signed(
        id: String,
        command: Command,
        payload: serde_json::Value,
        auth_token: Option<String>,
        key: &[u8],
    ) -> Result<Self> {
        let mut req = Request {
            id,
            timestamp: now_epoch(),
            command,
            payload,
            auth_token,
            signature: String::new(),
        };
        req.signature = sign_with(key, &req.canonical()?)?;
        Ok(req)
    }

    pub fn verify_with(&self, key: &[u8]) -> Result<bool> {
        verify_with(key, &self.canonical()?, &self.signature)
    }

    pub fn is_fresh(&self) -> bool {
        let now = now_epoch();
        now >= self.timestamp && now - self.timestamp <= MESSAGE_EXPIRY_SECONDS
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: String,
    pub success: bool,
    pub data: Option<serde_json::Value>,
    pub error: Option<String>,
    pub signature: String,
}

impl Response {
    fn canonical(&self) -> Result<String> {
        let bare = Response {
            id: self.id.clone(),
            success: self.success,
            data: self.data.clone(),
            error: self.error.clone(),
            signature: String::new(),
        };
        Ok(serde_json::to_string(&bare)?)
    }

    pub fn signed_with(
        id: String,
        success: bool,
        data: Option<serde_json::Value>,
        error: Option<String>,
        key: &[u8],
    ) -> Result<Self> {
        let mut resp = Response {
            id,
            success,
            data,
            error,
            signature: String::new(),
        };
        resp.signature = sign_with(key, &resp.canonical()?)?;
        Ok(resp)
    }

    pub fn unsigned(
        id: String,
        success: bool,
        data: Option<serde_json::Value>,
        error: Option<String>,
    ) -> Self {
        Self {
            id,
            success,
            data,
            error,
            signature: String::new(),
        }
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn verify_with(&self, key: &[u8]) -> Result<bool> {
        verify_with(key, &self.canonical()?, &self.signature)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakePayload {
    pub client_nonce: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeResponse {
    pub auth_token: String,
    pub auth_key: String,
    pub expires_at: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRef {
    pub session_id: String,
    pub generation: u64,
}

/// A node fixed to one of its ingresses.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngressPin {
    pub node_id: String,
    pub endpoint_key: String,
}

/// The client's node selection and ingress pins, forwarded with every
/// `apply-profile` of the session's core: the engine applies them together
/// with the profile, so a new core needs no `select-node` / `pin-ingress`
/// afterwards. Absent from older clients: the profile's default node, no
/// pins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyChoices {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_node_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pins: Vec<IngressPin>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ConnectPayload {
    #[serde(flatten)]
    pub session: SessionRef,
    pub profile: serde_json::Value,
    /// Replace a core owned by another session of the same OS user.
    pub take_over: bool,
    /// Authority of the API the profile came from; forwarded to the core's
    /// `apply-profile` when the core accepts it (0.5.0+). Optional: absent
    /// from older clients, whose profiles then apply without rule-set
    /// downloads.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_rule_set_hosts: Vec<String>,
    /// `rules` (absent: older clients) or `global`; forwarded to the core's
    /// `apply-profile` when the core accepts it (0.5.6+).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_mode: Option<String>,
    #[serde(flatten)]
    pub choices: ApplyChoices,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateProfilePayload {
    #[serde(flatten)]
    pub session: SessionRef,
    pub profile: serde_json::Value,
    /// As [`ConnectPayload::allowed_rule_set_hosts`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_rule_set_hosts: Vec<String>,
    /// As [`ConnectPayload::routing_mode`]. The same revision in another
    /// mode is applied again (the core dedupes on both).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing_mode: Option<String>,
    /// As [`ConnectPayload::choices`]; also used for a rollback, so the
    /// previous revision comes back with the client's current choices.
    #[serde(flatten)]
    pub choices: ApplyChoices,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CoreApiPayload {
    #[serde(flatten)]
    pub session: SessionRef,
    pub path: String,
    pub body: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub running: bool,
    pub pid: i64,
    pub bin_path: String,
    pub mode: String,
    pub service_version: String,
    /// [`SERVICE_BUILD_ID`]; absent from older services.
    pub service_build_id: String,
    pub session_id: Option<String>,
    pub generation: u64,
    pub profile_revision: Option<String>,
    pub lease_expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VersionInfo {
    pub service: String,
    pub version: String,
    /// [`SERVICE_BUILD_ID`]; absent from older services, which
    /// clients treat as outdated.
    pub build_id: String,
    /// The release's build number baked into this service (0 in a build
    /// without one). This and the next two are absent from services that
    /// predate `InstallUpdate`.
    pub build: u64,
    /// The silent update being installed, if any.
    pub update: Option<UpdateProgress>,
    /// The last silent update's result, if any.
    pub last_update: Option<UpdateResult>,
}

/// `InstallUpdate`'s payload: the package the client downloaded and what
/// the update feed says about it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallUpdatePayload {
    pub path: String,
    pub version: String,
    pub build: u64,
    pub length: u64,
    pub ed_signature: String,
    pub relaunch: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateProgress {
    pub version: String,
    pub build: u64,
    /// `waiting` (for the client to exit) or `installing`.
    pub phase: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateResult {
    pub version: String,
    pub build: u64,
    /// `installed` or `failed`.
    pub outcome: String,
    /// The `UPDATE_*` code of a failure (`UPDATE_INSTALL_FAILED:<detail>`
    /// for the installer's); `None` when installed.
    pub error: Option<String>,
    pub finished_at_ms: u64,
}

/// SHA-256 of this service's sources (build.rs). Clients built from the same
/// tree expect exactly this value and reinstall a service that reports
/// another one (or none).
pub const SERVICE_BUILD_ID: &str = env!("PPVPN_SERVICE_BUILD_ID");
