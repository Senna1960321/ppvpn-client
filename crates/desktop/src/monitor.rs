//! Connection monitor while signed in: which replica each core uses for the
//! current node and how far away that node is.
//!
//! - Replica: read from the cores' `GetStatus` (`selected_ingress`) every few
//!   seconds; cheap local IPC. Cores that do not report it leave `None`.
//! - Latency: one TCP entrance probe of the current node through the
//!   standard core every 60 s and right after the selection changes, plus any
//!   speed-test result for that node. A dedicated probe keeps the number
//!   fresh even when the user never runs a speed test, costs one handshake
//!   per replica per minute, and works with enhanced mode on because the TUN
//!   core excludes ingress addresses.
//! - Self-heal: if the standard core's selection drifted (e.g. a live
//!   `select-node` failed), the current node is selected again. A new or
//!   recreated core needs no repair: every apply carries the selection and
//!   the pins.
//! - Ingresses: each node's pin and ingress health from the same
//!   `GetStatus` (`nodes`), for `snapshot.node_ingresses`.
//! - Rule sets: the unavailable routing rule sets of the core in use (the
//!   enhanced core's while it is on, otherwise the standard core's), from
//!   the same `GetStatus` (`rule_sets`, core 0.5.0+).
//! - Replaced routes (macOS): another VPN's routes the enhanced core's TUN
//!   replaced for now (`replaced_routes`), while it is on.

use std::time::{Duration, Instant};

use crate::core_ipc;
use crate::session::ClientRef;
use crate::{Client, ConnectionDetail, ProbeMethod, ProbeResult};

const STATUS_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(30)
} else {
    Duration::from_secs(5)
};
const LATENCY_INTERVAL: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(60)
};
const LATENCY_TIMEOUT_MS: u64 = 3_000;

/// The stored latency is for `node` and younger than the refresh interval.
fn latency_fresh(
    stored: &Option<(String, Option<u32>, Instant)>,
    node: &str,
    now: Instant,
) -> bool {
    stored
        .as_ref()
        .is_some_and(|(id, _, at)| id == node && now.duration_since(*at) < LATENCY_INTERVAL)
}

impl Client {
    /// A speed-test result for the current node refreshes its latency.
    pub(crate) fn note_probe(&self, result: &ProbeResult) {
        if !matches!(result.method, ProbeMethod::Tcp | ProbeMethod::Icmp) {
            return;
        }
        if self.selected_node().as_deref() != Some(result.node_id.as_str()) {
            return;
        }
        let latency = result.success.then_some(result.latency_ms).flatten();
        self.session_state().latency = Some((result.node_id.clone(), latency, Instant::now()));
    }

    pub(crate) fn spawn_monitor(&self, session: u64) {
        let weak = self.this.clone();
        let task = self.runtime.spawn(async move {
            loop {
                tokio::time::sleep(STATUS_INTERVAL).await;
                let Some(client) = ClientRef::upgrade(&weak) else {
                    return;
                };
                if !client.session_is(session) || client.is_shut_down() {
                    return;
                }
                client.monitor_tick(session).await;
            }
        });
        let mut state = self.session_state();
        if state.is_session(session) {
            if let Some(previous) = state.monitor_task.replace(task) {
                previous.abort();
            }
        } else {
            task.abort();
        }
    }

    async fn monitor_tick(&self, session: u64) {
        let selected = self.selected_node();
        let standard = self.standard.transport().ok();

        // Standard core: replica, and re-select the current node if needed.
        let mut standard_detail = ConnectionDetail::default();
        let standard_status = match standard.as_ref() {
            Some(transport) => core_ipc::get_status(transport.as_ref()).await.ok(),
            None => None,
        };
        let standard_rule_sets = standard_status
            .as_ref()
            .map(|status| core_ipc::unavailable_rule_sets(&status.rule_sets))
            .unwrap_or_default();
        let standard_nodes = standard_status
            .as_ref()
            .and_then(|status| status.nodes.clone());
        if let (Some(status), Some(node), Some(transport)) =
            (standard_status, selected.as_deref(), standard.as_ref())
        {
            if status.selected_node_id.as_deref() == Some(node) {
                standard_detail = ConnectionDetail {
                    endpoint_key: status.selected_endpoint_key,
                    endpoint_label: status.selected_endpoint_label,
                    previous_endpoint_key: status.previous_endpoint_key,
                    latency_ms: None,
                };
            } else if status.is_serving() {
                // Status RPCs may finish after the user chose another node or
                // the standard core was replaced. Serialize only the repair
                // with explicit selection; probes never hold this lock.
                let _sync = self.cores.lock().await;
                let same_core = self
                    .standard
                    .transport()
                    .is_ok_and(|current| std::sync::Arc::ptr_eq(transport, &current));
                if self.session_is(session) && self.selected_node() == selected && same_core {
                    if let Err(error) = core_ipc::select_node(transport.as_ref(), node).await {
                        tracing::info!("standard core: select node failed ({})", error.detail());
                    }
                }
            }
        }

        // Latency of the current node.
        let mut latency = None;
        if let Some(node) = selected.as_deref() {
            let fresh = latency_fresh(&self.session_state().latency, node, Instant::now());
            if !fresh {
                if let Some(transport) = standard.as_ref() {
                    let ids = [node.to_string()];
                    let measured = core_ipc::probe_entrances(
                        transport.as_ref(),
                        "tcp",
                        &ids,
                        LATENCY_TIMEOUT_MS,
                        4,
                    )
                    .await
                    .ok()
                    .and_then(|results| results.into_iter().next())
                    .map(|result| core_ipc::entrance_to_probe(&result, ProbeMethod::Tcp, node));
                    if let Some(result) = measured {
                        let value = result.success.then_some(result.latency_ms).flatten();
                        self.session_state().latency =
                            Some((node.to_string(), value, Instant::now()));
                    }
                }
            }
            latency = self
                .session_state()
                .latency
                .as_ref()
                .filter(|(id, _, _)| id == node)
                .and_then(|(_, value, _)| *value);
        }

        // The path in use: the enhanced core's while it is on, otherwise the
        // standard core's (system proxy / local proxy).
        let enhanced_status = match &self.tunnel {
            Some(tunnel) => tunnel.core_status().await,
            None => self.enhanced.core_status().await,
        };
        let rule_sets_unavailable = match enhanced_status.as_ref() {
            Some(status) => core_ipc::unavailable_rule_sets(&status.rule_sets),
            None => standard_rule_sets,
        };
        let enhanced_nodes = enhanced_status
            .as_ref()
            .and_then(|status| status.nodes.clone());
        let replaced_routes = enhanced_status
            .as_ref()
            .map(|status| status.replaced_routes.clone())
            .unwrap_or_default();
        let node_ingresses = match enhanced_status.as_ref() {
            Some(_) => enhanced_nodes,
            None => standard_nodes,
        }
        .unwrap_or_default();
        let detail = match enhanced_status {
            Some(status) if status.selected_node_id == selected => ConnectionDetail {
                endpoint_key: status.selected_endpoint_key,
                endpoint_label: status.selected_endpoint_label,
                previous_endpoint_key: status.previous_endpoint_key,
                latency_ms: latency,
            },
            Some(_) => ConnectionDetail {
                latency_ms: latency,
                ..ConnectionDetail::default()
            },
            None => ConnectionDetail {
                latency_ms: standard.as_ref().and(latency),
                ..standard_detail
            },
        };
        if !self.session_is(session) || self.selected_node() != selected {
            return;
        }
        self.set_connection_detail(detail);
        self.set_rule_sets_unavailable(rule_sets_unavailable);
        self.set_node_ingresses(node_ingresses);
        self.set_replaced_routes(replaced_routes);
        // Compatible mode: re-enable the endpoint after a core restart.
        self.compat.reconcile().await;
    }
}

impl Client {
    fn set_rule_sets_unavailable(&self, ids: Vec<String>) {
        let changed = self
            .snapshot
            .lock()
            .map(|snapshot| snapshot.rule_sets_unavailable != ids)
            .unwrap_or(false);
        if changed {
            self.update(|snapshot| snapshot.rule_sets_unavailable = ids);
        }
    }

    fn set_replaced_routes(&self, routes: Vec<String>) {
        let changed = self
            .snapshot
            .lock()
            .map(|snapshot| snapshot.replaced_routes != routes)
            .unwrap_or(false);
        if changed {
            self.update(|snapshot| snapshot.replaced_routes = routes);
        }
    }

    /// A Core API event (`WatchEvents` envelope) of the core in use. Only
    /// `RuleSetChanged` affects the snapshot, until the next status poll
    /// confirms it. The client polls `GetStatus` today; this is the hook for
    /// an event stream.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn on_core_event(&self, event: &serde_json::Value) {
        let mut ids = self
            .snapshot
            .lock()
            .map(|snapshot| snapshot.rule_sets_unavailable.clone())
            .unwrap_or_default();
        if core_ipc::apply_rule_set_event(&mut ids, event) {
            self.update(|snapshot| snapshot.rule_sets_unavailable = ids);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_is_fresh_only_for_the_same_node_within_the_interval() {
        let now = Instant::now();
        let stored = Some(("a".to_string(), Some(40), now));
        assert!(latency_fresh(&stored, "a", now));
        assert!(!latency_fresh(&stored, "b", now));
        assert!(!latency_fresh(&stored, "a", now + LATENCY_INTERVAL));
        assert!(!latency_fresh(&None, "a", now));
    }
}

#[cfg(test)]
mod selection_tests;
