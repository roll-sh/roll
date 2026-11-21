//! The egress filter a proxy embeds (docs/secrets.md, "Using a key without seeing it"): a
//! sandbox's or a preview's request carries placeholders (`rlph_…`) where its code would put a key,
//! and [`filter`] swaps each for the key's value through [`Substitute`] (roll secrets'
//! [`Secrets::substitute`](crate::Secrets::substitute)), which decides whether this host and this
//! place may get it.
//!
//! No I/O here. A placeholder is swapped only in headers and the query string, never in the path
//! (or the body, which isn't passed in), and only for the request's own host. Anything starting
//! `rlph_` that doesn't swap is left as it is and the request is [`Blocked`]: the proxy must not
//! send it. Swaps are all or nothing, sizes are bounded ([`Limits`]), and nothing here logs or
//! returns a value or a placeholder.

use crate::Value;

/// What a placeholder starts with (one of `roll_auth::credentials::SECRET_PREFIXES`).
pub const PREFIX: &str = "rlph_";
/// A placeholder's length: the prefix, 34 random characters and a 6-character checksum.
pub const LEN: usize = PREFIX.len() + 40;

/// Where in a request a placeholder is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Header,
    Query,
}

/// A placeholder's position, as [`Substitute`] is asked about it: a header's name and its whole
/// value, or a query parameter's name and its whole (raw) value. Both hold the placeholder, never
/// the value it stands for.
#[derive(Clone, Copy, Debug)]
pub struct Position<'a> {
    pub place: Place,
    pub name: &'a str,
    pub value: &'a str,
}

/// Why a placeholder wasn't swapped. Never holds a value or a placeholder.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("not a live placeholder")]
    Unknown,
    #[error("the placeholder expired")]
    Expired,
    #[error("the placeholder was revoked")]
    Revoked,
    /// Likely an exfiltration attempt: roll secrets records an alert.
    #[error("{0} isn't a host the key is bound to")]
    WrongHost(String),
    #[error("{0}")]
    WrongPlace(String),
    /// Whoever asked for it may no longer, or the environment is protected, frozen or gone.
    #[error("{0}")]
    NotAllowed(String),
    /// Whoever set it up (the consumer's maker) can no longer reveal there, or sign in
    /// ([`crate::Error::MakerUnreachable`] over HTTP).
    #[error("{0}")]
    MakerUnreachable(String),
    /// The key doesn't resolve, or isn't one a placeholder can stand for any more.
    #[error("{0}")]
    Unusable(String),
    #[error("{0}")]
    Failed(String),
}

/// What swaps a placeholder for its value: [`Secrets`](crate::Secrets) in process (blocking: call
/// it off the async runtime). Checked again on every call; nothing is cached.
pub trait Substitute: Send + Sync {
    fn substitute(&self, placeholder: &str, host: &str, at: Position<'_>) -> Result<Value, Refusal>;
}

/// The bounds a request must fit before anything is asked.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Header names and values together.
    pub header_bytes: usize,
    pub query_bytes: usize,
    pub path_bytes: usize,
    /// Placeholders in one request.
    pub placeholders: usize,
    /// One swapped value.
    pub value_bytes: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits { header_bytes: 64 << 10, query_bytes: 16 << 10, path_bytes: 8 << 10, placeholders: 16, value_bytes: 16 << 10 }
    }
}

/// An outgoing request as the filter sees it. `host` is the one the proxy connects to (the name
/// it verifies TLS for), never one the client wrote in a header. `query` is the raw query string,
/// without `?`.
#[derive(Debug)]
pub struct Outgoing<'a> {
    pub host: &'a str,
    pub path: &'a str,
    pub headers: &'a mut Vec<(String, String)>,
    pub query: Option<&'a mut String>,
}

/// Why a request mustn't go out. Says where and why, never a value or a placeholder.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Blocked {
    #[error("too large to check for placeholders: {0}")]
    TooLarge(&'static str),
    #[error("a placeholder in {at}: {why}")]
    Refused { at: String, why: Refusal },
}

/// Swap every placeholder in `req`'s headers and query for its value, or none: answers how many
/// were swapped (0 leaves the request as it was).
pub fn filter(req: &mut Outgoing<'_>, with: &dyn Substitute, limits: &Limits) -> Result<usize, Blocked> {
    let header_bytes: usize = req.headers.iter().map(|(n, v)| n.len() + v.len()).sum();
    if header_bytes > limits.header_bytes {
        return Err(Blocked::TooLarge("headers"));
    }
    if req.query.as_deref().is_some_and(|q| q.len() > limits.query_bytes) {
        return Err(Blocked::TooLarge("the query"));
    }
    if req.path.len() > limits.path_bytes {
        return Err(Blocked::TooLarge("the path"));
    }
    let refused = |at: String, why: Refusal| Blocked::Refused { at, why };
    if req.path.contains(PREFIX) {
        return Err(refused("the path".into(), Refusal::WrongPlace("a placeholder is never swapped in the path".into())));
    }
    for (n, _) in req.headers.iter() {
        if n.contains(PREFIX) {
            return Err(refused("a header's name".into(), Refusal::WrongPlace("a placeholder is never swapped in a header's name".into())));
        }
    }
    // First every placeholder's shape, so nothing is asked about a request that can't go anyway.
    let mut found = 0;
    for (n, v) in req.headers.iter() {
        found += tokens(v).map_err(|w| refused(format!("header {n}"), w))?.len();
    }
    for (n, v) in req.query.as_deref().map(String::as_str).map(split_query).unwrap_or_default() {
        if n.contains(PREFIX) {
            return Err(refused("a query parameter's name".into(), Refusal::WrongPlace("a placeholder is never swapped in a query parameter's name".into())));
        }
        found += tokens(v).map_err(|w| refused(format!("query parameter {n}"), w))?.len();
    }
    if found == 0 {
        return Ok(0);
    }
    if found > limits.placeholders {
        return Err(Blocked::TooLarge("too many placeholders"));
    }
    // Then each swap, into copies: the request changes only if every one went through.
    let mut headers = req.headers.clone();
    for (n, v) in headers.iter_mut() {
        let at = || format!("header {n}");
        let mut out = v.clone();
        for t in tokens(v).map_err(|w| refused(at(), w))? {
            let value = with.substitute(t, req.host, Position { place: Place::Header, name: n.as_str(), value: v.as_str() }).map_err(|w| refused(at(), w))?;
            let value = value.expose();
            if value.len() > limits.value_bytes {
                return Err(refused(at(), Refusal::Unusable("the value is too large to send".into())));
            }
            if value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
                return Err(refused(at(), Refusal::Unusable("the value has a line break or NUL, which a header can't carry".into())));
            }
            out = out.replace(t, value);
        }
        *v = out;
    }
    let mut query = None;
    if let Some(q) = req.query.as_deref() {
        let mut parts = Vec::new();
        for raw in q.split('&') {
            let (n, v) = raw.split_once('=').unwrap_or((raw, ""));
            let at = || format!("query parameter {n}");
            match tokens(v).map_err(|w| refused(at(), w))?.as_slice() {
                [] => parts.push(raw.to_string()),
                [t] => {
                    let value = with.substitute(t, req.host, Position { place: Place::Query, name: n, value: v }).map_err(|w| refused(at(), w))?;
                    if value.expose().len() > limits.value_bytes {
                        return Err(refused(at(), Refusal::Unusable("the value is too large to send".into())));
                    }
                    parts.push(format!("{n}={}", v.replace(*t, &encode(value.expose()))));
                }
                _ => return Err(refused(at(), Refusal::WrongPlace("one placeholder a query parameter at most".into()))),
            }
        }
        query = Some(parts.join("&"));
    }
    *req.headers = headers;
    if let (Some(q), Some(new)) = (req.query.as_deref_mut(), query) {
        *q = new;
    }
    Ok(found)
}

/// The placeholders in `s`, each a well-formed one; anything else starting `rlph_` refuses it.
fn tokens(s: &str) -> Result<Vec<&str>, Refusal> {
    let mut out = vec![];
    let mut from = 0;
    while let Some(i) = s[from..].find(PREFIX) {
        let start = from + i;
        let end = start + LEN;
        let t = s.get(start..end).filter(|t| roll_auth::credentials::checksum_ok(t) && s[end..].bytes().next().is_none_or(|b| !b.is_ascii_alphanumeric() && b != b'_'));
        match t {
            Some(t) => {
                if !out.contains(&t) {
                    out.push(t);
                }
                from = end;
            }
            None => return Err(Refusal::Unknown),
        }
    }
    Ok(out)
}

/// `a=1&b&c=x` as (name, value) pairs, raw (a placeholder needs no decoding).
fn split_query(q: &str) -> Vec<(&str, &str)> {
    q.split('&').map(|p| p.split_once('=').unwrap_or((p, ""))).collect()
}

/// Percent-encoding for a query value: everything but RFC 3986's unreserved characters.
fn encode(v: &str) -> String {
    let mut out = String::with_capacity(v.len());
    for b in v.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Swaps placeholders it knows for `api.example.com`, in a header named `authorization` or
    /// the `key` parameter; records every call.
    struct Fake {
        known: Vec<(String, &'static str)>,
        calls: Mutex<Vec<(String, Place, String)>>,
    }

    impl Substitute for Fake {
        fn substitute(&self, placeholder: &str, host: &str, at: Position<'_>) -> Result<Value, Refusal> {
            self.calls.lock().unwrap().push((host.to_string(), at.place, at.name.to_string()));
            let (_, v) = self.known.iter().find(|(p, _)| p == placeholder).ok_or(Refusal::Unknown)?;
            if host != "api.example.com" {
                return Err(Refusal::WrongHost(host.into()));
            }
            match (at.place, at.name) {
                (Place::Header, "authorization") | (Place::Query, "key") => Ok(Value::new(*v)),
                _ => Err(Refusal::WrongPlace("not where the key goes".into())),
            }
        }
    }

    fn fake(values: &[&'static str]) -> (Fake, Vec<String>) {
        let known: Vec<(String, &'static str)> = values.iter().map(|v| (roll_auth::credentials::new_secret(PREFIX), *v)).collect();
        let ps = known.iter().map(|(p, _)| p.clone()).collect();
        (Fake { known, calls: Mutex::new(vec![]) }, ps)
    }

    type Headers = Vec<(String, String)>;

    fn run(with: &Fake, host: &str, path: &str, headers: &mut Vec<(String, String)>, query: Option<&mut String>, limits: &Limits) -> Result<usize, Blocked> {
        filter(&mut Outgoing { host, path, headers, query }, with, limits)
    }

    #[test]
    fn placeholders_are_swapped_only_where_and_for_whom_they_may_be() {
        let (f, p) = fake(&["sk_test_fake_one", "fake two&=/"]);
        let lim = Limits::default();
        // A header and a query parameter: swapped, the query value encoded; other parts untouched.
        let mut h = vec![("authorization".to_string(), format!("Bearer {}", p[0])), ("accept".into(), "*/*".into())];
        let mut q = format!("a=1&key={}&flag", p[1]);
        assert_eq!(run(&f, "api.example.com", "/v1/charges", &mut h, Some(&mut q), &lim), Ok(2));
        assert_eq!(h[0].1, "Bearer sk_test_fake_one");
        assert_eq!(q, "a=1&key=fake%20two%26%3D%2F&flag");

        // Nothing to swap: no call, nothing changed.
        let mut h = vec![("accept".to_string(), "*/*".to_string())];
        let calls = f.calls.lock().unwrap().len();
        assert_eq!(run(&f, "api.example.com", "/", &mut h, None, &lim), Ok(0));
        assert_eq!(f.calls.lock().unwrap().len(), calls);

        // Another host, another header, the path, a forged or unknown placeholder: refused, and
        // the request keeps its placeholders (all or nothing: the good one isn't swapped either).
        let unknown = roll_auth::credentials::new_secret(PREFIX);
        let mut forged = p[0].clone();
        forged.replace_range(10..11, if &forged[10..11] == "a" { "b" } else { "a" });
        let cases: Vec<(&str, String, Headers, Refusal)> = vec![
            ("evil.example", "/".into(), vec![("authorization".into(), p[0].clone())], Refusal::WrongHost("evil.example".into())),
            ("api.example.com", "/".into(), vec![("x-other".into(), p[0].clone())], Refusal::WrongPlace("not where the key goes".into())),
            ("api.example.com", "/".into(), vec![("authorization".into(), p[0].clone()), ("x-two".into(), unknown.clone())], Refusal::Unknown),
            ("api.example.com", "/".into(), vec![("authorization".into(), forged)], Refusal::Unknown),
            ("api.example.com", "/".into(), vec![("authorization".into(), format!("{PREFIX}short"))], Refusal::Unknown),
            ("api.example.com", format!("/v1/{}", p[0]), vec![("authorization".into(), p[0].clone())], Refusal::WrongPlace("a placeholder is never swapped in the path".into())),
        ];
        for (host, path, headers, why) in cases {
            let before = headers.clone();
            let mut h = headers;
            match run(&f, host, &path, &mut h, None, &lim) {
                Err(Blocked::Refused { why: w, at }) => {
                    assert_eq!(w, why, "{host} {path}");
                    assert!(!at.contains(PREFIX) && !w.to_string().contains(PREFIX) && !w.to_string().contains("fake"), "{at}: {w}");
                }
                other => panic!("{host} {path}: {other:?}"),
            }
            assert_eq!(h, before, "a refused request is left as it was");
        }
        // A value that would split a header is never written into one.
        let (f2, p2) = fake(&["fake\r\nx-injected: 1"]);
        let mut h = vec![("authorization".to_string(), p2[0].clone())];
        assert!(matches!(run(&f2, "api.example.com", "/", &mut h, None, &lim), Err(Blocked::Refused { why: Refusal::Unusable(_), .. })));
    }

    #[test]
    fn a_request_past_the_limits_is_refused_before_anything_is_asked() {
        let (f, p) = fake(&["sk_test_fake_one"]);
        let lim = Limits { header_bytes: 256, query_bytes: 64, path_bytes: 32, placeholders: 2, value_bytes: 8 };
        let big = vec![("authorization".to_string(), p[0].clone()), ("x-pad".into(), "x".repeat(256))];
        assert_eq!(run(&f, "api.example.com", "/", &mut big.clone(), None, &lim), Err(Blocked::TooLarge("headers")));
        let mut q = format!("key={}&pad={}", p[0], "x".repeat(64));
        assert_eq!(run(&f, "api.example.com", "/", &mut vec![], Some(&mut q), &lim), Err(Blocked::TooLarge("the query")));
        assert_eq!(run(&f, "api.example.com", &"/x".repeat(20), &mut vec![], None, &lim), Err(Blocked::TooLarge("the path")));
        let three = (0..3).map(|i| (format!("x-{i}"), roll_auth::credentials::new_secret(PREFIX))).collect::<Vec<_>>();
        assert_eq!(run(&f, "api.example.com", "/", &mut three.clone(), None, &lim), Err(Blocked::TooLarge("too many placeholders")));
        assert!(f.calls.lock().unwrap().is_empty(), "nothing was asked");
        // A value over its bound isn't sent.
        let mut h = vec![("authorization".to_string(), p[0].clone())];
        assert!(matches!(run(&f, "api.example.com", "/", &mut h, None, &lim), Err(Blocked::Refused { why: Refusal::Unusable(_), .. })));
        assert_eq!(h[0].1, p[0]);
    }
}
