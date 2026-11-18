//! Required keys (docs/secrets.md, "Required keys"): keys every standard environment of a project
//! must define in a folder. The grid flags one an environment lacks, and reading an environment's
//! values (`roll run`, an app's start, a check) reports it as a problem.
//!
//! A required key may also have a type and a pattern (typed keys): a write whose value doesn't fit
//! is refused, naming the key and never the value, and a resolved value that doesn't fit (through
//! a reference) is a problem, so `roll run` won't start with it. A `totp` key's value is a TOTP
//! seed, which hands out one-time codes (`totp.rs`).

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
    /// What a value must be made of, and what generators make (as Vault's password policies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<Policy>,
}

/// A password policy: a length every value has at least (and generators make), and the
/// character classes a value must use one of each of, and nothing outside them.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub length: u16,
    #[serde(default = "letters_and_digits")]
    pub classes: Vec<Class>,
    /// Which characters `symbol` means (default [`SYMBOLS`]): a target's own rules, say no `@` or
    /// `:` in a connection URL's password, no quotes for a shell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbols: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Lower,
    Upper,
    Digit,
    Symbol,
}

/// The symbols a policy's `symbol` class means unless it says: safe in a URL's password once
/// escaped, in YAML and in a shell's double quotes.
pub const SYMBOLS: &str = "!#%&*+-=?^_~";

fn letters_and_digits() -> Vec<Class> {
    vec![Class::Lower, Class::Upper, Class::Digit]
}

impl Class {
    fn name(self) -> &'static str {
        match self {
            Class::Lower => "a lowercase letter",
            Class::Upper => "an uppercase letter",
            Class::Digit => "a digit",
            Class::Symbol => "a symbol",
        }
    }
}

impl Policy {
    fn symbols(&self) -> &str {
        self.symbols.as_deref().unwrap_or(SYMBOLS)
    }

    fn class_of(&self, c: char) -> Option<Class> {
        if c.is_ascii_lowercase() {
            Some(Class::Lower)
        } else if c.is_ascii_uppercase() {
            Some(Class::Upper)
        } else if c.is_ascii_digit() {
            Some(Class::Digit)
        } else if self.symbols().contains(c) {
            Some(Class::Symbol)
        } else {
            None
        }
    }

    fn check(&mut self, key: &str) -> Result<()> {
        self.classes.sort();
        self.classes.dedup();
        if !(8..=256).contains(&self.length) || self.classes.is_empty() {
            return Err(Error::BadRequest(format!("{key}: a password policy is 8–256 characters of at least one class (lower, upper, digit, symbol)")));
        }
        if let Some(s) = &self.symbols
            && (s.is_empty() || s.len() > 32 || !s.chars().all(|c| c.is_ascii_punctuation()))
        {
            return Err(Error::BadRequest(format!("{key}: a policy's symbols are 1–32 ASCII punctuation characters")));
        }
        Ok(())
    }

    /// Why `v` breaks the policy, if it does. Never quotes the value.
    fn misfit(&self, key: &str, v: &str) -> Option<String> {
        if v.chars().count() < self.length as usize {
            return Some(format!("{key} must be at least {} characters", self.length));
        }
        let mut seen = vec![];
        for c in v.chars() {
            match self.class_of(c) {
                Some(class) if self.classes.contains(&class) => seen.push(class),
                _ => return Some(format!("{key} may only use {}", self.classes.iter().map(|c| c.name()).collect::<Vec<_>>().join(", "))),
            }
        }
        self.classes.iter().find(|c| !seen.contains(c)).map(|c| format!("{key} needs at least {}", c.name()))
    }
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
    /// A TOTP seed: an `otpauth://totp/…` URI or base32 (docs/secrets.md, "TOTP keys").
    Totp,
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
            KeyType::Totp => crate::totp::parse(v).is_ok(),
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
            KeyType::Totp => "a TOTP seed (an otpauth://totp/ URI or base32)",
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
            // A TOTP seed has several rules: say which one (never the value).
            let why = if t == KeyType::Totp { crate::totp::parse(v).err().map(|w| format!(": {w}")).unwrap_or_default() } else { String::new() };
            return Some(format!("{} must be {}{why}", r.key, t.name()));
        }
        match (&self.pattern, &r.pattern) {
            (Some(re), Some(p)) if !re.is_match(v) => return Some(format!("{} must match {p}", r.key)),
            _ => {}
        }
        r.password.as_ref().and_then(|p| p.misfit(&r.key, v))
    }
}

impl Required {
    pub(crate) fn rule(&self) -> Rule<'_> {
        Rule { required: self, pattern: self.pattern.as_deref().and_then(|p| compile(p).ok()) }
    }

    fn typed(&self) -> bool {
        self.kind.is_some() || self.pattern.is_some() || self.password.is_some()
    }
}

fn root() -> String {
    "/".into()
}

pub(crate) fn of(c: &Conn<'_>, project: &str) -> Result<Vec<Required>> {
    Ok(c.query("SELECT key, path, comment, type, pattern, owner, password FROM required WHERE project_id = ? ORDER BY path, key", [project], |r| {
        Ok(Required {
            key: r.get(0)?,
            path: r.get(1)?,
            comment: r.get(2)?,
            kind: r.get::<_, Option<String>>(3)?.and_then(|t| serde_json::from_value(serde_json::Value::String(t)).ok()),
            pattern: r.get(4)?,
            owner: r.get(5)?,
            password: r.get::<_, Option<String>>(6)?.and_then(|p| serde_json::from_str(&p).ok()),
        })
    })?)
}

impl Secrets {
    pub fn required(&self, project: &str, caller: &Caller) -> Result<Vec<Required>> {
        caller.need(Action::Read, &Scope::whole(project))?;
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
            if let Some(p) = &mut r.password {
                p.check(&r.key)?;
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
                let password = r.password.as_ref().map(|p| serde_json::to_string(p).unwrap_or_default());
                c.execute("INSERT INTO required (project_id, path, key, comment, created_at, type, pattern, owner, password) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)", params![project, r.path, r.key, r.comment, now, kind, r.pattern, r.owner, password])?;
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
