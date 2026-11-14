//! Required keys (docs/secrets.md, "Required keys"): keys every standard environment of a project
//! must define in a folder. The grid flags one an environment lacks, and reading an environment's
//! values (`roll run`, an app's start, a check) reports it as a problem.
//!
//! A required key may also have a type and a pattern (typed keys): a write whose value doesn't fit
//! is refused, naming the key and never the value, and a resolved value that doesn't fit (through
//! a reference) is a problem, so `roll run` won't start with it.

use crate::access::{Action, Caller, Scope};
use crate::write::{Item, ItemAction};
use crate::{Error, Result, Secrets, Value, audit, names};
use code_sql::{Conn, params};
use serde::{Deserialize, Serialize};

/// The most keys one project can require.
pub const MAX: usize = 500;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Required {
    pub key: String,
    #[serde(default = "root")]
    pub path: String,
    #[serde(default)]
    pub comment: String,
    /// What the value must be.
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub kind: Option<KeyType>,
    /// A regular expression the whole value must match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Who supplies it (a person or team, `@payments`): health and the daily notice name them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

/// A typed key's type.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum KeyType {
    /// `scheme://host…`
    Url,
    Int,
    /// 1–65535.
    Port,
    /// true, false, 1, 0, yes, no (any case).
    Bool,
    Json,
    Email,
}

impl KeyType {
    fn fits(self, v: &str) -> bool {
        match self {
            KeyType::Url => v.split_once("://").is_some_and(|(scheme, rest)| {
                let mut c = scheme.chars();
                c.next().is_some_and(|f| f.is_ascii_alphabetic()) && c.all(|x| x.is_ascii_alphanumeric() || "+.-".contains(x)) && !rest.is_empty() && !rest.starts_with('/') && !v.contains(char::is_whitespace)
            }),
            KeyType::Int => v.parse::<i64>().is_ok(),
            KeyType::Port => v.parse::<u16>().is_ok_and(|p| p > 0),
            KeyType::Bool => ["true", "false", "1", "0", "yes", "no"].contains(&v.to_ascii_lowercase().as_str()),
            KeyType::Json => serde_json::from_str::<serde_json::Value>(v).is_ok(),
            KeyType::Email => v.split_once('@').is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.') && !d.contains('@')) && !v.contains(char::is_whitespace),
        }
    }

    fn name(self) -> &'static str {
        match self {
            KeyType::Url => "a URL",
            KeyType::Int => "an integer",
            KeyType::Port => "a port (1–65535)",
            KeyType::Bool => "a boolean",
            KeyType::Json => "JSON",
            KeyType::Email => "an email address",
        }
    }
}

/// The longest pattern taken, and how big it may compile (regex has no backtracking blow-up).
const PATTERN_MAX: usize = 500;
const PATTERN_SIZE: usize = 1 << 20;

fn compile(pattern: &str) -> Result<regex::Regex> {
    regex::RegexBuilder::new(&format!("^(?:{pattern})$")).size_limit(PATTERN_SIZE).build().map_err(|e| Error::BadRequest(format!("pattern: {e}")))
}

/// A typed key with its pattern compiled once (a batch or a grid checks many values).
pub(crate) struct Rule<'a> {
    pub required: &'a Required,
    pattern: Option<regex::Regex>,
}

impl Rule<'_> {
    /// Why `v` doesn't fit, if it doesn't. Never quotes the value.
    pub(crate) fn misfit(&self, v: &str) -> Option<String> {
        let r = self.required;
        if let Some(t) = r.kind
            && !t.fits(v)
        {
            return Some(format!("{} must be {}", r.key, t.name()));
        }
        match (&self.pattern, &r.pattern) {
            (Some(re), Some(p)) if !re.is_match(v) => Some(format!("{} must match {p}", r.key)),
            _ => None,
        }
    }
}

impl Required {
    pub(crate) fn rule(&self) -> Rule<'_> {
        Rule { required: self, pattern: self.pattern.as_deref().and_then(|p| compile(p).ok()) }
    }

    fn typed(&self) -> bool {
        self.kind.is_some() || self.pattern.is_some()
    }
}

fn root() -> String {
    "/".into()
}

pub(crate) fn of(c: &Conn<'_>, project: &str) -> Result<Vec<Required>> {
    Ok(c.query("SELECT key, path, comment, type, pattern, owner FROM required WHERE project_id = ? ORDER BY path, key", [project], |r| {
        Ok(Required { key: r.get(0)?, path: r.get(1)?, comment: r.get(2)?, kind: r.get::<_, Option<String>>(3)?.and_then(|t| serde_json::from_value(serde_json::Value::String(t)).ok()), pattern: r.get(4)?, owner: r.get(5)? })
    })?)
}

impl Secrets {
    pub fn required(&self, project: &str, caller: &Caller) -> Result<Vec<Required>> {
        caller.need(Action::Read, &Scope::project(project))?;
        self.project(project)?;
        self.db.read(|c| of(c, project))
    }

    /// Replace the project's required keys.
    pub fn set_required(&self, project: &str, list: Vec<Required>, caller: &Caller) -> Result<Vec<Required>> {
        caller.need(Action::Admin, &Scope::project(project))?;
        if list.len() > MAX {
            return Err(Error::BadRequest(format!("at most {MAX} required keys")));
        }
        let mut list = list;
        for r in &mut list {
            names::key(&r.key)?;
            r.path = names::path(&r.path)?;
            names::comment(&r.comment)?;
            r.owner = r.owner.take().map(|o| o.trim().to_string()).filter(|o| !o.is_empty());
            // A plain name: it goes into chat notices, so no markup (`<!channel>`, links).
            if r.owner.as_ref().is_some_and(|o| o.chars().count() > 64 || !o.chars().all(|c| c.is_alphanumeric() || " @._-/".contains(c))) {
                return Err(Error::BadRequest(format!("{}: an owner is a name of at most 64 letters, digits, spaces and @._-/", r.key)));
            }
            if let Some(p) = &r.pattern {
                if p.is_empty() || p.len() > PATTERN_MAX {
                    return Err(Error::BadRequest(format!("{}: a pattern is 1–{PATTERN_MAX} characters", r.key)));
                }
                compile(p)?;
            }
        }
        list.sort_by(|a, b| (&a.path, &a.key).cmp(&(&b.path, &b.key)));
        list.dedup_by(|a, b| a.path == b.path && a.key == b.key);
        self.project(project)?;
        let now = self.db.now_ms()?;
        self.db.write(|c| {
            c.execute("DELETE FROM required WHERE project_id = ?", [project])?;
            for r in &list {
                let kind = r.kind.and_then(|t| serde_json::to_value(t).ok()).and_then(|v| v.as_str().map(str::to_string));
                c.execute("INSERT INTO required (project_id, path, key, comment, created_at, type, pattern, owner) VALUES (?, ?, ?, ?, ?, ?, ?, ?)", params![project, r.path, r.key, r.comment, now, kind, r.pattern, r.owner])?;
            }
            let keys: Vec<String> = list.iter().map(|r| r.key.clone()).collect();
            audit::record(c, project, caller, "required.set", None, None, &keys, &format!("{} required", crate::write::plural(list.len(), "key")), now)?;
            of(c, project)
        })
    }

    /// Refuse a write whose values don't fit their typed keys. A value with a reference is checked
    /// once resolved, at read time.
    pub(crate) fn check_types(&self, project: &str, items: &[Item]) -> Result<()> {
        let all: Vec<Required> = self.db.read(|c| of(c, project))?.into_iter().filter(Required::typed).collect();
        if all.is_empty() {
            return Ok(());
        }
        let rules: Vec<Rule<'_>> = all.iter().map(Required::rule).collect();
        let misfits: Vec<String> = items
            .iter()
            .filter_map(|i| match &i.action {
                ItemAction::Set { value: Some(v), .. } if crate::refs::refs(v.expose()).is_empty() => rules.iter().find(|r| r.required.path == i.path && r.required.key == i.key)?.misfit(v.expose()),
                _ => None,
            })
            .collect();
        if misfits.is_empty() { Ok(()) } else { Err(Error::BadRequest(misfits.join("; "))) }
    }

    /// The resolved values of `path` that don't fit their typed keys, as problems.
    pub(crate) fn type_problems(&self, project: &str, path: &str, values: &std::collections::BTreeMap<String, Value>) -> Result<Vec<crate::Problem>> {
        let all = self.db.read(|c| of(c, project))?;
        Ok(all.iter().filter(|r| r.path == path && r.typed()).filter_map(|r| Some(crate::Problem { key: r.key.clone(), problem: r.rule().misfit(values.get(&r.key)?.expose())? })).collect())
    }

    /// The keys `path` requires that `have` lacks.
    pub(crate) fn missing_required(&self, project: &str, path: &str, have: &[String]) -> Result<Vec<Required>> {
        let all = self.db.read(|c| of(c, project))?;
        Ok(all.into_iter().filter(|r| r.path == path && !have.contains(&r.key)).collect())
    }
}
