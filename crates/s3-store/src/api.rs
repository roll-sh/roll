//! The storage developer API's metadata (`s3-api`, a Supabase-Storage-compatible REST API next
//! to the S3 API): bucket limits, API projects and resumable (TUS) uploads.
//!
//! - **Bucket limits** (`file_size_limit`, `allowed_mime_types`) are a `bucket_config` row
//!   ([`LIMITS`]), copied to branches like the rest of a bucket's configuration. They're enforced
//!   inside the transaction that writes any new version ([`check_limits`], from
//!   `versioning::put_key`), so every write path honours them: the developer API, its resumable
//!   uploads, the S3 API (PUT, POST, copies, multipart, appends, renames) and the control plane.
//! - **A project** is an account's developer-API identity: an anon key and a service key (HS256
//!   JWTs signed with a key derived from the deployment's API key, [`KEY_STATE`]), and the auth
//!   providers whose user tokens the API accepts (an HS256 shared secret like Supabase's JWT
//!   secret, and OIDC issuers). The keys and the shared secret are kept sealed (AES-256-GCM, the
//!   session tokens' scheme) in `api_projects`; rotating the keys bumps the project's
//!   generation, which the keys carry, so old keys stop working at once.
//! - **A resumable upload** is a multipart upload (`uploads`, same id) plus an `api_uploads` row:
//!   the account, the declared length, the part size, whether it may overwrite, and when it
//!   expires. The row is written with the upload and removed in the transaction that completes
//!   or aborts it, so the two never disagree.

use crate::error::{Result, StoreError};
use crate::model::Upload;
use crate::store::{Store, find_bucket};
use crate::sts::OidcProvider;
use crate::targets::Secret;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Deserializer, Serialize};
use zeroize::Zeroize;

/// The `bucket_config` kind of a bucket's limits.
pub const LIMITS: &str = "limits";
/// The error code (`StoreError::Refused`) of a write whose content type the bucket refuses.
pub const INVALID_MIME_TYPE: &str = "InvalidMimeType";
/// The error code of a write larger than the bucket's `file_size_limit`.
pub const ENTITY_TOO_LARGE: &str = "EntityTooLarge";
/// The placeholder object a folder is made of (Supabase's): limits don't apply to it.
pub const FOLDER_PLACEHOLDER: &str = ".emptyFolderPlaceholder";
/// The `state` row holding the deployment's API key (64 hex digits).
pub const KEY_STATE: &str = "storage_api_key";
const SEAL_KID: u32 = 1;
/// Most providers a project may trust.
pub const MAX_OIDC_PROVIDERS: usize = 10;

/// A bucket's upload limits (Supabase's bucket settings).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BucketLimits {
    /// Largest object, in bytes; `None`: no limit beyond the API's own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_size_limit: Option<u64>,
    /// Media types objects may have (`image/png`, `image/*`); `None` (or empty): any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_mime_types: Option<Vec<String>>,
}

fn refused(code: &'static str, message: impl Into<String>) -> StoreError {
    StoreError::Refused { code, message: message.into() }
}

impl BucketLimits {
    pub fn is_empty(&self) -> bool {
        self.file_size_limit.is_none() && self.allowed_mime_types.as_ref().is_none_or(|m| m.is_empty())
    }

    /// An object of `size` bytes may be written as `key`.
    pub fn check_size(&self, key: &str, size: u64) -> Result<()> {
        match self.file_size_limit {
            Some(max) if size > max && !is_placeholder(key) => Err(refused(ENTITY_TOO_LARGE, "The object exceeded the maximum allowed size")),
            _ => Ok(()),
        }
    }

    /// An object with this `Content-Type` (none: `application/octet-stream`) may be written as
    /// `key`.
    pub fn check_type(&self, key: &str, content_type: Option<&str>) -> Result<()> {
        let Some(allowed) = self.allowed_mime_types.as_ref().filter(|a| !a.is_empty()) else { return Ok(()) };
        if is_placeholder(key) {
            return Ok(());
        }
        let ct = content_type.filter(|c| !c.trim().is_empty()).unwrap_or("application/octet-stream");
        if mime_allowed(ct, allowed) { Ok(()) } else { Err(refused(INVALID_MIME_TYPE, format!("mime type {ct} is not supported"))) }
    }

    /// Both checks (the size only when it's known).
    pub fn check(&self, key: &str, size: Option<u64>, content_type: Option<&str>) -> Result<()> {
        if let Some(n) = size {
            self.check_size(key, n)?;
        }
        self.check_type(key, content_type)
    }
}

fn is_placeholder(key: &str) -> bool {
    key.rsplit('/').next() == Some(FOLDER_PLACEHOLDER)
}

/// RFC 9110 `token` characters.
fn is_token(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+.^_`|~-".contains(&b))
}

/// The `type/subtype` of a media type (lowercase), its parameters checked and dropped.
/// `wildcard`: `type/*` is accepted (allowed-types lists).
pub fn media_type(s: &str, wildcard: bool) -> Option<String> {
    if s.len() > 1000 {
        return None;
    }
    let mut parts = s.split(';');
    let essence = parts.next()?.trim_matches([' ', '\t']);
    let (t, sub) = essence.split_once('/')?;
    let (t, sub) = (t.to_ascii_lowercase(), sub.to_ascii_lowercase());
    if !is_token(&t) || !is_token(&sub) || t.contains('*') || (sub.contains('*') && !(wildcard && sub == "*")) {
        return None;
    }
    for p in parts {
        let p = p.trim_matches([' ', '\t']);
        if p.is_empty() {
            continue;
        }
        let (k, v) = p.split_once('=')?;
        let quoted = v.len() >= 2 && v.starts_with('"') && v.ends_with('"');
        if !is_token(k) || !(is_token(v) || quoted) {
            return None;
        }
    }
    Some(format!("{t}/{sub}"))
}

/// Whether `content_type` is one of `allowed` (exactly, or by `type/*`).
pub fn mime_allowed(content_type: &str, allowed: &[String]) -> bool {
    let Some(want) = media_type(content_type, false) else { return false };
    let wild = format!("{}*", &want[..=want.find('/').unwrap_or(0)]);
    allowed.iter().any(|a| media_type(a, true).is_some_and(|a| a == want || a == wild))
}

/// An allowed-types list as stored: each media type normalized (`type/subtype` or `type/*`),
/// without duplicates. `Err` names the first invalid one.
pub fn normalize_mime_types(list: &[String]) -> std::result::Result<Vec<String>, String> {
    if list.len() > 100 {
        return Err("at most 100 allowed mime types".into());
    }
    let mut out: Vec<String> = Vec::new();
    for m in list.iter().filter(|m| !m.is_empty()) {
        let n = media_type(m, true).ok_or_else(|| m.clone())?;
        if !out.contains(&n) {
            out.push(n);
        }
    }
    Ok(out)
}

/// A `file_size_limit` as Supabase takes it: a number of bytes, or a string such as `"20MB"`,
/// `"1.5GB"`, `"30KB"`, `"3B"` (decimal units). `null`: no limit.
pub fn parse_size(v: &serde_json::Value) -> std::result::Result<Option<u64>, String> {
    const HINT: &str = "Invalid file size format, hint: use 20GB / 20MB / 30KB / 3B";
    match v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::Number(n) => n.as_u64().map(Some).ok_or_else(|| HINT.to_string()),
        serde_json::Value::String(s) => {
            let lower = s.trim().to_ascii_lowercase();
            let (num, mult) = if let Some(n) = lower.strip_suffix("gb") {
                (n, 1e9)
            } else if let Some(n) = lower.strip_suffix("mb") {
                (n, 1e6)
            } else if let Some(n) = lower.strip_suffix("kb") {
                (n, 1e3)
            } else if let Some(n) = lower.strip_suffix('b') {
                (n, 1.0)
            } else if !lower.is_empty() && lower.bytes().all(|b| b.is_ascii_digit()) {
                (lower.as_str(), 1.0)
            } else {
                return Err(HINT.into());
            };
            let valid = !num.is_empty() && num.bytes().all(|b| b.is_ascii_digit() || b == b'.') && num.matches('.').count() <= 1 && !num.starts_with('.') && !num.ends_with('.');
            if !valid {
                return Err(HINT.into());
            }
            let f: f64 = num.parse().map_err(|_| HINT.to_string())?;
            Ok(Some((f * mult).round() as u64))
        }
        _ => Err(HINT.into()),
    }
}

fn limits_row(c: &Connection, bucket_id: &str) -> Result<Option<BucketLimits>> {
    let v: Option<String> = c.prepare_cached("SELECT value FROM bucket_config WHERE bucket_id = ?1 AND kind = ?2 AND id = ''")?.query_row(params![bucket_id, LIMITS], |r| r.get(0)).optional()?;
    Ok(match v {
        Some(v) => Some(serde_json::from_str(&v)?),
        None => None,
    })
}

/// The bucket's limits allow a new version of `key` (inside the transaction that writes it).
pub(crate) fn check_limits(c: &Connection, bucket_id: &str, key: &str, size: u64, content_type: Option<&str>) -> Result<()> {
    match limits_row(c, bucket_id)? {
        Some(l) => l.check(key, Some(size), content_type),
        None => Ok(()),
    }
}

// ---- JWTs ----

/// HS256 JWTs: what the API keys and signed URLs are, and what HS256 user tokens are checked
/// with. No I/O.
pub mod jwt {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use hmac::{Hmac, Mac};
    use serde_json::{Map, Value};
    use sha2::Sha256;

    /// Longest token taken.
    pub const MAX_TOKEN: usize = 16 * 1024;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum JwtError {
        Malformed(&'static str),
        /// Signed with another algorithm (named) than the one checked.
        Algorithm(String),
        Signature,
        Expired,
        NotYetValid,
    }

    impl std::fmt::Display for JwtError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                JwtError::Malformed(m) => write!(f, "{m}"),
                JwtError::Algorithm(a) => write!(f, "unsupported JWT algorithm {a}"),
                JwtError::Signature => f.write_str("invalid signature"),
                JwtError::Expired => f.write_str("\"exp\" claim timestamp check failed"),
                JwtError::NotYetValid => f.write_str("\"nbf\" claim timestamp check failed"),
            }
        }
    }

    /// A token split and decoded (not verified).
    pub struct Decoded {
        pub alg: String,
        pub kid: Option<String>,
        pub claims: Map<String, Value>,
        /// `header.payload`, what the signature covers.
        pub signed: String,
        pub signature: Vec<u8>,
    }

    pub fn decode(token: &str) -> Result<Decoded, JwtError> {
        let token = token.trim();
        if token.len() > MAX_TOKEN {
            return Err(JwtError::Malformed("jwt too long"));
        }
        let mut parts = token.split('.');
        let (Some(h), Some(p), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else { return Err(JwtError::Malformed("jwt malformed")) };
        let header: Value = URL_SAFE_NO_PAD.decode(h).ok().and_then(|b| serde_json::from_slice(&b).ok()).ok_or(JwtError::Malformed("invalid jwt header"))?;
        if header.get("crit").is_some() {
            return Err(JwtError::Malformed("critical header parameters aren't supported"));
        }
        let alg = header.get("alg").and_then(Value::as_str).ok_or(JwtError::Malformed("jwt header has no alg"))?.to_string();
        let kid = header.get("kid").and_then(Value::as_str).map(str::to_string);
        let claims = match URL_SAFE_NO_PAD.decode(p).ok().and_then(|b| serde_json::from_slice::<Value>(&b).ok()) {
            Some(Value::Object(m)) => m,
            _ => return Err(JwtError::Malformed("invalid jwt payload")),
        };
        let signature = URL_SAFE_NO_PAD.decode(s).map_err(|_| JwtError::Malformed("invalid jwt signature encoding"))?;
        Ok(Decoded { alg, kid, claims, signed: format!("{h}.{p}"), signature })
    }

    fn mac(key: &[u8]) -> Hmac<Sha256> {
        <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length")
    }

    /// A compact HS256 JWT of `claims`.
    pub fn sign_hs256(key: &[u8], claims: &Value) -> String {
        let h = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
        let p = URL_SAFE_NO_PAD.encode(serde_json::to_vec(claims).expect("JSON"));
        let mut m = mac(key);
        m.update(h.as_bytes());
        m.update(b".");
        m.update(p.as_bytes());
        let sig = URL_SAFE_NO_PAD.encode(m.finalize().into_bytes());
        format!("{h}.{p}.{sig}")
    }

    /// Whether a decoded token's HS256 signature is `key`'s (compared in constant time).
    pub fn hs256_valid(key: &[u8], d: &Decoded) -> bool {
        let mut m = mac(key);
        m.update(d.signed.as_bytes());
        m.verify_slice(&d.signature).is_ok()
    }

    /// The claims of an HS256 token signed with `key` (times not checked: see [`check_times`]).
    pub fn verify_hs256(key: &[u8], token: &str) -> Result<Map<String, Value>, JwtError> {
        let d = decode(token)?;
        if d.alg != "HS256" {
            return Err(JwtError::Algorithm(d.alg));
        }
        if !hs256_valid(key, &d) {
            return Err(JwtError::Signature);
        }
        Ok(d.claims)
    }

    /// `exp` and `nbf` (when present) against `now_s`, with `leeway_s` either way.
    pub fn check_times(claims: &Map<String, Value>, now_s: i64, leeway_s: i64) -> Result<(), JwtError> {
        let time = |k: &str| -> Result<Option<i64>, JwtError> {
            match claims.get(k) {
                None | Some(Value::Null) => Ok(None),
                Some(v) => v.as_f64().map(|f| Some(f as i64)).ok_or(JwtError::Malformed("jwt time claims must be numbers")),
            }
        };
        if time("exp")?.is_some_and(|exp| now_s >= exp + leeway_s) {
            return Err(JwtError::Expired);
        }
        if time("nbf")?.is_some_and(|nbf| now_s + leeway_s < nbf) {
            return Err(JwtError::NotYetValid);
        }
        Ok(())
    }
}

// ---- Projects ----

/// A project's API key role.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiRole {
    /// The anonymous role (`anon`): what bucket policies grant `{"App": "anon"}`.
    Anon,
    /// The service role (`service_role`): bypasses rules, as the account's root user.
    Service,
}

impl ApiRole {
    pub fn as_str(self) -> &'static str {
        match self {
            ApiRole::Anon => "anon",
            ApiRole::Service => "service_role",
        }
    }
}

/// A verified API key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiKey {
    pub account: String,
    pub role: ApiRole,
}

/// Why an API key was refused.
#[derive(Debug)]
pub enum ApiKeyError {
    /// It claims to be a roll key but doesn't verify, or was rotated away.
    Invalid(String),
    Store(StoreError),
}

/// What's stored per project (secrets sealed).
#[derive(Default, Serialize, Deserialize)]
struct ProjectDoc {
    generation: u32,
    issued_at: i64,
    anon_key: String,
    service_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    jwt_secret: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    jwt_issuer: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    jwt_audience: Option<String>,
    #[serde(default)]
    oidc: Vec<OidcProvider>,
}

/// An account's developer-API project, secrets opened. `Debug` never shows them.
#[derive(Debug, Clone)]
pub struct ApiProject {
    pub account: String,
    /// Bumped by every key rotation; the keys carry it.
    pub generation: u32,
    /// When the current keys were made (Unix ms).
    pub issued_at: i64,
    pub updated_at: i64,
    pub anon_key: Secret,
    pub service_key: Secret,
    /// The HS256 secret user tokens may be signed with (Supabase's JWT secret).
    pub jwt_secret: Option<Secret>,
    /// HS256 user tokens must have this `iss`, when set.
    pub jwt_issuer: Option<String>,
    /// HS256 user tokens must name this `aud`, when set.
    pub jwt_audience: Option<String>,
    /// OIDC issuers whose (RS256/ES256) tokens are accepted.
    pub oidc: Vec<OidcProvider>,
}

fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

/// A change to a project's auth providers: omitted fields stay, `null` removes.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthUpdate {
    #[serde(default, deserialize_with = "present")]
    pub jwt_secret: Option<Option<Secret>>,
    #[serde(default, deserialize_with = "present")]
    pub jwt_issuer: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    pub jwt_audience: Option<Option<String>>,
    #[serde(default)]
    pub oidc: Option<Vec<OidcProvider>>,
}

impl std::fmt::Debug for AuthUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthUpdate").field("jwt_issuer", &self.jwt_issuer).field("jwt_audience", &self.jwt_audience).field("oidc", &self.oidc).finish_non_exhaustive()
    }
}

/// Years an API key lasts (Supabase's are ten).
const KEY_YEARS: i64 = 10;

fn hmac32(key: &[u8], label: &[u8]) -> [u8; 32] {
    use hmac::{Hmac, Mac};
    let mut m = <Hmac<sha2::Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    m.update(label);
    m.finalize().into_bytes().into()
}

fn read_master(c: &Connection) -> Result<Option<[u8; 32]>> {
    let v: Option<String> = c.prepare_cached("SELECT v FROM state WHERE k = ?1")?.query_row([KEY_STATE], |r| r.get(0)).optional()?;
    let Some(mut v) = v else { return Ok(None) };
    let mut out = [0u8; 32];
    let ok = hex::decode_to_slice(&v, &mut out).is_ok();
    v.zeroize();
    if !ok {
        return Err(StoreError::internal("the storage API key doesn't parse"));
    }
    Ok(Some(out))
}

fn project_row(c: &Connection, account: &str) -> Result<Option<(ProjectDoc, i64)>> {
    let r: Option<(String, i64)> = c.prepare_cached("SELECT value, updated_at FROM api_projects WHERE account_id = ?1")?.query_row([account], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
    Ok(match r {
        Some((v, at)) => Some((serde_json::from_str(&v)?, at)),
        None => None,
    })
}

fn invalid(m: impl Into<String>) -> StoreError {
    StoreError::InvalidArgument(m.into())
}

fn check_oidc(list: &[OidcProvider]) -> Result<()> {
    if list.len() > MAX_OIDC_PROVIDERS {
        return Err(invalid(format!("at most {MAX_OIDC_PROVIDERS} OIDC providers")));
    }
    for p in list {
        let u = url::Url::parse(&p.issuer).map_err(|_| invalid(format!("issuer {:?} isn't a URL", p.issuer)))?;
        if !matches!(u.scheme(), "https" | "http") || u.host_str().is_none() {
            return Err(invalid(format!("issuer {:?} must be an https:// URL", p.issuer)));
        }
        if p.audiences.is_empty() || p.audiences.len() > 20 || p.audiences.iter().any(|a| a.is_empty() || a.len() > 255) {
            return Err(invalid(format!("issuer {}: give 1-20 audiences (the tokens' aud)", p.issuer)));
        }
        if let Some(j) = &p.jwks_uri {
            let u = url::Url::parse(j).map_err(|_| invalid(format!("jwks_uri {j:?} isn't a URL")))?;
            if !matches!(u.scheme(), "https" | "http") {
                return Err(invalid(format!("jwks_uri {j:?} must be an https:// URL")));
            }
        }
    }
    Ok(())
}

impl Store {
    /// The deployment's API key, created on first use.
    async fn api_master(&self) -> Result<[u8; 32]> {
        if let Some(k) = self.read(read_master)? {
            return Ok(k);
        }
        let mut k = [0u8; 32];
        getrandom::fill(&mut k).map_err(|e| StoreError::internal(format!("randomness: {e}")))?;
        let mut hexed = hex::encode(k);
        k.zeroize();
        let r = self
            .write(|c| {
                c.execute("INSERT INTO state (k, v) VALUES (?1, ?2) ON CONFLICT (k) DO NOTHING", params![KEY_STATE, hexed])?;
                Ok(())
            })
            .await;
        hexed.zeroize();
        r?;
        self.read(read_master)?.ok_or_else(|| StoreError::internal("the storage API key vanished"))
    }

    /// The key signed URLs of the developer API are signed with (HS256), derived from the
    /// deployment's API key.
    pub async fn api_url_key(&self) -> Result<[u8; 32]> {
        Ok(hmac32(&self.api_master().await?, b"roll storage api signed urls v1"))
    }

    fn keys_key(master: &[u8; 32]) -> [u8; 32] {
        hmac32(master, b"roll storage api keys v1")
    }

    fn seal_key(master: &[u8; 32]) -> [u8; 32] {
        hmac32(master, b"roll storage api sealed secrets v1")
    }

    fn open_doc(&self, master: &[u8; 32], account: &str, doc: ProjectDoc, updated_at: i64) -> Result<ApiProject> {
        let sk = Self::seal_key(master);
        let open = |sealed: &str| -> Result<Secret> {
            let mut plain = crate::sts::open(&[(SEAL_KID, sk)], sealed).ok_or_else(|| StoreError::internal("a sealed API project secret doesn't open"))?;
            let s = String::from_utf8(plain.clone()).map_err(|_| StoreError::internal("a sealed API project secret isn't text"));
            plain.zeroize();
            Ok(Secret::new(s?))
        };
        Ok(ApiProject {
            account: account.to_string(),
            generation: doc.generation,
            issued_at: doc.issued_at,
            updated_at,
            anon_key: open(&doc.anon_key)?,
            service_key: open(&doc.service_key)?,
            jwt_secret: doc.jwt_secret.as_deref().map(open).transpose()?,
            jwt_issuer: doc.jwt_issuer,
            jwt_audience: doc.jwt_audience,
            oidc: doc.oidc,
        })
    }

    /// Mint the keys of `generation` into `doc`.
    fn mint_keys(&self, master: &[u8; 32], account: &str, generation: u32, doc: &mut ProjectDoc) {
        let now_s = self.now().div_euclid(1000);
        let kk = Self::keys_key(master);
        let sk = Self::seal_key(master);
        let key = |role: ApiRole| {
            let claims = serde_json::json!({ "iss": "roll", "ref": account, "role": role.as_str(), "gen": generation, "iat": now_s, "exp": now_s + KEY_YEARS * 365 * 86_400 });
            let mut t = jwt::sign_hs256(&kk, &claims);
            let sealed = crate::sts::seal(SEAL_KID, &sk, t.as_bytes());
            t.zeroize();
            sealed
        };
        doc.generation = generation;
        doc.issued_at = self.now();
        doc.anon_key = key(ApiRole::Anon);
        doc.service_key = key(ApiRole::Service);
    }

    /// The account's project, made (with fresh keys) on first use.
    pub async fn api_project(&self, account: &str) -> Result<ApiProject> {
        self.require_account(account)?;
        let master = self.api_master().await?;
        if let Some((doc, at)) = self.read(|c| project_row(c, account))? {
            return self.open_doc(&master, account, doc, at);
        }
        let mut doc = ProjectDoc::default();
        self.mint_keys(&master, account, 1, &mut doc);
        let text = serde_json::to_string(&doc)?;
        let now = self.now();
        self.write(|c| {
            c.execute("INSERT INTO api_projects (account_id, value, updated_at) VALUES (?1, ?2, ?3) ON CONFLICT (account_id) DO NOTHING", params![account, text, now])?;
            Ok(())
        })
        .await?;
        let (doc, at) = self.read(|c| project_row(c, account))?.ok_or_else(|| StoreError::internal("the API project vanished"))?;
        self.open_doc(&master, account, doc, at)
    }

    /// The account's project if it has one (nothing is created).
    pub fn api_project_if_any(&self, account: &str) -> Result<Option<ApiProject>> {
        let Some(master) = self.read(read_master)? else { return Ok(None) };
        match self.read(|c| project_row(c, account))? {
            Some((doc, at)) => Ok(Some(self.open_doc(&master, account, doc, at)?)),
            None => Ok(None),
        }
    }

    /// New anon and service keys: the old ones stop working at once.
    pub async fn rotate_api_keys(&self, account: &str) -> Result<ApiProject> {
        self.api_project(account).await?;
        let master = self.api_master().await?;
        let now = self.now();
        self.write(|c| {
            let (mut doc, _) = project_row(c, account)?.ok_or_else(|| StoreError::NotFound(format!("account {account} has no API project")))?;
            let next = doc.generation + 1;
            self.mint_keys(&master, account, next, &mut doc);
            c.execute("UPDATE api_projects SET value = ?2, updated_at = ?3 WHERE account_id = ?1", params![account, serde_json::to_string(&doc)?, now])?;
            Ok(())
        })
        .await?;
        self.api_project(account).await
    }

    /// Change the project's auth providers.
    pub async fn set_api_auth(&self, account: &str, u: AuthUpdate) -> Result<ApiProject> {
        if let Some(Some(s)) = &u.jwt_secret
            && !(32..=512).contains(&s.expose().len())
        {
            return Err(invalid("jwt_secret must be 32-512 characters"));
        }
        for v in [&u.jwt_issuer, &u.jwt_audience].into_iter().flatten().flatten() {
            if v.is_empty() || v.len() > 1024 {
                return Err(invalid("jwt_issuer and jwt_audience are 1-1024 characters"));
            }
        }
        if let Some(list) = &u.oidc {
            check_oidc(list)?;
        }
        self.api_project(account).await?;
        let master = self.api_master().await?;
        let sk = Self::seal_key(&master);
        let sealed = match &u.jwt_secret {
            Some(Some(s)) => Some(Some(crate::sts::seal(SEAL_KID, &sk, s.expose().as_bytes()))),
            Some(None) => Some(None),
            None => None,
        };
        let now = self.now();
        self.write(|c| {
            let (mut doc, _) = project_row(c, account)?.ok_or_else(|| StoreError::NotFound(format!("account {account} has no API project")))?;
            if let Some(s) = &sealed {
                doc.jwt_secret = s.clone();
            }
            if let Some(v) = &u.jwt_issuer {
                doc.jwt_issuer = v.clone();
            }
            if let Some(v) = &u.jwt_audience {
                doc.jwt_audience = v.clone();
            }
            if let Some(list) = &u.oidc {
                doc.oidc = list.clone();
            }
            c.execute("UPDATE api_projects SET value = ?2, updated_at = ?3 WHERE account_id = ?1", params![account, serde_json::to_string(&doc)?, now])?;
            Ok(())
        })
        .await?;
        self.api_project(account).await
    }

    /// Check an API key. `Ok(None)`: not a roll API key at all (`iss` isn't `roll`); `Err`: a
    /// roll key that doesn't verify, or was rotated away, or whose account is gone.
    pub fn verify_api_key(&self, token: &str) -> std::result::Result<Option<ApiKey>, ApiKeyError> {
        let bad = |m: &str| ApiKeyError::Invalid(m.to_string());
        let Ok(d) = jwt::decode(token) else { return Ok(None) };
        if d.claims.get("iss").and_then(|v| v.as_str()) != Some("roll") || d.alg != "HS256" {
            return Ok(None);
        }
        let master = self.read(read_master).map_err(ApiKeyError::Store)?.ok_or_else(|| bad("invalid API key"))?;
        if !jwt::hs256_valid(&Self::keys_key(&master), &d) {
            return Err(bad("invalid API key: signature verification failed"));
        }
        let account = d.claims.get("ref").and_then(|v| v.as_str()).ok_or_else(|| bad("invalid API key"))?.to_string();
        let role = match d.claims.get("role").and_then(|v| v.as_str()) {
            Some("anon") => ApiRole::Anon,
            Some("service_role") => ApiRole::Service,
            _ => return Err(bad("invalid API key role")),
        };
        let generation = d.claims.get("gen").and_then(|v| v.as_u64()).ok_or_else(|| bad("invalid API key"))?;
        let doc = self.read(|c| project_row(c, &account)).map_err(ApiKeyError::Store)?;
        match doc {
            Some((doc, _)) if u64::from(doc.generation) == generation => Ok(Some(ApiKey { account, role })),
            Some(_) => Err(bad("this API key was rotated: use the project's current keys")),
            None => Err(bad("invalid API key: the project doesn't exist")),
        }
    }

    /// A bucket's limits (none set: empty).
    pub fn bucket_limits(&self, bucket: &str) -> Result<BucketLimits> {
        self.read(|c| {
            let b = find_bucket(c, bucket)?;
            Ok(limits_row(c, &b.id)?.unwrap_or_default())
        })
    }

    /// Set a bucket's limits (empty limits remove the row). The allowed types are normalized.
    pub async fn set_bucket_limits(&self, bucket: &str, limits: &BucketLimits) -> Result<BucketLimits> {
        let mut l = limits.clone();
        if let Some(m) = &l.allowed_mime_types {
            let n = normalize_mime_types(m).map_err(|bad| refused(INVALID_MIME_TYPE, format!("mime type {bad} is not supported")))?;
            l.allowed_mime_types = if n.is_empty() { None } else { Some(n) };
        }
        let value = if l.is_empty() { None } else { Some(serde_json::to_string(&l)?) };
        self.set_bucket_config(bucket, LIMITS, "", value.as_deref(), None).await?;
        Ok(l)
    }

    // ---- Resumable uploads ----

    /// A resumable upload, with its multipart upload (`None`: no such upload).
    pub fn api_upload(&self, id: &str) -> Result<Option<ApiUpload>> {
        self.read(|c| {
            let row: Option<(String, Option<i64>, i64, String, i64, i64)> = c
                .prepare_cached("SELECT account_id, length, part_size, value, created_at, expires_at FROM api_uploads WHERE id = ?1")?
                .query_row([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))
                .optional()?;
            let Some((account, length, part_size, value, created_at, expires_at)) = row else { return Ok(None) };
            let upload = match crate::objects::find_upload_row(c, id) {
                Ok(u) => u,
                Err(StoreError::NoSuchUpload(_)) => return Ok(None),
                Err(e) => return Err(e),
            };
            let bucket: String = c.query_row("SELECT name FROM buckets WHERE id = ?1", [&upload.bucket_id], |r| r.get(0))?;
            Ok(Some(ApiUpload { id: id.to_string(), account, bucket, length: length.map(|l| l as u64), part_size: part_size as u64, info: serde_json::from_str(&value)?, created_at, expires_at, upload }))
        })
    }

    /// Declare a deferred upload's length (once).
    pub async fn set_api_upload_length(&self, id: &str, length: u64) -> Result<()> {
        self.write(|c| {
            let n = c.execute("UPDATE api_uploads SET length = ?2 WHERE id = ?1 AND length IS NULL", params![id, length as i64])?;
            if n == 0 {
                return Err(StoreError::InvalidRequest("the upload's length is already set".into()));
            }
            Ok(())
        })
        .await
    }

    /// Resumable uploads that expired before `now` (Unix ms), oldest first.
    pub fn expired_api_uploads(&self, now: i64, limit: usize) -> Result<Vec<String>> {
        self.read(|c| Ok(c.prepare_cached("SELECT id FROM api_uploads WHERE expires_at < ?1 ORDER BY expires_at LIMIT ?2")?.query_map(params![now, limit as i64], |r| r.get(0))?.collect::<rusqlite::Result<Vec<String>>>()?))
    }
}

/// What a new resumable upload records besides its multipart upload.
#[derive(Clone, Debug)]
pub struct NewApiUpload {
    pub account: String,
    /// `Upload-Length`; `None`: deferred.
    pub length: Option<u64>,
    pub part_size: u64,
    pub expires_at: i64,
    pub info: ApiUploadInfo,
}

/// A resumable upload's details (JSON in `api_uploads.value`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ApiUploadInfo {
    /// It may replace an existing object (`x-upsert: true`).
    pub upsert: bool,
    /// The `Upload-Metadata` header it was created with (HEAD echoes it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<String>,
    /// Made through a signed upload URL (`x-signature`) rather than by an authorized caller.
    pub signed: bool,
}

/// A resumable upload.
#[derive(Clone, Debug)]
pub struct ApiUpload {
    pub id: String,
    pub account: String,
    pub bucket: String,
    pub length: Option<u64>,
    pub part_size: u64,
    pub info: ApiUploadInfo,
    pub created_at: i64,
    pub expires_at: i64,
    pub upload: Upload,
}

pub(crate) fn insert_upload(c: &Connection, id: &str, a: &NewApiUpload, now: i64) -> Result<()> {
    c.execute(
        "INSERT INTO api_uploads (id, account_id, length, part_size, value, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, a.account, a.length.map(|l| l as i64), a.part_size as i64, serde_json::to_string(&a.info)?, now, a.expires_at],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_types() {
        assert_eq!(media_type("image/PNG; charset=utf-8", false).as_deref(), Some("image/png"));
        assert_eq!(media_type("text/plain;charset=\"utf-8\"", false).as_deref(), Some("text/plain"));
        assert_eq!(media_type("image/*", false), None);
        assert_eq!(media_type("image/*", true).as_deref(), Some("image/*"));
        assert_eq!(media_type("*/*", true), None);
        assert_eq!(media_type("image", false), None);
        assert_eq!(media_type("image/png;", false).as_deref(), Some("image/png"));
        assert_eq!(media_type("image/png; x", false), None);
        let allowed = vec!["image/*".to_string(), "application/pdf".to_string()];
        assert!(mime_allowed("image/jpeg", &allowed));
        assert!(mime_allowed("Application/PDF; q=1", &allowed));
        assert!(!mime_allowed("text/plain", &allowed));
        assert!(!mime_allowed("garbage", &allowed));
        assert_eq!(normalize_mime_types(&["Image/PNG".into(), "image/png".into(), "".into()]).unwrap(), vec!["image/png"]);
        assert_eq!(normalize_mime_types(&["no".into()]).unwrap_err(), "no");
    }

    #[test]
    fn sizes() {
        use serde_json::json;
        assert_eq!(parse_size(&json!(100)).unwrap(), Some(100));
        assert_eq!(parse_size(&json!(null)).unwrap(), None);
        assert_eq!(parse_size(&json!("20MB")).unwrap(), Some(20_000_000));
        assert_eq!(parse_size(&json!("1.5gb")).unwrap(), Some(1_500_000_000));
        assert_eq!(parse_size(&json!("30KB")).unwrap(), Some(30_000));
        assert_eq!(parse_size(&json!("3B")).unwrap(), Some(3));
        assert_eq!(parse_size(&json!("42")).unwrap(), Some(42));
        assert!(parse_size(&json!("x")).is_err());
        assert!(parse_size(&json!("1.MB")).is_err());
        assert!(parse_size(&json!(-1)).is_err());
    }

    #[test]
    fn limits() {
        let l = BucketLimits { file_size_limit: Some(10), allowed_mime_types: Some(vec!["image/*".into()]) };
        assert!(l.check("a.png", Some(10), Some("image/png")).is_ok());
        assert!(matches!(l.check("a.png", Some(11), Some("image/png")), Err(StoreError::Refused { code: ENTITY_TOO_LARGE, .. })));
        assert!(matches!(l.check("a.txt", Some(1), Some("text/plain")), Err(StoreError::Refused { code: INVALID_MIME_TYPE, .. })));
        assert!(matches!(l.check("a", Some(1), None), Err(StoreError::Refused { code: INVALID_MIME_TYPE, .. })));
        assert!(l.check("dir/.emptyFolderPlaceholder", Some(0), Some("text/plain")).is_ok());
        assert!(BucketLimits::default().is_empty());
        assert!(BucketLimits { allowed_mime_types: Some(vec![]), ..Default::default() }.is_empty());
    }

    #[test]
    fn hs256() {
        let t = jwt::sign_hs256(b"secret", &serde_json::json!({"sub": "u", "exp": 100}));
        let c = jwt::verify_hs256(b"secret", &t).unwrap();
        assert_eq!(c["sub"], "u");
        assert_eq!(jwt::verify_hs256(b"other", &t).unwrap_err(), jwt::JwtError::Signature);
        assert_eq!(jwt::check_times(&c, 99, 0), Ok(()));
        assert_eq!(jwt::check_times(&c, 100, 0), Err(jwt::JwtError::Expired));
        let mut tampered = t.clone();
        tampered.insert(t.find('.').unwrap() + 2, 'x');
        assert!(jwt::verify_hs256(b"secret", &tampered).is_err());
        assert!(matches!(jwt::decode("a.b"), Err(jwt::JwtError::Malformed(_))));
    }
}
