//! OpenTelemetry with no code (Heroku's Telemetry, `@vercel/otel`, the OpenTelemetry Operator's
//! injection): `build.otel` adds the language's zero-code agent to a generated Dockerfile, pinned,
//! and the plan says which (`Plan::otel`). It does nothing on its own: apps-control turns it on
//! at start only while the environment's traces are on, beside the `OTEL_*` variables it sets.
//!
//! - Node: `@opentelemetry/auto-instrumentations-node` in `/opt/roll-otel`, outside the app's
//!   own `node_modules`, started by `NODE_OPTIONS=--require …/register.js` ([`NODE_REQUIRE`]).
//! - Python: `opentelemetry-distro` and the OTLP exporter in the app's own virtualenv, with
//!   `opentelemetry-bootstrap` adding the instrumentations of the libraries it has, and its
//!   `auto_instrumentation` directory linked at [`PYTHON_PATH`], whose `sitecustomize` starts it
//!   when that's on `PYTHONPATH` (what `opentelemetry-instrument` does).
//!
//! - Java, Scala and Clojure: OpenTelemetry's Java agent jar, fetched by the Dockerfile from Maven
//!   Central and checked against its SHA-256 ([`JAVA_AGENT_SHA256`]), at [`JAVA_AGENT`], started
//!   by `JAVA_TOOL_OPTIONS=-javaagent:…`.
//! - .NET (8 and later, on the glibc runtime images roll builds on): OpenTelemetry's .NET
//!   automatic instrumentation, its release zip for the build's architecture checked against its
//!   SHA-256 in a stage of its own and copied to [`DOTNET_HOME`]; started by the runtime's
//!   profiler variables and its startup hook ([`DOTNET_VARS`], [`DOTNET_HOOK`]).
//!
//! Other languages and the tree's own Dockerfile are said and left alone.

use crate::Provider;

/// The Node packages, pinned.
pub const NODE_PACKAGES: &str = "@opentelemetry/auto-instrumentations-node@0.52.0 @opentelemetry/api@1.9.0";
/// What `NODE_OPTIONS` requires to start it.
pub const NODE_REQUIRE: &str = "/opt/roll-otel/node_modules/@opentelemetry/auto-instrumentations-node/build/src/register.js";
/// The Python packages, pinned.
/// (setuptools: its bootstrap reads `pkg_resources`, which a new virtualenv no longer has.)
pub const PYTHON_PACKAGES: &str = "opentelemetry-distro==0.48b0 opentelemetry-exporter-otlp==1.27.0 setuptools==75.8.0";
/// Where its `auto_instrumentation` directory is linked (in the virtualenv the image copies).
pub const PYTHON_PATH: &str = "/opt/venv/roll-otel";
/// The Java agent, pinned: its URL (Maven Central's) and SHA-256 (Maven Central's too).
pub const JAVA_AGENT_URL: &str = "https://repo1.maven.org/maven2/io/opentelemetry/javaagent/opentelemetry-javaagent/2.33.1/opentelemetry-javaagent-2.33.1.jar";
pub const JAVA_AGENT_SHA256: &str = "e43a270df53c6f423e0ab157be9c27ccc01d5dc3232e162fe54777573db2f149";
/// Where it is in the image that runs.
pub const JAVA_AGENT: &str = "/opt/roll-otel/opentelemetry-javaagent.jar";

/// .NET's automatic instrumentation, pinned: its release and each architecture's zip's SHA-256
/// (by Docker's `TARGETARCH`; the release's own name for it).
pub const DOTNET_VERSION: &str = "1.19.1";
const DOTNET_ARCHS: [(&str, &str, &str); 2] = [
    ("amd64", "x64", "fe4ddaf4c54b960dbb150db7cc4531c06598f2c1a70e56b3a8c13c7118e4df58"),
    ("arm64", "arm64", "0bb2e2ac37c68058addfb96d7955a85bc639db202b1e70991ca73f70ae39d70e"),
];
/// Where it is in the image that runs.
pub const DOTNET_HOME: &str = "/opt/roll-otel/dotnet";
/// The runtime's profiler variables that load its native part (set only when the app names no
/// profiler of its own: the runtime loads one).
pub const DOTNET_VARS: [(&str, &str); 4] = [
    ("CORECLR_ENABLE_PROFILING", "1"),
    ("CORECLR_PROFILER", "{918728DD-259F-4A6A-AC2B-B85E1B658318}"),
    ("CORECLR_PROFILER_PATH", "/opt/roll-otel/dotnet/native/OpenTelemetry.AutoInstrumentation.Native.so"),
    ("OTEL_DOTNET_AUTO_HOME", DOTNET_HOME),
];
/// Its startup hook, joined to the app's own `DOTNET_STARTUP_HOOKS`.
pub const DOTNET_HOOK: &str = "/opt/roll-otel/dotnet/net/OpenTelemetry.AutoInstrumentation.StartupHook.dll";

/// The stage that fetches .NET's: the architecture's zip, checked, unpacked, its native part at
/// one path whatever the architecture, debug symbols dropped.
fn dotnet_stage() -> String {
    let cases: String = DOTNET_ARCHS.iter().map(|(docker, arch, sha)| format!("{docker}) a={arch}; s={sha};; ")).collect();
    format!(
        "FROM debian:bookworm-slim AS roll-otel\nARG TARGETARCH\nRUN set -eu; case \"$TARGETARCH\" in {cases}*) echo \"no OpenTelemetry .NET for $TARGETARCH\"; exit 1;; esac \\\n    && apt-get update && apt-get install -y --no-install-recommends ca-certificates curl unzip && rm -rf /var/lib/apt/lists/* \\\n    && curl -fsSL -o /tmp/o.zip \"https://github.com/open-telemetry/opentelemetry-dotnet-instrumentation/releases/download/v{DOTNET_VERSION}/opentelemetry-dotnet-instrumentation-linux-glibc-$a.zip\" \\\n    && echo \"$s  /tmp/o.zip\" | sha256sum -c - \\\n    && mkdir -p {DOTNET_HOME} && unzip -q /tmp/o.zip -d {DOTNET_HOME} && rm /tmp/o.zip \\\n    && mv {DOTNET_HOME}/linux-$a {DOTNET_HOME}/native \\\n    && find {DOTNET_HOME} \\( -name '*.debug' -o -name '*.pdb' \\) -delete"
    )
}

/// `dockerfile` with the agent installed, and which one; or why not.
pub fn add(dockerfile: &str, provider: Provider) -> Result<(String, &'static str), String> {
    let lines: Vec<&str> = dockerfile.lines().collect();
    let last_from = lines.iter().rposition(|l| l.trim_start().to_ascii_uppercase().starts_with("FROM ")).ok_or("no FROM")?;
    let (at, step, kind) = match provider {
        // In the image that runs, as root, before it drops to the app's user.
        Provider::Node => (last_from + 1, format!("RUN npm install --prefix /opt/roll-otel --no-audit --no-fund --omit=dev {NODE_PACKAGES}"), "node"),
        // In the build stage's virtualenv (the image copies /opt/venv), after its dependencies,
        // so bootstrap sees the libraries to instrument.
        Provider::Python => (
            last_from,
            // A uv-made virtualenv has no pip: uv installs into it then.
            format!(
                "RUN if /opt/venv/bin/python -m pip --version >/dev/null 2>&1; then P=\"/opt/venv/bin/python -m pip install --no-cache-dir\"; else P=\"uv pip install --python /opt/venv/bin/python\"; fi \\\n    && $P {PYTHON_PACKAGES} \\\n    && R=\"$(/opt/venv/bin/opentelemetry-bootstrap -a requirements)\" && {{ [ -z \"$R\" ] || $P $R; }} \\\n    && ln -s \"$(/opt/venv/bin/python -c 'import os, opentelemetry.instrumentation.auto_instrumentation as m; print(os.path.dirname(m.__file__))')\" {PYTHON_PATH}"
            ),
            "python",
        ),
        // In the image that runs, checked before it's kept. 755: BuildKit gives the directory it
        // makes the same mode, and the app's user must get through it.
        Provider::Java | Provider::Scala | Provider::Clojure => (last_from + 1, format!("ADD --chmod=755 --checksum=sha256:{JAVA_AGENT_SHA256} {JAVA_AGENT_URL} {JAVA_AGENT}"), "java"),
        // A stage of its own before the image that runs (which has no curl or unzip), copied in.
        Provider::Dotnet => {
            let image = lines[last_from].split_whitespace().nth(1).unwrap_or("");
            let major = image.rsplit_once(':').and_then(|(_, t)| t.split('.').next()).and_then(|m| m.parse::<u32>().ok());
            if image.contains("alpine") || !major.is_some_and(|m| m >= 8) {
                return Err(format!("otel: OpenTelemetry's .NET instrumentation runs on .NET 8 and later on glibc, not {image}"));
            }
            let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
            out.insert(last_from + 1, format!("COPY --from=roll-otel {DOTNET_HOME} {DOTNET_HOME}"));
            out.insert(last_from, dotnet_stage());
            let mut text = out.join("\n");
            if dockerfile.ends_with('\n') {
                text.push('\n');
            }
            return Ok((text, "dotnet"));
        }
        other => return Err(format!("otel: no zero-code agent for {other:?} yet (an SDK of its own still sends to roll)")),
    };
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    out.insert(at, step);
    let mut text = out.join("\n");
    if dockerfile.ends_with('\n') {
        text.push('\n');
    }
    Ok((text, kind))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_agent_goes_where_the_language_runs_it() {
        let node = "FROM node:22 AS build\nRUN npm ci\nFROM node:22-slim\nWORKDIR /app\nUSER app\nCMD [\"node\", \"a.js\"]\n";
        let (df, kind) = add(node, Provider::Node).unwrap();
        assert_eq!(kind, "node");
        assert!(df.contains("FROM node:22-slim\nRUN npm install --prefix /opt/roll-otel"), "{df}");
        let py = "FROM python:3.12 AS build\nRUN pip install -r requirements.txt\nFROM python:3.12-slim\nCOPY --from=build /opt/venv /opt/venv\n";
        let (df, kind) = add(py, Provider::Python).unwrap();
        assert_eq!(kind, "python");
        assert!(df.contains("requirements.txt\nRUN if /opt/venv/bin/python -m pip"), "{df}");
        let jvm = "FROM maven:3-eclipse-temurin-21 AS build\nRUN mvn package\nFROM eclipse-temurin:21-jre\nCOPY --from=build /app/target/app.jar /app/app.jar\n";
        let (df, kind) = add(jvm, Provider::Java).unwrap();
        assert_eq!(kind, "java");
        assert!(df.contains(&format!("FROM eclipse-temurin:21-jre\nADD --chmod=755 --checksum=sha256:{JAVA_AGENT_SHA256} ")), "{df}");
        let net = "FROM mcr.microsoft.com/dotnet/sdk:9.0 AS build\nRUN dotnet publish\nFROM mcr.microsoft.com/dotnet/aspnet:9.0\nWORKDIR /app\nCOPY --from=build /out ./\n";
        let (df, kind) = add(net, Provider::Dotnet).unwrap();
        assert_eq!(kind, "dotnet");
        assert!(df.contains("RUN dotnet publish\nFROM debian:bookworm-slim AS roll-otel\n"), "{df}");
        assert!(df.contains(&format!("FROM mcr.microsoft.com/dotnet/aspnet:9.0\nCOPY --from=roll-otel {DOTNET_HOME} {DOTNET_HOME}\nWORKDIR /app")), "{df}");
        assert!(add(&net.replace("aspnet:9.0", "aspnet:6.0"), Provider::Dotnet).is_err());
        assert!(add(node, Provider::Go).is_err());
    }
}
