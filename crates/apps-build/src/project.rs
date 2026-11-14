//! A project in one file: a root roll.toml's `[apps]` and `[databases]` (Render's Blueprints),
//! and a Compose file turned into them (Railway's Compose import). `roll up --all` and
//! `POST /v1/apps/import` (the dashboard's import) both read them here; the design is "A project
//! in one file" in docs/apps/README.md. Text only: no files, no network.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// Why a file can't be a project, said for a person.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ProjectError(pub String);

fn err(m: impl Into<String>) -> ProjectError {
    ProjectError(m.into())
}

fn ensure(ok: bool, m: impl FnOnce() -> String) -> Result<(), ProjectError> {
    if ok { Ok(()) } else { Err(ProjectError(m())) }
}

/// A root roll.toml's project: the services (`[apps.<name>]`) and databases
/// (`[databases.<name>]`) it declares (Render's Blueprints). Each app's own settings stay in
/// its root's roll.toml `[app]`.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ProjectFile {
    #[serde(default)]
    pub apps: BTreeMap<String, AppEntry>,
    #[serde(default)]
    pub databases: BTreeMap<String, DbEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppEntry {
    /// The directory `roll up` uploads (relative to the file), or with `repo` the directory
    /// in the repo that builds.
    pub root: Option<String>,
    /// Or a public image.
    pub image: Option<String>,
    /// Or a roll code repo (`owner/name`): it deploys on each landing on `branch`.
    pub repo: Option<String>,
    pub branch: Option<String>,
    /// `web`, `worker` or `cron`.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub start: Option<String>,
    /// A cron service's five fields (UTC).
    pub schedule: Option<String>,
    /// Any other run setting, as `[app.run]` takes it (checked by the API).
    pub run: Option<toml::Table>,
    /// Databases below, by name: their connection variables go in.
    #[serde(default)]
    pub links: Vec<String>,
    /// The port it listens on (an image's; a root's own roll.toml can say it too).
    pub port: Option<u16>,
    /// Variables it starts with (set when the service is made, never over what it has).
    #[serde(default)]
    pub variables: BTreeMap<String, String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbEntry {
    /// `postgres` or `redis`.
    pub kind: String,
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 32 && n.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !n.starts_with('-') && !n.ends_with('-')
}

/// The `[apps]` and `[databases]` of `text` (the rest of roll.toml is other things'), checked.
pub fn parse_project(text: &str) -> Result<ProjectFile, ProjectError> {
    let v: toml::Table = toml::from_str(text).map_err(|e| err(format!("roll.toml isn't TOML: {e}")))?;
    let mut t = toml::Table::new();
    for k in ["apps", "databases"] {
        if let Some(x) = v.get(k) {
            t.insert(k.into(), x.clone());
        }
    }
    let f: ProjectFile = toml::Value::Table(t).try_into().map_err(|e| err(format!("roll.toml's [apps] or [databases]: {e}")))?;
    ensure(!f.apps.is_empty(), || "roll.toml declares no [apps.<name>]".to_string())?;
    for (n, d) in &f.databases {
        ensure(valid_name(n), || format!("database {n:?}: a name is a-z, 0-9 and -, at most 32"))?;
        ensure(matches!(d.kind.as_str(), "postgres" | "redis"), || format!("database {n}: kind is postgres or redis"))?;
    }
    for (n, a) in &f.apps {
        ensure(valid_name(n), || format!("app {n:?}: a name is a-z, 0-9 and -, at most 32"))?;
        if let Some(r) = &a.root {
            ensure(!r.starts_with('/') && !r.split('/').any(|c| c == ".."), || format!("app {n}: root is a directory inside this one (or the repo)"))?;
        }
        match (&a.root, &a.image, &a.repo) {
            (Some(_), None, None) | (None, Some(_), None) | (_, None, Some(_)) => {}
            _ => return Err(err(format!("app {n}: give one of root (a directory to upload), image, or repo (with root inside it)"))),
        }
        ensure(a.branch.is_none() || a.repo.is_some(), || format!("app {n}: branch is for a repo"))?;
        ensure(a.kind.as_deref().is_none_or(|k| ["web", "worker", "cron"].contains(&k)), || format!("app {n}: type is web, worker or cron"))?;
        for l in &a.links {
            ensure(f.databases.contains_key(l), || format!("app {n} links {l}, which isn't in [databases]"))?;
        }
    }
    Ok(f)
}

/// The run settings an entry names (its `run` table, then `type`, `start`, `schedule` and
/// `port` over it): what a new service is made with, and what an existing one is set to.
pub fn entry_run(a: &AppEntry) -> serde_json::Map<String, Value> {
    let mut run = match a.run.as_ref().map(serde_json::to_value) {
        Some(Ok(Value::Object(m))) => m,
        _ => serde_json::Map::new(),
    };
    for (k, v) in [("type", a.kind.clone()), ("start", a.start.clone()), ("schedule", a.schedule.clone())] {
        if let Some(v) = v {
            run.insert(k.into(), json!(v));
        }
    }
    if let Some(p) = a.port {
        run.insert("port".into(), json!(p));
    }
    run
}

/// The kind of source an entry makes.
pub fn entry_kind(a: &AppEntry) -> &'static str {
    if a.repo.is_some() {
        "repo"
    } else if a.image.is_some() {
        "image"
    } else {
        "upload"
    }
}

/// A Compose file as roll.toml text: image and build services as `[apps.<name>]`, Postgres and
/// Redis containers as roll's own `[databases.<name>]` (linked from what depends on them), and
/// notes on what isn't carried over (volumes, networks, commands, health checks).
pub fn compose_to_toml(text: &str) -> Result<(String, Vec<String>), ProjectError> {
    let mut v: serde_yaml_ng::Value = serde_yaml_ng::from_str(text).map_err(|e| err(format!("the Compose file isn't YAML: {e}")))?;
    // `<<: *defaults`, as Compose files share settings.
    v.apply_merge().map_err(|e| err(format!("the Compose file's merge keys: {e}")))?;
    let services = v.get("services").and_then(|s| s.as_mapping()).ok_or_else(|| err("the Compose file has no services"))?;
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
    let name_of = |k: &str| -> Result<String, ProjectError> {
        let n: String = k.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>().trim_matches('-').to_string();
        ensure(valid_name(&n), || format!("service {k:?} has no name roll can use (a-z, 0-9 and -, at most 32)"))?;
        Ok(n)
    };
    let mut dbs = std::collections::BTreeMap::new();
    let mut names = std::collections::BTreeMap::new();
    for (k, svc) in services {
        let k = str_of(k).ok_or_else(|| err("a service's name isn't text"))?;
        let n = name_of(&k)?;
        if let Some(other) = names.insert(n.clone(), k.clone()) {
            return Err(err(format!("services {other:?} and {k:?} would both be {n}: rename one")));
        }
        if let Some(kind) = svc.get("image").and_then(str_of).as_deref().and_then(db_kind) {
            dbs.insert(k.clone(), (n, kind));
        }
    }
    let q = |s: &str| toml::Value::String(s.to_string()).to_string();
    let (mut out, mut notes) = (String::new(), Vec::new());
    for (k, svc) in services {
        let k = str_of(k).ok_or_else(|| err("a service's name isn't text"))?;
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
            (None, None) => return Err(err(format!("service {k} has neither image nor build"))),
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
}
