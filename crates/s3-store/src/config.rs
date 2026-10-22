//! Bucket configuration: one `bucket_config` row per setting (`kind`, `id`), written durably
//! through the metadata log like everything else, plus the deployment's canonical owner id and
//! per-object ACLs.
//!
//! Most values are opaque to the store (`s3-server` owns their format). The ones the control
//! plane shows or the store enforces are typed here: bucket tags, the website configuration and
//! the public access block.
//!
//! One-per-bucket settings use `id = ""`; analytics, metrics, inventory and intelligent-tiering
//! configurations are keyed by their id. Branches copy their parent's configuration when they're
//! made (all but the policy), and a deleted bucket's configuration goes with it.

use crate::error::{Result, StoreError};
use crate::rows;
use crate::store::{Store, find_bucket};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

/// Kinds of `bucket_config` rows.
pub mod kind {
    pub const TAGGING: &str = "tagging";
    pub const POLICY: &str = "policy";
    pub const PUBLIC_ACCESS_BLOCK: &str = "public_access_block";
    pub const OWNERSHIP: &str = "ownership";
    pub const ACL: &str = "acl";
    pub const ENCRYPTION: &str = "encryption";
    pub const WEBSITE: &str = "website";
    pub const LOGGING: &str = "logging";
    pub const ACCELERATE: &str = "accelerate";
    pub const ANALYTICS: &str = "analytics";
    pub const METRICS: &str = "metrics";
    pub const INVENTORY: &str = "inventory";
    pub const INTELLIGENT_TIERING: &str = "intelligent_tiering";
}

/// S3's Block Public Access settings for a bucket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PublicAccessBlock {
    pub block_public_acls: bool,
    pub ignore_public_acls: bool,
    pub block_public_policy: bool,
    pub restrict_public_buckets: bool,
}

impl PublicAccessBlock {
    pub const ALL: Self = Self { block_public_acls: true, ignore_public_acls: true, block_public_policy: true, restrict_public_buckets: true };

    /// Both blocks together (a deployment-wide block applies on top of a bucket's own).
    pub fn or(self, o: Self) -> Self {
        Self {
            block_public_acls: self.block_public_acls || o.block_public_acls,
            ignore_public_acls: self.ignore_public_acls || o.ignore_public_acls,
            block_public_policy: self.block_public_policy || o.block_public_policy,
            restrict_public_buckets: self.restrict_public_buckets || o.restrict_public_buckets,
        }
    }
}

/// A bucket's static website configuration (S3's `WebsiteConfiguration`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebsiteConfig {
    /// `IndexDocument.Suffix`, e.g. `index.html`.
    pub index_document: Option<String>,
    /// `ErrorDocument.Key`.
    pub error_document: Option<String>,
    /// `RedirectAllRequestsTo`: every request goes elsewhere.
    pub redirect_all: Option<RedirectAll>,
    pub routing_rules: Vec<RoutingRule>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RedirectAll {
    pub host_name: String,
    /// `http` or `https`; `None` keeps the request's.
    pub protocol: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingRule {
    pub condition: Option<RoutingCondition>,
    pub redirect: Redirect,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RoutingCondition {
    pub key_prefix_equals: Option<String>,
    pub http_error_code_returned_equals: Option<u16>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Redirect {
    pub host_name: Option<String>,
    pub protocol: Option<String>,
    pub http_redirect_code: Option<u16>,
    pub replace_key_prefix_with: Option<String>,
    pub replace_key_with: Option<String>,
}

/// The `state` row holding the deployment's canonical owner id.
const OWNER_STATE: &str = "canonical_owner";

fn sha256_hex(s: &str) -> String {
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// The canonical user id of a roll access key: 64 hex chars, stable for the deployment.
pub fn canonical_user(owner: &str, key_id: &str) -> String {
    sha256_hex(&format!("roll canonical user\0{owner}\0{key_id}"))
}

/// The 12-digit account id of a deployment, for IAM-style ARNs (`arn:aws:iam::<account>:…`).
pub fn account_id(owner: &str) -> String {
    let n = u64::from_str_radix(owner.get(..15).unwrap_or("0"), 16).unwrap_or(0);
    format!("{:012}", n % 1_000_000_000_000)
}

pub(crate) fn bucket_config_row(c: &Connection, bucket_id: &str, kind: &str, id: &str) -> Result<Option<String>> {
    Ok(c.prepare_cached("SELECT value FROM bucket_config WHERE bucket_id = ?1 AND kind = ?2 AND id = ?3")?.query_row(params![bucket_id, kind, id], |r| r.get(0)).optional()?)
}

/// The deployment-wide block on top of the bucket's own.
pub(crate) fn effective_block(c: &Connection, bucket_id: &str, deployment: bool) -> Result<PublicAccessBlock> {
    let own = match bucket_config_row(c, bucket_id, kind::PUBLIC_ACCESS_BLOCK, "")? {
        Some(v) => serde_json::from_str(&v)?,
        None => PublicAccessBlock::default(),
    };
    Ok(if deployment { own.or(PublicAccessBlock::ALL) } else { own })
}

/// Making a bucket `public` counts as a public bucket policy (it's one, for `s3:GetObject`), so
/// `BlockPublicPolicy` refuses it.
pub(crate) fn check_may_be_public(c: &Connection, bucket_id: &str, name: &str, deployment: bool) -> Result<()> {
    if effective_block(c, bucket_id, deployment)?.block_public_policy {
        return Err(StoreError::InvalidArgument(format!("bucket {name} can't be made public: public access is blocked (BlockPublicPolicy)")));
    }
    Ok(())
}

/// A deleted bucket's configuration goes with it.
pub(crate) fn drop_bucket_config(c: &Connection, bucket_id: &str) -> Result<()> {
    c.execute("DELETE FROM bucket_config WHERE bucket_id = ?1", [bucket_id])?;
    Ok(())
}

/// A new branch starts with its parent's configuration, except the policy: its resources name
/// the parent bucket.
pub(crate) fn copy_bucket_config(c: &Connection, from: &str, to: &str) -> Result<()> {
    c.execute(
        "INSERT INTO bucket_config (bucket_id, kind, id, value) SELECT ?2, kind, id, value FROM bucket_config WHERE bucket_id = ?1 AND kind <> ?3",
        params![from, to, kind::POLICY],
    )?;
    Ok(())
}

impl Store {
    /// Create the deployment's canonical owner id on first start (random, then kept in the
    /// metadata for good, so it survives restarts, recovery and moving hosts).
    pub(crate) async fn ensure_owner(&self) -> Result<()> {
        if self.read(|c| Ok(c.query_row("SELECT 1 FROM state WHERE k = ?1", [OWNER_STATE], |_| Ok(())).optional()?.is_some()))? {
            return Ok(());
        }
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| StoreError::internal(format!("randomness: {e}")))?;
        let id = hex::encode(seed);
        self.write(|c| {
            c.execute("INSERT INTO state (k, v) VALUES (?1, ?2) ON CONFLICT (k) DO NOTHING", params![OWNER_STATE, id])?;
            Ok(())
        })
        .await
    }

    /// The deployment's canonical owner id (64 hex chars): the owner of every bucket and object,
    /// as ListBuckets, ACLs and listings show it.
    pub fn owner_id(&self) -> String {
        self.read(|c| Ok(c.query_row("SELECT v FROM state WHERE k = ?1", [OWNER_STATE], |r| r.get::<_, String>(0)).optional()?))
            .ok()
            .flatten()
            .unwrap_or_else(|| sha256_hex(&format!("roll owner\0{}", self.cfg.region)))
    }

    /// The canonical user id of an access key (see [`canonical_user`]).
    pub fn canonical_user(&self, key_id: &str) -> String {
        canonical_user(&self.owner_id(), key_id)
    }

    /// The deployment's 12-digit account id (see [`account_id`]).
    pub fn account_id(&self) -> String {
        account_id(&self.owner_id())
    }

    /// A bucket's one-per-bucket settings (`id = ""`), by kind.
    pub fn bucket_settings(&self, bucket_id: &str) -> Result<HashMap<String, String>> {
        self.read(|c| {
            let rows = c
                .prepare_cached("SELECT kind, value FROM bucket_config WHERE bucket_id = ?1 AND id = ''")?
                .query_map([bucket_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
                .collect::<rusqlite::Result<HashMap<_, _>>>()?;
            Ok(rows)
        })
    }

    /// One configuration value of a bucket.
    pub fn bucket_config(&self, bucket: &str, kind: &str, id: &str) -> Result<Option<String>> {
        self.read(|c| {
            let b = find_bucket(c, bucket)?;
            bucket_config_row(c, &b.id, kind, id)
        })
    }

    /// A bucket's configurations of `kind` with ids after `after`, in id order, at most `limit`.
    pub fn list_bucket_config(&self, bucket: &str, kind: &str, after: Option<&str>, limit: usize) -> Result<Vec<(String, String)>> {
        self.read(|c| {
            let b = find_bucket(c, bucket)?;
            let rows = c
                .prepare_cached("SELECT id, value FROM bucket_config WHERE bucket_id = ?1 AND kind = ?2 AND id > ?3 ORDER BY id LIMIT ?4")?
                .query_map(params![b.id, kind, after.unwrap_or(""), limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
    }

    /// Set (`Some`) or remove (`None`) a configuration value, durably. With `max`, adding a new
    /// id fails once the bucket has that many of `kind` (`StoreError::InvalidArgument`).
    pub async fn set_bucket_config(&self, bucket: &str, kind: &str, id: &str, value: Option<&str>, max: Option<usize>) -> Result<()> {
        self.write(|c| {
            let b = find_bucket(c, bucket)?;
            match value {
                Some(v) => {
                    if let Some(max) = max
                        && bucket_config_row(c, &b.id, kind, id)?.is_none()
                    {
                        let n: i64 = c.query_row("SELECT count(*) FROM bucket_config WHERE bucket_id = ?1 AND kind = ?2", params![b.id, kind], |r| r.get(0))?;
                        if n as usize >= max {
                            return Err(StoreError::InvalidArgument(format!("at most {max} {kind} configurations per bucket")));
                        }
                    }
                    c.execute(
                        "INSERT INTO bucket_config (bucket_id, kind, id, value) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (bucket_id, kind, id) DO UPDATE SET value = excluded.value",
                        params![b.id, kind, id, v],
                    )?;
                }
                None => {
                    c.execute("DELETE FROM bucket_config WHERE bucket_id = ?1 AND kind = ?2 AND id = ?3", params![b.id, kind, id])?;
                }
            }
            Ok(())
        })
        .await
    }

    /// The bucket's tags (empty: none).
    pub fn bucket_tags(&self, bucket: &str) -> Result<BTreeMap<String, String>> {
        match self.bucket_config(bucket, kind::TAGGING, "")? {
            Some(v) => Ok(serde_json::from_str(&v)?),
            None => Ok(BTreeMap::new()),
        }
    }

    /// Replace the bucket's tags (S3's limits: 50 tags, keys 1-128 and values up to 256
    /// characters, no `aws:` prefix). Empty removes them.
    pub async fn set_bucket_tags(&self, bucket: &str, tags: &BTreeMap<String, String>) -> Result<()> {
        validate_bucket_tags(tags.iter().map(|(k, v)| (k.as_str(), v.as_str())))?;
        let v = if tags.is_empty() { None } else { Some(rows::json(tags)?) };
        self.set_bucket_config(bucket, kind::TAGGING, "", v.as_deref(), None).await
    }

    /// The bucket's website configuration, if it has one.
    pub fn bucket_website(&self, bucket: &str) -> Result<Option<WebsiteConfig>> {
        self.bucket_config(bucket, kind::WEBSITE, "")?.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
    }

    /// The bucket's own public access block (`None`: it has none), and the one in force (with
    /// the deployment-wide block on top).
    pub fn public_access_block(&self, bucket_id: &str) -> Result<(Option<PublicAccessBlock>, PublicAccessBlock)> {
        let deployment = self.cfg.block_public_access;
        self.read(|c| {
            let own: Option<PublicAccessBlock> = bucket_config_row(c, bucket_id, kind::PUBLIC_ACCESS_BLOCK, "")?.map(|v| serde_json::from_str(&v)).transpose()?;
            Ok((own, effective_block(c, bucket_id, deployment)?))
        })
    }

    /// Change a visible object's ACL in place (a new row; branches keep the old one).
    pub async fn set_object_acl(&self, bucket: &str, key: &str, acl: Option<String>) -> Result<()> {
        let now = self.now();
        self.write(|c| {
            let b = find_bucket(c, bucket)?;
            let chain = rows::chain(c, &b.id)?;
            rows::update_in_place(c, &b.id, &chain, key, |r| r.headers.acl = acl, now)?;
            Ok(())
        })
        .await
    }
}

/// Bucket tags: at most 50, keys 1-128 and values 0-256 characters, unique keys, no `aws:`
/// prefix.
pub fn validate_bucket_tags<'a>(tags: impl IntoIterator<Item = (&'a str, &'a str)>) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    for (k, v) in tags {
        if seen.len() >= 50 {
            return Err(StoreError::InvalidArgument("Bucket tag count cannot be greater than 50".into()));
        }
        if k.is_empty() || k.chars().count() > 128 {
            return Err(StoreError::InvalidArgument("The TagKey you have provided is invalid".into()));
        }
        if v.chars().count() > 256 {
            return Err(StoreError::InvalidArgument("The TagValue you have provided is invalid".into()));
        }
        if k.to_ascii_lowercase().starts_with("aws:") {
            return Err(StoreError::InvalidArgument("System tags cannot be added/updated by requester".into()));
        }
        if !seen.insert(k) {
            return Err(StoreError::InvalidArgument("Cannot provide multiple Tags with the same key".into()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        let owner = "ab".repeat(32);
        let u = canonical_user(&owner, "RKABC");
        assert_eq!(u.len(), 64);
        assert_eq!(u, canonical_user(&owner, "RKABC"));
        assert_ne!(u, canonical_user(&owner, "RKABD"));
        assert_ne!(u, canonical_user(&"cd".repeat(32), "RKABC"));
        let a = account_id(&owner);
        assert_eq!(a.len(), 12);
        assert!(a.bytes().all(|b| b.is_ascii_digit()));
    }

    #[test]
    fn tag_rules() {
        let ok = |t: &[(&str, &str)]| validate_bucket_tags(t.iter().copied()).is_ok();
        assert!(ok(&[("a", ""), ("b", "x")]));
        assert!(!ok(&[("", "x")]));
        assert!(!ok(&[("aws:x", "y")]));
        assert!(!ok(&[("AWS:x", "y")]));
        assert!(!ok(&[("a", "1"), ("a", "2")]));
        assert!(!ok(&[(&"k".repeat(129), "v")]));
        assert!(!ok(&[("k", &"v".repeat(257))]));
        let many: Vec<(String, String)> = (0..51).map(|i| (format!("k{i}"), "v".into())).collect();
        assert!(validate_bucket_tags(many.iter().take(50).map(|(k, v)| (k.as_str(), v.as_str()))).is_ok());
        assert!(validate_bucket_tags(many.iter().map(|(k, v)| (k.as_str(), v.as_str()))).is_err());
    }

    #[test]
    fn blocks_combine() {
        let own = PublicAccessBlock { block_public_acls: true, ..Default::default() };
        assert_eq!(own.or(PublicAccessBlock::default()), own);
        assert_eq!(own.or(PublicAccessBlock::ALL), PublicAccessBlock::ALL);
    }
}
