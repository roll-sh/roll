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
//!
//! A request that was swapped into comes back with a [`Scrub`]: the proxy feeds it the response,
//! and it says when the host echoed a value back ([`Echoed`]), so the proxy stops the response.

use crate::Value;
use base64::Engine as _;
use zeroize::Zeroizing;

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
    /// The placeholder was issued to another consumer than the proxy serves: likely a leaked one
    /// (a log line, an error page) used from someone else's proxy. roll secrets records an alert.
    #[error("the placeholder wasn't issued to this consumer")]
    WrongConsumer,
    #[error("{0}")]
    WrongPlace(String),
    /// The key's binding names no header or query parameter, and the placeholder is an untrusted
    /// consumer's: the code holding it would pick a header the host might echo.
    #[error("the key's binding names no header or query parameter, which an untrusted consumer's placeholder needs")]
    PlacementRequired,
    /// Whoever asked for it may no longer, or the environment is protected, frozen or gone.
    #[error("{0}")]
    NotAllowed(String),
    /// Whoever set it up can no longer reach it: the consumer's maker can no longer reveal there,
    /// or whoever bound the key to its hosts can no longer manage it, or either can no longer sign
    /// in ([`crate::Error::MakerUnreachable`] over HTTP).
    #[error("{0}")]
    MakerUnreachable(String),
    /// The key doesn't resolve, or isn't one a placeholder can stand for any more.
    #[error("{0}")]
    Unusable(String),
    #[error("{0}")]
    Failed(String),
}

/// A placeholder swapped: its value, and what it stands for, which is what [`Echoed`] names.
#[derive(Debug)]
pub struct Swap {
    pub value: Value,
    pub key: String,
    /// The placeholder's record (`phd_…`), never the placeholder.
    pub placeholder: String,
}

/// What swaps a placeholder for its value: [`Secrets`](crate::Secrets) in process (blocking: call
/// it off the async runtime). `consumer` is the one the proxy serves (its interception CA's:
/// `apps:<project>/<env>`, `preview:…`, `sandbox:…`), never one the request names: a placeholder
/// swaps only for the consumer it was issued to. Checked again on every call; nothing is cached.
pub trait Substitute: Send + Sync {
    fn substitute(&self, placeholder: &str, consumer: &str, host: &str, at: Position<'_>) -> Result<Swap, Refusal>;
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

/// Swap every placeholder in `req`'s headers and query for its value, or none, and answer the
/// [`Scrub`] for its response (empty when nothing was swapped, which leaves the request as it
/// was). `consumer` is the one the proxy serves (its interception CA's), never taken from the
/// request: a placeholder issued to another is refused ([`Refusal::WrongConsumer`]). A request
/// that was swapped into asks for `Accept-Encoding: identity`, so the response can be scrubbed as
/// it arrives.
pub fn filter(req: &mut Outgoing<'_>, consumer: &str, with: &dyn Substitute, limits: &Limits) -> Result<Scrub, Blocked> {
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
        return Ok(Scrub::new(req.host, vec![]));
    }
    if found > limits.placeholders {
        return Err(Blocked::TooLarge("too many placeholders"));
    }
    // Then each swap, into copies: the request changes only if every one went through.
    let mut swaps = vec![];
    let mut headers = req.headers.clone();
    for (n, v) in headers.iter_mut() {
        let at = || format!("header {n}");
        let mut out = v.clone();
        for t in tokens(v).map_err(|w| refused(at(), w))? {
            let swap = with.substitute(t, consumer, req.host, Position { place: Place::Header, name: n.as_str(), value: v.as_str() }).map_err(|w| refused(at(), w))?;
            let value = swap.value.expose();
            if value.len() > limits.value_bytes {
                return Err(refused(at(), Refusal::Unusable("the value is too large to send".into())));
            }
            if value.bytes().any(|b| matches!(b, b'\r' | b'\n' | 0)) {
                return Err(refused(at(), Refusal::Unusable("the value has a line break or NUL, which a header can't carry".into())));
            }
            out = out.replace(t, value);
            swaps.push(swap);
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
                    let swap = with.substitute(t, consumer, req.host, Position { place: Place::Query, name: n, value: v }).map_err(|w| refused(at(), w))?;
                    if swap.value.expose().len() > limits.value_bytes {
                        return Err(refused(at(), Refusal::Unusable("the value is too large to send".into())));
                    }
                    parts.push(format!("{n}={}", v.replace(*t, &encode(swap.value.expose().as_bytes(), true))));
                    swaps.push(swap);
                }
                _ => return Err(refused(at(), Refusal::WrongPlace("one placeholder a query parameter at most".into()))),
            }
        }
        query = Some(parts.join("&"));
    }
    // The response is scrubbed as it arrives, which a compressed one can't be.
    headers.retain(|(n, _)| !n.eq_ignore_ascii_case("accept-encoding"));
    headers.push(("accept-encoding".into(), "identity".into()));
    *req.headers = headers;
    if let (Some(q), Some(new)) = (req.query.as_deref_mut(), query) {
        *q = new;
    }
    Ok(Scrub::new(req.host, swaps))
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

/// Percent-encoding for a query value: everything but RFC 3986's unreserved characters, in upper
/// or lower case hex.
fn encode(v: &[u8], upper: bool) -> String {
    // Never grown: a reallocation would leave a copy of the value behind.
    let mut out = String::with_capacity(v.len() * 3);
    for &b in v {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else if upper {
            out.push_str(&format!("%{b:02X}"));
        } else {
            out.push_str(&format!("%{b:02x}"));
        }
    }
    out
}

/// A value this short isn't looked for in a response: it would match ordinary bytes, and every
/// match ends the placeholder.
pub const SCRUB_MIN_BYTES: usize = 8;

/// A response echoed a value swapped into its request. Names the key, the host and the
/// placeholder's record (`phd_…`), never the value or the placeholder. The proxy stops the response
/// (the consumer gets a 502) and reports it with [`Secrets::echoed`](crate::Secrets::echoed).
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{host} echoed {key} back in its response")]
pub struct Echoed {
    pub key: String,
    pub host: String,
    pub placeholder: String,
}

/// One swapped value's forms, as a response might carry them back.
struct Forms {
    key: String,
    placeholder: String,
    forms: Vec<Zeroizing<Vec<u8>>>,
}

/// The scrub of one request's response (docs/secrets.md, "the proxy's contract", step 6): the
/// forms of each value [`filter`] swapped into it (as is, base64 standard and URL-safe, padded and
/// not, and percent-encoded), looked for in the response's headers ([`Scrub::headers`]) and its
/// body as it arrives ([`Scrub::chunk`]), with the last (longest form − 1) bytes carried over so a
/// value split between two reads is found. No I/O. In memory only: `Debug` shows no form, and
/// they're wiped on drop. Bounded by the filter's [`Limits`]: at most 16 values of 16 KiB.
pub struct Scrub {
    host: String,
    swapped: usize,
    values: Vec<Forms>,
    /// The longest form, less one: the bytes carried between chunks.
    window: usize,
    carry: Zeroizing<Vec<u8>>,
    echoed: Option<Echoed>,
}

impl std::fmt::Debug for Scrub {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let keys: Vec<&str> = self.values.iter().map(|v| v.key.as_str()).collect();
        f.debug_struct("Scrub").field("host", &self.host).field("keys", &keys).field("echoed", &self.echoed).finish_non_exhaustive()
    }
}

impl Scrub {
    fn new(host: &str, swaps: Vec<Swap>) -> Scrub {
        use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
        let swapped = swaps.len();
        let mut values: Vec<Forms> = vec![];
        for s in swaps {
            let v = s.value.expose().as_bytes();
            if v.len() < SCRUB_MIN_BYTES || values.iter().any(|f| f.placeholder == s.placeholder) {
                continue;
            }
            let encoded = [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD].map(|e| e.encode(v)).into_iter().chain([encode(v, true), encode(v, false)]);
            let mut forms: Vec<Zeroizing<Vec<u8>>> = vec![Zeroizing::new(v.to_vec())];
            for f in encoded.map(|f| Zeroizing::new(f.into_bytes())) {
                if !forms.iter().any(|g| g.as_slice() == f.as_slice()) {
                    forms.push(f);
                }
            }
            values.push(Forms { key: s.key, placeholder: s.placeholder, forms });
        }
        let window = values.iter().flat_map(|v| v.forms.iter()).map(|f| f.len()).max().unwrap_or(1) - 1;
        Scrub { host: host.to_string(), swapped, values, window, carry: Zeroizing::new(Vec::new()), echoed: None }
    }

    /// How many placeholders were swapped in the request: 0 left it as it was.
    pub fn swapped(&self) -> usize {
        self.swapped
    }

    /// Whether `bytes` hold a form of a swapped value; once one was found, every call says so.
    fn find(&mut self, bytes: &[u8]) -> Result<(), Echoed> {
        if self.echoed.is_none()
            && let Some(v) = self.values.iter().find(|v| v.forms.iter().any(|f| memchr::memmem::find(bytes, f).is_some()))
        {
            self.echoed = Some(Echoed { key: v.key.clone(), host: self.host.clone(), placeholder: v.placeholder.clone() });
        }
        self.echoed.clone().map_or(Ok(()), Err)
    }

    /// The response's headers: each name and value on its own.
    pub fn headers(&mut self, headers: &axum::http::HeaderMap) -> Result<(), Echoed> {
        for (n, v) in headers {
            self.find(n.as_str().as_bytes())?;
            self.find(v.as_bytes())?;
        }
        self.echoed.clone().map_or(Ok(()), Err)
    }

    /// The next bytes of the response's body, as they arrive (never inflated: the request asked
    /// for `identity`, and a compressed response is refused, not scrubbed).
    pub fn chunk(&mut self, bytes: &[u8]) -> Result<(), Echoed> {
        if self.values.is_empty() {
            return Ok(());
        }
        // Where the last chunk ends and this one starts: the carried bytes and enough of this
        // one to finish a form that started there.
        let mut seam = Zeroizing::new(Vec::with_capacity(self.carry.len() + self.window.min(bytes.len())));
        seam.extend_from_slice(&self.carry);
        seam.extend_from_slice(&bytes[..self.window.min(bytes.len())]);
        self.find(&seam)?;
        self.find(bytes)?;
        if bytes.len() >= self.window {
            self.carry = Zeroizing::new(bytes[bytes.len() - self.window..].to_vec());
        } else {
            let mut all = Zeroizing::new(Vec::with_capacity(self.carry.len() + bytes.len()));
            all.extend_from_slice(&self.carry);
            all.extend_from_slice(bytes);
            self.carry = Zeroizing::new(all[all.len().saturating_sub(self.window)..].to_vec());
        }
        Ok(())
    }
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
        fn substitute(&self, placeholder: &str, _consumer: &str, host: &str, at: Position<'_>) -> Result<Swap, Refusal> {
            self.calls.lock().unwrap().push((host.to_string(), at.place, at.name.to_string()));
            let (_, v) = self.known.iter().find(|(p, _)| p == placeholder).ok_or(Refusal::Unknown)?;
            if host != "api.example.com" {
                return Err(Refusal::WrongHost(host.into()));
            }
            match (at.place, at.name) {
                (Place::Header, "authorization") | (Place::Query, "key") => Ok(Swap { value: Value::new(*v), key: "K".into(), placeholder: "phd_1".into() }),
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
        filter(&mut Outgoing { host, path, headers, query }, "sandbox:sbx_1", with, limits).map(|s| s.swapped())
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
        // Its response is asked for uncompressed, so it can be scrubbed as it arrives.
        assert_eq!(h[1..], [("accept".to_string(), "*/*".to_string()), ("accept-encoding".into(), "identity".into())]);

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
