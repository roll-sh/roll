//! OpenID Connect ID tokens (JWTs, RS256 or ES256) for workload identities (`identities.rs`): a
//! CI job's or a cluster's own short-lived token, verified against its issuer's published keys.
//!
//! The same rules as storage's web identity (`s3-server/src/sts/oidc.rs`, which this follows):
//!
//! - Only an issuer an identity names is ever fetched from, over the guarded client
//!   ([`crate::Deliver::get`]: HTTPS, public addresses, no redirects, bounded reads), so a token
//!   can't make roll reach an address of its choosing.
//! - Keys come from the issuer's discovery document's `jwks_uri`, cached for an hour and fetched
//!   again early (at most every 30 s) when a token names a key id the cache doesn't have. If the
//!   issuer is down, keys up to a day old keep working.
//! - Checked: the signature, `iss` exactly, `aud`, `exp` and `nbf` with 60 s of leeway. The
//!   identity checks `sub` and its other claims.
//!
//! Public keys are cached, never a token or what it maps to.

use crate::{Error, Result};
use serde_json::{Map, Value as Json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The longest token taken.
pub const MAX_JWT_BYTES: usize = 16 * 1024;
const MAX_JWKS_BYTES: usize = 256 * 1024;
const MAX_DISCOVERY_BYTES: usize = 64 * 1024;
const MAX_KEYS: usize = 100;
const TTL: Duration = Duration::from_secs(3600);
const STALE_OK: Duration = Duration::from_secs(86_400);
const REFETCH_MIN: Duration = Duration::from_secs(30);
const LEEWAY_S: i64 = 60;
const LIFETIME_MAX_S: i64 = 86_400;
const MAX_CACHED: usize = 1000;

/// A public key from a JWKS.
#[derive(Clone, Debug)]
enum Key {
    Rsa { kid: Option<String>, n: Vec<u8>, e: Vec<u8> },
    /// An uncompressed SEC1 point.
    P256 { kid: Option<String>, point: Vec<u8> },
}

impl Key {
    fn kid(&self) -> Option<&str> {
        match self {
            Key::Rsa { kid, .. } | Key::P256 { kid, .. } => kid.as_deref(),
        }
    }

    fn verify(&self, alg: Alg, msg: &[u8], sig: &[u8]) -> bool {
        use ring::signature;
        match (self, alg) {
            (Key::Rsa { n, e, .. }, Alg::Rs256) => signature::RsaPublicKeyComponents { n: n.as_slice(), e: e.as_slice() }.verify(&signature::RSA_PKCS1_2048_8192_SHA256, msg, sig).is_ok(),
            (Key::P256 { point, .. }, Alg::Es256) => signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, point).verify(msg, sig).is_ok(),
            _ => false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Alg {
    Rs256,
    Es256,
}

fn b64(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

/// The usable signing keys of a JWKS document.
fn parse_jwks(doc: &[u8]) -> std::result::Result<Vec<Key>, String> {
    let v: Json = serde_json::from_slice(doc).map_err(|e| format!("the JWKS isn't JSON: {e}"))?;
    let keys = v.get("keys").and_then(Json::as_array).ok_or("the JWKS has no keys")?;
    let mut out = Vec::new();
    for k in keys.iter().take(MAX_KEYS) {
        let s = |n: &str| k.get(n).and_then(Json::as_str);
        if s("use").is_some_and(|u| u != "sig") {
            continue;
        }
        let kid = s("kid").map(str::to_string);
        match (s("kty"), s("alg")) {
            (Some("RSA"), None | Some("RS256")) => {
                let (Some(n), Some(e)) = (s("n").and_then(b64), s("e").and_then(b64)) else { continue };
                out.push(Key::Rsa { kid, n, e });
            }
            (Some("EC"), None | Some("ES256")) if s("crv") == Some("P-256") => {
                let (Some(x), Some(y)) = (s("x").and_then(b64), s("y").and_then(b64)) else { continue };
                if x.len() != 32 || y.len() != 32 {
                    continue;
                }
                let mut point = vec![4u8];
                point.extend_from_slice(&x);
                point.extend_from_slice(&y);
                out.push(Key::P256 { kid, point });
            }
            _ => {}
        }
    }
    Ok(out)
}

fn invalid(m: impl Into<String>) -> Error {
    Error::Unauthorized(m.into())
}

/// A JWT, decoded but not verified.
pub(crate) struct Jwt<'a> {
    alg: Alg,
    kid: Option<String>,
    pub claims: Map<String, Json>,
    signed: &'a str,
    signature: Vec<u8>,
}

impl<'a> Jwt<'a> {
    pub fn parse(token: &'a str) -> Result<Self> {
        let token = token.trim();
        if token.len() > MAX_JWT_BYTES {
            return Err(invalid("the identity token is too long"));
        }
        let mut parts = token.split('.');
        let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return Err(invalid("the identity token isn't a signed JWT"));
        };
        let header: Json = b64(h).and_then(|b| serde_json::from_slice(&b).ok()).ok_or_else(|| invalid("the identity token's header isn't valid"))?;
        let alg = match header.get("alg").and_then(Json::as_str) {
            Some("RS256") => Alg::Rs256,
            Some("ES256") => Alg::Es256,
            Some(a) => return Err(invalid(format!("the identity token is signed with {a}: RS256 and ES256 are taken"))),
            None => return Err(invalid("the identity token's header has no alg")),
        };
        if header.get("crit").is_some() {
            return Err(invalid("the identity token has critical header parameters"));
        }
        let kid = header.get("kid").and_then(Json::as_str).map(str::to_string);
        let claims = match b64(p).and_then(|b| serde_json::from_slice::<Json>(&b).ok()) {
            Some(Json::Object(m)) => m,
            _ => return Err(invalid("the identity token's claims aren't valid")),
        };
        let signature = b64(s).ok_or_else(|| invalid("the identity token's signature isn't valid"))?;
        let signed = &token[..h.len() + 1 + p.len()];
        Ok(Self { alg, kid, claims, signed, signature })
    }

    pub fn str(&self, name: &str) -> Option<&str> {
        self.claims.get(name).and_then(Json::as_str)
    }

    fn time(&self, name: &str) -> Result<Option<i64>> {
        match self.claims.get(name) {
            None => Ok(None),
            Some(v) => v.as_f64().map(|f| Some(f as i64)).ok_or_else(|| invalid(format!("the identity token's {name} isn't a number"))),
        }
    }

    fn audiences(&self) -> Vec<&str> {
        match self.claims.get("aud") {
            Some(Json::String(s)) => vec![s.as_str()],
            Some(Json::Array(a)) => a.iter().filter_map(Json::as_str).collect(),
            _ => Vec::new(),
        }
    }
}

struct Cached {
    keys: Arc<Vec<Key>>,
    fetched: Instant,
}

/// Issuers' cached keys.
#[derive(Default)]
pub(crate) struct Keys {
    cache: Mutex<HashMap<String, Cached>>,
    /// One fetch per issuer at a time: logins that need it wait for its answer.
    fetching: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// When an issuer's last fetch failed: none is tried again for [`REFETCH_MIN`], so a login
    /// flood while it's down costs no fetches.
    failed: Mutex<HashMap<String, Instant>>,
}

impl Keys {
    fn download(&self, get: &dyn crate::Deliver, issuer: &str) -> Result<Vec<Key>> {
        let url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
        let doc: Json = serde_json::from_slice(&get.get(&url, MAX_DISCOVERY_BYTES)?).map_err(|_| Error::Link(format!("{url} isn't JSON")))?;
        let jwks = doc.get("jwks_uri").and_then(Json::as_str).ok_or_else(|| Error::Link(format!("{url} has no jwks_uri")))?;
        parse_jwks(&get.get(jwks, MAX_JWKS_BYTES)?).map_err(Error::Link)
    }

    /// An issuer's keys: cached, or fetched when they're old (or `refresh` and not fetched in the
    /// last 30 s).
    fn keys(&self, get: &dyn crate::Deliver, issuer: &str, refresh: bool) -> Result<Arc<Vec<Key>>> {
        let fresh = |c: &Cached| {
            let age = c.fetched.elapsed();
            age < TTL && !(refresh && age >= REFETCH_MIN)
        };
        if let Some(c) = self.cache.lock().unwrap().get(issuer).filter(|c| fresh(c)) {
            return Ok(c.keys.clone());
        }
        let stale = || self.cache.lock().unwrap().get(issuer).filter(|c| c.fetched.elapsed() < STALE_OK).map(|c| c.keys.clone());
        let lock = self.fetching.lock().unwrap().entry(issuer.to_string()).or_default().clone();
        let _one = lock.lock().unwrap();
        // Someone else may just have fetched, or failed to.
        if let Some(c) = self.cache.lock().unwrap().get(issuer).filter(|c| c.fetched.elapsed() < REFETCH_MIN) {
            return Ok(c.keys.clone());
        }
        if self.failed.lock().unwrap().get(issuer).is_some_and(|at| at.elapsed() < REFETCH_MIN) {
            return stale().ok_or_else(|| Error::Link(format!("{issuer}'s keys are unavailable just now: try again shortly")));
        }
        match self.download(get, issuer) {
            Ok(keys) => {
                let keys = Arc::new(keys);
                let mut cache = self.cache.lock().unwrap();
                if cache.len() >= MAX_CACHED {
                    cache.clear();
                }
                cache.insert(issuer.to_string(), Cached { keys: keys.clone(), fetched: Instant::now() });
                self.failed.lock().unwrap().remove(issuer);
                Ok(keys)
            }
            Err(e) => {
                let mut failed = self.failed.lock().unwrap();
                if failed.len() >= MAX_CACHED {
                    failed.clear();
                }
                failed.insert(issuer.to_string(), Instant::now());
                drop(failed);
                match stale() {
                    Some(k) => {
                        tracing::warn!(issuer, "refreshing an OIDC issuer's keys failed; using the cached ones: {e}");
                        Ok(k)
                    }
                    None => Err(e),
                }
            }
        }
    }

    /// Verify `jwt`'s signature, issuer, audience and times at `now_s` (Unix seconds).
    pub fn verify(&self, get: &dyn crate::Deliver, jwt: &Jwt<'_>, issuer: &str, audience: &str, now_s: i64) -> Result<()> {
        if jwt.str("iss") != Some(issuer) {
            return Err(invalid("the identity token is from another issuer"));
        }
        let matching = |keys: &[Key]| {
            keys.iter()
                .filter(|k| match (&jwt.kid, k.kid()) {
                    (Some(want), Some(have)) => want == have,
                    _ => true,
                })
                .any(|k| k.verify(jwt.alg, jwt.signed.as_bytes(), &jwt.signature))
        };
        let keys = self.keys(get, issuer, false)?;
        let known_kid = jwt.kid.as_ref().is_none_or(|want| keys.iter().any(|k| k.kid() == Some(want)));
        if !(matching(&keys) || (!known_kid && matching(&self.keys(get, issuer, true)?))) {
            return Err(invalid("the identity token's signature doesn't verify"));
        }
        let exp = jwt.time("exp")?.ok_or_else(|| invalid("the identity token has no exp"))?;
        if now_s >= exp + LEEWAY_S {
            return Err(invalid("the identity token has expired"));
        }
        // A workload's token is short-lived; one good for days is a credential in its own right.
        if exp - now_s > LIFETIME_MAX_S {
            return Err(invalid("the identity token lives longer than a day"));
        }
        if jwt.time("nbf")?.is_some_and(|nbf| now_s + LEEWAY_S < nbf) {
            return Err(invalid("the identity token isn't valid yet"));
        }
        if !jwt.audiences().contains(&audience) {
            return Err(invalid(format!("the identity token isn't for audience {audience}")));
        }
        Ok(())
    }
}

/// Attempts per key in a sliding minute (in memory, per node: a cap on work, not an audit).
#[derive(Default)]
pub(crate) struct Window {
    hits: Mutex<HashMap<String, (Instant, u32)>>,
}

impl Window {
    /// Count an attempt for `key`; false past `max` in the current minute.
    pub fn allow(&self, key: &str, max: u32) -> bool {
        let mut hits = self.hits.lock().unwrap();
        if hits.len() >= 10_000 {
            hits.retain(|_, (at, _)| at.elapsed() < Duration::from_secs(60));
            if hits.len() >= 10_000 {
                hits.clear();
            }
        }
        let e = hits.entry(key.to_string()).or_insert((Instant::now(), 0));
        if e.0.elapsed() >= Duration::from_secs(60) {
            *e = (Instant::now(), 0);
        }
        e.1 += 1;
        e.1 <= max
    }
}

/// Whether `text` matches `pattern`, where `*` stands for any run of characters (none too). No
/// other wildcards; linear in practice (the classic two-pointer match, no recursion).
pub(crate) fn wildcard(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut i, mut j) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while j < t.len() {
        if i < p.len() && p[i] == '*' {
            star = Some((i, j));
            i += 1;
        } else if i < p.len() && p[i] == t[j] {
            i += 1;
            j += 1;
        } else if let Some((si, sj)) = star {
            i = si + 1;
            j = sj + 1;
            star = Some((si, sj + 1));
        } else {
            return false;
        }
    }
    p[i..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::{Window, wildcard};

    #[test]
    fn a_window_counts_each_key_apart() {
        let w = Window::default();
        assert!(w.allow("identity:a", 2) && w.allow("identity:a", 2) && !w.allow("identity:a", 2));
        assert!(w.allow("identity:b", 2), "another key has its own count");
    }

    #[test]
    fn wildcards_match_any_run_and_nothing_else() {
        assert!(wildcard("repo:acme/web:ref:refs/heads/*", "repo:acme/web:ref:refs/heads/main"));
        assert!(wildcard("repo:acme/web:*", "repo:acme/web:environment:production"));
        assert!(!wildcard("repo:acme/web:*", "repo:acme/website:ref:refs/heads/main"));
        assert!(wildcard("a*b*c", "abc") && wildcard("a*b*c", "a-b-b-c") && !wildcard("a*b*c", "a-c-b"));
        assert!(!wildcard("exact", "exactly") && wildcard("*", ""));
    }
}
