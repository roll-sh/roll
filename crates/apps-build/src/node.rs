//! Node: npm, pnpm, yarn and bun; Next, Nuxt, Remix, Astro, SvelteKit, Nest, Express and the
//! static builders (Vite, Create React App, Gatsby), whose output a static server serves.

use serde_json::Value;

use crate::df::Df;
use crate::static_site;
use crate::version::{self, Ver};
use crate::{Draft, Error, Overrides, Provider, Tree, invalid, text};

const DEFAULT: Ver = Ver(22, 0);

/// Files whose presence means `package.json` only builds another language's assets.
const OTHER_LANGUAGES: [&str; 11] = [
    "Gemfile",
    "composer.json",
    "requirements.txt",
    "pyproject.toml",
    "manage.py",
    "go.mod",
    "Cargo.toml",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "mix.exs",
];

pub(crate) fn detect(tree: &dyn Tree, o: &Overrides) -> Result<Option<Draft>, Error> {
    let Some(pkg) = package(tree)? else {
        return Ok(None);
    };
    if OTHER_LANGUAGES.iter().any(|f| tree.exists(f)) {
        return Ok(None);
    }
    let mut notes = Vec::new();
    let node = Setup::read(tree, &pkg, &mut notes);
    let script = |name: &str| {
        pkg["scripts"][name]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let has = |dep: &str| {
        ["dependencies", "devDependencies"]
            .iter()
            .any(|k| pkg[k].get(dep).is_some())
    };
    let start_script = script("start");
    let build = script("build").map(|_| node.pm.run("build"));
    if let Some(b) = &build {
        notes.push(format!("a build script: {b}"));
    }

    // A framework that builds a static site names the directory its output goes to.
    let static_build = |framework: &'static str, dir: &'static str, why: String| StaticBuild {
        framework,
        dir,
        why,
    };
    let fw: Option<Framework> = if has("next") {
        let start = "node_modules/.bin/next start -p $PORT".to_string();
        Some(Framework::server(
            "next",
            "Next.js",
            start_script.is_none().then_some(start),
        ))
    } else if has("nuxt") {
        Some(Framework::entry("nuxt", "Nuxt", "node .output/server/index.mjs").output(".output"))
    } else if has("@remix-run/dev") || has("@remix-run/node") {
        let entry = if has("vite") {
            "./build/server/index.js"
        } else {
            "./build/index.js"
        };
        let start = format!("node_modules/.bin/remix-serve {entry}");
        Some(Framework::server(
            "remix",
            "Remix",
            start_script.is_none().then_some(start),
        ))
    } else if has("@sveltejs/kit") {
        if has("@sveltejs/adapter-node") {
            Some(Framework::entry(
                "sveltekit",
                "SvelteKit with adapter-node",
                "node build",
            ))
        } else if has("@sveltejs/adapter-static") {
            Some(Framework::Static(static_build(
                "sveltekit",
                "build",
                "@sveltejs/adapter-static in dependencies: a static SvelteKit site".into(),
            )))
        } else {
            notes.push(
                "SvelteKit without adapter-node or adapter-static: add @sveltejs/adapter-node so \
                 `node build` runs the server"
                    .into(),
            );
            Some(Framework::entry("sveltekit", "SvelteKit", "node build"))
        }
    } else if has("astro") {
        if has("@astrojs/node") {
            Some(Framework::entry(
                "astro",
                "Astro with @astrojs/node",
                "node ./dist/server/entry.mjs",
            ))
        } else {
            Some(Framework::Static(static_build(
                "astro",
                "dist",
                "astro without a server adapter: a static site in dist".into(),
            )))
        }
    } else if has("@nestjs/core") {
        Some(Framework::entry("nestjs", "NestJS", "node dist/main"))
    } else if has("gatsby") {
        Some(Framework::Static(static_build(
            "gatsby",
            "public",
            "gatsby in dependencies: a static site in public".into(),
        )))
    } else if has("react-scripts") {
        Some(Framework::Static(static_build(
            "create-react-app",
            "build",
            "react-scripts in dependencies: Create React App, a static site in build".into(),
        )))
    } else if has("vite") && start_script.is_none_or(|s| s.starts_with("vite")) {
        Some(Framework::Static(static_build(
            "vite",
            "dist",
            "vite in dependencies: a static site in dist".into(),
        )))
    } else {
        [
            ("express", "Express"),
            ("fastify", "Fastify"),
            ("koa", "Koa"),
            ("hono", "Hono"),
            ("@hapi/hapi", "hapi"),
        ]
        .into_iter()
        .find(|(d, _)| has(d))
        .map(|(d, label)| Framework::server(d.trim_start_matches("@hapi/"), label, None))
    };

    let (framework, start, output) = match fw {
        Some(Framework::Static(s)) => {
            notes.push(s.why);
            if build.is_none() && o.build.is_none() {
                notes.push(format!("no build script: {} won't exist", s.dir));
            }
            notes.push(format!("{}/ is served by busybox httpd", s.dir));
            return Ok(Some(static_build_draft(
                node,
                notes,
                s.framework,
                s.dir,
                build,
            )));
        }
        Some(Framework::Server {
            name,
            label,
            start: default,
        }) => {
            notes.push(format!("{} in dependencies: {label}", dep_of(name)));
            let start = match (start_script, default) {
                (Some(script), _) => Some(node.pm.start(script, &pkg)),
                (None, Some(cmd)) => Some(cmd),
                (None, None) => entry(tree, &pkg),
            };
            (Some(name), start, None)
        }
        Some(Framework::Entry {
            name,
            label,
            start,
            output,
        }) => {
            notes.push(format!(
                "{} in dependencies: {label}, started with {start}",
                dep_of(name)
            ));
            (Some(name), Some(start.to_string()), output)
        }
        None => {
            let start = match start_script {
                Some(script) => Some(node.pm.start(script, &pkg)),
                None => entry(tree, &pkg),
            };
            if start.is_none() && build.is_some() && o.start.is_none() {
                notes.push("a build script and no start script: a static site in dist".into());
                notes.push("dist/ is served by busybox httpd".into());
                return Ok(Some(static_build_draft(node, notes, "", "dist", build)));
            }
            if start.is_none() && o.start.is_none() && static_site::index(tree).is_some() {
                return Ok(None);
            }
            (None, start, None)
        }
    };
    match (&start, start_script) {
        (Some(s), Some(script)) if s == script => {
            notes.push(format!("a start script that runs node directly: {s}"))
        }
        (Some(s), Some(_)) if s == &node.pm.start_script() => {
            notes.push(format!("a start script: {s}"))
        }
        (Some(s), _) => notes.push(format!("starts with {s}")),
        (None, _) => {
            notes.push("nothing says how to start it: add a start script or set start".into())
        }
    }
    let output = output.filter(|_| o.start.is_none());
    // The runtime image without devDependencies (as Heroku's Node builds do), unless the app
    // starts with one (`ts-node`, `nodemon`) or only its output directory is copied anyway.
    let starts_with: String = [start.as_deref(), start_script, o.start.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" ");
    let dev_used = pkg["devDependencies"].as_object().into_iter().flat_map(|m| m.keys()).find(|d| starts_with.split(|c: char| !(c.is_alphanumeric() || "@/-_.".contains(c))).any(|w| w == d.as_str()));
    let prune = match (node.pm, &output, dev_used) {
        (_, Some(_), _) => None,
        (_, None, Some(d)) => {
            notes.push(format!("the start command uses {d}, a devDependency: devDependencies stay in the image"));
            None
        }
        (Pm::Npm, None, None) => Some("npm prune --omit=dev"),
        (Pm::Pnpm, None, None) => Some("pnpm prune --prod"),
        _ => None,
    };
    if let Some(p) = prune {
        notes.push(format!("devDependencies are pruned after the build ({p})"));
    }
    let install = Some(node.install());
    let version = node.version.to_string();
    let render = Box::new(move |c: &crate::Commands<'_>| {
        let mut df = Df::new(c);
        node.base(&mut df, "base");
        df.from("base", Some("build"));
        node.source(&mut df, c);
        if let Some(build) = c.build {
            df.run(&node.build_caches(framework), build);
        }
        if let Some(p) = prune {
            df.run(&node.caches(), p);
        }
        df.from("base", None);
        df.env(&[("NODE_ENV", "production")]);
        node.runtime_extras(&mut df);
        match output {
            Some(dir) => df.line(&format!(
                "COPY --from=build --chown=node:node /app/{dir} /app/{dir}"
            )),
            None => df.line("COPY --from=build --chown=node:node /app /app"),
        }
        if framework == Some("astro") {
            df.env(&[("HOST", "0.0.0.0")]);
        }
        df.serve(Some("node"), c.start);
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Node,
        framework,
        language_version: Some(version),
        install,
        build,
        start,
        notes,
        render,
    }))
}

/// `package.json`, when there is one.
fn package(tree: &dyn Tree) -> Result<Option<Value>, Error> {
    let Some(bytes) = tree.read("package.json") else {
        return Ok(None);
    };
    let pkg: Value = serde_json::from_slice(&bytes).map_err(|e| invalid("package.json", e))?;
    if !pkg.is_object() {
        return Err(invalid("package.json", "it isn't a JSON object"));
    }
    Ok(Some(pkg))
}

/// A Node build that only makes assets for another language's app (Laravel's Vite, Rails'
/// jsbundling): the stage `assets`, with the tree at `/app` after `build`, without node_modules.
pub(crate) struct Assets {
    node: Setup,
    build: String,
}

impl Assets {
    /// The stage's text, after the syntax line of the Dockerfile it goes in.
    pub(crate) fn render(&self, c: &crate::Commands<'_>) -> String {
        let mut df = Df::new(c);
        self.node.base(&mut df, "assets-base");
        df.from("assets-base", Some("assets"));
        df.line("COPY . .");
        df.run(&self.node.caches(), &self.node.install());
        df.run(&[], &format!("{} && rm -rf node_modules", self.build));
        df.finish().trim_start_matches("# syntax=docker/dockerfile:1\n").to_string()
    }
}

pub(crate) fn assets(tree: &dyn Tree, notes: &mut Vec<String>) -> Result<Option<Assets>, Error> {
    let Some(pkg) = package(tree)? else {
        return Ok(None);
    };
    if pkg["scripts"]["build"]
        .as_str()
        .is_none_or(|s| s.trim().is_empty())
    {
        return Ok(None);
    }
    let node = Setup::read(tree, &pkg, notes);
    let build = node.pm.run("build");
    notes.push(format!(
        "package.json has a build script: the assets build with {build} in Node {}",
        node.version
    ));
    Ok(Some(Assets { node, build }))
}

enum Framework {
    /// A server started by the start script when there is one, else `start`.
    Server {
        name: &'static str,
        label: &'static str,
        start: Option<String>,
    },
    /// A built server entry, run directly (no package manager in front, faster cold starts).
    /// `output` is all the runtime needs when it's self-contained.
    Entry {
        name: &'static str,
        label: &'static str,
        start: &'static str,
        output: Option<&'static str>,
    },
    Static(StaticBuild),
}

impl Framework {
    fn server(name: &'static str, label: &'static str, start: Option<String>) -> Self {
        Framework::Server { name, label, start }
    }
    fn entry(name: &'static str, label: &'static str, start: &'static str) -> Self {
        Framework::Entry {
            name,
            label,
            start,
            output: None,
        }
    }
    fn output(self, dir: &'static str) -> Self {
        match self {
            Framework::Entry {
                name, label, start, ..
            } => Framework::Entry {
                name,
                label,
                start,
                output: Some(dir),
            },
            other => other,
        }
    }
}

struct StaticBuild {
    framework: &'static str,
    dir: &'static str,
    why: String,
}

fn dep_of(framework: &str) -> &str {
    match framework {
        "nuxt" | "next" | "astro" | "express" | "fastify" | "koa" | "hono" => framework,
        "remix" => "@remix-run/dev",
        "sveltekit" => "@sveltejs/kit",
        "nestjs" => "@nestjs/core",
        "hapi" => "@hapi/hapi",
        other => other,
    }
}

/// `node <main>`, `node index.js` or `node server.js`, when the file is there.
fn entry(tree: &dyn Tree, pkg: &Value) -> Option<String> {
    let main = pkg["main"]
        .as_str()
        .map(|m| m.trim_start_matches("./").to_string());
    main.into_iter()
        .chain(["index.js", "server.js", "app.js", "main.js"].map(String::from))
        .find(|f| crate::tree::valid(f) && tree.read(f).is_some())
        .map(|f| format!("node {f}"))
}

fn static_build_draft(
    node: Setup,
    notes: Vec<String>,
    framework: &'static str,
    dir: &'static str,
    build: Option<String>,
) -> Draft {
    let version = node.version.to_string();
    let install = Some(node.install());
    let render = Box::new(move |c: &crate::Commands<'_>| {
        let mut df = Df::new(c);
        node.base(&mut df, "base");
        df.from("base", Some("build"));
        node.source(&mut df, c);
        if let Some(build) = c.build {
            df.run(&[], build);
        }
        df.from(static_site::IMAGE, None);
        df.line(&format!(
            "COPY --from=build /app/{dir} {}",
            static_site::ROOT
        ));
        df.serve(Some(static_site::USER), c.start);
        df.finish()
    });
    Draft {
        provider: Provider::Static,
        framework: (!framework.is_empty()).then_some(framework),
        language_version: Some(version),
        install,
        build,
        start: Some(static_site::START.to_string()),
        notes,
        render,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pm {
    Npm,
    Pnpm,
    Yarn { berry: bool },
    Bun,
}

impl Pm {
    fn name(self) -> &'static str {
        match self {
            Pm::Npm => "npm",
            Pm::Pnpm => "pnpm",
            Pm::Yarn { .. } => "yarn",
            Pm::Bun => "bun",
        }
    }

    fn run(self, script: &str) -> String {
        format!("{} run {script}", self.name())
    }

    /// How the runtime runs a start script. A plain `node …` runs as itself: no package manager
    /// process in front of it at every cold start, and signals go straight to node. Not when a
    /// `prestart`/`poststart` hook would be skipped, under Bun (its image may have no node) or
    /// Yarn Berry (Plug'n'Play needs yarn's loader), or when the script needs a shell.
    fn start(self, script: &str, pkg: &Value) -> String {
        let plain = |t: &str| t.bytes().all(|b| b.is_ascii_alphanumeric() || b"_./=@:-".contains(&b));
        let hooks = ["prestart", "poststart"].iter().any(|h| pkg["scripts"].get(h).is_some());
        let direct = matches!(self, Pm::Npm | Pm::Pnpm | Pm::Yarn { berry: false })
            && !hooks
            && script.split_whitespace().next() == Some("node")
            && script.split_whitespace().all(plain);
        if direct { script.split_whitespace().collect::<Vec<_>>().join(" ") } else { self.start_script() }
    }

    /// The package manager running the start script. npm runs any package manager's scripts the
    /// same way and ships in the image, so pnpm and yarn needn't be downloaded at a cold start.
    fn start_script(self) -> String {
        match self {
            Pm::Yarn { berry: true } => "yarn run start".into(),
            Pm::Bun => "bun run start".into(),
            _ => "npm run start".into(),
        }
    }

    fn corepack(self) -> bool {
        matches!(self, Pm::Pnpm | Pm::Yarn { .. })
    }
}

/// The Node toolchain a tree asks for.
#[derive(Clone)]
struct Setup {
    pm: Pm,
    version: u32,
    locked: bool,
    /// What the install reads, when that's all it reads (no workspaces, patches or install
    /// scripts): they're copied first so the install's layer survives source edits.
    manifests: Option<Vec<String>>,
    /// Yarn Plug'n'Play: no node_modules, so the runtime needs yarn itself.
    pnp: bool,
}

impl Setup {
    fn read(tree: &dyn Tree, pkg: &Value, notes: &mut Vec<String>) -> Setup {
        let declared = pkg["packageManager"].as_str().and_then(|s| {
            let (name, v) = s.split_once('@').unwrap_or((s, ""));
            let pm = match name {
                "npm" => Pm::Npm,
                "pnpm" => Pm::Pnpm,
                "yarn" => Pm::Yarn {
                    berry: version::parse(v).is_some_and(|v| v.0 >= 2),
                },
                "bun" => Pm::Bun,
                _ => return None,
            };
            Some((pm, s))
        });
        let lockfile = [
            ("pnpm-lock.yaml", Pm::Pnpm),
            ("bun.lock", Pm::Bun),
            ("bun.lockb", Pm::Bun),
            ("yarn.lock", Pm::Yarn { berry: false }),
            ("package-lock.json", Pm::Npm),
            ("npm-shrinkwrap.json", Pm::Npm),
        ]
        .into_iter()
        .find(|(f, _)| tree.exists(f));
        let berry_files = tree.exists(".yarnrc.yml")
            || text(tree, "yarn.lock").is_some_and(|l| l.contains("__metadata:"));
        let mut pm = match (declared, lockfile) {
            (Some((pm, field)), _) => {
                notes.push(format!("packageManager is {field}: {}", pm.name()));
                pm
            }
            (None, Some((file, pm))) => {
                notes.push(format!("found {file}: {}", pm.name()));
                pm
            }
            (None, None) => {
                notes.push("no lockfile: npm".into());
                Pm::Npm
            }
        };
        if let Pm::Yarn { berry } = &mut pm {
            *berry |= berry_files;
        }
        if pm.corepack() {
            notes.push(format!("corepack provides {}", pm.name()));
        }
        let lock = lockfile
            .filter(|(_, l)| l.name() == pm.name())
            .map(|(f, _)| f);
        if lock.is_none() && declared.is_some() {
            notes.push(format!(
                "no {} lockfile: the install isn't frozen",
                pm.name()
            ));
        }
        let pnp = matches!(pm, Pm::Yarn { berry: true })
            && !text(tree, ".yarnrc.yml").is_some_and(|y| {
                y.contains("nodeLinker: node-modules") || y.contains("nodeLinker: \"node-modules\"")
            });

        let version = node_version(tree, pkg, notes);

        let scripts = &pkg["scripts"];
        let whole_tree = pkg.get("workspaces").is_some()
            || ["preinstall", "install", "postinstall", "prepare"]
                .iter()
                .any(|s| scripts.get(s).is_some())
            || ["pnpm-workspace.yaml", ".yarn", "patches", ".pnpmfile.cjs"]
                .iter()
                .any(|f| tree.exists(f));
        let manifests = (!whole_tree).then(|| {
            [
                "package.json",
                ".npmrc",
                ".yarnrc",
                ".yarnrc.yml",
                "bunfig.toml",
            ]
            .into_iter()
            .chain(lock)
            .filter(|f| tree.exists(f))
            .map(String::from)
            .collect()
        });
        Setup {
            pm,
            version,
            locked: lock.is_some(),
            manifests,
            pnp,
        }
    }

    fn install(&self) -> String {
        match (self.pm, self.locked) {
            (Pm::Npm, true) => "npm ci".into(),
            (Pm::Npm, false) => "npm install".into(),
            (Pm::Pnpm, true) => "pnpm install --frozen-lockfile".into(),
            (Pm::Pnpm, false) => "pnpm install".into(),
            (Pm::Yarn { berry: true }, true) => "yarn install --immutable".into(),
            (Pm::Yarn { berry: false }, true) => "yarn install --frozen-lockfile".into(),
            (Pm::Yarn { .. }, false) => "yarn install".into(),
            (Pm::Bun, true) => "bun install --frozen-lockfile".into(),
            (Pm::Bun, false) => "bun install".into(),
        }
    }

    fn caches(&self) -> Vec<&'static str> {
        vec![match self.pm {
            Pm::Npm => "/root/.npm",
            Pm::Pnpm => "/root/.local/share/pnpm/store",
            Pm::Yarn { berry: true } => "/root/.yarn/berry/cache",
            Pm::Yarn { berry: false } => "/usr/local/share/.cache/yarn",
            Pm::Bun => "/root/.bun/install/cache",
        }]
    }

    fn build_caches(&self, framework: Option<&str>) -> Vec<&'static str> {
        match framework {
            Some("next") => vec!["/app/.next/cache"],
            _ => Vec::new(),
        }
    }

    /// The stage `name`: Node and the package manager.
    fn base(&self, df: &mut Df, name: &str) {
        df.from(&format!("node:{}-slim", self.version), Some(name));
        if self.pm.corepack() {
            df.env(&[
                ("COREPACK_HOME", "/opt/corepack"),
                ("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0"),
            ]);
            df.run(
                &["/root/.npm"],
                "npm install -g corepack@latest && corepack enable",
            );
        }
        if self.pm == Pm::Bun {
            df.line("COPY --from=oven/bun:1 /usr/local/bin/bun /usr/local/bin/bun");
        }
        df.line("WORKDIR /app");
    }

    fn source(&self, df: &mut Df, c: &crate::Commands<'_>) {
        df.source(self.manifests.as_deref(), &self.caches(), c);
    }

    fn runtime_extras(&self, df: &mut Df) {
        if self.pnp {
            df.line("COPY --from=build /opt/corepack /opt/corepack");
        }
    }
}

fn node_version(tree: &dyn Tree, pkg: &Value, notes: &mut Vec<String>) -> u32 {
    if let Some(c) = pkg["engines"]["node"].as_str()
        && let Some(v) = version::pick(c, DEFAULT)
    {
        notes.push(format!("engines.node is {c}: Node {}", v.0));
        return v.0;
    }
    for file in [".nvmrc", ".node-version"] {
        let Some(pin) = text(tree, file) else {
            continue;
        };
        let pin = pin.trim();
        match version::parse(pin) {
            Some(v) => {
                notes.push(format!("{file} pins {pin}: Node {}", v.0));
                return v.0;
            }
            None => notes.push(format!("{file} says {pin}: Node {}", DEFAULT.0)),
        }
        return DEFAULT.0;
    }
    notes.push(format!("no Node version set: Node {}", DEFAULT.0));
    DEFAULT.0
}
