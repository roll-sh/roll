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
}

#[derive(Subcommand)]
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
        /// A cron service's schedule (five fields, UTC).
        #[arg(long)]
        schedule: Option<String>,
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
        #[arg(long)]
        health: Option<String>,
        #[arg(long)]
        schedule: Option<String>,
    },
    /// Delete a service: its environments, instances, deployments and images.
    Delete {
        #[command(flatten)]
        target: Target,
    },
    /// Deployments, newest first.
    Deploys {
        #[command(flatten)]
        target: Target,
    },
    /// Roll back to an earlier deployment (no build: its image runs again).
    Rollback {
        deployment: String,
        #[command(flatten)]
        target: Target,
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
    },
    /// This month's use and what it costs.
    Usage {
        #[command(flatten)]
        target: Target,
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
    /// Make or change an environment (`--branch` deploys on pushes to it).
    Env {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        branch: Option<String>,
    },
    /// Tear an environment down.
    EnvDelete {
        #[command(flatten)]
        target: Target,
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

/// Pack a directory as a `roll-bundle-1` (what the server unpacks), leaving out what never
/// belongs in a build.
fn pack(dir: &Path) -> anyhow::Result<Vec<u8>> {
    const SKIP: &[&str] = &[".git", ".roll", "node_modules", "target", ".venv", "__pycache__", ".next", ".turbo", ".DS_Store"];
    let staging = tempfile::tempdir()?;
    fn copy(from: &Path, to: &Path, files: &mut u64) -> anyhow::Result<()> {
        for e in std::fs::read_dir(from)? {
            let e = e?;
            let name = e.file_name();
            if SKIP.iter().any(|s| name == *s) {
                continue;
            }
            let ty = e.file_type()?;
            let dest = to.join(&name);
            if ty.is_dir() {
                std::fs::create_dir(&dest)?;
                copy(&e.path(), &dest, files)?;
            } else if ty.is_file() {
                std::fs::copy(e.path(), &dest)?;
                *files += 1;
            } else if ty.is_symlink() {
                #[cfg(unix)]
                std::os::unix::fs::symlink(std::fs::read_link(e.path())?, &dest)?;
            }
        }
        Ok(())
    }
    let mut files = 0;
    copy(dir, staging.path(), &mut files)?;
    if files == 0 {
        bail!("{} has nothing to deploy", dir.display());
    }
    let mut out = Vec::new();
    roll_fetch::pack(&mut out, &Default::default(), Some(staging.path()), roll_fetch::Limits::default())?;
    Ok(out)
}

/// Print a log's lines (SSE `data:` JSON) until the stream ends.
async fn follow(api: &Api, path: &str) -> anyhow::Result<()> {
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
            {
                print_line(&v);
            }
        }
    }
    Ok(())
}

fn print_line(l: &Value) {
    let t = l["t"].as_i64().and_then(chrono::DateTime::from_timestamp_millis).map(|t| t.format("%H:%M:%S").to_string()).unwrap_or_default();
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
    let dir = match u.dir {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    let dir = std::fs::canonicalize(&dir).with_context(|| format!("{}", dir.display()))?;
    let link = read_link(&dir);
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
    if let Some(m) = &u.message {
        req = req.header("x-roll-message", m);
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

pub async fn deploy(api: &Api, json_out: bool, d: Deploy) -> anyhow::Result<()> {
    let svc = service(api, &d.target).await?;
    let body = json!({ "revision": d.revision, "image": d.image });
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
                s(&all[0]["id"])
            } else {
                b.clone()
            };
            format!("{base}/deploys/{}/logs", enc(&dep))
        }
        None => format!("{base}/logs"),
    };
    if l.follow {
        return follow(api, &format!("{path}?follow=1&tail={}", l.tail)).await;
    }
    let lines = api.get(&format!("{path}?tail={}", l.tail)).await?;
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
                println!("or, for an apex domain, keep its records and add:");
                println!("  TXT    _roll-challenge.{}  \"{}\"", s(&v["hostname"]), s(&v["token"]));
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
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
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
        AppsCmd::Create { name, project, repo, root, image, kind, port, schedule } => {
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
        AppsCmd::Set { target, min, max, cpu, memory, port, start, health, schedule } => {
            let svc = service(api, &target).await?;
            let mut run = serde_json::Map::new();
            for (k, v) in [
                ("min_instances", min.map(Value::from)),
                ("max_instances", max.map(Value::from)),
                ("cpu", cpu.map(Value::from)),
                ("memory_mb", memory.map(Value::from)),
                ("port", port.map(Value::from)),
                ("start", start.map(Value::from)),
                ("health_path", health.map(Value::from)),
                ("schedule", schedule.map(Value::from)),
            ] {
                if let Some(v) = v {
                    run.insert(k.into(), v);
                }
            }
            api.call(reqwest::Method::PATCH, &format!("/v1/apps/services/{}", enc(&s(&svc["id"]))), Some(json!({ "run": run }))).await?
        }
        AppsCmd::Delete { target } => {
            let svc = service(api, &target).await?;
            api.call(reqwest::Method::DELETE, &format!("/v1/apps/services/{}", enc(&s(&svc["id"]))), None).await?;
            json!({ "deleted": svc["id"] })
        }
        AppsCmd::Deploys { target } => {
            let svc = service(api, &target).await?;
            let v = api.get(&format!("{}/deploys", env_path(&svc, &target.env))).await?;
            if !json_out {
                for d in v.as_array().into_iter().flatten() {
                    let rev = d["revision"].as_str().map(|r| &r[..r.len().min(8)]).unwrap_or("-").to_string();
                    println!("{}  {:<10} {:<8} {rev:<8} {}", s(&d["id"]), s(&d["status"]), s(&d["trigger"]), s(&d["message"]));
                }
                return Ok(());
            }
            v
        }
        AppsCmd::Rollback { deployment, target } => {
            let svc = service(api, &target).await?;
            let dep = api.post(&format!("{}/deploys/{}/rollback", env_path(&svc, &target.env), enc(&deployment)), json!({})).await?;
            if !json_out {
                return watch(api, &svc, &target.env, &dep).await;
            }
            dep
        }
        AppsCmd::Vars { target, set, unset, reveal } => {
            let svc = service(api, &target).await?;
            let base = format!("{}/variables", env_path(&svc, &target.env));
            if !set.is_empty() || !unset.is_empty() {
                let mut m = serde_json::Map::new();
                for kv in set {
                    let (k, v) = kv.split_once('=').ok_or_else(|| anyhow::anyhow!("{kv}: set variables as KEY=value"))?;
                    m.insert(k.to_string(), v.into());
                }
                api.call(reqwest::Method::PATCH, &base, Some(json!({ "set": m, "unset": unset }))).await?;
                if !json_out {
                    eprintln!("saved; `roll deploy` (or `roll up`) puts them live");
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
        AppsCmd::Metrics { target, minutes } => {
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
        AppsCmd::Env { target, branch } => {
            let svc = service(api, &target).await?;
            api.call(reqwest::Method::PUT, &env_path(&svc, &target.env), Some(json!({ "branch": branch }))).await?
        }
        AppsCmd::EnvDelete { target } => {
            let svc = service(api, &target).await?;
            api.call(reqwest::Method::DELETE, &env_path(&svc, &target.env), None).await?;
            json!({ "deleted": target.env })
        }
    };
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
