//! The HTTP layer, SigV4 failures and the Streams API, through `Alternator::handle` and over a
//! real socket. Expected behaviour was measured on ScyllaDB 2026.3.3 (see `src/http.rs`).

mod common;

use std::io::{Read, Write};
use std::sync::Arc;

use bytes::Bytes;
use nosql_alternator::auth::{self, RoleSecret, StaticCredentials};
use nosql_alternator::http::{HttpRequest, HttpResponse, MAX_REQUEST_BYTES};
use nosql_alternator::memory::MemoryBackend;
use nosql_alternator::{Alternator, Config};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn alt() -> Alternator {
    Alternator::new(Arc::new(MemoryBackend::new()), Config::default())
}

fn req(method: &str, path: &str, headers: &[(&str, &str)], body: impl Into<Bytes>) -> HttpRequest {
    let (path, query) = path.split_once('?').unwrap_or((path, ""));
    HttpRequest {
        method: method.into(),
        path: path.into(),
        query: query.into(),
        headers: headers
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        body: body.into(),
        peer: None,
    }
}

fn api(op: &str, body: &str) -> HttpRequest {
    let target = format!("DynamoDB_20120810.{op}");
    req(
        "POST",
        "/",
        &[("X-Amz-Target", &target), ("Host", "localhost:8000")],
        body.to_string(),
    )
}

fn json_of(r: &HttpResponse) -> Value {
    serde_json::from_slice(&r.body).unwrap_or(Value::Null)
}

fn message(r: &HttpResponse) -> String {
    json_of(r)["message"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

fn error_type(r: &HttpResponse) -> String {
    let t = json_of(r)["__type"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    t.rsplit('#').next().unwrap_or_default().to_string()
}

async fn call(a: &Alternator, op: &str, body: Value) -> (u16, Value) {
    let r = a.handle(api(op, &body.to_string())).await;
    (r.status, json_of(&r))
}

#[tokio::test]
async fn routing_and_health() {
    let a = alt();
    let r = a
        .handle(req("GET", "/", &[("Host", "node1:8000")], ""))
        .await;
    assert_eq!((r.status, &r.body[..]), (200, &b"healthy: node1:8000"[..]));
    assert_eq!(r.header("content-type"), Some("text/plain"));
    assert!(r.header("date").is_some());
    let r = a.handle(req("GET", "/?x=1", &[], "")).await;
    assert_eq!(&r.body[..], b"healthy: ");
    // Unknown all-uppercase methods count as GET.
    assert_eq!(a.handle(req("BREW", "/", &[], "")).await.status, 200);
    for (m, p) in [
        ("HEAD", "/"),
        ("PUT", "/"),
        ("DELETE", "/"),
        ("GET", "/foo"),
        ("get", "/"),
        ("POST", "/localnodes"),
    ] {
        let r = a.handle(req(m, p, &[], "")).await;
        assert_eq!(r.status, 404, "{m} {p}");
        assert_eq!(&r.body[..], br#"{"message": "Not found", "code": 404}"#);
        assert_eq!(r.header("content-type"), Some("application/json"));
    }
    let r = a.handle(req("OPTIONS", "/", &[], "")).await;
    assert_eq!((r.status, r.body.len()), (200, 0));

    let r = a.handle(req("GET", "/localnodes", &[], "")).await;
    assert_eq!((r.status, &r.body[..]), (200, &br#"["127.0.0.1"]"#[..]));
    assert_eq!(r.header("content-type"), Some("application/json"));
    for (q, want) in [
        ("dc=datacenter1", r#"["127.0.0.1"]"#),
        ("dc=other", "[]"),
        ("rack=rack1", r#"["127.0.0.1"]"#),
        ("dc=datacenter1&rack=nope", "[]"),
        ("dc=", r#"["127.0.0.1"]"#),
        ("junk=1", r#"["127.0.0.1"]"#),
    ] {
        let r = a
            .handle(req("GET", &format!("/localnodes?{q}"), &[], ""))
            .await;
        assert_eq!(std::str::from_utf8(&r.body).unwrap(), want, "{q}");
    }
    assert_eq!(
        a.handle(req("OPTIONS", "/localnodes", &[], ""))
            .await
            .status,
        200
    );
    assert_eq!(
        a.handle(req("GET", "/localnodes/", &[], "")).await.status,
        200
    );

    // Server and Date headers follow the configuration.
    let quiet = Alternator::new(
        Arc::new(MemoryBackend::new()),
        Config {
            server_header: Some("roll".into()),
            disable_date_header: true,
            ..Config::default()
        },
    );
    let r = quiet.handle(req("GET", "/", &[], "")).await;
    assert_eq!(r.header("server"), Some("roll"));
    assert!(r.header("date").is_none());
}

#[tokio::test]
async fn targets_and_bodies() {
    let a = alt();
    let r = a.handle(req("POST", "/", &[], "{}")).await;
    assert_eq!(
        (error_type(&r), message(&r)),
        (
            "UnknownOperationException".into(),
            "Unsupported operation ".into()
        )
    );
    for t in [
        "Foo.ListTables",
        ".ListTables",
        " DynamoDB_20120811.ListTables\t",
    ] {
        let r = a
            .handle(req("POST", "/", &[("X-Amz-Target", t)], "{}"))
            .await;
        assert_eq!(r.status, 200, "{t}");
        assert_eq!(r.header("content-type"), Some("application/x-amz-json-1.0"));
    }
    let r = a
        .handle(req(
            "POST",
            "/",
            &[("X-Amz-Target", "DynamoDB_20120810.listTables")],
            "{}",
        ))
        .await;
    assert_eq!(message(&r), "Unsupported operation listTables");
    // The unknown operation is reported before the body is parsed.
    let r = a.handle(api("TransactWriteItems", "not json")).await;
    assert_eq!(message(&r), "Unsupported operation TransactWriteItems");
    // The request Content-Type doesn't matter.
    let r = a
        .handle(req(
            "POST",
            "/",
            &[
                ("X-Amz-Target", "x.ListTables"),
                ("Content-Type", "text/plain"),
            ],
            "{}",
        ))
        .await;
    assert_eq!(r.status, 200);
    assert_eq!(
        message(&a.handle(api("ListTables", "")).await),
        "Parsing JSON failed: The document is empty. at 0"
    );
    assert_eq!(
        message(&a.handle(api("ListTables", "[1]")).await),
        "Request content must be an object"
    );
    assert_eq!(
        message(&a.handle(api("ListTables", "{\"a\" 1}")).await),
        "Parsing JSON failed: Missing a colon after a name of object member. at 5"
    );
    let deep = "[".repeat(80) + &"]".repeat(80);
    assert_eq!(
        message(
            &a.handle(api("ListTables", &format!("{{\"a\":{deep}}}")))
                .await
        ),
        "Max nested level reached: 78"
    );
    // TagResource answers with an empty body.
    call(&a, "CreateTable", json!({"TableName":"tgt","KeySchema":[{"AttributeName":"p","KeyType":"HASH"}],"AttributeDefinitions":[{"AttributeName":"p","AttributeType":"S"}],"BillingMode":"PAY_PER_REQUEST"})).await;
    let r = a
        .handle(api(
            "TagResource",
            r#"{"ResourceArn":"arn:scylla:alternator:alternator_tgt:scylla:table/tgt","Tags":[{"Key":"k","Value":"v"}]}"#,
        ))
        .await;
    assert_eq!((r.status, r.body.len()), (200, 0));
    // DescribeEndpoints uses the Host header.
    let (_, v) = call(&a, "DescribeEndpoints", json!({})).await;
    assert_eq!(v["Endpoints"][0]["Address"], "localhost:8000");
    let r = a
        .handle(req(
            "POST",
            "/",
            &[("X-Amz-Target", "x.DescribeEndpoints")],
            "{}",
        ))
        .await;
    assert_eq!(
        message(&r),
        "DescribeEndpoints needs a 'Host:' header in request"
    );
}

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

#[tokio::test]
async fn compressed_requests() {
    let a = alt();
    let target = ("X-Amz-Target", "DynamoDB_20120810.ListTables");
    let ok = |r: &HttpResponse| r.status == 200 && json_of(r)["TableNames"].is_array();
    assert!(ok(&a
        .handle(req(
            "POST",
            "/",
            &[target, ("Content-Encoding", "gzip")],
            gzip(b"{}")
        ))
        .await));
    assert!(ok(&a
        .handle(req(
            "POST",
            "/",
            &[target, ("Content-Encoding", " deflate ")],
            zlib(b"{}")
        ))
        .await));
    assert!(ok(&a
        .handle(req("POST", "/", &[target, ("Content-Encoding", "")], "{}"))
        .await));
    let two = [gzip(b"{"), gzip(b"}")].concat();
    assert!(ok(&a
        .handle(req(
            "POST",
            "/",
            &[target, ("Content-Encoding", "gzip")],
            two
        ))
        .await));
    for enc in ["identity", "GZIP", "x-gzip", "br", "gzip, deflate"] {
        let r = a
            .handle(req("POST", "/", &[target, ("Content-Encoding", enc)], "{}"))
            .await;
        assert_eq!(
            (r.status, message(&r)),
            (500, "Unsupported Content-Encoding".into()),
            "{enc}"
        );
        assert_eq!(error_type(&r), "InternalServerError");
    }
    for (enc, body, want) in [
        (
            "gzip",
            b"{}".to_vec(),
            "Error during gzip decompression of request body",
        ),
        ("gzip", Vec::new(), "Truncated gzip in request body"),
        (
            "deflate",
            gzip(b"{}"),
            "Error during gzip decompression of request body",
        ),
        (
            "gzip",
            [gzip(b"{}"), b"junk".to_vec()].concat(),
            "Error during gzip decompression of request body",
        ),
    ] {
        let r = a
            .handle(req("POST", "/", &[target, ("Content-Encoding", enc)], body))
            .await;
        assert_eq!(message(&r), want);
    }
    let g = gzip(b"{}");
    let r = a
        .handle(req(
            "POST",
            "/",
            &[target, ("Content-Encoding", "gzip")],
            g[..g.len() - 1].to_vec(),
        ))
        .await;
    assert_eq!(message(&r), "Truncated gzip in request body");
    // A compressed empty document.
    let r = a
        .handle(req(
            "POST",
            "/",
            &[target, ("Content-Encoding", "gzip")],
            gzip(b""),
        ))
        .await;
    assert_eq!(
        message(&r),
        "Parsing JSON failed: The document is empty. at 0"
    );
}

#[tokio::test]
async fn compressed_responses_and_cors() {
    let a = alt();
    call(&a, "CreateTable", json!({"TableName":"big","KeySchema":[{"AttributeName":"p","KeyType":"HASH"}],"AttributeDefinitions":[{"AttributeName":"p","AttributeType":"S"}],"BillingMode":"PAY_PER_REQUEST"})).await;
    call(
        &a,
        "PutItem",
        json!({"TableName":"big","Item":{"p":{"S":"k"},"x":{"S":"y".repeat(5000)}}}),
    )
    .await;
    let get = r#"{"TableName":"big","Key":{"p":{"S":"k"}}}"#;
    let target = ("X-Amz-Target", "DynamoDB_20120810.GetItem");
    let r = a
        .handle(req(
            "POST",
            "/",
            &[target, ("Accept-Encoding", "gzip")],
            get,
        ))
        .await;
    assert_eq!(r.header("content-encoding"), Some("gzip"));
    let mut plain = String::new();
    flate2::read::GzDecoder::new(&r.body[..])
        .read_to_string(&mut plain)
        .unwrap();
    assert!(plain.contains(&"y".repeat(5000)));
    let r = a
        .handle(req(
            "POST",
            "/",
            &[target, ("Accept-Encoding", "deflate;q=0.9, gzip;q=0.5")],
            get,
        ))
        .await;
    assert_eq!(r.header("content-encoding"), Some("deflate"));
    let r = a
        .handle(req(
            "POST",
            "/",
            &[target, ("Accept-Encoding", "gzip;q=0")],
            get,
        ))
        .await;
    assert_eq!(r.header("content-encoding"), None);
    let r = a.handle(req("POST", "/", &[target], get)).await;
    assert_eq!(r.header("content-encoding"), None);
    // Small responses stay uncompressed.
    let r = a
        .handle(req(
            "POST",
            "/",
            &[
                ("X-Amz-Target", "x.ListTables"),
                ("Accept-Encoding", "gzip"),
            ],
            "{}",
        ))
        .await;
    assert_eq!(r.header("content-encoding"), None);

    // CORS.
    let r = a
        .handle(req(
            "POST",
            "/",
            &[("X-Amz-Target", "x.ListTables"), ("Origin", "http://x")],
            "{}",
        ))
        .await;
    assert_eq!(r.header("access-control-allow-origin"), Some("*"));
    assert_eq!(
        r.header("access-control-expose-headers"),
        Some("x-amzn-RequestId,x-amzn-ErrorType,x-amzn-ErrorMessage,Date")
    );
    assert!(r.header("access-control-max-age").is_none());
    let r = a
        .handle(req(
            "OPTIONS",
            "/",
            &[
                ("Origin", "null"),
                ("Access-Control-Request-Method", "POST"),
                ("Access-Control-Request-Headers", "x-amz-target"),
            ],
            "",
        ))
        .await;
    assert_eq!(r.header("access-control-allow-methods"), Some("POST"));
    assert_eq!(
        r.header("access-control-allow-headers"),
        Some("x-amz-target")
    );
    assert_eq!(r.header("access-control-max-age"), Some("7200"));
    let r = a.handle(req("GET", "/", &[("Origin", "")], "")).await;
    assert!(r.header("access-control-allow-origin").is_none());
    let r = a
        .handle(req("GET", "/localnodes", &[("Origin", "x")], ""))
        .await;
    assert!(r.header("access-control-allow-origin").is_none());
}

/// Sends raw bytes to the server and returns the status line and body.
async fn raw(addr: std::net::SocketAddr, head: &str, body: &[u8]) -> (String, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(head.as_bytes()).await.unwrap();
    let _ = s.write_all(body).await;
    let mut out = Vec::new();
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), s.read_to_end(&mut out)).await;
    let text = String::from_utf8_lossy(&out).to_string();
    let status = text.lines().next().unwrap_or_default().to_string();
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or_default()
        .to_string();
    (status, body)
}

#[tokio::test]
async fn request_size_limit() {
    let a = alt();
    // In-process: one byte over the limit.
    let r = a
        .handle(api("ListTables", &" ".repeat(MAX_REQUEST_BYTES + 1)))
        .await;
    assert_eq!((r.status, error_type(&r)), (413, "PayloadTooLarge".into()));
    assert_eq!(
        message(&r),
        "Request content length limit of 16777216 bytes exceeded"
    );
    // Exactly the limit is fine (whitespace around the document).
    let body = format!("{{}}{}", " ".repeat(MAX_REQUEST_BYTES - 2));
    assert_eq!(a.handle(api("ListTables", &body)).await.status, 200);
    // Over a socket: refused from the Content-Length, before the body.
    let (addr, _) = common::start(false).await;
    let head = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nX-Amz-Target: DynamoDB_20120810.ListTables\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        MAX_REQUEST_BYTES + 1
    );
    let (status, body) = raw(addr, &head, b"").await;
    assert!(status.contains("413"), "{status}");
    assert!(body.contains("PayloadTooLarge"), "{body}");
    let head = "POST / HTTP/1.1\r\nHost: x\r\nX-Amz-Target: DynamoDB_20120810.ListTables\r\nContent-Length: 2\r\nConnection: close\r\n\r\n";
    let (status, body) = raw(addr, head, b"{}").await;
    assert!(status.contains("200"), "{status}");
    assert_eq!(body, r#"{"TableNames":[]}"#);
}

fn signed(op: &str, body: &str, access: &str, secret: &str, amz_date: &str) -> HttpRequest {
    let target = format!("DynamoDB_20120810.{op}");
    let mut r = req(
        "POST",
        "/",
        &[
            ("Host", "localhost:8000"),
            ("X-Amz-Target", &target),
            ("X-Amz-Date", amz_date),
        ],
        body.to_string(),
    );
    let signed_headers = vec![
        "host".to_string(),
        "x-amz-date".to_string(),
        "x-amz-target".to_string(),
    ];
    let date = &amz_date[..8];
    let sig = auth::sign(
        &r,
        body.as_bytes(),
        &signed_headers,
        amz_date,
        (date, "us-east-1", "dynamodb"),
        secret,
    );
    r.headers.push((
        "Authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={access}/{date}/us-east-1/dynamodb/aws4_request, SignedHeaders=host;x-amz-date;x-amz-target, Signature={sig}"
        ),
    ));
    r
}

fn now_amz() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

#[tokio::test]
async fn signature_failures() {
    let a = Alternator::new(
        Arc::new(MemoryBackend::new()),
        Config {
            enforce_authorization: true,
            ..Config::default()
        },
    )
    .with_credentials(Arc::new(StaticCredentials(vec![(
        "alice".into(),
        RoleSecret {
            salted_hash: "s3cret".into(),
            can_login: true,
        },
    )])));
    let now = now_amz();
    assert_eq!(
        a.handle(signed("ListTables", "{}", "alice", "s3cret", &now))
            .await
            .status,
        200
    );
    let cases = [
        (
            signed("ListTables", "{}", "alice", "wrong", &now),
            "UnrecognizedClientException",
        ),
        (
            signed("ListTables", "{}", "nobody", "s3cret", &now),
            "UnrecognizedClientException",
        ),
        (
            signed("ListTables", "{}", "alice", "s3cret", "20170101T010101Z"),
            "InvalidSignatureException",
        ),
        (
            signed("ListTables", "{}", "alice", "s3cret", "30200101T010101Z"),
            "InvalidSignatureException",
        ),
        (
            api("ListTables", "{}"),
            "MissingAuthenticationTokenException",
        ),
    ];
    for (r, want) in cases {
        let resp = a.handle(r).await;
        assert_eq!(resp.status, 400);
        assert_eq!(error_type(&resp), want, "{}", message(&resp));
    }
    let r = a
        .handle(signed(
            "ListTables",
            "{}",
            "alice",
            "s3cret",
            "20170101T010101Z",
        ))
        .await;
    assert!(message(&r).starts_with("Signature expired: 20170101T010101Z is now earlier than"));
    let r = a
        .handle(signed(
            "ListTables",
            "{}",
            "alice",
            "s3cret",
            "30200101T010101Z",
        ))
        .await;
    assert!(
        message(&r).starts_with("Signature not yet current: 30200101T010101Z is still later than")
    );
    // A changed body breaks the signature.
    let mut r = signed("ListTables", "{}", "alice", "s3cret", &now);
    r.body = Bytes::from_static(b"{ }");
    assert_eq!(
        error_type(&a.handle(r).await),
        "UnrecognizedClientException"
    );
    // A changed signed header too, while unsigned headers don't matter.
    let mut r = signed("ListTables", "{}", "alice", "s3cret", &now);
    r.headers.push(("X-Other".into(), "anything".into()));
    assert_eq!(a.handle(r).await.status, 200);
    let mut r = signed("ListTables", "{}", "alice", "s3cret", &now);
    r.headers.retain(|(k, _)| k != "Host");
    r.headers.push(("Host".into(), "evil:1".into()));
    assert_eq!(
        error_type(&a.handle(r).await),
        "UnrecognizedClientException"
    );
    // Malformed Authorization headers.
    for h in [
        "AWS4-HMAC-SHA1 Credential=a/b/c/d/aws4_request",
        "AWS4-HMAC-SHA256 Credential=alice/x/aws4_request, SignedHeaders=host, Signature=00",
        "AWS4-HMAC-SHA256",
    ] {
        let mut r = api("ListTables", "{}");
        r.headers.push(("X-Amz-Date".into(), now.clone()));
        r.headers.push(("Authorization".into(), h.into()));
        assert_eq!(
            error_type(&a.handle(r).await),
            "InvalidSignatureException",
            "{h}"
        );
    }
    // The health check needs no signature.
    assert_eq!(a.handle(req("GET", "/", &[], "")).await.status, 200);
}

async fn alt_on(kind: common::Kind) -> Alternator {
    Alternator::new(common::backend(kind).await, Config::default())
}

#[tokio::test]
async fn streams_api() {
    streams_api_on(common::Kind::Memory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn streams_api_engine() {
    streams_api_on(common::Kind::Engine).await;
}

async fn streams_api_on(kind: common::Kind) {
    let a = alt_on(kind).await;
    let (st, v) = call(
        &a,
        "CreateTable",
        json!({"TableName":"str","KeySchema":[{"AttributeName":"p","KeyType":"HASH"},{"AttributeName":"c","KeyType":"RANGE"}],
               "AttributeDefinitions":[{"AttributeName":"p","AttributeType":"S"},{"AttributeName":"c","AttributeType":"N"}],
               "BillingMode":"PAY_PER_REQUEST","Tags":[{"Key":"system:initial_tablets","Value":"1"}],
               "StreamSpecification":{"StreamEnabled":true,"StreamViewType":"NEW_AND_OLD_IMAGES"}}),
    )
    .await;
    assert_eq!(st, 200);
    let arn = v["TableDescription"]["LatestStreamArn"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(arn.starts_with(
        "arn:aws:dynamodb:us-east-1:000000000000:table/alternator_str@str_scylla_cdc_log/stream/"
    ));
    let (_, v) = call(&a, "ListStreams", json!({"TableName":"str"})).await;
    assert_eq!(v["Streams"][0]["StreamArn"], arn.as_str());
    let (_, v) = call(&a, "DescribeStream", json!({"StreamArn": arn})).await;
    let d = &v["StreamDescription"];
    assert_eq!(d["StreamStatus"], "ENABLED");
    assert_eq!(d["StreamViewType"], "NEW_AND_OLD_IMAGES");
    let shard = d["Shards"][0]["ShardId"].as_str().unwrap().to_string();
    assert!(shard.starts_with("H "));
    let (_, v) = call(
        &a,
        "GetShardIterator",
        json!({"StreamArn": arn, "ShardId": shard, "ShardIteratorType":"TRIM_HORIZON"}),
    )
    .await;
    let it = v["ShardIterator"].as_str().unwrap().to_string();

    call(
        &a,
        "PutItem",
        json!({"TableName":"str","Item":{"p":{"S":"a"},"c":{"N":"1"},"x":{"N":"1.50"}}}),
    )
    .await;
    call(&a, "UpdateItem", json!({"TableName":"str","Key":{"p":{"S":"a"},"c":{"N":"1"}},"UpdateExpression":"SET y = :v","ExpressionAttributeValues":{":v":{"S":"two"}}})).await;
    call(&a, "PutItem", json!({"TableName":"str","Item":{"p":{"S":"a"},"c":{"N":"1"},"x":{"N":"1.5"},"y":{"S":"two"}}})).await; // no-op
    call(
        &a,
        "DeleteItem",
        json!({"TableName":"str","Key":{"p":{"S":"a"},"c":{"N":"1"}}}),
    )
    .await;
    call(
        &a,
        "DeleteItem",
        json!({"TableName":"str","Key":{"p":{"S":"zz"},"c":{"N":"1"}}}),
    )
    .await; // no-op

    // Records show up only after the streams time window.
    let (_, v) = call(&a, "GetRecords", json!({"ShardIterator": it})).await;
    assert_eq!(v["Records"].as_array().unwrap().len(), 0);
    assert!(v["NextShardIterator"].is_string());
    a.advance_clock(11_000);
    let (_, v) = call(&a, "GetRecords", json!({"ShardIterator": it})).await;
    let recs = v["Records"].as_array().unwrap();
    let names: Vec<_> = recs
        .iter()
        .map(|r| r["eventName"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["INSERT", "MODIFY", "REMOVE"]);
    assert_eq!(recs[0]["eventSource"], "scylladb:alternator");
    assert_eq!(recs[0]["eventVersion"], "1.1");
    assert_eq!(recs[0]["awsRegion"], "datacenter1");
    assert_eq!(recs[0]["dynamodb"]["NewImage"]["x"], json!({"N":"1.5"}));
    assert!(recs[0]["dynamodb"].get("OldImage").is_none());
    assert_eq!(recs[1]["dynamodb"]["OldImage"]["x"], json!({"N":"1.5"}));
    assert_eq!(recs[1]["dynamodb"]["NewImage"]["y"], json!({"S":"two"}));
    assert_eq!(
        recs[2]["dynamodb"]["Keys"],
        json!({"p":{"S":"a"},"c":{"N":"1"}})
    );
    let last_seq = recs[2]["dynamodb"]["SequenceNumber"]
        .as_str()
        .unwrap()
        .to_string();
    let next = v["NextShardIterator"].as_str().unwrap().to_string();
    let (_, v) = call(&a, "GetRecords", json!({"ShardIterator": next})).await;
    assert_eq!(v["Records"].as_array().unwrap().len(), 0);
    let (_, v) = call(&a, "GetRecords", json!({"ShardIterator": it, "Limit": 1})).await;
    assert_eq!(v["Records"].as_array().unwrap().len(), 1);
    let (_, v) = call(&a, "GetShardIterator", json!({"StreamArn": arn, "ShardId": shard, "ShardIteratorType":"AT_SEQUENCE_NUMBER","SequenceNumber": last_seq})).await;
    let (_, r) = call(
        &a,
        "GetRecords",
        json!({"ShardIterator": v["ShardIterator"]}),
    )
    .await;
    assert_eq!(r["Records"][0]["eventName"], "REMOVE");
    let (_, v) = call(&a, "GetShardIterator", json!({"StreamArn": arn, "ShardId": shard, "ShardIteratorType":"AFTER_SEQUENCE_NUMBER","SequenceNumber": last_seq})).await;
    let (_, r) = call(
        &a,
        "GetRecords",
        json!({"ShardIterator": v["ShardIterator"]}),
    )
    .await;
    assert_eq!(r["Records"].as_array().unwrap().len(), 0);
    let (_, v) = call(
        &a,
        "GetShardIterator",
        json!({"StreamArn": arn, "ShardId": shard, "ShardIteratorType":"LATEST"}),
    )
    .await;
    let (_, r) = call(
        &a,
        "GetRecords",
        json!({"ShardIterator": v["ShardIterator"]}),
    )
    .await;
    assert_eq!(r["Records"].as_array().unwrap().len(), 0);

    // Errors.
    let (_, v) = call(&a, "GetRecords", json!({"ShardIterator": "bogus"})).await;
    assert_eq!(v["message"], "Malformed value ShardIterator : \"bogus\"");
    let (_, v) = call(
        &a,
        "GetRecords",
        json!({"ShardIterator": it, "Limit": 1001}),
    )
    .await;
    assert_eq!(v["message"], "Limit must be less than or equal to 1000");
    let (_, v) = call(
        &a,
        "GetShardIterator",
        json!({"StreamArn": arn, "ShardId": shard, "ShardIteratorType":"AT_SEQUENCE_NUMBER"}),
    )
    .await;
    assert_eq!(
        v["message"],
        "Missing required parameter \"SequenceNumber\""
    );
    let (_, v) = call(&a, "DescribeStream", json!({"StreamArn": "bogus"})).await;
    assert_eq!(v["message"], "Malformed value StreamArn : \"bogus\"");

    // Disabling closes the shard; the stream stays readable.
    call(
        &a,
        "UpdateTable",
        json!({"TableName":"str","StreamSpecification":{"StreamEnabled":false}}),
    )
    .await;
    let (_, v) = call(&a, "DescribeStream", json!({"StreamArn": arn})).await;
    assert_eq!(v["StreamDescription"]["StreamStatus"], "DISABLED");
    assert!(
        v["StreamDescription"]["Shards"][0]["SequenceNumberRange"]["EndingSequenceNumber"]
            .is_string()
    );
    let (_, v) = call(&a, "GetRecords", json!({"ShardIterator": next})).await;
    assert!(v.get("NextShardIterator").is_none());
}

#[tokio::test]
async fn keys_only_streams_record_every_write() {
    keys_only_on(common::Kind::Memory).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn keys_only_streams_record_every_write_engine() {
    keys_only_on(common::Kind::Engine).await;
}

async fn keys_only_on(kind: common::Kind) {
    let a = alt_on(kind).await;
    call(&a, "CreateTable", json!({"TableName":"sko","KeySchema":[{"AttributeName":"p","KeyType":"HASH"}],"AttributeDefinitions":[{"AttributeName":"p","AttributeType":"S"}],"BillingMode":"PAY_PER_REQUEST","Tags":[{"Key":"system:initial_tablets","Value":"1"}],"StreamSpecification":{"StreamEnabled":true,"StreamViewType":"KEYS_ONLY"}})).await;
    let (_, v) = call(&a, "ListStreams", json!({"TableName":"sko"})).await;
    let arn = v["Streams"][0]["StreamArn"].clone();
    let (_, d) = call(&a, "DescribeStream", json!({"StreamArn": arn})).await;
    let shard = d["StreamDescription"]["Shards"][0]["ShardId"].clone();
    call(
        &a,
        "PutItem",
        json!({"TableName":"sko","Item":{"p":{"S":"a"}}}),
    )
    .await;
    call(
        &a,
        "PutItem",
        json!({"TableName":"sko","Item":{"p":{"S":"a"}}}),
    )
    .await;
    call(
        &a,
        "UpdateItem",
        json!({"TableName":"sko","Key":{"p":{"S":"b"}},"UpdateExpression":"REMOVE x"}),
    )
    .await;
    call(
        &a,
        "DeleteItem",
        json!({"TableName":"sko","Key":{"p":{"S":"missing"}}}),
    )
    .await;
    a.advance_clock(11_000);
    let (_, it) = call(
        &a,
        "GetShardIterator",
        json!({"StreamArn": arn, "ShardId": shard, "ShardIteratorType":"TRIM_HORIZON"}),
    )
    .await;
    let (_, v) = call(
        &a,
        "GetRecords",
        json!({"ShardIterator": it["ShardIterator"]}),
    )
    .await;
    let names: Vec<_> = v["Records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["eventName"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(names, ["INSERT", "INSERT", "MODIFY", "REMOVE"]);
    // A REMOVE-only update of a missing item doesn't create it.
    let (_, g) = call(
        &a,
        "GetItem",
        json!({"TableName":"sko","Key":{"p":{"S":"b"}}}),
    )
    .await;
    assert!(g.get("Item").is_none());
}
