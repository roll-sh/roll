//! A static site's own `_redirects` and `_headers` (Netlify's and Cloudflare Pages' files, the
//! same syntax), read once when the site loads. A line that doesn't parse is skipped, as theirs
//! are; the files themselves are never served.
//!
//! - `_redirects`: `/from /to [status]` per line. `*` at the end of `from` matches the rest and
//!   `:splat` puts it in `to`; a `:name` segment matches one segment and `:name` puts it in `to`.
//!   301 by default; 301, 302, 303, 307 and 308 redirect (`to` may be another site's URL), 200
//!   serves `to`'s file instead (a rewrite, only where no file is at the path) and 404 answers
//!   `to`'s file with a 404. The first rule that matches decides.
//! - `_headers`: a path (the same patterns) on its own line, then indented `Name: value` lines
//!   for the responses at it. Every matching block adds its headers, in order.

/// The most redirect rules and header blocks a site gets, and the most either file may weigh.
pub const MAX_REDIRECTS: usize = 2000;
pub const MAX_HEADER_RULES: usize = 100;
pub const MAX_FILE: u64 = 64 << 10;

/// The longest a rule's `to` may grow to once its placeholders are filled in.
const MAX_TARGET: usize = 4096;

/// What a placeholder's value may hold as it is: the rest is percent-encoded, so a request's
/// path can't turn a redirect into another host's (`\evil.com`, `//evil.com`) or add a query.
const SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');
const SPLAT: &percent_encoding::AsciiSet = &SEGMENT.remove(b'/');

/// Headers a site may not set: the response's framing (and how its body is encoded) is roll's.
const RESERVED: [&str; 10] = ["content-length", "transfer-encoding", "connection", "keep-alive", "upgrade", "te", "trailer", "proxy-connection", "content-encoding", "content-range"];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SiteRules {
    redirects: Vec<Redirect>,
    headers: Vec<(Pattern, Vec<(String, String)>)>,
}

#[derive(Clone, Debug, PartialEq)]
struct Redirect {
    from: Pattern,
    to: String,
    status: u16,
}

/// A path pattern: its segments, each literal or `:name`, and whether a final `*` takes the rest.
#[derive(Clone, Debug, PartialEq)]
struct Pattern {
    segments: Vec<Segment>,
    splat: bool,
}

#[derive(Clone, Debug, PartialEq)]
enum Segment {
    Literal(String),
    Name(String),
}

/// What a redirect rule makes of a request's path.
#[derive(Debug, PartialEq)]
pub enum Outcome {
    /// Send the visitor on: the status and `Location`.
    Redirect(u16, String),
    /// Serve this path's file instead, with this status (200, or 404).
    Serve(u16, String),
}

impl Pattern {
    fn parse(p: &str) -> Option<Pattern> {
        if !p.starts_with('/') {
            return None;
        }
        let (body, splat) = match p.strip_suffix('*') {
            Some(b) => (b, true),
            None => (p, false),
        };
        let segments = body
            .split('/')
            .filter(|s| !s.is_empty())
            .map(|s| match s.strip_prefix(':') {
                Some(n) if !n.is_empty() => Segment::Name(n.to_string()),
                _ => Segment::Literal(s.to_string()),
            })
            .collect();
        Some(Pattern { segments, splat })
    }

    /// The placeholders `path` binds, if it matches (`splat` for the rest).
    fn matches(&self, path: &str) -> Option<Vec<(String, String)>> {
        let parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if parts.len() < self.segments.len() || (!self.splat && parts.len() != self.segments.len()) {
            return None;
        }
        let mut bound = Vec::new();
        for (seg, part) in self.segments.iter().zip(&parts) {
            match seg {
                Segment::Literal(l) if l == part => {}
                Segment::Literal(_) => return None,
                Segment::Name(n) => bound.push((n.clone(), part.to_string())),
            }
        }
        if self.splat {
            bound.push(("splat".into(), parts[self.segments.len()..].join("/")));
        }
        Some(bound)
    }
}

impl SiteRules {
    /// Both files' text, either of them absent.
    pub fn parse(redirects: Option<&str>, headers: Option<&str>) -> SiteRules {
        let mut rules = SiteRules::default();
        for line in redirects.unwrap_or("").lines() {
            if rules.redirects.len() >= MAX_REDIRECTS {
                break;
            }
            let line = line.split('#').next().unwrap_or("").trim();
            let mut f = line.split_whitespace();
            let (Some(from), Some(to)) = (f.next(), f.next()) else { continue };
            let status = match f.next() {
                None => 301,
                Some(s) => match s.trim_end_matches('!').parse::<u16>() {
                    Ok(n @ (200 | 301 | 302 | 303 | 307 | 308 | 404)) => n,
                    _ => continue,
                },
            };
            // A rewrite or a 404 serves one of the site's own files; a redirect may leave it.
            let external = to.starts_with("https://") || to.starts_with("http://");
            if !(to.starts_with('/') || external && !matches!(status, 200 | 404)) || to.contains(['\r', '\n']) {
                continue;
            }
            if let Some(from) = Pattern::parse(from) {
                rules.redirects.push(Redirect { from, to: to.to_string(), status });
            }
        }
        let mut current: Option<(Pattern, Vec<(String, String)>)> = None;
        for line in headers.unwrap_or("").lines() {
            if line.trim().is_empty() || line.trim_start().starts_with('#') {
                continue;
            }
            if !line.starts_with([' ', '\t']) {
                if let Some(block) = current.take().filter(|(_, h)| !h.is_empty()) {
                    rules.headers.push(block);
                }
                if rules.headers.len() >= MAX_HEADER_RULES {
                    break;
                }
                current = Pattern::parse(line.trim()).map(|p| (p, Vec::new()));
                continue;
            }
            let Some((_, h)) = current.as_mut() else { continue };
            let Some((name, value)) = line.trim().split_once(':') else { continue };
            let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
            if name.is_empty() || RESERVED.contains(&name.as_str()) || axum::http::HeaderName::from_bytes(name.as_bytes()).is_err() || axum::http::HeaderValue::from_str(value).is_err() {
                continue;
            }
            h.push((name, value.to_string()));
        }
        if let Some(block) = current.filter(|(_, h)| !h.is_empty())
            && rules.headers.len() < MAX_HEADER_RULES
        {
            rules.headers.push(block);
        }
        rules
    }

    /// What the first matching redirect rule makes of `path` (its query kept on a redirect that
    /// names none), when `exists` says whether a file answers it (a rewrite only fills a gap).
    pub fn redirect(&self, path: &str, query: Option<&str>, exists: bool) -> Option<Outcome> {
        for r in &self.redirects {
            let Some(mut bound) = r.from.matches(path) else { continue };
            if r.status == 200 && exists {
                continue;
            }
            // Longer names first, so `:id` doesn't eat `:identity`.
            bound.sort_by_key(|(n, _)| std::cmp::Reverse(n.len()));
            let mut to = r.to.clone();
            for (name, value) in &bound {
                let set = if name == "splat" { SPLAT } else { SEGMENT };
                to = to.replace(&format!(":{name}"), &percent_encoding::utf8_percent_encode(value, set).to_string());
                if to.len() > MAX_TARGET {
                    return None;
                }
            }
            // Still on this site where the rule said so: never `//host`.
            if r.to.starts_with('/') && (to.starts_with("//") || to.starts_with("/\\")) {
                return None;
            }
            return Some(match r.status {
                200 | 404 => Outcome::Serve(r.status, to),
                s => {
                    if let Some(q) = query.filter(|q| !q.is_empty() && !to.contains('?')) {
                        to = format!("{to}?{q}");
                    }
                    Outcome::Redirect(s, to)
                }
            });
        }
        None
    }

    /// The headers every block matching `path` adds, in order.
    pub fn headers(&self, path: &str) -> Vec<(String, String)> {
        self.headers.iter().filter(|(p, _)| p.matches(path).is_some()).flat_map(|(_, h)| h.iter().cloned()).collect()
    }
}

/// The files a site's rules come from, which are never served themselves.
pub fn is_rules_file(path: &str) -> bool {
    matches!(path.trim_matches('/'), "_redirects" | "_headers")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redirects_match_in_order_with_splats_and_placeholders() {
        let r = SiteRules::parse(
            Some(
                "# moved\n/old /new\n/blog/:year/:slug /posts/:slug?y=:year 302\n/docs/* /guide/:splat 308\n/app/* /index.html 200\n/gone /404.html 404\n/out https://example.com/x 307\nnot-a-rule\n/bad /x 418\n/ext https://example.com 200\n",
            ),
            None,
        );
        assert_eq!(r.redirect("/old", Some("a=1"), false), Some(Outcome::Redirect(301, "/new?a=1".into())), "the query goes along");
        assert_eq!(r.redirect("/blog/2026/hello", None, false), Some(Outcome::Redirect(302, "/posts/hello?y=2026".into())));
        assert_eq!(r.redirect("/docs/a/b", None, false), Some(Outcome::Redirect(308, "/guide/a/b".into())));
        assert_eq!(r.redirect("/app/settings", None, false), Some(Outcome::Serve(200, "/index.html".into())));
        assert_eq!(r.redirect("/app/logo.png", None, true), None, "a rewrite never hides a file");
        assert_eq!(r.redirect("/gone", None, false), Some(Outcome::Serve(404, "/404.html".into())));
        assert_eq!(r.redirect("/out", None, false), Some(Outcome::Redirect(307, "https://example.com/x".into())));
        assert_eq!(r.redirect("/bad", None, false), None, "an unknown status is skipped");
        assert_eq!(r.redirect("/ext", None, false), None, "a rewrite stays on the site");
        assert_eq!(r.redirect("/blog/2026", None, false), None);
        // A path can't send the visitor to another host, or add a query.
        let open = SiteRules::parse(Some("/go/* /:splat 301\n/u/:name /:name 302\n"), None);
        assert_eq!(open.redirect("/go/\\evil.com", None, false), Some(Outcome::Redirect(301, "/%5Cevil.com".into())));
        assert_eq!(open.redirect("/u/a?b=1#x", None, false), Some(Outcome::Redirect(302, "/a%3Fb%3D1%23x".into())));
        assert_eq!(open.redirect("/go/a/b", None, false), Some(Outcome::Redirect(301, "/a/b".into())));
    }

    #[test]
    fn header_blocks_add_their_headers_and_framing_is_roll_s() {
        let r = SiteRules::parse(
            None,
            Some("/*\n  X-Frame-Options: DENY\n  Content-Length: 1\n/assets/*\n  Cache-Control: public, max-age=31536000, immutable\n  Bad Name: x\n/empty\n"),
        );
        assert_eq!(r.headers("/assets/app.js"), [("x-frame-options".to_string(), "DENY".to_string()), ("cache-control".to_string(), "public, max-age=31536000, immutable".to_string())]);
        assert_eq!(r.headers("/about"), [("x-frame-options".to_string(), "DENY".to_string())]);
        assert!(is_rules_file("/_redirects") && is_rules_file("_headers") && !is_rules_file("/headers"));
    }
}
