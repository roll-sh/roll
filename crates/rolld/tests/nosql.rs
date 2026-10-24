//! rolld with roll nosql (`--nosql-only`, single mode): the API, the CQL, shard-aware, Alternator
//! and REST listeners, a graceful stop that keeps acknowledged writes, and a restart that serves
//! them. Hosted mode with TLS, SNI routing, the PROXY protocol and Alternator over HTTPS, and
//! rolld's full mode next to the Postgres control plane. On S3 when `ROLL_E2E_S3_BUCKET` is set.

use aws_sdk_dynamodb::types::{AttributeDefinition, AttributeValue, BillingMode, KeySchemaElement, KeyType, ScalarAttributeType};
use nosql_proto::client::{Client, ClientConfig};
use nosql_proto::{QueryParams, ResultBody};
use serde_json::{Value, json};
use std::process::{Child, Command};
use std::time::Duration;

/// A port nothing listens on, never handed out twice in this process (a released ephemeral
/// port can come straight back from the next `bind(0)`).
fn free_port() -> u16 {
    static GIVEN: std::sync::Mutex<Vec<u16>> = std::sync::Mutex::new(Vec::new());
    loop {
        let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut given = GIVEN.lock().unwrap();
        if !given.contains(&port) {
            given.push(port);
            return port;
        }
    }
}

/// Each test starts its own rolld on picked ports; running them one at a time keeps one test's
/// daemon from taking a port another test just picked.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct Ports {
    api: u16,
    cql: u16,
    shard: u16,
    rest: u16,
    alternator: u16,
}

struct Rolld(Child);

impl Drop for Rolld {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start(home: &std::path::Path, p: &Ports, extra: &[String]) -> Rolld {
    let child = Command::new(env!("CARGO_BIN_EXE_rolld"))
        .env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("RUST_LOG", "warn")
        .envs(storage_env(home))
        .args(["--nosql-only", "--home"])
        .arg(home)
        .args(["--api", &format!("127.0.0.1:{}", p.api)])
        .args(["--nosql-cql", &format!("127.0.0.1:{}", p.cql)])
        .args(["--nosql-cql-shard-aware", &format!("127.0.0.1:{}", p.shard)])
        .args(["--nosql-rest", &format!("127.0.0.1:{}", p.rest)])
        .args(["--nosql-alternator", &format!("127.0.0.1:{}", p.alternator)])
        .args(extra)
        .spawn()
        .expect("rolld starts");
    Rolld(child)
}

/// Object storage for a rolld under `home`: S3 under a prefix unique to `home` when
/// `ROLL_E2E_S3_BUCKET` is set (with `ROLL_E2E_S3_ENDPOINT` and the AWS credentials), otherwise
/// rolld's directory under `--home`.
fn storage_env(home: &std::path::Path) -> Vec<(String, String)> {
    let Ok(bucket) = std::env::var("ROLL_E2E_S3_BUCKET") else { return vec![] };
    let unique = home.file_name().unwrap().to_string_lossy().trim_start_matches('.').to_string();
    let mut env = vec![("ROLL_S3_BUCKET".to_string(), bucket), ("ROLL_S3_PREFIX".to_string(), format!("nosql-tests/rolld-{unique}"))];
    if let Ok(e) = std::env::var("ROLL_E2E_S3_ENDPOINT") {
        env.push(("ROLL_S3_ENDPOINT".into(), e));
    }
    for k in ["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY", "AWS_SESSION_TOKEN", "AWS_REGION"] {
        if let Ok(v) = std::env::var(k) {
            env.push((k.into(), v));
        }
    }
    env
}

fn ports() -> Ports {
    Ports { api: free_port(), cql: free_port(), shard: free_port(), rest: free_port(), alternator: free_port() }
}

async fn wait_api(http: &reqwest::Client, api: &str) -> Value {
    for _ in 0..300 {
        if let Ok(r) = http.get(format!("{api}/v1/nosql")).send().await
            && r.status().is_success()
        {
            return r.json().await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("rolld's API didn't come up");
}

async fn rows(c: &Client, q: &str) -> Vec<Vec<Option<bytes::Bytes>>> {
    match c.query(q, QueryParams::default()).await.unwrap() {
        ResultBody::Rows(r) => r.to_vec(),
        _ => vec![],
    }
}

#[tokio::test]
async fn nosql_only_serves_and_restarts() {
    let _serial = SERIAL.lock().await;
    let home = tempfile::tempdir().unwrap();
    let p = ports();
    let mode = ["--nosql-single".to_string(), "dev".to_string()];
    let api = format!("http://127.0.0.1:{}", p.api);
    let http = reqwest::Client::new();
    let mut d = start(home.path(), &p, &mode);
    let info = wait_api(&http, &api).await;
    assert_eq!(info["endpoints"]["cql"], format!("127.0.0.1:{}", p.cql));
    assert_eq!(info["endpoints"]["cql_shard_aware"], format!("127.0.0.1:{}", p.shard));
    assert_eq!(info["endpoints"]["alternator"], format!("http://127.0.0.1:{}", p.alternator));
    assert!(info["single"].is_string());

    // Single mode: no credentials needed, and the REST API answers for it.
    let single = Client::connect(ClientConfig::new(format!("127.0.0.1:{}", p.cql))).await.unwrap();
    rows(&single, "SELECT * FROM system.local").await;
    let shard = Client::connect(ClientConfig::new(format!("127.0.0.1:{}", p.shard))).await.unwrap();
    rows(&shard, "SELECT * FROM system.local").await;
    let name: Value = http.get(format!("http://127.0.0.1:{}/storage_service/cluster_name", p.rest)).send().await.unwrap().json().await.unwrap();
    assert_eq!(name, json!("dev"));

    // A hosted database through the API, with its credentials.
    let db: Value = http.post(format!("{api}/v1/nosql/databases")).json(&json!({"name": "app"})).send().await.unwrap().json().await.unwrap();
    let user = db["credentials"]["username"].as_str().unwrap().to_string();
    let pass = db["credentials"]["password"].as_str().unwrap().to_string();
    let c = Client::connect(ClientConfig::new(format!("127.0.0.1:{}", p.cql)).credentials(&user, &pass)).await.unwrap();
    c.query("CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}", QueryParams::default()).await.unwrap();
    c.query("CREATE TABLE ks.t (k int PRIMARY KEY, v text)", QueryParams::default()).await.unwrap();
    c.query("INSERT INTO ks.t (k, v) VALUES (1, 'kept')", QueryParams::default()).await.unwrap();
    drop((c, single, shard));

    // Alternator: the single-mode database takes unsigned requests; `app` authenticates with
    // its superuser's salted hash, the access key naming the database.
    let alt = format!("http://127.0.0.1:{}", p.alternator);
    let (st, v) = dynamodb(&http, &alt, "CreateTable", json!({"TableName": "kvs", "KeySchema": [{"AttributeName": "k", "KeyType": "HASH"}], "AttributeDefinitions": [{"AttributeName": "k", "AttributeType": "S"}], "BillingMode": "PAY_PER_REQUEST"})).await;
    assert_eq!(st, 200, "{v}");
    let (st, _) = dynamodb(&http, &alt, "PutItem", json!({"TableName": "kvs", "Item": {"k": {"S": "a"}, "v": {"S": "kept"}}})).await;
    assert_eq!(st, 200);
    let creds: Value = http.get(format!("{api}/v1/nosql/databases/{}/alternator", db["id"].as_str().unwrap())).send().await.unwrap().json().await.unwrap();
    assert_eq!(creds["endpoint"], alt);
    assert_eq!(creds["access_key_id"], format!("{}@{}", db["credentials"]["role"].as_str().unwrap(), db["id"].as_str().unwrap()));
    assert_eq!(creds["enforced"], true);
    assert_eq!(db["endpoints"]["alternator"], alt);
    let app = sdk(&alt, creds["access_key_id"].as_str().unwrap(), creds["secret_access_key"].as_str().unwrap());
    app.create_table()
        .table_name("orders")
        .key_schema(KeySchemaElement::builder().attribute_name("id").key_type(KeyType::Hash).build().unwrap())
        .attribute_definitions(AttributeDefinition::builder().attribute_name("id").attribute_type(ScalarAttributeType::S).build().unwrap())
        .billing_mode(BillingMode::PayPerRequest)
        .send()
        .await
        .unwrap();
    app.put_item().table_name("orders").item("id", AttributeValue::S("o1".into())).send().await.unwrap();
    let (st, v) = dynamodb(&http, &alt, "ListTables", json!({})).await;
    assert_eq!((st, v), (200, json!({"TableNames": ["kvs"]})), "the single-mode database is another one");

    // SIGTERM: a graceful stop.
    unsafe { libc::kill(d.0.id() as i32, libc::SIGTERM) };
    let status = d.0.wait().unwrap();
    assert!(status.success(), "{status:?}");

    let _d = start(home.path(), &p, &mode);
    wait_api(&http, &api).await;
    let list: Value = http.get(format!("{api}/v1/nosql/databases")).send().await.unwrap().json().await.unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2, "the single-mode database and app");
    let c = Client::connect(ClientConfig::new(format!("127.0.0.1:{}", p.cql)).credentials(&user, &pass)).await.unwrap();
    let r = rows(&c, "SELECT v FROM ks.t WHERE k = 1").await;
    assert_eq!(r[0][0].as_deref(), Some(&b"kept"[..]));
    // Alternator tables too, through both APIs.
    let (st, v) = dynamodb(&http, &alt, "GetItem", json!({"TableName": "kvs", "Key": {"k": {"S": "a"}}})).await;
    assert_eq!((st, v), (200, json!({"Item": {"k": {"S": "a"}, "v": {"S": "kept"}}})));
    let got = app.get_item().table_name("orders").key("id", AttributeValue::S("o1".into())).send().await.unwrap();
    assert!(got.item.is_some());
    // (Without TLS the CQL connection stays on the single-mode database, which needs no login.)
    let r = rows(&c, "SELECT k, \":attrs\" FROM alternator_kvs.kvs").await;
    assert_eq!(r[0][0].as_deref(), Some(&b"a"[..]));
}

/// An unsigned DynamoDB API request.
async fn dynamodb(http: &reqwest::Client, url: &str, op: &str, body: Value) -> (u16, Value) {
    let r = http
        .post(format!("{url}/"))
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("X-Amz-Target", format!("DynamoDB_20120810.{op}"))
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let st = r.status().as_u16();
    (st, serde_json::from_slice(&r.bytes().await.unwrap()).unwrap_or(Value::Null))
}

fn sdk(url: &str, access_key: &str, secret: &str) -> aws_sdk_dynamodb::Client {
    use aws_sdk_dynamodb::config::{BehaviorVersion, Credentials, Region};
    let conf = aws_sdk_dynamodb::Config::builder()
        .behavior_version(BehaviorVersion::latest())
        .region(Region::new("us-east-1"))
        .endpoint_url(url)
        .credentials_provider(Credentials::new(access_key, secret, None, None, "test"))
        .build();
    aws_sdk_dynamodb::Client::from_conf(conf)
}

/// A TCP proxy in front of `upstream` that opens every connection with a PROXY v1 header naming
/// `client_ip`, as an AWS NLB or HAProxy does.
async fn proxy_v1(upstream: u16, client_ip: &'static str) -> std::net::SocketAddr {
    use tokio::io::AsyncWriteExt;
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut down, peer)) = l.accept().await {
            tokio::spawn(async move {
                let Ok(mut up) = tokio::net::TcpStream::connect(("127.0.0.1", upstream)).await else { return };
                let header = format!("PROXY TCP4 {client_ip} 127.0.0.1 {} {upstream}\r\n", peer.port());
                if up.write_all(header.as_bytes()).await.is_ok() {
                    let _ = tokio::io::copy_bidirectional(&mut down, &mut up).await;
                }
            });
        }
    });
    addr
}

/// Hosted mode as on AWS: TLS on the CQL ports with SNI routing (`<id>.nosql.test`), the PROXY
/// protocol (behind a load balancer), Alternator over HTTPS routed by `<id>.alternator.test`,
/// the API's endpoints and credentials working verbatim, and both surviving a restart.
#[tokio::test]
async fn tls_sni_and_proxy_protocol() {
    let _serial = SERIAL.lock().await;
    use nosql_proto::client::TlsConfig;
    use tokio_rustls::rustls;
    let home = tempfile::tempdir().unwrap();
    let ck = rcgen::generate_simple_self_signed(vec!["*.nosql.test".into(), "*.alternator.test".into()]).unwrap();
    let (cert, key) = (home.path().join("tls.crt"), home.path().join("tls.key"));
    std::fs::write(&cert, ck.cert.pem()).unwrap();
    std::fs::write(&key, ck.signing_key.serialize_pem()).unwrap();
    let p = ports();
    let https = free_port();
    let args: Vec<String> = [
        "--nosql-tls-cert", cert.to_str().unwrap(), "--nosql-tls-key", key.to_str().unwrap(), "--nosql-domain", "nosql.test",
        "--nosql-alternator-https", &format!("127.0.0.1:{https}"), "--nosql-proxy-protocol",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let api = format!("http://127.0.0.1:{}", p.api);
    let http = reqwest::Client::new();
    let mut d = start(home.path(), &p, &args);
    let info = wait_api(&http, &api).await;
    assert!(info["single"].is_null(), "{info}");

    let db: Value = http.post(format!("{api}/v1/nosql/databases")).json(&json!({"name": "app"})).send().await.unwrap().json().await.unwrap();
    let id = db["id"].as_str().unwrap().to_string();
    assert_eq!(db["tls"], true);
    assert_eq!(db["server_name"], format!("{id}.nosql.test"));
    assert_eq!(db["endpoints"]["cql"], format!("{id}.nosql.test:{}", p.cql));
    assert_eq!(db["endpoints"]["alternator"], format!("https://{id}.alternator.test:{https}"));
    let (user, pass) = (db["credentials"]["username"].as_str().unwrap().to_string(), db["credentials"]["password"].as_str().unwrap().to_string());
    assert!(!user.contains('@'), "the server name names the database: {user}");
    let open: Value = http.post(format!("{api}/v1/nosql/databases")).json(&json!({"name": "open", "settings": {"authenticator": "AllowAllAuthenticator"}})).send().await.unwrap().json().await.unwrap();
    let open_id = open["id"].as_str().unwrap().to_string();

    let mut roots = rustls::RootCertStore::empty();
    roots.add(ck.cert.der().clone()).unwrap();
    let tls = |name: &str| TlsConfig::with_roots(roots.clone(), name).unwrap();
    let lb = proxy_v1(p.cql, "198.51.100.7").await;
    let lb_shard = proxy_v1(p.shard, "198.51.100.8").await;
    let conn = |addr: std::net::SocketAddr, name: String| ClientConfig::new(addr.to_string()).tls(tls(&name)).credentials(&user, &pass);

    // Through the load balancer, by server name, with the API's credentials as given.
    let c = Client::connect(conn(lb, format!("{id}.nosql.test"))).await.unwrap();
    c.query("CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}", QueryParams::default()).await.unwrap();
    c.query("CREATE TABLE ks.t (k int PRIMARY KEY, v text)", QueryParams::default()).await.unwrap();
    c.query("INSERT INTO ks.t (k, v) VALUES (1, 'tls')", QueryParams::default()).await.unwrap();
    // The PROXY header's address is the client's, and the session is TLS.
    let r = rows(&c, "SELECT address, ssl_enabled, username FROM system.clients").await;
    assert_eq!(r.len(), 1, "{r:?}");
    assert_eq!(r[0][0].as_deref(), Some(&[198u8, 51, 100, 7][..]));
    assert_eq!(r[0][1].as_deref(), Some(&[1u8][..]));
    assert_eq!(r[0][2].as_deref(), Some(user.as_bytes()));
    let s = Client::connect(conn(lb_shard, format!("{id}.nosql.test"))).await.unwrap();
    assert_eq!(rows(&s, "SELECT v FROM ks.t WHERE k = 1").await[0][0].as_deref(), Some(&b"tls"[..]));
    // The open database by its name needs no credentials and doesn't see app's keyspace.
    let o = Client::connect(ClientConfig::new(lb.to_string()).tls(tls(&format!("{open_id}.nosql.test")))).await.unwrap();
    assert!(o.query("SELECT v FROM ks.t", QueryParams::default()).await.is_err());
    // An unknown server name routes by `user@db`; a wrong password fails.
    let by_user = ClientConfig::new(lb.to_string()).tls(tls("other.nosql.test")).credentials(format!("{user}@{id}"), &pass);
    let u = Client::connect(by_user).await.unwrap();
    assert_eq!(rows(&u, "SELECT v FROM ks.t WHERE k = 1").await.len(), 1);
    assert!(Client::connect(ClientConfig::new(lb.to_string()).tls(tls(&format!("{id}.nosql.test"))).credentials(&user, "wrong")).await.is_err());
    // Without TLS, or without the PROXY header, nothing is served.
    assert!(tokio::time::timeout(Duration::from_secs(10), Client::connect(ClientConfig::new(lb.to_string()).credentials(&user, &pass))).await.is_ok_and(|r| r.is_err()));
    let direct = ClientConfig::new(format!("127.0.0.1:{}", p.cql)).tls(tls(&format!("{id}.nosql.test"))).credentials(&user, &pass);
    assert!(tokio::time::timeout(Duration::from_secs(10), Client::connect(direct)).await.is_ok_and(|r| r.is_err()));
    drop((c, s, o, u));

    // Alternator over HTTPS, routed by host: the open database answers unsigned requests, app
    // demands a signature.
    let alt_http = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(ck.cert.pem().as_bytes()).unwrap())
        .resolve(&format!("{open_id}.alternator.test"), ([127, 0, 0, 1], https).into())
        .resolve(&format!("{id}.alternator.test"), ([127, 0, 0, 1], https).into())
        .build()
        .unwrap();
    let open_url = format!("https://{open_id}.alternator.test:{https}");
    let (st, v) = dynamodb(&alt_http, &open_url, "CreateTable", json!({"TableName": "kvs", "KeySchema": [{"AttributeName": "k", "KeyType": "HASH"}], "AttributeDefinitions": [{"AttributeName": "k", "AttributeType": "S"}], "BillingMode": "PAY_PER_REQUEST"})).await;
    assert_eq!(st, 200, "{v}");
    let (st, v) = dynamodb(&alt_http, &open_url, "PutItem", json!({"TableName": "kvs", "Item": {"k": {"S": "a"}}})).await;
    assert_eq!(st, 200, "{v}");
    let (st, v) = dynamodb(&alt_http, &format!("https://{id}.alternator.test:{https}"), "ListTables", json!({})).await;
    assert_eq!(st, 400, "{v}");
    assert!(v["__type"].as_str().is_some_and(|t| t.ends_with("MissingAuthenticationTokenException")), "{v}");
    // The plain HTTP port routes by host too.
    let alt = format!("http://127.0.0.1:{}", p.alternator);
    let r = http
        .post(format!("{alt}/"))
        .header("Host", format!("{open_id}.alternator.test"))
        .header("Content-Type", "application/x-amz-json-1.0")
        .header("X-Amz-Target", "DynamoDB_20120810.ListTables")
        .body("{}")
        .send()
        .await
        .unwrap();
    assert_eq!(r.json::<Value>().await.unwrap(), json!({"TableNames": ["kvs"]}));

    // A restart keeps both, behind the same names.
    unsafe { libc::kill(d.0.id() as i32, libc::SIGTERM) };
    assert!(d.0.wait().unwrap().success());
    let _d = start(home.path(), &p, &args);
    wait_api(&http, &api).await;
    let c = Client::connect(conn(lb, format!("{id}.nosql.test"))).await.unwrap();
    assert_eq!(rows(&c, "SELECT v FROM ks.t WHERE k = 1").await[0][0].as_deref(), Some(&b"tls"[..]));
    let (st, v) = dynamodb(&alt_http, &open_url, "GetItem", json!({"TableName": "kvs", "Key": {"k": {"S": "a"}}})).await;
    assert_eq!((st, v), (200, json!({"Item": {"k": {"S": "a"}}})));
}

/// rolld's full mode (the Postgres control plane and proxy, Redis, streams and tasks next to
/// nosql; queues off to keep it small): every API answers on one port, nosql serves CQL, and a
/// graceful stop keeps nosql's acknowledged writes.
#[tokio::test]
async fn full_mode_with_postgres() {
    let _serial = SERIAL.lock().await;
    let home = tempfile::tempdir().unwrap();
    let p = ports();
    let (proxy, redis, redis_http, kafka) = (free_port(), free_port(), free_port(), free_port());
    let spawn = || {
        Rolld(
            Command::new(env!("CARGO_BIN_EXE_rolld"))
                .env_clear()
                .env("PATH", std::env::var("PATH").unwrap_or_default())
                .env("RUST_LOG", "warn")
                .envs(storage_env(home.path()))
                .args(["--no-queues", "--home"])
                .arg(home.path())
                .args(["--api", &format!("127.0.0.1:{}", p.api), "--proxy", &format!("127.0.0.1:{proxy}")])
                .args(["--redis", &format!("127.0.0.1:{redis}"), "--redis-http", &format!("127.0.0.1:{redis_http}")])
                .args(["--kafka", &format!("127.0.0.1:{kafka}")])
                .args(["--nosql-cql", &format!("127.0.0.1:{}", p.cql), "--nosql-cql-shard-aware", &format!("127.0.0.1:{}", p.shard)])
                .args(["--nosql-rest", &format!("127.0.0.1:{}", p.rest), "--nosql-alternator", &format!("127.0.0.1:{}", p.alternator)])
                .spawn()
                .expect("rolld starts"),
        )
    };
    let api = format!("http://127.0.0.1:{}", p.api);
    let http = reqwest::Client::new();
    let mut d = spawn();
    let info = wait_api(&http, &api).await;
    assert!(info["single"].is_null(), "no single-mode database in full mode: {info}");
    let health = http.get(format!("{api}/v1/health")).send().await.unwrap();
    assert!(health.status().is_success(), "the Postgres control plane answers");
    assert!(http.get(format!("{api}/v1/projects")).send().await.unwrap().status().is_success());

    let db: Value = http.post(format!("{api}/v1/nosql/databases")).json(&json!({"name": "full", "dialect": "cassandra"})).send().await.unwrap().json().await.unwrap();
    let (user, pass) = (db["credentials"]["username"].as_str().unwrap().to_string(), db["credentials"]["password"].as_str().unwrap().to_string());
    let cfg = || ClientConfig::new(format!("127.0.0.1:{}", p.cql)).credentials(&user, &pass);
    let c = Client::connect(cfg()).await.unwrap();
    c.query("CREATE KEYSPACE ks WITH replication = {'class': 'SimpleStrategy', 'replication_factor': 1}", QueryParams::default()).await.unwrap();
    c.query("CREATE TABLE ks.t (k int PRIMARY KEY, v text)", QueryParams::default()).await.unwrap();
    c.query("INSERT INTO ks.t (k, v) VALUES (1, 'full')", QueryParams::default()).await.unwrap();
    drop(c);

    unsafe { libc::kill(d.0.id() as i32, libc::SIGTERM) };
    assert!(d.0.wait().unwrap().success());
    let _d = spawn();
    wait_api(&http, &api).await;
    let c = Client::connect(cfg()).await.unwrap();
    assert_eq!(rows(&c, "SELECT v FROM ks.t WHERE k = 1").await[0][0].as_deref(), Some(&b"full"[..]));
}

/// Configuration errors stop rolld before it serves, with a message naming the flag.
#[test]
fn bad_flags_fail_at_start() {
    let home = tempfile::tempdir().unwrap();
    let p = ports();
    let missing = home.path().join("missing.pem").display().to_string();
    for (args, expect) in [
        (vec!["--nosql-alternator-write-isolation", "sometimes"], "unknown Alternator write isolation"),
        (vec!["--nosql-single", "dev", "--nosql-single-setting", "authenticator"], "expected KEY=VALUE"),
        (vec!["--nosql-single", "dev", "--nosql-from", "ftp://elsewhere"], "expected <api url>/<branch> or cql://host:port"),
        (vec!["--nosql-tls-cert", &missing, "--nosql-tls-key", &missing], "No such file"),
        (vec!["--nosql-alternator-https", "127.0.0.1:0"], "--nosql-tls-cert"),
        (vec!["--nosql-tls-client-auth", "true"], "--nosql-tls-client-auth true needs --nosql-tls-cert, --nosql-tls-key and --nosql-tls-client-ca"),
        (vec!["--nosql-tls-client-auth", "sometimes"], "invalid value 'sometimes'"),
        (vec!["--nosql-tls-client-ca", &missing], "--nosql-tls-cert"),
        (vec!["--nosql-cql-tls", "127.0.0.1:0"], "--nosql-tls-cert"),
        (vec!["--nosql-single", "dev", "--nosql-single-settings-json", "[1]"], "--nosql-single-settings-json: expected a JSON object"),
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_rolld"))
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .args(["--nosql-only", "--home"])
            .arg(home.path())
            .args(["--api", &format!("127.0.0.1:{}", p.api), "--nosql-cql", &format!("127.0.0.1:{}", p.cql)])
            .args(["--nosql-cql-shard-aware", &format!("127.0.0.1:{}", p.shard), "--nosql-rest", &format!("127.0.0.1:{}", p.rest)])
            .args(["--nosql-alternator", &format!("127.0.0.1:{}", p.alternator)])
            .args(&args)
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let t0 = std::time::Instant::now();
        while child.try_wait().unwrap().is_none() {
            if t0.elapsed() > Duration::from_secs(30) {
                let _ = child.kill();
                panic!("{args:?}: rolld kept running");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let out = child.wait_with_output().unwrap();
        assert!(!out.status.success(), "{args:?}");
        let text = String::from_utf8_lossy(&out.stderr);
        assert!(text.contains(expect), "{args:?}: {text}");
    }
}
