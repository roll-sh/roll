//! Connectors that sync, against real servers (`scripts/search-connectors-e2e.sh` starts
//! Postgres and MinIO and sets `ROLL_E2E_CONNECTORS_PG` / `ROLL_E2E_CONNECTORS_S3`; without them
//! these tests skip): full and incremental PostgreSQL syncs, deletes found by a full sync, an S3
//! bucket's objects and their text, cancel, the job's counters, and no credential in what the
//! API returns or records.

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

async fn cluster(settings: &[(&str, &str)]) -> Arc<Cluster> {
    let mut cfg = ClusterConfig::new("connectors");
    cfg.sealer = search_es::seal::Sealer::from_secret("0123456789abcdef0123456789abcdef-connectors");
    cfg.node_settings.push(("roll.connectors.allow_private_hosts".into(), "true".into()));
    cfg.node_settings.push(("roll.connectors.max_docs_per_second".into(), "100000".into()));
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
    let bucket = Client::new(at_, Some(Credentials { access_key: S3_KEY.into(), secret_key: S3_SECRET.into(), session_token: None }), true).unwrap();
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
    assert_eq!((at(&job, "status").as_str(), num(&job, "indexed_document_count")), (Some("completed"), 1), "{job:?}");
    bucket.delete("old/gone.txt").await.unwrap();
    let job = finished(&c, &start(&c, "files", "full").await).await;
    assert_eq!((num(&job, "deleted_document_count"), num(&job, "total_document_count")), (1, 5), "{job:?}");
    assert!(!visible(&c).await.contains(S3_SECRET), "the secret key is visible");
}
