//! Table schemas: what Alternator remembers about a table.
//!
//! ScyllaDB derives this from the CQL schema of `alternator_T.T` (key columns, the views
//! `T:gsi` / `T!:lsi` that implement indexes, CDC options for streams) and from the
//! `scylla_tags` table extension, which also holds Alternator's own `system:` tags (billing
//! mode, provisioned throughput, TTL attribute, write isolation...). The engine integrator
//! persists a [`TableSchema`] that way; the in-memory backend just keeps the struct.

use crate::json::Json;

/// The type of a key attribute.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ScalarType {
    S,
    N,
    B,
}

impl ScalarType {
    pub fn parse(s: &str) -> Option<ScalarType> {
        match s {
            "S" => Some(ScalarType::S),
            "N" => Some(ScalarType::N),
            "B" => Some(ScalarType::B),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            ScalarType::S => "S",
            ScalarType::N => "N",
            ScalarType::B => "B",
        }
    }

    /// The CQL column type.
    pub fn cql_type(self) -> &'static str {
        match self {
            ScalarType::S => "text",
            ScalarType::N => "decimal",
            ScalarType::B => "blob",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyAttr {
    pub name: String,
    pub ty: ScalarType,
}

/// A primary key or index key: a hash key and an optional range key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeySchema {
    pub hash: KeyAttr,
    pub range: Option<KeyAttr>,
}

impl KeySchema {
    /// The key attribute names, hash first.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        std::iter::once(self.hash.name.as_str()).chain(self.range.as_ref().map(|r| r.name.as_str()))
    }

    pub fn contains(&self, name: &str) -> bool {
        self.names().any(|n| n == name)
    }

    pub fn attr(&self, name: &str) -> Option<&KeyAttr> {
        if self.hash.name == name {
            return Some(&self.hash);
        }
        self.range.as_ref().filter(|r| r.name == name)
    }

    pub fn to_json(&self) -> Json {
        let mut v = vec![
            Json::object()
                .with("AttributeName", Json::str(self.hash.name.clone()))
                .with("KeyType", Json::str("HASH")),
        ];
        if let Some(r) = &self.range {
            v.push(
                Json::object()
                    .with("AttributeName", Json::str(r.name.clone()))
                    .with("KeyType", Json::str("RANGE")),
            );
        }
        Json::Array(v)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndexKind {
    Global,
    Local,
}

#[derive(Clone, Debug, PartialEq)]
pub struct IndexSchema {
    pub name: String,
    pub kind: IndexKind,
    pub key: KeySchema,
    /// As requested (ScyllaDB projects every attribute regardless, and reports `ALL`).
    pub projection: Json,
    /// `ProvisionedThroughput` as given, if any.
    pub provisioned: Option<(u64, u64)>,
    /// `WarmThroughput` as given, if any.
    pub warm: Option<Json>,
    /// A GSI added by UpdateTable is `CREATING` (backfilling) until this time.
    pub ready_at_ms: i64,
}

impl IndexSchema {
    /// The CQL name of the view: `T:gsi` or `T!:lsi`.
    pub fn view_name(&self, table: &str) -> String {
        match self.kind {
            IndexKind::Global => format!("{table}:{}", self.name),
            IndexKind::Local => format!("{table}!:{}", self.name),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StreamSpec {
    pub view_type: String,
    /// Identifies this enabling of the stream (its ARN and label derive from it).
    pub created_at_ms: i64,
    pub enabled: bool,
    /// When DescribeTable starts showing the stream's ARN (a stream enabled by UpdateTable
    /// shows up once its CDC generation is in place, about a second later).
    pub visible_at_ms: i64,
    /// A stream enabled by UpdateTable while vector indexes exist is reported as disabled
    /// until the last vector index is dropped (as ScyllaDB does).
    pub reported_disabled: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TableSchema {
    pub name: String,
    /// The CQL table id (a timeuuid).
    pub id: uuid::Uuid,
    /// Milliseconds since the epoch.
    pub created_at_ms: i64,
    pub key: KeySchema,
    /// Every attribute with a known type (keys of the table and of its indexes), in the order
    /// DescribeTable lists them.
    pub attribute_definitions: Vec<KeyAttr>,
    pub gsis: Vec<IndexSchema>,
    pub lsis: Vec<IndexSchema>,
    pub billing_mode: String,
    pub billing_changed_ms: i64,
    /// ReadCapacityUnits, WriteCapacityUnits (any `i32`; not enforced).
    pub provisioned: (i64, i64),
    pub stream: Option<StreamSpec>,
    /// User tags and `system:` tags, in insertion order.
    pub tags: Vec<(String, String)>,
    pub ttl_attribute: Option<String>,
    /// Table-level options remembered verbatim, returned by DescribeTable where ScyllaDB does.
    pub options: Vec<(String, Json)>,
    /// Vector indexes, in the order DescribeTable lists them.
    pub vector_indexes: Vec<VectorIndex>,
    /// Whether the table uses tablets (decided at creation by `system:initial_tablets`).
    pub tablets: bool,
}

/// A vector index (ScyllaDB implements them as CQL custom indexes served by a vector store).
#[derive(Clone, Debug, PartialEq)]
pub struct VectorIndex {
    pub name: String,
    pub attribute: String,
    pub dimensions: u32,
    /// `COSINE` (the default), `EUCLIDEAN` or `DOT_PRODUCT`.
    pub similarity: String,
}

impl TableSchema {
    /// The table's ARN, as ScyllaDB forms it.
    pub fn arn(&self) -> String {
        format!(
            "arn:scylla:alternator:alternator_{0}:scylla:table/{0}",
            self.name
        )
    }

    pub fn index(&self, name: &str) -> Option<&IndexSchema> {
        self.gsis.iter().chain(&self.lsis).find(|i| i.name == name)
    }

    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    pub fn option(&self, key: &str) -> Option<&Json> {
        self.options.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Every attribute that is a key of the table or of one of its indexes.
    pub fn is_any_key(&self, name: &str) -> bool {
        self.key.contains(name)
            || self
                .gsis
                .iter()
                .chain(&self.lsis)
                .any(|i| i.key.contains(name))
    }

    pub fn attr_type(&self, name: &str) -> Option<ScalarType> {
        self.attribute_definitions
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.ty)
    }
}
