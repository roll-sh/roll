//! Swift (Nixpacks' Swift; Vapor's and Hummingbird's own Dockerfiles): `swift build -c release`
//! on the Swift image with its standard library linked in statically, the executable and its
//! resource bundles run on Debian slim. Vapor's is started with `serve` on `$PORT`; another
//! executable is started as it is and reads `PORT` itself.

use crate::df::Df;
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, text};

/// The Swift releases built, by `swift-tools-version`'s major.minor; others build on the default.
const VERSIONS: [&str; 5] = ["6.0", "6.1", "6.2", "6.3", "6.4"];
const DEFAULT: &str = "6.2";
const RUN: &str = "debian:bookworm-slim";

/// The first `.executableTarget(name: "…")` in a manifest.
fn executable(manifest: &str) -> Option<String> {
    let at = manifest.find(".executableTarget(")?;
    let rest = &manifest[at..];
    let name = rest.find("name:")?;
    let quoted = rest[name + 5..].trim_start().strip_prefix('"')?;
    let end = quoted.find('"')?;
    let n = &quoted[..end];
    (!n.is_empty() && n.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')).then(|| n.to_string())
}

pub(crate) fn detect(tree: &dyn Tree, _o: &Overrides) -> Result<Option<Draft>, Error> {
    let Some(manifest) = text(tree, "Package.swift") else {
        return Ok(None);
    };
    let Some(name) = executable(&manifest) else {
        return Err(Error::NoBinary("Package.swift has no .executableTarget to run: roll builds a Swift package's executable (add one, or a Dockerfile)".into()));
    };
    let tools = manifest.lines().next().and_then(|l| l.split("swift-tools-version:").nth(1)).map(|v| v.trim().to_string());
    let version = tools.as_deref().and_then(|t| VERSIONS.iter().find(|v| t == **v || t.starts_with(&format!("{v}.")))).copied().unwrap_or(DEFAULT);
    let vapor = manifest.contains("vapor/vapor");
    let mut notes = vec![format!("found Package.swift: Swift {version}, the executable {name}")];
    let start = if vapor {
        notes.push("vapor/vapor in Package.swift: Vapor, served on $PORT".into());
        format!("/app/{name} serve --env production --hostname 0.0.0.0 --port $PORT")
    } else {
        notes.push(format!("started as /app/{name}: it reads PORT itself"));
        format!("/app/{name}")
    };
    let image = format!("swift:{version}-bookworm");
    let manifests: Vec<String> = ["Package.swift", "Package.resolved"].into_iter().filter(|f| tree.exists(f)).map(String::from).collect();
    let (bin, public, resources) = (name.clone(), tree.exists("Public"), tree.exists("Resources"));
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::new(c);
        df.from(&image, Some("build"));
        df.line("WORKDIR /src");
        df.source(Some(&manifests), &[], c);
        if let Some(build) = c.build {
            df.run(&[], build);
        }
        // The executable and its resource bundles out of the build directory.
        df.run(&[], &format!("mkdir -p /out && cp \"$(swift build -c release --show-bin-path)/{bin}\" /out/ && find \"$(swift build -c release --show-bin-path)\" -maxdepth 1 -name '*.resources' -exec cp -R {{}} /out/ \\;"));
        df.from(RUN, None);
        df.run(&[], "apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata && rm -rf /var/lib/apt/lists/* && useradd --system --uid 10001 app");
        df.line("WORKDIR /app");
        df.line("COPY --from=build --chown=app /out/ /app/");
        // Vapor's templates and public files, where it reads them (its working directory).
        if public {
            df.line("COPY --from=build --chown=app /src/Public /app/Public");
        }
        if resources {
            df.line("COPY --from=build --chown=app /src/Resources /app/Resources");
        }
        df.serve(Some("app"), c.start);
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Swift,
        framework: vapor.then_some("vapor"),
        language_version: Some(version.into()),
        install: Some("swift package resolve".into()),
        build: Some("swift build -c release --static-swift-stdlib".into()),
        start: Some(start),
        notes,
        render,
    }))
}
