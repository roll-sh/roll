//! A static site's edge functions (docs/apps/README.md, "Edge functions"; Netlify Edge Functions,
//! Vercel's Middleware): code that runs before the site's files on the paths it names, to answer
//! itself, change the request, or change the file's answer (`context.next()`). They're found
//! here, with their paths, and run on Deno in the site's functions service ([`crate::functions`]).
//!
//! - Netlify's `netlify/edge-functions/<name>.{ts,js,mjs,mts,tsx,jsx}` (or `<name>/index.*`),
//!   each with its `export const config = { path, excludedPath }`, or netlify.toml's
//!   `[[edge_functions]] function = "<name>" path = "…"`.
//! - Vercel's `middleware.{ts,js}` at the root: every path, or its `config.matcher`.

use crate::Tree;
use serde::{Deserialize, Serialize};

pub const NETLIFY_DIR: &str = "netlify/edge-functions";
const EXT: [&str; 6] = [".ts", ".js", ".mjs", ".mts", ".tsx", ".jsx"];
/// The most edge functions a site has, and paths one names.
const MAX: usize = 20;
const MAX_PATHS: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EdgeKind {
    Netlify,
    Vercel,
}

/// One edge function: its name, file, platform, and the paths it runs on (and not on).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EdgeFn {
    pub name: String,
    pub kind: EdgeKind,
    pub file: String,
    pub paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub excluded: Vec<String>,
}

/// A tree's edge functions, in the order they run (netlify.toml's first, then by name; Vercel's
/// middleware last), and notes on any that can't run.
pub fn find(tree: &dyn Tree) -> (Vec<EdgeFn>, Vec<String>) {
    let mut out: Vec<EdgeFn> = Vec::new();
    let mut notes = Vec::new();
    let text = |p: &str| tree.read(p).and_then(|b| String::from_utf8(b).ok());
    // netlify.toml's declarations, in their order.
    let mut declared: Vec<(String, String)> = Vec::new();
    if let Some(toml) = text("netlify.toml").and_then(|t| t.parse::<toml::Table>().ok()) {
        for e in toml.get("edge_functions").and_then(|v| v.as_array()).into_iter().flatten() {
            if let (Some(f), Some(p)) = (e.get("function").and_then(|v| v.as_str()), e.get("path").and_then(|v| v.as_str())) {
                declared.push((f.to_string(), p.to_string()));
            }
        }
    }
    let mut files: Vec<(String, String)> = Vec::new();
    for entry in tree.list(NETLIFY_DIR) {
        let (name, file) = match entry.strip_suffix('/') {
            Some(dir) => match EXT.iter().map(|e| format!("{NETLIFY_DIR}/{dir}/index{e}")).find(|f| tree.exists(f)) {
                Some(f) => (dir.to_string(), f),
                None => continue,
            },
            None => match EXT.iter().find_map(|e| entry.strip_suffix(e)) {
                Some(n) => (n.to_string(), format!("{NETLIFY_DIR}/{entry}")),
                None => continue,
            },
        };
        if name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            files.push((name, file));
        }
    }
    let add = |f: EdgeFn, out: &mut Vec<EdgeFn>| {
        if out.len() < MAX {
            out.push(f);
        }
    };
    for (name, path) in &declared {
        match files.iter().find(|(n, _)| n == name) {
            Some((_, file)) => match out.iter_mut().find(|f| &f.name == name) {
                Some(f) => f.paths.push(path.clone()),
                None => add(EdgeFn { name: name.clone(), kind: EdgeKind::Netlify, file: file.clone(), paths: vec![path.clone()], excluded: Vec::new() }, &mut out),
            },
            None => notes.push(format!("netlify.toml names the edge function {name}, which {NETLIFY_DIR}/ doesn't have")),
        }
    }
    for (name, file) in &files {
        let (paths, excluded) = text(file).map(|t| config_paths(&t, "path", "excludedPath")).unwrap_or_default();
        match out.iter_mut().find(|f| &f.name == name) {
            Some(f) => {
                f.paths.extend(paths);
                f.excluded.extend(excluded);
            }
            None if !paths.is_empty() => add(EdgeFn { name: name.clone(), kind: EdgeKind::Netlify, file: file.clone(), paths, excluded }, &mut out),
            None => notes.push(format!("{file} names no path (its config.path, or netlify.toml's [[edge_functions]]): it doesn't run")),
        }
    }
    if let Some(file) = ["middleware.ts", "middleware.js", "middleware.mjs"].into_iter().find(|f| tree.exists(f)) {
        let (mut paths, _) = text(file).map(|t| config_paths(&t, "matcher", "")).unwrap_or_default();
        if paths.is_empty() {
            paths.push("/*".into());
        }
        add(EdgeFn { name: "middleware".into(), kind: EdgeKind::Vercel, file: file.into(), paths, excluded: Vec::new() }, &mut out);
    }
    for f in &mut out {
        f.paths.truncate(MAX_PATHS);
        f.excluded.truncate(MAX_PATHS);
    }
    (out, notes)
}

/// A function's inline `export const config = { <key>: "…" | ["…", …] }` (and `<not>`): the
/// strings, read without running it.
fn config_paths(src: &str, key: &str, not: &str) -> (Vec<String>, Vec<String>) {
    let Some(at) = src.find("export const config") else { return (Vec::new(), Vec::new()) };
    let body = &src[at..];
    let end = body.find("};").map_or(body.len(), |i| i + 1);
    let body = &body[..end];
    let strings = |k: &str| -> Vec<String> {
        if k.is_empty() {
            return Vec::new();
        }
        let Some(i) = body.find(&format!("{k}:")).or_else(|| body.find(&format!("{k} :"))) else { return Vec::new() };
        let rest = body[i + k.len()..].trim_start_matches([' ', ':']);
        let span = if rest.starts_with('[') { &rest[..rest.find(']').map_or(rest.len(), |j| j + 1)] } else { &rest[..rest.find([',', '\n', '}']).unwrap_or(rest.len())] };
        let mut out = Vec::new();
        let mut chars = span.chars();
        while let Some(c) = chars.next() {
            if c == '"' || c == '\'' || c == '`' {
                let s: String = chars.by_ref().take_while(|d| *d != c).collect();
                if s.starts_with('/') && s.len() <= 200 {
                    out.push(s);
                }
            }
        }
        out
    };
    (strings(key), strings(not))
}

/// Whether `path` is one an edge function runs on: one of its patterns and none of its excluded
/// ones. A pattern is a path where `*` is any rest, `:name` one segment and `:name*` (or `:name+`,
/// `(.*)`) any rest.
pub fn runs_on(f: &EdgeFn, path: &str) -> bool {
    f.paths.iter().any(|p| matches(p, path)) && !f.excluded.iter().any(|p| matches(p, path))
}

pub fn matches(pattern: &str, path: &str) -> bool {
    let pattern = pattern.split('?').next().unwrap_or("");
    let (mut pi, mut si) = (pattern.split('/').filter(|s| !s.is_empty()).peekable(), path.split('/').filter(|s| !s.is_empty()).peekable());
    loop {
        match (pi.next(), si.peek()) {
            (None, None) => return true,
            (None, Some(_)) => return false,
            (Some(p), seg) => {
                // Any rest, none included.
                if p == "*" || p == "(.*)" || (p.starts_with(':') && (p.ends_with('*') || p.ends_with("(.*)"))) {
                    return true;
                }
                if p.starts_with(':') && p.ends_with('+') {
                    return seg.is_some();
                }
                let Some(s) = seg else { return false };
                if !(p.starts_with(':') || p == *s || (p.contains('*') && glob(p, s))) {
                    return false;
                }
                si.next();
            }
        }
    }
}

/// One segment against a pattern with `*` in it (`*.png`, `v*`).
fn glob(p: &str, s: &str) -> bool {
    let parts: Vec<&str> = p.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !s.starts_with(first) || s.len() < first.len() + last.len() || !s.ends_with(last) {
        return false;
    }
    let mut rest = &s[first.len()..s.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> crate::MapTree {
        crate::MapTree(files.iter().map(|(p, c)| (p.to_string(), c.to_string())).collect())
    }

    #[test]
    fn edge_functions_are_found_with_their_paths() {
        let files = [
            ("netlify.toml", "[[edge_functions]]\nfunction = \"geo\"\npath = \"/shop/*\"\n"),
            ("netlify/edge-functions/geo.ts", "export default async (req, ctx) => ctx.next();"),
            ("netlify/edge-functions/auth.ts", "export default () => new Response('no', { status: 401 });\nexport const config = { path: [\"/admin/*\", \"/account\"], excludedPath: \"/admin/login\" };"),
            ("netlify/edge-functions/lost.js", "export default () => {}"),
            ("middleware.ts", "export default function middleware() {}\nexport const config = { matcher: ['/about/:path*'] };"),
        ];
        let (found, notes) = find(&tree(&files));
        let names: Vec<(&str, Vec<&str>)> = found.iter().map(|f| (f.name.as_str(), f.paths.iter().map(String::as_str).collect())).collect();
        assert_eq!(names, [("geo", vec!["/shop/*"]), ("auth", vec!["/admin/*", "/account"]), ("middleware", vec!["/about/:path*"])]);
        assert!(notes.iter().any(|n| n.contains("lost.js names no path")), "{notes:?}");
        let auth = &found[1];
        assert!(runs_on(auth, "/admin/users") && runs_on(auth, "/account") && !runs_on(auth, "/admin/login") && !runs_on(auth, "/accounts"));
        assert!(runs_on(&found[2], "/about") && runs_on(&found[2], "/about/team/x") && !runs_on(&found[2], "/"));
        assert!(matches("/blog/:slug", "/blog/a") && !matches("/blog/:slug", "/blog/a/b") && matches("/*", "/") && matches("/img/*.png", "/img/a.png"));
    }
}
