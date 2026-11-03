//! The inbox: backend messages listed on demand, marked read/unread, and the
//! unread badge in [`crate::ClientSnapshot::unread_notifications`], polled
//! while signed in.
//!
//! OS pop-ups are not shown from here: the backend pushes them to the
//! separate push agent ([`crate::PushAgent`]). Failures of the badge poll
//! (network, a token that still lacks the message scopes) are logged, never
//! shown.

use std::sync::atomic::Ordering;
use std::time::Duration;

use crate::api::ApiErrorExt;
use crate::api::{ApiError, Message};
use crate::errors::{ClientError, ErrorCode};
use crate::session::ClientRef;
use crate::{Client, InboxMessage, InboxPage, MessageCategory, MessageSeverity};

const POLL_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(50)
} else {
    Duration::from_secs(60)
};

/// Badge poll of the signed-in session (reset with it).
#[derive(Default)]
pub(crate) struct NotificationState {
    pub(crate) task: Option<tokio::task::JoinHandle<()>>,
}

impl NotificationState {
    pub(crate) fn reset(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

// ---------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------

pub(crate) fn severity(value: &str) -> MessageSeverity {
    match value {
        "critical" => MessageSeverity::Critical,
        "important" => MessageSeverity::Important,
        "normal" => MessageSeverity::Normal,
        _ => MessageSeverity::Unspecified,
    }
}

/// Category from the backend message type and event key (first match wins).
pub(crate) fn category(kind: &str, event_key: &str) -> MessageCategory {
    match (kind, event_key) {
        (_, key) if key.starts_with("subscription.expire_reminder") => {
            MessageCategory::SubscriptionExpiring
        }
        (_, "subscription.expired" | "subscription.suspended" | "subscription.past_due") => {
            MessageCategory::SubscriptionExpired
        }
        ("invoice" | "wallet", _) | (_, "proxy.auto_renew_failed") => MessageCategory::Billing,
        ("order", _) => MessageCategory::Order,
        ("proxy", _) => MessageCategory::Route,
        ("broadcast" | "campaign" | "marketing", _) => MessageCategory::Announcement,
        (_, key) if key.starts_with("broadcast:") => MessageCategory::Announcement,
        _ => MessageCategory::Other,
    }
}

/// `deep_link` resolved against the site root of `api_base`; absolute
/// http(s) links are kept, anything else (or empty) is `None`.
pub(crate) fn absolute_link(api_base: &str, link: &str) -> Option<String> {
    let link = link.trim();
    if link.is_empty() {
        return None;
    }
    let mut base = reqwest::Url::parse(api_base.trim()).ok()?;
    base.set_path("/");
    base.set_query(None);
    base.set_fragment(None);
    let url = base.join(link).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
}

pub(crate) fn to_notification(message: Message, api_base: &str) -> InboxMessage {
    InboxMessage {
        id: message.id,
        title: message.title,
        content: message.content,
        category: category(&message.kind, &message.event_key),
        kind: message.kind,
        event_key: message.event_key,
        severity: severity(&message.severity),
        deep_link: absolute_link(api_base, &message.deep_link),
        push: message.push,
        read: message.is_read,
        created_at: message.created_at,
    }
}

/// A 403: the (old) token lacks the message scopes; quiet until it refreshes.
fn is_forbidden(error: &ClientError) -> bool {
    matches!(
        error,
        ClientError::Failed { code, detail }
            if matches!(code, ErrorCode::RequestRejected | ErrorCode::TeamDisabled)
                && detail.contains("-> HTTP 403")
    )
}

#[uniffi::export]
impl Client {
    /// Phones only: stop background unread polling and refresh on resume.
    pub fn set_notification_foreground(&self, foreground: bool) {
        if !matches!(self.config.platform.as_str(), "ios" | "android") {
            return;
        }
        {
            // Serialize the flag and cancellation with sign-in's task install.
            let mut state = self.session_state();
            if self
                .notification_foreground
                .swap(foreground, Ordering::SeqCst)
                == foreground
            {
                return;
            }
            if !foreground {
                state.notifications.reset();
            }
        }
        if foreground {
            if let Ok(session) = self.current_session() {
                self.spawn_notification_loop(session);
            }
        }
    }
}

impl Client {
    /// Starts the unread-count poll of `session` (replacing any previous one).
    pub(crate) fn spawn_notification_loop(&self, session: u64) {
        let weak = self.this.clone();
        let task = self.runtime.spawn(async move {
            let mut delay = Duration::ZERO;
            loop {
                tokio::time::sleep(delay).await;
                delay = POLL_INTERVAL;
                let Some(client) = ClientRef::upgrade(&weak) else {
                    return;
                };
                if !client.session_is(session)
                    || client.is_shut_down()
                    || !client.notification_foreground.load(Ordering::SeqCst)
                {
                    return;
                }
                match client.refresh_unread(session).await {
                    Ok(()) => {}
                    Err(ClientError::NotSignedIn) => return,
                    Err(error) if is_forbidden(&error) => {
                        tracing::info!("unread count not permitted yet (token scope): {error}");
                    }
                    Err(error) => tracing::info!("unread count poll failed: {error}"),
                }
            }
        });
        let mut state = self.session_state();
        if state.is_session(session) && self.notification_foreground.load(Ordering::SeqCst) {
            if let Some(previous) = state.notifications.task.replace(task) {
                previous.abort();
            }
        } else {
            task.abort();
        }
    }

    async fn refresh_unread(&self, session: u64) -> Result<(), ClientError> {
        let count = self
            .bearer(
                session,
                |token| async move { self.auth.api().unread_count(&token).await },
                ApiError::into_client_error,
            )
            .await?;
        let count = u32::try_from(count).unwrap_or(u32::MAX);
        self.update_session(session, |_, snapshot| {
            snapshot.unread_notifications = count;
            true
        });
        Ok(())
    }

    // --- user actions ------------------------------------------------------------

    pub(crate) async fn list_notifications(
        &self,
        page: u32,
        page_size: u32,
    ) -> Result<InboxPage, ClientError> {
        let session = self.current_session()?;
        let (page, page_size) = (page.max(1), page_size.clamp(1, 100));
        let listed = self
            .bearer(
                session,
                |token| async move { self.auth.api().messages(&token, page, page_size).await },
                ApiError::into_client_error,
            )
            .await
            .map_err(|error| self.report(error))?;
        let _ = self.refresh_unread(session).await;
        Ok(InboxPage {
            items: listed
                .items
                .into_iter()
                .map(|message| to_notification(message, &self.config.api_base))
                .collect(),
            total: u32::try_from(listed.total).unwrap_or(u32::MAX),
        })
    }

    pub(crate) async fn read_notification(&self, id: u64) -> Result<(), ClientError> {
        let session = self.current_session()?;
        self.bearer(
            session,
            |token| async move { self.auth.api().mark_message_read(&token, id).await },
            ApiError::into_client_error,
        )
        .await
        .map_err(|error| self.report(error))?;
        let _ = self.refresh_unread(session).await;
        Ok(())
    }

    pub(crate) async fn unread_notification(&self, id: u64) -> Result<(), ClientError> {
        let session = self.current_session()?;
        self.bearer(
            session,
            |token| async move { self.auth.api().mark_message_unread(&token, id).await },
            ApiError::into_client_error,
        )
        .await
        .map_err(|error| self.report(error))?;
        let _ = self.refresh_unread(session).await;
        Ok(())
    }

    /// Marks every message up to the newest one as read.
    pub(crate) async fn read_all_notifications(&self) -> Result<(), ClientError> {
        let session = self.current_session()?;
        let newest = self
            .bearer(
                session,
                |token| async move { self.auth.api().messages(&token, 1, 1).await },
                ApiError::into_client_error,
            )
            .await
            .map_err(|error| self.report(error))?
            .items
            .first()
            .map(|message| message.id);
        if let Some(up_to) = newest {
            self.bearer(
                session,
                |token| async move { self.auth.api().mark_all_messages_read(&token, up_to).await },
                ApiError::into_client_error,
            )
            .await
            .map_err(|error| self.report(error))?;
        }
        let _ = self.refresh_unread(session).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::auth::test_support::MemoryPlatform;
    use crate::cores::test_support::{FakeLauncher, NoService};
    use crate::test_backend::{serve, temp_dir, wait_until, Backend};
    use crate::{
        AuthState, ClientConfig, ClientListener, ClientSnapshot, ProbeResult, TrafficSample,
    };

    #[derive(Default)]
    struct Recorder;

    impl ClientListener for Recorder {
        fn on_snapshot(&self, _: ClientSnapshot) {}
        fn on_probe_result(&self, _: ProbeResult) {}
        fn on_traffic(&self, _: TrafficSample) {}
    }

    fn client(base: &str, data_dir: &str) -> Arc<Client> {
        client_for_platform(base, data_dir, "macos")
    }

    fn client_for_platform(base: &str, data_dir: &str, platform: &str) -> Arc<Client> {
        Client::with_parts(
            ClientConfig {
                api_base: base.to_string(),
                data_dir: data_dir.to_string(),
                log_dir: data_dir.to_string(),
                platform: platform.into(),
                app_version: "0.0.0-test".into(),
            },
            MemoryPlatform::with_blob(r#"{"version":1,"active_refresh":"jyr_old"}"#),
            Arc::new(Recorder),
            Arc::new(NoService),
            FakeLauncher::new(),
            Arc::new(crate::sysproxy::tests::FakeWriter::default()),
        )
    }

    fn block_on<T>(future: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(future)
    }

    #[test]
    fn phone_badge_poll_stops_in_background_and_refreshes_on_resume() {
        for platform in ["ios", "android"] {
            let backend = Backend::new();
            *backend.unread.lock().unwrap() = 3;
            let base = serve(backend.clone());
            let dir = temp_dir("ppvpn-phone-badge-test");
            let client = client_for_platform(&base, &dir, platform);
            wait_until(|| client.snapshot().unread_notifications == 3);
            client.set_notification_foreground(false);
            assert!(client.session_state().notifications.task.is_none());
            // A poll already in flight may have reached the server; let it
            // drain before proving no subsequent request is made.
            std::thread::sleep(POLL_INTERVAL * 2);
            let polls = backend.requests_matching("/messages/unread-count").len();
            *backend.unread.lock().unwrap() = 7;
            std::thread::sleep(POLL_INTERVAL * 3);
            assert_eq!(
                backend.requests_matching("/messages/unread-count").len(),
                polls
            );
            assert_eq!(client.snapshot().unread_notifications, 3);
            // Sign-in cannot restart a background phone's poll either.
            client.spawn_notification_loop(client.current_session().unwrap());
            assert!(client.session_state().notifications.task.is_none());
            client.set_notification_foreground(true);
            wait_until(|| client.snapshot().unread_notifications == 7);
            block_on(client.logout()).unwrap();
            client.set_notification_foreground(false);
            client.set_notification_foreground(true);
            assert!(client.session_state().notifications.task.is_none());
            client.shutdown();
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn desktop_badge_poll_does_not_follow_mobile_lifecycle() {
        let backend = Backend::new();
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-desktop-badge-test");
        let client = client(&base, &dir);
        wait_until(|| matches!(client.snapshot().auth, AuthState::SignedIn));
        client.set_notification_foreground(false);
        *backend.unread.lock().unwrap() = 9;
        wait_until(|| client.snapshot().unread_notifications == 9);
        client.shutdown();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn badge_list_and_read_state() {
        let backend = Backend::new();
        for id in 5..=8 {
            backend.add_message(id);
        }
        *backend.unread.lock().unwrap() = 3;
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-inbox-test");
        let client = client(&base, &dir);

        wait_until(|| client.snapshot().unread_notifications == 3);
        assert!(matches!(client.snapshot().auth, AuthState::SignedIn));
        // The badge is polled; updates are never followed (no pop-ups here).
        *backend.unread.lock().unwrap() = 4;
        wait_until(|| client.snapshot().unread_notifications == 4);
        assert!(backend.requests_matching("/messages/updates").is_empty());

        let page = block_on(client.notifications(1, 2)).unwrap();
        assert_eq!(page.total, 4);
        assert_eq!(
            page.items.iter().map(|n| n.id).collect::<Vec<_>>(),
            vec![8, 7]
        );
        let first = &page.items[0];
        assert_eq!(first.severity, MessageSeverity::Important);
        assert_eq!(first.category, MessageCategory::SubscriptionExpiring);
        assert_eq!(
            first.deep_link.as_deref(),
            Some(format!("{base}/app/subscriptions/88").as_str())
        );

        block_on(client.mark_notification_unread(6)).unwrap();
        assert_eq!(
            backend.requests_matching("/6/unread"),
            vec!["PUT /api/v1/messages/6/unread"]
        );
        block_on(client.mark_notification_read(6)).unwrap();
        assert_eq!(
            backend.requests_matching("/6/read"),
            vec!["PUT /api/v1/messages/6/read"]
        );
        // read-all goes up to the newest message.
        block_on(client.mark_all_notifications_read()).unwrap();
        assert_eq!(
            backend.requests_matching("read-all"),
            vec!["PUT /api/v1/messages/read-all?up_to_id=8"]
        );
        assert!(client.snapshot().last_error.is_none());

        block_on(client.logout()).unwrap();
        assert_eq!(client.snapshot().unread_notifications, 0);
        client.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn forbidden_badge_poll_is_quiet() {
        let backend = Backend::new();
        *backend.forbidden.lock().unwrap() = true;
        let base = serve(backend.clone());
        let dir = temp_dir("ppvpn-inbox-test");
        let client = client(&base, &dir);
        wait_until(|| backend.requests_matching("/unread-count").len() >= 3);
        let snapshot = client.snapshot();
        assert!(matches!(snapshot.auth, AuthState::SignedIn));
        assert!(snapshot.last_error.is_none(), "{:?}", snapshot.last_error);
        std::thread::sleep(Duration::from_millis(10));
        client.shutdown();
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn categories_follow_type_and_event_key() {
        use MessageCategory::*;
        for (kind, event_key, expected) in [
            (
                "subscription",
                "subscription.expire_reminder_3d",
                SubscriptionExpiring,
            ),
            (
                "subscription",
                "subscription.expire_reminder_1d",
                SubscriptionExpiring,
            ),
            ("subscription", "subscription.expired", SubscriptionExpired),
            (
                "subscription",
                "subscription.suspended",
                SubscriptionExpired,
            ),
            ("subscription", "subscription.past_due", SubscriptionExpired),
            ("subscription", "subscription.renewed", Other),
            ("invoice", "invoice.issued", Billing),
            ("wallet", "wallet.topup_succeeded", Billing),
            ("proxy", "proxy.auto_renew_failed", Billing),
            ("order", "order.paid", Order),
            ("proxy", "proxy.chain_unhealthy", Route),
            ("broadcast", "broadcast:42", Announcement),
            ("campaign", "", Announcement),
            ("marketing", "", Announcement),
            ("", "broadcast:7", Announcement),
            ("ticket", "ticket.replied", Other),
            ("identity", "identity.verified", Other),
            ("", "", Other),
        ] {
            assert_eq!(category(kind, event_key), expected, "{kind} / {event_key}");
        }
    }

    #[test]
    fn deep_links_become_absolute_against_the_site_root() {
        let base = "https://www.example.com/api/prefix/";
        assert_eq!(
            absolute_link(base, "/app/subscriptions/88").as_deref(),
            Some("https://www.example.com/app/subscriptions/88")
        );
        assert_eq!(
            absolute_link(base, "https://example.com/x").as_deref(),
            Some("https://example.com/x")
        );
        assert_eq!(absolute_link(base, "  "), None);
        assert_eq!(absolute_link(base, "javascript:alert(1)"), None);
        assert_eq!(severity(""), MessageSeverity::Unspecified);
        assert_eq!(severity("critical"), MessageSeverity::Critical);
    }
}
