//! Dart (Nixpacks' Dart; Dart's own server Dockerfile): `dart compile exe` on the Dart image, the
//! native executable run on Debian slim (its glibc; a start command needs a shell, which
//! Dart's own `scratch` and `/runtime` image has none of). Shelf and Dart Frog servers read
//! `PORT`.

use crate::df::Df;
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, text};

const IMAGE: &str = "dart:3.13";
const RUN: &str = "debian:bookworm-slim";

pub(crate) fn detect(tree: &dyn Tree, _o: &Overrides) -> Result<Option<Draft>, Error> {
    let Some(pubspec) = text(tree, "pubspec.yaml") else {
        return Ok(None);
    };
    // A Flutter app isn't a server.
    if pubspec.lines().any(|l| l.trim_start().starts_with("flutter:")) {
        return Err(Error::NoBinary("pubspec.yaml is a Flutter app's: roll builds Dart servers (bin/server.dart), not Flutter apps (build it for the web and deploy the output, or add a Dockerfile)".into()));
    }
    let name = pubspec.lines().find_map(|l| l.strip_prefix("name:")).map(|n| n.trim().trim_matches(['"', '\'']).to_string()).unwrap_or_default();
    // Its entry: bin/server.dart (Shelf's template), else bin/<name>.dart.
    let entry = ["bin/server.dart".to_string(), format!("bin/{name}.dart")].into_iter().find(|f| tree.exists(f) && crate::tree::plain(f));
    let Some(entry) = entry else {
        return Err(Error::NoBinary("pubspec.yaml without a bin/server.dart (or bin/<name>.dart): roll compiles a Dart server's entry point (add one, or a Dockerfile)".into()));
    };
    let notes = vec![format!("found pubspec.yaml: Dart, {entry} compiled to a native executable"), "it reads PORT".to_string()];
    let manifests: Vec<String> = ["pubspec.yaml", "pubspec.lock"].into_iter().filter(|f| tree.exists(f)).map(String::from).collect();
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::new(c);
        df.from(IMAGE, Some("build"));
        df.line("WORKDIR /src");
        df.source(Some(&manifests), &[], c);
        if let Some(build) = c.build {
            df.run(&[], &format!("mkdir -p /out && {build}"));
        }
        df.from(RUN, None);
        df.run(&[], "apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* && useradd --system --uid 10001 app");
        df.line("COPY --from=build --chown=app /out/server /app/server");
        df.serve(Some("app"), c.start);
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Dart,
        framework: None,
        language_version: Some("3.13".into()),
        install: Some("dart pub get".into()),
        build: Some(format!("dart compile exe {entry} -o /out/server")),
        start: Some("/app/server".into()),
        notes,
        render,
    }))
}
