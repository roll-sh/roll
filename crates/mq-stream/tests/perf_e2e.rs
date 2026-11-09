//! RabbitMQ's Java stream PerfTest in Docker against an in-process broker. Runs only with
//! `ROLL_E2E=1`; `ROLL_E2E_PERF_SECS` sets the duration (default 10).

use mq_core::log::FileStore;
use mq_core::meta::MemMeta;
use mq_core::{Broker, BrokerConfig};
use std::process::Command;

#[tokio::test(flavor = "multi_thread")]
async fn stream_perf_test() {
    if std::env::var("ROLL_E2E").as_deref() != Ok("1") {
        eprintln!("skipping: set ROLL_E2E=1 to run stream-perf-test in Docker");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let broker = Broker::start(BrokerConfig::default(), MemMeta::new(), FileStore::new(dir.path())).await.unwrap();
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = mq_stream::Config { advertised_host: "host.docker.internal".into(), ..mq_stream::Config::new(addr) };
    tokio::spawn(mq_stream::serve_listener(broker, listener, config));

    let secs = std::env::var("ROLL_E2E_PERF_SECS").unwrap_or_else(|_| "10".into());
    let uri = format!("rabbitmq-stream://guest:guest@host.docker.internal:{}/%2f", addr.port());
    let out = tokio::task::spawn_blocking(move || {
        Command::new("docker")
            .args(["run", "--rm", "--add-host=host.docker.internal:host-gateway", "pivotalrabbitmq/stream-perf-test"])
            .args(["--uris", &uri, "--time", &secs])
            .output()
            .expect("docker run")
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    print!("{stdout}");
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "stream-perf-test failed");
    let summary = stdout.lines().find(|l| l.contains("Summary")).expect("a summary line");
    println!("PerfTest {summary}");
    let consumed = summary.split("consumed ").nth(1).and_then(|s| s.split_whitespace().next()).and_then(|n| n.parse::<f64>().ok());
    assert!(consumed.is_some_and(|n| n > 0.0), "nothing consumed: {summary}");
}
