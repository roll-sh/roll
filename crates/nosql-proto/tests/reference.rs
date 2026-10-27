//! Against the reference images. Set `NOSQL_PROTO_REF_CASSANDRA` and/or
//! `NOSQL_PROTO_REF_SCYLLA` to `host:port` (e.g. `127.0.0.1:29042` / `127.0.0.1:19042`);
//! without them every test here passes without doing anything.
//!
//! - Round trips: the client on every version and compression each reference supports —
//!   SUPPORTED, column types (including the per-version downgrades), prepared statements,
//!   paging, batches, multi-segment v5 messages, tracing, warnings, errors with their
//!   bodies, schema change results and events.
//! - Negotiation: raw bytes sent to the reference, and the reply compared byte for byte with
//!   what `ServerDecoder` + `Rejection` produce for the same bytes, including whether the
//!   connection is closed.
//! - Malformed bodies: the reference's ERROR compared with `DecodeError::to_error`.
//!
//! Keyspaces are named `proto_*` and dropped at the end.

#![allow(clippy::field_reassign_with_default)] // QueryParams are built field by field

use std::time::Duration;

use bytes::{BufMut, Bytes, BytesMut};
use nosql_proto::client::{Client, ClientConfig, ClientError};
use nosql_proto::supported::{self, ScyllaNode};
use nosql_proto::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_util::codec::Decoder;

fn reference(d: Dialect) -> Option<String> {
    let var = match d {
        Dialect::Cassandra => "NOSQL_PROTO_REF_CASSANDRA",
        Dialect::Scylla => "NOSQL_PROTO_REF_SCYLLA",
    };
    let v = std::env::var(var).ok().filter(|v| !v.is_empty());
    if v.is_none() {
        eprintln!("{var} not set; skipping");
    }
    v
}

fn int(n: i32) -> Value {
    Value::new(Bytes::copy_from_slice(&n.to_be_bytes()))
}

fn text(s: &str) -> Value {
    Value::new(Bytes::copy_from_slice(s.as_bytes()))
}

fn ok<T>(r: Result<T, ClientError>, what: &str) -> T {
    r.unwrap_or_else(|e| panic!("{what}: {e}"))
}

fn server_error<T: std::fmt::Debug>(r: Result<T, ClientError>) -> ErrorBody {
    match r {
        Err(ClientError::Server(e)) => e,
        other => panic!("expected a server error, got {other:?}"),
    }
}

/// What a column of each type looks like on `v`: our own downgrade, which the references
/// must agree with.
fn on(v: Version, dialect: Dialect, t: ColType) -> ColType {
    let mut b = BytesMut::new();
    t.encode(Ctx::new(v).dialect(dialect), &mut b);
    ColType::decode(&mut Reader::new(b.freeze())).unwrap()
}

const KS: &str = "proto_rt";

fn table_types() -> Vec<(&'static str, ColType)> {
    vec![
        ("k", ColType::Int),
        ("c", ColType::Int),
        ("b", ColType::Blob),
        ("d", ColType::Date),
        ("dur", ColType::Duration),
        ("l", ColType::list(ColType::Int)),
        ("m", ColType::map(ColType::Varchar, ColType::Int)),
        ("s", ColType::set(ColType::Varchar)),
        ("si", ColType::Smallint),
        ("t", ColType::Varchar),
        ("ti", ColType::Tinyint),
        ("tm", ColType::Time),
        ("tu", ColType::Tuple(vec![ColType::Int, ColType::Varchar])),
        (
            "u",
            ColType::Udt { keyspace: KS.into(), name: "addr".into(), fields: vec![("street".into(), ColType::Varchar), ("n".into(), ColType::Int)] },
        ),
    ]
}

async fn setup(addr: &str) -> Client {
    let c = ok(Client::connect(ClientConfig::new(addr)).await, "connect");
    let q = |s: &'static str| {
        let c = c.clone();
        async move { ok(c.query(s, QueryParams::default()).await, s) }
    };
    q("DROP KEYSPACE IF EXISTS proto_rt").await;
    let r = q("CREATE KEYSPACE proto_rt WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}").await;
    assert_eq!(
        r,
        ResultBody::SchemaChange(SchemaChange::keyspace(SchemaChangeType::Created, KS)),
        "keyspace schema change result"
    );
    let r = q("CREATE TYPE proto_rt.addr (street text, n int)").await;
    assert_eq!(r, ResultBody::SchemaChange(SchemaChange::object(SchemaChangeType::Created, SchemaTarget::Type, KS, "addr")));
    let r = q("CREATE TABLE proto_rt.t (k int, c int, b blob, d date, dur duration, l list<int>, m map<text, int>, \
               s set<text>, si smallint, t text, ti tinyint, tm time, tu tuple<int, text>, u frozen<addr>, \
               PRIMARY KEY (k, c))")
    .await;
    assert_eq!(r, ResultBody::SchemaChange(SchemaChange::object(SchemaChangeType::Created, SchemaTarget::Table, KS, "t")));
    c
}

/// Everything per (version, compression) combination; `k` keeps their rows apart.
async fn round_trip(addr: &str, dialect: Dialect, v: Version, comp: Option<Compression>, k: i32) {
    let label = format!("{dialect:?} {v} {comp:?}");
    let c = ok(Client::connect(ClientConfig::new(addr).version(v).compression(comp)).await, &label);
    assert_eq!(c.version(), v);
    // SUPPORTED, exactly.
    match dialect {
        Dialect::Cassandra => assert_eq!(c.supported(), &supported::cassandra(), "{label}"),
        Dialect::Scylla => {
            let host = c.supported().get("SCYLLA_HOST_ID").unwrap()[0].clone();
            assert_eq!(c.supported(), &supported::scylla(&ScyllaNode::new(host)), "{label}");
        }
    }
    // system.local
    let ResultBody::Rows(rows) = ok(c.query("SELECT key, release_version FROM system.local", QueryParams::default()).await, &label) else {
        panic!("{label}: not rows")
    };
    assert_eq!(rows.len(), 1);
    assert_eq!(rows.to_vec()[0][0].as_deref(), Some(&b"local"[..]));
    assert!(rows.metadata.global_tables_spec);
    assert_eq!(rows.metadata.columns[0], ColSpec::new("system", "local", "key", ColType::Varchar));

    // Prepared insert of every column type.
    let insert = ok(c.prepare("INSERT INTO proto_rt.t (k, c, b, d, l, m, s, si, t, ti, tm, tu, u, dur) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)").await, &label);
    assert_eq!(insert.result_metadata_id.is_some(), v >= Version::V5, "{label}");
    assert_eq!(insert.metadata.columns.len(), 14);
    if v >= Version::V4 {
        assert_eq!(insert.metadata.pk_indexes, vec![0], "{label}");
    }
    assert!(insert.result_metadata.no_metadata);
    let bind_types: Vec<ColType> = insert.metadata.columns.iter().map(|c| c.ty.clone()).collect();
    let expected = |name: &str| on(v, dialect, table_types().into_iter().find(|(n, _)| *n == name).unwrap().1);
    let mut diffs = vec![];
    for (spec, name) in insert.metadata.columns.iter().zip(["k", "c", "b", "d", "l", "m", "s", "si", "t", "ti", "tm", "tu", "u", "dur"]) {
        if spec.ty != expected(name) {
            diffs.push(format!("{label}: bind type of {name}: reference {:?}, ours {:?}", spec.ty, expected(name)));
        }
    }
    assert!(diffs.is_empty(), "{}", diffs.join("\n"));
    let _ = bind_types;
    let list = {
        let mut b = BytesMut::new();
        b.put_i32(2);
        for n in [7, 8] {
            b.put_i32(4);
            b.put_i32(n);
        }
        Value::new(b.freeze())
    };
    let map = {
        let mut b = BytesMut::new();
        b.put_i32(1);
        b.put_i32(1);
        b.put_slice(b"a");
        b.put_i32(4);
        b.put_i32(1);
        Value::new(b.freeze())
    };
    let set = {
        let mut b = BytesMut::new();
        b.put_i32(1);
        b.put_i32(1);
        b.put_slice(b"x");
        Value::new(b.freeze())
    };
    let tuple = {
        let mut b = BytesMut::new();
        b.put_i32(4);
        b.put_i32(5);
        b.put_i32(-1);
        Value::new(b.freeze())
    };
    let udt = {
        let mut b = BytesMut::new();
        b.put_i32(4);
        b.put_slice(b"main");
        b.put_i32(4);
        b.put_i32(10);
        Value::new(b.freeze())
    };
    // duration 1mo2d3ns: three zig-zag vints.
    let dur = Value::new(Bytes::from_static(&[2, 4, 6]));
    let big = Bytes::from((0..300_000u32).map(|i| (i % 251) as u8).collect::<Vec<_>>());
    for cc in 0..5 {
        let blob = if cc == 4 { big.clone() } else { Bytes::from(vec![cc as u8; 3]) };
        let mut p = QueryParams::new(Consistency::One);
        p.values = vec![
            int(k),
            int(cc),
            Value::new(blob),
            Value::new(Bytes::from_static(&[0x80, 0, 0, 10])),
            list.clone(),
            map.clone(),
            set.clone(),
            Value::new(Bytes::from_static(&[0, 3])),
            text("hé"),
            Value::new(Bytes::from_static(&[9])),
            Value::new(Bytes::copy_from_slice(&1_000_000i64.to_be_bytes())),
            tuple.clone(),
            udt.clone(),
            dur.clone(),
        ];
        if cc == 3 && v >= Version::V4 {
            p.values[13] = Value::Unset;
        }
        assert_eq!(ok(c.execute(&insert, p).await, &label), ResultBody::Void);
    }

    // Result metadata of every type, then paging two rows at a time.
    let select = "SELECT k, c, b, d, dur, l, m, s, si, t, ti, tm, tu, u FROM proto_rt.t WHERE k = ?";
    let mut p = QueryParams::new(Consistency::One);
    p.values = vec![int(k)];
    p.page_size = Some(2);
    let mut pager = c.pages(select, p.clone());
    let mut pages = vec![];
    while let Some(page) = ok(pager.next_page().await, &label) {
        pages.push(page);
    }
    assert_eq!(pages.iter().map(Rows::len).collect::<Vec<_>>(), vec![2, 2, 1], "{label}");
    assert!(pages[0].metadata.paging_state.is_some() && pages[2].metadata.paging_state.is_none());
    for (spec, (name, ty)) in pages[0].metadata.columns.iter().zip(table_types()) {
        assert_eq!((spec.name.as_str(), &spec.ty), (name, &on(v, dialect, ty)), "{label}: result type of {name}");
    }
    let rows: Vec<_> = pages.iter().flat_map(Rows::to_vec).collect();
    assert_eq!(rows[4][2].as_ref(), Some(&big), "{label}: 300 KB blob");
    assert_eq!(rows[0][9].as_deref(), Some("hé".as_bytes()));
    if v >= Version::V4 {
        assert_eq!(rows[3][4], None, "{label}: unset left the column null");
    }
    assert_eq!(rows[0][4].as_deref(), Some(&[2u8, 4, 6][..]));
    // The same through a prepared statement with skip_metadata.
    let prepared = ok(c.prepare(select).await, &label);
    assert_eq!(prepared.result_metadata.columns.len(), 14);
    let mut sp = p.clone();
    sp.skip_metadata = true;
    sp.page_size = None;
    let ResultBody::Rows(r) = ok(c.execute(&prepared, sp).await, &label) else { panic!() };
    assert!(r.metadata.no_metadata && r.metadata.columns.is_empty());
    assert_eq!((r.metadata.column_count, r.len()), (14, 5));
    let all = c.pages_prepared(&prepared, p.clone()).collect().await;
    assert_eq!(ok(all, &label).len(), 5);

    // A batch of text and prepared statements.
    let mut bp = QueryParams::new(Consistency::One);
    bp.timestamp = Some(1_700_000_000_000_000);
    let small = ok(c.prepare("INSERT INTO proto_rt.t (k, c, t) VALUES (?, ?, ?)").await, &label);
    let batch = Batch {
        kind: BatchType::Unlogged,
        statements: vec![
            BatchStatement { query: BatchQuery::Text(format!("INSERT INTO proto_rt.t (k, c, t) VALUES ({k}, 10, 'a')")), values: vec![] },
            BatchStatement { query: BatchQuery::Prepared(small.id.clone()), values: vec![int(k), int(11), text("b")] },
            BatchStatement { query: BatchQuery::Text("INSERT INTO proto_rt.t (k, c, t) VALUES (?, 12, ?)".into()), values: vec![int(k), text("c")] },
        ],
        params: bp,
    };
    assert_eq!(ok(c.batch(batch).await, &label), ResultBody::Void);
    let ResultBody::Rows(r) = ok(c.query(&format!("SELECT c, writetime(t) FROM proto_rt.t WHERE k = {k} AND c >= 10"), QueryParams::default()).await, &label) else { panic!() };
    let rows = r.to_vec();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1][1].as_deref(), Some(&1_700_000_000_000_000i64.to_be_bytes()[..]));

    // Tracing.
    let traced = ok(
        c.send(RequestFrame {
            stream: 0,
            tracing: true,
            custom_payload: None,
            request: Request::Query(Query { query: "SELECT key FROM system.local".into(), params: QueryParams::default() }),
        })
        .await,
        &label,
    );
    assert!(traced.tracing_id.is_some(), "{label}: tracing id");

    // A custom payload (v4+) is accepted and ignored.
    let resp = ok(
        c.send(RequestFrame {
            stream: 0,
            tracing: false,
            custom_payload: Some(vec![("k".into(), Some(Bytes::from_static(b"v")))]),
            request: Request::Query(Query { query: "SELECT key FROM system.local".into(), params: QueryParams::default() }),
        })
        .await,
        &label,
    );
    assert!(matches!(resp.response, Response::Result(ResultBody::Rows(_))), "{label}: {resp:?}");

    // Errors and their bodies.
    let e = server_error(c.query(&format!("SELECT * FROM proto_rt.t WHERE k = {k}"), QueryParams::new(Consistency::Three)).await);
    assert_eq!(e.code, code::UNAVAILABLE, "{label}: {e}");
    assert!(
        matches!(e.detail, ErrorDetail::Unavailable { consistency: Consistency::Three, required: 3, alive: 1 }),
        "{label}: {e:?}"
    );
    let e = server_error(c.query("SELEC 1", QueryParams::default()).await);
    assert_eq!(e.code, code::SYNTAX_ERROR, "{label}");
    let e = server_error(c.query("SELECT * FROM proto_rt.missing", QueryParams::default()).await);
    assert_eq!(e.code, code::INVALID, "{label}: {e}");
    let e = server_error(c.query("CREATE KEYSPACE proto_rt WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}", QueryParams::default()).await);
    assert_eq!(e.detail, ErrorDetail::AlreadyExists { keyspace: KS.into(), table: String::new() }, "{label}");
    let e = server_error(c.query("CREATE TABLE proto_rt.t (k int PRIMARY KEY)", QueryParams::default()).await);
    assert_eq!(e.detail, ErrorDetail::AlreadyExists { keyspace: KS.into(), table: "t".into() }, "{label}");
    let ghost = Prepared { id: Bytes::from_static(&[0xAB; 16]), ..prepared.clone() };
    let e = server_error(c.execute(&ghost, QueryParams::default()).await);
    assert_eq!(e.detail, ErrorDetail::Unprepared { id: ghost.id.clone() }, "{label}");
    // Set_keyspace.
    assert_eq!(ok(c.query("USE proto_rt", QueryParams::default()).await, &label), ResultBody::SetKeyspace(KS.into()));

    // v5: keyspace per request, now_in_seconds.
    if v >= Version::V5 {
        let mut kp = QueryParams::new(Consistency::One);
        kp.keyspace = Some("system".into());
        kp.now_in_seconds = Some(1_800_000_000);
        let ResultBody::Rows(r) = ok(c.query("SELECT key FROM local", kp).await, &label) else { panic!() };
        assert_eq!(r.len(), 1);
        let p = ok(c.prepare_in("SELECT key FROM local", Some("system")).await, &label);
        assert_eq!(p.result_metadata.columns[0].keyspace, "system");
    }
    // Cassandra warns on batches over 5 KiB (v4+ only).
    if dialect == Dialect::Cassandra {
        let big_text = "w".repeat(3000);
        let resp = ok(
            c.send(RequestFrame::new(
                0,
                Request::Batch(Batch {
                    kind: BatchType::Logged,
                    // Cassandra only warns for batches spanning several partitions.
                    statements: [k, k + 1000]
                        .map(|pk| BatchStatement {
                            query: BatchQuery::Text("INSERT INTO proto_rt.t (k, c, t) VALUES (?, 20, ?)".into()),
                            values: vec![int(pk), text(&big_text)],
                        })
                        .to_vec(),
                    params: QueryParams::new(Consistency::One),
                }),
            ))
            .await,
            &label,
        );
        assert_eq!(resp.response, Response::Result(ResultBody::Void));
        assert_eq!(!resp.warnings.is_empty(), v >= Version::V4, "{label}: {:?}", resp.warnings);
    }
}

async fn events(addr: &str) {
    let listener = ok(Client::connect(ClientConfig::new(addr)).await, "listener");
    let mut rx = ok(listener.register(&[EventType::SchemaChange, EventType::StatusChange, EventType::TopologyChange]).await, "register");
    let c = ok(Client::connect(ClientConfig::new(addr)).await, "connect");
    ok(c.query("CREATE TABLE proto_rt.ev (k int PRIMARY KEY)", QueryParams::default()).await, "create");
    let e = tokio::time::timeout(Duration::from_secs(20), rx.recv()).await.expect("event in time").expect("event");
    assert_eq!(e, Event::SchemaChange(SchemaChange::object(SchemaChangeType::Created, SchemaTarget::Table, KS, "ev")));
    // An unknown event type.
    let e = server_error(listener.request(Request::Register(vec!["FOO".into()])).await);
    let dialect = if listener.supported().has("SCYLLA_SHARD") { Dialect::Scylla } else { Dialect::Cassandra };
    assert_eq!(e, EventType::parse("FOO", dialect).unwrap_err().to_error(dialect));
}

/// Result metadata ids: METADATA_CHANGED after an ALTER (v5, or v4 with ScyllaDB's
/// extension), and ScyllaDB's LWT mark.
async fn metadata_ids(addr: &str, dialect: Dialect) {
    let mut cfg = ClientConfig::new(addr);
    cfg.scylla_extensions = true;
    let c = ok(Client::connect(cfg).await, "connect");
    assert_eq!(c.ctx().features.metadata_id, dialect == Dialect::Scylla);
    assert!(c.ctx().metadata_id());
    let q = |s: &str| {
        let c = c.clone();
        let s = s.to_string();
        async move { ok(c.query(&s, QueryParams::default()).await, &s) }
    };
    q("CREATE TABLE proto_rt.mc (k int PRIMARY KEY, a int)").await;
    q("INSERT INTO proto_rt.mc (k, a) VALUES (1, 2)").await;
    let p = ok(c.prepare("SELECT * FROM proto_rt.mc").await, "prepare");
    let id = p.result_metadata_id.clone().expect("result metadata id");
    let mut skip = QueryParams::default();
    skip.skip_metadata = true;
    let ResultBody::Rows(r) = ok(c.execute(&p, skip.clone()).await, "execute") else { panic!() };
    assert!(r.metadata.no_metadata && r.metadata.new_metadata_id.is_none());
    q("ALTER TABLE proto_rt.mc ADD b int").await;
    // The ALTER evicts the statement; re-preparing gives the same id and a new result
    // metadata id. Executing with the old one makes the server send METADATA_CHANGED.
    let e = server_error(c.execute(&p, skip.clone()).await);
    assert_eq!(e.detail, ErrorDetail::Unprepared { id: p.id.clone() });
    let p2 = ok(c.prepare("SELECT * FROM proto_rt.mc").await, "re-prepare");
    assert_eq!(p2.id, p.id);
    let new_id = p2.result_metadata_id.clone().unwrap();
    assert_ne!(new_id, id);
    let stale = Prepared { result_metadata_id: Some(id), ..p2.clone() };
    let ResultBody::Rows(r) = ok(c.execute(&stale, skip).await, "execute with the old metadata id") else { panic!() };
    assert!(!r.metadata.no_metadata, "{dialect:?}: {:?}", r.metadata);
    assert_eq!(r.metadata.new_metadata_id, Some(new_id), "METADATA_CHANGED");
    assert_eq!(r.metadata.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["k", "a", "b"]);
    if dialect == Dialect::Scylla {
        let lwt = ok(c.prepare("INSERT INTO proto_rt.mc (k, a) VALUES (?, ?) IF NOT EXISTS").await, "prepare lwt");
        assert!(lwt.metadata.lwt);
        assert!(!p.metadata.lwt);
    }
}

async fn full(dialect: Dialect) {
    let Some(addr) = reference(dialect) else { return };
    let admin = setup(&addr).await;
    let mut k = 0;
    for &v in dialect.versions() {
        let comps: &[Option<Compression>] =
            if v >= Version::V5 { &[None, Some(Compression::Lz4)] } else { &[None, Some(Compression::Lz4), Some(Compression::Snappy)] };
        for &comp in comps {
            k += 1;
            round_trip(&addr, dialect, v, comp, k).await;
        }
    }
    events(&addr).await;
    metadata_ids(&addr, dialect).await;
    // Negotiation from the default (v5) lands on the dialect's best version.
    let c = ok(Client::connect(ClientConfig::new(&addr)).await, "negotiate");
    assert_eq!(c.version(), dialect.current());
    ok(admin.query("DROP KEYSPACE proto_rt", QueryParams::default()).await, "drop");
}

#[tokio::test]
async fn cassandra_round_trips() {
    full(Dialect::Cassandra).await;
}

#[tokio::test]
async fn scylla_round_trips() {
    full(Dialect::Scylla).await;
}

// ---- raw bytes ----

fn frame(version: u8, flags: u8, stream: i16, opcode: u8, body: &[u8]) -> Vec<u8> {
    let mut b = BytesMut::new();
    Header { version, flags: Flags(flags), stream, opcode, length: body.len() as u32 }.write(&mut b);
    b.put_slice(body);
    b.to_vec()
}

fn startup(version: u8) -> Vec<u8> {
    let mut b = BytesMut::new();
    Request::Startup(Startup::new("3.0.0")).encode(Version::V4, &mut b);
    frame(version, 0, 0, 1, &b)
}

/// The reference's reply and whether it closed (EOF or reset) within the wait.
async fn send_raw(addr: &str, pre: Option<&[u8]>, probe: &[u8]) -> (Vec<u8>, bool) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    if let Some(pre) = pre {
        s.write_all(pre).await.unwrap();
        let mut buf = [0u8; 9];
        s.read_exact(&mut buf).await.unwrap();
        let len = u32::from_be_bytes(buf[5..9].try_into().unwrap()) as usize;
        let mut body = vec![0; len];
        s.read_exact(&mut body).await.unwrap();
        assert_eq!(buf[4], 2, "READY");
    }
    s.write_all(probe).await.unwrap();
    let mut out = vec![];
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + Duration::from_millis(1200);
    loop {
        match tokio::time::timeout_at(deadline, s.read(&mut buf)).await {
            Err(_) => return (out, false),
            Ok(Ok(0)) | Ok(Err(_)) => return (out, true),
            Ok(Ok(n)) => out.extend_from_slice(&buf[..n]),
        }
    }
}

/// Our answer to the same bytes: the reply bytes and whether we close.
fn ours(dialect: Dialect, established: Option<Version>, probe: &[u8]) -> (Vec<u8>, bool) {
    let mut d = ServerDecoder::new(dialect);
    if let Some(v) = established {
        d.established(v);
    }
    let mut buf = BytesMut::from(probe);
    match d.decode(&mut buf).unwrap() {
        Some(Inbound::Reject(r)) => (r.to_bytes().map(|b| b.to_vec()).unwrap_or_default(), r.close),
        other => panic!("our decoder accepted {probe:02x?}: {other:?}"),
    }
}

fn body_probes() -> Vec<(&'static str, Vec<u8>)> {
    let q = |body: &[u8]| frame(4, 0, 9, 7, body);
    let lstr = |s: &[u8]| {
        let mut b = (s.len() as u32).to_be_bytes().to_vec();
        b.extend_from_slice(s);
        b
    };
    vec![
        ("query truncated", q(&[&100u32.to_be_bytes()[..], b"select"].concat())),
        ("query without parameters", q(&lstr(b"select"))),
        ("bad consistency", q(&[lstr(b"select * from system.local where k=1"), vec![0x00, 0x63, 0x00]].concat())),
        ("value length -3", q(&[lstr(b"select ?"), vec![0, 1, 1, 0, 1], (-3i32).to_be_bytes().to_vec()].concat())),
        ("value count beyond the body", q(&[lstr(b"select ?"), vec![0, 1, 1, 0xff, 0xff]].concat())),
        ("bad utf-8", q(&[lstr(b"\xff\xfe"), vec![0, 1, 0]].concat())),
        ("register unknown event", frame(4, 0, 9, 11, b"\x00\x01\x00\x03FOO")),
        ("batch query kind", frame(4, 0, 9, 13, b"\x00\x00\x01\x05")),
        ("string truncated in register", frame(4, 0, 9, 11, b"\x00\x01\x00\x09SCHEMA")),
    ]
}

/// A name, bytes to send first (STARTUP) and the probe.
type Probe = (&'static str, Option<Vec<u8>>, Vec<u8>);

async fn raw(dialect: Dialect) {
    let Some(addr) = reference(dialect) else { return };
    let startup_body = startup(4);
    let mut probes: Vec<Probe> = vec![
        ("v6 without USE_BETA", None, frame(6, 0, 0x0102, 5, b"")),
        ("v7", None, frame(7, 0, 0x0102, 5, b"")),
        ("v0", None, frame(0, 0, 0x0102, 5, b"")),
        ("v1 header", None, vec![1, 0, 7, 5, 0, 0, 0, 0, 0]),
        ("v2 header", None, vec![2, 0, 7, 5, 0, 0, 0, 0, 0]),
        ("v65", None, frame(65, 0, 0x0102, 5, b"")),
        ("v127", None, frame(127, 0, 0x0102, 5, b"")),
        ("response bit", None, frame(0x84, 0, 0x0102, 5, b"")),
        ("unknown opcode", None, frame(4, 0, 0x0102, 0x20, b"")),
        ("response opcode", None, frame(4, 0, 0x0102, 2, b"")),
        ("credentials", None, frame(4, 0, 0x0102, 4, b"")),
        ("query before startup", None, frame(4, 0, 0x0102, 7, b"\x00\x00\x00\x08select 1\x00\x01\x00")),
        ("short garbage", None, vec![9, 0, 1, 2, 5]),
        ("post: unknown opcode", Some(startup_body.clone()), frame(4, 0, 9, 0x20, b"")),
        ("post: response opcode", Some(startup_body.clone()), frame(4, 0, 9, 2, b"")),
        ("post: v3 frame", Some(startup_body.clone()), frame(3, 0, 9, 5, b"")),
        ("post: v5 frame", Some(startup_body.clone()), frame(5, 0, 9, 5, b"")),
        ("post: v7 frame", Some(startup_body.clone()), frame(7, 0, 9, 5, b"")),
    ];
    match dialect {
        Dialect::Cassandra => {}
        Dialect::Scylla => {
            probes.push(("v5", None, frame(5, 0, 0x0102, 5, b"")));
            let mut big = BytesMut::new();
            Header { version: 4, flags: Flags(0), stream: 1, opcode: 5, length: 131_072 }.write(&mut big);
            probes.push(("initial too large", None, big.to_vec()));
            let mut huge = BytesMut::new();
            Header { version: 4, flags: Flags(0), stream: 1, opcode: 7, length: 268_435_456 }.write(&mut huge);
            probes.push(("post: request too large", Some(startup_body.clone()), huge.to_vec()));
            probes.push(("post: compression flag", Some(startup_body.clone()), frame(4, 1, 9, 5, b"")));
        }
    }
    let mut failures = vec![];
    for (name, pre, probe) in probes {
        let (theirs, their_close) = send_raw(&addr, pre.as_deref(), &probe).await;
        let (mine, my_close) = ours(dialect, pre.as_ref().map(|_| Version::V4), &probe);
        if theirs != mine || their_close != my_close {
            failures.push(format!("{name}:\n  reference {} close={their_close}\n  ours      {} close={my_close}", hex(&theirs), hex(&mine)));
        }
    }
    // Malformed bodies after STARTUP: the reference's ERROR against ours.
    for (name, probe) in body_probes() {
        let (theirs, _) = send_raw(&addr, Some(&startup_body), &probe).await;
        let mut cc = ClientCodec::new();
        let f = cc.decode(&mut BytesMut::from(&theirs[..])).unwrap().unwrap();
        let Response::Error(their_err) = Response::decode(Version::V4, f.opcode, f.body).unwrap() else {
            failures.push(format!("{name}: reference did not answer with an error"));
            continue;
        };
        let mut d = ServerDecoder::new(dialect);
        d.established(Version::V4);
        let Some(Inbound::Frame(frame)) = d.decode(&mut BytesMut::from(&probe[..])).unwrap() else { panic!("{name}") };
        let ctx = Ctx::new(Version::V4).dialect(dialect);
        let decoded = RequestFrame::decode(ctx, &frame).and_then(|r| match &r.request {
            Request::Register(names) => EventType::parse_all(names, dialect).map(|_| r),
            _ => Ok(r),
        });
        match decoded {
            Err(e) if e.to_error(dialect) == their_err => {}
            other => failures.push(format!("{name}: reference {their_err:?}, ours {other:?}")),
        }
    }
    assert!(failures.is_empty(), "{dialect:?} differences:\n{}", failures.join("\n"));
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[tokio::test]
async fn cassandra_negotiation_bytes() {
    raw(Dialect::Cassandra).await;
}

#[tokio::test]
async fn scylla_negotiation_bytes() {
    raw(Dialect::Scylla).await;
}

/// STARTUP validation texts, against what `Startup::validate` returns.
async fn startup_checks(dialect: Dialect) {
    let Some(addr) = reference(dialect) else { return };
    let cases: Vec<(Version, Vec<(&str, &str)>)> = vec![
        (Version::V4, vec![]),
        (Version::V4, vec![("CQL_VERSION", "2.0.0")]),
        (Version::V4, vec![("CQL_VERSION", "3.0.0"), ("COMPRESSION", "zstd")]),
        (Version::V4, vec![("CQL_VERSION", "3.0.0"), ("COMPRESSION", "ZSTD")]),
        (Version::V4, vec![("CQL_VERSION", "3.0.0"), ("COMPRESSION", "LZ4")]),
        (Version::V5, vec![("CQL_VERSION", "3.0.0"), ("COMPRESSION", "snappy")]),
    ];
    for (v, opts) in cases {
        if !dialect.supports(v) {
            continue;
        }
        let s = Startup { options: opts.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect() };
        let mut b = BytesMut::new();
        Request::Startup(s.clone()).encode(v, &mut b);
        let (theirs, _) = send_raw(&addr, None, &frame(v.as_u8(), 0, 3, 1, &b)).await;
        let mut cc = ClientCodec::new();
        if let Ok(Some(Compression::Lz4)) = s.validate(dialect, v) {
            cc.set_compression(Some(Compression::Lz4));
        }
        let f = cc.decode(&mut BytesMut::from(&theirs[..])).unwrap().unwrap();
        let theirs = Response::decode(v, f.opcode, f.body).unwrap();
        let ours = match s.validate(dialect, v) {
            Ok(_) => Response::Ready,
            Err(e) => Response::Error(e),
        };
        assert_eq!(theirs, ours, "{dialect:?} {v} {opts:?}");
    }
}

#[tokio::test]
async fn cassandra_startup_validation() {
    startup_checks(Dialect::Cassandra).await;
}

#[tokio::test]
async fn scylla_startup_validation() {
    startup_checks(Dialect::Scylla).await;
}
