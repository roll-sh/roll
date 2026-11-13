//! roll apps from the command line (docs/apps/README.md): `roll up`, `roll deploy`, `roll logs`,
//! `roll domains` and `roll apps …`, all on `/v1/apps` (docs/apps/http-api.md).
//!
//! A directory remembers its service in `.roll/app.json` (roll's local state directory, never
//! committed), so `roll up`, `roll logs` and the rest need no flags after the first `roll up`.

use crate::{Api, s};
use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Which service and environment a command acts on.
#[derive(Args, Clone, Debug, Default)]
pub struct Target {
    /// The service: an id (`svc_…`) or a name (default: the one this directory is linked to).
    #[arg(long, short = 's')]
    pub service: Option<String>,
    /// The workspace project, to tell services of the same name apart (and for new services).
    #[arg(long, short = 'p')]
    pub project: Option<String>,
    /// The environment.
    #[arg(long, short = 'e', default_value = "production")]
    pub env: String,
}

#[derive(Args)]
pub struct Up {
    /// The directory to deploy (default: this one).
    pub dir: Option<PathBuf>,
    #[command(flatten)]
    pub target: Target,
    /// Don't follow the build: print the deployment and return.
    #[arg(long, short = 'd')]
    pub detach: bool,
    /// What the deployment is (shown in the dashboard).
    #[arg(long, short = 'm')]
    pub message: Option<String>,
    /// Every service and database the directory's roll.toml declares (`[apps.<name>]`,
    /// `[databases.<name>]`): made where missing, linked, and deployed. The plan is printed
    /// first.
    #[arg(long)]
    pub all: bool,
    /// With `--all`: print the plan, change nothing.
    #[arg(long, requires = "all")]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct Deploy {
    #[command(flatten)]
    pub target: Target,
    /// The commit to deploy (a repo service; default: its branch's head).
    #[arg(long)]
    pub revision: Option<String>,
    /// Deploy this public image instead of the service's source.
    #[arg(long)]
    pub image: Option<String>,
    /// Build and health-check it without putting it live: it answers on its own URL
    /// (`<id>--<host>`), and `roll apps rollback <id>` puts it live.
    #[arg(long)]
    pub no_traffic: bool,
    #[arg(long, short = 'd')]
    pub detach: bool,
}

#[derive(Args)]
pub struct Logs {
    #[command(flatten)]
    pub target: Target,
    /// Keep following.
    #[arg(long, short = 'f')]
    pub follow: bool,
    /// Lines from the end.
    #[arg(long, short = 'n', default_value_t = 200)]
    pub tail: usize,
    /// A deployment's build log instead of the runtime log (`latest` for the newest).
    #[arg(long, short = 'b')]
    pub build: Option<String>,
    /// Only lines that contain this (any case), searched back through the stored logs.
    #[arg(long, short = 'g')]
    pub grep: Option<String>,
    /// Only lines from this long ago on: `30m`, `2h`, `7d`.
    #[arg(long)]
    pub since: Option<String>,
    /// The requests instead (the proxy's access log): method, path, status, time.
    #[arg(long, conflicts_with = "build")]
    pub http: bool,
    /// With `--http`: only these statuses (`500`, `5xx`, `400-499`).
    #[arg(long, requires = "http")]
    pub status: Option<String>,
    /// With `--http`: only requests that took at least this many milliseconds.
    #[arg(long, requires = "http")]
    pub min_ms: Option<u64>,
}

#[derive(Subcommand)]
pub enum Domains {
    /// The service's hostnames: its own and its custom domains.
    #[command(alias = "ls")]
    List {
        #[command(flatten)]
        target: Target,
    },
    /// Add a custom domain and print the DNS record it needs.
    Add {
        hostname: String,
        #[command(flatten)]
        target: Target,
    },
    /// Check a custom domain's DNS now.
    Verify {
        hostname: String,
        #[command(flatten)]
        target: Target,
    },
    /// Remove a custom domain.
    #[command(alias = "rm")]
    Remove {
        hostname: String,
        #[command(flatten)]
        target: Target,
    },
    /// Make a custom domain redirect to another of the environment's hostnames (308), or serve
    /// again with `--to none`.
    Redirect {
        hostname: String,
        #[arg(long)]
        to: String,
        #[command(flatten)]
        target: Target,
    },
}

#[derive(Subcommand)]
// Parsed once per run: `set`'s many flags make it big, and boxing them would only obscure clap.
#[allow(clippy::large_enum_variant)]
pub enum AppsCmd {
    /// Services, with each environment's state and URL.
    #[command(alias = "ls")]
    List {
        #[arg(long, short = 'p')]
        project: Option<String>,
    },
    /// A service: settings, environments, instances.
    Show {
        #[command(flatten)]
        target: Target,
    },
    /// Make a service from a repo (`--repo owner/name`) or an image (`--image`).
    Create {
        name: String,
        #[arg(long, short = 'p')]
        project: String,
        #[arg(long, conflicts_with = "image")]
        repo: Option<String>,
        /// A subdirectory of the repo to build.
        #[arg(long)]
        root: Option<String>,
        #[arg(long)]
        image: Option<String>,
        /// web (default), worker or cron.
        #[arg(long = "type")]
        kind: Option<String>,
        /// The port the app listens on.
        #[arg(long)]
        port: Option<u16>,
        /// A cron service's schedule (five fields, in `--timezone`, else UTC).
        #[arg(long)]
        schedule: Option<String>,
        /// The zone a cron schedule's fields are in (IANA: `Europe/Budapest`); `UTC` for UTC.
        #[arg(long)]
        timezone: Option<String>,
    },
    /// Change run settings: `roll apps set --min 1 --max 5 --cpu 1 --memory 1024`.
    Set {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        min: Option<u32>,
        #[arg(long)]
        max: Option<u32>,
        #[arg(long)]
        cpu: Option<f64>,
        #[arg(long)]
        memory: Option<u32>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        start: Option<String>,
        /// Run once per deploy before the switch (a migration); a failure stops the deploy.
        #[arg(long)]
        pre_deploy: Option<String>,
        #[arg(long)]
        health: Option<String>,
        #[arg(long)]
        schedule: Option<String>,
        /// The zone a cron schedule's fields are in (IANA: `Europe/Budapest`).
        #[arg(long)]
        timezone: Option<String>,
        /// `web`, `worker` or `cron`.
        #[arg(long = "type", value_name = "TYPE")]
        kind: Option<String>,
        /// Requests one instance takes at once before another starts.
        #[arg(long)]
        concurrency: Option<u32>,
        /// Idle seconds before an instance is suspended (0: never).
        #[arg(long, value_name = "SECONDS")]
        suspend_after: Option<u32>,
        /// Idle seconds before it's stopped.
        #[arg(long, value_name = "SECONDS")]
        sleep_after: Option<u32>,
        /// A cron run's limit, in seconds.
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<u32>,
        /// A volume, `<mount>:<size in GB>` (`/data:5`); `none` removes it.
        #[arg(long)]
        volume: Option<String>,
        /// Deploy a push only when it touches these paths (comma-separated, gitignore-style);
        /// `none`: every push.
        #[arg(long)]
        watch: Option<String>,
        /// A gradual rollout: the shares of requests a new deployment takes in turn (`10,50`);
        /// `none` switches at once.
        #[arg(long, value_name = "PERCENTS")]
        rollout: Option<String>,
        /// How long each rollout step lasts.
        #[arg(long, value_name = "SECONDS")]
        rollout_step: Option<u32>,
        /// A step whose 5xx share reaches this (and beats the old deployment's) rolls back.
        #[arg(long, value_name = "PERCENT")]
        rollout_errors: Option<u8>,
        /// The app speaks HTTP/2 without TLS (a gRPC server): `true` or `false`.
        #[arg(long)]
        h2c: Option<bool>,
        /// A worker follows a stream group's lag: `<stream>:<group>[:<per instance>]`, or `none`.
        #[arg(long, value_name = "STREAM:GROUP[:PER]")]
        scale: Option<String>,
        /// Public URLs (`true`), or only the project's private network (`false`).
        #[arg(long)]
        public: Option<bool>,
        /// A replaced instance's time to finish (its requests, then SIGTERM), 1–300 seconds;
        /// `0` goes back to 30.
        #[arg(long, value_name = "SECONDS")]
        drain: Option<u32>,
        /// A web service also scales on CPU: another instance while they average over this
        /// percent for a minute (10–100); `0` turns it off.
        #[arg(long, value_name = "PERCENT")]
        cpu_target: Option<u32>,
        /// A worker that stops: `always` started again, `on_failure`, or `never`.
        #[arg(long, value_parser = ["always", "on_failure", "never"])]
        restart: Option<String>,
        /// With `--restart on_failure`: failures in a row before it's left stopped (default 10).
        #[arg(long)]
        restart_tries: Option<u32>,
        /// A web service keeps each visitor on one instance while it's up (`true`), or not.
        #[arg(long)]
        affinity: Option<bool>,
    },
    /// Delete a service: its environments, instances, deployments and images (asks first;
    /// `--yes` doesn't).
    Delete {
        #[command(flatten)]
        target: Target,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Deployments, newest first.
    Deploys {
        #[command(flatten)]
        target: Target,
    },
    /// Reach the service's port from this machine, a private one too (Fly's `fly proxy`):
    /// connections to `127.0.0.1:<local port>` go to it through roll, a tunnel each, waking it
    /// and keeping it up while they're open. Ctrl-C ends it.
    Proxy {
        #[command(flatten)]
        target: Target,
        /// The port here (default: the service's own).
        #[arg(long)]
        local_port: Option<u16>,
    },
    /// A shell in a running instance (a web service asleep is woken and kept up while it's
    /// open); leave with `exit` or Ctrl-D.
    Ssh {
        #[command(flatten)]
        target: Target,
        /// The instance (default: one that's running, or a woken one).
        #[arg(long)]
        instance: Option<String>,
    },
    /// Run a one-off command in a fresh container of the live deployment, with its variables
    /// and on its network (no port, no volume): `roll apps run -- npx prisma db seed`. Its output
    /// streams here; the CLI exits with the command's exit code.
    Run {
        #[command(flatten)]
        target: Target,
        /// The job's limit, in seconds (default 3600).
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Run the live image again with the current variables and settings (no build, same
    /// revision): how changed variables reach a `roll up` service.
    Redeploy {
        #[command(flatten)]
        target: Target,
    },
    /// Deploy what another environment runs here (its image: no build), with this
    /// environment's variables: `roll apps promote --from staging` puts staging's image live in
    /// production.
    Promote {
        /// The environment whose live deployment is promoted.
        #[arg(long)]
        from: String,
        #[command(flatten)]
        target: Target,
    },
    /// Roll back to an earlier deployment (no build: its image runs again); with no id, the one
    /// that was live before the current one (`heroku rollback`).
    Rollback {
        deployment: Option<String>,
        #[command(flatten)]
        target: Target,
        /// Keep today's variables (by default they come back as they were when it was live).
        #[arg(long)]
        keep_variables: bool,
    },
    /// Variables: list (masked), `--reveal`, or change with `KEY=value` and `-u KEY`.
    Vars {
        #[command(flatten)]
        target: Target,
        /// KEY=value pairs to set.
        set: Vec<String>,
        /// Names to remove.
        #[arg(long, short = 'u')]
        unset: Vec<String>,
        /// Print the values.
        #[arg(long)]
        reveal: bool,
        /// Set every `KEY=value` line of a `.env` file (`export`, comments and quotes as in a
        /// shell's).
        #[arg(long, value_name = "PATH")]
        from_file: Option<PathBuf>,
    },
    /// Link a roll resource (`postgres <project>`, `redis <db>`): its connection variables go in.
    Link {
        kind: String,
        r#ref: String,
        /// Prefix the variables (`ORDERS` → `ORDERS_DATABASE_URL`).
        #[arg(long)]
        prefix: Option<String>,
        #[command(flatten)]
        target: Target,
    },
    /// Take a link out: its variables leave the next deploy.
    Unlink {
        kind: String,
        r#ref: String,
        #[command(flatten)]
        target: Target,
    },
    /// Stop a queued or running deploy (the live one keeps serving).
    Cancel {
        deployment: String,
        #[command(flatten)]
        target: Target,
    },
    /// An image service's private registry login: `--username` with the password or token on
    /// stdin, or `--remove`. Sealed, sent only to that image's registry, never shown again.
    Registry {
        #[command(flatten)]
        target: Target,
        #[arg(long, conflicts_with = "remove")]
        username: Option<String>,
        #[arg(long)]
        remove: bool,
    },
    /// Scale to zero now.
    Sleep {
        #[command(flatten)]
        target: Target,
    },
    /// Start an instance now.
    Wake {
        #[command(flatten)]
        target: Target,
    },
    /// Replace the instances with fresh ones (the same deployment).
    Restart {
        #[command(flatten)]
        target: Target,
    },
    /// Requests, latency, CPU, memory and cold starts over the last hour.
    Metrics {
        #[command(flatten)]
        target: Target,
        #[arg(long, default_value_t = 60)]
        minutes: i64,
        /// The last `1h`, `24h` or `7d` instead of `--minutes`.
        #[arg(long)]
        range: Option<String>,
    },
    /// Who changed what in the service (settings, variables by name, domains, links), newest
    /// first.
    Activity {
        #[command(flatten)]
        target: Target,
    },
    /// A Compose file as roll.toml's `[apps]` and `[databases]` (Railway's Compose import):
    /// printed, or appended to ./roll.toml with `--write`; `roll up --all` applies it.
    ImportCompose {
        /// The Compose file (default: compose.yaml, compose.yml, docker-compose.yml here).
        file: Option<PathBuf>,
        #[arg(long)]
        write: bool,
    },
    /// The environment's deploy hook: a new secret URL that deploys it (printed once; the old
    /// one stops working), or `--remove`.
    Hook {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        remove: bool,
    },
    /// The environment's volume backups (daily, the newest seven kept): list them, `--now` to
    /// back up now, or `--restore <id>` to put the volume back as it was then (asks first;
    /// `--yes` doesn't).
    Backups {
        #[command(flatten)]
        target: Target,
        #[arg(long, conflicts_with = "restore")]
        now: bool,
        #[arg(long, value_name = "ID")]
        restore: Option<String>,
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// The project's log drains: list them, `--add <https url>` (prints its signing secret
    /// once), or `--remove <id>`.
    Drains {
        #[command(flatten)]
        target: Target,
        #[arg(long, conflicts_with = "remove")]
        add: Option<String>,
        #[arg(long)]
        remove: Option<String>,
    },
    /// The project's notification targets: list them, `--add <url>` (`--kind slack|discord|webhook`,
    /// a webhook's secret printed once), or `--remove <id>`.
    Notifications {
        #[command(flatten)]
        target: Target,
        #[arg(long, conflicts_with = "remove")]
        add: Option<String>,
        #[arg(long, default_value = "webhook")]
        kind: String,
        #[arg(long)]
        remove: Option<String>,
    },
    /// The project's spend limits this month: show them, or set `--alert` and `--hard` (dollars;
    /// `0` removes one). At the hard limit its services stop until it's raised.
    Limits {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        alert: Option<f64>,
        #[arg(long)]
        hard: Option<f64>,
    },
    /// This month's use and what it costs.
    Usage {
        #[command(flatten)]
        target: Target,
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// Make or change an environment (`--branch` deploys on pushes to it; `--variables-from`
    /// starts a new one with another's variables).
    Env {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        branch: Option<String>,
        /// Stop deploying on pushes.
        #[arg(long, conflicts_with = "branch")]
        no_branch: bool,
        #[arg(long)]
        variables_from: Option<String>,
        /// Deploy a push only once its commit's checks pass (`true`), or at once (`false`).
        #[arg(long)]
        wait_for_ci: Option<bool>,
        /// This environment's own size or scale over the service's, `field=value` (`min_instances`,
        /// `max_instances`, `cpu`, `memory_mb`, `concurrency`, `suspend_after_s`,
        /// `sleep_after_s`); `field=` goes back to the service's. Repeatable.
        #[arg(long = "run", value_name = "FIELD=VALUE")]
        run: Vec<String>,
        /// An image service: deploy when its tag points at a new image (`true`), or not.
        #[arg(long)]
        follow_tag: Option<bool>,
        /// Only visitors with its link get in (a new link, printed once; the old one stops).
        #[arg(long, conflicts_with = "open")]
        private: bool,
        /// Anyone with its URL gets in again.
        #[arg(long)]
        open: bool,
        /// Maintenance: its public names answer roll's page (with this note, if given) and
        /// nothing wakes.
        #[arg(long, value_name = "NOTE", num_args = 0..=1, default_missing_value = "", conflicts_with = "serve")]
        maintenance: Option<String>,
        /// End maintenance.
        #[arg(long)]
        serve: bool,
        /// Only this network gets in through its public URLs (`a.b.c.d/n`, an IPv6 `/n`, an
        /// address). Repeatable; the list replaces the one it has.
        #[arg(long = "allow-ip", value_name = "CIDR", conflicts_with = "allow_any")]
        allow_ip: Vec<String>,
        /// Anyone gets in again.
        #[arg(long)]
        allow_any: bool,
        /// Each client's requests a minute through its public URLs, `N` or `N:BURST`.
        #[arg(long, value_name = "N[:BURST]", conflicts_with = "no_rate_limit")]
        rate_limit: Option<String>,
        /// No limit per client.
        #[arg(long)]
        no_rate_limit: bool,
    },
    /// Tear an environment down (asks first; `--yes` doesn't).
    EnvDelete {
        #[command(flatten)]
        target: Target,
        #[arg(long, short = 'y')]
        yes: bool,
    },
}

const LINK: &str = ".roll/app.json";

fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

fn read_link(dir: &Path) -> Option<Value> {
    std::fs::read(dir.join(LINK)).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn write_link(dir: &Path, v: &Value) -> anyhow::Result<()> {
    let path = dir.join(LINK);
    std::fs::create_dir_all(path.parent().expect("a parent"))?;
    std::fs::write(&path, serde_json::to_vec_pretty(v)?)?;
    // `.roll/` is local state: keep it out of git without touching the repo's .gitignore.
    let ignore = dir.join(".roll").join(".gitignore");
    if !ignore.exists() {
        let _ = std::fs::write(ignore, "*\n");
    }
    Ok(())
}

/// The project id `p` names (an id, or a project's name).
async fn project_id(api: &Api, p: &str) -> anyhow::Result<String> {
    if p.starts_with("prj_") {
        return Ok(p.to_string());
    }
    let all = api.get("/v1/workspace/projects").await?;
    all.as_array()
        .into_iter()
        .flatten()
        .find(|x| x["name"] == p)
        .map(|x| s(&x["id"]))
        .ok_or_else(|| anyhow::anyhow!("no project named {p} (roll apps puts services in workspace projects)"))
}

/// The service a target names: `--service` (id or name), else this directory's link.
async fn service(api: &Api, t: &Target) -> anyhow::Result<Value> {
    let key = match &t.service {
        Some(k) => k.clone(),
        None => match read_link(&std::env::current_dir()?) {
            Some(l) => s(&l["service"]),
            None => bail!("which service? pass --service, or run `roll up` here first"),
        },
    };
    if key.starts_with("svc_") {
        return api.get(&format!("/v1/apps/services/{}", enc(&key))).await;
    }
    let path = match &t.project {
        Some(p) => format!("/v1/apps/services?project={}", enc(&project_id(api, p).await?)),
        None => "/v1/apps/services".into(),
    };
    let all = api.get(&path).await?;
    let found: Vec<&Value> = all.as_array().into_iter().flatten().filter(|x| x["name"] == key.as_str()).collect();
    match found.as_slice() {
        [one] => Ok((*one).clone()),
        [] => bail!("no service named {key}"),
        _ => bail!("{} services are named {key}: pass --project", found.len()),
    }
}

fn env_path(svc: &Value, env: &str) -> String {
    format!("/v1/apps/services/{}/environments/{}", enc(&s(&svc["id"])), enc(env))
}

/// Pack a directory as a `roll-bundle-1` (what the server unpacks). Left out: what `.gitignore`
/// (also a parent's, as in a monorepo), `.dockerignore` and `.rollignore` leave out, build output
/// that never belongs in a build, and every `.env` file but a template: an upload is kept with its
/// deployment, so a developer's local secrets must never be in it.
fn pack(dir: &Path) -> anyhow::Result<Vec<u8>> {
    let staging = tempfile::tempdir()?;
    let mut files = 0;
    let mut skipped_env = Vec::new();
    for entry in walk(dir) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(dir)?;
        if rel.as_os_str().is_empty() {
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if secret_env_file(&name) {
            skipped_env.push(rel.display().to_string());
            continue;
        }
        let ty = entry.file_type().context("a walked entry has a type")?;
        let dest = staging.path().join(rel);
        if ty.is_dir() {
            std::fs::create_dir(&dest)?;
        } else if ty.is_file() {
            std::fs::copy(entry.path(), &dest)?;
            files += 1;
        } else if ty.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &dest)?;
        }
    }
    if !skipped_env.is_empty() {
        eprintln!("not uploaded: {} (set variables with `roll apps vars --from-file .env` instead)", skipped_env.join(", "));
    }
    if files == 0 {
        bail!("{} has nothing to deploy", dir.display());
    }
    let mut out = Vec::new();
    roll_fetch::pack(&mut out, &Default::default(), Some(staging.path()), roll_fetch::Limits::default())?;
    Ok(out)
}

/// The tree `roll up` sends, parents before children, symlinks not followed.
fn walk(dir: &Path) -> ignore::Walk {
    const SKIP: &[&str] = &[".git", ".roll", "node_modules", "target", ".venv", "__pycache__", ".next", ".turbo", ".DS_Store"];
    ignore::WalkBuilder::new(dir)
        .hidden(false)
        .require_git(false)
        .git_global(false)
        .add_custom_ignore_filename(".dockerignore")
        .add_custom_ignore_filename(".rollignore")
        .filter_entry(|e| !SKIP.iter().any(|s| e.file_name() == *s))
        .sort_by_file_name(|a, b| a.cmp(b))
        .build()
}

/// `.env`, `.env.local`, `.env.production`…, but not `.env.example`, `.env.sample` or
/// `.env.template`, which hold names, not values.
fn secret_env_file(name: &str) -> bool {
    match name.strip_prefix(".env") {
        Some("") => true,
        Some(rest) => rest.starts_with('.') && ![".example", ".sample", ".template"].contains(&rest),
        None => false,
    }
}

/// `…/tunnel` for each connection to `127.0.0.1:local`: its bytes copied both ways.
async fn proxy(api: &Api, env: &str, what: &str, port: u16, local: u16) -> anyhow::Result<()> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let l = tokio::net::TcpListener::bind(("127.0.0.1", local)).await.map_err(|e| anyhow::anyhow!("127.0.0.1:{local} is taken ({e}): pick another with --local-port"))?;
    let base = api.base.strip_prefix("http").map(|rest| format!("ws{rest}")).unwrap_or_else(|| api.base.clone());
    let url = format!("{base}{env}/tunnel");
    let mut headers: Vec<(String, String)> = api.headers.clone();
    if let Some(k) = &api.key {
        headers.push(("authorization".into(), format!("Bearer {k}")));
    }
    eprintln!("127.0.0.1:{local} → {what}:{port} (Ctrl-C ends it)");
    loop {
        let (conn, _) = tokio::select! {
            a = l.accept() => a?,
            _ = tokio::signal::ctrl_c() => return Ok(()),
        };
        let mut req = url.as_str().into_client_request()?;
        for (k, v) in &headers {
            req.headers_mut().insert(reqwest::header::HeaderName::from_bytes(k.as_bytes())?, v.parse()?);
        }
        tokio::spawn(async move {
            if let Err(e) = carry(req, conn).await {
                eprintln!("a connection ended: {e:#}");
            }
        });
    }
}

/// One local connection over one tunnel, until either side closes.
async fn carry(req: tokio_tungstenite::tungstenite::handshake::client::Request, conn: tokio::net::TcpStream) -> anyhow::Result<()> {
    use futures::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::{Error as WsError, Message};
    let (ws, _) = tokio_tungstenite::connect_async(req).await.map_err(|e| match e {
        WsError::Http(r) => anyhow::anyhow!("{}: {}", r.status(), String::from_utf8_lossy(r.body().as_deref().unwrap_or_default())),
        e => anyhow::anyhow!("{e}"),
    })?;
    let (mut tx, mut rx) = ws.split();
    let (mut rd, mut wr) = conn.into_split();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        tokio::select! {
            n = rd.read(&mut buf) => match n? {
                0 => break,
                n => tx.send(Message::Binary(buf[..n].to_vec().into())).await?,
            },
            m = rx.next() => match m {
                Some(Ok(Message::Binary(b))) => wr.write_all(&b).await?,
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
        }
    }
    let _ = tx.send(Message::Close(None)).await;
    Ok(())
}

/// A shell over `…/shell`: the terminal raw, keystrokes and the screen copied both ways, the
/// window's size sent when it changes.
async fn ssh(api: &Api, env: &str, instance: Option<&str>) -> anyhow::Result<()> {
    use futures::{SinkExt, StreamExt};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_tungstenite::tungstenite::{Error as WsError, Message, client::IntoClientRequest};
    let (cols, rows) = term::size().unwrap_or((80, 24));
    let mut path = format!("{env}/shell?cols={cols}&rows={rows}");
    if let Some(i) = instance {
        path.push_str(&format!("&instance={}", enc(i)));
    }
    let base = api.base.strip_prefix("http").map(|rest| format!("ws{rest}")).unwrap_or_else(|| api.base.clone());
    let mut req = format!("{base}{path}").into_client_request()?;
    if let Some(k) = &api.key {
        req.headers_mut().insert("authorization", format!("Bearer {k}").parse()?);
    }
    for (k, v) in &api.headers {
        req.headers_mut().insert(reqwest::header::HeaderName::from_bytes(k.as_bytes())?, v.parse()?);
    }
    let (ws, _) = tokio_tungstenite::connect_async(req).await.map_err(|e| match e {
        WsError::Http(r) => anyhow::anyhow!("{}: {}", r.status(), String::from_utf8_lossy(r.body().as_deref().unwrap_or_default())),
        e => anyhow::anyhow!("can't reach {}: {e}", api.base),
    })?;
    let (mut tx, mut rx) = ws.split();
    // Keystrokes on a task of their own: a read cancelled by the select below would lose them.
    let (keys_tx, mut keys) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 4096];
        while let Ok(n) = stdin.read(&mut buf).await {
            if n == 0 || keys_tx.send(buf[..n].to_vec()).await.is_err() {
                break;
            }
        }
    });
    let _raw = term::Raw::enter();
    let mut winch = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change())?;
    let mut stdout = tokio::io::stdout();
    loop {
        tokio::select! {
            k = keys.recv() => match k {
                Some(b) => tx.send(Message::Binary(b.into())).await?,
                None => break,
            },
            m = rx.next() => match m {
                Some(Ok(Message::Binary(b))) => {
                    stdout.write_all(&b).await?;
                    stdout.flush().await?;
                }
                Some(Ok(Message::Text(t))) => {
                    stdout.write_all(t.as_bytes()).await?;
                    stdout.flush().await?;
                }
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                Some(Ok(_)) => {}
            },
            _ = winch.recv() => {
                if let Some((c, r)) = term::size() {
                    tx.send(Message::Text(format!("{{\"resize\":[{c},{r}]}}").into())).await?;
                }
            }
        }
    }
    let _ = tx.close().await;
    Ok(())
}

/// The local terminal: its size, and raw mode for as long as a guard lives.
mod term {
    /// Columns and rows of the terminal on stdout.
    pub fn size() -> Option<(u16, u16)> {
        let mut ws = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: TIOCGWINSZ writes one winsize into the struct we pass.
        let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
        (ok && ws.ws_col > 0).then_some((ws.ws_col, ws.ws_row))
    }

    /// Raw mode on stdin (keys go through as typed, Ctrl-C included) until dropped; nothing when
    /// stdin isn't a terminal.
    pub struct Raw(Option<libc::termios>);

    impl Raw {
        pub fn enter() -> Raw {
            // SAFETY: isatty reads a descriptor; tcgetattr fills the termios we own;
            // cfmakeraw and tcsetattr take it by pointer.
            unsafe {
                if libc::isatty(libc::STDIN_FILENO) != 1 {
                    return Raw(None);
                }
                let mut t: libc::termios = std::mem::zeroed();
                if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                    return Raw(None);
                }
                let was = t;
                libc::cfmakeraw(&mut t);
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
                Raw(Some(was))
            }
        }
    }

    impl Drop for Raw {
        fn drop(&mut self) {
            if let Some(t) = &self.0 {
                // SAFETY: restores the termios read in `enter` on the same descriptor.
                unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, t) };
            }
        }
    }
}

/// `30s`, `15m`, `2h`, `7d` in milliseconds.
fn ago_ms(d: &str) -> anyhow::Result<i64> {
    let (n, unit) = d.trim().split_at(d.trim().len().saturating_sub(1));
    let n: i64 = n.parse().map_err(|_| anyhow::anyhow!("--since is like 30m, 2h or 7d"))?;
    Ok(n * match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => bail!("--since is like 30m, 2h or 7d"),
    })
}

/// The git commit `dir` is checked out at: its sha, its subject, and whether the tree has local
/// changes. None outside a git checkout (or without git).
fn git_head(dir: &Path) -> Option<(String, String, bool)> {
    let git = |args: &[&str]| std::process::Command::new("git").arg("-C").arg(dir).args(args).output().ok().filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
    let head = git(&["log", "-1", "--format=%H%x00%s"])?;
    let (sha, subject) = head.split_once('\0')?;
    let dirty = git(&["status", "--porcelain"]).is_some_and(|s| !s.is_empty());
    Some((sha.to_string(), subject.chars().take(500).collect(), dirty))
}

/// Arguments as one `sh -c` command, each quoted when the shell would split or expand it.
fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|a| {
            if !a.is_empty() && a.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_./=:@%+,".contains(&b)) {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Ask before deleting `name` (typed back), unless `--yes`. Without a terminal there's nobody to
/// ask: `--yes` is required.
fn confirm(yes: bool, name: &str, what: &str) -> anyhow::Result<()> {
    use std::io::{BufRead, IsTerminal, Write};
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        bail!("this deletes {what}: pass --yes to go ahead");
    }
    eprint!("This deletes {what}. Type {name} to go ahead: ");
    std::io::stderr().flush()?;
    let mut typed = String::new();
    std::io::stdin().lock().read_line(&mut typed)?;
    if typed.trim() != name {
        bail!("not deleted");
    }
    Ok(())
}

/// `.env` text as variables, the way the dashboard reads it (`parseEnv`): `KEY=value` lines,
/// `export` allowed, `#` comments and blank lines skipped, a value in single quotes as is, in
/// double quotes with `\n` for a line break. The 1-based lines it can't read come back too.
fn parse_env(text: &str) -> (Vec<(String, String)>, Vec<usize>) {
    let (mut vars, mut bad) = (Vec::new(), Vec::new());
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim();
        let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            bad.push(i + 1);
            continue;
        };
        let k = k.trim();
        if k.is_empty() || k.starts_with(|c: char| c.is_ascii_digit()) || !k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
            bad.push(i + 1);
            continue;
        }
        let v = v.trim();
        let v = match (v.chars().next(), v.len() >= 2) {
            (Some('\''), true) if v.ends_with('\'') => v[1..v.len() - 1].to_string(),
            (Some('"'), true) if v.ends_with('"') => v[1..v.len() - 1].replace("\\n", "\n").replace("\\\"", "\""),
            _ => v.to_string(),
        };
        vars.push((k.to_string(), v));
    }
    (vars, bad)
}

/// Print a log's lines (SSE `data:` JSON) until the stream ends.
async fn follow(api: &Api, path: &str) -> anyhow::Result<()> {
    follow_matching(api, path, None).await
}

/// [`follow`], printing only lines containing `grep` (case-insensitive, as `?q=` searches).
async fn follow_matching(api: &Api, path: &str, grep: Option<&str>) -> anyhow::Result<()> {
    let grep = grep.map(str::to_lowercase);
    let mut req = api.http.get(format!("{}{path}", api.base));
    if let Some(k) = &api.key {
        req = req.bearer_auth(k);
    }
    let mut res = req.send().await.with_context(|| format!("can't reach {}", api.base))?;
    if !res.status().is_success() {
        bail!("{}: {}", res.status(), res.text().await.unwrap_or_default());
    }
    let mut buf = Vec::new();
    while let Some(chunk) = res.chunk().await? {
        buf.extend_from_slice(&chunk);
        while let Some(i) = buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = buf.drain(..=i).collect();
            let line = String::from_utf8_lossy(&line);
            if let Some(data) = line.trim_end().strip_prefix("data:")
                && let Ok(v) = serde_json::from_str::<Value>(data.trim())
                && grep.as_ref().is_none_or(|g| s(&v["text"]).to_lowercase().contains(g))
            {
                print_line(&v);
            }
        }
    }
    Ok(())
}

fn print_line(l: &Value) {
    let t = l["t"].as_i64().and_then(chrono::DateTime::from_timestamp_millis).map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default();
    // An access line: `GET /path 200 12ms`, the cold start's wait when there was one.
    if l["stream"] == "http"
        && let Ok(r) = serde_json::from_str::<Value>(&s(&l["text"]))
    {
        let cold = if r["cold"] == true { format!("  cold +{}ms", r["waited_ms"]) } else { String::new() };
        let inst = r["instance"].as_str().map(|i| i.rsplit('-').next().unwrap_or(i).to_string()).unwrap_or_default();
        println!("{t} {inst:>5} │ {} {} {} {}ms{cold}", s(&r["method"]), s(&r["path"]), r["status"], r["ms"]);
        return;
    }
    let who = match l["stream"].as_str() {
        Some("build") => "build".to_string(),
        Some("system") => "roll".to_string(),
        _ => l["instance"].as_str().map(|i| i.rsplit('-').next().unwrap_or(i).to_string()).unwrap_or_default(),
    };
    println!("{t} {who:>5} │ {}", s(&l["text"]));
}

/// Follow a deployment's build and rollout, then say how it ended.
async fn watch(api: &Api, svc: &Value, env: &str, dep: &Value) -> anyhow::Result<()> {
    let base = format!("{}/deploys/{}", env_path(svc, env), enc(&s(&dep["id"])));
    follow(api, &format!("{base}/logs?follow=1&tail=1000")).await?;
    let d = api.get(&base).await?;
    match d["status"].as_str() {
        Some("active") => {
            let e = api.get(&env_path(svc, env)).await?;
            println!("\n✓ {} is live: {}", s(&svc["name"]), s(&e["url"]));
            if let Some(p) = e["port"].as_u64() {
                println!("  locally also on port {p}");
            }
            Ok(())
        }
        Some(st) => bail!("the deploy is {st}: {}", d["error"].as_str().unwrap_or("see the log above")),
        None => bail!("the deploy has no status"),
    }
}

pub async fn up(api: &Api, json_out: bool, u: Up) -> anyhow::Result<()> {
    let dir = match u.dir.clone() {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    let dir = std::fs::canonicalize(&dir).with_context(|| format!("{}", dir.display()))?;
    if u.all {
        return up_all(api, json_out, &dir, &u).await;
    }
    let link = read_link(&dir);
    let git = git_head(&dir);
    let svc = match (&u.target.service, &link) {
        (Some(_), _) => service(api, &u.target).await?,
        (None, Some(l)) => api.get(&format!("/v1/apps/services/{}", enc(&s(&l["service"])))).await?,
        (None, None) => {
            // First time here: a project and an upload service named after the directory.
            let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("app").to_ascii_lowercase();
            let name: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>().trim_matches('-').chars().take(32).collect();
            let name = if name.is_empty() { "app".to_string() } else { name };
            let project = match &u.target.project {
                Some(p) => project_id(api, p).await?,
                None => match project_id(api, &name).await {
                    Ok(p) => p,
                    Err(_) => s(&api.post("/v1/workspace/projects", json!({ "name": name })).await?["id"]),
                },
            };
            let svc = api.post("/v1/apps/services", json!({ "project": project, "name": name, "source": { "kind": "upload" } })).await?;
            eprintln!("made service {} in project {}", s(&svc["name"]), project);
            // What the code talks to, said once: roll apps links the database's variables in.
            let tree = apps_build::DirTree::new(&dir);
            for (store, why) in apps_build::uses(&tree) {
                let make = match store {
                    apps_build::Datastore::Postgres => format!("roll db create {name}-db"),
                    apps_build::Datastore::Redis => format!("roll redis create {name}-cache"),
                };
                let kind = if store == apps_build::Datastore::Postgres { "postgres" } else { "redis" };
                eprintln!("the code uses {} ({why}): `{make}`, then `roll apps link {kind} <its id>` puts its URL in", store.label());
            }
            svc
        }
    };
    write_link(&dir, &json!({ "service": svc["id"], "project": svc["project"], "name": svc["name"] }))?;
    let bundle = tokio::task::spawn_blocking(move || pack(&dir)).await??;
    eprintln!("uploading {} KiB", bundle.len() / 1024);
    let mut req = api.http.post(format!("{}{}/deploys", api.base, env_path(&svc, &u.target.env))).header("content-type", "application/gzip").body(bundle);
    if let Some(k) = &api.key {
        req = req.bearer_auth(k);
    }
    // The message: the one given, else the checkout's last commit (said to have local changes
    // when the tree isn't clean), percent-encoded since a subject needn't be ASCII.
    let message = u.message.clone().or_else(|| git.as_ref().map(|(_, subject, dirty)| if *dirty { format!("{subject} (with local changes)") } else { subject.clone() }));
    if let Some(m) = &message {
        req = req.header("x-roll-message", urlencoding::encode(m).into_owned());
    }
    if let Some((sha, _, _)) = &git {
        req = req.header("x-roll-revision", sha);
    }
    let res = req.send().await.with_context(|| format!("can't reach {}", api.base))?;
    let status = res.status();
    let dep: Value = res.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("{}", dep["error"].as_str().unwrap_or(status.as_str()));
    }
    if json_out || u.detach {
        println!("{}", serde_json::to_string_pretty(&dep)?);
        return Ok(());
    }
    watch(api, &svc, &u.target.env, &dep).await
}

// ── a project in one file: `roll up --all` ──

/// A root roll.toml's project: the services (`[apps.<name>]`) and databases
/// (`[databases.<name>]`) it declares (Render's Blueprints). Each app's own settings stay in
/// its root's roll.toml `[app]`.
#[derive(Debug, Default, serde::Deserialize)]
struct ProjectFile {
    #[serde(default)]
    apps: std::collections::BTreeMap<String, AppEntry>,
    #[serde(default)]
    databases: std::collections::BTreeMap<String, DbEntry>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct AppEntry {
    /// The directory `roll up` uploads (relative to the file), or with `repo` the directory
    /// in the repo that builds.
    root: Option<String>,
    /// Or a public image.
    image: Option<String>,
    /// Or a roll code repo (`owner/name`): it deploys on each landing on `branch`.
    repo: Option<String>,
    branch: Option<String>,
    /// `web`, `worker` or `cron`.
    #[serde(rename = "type")]
    kind: Option<String>,
    start: Option<String>,
    /// A cron service's five fields (UTC).
    schedule: Option<String>,
    /// Any other run setting, as `[app.run]` takes it (checked by the API).
    run: Option<toml::Table>,
    /// Databases below, by name: their connection variables go in.
    #[serde(default)]
    links: Vec<String>,
    /// The port it listens on (an image's; a root's own roll.toml can say it too).
    port: Option<u16>,
    /// Variables it starts with (set when the service is made, never over what it has).
    #[serde(default)]
    variables: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct DbEntry {
    /// `postgres` or `redis`.
    kind: String,
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 32 && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !n.starts_with('-') && !n.ends_with('-')
}

/// The `[apps]` and `[databases]` of `text` (the rest of roll.toml is other things'), checked.
fn parse_project(text: &str) -> anyhow::Result<ProjectFile> {
    let v: toml::Table = toml::from_str(text).context("roll.toml isn't TOML")?;
    let mut t = toml::Table::new();
    for k in ["apps", "databases"] {
        if let Some(x) = v.get(k) {
            t.insert(k.into(), x.clone());
        }
    }
    let f: ProjectFile = toml::Value::Table(t).try_into().context("roll.toml's [apps] or [databases]")?;
    anyhow::ensure!(!f.apps.is_empty(), "roll.toml declares no [apps.<name>]");
    for (n, d) in &f.databases {
        anyhow::ensure!(valid_name(n), "database {n:?}: a name is a-z, 0-9 and -, at most 32");
        anyhow::ensure!(matches!(d.kind.as_str(), "postgres" | "redis"), "database {n}: kind is postgres or redis");
    }
    for (n, a) in &f.apps {
        anyhow::ensure!(valid_name(n), "app {n:?}: a name is a-z, 0-9 and -, at most 32");
        if let Some(r) = &a.root {
            anyhow::ensure!(!r.starts_with('/') && !r.split('/').any(|c| c == ".."), "app {n}: root is a directory inside this one (or the repo)");
        }
        match (&a.root, &a.image, &a.repo) {
            (Some(_), None, None) | (None, Some(_), None) | (_, None, Some(_)) => {}
            _ => bail!("app {n}: give one of root (a directory to upload), image, or repo (with root inside it)"),
        }
        anyhow::ensure!(a.branch.is_none() || a.repo.is_some(), "app {n}: branch is for a repo");
        anyhow::ensure!(a.kind.as_deref().is_none_or(|k| ["web", "worker", "cron"].contains(&k)), "app {n}: type is web, worker or cron");
        for l in &a.links {
            anyhow::ensure!(f.databases.contains_key(l), "app {n} links {l}, which isn't in [databases]");
        }
    }
    Ok(f)
}

/// The run settings an entry names (its `run` table, then `type`, `start`, `schedule` and
/// `port` over it): what a new service is made with, and what an existing one is set to.
fn entry_run(a: &AppEntry) -> anyhow::Result<serde_json::Map<String, Value>> {
    let mut run = match &a.run {
        Some(t) => match serde_json::to_value(t)? {
            Value::Object(m) => m,
            _ => serde_json::Map::new(),
        },
        None => serde_json::Map::new(),
    };
    for (k, v) in [("type", a.kind.clone()), ("start", a.start.clone()), ("schedule", a.schedule.clone())] {
        if let Some(v) = v {
            run.insert(k.into(), json!(v));
        }
    }
    if let Some(p) = a.port {
        run.insert("port".into(), json!(p));
    }
    Ok(run)
}

/// The kind of source an entry makes.
fn entry_kind(a: &AppEntry) -> &'static str {
    if a.repo.is_some() {
        "repo"
    } else if a.image.is_some() {
        "image"
    } else {
        "upload"
    }
}

/// What `--all` will do, as lines: `+` made, `=` already there, `~` linked, `↑` deployed.
fn plan_lines(f: &ProjectFile, services: &std::collections::BTreeMap<String, Value>, dbs: &std::collections::BTreeMap<(String, String), String>, links: &std::collections::BTreeMap<String, Vec<String>>) -> Vec<String> {
    let mut out = Vec::new();
    for (n, d) in &f.databases {
        let have = dbs.contains_key(&(d.kind.clone(), n.clone()));
        out.push(format!("{} {} database {n}", if have { "=" } else { "+" }, d.kind));
    }
    for (n, a) in &f.apps {
        let from = match (&a.repo, &a.image, &a.root) {
            (Some(r), _, root) => format!("repo {r}{}", root.as_ref().map(|d| format!(" at {d}")).unwrap_or_default()),
            (None, Some(i), _) => format!("image {i}"),
            (None, None, r) => format!("from {}", r.as_deref().unwrap_or(".")),
        };
        let have = services.get(n);
        out.push(format!("{} service {n} ({from})", if have.is_some() { "=" } else { "+" }));
        for l in &a.links {
            let kind = &f.databases[l].kind;
            let linked = dbs.get(&(kind.clone(), l.clone())).is_some_and(|r| links.get(n).is_some_and(|ls| ls.contains(r)));
            if !linked {
                out.push(format!("~ link {n} to {l}"));
            }
        }
        // An existing service's settings that the file names differently.
        if let (Some(s), Ok(run)) = (have, entry_run(a)) {
            let differs: Vec<&String> = run.iter().filter(|(k, v)| s["run"].get(k.as_str()) != Some(*v)).map(|(k, _)| k).collect();
            if !differs.is_empty() {
                out.push(format!("~ settings {n} ({})", differs.iter().map(|k| k.as_str()).collect::<Vec<_>>().join(", ")));
            }
        }
        let image_moved = a.image.as_ref().is_some_and(|i| have.is_some_and(|s| s["source"]["image"].as_str() != Some(i.as_str())));
        if (a.root.is_some() && a.repo.is_none()) || image_moved {
            out.push(format!("↑ deploy {n}"));
        }
    }
    out
}

/// A Compose file as roll.toml text: image and build services as `[apps.<name>]`, Postgres and
/// Redis containers as roll's own `[databases.<name>]` (linked from what depends on them), and
/// notes on what isn't carried over (volumes, networks, commands, health checks).
fn compose_to_toml(text: &str) -> anyhow::Result<(String, Vec<String>)> {
    let mut v: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).context("the Compose file isn't YAML")?;
    // `<<: *defaults`, as Compose files share settings.
    v.apply_merge().context("the Compose file's merge keys")?;
    let services = v.get("services").and_then(|s| s.as_mapping()).context("the Compose file has no services")?;
    let str_of = |v: &serde_yaml_ng::Value| -> Option<String> {
        match v {
            serde_yaml_ng::Value::String(s) => Some(s.clone()),
            serde_yaml_ng::Value::Number(n) => Some(n.to_string()),
            serde_yaml_ng::Value::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    };
    let db_kind = |image: &str| {
        let repo = image.split(['@']).next().unwrap_or(image).rsplit('/').next().unwrap_or(image).split(':').next().unwrap_or("");
        if repo.starts_with("postgres") || repo.starts_with("postgis") || repo == "postgresql" {
            Some("postgres")
        } else if repo == "redis" || repo == "valkey" || repo.starts_with("redis-") {
            Some("redis")
        } else {
            None
        }
    };
    let name_of = |k: &str| -> anyhow::Result<String> {
        let n: String = k.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>().trim_matches('-').to_string();
        anyhow::ensure!(valid_name(&n), "service {k:?} has no name roll can use (a-z, 0-9 and -, at most 32)");
        Ok(n)
    };
    let mut dbs = std::collections::BTreeMap::new();
    let mut names = std::collections::BTreeMap::new();
    for (k, svc) in services {
        let k = str_of(k).context("a service's name")?;
        let n = name_of(&k)?;
        if let Some(other) = names.insert(n.clone(), k.clone()) {
            bail!("services {other:?} and {k:?} would both be {n}: rename one");
        }
        if let Some(kind) = svc.get("image").and_then(str_of).as_deref().and_then(db_kind) {
            dbs.insert(k.clone(), (n, kind));
        }
    }
    let q = |s: &str| toml::Value::String(s.to_string()).to_string();
    let (mut out, mut notes) = (String::new(), Vec::new());
    for (k, svc) in services {
        let k = str_of(k).context("a service's name")?;
        if dbs.contains_key(&k) {
            continue;
        }
        let name = name_of(&k)?;
        out.push_str(&format!("[apps.{name}]\n"));
        match (svc.get("build"), svc.get("image").and_then(str_of)) {
            (Some(b), _) => {
                let ctx = str_of(b).or_else(|| b.get("context").and_then(str_of)).unwrap_or_else(|| ".".into());
                let ctx = ctx.trim_start_matches("./").trim_end_matches('/');
                out.push_str(&format!("root = {}\n", q(if ctx.is_empty() { "." } else { ctx })));
            }
            (None, Some(img)) => out.push_str(&format!("image = {}\n", q(&img))),
            (None, None) => bail!("service {k} has neither image nor build"),
        }
        // The container's own port: `8080:80` → 80, `80`, or `{target: 80}`.
        if let Some(p) = svc.get("ports").and_then(|p| p.as_sequence()).and_then(|s| s.first()) {
            let port = match p.get("target") {
                Some(t) => str_of(t),
                None => str_of(p).map(|s| s.split('/').next().unwrap_or("").rsplit(':').next().unwrap_or("").to_string()),
            };
            if let Some(port) = port.and_then(|p| p.parse::<u16>().ok()) {
                out.push_str(&format!("port = {port}\n"));
            }
        }
        let links: Vec<String> = match svc.get("depends_on") {
            Some(serde_yaml_ng::Value::Sequence(s)) => s.iter().filter_map(str_of).collect(),
            Some(serde_yaml_ng::Value::Mapping(m)) => m.keys().filter_map(str_of).collect(),
            _ => Vec::new(),
        };
        let linked: Vec<(&String, &(String, &str))> = links.iter().filter_map(|l| dbs.get_key_value(l)).collect();
        if !linked.is_empty() {
            out.push_str(&format!("links = [{}]\n", linked.iter().map(|(_, (n, _))| q(n)).collect::<Vec<_>>().join(", ")));
        }
        let vars: Vec<(String, String)> = match svc.get("environment") {
            Some(serde_yaml_ng::Value::Mapping(m)) => m.iter().filter_map(|(k, v)| Some((str_of(k)?, str_of(v).unwrap_or_default()))).collect(),
            Some(serde_yaml_ng::Value::Sequence(s)) => s.iter().filter_map(str_of).filter_map(|kv| kv.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))).collect(),
            _ => Vec::new(),
        };
        // A link sets its own URL: the Compose one (`postgres://…@db:5432`) names a container
        // that won't exist, and a service's variables would win over the link's.
        let (vars, dropped_vars): (Vec<_>, Vec<_>) = vars.into_iter().partition(|(var, value)| {
            !linked.iter().any(|(host, (_, kind))| {
                let own = if *kind == "postgres" { "DATABASE_URL" } else { "REDIS_URL" };
                var == own || value.contains(&format!("@{host}:")) || value.contains(&format!("//{host}:")) || value.contains(&format!("@{host}/")) || value.contains(&format!("//{host}/"))
            })
        });
        if !dropped_vars.is_empty() {
            notes.push(format!("{k}: {} left out: the link sets the database's URL", dropped_vars.iter().map(|(v, _)| v.as_str()).collect::<Vec<_>>().join(", ")));
        }
        if !vars.is_empty() {
            out.push_str(&format!("[apps.{name}.variables]\n"));
            for (k, v) in vars {
                out.push_str(&format!("{} = {}\n", q(&k), q(&v)));
            }
        }
        out.push('\n');
        let dropped: Vec<&str> = ["volumes", "networks", "command", "entrypoint", "healthcheck", "env_file"].into_iter().filter(|f| svc.get(*f).is_some()).collect();
        if !dropped.is_empty() {
            notes.push(format!("{k}: not carried over: {} (roll.toml [app] in its root, volumes in its settings)", dropped.join(", ")));
        }
    }
    for (k, (name, kind)) in &dbs {
        out.push_str(&format!("[databases.{name}]\nkind = {}\n\n", q(kind)));
        notes.push(format!("{k}: a roll {kind} database instead of a container; its link sets {}", if *kind == "postgres" { "DATABASE_URL" } else { "REDIS_URL" }));
    }
    Ok((out, notes))
}

async fn up_all(api: &Api, json_out: bool, dir: &Path, u: &Up) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(dir.join("roll.toml")).with_context(|| format!("no roll.toml in {}", dir.display()))?;
    let f = parse_project(&text)?;
    let pname = match &u.target.project {
        Some(p) => p.clone(),
        None => dir.file_name().and_then(|n| n.to_str()).unwrap_or("app").to_ascii_lowercase(),
    };
    // Looked up, not guessed from an error: a failed request must never make a second project.
    let found = api.get("/v1/workspace/projects").await?.as_array().into_iter().flatten().find(|x| x["name"] == pname.as_str() || x["id"] == pname.as_str()).cloned();
    let project = found.as_ref().map(|x| s(&x["id"]));
    // Its databases are the ones on its canvas: another project's `db` is never this one's.
    let placed: std::collections::HashSet<(String, String)> =
        found.iter().flat_map(|x| x["resources"].as_array().into_iter().flatten()).map(|r| (s(&r["kind"]), s(&r["ref"]))).collect();
    // What's there: the project's services, databases by name, links by service.
    let mut services = std::collections::BTreeMap::new();
    let mut links: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
    if let Some(p) = &project {
        for svc in api.get(&format!("/v1/apps/services?project={}", enc(p))).await?.as_array().into_iter().flatten() {
            let ls = api.get(&format!("/v1/apps/services/{}/links", enc(&s(&svc["id"])))).await.unwrap_or(Value::Null);
            links.insert(s(&svc["name"]), ls.as_array().into_iter().flatten().map(|l| s(&l["ref"])).collect());
            services.insert(s(&svc["name"]), svc.clone());
        }
    }
    let mut dbs = std::collections::BTreeMap::new();
    let wants = |k: &str| f.databases.values().any(|d| d.kind == k);
    if wants("postgres") {
        for p in api.get("/v1/projects").await?.as_array().into_iter().flatten().filter(|p| placed.contains(&("postgres".to_string(), s(&p["id"])))) {
            dbs.insert(("postgres".to_string(), s(&p["name"])), s(&p["id"]));
        }
    }
    if wants("redis") {
        for d in api.get("/v1/redis/databases").await?.as_array().into_iter().flatten().filter(|d| placed.contains(&("redis".to_string(), s(&d["id"])))) {
            dbs.insert(("redis".to_string(), s(&d["name"])), s(&d["id"]));
        }
    }
    // A service the file names with another source is a mistake to stop at, not to convert.
    for (n, a) in &f.apps {
        if let Some(svc) = services.get(n) {
            let kind = s(&svc["source"]["kind"]);
            let want = entry_kind(a);
            anyhow::ensure!(kind == want, "service {n} exists with a {kind} source; the file makes it {want}: rename one");
        }
    }
    let plan = plan_lines(&f, &services, &dbs, &links);
    if !json_out {
        println!("project {pname}{}", if project.is_none() { " (new)" } else { "" });
        for l in &plan {
            println!("  {l}");
        }
    }
    if u.dry_run {
        if json_out {
            println!("{}", serde_json::to_string_pretty(&json!({ "project": pname, "plan": plan }))?);
        }
        return Ok(());
    }
    let project = match project {
        Some(p) => p,
        None => s(&api.post("/v1/workspace/projects", json!({ "name": pname })).await?["id"]),
    };
    // Databases first (their variables are in the services' first deploys), placed on the
    // project's canvas in a row.
    for (i, (n, d)) in f.databases.iter().enumerate() {
        if dbs.contains_key(&(d.kind.clone(), n.clone())) {
            continue;
        }
        let made = match d.kind.as_str() {
            "postgres" => api.post("/v1/projects", json!({ "name": n })).await?,
            _ => api.post("/v1/redis/databases", json!({ "name": n })).await?,
        };
        let r = s(&made["id"]);
        let _ = api.call(reqwest::Method::PUT, &format!("/v1/workspace/projects/{}/resources/{}/{}", enc(&project), d.kind, enc(&r)), Some(json!({ "x": 48 + 320 * i as i64, "y": 340 }))).await;
        eprintln!("made {} database {n}", d.kind);
        dbs.insert((d.kind.clone(), n.clone()), r);
    }
    let mut out = Vec::new();
    for (n, a) in &f.apps {
        let fresh = !services.contains_key(n);
        let svc = match services.get(n) {
            Some(svc) => svc.clone(),
            None => {
                let source = match (&a.repo, &a.image) {
                    (Some(r), _) => {
                        let mut src = json!({ "kind": "repo", "repo": r });
                        if let Some(root) = &a.root {
                            src["root"] = json!(root);
                        }
                        src
                    }
                    (None, Some(i)) => json!({ "kind": "image", "image": i }),
                    (None, None) => json!({ "kind": "upload" }),
                };
                let made_links: Vec<Value> = a.links.iter().map(|l| json!({ "kind": f.databases[l].kind, "ref": dbs[&(f.databases[l].kind.clone(), l.clone())] })).collect();
                let mut body = json!({ "project": project, "name": n, "source": source, "links": made_links });
                let run = entry_run(a)?;
                if !run.is_empty() {
                    body["run"] = Value::Object(run);
                }
                if !a.variables.is_empty() {
                    body["variables"] = json!(a.variables);
                }
                let svc = api.post("/v1/apps/services", body).await?;
                eprintln!("made service {n}");
                if let Some(b) = &a.branch {
                    api.call(reqwest::Method::PUT, &env_path(&svc, &u.target.env), Some(json!({ "branch": b }))).await?;
                }
                svc
            }
        };
        let id = s(&svc["id"]);
        if !fresh {
            // The settings the file names, where they differ (the rest stay as they are).
            let run = entry_run(a)?;
            let differs: serde_json::Map<String, Value> = run.into_iter().filter(|(k, v)| svc["run"].get(k.as_str()) != Some(v)).collect();
            if !differs.is_empty() {
                api.call(reqwest::Method::PATCH, &format!("/v1/apps/services/{}", enc(&id)), Some(json!({ "run": differs }))).await?;
                eprintln!("set {n}'s settings");
            }
            for l in &a.links {
                let kind = &f.databases[l].kind;
                let r = &dbs[&(kind.clone(), l.clone())];
                if !links.get(n).is_some_and(|ls| ls.contains(r)) {
                    api.call(reqwest::Method::PUT, &format!("/v1/apps/services/{}/links/{kind}/{}", enc(&id), enc(r)), Some(json!({}))).await?;
                    eprintln!("linked {n} to {l}");
                }
            }
        }
        let dep = match (&a.root, &a.image) {
            (Some(root), _) if a.repo.is_none() => {
                let tree = dir.join(root);
                let bundle = tokio::task::spawn_blocking(move || pack(&tree)).await??;
                let mut req = api.http.post(format!("{}{}/deploys", api.base, env_path(&svc, &u.target.env))).header("content-type", "application/gzip").body(bundle);
                if let Some(k) = &api.key {
                    req = req.bearer_auth(k);
                }
                if let Some(m) = &u.message {
                    req = req.header("x-roll-message", urlencoding::encode(m).into_owned());
                }
                let res = req.send().await.with_context(|| format!("can't reach {}", api.base))?;
                let (ok, v) = (res.status().is_success(), res.json::<Value>().await.unwrap_or(Value::Null));
                anyhow::ensure!(ok, "{n}: {}", v["error"].as_str().unwrap_or("the upload failed"));
                Some(v)
            }
            (None, Some(image)) if !fresh && svc["source"]["image"].as_str() != Some(image.as_str()) => {
                api.call(reqwest::Method::PATCH, &format!("/v1/apps/services/{}", enc(&id)), Some(json!({ "source": { "kind": "image", "image": image } }))).await?;
                Some(api.post(&format!("{}/deploys", env_path(&svc, &u.target.env)), json!({})).await?)
            }
            // A new image service deploys as it's made.
            _ => svc["environments"][0]["latest"].as_object().map(|_| svc["environments"][0]["latest"].clone()),
        };
        if let Some(d) = &dep {
            if !json_out {
                println!("↑ {n}: {}", s(&d["id"]));
            }
            out.push(json!({ "service": n, "deployment": d["id"] }));
        }
    }
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "project": pname, "deploys": out }))?);
    } else if !out.is_empty() {
        eprintln!("follow one with `roll logs --build latest -s <name>`");
    }
    Ok(())
}

pub async fn deploy(api: &Api, json_out: bool, d: Deploy) -> anyhow::Result<()> {
    let svc = service(api, &d.target).await?;
    let body = json!({ "revision": d.revision, "image": d.image, "no_traffic": d.no_traffic.then_some(true) });
    let dep = api.post(&format!("{}/deploys", env_path(&svc, &d.target.env)), body).await?;
    if json_out || d.detach {
        println!("{}", serde_json::to_string_pretty(&dep)?);
        return Ok(());
    }
    watch(api, &svc, &d.target.env, &dep).await
}

pub async fn logs(api: &Api, json_out: bool, l: Logs) -> anyhow::Result<()> {
    let svc = service(api, &l.target).await?;
    let base = env_path(&svc, &l.target.env);
    let path = match &l.build {
        Some(b) => {
            let dep = if b == "latest" {
                let all = api.get(&format!("{base}/deploys")).await?;
                if all.as_array().is_none_or(Vec::is_empty) {
                    bail!("{} has no deployments in {} yet", s(&svc["name"]), l.target.env);
                }
                s(&all[0]["id"])
            } else {
                b.clone()
            };
            format!("{base}/deploys/{}/logs", enc(&dep))
        }
        None => format!("{base}/logs"),
    };
    let since = match &l.since {
        Some(d) => format!("&since={}", chrono::Utc::now().timestamp_millis() - ago_ms(d)?),
        None => String::new(),
    };
    let mut http = String::new();
    if l.http {
        http.push_str("&http=1");
        if let Some(s) = &l.status {
            http.push_str(&format!("&status={}", enc(s)));
        }
        if let Some(ms) = l.min_ms {
            http.push_str(&format!("&min_ms={ms}"));
        }
    }
    if l.follow {
        return follow_matching(api, &format!("{path}?follow=1&tail={}{since}{http}", l.tail), l.grep.as_deref()).await;
    }
    let grep = l.grep.as_deref().map(|g| format!("&q={}", enc(g))).unwrap_or_default();
    let lines = api.get(&format!("{path}?tail={}{since}{grep}{http}", l.tail)).await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&lines)?);
        return Ok(());
    }
    for line in lines.as_array().into_iter().flatten() {
        print_line(line);
    }
    Ok(())
}

pub async fn domains(api: &Api, json_out: bool, d: Domains) -> anyhow::Result<()> {
    let out = match d {
        Domains::List { target } => {
            let svc = service(api, &target).await?;
            let v = api.get(&format!("{}/domains", env_path(&svc, &target.env))).await?;
            if !json_out {
                for d in v.as_array().into_iter().flatten() {
                    let extra = if d["kind"] == "custom" && d["status"] != "active" { format!("  → CNAME {}", s(&d["target"])) } else { String::new() };
                    println!("{:<44} {:<7} {}{extra}", s(&d["hostname"]), s(&d["kind"]), s(&d["status"]));
                }
                return Ok(());
            }
            v
        }
        Domains::Add { hostname, target } => {
            let svc = service(api, &target).await?;
            let v = api.post(&format!("{}/domains", env_path(&svc, &target.env)), json!({ "hostname": hostname })).await?;
            if !json_out {
                println!("Added {}. Point it at roll:", s(&v["hostname"]));
                println!("  CNAME  {}  →  {}", s(&v["hostname"]), s(&v["target"]));
                if v["record"] == "both" {
                    // The CNAME target is every tenant's: the TXT record proves the domain is yours.
                    println!("and prove it's yours:");
                } else {
                    println!("or, for an apex domain, keep its records and add:");
                }
                let h = s(&v["hostname"]);
                println!("  TXT    _roll-challenge.{}  \"{}\"", h.strip_prefix("*.").unwrap_or(&h), s(&v["token"]));
                println!("roll checks every minute; `roll domains verify {}` checks now.", s(&v["hostname"]));
                return Ok(());
            }
            v
        }
        Domains::Verify { hostname, target } => {
            let svc = service(api, &target).await?;
            api.post(&format!("{}/domains/{}/verify", env_path(&svc, &target.env), enc(&hostname)), json!({})).await?
        }
        Domains::Remove { hostname, target } => {
            let svc = service(api, &target).await?;
            api.call(reqwest::Method::DELETE, &format!("{}/domains/{}", env_path(&svc, &target.env), enc(&hostname)), None).await?;
            json!({ "removed": hostname })
        }
        Domains::Redirect { hostname, to, target } => {
            let svc = service(api, &target).await?;
            let to = if to == "none" { Value::Null } else { json!(to) };
            api.call(reqwest::Method::PATCH, &format!("{}/domains/{}", env_path(&svc, &target.env), enc(&hostname)), Some(json!({ "redirect_to": to }))).await?
        }
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        human(&out)?;
    }
    Ok(())
}

/// What a command answered, for a person: by its shape (an environment, a service, a domain, a
/// link, something removed), else as JSON.
fn human(v: &Value) -> anyhow::Result<()> {
    if v.get("needs_deploy").is_some() && v.get("instances").is_some() {
        let n = v["instances"].as_array().map_or(0, Vec::len);
        let private = if v["private"] == true { "  private" } else { "" };
        println!("{:<12} {:<10} {}  ({n} {}){private}", s(&v["name"]), s(&v["state"]), s(&v["url"]), if n == 1 { "instance" } else { "instances" });
        if v["needs_deploy"] == true {
            println!("variables or settings changed since the live deployment: `roll apps redeploy` applies them");
        }
    } else if v.get("source").is_some() && v.get("run").is_some() {
        print_service(v);
    } else if v.get("hostname").is_some() && v.get("status").is_some() {
        match v["error"].as_str() {
            Some(e) if v["status"] != "active" => println!("{}  {}: {e}", s(&v["hostname"]), s(&v["status"])),
            _ => println!("{}  {}", s(&v["hostname"]), s(&v["status"])),
        }
    } else if v.get("vars").is_some() && v.get("kind").is_some() {
        let vars: Vec<&str> = v["vars"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        println!("linked {} {}: {}", s(&v["kind"]), s(&v["ref"]), vars.join(", "));
        println!("its services get them on their next deploy (`roll apps redeploy`)");
    } else if let Some(h) = v["removed"].as_str() {
        println!("removed {h}");
    } else if let Some(e) = v["deleted"].as_str() {
        println!("deleted {e}");
    } else {
        println!("{}", serde_json::to_string_pretty(v)?);
    }
    Ok(())
}

fn print_service(v: &Value) {
    println!("{}  {}  {}", s(&v["id"]), s(&v["name"]), s(&v["run"]["type"]));
    for e in v["environments"].as_array().into_iter().flatten() {
        let latest = match e["latest"]["status"].as_str() {
            Some(st) if st != "active" => format!("  (latest deploy {st})"),
            _ => String::new(),
        };
        println!("  {:<12} {:<10} {}{latest}", s(&e["name"]), s(&e["state"]), s(&e["url"]));
    }
}

pub async fn run(api: &Api, json_out: bool, cmd: AppsCmd) -> anyhow::Result<()> {
    let out = match cmd {
        AppsCmd::List { project } => {
            let path = match project {
                Some(p) => format!("/v1/apps/services?project={}", enc(&project_id(api, &p).await?)),
                None => "/v1/apps/services".into(),
            };
            let v = api.get(&path).await?;
            if !json_out {
                for svc in v.as_array().into_iter().flatten() {
                    print_service(svc);
                }
                return Ok(());
            }
            v
        }
        AppsCmd::Show { target } => {
            let svc = service(api, &target).await?;
            if !json_out {
                print_service(&svc);
                let e = api.get(&env_path(&svc, &target.env)).await?;
                for i in e["instances"].as_array().into_iter().flatten() {
                    println!("    {}  {}  {} in flight", s(&i["id"]), s(&i["state"]), s(&i["in_flight"]));
                }
                return Ok(());
            }
            svc
        }
        AppsCmd::Create { name, project, repo, root, image, kind, port, schedule, timezone } => {
            let source = match (repo, image) {
                (Some(r), None) => json!({ "kind": "repo", "repo": r, "root": root }),
                (None, Some(i)) => json!({ "kind": "image", "image": i }),
                (None, None) => json!({ "kind": "upload" }),
                _ => bail!("--repo or --image, not both"),
            };
            let mut run = serde_json::Map::new();
            if let Some(k) = kind {
                run.insert("type".into(), k.into());
            }
            if let Some(p) = port {
                run.insert("port".into(), p.into());
            }
            if let Some(tz) = timezone {
                run.insert("timezone".into(), tz.into());
            }
            if let Some(sch) = schedule {
                run.insert("schedule".into(), sch.into());
            }
            let project = project_id(api, &project).await?;
            let v = api.post("/v1/apps/services", json!({ "project": project, "name": name, "source": source, "run": run })).await?;
            if !json_out {
                print_service(&v);
                return Ok(());
            }
            v
        }
        AppsCmd::Set { target, min, max, cpu, memory, port, start, pre_deploy, health, schedule, timezone, kind, concurrency, suspend_after, sleep_after, timeout, volume, watch, rollout, rollout_step, rollout_errors, h2c, scale, public, drain, cpu_target, restart, restart_tries, affinity } => {
            let svc = service(api, &target).await?;
            // The service's rollout with what was given replaced (the API takes it whole).
            let rollout = if rollout.is_some() || rollout_step.is_some() || rollout_errors.is_some() {
                let mut r = svc["run"]["rollout"].as_object().cloned().unwrap_or_default();
                match rollout.as_deref() {
                    None => {}
                    Some("none") => {
                        r.insert("steps".into(), json!([]));
                    }
                    Some(v) => {
                        let steps = v.split(',').map(|p| p.trim().trim_end_matches('%').parse::<u8>()).collect::<Result<Vec<_>, _>>().map_err(|_| anyhow::anyhow!("--rollout is percents like 10,50, or none"))?;
                        r.insert("steps".into(), json!(steps));
                    }
                }
                if let Some(n) = rollout_step {
                    r.insert("step_s".into(), json!(n));
                }
                if let Some(n) = rollout_errors {
                    r.insert("error_pct".into(), json!(n));
                }
                Some(Value::Object(r))
            } else {
                None
            };
            let volume = match volume.as_deref() {
                None => None,
                Some("none") => Some(Value::Null),
                Some(v) => {
                    let (mount, size) = v.rsplit_once(':').ok_or_else(|| anyhow::anyhow!("--volume is <mount>:<size in GB>, like /data:5"))?;
                    let size: u32 = size.parse().map_err(|_| anyhow::anyhow!("--volume's size is a whole number of GB"))?;
                    Some(json!({ "mount": mount, "size_gb": size }))
                }
            };
            let mut body = serde_json::Map::new();
            match watch.as_deref() {
                None => {}
                Some("none") => {
                    body.insert("build".into(), json!({ "watch": null }));
                }
                Some(w) => {
                    let paths: Vec<&str> = w.split(',').map(str::trim).filter(|p| !p.is_empty()).collect();
                    body.insert("build".into(), json!({ "watch": paths }));
                }
            }
            let mut run = serde_json::Map::new();
            for (k, v) in [
                ("min_instances", min.map(Value::from)),
                ("max_instances", max.map(Value::from)),
                ("cpu", cpu.map(Value::from)),
                ("memory_mb", memory.map(Value::from)),
                ("port", port.map(Value::from)),
                ("start", start.map(Value::from)),
                ("pre_deploy", pre_deploy.map(Value::from)),
                ("health_path", health.map(Value::from)),
                ("schedule", schedule.map(Value::from)),
                ("timezone", timezone.map(Value::from)),
                ("type", kind.map(Value::from)),
                ("concurrency", concurrency.map(Value::from)),
                ("suspend_after_s", suspend_after.map(Value::from)),
                ("sleep_after_s", sleep_after.map(Value::from)),
                ("timeout_s", timeout.map(Value::from)),
                ("volume", volume),
                ("rollout", rollout),
                ("h2c", h2c.map(Value::from)),
                ("public", public.map(Value::from)),
                ("drain_s", drain.map(|d| if d == 0 { Value::Null } else { Value::from(d) })),
                ("cpu_target", cpu_target.map(|t| if t == 0 { Value::Null } else { Value::from(t) })),
                ("restart", restart.map(Value::from)),
                ("restart_tries", restart_tries.map(Value::from)),
                ("affinity", affinity.map(Value::from)),
                ("scale", match scale.as_deref() {
                    None => None,
                    Some("none") => Some(Value::Null),
                    Some(v) => {
                        let mut parts = v.splitn(3, ':');
                        let (stream, group) = (parts.next().unwrap_or(""), parts.next().ok_or_else(|| anyhow::anyhow!("--scale is <stream>:<group>[:<per instance>]"))?);
                        let mut sc = json!({ "stream": stream, "group": group });
                        if let Some(p) = parts.next() {
                            sc["per_instance"] = json!(p.parse::<u64>().map_err(|_| anyhow::anyhow!("--scale's per instance is a number"))?);
                        }
                        Some(sc)
                    }
                }),
            ] {
                if let Some(v) = v {
                    run.insert(k.into(), v);
                }
            }
            if !run.is_empty() {
                body.insert("run".into(), Value::Object(run));
            }
            if body.is_empty() {
                bail!("nothing to change: see `roll apps set --help`");
            }
            api.call(reqwest::Method::PATCH, &format!("/v1/apps/services/{}", enc(&s(&svc["id"]))), Some(Value::Object(body))).await?
        }
        AppsCmd::Delete { target, yes } => {
            let svc = service(api, &target).await?;
            confirm(yes, &s(&svc["name"]), "every environment, instance, deployment and image of the service")?;
            api.call(reqwest::Method::DELETE, &format!("/v1/apps/services/{}", enc(&s(&svc["id"]))), None).await?;
            json!({ "deleted": svc["id"] })
        }
        AppsCmd::Deploys { target } => {
            let svc = service(api, &target).await?;
            let v = api.get(&format!("{}/deploys", env_path(&svc, &target.env))).await?;
            if !json_out {
                for d in v.as_array().into_iter().flatten() {
                    let rev = d["revision"].as_str().map(|r| &r[..r.len().min(8)]).unwrap_or("-").to_string();
                    let by = d["actor"].as_str().map(|a| format!("  ({})", a.strip_prefix("git:").unwrap_or(a))).unwrap_or_default();
                    println!("{}  {:<10} {:<8} {rev:<8} {}{by}", s(&d["id"]), s(&d["status"]), s(&d["trigger"]), s(&d["message"]));
                }
                return Ok(());
            }
            v
        }
        AppsCmd::Ssh { target, instance } => {
            let svc = service(api, &target).await?;
            return ssh(api, &env_path(&svc, &target.env), instance.as_deref()).await;
        }
        AppsCmd::Proxy { target, local_port } => {
            let svc = service(api, &target).await?;
            let port = svc["run"]["port"].as_u64().unwrap_or(8080) as u16;
            return proxy(api, &env_path(&svc, &target.env), &format!("{}/{}", s(&svc["name"]), target.env), port, local_port.unwrap_or(port)).await;
        }
        AppsCmd::Run { target, timeout, command } => {
            let svc = service(api, &target).await?;
            let base = format!("{}/jobs", env_path(&svc, &target.env));
            let job = api.post(&base, json!({ "command": shell_join(&command), "timeout_s": timeout })).await?;
            if !json_out {
                let id = s(&job["id"]);
                follow(api, &format!("{base}/{}/logs?follow=1", enc(&id))).await?;
                let done = api.get(&format!("{base}/{}", enc(&id))).await?;
                match done["exit_code"].as_i64() {
                    Some(0) => return Ok(()),
                    Some(code) => std::process::exit(code.clamp(1, 255) as i32),
                    None => bail!("the job {}", s(&done["status"])),
                }
            }
            job
        }
        AppsCmd::Redeploy { target } => {
            let svc = service(api, &target).await?;
            let dep = api.post(&format!("{}/redeploy", env_path(&svc, &target.env)), json!({})).await?;
            if !json_out {
                return watch(api, &svc, &target.env, &dep).await;
            }
            dep
        }
        AppsCmd::Promote { from, target } => {
            let svc = service(api, &target).await?;
            let dep = api.post(&format!("{}/promote", env_path(&svc, &target.env)), json!({ "from": from })).await?;
            if !json_out {
                return watch(api, &svc, &target.env, &dep).await;
            }
            dep
        }
        AppsCmd::Rollback { deployment, target, keep_variables } => {
            let svc = service(api, &target).await?;
            // No id: the server picks the one before what's live, walking back through rollbacks.
            let path = match &deployment {
                Some(d) => format!("{}/deploys/{}/rollback", env_path(&svc, &target.env), enc(d)),
                None => format!("{}/rollback", env_path(&svc, &target.env)),
            };
            let dep = api.post(&path, json!({ "keep_variables": keep_variables })).await?;
            if deployment.is_none() && !json_out {
                eprintln!("rolling back to {}", s(&dep["rollback_of"]));
            }
            if !json_out {
                return watch(api, &svc, &target.env, &dep).await;
            }
            dep
        }
        AppsCmd::Vars { target, set, unset, reveal, from_file } => {
            let svc = service(api, &target).await?;
            let base = format!("{}/variables", env_path(&svc, &target.env));
            let mut m = serde_json::Map::new();
            if let Some(path) = &from_file {
                let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
                let (vars, bad) = parse_env(&text);
                if !bad.is_empty() {
                    bail!("{}: lines {} aren't KEY=value", path.display(), bad.iter().map(usize::to_string).collect::<Vec<_>>().join(", "));
                }
                m.extend(vars.into_iter().map(|(k, v)| (k, Value::String(v))));
            }
            if !set.is_empty() || !unset.is_empty() || !m.is_empty() {
                for kv in set {
                    let (k, v) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("{kv}: set variables as KEY=value"))?;
                    m.insert(k.to_string(), v.into());
                }
                api.call(reqwest::Method::PATCH, &base, Some(json!({ "set": m, "unset": unset }))).await?;
                if !json_out {
                    eprintln!("saved; `roll apps redeploy` puts them live (a new deploy or push does too)");
                }
            }
            let v = if reveal { api.get(&format!("{base}/values")).await? } else { api.get(&base).await? };
            if !json_out {
                if reveal {
                    for (k, val) in v.as_object().into_iter().flatten() {
                        println!("{k}={}", s(val));
                    }
                } else {
                    for x in v["vars"].as_array().into_iter().flatten() {
                        println!("{:<32} {:<10} {}", s(&x["name"]), s(&x["source"]), s(&x["masked"]));
                    }
                }
                return Ok(());
            }
            v
        }
        AppsCmd::Link { kind, r#ref, prefix, target } => {
            let svc = service(api, &target).await?;
            let path = format!("/v1/apps/services/{}/links/{}/{}", enc(&s(&svc["id"])), enc(&kind), enc(&r#ref));
            api.call(reqwest::Method::PUT, &path, Some(json!({ "prefix": prefix }))).await?
        }
        AppsCmd::Unlink { kind, r#ref, target } => {
            let svc = service(api, &target).await?;
            api.call(reqwest::Method::DELETE, &format!("/v1/apps/services/{}/links/{}/{}", enc(&s(&svc["id"])), enc(&kind), enc(&r#ref)), None).await?;
            json!({ "unlinked": format!("{kind}/{ref}") })
        }
        AppsCmd::Cancel { deployment, target } => {
            let svc = service(api, &target).await?;
            api.post(&format!("{}/deploys/{}/cancel", env_path(&svc, &target.env), enc(&deployment)), json!({})).await?
        }
        AppsCmd::Registry { target, username, remove } => {
            let svc = service(api, &target).await?;
            let path = format!("/v1/apps/services/{}/registry", enc(&s(&svc["id"])));
            if remove {
                api.call(reqwest::Method::DELETE, &path, None).await?
            } else {
                let user = username.ok_or_else(|| anyhow::anyhow!("--username, with the password or token on stdin (or --remove)"))?;
                let mut password = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut password)?;
                let password = password.trim_end_matches(['\r', '\n']).to_string();
                anyhow::ensure!(!password.is_empty(), "the password or token goes on stdin (never argv)");
                api.call(reqwest::Method::PUT, &path, Some(json!({ "username": user, "password": password }))).await?
            }
        }
        AppsCmd::Sleep { target } => {
            let svc = service(api, &target).await?;
            api.post(&format!("{}/sleep", env_path(&svc, &target.env)), json!({})).await?
        }
        AppsCmd::Wake { target } => {
            let svc = service(api, &target).await?;
            api.post(&format!("{}/wake", env_path(&svc, &target.env)), json!({})).await?
        }
        AppsCmd::Restart { target } => {
            let svc = service(api, &target).await?;
            api.post(&format!("{}/restart", env_path(&svc, &target.env)), json!({})).await?
        }
        AppsCmd::Metrics { target, minutes, range } => {
            let minutes = match range.as_deref() {
                None => minutes,
                Some("1h") => 60,
                Some("24h" | "1d") => 24 * 60,
                Some("7d" | "1w") => 7 * 24 * 60,
                Some(r) => bail!("--range is 1h, 24h or 7d, not {r}"),
            };
            let svc = service(api, &target).await?;
            let v = api.get(&format!("{}/metrics?minutes={minutes}", env_path(&svc, &target.env))).await?;
            if !json_out {
                let sum = |k: &str| v[k].as_array().map(|a| a.iter().filter_map(Value::as_f64).sum::<f64>()).unwrap_or(0.0);
                let max = |k: &str| v[k].as_array().map(|a| a.iter().filter_map(Value::as_f64).fold(0.0, f64::max)).unwrap_or(0.0);
                println!("requests      {}", sum("requests"));
                println!("5xx           {}", sum("errors"));
                println!("p95 (worst)   {} ms", max("p95_ms"));
                println!("cold starts   {}", sum("cold_starts"));
                println!("cold start    {:.0} ms (slowest minute)", max("cold_start_ms"));
                println!("instances     {} (most)", max("instances"));
                println!("memory        {} (most)", crate::bytes(max("memory_bytes")));
                return Ok(());
            }
            v
        }
        AppsCmd::ImportCompose { file, write } => {
            let path = match file {
                Some(f) => f,
                None => ["compose.yaml", "compose.yml", "docker-compose.yml", "docker-compose.yaml"].iter().map(PathBuf::from).find(|p| p.exists()).ok_or_else(|| anyhow::anyhow!("no compose.yaml here: name the file"))?,
            };
            let text = std::fs::read_to_string(&path).with_context(|| format!("{}", path.display()))?;
            let (mut toml_text, notes) = compose_to_toml(&text)?;
            // Roots are the Compose file's: relative to where roll.toml is written (here).
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty() && *d != Path::new(".")) {
                let d = dir.to_string_lossy().trim_end_matches('/').to_string();
                toml_text = toml_text
                    .lines()
                    .map(|l| match l.strip_prefix("root = \"") {
                        Some(r) => format!("root = \"{}\"", if r.trim_end_matches('"') == "." { d.clone() } else { format!("{d}/{}", r.trim_end_matches('"')) }),
                        None => l.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
                    + "\n";
            }
            for n in &notes {
                eprintln!("# {n}");
            }
            if write {
                let existing = std::fs::read_to_string("roll.toml").unwrap_or_default();
                anyhow::ensure!(!existing.contains("[apps.") && !existing.contains("[databases."), "roll.toml declares [apps] or [databases] already: merge by hand (print without --write)");
                let joined = if existing.trim().is_empty() { toml_text.clone() } else { format!("{}\n\n{toml_text}", existing.trim_end()) };
                // Written only if it reads back as a project.
                parse_project(&joined).context("the joined roll.toml wouldn't parse; print without --write and merge by hand")?;
                std::fs::write("roll.toml", joined)?;
                eprintln!("wrote roll.toml: `roll up --all --dry-run` shows what it would do");
                return Ok(());
            }
            if !json_out {
                print!("{toml_text}");
                return Ok(());
            }
            json!({ "toml": toml_text, "notes": notes })
        }
        AppsCmd::Hook { target, remove } => {
            let svc = service(api, &target).await?;
            let path = format!("{}/hook", env_path(&svc, &target.env));
            if remove {
                api.call(reqwest::Method::DELETE, &path, None).await?;
                json!({ "removed": true })
            } else {
                let h = api.post(&path, json!({})).await?;
                let url = format!("{}{}", api.base, s(&h["path"]));
                if !json_out {
                    println!("{url}");
                    eprintln!("shown once: a POST to it deploys {} (only its hash is kept)", target.env);
                    return Ok(());
                }
                json!({ "url": url })
            }
        }
        AppsCmd::Backups { target, now, restore, yes } => {
            let svc = service(api, &target).await?;
            let path = format!("{}/volume/backups", env_path(&svc, &target.env));
            let when = |b: &Value| b["at"].as_i64().and_then(chrono::DateTime::from_timestamp_millis).map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string()).unwrap_or_default();
            if now {
                let b = api.post(&path, json!({})).await?;
                if !json_out {
                    println!("{}  {}  {} KiB", s(&b["id"]), when(&b), b["size"].as_u64().unwrap_or(0) / 1024);
                    return Ok(());
                }
                b
            } else if let Some(id) = restore {
                confirm(yes, &target.env, "the volume's current files (they become the backup's)")?;
                let b = api.post(&format!("{path}/{}/restore", enc(&id)), json!({})).await?;
                if !json_out {
                    println!("restored {} from {}; it wakes on the next request", target.env, when(&b));
                    return Ok(());
                }
                b
            } else {
                let v = api.get(&path).await?;
                if !json_out {
                    for b in v.as_array().into_iter().flatten() {
                        println!("{}  {}  {} KiB", s(&b["id"]), when(b), b["size"].as_u64().unwrap_or(0) / 1024);
                    }
                    return Ok(());
                }
                v
            }
        }
        AppsCmd::Activity { target } => {
            let svc = service(api, &target).await?;
            let v = api.get(&format!("/v1/apps/services/{}/activity", enc(&s(&svc["id"])))).await?;
            if !json_out {
                for e in v.as_array().into_iter().flatten() {
                    let t = e["at"].as_i64().and_then(chrono::DateTime::from_timestamp_millis).map(|t| t.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_default();
                    let who = e["data"]["actor"].as_str().unwrap_or("someone");
                    let what = s(&e["kind"]).trim_start_matches("audit.").to_string();
                    let env = e["environment"].as_str().filter(|x| !x.is_empty()).map(|x| format!(" ({x})")).unwrap_or_default();
                    let mut detail = e["data"].clone();
                    if let Some(o) = detail.as_object_mut() {
                        o.remove("actor");
                    }
                    println!("{t}  {who:<16} {what}{env}  {detail}");
                }
                return Ok(());
            }
            v
        }
        AppsCmd::Drains { target, add, remove } => {
            let svc = service(api, &target).await?;
            let path = format!("/v1/apps/projects/{}/drains", enc(&s(&svc["project"])));
            if let Some(url) = add {
                let d = api.post(&path, json!({ "url": url })).await?;
                if !json_out {
                    println!("{}  {}  secret {}", s(&d["id"]), s(&d["host"]), s(&d["secret"]));
                    eprintln!("the secret is shown once: check X-Roll-Signature-256 with it");
                    return Ok(());
                }
                d
            } else if let Some(id) = remove {
                api.call(reqwest::Method::DELETE, &format!("{path}/{}", enc(&id)), None).await?;
                json!({ "deleted": id })
            } else {
                let v = api.get(&path).await?;
                if !json_out {
                    for d in v.as_array().into_iter().flatten() {
                        println!("{}  {}", s(&d["id"]), s(&d["host"]));
                    }
                    return Ok(());
                }
                v
            }
        }
        AppsCmd::Notifications { target, add, kind, remove } => {
            let svc = service(api, &target).await?;
            let path = format!("/v1/apps/projects/{}/notifications", enc(&s(&svc["project"])));
            if let Some(url) = add {
                let t = api.post(&path, json!({ "kind": kind, "url": url })).await?;
                if !json_out {
                    println!("{}  {} {}  events {}", s(&t["id"]), s(&t["kind"]), s(&t["host"]), t["events"]);
                    if let Some(secret) = t["secret"].as_str() {
                        println!("secret {secret}");
                        eprintln!("the secret is shown once: check X-Roll-Signature-256 with it");
                    }
                    return Ok(());
                }
                t
            } else if let Some(id) = remove {
                api.call(reqwest::Method::DELETE, &format!("{path}/{}", enc(&id)), None).await?;
                json!({ "deleted": id })
            } else {
                let v = api.get(&path).await?;
                if !json_out {
                    for t in v.as_array().into_iter().flatten() {
                        println!("{}  {:<8} {}  {}", s(&t["id"]), s(&t["kind"]), s(&t["host"]), t["events"]);
                    }
                    return Ok(());
                }
                v
            }
        }
        AppsCmd::Limits { target, alert, hard } => {
            let svc = service(api, &target).await?;
            let path = format!("/v1/apps/projects/{}/limits", enc(&s(&svc["project"])));
            let mut v = api.get(&path).await?;
            if alert.is_some() || hard.is_some() {
                // Unset flags keep what's there; 0 removes a limit.
                let pick = |given: Option<f64>, k: &str| match given {
                    Some(x) if x <= 0.0 => Value::Null,
                    Some(x) => json!(x),
                    None => v[k].clone(),
                };
                let body = json!({ "alert_usd": pick(alert, "alert_usd"), "hard_usd": pick(hard, "hard_usd") });
                v = api.call(reqwest::Method::PUT, &path, Some(body)).await?;
            }
            if !json_out {
                let usd = |k: &str| v[k].as_f64().map_or("none".to_string(), |x| format!("${x:.2}"));
                println!("this month  ${:.2}{}", v["month_usd"].as_f64().unwrap_or(0.0), if v["capped"] == true { "  (paused: at its limit)" } else { "" });
                println!("alert       {}", usd("alert_usd"));
                println!("hard limit  {}", usd("hard_usd"));
                return Ok(());
            }
            v
        }
        AppsCmd::Usage { target, days } => {
            let svc = service(api, &target).await?;
            let v = api.get(&format!("{}/usage?days={days}", env_path(&svc, &target.env))).await?;
            if !json_out {
                let total = |k: &str| v["days"].as_array().map(|a| a.iter().filter_map(|d| d[k].as_f64()).sum::<f64>()).unwrap_or(0.0);
                println!("CPU      {:.1} vCPU-s", total("cpu_seconds"));
                println!("memory   {:.1} GB-s", total("gb_seconds"));
                println!("requests {}", total("requests"));
                println!("egress   {}", crate::bytes(total("egress_bytes")));
                println!("builds   {:.1} min", total("build_seconds") / 60.0);
                println!("cost     ${:.4} over {days} days", v["total_usd"].as_f64().unwrap_or(0.0));
                return Ok(());
            }
            v
        }
        AppsCmd::Env { target, branch, no_branch, variables_from, wait_for_ci, run, follow_tag, private, open, maintenance, serve, allow_ip, allow_any, rate_limit, no_rate_limit } => {
            let svc = service(api, &target).await?;
            // Only what was asked changes: no `--branch` keeps the one it has.
            let mut body = serde_json::Map::new();
            if let Some(b) = branch {
                body.insert("branch".into(), json!(b));
            } else if no_branch {
                body.insert("branch".into(), Value::Null);
            }
            if let Some(f) = variables_from {
                body.insert("variables_from".into(), json!(f));
            }
            if let Some(w) = wait_for_ci {
                body.insert("wait_for_ci".into(), json!(w));
            }
            if let Some(f) = follow_tag {
                body.insert("follow_tag".into(), json!(f));
            }
            if let Some(note) = maintenance {
                body.insert("maintenance".into(), json!({ "message": Some(note).filter(|n| !n.is_empty()) }));
            } else if serve {
                body.insert("maintenance".into(), json!(false));
            }
            if !allow_ip.is_empty() {
                body.insert("allow_ips".into(), json!(allow_ip));
            } else if allow_any {
                body.insert("allow_ips".into(), Value::Null);
            }
            if let Some(r) = rate_limit {
                let (n, burst) = r.split_once(':').map_or((r.as_str(), None), |(n, b)| (n, Some(b)));
                let n: u32 = n.parse().map_err(|_| anyhow::anyhow!("--rate-limit is N or N:BURST"))?;
                let mut l = json!({ "per_minute": n });
                if let Some(b) = burst {
                    l["burst"] = json!(b.parse::<u32>().map_err(|_| anyhow::anyhow!("--rate-limit is N or N:BURST"))?);
                }
                body.insert("rate_limit".into(), l);
            } else if no_rate_limit {
                body.insert("rate_limit".into(), Value::Null);
            }
            // A new access token, made here: only its link shows it, once.
            let token = private.then(new_token);
            if let Some(t) = &token {
                body.insert("access".into(), json!({ "token": t }));
            } else if open {
                body.insert("access".into(), Value::Null);
            }
            if !run.is_empty() {
                let mut r = serde_json::Map::new();
                for kv in &run {
                    let (k, v) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("--run is field=value, like min_instances=1"))?;
                    let v = match v.trim() {
                        "" => Value::Null,
                        n => serde_json::from_str::<serde_json::Number>(n).map(Value::Number).map_err(|_| anyhow::anyhow!("--run {k}: {n} isn't a number"))?,
                    };
                    r.insert(k.trim().to_string(), v);
                }
                body.insert("run".into(), Value::Object(r));
            }
            let e = api.call(reqwest::Method::PUT, &env_path(&svc, &target.env), Some(Value::Object(body))).await?;
            if let (Some(t), Some(url)) = (&token, e["url"].as_str()) {
                println!("{url}/?roll_access={t}");
                eprintln!("shown once: only visitors with this link get in (it sets a cookie)");
                return Ok(());
            }
            e
        }
        AppsCmd::EnvDelete { target, yes } => {
            let svc = service(api, &target).await?;
            confirm(yes, &target.env, "the environment's instances, domains, variables and volume")?;
            api.call(reqwest::Method::DELETE, &env_path(&svc, &target.env), None).await?;
            json!({ "deleted": target.env })
        }
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else {
        human(&out)?;
    }
    Ok(())
}

/// An environment's access token: 32 characters of lowercase base32 from the OS's randomness
/// (the dashboard's are the same shape).
pub(crate) fn new_token() -> String {
    const ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).expect("system randomness");
    b.iter().map(|x| ALPHABET[(x & 31) as usize] as char).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_compose_file_becomes_a_projects_roll_toml() {
        let compose = "x-proxy: &proxy\n  image: nginx:1.27\nservices:\n  web:\n    build: ./apps/web\n    ports: [\"8080:3000\"]\n    environment:\n      NODE_ENV: production\n      DATABASE_URL: postgres://app:pw@db:5432/app\n    depends_on: [db, cache]\n    volumes: [\"./data:/data\"]\n  proxy:\n    <<: *proxy\n    ports:\n      - target: 80\n  db:\n    image: postgres:17\n  cache:\n    image: redis:7-alpine\n";
        let (text, notes) = compose_to_toml(compose).unwrap();
        let f = parse_project(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        let web = &f.apps["web"];
        assert_eq!((web.root.as_deref(), web.port, web.links.clone()), (Some("apps/web"), Some(3000), vec!["db".to_string(), "cache".to_string()]));
        assert_eq!(web.variables["NODE_ENV"], "production");
        assert_eq!((f.apps["proxy"].image.as_deref(), f.apps["proxy"].port), (Some("nginx:1.27"), Some(80)));
        assert_eq!((f.databases["db"].kind.as_str(), f.databases["cache"].kind.as_str()), ("postgres", "redis"));
        assert!(notes.iter().any(|n| n.starts_with("web: not carried over: volumes")), "{notes:?}");
        assert!(!web.variables.contains_key("DATABASE_URL") && notes.iter().any(|n| n.contains("DATABASE_URL left out")), "the link sets it: {notes:?}");
        assert!(compose_to_toml("services:\n  web_1:\n    image: a\n  web-1:\n    image: b\n").is_err(), "two services, one name");
    }

    #[test]
    fn a_projects_file_is_checked_and_planned() {
        let f = parse_project("[app]\nport = 3000\n[apps.web]\nroot = \"apps/web\"\nlinks = [\"db\", \"cache\"]\n[apps.proxy]\nimage = \"nginx:1.27\"\n[databases.db]\nkind = \"postgres\"\n[databases.cache]\nkind = \"redis\"\n").unwrap();
        let mut dbs = std::collections::BTreeMap::new();
        dbs.insert(("postgres".to_string(), "db".to_string()), "pg_1".to_string());
        let mut services = std::collections::BTreeMap::new();
        services.insert("proxy".to_string(), serde_json::json!({ "source": { "kind": "image", "image": "nginx:1.27" } }));
        let links = std::collections::BTreeMap::new();
        assert_eq!(
            plan_lines(&f, &services, &dbs, &links),
            ["+ redis database cache", "= postgres database db", "= service proxy (image nginx:1.27)", "+ service web (from apps/web)", "~ link web to db", "~ link web to cache", "↑ deploy web"]
        );
        // A repo worker whose settings in the file differ from the service's.
        let f2 = parse_project("[apps.jobs]\nrepo = \"acme/shop\"\nroot = \"jobs\"\nbranch = \"main\"\ntype = \"worker\"\n[apps.jobs.run]\nmin_instances = 2\n").unwrap();
        let mut have = std::collections::BTreeMap::new();
        have.insert("jobs".to_string(), serde_json::json!({ "source": { "kind": "repo" }, "run": { "type": "worker", "min_instances": 1 } }));
        assert_eq!(plan_lines(&f2, &have, &dbs, &links), ["= service jobs (repo acme/shop at jobs)", "~ settings jobs (min_instances)"]);
        assert!(parse_project("[apps.web]\nimage = \"a\"\nrepo = \"b/c\"\n").is_err(), "one source");
        assert!(parse_project("[apps.web]\nimage = \"a\"\nbranch = \"main\"\n").is_err(), "a branch is a repo's");
        for bad in ["[apps.web]\nroot = \"../x\"\n", "[apps.web]\nroot = \"a\"\nimage = \"b\"\n", "[apps.web]\nroot = \"a\"\nlinks = [\"nope\"]\n", "[apps.Web]\nroot = \"a\"\n", "[apps.web]\nroot = \"a\"\n[databases.db]\nkind = \"mysql\"\n", "[apps.web]\nroot = \"a\"\nreplicas = 1\n"] {
            assert!(parse_project(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_dot_env_file_reads_as_the_dashboard_reads_it() {
        let (vars, bad) = parse_env("# db\nexport DATABASE_URL=postgres://x\nNAME='a b'\nMULTI=\"one\\ntwo\"\n\n1BAD=x\nnope\n");
        assert_eq!(vars, [("DATABASE_URL".into(), "postgres://x".into()), ("NAME".into(), "a b".into()), ("MULTI".into(), "one\ntwo".into())]);
        assert_eq!(bad, [6, 7]);
    }

    #[test]
    fn roll_up_leaves_out_ignored_files_and_local_env_files() {
        let d = tempfile::tempdir().unwrap();
        let w = |p: &str, body: &str| {
            let p = d.path().join(p);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        };
        // The app lives in a monorepo: the parent's .gitignore counts too.
        w(".gitignore", "*.log\n");
        w("app/.gitignore", "dist/\n");
        w("app/.dockerignore", "notes.md\n");
        w("app/.rollignore", "fixtures/\n");
        for f in ["server.js", ".env.example", ".npmrc", "src/index.js"] {
            w(&format!("app/{f}"), "ok");
        }
        for f in [".env", ".env.local", "src/.env.production", "dist/bundle.js", "debug.log", "notes.md", "fixtures/big.json", "node_modules/x/index.js"] {
            w(&format!("app/{f}"), "secret or ignored");
        }
        let bundle = pack(&d.path().join("app")).unwrap();
        let out = tempfile::tempdir().unwrap();
        std::fs::create_dir(out.path().join("t")).unwrap();
        roll_fetch::unpack(bundle.as_slice(), &out.path().join("t"), &out.path().join("env"), roll_fetch::Limits::default()).unwrap();
        let mut got = walkdir(&out.path().join("t"));
        got.sort();
        assert_eq!(got, [".dockerignore", ".env.example", ".gitignore", ".npmrc", ".rollignore", "server.js", "src/index.js"]);
    }

    #[test]
    fn roll_up_names_the_checkouts_commit() {
        let d = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| assert!(std::process::Command::new("git").arg("-C").arg(d.path()).args(args).output().unwrap().status.success());
        git(&["init", "-q"]);
        std::fs::write(d.path().join("a"), "1").unwrap();
        git(&["add", "a"]);
        git(&["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "Café ✓ ready"]);
        let (sha, subject, dirty) = git_head(d.path()).unwrap();
        assert_eq!((sha.len(), subject.as_str(), dirty), (40, "Café ✓ ready", false));
        std::fs::write(d.path().join("b"), "2").unwrap();
        assert!(git_head(d.path()).unwrap().2, "an untracked file is a local change");
        assert!(git_head(tempfile::tempdir().unwrap().path()).is_none());
    }

    #[test]
    fn a_command_keeps_its_arguments_through_sh() {
        let args: Vec<String> = ["npx", "prisma", "db", "execute", "--stdin", "it's $HOME", ""].map(String::from).to_vec();
        let joined = shell_join(&args);
        let out = std::process::Command::new("sh").arg("-c").arg(format!("printf '%s|' {joined}")).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout), "npx|prisma|db|execute|--stdin|it's $HOME||");
    }

    fn walkdir(root: &Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else {
                    out.push(p.strip_prefix(root).unwrap().display().to_string());
                }
            }
        }
        out
    }
}
