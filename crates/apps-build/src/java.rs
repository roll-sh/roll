//! Java: Maven and Gradle (their wrappers when the tree has them); Spring Boot. The jar runs on
//! a JRE image.

use crate::df::Df;
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, text};

const DEFAULT: u32 = 21;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Maven,
    Gradle,
}

pub(crate) fn detect(tree: &dyn Tree, _: &Overrides) -> Result<Option<Draft>, Error> {
    let (tool, file, build_file) = if let Some(pom) = text(tree, "pom.xml") {
        (Tool::Maven, "pom.xml", pom)
    } else if let Some(g) = text(tree, "build.gradle.kts") {
        (Tool::Gradle, "build.gradle.kts", g)
    } else if let Some(g) = text(tree, "build.gradle") {
        (Tool::Gradle, "build.gradle", g)
    } else {
        return Ok(None);
    };
    let mut notes = Vec::new();
    let wrapper = match tool {
        Tool::Maven => tree.exists("mvnw").then_some("mvnw"),
        Tool::Gradle => tree.exists("gradlew").then_some("gradlew"),
    };
    let spring = match tool {
        Tool::Maven => build_file.contains("spring-boot"),
        Tool::Gradle => build_file.contains("org.springframework.boot"),
    };
    let java = match java_version(tool, &build_file).or_else(|| {
        text(tree, ".java-version")
            .and_then(|v| crate::version::parse(v.trim()))
            .map(|v| v.0)
    }) {
        Some(v) => {
            notes.push(format!("{file} targets Java {v}"));
            v
        }
        None => {
            notes.push(format!("no Java version set: Java {DEFAULT}"));
            DEFAULT
        }
    };
    let (image, build) = match (tool, wrapper) {
        (Tool::Maven, Some(_)) => (
            format!("eclipse-temurin:{java}-jdk"),
            "./mvnw -B -DskipTests package".to_string(),
        ),
        (Tool::Maven, None) => (
            format!("maven:3-eclipse-temurin-{java}"),
            "mvn -B -DskipTests package".to_string(),
        ),
        (Tool::Gradle, w) => {
            let task = if spring { "bootJar" } else { "assemble" };
            let gradle = if w.is_some() { "./gradlew" } else { "gradle" };
            let image = if w.is_some() {
                format!("eclipse-temurin:{java}-jdk")
            } else {
                format!("gradle:jdk{java}")
            };
            (image, format!("{gradle} --no-daemon {task}"))
        }
    };
    notes.push(match (tool, wrapper) {
        (Tool::Maven, Some(_)) => "found pom.xml and mvnw: Maven, through its wrapper".to_string(),
        (Tool::Maven, None) => "found pom.xml: Maven".to_string(),
        (Tool::Gradle, Some(_)) => format!("found {file} and gradlew: Gradle, through its wrapper"),
        (Tool::Gradle, None) => format!("found {file}: Gradle"),
    });
    let start = if spring {
        notes.push("Spring Boot: server.port comes from $PORT".into());
        "java $JAVA_OPTS -Dserver.port=$PORT -jar /app/app.jar"
    } else {
        "java $JAVA_OPTS -jar /app/app.jar"
    };
    notes.push(format!("the jar runs on eclipse-temurin:{java}-jre"));
    let out = match tool {
        Tool::Maven => "target",
        Tool::Gradle => "build/libs",
    };
    let cache = match tool {
        Tool::Maven => "/root/.m2",
        Tool::Gradle => "/root/.gradle",
    };
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::new();
        df.from(&image, Some("build"));
        df.line("WORKDIR /src");
        if tool == Tool::Gradle {
            df.env(&[("GRADLE_USER_HOME", "/root/.gradle")]);
        }
        df.copy_source();
        if let Some(w) = wrapper {
            df.line(&format!("RUN chmod +x {w}"));
        }
        if let Some(install) = c.install {
            df.run(&[cache], install);
        }
        // The app's jar: not the sources, javadoc, plain (Gradle) or original (Spring's repackage).
        let pick = format!(
            "mkdir -p /out && jar=$(find {out} -maxdepth 1 -name '*.jar' ! -name '*-sources.jar' \
             ! -name '*-javadoc.jar' ! -name '*-plain.jar' ! -name 'original-*' | head -n 1) && \
             test -n \"$jar\" && cp \"$jar\" /out/app.jar"
        );
        match c.build {
            Some(build) => df.run(&[cache], &format!("{build} && {pick}")),
            None => df.run(&[], &pick),
        }
        df.from(&format!("eclipse-temurin:{java}-jre"), None);
        df.line("RUN useradd --system --uid 10001 app");
        df.line("WORKDIR /app");
        df.line("COPY --from=build /out/app.jar /app/app.jar");
        df.env(&[("JAVA_OPTS", "-XX:MaxRAMPercentage=75")]);
        df.serve(Some("app"), c.start);
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Java,
        framework: spring.then_some("spring"),
        language_version: Some(java.to_string()),
        install: None,
        build: Some(build),
        start: Some(start.to_string()),
        notes,
        render,
    }))
}

/// The Java release a build file targets (`<java.version>17</java.version>`,
/// `JavaLanguageVersion.of(17)`, `sourceCompatibility = JavaVersion.VERSION_17`).
fn java_version(tool: Tool, file: &str) -> Option<u32> {
    let number = |s: &str| -> Option<u32> {
        let s = s.trim().trim_matches(['"', '\'']).trim();
        let s = s.strip_prefix("1.").unwrap_or(s);
        let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
        s[..end].parse().ok().filter(|v| (8..=40).contains(v))
    };
    match tool {
        Tool::Maven => [
            "java.version",
            "maven.compiler.release",
            "release",
            "maven.compiler.target",
            "maven.compiler.source",
        ]
        .iter()
        .find_map(|tag| {
            file.split(&format!("<{tag}>"))
                .nth(1)?
                .split('<')
                .next()
                .and_then(number)
        }),
        Tool::Gradle => {
            let after = |marker: &str| file.split(marker).nth(1).and_then(number);
            after("JavaLanguageVersion.of(").or_else(|| {
                after("JavaVersion.VERSION_").or_else(|| {
                    let rest = file.split("sourceCompatibility").nth(1)?;
                    after_eq(rest).and_then(number)
                })
            })
        }
    }
}

fn after_eq(s: &str) -> Option<&str> {
    let rest = s.trim_start().strip_prefix('=')?;
    rest.lines().next()
}
