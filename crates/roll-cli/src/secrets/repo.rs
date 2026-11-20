//! A repo's suggestion (docs/secrets.md, "The repo's suggestion"): `[secrets]` in the nearest
//! `roll.toml` may name the project, the environment, the folder and environments by branch, so
//! teammates onboard from the repo, as Doppler's `doppler.yaml` and Infisical's `.infisical.yaml`.
//! It's only a suggestion, as direnv's `allow`: it's used once the person has approved that exact
//! table for that directory, and never to choose a protected environment. It never holds an API
//! URL, a token or a value: a repository never chooses where a token goes.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const FILE: &str = "roll.toml";
const MAX_BYTES: u64 = 64 << 10;
const MAX_BRANCHES: usize = 64;
/// roll code's keys in the same table (`code_control::settings`): not ours, never warned about.
const CODE_KEYS: &[&str] = &["unstored_credentials"];
/// Words in a key that ask for something a repo never holds.
const CREDENTIAL_WORDS: &[&str] = &["token", "secret", "password", "passwd", "key", "credential", "auth", "url", "api", "endpoint", "host", "value", "cert"];

/// What the table suggests: only the keys that passed.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Branch glob → environment.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub branches: BTreeMap<String, String>,
}

impl Table {
    pub fn is_empty(&self) -> bool {
        self == &Table::default()
    }

    /// What an approval keeps: SHA-256 of the table as used (ignored keys aren't in it).
    pub fn digest(&self) -> String {
        let canonical = serde_json::to_string(self).expect("a table serializes");
        Sha256::digest(canonical.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
    }

    /// The environment for `branch`: the most specific (longest) matching pattern, else
    /// `environment`; and the pattern that chose it.
    pub fn environment_for(&self, branch: Option<&str>) -> Option<(String, Option<String>)> {
        let by_branch = branch.and_then(|b| self.branches.iter().filter(|(p, _)| code_review::glob::whole(p, b)).max_by_key(|(p, _)| p.len()));
        match by_branch {
            Some((p, e)) => Some((e.clone(), Some(p.clone()))),
            None => self.environment.clone().map(|e| (e, None)),
        }
    }

    /// `project shop, environment development, path /api, branch release/* → staging`
    pub fn describe(&self) -> String {
        let mut parts = vec![];
        if let Some(p) = &self.project {
            parts.push(format!("project {p}"));
        }
        if let Some(e) = &self.environment {
            parts.push(format!("environment {e}"));
        }
        if let Some(p) = &self.path {
            parts.push(format!("path {p}"));
        }
        for (b, e) in &self.branches {
            parts.push(format!("branch {b} → {e}"));
        }
        if parts.is_empty() { "nothing".into() } else { parts.join(", ") }
    }
}

/// The nearest `roll.toml`'s `[secrets]`.
#[derive(Clone, Debug)]
pub struct Found {
    /// The `roll.toml`.
    pub file: PathBuf,
    /// Its directory: what an approval is for.
    pub dir: PathBuf,
    pub table: Table,
    /// Keys left out, and why.
    pub ignored: Vec<String>,
}

/// The `[secrets]` of the nearest `roll.toml` walking up from `start`; `None` when that file has
/// none (or there's no `roll.toml`).
pub fn nearest(start: &Path) -> Option<Found> {
    let dir = start.ancestors().find(|d| std::fs::metadata(d.join(FILE)).is_ok_and(|m| m.is_file()))?;
    let file = dir.join(FILE);
    let (table, ignored) = match std::fs::metadata(&file) {
        Ok(m) if m.len() > MAX_BYTES => (Table::default(), vec![format!("{FILE} is larger than {} KiB", MAX_BYTES >> 10)]),
        _ => match std::fs::read_to_string(&file) {
            Ok(text) => parse(&text)?,
            Err(e) => (Table::default(), vec![format!("reading {FILE}: {e}")]),
        },
    };
    Some(Found { file, dir: dir.to_path_buf(), table, ignored })
}

/// The table and what was left out of it; `None` without a `[secrets]` table.
pub fn parse(text: &str) -> Option<(Table, Vec<String>)> {
    let doc: toml::Table = match text.parse() {
        Ok(d) => d,
        Err(e) => return Some((Table::default(), vec![format!("{FILE} doesn't parse: {}", e.to_string().lines().next().unwrap_or_default())])),
    };
    let s = doc.get("secrets")?;
    let Some(s) = s.as_table() else { return Some((Table::default(), vec!["[secrets] isn't a table".into()])) };
    let (mut t, mut ignored) = (Table::default(), vec![]);
    for (k, v) in s {
        let mut word = |check: fn(&str) -> bool, what: &str| match v.as_str() {
            Some(x) if check(x) => Some(x.to_string()),
            _ => {
                ignored.push(format!("{k}: {what}"));
                None
            }
        };
        match k.as_str() {
            k if CODE_KEYS.contains(&k) => {}
            "project" => t.project = word(project_ok, "a project's name or prj_… id"),
            "environment" => t.environment = word(slug_ok, "an environment's slug"),
            "path" => t.path = word(path_ok, "a folder like /api"),
            "branches" => match v.as_table() {
                Some(b) if b.len() <= MAX_BRANCHES => {
                    for (pattern, env) in b {
                        match env.as_str() {
                            Some(e) if slug_ok(e) && !pattern.trim().is_empty() && pattern.len() <= 200 => {
                                t.branches.insert(pattern.clone(), e.to_string());
                            }
                            _ => ignored.push(format!("branches.{pattern}: a branch glob and an environment's slug")),
                        }
                    }
                }
                _ => ignored.push(format!("branches: a table of at most {MAX_BRANCHES} branch globs to environments")),
            },
            other if credential_looking(other) => ignored.push(format!("{other}: a repo never holds an API URL, a token or a value")),
            other => ignored.push(format!("{other}: not a key roll secrets reads (project, environment, path, branches)")),
        }
    }
    Some((t, ignored))
}

fn credential_looking(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    CREDENTIAL_WORDS.iter().any(|w| k.contains(w))
}

/// Not a credential or an address: a token's `rl…_` prefix, a URL, a control character.
fn plain(s: &str) -> bool {
    let token = s.len() > 4 && s.as_bytes()[..2] == *b"rl" && s.as_bytes()[3] == b'_' && !s.starts_with("prj_");
    !s.is_empty() && !token && !s.contains("://") && !s.chars().any(char::is_control)
}

fn project_ok(s: &str) -> bool {
    plain(s) && s.len() <= 100
}

fn slug_ok(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

fn path_ok(s: &str) -> bool {
    plain(s) && s.starts_with('/') && s.len() <= 256 && !s.split('/').any(|p| p == "..")
}

/// The branch checked out in `dir` (`None` when HEAD is detached, or it isn't a git checkout).
pub fn branch(dir: &Path) -> Option<String> {
    let out = std::process::Command::new("git").arg("-C").arg(dir).args(["symbolic-ref", "--short", "-q", "HEAD"]).stderr(std::process::Stdio::null()).output().ok()?;
    let b = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (out.status.success() && !b.is_empty()).then_some(b)
}

