//! A cluster's security audit entries: objects of NDJSON (zstd) under
//! `search/c/<cluster>/audit/`, written by the node that recorded them (`search-server`'s
//! audit log, which batches a cluster's entries for a few seconds). Each object is written once
//! under a fresh key that carries its time range, so a read fetches only the objects that
//! overlap what it asks for and retention deletes by key alone. Unique keys need no fencing: a
//! node that lost a cluster can only add entries that are true. They go with their cluster
//! (`Store::delete_cluster` removes its whole prefix); branches don't copy them, and garbage
//! collection never reads them.

use crate::keys;
use crate::{Store, random_u64};
use anyhow::Context;
use bytes::Bytes;

/// What one audit object decompresses to at most (a node writes a few MiB at a time).
const MAX_INFLATED: u64 = 256 << 20;

/// One object of audit entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuditObject {
    pub key: String,
    /// The first and last entry's time (Unix milliseconds).
    pub first_ms: i64,
    pub last_ms: i64,
    /// Stored (compressed) size.
    pub bytes: u64,
}

impl Store {
    /// Writes entries recorded from `first_ms` to `last_ms` (`ndjson`: one per line) as a new
    /// object. Returns it.
    pub async fn write_audit(&self, cluster: &str, first_ms: i64, last_ms: i64, ndjson: &[u8]) -> anyhow::Result<AuditObject> {
        keys::validate_cluster(cluster)?;
        let data = zstd::bulk::compress(ndjson, 3)?;
        let key = keys::audit_key(cluster, first_ms.max(0), last_ms.max(first_ms).max(0), random_u64());
        let bytes = data.len() as u64;
        self.inner.objects.put(&key, Bytes::from(data)).await.with_context(|| format!("writing audit entries {key}"))?;
        Ok(AuditObject { key, first_ms, last_ms, bytes })
    }

    /// A cluster's audit objects, oldest first (one listing).
    pub async fn list_audit(&self, cluster: &str) -> anyhow::Result<Vec<AuditObject>> {
        keys::validate_cluster(cluster)?;
        let prefix = keys::audit_prefix(cluster);
        let mut out: Vec<AuditObject> = self
            .inner
            .objects
            .list(&prefix)
            .await?
            .into_iter()
            .filter_map(|(key, bytes)| {
                let (first_ms, last_ms) = keys::parse_audit_file(key.strip_prefix(prefix.as_str())?)?;
                Some(AuditObject { key, first_ms, last_ms, bytes })
            })
            .collect();
        out.sort_by(|a, b| (a.first_ms, a.last_ms, &a.key).cmp(&(b.first_ms, b.last_ms, &b.key)));
        Ok(out)
    }

    /// An audit object's entries (`None`: deleted since it was listed).
    pub async fn read_audit(&self, o: &AuditObject) -> anyhow::Result<Option<Vec<u8>>> {
        match self.inner.objects.get(&o.key).await.with_context(|| format!("reading audit entries {}", o.key))? {
            Some(raw) => Ok(Some(crate::manifest::inflate_bounded(&raw, MAX_INFLATED).with_context(|| format!("audit entries {} are damaged", o.key))?)),
            None => Ok(None),
        }
    }

    /// Deletes audit objects (retention).
    pub async fn delete_audit(&self, objects: &[AuditObject]) -> anyhow::Result<()> {
        for o in objects {
            self.inner.objects.delete(&o.key).await.with_context(|| format!("deleting audit entries {}", o.key))?;
        }
        Ok(())
    }
}
