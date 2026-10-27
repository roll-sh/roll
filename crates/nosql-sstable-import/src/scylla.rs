//! `Scylla.db`, ScyllaDB's metadata component, from its published format description
//! (`docs/dev/sstable-scylla-format.md`): a set of tagged subcomponents, optionally followed by a
//! CRC32 of everything before it.
//!
//! Import uses the `features` bits and the `schema` subcomponent (keyspace, table, every column
//! with its kind and marshal type): it names the primary key columns, which Statistics.db
//! doesn't, so a table can be recreated from the SSTables alone.

use crate::input::{In, SliceIn};
use crate::types::SType;
use anyhow::{Context, Result, bail};
use bytes::Bytes;

const FEATURES: u32 = 2;
const SSTABLE_ORIGIN: u32 = 6;
const SCYLLA_VERSION: u32 = 8;
const SCHEMA: u32 = 11;
const COMPONENTS_DIGESTS: u32 = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnKind {
    PartitionKey,
    Clustering,
    Static,
    Regular,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SchemaColumn {
    pub kind: ColumnKind,
    pub name: String,
    pub ty: SType,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScyllaSchema {
    pub table_id: [u8; 16],
    pub keyspace: String,
    pub table: String,
    /// In the file's order (primary key columns by position).
    pub columns: Vec<SchemaColumn>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ScyllaMetadata {
    pub features: u64,
    pub origin: Option<String>,
    pub version: Option<String>,
    pub schema: Option<ScyllaSchema>,
}

fn string32(r: &mut SliceIn) -> Result<String> {
    let n = r.be32()? as usize;
    Ok(String::from_utf8_lossy(&r.bytes(n)?).into_owned())
}

impl ScyllaMetadata {
    pub fn parse(raw: &[u8]) -> Result<ScyllaMetadata> {
        let mut r = SliceIn::new(Bytes::copy_from_slice(raw));
        let n = r.be32()?;
        let n = r.count(n as u64, 8)?;
        let mut m = ScyllaMetadata::default();
        let mut digested = false;
        for _ in 0..n {
            let tag = r.be32()?;
            let size = r.be32()? as usize;
            let mut body = SliceIn::new(r.bytes(size)?);
            match tag {
                FEATURES => m.features = body.be64()?,
                SSTABLE_ORIGIN => m.origin = Some(string32(&mut body)?),
                SCYLLA_VERSION => m.version = Some(string32(&mut body)?),
                SCHEMA => m.schema = Some(parse_schema(&mut body).context("schema subcomponent")?),
                COMPONENTS_DIGESTS => digested = true,
                _ => {}
            }
        }
        if digested {
            let at = r.pos();
            let want = r.be32().context("the trailing digest is missing")?;
            if crc32fast::hash(&raw[..at]) != want {
                bail!("Scylla.db digest mismatch");
            }
        }
        Ok(m)
    }
}

fn parse_schema(r: &mut SliceIn) -> Result<ScyllaSchema> {
    let id = r.bytes(16)?;
    r.bytes(16)?; // schema version
    let keyspace = string32(r)?;
    let table = string32(r)?;
    let n = r.be32()?;
    let n = r.count(n as u64, 9)?;
    let mut columns = Vec::with_capacity(n);
    for _ in 0..n {
        let kind = match r.u8()? {
            1 => ColumnKind::PartitionKey,
            2 => ColumnKind::Clustering,
            3 => ColumnKind::Static,
            4 => ColumnKind::Regular,
            k => bail!("unknown column kind {k}"),
        };
        let name = string32(r)?;
        let ty = SType::parse(&string32(r)?)?;
        columns.push(SchemaColumn { kind, name, ty });
    }
    Ok(ScyllaSchema { table_id: id[..].try_into().expect("16"), keyspace, table, columns })
}

impl ScyllaSchema {
    /// A `CREATE TABLE` for this table (columns and key only; options are defaults).
    pub fn create_table(&self) -> String {
        use crate::types::quote;
        let cols: Vec<String> = self.columns.iter().map(|c| format!("{} {}{}", quote(&c.name), c.ty.cql(), if c.kind == ColumnKind::Static { " static" } else { "" })).collect();
        let pk: Vec<String> = self.columns.iter().filter(|c| c.kind == ColumnKind::PartitionKey).map(|c| quote(&c.name)).collect();
        let ck: Vec<&SchemaColumn> = self.columns.iter().filter(|c| c.kind == ColumnKind::Clustering).collect();
        let mut key = format!("({})", pk.join(", "));
        for c in &ck {
            key.push_str(", ");
            key.push_str(&quote(&c.name));
        }
        let mut s = format!("CREATE TABLE IF NOT EXISTS {}.{} ({}, PRIMARY KEY ({key}))", quote(&self.keyspace), quote(&self.table), cols.join(", "));
        if ck.iter().any(|c| matches!(c.ty, SType::Reversed(_))) {
            let order: Vec<String> = ck.iter().map(|c| format!("{} {}", quote(&c.name), if matches!(c.ty, SType::Reversed(_)) { "DESC" } else { "ASC" })).collect();
            s.push_str(&format!(" WITH CLUSTERING ORDER BY ({})", order.join(", ")));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s32(out: &mut Vec<u8>, s: &str) {
        out.extend_from_slice(&(s.len() as u32).to_be_bytes());
        out.extend_from_slice(s.as_bytes());
    }

    pub fn build(digest: bool) -> Vec<u8> {
        let m = "org.apache.cassandra.db.marshal.";
        let mut schema = vec![7u8; 32];
        s32(&mut schema, "ks");
        s32(&mut schema, "t");
        schema.extend_from_slice(&3u32.to_be_bytes());
        for (k, n, t) in [(1u8, "k", format!("{m}Int32Type")), (2, "c", format!("{m}ReversedType({m}UTF8Type)")), (4, "v", format!("{m}ListType({m}Int32Type)"))] {
            schema.push(k);
            s32(&mut schema, n);
            s32(&mut schema, &t);
        }
        let mut subs: Vec<(u32, Vec<u8>)> = vec![(FEATURES, 0x7fu64.to_be_bytes().to_vec()), (SCHEMA, schema), (99, vec![1, 2, 3])];
        if digest {
            subs.push((COMPONENTS_DIGESTS, 0u32.to_be_bytes().to_vec()));
        }
        let mut out = (subs.len() as u32).to_be_bytes().to_vec();
        for (t, b) in subs {
            out.extend_from_slice(&t.to_be_bytes());
            out.extend_from_slice(&(b.len() as u32).to_be_bytes());
            out.extend_from_slice(&b);
        }
        if digest {
            let c = crc32fast::hash(&out);
            out.extend_from_slice(&c.to_be_bytes());
        }
        out
    }

    #[test]
    fn parses_schema_and_digest() {
        for digest in [false, true] {
            let m = ScyllaMetadata::parse(&build(digest)).unwrap();
            assert_eq!(m.features, 0x7f);
            let s = m.schema.unwrap();
            assert_eq!((s.keyspace.as_str(), s.table.as_str(), s.columns.len()), ("ks", "t", 3));
            assert_eq!(s.create_table(), "CREATE TABLE IF NOT EXISTS ks.t (k int, c text, v list<int>, PRIMARY KEY ((k), c)) WITH CLUSTERING ORDER BY (c DESC)");
        }
        let mut bad = build(true);
        let n = bad.len();
        bad[n - 10] ^= 1;
        assert!(ScyllaMetadata::parse(&bad).is_err());
        assert!(ScyllaMetadata::parse(&build(false)[..20]).is_err());
    }
}
