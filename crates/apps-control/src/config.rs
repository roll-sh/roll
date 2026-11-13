//! `roll.toml` `[app]`: a service's build and deploy settings kept in its tree (config as code,
//! like `railway.json`). Read from the service's root directory at each build; what it sets wins
//! over the service's settings, and every value it sets is named in the build log. The file's
//! other sections are roll code's (checks, previews, the queue).
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
//! ```
//!
//! What a push can't change is what it shouldn't: the volume, domains, access and variables
//! stay the service's settings.

use crate::model::{Build, Builder, Run};
use crate::{Error, Result};
use serde::Deserialize;
use std::path::Path;

/// The largest `roll.toml` read; a real one is a few hundred bytes.
const MAX_BYTES: u64 = 64 << 10;

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub builder: Option<Builder>,
    pub dockerfile: Option<String>,
    pub install: Option<String>,
    pub build: Option<String>,
    pub start: Option<String>,
    pub pre_deploy: Option<String>,
    pub health_path: Option<String>,
    /// Path patterns a push must touch to deploy ([`Build::watch`]).
    pub watch: Option<Vec<String>>,
    /// Run settings ([`Run`]'s fields, as the API's `run`), but `volume`.
    pub run: Option<toml::Table>,
    /// Per environment, the same keys again (no `environments` in them), applied last.
    pub environments: Option<std::collections::BTreeMap<String, AppConfig>>,
}

#[derive(Deserialize)]
struct File {
    app: Option<AppConfig>,
    // Every other section belongs to roll code.
    #[serde(flatten)]
    _rest: toml::Table,
}

/// `[app]` of `<root>/roll.toml`, if there's one. The file must be a regular file (a pushed tree
/// can hold symlinks), and a malformed `[app]` is an error, not a silent default.
pub fn read(root: &Path) -> Result<Option<AppConfig>> {
    let path = root.join("roll.toml");
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(Error::Internal(e.to_string())),
    };
    if !meta.is_file() {
        return Err(Error::BadRequest("roll.toml isn't a regular file".into()));
    }
    if meta.len() > MAX_BYTES {
        return Err(Error::BadRequest(format!("roll.toml is larger than {} KiB", MAX_BYTES >> 10)));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| Error::BadRequest(format!("roll.toml: {e}")))?;
    parse(&text)
}

pub fn parse(text: &str) -> Result<Option<AppConfig>> {
    let f: File = toml::from_str(text).map_err(|e| Error::BadRequest(format!("roll.toml: {}", e.message())))?;
    if let Some(app) = &f.app {
        for (name, e) in app.environments.iter().flatten() {
            if e.environments.is_some() {
                return Err(Error::BadRequest(format!("roll.toml: [app.environments.{name}] can't hold environments")));
            }
        }
        for run in std::iter::once(&app.run).chain(app.environments.iter().flatten().map(|(_, e)| &e.run)).flatten() {
            if run.contains_key("volume") {
                return Err(Error::BadRequest("roll.toml: a volume is the service's setting, not the tree's (`roll apps set --volume`)".into()));
            }
        }
    }
    Ok(f.app)
}

impl AppConfig {
    /// The service's settings with this file's on top (then the environment's block, when
    /// `env` is given and has one), checked as the API checks them, and a line for each value
    /// the file set.
    pub fn apply(&self, build: &Build, run: &Run, env: Option<&str>) -> Result<(Build, Run, Vec<String>)> {
        let (b, r, mut said) = self.apply_one(build, run, "")?;
        match env.and_then(|e| self.environments.as_ref()?.get(e).map(|c| (e, c))) {
            Some((e, c)) => {
                let (b, r, more) = c.apply_one(&b, &r, &format!(" ({e})"))?;
                said.extend(more);
                Ok((b, r, said))
            }
            None => Ok((b, r, said)),
        }
    }

    /// The paths a push must touch to deploy to `env`: its block's `watch`, else the file's.
    pub fn watch_for(&self, env: &str) -> Option<Vec<String>> {
        self.environments.as_ref().and_then(|m| m.get(env)).and_then(|c| c.watch.clone()).or_else(|| self.watch.clone())
    }

    fn apply_one(&self, build: &Build, run: &Run, scope: &str) -> Result<(Build, Run, Vec<String>)> {
        let (mut b, mut r, mut said) = (build.clone(), run.clone(), Vec::new());
        let mut set = |what: &str, value: &str| said.push(format!("roll.toml sets {what}: {value}{scope}"));
        if let Some(v) = self.builder {
            set("builder", if v == Builder::Dockerfile { "dockerfile" } else { "auto" });
            b.builder = v;
        }
        for (what, from, to) in [
            ("dockerfile", &self.dockerfile, &mut b.dockerfile),
            ("install", &self.install, &mut b.install),
            ("build", &self.build, &mut b.build),
            ("start", &self.start, &mut b.start),
        ] {
            if let Some(v) = from {
                set(what, v);
                *to = Some(v.clone());
            }
        }
        for (what, from, to) in [("pre_deploy", &self.pre_deploy, &mut r.pre_deploy), ("health_path", &self.health_path, &mut r.health_path)] {
            if let Some(v) = from {
                set(what, v);
                *to = Some(v.clone());
            }
        }
        if let Some(w) = &self.watch {
            set("watch", &w.join(", "));
            b.watch = Some(w.clone());
        }
        if let Some(t) = &self.run {
            let patch = serde_json::to_value(t).map_err(|e| Error::BadRequest(format!("roll.toml: {e}")))?;
            for (k, v) in t {
                set(&format!("run.{k}"), &v.to_string());
            }
            r = r.merged(Some(&patch)).map_err(|e| Error::BadRequest(format!("roll.toml [app.run]: {e}")))?;
        }
        b.check()?;
        let r = r.normalized()?;
        Ok((b, r, said))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_is_read_beside_roll_codes_sections_and_wins_over_the_settings() {
        let text = "[checks.test]\nimage = \"node:22\"\nrun = \"npm test\"\n\n[app]\nbuild = \"pnpm build\"\npre_deploy = \"pnpm migrate\"\n";
        let app = parse(text).unwrap().unwrap();
        let build = Build { build: Some("npm run build".into()), ..Build::default() };
        let (b, r, said) = app.apply(&build, &Run::default(), None).unwrap();
        assert_eq!((b.build.as_deref(), r.pre_deploy.as_deref()), (Some("pnpm build"), Some("pnpm migrate")));
        assert_eq!(said, ["roll.toml sets build: pnpm build", "roll.toml sets pre_deploy: pnpm migrate"]);
        assert!(parse("[app]\nbuidl = \"x\"\n").is_err(), "a typo isn't ignored");
        assert!(parse("[app]\nhealth_path = \"healthz\"\n").unwrap().unwrap().apply(&Build::default(), &Run::default(), None).is_err(), "checked like the API");
        assert_eq!(parse("[preview]\nimage = \"x\"\n").unwrap(), None);
    }

    #[test]
    fn run_settings_and_an_environments_block_go_on_top() {
        let text = "[app]\nstart = \"node a.js\"\n[app.run]\nconcurrency = 20\n[app.environments.production]\nwatch = [\"web/**\"]\n[app.environments.production.run]\nmin_instances = 1\n";
        let app = parse(text).unwrap().unwrap();
        let (b, r, said) = app.apply(&Build::default(), &Run::default(), Some("production")).unwrap();
        assert_eq!((b.start.as_deref(), r.concurrency, r.min_instances), (Some("node a.js"), 20, 1));
        assert!(said.contains(&"roll.toml sets run.min_instances: 1 (production)".to_string()), "{said:?}");
        let (_, staging, _) = app.apply(&Build::default(), &Run::default(), Some("staging")).unwrap();
        assert_eq!(staging.min_instances, Run::default().min_instances, "another environment doesn't get production's block");
        assert_eq!(app.watch_for("production").as_deref(), Some(&["web/**".to_string()][..]));
        assert!(parse("[app.run]\nvolume = { mount = \"/data\", size_gb = 9 }\n").is_err(), "a push can't move a volume");
        assert!(parse("[app.run]\nmin_instances = \"lots\"\n").unwrap().unwrap().apply(&Build::default(), &Run::default(), None).is_err());
    }
}
