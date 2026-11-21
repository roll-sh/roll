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
//! Other languages (Java's agent wants a pinned jar and its checksum) and the tree's own
//! Dockerfile are said and left alone.

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
        assert!(add(node, Provider::Go).is_err());
    }
}
