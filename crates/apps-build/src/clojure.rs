//! Clojure (Heroku's and Railpack's): Leiningen's `lein uberjar` for a `project.clj`, tools.build's
//! `clojure -T:build uber` for a `deps.edn` with a `build.clj`; the standalone jar runs on a JRE.

use crate::df::Df;
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, text};

const LEIN: &str = "clojure:temurin-21-lein";
const DEPS: &str = "clojure:temurin-21-tools-deps";
const JRE: &str = "eclipse-temurin:21-jre";
const CACHES: [&str; 2] = ["/root/.m2", "/root/.gitlibs"];

pub(crate) fn detect(tree: &dyn Tree, o: &Overrides) -> Result<Option<Draft>, Error> {
    let lein = text(tree, "project.clj");
    let deps = text(tree, "deps.edn");
    if lein.is_none() && deps.is_none() {
        return Ok(None);
    }
    let mut notes = Vec::new();
    let (image, install, build, manifests) = if lein.is_some() {
        notes.push("found project.clj: Clojure, built with Leiningen (lein uberjar)".to_string());
        (LEIN, "lein deps", "lein uberjar", vec!["project.clj".to_string()])
    } else {
        let deps = deps.unwrap_or_default();
        if !tree.exists("build.clj") && o.build.is_none() {
            return Err(Error::NoBinary("deps.edn without a build.clj (tools.build's `uber`): add one, or set build and start".into()));
        }
        if !deps.contains(":build") && o.build.is_none() {
            return Err(Error::NoBinary("deps.edn has no :build alias for build.clj (tools.build): add one, or set build".into()));
        }
        notes.push("found deps.edn and build.clj: Clojure, built with tools.build (clojure -T:build uber)".to_string());
        (DEPS, "clojure -P -T:build", "clojure -T:build uber", vec!["deps.edn".to_string()])
    };
    notes.push(format!("the standalone jar runs on {JRE}"));
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::new(c);
        df.from(image, Some("build"));
        df.line("WORKDIR /src");
        df.source(Some(&manifests), &CACHES, c);
        // The uberjar wherever the build put it (`target/`, `target/uberjar/`, a build.clj's own
        // name): the one with its dependencies in (`-standalone`), else the only jar there.
        let pick = "mkdir -p /out && jar=$(find target -maxdepth 2 -name '*-standalone.jar' | head -n 1) && jar=${jar:-$(find target -maxdepth 2 -name '*.jar' | head -n 1)} && test -n \"$jar\" && cp \"$jar\" /out/app.jar";
        match c.build {
            Some(build) => df.run(&CACHES, &format!("{build} && {pick}")),
            None => df.run(&[], pick),
        }
        df.from(JRE, None);
        df.line("RUN useradd --system --uid 10001 app");
        df.line("WORKDIR /app");
        df.line("COPY --from=build /out/app.jar /app/app.jar");
        df.env(&[("JAVA_OPTS", "-XX:MaxRAMPercentage=75")]);
        df.serve(Some("app"), c.start);
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Clojure,
        framework: None,
        language_version: Some("21".into()),
        install: Some(install.into()),
        build: Some(build.into()),
        start: Some("java $JAVA_OPTS -jar /app/app.jar".into()),
        notes,
        render,
    }))
}
