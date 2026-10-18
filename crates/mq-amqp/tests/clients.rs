//! Real client libraries in Docker against an in-process broker (`tests/queues/clients`).
//! Runs only with `ROLL_E2E=1`; `ROLL_E2E_CLIENTS=a,b` picks the clients (default: pika,
//! amqplib and amqp091-go). `scripts/test-queues-clients.sh` runs the full matrix.

use mq_core::log::FileStore;
use mq_core::meta::MemMeta;
use mq_core::{Broker, BrokerConfig};
use std::path::PathBuf;
use std::process::Command;

#[tokio::test(flavor = "multi_thread")]
async fn client_matrix() {
    if std::env::var("ROLL_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set ROLL_E2E=1 to run the Docker client matrix");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let broker = Broker::start(BrokerConfig::default(), MemMeta::new(), FileStore::new(dir.path())).await.unwrap();
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = mq_amqp::ServerConfig { listen: addr, amqp10: None };
    tokio::spawn(mq_amqp::serve_listener(broker, listener, cfg));

    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/queues/clients");
    let clients = std::env::var("ROLL_E2E_CLIENTS").unwrap_or_else(|_| "python-pika,node-amqplib,go-amqp091".into());
    let url = format!("amqp://guest:guest@host.docker.internal:{}/%2F", addr.port());
    let mut failed = Vec::new();
    for client in clients.split(',').map(str::trim).filter(|c| !c.is_empty()) {
        let (client, root, url) = (client.to_string(), root.clone(), url.clone());
        let ok = tokio::task::spawn_blocking(move || {
            let image = format!("roll-qtest-{client}");
            let built = Command::new("docker").args(["build", "-q", "-t", &image]).arg(root.join(&client)).status().is_ok_and(|s| s.success());
            if !built {
                eprintln!("{client}: docker build failed");
                return (client, false);
            }
            let run = Command::new("docker")
                .args(["run", "--rm", "--add-host=host.docker.internal:host-gateway", "-e", &format!("AMQP_URL={url}"), &image])
                .output()
                .expect("docker run");
            print!("── {client}\n{}", String::from_utf8_lossy(&run.stdout));
            (client, run.status.success())
        })
        .await
        .unwrap();
        if !ok.1 {
            failed.push(ok.0);
        }
    }
    assert!(failed.is_empty(), "clients failed: {failed:?}");
}
