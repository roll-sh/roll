//! Typed errors callers act on. Everything else the store returns is an `anyhow::Error` with
//! context; these two are attached as the error's source so callers can `downcast_ref` them:
//!
//! - [`Fenced`]: another node took the cluster over under a higher ownership epoch. The caller
//!   must stop serving writes for the cluster (unload it) — nothing it writes can become visible.
//! - [`WalFull`]: object storage has fallen so far behind that the local WAL reached its disk
//!   limit. Transient; Elasticsearch's equivalent is a 429 `es_rejected_execution_exception`.

use std::fmt;

/// A write under an ownership epoch older than one already in object storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fenced {
    pub cluster: String,
    /// The epoch this node writes under ([`crate::Store::set_epoch`]).
    pub epoch: u64,
    /// The newer epoch found.
    pub owner_epoch: u64,
}

impl fmt::Display for Fenced {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cluster [{}] is owned under epoch {} now; this node (epoch {}) may no longer write it",
            self.cluster, self.owner_epoch, self.epoch
        )
    }
}

impl std::error::Error for Fenced {}

/// The local WAL holds more unshipped bytes than `WalConfig::max_local_bytes`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WalFull {
    pub unshipped_bytes: u64,
    pub limit: u64,
}

impl fmt::Display for WalFull {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "the WAL has {} bytes waiting to be shipped to object storage, at its local limit of {} bytes; \
             writes resume once object storage catches up",
            self.unshipped_bytes, self.limit
        )
    }
}

impl std::error::Error for WalFull {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_survive_anyhow_and_downcast() {
        let e: anyhow::Error = Fenced { cluster: "c".into(), epoch: 1, owner_epoch: 2 }.into();
        let e = e.context("writing a manifest");
        assert_eq!(e.downcast_ref::<Fenced>().map(|f| f.owner_epoch), Some(2));
        assert!(format!("{e:#}").contains("owned under epoch 2"));
        let e: anyhow::Error = WalFull { unshipped_bytes: 10, limit: 8 }.into();
        assert!(e.downcast_ref::<WalFull>().is_some());
    }
}
