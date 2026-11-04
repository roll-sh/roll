//! `roll agent …`: connect this machine to roll code agents (docs/code/agents.md).
//!
//! `login` runs the device flow (or takes a machine token minted in the dashboard,
//! `--machine-token`) and keeps the machine token in `~/.config/roll/agent.json`
//! (0600; `ROLL_AGENT_CONFIG` names another file). `up` runs the daemon (`agent_host::Host`) with
//! the providers from its config and keeps the link to the relay up until Ctrl-C; without a login
//! file it takes the machine token from `ROLL_AGENT_TOKEN` (a roll-hosted sandbox, whose machine
//! roll made), and either way it runs without that variable (see [`env_login`]). `status` shows
//! the login and whether roll still accepts it; `logout` revokes the machine and forgets it.
//!
//! `ROLL_AGENT_FAKE=1` is for tests only (`pnpm test:agents`): `up` offers the fake providers of
//! `agent_host::provider::fake` (`fake`, which plays a scripted turn with an approval and an edit,
//! and `echo`) instead of the real ones, so nothing calls a model.

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
        #[arg(long, conflicts_with = "machine_token")]
        name: Option<String>,
        /// A machine token made in the dashboard ("Add machine"): saved as is, no code to confirm.
        #[arg(long, env = "ROLL_AGENT_TOKEN", hide_env_values = true, value_name = "rlm_…")]
        machine_token: Option<String>,
    },
    /// Run the agent daemon here (providers, threads, terminals) and keep it connected to roll.
    /// Logs go to stderr; Ctrl-C stops it.
    Up,
    /// Show this machine's login and whether roll accepts it.
    Status,
    /// Remove this machine from roll and forget its token.
    Logout,
    /// The owner's automations: triggers, filters and steps that run agents (docs/code/automations.md).
    Automation(crate::automation::Automation),
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
        Agent::Login { machine_token: Some(token), .. } => login_with_token(api, json_out, &token).await,
        Agent::Login { name, machine_token: None } => login(api, json_out, name).await,
        Agent::Up => up(api).await,
        Agent::Status => status(json_out).await,
        Agent::Logout => logout(json_out).await,
        Agent::Automation(a) => crate::automation::run(api, json_out, a).await,
    }
}

/// Variables a sandbox's machine runs `up` with instead of a login file: roll created the
/// machine and handed its token to the sandbox.
const ENV_TOKEN: &str = "ROLL_AGENT_TOKEN";
const ENV_VARS: [&str; 4] = [ENV_TOKEN, "ROLL_AGENT_MACHINE", "ROLL_AGENT_OWNER", "ROLL_AGENT_NAME"];
/// The pipe `up` hands the login from [`ENV_VARS`] over on when it starts itself again without
/// them (`null`: there was no token to take).
const ENV_FD: &str = "ROLL_AGENT_LOGIN_FD";

/// The login from `ROLL_AGENT_TOKEN` (and `ROLL_API_URL`), when there's no login file.
///
/// The variables must not stay anywhere the providers and terminals `up` starts can read them:
/// not in the environment they inherit, and not in this process's own `/proc/<pid>/environ`,
/// which removing a variable doesn't change and every process of the user can read. So when any
/// of them is set, `up` writes what it took from them to a pipe and execs itself again without
/// them (the same pid), and the new process reads the login from the pipe ([`ENV_FD`]). The
/// token is never written to disk. With a login file, the file wins and the variables are only
/// dropped.
fn env_login(api: &str) -> anyhow::Result<Option<Saved>> {
    if let Some(fd) = std::env::var_os(ENV_FD) {
        // SAFETY: `up` reads this first thing, before it starts any task, resolver lookup or
        // child that could read the environment from another thread.
        unsafe { std::env::remove_var(ENV_FD) };
        return read_handed_over(&fd.to_string_lossy());
    }
    if ENV_VARS.iter().all(|k| std::env::var_os(k).is_none()) {
        return Ok(None);
    }
    let get = |k: &str| std::env::var(k).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let saved = match get(ENV_TOKEN) {
        Some(_) if saved_path()?.exists() => None,
        Some(token) => {
            if !token.starts_with("rlm_") {
                bail!("{ENV_TOKEN} isn't a machine token (rlm_…)");
            }
            Some(Saved {
                api: api.trim_end_matches('/').to_string(),
                machine: get("ROLL_AGENT_MACHINE").unwrap_or_else(|| "machine".into()),
                owner: get("ROLL_AGENT_OWNER").unwrap_or_else(|| "?".into()),
                name: get("ROLL_AGENT_NAME").unwrap_or_else(hostname),
                token,
            })
        }
        None => None,
    };
    Err(exec_without_env(&serde_json::to_vec(&saved)?))
}

/// Starts this program again with the same arguments, without [`ENV_VARS`], and `handed` on a
/// pipe named by [`ENV_FD`]. Returns only if that fails.
fn exec_without_env(handed: &[u8]) -> anyhow::Error {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    let mut fds = [0i32; 2];
    // SAFETY: `fds` has room for the two descriptors pipe(2) writes.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return anyhow::Error::new(std::io::Error::last_os_error()).context("handing the login over");
    }
    // SAFETY: pipe(2) just made both and nothing else owns them.
    let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    // The login is far smaller than a pipe's buffer, so this doesn't block.
    let mut w = std::fs::File::from(write);
    if let Err(e) = w.write_all(handed) {
        return anyhow::Error::new(e).context("handing the login over");
    }
    drop(w);
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(e) => return anyhow::Error::new(e).context("finding this program to start it again"),
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.args(std::env::args_os().skip(1)).env(ENV_FD, read.as_raw_fd().to_string());
    for k in ENV_VARS {
        cmd.env_remove(k);
    }
    // `read` has no close-on-exec flag (pipe(2) doesn't set one), so the new program inherits it.
    anyhow::Error::new(cmd.exec()).context("starting `roll agent up` again without ROLL_AGENT_TOKEN")
}

/// The login a previous exec of `up` handed over on pipe `fd`.
fn read_handed_over(fd: &str) -> anyhow::Result<Option<Saved>> {
    use std::io::Read;
    use std::os::fd::FromRawFd;
    let fd: i32 = fd.parse().ok().filter(|fd| *fd > 2).with_context(|| format!("{ENV_FD} isn't a descriptor"))?;
    // SAFETY: `exec_without_env` made this descriptor for us alone and passed its number here;
    // it was open across the exec and nothing else in this process uses it.
    let pipe = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut text = Vec::new();
    pipe.take(64 << 10).read_to_end(&mut text).context("reading the handed-over login")?;
    serde_json::from_slice(&text).context("reading the handed-over login")
}

async fn login(api: &str, json_out: bool, name: Option<String>) -> anyhow::Result<()> {
    let api = api.trim_end_matches('/').to_string();
    let http = reqwest::Client::new();
    let name = name.unwrap_or_else(hostname);
    // The dashboard shows hostname and OS before anyone approves ("Approve ada-mbp (macOS)?").
    let about = json!({ "name": name, "hostname": hostname(), "os": std::env::consts::OS });
    let start: agent_proto::DeviceLogin = serde_json::from_value(post(&http, &format!("{api}/v1/code/agents/login"), about).await?)?;
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
                return saved_login(json_out, Saved { api: api.clone(), machine: machine.0, owner, name: name.clone(), token });
            }
        }
    }
}

fn saved_login(json_out: bool, saved: Saved) -> anyhow::Result<()> {
    let path = save(&saved)?;
    if json_out {
        println!("{}", json!({ "status": "approved", "machine": saved.machine, "owner": saved.owner, "name": saved.name, "config": path }));
    } else {
        println!("Connected {} to {} as {}. Run `roll agent up` to start it.", saved.name, saved.owner, saved.machine);
    }
    Ok(())
}

/// `login --machine-token rlm_…`: ask roll which machine the token is, then save it.
async fn login_with_token(api: &str, json_out: bool, token: &str) -> anyhow::Result<()> {
    let api = api.trim_end_matches('/').to_string();
    let token = token.trim();
    if !token.starts_with("rlm_") {
        bail!("that isn't a machine token (they start with rlm_): make one under Add machine in the dashboard");
    }
    let res = reqwest::Client::new()
        .get(format!("{api}/v1/code/agents/connect"))
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("can't reach {api}"))?;
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        bail!("roll doesn't accept this machine token (it was revoked or never existed)");
    }
    if !res.status().is_success() {
        let status = res.status();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        bail!("checking the token: {}", v["error"].as_str().map(str::to_string).unwrap_or_else(|| status.to_string()));
    }
    let who: agent_proto::MachineLogin = res.json().await.context("checking the token: not a roll agents API")?;
    saved_login(json_out, Saved { api, machine: who.machine.id.0, owner: who.owner, name: who.machine.name, token: token.to_string() })
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

/// `ROLL_AGENT_FAKE=1` (tests only): scripted providers instead of the real ones.
fn fake_providers() -> Option<Vec<Arc<dyn Provider>>> {
    use agent_host::provider::fake::{FakeProvider, Script};
    if std::env::var("ROLL_AGENT_FAKE").ok().is_none_or(|v| v.is_empty() || v == "0") {
        return None;
    }
    let caps = agent_proto::Capabilities::default();
    Some(vec![FakeProvider::with("fake", caps, Script::Demo), FakeProvider::with("echo", caps, Script::Echo)])
}

async fn up(api: &str) -> anyhow::Result<()> {
    let saved = match env_login(api)? {
        Some(s) => s,
        None => load()?,
    };
    let config = agent_host::config::Config::load()?;
    let home = config.home.clone();
    let providers = match fake_providers() {
        Some(fake) => {
            eprintln!("roll agent: ROLL_AGENT_FAKE is set: offering only the test providers (fake, echo)");
            fake
        }
        None => providers(&config),
    };
    let host = agent_host::Host::open(config, providers).await.with_context(|| format!("opening the agent data in {}", home.display()))?;
    eprintln!("roll agent: {} ({}) for {}, data in {}", saved.name, saved.machine, saved.owner, home.display());
    let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(e) = rx.recv().await {
            match e {
                LinkEvent::Connecting => {}
                LinkEvent::Connected { owner, machine, .. } => eprintln!("roll agent: connected to roll as {owner}/{machine}"),
                LinkEvent::Disconnected { error, retry } => eprintln!("roll agent: {error}; retrying in {:.1} s", retry.as_secs_f64()),
                LinkEvent::Notice { message } => eprintln!("roll agent: {message}"),
            }
        }
    });
    let link = LinkConfig { api: saved.api.clone(), token: saved.token.clone() };
    // On Ctrl-C or SIGTERM (`docker stop`, a sandbox going to sleep) the daemon stops its work
    // first (runs waiting on their user stay waiting; providers write their conversations out),
    // then the link sends the last records before it closes.
    let stopping = host.clone();
    let shutdown = async move {
        stop_signal().await;
        eprintln!("roll agent: stopping");
        if tokio::time::timeout(SHUTDOWN, stopping.shutdown()).await.is_err() {
            eprintln!("roll agent: providers took over {} s to stop; leaving them", SHUTDOWN.as_secs());
        }
    };
    let res = relay::run(host, link, events, shutdown).await;
    match res {
        Ok(()) => {
            eprintln!("roll agent: stopped");
            Ok(())
        }
        Err(e) => bail!("{e}"),
    }
}

/// How long a stopping daemon gives its runs and providers, before the link's last records
/// (`relay::DRAIN`): within `docker stop`'s 10 s.
const SHUTDOWN: std::time::Duration = std::time::Duration::from_secs(5);

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
