//! SSTable file names and format versions (`org.apache.cassandra.io.sstable.Descriptor`,
//! `BigFormat.BigVersion`, `BtiFormat.BtiVersion`; ScyllaDB's `ms`/`mt` from its documentation:
//! the `me` data format with Cassandra 5.0's trie index).
//!
//! A component file is `<version>-<generation>-<format>-<Component>`, e.g. `nb-1-big-Data.db`,
//! `da-3gdq_0bki_2cvk01yl83nj0tp5gh-bti-Statistics.db` (UUID generations are base-36).

use anyhow::{Result, bail};

/// The on-disk format family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Format {
    /// `big`: Index.db + Summary.db (Cassandra 3–5, ScyllaDB `me`) or, for ScyllaDB `ms`/`mt`,
    /// Partitions.db + Rows.db under the `big` name.
    Big,
    /// `bti`: Cassandra 5.0's trie-indexed format (Partitions.db + Rows.db).
    Bti,
}

/// A format version and what it implies for reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Version {
    pub name: [u8; 2],
    pub format: Format,
}

impl Version {
    pub fn parse(name: &str, format: Format) -> Result<Version> {
        let b = name.as_bytes();
        if b.len() != 2 || !b.iter().all(u8::is_ascii_lowercase) {
            bail!("unknown sstable version {name:?}");
        }
        let v = Version { name: [b[0], b[1]], format };
        let ok = match format {
            // 3.0 (`ma`) to 5.0 (`oa`); ScyllaDB `md`/`me`/`ms`/`mt` are `m` versions.
            Format::Big => matches!(b[0], b'm' | b'n' | b'o'),
            Format::Bti => b[0] == b'd',
        };
        if !ok {
            bail!("sstable version {name} ({}) is not supported: import reads Cassandra 3.0+ (m*, nb, oa, da) and ScyllaDB me/ms/mt sstables", format.name());
        }
        Ok(v)
    }

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.name).unwrap_or("??")
    }

    fn at_least(&self, v: &str) -> bool {
        self.name.as_slice() >= v.as_bytes()
    }

    /// Cassandra 5.0's unsigned 32-bit local deletion times (`hasUIntDeletionTime`): `oa` and
    /// `da`. They also read as messaging version 50 (negative times decode as unsigned).
    pub fn uint_deletion_time(&self) -> bool {
        match self.format {
            Format::Bti => true,
            Format::Big => self.name[0] != b'm' && self.at_least("oa"),
        }
    }

    /// Statistics.db components carry CRC32s (`na`+).
    pub fn metadata_checksum(&self) -> bool {
        match self.format {
            Format::Bti => true,
            Format::Big => self.name[0] != b'm' && self.at_least("na"),
        }
    }

    /// CompressionInfo.db has `maxCompressedLength` (`na`+; chunks that don't compress are
    /// stored raw).
    pub fn max_compressed_length(&self) -> bool {
        self.metadata_checksum()
    }
}

impl Format {
    pub fn name(&self) -> &'static str {
        match self {
            Format::Big => "big",
            Format::Bti => "bti",
        }
    }
}

/// One component file name, split.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentName {
    /// `<version>-<generation>-<format>`: what all components of one SSTable share.
    pub prefix: String,
    pub version: String,
    pub generation: String,
    pub format: Format,
    /// `Data.db`, `Statistics.db`, `TOC.txt`, …
    pub component: String,
}

impl ComponentName {
    /// Parse a file name (no directory). `None` for files that aren't SSTable components
    /// (`schema.cql`, `manifest.json`) and for pre-3.0 names (`<ks>-<cf>-ka-1-Data.db`), which
    /// [`ComponentName::legacy`] recognises.
    pub fn parse(file: &str) -> Option<ComponentName> {
        let mut parts = file.splitn(4, '-');
        let version = parts.next()?;
        let generation = parts.next()?;
        let format = match parts.next()? {
            "big" => Format::Big,
            "bti" => Format::Bti,
            _ => return None,
        };
        let component = parts.next()?;
        if version.len() != 2 || !version.bytes().all(|c| c.is_ascii_lowercase()) || generation.is_empty() || component.is_empty() {
            return None;
        }
        if !generation.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_') {
            return None;
        }
        Some(ComponentName { prefix: format!("{version}-{generation}-{}", format.name()), version: version.into(), generation: generation.into(), format, component: component.into() })
    }

    /// A pre-3.0 component name (`ka`/`la`), which import can't read.
    pub fn legacy(file: &str) -> bool {
        let parts: Vec<&str> = file.split('-').collect();
        parts.len() >= 3 && parts.iter().any(|p| matches!(*p, "ka" | "la" | "jb" | "ic")) && file.ends_with("Data.db")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        let c = ComponentName::parse("nb-12-big-Data.db").unwrap();
        assert_eq!((c.prefix.as_str(), c.version.as_str(), c.component.as_str(), c.format), ("nb-12-big", "nb", "Data.db", Format::Big));
        let c = ComponentName::parse("da-3gdq_0bki_2cvk01yl83nj0tp5gh-bti-Partitions.db").unwrap();
        assert_eq!((c.format, c.generation.as_str()), (Format::Bti, "3gdq_0bki_2cvk01yl83nj0tp5gh"));
        assert_eq!(ComponentName::parse("mt-3h4u_0cnc_5xh6o2d8zy2c2mzkrn-big-TOC.txt").unwrap().component, "TOC.txt");
        assert!(ComponentName::parse("schema.cql").is_none());
        assert!(ComponentName::parse("manifest.json").is_none());
        assert!(ComponentName::parse("ks-cf-ka-1-Data.db").is_none());
        assert!(ComponentName::legacy("ks-cf-ka-1-Data.db"));
    }

    #[test]
    fn versions() {
        let v = |n, f| Version::parse(n, f).unwrap();
        assert!(!v("nb", Format::Big).uint_deletion_time() && v("nb", Format::Big).metadata_checksum());
        assert!(v("oa", Format::Big).uint_deletion_time());
        assert!(v("da", Format::Bti).uint_deletion_time() && v("da", Format::Bti).max_compressed_length());
        for scylla in ["me", "ms", "mt", "md"] {
            let s = v(scylla, Format::Big);
            assert!(!s.uint_deletion_time() && !s.metadata_checksum() && !s.max_compressed_length(), "{scylla}");
        }
        assert!(Version::parse("ka", Format::Big).is_err());
        assert!(Version::parse("oa", Format::Bti).is_err());
    }
}
