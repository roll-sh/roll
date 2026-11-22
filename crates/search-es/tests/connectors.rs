//! Connectors that sync, against real servers (`scripts/search-connectors-e2e.sh` starts
//! Postgres and MinIO and sets `ROLL_E2E_CONNECTORS_PG` / `ROLL_E2E_CONNECTORS_S3`; without them
//! these tests skip): full and incremental PostgreSQL syncs, deletes found by a full sync, an S3
//! bucket's objects and their text, cancel, the job's counters, and no credential in what the
//! API returns or records. The tests against a fake Postgres (where a credential is sent, a
//! silent server) need no Docker and always run.

use search_es::json::{Value, parse};
use search_es::{Cluster, ClusterConfig, EsRequest, MemoryStore};
use std::sync::Arc;
use std::time::Duration;

const PG_PASSWORD: &str = "pg-secret-7731";
const S3_KEY: &str = "rollminio";
const S3_SECRET: &str = "s3-secret-5523";

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn slots() -> Arc<search_es::slots::Slots> {
    search_es::slots::Slots::new(search_es::xpack::entsearch::sync::SLOTS)
}

async fn cluster(settings: &[(&str, &str)]) -> Arc<Cluster> {
    let mut cfg = ClusterConfig::new("connectors");
    // Its own slots: the tests' clusters share a name, and the node's pool keys on it.
    cfg.connector_slots = Some(slots());
    cfg.sealer = search_es::seal::Sealer::from_secret("0123456789abcdef0123456789abcdef-connectors");
    cfg.node_settings.push(("roll.connectors.allow_private_hosts".into(), "true".into()));
    cfg.node_settings.push(("roll.connectors.max_docs_per_second".into(), "100000".into()));
    cfg.node_settings.push(("roll.connectors.max_bytes_per_second".into(), (1u64 << 30).to_string()));
    for (k, v) in settings {
        cfg.node_settings.push((k.to_string(), v.to_string()));
    }
    let c = Arc::new(Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap());
    c.enable_background_tasks();
    c
}

/// The response's status, parsed body and raw text.
async fn call(c: &Cluster, method: &str, uri: &str, body: Option<&str>) -> (u16, Value, String) {
    let mut r = EsRequest::new(method, uri);
    if let Some(b) = body {
        r.headers.push(("content-type".into(), "application/json".into()));
        r.body = b.to_string().into();
    }
    let resp = search_es::handle(c, r).await;
    let text = String::from_utf8_lossy(&resp.body).into_owned();
    (resp.status, parse(&resp.body).ok().flatten().unwrap_or(Value::Null), text)
}

fn at<'a>(v: &'a Value, path: &str) -> &'a Value {
    let mut cur = v;
    for p in path.split('.') {
        cur = match cur {
            Value::Arr(a) => p.parse::<usize>().ok().and_then(|i| a.get(i)).unwrap_or(&Value::Null),
            other => other.get(p).unwrap_or(&Value::Null),
        };
    }
    cur
}

fn num(v: &Value, path: &str) -> i64 {
    at(v, path).as_i64().unwrap_or(-1)
}

/// Starts a sync job of `connector` and returns its id.
async fn start(c: &Cluster, connector: &str, job_type: &str) -> String {
    let (s, v, t) = call(c, "POST", "/_connector/_sync_job", Some(&format!(r#"{{"id":"{connector}","job_type":"{job_type}"}}"#))).await;
    assert_eq!(s, 201, "{t}");
    at(&v, "id").as_str().unwrap().to_string()
}

/// Waits for job `id` to reach a final status; the job.
async fn finished(c: &Cluster, id: &str) -> Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(90);
    loop {
        let (_, v, _) = call(c, "GET", &format!("/_connector/_sync_job/{id}"), None).await;
        if matches!(at(&v, "status").as_str(), Some("completed" | "error" | "canceled")) {
            return v;
        }
        assert!(std::time::Instant::now() < deadline, "job {id} didn't finish: {v:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Everything a client can read about connectors and jobs, and what the cluster stores.
async fn visible(c: &Cluster) -> String {
    let mut out = String::new();
    for uri in ["/_connector", "/_connector/_sync_job", "/_connector/pg", "/_connector/files"] {
        out.push_str(&call(c, "GET", uri, None).await.2);
    }
    out.push_str(&format!("{:?}", c.state().metadata.xpack));
    out
}

async fn pg_client(addr: &str) -> tokio_postgres::Client {
    let (host, port) = addr.split_once(':').unwrap();
    let mut cfg = tokio_postgres::Config::new();
    cfg.host(host).port(port.parse().unwrap()).user("roll").password(PG_PASSWORD).dbname("shop");
    let (client, conn) = cfg.connect(tokio_postgres::NoTls).await.unwrap();
    tokio::spawn(conn);
    client
}

async fn pg_connector(c: &Cluster, addr: &str, id: &str, index: &str, tables: &str, password: &str) {
    let (host, port) = addr.split_once(':').unwrap();
    let (s, _, t) = call(c, "PUT", &format!("/_connector/{id}"), Some(&format!(r#"{{"index_name":"{index}","is_native":true,"service_type":"postgresql","name":"shop"}}"#))).await;
    assert_eq!(s, 201, "{t}");
    let (_, v, _) = call(c, "GET", &format!("/_connector/{id}"), None).await;
    assert_eq!(at(&v, "status").as_str(), Some("needs_configuration"), "roll fills the schema: {v:?}");
    assert_eq!(at(&v, "configuration.password.sensitive"), &Value::Bool(true));
    let values = format!(
        r#"{{"values":{{"host":"{host}","port":{port},"username":"roll","password":"{password}","database":"shop","tables":"{tables}","ssl_enabled":false,"incremental_column":"updated_at"}}}}"#
    );
    let (s, _, t) = call(c, "PUT", &format!("/_connector/{id}/_configuration"), Some(&values)).await;
    assert_eq!(s, 200, "{t}");
}

#[tokio::test]
async fn postgres_full_incremental_and_deletes() {
    let Some(addr) = env("ROLL_E2E_CONNECTORS_PG") else {
        eprintln!("skipped: ROLL_E2E_CONNECTORS_PG isn't set (scripts/search-connectors-e2e.sh)");
        return;
    };
    let db = pg_client(&addr).await;
    db.batch_execute(
        "DROP TABLE IF EXISTS items, notes, big; \
         CREATE TABLE items (id int PRIMARY KEY, name text, price int, updated_at timestamptz NOT NULL); \
         INSERT INTO items VALUES (1, 'apple', 3, '2026-01-01Z'), (2, 'pear', 4, '2026-01-02Z'), (3, 'plum', 5, '2026-01-03Z'); \
         CREATE TABLE notes (body text);",
    )
    .await
    .unwrap();
    let c = cluster(&[]).await;
    pg_connector(&c, &addr, "pg", "content-shop", "*", PG_PASSWORD).await;
    let (_, v, _) = call(&c, "GET", "/_connector/pg", None).await;
    assert_eq!(at(&v, "configuration.password.value").as_str(), Some("********"), "{v:?}");
    assert_eq!(at(&v, "status").as_str(), Some("configured"));

    // A full sync indexes the table (`notes` has no primary key: skipped, as Elastic does).
    let job = finished(&c, &start(&c, "pg", "full").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("completed"), "{job:?}");
    assert_eq!((num(&job, "indexed_document_count"), num(&job, "deleted_document_count"), num(&job, "total_document_count")), (3, 0, 3));
    assert_eq!(at(&job, "worker_hostname").as_str(), Some("roll"));
    assert_eq!(at(&job, "metadata.skipped_tables.0").as_str(), Some("notes"));
    for k in ["started_at", "completed_at", "last_seen"] {
        assert!(at(&job, k).as_str().is_some(), "{k}: {job:?}");
    }
    let (_, doc, _) = call(&c, "GET", "/content-shop/_doc/shop_public_items_2", None).await;
    assert_eq!(at(&doc, "_source.public_items_name").as_str(), Some("pear"), "{doc:?}");
    assert_eq!(at(&doc, "_source.table").as_str(), Some("items"));
    let (_, conn, _) = call(&c, "GET", "/_connector/pg", None).await;
    assert_eq!((at(&conn, "status").as_str(), at(&conn, "last_sync_status").as_str()), (Some("connected"), Some("completed")));
    assert_eq!(num(&conn, "last_indexed_document_count"), 3);

    // An incremental sync reads rows at or after the last sync's newest `updated_at`.
    db.batch_execute("UPDATE items SET name = 'pear (ripe)', updated_at = '2026-02-01Z' WHERE id = 2; INSERT INTO items VALUES (4, 'fig', 9, '2026-02-02Z');").await.unwrap();
    let job = finished(&c, &start(&c, "pg", "incremental").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("completed"), "{job:?}");
    assert_eq!((num(&job, "indexed_document_count"), num(&job, "deleted_document_count"), num(&job, "total_document_count")), (3, 0, 4), "rows 3 (at the cursor), 2 and 4");
    let (_, doc, _) = call(&c, "GET", "/content-shop/_doc/shop_public_items_2", None).await;
    assert_eq!(at(&doc, "_source.public_items_name").as_str(), Some("pear (ripe)"));
    let (_, conn, _) = call(&c, "GET", "/_connector/pg", None).await;
    let cursor = at(&conn, "sync_cursor.tables").get("public.items").and_then(Value::as_str).unwrap_or_default().to_string();
    assert!(cursor.starts_with("2026-02-02"), "{conn:?}");

    // A deleted row disappears after a full sync (an incremental one doesn't delete).
    db.batch_execute("DELETE FROM items WHERE id = 1").await.unwrap();
    let job = finished(&c, &start(&c, "pg", "full").await).await;
    assert_eq!((num(&job, "indexed_document_count"), num(&job, "deleted_document_count"), num(&job, "total_document_count")), (3, 1, 3), "{job:?}");
    let (s, _, _) = call(&c, "GET", "/content-shop/_doc/shop_public_items_1", None).await;
    assert_eq!(s, 404);

    // Sync rules: excluded rows aren't indexed, and a full sync removes them.
    let rules = r#"{"rules":[{"id":"cheap","order":0,"field":"public_items_price","policy":"exclude","rule":"<","value":"5"},{"id":"DEFAULT","order":1,"field":"_","policy":"include","rule":"regex","value":".*"}]}"#;
    assert_eq!(call(&c, "PUT", "/_connector/pg/_filtering", Some(rules)).await.0, 200);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (_, v, _) = call(&c, "GET", "/_connector/pg", None).await;
        if at(&v, "filtering.0.draft.validation.state").as_str() == Some("valid") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "roll validates the draft: {v:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(call(&c, "PUT", "/_connector/pg/_filtering/_activate", None).await.0, 200);
    let job = finished(&c, &start(&c, "pg", "full").await).await;
    assert_eq!((num(&job, "indexed_document_count"), num(&job, "deleted_document_count"), num(&job, "total_document_count")), (2, 1, 2), "{job:?}");

    // A wrong password fails the job, and the error names neither password.
    let wrong = "wrong-pw-90210";
    assert_eq!(call(&c, "PUT", "/_connector/pg/_configuration", Some(&format!(r#"{{"values":{{"password":"{wrong}"}}}}"#))).await.0, 200);
    let job = finished(&c, &start(&c, "pg", "full").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("error"), "{job:?}");
    let (_, conn, _) = call(&c, "GET", "/_connector/pg", None).await;
    assert_eq!((at(&conn, "status").as_str(), at(&conn, "last_sync_status").as_str()), (Some("error"), Some("error")));
    // Writing the redacted placeholder back keeps the stored value.
    assert_eq!(call(&c, "PUT", "/_connector/pg/_configuration", Some(r#"{"values":{"password":"********"}}"#)).await.0, 200);
    let job = finished(&c, &start(&c, "pg", "full").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("error"), "still the wrong one");
    let seen = visible(&c).await;
    for secret in [PG_PASSWORD, wrong] {
        assert!(!seen.contains(secret), "{secret} is visible");
    }
}

#[tokio::test]
async fn cancel_stops_a_running_sync() {
    let Some(addr) = env("ROLL_E2E_CONNECTORS_PG") else {
        eprintln!("skipped: ROLL_E2E_CONNECTORS_PG isn't set");
        return;
    };
    let db = pg_client(&addr).await;
    db.batch_execute(
        "DROP TABLE IF EXISTS big; CREATE TABLE big (id int PRIMARY KEY, body text, updated_at timestamptz NOT NULL DEFAULT now()); \
         INSERT INTO big (id, body) SELECT g, repeat('x', 100) FROM generate_series(1, 5000) g;",
    )
    .await
    .unwrap();
    // 200 rows a second: the table takes 25 s.
    let c = cluster(&[("roll.connectors.max_docs_per_second", "200")]).await;
    pg_connector(&c, &addr, "pg", "content-big", "big", PG_PASSWORD).await;
    let id = start(&c, "pg", "full").await;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let (_, v, _) = call(&c, "GET", &format!("/_connector/_sync_job/{id}"), None).await;
        if at(&v, "status").as_str() == Some("in_progress") {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "{v:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (s, _, t) = call(&c, "PUT", &format!("/_connector/_sync_job/{id}/_cancel"), None).await;
    assert_eq!(s, 200, "{t}");
    let job = finished(&c, &id).await;
    assert_eq!(at(&job, "status").as_str(), Some("canceled"), "{job:?}");
    let indexed = num(&job, "indexed_document_count");
    assert!(indexed > 0 && indexed < 5000, "stopped part way: {indexed}");
    assert!(at(&job, "canceled_at").as_str().is_some());
    let (_, conn, _) = call(&c, "GET", "/_connector/pg", None).await;
    assert_eq!(at(&conn, "last_sync_status").as_str(), Some("canceled"));
}

#[tokio::test]
async fn s3_bucket_objects_and_text() {
    let Some(endpoint) = env("ROLL_E2E_CONNECTORS_S3") else {
        eprintln!("skipped: ROLL_E2E_CONNECTORS_S3 isn't set");
        return;
    };
    use search_es::s3::{Client, Credentials, Location};
    let at_ = Location { endpoint: reqwest::Url::parse(&endpoint).unwrap(), bucket: "docs".into(), region: "us-east-1".into(), path_style: true };
    let bucket = Client::new(at_, Some(Credentials { access_key: S3_KEY.into(), secret_key: S3_SECRET.into(), session_token: None }), &search_es::egress::Egress::of(|_| None, true).unwrap()).unwrap();
    for (key, body) in [
        ("notes/hello.txt", "hello from a text file"),
        ("data/item.json", r#"{"title":"blue kettle","tags":["kitchen"]}"#),
        ("site/index.html", "<html><head><script>var secret='no'</script></head><body><h1>Welcome &amp; enjoy</h1></body></html>"),
        ("bin/blob.bin", "\u{0}\u{1}binary"),
        ("old/gone.txt", "soon deleted"),
    ] {
        bucket.put(key, bytes::Bytes::from(body.to_string())).await.unwrap();
    }
    let c = cluster(&[]).await;
    let (s, _, t) = call(&c, "PUT", "/_connector/files", Some(r#"{"index_name":"content-files","is_native":true,"service_type":"s3"}"#)).await;
    assert_eq!(s, 201, "{t}");
    let values = format!(r#"{{"values":{{"buckets":"docs","endpoint":"{endpoint}","aws_access_key_id":"{S3_KEY}","aws_secret_access_key":"{S3_SECRET}"}}}}"#);
    assert_eq!(call(&c, "PUT", "/_connector/files/_configuration", Some(&values)).await.0, 200);

    let job = finished(&c, &start(&c, "files", "full").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("completed"), "{job:?}");
    assert_eq!((num(&job, "indexed_document_count"), num(&job, "total_document_count")), (5, 5), "{job:?}");
    let search = |q: &'static str| {
        let c = c.clone();
        async move {
            let (_, v, _) = call(&c, "POST", "/content-files/_search", Some(&format!(r#"{{"query":{{"match":{{"body":"{q}"}}}}}}"#))).await;
            at(&v, "hits.hits").as_array().map(|h| h.iter().filter_map(|x| at(x, "_source.filename").as_str().map(str::to_string)).collect::<Vec<_>>()).unwrap_or_default()
        }
    };
    assert_eq!(search("hello").await, ["notes/hello.txt"]);
    assert_eq!(search("kettle").await, ["data/item.json"]);
    assert_eq!(search("welcome").await, ["site/index.html"]);
    assert!(search("secret").await.is_empty(), "scripts aren't text");
    let (_, doc, _) = call(&c, "POST", "/content-files/_search", Some(r#"{"query":{"term":{"filename.keyword":"bin/blob.bin"}}}"#)).await;
    assert_eq!(num(&doc, "hits.hits.0._source.size_in_bytes"), 8, "{doc:?}");
    assert!(at(&doc, "hits.hits.0._source.body").is_null(), "no text from a binary");

    // An incremental sync reads only what changed; a full one finds what was deleted.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    bucket.put("notes/new.txt", bytes::Bytes::from("brand new")).await.unwrap();
    let job = finished(&c, &start(&c, "files", "incremental").await).await;
    // The new object, and those in the cursor's own instant again (read rather than risk missing
    // one written in that second).
    let n = num(&job, "indexed_document_count");
    assert!(at(&job, "status").as_str() == Some("completed") && (2..5).contains(&n), "{job:?}");
    let (_, v, _) = call(&c, "POST", "/content-files/_refresh", None).await;
    assert_eq!(at(&v, "_shards.failed"), &Value::from(0));
    let (_, v, _) = call(&c, "POST", "/content-files/_search", Some(r#"{"query":{"match":{"body":"brand"}}}"#)).await;
    assert_eq!(num(&v, "hits.total.value"), 1, "{v:?}");
    bucket.delete("old/gone.txt").await.unwrap();
    let job = finished(&c, &start(&c, "files", "full").await).await;
    assert_eq!((num(&job, "deleted_document_count"), num(&job, "total_document_count")), (1, 5), "{job:?}");
    assert!(!visible(&c).await.contains(S3_SECRET), "the secret key is visible");

    // A 9 MB JSON array is scanned for its values, never made a tree, and the object and its
    // text are charged to the request breaker: under a 4 MB limit the sync fails.
    bucket.put("big/zeros.json", bytes::Bytes::from(format!("[{}0]", "0,".repeat(4_700_000)))).await.unwrap();
    let (s, _, t) = call(&c, "PUT", "/_cluster/settings", Some(r#"{"persistent":{"indices.breaker.request.limit":"4mb"}}"#)).await;
    assert_eq!(s, 200, "{t}");
    let job = finished(&c, &start(&c, "files", "full").await).await;
    assert!(at(&job, "error").as_str().is_some_and(|e| e.contains("Data too large")), "{job:?}");
    call(&c, "PUT", "/_cluster/settings", Some(r#"{"persistent":{"indices.breaker.request.limit":null}}"#)).await;
    bucket.delete("big/zeros.json").await.unwrap();

    // Active advanced rules naming a bucket outside [buckets] (set directly, past the draft's
    // validation) fail the sync rather than reading another bucket with the stored key.
    let filtering = r#"{"filtering":[{"domain":"DEFAULT","active":{"advanced_snippet":{"value":[{"bucket":"payroll"}]},"rules":[{"id":"DEFAULT","order":0,"field":"_","policy":"include","rule":"regex","value":".*"}],"validation":{"state":"valid","errors":[]}},"draft":{"advanced_snippet":{"value":{}},"rules":[{"id":"DEFAULT","order":0,"field":"_","policy":"include","rule":"regex","value":".*"}],"validation":{"state":"valid","errors":[]}}}]}"#;
    let (s, _, t) = call(&c, "PUT", "/_connector/files/_filtering", Some(filtering)).await;
    assert_eq!(s, 200, "{t}");
    let job = finished(&c, &start(&c, "files", "full").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("error"), "{job:?}");
    assert!(at(&job, "error").as_str().is_some_and(|e| e.contains("payroll")), "{job:?}");
}

#[tokio::test]
async fn postgres_pages_are_bounded_by_bytes() {
    let Some(addr) = env("ROLL_E2E_CONNECTORS_PG") else {
        eprintln!("skipped: ROLL_E2E_CONNECTORS_PG isn't set");
        return;
    };
    let db = pg_client(&addr).await;
    // 40 rows of 1.5 MiB under a two-column key, and one over the 10 MiB limit: pages are cut by
    // their bytes, and the large row is skipped and counted.
    db.batch_execute(
        "DROP TABLE IF EXISTS wide; CREATE TABLE wide (region text, id int, body text, updated_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (region, id)); \
         INSERT INTO wide (region, id, body) SELECT CASE WHEN g % 2 = 0 THEN 'eu' ELSE 'us' END, g, repeat(md5(g::text), 49152) FROM generate_series(1, 40) g; \
         INSERT INTO wide (region, id, body) VALUES ('eu', 1000, repeat('y', 11 << 20));",
    )
    .await
    .unwrap();
    let c = cluster(&[]).await;
    pg_connector(&c, &addr, "pg", "content-wide", "wide", PG_PASSWORD).await;
    let job = finished(&c, &start(&c, "pg", "full").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("completed"), "{job:?}");
    assert_eq!((num(&job, "indexed_document_count"), num(&job, "metadata.skipped_document_count"), num(&job, "total_document_count")), (40, 1, 40), "{job:?}");
    db.batch_execute("DROP TABLE wide").await.unwrap();

    // A row of a million zeros (2 MB of text) is never parsed into a tree: its page costs its
    // bytes, so it syncs under a 4 MB request breaker (a tree, or three times the text, wouldn't
    // fit). A pretty-printed `json` column keeps its line breaks through the server's JSON, and
    // still syncs as one document.
    db.batch_execute(
        "DROP TABLE IF EXISTS zeros; CREATE TABLE zeros (id int PRIMARY KEY, z int[], pretty json); \
         INSERT INTO zeros VALUES (1, array_fill(0, ARRAY[1000000]), E'{\\n  \"a\": [1,\\n    2],\\n  \"b\": \"x\"\\n}');",
    )
    .await
    .unwrap();
    let (s, _, t) = call(&c, "PUT", "/content-zeros", Some(r#"{"mappings":{"dynamic":false}}"#)).await;
    assert_eq!(s, 200, "{t}");
    let (s, _, t) = call(&c, "PUT", "/_cluster/settings", Some(r#"{"persistent":{"indices.breaker.request.limit":"4mb"}}"#)).await;
    assert_eq!(s, 200, "{t}");
    pg_connector(&c, &addr, "zeros", "content-zeros", "zeros", PG_PASSWORD).await;
    let job = finished(&c, &start(&c, "zeros", "full").await).await;
    assert_eq!((at(&job, "status").as_str(), num(&job, "indexed_document_count")), (Some("completed"), 1), "{job:?}");
    call(&c, "POST", "/content-zeros/_refresh", None).await;
    let (_, v, _) = call(&c, "POST", "/content-zeros/_search", Some(r#"{"_source":["public_zeros_pretty"]}"#)).await;
    assert_eq!(at(&v, "hits.hits.0._source.public_zeros_pretty.a.1"), &Value::from(2), "{v:?}");
    db.batch_execute("DROP TABLE zeros").await.unwrap();
}

/// How a fake Postgres behaves.
#[derive(Clone, Copy, PartialEq)]
enum Fake {
    /// Asks for a cleartext password, records what it gets, refuses it.
    Password,
    /// Accepts connections and never answers.
    Silent,
    /// Answers the startup with a message header claiming almost 4 GiB.
    HugeHeader,
}

/// A fake Postgres server (see [`Fake`]) and the passwords it was sent.
async fn fake_postgres(mode: Fake) -> (u16, Arc<std::sync::Mutex<Vec<String>>>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let got = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = got.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut s, _)) = listener.accept().await else { return };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut held = Vec::new();
                if mode == Fake::Silent {
                    let _ = s.read_to_end(&mut held).await;
                    return;
                }
                // The startup message (no SSLRequest: the connectors here don't use TLS).
                let Ok(len) = s.read_i32().await else { return };
                let mut rest = vec![0; len as usize - 4];
                if s.read_exact(&mut rest).await.is_err() {
                    return;
                }
                if mode == Fake::HugeHeader {
                    let _ = s.write_all(&[b'D', 0xFF, 0xFF, 0xFF, 0xF0]).await;
                    let _ = s.read_to_end(&mut held).await;
                    return;
                }
                let _ = s.write_all(&[b'R', 0, 0, 0, 8, 0, 0, 0, 3]).await;
                let (Ok(kind), Ok(len)) = (s.read_u8().await, s.read_i32().await) else { return };
                let mut body = vec![0; len as usize - 4];
                if kind == b'p' && s.read_exact(&mut body).await.is_ok() {
                    seen.lock().unwrap().push(String::from_utf8_lossy(&body).trim_end_matches('\0').to_string());
                }
                let fields = b"SFATAL\0C28P01\0Mpassword authentication failed\0\0";
                let mut e = vec![b'E'];
                e.extend_from_slice(&((fields.len() + 4) as i32).to_be_bytes());
                e.extend_from_slice(fields);
                let _ = s.write_all(&e).await;
            });
        }
    });
    (port, got)
}

/// Pointing a connector somewhere else without giving the password again clears it: the new host
/// never receives the stored one.
#[tokio::test]
async fn a_retargeted_connector_never_sends_the_stored_password() {
    let (first, first_got) = fake_postgres(Fake::Password).await;
    let (second, second_got) = fake_postgres(Fake::Password).await;
    let c = cluster(&[]).await;
    pg_connector(&c, &format!("127.0.0.1:{first}"), "pg", "content-rt", "*", "pw-retarget-41").await;
    let job = finished(&c, &start(&c, "pg", "full").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("error"), "{job:?}");
    assert_eq!(*first_got.lock().unwrap(), ["pw-retarget-41"], "the host it was configured with gets it");

    let (s, _, t) = call(&c, "PUT", "/_connector/pg/_configuration", Some(&format!(r#"{{"values":{{"port":{second}}}}}"#))).await;
    assert_eq!(s, 200, "{t}");
    let (_, v, _) = call(&c, "GET", "/_connector/pg", None).await;
    assert_eq!((at(&v, "status").as_str(), at(&v, "configuration.password.value").as_str()), (Some("needs_configuration"), Some("")), "{v:?}");
    let job = finished(&c, &start(&c, "pg", "full").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("error"), "{job:?}");
    assert!(second_got.lock().unwrap().is_empty(), "the new host got a password: {:?}", second_got.lock().unwrap());

    // Giving it again with the new target works.
    let body = format!(r#"{{"values":{{"port":{second},"password":"pw-retarget-42"}}}}"#);
    assert_eq!(call(&c, "PUT", "/_connector/pg/_configuration", Some(&body)).await.0, 200);
    finished(&c, &start(&c, "pg", "full").await).await;
    assert_eq!(*second_got.lock().unwrap(), ["pw-retarget-42"]);

    // A detour through another service doesn't carry the password to a new host: switching
    // starts from the service's own schema, and `host` isn't an s3 field to set aside.
    let (third, third_got) = fake_postgres(Fake::Password).await;
    assert_eq!(call(&c, "PUT", "/_connector/pg/_service_type", Some(r#"{"service_type":"s3"}"#)).await.0, 200);
    let (s, _, t) = call(&c, "PUT", "/_connector/pg/_configuration", Some(&format!(r#"{{"values":{{"host":"127.0.0.1","port":{third}}}}}"#))).await;
    assert_eq!(s, 400, "an s3 connector has no host: {t}");
    assert_eq!(call(&c, "PUT", "/_connector/pg/_service_type", Some(r#"{"service_type":"postgresql"}"#)).await.0, 200);
    let (_, v, _) = call(&c, "GET", "/_connector/pg", None).await;
    assert_eq!((at(&v, "status").as_str(), at(&v, "configuration.password.value").as_str()), (Some("needs_configuration"), Some("")), "{v:?}");
    let body = format!(r#"{{"values":{{"host":"127.0.0.1","port":{third},"username":"roll","database":"shop","ssl_enabled":false}}}}"#);
    assert_eq!(call(&c, "PUT", "/_connector/pg/_configuration", Some(&body)).await.0, 200);
    let job = finished(&c, &start(&c, "pg", "full").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("error"), "{job:?}");
    assert!(third_got.lock().unwrap().is_empty(), "the detour's host got a password: {:?}", third_got.lock().unwrap());
}

/// A server whose message header claims a huge length fails the sync at the header, before the
/// client library reserves that much memory.
#[tokio::test]
async fn a_huge_message_header_fails_the_sync() {
    let (port, _) = fake_postgres(Fake::HugeHeader).await;
    let c = cluster(&[]).await;
    pg_connector(&c, &format!("127.0.0.1:{port}"), "pg", "content-huge", "*", "pw-huge-1").await;
    let started = std::time::Instant::now();
    let job = finished(&c, &start(&c, "pg", "full").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("error"), "{job:?}");
    assert!(at(&job, "error").as_str().is_some_and(|e| e.contains("4294967280 bytes")), "{job:?}");
    assert!(started.elapsed() < Duration::from_secs(10), "took {:?}", started.elapsed());
}

/// The node's egress rules cover connectors as they cover reindex from remote: a host the node
/// denies (its own addresses, the operator's networks) is refused with reindex's refusal, written
/// out or behind a name, even where private hosts are allowed, and nothing connects to it.
#[tokio::test]
async fn a_denied_address_is_refused_as_reindex_refuses_it() {
    let (port, got) = fake_postgres(Fake::Password).await;
    let c = cluster(&[("roll.egress.deny_addresses", "127.0.0.0/8,::1")]).await;
    for (id, host) in [("pg", "127.0.0.1"), ("pg-named", "localhost.")] {
        pg_connector(&c, &format!("{host}:{port}"), id, &format!("content-{id}"), "*", "pw-denied-1").await;
        let job = finished(&c, &start(&c, id, "full").await).await;
        assert_eq!(at(&job, "status").as_str(), Some("error"), "{job:?}");
        assert!(at(&job, "error").as_str().is_some_and(|e| e.contains(&format!("[{host}] is an address roll may not reach"))), "{host}: {job:?}");
    }
    assert!(got.lock().unwrap().is_empty(), "a denied address was reached");
}

/// A sealed value copied into a self-managed connector isn't shown, and once the connector is
/// run by roll it's only text: the sync sends the ciphertext, never what it seals.
#[tokio::test]
async fn a_planted_sealed_value_opens_to_nothing() {
    let (victim_port, _) = fake_postgres(Fake::Password).await;
    let (port, got) = fake_postgres(Fake::Password).await;
    let c = cluster(&[]).await;
    pg_connector(&c, &format!("127.0.0.1:{victim_port}"), "victim", "content-victim", "*", "victim-pw-77").await;
    let stolen = at(c.state().metadata.xpack.get("connector", "victim").unwrap(), "configuration.password.value").as_str().unwrap().to_string();
    assert!(stolen.starts_with("::roll_sealed::") && !stolen.contains("victim-pw-77"));

    let (s, _, t) = call(&c, "PUT", "/_connector/planted", Some(r#"{"index_name":"content-planted","service_type":"postgresql"}"#)).await;
    assert_eq!(s, 201, "{t}");
    let configuration = format!(
        r#"{{"configuration":{{"host":{{"label":"Host","value":"127.0.0.1"}},"port":{{"label":"Port","value":{port}}},"username":{{"label":"Username","value":"roll"}},"database":{{"label":"Database","value":"shop"}},"ssl_enabled":{{"label":"SSL","value":false}},"password":{{"label":"Password","sensitive":true,"value":"{stolen}"}}}}}}"#
    );
    let (s, _, t) = call(&c, "PUT", "/_connector/planted/_configuration", Some(&configuration)).await;
    assert_eq!(s, 200, "{t}");
    let (_, v, _) = call(&c, "GET", "/_connector/planted", None).await;
    assert_eq!(at(&v, "configuration.password.value").as_str(), Some("********"), "a sealed value is never shown: {v:?}");

    // Once roll runs it, a sealed box a client wrote is refused, not opened: the connector needs
    // its password again, and nothing is sent.
    assert_eq!(call(&c, "PUT", "/_connector/planted/_native", Some(r#"{"is_native":true}"#)).await.0, 200);
    let (_, v, _) = call(&c, "GET", "/_connector/planted", None).await;
    assert_eq!((at(&v, "status").as_str(), at(&v, "configuration.password.value").as_str()), (Some("needs_configuration"), Some("")), "{v:?}");
    let job = finished(&c, &start(&c, "planted", "full").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("error"), "{job:?}");
    assert!(got.lock().unwrap().is_empty(), "something was sent: {:?}", got.lock().unwrap());
}

/// A detour through self-managed (where roll doesn't guard the configuration) and back, or a
/// sealed value moved from a non-sensitive entry to the sensitive one, never sends the stored
/// password to a host it wasn't paired with.
#[tokio::test]
async fn a_detour_through_self_managed_never_sends_the_password_elsewhere() {
    let (home, home_got) = fake_postgres(Fake::Password).await;
    let (evil, evil_got) = fake_postgres(Fake::Password).await;
    let c = cluster(&[]).await;
    pg_connector(&c, &format!("127.0.0.1:{home}"), "x", "content-x", "*", "pw-paired-7").await;
    let sealed = at(c.state().metadata.xpack.get("connector", "x").unwrap(), "configuration.password.value").as_str().unwrap().to_string();
    // (a) Made self-managed, pointed elsewhere, made native again.
    assert_eq!(call(&c, "PUT", "/_connector/x/_native", Some(r#"{"is_native":false}"#)).await.0, 200);
    assert_eq!(call(&c, "PUT", "/_connector/x/_configuration", Some(&format!(r#"{{"values":{{"port":{evil}}}}}"#))).await.0, 200);
    assert_eq!(call(&c, "PUT", "/_connector/x/_native", Some(r#"{"is_native":true}"#)).await.0, 200);
    // (b) The sealed value written back under a non-sensitive entry, then (c) the entry made
    // sensitive with the same value.
    let replace = |sensitive: bool| {
        format!(
            r#"{{"configuration":{{"host":{{"label":"Host","value":"127.0.0.1"}},"port":{{"label":"Port","value":{evil}}},"username":{{"label":"Username","value":"roll"}},"database":{{"label":"Database","value":"shop"}},"ssl_enabled":{{"label":"SSL","value":false}},"password":{{"label":"Password","sensitive":{sensitive},"value":"{sealed}"}}}}}}"#
        )
    };
    assert_eq!(call(&c, "PUT", "/_connector/x/_configuration", Some(&replace(false))).await.0, 200);
    assert_eq!(call(&c, "PUT", "/_connector/x/_configuration", Some(&replace(true))).await.0, 200);
    // (d) The sync reaches the new host without the paired password.
    let job = finished(&c, &start(&c, "x", "full").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("error"), "{job:?}");
    assert!(home_got.lock().unwrap().is_empty());
    assert!(!evil_got.lock().unwrap().iter().any(|p| p == "pw-paired-7"), "the paired password reached another host: {:?}", evil_got.lock().unwrap());
}

/// A cluster whose queued job is canceled while the node's slots are all taken doesn't hold up
/// the queue: another tenant's sync still runs when a slot frees.
#[tokio::test]
async fn a_canceled_queued_job_doesnt_hold_up_other_tenants() {
    use search_es::slots::{Caps, Slots};
    let pool = Slots::new(Caps { node: 1, owner: 1, cluster: 1 });
    let tenant = |name: &'static str, owner: &'static str| {
        let pool = pool.clone();
        async move {
            let mut cfg = ClusterConfig::new(name);
            cfg.sealer = search_es::seal::Sealer::from_secret("0123456789abcdef0123456789abcdef-connectors");
            cfg.node_settings.push(("roll.connectors.allow_private_hosts".into(), "true".into()));
            cfg.owner = Some(owner.into());
            cfg.connector_slots = Some(pool);
            let c = Arc::new(Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap());
            c.enable_background_tasks();
            c
        }
    };
    let (silent, _) = fake_postgres(Fake::Silent).await;
    let (port, _) = fake_postgres(Fake::Password).await;
    let (holder, queued, other) = (tenant("holder", "hal").await, tenant("queued", "quinn").await, tenant("other", "olga").await);
    // The only slot: a sync waiting on a silent server.
    pg_connector(&holder, &format!("127.0.0.1:{silent}"), "pg", "content-h", "*", "pw-h").await;
    let held = start(&holder, "pg", "full").await;
    let in_progress = |c: Arc<Cluster>, id: String| async move {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while call(&c, "GET", &format!("/_connector/_sync_job/{id}"), None).await.1.get("status").and_then(Value::as_str) != Some("in_progress") {
            assert!(std::time::Instant::now() < deadline, "{id} never started");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    in_progress(holder.clone(), held.clone()).await;
    // Two tenants wait for it, in order.
    pg_connector(&queued, &format!("127.0.0.1:{port}"), "pg", "content-q", "*", "pw-q").await;
    let canceled = start(&queued, "pg", "full").await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while pool.queued() != ["queued"] {
        assert!(std::time::Instant::now() < deadline, "{:?}", pool.queued());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    pg_connector(&other, &format!("127.0.0.1:{port}"), "pg", "content-o", "*", "pw-o").await;
    let wanted = start(&other, "pg", "full").await;
    while pool.queued() != ["queued", "other"] {
        assert!(std::time::Instant::now() < deadline, "{:?}", pool.queued());
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The first waiter's job is canceled (a pending job goes straight to canceled), then the
    // slot frees: the other tenant's sync runs.
    assert_eq!(call(&queued, "PUT", &format!("/_connector/_sync_job/{canceled}/_cancel"), None).await.0, 200);
    assert_eq!(call(&holder, "PUT", &format!("/_connector/_sync_job/{held}/_cancel"), None).await.0, 200);
    let job = finished(&other, &wanted).await;
    assert_eq!(at(&job, "status").as_str(), Some("error"), "it ran (and the fake refused it): {job:?}");
    assert!(at(&job, "started_at").as_str().is_some());
}

/// A server that accepts the connection and never answers doesn't hold the sync: cancel ends it
/// at once.
#[tokio::test]
async fn cancel_stops_a_sync_waiting_on_a_silent_server() {
    let (port, _) = fake_postgres(Fake::Silent).await;
    let c = cluster(&[]).await;
    pg_connector(&c, &format!("127.0.0.1:{port}"), "pg", "content-silent", "*", "pw-silent-1").await;
    let id = start(&c, "pg", "full").await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while call(&c, "GET", &format!("/_connector/_sync_job/{id}"), None).await.1.get("status").and_then(Value::as_str) != Some("in_progress") {
        assert!(std::time::Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let canceled_at = std::time::Instant::now();
    assert_eq!(call(&c, "PUT", &format!("/_connector/_sync_job/{id}/_cancel"), None).await.0, 200);
    let job = finished(&c, &id).await;
    assert_eq!(at(&job, "status").as_str(), Some("canceled"), "{job:?}");
    assert!(canceled_at.elapsed() < Duration::from_secs(5), "took {:?}", canceled_at.elapsed());
}

/// A branch keeps the connectors it inherited paused: no sync of theirs runs there (it would
/// read the parent's source with the parent's stored credentials) until the branch puts the
/// connector again and configures it.
#[tokio::test]
async fn a_branch_runs_no_inherited_sync_until_the_connector_is_its_own() {
    let (port, got) = fake_postgres(Fake::Password).await;
    let mut cfg = ClusterConfig::new("branch");
    cfg.connector_slots = Some(slots());
    cfg.sealer = search_es::seal::Sealer::from_secret("0123456789abcdef0123456789abcdef-connectors");
    cfg.node_settings.push(("roll.connectors.allow_private_hosts".into(), "true".into()));
    cfg.branch_of = Some("parent".into());
    let b = Arc::new(Cluster::open(cfg, Arc::new(MemoryStore::new())).await.unwrap());
    // The branch's copy of its parent's state: a configured connector with a job waiting.
    pg_connector(&b, &format!("127.0.0.1:{port}"), "pg", "content-pg", "*", "pw-parent-3").await;
    let waiting = start(&b, "pg", "full").await;
    // Its first load marks what it inherited, then its scheduler starts.
    search_es::branch::mark_inherited(&b).await.unwrap();
    b.enable_background_tasks();
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (_, j, _) = call(&b, "GET", &format!("/_connector/_sync_job/{waiting}"), None).await;
    assert_eq!(at(&j, "status").as_str(), Some("pending"), "an inherited connector's job waits: {j:?}");
    let again = start(&b, "pg", "full").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (_, j, _) = call(&b, "GET", &format!("/_connector/_sync_job/{again}"), None).await;
    assert_eq!(at(&j, "status").as_str(), Some("pending"), "a job started on the branch waits too: {j:?}");
    assert!(got.lock().unwrap().is_empty(), "the parent's source was reached");
    for id in [&waiting, &again] {
        assert_eq!(call(&b, "PUT", &format!("/_connector/_sync_job/{id}/_cancel"), None).await.0, 200);
    }

    // A configuration write that gives no password makes it the branch's own without one: the
    // parent's stored password is cleared, and a sync sends none.
    assert_eq!(call(&b, "PUT", "/_connector/pg/_configuration", Some(r#"{"values":{}}"#)).await.0, 200);
    let (_, c, _) = call(&b, "GET", "/_connector/pg", None).await;
    assert_eq!(at(&c, "status").as_str(), Some("needs_configuration"), "{c:?}");
    assert_eq!(c.pointer(&["configuration", "password", "value"]).and_then(Value::as_str), Some(""), "{c:?}");
    let job = finished(&b, &start(&b, "pg", "full").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("error"), "{job:?}");
    assert!(got.lock().unwrap().is_empty(), "the parent's password was sent: {:?}", got.lock().unwrap());

    // Put again and configured on the branch: its own, and its syncs run.
    let (s, _, t) = call(&b, "PUT", "/_connector/pg", Some(r#"{"index_name":"content-pg","is_native":true,"service_type":"postgresql","name":"shop"}"#)).await;
    assert_eq!(s, 200, "{t}");
    let values = format!(r#"{{"values":{{"host":"127.0.0.1","port":{port},"username":"roll","password":"pw-branch-4","database":"shop","ssl_enabled":false}}}}"#);
    assert_eq!(call(&b, "PUT", "/_connector/pg/_configuration", Some(&values)).await.0, 200);
    let job = finished(&b, &start(&b, "pg", "full").await).await;
    assert_eq!(at(&job, "status").as_str(), Some("error"), "it ran (the fake refused it): {job:?}");
    assert_eq!(got.lock().unwrap().first().map(String::as_str), Some("pw-branch-4"));
}
