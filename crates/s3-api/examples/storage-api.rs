//! A standalone roll storage with its developer API, for client tests (`packages/storage`) and
//! manual testing: a store on a local directory, the S3 API and the developer API.
//!
//! ```text
//! cargo run -p s3-api --example storage-api -- --dir /tmp/roll-api [--host 127.0.0.1] \
//!     [--port 0] [--s3-port 0] [--jwt-secret <HS256 secret for user tokens>]
//! ```
//!
//! Once both listeners are up it prints one line of JSON on stdout:
//! `{"url": "http://127.0.0.1:<port>/storage/v1", "s3": "http://127.0.0.1:<port>", "account":
//! "<12 digits>", "anon_key": "…", "service_key": "…", "jwt_secret": "…"}`. Logs go to stderr.

use s3_server::{Config, Server};
use s3_store::{AuthUpdate, LocalBackend, Secret, Store, StoreConfig};
use std::path::PathBuf;
use std::sync::Arc;

/// Supabase's local development JWT secret: user tokens signed with it are accepted.
const DEFAULT_JWT_SECRET: &str = "super-secret-jwt-token-with-at-least-32-characters-long";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_writer(std::io::stderr).with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into())).init();
    let mut host = "127.0.0.1".to_string();
    let mut port = 0u16;
    let mut s3_port = 0u16;
    let mut dir = std::env::temp_dir().join("roll-storage-api");
    let mut jwt_secret = DEFAULT_JWT_SECRET.to_string();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| anyhow::anyhow!("{flag} needs a value"));
        match flag.as_str() {
            "--host" => host = value()?,
            "--port" => port = value()?.parse()?,
            "--s3-port" => s3_port = value()?.parse()?,
            "--dir" => dir = PathBuf::from(value()?),
            "--jwt-secret" => jwt_secret = value()?,
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    std::fs::create_dir_all(&dir)?;
    let backend = Arc::new(LocalBackend::new(&dir.join("blobs"))?);
    let mut scfg = StoreConfig::new(dir.join("meta"));
    scfg.lifecycle_interval = None;
    let store = Store::open(scfg, backend).await?;
    let spool = dir.join("spool");
    std::fs::create_dir_all(&spool)?;
    let account = store.default_account_id();
    let project = store.set_api_auth(&account, AuthUpdate { jwt_secret: Some(Some(Secret::new(jwt_secret.clone()))), ..Default::default() }).await?;
    let s3 = tokio::net::TcpListener::bind((host.as_str(), s3_port)).await?;
    let api_l = tokio::net::TcpListener::bind((host.as_str(), port)).await?;
    let server = Server::new(store.clone(), Config { spool_dir: spool.clone(), ..Config::default() });
    let api = s3_api::Api::new(server.clone(), s3_api::Config { spool_dir: spool, ..Default::default() });
    let ready = serde_json::json!({
        "url": format!("http://{host}:{}{}", api_l.local_addr()?.port(), s3_api::PREFIX),
        "s3": format!("http://{host}:{}", s3.local_addr()?.port()),
        "account": account,
        "anon_key": project.anon_key.expose(),
        "service_key": project.service_key.expose(),
        "jwt_secret": jwt_secret,
    });
    println!("{ready}");
    tokio::select! {
        r = s3_server::serve(s3, server) => r?,
        r = s3_api::serve(api_l, api) => r?,
        _ = tokio::signal::ctrl_c() => {}
    }
    store.close().await;
    Ok(())
}
