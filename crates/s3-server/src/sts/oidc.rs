//! Web identity tokens for `AssumeRoleWithWebIdentity`: OpenID Connect ID tokens (JWTs)
//! signed with RS256 or ES256 by a provider a role trusts.
//!
//! - Only issuers named in the role's trust are ever fetched from, so a token can't make roll
//!   reach an address of its choosing.
//! - The provider's keys come from its JWKS (`jwks_uri`, or the issuer's OpenID discovery
//!   document): HTTPS only (plain HTTP only with [`crate::Config::oidc_allow_http`], for
//!   tests), a 5 s timeout, size caps, cached for an hour and refetched early (at most every
//!   30 s) when a token names a key id the cache doesn't have. If the provider is down, keys up
//!   to a day old keep working.
//! - Checked: the signature, `iss` (exactly), `aud` (one of the provider's audiences), `exp`
//!   and `nbf` (60 s leeway), and the role's subject pattern against `sub`.
//! - The fetch is behind [`JwksFetch`] so tests can serve their own keys.

use crate::sts::StsError;
use futures::future::BoxFuture;
use s3_store::sts::OidcProvider;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

/// Longest `WebIdentityToken` (AWS's limit).
pub const MAX_JWT_BYTES: usize = 20_000;
/// Largest JWKS and discovery documents read.
pub const MAX_JWKS_BYTES: usize = 256 * 1024;
pub const MAX_DISCOVERY_BYTES: usize = 64 * 1024;
const MAX_KEYS: usize = 100;
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);
const TTL: Duration = Duration::from_secs(3600);
const STALE_OK: Duration = Duration::from_secs(86_400);
const REFETCH_MIN: Duration = Duration::from_secs(30);
const LEEWAY_S: i64 = 60;
/// Issuers whose keys are cached at once (they're bounded by the roles' trust anyway).
const MAX_CACHED: usize = 1000;

/// Gets a document over HTTP(S) for the OIDC verifier.
pub trait JwksFetch: Send + Sync {
    /// The body of `GET url` (a 2xx answer), refusing more than `limit` bytes.
    fn get<'a>(&'a self, url: &'a str, limit: usize) -> BoxFuture<'a, Result<Vec<u8>, String>>;
}

/// The real fetcher: reqwest over rustls.
pub struct HttpFetch {
    client: reqwest::Client,
    allow_http: bool,
}

impl HttpFetch {
    /// `allow_http`: also fetch `http://` URLs (tests only).
    pub fn new(allow_http: bool) -> Self {
        let client = reqwest::Client::builder()
            .timeout(FETCH_TIMEOUT)
            .connect_timeout(FETCH_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .https_only(!allow_http)
            .build()
            .expect("an HTTP client");
        Self { client, allow_http }
    }
}

impl JwksFetch for HttpFetch {
    fn get<'a>(&'a self, url: &'a str, limit: usize) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(async move {
            if !(url.starts_with("https://") || (self.allow_http && url.starts_with("http://"))) {
                return Err(format!("{url} isn't an https:// URL"));
            }
            let mut r = self.client.get(url).header("accept", "application/json").send().await.map_err(|e| format!("{url}: {e}"))?;
            if !r.status().is_success() {
                return Err(format!("{url}: HTTP {}", r.status()));
            }
            if r.content_length().is_some_and(|n| n > limit as u64) {
                return Err(format!("{url}: larger than {limit} bytes"));
            }
            let mut out = Vec::new();
            while let Some(chunk) = r.chunk().await.map_err(|e| format!("{url}: {e}"))? {
                if out.len() + chunk.len() > limit {
                    return Err(format!("{url}: larger than {limit} bytes"));
                }
                out.extend_from_slice(&chunk);
            }
            Ok(out)
        })
    }
}

/// A public key from a JWKS.
#[derive(Clone, Debug)]
enum Key {
    Rsa { kid: Option<String>, n: Vec<u8>, e: Vec<u8> },
    /// Uncompressed SEC1 point.
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
fn parse_jwks(doc: &[u8]) -> Result<Vec<Key>, String> {
    let v: Value = serde_json::from_slice(doc).map_err(|e| format!("the JWKS isn't JSON: {e}"))?;
    let keys = v.get("keys").and_then(Value::as_array).ok_or("the JWKS has no keys")?;
    let mut out = Vec::new();
    for k in keys.iter().take(MAX_KEYS) {
        let s = |n: &str| k.get(n).and_then(Value::as_str);
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

/// A JWT, decoded but not verified.
struct Jwt<'a> {
    alg: Alg,
    kid: Option<String>,
    claims: serde_json::Map<String, Value>,
    signed: &'a str,
    signature: Vec<u8>,
}

fn invalid(m: impl Into<String>) -> OidcError {
    OidcError::Invalid(m.into())
}

impl<'a> Jwt<'a> {
    fn parse(token: &'a str) -> Result<Self, OidcError> {
        if token.len() > MAX_JWT_BYTES {
            return Err(invalid("The web identity token is too long."));
        }
        let mut parts = token.trim().split('.');
        let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return Err(invalid("The web identity token isn't a signed JWT."));
        };
        let header: Value = b64(h).and_then(|b| serde_json::from_slice(&b).ok()).ok_or_else(|| invalid("The web identity token's header isn't valid."))?;
        let alg = match header.get("alg").and_then(Value::as_str) {
            Some("RS256") => Alg::Rs256,
            Some("ES256") => Alg::Es256,
            Some(a) => return Err(invalid(format!("Unsupported signing algorithm {a}: RS256 and ES256 are."))),
            None => return Err(invalid("The web identity token's header has no alg.")),
        };
        if header.get("crit").is_some() {
            return Err(invalid("The web identity token has critical header parameters."));
        }
        let kid = header.get("kid").and_then(Value::as_str).map(str::to_string);
        let claims = match b64(p).and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
            Some(Value::Object(m)) => m,
            _ => return Err(invalid("The web identity token's claims aren't valid.")),
        };
        let signature = b64(s).ok_or_else(|| invalid("The web identity token's signature isn't valid."))?;
        let signed = &token.trim()[..h.len() + 1 + p.len()];
        Ok(Self { alg, kid, claims, signed, signature })
    }

    fn str(&self, name: &str) -> Option<&str> {
        self.claims.get(name).and_then(Value::as_str)
    }

    fn time(&self, name: &str) -> Result<Option<i64>, OidcError> {
        match self.claims.get(name) {
            None => Ok(None),
            Some(v) => v.as_f64().map(|f| Some(f as i64)).ok_or_else(|| invalid(format!("The web identity token's {name} isn't a number."))),
        }
    }

    fn audiences(&self) -> Vec<&str> {
        match self.claims.get("aud") {
            Some(Value::String(s)) => vec![s.as_str()],
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
            _ => Vec::new(),
        }
    }
}

/// Why a web identity token was refused.
#[derive(Debug)]
pub(crate) enum OidcError {
    /// `InvalidIdentityToken`.
    Invalid(String),
    /// `ExpiredTokenException`.
    Expired,
    /// `AccessDenied`: valid, but the role's trust doesn't take this subject.
    Denied,
    /// `IDPCommunicationError`: the provider's keys couldn't be fetched.
    Idp(String),
}

impl OidcError {
    pub(crate) fn into_sts(self) -> StsError {
        match self {
            OidcError::Invalid(m) => StsError::new("InvalidIdentityToken", 400, m),
            OidcError::Expired => StsError::new("ExpiredTokenException", 400, "Token is expired"),
            OidcError::Denied => StsError::new("AccessDenied", 403, "Not authorized to perform sts:AssumeRoleWithWebIdentity"),
            OidcError::Idp(m) => {
                tracing::warn!(error = m, "fetching an OIDC provider's keys failed");
                StsError::new("IDPCommunicationError", 400, "Couldn't retrieve the identity provider's signing keys. Please try again.")
            }
        }
    }
}

/// A verified token.
#[derive(Debug)]
pub(crate) struct Verified {
    pub issuer: String,
    pub subject: String,
    /// The audience the role accepted.
    pub audience: String,
    /// Session tags from the `https://aws.amazon.com/tags` claim.
    pub tags: Vec<(String, String)>,
    pub transitive_tag_keys: Vec<String>,
    /// The `https://aws.amazon.com/source_identity` claim.
    pub source_identity: Option<String>,
}

struct Cached {
    keys: Arc<Vec<Key>>,
    fetched: Instant,
}

/// The verifier: a fetcher and the providers' cached keys.
pub(crate) struct Oidc {
    fetch: RwLock<Arc<dyn JwksFetch>>,
    cache: Mutex<HashMap<String, Cached>>,
    /// One fetch per provider at a time.
    fetching: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Oidc {
    pub(crate) fn new(fetch: Arc<dyn JwksFetch>) -> Self {
        Self { fetch: RwLock::new(fetch), cache: Default::default(), fetching: Default::default() }
    }

    pub(crate) fn set_fetch(&self, fetch: Arc<dyn JwksFetch>) {
        *self.fetch.write().unwrap() = fetch;
        self.cache.lock().unwrap().clear();
    }

    /// Fetch a provider's keys now.
    async fn download(&self, p: &OidcProvider) -> Result<Vec<Key>, String> {
        let fetch = self.fetch.read().unwrap().clone();
        let jwks_uri = match &p.jwks_uri {
            Some(u) => u.clone(),
            None => {
                let url = format!("{}/.well-known/openid-configuration", p.issuer.trim_end_matches('/'));
                let doc: Value = serde_json::from_slice(&fetch.get(&url, MAX_DISCOVERY_BYTES).await?).map_err(|e| format!("{url}: not JSON: {e}"))?;
                doc.get("jwks_uri").and_then(Value::as_str).ok_or_else(|| format!("{url} has no jwks_uri"))?.to_string()
            }
        };
        parse_jwks(&fetch.get(&jwks_uri, MAX_JWKS_BYTES).await?)
    }

    /// A provider's keys: cached, or fetched when they're old (or `refresh` and not fetched in
    /// the last 30 s).
    async fn keys(&self, p: &OidcProvider, refresh: bool) -> Result<Arc<Vec<Key>>, OidcError> {
        let id = format!("{}\n{}", p.issuer, p.jwks_uri.as_deref().unwrap_or_default());
        let fresh = |c: &Cached| {
            let age = c.fetched.elapsed();
            age < TTL && !(refresh && age >= REFETCH_MIN)
        };
        if let Some(c) = self.cache.lock().unwrap().get(&id).filter(|c| fresh(c)) {
            return Ok(c.keys.clone());
        }
        let lock = self.fetching.lock().unwrap().entry(id.clone()).or_default().clone();
        let _one = lock.lock().await;
        // Someone else may just have fetched them.
        if let Some(c) = self.cache.lock().unwrap().get(&id).filter(|c| c.fetched.elapsed() < REFETCH_MIN || fresh(c)) {
            return Ok(c.keys.clone());
        }
        match self.download(p).await {
            Ok(keys) => {
                let keys = Arc::new(keys);
                let mut cache = self.cache.lock().unwrap();
                if cache.len() >= MAX_CACHED {
                    cache.clear();
                }
                cache.insert(id, Cached { keys: keys.clone(), fetched: Instant::now() });
                Ok(keys)
            }
            Err(e) => match self.cache.lock().unwrap().get(&id).filter(|c| c.fetched.elapsed() < STALE_OK) {
                Some(c) => {
                    tracing::warn!(issuer = p.issuer, error = e, "refreshing an OIDC provider's keys failed; using the cached ones");
                    Ok(c.keys.clone())
                }
                None => Err(OidcError::Idp(e)),
            },
        }
    }

    /// Verify `token` against the role's `providers` at `now_s` (Unix seconds).
    pub(crate) async fn verify(&self, providers: &[OidcProvider], token: &str, now_s: i64) -> Result<Verified, OidcError> {
        let jwt = Jwt::parse(token)?;
        let iss = jwt.str("iss").ok_or_else(|| invalid("The web identity token has no iss."))?.to_string();
        let candidates: Vec<&OidcProvider> = providers.iter().filter(|p| p.issuer == iss).collect();
        let Some(first) = candidates.first() else { return Err(invalid(format!("No OpenIDConnect provider found in your account for {iss}"))) };
        let matching = |keys: &[Key]| {
            keys.iter().filter(|k| match (&jwt.kid, k.kid()) {
                (Some(want), Some(have)) => want == have,
                (Some(_), None) | (None, _) => true,
            })
            .any(|k| k.verify(jwt.alg, jwt.signed.as_bytes(), &jwt.signature))
        };
        let keys = self.keys(first, false).await?;
        let known_kid = jwt.kid.as_ref().is_none_or(|want| keys.iter().any(|k| k.kid() == Some(want)));
        let ok = matching(&keys) || (!known_kid && matching(&self.keys(first, true).await?));
        if !ok {
            return Err(invalid("The web identity token's signature doesn't verify."));
        }
        let exp = jwt.time("exp")?.ok_or_else(|| invalid("The web identity token has no exp."))?;
        if now_s >= exp + LEEWAY_S {
            return Err(OidcError::Expired);
        }
        if jwt.time("nbf")?.is_some_and(|nbf| now_s + LEEWAY_S < nbf) {
            return Err(invalid("The web identity token isn't valid yet."));
        }
        let sub = jwt.str("sub").filter(|s| !s.is_empty()).ok_or_else(|| invalid("The web identity token has no sub."))?.to_string();
        let auds = jwt.audiences();
        let mut audience_ok = false;
        let mut chosen = None;
        for p in &candidates {
            let Some(aud) = auds.iter().find(|a| p.audiences.iter().any(|w| w == *a)) else { continue };
            audience_ok = true;
            if p.subject.as_ref().is_none_or(|pat| crate::policy::wildcard(pat, &sub, false)) {
                chosen = Some(aud.to_string());
                break;
            }
        }
        let audience = match chosen {
            Some(a) => a,
            None if audience_ok => return Err(OidcError::Denied),
            None => return Err(invalid("Incorrect token audience")),
        };
        let (tags, transitive_tag_keys) = aws_tags(&jwt)?;
        let source_identity = jwt.str("https://aws.amazon.com/source_identity").map(str::to_string);
        if let Some(si) = &source_identity {
            s3_store::sts::validate_source_identity(si).map_err(invalid)?;
        }
        Ok(Verified { issuer: iss, subject: sub, audience, tags, transitive_tag_keys, source_identity })
    }
}

/// Session tags in a token, as AWS reads them: the `https://aws.amazon.com/tags` claim with
/// `principal_tags` (`{key: [value]}`) and `transitive_tag_keys`.
fn aws_tags(jwt: &Jwt) -> Result<s3_store::sts::SessionTags, OidcError> {
    let Some(t) = jwt.claims.get("https://aws.amazon.com/tags") else { return Ok((Vec::new(), Vec::new())) };
    let bad = || invalid("The web identity token's https://aws.amazon.com/tags claim isn't valid.");
    let mut tags = Vec::new();
    if let Some(pt) = t.get("principal_tags") {
        for (k, v) in pt.as_object().ok_or_else(bad)? {
            let v = match v {
                Value::Array(a) => a.first().and_then(Value::as_str).ok_or_else(bad)?,
                Value::String(s) => s.as_str(),
                _ => return Err(bad()),
            };
            tags.push((k.clone(), v.to_string()));
        }
    }
    let transitive: Vec<String> = match t.get("transitive_tag_keys") {
        Some(Value::Array(a)) => a.iter().map(|v| v.as_str().map(str::to_string).ok_or_else(bad)).collect::<Result<_, _>>()?,
        None => Vec::new(),
        Some(_) => return Err(bad()),
    };
    s3_store::sts::validate_tags(&tags, &transitive).map_err(invalid)?;
    Ok((tags, transitive))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use ring::rand::SystemRandom;
    use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};

    struct Fake(Mutex<HashMap<String, Vec<u8>>>, std::sync::atomic::AtomicUsize);

    impl JwksFetch for Fake {
        fn get<'a>(&'a self, url: &'a str, limit: usize) -> BoxFuture<'a, Result<Vec<u8>, String>> {
            Box::pin(async move {
                self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let body = self.0.lock().unwrap().get(url).cloned().ok_or_else(|| format!("{url}: 404"))?;
                if body.len() > limit { Err("too big".into()) } else { Ok(body) }
            })
        }
    }

    fn enc(v: &Value) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string())
    }

    struct Signer {
        key: EcdsaKeyPair,
        kid: String,
    }

    impl Signer {
        fn new(kid: &str) -> Self {
            let rng = SystemRandom::new();
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, &rng).unwrap();
            Self { key: EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8.as_ref(), &rng).unwrap(), kid: kid.into() }
        }

        fn jwk(&self) -> Value {
            let p = self.key.public_key().as_ref();
            let b = |s: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(s);
            serde_json::json!({ "kty": "EC", "crv": "P-256", "kid": self.kid, "use": "sig", "x": b(&p[1..33]), "y": b(&p[33..65]) })
        }

        fn sign(&self, claims: Value) -> String {
            let msg = format!("{}.{}", enc(&serde_json::json!({ "alg": "ES256", "kid": self.kid, "typ": "JWT" })), enc(&claims));
            let sig = self.key.sign(&SystemRandom::new(), msg.as_bytes()).unwrap();
            format!("{msg}.{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.as_ref()))
        }
    }

    const ISS: &str = "https://idp.example.com";

    fn provider() -> OidcProvider {
        OidcProvider { issuer: ISS.into(), audiences: vec!["app".into()], subject: Some("user-*".into()), jwks_uri: None }
    }

    fn setup(signers: &[&Signer]) -> (Oidc, Arc<Fake>) {
        let keys = serde_json::json!({ "keys": signers.iter().map(|s| s.jwk()).collect::<Vec<_>>() });
        let mut m = HashMap::new();
        m.insert(format!("{ISS}/.well-known/openid-configuration"), serde_json::json!({ "jwks_uri": "https://keys.example.com/jwks" }).to_string().into_bytes());
        m.insert("https://keys.example.com/jwks".to_string(), keys.to_string().into_bytes());
        let fake = Arc::new(Fake(Mutex::new(m), Default::default()));
        (Oidc::new(fake.clone()), fake)
    }

    const NOW: i64 = 1_800_000_000;

    fn claims() -> Value {
        serde_json::json!({ "iss": ISS, "aud": ["other", "app"], "sub": "user-1", "exp": NOW + 600, "iat": NOW })
    }

    #[tokio::test]
    async fn verifies_and_checks_claims() {
        let s = Signer::new("k1");
        let (o, fake) = setup(&[&s]);
        let v = o.verify(&[provider()], &s.sign(claims()), NOW).await.unwrap();
        assert_eq!((v.issuer.as_str(), v.subject.as_str(), v.audience.as_str()), (ISS, "user-1", "app"));
        assert_eq!(fake.1.load(std::sync::atomic::Ordering::SeqCst), 2, "discovery and JWKS");
        o.verify(&[provider()], &s.sign(claims()), NOW).await.unwrap();
        assert_eq!(fake.1.load(std::sync::atomic::Ordering::SeqCst), 2, "cached");

        let with = |f: &dyn Fn(&mut Value)| {
            let mut c = claims();
            f(&mut c);
            s.sign(c)
        };
        let err = |r: Result<Verified, OidcError>| format!("{:?}", r.unwrap_err());
        assert!(matches!(o.verify(&[provider()], &with(&|c| c["exp"] = (NOW - 61).into()), NOW).await, Err(OidcError::Expired)));
        o.verify(&[provider()], &with(&|c| c["exp"] = (NOW - 30).into()), NOW).await.expect("within the leeway");
        let e = err(o.verify(&[provider()], &with(&|c| c["nbf"] = (NOW + 600).into()), NOW).await);
        assert!(e.contains("isn't valid yet"), "{e}");
        assert!(err(o.verify(&[provider()], &with(&|c| c["aud"] = "nope".into()), NOW).await).contains("audience"));
        assert!(matches!(o.verify(&[provider()], &with(&|c| c["sub"] = "admin".into()), NOW).await, Err(OidcError::Denied)));
        assert!(err(o.verify(&[provider()], &with(&|c| c["iss"] = "https://evil.example.com".into()), NOW).await).contains("No OpenIDConnect provider"));
        assert!(err(o.verify(&[provider()], &with(&|c| { c.as_object_mut().unwrap().remove("exp"); }), NOW).await).contains("exp"));

        // Tampered claims, a stranger's key, and `alg: none`.
        let good = s.sign(claims());
        let parts: Vec<&str> = good.split('.').collect();
        let mut c = claims();
        c["sub"] = "user-2".into();
        let tampered = format!("{}.{}.{}", parts[0], enc(&c), parts[2]);
        assert!(err(o.verify(&[provider()], &tampered, NOW).await).contains("signature"));
        let stranger = Signer { key: Signer::new("x").key, kid: "k1".into() };
        assert!(err(o.verify(&[provider()], &stranger.sign(claims()), NOW).await).contains("signature"));
        let none = format!("{}.{}.", enc(&serde_json::json!({ "alg": "none" })), enc(&claims()));
        assert!(err(o.verify(&[provider()], &none, NOW).await).contains("Unsupported"));
        let hs = format!("{}.{}.x", enc(&serde_json::json!({ "alg": "HS256" })), enc(&claims()));
        assert!(err(o.verify(&[provider()], &hs, NOW).await).contains("Unsupported"));
        assert!(err(o.verify(&[provider()], "a.b", NOW).await).contains("JWT"));
        assert!(err(o.verify(&[provider()], &"a".repeat(MAX_JWT_BYTES + 1), NOW).await).contains("too long"));
    }

    #[tokio::test]
    async fn key_rotation_refetches_once() {
        let old = Signer::new("old");
        let new = Signer::new("new");
        let (o, fake) = setup(&[&old]);
        o.verify(&[provider()], &old.sign(claims()), NOW).await.unwrap();
        // The provider rotates its keys; the cache doesn't know `new` yet.
        let keys = serde_json::json!({ "keys": [old.jwk(), new.jwk()] });
        fake.0.lock().unwrap().insert("https://keys.example.com/jwks".into(), keys.to_string().into_bytes());
        // Fetched less than 30 s ago: no refetch yet.
        assert!(o.verify(&[provider()], &new.sign(claims()), NOW).await.is_err());
        // Pretend the cache is older.
        for c in o.cache.lock().unwrap().values_mut() {
            c.fetched -= REFETCH_MIN;
        }
        o.verify(&[provider()], &new.sign(claims()), NOW).await.unwrap();
        assert_eq!(fake.1.load(std::sync::atomic::Ordering::SeqCst), 4);
    }

    #[tokio::test]
    async fn provider_outages() {
        let s = Signer::new("k1");
        let (o, fake) = setup(&[&s]);
        let explicit = OidcProvider { jwks_uri: Some("https://keys.example.com/jwks".into()), ..provider() };
        o.verify(std::slice::from_ref(&explicit), &s.sign(claims()), NOW).await.unwrap();
        assert_eq!(fake.1.load(std::sync::atomic::Ordering::SeqCst), 1, "no discovery with an explicit jwks_uri");
        fake.0.lock().unwrap().clear();
        for c in o.cache.lock().unwrap().values_mut() {
            c.fetched -= TTL;
        }
        o.verify(std::slice::from_ref(&explicit), &s.sign(claims()), NOW).await.expect("stale keys while the provider is down");
        o.cache.lock().unwrap().clear();
        assert!(matches!(o.verify(&[explicit], &s.sign(claims()), NOW).await, Err(OidcError::Idp(_))));
    }

    #[test]
    fn jwks_parsing() {
        let s = Signer::new("a");
        let doc = serde_json::json!({ "keys": [
            s.jwk(),
            { "kty": "RSA", "kid": "r", "n": "AQAB", "e": "AQAB" },
            { "kty": "RSA", "kid": "enc", "use": "enc", "n": "AQAB", "e": "AQAB" },
            { "kty": "oct", "k": "abc" },
            { "kty": "EC", "crv": "P-384", "x": "AA", "y": "AA" },
        ]});
        let keys = parse_jwks(doc.to_string().as_bytes()).unwrap();
        assert_eq!(keys.iter().filter_map(Key::kid).collect::<Vec<_>>(), vec!["a", "r"]);
        assert!(parse_jwks(b"[]").is_err());
        assert!(parse_jwks(b"nope").is_err());
    }

    #[test]
    fn aws_tag_claims() {
        let s = Signer::new("a");
        let mut c = claims();
        c["https://aws.amazon.com/tags"] = serde_json::json!({ "principal_tags": { "team": ["red"], "env": ["prod"] }, "transitive_tag_keys": ["team"] });
        let t = s.sign(c);
        let jwt = Jwt::parse(&t).unwrap();
        let (tags, tr) = aws_tags(&jwt).unwrap();
        assert!(tags.contains(&("team".into(), "red".into())));
        assert_eq!(tr, vec!["team".to_string()]);
    }

    #[tokio::test]
    async fn http_fetch_refuses_plain_http() {
        let f = HttpFetch::new(false);
        assert!(f.get("http://127.0.0.1:1/jwks", 10).await.unwrap_err().contains("https"));
        assert!(f.get("file:///etc/passwd", 10).await.is_err());
    }
}
