//! `roll.toml` `[app]`: a service's build and deploy settings kept in its tree (config as code,
//! like `railway.json`). Read from the service's root directory at each build; what it sets wins
//! over the service's settings, and every value it sets is named in the build log. The file's
//! other sections are roll code's (checks, previews, the queue) and `roll up --all`'s
//! (`[apps.<name>]`, [`crate::project`]).
//!
//! ```toml
//! [app]
//! builder = "dockerfile"          # or "auto"
//! dockerfile = "docker/Dockerfile"
//! install = "pnpm install --frozen-lockfile"
//! build = "pnpm build"
//! start = "node dist/server.js"
//! pre_deploy = "pnpm prisma migrate deploy"
//! health_path = "/healthz"
//! watch = ["apps/web/**", "packages/ui/**"]
//!
//! [app.run]                       # any run setting but the volume
//! min_instances = 0
//! concurrency = 50
//!
//! [app.environments.production]   # on top of the above, in that environment only
//! start = "node dist/server.js --cluster"
//!
//! [app.environments.production.run]
//! min_instances = 1
//! memory_mb = 1024
//!
//! [app.previews.run]              # preview environments (ephemeral, `pv-<id>`)
//! memory_mb = 256
//! ```
//!
//! Here the file is parsed and its shape checked (unknown keys, blocks inside blocks, a volume,
//! run settings that don't exist), so `roll apps plan DIR` and `roll up` say what's wrong before
//! anything is pushed; apps-control applies it to a service's settings and checks the values as
//! the API does. `docs/apps/roll.schema.json` is the same shape for editors.

use crate::{Builder, Overrides};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The largest `roll.toml` read; a real one is a few hundred bytes.
pub const MAX_BYTES: u64 = 64 << 10;

/// What `[app.run]` takes: a service's run settings (apps-control's `Run`, whose tests keep the
/// two equal), but `volume`.
pub const RUN_KEYS: [&str; 29] = [
    "type", "port", "start", "pre_deploy", "schedule", "timezone", "timeout_s", "retries", "min_instances", "max_instances", "concurrency", "cpu", "memory_mb", "health_path",
    "health_timeout_s", "suspend_after_s", "sleep_after_s", "alerts", "rollout", "h2c", "scale", "public", "drain_s", "cpu_target", "restart", "restart_tries", "affinity", "tcp",
    "rollback_on_crash",
];

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub builder: Option<Builder>,
    pub dockerfile: Option<String>,
    pub install: Option<String>,
    pub build: Option<String>,
    pub start: Option<String>,
    pub pre_deploy: Option<String>,
    pub health_path: Option<String>,
    /// Path patterns a push must touch to deploy.
    pub watch: Option<Vec<String>>,
    /// A single-page app.
    pub spa: Option<bool>,
    /// Run settings ([`RUN_KEYS`], as the API's `run`).
    pub run: Option<toml::Table>,
    /// Per environment, the same keys again (no `environments` in them), applied last.
    pub environments: Option<std::collections::BTreeMap<String, AppConfig>>,
    /// Preview environments (ephemeral ones), the same keys again, before a named block.
    pub previews: Option<Box<AppConfig>>,
}

/// The environment a build or a deployment is for.
#[derive(Clone, Copy, Debug)]
pub struct EnvScope<'a> {
    pub name: &'a str,
    /// A preview's (`[app.previews]`, and scaled to zero by default).
    pub ephemeral: bool,
}

#[derive(Deserialize)]
struct File {
    app: Option<AppConfig>,
    // Every other section is roll code's or `roll up --all`'s.
    #[serde(flatten)]
    _rest: toml::Table,
}

/// `[app]` of `<root>/roll.toml`, if there's one. The file must be a regular file (a pushed tree
/// can hold symlinks), and a malformed `[app]` is an error, not a silent default.
pub fn read(root: &Path) -> Result<Option<AppConfig>, String> {
    let path = root.join("roll.toml");
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("roll.toml: {e}")),
    };
    if !meta.is_file() {
        return Err("roll.toml isn't a regular file".into());
    }
    if meta.len() > MAX_BYTES {
        return Err(format!("roll.toml is larger than {} KiB", MAX_BYTES >> 10));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| format!("roll.toml: {e}"))?;
    parse(&text)
}

pub fn parse(text: &str) -> Result<Option<AppConfig>, String> {
    let f: File = toml::from_str(text).map_err(|e| format!("roll.toml: {}", e.message()))?;
    let Some(app) = &f.app else { return Ok(None) };
    let blocks = app.environments.iter().flatten().map(|(n, e)| (format!("app.environments.{n}"), e)).chain(app.previews.iter().map(|p| ("app.previews".to_string(), &**p)));
    let mut all = vec![("app".to_string(), app)];
    for (name, e) in blocks {
        if e.environments.is_some() || e.previews.is_some() {
            return Err(format!("roll.toml: [{name}] holds settings, not more blocks"));
        }
        all.push((name, e));
    }
    for (name, e) in all {
        for k in e.run.iter().flat_map(|r| r.keys()) {
            if k == "volume" {
                return Err("roll.toml: a volume is the service's setting, not the tree's (`roll apps set --volume`)".into());
            }
            if !RUN_KEYS.contains(&k.as_str()) {
                return Err(format!("roll.toml: [{name}.run] has no setting {k}"));
            }
        }
    }
    Ok(f.app)
}

impl AppConfig {
    /// The blocks that apply in `env`, in the order they're applied, each with what its lines
    /// end with: the file's, a preview's `[app.previews]`, then the environment's own.
    pub fn layers(&self, env: Option<EnvScope<'_>>) -> Vec<(&AppConfig, String)> {
        let mut v = vec![(self, String::new())];
        if let Some(p) = self.previews.as_deref().filter(|_| env.is_some_and(|e| e.ephemeral)) {
            v.push((p, " (previews)".into()));
        }
        if let Some((name, c)) = env.and_then(|e| self.environments.as_ref()?.get(e.name).map(|c| (e.name, c))) {
            v.push((c, format!(" ({name})")));
        }
        v
    }

    /// A line for each value this block sets (`roll.toml sets build: pnpm build`), `scope` after it.
    pub fn lines(&self, scope: &str) -> Vec<String> {
        let mut said = Vec::new();
        let mut set = |what: &str, value: &str| said.push(format!("roll.toml sets {what}: {value}{scope}"));
        if let Some(b) = self.builder {
            set("builder", if b == Builder::Dockerfile { "dockerfile" } else { "auto" });
        }
        for (what, v) in [("dockerfile", &self.dockerfile), ("install", &self.install), ("build", &self.build), ("start", &self.start), ("pre_deploy", &self.pre_deploy), ("health_path", &self.health_path)] {
            if let Some(v) = v {
                set(what, v);
            }
        }
        if let Some(w) = &self.watch {
            set("watch", &w.join(", "));
        }
        if let Some(v) = self.spa {
            set("spa", &v.to_string());
        }
        for (k, v) in self.run.iter().flatten() {
            set(&format!("run.{k}"), &v.to_string());
        }
        said
    }

    /// The paths a push must touch to deploy to `env`: its block's `watch`, else the file's.
    pub fn watch_for(&self, env: &str) -> Option<Vec<String>> {
        self.environments.as_ref().and_then(|m| m.get(env)).and_then(|c| c.watch.clone()).or_else(|| self.watch.clone())
    }

    /// What a build of this tree in `env` detects with: `base` with the file's build settings on
    /// top, and the lines saying what it set (run settings too: a deploy applies them).
    pub fn overrides(&self, base: &Overrides, env: Option<EnvScope<'_>>) -> (Overrides, Vec<String>) {
        let (mut o, mut said) = (base.clone(), Vec::new());
        for (c, scope) in self.layers(env) {
            if let Some(b) = c.builder {
                o.builder = b;
            }
            for (from, to) in [(&c.dockerfile, &mut o.dockerfile), (&c.install, &mut o.install), (&c.build, &mut o.build), (&c.start, &mut o.start)] {
                if from.is_some() {
                    to.clone_from(from);
                }
            }
            if c.spa.is_some() {
                o.spa = c.spa;
            }
            said.extend(c.lines(&scope));
        }
        (o, said)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn its_shape_is_checked_and_its_build_settings_planned_with() {
        assert!(parse("[app]\nbuidl = \"x\"\n").is_err(), "a typo isn't ignored");
        assert!(parse("[app.run]\nmin_instaces = 1\n").unwrap_err().contains("min_instaces"), "nor in run");
        assert!(parse("[app.run]\nvolume = { mount = \"/data\", size_gb = 9 }\n").is_err(), "a push can't move a volume");
        assert!(parse("[app.environments.production.environments.x]\nstart = \"a\"\n").is_err());
        assert_eq!(parse("[checks.test]\nrun = \"npm test\"\n[apps.web]\nroot = \"web\"\n").unwrap(), None, "roll code's and roll up --all's sections are theirs");
        let app = parse("[app]\nbuild = \"pnpm build\"\n[app.run]\nconcurrency = 20\n[app.environments.production]\nstart = \"node a.js\"\n").unwrap().unwrap();
        let (o, said) = app.overrides(&Overrides::default(), Some(EnvScope { name: "production", ephemeral: false }));
        assert_eq!((o.build.as_deref(), o.start.as_deref()), (Some("pnpm build"), Some("node a.js")));
        assert_eq!(said, ["roll.toml sets build: pnpm build", "roll.toml sets run.concurrency: 20", "roll.toml sets start: node a.js (production)"]);
        let (staging, _) = app.overrides(&Overrides::default(), Some(EnvScope { name: "staging", ephemeral: false }));
        assert_eq!(staging.start, None, "another environment doesn't get production's block");
    }

    #[test]
    fn the_editors_schema_has_the_files_keys() {
        let schema: serde_json::Value = serde_json::from_str(include_str!("../../../docs/apps/roll.schema.json")).unwrap();
        let keys = |v: &serde_json::Value| v.as_object().unwrap().keys().cloned().collect::<std::collections::BTreeSet<_>>();
        // Every field set, so every key serializes: a new field doesn't build until it's named here.
        let s = Some(String::new());
        let all = AppConfig {
            builder: Some(Builder::Auto),
            dockerfile: s.clone(),
            install: s.clone(),
            build: s.clone(),
            start: s.clone(),
            pre_deploy: s.clone(),
            health_path: s.clone(),
            watch: Some(vec![]),
            spa: Some(true),
            run: Some(toml::Table::new()),
            environments: Some(Default::default()),
            previews: Some(Box::default()),
        };
        let defs = &schema["definitions"];
        assert_eq!(keys(&serde_json::to_value(&all).unwrap()), keys(&defs["app"]["properties"]));
        let mut settings = keys(&defs["app"]["properties"]);
        settings.remove("environments");
        settings.remove("previews");
        assert_eq!(settings, keys(&defs["settings"]["properties"]));
        assert_eq!(keys(&defs["run"]["properties"]), RUN_KEYS.iter().map(|k| k.to_string()).collect());
    }
}
