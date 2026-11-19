//! A static site's own `_redirects` and `_headers` (Netlify's and Cloudflare Pages' files, the
//! same syntax), read once when the site loads. A line that doesn't parse is skipped, as theirs
//! are; the files themselves are never served.
//!
//! - `_redirects`: `/from /to [status]` per line. `*` at the end of `from` matches the rest and
//!   `:splat` puts it in `to`; a `:name` segment matches one segment and `:name` puts it in `to`.
//!   301 by default; 301, 302, 303, 307 and 308 redirect (`to` may be another site's URL), 200
//!   serves `to`'s file instead (a rewrite, only where no file is at the path) and 404 answers
//!   `to`'s file with a 404. The first rule that matches decides. A 200 whose `to` is
//!   `service:<name>/<path>` sends the request to the project's service of that name in the
//!   same environment (Netlify's and Vercel's proxy rewrites, only to the project's own); one
//!   whose `to` is another site's `https://` URL proxies it there (Netlify's 200 proxies,
//!   Vercel's external rewrites), fetched as roll fetches any URL a tenant chose.
//! - Conditions after the status (Netlify's): `Country=us,ca` (the edge's country), `Language=en`
//!   (the browser's `Accept-Language`) and `Cookie=name` (one of them set): each given must hold
//!   for the rule to match. A rule on something roll doesn't know here (`Role=`) never matches.
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
    when: Conditions,
}

/// A rule's conditions ([`Redirect::when`]): each given must hold.
#[derive(Clone, Debug, Default, PartialEq)]
struct Conditions {
    countries: Vec<String>,
    languages: Vec<String>,
    cookies: Vec<String>,
}

/// What a rule's conditions read of a request.
#[derive(Debug, Default)]
pub struct Visitor {
    /// The edge's (`CF-IPCountry`), upper case.
    pub country: Option<String>,
    /// `Accept-Language`'s languages, lower case, each with its primary tag too (`en-us`, `en`).
    pub languages: Vec<String>,
    /// The names of the cookies it sent.
    pub cookies: Vec<String>,
}

impl Visitor {
    pub fn of(headers: &axum::http::HeaderMap) -> Visitor {
        let text = |h: &str| headers.get_all(h).iter().filter_map(|v| v.to_str().ok()).collect::<Vec<_>>().join(if h == "cookie" { ";" } else { "," });
        let country = headers.get("cf-ipcountry").and_then(|v| v.to_str().ok()).filter(|c| c.len() == 2 && c.bytes().all(|b| b.is_ascii_alphabetic())).map(|c| c.to_ascii_uppercase());
        let mut languages = Vec::new();
        for l in text("accept-language").split(',').take(20) {
            let tag = l.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            if tag.is_empty() || tag == "*" || tag.len() > 35 {
                continue;
            }
            if let Some(primary) = tag.split('-').next().filter(|p| *p != tag) {
                languages.push(primary.to_string());
            }
            languages.push(tag);
        }
        let cookies = text("cookie").split(';').take(100).filter_map(|kv| kv.trim().split_once('=').map(|(k, _)| k.trim().to_string())).filter(|k| !k.is_empty()).collect();
        Visitor { country, languages, cookies }
    }
}

impl Conditions {
    fn hold(&self, v: &Visitor) -> bool {
        (self.countries.is_empty() || v.country.as_ref().is_some_and(|c| self.countries.contains(c)))
            && (self.languages.is_empty() || v.languages.iter().any(|l| self.languages.contains(l)))
            && (self.cookies.is_empty() || v.cookies.iter().any(|c| self.cookies.contains(c)))
    }
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
    /// Send the request to the project's service `service` in this environment, at this path
    /// (its query kept unless the rule names one).
    Proxy { service: String, path: String },
    /// Fetch this URL on another site and answer with what it says (its query kept likewise).
    External(String),
}

/// `service:<name>[/<path>]`: the name, and the path (`/` when none).
fn service_target(to: &str) -> Option<(&str, &str)> {
    let rest = to.strip_prefix("service:")?;
    let (name, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    let ok = !name.is_empty() && name.len() <= 32 && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    ok.then_some((name, path))
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
            // The status, if it's there, then conditions.
            let mut rest: Vec<&str> = f.collect();
            let status = match rest.first().filter(|s| !s.contains('=')) {
                None => 301,
                Some(s) => match s.trim_end_matches('!').parse::<u16>() {
                    Ok(n @ (200 | 301 | 302 | 303 | 307 | 308 | 404)) => {
                        rest.remove(0);
                        n
                    }
                    _ => continue,
                },
            };
            let mut when = Conditions::default();
            let mut known = true;
            for c in rest {
                let Some((k, v)) = c.split_once('=') else {
                    known = false;
                    break;
                };
                let list = || v.split(',').map(str::trim).filter(|x| !x.is_empty()).take(50);
                match k.to_ascii_lowercase().as_str() {
                    "country" => when.countries = list().map(str::to_ascii_uppercase).collect(),
                    "language" => when.languages = list().map(str::to_ascii_lowercase).collect(),
                    "cookie" => when.cookies = list().map(str::to_string).collect(),
                    // `Role=` (Netlify Identity) and the like: nobody here holds it.
                    _ => known = false,
                }
            }
            if !known {
                continue;
            }
            // A rewrite or a 404 serves one of the site's own files, a 200 to another site's
            // https:// URL proxies it, a redirect may leave it.
            let external = to.starts_with("https://") || to.starts_with("http://");
            let service = service_target(to).is_some() && status == 200;
            // Its host is the rule's: a placeholder there would let a visitor pick it (a port is fine).
            // (Plain http:// is taken here and refused at the fetch unless private targets are
            // allowed, as for any URL a tenant chose.)
            let scheme = ["https://", "http://"].into_iter().find(|s| to.starts_with(s));
            let proxied = status == 200
                && scheme.is_some_and(|s| to[s.len()..].split(['/', '?']).next().is_some_and(|host| !host.is_empty() && !host.split(':').skip(1).any(|p| !p.bytes().all(|b| b.is_ascii_digit()))));
            if !(to.starts_with('/') || service || proxied || external && !matches!(status, 200 | 404)) || to.contains(['\r', '\n']) {
                continue;
            }
            if let Some(from) = Pattern::parse(from) {
                rules.redirects.push(Redirect { from, to: to.to_string(), status, when });
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
    pub fn redirect(&self, path: &str, query: Option<&str>, exists: bool, visitor: &Visitor) -> Option<Outcome> {
        for r in &self.redirects {
            let Some(mut bound) = r.from.matches(path) else { continue };
            if (r.status == 200 && exists) || !r.when.hold(visitor) {
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
            if let Some((service, path)) = service_target(&to) {
                let mut path = if path.is_empty() { "/".to_string() } else { path.to_string() };
                if path.starts_with("//") || path.starts_with("/\\") {
                    return None;
                }
                if let Some(q) = query.filter(|q| !q.is_empty() && !path.contains('?')) {
                    path = format!("{path}?{q}");
                }
                return Some(Outcome::Proxy { service: service.to_string(), path });
            }
            if r.status == 200 && (to.starts_with("https://") || to.starts_with("http://")) {
                if let Some(q) = query.filter(|q| !q.is_empty() && !to.contains('?')) {
                    to = format!("{to}?{q}");
                }
                return Some(Outcome::External(to));
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

/// The files a site's rules come from, which are never served themselves (a site built from
/// its tree's root has its `netlify.toml` and `vercel.json` in it, and those can hold build
/// settings).
pub fn is_rules_file(path: &str) -> bool {
    matches!(path.trim_matches('/'), "_redirects" | "_headers" | "netlify.toml" | "vercel.json")
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
        assert_eq!(r.redirect("/old", Some("a=1"), false, &Visitor::default()), Some(Outcome::Redirect(301, "/new?a=1".into())), "the query goes along");
        assert_eq!(r.redirect("/blog/2026/hello", None, false, &Visitor::default()), Some(Outcome::Redirect(302, "/posts/hello?y=2026".into())));
        assert_eq!(r.redirect("/docs/a/b", None, false, &Visitor::default()), Some(Outcome::Redirect(308, "/guide/a/b".into())));
        assert_eq!(r.redirect("/app/settings", None, false, &Visitor::default()), Some(Outcome::Serve(200, "/index.html".into())));
        assert_eq!(r.redirect("/app/logo.png", None, true, &Visitor::default()), None, "a rewrite never hides a file");
        assert_eq!(r.redirect("/gone", None, false, &Visitor::default()), Some(Outcome::Serve(404, "/404.html".into())));
        assert_eq!(r.redirect("/out", None, false, &Visitor::default()), Some(Outcome::Redirect(307, "https://example.com/x".into())));
        assert_eq!(r.redirect("/bad", None, false, &Visitor::default()), None, "an unknown status is skipped");
        assert_eq!(r.redirect("/ext", None, false, &Visitor::default()), Some(Outcome::External("https://example.com".into())), "a 200 to another site proxies it");
        assert_eq!(r.redirect("/blog/2026", None, false, &Visitor::default()), None);
        // A path can't send the visitor to another host, or add a query.
        let open = SiteRules::parse(Some("/go/* /:splat 301\n/u/:name /:name 302\n"), None);
        assert_eq!(open.redirect("/go/\\evil.com", None, false, &Visitor::default()), Some(Outcome::Redirect(301, "/%5Cevil.com".into())));
        assert_eq!(open.redirect("/u/a?b=1#x", None, false, &Visitor::default()), Some(Outcome::Redirect(302, "/a%3Fb%3D1%23x".into())));
        assert_eq!(open.redirect("/go/a/b", None, false, &Visitor::default()), Some(Outcome::Redirect(301, "/a/b".into())));
        // To the project's own service, its query kept; never another kind of target or status.
        let api = SiteRules::parse(Some("/api/* service:api/v1/:splat 200\n/hc service:api 200\n/bad service:API/x 200\n/moved service:api/x 301\n"), None);
        assert_eq!(api.redirect("/api/users/7", Some("a=1"), false, &Visitor::default()), Some(Outcome::Proxy { service: "api".into(), path: "/v1/users/7?a=1".into() }));
        assert_eq!(api.redirect("/hc", None, false, &Visitor::default()), Some(Outcome::Proxy { service: "api".into(), path: "/".into() }));
        assert_eq!((api.redirect("/bad", None, false, &Visitor::default()), api.redirect("/moved", None, false, &Visitor::default())), (None, None));
    }

    #[test]
    fn rules_proxy_to_other_sites_and_hold_their_conditions() {
        let r = SiteRules::parse(Some("/api/* https://api.example.com/v2/:splat 200\n/x/:host https://:host/ 200\n/fr / 302 Country=fr,be Language=fr\n/beta/* /beta.html 200 Cookie=beta\n/admin /a.html 200 Role=admin\n"), None);
        assert_eq!(r.redirect("/api/users/7", Some("a=1"), false, &Visitor::default()), Some(Outcome::External("https://api.example.com/v2/users/7?a=1".into())));
        assert_eq!(r.redirect("/x/evil.com", None, false, &Visitor::default()), None, "a visitor never picks the host");
        let fr = Visitor { country: Some("BE".into()), languages: vec!["fr-be".into(), "fr".into()], cookies: vec![] };
        assert_eq!(r.redirect("/fr", None, false, &fr), Some(Outcome::Redirect(302, "/".into())));
        assert_eq!(r.redirect("/fr", None, false, &Visitor { country: Some("BE".into()), ..Visitor::default() }), None, "every condition holds");
        assert_eq!(r.redirect("/beta/x", None, false, &Visitor { cookies: vec!["beta".into()], ..Visitor::default() }), Some(Outcome::Serve(200, "/beta.html".into())));
        assert_eq!(r.redirect("/beta/x", None, false, &Visitor::default()), None);
        assert_eq!(r.redirect("/admin", None, false, &Visitor { cookies: vec!["nf_jwt".into()], ..Visitor::default() }), None, "a condition roll doesn't know never matches");
        let mut h = axum::http::HeaderMap::new();
        h.insert("accept-language", "fr-CH, en;q=0.8".parse().unwrap());
        h.insert("cookie", "beta=1; roll_b=2".parse().unwrap());
        h.insert("cf-ipcountry", "ch".parse().unwrap());
        let v = Visitor::of(&h);
        assert_eq!((v.country.as_deref(), v.languages, v.cookies), (Some("CH"), vec!["fr".to_string(), "fr-ch".into(), "en".into()], vec!["beta".to_string(), "roll_b".into()]));
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
