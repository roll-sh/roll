//! System packages for a generated Dockerfile (Railpack's `aptPkgs`, Heroku's Aptfile):
//! `build.packages` or roll.toml `[app] packages = ["ffmpeg"]`, installed in every stage that
//! starts from an image (a stage built on an earlier one has them already), with that image's
//! package manager: apt on Debian and Ubuntu images, apk on Alpine ones. An image without one
//! (distroless, busybox, scratch) can't take them: said, not ignored.

use crate::df::apt;

/// At most this many packages.
pub const MAX: usize = 50;

/// A package name as apt or apk takes it, with a version when pinned (`ffmpeg`,
/// `libvips42=8.14.1-3`, `g++`): only characters that mean nothing to a shell, so a name can't
/// be more than one argument.
pub fn name_ok(name: &str) -> bool {
    let b = name.as_bytes();
    !b.is_empty() && b.len() <= 100 && b[0].is_ascii_alphanumeric() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"+-.:=~_".contains(c))
}

/// The packages checked: their names, how many.
pub fn check(packages: &[String]) -> Result<(), String> {
    if packages.len() > MAX {
        return Err(format!("packages: at most {MAX}"));
    }
    match packages.iter().find(|p| !name_ok(p)) {
        Some(p) => Err(format!("packages: {p:?} isn't a package name (lowercase letters, digits and + - . : = ~ _)")),
        None => Ok(()),
    }
}

/// `dockerfile` with `packages` installed after each `FROM <image>` (and the `ARG` lines right
/// after it, so the layer sits before anything that changes more often).
pub fn add(dockerfile: &str, packages: &[String]) -> Result<String, String> {
    check(packages)?;
    if packages.is_empty() {
        return Ok(dockerfile.to_string());
    }
    let names: Vec<&str> = packages.iter().map(String::as_str).collect();
    let lines: Vec<&str> = dockerfile.lines().collect();
    let froms: Vec<usize> = lines.iter().enumerate().filter(|(_, l)| l.trim_start().to_ascii_uppercase().starts_with("FROM ")).map(|(i, _)| i).collect();
    let mut stages: Vec<String> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        out.push(lines[i].to_string());
        if let Some(n) = froms.iter().position(|f| *f == i) {
            let words: Vec<&str> = lines[i].split_whitespace().collect();
            let image = words.get(1).copied().unwrap_or("");
            if let (Some(as_), Some(name)) = (words.get(2), words.get(3))
                && as_.eq_ignore_ascii_case("as")
            {
                stages.push(name.to_string());
            }
            let last = n == froms.len() - 1;
            let install = if stages.iter().any(|s| s == image) {
                None
            } else {
                let base = image.to_ascii_lowercase();
                if base.contains("alpine") {
                    Some(format!("RUN apk add --no-cache {}", names.join(" ")))
                } else if base.contains("distroless") || base.starts_with("busybox") || base == "scratch" {
                    if last {
                        return Err(format!("packages need an image with a package manager: this build runs on {image} (a Dockerfile can use another)"));
                    }
                    None
                } else {
                    Some(format!("RUN {}", apt(&names)))
                }
            };
            while i + 1 < lines.len() && lines[i + 1].trim_start().starts_with("ARG ") {
                i += 1;
                out.push(lines[i].to_string());
            }
            if let Some(r) = install {
                out.push(r);
            }
        }
        i += 1;
    }
    let mut text = out.join("\n");
    if dockerfile.ends_with('\n') {
        text.push('\n');
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_image_stage_installs_them_with_its_manager() {
        let df = "# syntax=docker/dockerfile:1\n\nFROM node:22-slim AS base\nARG ROLL_BUILD_ENV\nWORKDIR /app\n\nFROM base AS build\nRUN npm ci\n\nFROM node:22-alpine\nCOPY --from=build /app /app\n";
        let out = add(df, &["ffmpeg".into(), "libvips42".into()]).unwrap();
        assert!(out.contains("FROM node:22-slim AS base\nARG ROLL_BUILD_ENV\nRUN apt-get update && apt-get install -y --no-install-recommends ffmpeg libvips42"), "{out}");
        assert!(out.contains("FROM base AS build\nRUN npm ci"), "a stage on an earlier one has them: {out}");
        assert!(out.contains("FROM node:22-alpine\nRUN apk add --no-cache ffmpeg libvips42\n"), "{out}");
        assert!(add("FROM golang:1.23 AS build\nFROM gcr.io/distroless/static-debian12:nonroot\n", &["ffmpeg".into()]).unwrap_err().contains("distroless"));
        assert!(check(&["ffmpeg; curl evil".into()]).is_err() && check(&["$(id)".into()]).is_err() && check(&["g++".into(), "libssl3=3.0.15-1".into()]).is_ok());
    }
}
