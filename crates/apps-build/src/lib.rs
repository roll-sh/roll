//! roll apps builds: read a source tree and decide how to build and run it.
//!
//! [`plan`] tries the providers in order: a Dockerfile, then Node, Python, Go, Rust, Ruby, PHP,
//! Java and static sites. The first one that matches wins. The [`Plan`] it returns holds the
//! commands it detected, the port, notes that explain each decision, and a Dockerfile that builds
//! a slim runtime image with BuildKit. It only reads files: no network, no processes. The design
//! is "Builds" in docs/apps/README.md.

mod cargo;
mod df;
mod docker;
mod golang;
mod java;
mod node;
mod php;
mod python;
mod ruby;
mod static_site;
#[cfg(test)]
mod tests;
mod tree;
mod version;

use serde::{Deserialize, Serialize};

pub use tree::{DirTree, MapTree, Tree};
pub use node::{WorkspaceApp, workspace_app};

/// The port a generated image listens on. The image sets `PORT` to it.
pub const DEFAULT_PORT: u16 = 8080;

/// The `.dockerignore` a build uses when the tree has none.
pub const DOCKERIGNORE: &str = "\
.git
**/node_modules
target
**/__pycache__
.venv
.env*
.roll
";

/// What the service's build settings (or `roll.toml` `[app]`) say. Empty commands count as unset.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(default)]
pub struct Overrides {
    pub builder: Builder,
    /// The Dockerfile's path in the tree when `builder` is `dockerfile` (default `Dockerfile`).
    pub dockerfile: Option<String>,
    pub install: Option<String>,
    pub build: Option<String>,
    pub start: Option<String>,
    /// Names of the variables the build sees (the service's, at deploy time): each install and
    /// build step mounts them as BuildKit secrets, so they're in its environment but never in a
    /// layer, the image's history or the cache's keys. `ROLL_BUILD_ENV` (a hash of the values)
    /// is the stage's build argument, so a changed value rebuilds what read it.
    #[serde(skip)]
    pub build_env: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Builder {
    /// Detect: a `Dockerfile` at the root wins, else the providers decide.
    #[default]
    Auto,
    /// Build the tree's own Dockerfile.
    Dockerfile,
}

/// How a tree builds and runs (`Plan` in docs/apps/http-api.md).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Plan {
    pub provider: Provider,
    pub framework: Option<String>,
    pub language_version: Option<String>,
    pub install: Option<String>,
    pub build: Option<String>,
    /// `None` when nothing says how to start the app: the service's `run.start` has to.
    pub start: Option<String>,
    pub port: u16,
    /// What builds the image: the tree's own, or the generated one.
    pub dockerfile: String,
    pub notes: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Dockerfile,
    Node,
    Python,
    Go,
    Rust,
    Ruby,
    Php,
    Java,
    Static,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "couldn't tell how to build this: found no Dockerfile, package.json, requirements.txt, \
         pyproject.toml, go.mod, Cargo.toml, Gemfile, composer.json, index.php, pom.xml, \
         build.gradle or index.html at the root; add a Dockerfile or set the root directory"
    )]
    NothingDetected,
    #[error("the Dockerfile path {0:?} must be a relative path inside the source, without `..`")]
    BadDockerfilePath(String),
    #[error("there's no {0} in the source (the builder is set to dockerfile)")]
    NoDockerfile(String),
    #[error("{file} isn't valid: {reason}")]
    Invalid { file: String, reason: String },
    /// Several things could be built (binaries, `cmd/` programs): the build setting has to choose.
    #[error("{0}")]
    Ambiguous(String),
    #[error("{0}")]
    NoBinary(String),
}

/// Detects how `tree` builds and runs. `tree` is the service's root directory.
pub fn plan(tree: &dyn Tree, overrides: &Overrides) -> Result<Plan, Error> {
    let o = overrides.trimmed();
    if o.builder == Builder::Dockerfile || tree.read("Dockerfile").is_some() {
        return docker::plan(tree, &o);
    }
    type Detect = fn(&dyn Tree, &Overrides) -> Result<Option<Draft>, Error>;
    const PROVIDERS: [Detect; 8] = [
        node::detect,
        python::detect,
        golang::detect,
        cargo::detect,
        ruby::detect,
        php::detect,
        java::detect,
        static_site::detect,
    ];
    for detect in PROVIDERS {
        if let Some(draft) = detect(tree, &o)? {
            return Ok(draft.finish(&o));
        }
    }
    Err(Error::NothingDetected)
}

impl Overrides {
    fn trimmed(&self) -> Overrides {
        let clean = |s: &Option<String>| {
            s.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
        };
        Overrides {
            builder: self.builder,
            dockerfile: clean(&self.dockerfile),
            install: clean(&self.install),
            build: clean(&self.build),
            start: clean(&self.start),
            build_env: self.build_env.clone(),
        }
    }
}

/// The commands a provider's Dockerfile runs, after the overrides.
pub(crate) struct Commands<'a> {
    pub install: Option<&'a str>,
    pub build: Option<&'a str>,
    pub start: Option<&'a str>,
    /// The install was set in the service, so it may read any file: copy the whole source first.
    pub custom_install: bool,
    /// [`Overrides::build_env`].
    pub env: &'a [String],
}

type Render = Box<dyn FnOnce(&Commands<'_>) -> String>;

/// What a provider detected, before the overrides replace any of its commands.
pub(crate) struct Draft {
    pub provider: Provider,
    pub framework: Option<&'static str>,
    pub language_version: Option<String>,
    pub install: Option<String>,
    pub build: Option<String>,
    pub start: Option<String>,
    pub notes: Vec<String>,
    pub render: Render,
}

impl Draft {
    fn finish(self, o: &Overrides) -> Plan {
        let mut notes = self.notes;
        let mut pick = |what: &str, set: &Option<String>, detected: Option<String>| match set {
            Some(cmd) => {
                notes.push(format!("{what} is set in the service: {cmd}"));
                Some(cmd.clone())
            }
            None => detected,
        };
        let install = pick("install", &o.install, self.install);
        let build = pick("build", &o.build, self.build);
        let start = pick("start", &o.start, self.start);
        let dockerfile = (self.render)(&Commands {
            install: install.as_deref(),
            build: build.as_deref(),
            start: start.as_deref(),
            custom_install: o.install.is_some(),
            env: &o.build_env,
        });
        Plan {
            provider: self.provider,
            framework: self.framework.map(String::from),
            language_version: self.language_version,
            install,
            build,
            start,
            port: DEFAULT_PORT,
            dockerfile,
            notes,
        }
    }
}

/// A file as text (lossy: providers only look for markers in it).
pub(crate) fn text(tree: &dyn Tree, path: &str) -> Option<String> {
    tree.read(path)
        .map(|b| String::from_utf8_lossy(&b).into_owned())
}

pub(crate) fn invalid(file: &str, reason: impl std::fmt::Display) -> Error {
    Error::Invalid {
        file: file.to_string(),
        reason: reason.to_string().trim().to_string(),
    }
}

/// `dir/path`, or `path` at the root.
pub(crate) fn join(dir: &str, path: &str) -> String {
    if dir.is_empty() {
        path.to_string()
    } else {
        format!("{dir}/{path}")
    }
}
