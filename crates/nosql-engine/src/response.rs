//! What statements return: results, result metadata, prepared statement metadata, schema change
//! descriptions and events. Engine types; the server converts them to protocol bodies.

use bytes::Bytes;
use nosql_types::CqlType;

use crate::error::Consistency;

/// A bound value: serialised bytes, null, or unset (protocol v4+).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Param {
    Set(Bytes),
    Null,
    Unset,
}

impl Param {
    pub fn new(b: impl Into<Bytes>) -> Self {
        Param::Set(b.into())
    }
}

/// QUERY / EXECUTE / BATCH parameters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueryParams {
    pub consistency: Consistency,
    pub values: Vec<Param>,
    /// Names for the values (`names_for_values`).
    pub names: Option<Vec<String>>,
    pub skip_metadata: bool,
    /// Rows per page; `None` or non-positive: no paging.
    pub page_size: Option<i32>,
    pub paging_state: Option<Bytes>,
    pub serial_consistency: Option<Consistency>,
    /// The client-supplied default timestamp (microseconds).
    pub timestamp: Option<i64>,
    /// Per-query keyspace (v5).
    pub keyspace: Option<String>,
    /// Per-query "now" (v5).
    pub now_in_seconds: Option<u32>,
}

impl QueryParams {
    pub fn with_values(values: Vec<Param>) -> Self {
        QueryParams {
            values,
            ..Default::default()
        }
    }

    pub fn paged(mut self, page_size: i32) -> Self {
        self.page_size = Some(page_size);
        self
    }
}

/// A column of a result or a bind variable.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ColumnSpec {
    pub keyspace: String,
    pub table: String,
    pub name: String,
    pub ty: CqlType,
}

impl ColumnSpec {
    pub fn new(
        keyspace: impl Into<String>,
        table: impl Into<String>,
        name: impl Into<String>,
        ty: CqlType,
    ) -> Self {
        ColumnSpec {
            keyspace: keyspace.into(),
            table: table.into(),
            name: name.into(),
            ty,
        }
    }
}

/// The metadata of a Rows result (also the result metadata of a prepared SELECT).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct ResultMetadata {
    pub columns: Vec<ColumnSpec>,
}

impl ResultMetadata {
    /// The result metadata id (v5): an MD5 over the column specs.
    pub fn id(&self) -> [u8; 16] {
        use md5::{Digest, Md5};
        let mut h = Md5::new();
        for c in &self.columns {
            h.update(c.keyspace.as_bytes());
            h.update([0]);
            h.update(c.table.as_bytes());
            h.update([0]);
            h.update(c.name.as_bytes());
            h.update([0]);
            h.update(nosql_types::cql_name(&c.ty).as_bytes());
            h.update([1]);
        }
        let d = h.finalize();
        let mut out = [0u8; 16];
        out.copy_from_slice(&d[..16]);
        out
    }
}

/// A Rows result.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Rows {
    pub metadata: ResultMetadata,
    /// One `Option<Bytes>` per column (`None` is null).
    pub rows: Vec<Vec<Option<Bytes>>>,
    /// Where the next page starts; `None` on the last page.
    pub paging_state: Option<Bytes>,
}

/// `CREATED`, `UPDATED`, `DROPPED`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeType {
    Created,
    Updated,
    Dropped,
}

impl ChangeType {
    pub fn name(self) -> &'static str {
        match self {
            ChangeType::Created => "CREATED",
            ChangeType::Updated => "UPDATED",
            ChangeType::Dropped => "DROPPED",
        }
    }
}

/// What changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChangeTarget {
    Keyspace,
    Table,
    Type,
    Function,
    Aggregate,
}

impl ChangeTarget {
    pub fn name(self) -> &'static str {
        match self {
            ChangeTarget::Keyspace => "KEYSPACE",
            ChangeTarget::Table => "TABLE",
            ChangeTarget::Type => "TYPE",
            ChangeTarget::Function => "FUNCTION",
            ChangeTarget::Aggregate => "AGGREGATE",
        }
    }
}

/// A schema change (RESULT body and event).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SchemaChange {
    pub change: ChangeType,
    pub target: ChangeTarget,
    pub keyspace: String,
    /// Table, type, function or aggregate name; `None` for keyspaces.
    pub name: Option<String>,
    /// Argument types of a function or aggregate.
    pub arg_types: Option<Vec<String>>,
}

impl SchemaChange {
    pub fn keyspace(change: ChangeType, keyspace: impl Into<String>) -> Self {
        SchemaChange {
            change,
            target: ChangeTarget::Keyspace,
            keyspace: keyspace.into(),
            name: None,
            arg_types: None,
        }
    }

    pub fn object(
        change: ChangeType,
        target: ChangeTarget,
        keyspace: impl Into<String>,
        name: impl Into<String>,
    ) -> Self {
        SchemaChange {
            change,
            target,
            keyspace: keyspace.into(),
            name: Some(name.into()),
            arg_types: None,
        }
    }
}

/// The body of a successful response.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResponseBody {
    Void,
    Rows(Rows),
    SetKeyspace(String),
    SchemaChange(SchemaChange),
}

/// A statement's result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Response {
    pub body: ResponseBody,
    pub warnings: Vec<String>,
    pub tracing_id: Option<[u8; 16]>,
    pub custom_payload: Vec<(String, Option<Bytes>)>,
    /// The result of a conditional statement or batch (`[applied]` first). Its columns depend
    /// on whether it applied, so Cassandra never reports `METADATA_CHANGED` for it
    /// (CASSANDRA-13992).
    pub conditional: bool,
}

impl Response {
    pub fn new(body: ResponseBody) -> Self {
        Response {
            body,
            warnings: Vec::new(),
            tracing_id: None,
            custom_payload: Vec::new(),
            conditional: false,
        }
    }

    pub fn void() -> Self {
        Response::new(ResponseBody::Void)
    }

    pub fn rows(&self) -> Option<&Rows> {
        match &self.body {
            ResponseBody::Rows(r) => Some(r),
            _ => None,
        }
    }

    pub fn with_warnings(mut self, w: Vec<String>) -> Self {
        self.warnings = w;
        self
    }
}

/// A PREPARED result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Prepared {
    pub id: Bytes,
    /// The bind variables' specs.
    pub bind: Vec<ColumnSpec>,
    /// Indexes of the bind variables that form the partition key, in key order.
    pub pk_indexes: Vec<u16>,
    /// `None` for statements that return no rows (sent as NO_METADATA).
    pub result: Option<ResultMetadata>,
    pub result_metadata_id: [u8; 16],
    /// A conditional statement (ScyllaDB's LWT metadata flag).
    pub is_lwt: bool,
    /// The tracing session of a traced PREPARE ([`crate::Session::set_tracing`]).
    pub tracing_id: Option<[u8; 16]>,
}

/// Events pushed to registered connections.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    SchemaChange(SchemaChange),
    /// `NEW_NODE`, `REMOVED_NODE`, `MOVED_NODE`.
    TopologyChange {
        change: &'static str,
        address: std::net::SocketAddr,
    },
    /// `UP`, `DOWN`.
    StatusChange {
        change: &'static str,
        address: std::net::SocketAddr,
    },
}

/// A BATCH request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    pub kind: BatchKind,
    pub statements: Vec<BatchStatement>,
    pub params: QueryParams,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchKind {
    Logged,
    Unlogged,
    Counter,
}

/// One statement of a BATCH: query text or a prepared id, with its values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchStatement {
    Query { text: String, values: Vec<Param> },
    Prepared { id: Bytes, values: Vec<Param> },
}

/// Who is connecting (for `system.clients` and auth).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientInfo {
    pub address: Option<std::net::SocketAddr>,
    pub driver_name: Option<String>,
    pub driver_version: Option<String>,
    pub protocol_version: u8,
    /// The STARTUP options in the order the client sent them (`client_options` of
    /// `system.clients` / `system_views.clients`).
    pub options: Vec<(String, String)>,
    /// ScyllaDB's `system.clients.client_type` (`None`: `cql`; Alternator connections are
    /// `alternator`).
    pub client_type: Option<String>,
    /// The client certificate the TLS handshake verified (certificate authenticators,
    /// [`crate::auth::certificate`]).
    pub certificate: Option<std::sync::Arc<crate::auth::certificate::ClientCertificate>>,
}
