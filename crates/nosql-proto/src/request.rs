//! Request messages (spec 4.1) and the query parameters QUERY, EXECUTE and BATCH share.
//!
//! Decoding follows Cassandra exactly where it is looser than the spec: QUERY flags are read
//! as a byte before v5 but every bit is honoured (so 0x80 reads a keyspace on v4 too), BATCH
//! parameters go through the same decoder as QUERY's, and values in a BATCH never carry
//! names. Follows `org.apache.cassandra.cql3.QueryOptions` and the request classes in
//! `org.apache.cassandra.transport.messages`.

use bytes::{BufMut, Bytes, BytesMut};

use crate::compress::Compression;
use crate::consistency::Consistency;
use crate::error::DecodeError;
use crate::error_body::ErrorBody;
use crate::frame::{Flags, Frame, Opcode};
use crate::version::{Ctx, Dialect, Direction, Version};
use crate::wire::{PutExt, Reader, Value};

/// A `[bytes map]` custom payload, in wire order.
pub type CustomPayload = Vec<(String, Option<Bytes>)>;

/// Query parameter flags.
pub mod qflag {
    pub const VALUES: u32 = 0x0001;
    pub const SKIP_METADATA: u32 = 0x0002;
    pub const PAGE_SIZE: u32 = 0x0004;
    pub const PAGING_STATE: u32 = 0x0008;
    pub const SERIAL_CONSISTENCY: u32 = 0x0010;
    pub const TIMESTAMP: u32 = 0x0020;
    pub const NAMES_FOR_VALUES: u32 = 0x0040;
    /// v5
    pub const KEYSPACE: u32 = 0x0080;
    /// v5
    pub const NOW_IN_SECONDS: u32 = 0x0100;
}

/// `<query_parameters>` (and BATCH's tail, which uses the same layout).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryParams {
    pub consistency: Consistency,
    pub values: Vec<Value>,
    /// Names for `values` (flag 0x40), same length.
    pub names: Option<Vec<String>>,
    pub skip_metadata: bool,
    pub page_size: Option<i32>,
    pub paging_state: Option<Bytes>,
    pub serial_consistency: Option<Consistency>,
    /// Microseconds.
    pub timestamp: Option<i64>,
    /// v5: overrides the connection's keyspace.
    pub keyspace: Option<String>,
    /// v5: the request's notion of now, in seconds (an unsigned int on the wire).
    pub now_in_seconds: Option<u32>,
    /// The flags as received (decoding only; encoding derives them).
    pub raw_flags: u32,
}

impl Default for QueryParams {
    fn default() -> Self {
        QueryParams {
            consistency: Consistency::One,
            values: vec![],
            names: None,
            skip_metadata: false,
            page_size: None,
            paging_state: None,
            serial_consistency: None,
            timestamp: None,
            keyspace: None,
            now_in_seconds: None,
            raw_flags: 0,
        }
    }
}

impl QueryParams {
    pub fn new(consistency: Consistency) -> Self {
        QueryParams { consistency, ..Default::default() }
    }

    /// The flags this encodes with on `version` (KEYSPACE and NOW_IN_SECONDS exist from v5).
    pub fn flags(&self, version: Version) -> u32 {
        let mut f = 0;
        if !self.values.is_empty() {
            f |= qflag::VALUES;
            if self.names.is_some() {
                f |= qflag::NAMES_FOR_VALUES;
            }
        }
        if self.skip_metadata {
            f |= qflag::SKIP_METADATA;
        }
        if self.page_size.is_some() {
            f |= qflag::PAGE_SIZE;
        }
        if self.paging_state.is_some() {
            f |= qflag::PAGING_STATE;
        }
        if self.serial_consistency.is_some() {
            f |= qflag::SERIAL_CONSISTENCY;
        }
        if self.timestamp.is_some() {
            f |= qflag::TIMESTAMP;
        }
        if version >= Version::V5 {
            if self.keyspace.is_some() {
                f |= qflag::KEYSPACE;
            }
            if self.now_in_seconds.is_some() {
                f |= qflag::NOW_IN_SECONDS;
            }
        }
        f
    }

    pub fn encode(&self, version: Version, out: &mut BytesMut) {
        self.encode_with(version, out, true);
    }

    /// `with_values: false` for BATCH, whose values live with each statement.
    fn encode_with(&self, version: Version, out: &mut BytesMut, with_values: bool) {
        let mut flags = self.flags(version);
        if !with_values {
            flags &= !(qflag::VALUES | qflag::NAMES_FOR_VALUES | qflag::SKIP_METADATA | qflag::PAGE_SIZE | qflag::PAGING_STATE);
        }
        out.put_consistency(self.consistency);
        if version >= Version::V5 {
            out.put_u32(flags);
        } else {
            out.put_u8(flags as u8);
        }
        if flags & qflag::VALUES != 0 {
            out.put_u16(self.values.len() as u16);
            for (i, v) in self.values.iter().enumerate() {
                if let Some(names) = &self.names {
                    out.put_string(names.get(i).map_or("", String::as_str));
                }
                out.put_value(v);
            }
        }
        if let (true, Some(n)) = (flags & qflag::PAGE_SIZE != 0, self.page_size) {
            out.put_i32(n);
        }
        if let (true, Some(p)) = (flags & qflag::PAGING_STATE != 0, &self.paging_state) {
            out.put_cql_bytes(Some(p));
        }
        if let Some(c) = self.serial_consistency {
            out.put_consistency(c);
        }
        if let Some(t) = self.timestamp {
            out.put_i64(t);
        }
        if let (true, Some(k)) = (flags & qflag::KEYSPACE != 0, &self.keyspace) {
            out.put_string(k);
        }
        if let (true, Some(n)) = (flags & qflag::NOW_IN_SECONDS != 0, self.now_in_seconds) {
            out.put_u32(n);
        }
    }

    pub fn decode(version: Version, r: &mut Reader) -> Result<Self, DecodeError> {
        let consistency = r.consistency()?;
        let flags = if version >= Version::V5 { r.u32()? } else { r.u8()? as u32 };
        let mut p = QueryParams { consistency, raw_flags: flags, ..Default::default() };
        if flags & qflag::VALUES != 0 {
            if flags & qflag::NAMES_FOR_VALUES != 0 {
                let n = r.u16()? as usize;
                let mut names = Vec::with_capacity(n.min(r.remaining() / 6));
                let mut values = Vec::with_capacity(n.min(r.remaining() / 6));
                for _ in 0..n {
                    names.push(r.string()?);
                    values.push(r.value(version)?);
                }
                p.names = Some(names);
                p.values = values;
            } else {
                p.values = r.value_list(version)?;
            }
        }
        p.skip_metadata = flags & qflag::SKIP_METADATA != 0;
        if flags & qflag::PAGE_SIZE != 0 {
            p.page_size = Some(r.i32()?);
        }
        if flags & qflag::PAGING_STATE != 0 {
            p.paging_state = r.bytes()?;
        }
        if flags & qflag::SERIAL_CONSISTENCY != 0 {
            p.serial_consistency = Some(r.consistency()?);
        }
        if flags & qflag::TIMESTAMP != 0 {
            let t = r.i64()?;
            if t == i64::MIN {
                return Err(DecodeError::TimestampOutOfRange(t));
            }
            p.timestamp = Some(t);
        }
        if flags & qflag::KEYSPACE != 0 {
            p.keyspace = Some(r.string()?);
        }
        if flags & qflag::NOW_IN_SECONDS != 0 {
            p.now_in_seconds = Some(r.u32()?);
        }
        Ok(p)
    }
}

/// STARTUP options, as sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Startup {
    pub options: Vec<(String, String)>,
}

pub mod startup_key {
    pub const CQL_VERSION: &str = "CQL_VERSION";
    pub const COMPRESSION: &str = "COMPRESSION";
    pub const DRIVER_NAME: &str = "DRIVER_NAME";
    pub const DRIVER_VERSION: &str = "DRIVER_VERSION";
    pub const THROW_ON_OVERLOAD: &str = "THROW_ON_OVERLOAD";
    pub const NO_COMPACT: &str = "NO_COMPACT";
    pub const SCYLLA_USE_METADATA_ID: &str = "SCYLLA_USE_METADATA_ID";
    pub const SCYLLA_RATE_LIMIT_ERROR: &str = "SCYLLA_RATE_LIMIT_ERROR";
    pub const SCYLLA_LWT_ADD_METADATA_MARK: &str = "SCYLLA_LWT_ADD_METADATA_MARK";
    pub const TABLETS_ROUTING_V1: &str = "TABLETS_ROUTING_V1";
    pub const TABLETS_ROUTING_V2: &str = "TABLETS_ROUTING_V2_EXPERIMENTAL";
}

impl Startup {
    pub fn new(cql_version: &str) -> Self {
        Startup { options: vec![(startup_key::CQL_VERSION.into(), cql_version.into())] }
    }

    pub fn with(mut self, k: &str, v: &str) -> Self {
        self.options.push((k.into(), v.into()));
        self
    }

    /// An option by key, case-insensitively (Cassandra upper-cases the keys).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.options.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
    }

    /// The checks the reference makes before answering STARTUP, in its order, with its
    /// messages (all protocol errors). Returns the negotiated body/segment compression.
    pub fn validate(&self, dialect: Dialect, version: Version) -> Result<Option<Compression>, ErrorBody> {
        if dialect == Dialect::Cassandra {
            let Some(v) = self.get(startup_key::CQL_VERSION) else {
                return Err(ErrorBody::protocol("Missing value CQL_VERSION in STARTUP message"));
            };
            match parse_version(v) {
                None => return Err(ErrorBody::protocol(format!("Invalid version value: {v}"))),
                Some(parsed) if parsed < (2, 99, 0) => {
                    return Err(ErrorBody::protocol(format!(
                        "CQL version {v} is not supported by the binary protocol (supported version are >= 3.0.0)"
                    )));
                }
                Some(_) => {}
            }
        }
        let Some(c) = self.get(startup_key::COMPRESSION) else { return Ok(None) };
        let lower = c.to_lowercase();
        match Compression::from_name(&lower) {
            Some(Compression::Snappy) if version >= Version::V5 && dialect == Dialect::Cassandra => {
                Err(ErrorBody::protocol("Snappy compression is not supported in protocol V5"))
            }
            Some(z) => Ok(Some(z)),
            None => Err(ErrorBody::protocol(format!("Unknown compression algorithm: {lower}"))),
        }
    }
}

/// `major.minor[.patch]…` as Cassandra's `CassandraVersion` accepts it, enough to compare.
fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let core = s.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    if let Some(hotfix) = parts.next() {
        hotfix.parse::<u32>().ok()?;
    }
    parts.next().is_none().then_some((major, minor, patch))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub query: String,
    pub params: QueryParams,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Prepare {
    pub query: String,
    /// v5 (flag 0x01).
    pub keyspace: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Execute {
    pub id: Bytes,
    /// v5, or v4 with `SCYLLA_USE_METADATA_ID`.
    pub result_metadata_id: Option<Bytes>,
    pub params: QueryParams,
    /// ScyllaDB `TABLETS_ROUTING_V2_EXPERIMENTAL`: block index (high nibble) and value.
    pub tablet_version_block: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BatchType {
    Logged,
    Unlogged,
    Counter,
    /// Anything else. Cassandra rejects it (`Invalid BATCH message type`); ScyllaDB runs the
    /// batch as logged.
    Other(u8),
}

impl BatchType {
    pub fn from_u8(b: u8) -> Self {
        match b {
            0 => BatchType::Logged,
            1 => BatchType::Unlogged,
            2 => BatchType::Counter,
            n => BatchType::Other(n),
        }
    }

    pub fn as_u8(self) -> u8 {
        match self {
            BatchType::Logged => 0,
            BatchType::Unlogged => 1,
            BatchType::Counter => 2,
            BatchType::Other(n) => n,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BatchQuery {
    Text(String),
    Prepared(Bytes),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchStatement {
    pub query: BatchQuery,
    pub values: Vec<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    pub kind: BatchType,
    pub statements: Vec<BatchStatement>,
    /// Consistency, serial consistency, timestamp, keyspace and now-in-seconds; the rest is
    /// ignored.
    pub params: QueryParams,
}

/// A request body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    Startup(Startup),
    Options,
    Query(Query),
    Prepare(Prepare),
    Execute(Execute),
    Batch(Batch),
    /// Event type names as sent (see [`crate::event::EventType::parse`]).
    Register(Vec<String>),
    AuthResponse(Option<Bytes>),
}

impl Request {
    pub fn opcode(&self) -> Opcode {
        match self {
            Request::Startup(_) => Opcode::Startup,
            Request::Options => Opcode::Options,
            Request::Query(_) => Opcode::Query,
            Request::Prepare(_) => Opcode::Prepare,
            Request::Execute(_) => Opcode::Execute,
            Request::Batch(_) => Opcode::Batch,
            Request::Register(_) => Opcode::Register,
            Request::AuthResponse(_) => Opcode::AuthResponse,
        }
    }

    /// Decodes a body (after any custom payload). Trailing bytes are ignored, as the spec asks.
    pub fn decode(ctx: impl Into<Ctx>, opcode: Opcode, body: Bytes) -> Result<Self, DecodeError> {
        let ctx = ctx.into();
        let v = ctx.version;
        let mut r = Reader::new(body);
        Ok(match opcode {
            Opcode::Startup => Request::Startup(Startup { options: r.string_map()? }),
            Opcode::Options => Request::Options,
            Opcode::Query => {
                let query = r.long_string()?;
                Request::Query(Query { query, params: QueryParams::decode(v, &mut r)? })
            }
            Opcode::Prepare => {
                let query = r.long_string()?;
                let mut keyspace = None;
                if v >= Version::V5 && r.u32()? & 0x01 != 0 {
                    keyspace = Some(r.string()?);
                }
                Request::Prepare(Prepare { query, keyspace })
            }
            Opcode::Execute => {
                let id = r.short_bytes()?;
                let result_metadata_id = if ctx.metadata_id() { Some(r.short_bytes()?) } else { None };
                let params = QueryParams::decode(v, &mut r)?;
                let tablet_version_block = if ctx.features.tablets_v2 && !r.is_empty() { Some(r.u8()?) } else { None };
                Request::Execute(Execute { id, result_metadata_id, params, tablet_version_block })
            }
            Opcode::Batch => {
                let kind = r.u8()?;
                let n = r.u16()? as usize;
                // A statement is at least a kind byte, a 2-byte id length and a value count.
                let mut statements = Vec::with_capacity(n.min(r.remaining() / 5));
                for _ in 0..n {
                    let query = match r.u8()? {
                        0 => BatchQuery::Text(r.long_string()?),
                        1 => BatchQuery::Prepared(r.short_bytes()?),
                        k => return Err(DecodeError::BatchKind(k)),
                    };
                    statements.push(BatchStatement { query, values: r.value_list(v)? });
                }
                let params = QueryParams::decode(v, &mut r)?;
                let kind = BatchType::from_u8(kind);
                if let (Dialect::Cassandra, BatchType::Other(n)) = (ctx.dialect, kind) {
                    return Err(DecodeError::BatchType(n));
                }
                Request::Batch(Batch { kind, statements, params })
            }
            Opcode::Register => Request::Register(r.string_list()?),
            Opcode::AuthResponse => {
                // ScyllaDB also takes a body that is the bare token, without its [bytes] length.
                let rest = r.rest();
                let framed = rest.len() >= 4 && {
                    let n = i32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]);
                    n == -1 || (n >= 0 && n as usize == rest.len() - 4)
                };
                if ctx.dialect == Dialect::Scylla && !framed {
                    Request::AuthResponse(Some(rest))
                } else {
                    Request::AuthResponse(Reader::new(rest).bytes()?)
                }
            }
            Opcode::Credentials => return Err(DecodeError::UnsupportedMessage),
            op => return Err(DecodeError::WrongDirection { opcode: op as u8, got: Direction::Request }),
        })
    }

    pub fn encode(&self, ctx: impl Into<Ctx>, out: &mut BytesMut) {
        let ctx = ctx.into();
        let v = ctx.version;
        match self {
            Request::Startup(s) => out.put_string_map(&s.options),
            Request::Options => {}
            Request::Query(q) => {
                out.put_long_string(&q.query);
                q.params.encode(v, out);
            }
            Request::Prepare(p) => {
                out.put_long_string(&p.query);
                if v >= Version::V5 {
                    match &p.keyspace {
                        Some(k) => {
                            out.put_u32(1);
                            out.put_string(k);
                        }
                        None => out.put_u32(0),
                    }
                }
            }
            Request::Execute(e) => {
                out.put_short_bytes(&e.id);
                if ctx.metadata_id() {
                    out.put_short_bytes(e.result_metadata_id.as_deref().unwrap_or_default());
                }
                e.params.encode(v, out);
                if let (true, Some(b)) = (ctx.features.tablets_v2, e.tablet_version_block) {
                    out.put_u8(b);
                }
            }
            Request::Batch(b) => {
                out.put_u8(b.kind.as_u8());
                out.put_u16(b.statements.len() as u16);
                for s in &b.statements {
                    match &s.query {
                        BatchQuery::Text(q) => {
                            out.put_u8(0);
                            out.put_long_string(q);
                        }
                        BatchQuery::Prepared(id) => {
                            out.put_u8(1);
                            out.put_short_bytes(id);
                        }
                    }
                    out.put_u16(s.values.len() as u16);
                    for val in &s.values {
                        out.put_value(val);
                    }
                }
                b.params.encode_with(v, out, false);
            }
            Request::Register(events) => out.put_string_list(events),
            Request::AuthResponse(t) => out.put_cql_bytes(t.as_deref()),
        }
    }
}

/// A request with its envelope: stream, tracing flag and custom payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestFrame {
    pub stream: i16,
    pub tracing: bool,
    /// v4+.
    pub custom_payload: Option<CustomPayload>,
    pub request: Request,
}

impl RequestFrame {
    pub fn new(stream: i16, request: Request) -> Self {
        RequestFrame { stream, tracing: false, custom_payload: None, request }
    }

    pub fn decode(ctx: impl Into<Ctx>, frame: &Frame) -> Result<Self, DecodeError> {
        let ctx = ctx.into();
        if frame.direction != Direction::Request || frame.opcode.direction() != Direction::Request {
            return Err(DecodeError::WrongDirection { opcode: frame.opcode as u8, got: frame.direction });
        }
        let mut r = Reader::new(frame.body.clone());
        let custom_payload = if frame.flags.has(Flags::CUSTOM_PAYLOAD) { Some(r.bytes_map()?) } else { None };
        if custom_payload.is_some() && ctx.version < Version::V4 {
            return Err(DecodeError::CustomPayloadBeforeV4);
        }
        let request = Request::decode(ctx, frame.opcode, r.rest())?;
        Ok(RequestFrame { stream: frame.stream, tracing: frame.flags.has(Flags::TRACING), custom_payload, request })
    }

    pub fn to_frame(&self, ctx: impl Into<Ctx>) -> Frame {
        let ctx = ctx.into();
        let mut flags = Flags::default();
        let mut body = BytesMut::new();
        if let (true, Some(p)) = (ctx.version >= Version::V4, &self.custom_payload) {
            flags.set(Flags::CUSTOM_PAYLOAD, true);
            body.put_bytes_map(p);
        }
        flags.set(Flags::TRACING, self.tracing);
        flags.set(Flags::USE_BETA, ctx.version.is_beta());
        self.request.encode(ctx, &mut body);
        Frame { version: ctx.version, direction: Direction::Request, flags, stream: self.stream, opcode: self.request.opcode(), body: body.freeze() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Field;
    use crate::version::Features;

    fn round(req: &Request, ctx: Ctx) -> Request {
        let mut b = BytesMut::new();
        req.encode(ctx, &mut b);
        Request::decode(ctx, req.opcode(), b.freeze()).unwrap()
    }

    fn full_params(v: Version) -> QueryParams {
        QueryParams {
            consistency: Consistency::LocalQuorum,
            values: vec![Value::new(&b"\x00\x00\x00\x01"[..]), Value::Null, if v >= Version::V4 { Value::Unset } else { Value::Null }],
            names: None,
            skip_metadata: true,
            page_size: Some(100),
            paging_state: Some(Bytes::from_static(b"state")),
            serial_consistency: Some(Consistency::LocalSerial),
            timestamp: Some(1_700_000_000_000_000),
            keyspace: (v >= Version::V5).then(|| "ks".into()),
            now_in_seconds: (v >= Version::V5).then_some(1_700_000_000),
            raw_flags: 0,
        }
    }

    fn strip(mut r: Request) -> Request {
        match &mut r {
            Request::Query(q) => q.params.raw_flags = 0,
            Request::Execute(e) => e.params.raw_flags = 0,
            Request::Batch(b) => b.params.raw_flags = 0,
            _ => {}
        }
        r
    }

    fn all(v: Version) -> Vec<Request> {
        let p = full_params(v);
        let mut named = p.clone();
        named.names = Some(vec!["a".into(), "b".into(), "c".into()]);
        let mut batch_params = QueryParams::new(Consistency::Quorum);
        batch_params.serial_consistency = Some(Consistency::Serial);
        batch_params.timestamp = Some(5);
        if v >= Version::V5 {
            batch_params.keyspace = Some("ks".into());
            batch_params.now_in_seconds = Some(9);
        }
        vec![
            Request::Startup(Startup::new("3.0.0").with("COMPRESSION", "lz4")),
            Request::Options,
            Request::Query(Query { query: "SELECT * FROM t WHERE k = ?".into(), params: p.clone() }),
            Request::Query(Query { query: "SELECT :a".into(), params: named }),
            Request::Query(Query { query: "SELECT 1".into(), params: QueryParams::default() }),
            Request::Prepare(Prepare { query: "SELECT ?".into(), keyspace: (v >= Version::V5).then(|| "ks".into()) }),
            Request::Execute(Execute {
                id: Bytes::from_static(&[1; 16]),
                result_metadata_id: (v >= Version::V5).then(|| Bytes::from_static(&[2; 16])),
                params: p,
                tablet_version_block: None,
            }),
            Request::Batch(Batch {
                kind: BatchType::Unlogged,
                statements: vec![
                    BatchStatement { query: BatchQuery::Text("INSERT ?".into()), values: vec![Value::new(&b"x"[..])] },
                    BatchStatement { query: BatchQuery::Prepared(Bytes::from_static(&[3; 16])), values: vec![] },
                ],
                params: batch_params,
            }),
            Request::Register(vec!["SCHEMA_CHANGE".into(), "STATUS_CHANGE".into()]),
            Request::AuthResponse(Some(Bytes::from_static(b"\x00cassandra\x00cassandra"))),
            Request::AuthResponse(None),
        ]
    }

    #[test]
    fn every_request_round_trips_on_every_version() {
        for v in [Version::V3, Version::V4, Version::V5, Version::V6] {
            for req in all(v) {
                assert_eq!(strip(round(&req, Ctx::new(v))), req, "{v} {req:?}");
            }
        }
    }

    #[test]
    fn v5_only_fields_are_dropped_before_v5() {
        let mut p = QueryParams::new(Consistency::One);
        p.keyspace = Some("ks".into());
        p.now_in_seconds = Some(1);
        let q = Request::Query(Query { query: "x".into(), params: p });
        let Request::Query(back) = round(&q, Ctx::new(Version::V4)) else { panic!() };
        assert_eq!((back.params.keyspace, back.params.now_in_seconds), (None, None));
        let prep = Request::Prepare(Prepare { query: "x".into(), keyspace: Some("ks".into()) });
        assert_eq!(round(&prep, Ctx::new(Version::V4)), Request::Prepare(Prepare { query: "x".into(), keyspace: None }));
        // v4 flags are one byte, v5 four.
        let mut b = BytesMut::new();
        QueryParams::default().encode(Version::V4, &mut b);
        assert_eq!(&b[..], b"\x00\x01\x00");
        let mut b = BytesMut::new();
        QueryParams::default().encode(Version::V5, &mut b);
        assert_eq!(&b[..], b"\x00\x01\x00\x00\x00\x00");
    }

    #[test]
    fn scylla_takes_a_bare_auth_token() {
        let token = &b"\x00cassandra\x00cassandra"[..];
        let framed = Bytes::from([&(token.len() as i32).to_be_bytes()[..], token].concat());
        for d in [Dialect::Scylla, Dialect::Cassandra] {
            let ctx = Ctx::new(Version::V4).dialect(d);
            let r = Request::decode(ctx, Opcode::AuthResponse, framed.clone()).unwrap();
            assert_eq!(r, Request::AuthResponse(Some(Bytes::from_static(token))), "{d:?}");
            let r = Request::decode(ctx, Opcode::AuthResponse, Bytes::from_static(b"\xff\xff\xff\xff")).unwrap();
            assert_eq!(r, Request::AuthResponse(None), "{d:?}");
        }
        // A body that isn't [bytes]: the whole body (ScyllaDB), a decode error (Cassandra).
        let bare = Bytes::from_static(token);
        let ctx = Ctx::new(Version::V4).dialect(Dialect::Scylla);
        assert_eq!(Request::decode(ctx, Opcode::AuthResponse, bare.clone()).unwrap(), Request::AuthResponse(Some(bare.clone())));
        assert_eq!(Request::decode(ctx, Opcode::AuthResponse, Bytes::new()).unwrap(), Request::AuthResponse(Some(Bytes::new())));
        let ctx = Ctx::new(Version::V4).dialect(Dialect::Cassandra);
        assert!(Request::decode(ctx, Opcode::AuthResponse, bare).is_err());
    }

    #[test]
    fn cassandra_reads_keyspace_bit_on_v4() {
        // consistency ONE, flags 0x80, keyspace "ks".
        let body = Bytes::from_static(b"\x00\x00\x00\x01x\x00\x01\x80\x00\x02ks");
        let Request::Query(q) = Request::decode(Version::V4, Opcode::Query, body).unwrap() else { panic!() };
        assert_eq!(q.params.keyspace.as_deref(), Some("ks"));
    }

    #[test]
    fn scylla_metadata_id_and_tablets_on_v4() {
        let f = Features { metadata_id: true, tablets_v2: true, ..Default::default() };
        let ctx = Ctx::new(Version::V4).dialect(Dialect::Scylla).features(f);
        let e = Request::Execute(Execute {
            id: Bytes::from_static(b"id"),
            result_metadata_id: Some(Bytes::from_static(b"md")),
            params: QueryParams::default(),
            tablet_version_block: Some(0xA3),
        });
        assert_eq!(round(&e, ctx), e);
        // Without the features, a v4 EXECUTE has neither.
        let plain = Ctx::new(Version::V4);
        let Request::Execute(back) = round(&e, plain) else { panic!() };
        assert_eq!((back.result_metadata_id, back.tablet_version_block), (None, None));
    }

    #[test]
    fn batch_type_is_dialect_specific() {
        let body = Bytes::from_static(b"\x07\x00\x00\x00\x01\x00");
        assert_eq!(Request::decode(Version::V4, Opcode::Batch, body.clone()), Err(DecodeError::BatchType(7)));
        let scylla = Ctx::new(Version::V4).dialect(Dialect::Scylla);
        assert!(matches!(Request::decode(scylla, Opcode::Batch, body), Ok(Request::Batch(Batch { kind: BatchType::Other(7), .. }))));
        let bad_kind = Bytes::from_static(b"\x00\x00\x01\x05");
        assert_eq!(Request::decode(Version::V4, Opcode::Batch, bad_kind), Err(DecodeError::BatchKind(5)));
    }

    #[test]
    fn malformed_requests() {
        let dec = |op, b: &'static [u8]| Request::decode(Version::V4, op, Bytes::from_static(b));
        // The probes sent to both references.
        assert!(matches!(dec(Opcode::Query, b"\x00\x00\x00\x64select"), Err(DecodeError::Truncated { field: Field::LongString, need: 100, have: 6, .. })));
        assert!(matches!(dec(Opcode::Query, b"\x00\x00\x00\x06select"), Err(DecodeError::Truncated { field: Field::Short, need: 2, have: 0, at: 10, len: 10 })));
        assert_eq!(dec(Opcode::Query, b"\x00\x00\x00\x01x\x00\x63\x00"), Err(DecodeError::UnknownConsistency(99)));
        assert_eq!(dec(Opcode::Query, b"\x00\x00\x00\x01x\x00\x01\x01\x00\x01\xff\xff\xff\xfd"), Err(DecodeError::InvalidValueLength(-3)));
        assert!(matches!(dec(Opcode::Query, b"\x00\x00\x00\x01x\x00\x01\x01\xff\xff"), Err(DecodeError::Truncated { field: Field::Int, need: 4, have: 0, .. })));
        assert!(matches!(dec(Opcode::Query, b"\x00\x00\x00\x02\xff\xfe\x00\x01\x00"), Err(DecodeError::Utf8 { .. })));
        assert_eq!(
            dec(Opcode::Query, b"\x00\x00\x00\x01x\x00\x01\x20\x80\x00\x00\x00\x00\x00\x00\x00"),
            Err(DecodeError::TimestampOutOfRange(i64::MIN))
        );
        assert_eq!(dec(Opcode::Credentials, b""), Err(DecodeError::UnsupportedMessage));
        assert!(matches!(dec(Opcode::Ready, b""), Err(DecodeError::WrongDirection { opcode: 2, .. })));
        // Every prefix of every valid request fails cleanly or decodes.
        for v in [Version::V3, Version::V4, Version::V5] {
            for req in all(v) {
                let mut b = BytesMut::new();
                req.encode(v, &mut b);
                let full = b.freeze();
                for cut in 0..full.len() {
                    let _ = Request::decode(v, req.opcode(), full.slice(..cut));
                }
            }
        }
    }

    #[test]
    fn startup_validation() {
        let s = |o: &[(&str, &str)]| Startup { options: o.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
        let c = Dialect::Cassandra;
        assert_eq!(s(&[]).validate(c, Version::V4).unwrap_err().message, "Missing value CQL_VERSION in STARTUP message");
        assert_eq!(
            s(&[("CQL_VERSION", "2.0.0")]).validate(c, Version::V4).unwrap_err().message,
            "CQL version 2.0.0 is not supported by the binary protocol (supported version are >= 3.0.0)"
        );
        assert_eq!(s(&[("cql_version", "x")]).validate(c, Version::V4).unwrap_err().message, "Invalid version value: x");
        assert_eq!(
            s(&[("CQL_VERSION", "3.0.0"), ("COMPRESSION", "ZSTD")]).validate(c, Version::V4).unwrap_err().message,
            "Unknown compression algorithm: zstd"
        );
        assert_eq!(
            s(&[("CQL_VERSION", "3.0.0"), ("COMPRESSION", "snappy")]).validate(c, Version::V5).unwrap_err().message,
            "Snappy compression is not supported in protocol V5"
        );
        assert_eq!(s(&[("CQL_VERSION", "3.4.7"), ("COMPRESSION", "LZ4")]).validate(c, Version::V5), Ok(Some(Compression::Lz4)));
        // ScyllaDB does not check CQL_VERSION.
        assert_eq!(s(&[("CQL_VERSION", "2.0.0")]).validate(Dialect::Scylla, Version::V4), Ok(None));
        assert_eq!(s(&[]).validate(Dialect::Scylla, Version::V4), Ok(None));
        assert_eq!(
            s(&[("COMPRESSION", "zstd")]).validate(Dialect::Scylla, Version::V4).unwrap_err().message,
            "Unknown compression algorithm: zstd"
        );
    }

    #[test]
    fn envelope_prefix() {
        let rf = RequestFrame {
            stream: 7,
            tracing: true,
            custom_payload: Some(vec![("k".into(), Some(Bytes::from_static(b"v")))]),
            request: Request::Options,
        };
        let f = rf.to_frame(Version::V4);
        assert!(f.flags.has(Flags::TRACING) && f.flags.has(Flags::CUSTOM_PAYLOAD));
        assert_eq!(RequestFrame::decode(Version::V4, &f).unwrap(), rf);
        // v3 drops the payload when encoding and refuses it when decoding.
        assert_eq!(rf.to_frame(Version::V3).flags, Flags(Flags::TRACING));
        let mut f3 = f.clone();
        f3.version = Version::V3;
        assert_eq!(RequestFrame::decode(Version::V3, &f3), Err(DecodeError::CustomPayloadBeforeV4));
        // Beta flag on v6.
        assert!(RequestFrame::new(0, Request::Options).to_frame(Version::V6).flags.has(Flags::USE_BETA));
        // A request opcode with the response bit.
        let mut wrong = f;
        wrong.direction = Direction::Response;
        assert_eq!(RequestFrame::decode(Version::V4, &wrong), Err(DecodeError::WrongDirection { opcode: 5, got: Direction::Response }));
    }
}
