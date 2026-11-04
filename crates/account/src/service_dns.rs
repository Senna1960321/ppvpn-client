//! API-only DNS recovery (#521 step B), off until a user-requested report
//! proves that system answers fail but an alternate address verifies TLS.
//! No address is cached: every new connection resolves again, preserving
//! DNS entry switching. HTTP, SNI and certificate checks keep the API host.

use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::reachability::{self, Mode, Report, Target, DOH_RESOLVERS, SYSTEM};

type Lookup = Pin<Box<dyn Future<Output = Vec<IpAddr>> + Send>>;
const RECOVERY_TIMEOUT: Duration = Duration::from_secs(15);
const ADDRESSES_PER_LOOKUP: usize = 8;

pub(crate) struct RecoveryDns {
    target: Target,
    enabled: AtomicBool,
    #[cfg(test)]
    test_inputs: Option<tests::Inputs>,
}

impl RecoveryDns {
    pub(crate) fn new(base: &str) -> Option<Self> {
        let target = reachability::api_target(base)?;
        if target.tls.is_none()
            || target.host.parse::<IpAddr>().is_ok()
            || crate::api::is_loopback_base(base)
        {
            return None;
        }
        Some(Self {
            target,
            enabled: AtomicBool::new(false),
            #[cfg(test)]
            test_inputs: None,
        })
    }

    pub(crate) fn enable_from(&self, report: &Report) -> bool {
        let enabled = proves_alternate(report, &self.target);
        self.enabled.store(enabled, Ordering::Relaxed);
        enabled
    }
}

/// Do not infer poisoning from different DNS answers alone, an unchecked
/// address, a TCP connection, or a probe already carried through the VPN.
fn proves_alternate(report: &Report, target: &Target) -> bool {
    if report.mode != Mode::Compare || report.vpn_on {
        return false;
    }
    let Some(host) = report
        .hosts
        .iter()
        .find(|h| h.label == "api" && h.host == target.host && h.port == target.port)
    else {
        return false;
    };
    let Some(system) = host.lookups.iter().find(|l| l.resolver == SYSTEM) else {
        return false;
    };
    let verified = |check: &reachability::AddressCheck| {
        check.tcp.ok
            && check
                .tls
                .as_ref()
                .is_some_and(|s| s.ok && s.error.is_none())
    };
    let all_system_failed = system.addresses.iter().all(|ip| {
        host.addresses
            .iter()
            .any(|a| a.address == *ip && !verified(a))
    });
    all_system_failed
        && host.addresses.iter().any(|a| {
            !system.addresses.contains(&a.address)
                && verified(a)
                && a.from
                    .iter()
                    .any(|from| DOH_RESOLVERS.iter().any(|r| r.name == from))
        })
}

impl Resolve for RecoveryDns {
    fn resolve(&self, name: Name) -> Resolving {
        let host = name.as_str().to_string();
        if !self.enabled.load(Ordering::Relaxed) || !host.eq_ignore_ascii_case(&self.target.host) {
            // Includes a configured proxy's host: never send it to DoH or
            // probe it with the API's certificate or port.
            return Box::pin(async move {
                let addresses: Vec<_> =
                    tokio::net::lookup_host((host.as_str(), 0)).await?.collect();
                Ok(Box::new(addresses.into_iter()) as Addrs)
            });
        }
        let target = self.target.clone();
        let (lookups, tls) = self.inputs();
        Box::pin(async move {
            let address =
                tokio::time::timeout(RECOVERY_TIMEOUT, first_verified(target, tls, lookups))
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::TimedOut, "API DNS recovery timed out")
                    })?
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::NotFound,
                            "API DNS recovery found no address with verified TLS",
                        )
                    })?;
            Ok(Box::new(std::iter::once(address)) as Addrs)
        })
    }
}

impl RecoveryDns {
    fn inputs(&self) -> (Vec<Lookup>, Result<Arc<rustls::ClientConfig>, String>) {
        #[cfg(test)]
        if let Some(inputs) = &self.test_inputs {
            return ((inputs.lookups)(), Ok(inputs.tls.clone()));
        }
        let mut lookups: Vec<Lookup> = Vec::new();
        let (host, port) = (self.target.host.clone(), self.target.port);
        lookups.push(Box::pin(async move {
            reachability::system_lookup(&host, port).await.addresses
        }));
        if let Some(client) = reachability::doh_client() {
            for resolver in DOH_RESOLVERS {
                let (client, host) = (client.clone(), self.target.host.clone());
                lookups.push(Box::pin(async move {
                    reachability::doh_lookup(&client, resolver, &host)
                        .await
                        .addresses
                }));
            }
        }
        (lookups, reachability::tls_config().map(Arc::new))
    }
}

#[cfg(test)]
mod tests;

/// Race lookups and their TLS probes, not HTTP requests: even a POST is
/// sent only once. Dropping the sets cancels all losing probes.
async fn first_verified(
    target: Target,
    tls: Result<Arc<rustls::ClientConfig>, String>,
    lookups: Vec<Lookup>,
) -> Option<SocketAddr> {
    let mut queries = tokio::task::JoinSet::new();
    let mut probes = tokio::task::JoinSet::new();
    let mut seen = HashSet::new();
    for lookup in lookups {
        queries.spawn(lookup);
    }
    loop {
        tokio::select! {
            found = queries.join_next(), if !queries.is_empty() => {
                if let Some(Ok(addresses)) = found {
                    for ip in addresses.into_iter().take(ADDRESSES_PER_LOOKUP) {
                        if !seen.insert(ip) { continue; }
                        let (target, tls) = (target.clone(), tls.clone());
                        probes.spawn(async move {
                            reachability::check_address(ip, Vec::new(), &target, tls).await
                        });
                    }
                }
            }
            checked = probes.join_next(), if !probes.is_empty() => {
                if let Some(Ok(check)) = checked {
                    if check.tcp.ok && check.tls.is_some_and(|s| s.ok && s.error.is_none()) {
                        return Some(SocketAddr::new(check.address, target.port));
                    }
                }
            }
            else => return None,
        }
    }
}
