//! roll apps builds: read a source tree and decide how to build and run it.
//!
//! [`plan`] tries the providers in order: a Dockerfile, then Node, Python, Go, Rust, Ruby, PHP,
//! Java and static sites. The first one that matches wins. The [`Plan`] it returns holds the
//! commands it detected, the port, notes that explain each decision, and a Dockerfile that builds
//! a slim runtime image with BuildKit. It only reads files: no network, no processes. The design
//! is "Builds" in docs/apps/README.md.

pub mod app;
mod caches;
mod cargo;
mod clojure;
mod deno;
mod df;
mod docker;
mod dotnet;
pub mod edge;
mod elixir;
pub mod functions;
mod gleam;
mod golang;
mod java;
mod node;
pub mod otel;
pub mod next_cache;
pub mod source_maps;
pub mod packages;
mod php;
pub mod imports;
pub mod project;
mod python;
pub mod site_configs;
mod ruby;
mod scala;
mod shell;
mod static_site;
#[cfg(test)]
mod tests;
mod tree;
mod uses;
mod version;

use serde::{Deserialize, Serialize};

pub use caches::{LAYERS_ARG, isolate_caches};
pub use tree::{DirTree, MapTree, SubTree, Tree};
pub use node::{WorkspaceApp, workspace_app};
pub use uses::{Datastore, uses};
/// A plan's start when it serves files from `/site` (busybox httpd): a static site.
pub const STATIC_START: &str = static_site::START;

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
    /// A single-page app, whatever the detection says (`None`: detected).
    pub spa: Option<bool>,
    /// System packages a generated Dockerfile installs ([`packages`]).
    pub packages: Vec<String>,
    /// The language's OpenTelemetry agent in a generated Dockerfile ([`otel`]).
    pub otel: bool,
    /// Names of the variables the build sees (the service's, at deploy time): each install and
    /// build step mounts them as BuildKit secrets, so they're in its environment but never in a
    /// layer, the image's history or the cache's keys. `ROLL_BUILD_ENV` (a hash of the values)
    /// is the stage's build argument, so a changed value rebuilds what read it.
    #[serde(skip)]
    pub build_env: Vec<String>,
    /// Of `build_env`, those kept as files (`files` on the variables): a name and its absolute
    /// path. Each step mounts them there (`--mount=type=secret,target=…`: an `.npmrc`, a
    /// registry's login) instead of in its environment, and still never in a layer.
    #[serde(skip)]
    pub build_files: Vec<(String, String)>,
    /// The Procfile's process this service runs (`web`, `worker`): its command is the start
    /// command when nothing above names one. None reads no Procfile.
    #[serde(skip)]
    pub process: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Builder {
    /// Detect: a `Dockerfile` at the root wins, else the providers decide.
    #[default]
    Auto,
    /// Build the tree's own Dockerfile.
    Dockerfile,
    /// Run a static site's functions (`netlify/functions/`, `api/`) as a small Node service
    /// ([`functions`]).
    Functions,
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
    /// A single-page app: a static site with a client-side router, so a page path the server
    /// has no file for answers `index.html` (roll's proxy does it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub spa: bool,
    /// Datastores its manifests have client libraries for ([`uses`]): New service offers to
    /// make and link them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uses: Vec<Datastore>,
    /// The Procfile's `release:` (Heroku's release phase): the pre-deploy command when the
    /// service sets none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<String>,
    /// vercel.json's `crons`: paths the service calls on a schedule ([`site_configs::vercel_crons`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub crons: Vec<Cron>,
    /// A static site's function directories the `functions` builder runs (`netlify/functions`,
    /// `api`): a service of their own beside it does ([`functions`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub functions: Vec<String>,
    /// A static site's edge functions ([`edge`]): run before its files on their paths, by its
    /// functions service.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edge: Vec<edge::EdgeFn>,
    /// The OpenTelemetry agent the build added (`node`, `python`; [`otel`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub otel: Option<String>,
    /// roll's Next.js adapter is in the image ([`next_cache`]): its instances share a cache.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub next_cache: bool,
    /// Its Node build writes source maps ([`source_maps`]): it starts with `--enable-source-maps`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub source_maps: bool,
}

/// The images a Dockerfile builds on (its `FROM`s), for watching them move: not its own stages,
/// `scratch`, ones already pinned by digest or named with a build argument; at most 10.
pub fn base_images(dockerfile: &str) -> Vec<String> {
    let (mut stages, mut out) = (Vec::<String>::new(), Vec::<String>::new());
    let joined = dockerfile.replace("\\\r\n", " ").replace("\\\n", " ");
    for line in joined.lines() {
        let mut w = line.split_whitespace();
        if !w.next().is_some_and(|k| k.eq_ignore_ascii_case("FROM")) {
            continue;
        }
        let Some(image) = w.find(|t| !t.starts_with("--")) else { continue };
        let lower = image.to_ascii_lowercase();
        // A stage named before it is one of its own, not an image.
        let own = stages.contains(&lower);
        if let (Some(as_), Some(name)) = (w.next(), w.next())
            && as_.eq_ignore_ascii_case("AS")
        {
            stages.push(name.to_ascii_lowercase());
        }
        if own || lower == "scratch" || image.contains('@') || image.contains('$') || out.iter().any(|o| o == image) || out.len() >= 10 {
            continue;
        }
        out.push(image.to_string());
    }
    out
}

/// A path called on a schedule (vercel.json's `crons`).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Cron {
    pub path: String,
    pub schedule: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Dockerfile,
    Deno,
    Node,
    Python,
    Go,
    Rust,
    Ruby,
    Php,
    Java,
    Dotnet,
    Elixir,
    /// sbt ([`scala`]).
    Scala,
    Clojure,
    Gleam,
    /// A root `start.sh` ([`shell`]).
    Shell,
    Static,
    /// A static site's functions ([`functions`]).
    Functions,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(
        "couldn't tell how to build this: found no Dockerfile, package.json, deno.json, \
         requirements.txt, pyproject.toml, go.mod, Cargo.toml, Gemfile, composer.json, index.php, \
         pom.xml, build.gradle, a .csproj, mix.exs, build.sbt, project.clj, deps.edn, gleam.toml, \
         index.html or start.sh at the root{0}; add a Dockerfile or set the \
         root directory"
    )]
    NothingDetected(String),
    #[error("the Dockerfile path {0:?} must be a relative path inside the source, without `..`")]
    BadDockerfilePath(String),
    #[error("there's no {0} in the source (the builder is set to dockerfile)")]
    NoDockerfile(String),
    #[error("{file} isn't valid: {reason}")]
    Invalid { file: String, reason: String },
    /// Several things could be built (binaries, `cmd/` programs): the build setting has to choose.
    #[error("{0}")]
    Ambiguous(String),
    /// The packages asked for can't be installed (a name, or an image without a package manager).
    #[error("{0}")]
    Packages(String),
    #[error("{0}")]
    NoBinary(String),
}

/// Detects how `tree` builds and runs. `tree` is the service's root directory.
/// A folder of a tree that builds on its own (Railway's monorepo import, Vercel's root picker).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Candidate {
    pub root: String,
    pub provider: Provider,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framework: Option<String>,
}

/// The folders under the tree's root, up to three levels down, that plan as an app that runs or
/// serves something (a start command, a framework, its own Dockerfile: a library package or a
/// plain folder of HTML is left out), dependencies and build output skipped; at most 20,
/// shallowest first.
pub fn candidates(tree: &dyn Tree) -> Vec<Candidate> {
    const SKIP: [&str; 16] = ["node_modules", ".git", "vendor", "target", "dist", "build", "out", ".next", "coverage", "test", "tests", "__tests__", "fixtures", "docs", ".github", "tmp"];
    let mut out = Vec::new();
    let mut level: Vec<String> = vec![String::new()];
    for _ in 0..3 {
        let mut next = Vec::new();
        for dir in &level {
            for name in tree.list(dir).into_iter().filter_map(|n| n.strip_suffix('/').map(str::to_string)) {
                if SKIP.contains(&name.as_str()) || name.starts_with('.') {
                    continue;
                }
                let path = if dir.is_empty() { name } else { format!("{dir}/{name}") };
                let sub = SubTree { inner: tree, prefix: path.clone() };
                if let Ok(p) = detect(&sub, &Overrides::default())
                    && (p.provider == Provider::Dockerfile || p.framework.is_some() || (p.provider != Provider::Static && p.start.is_some()))
                {
                    out.push(Candidate { root: path.clone(), provider: p.provider, framework: p.framework });
                    if out.len() == 20 {
                        return out;
                    }
                    // An app's own folders aren't apps of their own.
                    continue;
                }
                next.push(path);
            }
        }
        level = next;
    }
    out
}

/// Next.js's deployment id (`deploymentId`), set at a Next build: its pages then name it on
/// their requests (`?dpl=` on assets, `x-deployment-id` on its own fetches), which roll's skew
/// protection follows, and Next reloads a page whose build is no longer the one answering.
pub const NEXT_DEPLOYMENT_ID: &str = "NEXT_DEPLOYMENT_ID";

pub fn plan(tree: &dyn Tree, overrides: &Overrides) -> Result<Plan, Error> {
    // A Procfile (Heroku's; Nixpacks and Railpack read it too) names the start command when
    // the service doesn't, unless the tree's own Dockerfile has its CMD.
    // A static site stays one (its files served, nothing running) whatever a Procfile says.
    let procfile = procfile(tree);
    let own_dockerfile = overrides.builder == Builder::Dockerfile || tree.read("Dockerfile").is_some();
    let mut p = detect(tree, overrides)?;
    let from_procfile = match (&overrides.process, overrides.start.as_deref().map(str::trim).filter(|s| !s.is_empty())) {
        (Some(process), None) if !own_dockerfile && p.provider != Provider::Static => procfile.get(process.as_str()).map(|cmd| (process.clone(), cmd.clone())),
        _ => None,
    };
    if let Some((process, cmd)) = &from_procfile {
        p = detect(tree, &Overrides { start: Some(cmd.clone()), ..overrides.clone() })?;
        let said = format!("start is set in the service: {cmd}");
        p.notes.retain(|n| *n != said);
        p.notes.push(format!("start: the Procfile's {process}: {cmd}"));
    }
    // Heroku's release phase runs once per app: for its web service, not its workers or crons.
    if !own_dockerfile
        && p.provider != Provider::Static
        && overrides.process.as_deref() == Some("web")
        && let Some(cmd) = procfile.get("release")
    {
        p.release = Some(cmd.clone());
        p.notes.push(format!("pre-deploy: the Procfile's release: {cmd}"));
    }
    // A runtime past (or near) its end of life: said, so a deploy isn't quietly unpatched.
    if let Some(v) = p.language_version.clone() {
        let today = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64 / 86_400).unwrap_or(0);
        if let Some(n) = version::end_of_life_note(p.provider, &v, today) {
            p.notes.push(n);
        }
    }
    // A static site's functions (Netlify's, Vercel's `api/`) never run: said, not dropped silently.
    if p.start.as_deref() == Some(static_site::START) {
        for (dir, what, route) in [("netlify/functions", "Netlify Functions", "/.netlify/functions/*"), ("api", "Vercel functions", "/api/*")] {
            let n = tree.list(dir).iter().filter(|f| [".js", ".mjs", ".cjs", ".ts", ".mts", ".py", ".go"].iter().any(|e| f.ends_with(e)) || f.ends_with('/')).count();
            if n > 0 {
                p.notes.push(format!("{dir}/ has {n} {what}, which a static site doesn't run: Run its functions makes a service of them, with {route} routed to it"));
                if functions::DIRS.iter().any(|(d, _)| *d == dir) {
                    p.functions.push(dir.to_string());
                }
            }
        }
    }
    // Its edge functions: run before its files once its functions service is (Run its functions).
    if p.start.as_deref() == Some(static_site::START) {
        let (found, notes) = edge::find(tree);
        p.notes.extend(notes);
        for f in &found {
            p.notes.push(format!("{}: an edge function on {}, run before the files by its functions service (Run its functions)", f.file, f.paths.join(", ")));
        }
        if !found.is_empty() {
            p.functions.push("edge".into());
        }
        p.edge = found;
    } else if !own_dockerfile && p.start.is_some() && overrides.builder != Builder::Functions {
        // Middleware for any web service (Vercel's Routing Middleware, Netlify's edge functions on
        // a server's routes): the same files, run before the service by its functions service.
        // Next.js runs its own middleware.ts; a server's own module of that name isn't one.
        let (mut found, notes) = edge::find(tree);
        let next = p.framework.as_deref() == Some("next");
        found.retain(|f| {
            let keep = f.kind != edge::EdgeKind::Vercel || (!next && edge::vercel_middleware(tree, &f.file));
            if !keep && !next {
                p.notes.push(format!("{}: not Vercel's middleware (no config.matcher, no next/server or @vercel import): left to the app", f.file));
            }
            keep
        });
        p.notes.extend(notes);
        for f in &found {
            p.notes.push(format!("{}: an edge function on {}, run before the service by its functions service (Run its functions)", f.file, f.paths.join(", ")));
        }
        if !found.is_empty() {
            p.functions.push("edge".into());
        }
        p.edge = found;
    }
    // Vercel's crons, whatever builds it (a Next.js app's usually).
    if let Some(text) = tree.read("vercel.json").and_then(|b| String::from_utf8(b).ok()) {
        for c in site_configs::vercel_crons(&text) {
            p.notes.push(format!("vercel.json's cron: {} calls {}", c.schedule, c.path));
            p.crons.push(c);
        }
    }
    for (store, why) in uses(tree) {
        p.uses.push(store);
        p.notes.push(format!("{why}: it uses {}", store.label()));
    }
    if overrides.otel {
        if own_dockerfile {
            p.notes.push("otel: no agent added to the tree's own Dockerfile (start one of its own; roll's OTEL_* variables point it at roll)".into());
        } else {
            match otel::add(&p.dockerfile, p.provider) {
                Ok((df, kind)) => {
                    p.dockerfile = df;
                    p.otel = Some(kind.to_string());
                    p.notes.push(format!("otel: the {kind} OpenTelemetry agent, started while traces are on"));
                }
                Err(why) => p.notes.push(why),
            }
        }
    }
    // A Node server built with source maps: its stacks name the source's lines.
    if p.provider == Provider::Node && p.start.as_deref() != Some(static_site::START) {
        let pkg: serde_json::Value = tree.read("package.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        if let Some(why) = source_maps::emitted(tree, &pkg) {
            p.source_maps = true;
            p.notes.push(format!("{why}: it starts with --enable-source-maps, so its stacks name the source's lines"));
        }
    }
    // A Next server's caches shared by its instances (not a static export, not its own Dockerfile).
    if p.provider == Provider::Node && p.framework.as_deref() == Some("next") && !own_dockerfile {
        let pkg: serde_json::Value = tree.read("package.json").and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        match next_cache::adapters(&pkg) {
            Ok(()) => {
                p.dockerfile = next_cache::add(&p.dockerfile);
                p.next_cache = true;
                p.notes.push("next: roll's adapter shares its ISR, fetch and \"use cache\" entries between instances and across sleep".into());
            }
            Err(why) => p.notes.push(why),
        }
    }
    if !overrides.packages.is_empty() {
        if own_dockerfile {
            p.notes.push("packages: not added to the tree's own Dockerfile (it installs what it needs)".into());
        } else if p.provider == Provider::Static {
            return Err(Error::Packages("packages: a static site runs no container to install them in".into()));
        } else {
            p.dockerfile = packages::add(&p.dockerfile, &overrides.packages).map_err(Error::Packages)?;
            p.notes.push(format!("packages: {}", overrides.packages.join(", ")));
        }
    }
    Ok(p)
}

fn detect(tree: &dyn Tree, overrides: &Overrides) -> Result<Plan, Error> {
    let o = overrides.trimmed();
    if o.builder == Builder::Functions {
        return functions::plan(tree, &o);
    }
    if o.builder == Builder::Dockerfile || tree.read("Dockerfile").is_some() {
        return docker::plan(tree, &o);
    }
    type Detect = fn(&dyn Tree, &Overrides) -> Result<Option<Draft>, Error>;
    const PROVIDERS: [Detect; 15] = [
        // Before Node: a Deno project's deno.json decides, a package.json beside it notwithstanding.
        deno::detect,
        node::detect,
        python::detect,
        golang::detect,
        cargo::detect,
        ruby::detect,
        php::detect,
        java::detect,
        dotnet::detect,
        elixir::detect,
        scala::detect,
        clojure::detect,
        gleam::detect,
        // A static site first: one with a dev-only start.sh beside its index.html stays served
        // as files (no instance), as it was before start.sh was built.
        static_site::detect,
        // Last: a start.sh runs when nothing else says how to build the tree.
        shell::detect,
    ];
    for detect in PROVIDERS {
        if let Some(draft) = detect(tree, &o)? {
            let mut p = draft.finish(&o);
            // Served as files (busybox httpd) and routed in the browser: a single-page app.
            match o.spa {
                Some(v) => {
                    p.spa = v;
                    p.notes.push(format!("spa is set in the service: {}", if v { "a page path with no file answers index.html" } else { "unknown paths are 404s" }));
                }
                None => {
                    if p.start.as_deref() == Some(static_site::START)
                        && let Some(router) = node::client_router(tree)
                    {
                        p.spa = true;
                        p.notes.push(format!("{router} in dependencies: a single-page app, so a page path with no file answers index.html"));
                    }
                }
            }
            return Ok(p);
        }
    }
    Err(Error::NothingDetected(found(tree)))
}

/// What the root holds that roll doesn't build yet, said with the error: `" (found mix.exs:
/// Elixir)"`, or nothing.
fn found(tree: &dyn Tree) -> String {
    const KNOWN: [(&str, &str); 6] = [
        ("pubspec.yaml", "Dart"),
        ("Package.swift", "Swift"),
        ("stack.yaml", "Haskell"),
        ("CMakeLists.txt", "C or C++ (CMake)"),
        ("Makefile", "make"),
        ("dune-project", "OCaml"),
    ];
    let mut seen: Vec<String> = KNOWN.iter().filter(|(f, _)| tree.exists(f)).map(|(f, what)| format!("{f}: {what}")).collect();
    if let Some(sln) = tree.list("").into_iter().find(|f| f.ends_with(".sln")) {
        seen.push(format!("{sln}: a .NET solution (set the root directory to its project)"));
    }
    if seen.is_empty() { String::new() } else { format!(" (found {}, which roll doesn't build itself)", seen.join("; ")) }
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
            spa: self.spa,
            packages: self.packages.clone(),
            otel: self.otel,
            build_env: self.build_env.clone(),
            build_files: self.build_files.clone(),
            process: self.process.clone(),
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
    /// [`Overrides::build_files`].
    pub files: &'a [(String, String)],
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
            files: &o.build_files,
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
            spa: false,
            uses: Vec::new(),
            release: None,
            crons: Vec::new(),
            functions: Vec::new(),
            edge: Vec::new(),
            otel: None,
            next_cache: false,
            source_maps: false,
        }
    }
}

/// A Procfile's processes: `name: command` lines (`#` comments; names of letters, digits, `_`
/// and `-`).
fn procfile(tree: &dyn Tree) -> std::collections::BTreeMap<String, String> {
    let Some(t) = text(tree, "Procfile") else { return Default::default() };
    t.lines()
        .filter_map(|l| {
            let l = l.trim();
            let (name, cmd) = l.split_once(':')?;
            let (name, cmd) = (name.trim(), cmd.trim());
            let ok = !l.starts_with('#') && !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') && !cmd.is_empty();
            ok.then(|| (name.to_string(), cmd.to_string()))
        })
        .collect()
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
