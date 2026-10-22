//! The CQL connection against the nosql-proto client and raw sockets: handshake, versions,
//! compression, authentication, multiplexing, events, prepared statements, metadata ids and
//! malformed input, in both dialects. Expected texts are the references' (see
//! `docs/nosql/reference/supported.md`).

mod common;

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use common::*;
use nosql_engine::Dialect;
use nosql_proto::client::{Client, ClientError};
use nosql_proto::event::{EventType, SchemaChangeType, SchemaTarget};
use nosql_proto::{
    Batch, BatchQuery, BatchStatement, BatchType, Compression, Consistency, Event, Query, QueryParams, Request, RequestFrame, Response, ResultBody,
    Value, Version, code,
};

const ALL: [Dialect; 2] = [Dialect::Scylla, Dialect::Cassandra];

fn versions(d: Dialect) -> &'static [Version] {
    match d {
        Dialect::Scylla => &[Version::V3, Version::V4],
        Dialect::Cassandra => &[Version::V3, Version::V4, Version::V5],
    }
}

fn q() -> QueryParams {
    QueryParams::default()
}

fn values(v: Vec<Bytes>) -> QueryParams {
    QueryParams { values: v.into_iter().map(Value::Set).collect(), ..Default::default() }
}

async fn setup(c: &Client, ks: &str) {
    c.query(&format!("CREATE KEYSPACE {ks} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}"), q()).await.unwrap();
    c.query(&format!("CREATE TABLE {ks}.t (k int, c int, v text, PRIMARY KEY (k, c))"), q()).await.unwrap();
}

fn rows(r: ResultBody) -> nosql_proto::Rows {
    match r {
        ResultBody::Rows(r) => r,
        other => panic!("not rows: {other:?}"),
    }
}

fn server_error(e: ClientError) -> nosql_proto::ErrorBody {
    match e {
        ClientError::Server(b) => b,
        other => panic!("not a server error: {other:?}"),
    }
}

#[tokio::test]
async fn handshake_matrix() {
    init_tracing();
    for d in ALL {
        let s = single(d).await;
        let setup_client = connect(s.addr, Version::V4, None).await;
        setup(&setup_client, "srv_m").await;
        for &v in versions(d) {
            for comp in [None, Some(Compression::Lz4), Some(Compression::Snappy)] {
                if v == Version::V5 && comp == Some(Compression::Snappy) {
                    continue;
                }
                let c = connect(s.addr, v, comp).await;
                assert_eq!(c.version(), v);
                let r = rows(c.query("SELECT key FROM system.local", q()).await.unwrap());
                assert_eq!(r.len(), 1, "{d:?} {v} {comp:?}");
                // A value bigger than a v5 segment payload (128 KiB), compressed or not.
                let big = "x".repeat(300_000);
                let k = v.as_u8() as i32 * 10 + comp.map_or(0, |c| if c == Compression::Lz4 { 1 } else { 2 });
                c.query("INSERT INTO srv_m.t (k, c, v) VALUES (?, 0, ?)", values(vec![int(k), text(&big)])).await.unwrap();
                let r = rows(c.query("SELECT v FROM srv_m.t WHERE k = ?", values(vec![int(k)])).await.unwrap());
                assert_eq!(r.to_vec(), vec![vec![Some(text(&big))]], "{d:?} {v} {comp:?}");
                let p = c.prepare("SELECT c FROM srv_m.t WHERE k = ?").await.unwrap();
                let r = rows(c.execute(&p, values(vec![int(k)])).await.unwrap());
                assert_eq!(r.to_vec(), vec![vec![Some(int(0))]]);
            }
        }
    }
}

#[tokio::test]
async fn cassandra_v6_beta() {
    let s = single(Dialect::Cassandra).await;
    let c = connect(s.addr, Version::V6, Some(Compression::Lz4)).await;
    assert_eq!(c.version(), Version::V6);
    assert_eq!(rows(c.query("SELECT key FROM system.local", q()).await.unwrap()).len(), 1);
}

#[tokio::test]
async fn supported_maps() {
    let s = single_with(
        Dialect::Scylla,
        |c| c.settings.shards = 4,
        |_| {},
        |st| {
            st.shard_aware_port = Some(19042);
            st.shard_aware_port_ssl = Some(19142);
        },
    )
    .await;
    let c = connect(s.addr, Version::V4, None).await;
    let sup = c.supported();
    let keys: Vec<&str> = sup.0.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        [
            "CLIENT_OPTIONS",
            "COMPRESSION",
            "CQL_VERSION",
            "SCYLLA_HOST_ID",
            "SCYLLA_LWT_ADD_METADATA_MARK",
            "SCYLLA_NR_SHARDS",
            "SCYLLA_PARTITIONER",
            "SCYLLA_RATE_LIMIT_ERROR",
            "SCYLLA_SHARD",
            "SCYLLA_SHARDING_ALGORITHM",
            "SCYLLA_SHARDING_IGNORE_MSB",
            "SCYLLA_SHARD_AWARE_PORT",
            "SCYLLA_SHARD_AWARE_PORT_SSL",
            "SCYLLA_USE_METADATA_ID",
            "TABLETS_ROUTING_V1"
        ]
    );
    assert_eq!(sup.get("SCYLLA_NR_SHARDS").unwrap(), ["4"]);
    let shard: u32 = sup.get("SCYLLA_SHARD").unwrap()[0].parse().unwrap();
    assert!(shard < 4);
    assert_eq!(sup.get("SCYLLA_SHARD_AWARE_PORT").unwrap(), ["19042"]);
    assert_eq!(sup.get("SCYLLA_HOST_ID").unwrap(), [nosql_types::timeuuid::format_uuid(&s.db.config().host_id())]);
    assert_eq!(sup.get("COMPRESSION").unwrap(), ["lz4", "snappy"]);
    assert_eq!(sup.get("CQL_VERSION").unwrap(), ["3.3.1"]);
    // Shards go round robin on the regular port.
    let mut seen = std::collections::HashSet::new();
    for _ in 0..16 {
        let c = connect(s.addr, Version::V4, None).await;
        seen.insert(c.supported().get("SCYLLA_SHARD").unwrap()[0].clone());
    }
    assert!(seen.len() >= 2, "{seen:?}");
    // Cassandra's map, exactly.
    let s = single(Dialect::Cassandra).await;
    for v in [Version::V3, Version::V4, Version::V5] {
        let c = connect(s.addr, v, None).await;
        assert_eq!(c.supported(), &nosql_proto::supported::cassandra());
    }
}

/// The reference's "Accepted protocol versions" table, ScyllaDB column.
#[tokio::test]
async fn scylla_version_negotiation() {
    let s = single(Dialect::Scylla).await;
    for v in [1u8, 2, 5, 6, 7, 127] {
        let mut r = Raw::connect(s.addr).await;
        r.send(&envelope(v, 0, 1, 5, &[])).await;
        let f = r.read().await.unwrap();
        assert_eq!((f.version, f.stream), (0x84, 0), "v{v}");
        assert_eq!(f.error().unwrap(), (code::PROTOCOL_ERROR, format!("Invalid or unsupported protocol version: {v}")));
        assert!(r.closed().await, "v{v}");
    }
    // The response direction bit is just another version number to ScyllaDB.
    let mut r = Raw::connect(s.addr).await;
    r.send(&envelope(0x84, 0, 1, 5, &[])).await;
    assert_eq!(r.read().await.unwrap().error().unwrap().1, "Invalid or unsupported protocol version: 132");
    for v in [3u8, 4] {
        let mut r = Raw::connect(s.addr).await;
        r.send(&envelope(v, 0, 1, 5, &[])).await;
        let f = r.read().await.unwrap();
        assert_eq!((f.version, f.stream, f.opcode), (0x80 | v, 1, 6));
    }
    // Negative stream ids are echoed.
    let mut r = Raw::connect(s.addr).await;
    r.send(&envelope(4, 0, -5, 5, &[])).await;
    assert_eq!(r.read().await.unwrap().stream, -5);
}

/// The same table, Cassandra column.
#[tokio::test]
async fn cassandra_version_negotiation() {
    let s = single(Dialect::Cassandra).await;
    let all = "(3/v3, 4/v4, 5/v5, 6/v6-beta)";
    for v in [7u8, 127] {
        let mut r = Raw::connect(s.addr).await;
        r.send(&envelope(v, 0, 1, 5, &[])).await;
        let f = r.read().await.unwrap();
        assert_eq!((f.version, f.stream), (0x85, 1));
        assert_eq!(f.error().unwrap().1, format!("Invalid or unsupported protocol version ({v}); supported versions are {all}"));
        assert!(!r.closed().await, "the connection stays open");
    }
    let mut r = Raw::connect(s.addr).await;
    r.send(&envelope(6, 0, 1, 5, &[])).await;
    let f = r.read().await.unwrap();
    assert_eq!((f.version, f.flags), (0x86, 0x10));
    assert_eq!(f.error().unwrap().1, "Beta version of the protocol used (6/v6-beta), but USE_BETA flag is unset");
    let mut r = Raw::connect(s.addr).await;
    r.send(&envelope(6, 0x10, 1, 5, &[])).await;
    assert_eq!(r.read().await.unwrap().opcode, 6);
    // v1/v2 get an 8-byte header in their own version.
    let mut r = Raw::connect(s.addr).await;
    let mut v2 = vec![2u8, 0, 1, 1];
    v2.extend_from_slice(&(0u32).to_be_bytes());
    v2.push(0);
    r.send(&v2).await;
    let f = r.read_as(true).await.unwrap();
    assert_eq!((f.version, f.stream), (0x82, 1));
    assert!(f.error().unwrap().1.starts_with("Invalid or unsupported protocol version (2)"));
    let mut r = Raw::connect(s.addr).await;
    r.send(&envelope(0x84, 0, 1, 5, &[])).await;
    assert_eq!(r.read().await.unwrap().error().unwrap().1, "Wrong protocol direction (expected REQUEST, got RESPONSE) for opcode 5 (OPTIONS)");
}

#[tokio::test]
async fn startup_validation() {
    let ok = [("CQL_VERSION", "3.0.0")];
    /// STARTUP options, Cassandra's error, ScyllaDB's error.
    type Case<'a> = (&'a [(&'a str, &'a str)], Option<&'a str>, Option<&'a str>);
    let cases: &[Case] = &[
        // options, Cassandra's error, ScyllaDB's error.
        (&[], Some("Missing value CQL_VERSION in STARTUP message"), None),
        (&[("CQL_VERSION", "2.0.0")], Some("CQL version 2.0.0 is not supported by the binary protocol (supported version are >= 3.0.0)"), None),
        (&[("CQL_VERSION", "abc")], Some("Invalid version value: abc"), None),
        (&[("CQL_VERSION", "3.4.7")], None, None),
        (&[("CQL_VERSION", "3.0.0"), ("COMPRESSION", "zstd")], Some("Unknown compression algorithm: zstd"), Some("Unknown compression algorithm: zstd")),
        (&[("CQL_VERSION", "3.0.0"), ("COMPRESSION", "LZ4")], None, None),
        (&[("CQL_VERSION", "3.0.0"), ("NOPE", "1")], None, None),
        (&[("CQL_VERSION", "3.0.0"), ("DRIVER_NAME", "x"), ("DRIVER_VERSION", "1")], None, None),
    ];
    for d in ALL {
        let s = single(d).await;
        for (opts, cass, scylla) in cases {
            let mut r = Raw::connect(s.addr).await;
            r.send(&envelope(4, 0, 1, 1, &startup_body(opts))).await;
            let f = r.read().await.unwrap();
            let want = if d == Dialect::Cassandra { cass } else { scylla };
            match want {
                Some(msg) => assert_eq!(f.error(), Some((code::PROTOCOL_ERROR, msg.to_string())), "{d:?} {opts:?}"),
                None => assert_eq!(f.opcode, 2, "{d:?} {opts:?} {f:?}"),
            }
            assert_eq!((f.version, f.stream), (0x84, 1));
        }
        // A failed STARTUP can be retried.
        let mut r = Raw::connect(s.addr).await;
        r.send(&envelope(4, 0, 1, 1, &startup_body(&[("CQL_VERSION", "3.0.0"), ("COMPRESSION", "zstd")]))).await;
        assert!(r.read().await.unwrap().error().is_some());
        r.send(&envelope(4, 0, 2, 1, &startup_body(&ok))).await;
        assert_eq!(r.read().await.unwrap().opcode, 2);
        // Cassandra pins the version at STARTUP even when it fails.
        if d == Dialect::Cassandra {
            let mut r = Raw::connect(s.addr).await;
            r.send(&envelope(4, 0, 1, 1, &startup_body(&[]))).await;
            assert!(r.read().await.unwrap().error().is_some());
            r.send(&envelope(3, 0, 2, 5, &[])).await;
            assert_eq!(
                r.read().await.unwrap().error().unwrap().1,
                "Invalid message version. Got 3/v3 but previous messages on this connection had version 4/v4"
            );
        }
    }
    // ScyllaDB refuses a compressed STARTUP and hangs up.
    let s = single(Dialect::Scylla).await;
    let mut r = Raw::connect(s.addr).await;
    r.send(&envelope(4, 0x01, 1, 1, &startup_body(&ok))).await;
    let f = r.read().await.unwrap();
    assert_eq!((f.stream, f.error().unwrap()), (0, (code::PROTOCOL_ERROR, "Unknown compression algorithm".into())));
    assert!(r.closed().await);
}

#[tokio::test]
async fn connection_stage_errors() {
    for d in ALL {
        let s = single(d).await;
        // Before STARTUP.
        for (opcode, name) in [(7u8, "QUERY"), (11, "REGISTER"), (15, "AUTH_RESPONSE")] {
            let mut r = Raw::connect(s.addr).await;
            r.send(&envelope(4, 0, 1, opcode, &query_body("SELECT 1", 0))).await;
            let f = r.read().await.unwrap();
            let msg = match d {
                Dialect::Cassandra => format!("Unexpected message {name}, expecting STARTUP or OPTIONS"),
                Dialect::Scylla => format!("Unexpected message {opcode}, expecting STARTUP or OPTIONS"),
            };
            assert_eq!(f.error(), Some((code::PROTOCOL_ERROR, msg)), "{d:?}");
            // Cassandra drops the connection, ScyllaDB keeps it.
            assert_eq!(r.closed().await, d == Dialect::Cassandra, "{d:?} {name}");
        }
        // After READY.
        let mut r = Raw::ready(s.addr, 4).await;
        r.send(&envelope(4, 0, 1, 1, &startup_body(&[("CQL_VERSION", "3.0.0")]))).await;
        assert_eq!(
            r.read().await.unwrap().error(),
            Some((code::PROTOCOL_ERROR, "Unexpected message STARTUP, the connection is already initialized".into()))
        );
        r.send(&envelope(4, 0, 2, 5, &[])).await;
        assert_eq!(r.read().await.unwrap().opcode, 6, "OPTIONS after STARTUP");
        // REGISTER with an unknown event.
        let mut body = 1u16.to_be_bytes().to_vec();
        body.extend_from_slice(&4u16.to_be_bytes());
        body.extend_from_slice(b"NOPE");
        r.send(&envelope(4, 0, 3, 11, &body)).await;
        let want = match d {
            Dialect::Cassandra => "Invalid value 'NOPE' for Type",
            Dialect::Scylla => "Invalid value 'NOPE' for Event.Type",
        };
        assert_eq!(r.read().await.unwrap().error(), Some((code::PROTOCOL_ERROR, want.into())));
        // AUTH_RESPONSE after READY.
        r.send(&envelope(4, 0, 4, 15, &[0, 0, 0, 0])).await;
        let f = r.read().await.unwrap();
        match d {
            Dialect::Cassandra => assert_eq!(f.opcode, 0x10),
            Dialect::Scylla => assert_eq!(f.error(), Some((code::SERVER_ERROR, "Should not reach".into()))),
        }
        // A response opcode sent by the client.
        r.send(&envelope(4, 0, 5, 2, &[])).await;
        let f = r.read().await.unwrap();
        match d {
            Dialect::Cassandra => assert_eq!(f.error().unwrap().1, "Wrong protocol direction (expected RESPONSE, got REQUEST) for opcode 2 (READY)"),
            Dialect::Scylla => assert_eq!(f.error().unwrap().1, "Unknown opcode 2"),
        }
        // Unknown opcode.
        r.send(&envelope(4, 0, 6, 0x2a, &[])).await;
        assert_eq!(r.read().await.unwrap().error().unwrap().1, "Unknown opcode 42");
        // Still usable.
        r.send(&envelope(4, 0, 7, 7, &query_body("SELECT key FROM system.local", 0))).await;
        assert_eq!(r.read().await.unwrap().opcode, 8);
    }
}

#[tokio::test]
async fn request_decoding_errors() {
    for d in ALL {
        let s = single(d).await;
        let mut r = Raw::ready(s.addr, 4).await;
        // Consistency 0x0042.
        let mut b = query_body("SELECT key FROM system.local", 0);
        let n = b.len();
        b[n - 3..n - 1].copy_from_slice(&0x42u16.to_be_bytes());
        r.send(&envelope(4, 0, 1, 7, &b)).await;
        assert_eq!(r.read().await.unwrap().error(), Some((code::PROTOCOL_ERROR, "Unknown code 66 for a consistency level".into())));
        // Page size 0.
        let mut b = query_body("SELECT key FROM system.local", 0x04);
        b.extend_from_slice(&0i32.to_be_bytes());
        r.send(&envelope(4, 0, 2, 7, &b)).await;
        let f = r.read().await.unwrap();
        match d {
            Dialect::Cassandra => assert_eq!(f.error(), Some((code::PROTOCOL_ERROR, "The page size cannot be 0".into()))),
            Dialect::Scylla => assert_eq!(f.opcode, 8, "{f:?}"),
        }
        // Flag 0x80 on v4 with nothing after it.
        r.send(&envelope(4, 0, 3, 7, &query_body("SELECT key FROM system.local", 0x80))).await;
        let want = match d {
            Dialect::Cassandra => (code::PROTOCOL_ERROR, "Not enough bytes to read an UTF8 serialized string preceded by its 2 bytes length".to_string()),
            Dialect::Scylla => (code::SERVER_ERROR, "Bit mask contains invalid enumeration indices.".to_string()),
        };
        assert_eq!(r.read().await.unwrap().error(), Some(want));
        // A truncated QUERY body.
        r.send(&envelope(4, 0, 4, 7, &[0, 0])).await;
        let want = match d {
            Dialect::Cassandra => "Not enough bytes to read an UTF8 serialized string preceded by its 4 bytes length",
            Dialect::Scylla => "truncated frame: expected 4 bytes, length is 2",
        };
        assert_eq!(r.read().await.unwrap().error(), Some((code::PROTOCOL_ERROR, want.into())));
        // EXECUTE of an id nobody prepared.
        let mut b = 4u16.to_be_bytes().to_vec();
        b.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef, 0, 1, 0]);
        r.send(&envelope(4, 0, 5, 10, &b)).await;
        let (c, _) = r.read().await.unwrap().error().unwrap();
        assert_eq!(c, code::UNPREPARED);
        // BATCH of an unknown type: Cassandra refuses it, ScyllaDB runs it.
        r.send(&envelope(4, 0, 6, 13, &[7, 0, 0, 0, 1, 0])).await;
        let f = r.read().await.unwrap();
        match d {
            Dialect::Cassandra => assert_eq!(f.error(), Some((code::PROTOCOL_ERROR, "Invalid BATCH message type 7".into()))),
            Dialect::Scylla => assert_eq!(f.opcode, 8, "{f:?}"),
        }
    }
}

#[tokio::test]
async fn oversized_frames() {
    // ScyllaDB: before STARTUP the body is capped at 100000 bytes; after it, the estimate.
    let s = single(Dialect::Scylla).await;
    let mut r = Raw::connect(s.addr).await;
    let mut h = envelope(4, 0, 1, 5, &[]);
    h[5..9].copy_from_slice(&300_000_000u32.to_be_bytes());
    r.send(&h).await;
    assert_eq!(
        r.read().await.unwrap().error(),
        Some((code::PROTOCOL_ERROR, "Initial message size too large (300000000), rejecting as invalid".into()))
    );
    assert!(r.closed().await);
    let mut r = Raw::ready(s.addr, 4).await;
    let mut h = envelope(4, 0, 1, 7, &[]);
    h[5..9].copy_from_slice(&60_000_000u32.to_be_bytes());
    r.send(&h).await;
    let (c, m) = r.read().await.unwrap().error().unwrap();
    assert_eq!(c, code::INVALID);
    assert!(m.starts_with("request size too large (frame size 60000000; estimate 120008000; allowed"), "{m}");
    assert!(r.closed().await);
    // Cassandra: frames over 16 MiB are skipped and answered; the connection stays usable.
    let s = single(Dialect::Cassandra).await;
    let mut r = Raw::ready(s.addr, 4).await;
    let body = vec![0u8; 17 << 20];
    r.send(&envelope(4, 0, 9, 7, &body)).await;
    let f = r.read().await.unwrap();
    assert_eq!(f.stream, 9);
    assert_eq!(
        f.error(),
        Some((code::INVALID, format!("Request is too big: length {} exceeds maximum allowed length 16777216.", (17 << 20) + 9)))
    );
    r.send(&envelope(4, 0, 10, 5, &[])).await;
    assert_eq!(r.read().await.unwrap().opcode, 6);
}

#[tokio::test]
async fn corrupt_compressed_bodies() {
    for d in ALL {
        let s = single(d).await;
        let mut r = Raw::connect(s.addr).await;
        r.send(&envelope(4, 0, 1, 1, &startup_body(&[("CQL_VERSION", "3.0.0"), ("COMPRESSION", "lz4")]))).await;
        let f = r.read().await.unwrap();
        assert_eq!(f.opcode, 2);
        assert_eq!(f.flags & 1, 1, "READY is compressed");
        r.send(&envelope(4, 1, 2, 5, &[0xff, 0xff, 0xff, 0xff, 1, 2, 3])).await;
        if d == Dialect::Scylla {
            // The ERROR is compressed like every response after STARTUP.
            let mut f = r.read().await.unwrap();
            assert_eq!((f.opcode, f.flags & 1), (0, 1));
            f.body = Compression::Lz4.decompress(&f.body, 1 << 20).unwrap();
            assert_eq!(f.error().unwrap().0, code::PROTOCOL_ERROR);
        }
        assert!(r.closed().await, "{d:?}");
    }
}

#[tokio::test]
async fn handshake_timeout() {
    let s = single_with(Dialect::Scylla, |_| {}, |_| {}, |st| st.handshake_timeout = Duration::from_millis(200)).await;
    let mut r = Raw::connect(s.addr).await;
    assert!(r.closed().await);
    // Ready connections stay.
    let mut r = Raw::ready(s.addr, 4).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    r.send(&envelope(4, 0, 2, 5, &[])).await;
    assert_eq!(r.read().await.unwrap().opcode, 6);
}

#[tokio::test]
async fn password_authentication() {
    for d in ALL {
        let s = single_with(d, |_| {}, |spec| *spec = spec.clone().with_password("cassandra", "cassandra"), |_| {}).await;
        // AUTHENTICATE names PasswordAuthenticator.
        let mut r = Raw::connect(s.addr).await;
        r.send(&envelope(4, 0, 1, 1, &startup_body(&[("CQL_VERSION", "3.0.0")]))).await;
        let f = r.read().await.unwrap();
        assert_eq!(f.opcode, 3);
        assert_eq!(&f.body[2..], b"org.apache.cassandra.auth.PasswordAuthenticator");
        // Only AUTH_RESPONSE now (ScyllaDB prints the opcode's number).
        for (op, name) in [(7u8, "QUERY"), (5, "OPTIONS"), (1, "STARTUP"), (11, "REGISTER"), (9, "PREPARE")] {
            let body = if op == 1 { startup_body(&[("CQL_VERSION", "3.0.0")]) } else if op == 11 { vec![0, 0] } else { query_body("SELECT 1", 0) };
            r.send(&envelope(4, 0, 2, op, &body)).await;
            let shown = if d == Dialect::Scylla { op.to_string() } else { name.to_string() };
            assert_eq!(r.read().await.unwrap().error(), Some((code::PROTOCOL_ERROR, format!("Unexpected message {shown}, expecting SASL_RESPONSE"))));
        }
        // Tokens that aren't SASL PLAIN, as each reference answers them.
        let scylla_bad = "Username and/or password are incorrect";
        let cases: [(&[u8], &str, &str); 5] = [
            (b"cassandra\0cassandra", "Authentication ID must not be null", scylla_bad),
            (b"\0cassandra\0", "Password must not be null", scylla_bad),
            (b"a\0b\0c\0d", "Credential format error: username or password is empty or contains NUL(\\0) character", scylla_bad),
            (b"", "Password must not be null", "Authentication ID must not be null"),
            (b"\0\0pw", "Authentication ID must not be null", scylla_bad),
        ];
        for (tok, cass, scylla) in cases {
            let mut body = (tok.len() as i32).to_be_bytes().to_vec();
            body.extend_from_slice(tok);
            r.send(&envelope(4, 0, 3, 15, &body)).await;
            let want = if d == Dialect::Cassandra { cass } else { scylla };
            assert_eq!(r.read().await.unwrap().error(), Some((code::BAD_CREDENTIALS, want.to_string())), "{d:?} {tok:?}");
        }
        r.send(&envelope(4, 0, 3, 15, &(-1i32).to_be_bytes())).await;
        let want = match d {
            Dialect::Cassandra => (code::SERVER_ERROR, "java.lang.NullPointerException: Cannot invoke \"java.nio.ByteBuffer.remaining()\" because \"b\" is null".to_string()),
            Dialect::Scylla => (code::BAD_CREDENTIALS, "Authentication ID must not be null".to_string()),
        };
        assert_eq!(r.read().await.unwrap().error(), Some(want));
        // Platform credentials succeed: ScyllaDB's AUTH_SUCCESS token is empty, Cassandra's null.
        let mut token = (20i32).to_be_bytes().to_vec();
        token.extend_from_slice(b"\0cassandra\0cassandra");
        let ok: &[u8] = if d == Dialect::Scylla { &[0, 0, 0, 0] } else { &[0xff, 0xff, 0xff, 0xff] };
        r.send(&envelope(4, 0, 3, 15, &token)).await;
        let f = r.read().await.unwrap();
        assert_eq!((f.opcode, &f.body[..]), (0x10, ok));
        // Again once ready: a new negotiation, in both dialects.
        r.send(&envelope(4, 0, 3, 15, &token)).await;
        let f = r.read().await.unwrap();
        assert_eq!((f.opcode, &f.body[..]), (0x10, ok));
        r.send(&envelope(4, 0, 4, 7, &query_body("SELECT key FROM system.local", 0))).await;
        assert_eq!(r.read().await.unwrap().opcode, 8);
        // The client needs credentials.
        let e = Client::connect(cfg(s.addr, Version::V4)).await.unwrap_err();
        assert!(matches!(e, ClientError::Auth(_)), "{e:?}");
        let c = Client::connect(cfg(s.addr, Version::V4).credentials("cassandra", "cassandra")).await.unwrap();
        assert_eq!(rows(c.query("SELECT key FROM system.local", q()).await.unwrap()).len(), 1);
        // On v5 segments start after AUTHENTICATE.
        if d == Dialect::Cassandra {
            let c = Client::connect(cfg(s.addr, Version::V5).compression(Some(Compression::Lz4)).credentials("cassandra", "cassandra")).await.unwrap();
            assert_eq!(rows(c.query("SELECT key FROM system.local", q()).await.unwrap()).len(), 1);
        }
    }
}

#[tokio::test]
async fn multiplexing() {
    for in_flight in [256, 1] {
        let s = single_with(Dialect::Scylla, |_| {}, |_| {}, |st| st.max_in_flight = in_flight).await;
        let c = connect(s.addr, Version::V4, None).await;
        setup(&c, "srv_mx").await;
        let p = c.prepare("INSERT INTO srv_mx.t (k, c, v) VALUES (?, ?, ?)").await.unwrap();
        let writes = (0..300).map(|i| {
            let c = c.clone();
            let p = p.clone();
            async move { c.execute(&p, values(vec![int(i % 7), int(i), text(&format!("v{i}"))])).await }
        });
        for r in futures::future::join_all(writes).await {
            assert_eq!(r.unwrap(), ResultBody::Void);
        }
        let sel = c.prepare("SELECT v FROM srv_mx.t WHERE k = ? AND c = ?").await.unwrap();
        let reads = (0..300).map(|i| {
            let c = c.clone();
            let sel = sel.clone();
            async move { (i, c.execute(&sel, values(vec![int(i % 7), int(i)])).await) }
        });
        for (i, r) in futures::future::join_all(reads).await {
            assert_eq!(rows(r.unwrap()).to_vec(), vec![vec![Some(text(&format!("v{i}")))]], "{i}");
        }
    }
}

#[tokio::test]
async fn schema_events() {
    for d in ALL {
        let s = single(d).await;
        for &v in versions(d) {
            let ks = format!("srv_ev{}", v.as_u8());
            let listener = connect(s.addr, v, None).await;
            let mut events = listener.register(&[EventType::SchemaChange]).await.unwrap();
            let status_only = connect(s.addr, v, None).await;
            let mut quiet = status_only.register(&[EventType::StatusChange, EventType::TopologyChange]).await.unwrap();
            let c = connect(s.addr, v, None).await;
            setup(&c, &ks).await;
            let e = tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap().unwrap();
            assert_eq!(e, Event::SchemaChange(nosql_proto::SchemaChange::keyspace(SchemaChangeType::Created, &ks)), "{d:?} {v}");
            let e = tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap().unwrap();
            assert_eq!(e, Event::SchemaChange(nosql_proto::SchemaChange::object(SchemaChangeType::Created, SchemaTarget::Table, &ks, "t")));
            c.query(&format!("DROP TABLE {ks}.t"), q()).await.unwrap();
            let e = tokio::time::timeout(Duration::from_secs(5), events.recv()).await.unwrap().unwrap();
            assert_eq!(e, Event::SchemaChange(nosql_proto::SchemaChange::object(SchemaChangeType::Dropped, SchemaTarget::Table, &ks, "t")));
            assert!(tokio::time::timeout(Duration::from_millis(200), quiet.recv()).await.is_err(), "not registered for schema changes");
        }
    }
}

#[tokio::test]
async fn prepared_statements_survive_reconnects() {
    for d in ALL {
        let s = single(d).await;
        let c = connect(s.addr, Version::V4, None).await;
        setup(&c, "srv_ps").await;
        let p = c.prepare("INSERT INTO srv_ps.t (k, c, v) VALUES (?, ?, ?)").await.unwrap();
        assert_eq!(p.metadata.columns.len(), 3);
        assert_eq!(p.metadata.pk_indexes, vec![0]);
        assert!(p.result_metadata.no_metadata);
        drop(c);
        let c2 = connect(s.addr, Version::V4, None).await;
        c2.execute(&p, values(vec![int(1), int(2), text("x")])).await.unwrap();
        // The same statement prepared again gets the same id.
        let again = c2.prepare("INSERT INTO srv_ps.t (k, c, v) VALUES (?, ?, ?)").await.unwrap();
        assert_eq!(again.id, p.id);
        // An unknown id is UNPREPARED with the id in the body.
        let mut bogus = p.clone();
        bogus.id = Bytes::from_static(&[9; 16]);
        let e = server_error(c2.execute(&bogus, values(vec![int(1), int(2), text("x")])).await.unwrap_err());
        assert_eq!(e.code, code::UNPREPARED);
        assert_eq!(e.detail, nosql_proto::ErrorDetail::Unprepared { id: bogus.id.clone() });
        // Values and markers must agree.
        let e = server_error(c2.execute(&p, values(vec![int(1)])).await.unwrap_err());
        assert_eq!(e.code, code::INVALID);
    }
}

#[tokio::test]
async fn v5_result_metadata_ids() {
    let s = single(Dialect::Cassandra).await;
    let c = connect(s.addr, Version::V5, None).await;
    setup(&c, "srv_md").await;
    c.query("INSERT INTO srv_md.t (k, c, v) VALUES (1, 1, 'a')", q()).await.unwrap();
    let p = c.prepare("SELECT c, v FROM srv_md.t WHERE k = ?").await.unwrap();
    let id = p.result_metadata_id.clone().unwrap();
    assert_eq!(id.len(), 16);
    let skip = QueryParams { skip_metadata: true, ..values(vec![int(1)]) };
    let r = rows(c.execute(&p, skip.clone()).await.unwrap());
    assert!(r.metadata.no_metadata, "same id + skip: no metadata");
    assert_eq!(r.to_vec(), vec![vec![Some(int(1)), Some(text("a"))]]);
    let mut stale = p.clone();
    stale.result_metadata_id = Some(Bytes::from_static(&[0; 16]));
    let r = rows(c.execute(&stale, skip).await.unwrap());
    assert_eq!(r.metadata.new_metadata_id.as_ref(), Some(&id), "METADATA_CHANGED with the current id");
    assert_eq!(r.metadata.columns.len(), 2);
    // Without skip, full metadata and no change flag.
    let r = rows(c.execute(&p, values(vec![int(1)])).await.unwrap());
    assert!(!r.metadata.no_metadata && r.metadata.new_metadata_id.is_none());
    // Keyspace per request (v5).
    let in_ks = QueryParams { keyspace: Some("srv_md".into()), ..values(vec![int(1)]) };
    let r = rows(c.query("SELECT v FROM t WHERE k = ?", in_ks).await.unwrap());
    assert_eq!(r.len(), 1);
    let p = c.prepare_in("SELECT v FROM t WHERE k = ?", Some("srv_md")).await.unwrap();
    assert_eq!(rows(c.execute(&p, values(vec![int(1)])).await.unwrap()).len(), 1);
    // QUERY with skip_metadata.
    let r = rows(c.query("SELECT v FROM srv_md.t", QueryParams { skip_metadata: true, ..q() }).await.unwrap());
    assert!(r.metadata.no_metadata);
}

#[tokio::test]
async fn scylla_extensions() {
    let s = single(Dialect::Scylla).await;
    let mut cfg = cfg(s.addr, Version::V4);
    cfg.scylla_extensions = true;
    let c = Client::connect(cfg).await.unwrap();
    assert!(c.ctx().features.metadata_id);
    setup(&c, "srv_sx").await;
    let p = c.prepare("SELECT v FROM srv_sx.t WHERE k = ?").await.unwrap();
    assert_eq!(p.result_metadata_id.as_ref().map(|i| i.len()), Some(16), "SCYLLA_USE_METADATA_ID on v4");
    let mut stale = p.clone();
    stale.result_metadata_id = Some(Bytes::from_static(&[1; 16]));
    c.query("INSERT INTO srv_sx.t (k, c, v) VALUES (1, 1, 'a')", q()).await.unwrap();
    let r = rows(c.execute(&stale, QueryParams { skip_metadata: true, ..values(vec![int(1)]) }).await.unwrap());
    assert!(r.metadata.new_metadata_id.is_some());
    // Without the extension v4 has no metadata ids.
    let plain = connect(s.addr, Version::V4, None).await;
    let p = plain.prepare("SELECT v FROM srv_sx.t WHERE k = ?").await.unwrap();
    assert_eq!(p.result_metadata_id, None);
}

/// Rejections among 100 runs of `cql` (`#` is the run's number), and the last rejection.
async fn rate_limited(c: &Client, cql: &str) -> (usize, Option<nosql_proto::ErrorBody>) {
    let (mut n, mut last) = (0, None);
    for i in 0..100 {
        match c.query(&cql.replace('#', &i.to_string()), q()).await {
            Ok(_) => {}
            Err(ClientError::Server(e)) if e.message.starts_with("Per-partition rate limit") => {
                n += 1;
                last = Some(e);
            }
            Err(e) => panic!("{cql}: {e:?}"),
        }
    }
    (n, last)
}

/// ScyllaDB's `per_partition_rate_limit`, with what counts as measured on the reference (see
/// `nosql_engine::rate_limit`): RATE_LIMIT_ERROR on connections that negotiated
/// SCYLLA_RATE_LIMIT_ERROR, CONFIG_ERROR on the others.
#[tokio::test]
async fn per_partition_rate_limit() {
    let s = single(Dialect::Scylla).await;
    let plain = connect(s.addr, Version::V4, None).await;
    assert_eq!(plain.ctx().features.rate_limit_code, None);
    let mut ext = cfg(s.addr, Version::V4);
    ext.scylla_extensions = true;
    let ext = Client::connect(ext).await.unwrap();
    assert_eq!(ext.ctx().features.rate_limit_code, Some(code::SCYLLA_RATE_LIMIT));
    let limit = "per_partition_rate_limit = {'max_writes_per_second': 1, 'max_reads_per_second': 1}";
    for cql in [
        "CREATE KEYSPACE srv_rl WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}".to_string(),
        format!("CREATE TABLE srv_rl.t (k int, c int, v int, PRIMARY KEY (k, c)) WITH {limit}"),
        "CREATE TABLE srv_rl.u (k int, c int, v int, PRIMARY KEY (k, c))".to_string(),
        format!("CREATE TABLE srv_rl.n (k int PRIMARY KEY, n counter) WITH {limit}"),
    ] {
        plain.query(&cql, q()).await.unwrap();
    }
    let write = "Per-partition rate limit reached for write in table srv_rl.t, rejected by coordinator";
    let read = "Per-partition rate limit reached for read in table srv_rl.t, rejected by coordinator";
    let cases: [(&str, Option<&str>); 18] = [
        ("INSERT INTO srv_rl.t (k, c, v) VALUES (1, #, 1)", Some(write)),
        ("UPDATE srv_rl.t USING TTL 100 SET v = 1 WHERE k = 2 AND c = #", Some(write)),
        ("DELETE FROM srv_rl.t WHERE k = 3", Some(write)),
        ("BEGIN UNLOGGED BATCH INSERT INTO srv_rl.t (k, c, v) VALUES (4, #, 1); INSERT INTO srv_rl.t (k, c, v) VALUES (5, #, 1) APPLY BATCH", Some(write)),
        // A logged batch of one mutation skips the batch log, and is limited.
        ("BEGIN BATCH INSERT INTO srv_rl.t (k, c, v) VALUES (6, #, 1); INSERT INTO srv_rl.t (k, c, v) VALUES (6, #, 2) APPLY BATCH", Some(write)),
        ("BEGIN BATCH INSERT INTO srv_rl.t (k, c, v) VALUES (7, #, 1); INSERT INTO srv_rl.t (k, c, v) VALUES (8, #, 1) APPLY BATCH", None),
        ("BEGIN BATCH INSERT INTO srv_rl.t (k, c, v) VALUES (9, #, 1); INSERT INTO srv_rl.u (k, c, v) VALUES (9, #, 1) APPLY BATCH", None),
        ("INSERT INTO srv_rl.t (k, c, v) VALUES (10, #, 1) IF NOT EXISTS", None),
        ("UPDATE srv_rl.n SET n = n + 1 WHERE k = 1", None),
        ("SELECT * FROM srv_rl.n WHERE k = 1", Some("Per-partition rate limit reached for read in table srv_rl.n, rejected by coordinator")),
        ("SELECT * FROM srv_rl.t WHERE k = 11", Some(read)),
        ("SELECT * FROM srv_rl.t WHERE k IN (12, 13)", Some(read)),
        ("SELECT count(*) FROM srv_rl.t WHERE k = 14", Some(read)),
        ("SELECT * FROM srv_rl.t WHERE k = 15 AND v = 1 ALLOW FILTERING", Some(read)),
        ("SELECT * FROM srv_rl.t", None),
        ("SELECT * FROM srv_rl.t WHERE token(k) > 0", None),
        ("SELECT DISTINCT k FROM srv_rl.t", None),
        ("INSERT INTO srv_rl.u (k, c, v) VALUES (1, #, 1)", None),
    ];
    for (cql, want) in cases {
        let (n, e) = rate_limited(&plain, cql).await;
        match want {
            // Admitted with probability 1 / (count ln 2), the count halving every second: about
            // 8 of 100 pass when they run within one second (more on a slow machine).
            Some(m) => {
                assert!((20..=100).contains(&n), "{cql}: {n} rejected");
                let e = e.unwrap();
                assert_eq!((e.code, e.message.as_str(), e.detail), (code::CONFIG_ERROR, m, nosql_proto::ErrorDetail::None), "{cql}");
            }
            None => assert_eq!(n, 0, "{cql}"),
        }
    }
    // With the extension: the code and body ScyllaDB sends (op type 1 write, 0 read).
    for (cql, op_type) in [("INSERT INTO srv_rl.t (k, c, v) VALUES (1, #, 1)", 1), ("SELECT * FROM srv_rl.t WHERE k = 11", 0)] {
        let (n, e) = rate_limited(&ext, cql).await;
        assert!(n >= 20, "{cql}: {n}");
        let e = e.unwrap();
        assert_eq!((e.code, e.detail), (code::SCYLLA_RATE_LIMIT, nosql_proto::ErrorDetail::RateLimit { op_type, rejected_by_coordinator: true }));
    }
    // ALTER replaces the limits: no read limit now, and a write limit far above the rate.
    plain.query("ALTER TABLE srv_rl.t WITH per_partition_rate_limit = {'max_writes_per_second': 1000}", q()).await.unwrap();
    assert_eq!(rate_limited(&plain, "SELECT * FROM srv_rl.t WHERE k = 11").await.0, 0);
    assert_eq!(rate_limited(&plain, "INSERT INTO srv_rl.t (k, c, v) VALUES (1, #, 1)").await.0, 0);
}

#[tokio::test]
async fn use_tracing_and_payloads() {
    for d in ALL {
        let s = single(d).await;
        let c = connect(s.addr, Version::V4, None).await;
        setup(&c, "srv_use").await;
        assert_eq!(c.query("USE srv_use", q()).await.unwrap(), ResultBody::SetKeyspace("srv_use".into()));
        c.query("INSERT INTO t (k, c, v) VALUES (1, 1, 'a')", q()).await.unwrap();
        assert_eq!(rows(c.query("SELECT v FROM t WHERE k = 1", q()).await.unwrap()).len(), 1);
        // Unqualified statements prepare in the connection's keyspace.
        let p = c.prepare("SELECT v FROM t WHERE k = ?").await.unwrap();
        assert_eq!(rows(c.execute(&p, values(vec![int(1)])).await.unwrap()).len(), 1);
        // Tracing: the response carries a session id.
        let mut rf = RequestFrame::new(0, Request::Query(Query { query: "SELECT v FROM t WHERE k = 1".into(), params: q() }));
        rf.tracing = true;
        let resp = c.send(rf).await.unwrap();
        assert!(resp.tracing_id.is_some());
        assert!(matches!(resp.response, Response::Result(ResultBody::Rows(_))));
        // A request custom payload is accepted (and not echoed).
        let mut rf = RequestFrame::new(0, Request::Query(Query { query: "SELECT v FROM t WHERE k = 1".into(), params: q() }));
        rf.custom_payload = Some(vec![("k".into(), Some(Bytes::from_static(b"v")))]);
        let resp = c.send(rf).await.unwrap();
        assert_eq!(resp.custom_payload, None);
        // Errors keep the connection.
        let e = server_error(c.query("SELEC", q()).await.unwrap_err());
        assert_eq!(e.code, code::SYNTAX_ERROR);
        assert_eq!(rows(c.query("SELECT v FROM t WHERE k = 1", q()).await.unwrap()).len(), 1);
    }
}

#[tokio::test]
async fn batches_and_paging() {
    for d in ALL {
        let s = single(d).await;
        let c = connect(s.addr, Version::V4, None).await;
        setup(&c, "srv_bp").await;
        let p = c.prepare("INSERT INTO srv_bp.t (k, c, v) VALUES (?, ?, ?)").await.unwrap();
        let mut statements: Vec<BatchStatement> =
            (0..20).map(|i| BatchStatement { query: BatchQuery::Prepared(p.id.clone()), values: vec![Value::Set(int(1)), Value::Set(int(i)), Value::Set(text("p"))] }).collect();
        statements.push(BatchStatement { query: BatchQuery::Text("INSERT INTO srv_bp.t (k, c, v) VALUES (1, 100, 'text')".into()), values: vec![] });
        for kind in [BatchType::Logged, BatchType::Unlogged] {
            let b = Batch { kind, statements: statements.clone(), params: QueryParams::new(Consistency::One) };
            assert_eq!(c.batch(b).await.unwrap(), ResultBody::Void);
        }
        let all = c.pages("SELECT c FROM srv_bp.t WHERE k = 1", QueryParams { page_size: Some(6), ..q() }).collect().await.unwrap();
        assert_eq!(all.len(), 21, "{d:?}");
        let mut pager = c.pages("SELECT c FROM srv_bp.t WHERE k = 1", QueryParams { page_size: Some(6), ..q() });
        let first = pager.next_page().await.unwrap().unwrap();
        assert_eq!(first.len(), 6);
        assert!(first.metadata.paging_state.is_some());
        let sel = c.prepare("SELECT c FROM srv_bp.t WHERE k = ?").await.unwrap();
        let all = c.pages_prepared(&sel, QueryParams { page_size: Some(5), ..values(vec![int(1)]) }).collect().await.unwrap();
        assert_eq!(all.len(), 21);
    }
}

/// Deterministic noise.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

#[tokio::test]
async fn malformed_input_never_takes_the_server_down() {
    let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
    for d in ALL {
        let s = single(d).await;
        for round in 0..60u32 {
            let mut r = if round.is_multiple_of(3) { Raw::connect(s.addr).await } else { Raw::ready(s.addr, 4).await };
            for _ in 0..5 {
                let opcode = [1u8, 5, 7, 9, 10, 11, 13, 15, (rng.next() % 256) as u8][(rng.next() % 9) as usize];
                let len = (rng.next() % 200) as usize;
                let mut body = rng.bytes(len);
                if rng.next().is_multiple_of(2) && body.len() > 4 {
                    // Plausible lengths in the first field.
                    body[..4].copy_from_slice(&((rng.next() % 64) as u32).to_be_bytes());
                }
                let flags = (rng.next() % 16) as u8 & !0x01;
                r.send(&envelope(4, flags, (rng.next() % 100) as i16, opcode, &body)).await;
            }
            // Garbage without an envelope.
            if round.is_multiple_of(5) {
                let junk = rng.bytes(64);
                r.send(&junk).await;
            }
            drop(r);
        }
        // Still serving.
        let c = connect(s.addr, Version::V4, None).await;
        assert_eq!(rows(c.query("SELECT key FROM system.local", q()).await.unwrap()).len(), 1);
    }
}

#[tokio::test]
async fn shard_aware_listener() {
    let s = single_with(Dialect::Scylla, |c| c.settings.shards = 3, |_| {}, |_| {}).await;
    let opts = nosql_server::ListenOptions { shard_aware: true, ..Default::default() };
    let dir: Arc<dyn nosql_server::Directory> = s.dir.clone();
    let addr = nosql_server::net::start(dir, opts).await.unwrap();
    for _ in 0..6 {
        let sock = tokio::net::TcpSocket::new_v4().unwrap();
        sock.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let port = sock.local_addr().unwrap().port();
        let mut stream = sock.connect(addr).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream.write_all(&envelope(4, 0, 1, 5, &[])).await.unwrap();
        let mut buf = vec![0u8; 4096];
        let mut got = Vec::new();
        while got.len() < 9 || got.len() < 9 + u32::from_be_bytes(got[5..9].try_into().unwrap()) as usize {
            let n = stream.read(&mut buf).await.unwrap();
            assert!(n > 0);
            got.extend_from_slice(&buf[..n]);
        }
        let mut r = nosql_proto::wire::Reader::new(Bytes::copy_from_slice(&got[9..]));
        let map = r.string_multimap().unwrap();
        let shard = &map.iter().find(|(k, _)| k == "SCYLLA_SHARD").unwrap().1[0];
        assert_eq!(shard, &(port as u32 % 3).to_string());
    }
}

/// One AUTH_RESPONSE-stage probe sequence, as (description, answer) pairs.
async fn auth_probes(addr: std::net::SocketAddr) -> Vec<(String, String)> {
    fn shown(f: Option<RawFrame>) -> String {
        match f {
            None => "closed".into(),
            Some(f) => match f.error() {
                Some((c, m)) => format!("ERROR {c:#06x} {m} (version {:#x}, stream {})", f.version, f.stream),
                None => format!("opcode {} body {:02x?} (version {:#x}, stream {})", f.opcode, &f.body[..], f.version, f.stream),
            },
        }
    }
    let token = |t: &[u8]| {
        let mut b = (t.len() as i32).to_be_bytes().to_vec();
        b.extend_from_slice(t);
        b
    };
    let mut out = Vec::new();
    let mut r = Raw::connect(addr).await;
    r.send(&envelope(4, 0, 1, 1, &startup_body(&[("CQL_VERSION", "3.0.0")]))).await;
    out.push(("STARTUP".into(), shown(r.read().await)));
    for (op, body) in [(7u8, query_body("SELECT 1", 0)), (5, vec![]), (1, startup_body(&[("CQL_VERSION", "3.0.0")])), (11, vec![0, 0]), (9, query_body("SELECT 1", 0)[..12].to_vec())] {
        r.send(&envelope(4, 0, 2, op, &body)).await;
        out.push((format!("opcode {op} while authenticating"), shown(r.read().await)));
    }
    for t in [&b"cassandra\0cassandra"[..], b"\0cassandra\0", b"a\0b\0c\0d", b"", b"\0\0pw", b"\0cassandra\0cassandra", b"\0cassandra\0cassandra"] {
        r.send(&envelope(4, 0, 3, 15, &token(t))).await;
        out.push((format!("AUTH_RESPONSE {t:?}"), shown(r.read().await)));
    }
    r.send(&envelope(4, 0, 4, 7, &query_body("SELECT key FROM system.local", 0))).await;
    out.push(("QUERY after login".into(), shown(r.read().await).split(" body").next().unwrap_or("").to_string()));
    let mut r = Raw::connect(addr).await;
    r.send(&envelope(4, 0, 1, 1, &startup_body(&[("CQL_VERSION", "3.0.0")]))).await;
    r.read().await;
    r.send(&envelope(4, 0, 3, 15, &(-1i32).to_be_bytes())).await;
    out.push(("AUTH_RESPONSE null".into(), shown(r.read().await)));
    out
}

/// Probes the server answers itself (not the engine's authenticator), against the
/// PasswordAuthenticator references (`ROLL_NOSQL_REF_SCYLLA_AUTH=127.0.0.1:19043`,
/// `ROLL_NOSQL_REF_CASSANDRA_AUTH=127.0.0.1:29043`, superuser cassandra/cassandra).
#[tokio::test]
async fn auth_matches_the_references() {
    let mut ran = false;
    for (d, var) in [(Dialect::Scylla, "ROLL_NOSQL_REF_SCYLLA_AUTH"), (Dialect::Cassandra, "ROLL_NOSQL_REF_CASSANDRA_AUTH")] {
        let Ok(reference) = std::env::var(var) else {
            eprintln!("skipped {d:?}: set {var}");
            continue;
        };
        ran = true;
        let reference: std::net::SocketAddr = reference.parse().unwrap();
        let s = single_with(d, |_| {}, |spec| *spec = spec.clone().with_password("cassandra", "cassandra"), |_| {}).await;
        let ours = auth_probes(s.addr).await;
        let theirs = auth_probes(reference).await;
        assert_eq!(ours, theirs, "{d:?}");
        // v5 (Cassandra): AUTHENTICATE in a v5 envelope.
        if d == Dialect::Cassandra {
            for addr in [s.addr, reference] {
                let mut r = Raw::connect(addr).await;
                r.send(&envelope(5, 0, 1, 1, &startup_body(&[("CQL_VERSION", "3.0.0")]))).await;
                let f = r.read().await.unwrap();
                assert_eq!((f.version, f.opcode), (0x85, 3));
            }
        }
        // Clients of both log in with the same credentials.
        for addr in [s.addr, reference] {
            let c = Client::connect(cfg(addr, Version::V4).credentials("cassandra", "cassandra")).await.unwrap();
            assert_eq!(rows(c.query("SELECT key FROM system.local", q()).await.unwrap()).len(), 1);
        }
    }
    if !ran {
        eprintln!("auth_matches_the_references: no auth reference configured");
    }
}

/// Cells as UTF-8 (`None` for null).
async fn texts(c: &Client, cql: &str, params: QueryParams) -> Vec<Vec<Option<String>>> {
    let r = rows(c.query(cql, params).await.unwrap_or_else(|e| panic!("{cql}: {e:?}")));
    r.to_vec().into_iter().map(|row| row.into_iter().map(|v| v.map(|b| String::from_utf8_lossy(&b).into_owned())).collect()).collect()
}

/// A request frame with the TRACING flag.
fn traced(request: Request) -> RequestFrame {
    RequestFrame { tracing: true, ..RequestFrame::new(0, request) }
}

/// Frames with the TRACING flag run traced: every RESULT (and a traced PREPARE's) names a
/// complete trace in `system_traces`; errors and untraced frames carry no id.
#[tokio::test]
async fn tracing_writes_complete_traces() {
    for d in ALL {
        let s = single(d).await;
        let c = connect(s.addr, Version::V4, None).await;
        setup(&c, "srv_trc").await;
        let c = &c;
        let session_of = |id: [u8; 16]| async move {
            let by_id = QueryParams { values: vec![Value::Set(Bytes::copy_from_slice(&id))], ..q() };
            let sessions = rows(c.query("SELECT duration, request FROM system_traces.sessions WHERE session_id = ?", by_id.clone()).await.unwrap());
            let events = rows(c.query("SELECT activity FROM system_traces.events WHERE session_id = ?", by_id).await.unwrap());
            (sessions.to_vec(), events.len())
        };
        let query = |cql: &str| Request::Query(Query { query: cql.into(), params: q() });
        // QUERY.
        let resp = c.send(traced(query("INSERT INTO srv_trc.t (k, c, v) VALUES (1, 1, 'a')"))).await.unwrap();
        let id = resp.tracing_id.expect("traced QUERY");
        let (sessions, events) = session_of(id).await;
        assert_eq!(sessions.len(), 1, "{d:?}");
        assert!(sessions[0][0].is_some(), "{d:?}: the session row has its duration when the response arrives");
        assert_eq!(sessions[0][1].as_deref(), Some(&b"Execute CQL3 query"[..]), "{d:?}");
        assert!(events > 0, "{d:?}");
        // PREPARE and EXECUTE.
        let resp = c.send(traced(Request::Prepare(nosql_proto::Prepare { query: "SELECT v FROM srv_trc.t WHERE k = ?".into(), keyspace: None }))).await.unwrap();
        let id = resp.tracing_id.expect("traced PREPARE");
        assert_eq!(session_of(id).await.0[0][1].as_deref(), Some(&b"Preparing CQL3 query"[..]), "{d:?}");
        let Response::Result(ResultBody::Prepared(p)) = resp.response else { panic!("{d:?}: not PREPARED") };
        let exec = Request::Execute(nosql_proto::Execute { id: p.id.clone(), result_metadata_id: None, params: values(vec![int(1)]), tablet_version_block: None });
        let resp = c.send(traced(exec)).await.unwrap();
        let id = resp.tracing_id.expect("traced EXECUTE");
        assert_eq!(session_of(id).await.0.len(), 1, "{d:?}");
        // BATCH.
        let b = Batch {
            kind: BatchType::Logged,
            statements: vec![BatchStatement { query: BatchQuery::Text("INSERT INTO srv_trc.t (k, c, v) VALUES (2, 1, 'b')".into()), values: vec![] }],
            params: QueryParams { consistency: Consistency::One, ..q() },
        };
        let resp = c.send(traced(Request::Batch(b))).await.unwrap();
        let id = resp.tracing_id.expect("traced BATCH");
        assert_eq!(session_of(id).await.0[0][1].as_deref(), Some(&b"Execute batch of CQL3 queries"[..]), "{d:?}");
        // A traced error carries its id too (both references set the tracing flag on ERROR
        // frames, syntax errors included); untraced frames carry none.
        let resp = c.send(traced(query("SELEC"))).await.unwrap();
        assert!(matches!(resp.response, Response::Error(_)), "{d:?}");
        let id = resp.tracing_id.expect("traced ERROR");
        assert_eq!(session_of(id).await.0.len(), 1, "{d:?}");
        let resp = c.send(RequestFrame::new(0, query("SELECT v FROM srv_trc.t"))).await.unwrap();
        assert!(resp.tracing_id.is_none(), "{d:?}");
        let resp = c.send(RequestFrame::new(0, Request::Prepare(nosql_proto::Prepare { query: "SELECT k FROM srv_trc.t".into(), keyspace: None }))).await.unwrap();
        assert!(resp.tracing_id.is_none(), "{d:?}");
    }
}

/// Bound connections are rows of `system.clients` (ScyllaDB) / `system_views.clients`
/// (Cassandra): driver, STARTUP options, role, keyspace, request count, shard; closed ones go.
#[tokio::test]
async fn connections_are_listed_in_the_clients_table() {
    for d in ALL {
        let s = single_with(d, |_| {}, |spec| *spec = spec.clone().with_password("cassandra", "cassandra"), |_| {}).await;
        let c = Client::connect(cfg(s.addr, Version::V4).credentials("cassandra", "cassandra")).await.unwrap();
        let (table, extra) = match d {
            Dialect::Scylla => ("system.clients", "client_type"),
            Dialect::Cassandra => ("system_views.clients", "keyspace_name"),
        };
        let cql = format!("SELECT driver_name, driver_version, username, connection_stage, {extra} FROM {table}");
        let got = texts(&c, &cql, q()).await;
        let stage = if d == Dialect::Scylla { "READY" } else { "ready" };
        assert_eq!(got.len(), 1, "{d:?}: {got:?}");
        assert_eq!(got[0][0].as_deref(), Some("roll nosql-proto"), "{d:?}");
        assert_eq!(got[0][1].as_deref(), Some(env!("CARGO_PKG_VERSION")), "{d:?}");
        assert_eq!(got[0][2].as_deref(), Some("cassandra"), "{d:?}");
        assert_eq!(got[0][3].as_deref(), Some(stage), "{d:?}");
        assert_eq!(got[0][4].as_deref(), if d == Dialect::Scylla { Some("cql") } else { None }, "{d:?}");
        // The STARTUP map, port and protocol version.
        let r = rows(c.query(&format!("SELECT client_options, port, protocol_version FROM {table}"), q()).await.unwrap()).to_vec();
        let options = String::from_utf8_lossy(r[0][0].as_ref().unwrap()).into_owned();
        assert!(options.contains("CQL_VERSION") && options.contains("DRIVER_NAME"), "{d:?}: {options:?}");
        assert!(i32::from_be_bytes(r[0][1].as_ref().unwrap()[..].try_into().unwrap()) > 0);
        assert_eq!(r[0][2].as_deref(), Some(&4i32.to_be_bytes()[..]), "{d:?}");
        // USE (Cassandra shows the keyspace) and the request count.
        setup(&c, "srv_cli").await;
        c.query("USE srv_cli", q()).await.unwrap();
        if d == Dialect::Cassandra {
            let got = texts(&c, "SELECT keyspace_name FROM system_views.clients", q()).await;
            assert_eq!(got, [[Some("srv_cli".to_string())]]);
            let r = rows(c.query("SELECT request_count FROM system_views.clients", q()).await.unwrap()).to_vec();
            let n = i64::from_be_bytes(r[0][0].as_ref().unwrap()[..].try_into().unwrap());
            assert!(n >= 5, "{n}");
        } else {
            let r = rows(c.query("SELECT shard_id FROM system.clients", q()).await.unwrap()).to_vec();
            assert_eq!(r[0][0].as_deref(), Some(&0i32.to_be_bytes()[..]));
        }
        // A second connection, then it closes.
        let c2 = Client::connect(cfg(s.addr, Version::V4).credentials("cassandra", "cassandra")).await.unwrap();
        assert_eq!(texts(&c, &cql, q()).await.len(), 2, "{d:?}");
        assert_eq!(s.db.clients().len(), 2);
        drop(c2);
        let mut left = 2;
        for _ in 0..200 {
            left = s.db.clients().len();
            if left == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(left, 1, "{d:?}: a closed connection leaves the table");
    }
}

/// The platform superuser is the engine's role (`DbSpec::with_password` sets
/// `DbConfig::superuser`): the engine's PasswordAuthenticator checks it, so the role's password
/// is the one truth — a rotated one applies at once, the spec's old one stops working.
#[tokio::test]
async fn the_engine_authenticates_the_platform_superuser() {
    for d in ALL {
        let s = single_with(
            d,
            |c| {
                c.settings.authenticator = "PasswordAuthenticator".into();
                c.settings.auth_bcrypt_log2_rounds = 4;
                c.superuser = Some(nosql_engine::config::Superuser { name: "admin".into(), password: "pw0".into() });
            },
            |spec| *spec = spec.clone().with_password("admin", "pw0"),
            |_| {},
        )
        .await;
        let login = |u: &'static str, p: &'static str| Client::connect(cfg(s.addr, Version::V4).credentials(u, p));
        let c = login("admin", "pw0").await.unwrap();
        // A superuser role of the engine's (LIST ROLES shows it).
        let roles = texts(&c, "LIST ROLES", q()).await;
        assert!(roles.iter().any(|r| r[0].as_deref() == Some("admin")), "{d:?}: {roles:?}");
        // AUTH_SUCCESS: empty bytes in ScyllaDB, null in Cassandra (from the engine too).
        let mut r = Raw::connect(s.addr).await;
        r.send(&envelope(4, 0, 1, 1, &startup_body(&[("CQL_VERSION", "3.0.0")]))).await;
        assert_eq!(r.read().await.unwrap().opcode, 3);
        let mut token = (10i32).to_be_bytes().to_vec();
        token.extend_from_slice(b"\0admin\0pw0");
        r.send(&envelope(4, 0, 2, 15, &token)).await;
        let f = r.read().await.unwrap();
        let ok: &[u8] = if d == Dialect::Scylla { &[0, 0, 0, 0] } else { &[0xff, 0xff, 0xff, 0xff] };
        assert_eq!((f.opcode, &f.body[..]), (0x10, ok), "{d:?}");
        // Rotation through the engine: the new password works, the spec's old one doesn't.
        assert!(nosql_engine::auth::set_password(&s.db, "admin", "pw1").await.unwrap());
        login("admin", "pw1").await.unwrap();
        let e = login("admin", "pw0").await.unwrap_err();
        let want = match d {
            Dialect::Scylla => "Username and/or password are incorrect",
            Dialect::Cassandra => "Provided username admin and/or password are incorrect",
        };
        assert!(format!("{e:?}").contains(want), "{d:?}: {e:?}");
        // Other roles log in through the engine as before, and a ready connection can log in
        // again as one (the clients table follows).
        c.query("CREATE ROLE bob WITH PASSWORD = 'b' AND LOGIN = true", q()).await.unwrap();
        login("bob", "b").await.unwrap();
        let mut token = (6i32).to_be_bytes().to_vec();
        token.extend_from_slice(b"\0bob\0b");
        r.send(&envelope(4, 0, 3, 15, &token)).await;
        assert_eq!(r.read().await.unwrap().opcode, 0x10, "{d:?}");
        let table = if d == Dialect::Scylla { "system.clients" } else { "system_views.clients" };
        let users = texts(&c, &format!("SELECT username FROM {table}"), q()).await;
        assert!(users.iter().any(|u| u[0].as_deref() == Some("bob")), "{d:?}: {users:?}");
    }
}
