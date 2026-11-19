//! Gleam (Railpack's, Nixpacks'): `gleam export erlang-shipment` on the Gleam image, the
//! shipment run on the same image's Erlang. A JavaScript-target project isn't built here.

use crate::df::Df;
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, text};

const IMAGE: &str = "ghcr.io/gleam-lang/gleam:v1.9.1-erlang-slim";

pub(crate) fn detect(tree: &dyn Tree, _: &Overrides) -> Result<Option<Draft>, Error> {
    let Some(toml) = text(tree, "gleam.toml") else {
        return Ok(None);
    };
    let target = toml.lines().find_map(|l| l.trim().strip_prefix("target").map(str::trim_start).and_then(|r| r.strip_prefix('=')).map(|v| v.trim().trim_matches('"').to_string()));
    if target.as_deref() == Some("javascript") {
        return Err(Error::NoBinary("gleam.toml targets JavaScript: roll builds Gleam's Erlang target (set target = \"erlang\", or add a Dockerfile)".into()));
    }
    let notes = vec![
        "found gleam.toml: Gleam, exported as an Erlang shipment (gleam export erlang-shipment)".to_string(),
        "it runs as build/erlang-shipment/entrypoint.sh run".to_string(),
    ];
    let manifests: Vec<String> = ["gleam.toml", "manifest.toml"].into_iter().filter(|f| tree.exists(f)).map(String::from).collect();
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::new(c);
        df.from(IMAGE, Some("build"));
        df.line("WORKDIR /src");
        df.source(Some(&manifests), &[], c);
        if let Some(build) = c.build {
            df.run(&[], build);
        }
        df.from(IMAGE, None);
        df.line("RUN useradd --system --uid 10001 app");
        df.line("WORKDIR /app");
        df.line("COPY --from=build --chown=app /src/build/erlang-shipment /app");
        df.serve(Some("app"), c.start);
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Gleam,
        framework: None,
        language_version: Some("1.9".into()),
        install: Some("gleam deps download".into()),
        build: Some("gleam export erlang-shipment".into()),
        start: Some("/app/entrypoint.sh run".into()),
        notes,
        render,
    }))
}
