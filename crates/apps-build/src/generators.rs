//! Static site generators that are one binary (Netlify's, Cloudflare Pages'): Hugo (extended,
//! for its Sass and image processing) and Zola. Each is downloaded at the version pinned here and
//! checked against its SHA-256 for the build's architecture, the site built, and its `public`
//! served by the static server as any static build is.

use crate::df::Df;
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, text};

/// What builds on (Hugo extended and Zola link glibc).
const BASE: &str = "debian:bookworm-slim";

/// A generator: its name, version, download URL (`{arch}` the release's own name for it) and
/// each architecture's SHA-256, by Docker's `TARGETARCH`.
struct Generator {
    name: &'static str,
    version: &'static str,
    url: &'static str,
    archs: [(&'static str, &'static str, &'static str); 2],
    /// The command that builds the site into `public`.
    build: &'static str,
}

const HUGO: Generator = Generator {
    name: "hugo",
    version: "0.169.0",
    url: "https://github.com/gohugoio/hugo/releases/download/v0.169.0/hugo_extended_0.169.0_linux-{arch}.tar.gz",
    archs: [("amd64", "amd64", "c41672a0725d307758a132bc94a962e59760eb33f9ba08c991b81f9881d6fa7d"), ("arm64", "arm64", "5b190e5d2836c09efe25cc6249f70bde8a6c5331ef412a4c9137e9d5de89a515")],
    build: "hugo --minify",
};

const ZOLA: Generator = Generator {
    name: "zola",
    version: "0.23.6",
    url: "https://github.com/getzola/zola/releases/download/v0.23.6/zola-v0.23.6-{arch}.tar.gz",
    archs: [("amd64", "x86_64-unknown-linux-gnu", "8f5132b3522412d04e395e0b25f6d68613ad272a873e54a2b3ebf664873024a4"), ("arm64", "aarch64-unknown-linux-gnu", "266448fffbf7c7004ca399d0e76dd699541771096d8a42aede98cebe2a029d02")],
    build: "zola build",
};

impl Generator {
    /// The step that installs it: the build architecture's tarball, checked, its binary on PATH.
    fn install(&self) -> String {
        let cases: String = self.archs.iter().map(|(docker, arch, sha)| format!("{docker}) a={arch}; s={sha};; ")).collect();
        let url = self.url.replace("{arch}", "$a");
        format!(
            "set -eu; case \"$TARGETARCH\" in {cases}*) echo \"no {name} for $TARGETARCH\"; exit 1;; esac; curl -fsSL -o /tmp/g.tgz \"{url}\"; echo \"$s  /tmp/g.tgz\" | sha256sum -c -; tar -xzf /tmp/g.tgz -C /usr/local/bin {name}; rm /tmp/g.tgz",
            name = self.name
        )
    }
}

/// Another language's app: a config.toml beside one is that app's, never a site's.
const MANIFESTS: [&str; 9] = ["go.mod", "requirements.txt", "pyproject.toml", "Cargo.toml", "Gemfile", "composer.json", "pom.xml", "build.gradle", "mix.exs"];

/// Whether `text` sets `key` at its top level (a line `key = …` before any `[table]`).
fn top_level(text: &str, key: &str) -> bool {
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            return false;
        }
        if let Some(rest) = line.strip_prefix(key)
            && rest.trim_start().starts_with('=')
        {
            return true;
        }
    }
    false
}

pub(crate) fn detect(tree: &dyn Tree, o: &Overrides) -> Result<Option<Draft>, Error> {
    // A start command set, or another language's app here: not a site.
    if o.start.is_some() || MANIFESTS.iter().any(|f| tree.exists(f)) {
        return Ok(None);
    }
    let node_app = text(tree, "package.json").and_then(|p| serde_json::from_str::<serde_json::Value>(&p).ok()).is_some_and(|p| p["scripts"]["start"].is_string());
    // Hugo's own config names; or a config.toml setting `baseURL` at its top level beside Hugo's
    // own directories. Zola's sets `base_url` beside `templates` and `content`.
    let config = text(tree, "config.toml");
    let hugo = ["hugo.toml", "hugo.yaml", "hugo.yml", "hugo.json"].into_iter().any(|f| tree.exists(f))
        || (!node_app && config.as_deref().is_some_and(|c| top_level(c, "baseURL") || top_level(c, "baseurl")) && ["layouts", "themes", "archetypes"].into_iter().any(|d| tree.exists(d)));
    let zola = !hugo && !node_app && config.as_deref().is_some_and(|c| top_level(c, "base_url")) && tree.exists("templates") && tree.exists("content");
    let g = match (hugo, zola) {
        (true, _) => HUGO,
        (_, true) => ZOLA,
        _ => return Ok(None),
    };
    let mut notes = vec![format!("{} config: a static {} site, built into public", if hugo { "Hugo's" } else { "Zola's" }, g.name)];
    notes.push(format!("{} {} downloaded and checked against its SHA-256 for the build's architecture", g.name, g.version));
    if tree.exists("package.json") {
        notes.push("its package.json isn't installed: a theme that needs npm (PostCSS, Tailwind) builds from its own Dockerfile".into());
    }
    let install = g.install();
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::new(c);
        df.from(BASE, Some("build"));
        df.line("ARG TARGETARCH");
        // git: Hugo's `enableGitInfo` and modules.
        df.run(&[], "apt-get update && apt-get install -y --no-install-recommends ca-certificates curl git && rm -rf /var/lib/apt/lists/*");
        df.run(&[], &install);
        df.line("WORKDIR /app");
        df.copy_source();
        if let Some(build) = c.build {
            df.run(&[], build);
        }
        df.from(crate::static_site::IMAGE, None);
        df.line(&format!("COPY --from=build /app/public {}", crate::static_site::ROOT));
        df.serve(Some(crate::static_site::USER), c.start);
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Static,
        framework: Some(g.name),
        language_version: Some(g.version.into()),
        install: None,
        build: Some(g.build.into()),
        start: Some(crate::static_site::START.into()),
        notes,
        render,
    }))
}
