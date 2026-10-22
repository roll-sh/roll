//! A standalone roll storage server for conformance suites (ceph/s3-tests) and manual testing:
//! a store on a local directory plus the S3 API on one port, with three access keys.
//!
//! ```text
//! cargo run -p s3-server --example serve -- --port 59100 --dir /tmp/roll-s3 \
//!     [--host 127.0.0.1] [--conf s3tests.conf] [--website-domain web.example.com]
//! ```
//!
//! - `main`: an admin key covering every bucket (`rollmain` / `rollmainsecret0123456789`).
//! - `alt` and `tenant`: admin keys scoped to no bucket, so they stand in for other accounts:
//!   they see nothing of `main`'s buckets unless a policy or an ACL lets them in.
//!
//! `--conf` writes an s3-tests configuration (keys, the canonical owner and user ids) once the
//! server is listening; the file appearing means the server is ready.

use s3_server::{Config, Server};
use s3_store::{LocalBackend, Permission, Store, StoreConfig};
use std::path::PathBuf;
use std::sync::Arc;

const MAIN_KEY: &str = "rollmain";
const MAIN_SECRET: &str = "rollmainsecret0123456789";

struct Args {
    host: String,
    port: u16,
    dir: PathBuf,
    conf: Option<PathBuf>,
    website_domain: Option<String>,
}

fn args() -> anyhow::Result<Args> {
    let mut a = Args { host: "127.0.0.1".into(), port: 59100, dir: std::env::temp_dir().join("roll-s3-serve"), conf: None, website_domain: None };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or_else(|| anyhow::anyhow!("{flag} needs a value"));
        match flag.as_str() {
            "--host" => a.host = value()?,
            "--port" => a.port = value()?.parse()?,
            "--dir" => a.dir = value()?.into(),
            "--conf" => a.conf = Some(value()?.into()),
            "--website-domain" => a.website_domain = Some(value()?),
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    Ok(a)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let a = args()?;
    std::fs::create_dir_all(&a.dir)?;
    let backend = Arc::new(LocalBackend::new(&a.dir.join("blobs"))?);
    let mut scfg = StoreConfig::new(a.dir.join("meta"));
    scfg.lifecycle_interval = None;
    let store = Store::open(scfg, backend).await?;
    store.ensure_key(MAIN_KEY, MAIN_SECRET, "main").await?;
    // Other "accounts": admin keys that cover no bucket.
    let mut others = Vec::new();
    for name in ["alt", "tenant"] {
        let existing = store.keys()?.into_iter().find(|k| k.name == name);
        others.push(match existing {
            Some(k) => k,
            None => store.create_key(name, Permission::Admin, Some(Vec::new())).await?,
        });
    }
    let spool = a.dir.join("spool");
    std::fs::create_dir_all(&spool)?;
    let cfg = Config { spool_dir: spool, website_domain: a.website_domain.clone(), ..Config::default() };
    let listener = tokio::net::TcpListener::bind((a.host.as_str(), a.port)).await?;
    let server = Server::new(store.clone(), cfg);
    eprintln!("roll storage listening on http://{}:{} (main key {MAIN_KEY})", a.host, a.port);
    if let Some(path) = &a.conf {
        let (alt, tenant) = (&others[0], &others[1]);
        let conf = format!(
            "[DEFAULT]\nhost = {host}\nport = {port}\nis_secure = False\nssl_verify = False\n\n\
             [fixtures]\nbucket prefix = roll-{{random}}-\n\n\
             [s3 main]\ndisplay_name = roll\nuser_id = {owner}\nemail = main@example.com\napi_name = \naccess_key = {MAIN_KEY}\nsecret_key = {MAIN_SECRET}\n\n\
             [s3 alt]\ndisplay_name = roll\nuser_id = {alt_id}\nemail = alt@example.com\naccess_key = {alt_key}\nsecret_key = {alt_secret}\n\n\
             [s3 tenant]\ndisplay_name = roll\nuser_id = {tenant_id}\nemail = tenant@example.com\naccess_key = {tenant_key}\nsecret_key = {tenant_secret}\ntenant = testx\n\n\
             [iam]\nemail = iam@example.com\nuser_id = none\naccess_key = noiam\nsecret_key = noiamsecret\ndisplay_name = none\n\n\
             [iam root]\naccess_key = noiamroot\nsecret_key = noiamrootsecret\nuser_id = none\nemail = root@example.com\n\n\
             [iam alt root]\naccess_key = noiamaltroot\nsecret_key = noiamaltrootsecret\nuser_id = none\nemail = altroot@example.com\n",
            host = a.host,
            port = a.port,
            owner = store.owner_id(),
            alt_id = store.canonical_user(&alt.id),
            alt_key = alt.id,
            alt_secret = alt.secret,
            tenant_id = store.canonical_user(&tenant.id),
            tenant_key = tenant.id,
            tenant_secret = tenant.secret,
        );
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, conf)?;
        std::fs::rename(&tmp, path)?;
    }
    tokio::select! {
        r = s3_server::serve(listener, server) => r?,
        _ = tokio::signal::ctrl_c() => {}
    }
    store.close().await;
    Ok(())
}
