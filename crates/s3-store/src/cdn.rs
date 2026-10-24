//! The CDN's metadata: each bucket's CDN setting, the deployment's Cloudflare configuration
//! (its API token sealed), the purges changes queue through the event outbox, and the cached
//! egress the edge Worker reports.
//!
//! - **Purges are events.** A change to an object of a bucket with the CDN on (a new version, a
//!   delete, an ACL change, a restore) queues a [`PURGE`] row in the *same* transaction as the
//!   change (`events::emit`), so a committed change is always purged, at least once, and only
//!   after it's durable. Changes that can make a whole bucket more private (its policy, ACL,
//!   public access block, ownership or public flag, the CDN setting itself, deleting the bucket)
//!   queue a bucket-wide purge.
//! - **Usage is idempotent.** The Worker reports bytes per bucket in batches with ids;
//!   [`Store::record_cdn_usage`] counts a batch id once, in the same durable write as the usage.

use crate::error::{Result, StoreError};
use crate::store::{BucketRow, Store};
use crate::targets::Secret;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The `bucket_config` kind of a bucket's CDN setting.
pub const KIND: &str = "cdn";
/// Outbox row kind of a purge.
pub const PURGE: &str = "cdn_purge";
/// `state` key of the deployment's CDN settings.
const SETTINGS: &str = "cdn_settings";
/// Usage batch ids are remembered this long (a Worker retries for far less).
const BATCH_MEMORY_MS: i64 = 7 * 86_400_000;
/// Most entries one usage batch may carry.
pub const MAX_USAGE_ENTRIES: usize = 10_000;

/// How a bucket's objects are addressed through the CDN.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CdnMode {
    /// By key (`/<bucket>/<key>`): a short edge TTL, purged on every change.
    #[default]
    Mutable,
    /// By version (`/v/<token>/<bucket>/<key>`): cached for a year, never stale.
    Immutable,
}

/// A bucket's CDN setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BucketCdn {
    pub enabled: bool,
    pub mode: CdnMode,
}

/// What a [`PURGE`] row carries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PurgeTask {
    /// The bucket's name (the row's key is the object's).
    pub bucket: String,
    /// The whole bucket (the row's key is empty).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
}

/// How objects are purged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PurgeMode {
    /// By URL (`files`): every mutable URL form of the object, public and credentialed.
    #[default]
    Files,
    /// By cache tag (`rk-…`): every URL form, immutable ones included.
    Tags,
}

/// A path prefix the edge Worker serves, as it's configured there (`ROLL_ROUTES`). roll uses the
/// same list to know every URL an object is cached under.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CdnRoute {
    pub prefix: String,
    /// `s3` (`<prefix><bucket>/<key>`, S3 semantics), `immutable` (`<prefix><token>/<bucket>/<key>`),
    /// `bucket-key` (`<prefix><bucket>/<key>`, another API's semantics), `website`
    /// (`<prefix><key>` of one bucket's website) or `none` (never served through the CDN: 404
    /// at the edge).
    pub kind: String,
    /// `any`, `public` or `token`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth: Option<String>,
    /// The origin for this prefix (default: the Worker's `ROLL_ORIGIN`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// `website` routes: the bucket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bucket: Option<String>,
    /// `bucket-key` routes: pass the request's `?token=` to the origin.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub forward_token: bool,
    /// `none`: never stored at the edge.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache: Option<String>,
    /// A route of the storage developer API (`/storage/v1/…`): its origin is the Worker's
    /// `ROLL_API_ORIGIN` unless `origin` is set, and its copies are purged by cache tag (their
    /// URLs vary with transformations, tokens and `Accept`, so they can't be listed).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub api: bool,
    /// Image renditions: the edge keys copies by whether the client accepts WebP (no `format`
    /// means WebP when it's accepted), as Cloudflare's cache ignores `Vary: Accept`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub vary_accept: bool,
}

impl CdnRoute {
    pub fn new(prefix: &str, kind: &str) -> Self {
        Self { prefix: prefix.into(), kind: kind.into(), ..Default::default() }
    }

    /// Whether credentialed copies may be cached under this route (a private namespace).
    pub fn credentialed(&self) -> bool {
        self.auth.as_deref() != Some("public") && !matches!(self.kind.as_str(), "website" | "none")
    }

    /// A route of the storage developer API, under [`API_PREFIX`].
    fn api_route(prefix: &str, kind: &str, auth: &str) -> Self {
        Self { prefix: format!("{API_PREFIX}{prefix}"), kind: kind.into(), auth: Some(auth.into()), api: true, ..Default::default() }
    }
}

/// Where the storage developer API is mounted (`s3_api::PREFIX`, with its `/`).
pub const API_PREFIX: &str = "/storage/v1/";

/// The developer API's routes: public and signed reads of objects and image renditions, and
/// immutable URLs (`/storage/v1/v/<token>/<bucket>/<key>`). Nothing else under the API is served
/// at the edge: authenticated requests (a user's JWT) never go through the CDN.
pub fn api_routes() -> Vec<CdnRoute> {
    let token = |mut r: CdnRoute| {
        r.forward_token = true;
        r
    };
    let images = |mut r: CdnRoute| {
        r.vary_accept = true;
        r
    };
    vec![
        CdnRoute::api_route("", "none", "public"),
        CdnRoute::api_route("object/public/", "bucket-key", "public"),
        token(CdnRoute::api_route("object/sign/", "bucket-key", "token")),
        images(CdnRoute::api_route("render/image/public/", "bucket-key", "public")),
        images(token(CdnRoute::api_route("render/image/sign/", "bucket-key", "token"))),
        CdnRoute::api_route("v/", "immutable", "public"),
    ]
}

/// The routes when none are configured: immutable URLs under `/v/`, S3 paths for the rest, and
/// the developer API's reads ([`api_routes`]).
pub fn default_routes() -> Vec<CdnRoute> {
    let mut r = vec![CdnRoute::new("/v/", "immutable"), CdnRoute::new("/", "s3")];
    r.extend(api_routes());
    r
}

/// The deployment's CDN settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct CdnSettings {
    /// What clients use, e.g. `https://cdn.example.com`; `None`: no CDN.
    pub base_url: Option<String>,
    /// The Cloudflare zone of the hostname.
    pub zone_id: Option<String>,
    /// The Cloudflare API token (`Zone.Cache Purge`); sealed at rest, never shown.
    #[serde(skip)]
    pub api_token: Option<Secret>,
    pub purge: PurgeMode,
    pub routes: Vec<CdnRoute>,
    /// Edge TTL of mutable URLs (they're purged on change too).
    pub mutable_ttl_s: u32,
    /// URLs, tags or prefixes per purge request (Cloudflare's per-request limit for the plan).
    pub purge_batch: u32,
    /// Purge requests per minute at most (Cloudflare's rate limit for the plan).
    pub purge_per_minute: u32,
}

impl Default for CdnSettings {
    fn default() -> Self {
        Self { base_url: None, zone_id: None, api_token: None, purge: PurgeMode::Files, routes: default_routes(), mutable_ttl_s: 60, purge_batch: 30, purge_per_minute: 600 }
    }
}

impl CdnSettings {
    /// `host[:port]` of the base URL.
    pub fn host(&self) -> Option<String> {
        let u = url::Url::parse(self.base_url.as_deref()?).ok()?;
        let host = u.host_str()?.to_string();
        Some(match u.port() {
            Some(p) => format!("{host}:{p}"),
            None => host,
        })
    }

    /// The base URL without a trailing `/`.
    pub fn base(&self) -> Option<&str> {
        self.base_url.as_deref().map(|b| b.trim_end_matches('/'))
    }

    /// Whether purges can be sent (a zone and a token).
    pub fn can_purge(&self) -> bool {
        self.zone_id.is_some() && self.api_token.is_some()
    }

    fn check(&self) -> Result<()> {
        let bad = |m: String| Err(StoreError::InvalidArgument(m));
        if let Some(b) = &self.base_url {
            match url::Url::parse(b) {
                Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() && u.username().is_empty() && u.query().is_none() && u.path() == "/" => {}
                _ => return bad(format!("base_url {b:?} must be http(s)://host[:port], without a path")),
            }
        }
        if let Some(z) = &self.zone_id
            && (z.is_empty() || z.len() > 64 || !z.bytes().all(|c| c.is_ascii_alphanumeric()))
        {
            return bad("zone_id must be Cloudflare's zone id (letters and digits)".into());
        }
        if self.routes.is_empty() || self.routes.len() > 32 {
            return bad("give 1 to 32 routes".into());
        }
        for r in &self.routes {
            if !r.prefix.starts_with('/') || r.prefix.len() > 256 {
                return bad(format!("route prefix {:?} must start with /", r.prefix));
            }
            if !matches!(r.kind.as_str(), "s3" | "immutable" | "bucket-key" | "website" | "none") {
                return bad(format!("route kind {:?} is s3, immutable, bucket-key, website or none", r.kind));
            }
            if r.api && matches!(r.kind.as_str(), "s3" | "website") {
                return bad(format!("route {:?}: the developer API's routes are bucket-key, immutable or none", r.prefix));
            }
            if !matches!(r.auth.as_deref(), None | Some("any" | "public" | "token")) {
                return bad("route auth is any, public or token".into());
            }
            if (r.kind == "website") != r.bucket.is_some() {
                return bad("website routes (and only they) name their bucket".into());
            }
        }
        if !(1..=500).contains(&self.purge_batch) {
            return bad("purge_batch is 1 to 500".into());
        }
        if self.purge_per_minute == 0 {
            return bad("purge_per_minute must be at least 1".into());
        }
        if self.mutable_ttl_s > 31_536_000 {
            return bad("mutable_ttl_s is at most a year".into());
        }
        Ok(())
    }
}

/// What the settings are stored as (the token sealed).
#[derive(Serialize, Deserialize)]
struct Stored {
    #[serde(flatten)]
    settings: CdnSettings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_token_sealed: Option<String>,
}

/// A change to the settings (`None`: unchanged; an empty `api_token`, `zone_id` or `base_url`
/// removes it).
#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CdnSettingsPatch {
    pub base_url: Option<String>,
    pub zone_id: Option<String>,
    pub api_token: Option<Secret>,
    pub purge: Option<PurgeMode>,
    pub routes: Option<Vec<CdnRoute>>,
    pub mutable_ttl_s: Option<u32>,
    pub purge_batch: Option<u32>,
    pub purge_per_minute: Option<u32>,
}

impl std::fmt::Debug for CdnSettingsPatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CdnSettingsPatch").field("base_url", &self.base_url).field("zone_id", &self.zone_id).field("routes", &self.routes).finish_non_exhaustive()
    }
}

/// The token of an immutable URL for a blob: derived from the blob id (blobs never change, so
/// neither does what the URL serves) without showing it.
pub fn immutable_token(blob_id: &str) -> String {
    let d = Sha256::digest(format!("roll-cdn-v1\0{blob_id}").as_bytes());
    hex::encode(&d[..10])
}

/// `rk-<32 hex>`: the cache tag of an object (the Worker tags every copy of it with it).
pub fn object_tag(bucket: &str, key: &str) -> String {
    format!("rk-{}", &hex::encode(Sha256::digest(format!("{bucket}/{key}").as_bytes()))[..32])
}

/// `rb-<32 hex>`: the cache tag of every object of a bucket.
pub fn bucket_tag(bucket: &str) -> String {
    format!("rb-{}", &hex::encode(Sha256::digest(bucket.as_bytes()))[..32])
}

/// Whether an event changes what the CDN may serve for its key.
pub fn purges(event: &str) -> bool {
    ["ObjectCreated:", "ObjectRemoved:", "LifecycleExpiration:", "ObjectAcl:", "ObjectRestore:Completed"].iter().any(|p| event.starts_with(p))
}

/// The outbox destination of a bucket's purges.
pub(crate) fn dest(bucket_id: &str) -> String {
    format!("cdn:{bucket_id}")
}

/// Whether a stored CDN setting has the CDN on.
pub(crate) fn is_on(value: &str) -> bool {
    serde_json::from_str::<BucketCdn>(value).is_ok_and(|b| b.enabled)
}

/// Whether the bucket has the CDN on (inside a transaction).
pub(crate) fn enabled(c: &Connection, bucket_id: &str) -> Result<bool> {
    Ok(crate::config::bucket_config_row(c, bucket_id, KIND, "")?.is_some_and(|v| is_on(&v)))
}

/// Queue a purge of the whole bucket if it has the CDN on (inside the transaction that can make
/// it more private).
pub(crate) fn purge_bucket(c: &Connection, b: &BucketRow, now: i64) -> Result<()> {
    if enabled(c, &b.id)? {
        crate::events::emit_cdn_purge(c, b, "", true, now)?;
    }
    Ok(())
}

/// Whether changing a `bucket_config` kind can make a bucket's objects more private.
pub(crate) fn guards_access(kind: &str) -> bool {
    use crate::config::kind;
    [kind::POLICY, kind::ACL, kind::PUBLIC_ACCESS_BLOCK, kind::OWNERSHIP, KIND].contains(&kind)
}

impl Store {
    /// A bucket's CDN setting (`None`: never set; off).
    pub fn bucket_cdn(&self, bucket: &str) -> Result<Option<BucketCdn>> {
        Ok(self.bucket_config(bucket, KIND, "")?.map(|v| serde_json::from_str(&v)).transpose()?)
    }

    /// Set (`Some`) or remove a bucket's CDN setting. Whatever the edge has of the bucket is
    /// purged (turning it off must stop it being served).
    pub async fn set_bucket_cdn(&self, bucket: &str, cdn: Option<&BucketCdn>) -> Result<()> {
        let v = cdn.map(serde_json::to_string).transpose()?;
        self.set_bucket_config(bucket, KIND, "", v.as_deref(), None).await
    }

    /// The deployment's CDN settings, the API token opened.
    pub fn cdn_settings(&self) -> Result<CdnSettings> {
        self.load_cdn_settings(true)
    }

    /// The deployment's CDN settings without the API token (what serving needs).
    pub fn cdn_routing(&self) -> Result<CdnSettings> {
        self.load_cdn_settings(false)
    }

    fn load_cdn_settings(&self, token: bool) -> Result<CdnSettings> {
        let Some(v) = self.read(|c| Ok(c.prepare_cached("SELECT v FROM state WHERE k = ?1")?.query_row([SETTINGS], |r| r.get::<_, String>(0)).optional()?))? else {
            return Ok(CdnSettings::default());
        };
        let stored: Stored = serde_json::from_str(&v)?;
        let mut s = stored.settings;
        if let Some(sealed) = stored.api_token_sealed.filter(|_| token) {
            s.api_token = Some(self.open_target_secret(&sealed)?);
        }
        Ok(s)
    }

    /// Change the deployment's CDN settings; returns them (without the token).
    pub async fn update_cdn_settings(&self, p: CdnSettingsPatch) -> Result<CdnSettings> {
        let mut s = self.cdn_settings()?;
        let text = |v: String| Some(v.trim().to_string()).filter(|v| !v.is_empty());
        if let Some(b) = p.base_url {
            s.base_url = text(b).map(|b| b.trim_end_matches('/').to_string() + "/");
        }
        if let Some(z) = p.zone_id {
            s.zone_id = text(z);
        }
        if let Some(t) = p.api_token {
            if t.expose().len() > 512 {
                return Err(StoreError::InvalidArgument("api_token is at most 512 characters".into()));
            }
            s.api_token = Some(t).filter(|t| !t.expose().trim().is_empty());
        }
        if let Some(m) = p.purge {
            s.purge = m;
        }
        if let Some(r) = p.routes {
            s.routes = r;
        }
        if let Some(t) = p.mutable_ttl_s {
            s.mutable_ttl_s = t;
        }
        if let Some(b) = p.purge_batch {
            s.purge_batch = b;
        }
        if let Some(n) = p.purge_per_minute {
            s.purge_per_minute = n;
        }
        s.check()?;
        let sealed = match &s.api_token {
            Some(t) => Some(self.seal_target_secret(t.expose()).await?),
            None => None,
        };
        let v = serde_json::to_string(&Stored { settings: s.clone(), api_token_sealed: sealed })?;
        self.write(|c| {
            c.execute("INSERT INTO state (k, v) VALUES (?1, ?2) ON CONFLICT (k) DO UPDATE SET v = excluded.v", params![SETTINGS, v])?;
            Ok(())
        })
        .await?;
        Ok(CdnSettings { api_token: None, ..s })
    }

    /// Count a usage batch the edge Worker reported: `(bucket name, bytes served)` per entry,
    /// for today. A batch id already counted changes nothing (`false`). Buckets that don't exist
    /// (any more) are skipped.
    pub async fn record_cdn_usage(&self, batch: &str, entries: &[(String, u64)]) -> Result<bool> {
        if batch.is_empty() || batch.len() > 128 || !batch.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
            return Err(StoreError::InvalidArgument("a batch id is 1-128 letters, digits, - and _".into()));
        }
        if entries.len() > MAX_USAGE_ENTRIES {
            return Err(StoreError::InvalidArgument(format!("at most {MAX_USAGE_ENTRIES} entries per batch")));
        }
        let now = self.now();
        let day = crate::ids::day_of(now);
        self.write(|c| {
            c.prepare_cached("DELETE FROM cdn_batches WHERE received_at < ?1")?.execute([now - BATCH_MEMORY_MS])?;
            if c.prepare_cached("INSERT INTO cdn_batches (id, received_at) VALUES (?1, ?2) ON CONFLICT (id) DO NOTHING")?.execute(params![batch, now])? == 0 {
                return Ok(false);
            }
            for (bucket, bytes) in entries {
                if *bytes == 0 {
                    continue;
                }
                let Some(id) = c.prepare_cached("SELECT id FROM buckets WHERE name = ?1")?.query_row([bucket], |r| r.get::<_, String>(0)).optional()? else { continue };
                c.prepare_cached(
                    "INSERT INTO usage (bucket_id, day, class_a, class_b, bytes_in, egress_internet, egress_internal, byte_ms, egress_cached) VALUES (?1, ?2, 0, 0, 0, 0, 0, 0, ?3)
                     ON CONFLICT (bucket_id, day) DO UPDATE SET egress_cached = egress_cached + excluded.egress_cached",
                )?
                .execute(params![id, day, (*bytes).min(i64::MAX as u64) as i64])?;
            }
            Ok(true)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_tags_and_events() {
        let t = immutable_token("0123abcd");
        assert_eq!(t.len(), 20);
        assert_eq!(t, immutable_token("0123abcd"));
        assert_ne!(t, immutable_token("0123abce"));
        // The Worker's tags: the first 32 hex digits of SHA-256 of `<bucket>/<key>` and `<bucket>`.
        assert_eq!(object_tag("b", "k"), format!("rk-{}", &hex::encode(Sha256::digest(b"b/k"))[..32]));
        assert_eq!(bucket_tag("b"), format!("rb-{}", &hex::encode(Sha256::digest(b"b"))[..32]));
        for e in ["ObjectCreated:Put", "ObjectRemoved:DeleteMarkerCreated", "LifecycleExpiration:Delete", "ObjectAcl:Put", "ObjectRestore:Completed"] {
            assert!(purges(e), "{e}");
        }
        for e in ["ObjectTagging:Put", "Replication:OperationFailedReplication", "ObjectRestore:Post", "TestEvent"] {
            assert!(!purges(e), "{e}");
        }
    }

    #[test]
    fn settings_are_checked() {
        let ok = CdnSettings { base_url: Some("https://cdn.example.com/".into()), zone_id: Some("abc123".into()), ..Default::default() };
        ok.check().unwrap();
        // The defaults front the developer API's reads, and nothing else under it.
        let api: Vec<&CdnRoute> = ok.routes.iter().filter(|r| r.api).collect();
        assert!(api.iter().any(|r| r.prefix == "/storage/v1/" && r.kind == "none"));
        assert!(api.iter().any(|r| r.prefix == "/storage/v1/render/image/sign/" && r.forward_token && r.vary_accept && r.credentialed()));
        assert!(!api.iter().any(|r| r.prefix.contains("authenticated")));
        let mut bad = ok.clone();
        bad.routes.push(CdnRoute { api: true, ..CdnRoute::new("/storage/v1/x/", "s3") });
        assert!(bad.check().is_err(), "the API's routes aren't S3's");
        assert_eq!(ok.host().as_deref(), Some("cdn.example.com"));
        assert_eq!(ok.base(), Some("https://cdn.example.com"));
        assert_eq!(CdnSettings { base_url: Some("http://127.0.0.1:8787/".into()), ..Default::default() }.host().as_deref(), Some("127.0.0.1:8787"));
        for bad in [
            CdnSettings { base_url: Some("ftp://x/".into()), ..Default::default() },
            CdnSettings { base_url: Some("https://x/path".into()), ..Default::default() },
            CdnSettings { zone_id: Some("a b".into()), ..Default::default() },
            CdnSettings { routes: vec![], ..Default::default() },
            CdnSettings { routes: vec![CdnRoute::new("x", "s3")], ..Default::default() },
            CdnSettings { routes: vec![CdnRoute::new("/", "nope")], ..Default::default() },
            CdnSettings { routes: vec![CdnRoute::new("/", "website")], ..Default::default() },
            CdnSettings { purge_batch: 0, ..Default::default() },
        ] {
            assert!(bad.check().is_err(), "{bad:?}");
        }
        let p = CdnSettingsPatch { api_token: Some(Secret::new("cf-secret-token")), ..Default::default() };
        assert!(!format!("{p:?}").contains("cf-secret-token"));
    }
}
