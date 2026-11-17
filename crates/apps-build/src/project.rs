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
    /// The directory `roll up` uploads (relative to the file), or with `repo` or `git` the
    /// directory in the repo that builds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// Or a public image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Or a roll code repo (`owner/name`): it deploys on each landing on `branch`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    /// Or a public Git repository's URL (`https://github.com/owner/name`): it deploys on each
    /// new commit on `branch` (its default branch when none is named).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// `web`, `worker` or `cron`.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// A cron service's five fields (UTC).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schedule: Option<String>,
    /// Any other run setting, as `[app.run]` takes it (checked by the API).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<toml::Table>,
    /// Databases below, by name: their connection variables go in.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<String>,
    /// The port it listens on (an image's; a root's own roll.toml can say it too).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Variables it starts with (set when the service is made, never over what it has): a value,
    /// `{ generate = "secret" }` (made once, when it's made) or `{ ask = "what it is" }` (whoever
    /// makes it says it): Heroku's `app.json` and Render's Blueprints.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, VarSpec>,
    /// Environments besides production (Render's preview and staging environments, Railway's
    /// environments): `[apps.web.environments.staging]`, made with the service and brought to
    /// what the file says when it's read again.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environments: BTreeMap<String, EnvEntry>,
}

/// An environment of an app: the branch it follows, its own size and scale, and variables it
/// starts with (as they are: a secret goes in roll secrets).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnvEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Its own run settings over the app's: what an environment may set
    /// (`min_instances`, `max_instances`, `cpu`, `memory_mb`, `concurrency`, `suspend_after_s`,
    /// `sleep_after_s`; checked by the API).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<toml::Table>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, String>,
}

/// A variable a project file gives its service.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum VarSpec {
    Value(String),
    Generate(Generate),
    Ask(Ask),
}

/// `{ generate = "secret" }`: 32 random bytes as 64 hex characters, made when the service is.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Generate {
    pub generate: String,
}

/// `{ ask = "Your Stripe secret key" }`: asked of whoever makes the service, `default` offered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ask {
    pub ask: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

impl VarSpec {
    /// The value, when the file gives one.
    pub fn value(&self) -> Option<&str> {
        match self {
            VarSpec::Value(v) => Some(v),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DbEntry {
    /// One of [`DB_KINDS`].
    pub kind: String,
}

/// What `[databases]` can declare: roll's products a service links by name.
pub const DB_KINDS: [&str; 5] = ["postgres", "redis", "bucket", "stream", "nosql"];

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
        ensure(DB_KINDS.contains(&d.kind.as_str()), || format!("database {n}: kind is {}", DB_KINDS.join(", ")))?;
        ensure(!f.apps.contains_key(n), || format!("{n} is both a database and an app: a link would name either"))?;
    }
    for (n, a) in &f.apps {
        ensure(valid_name(n), || format!("app {n:?}: a name is a-z, 0-9 and -, at most 32"))?;
        if let Some(r) = &a.root {
            ensure(!r.starts_with('/') && !r.split('/').any(|c| c == ".."), || format!("app {n}: root is a directory inside this one (or the repo)"))?;
        }
        let sources = [a.image.is_some(), a.repo.is_some(), a.git.is_some()].into_iter().filter(|s| *s).count();
        if !(sources == 1 && !(a.image.is_some() && a.root.is_some()) || sources == 0 && a.root.is_some()) {
            return Err(err(format!("app {n}: give one of root (a directory to upload), image, repo or git (with root inside it)")));
        }
        ensure(a.branch.is_none() || a.repo.is_some() || a.git.is_some(), || format!("app {n}: branch is for a repo or git"))?;
        ensure(a.kind.as_deref().is_none_or(|k| ["web", "worker", "cron"].contains(&k)), || format!("app {n}: type is web, worker or cron"))?;
        // A database, or another app (Render's `fromService`: its private and public URLs).
        for l in &a.links {
            ensure(l != n, || format!("app {n} links itself"))?;
            ensure(f.databases.contains_key(l) || f.apps.contains_key(l), || format!("app {n} links {l}, which isn't in [databases] or [apps]"))?;
        }
        for (e, env) in &a.environments {
            ensure(valid_name(e) && e != "production", || format!("app {n}: environment {e:?} is a-z, 0-9 and - (production is the app's own settings)"))?;
            ensure(env.branch.is_none() || a.repo.is_some() || a.git.is_some(), || format!("app {n}: {e}'s branch is for a repo or git"))?;
        }
        for (k, v) in &a.variables {
            match v {
                VarSpec::Value(_) => {}
                VarSpec::Generate(g) => ensure(g.generate == "secret", || format!("app {n}: {k}'s generate is \"secret\""))?,
                VarSpec::Ask(q) => ensure(!q.ask.trim().is_empty() && q.ask.len() <= 200 && q.default.as_ref().is_none_or(|d| d.len() <= 2000), || format!("app {n}: {k}'s ask is what to ask, at most 200 characters"))?,
            }
        }
    }
    Ok(f)
}

/// A project as roll.toml's `[apps]` and `[databases]`, `notes` first as comments (what a file
/// doesn't carry). [`parse_project`] reads it back as it was.
pub fn to_toml(f: &ProjectFile, notes: &[String]) -> Result<String, ProjectError> {
    let body = toml::to_string(f).map_err(|e| err(format!("the project as roll.toml: {e}")))?;
    let mut out = String::from("# roll.toml: this project's services and databases (`roll up --all` makes what's missing).\n");
    for n in notes {
        for line in n.lines() {
            out.push_str("# ");
            out.push_str(line);
            out.push('\n');
        }
    }
    out.push('\n');
    out.push_str(&body);
    Ok(out)
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

/// The source a new service from this entry is made with (`POST /v1/apps/services`' `source`).
pub fn entry_source(a: &AppEntry) -> Value {
    let mut s = match (&a.repo, &a.git, &a.image) {
        (Some(r), _, _) => serde_json::json!({ "kind": "repo", "repo": r }),
        (None, Some(u), _) => serde_json::json!({ "kind": "git", "url": u }),
        (None, None, Some(i)) => return serde_json::json!({ "kind": "image", "image": i }),
        (None, None, None) => return serde_json::json!({ "kind": "upload" }),
    };
    if let Some(root) = &a.root {
        s["root"] = Value::from(root.as_str());
    }
    s
}

/// What a link names in `f`: a database's kind, or `service` for another app.
pub fn link_kind<'a>(f: &'a ProjectFile, link: &str) -> &'a str {
    f.databases.get(link).map_or("service", |d| d.kind.as_str())
}

/// The kind of source an entry makes.
pub fn entry_kind(a: &AppEntry) -> &'static str {
    if a.repo.is_some() {
        "repo"
    } else if a.git.is_some() {
        "git"
    } else if a.image.is_some() {
        "image"
    } else {
        "upload"
    }
}

/// The most nodes a Compose file may expand to, its aliases' copies included.
pub const COMPOSE_NODES: usize = 100_000;

/// Its nodes counted as a reader would see them (aliases replayed), without building any:
/// an error past [`COMPOSE_NODES`].
fn count_nodes(text: &str) -> Result<(), ProjectError> {
    use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};
    use std::cell::Cell;
    #[derive(Clone, Copy)]
    struct Count<'a>(&'a Cell<usize>);
    impl Count<'_> {
        fn one<E: serde::de::Error>(self) -> Result<(), E> {
            let n = self.0.get() + 1;
            self.0.set(n);
            if n > COMPOSE_NODES { Err(E::custom(format!("it expands past {COMPOSE_NODES} nodes (YAML aliases)"))) } else { Ok(()) }
        }
    }
    impl<'de> DeserializeSeed<'de> for Count<'_> {
        type Value = ();
        fn deserialize<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_any(self)
        }
    }
    impl<'de> Visitor<'de> for Count<'_> {
        type Value = ();
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("YAML")
        }
        fn visit_bool<E: serde::de::Error>(self, _: bool) -> Result<(), E> {
            self.one()
        }
        fn visit_i64<E: serde::de::Error>(self, _: i64) -> Result<(), E> {
            self.one()
        }
        fn visit_u64<E: serde::de::Error>(self, _: u64) -> Result<(), E> {
            self.one()
        }
        fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<(), E> {
            self.one()
        }
        fn visit_str<E: serde::de::Error>(self, _: &str) -> Result<(), E> {
            self.one()
        }
        fn visit_unit<E: serde::de::Error>(self) -> Result<(), E> {
            self.one()
        }
        fn visit_none<E: serde::de::Error>(self) -> Result<(), E> {
            self.one()
        }
        fn visit_some<D: serde::Deserializer<'de>>(self, d: D) -> Result<(), D::Error> {
            d.deserialize_any(self)
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<(), A::Error> {
            self.one()?;
            while a.next_element_seed(self)?.is_some() {}
            Ok(())
        }
        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<(), A::Error> {
            self.one()?;
            while a.next_key_seed(self)?.is_some() {
                a.next_value_seed(self)?;
            }
            Ok(())
        }
        fn visit_enum<A: serde::de::EnumAccess<'de>>(self, a: A) -> Result<(), A::Error> {
            use serde::de::VariantAccess;
            let ((), v) = a.variant_seed(self)?;
            v.newtype_variant_seed(self)
        }
    }
    let n = Cell::new(0);
    Count(&n).deserialize(serde_yaml_ng::Deserializer::from_str(text)).map_err(|e| err(format!("the Compose file isn't one roll reads: {e}")))
}

/// A Compose file as roll.toml text: image and build services as `[apps.<name>]`, Postgres and
/// Redis containers as roll's own `[databases.<name>]` (linked from what depends on them), and
/// notes on what isn't carried over (volumes, networks, commands, health checks).
pub fn compose_to_toml(text: &str) -> Result<(String, Vec<String>), ProjectError> {
    // Aliases expand as the file is read (a "billion laughs" file of a few KB is gigabytes): its
    // nodes are counted first, stopping at the budget, before any value is built.
    count_nodes(text)?;
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

    /// A file declares buckets, streams and NoSQL databases, and an app links another by name
    /// (its URLs); a self-link or a name that's both an app and a database is refused.
    #[test]
    fn a_file_links_buckets_streams_and_other_apps() {
        let f = parse_project("[apps.web]\nimage = \"a:1\"\nlinks = [\"api\", \"uploads\"]\n[apps.api]\nimage = \"b:1\"\nlinks = [\"events\", \"db\"]\n[databases.uploads]\nkind = \"bucket\"\n[databases.events]\nkind = \"stream\"\n[databases.db]\nkind = \"nosql\"\n").unwrap();
        assert_eq!(f.apps["web"].links.iter().map(|l| link_kind(&f, l)).collect::<Vec<_>>(), ["service", "bucket"]);
        assert_eq!(f.apps["api"].links.iter().map(|l| link_kind(&f, l)).collect::<Vec<_>>(), ["stream", "nosql"]);
        assert!(parse_project("[apps.web]\nimage = \"a:1\"\nlinks = [\"web\"]\n").is_err(), "a self-link");
        assert!(parse_project("[apps.db]\nimage = \"a:1\"\n[databases.db]\nkind = \"redis\"\n").is_err(), "one name, two things");
        assert!(parse_project("[apps.web]\nimage = \"a:1\"\n[databases.q]\nkind = \"mysql\"\n").is_err(), "a kind roll doesn't have");
        // Environments besides production: a branch (a repo's or a Git URL's), its own size.
        let f = parse_project("[apps.web]\ngit = \"https://github.com/acme/site\"\n[apps.web.environments.staging]\nbranch = \"develop\"\nrun = { max_instances = 1 }\nvariables = { LOG_LEVEL = \"debug\" }\n").unwrap();
        assert_eq!(f.apps["web"].environments["staging"].branch.as_deref(), Some("develop"));
        assert!(parse_project("[apps.web]\nimage = \"a:1\"\n[apps.web.environments.production]\n").is_err(), "production is the app's own");
        assert!(parse_project("[apps.web]\nimage = \"a:1\"\n[apps.web.environments.staging]\nbranch = \"x\"\n").is_err(), "an image has no branch");
    }

    #[test]
    fn a_compose_file_becomes_a_projects_roll_toml() {
        let compose = "x-proxy: &proxy\n  image: nginx:1.27\nservices:\n  web:\n    build: ./apps/web\n    ports: [\"8080:3000\"]\n    environment:\n      NODE_ENV: production\n      DATABASE_URL: postgres://app:pw@db:5432/app\n    depends_on: [db, cache]\n    volumes: [\"./data:/data\"]\n  proxy:\n    <<: *proxy\n    ports:\n      - target: 80\n  db:\n    image: postgres:17\n  cache:\n    image: redis:7-alpine\n";
        let (text, notes) = compose_to_toml(compose).unwrap();
        let f = parse_project(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
        let web = &f.apps["web"];
        assert_eq!((web.root.as_deref(), web.port, web.links.clone()), (Some("apps/web"), Some(3000), vec!["db".to_string(), "cache".to_string()]));
        assert_eq!(web.variables["NODE_ENV"].value(), Some("production"));
        assert_eq!((f.apps["proxy"].image.as_deref(), f.apps["proxy"].port), (Some("nginx:1.27"), Some(80)));
        assert_eq!((f.databases["db"].kind.as_str(), f.databases["cache"].kind.as_str()), ("postgres", "redis"));
        assert!(notes.iter().any(|n| n.starts_with("web: not carried over: volumes")), "{notes:?}");
        assert!(!web.variables.contains_key("DATABASE_URL") && notes.iter().any(|n| n.contains("DATABASE_URL left out")), "the link sets it: {notes:?}");
        assert!(compose_to_toml("services:\n  web_1:\n    image: a\n  web-1:\n    image: b\n").is_err(), "two services, one name");
        // A few KB that aliases make gigabytes stops at the budget, before anything is built.
        let bomb = format!("x: &a [{}]\ny: [{}]\nservices: {{}}\n", vec!["1"; 2000].join(","), vec!["*a"; 2000].join(","));
        let e = compose_to_toml(&bomb).unwrap_err().to_string();
        assert!(e.contains("expands past"), "{e}");
    }
}
