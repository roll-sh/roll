//! Another platform's file as roll.toml text (Render's Heroku migration, Railway's and Koyeb's
//! imports): Render's `render.yaml` (Blueprint), Fly's `fly.toml`, Railway's `railway.json` or
//! `railway.toml`, and Heroku's `app.json` with its `Procfile`. What maps onto roll.toml does
//! (services, their kind, start and pre-deploy commands, ports, health paths, instances, memory,
//! variables, Postgres and Redis as roll's own databases linked from what uses them); the rest is
//! a note saying what wasn't carried over. Reads text only, as [`crate::project::compose_to_toml`].

use crate::project::ProjectError;
use toml::{Table, Value};

fn err(m: impl Into<String>) -> ProjectError {
    ProjectError(m.into())
}

/// A name roll can use: a-z, 0-9 and -, at most 32.
fn name_of(s: &str) -> String {
    let n: String = s.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let n = n.trim_matches('-').chars().take(32).collect::<String>();
    n.trim_matches('-').to_string()
}

/// Every app and database, as roll.toml.
#[derive(Default)]
struct Out {
    apps: Table,
    databases: Table,
    notes: Vec<String>,
}

impl Out {
    fn app(&mut self, name: &str) -> Result<&mut Table, ProjectError> {
        let n = name_of(name);
        if n.is_empty() {
            return Err(err(format!("{name:?} has no name roll can use (a-z, 0-9 and -)")));
        }
        Ok(self.apps.entry(n).or_insert_with(|| Value::Table(Table::new())).as_table_mut().expect("a table"))
    }

    fn database(&mut self, name: &str, kind: &str) -> String {
        let n = name_of(name);
        let mut t = Table::new();
        t.insert("kind".into(), Value::String(kind.into()));
        self.databases.insert(n.clone(), Value::Table(t));
        n
    }

    fn finish(self) -> Result<(String, Vec<String>), ProjectError> {
        if self.apps.is_empty() {
            return Err(err("the file names no service roll can run"));
        }
        let mut doc = Table::new();
        doc.insert("apps".into(), Value::Table(self.apps));
        if !self.databases.is_empty() {
            doc.insert("databases".into(), Value::Table(self.databases));
        }
        let text = toml::to_string(&doc).map_err(|e| err(e.to_string()))?;
        // As roll up --all reads it: checked before it's answered.
        crate::project::parse_project(&text)?;
        Ok((text, self.notes))
    }
}

fn run_of(t: &mut Table) -> &mut Table {
    t.entry("run").or_insert_with(|| Value::Table(Table::new())).as_table_mut().expect("a table")
}

fn vars_of(t: &mut Table) -> &mut Table {
    t.entry("variables").or_insert_with(|| Value::Table(Table::new())).as_table_mut().expect("a table")
}

fn link(t: &mut Table, db: &str) {
    let links = t.entry("links").or_insert_with(|| Value::Array(Vec::new())).as_array_mut().expect("an array");
    if !links.iter().any(|l| l.as_str() == Some(db)) {
        links.push(Value::String(db.into()));
    }
}

fn spec(key: &str) -> Value {
    let mut t = Table::new();
    t.insert(key.into(), Value::String(if key == "generate" { "secret".into() } else { "set it when the service is made".into() }));
    Value::Table(t)
}

/// A JSON or YAML scalar as text.
fn text(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Render's `render.yaml` (a Blueprint): its services and databases.
pub fn render_to_toml(yaml: &str) -> Result<(String, Vec<String>), ProjectError> {
    crate::project::count_nodes(yaml)?;
    let v: serde_json::Value = serde_yaml_ng::from_str(yaml).map_err(|e| err(format!("render.yaml isn't YAML: {e}")))?;
    let mut out = Out::default();
    let mut dbs = std::collections::BTreeMap::new();
    for d in v["databases"].as_array().into_iter().flatten() {
        let Some(n) = d["name"].as_str() else { continue };
        dbs.insert(n.to_string(), out.database(n, "postgres"));
        out.notes.push(format!("{n}: a roll Postgres database; its link sets DATABASE_URL"));
    }
    let services = v["services"].as_array().ok_or_else(|| err("render.yaml has no services"))?;
    for s in services.iter().filter(|s| matches!(s["type"].as_str(), Some("redis" | "keyvalue"))) {
        let Some(n) = s["name"].as_str() else { continue };
        dbs.insert(n.to_string(), out.database(n, "redis"));
        out.notes.push(format!("{n}: a roll Redis database; its link sets REDIS_URL"));
    }
    for s in services {
        let (Some(kind), Some(name)) = (s["type"].as_str(), s["name"].as_str()) else { continue };
        if matches!(kind, "redis" | "keyvalue") {
            continue;
        }
        let mut notes = Vec::new();
        let t = out.app(name)?;
        t.insert("type".into(), Value::String(match kind {
            "worker" => "worker",
            "cron" => "cron",
            _ => "web",
        }.into()));
        if kind == "pserv" {
            run_of(t).insert("public".into(), Value::Boolean(false));
        }
        match s["image"]["url"].as_str() {
            Some(img) => {
                t.insert("image".into(), Value::String(img.into()));
            }
            None => {
                // Its folder in the repository (none: the file's own).
                let root = s["rootDir"].as_str().unwrap_or(".").trim_start_matches("./").trim_end_matches('/');
                t.insert("root".into(), Value::String(if root.is_empty() { ".".into() } else { root.to_string() }));
            }
        }
        if let Some(c) = s["startCommand"].as_str().or(s["dockerCommand"].as_str()) {
            t.insert("start".into(), Value::String(c.into()));
        }
        if let Some(c) = s["schedule"].as_str() {
            t.insert("schedule".into(), Value::String(c.into()));
        }
        if let Some(c) = s["preDeployCommand"].as_str() {
            run_of(t).insert("pre_deploy".into(), Value::String(c.into()));
        }
        if let Some(p) = s["healthCheckPath"].as_str() {
            run_of(t).insert("health_path".into(), Value::String(p.into()));
        }
        if let Some(n) = s["numInstances"].as_i64().filter(|n| *n >= 1) {
            run_of(t).insert("min_instances".into(), Value::Integer(n));
        }
        if let Some(b) = s["buildCommand"].as_str() {
            notes.push(format!("{name}: its build command ({b}) isn't carried over: roll detects the build (roll.toml [app] build in its root sets one)"));
        }
        for e in s["envVars"].as_array().into_iter().flatten() {
            let Some(k) = e["key"].as_str() else {
                if let Some(g) = e["fromGroup"].as_str() {
                    notes.push(format!("{name}: the env group {g} isn't carried over: the project's roll secrets hold shared values"));
                }
                continue;
            };
            if let Some(db) = e["fromDatabase"]["name"].as_str().or(e["fromService"]["name"].as_str()).and_then(|n| dbs.get(n)) {
                link(t, db);
                notes.push(format!("{name}: {k} left out: the link to {db} sets its URL"));
            } else if e.get("fromService").is_some() {
                notes.push(format!("{name}: {k} (from another service) isn't carried over: link the service, and its URL is set"));
            } else if e["generateValue"] == true {
                vars_of(t).insert(k.into(), spec("generate"));
            } else if e["sync"] == false {
                vars_of(t).insert(k.into(), spec("ask"));
            } else if let Some(val) = text(&e["value"]) {
                vars_of(t).insert(k.into(), Value::String(val));
            }
        }
        for f in ["disk", "domains", "scaling", "autoDeploy", "buildFilter"] {
            if s.get(f).is_some() {
                notes.push(format!("{name}: {f} isn't carried over (set it in the service's settings)"));
            }
        }
        out.notes.extend(notes);
    }
    out.finish()
}

/// A size as Fly writes it (`512mb`, `1gb`, `2048`): megabytes.
fn megabytes(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(n) => Some(*n),
        Value::String(s) => {
            let s = s.trim().to_ascii_lowercase();
            if let Some(g) = s.strip_suffix("gb") {
                g.trim().parse::<f64>().ok().map(|g| (g * 1024.0) as i64)
            } else {
                s.trim_end_matches("mb").trim().parse().ok()
            }
        }
        _ => None,
    }
}

/// Fly's `fly.toml`: its app (each of its processes a service), build, env, port and machines.
pub fn fly_to_toml(text: &str) -> Result<(String, Vec<String>), ProjectError> {
    let f: Table = text.parse().map_err(|e| err(format!("fly.toml isn't TOML: {e}")))?;
    let app = f.get("app").and_then(Value::as_str).map(name_of).filter(|n| !n.is_empty()).unwrap_or_else(|| "web".into());
    let mut out = Out::default();
    let http = f.get("http_service").and_then(Value::as_table);
    let port = http.and_then(|h| h.get("internal_port")).or_else(|| f.get("services").and_then(Value::as_array).and_then(|s| s.first()).and_then(|s| s.get("internal_port"))).and_then(Value::as_integer);
    let web_procs: Vec<String> = http.and_then(|h| h.get("processes")).and_then(Value::as_array).map(|a| a.iter().filter_map(|p| p.as_str().map(str::to_string)).collect()).unwrap_or_else(|| vec!["app".into()]);
    let processes: Vec<(String, Option<String>)> = match f.get("processes").and_then(Value::as_table) {
        Some(p) if !p.is_empty() => p.iter().map(|(k, v)| (k.clone(), v.as_str().map(str::to_string))).collect(),
        _ => vec![("app".into(), None)],
    };
    let single = processes.len() == 1;
    for (proc_name, cmd) in &processes {
        let name = if single { app.clone() } else { format!("{app}-{}", name_of(proc_name)) };
        let web = web_procs.contains(proc_name);
        let t = out.app(&name)?;
        t.insert("type".into(), Value::String(if web { "web" } else { "worker" }.into()));
        match f.get("build").and_then(|b| b.get("image")).and_then(Value::as_str) {
            Some(img) => t.insert("image".into(), Value::String(img.into())),
            None => t.insert("root".into(), Value::String(".".into())),
        };
        if let Some(c) = cmd {
            t.insert("start".into(), Value::String(c.clone()));
        }
        if let (true, Some(p)) = (web, port) {
            t.insert("port".into(), Value::Integer(p));
        }
        if let Some(env) = f.get("env").and_then(Value::as_table) {
            for (k, v) in env {
                let val = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                vars_of(t).insert(k.clone(), Value::String(val));
            }
        }
        if let Some(c) = f.get("deploy").and_then(|d| d.get("release_command")).and_then(Value::as_str) {
            run_of(t).insert("pre_deploy".into(), Value::String(c.into()));
        }
        // Its `[metrics]` (one, or one per process): scraped as Fly does.
        let metrics = match f.get("metrics") {
            Some(Value::Array(a)) => a.iter().find(|m| m.get("processes").and_then(Value::as_array).is_none_or(|p| p.iter().any(|x| x.as_str() == Some(proc_name)))),
            other => other,
        };
        if let Some(m) = metrics.and_then(Value::as_table) {
            let mut s = Table::new();
            if let Some(p) = m.get("port").and_then(Value::as_integer) {
                s.insert("port".into(), Value::Integer(p));
            }
            if let Some(p) = m.get("path").and_then(Value::as_str) {
                s.insert("path".into(), Value::String(p.into()));
            }
            run_of(t).insert("metrics".into(), Value::Table(s));
        }
        if web && let Some(n) = http.and_then(|h| h.get("min_machines_running")).and_then(Value::as_integer) {
            run_of(t).insert("min_instances".into(), Value::Integer(n));
        }
        if let Some(vm) = f.get("vm").and_then(Value::as_array).and_then(|v| v.first()).or_else(|| f.get("vm")) {
            if let Some(mb) = vm.get("memory").and_then(megabytes).or_else(|| vm.get("memory_mb").and_then(Value::as_integer)) {
                run_of(t).insert("memory_mb".into(), Value::Integer(mb));
            }
            if let Some(c) = vm.get("cpus").and_then(Value::as_integer) {
                run_of(t).insert("cpu".into(), Value::Float(c as f64));
            }
        }
    }
    if f.get("build").and_then(|b| b.get("dockerfile")).is_some() {
        out.notes.push(format!("{app}: its Dockerfile is used as roll finds it (build.dockerfile in its root's roll.toml names another)"));
    }
    for (k, what) in [("mounts", "a volume: give the service one in its settings"), ("statics", "static files: a static site is its own service"), ("checks", "health checks: run.health_path")] {
        if f.get(k).is_some() {
            out.notes.push(format!("{app}: {k} isn't carried over ({what})"));
        }
    }
    out.finish()
}

/// Railway's `railway.json` or `railway.toml` (config as code): one service's build and deploy.
pub fn railway_to_toml(text: &str) -> Result<(String, Vec<String>), ProjectError> {
    let v: serde_json::Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            let t: Table = text.parse().map_err(|e| err(format!("railway.json isn't JSON (nor railway.toml TOML): {e}")))?;
            serde_json::to_value(t).map_err(|e| err(e.to_string()))?
        }
    };
    let mut out = Out::default();
    let mut notes = Vec::new();
    let d = &v["deploy"];
    let t = out.app("web")?;
    t.insert("root".into(), Value::String(".".into()));
    if let Some(c) = d["startCommand"].as_str() {
        t.insert("start".into(), Value::String(c.into()));
    }
    if let Some(s) = d["cronSchedule"].as_str() {
        t.insert("type".into(), Value::String("cron".into()));
        t.insert("schedule".into(), Value::String(s.into()));
    }
    let pre = match &d["preDeployCommand"] {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(a) => Some(a.iter().filter_map(|c| c.as_str()).collect::<Vec<_>>().join(" && ")).filter(|s| !s.is_empty()),
        _ => None,
    };
    if let Some(c) = pre {
        run_of(t).insert("pre_deploy".into(), Value::String(c));
    }
    if let Some(p) = d["healthcheckPath"].as_str() {
        run_of(t).insert("health_path".into(), Value::String(p.into()));
    }
    if let Some(n) = d["numReplicas"].as_i64().filter(|n| *n >= 1) {
        run_of(t).insert("min_instances".into(), Value::Integer(n));
    }
    if d["sleepApplication"] == true {
        run_of(t).insert("min_instances".into(), Value::Integer(0));
    }
    if let Some(p) = d["restartPolicyType"].as_str() {
        let r = match p.to_ascii_uppercase().as_str() {
            "NEVER" => "never",
            "ON_FAILURE" => "on_failure",
            _ => "always",
        };
        run_of(t).insert("restart".into(), Value::String(r.into()));
    }
    if let Some(b) = v["build"]["buildCommand"].as_str() {
        notes.push(format!("web: its build command ({b}) isn't carried over: roll detects the build (roll.toml [app] build in its root sets one)"));
    }
    notes.push("web: named web; rename it in the file if the project has more than one".into());
    out.notes.extend(notes);
    out.finish()
}

/// Heroku's `app.json` and `Procfile`: a service per process (`release` its pre-deploy), its
/// variables (`generator: secret`, a required one with no value asked for), and Heroku Postgres
/// and Redis as roll's databases, linked from every service.
pub fn heroku_to_toml(app_json: &str, procfile: Option<&str>) -> Result<(String, Vec<String>), ProjectError> {
    let v: serde_json::Value = serde_json::from_str(app_json).map_err(|e| err(format!("app.json isn't JSON: {e}")))?;
    let mut out = Out::default();
    let mut dbs = Vec::new();
    for a in v["addons"].as_array().into_iter().flatten() {
        let plan = a.as_str().or(a["plan"].as_str()).unwrap_or("");
        if plan.starts_with("heroku-postgresql") {
            dbs.push(out.database("db", "postgres"));
            out.notes.push("heroku-postgresql: a roll Postgres database, db; its link sets DATABASE_URL".into());
        } else if plan.starts_with("heroku-redis") || plan.starts_with("rediscloud") {
            dbs.push(out.database("redis", "redis"));
            out.notes.push(format!("{plan}: a roll Redis database, redis; its link sets REDIS_URL"));
        } else if !plan.is_empty() {
            out.notes.push(format!("{plan}: an add-on roll doesn't have; set its variables yourself"));
        }
    }
    let mut procs: Vec<(String, String)> = procfile
        .unwrap_or("")
        .lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, c)| (k.trim().to_string(), c.trim().to_string()))
        .filter(|(k, c)| !k.is_empty() && !c.is_empty() && !k.starts_with('#'))
        .collect();
    let release = procs.iter().position(|(k, _)| k == "release").map(|i| procs.remove(i).1);
    if procs.is_empty() {
        procs.push(("web".into(), String::new()));
    }
    let base = v["name"].as_str().map(name_of).filter(|n| !n.is_empty());
    let single = procs.len() == 1;
    for (kind, cmd) in &procs {
        let name = match (&base, single) {
            (Some(b), true) => b.clone(),
            (Some(b), false) => format!("{b}-{}", name_of(kind)),
            (None, _) => name_of(kind),
        };
        let t = out.app(&name)?;
        t.insert("root".into(), Value::String(".".into()));
        t.insert("type".into(), Value::String(if kind == "web" { "web" } else { "worker" }.into()));
        if !cmd.is_empty() {
            t.insert("start".into(), Value::String(cmd.clone()));
        }
        if kind == "web" && let Some(r) = &release {
            run_of(t).insert("pre_deploy".into(), Value::String(r.clone()));
        }
        if let Some(q) = v["formation"][kind.as_str()]["quantity"].as_i64().filter(|q| *q >= 1) {
            run_of(t).insert("min_instances".into(), Value::Integer(q));
        }
        for db in &dbs {
            link(t, db);
        }
        for (k, e) in v["env"].as_object().into_iter().flatten() {
            if ["DATABASE_URL", "REDIS_URL"].contains(&k.as_str()) && !dbs.is_empty() {
                continue;
            }
            let val = match e {
                serde_json::Value::Object(o) if o.get("generator").and_then(|g| g.as_str()) == Some("secret") => spec("generate"),
                serde_json::Value::Object(o) => match o.get("value").and_then(text) {
                    Some(val) => Value::String(val),
                    None if o.get("required") != Some(&serde_json::Value::Bool(false)) => spec("ask"),
                    None => continue,
                },
                other => match text(other) {
                    Some(val) => Value::String(val),
                    None => continue,
                },
            };
            vars_of(t).insert(k.clone(), val);
        }
    }
    if v["scripts"]["postdeploy"].is_string() {
        out.notes.push("scripts.postdeploy isn't carried over: a pre-deploy command (run.pre_deploy) runs before each deploy goes live".into());
    }
    if v["buildpacks"].is_array() {
        out.notes.push("buildpacks aren't carried over: roll detects the build".into());
    }
    out.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_fly_railway_and_heroku_files_read_as_roll_toml() {
        let (t, notes) = render_to_toml(
            "services:\n  - type: web\n    name: api\n    runtime: node\n    rootDir: ./server\n    buildCommand: npm ci\n    startCommand: node index.js\n    healthCheckPath: /health\n    envVars:\n      - key: DATABASE_URL\n        fromDatabase: { name: main, property: connectionString }\n      - key: SECRET\n        generateValue: true\n      - key: STRIPE_KEY\n        sync: false\n      - key: MODE\n        value: prod\n  - type: worker\n    name: jobs\n    image: { url: ghcr.io/acme/jobs:1 }\n  - type: keyvalue\n    name: cache\ndatabases:\n  - name: main\n",
        )
        .unwrap();
        let f = crate::project::parse_project(&t).unwrap();
        let api = &f.apps["api"];
        assert_eq!((api.root.as_deref(), api.start.as_deref(), api.links.clone()), (Some("server"), Some("node index.js"), vec!["main".to_string()]));
        assert!(t.contains("generate = \"secret\"") && t.contains("MODE = \"prod\"") && !t.contains("DATABASE_URL"), "{t}");
        assert_eq!(f.apps["jobs"].image.as_deref(), Some("ghcr.io/acme/jobs:1"));
        assert!(t.contains("[databases.cache]") && notes.iter().any(|n| n.contains("build command")), "{t} {notes:?}");
        let (t, _) = fly_to_toml("app = \"Shop\"\n[env]\nLOG = \"info\"\n[http_service]\ninternal_port = 3000\nmin_machines_running = 1\n[processes]\napp = \"node server.js\"\nworker = \"node worker.js\"\n[[vm]]\nmemory = \"1gb\"\n[mounts]\nsource = \"data\"\n[metrics]\nport = 9091\npath = \"/metrics\"\n").unwrap();
        let f = crate::project::parse_project(&t).unwrap();
        assert_eq!((f.apps["shop-app"].port, f.apps["shop-worker"].kind.as_deref()), (Some(3000), Some("worker")), "{t}");
        assert!(t.contains("memory_mb = 1024") && t.contains("port = 9091"), "its [metrics] is scraped: {t}");
        let (t, _) = railway_to_toml(r#"{"deploy": {"startCommand": "npm start", "healthcheckPath": "/up", "numReplicas": 2, "preDeployCommand": ["npm run migrate"]}}"#).unwrap();
        assert!(t.contains("pre_deploy = \"npm run migrate\"") && t.contains("min_instances = 2") && t.contains("health_path = \"/up\""), "{t}");
        let (t, notes) = heroku_to_toml(r#"{"name": "blog", "env": {"SECRET_KEY_BASE": {"generator": "secret"}, "SMTP_HOST": {"description": "your mail server", "required": true}, "RACK_ENV": "production"}, "addons": ["heroku-postgresql:essential-0", "papertrail"]}"#, Some("web: bundle exec puma\nworker: bundle exec sidekiq\nrelease: rails db:migrate\n")).unwrap();
        let f = crate::project::parse_project(&t).unwrap();
        assert_eq!((f.apps["blog-web"].links.clone(), f.apps["blog-worker"].start.as_deref()), (vec!["db".to_string()], Some("bundle exec sidekiq")), "{t}");
        assert!(t.contains("pre_deploy = \"rails db:migrate\"") && t.contains("ask = ") && notes.iter().any(|n| n.contains("papertrail")), "{t} {notes:?}");
    }
}
