//! A standalone roll storage server for conformance suites (ceph/s3-tests) and manual testing:
//! a store on a local directory plus the S3 API on one port, with three accounts.
//!
//! ```text
//! cargo run -p s3-server --example serve -- --port 59100 --dir /tmp/roll-s3 \
//!     [--host 127.0.0.1] [--conf s3tests.conf] [--website-domain web.example.com]
//! ```
//!
//! - `main`: the default account's root key (`rollmain` / `rollmainsecret0123456789`).
//! - `alt` and `tenant`: the root keys of two other accounts (with their own canonical ids,
//!   display names and emails), so cross-account tests are really cross-account.
//!
//! `--conf` writes an s3-tests configuration (keys, the accounts' canonical ids, display names
//! and emails) once the server is listening; the file appearing means the server is ready.

use s3_server::{Config, Server};
use s3_store::{AccountUpdate, LocalBackend, Permission, Store, StoreConfig};
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
    let main = store.update_account(&store.default_account_id(), AccountUpdate { name: Some("main".into()), email: Some(Some("main@example.com".into())) }).await?;
    store.ensure_key(MAIN_KEY, MAIN_SECRET, "main").await?;
    // The other accounts, each with its root key.
    let mut others = Vec::new();
    for name in ["alt", "tenant"] {
        let account = match store.accounts()?.into_iter().find(|a| a.name == name) {
            Some(a) => a,
            None => store.create_account(name, Some(&format!("{name}@example.com"))).await?,
        };
        let key = match store.keys_of(&account.id)?.into_iter().find(|k| k.is_root()) {
            Some(k) => k,
            None => store.create_key_in(&account.id, name, Permission::Admin, None).await?,
        };
        others.push((account, key));
    }
    let spool = a.dir.join("spool");
    std::fs::create_dir_all(&spool)?;
    let cfg = Config { spool_dir: spool, website_domain: a.website_domain.clone(), ..Config::default() };
    let listener = tokio::net::TcpListener::bind((a.host.as_str(), a.port)).await?;
    let server = Server::new(store.clone(), cfg);
    eprintln!("roll storage listening on http://{}:{} (main key {MAIN_KEY})", a.host, a.port);
    if let Some(path) = &a.conf {
        let ((alt, alt_key), (tenant, tenant_key)) = (&others[0], &others[1]);
        let conf = format!(
            "[DEFAULT]\nhost = {host}\nport = {port}\nis_secure = False\nssl_verify = False\n\n\
             [fixtures]\nbucket prefix = roll-{{random}}-\n\n\
             [s3 main]\ndisplay_name = {main_name}\nuser_id = {owner}\nemail = main@example.com\napi_name = \naccess_key = {MAIN_KEY}\nsecret_key = {MAIN_SECRET}\n\n\
             [s3 alt]\ndisplay_name = {alt_name}\nuser_id = {alt_id}\nemail = alt@example.com\naccess_key = {alt_key}\nsecret_key = {alt_secret}\n\n\
             [s3 tenant]\ndisplay_name = {tenant_name}\nuser_id = {tenant_id}\nemail = tenant@example.com\naccess_key = {tenant_key}\nsecret_key = {tenant_secret}\ntenant = testx\n\n\
             [iam]\nemail = iam@example.com\nuser_id = none\naccess_key = noiam\nsecret_key = noiamsecret\ndisplay_name = none\n\n\
             [iam root]\naccess_key = noiamroot\nsecret_key = noiamrootsecret\nuser_id = none\nemail = root@example.com\n\n\
             [iam alt root]\naccess_key = noiamaltroot\nsecret_key = noiamaltrootsecret\nuser_id = none\nemail = altroot@example.com\n",
            host = a.host,
            port = a.port,
            main_name = main.name,
            owner = main.canonical,
            alt_name = alt.name,
            alt_id = alt.canonical,
            alt_key = alt_key.id,
            alt_secret = alt_key.secret,
            tenant_name = tenant.name,
            tenant_id = tenant.canonical,
            tenant_key = tenant_key.id,
            tenant_secret = tenant_key.secret,
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
