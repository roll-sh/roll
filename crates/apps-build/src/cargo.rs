//! Rust: `cargo build --release` with BuildKit cache mounts for the registry and the target
//! dir, and the one binary on debian slim.

use crate::df::{Df, apt};
use crate::{Commands, Draft, Error, Overrides, Provider, Tree, invalid, join, text};

/// The build and runtime images share a Debian release, so the binary's glibc matches.
const BUILD: &str = "slim-bookworm";
const RUNTIME: &str = "debian:bookworm-slim";
/// Workspace members looked at, at most.
const MAX_MEMBERS: usize = 200;

/// A binary target: its package, and whether that package is a workspace member.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Bin {
    package: String,
    name: String,
    member: bool,
}

pub(crate) fn detect(tree: &dyn Tree, o: &Overrides) -> Result<Option<Draft>, Error> {
    let Some(manifest) = text(tree, "Cargo.toml") else {
        return Ok(None);
    };
    let root: toml::Table = toml::from_str(&manifest).map_err(|e| invalid("Cargo.toml", e))?;
    let mut notes = Vec::new();

    let mut bins = package_bins(tree, "", &root, false);
    let default_run = root
        .get("package")
        .and_then(|p| p.get("default-run"))
        .and_then(|d| d.as_str());
    if bins.is_empty()
        && let Some(ws) = root.get("workspace")
    {
        for dir in members(tree, ws) {
            let Some(text) = text(tree, &join(&dir, "Cargo.toml")) else {
                continue;
            };
            let Ok(member) = toml::from_str::<toml::Table>(&text) else {
                return Err(invalid(&join(&dir, "Cargo.toml"), "it isn't valid TOML"));
            };
            bins.extend(package_bins(tree, &dir, &member, true));
        }
    }

    // A build override that names one `--bin` keeps the slim runtime.
    let named = o.build.as_deref().and_then(|b| flag(b, "--bin"));
    let chosen = match (named, bins.as_slice()) {
        (Some(name), _) => {
            notes.push(format!("build names --bin {name}: deploying {name}"));
            Some(Bin {
                package: String::new(),
                name: name.to_string(),
                member: false,
            })
        }
        (None, [one]) => {
            notes.push(if one.member && one.name != one.package {
                format!(
                    "the workspace has one binary: {} in {}",
                    one.name, one.package
                )
            } else if one.member {
                format!("the workspace has one binary: {}", one.name)
            } else {
                format!("Cargo.toml builds one binary: {}", one.name)
            });
            Some(one.clone())
        }
        (None, many)
            if !many.is_empty()
                && default_run.is_some_and(|d| many.iter().any(|b| b.name == d)) =>
        {
            let d = default_run.unwrap_or_default();
            notes.push(format!("default-run is {d}: deploying it"));
            many.iter().find(|b| b.name == d).cloned()
        }
        (None, []) if o.build.is_none() => {
            return Err(Error::NoBinary(
                "Cargo.toml has no binary target (src/main.rs, src/bin/ or [[bin]]): roll deploys a binary"
                    .into(),
            ));
        }
        (None, many) if o.build.is_none() => {
            let list: Vec<String> = many.iter().take(10).map(|b| b.name.clone()).collect();
            let first = &many[0];
            let p = if first.member {
                format!(" -p {}", first.package)
            } else {
                String::new()
            };
            return Err(Error::Ambiguous(format!(
                "Cargo.toml builds {} binaries ({}{}): set build to the one to deploy, e.g. \
                 `cargo build --release{p} --bin {}`",
                many.len(),
                list.join(", "),
                if many.len() > 10 { ", …" } else { "" },
                first.name
            )));
        }
        _ => None,
    };

    let (image_tag, version) = toolchain(tree, &mut notes);
    let locked = tree.exists("Cargo.lock");
    let openssl = text(tree, "Cargo.lock").is_some_and(|l| l.contains("name = \"openssl-sys\""));
    if openssl {
        notes.push("openssl-sys in Cargo.lock: installing libssl".into());
    }
    let build = chosen.as_ref().map(|b| {
        let mut cmd = String::from("cargo build --release");
        if locked {
            cmd.push_str(" --locked");
        }
        if b.member {
            cmd.push_str(&format!(" -p {}", b.package));
        }
        cmd.push_str(&format!(" --bin {}", b.name));
        cmd
    });
    let keep_toolchain = o.build.is_some() && named.is_none();
    let bin = chosen.map(|b| b.name);
    let start = if keep_toolchain {
        notes.push(
            "build is set: the image keeps the Rust toolchain and start decides what runs".into(),
        );
        None
    } else {
        bin.clone()
    };
    if let Some(b) = &bin
        && !keep_toolchain
    {
        notes.push(format!("{b} runs on {RUNTIME}"));
    }
    let render = Box::new(move |c: &Commands<'_>| {
        let mut df = Df::new(c);
        df.from(&format!("rust:{image_tag}-{BUILD}"), Some("build"));
        if openssl {
            df.run(&[], &apt(&["pkg-config", "libssl-dev"]));
        }
        df.line("WORKDIR /src");
        df.env(&[("CARGO_TARGET_DIR", "/target")]);
        df.copy_source();
        let caches = [
            "/usr/local/cargo/registry",
            "/usr/local/cargo/git",
            "/target",
        ];
        if let Some(install) = c.install {
            df.run(&caches, install);
        }
        if keep_toolchain {
            if let Some(build) = c.build {
                // The target dir is a cache mount: a custom build puts what it keeps elsewhere.
                df.run(&caches[..2], build);
            }
            df.serve(None, c.start);
            return df.finish();
        }
        if let (Some(build), Some(bin)) = (c.build, &bin) {
            df.run(
                &caches,
                &format!("{build} && mkdir -p /out && cp /target/release/{bin} /out/"),
            );
        }
        df.from(RUNTIME, None);
        let mut packages = vec!["ca-certificates"];
        if openssl {
            packages.push("libssl3");
        }
        df.run(&[], &apt(&packages));
        df.line("RUN useradd --system --uid 10001 --no-create-home app");
        if let Some(bin) = &bin {
            df.line(&format!(
                "COPY --from=build /out/{bin} /usr/local/bin/{bin}"
            ));
        }
        df.serve(Some("app"), c.start);
        df.finish()
    });
    Ok(Some(Draft {
        provider: Provider::Rust,
        framework: None,
        language_version: Some(version),
        install: None,
        build,
        start,
        notes,
        render,
    }))
}

/// The value after `flag` in a command line (`--bin api` or `--bin=api`).
fn flag<'a>(cmd: &'a str, flag: &str) -> Option<&'a str> {
    let mut words = cmd.split_whitespace();
    while let Some(w) = words.next() {
        if w == flag {
            return words.next();
        }
        if let Some(v) = w.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
            return Some(v);
        }
    }
    None
}

/// The binary targets of the package in `dir` (Cargo's target auto-discovery, plus `[[bin]]`).
fn package_bins(tree: &dyn Tree, dir: &str, manifest: &toml::Table, member: bool) -> Vec<Bin> {
    let Some(package) = manifest.get("package") else {
        return Vec::new();
    };
    let Some(pkg) = package.get("name").and_then(|n| n.as_str()) else {
        return Vec::new();
    };
    let mut names: Vec<String> = Vec::new();
    let explicit = manifest
        .get("bin")
        .and_then(|b| b.as_array())
        .cloned()
        .unwrap_or_default();
    for bin in &explicit {
        if let Some(name) = bin.get("name").and_then(|n| n.as_str()) {
            names.push(name.to_string());
        }
    }
    let autobins = package
        .get("autobins")
        .and_then(|a| a.as_bool())
        .unwrap_or(true);
    if autobins {
        if tree.read(&join(dir, "src/main.rs")).is_some() {
            names.push(pkg.to_string());
        }
        for entry in tree.list(&join(dir, "src/bin")) {
            if let Some(name) = entry.strip_suffix(".rs") {
                names.push(name.to_string());
            } else if let Some(sub) = entry.strip_suffix('/')
                && tree
                    .read(&join(dir, &format!("src/bin/{sub}/main.rs")))
                    .is_some()
            {
                names.push(sub.to_string());
            }
        }
    }
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| Bin {
            package: pkg.to_string(),
            name,
            member,
        })
        .collect()
}

/// The workspace's member directories: exact paths and `dir/*` globs.
fn members(tree: &dyn Tree, ws: &toml::Value) -> Vec<String> {
    let list = |key: &str| -> Vec<String> {
        ws.get(key)
            .and_then(|m| m.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|m| m.as_str())
                    .map(|m| m.trim_end_matches('/').to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    let exclude = list("exclude");
    let mut dirs = Vec::new();
    for member in list("members") {
        match member.strip_suffix("/*") {
            Some(parent) => dirs.extend(
                tree.list(parent)
                    .into_iter()
                    .filter_map(|d| d.strip_suffix('/').map(|d| join(parent, d))),
            ),
            None if member == "*" => dirs.extend(
                tree.list("")
                    .into_iter()
                    .filter_map(|d| d.strip_suffix('/').map(String::from)),
            ),
            None if !member.contains(['*', '?', '[']) => dirs.push(member),
            None => {}
        }
    }
    dirs.retain(|d| !exclude.contains(d) && crate::tree::valid(d));
    dirs.truncate(MAX_MEMBERS);
    dirs
}

/// The `rust:` image tag and the version to show, from `rust-toolchain.toml` or `rust-toolchain`.
fn toolchain(tree: &dyn Tree, notes: &mut Vec<String>) -> (String, String) {
    let channel = text(tree, "rust-toolchain.toml")
        .and_then(|t| toml::from_str::<toml::Table>(&t).ok())
        .and_then(|t| {
            t.get("toolchain")?
                .get("channel")?
                .as_str()
                .map(String::from)
        })
        .or_else(|| {
            text(tree, "rust-toolchain").and_then(|t| {
                let t = t.trim();
                (!t.is_empty() && !t.contains('[')).then(|| t.to_string())
            })
        });
    match channel {
        Some(c)
            if c.chars().next().is_some_and(|c| c.is_ascii_digit())
                && c.chars().all(|c| c.is_ascii_digit() || c == '.') =>
        {
            notes.push(format!("the toolchain is {c}: rust:{c}"));
            (c.clone(), c)
        }
        Some(c) => {
            notes.push(format!(
                "the toolchain is {c}: rustup installs it from the stable image"
            ));
            ("1".into(), c)
        }
        None => {
            notes.push("no toolchain pinned: the latest stable Rust".into());
            ("1".into(), "stable".into())
        }
    }
}
