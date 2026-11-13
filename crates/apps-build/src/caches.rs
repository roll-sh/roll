//! BuildKit's cache mounts (`RUN --mount=type=cache,…`) belong to the builder and are keyed by
//! their `id`, which is the target path when none is given. On a node that builds many services,
//! two services' `/root/.npm` would be one directory, and one build could leave files there that
//! another reads. Every cache mount of a Dockerfile, the generated one or the tree's own, gets
//! the service's namespace (a secret, so no other Dockerfile can name it) in its id.

/// `dockerfile` with every cache mount's id under `ns`: `id=<ns>-<its id or target>`.
pub fn isolate_caches(dockerfile: &str, ns: &str) -> String {
    let mut out = String::with_capacity(dockerfile.len() + 64);
    let mut rest = dockerfile;
    while let Some(i) = rest.find("--mount=") {
        out.push_str(&rest[..i + 8]);
        rest = &rest[i + 8..];
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        out.push_str(&isolate(&rest[..end], ns));
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

fn isolate(flag: &str, ns: &str) -> String {
    // Quoted, the flag's value (`--mount="…"`) or the whole flag (`"--mount=…"`).
    let lead = if flag.starts_with('"') { "\"" } else { "" };
    let tail = if flag.len() > lead.len() && flag.ends_with('"') { "\"" } else { "" };
    let body = &flag[lead.len()..flag.len() - tail.len()];
    let parts: Vec<&str> = body.split(',').collect();
    let named = |p: &str, k: &str| p.split_once('=').is_some_and(|(a, _)| a.trim().eq_ignore_ascii_case(k));
    let value = |k: &str| parts.iter().find(|p| named(p, k)).and_then(|p| p.split_once('=')).map(|(_, v)| v);
    if !value("type").is_some_and(|t| t.eq_ignore_ascii_case("cache")) {
        return flag.to_string();
    }
    let Some(base) = value("id").or(value("target")).or(value("dst")).or(value("destination")) else { return flag.to_string() };
    let kept: Vec<&str> = parts.iter().copied().filter(|p| !named(p, "id")).collect();
    format!("{lead}id={ns}-{base},{}{tail}", kept.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cache_mount_is_the_services_own() {
        let df = "FROM node:22\nRUN --mount=type=cache,target=/root/.npm npm ci\nRUN --mount=type=cache,id=next,target=/app/.next/cache \\\n    --mount=type=secret,id=NPM_TOKEN,env=NPM_TOKEN \"--mount=type=cache,dst=/x\" npm run build\nRUN echo --mount=type=bind,target=/y\n";
        assert_eq!(
            isolate_caches(df, "ns1"),
            "FROM node:22\nRUN --mount=id=ns1-/root/.npm,type=cache,target=/root/.npm npm ci\nRUN --mount=id=ns1-next,type=cache,target=/app/.next/cache \\\n    --mount=type=secret,id=NPM_TOKEN,env=NPM_TOKEN \"--mount=id=ns1-/x,type=cache,dst=/x\" npm run build\nRUN echo --mount=type=bind,target=/y\n"
        );
    }
}
