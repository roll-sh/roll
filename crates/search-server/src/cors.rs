//! Browsers' cross-origin requests to a cluster (docs/search/README.md, "Search from the
//! browser"), with Elasticsearch's `http.cors.*` semantics (its `CorsHandler`), set per cluster
//! as Elastic Cloud's user settings set them per deployment.
//!
//! - A preflight (`OPTIONS` with `Origin` and `Access-Control-Request-Method`) is answered
//!   before authentication, since browsers send no credentials with it: an allowed origin gets
//!   200 with the allowed methods, headers and max age (one header line per method and per
//!   header, as Elasticsearch writes them), any other an empty 403. The cluster's IP allowlist
//!   is checked first ([`crate::allow`]).
//! - Any other request is authenticated first; then an `Origin` the cluster doesn't allow gets
//!   an empty 403, and an allowed one gets `Access-Control-Allow-Origin`, `Vary: Origin` and
//!   `Access-Control-Expose-Headers` on every answer (errors included, so a page can read a 401
//!   or a 429). A request without `Origin`, or from the request's own host
//!   (`isSameOrigin`), isn't a cross-origin one and passes.
//! - Credentials are never allowed (`http.cors.allow-credentials: false`): pages send keys in
//!   `Authorization`, never cookies, so `*` is safe and is answered as `*`.
//!
//! One difference: Elasticsearch leaves `Vary` out of `*`'s answers; roll always sends it, so a
//! cache in front never hands one origin's answer to another.

use regex::Regex;

/// `http.cors.allow-methods`' default, in the order Elasticsearch answers them (its `Method`
/// enum's).
pub const DEFAULT_METHODS: [&str; 6] = ["GET", "POST", "PUT", "DELETE", "OPTIONS", "HEAD"];
/// The methods Elasticsearch's `http.cors.allow-methods` takes, in its answer order.
pub const METHODS: [&str; 9] = ["GET", "POST", "PUT", "DELETE", "OPTIONS", "HEAD", "PATCH", "TRACE", "CONNECT"];
/// `http.cors.allow-headers`' default (Elasticsearch 9), in the order Elasticsearch answers them.
pub const DEFAULT_HEADERS: [&str; 7] = ["Authorization", "X-Elastic-Client-Meta", "X-Requested-With", "Accept", "User-Agent", "Content-Length", "Content-Type"];
/// `http.cors.max-age`' default: 20 days, in seconds.
pub const DEFAULT_MAX_AGE: i64 = 1_728_000;
/// `http.cors.exposed-headers`' default.
pub const EXPOSED_HEADERS: &str = "X-elastic-product";
/// The most origins a cluster may allow.
pub const MAX_ORIGINS: usize = 32;
/// The most headers a cluster may allow.
pub const MAX_HEADERS: usize = 64;
/// The longest `/regex/`, and what it may compile to.
const MAX_PATTERN: usize = 1024;
const PATTERN_SIZE_LIMIT: usize = 256 * 1024;

/// One entry of `allow-origin`.
#[derive(Clone, Debug)]
pub enum AllowedOrigin {
    /// `*`: any origin.
    Any,
    /// An origin as browsers send it (`https://shop.example.com`, `http://localhost:3000`).
    Exact(String),
    /// `/regex/`: origins it matches as a whole (Java's `Matcher.matches`).
    Pattern(String, Regex),
}

impl AllowedOrigin {
    /// An entry as an owner writes it: `*`, `/regex/` or `scheme://host[:port]` (http or
    /// https, no path), kept as a browser serializes an origin (lower case, no default port).
    pub fn parse(s: &str) -> Result<AllowedOrigin, String> {
        let s = s.trim();
        if s == "*" {
            return Ok(AllowedOrigin::Any);
        }
        if let Some(body) = s.strip_prefix('/').and_then(|r| r.strip_suffix('/')).filter(|_| s.len() >= 2) {
            if body.is_empty() || body.len() > MAX_PATTERN {
                return Err(format!("a /regex/ origin is 1 to {MAX_PATTERN} characters"));
            }
            let re = regex::RegexBuilder::new(&format!("^(?:{body})$"))
                .size_limit(PATTERN_SIZE_LIMIT)
                .build()
                .map_err(|e| format!("{s:?} isn't a regular expression: {}", e.to_string().lines().last().unwrap_or_default()))?;
            return Ok(AllowedOrigin::Pattern(body.to_string(), re));
        }
        normalize_origin(s).map(AllowedOrigin::Exact)
    }

    fn matches(&self, origin: &str) -> bool {
        match self {
            AllowedOrigin::Any => true,
            AllowedOrigin::Exact(o) => o == origin,
            AllowedOrigin::Pattern(_, re) => re.is_match(origin),
        }
    }

    /// The entry as written back (`/regex/` keeps its slashes).
    pub fn as_written(&self) -> String {
        match self {
            AllowedOrigin::Any => "*".into(),
            AllowedOrigin::Exact(o) => o.clone(),
            AllowedOrigin::Pattern(p, _) => format!("/{p}/"),
        }
    }
}

/// `scheme://host[:port]` as a browser's `Origin` header has it: http or https, lower case, the
/// scheme's default port dropped, nothing after the authority (a trailing `/` is forgiven).
pub fn normalize_origin(s: &str) -> Result<String, String> {
    let bad = || format!("{s:?} isn't an origin: write scheme://host[:port] (https://shop.example.com, http://localhost:3000), * or /regex/");
    let lower = s.trim().trim_end_matches('/').to_ascii_lowercase();
    let (scheme, rest) = lower.split_once("://").ok_or_else(bad)?;
    let default_port = match scheme {
        "http" => "80",
        "https" => "443",
        _ => return Err(bad()),
    };
    if rest.is_empty() || rest.contains(['/', '?', '#', '@', ' ', '\\']) {
        return Err(bad());
    }
    let (host, port) = match rest.strip_prefix('[') {
        // An IPv6 literal: `[::1]` or `[::1]:8080`.
        Some(v6) => {
            let (addr, after) = v6.split_once(']').ok_or_else(bad)?;
            addr.parse::<std::net::Ipv6Addr>().map_err(|_| bad())?;
            let port = match after {
                "" => None,
                p => Some(p.strip_prefix(':').ok_or_else(bad)?),
            };
            (format!("[{addr}]"), port)
        }
        None => match rest.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p)),
            None => (rest.to_string(), None),
        },
    };
    let host_ok = !host.is_empty()
        && host.len() <= 253
        && (host.starts_with('[')
            || host.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')));
    if !host_ok {
        return Err(bad());
    }
    match port {
        None => Ok(format!("{scheme}://{host}")),
        Some(p) if p == default_port => Ok(format!("{scheme}://{host}")),
        Some(p) if !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u32>().is_ok_and(|n| (1..=65535).contains(&n)) => {
            Ok(format!("{scheme}://{host}:{}", p.parse::<u32>().unwrap_or_default()))
        }
        Some(_) => Err(bad()),
    }
}

/// A cluster's CORS settings (`http.cors.enabled: true` with these values).
#[derive(Clone, Debug)]
pub struct Cors {
    origins: Vec<AllowedOrigin>,
    methods: Vec<String>,
    headers: Vec<String>,
    max_age: i64,
}

impl Cors {
    /// `origins` as [`AllowedOrigin::parse`] reads them (at least one); `methods`, `headers` and
    /// `max_age`: Elasticsearch's defaults when `None`.
    pub fn new(origins: &[String], methods: Option<&[String]>, headers: Option<&[String]>, max_age: Option<i64>) -> Result<Cors, String> {
        if origins.is_empty() || origins.len() > MAX_ORIGINS {
            return Err(format!("allow_origin: 1 to {MAX_ORIGINS} origins"));
        }
        let mut parsed: Vec<AllowedOrigin> = Vec::with_capacity(origins.len());
        for o in origins {
            let p = AllowedOrigin::parse(o).map_err(|e| format!("allow_origin: {e}"))?;
            if !parsed.iter().any(|q| q.as_written() == p.as_written()) {
                parsed.push(p);
            }
        }
        let methods = match methods {
            None => DEFAULT_METHODS.iter().map(|m| m.to_string()).collect(),
            Some(given) => {
                let mut want = Vec::new();
                for m in given {
                    let up = m.trim().to_ascii_uppercase();
                    if !METHODS.contains(&up.as_str()) {
                        return Err(format!("allow_methods: {m:?} isn't one of {}", METHODS.join(", ")));
                    }
                    want.push(up);
                }
                if want.is_empty() {
                    return Err("allow_methods: at least one method".into());
                }
                // Elasticsearch keeps them in an `EnumSet`: answered in the enum's order.
                METHODS.iter().filter(|m| want.iter().any(|w| w == *m)).map(|m| m.to_string()).collect()
            }
        };
        let headers = match headers {
            None => DEFAULT_HEADERS.iter().map(|h| h.to_string()).collect(),
            Some(given) => {
                if given.len() > MAX_HEADERS {
                    return Err(format!("allow_headers: at most {MAX_HEADERS}"));
                }
                let mut out: Vec<String> = Vec::new();
                for h in given {
                    let h = h.trim();
                    if h.is_empty() || h.len() > 128 || !h.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)) {
                        return Err(format!("allow_headers: {h:?} isn't a header name"));
                    }
                    if !out.iter().any(|o| o.eq_ignore_ascii_case(h)) {
                        out.push(h.to_string());
                    }
                }
                out
            }
        };
        let max_age = max_age.unwrap_or(DEFAULT_MAX_AGE);
        if !(-1..=i64::from(i32::MAX)).contains(&max_age) {
            return Err("max_age: -1 to 2147483647 seconds".into());
        }
        Ok(Cors { origins: parsed, methods, headers, max_age })
    }

    pub fn origins(&self) -> &[AllowedOrigin] {
        &self.origins
    }

    fn any(&self) -> bool {
        self.origins.iter().any(|o| matches!(o, AllowedOrigin::Any))
    }

    /// Whether a request from `origin` to `host` (its `Host` header) may be answered.
    pub fn allows(&self, origin: &str, host: Option<&str>) -> bool {
        same_origin(origin, host) || self.origins.iter().any(|o| o.matches(origin))
    }

    /// The headers every answer to an allowed `origin` carries (`None`: not allowed). A
    /// browser key's 429 (`throttled`) also exposes its `Retry-After`, so the page can back off.
    pub fn answer_headers(&self, origin: &str, host: Option<&str>, throttled: bool) -> Option<Vec<(&'static str, String)>> {
        if !self.allows(origin, host) {
            return None;
        }
        let exposed = if throttled { format!("{EXPOSED_HEADERS},Retry-After") } else { EXPOSED_HEADERS.into() };
        Some(vec![("access-control-allow-origin", self.allow_origin_value(origin, host)), ("vary", "origin".into()), ("access-control-expose-headers", exposed)])
    }

    /// A preflight's 200 headers for `origin` (`None`: answer an empty 403).
    pub fn preflight_headers(&self, origin: &str, host: Option<&str>) -> Option<Vec<(&'static str, String)>> {
        if !self.allows(origin, host) {
            return None;
        }
        let mut h = vec![("access-control-allow-origin", self.allow_origin_value(origin, host)), ("vary", "origin".into())];
        h.extend(self.methods.iter().map(|m| ("access-control-allow-methods", m.clone())));
        h.extend(self.headers.iter().map(|x| ("access-control-allow-headers", x.clone())));
        h.push(("access-control-max-age", self.max_age.to_string()));
        Some(h)
    }

    /// `*` for any origin (credentials are never allowed), else the origin itself.
    fn allow_origin_value(&self, origin: &str, host: Option<&str>) -> String {
        if self.any() && !same_origin(origin, host) { "*".into() } else { origin.to_string() }
    }

    /// The `http.cors.*` node settings these are, as `_nodes/settings` and
    /// `_cluster/settings?include_defaults` show them. Elasticsearch's `allow-origin` is `*`, one
    /// `/regex/` or a list of origins: several entries with a pattern among them are shown as one
    /// pattern matching any of them.
    pub fn settings(&self) -> Vec<(String, String)> {
        let origin = if self.any() {
            "*".to_string()
        } else if self.origins.iter().all(|o| matches!(o, AllowedOrigin::Exact(_))) {
            self.origins.iter().map(AllowedOrigin::as_written).collect::<Vec<_>>().join(",")
        } else if let [AllowedOrigin::Pattern(p, _)] = self.origins.as_slice() {
            format!("/{p}/")
        } else {
            let alts: Vec<String> = self
                .origins
                .iter()
                .map(|o| match o {
                    AllowedOrigin::Pattern(p, _) => format!("(?:{p})"),
                    other => regex::escape(&other.as_written()),
                })
                .collect();
            format!("/{}/", alts.join("|"))
        };
        vec![
            ("http.cors.allow-credentials".into(), "false".into()),
            ("http.cors.allow-headers".into(), self.headers.join(",")),
            ("http.cors.allow-methods".into(), self.methods.join(",")),
            ("http.cors.allow-origin".into(), origin),
            ("http.cors.enabled".into(), "true".into()),
            ("http.cors.max-age".into(), self.max_age.to_string()),
        ]
    }
}

/// Elasticsearch's `isSameOrigin`: the origin without its `http(s)://` is the `Host` header.
fn same_origin(origin: &str, host: Option<&str>) -> bool {
    let Some(host) = host.filter(|h| !h.is_empty()) else { return false };
    let bare = origin.strip_prefix("https://").or_else(|| origin.strip_prefix("http://")).unwrap_or(origin);
    bare == host
}

/// Whether a request is a CORS preflight (Elasticsearch's `isPreflightRequest`).
pub fn is_preflight(method: &str, has_origin: bool, has_request_method: bool) -> bool {
    method == "OPTIONS" && has_origin && has_request_method
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cors(origins: &[&str]) -> Cors {
        Cors::new(&origins.iter().map(|s| s.to_string()).collect::<Vec<_>>(), None, None, None).unwrap()
    }

    #[test]
    fn origins_are_read_as_browsers_send_them() {
        assert_eq!(normalize_origin("HTTPS://Shop.Example.com:443/").unwrap(), "https://shop.example.com");
        assert_eq!(normalize_origin("http://localhost:3000").unwrap(), "http://localhost:3000");
        assert_eq!(normalize_origin("http://[::1]:8080").unwrap(), "http://[::1]:8080");
        for bad in ["shop.example.com", "ftp://x", "https://x/path", "https://", "https://a b", "https://x:0", "https://x:99999", "https://u@x"] {
            assert!(normalize_origin(bad).is_err(), "{bad}");
        }
        assert!(AllowedOrigin::parse("/https://(.+\\.)?example\\.com/").is_ok());
        assert!(AllowedOrigin::parse("/(/").is_err());
    }

    #[test]
    fn allowed_origins_as_elasticsearch_decides() {
        let c = cors(&["https://shop.example.com", "/https://.*\\.preview\\.example/"]);
        assert!(c.allows("https://shop.example.com", None));
        assert!(c.allows("https://pr-12.preview.example", None));
        // A pattern matches the whole origin, never a part of it.
        assert!(!c.allows("https://pr-12.preview.example.evil.com", None));
        assert!(!c.allows("https://evil.com", None));
        // Same origin as the request's host (isSameOrigin).
        assert!(c.allows("http://c1.search.example:9200", Some("c1.search.example:9200")));
        let h = c.answer_headers("https://shop.example.com", None, false).unwrap();
        assert_eq!(h[0], ("access-control-allow-origin", "https://shop.example.com".to_string()));
        assert!(h.contains(&("vary", "origin".to_string())));
        let p = c.preflight_headers("https://shop.example.com", None).unwrap();
        assert_eq!(p.iter().filter(|(k, _)| *k == "access-control-allow-methods").count(), 6);
        assert!(p.contains(&("access-control-allow-headers", "X-Elastic-Client-Meta".to_string())));
        assert!(p.contains(&("access-control-max-age", "1728000".to_string())));
        assert!(c.preflight_headers("https://evil.com", None).is_none());
        let any = cors(&["*"]);
        assert_eq!(any.answer_headers("https://anyone.example", None, false).unwrap()[0].1, "*");
        let shown = c.settings();
        assert!(shown.contains(&("http.cors.allow-origin".into(), "/https://shop\\.example\\.com|(?:https://.*\\.preview\\.example)/".into())), "{shown:?}");
        assert!(shown.contains(&("http.cors.allow-credentials".into(), "false".into())));
    }
}
