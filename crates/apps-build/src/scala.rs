//! Scala with sbt (Heroku's and Railpack's Scala): sbt-native-packager's `stage` (a start script
//! under `target/universal/stage/bin`, Play's way) or sbt-assembly's one jar, built on the sbt
//! image and run on a JRE.

use crate::df::Df;
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, text};

const SBT: &str = "sbtscala/scala-sbt:eclipse-temurin-21.0.5_11_1.10.7_3.6.2";
const JRE: &str = "eclipse-temurin:21-jre";
/// Where sbt, Coursier and Ivy keep what they downloaded: cache mounts, so a rebuild doesn't
/// fetch the world again.
const CACHES: [&str; 3] = ["/root/.cache/coursier", "/root/.ivy2", "/root/.sbt"];

#[derive(Clone, Copy, PartialEq)]
enum Package {
    /// sbt-native-packager: `stage`, a script that runs the app.
    Stage,
    /// sbt-assembly: one jar.
    Assembly,
}

/// sbt-native-packager's `normalizedName`: lowercase, runs of other characters as `-`.
fn normalized(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

pub(crate) fn detect(tree: &dyn Tree, o: &Overrides) -> Result<Option<Draft>, Error> {
    let Some(build) = text(tree, "build.sbt") else {
        return Ok(None);
    };
    let plugins = text(tree, "project/plugins.sbt").unwrap_or_default();
    let mut notes = vec!["found build.sbt: Scala, built with sbt".to_string()];
    let play = build.contains("PlayScala") || plugins.contains("sbt-plugin\"") && plugins.contains("playframework");
    let package = if play || plugins.contains("sbt-native-packager") {
        Package::Stage
    } else if plugins.contains("sbt-assembly") {
        Package::Assembly
    } else if o.start.is_some() {
        notes.push("neither sbt-native-packager nor sbt-assembly in project/plugins.sbt: the start set in the service runs in the build's tree".into());
        Package::Stage
    } else {
        return Err(Error::NoBinary("build.sbt: add sbt-native-packager (stage) or sbt-assembly (one jar) to project/plugins.sbt, or set start".into()));
    };
    // `name := "shop"` names the stage's start script.
    let name = build
        .lines()
        .find_map(|l| l.trim().strip_prefix("name").map(str::trim_start).and_then(|r| r.strip_prefix(":=")).map(str::trim))
        .and_then(|v| v.strip_prefix('"').and_then(|v| v.split('"').next()))
        .map(normalized)
        .filter(|n| !n.is_empty());
    let (build_cmd, start) = match package {
        Package::Stage => {
            let Some(name) = name.clone().or_else(|| o.start.is_some().then(String::new)) else {
                return Err(Error::NoBinary("build.sbt sets no `name := \"…\"`, which names the start script: set it, or set start".into()));
            };
            notes.push(if play { "Play: built with sbt stage, its port from $PORT".into() } else { "sbt-native-packager: built with sbt stage".to_string() });
            let start = if play {
                format!("/app/bin/{name} -Dhttp.port=$PORT -Dplay.server.pidfile.path=/dev/null")
            } else {
                format!("/app/bin/{name}")
            };
            ("sbt -batch stage", start)
        }
        Package::Assembly => {
            notes.push("sbt-assembly: built as one jar (sbt assembly)".into());
            ("sbt -batch assembly", "java $JAVA_OPTS -jar /app/app.jar".to_string())
        }
    };
    if play {
        notes.push("Play needs its secret: set PLAY_HTTP_SECRET_KEY (play.http.secret.key = ${?PLAY_HTTP_SECRET_KEY})".into());
    }
    notes.push(format!("it runs on {JRE}"));
    let mut manifests = vec!["build.sbt".to_string()];
    if tree.exists("project") {
        manifests.push("project".into());
    }
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::new(c);
        df.from(SBT, Some("build"));
        df.line("WORKDIR /src");
        // The build definition first: its dependencies' layer survives code edits.
        match (c.install, c.custom_install) {
            (Some(install), false) => {
                df.line("COPY build.sbt ./");
                if manifests.iter().any(|m| m == "project") {
                    df.line("COPY project ./project");
                }
                df.run(&CACHES, install);
                df.copy_source();
            }
            (install, _) => {
                df.copy_source();
                if let Some(install) = install {
                    df.run(&CACHES, install);
                }
            }
        }
        let pick = match package {
            Package::Stage => "mkdir -p /out && cp -r target/universal/stage/. /out/",
            Package::Assembly => "mkdir -p /out && jar=$(ls target/scala-*/*.jar 2>/dev/null | head -n 1) && test -n \"$jar\" && cp \"$jar\" /out/app.jar",
        };
        match c.build {
            Some(build) => df.run(&CACHES, &format!("{build} && {pick}")),
            None => df.run(&[], pick),
        }
        df.from(JRE, None);
        df.line("RUN useradd --system --uid 10001 app");
        df.line("WORKDIR /app");
        df.line("COPY --from=build --chown=app /out/ /app/");
        df.env(&[("JAVA_OPTS", "-XX:MaxRAMPercentage=75")]);
        df.serve(Some("app"), c.start);
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Scala,
        framework: play.then_some("play"),
        language_version: Some("21".into()),
        install: Some("sbt -batch update".into()),
        build: Some(build_cmd.into()),
        start: Some(start),
        notes,
        render,
    }))
}
