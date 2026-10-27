//! The catalog: schemas, relations (tables, views, sequences, indexes), constraints and types.
//! It is one immutable value per version; DDL clones it, edits the clone inside the
//! transaction, and the commit stores the objects it changed (`persist`) with the
//! transaction's data, so DDL is transactional.

pub mod acl;
pub mod btsim;
pub mod depend;
pub mod ginsim;
pub mod gistsim;
pub mod hashidx;
pub mod heapsim;
pub mod jsonbin;
pub mod lz4;
pub mod persist;
pub mod pmap;
pub mod relindex;
pub mod pglz;
pub mod identify;
pub mod names;
pub mod partition;
pub mod pubsub;
pub mod relsize;
pub mod shared;
pub mod shdepend;
pub mod static_rows;
pub mod sys_rels;
pub mod sys_sizes;
pub mod sysdelta;
pub mod system;
pub mod tsearch;
pub mod type_procs;
pub mod userobj;

use crate::error::{Error, Result, code};
use crate::types::{DataType, Value};
use crate::{bail, err};
pub use partition::{PartBound, PartKey, PartKeyCol, PartStrategy, RangeDatum};
pub use shared::{Subscription, Tablespace};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

pub const PG_CATALOG_OID: u32 = 11;
pub const PUBLIC_OID: u32 = 2200;
pub const INFORMATION_SCHEMA_OID: u32 = 13327;
pub const PG_TOAST_OID: u32 = 99;
pub const FIRST_USER_OID: u32 = 16384;
pub const BOOTSTRAP_SUPERUSER_OID: u32 = 10;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Schema {
    pub oid: u32,
    pub name: String,
    pub owner: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Identity {
    Always,
    ByDefault,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub ty: DataType,
    pub not_null: bool,
    /// Default expression as SQL text (re-parsed when used).
    pub default: Option<String>,
    pub identity: Option<Identity>,
    /// Sequence backing a serial/identity column.
    pub seq: Option<u32>,
    /// `GENERATED ALWAYS AS (expr) [STORED | VIRTUAL]`
    pub generated: Option<String>,
    /// The generated column is VIRTUAL (computed when read). Its value is still kept in the
    /// row, but nothing outside reads it from there.
    #[serde(default)]
    pub virtual_gen: bool,
    /// Value for rows stored before this column was added.
    #[serde(default, with = "value_serde")]
    pub missing: Option<Value>,
    pub dropped: bool,
    pub collation: Option<String>,
    /// The domain this column was declared with (its `ty` is the domain's base type).
    #[serde(default)]
    pub domain: Option<u32>,
    /// Number of parents this column is inherited from (attinhcount).
    #[serde(default)]
    pub inhcount: u16,
    /// Defined locally, not only inherited (attislocal).
    #[serde(default = "yes")]
    pub islocal: bool,
    /// ALTER COLUMN SET STATISTICS (attstattarget; None: the default).
    #[serde(default)]
    pub stattarget: Option<i16>,
    /// ALTER COLUMN SET STORAGE (attstorage; None: the type's).
    #[serde(default)]
    pub storage: Option<char>,
    /// COMPRESSION pglz / lz4 (attcompression 'p' / 'l'; None: the default).
    #[serde(default)]
    pub compression: Option<char>,
    /// ALTER COLUMN SET (n_distinct = …) as `name=value` (attoptions).
    #[serde(default)]
    pub options: Vec<String>,
    /// attndims: the array dimensions the column was declared with (`int[][]`: 2); 0 when it
    /// was not declared with a type (CREATE TABLE AS, LIKE, inherited). None: from before
    /// this was kept.
    #[serde(default)]
    pub ndims: Option<i16>,
    /// The pg_attrdef oid of the default or generation expression (0: none, or not yet
    /// given one; `Catalog::put_table` gives it).
    #[serde(default)]
    pub def_oid: u32,
}

impl Column {
    pub fn new(name: impl Into<String>, ty: DataType) -> Self {
        Column {
            name: name.into(),
            ty,
            not_null: false,
            default: None,
            identity: None,
            seq: None,
            generated: None,
            virtual_gen: false,
            missing: None,
            dropped: false,
            collation: None,
            domain: None,
            inhcount: 0,
            islocal: true,
            stattarget: None,
            storage: None,
            compression: None,
            options: Vec::new(),
            ndims: Some(0),
            def_oid: 0,
        }
    }

    /// attndims.
    pub fn attndims(&self) -> i16 {
        self.ndims.unwrap_or(if self.ty.is_array() { 1 } else { 0 })
    }
}

/// Stored values in the catalog (missing values) as text plus type, so the JSON stays readable.
mod value_serde {
    use crate::storage::row;
    use crate::types::Value;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &Option<Value>, s: S) -> Result<S::Ok, S::Error> {
        match v {
            None => s.serialize_none(),
            Some(v) => {
                let mut out = Vec::new();
                row::encode_value(&mut out, v);
                let h: String = out.iter().map(|b| format!("{b:02x}")).collect();
                s.serialize_some(&h)
            }
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
        let h: Option<String> = Option::deserialize(d)?;
        match h {
            None => Ok(None),
            Some(h) => {
                let bytes: Vec<u8> = (0..h.len())
                    .step_by(2)
                    .map(|i| u8::from_str_radix(&h[i..i + 2], 16))
                    .collect::<Result<_, _>>()
                    .map_err(serde::de::Error::custom)?;
                let mut pos = 0;
                row::decode_value(&bytes, &mut pos)
                    .map(Some)
                    .map_err(serde::de::Error::custom)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum FkAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl ConstraintKind {
    /// The index enforcing a primary key, unique or exclusion constraint.
    pub fn index(&self) -> Option<u32> {
        match self {
            ConstraintKind::PrimaryKey { index }
            | ConstraintKind::Unique { index }
            | ConstraintKind::Exclusion { index, .. } => Some(*index),
            _ => None,
        }
    }
}

impl FkAction {
    pub fn code(&self) -> char {
        match self {
            FkAction::NoAction => 'a',
            FkAction::Restrict => 'r',
            FkAction::Cascade => 'c',
            FkAction::SetNull => 'n',
            FkAction::SetDefault => 'd',
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ConstraintKind {
    PrimaryKey {
        index: u32,
    },
    Unique {
        index: u32,
    },
    Check {
        expr: String,
        no_inherit: bool,
    },
    ForeignKey {
        columns: Vec<usize>,
        ref_table: u32,
        ref_columns: Vec<usize>,
        on_delete: FkAction,
        on_update: FkAction,
        match_full: bool,
        deferrable: bool,
        #[serde(default)]
        initially_deferred: bool,
    },
    NotNull {
        column: usize,
    },
    /// EXCLUDE: one operator per key column of the index.
    Exclusion {
        index: u32,
        ops: Vec<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Constraint {
    pub oid: u32,
    pub name: String,
    pub kind: ConstraintKind,
    pub validated: bool,
    /// DEFERRABLE / INITIALLY DEFERRED of index-backed constraints (foreign keys keep theirs
    /// in the kind).
    #[serde(default)]
    pub deferrable: bool,
    #[serde(default)]
    pub initially_deferred: bool,
    /// Number of parents this constraint is inherited from (coninhcount).
    #[serde(default)]
    pub inhcount: u16,
    /// Defined locally, not only inherited (conislocal).
    #[serde(default = "yes")]
    pub islocal: bool,
    /// The partitioned table's constraint this partition's one comes from (conparentid).
    #[serde(default)]
    pub parent: u32,
    /// Checked (conenforced); NOT ENFORCED check and foreign key constraints aren't.
    #[serde(default = "yes")]
    pub enforced: bool,
    /// NOT NULL ... NO INHERIT (check constraints keep theirs in the kind).
    #[serde(default)]
    pub no_inherit: bool,
    /// tgenabled of a foreign key's internal triggers, by trigger oid, where not 'O'
    /// (ALTER TABLE ... DISABLE TRIGGER ALL).
    #[serde(default)]
    pub ri_enabled: Vec<(u32, char)>,
    /// A foreign key referencing a partitioned table: the oid before its check triggers',
    /// which follow the constraints derived for the referenced partitions (0: the check
    /// triggers follow the action triggers).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub ri_check: u32,
    /// conperiod: a primary key ('p') or unique constraint ('u') WITHOUT OVERLAPS (kept as
    /// the exclusion constraint Postgres enforces it with: `=` per column, `&&` on the
    /// last), or a foreign key using PERIOD ('f').
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<char>,
    /// ON DELETE SET NULL / SET DEFAULT (columns): the foreign key's columns those actions
    /// set (confdelsetcols; empty: all of them).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub del_set_cols: Vec<usize>,
    /// A check constraint's `ROW(...)::composite` expressions: the composite type's column
    /// types when the constraint was made (what the RowExpr Postgres stores has its
    /// arguments coerced to), by type oid. Using the expression after the type's columns
    /// changed fails, as it does there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub row_types: Vec<(u32, Vec<DataType>)>,
    /// Made as a partition's copy of its parent's (index-backed or foreign key) constraint,
    /// rather than defined on its own and attached: connoinherit false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_parent: bool,
}

fn is_zero(v: &u32) -> bool {
    *v == 0
}

impl Constraint {
    pub fn new(oid: u32, name: String, kind: ConstraintKind, validated: bool) -> Self {
        Constraint {
            oid,
            name,
            kind,
            validated,
            deferrable: false,
            initially_deferred: false,
            inhcount: 0,
            islocal: true,
            parent: 0,
            enforced: true,
            no_inherit: false,
            ri_enabled: Vec::new(),
            ri_check: 0,
            period: None,
            del_set_cols: Vec::new(),
            row_types: Vec::new(),
            from_parent: false,
        }
    }

    /// conperiod: WITHOUT OVERLAPS, or a foreign key's PERIOD.
    pub fn is_period(&self) -> bool {
        self.period.is_some()
    }

    /// A primary key or unique constraint WITHOUT OVERLAPS (an exclusion constraint inside).
    pub fn without_overlaps(&self) -> bool {
        matches!(self.period, Some('p' | 'u'))
            && matches!(self.kind, ConstraintKind::Exclusion { .. })
    }

    /// (DEFERRABLE, INITIALLY DEFERRED).
    pub fn deferral(&self) -> (bool, bool) {
        match &self.kind {
            ConstraintKind::ForeignKey {
                deferrable,
                initially_deferred,
                ..
            } => (*deferrable, *deferrable && *initially_deferred),
            _ => (self.deferrable, self.deferrable && self.initially_deferred),
        }
    }

    pub fn contype(&self) -> char {
        match self.kind {
            ConstraintKind::PrimaryKey { .. } => 'p',
            ConstraintKind::Unique { .. } => 'u',
            ConstraintKind::Check { .. } => 'c',
            ConstraintKind::ForeignKey { .. } => 'f',
            ConstraintKind::NotNull { .. } => 'n',
            ConstraintKind::Exclusion { .. } => self
                .period
                .filter(|c| matches!(c, 'p' | 'u'))
                .unwrap_or('x'),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum IndexKey {
    Column(usize),
    /// An expression over the table's columns, as SQL text.
    Expr(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IndexColumn {
    pub key: IndexKey,
    pub desc: bool,
    pub nulls_first: bool,
    /// Explicit operator class oid (0: the type's default).
    #[serde(default)]
    pub opclass: u32,
    /// Explicit `COLLATE` name.
    #[serde(default)]
    pub collation: Option<String>,
    /// ALTER INDEX ... ALTER COLUMN n SET STATISTICS (expression columns).
    #[serde(default)]
    pub stattarget: Option<i16>,
    /// Operator class options (`tsvector_ops (siglen = 100)`), as `name=value`
    /// (pg_attribute.attoptions of the index column).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub opclass_options: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Index {
    pub oid: u32,
    pub name: String,
    pub table: u32,
    /// Prefix of this index's keys within the table's keyspace (0 is the primary data).
    pub id: u32,
    pub columns: Vec<IndexColumn>,
    pub include: Vec<usize>,
    pub unique: bool,
    pub primary: bool,
    pub nulls_not_distinct: bool,
    /// Partial index predicate (SQL).
    pub predicate: Option<String>,
    pub method: String,
    pub valid: bool,
    /// WITH (storage parameters), as `name=value`.
    #[serde(default)]
    pub options: Vec<String>,
    /// The partitioned index this partition's index is attached to (0: none).
    #[serde(default)]
    pub parent: u32,
    /// CLUSTER ... USING / ALTER TABLE ... CLUSTER ON marked it (pg_index.indisclustered).
    #[serde(default)]
    pub clustered: bool,
    /// REPLICA IDENTITY USING INDEX chose it (pg_index.indisreplident).
    #[serde(default)]
    pub replident: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum RelKind {
    Table,
    View,
    Sequence,
    Index,
    MaterializedView,
}

impl RelKind {
    pub fn relkind(&self) -> char {
        match self {
            RelKind::Table => 'r',
            RelKind::View => 'v',
            RelKind::Sequence => 'S',
            RelKind::Index => 'i',
            RelKind::MaterializedView => 'm',
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Table {
    pub oid: u32,
    pub schema: u32,
    pub name: String,
    pub owner: u32,
    pub columns: Vec<Column>,
    /// Primary key columns; without one rows are keyed by a hidden rowid.
    pub pkey: Option<Vec<usize>>,
    pub pkey_index: Option<u32>,
    /// The primary key is deferrable: its values may clash until it is checked, so rows are
    /// stored under the hidden rowid and the key's index has entries of its own (id != 0).
    #[serde(default)]
    pub pkey_unkeyed: bool,
    pub indexes: Vec<u32>,
    pub constraints: Vec<Constraint>,
    pub next_index_id: u32,
    /// Sequence used for hidden rowids.
    pub rowid_seq: Option<u32>,
    pub temporary: bool,
    pub unlogged: bool,
    pub comment: Option<String>,
    /// The row type's oid; 0 until the table is stored.
    #[serde(default)]
    pub rowtype: u32,
    /// The row type's array type's oid (0: the oid after the row type's, as catalogs from
    /// before this was kept had it).
    #[serde(default)]
    pub rowarray: u32,
    /// The TOAST table's oid (its index has the next one); 0: none.
    #[serde(default)]
    pub toast: u32,
    /// Its TOAST table's and TOAST index's names when renamed (ALTER TABLE / INDEX ...
    /// RENAME under allow_system_table_mods); None: `pg_toast_<oid>[_index]`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toast_relname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub toast_indname: Option<String>,
    /// ENABLE ROW LEVEL SECURITY / FORCE ROW LEVEL SECURITY.
    #[serde(default)]
    pub rls: bool,
    #[serde(default)]
    pub rls_force: bool,
    /// CREATE TABLE ... OF type: the composite type's oid (0: not a typed table).
    #[serde(default)]
    pub of_type: u32,
    /// INHERITS: the parent tables, in order (pg_inherits.inhseqno is the position + 1).
    #[serde(default)]
    pub inherits: Vec<u32>,
    /// PARTITION BY: a partitioned table, which stores no rows itself.
    #[serde(default)]
    pub partkey: Option<PartKey>,
    /// PARTITION OF / ATTACH PARTITION: the bound of a partition (its parent is `inherits[0]`).
    #[serde(default)]
    pub partbound: Option<PartBound>,
    /// WITH (storage parameters), as `name=value` (pg_class.reloptions).
    #[serde(default)]
    pub options: Vec<String>,
    /// REPLICA IDENTITY FULL / NOTHING / USING INDEX ('f', 'n', 'i'; None: DEFAULT).
    #[serde(default)]
    pub replident: Option<char>,
    /// CREATE FOREIGN TABLE: a foreign table (relkind 'f'), which has no storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub foreign: Option<ForeignTableDef>,
    /// DETACH PARTITION ... CONCURRENTLY marked it (pg_inherits.inhdetachpending): its first
    /// transaction committed, the second hasn't (yet). Queries whose snapshot sees the mark
    /// leave it out of the parent (`DetachView`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub detach_pending: bool,
}

/// A foreign table's pg_foreign_table row and its columns' attfdwoptions.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ForeignTableDef {
    /// ftserver
    pub server: u32,
    /// ftoptions, as `name=value` (None: NULL).
    pub options: Option<Vec<String>>,
    /// attfdwoptions: (column position, `name=value` options) of the columns that have some.
    #[serde(default)]
    pub col_options: Vec<(usize, Vec<String>)>,
}

impl ForeignTableDef {
    /// Column `col`'s FDW options.
    pub fn column_options(&self, col: usize) -> Option<&Vec<String>> {
        self.col_options.iter().find(|(c, _)| *c == col).map(|(_, o)| o)
    }

    pub fn set_column_options(&mut self, col: usize, opts: Option<Vec<String>>) {
        self.col_options.retain(|(c, _)| *c != col);
        if let Some(o) = opts.filter(|o| !o.is_empty()) {
            self.col_options.push((col, o));
            self.col_options.sort_by_key(|(c, _)| *c);
        }
    }
}

/// One pg_statistic row.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ColStats {
    pub attnum: i16,
    pub inherit: bool,
    pub nullfrac: f32,
    pub width: i32,
    pub distinct: f32,
    pub slots: Vec<StatSlot>,
}

/// A pg_statistic slot (stakindN, staopN, stacollN, stanumbersN, stavaluesN).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StatSlot {
    pub kind: i16,
    pub op: u32,
    pub coll: u32,
    pub numbers: Option<Vec<f32>>,
    /// The values' type and their text forms (default output style).
    pub values: Option<(DataType, Vec<String>)>,
}

thread_local! {
    /// The temporary schema (`pg_temp_N`) of the session running on this thread, which
    /// `pg_temp` names.
    static SESSION_TEMP_SCHEMA: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

/// The temporary schema `pg_temp` means on this thread (`set_session_temp_schema`).
pub fn session_temp_schema() -> Option<String> {
    SESSION_TEMP_SCHEMA.with(|t| t.borrow().clone())
}

/// Make `pg_temp` mean `schema` for lookups on this thread (the session about to run).
pub fn set_session_temp_schema(schema: &str) {
    SESSION_TEMP_SCHEMA.with(|t| {
        let mut t = t.borrow_mut();
        if t.as_deref() != Some(schema) {
            *t = Some(schema.to_string());
        }
    });
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct RelStats {
    pub pages: i32,
    pub tuples: f64,
    pub allvisible: i32,
    /// pg_class.relallfrozen.
    #[serde(default)]
    pub allfrozen: i32,
}

impl RelStats {
    /// A relation's pg_class statistics as created (before VACUUM, ANALYZE or a restore
    /// set them), by relkind.
    pub fn initial(kind: char) -> RelStats {
        let (pages, tuples) = match kind {
            'i' => (1, 0.0),
            'I' => (0, 0.0),
            'S' => (1, 1.0),
            _ => (0, -1.0),
        };
        RelStats {
            pages,
            tuples,
            ..RelStats::default()
        }
    }
}

/// A TOAST table's columns (its index has the first two).
pub const TOAST_COLUMNS: [&str; 3] = ["chunk_id", "chunk_seq", "chunk_data"];

/// The name of a table's TOAST table, or of its index.
pub fn toast_name(table: u32, index: bool) -> String {
    if index {
        format!("pg_toast_{table}_index")
    } else {
        format!("pg_toast_{table}")
    }
}

/// A row type's array type: `rowarray`, or for catalogs from before that was kept the oid
/// after the row type's.
fn array_of(rowtype: u32, rowarray: u32) -> u32 {
    match (rowtype, rowarray) {
        (0, _) => 0,
        (r, 0) => r + 1,
        (_, a) => a,
    }
}

impl Table {
    /// The name of its TOAST table, or (`index`) of the TOAST table's index.
    pub fn toast_relname(&self, index: bool) -> String {
        let given = if index { &self.toast_indname } else { &self.toast_relname };
        given.clone().unwrap_or_else(|| toast_name(self.oid, index))
    }

    /// The storage id (key prefix) of new index `oid` of this table: its oid, which no other
    /// transaction's index can have. (Ids were `next_index_id`, a per-table counter: two
    /// transactions making indexes of one table at once, as `pg_restore -j` does, took the
    /// same id, and the merged catalog had both indexes' entries under one prefix. The
    /// counter still counts, for the catalogs that have it.)
    pub fn new_index_id(&mut self, oid: u32) -> u32 {
        self.next_index_id += 1;
        oid
    }

    /// The row type's array type (0: no row type yet).
    pub fn array_type(&self) -> u32 {
        array_of(self.rowtype, self.rowarray)
    }

    /// pg_class.relkind: 'f' a foreign table, 'p' a partitioned table, else 'r'.
    pub fn relkind(&self) -> char {
        if self.foreign.is_some() {
            'f'
        } else if self.partkey.is_some() {
            'p'
        } else {
            'r'
        }
    }

    pub fn is_foreign(&self) -> bool {
        self.foreign.is_some()
    }


    pub fn live_columns(&self) -> impl Iterator<Item = (usize, &Column)> {
        self.columns.iter().enumerate().filter(|(_, c)| !c.dropped)
    }

    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns
            .iter()
            .position(|c| !c.dropped && c.name == name)
    }

    pub fn column(&self, name: &str) -> Result<(usize, &Column)> {
        match self.column_index(name) {
            Some(i) => Ok((i, &self.columns[i])),
            None => Err(err!(
                UNDEFINED_COLUMN,
                "column \"{name}\" of relation \"{}\" does not exist",
                self.name
            )),
        }
    }

    /// Whether the table gets a TOAST table (heapam_relation_needs_toast_table): it has a
    /// column that may be stored out of line, and its rows may outgrow TOAST_TUPLE_THRESHOLD.
    pub fn needs_toast(&self) -> bool {
        if self.partkey.is_some() || self.foreign.is_some() {
            return false;
        }
        let (mut len, mut unbounded, mut toastable) = (0i64, false, false);
        for c in self.columns.iter().filter(|c| !c.dropped && !c.virtual_gen) {
            let align = match system::align(&c.ty) {
                'c' => 1,
                's' => 2,
                'd' => 8,
                _ => 4,
            };
            len = (len + align - 1) / align * align;
            if c.ty.typlen() > 0 {
                len += c.ty.typlen() as i64;
                continue;
            }
            // type_maximum_size: bounded by the type modifier (a domain's isn't the column's).
            let m = c.ty.typmod() as i64;
            let max = match &c.ty {
                _ if c.domain.is_some() => None,
                DataType::Varchar(_) | DataType::Bpchar(_) if m >= 4 => Some((m - 4) * 4 + 4),
                DataType::Bit(_) | DataType::Varbit(_) if m >= 0 => Some((m + 7) / 8 + 8),
                DataType::Numeric(_) if m >= 4 => {
                    Some(8 + ((((m - 4) >> 16) & 0xffff) + 6) / 4 * 2)
                }
                _ => None,
            };
            match max {
                Some(m) => len += m,
                None => unbounded = true,
            }
            toastable |= c.storage.unwrap_or_else(|| system::typstorage(&c.ty)) != 'p';
        }
        let maxalign = |n: i64| (n + 7) / 8 * 8;
        toastable
            && (unbounded
                || maxalign(23 + (self.columns.len() as i64 + 7) / 8) + maxalign(len) > 2032)
    }

    pub fn has_rowid(&self) -> bool {
        self.storage_pkey().is_none()
    }

    /// The columns rows are stored by: the primary key's, unless it is deferrable.
    pub fn storage_pkey(&self) -> Option<&Vec<usize>> {
        self.pkey.as_ref().filter(|_| !self.pkey_unkeyed)
    }

    /// WITH (roll_pk_clustered = true): roll's own storage parameter (see `heap_options`),
    /// rows keyed by the primary key rather than stored in heap order under a row id.
    pub fn pk_clustered(&self) -> bool {
        pk_clustered_option(&self.options)
    }
}

/// roll's storage parameter for the primary-key-clustered layout (not Postgres's).
pub const PK_CLUSTERED: &str = "roll_pk_clustered";

/// Whether reloptions (`name=value`) turn `roll_pk_clustered` on.
pub fn pk_clustered_option(options: &[String]) -> bool {
    options.iter().any(|o| {
        o.split_once('=').is_some_and(|(k, v)| {
            k == PK_CLUSTERED
                && crate::types::text::parse_bool(v.trim_matches('\'')).unwrap_or(false)
        })
    })
}

impl Table {

    /// The check constraints that are enforced.
    pub fn enforced_checks(&self) -> impl Iterator<Item = (&str, &str)> {
        self.constraints
            .iter()
            .filter(|c| c.enforced)
            .filter_map(|c| match &c.kind {
                ConstraintKind::Check { expr, .. } => Some((c.name.as_str(), expr.as_str())),
                _ => None,
            })
    }

    pub fn checks(&self) -> impl Iterator<Item = (&str, &str)> {
        self.constraints.iter().filter_map(|c| match &c.kind {
            ConstraintKind::Check { expr, .. } => Some((c.name.as_str(), expr.as_str())),
            _ => None,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ViewDef {
    pub oid: u32,
    pub schema: u32,
    pub name: String,
    pub owner: u32,
    /// The defining query, as SQL text.
    pub query: String,
    pub columns: Vec<(String, DataType)>,
    pub materialized: bool,
    pub check_option: Option<String>,
    /// Materialized views: the stored rows, kept as a table (same oid as the view) that is not
    /// visible by name.
    #[serde(default)]
    pub storage: Option<Arc<Table>>,
    /// Relations the query reads (for DROP / ALTER dependency checks).
    #[serde(default)]
    pub deps: Vec<u32>,
    /// The row type's oid; 0 until the view is stored.
    #[serde(default)]
    pub rowtype: u32,
    /// The row type's array type's oid (0: the oid after the row type's, as catalogs from
    /// before this was kept had it).
    #[serde(default)]
    pub rowarray: u32,
    /// The `_RETURN` rule's pg_rewrite oid (0: from before this was kept).
    #[serde(default)]
    pub rule: u32,
    /// WITH (security_invoker = …, security_barrier = …) as `name=value`.
    #[serde(default)]
    pub options: Vec<String>,
    /// Materialized views: false after WITH NO DATA (pg_class.relispopulated).
    #[serde(default = "yes")]
    pub populated: bool,
    /// Per column: the domain (or array of one) its values have.
    #[serde(default)]
    pub domains: Vec<Option<u32>>,    /// Per column: the collation the query derives, when not the type's default.
    #[serde(default)]
    pub collations: Vec<Option<String>>,
    /// Per column: ALTER VIEW ... ALTER COLUMN ... SET DEFAULT's expression (SQL).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub defaults: Vec<Option<String>>,
    /// Per column: the pg_attrdef oid of its default (0: none).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub default_oids: Vec<u32>,
    /// The columns of FROM functions' named composite results the query reads, as they
    /// were when the view was made: (the composite type's relation, attnum, name, type).
    /// Postgres's stored query holds them by number and type, so a column dropped or
    /// retyped since (with the dependency that prevents it removed by hand) fails when
    /// the view is used, rather than resolving afresh.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rowtype_cols: Vec<(u32, i16, String, DataType)>,
}

impl ViewDef {
    /// The view's row shape as a table (for its triggers' OLD / NEW).
    pub fn as_table(&self) -> Arc<Table> {
        Arc::new(Table {
            oid: self.oid,
            schema: self.schema,
            name: self.name.clone(),
            owner: self.owner,
            columns: self
                .columns
                .iter()
                .enumerate()
                .map(|(i, (n, ty))| {
                    let mut c = Column::new(n.clone(), ty.clone());
                    c.default = self.defaults.get(i).cloned().flatten();
                    c
                })
                .collect(),
            pkey: None,
            pkey_index: None,
            pkey_unkeyed: false,
            indexes: Vec::new(),
            constraints: Vec::new(),
            next_index_id: 0,
            rowid_seq: None,
            temporary: false,
            unlogged: false,
            comment: None,
            rowtype: self.rowtype,
            rowarray: self.rowarray,
            toast: 0,
            toast_relname: None,
            toast_indname: None,
            rls: false,
            rls_force: false,
            of_type: 0,
            inherits: Vec::new(),
            partkey: None,
            partbound: None,
            options: Vec::new(),
            replident: None,
            foreign: None,
            detach_pending: false,
        })
    }

    /// The row type's array type (0: no row type yet).
    pub fn array_type(&self) -> u32 {
        array_of(self.rowtype, self.rowarray)
    }

    /// The `_RETURN` rule's pg_rewrite oid.
    pub fn rule_oid(&self) -> u32 {
        if self.rule != 0 {
            self.rule
        } else {
            depend::rule_oid(self.oid)
        }
    }

    /// A boolean view option (`security_invoker`).
    pub fn option(&self, name: &str) -> bool {
        // (`WITH (security_barrier)`: a bare name is true.)
        self.options.iter().any(|o| {
            o == name
                || o.split_once('=')
                    .is_some_and(|(k, v)| k == name && matches!(v, "true" | "on" | "1" | "yes"))
        })
    }
}

/// Selectivity estimators (pg_proc oid, name) an operator's RESTRICT / JOIN may name; the
/// join ones end in `joinsel`.
impl Catalog {
    /// The function a RESTRICT / JOIN estimator name stands for: a built-in one, or a user
    /// function of the estimator's signature (0: none).
    pub fn estimator_oid(&self, name: &str, join: bool) -> u32 {
        if let Some(e) = ESTIMATORS.iter().find(|e| e.1 == name) {
            return e.0;
        }
        self.user_estimator(name, join).map_or(0, |f| f.oid)
    }

    /// A user function named `name` with a restriction (join) estimator's arguments.
    pub fn user_estimator(&self, name: &str, join: bool) -> Option<&Arc<Function>> {
        let want: &[u32] = if join {
            &[2281, 26, 2281, 21, 2281]
        } else {
            &[2281, 26, 2281, 23]
        };
        self.functions.values().find(|f| {
            f.name == name
                && f.args.len() == want.len()
                && f.args.iter().zip(want).all(|(a, w)| a.1.oid() == *w)
        })
    }
}

pub const ESTIMATORS: &[(u32, &str)] = &[
    (140, "areajoinsel"),
    (139, "areasel"),
    (3818, "arraycontjoinsel"),
    (3817, "arraycontsel"),
    (1303, "contjoinsel"),
    (1302, "contsel"),
    (105, "eqjoinsel"),
    (101, "eqsel"),
    (1816, "iclikejoinsel"),
    (1814, "iclikesel"),
    (1817, "icnlikejoinsel"),
    (1815, "icnlikesel"),
    (1826, "icregexeqjoinsel"),
    (1820, "icregexeqsel"),
    (1829, "icregexnejoinsel"),
    (1823, "icregexnesel"),
    (1825, "likejoinsel"),
    (1819, "likesel"),
    (5041, "matchingjoinsel"),
    (5040, "matchingsel"),
    (4243, "multirangesel"),
    (106, "neqjoinsel"),
    (102, "neqsel"),
    (3561, "networkjoinsel"),
    (3560, "networksel"),
    (1828, "nlikejoinsel"),
    (1822, "nlikesel"),
    (1301, "positionjoinsel"),
    (1300, "positionsel"),
    (3438, "prefixjoinsel"),
    (3437, "prefixsel"),
    (3169, "rangesel"),
    (1824, "regexeqjoinsel"),
    (1818, "regexeqsel"),
    (1827, "regexnejoinsel"),
    (1821, "regexnesel"),
    (398, "scalargejoinsel"),
    (337, "scalargesel"),
    (108, "scalargtjoinsel"),
    (104, "scalargtsel"),
    (386, "scalarlejoinsel"),
    (336, "scalarlesel"),
    (107, "scalarltjoinsel"),
    (103, "scalarltsel"),
    (3687, "tsmatchjoinsel"),
    (3686, "tsmatchsel"),
];

/// CREATE OPERATOR (pg_operator). A shell operator (made by naming it as another operator's
/// commutator or negator) has no function yet.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Operator {
    pub oid: u32,
    pub name: String,
    pub schema: u32,
    pub owner: u32,
    /// None: a prefix operator.
    pub left: Option<DataType>,
    pub right: DataType,
    /// The implementing function (a user function's or a built-in's pg_proc oid; 0 for a
    /// shell) and its result type.
    pub func: u32,
    pub result: Option<DataType>,
    pub commutator: u32,
    pub negator: u32,
    pub restrict: Option<String>,
    pub join: Option<String>,
    pub hashes: bool,
    pub merges: bool,
}

/// CREATE STATISTICS: an extended statistics object (pg_statistic_ext).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StatExt {
    pub oid: u32,
    pub name: String,
    pub schema: u32,
    pub owner: u32,
    pub table: u32,
    /// Plain column keys (column indexes), ascending.
    pub columns: Vec<usize>,
    /// Expressions, as SQL over the table's columns.
    pub exprs: Vec<String>,
    /// Kinds named in the statement ('d' ndistinct, 'f' dependencies, 'm' mcv); empty: all.
    pub kinds: Vec<char>,
    /// ALTER STATISTICS ... SET STATISTICS (None: the default).
    pub stattarget: Option<i16>,
    /// What ANALYZE built (pg_statistic_ext_data), for the table alone and with its children.
    #[serde(default)]
    pub data: Vec<StatExtData>,
}

/// One pg_statistic_ext_data row: the serialized statistics of each kind built.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StatExtData {
    pub inherit: bool,
    pub ndistinct: Option<Vec<u8>>,
    pub dependencies: Option<Vec<u8>>,
    pub mcv: Option<Vec<u8>>,
    /// Each expression's pg_statistic row (None: not computed).
    pub exprs: Option<Vec<Option<ColStats>>>,
}

impl StatExt {
    /// pg_statistic_ext.stxkind: the kinds built, plus 'e' for expressions.
    pub fn stxkind(&self) -> Vec<char> {
        let single_expr = self.columns.is_empty() && self.exprs.len() == 1;
        let mut out: Vec<char> = if single_expr {
            Vec::new()
        } else if self.kinds.is_empty() {
            vec!['d', 'f', 'm']
        } else {
            ['d', 'f', 'm']
                .into_iter()
                .filter(|k| self.kinds.contains(k))
                .collect()
        };
        if !self.exprs.is_empty() {
            out.push('e');
        }
        out
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Sequence {
    pub oid: u32,
    pub schema: u32,
    pub name: String,
    pub owner: u32,
    pub ty: DataType,
    pub start: i64,
    pub increment: i64,
    pub min: i64,
    pub max: i64,
    pub cache: i64,
    pub cycle: bool,
    pub owned_by: Option<(u32, usize)>,
    /// ALTER SEQUENCE SET [UN]LOGGED / CREATE UNLOGGED SEQUENCE: its own persistence (None:
    /// as its owning table's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlogged: Option<bool>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EnumType {
    pub oid: u32,
    pub array_oid: u32,
    pub schema: u32,
    pub name: String,
    pub labels: Vec<String>,
    #[serde(default = "bootstrap_owner")]
    pub owner: u32,
    /// pg_enum oids of the labels (empty: from before labels had oids).
    #[serde(default)]
    pub label_oids: Vec<u32>,
    /// enumsortorder of the labels (empty: 1, 2, …).
    #[serde(default)]
    pub sort: Vec<f32>,
}

impl EnumType {
    /// The enum as a column / value type.
    pub fn data_type(&self) -> DataType {
        DataType::Enum(self.oid, Arc::from(self.name.as_str()), self.array_oid)
    }

    /// Label `i`'s pg_enum oid.
    pub fn label_oid(&self, i: usize) -> u32 {
        self.label_oids
            .get(i)
            .copied()
            .unwrap_or_else(|| self.oid.wrapping_mul(256).wrapping_add(i as u32) | 0x2000_0000)
    }

    /// Label `i`'s enumsortorder.
    pub fn sort_order(&self, i: usize) -> f32 {
        self.sort.get(i).copied().unwrap_or(i as f32 + 1.0)
    }

    /// Give labels from before labels had oids theirs (as CREATE TYPE would have).
    fn fill_label_oids(&mut self, cat: &mut Catalog) {
        if self.label_oids.len() != self.labels.len() {
            self.label_oids = (0..self.labels.len())
                .map(|_| cat.alloc_even_oid())
                .collect();
        }
        if self.sort.len() != self.labels.len() {
            self.sort = (1..=self.labels.len()).map(|i| i as f32).collect();
        }
    }

    /// ALTER TYPE ADD VALUE `label` at position `at`, its oid and sort order chosen as Postgres
    /// does: an even oid when that sorts right among the even ones (comparisons then go by
    /// oid), an odd one otherwise; the sort order halfway between the neighbours.
    pub fn add_label(&mut self, cat: &mut Catalog, at: usize, label: String) {
        self.fill_label_oids(cat);
        if at > 0 && at < self.labels.len() {
            let mid = (self.sort[at - 1] + self.sort[at]) / 2.0;
            if mid == self.sort[at - 1] || mid == self.sort[at] {
                self.sort = (1..=self.labels.len()).map(|i| i as f32).collect();
            }
        }
        let order = match at {
            0 if self.labels.is_empty() => 1.0,
            0 => self.sort[0] - 1.0,
            n if n == self.labels.len() => self.sort[n - 1] + 1.0,
            n => (self.sort[n - 1] + self.sort[n]) / 2.0,
        };
        let oid = loop {
            let o = cat.alloc_oid();
            let sorts_ok = self
                .label_oids
                .iter()
                .zip(&self.sort)
                .filter(|(x, _)| *x & 1 == 0)
                .all(|(x, s)| if *s < order { *x < o } else { *x > o });
            if sorts_ok == (o & 1 == 0) {
                break o;
            }
        };
        self.labels.insert(at, label);
        self.label_oids.insert(at, oid);
        self.sort.insert(at, order);
    }
}

/// CREATE TYPE ... AS RANGE: a range type and its multirange.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RangeDef {
    pub oid: u32,
    pub array_oid: u32,
    pub multi_oid: u32,
    pub multi_array_oid: u32,
    pub schema: u32,
    pub name: String,
    pub multi_name: String,
    pub owner: u32,
    pub subtype: DataType,
    pub subdiff: Option<String>,
    pub canonical: Option<String>,
    pub collation: u32,
    /// The collation's definition when it isn't a built-in one (`RangeType::coll_def`).
    #[serde(default)]
    pub coll_def: Option<crate::types::collation::CollationDef>,
    /// The constructor functions' names: the type names at creation (renames keep them).
    #[serde(default)]
    pub ctors: (String, String),
    /// pg_proc oids of the constructors (two of the range, three of the multirange), then
    /// pg_cast's of the range → multirange cast.
    #[serde(default)]
    pub fn_oids: Vec<u32>,
    /// The domain the subtype is declared as (`subtype` is its base type).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype_domain: Option<u32>,
    /// SUBTYPE_OPCLASS, when given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtype_opclass: Option<String>,
    /// SUBTYPE_OPCLASS's pg_opclass oid (rngsubopc; 0: the subtype's default btree class).
    #[serde(default)]
    pub subtype_opc: u32,
}

impl Catalog {
    /// The schema of range `r`'s multirange type.
    pub fn multirange_schema(&self, r: &RangeDef) -> u32 {
        self.multirange_schemas
            .get(&r.multi_oid)
            .copied()
            .unwrap_or(r.schema)
    }
}

impl RangeDef {
    pub fn range_type(&self) -> Arc<crate::types::range::RangeType> {
        Arc::new(crate::types::range::RangeType {
            oid: self.oid,
            array_oid: self.array_oid,
            multi_oid: self.multi_oid,
            multi_array_oid: self.multi_array_oid,
            name: self.name.as_str().into(),
            multi_name: self.multi_name.as_str().into(),
            schema: self.schema,
            subtype: self.subtype.clone(),
            canonical: self.canonical.as_deref().map(Into::into),
            subdiff: self.subdiff.as_deref().map(Into::into),
            collation: self.collation,
            coll_def: self.coll_def.clone(),
            subtype_domain: self.subtype_domain,
            subtype_opclass: self.subtype_opclass.as_deref().map(Into::into),
        })
    }
}

/// CREATE TYPE ... AS (...): a composite type (with a pg_class entry of relkind 'c').
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompositeType {
    pub oid: u32,
    pub array_oid: u32,
    /// Its pg_class / pg_attribute relation.
    pub relid: u32,
    pub schema: u32,
    pub name: String,
    pub owner: u32,
    pub fields: Vec<(String, DataType)>,
    /// attndims of the fields declared with other than one array dimension per array.
    #[serde(default)]
    pub dims: BTreeMap<String, i16>,
    /// Fields declared with a COLLATE.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub collations: BTreeMap<String, String>,
}

impl CompositeType {
    /// attndims of field `name` of type `ty`.
    pub fn attndims(&self, name: &str, ty: &DataType) -> i16 {
        self.dims
            .get(name)
            .copied()
            .unwrap_or(if ty.is_array() { 1 } else { 0 })
    }

    /// Record field `name`'s declared dimensions.
    pub fn set_dims(&mut self, name: &str, ty: &DataType, n: i16) {
        if n == if ty.is_array() { 1 } else { 0 } {
            self.dims.remove(name);
        } else {
            self.dims.insert(name.to_string(), n);
        }
    }
}

impl CompositeType {
    pub fn data_type(&self) -> DataType {
        DataType::Row(Arc::new(crate::types::RowType {
            oid: self.oid,
            array_oid: self.array_oid,
            name: Some(Arc::from(self.name.as_str())),
            fields: self.fields.clone(),
            colls: Vec::new(),
        }))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Domain {
    pub oid: u32,
    pub schema: u32,
    pub name: String,
    pub base: DataType,
    pub not_null: bool,
    pub default: Option<String>,
    pub checks: Vec<(String, String)>,
    #[serde(default = "bootstrap_owner")]
    pub owner: u32,
    /// The NOT NULL constraint's name (`<domain>_not_null` unless named).
    #[serde(default)]
    pub not_null_name: Option<String>,
    /// CHECK constraints added NOT VALID (and not validated since).
    #[serde(default)]
    pub invalid: Vec<String>,
    /// The domain this one is declared over (whose constraints apply too).
    #[serde(default)]
    pub parent: Option<u32>,
    /// The array-of-domain type this one is declared over (`CREATE DOMAIN d AS posint[]`):
    /// its base type's identity, which `base` (the erased `integer[]`) loses.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_domain: Option<u32>,
    /// The array type `_name` (0: none).
    #[serde(default)]
    pub array_oid: u32,
    /// pg_constraint oids of the constraints, by name.
    #[serde(default)]
    pub con_oids: Vec<(String, u32)>,
    /// `COLLATE` (typcollation), when given or taken from the base domain.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collation: Option<String>,
}

impl Domain {
    /// The constraints' names: NOT NULL's, then the checks'.
    pub fn constraint_names(&self) -> impl Iterator<Item = String> + '_ {
        let nn = self.not_null.then(|| {
            self.not_null_name
                .clone()
                .unwrap_or_else(|| format!("{}_not_null", self.name))
        });
        nn.into_iter()
            .chain(self.checks.iter().map(|c| c.0.clone()))
    }

    /// Give constraints added since the last call an oid; forget dropped ones'.
    pub fn assign_con_oids(&mut self, cat: &mut Catalog) {
        let names: Vec<String> = self.constraint_names().collect();
        self.con_oids.retain(|x| names.contains(&x.0));
        for n in names {
            if !self.con_oids.iter().any(|x| x.0 == n) {
                self.con_oids.push((n, cat.alloc_oid()));
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Function {
    pub oid: u32,
    pub schema: u32,
    pub name: String,
    pub args: Vec<(Option<String>, DataType)>,
    pub ret: DataType,
    pub returns_set: bool,
    pub language: String,
    pub body: String,
    pub volatility: char,
    pub strict: bool,
    /// Per input argument: its DEFAULT expression.
    #[serde(default)]
    pub defaults: Vec<Option<String>>,
    /// OUT / TABLE columns (names and types) for functions returning rows.
    #[serde(default)]
    pub out_cols: Vec<(String, DataType)>,
    #[serde(default = "bootstrap_owner")]
    pub owner: u32,
    /// SECURITY DEFINER: runs as its owner.
    #[serde(default)]
    pub security_definer: bool,
    /// A procedure (CREATE PROCEDURE, run with CALL).
    #[serde(default)]
    pub procedure: bool,
    /// Every parameter's mode in declaration order (`i`n, `o`ut, in`b`oth, `v`ariadic,
    /// `t`able); empty
    /// for input parameters only.
    #[serde(default)]
    pub modes: Vec<char>,
    /// An aggregate (CREATE AGGREGATE; prokind 'a'): how it runs.
    #[serde(default)]
    pub agg: Option<Box<AggSpec>>,
    /// COST; 0: the language's default (1 for internal / C, else 100).
    #[serde(default)]
    pub cost: f32,
    /// ROWS of a set-returning function; 0: the default (1000).
    #[serde(default)]
    pub rows: f32,
    /// PARALLEL `s`afe, `r`estricted or `u`nsafe.
    #[serde(default = "parallel_unsafe")]
    pub parallel: char,
    #[serde(default)]
    pub leakproof: bool,
    /// SET clauses, `name=value`, in effect while it runs (proconfig).
    #[serde(default)]
    pub config: Vec<String>,
    /// Per input parameter (as `args`): the domain (or array of one) it was declared with.
    #[serde(default)]
    pub arg_domains: Vec<Option<u32>>,
    /// The domain (or array of one) the result was declared as.
    #[serde(default)]
    pub ret_domain: Option<u32>,
    /// Per output parameter (as `out_cols`): the domain (or array of one) it was declared with.
    #[serde(default)]
    pub out_domains: Vec<Option<u32>>,
    /// A SQL-standard body (prosqlbody): 'r' `RETURN expr`, 'a' `BEGIN ATOMIC ... END`.
    #[serde(default)]
    pub sql_body: Option<char>,
    /// The statements of a `BEGIN ATOMIC` body that are `RETURN expr` (kept as `SELECT expr`).
    #[serde(default)]
    pub atomic_returns: Vec<usize>,
    /// A C function's library (probin); `body` is its symbol (prosrc).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probin: Option<String>,
    /// The planner support function (prosupport; 0: none).
    #[serde(default)]
    pub support: u32,
    /// WINDOW (prokind 'w').
    #[serde(default)]
    pub window: bool,
    /// TRANSFORM FOR TYPE ... (protrftypes).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub trftypes: Vec<u32>,
}

fn parallel_unsafe() -> char {
    'u'
}

impl Function {
    /// The domain input parameter `i` was declared with.
    pub fn arg_domain(&self, i: usize) -> Option<u32> {
        self.arg_domains.get(i).copied().flatten()
    }

    /// The same input parameter types (domains told apart from their base types).
    pub fn same_params(&self, other: &Function) -> bool {
        self.args.len() == other.args.len()
            && (0..self.args.len()).all(|i| {
                self.args[i].1 == other.args[i].1 && self.arg_domain(i) == other.arg_domain(i)
            })
    }

    /// COST as pg_proc has it.
    pub fn procost(&self) -> f32 {
        match self.cost {
            c if c > 0.0 => c,
            _ if matches!(self.language.as_str(), "internal" | "c") => 1.0,
            _ => 100.0,
        }
    }

    /// ROWS as pg_proc has it.
    pub fn prorows(&self) -> f32 {
        match self.rows {
            _ if !self.returns_set => 0.0,
            r if r > 0.0 => r,
            _ => 1000.0,
        }
    }
}

/// An aggregate's pg_aggregate entry. Support functions are oids (0: none), of user
/// functions or built-in ones.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AggSpec {
    /// `n`ormal, `o`rdered-set or `h`ypothetical-set.
    pub kind: char,
    pub num_direct: i16,
    pub transfn: u32,
    pub finalfn: u32,
    pub combinefn: u32,
    pub serialfn: u32,
    pub deserialfn: u32,
    pub mtransfn: u32,
    pub minvtransfn: u32,
    pub mfinalfn: u32,
    pub finalextra: bool,
    pub mfinalextra: bool,
    pub finalmodify: char,
    pub mfinalmodify: char,
    pub sortop: u32,
    pub transtype: DataType,
    pub transspace: i32,
    pub mtranstype: Option<DataType>,
    pub mtransspace: i32,
    pub initval: Option<String>,
    pub minitval: Option<String>,
}

/// Trigger functions built into Postgres (in C): (oid, name).
const BUILTIN_TRIGGERS: &[(u32, &str)] = &[
    (1291, "suppress_redundant_updates_trigger"),
    (3752, "tsvector_update_trigger"),
    (3753, "tsvector_update_trigger_column"),
];

impl Function {
    /// A built-in trigger function by oid or name (language `internal`).
    pub fn builtin_trigger(oid: u32, name: &str) -> Option<Arc<Function>> {
        let &(oid, name) = BUILTIN_TRIGGERS
            .iter()
            .find(|(o, n)| *o == oid || *n == name)?;
        Some(Arc::new(Function {
            oid,
            schema: PG_CATALOG_OID,
            name: name.to_string(),
            args: Vec::new(),
            ret: DataType::Trigger,
            returns_set: false,
            language: "internal".into(),
            body: name.to_string(),
            volatility: 'v',
            strict: false,
            defaults: Vec::new(),
            out_cols: Vec::new(),
            owner: BOOTSTRAP_SUPERUSER_OID,
            security_definer: false,
            procedure: false,
            modes: Vec::new(),
            agg: None,
            cost: 0.0,
            rows: 0.0,
            parallel: 's',
            leakproof: false,
            config: Vec::new(),
            arg_domains: Vec::new(),
            ret_domain: None,
            out_domains: Vec::new(),
            sql_body: None,
            atomic_returns: Vec::new(),
            probin: None,
            support: 0,
            window: false,
            trftypes: Vec::new(),
        }))
    }

    /// The last input parameter is VARIADIC.
    pub fn is_variadic(&self) -> bool {
        self.modes.contains(&'v')
    }

    /// All parameters in declaration order, OUT ones included: (name, type, mode).
    pub fn all_params(&self) -> Vec<(Option<String>, DataType, char)> {
        if self.modes.is_empty() {
            return self
                .args
                .iter()
                .map(|(n, t)| (n.clone(), t.clone(), 'i'))
                .collect();
        }
        let (mut ins, mut outs) = (self.args.iter(), self.out_cols.iter());
        let mut all = Vec::with_capacity(self.modes.len());
        for &m in &self.modes {
            match m {
                'i' | 'v' => all.extend(ins.next().map(|(n, t)| (n.clone(), t.clone(), m))),
                'b' => {
                    outs.next();
                    all.extend(ins.next().map(|(n, t)| (n.clone(), t.clone(), m)));
                }
                _ => all.extend(
                    outs.next()
                        .map(|(n, t)| ((!n.is_empty()).then(|| n.clone()), t.clone(), m)),
                ),
            }
        }
        all
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Role {
    pub oid: u32,
    pub name: String,
    pub superuser: bool,
    pub login: bool,
    pub create_db: bool,
    pub create_role: bool,
    /// SCRAM-SHA-256 verifier, as stored in pg_authid.rolpassword.
    pub password: Option<String>,
    #[serde(default = "yes")]
    pub inherit: bool,
    #[serde(default)]
    pub replication: bool,
    #[serde(default)]
    pub bypass_rls: bool,
    #[serde(default = "no_limit")]
    pub conn_limit: i32,
    /// VALID UNTIL, as a timestamptz (microseconds since 2000-01-01).
    #[serde(default)]
    pub valid_until: Option<i64>,
}

fn yes() -> bool {
    true
}

fn no_limit() -> i32 {
    -1
}

fn bootstrap_owner() -> u32 {
    BOOTSTRAP_SUPERUSER_OID
}

impl Role {
    pub fn new(oid: u32, name: impl Into<String>) -> Role {
        Role {
            oid,
            name: name.into(),
            superuser: false,
            login: false,
            create_db: false,
            create_role: false,
            password: None,
            inherit: true,
            replication: false,
            bypass_rls: false,
            conn_limit: -1,
            valid_until: None,
        }
    }
}

/// Settings applied when `role` (0: any) connects to `database` (None: any).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RoleSetting {
    pub role: u32,
    pub database: Option<String>,
    /// (name, value)
    pub settings: Vec<(String, String)>,
}

/// A row-level security policy (CREATE POLICY).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Policy {
    pub oid: u32,
    pub name: String,
    pub table: u32,
    /// '*' ALL, 'r' SELECT, 'a' INSERT, 'w' UPDATE, 'd' DELETE.
    pub cmd: char,
    pub permissive: bool,
    /// Roles it applies to; 0 is PUBLIC.
    pub roles: Vec<u32>,
    /// USING and WITH CHECK, as SQL over the table's columns.
    pub qual: Option<String>,
    pub with_check: Option<String>,
}

/// A rewrite rule (CREATE RULE) on INSERT / UPDATE / DELETE; a view's `_RETURN` rule is its
/// `ViewDef`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Rule {
    pub oid: u32,
    pub name: String,
    /// pg_rewrite.ev_class.
    pub table: u32,
    /// pg_rewrite.ev_type: '2' UPDATE, '3' INSERT, '4' DELETE.
    pub event: char,
    /// pg_rewrite.ev_enabled: 'O' origin, 'D' disabled, 'R' replica, 'A' always.
    pub enabled: char,
    pub instead: bool,
    /// The WHERE condition, as SQL over OLD / NEW.
    pub qual: Option<String>,
    /// The actions, as SQL statements (none: NOTHING).
    pub actions: Vec<String>,
}

/// An event trigger (CREATE EVENT TRIGGER).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EventTrigger {
    pub oid: u32,
    pub name: String,
    /// ddl_command_start, ddl_command_end, sql_drop, table_rewrite or login.
    pub event: String,
    pub owner: u32,
    pub function: u32,
    /// 'O' origin, 'D' disabled, 'R' replica, 'A' always.
    pub enabled: char,
    /// WHEN TAG IN (...), upper-cased.
    pub tags: Option<Vec<String>>,
}

/// A trigger (CREATE TRIGGER).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Trigger {
    pub oid: u32,
    pub name: String,
    pub table: u32,
    pub function: u32,
    /// Postgres tgtype bits (see `trigger_type`).
    pub tgtype: u16,
    /// UPDATE OF columns (indexes into the table's columns).
    pub columns: Vec<usize>,
    /// WHEN condition, as SQL over OLD / NEW.
    pub when: Option<String>,
    pub args: Vec<String>,
    /// 'O' enabled, 'D' disabled (ALTER TABLE ... DISABLE TRIGGER).
    pub enabled: char,
    /// REFERENCING OLD TABLE / NEW TABLE names (transition tables).
    #[serde(default)]
    pub old_table: Option<String>,
    #[serde(default)]
    pub new_table: Option<String>,
    /// CREATE CONSTRAINT TRIGGER, with its deferrability.
    #[serde(default)]
    pub constraint: bool,
    #[serde(default)]
    pub deferrable: bool,
    #[serde(default)]
    pub initdeferred: bool,
    /// The partitioned table's trigger this one was cloned from (pg_trigger.tgparentid).
    #[serde(default)]
    pub parent: u32,
    /// A constraint trigger's pg_constraint row (contype 't'; 0: none).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub con_oid: u32,
    /// A constraint trigger's FROM table (tgconstrrelid; 0: none).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub constr_rel: u32,
}

/// pg_trigger.tgtype bits.
pub mod trigger_type {
    pub const ROW: u16 = 1;
    pub const BEFORE: u16 = 2;
    pub const INSERT: u16 = 4;
    pub const DELETE: u16 = 8;
    pub const UPDATE: u16 = 16;
    pub const TRUNCATE: u16 = 32;
    pub const INSTEAD: u16 = 64;
}

/// An extension's pg_extension columns besides its name, schema and version, and what
/// pg_depend records of it.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct ExtensionInfo {
    pub owner: u32,
    pub relocatable: bool,
    /// pg_extension_config_dump: (table, condition), as extconfig / extcondition.
    #[serde(default)]
    pub config: Vec<(u32, String)>,
    /// Member objects (pg_depend deptype 'e'): (class, oid), in the order they joined.
    #[serde(default)]
    pub members: Vec<(u32, u32)>,
    /// The extensions it requires (deptype 'n').
    #[serde(default)]
    pub requires: Vec<u32>,
}

/// (The maps with an entry per relation or function are shared by a catalog's copies until
/// one changes them, `Arc::make_mut`: a DDL statement copies the catalog a few times, which
/// grew with the catalog.)
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Catalog {
    /// The index key format the database's indexes are in (`storage::keys::
    /// INDEX_KEY_FORMAT`; 0: before it was recorded). Opening a database of an older one
    /// rebuilds the indexes whose keys changed.
    #[serde(default)]
    pub index_key_format: u32,
    /// The table storage format the database's rows are in (`storage::keys::
    /// STORAGE_FORMAT`; 0: before it was recorded, primary-keyed tables keyed by the key's
    /// values). Opening a database of an older one re-keys those tables' rows.
    #[serde(default)]
    pub storage_format: u32,
    /// The format the catalog was stored in (`persist::CATALOG_FORMAT`; 0: one JSON value),
    /// as loaded: the header record writes its own.
    #[serde(default, skip_serializing)]
    pub catalog_format: u32,
    pub version: u64,
    pub next_oid: u32,
    #[serde(default)]
    pub schemas: BTreeMap<u32, Schema>,
    #[serde(default)]
    pub tables: relindex::TableMap,
    #[serde(default)]
    pub indexes: relindex::IndexMap,
    #[serde(default)]
    pub views: relindex::ViewMap,
    #[serde(default)]
    pub sequences: relindex::SequenceMap,
    #[serde(default)]
    pub enums: BTreeMap<u32, Arc<EnumType>>,
    #[serde(default)]
    pub composites: BTreeMap<u32, Arc<CompositeType>>,
    /// The domains composite types' fields are declared as: type oid → field → domain.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub composite_domains: BTreeMap<u32, BTreeMap<String, u32>>,
    /// Multirange types made in another schema than their range type: multirange oid →
    /// schema.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub multirange_schemas: BTreeMap<u32, u32>,
    /// Array types whose name isn't `_` + their element type's (a rename moved it out of
    /// another type's way, or it had to dodge one): array oid → typname.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub array_names: BTreeMap<u32, String>,
    /// CREATE TYPE ... AS RANGE.
    #[serde(default)]
    pub ranges: BTreeMap<u32, Arc<RangeDef>>,
    #[serde(default)]
    pub domains: BTreeMap<u32, Arc<Domain>>,
    #[serde(default)]
    pub functions: pmap::PMap<Arc<Function>>,
    #[serde(default)]
    pub roles: BTreeMap<u32, Role>,
    /// (object oid, sub-id, text): `COMMENT ON`. Their pg_description classoid is their
    /// object's catalog (`class_of`).
    /// (Shared by the catalog's copies until one changes it: `comments_mut`.)
    #[serde(default)]
    pub comments: Arc<Vec<(u32, i32, String)>>,
    /// pg_description rows (objoid, classoid, objsubid, text) whose classoid isn't their
    /// object's own catalog: a superuser's UPDATE / INSERT on pg_description put them there
    /// (pgjdbc moves a function's comment onto a table). Kept as written, as Postgres keeps
    /// them; dropping the object doesn't remove them (DeleteComments goes by its class).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub class_comments: Vec<(u32, u32, i32, String)>,
    /// Installed extensions: (oid, name, schema, version).
    #[serde(default)]
    pub extensions: Vec<(u32, String, u32, String)>,
    /// What the rest of an extension's pg_extension row and dependencies hold (extensions
    /// made by running their scripts; the stand-ins made before have none).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extension_info: BTreeMap<u32, ExtensionInfo>,
    #[serde(default)]
    pub triggers: BTreeMap<u32, Arc<Trigger>>,
    #[serde(default)]
    pub policies: BTreeMap<u32, Arc<Policy>>,
    /// Granted privileges of objects whose ACL isn't the default.
    #[serde(default)]
    pub acls: Vec<acl::ObjAcl>,
    /// ALTER DEFAULT PRIVILEGES entries.
    #[serde(default)]
    pub default_acls: Vec<acl::DefaultAcl>,
    /// Role memberships (GRANT role TO role).
    #[serde(default)]
    pub members: Vec<acl::AuthMember>,
    /// ALTER ROLE / DATABASE … SET (pg_db_role_setting).
    #[serde(default)]
    pub role_settings: Vec<RoleSetting>,
    /// pg_depend rows a superuser's DELETE took out (`removed`) or INSERT put in (`added`):
    /// pg_depend is otherwise derived from the objects, and these edits apply over it,
    /// for what pg_depend shows and for every dependency check (DROP, ALTER COLUMN TYPE).
    #[serde(default, skip_serializing_if = "DependEdits::is_empty")]
    pub depend_edits: DependEdits,
    /// pg_class.relpages / reltuples / relallvisible of tables and indexes, as VACUUM,
    /// ANALYZE, CREATE INDEX and TRUNCATE last set them (absent: as created).
    #[serde(default)]
    pub relstats: pmap::PMap<RelStats>,
    /// CREATE STATISTICS objects.
    #[serde(default)]
    pub statistics: BTreeMap<u32, Arc<StatExt>>,
    /// CREATE OPERATOR operators.
    #[serde(default)]
    pub operators: BTreeMap<u32, Arc<Operator>>,
    /// CREATE ACCESS METHOD access methods.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub access_methods: BTreeMap<u32, Arc<userobj::AccessMethod>>,
    /// CREATE OPERATOR FAMILY / CLASS families and classes, and the operators and support
    /// functions they (or ALTER OPERATOR FAMILY) added to families, built-in ones included.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub opfamilies: BTreeMap<u32, Arc<userobj::OpFamily>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub opclasses: BTreeMap<u32, Arc<userobj::OpClass>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub opmembers: BTreeMap<u32, Arc<userobj::OpMember>>,
    /// Built-in pg_amop / pg_amproc rows ALTER OPERATOR FAMILY ... DROP removed.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub opmembers_removed: std::collections::BTreeSet<u32>,
    /// CREATE CAST casts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub casts: BTreeMap<u32, Arc<userobj::Cast>>,
    /// Relations whose access method isn't the default (heap for tables and materialized
    /// views, none for partitioned tables), and indexes of a made access method.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rel_am: BTreeMap<u32, u32>,
    /// CREATE TYPE base types and shell types.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub base_types: BTreeMap<u32, Arc<userobj::BaseTypeDef>>,
    /// CREATE CONVERSION conversions.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub conversions: BTreeMap<u32, Arc<userobj::Conversion>>,
    /// CREATE LANGUAGE languages.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub languages: BTreeMap<u32, Arc<userobj::Language>>,
    /// Foreign-data wrappers, foreign servers and user mappings.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fdws: BTreeMap<u32, Arc<userobj::ForeignDataWrapper>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub servers: BTreeMap<u32, Arc<userobj::ForeignServer>>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub user_mappings: BTreeMap<u32, Arc<userobj::UserMapping>>,
    /// CREATE TRANSFORM transforms.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub transforms: BTreeMap<u32, Arc<userobj::Transform>>,
    /// CREATE TABLESPACE tablespaces (cluster-wide: see `shared`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub tablespaces: BTreeMap<u32, shared::Tablespace>,
    /// pg_class.reltablespace of the relations not in the database's default tablespace.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rel_tablespace: BTreeMap<u32, u32>,
    /// Every database's subscriptions (cluster-wide: see `shared`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub subscriptions: BTreeMap<u32, shared::Subscription>,
    /// CREATE PUBLICATION publications, with their tables and schemas.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub publications: BTreeMap<u32, Arc<pubsub::Publication>>,
    /// pg_statistic: ANALYZE's per-column statistics of each relation (a table, or an
    /// index's expression columns).
    #[serde(default)]
    pub colstats: BTreeMap<u32, Vec<ColStats>>,
    /// CREATE TEXT SEARCH CONFIGURATION / DICTIONARY / PARSER / TEMPLATE objects (the
    /// built-in ones aren't stored).
    #[serde(default)]
    pub ts_configs: BTreeMap<u32, Arc<crate::types::tsearch::config::TsConfigDef>>,
    #[serde(default)]
    pub ts_dicts: BTreeMap<u32, Arc<crate::types::tsearch::config::TsDictDef>>,
    #[serde(default)]
    pub ts_parsers: BTreeMap<u32, Arc<crate::types::tsearch::config::TsParserDef>>,
    #[serde(default)]
    pub ts_templates: BTreeMap<u32, Arc<crate::types::tsearch::config::TsTemplateDef>>,
    /// Built-in text search objects that were dropped (an altered built-in is stored in the
    /// maps above under its own oid).
    #[serde(default)]
    pub ts_removed: std::collections::BTreeSet<u32>,
    /// CREATE COLLATION collations (the built-in ones aren't stored).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub collations: BTreeMap<u32, Arc<crate::types::collation::CollationDef>>,
    /// pg_parameter_acl: the parameters privileges were granted on, (oid, name); their ACLs
    /// are in `acls`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parameters: Vec<(u32, String)>,
    /// pg_class.relfrozenxid of relations with storage (tables, materialized views, TOAST
    /// tables), as full transaction ids; one missing (made before they were kept) shows the
    /// first normal xid.
    #[serde(default, skip_serializing_if = "pmap::PMap::is_empty")]
    pub frozen_xids: pmap::PMap<u64>,
    /// pg_class.relfilenode of relations whose storage was replaced (TRUNCATE, a rewrite,
    /// REINDEX, ...), where it no longer equals the oid.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub filenodes: BTreeMap<u32, u32>,
    /// pg_proc.proowner of built-in routines given another owner (ALTER FUNCTION ... OWNER
    /// TO); the others are the bootstrap superuser's.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub builtin_proc_owners: BTreeMap<u32, u32>,
    /// Tables whose triggers were all dropped: pg_class.relhastriggers stays set until
    /// VACUUM clears it.
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub had_triggers: std::collections::BTreeSet<u32>,
    /// (version, hash) of the cluster-wide catalogs this copy holds (see `shared`).
    #[serde(default)]
    pub shared_stamp: (u64, u64),
    /// The cluster's oid counter, when the database runs in one.
    #[serde(skip)]
    pub oids: Option<Arc<shared::OidGen>>,
    /// The database's locale (its pg_database row's, which the cluster keeps) with its
    /// default collation opened; None: libc C (`collation::default_for`).
    #[serde(skip)]
    pub db_locale: Option<Arc<crate::types::collation::DbDefault>>,
    /// (Shared by a catalog's copies until one changes it: copying it per DDL statement grew
    /// with the catalog.)
    #[serde(skip)]
    names: pmap::PHash<(u32, String), (RelKind, u32)>,
    /// Parent oid → its direct children, in oid order.
    #[serde(skip)]
    children: Arc<HashMap<u32, Vec<u32>>>,
    /// Where tables' and materialized views' pg_class rows lie in pg_class's heap, as a
    /// sequence: a new row goes last, and so does a row updated other than in place (an
    /// in-place update, of relpages or relhasindex say, leaves it). REINDEX DATABASE /
    /// SCHEMA go through pg_class in this order. Relations without an entry come first.
    ///
    /// This is exact while pg_class's page 0 takes every new version in line-pointer order
    /// (a new database's page 0 has line pointers 2-18 and 20-45 unused, 1 and 19
    /// redirects, 46-47 live, ~7.5 kB free). It is not once pruning frees a line pointer
    /// below a live row: heap_page_prune_opt (on a syscache / relcache fetch of a page-0
    /// row, with under 819 bytes free and the dead versions' xmax behind the xmin horizon)
    /// makes dead heap-only versions' line pointers unused, and the next insert or update
    /// takes the lowest. Modelling that needs every pg_class row (indexes, TOAST tables and
    /// indexes, sequences, views, composite types too), each DDL's number of new versions
    /// (TOAST creation and CREATE VIEW update the row once more, CREATE MATERIALIZED VIEW
    /// four times, a partition updates its parent), tuple lengths (176 bytes plain, more
    /// with relacl / reloptions / relpartbound), HOT versus non-HOT (RENAME and new
    /// relfilenodes leave LP_DEAD, not reused before VACUUM), and the other pages' line
    /// pointers and free space for when page 0 is full.
    #[serde(default)]
    pub class_order: pmap::PMap<u64>,
    #[serde(default)]
    pub class_tick: u64,
    /// Relations whose relhastriggers is set (it stays set once a trigger was made).
    #[serde(default)]
    pub class_has_triggers: std::collections::BTreeSet<u32>,
    /// Relations whose relhassubclass is set.
    #[serde(default)]
    pub class_has_subclass: std::collections::BTreeSet<u32>,
    /// CREATE RULE rules (pg_rewrite rows other than views' `_RETURN`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub rules: BTreeMap<u32, Arc<Rule>>,
    /// Relations whose relhasrules is set (it stays set after their rules are dropped,
    /// until VACUUM).
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub class_has_rules: std::collections::BTreeSet<u32>,
    /// CREATE EVENT TRIGGER event triggers.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub event_triggers: BTreeMap<u32, Arc<EventTrigger>>,
    /// Counts the copies of catalogs made on this thread (`catalog_copies`).
    #[serde(skip)]
    pub copies: CopyTally,
    /// Lookups by key that otherwise scan a map, for a catalog that no longer changes
    /// (`freeze`).
    #[serde(skip)]
    lookups: Lookups,
    /// A view of a catalog leaving out partitions pending detach (`detach_view`); None for
    /// the catalog itself.
    #[serde(skip)]
    pub view: Option<Arc<DetachView>>,
}

/// What a catalog derived by `Catalog::detach_view` leaves out: Postgres's partition
/// descriptors built with omit_detached (find_inheritance_children_extended skips a
/// partition marked detach pending whose marking transaction the active snapshot sees).
#[derive(Debug)]
pub struct DetachView {
    /// The catalog itself, every partition in.
    pub full: Arc<Catalog>,
    /// (parent, partition) pairs left out.
    pub omitted: Vec<(u32, u32)>,
    /// Insert routing leaves nothing out (ExecSetupPartitionTupleRouting's directory in a
    /// REPEATABLE READ / SERIALIZABLE transaction).
    pub routing_full: bool,
}

/// `Catalog::lookups`: hashes built on first use, only for a frozen catalog (one published
/// in an `Arc`, which nothing changes any more: a copy, which DDL edits, starts unfrozen,
/// and its lookups scan).
#[derive(Default)]
struct Lookups {
    frozen: bool,
    /// A composite type's relation → the type (the first, by oid).
    composite_rels: std::sync::OnceLock<HashMap<u32, u32>>,
    /// Schema name → oid (the first, by oid).
    schema_names: std::sync::OnceLock<HashMap<String, u32>>,
    /// (class, oid, sub) → the first in `acls`.
    acls: std::sync::OnceLock<HashMap<(u32, u32, i32), usize>>,
    /// `user_type_name`'s answers.
    user_types: std::sync::OnceLock<HashMap<u32, (u32, String, bool)>>,
    /// Partitions pending detach: (parent, partition).
    detach_pending: std::sync::OnceLock<Vec<(u32, u32)>>,
}

impl Clone for Lookups {
    fn clone(&self) -> Self {
        // (A copy may be edited.)
        Lookups::default()
    }
}

impl std::fmt::Debug for Lookups {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Lookups")
    }
}

thread_local! {
    static COPIES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A field of `Catalog` counting its copies: a copy costs as much as the catalog is big, so
/// a statement's copies mustn't grow with the objects the catalog holds (ALTER TABLE once
/// copied it for every table, minutes per statement over a large catalog).
#[derive(Debug, Default)]
pub struct CopyTally;

impl Clone for CopyTally {
    fn clone(&self) -> Self {
        COPIES.with(|c| c.set(c.get() + 1));
        CopyTally
    }
}

/// How many catalogs this thread has copied so far.
pub fn catalog_copies() -> u64 {
    COPIES.with(|c| c.get())
}

/// A relation found by name.
#[derive(Clone, Debug)]
pub enum Relation {
    Table(Arc<Table>),
    View(Arc<ViewDef>),
    Sequence(Arc<Sequence>),
    Index(Arc<Index>),
}

impl Relation {
    pub fn oid(&self) -> u32 {
        match self {
            Relation::Table(t) => t.oid,
            Relation::View(v) => v.oid,
            Relation::Sequence(s) => s.oid,
            Relation::Index(i) => i.oid,
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Relation::Table(t) if t.foreign.is_some() => "foreign table",
            Relation::Table(_) => "table",
            Relation::View(v) if v.materialized => "materialized view",
            Relation::View(_) => "view",
            Relation::Sequence(_) => "sequence",
            Relation::Index(_) => "index",
        }
    }
}

/// The implicit role standing for the current database's owner (owns `public`).
pub const PG_DATABASE_OWNER_OID: u32 = 6171;

/// Postgres's predefined (non-login) roles.
pub const PREDEFINED_ROLES: &[(u32, &str)] = &[
    (PG_DATABASE_OWNER_OID, "pg_database_owner"),
    (6181, "pg_read_all_data"),
    (6182, "pg_write_all_data"),
    (3373, "pg_monitor"),
    (3374, "pg_read_all_settings"),
    (3375, "pg_read_all_stats"),
    (3377, "pg_stat_scan_tables"),
    (4569, "pg_read_server_files"),
    (4570, "pg_write_server_files"),
    (4571, "pg_execute_server_program"),
    (4200, "pg_signal_backend"),
    (4544, "pg_checkpoint"),
    (6337, "pg_maintain"),
    (4550, "pg_use_reserved_connections"),
    (6304, "pg_create_subscription"),
    (6392, "pg_signal_autovacuum_worker"),
];

impl Catalog {
    /// A fresh database: pg_catalog, information_schema, public, and the bootstrap role.
    pub fn bootstrap(owner_name: &str) -> Catalog {
        let mut c = Catalog {
            next_oid: FIRST_USER_OID,
            index_key_format: crate::storage::keys::INDEX_KEY_FORMAT,
            storage_format: crate::storage::keys::STORAGE_FORMAT,
            catalog_format: persist::CATALOG_FORMAT,
            ..Default::default()
        };
        for (oid, name) in [
            (PG_CATALOG_OID, "pg_catalog"),
            (PUBLIC_OID, "public"),
            (INFORMATION_SCHEMA_OID, "information_schema"),
            (PG_TOAST_OID, "pg_toast"),
        ] {
            let owner = if oid == PUBLIC_OID {
                PG_DATABASE_OWNER_OID
            } else {
                BOOTSTRAP_SUPERUSER_OID
            };
            c.schemas.insert(
                oid,
                Schema {
                    oid,
                    name: name.into(),
                    owner,
                },
            );
        }
        for (oid, name) in PREDEFINED_ROLES {
            c.roles.insert(*oid, Role::new(*oid, *name));
        }
        c.roles.insert(
            BOOTSTRAP_SUPERUSER_OID,
            Role {
                superuser: true,
                login: true,
                create_db: true,
                create_role: true,
                replication: true,
                bypass_rls: true,
                ..Role::new(BOOTSTRAP_SUPERUSER_OID, owner_name)
            },
        );
        // Postgres 15+: public is usable by everyone but only its owner may create in it.
        c.set_acl(
            acl::ObjKind::Schema,
            PUBLIC_OID,
            0,
            Some(vec![
                acl::AclItem {
                    grantee: PG_DATABASE_OWNER_OID,
                    grantor: PG_DATABASE_OWNER_OID,
                    privs: acl::USAGE | acl::CREATE,
                    goptions: 0,
                },
                acl::AclItem {
                    grantee: 0,
                    grantor: PG_DATABASE_OWNER_OID,
                    privs: acl::USAGE,
                    goptions: 0,
                },
            ]),
        );
        // initdb: the templates take connections but no CREATE / TEMPORARY from PUBLIC
        // (template1 = 1, template0 = 4: `=c/postgres`, `postgres=CTc/postgres`).
        for db in [1, 4] {
            c.set_acl(
                acl::ObjKind::Database,
                db,
                0,
                Some(vec![
                    acl::AclItem {
                        grantee: 0,
                        grantor: BOOTSTRAP_SUPERUSER_OID,
                        privs: acl::CONNECT,
                        goptions: 0,
                    },
                    acl::AclItem {
                        grantee: BOOTSTRAP_SUPERUSER_OID,
                        grantor: BOOTSTRAP_SUPERUSER_OID,
                        privs: acl::CREATE | acl::TEMPORARY | acl::CONNECT,
                        goptions: 0,
                    },
                ]),
            );
        }
        c.reindex();
        c
    }

    /// information_schema's domains (cardinal_number, sql_identifier, …), with Postgres's oids.
    fn add_information_schema_domains(&mut self) {
        let domains: [(u32, &str, DataType, Option<(&str, &str)>, Option<&str>); 5] = [
            (
                13341,
                "cardinal_number",
                DataType::Int4,
                Some(("cardinal_number_domain_check", "VALUE >= 0")),
                None,
            ),
            (13344, "character_data", DataType::Varchar(-1), None, None),
            (13346, "sql_identifier", DataType::Name, None, None),
            (
                13352,
                "time_stamp",
                DataType::Timestamptz(2),
                None,
                Some("CURRENT_TIMESTAMP(2)"),
            ),
            (
                13354,
                "yes_or_no",
                DataType::Varchar(7),
                Some(("yes_or_no_check", "VALUE IN ('YES', 'NO')")),
                None,
            ),
        ];
        for (oid, name, base, check, default) in domains {
            if self.domains.contains_key(&oid) {
                continue;
            }
            let d = Domain {
                oid,
                schema: INFORMATION_SCHEMA_OID,
                name: name.into(),
                base,
                not_null: false,
                default: default.map(String::from),
                checks: check
                    .iter()
                    .map(|c| (c.0.to_string(), c.1.to_string()))
                    .collect(),
                owner: BOOTSTRAP_SUPERUSER_OID,
                not_null_name: None,
                invalid: Vec::new(),
                parent: None,
                base_domain: None,
                array_oid: oid - 1,
                con_oids: check.iter().map(|c| (c.0.to_string(), oid + 1)).collect(),
                collation: None,
            };
            self.domains.insert(oid, Arc::new(d));
        }
    }

    /// Rebuild name lookups (after deserialising).
    pub fn reindex(&mut self) {
        // Catalogs from before information_schema had Postgres's oid.
        if let Some(mut s) = self
            .schemas
            .remove(&13000)
            .filter(|s| s.name == "information_schema")
        {
            s.oid = INFORMATION_SCHEMA_OID;
            self.schemas.insert(s.oid, s);
        }
        if self.schemas.contains_key(&INFORMATION_SCHEMA_OID) {
            self.add_information_schema_domains();
        }
        // Catalogs from before row types had oids.
        for oid in self
            .tables
            .values()
            .filter(|t| t.rowtype == 0)
            .map(|t| t.oid)
            .collect::<Vec<_>>()
        {
            let mut t = (*self.tables[&oid]).clone();
            (t.rowtype, t.rowarray) = self.alloc_rowtype();
            self.tables.insert(oid, Arc::new(t));
        }
        for oid in self
            .views
            .values()
            .filter(|v| v.rowtype == 0)
            .map(|v| v.oid)
            .collect::<Vec<_>>()
        {
            let mut v = (*self.views[&oid]).clone();
            (v.rowtype, v.rowarray) = self.alloc_rowtype();
            self.views.insert(oid, Arc::new(v));
        }
        // Catalogs from before defaults had pg_attrdef oids.
        for oid in self
            .tables
            .values()
            .filter(|t| {
                t.columns.iter().any(|c| {
                    c.def_oid == 0 && !c.dropped && (c.default.is_some() || c.generated.is_some())
                })
            })
            .map(|t| t.oid)
            .collect::<Vec<_>>()
        {
            let mut t = (*self.tables[&oid]).clone();
            for c in &mut t.columns {
                if c.def_oid == 0 && !c.dropped && (c.default.is_some() || c.generated.is_some()) {
                    c.def_oid = self.alloc_oid();
                }
            }
            self.tables.insert(oid, Arc::new(t));
        }
        self.names.clear();
        Arc::make_mut(&mut self.children).clear();
        for t in self.tables.values() {
            self.names
                .insert((t.schema, t.name.clone()), (RelKind::Table, t.oid));
            for p in &t.inherits {
                Arc::make_mut(&mut self.children).entry(*p).or_default().push(t.oid);
            }
        }
        for v in self.views.values() {
            let kind = if v.materialized {
                RelKind::MaterializedView
            } else {
                RelKind::View
            };
            self.names.insert((v.schema, v.name.clone()), (kind, v.oid));
        }
        for s in self.sequences.values() {
            self.names
                .insert((s.schema, s.name.clone()), (RelKind::Sequence, s.oid));
        }
        for i in self.indexes.values() {
            let schema = self
                .index_table(i.table)
                .map(|t| t.schema)
                .unwrap_or(PUBLIC_OID);
            self.names
                .insert((schema, i.name.clone()), (RelKind::Index, i.oid));
        }
    }

    /// Whether the name index and children lists are alike (debug checks of `reindex_rels`).
    pub fn names_eq(&self, o: &Catalog) -> bool {
        let kids = |c: &Catalog| -> BTreeMap<u32, Vec<u32>> {
            c.children.iter().filter(|(_, v)| !v.is_empty()).map(|(k, v)| (*k, v.clone())).collect()
        };
        self.names.len() == o.names.len()
            && self.names.iter().all(|(k, v)| o.names.get(k) == Some(v))
            && kids(self) == kids(o)
    }

    /// The comments, to change (copied first when other catalogs share them).
    pub fn comments_mut(&mut self) -> &mut Vec<(u32, i32, String)> {
        Arc::make_mut(&mut self.comments)
    }

    /// The relation name index's entry of relation `oid` (an index is in its table's schema).
    fn name_entry(&self, oid: u32) -> Option<((u32, String), (RelKind, u32))> {
        if let Some(t) = self.tables.get(&oid) {
            return Some(((t.schema, t.name.clone()), (RelKind::Table, oid)));
        }
        if let Some(v) = self.views.get(&oid) {
            let kind = if v.materialized { RelKind::MaterializedView } else { RelKind::View };
            return Some(((v.schema, v.name.clone()), (kind, oid)));
        }
        if let Some(s) = self.sequences.get(&oid) {
            return Some(((s.schema, s.name.clone()), (RelKind::Sequence, oid)));
        }
        let i = self.indexes.get(&oid)?;
        let schema = self.index_table(i.table).map_or(PUBLIC_OID, |t| t.schema);
        Some(((schema, i.name.clone()), (RelKind::Index, oid)))
    }

    /// `reindex`'s name index and children lists brought up to date for the relations in
    /// `rels`, whose entries may differ from those of `before` (the catalog this one was
    /// copied from, whose index was whole): what a merged commit changed, not the whole
    /// catalog. Err: two relations of one schema took one name
    /// (pg_class_relname_nsp_index).
    pub fn reindex_rels(&mut self, before: &Catalog, rels: &std::collections::BTreeSet<u32>) -> Result<()> {
        // (A table moved to another schema moves its indexes' names too.)
        let mut rels = rels.clone();
        for &o in rels.clone().iter() {
            if let (Some(a), Some(b)) = (before.tables.get(&o), self.tables.get(&o))
                && a.schema != b.schema
            {
                rels.extend(b.indexes.iter().copied().chain(b.pkey_index));
            }
        }
        for &o in &rels {
            if let Some((k, (_, at))) = before.name_entry(o)
                && self.names.get(&k).is_some_and(|e| e.1 == at)
            {
                self.names.remove(&k);
            }
            if let Some(t) = before.tables.get(&o).filter(|t| !t.inherits.is_empty()) {
                for p in &t.inherits {
                    if let Some(v) = Arc::make_mut(&mut self.children).get_mut(p) {
                        v.retain(|c| *c != o);
                    }
                }
            }
        }
        for &o in &rels {
            if let Some((k, e)) = self.name_entry(o) {
                if let Some(&(_, other)) = self.names.get(&k)
                    && other != o
                    && self.name_entry(other).is_some_and(|x| x.0 == k)
                {
                    return Err(Error::new(
                        code::UNIQUE_VIOLATION,
                        "duplicate key value violates unique constraint \"pg_class_relname_nsp_index\"",
                    )
                    .detail(format!("Key (relname, relnamespace)=({}, {}) already exists.", k.1, k.0)));
                }
                self.names.insert(k, e);
            }
            if let Some(t) = self.tables.get(&o).cloned() {
                for p in &t.inherits {
                    let v = Arc::make_mut(&mut self.children).entry(*p).or_default();
                    // (In table oid order, as `reindex` lists them.)
                    if !v.contains(&o) {
                        let at = v.partition_point(|c| *c < o);
                        v.insert(at, o);
                    }
                }
            }
        }
        Ok(())
    }

    /// An even oid (enum labels: comparing them then follows the sort order).
    pub fn alloc_even_oid(&mut self) -> u32 {
        loop {
            let o = self.alloc_oid();
            if o & 1 == 0 {
                return o;
            }
        }
    }

    /// Oids for a row type and its array type, the array type's first (as AddNewRelationType
    /// and DefineEnum assign them): (row type, array type).
    pub fn alloc_rowtype(&mut self) -> (u32, u32) {
        let o = self.alloc_oids(2);
        (o + 1, o)
    }

    pub fn alloc_oid(&mut self) -> u32 {
        self.alloc_oids(1)
    }

    /// An index-backed constraint's oid, then for a deferrable one its unique_key_recheck
    /// trigger's (index_constraint_create), which is the next.
    pub fn alloc_con_oid(&mut self, deferrable: bool) -> u32 {
        let o = self.alloc_oid();
        if deferrable {
            self.alloc_oid();
        }
        o
    }

    /// `n` consecutive oids.
    pub fn alloc_oids(&mut self, n: u32) -> u32 {
        let o = match &self.oids {
            Some(g) => g.take(self.next_oid, n),
            None => self.next_oid,
        };
        self.next_oid = o + n;
        o
    }

    /// Whether pg_description has the row (`oid`, `class`, `sub`): its unique index's key.
    pub fn has_description(&self, oid: u32, class: u32, sub: i32) -> bool {
        if self.class_of(oid) != Some(class) {
            return self.class_comments.iter().any(|c| (c.0, c.1, c.2) == (oid, class, sub));
        }
        self.comments.iter().any(|c| c.0 == oid && c.1 == sub && !c.2.is_empty())
            || system::builtin_description(self, oid, Some(class), sub).is_some()
    }

    /// pg_description's row (`oid`, `class`, `sub`) deleted (a superuser's DML).
    pub fn remove_description(&mut self, oid: u32, class: u32, sub: i32) {
        if self.class_of(oid) != Some(class) {
            self.class_comments.retain(|c| (c.0, c.1, c.2) != (oid, class, sub));
            return;
        }
        self.comments_mut().retain(|c| !(c.0 == oid && c.1 == sub));
        if system::builtin_description(self, oid, Some(class), sub).is_some() {
            // (An empty comment hides the one Postgres ships.)
            self.comments_mut().push((oid, sub, String::new()));
        }
    }

    /// pg_description's row (`oid`, `class`, `sub`) set to `text` (a superuser's DML).
    pub fn put_description(&mut self, oid: u32, class: u32, sub: i32, text: String) {
        if self.class_of(oid) != Some(class) {
            self.class_comments.retain(|c| (c.0, c.1, c.2) != (oid, class, sub));
            self.class_comments.push((oid, class, sub, text));
            return;
        }
        self.comments_mut().retain(|c| !(c.0 == oid && c.1 == sub));
        self.comments_mut().push((oid, sub, text));
    }

    /// The system catalog (pg_class, pg_proc, …) object `oid` of this database is in, for
    /// pg_description; None for what isn't one of this database's objects.
    pub fn class_of(&self, oid: u32) -> Option<u32> {
        use depend::*;
        let class = if self.tables.contains_key(&oid)
            || self.views.contains_key(&oid)
            || self.indexes.contains_key(&oid)
            || self.sequences.contains_key(&oid)
            // (A composite type's pg_class entry.)
            || self.composites.values().any(|c| c.relid == oid)
        {
            REL
        } else if self.schemas.contains_key(&oid) {
            NAMESPACE
        } else if self.functions.contains_key(&oid) {
            PROC
        } else if self.enums.contains_key(&oid)
            || self.domains.contains_key(&oid)
            || self.composites.contains_key(&oid)
            || self.ranges.contains_key(&oid)
        {
            TYPE
        } else if self
            .tables
            .values()
            .any(|t| t.constraints.iter().any(|c| c.oid == oid))
        {
            CONSTRAINT
        } else if self.triggers.contains_key(&oid) {
            TRIGGER
        } else if self.policies.contains_key(&oid) {
            POLICY
        } else if self.rules.contains_key(&oid) {
            REWRITE
        } else if self.event_triggers.contains_key(&oid) {
            EVENT_TRIGGER
        } else if self.operators.contains_key(&oid) {
            OPERATOR
        } else if self.statistics.contains_key(&oid) {
            STATEXT
        } else if self.collations.contains_key(&oid) {
            COLLATION
        } else if self.fdws.contains_key(&oid) {
            FDW
        } else if self.servers.contains_key(&oid) {
            SERVER
        } else if self.opclasses.contains_key(&oid) {
            OPCLASS
        } else if self.opfamilies.contains_key(&oid) {
            OPFAMILY
        } else if self.casts.contains_key(&oid) {
            CAST
        } else if self.conversions.contains_key(&oid) {
            CONVERSION
        } else if self.access_methods.contains_key(&oid) {
            AM
        } else if self.languages.contains_key(&oid) {
            LANGUAGE
        } else if self.transforms.contains_key(&oid) {
            TRANSFORM
        } else if self.publications.contains_key(&oid) {
            PUBLICATION
        } else if self.subscriptions.contains_key(&oid) {
            SUBSCRIPTION
        } else if self.base_types.contains_key(&oid) {
            TYPE
        } else if self.extensions.iter().any(|e| e.0 == oid)
            || oid == crate::sql::func::system::PLPGSQL_EXT_OID
        {
            EXTENSION
        } else {
            match [
                tsearch::TsKind::Config,
                tsearch::TsKind::Dict,
                tsearch::TsKind::Parser,
                tsearch::TsKind::Template,
            ]
            .into_iter()
            .find(|k| self.ts_objects(*k).iter().any(|x| x.0 == oid))
            {
                Some(k) => ts_class(k),
                None => return system::builtin_class(oid),
            }
        };
        Some(class)
    }

    /// The shared catalog (pg_authid, pg_database, pg_tablespace) object `oid` is in, given
    /// the cluster's database oids, for pg_shdescription.
    pub fn shared_class_of(&self, oid: u32, databases: &[u32]) -> Option<u32> {
        if self.roles.contains_key(&oid) {
            Some(1260)
        } else if databases.contains(&oid) {
            Some(1262)
        } else if oid == 1663 || oid == 1664 || self.tablespaces.contains_key(&oid) {
            Some(1213)
        } else {
            None
        }
    }

    /// A tablespace's oid by name (built-in ones included).
    pub fn tablespace_oid(&self, name: &str) -> Option<u32> {
        match name {
            "pg_default" => Some(1663),
            "pg_global" => Some(1664),
            _ => self
                .tablespaces
                .values()
                .find(|t| t.name == name)
                .map(|t| t.oid),
        }
    }

    pub fn tablespace_name(&self, oid: u32) -> Option<String> {
        match oid {
            1663 => Some("pg_default".into()),
            1664 => Some("pg_global".into()),
            _ => self.tablespaces.get(&oid).map(|t| t.name.clone()),
        }
    }

    /// A tablespace's owner (the bootstrap superuser's for the built-in ones).
    pub fn tablespace_owner(&self, oid: u32) -> u32 {
        self.tablespaces
            .get(&oid)
            .map_or(BOOTSTRAP_SUPERUSER_OID, |t| t.owner)
    }

    /// pg_class.reltablespace of relation `oid` (0: the database's default).
    pub fn rel_tablespace(&self, oid: u32) -> u32 {
        self.rel_tablespace.get(&oid).copied().unwrap_or(0)
    }

    /// Put relation `oid` in tablespace `spc` (0 or the database's: its default).
    pub fn set_rel_tablespace(&mut self, oid: u32, spc: u32) {
        if spc == 0 || spc == 1663 {
            self.rel_tablespace.remove(&oid);
        } else {
            self.rel_tablespace.insert(oid, spc);
        }
    }

    pub fn schema_by_name(&self, name: &str) -> Option<&Schema> {
        // `pg_temp` is the running session's temporary schema.
        if name == "pg_temp"
            && let Some(t) = SESSION_TEMP_SCHEMA.with(|t| t.borrow().clone())
        {
            return self.schema_named(&t);
        }
        self.schema_named(name)
    }

    fn schema_named(&self, name: &str) -> Option<&Schema> {
        if self.lookups.frozen {
            let m = self.lookups.schema_names.get_or_init(|| {
                let mut m = HashMap::new();
                for s in self.schemas.values() {
                    m.entry(s.name.clone()).or_insert(s.oid);
                }
                m
            });
            return self.schemas.get(m.get(name)?);
        }
        self.schemas.values().find(|s| s.name == name)
    }

    pub fn schema_oid(&self, name: &str) -> Result<u32> {
        self.schema_by_name(name)
            .map(|s| s.oid)
            .ok_or_else(|| err!(UNDEFINED_SCHEMA, "schema \"{name}\" does not exist"))
    }

    pub fn schema_name(&self, oid: u32) -> &str {
        self.schemas
            .get(&oid)
            .map(|s| s.name.as_str())
            .unwrap_or("?")
    }

    pub fn relation_in(&self, schema: u32, name: &str) -> Option<Relation> {
        let (kind, oid) = self.names.get(&(schema, name.to_string()))?;
        Some(match kind {
            RelKind::Table => Relation::Table(self.tables.get(oid)?.clone()),
            RelKind::View | RelKind::MaterializedView => {
                Relation::View(self.views.get(oid)?.clone())
            }
            RelKind::Sequence => Relation::Sequence(self.sequences.get(oid)?.clone()),
            RelKind::Index => Relation::Index(self.indexes.get(oid)?.clone()),
        })
    }

    /// Find a relation by (optionally qualified) name using the search path.
    pub fn lookup(
        &self,
        schema: Option<&str>,
        name: &str,
        search_path: &[String],
    ) -> Result<Option<Relation>> {
        if let Some(s) = schema {
            let soid = self.schema_oid(s)?;
            return Ok(self.relation_in(soid, name));
        }
        for s in self.effective_path(search_path) {
            if let Some(r) = self.relation_in(s, name) {
                return Ok(Some(r));
            }
            // A system relation earlier in the path hides the user's of the same name.
            if Self::system_relation_in(s, name) {
                return Ok(None);
            }
        }
        Ok(None)
    }

    /// Whether schema `s` (pg_catalog or information_schema) has a system relation `name`.
    pub fn system_relation_in(s: u32, name: &str) -> bool {
        let schema = match s {
            PG_CATALOG_OID => "pg_catalog",
            INFORMATION_SCHEMA_OID => "information_schema",
            _ => return false,
        };
        system::SysTable::lookup(Some(schema), name, &[]).is_some()
    }

    /// Query-time lookup: a missing schema just means no such relation (unlike DDL, where
    /// it's an error of its own).
    pub fn find(
        &self,
        schema: Option<&str>,
        name: &str,
        search_path: &[String],
    ) -> Option<Relation> {
        match schema {
            Some(s) => self
                .schema_by_name(s)
                .and_then(|sc| self.relation_in(sc.oid, name)),
            None => self.lookup(None, name, search_path).ok().flatten(),
        }
    }

    /// Search path as schema OIDs: pg_catalog first unless listed explicitly.
    pub fn effective_path(&self, search_path: &[String]) -> Vec<u32> {
        let mut out = Vec::new();
        if !search_path.iter().any(|s| s == "pg_catalog") {
            out.push(PG_CATALOG_OID);
        }
        for s in search_path {
            if let Some(sc) = self.schema_by_name(s) {
                out.push(sc.oid);
            }
        }
        out
    }

    /// The path functions and operators are looked up in: the temporary schema never is
    /// (unless named explicitly).
    pub fn routine_path(&self, search_path: &[String]) -> Vec<u32> {
        self.effective_path(search_path)
            .into_iter()
            .filter(|s| !self.schema_name(*s).starts_with("pg_temp"))
            .collect()
    }

    /// Schema where unqualified CREATEs go: the first existing schema in the path.
    pub fn creation_schema(&self, search_path: &[String]) -> Result<u32> {
        for s in search_path {
            if s == "pg_catalog" || s.starts_with("pg_temp") {
                continue;
            }
            if let Some(sc) = self.schema_by_name(s) {
                return Ok(sc.oid);
            }
        }
        Err(Error::new(
            code::INVALID_SCHEMA_NAME,
            "no schema has been selected to create in",
        ))
    }

    pub fn table(&self, oid: u32) -> Option<&Arc<Table>> {
        self.tables.get(&oid)
    }

    pub fn index(&self, oid: u32) -> Option<&Arc<Index>> {
        self.indexes.get(&oid)
    }

    /// A statistics object by (optionally qualified) name.
    /// A user-defined operator by name and argument types, first in `schemas`.
    pub fn find_operator(
        &self,
        schemas: &[u32],
        name: &str,
        left: Option<&DataType>,
        right: &DataType,
    ) -> Option<Arc<Operator>> {
        schemas.iter().find_map(|sc| {
            self.operators
                .values()
                .find(|o| {
                    o.schema == *sc
                        && o.name == name
                        && o.right.same_base(right)
                        && o.left.as_ref().map(|t| t.oid()) == left.map(|t| t.oid())
                })
                .cloned()
        })
    }

    /// An operator's name as `regoper` shows it: qualified unless its schema is searched.
    pub fn operator_name(&self, o: &Operator, search_path: &[String]) -> String {
        let visible = self
            .effective_path(search_path)
            .iter()
            .find(|sc| {
                self.operators.values().any(|x| {
                    x.schema == **sc
                        && x.name == o.name
                        && x.left.is_some() == o.left.is_some()
                        && x.right.same_base(&o.right)
                        && x.left.as_ref().map(|t| t.oid()) == o.left.as_ref().map(|t| t.oid())
                })
            })
            .copied();
        if visible == Some(o.schema) {
            o.name.clone()
        } else {
            format!("{}.{}", self.schema_name(o.schema), o.name)
        }
    }

    /// `===(integer,integer)` (regoperator).
    pub fn operator_signature(&self, o: &Operator, search_path: &[String]) -> String {
        let ty = |t: &DataType| self.format_type(t, None, search_path);
        format!(
            "{}({},{})",
            self.operator_name(o, search_path),
            o.left.as_ref().map_or("NONE".into(), ty),
            ty(&o.right)
        )
    }

    pub fn find_statistics(
        &self,
        schema: Option<&str>,
        name: &str,
        search_path: &[String],
    ) -> Result<Option<Arc<StatExt>>> {
        // (get_statistics_object_oid doesn't look in the temporary schema unless named.)
        let schemas: Vec<u32> = match schema {
            Some(s) => vec![self.schema_oid(s)?],
            None => self.routine_path(search_path),
        };
        Ok(schemas.iter().find_map(|sc| {
            self.statistics
                .values()
                .find(|x| x.schema == *sc && x.name == name)
                .cloned()
        }))
    }

    /// pg_class.relpages / reltuples / relallvisible / relallfrozen of relation `oid` of
    /// relkind `kind`.
    pub fn rel_stats(&self, oid: u32, kind: char) -> RelStats {
        self.relstats
            .get(&oid)
            .copied()
            .unwrap_or_else(|| RelStats::initial(kind))
    }

    /// The table an index is on: a table, or a materialized view's storage.
    pub fn index_table(&self, oid: u32) -> Option<Arc<Table>> {
        self.tables
            .get(&oid)
            .cloned()
            .or_else(|| self.views.get(&oid).and_then(|v| v.storage.clone()))
    }

    /// Store a table, or a materialized view's storage.
    pub fn put_storage(&mut self, t: Table) {
        match self.views.get(&t.oid) {
            Some(v) if v.materialized => {
                let v = ViewDef {
                    storage: Some(Arc::new(t)),
                    ..(**v).clone()
                };
                self.put_view(v);
            }
            _ => self.put_table(t),
        }
    }

    pub fn table_indexes(&self, t: &Table) -> Vec<Arc<Index>> {
        t.indexes
            .iter()
            .filter_map(|i| self.indexes.get(i).cloned())
            .collect()
    }

    pub fn name_taken(&self, schema: u32, name: &str) -> bool {
        self.names.contains_key(&(schema, name.to_string()))
            || self
                .composites
                .values()
                .any(|c| c.schema == schema && c.name == name)
    }

    /// An array type's name (pg_type.typname): `_` + its element type's, unless it was
    /// given another.
    pub fn array_typname(&self, array_oid: u32, elem: &str) -> String {
        match self.array_names.get(&array_oid) {
            Some(n) => n.clone(),
            None => array_type_name(elem),
        }
    }

    /// The user type named `typname` in `schema` (an array type by its typname too).
    pub fn type_by_typname(&self, schema: u32, typname: &str) -> Option<u32> {
        let mut found = None;
        self.for_each_user_type(&mut |o, a, s, n| {
            if found.is_some() || s != schema {
                return;
            }
            if n == typname {
                found = Some(o);
            } else if a != 0 && self.array_typname(a, n) == typname {
                found = Some(a);
            }
        });
        found
    }

    /// Whether `schema` has a user type with this typname: a type, a relation's row type,
    /// or an array type of either (TYPENAMENSP).
    pub fn type_named_in(&self, schema: u32, typname: &str) -> bool {
        self.user_type_entries().any(|(o, a, s, n)| {
            s == schema && o != 0 && (n == typname || a != 0 && self.array_typname(a, n) == typname)
        })
    }

    /// makeArrayTypeName: `_` + the name, or with `_1`, `_2`, ... appended when that is
    /// taken in the schema.
    pub fn make_array_type_name(&self, schema: u32, elem: &str, except: u32) -> String {
        let mut name = array_type_name(elem);
        let mut pass = 0;
        let taken = |name: &str| {
            let mut hit = false;
            self.for_each_user_type(&mut |o, a, s, n| {
                if s == schema
                    && (o != except && n == name
                        || a != 0 && a != except && self.array_typname(a, n) == name)
                {
                    hit = true;
                }
            });
            hit
        };
        while taken(&name) {
            pass += 1;
            name = make_object_name("", Some(elem), &pass.to_string());
        }
        name
    }

    /// RenameTypeInternal's dodge: a type is getting `name` in `schema`; an autogenerated
    /// array type already called that moves out of the way (moveArrayTypeName). `except`:
    /// the array type of the type being renamed, which is handled after.
    pub fn move_array_type_name(&mut self, schema: u32, name: &str, except: u32) {
        let Some(a) = self.type_by_typname(schema, name) else {
            return;
        };
        if a == except || self.user_type_name(a).is_none_or(|(_, _, array)| !array) {
            return;
        }
        let newname = self.make_array_type_name(schema, name, 0);
        self.array_names.insert(a, newname);
    }

    /// A type renamed to `name`: its array type gets the name makeArrayTypeName picks.
    pub fn rename_array_type(&mut self, schema: u32, array_oid: u32, name: &str) {
        if array_oid == 0 {
            return;
        }
        self.array_names.remove(&array_oid);
        let n = self.make_array_type_name(schema, name, array_oid);
        if n != array_type_name(name) {
            self.array_names.insert(array_oid, n);
        }
    }

    /// An array type known by a name it was given (`array_names`), on the search path
    /// (or in `schema`): its element type's oid.
    pub fn array_by_given_name(
        &self,
        schema: Option<u32>,
        name: &str,
        search_path: &[String],
    ) -> Option<u32> {
        if !self.array_names.values().any(|n| n == name) {
            return None;
        }
        let schemas = match schema {
            Some(s) => vec![s],
            None => self.effective_path(search_path),
        };
        for s in schemas {
            let mut found = None;
            self.for_each_user_type(&mut |o, a, ts, _| {
                if found.is_none()
                    && ts == s
                    && a != 0
                    && self.array_names.get(&a).is_some_and(|n| n == name)
                {
                    found = Some(o);
                }
            });
            if found.is_some() {
                return found;
            }
        }
        None
    }

    /// A type made with the name another type's array type has (TypeCreate's
    /// moveArrayTypeName): that array type moves aside, and the new type's own array type
    /// is named after it (newer types are those with higher oids).
    /// `dodge_array_type_names` after a statement that made this catalog from `before`
    /// (which has no clash left: every save resolves them): the pass over every type runs
    /// only when a type the statement made or renamed could be in one, by its name or by its
    /// array type's name.
    pub fn dodge_array_type_names_since(&mut self, before: &Catalog) {
        if self.may_clash_since(before) {
            self.dodge_array_type_names();
        } else {
            debug_assert!(
                !before.array_name_clashes().is_empty() || self.array_name_clashes().is_empty(),
                "an array type name clash the check missed"
            );
        }
    }

    fn may_clash_since(&self, before: &Catalog) -> bool {
        if self.array_names != before.array_names {
            return true;
        }
        // (oid, array oid, schema, name) of the types the statement made or renamed.
        let mut new: Vec<(u32, u32, u32, &str)> = Vec::new();
        for (_, b, t) in before.tables.changes(&self.tables) {
            if let Some(t) = t
                && b.is_none_or(|b| b.name != t.name || b.schema != t.schema || b.rowtype != t.rowtype)
            {
                new.push((t.rowtype, t.array_type(), t.schema, &t.name));
            }
        }
        for (_, b, v) in before.views.changes(&self.views) {
            if let Some(v) = v
                && b.is_none_or(|b| b.name != v.name || b.schema != v.schema || b.rowtype != v.rowtype)
            {
                new.push((v.rowtype, v.array_type(), v.schema, &v.name));
            }
        }
        fn changed<'a, V>(
            a: &BTreeMap<u32, Arc<V>>,
            b: &'a BTreeMap<u32, Arc<V>>,
        ) -> impl Iterator<Item = &'a Arc<V>> {
            b.iter()
                .filter(|(k, v)| a.get(k).is_none_or(|x| !Arc::ptr_eq(x, v)))
                .map(|(_, v)| v)
        }
        for e in changed(&before.enums, &self.enums) {
            new.push((e.oid, e.array_oid, e.schema, &e.name));
        }
        for c in changed(&before.composites, &self.composites) {
            new.push((c.oid, c.array_oid, c.schema, &c.name));
        }
        for d in changed(&before.domains, &self.domains) {
            new.push((d.oid, d.array_oid, d.schema, &d.name));
        }
        for b in changed(&before.base_types, &self.base_types) {
            new.push((b.ty.oid, b.ty.array_oid, b.ty.schema, &b.ty.name));
        }
        for r in changed(&before.ranges, &self.ranges) {
            new.push((r.oid, r.array_oid, r.schema, &r.name));
            new.push((r.multi_oid, r.multi_array_oid, self.multirange_schema(r), &r.multi_name));
        }
        new.iter().any(|&(_, a, s, n)| {
            // (An array type's default name starts with `_`.)
            n.starts_with('_')
                || self.array_names.values().any(|x| x == n)
                || (a != 0 && self.type_name_taken(s, &self.array_typname(a, n)))
        })
    }

    /// Whether a user type, or a relation (whose row type it would be), has `name` in
    /// `schema`.
    fn type_name_taken(&self, schema: u32, name: &str) -> bool {
        // (Tables and views by the name index.)
        if self.names.contains_key(&(schema, name.to_string())) {
            return true;
        }
        debug_assert!(
            !(self.tables.values().any(|t| t.schema == schema && t.name == name)
                || self.views.values().any(|v| v.schema == schema && v.name == name)),
            "a relation missing from the name index"
        );
        self.enums.values().any(|e| e.schema == schema && e.name == name)
            || self.composites.values().any(|c| c.schema == schema && c.name == name)
            || self.domains.values().any(|d| d.schema == schema && d.name == name)
            || self.base_types.values().any(|b| b.ty.schema == schema && &*b.ty.name == name)
            || self.ranges.values().any(|r| {
                (r.schema == schema && r.name == name)
                    || (self.multirange_schema(r) == schema && r.multi_name == name)
            })
    }

    pub fn dodge_array_type_names(&mut self) {
        for (arr, own_array, s, n, late) in self.array_name_clashes() {
            if late {
                let name = self.make_array_type_name(s, &n, arr);
                self.array_names.insert(arr, name);
                continue;
            }
            let moved = self.make_array_type_name(s, &n, own_array);
            self.array_names.insert(arr, moved);
            self.rename_array_type(s, own_array, &n);
        }
    }

    /// Types named as another type's array type: newer than that type, (the array type,
    /// the type's own array type, schema, name, false); older, the array type made after a
    /// type already had its name (makeArrayTypeName gave it `_name_1`, or the next number
    /// free, when it was made): (the array type, 0, schema, its element type's name, true).
    fn array_name_clashes(&self) -> Vec<(u32, u32, u32, String, bool)> {
        let mut types: Vec<(u32, u32, u32, String)> = Vec::new();
        self.for_each_user_type(&mut |o, a, s, n| types.push((o, a, s, n.to_string())));
        let mut by_array: std::collections::HashMap<(u32, String), (u32, u32)> =
            std::collections::HashMap::new();
        for (o, a, s, n) in &types {
            if *a != 0 {
                by_array.insert((*s, self.array_typname(*a, n)), (*a, *o));
            }
        }
        let mut clashes: Vec<(u32, u32, u32, String, bool)> = Vec::new();
        for (o, a, s, n) in &types {
            if let Some(&(arr, elem)) = by_array.get(&(*s, n.clone()))
                && elem != *o
            {
                if *o > elem {
                    clashes.push((arr, *a, *s, n.clone(), false));
                } else if let Some((_, _, _, en)) = types.iter().find(|t| t.0 == elem) {
                    clashes.push((arr, 0, *s, en.clone(), true));
                }
            }
        }
        // (The late array types' names first, as they were made before.)
        clashes.sort_by_key(|c| !c.4);
        clashes
    }

    fn for_each_user_type(&self, f: &mut dyn FnMut(u32, u32, u32, &str)) {
        for e in self.enums.values() {
            f(e.oid, e.array_oid, e.schema, &e.name);
        }
        for c in self.composites.values() {
            f(c.oid, c.array_oid, c.schema, &c.name);
        }
        for d in self.domains.values() {
            f(d.oid, d.array_oid, d.schema, &d.name);
        }
        for b in self.base_types.values() {
            f(b.ty.oid, b.ty.array_oid, b.ty.schema, &b.ty.name);
        }
        for r in self.ranges.values() {
            f(r.oid, r.array_oid, r.schema, &r.name);
            f(
                r.multi_oid,
                r.multi_array_oid,
                self.multirange_schema(r),
                &r.multi_name,
            );
        }
        for t in self.tables.values() {
            f(t.rowtype, t.array_type(), t.schema, &t.name);
        }
        for v in self.views.values() {
            f(v.rowtype, v.array_type(), v.schema, &v.name);
        }
    }

    /// An enum or composite type, or its array type, by oid: (schema, name, is the array type).
    pub fn user_type_name(&self, oid: u32) -> Option<(u32, &str, bool)> {
        // Tables' and views' row types (which `user_type_entries` lists last) come from the
        // relation maps' indexes (`relindex`); the other types are few.
        let rel = || -> Option<(u32, &str, bool)> {
            let (rel, array) = *self.tables.ix().rowtypes.get(&oid)?;
            let t = self.tables.get(&rel)?;
            Some((t.schema, t.name.as_str(), array))
        };
        let view = || -> Option<(u32, &str, bool)> {
            let (rel, array) = *self.views.ix().rowtypes.get(&oid)?;
            let v = self.views.get(&rel)?;
            Some((v.schema, v.name.as_str(), array))
        };
        let found = if self.lookups.frozen {
            let m = self.lookups.user_types.get_or_init(|| {
                let mut m = HashMap::new();
                for (o, a, s, n) in self.type_entries() {
                    // (The first that has it, as the scan below finds it.)
                    if o != 0 {
                        m.entry(o).or_insert((s, n.to_string(), false));
                        if a != 0 {
                            m.entry(a).or_insert((s, n.to_string(), true));
                        }
                    }
                }
                m
            });
            m.get(&oid).map(|(s, n, a)| (*s, n.as_str(), *a))
        } else {
            self.type_entries()
                // (A shell type has no array type: array oid 0 names nothing.)
                .find(|(o, a, _, _)| *o != 0 && (*o == oid || (*a == oid && oid != 0)))
                .map(|(o, _, s, n)| (s, n, o != oid))
        };
        let found = found.or_else(rel).or_else(view);
        debug_assert_eq!(
            found,
            self.user_type_entries()
                .find(|(o, a, _, _)| *o != 0 && (*o == oid || (*a == oid && oid != 0)))
                .map(|(o, _, s, n)| (s, n, o != oid)),
            "user type index"
        );
        found
    }

    /// The types `user_type_name` names: (oid, array oid, schema, name), in the order it
    /// looks.
    fn user_type_entries(&self) -> impl Iterator<Item = (u32, u32, u32, &str)> + '_ {
        let tables = self
            .tables
            .values()
            .map(|t| (t.rowtype, t.array_type(), t.schema, t.name.as_str()));
        let views = self
            .views
            .values()
            .map(|v| (v.rowtype, v.array_type(), v.schema, v.name.as_str()));
        self.type_entries().chain(tables).chain(views)
    }

    /// `user_type_entries` but for tables' and views' row types.
    fn type_entries(&self) -> impl Iterator<Item = (u32, u32, u32, &str)> + '_ {
        let enums = self
            .enums
            .values()
            .map(|e| (e.oid, e.array_oid, e.schema, e.name.as_str()));
        let comps = self
            .composites
            .values()
            .map(|c| (c.oid, c.array_oid, c.schema, c.name.as_str()));
        let ranges = self.ranges.values().flat_map(|r| {
            [
                (r.oid, r.array_oid, r.schema, r.name.as_str()),
                (
                    r.multi_oid,
                    r.multi_array_oid,
                    self.multirange_schema(r),
                    r.multi_name.as_str(),
                ),
            ]
        });
        let domains = self
            .domains
            .values()
            .map(|d| (d.oid, d.array_oid, d.schema, d.name.as_str()));
        let bases = self
            .base_types
            .values()
            .map(|b| (b.ty.oid, b.ty.array_oid, b.ty.schema, &*b.ty.name));
        enums.chain(comps).chain(domains).chain(bases).chain(ranges)
    }

    /// Every oid `user_type_name` knows (types and their array types).
    pub fn user_type_oids(&self) -> std::collections::HashSet<u32> {
        let mut out = std::collections::HashSet::new();
        let mut add = |o: u32, a: u32| {
            if o != 0 {
                out.insert(o);
                if a != 0 {
                    out.insert(a);
                }
            }
        };
        self.enums.values().for_each(|e| add(e.oid, e.array_oid));
        self.composites.values().for_each(|c| add(c.oid, c.array_oid));
        self.domains.values().for_each(|d| add(d.oid, d.array_oid));
        self.base_types.values().for_each(|b| add(b.ty.oid, b.ty.array_oid));
        for r in self.ranges.values() {
            add(r.oid, r.array_oid);
            add(r.multi_oid, r.multi_array_oid);
        }
        self.tables.values().for_each(|t| add(t.rowtype, t.array_type()));
        self.views.values().for_each(|v| add(v.rowtype, v.array_type()));
        out
    }

    /// The value type of any type oid a client may name (a Parse message's parameter types):
    /// built-in, user-defined, a domain's base type, or an array of any of them.
    pub fn any_data_type(&self, oid: u32) -> Option<DataType> {
        if let Some(t) = DataType::from_oid(oid).or_else(|| self.user_data_type(oid)) {
            return Some(t);
        }
        if let Some((d, array)) = self.domain_type(oid) {
            return Some(if array {
                DataType::array_of(d.base.clone())
            } else {
                d.base.clone()
            });
        }
        // An array of a user type: find its element.
        let mut elem = None;
        let mut check = |o: u32, a: u32| {
            if a == oid && o != 0 && elem.is_none() {
                elem = Some(o);
            }
        };
        self.enums.values().for_each(|e| check(e.oid, e.array_oid));
        self.composites.values().for_each(|c| check(c.oid, c.array_oid));
        self.base_types.values().for_each(|b| check(b.ty.oid, b.ty.array_oid));
        for r in self.ranges.values() {
            check(r.oid, r.array_oid);
            check(r.multi_oid, r.multi_array_oid);
        }
        self.tables.values().for_each(|t| check(t.rowtype, t.array_type()));
        self.views.values().for_each(|v| check(v.rowtype, v.array_type()));
        let elem = elem?;
        self.user_data_type(elem).map(DataType::array_of)
    }

    /// The domain a type oid names, or the domain an array type `_d` holds: (domain, array).
    pub fn domain_type(&self, oid: u32) -> Option<(&Arc<Domain>, bool)> {
        if let Some(d) = self.domains.get(&oid) {
            return Some((d, false));
        }
        self.domains
            .values()
            .find(|d| d.array_oid == oid && oid != 0)
            .map(|d| (d, true))
    }

    /// The array-of-domain type a domain is declared over (through domains over domains).
    pub fn domain_array_base(&self, d: &Domain) -> Option<u32> {
        let mut cur = d;
        loop {
            if let Some(b) = cur.base_domain {
                return Some(b);
            }
            cur = self.domains.get(&cur.parent?)?;
        }
    }

    /// The type oid a domain's pg_type.typbasetype names: the domain or array-of-domain
    /// type it's declared over, else its base type's.
    pub fn domain_base_oid(&self, d: &Domain) -> u32 {
        d.parent
            .or(d.base_domain)
            .unwrap_or_else(|| system::type_oid(self, &d.base))
    }

    /// A domain's constraints in the order they are checked: NOT NULL (its own or an
    /// ancestor's), then the CHECKs of each level by name, ancestors first.
    pub fn domain_constraints(&self, d: &Domain) -> (bool, Vec<(String, String)>) {
        let mut not_null = false;
        let mut levels = Vec::new();
        let mut cur = Some(d);
        while let Some(x) = cur {
            not_null |= x.not_null;
            let mut checks = x.checks.clone();
            checks.sort_by(|a, b| a.0.cmp(&b.0));
            levels.push(checks);
            cur = x.parent.and_then(|p| self.domains.get(&p)).map(|p| &**p);
        }
        (not_null, levels.into_iter().rev().flatten().collect())
    }

    /// Whether domain `d` is `of` or declared (at any depth) over it.
    pub fn domain_derives(&self, d: &Domain, of: u32) -> bool {
        let mut cur = Some(d);
        while let Some(x) = cur {
            if x.oid == of {
                return true;
            }
            cur = x.parent.and_then(|p| self.domains.get(&p)).map(|p| &**p);
        }
        false
    }

    /// A type as format_type_be shows it (a domain, or an array of one, when `domain` is
    /// set): user types are schema-qualified unless visible through the path.
    pub fn format_type(&self, t: &DataType, domain: Option<u32>, search_path: &[String]) -> String {
        let oid = domain.unwrap_or_else(|| system::type_oid(self, t));
        match self.user_type_name(oid) {
            Some((schema, name, array)) => {
                let n = self.qualified_name(schema, name, search_path);
                if array { format!("{n}[]") } else { n }
            }
            None => t.display(),
        }
    }

    /// The type of a function's input parameter `i` as signatures show it.
    pub fn param_type_name(&self, f: &Function, i: usize, search_path: &[String]) -> String {
        match f.args.get(i) {
            Some(a) => self.format_type(&a.1, f.arg_domains.get(i).copied().flatten(), search_path),
            None => String::new(),
        }
    }

    /// The types of a function's input parameters as signatures show them.
    pub fn param_type_names(&self, f: &Function, search_path: &[String]) -> Vec<String> {
        (0..f.args.len())
            .map(|i| self.param_type_name(f, i, search_path))
            .collect()
    }

    /// A function's declared result type as signatures show it.
    pub fn ret_type_name(&self, f: &Function, search_path: &[String]) -> String {
        self.format_type(&f.ret, f.ret_domain, search_path)
    }

    /// A table's or view's row type.
    pub fn row_type_of(&self, rel: u32) -> Option<DataType> {
        let (oid, array_oid, name, fields): (u32, u32, &str, Vec<(String, DataType)>) =
            if let Some(t) = self.tables.get(&rel) {
                (
                    t.rowtype,
                    t.array_type(),
                    &t.name,
                    t.live_columns()
                        .map(|(_, c)| (c.name.clone(), c.ty.clone()))
                        .collect(),
                )
            } else if let Some(v) = self.views.get(&rel) {
                (v.rowtype, v.array_type(), &v.name, v.columns.clone())
            } else {
                return system::SysTable::by_oid(rel).and_then(|t| t.row_type());
            };
        if oid == 0 {
            return Some(DataType::row(fields));
        }
        Some(DataType::Row(Arc::new(crate::types::RowType {
            oid,
            array_oid,
            name: Some(Arc::from(name)),
            colls: Vec::new(),
            fields,
        })))
    }

    /// The type of an enum or composite type's oid.
    pub fn user_data_type(&self, oid: u32) -> Option<DataType> {
        if let Some(e) = self.enums.get(&oid) {
            return Some(e.data_type());
        }
        if let Some(b) = self.base_types.get(&oid) {
            return Some(b.data_type());
        }
        if let Some(c) = self.composites.get(&oid) {
            return Some(c.data_type());
        }
        if let Some(r) = self.ranges.get(&oid) {
            return Some(DataType::Range(r.range_type()));
        }
        if let Some(r) = self.ranges.values().find(|r| r.multi_oid == oid) {
            return Some(DataType::Multirange(r.range_type()));
        }
        let rel = self
            .tables
            .values()
            .find(|t| t.rowtype == oid)
            .map(|t| t.oid)
            .or_else(|| {
                self.views
                    .values()
                    .find(|v| v.rowtype == oid)
                    .map(|v| v.oid)
            });
        match rel {
            Some(rel) => self.row_type_of(rel),
            None => system::SysTable::by_rowtype(oid).and_then(|t| t.row_type()),
        }
    }

    pub fn put_table(&mut self, mut t: Table) {
        if t.rowtype == 0 {
            (t.rowtype, t.rowarray) = self.alloc_rowtype();
        }
        match self.tables.get(&t.oid) {
            None => self.class_moved(t.oid),
            Some(old) if class_row_changes(old, &t) => self.class_moved(t.oid),
            _ => {}
        }
        // A new foreign key's triggers set relhastriggers on both tables.
        let new_fks: Vec<u32> = t
            .constraints
            .iter()
            .filter_map(|c| match &c.kind {
                ConstraintKind::ForeignKey { ref_table, .. } => Some(*ref_table),
                _ => None,
            })
            .collect();
        if !new_fks.is_empty() {
            self.class_triggers(t.oid);
            for r in new_fks {
                self.class_triggers(r);
            }
        }
        self.assign_attrdefs(&mut t);
        self.put_name(t.schema, &t.name, RelKind::Table, t.oid);
        let old = self
            .tables
            .get(&t.oid)
            .map(|o| o.inherits.clone())
            .unwrap_or_default();
        // A parent's first child sets its relhassubclass (after the child's row is in).
        for p in &t.inherits {
            if self.class_has_subclass.insert(*p) && self.class_order.contains_key(p) {
                self.class_moved(*p);
            }
        }
        if old != t.inherits {
            self.unlink_children(t.oid, &old);
            for p in &t.inherits {
                let v = Arc::make_mut(&mut self.children).entry(*p).or_default();
                if let Err(i) = v.binary_search(&t.oid) {
                    v.insert(i, t.oid);
                }
            }
        }
        // A changed row type (columns dropped, added, retyped or renamed): signatures
        // naming it see the new one, as Postgres's, which look the type up, do.
        let row_changed = self.tables.get(&t.oid).is_some_and(|o| {
            o.rowtype == t.rowtype
                && !o
                    .live_columns()
                    .map(|(_, c)| (&c.name, &c.ty))
                    .eq(t.live_columns().map(|(_, c)| (&c.name, &c.ty)))
        });
        let (oid, rowtype) = (t.oid, t.rowtype);
        self.tables.insert(t.oid, Arc::new(t));
        if row_changed && let Some(new) = self.row_type_of(oid) {
            self.retype_signatures(rowtype, &new);
        }
    }

    /// Put row type `new` (of oid `rowtype`) in the user functions' signatures that use it.
    fn retype_signatures(&mut self, rowtype: u32, new: &DataType) {
        fn subst(t: &DataType, oid: u32, new: &DataType) -> Option<DataType> {
            match t {
                DataType::Row(r) if r.oid == oid => (t != new).then(|| new.clone()),
                DataType::Array(e) => subst(e, oid, new).map(DataType::array_of),
                _ => None,
            }
        }
        let uses: Vec<u32> = self
            .functions
            .values()
            .filter(|f| {
                f.ret.contains_row(rowtype)
                    || f.args.iter().any(|(_, t)| t.contains_row(rowtype))
                    || f.out_cols.iter().any(|(_, t)| t.contains_row(rowtype))
            })
            .map(|f| f.oid)
            .collect();
        for oid in uses {
            let Some(f) = self.functions.get_mut(&oid) else {
                continue;
            };
            let f = Arc::make_mut(f);
            if let Some(t) = subst(&f.ret, rowtype, new) {
                f.ret = t;
            }
            let types = f.args.iter_mut().map(|(_, t)| t);
            for t in types.chain(f.out_cols.iter_mut().map(|(_, t)| t)) {
                if let Some(nt) = subst(t, rowtype, new) {
                    *t = nt;
                }
            }
        }
    }

    /// Give the table a TOAST table (and its index) if it needs one and has none
    /// (AlterTableCreateToastTable).
    pub fn add_toast(&mut self, t: &mut Table) {
        if t.toast == 0 && t.needs_toast() {
            t.toast = self.alloc_oids(2);
        }
    }

    /// The oids rewriting a table into a new heap takes (make_new_heap, finish_heap_swap):
    /// the transient heap's, its array and row types', then its TOAST table's, which the table
    /// takes over (unless `by_content`: VACUUM FULL and CLUSTER swap the TOAST tables'
    /// contents when both have one), then a new file for each of its indexes.
    /// The table's relfilenode becomes the transient heap's oid; a TOAST table swapped by
    /// content keeps its oid and takes the new one's files.
    pub fn rewrite_heap(&mut self, t: &mut Table, by_content: bool) {
        let heap = self.alloc_oids(3);
        self.filenodes.insert(t.oid, heap);
        self.class_moved(t.oid);
        let old = t.toast;
        t.toast = 0;
        self.add_toast(t);
        if by_content && old != 0 && t.toast != 0 {
            let new = t.toast;
            t.toast = old;
            self.filenodes.insert(old, new);
            self.filenodes.insert(old + 1, new + 1);
        }
        self.new_index_files(t);
    }

    /// A relation's pg_class row was inserted, or updated other than in place: it's now the
    /// last in pg_class's heap.
    pub fn class_moved(&mut self, oid: u32) {
        self.class_tick += 1;
        self.class_order.insert(oid, self.class_tick);
    }

    /// A trigger was made on relation `oid`: the first sets relhastriggers.
    pub fn class_triggers(&mut self, oid: u32) {
        if self.class_has_triggers.insert(oid) {
            self.class_moved(oid);
        }
    }

    /// The relations in the order a pg_class heap scan returns them.
    pub fn class_scan_key(&self, oid: u32) -> (u64, u32) {
        (self.class_order.get(&oid).copied().unwrap_or(0), oid)
    }

    /// A new file for each of a table's indexes (reindex_relation), each taking an oid, in
    /// index oid order (RelationGetIndexList).
    pub fn new_index_files(&mut self, t: &Table) {
        let mut ixs: Vec<u32> = t.indexes.iter().chain(&t.pkey_index).copied().collect();
        ixs.sort_unstable();
        ixs.dedup();
        for ix in ixs {
            let node = self.alloc_oid();
            self.filenodes.insert(ix, node);
        }
    }

    /// A new file for a relation (RelationSetNewRelfilenumber), taking an oid.
    pub fn new_filenode(&mut self, rel: u32) {
        let node = self.alloc_oid();
        self.filenodes.insert(rel, node);
        if self.class_order.contains_key(&rel) {
            self.class_moved(rel);
        }
    }

    /// TRUNCATE's new files: the heap's, its TOAST table's and TOAST index's, then its
    /// indexes'.
    pub fn truncate_files(&mut self, t: &Table) {
        self.new_filenode(t.oid);
        if t.toast != 0 {
            self.new_filenode(t.toast);
            self.new_filenode(t.toast + 1);
        }
        self.new_index_files(t);
    }

    /// The user relations that may have storage of their own.
    pub fn storage_oids(&self) -> Vec<u32> {
        let mut v: Vec<u32> = (self.tables.keys())
            .chain(self.views.keys())
            .chain(self.indexes.keys())
            .chain(self.sequences.keys())
            .copied()
            .collect();
        for t in self.toasted() {
            v.extend([t.toast, t.toast + 1]);
        }
        v
    }

    /// Where a relation's storage is (pg_relation_filenode, pg_relation_filepath): (its file
    /// number, a shared catalog's, the temporary schema's backend number); None: no such
    /// relation, or one without storage.
    pub fn storage_location(&self, oid: u32) -> Option<(u32, bool, Option<u32>)> {
        if oid < FIRST_USER_OID {
            let rows = static_rows::rows(system::SysTable::PgClass);
            let r = rows.iter().find(|r| static_rows::oid_at(r, 0) == oid)?;
            if !matches!(r.get(17), Some(Value::Char(b'r' | b'i' | b't' | b'S' | b'm'))) {
                return None;
            }
            // A mapped catalog's file is its oid's (relmapper), until REINDEX gives it another.
            let node = match (self.filenodes.get(&oid), static_rows::oid_at(r, 7)) {
                (Some(n), _) => *n,
                (None, 0) => oid,
                (None, n) => n,
            };
            return Some((node, static_rows::oid_at(r, 8) == 1664, None));
        }
        let node = self.relfilenode(oid).filter(|n| *n != 0)?;
        let schema = if let Some(t) = self.index_table(oid) {
            t.schema
        } else if let Some(i) = self.indexes.get(&oid) {
            self.index_table(i.table).map_or(PUBLIC_OID, |t| t.schema)
        } else if let Some(s) = self.sequences.get(&oid) {
            s.schema
        } else if let Some((t, _)) = self.toast_owner(oid) {
            self.toast_schema(t)
        } else {
            PUBLIC_OID
        };
        let name = self.schema_name(schema);
        let backend = (name.strip_prefix("pg_temp_"))
            .or_else(|| name.strip_prefix("pg_toast_temp_"))
            .and_then(|n| n.parse().ok());
        Some((node, false, backend))
    }

    /// pg_class.relfilenode of a relation of this database (0: one without storage, or a
    /// mapped catalog); None: no such relation.
    pub fn relfilenode(&self, oid: u32) -> Option<u32> {
        if oid < FIRST_USER_OID {
            return static_rows::rows(system::SysTable::PgClass)
                .iter()
                .find(|r| static_rows::oid_at(r, 0) == oid)
                .map(|r| static_rows::oid_at(r, 7));
        }
        let stored = if let Some(t) = self.tables.get(&oid) {
            t.partkey.is_none()
        } else if let Some(v) = self.views.get(&oid) {
            v.storage.is_some()
        } else if let Some(i) = self.indexes.get(&oid) {
            self.index_table(i.table).is_none_or(|t| t.partkey.is_none())
        } else if self.sequences.contains_key(&oid) || self.toast_owner(oid).is_some() {
            true
        } else if self.composites.values().any(|c| c.relid == oid) {
            false
        } else {
            return None;
        };
        Some(if stored {
            self.filenodes.get(&oid).copied().unwrap_or(oid)
        } else {
            0
        })
    }

    /// Tables (and materialized views' storage) with a TOAST table.
    pub fn toasted(&self) -> impl Iterator<Item = &Arc<Table>> {
        self.tables
            .values()
            .chain(self.views.values().filter_map(|v| v.storage.as_ref()))
            .filter(|t| t.toast != 0)
    }

    /// The table whose TOAST table (false) or TOAST index (true) is called `name`, in
    /// schema `schema` or (None) on `search_path`.
    pub fn find_toast(
        &self,
        schema: Option<&str>,
        name: &str,
        search_path: &[String],
    ) -> Option<(Arc<Table>, bool)> {
        let path = match schema {
            Some(s) => vec![self.schema_by_name(s)?.oid],
            None => self.effective_path(search_path),
        };
        // (Rare: only names no other relation has get here.)
        self.toasted()
            .filter(|t| path.contains(&self.toast_schema(t)))
            .find_map(|t| {
                [false, true]
                    .into_iter()
                    .find(|&ix| t.toast_relname(ix) == name)
                    .map(|ix| (t.clone(), ix))
            })
    }

    /// The table a TOAST table or TOAST index belongs to: (table, is the index).
    pub fn toast_owner(&self, oid: u32) -> Option<(&Arc<Table>, bool)> {
        // (The relation maps' indexes: `relindex`.)
        let t = match self.tables.ix().toast.get(&oid) {
            Some(rel) => self.tables.get(rel)?,
            None => self.views.get(self.views.ix().toast.get(&oid)?)?.storage.as_ref()?,
        };
        debug_assert!(
            self.toasted()
                .find(|t| t.toast == oid || t.toast + 1 == oid)
                .is_some_and(|x| x.oid == t.oid),
            "toast index"
        );
        Some((t, t.toast != oid))
    }

    /// Mark the catalog as one that no longer changes (it is about to be published in an
    /// `Arc`): its lookups may keep hashes.
    pub fn freeze(&mut self) {
        self.lookups.frozen = true;
    }

    /// The partitions pending detach (DETACH PARTITION ... CONCURRENTLY's first transaction
    /// committed, its second not): (parent, partition).
    pub fn detach_pending(&self) -> Vec<(u32, u32)> {
        if self.lookups.frozen {
            self.lookups
                .detach_pending
                .get_or_init(|| self.scan_detach_pending())
                .clone()
        } else {
            self.scan_detach_pending()
        }
    }

    fn scan_detach_pending(&self) -> Vec<(u32, u32)> {
        self.tables
            .values()
            .filter(|t| t.detach_pending)
            .filter_map(|t| t.inherits.first().map(|p| (*p, t.oid)))
            .collect()
    }

    /// Whether a partition is pending detach (a cheap check for a frozen catalog).
    pub fn has_detach_pending(&self) -> bool {
        if self.lookups.frozen {
            !self
                .lookups
                .detach_pending
                .get_or_init(|| self.scan_detach_pending())
                .is_empty()
        } else {
            self.tables.values().any(|t| t.detach_pending)
        }
    }

    /// This catalog without the partitions `omitted` ((parent, partition) pairs) in their
    /// parents: what a query whose snapshot sees their detach marks plans with. Its version
    /// is a new one (caches keyed by the version tell it from the catalog itself).
    pub fn detach_view(self: &Arc<Self>, omitted: Vec<(u32, u32)>, routing_full: bool) -> Catalog {
        let mut c = (**self).clone();
        for (p, child) in &omitted {
            if let Some(v) = Arc::make_mut(&mut c.children).get_mut(p) {
                v.retain(|x| x != child);
            }
        }
        c.view = Some(Arc::new(DetachView {
            full: self.clone(),
            omitted,
            routing_full,
        }));
        c.bump();
        c
    }

    /// A catalog to edit and store: a view's left-out partitions back in their parents
    /// (those still partitions of them).
    pub fn unview(&mut self) {
        let Some(v) = self.view.take() else {
            return;
        };
        for (p, child) in &v.omitted {
            if self.tables.get(child).is_some_and(|t| t.inherits.contains(p)) {
                let list = Arc::make_mut(&mut self.children).entry(*p).or_default();
                if let Err(i) = list.binary_search(child) {
                    list.insert(i, *child);
                }
            }
        }
    }

    /// The catalog without any partition left out (a view's `full`, or this one).
    pub fn with_detached(self: &Arc<Self>) -> Arc<Catalog> {
        match &self.view {
            Some(v) => v.full.clone(),
            None => self.clone(),
        }
    }

    /// A partitioned table's partitions an insert routes to: those a REPEATABLE READ view
    /// left out too (ExecSetupPartitionTupleRouting includes detached partitions there).
    pub fn routing_partitions(&self, oid: u32) -> Vec<u32> {
        match &self.view {
            Some(v) if v.routing_full => v.full.partitions(oid),
            _ => self.partitions(oid),
        }
    }

    /// `acls` by object, for a frozen catalog (`Catalog::acl`).
    fn acl_lookup(&self) -> Option<&HashMap<(u32, u32, i32), usize>> {
        if !self.lookups.frozen {
            return None;
        }
        Some(self.lookups.acls.get_or_init(|| {
            let mut m = HashMap::with_capacity(self.acls.len());
            for (i, a) in self.acls.iter().enumerate() {
                m.entry((a.class, a.oid, a.sub)).or_insert(i);
            }
            m
        }))
    }

    /// Whether `oid` is a table's or view's row type (Some(false)) or the array type of one
    /// (Some(true)).
    pub fn rel_rowtype(&self, oid: u32) -> Option<bool> {
        // (The relation maps' indexes: `relindex`.)
        let found = (self.tables.ix().rowtypes.get(&oid))
            .or_else(|| self.views.ix().rowtypes.get(&oid))
            .map(|r| r.1);
        debug_assert_eq!(
            found,
            {
                let tables = self.tables.values().map(|t| (t.rowtype, t.array_type()));
                let views = self.views.values().map(|v| (v.rowtype, v.array_type()));
                let mut all = tables.chain(views).filter(|(r, _)| *r != 0);
                if all.clone().any(|(r, _)| r == oid) {
                    Some(false)
                } else {
                    all.any(|(_, a)| a == oid).then_some(true)
                }
            },
            "row type index"
        );
        found
    }

    /// The tables named `name` (in any schema) with row-level security on, and whether a
    /// view of that name is a security barrier: what makes a statement naming it run its
    /// quals at another security level.
    pub fn rls_named(&self, name: &str) -> (Vec<&Arc<Table>>, bool) {
        // (The relation maps' indexes: `relindex`.)
        let tables: Vec<&Arc<Table>> = match self.tables.ix().rls.get(&name.to_string()) {
            Some(c) => c.keys().filter_map(|o| self.tables.get(&o)).collect(),
            None => Vec::new(),
        };
        let barrier = self.views.ix().barriers.contains_key(&name.to_string());
        debug_assert!(
            tables.iter().map(|t| t.oid).eq(self
                .tables
                .values()
                .filter(|t| t.rls && t.name == name)
                .map(|t| t.oid))
                && barrier
                    == self
                        .views
                        .values()
                        .any(|v| v.name == name && v.option("security_barrier")),
            "row-level security index"
        );
        (tables, barrier)
    }

    /// The tables, views, sequences and indexes whose name `keep` accepts, through the name
    /// index (one probe per schema when `exact` names the one name to look for).
    pub fn relations_named(&self, exact: Option<&str>, keep: impl Fn(&str) -> bool) -> Vec<u32> {
        match exact {
            Some(n) => (self.schemas.keys())
                .filter_map(|s| self.names.get(&(*s, n.to_string())))
                .map(|(_, o)| *o)
                .collect(),
            None => (self.names.iter())
                .filter(|((_, n), _)| keep(n))
                .map(|(_, (_, o))| *o)
                .collect(),
        }
    }

    /// The composite type (CREATE TYPE ... AS) whose relation is `relid`.
    pub fn composite_of_rel(&self, relid: u32) -> Option<&Arc<CompositeType>> {
        if self.lookups.frozen {
            let m = self.lookups.composite_rels.get_or_init(|| {
                let mut m = HashMap::new();
                for c in self.composites.values() {
                    m.entry(c.relid).or_insert(c.oid);
                }
                m
            });
            return self.composites.get(m.get(&relid)?);
        }
        self.composites.values().find(|c| c.relid == relid)
    }

    /// The schema a table's TOAST table is in: pg_toast, or for a temporary table its
    /// session's pg_toast_temp_N.
    pub fn toast_schema(&self, t: &Table) -> u32 {
        if !t.temporary {
            return PG_TOAST_OID;
        }
        let name = self
            .schema_name(t.schema)
            .replacen("pg_temp_", "pg_toast_temp_", 1);
        self.schema_by_name(&name).map_or(PG_TOAST_OID, |s| s.oid)
    }

    /// Give columns with a default or generation expression a pg_attrdef oid (as
    /// StoreAttrDefault allocates one), and take it from those without. A column keeps its
    /// oid; one without takes the stored table's while the expression is the same (a copy
    /// made before the table was stored).
    fn assign_attrdefs(&mut self, t: &mut Table) {
        let old = self.tables.get(&t.oid).cloned();
        for (i, c) in t.columns.iter_mut().enumerate() {
            if c.dropped || (c.default.is_none() && c.generated.is_none()) {
                c.def_oid = 0;
            } else if c.def_oid == 0 {
                c.def_oid = match old.as_ref().and_then(|o| o.columns.get(i)) {
                    Some(o)
                        if o.def_oid != 0
                            && !o.dropped
                            && o.default == c.default
                            && o.generated == c.generated =>
                    {
                        o.def_oid
                    }
                    _ => self.alloc_oid(),
                };
            }
        }
    }

    fn unlink_children(&mut self, child: u32, parents: &[u32]) {
        for p in parents {
            if let Some(v) = Arc::make_mut(&mut self.children).get_mut(p) {
                v.retain(|c| *c != child);
                if v.is_empty() {
                    Arc::make_mut(&mut self.children).remove(p);
                }
            }
        }
    }

    /// Direct inheritance children of a table, in oid order.
    pub fn children(&self, oid: u32) -> &[u32] {
        self.children.get(&oid).map_or(&[], Vec::as_slice)
    }

    /// A table and all its descendants, breadth-first with each level in oid order (Postgres's
    /// find_all_inheritors), each once.
    /// A partitioned table's partitions in bound order.
    pub fn partitions(&self, oid: u32) -> Vec<u32> {
        let mut parts: Vec<(u32, &PartBound)> = self
            .children(oid)
            .iter()
            .filter_map(|c| {
                self.tables
                    .get(c)
                    .and_then(|t| t.partbound.as_ref().map(|b| (*c, b)))
            })
            .collect();
        let _g = self
            .tables
            .get(&oid)
            .map(|t| partition::under(partition::key_collations(self, t)));
        parts.sort_by(|a, b| partition::bound_order(a.1, b.1));
        parts.into_iter().map(|(c, _)| c).collect()
    }

    /// The tables a scan of `oid` reads, in plan order: inheritance children breadth-first,
    /// partitions depth-first in bound order (Postgres's expand_inherited_rtentry).
    pub fn expansion(&self, oid: u32) -> Vec<u32> {
        if !self.tables.get(&oid).is_some_and(|t| t.partkey.is_some()) {
            return self.inheritors(oid);
        }
        let mut out = vec![oid];
        for p in self.partitions(oid) {
            out.extend(self.expansion(p));
        }
        out
    }

    /// The partition-key ancestors of a partition, nearest first.
    pub fn partition_ancestors(&self, oid: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut cur = oid;
        // (get_partition_ancestors stops at a partition whose detach is pending.)
        while let Some(t) = self.tables.get(&cur)
            && t.partbound.is_some()
            && !t.detach_pending
            && let Some(p) = t.inherits.first()
            && !out.contains(p)
        {
            out.push(*p);
            cur = *p;
        }
        out
    }

    /// Whether function `handler` is a foreign-data wrapper handler with native code.
    pub fn fdw_handler_native(&self, handler: u32) -> bool {
        self.functions.get(&handler).is_some_and(|h| {
            h.probin
                .as_deref()
                .is_some_and(|b| crate::ext::native::fdw_handler(b, &h.body))
        })
    }

    /// The handler function of foreign table `t`'s wrapper (0: none; not a foreign table).
    pub fn fdw_handler_of(&self, t: &Table) -> u32 {
        t.foreign
            .as_ref()
            .and_then(|f| self.servers.get(&f.server))
            .and_then(|s| self.fdws.get(&s.fdw))
            .map_or(0, |w| w.handler)
    }

    /// CheckValidResultRel for foreign table `t`: why it can't take the rows a command
    /// (`what`: "insert into", "update", "delete from", "truncate") would change: no
    /// handler, or a wrapper that can't change rows (file_fdw). None for other tables.
    pub fn fdw_modify_error(&self, t: &Table, what: &str) -> Option<Error> {
        if let Some(e) = self.fdw_routine_error(t) {
            return Some(e);
        }
        // (postgres_fdw changes the remote table's rows.)
        let modifies = self.functions.get(&self.fdw_handler_of(t)).is_some_and(|h| h.body == "postgres_fdw_handler");
        (t.is_foreign() && !modifies).then(|| {
            Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("cannot {what} foreign table \"{}\"", t.name),
            )
        })
    }

    /// Why foreign table `t` can't be read or written (GetFdwRoutineForRelation): its
    /// wrapper has no handler, or one that can't run here. None for other tables.
    pub fn fdw_routine_error(&self, t: &Table) -> Option<Error> {
        let f = t.foreign.as_ref()?;
        let fdw = self.servers.get(&f.server).and_then(|s| self.fdws.get(&s.fdw));
        let name = fdw.map_or("", |w| w.name.as_str());
        // (Handlers the engine has native code for run.)
        if fdw.is_some_and(|w| self.fdw_handler_native(w.handler)) {
            return None;
        }
        Some(match fdw {
            Some(w) if w.handler != 0 => Error::new(
                code::FEATURE_NOT_SUPPORTED,
                format!("foreign-data wrapper \"{name}\" handlers are not supported"),
            ),
            _ => Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("foreign-data wrapper \"{name}\" has no handler"),
            ),
        })
    }

    pub fn inheritors(&self, oid: u32) -> Vec<u32> {
        let mut out = vec![oid];
        let mut i = 0;
        while i < out.len() {
            for c in self.children(out[i]) {
                if !out.contains(c) {
                    out.push(*c);
                }
            }
            i += 1;
        }
        out
    }

    pub fn put_view(&mut self, mut v: ViewDef) {
        if v.rowtype == 0 {
            (v.rowtype, v.rowarray) = self.alloc_rowtype();
        }
        if v.materialized && !self.views.contains_key(&v.oid) {
            self.class_moved(v.oid);
        }
        if v.rule == 0 && !self.views.contains_key(&v.oid) {
            v.rule = self.alloc_oid();
        }
        // A materialized view's storage goes by the view's name.
        if let Some(st) = &v.storage
            && (st.name != v.name || st.schema != v.schema || st.owner != v.owner)
        {
            v.storage = Some(Arc::new(Table {
                name: v.name.clone(),
                schema: v.schema,
                owner: v.owner,
                ..(**st).clone()
            }));
        }
        let kind = if v.materialized {
            RelKind::MaterializedView
        } else {
            RelKind::View
        };
        self.names.insert((v.schema, v.name.clone()), (kind, v.oid));
        self.views.insert(v.oid, Arc::new(v));
    }

    pub fn put_sequence(&mut self, s: Sequence) {
        self.names
            .insert((s.schema, s.name.clone()), (RelKind::Sequence, s.oid));
        self.sequences.insert(s.oid, Arc::new(s));
    }

    pub fn put_index(&mut self, i: Index) {
        let schema = self
            .index_table(i.table)
            .map(|t| t.schema)
            .unwrap_or(PUBLIC_OID);
        self.put_name(schema, &i.name, RelKind::Index, i.oid);
        self.indexes.insert(i.oid, Arc::new(i));
    }

    /// Record a relation's name (the shared map is copied only when it changes).
    fn put_name(&mut self, schema: u32, name: &str, kind: RelKind, oid: u32) {
        let key = (schema, name.to_string());
        if self.names.get(&key).is_none_or(|e| e.0 != kind || e.1 != oid) {
            self.names.insert(key, (kind, oid));
        }
    }

    pub fn remove_name(&mut self, schema: u32, name: &str) {
        let key = (schema, name.to_string());
        if self.names.contains_key(&key) {
            self.names.remove(&key);
        }
    }

    pub fn drop_table(&mut self, oid: u32) -> Option<Arc<Table>> {
        let t = self.tables.remove(&oid)?;
        self.array_names.remove(&t.array_type());
        self.class_order.remove(&oid);
        self.class_has_triggers.remove(&oid);
        self.remove_name(t.schema, &t.name);
        self.relstats.remove(&oid);
        self.colstats.remove(&oid);
        self.statistics.retain(|_, x| x.table != oid);
        if let Some(p) = t.pkey_index {
            self.relstats.remove(&p);
        }
        self.unlink_children(oid, &t.inherits);
        for i in &t.indexes {
            self.relstats.remove(i);
            self.colstats.remove(i);
            if let Some(idx) = self.indexes.remove(i) {
                self.remove_name(t.schema, &idx.name);
            }
        }
        // Owned sequences go with the table (`relindex::SeqIx::owned`).
        let owned: Vec<u32> = (self.sequences.ix().owned.get(&oid))
            .map(|v| (**v).clone())
            .unwrap_or_default();
        debug_assert_eq!(
            owned,
            self.sequences
                .values()
                .filter(|s| s.owned_by.map(|o| o.0) == Some(oid))
                .map(|s| s.oid)
                .collect::<Vec<u32>>(),
            "owned sequences index"
        );
        for s in owned {
            if let Some(seq) = self.sequences.remove(&s) {
                self.remove_name(seq.schema, &seq.name);
            }
        }
        if let Some(r) = t.rowid_seq
            && let Some(seq) = self.sequences.remove(&r)
        {
            self.remove_name(seq.schema, &seq.name);
        }
        self.comments_mut().retain(|(o, _, _)| *o != oid);
        Some(t)
    }

    /// ChooseRelationName: `base`'s first name no relation in the schema has, then the same
    /// with the label numbered (`t_a_key1`, `t_a_key2`…, truncated again each time).
    pub fn choose_name(&self, schema: u32, base: &ObjectName) -> String {
        base.choose(|n| self.name_taken(schema, n))
    }

    /// makeObjectName(`table`, the columns joined with `_` (none: no second part), `suffix`):
    /// `table_cols_suffix`, the longer part truncated first to fit NAMEDATALEN - 1 bytes.
    pub fn object_name(table: &str, cols: &[&str], suffix: &str) -> ObjectName {
        let name2 = (!cols.is_empty()).then(|| {
            // ChooseIndexNameAddition: stop once NAMEDATALEN bytes are there.
            let mut buf = String::new();
            for c in cols {
                if !buf.is_empty() {
                    buf.push('_');
                }
                buf.push_str(&truncate_ident(c));
                if buf.len() >= 64 {
                    break;
                }
            }
            buf
        });
        ObjectName::new(table, name2.as_deref(), suffix)
    }

    pub fn enum_by_oid(&self, oid: u32) -> Option<&Arc<EnumType>> {
        self.enums.get(&oid)
    }

    pub fn type_by_name(
        &self,
        schema: Option<u32>,
        name: &str,
        search_path: &[String],
    ) -> Option<u32> {
        let schemas = match schema {
            Some(s) => vec![s],
            None => self.effective_path(search_path),
        };
        for s in schemas {
            if let Some(e) = self
                .enums
                .values()
                .find(|e| e.schema == s && e.name == name)
            {
                return Some(e.oid);
            }
            if let Some(d) = self
                .domains
                .values()
                .find(|d| d.schema == s && d.name == name)
            {
                return Some(d.oid);
            }
            if let Some(c) = self
                .composites
                .values()
                .find(|c| c.schema == s && c.name == name)
            {
                return Some(c.oid);
            }
            if let Some(r) = self.ranges.values().find(|r| {
                r.schema == s && r.name == name
                    || self.multirange_schema(r) == s && r.multi_name == name
            }) {
                return Some(if r.name == name && r.schema == s {
                    r.oid
                } else {
                    r.multi_oid
                });
            }
            if let Some(b) = self
                .base_types
                .values()
                .find(|b| b.ty.schema == s && &*b.ty.name == name)
            {
                return Some(b.oid());
            }
        }
        None
    }

    pub fn role_by_name(&self, name: &str) -> Option<&Role> {
        self.roles.values().find(|r| r.name == name)
    }

    /// A function's name as `generate_function_name` shows it: unqualified if the search path
    /// finds this function first among those with its name and argument types.
    pub fn function_name(&self, f: &Function, search_path: &[String]) -> String {
        let same = |g: &Function| {
            g.name == f.name
                && g.args.len() == f.args.len()
                && g.args.iter().zip(&f.args).all(|(a, b)| a.1 == b.1)
        };
        // (FunctionIsVisible: the temporary schema is never searched for functions.)
        let first = self
            .effective_path(search_path)
            .into_iter()
            .filter(|s| !self.schema_name(*s).starts_with("pg_temp"))
            .find(|s| self.functions.values().any(|g| g.schema == *s && same(g)));
        if first == Some(f.schema) {
            quote_ident(&f.name)
        } else {
            // (get_namespace_name_or_temp: a temporary schema is `pg_temp`.)
            let schema = self.schema_name(f.schema);
            let schema = if schema.starts_with("pg_temp_") { "pg_temp" } else { schema };
            format!("{}.{}", quote_ident(schema), quote_ident(&f.name))
        }
    }

    pub fn qualified_name(&self, schema: u32, name: &str, search_path: &[String]) -> String {
        // Unqualified if the name resolves to this object through the path.
        let visible = self.effective_path(search_path).into_iter().find(|s| {
            self.names.contains_key(&(*s, name.to_string()))
                || self.type_by_name(Some(*s), name, &[]).is_some()
                || Self::system_relation_in(*s, name)
        }) == Some(schema);
        if visible {
            quote_ident(name)
        } else {
            format!(
                "{}.{}",
                quote_ident(self.schema_name(schema)),
                quote_ident(name)
            )
        }
    }

    /// A foreign key made for a partition of the referenced table, under the table's own
    /// key (it only carries the referenced side's actions).
    pub fn fk_is_derived(t: &Table, con: &Constraint) -> bool {
        con.parent != 0 && t.constraints.iter().any(|c| c.oid == con.parent)
    }

    /// A foreign key a partition has from its partitioned table (only checks its rows; the
    /// partitioned table's key acts for the referenced side).
    pub fn fk_is_clone(t: &Table, con: &Constraint) -> bool {
        con.parent != 0 && !Self::fk_is_derived(t, con)
    }

    /// The tables holding a table's rows: its leaf partitions, or itself.
    pub fn leaves(&self, oid: u32) -> Vec<Arc<Table>> {
        match self.tables.get(&oid) {
            Some(t) if t.partkey.is_some() => self
                .expansion(oid)
                .into_iter()
                .filter_map(|o| self.tables.get(&o).cloned())
                .filter(|l| l.partkey.is_none())
                .collect(),
            Some(t) => vec![t.clone()],
            None => Vec::new(),
        }
    }

    /// Relations that depend on `table` through foreign keys.
    pub fn referencing_fks(&self, table: u32) -> Vec<(Arc<Table>, Constraint)> {
        let mut out = Vec::new();
        // (The tables whose foreign keys reference it, by the tables map's index, in oid
        // order as the scan of every table found them.)
        let referencers = self.tables.ix().fk_refs.get(&table);
        for t in referencers.into_iter().flat_map(|c| c.keys()).filter_map(|o| self.tables.get(&o)) {
            for c in &t.constraints {
                if let ConstraintKind::ForeignKey { ref_table, .. } = &c.kind
                    && *ref_table == table
                {
                    out.push((t.clone(), c.clone()));
                }
            }
        }
        debug_assert_eq!(
            out.iter().map(|(t, c)| (t.oid, c.oid)).collect::<Vec<_>>(),
            (self.tables.values())
                .flat_map(|t| t.constraints.iter().map(move |c| (t, c)))
                .filter(|(_, c)| matches!(&c.kind, ConstraintKind::ForeignKey { ref_table, .. } if *ref_table == table))
                .map(|(t, c)| (t.oid, c.oid))
                .collect::<Vec<_>>(),
            "foreign keys referencing a table"
        );
        out
    }

    /// A new version for a changed catalog. Versions are unique in the process, not just
    /// increasing: after a rolled-back transaction's DDL, the next transaction's changes get
    /// versions the rolled-back catalogs never had, so caches keyed by the version (bound
    /// statements, PL/pgSQL plans) can't hand out a plan made for a different catalog.
    pub fn bump(&mut self) {
        use std::sync::atomic::{AtomicU64, Ordering};
        static LAST: AtomicU64 = AtomicU64::new(0);
        let mut cur = LAST.load(Ordering::Relaxed);
        loop {
            let next = cur.max(self.version) + 1;
            match LAST.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Relaxed) {
                Ok(_) => {
                    self.version = next;
                    return;
                }
                Err(c) => cur = c,
            }
        }
    }
}

/// One pg_depend row: (classid, objid, objsubid), (refclassid, refobjid, refobjsubid),
/// deptype.
pub type DependRow = ((u32, u32, i32), (u32, u32, i32), char);

/// Superuser DML on pg_depend (see `Catalog::depend_edits`).
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct DependEdits {
    #[serde(default)]
    pub removed: Vec<DependRow>,
    #[serde(default)]
    pub added: Vec<DependRow>,
}

impl DependEdits {
    pub fn is_empty(&self) -> bool {
        self.removed.is_empty() && self.added.is_empty()
    }

    /// DELETE of `row`: an inserted copy goes; a derived one is masked.
    pub fn remove(&mut self, row: DependRow) {
        if let Some(i) = self.added.iter().position(|r| *r == row) {
            self.added.remove(i);
        } else if !self.removed.contains(&row) {
            self.removed.push(row);
        }
    }

    /// INSERT of `row`.
    pub fn add(&mut self, row: DependRow) {
        self.added.push(row);
    }
}

/// A generated name, makeObjectName(name1, name2, label), with what it was made of: when it
/// is taken, ChooseRelationName / ChooseConstraintName number the label (`label1`, ...) and
/// make it again.
#[derive(Clone, Debug)]
pub struct ObjectName {
    name1: String,
    name2: Option<String>,
    label: String,
    name: String,
}

impl ObjectName {
    pub fn new(name1: &str, name2: Option<&str>, label: &str) -> Self {
        ObjectName {
            name1: name1.to_string(),
            name2: name2.map(str::to_string),
            label: label.to_string(),
            name: make_object_name(name1, name2, label),
        }
    }

    /// The name with the label numbered `pass` (0: as is).
    pub fn pass(&self, pass: usize) -> String {
        if pass == 0 {
            return self.name.clone();
        }
        make_object_name(&self.name1, self.name2.as_deref(), &format!("{}{pass}", self.label))
    }

    /// The first of the names (`pass` 0, 1, ...) that isn't `taken`.
    pub fn choose(&self, taken: impl Fn(&str) -> bool) -> String {
        (0..).map(|i| self.pass(i)).find(|n| !taken(n)).unwrap()
    }
}

impl std::ops::Deref for ObjectName {
    type Target = str;
    fn deref(&self) -> &str {
        &self.name
    }
}

impl std::fmt::Display for ObjectName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// makeObjectName: `name1_name2_label` (parts that are missing or empty left out with their
/// underscore), the longer of name1 / name2 shortened first so it fits NAMEDATALEN - 1
/// bytes, each clipped on a character boundary.
pub fn make_object_name(name1: &str, name2: Option<&str>, label: &str) -> String {
    let mut overhead = 0;
    let mut n1 = name1.len();
    let mut n2 = 0;
    if let Some(n) = name2 {
        n2 = n.len();
        overhead += 1;
    }
    if !label.is_empty() {
        overhead += label.len() + 1;
    }
    let avail = 63usize.saturating_sub(overhead);
    while n1 + n2 > avail {
        if n1 > n2 {
            n1 -= 1;
        } else {
            n2 -= 1;
        }
    }
    let clip = |s: &str, mut l: usize| {
        while !s.is_char_boundary(l) {
            l -= 1;
        }
        s[..l].to_string()
    };
    let mut out = clip(name1, n1);
    if let Some(n) = name2 {
        out.push('_');
        out.push_str(&clip(n, n2));
    }
    if !label.is_empty() {
        out.push('_');
        out.push_str(label);
    }
    out
}

/// `base` with `suffix` appended, `base` shortened (by whole characters) so the result fits
/// NAMEDATALEN - 1 bytes (set_rtable_names' `refname_N`).
pub fn suffixed_name(base: &str, suffix: &str) -> String {
    let mut end = base.len().min(63usize.saturating_sub(suffix.len()));
    while !base.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{suffix}", &base[..end])
}

/// makeArrayTypeName: `_` and the element type's name, truncated to NAMEDATALEN - 1 bytes.
pub fn array_type_name(elem: &str) -> String {
    truncate_ident(&format!("_{elem}"))
}

pub fn truncate_ident(s: &str) -> String {
    if s.len() <= 63 {
        return s.to_string();
    }
    let mut end = 63;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Quote an identifier if needed (quote_ident).
pub fn quote_ident(s: &str) -> String {
    let simple = !s.is_empty()
        && s.chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !is_reserved_keyword(s)
        && !is_col_name_keyword(s);
    if simple {
        s.to_string()
    } else {
        format!("\"{}\"", s.replace('"', "\"\""))
    }
}

/// Keywords that can name columns but not functions or types (`COL_NAME_KEYWORD`): quoted as
/// identifiers too.
pub fn is_col_name_keyword(s: &str) -> bool {
    matches!(
        s,
        "between"
            | "bigint"
            | "bit"
            | "boolean"
            | "char"
            | "character"
            | "coalesce"
            | "dec"
            | "decimal"
            | "exists"
            | "extract"
            | "float"
            | "greatest"
            | "grouping"
            | "inout"
            | "int"
            | "integer"
            | "interval"
            | "json"
            | "json_array"
            | "json_arrayagg"
            | "json_exists"
            | "json_object"
            | "json_objectagg"
            | "json_query"
            | "json_scalar"
            | "json_serialize"
            | "json_table"
            | "json_value"
            | "least"
            | "merge_action"
            | "national"
            | "nchar"
            | "none"
            | "normalize"
            | "nullif"
            | "numeric"
            | "out"
            | "overlay"
            | "position"
            | "precision"
            | "real"
            | "row"
            | "setof"
            | "smallint"
            | "substring"
            | "time"
            | "timestamp"
            | "treat"
            | "trim"
            | "values"
            | "varchar"
            | "xmlattributes"
            | "xmlconcat"
            | "xmlelement"
            | "xmlexists"
            | "xmlforest"
            | "xmlnamespaces"
            | "xmlparse"
            | "xmlpi"
            | "xmlroot"
            | "xmlserialize"
            | "xmltable"
    )
}

pub fn is_reserved_keyword(s: &str) -> bool {
    matches!(
        s,
        "all"
            | "analyse"
            | "analyze"
            | "and"
            | "any"
            | "array"
            | "as"
            | "asc"
            | "asymmetric"
            | "both"
            | "case"
            | "cast"
            | "check"
            | "collate"
            | "column"
            | "constraint"
            | "create"
            | "current_catalog"
            | "current_date"
            | "current_role"
            | "current_time"
            | "current_timestamp"
            | "current_user"
            | "default"
            | "deferrable"
            | "desc"
            | "distinct"
            | "do"
            | "else"
            | "end"
            | "except"
            | "false"
            | "fetch"
            | "for"
            | "foreign"
            | "from"
            | "grant"
            | "group"
            | "having"
            | "in"
            | "initially"
            | "intersect"
            | "into"
            | "lateral"
            | "leading"
            | "limit"
            | "localtime"
            | "localtimestamp"
            | "not"
            | "null"
            | "offset"
            | "on"
            | "only"
            | "or"
            | "order"
            | "placing"
            | "primary"
            | "references"
            | "returning"
            | "select"
            | "session_user"
            | "some"
            | "symmetric"
            | "system_user"
            | "table"
            | "then"
            | "to"
            | "trailing"
            | "true"
            | "union"
            | "unique"
            | "user"
            | "using"
            | "variadic"
            | "when"
            | "where"
            | "window"
            | "with"
            | "authorization"
            | "binary"
            | "collation"
            | "concurrently"
            | "cross"
            | "current_schema"
            | "freeze"
            | "full"
            | "ilike"
            | "inner"
            | "is"
            | "isnull"
            | "join"
            | "left"
            | "like"
            | "natural"
            | "notnull"
            | "outer"
            | "overlaps"
            | "right"
            | "similar"
            | "tablesample"
            | "verbose"
    )
}

pub fn check_new_name(c: &Catalog, schema: u32, name: &str, what: &str) -> Result<()> {
    if c.name_taken(schema, name) {
        let code = if what == "index" || what == "sequence" || what == "view" || what == "table" {
            code::DUPLICATE_TABLE
        } else {
            code::DUPLICATE_OBJECT
        };
        return Err(Error::new(
            code,
            format!("relation \"{name}\" already exists"),
        ));
    }
    Ok(())
}

pub fn no_such_relation(schema: Option<&str>, name: &str) -> Error {
    match schema {
        Some(s) => err!(UNDEFINED_TABLE, "relation \"{s}.{name}\" does not exist"),
        None => err!(UNDEFINED_TABLE, "relation \"{name}\" does not exist"),
    }
}

pub fn wrong_kind(name: &str, expected: &str) -> Error {
    err!(WRONG_OBJECT_TYPE, "\"{name}\" is not a {expected}")
}

pub fn ensure(cond: bool, e: impl FnOnce() -> Error) -> Result<()> {
    if cond { Ok(()) } else { Err(e()) }
}

pub fn not_supported(what: &str) -> Result<()> {
    bail!(FEATURE_NOT_SUPPORTED, "{what} is not supported yet")
}

/// Whether storing `new` over `old` updates the table's pg_class row other than in place:
/// its name, schema, owner, attribute count, TOAST table, check constraints, row security,
/// options, persistence or replica identity changed.
fn class_row_changes(old: &Table, new: &Table) -> bool {
    let checks = |t: &Table| {
        t.constraints
            .iter()
            .filter(|c| matches!(c.kind, ConstraintKind::Check { .. }))
            .count()
    };
    old.name != new.name
        || old.schema != new.schema
        || old.owner != new.owner
        || old.columns.len() != new.columns.len()
        || old.toast != new.toast
        || checks(old) != checks(new)
        || old.rls != new.rls
        || old.rls_force != new.rls_force
        || old.options != new.options
        || old.unlogged != new.unlogged
        || old.replident != new.replident
        || old.of_type != new.of_type
}
