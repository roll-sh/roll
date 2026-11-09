//! A standalone AMQP 0-9-1 test broker for the client compatibility matrix and RabbitMQ's own
//! test tools (`scripts/test-queues-clients.sh`).
//!
//! ```text
//! cargo run -p mq-amqp --example broker -- [PORT]     # or ROLL_QUEUES_PORT; 0 picks a free port
//! ```
//!
//! It serves `0.0.0.0:<port>` with an `mq-core` broker on a temporary `FileStore` (deleted on
//! exit; set `ROLL_QUEUES_DATA` to keep it), default user `guest`/`guest` on vhost `/`, and
//! prints `listening on <addr>` once it accepts connections.

use mq_core::log::{FileStore, LogStore};
use mq_core::meta::SqliteMeta;
use mq_core::{Broker, BrokerConfig};
use std::net::SocketAddr;
use std::sync::Arc;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "mq_core=info,mq_amqp=info,warn".into()))
        .init();
    let port: u16 = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("ROLL_QUEUES_PORT").ok())
        .map(|p| p.parse())
        .transpose()?
        .unwrap_or(5672);
    let (_tmp, dir) = match std::env::var("ROLL_QUEUES_DATA") {
        Ok(d) => (None, std::path::PathBuf::from(d)),
        Err(_) => {
            let t = tempfile::tempdir()?;
            let p = t.path().to_path_buf();
            (Some(t), p)
        }
    };
    std::fs::create_dir_all(&dir)?;
    let fsync = std::env::var("ROLL_QUEUES_FSYNC").map(|v| v != "0").unwrap_or(true);
    let meta = SqliteMeta::open(&dir.join("meta.db"))?;
    let logs: Arc<dyn LogStore> = if fsync { FileStore::with_wal(dir.join("logs"), 16 << 20)? } else { FileStore::with(dir.join("logs"), 16 << 20, false) };
    let broker = Broker::start(BrokerConfig::default(), meta, logs.clone()).await?;

    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([0, 0, 0, 0], port))).await?;
    let addr = listener.local_addr()?;
    println!("listening on {addr}");
    let config = mq_amqp::ServerConfig { listen: addr, amqp10: None };
    tokio::select! {
        r = mq_amqp::serve_listener(broker.clone(), listener, config) => r?,
        _ = tokio::signal::ctrl_c() => {}
    }
    broker.stop().await;
    logs.close().await;
    Ok(())
}
