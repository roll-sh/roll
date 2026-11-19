//! A root `start.sh` and nothing else roll builds (Railpack's and Nixpacks' shell provider; after a
//! static site, so one with a dev script stays served as files): the tree on a small Debian image,
//! the script run with bash. Packages it needs come from the service's `packages` (apt).

use crate::df::Df;
use crate::{Commands, Draft, Error, Overrides, Provider, Tree};

const IMAGE: &str = "debian:bookworm-slim";

pub(crate) fn detect(tree: &dyn Tree, _: &Overrides) -> Result<Option<Draft>, Error> {
    if !tree.exists("start.sh") {
        return Ok(None);
    }
    let notes = vec![
        "found start.sh and nothing else roll builds: run with bash on debian:bookworm-slim".to_string(),
        "what it needs installed goes in the service's packages (apt), or a Dockerfile".to_string(),
    ];
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::new(c);
        df.from(IMAGE, None);
        df.line(&format!("RUN {}", crate::df::apt(&["ca-certificates"])));
        df.line("RUN useradd --system --uid 10001 --create-home app");
        df.line("WORKDIR /app");
        df.line("COPY --chown=app . .");
        if let Some(install) = c.install {
            df.run(&[], install);
        }
        if let Some(build) = c.build {
            df.run(&[], build);
        }
        df.serve(Some("app"), c.start);
        df.finish()
    });
    Ok(Some(Draft { provider: Provider::Shell, framework: None, language_version: None, install: None, build: None, start: Some("bash /app/start.sh".into()), notes, render }))
}
