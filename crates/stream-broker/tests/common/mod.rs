//! A broker on a random port over a temp dir, for integration tests.
#![allow(dead_code)]

pub mod client;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use stream_broker::store::Settings;
use stream_broker::{Broker, Config, server};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub struct TestBroker {
    pub broker: Arc<Broker>,
    pub addr: String,
    /// Base URL of the HTTP API.
    pub http: String,
    pub dir: PathBuf,
    /// The default stream: unauthenticated connections land here.
    pub stream_id: String,
    pub username: String,
    pub secret: String,
    tasks: Vec<JoinHandle<()>>,
}

pub fn temp_dir(name: &str) -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!("roll-stream-{name}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub fn init_tracing() {
    let _ = tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).with_test_writer().try_init();
}

/// Settings tests usually want: quick rebalances, eager uploads.
pub fn test_config(dir: &std::path::Path) -> Config {
    let mut c = Config::local(dir.to_path_buf());
    c.groups.initial_rebalance_delay_ms = 0;
    c.upload.min_bytes = 1;
    c.upload.max_age_ms = 0;
    c.upload.interval_ms = 100;
    c.clean_interval_ms = 200;
    // `ROLL_E2E_S3_BUCKET` (with `ROLL_E2E_S3_ENDPOINT` and AWS credentials) puts object storage
    // on S3, under a prefix of its own per broker directory.
    if let Ok(bucket) = std::env::var("ROLL_E2E_S3_BUCKET") {
        let id = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        c.storage = pg_storage::StorageConfig::S3 {
            bucket,
            region: std::env::var("AWS_REGION").ok(),
            endpoint: std::env::var("ROLL_E2E_S3_ENDPOINT").ok(),
            prefix: Some(format!("stream-tests/{id}")),
        };
    }
    c
}

impl TestBroker {
    pub async fn start(name: &str) -> Self {
        Self::start_with(name, |_| {}).await
    }

    pub async fn start_with(name: &str, f: impl FnOnce(&mut Config)) -> Self {
        let dir = temp_dir(name);
        let mut config = test_config(&dir);
        f(&mut config);
        Self::boot(dir, config, None).await
    }

    /// Start (or restart) a broker over `dir`.
    pub async fn boot(dir: PathBuf, mut config: Config, existing: Option<(String, String, String)>) -> Self {
        init_tracing();
        let listener = TcpListener::bind(config.listen.ip().to_string() + ":0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        config.listen = listener.local_addr().unwrap();
        config.advertised_port = port;
        let http_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = format!("http://{}", http_listener.local_addr().unwrap());
        config.http_url = http.clone();
        if config.default_stream.is_none() {
            config.default_stream = Some("test".into());
        }
        let broker = Broker::new(config).await.unwrap();
        let (stream_id, username, secret) = match existing {
            Some(x) => x,
            None => match broker.find_stream("test") {
                Some(s) => (s.id.clone(), String::new(), String::new()),
                None => {
                    let (s, cred, secret) = broker.create_stream("test", Settings::default()).unwrap();
                    (s.id.clone(), cred.username, secret)
                }
            },
        };
        broker.txns.recover(&broker).await.unwrap();
        let mut tasks = vec![];
        let b = broker.clone();
        tasks.push(tokio::spawn(async move {
            server::serve_on(b, listener).await.unwrap();
        }));
        tasks.push(tokio::spawn(broker.clone().run()));
        let router = stream_broker::http::router(broker.clone());
        tasks.push(tokio::spawn(async move {
            axum::serve(http_listener, router).await.unwrap();
        }));
        TestBroker { broker, addr: format!("127.0.0.1:{port}"), http, dir, stream_id, username, secret, tasks }
    }

    /// Stop serving and flush (simulates a clean shutdown); returns what's needed to reboot.
    pub async fn stop(mut self) -> (PathBuf, (String, String, String)) {
        self.broker.sync_all().await;
        for t in std::mem::take(&mut self.tasks) {
            t.abort();
            let _ = t.await;
        }
        (self.dir.clone(), (self.stream_id.clone(), self.username.clone(), self.secret.clone()))
    }
}

impl Drop for TestBroker {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}
