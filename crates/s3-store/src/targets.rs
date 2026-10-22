//! Replication targets: S3 buckets outside roll (another AWS account or region, or any
//! S3-compatible endpoint, another roll deployment included) that replication rules can name.
//!
//! A target is an endpoint (none: AWS S3 itself), a region, credentials and the destination
//! buckets it serves. A replication rule reaches it by its `Destination` bucket ARN
//! (`arn:aws:s3:::<bucket>`, when the target lists the bucket) or by naming it explicitly
//! (`s3-server`'s `<RollReplicationTarget>` extension, the control plane's `target`).
//!
//! The secret access key is never stored in the clear: it's sealed with AES-256-GCM (the
//! session tokens' scheme, `sts.rs`) under a dedicated 32-byte key kept in `state`
//! (`replication_target_key`, created once and never rotated away, so a sealed secret stays
//! readable for as long as the metadata exists). A target without an access key uses the
//! deployment's own AWS credentials (the default chain, as the backing bucket does).

use crate::error::{Result, StoreError};
use crate::rows;
use crate::store::Store;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use zeroize::Zeroize;

const KEY_STATE: &str = "replication_target_key";
const KEY_ID: u32 = 1;

/// A secret access key in memory: wiped on drop, never printed.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(REDACTED)")
    }
}

/// A replication target as stored (its secret only when asked for).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ReplicationTarget {
    /// 1-63 lowercase letters, digits and hyphens.
    pub name: String,
    /// `https://s3.example.com`; `None`: AWS S3.
    pub endpoint: Option<String>,
    pub region: String,
    /// `None`: the deployment's own AWS credentials.
    pub access_key_id: Option<String>,
    #[serde(skip)]
    pub secret: Option<Secret>,
    /// Path-style requests (most S3-compatible services want them).
    pub path_style: bool,
    /// Destination buckets of this target that replication rules may name by bucket ARN.
    pub buckets: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// What a target is made of (create, or replace on update).
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TargetSpec {
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub region: Option<String>,
    #[serde(default)]
    pub access_key_id: Option<String>,
    /// Required with `access_key_id` on create; on update, omitted keeps the stored one.
    #[serde(default)]
    pub secret_access_key: Option<String>,
    #[serde(default)]
    pub path_style: Option<bool>,
    #[serde(default)]
    pub buckets: Vec<String>,
}

impl std::fmt::Debug for TargetSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TargetSpec").field("endpoint", &self.endpoint).field("region", &self.region).field("access_key_id", &self.access_key_id).field("buckets", &self.buckets).finish_non_exhaustive()
    }
}

impl Drop for TargetSpec {
    fn drop(&mut self) {
        if let Some(s) = &mut self.secret_access_key {
            s.zeroize();
        }
    }
}

fn invalid(m: impl Into<String>) -> StoreError {
    StoreError::InvalidArgument(m.into())
}

/// 1-63 lowercase letters, digits and hyphens, not starting or ending with a hyphen.
pub fn validate_target_name(name: &str) -> std::result::Result<(), String> {
    let ok = (1..=63).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') && !name.starts_with('-') && !name.ends_with('-');
    if ok { Ok(()) } else { Err(format!("target names are 1-63 lowercase letters, digits and hyphens (got {name:?})")) }
}

const COLS: &str = "name, endpoint, region, access_key_id, secret, path_style, buckets, created_at, updated_at";

/// A row, with the sealed secret.
fn target_row(r: &rusqlite::Row) -> rusqlite::Result<(ReplicationTarget, Option<String>)> {
    let buckets: String = r.get(6)?;
    Ok((
        ReplicationTarget {
            name: r.get(0)?,
            endpoint: r.get(1)?,
            region: r.get(2)?,
            access_key_id: r.get(3)?,
            secret: None,
            path_style: r.get::<_, i64>(5)? != 0,
            buckets: serde_json::from_str(&buckets).unwrap_or_default(),
            created_at: r.get(7)?,
            updated_at: r.get(8)?,
        },
        r.get(4)?,
    ))
}

fn find(c: &Connection, name: &str) -> Result<Option<(ReplicationTarget, Option<String>)>> {
    Ok(c.prepare_cached(&format!("SELECT {COLS} FROM replication_targets WHERE name = ?1"))?.query_row([name], target_row).optional()?)
}

/// Whether `name` is a replication target (inside a transaction).
pub(crate) fn exists(c: &Connection, name: &str) -> Result<bool> {
    Ok(c.prepare_cached("SELECT 1 FROM replication_targets WHERE name = ?1")?.exists([name])?)
}

fn read_key(c: &Connection) -> Result<Option<[u8; 32]>> {
    let v: Option<String> = c.prepare_cached("SELECT v FROM state WHERE k = ?1")?.query_row([KEY_STATE], |r| r.get(0)).optional()?;
    let Some(mut v) = v else { return Ok(None) };
    let mut out = [0u8; 32];
    let ok = hex::decode_to_slice(&v, &mut out).is_ok();
    v.zeroize();
    if !ok {
        return Err(StoreError::internal("the replication target key doesn't parse"));
    }
    Ok(Some(out))
}

impl TargetSpec {
    /// Checked and normalized: `(endpoint, region, access key, path style, buckets)`.
    fn check(&self) -> Result<(Option<String>, String, Option<String>, bool, Vec<String>)> {
        let endpoint = self.endpoint.as_deref().map(str::trim).filter(|e| !e.is_empty()).map(|e| e.trim_end_matches('/').to_string());
        if let Some(e) = &endpoint {
            let u = url::Url::parse(e).map_err(|_| invalid(format!("endpoint {e:?} isn't a URL")))?;
            if !matches!(u.scheme(), "http" | "https") || u.host_str().is_none() || !u.username().is_empty() || u.query().is_some() {
                return Err(invalid("endpoint must be http(s)://host[:port], without credentials"));
            }
        }
        let region = self.region.clone().filter(|r| !r.is_empty()).unwrap_or_else(|| "us-east-1".into());
        if region.len() > 64 || !region.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err(invalid(format!("invalid region {region:?}")));
        }
        let key = self.access_key_id.clone().filter(|k| !k.is_empty());
        if key.as_ref().is_some_and(|k| k.len() > 128 || !k.bytes().all(|b| b.is_ascii_graphic())) {
            return Err(invalid("invalid access_key_id"));
        }
        if self.secret_access_key.as_ref().is_some_and(|s| s.is_empty() || s.len() > 256) {
            return Err(invalid("secret_access_key must be 1-256 characters"));
        }
        if self.buckets.len() > 100 {
            return Err(invalid("a target serves at most 100 buckets"));
        }
        let mut buckets = Vec::new();
        for b in &self.buckets {
            crate::validate::bucket_name(b)?;
            if !buckets.contains(b) {
                buckets.push(b.clone());
            }
        }
        Ok((endpoint, region, key, self.path_style.unwrap_or(true), buckets))
    }
}

impl Store {
    async fn ensure_target_key(&self) -> Result<[u8; 32]> {
        if let Some(k) = self.read(read_key)? {
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
        self.read(read_key)?.ok_or_else(|| StoreError::internal("the replication target key vanished"))
    }

    fn open_secret(&self, sealed: &str) -> Result<Secret> {
        let key = self.read(read_key)?.ok_or_else(|| StoreError::internal("a sealed target secret without the target key"))?;
        let mut plain = crate::sts::open(&[(KEY_ID, key)], sealed).ok_or_else(|| StoreError::internal("a replication target's secret doesn't open"))?;
        let s = String::from_utf8(plain.clone()).map_err(|_| StoreError::internal("a replication target's secret isn't text"));
        plain.zeroize();
        Ok(Secret(s?))
    }

    /// Every replication target (without secrets), by name.
    pub fn replication_targets(&self) -> Result<Vec<ReplicationTarget>> {
        self.read(|c| Ok(c.prepare(&format!("SELECT {COLS} FROM replication_targets ORDER BY name"))?.query_map([], target_row)?.map(|r| r.map(|(t, _)| t)).collect::<rusqlite::Result<Vec<_>>>()?))
    }

    /// A target with its secret opened (for the replication that uses it).
    pub fn replication_target(&self, name: &str) -> Result<Option<ReplicationTarget>> {
        let Some((mut t, sealed)) = self.read(|c| find(c, name))? else { return Ok(None) };
        if let Some(s) = sealed {
            t.secret = Some(self.open_secret(&s)?);
        }
        Ok(Some(t))
    }

    /// The target that lists `bucket` among its buckets, if any.
    pub fn target_for_bucket(&self, bucket: &str) -> Result<Option<String>> {
        Ok(self.replication_targets()?.into_iter().find(|t| t.buckets.iter().any(|b| b == bucket)).map(|t| t.name))
    }

    /// Create (`create`) or replace a target. On update, a spec without a secret keeps the
    /// stored one (as long as the access key stays the same).
    pub async fn put_replication_target(&self, name: &str, spec: &TargetSpec, create: bool) -> Result<ReplicationTarget> {
        validate_target_name(name).map_err(invalid)?;
        let (endpoint, region, key, path_style, buckets) = spec.check()?;
        let master = self.ensure_target_key().await?;
        let sealed = spec.secret_access_key.as_ref().map(|s| crate::sts::seal(KEY_ID, &master, s.as_bytes()));
        let now = self.now();
        let buckets_json = rows::json(&buckets)?;
        self.write(|c| {
            let old = find(c, name)?;
            match (&old, create) {
                (Some(_), true) => return Err(StoreError::Conflict(format!("a replication target named {name} already exists"))),
                (None, false) => return Err(StoreError::NotFound(format!("no replication target {name}"))),
                _ => {}
            }
            let secret = match (&key, &sealed, &old) {
                (None, _, _) => None,
                (Some(_), Some(s), _) => Some(s.clone()),
                (Some(k), None, Some((o, s))) if o.access_key_id.as_ref() == Some(k) && s.is_some() => s.clone(),
                (Some(_), None, _) => return Err(invalid("secret_access_key is required with access_key_id")),
            };
            let created = old.as_ref().map_or(now, |(o, _)| o.created_at);
            c.execute(
                &format!("INSERT INTO replication_targets ({COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                          ON CONFLICT (name) DO UPDATE SET endpoint = excluded.endpoint, region = excluded.region, access_key_id = excluded.access_key_id,
                            secret = excluded.secret, path_style = excluded.path_style, buckets = excluded.buckets, updated_at = excluded.updated_at"),
                params![name, endpoint, region, key, secret, path_style as i64, buckets_json, created, now],
            )?;
            Ok(())
        })
        .await?;
        let mut t = self.replication_target(name)?.ok_or_else(|| StoreError::internal("target vanished"))?;
        t.secret = None;
        Ok(t)
    }

    /// Delete a target. One that a bucket's replication rule still names can't go
    /// (`Conflict`): its replication would fail.
    pub async fn delete_replication_target(&self, name: &str) -> Result<()> {
        self.write(|c| {
            if find(c, name)?.is_none() {
                return Err(StoreError::NotFound(format!("no replication target {name}")));
            }
            let cfgs = c.prepare("SELECT b.name, bc.value FROM bucket_config bc JOIN buckets b ON b.id = bc.bucket_id WHERE bc.kind = 'replication' AND bc.id = ''")?.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
            for (bucket, v) in cfgs {
                let cfg: crate::events::ReplicationConfig = serde_json::from_str(&v)?;
                if cfg.rules.iter().any(|r| r.target.as_deref() == Some(name)) {
                    return Err(StoreError::Conflict(format!("bucket {bucket}'s replication uses the target {name}; change it first")));
                }
            }
            c.execute("DELETE FROM replication_targets WHERE name = ?1", [name])?;
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_specs() {
        assert!(validate_target_name("aws-backup").is_ok());
        assert!(validate_target_name("-x").is_err());
        assert!(validate_target_name("A").is_err());
        assert!(validate_target_name("").is_err());
        let spec = TargetSpec { endpoint: Some("http://127.0.0.1:9000/".into()), buckets: vec!["a".into(), "a".into()], ..Default::default() };
        let (e, r, k, p, b) = spec.check().unwrap();
        assert_eq!((e.as_deref(), r.as_str(), k, p, b), (Some("http://127.0.0.1:9000"), "us-east-1", None, true, vec!["a".to_string()]));
        assert!(TargetSpec { endpoint: Some("ftp://x".into()), ..Default::default() }.check().is_err());
        assert!(TargetSpec { endpoint: Some("https://u:p@x".into()), ..Default::default() }.check().is_err());
        assert!(TargetSpec { buckets: vec!["Bad_Name".into()], ..Default::default() }.check().is_err());
        let s = TargetSpec { secret_access_key: Some("supersecret".into()), ..Default::default() };
        assert!(!format!("{s:?}").contains("supersecret"));
        assert_eq!(format!("{:?}", Secret::new("supersecret")), "Secret(REDACTED)");
    }
}
