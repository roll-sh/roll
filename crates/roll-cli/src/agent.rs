//! `roll agent …`: connect this machine to roll code agents (docs/code/agents.md).
//!
//! `login` runs the device flow and keeps the machine token in `~/.config/roll/agent.json`
//! (0600; `ROLL_AGENT_CONFIG` names another file). `up` runs the daemon (`agent_host::Host`) with
//! the providers from its config and keeps the link to the relay up until Ctrl-C. `status` shows
//! the login and whether roll still accepts it; `logout` revokes the machine and forgets it.

use agent_host::provider::Provider;
use agent_host::relay::{self, LinkConfig, LinkEvent};
use anyhow::{Context, bail};
use clap::Subcommand;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[derive(Subcommand)]
pub enum Agent {
    /// Connect this machine to roll: shows a code to confirm in the dashboard.
    Login {
        /// What to call this machine (default: its hostname).
        #[arg(long)]
        name: Option<String>,
    },
    /// Run the agent daemon here (providers, threads, terminals) and keep it connected to roll.
    /// Logs go to stderr; Ctrl-C stops it.
    Up,
    /// Show this machine's login and whether roll accepts it.
    Status,
    /// Remove this machine from roll and forget its token.
    Logout,
}

/// What `login` stores.
#[derive(serde::Serialize, serde::Deserialize)]
struct Saved {
    api: String,
    machine: String,
    owner: String,
    name: String,
    token: String,
}

fn saved_path() -> anyhow::Result<PathBuf> {
    if let Some(p) = std::env::var_os("ROLL_AGENT_CONFIG").filter(|p| !p.is_empty()) {
        return Ok(PathBuf::from(p));
    }
    let base = match std::env::var_os("XDG_CONFIG_HOME").filter(|p| !p.is_empty()) {
        Some(x) => PathBuf::from(x),
        None => agent_host::config::user_home().context("HOME isn't set")?.join(".config"),
    };
    Ok(base.join("roll").join("agent.json"))
}

fn load() -> anyhow::Result<Saved> {
    let path = saved_path()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => bail!("this machine isn't connected to roll: run `roll agent login`"),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    serde_json::from_str(&text).with_context(|| format!("{} isn't a roll agent login", path.display()))
}

/// Write the login, readable only by this user (written aside and renamed into place).
fn save(s: &Saved) -> anyhow::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let path = saved_path()?;
    let dir = path.parent().context("the config path has no directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(".agent.json.{}", std::process::id()));
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(serde_json::to_string_pretty(s)?.as_bytes())?;
    f.sync_all()?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: the buffer is valid for its length; the name is NUL-terminated within it.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    match std::str::from_utf8(&buf[..end]) {
        Ok(h) if ok && !h.is_empty() => h.to_string(),
        _ => "machine".into(),
    }
}

async fn post(http: &reqwest::Client, url: &str, body: Value) -> anyhow::Result<Value> {
    let res = http.post(url).json(&body).send().await.with_context(|| format!("can't reach {url}"))?;
    let status = res.status();
    let v: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("{}", v["error"].as_str().map(str::to_string).unwrap_or_else(|| status.to_string()));
    }
    Ok(v)
}

pub async fn run(api: &str, json_out: bool, cmd: Agent) -> anyhow::Result<()> {
    match cmd {
        Agent::Login { name } => login(api, json_out, name).await,
        Agent::Up => up().await,
        Agent::Status => status(json_out).await,
        Agent::Logout => logout(json_out).await,
    }
}

async fn login(api: &str, json_out: bool, name: Option<String>) -> anyhow::Result<()> {
    let api = api.trim_end_matches('/').to_string();
    let http = reqwest::Client::new();
    let name = name.unwrap_or_else(hostname);
    let start: agent_proto::DeviceLogin = serde_json::from_value(post(&http, &format!("{api}/v1/code/agents/login"), json!({ "name": name })).await?)?;
    if json_out {
        println!("{}", serde_json::to_string(&start)?);
    } else {
        println!("To connect {name} to roll, open\n\n  {}\n\nand confirm the code {}", start.verification_uri, start.user_code);
    }
    let interval = Duration::from_secs(start.interval.clamp(1, 60) as u64);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(start.expires_in as u64 + 5);
    loop {
        tokio::time::sleep(interval).await;
        let polled = post(&http, &format!("{api}/v1/code/agents/login/poll"), json!({ "device_code": start.device_code })).await?;
        match serde_json::from_value(polled)? {
            agent_proto::LoginPoll::Pending if tokio::time::Instant::now() < deadline => continue,
            agent_proto::LoginPoll::Pending | agent_proto::LoginPoll::Expired => bail!("the code expired before it was confirmed: run `roll agent login` again"),
            agent_proto::LoginPoll::Denied => bail!("the login was denied"),
            agent_proto::LoginPoll::Approved { token, machine, owner } => {
                let saved = Saved { api: api.clone(), machine: machine.0, owner, name: name.clone(), token };
                let path = save(&saved)?;
                if json_out {
                    println!("{}", json!({ "status": "approved", "machine": saved.machine, "owner": saved.owner, "name": saved.name, "config": path }));
                } else {
                    println!("Connected {} to {} as {}. Run `roll agent up` to start it.", saved.name, saved.owner, saved.machine);
                }
                return Ok(());
            }
        }
    }
}

/// The providers the daemon's config describes: Codex and Claude Code (ACP agents once their
/// adapter lands).
fn providers(config: &agent_host::config::Config) -> Vec<Arc<dyn Provider>> {
    use agent_host::provider::{acp, claude::Claude, codex::Codex};
    use agent_proto::ProviderId;
    let mut all: Vec<Arc<dyn Provider>> =
        vec![Arc::new(Codex::new(config.provider(&ProviderId::codex()))), Arc::new(Claude::new(config.provider(&ProviderId::claude())))];
    // ACP agents: the configured ones, and any known agent installed here.
    all.extend(acp::agents(&config.acp).into_iter().map(|a| Arc::new(acp::Acp::new(a.name, a.config)) as Arc<dyn Provider>));
    all
}

async fn up() -> anyhow::Result<()> {
    let saved = load()?;
    let config = agent_host::config::Config::load()?;
    let home = config.home.clone();
    let providers = providers(&config);
    let host = agent_host::Host::open(config, providers).await.with_context(|| format!("opening the agent data in {}", home.display()))?;
    eprintln!("roll agent: {} ({}) for {}, data in {}", saved.name, saved.machine, saved.owner, home.display());
    let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(e) = rx.recv().await {
            match e {
                LinkEvent::Connecting => {}
                LinkEvent::Connected { owner, machine, .. } => eprintln!("roll agent: connected to roll as {owner}/{machine}"),
                LinkEvent::Disconnected { error, retry } => eprintln!("roll agent: {error}; retrying in {:.1} s", retry.as_secs_f64()),
            }
        }
    });
    let link = LinkConfig { api: saved.api.clone(), token: saved.token.clone() };
    let res = relay::run(host, link, events, stop_signal()).await;
    match res {
        Ok(()) => {
            eprintln!("roll agent: stopped");
            Ok(())
        }
        Err(e) => bail!("{e}"),
    }
}

/// Ctrl-C or SIGTERM.
async fn stop_signal() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => return std::future::pending().await,
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

async fn status(json_out: bool) -> anyhow::Result<()> {
    let saved = load()?;
    let check = relay::check(&LinkConfig { api: saved.api.clone(), token: saved.token.clone() }).await;
    let accepted = match &check {
        Ok(()) => "accepted".to_string(),
        Err(relay::LinkError::Revoked) => "revoked: run `roll agent login` again".to_string(),
        Err(e) => format!("unreachable: {e}"),
    };
    if json_out {
        println!("{}", json!({ "api": saved.api, "owner": saved.owner, "machine": saved.machine, "name": saved.name, "config": saved_path()?, "accepted": check.is_ok(), "status": accepted }));
    } else {
        println!("machine  {} ({})\nowner    {}\napi      {}\nconfig   {}\ntoken    {accepted}", saved.name, saved.machine, saved.owner, saved.api, saved_path()?.display());
    }
    match check {
        Ok(()) => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn logout(json_out: bool) -> anyhow::Result<()> {
    let saved = load()?;
    let res = reqwest::Client::new()
        .delete(format!("{}/v1/code/agents/connect", saved.api))
        .bearer_auth(&saved.token)
        .send()
        .await
        .with_context(|| format!("can't reach {}", saved.api))?;
    // 401: roll had already forgotten the token; forgetting it here too is all that's left.
    if !res.status().is_success() && res.status() != reqwest::StatusCode::UNAUTHORIZED {
        let status = res.status();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        bail!("removing the machine: {}", v["error"].as_str().map(str::to_string).unwrap_or_else(|| status.to_string()));
    }
    std::fs::remove_file(saved_path()?)?;
    if json_out {
        println!("{}", json!({ "status": "logged_out", "machine": saved.machine }));
    } else {
        println!("Removed {} ({}) from {}.", saved.name, saved.machine, saved.owner);
    }
    Ok(())
}
