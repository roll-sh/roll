//! Opening one SSTable of a backup: its components found by name, `TOC.txt` checked against
//! them, `Statistics.db` parsed, and `Data.db` opened for a sequential scan with its
//! compression, chunk checksums and digest. A sequential scan of Data.db is all an import
//! needs, so the index components (`Index.db`/`Summary.db` of `big`, `Partitions.db`/`Rows.db`
//! of `bti` and ScyllaDB `ms`/`mt`) and the bloom filter are not read.

use crate::compression::CompressionInfo;
use crate::data::{DataReader, RandomAccess};
use crate::descriptor::{ComponentName, Version};
use crate::rows::Scanner;
use crate::scylla::ScyllaMetadata;
use crate::stats::Statistics;
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Where a backup's files are read from (blocking calls; run on a blocking thread).
pub trait Store: Send + Sync {
    /// A file for random access reads (its size is known from the listing).
    fn file(&self, key: &str, size: u64) -> Arc<dyn RandomAccess>;
    /// A small file whole.
    fn get(&self, key: &str) -> Result<Option<Bytes>>;
}

/// The files of one SSTable.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SstFiles {
    /// `<version>-<generation>-<format>`.
    pub prefix: String,
    pub version: Version,
    /// Component (`Data.db`) → (key, size).
    pub components: BTreeMap<String, (String, u64)>,
}

impl SstFiles {
    /// Group the files of a directory listing into SSTables (other files are ignored).
    pub fn group(files: &[(String, u64)]) -> Result<Vec<SstFiles>> {
        let mut by: BTreeMap<String, SstFiles> = BTreeMap::new();
        for (key, size) in files {
            let name = key.rsplit('/').next().unwrap_or(key);
            if ComponentName::legacy(name) {
                bail!("{key}: pre-3.0 sstables (ka/la) are not supported; upgrade them with `nodetool upgradesstables` first");
            }
            let Some(c) = ComponentName::parse(name) else { continue };
            let version = Version::parse(&c.version, c.format).with_context(|| key.clone())?;
            let dir = &key[..key.len() - name.len()];
            let id = format!("{dir}{}", c.prefix);
            by.entry(id).or_insert_with(|| SstFiles { prefix: c.prefix.clone(), version, components: BTreeMap::new() }).components.insert(c.component.clone(), (key.clone(), *size));
        }
        Ok(by.into_values().filter(|s| s.components.contains_key("Data.db") || s.components.contains_key("TOC.txt")).collect())
    }

    pub fn data_size(&self) -> u64 {
        self.components.get("Data.db").map_or(0, |c| c.1)
    }

    pub fn data_key(&self) -> &str {
        self.components.get("Data.db").map_or(self.prefix.as_str(), |c| c.0.as_str())
    }

    fn get(&self, store: &dyn Store, component: &str) -> Result<Option<Bytes>> {
        match self.components.get(component) {
            Some((key, _)) => store.get(key).with_context(|| key.clone()),
            None => Ok(None),
        }
    }
}

/// An SSTable ready to scan.
pub struct OpenSst {
    pub files: SstFiles,
    pub stats: Statistics,
    pub scylla: Option<ScyllaMetadata>,
    pub scanner: Scanner<DataReader>,
}

/// Components a usable SSTable needs (besides CompressionInfo.db for compressed data).
const REQUIRED: &[&str] = &["Data.db", "Statistics.db"];

/// Open an SSTable: `window` is the read-ahead of Data.db in bytes.
pub fn open(store: &dyn Store, files: &SstFiles, window: usize) -> Result<OpenSst> {
    let name = files.data_key().to_string();
    let toc: Vec<String> = match files.get(store, "TOC.txt")? {
        Some(t) => String::from_utf8_lossy(&t).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect(),
        None => Vec::new(),
    };
    for c in REQUIRED.iter().map(|c| c.to_string()).chain(toc.iter().filter(|c| is_needed(c)).cloned()) {
        if !files.components.contains_key(&c) {
            bail!("{name}: the sstable is incomplete: {c} is missing from the backup");
        }
    }
    let raw = files.get(store, "Statistics.db")?.ok_or_else(|| anyhow::anyhow!("{name}: Statistics.db is missing"))?;
    let stats = Statistics::parse(raw, &files.version).with_context(|| format!("{name}: Statistics.db"))?;
    if let Some(p) = &stats.partitioner
        && !p.ends_with("Murmur3Partitioner")
    {
        bail!("{name}: partitioner {p} is not supported (roll uses Murmur3Partitioner, the default of both products)");
    }
    let scylla = match files.get(store, "Scylla.db")? {
        Some(raw) => Some(ScyllaMetadata::parse(&raw).with_context(|| format!("{name}: Scylla.db"))?),
        None => None,
    };
    let digest = match files.get(store, "Digest.crc32")? {
        Some(d) => Some(String::from_utf8_lossy(&d).trim().parse::<i64>().map(|v| v as u32).with_context(|| format!("{name}: Digest.crc32"))?),
        None => None,
    };
    let (dkey, dsize) = files.components.get("Data.db").cloned().ok_or_else(|| anyhow::anyhow!("{name}: Data.db is missing"))?;
    let data = store.file(&dkey, dsize);
    let reader = match files.components.get("CompressionInfo.db") {
        Some((ckey, csize)) => {
            let ci_file = store.file(ckey, *csize);
            let head = ci_file.read_at(0, (*csize).min(CompressionInfo::HEAD_MAX) as usize)?;
            let info = CompressionInfo::parse(head, *csize, files.version.max_compressed_length()).with_context(|| format!("{name}: CompressionInfo.db"))?;
            DataReader::compressed(name.clone(), data, info, ci_file, window, digest)?
        }
        None => {
            let crc = files.components.get("CRC.db").map(|(k, s)| store.file(k, *s));
            DataReader::plain(name.clone(), data, crc, window, digest)?
        }
    };
    let scanner = Scanner::new(reader, Arc::new(stats.header.clone()), &files.version);
    Ok(OpenSst { files: files.clone(), stats, scylla, scanner })
}

/// Components whose absence makes the data unreadable (the indexes and filter aren't read).
fn is_needed(c: &str) -> bool {
    matches!(c, "Data.db" | "Statistics.db" | "CompressionInfo.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_components() {
        let files = vec![
            ("a/t-1/nb-1-big-Data.db".to_string(), 10),
            ("a/t-1/nb-1-big-Statistics.db".to_string(), 5),
            ("a/t-1/nb-2-big-Data.db".to_string(), 7),
            ("a/t-1/schema.cql".to_string(), 1),
            ("a/t-1/mt-3h4u_0cnc_5xh6o2d8zy2c2mzkrn-big-TOC.txt".to_string(), 1),
        ];
        let g = SstFiles::group(&files).unwrap();
        assert_eq!(g.len(), 3);
        assert_eq!(g[0].components.len(), 1);
        assert_eq!(g[0].prefix, "mt-3h4u_0cnc_5xh6o2d8zy2c2mzkrn-big");
        assert_eq!(g[1].data_size(), 10);
        assert!(SstFiles::group(&[("x/ks-cf-ka-1-Data.db".to_string(), 1)]).unwrap_err().to_string().contains("pre-3.0"));
    }
}
