//! RESULT bodies (spec 4.2.5): Void, Rows, Set_keyspace, Prepared, Schema_change.
//!
//! Rows keep their cells serialised: a decoded [`Rows`] holds the `<rows_content>` as one
//! `Bytes` (checked once at decode, so iterating never fails) and hands out cells as slices
//! of it; an encoded one writes cells the engine serialised already ([`RowsBuilder`]).
//! Follows `org.apache.cassandra.cql3.ResultSet` and `transport.messages.ResultMessage`.

use bytes::{BufMut, Bytes, BytesMut};

use crate::error::{DecodeError, Field};
use crate::event::SchemaChange;
use crate::types::{ColSpec, put_specs, read_specs, same_table};
use crate::version::{Ctx, Version};
use crate::wire::{PutExt, Reader};

/// Result metadata flags.
pub mod mflag {
    pub const GLOBAL_TABLES_SPEC: u32 = 0x0001;
    pub const HAS_MORE_PAGES: u32 = 0x0002;
    pub const NO_METADATA: u32 = 0x0004;
    /// v5 (or v4 with `SCYLLA_USE_METADATA_ID`)
    pub const METADATA_CHANGED: u32 = 0x0008;
    /// Prepared metadata: the statement is a lightweight transaction. The bit ScyllaDB
    /// advertises in `SCYLLA_LWT_ADD_METADATA_MARK` (`LWT_OPTIMIZATION_META_BIT_MASK=2147483648`).
    pub const SCYLLA_LWT: u32 = 0x8000_0000;
}

/// `<metadata>` of a Rows result (also a Prepared result's `<result_metadata>`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RowsMetadata {
    pub global_tables_spec: bool,
    /// With NO_METADATA only the count is sent.
    pub no_metadata: bool,
    pub column_count: i32,
    pub paging_state: Option<Bytes>,
    /// METADATA_CHANGED: the new result metadata id.
    pub new_metadata_id: Option<Bytes>,
    pub columns: Vec<ColSpec>,
}

impl RowsMetadata {
    /// Metadata for `columns`, with Cassandra's choice of the global table spec.
    pub fn new(columns: Vec<ColSpec>) -> Self {
        RowsMetadata {
            global_tables_spec: same_table(&columns),
            no_metadata: false,
            column_count: columns.len() as i32,
            paging_state: None,
            new_metadata_id: None,
            columns,
        }
    }

    /// The empty NO_METADATA metadata of non-SELECT prepared statements.
    pub fn empty() -> Self {
        RowsMetadata { no_metadata: true, ..Default::default() }
    }

    /// The same columns sent as a count only (the client asked to skip metadata).
    pub fn skipped(&self) -> Self {
        RowsMetadata {
            global_tables_spec: false,
            no_metadata: true,
            column_count: self.column_count,
            paging_state: self.paging_state.clone(),
            new_metadata_id: None,
            columns: vec![],
        }
    }

    pub fn flags(&self) -> u32 {
        let mut f = 0;
        if self.global_tables_spec && !self.no_metadata {
            f |= mflag::GLOBAL_TABLES_SPEC;
        }
        if self.paging_state.is_some() {
            f |= mflag::HAS_MORE_PAGES;
        }
        if self.no_metadata {
            f |= mflag::NO_METADATA;
        } else if self.new_metadata_id.is_some() {
            f |= mflag::METADATA_CHANGED;
        }
        f
    }

    pub fn encode(&self, ctx: Ctx, out: &mut BytesMut) {
        let mut flags = self.flags();
        if !ctx.metadata_id() {
            flags &= !mflag::METADATA_CHANGED;
        }
        out.put_u32(flags);
        out.put_i32(if self.no_metadata { self.column_count } else { self.columns.len() as i32 });
        if let Some(p) = &self.paging_state {
            out.put_cql_bytes(Some(p));
        }
        if let (true, Some(id)) = (flags & mflag::METADATA_CHANGED != 0, &self.new_metadata_id) {
            out.put_short_bytes(id);
        }
        if !self.no_metadata {
            put_specs(out, ctx, flags & mflag::GLOBAL_TABLES_SPEC != 0, &self.columns);
        }
    }

    pub fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        let flags = r.u32()?;
        let column_count = r.i32()?;
        if column_count < 0 {
            return Err(DecodeError::invalid(format!("Invalid column count {column_count}")));
        }
        let paging_state = if flags & mflag::HAS_MORE_PAGES != 0 { r.bytes()? } else { None };
        let no_metadata = flags & mflag::NO_METADATA != 0;
        let new_metadata_id = if flags & mflag::METADATA_CHANGED != 0 && !no_metadata { Some(r.short_bytes()?) } else { None };
        let global_tables_spec = flags & mflag::GLOBAL_TABLES_SPEC != 0;
        let columns = if no_metadata { vec![] } else { read_specs(r, global_tables_spec, column_count as usize)? };
        Ok(RowsMetadata { global_tables_spec, no_metadata, column_count, paging_state, new_metadata_id, columns })
    }
}

/// A Rows result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rows {
    pub metadata: RowsMetadata,
    pub row_count: i32,
    /// `row_count * column_count` `[bytes]` cells, back to back.
    pub content: Bytes,
}

impl Rows {
    pub fn empty(metadata: RowsMetadata) -> Self {
        Rows { metadata, row_count: 0, content: Bytes::new() }
    }

    pub fn column_count(&self) -> usize {
        self.metadata.column_count.max(0) as usize
    }

    pub fn len(&self) -> usize {
        self.row_count.max(0) as usize
    }

    pub fn is_empty(&self) -> bool {
        self.row_count <= 0
    }

    /// The rows; each yields its cells (`None` for null) as slices of `content`.
    pub fn rows(&self) -> RowIter<'_> {
        RowIter { rows: self, at: 0, left: self.len() }
    }

    /// All cells, copied into vectors (the `Bytes` still share `content`).
    pub fn to_vec(&self) -> Vec<Vec<Option<Bytes>>> {
        self.rows().map(|r| r.collect()).collect()
    }

    fn encode(&self, ctx: Ctx, out: &mut BytesMut) {
        self.metadata.encode(ctx, out);
        out.put_i32(self.row_count);
        out.put_slice(&self.content);
    }

    fn decode(r: &mut Reader) -> Result<Self, DecodeError> {
        let metadata = RowsMetadata::decode(r)?;
        let row_count = r.i32()?;
        if row_count < 0 {
            return Err(DecodeError::invalid(format!("Invalid row count {row_count}")));
        }
        let cols = metadata.column_count as usize;
        // Walk the cells once without allocating; rows of zero columns take no bytes.
        let mut probe = r.clone();
        if cols > 0 {
            for _ in 0..row_count {
                for _ in 0..cols {
                    let n = probe.i32()?;
                    if n > 0 {
                        probe.raw(n as usize, Field::Bytes)?;
                    }
                }
            }
        }
        let content = r.raw(probe.offset() - r.offset(), Field::Raw)?;
        Ok(Rows { metadata, row_count, content })
    }
}

pub struct RowIter<'a> {
    rows: &'a Rows,
    at: usize,
    left: usize,
}

impl<'a> Iterator for RowIter<'a> {
    type Item = CellIter;

    fn next(&mut self) -> Option<CellIter> {
        if self.left == 0 {
            return None;
        }
        self.left -= 1;
        let cols = self.rows.column_count();
        let start = self.at;
        let c = &self.rows.content;
        for _ in 0..cols {
            let n = i32::from_be_bytes(c[self.at..self.at + 4].try_into().expect("validated"));
            self.at += 4 + n.max(0) as usize;
        }
        Some(CellIter { buf: c.slice(start..self.at), at: 0 })
    }
}

/// The cells of one row.
pub struct CellIter {
    buf: Bytes,
    at: usize,
}

impl Iterator for CellIter {
    type Item = Option<Bytes>;

    fn next(&mut self) -> Option<Option<Bytes>> {
        if self.at >= self.buf.len() {
            return None;
        }
        let n = i32::from_be_bytes(self.buf[self.at..self.at + 4].try_into().expect("validated"));
        self.at += 4;
        if n < 0 {
            return Some(None);
        }
        let v = self.buf.slice(self.at..self.at + n as usize);
        self.at += n as usize;
        Some(Some(v))
    }
}

/// Builds `<rows_content>` from cells the engine serialised.
#[derive(Debug, Default)]
pub struct RowsBuilder {
    buf: BytesMut,
    rows: i32,
}

impl RowsBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_capacity(bytes: usize) -> Self {
        RowsBuilder { buf: BytesMut::with_capacity(bytes), rows: 0 }
    }

    /// Appends one cell of the current row.
    pub fn cell(&mut self, v: Option<&[u8]>) -> &mut Self {
        self.buf.put_cql_bytes(v);
        self
    }

    /// Appends a cell that is already `[bytes]`-encoded (length prefix included).
    pub fn raw_cell(&mut self, encoded: &[u8]) -> &mut Self {
        self.buf.put_slice(encoded);
        self
    }

    /// Ends the current row.
    pub fn end_row(&mut self) -> &mut Self {
        self.rows += 1;
        self
    }

    pub fn row_count(&self) -> i32 {
        self.rows
    }

    pub fn finish(self, metadata: RowsMetadata) -> Rows {
        Rows { metadata, row_count: self.rows, content: self.buf.freeze() }
    }
}

/// Bind-marker metadata of a Prepared result.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreparedMetadata {
    pub global_tables_spec: bool,
    /// ScyllaDB's LWT mark ([`mflag::SCYLLA_LWT`]).
    pub lwt: bool,
    /// v4+: bind index of each partition key column, in key order. Cassandra sends none
    /// when the markers span tables.
    pub pk_indexes: Vec<u16>,
    pub columns: Vec<ColSpec>,
}

impl PreparedMetadata {
    pub fn new(columns: Vec<ColSpec>, pk_indexes: Vec<u16>) -> Self {
        PreparedMetadata { global_tables_spec: same_table(&columns), lwt: false, pk_indexes, columns }
    }

    fn encode(&self, ctx: Ctx, out: &mut BytesMut) {
        let version = ctx.version;
        let global = self.global_tables_spec && !self.columns.is_empty();
        let mut flags = if global { mflag::GLOBAL_TABLES_SPEC } else { 0 };
        if self.lwt {
            flags |= mflag::SCYLLA_LWT;
        }
        out.put_u32(flags);
        out.put_i32(self.columns.len() as i32);
        if version >= Version::V4 {
            if global {
                out.put_i32(self.pk_indexes.len() as i32);
                for i in &self.pk_indexes {
                    out.put_u16(*i);
                }
            } else {
                out.put_i32(0);
            }
        }
        put_specs(out, ctx, global, &self.columns);
    }

    fn decode(version: Version, r: &mut Reader) -> Result<Self, DecodeError> {
        let flags = r.u32()?;
        let count = r.i32()?;
        if count < 0 {
            return Err(DecodeError::invalid(format!("Invalid column count {count}")));
        }
        let mut pk_indexes = vec![];
        if version >= Version::V4 {
            let n = r.i32()?.max(0) as usize;
            pk_indexes.reserve(n.min(r.remaining() / 2));
            for _ in 0..n {
                pk_indexes.push(r.u16()?);
            }
        }
        let global_tables_spec = flags & mflag::GLOBAL_TABLES_SPEC != 0;
        let columns = read_specs(r, global_tables_spec, count as usize)?;
        Ok(PreparedMetadata { global_tables_spec, lwt: flags & mflag::SCYLLA_LWT != 0, pk_indexes, columns })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    pub id: Bytes,
    /// v5, or v4 with `SCYLLA_USE_METADATA_ID`.
    pub result_metadata_id: Option<Bytes>,
    pub metadata: PreparedMetadata,
    pub result_metadata: RowsMetadata,
}

/// Result kinds.
pub mod kind {
    pub const VOID: i32 = 0x0001;
    pub const ROWS: i32 = 0x0002;
    pub const SET_KEYSPACE: i32 = 0x0003;
    pub const PREPARED: i32 = 0x0004;
    pub const SCHEMA_CHANGE: i32 = 0x0005;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResultBody {
    Void,
    Rows(Rows),
    SetKeyspace(String),
    Prepared(Prepared),
    SchemaChange(SchemaChange),
}

impl ResultBody {
    pub fn kind(&self) -> i32 {
        match self {
            ResultBody::Void => kind::VOID,
            ResultBody::Rows(_) => kind::ROWS,
            ResultBody::SetKeyspace(_) => kind::SET_KEYSPACE,
            ResultBody::Prepared(_) => kind::PREPARED,
            ResultBody::SchemaChange(_) => kind::SCHEMA_CHANGE,
        }
    }

    pub fn encode(&self, ctx: impl Into<Ctx>, out: &mut BytesMut) {
        let ctx = ctx.into();
        out.put_i32(self.kind());
        match self {
            ResultBody::Void => {}
            ResultBody::Rows(rows) => rows.encode(ctx, out),
            ResultBody::SetKeyspace(ks) => out.put_string(ks),
            ResultBody::Prepared(p) => {
                out.put_short_bytes(&p.id);
                if ctx.metadata_id() {
                    out.put_short_bytes(p.result_metadata_id.as_deref().unwrap_or_default());
                }
                p.metadata.encode(ctx, out);
                let mut rm = p.result_metadata.clone();
                rm.new_metadata_id = None;
                rm.encode(ctx, out);
            }
            ResultBody::SchemaChange(c) => c.encode(ctx.version, out),
        }
    }

    pub fn decode(ctx: impl Into<Ctx>, r: &mut Reader) -> Result<Self, DecodeError> {
        let ctx = ctx.into();
        Ok(match r.i32()? {
            kind::VOID => ResultBody::Void,
            kind::ROWS => ResultBody::Rows(Rows::decode(r)?),
            kind::SET_KEYSPACE => ResultBody::SetKeyspace(r.string()?),
            kind::PREPARED => {
                let id = r.short_bytes()?;
                let result_metadata_id = if ctx.metadata_id() { Some(r.short_bytes()?) } else { None };
                let metadata = PreparedMetadata::decode(ctx.version, r)?;
                let result_metadata = RowsMetadata::decode(r)?;
                ResultBody::Prepared(Prepared { id, result_metadata_id, metadata, result_metadata })
            }
            kind::SCHEMA_CHANGE => ResultBody::SchemaChange(SchemaChange::decode(r)?),
            k => return Err(DecodeError::invalid(format!("Unknown result kind {k}"))),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{SchemaChangeType, SchemaTarget};
    use crate::types::ColType;
    use crate::version::Features;

    fn cols() -> Vec<ColSpec> {
        vec![ColSpec::new("ks", "t", "k", ColType::Int), ColSpec::new("ks", "t", "v", ColType::list(ColType::Varchar))]
    }

    fn rows() -> Rows {
        let mut b = RowsBuilder::new();
        b.cell(Some(b"\x00\x00\x00\x01")).cell(None).end_row();
        b.cell(Some(b"\x00\x00\x00\x02")).cell(Some(b"")).end_row();
        let mut md = RowsMetadata::new(cols());
        md.paging_state = Some(Bytes::from_static(b"page"));
        b.finish(md)
    }

    fn round(r: &ResultBody, ctx: Ctx) -> ResultBody {
        let mut b = BytesMut::new();
        r.encode(ctx, &mut b);
        b.put_slice(b"trailing bytes are ignored");
        ResultBody::decode(ctx, &mut Reader::new(b.freeze())).unwrap()
    }

    fn all() -> Vec<ResultBody> {
        let mut changed = rows();
        changed.metadata.new_metadata_id = Some(Bytes::from_static(b"newid"));
        let mut mixed = RowsMetadata::new(vec![ColSpec::new("a", "t", "x", ColType::Int), ColSpec::new("b", "t", "y", ColType::Int)]);
        assert!(!mixed.global_tables_spec);
        mixed.paging_state = None;
        vec![
            ResultBody::Void,
            ResultBody::Rows(rows()),
            ResultBody::Rows(Rows::empty(mixed)),
            ResultBody::Rows(Rows { metadata: rows().metadata.skipped(), ..rows() }),
            ResultBody::Rows(changed),
            ResultBody::SetKeyspace("ks".into()),
            ResultBody::Prepared(Prepared {
                id: Bytes::from_static(&[9; 16]),
                result_metadata_id: Some(Bytes::from_static(&[8; 16])),
                metadata: PreparedMetadata::new(cols(), vec![0]),
                result_metadata: RowsMetadata::new(cols()),
            }),
            ResultBody::Prepared(Prepared {
                id: Bytes::from_static(&[9; 16]),
                result_metadata_id: Some(Bytes::from_static(&[8; 16])),
                metadata: PreparedMetadata { lwt: true, ..PreparedMetadata::new(vec![], vec![]) },
                result_metadata: RowsMetadata::empty(),
            }),
            ResultBody::SchemaChange(SchemaChange {
                change: SchemaChangeType::Created,
                target: SchemaTarget::Table,
                keyspace: "ks".into(),
                name: Some("t".into()),
                arg_types: None,
            }),
        ]
    }

    #[test]
    fn v5_round_trips() {
        for r in all() {
            assert_eq!(round(&r, Ctx::new(Version::V5)), r, "{r:?}");
        }
    }

    #[test]
    fn version_gates() {
        let v4 = Ctx::new(Version::V4);
        for r in all() {
            let back = round(&r, v4);
            match (&r, &back) {
                (ResultBody::Prepared(a), ResultBody::Prepared(b)) => {
                    assert_eq!(b.result_metadata_id, None);
                    assert_eq!(a.metadata, b.metadata);
                }
                (ResultBody::Rows(a), ResultBody::Rows(b)) if a.metadata.new_metadata_id.is_some() => {
                    assert_eq!(b.metadata.new_metadata_id, None, "METADATA_CHANGED is v5");
                    assert_eq!(a.content, b.content);
                }
                _ => assert_eq!(back, r),
            }
        }
        // ScyllaDB's extension carries metadata ids on v4.
        let scylla = v4.features(Features { metadata_id: true, ..Default::default() });
        for r in all() {
            assert_eq!(round(&r, scylla), r);
        }
        // v3 has no partition key indexes.
        let v3 = Ctx::new(Version::V3);
        let ResultBody::Prepared(p) = round(&all()[6], v3) else { panic!() };
        assert!(p.metadata.pk_indexes.is_empty());
    }

    #[test]
    fn prepared_matches_scylla_bytes() {
        // PREPARE "select key from system.local" against ScyllaDB 2026.3, v4.
        let body = b"\x00\x00\x00\x04\x00\x10\x90\x03\x7c\x2e\x32\xba\x1e\x69\x71\x8d\x4e\xf8\x45\x44\x0f\x56\
\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\
\x00\x00\x00\x01\x00\x00\x00\x01\x00\x06system\x00\x05local\x00\x03key\x00\x0d";
        let r = ResultBody::decode(Version::V4, &mut Reader::new(Bytes::from_static(body))).unwrap();
        let mut out = BytesMut::new();
        r.encode(Version::V4, &mut out);
        assert_eq!(&out[..], &body[..]);
        let ResultBody::Prepared(p) = r else { panic!() };
        assert_eq!(p.result_metadata.columns, vec![ColSpec::new("system", "local", "key", ColType::Varchar)]);
    }

    #[test]
    fn rows_iterate_without_copying() {
        let r = rows();
        let cells = r.to_vec();
        assert_eq!(cells.len(), 2);
        assert_eq!(cells[0][1], None);
        assert_eq!(cells[1][1], Some(Bytes::new()));
        let c = cells[0][0].as_ref().unwrap();
        let base = r.content.as_ptr() as usize;
        assert!((base..base + r.content.len()).contains(&(c.as_ptr() as usize)));
    }

    #[test]
    fn malformed_rows() {
        let enc = |r: &ResultBody| {
            let mut b = BytesMut::new();
            r.encode(Version::V5, &mut b);
            b.freeze()
        };
        for r in all() {
            let full = enc(&r);
            for cut in 0..full.len() {
                assert!(ResultBody::decode(Version::V5, &mut Reader::new(full.slice(..cut))).is_err(), "{r:?} cut {cut}");
            }
        }
        // Huge row count with no cells.
        let mut b = BytesMut::new();
        b.put_i32(kind::ROWS);
        RowsMetadata::new(cols()).encode(Ctx::new(Version::V4), &mut b);
        b.put_i32(i32::MAX);
        assert!(ResultBody::decode(Version::V4, &mut Reader::new(b.freeze())).is_err());
        // Zero columns, huge row count: valid, empty content, no work per row.
        let mut b = BytesMut::new();
        b.put_i32(kind::ROWS);
        RowsMetadata::new(vec![]).encode(Ctx::new(Version::V4), &mut b);
        b.put_i32(i32::MAX);
        let ResultBody::Rows(r) = ResultBody::decode(Version::V4, &mut Reader::new(b.freeze())).unwrap() else { panic!() };
        assert_eq!(r.rows().next().map(|c| c.count()), Some(0));
        // Negative counts and unknown kinds.
        for body in [&b"\x00\x00\x00\x02\x00\x00\x00\x00\xff\xff\xff\xff"[..], b"\x00\x00\x00\x09", b"\x00\x00\x00\x02\x00\x00\x00\x00\x00\x00\x00\x00\xff\xff\xff\xff"] {
            assert!(ResultBody::decode(Version::V4, &mut Reader::new(Bytes::copy_from_slice(body))).is_err());
        }
        // Column count far beyond the body.
        let body = b"\x00\x00\x00\x02\x00\x00\x00\x00\x7f\xff\xff\xff";
        assert!(matches!(
            ResultBody::decode(Version::V4, &mut Reader::new(Bytes::from_static(body))),
            Err(DecodeError::Truncated { .. })
        ));
    }
}
