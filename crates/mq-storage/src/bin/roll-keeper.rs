//! `roll-keeper`: a roll queues log keeper (one per availability zone).

use clap::Parser;
use mq_core::transport::{Transport, TransportConfig};
use mq_storage::KeeperConfig;
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "roll-keeper", version, about = "roll queues log keeper: stores the hot tail of replicated logs on local disk")]
struct Args {
    /// Address to listen on.
    #[arg(long, env = "ROLL_KEEPER_LISTEN", default_value = "0.0.0.0:9580")]
    listen: SocketAddr,
    /// Data directory (put it on its own EBS gp3 volume).
    #[arg(long, env = "ROLL_KEEPER_DIR", default_value = "/var/lib/roll-keeper")]
    dir: PathBuf,
    /// Availability zone id of this node (e.g. use1-az1).
    #[arg(long, env = "ROLL_KEEPER_AZ", default_value = "")]
    az: String,
    /// Acknowledge appends without fsync (unsafe; tests only).
    #[arg(long, env = "ROLL_KEEPER_NO_FSYNC")]
    no_fsync: bool,
    /// Blocking I/O threads.
    #[arg(long, env = "ROLL_KEEPER_SHARDS", default_value_t = 8)]
    shards: usize,
    /// Shared secret brokers must prove they know before writing (set the same value as the
    /// brokers' `ROLL_QUEUES_KEEPER_SECRET`).
    #[arg(long, env = "ROLL_KEEPER_SECRET", hide_env_values = true)]
    secret: Option<String>,
    /// Serve TLS with this certificate chain (PEM); needs `--tls-key`.
    #[arg(long, env = "ROLL_KEEPER_TLS_CERT", requires = "tls_key")]
    tls_cert: Option<PathBuf>,
    /// Private key for `--tls-cert` (PEM).
    #[arg(long, env = "ROLL_KEEPER_TLS_KEY", requires = "tls_cert")]
    tls_key: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into())).init();
    let a = Args::parse();
    if a.secret.is_none() {
        tracing::warn!("no ROLL_KEEPER_SECRET: any host that can reach {} can write logs", a.listen);
    }
    let transport = TransportConfig { secret: a.secret, tls_cert: a.tls_cert, tls_key: a.tls_key, ..Default::default() };
    let cfg = KeeperConfig { dir: a.dir, az: a.az, fsync: !a.no_fsync, shards: a.shards, transport: Transport::new("keeper", &transport)? };
    tokio::select! {
        r = mq_storage::keeper::serve(a.listen, cfg) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
    }
}
