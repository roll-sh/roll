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
    policy: Option<Option<String>>,
    public_access_block: Option<Option<PublicAccessBlock>>,
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
        v["replication"] = match &replication {
            Some(cfg) => {
                let rules: Vec<Value> = cfg
                    .rules
                    .iter()
                    .map(|r| {
                        let mut rv = rule_view(r);
                        let w = backlog.replicate.get(&r.destination);
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
        let policy = self.store.bucket_config(&b.name, kind::POLICY, "").ok().flatten();
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
        let mut out = Prepared { public_access_block: p.public_access_block, ..Default::default() };

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
                Some(Some(text))
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
            self.store.set_bucket_config(bucket, kind::POLICY, "", policy.as_deref(), None).await?;
        }
        if let Some(w) = &p.website {
            let v = w.as_ref().map(serde_json::to_string).transpose().map_err(anyhow::Error::from)?;
            self.store.set_bucket_config(bucket, kind::WEBSITE, "", v.as_deref(), None).await?;
        }
        if let Some(r) = &p.replication {
            self.store.set_replication(bucket, r.as_ref()).await?;
        }
        Ok(())
    }
}
