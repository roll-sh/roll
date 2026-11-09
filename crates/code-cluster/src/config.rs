//! Cluster configuration and its timing invariants.

use crate::{Error, Result};
use std::time::Duration;

/// How a node takes part in the cluster. [`ClusterConfig::new`] gives production defaults;
/// [`ClusterConfig::for_tests`] gives the same shape about ten times faster.
#[derive(Clone)]
pub struct ClusterConfig {
    /// This node's id: unique among live nodes, `[A-Za-z0-9._:-]`, at most 128 bytes. Random per
    /// process by default ([`ClusterConfig::random_node_id`]).
    pub node_id: String,
    /// The base URL other nodes reach this one at (`http://10.0.0.5:7070`), without a path.
    /// Plain HTTP: put the nodes on a private network.
    pub advertise_url: String,
    /// The shared cluster secret for signing internal requests (at least 16 bytes).
    pub secret: Vec<u8>,
    /// How often the node's row is touched (1 s).
    pub heartbeat: Duration,
    /// A node is live while its heartbeat is younger than this, on the database's clock (5 s).
    pub node_ttl: Duration,
    /// How long a lease lasts on the database's clock after it's taken or renewed (10 s).
    pub lease_ttl: Duration,
    /// How often this node's leases are renewed, all in one statement (2 s). Retried sooner
    /// after a failure.
    pub renew_every: Duration,
    /// A lease is used only until `sent + lease_ttl - lease_margin` on this node's clock, where
    /// `sent` is when the renewal (or acquisition) that succeeded was sent (3 s).
    pub lease_margin: Duration,
    /// How often expired leases are looked for and taken by their preferred node (1 s).
    pub sweep_every: Duration,
    /// Expired leases read per page by the sweeper (512).
    pub sweep_page: usize,
    /// How often keys whose preferred node is another live node are handed off (10 s).
    pub rebalance_every: Duration,
    /// Keys handed off at once by rebalancing (4).
    pub handoffs_per_round: usize,
    /// How long a handoff waits for in-flight guards before it goes ahead anyway (30 s).
    pub handoff_timeout: Duration,
    /// How long `route` remembers which node owns a key that isn't ours (≤ 1 s; 1 s).
    pub remote_cache_ttl: Duration,
    /// Request bodies up to this size are buffered when forwarding, so a forward can be retried
    /// once (4 MiB). Larger or unsized bodies stream, with a single attempt.
    pub forward_buffer: usize,
    /// Connect timeout for forwarded requests (2 s).
    pub connect_timeout: Duration,
    /// The longest a forwarder waits before its one retry (1 s).
    pub max_retry_wait: Duration,
}

impl std::fmt::Debug for ClusterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterConfig")
            .field("node_id", &self.node_id)
            .field("advertise_url", &self.advertise_url)
            .field("secret", &"<redacted>")
            .field("heartbeat", &self.heartbeat)
            .field("node_ttl", &self.node_ttl)
            .field("lease_ttl", &self.lease_ttl)
            .field("renew_every", &self.renew_every)
            .field("lease_margin", &self.lease_margin)
            .field("sweep_every", &self.sweep_every)
            .field("sweep_page", &self.sweep_page)
            .field("rebalance_every", &self.rebalance_every)
            .field("handoffs_per_round", &self.handoffs_per_round)
            .field("handoff_timeout", &self.handoff_timeout)
            .field("remote_cache_ttl", &self.remote_cache_ttl)
            .field("forward_buffer", &self.forward_buffer)
            .field("connect_timeout", &self.connect_timeout)
            .field("max_retry_wait", &self.max_retry_wait)
            .finish()
    }
}

const fn ms(n: u64) -> Duration {
    Duration::from_millis(n)
}

impl ClusterConfig {
    /// Production timings.
    pub fn new(node_id: impl Into<String>, advertise_url: impl Into<String>, secret: impl Into<Vec<u8>>) -> ClusterConfig {
        ClusterConfig {
            node_id: node_id.into(),
            advertise_url: advertise_url.into(),
            secret: secret.into(),
            heartbeat: ms(1000),
            node_ttl: ms(5000),
            lease_ttl: ms(10_000),
            renew_every: ms(2000),
            lease_margin: ms(3000),
            sweep_every: ms(1000),
            sweep_page: 512,
            rebalance_every: ms(10_000),
            handoffs_per_round: 4,
            handoff_timeout: ms(30_000),
            remote_cache_ttl: ms(1000),
            forward_buffer: 4 << 20,
            connect_timeout: ms(2000),
            max_retry_wait: ms(1000),
        }
    }

    /// The same shape with short timings, for tests: leases last 1.5 s and are trusted for 1 s.
    pub fn for_tests(node_id: impl Into<String>, advertise_url: impl Into<String>, secret: impl Into<Vec<u8>>) -> ClusterConfig {
        ClusterConfig {
            heartbeat: ms(100),
            node_ttl: ms(600),
            lease_ttl: ms(1500),
            renew_every: ms(250),
            lease_margin: ms(500),
            sweep_every: ms(100),
            rebalance_every: ms(300),
            handoff_timeout: ms(1500),
            remote_cache_ttl: ms(200),
            connect_timeout: ms(1000),
            max_retry_wait: ms(300),
            ..ClusterConfig::new(node_id, advertise_url, secret)
        }
    }

    /// A random node id (`n-` and 16 hex digits), unique per process.
    pub fn random_node_id() -> String {
        use std::hash::{BuildHasher, Hasher};
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        h.write_u32(std::process::id());
        h.write_u64(N.fetch_add(1, Ordering::Relaxed));
        format!("n-{:016x}", h.finish())
    }

    /// How long after sending a renewal the node may act on it: `lease_ttl - lease_margin`.
    pub fn lease_validity(&self) -> Duration {
        self.lease_ttl.saturating_sub(self.lease_margin)
    }

    /// Check the invariants the lease protocol rests on (see the crate docs).
    pub fn validate(&self) -> Result<()> {
        let bad = |m: String| Err(Error::Config(m));
        let id = &self.node_id;
        if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:-".contains(&b)) {
            return bad(format!("node id {id:?} must be 1-128 characters of [A-Za-z0-9._:-]"));
        }
        let url = &self.advertise_url;
        // Plain HTTP only: node-to-node traffic stays on the private network (the HTTP client
        // is built without TLS).
        if !url.starts_with("http://") || reqwest::Url::parse(url).is_err() {
            return bad(format!("advertise url {url:?} must be an http:// URL"));
        }
        if self.secret.len() < 16 {
            return bad("the cluster secret must be at least 16 bytes".into());
        }
        let zero = Duration::ZERO;
        for (name, d) in [
            ("heartbeat", self.heartbeat),
            ("renew_every", self.renew_every),
            ("lease_margin", self.lease_margin),
            ("sweep_every", self.sweep_every),
            ("rebalance_every", self.rebalance_every),
        ] {
            if d == zero {
                return bad(format!("{name} must be positive"));
            }
        }
        if self.lease_margin >= self.lease_ttl {
            return bad("lease_margin must be shorter than lease_ttl".into());
        }
        // At least two renewal attempts fit in the time a renewal is trusted, so one slow or
        // failed renewal doesn't drop every lease.
        if self.renew_every * 2 > self.lease_validity() {
            return bad("renew_every must be at most half of lease_ttl - lease_margin".into());
        }
        if self.heartbeat * 2 > self.node_ttl {
            return bad("heartbeat must be at most half of node_ttl".into());
        }
        // A dead node leaves the live list before its leases expire, so the node that sweeps an
        // expired lease already computes placement without it.
        if self.node_ttl > self.lease_ttl {
            return bad("node_ttl must not exceed lease_ttl".into());
        }
        if self.remote_cache_ttl > Duration::from_secs(1) {
            return bad("remote_cache_ttl must be at most 1 s".into());
        }
        if self.sweep_page == 0 || self.handoffs_per_round == 0 {
            return bad("sweep_page and handoffs_per_round must be positive".into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_valid_and_invariants_checked() {
        let s = vec![7u8; 32];
        ClusterConfig::new("a", "http://10.0.0.1:7070", s.clone()).validate().unwrap();
        ClusterConfig::for_tests("a", "http://127.0.0.1:1", s.clone()).validate().unwrap();
        let mut c = ClusterConfig::new("a", "http://h:1", s.clone());
        c.renew_every = Duration::from_secs(4);
        assert!(c.validate().is_err());
        let mut c = ClusterConfig::new("a", "http://h:1", s.clone());
        c.lease_margin = c.lease_ttl;
        assert!(c.validate().is_err());
        let mut c = ClusterConfig::new("a", "http://h:1", s.clone());
        c.node_ttl = Duration::from_secs(11);
        assert!(c.validate().is_err());
        assert!(ClusterConfig::new("a\nb", "http://h:1", s.clone()).validate().is_err());
        assert!(ClusterConfig::new("a", "h:1", s.clone()).validate().is_err());
        assert!(ClusterConfig::new("a", "https://h:1", s.clone()).validate().is_err());
        assert!(ClusterConfig::new("a", "http://h:1", b"short".to_vec()).validate().is_err());
        assert_ne!(ClusterConfig::random_node_id(), ClusterConfig::random_node_id());
        assert!(!format!("{:?}", ClusterConfig::new("a", "http://h:1", s)).contains("7, 7"));
    }
}
