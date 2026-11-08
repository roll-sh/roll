//! Docker end-to-end: three real `rolld` containers running roll queues as a roll cloud fleet
//! does: leases in DynamoDB (DynamoDB Local), metadata in the system database (Postgres), logs in
//! S3 (MinIO). Kills a queue owner with SIGKILL and rolls a node with SIGTERM while a publisher
//! and a consumer (on other nodes) keep going; every confirmed message must arrive and the client
//! connections must never drop. Then restarts Postgres: the nodes reconnect and metadata changes
//! work again. Skipped unless `ROLL_E2E=1`.

use futures_util::StreamExt;
use lapin::options::*;
use lapin::types::{AMQPValue, FieldTable};
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties};
use roll_cluster::Backend;
use std::collections::HashSet;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

const IMAGE: &str = "roll-queues:e2e";
const QUEUES: usize = 6;
const S3_USER: &str = "rolle2e";
const S3_PASS: &str = "rolle2e-secret";

fn e2e() -> bool {
    std::env::var("ROLL_E2E").is_ok_and(|v| v == "1")
}

fn docker(args: &[&str]) -> String {
    let out = Command::new("docker").args(args).output().expect("docker runs");
    assert!(out.status.success(), "docker {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn docker_ok(args: &[&str]) -> bool {
    Command::new("docker").args(args).output().is_ok_and(|o| o.status.success())
}

/// Where the host reaches a container's published port.
fn published(container: &str, port: &str) -> String {
    let p = Command::new("docker").args(["port", container, port]).output().map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default();
    format!("127.0.0.1:{}", p.lines().next().unwrap_or_default().rsplit(':').next().unwrap_or_default())
}

struct Env {
    tag: String,
    /// The lease table, read from here (the nodes reach DynamoDB Local on the network).
    leases: Option<roll_cluster::DynamoDb>,
}

impl Env {
    fn name(&self, what: &str) -> String {
        format!("roll-qe2e-{}-{what}", self.tag)
    }

    fn net(&self) -> String {
        self.name("net")
    }

    fn start_node(&self, n: &str) {
        let (name, net) = (self.name(n), self.net());
        let env = [
            format!("ROLL_NODE_ID={n}"),
            format!("ROLL_ADVERTISE_URL=http://{name}:7070"),
            "ROLL_CLUSTER_BACKEND=dynamodb".into(),
            "ROLL_CLUSTER_TABLE=roll-cluster".into(),
            format!("ROLL_CLUSTER_DYNAMODB_ENDPOINT=http://{}:8000", self.name("ddb")),
            "ROLL_CLUSTER_SECRET=e2e-cluster-secret-0123456789".into(),
            format!("ROLL_SYSTEM_DATABASE=postgres://postgres:roll@{}:5432/postgres", self.name("pg")),
            "ROLL_S3_BUCKET=roll".into(),
            format!("ROLL_S3_ENDPOINT=http://{}:9000", self.name("s3")),
            "AWS_REGION=us-east-1".into(),
            format!("AWS_ACCESS_KEY_ID={S3_USER}"),
            format!("AWS_SECRET_ACCESS_KEY={S3_PASS}"),
            "ROLL_QUEUES_USER=e2e".into(),
            "ROLL_QUEUES_PASSWORD=e2e".into(),
            "ROLL_ADMIN_TOKEN=e2e-operator-token-0123456789abcdef".into(),
            "ROLL_DRAIN_TIMEOUT=20".into(),
            "RUST_LOG=info,mq_cluster=debug".into(),
        ];
        let mut args: Vec<&str> = vec!["run", "-d", "--name", &name, "--hostname", n, "--network", &net];
        for e in &env {
            args.extend(["-e", e.as_str()]);
        }
        args.extend(["-p", "127.0.0.1::5672", IMAGE]);
        docker(&args);
    }

    fn url(&self, n: &str) -> String {
        format!("amqp://e2e:e2e@{}/%2f", published(&self.name(n), "5672/tcp"))
    }

    /// The node each queue's lease names (a released lease names its last holder until another
    /// node takes it).
    fn owners(&self) -> Vec<(String, String)> {
        let leases = self.leases.as_ref().expect("lease table");
        let mut names: Vec<String> = (0..QUEUES).map(|q| format!("q{q}")).collect();
        names.push("after-pg".into());
        names.into_iter().filter_map(|q| leases.holder(&mq_cluster::queue_key("/", &q, 0)).unwrap().map(|(node, _)| (q, node))).collect()
    }

    /// Every container's state and the tail of each node's log (when a node doesn't come up).
    fn dump(&self) {
        for n in ["n1", "n2", "n3", "pg", "s3", "ddb"] {
            let name = self.name(n);
            let state = Command::new("docker").args(["inspect", "-f", "{{.State.Status}} exit={{.State.ExitCode}}", &name]).output();
            eprintln!("── {name}: {}", state.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default());
            if n.starts_with('n')
                && let Ok(o) = Command::new("docker").args(["logs", "--tail", "40", &name]).output()
            {
                eprintln!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr));
            }
        }
    }

    async fn wait_owners(&self, what: &str, timeout: Duration, ok: impl Fn(&[(String, String)]) -> bool) {
        let t = Instant::now();
        loop {
            let owners = self.owners();
            if ok(&owners) {
                return;
            }
            assert!(t.elapsed() < timeout, "{what}: {owners:?}");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// MinIO (`minio/minio` when it can be pulled, else `cgr.dev/chainguard/minio`, or
    /// `ROLL_MINIO_IMAGE`) with the bucket `roll`, made with the MinIO client next to it.
    async fn start_s3(&self) {
        let image = std::env::var("ROLL_MINIO_IMAGE")
            .unwrap_or_else(|_| if docker_ok(&["pull", "-q", "minio/minio"]) { "minio/minio".into() } else { "cgr.dev/chainguard/minio".into() });
        let (user, pass) = (format!("MINIO_ROOT_USER={S3_USER}"), format!("MINIO_ROOT_PASSWORD={S3_PASS}"));
        let (name, net) = (self.name("s3"), self.net());
        let mut args = vec!["run", "-d", "--name", &name, "--network", &net, "-e", &user, "-e", &pass];
        if image.starts_with("cgr.dev/") {
            args.extend([image.as_str(), "server", "/data"]);
        } else {
            args.extend(["--entrypoint", "minio", image.as_str(), "server", "/data"]);
        }
        docker(&args);
        let mc = std::env::var("ROLL_MC_IMAGE").unwrap_or_else(|_| "cgr.dev/chainguard/minio-client".into());
        let host = format!("MC_HOST_local=http://{S3_USER}:{S3_PASS}@127.0.0.1:9000");
        let beside = format!("container:{name}");
        let t = Instant::now();
        while !docker_ok(&["run", "--rm", "--network", &beside, "-e", &host, &mc, "mb", "--ignore-existing", "local/roll"]) {
            assert!(t.elapsed() < Duration::from_secs(60), "MinIO never created the bucket");
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }

    /// DynamoDB Local, one database whatever the credentials (`-sharedDb`), with the lease table.
    async fn start_dynamodb(&mut self) {
        let (name, net) = (self.name("ddb"), self.net());
        docker(&["run", "-d", "--name", &name, "--network", &net, "-p", "127.0.0.1::8000", "amazon/dynamodb-local", "-jar", "DynamoDBLocal.jar", "-sharedDb", "-inMemory"]);
        let endpoint = format!("http://{}", published(&name, "8000/tcp"));
        let t = Instant::now();
        loop {
            let mut cfg = roll_cluster::DynamoDbConfig::new("roll-cluster");
            cfg.endpoint = Some(endpoint.clone());
            cfg.create_table = true;
            match tokio::task::spawn_blocking(move || roll_cluster::DynamoDb::connect(cfg)).await.unwrap() {
                Ok(db) => {
                    self.leases = Some(db);
                    return;
                }
                Err(e) => assert!(t.elapsed() < Duration::from_secs(60), "DynamoDB Local never answered: {e}"),
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if std::env::var("ROLL_E2E_KEEP").is_ok() {
            return;
        }
        drop(self.leases.take());
        for n in ["n1", "n2", "n3", "pg", "s3", "ddb"] {
            let _ = docker_ok(&["rm", "-f", "-v", &self.name(n)]);
        }
        let _ = docker_ok(&["network", "rm", &self.net()]);
    }
}

/// An AMQP connection to node `n`, waiting for it to come up; when it doesn't, every node's
/// state and log tail go to the output before the test fails.
async fn connect(env: &Env, n: &str) -> (Connection, Channel) {
    let t = Instant::now();
    loop {
        let url = env.url(n);
        match Connection::connect(&url, ConnectionProperties::default()).await {
            Ok(c) => {
                let ch = c.create_channel().await.unwrap();
                return (c, ch);
            }
            Err(e) if t.elapsed() > Duration::from_secs(120) => {
                env.dump();
                panic!("broker {n} at {url} never came up: {e}");
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    }
}

fn quorum() -> FieldTable {
    let mut t = FieldTable::default();
    t.insert("x-queue-type".into(), AMQPValue::LongString("quorum".into()));
    t
}

/// Publish `rounds` × every queue with confirms on `ch`; returns the confirmed bodies.
async fn publish(ch: Channel, prefix: String, rounds: u32) -> HashSet<String> {
    let mut confirmed = HashSet::new();
    for r in 0..rounds {
        for q in 0..QUEUES {
            let body = format!("{prefix}:{q}:{r}");
            let props = BasicProperties::default().with_delivery_mode(2);
            if let Ok(p) = ch.basic_publish("".into(), format!("q{q}").as_str().into(), BasicPublishOptions::default(), body.as_bytes(), props).await
                && let Ok(Ok(c)) = tokio::time::timeout(Duration::from_secs(20), p).await
                && c.is_ack()
            {
                confirmed.insert(body);
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    confirmed
}

async fn wait_all(seen: &parking_lot::Mutex<HashSet<String>>, want: &HashSet<String>, what: &str) {
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(60) && !want.iter().all(|m| seen.lock().contains(m)) {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let missing: Vec<_> = want.iter().filter(|m| !seen.lock().contains(*m)).take(10).collect();
    assert!(missing.is_empty(), "{what}: {} confirmed messages never arrived, e.g. {missing:?}", want.iter().filter(|m| !seen.lock().contains(*m)).count());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cluster_survives_kill_and_rolling_restart_under_load() {
    if !e2e() {
        eprintln!("skipped: set ROLL_E2E=1 (needs Docker)");
        return;
    }
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let dockerfile = format!("{root}/deploy/queues/Dockerfile");
    docker(&["build", "-q", "--build-arg", "PROFILE=dev", "-f", &dockerfile, "-t", IMAGE, root]);
    let mut env = Env { tag: format!("{:x}", std::process::id()), leases: None };
    docker(&["network", "create", &env.net()]);
    docker(&["run", "-d", "--name", &env.name("pg"), "--network", &env.net(), "-e", "POSTGRES_PASSWORD=roll", "postgres:18-alpine"]);
    env.start_s3().await;
    env.start_dynamodb().await;
    let t = Instant::now();
    while !docker_ok(&["exec", &env.name("pg"), "pg_isready", "-U", "postgres"]) {
        assert!(t.elapsed() < Duration::from_secs(60), "postgres didn't start");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // pg_isready passes during the init restart; give it a moment.
    tokio::time::sleep(Duration::from_secs(2)).await;
    for n in ["n1", "n2", "n3"] {
        env.start_node(n);
    }

    let (_c1, ch1) = connect(&env, "n1").await;
    for q in 0..QUEUES {
        ch1.queue_declare(format!("q{q}").as_str().into(), QueueDeclareOptions { durable: true, ..Default::default() }, quorum()).await.unwrap();
    }
    let (_c3, _) = connect(&env, "n3").await;
    env.wait_owners("queues never spread to n3", Duration::from_secs(60), |o| o.iter().any(|(_, n)| n == "n3")).await;

    // Consumer on n2 for the whole test; publisher on n1.
    let (c2, ch2) = connect(&env, "n2").await;
    ch2.basic_qos(200, BasicQosOptions::default()).await.unwrap();
    let seen = Arc::new(parking_lot::Mutex::new(HashSet::<String>::new()));
    for q in 0..QUEUES {
        let mut cons = ch2.basic_consume(format!("q{q}").as_str().into(), "".into(), BasicConsumeOptions::default(), FieldTable::default()).await.unwrap();
        let seen = seen.clone();
        tokio::spawn(async move {
            while let Some(Ok(d)) = cons.next().await {
                seen.lock().insert(String::from_utf8_lossy(&d.data).into_owned());
                let _ = d.ack(BasicAckOptions::default()).await;
            }
        });
    }
    ch1.confirm_select(ConfirmSelectOptions::default()).await.unwrap();

    // 1. SIGKILL the node owning some queues while traffic flows: its leases lapse (10 s) and
    // the others take its queues over from the logs in S3.
    let publisher = tokio::spawn(publish(ch1.clone(), "kill".into(), 400));
    tokio::time::sleep(Duration::from_secs(2)).await;
    docker(&["kill", "-s", "KILL", &env.name("n3")]);
    let killed_at = Instant::now();
    let confirmed = publisher.await.unwrap();
    assert!(confirmed.len() as f64 > 400.0 * QUEUES as f64 * 0.9, "most publishes confirmed through failover: {}", confirmed.len());
    wait_all(&seen, &confirmed, "after SIGKILL").await;
    env.wait_owners("queues failed over off n3", Duration::from_secs(30), |o| !o.iter().any(|(_, n)| n == "n3")).await;
    eprintln!("failover complete {:?} after kill", killed_at.elapsed());

    // 2. Rolling restart: n3 comes back, takes queues, then is stopped gracefully (drain) and
    // started again, all under load. Nothing may be lost and no publish may fail.
    docker(&["rm", "-f", "-v", &env.name("n3")]);
    env.start_node("n3");
    env.wait_owners("queues never moved back to n3", Duration::from_secs(60), |o| o.iter().any(|(_, n)| n == "n3")).await;
    let publisher = tokio::spawn(publish(ch1.clone(), "roll".into(), 400));
    tokio::time::sleep(Duration::from_secs(1)).await;
    docker(&["stop", "-t", "40", &env.name("n3")]);
    env.wait_owners("a stopped node hands every queue off", Duration::from_secs(5), |o| !o.iter().any(|(_, n)| n == "n3")).await;
    docker(&["start", &env.name("n3")]);
    let confirmed = publisher.await.unwrap();
    assert_eq!(confirmed.len(), 400 * QUEUES, "every publish confirmed through a rolling restart");
    wait_all(&seen, &confirmed, "after rolling restart").await;
    assert!(c2.status().connected(), "the consumer's connection never dropped");

    // 3. Restart Postgres (a system database failover): the nodes reconnect, and metadata writes
    // and the change feed work again.
    docker(&["restart", "-t", "5", &env.name("pg")]);
    let t = Instant::now();
    while !docker_ok(&["exec", &env.name("pg"), "pg_isready", "-U", "postgres"]) {
        assert!(t.elapsed() < Duration::from_secs(60), "postgres didn't come back");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let t = Instant::now();
    loop {
        let (_c, ch) = connect(&env, "n1").await;
        if ch.queue_declare("after-pg".into(), QueueDeclareOptions { durable: true, ..Default::default() }, quorum()).await.is_ok() {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(30), "metadata writes never recovered after the Postgres restart");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    env.wait_owners("the new queue never got an owner", Duration::from_secs(30), |o| o.iter().any(|(q, _)| q == "after-pg")).await;
    // The queue reaches another node through the change feed (asynchronous, so poll), and
    // traffic still flows end to end.
    let t = Instant::now();
    loop {
        let (_c, ch) = connect(&env, "n2").await;
        match ch.queue_declare("after-pg".into(), QueueDeclareOptions { passive: true, ..Default::default() }, FieldTable::default()).await {
            Ok(_) => break,
            Err(e) => assert!(t.elapsed() < Duration::from_secs(15), "n2 never saw the queue declared on n1: {e}"),
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    eprintln!("change feed caught up on n2 {:?} after the declare", t.elapsed());
    let confirmed = publish(ch1.clone(), "after-pg".into(), 50).await;
    assert_eq!(confirmed.len(), 50 * QUEUES, "every publish confirmed after the Postgres restart");
    wait_all(&seen, &confirmed, "after the Postgres restart").await;
}
