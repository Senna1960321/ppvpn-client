//! Path-routed mock backend for tests that need several endpoints at once:
//! sign-in restore, account, messages, device registration, the push queue
//! and the health route. Keeps request lines (with query) for assertions.

// Shared by several test modules; not every helper is used by each.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::auth::test_support::token_set;

pub(crate) const PUSH_TOKEN: &str = "ppd_test_token";

#[derive(Default)]
pub(crate) struct Backend {
    pub(crate) messages: Mutex<Vec<Value>>,
    /// Answer every message endpoint with 403 (old token scope).
    pub(crate) forbidden: Mutex<bool>,
    pub(crate) requests: Mutex<Vec<String>>,
    pub(crate) unread: Mutex<u64>,
    /// Push queue: items not yet acked are delivered.
    pub(crate) push_items: Mutex<Vec<Value>>,
    pub(crate) acked: Mutex<Vec<u64>>,
    pub(crate) ack_delay: Mutex<Option<std::time::Duration>>,
    /// The push token the queue accepts (401 otherwise).
    pub(crate) push_token: Mutex<String>,
    /// Status for `POST /devices/register` (e.g. 403 before the scope).
    pub(crate) register_status: Mutex<Option<&'static str>>,
    /// Bodies of `POST /devices/register` and `/devices/unregister`.
    pub(crate) device_bodies: Mutex<Vec<Value>>,
    /// Request heads (lower-cased) of `GET /api/v1/health`.
    pub(crate) health_heads: Mutex<Vec<String>>,
    /// Body of `GET /api/v1/me/proxy-profile`; none answers 404
    /// `SUBSCRIPTION_NOT_FOUND`.
    pub(crate) profile: Mutex<Option<String>>,
}

impl Backend {
    pub(crate) fn new() -> Arc<Self> {
        let backend = Self::default();
        *backend.push_token.lock().unwrap() = PUSH_TOKEN.to_string();
        Arc::new(backend)
    }

    pub(crate) fn add_message(&self, id: u64) {
        self.messages.lock().unwrap().push(json!({
            "id": id, "user_id": 7, "title": format!("t{id}"), "content": "c",
            "type": "subscription", "event_key": "subscription.expire_reminder_3d",
            "severity": "important", "deep_link": "/app/subscriptions/88",
            "push": true, "is_read": false, "created_at": "2026-09-29T00:00:00Z",
            "unknown_field": 1
        }));
    }

    pub(crate) fn add_push(&self, id: u64, created_at: &str) {
        self.push_items.lock().unwrap().push(json!({
            "id": id, "title": format!("p{id}"), "body": "b",
            "deep_link": "/app/subscriptions/88", "severity": "critical",
            "event_key": "invoice.issued", "created_at": created_at
        }));
    }

    /// A push about inbox message `message_id` (`add_push` has none, like a
    /// broadcast).
    pub(crate) fn add_push_for_message(&self, id: u64, created_at: &str, message_id: u64) {
        self.add_push(id, created_at);
        let mut items = self.push_items.lock().unwrap();
        if let Some(item) = items.last_mut() {
            item["message_id"] = json!(message_id);
        }
    }

    pub(crate) fn requests_matching(&self, needle: &str) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|line| line.contains(needle))
            .cloned()
            .collect()
    }

    fn pending_push(&self, after: u64) -> Vec<Value> {
        let acked = self.acked.lock().unwrap().clone();
        self.push_items
            .lock()
            .unwrap()
            .iter()
            .filter(|item| {
                let id = item["id"].as_u64().unwrap();
                id > after && !acked.contains(&id)
            })
            .cloned()
            .collect()
    }

    fn route(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &str,
        body: &str,
    ) -> (&'static str, String) {
        let param = |name: &str| {
            query
                .split('&')
                .find_map(|pair| pair.strip_prefix(&format!("{name}=")))
                .and_then(|value| value.parse::<u64>().ok())
        };
        if path.starts_with("/api/v1/messages") && *self.forbidden.lock().unwrap() {
            return ("403 Forbidden", r#"{"code":"403001"}"#.into());
        }
        if path.starts_with("/api/v1/push/") {
            let token = self.push_token.lock().unwrap().to_ascii_lowercase();
            if headers.contains("x-push-token: ppd_forbidden") {
                return ("403 Forbidden", r#"{"code":"PUSH_TOKEN_REVOKED"}"#.into());
            }
            if !headers.contains(&format!("x-push-token: {token}")) {
                return (
                    "401 Unauthorized",
                    r#"{"code":"PUSH_TOKEN_INVALID"}"#.into(),
                );
            }
        }
        match (method, path) {
            // The backend's unauthenticated health route (200, no-store).
            ("GET", "/api/v1/health") => {
                self.health_heads.lock().unwrap().push(headers.to_string());
                ("200 OK", r#"{"status":"ok"}"#.into())
            }
            ("POST", "/api/v1/auth/device/refresh") => {
                ("200 OK", r#"{"refresh_token":"jyr_prepared"}"#.into())
            }
            ("POST", "/api/v1/auth/device/refresh/commit") => ("200 OK", token_set("jyr_a")),
            ("POST", "/api/v1/auth/device/revoke") => ("200 OK", r#"{"revoked":true}"#.into()),
            ("GET", "/api/v1/users/me") => {
                ("200 OK", r#"{"id":"u7","name":"N","avatar":""}"#.into())
            }
            ("GET", "/api/v1/me/teams") => (
                "200 OK",
                r#"{"items":[{"id":"t1","name":"P","is_personal":true,"is_default":true}]}"#.into(),
            ),
            ("GET", "/api/v1/me/proxy-profile") => match self.profile.lock().unwrap().clone() {
                Some(profile) => ("200 OK", profile),
                None => (
                    "404 Not Found",
                    r#"{"code":"SUBSCRIPTION_NOT_FOUND"}"#.into(),
                ),
            },
            ("GET", "/api/v1/messages/unread-count") => (
                "200 OK",
                json!({"count": *self.unread.lock().unwrap()}).to_string(),
            ),
            ("GET", "/api/v1/messages") => {
                let mut all = self.messages.lock().unwrap().clone();
                all.reverse();
                let size = param("page_size").unwrap_or(20) as usize;
                let total = all.len();
                let items: Vec<Value> = all.into_iter().take(size).collect();
                (
                    "200 OK",
                    json!({"items": items, "total": total, "page": 1, "page_size": size})
                        .to_string(),
                )
            }
            ("PUT", path) if path.starts_with("/api/v1/messages/") => {
                ("200 OK", r#"{"updated":true}"#.into())
            }
            ("POST", "/api/v1/devices/register") => {
                if let Some(status) = *self.register_status.lock().unwrap() {
                    return (
                        status,
                        r#"{"code":"DEVICE_DESKTOP_SESSION_REQUIRED"}"#.into(),
                    );
                }
                let request: Value = serde_json::from_str(body).unwrap_or_default();
                self.device_bodies.lock().unwrap().push(request.clone());
                let desktop = request["platform"]
                    .as_str()
                    .is_some_and(|platform| platform.starts_with("desktop-"));
                if desktop {
                    return (
                        "200 OK",
                        json!({"id": 42, "platform": request["platform"],
                               "push_token": *self.push_token.lock().unwrap()})
                        .to_string(),
                    );
                }
                if request["token"].as_str().unwrap_or_default().is_empty() {
                    return (
                        "400 Bad Request",
                        r#"{"code":"DEVICE_TOKEN_REQUIRED"}"#.into(),
                    );
                }
                (
                    "200 OK",
                    json!({"id": 43, "platform": request["platform"], "token_hash_prefix": "0a1b"})
                        .to_string(),
                )
            }
            ("POST", "/api/v1/devices/unregister") => {
                let request: Value = serde_json::from_str(body).unwrap_or_default();
                self.device_bodies.lock().unwrap().push(request);
                ("200 OK", r#"{"message":"device unregistered"}"#.into())
            }
            ("DELETE", "/api/v1/devices/42") => ("204 No Content", String::new()),
            ("GET", "/api/v1/push/pull") => {
                let after = param("after").unwrap_or(0);
                let wait = param("wait").unwrap_or(25);
                if wait > 25 {
                    return ("422 Unprocessable Entity", "{}".into());
                }
                let deadline = Instant::now() + Duration::from_secs(wait);
                loop {
                    let items = self.pending_push(after);
                    if !items.is_empty() || Instant::now() >= deadline {
                        let items: Vec<Value> = items.into_iter().take(50).collect();
                        let cursor = items
                            .iter()
                            .filter_map(|item| item["id"].as_u64())
                            .max()
                            .unwrap_or(after);
                        return (
                            "200 OK",
                            json!({"items": items, "cursor": cursor}).to_string(),
                        );
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            ("POST", "/api/v1/push/ack") => {
                if let Some(delay) = *self.ack_delay.lock().unwrap() {
                    std::thread::sleep(delay);
                }
                let request: Value = serde_json::from_str(body).unwrap_or_default();
                let ids: Vec<u64> = request["ids"]
                    .as_array()
                    .map(|ids| ids.iter().filter_map(Value::as_u64).collect())
                    .unwrap_or_default();
                self.acked.lock().unwrap().extend(&ids);
                ("200 OK", json!({"acked": ids.len()}).to_string())
            }
            _ => ("404 Not Found", r#"{"code":"NOT_FOUND"}"#.into()),
        }
    }
}

/// Serves `backend` on a local port; returns the base URL.
pub(crate) fn serve(backend: Arc<Backend>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for socket in listener.incoming() {
            let Ok(mut socket) = socket else { return };
            let backend = backend.clone();
            std::thread::spawn(move || {
                let mut request = Vec::new();
                let mut chunk = [0u8; 4096];
                let (head, body) = loop {
                    let Ok(read) = socket.read(&mut chunk) else {
                        return;
                    };
                    if read == 0 {
                        return;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let head = String::from_utf8_lossy(&request[..end]).into_owned();
                    let length = head
                        .to_ascii_lowercase()
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        let body = String::from_utf8_lossy(&request[end + 4..end + 4 + length])
                            .into_owned();
                        break (head, body);
                    }
                };
                let line = head.lines().next().unwrap_or_default().to_string();
                let mut parts = line.split(' ');
                let method = parts.next().unwrap_or_default().to_string();
                let target = parts.next().unwrap_or_default().to_string();
                backend
                    .requests
                    .lock()
                    .unwrap()
                    .push(format!("{method} {target}"));
                let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                let (status, response_body) =
                    backend.route(&method, path, query, &head.to_ascii_lowercase(), &body);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                let _ = socket.write_all(response.as_bytes());
            });
        }
    });
    base
}

pub(crate) fn temp_dir(prefix: &str) -> String {
    std::env::temp_dir()
        .join(format!("{prefix}-{}", uuid::Uuid::new_v4()))
        .to_string_lossy()
        .into_owned()
}

pub(crate) fn wait_until(what: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !what() {
        assert!(Instant::now() < deadline, "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}
