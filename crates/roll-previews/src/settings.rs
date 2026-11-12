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
    /// `"<kind>:<ref>"` → `branch`, `schema` or `skip`; resources not listed get `data`.
    pub resources: BTreeMap<String, ResourceRule>,
    pub data: Mode,
    /// Re-branch every resource from its parent this often (0: never).
    pub reset_every_hours: u32,
    pub sleep_after_s: u32,
    pub ttl_hours: u32,
    pub max_previews: u32,
    pub max_bytes: u64,
    pub secrets: SecretsRule,
    pub env: BTreeMap<String, String>,
    pub github: GithubRule,
    /// Who may open a preview's URLs: `link` (default) needs the preview's link, which carries
    /// its access token once and then a cookie; `public` is anyone who can reach the domain.
    pub access: Access,
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
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: false,
            branches: vec!["*".into()],
            drafts: false,
            resources: BTreeMap::new(),
            data: Mode::Branch,
            reset_every_hours: 0,
            sleep_after_s: 600,
            ttl_hours: 168,
            max_previews: 10,
            max_bytes: 10 << 30,
            secrets: SecretsRule::default(),
            env: BTreeMap::new(),
            github: GithubRule::default(),
            access: Access::Link,
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
        GithubRule { comment: true, status: true, deployment: true }
    }
}

/// Whether an environment name is production's.
pub fn is_production(env: &str) -> bool {
    matches!(env.to_ascii_lowercase().as_str(), "production" | "prod" | "live")
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
        for key in self.resources.keys() {
            let Some((kind, r)) = key.split_once(':') else { return bad(format!("resources: {key:?} isn't \"<kind>:<ref>\"")) };
            Kind::parse(kind)?;
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

    /// How a project resource is branched, or `None` to leave it out.
    pub fn mode_of(&self, kind: Kind, r#ref: &str) -> Option<Mode> {
        match self.resources.get(&format!("{}:{}", kind.as_str(), r#ref)) {
            Some(ResourceRule::Skip) => None,
            Some(ResourceRule::Schema) => Some(Mode::Schema),
            Some(ResourceRule::Branch) => Some(Mode::Branch),
            None => Some(self.data),
        }
    }
}

fn merge(into: &mut Value, patch: &Value) {
    match (into, patch) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                match a.get_mut(k) {
                    // `resources` and `env` are maps: a patch replaces them whole, so a key
                    // can be removed.
                    Some(slot) if v.is_object() && !matches!(k.as_str(), "resources" | "env") => merge(slot, v),
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
        let r = Settings::default().patched(&json!({ "resources": { "redis:rdb_1": "skip", "postgres:prj_1": "schema" } })).unwrap();
        assert_eq!(r.mode_of(Kind::Redis, "rdb_1"), None);
        assert_eq!(r.mode_of(Kind::Postgres, "prj_1"), Some(Mode::Schema));
        assert_eq!(r.mode_of(Kind::Bucket, "b"), Some(Mode::Branch));
    }
}
