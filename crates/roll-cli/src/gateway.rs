//! `roll gateway …`: roll's model gateway on a machine without `roll agent up`
//! (docs/code/gateway.md, "Standalone"). The machine that runs `roll agent up` hosts it from the
//! dashboard instead; this is the same gateway (`model_gateway::Gateway`) driven by a TOML file
//! (`model_gateway::config`).
//!
//! - `serve` runs it until Ctrl-C and applies the file again whenever it's saved (a change that
//!   doesn't parse or check is reported and the running settings stay).
//! - `client add|list|revoke` manage client keys in the same `gateway.db` (the file's
//!   `data_dir`), beside a running `serve` or without one: a revoked key is refused on its next
//!   request. `add` prints the key once; the gateway keeps only its hash.

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use model_gateway::config::{self, Reload};
use model_gateway::store::Store;
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Args)]
pub struct Gateway {
    /// The gateway's config file (TOML: its settings, and `[keys]` naming where each upstream's
    /// key comes from).
    #[arg(long, env = "ROLL_GATEWAY_CONFIG", global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the gateway from the config file, applying the file again when it's saved. Ctrl-C
    /// stops it (requests in flight finish).
    Serve,
    /// Client keys: one per machine or tool that sends requests through the gateway.
    #[command(subcommand)]
    Client(Client),
}

#[derive(Subcommand)]
enum Client {
    /// Make a client key and print it once (the gateway keeps only its hash).
    Add {
        /// What the key is for ("laptop", "ci").
        name: String,
        /// The roll machine it's for (`mch_…`): its usage is counted by it, and its earlier key
        /// is revoked.
        #[arg(long, value_name = "mch_…")]
        machine: Option<String>,
    },
    /// The client keys (never the keys themselves), with when each was last used.
    #[command(alias = "ls")]
    List,
    /// Revoke a client key: its next request is refused.
    #[command(alias = "rm")]
    Revoke {
        /// Its id (`gwc_…`).
        id: String,
    },
}

pub async fn run(json: bool, g: Gateway) -> anyhow::Result<()> {
    let Some(path) = g.config else { bail!("name the gateway's config file: --config <file> (or ROLL_GATEWAY_CONFIG)") };
    match g.cmd {
        Cmd::Serve => serve(&path).await,
        Cmd::Client(c) => client(&path, json, c),
    }
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn store(path: &Path) -> anyhow::Result<Store> {
    let dir = config::data_dir(path)?;
    Store::open(&dir.join("gateway.db"))
}

fn when(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(ms as i64).map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_else(|| "-".into())
}

fn client(path: &Path, json: bool, c: Client) -> anyhow::Result<()> {
    let store = store(path)?;
    match c {
        Client::Add { name, machine } => {
            let machine = match machine {
                Some(m) => {
                    let m = m.trim();
                    let ok = m.len() > 4 && m.len() <= 64 && m.starts_with("mch_") && m.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
                    if !ok {
                        bail!("--machine is a roll machine id (mch_…)");
                    }
                    Some(agent_proto::MachineId(m.to_string()))
                }
                None => None,
            };
            let (client, key) = store.issue(&name, machine, now())?;
            if json {
                println!("{}", json!({ "client": client, "key": key.expose() }));
            } else {
                eprintln!("roll gateway: made {} ({}); this is the only time its key is shown:", client.id, client.name);
                println!("{}", key.expose());
            }
        }
        Client::List => {
            let clients = store.clients()?;
            let used: std::collections::HashMap<String, u64> = store.last_used()?.into_iter().collect();
            if json {
                let rows: Vec<_> = clients.iter().map(|c| json!({ "client": c, "last_used_at": used.get(&c.id) })).collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if clients.is_empty() {
                eprintln!("roll gateway: no client keys yet (roll gateway client add <name>)");
            } else {
                println!("{:<21} {:<24} {:<24} {:<21} LAST USED", "ID", "NAME", "MACHINE", "CREATED");
                for c in &clients {
                    let machine = c.machine.as_ref().map_or("-", |m| m.as_str());
                    let last = used.get(&c.id).map_or_else(|| "never".into(), |t| when(*t));
                    println!("{:<21} {:<24} {:<24} {:<21} {last}", c.id, c.name, machine, when(c.created_at));
                }
            }
        }
        Client::Revoke { id } => {
            if !store.revoke(id.trim())? {
                bail!("no client {id}");
            }
            if json {
                println!("{}", json!({ "revoked": id }));
            } else {
                eprintln!("roll gateway: revoked {id}");
            }
        }
    }
    Ok(())
}

/// What a run state looks like on stderr.
fn say(run: &agent_proto::GatewayRun) {
    use agent_proto::GatewayRun;
    match run {
        GatewayRun::Running { addresses, .. } => {
            let urls: Vec<String> = addresses.iter().map(|a| format!("http://{a}")).collect();
            eprintln!("roll gateway: listening on {}", urls.join(", "));
        }
        GatewayRun::Failed { error, addresses } if addresses.is_empty() => eprintln!("roll gateway: can't listen: {error} (retrying)"),
        GatewayRun::Failed { error, addresses } => eprintln!("roll gateway: listening on {} only: {error} (retrying the rest)", addresses.join(", ")),
        GatewayRun::Stopped => eprintln!("roll gateway: not serving (enabled = false in the config)"),
    }
}

async fn serve(path: &Path) -> anyhow::Result<()> {
    let loaded = config::load(path)?;
    let data_dir = loaded.data_dir.clone();
    let gateway = model_gateway::Gateway::open(&data_dir).await.with_context(|| format!("opening the gateway in {}", data_dir.display()))?;
    eprintln!("roll gateway: {} upstream(s) from {}, data in {}", loaded.settings.upstreams.len(), path.display(), data_dir.display());
    let mut runs = gateway.watch_run();
    let mut shown = gateway.apply(loaded.settings, loaded.keys).await;
    say(&shown);

    let watcher = tokio::spawn(config::watch(path.to_path_buf(), loaded.stamp, config::INTERVAL, {
        let gateway = gateway.clone();
        let path = path.to_path_buf();
        move |reload| {
            let gateway = gateway.clone();
            let path = path.clone();
            let data_dir = data_dir.clone();
            async move {
                match reload {
                    Reload::Apply(l) => {
                        if l.data_dir != data_dir {
                            eprintln!("roll gateway: data_dir changed in {}: it takes effect on the next start", path.display());
                        }
                        eprintln!("roll gateway: {} changed: applying it", path.display());
                        gateway.apply(l.settings, l.keys).await;
                    }
                    Reload::Keep { why } => eprintln!("roll gateway: {} changed but can't be used, so the settings in use stay: {why}", path.display()),
                }
            }
        }
    }));

    let ctrl_c = tokio::signal::ctrl_c();
    tokio::pin!(ctrl_c);
    loop {
        tokio::select! {
            _ = &mut ctrl_c => break,
            changed = runs.changed() => {
                if changed.is_err() {
                    break;
                }
                let run = runs.borrow_and_update().clone();
                if run != shown {
                    say(&run);
                    shown = run;
                }
            }
        }
    }
    watcher.abort();
    eprintln!("roll gateway: stopping");
    gateway.stop().await;
    Ok(())
}
