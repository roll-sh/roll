//! Data types: identity (OIDs, names, categories), typmods and type-name parsing.

pub mod array;
pub mod binary;
pub mod cast;
pub mod collation;
pub mod datetime;
pub mod datum;
pub mod encnames;
pub mod mbconv;
pub mod extstats;
pub mod geo;
pub mod hash;
pub mod json;
pub mod jsonpath;
pub mod numeric;
pub mod ondisk;
pub mod range;
pub mod regex;
pub mod snapshot;
pub mod text;
pub mod tsearch;
pub mod tz;
pub mod value;
pub mod xml;

pub use numeric::Numeric;
pub use value::{Interval, Value};

use crate::error::{Result, code};
use crate::{bail, err};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::sync::Arc;

pub mod oid {
    pub const BOOL: u32 = 16;
    pub const BYTEA: u32 = 17;
    pub const CHAR: u32 = 18;
    pub const NAME: u32 = 19;
    pub const INT8: u32 = 20;
    pub const INT2: u32 = 21;
    pub const INT2VECTOR: u32 = 22;
    pub const INT4: u32 = 23;
    pub const REGPROC: u32 = 24;
    pub const TEXT: u32 = 25;
    pub const OID: u32 = 26;
    pub const TID: u32 = 27;
    pub const XID: u32 = 28;
    pub const CID: u32 = 29;
    pub const OIDVECTOR: u32 = 30;
    pub const JSON: u32 = 114;
    pub const XML: u32 = 142;
    pub const PG_NODE_TREE: u32 = 194;
    pub const PG_NDISTINCT: u32 = 3361;
    pub const PG_DEPENDENCIES: u32 = 3402;
    pub const PG_MCV_LIST: u32 = 5017;
    pub const GTSVECTOR: u32 = 3642;
    pub const PG_BRIN_BLOOM_SUMMARY: u32 = 4600;
    pub const PG_BRIN_MINMAX_MULTI_SUMMARY: u32 = 4601;
    pub const FLOAT4: u32 = 700;
    pub const FLOAT8: u32 = 701;
    pub const UNKNOWN: u32 = 705;
    pub const MONEY: u32 = 790;
    pub const INET: u32 = 869;
    pub const CIDR: u32 = 650;
    pub const MACADDR: u32 = 829;
    pub const MACADDR8: u32 = 774;
    pub const ACLITEM: u32 = 1033;
    pub const BPCHAR: u32 = 1042;
    pub const VARCHAR: u32 = 1043;
    pub const DATE: u32 = 1082;
    pub const TIME: u32 = 1083;
    pub const TIMESTAMP: u32 = 1114;
    pub const TIMESTAMPTZ: u32 = 1184;
    pub const INTERVAL: u32 = 1186;
    pub const TIMETZ: u32 = 1266;
    pub const BIT: u32 = 1560;
    pub const VARBIT: u32 = 1562;
    pub const NUMERIC: u32 = 1700;
    pub const REFCURSOR: u32 = 1790;
    pub const REGPROCEDURE: u32 = 2202;
    pub const REGOPER: u32 = 2203;
    pub const REGOPERATOR: u32 = 2204;
    pub const REGCLASS: u32 = 2205;
    pub const REGTYPE: u32 = 2206;
    pub const RECORD: u32 = 2249;
    pub const CSTRING: u32 = 2275;
    pub const ANY: u32 = 2276;
    pub const ANYARRAY: u32 = 2277;
    pub const VOID: u32 = 2278;
    pub const TRIGGER: u32 = 2279;
    pub const ANYELEMENT: u32 = 2283;
    pub const ANYENUM: u32 = 3500;
    pub const ANYNONARRAY: u32 = 2776;
    pub const UUID: u32 = 2950;
    pub const TSVECTOR: u32 = 3614;
    pub const TSQUERY: u32 = 3615;
    pub const REGCONFIG: u32 = 3734;
    pub const REGDICTIONARY: u32 = 3769;
    pub const REGCOLLATION: u32 = 4191;
    pub const JSONB: u32 = 3802;
    pub const JSONPATH: u32 = 4072;
    pub const REGNAMESPACE: u32 = 4089;
    pub const REGROLE: u32 = 4096;
    pub const ANYCOMPATIBLE: u32 = 5077;
    pub const ANYCOMPATIBLEARRAY: u32 = 5078;
    pub const PG_LSN: u32 = 3220;
    pub const XID8: u32 = 5069;
    pub const PG_SNAPSHOT: u32 = 5038;
    pub const TXID_SNAPSHOT: u32 = 2970;
    pub const INT4RANGE: u32 = 3904;
    pub const INT8RANGE: u32 = 3926;
    pub const ANYRANGE: u32 = 3831;
    pub const ANYMULTIRANGE: u32 = 4537;
    pub const ANYCOMPATIBLERANGE: u32 = 5080;
    pub const ANYCOMPATIBLEMULTIRANGE: u32 = 4538;
    pub const ANYCOMPATIBLENONARRAY: u32 = 5079;

    /// Pseudo-types only Postgres's internals take or return (no values of them exist here):
    /// (oid, name, typlen, typalign).
    pub const OTHER_PSEUDO: &[(u32, &str, i16, char)] = &[
        (32, "pg_ddl_command", 8, 'd'),
        (269, "table_am_handler", 4, 'i'),
        (325, "index_am_handler", 4, 'i'),
        (2280, "language_handler", 4, 'i'),
        (2281, "internal", 8, 'd'),
        (3115, "fdw_handler", 4, 'i'),
        (3310, "tsm_handler", 4, 'i'),
        (3838, "event_trigger", 4, 'i'),
        (5079, "anycompatiblenonarray", 4, 'i'),
    ];

    pub fn other_pseudo_name(oid: u32) -> Option<&'static str> {
        OTHER_PSEUDO.iter().find(|p| p.0 == oid).map(|p| p.1)
    }

    pub fn other_pseudo_oid(name: &str) -> Option<u32> {
        OTHER_PSEUDO.iter().find(|p| p.1 == name).map(|p| p.0)
    }
}

/// A composite type's fields; named composites (CREATE TYPE ... AS) also carry their identity.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RowType {
    /// The composite type's oid, or 0 for an anonymous row (`record`).
    pub oid: u32,
    pub array_oid: u32,
    /// Its name as `format_type` shows it.
    pub name: Option<Arc<str>>,
    pub fields: Vec<(String, DataType)>,
    /// An anonymous row's field collations other than the default, as its expressions
    /// derive them (the attcollations of the row type Postgres blesses for a ROW()); empty
    /// when there are none. A named composite's are in the catalog. Not part of the type's
    /// identity.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub colls: Vec<Option<Arc<str>>>,
}

impl PartialEq for RowType {
    fn eq(&self, o: &Self) -> bool {
        self.oid == o.oid && self.array_oid == o.array_oid && self.name == o.name && self.fields == o.fields
    }
}

impl Eq for RowType {}

impl std::hash::Hash for RowType {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.oid, self.array_oid, &self.name, &self.fields).hash(state)
    }
}

impl DataType {
    /// The composite type's oid, for a named composite.
    pub fn row_oid(&self) -> Option<u32> {
        match self {
            DataType::Row(r) if r.oid != 0 => Some(r.oid),
            _ => None,
        }
    }

    /// Whether values of this type hold composite values (directly, in arrays or ranges).
    pub fn holds_composite(&self) -> bool {
        match self {
            DataType::Row(_) => true,
            DataType::Array(e) => e.holds_composite(),
            DataType::Range(r) | DataType::Multirange(r) => r.subtype.holds_composite(),
            _ => false,
        }
    }

    /// Whether composite `oid` is this type, or an element or (nested) field of it.
    pub fn contains_row(&self, oid: u32) -> bool {
        match self {
            DataType::Row(r) => r.oid == oid || r.fields.iter().any(|(_, t)| t.contains_row(oid)),
            DataType::Array(e) => e.contains_row(oid),
            DataType::Range(rt) | DataType::Multirange(rt) => rt.subtype.contains_row(oid),
            _ => false,
        }
    }
}

impl std::ops::Deref for RowType {
    type Target = Vec<(String, DataType)>;
    fn deref(&self) -> &Self::Target {
        &self.fields
    }
}

/// A Postgres data type. Variants that accept a modifier carry it (`-1` = none), the way
/// `atttypmod` does.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DataType {
    Bool,
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    /// typmod = ((precision << 16) | scale) + 4
    Numeric(i32),
    Text,
    /// typmod = max length + 4
    Varchar(i32),
    /// typmod = length + 4
    Bpchar(i32),
    Name,
    /// The single-byte `"char"` type.
    Char,
    Bytea,
    Date,
    /// typmod = fractional-second precision
    Time(i32),
    Timetz(i32),
    Timestamp(i32),
    Timestamptz(i32),
    Interval(i32),
    Uuid,
    Json,
    /// XML documents, kept as text.
    Xml,
    /// A cursor (portal) name.
    Refcursor,
    Jsonb,
    /// SQL/JSON path; values are its canonical text.
    JsonPath,
    Oid,
    Regclass,
    Regtype,
    Regproc,
    Regprocedure,
    Regnamespace,
    Regrole,
    Regconfig,
    Regdictionary,
    Regoper,
    Regoperator,
    Regcollation,
    Int2Vector,
    OidVector,
    PgNodeTree,
    /// Extended statistics (pg_statistic_ext_data): Postgres's serialized forms, as bytea.
    PgNdistinct,
    PgDependencies,
    PgMcvList,
    /// Index-internal types (a GiST tsvector signature, BRIN summaries): they exist in
    /// pg_type, but their input functions reject every value, so only NULLs are seen.
    Gtsvector,
    PgBrinBloomSummary,
    PgBrinMinmaxMultiSummary,
    Xid,
    Tid,
    Cid,
    Aclitem,
    PgLsn,
    /// A full transaction id (epoch and xid).
    Xid8,
    /// pg_snapshot / txid_snapshot: which transactions were visible.
    PgSnapshot,
    TxidSnapshot,
    Inet,
    Cidr,
    Macaddr,
    Macaddr8,
    /// Full text search: a document and a query.
    TsVector,
    TsQuery,
    Point,
    Lseg,
    Path,
    GBox,
    Polygon,
    Line,
    Circle,
    Bit(i32),
    Varbit(i32),
    Money,
    Array(Box<DataType>),
    /// The `record` pseudo-type (function arguments, untyped rows).
    Record,
    /// A composite with known field names and types (ROW(...), whole-row references, table row
    /// types).
    Row(Arc<RowType>),
    /// A user enum type: its oid, name and array type's oid. (Catalogs stored before the
    /// array oid was kept have just the first two; their array type is oid + 1.)
    #[serde(deserialize_with = "de_enum")]
    Enum(u32, Arc<str>, u32),
    /// A range type, and the multirange over it.
    Range(Arc<range::RangeType>),
    Multirange(Arc<range::RangeType>),
    /// An untyped string literal whose type isn't decided yet.
    Unknown,
    Void,
    Cstring,
    Trigger,
    // Polymorphic pseudo-types, only used in function signatures.
    AnyElement,
    AnyArray,
    AnyNonArray,
    AnyCompatible,
    AnyCompatibleArray,
    AnyEnum,
    AnyRange,
    AnyMultirange,
    AnyCompatibleRange,
    AnyCompatibleMultirange,
    Any,
    /// `anycompatiblenonarray` (catalogs stored before it was a variant have it as a
    /// pseudo `Base`).
    AnyCompatibleNonArray,
    /// A base type made by CREATE TYPE with input and output functions, or a shell type
    /// (CREATE TYPE name).
    Base(Arc<BaseType>),
}

/// A base type CREATE TYPE made. Its values are those of `repr`, the built-in type whose
/// input and output functions its own (internal-language) ones are; a shell type has none.
/// Two are the same type when their oids are.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BaseType {
    pub oid: u32,
    pub name: Arc<str>,
    pub schema: u32,
    pub array_oid: u32,
    pub repr: Option<DataType>,
    /// typlen, typbyval, typalign, typstorage
    pub len: i16,
    pub byval: bool,
    pub align: char,
    pub storage: char,
    /// typcategory, typispreferred
    pub category: char,
    pub preferred: bool,
    pub collatable: bool,
    /// One of the pseudo-types only Postgres's internals pass (`internal`,
    /// `index_am_handler`, ...), named in signatures.
    #[serde(default)]
    pub pseudo: bool,
    /// An extension's type the engine implements natively (`crate::ext::types` kind; 0:
    /// none): its values are `Value::Ext` of that kind.
    #[serde(default, skip_serializing_if = "is_zero_u16")]
    pub ext: u16,
    /// The type modifier (`vector(3)`: 3; -1: none), as the variants of the built-in types
    /// with modifiers carry theirs. Not part of the type's identity (`eq` is by oid).
    #[serde(default = "no_typmod", skip_serializing_if = "is_no_typmod")]
    pub typmod: i32,
}

fn is_zero_u16(v: &u16) -> bool {
    *v == 0
}

fn no_typmod() -> i32 {
    -1
}

fn is_no_typmod(v: &i32) -> bool {
    *v == -1
}

impl PartialEq for BaseType {
    fn eq(&self, other: &Self) -> bool {
        self.oid == other.oid
    }
}

impl Eq for BaseType {}

impl std::hash::Hash for BaseType {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.oid.hash(state)
    }
}

impl BaseType {
    pub fn is_shell(&self) -> bool {
        self.repr.is_none() && !self.pseudo
    }

    /// A pseudo-type of `oid::OTHER_PSEUDO` (`internal`, ...), as a type of signatures.
    pub fn other_pseudo(oid: u32) -> Option<DataType> {
        let &(oid, name, len, align) = oid::OTHER_PSEUDO.iter().find(|p| p.0 == oid)?;
        Some(DataType::Base(Arc::new(BaseType {
            oid,
            name: Arc::from(name),
            schema: 11,
            array_oid: 0,
            repr: None,
            len,
            byval: true,
            align,
            storage: 'p',
            category: 'P',
            preferred: false,
            collatable: false,
            pseudo: true,
            ext: 0,
            typmod: -1,
        })))
    }
}

/// Postgres's `typcategory`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Array,
    Bool,
    Composite,
    DateTime,
    Enum,
    Geometric,
    Network,
    Numeric,
    Pseudo,
    Range,
    String,
    Timespan,
    User,
    BitString,
    Unknown,
    Internal,
}

impl Category {
    /// The category a typcategory letter names (others count as user-defined).
    pub fn from_char(c: char) -> Category {
        match c {
            'A' => Category::Array,
            'B' => Category::Bool,
            'C' => Category::Composite,
            'D' => Category::DateTime,
            'E' => Category::Enum,
            'G' => Category::Geometric,
            'I' => Category::Network,
            'N' => Category::Numeric,
            'P' => Category::Pseudo,
            'R' => Category::Range,
            'S' => Category::String,
            'T' => Category::Timespan,
            'V' => Category::BitString,
            'X' => Category::Unknown,
            'Z' => Category::Internal,
            _ => Category::User,
        }
    }
}

impl DataType {
    pub const NUMERIC: DataType = DataType::Numeric(-1);
    pub const VARCHAR: DataType = DataType::Varchar(-1);
    pub const TIMESTAMP: DataType = DataType::Timestamp(-1);
    pub const TIMESTAMPTZ: DataType = DataType::Timestamptz(-1);
    pub const TIME: DataType = DataType::Time(-1);
    pub const INTERVAL: DataType = DataType::Interval(-1);

    pub fn array_of(elem: DataType) -> DataType {
        DataType::Array(Box::new(elem))
    }

    pub fn oid(&self) -> u32 {
        use DataType::*;
        match self {
            Bool => oid::BOOL,
            Int2 => oid::INT2,
            Int4 => oid::INT4,
            Int8 => oid::INT8,
            Float4 => oid::FLOAT4,
            Float8 => oid::FLOAT8,
            Numeric(_) => oid::NUMERIC,
            Text => oid::TEXT,
            Varchar(_) => oid::VARCHAR,
            Bpchar(_) => oid::BPCHAR,
            Name => oid::NAME,
            Char => oid::CHAR,
            Bytea => oid::BYTEA,
            Date => oid::DATE,
            Time(_) => oid::TIME,
            Timetz(_) => oid::TIMETZ,
            Timestamp(_) => oid::TIMESTAMP,
            Timestamptz(_) => oid::TIMESTAMPTZ,
            Interval(_) => oid::INTERVAL,
            Uuid => oid::UUID,
            Json => oid::JSON,
            Xml => oid::XML,
            Refcursor => oid::REFCURSOR,
            Jsonb => oid::JSONB,
            JsonPath => oid::JSONPATH,
            Oid => oid::OID,
            Regclass => oid::REGCLASS,
            Regtype => oid::REGTYPE,
            Regproc => oid::REGPROC,
            Regprocedure => oid::REGPROCEDURE,
            Regnamespace => oid::REGNAMESPACE,
            Regrole => oid::REGROLE,
            Regconfig => oid::REGCONFIG,
            Regdictionary => oid::REGDICTIONARY,
            Regoper => oid::REGOPER,
            Regoperator => oid::REGOPERATOR,
            Regcollation => oid::REGCOLLATION,
            Int2Vector => oid::INT2VECTOR,
            OidVector => oid::OIDVECTOR,
            PgNodeTree => oid::PG_NODE_TREE,
            PgNdistinct => oid::PG_NDISTINCT,
            PgDependencies => oid::PG_DEPENDENCIES,
            PgMcvList => oid::PG_MCV_LIST,
            Gtsvector => oid::GTSVECTOR,
            PgBrinBloomSummary => oid::PG_BRIN_BLOOM_SUMMARY,
            PgBrinMinmaxMultiSummary => oid::PG_BRIN_MINMAX_MULTI_SUMMARY,
            Xid => oid::XID,
            Tid => oid::TID,
            Cid => oid::CID,
            Aclitem => oid::ACLITEM,
            PgLsn => oid::PG_LSN,
            Xid8 => oid::XID8,
            PgSnapshot => oid::PG_SNAPSHOT,
            TxidSnapshot => oid::TXID_SNAPSHOT,
            Inet => oid::INET,
            Cidr => oid::CIDR,
            Macaddr => oid::MACADDR,
            Macaddr8 => oid::MACADDR8,
            TsVector => oid::TSVECTOR,
            TsQuery => oid::TSQUERY,
            Point => 600,
            Lseg => 601,
            Path => 602,
            GBox => 603,
            Polygon => 604,
            Line => 628,
            Circle => 718,
            Bit(_) => oid::BIT,
            Varbit(_) => oid::VARBIT,
            Money => oid::MONEY,
            Array(e) => e.array_oid(),
            Row(r) if r.oid != 0 => r.oid,
            Record | Row(_) => oid::RECORD,
            Enum(o, ..) => *o,
            Range(r) => r.oid,
            Multirange(r) => r.multi_oid,
            Base(b) => b.oid,
            AnyRange => oid::ANYRANGE,
            AnyMultirange => oid::ANYMULTIRANGE,
            AnyCompatibleRange => oid::ANYCOMPATIBLERANGE,
            AnyCompatibleMultirange => oid::ANYCOMPATIBLEMULTIRANGE,
            AnyCompatibleNonArray => oid::ANYCOMPATIBLENONARRAY,
            AnyEnum => oid::ANYENUM,
            Unknown => oid::UNKNOWN,
            Void => oid::VOID,
            Cstring => oid::CSTRING,
            Trigger => oid::TRIGGER,
            AnyElement => oid::ANYELEMENT,
            AnyArray => oid::ANYARRAY,
            AnyNonArray => oid::ANYNONARRAY,
            AnyCompatible => oid::ANYCOMPATIBLE,
            AnyCompatibleArray => oid::ANYCOMPATIBLEARRAY,
            Any => oid::ANY,
        }
    }

    /// OID of the array type whose elements are `self`.
    pub fn array_oid(&self) -> u32 {
        use DataType::*;
        match self {
            Bool => 1000,
            Bytea => 1001,
            Char => 1002,
            Name => 1003,
            Int2 => 1005,
            Int2Vector => 1006,
            Int4 => 1007,
            Regproc => 1008,
            Text => 1009,
            Oid => 1028,
            Tid => 1010,
            Xid => 1011,
            Cid => 1012,
            OidVector => 1013,
            Bpchar(_) => 1014,
            Varchar(_) => 1015,
            Int8 => 1016,
            Float4 => 1021,
            Float8 => 1022,
            Aclitem => 1034,
            Inet => 1041,
            Cidr => 651,
            Macaddr => 1040,
            Macaddr8 => 775,
            TsVector => 3643,
            TsQuery => 3645,
            Gtsvector => 3644,
            Point => 1017,
            Lseg => 1018,
            Path => 1019,
            GBox => 1020,
            Polygon => 1027,
            Line => 629,
            Circle => 719,
            Date => 1182,
            Time(_) => 1183,
            Timestamp(_) => 1115,
            Timestamptz(_) => 1185,
            Interval(_) => 1187,
            Numeric(_) => 1231,
            Timetz(_) => 1270,
            Bit(_) => 1561,
            Varbit(_) => 1563,
            Money => 791,
            Regprocedure => 2207,
            Regclass => 2210,
            Regtype => 2211,
            Uuid => 2951,
            Json => 199,
            Xml => 143,
            Refcursor => 2201,
            Jsonb => 3807,
            JsonPath => 4073,
            Regnamespace => 4090,
            Regrole => 4097,
            Regconfig => 3735,
            Regdictionary => 3770,
            Regoper => 2208,
            Regoperator => 2209,
            Regcollation => 4192,
            PgLsn => 3221,
            Xid8 => 271,
            PgSnapshot => 5039,
            TxidSnapshot => 2949,
            Row(r) if r.oid != 0 => r.array_oid,
            Record | Row(_) => 2287,
            Enum(_, _, a) => *a,
            Range(r) => r.array_oid,
            Multirange(r) => r.multi_array_oid,
            Base(b) => b.array_oid,
            Cstring => 1263,
            _ => 0,
        }
    }

    pub fn from_oid(o: u32) -> Option<DataType> {
        use DataType::*;
        Some(match o {
            oid::BOOL => Bool,
            oid::BYTEA => Bytea,
            oid::CHAR => Char,
            oid::NAME => Name,
            oid::INT8 => Int8,
            oid::INT2 => Int2,
            oid::INT4 => Int4,
            oid::REGPROC => Regproc,
            oid::TEXT => Text,
            oid::OID => Oid,
            oid::TID => Tid,
            oid::XID => Xid,
            oid::CID => Cid,
            oid::INT2VECTOR => Int2Vector,
            oid::OIDVECTOR => OidVector,
            oid::JSON => Json,
            oid::XML => Xml,
            oid::REFCURSOR => Refcursor,
            oid::PG_NODE_TREE => PgNodeTree,
            oid::PG_NDISTINCT => PgNdistinct,
            oid::PG_DEPENDENCIES => PgDependencies,
            oid::PG_MCV_LIST => PgMcvList,
            oid::GTSVECTOR => Gtsvector,
            oid::PG_BRIN_BLOOM_SUMMARY => PgBrinBloomSummary,
            oid::PG_BRIN_MINMAX_MULTI_SUMMARY => PgBrinMinmaxMultiSummary,
            oid::FLOAT4 => Float4,
            oid::FLOAT8 => Float8,
            oid::UNKNOWN => Unknown,
            oid::MONEY => Money,
            oid::INET => Inet,
            oid::CIDR => Cidr,
            oid::MACADDR => Macaddr,
            oid::MACADDR8 => Macaddr8,
            oid::TSVECTOR => TsVector,
            oid::TSQUERY => TsQuery,
            600 => Point,
            601 => Lseg,
            602 => Path,
            603 => GBox,
            604 => Polygon,
            628 => Line,
            718 => Circle,
            oid::ACLITEM => Aclitem,
            oid::BPCHAR => Bpchar(-1),
            oid::VARCHAR => Varchar(-1),
            oid::DATE => Date,
            oid::TIME => Time(-1),
            oid::TIMESTAMP => Timestamp(-1),
            oid::TIMESTAMPTZ => Timestamptz(-1),
            oid::INTERVAL => Interval(-1),
            oid::TIMETZ => Timetz(-1),
            oid::BIT => Bit(-1),
            oid::VARBIT => Varbit(-1),
            oid::NUMERIC => Numeric(-1),
            oid::REGPROCEDURE => Regprocedure,
            oid::REGCLASS => Regclass,
            oid::REGTYPE => Regtype,
            oid::RECORD => Record,
            oid::CSTRING => Cstring,
            oid::ANY => Any,
            oid::ANYARRAY => AnyArray,
            oid::VOID => Void,
            oid::TRIGGER => Trigger,
            oid::ANYELEMENT => AnyElement,
            oid::ANYENUM => AnyEnum,
            oid::ANYNONARRAY => AnyNonArray,
            oid::ANYCOMPATIBLE => AnyCompatible,
            oid::ANYCOMPATIBLEARRAY => AnyCompatibleArray,
            oid::UUID => Uuid,
            oid::JSONB => Jsonb,
            oid::JSONPATH => JsonPath,
            oid::REGNAMESPACE => Regnamespace,
            oid::REGROLE => Regrole,
            oid::REGCONFIG => Regconfig,
            oid::REGDICTIONARY => Regdictionary,
            oid::REGOPER => Regoper,
            oid::REGOPERATOR => Regoperator,
            oid::REGCOLLATION => Regcollation,
            oid::PG_LSN => PgLsn,
            oid::XID8 => Xid8,
            oid::PG_SNAPSHOT => PgSnapshot,
            oid::TXID_SNAPSHOT => TxidSnapshot,
            oid::ANYRANGE => AnyRange,
            oid::ANYMULTIRANGE => AnyMultirange,
            oid::ANYCOMPATIBLERANGE => AnyCompatibleRange,
            oid::ANYCOMPATIBLEMULTIRANGE => AnyCompatibleMultirange,
            oid::ANYCOMPATIBLENONARRAY => AnyCompatibleNonArray,
            o if range::builtin_by_oid(o).is_some() => Range(range::builtin_by_oid(o).unwrap()),
            o if range::builtin_by_multi_oid(o).is_some() => {
                Multirange(range::builtin_by_multi_oid(o).unwrap())
            }
            _ => {
                // Array types.
                for t in Self::all_scalars() {
                    if t.array_oid() == o && o != 0 {
                        return Some(Array(Box::new(t)));
                    }
                }
                return None;
            }
        })
    }

    /// Every concrete scalar type (for catalog listings and reverse lookups).
    pub fn all_scalars() -> Vec<DataType> {
        use DataType::*;
        vec![
            Bool,
            Bytea,
            Char,
            Name,
            Int8,
            Int2,
            Int2Vector,
            Int4,
            Regproc,
            Text,
            Oid,
            Tid,
            Xid,
            Cid,
            OidVector,
            Json,
            Xml,
            Refcursor,
            PgNodeTree,
            Float4,
            Float8,
            Money,
            Macaddr,
            Macaddr8,
            Point,
            Lseg,
            Path,
            GBox,
            Polygon,
            Line,
            Circle,
            Inet,
            Cidr,
            Aclitem,
            Bpchar(-1),
            Varchar(-1),
            Date,
            Time(-1),
            Timestamp(-1),
            Timestamptz(-1),
            Interval(-1),
            Timetz(-1),
            Bit(-1),
            Varbit(-1),
            Numeric(-1),
            Regprocedure,
            Regclass,
            Regtype,
            Record,
            Cstring,
            Uuid,
            Jsonb,
            JsonPath,
            Regnamespace,
            Regrole,
            TsVector,
            TsQuery,
            Gtsvector,
            PgBrinBloomSummary,
            PgBrinMinmaxMultiSummary,
            Regconfig,
            Regdictionary,
            Regoper,
            Regoperator,
            Regcollation,
            PgLsn,
            Xid8,
            PgSnapshot,
            TxidSnapshot,
        ]
        .into_iter()
        .chain(
            range::builtins()
                .iter()
                .flat_map(|r| [Range(r.clone()), Multirange(r.clone())]),
        )
        .collect()
    }

    pub fn typmod(&self) -> i32 {
        use DataType::*;
        match self {
            Numeric(m) | Varchar(m) | Bpchar(m) | Time(m) | Timetz(m) | Timestamp(m)
            | Timestamptz(m) | Interval(m) | Bit(m) | Varbit(m) => *m,
            Base(b) => b.typmod,
            _ => -1,
        }
    }

    /// A base type with type modifier `m` (-1: none).
    pub fn base_with_typmod(b: &Arc<BaseType>, m: i32) -> DataType {
        if b.typmod == m {
            return DataType::Base(b.clone());
        }
        DataType::Base(Arc::new(BaseType { typmod: m, ..(**b).clone() }))
    }

    /// The type modifier as the catalogs show it (atttypmod, typtypmod): an array's is its
    /// element's (`character(5)[]`: 9).
    pub fn catalog_typmod(&self) -> i32 {
        match self {
            DataType::Array(e) => e.catalog_typmod(),
            t => t.typmod(),
        }
    }

    /// The same type without a modifier.
    pub fn without_typmod(&self) -> DataType {
        use DataType::*;
        match self {
            Numeric(_) => Numeric(-1),
            Varchar(_) => Varchar(-1),
            Bpchar(_) => Bpchar(-1),
            Time(_) => Time(-1),
            Timetz(_) => Timetz(-1),
            Timestamp(_) => Timestamp(-1),
            Timestamptz(_) => Timestamptz(-1),
            Interval(_) => Interval(-1),
            Bit(_) => Bit(-1),
            Varbit(_) => Varbit(-1),
            Base(b) if b.typmod != -1 => DataType::base_with_typmod(b, -1),
            Array(e) => Array(Box::new(e.without_typmod())),
            t => t.clone(),
        }
    }

    /// Same type identity, ignoring modifiers.
    pub fn same_base(&self, other: &DataType) -> bool {
        self.oid() == other.oid()
    }

    /// `typlen`: fixed size in bytes, -1 for varlena, -2 for C strings.
    pub fn typlen(&self) -> i16 {
        use DataType::*;
        match self {
            Bool | Char => 1,
            Int2 => 2,
            Int4 | Float4 | Oid | Regclass | Regtype | Regproc | Regprocedure | Regnamespace
            | Regrole | Regconfig | Regdictionary | Regoper | Regoperator | Regcollation | Date
            | Xid | Cid => 4,
            Int8 | Float8 | Money | Time(_) | Timestamp(_) | Timestamptz(_) | PgLsn | Xid8 => 8,
            Tid => 6,
            Macaddr => 6,
            Macaddr8 => 8,
            Point => 16,
            Lseg => 32,
            Path => -1,
            GBox => 32,
            Polygon => -1,
            Line => 24,
            Circle => 24,
            Timetz(_) => 12,
            Interval(_) | Uuid | Aclitem => 16,
            Name => 64,
            Unknown | Cstring => -2,
            Enum(..) => 4,
            Base(b) => b.len,
            Row(_) => -1,
            Void | Trigger | Any | AnyElement | AnyNonArray | AnyCompatible
            | AnyCompatibleNonArray | AnyEnum => 4,
            Record
            | AnyArray
            | AnyCompatibleArray
            | AnyRange
            | AnyMultirange
            | AnyCompatibleRange
            | AnyCompatibleMultirange => -1,
            _ => -1,
        }
    }

    /// The element type of a fixed-length type that subscripts like a 0-based array
    /// (point[0] is x; box[1] is the low corner).
    pub fn fixed_elem(&self) -> Option<DataType> {
        match self {
            DataType::Point | DataType::Line => Some(DataType::Float8),
            DataType::Lseg | DataType::GBox => Some(DataType::Point),
            // name is a fixed-length array of NAMEDATALEN "char"s (raw_array_subscript_handler).
            DataType::Name => Some(DataType::Char),
            _ => None,
        }
    }

    pub fn by_value(&self) -> bool {
        if let DataType::Base(b) = self {
            return b.byval;
        }
        matches!(self.typlen(), 1 | 2 | 4 | 8) && !matches!(self, DataType::Macaddr8)
    }

    pub fn category(&self) -> Category {
        use DataType::*;
        match self {
            Bool => Category::Bool,
            Int2 | Int4 | Int8 | Float4 | Float8 | Numeric(_) | Oid | Regclass | Regtype
            | Regproc | Regprocedure | Regnamespace | Regrole | Regconfig | Regdictionary
            | Regoper | Regoperator | Regcollation | Money => Category::Numeric,
            Text | Varchar(_) | Bpchar(_) | Name => Category::String,
            Char
            | PgNodeTree
            | PgNdistinct
            | PgDependencies
            | PgMcvList
            | PgBrinBloomSummary
            | PgBrinMinmaxMultiSummary => Category::Internal,
            Date | Time(_) | Timetz(_) | Timestamp(_) | Timestamptz(_) => Category::DateTime,
            Interval(_) => Category::Timespan,
            Array(_) | Int2Vector | OidVector => Category::Array,
            Record => Category::Pseudo,
            Row(_) => Category::Composite,
            Enum(..) => Category::Enum,
            Unknown => Category::Unknown,
            Inet | Cidr => Category::Network,
            Point | Lseg | Path | GBox | Polygon | Line | Circle => Category::Geometric,
            Bit(_) | Varbit(_) => Category::BitString,
            Void
            | Cstring
            | Trigger
            | Any
            | AnyElement
            | AnyArray
            | AnyNonArray
            | AnyCompatible
            | AnyCompatibleArray
            | AnyEnum
            | AnyRange
            | AnyMultirange
            | AnyCompatibleRange
            | AnyCompatibleMultirange
            | AnyCompatibleNonArray => Category::Pseudo,
            Range(_) | Multirange(_) => Category::Range,
            Bytea | Uuid | Json | Xml | Refcursor | Jsonb | JsonPath | Xid | Tid | Cid
            | Aclitem | PgLsn | Macaddr | Macaddr8 | TsVector | TsQuery | Gtsvector | Xid8
            | PgSnapshot | TxidSnapshot => Category::User,
            Base(b) if b.is_shell() => Category::Pseudo,
            Base(b) => Category::from_char(b.category),
        }
    }

    /// `typispreferred`.
    pub fn is_preferred(&self) -> bool {
        use DataType::*;
        if let Base(b) = self {
            return b.preferred;
        }
        matches!(
            self,
            Bool | Text | Float8 | Oid | Timestamptz(_) | Interval(_) | Varbit(_) | Inet
        )
    }

    pub fn category_char(&self) -> char {
        if let DataType::Base(b) = self
            && !b.is_shell()
        {
            return b.category;
        }
        match self.category() {
            Category::Array => 'A',
            Category::Bool => 'B',
            Category::Composite => 'C',
            Category::DateTime => 'D',
            Category::Enum => 'E',
            Category::Geometric => 'G',
            Category::Network => 'I',
            Category::Numeric => 'N',
            Category::Pseudo => 'P',
            Category::Range => 'R',
            Category::String => 'S',
            Category::Timespan => 'T',
            Category::User => 'U',
            Category::BitString => 'V',
            Category::Unknown => 'X',
            Category::Internal => 'Z',
        }
    }

    /// Whether a default btree or hash opclass gives the type the equality that grouping,
    /// DISTINCT and set operations need (`get_sort_group_operators`).
    pub fn has_equality(&self) -> bool {
        use DataType::*;
        match self {
            Array(e) => e.has_equality(),
            // A named composite's fields need it (an anonymous record's are looked at as
            // rows are compared).
            Row(r) if r.oid != 0 => r.fields.iter().all(|(_, t)| t.has_equality()),
            Json
            | Xml
            | JsonPath
            | Refcursor
            | Cstring
            | Point
            | Lseg
            | Path
            | GBox
            | Polygon
            | Line
            | Circle
            | Gtsvector
            | PgBrinBloomSummary
            | PgBrinMinmaxMultiSummary
            | PgNodeTree
            | PgNdistinct
            | PgDependencies
            | PgMcvList
            | PgSnapshot
            | TxidSnapshot => false,
            // A made base type has no operators unless some are made for it; an extension's
            // native type has its default operator classes' (its values' own).
            Base(b) => b.ext != 0,
            _ => true,
        }
    }

    pub fn is_polymorphic(&self) -> bool {
        use DataType::*;
        matches!(
            self,
            AnyElement
                | AnyArray
                | AnyNonArray
                | AnyCompatible
                | AnyCompatibleArray
                | AnyEnum
                | AnyRange
                | AnyMultirange
                | AnyCompatibleRange
                | AnyCompatibleMultirange
                | AnyCompatibleNonArray
        )
    }

    pub fn is_array(&self) -> bool {
        matches!(self, DataType::Array(_))
    }

    pub fn elem(&self) -> Option<&DataType> {
        match self {
            DataType::Array(e) => Some(e),
            _ => None,
        }
    }

    pub fn is_string(&self) -> bool {
        matches!(
            self,
            DataType::Text | DataType::Varchar(_) | DataType::Bpchar(_) | DataType::Name
        )
    }

    /// `type_is_collatable`: the string types and arrays of them.
    pub fn collatable(&self) -> bool {
        match self {
            DataType::Array(e) => e.collatable(),
            DataType::Base(b) => b.collatable,
            t => t.is_string() || t.text_like_internal(),
        }
    }

    /// The internal types stored as text (pg_node_tree) or with a text output that are
    /// collatable like text.
    fn text_like_internal(&self) -> bool {
        matches!(
            self,
            DataType::PgNodeTree
                | DataType::PgNdistinct
                | DataType::PgDependencies
                | DataType::PgMcvList
                | DataType::PgBrinBloomSummary
                | DataType::PgBrinMinmaxMultiSummary
        )
    }

    /// `typcollation`: "C" for name, the database default for the other collatable types.
    pub fn default_collation(&self) -> Option<&'static str> {
        match self {
            DataType::Array(e) => e.default_collation(),
            DataType::Name => Some("C"),
            DataType::Base(b) if b.collatable => Some("default"),
            t if t.is_string() || t.text_like_internal() => Some("default"),
            _ => None,
        }
    }

    pub fn is_integer(&self) -> bool {
        matches!(self, DataType::Int2 | DataType::Int4 | DataType::Int8)
    }

    pub fn is_numeric_category(&self) -> bool {
        self.category() == Category::Numeric
    }

    /// The internal name, as in `pg_type.typname`.
    pub fn typname(&self) -> String {
        use DataType::*;
        match self {
            Bool => "bool".into(),
            Int2 => "int2".into(),
            Int4 => "int4".into(),
            Int8 => "int8".into(),
            Float4 => "float4".into(),
            Float8 => "float8".into(),
            Numeric(_) => "numeric".into(),
            Text => "text".into(),
            Varchar(_) => "varchar".into(),
            Bpchar(_) => "bpchar".into(),
            Name => "name".into(),
            Char => "char".into(),
            Bytea => "bytea".into(),
            Date => "date".into(),
            Time(_) => "time".into(),
            Timetz(_) => "timetz".into(),
            Timestamp(_) => "timestamp".into(),
            Timestamptz(_) => "timestamptz".into(),
            Interval(_) => "interval".into(),
            Uuid => "uuid".into(),
            Json => "json".into(),
            Xml => "xml".into(),
            Refcursor => "refcursor".into(),
            Jsonb => "jsonb".into(),
            JsonPath => "jsonpath".into(),
            Oid => "oid".into(),
            Regclass => "regclass".into(),
            Regtype => "regtype".into(),
            Regproc => "regproc".into(),
            Regprocedure => "regprocedure".into(),
            Regnamespace => "regnamespace".into(),
            Regrole => "regrole".into(),
            Regconfig => "regconfig".into(),
            Regdictionary => "regdictionary".into(),
            Regoper => "regoper".into(),
            Regoperator => "regoperator".into(),
            Regcollation => "regcollation".into(),
            Int2Vector => "int2vector".into(),
            OidVector => "oidvector".into(),
            PgNodeTree => "pg_node_tree".into(),
            PgNdistinct => "pg_ndistinct".into(),
            PgDependencies => "pg_dependencies".into(),
            PgMcvList => "pg_mcv_list".into(),
            Gtsvector => "gtsvector".into(),
            PgBrinBloomSummary => "pg_brin_bloom_summary".into(),
            PgBrinMinmaxMultiSummary => "pg_brin_minmax_multi_summary".into(),
            Xid => "xid".into(),
            Tid => "tid".into(),
            Cid => "cid".into(),
            Aclitem => "aclitem".into(),
            PgLsn => "pg_lsn".into(),
            Xid8 => "xid8".into(),
            PgSnapshot => "pg_snapshot".into(),
            TxidSnapshot => "txid_snapshot".into(),
            Inet => "inet".into(),
            Cidr => "cidr".into(),
            Macaddr => "macaddr".into(),
            Macaddr8 => "macaddr8".into(),
            TsVector => "tsvector".into(),
            TsQuery => "tsquery".into(),
            Point => "point".into(),
            Lseg => "lseg".into(),
            Path => "path".into(),
            GBox => "box".into(),
            Polygon => "polygon".into(),
            Line => "line".into(),
            Circle => "circle".into(),
            Bit(_) => "bit".into(),
            Varbit(_) => "varbit".into(),
            Money => "money".into(),
            Array(e) => format!("_{}", e.typname()),
            Row(r) if r.name.is_some() => r.name.as_deref().unwrap().to_string(),
            Record | Row(_) => "record".into(),
            Enum(_, n, _) => n.to_string(),
            Range(r) => r.name.to_string(),
            Multirange(r) => r.multi_name.to_string(),
            Base(b) => b.name.to_string(),
            AnyRange => "anyrange".into(),
            AnyMultirange => "anymultirange".into(),
            AnyCompatibleRange => "anycompatiblerange".into(),
            AnyCompatibleMultirange => "anycompatiblemultirange".into(),
            AnyCompatibleNonArray => "anycompatiblenonarray".into(),
            AnyEnum => "anyenum".into(),
            Unknown => "unknown".into(),
            Void => "void".into(),
            Cstring => "cstring".into(),
            Trigger => "trigger".into(),
            AnyElement => "anyelement".into(),
            AnyArray => "anyarray".into(),
            AnyNonArray => "anynonarray".into(),
            AnyCompatible => "anycompatible".into(),
            AnyCompatibleArray => "anycompatiblearray".into(),
            Any => "any".into(),
        }
    }

    /// `format_type` without the modifier (`character varying`, `timestamp with time zone`…).
    pub fn sql_name(&self) -> String {
        use DataType::*;
        match self {
            Bool => "boolean".into(),
            Int2 => "smallint".into(),
            Int4 => "integer".into(),
            Int8 => "bigint".into(),
            Float4 => "real".into(),
            Float8 => "double precision".into(),
            Varchar(_) => "character varying".into(),
            Bpchar(_) => "character".into(),
            Char => "\"char\"".into(),
            Any => "\"any\"".into(),
            Time(_) => "time without time zone".into(),
            Timetz(_) => "time with time zone".into(),
            Timestamp(_) => "timestamp without time zone".into(),
            Timestamptz(_) => "timestamp with time zone".into(),
            Bit(_) => "bit".into(),
            Varbit(_) => "bit varying".into(),
            Array(e) => format!("{}[]", e.sql_name()),
            // User types are quoted as identifiers when needed.
            Enum(..) | Row(_) | Base(_) => crate::catalog::quote_ident(&self.typname()),
            Range(r) | Multirange(r) if r.schema != crate::catalog::PG_CATALOG_OID => {
                crate::catalog::quote_ident(&self.typname())
            }
            t => t.typname(),
        }
    }

    /// An anonymous row type with these fields.
    pub fn row(fields: Vec<(String, DataType)>) -> DataType {
        DataType::Row(Arc::new(RowType {
            oid: 0,
            array_oid: 0,
            name: None,
            fields,
            colls: Vec::new(),
        }))
    }

    /// `format_type(oid, typmod)`, as used in error messages and `\d`.
    pub fn display(&self) -> String {
        use DataType::*;
        let m = self.typmod();
        match self {
            Numeric(m) if *m >= 4 => {
                let (p, s) = numeric_typmod_parts(*m);
                format!("numeric({p},{s})")
            }
            Varchar(m) if *m >= 4 => format!("character varying({})", m - 4),
            Bpchar(m) if *m >= 4 => format!("character({})", m - 4),
            Bit(m) if *m >= 0 => format!("bit({m})"),
            Varbit(m) if *m >= 0 => format!("bit varying({m})"),
            Time(_) if m >= 0 => format!("time({m}) without time zone"),
            Timetz(_) if m >= 0 => format!("time({m}) with time zone"),
            Timestamp(_) if m >= 0 => format!("timestamp({m}) without time zone"),
            Timestamptz(_) if m >= 0 => format!("timestamp({m}) with time zone"),
            Interval(_) if m >= 0 => interval_typmod_display(m),
            Base(_) if m >= 0 => format!("{}({m})", self.sql_name()),
            Array(e) => format!("{}[]", e.display()),
            t => t.sql_name(),
        }
    }

    /// Resolve a type by name, as written in SQL (`int`, `varchar`, `timestamptz`, `pg_catalog.text`…).
    pub fn from_name(name: &str) -> Option<DataType> {
        use DataType::*;
        Some(match name {
            "bool" | "boolean" => Bool,
            "int2" | "smallint" | "smallserial" | "serial2" => Int2,
            "int4" | "int" | "integer" | "serial" | "serial4" => Int4,
            "int8" | "bigint" | "bigserial" | "serial8" => Int8,
            "float4" | "real" => Float4,
            "float8" | "double precision" => Float8,
            "numeric" | "decimal" => Numeric(-1),
            "text" => Text,
            "varchar" | "character varying" => Varchar(-1),
            "bpchar" | "char" | "character" => Bpchar(-1),
            "name" => Name,
            "\"char\"" => Char,
            "bytea" => Bytea,
            "date" => Date,
            "time" | "time without time zone" => Time(-1),
            "timetz" | "time with time zone" => Timetz(-1),
            "timestamp" | "timestamp without time zone" => Timestamp(-1),
            "timestamptz" | "timestamp with time zone" => Timestamptz(-1),
            "interval" => Interval(-1),
            "uuid" => Uuid,
            "json" => Json,
            "xml" => Xml,
            "refcursor" => Refcursor,
            "jsonb" => Jsonb,
            "jsonpath" => JsonPath,
            "oid" => Oid,
            "regclass" => Regclass,
            "regtype" => Regtype,
            "regproc" => Regproc,
            "regprocedure" => Regprocedure,
            "regnamespace" => Regnamespace,
            "regrole" => Regrole,
            "regconfig" => Regconfig,
            "regdictionary" => Regdictionary,
            "regoper" => Regoper,
            "regoperator" => Regoperator,
            "regcollation" => Regcollation,
            "int2vector" => Int2Vector,
            "oidvector" => OidVector,
            "pg_node_tree" => PgNodeTree,
            "pg_ndistinct" => PgNdistinct,
            "pg_dependencies" => PgDependencies,
            "pg_mcv_list" => PgMcvList,
            "gtsvector" => Gtsvector,
            "pg_brin_bloom_summary" => PgBrinBloomSummary,
            "pg_brin_minmax_multi_summary" => PgBrinMinmaxMultiSummary,
            "xid" => Xid,
            "tid" => Tid,
            "cid" => Cid,
            "aclitem" => Aclitem,
            "pg_lsn" => PgLsn,
            "xid8" => Xid8,
            "pg_snapshot" => PgSnapshot,
            "txid_snapshot" => TxidSnapshot,
            "inet" => Inet,
            "cidr" => Cidr,
            "macaddr" => Macaddr,
            "macaddr8" => Macaddr8,
            "tsvector" => TsVector,
            "tsquery" => TsQuery,
            "point" => Point,
            "lseg" => Lseg,
            "path" => Path,
            "box" => GBox,
            "polygon" => Polygon,
            "line" => Line,
            "circle" => Circle,
            "bit" => Bit(-1),
            "varbit" | "bit varying" => Varbit(-1),
            "money" => Money,
            "record" => Record,
            "unknown" => Unknown,
            "void" => Void,
            "cstring" => Cstring,
            "trigger" => Trigger,
            "anyelement" => AnyElement,
            "anyenum" => AnyEnum,
            "anyrange" => AnyRange,
            "anymultirange" => AnyMultirange,
            "anycompatiblerange" => AnyCompatibleRange,
            "anycompatiblemultirange" => AnyCompatibleMultirange,
            "anycompatiblenonarray" => AnyCompatibleNonArray,
            "anyarray" => AnyArray,
            "anynonarray" => AnyNonArray,
            "anycompatible" => AnyCompatible,
            "anycompatiblearray" => AnyCompatibleArray,
            "any" => Any,
            _ => {
                if let Some(r) = range::builtin(name) {
                    return Some(Range(r));
                }
                if let Some(r) = range::builtin_multi(name) {
                    return Some(Multirange(r));
                }
                if let Some(e) = name.strip_prefix('_') {
                    return Self::from_name(e).map(|e| Array(Box::new(e)));
                }
                return None;
            }
        })
    }

    /// Apply a modifier list as written in SQL (`varchar(10)`, `numeric(10,2)`, `timestamp(3)`).
    pub fn with_modifiers(self, mods: &[i64], type_name: &str) -> Result<DataType> {
        use DataType::*;
        if mods.is_empty() {
            return Ok(match (self, type_name) {
                // `char` without a length means char(1).
                (Bpchar(_), "bpchar") => Bpchar(-1),
                (Bpchar(_), _) => Bpchar(5),
                (Bit(_), "bit") => Bit(1),
                (t, _) => t,
            });
        }
        let t = match self {
            Numeric(_) => {
                if mods.len() > 2 {
                    bail!(INVALID_PARAMETER_VALUE, "invalid NUMERIC type modifier");
                }
                let p = mods[0];
                let s = mods.get(1).copied().unwrap_or(0);
                if !(1..=1000).contains(&p) {
                    bail!(
                        INVALID_PARAMETER_VALUE,
                        "NUMERIC precision {p} must be between 1 and 1000"
                    );
                }
                if !(-1000..=1000).contains(&s) {
                    bail!(
                        INVALID_PARAMETER_VALUE,
                        "NUMERIC scale {s} must be between -1000 and 1000"
                    );
                }
                Numeric((((p as i32) << 16) | (s as i32 & 0x7ff)) + 4)
            }
            Varchar(_) | Bpchar(_) => {
                let n = mods[0];
                let kind = if matches!(self, Varchar(_)) {
                    "varchar"
                } else {
                    "char"
                };
                if n < 1 {
                    bail!(
                        INVALID_PARAMETER_VALUE,
                        "length for type {kind} must be at least 1"
                    );
                }
                if n > 10_485_760 {
                    bail!(
                        INVALID_PARAMETER_VALUE,
                        "length for type {kind} cannot exceed 10485760"
                    );
                }
                if matches!(self, Varchar(_)) {
                    Varchar(n as i32 + 4)
                } else {
                    Bpchar(n as i32 + 4)
                }
            }
            Bit(_) => Bit(mods[0] as i32),
            Varbit(_) => Varbit(mods[0] as i32),
            Time(_) | Timetz(_) | Timestamp(_) | Timestamptz(_) => {
                // anytime_typmod_check / anytimestamp_typmod_check.
                let p = mods[0];
                let what = if matches!(self, Time(_) | Timetz(_)) {
                    "TIME"
                } else {
                    "TIMESTAMP"
                };
                let tz = if matches!(self, Timetz(_) | Timestamptz(_)) {
                    " WITH TIME ZONE"
                } else {
                    ""
                };
                if p < 0 {
                    bail!(
                        INVALID_PARAMETER_VALUE,
                        "{what}({p}){tz} precision must not be negative"
                    );
                }
                if p > 6 {
                    let mut w = crate::error::Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!("{what}({p}){tz} precision reduced to maximum allowed, 6"),
                    );
                    w.severity = crate::error::Severity::Warning;
                    crate::types::tsearch::queue(w);
                }
                let p = p.min(6) as i32;
                match self {
                    Time(_) => Time(p),
                    Timetz(_) => Timetz(p),
                    Timestamp(_) => Timestamp(p),
                    _ => Timestamptz(p),
                }
            }
            Interval(_) => Interval(mods[0] as i32),
            Float4 | Float8 => {
                // float(p): p <= 24 → real, else double precision.
                let p = mods[0];
                if p < 1 {
                    bail!(
                        INVALID_PARAMETER_VALUE,
                        "precision for type float must be at least 1 bit"
                    );
                }
                if p > 53 {
                    bail!(
                        INVALID_PARAMETER_VALUE,
                        "precision for type float must be less than 54 bits"
                    );
                }
                if p <= 24 { Float4 } else { Float8 }
            }
            Base(b) if b.ext != 0 && crate::ext::types::has_typmod_in(b.ext) => {
                let m = crate::ext::types::typmod_in(b.ext, mods)?;
                DataType::base_with_typmod(&b, m)
            }
            t => {
                return Err(err!(
                    SYNTAX_ERROR,
                    "type modifier is not allowed for type \"{}\"",
                    t.typname()
                ));
            }
        };
        Ok(t)
    }

    /// Size/precision limits for wire RowDescription.
    pub fn wire_size(&self) -> i16 {
        self.typlen()
    }
}

/// Field names and types of a composite value of type `ty` with `n` fields (`f1`, `f2`, … for
/// anonymous records).
pub fn record_field_types(ty: &DataType, n: usize) -> Vec<(String, DataType)> {
    match ty {
        DataType::Row(fields) if fields.len() == n => fields.fields.clone(),
        _ => (1..=n)
            .map(|i| (format!("f{i}"), DataType::Unknown))
            .collect(),
    }
}

pub fn numeric_typmod_parts(m: i32) -> (i32, i32) {
    let t = m - 4;
    let p = (t >> 16) & 0xffff;
    // Scale is an 11-bit signed field.
    let s = ((t & 0x7ff) ^ 1024) - 1024;
    (p, s)
}

fn interval_typmod_display(m: i32) -> String {
    // Low 16 bits: precision (0xFFFF = none); high bits: range mask.
    let prec = m & 0xffff;
    let range = (m >> 16) & 0x7fff;
    let fields = interval_range_name(range);
    let mut s = "interval".to_string();
    if let Some(f) = fields {
        s.push(' ');
        s.push_str(f);
    }
    if prec != 0xffff {
        s.push_str(&format!("({prec})"));
    }
    s
}

pub mod interval_fields {
    pub const MONTH: i32 = 1 << 1;
    pub const YEAR: i32 = 1 << 2;
    pub const DAY: i32 = 1 << 3;
    pub const HOUR: i32 = 1 << 10;
    pub const MINUTE: i32 = 1 << 11;
    pub const SECOND: i32 = 1 << 12;
    pub const FULL_RANGE: i32 = 0x7fff;
}

fn interval_range_name(range: i32) -> Option<&'static str> {
    use interval_fields::*;
    Some(match range {
        YEAR => "year",
        MONTH => "month",
        DAY => "day",
        HOUR => "hour",
        MINUTE => "minute",
        SECOND => "second",
        r if r == YEAR | MONTH => "year to month",
        r if r == DAY | HOUR => "day to hour",
        r if r == DAY | HOUR | MINUTE => "day to minute",
        r if r == DAY | HOUR | MINUTE | SECOND => "day to second",
        r if r == HOUR | MINUTE => "hour to minute",
        r if r == HOUR | MINUTE | SECOND => "hour to second",
        r if r == MINUTE | SECOND => "minute to second",
        _ => return None,
    })
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.display())
    }
}

/// Error for a type that exists in Postgres but isn't implemented here.
pub fn type_unsupported(name: &str) -> crate::error::Error {
    crate::error::Error::new(
        code::FEATURE_NOT_SUPPORTED,
        format!("type \"{name}\" is not supported yet"),
    )
}

/// `DataType::Enum`'s fields: (oid, name, array oid), or (oid, name) as stored before the
/// array oid was kept, when the array type's oid was always the enum's + 1.
fn de_enum<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<(u32, Arc<str>, u32), D::Error> {
    struct V;
    impl<'de> serde::de::Visitor<'de> for V {
        type Value = (u32, Arc<str>, u32);
        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an enum type's oid, name and array oid")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut s: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            use serde::de::Error;
            let oid: u32 = s
                .next_element()?
                .ok_or_else(|| A::Error::invalid_length(0, &self))?;
            let name: Arc<str> = s
                .next_element()?
                .ok_or_else(|| A::Error::invalid_length(1, &self))?;
            let array = s.next_element::<u32>()?.unwrap_or(oid + 1);
            Ok((oid, name, array))
        }
    }
    d.deserialize_seq(V)
}

#[cfg(test)]
mod enum_serde_tests {
    use super::DataType;

    #[test]
    fn enum_array_oid_round_trips_and_defaults() {
        let t = DataType::Enum(70001, "e".into(), 70000);
        let s = serde_json::to_string(&t).unwrap();
        assert_eq!(serde_json::from_str::<DataType>(&s).unwrap(), t);
        // Stored before the array oid was kept: the array type followed the type.
        let old: DataType = serde_json::from_str(r#"{"Enum":[70001,"e"]}"#).unwrap();
        assert_eq!(old, DataType::Enum(70001, "e".into(), 70002));
        assert_eq!(old.array_oid(), 70002);
    }
}
