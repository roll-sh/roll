//! Suggested links (docs/secrets.md, "Links"): the resources on a project's canvas that no
//! environment links yet, each with its provider's usual fields under their usual key names, for
//! every standard environment that has neither the link nor those keys. Only roll can offer this:
//! it knows the canvas.

use crate::access::{Action, Caller, Scope};
use crate::links::Mode;
use crate::{Error, Result, Secrets, envs};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Serialize)]
pub struct Suggestion {
    pub kind: String,
    #[serde(rename = "ref")]
    pub r#ref: String,
    /// The resource's name, when its product says.
    pub name: Option<String>,
    pub links: Vec<SuggestedLink>,
    /// Where they'd go: standard environments without the link or any of those keys at `/`.
    pub environments: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct SuggestedLink {
    pub key: String,
    pub field: String,
    pub mode: Mode,
}

/// A resource's name as a key prefix: `analytics-db` → `ANALYTICS_DB_`.
fn prefix(name: &str) -> String {
    let p: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    let p = p.trim_matches('_');
    if p.is_empty() || p.starts_with(|c: char| c.is_ascii_digit()) { format!("R_{p}_") } else { format!("{p}_") }
}

impl Secrets {
    pub fn suggestions(&self, project: &str, caller: &Caller) -> Result<Vec<Suggestion>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        let members = self.members(project)?;
        type Seen = (Vec<String>, BTreeSet<(String, String, String)>, BTreeMap<String, BTreeSet<String>>);
        let (envs, linked, keys): Seen = self.db.read(|c| {
            let envs: Vec<String> = envs::all(c, project)?.into_iter().filter(|e| e.kind == envs::Kind::Standard).map(|e| e.slug).collect();
            let mut linked = BTreeSet::new();
            let mut keys: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
            let rows: Vec<(String, String, Option<String>, String)> = c.query(
                "SELECT e.slug, s.key, s.link, s.path FROM secrets s JOIN environments e ON e.id = s.env_id WHERE s.project_id = ? AND s.owner = ''",
                [project],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            for (slug, key, link, path) in rows {
                if let Some(l) = link.and_then(|l| serde_json::from_str::<crate::Link>(&l).ok()) {
                    linked.insert((slug.clone(), l.kind, l.r#ref));
                }
                if path == "/" {
                    keys.entry(slug).or_default().insert(key);
                }
            }
            Ok::<_, Error>((envs, linked, keys))
        })?;
        let offered = |kind: &str| self.links.get(kind).ok().filter(|p| !p.suggested().is_empty());
        let mut per_kind: BTreeMap<&str, usize> = BTreeMap::new();
        for (kind, _) in &members {
            if offered(kind).is_some() {
                *per_kind.entry(kind.as_str()).or_default() += 1;
            }
        }
        let mut out = vec![];
        for (kind, r) in &members {
            let Some(provider) = offered(kind) else { continue };
            let name = provider.name(r);
            let pre = if per_kind.get(kind.as_str()).copied().unwrap_or(0) > 1 { prefix(name.as_deref().unwrap_or(r)) } else { String::new() };
            let fields = provider.fields_for(r);
            let links: Vec<SuggestedLink> = provider
                .suggested_for(r)
                .iter()
                .filter_map(|(field, mode)| fields.iter().find(|f| f.field == *field).map(|f| SuggestedLink { key: format!("{pre}{}", f.suggest), field: f.field.to_string(), mode: *mode }))
                .collect();
            let environments: Vec<String> = envs
                .iter()
                .filter(|slug| !linked.contains(&((*slug).clone(), kind.clone(), r.clone())))
                .filter(|slug| keys.get(*slug).is_none_or(|have| links.iter().all(|l| !have.contains(&l.key))))
                .filter(|slug| caller.allows(Action::Admin, &Scope::env(project, slug)))
                .cloned()
                .collect();
            if !environments.is_empty() && !links.is_empty() {
                out.push(Suggestion { kind: kind.clone(), r#ref: r.clone(), name, links, environments });
            }
        }
        Ok(out)
    }
}
