//! The ACP registry end to end (docs/code/agents.md, "The ACP registry"): a local HTTP server
//! plays the registry (`[registry] url` in the daemon's config.toml) and serves its archives; the
//! daemon lists it, installs agents from it while it runs (a `.tar.gz` and a `.zip` holding a tiny
//! ACP agent that answers `initialize`), refuses a bad checksum without keeping anything, and
//! removes them again.

mod common;

use agent_host::config::Config;
use agent_host::provider::Provider;
use agent_host::provider::fake::{FakeProvider, Script};
use agent_host::registry::{self, LIST_MAX_AGE, Registry};
use agent_host::{Daemon, Host};
use agent_proto::*;
use axum::extract::{Path as UrlPath, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// A tiny ACP agent: answers `initialize`, then waits.
const AGENT: &str = r#"#!/bin/sh
IFS= read -r line
id=$(printf '%s\n' "$line" | sed -n 's/^.*"id":[ ]*\([0-9][0-9]*\).*$/\1/p')
printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentCapabilities":{},"agentInfo":{"name":"tiny","title":"Tiny","version":"1.2.3"},"authMethods":[]}}\n' "$id"
cat >/dev/null
"#;

#[derive(Default)]
struct Served {
    /// `None`: the registry is down (503).
    registry: Mutex<Option<String>>,
    files: Mutex<HashMap<String, Vec<u8>>>,
    registry_hits: AtomicUsize,
}

async fn registry_json(State(s): State<Arc<Served>>) -> axum::response::Response {
    s.registry_hits.fetch_add(1, Ordering::SeqCst);
    match s.registry.lock().unwrap().clone() {
        Some(text) => ([("content-type", "application/json")], text).into_response(),
        None => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

async fn file(State(s): State<Arc<Served>>, UrlPath(name): UrlPath<String>) -> axum::response::Response {
    match s.files.lock().unwrap().get(&name).cloned() {
        Some(bytes) => bytes.into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

fn tar_gz(path: &str, body: &[u8]) -> Vec<u8> {
    let mut b = tar::Builder::new(flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast()));
    let mut h = tar::Header::new_gnu();
    h.set_size(body.len() as u64);
    h.set_mode(0o644);
    h.set_cksum();
    b.append_data(&mut h, path, body).unwrap();
    b.into_inner().unwrap().finish().unwrap()
}

fn zip_of(path: &str, body: &[u8]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).unix_permissions(0o755);
    w.start_file(path, opts).unwrap();
    w.write_all(body).unwrap();
    w.finish().unwrap().into_inner()
}

struct Env {
    home: tempfile::TempDir,
    served: Arc<Served>,
    base: String,
}

async fn env() -> Env {
    let served = Arc::new(Served::default());
    let app = axum::Router::new().route("/registry.json", get(registry_json)).route("/files/{name}", get(file)).with_state(served.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let target = registry::index::platform().expect("a platform the registry names");
    let tiny = tar_gz("bin/tiny-agent", AGENT.as_bytes());
    let zippy = zip_of("zippy/agent", AGENT.as_bytes());
    let sha = |b: &[u8]| hex::encode(Sha256::digest(b));
    let binary = |file: &str, cmd: &str, sha256: Option<String>| {
        let mut t = json!({ "archive": format!("{base}/files/{file}"), "cmd": cmd, "env": { "TINY_MODE": "acp", "TINY_API_KEY": "never" } });
        if let Some(h) = sha256 {
            t["sha256"] = json!(h);
        }
        json!({ target: t })
    };
    let index = json!({
        "version": "1.0.0",
        "agents": [
            { "id": "tiny", "name": "Tiny Agent", "version": "1.0.0", "description": "A tiny agent",
              "icon": "https://cdn.agentclientprotocol.com/registry/v1/latest/tiny.svg",
              "distribution": { "binary": binary("tiny.tar.gz", "./bin/tiny-agent", Some(sha(&tiny))) } },
            { "id": "zippy", "name": "Zippy", "version": "2.0.0",
              "distribution": { "binary": binary("zippy.zip", "zippy/agent", None) } },
            { "id": "badhash", "name": "Bad Hash", "version": "1.0.0",
              "distribution": { "binary": binary("tiny.tar.gz", "bin/tiny-agent", Some("0".repeat(64))) } },
            { "id": "github-copilot-cli", "name": "GitHub Copilot", "version": "1.0.105",
              "distribution": { "npx": { "package": "@github/copilot@1.0.105", "args": ["--acp"] } } },
            { "id": "elsewhere", "name": "Elsewhere", "version": "1.0.0",
              "distribution": { "binary": { "plan9-mips": { "archive": "https://example.com/a.tar.gz", "cmd": "a" } } } },
            { "id": "Broken", "name": "Dropped: not a registry id", "version": "1.0.0", "distribution": {} }
        ]
    });
    *served.registry.lock().unwrap() = Some(index.to_string());
    served.files.lock().unwrap().extend([("tiny.tar.gz".to_string(), tiny), ("zippy.zip".to_string(), zippy)]);

    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), format!("[registry]\nurl = \"{base}/registry.json\"\n")).unwrap();
    Env { home, served, base }
}

impl Env {
    fn config(&self) -> Config {
        Config::load_from(self.home.path().to_path_buf()).unwrap()
    }

    async fn host(&self) -> Arc<Host> {
        // A provider that is the registry's GitHub Copilot under roll's own name for it.
        let copilot = FakeProvider::with("acp:copilot", Capabilities::default(), Script::Echo);
        let host = Host::open(self.config(), vec![copilot as Arc<dyn Provider>]).await.unwrap();
        wait_info(&host, "the fake's probe", |i| i.providers.iter().any(|p| p.id.0 == "acp:copilot" && !p.checking)).await;
        host
    }

    fn installed_json(&self) -> serde_json::Value {
        serde_json::from_slice(&std::fs::read(self.home.path().join("acp/installed.json")).unwrap()).unwrap()
    }

    fn tools(&self, id: &str) -> PathBuf {
        self.home.path().join("acp/tools").join(id)
    }
}

async fn wait_info(host: &Arc<Host>, what: &str, f: impl Fn(&MachineInfo) -> bool) -> MachineInfo {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        let info = host.info().await;
        if f(&info) {
            return info;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}: {info:#?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn list(host: &Arc<Host>) -> RegistryList {
    match common::run(host, Command::RegistryList {}).await {
        Reply::Registry(l) => l,
        other => panic!("registry.list: {other:?}"),
    }
}

fn files_under(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.file_type().unwrap().is_dir() {
            out.extend(files_under(&e.path()));
        } else {
            out.push(e.path());
        }
    }
    out
}

#[tokio::test]
async fn lists_installs_and_removes_an_agent_without_a_restart() {
    let env = env().await;
    let host = env.host().await;

    let listed = list(&host).await;
    assert!(listed.error.is_none() && listed.fetched_at > 0, "{listed:?}");
    let ids: Vec<&str> = listed.agents.iter().map(|a| a.id.as_str()).collect();
    assert_eq!(ids, ["tiny", "zippy", "badhash", "github-copilot-cli", "elsewhere"], "the bad entry is dropped");
    let tiny = &listed.agents[0];
    assert_eq!((tiny.kinds.as_slice(), tiny.installable, tiny.integrity), (&[RegistryKind::Binary][..], true, RegistryIntegrity::Sha256));
    assert_eq!((tiny.installed.as_deref(), tiny.offered.as_ref()), (None, None));
    assert_eq!(listed.agents[1].integrity, RegistryIntegrity::None);
    let copilot = &listed.agents[3];
    assert_eq!(copilot.offered, Some(ProviderId("acp:copilot".into())), "a known agent under roll's own name");
    assert_eq!((copilot.kinds.as_slice(), copilot.integrity), (&[RegistryKind::Npx][..], RegistryIntegrity::Registry));
    let elsewhere = &listed.agents[4];
    assert!(elsewhere.kinds.is_empty() && !elsewhere.installable);

    // Installing: the reply comes once the files are in place, installed.json is written and the
    // provider is offered.
    assert_eq!(common::run(&host, Command::RegistryInstall { agent: "tiny".into() }).await, Reply::Ok);
    let target = registry::index::platform().unwrap();
    let command = env.tools("tiny").join("1.0.0").join(target).join("bin/tiny-agent");
    assert!(command.is_file(), "{}", command.display());
    let saved = env.installed_json();
    assert_eq!(saved[0]["id"], "tiny");
    assert_eq!(saved[0]["command"], command.display().to_string());
    assert_eq!(saved[0]["env"], json!({ "TINY_MODE": "acp" }), "a credential's variable never comes along");
    let info = host.info().await;
    assert!(info.providers.iter().any(|p| p.id.0 == "acp:tiny"), "offered at once: {:?}", info.providers);
    let installed = info.settings.as_ref().unwrap().registry.clone().unwrap();
    assert_eq!((installed[0].id.as_str(), installed[0].version.as_str(), installed[0].kind), ("tiny", "1.0.0", RegistryKind::Binary));
    // Its probe answers: it's the agent, running from where it was installed.
    let info = wait_info(&host, "tiny's probe", |i| i.providers.iter().any(|p| p.id.0 == "acp:tiny" && p.installed && !p.checking)).await;
    let p = info.providers.iter().find(|p| p.id.0 == "acp:tiny").unwrap();
    assert_eq!((p.name.as_str(), p.version.as_deref()), ("Tiny Agent", Some("1.2.3")));

    let listed = list(&host).await;
    assert_eq!(listed.agents[0].installed.as_deref(), Some("1.0.0"));
    assert_eq!(listed.agents[0].offered, Some(ProviderId("acp:tiny".into())));
    // Installing it again only offers it.
    assert_eq!(common::run(&host, Command::RegistryInstall { agent: "tiny".into() }).await, Reply::Ok);

    // The next daemon start offers it too.
    let next: Vec<String> = registry::acp_providers(&env.config()).iter().map(|p| p.id().0).collect();
    assert!(next.contains(&"acp:tiny".to_string()), "{next:?}");

    // An agent the machine has under another name isn't installed twice.
    let Reply::Error(e) = common::run(&host, Command::RegistryInstall { agent: "github-copilot-cli".into() }).await else { panic!() };
    assert_eq!(e.code, ErrorCode::Conflict, "{}", e.message);
    let Reply::Error(e) = common::run(&host, Command::RegistryInstall { agent: "nope".into() }).await else { panic!() };
    assert_eq!(e.code, ErrorCode::NotFound);

    // A zip.
    assert_eq!(common::run(&host, Command::RegistryInstall { agent: "zippy".into() }).await, Reply::Ok);
    wait_info(&host, "zippy's probe", |i| i.providers.iter().any(|p| p.id.0 == "acp:zippy" && p.installed && !p.checking)).await;

    // Removing.
    assert_eq!(common::run(&host, Command::RegistryRemove { agent: "tiny".into() }).await, Reply::Ok);
    assert!(!env.tools("tiny").exists());
    assert_eq!(env.installed_json().as_array().unwrap().len(), 1);
    let info = host.info().await;
    assert!(!info.providers.iter().any(|p| p.id.0 == "acp:tiny"));
    assert!(info.settings.unwrap().registry.unwrap().iter().all(|i| i.id != "tiny"));
    let Reply::Error(e) = common::run(&host, Command::RegistryRemove { agent: "tiny".into() }).await else { panic!() };
    assert_eq!(e.code, ErrorCode::NotFound);
    assert!(!registry::acp_providers(&env.config()).iter().any(|p| p.id().0 == "acp:tiny"));
}

#[tokio::test]
async fn a_bad_checksum_fails_and_leaves_nothing() {
    let env = env().await;
    let host = env.host().await;
    let Reply::Error(e) = common::run(&host, Command::RegistryInstall { agent: "badhash".into() }).await else { panic!() };
    assert_eq!(e.code, ErrorCode::Provider);
    assert!(e.message.contains("SHA-256"), "{}", e.message);
    assert!(files_under(&env.tools("badhash")).is_empty(), "{:?}", files_under(&env.tools("badhash")));
    assert!(!env.tools("badhash").exists());
    assert!(!env.home.path().join("acp/installed.json").exists());
    assert!(!host.info().await.providers.iter().any(|p| p.id.0 == "acp:badhash"));
    // A missing archive is a failure too.
    env.served.files.lock().unwrap().clear();
    let Reply::Error(e) = common::run(&host, Command::RegistryInstall { agent: "tiny".into() }).await else { panic!() };
    assert!(e.message.contains("404"), "{}", e.message);
    assert!(!env.tools("tiny").exists());
}

#[tokio::test]
async fn the_list_is_reused_for_a_while_and_kept_for_when_the_registry_is_down() {
    let env = env().await;
    let config = env.config();
    let fresh = Registry::new(&config);
    let first = fresh.index(Some(LIST_MAX_AGE)).await;
    assert!(first.error.is_none() && first.index.is_some());
    assert_eq!(env.served.registry_hits.load(Ordering::SeqCst), 1);
    fresh.index(Some(LIST_MAX_AGE)).await;
    assert_eq!(env.served.registry_hits.load(Ordering::SeqCst), 1, "reused within five minutes");
    fresh.index(None).await;
    assert_eq!(env.served.registry_hits.load(Ordering::SeqCst), 2, "an install always fetches");
    assert!(env.home.path().join("acp/registry.json").is_file());

    // Down: a daemon started now falls back to the copy, and says why.
    *env.served.registry.lock().unwrap() = None;
    let offline = Registry::new(&config).index(Some(LIST_MAX_AGE)).await;
    assert_eq!(offline.index.as_ref().map(|i| i.agents.len()), Some(5));
    assert!(offline.error.as_deref().is_some_and(|e| e.contains("503")), "{:?}", offline.error);
    assert!(offline.fetched_at > 0);
    // The daemon still installs from the copy.
    let host = env.host().await;
    let listed = list(&host).await;
    assert!(listed.error.is_some() && listed.agents.len() == 5);
    assert_eq!(common::run(&host, Command::RegistryInstall { agent: "zippy".into() }).await, Reply::Ok);

    // With no copy either, the list is empty and says why.
    std::fs::remove_file(env.home.path().join("acp/registry.json")).unwrap();
    let none = Registry::new(&config).index(Some(LIST_MAX_AGE)).await;
    assert!(none.index.is_none() && none.error.is_some() && none.fetched_at == 0);
    let _ = env.base;
}
