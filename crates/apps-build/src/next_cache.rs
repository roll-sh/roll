//! Next.js's cache across instances (docs/apps/README.md, "Next.js across instances"): a Next
//! build's generated Dockerfile gets roll's adapter in [`DIR`] and `NEXT_ADAPTER_PATH` in every
//! stage, so `next build` and `next start` (Next 16 and later) copy roll's handlers into the
//! project's `.roll-next` (Next takes a handler only from inside the project) and point its
//! `cacheHandler` and `cacheHandlers` at them, and the plan says so (`Plan::next_cache`): apps-control then
//! gives its instances the cache's address and token. The handlers are plain Node (no packages)
//! and do nothing without those variables. The tree's own Dockerfile is left alone.

use serde_json::Value;

/// Where the adapter and handlers are, in every stage.
pub const DIR: &str = "/opt/roll-next";

const FILES: [(&str, &str); 4] = [
    ("adapter.js", include_str!("next_cache/adapter.js")),
    ("client.js", include_str!("next_cache/client.js")),
    ("incremental.js", include_str!("next_cache/incremental.js")),
    ("use-cache.js", include_str!("next_cache/use-cache.js")),
];

/// Whether package.json's `next` is one with adapters (16 and later; a tag such as `latest` or
/// `canary` counts).
pub fn adapters(pkg: &Value) -> Result<(), String> {
    let spec = ["dependencies", "devDependencies"].iter().find_map(|d| pkg[d]["next"].as_str()).unwrap_or("");
    let major = spec.trim_start_matches(|c: char| !c.is_ascii_digit()).split(|c: char| !c.is_ascii_digit()).next().and_then(|m| m.parse::<u32>().ok());
    match major {
        Some(m) if m < 16 => Err(format!("next {spec}: Next 16 or later shares its cache across instances (adapters); this one keeps it per instance")),
        _ => Ok(()),
    }
}

/// `dockerfile` with the adapter in every stage.
pub fn add(dockerfile: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD;
    let files = FILES.iter().map(|(name, text)| format!("echo {} | base64 -d > {DIR}/{name}", b64.encode(text))).collect::<Vec<_>>().join(" \\\n    && ");
    let steps = format!("RUN mkdir -p {DIR} \\\n    && {files}\nENV NEXT_ADAPTER_PATH={DIR}/adapter.js");
    let mut out = Vec::new();
    for line in dockerfile.lines() {
        out.push(line.to_string());
        if line.trim_start().to_ascii_uppercase().starts_with("FROM ") {
            out.push(steps.clone());
        }
    }
    let mut text = out.join("\n");
    if dockerfile.ends_with('\n') {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    #[test]
    fn every_stage_gets_the_adapter_and_old_next_says_why_not() {
        let df = super::add("FROM node:22-slim AS build\nRUN npm ci\nFROM node:22-slim\nCMD [\"node\", \"server.js\"]\n");
        assert_eq!(df.matches("ENV NEXT_ADAPTER_PATH=/opt/roll-next/adapter.js").count(), 2, "{df}");
        assert!(df.ends_with("CMD [\"node\", \"server.js\"]\n"));
        assert!(super::adapters(&json!({"dependencies": {"next": "^16.2.0"}})).is_ok());
        assert!(super::adapters(&json!({"dependencies": {"next": "canary"}})).is_ok());
        assert!(super::adapters(&json!({"dependencies": {"next": "15.3.1"}})).unwrap_err().contains("Next 16"));
    }
}
