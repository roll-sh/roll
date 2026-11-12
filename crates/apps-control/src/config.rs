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
//! ```

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
    Ok(f.app)
}

impl AppConfig {
    /// The service's settings with this file's on top, checked as the API checks them, and a
    /// line for each value the file set.
    pub fn apply(&self, build: &Build, run: &Run) -> Result<(Build, Run, Vec<String>)> {
        let (mut b, mut r, mut said) = (build.clone(), run.clone(), Vec::new());
        let mut set = |what: &str, value: &str| said.push(format!("roll.toml sets {what}: {value}"));
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
        let (b, r, said) = app.apply(&build, &Run::default()).unwrap();
        assert_eq!((b.build.as_deref(), r.pre_deploy.as_deref()), (Some("pnpm build"), Some("pnpm migrate")));
        assert_eq!(said, ["roll.toml sets build: pnpm build", "roll.toml sets pre_deploy: pnpm migrate"]);
        assert!(parse("[app]\nbuidl = \"x\"\n").is_err(), "a typo isn't ignored");
        assert!(parse("[app]\nhealth_path = \"healthz\"\n").unwrap().unwrap().apply(&Build::default(), &Run::default()).is_err(), "checked like the API");
        assert_eq!(parse("[preview]\nimage = \"x\"\n").unwrap(), None);
    }
}
