//! OpenID Connect ID tokens (JWTs, RS256 or ES256) from workloads: a CI job's or a cluster's own
//! short-lived token, verified against its issuer's published keys. roll secrets' workload
//! identities (`roll-secrets/src/identities.rs`) and roll-access's trusted machine clients both
//! check tokens here, so the rules are one.
//!
//! The same rules as storage's web identity (`s3-server/src/sts/oidc.rs`, which this follows):
//!
//! - Only an issuer an identity names is ever fetched from, over the caller's guarded client
//!   ([`Fetch`]; rolld's: HTTPS, public addresses, no redirects, bounded reads), so a token can't
//!   make roll reach an address of its choosing. The size caps are here, not the caller's.
//! - Keys come from the issuer's discovery document's `jwks_uri`, cached for an hour and fetched
//!   again early (at most every 30 s) when a token names a key id the cache doesn't have. If the
//!   issuer is down, keys up to a day old keep working.
//! - Checked: the signature, `iss` exactly, `aud`, `exp` and `nbf` with 60 s of leeway. The
//!   identity checks `sub` and its other claims.
//!
//! Public keys are cached, never a token or what it maps to.
//!
//! An identity may instead pin its issuer's keys (a JWKS given when it's made, as Vault's
//! `jwt_validation_pubkeys`): then the issuer is only compared with `iss`, never fetched, so an
//! issuer roll can't reach (a cluster's own, `https://kubernetes.default.svc`) works.

use serde_json::{Map, Value as Json};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Why a token didn't check out, or couldn't be checked. Never holds a token.
#[derive(Debug)]
pub enum Error {
    /// The token isn't good (401): its signature, issuer, audience or times.
    Invalid(String),
    /// The issuer's keys couldn't be had (a link problem: 502).
    Unavailable(String),
    /// The caller's fetcher failed: its own error, for the caller to answer as it would.
    Fetch(Box<dyn std::error::Error + Send + Sync>),
    /// Nothing to verify with: no fetcher and no pinned keys.
    Off,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Invalid(m) | Error::Unavailable(m) => f.write_str(m),
            Error::Fetch(e) => e.fmt(f),
            Error::Off => f.write_str("no keys to verify identity tokens with on this server"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;

/// How an issuer's discovery document and keys are fetched: the caller's guarded client, at most
/// `limit` bytes. An issuer is a URL a tenant chose, so this crate relies on its fetcher for SSRF
/// safety: an implementation MUST refuse anything but public addresses (a resolver that drops
/// private ones, and literal private hosts), MUST NOT follow redirects, and MUST stop reading at
/// `limit`. rolld's guarded client (`roll_net::PublicOnly`) is the one both callers hand it.
pub trait Fetch {
    fn get(&self, url: &str, limit: usize) -> std::result::Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>>;
}

/// Issuers every tenant of theirs shares (every GitHub repo's Actions sign with GitHub's): an
/// identity of theirs names one tenant in its subject, literally, or anyone's job could log in as
/// it. (issuer, subject prefix, whether the tenant is `owner/name`, the shape to show.) A prefix of
/// "" means the whole subject is literal (Google's `sub` is an account's number).
pub const SHARED: &[(&str, &str, bool, &str)] = &[
    ("https://token.actions.githubusercontent.com", "repo:", true, "repo:<owner>/<repo>:… (GitHub Actions' sub)"),
    ("https://gitlab.com", "project_path:", true, "project_path:<group>/<project>:… (GitLab CI's sub)"),
    ("https://app.terraform.io", "organization:", false, "organization:<org>:… (Terraform Cloud's sub)"),
    ("https://agent.buildkite.com", "organization:", false, "organization:<org>:… (Buildkite's sub)"),
    ("https://accounts.google.com", "", false, "the service account's numeric id, exactly (Google's sub)"),
];

/// Whether `subject` names one tenant of a shared issuer literally.
pub fn pinned(subject: &str, prefix: &str, owner_name: bool) -> bool {
    if prefix.is_empty() {
        return !subject.contains('*');
    }
    let Some((tenant, _)) = subject.strip_prefix(prefix).and_then(|r| r.split_once(':')) else { return false };
    let shaped = if owner_name { tenant.split_once('/').is_some_and(|(o, n)| !o.is_empty() && !n.is_empty()) } else { !tenant.is_empty() };
    shaped && !tenant.contains('*')
}

/// Azure Pipelines' issuer: one per organization (`https://vstoken.dev.azure.com/<organization id>`),
/// signing for every project, pipeline and service connection in it.
const AZURE_DEVOPS: &str = "https://vstoken.dev.azure.com/";
/// Microsoft Entra's v2.0 issuer: one per tenant (`https://login.microsoftonline.com/<tenant>/v2.0`),
/// signing for every app and identity in it, Azure DevOps' newer service connections among them.
const ENTRA: (&str, &str) = ("https://login.microsoftonline.com/", "/v2.0");

/// Why an identity for `issuer` with `subject` would let other tenants in, if it would: a shared
/// issuer's subject must name one tenant literally. Not optional: every identity is checked.
///
/// Azure's issuers are per organization or tenant, but everyone in it shares them: any project's
/// pipeline, any app. So the subject names one pipeline or service connection (`p://` or `sc://`
/// with an organization, a project and a name), or one Entra subject exactly.
pub fn open_to_others(issuer: &str, subject: &str) -> Option<&'static str> {
    if let Some(org) = issuer.strip_prefix(AZURE_DEVOPS) {
        let named = |prefix: &str| subject.strip_prefix(prefix).is_some_and(|r| r.splitn(3, '/').filter(|p| !p.is_empty()).count() == 3);
        let one = !org.is_empty() && !subject.contains('*') && (named("p://") || named("sc://"));
        return (!one).then_some("p://<organization>/<project>/<pipeline> or sc://<organization>/<project>/<service connection>, exactly (Azure Pipelines' sub)");
    }
    if issuer.strip_prefix(ENTRA.0).and_then(|r| r.strip_suffix(ENTRA.1)).is_some_and(|t| !t.is_empty() && !t.contains('/')) && subject.contains('*') {
        return Some("the token's sub exactly (Microsoft Entra's: an Azure DevOps service connection's /eid1/…/sc/<organization id>/<connection id>)");
    }
    SHARED.iter().find(|(i, ..)| *i == issuer).filter(|(_, prefix, owner_name, _)| !pinned(subject, prefix, *owner_name)).map(|(.., shape)| *shape)
}

/// The longest token taken.
pub const MAX_JWT_BYTES: usize = 16 * 1024;
const MAX_JWKS_BYTES: usize = 256 * 1024;
/// The largest pinned JWKS (stored with the identity).
pub const MAX_PINNED_BYTES: usize = 64 * 1024;
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
    Error::Invalid(m.into())
}

/// A JWT, decoded but not verified.
pub struct Jwt<'a> {
    alg: Alg,
    kid: Option<String>,
    /// The header's `typ` (`oauth-id-jag+jwt` for an identity assertion grant), as given.
    pub typ: Option<String>,
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
        let typ = header.get("typ").and_then(Json::as_str).map(str::to_string);
        let claims = match b64(p).and_then(|b| serde_json::from_slice::<Json>(&b).ok()) {
            Some(Json::Object(m)) => m,
            _ => return Err(invalid("the identity token's claims aren't valid")),
        };
        let signature = b64(s).ok_or_else(|| invalid("the identity token's signature isn't valid"))?;
        let signed = &token[..h.len() + 1 + p.len()];
        Ok(Self { alg, kid, typ, claims, signed, signature })
    }

    pub fn str(&self, name: &str) -> Option<&str> {
        self.claims.get(name).and_then(Json::as_str)
    }

    /// The first of an identity's `claims` (name or pointer → pattern) the token doesn't match,
    /// if any. See [`claim`] for what a key names and what a value matches.
    pub fn unmatched<'c>(&self, claims: &'c BTreeMap<String, String>) -> Option<&'c str> {
        claims.iter().find(|(key, want)| !self.matches(key, want)).map(|(key, _)| key.as_str())
    }

    /// Whether the claim `key` names (see [`claim`]) matches `pattern`.
    ///
    /// A plain name (`ref`) matches only a string claim by [`wildcard`], exactly as rules did
    /// before pointers: a rule made then never starts matching a list or a number it used to
    /// refuse. A pointer (`/groups`) opts into more: a string by [`wildcard`], a number or a
    /// boolean by its JSON text (`true`, `42`), an array when any of its first
    /// [`MAX_CLAIM_ELEMENTS`] elements does (one level: an array in an array, an object or null
    /// never matches). A missing claim never matches.
    pub fn matches(&self, key: &str, pattern: &str) -> bool {
        if !key.starts_with('/') {
            return self.str(key).is_some_and(|s| wildcard(pattern, s));
        }
        let one = |v: &Json| match v {
            Json::String(s) => wildcard(pattern, s),
            Json::Number(_) | Json::Bool(_) => wildcard(pattern, &v.to_string()),
            _ => false,
        };
        match claim(&self.claims, key) {
            Some(Json::Array(a)) => a.iter().take(MAX_CLAIM_ELEMENTS).any(one),
            Some(v) => one(v),
            None => false,
        }
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
pub struct Keys {
    cache: Mutex<HashMap<String, Cached>>,
    /// One fetch per issuer at a time: logins that need it wait for its answer.
    fetching: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    /// When an issuer's last fetch failed: none is tried again for [`REFETCH_MIN`], so a login
    /// flood while it's down costs no fetches.
    failed: Mutex<HashMap<String, Instant>>,
}

impl Keys {
    fn download(&self, get: &dyn Fetch, issuer: &str) -> Result<Vec<Key>> {
        let url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
        let doc: Json = serde_json::from_slice(&get.get(&url, MAX_DISCOVERY_BYTES).map_err(Error::Fetch)?).map_err(|_| Error::Unavailable(format!("{url} isn't JSON")))?;
        let jwks = doc.get("jwks_uri").and_then(Json::as_str).ok_or_else(|| Error::Unavailable(format!("{url} has no jwks_uri")))?;
        parse_jwks(&get.get(jwks, MAX_JWKS_BYTES).map_err(Error::Fetch)?).map_err(Error::Unavailable)
    }

    /// An issuer's keys: cached, or fetched when they're old (or `refresh` and not fetched in the
    /// last 30 s).
    fn keys(&self, get: &dyn Fetch, issuer: &str, refresh: bool) -> Result<Arc<Vec<Key>>> {
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
            return stale().ok_or_else(|| Error::Unavailable(format!("{issuer}'s keys are unavailable just now: try again shortly")));
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

    /// Verify `jwt`'s signature, issuer, audience and times at `now_s` (Unix seconds), with the
    /// issuer's published keys, or with `pinned` (a JWKS) when the identity carries its own.
    pub fn verify(&self, get: Option<&dyn Fetch>, pinned: Option<&str>, jwt: &Jwt<'_>, issuer: &str, audience: &str, now_s: i64) -> Result<()> {
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
        let verified = match (pinned, get) {
            (Some(doc), _) => matching(&parse_jwks(doc.as_bytes()).map_err(|_| invalid("the identity's keys don't parse"))?),
            (None, Some(get)) => {
                let keys = self.keys(get, issuer, false)?;
                let known_kid = jwt.kid.as_ref().is_none_or(|want| keys.iter().any(|k| k.kid() == Some(want)));
                matching(&keys) || (!known_kid && matching(&self.keys(get, issuer, true)?))
            }
            (None, None) => return Err(Error::Off),
        };
        if !verified {
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

/// A JWKS an identity pins, checked when it's given: how many signing keys roll can use in it.
pub fn pinned_keys(doc: &str) -> std::result::Result<usize, String> {
    if doc.len() > MAX_PINNED_BYTES {
        return Err(format!("jwks is at most {} KiB", MAX_PINNED_BYTES / 1024));
    }
    let n = parse_jwks(doc.as_bytes())?.len();
    if n == 0 {
        return Err("jwks has no RS256 or ES256 (P-256) signing key".into());
    }
    Ok(n)
}

/// The most claim rules one identity has.
pub const MAX_CLAIMS: usize = 10;
/// The most segments a claim's JSON pointer has.
pub const MAX_POINTER_DEPTH: usize = 8;
/// The most elements of an array claim a rule looks at; later ones never match.
pub const MAX_CLAIM_ELEMENTS: usize = 256;
/// Checked by [`Keys::verify`] or the identity's own fields; never a claim rule.
const RESERVED: [&str; 6] = ["iss", "aud", "sub", "exp", "nbf", "iat"];

/// The value a claim rule's key names in a token's claims. A key starting with `/` is a JSON
/// pointer (RFC 6901: `~1` is `/`, `~0` is `~`) into nested objects, with a decimal index for an
/// array element (`/realm_access/roles`, `/groups/0`); any other key is a top-level claim's name,
/// as it always was. Walks at most [`MAX_POINTER_DEPTH`] segments; a malformed pointer names
/// nothing.
pub fn claim<'j>(claims: &'j Map<String, Json>, key: &str) -> Option<&'j Json> {
    let Some(pointer) = key.strip_prefix('/') else { return claims.get(key) };
    let mut segments = pointer.split('/');
    let mut at = claims.get(&unescape(segments.next()?)?)?;
    for (n, segment) in segments.enumerate() {
        if n + 1 >= MAX_POINTER_DEPTH {
            return None;
        }
        let segment = unescape(segment)?;
        at = match at {
            Json::Object(m) => m.get(&segment)?,
            Json::Array(a) => a.get(index(&segment)?)?,
            _ => return None,
        };
    }
    Some(at)
}

/// A pointer segment with its escapes undone; None for a `~` not followed by `0` or `1`.
fn unescape(segment: &str) -> Option<String> {
    let mut out = String::with_capacity(segment.len());
    let mut chars = segment.chars();
    while let Some(c) = chars.next() {
        out.push(match c {
            '~' => match chars.next()? {
                '0' => '~',
                '1' => '/',
                _ => return None,
            },
            c => c,
        });
    }
    Some(out)
}

/// RFC 6901's array index: `0`, or digits without a leading zero.
fn index(segment: &str) -> Option<usize> {
    let digits = !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit());
    (digits && (segment == "0" || !segment.starts_with('0'))).then(|| segment.parse().ok()).flatten()
}

/// An identity's subject and claim rules, checked when it's made: the shared-issuer rule first
/// ([`open_to_others`], never optional: a claim rule narrows a subject, it never stands in for
/// one), then the rules themselves. Roll secrets' identities and roll-access's trusted machine
/// clients both call this, so their rules are one. The error is the caller's message to show.
pub fn check_identity(issuer: &str, subject: &str, claims: &BTreeMap<String, String>) -> std::result::Result<(), String> {
    if let Some(shape) = open_to_others(issuer, subject) {
        return Err(format!("{issuer} signs for everyone: the subject names one of its tenants, {shape}"));
    }
    let usage = || format!("claims are at most {MAX_CLAIMS} other claims (not iss, aud, sub or the times), each a name or a JSON pointer (/realm_access/roles, at most {MAX_POINTER_DEPTH} segments) to a value or a pattern");
    if claims.len() > MAX_CLAIMS {
        return Err(usage());
    }
    for (key, want) in claims {
        let first = match key.strip_prefix('/') {
            Some(pointer) => {
                let segments: Vec<&str> = pointer.split('/').collect();
                if segments.len() > MAX_POINTER_DEPTH || segments.iter().any(|s| s.is_empty() || unescape(s).is_none()) {
                    return Err(format!("{key}: {}", usage()));
                }
                unescape(segments[0]).unwrap_or_default()
            }
            None => key.clone(),
        };
        if key.is_empty() || key.len() > 256 || want.is_empty() || want.len() > 256 || RESERVED.contains(&first.as_str()) {
            return Err(usage());
        }
    }
    Ok(())
}

/// Attempts per key in a sliding minute (in memory, per node: a cap on work, not an audit).
#[derive(Default)]
pub struct Window {
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
pub fn wildcard(pattern: &str, text: &str) -> bool {
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
    use super::{Jwt, MAX_CLAIM_ELEMENTS, Window, check_identity, open_to_others, wildcard};
    use serde_json::{Value as Json, json};
    use std::collections::BTreeMap;

    /// A token with `claims` (unsigned: matching never looks at the signature).
    fn jwt(claims: Json) -> Jwt<'static> {
        use base64::Engine;
        let b64 = |v: Json| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v.to_string());
        let token = format!("{}.{}.c2ln", b64(json!({ "alg": "ES256" })), b64(claims));
        Jwt::parse(Box::leak(token.into_boxed_str())).unwrap()
    }

    fn rules(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_window_counts_each_key_apart() {
        let w = Window::default();
        assert!(w.allow("identity:a", 2) && w.allow("identity:a", 2) && !w.allow("identity:a", 2));
        assert!(w.allow("identity:b", 2), "another key has its own count");
    }

    #[test]
    fn a_shared_issuer_needs_one_tenant_named() {
        let gh = "https://token.actions.githubusercontent.com";
        assert!(open_to_others(gh, "repo:acme/web:ref:refs/heads/main").is_none() && open_to_others(gh, "repo:acme/web:*").is_none());
        assert!(open_to_others(gh, "repo:*").is_some() && open_to_others(gh, "repo:acme/*:ref:x").is_some() && open_to_others(gh, "*").is_some());
        assert!(open_to_others("https://accounts.google.com", "1234*").is_some() && open_to_others("https://issuer.example.com", "*").is_none());
        // Azure: one issuer per organization or tenant, so one pipeline or service connection.
        let ado = "https://vstoken.dev.azure.com/00000000-0000-0000-0000-00000000a0b1";
        assert!(open_to_others(ado, "p://acme/shop/deploy").is_none() && open_to_others(ado, "sc://acme/shop/roll-prod").is_none());
        assert!(open_to_others(ado, "p://acme/shop/*").is_some() && open_to_others(ado, "p://acme/shop").is_some() && open_to_others(ado, "repo:acme/web:*").is_some());
        let entra = "https://login.microsoftonline.com/00000000-0000-0000-0000-00000000c0d2/v2.0";
        assert!(open_to_others(entra, "/eid1/c/pub/t/dGVzdA/a/dGVzdA/sc/org-1/sc-1").is_none() && open_to_others(entra, "/eid1/*").is_some());
    }

    #[test]
    fn claim_rules_reach_nested_and_list_claims() {
        let t = jwt(json!({
            "sub": "svc", "realm_access": { "roles": ["viewer", "deployer"] }, "groups": ["ops", "team-web"],
            "ref": "refs/heads/main", "a/b": { "c~d": "x" }, "run_attempt": 2, "email_verified": true, "nested": [["deployer"]],
        }));
        // A nested array (Keycloak's), a top-level one (Entra's groups, GitLab's groups_direct),
        // a plain name as before, and, by pointer, numbers and booleans by their JSON text.
        assert_eq!(t.unmatched(&rules(&[("/realm_access/roles", "deployer"), ("/groups", "team-*"), ("ref", "refs/heads/*")])), None);
        assert!(t.matches("/groups", "ops") && t.matches("/run_attempt", "2") && t.matches("/email_verified", "true") && t.matches("/groups/1", "team-web"));
        // A plain name matches only a string, as rules made before pointers did: one on a claim a
        // token sends as a list or a number doesn't start letting it in.
        assert!(!t.matches("groups", "ops") && !t.matches("run_attempt", "2") && !t.matches("email_verified", "true"));
        // No match: a role it doesn't have, a missing claim, an object, an array in an array.
        assert_eq!(t.unmatched(&rules(&[("/groups", "ops"), ("/realm_access/roles", "admin")])), Some("/realm_access/roles"));
        assert!(!t.matches("/realm_access/missing", "*") && !t.matches("/realm_access", "*") && !t.matches("/nested", "deployer"));
        assert!(!t.matches("/groups/01", "team-web") && !t.matches("/ref/0", "*"), "no leading zeros; a string has no elements");
        // Escapes: ~1 is /, ~0 is ~; anything else after ~ names nothing.
        assert!(t.matches("/a~1b/c~0d", "x") && !t.matches("/a/b/c~d", "x") && !t.matches("/a~2b/c~0d", "x"));
    }

    #[test]
    fn claim_rules_are_bounded() {
        // Depth: eight segments walk, a ninth doesn't (and isn't accepted when made).
        let deep = |n: usize| (0..n).fold(json!("x"), |v, _| json!({ "k": v }));
        let t = jwt(json!({ "d": deep(7), "many": (0..MAX_CLAIM_ELEMENTS + 1).map(|i| i.to_string()).collect::<Vec<_>>() }));
        assert!(t.matches(&format!("/d{}", "/k".repeat(7)), "x"));
        let t9 = jwt(json!({ "d": deep(8) }));
        assert!(!t9.matches(&format!("/d{}", "/k".repeat(8)), "x"));
        let issuer = "https://sso.example.com/realms/acme";
        assert!(check_identity(issuer, "svc", &rules(&[(&format!("/d{}", "/k".repeat(7)), "x")])).is_ok());
        assert!(check_identity(issuer, "svc", &rules(&[(&format!("/d{}", "/k".repeat(8)), "x")])).is_err());
        // Length: only the first 256 elements are looked at.
        assert!(t.matches("/many", "255") && !t.matches("/many", "256"));
        // Count, empty segments, bad escapes and the claims the token's checks own.
        let eleven: Vec<(String, String)> = (0..11).map(|i| (format!("c{i}"), "x".into())).collect();
        assert!(check_identity(issuer, "svc", &eleven.into_iter().collect()).is_err());
        for key in ["/a//b", "/a~2", "/", "/sub", "aud", "/exp/x"] {
            assert!(check_identity(issuer, "svc", &rules(&[(key, "x")])).is_err(), "{key}");
        }
    }

    #[test]
    fn claim_rules_never_stand_in_for_a_shared_issuers_subject() {
        let gh = "https://token.actions.githubusercontent.com";
        let narrowing = rules(&[("/repository_owner", "acme"), ("repository", "acme/web")]);
        assert!(check_identity(gh, "repo:*", &narrowing).is_err() && check_identity(gh, "*", &narrowing).is_err());
        assert!(check_identity("https://gitlab.com", "project_path:*", &rules(&[("/groups_direct", "acme")])).is_err());
        assert!(check_identity(gh, "repo:acme/web:*", &narrowing).is_ok());
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
