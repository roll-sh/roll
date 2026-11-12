//! Static sites: an `index.html`, served by busybox's httpd (a ~4 MB image, so cold starts are
//! fast). Framework builds (Vite, Astro…) use the same runtime for their output.

use crate::df::Df;
use crate::{Draft, Error, Overrides, Provider, Tree};

pub(crate) const IMAGE: &str = "busybox:1.37";
pub(crate) const ROOT: &str = "/site";
/// busybox's `nobody`.
pub(crate) const USER: &str = "65534:65534";
pub(crate) const START: &str = "httpd -f -v -p $PORT -h /site";

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
    let notes = vec![match dir {
        "." => "found index.html: a static site, served by busybox httpd".to_string(),
        _ => {
            "found public/index.html: a static site in public, served by busybox httpd".to_string()
        }
    }];
    let render = Box::new(move |c: &crate::Commands<'_>| {
        let mut df = Df::new();
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
