//! A static site's `vercel.json` and `netlify.toml` rules, as the `_redirects` and `_headers`
//! lines roll apps' site reader takes (apps-control's `site_rules`: one reader for every site's
//! rules, the same bounds and the same checks on where a rule may send a visitor). Read from the
//! tree at plan time and appended to the site's own files in the image: the configs themselves
//! never go into it (`netlify.toml` can hold build settings). What can't be said in those lines (a
//! regular expression in a path, a condition, `has`/`missing`) is left out and named in
//! `skipped`, so a site moved from Vercel or Netlify says what it lost instead of losing it
//! quietly.
//!
//! - `netlify.toml`: `[[redirects]]` (`from`, `to`, `status`, default 301) and `[[headers]]`
//!   (`for` and its `values`). Its path syntax is `_redirects`'.
//! - `vercel.json`: `redirects` (`source`, `destination`, `permanent` 308/307 or
//!   `statusCode`), `rewrites` (`source`, `destination`: a rewrite, 200) and `headers`
//!   (`source`, `headers: [{key, value}]`). A `:name` segment stays one; `:name*`, `:name+` and
//!   `(.*)` at the end take the rest (`:splat`), and `$1` in a destination is it.

use serde_json::Value;

/// One config's rules as `_redirects` and `_headers` text, and what was left out.
#[derive(Debug, Default, PartialEq)]
pub struct Converted {
    pub redirects: String,
    pub headers: String,
    pub skipped: Vec<String>,
}

/// The most rules a config gives (the rules files' own bounds apply after).
const MAX_RULES: usize = 2000;

fn clean(s: &str) -> Option<&str> {
    let s = s.trim();
    (!s.is_empty() && !s.contains(char::is_whitespace) && !s.contains(['\r', '\n', '#'])).then_some(s)
}

/// `netlify.toml`'s redirects and headers.
pub fn from_netlify(text: &str) -> Converted {
    let mut out = Converted::default();
    let Ok(doc) = text.parse::<toml::Table>() else {
        out.skipped.push("netlify.toml doesn't parse".into());
        return out;
    };
    for r in doc.get("redirects").and_then(|v| v.as_array()).into_iter().flatten().take(MAX_RULES) {
        let (from, to) = (r.get("from").and_then(|v| v.as_str()).and_then(clean), r.get("to").and_then(|v| v.as_str()).and_then(clean));
        let status = r.get("status").and_then(|v| v.as_integer()).unwrap_or(301);
        let (Some(from), Some(to)) = (from, to) else {
            out.skipped.push("netlify.toml: a redirect without from and to".into());
            continue;
        };
        if ["conditions", "query", "signed", "headers"].iter().any(|k| r.get(*k).is_some()) {
            out.skipped.push(format!("netlify.toml: {from} → {to} (its conditions, query or signing)"));
            continue;
        }
        out.redirects.push_str(&format!("{from} {to} {status}\n"));
    }
    for h in doc.get("headers").and_then(|v| v.as_array()).into_iter().flatten().take(MAX_RULES) {
        let (Some(path), Some(values)) = (h.get("for").and_then(|v| v.as_str()).and_then(clean), h.get("values").and_then(|v| v.as_table())) else {
            out.skipped.push("netlify.toml: a headers block without for and values".into());
            continue;
        };
        out.headers.push_str(&format!("{path}\n"));
        for (k, v) in values {
            let Some(v) = v.as_str().filter(|v| !v.contains(['\r', '\n'])) else { continue };
            out.headers.push_str(&format!("  {k}: {v}\n"));
        }
    }
    out
}

/// A Vercel path (path-to-regexp) as a `_redirects` one, and the name its rest is bound to.
fn vercel_source(p: &str) -> Option<(String, Option<String>)> {
    let p = clean(p)?;
    if !p.starts_with('/') {
        return None;
    }
    let mut segs: Vec<&str> = p.split('/').skip(1).collect();
    let mut rest = None;
    if let Some(last) = segs.last().copied() {
        if last == "(.*)" {
            rest = Some("1".to_string());
            segs.pop();
        } else if let Some(n) = last.strip_prefix(':').and_then(|n| n.strip_suffix('*').or_else(|| n.strip_suffix('+'))) {
            rest = Some(n.to_string());
            segs.pop();
        }
    }
    // Anything else of a regular expression isn't a `_redirects` path.
    let plain = |s: &str| !s.contains(['(', ')', '*', '+', '?', '[', ']', '{', '}', '|', '\\']);
    if !segs.iter().all(|s| plain(s)) {
        return None;
    }
    let mut out = format!("/{}", segs.join("/"));
    if rest.is_some() {
        out = format!("{}/*", out.trim_end_matches('/'));
    }
    Some((out, rest))
}

/// A destination with the rest's name (`:path*`, `:path`, `$1`) as `:splat`.
fn vercel_destination(d: &str, rest: Option<&str>) -> Option<String> {
    let mut d = clean(d)?.to_string();
    if let Some(n) = rest {
        if n == "1" {
            d = d.replace("$1", ":splat");
        } else {
            for form in [format!(":{n}*"), format!(":{n}+"), format!(":{n}")] {
                d = d.replace(&form, ":splat");
            }
        }
    }
    (!d.contains('$')).then_some(d)
}

/// `vercel.json`'s redirects, rewrites and headers.
pub fn from_vercel(text: &str) -> Converted {
    let mut out = Converted::default();
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        out.skipped.push("vercel.json doesn't parse".into());
        return out;
    };
    let rule = |r: &Value, what: &str, out: &mut Converted, status: u16| {
        let (source, dest) = (r["source"].as_str().unwrap_or(""), r["destination"].as_str().unwrap_or(""));
        if r.get("has").is_some() || r.get("missing").is_some() {
            out.skipped.push(format!("vercel.json {what}: {source} → {dest} (has/missing)"));
            return;
        }
        match vercel_source(source).and_then(|(from, rest)| Some((from, vercel_destination(dest, rest.as_deref())?))) {
            Some((from, to)) => out.redirects.push_str(&format!("{from} {to} {status}\n")),
            None => out.skipped.push(format!("vercel.json {what}: {source} → {dest} (a pattern _redirects can't say)")),
        }
    };
    for r in v["redirects"].as_array().into_iter().flatten().take(MAX_RULES) {
        let status = r["statusCode"].as_u64().map(|s| s as u16).unwrap_or(if r["permanent"] == false { 307 } else { 308 });
        rule(r, "redirect", &mut out, status);
    }
    for r in v["rewrites"].as_array().into_iter().flatten().take(MAX_RULES) {
        rule(r, "rewrite", &mut out, 200);
    }
    for h in v["headers"].as_array().into_iter().flatten().take(MAX_RULES) {
        let source = h["source"].as_str().unwrap_or("");
        let Some((path, _)) = vercel_source(source).filter(|_| h.get("has").is_none() && h.get("missing").is_none()) else {
            out.skipped.push(format!("vercel.json headers: {source} (a pattern _headers can't say)"));
            continue;
        };
        out.headers.push_str(&format!("{path}\n"));
        for kv in h["headers"].as_array().into_iter().flatten() {
            if let (Some(k), Some(val)) = (kv["key"].as_str(), kv["value"].as_str().filter(|v| !v.contains(['\r', '\n']))) {
                out.headers.push_str(&format!("  {k}: {val}\n"));
            }
        }
    }
    for k in ["trailingSlash", "routes"] {
        if v.get(k).is_some() {
            out.skipped.push(format!("vercel.json {k}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vercel_and_netlify_rules_become_redirects_and_headers_lines() {
        let v = from_vercel(
            r#"{ "redirects": [{ "source": "/old/:slug", "destination": "/new/:slug" }, { "source": "/blog/:path*", "destination": "https://blog.example/:path*", "permanent": false }, { "source": "/re/(\\d+)", "destination": "/x" }],
                 "rewrites": [{ "source": "/app/(.*)", "destination": "/index.html" }, { "source": "/api/:rest*", "destination": "service:api/:rest*" }],
                 "headers": [{ "source": "/assets/(.*)", "headers": [{ "key": "Cache-Control", "value": "public, max-age=31536000, immutable" }] }],
                 "trailingSlash": false }"#,
        );
        assert_eq!(v.redirects, "/old/:slug /new/:slug 308\n/blog/* https://blog.example/:splat 307\n/app/* /index.html 200\n/api/* service:api/:splat 200\n");
        assert_eq!(v.headers, "/assets/*\n  Cache-Control: public, max-age=31536000, immutable\n");
        assert_eq!(v.skipped, vec!["vercel.json redirect: /re/(\\d+) → /x (a pattern _redirects can't say)".to_string(), "vercel.json trailingSlash".to_string()]);
        let n = from_netlify("[[redirects]]\nfrom = \"/docs/*\"\nto = \"/guide/:splat\"\n\n[[redirects]]\nfrom = \"/x\"\nto = \"/y\"\nconditions = { Country = [\"us\"] }\n\n[[headers]]\nfor = \"/*\"\n[headers.values]\nX-Frame-Options = \"DENY\"\n");
        assert_eq!((n.redirects.as_str(), n.headers.as_str(), n.skipped.len()), ("/docs/* /guide/:splat 301\n", "/*\n  X-Frame-Options: DENY\n", 1));
    }
}
