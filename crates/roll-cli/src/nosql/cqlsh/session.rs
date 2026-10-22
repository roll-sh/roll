//! The connection cqlsh runs on (a `nosql_proto` client) and the text of its errors, which
//! follows the Python driver's exception classes (`cassandra/protocol.py` and
//! `cassandra/__init__.py` of the DataStax driver bundled with Apache Cassandra, Apache-2.0).

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use nosql_proto::client::{Client, ClientConfig, ClientError, TlsConfig};
use nosql_proto::error_body::{ErrorBody, ErrorDetail, code};
use nosql_proto::{Compression, Consistency, Query, QueryParams, Request, RequestFrame, Response, ResultBody, Rows, Version};

/// How to reach a server.
#[derive(Clone, Debug)]
pub struct Target {
    pub host: String,
    pub port: u16,
    pub credentials: Option<(String, String)>,
    pub tls: Option<TlsConfig>,
    pub protocol: Option<Version>,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

impl Target {
    pub fn addr(&self) -> String {
        if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]:{}", self.host, self.port)
        } else {
            format!("{}:{}", self.host, self.port)
        }
    }

    pub async fn connect(&self) -> Result<Client, ClientError> {
        let mut cfg = ClientConfig::new(self.addr());
        if let Some(v) = self.protocol {
            cfg = cfg.version(v);
        }
        cfg = cfg.compression(Some(Compression::Lz4));
        if let Some((u, p)) = &self.credentials {
            cfg = cfg.credentials(u.clone(), p.clone());
        }
        if let Some(t) = &self.tls {
            cfg = cfg.tls(t.clone());
        }
        cfg.connect_timeout = self.connect_timeout;
        cfg.request_timeout = Some(self.request_timeout);
        cfg.startup.retain(|(k, _)| k != "DRIVER_NAME" && k != "DRIVER_VERSION");
        cfg.startup.push(("DRIVER_NAME".into(), "roll cqlsh".into()));
        cfg.startup.push(("DRIVER_VERSION".into(), env!("CARGO_PKG_VERSION").into()));
        Client::connect(cfg).await
    }
}

/// A failed request.
#[derive(Debug)]
pub enum Failure {
    Server(ErrorBody),
    Client(String),
}

impl From<ClientError> for Failure {
    fn from(e: ClientError) -> Self {
        match e {
            ClientError::Server(b) => Failure::Server(b),
            ClientError::Timeout => Failure::Client("OperationTimedOut: errors={}, last_host=None".into()),
            e => Failure::Client(format!("NoHostAvailable: {e}")),
        }
    }
}

impl Failure {
    /// What cqlsh prints: `<class name>: <message>`.
    pub fn line(&self) -> String {
        match self {
            Failure::Server(e) => error_line(e),
            Failure::Client(s) => s.clone(),
        }
    }

    /// The message alone (DESCRIBE prints what follows `message=`).
    pub fn describe_line(&self) -> String {
        let full = match self {
            Failure::Server(e) => error_message(e).1,
            Failure::Client(s) => s.clone(),
        };
        match full.split_once("message=") {
            Some((_, m)) => m.trim_matches('"').to_string(),
            None => String::new(),
        }
    }
}

fn summary(code: i32) -> &'static str {
    match code {
        code::SERVER_ERROR => "Server error",
        code::PROTOCOL_ERROR => "Protocol error",
        code::BAD_CREDENTIALS => "Bad credentials",
        code::UNAVAILABLE => "Unavailable exception",
        code::OVERLOADED => "Coordinator node overloaded",
        code::IS_BOOTSTRAPPING => "Coordinator node is bootstrapping",
        code::TRUNCATE_ERROR => "Error during truncate",
        code::WRITE_TIMEOUT | code::READ_TIMEOUT => "Coordinator node timed out waiting for replica nodes' responses",
        code::READ_FAILURE => "Replica(s) failed to execute read",
        code::FUNCTION_FAILURE => "User Defined Function failure",
        code::WRITE_FAILURE => "Replica(s) failed to execute write",
        code::CDC_WRITE_FAILURE => "Failed to execute write due to CDC space exhaustion.",
        code::SYNTAX_ERROR => "Syntax error in CQL query",
        code::UNAUTHORIZED => "Unauthorized",
        code::INVALID => "Invalid query",
        code::CONFIG_ERROR => "Query invalid because of configuration issue",
        code::ALREADY_EXISTS => "Item already exists",
        code::UNPREPARED => "Matching prepared statement not found on this node",
        _ => "Unknown",
    }
}

/// `ErrorMessage.summary_msg`.
pub fn summary_msg(e: &ErrorBody) -> String {
    format!("Error from server: code={:04x} [{}] message=\"{}\"", e.code, summary(e.code), e.message)
}

/// Python's `repr` of a str.
pub fn py_repr(s: &str) -> String {
    let q = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::new();
    out.push(q);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == q => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push(q);
    out
}

fn reasons(r: &[(std::net::IpAddr, u16)]) -> String {
    let parts: Vec<String> = r.iter().map(|(a, c)| format!("'{a}': '0x{c:04x}'")).collect();
    format!("{{{}}}", parts.join(", "))
}

/// The exception class cqlsh names and its `str()`.
pub fn error_message(e: &ErrorBody) -> (&'static str, String) {
    let raw = |name| (name, e.message.clone());
    let cl = |c: &Consistency| c.name();
    match (e.code, &e.detail) {
        (code::UNAVAILABLE, ErrorDetail::Unavailable { consistency, required, alive }) => (
            "Unavailable",
            format!(
                "{} info={{'consistency': '{}', 'required_replicas': {required}, 'alive_replicas': {alive}}}",
                summary_msg(e),
                cl(consistency)
            ),
        ),
        (code::READ_TIMEOUT, ErrorDetail::ReadTimeout { consistency, received, block_for, .. }) => (
            "ReadTimeout",
            format!(
                "{} info={{'consistency': '{}', 'required_responses': {block_for}, 'received_responses': {received}}}",
                summary_msg(e),
                cl(consistency)
            ),
        ),
        (code::WRITE_TIMEOUT, ErrorDetail::WriteTimeout { consistency, received, block_for, write_type, .. }) => (
            "WriteTimeout",
            format!(
                "{} info={{'consistency': '{}', 'required_responses': {block_for}, 'received_responses': {received}, 'write_type': '{write_type}'}}",
                summary_msg(e),
                cl(consistency)
            ),
        ),
        (code::READ_FAILURE | code::WRITE_FAILURE, d) => {
            let name = if e.code == code::READ_FAILURE { "ReadFailure" } else { "WriteFailure" };
            let (c, received, block_for, failures, map) = match d {
                ErrorDetail::ReadFailure { consistency, received, block_for, num_failures, reasons: r, .. }
                | ErrorDetail::WriteFailure { consistency, received, block_for, num_failures, reasons: r, .. } => {
                    let n = if r.is_empty() { *num_failures } else { r.len() as i32 };
                    (cl(consistency), *received, *block_for, n, (!r.is_empty()).then(|| reasons(r)))
                }
                _ => ("Not Set", 0, 0, 0, None),
            };
            let mut info = format!(
                "{{'consistency': '{c}', 'required_responses': {block_for}, 'received_responses': {received}, 'failures': {failures}"
            );
            if let Some(m) = map {
                info.push_str(&format!(", 'error_code_map': {m}"));
            }
            info.push('}');
            (name, format!("{} info={info}", summary_msg(e)))
        }
        (code::FUNCTION_FAILURE, _) => ("FunctionFailure", summary_msg(e)),
        (code::UNAUTHORIZED, _) => ("Unauthorized", summary_msg(e)),
        (code::INVALID, _) => ("InvalidRequest", summary_msg(e)),
        (code::ALREADY_EXISTS, ErrorDetail::AlreadyExists { keyspace, table }) => (
            "AlreadyExists",
            if table.is_empty() {
                format!("Keyspace '{keyspace}' already exists")
            } else {
                format!("Table '{keyspace}.{table}' already exists")
            },
        ),
        (code::SERVER_ERROR, _) => raw("ServerError"),
        (code::PROTOCOL_ERROR, _) => raw("ProtocolException"),
        (code::BAD_CREDENTIALS, _) => raw("BadCredentials"),
        (code::OVERLOADED, _) => raw("OverloadedErrorMessage"),
        (code::IS_BOOTSTRAPPING, _) => raw("IsBootstrappingErrorMessage"),
        (code::TRUNCATE_ERROR, _) => raw("TruncateError"),
        (code::CDC_WRITE_FAILURE, _) => raw("CDCWriteException"),
        (code::SYNTAX_ERROR, _) => raw("SyntaxException"),
        (code::CONFIG_ERROR, _) => raw("ConfigurationException"),
        (code::UNPREPARED, _) => raw("PreparedQueryNotFound"),
        _ => raw("ErrorMessage"),
    }
}

/// `<class>: <message>`, as `perform_simple_statement` prints it.
pub fn error_line(e: &ErrorBody) -> String {
    let (name, msg) = error_message(e);
    format!("{name}: {msg}")
}

/// One response to a QUERY.
pub struct Reply {
    pub body: ResultBody,
    pub warnings: Vec<String>,
    pub tracing_id: Option<[u8; 16]>,
}

/// The connection with the settings statements run with.
#[derive(Clone)]
pub struct Conn {
    pub client: Client,
    pub target: Arc<Target>,
}

impl Conn {
    pub async fn query(&self, cql: &str, params: QueryParams, trace: bool) -> Result<Reply, Failure> {
        let mut f = RequestFrame::new(0, Request::Query(Query { query: cql.into(), params }));
        f.tracing = trace;
        let r = self.client.send(f).await?;
        match r.response {
            Response::Result(body) => Ok(Reply { body, warnings: r.warnings, tracing_id: r.tracing_id }),
            Response::Error(e) => Err(Failure::Server(e)),
            other => Err(Failure::Client(format!("ProtocolError: unexpected {:?}", other.opcode()))),
        }
    }

    /// Runs a query and fetches every page.
    pub async fn all_rows(&self, cql: &str, cl: Consistency) -> Result<Option<Rows>, Failure> {
        let mut params = QueryParams::new(cl);
        params.page_size = Some(5000);
        let mut out: Option<Rows> = None;
        loop {
            let r = self.query(cql, params.clone(), false).await?;
            let ResultBody::Rows(rows) = r.body else { return Ok(out) };
            let next = rows.metadata.paging_state.clone();
            out = Some(match out {
                None => rows,
                Some(mut acc) => {
                    let mut content = acc.content.to_vec();
                    content.extend_from_slice(&rows.content);
                    acc.content = Bytes::from(content);
                    acc.row_count += rows.row_count;
                    acc
                }
            });
            match next {
                Some(p) => params.paging_state = Some(p),
                None => return Ok(out),
            }
        }
    }

    /// Text cells of every row of a simple query (nulls as `None`).
    pub async fn strings(&self, cql: &str) -> Result<Vec<Vec<Option<String>>>, Failure> {
        let Some(rows) = self.all_rows(cql, Consistency::One).await? else { return Ok(vec![]) };
        Ok(rows.rows().map(|r| r.map(|c| c.map(|b| String::from_utf8_lossy(&b).into_owned())).collect()).collect())
    }
}

/// The value of a column in a row, by name.
pub fn cell<'a>(rows: &'a Rows, row: &'a [Option<Bytes>], name: &str) -> Option<&'a Bytes> {
    let i = rows.metadata.columns.iter().position(|c| c.name == name)?;
    row.get(i)?.as_ref()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_read_like_the_python_driver() {
        let e = ErrorBody::new(code::INVALID, "unconfigured table nope");
        assert_eq!(
            error_line(&e),
            "InvalidRequest: Error from server: code=2200 [Invalid query] message=\"unconfigured table nope\""
        );
        let e = ErrorBody::new(code::SYNTAX_ERROR, "line 1:0 no viable alternative at input 'SELEC'");
        assert_eq!(error_line(&e), "SyntaxException: line 1:0 no viable alternative at input 'SELEC'");
        let e = ErrorBody::new(code::CONFIG_ERROR, "SimpleStrategy doesn't support tablet replication");
        assert_eq!(error_line(&e), "ConfigurationException: SimpleStrategy doesn't support tablet replication");
        let e = ErrorBody::with(
            code::ALREADY_EXISTS,
            "x",
            ErrorDetail::AlreadyExists { keyspace: "ks".into(), table: "t".into() },
        );
        assert_eq!(error_line(&e), "AlreadyExists: Table 'ks.t' already exists");
        let e = ErrorBody::with(
            code::UNAVAILABLE,
            "Cannot achieve consistency level QUORUM",
            ErrorDetail::Unavailable { consistency: Consistency::Quorum, required: 2, alive: 1 },
        );
        assert_eq!(
            error_line(&e),
            "Unavailable: Error from server: code=1000 [Unavailable exception] message=\"Cannot achieve consistency level QUORUM\" info={'consistency': 'QUORUM', 'required_replicas': 2, 'alive_replicas': 1}"
        );
        let e = ErrorBody::with(
            code::WRITE_TIMEOUT,
            "t",
            ErrorDetail::WriteTimeout {
                consistency: Consistency::One,
                received: 0,
                block_for: 1,
                write_type: "SIMPLE".into(),
                contentions: None,
            },
        );
        assert!(error_line(&e).ends_with("info={'consistency': 'ONE', 'required_responses': 1, 'received_responses': 0, 'write_type': 'SIMPLE'}"));
        let f = Failure::Server(ErrorBody::new(code::INVALID, "Keyspace 'x' not found"));
        assert_eq!(f.describe_line(), "Keyspace 'x' not found");
        assert_eq!(Failure::Server(ErrorBody::new(code::SYNTAX_ERROR, "bad")).describe_line(), "");
        assert_eq!(py_repr("it's"), "\"it's\"");
        assert_eq!(py_repr("a"), "'a'");
    }
}
