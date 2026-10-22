//! The storage control plane: views of buckets and objects for the API, uploads, presigning,
//! usage and its cost.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use s3_store::{AccessKey, Bucket, ByteStream, DeleteOutcome, ListQuery, ObjectHeaders, ObjectMeta, Permission, Store, StoreError, UsageDay, VersionsQuery, WriteCondition};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(thiserror::Error, Debug)]
pub enum ControlError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, ControlError>;

impl From<StoreError> for ControlError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::NoSuchBucket(_) | StoreError::NoSuchKey(_) | StoreError::NoSuchUpload(_) | StoreError::NotFound(_) | StoreError::NoSuchVersion(..) | StoreError::DeleteMarker { .. } => {
                Self::NotFound(e.to_string())
            }
            StoreError::BucketExists(_)
            | StoreError::BucketNotEmpty(_)
            | StoreError::BucketHasBranches(_)
            | StoreError::PreconditionFailed
            | StoreError::ObjectLocked(_)
            | StoreError::InvalidBucketState(_)
            | StoreError::Conflict(_) => Self::Conflict(e.to_string()),
            StoreError::InvalidRequest(_) | StoreError::Refused { .. } => Self::Invalid(e.to_string()),
            StoreError::InvalidPart(_)
            | StoreError::InvalidPartOrder
            | StoreError::EntityTooSmall(_)
            | StoreError::InvalidBucketName(_)
            | StoreError::KeyTooLong
            | StoreError::MetadataTooLarge
            | StoreError::InvalidArgument(_) => Self::Invalid(e.to_string()),
            StoreError::Unavailable(_) => Self::Unavailable(e.to_string()),
            StoreError::Internal(e) => Self::Internal(e),
        }
    }
}

/// What customers pay: S3 Standard list prices (us-east-1). See `docs/storage/pricing.md` for
/// what it costs us.
#[derive(Clone, Debug, Serialize)]
pub struct Pricing {
    pub usd_per_gb_month: f64,
    /// PUT, COPY, POST, LIST, multipart.
    pub usd_per_million_class_a: f64,
    /// GET, HEAD and the rest.
    pub usd_per_million_class_b: f64,
    pub usd_per_gb_egress_internet: f64,
    /// Inside roll (same region): free.
    pub usd_per_gb_egress_internal: f64,
}

impl Default for Pricing {
    fn default() -> Self {
        Self { usd_per_gb_month: 0.023, usd_per_million_class_a: 5.0, usd_per_million_class_b: 0.4, usd_per_gb_egress_internet: 0.09, usd_per_gb_egress_internal: 0.0 }
    }
}

#[derive(Clone, Debug)]
pub struct ControlConfig {
    /// The S3 endpoint clients use, e.g. `http://127.0.0.1:59000`.
    pub endpoint: String,
    /// The region to sign with (any is accepted).
    pub region: String,
    pub virtual_host_domain: Option<String>,
    /// `<bucket>.<domain>` is a bucket's static website (`ROLL_S3_WEBSITE_DOMAIN`).
    pub website_domain: Option<String>,
    /// Storage class of blobs (`None`: a local directory).
    pub storage_class: Option<String>,
    pub pricing: Pricing,
}

impl ControlConfig {
    pub fn local(endpoint: impl Into<String>) -> Self {
        Self { endpoint: endpoint.into(), region: "us-east-1".into(), virtual_host_domain: None, website_domain: None, storage_class: None, pricing: Pricing::default() }
    }
}

/// The control plane's own key, for presigned URLs. Never listed, never deletable through the
/// API.
pub const CONTROL_KEY_ID: &str = "RKROLLCONTROLPLANE00";
const CONTROL_KEY_NAME: &str = "roll control plane (presigned URLs)";

const DAY_MS: i64 = 86_400_000;
/// AWS bills a month as 730 hours.
const MONTH_MS: f64 = 730.0 * 3_600_000.0;
const GB: f64 = 1e9;

pub struct Control {
    pub store: Arc<Store>,
    pub cfg: ControlConfig,
    presign_secret: String,
    /// The S3 API's event notifier (validates notification destinations); set by
    /// [`Control::set_notifier`].
    notifier: std::sync::OnceLock<Arc<s3_server::Notifier>>,
}

fn random_secret() -> String {
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut bytes = [0u8; 40];
    getrandom::fill(&mut bytes).expect("os randomness");
    bytes.iter().map(|b| ALNUM[*b as usize % ALNUM.len()] as char).collect()
}

/// A listing cursor (opaque to clients).
pub fn encode_cursor(s: &str) -> String {
    URL_SAFE_NO_PAD.encode(s)
}

pub fn decode_cursor(s: &str) -> Result<String> {
    URL_SAFE_NO_PAD.decode(s).ok().and_then(|b| String::from_utf8(b).ok()).ok_or_else(|| ControlError::Invalid("invalid cursor".into()))
}

pub fn summary(o: &ObjectMeta) -> Value {
    json!({ "key": o.key, "size": o.size, "etag": o.etag, "content_type": o.headers.content_type, "last_modified": o.modified_at })
}

pub fn info(o: &ObjectMeta) -> Value {
    let h = &o.headers;
    json!({
        "key": o.key, "size": o.size, "etag": o.etag, "content_type": h.content_type, "last_modified": o.modified_at,
        "metadata": h.metadata, "tags": h.tags,
        "cache_control": h.cache_control, "content_disposition": h.content_disposition, "content_encoding": h.content_encoding,
        "content_language": h.content_language, "expires": h.expires,
        "checksum": o.checksum.as_ref().map(|c| json!({ "algorithm": c.algorithm, "type": c.kind, "value": c.value })),
        "parts": o.parts,
        "version_id": o.version_id,
        "lock": if o.lock.is_empty() { Value::Null } else { json!({ "mode": o.lock.mode, "retain_until": o.lock.retain_until, "legal_hold": o.lock.legal_hold }) },
    })
}

/// One day of usage, priced.
#[derive(Clone, Debug, Default, Serialize)]
pub struct DailyUsage {
    pub day: String,
    pub class_a: u64,
    pub class_b: u64,
    pub egress_bytes: u64,
    /// Average bytes stored over the day (so far, for today).
    pub storage_bytes: u64,
    pub usd: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Cost {
    pub storage_usd: f64,
    pub class_a_usd: f64,
    pub class_b_usd: f64,
    pub egress_usd: f64,
    pub total_usd: f64,
    /// Requests and egress at this window's daily rate for 30 days, plus what's stored now for a
    /// month.
    pub projected_month_usd: f64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Usage {
    pub bucket: String,
    pub days: i64,
    pub class_a: u64,
    pub class_b: u64,
    pub bytes_in: u64,
    pub egress_internet_bytes: u64,
    pub egress_internal_bytes: u64,
    pub gb_months: f64,
    /// Stored now (own objects plus the trash).
    pub storage_bytes: u64,
    pub objects: u64,
    pub daily: Vec<DailyUsage>,
    pub cost: Cost,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageAll {
    pub days: i64,
    pub cost: Cost,
    pub daily: Vec<DailyUsage>,
    pub buckets: Vec<Usage>,
}

fn round(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

impl Control {
    /// The control plane over `store`. Creates its presigning key on first use.
    pub async fn new(store: Arc<Store>, cfg: ControlConfig) -> anyhow::Result<Arc<Self>> {
        let secret = match store.access_key(CONTROL_KEY_ID)? {
            Some(k) => k.secret,
            None => store.ensure_key(CONTROL_KEY_ID, &random_secret(), CONTROL_KEY_NAME).await?.secret,
        };
        Ok(Arc::new(Self { store, cfg, presign_secret: secret, notifier: Default::default() }))
    }

    /// Connect the S3 API's notifier, so `PATCH` can set notifications (their destinations are
    /// validated, and sent a test event, as through the S3 API).
    pub fn set_notifier(&self, n: Arc<s3_server::Notifier>) {
        let _ = self.notifier.set(n);
    }

    /// Replace a bucket's notifications (`configurations` empty and no EventBridge: none).
    pub async fn set_notifications(&self, bucket: &str, cfg: s3_store::NotificationConfig, skip_validation: bool) -> Result<()> {
        let b = self.store.bucket(bucket)?;
        if cfg.is_empty() {
            self.store.set_notifications(bucket, None).await?;
            return Ok(());
        }
        let n = self.notifier.get().ok_or_else(|| ControlError::Unavailable("notifications need the S3 API, which isn't running".into()))?;
        n.put_config(&b, cfg, skip_validation).await.map_err(|e| match e {
            s3_server::NotifyError::Invalid(m) => ControlError::Invalid(m),
            s3_server::NotifyError::Destinations(f) => {
                ControlError::Invalid(format!("Unable to validate the following destination configurations: {}", f.iter().map(|(a, w)| format!("{a}: {w}")).collect::<Vec<_>>().join("; ")))
            }
            s3_server::NotifyError::Store(e) => e.into(),
        })?;
        Ok(())
    }

    /// The `notifications` of a bucket view (its `backlog` comes with the other settings), and
    /// whether there are any.
    fn notifications_view(&self, b: &Bucket) -> (Value, bool) {
        let cfg = self.store.notifications(&b.name).ok().flatten().unwrap_or_default();
        let secret = self.store.bucket_config(&b.name, s3_store::config::kind::WEBHOOK_SECRET, "").ok().flatten();
        (json!({ "configurations": cfg.configurations, "event_bridge": cfg.event_bridge, "webhook_secret": secret }), !cfg.is_empty())
    }

    pub fn info(&self) -> Result<Value> {
        let st = self.store.stats()?;
        Ok(json!({
            "endpoint": self.cfg.endpoint,
            "region": self.cfg.region,
            "virtual_host_domain": self.cfg.virtual_host_domain,
            "website_domain": self.cfg.website_domain,
            "storage": self.store.describe(),
            "storage_class": self.cfg.storage_class.as_deref().unwrap_or("LOCAL"),
            "recover_days_default": self.store.cfg.recover_days_default,
            "owner_id": self.store.owner_id(),
            "account_id": self.store.account_id(),
            "block_public_access": self.store.cfg.block_public_access,
            "pricing": self.cfg.pricing,
            "buckets": st.buckets,
            "objects": st.objects,
            "bytes": st.bytes,
            "scrub": { "last_at": st.scrub.last_at, "missing": st.scrub.missing, "mismatched": st.scrub.mismatched },
        }))
    }

    /// `<bucket>.<domain>` with the endpoint's scheme and port.
    fn virtual_host_url(&self, bucket: &str) -> Option<String> {
        let domain = self.cfg.virtual_host_domain.as_deref()?;
        let u = url::Url::parse(&self.cfg.endpoint).ok()?;
        let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
        Some(format!("{}://{bucket}.{domain}{port}", u.scheme()))
    }

    pub fn bucket_view(&self, b: &Bucket) -> Value {
        let mut v = serde_json::to_value(b).unwrap_or_default();
        let endpoint = self.cfg.endpoint.trim_end_matches('/');
        v["tags"] = json!(self.store.bucket_tags(&b.name).unwrap_or_default());
        let website = self.store.bucket_website(&b.name).ok().flatten();
        let has_website = website.is_some();
        v["website"] = json!(website);
        let (notifications, has_notifications) = self.notifications_view(b);
        v["notifications"] = notifications;
        self.settings_view(b, &mut v, has_notifications, has_website);
        v["endpoint"] = json!(endpoint);
        v["url"] = json!(format!("{endpoint}/{}", b.name));
        v["virtual_host_url"] = json!(self.virtual_host_url(&b.name));
        v
    }

    pub fn list_objects(&self, bucket: &str, prefix: &str, delimiter: Option<&str>, cursor: Option<&str>, limit: usize) -> Result<Value> {
        let start_after = cursor.map(decode_cursor).transpose()?;
        let q = ListQuery { prefix: prefix.to_string(), delimiter: delimiter.filter(|d| !d.is_empty()).map(str::to_string), start_after, max_keys: limit.clamp(1, 1000) };
        let page = self.store.list_objects(bucket, &q)?;
        let next = if page.truncated { page.next.as_deref().map(encode_cursor) } else { None };
        Ok(json!({ "objects": page.objects.iter().map(summary).collect::<Vec<_>>(), "prefixes": page.prefixes, "next_cursor": next }))
    }

    /// Delete by keys and/or everything under a prefix (`""` empties the bucket).
    pub async fn delete_many(&self, bucket: &str, keys: Option<Vec<String>>, prefix: Option<String>) -> Result<u64> {
        if keys.is_none() && prefix.is_none() {
            return Err(ControlError::Invalid("give keys or a prefix".into()));
        }
        self.store.bucket(bucket)?;
        let mut deleted = 0;
        if let Some(keys) = keys {
            if keys.len() > 1000 {
                return Err(ControlError::Invalid("at most 1000 keys at a time".into()));
            }
            for (key, r) in self.store.delete_objects(bucket, &keys).await? {
                match r {
                    Ok(true) => deleted += 1,
                    Ok(false) | Err(StoreError::NoSuchKey(_)) => {}
                    Err(e) => {
                        tracing::warn!(bucket, key, error = %e, "delete failed");
                        return Err(e.into());
                    }
                }
            }
        }
        if let Some(p) = prefix {
            deleted += self.store.delete_prefix(bucket, &p).await?;
        }
        Ok(deleted)
    }

    /// Delete an object (to the trash, or behind a delete marker in a versioned bucket), or with
    /// `version_id` that version for good (to the trash; never one under Object Lock).
    pub async fn delete_object(&self, bucket: &str, key: &str, version_id: Option<&str>) -> Result<DeleteOutcome> {
        let missing = || ControlError::NotFound(format!("no object {key} in {bucket}"));
        match version_id {
            Some(v) => {
                if !s3_proto::versioning::valid_version_id(v) {
                    return Err(ControlError::Invalid(format!("invalid version id {v}")));
                }
                let o = self.store.delete_object_version(bucket, key, Some(v), None, false).await?;
                if !o.existed {
                    return Err(ControlError::NotFound(format!("no version {v} of {key} in {bucket}")));
                }
                Ok(o)
            }
            None => {
                // Never leave a delete marker for a key that wasn't there.
                match self.store.get_object(bucket, key) {
                    Err(StoreError::NoSuchKey(_)) => return Err(missing()),
                    r => r?,
                };
                let o = self.store.delete_object_version(bucket, key, None, None, false).await?;
                if !o.existed {
                    return Err(missing());
                }
                Ok(o)
            }
        }
    }

    /// A page of the bucket's versions (newest first within a key). The cursor is the last
    /// `(key, version_id)` of the previous page.
    pub fn list_versions(&self, bucket: &str, prefix: &str, cursor: Option<&str>, limit: usize) -> Result<Value> {
        let (key_marker, version_id_marker) = match cursor {
            Some(c) => {
                let (k, v): (String, String) = serde_json::from_str(&decode_cursor(c)?).map_err(|_| ControlError::Invalid("invalid cursor".into()))?;
                (Some(k), Some(v))
            }
            None => (None, None),
        };
        let q = VersionsQuery { prefix: prefix.to_string(), delimiter: None, key_marker, version_id_marker, max_keys: limit.clamp(1, 1000) };
        let page = self.store.list_versions(bucket, &q)?;
        let next = match (page.truncated, &page.next_key_marker, &page.next_version_id_marker) {
            (true, Some(k), Some(v)) => Some(encode_cursor(&serde_json::to_string(&(k, v)).unwrap_or_default())),
            _ => None,
        };
        Ok(json!({ "versions": page.versions, "next_cursor": next }))
    }

    /// An upload from the dashboard or the CLI: the same path as PutObject.
    pub async fn upload(&self, bucket: &str, key: &str, headers: ObjectHeaders, body: ByteStream, len: u64) -> Result<ObjectMeta> {
        let b = self.store.bucket(bucket)?;
        self.store.meter.request(&b.id, s3_store::Class::A);
        let meta = s3_server::put_object(&self.store, &b, key, headers, body, len, &WriteCondition::none()).await?;
        self.store.meter.bytes_in(&b.id, len);
        Ok(meta)
    }

    pub fn presign(&self, bucket: &str, key: &str, method: &str, expires_s: i64) -> Result<(String, i64)> {
        let method = method.to_ascii_uppercase();
        if method != "GET" && method != "PUT" {
            return Err(ControlError::Invalid("method must be GET or PUT".into()));
        }
        if !(1..=604_800).contains(&expires_s) {
            return Err(ControlError::Invalid("expires_s must be between 1 and 604800 (7 days)".into()));
        }
        s3_store::validate::key(key)?;
        self.store.bucket(bucket)?;
        let now_ms = self.store.now();
        let path = format!("/{bucket}/{key}");
        let url = s3_proto::sigv4::presign_url(&s3_proto::sigv4::Presign {
            method: &method,
            endpoint: self.cfg.endpoint.trim_end_matches('/'),
            path: &path,
            query: &[],
            access_key: CONTROL_KEY_ID,
            secret: &self.presign_secret,
            region: &self.cfg.region,
            now: now_ms / 1000,
            expires_s,
        });
        Ok((url, now_ms + expires_s * 1000))
    }

    pub fn list_trash(&self, bucket: &str, prefix: &str, cursor: Option<&str>, limit: usize) -> Result<Value> {
        let after = match cursor {
            Some(c) => {
                let (k, at): (String, i64) = serde_json::from_str(&decode_cursor(c)?).map_err(|_| ControlError::Invalid("invalid cursor".into()))?;
                Some((k, at))
            }
            None => None,
        };
        let (entries, more) = self.store.list_trash(bucket, prefix, after.as_ref().map(|(k, at)| (k.as_str(), *at)), limit.clamp(1, 1000))?;
        let next = match (more, entries.last()) {
            (true, Some(e)) => Some(encode_cursor(&serde_json::to_string(&(&e.key, e.deleted_at)).unwrap_or_default())),
            _ => None,
        };
        Ok(json!({ "objects": entries, "next_cursor": next }))
    }

    /// Keys as the API shows them (the control plane's own key is hidden).
    pub fn keys(&self) -> Result<Vec<AccessKey>> {
        Ok(self.store.keys()?.into_iter().filter(|k| k.id != CONTROL_KEY_ID).collect())
    }

    pub async fn create_key(&self, name: &str, permission: &str, buckets: Option<Vec<String>>) -> Result<AccessKey> {
        let p = Permission::parse(permission).ok_or_else(|| ControlError::Invalid("permission must be read, write or admin".into()))?;
        if let Some(bs) = &buckets {
            if bs.is_empty() {
                return Err(ControlError::Invalid("buckets: give at least one, or leave it out for all".into()));
            }
            for b in bs {
                s3_store::validate::bucket_name(b)?;
            }
        }
        Ok(self.store.create_key(name, p, buckets).await?)
    }

    pub async fn delete_key(&self, id: &str) -> Result<()> {
        if id == CONTROL_KEY_ID {
            return Err(ControlError::NotFound(format!("no access key {id}")));
        }
        Ok(self.store.delete_key(id).await?)
    }

    fn since_day(&self, days: i64) -> String {
        s3_store::day_of(self.store.now() - (days - 1) * DAY_MS)
    }

    /// Price one bucket's (or several buckets') metered days.
    fn price(&self, rows: &[&UsageDay], days: i64, storage_bytes: u64) -> (Vec<DailyUsage>, Cost, [u64; 5], f64) {
        let p = &self.cfg.pricing;
        let now = self.store.now();
        let today = s3_store::day_of(now);
        let today_ms = (now.rem_euclid(DAY_MS)).max(1) as f64;
        let mut by_day: BTreeMap<&str, DailyUsage> = BTreeMap::new();
        let mut byte_ms_day: BTreeMap<&str, f64> = BTreeMap::new();
        let (mut a, mut b, mut bin, mut ei, mut en, mut byte_ms) = (0u64, 0u64, 0u64, 0u64, 0u64, 0f64);
        for r in rows {
            a += r.class_a;
            b += r.class_b;
            bin += r.bytes_in;
            ei += r.egress_internet;
            en += r.egress_internal;
            byte_ms += r.byte_ms;
            let d = by_day.entry(r.day.as_str()).or_insert_with(|| DailyUsage { day: r.day.clone(), ..Default::default() });
            d.class_a += r.class_a;
            d.class_b += r.class_b;
            d.egress_bytes += r.egress_internet + r.egress_internal;
            d.usd += r.class_a as f64 / 1e6 * p.usd_per_million_class_a
                + r.class_b as f64 / 1e6 * p.usd_per_million_class_b
                + r.egress_internet as f64 / GB * p.usd_per_gb_egress_internet
                + r.egress_internal as f64 / GB * p.usd_per_gb_egress_internal
                + r.byte_ms / (GB * MONTH_MS) * p.usd_per_gb_month;
            *byte_ms_day.entry(r.day.as_str()).or_default() += r.byte_ms;
        }
        let daily = by_day
            .into_values()
            .map(|mut d| {
                let span = if d.day == today { today_ms } else { DAY_MS as f64 };
                d.storage_bytes = (byte_ms_day.get(d.day.as_str()).copied().unwrap_or(0.0) / span) as u64;
                d.usd = round(d.usd);
                d
            })
            .collect();
        let gb_months = byte_ms / (GB * MONTH_MS);
        let storage_usd = gb_months * p.usd_per_gb_month;
        let class_a_usd = a as f64 / 1e6 * p.usd_per_million_class_a;
        let class_b_usd = b as f64 / 1e6 * p.usd_per_million_class_b;
        let egress_usd = ei as f64 / GB * p.usd_per_gb_egress_internet + en as f64 / GB * p.usd_per_gb_egress_internal;
        let traffic = class_a_usd + class_b_usd + egress_usd;
        let projected = traffic * 30.0 / days as f64 + storage_bytes as f64 / GB * p.usd_per_gb_month;
        let cost = Cost {
            storage_usd: round(storage_usd),
            class_a_usd: round(class_a_usd),
            class_b_usd: round(class_b_usd),
            egress_usd: round(egress_usd),
            total_usd: round(storage_usd + traffic),
            projected_month_usd: round(projected),
        };
        (daily, cost, [a, b, bin, ei, en], gb_months)
    }

    fn bucket_usage(&self, b: &Bucket, rows: &[&UsageDay], days: i64) -> Usage {
        let storage_bytes = b.bytes + b.trash_bytes + b.version_bytes;
        let (daily, cost, [a, bb, bin, ei, en], gb_months) = self.price(rows, days, storage_bytes);
        Usage {
            bucket: b.name.clone(),
            days,
            class_a: a,
            class_b: bb,
            bytes_in: bin,
            egress_internet_bytes: ei,
            egress_internal_bytes: en,
            gb_months,
            storage_bytes,
            objects: b.objects,
            daily,
            cost,
        }
    }

    pub fn usage(&self, bucket: &str, days: i64) -> Result<Usage> {
        let days = days.clamp(1, 366);
        let b = self.store.bucket(bucket)?;
        let rows = self.store.usage(Some(&b.id), &self.since_day(days))?;
        Ok(self.bucket_usage(&b, &rows.iter().collect::<Vec<_>>(), days))
    }

    /// Every bucket, and the total (which includes buckets deleted within the window).
    pub fn usage_all(&self, days: i64) -> Result<UsageAll> {
        let days = days.clamp(1, 366);
        let rows = self.store.usage(None, &self.since_day(days))?;
        let buckets = self.store.buckets()?;
        let per = buckets.iter().map(|b| self.bucket_usage(b, &rows.iter().filter(|r| r.bucket_id == b.id).collect::<Vec<_>>(), days)).collect();
        let stored: u64 = buckets.iter().map(|b| b.bytes + b.trash_bytes + b.version_bytes).sum();
        let (daily, cost, _, _) = self.price(&rows.iter().collect::<Vec<_>>(), days, stored);
        Ok(UsageAll { days, cost, daily, buckets: per })
    }
}
