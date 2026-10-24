//! Bucket settings the control plane edits besides the store's own (`BucketUpdate`):
//! replication, the website's documents, the bucket policy and the public access block. Each
//! is checked as its S3 API call checks it, and everything in a `PATCH` is checked before
//! anything is applied ([`Control::prepare_settings`], then [`Control::apply_settings`]).

use crate::control::{Control, ControlError, Result};
use s3_store::config::kind;
use s3_store::events::{BACKFILL, NOTIFY, REPLICATE};
use s3_store::{Bucket, PublicAccessBlock, ReplicationConfig, ReplicationRule, RuleFilter, StoreError, WebsiteConfig};
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, HashSet};

/// Rows of the outbox counted for a bucket's backlog.
const BACKLOG_LIMIT: usize = 1000;

/// A present field, `null` included (`Some(None)`); absent fields are `None` (with
/// `#[serde(default)]`).
fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

/// A present JSON value, `null` included.
fn present_value<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// The settings fields of a bucket `PATCH`.
#[derive(Default, Deserialize)]
pub(crate) struct SettingsPatch {
    /// Replaces the replication rules; `null` (or no rules) removes them.
    #[serde(default, deserialize_with = "present")]
    pub replication: Option<Option<ReplicationPatch>>,
    /// The website's index and error documents; `null` removes the website configuration.
    #[serde(default, deserialize_with = "present")]
    pub website: Option<Option<WebsitePatch>>,
    /// The bucket policy (an object, or JSON text); `null` removes it.
    #[serde(default, deserialize_with = "present_value")]
    pub policy: Option<Value>,
    /// The bucket's own public access block; `null` removes it.
    #[serde(default, deserialize_with = "present")]
    pub public_access_block: Option<Option<PublicAccessBlock>>,
    /// The CDN (`{"enabled": true, "mode": "mutable"}`); `null` turns it off.
    #[serde(default, deserialize_with = "present")]
    pub cdn: Option<Option<s3_store::cdn::BucketCdn>>,
    /// The developer API's size limit: bytes, or `"20MB"`-style text; `null` removes it.
    #[serde(default, deserialize_with = "present_value")]
    pub file_size_limit: Option<Value>,
    /// Allowed media types (`image/*` wildcards); `null` (or `[]`) removes them.
    #[serde(default, deserialize_with = "present_value")]
    pub allowed_mime_types: Option<Value>,
}

#[derive(Deserialize)]
pub(crate) struct ReplicationPatch {
    pub rules: Vec<RuleInput>,
}

/// A replication rule as the control plane takes it (S3's V2 form, by bucket name). Unknown
/// fields (the view's `pending`, `last_error`, …) are ignored, so a view's rule can be sent back.
#[derive(Deserialize)]
pub(crate) struct RuleInput {
    pub destination: String,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub enabled: Option<bool>,
    #[serde(default)]
    pub prefix: Option<String>,
    #[serde(default)]
    pub tags: Option<BTreeMap<String, String>>,
    #[serde(default)]
    pub delete_markers: Option<bool>,
    #[serde(default)]
    pub existing_objects: Option<bool>,
    #[serde(default)]
    pub storage_class: Option<String>,
    #[serde(default)]
    pub owner_override: Option<bool>,
    /// The replication target (`/v1/storage/replication-targets`) the destination bucket is
    /// on; omitted: the target listing the bucket, else a roll bucket.
    #[serde(default)]
    pub target: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct WebsitePatch {
    pub index_document: String,
    #[serde(default)]
    pub error_document: Option<String>,
}

/// Checked settings, ready to apply.
#[derive(Default)]
pub(crate) struct Prepared {
    replication: Option<Option<ReplicationConfig>>,
    website: Option<Option<WebsiteConfig>>,
    /// The document and the principals it names.
    policy: Option<Option<(String, String)>>,
    public_access_block: Option<Option<PublicAccessBlock>>,
    cdn: Option<Option<s3_store::cdn::BucketCdn>>,
    /// The bucket's limits after this PATCH, when it changes them.
    limits: Option<s3_store::BucketLimits>,
}

fn invalid(m: impl Into<String>) -> ControlError {
    ControlError::Invalid(m.into())
}

/// A rule's view: its filter flattened to a prefix and tags.
fn rule_view(r: &ReplicationRule) -> Value {
    let tags: BTreeMap<&str, &str> = r.filter.iter().flat_map(|f| f.tags.iter().map(|(k, v)| (k.as_str(), v.as_str()))).collect();
    json!({
        "id": r.id,
        "priority": r.priority,
        "enabled": r.enabled,
        "prefix": r.prefix(),
        "tags": tags,
        "delete_markers": r.replicates_markers(),
        "existing_objects": r.existing_objects,
        "destination": r.destination,
        "target": r.target,
        "storage_class": r.storage_class,
        "owner_override": r.owner_override,
    })
}

/// What's waiting in the outbox for one destination.
#[derive(Default)]
struct Waiting {
    pending: u64,
    last_error: Option<String>,
}

impl Waiting {
    fn json(&self) -> Value {
        json!({ "pending": self.pending, "last_error": self.last_error })
    }
}

/// A bucket's outbox, by destination: notifications by ARN, replication by destination bucket,
/// and the rules still backfilling.
#[derive(Default)]
pub(crate) struct Backlog {
    notify: BTreeMap<String, Waiting>,
    replicate: BTreeMap<String, Waiting>,
    backfilling: HashSet<String>,
}

impl Backlog {
    pub fn notifications(&self) -> Value {
        Value::Object(self.notify.iter().map(|(k, w)| (k.clone(), w.json())).collect::<Map<_, _>>())
    }
}

impl Control {
    /// What's waiting in a bucket's outbox (none is read for a bucket without notifications or
    /// replication). At most [`BACKLOG_LIMIT`] rows are counted.
    pub(crate) fn backlog(&self, b: &Bucket, any: bool) -> Backlog {
        let mut out = Backlog::default();
        if !any {
            return out;
        }
        for e in self.store.pending_events(Some(&b.id), BACKLOG_LIMIT).unwrap_or_default() {
            let w = match e.kind.as_str() {
                NOTIFY => out.notify.entry(e.dest.clone()).or_default(),
                REPLICATE => out.replicate.entry(e.dest.strip_prefix("replicate:").unwrap_or(&e.dest).to_string()).or_default(),
                BACKFILL => {
                    out.backfilling.insert(e.dest.strip_prefix("backfill:").unwrap_or(&e.dest).to_string());
                    continue;
                }
                _ => continue,
            };
            w.pending += 1;
            if e.error.is_some() {
                w.last_error = e.error.clone();
            }
        }
        out
    }

    /// The `replication`, `policy`, `policy_public`, `public_access_block` and `website_url` of
    /// a bucket view, and its notifications backlog.
    pub(crate) fn settings_view(&self, b: &Bucket, v: &mut Value, has_notifications: bool, website: bool) {
        let replication = self.store.replication(&b.name).ok().flatten();
        let backlog = self.backlog(b, has_notifications || replication.is_some());
        // S3's replication metrics (and Replication Time Control's), per rule.
        let (pending, latency) = match &replication {
            Some(_) => (self.store.replication_backlog(&b.id).unwrap_or_default(), self.notifier.get().map(|n| n.replication_metrics(&b.id)).unwrap_or_default()),
            None => Default::default(),
        };
        let now = self.store.now();
        v["replication"] = match &replication {
            Some(cfg) => {
                let rules: Vec<Value> = cfg
                    .rules
                    .iter()
                    .map(|r| {
                        let mut rv = rule_view(r);
                        let dest = r.dest_key();
                        let w = backlog.replicate.get(&dest);
                        let p = pending.get(&r.id).cloned().unwrap_or_default();
                        let l = latency.get(&r.id);
                        rv["metrics"] = json!({
                            "enabled": r.metrics(),
                            "replication_time_control": r.replication_time(),
                            "operations_pending": p.operations,
                            "bytes_pending": p.bytes,
                            "oldest_pending_ms": p.oldest_at.map(|at| (now - at).max(0)),
                            "latency_ms": l.map(|l| l.last_ms),
                            "max_latency_ms": l.map(|l| l.max_ms),
                            "replicated": l.map_or(0, |l| l.replicated),
                            "operations_failed": l.map_or(0, |l| l.failed),
                            "replicated_after_threshold": l.map_or(0, |l| l.late),
                        });
                        rv["pending"] = json!(w.map_or(0, |w| w.pending));
                        rv["last_error"] = json!(w.and_then(|w| w.last_error.clone()));
                        rv["backfilling"] = json!(backlog.backfilling.contains(&r.id));
                        rv
                    })
                    .collect();
                json!({ "rules": rules })
            }
            None => Value::Null,
        };
        v["notifications"]["backlog"] = backlog.notifications();
        let policy = s3_server::bucket_policy(&self.store, &b.name);
        v["policy_public"] = json!(policy.as_deref().is_some_and(|p| s3_server::validate_bucket_policy(p, &b.name).unwrap_or(false)));
        v["policy"] = policy.map(|p| serde_json::from_str(&p).unwrap_or(Value::String(p))).unwrap_or(Value::Null);
        v["public_access_block"] = json!(self.store.public_access_block(&b.id).ok().and_then(|(own, _)| own));
        v["website_url"] = json!(if website { self.website_url(&b.name) } else { None });
    }

    /// `<scheme>://<bucket>.<website domain>[:port]`, with the endpoint's scheme and port.
    pub fn website_url(&self, bucket: &str) -> Option<String> {
        let domain = self.cfg.website_domain.as_deref()?;
        let u = url::Url::parse(&self.cfg.endpoint).ok()?;
        let port = u.port().map(|p| format!(":{p}")).unwrap_or_default();
        Some(format!("{}://{bucket}.{domain}{port}", u.scheme()))
    }

    /// Check a PATCH's settings against the bucket as it will be (`versioning` and `public`:
    /// the same PATCH's).
    pub(crate) fn prepare_settings(&self, b: &Bucket, versioning: Option<&str>, public: Option<bool>, p: SettingsPatch) -> Result<Prepared> {
        let mut out = Prepared { public_access_block: p.public_access_block, cdn: p.cdn, ..Default::default() };

        // Upload limits.
        if p.file_size_limit.is_some() || p.allowed_mime_types.is_some() {
            let mut l = self.store.bucket_limits(&b.name)?;
            if let Some(v) = &p.file_size_limit {
                l.file_size_limit = s3_store::api::parse_size(v).map_err(|m| invalid(format!("file_size_limit: {m}")))?;
            }
            if let Some(v) = &p.allowed_mime_types {
                l.allowed_mime_types = match v {
                    Value::Null => None,
                    Value::Array(a) => {
                        let list: Vec<String> = a.iter().map(|x| x.as_str().map(str::to_string).ok_or_else(|| invalid("allowed_mime_types must be an array of strings"))).collect::<Result<_>>()?;
                        let n = s3_store::api::normalize_mime_types(&list).map_err(|bad| invalid(format!("allowed_mime_types: {bad} isn't a media type (type/subtype or type/*)")))?;
                        Some(n).filter(|n| !n.is_empty())
                    }
                    _ => return Err(invalid("allowed_mime_types must be an array of strings")),
                };
            }
            out.limits = Some(l);
        }

        // The public access block in force once this PATCH is applied.
        let (own, _) = self.store.public_access_block(&b.id)?;
        let own = match &out.public_access_block {
            Some(new) => *new,
            None => own,
        };
        let mut block = own.unwrap_or_default();
        if self.store.cfg.block_public_access {
            block = block.or(PublicAccessBlock::ALL);
        }
        if public == Some(true) && block.block_public_policy {
            return Err(invalid(format!("bucket {} can't be made public: public access is blocked (BlockPublicPolicy)", b.name)));
        }

        out.policy = match p.policy {
            None => None,
            Some(Value::Null) => Some(None),
            Some(v) => {
                let text = match v {
                    Value::String(s) => s,
                    Value::Object(_) => v.to_string(),
                    _ => return Err(invalid("policy must be a bucket policy document (a JSON object)")),
                };
                let public = s3_server::validate_bucket_policy(&text, &b.name).map_err(|m| invalid(format!("invalid bucket policy: {m}")))?;
                if public && block.block_public_policy {
                    return Err(invalid("this policy grants public access, which is blocked (BlockPublicPolicy)"));
                }
                let principals = s3_server::bucket_policy_principals(&self.store, &text).map_err(|m| invalid(format!("invalid bucket policy: {m}")))?;
                Some(Some((text, principals)))
            }
        };

        out.website = match p.website {
            None => None,
            Some(None) => Some(None),
            Some(Some(w)) => {
                // PutBucketWebsite's checks.
                let index = w.index_document.trim().to_string();
                if index.is_empty() {
                    return Err(invalid("A value for IndexDocument Suffix must be provided"));
                }
                if index.contains('/') {
                    return Err(invalid("The IndexDocument Suffix is not well formed"));
                }
                let error = w.error_document.map(|e| e.trim().to_string()).filter(|e| !e.is_empty());
                if let Some(e) = &error {
                    s3_store::validate::key(e).map_err(|_| invalid("The ErrorDocument Key is not well formed"))?;
                }
                let routing_rules = self.store.bucket_website(&b.name)?.map(|w| w.routing_rules).unwrap_or_default();
                Some(Some(WebsiteConfig { index_document: Some(index), error_document: error, redirect_all: None, routing_rules }))
            }
        };

        out.replication = match p.replication {
            None => None,
            Some(None) => Some(None),
            Some(Some(r)) if r.rules.is_empty() => Some(None),
            Some(Some(r)) => Some(Some(self.replication_config(b, versioning, r.rules)?)),
        };
        Ok(out)
    }

    /// The replication configuration a PATCH asks for, checked as PutBucketReplication checks it.
    fn replication_config(&self, b: &Bucket, versioning: Option<&str>, rules: Vec<RuleInput>) -> Result<ReplicationConfig> {
        let existing = self.store.replication(&b.name)?.unwrap_or_default();
        let mut cfg = ReplicationConfig { role: existing.role.clone(), rules: Vec::with_capacity(rules.len()) };
        for r in rules {
            let id = r.id.unwrap_or_default().trim().to_string();
            let kept = existing.rules.iter().find(|e| !id.is_empty() && e.id == id);
            let prefix = r.prefix.filter(|p| !p.is_empty());
            let tags: Vec<(String, String)> = r.tags.unwrap_or_default().into_iter().collect();
            let and = usize::from(prefix.is_some()) + tags.len() > 1;
            cfg.rules.push(ReplicationRule {
                id,
                priority: r.priority,
                enabled: r.enabled.unwrap_or(true),
                legacy_prefix: None,
                filter: Some(RuleFilter { prefix, tags, and }),
                delete_markers: r.delete_markers.unwrap_or(false),
                destination: r.destination.trim().to_string(),
                target: match r.target.filter(|t| !t.is_empty()) {
                    Some(t) => Some(t),
                    None => self.store.target_for_bucket(r.destination.trim())?,
                },
                storage_class: r.storage_class.filter(|s| !s.is_empty()),
                owner_override: r.owner_override.unwrap_or(false),
                account: kept.and_then(|k| k.account.clone()),
                existing_objects: r.existing_objects.unwrap_or(false),
                source_selection: kept.and_then(|k| k.source_selection.clone()),
                destination_extra: kept.and_then(|k| k.destination_extra.clone()),
            });
        }
        cfg.validate(&b.name).map_err(invalid)?;
        for (k, v) in cfg.rules.iter().filter_map(|r| r.filter.as_ref()).flat_map(|f| &f.tags) {
            if k.is_empty() || k.len() > 128 || v.len() > 256 {
                return Err(invalid("a replication rule's tag keys are 1-128 characters and values at most 256"));
            }
        }
        let enabled = |v: Option<&str>| v == Some("Enabled");
        if !enabled(versioning.or(b.versioning.as_deref())) {
            return Err(invalid("Versioning must be 'Enabled' on the bucket to apply a replication configuration"));
        }
        for r in &cfg.rules {
            if let Some(t) = &r.target {
                if self.store.replication_target(t)?.is_none() {
                    return Err(invalid(format!("no replication target {t}")));
                }
                continue;
            }
            match self.store.bucket(&r.destination) {
                Ok(d) if !enabled(d.versioning.as_deref()) => return Err(invalid(format!("Destination bucket must have versioning enabled: {}", r.destination))),
                Ok(_) => {}
                Err(StoreError::NoSuchBucket(_)) => {
                    return Err(invalid(format!("Destination bucket must exist: {} (replication goes to buckets in this roll deployment)", r.destination)));
                }
                Err(e) => return Err(e.into()),
            }
        }
        Ok(cfg)
    }

    /// Check the destinations of replication rules on targets outside roll (the bucket has
    /// versioning enabled there, and Object Lock when the source has it), as PutBucketReplication
    /// does.
    pub(crate) async fn check_replication_targets(&self, b: &Bucket, p: &mut Prepared) -> Result<()> {
        let Some(Some(cfg)) = &mut p.replication else { return Ok(()) };
        if cfg.rules.iter().all(|r| r.target.is_none()) {
            return Ok(());
        }
        let n = self.notifier.get().ok_or_else(|| ControlError::Unavailable("replication to buckets outside roll needs the S3 API, which isn't running".into()))?;
        n.check_replication(cfg, b.object_lock.is_some()).await.map_err(invalid)
    }

    /// Apply checked settings: the public access block before the store's own update (it
    /// checks `public` against the block; `first`), the rest after (`!first`).
    pub(crate) async fn apply_settings(&self, bucket: &str, p: &Prepared, first: bool) -> Result<()> {
        if first {
            if let Some(pab) = &p.public_access_block {
                let v = pab.map(|b| serde_json::to_string(&b)).transpose().map_err(anyhow::Error::from)?;
                self.store.set_bucket_config(bucket, kind::PUBLIC_ACCESS_BLOCK, "", v.as_deref(), None).await?;
            }
            return Ok(());
        }
        if let Some(policy) = &p.policy {
            self.store.set_bucket_policy(bucket, policy.as_ref().map(|(d, b)| (d.as_str(), b.as_str()))).await?;
        }
        if let Some(w) = &p.website {
            let v = w.as_ref().map(serde_json::to_string).transpose().map_err(anyhow::Error::from)?;
            self.store.set_bucket_config(bucket, kind::WEBSITE, "", v.as_deref(), None).await?;
        }
        if let Some(r) = &p.replication {
            self.store.set_replication(bucket, r.as_ref()).await?;
        }
        if let Some(cdn) = &p.cdn {
            self.store.set_bucket_cdn(bucket, cdn.as_ref().filter(|c| c.enabled)).await?;
        }
        if let Some(l) = &p.limits {
            self.store.set_bucket_limits(bucket, l).await?;
        }
        Ok(())
    }
}
