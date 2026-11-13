//! A project's preview rules (docs/previews.md, `Settings`). Stored as JSON per project; a
//! project without a row has the defaults, previews off.

use crate::{Error, Kind, Mode, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

pub const MAX_ENV: usize = 200;
pub const MAX_BRANCH_GLOBS: usize = 50;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    /// Globs on a change's branch (`*` for every branch). A change with no branch name (made in
    /// roll) counts as `*`.
    pub branches: Vec<String>,
    /// Previews for draft changes too.
    pub drafts: bool,
    /// Only changes whose title asks (`[preview]`) get one (Render's manual mode).
    pub manual: bool,
    /// Gitignore-style patterns (CODEOWNERS'): a change gets a preview only when it touches a path
    /// one matches (empty: any path) …
    pub paths: Vec<String>,
    /// … that none of these match (empty: none ignored). A change's paths are everything since
    /// its merge base with the default branch.
    pub ignore_paths: Vec<String>,
    /// `"<kind>:<ref>"` → `branch`, `schema` or `skip`; resources not listed get `data`.
    pub resources: BTreeMap<String, ResourceRule>,
    pub data: Mode,
    /// Re-branch every resource from its parent this often (0: never).
    pub reset_every_hours: u32,
    pub sleep_after_s: u32,
    pub ttl_hours: u32,
    pub max_previews: u32,
    pub max_bytes: u64,
    /// What the project's previews may cost in a month, in USD (0: no cap). Once they've cost
    /// that this month, new previews wait (`queued`) and the running ones carry on.
    pub max_usd_month: f64,
    pub secrets: SecretsRule,
    pub env: BTreeMap<String, String>,
    pub github: GithubRule,
    /// Who may open a preview's URLs: `link` (default) needs the preview's link, which carries
    /// its access token once and then a cookie; `public` is anyone who can reach the domain.
    pub access: Access,
    /// A command run once on the preview's fresh data (after branching, and after each reset)
    /// before its services deploy: anonymise, migrate, load fixtures.
    pub seed: Option<Seed>,
    /// A command run at each new head of the change (after the seed, before services deploy,
    /// and again after a reset): migrations, Heroku's release phase. A failure fails the preview,
    /// and its services stay on what they ran.
    pub release: Option<Seed>,
    /// Column masks for NoSQL branches: `"nosql:<ref>"` → `{"keyspace.table.column": "mask_inner(1, 1)"}`
    /// (Cassandra 5's mask functions). Merged onto what the branch inherits from its parent,
    /// whenever it's made or reset: the preview reads masked values, the parent is untouched.
    pub masking: BTreeMap<String, BTreeMap<String, String>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seed {
    /// The container image it runs in (`postgres:18-alpine`, `node:22`).
    pub image: String,
    /// Run as `sh -c <run>` in the change's tree, with the branches' variables.
    pub run: String,
    #[serde(default = "seed_timeout")]
    pub timeout_s: u32,
    #[serde(default = "seed_memory")]
    pub memory_mb: u32,
}

fn seed_timeout() -> u32 {
    600
}

fn seed_memory() -> u32 {
    1024
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    #[default]
    Link,
    Public,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResourceRule {
    Branch,
    Schema,
    Skip,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SecretsRule {
    pub base: String,
    pub allow_production: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubRule {
    pub comment: bool,
    pub status: bool,
    pub deployment: bool,
    /// A screenshot of the first service at each new head (it wakes the service once per push).
    #[serde(default)]
    pub screenshot: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: false,
            branches: vec!["*".into()],
            drafts: false,
            manual: false,
            paths: Vec::new(),
            ignore_paths: Vec::new(),
            resources: BTreeMap::new(),
            data: Mode::Branch,
            reset_every_hours: 0,
            sleep_after_s: 600,
            ttl_hours: 168,
            max_previews: 10,
            max_bytes: 10 << 30,
            max_usd_month: 0.0,
            secrets: SecretsRule::default(),
            env: BTreeMap::new(),
            github: GithubRule::default(),
            access: Access::Link,
            seed: None,
            release: None,
            masking: BTreeMap::new(),
        }
    }
}

impl Default for SecretsRule {
    fn default() -> Self {
        SecretsRule { base: "development".into(), allow_production: false }
    }
}

impl Default for GithubRule {
    fn default() -> Self {
        GithubRule { comment: true, status: true, deployment: true, screenshot: false }
    }
}

/// Whether an environment name is production's.
/// Whether an environment name looks like production's. Errs on the side of yes: `prod`,
/// `production-eu`, `prd`, `live`, `main`, `master`.
pub fn is_production(env: &str) -> bool {
    let e = env.to_ascii_lowercase();
    e.contains("prod") || e.split(['-', '_']).any(|w| matches!(w, "prd" | "live" | "main" | "master"))
}

impl Settings {
    /// These settings with `patch`'s fields on top (objects merge, everything else replaces),
    /// checked.
    pub fn patched(&self, patch: &Value) -> Result<Settings> {
        if !patch.is_object() {
            return Err(Error::BadRequest("settings are a JSON object".into()));
        }
        let mut v = serde_json::to_value(self).expect("settings serialize");
        merge(&mut v, patch);
        let s: Settings = serde_json::from_value(v).map_err(|e| Error::BadRequest(format!("settings: {e}")))?;
        s.check()?;
        Ok(s)
    }

    pub fn check(&self) -> Result<()> {
        let bad = |m: String| Err(Error::BadRequest(m));
        if self.branches.is_empty() || self.branches.len() > MAX_BRANCH_GLOBS || self.branches.iter().any(|b| b.is_empty() || b.len() > 200) {
            return bad(format!("branches: 1–{MAX_BRANCH_GLOBS} globs of 1–200 characters"));
        }
        for (name, list) in [("paths", &self.paths), ("ignore_paths", &self.ignore_paths)] {
            if list.len() > MAX_BRANCH_GLOBS || list.iter().any(|p| p.len() > 200 || code_control::review::pattern::Pattern::new(p).is_none()) {
                return bad(format!("{name}: at most {MAX_BRANCH_GLOBS} patterns of 1–200 characters"));
            }
        }
        for key in self.resources.keys() {
            let Some((kind, r)) = key.split_once(':') else { return bad(format!("resources: {key:?} isn't \"<kind>:<ref>\"")) };
            let kind = Kind::parse(kind)?;
            if self.resources[key] == ResourceRule::Schema && !kind.has_schema_mode() {
                return bad(format!("resources: {key}: a {} branch can't be structure-only yet; branch it or skip it", kind.as_str()));
            }
            if r.is_empty() {
                return bad(format!("resources: {key:?} has no ref"));
            }
        }
        if !(60..=7 * 86_400).contains(&self.sleep_after_s) {
            return bad("sleep_after_s: 60 to 604800".into());
        }
        if !(1..=24 * 90).contains(&self.ttl_hours) {
            return bad("ttl_hours: 1 to 2160".into());
        }
        if self.reset_every_hours > 24 * 90 {
            return bad("reset_every_hours: 0 (never) to 2160".into());
        }
        if !(1..=500).contains(&self.max_previews) {
            return bad("max_previews: 1 to 500".into());
        }
        if !self.max_usd_month.is_finite() || self.max_usd_month < 0.0 {
            return bad("max_usd_month: 0 (no cap) or more".into());
        }
        if self.max_bytes < (1 << 20) {
            return bad("max_bytes: at least 1 MiB".into());
        }
        let base = &self.secrets.base;
        if base.is_empty() || base.len() > 64 || !base.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return bad("secrets.base: an environment name (1–64 of a-z, 0-9, - and _)".into());
        }
        if is_production(base) && !self.secrets.allow_production {
            return bad(format!("secrets.base {base:?} is production's: set secrets.allow_production to inherit it"));
        }
        for (name, job) in [("seed", &self.seed), ("release", &self.release)] {
            let Some(job) = job else { continue };
            if job.image.trim().is_empty() || job.image.len() > 255 || job.image.contains(char::is_whitespace) {
                return bad(format!("{name}.image: an image reference"));
            }
            if job.run.trim().is_empty() || job.run.len() > 8192 {
                return bad(format!("{name}.run: a command, at most 8 KiB"));
            }
            if !(10..=3600).contains(&job.timeout_s) {
                return bad(format!("{name}.timeout_s: 10 to 3600"));
            }
            if !(64..=16384).contains(&job.memory_mb) {
                return bad(format!("{name}.memory_mb: 64 to 16384"));
            }
        }
        for (key, cols) in &self.masking {
            let postgres = key.strip_prefix("postgres:").is_some_and(|r| !r.is_empty());
            if !postgres && !key.strip_prefix("nosql:").is_some_and(|r| !r.is_empty()) {
                return bad(format!("masking: {key:?} isn't \"postgres:<ref>\" or \"nosql:<ref>\" (only those branches can mask columns)"));
            }
            if cols.len() > 200 {
                return bad(format!("masking: {key}: at most 200 columns"));
            }
            for (col, f) in cols {
                if col.split('.').count() != 3 || col.split('.').any(str::is_empty) || col.len() > 200 || f.trim().is_empty() || f.len() > 200 {
                    return bad(format!("masking: {key}: {col:?} → {f:?} isn't \"schema.table.column\" → a rule"));
                }
                if postgres && !crate::mask::is_rule(f) {
                    return bad(format!("masking: {key}: {col}: {f:?} isn't a rule ({})", crate::mask::RULES));
                }
            }
        }
        if self.env.len() > MAX_ENV {
            return bad(format!("env: at most {MAX_ENV} variables"));
        }
        for (k, v) in &self.env {
            let name_ok = !k.is_empty()
                && k.len() <= 128
                && k.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
                && k.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
            if !name_ok || k.starts_with("ROLL_") {
                return bad(format!("env: {k:?} isn't a variable name (ROLL_* is roll's)"));
            }
            if v.len() > 32 << 10 {
                return bad(format!("env: {k} is over 32 KiB"));
            }
        }
        Ok(())
    }

    /// How a project resource is branched, or `None` to leave it out. With `data: "schema"`,
    /// a kind that can't be structure-only is left out unless a rule says to branch it: a
    /// project that asked for no rows never gets a full copy by default.
    pub fn mode_of(&self, kind: Kind, r#ref: &str) -> Option<Mode> {
        match self.resources.get(&format!("{}:{}", kind.as_str(), r#ref)) {
            Some(ResourceRule::Skip) => None,
            Some(ResourceRule::Schema) => kind.has_schema_mode().then_some(Mode::Schema),
            Some(ResourceRule::Branch) => Some(Mode::Branch),
            None if self.data == Mode::Schema && !kind.has_schema_mode() => None,
            None => Some(self.data),
        }
    }
}

fn merge(into: &mut Value, patch: &Value) {
    match (into, patch) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                match a.get_mut(k) {
                    // `resources`, `env` and `masking` are maps: a patch replaces them whole, so
                    // a key can be removed.
                    Some(slot) if v.is_object() && !matches!(k.as_str(), "resources" | "env" | "masking") => merge(slot, v),
                    _ => {
                        a.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, v) => *slot = v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn patches_merge_and_production_needs_a_yes() {
        let s = Settings::default().patched(&json!({ "enabled": true, "github": { "comment": false } })).unwrap();
        assert!(s.enabled && !s.github.comment && s.github.status);
        assert!(Settings::default().patched(&json!({ "secrets": { "base": "production" } })).is_err());
        let p = Settings::default().patched(&json!({ "secrets": { "base": "production", "allow_production": true } })).unwrap();
        assert_eq!(p.secrets.base, "production");
        assert!(Settings::default().patched(&json!({ "nope": 1 })).is_err());
        assert!(Settings::default().patched(&json!({ "env": { "ROLL_X": "1" } })).is_err());
        let r = Settings::default().patched(&json!({ "resources": { "redis:rdb_1": "skip", "nosql:n1": "schema" } })).unwrap();
        assert_eq!(r.mode_of(Kind::Redis, "rdb_1"), None);
        assert_eq!(r.mode_of(Kind::Nosql, "n1"), Some(Mode::Schema));
        assert_eq!(r.mode_of(Kind::Bucket, "b"), Some(Mode::Branch));
        // Structure-only where a kind can't do it: refused, never a silent full copy.
        assert!(Settings::default().patched(&json!({ "resources": { "bucket:b": "schema" } })).is_err());
        let schema = Settings::default().patched(&json!({ "data": "schema" })).unwrap();
        assert_eq!(schema.mode_of(Kind::Redis, "rdb_1"), None);
        assert_eq!(schema.mode_of(Kind::Postgres, "prj_1"), Some(Mode::Schema));
        assert_eq!(schema.mode_of(Kind::Stream, "st_1"), Some(Mode::Schema));
        assert!(Settings::default().patched(&json!({ "masking": { "nosql:n1": { "app.users.email": "mask_default()" } } })).is_ok());
        assert!(Settings::default().patched(&json!({ "masking": { "postgres:p1": { "a.b.c": "mask_null()" } } })).is_err(), "Postgres takes rules, not functions");
        assert!(Settings::default().patched(&json!({ "masking": { "postgres:p1": { "public.users.email": "email" } } })).is_ok());
        assert!(Settings::default().patched(&json!({ "masking": { "redis:r1": { "a.b.c": "null" } } })).is_err(), "Redis can't mask");
    }
}
