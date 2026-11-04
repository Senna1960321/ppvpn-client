//! Backend HTTP client and response models.
//!
//! A desktop or CLI access token may only call a handful of bearer
//! endpoints; everything else goes through the public device-authorization
//! endpoints ([`crate::auth`]).
//!
//! Errors never carry response bodies or credentials: only the HTTP status and
//! the backend problem `code` travel into [`ApiError`].

use std::sync::Arc;
use std::time::Duration;

use reqwest::{StatusCode, Url};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::Value;

pub const PATH_DEVICE_CODE: &str = "/api/v1/auth/device/code";
pub const PATH_DEVICE_TOKEN: &str = "/api/v1/auth/device/token";
pub const PATH_DEVICE_ACTIVATE: &str = "/api/v1/auth/device/activate";
pub const PATH_DEVICE_REFRESH: &str = "/api/v1/auth/device/refresh";
pub const PATH_DEVICE_REFRESH_COMMIT: &str = "/api/v1/auth/device/refresh/commit";
pub const PATH_DEVICE_REVOKE: &str = "/api/v1/auth/device/revoke";
pub const PATH_USERS_ME: &str = "/api/v1/users/me";
pub const PATH_PROXY_PROFILE: &str = "/api/v1/me/proxy-profile";
pub const PATH_TEAMS: &str = "/api/v1/me/teams";
pub const PATH_SWITCH_TEAM: &str = "/api/v1/me/switch-team";
pub const PATH_MESSAGES: &str = "/api/v1/messages";
pub const PATH_MESSAGES_UNREAD: &str = "/api/v1/messages/unread-count";
pub const PATH_MESSAGES_READ_ALL: &str = "/api/v1/messages/read-all";
pub const PATH_DEVICES: &str = "/api/v1/devices";
pub const PATH_DEVICES_REGISTER: &str = "/api/v1/devices/register";
pub const PATH_DEVICES_UNREGISTER: &str = "/api/v1/devices/unregister";

/// Longest backend problem code copied into an error detail.
const MAX_PROBLEM_CODE_LEN: usize = 64;
/// Most characters of a problem's `detail` kept for display.
pub const MAX_PROBLEM_DETAIL_LEN: usize = 200;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// `error` and every `source()` under it, joined with `: ` (#537): reqwest's
/// own message only says the request failed, the cause (a reset, a closed
/// pooled connection, an OS error) is further down. A level whose text the
/// previous one already contains is skipped.
pub fn error_chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut chain = error.to_string();
    let mut last = chain.clone();
    let mut source = error.source();
    while let Some(error) = source {
        let text = error.to_string();
        if !last.contains(&text) {
            chain.push_str(": ");
            chain.push_str(&text);
        }
        last = text;
        source = error.source();
    }
    chain
}

/// A failed backend call, kept structured until the caller decides whether it
/// is terminal for the saved credential.
#[derive(Debug)]
pub enum ApiError {
    /// No HTTP response (DNS, TCP, TLS, timeout).
    Transport(reqwest::Error),
    /// Non-success status with the backend problem code, if any.
    Status {
        /// `METHOD /path` of the request (no query, no credentials).
        request: String,
        status: StatusCode,
        code: Option<String>,
        /// RFC 3339 end of the subscription, on `SUBSCRIPTION_EXPIRED`
        /// problems that carry one.
        expired_at: Option<String>,
        /// The problem's `detail`, for showing to the user as it is (one
        /// line, at most [`MAX_PROBLEM_DETAIL_LEN`] characters). Never
        /// branch on it: the code says what happened.
        detail: Option<String>,
    },
    /// A success status with a body we could not decode.
    Decode(&'static str),
    /// The configured API base cannot be used.
    Config(&'static str),
}

impl ApiError {
    /// 401/403 from a credential endpoint: the saved login can never work
    /// again and must be cleared.
    pub fn is_terminal_credential(&self) -> bool {
        // A disabled team is not a dead login: keep the credential so the
        // user can switch teams.
        !self.is_team_disabled()
            && matches!(
                self,
                Self::Status { status, .. }
                    if *status == StatusCode::UNAUTHORIZED || *status == StatusCode::FORBIDDEN
            )
    }

    /// 403 with problem code `403012`: the current team is disabled.
    pub fn is_team_disabled(&self) -> bool {
        matches!(
            self,
            Self::Status { status, code: Some(code), .. }
                if *status == StatusCode::FORBIDDEN && code == PROBLEM_TEAM_DISABLED
        )
    }

    pub fn is_unauthorized(&self) -> bool {
        matches!(self, Self::Status { status, .. } if *status == StatusCode::UNAUTHORIZED)
    }

    /// Worth retrying on the next poll or refresh tick.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Status { status, .. } => {
                *status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
            }
            Self::Decode(_) | Self::Config(_) => false,
        }
    }

    pub fn is_rate_limited(&self) -> bool {
        matches!(self, Self::Status { status, .. } if *status == StatusCode::TOO_MANY_REQUESTS)
    }

    /// See [`subscription_problem`].
    pub fn subscription_problem(&self) -> Option<SubscriptionProblem> {
        match self {
            Self::Status { status, code, .. } => subscription_problem(*status, code.as_deref()),
            _ => None,
        }
    }
}

/// `"<METHOD> <path> -> HTTP <status> <problem code>"`; the backend reuses
/// problem codes across endpoints, so the request tells them apart.
pub fn status_detail(request: &str, status: StatusCode, code: Option<&str>) -> String {
    match code {
        Some(code) => format!("{request} -> HTTP {} {code}", status.as_u16()),
        None => format!("{request} -> HTTP {}", status.as_u16()),
    }
}

/// Backend problem code for a suspended team (e.g. switching into one).
pub const PROBLEM_TEAM_DISABLED: &str = "403012";
/// Backend problem code for a dissolved team; shown like a suspended one.
pub const PROBLEM_TEAM_DISSOLVED: &str = "404011";
/// Backend problem code for an expired subscription; may carry its end.
pub const PROBLEM_SUBSCRIPTION_EXPIRED: &str = "SUBSCRIPTION_EXPIRED";
/// Proxy-profile 404: the team has no live subscription.
pub const PROBLEM_SUBSCRIPTION_REQUIRED: &str = "SUBSCRIPTION_REQUIRED";
/// Proxy-profile 404: the subscription exists, but no endpoint is published
/// for it yet.
pub const PROBLEM_PROXY_NODES_NOT_READY: &str = "PROXY_NODES_NOT_READY";

/// What a failed team-scoped call says about the team's service, on any
/// endpoint and status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubscriptionProblem {
    /// The current team is suspended (`403012`) or dissolved (`404011`).
    TeamDisabled,
    /// The subscription has ended.
    Expired,
    /// No (active) subscription, or payment required.
    NoSubscription,
    /// The subscription exists, but its nodes are not published yet
    /// (`PROXY_NODES_NOT_READY`); they usually arrive within minutes.
    NodesNotReady,
}

/// The [`SubscriptionProblem`] a status and problem code stand for, if any.
pub fn subscription_problem(status: StatusCode, code: Option<&str>) -> Option<SubscriptionProblem> {
    match code {
        Some(PROBLEM_TEAM_DISABLED) if status == StatusCode::FORBIDDEN => {
            Some(SubscriptionProblem::TeamDisabled)
        }
        Some(PROBLEM_TEAM_DISSOLVED) if status == StatusCode::NOT_FOUND => {
            Some(SubscriptionProblem::TeamDisabled)
        }
        Some(PROBLEM_SUBSCRIPTION_EXPIRED) => Some(SubscriptionProblem::Expired),
        Some(PROBLEM_PROXY_NODES_NOT_READY) => Some(SubscriptionProblem::NodesNotReady),
        Some(
            PROBLEM_SUBSCRIPTION_REQUIRED
            | "SUBSCRIPTION_NOT_FOUND"
            | "SUBSCRIPTION_NOT_ACTIVE"
            | "POINTS_NO_ACTIVE_SUBSCRIPTION",
        ) => Some(SubscriptionProblem::NoSubscription),
        _ if status == StatusCode::PAYMENT_REQUIRED => Some(SubscriptionProblem::NoSubscription),
        _ => None,
    }
}

#[derive(Deserialize)]
struct ApiProblem {
    #[serde(default)]
    code: Option<String>,
    /// Subscription end on `SUBSCRIPTION_EXPIRED` (accepted as `expired_at`
    /// or `expires_at`).
    #[serde(default, alias = "expires_at")]
    expired_at: Option<String>,
    /// RFC 9457 `detail`: the explanation shown to the user.
    #[serde(default)]
    detail: Option<String>,
    /// Human-readable backend explanation; logged, never put in details.
    #[serde(default)]
    message: Option<String>,
}

// ---------------------------------------------------------------------------
// Client
// ---------------------------------------------------------------------------

/// What a host configures an [`Api`] with.
#[derive(Clone, Debug)]
pub struct ApiConfig {
    /// Backend base URL, e.g. `https://api.example.com` (a path prefix is
    /// kept). HTTPS only, except loopback hosts in debug builds or with
    /// `trust_local_backend`.
    pub base: String,
    /// Product audience sent as `X-Product-Aud` on device-authorization
    /// calls. `None` sends no header: the backend then runs its CLI device
    /// authorization, which is the only way to obtain a `cli` token (it
    /// rejects `cli` as a header value on purpose).
    pub header_audience: Option<String>,
    /// The audience access tokens must carry, exactly. Usually the header's
    /// value; `cli` for the header-less CLI flow.
    pub token_audience: String,
    /// `Accept-Language` value (e.g. `zh-CN`): the backend localises display
    /// names by it. `None` sends no header.
    pub accept_language: Option<String>,
    /// Test relaxations for a local mock backend: plain `http` and
    /// self-signed TLS, the backend's own `http` verification page, short
    /// device codes. Applied only when `base` is a loopback host.
    pub trust_local_backend: bool,
}

/// The desktop apps' product audience: sent as `X-Product-Aud` so the
/// backend issues desktop sessions, and required on their access tokens.
pub const DESKTOP_AUDIENCE: &str = "ppvpn";

/// The CLI's token audience. The backend issues `cli` tokens only through
/// the device authorization that carries no product header (it rejects
/// `cli` as a header value on purpose), so the CLI sends none.
pub const CLI_AUDIENCE: &str = "cli";

impl ApiConfig {
    /// The desktop apps: product header and token audience
    /// [`DESKTOP_AUDIENCE`].
    pub fn desktop(base: &str, accept_language: Option<String>, trust_local_backend: bool) -> Self {
        Self {
            base: base.to_string(),
            header_audience: Some(DESKTOP_AUDIENCE.to_string()),
            token_audience: DESKTOP_AUDIENCE.to_string(),
            accept_language,
            trust_local_backend,
        }
    }

    /// The CLI: no product header, token audience [`CLI_AUDIENCE`].
    pub fn cli(base: &str, accept_language: Option<String>, trust_local_backend: bool) -> Self {
        Self {
            base: base.to_string(),
            header_audience: None,
            token_audience: CLI_AUDIENCE.to_string(),
            accept_language,
            trust_local_backend,
        }
    }
}

/// HTTP client bound to one configured API base.
pub struct Api {
    /// Through the OS proxy ([`crate::sysproxy`]), read at each request.
    http: reqwest::Client,
    /// No proxy: when the OS proxy cannot be reached.
    direct: reqwest::Client,
    dns_recovery: Option<Arc<crate::service_dns::RecoveryDns>>,
    base: String,
    header_audience: Option<String>,
    token_audience: String,
    /// `trust_local_backend` and a loopback base.
    trust_local: bool,
}

/// `Accept-Language` with the system locale (e.g. `zh-CN`): the backend
/// localises display names (node entry and line labels) by it, zh for any
/// zh-*, else en, as the apps pick their UI language.
fn language_headers(locale: &str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Ok(value) = reqwest::header::HeaderValue::from_str(locale) {
        headers.insert(reqwest::header::ACCEPT_LANGUAGE, value);
    }
    headers
}

/// `base` names this machine (loopback address or `localhost`).
pub fn is_loopback_base(base: &str) -> bool {
    let Ok(url) = Url::parse(base.trim()) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

impl Api {
    pub fn new(config: ApiConfig) -> Self {
        let trust_local = config.trust_local_backend && is_loopback_base(&config.base);
        let dns_recovery = crate::service_dns::RecoveryDns::new(&config.base).map(Arc::new);
        let headers = config
            .accept_language
            .as_deref()
            .map(language_headers)
            .unwrap_or_default();
        // Building only fails when the TLS backend cannot initialise; fall
        // back to the default client instead of panicking (hosts may call
        // this across an FFI boundary).
        let builder = || {
            let builder = reqwest::Client::builder()
                .default_headers(headers.clone())
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(20))
                // Shorter than the 60 s polls (#537): a connection idle that
                // long is not reused, a path may have dropped it silently.
                .pool_idle_timeout(Duration::from_secs(30))
                // A local mock backend may use a self-signed certificate;
                // only ever for a loopback base under the test switch.
                .danger_accept_invalid_certs(trust_local);
            match &dns_recovery {
                Some(resolver) => builder.dns_resolver(resolver.clone()),
                None => builder,
            }
        };
        // The OS proxy, as the native apps' HTTP stacks used it (#476).
        let http = builder()
            .proxy(reqwest::Proxy::custom(crate::sysproxy::for_url))
            .build()
            .unwrap_or_default();
        let direct = builder().no_proxy().build().unwrap_or_default();
        Self {
            http,
            direct,
            dns_recovery,
            base: config.base.trim().to_string(),
            header_audience: config.header_audience,
            token_audience: config.token_audience,
            trust_local,
        }
    }

    /// The audience access tokens must carry.
    pub fn token_audience(&self) -> &str {
        &self.token_audience
    }

    /// The local-backend test relaxations apply.
    pub fn trusts_local_backend(&self) -> bool {
        self.trust_local
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// After an explicitly requested diagnostic, recover only when it proves
    /// an alternate TLS-valid API address. This lasts for this Api instance;
    /// it changes no system settings and stores no IP addresses.
    pub fn recover_dns_from(&self, report: &crate::reachability::Report) -> bool {
        self.dns_recovery
            .as_ref()
            .is_some_and(|dns| dns.enable_from(report))
    }

    /// `base` + `path`, keeping any path prefix of the base. HTTPS only,
    /// except loopback hosts in debug builds or under the test switch.
    pub fn endpoint(&self, path: &str) -> Result<Url, ApiError> {
        let mut url =
            Url::parse(&self.base).map_err(|_| ApiError::Config("API base URL is invalid"))?;
        let local_http = url.scheme() == "http"
            && (self.trust_local || (cfg!(debug_assertions) && is_loopback_base(&self.base)));
        if url.scheme() != "https" && !local_http {
            return Err(ApiError::Config("API base URL must use HTTPS"));
        }
        let prefix = url.path().trim_end_matches('/').to_string();
        url.set_path(&format!("{prefix}{path}"));
        url.set_query(None);
        url.set_fragment(None);
        Ok(url)
    }

    /// Sends through the OS proxy; when that proxy cannot be reached (a
    /// proxy app quit and left the setting on), this request goes direct.
    async fn send(
        &self,
        request: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let request = request.build()?;
        let fallback = crate::sysproxy::for_url(request.url())
            .and_then(|proxy| Some((proxy, request.try_clone()?)));
        match self.http.execute(request).await {
            Err(error) if error.is_connect() => match fallback {
                Some((proxy, request)) => {
                    tracing::warn!(
                        proxy = %proxy.host_str().unwrap_or_default(),
                        port = proxy.port_or_known_default().unwrap_or_default(),
                        error = %error_chain(&error),
                        "API: the system proxy is unreachable, going direct"
                    );
                    self.direct.execute(request).await
                }
                None => Err(error),
            },
            result => result,
        }
    }

    /// Unauthenticated device-authorization call.
    pub async fn post_public<T: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
    ) -> Result<T, ApiError> {
        let mut request = self.http.post(self.endpoint(path)?);
        if let Some(audience) = &self.header_audience {
            request = request.header("X-Product-Aud", audience);
        }
        decode("POST", self.send(request.json(body)).await).await
    }

    async fn get_bearer<T: DeserializeOwned>(
        &self,
        path: &str,
        token: &str,
    ) -> Result<T, ApiError> {
        let request = self.http.get(self.endpoint(path)?).bearer_auth(token);
        decode("GET", self.send(request).await).await
    }

    /// `GET /users/me`.
    pub async fn account(&self, token: &str) -> Result<UserInfo, ApiError> {
        let user: UserResponse = self.get_bearer(PATH_USERS_ME, token).await?;
        Ok(user.into_user_info())
    }

    /// `GET /me/teams`.
    pub async fn teams(&self, token: &str) -> Result<Vec<TeamEntry>, ApiError> {
        let list: TeamListResponse = self.get_bearer(PATH_TEAMS, token).await?;
        Ok(list.items)
    }

    /// `POST /me/switch-team`; returns the team and a fresh access token
    /// bound to it.
    pub async fn switch_team(
        &self,
        token: &str,
        team_id: &str,
    ) -> Result<SwitchTeamResponse, ApiError> {
        let request = self
            .http
            .post(self.endpoint(PATH_SWITCH_TEAM)?)
            .bearer_auth(token)
            .json(&serde_json::json!({ "team_id": team_id }));
        decode("POST", self.send(request).await).await
    }

    async fn get_query<T: DeserializeOwned>(
        &self,
        path: &str,
        token: &str,
        query: &[(&str, String)],
    ) -> Result<T, ApiError> {
        let mut url = self.endpoint(path)?;
        url.query_pairs_mut()
            .extend_pairs(query.iter().map(|(key, value)| (*key, value.as_str())));
        let request = self.http.get(url).bearer_auth(token);
        decode("GET", self.send(request).await).await
    }

    async fn put_query<T: DeserializeOwned>(
        &self,
        path: &str,
        token: &str,
        query: &[(&str, String)],
    ) -> Result<T, ApiError> {
        let mut url = self.endpoint(path)?;
        if !query.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(query.iter().map(|(key, value)| (*key, value.as_str())));
        }
        let request = self.http.put(url).bearer_auth(token);
        decode("PUT", self.send(request).await).await
    }

    /// `GET /messages?page=&page_size=`: newest first.
    pub async fn messages(
        &self,
        token: &str,
        page: u32,
        page_size: u32,
    ) -> Result<MessagePage, ApiError> {
        self.get_query(
            PATH_MESSAGES,
            token,
            &[
                ("page", page.to_string()),
                ("page_size", page_size.to_string()),
            ],
        )
        .await
    }

    /// `GET /messages/unread-count`.
    pub async fn unread_count(&self, token: &str) -> Result<u64, ApiError> {
        let count: UnreadCount = self.get_bearer(PATH_MESSAGES_UNREAD, token).await?;
        Ok(count.count)
    }

    /// `PUT /messages/{id}/read` (idempotent).
    pub async fn mark_message_read(&self, token: &str, id: u64) -> Result<(), ApiError> {
        let _: Value = self
            .put_query(&format!("{PATH_MESSAGES}/{id}/read"), token, &[])
            .await?;
        Ok(())
    }

    /// `POST /devices/register`: registers (or re-registers) this install for
    /// push. On the desktop channel the push token is only in this answer.
    pub async fn register_device(
        &self,
        token: &str,
        body: &crate::device::DeviceRegistrationRequest,
    ) -> Result<DeviceRegistration, ApiError> {
        let request = self
            .http
            .post(self.endpoint(PATH_DEVICES_REGISTER)?)
            .bearer_auth(token)
            .json(body);
        decode("POST", self.send(request).await).await
    }

    /// `POST /devices/unregister`: disables the device holding an APNs or
    /// FCM `push_token` (official channels; desktop devices use
    /// [`Self::delete_device`]).
    pub async fn unregister_push_token(
        &self,
        token: &str,
        push_token: &str,
    ) -> Result<(), ApiError> {
        let request = self
            .http
            .post(self.endpoint(PATH_DEVICES_UNREGISTER)?)
            .bearer_auth(token)
            .json(&serde_json::json!({ "token": push_token }));
        let response = self.send(request).await.map_err(ApiError::Transport)?;
        check_status("POST", response).await.map(|_| ())
    }

    /// `DELETE /devices/{id}`; an unknown device counts as deleted.
    pub async fn delete_device(&self, token: &str, id: u64) -> Result<(), ApiError> {
        let request = self
            .http
            .delete(self.endpoint(&format!("{PATH_DEVICES}/{id}"))?)
            .bearer_auth(token);
        let response = self.send(request).await.map_err(ApiError::Transport)?;
        match check_status("DELETE", response).await {
            Ok(_) => Ok(()),
            Err(ApiError::Status { status, .. }) if status == StatusCode::NOT_FOUND => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// `PUT /messages/{id}/unread`.
    // TODO(backend): endpoint requested, contract not yet confirmed.
    pub async fn mark_message_unread(&self, token: &str, id: u64) -> Result<(), ApiError> {
        let _: Value = self
            .put_query(&format!("{PATH_MESSAGES}/{id}/unread"), token, &[])
            .await?;
        Ok(())
    }

    /// `PUT /messages/read-all?up_to_id=`.
    pub async fn mark_all_messages_read(&self, token: &str, up_to_id: u64) -> Result<(), ApiError> {
        let _: Value = self
            .put_query(
                PATH_MESSAGES_READ_ALL,
                token,
                &[("up_to_id", up_to_id.to_string())],
            )
            .await?;
        Ok(())
    }

    /// `GET /me/proxy-profile` as raw bytes; the core validates the document.
    pub async fn proxy_profile(&self, token: &str) -> Result<Vec<u8>, ApiError> {
        let request = self
            .http
            .get(self.endpoint(PATH_PROXY_PROFILE)?)
            .bearer_auth(token)
            .header("Cache-Control", "no-cache");
        let response = self.send(request).await.map_err(ApiError::Transport)?;
        let response = check_status("GET", response).await?;
        let bytes = response.bytes().await.map_err(ApiError::Transport)?;
        Ok(bytes.to_vec())
    }
}

async fn check_status(
    method: &str,
    response: reqwest::Response,
) -> Result<reqwest::Response, ApiError> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    // Path only: queries (paging cursors) are left out, and credentials
    // travel in headers or bodies, never in the URL.
    let request = format!("{method} {}", response.url().path());
    let problem = response.json::<ApiProblem>().await.ok();
    let (code, message, expired_at, detail) = match problem {
        Some(problem) => (
            problem.code,
            problem.message,
            problem.expired_at,
            problem.detail,
        ),
        None => (None, None, None, None),
    };
    let expired_at = expired_at.filter(|at| is_timestamp(at));
    let code = code.filter(|code| code.len() <= MAX_PROBLEM_CODE_LEN);
    if let Some(message) = message.filter(|message| !message.is_empty()) {
        tracing::info!(
            %request,
            status = status.as_u16(),
            code = code.as_deref().unwrap_or(""),
            "backend problem: {}",
            message.chars().take(300).collect::<String>()
        );
    }
    Err(ApiError::Status {
        request,
        status,
        code,
        expired_at,
        detail: detail.as_deref().and_then(display_detail),
    })
}

/// `detail` as one printable line of at most [`MAX_PROBLEM_DETAIL_LEN`]
/// characters, or `None` when nothing is left.
fn display_detail(detail: &str) -> Option<String> {
    let line = detail
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_PROBLEM_DETAIL_LEN)
        .collect::<String>();
    (!line.is_empty()).then_some(line)
}

/// Marks the subscription end inside a `SubscriptionExpired` error detail.
pub const EXPIRED_AT_TAG: &str = "expired_at=";

/// A plausible RFC 3339 timestamp (only its characters and length are
/// checked; it is shown, never computed with).
fn is_timestamp(value: &str) -> bool {
    (10..=40).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b':' | b'+' | b'.'))
}

/// The subscription end carried in a `SubscriptionExpired` detail.
pub fn expired_at_from_detail(detail: &str) -> Option<String> {
    detail
        .split(' ')
        .find_map(|part| part.strip_prefix(EXPIRED_AT_TAG))
        .filter(|at| is_timestamp(at))
        .map(str::to_string)
}

async fn decode<T: DeserializeOwned>(
    method: &str,
    sent: Result<reqwest::Response, reqwest::Error>,
) -> Result<T, ApiError> {
    let response = check_status(method, sent.map_err(ApiError::Transport)?).await?;
    let bytes = response.bytes().await.map_err(ApiError::Transport)?;
    serde_json::from_slice(&bytes)
        .map_err(|_| ApiError::Decode("backend returned an invalid response"))
}

// ---------------------------------------------------------------------------
// Response models
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Messages (notifications)
// ---------------------------------------------------------------------------

/// One backend message, parsed leniently (old messages may lack fields).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub id: u64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub content: String,
    #[serde(default, rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub event_key: String,
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub deep_link: String,
    #[serde(default)]
    pub push: bool,
    #[serde(default)]
    pub is_read: bool,
    #[serde(default)]
    pub created_at: String,
}

/// `POST /devices/register` answer.
#[derive(Deserialize, Debug)]
pub struct DeviceRegistration {
    pub id: u64,
    /// The self-hosted channel's token (desktop platforms only; empty for
    /// APNs and FCM devices).
    #[serde(default)]
    pub push_token: String,
}

#[derive(Deserialize, Debug, Default)]
pub struct MessagePage {
    #[serde(default)]
    pub items: Vec<Message>,
    #[serde(default)]
    pub total: u64,
}

#[derive(Deserialize)]
struct UnreadCount {
    #[serde(default)]
    count: u64,
}

#[derive(Deserialize)]
struct UserResponse {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    avatar: Option<String>,
    /// Not in the backend's `/users/me` today (identifiers live in
    /// credentials); read when it is added.
    #[serde(default)]
    email: Option<String>,
}

/// The signed-in user (`GET /users/me`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct UserInfo {
    pub id: String,
    pub name: String,
    pub avatar_url: Option<String>,
    pub email: Option<String>,
}

impl UserResponse {
    fn into_user_info(self) -> UserInfo {
        UserInfo {
            id: self.id,
            name: self.name,
            avatar_url: self.avatar.filter(|url| !url.is_empty()),
            email: self.email.filter(|email| !email.trim().is_empty()),
        }
    }
}

#[derive(Deserialize)]
struct TeamListResponse {
    #[serde(default)]
    items: Vec<TeamEntry>,
}

/// One `GET /me/teams` item.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct TeamEntry {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_personal: bool,
    /// The team the current access token is bound to.
    #[serde(default)]
    pub is_default: bool,
    /// `active`, `disabled` or `deleted`; absent means active.
    #[serde(default)]
    pub status: Option<String>,
}

impl TeamEntry {
    /// `active`, or no status at all.
    pub fn is_active(&self) -> bool {
        matches!(self.status.as_deref(), None | Some("active"))
    }
}

#[derive(Deserialize)]
pub struct SwitchTeamResponse {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_personal: bool,
    #[serde(default)]
    pub token: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl std::fmt::Display for Layer {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(self.0)
        }
    }

    impl std::error::Error for Layer {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            self.1.as_deref().map(|layer| layer as _)
        }
    }

    #[test]
    fn the_error_chain_names_every_cause_once() {
        let error = Layer(
            "error sending request for url (https://www.example.com/x)",
            Some(Box::new(Layer(
                "client error (SendRequest)",
                Some(Box::new(Layer(
                    "connection closed before message completed",
                    Some(Box::new(Layer(
                        "connection closed before message completed",
                        None,
                    ))),
                ))),
            ))),
        );
        assert_eq!(
            error_chain(&error),
            "error sending request for url (https://www.example.com/x): \
             client error (SendRequest): connection closed before message completed"
        );
        assert_eq!(error_chain(&Layer("timed out", None)), "timed out");
    }

    fn api(base: &str, trust_local_backend: bool) -> Api {
        Api::new(ApiConfig {
            base: base.to_string(),
            header_audience: Some("app".to_string()),
            token_audience: "app".to_string(),
            accept_language: None,
            trust_local_backend,
        })
    }

    #[test]
    fn client_presets_differ_only_in_audience() {
        let desktop = ApiConfig::desktop("https://api.example.com", Some("zh-CN".into()), true);
        assert_eq!(desktop.header_audience.as_deref(), Some("ppvpn"));
        assert_eq!(desktop.token_audience, "ppvpn");
        assert_eq!(desktop.accept_language.as_deref(), Some("zh-CN"));
        assert!(desktop.trust_local_backend);
        let cli = ApiConfig::cli("https://api.example.com", None, false);
        assert_eq!(cli.header_audience, None);
        assert_eq!(cli.token_audience, "cli");
        assert_eq!(cli.base, "https://api.example.com");
        assert!(!cli.trust_local_backend);
    }

    #[test]
    fn requests_carry_the_configured_language() {
        let headers = language_headers("zh-CN");
        assert_eq!(headers[reqwest::header::ACCEPT_LANGUAGE], "zh-CN");
        // Not a valid header value: no header rather than a failed client.
        assert!(language_headers("zh\nCN").is_empty());
    }

    #[test]
    fn team_status_marks_disabled_teams_inactive() {
        let entries: Vec<TeamEntry> = serde_json::from_str(
            r#"[{"id":"a","name":"A","status":"active"},{"id":"b","name":"B","status":"disabled"},{"id":"c","name":"C"},{"id":"d","name":"D","status":"deleted"}]"#,
        )
        .unwrap();
        let active: Vec<bool> = entries.iter().map(TeamEntry::is_active).collect();
        assert_eq!(active, [true, false, true, false]);
    }

    #[test]
    fn user_email_is_optional() {
        let user: UserResponse =
            serde_json::from_str(r#"{"id":"u1","name":"N","avatar":"","email":"a@example.com"}"#)
                .unwrap();
        let info = user.into_user_info();
        assert_eq!(info.email.as_deref(), Some("a@example.com"));
        assert_eq!(info.avatar_url, None);
        let user: UserResponse = serde_json::from_str(r#"{"id":"u1","name":"N"}"#).unwrap();
        assert_eq!(user.into_user_info().email, None);
    }

    #[test]
    fn expired_at_survives_the_detail_round_trip() {
        let detail =
            format!("GET /x -> HTTP 400 SUBSCRIPTION_EXPIRED {EXPIRED_AT_TAG}2026-09-01T00:00:00Z");
        assert_eq!(
            expired_at_from_detail(&detail).as_deref(),
            Some("2026-09-01T00:00:00Z")
        );
        assert_eq!(
            expired_at_from_detail("HTTP 400 SUBSCRIPTION_EXPIRED"),
            None
        );
        let problem: ApiProblem = serde_json::from_str(
            r#"{"code":"SUBSCRIPTION_EXPIRED","expires_at":"2026-09-01T00:00:00Z"}"#,
        )
        .unwrap();
        assert_eq!(problem.expired_at.as_deref(), Some("2026-09-01T00:00:00Z"));
        assert!(!is_timestamp("x y"));
    }

    #[test]
    fn problem_fields_are_read_at_the_root() {
        // The backend's problem+json: root-level fields, no `data` wrapper.
        let problem: ApiProblem = serde_json::from_str(
            r#"{"type":"about:blank","title":"Not Found","status":404,"detail":"no subscription","code":"SUBSCRIPTION_REQUIRED"}"#,
        )
        .unwrap();
        assert_eq!(problem.code.as_deref(), Some(PROBLEM_SUBSCRIPTION_REQUIRED));
        assert_eq!(problem.expired_at, None);
        assert_eq!(problem.detail.as_deref(), Some("no subscription"));
    }

    #[test]
    fn a_problem_detail_is_kept_as_one_short_line() {
        assert_eq!(
            display_detail("  no active\n subscription\t ").as_deref(),
            Some("no active subscription")
        );
        assert_eq!(display_detail("\u{1b}[31m").as_deref(), Some("[31m"));
        assert_eq!(display_detail(" \r\n "), None);
        let long = "x".repeat(MAX_PROBLEM_DETAIL_LEN + 50);
        assert_eq!(
            display_detail(&long).map(|d| d.chars().count()),
            Some(MAX_PROBLEM_DETAIL_LEN)
        );
    }

    #[test]
    fn credential_errors_are_classified() {
        let status = |status: u16, problem: Option<&str>| ApiError::Status {
            expired_at: None,
            detail: None,
            request: "GET /api/v1/users/me".to_string(),
            status: StatusCode::from_u16(status).unwrap(),
            code: problem.map(str::to_string),
        };
        assert!(status(401, None).is_terminal_credential());
        assert!(status(403, None).is_terminal_credential());
        assert!(!status(403, Some(PROBLEM_TEAM_DISABLED)).is_terminal_credential());
        assert!(status(403, Some(PROBLEM_TEAM_DISABLED)).is_team_disabled());
        assert!(!status(500, None).is_terminal_credential());
        assert!(status(503, None).is_transient());
        assert!(status(429, None).is_rate_limited());
    }

    #[test]
    fn subscription_problems_are_classified() {
        let problem = |status: u16, code: Option<&str>| {
            subscription_problem(StatusCode::from_u16(status).unwrap(), code)
        };
        use SubscriptionProblem::*;
        assert_eq!(
            problem(403, Some(PROBLEM_TEAM_DISABLED)),
            Some(TeamDisabled)
        );
        assert_eq!(
            problem(404, Some(PROBLEM_TEAM_DISSOLVED)),
            Some(TeamDisabled)
        );
        // The team codes count only with their own status.
        assert_eq!(problem(400, Some(PROBLEM_TEAM_DISABLED)), None);
        assert_eq!(problem(403, Some(PROBLEM_TEAM_DISSOLVED)), None);
        assert_eq!(problem(400, Some("SUBSCRIPTION_EXPIRED")), Some(Expired));
        for code in [
            "SUBSCRIPTION_NOT_FOUND",
            "SUBSCRIPTION_NOT_ACTIVE",
            "POINTS_NO_ACTIVE_SUBSCRIPTION",
        ] {
            assert_eq!(problem(409, Some(code)), Some(NoSubscription), "{code}");
        }
        assert_eq!(problem(402, None), Some(NoSubscription));
        // The proxy-profile 404 codes; older backends send `NOT_FOUND` or
        // no code for both, which stays unclassified.
        assert_eq!(
            problem(404, Some(PROBLEM_SUBSCRIPTION_REQUIRED)),
            Some(NoSubscription)
        );
        assert_eq!(
            problem(404, Some(PROBLEM_PROXY_NODES_NOT_READY)),
            Some(NodesNotReady)
        );
        assert_eq!(problem(404, Some("NOT_FOUND")), None);
        assert_eq!(problem(404, None), None);
        assert_eq!(problem(404, Some("SOMETHING_NEW")), None);
        assert_eq!(problem(401, None), None);
        assert_eq!(ApiError::Decode("x").subscription_problem(), None);
    }

    #[test]
    fn only_loopback_hosts_are_local() {
        for base in [
            "http://127.0.0.1:8080",
            "http://localhost",
            "http://[::1]:9",
        ] {
            assert!(is_loopback_base(base), "{base}");
        }
        for base in [
            "http://192.0.2.10",
            "https://api.example.com",
            "http://127.0.0.1.nip.io",
            "http://localhost.example.com",
            "not a url",
        ] {
            assert!(!is_loopback_base(base), "{base}");
        }
        // The switch never relaxes a remote backend.
        assert!(!api("http://api.example.com", true).trusts_local_backend());
        assert!(api("http://127.0.0.1:9", true).trusts_local_backend());
    }

    #[test]
    fn endpoint_keeps_base_prefix_and_requires_https() {
        let prefixed = api("https://api.example.com/prefix/", false);
        assert_eq!(
            prefixed.endpoint(PATH_USERS_ME).unwrap().as_str(),
            "https://api.example.com/prefix/api/v1/users/me"
        );
        assert!(api("http://api.example.com", false)
            .endpoint(PATH_USERS_ME)
            .is_err());
        assert!(api("http://api.example.com", true)
            .endpoint(PATH_USERS_ME)
            .is_err());
        // Under the switch plain http works for a loopback base in release
        // builds too; debug builds allow it anyway.
        assert!(api("http://127.0.0.1:8080", true)
            .endpoint(PATH_USERS_ME)
            .is_ok());
    }
}
