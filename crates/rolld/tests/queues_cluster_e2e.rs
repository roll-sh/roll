//! Docker end-to-end: a three-node roll queues cluster of real `rolld` containers with metadata in
//! Postgres. Kills a queue owner with SIGKILL and rolls a node with SIGTERM while a publisher and
//! a consumer (on other nodes) keep going; every confirmed message must arrive and the client
//! connections must never drop. Then restarts Postgres: the nodes reconnect and metadata changes
//! work again. Nodes talk to each other through the cluster secret. Skipped unless `ROLL_E2E=1`.

use futures_util::StreamExt;
use lapin::options::*;
use lapin::types::{AMQPValue, FieldTable};
use lapin::{BasicProperties, Channel, Connection, ConnectionProperties};
use std::collections::HashSet;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

const IMAGE: &str = "roll-queues:e2e";
const QUEUES: usize = 6;

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

struct Env {
    tag: String,
}

impl Env {
    fn name(&self, what: &str) -> String {
        format!("roll-qe2e-{}-{what}", self.tag)
    }

    fn net(&self) -> String {
        self.name("net")
    }

    fn start_node(&self, n: &str) {
        let meta = format!("ROLL_QUEUES_CLUSTER_META=postgres://postgres:roll@{}:5432/postgres", self.name("pg"));
        let id = format!("ROLL_QUEUES_NODE_ID={n}");
        let vol = format!("{}:/shared", self.name("logs"));
        docker(&[
            "run", "-d", "--name", &self.name(n), "--hostname", n, "--network", &self.net(), "-v", &vol, "-e", &meta, "-e", &id,
            "-e", "ROLL_QUEUES_STORAGE=file:/shared/logs", "-e", "ROLL_QUEUES_USER=e2e", "-e", "ROLL_QUEUES_PASSWORD=e2e",
            "-e", "ROLL_QUEUES_DRAIN_TIMEOUT=20", "-e", "ROLL_QUEUES_CLUSTER_SECRET=e2e-cluster-secret", "-e", "RUST_LOG=info,mq_cluster=debug", "-p", "127.0.0.1::5672", IMAGE,
        ]);
    }

    fn url(&self, n: &str) -> String {
        let port = docker(&["port", &self.name(n), "5672/tcp"]);
        let port = port.lines().next().unwrap_or_default().rsplit(':').next().unwrap_or_default().to_string();
        format!("amqp://e2e:e2e@127.0.0.1:{port}/%2f")
    }

    fn psql(&self, sql: &str) -> String {
        docker(&["exec", &self.name("pg"), "psql", "-U", "postgres", "-At", "-c", sql])
    }

    /// Owner node of each queue (from the ownership records).
    fn owners(&self) -> Vec<(String, String)> {
        self.psql("SELECT key, value->>'node' FROM mq_meta WHERE kind = 'mq_owner' ORDER BY key")
            .lines()
            .filter_map(|l| l.split_once('|').map(|(k, v)| (k.to_string(), v.to_string())))
            .collect()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        if std::env::var("ROLL_E2E_KEEP").is_ok() {
            return;
        }
        for n in ["n1", "n2", "n3", "pg"] {
            let _ = docker_ok(&["rm", "-f", "-v", &self.name(n)]);
        }
        let _ = docker_ok(&["volume", "rm", "-f", &self.name("logs")]);
        let _ = docker_ok(&["network", "rm", &self.net()]);
    }
}

async fn connect(url: &str) -> (Connection, Channel) {
    let t = Instant::now();
    loop {
        match Connection::connect(url, ConnectionProperties::default()).await {
            Ok(c) => {
                let ch = c.create_channel().await.unwrap();
                return (c, ch);
            }
            Err(e) if t.elapsed() > Duration::from_secs(90) => panic!("broker at {url} never came up: {e}"),
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
                && let Ok(Ok(c)) = tokio::time::timeout(Duration::from_secs(15), p).await
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
    let env = Env { tag: format!("{:x}", std::process::id()) };
    docker(&["network", "create", &env.net()]);
    docker(&["volume", "create", &env.name("logs")]);
    docker(&["run", "-d", "--name", &env.name("pg"), "--network", &env.net(), "-e", "POSTGRES_PASSWORD=roll", "postgres:18-alpine"]);
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

    let (_c1, ch1) = connect(&env.url("n1")).await;
    for q in 0..QUEUES {
        ch1.queue_declare(format!("q{q}").as_str().into(), QueueDeclareOptions { durable: true, ..Default::default() }, quorum()).await.unwrap();
    }
    let (_c3, _) = connect(&env.url("n3")).await;
    // Wait until rebalancing has given n3 some queues.
    let t = Instant::now();
    while !env.owners().iter().any(|(_, n)| n == "n3") {
        assert!(t.elapsed() < Duration::from_secs(40), "queues never spread to n3: {:?}", env.owners());
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // Consumer on n2 for the whole test; publisher on n1.
    let (c2, ch2) = connect(&env.url("n2")).await;
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

    // 1. SIGKILL the node owning some queues while traffic flows.
    let publisher = tokio::spawn(publish(ch1.clone(), "kill".into(), 400));
    tokio::time::sleep(Duration::from_secs(2)).await;
    docker(&["kill", "-s", "KILL", &env.name("n3")]);
    let killed_at = Instant::now();
    let confirmed = publisher.await.unwrap();
    assert!(confirmed.len() as f64 > 400.0 * QUEUES as f64 * 0.9, "most publishes confirmed through failover: {}", confirmed.len());
    wait_all(&seen, &confirmed, "after SIGKILL").await;
    assert!(!env.owners().iter().any(|(_, n)| n == "n3"), "queues failed over off n3");
    eprintln!("failover complete {:?} after kill", killed_at.elapsed());

    // 2. Rolling restart: n3 comes back, takes queues, then is stopped gracefully (drain) and
    // started again, all under load. Nothing may be lost and no publish may fail.
    docker(&["rm", "-f", "-v", &env.name("n3")]);
    env.start_node("n3");
    let t = Instant::now();
    while !env.owners().iter().any(|(_, n)| n == "n3") {
        assert!(t.elapsed() < Duration::from_secs(40), "queues never moved back to n3");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let publisher = tokio::spawn(publish(ch1.clone(), "roll".into(), 400));
    tokio::time::sleep(Duration::from_secs(1)).await;
    docker(&["stop", "-t", "40", &env.name("n3")]);
    assert!(!env.owners().iter().any(|(_, n)| n == "n3"), "a stopped node hands every queue off");
    docker(&["start", &env.name("n3")]);
    let confirmed = publisher.await.unwrap();
    assert_eq!(confirmed.len(), 400 * QUEUES, "every publish confirmed through a rolling restart");
    wait_all(&seen, &confirmed, "after rolling restart").await;
    assert!(c2.status().connected(), "the consumer's connection never dropped");

    // 3. Restart Postgres (a metadata failover): the nodes reconnect, and metadata writes and
    // the change feed work again.
    docker(&["restart", "-t", "5", &env.name("pg")]);
    let t = Instant::now();
    while !docker_ok(&["exec", &env.name("pg"), "pg_isready", "-U", "postgres"]) {
        assert!(t.elapsed() < Duration::from_secs(60), "postgres didn't come back");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let t = Instant::now();
    loop {
        let (_c, ch) = connect(&env.url("n1")).await;
        if ch.queue_declare("after-pg".into(), QueueDeclareOptions { durable: true, ..Default::default() }, quorum()).await.is_ok() {
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(30), "metadata writes never recovered after the Postgres restart");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let t = Instant::now();
    while !env.owners().iter().any(|(k, _)| k.contains("after-pg")) {
        assert!(t.elapsed() < Duration::from_secs(30), "the new queue never got an owner");
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // The queue reaches another node through the change feed (asynchronous, so poll), and
    // traffic still flows end to end.
    let t = Instant::now();
    loop {
        let (_c, ch) = connect(&env.url("n2")).await;
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
