//! A project as roll.toml (Fly's `fly config save`, Render's Blueprint export): its services and
//! the databases they link, written with the parser `roll up --all` reads
//! (`apps_build::project`), so the file reads back as the project it came from. Variables go by
//! name only (a value is never written to a file); what a file doesn't carry is said in its
//! header. docs/apps/README.md, "A project in one file".

use crate::model::{PRODUCTION, Run, RunType, Source};
use crate::{Apps, Error, Result};
use apps_build::project::{AppEntry, DbEntry, ProjectFile, to_toml, valid_name};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

/// What roll.toml's entry sets at its top level, not under `run`.
const TOP_LEVEL: [&str; 4] = ["type", "start", "schedule", "port"];

impl Apps {
    /// `POST …/projects/{project}/export {databases?}`: the project's roll.toml. `databases`
    /// names the databases its services link (`{"postgres:<ref>": "db"}`, as the caller lists
    /// them; roll apps doesn't keep other products' names); one not named is named after its
    /// ref.
    pub async fn export_project(self: &Arc<Self>, project: &str, body: Value) -> Result<Value> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| k.as_str() != "databases") {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        let names: HashMap<String, String> = match o.get("databases") {
            None | Some(Value::Null) => HashMap::new(),
            Some(Value::Object(m)) if m.len() <= 200 => m.iter().filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string()))).collect(),
            Some(_) => return Err(Error::BadRequest("databases is an object of \"kind:ref\" to names".into())),
        };
        {
            let (this, p) = (self.clone(), project.to_string());
            tokio::task::spawn_blocking(move || this.project_name(&p)).await.map_err(|e| Error::Internal(e.to_string()))??;
        }
        let services = self.services(Some(project.to_string())).await?;
        let defaults = serde_json::to_value(Run::default()).expect("JSON");
        let mut file = ProjectFile::default();
        let mut notes = Vec::new();
        for svc in &services {
            let mut a = AppEntry { root: None, image: None, repo: None, git: None, branch: None, kind: None, start: svc.run.start.clone(), schedule: svc.run.schedule.clone(), run: None, links: Vec::new(), port: svc.run.port, variables: BTreeMap::new() };
            match &svc.source {
                Source::Repo { repo, root } => {
                    a.repo = Some(repo.clone());
                    a.root = root.clone();
                    let (sid, e) = (svc.id.clone(), PRODUCTION.to_string());
                    a.branch = self.db(move |s| s.environment(&sid, &e)).await.ok().and_then(|e| e.branch);
                }
                Source::Git { url, root } => {
                    a.git = Some(url.clone());
                    a.root = root.clone();
                    let (sid, e) = (svc.id.clone(), PRODUCTION.to_string());
                    a.branch = self.db(move |s| s.environment(&sid, &e)).await.ok().and_then(|e| e.branch);
                }
                Source::Image { image } => a.image = Some(image.clone()),
                Source::Upload => {
                    a.root = Some(".".into());
                    notes.push(format!("{}: uploaded with `roll up`; its root here is this directory (change it to the app's)", svc.name));
                }
            }
            if svc.run.kind != RunType::Web {
                a.kind = serde_json::to_value(svc.run.kind).ok().and_then(|v| v.as_str().map(str::to_string));
            }
            // The run settings that differ from a new service's, as `[apps.<name>.run]`.
            let run = serde_json::to_value(&svc.run).expect("JSON");
            let mut table = toml::Table::new();
            for (k, v) in run.as_object().into_iter().flatten() {
                if TOP_LEVEL.contains(&k.as_str()) || v.is_null() || defaults.get(k) == Some(v) {
                    continue;
                }
                if let Ok(t) = toml::Value::try_from(v) {
                    table.insert(k.clone(), t);
                }
            }
            a.run = (!table.is_empty()).then_some(table);
            // What it links that a file declares (databases, buckets, streams, NoSQL, another
            // service of the project by its name); the rest said.
            let mut other = Vec::new();
            for l in self.links(&svc.id).await? {
                let kind = l.kind.as_str();
                if kind == "service" {
                    match services.iter().find(|s| s.id == l.r#ref) {
                        Some(t) => a.links.push(t.name.clone()),
                        None => other.push(format!("service {}", l.r#ref)),
                    }
                } else if apps_build::project::DB_KINDS.contains(&kind) {
                    let named = names.get(&format!("{kind}:{}", l.r#ref)).cloned().unwrap_or_else(|| l.r#ref.clone());
                    let name = name_of(&named);
                    file.databases.insert(name.clone(), DbEntry { kind: kind.to_string() });
                    a.links.push(name);
                } else {
                    other.push(format!("{kind} {}", l.r#ref));
                }
            }
            if !other.is_empty() {
                notes.push(format!("{}: links a file doesn't declare (made again with `roll apps link`): {}", svc.name, other.join(", ")));
            }
            let (sid, e) = (svc.id.clone(), PRODUCTION.to_string());
            let vars: Vec<String> = self.db(move |s| s.variables(&sid, &e)).await.map(|v| v.into_keys().collect()).unwrap_or_default();
            if !vars.is_empty() {
                notes.push(format!("{}: variables set by name only, never written here (`roll apps vars`): {}", svc.name, vars.join(", ")));
            }
            file.apps.insert(svc.name.clone(), a);
        }
        if file.apps.is_empty() {
            return Err(Error::Conflict("this project has no services to write".into()));
        }
        let text = to_toml(&file, &notes).map_err(|e| Error::Internal(e.to_string()))?;
        // Read back as `roll up --all` reads it: what's written is what's there.
        apps_build::project::parse_project(&text).map_err(|e| Error::Internal(format!("the export doesn't read back: {e}")))?;
        Ok(serde_json::json!({ "roll_toml": text, "notes": notes }))
    }
}

/// A database's name as roll.toml takes it (a-z, 0-9, -).
fn name_of(n: &str) -> String {
    let s: String = n.to_ascii_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect::<String>().trim_matches('-').chars().take(32).collect();
    if valid_name(&s) { s } else { "db".to_string() }
}
