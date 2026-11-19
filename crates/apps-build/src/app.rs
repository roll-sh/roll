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
//! Here the file is parsed and its shape checked (unknown keys, blocks inside blocks, a volume);
//! [`AppConfig::check_all`] also names run settings that don't exist in any block, so
//! `roll apps plan DIR` and `roll up` say what's wrong before anything is pushed. A build checks
//! only the blocks it applies (apps-control, with the values, as the API does): a typo in
//! staging's block never stops production's deploys. `docs/apps/roll.schema.json` is the same
//! shape for editors.

use crate::{Builder, Overrides};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// The largest `roll.toml` read; a real one is a few hundred bytes.
pub const MAX_BYTES: u64 = 64 << 10;

/// What `[app.run]` takes: a service's run settings (apps-control's `Run`, whose tests keep the
/// two equal), but `volume`.
pub const RUN_KEYS: [&str; 38] = [
    "type", "port", "start", "pre_deploy", "schedule", "timezone", "timeout_s", "retries", "min_instances", "max_instances", "concurrency", "cpu", "memory_mb", "health_path",
    "health_timeout_s", "suspend_after_s", "sleep_after_s", "alerts", "rollout", "h2c", "scale", "public", "drain_s", "cpu_target", "memory_target", "restart", "restart_tries", "affinity", "tcp",
    "rollback_on_crash", "schedules", "health_every_s", "health_failures", "health_command", "rebuild_on_base", "sidecars", "block_on", "slo",
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
    /// System packages the generated Dockerfile installs (`["ffmpeg"]`).
    pub packages: Option<Vec<String>>,
    /// Run settings ([`RUN_KEYS`], as the API's `run`).
    pub run: Option<toml::Table>,
    /// Per environment, the same keys again (no `environments` in them), applied last.
    pub environments: Option<std::collections::BTreeMap<String, AppConfig>>,
    /// Preview environments (ephemeral ones), the same keys again, before a named block.
    pub previews: Option<Box<AppConfig>>,
    /// Only in `[app.previews]`: what roll previews' screenshots do (`pages`, `hide`,
    /// `[[states]]`), as its `[preview]` takes them. Opaque here (roll previews checks it with
    /// code-preview's rules); builds and deploys never read it. [`screenshots`].
    pub screenshots: Option<toml::Table>,
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

/// Why a `roll.toml` wasn't read: the file's fault, or the machine's (it couldn't be read).
#[derive(Debug)]
pub enum ReadError {
    Invalid(String),
    Io(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::Invalid(m) | ReadError::Io(m) => f.write_str(m),
        }
    }
}

/// `[app]` of `<root>/roll.toml`, if there's one. The file must be a regular file (a pushed tree
/// can hold symlinks), and a malformed `[app]` is an error, not a silent default.
pub fn read(root: &Path) -> Result<Option<AppConfig>, ReadError> {
    let path = root.join("roll.toml");
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(ReadError::Io(format!("roll.toml: {e}"))),
    };
    if !meta.is_file() {
        return Err(ReadError::Invalid("roll.toml isn't a regular file".into()));
    }
    if meta.len() > MAX_BYTES {
        return Err(ReadError::Invalid(format!("roll.toml is larger than {} KiB", MAX_BYTES >> 10)));
    }
    let text = std::fs::read(&path).map_err(|e| ReadError::Io(format!("roll.toml: {e}")))?;
    let text = String::from_utf8(text).map_err(|_| ReadError::Invalid("roll.toml isn't UTF-8".into()))?;
    parse(&text).map_err(ReadError::Invalid)
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
    for (name, e) in &all {
        if let Some(p) = &e.packages {
            crate::packages::check(p).map_err(|why| format!("roll.toml [{name}]: {why}"))?;
        }
        if e.screenshots.is_some() && name != "app.previews" {
            return Err(format!("roll.toml: screenshots go in [app.previews.screenshots], not [{name}]"));
        }
    }
    for (_, e) in all {
        if e.run.as_ref().is_some_and(|r| r.contains_key("volume")) {
            return Err("roll.toml: a volume is the service's setting, not the tree's (`roll apps set --volume`)".into());
        }
    }
    Ok(f.app)
}

/// `[app.previews.screenshots]` of `<root>/roll.toml` (roll previews, at the preview's own
/// revision), read with [`read`]'s rules.
pub fn screenshots(root: &Path) -> Result<Option<toml::Table>, ReadError> {
    Ok(read(root)?.and_then(|a| a.previews).and_then(|p| p.screenshots))
}

impl AppConfig {
    /// Every block's run settings exist (before a push: a deploy checks only what it applies).
    pub fn check_all(&self) -> Result<(), String> {
        let blocks = std::iter::once(("app".to_string(), self)).chain(self.environments.iter().flatten().map(|(n, e)| (format!("app.environments.{n}"), e))).chain(self.previews.iter().map(|p| ("app.previews".to_string(), &**p)));
        for (name, e) in blocks {
            if let Some(k) = e.run.iter().flat_map(|r| r.keys()).find(|k| !RUN_KEYS.contains(&k.as_str())) {
                return Err(format!("roll.toml: [{name}.run] has no setting {k}"));
            }
        }
        Ok(())
    }

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
        if let Some(p) = &self.packages {
            set("packages", &p.join(", "));
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
            if let Some(p) = &c.packages {
                o.packages.clone_from(p);
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
        let typo = parse("[app.environments.staging.run]\nmin_instaces = 1\n").unwrap().unwrap();
        assert!(typo.check_all().unwrap_err().contains("min_instaces"), "nor in run, before a push");
        assert!(parse("[app.run]\nvolume = { mount = \"/data\", size_gb = 9 }\n").is_err(), "a push can't move a volume");
        assert!(parse("[app.environments.production.environments.x]\nstart = \"a\"\n").is_err());
        assert_eq!(parse("[checks.test]\nrun = \"npm test\"\n[apps.web]\nroot = \"web\"\n").unwrap(), None, "roll code's and roll up --all's sections are theirs");
        let app = parse("[app]\nbuild = \"pnpm build\"\n[app.run]\nconcurrency = 20\n[app.environments.production]\nstart = \"node a.js\"\n").unwrap().unwrap();
        let (o, said) = app.overrides(&Overrides::default(), Some(EnvScope { name: "production", ephemeral: false }));
        assert_eq!((o.build.as_deref(), o.start.as_deref()), (Some("pnpm build"), Some("node a.js")));
        assert_eq!(said, ["roll.toml sets build: pnpm build", "roll.toml sets run.concurrency: 20", "roll.toml sets start: node a.js (production)"]);
        let (staging, _) = app.overrides(&Overrides::default(), Some(EnvScope { name: "staging", ephemeral: false }));
        assert_eq!(staging.start, None, "another environment doesn't get production's block");
        // roll previews' screenshots: in the previews block only, opaque to the build.
        let shots = "[app.previews.screenshots]\npages = [\"/pricing\"]\n[[app.previews.screenshots.states]]\nname = \"cart\"\npage = \"/\"\nsteps = [{ click = \"#open-cart\" }]\n";
        let app = parse(shots).unwrap().unwrap();
        let s = app.previews.as_ref().unwrap().screenshots.as_ref().unwrap();
        assert_eq!((s["pages"].as_array().map(Vec::len), s["states"].as_array().map(Vec::len)), (Some(1), Some(1)));
        assert!(app.overrides(&Overrides::default(), Some(EnvScope { name: "pv-1", ephemeral: true })).1.is_empty(), "nothing a build says");
        assert!(parse("[app.screenshots]\npages = [\"/\"]\n").is_err());
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
            packages: Some(vec![]),
            run: Some(toml::Table::new()),
            environments: Some(Default::default()),
            previews: Some(Box::default()),
            screenshots: Some(toml::Table::new()),
        };
        let defs = &schema["definitions"];
        let mut fields = keys(&serde_json::to_value(&all).unwrap());
        assert!(fields.remove("screenshots") && defs["previews"]["properties"]["screenshots"].is_object(), "screenshots are the previews block's alone");
        assert_eq!(fields, keys(&defs["app"]["properties"]));
        let mut settings = keys(&defs["app"]["properties"]);
        settings.remove("environments");
        settings.remove("previews");
        assert_eq!(settings, keys(&defs["settings"]["properties"]));
        assert_eq!(keys(&defs["run"]["properties"]), RUN_KEYS.iter().map(|k| k.to_string()).collect());
    }
}
