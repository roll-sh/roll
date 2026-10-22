//! Backup storage: object storage (`s3://bucket/prefix`, through `pg_storage::Objects`) or a
//! local directory (`file:///path`), read with ranged GETs from blocking threads.

use crate::data::RandomAccess;
use crate::sstable::Store;
use anyhow::{Result, bail};
use bytes::Bytes;
use pg_storage::{Objects, StorageConfig};
use std::sync::Arc;

/// A backup location.
#[derive(Clone)]
pub struct Location {
    pub objects: Objects,
    /// Shown in messages (`s3://bucket/prefix`).
    pub url: String,
    /// Query parameters of the URL (`tag`, `region`, `endpoint`, …).
    pub params: Vec<(String, String)>,
}

impl Location {
    /// `s3://bucket/prefix[?region=..&endpoint=..&tag=..]` or `file:///absolute/path[?tag=..]`.
    pub fn parse(from: &str) -> Result<Location> {
        let (base, query) = from.split_once('?').unwrap_or((from, ""));
        let params: Vec<(String, String)> = query.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').map_or((p.to_string(), String::new()), |(k, v)| (k.to_string(), v.to_string()))).collect();
        let param = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let cfg = if let Some(rest) = base.strip_prefix("s3://") {
            let (bucket, prefix) = rest.split_once('/').unwrap_or((rest, ""));
            if bucket.is_empty() {
                bail!("the source URL has no bucket");
            }
            let prefix = prefix.trim_matches('/');
            StorageConfig::S3 { bucket: bucket.to_string(), region: param("region"), endpoint: param("endpoint"), prefix: (!prefix.is_empty()).then(|| prefix.to_string()) }
        } else if let Some(path) = base.strip_prefix("file://") {
            if !path.starts_with('/') {
                bail!("file:// sources need an absolute path");
            }
            let p = std::path::PathBuf::from(path);
            if !p.is_dir() {
                bail!("{path} is not a directory");
            }
            StorageConfig::Local(p)
        } else {
            bail!("unsupported backup URL {base:?} (s3://bucket/path or file:///path)");
        };
        Ok(Location { objects: Objects::new(&cfg)?, url: base.to_string(), params })
    }

    pub fn param(&self, k: &str) -> Option<&str> {
        self.params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    /// Every file of the backup: `(key, size)`, keys relative to the location.
    pub async fn list(&self) -> Result<Vec<(String, u64)>> {
        let mut v = self.objects.list("").await?;
        v.sort();
        Ok(v)
    }

    /// A blocking [`Store`] over this location (call its methods off the async runtime).
    pub fn store(&self, rt: tokio::runtime::Handle) -> ObjectStore {
        ObjectStore { objects: self.objects.clone(), rt }
    }
}

/// Blocking reads of objects through the runtime (from `spawn_blocking` threads).
#[derive(Clone)]
pub struct ObjectStore {
    objects: Objects,
    rt: tokio::runtime::Handle,
}

struct ObjectFile {
    objects: Objects,
    rt: tokio::runtime::Handle,
    key: String,
    size: u64,
}

impl RandomAccess for ObjectFile {
    fn len(&self) -> u64 {
        self.size
    }

    fn read_at(&self, offset: u64, len: usize) -> Result<Bytes> {
        let end = offset.checked_add(len as u64).filter(|e| *e <= self.size).ok_or_else(|| anyhow::anyhow!("{}: read past the end", self.key))?;
        if len == 0 {
            return Ok(Bytes::new());
        }
        let mut last = None;
        for attempt in 0..3 {
            match self.rt.block_on(self.objects.get_range(&self.key, offset..end)) {
                Ok(Some(b)) if b.len() == len => return Ok(b),
                Ok(Some(b)) => bail!("{}: short read ({} of {len} bytes at {offset})", self.key, b.len()),
                Ok(None) => bail!("{}: the file disappeared", self.key),
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(200 << attempt));
                }
            }
        }
        Err(last.expect("an error").context(format!("reading {}", self.key)))
    }
}

impl Store for ObjectStore {
    fn file(&self, key: &str, size: u64) -> Arc<dyn RandomAccess> {
        Arc::new(ObjectFile { objects: self.objects.clone(), rt: self.rt.clone(), key: key.to_string(), size })
    }

    fn get(&self, key: &str) -> Result<Option<Bytes>> {
        self.rt.block_on(self.objects.get(key))
    }
}
