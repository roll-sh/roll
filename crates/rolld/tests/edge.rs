//! rolld with the edge on (`ROLL_EDGE=1`, dev-mode certificates): HTTPS routed by Host to the
//! API, tasks, code's git routes and the dashboard; connection details naming the public
//! hostnames; a RabbitMQ stream client over TLS told the name it connected with; redis-cli over
//! TLS with the database named by SNI; and a stop that fails `/healthz` at once while the
//! listeners keep serving. The domain is under `localhost`, so every edge name resolves here.
//!
//! redis-cli runs in Docker (`REDIS_IMAGE`); without Docker that part is skipped.

use serde_json::{Value, json};
use std::process::{Child, Command};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};

/// The operator token: the edge listens for the internet, so rolld wants one.
const TOKEN: &str = "edge-test-operator-token-0123456789";
/// redis-cli's image, pinned by digest (the multi-arch index), so a rebuilt upstream tag can't change it.
const REDIS_IMAGE: &str = "redis:8.10.3-alpine@sha256:71885d8cc6e42f2bc0aa2eb853bfc4be200b5108a46ac50e117ae1df0531fb80";

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

struct Rolld(Child);

impl Drop for Rolld {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// One HTTPS request through the edge, with the operator token: (status, body).
async fn https(port: u16, roots: &RootCertStore, host: &str, path: &str) -> (u16, String) {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions().unwrap().with_root_certificates(roots.clone()).with_no_client_auth();
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let mut tls = tokio_rustls::TlsConnector::from(Arc::new(cfg)).connect(ServerName::try_from(host.to_string()).unwrap(), tcp).await.unwrap();
    tls.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nAuthorization: Bearer {TOKEN}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut out = Vec::new();
    let _ = tls.read_to_end(&mut out).await;
    let text = String::from_utf8_lossy(&out).to_string();
    let status = text.split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (status, text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default())
}

async fn healthz(http: &reqwest::Client, api: &str) -> u16 {
    http.get(format!("{api}/healthz")).send().await.map_or(0, |r| r.status().as_u16())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn edge_routes_by_host_and_sni_and_drains_on_sigterm() {
    // Two rustls providers are linked (the stream client brings aws-lc-rs): pick one.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let home = tempfile::tempdir().unwrap();
    let certs = home.path().join("certs");
    // The dashboard, standing in for the web app's container.
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let dashboard = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, axum::Router::new().fallback(|| async { "dashboard" })).await });

    let (api, https_port, edge_redis, edge_kafka, edge_nosql) = (free_port(), free_port(), free_port(), free_port(), free_port());
    let (edge_amqps, edge_mqtts, edge_stomps, edge_stream) = (free_port(), free_port(), free_port(), free_port());
    let local = |p: u16| format!("127.0.0.1:{p}");
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_rolld"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("RUST_LOG", "warn")
        .env("ROLL_EDGE", "1")
        .env("ROLL_ADMIN_TOKEN", TOKEN)
        .env("ROLL_EDGE_DOMAIN", "roll.localhost")
        .env("ROLL_EDGE_SITE_DOMAIN", "rollsite.test")
        .env("ROLL_EDGE_REGION", "r1")
        .env("ROLL_EDGE_CERT_DIR", &certs)
        .env("ROLL_EDGE_DRAIN_DELAY", "3")
        .env("ROLL_DASHBOARD_UPSTREAM", &dashboard)
        .env("ROLL_EDGE_HTTPS", local(https_port))
        .env("ROLL_EDGE_REDIS", local(edge_redis))
        .env("ROLL_EDGE_KAFKA", local(edge_kafka))
        .env("ROLL_EDGE_NOSQL", local(edge_nosql))
        .env("ROLL_EDGE_PG", local(free_port()))
        .env("ROLL_EDGE_AMQPS", local(edge_amqps))
        .env("ROLL_EDGE_MQTTS", local(edge_mqtts))
        .env("ROLL_EDGE_STOMPS", local(edge_stomps))
        .env("ROLL_EDGE_MQ_STREAM", local(edge_stream))
        .env("ROLL_QUEUES_USER", "roll-admin")
        .env("ROLL_QUEUES_PASSWORD", "edge-test-queues-password");
    for var in ["ROLL_QUEUES_AMQP", "ROLL_QUEUES_MANAGEMENT", "ROLL_QUEUES_STOMP", "ROLL_QUEUES_WEB_STOMP", "ROLL_QUEUES_MQTT", "ROLL_QUEUES_WEB_MQTT", "ROLL_QUEUES_STREAM", "ROLL_QUEUES_HTTP"] {
        cmd.env(var, local(free_port()));
    }
    cmd.args(["--home"]).arg(home.path()).args(["--api", &local(api)]);
    let ports = [
        ("--proxy", free_port()),
        ("--redis", free_port()),
        ("--redis-http", free_port()),
        ("--kafka", free_port()),
        ("--s3", free_port()),
        ("--storage-api", free_port()),
        ("--nosql-cql", free_port()),
        ("--nosql-cql-shard-aware", free_port()),
        ("--nosql-rest", free_port()),
        ("--nosql-alternator", free_port()),
        ("--search-http", free_port()),
    ];
    for (flag, port) in ports {
        cmd.args([flag, &local(port)]);
    }
    let mut rolld = Rolld(cmd.spawn().expect("rolld starts"));
    let api = format!("http://127.0.0.1:{api}");
    let http = reqwest::Client::new();
    let started = Instant::now();
    while healthz(&http, &api).await != 200 {
        assert!(started.elapsed() < Duration::from_secs(60), "rolld didn't become healthy");
        assert!(rolld.0.try_wait().unwrap().is_none(), "rolld exited");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let mut roots = RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(certs.join("edge.crt")).unwrap() {
        roots.add(c.unwrap()).unwrap();
    }

    // Host picks the router: the API, tasks, code's git routes, the dashboard.
    let (s, body) = https(https_port, &roots, "api.roll.localhost", "/v1/redis").await;
    assert_eq!(s, 200, "{body}");
    assert_eq!(https(https_port, &roots, "tasks.r1.roll.localhost", "/v1/redis").await.0, 404, "tasks has no Redis API");
    assert_eq!(https(https_port, &roots, "tasks.r1.roll.localhost", "/v1/tasks/queues").await.0, 200);
    assert_eq!(https(https_port, &roots, "roll.localhost", "/dashboard").await.1, "dashboard");
    let (s, body) = https(https_port, &roots, "roll.localhost", "/acme/app.git/info/refs?service=git-upload-pack").await;
    assert!(s == 404 && body != "dashboard", "git paths go to code: {s} {body}");

    // Connection details name the public hostnames, over TLS, never this node's listeners.
    let post = |path: &str, body: Value| http.post(format!("{api}{path}")).bearer_auth(TOKEN).json(&body).send();
    let db: Value = post("/v1/nosql/databases", json!({ "name": "app" })).await.unwrap().json().await.unwrap();
    let id = db["id"].as_str().unwrap();
    let host = format!("{id}.nosql.r1.roll.localhost");
    assert_eq!((db["endpoints"]["cql"].as_str(), db["tls"].as_bool(), db["server_name"].as_str()), (Some(format!("{host}:{edge_nosql}").as_str()), Some(true), Some(host.as_str())), "{db}");
    assert_eq!(db["endpoints"]["alternator"].as_str(), Some(format!("https://{host}").as_str()), "{db}");
    assert!(db["endpoints"]["cql_shard_aware"].is_null() && db["endpoints"]["rest"].is_null(), "{db}");
    let stream: Value = post("/v1/streams", json!({ "name": "events" })).await.unwrap().json().await.unwrap();
    let sid = stream["id"].as_str().unwrap();
    assert_eq!(stream["bootstrap"].as_str(), Some(format!("{sid}.kafka.r1.roll.localhost:{edge_kafka}").as_str()), "{stream}");
    assert_eq!((stream["security_protocol"].as_str(), stream["http"].as_str()), (Some("SASL_SSL"), Some(format!("https://{sid}.kafka.r1.roll.localhost/v1/streams/{sid}").as_str())), "{stream}");
    // Queues: each vhost at `<label>.mq.…` over TLS on the edge's ports, nothing in the clear.
    let vh: Value = post("/v1/queues/vhosts", json!({ "name": "shop", "credentials": true })).await.unwrap().json().await.unwrap();
    let (user, pass) = (vh["credentials"]["user"].as_str().unwrap(), vh["credentials"]["password"].as_str().unwrap());
    let urls = &vh["credentials"]["urls"];
    let mq = "shop.mq.r1.roll.localhost";
    assert_eq!(urls["amqps"].as_str(), Some(format!("amqps://{user}:{pass}@{mq}:{edge_amqps}/shop").as_str()), "{vh}");
    assert_eq!(urls["mqtt"].as_str(), Some(format!("mqtts://shop%3A{user}:{pass}@{mq}:{edge_mqtts}").as_str()), "{vh}");
    assert_eq!(urls["stomp"].as_str(), Some(format!("stomp+ssl://{user}:{pass}@{mq}:{edge_stomps}/shop").as_str()), "{vh}");
    assert_eq!(urls["stream"].as_str(), Some(format!("rabbitmq-stream+tls://{user}:{pass}@{mq}:{edge_stream}/shop").as_str()), "{vh}");
    assert_eq!(urls["http"].as_str(), Some(format!("https://{user}:{pass}@{mq}/v1/vhosts/shop").as_str()), "{vh}");
    assert!(urls["amqp"].is_null() && urls["management"].is_null(), "{vh}");
    assert_eq!(vh["amqp_url"].as_str(), Some(format!("amqps://roll-admin@{mq}:{edge_amqps}/shop").as_str()), "{vh}");
    let overview: Value = http.get(format!("{api}/v1/queues/overview")).bearer_auth(TOKEN).send().await.unwrap().json().await.unwrap();
    assert_eq!(overview["endpoints"]["amqps"].as_str(), Some(format!("amqps://default.mq.r1.roll.localhost:{edge_amqps}").as_str()), "the default vhost /: {overview}");

    // A stream client over TLS: `Open` and `Metadata` name the host it connected with (one
    // virtual node), so it publishes and reads through the edge, never at this node's own port.
    {
        use rabbitmq_stream_client::types::{Message, OffsetSpecification};
        let tls = rabbitmq_stream_client::TlsConfiguration::builder().add_root_certificates(certs.join("edge.crt").to_str().unwrap()).build().unwrap();
        let env = rabbitmq_stream_client::Environment::builder().host(mq).port(edge_stream).username(user).password(pass).virtual_host("shop").tls(tls).build().await.unwrap();
        env.stream_creator().create("events").await.unwrap();
        let producer = env.producer().build("events").await.unwrap();
        for i in 0..10 {
            assert!(producer.send_with_confirm(Message::builder().body(format!("e{i}")).build()).await.unwrap().confirmed());
        }
        let mut consumer = env.consumer().offset(OffsetSpecification::First).build("events").await.unwrap();
        for i in 0..10 {
            let d = tokio::time::timeout(Duration::from_secs(10), futures_util::StreamExt::next(&mut consumer)).await.unwrap().unwrap().unwrap();
            assert_eq!(d.message().data(), Some(format!("e{i}").as_bytes()));
        }
        let _ = producer.close().await;
    }

    // redis-cli over TLS: the server name picks the database.
    let docker = Command::new("docker").arg("info").output().is_ok_and(|o| o.status.success());
    if docker {
        let db: Value = http.post(format!("{api}/v1/redis/databases")).bearer_auth(TOKEN).json(&json!({ "name": "cache" })).send().await.unwrap().json().await.unwrap();
        let (id, password) = db["rest_token"].as_str().unwrap().rsplit_once('_').unwrap();
        let sni = format!("{id}.redis.r1.roll.localhost");
        let cli = |args: &[&str]| {
            let port = edge_redis.to_string();
            let mut a = vec!["run", "--rm", "--network", "host", REDIS_IMAGE, "redis-cli", "--tls", "--insecure", "--sni", &sni, "-h", "127.0.0.1", "-p", &port, "--no-auth-warning", "-a", password];
            a.extend_from_slice(args);
            let out = Command::new("docker").args(&a).output().unwrap();
            // Only redis-cli's answer: Docker writes its own progress to stderr.
            assert!(out.status.success(), "redis-cli: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        // Pulled first, so the first command isn't timed against the download.
        assert!(Command::new("docker").args(["pull", "-q", REDIS_IMAGE]).output().unwrap().status.success());
        assert_eq!(cli(&["SET", "k", "through the edge"]), "OK");
        assert_eq!(cli(&["GET", "k"]), "through the edge");
    } else {
        eprintln!("redis-cli part skipped: no Docker");
    }

    // SIGTERM: /healthz fails at once; the listeners keep serving for the drain delay; then it exits.
    let stopped = Instant::now();
    assert!(Command::new("kill").args(["-TERM", &rolld.0.id().to_string()]).status().unwrap().success());
    while healthz(&http, &api).await == 200 {
        assert!(stopped.elapsed() < Duration::from_secs(1), "healthz still 200 after SIGTERM");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(healthz(&http, &api).await, 503);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(https(https_port, &roots, "api.roll.localhost", "/v1/redis").await.0, 200, "still serving while the NLB lets go");
    loop {
        if rolld.0.try_wait().unwrap().is_some() {
            break;
        }
        assert!(stopped.elapsed() < Duration::from_secs(60), "rolld didn't exit");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(stopped.elapsed() >= Duration::from_secs(3), "exited before the drain delay: {:?}", stopped.elapsed());
}
