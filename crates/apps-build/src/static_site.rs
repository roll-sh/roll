//! Static sites: an `index.html`, served by busybox's httpd (a ~4 MB image, so cold starts are
//! fast). Framework builds (Vite, Astro…) use the same runtime for their output.

use crate::df::Df;
use crate::{Draft, Error, Overrides, Provider, Tree};

pub(crate) const IMAGE: &str = "busybox:1.37";
pub(crate) const ROOT: &str = "/site";
/// busybox's `nobody`.
pub(crate) const USER: &str = "65534:65534";
pub(crate) const START: &str = "httpd -f -v -p $PORT -h /site";

/// The most a `vercel.json` or `netlify.toml` may weigh to be read.
const MAX_CONFIG: usize = 64 << 10;

/// A `RUN` that appends what the tree's `netlify.toml` and `vercel.json` say to the site's own
/// `_redirects` and `_headers` (as those lines; the configs themselves never go into the image),
/// with notes on what was taken and left out. None: neither says anything.
pub(crate) fn config_rules(tree: &dyn Tree, notes: &mut Vec<String>) -> Option<String> {
    let (mut redirects, mut headers) = (String::new(), String::new());
    for (name, convert) in [("netlify.toml", crate::site_configs::from_netlify as fn(&str) -> crate::site_configs::Converted), ("vercel.json", crate::site_configs::from_vercel)] {
        let Some(text) = tree.read(name).filter(|t| t.len() <= MAX_CONFIG) else { continue };
        let c = convert(&String::from_utf8_lossy(&text));
        let (r, h) = (c.redirects.lines().count(), c.headers.lines().filter(|l| !l.starts_with(' ')).count());
        if r + h > 0 {
            notes.push(format!("{name}: {r} redirect and rewrite rules, {h} header rules, served as the site's own"));
        }
        for why in c.skipped.iter().take(20) {
            notes.push(format!("not taken from {why}"));
        }
        redirects.push_str(&c.redirects);
        headers.push_str(&c.headers);
    }
    // printf's own escapes, in single quotes: no line break, quote or `%` reaches the Dockerfile.
    let quoted = |s: &str| format!("'{}'", s.replace('\\', "\\\\").replace('%', "%%").replace('\n', "\\n").replace('\r', "").replace('\'', "'\\''"));
    let mut cmds = Vec::new();
    if !redirects.is_empty() {
        cmds.push(format!("printf {} >> {ROOT}/_redirects", quoted(&format!("\n{redirects}"))));
    }
    if !headers.is_empty() {
        cmds.push(format!("printf {} >> {ROOT}/_headers", quoted(&format!("\n{headers}"))));
    }
    (!cmds.is_empty()).then(|| cmds.join(" && "))
}

/// The directory holding the site's `index.html`: the root, or `public`.
pub(crate) fn index(tree: &dyn Tree) -> Option<&'static str> {
    if tree.read("index.html").is_some() {
        Some(".")
    } else if tree.read("public/index.html").is_some() {
        Some("public")
    } else {
        None
    }
}

pub(crate) fn detect(tree: &dyn Tree, _: &Overrides) -> Result<Option<Draft>, Error> {
    let Some(dir) = index(tree) else {
        return Ok(None);
    };
    let mut notes = vec![match dir {
        "." => "found index.html: a static site, served by busybox httpd".to_string(),
        _ => {
            "found public/index.html: a static site in public, served by busybox httpd".to_string()
        }
    }];
    let rules = config_rules(tree, &mut notes);
    let render = Box::new(move |c: &crate::Commands<'_>| {
        let mut df = Df::new(c);
        df.from(IMAGE, None);
        if c.install.is_some() || c.build.is_some() {
            // The build steps run in the same small image: there's no toolchain to leave behind.
            df.line("WORKDIR /src");
            df.line("COPY . .");
            for cmd in [c.install, c.build].into_iter().flatten() {
                df.run(&[], cmd);
            }
            df.line(&format!(
                "RUN mkdir -p {ROOT} && cp -r /src/{dir}/. {ROOT}/"
            ));
        } else {
            df.line(&format!("COPY {dir} {ROOT}"));
        }
        if let Some(r) = &rules {
            df.line(&format!("RUN {r}"));
        }
        df.serve(Some(USER), c.start);
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Static,
        framework: None,
        language_version: None,
        install: None,
        build: None,
        start: Some(START.to_string()),
        notes,
        render,
    }))
}
