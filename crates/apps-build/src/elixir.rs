//! Elixir: a `mix release` on the Elixir image, run on the same image (its ERTS needs the same
//! system libraries) with `PHX_SERVER=true`, so a Phoenix app serves (Render's, Fly's and
//! Railpack's Elixir).

use crate::df::{Df, apt};
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, text};

const IMAGE: &str = "elixir:1.17-slim";

pub(crate) fn detect(tree: &dyn Tree, o: &Overrides) -> Result<Option<Draft>, Error> {
    let Some(mix) = text(tree, "mix.exs") else {
        return Ok(None);
    };
    let mut notes = vec!["found mix.exs: Elixir, built as a release (mix release)".to_string()];
    // `app: :shop` names the release, and what runs.
    let app = mix
        .split("app:")
        .nth(1)
        .and_then(|r| r.trim_start().strip_prefix(':'))
        .map(|r| r.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect::<String>())
        .filter(|a| !a.is_empty());
    let Some(app) = app else {
        if o.start.is_none() {
            return Err(Error::NoBinary("mix.exs names no app (`app: :name`): set start".into()));
        }
        return Ok(None);
    };
    let phoenix = mix.contains(":phoenix");
    let assets = phoenix && tree.exists("assets") && mix.contains("assets.deploy");
    if phoenix {
        notes.push("phoenix in mix.exs: PHX_SERVER=true, and SECRET_KEY_BASE must be set (mix phx.gen.secret)".into());
    }
    if assets {
        notes.push("an assets.deploy alias and assets/: deployed before the release".into());
    }
    let build = Some(if assets { "mix assets.deploy && mix release".to_string() } else { "mix release".to_string() });
    let start = Some(format!("/app/bin/{app} start"));
    notes.push(format!("the release runs as /app/bin/{app} start"));
    let manifests: Vec<String> = ["mix.exs", "mix.lock"].into_iter().filter(|f| tree.exists(f)).map(String::from).collect();
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::new(c);
        df.from(IMAGE, Some("build"));
        df.line("WORKDIR /app");
        df.env(&[("MIX_ENV", "prod"), ("LANG", "C.UTF-8")]);
        // The slim image has no CA certificates (Hex is fetched over TLS) and no git (a git dep).
        df.line(&format!("RUN {}", apt(&["ca-certificates", "git"])));
        df.line("RUN mix local.hex --force && mix local.rebar --force");
        // The dependencies first (their layer survives code edits), compiled, then the code.
        df.source(Some(&manifests), &[], c);
        if let Some(build) = c.build {
            df.run(&[], build);
        }
        df.from(IMAGE, None);
        // An app's own TLS calls (an API, a database over TLS) need the certificates too.
        df.line(&format!("RUN {}", apt(&["ca-certificates"])));
        df.line("WORKDIR /app");
        df.line(&format!("COPY --from=build --chown=nobody:nogroup /app/_build/prod/rel/{app} ./"));
        // A release writes to its tmp: where `nobody` can.
        df.env(&[("PHX_SERVER", "true"), ("LANG", "C.UTF-8"), ("RELEASE_TMP", "/tmp")]);
        df.serve(Some("nobody"), c.start);
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Elixir,
        framework: phoenix.then_some("phoenix"),
        language_version: Some("1.17".into()),
        install: Some("mix deps.get --only prod && mix deps.compile".into()),
        build,
        start,
        notes,
        render,
    }))
}
