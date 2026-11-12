//! Go: a static binary (`CGO_ENABLED=0`) on distroless.

use crate::df::{Df, json};
use crate::version;
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, text};

const RUNTIME: &str = "gcr.io/distroless/static-debian12:nonroot";
/// The same image with busybox, for a start command that needs a shell.
const RUNTIME_SHELL: &str = "gcr.io/distroless/static-debian12:debug-nonroot";
const BINARY: &str = "/app/server";

pub(crate) fn detect(tree: &dyn Tree, o: &Overrides) -> Result<Option<Draft>, Error> {
    let Some(gomod) = text(tree, "go.mod") else {
        return Ok(None);
    };
    let mut notes = Vec::new();
    let directive = gomod
        .lines()
        .find_map(|l| l.trim().strip_prefix("go ").map(str::trim));
    let go = match directive.and_then(version::parse) {
        Some(v) => {
            notes.push(format!(
                "go.mod says go {}: golang:{}",
                directive.unwrap_or(""),
                v.minor()
            ));
            v.minor()
        }
        None => {
            notes.push("go.mod has no go directive: the latest Go".into());
            "1".into()
        }
    };

    let root_main = tree.list("").iter().any(|f| {
        f.ends_with(".go")
            && !f.ends_with("_test.go")
            && text(tree, f).is_some_and(|src| src.lines().any(|l| l.trim() == "package main"))
    });
    let package = if root_main {
        notes.push("package main at the root: building .".into());
        Some(".".to_string())
    } else {
        let cmds: Vec<String> = tree
            .list("cmd")
            .into_iter()
            .filter_map(|d| d.strip_suffix('/').map(String::from))
            .collect();
        match cmds.as_slice() {
            [one] => {
                notes.push(format!(
                    "no main package at the root, one in cmd/: building ./cmd/{one}"
                ));
                Some(format!("./cmd/{one}"))
            }
            [] if o.build.is_none() => {
                return Err(Error::NoBinary(
                    "go.mod has no main package at the root or in cmd/<name>: set build".into(),
                ));
            }
            many if o.build.is_none() => {
                return Err(Error::Ambiguous(format!(
                    "cmd/ has {} programs ({}): set build to the one to deploy, e.g. \
                     `go build -o /out/server ./cmd/{}`",
                    many.len(),
                    many.join(", "),
                    many[0]
                )));
            }
            _ => None,
        }
    };
    let vendored = tree.exists("vendor/modules.txt");
    let install = (!vendored).then(|| "go mod download".to_string());
    if vendored {
        notes.push("found vendor/: building from it".into());
    }
    let build =
        package.map(|p| format!("go build -trimpath -ldflags=\"-s -w\" -o /out/server {p}"));
    // A build override that writes /out/server keeps the distroless runtime.
    let keep_toolchain = o
        .build
        .as_deref()
        .is_some_and(|b| !b.contains("/out/server"));
    let start = if keep_toolchain {
        notes.push(
            "build is set: the image keeps the Go toolchain and start decides what runs".into(),
        );
        None
    } else {
        notes.push(format!("CGO is off: a static binary on {RUNTIME}"));
        Some(BINARY.to_string())
    };
    let manifests: Vec<String> = ["go.mod", "go.sum"]
        .into_iter()
        .filter(|f| tree.exists(f))
        .map(String::from)
        .collect();
    let language_version = Some(go.clone());
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::new(c);
        df.from(&format!("golang:{go}"), Some("build"));
        df.line("WORKDIR /src");
        df.env(&[("CGO_ENABLED", "0")]);
        let caches = ["/go/pkg/mod", "/root/.cache/go-build"];
        df.source(Some(&manifests), &caches[..1], c);
        if keep_toolchain {
            if let Some(build) = c.build {
                df.run(&caches, build);
            }
            df.serve(None, c.start);
            return df.finish();
        }
        if let Some(build) = c.build {
            df.run(&caches, build);
        }
        df.from(
            if c.start == Some(BINARY) {
                RUNTIME
            } else {
                RUNTIME_SHELL
            },
            None,
        );
        df.line(&format!("COPY --from=build /out/server {BINARY}"));
        df.serve(Some("nonroot:nonroot"), None);
        match c.start {
            Some(BINARY) => df.line(&format!("CMD [{}]", json(BINARY))),
            Some(start) => df.line(&format!("CMD [\"/busybox/sh\", \"-c\", {}]", json(start))),
            None => {}
        }
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Go,
        framework: None,
        language_version,
        install,
        build,
        start,
        notes,
        render,
    }))
}
