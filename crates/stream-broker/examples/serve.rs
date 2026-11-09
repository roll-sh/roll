//! The broker on its own (no Postgres control plane), for measuring and experimenting.
//!
//! ```sh
//! cargo run --release -p stream-broker --example serve -- /tmp/roll-streams
//! ```
//!
//! Kafka on `ROLL_KAFKA` (127.0.0.1:9092), HTTP on `ROLL_HTTP` (127.0.0.1:7071). `ROLL_IDLE_UNLOAD_MS`
//! and `ROLL_CLEAN_INTERVAL_MS` override how soon idle partitions close and how often cleanup runs.

use std::time::Instant;
use stream_broker::store::Settings;
use stream_broker::{Broker, Config};

// The same allocator setup as rolld.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
const MI_OPTION_PURGE_DELAY: libmimalloc_sys::mi_option_t = 15;

fn env<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok().and_then(|v| v.parse().ok())
}

fn main() -> anyhow::Result<()> {
    unsafe { libmimalloc_sys::mi_option_set(MI_OPTION_PURGE_DELAY, 0) };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .on_thread_park(|| unsafe { libmimalloc_sys::mi_collect(false) })
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    let started = Instant::now();
    let home = std::path::absolute(std::env::args().nth(1).unwrap_or_else(|| ".roll".into()))?;
    let mut config = Config::local(home);
    config.listen = env("ROLL_KAFKA").unwrap_or_else(|| "127.0.0.1:9092".parse().unwrap());
    config.advertised_port = config.listen.port();
    config.default_stream = Some("default".into());
    let http: std::net::SocketAddr = env("ROLL_HTTP").unwrap_or_else(|| "127.0.0.1:7071".parse().unwrap());
    config.http_url = format!("http://{http}");
    if let Some(ms) = env("ROLL_IDLE_UNLOAD_MS") {
        config.idle_unload_ms = ms;
    }
    if let Some(ms) = env("ROLL_CLEAN_INTERVAL_MS") {
        config.clean_interval_ms = ms;
    }
    if let Some(mb) = env::<u64>("ROLL_STREAM_DISK_MB") {
        config.local_disk_bytes = mb << 20;
    }
    config.evict_on_unload = std::env::var("ROLL_STREAM_KEEP_LOCAL").is_err();

    let broker = Broker::new(config).await?;
    if broker.find_stream("default").is_none() {
        broker.create_stream("default", Settings::default())?;
    }
    broker.txns.recover(&broker).await?;
    let kafka = tokio::net::TcpListener::bind(broker.config.listen).await?;
    let listener = tokio::net::TcpListener::bind(http).await?;
    let topics: usize = broker.streams().iter().map(|s| s.topics().len()).sum();
    eprintln!("ready in {:.1} ms: kafka {} · http {http} · {topics} topics", started.elapsed().as_secs_f64() * 1e3, broker.config.listen);

    if std::env::var("ROLL_COLLECT").as_deref() == Ok("normal") && std::env::var("ROLL_TICK").is_ok() {
        tokio::spawn(async {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                unsafe { libmimalloc_sys::mi_collect(true) };
            }
        });
    }
    let router = stream_broker::http::router(broker.clone());
    tokio::select! {
        r = stream_broker::server::serve_on(broker.clone(), kafka) => r?,
        r = axum::serve(listener, router) => r?,
        _ = broker.clone().run() => {}
        _ = tokio::signal::ctrl_c() => {}
    }
    broker.sync_all().await;
    Ok(())
}
