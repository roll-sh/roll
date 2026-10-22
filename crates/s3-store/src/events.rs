//! The durable event outbox behind event notifications and replication, the notification and
//! replication configurations, and each version's replication status.
//!
//! - **Atomic.** Every change S3 can notify about records its events in the *same* metadata
//!   transaction as the change ([`emit`]): an event exists if and only if its write committed.
//!   A write that fails or is fenced rolls its events back with it, and a committed one's
//!   events survive restarts and recovery like any other row.
//! - **Free when unused.** [`emit`] looks up the bucket's notification and replication
//!   configurations (one primary-key read) and writes nothing unless one of them matches.
//! - **One row per delivery.** A change matching two destinations is two rows, retried on their
//!   own. Rows share the change's sequencer (S3's `sequencer`: monotonic, never reused, so a
//!   key's events compare in order), and a row is only handed out once every older row for the
//!   same destination and key is gone, so each destination sees a key's events in order.
//! - **Only durable rows go out.** [`Store::due_events`] waits until everything it read is in
//!   object storage, so an event is never delivered for a write that wasn't acknowledged.
//! - **At least once.** Delivered rows are deleted with relaxed writes: a crash may redeliver,
//!   never lose. Rows that keep failing past the retention window are dropped by the dispatcher
//!   (counted and logged).
//! - **Replication** rows are applied by [`Store::replicate`] in one durable transaction that
//!   also deletes the row, so a replica is written exactly once.
//!
//! The dispatcher itself (delivery, retries, the S3 event JSON) is `s3-server`'s.

use crate::error::{Result, StoreError};
use crate::model::*;
use crate::rows::{self, Row};
use crate::store::{BucketRow, Store, find_bucket, find_bucket_id};
use crate::versioning::{self, Mode};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

/// `bucket_config` kinds owned here.
pub mod kind {
    pub const NOTIFICATION: &str = "notification";
    pub const REPLICATION: &str = "replication";
    pub const WEBHOOK_SECRET: &str = "webhook_secret";
}

/// Outbox row kinds.
pub const NOTIFY: &str = "notify";
pub const REPLICATE: &str = "replicate";
pub const BACKFILL: &str = "backfill";

/// The destination of EventBridge rows (`EventBridgeConfiguration` has no ARN).
pub const EVENT_BRIDGE: &str = "eventbridge";

/// S3's event types, as configurations name them.
pub const EVENT_TYPES: &[&str] = &[
    "s3:TestEvent",
    "s3:ObjectCreated:*",
    "s3:ObjectCreated:Put",
    "s3:ObjectCreated:Post",
    "s3:ObjectCreated:Copy",
    "s3:ObjectCreated:CompleteMultipartUpload",
    "s3:ObjectRemoved:*",
    "s3:ObjectRemoved:Delete",
    "s3:ObjectRemoved:DeleteMarkerCreated",
    "s3:ObjectRestore:*",
    "s3:ObjectRestore:Post",
    "s3:ObjectRestore:Completed",
    "s3:ObjectRestore:Delete",
    "s3:ReducedRedundancyLostObject",
    "s3:Replication:*",
    "s3:Replication:OperationFailedReplication",
    "s3:Replication:OperationNotTracked",
    "s3:Replication:OperationMissedThreshold",
    "s3:Replication:OperationReplicatedAfterThreshold",
    "s3:LifecycleExpiration:*",
    "s3:LifecycleExpiration:Delete",
    "s3:LifecycleExpiration:DeleteMarkerCreated",
    "s3:LifecycleTransition",
    "s3:IntelligentTiering",
    "s3:ObjectTagging:*",
    "s3:ObjectTagging:Put",
    "s3:ObjectTagging:Delete",
    "s3:ObjectAcl:Put",
];

/// Whether a configuration's event type (`s3:ObjectCreated:*`) covers an event
/// (`ObjectCreated:Put`, without the `s3:` prefix, as event records name it).
pub fn event_matches(pattern: &str, name: &str) -> bool {
    let Some(p) = pattern.strip_prefix("s3:") else { return false };
    match p.strip_suffix(":*") {
        Some(family) => name.strip_prefix(family).is_some_and(|rest| rest.starts_with(':')),
        None => p == name,
    }
}

/// Whether two event types can match the same event.
fn events_overlap(a: &str, b: &str) -> bool {
    let fam = |e: &str| e.strip_prefix("s3:").unwrap_or(e).split(':').next().unwrap_or_default().to_string();
    a == b || (fam(a) == fam(b) && (a.ends_with(":*") || b.ends_with(":*")))
}

// ---- Notification configuration ----

/// A bucket's notification configuration (S3's `NotificationConfiguration`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationConfig {
    /// Topic, queue and function configurations, in the order given.
    pub configurations: Vec<NotificationTarget>,
    /// `EventBridgeConfiguration`: every event also goes to EventBridge.
    pub event_bridge: bool,
}

/// One `TopicConfiguration`, `QueueConfiguration` or `CloudFunctionConfiguration`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationTarget {
    /// S3's `Id` (generated when not given).
    pub id: String,
    /// `queue`, `topic` or `function`: the element it's configured as.
    #[serde(rename = "type")]
    pub kind: String,
    /// The destination's ARN (`arn:aws:sqs:…`, `arn:roll:queues:…`, `arn:roll:webhook:…`, …).
    pub destination: String,
    /// Event types (`s3:ObjectCreated:*`, …).
    pub events: Vec<String>,
    /// `FilterRule` `prefix` and `suffix` on the key.
    pub prefix: Option<String>,
    pub suffix: Option<String>,
}

impl NotificationTarget {
    pub fn matches(&self, event: &str, key: &str) -> bool {
        self.events.iter().any(|p| event_matches(p, event))
            && self.prefix.as_deref().is_none_or(|p| key.starts_with(p))
            && self.suffix.as_deref().is_none_or(|s| key.ends_with(s))
    }
}

fn random_id(prefix: &str) -> String {
    let mut b = [0u8; 12];
    getrandom::fill(&mut b).expect("os randomness");
    format!("{prefix}{}", hex::encode(b))
}

impl NotificationConfig {
    /// S3's rules: known event types, at least one per configuration, unique ids (missing ones
    /// are generated), filter values up to 1024 bytes, and no two configurations that could
    /// both match one event (a shared event type with overlapping prefix and suffix filters).
    pub fn validate(&mut self) -> Result<(), String> {
        if self.configurations.len() > 1000 {
            return Err("at most 1000 notification configurations per bucket".into());
        }
        let mut ids = std::collections::HashSet::new();
        for t in &mut self.configurations {
            if t.id.is_empty() {
                t.id = random_id("roll-");
            }
            if t.id.len() > 255 {
                return Err("a configuration Id is at most 255 characters".into());
            }
            if !ids.insert(t.id.clone()) {
                return Err(format!("the configuration Id {} is used twice", t.id));
            }
            if !matches!(t.kind.as_str(), "queue" | "topic" | "function") {
                return Err(format!("unknown configuration type {:?} (queue, topic or function)", t.kind));
            }
            if t.destination.trim().is_empty() {
                return Err("every configuration needs a destination ARN".into());
            }
            if t.events.is_empty() {
                return Err("every configuration needs at least one event".into());
            }
            for e in &t.events {
                if e == "s3:TestEvent" || !EVENT_TYPES.contains(&e.as_str()) {
                    return Err(format!("The event is not supported for notifications: {e}"));
                }
            }
            for v in [&t.prefix, &t.suffix].into_iter().flatten() {
                if v.len() > 1024 {
                    return Err("a filter rule value is at most 1024 bytes".into());
                }
            }
            t.prefix = t.prefix.take().filter(|p| !p.is_empty());
            t.suffix = t.suffix.take().filter(|s| !s.is_empty());
        }
        let cs = &self.configurations;
        for (i, a) in cs.iter().enumerate() {
            for b in &cs[i + 1..] {
                let events = a.events.iter().any(|x| b.events.iter().any(|y| events_overlap(x, y)));
                let prefixes = match (&a.prefix, &b.prefix) {
                    (Some(x), Some(y)) => x.starts_with(y.as_str()) || y.starts_with(x.as_str()),
                    _ => true,
                };
                let suffixes = match (&a.suffix, &b.suffix) {
                    (Some(x), Some(y)) => x.ends_with(y.as_str()) || y.ends_with(x.as_str()),
                    _ => true,
                };
                if events && prefixes && suffixes {
                    return Err("Configurations overlap. Configurations on the same bucket cannot share a common event type.".into());
                }
            }
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.configurations.is_empty() && !self.event_bridge
    }
}

// ---- Replication configuration ----

/// A bucket's replication configuration (S3's `ReplicationConfiguration`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplicationConfig {
    /// Accepted and returned; roll needs no role.
    pub role: String,
    pub rules: Vec<ReplicationRule>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplicationRule {
    pub id: String,
    pub priority: Option<i64>,
    pub enabled: bool,
    /// The legacy (V1) form: `<Prefix>` on the rule, no `<Filter>`. Delete markers replicate.
    pub legacy_prefix: Option<String>,
    /// `<Filter>`: a prefix, one tag, or `<And>` of a prefix and tags.
    pub filter: Option<RuleFilter>,
    /// `DeleteMarkerReplication` `Enabled`.
    pub delete_markers: bool,
    /// The destination bucket (a roll bucket, by name).
    pub destination: String,
    pub storage_class: Option<String>,
    /// `AccessControlTranslation` `Owner=Destination`: replicas are private to the destination.
    pub owner_override: bool,
    pub account: Option<String>,
    /// `ExistingObjectReplication` `Enabled`: versions written before the rule replicate too.
    pub existing_objects: bool,
    /// Elements stored and returned as given but not acted on (`SourceSelectionCriteria`, and
    /// the destination's `EncryptionConfiguration`, `ReplicationTime` and `Metrics`), as XML.
    pub source_selection: Option<String>,
    pub destination_extra: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RuleFilter {
    pub prefix: Option<String>,
    pub tags: Vec<(String, String)>,
    /// Written as `<And>`.
    pub and: bool,
}

impl ReplicationRule {
    pub fn prefix(&self) -> &str {
        self.legacy_prefix.as_deref().or(self.filter.as_ref().and_then(|f| f.prefix.as_deref())).unwrap_or_default()
    }

    /// Whether a version of `key` with these tags is in scope.
    pub fn matches(&self, key: &str, tags: &BTreeMap<String, String>) -> bool {
        key.starts_with(self.prefix()) && self.filter.as_ref().is_none_or(|f| f.tags.iter().all(|(k, v)| tags.get(k) == Some(v)))
    }

    /// Whether delete markers replicate: V1 rules always, V2 rules when enabled.
    pub fn replicates_markers(&self) -> bool {
        self.delete_markers || self.legacy_prefix.is_some()
    }
}

impl ReplicationConfig {
    pub fn validate(&mut self, source: &str) -> Result<(), String> {
        if self.rules.is_empty() {
            return Err("a replication configuration needs at least one rule".into());
        }
        if self.rules.len() > 1000 {
            return Err("at most 1000 replication rules".into());
        }
        let mut ids = std::collections::HashSet::new();
        let mut priorities = std::collections::HashSet::new();
        let v2 = self.rules.iter().any(|r| r.filter.is_some());
        for r in &mut self.rules {
            if r.id.is_empty() {
                r.id = random_id("");
            }
            if r.id.len() > 255 {
                return Err("a rule ID is at most 255 characters".into());
            }
            if !ids.insert(r.id.clone()) {
                return Err("Rule Id must be unique".into());
            }
            if v2 && r.legacy_prefix.is_some() {
                return Err("rules can't mix the Prefix and Filter forms".into());
            }
            if v2 && let Some(p) = r.priority
                && !priorities.insert(p)
            {
                return Err("Found duplicate priority. Priority must be unique among all rules".into());
            }
            if r.destination.is_empty() {
                return Err("every rule needs a destination bucket".into());
            }
            if r.destination == source {
                return Err("Destination bucket cannot be the same as the source bucket.".into());
            }
        }
        Ok(())
    }
}

// ---- Who made a change ----

/// Who and what made the change being written, for event records. `s3-server` sets it for each
/// request ([`with_origin`]); changes made outside a request (lifecycle, replication, the
/// control plane) have none.
#[derive(Clone, Debug, Default)]
pub struct Origin {
    /// The requester (`userIdentity.principalId`).
    pub principal: Option<String>,
    pub source_ip: Option<String>,
    pub request_id: Option<String>,
    /// How a plain write arrived: `Put` (default) or `Post`.
    pub create: Option<&'static str>,
}

tokio::task_local! {
    static ORIGIN: RefCell<Origin>;
}

/// Run `f` with `origin` describing the changes it makes.
pub async fn with_origin<F: std::future::Future>(origin: Origin, f: F) -> F::Output {
    ORIGIN.scope(RefCell::new(origin), f).await
}

/// Change the current request's origin (once it's authenticated, say). Outside
/// [`with_origin`] this does nothing.
pub fn set_origin(f: impl FnOnce(&mut Origin)) {
    let _ = ORIGIN.try_with(|o| f(&mut o.borrow_mut()));
}

fn origin() -> Origin {
    ORIGIN.try_with(|o| o.borrow().clone()).unwrap_or_default()
}

// ---- Event records ----

/// What an outbox row of kind [`NOTIFY`] carries: everything the S3 event JSON needs, captured
/// when the change was made.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct EventRecord {
    /// Without `s3:`: `ObjectCreated:Put`, `TestEvent`, …
    pub name: String,
    /// Unix ms.
    pub time: i64,
    pub bucket: String,
    pub key: String,
    pub size: Option<u64>,
    pub etag: Option<String>,
    /// Only for versioned buckets (as S3).
    pub version_id: Option<String>,
    /// 18 upper-case hex digits; compare as strings within one key.
    pub sequencer: String,
    /// The matching configuration's `Id`.
    pub config_id: String,
    pub principal: Option<String>,
    pub source_ip: Option<String>,
    pub request_id: Option<String>,
    /// For `Replication:*` events: why.
    pub reason: Option<String>,
    /// For replication events: the destination bucket.
    pub replication_destination: Option<String>,
}

/// What an outbox row of kind [`REPLICATE`] carries.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReplicateTask {
    pub rule: String,
    pub destination: String,
    /// S3's version id (`null` for the null version).
    pub version_id: String,
    pub owner_override: bool,
}

/// What an outbox row of kind [`BACKFILL`] carries: `ExistingObjectReplication` walking the
/// bucket's versions.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackfillTask {
    pub rule: String,
    /// Resume after this version.
    pub key_marker: Option<String>,
    pub version_id_marker: Option<String>,
}

/// A row of the outbox.
#[derive(Clone, Debug)]
pub struct PendingEvent {
    /// The row's id (also its sequencer, for the first row of a change).
    pub seq: i64,
    pub bucket_id: String,
    pub key: String,
    /// The destination ARN ([`NOTIFY`]), `replicate:<bucket>` or `backfill:<rule>`.
    pub dest: String,
    pub kind: String,
    pub body: String,
    pub created_at: i64,
    pub attempts: u32,
    pub error: Option<String>,
}

impl PendingEvent {
    pub fn record(&self) -> Result<EventRecord> {
        Ok(serde_json::from_str(&self.body)?)
    }
}

/// How a replication row ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Replicated {
    /// The replica is in the destination (written now, or already there).
    Completed,
    /// It can't be replicated (the source version is now `FAILED`).
    Failed(String),
    /// Nothing to do: the source version or bucket is gone, or the row was handled already.
    Skipped,
}

/// Per-store state of the outbox.
#[derive(Default)]
pub(crate) struct EventState {
    /// Some replication status was ever written (until then reads skip the lookup).
    has_status: AtomicBool,
}

const EVENT_COLS: &str = "seq, bucket_id, key, dest, kind, body, created_at, attempts, error";

fn event_row(r: &rusqlite::Row) -> rusqlite::Result<PendingEvent> {
    Ok(PendingEvent {
        seq: r.get(0)?,
        bucket_id: r.get(1)?,
        key: r.get(2)?,
        dest: r.get(3)?,
        kind: r.get(4)?,
        body: r.get(5)?,
        created_at: r.get(6)?,
        attempts: r.get::<_, i64>(7)?.max(0) as u32,
        error: r.get(8)?,
    })
}

/// The bucket's notification and replication configurations, if it has either.
fn targets(c: &Connection, bucket_id: &str) -> Result<Option<(Option<NotificationConfig>, Option<ReplicationConfig>)>> {
    let rows = c
        .prepare_cached("SELECT kind, value FROM bucket_config WHERE bucket_id = ?1 AND id = '' AND kind IN ('notification', 'replication')")?
        .query_map([bucket_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if rows.is_empty() {
        return Ok(None);
    }
    let (mut n, mut r) = (None, None);
    for (k, v) in rows {
        if k == kind::NOTIFICATION {
            n = Some(serde_json::from_str(&v)?);
        } else {
            r = Some(serde_json::from_str(&v)?);
        }
    }
    Ok(Some((n, r)))
}

/// Take `n` sequence numbers (never reused: the counter lives in `state`, not in the table).
fn take_seqs(c: &Connection, n: i64) -> Result<i64> {
    let cur: i64 = c.query_row("SELECT v FROM state WHERE k = 'event_seq'", [], |r| r.get::<_, String>(0)).optional()?.and_then(|v| v.parse().ok()).unwrap_or(0);
    c.execute("INSERT INTO state (k, v) VALUES ('event_seq', ?1) ON CONFLICT (k) DO UPDATE SET v = excluded.v", [(cur + n).to_string()])?;
    Ok(cur + 1)
}

pub fn sequencer(seq: i64) -> String {
    format!("{seq:018X}")
}

/// `next_at` of a row queued behind an older row for the same destination and key: it isn't due
/// until that one is gone ([`remove_event`] makes it due), so a backlog behind a failing
/// destination costs the dispatcher nothing.
const WAITING: i64 = i64::MAX;

#[allow(clippy::too_many_arguments)]
fn insert_event(c: &Connection, seq: i64, bucket_id: &str, key: &str, dest: &str, kind: &str, body: &str, now: i64) -> Result<()> {
    let behind = c.prepare_cached("SELECT 1 FROM events WHERE dest = ?1 AND key = ?2 LIMIT 1")?.exists(params![dest, key])?;
    let next_at = if behind { WAITING } else { now };
    c.prepare_cached("INSERT INTO events (seq, bucket_id, key, dest, kind, body, created_at, attempts, next_at, error) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, NULL)")?
        .execute(params![seq, bucket_id, key, dest, kind, body, now, next_at])?;
    Ok(())
}

/// Delete an outbox row; the next one for its destination and key becomes due. `false`: it was
/// gone already.
fn remove_event(c: &Connection, seq: i64, now: i64) -> Result<bool> {
    let Some((dest, key)) = c.prepare_cached("SELECT dest, key FROM events WHERE seq = ?1")?.query_row([seq], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))).optional()? else {
        return Ok(false);
    };
    c.prepare_cached("DELETE FROM events WHERE seq = ?1")?.execute([seq])?;
    c.prepare_cached("UPDATE events SET next_at = ?3 WHERE seq = (SELECT seq FROM events WHERE dest = ?1 AND key = ?2 ORDER BY seq LIMIT 1) AND next_at = ?4")?
        .execute(params![dest, key, now, WAITING])?;
    Ok(true)
}

fn set_status(c: &Connection, bucket_id: &str, key: &str, version_id: &str, status: Option<&str>) -> Result<()> {
    match status {
        Some(s) => c
            .prepare_cached("INSERT INTO replication_status (bucket_id, key, version_id, status) VALUES (?1, ?2, ?3, ?4) ON CONFLICT (bucket_id, key, version_id) DO UPDATE SET status = excluded.status")?
            .execute(params![bucket_id, key, version_id, s])?,
        None => c.prepare_cached("DELETE FROM replication_status WHERE bucket_id = ?1 AND key = ?2 AND version_id = ?3")?.execute(params![bucket_id, key, version_id])?,
    };
    Ok(())
}

/// A change to report.
pub(crate) struct Change<'a> {
    /// Without `s3:`: `ObjectCreated:Put`, …
    pub name: &'a str,
    pub key: &'a str,
    /// S3's version id of the version concerned (`null` for the null version).
    pub version_id: Option<String>,
    pub size: Option<u64>,
    pub etag: Option<&'a str>,
    /// The version's tags if the change replicates (new versions, user delete markers,
    /// restores); `None` if it doesn't.
    pub replicate: Option<&'a BTreeMap<String, String>>,
    /// Overrides the request's principal.
    pub principal: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub replication_destination: Option<&'a str>,
}

impl<'a> Change<'a> {
    pub fn new(name: &'a str, key: &'a str) -> Self {
        Self { name, key, version_id: None, size: None, etag: None, replicate: None, principal: None, reason: None, replication_destination: None }
    }

    /// A new version (or a restored one): its id, size and ETag, and it replicates.
    pub fn version(name: &'a str, r: &'a Row) -> Self {
        let data = r.blob_id.is_some();
        Self { version_id: Some(r.version_id()), size: data.then_some(r.size), etag: data.then_some(r.etag.as_str()), replicate: Some(&r.headers.tags), ..Self::new(name, &r.key) }
    }
}

/// Record `ch`'s events (and replication) for bucket `b`, inside the transaction making the
/// change. Writes nothing unless a configuration matches.
pub(crate) fn emit(c: &Connection, b: &BucketRow, ch: Change<'_>, now: i64) -> Result<()> {
    let Some((notify, replicate)) = targets(c, &b.id)? else { return Ok(()) };
    let mut rows: Vec<(String, &'static str, String)> = Vec::new(); // (dest, kind, body)
    let o = origin();
    let mut record = EventRecord {
        name: ch.name.to_string(),
        time: now,
        bucket: b.name.clone(),
        key: ch.key.to_string(),
        size: ch.size,
        etag: ch.etag.map(str::to_string),
        version_id: if b.versioning.is_some() { ch.version_id.clone() } else { None },
        sequencer: String::new(),
        config_id: String::new(),
        principal: ch.principal.map(str::to_string).or(o.principal),
        source_ip: o.source_ip,
        request_id: o.request_id,
        reason: ch.reason.map(str::to_string),
        replication_destination: ch.replication_destination.map(str::to_string),
    };
    let mut notify_hits: Vec<(String, String)> = Vec::new(); // (dest, config id)
    if let Some(n) = &notify {
        for t in n.configurations.iter().filter(|t| t.matches(ch.name, ch.key)) {
            notify_hits.push((t.destination.clone(), t.id.clone()));
        }
        if n.event_bridge {
            notify_hits.push((EVENT_BRIDGE.to_string(), String::new()));
        }
    }
    // Replication: per destination bucket, the highest-priority enabled rule in scope.
    let mut replicas: BTreeMap<String, &ReplicationRule> = BTreeMap::new();
    if let (Some(cfg), Some(tags), Some(vid)) = (&replicate, ch.replicate, &ch.version_id) {
        let marker = ch.size.is_none();
        for r in cfg.rules.iter().filter(|r| r.enabled && r.matches(ch.key, tags) && (!marker || r.replicates_markers())) {
            let better = replicas.get(&r.destination).is_none_or(|cur| r.priority.unwrap_or(0) > cur.priority.unwrap_or(0));
            if better {
                replicas.insert(r.destination.clone(), r);
            }
        }
        if replicas.is_empty() {
            // A new version out of scope: no status (a replaced null version's goes).
            set_status(c, &b.id, ch.key, vid, None)?;
        }
    }
    if notify_hits.is_empty() && replicas.is_empty() {
        return Ok(());
    }
    let first = take_seqs(c, (notify_hits.len() + replicas.len()) as i64)?;
    record.sequencer = sequencer(first);
    for (dest, id) in notify_hits {
        record.config_id = id;
        rows.push((dest, NOTIFY, serde_json::to_string(&record)?));
    }
    if let Some(vid) = &ch.version_id {
        for (dest, rule) in &replicas {
            let task = ReplicateTask { rule: rule.id.clone(), destination: dest.clone(), version_id: vid.clone(), owner_override: rule.owner_override };
            rows.push((format!("replicate:{dest}"), REPLICATE, serde_json::to_string(&task)?));
        }
        if !replicas.is_empty() {
            set_status(c, &b.id, ch.key, vid, Some("PENDING"))?;
        }
    }
    for (i, (dest, kind, body)) in rows.iter().enumerate() {
        insert_event(c, first + i as i64, &b.id, ch.key, dest, kind, body, now)?;
    }
    Ok(())
}

/// `ObjectCreated:Put`, or `ObjectCreated:Post` for a browser form upload.
pub(crate) fn created() -> String {
    format!("ObjectCreated:{}", origin().create.unwrap_or("Put"))
}

/// A version's metadata changed (`ObjectTagging:*`, `ObjectAcl:Put`).
pub(crate) fn emit_update(c: &Connection, b: &BucketRow, name: &str, r: &Row, now: i64) -> Result<()> {
    let mut ch = Change::new(name, &r.key);
    ch.version_id = Some(r.version_id());
    emit(c, b, ch, now)
}

/// The outcome of a delete (`DeleteObject`, lifecycle) as its event: `removed` is the family
/// (`ObjectRemoved` or `LifecycleExpiration`). User-made delete markers replicate.
pub(crate) fn emit_delete(c: &Connection, b: &BucketRow, key: &str, o: &DeleteOutcome, by_version: bool, removed: &str, now: i64) -> Result<()> {
    if !o.existed && !o.delete_marker {
        return Ok(());
    }
    if by_version && !o.existed {
        return Ok(());
    }
    let none = BTreeMap::new();
    let marker_created = o.delete_marker && !by_version;
    let name = if marker_created { format!("{removed}:DeleteMarkerCreated") } else { format!("{removed}:Delete") };
    let mut ch = Change::new(&name, key);
    ch.version_id = o.version_id.clone();
    if marker_created && removed == "ObjectRemoved" {
        ch.replicate = Some(&none);
    }
    emit(c, b, ch, now)
}

impl Store {
    // ---- Configuration ----

    /// The bucket's notification configuration (`None`: none).
    pub fn notifications(&self, bucket: &str) -> Result<Option<NotificationConfig>> {
        self.bucket_config(bucket, kind::NOTIFICATION, "")?.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
    }

    /// Replace the bucket's notification configuration (`None` or an empty one removes it).
    /// The caller validated it ([`NotificationConfig::validate`]) and its destinations.
    pub async fn set_notifications(&self, bucket: &str, cfg: Option<&NotificationConfig>) -> Result<()> {
        let v = cfg.filter(|c| !c.is_empty()).map(rows::json).transpose()?;
        self.set_bucket_config(bucket, kind::NOTIFICATION, "", v.as_deref(), None).await
    }

    /// The bucket's replication configuration (`None`: none).
    pub fn replication(&self, bucket: &str) -> Result<Option<ReplicationConfig>> {
        self.bucket_config(bucket, kind::REPLICATION, "")?.map(|v| Ok(serde_json::from_str(&v)?)).transpose()
    }

    /// Set (`Some`) or remove (`None`) the bucket's replication configuration. Like S3, the
    /// bucket and every destination must have versioning enabled (checked in the same
    /// transaction). Rules with `ExistingObjectReplication` start a backfill of the versions
    /// already there.
    pub async fn set_replication(&self, bucket: &str, cfg: Option<&ReplicationConfig>) -> Result<()> {
        let now = self.now();
        self.write(|c| {
            let b = find_bucket(c, bucket)?;
            let Some(cfg) = cfg else {
                c.execute("DELETE FROM bucket_config WHERE bucket_id = ?1 AND kind = ?2 AND id = ''", params![b.id, kind::REPLICATION])?;
                return Ok(());
            };
            if Mode::of(&b) != Mode::Enabled {
                return Err(StoreError::InvalidRequest("Versioning must be 'Enabled' on the bucket to apply a replication configuration".into()));
            }
            for r in &cfg.rules {
                let d = match find_bucket(c, &r.destination) {
                    Ok(d) => d,
                    Err(StoreError::NoSuchBucket(_)) => return Err(StoreError::InvalidRequest(format!("Destination bucket must exist: {}", r.destination))),
                    Err(e) => return Err(e),
                };
                if Mode::of(&d) != Mode::Enabled {
                    return Err(StoreError::InvalidRequest(format!("Destination bucket must have versioning enabled: {}", r.destination)));
                }
            }
            c.execute(
                "INSERT INTO bucket_config (bucket_id, kind, id, value) VALUES (?1, ?2, '', ?3) ON CONFLICT (bucket_id, kind, id) DO UPDATE SET value = excluded.value",
                params![b.id, kind::REPLICATION, rows::json(cfg)?],
            )?;
            self.events.has_status.store(true, Ordering::Relaxed);
            let fills: Vec<&ReplicationRule> = cfg.rules.iter().filter(|r| r.enabled && r.existing_objects).collect();
            if !fills.is_empty() {
                let first = take_seqs(c, fills.len() as i64)?;
                for (i, r) in fills.iter().enumerate() {
                    let task = BackfillTask { rule: r.id.clone(), ..Default::default() };
                    insert_event(c, first + i as i64, &b.id, "", &format!("backfill:{}", r.id), BACKFILL, &rows::json(&task)?, now)?;
                }
            }
            Ok(())
        })
        .await
    }

    /// The bucket's webhook signing secret, created on first use (`rotate`: replaced).
    pub async fn webhook_secret(&self, bucket: &str, rotate: bool) -> Result<String> {
        if !rotate && let Some(s) = self.bucket_config(bucket, kind::WEBHOOK_SECRET, "")? {
            return Ok(s);
        }
        let fresh = random_id("whsec_") + &random_id("");
        self.write(|c| {
            let b = find_bucket(c, bucket)?;
            if !rotate && let Some(s) = crate::config::bucket_config_row(c, &b.id, kind::WEBHOOK_SECRET, "")? {
                return Ok(s);
            }
            c.execute(
                "INSERT INTO bucket_config (bucket_id, kind, id, value) VALUES (?1, ?2, '', ?3) ON CONFLICT (bucket_id, kind, id) DO UPDATE SET value = excluded.value",
                params![b.id, kind::WEBHOOK_SECRET, fresh],
            )?;
            Ok(fresh.clone())
        })
        .await
    }

    /// The bucket's webhook secret, if it has one (by bucket id: deliveries outlive renames
    /// and deletes).
    pub fn webhook_secret_of(&self, bucket_id: &str) -> Result<Option<String>> {
        self.read(|c| crate::config::bucket_config_row(c, bucket_id, kind::WEBHOOK_SECRET, ""))
    }

    // ---- The outbox ----

    /// Up to `limit` rows ready to go (due, and the oldest of their destination and key), in
    /// sequence order. Returns once everything read is durable, so nothing returned belongs to
    /// a write that may still fail.
    pub async fn due_events(&self, limit: usize) -> Result<Vec<PendingEvent>> {
        self.log.check_open()?;
        let now = self.now();
        let (rows, batch) = self.log.db.read_seen(|c| {
            Ok(c.prepare_cached(&format!(
                "SELECT {EVENT_COLS} FROM events e WHERE next_at <= ?1
                   AND NOT EXISTS (SELECT 1 FROM events p WHERE p.dest = e.dest AND p.key = e.key AND p.seq < e.seq)
                 ORDER BY next_at, seq LIMIT ?2"
            ))?
            .query_map(params![now, limit as i64], event_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?)
        })?;
        self.log.wait(batch).await?;
        Ok(rows)
    }

    /// Every row in the outbox (of one bucket, by id), oldest first, at most `limit`.
    pub fn pending_events(&self, bucket_id: Option<&str>, limit: usize) -> Result<Vec<PendingEvent>> {
        self.read(|c| {
            let rows = match bucket_id {
                Some(b) => c
                    .prepare(&format!("SELECT {EVENT_COLS} FROM events WHERE bucket_id = ?1 ORDER BY seq LIMIT ?2"))?
                    .query_map(params![b, limit as i64], event_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
                None => c.prepare(&format!("SELECT {EVENT_COLS} FROM events ORDER BY seq LIMIT ?1"))?.query_map([limit as i64], event_row)?.collect::<rusqlite::Result<Vec<_>>>()?,
            };
            Ok(rows)
        })
    }

    /// How many rows the outbox holds.
    pub fn events_backlog(&self) -> Result<u64> {
        self.read(|c| Ok(c.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))? as u64))
    }

    /// Rows that were delivered (or are dropped for good) go. A relaxed write: if it's lost in
    /// a crash, they're delivered again (at least once).
    pub fn events_done(&self, seqs: &[i64]) -> Result<()> {
        if seqs.is_empty() {
            return Ok(());
        }
        let now = self.now();
        self.write_relaxed(|c| {
            for s in seqs {
                remove_event(c, *s, now)?;
            }
            Ok(())
        })
    }

    /// A delivery failed: try again at `next_at` (a relaxed write).
    pub fn event_retry(&self, seq: i64, next_at: i64, error: &str) -> Result<()> {
        let error: String = error.chars().take(512).collect();
        self.write_relaxed(|c| {
            c.prepare_cached("UPDATE events SET attempts = attempts + 1, next_at = ?2, error = ?3 WHERE seq = ?1")?.execute(params![seq, next_at, error])?;
            Ok(())
        })
    }

    // ---- Replication ----

    /// The replication status S3 shows for a version (`PENDING`, `COMPLETED`, `FAILED` or
    /// `REPLICA`), if any.
    pub fn replication_status(&self, bucket_id: &str, key: &str, version_id: &str) -> Result<Option<String>> {
        if !self.events.has_status.load(Ordering::Relaxed) {
            return Ok(None);
        }
        self.read(|c| {
            Ok(c.prepare_cached("SELECT status FROM replication_status WHERE bucket_id = ?1 AND key = ?2 AND version_id = ?3")?
                .query_row(params![bucket_id, key, version_id], |r| r.get(0))
                .optional()?)
        })
    }

    pub(crate) fn load_event_state(&self) -> Result<()> {
        let any = self.read(|c| {
            Ok(c.prepare("SELECT 1 FROM replication_status LIMIT 1")?.exists([])? || c.prepare("SELECT 1 FROM bucket_config WHERE kind = 'replication' LIMIT 1")?.exists([])?)
        })?;
        self.events.has_status.store(any, Ordering::Relaxed);
        Ok(())
    }

    /// Apply a [`REPLICATE`] row: the replica shares the source version's blob (a new
    /// reference, no bytes copied) and keeps its version id, at its place in the destination's
    /// history. One durable transaction also sets both statuses and deletes the row, so a
    /// replica is written once however often this runs. An error means "try again".
    pub async fn replicate(&self, ev: &PendingEvent) -> Result<Replicated> {
        let task: ReplicateTask = serde_json::from_str(&ev.body)?;
        let now = self.now();
        let out = self
            .write(|c| {
                if !remove_event(c, ev.seq, now)? {
                    return Ok(Replicated::Skipped);
                }
                let Some(src) = find_bucket_id(c, &ev.bucket_id)? else { return Ok(Replicated::Skipped) };
                let chain = rows::chain(c, &src.id)?;
                let Some((row, _)) = versioning::find_version(c, &chain, &ev.key, &task.version_id)? else {
                    set_status(c, &src.id, &ev.key, &task.version_id, None)?;
                    return Ok(Replicated::Skipped);
                };
                let failed = |why: String| -> Result<Replicated> {
                    set_status(c, &src.id, &ev.key, &task.version_id, Some("FAILED"))?;
                    let mut ch = Change::new("Replication:OperationFailedReplication", &ev.key);
                    ch.version_id = Some(task.version_id.clone());
                    ch.size = row.blob_id.is_some().then_some(row.size);
                    ch.etag = row.blob_id.is_some().then_some(row.etag.as_str());
                    ch.reason = Some(&why);
                    ch.replication_destination = Some(&task.destination);
                    emit(c, &src, ch, now)?;
                    Ok(Replicated::Failed(why))
                };
                let dest = match find_bucket(c, &task.destination) {
                    Ok(d) => d,
                    Err(StoreError::NoSuchBucket(_)) => return failed(format!("the destination bucket {} doesn't exist", task.destination)),
                    Err(e) => return Err(e),
                };
                if Mode::of(&dest) != Mode::Enabled {
                    return failed(format!("versioning isn't enabled on the destination bucket {}", task.destination));
                }
                let dchain = rows::chain(c, &dest.id)?;
                if versioning::find_version(c, &dchain, &ev.key, &task.version_id)?.is_none() {
                    // Refusals first, before anything changes: a new null version replaces the
                    // destination's, which must be deletable.
                    if row.is_null
                        && let Some((old, _)) = versioning::find_version(c, &dchain, &ev.key, "null")?
                        && let Err(e) = versioning::check_deletable(&old, now, false)
                    {
                        return failed(e.to_string());
                    }
                    let mut replica = row.clone();
                    if dest.object_lock.is_none() {
                        replica.lock = ObjectLock::default();
                    }
                    if task.owner_override {
                        replica.headers.acl = None;
                    }
                    if let Some(blob) = &replica.blob_id {
                        rows::add_ref(c, blob, 1, now)?;
                    }
                    let stored = versioning::restore(c, &dest, &dchain, &ev.key, replica, now)?;
                    let name = if stored.delete_marker { "ObjectRemoved:DeleteMarkerCreated" } else { "ObjectCreated:Put" };
                    let mut ch = Change::version(name, &stored);
                    ch.replicate = None; // replicas never replicate on (no chains, no loops)
                    ch.principal = Some("roll:replication");
                    emit(c, &dest, ch, now)?;
                }
                set_status(c, &dest.id, &ev.key, &task.version_id, Some("REPLICA"))?;
                // COMPLETED once no other destination still waits for this version.
                let others = c
                    .prepare_cached("SELECT body FROM events WHERE bucket_id = ?1 AND key = ?2 AND kind = 'replicate'")?
                    .query_map(params![src.id, ev.key], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
                    .into_iter()
                    .filter_map(|b| serde_json::from_str::<ReplicateTask>(&b).ok())
                    .any(|t| t.version_id == task.version_id);
                if !others {
                    let cur: Option<String> = c
                        .prepare_cached("SELECT status FROM replication_status WHERE bucket_id = ?1 AND key = ?2 AND version_id = ?3")?
                        .query_row(params![src.id, ev.key, task.version_id], |r| r.get(0))
                        .optional()?;
                    if cur.as_deref() != Some("FAILED") {
                        set_status(c, &src.id, &ev.key, &task.version_id, Some("COMPLETED"))?;
                    }
                }
                Ok(Replicated::Completed)
            })
            .await?;
        self.events.has_status.store(true, Ordering::Relaxed);
        Ok(out)
    }

    /// Give up on a [`REPLICATE`] row (it kept failing past the retention window): the source
    /// version is `FAILED`, and the row goes, in one durable write.
    pub async fn abandon_replication(&self, ev: &PendingEvent, why: &str) -> Result<()> {
        let task: ReplicateTask = serde_json::from_str(&ev.body)?;
        let now = self.now();
        self.write(|c| {
            if !remove_event(c, ev.seq, now)? {
                return Ok(());
            }
            set_status(c, &ev.bucket_id, &ev.key, &task.version_id, Some("FAILED"))?;
            if let Some(src) = find_bucket_id(c, &ev.bucket_id)? {
                let mut ch = Change::new("Replication:OperationFailedReplication", &ev.key);
                ch.version_id = Some(task.version_id.clone());
                ch.reason = Some(why);
                ch.replication_destination = Some(&task.destination);
                emit(c, &src, ch, now)?;
            }
            Ok(())
        })
        .await
    }

    /// One step of a [`BACKFILL`] row: queue replication of up to 500 more versions already in
    /// the bucket (those without a replication status), then move its cursor on, or delete it
    /// when done. Returns whether it's done.
    pub async fn backfill(&self, ev: &PendingEvent) -> Result<bool> {
        let mut task: BackfillTask = serde_json::from_str(&ev.body)?;
        let now = self.now();
        let done = self
            .write(|c| {
                if !c.prepare_cached("SELECT 1 FROM events WHERE seq = ?1")?.exists([ev.seq])? {
                    return Ok(true);
                }
                let Some(b) = find_bucket_id(c, &ev.bucket_id)? else {
                    remove_event(c, ev.seq, now)?;
                    return Ok(true);
                };
                let rule = targets(c, &b.id)?.and_then(|(_, r)| r).and_then(|r| r.rules.into_iter().find(|x| x.id == task.rule && x.enabled && x.existing_objects));
                let Some(rule) = rule else {
                    remove_event(c, ev.seq, now)?;
                    return Ok(true);
                };
                let chain = rows::chain(c, &b.id)?;
                let q = VersionsQuery { prefix: rule.prefix().to_string(), key_marker: task.key_marker.clone(), version_id_marker: task.version_id_marker.clone(), max_keys: 500, ..Default::default() };
                let page = versioning::list_versions(c, &chain, &q)?;
                for v in &page.versions {
                    let has: bool = c.prepare_cached("SELECT 1 FROM replication_status WHERE bucket_id = ?1 AND key = ?2 AND version_id = ?3")?.exists(params![b.id, v.key, v.version_id])?;
                    if has {
                        continue;
                    }
                    let Some((row, _)) = versioning::find_version(c, &chain, &v.key, &v.version_id)? else { continue };
                    if !rule.matches(&v.key, &row.headers.tags) || (row.delete_marker && !rule.replicates_markers()) {
                        continue;
                    }
                    let seq = take_seqs(c, 1)?;
                    let t = ReplicateTask { rule: rule.id.clone(), destination: rule.destination.clone(), version_id: v.version_id.clone(), owner_override: rule.owner_override };
                    insert_event(c, seq, &b.id, &v.key, &format!("replicate:{}", rule.destination), REPLICATE, &rows::json(&t)?, now)?;
                    set_status(c, &b.id, &v.key, &v.version_id, Some("PENDING"))?;
                }
                if !page.truncated {
                    remove_event(c, ev.seq, now)?;
                    return Ok(true);
                }
                task.key_marker = page.next_key_marker.clone();
                task.version_id_marker = page.next_version_id_marker.clone();
                c.execute("UPDATE events SET body = ?2 WHERE seq = ?1", params![ev.seq, rows::json(&task)?])?;
                Ok(false)
            })
            .await?;
        self.events.has_status.store(true, Ordering::Relaxed);
        Ok(done)
    }

    // ---- Inventory ----

    /// One page of an inventory report: the bucket's current versions (or, with
    /// `all_versions`, every version and delete marker) under `prefix`, after `after`
    /// (`(key, version id)` of the previous page's last entry). Returns the entries and where
    /// to resume (`None`: done).
    pub fn inventory_page(&self, bucket: &str, all_versions: bool, prefix: &str, after: Option<InventoryCursor>, limit: usize) -> Result<(Vec<InventoryEntry>, Option<InventoryCursor>)> {
        self.read(|c| {
            let b = find_bucket(c, bucket)?;
            let chain = rows::chain(c, &b.id)?;
            let status = |key: &str, vid: &str| -> Result<Option<String>> {
                Ok(c.prepare_cached("SELECT status FROM replication_status WHERE bucket_id = ?1 AND key = ?2 AND version_id = ?3")?
                    .query_row(params![b.id, key, vid], |r| r.get(0))
                    .optional()?)
            };
            let entry = |r: &Row, latest: bool| -> Result<InventoryEntry> {
                Ok(InventoryEntry {
                    key: r.key.clone(),
                    version_id: r.version_id(),
                    is_latest: latest,
                    delete_marker: r.delete_marker,
                    size: r.size,
                    modified_at: r.modified_at,
                    etag: r.etag.clone(),
                    multipart: r.parts > 0,
                    checksum_algorithm: r.checksum.as_ref().map(|x| x.algorithm.clone()),
                    lock: r.lock.clone(),
                    replication_status: status(&r.key, &r.version_id())?,
                })
            };
            let mut out = Vec::new();
            if !all_versions {
                let start = after.as_ref().map(|(k, _)| k.as_str());
                let mut m = rows::Merger::new(c, &chain, prefix, None, start);
                while out.len() < limit {
                    match m.next_entry()? {
                        Some(rows::Entry::Object(r)) => out.push(entry(&r, true)?),
                        Some(rows::Entry::Prefix(_)) => {}
                        None => return Ok((out, None)),
                    }
                }
                let next = out.last().map(|e| (e.key.clone(), e.version_id.clone()));
                return Ok((out, next));
            }
            let (km, vm) = after.map_or((None, None), |(k, v)| (Some(k), Some(v)));
            let q = VersionsQuery { prefix: prefix.to_string(), key_marker: km, version_id_marker: vm, max_keys: limit, ..Default::default() };
            let page = versioning::list_versions(c, &chain, &q)?;
            for v in &page.versions {
                if let Some((r, _)) = versioning::find_version(c, &chain, &v.key, &v.version_id)? {
                    out.push(entry(&r, v.is_latest)?);
                }
            }
            let next = if page.truncated { page.next_key_marker.zip(page.next_version_id_marker) } else { None };
            Ok((out, next))
        })
    }

    // ---- Job state ----

    /// A value kept for a background job (when it last ran, say).
    pub fn job_state(&self, name: &str) -> Result<Option<String>> {
        self.read(|c| Ok(c.query_row("SELECT v FROM state WHERE k = ?1", [format!("job:{name}")], |r| r.get(0)).optional()?))
    }

    /// Keep a value for a background job, durably.
    pub async fn set_job_state(&self, name: &str, value: &str) -> Result<()> {
        self.write(|c| {
            c.execute("INSERT INTO state (k, v) VALUES (?1, ?2) ON CONFLICT (k) DO UPDATE SET v = excluded.v", params![format!("job:{name}"), value])?;
            Ok(())
        })
        .await
    }
}

/// A deleted bucket's replication statuses go with it (its outbox rows are still delivered).
pub(crate) fn drop_bucket(c: &Connection, bucket_id: &str) -> Result<()> {
    c.execute("DELETE FROM replication_status WHERE bucket_id = ?1", [bucket_id])?;
    Ok(())
}

/// Where an inventory listing resumes: `(key, version id)` of the last entry.
pub type InventoryCursor = (String, String);

/// One line of an inventory report.
#[derive(Clone, Debug)]
pub struct InventoryEntry {
    pub key: String,
    pub version_id: String,
    pub is_latest: bool,
    pub delete_marker: bool,
    pub size: u64,
    pub modified_at: i64,
    pub etag: String,
    pub multipart: bool,
    pub checksum_algorithm: Option<String>,
    pub lock: ObjectLock,
    pub replication_status: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_patterns() {
        assert!(event_matches("s3:ObjectCreated:*", "ObjectCreated:Put"));
        assert!(event_matches("s3:ObjectCreated:*", "ObjectCreated:CompleteMultipartUpload"));
        assert!(!event_matches("s3:ObjectCreated:*", "ObjectCreatedX:Put"));
        assert!(event_matches("s3:ObjectRemoved:DeleteMarkerCreated", "ObjectRemoved:DeleteMarkerCreated"));
        assert!(!event_matches("s3:ObjectRemoved:Delete", "ObjectRemoved:DeleteMarkerCreated"));
        assert!(!event_matches("ObjectCreated:*", "ObjectCreated:Put"));
        assert!(event_matches("s3:LifecycleTransition", "LifecycleTransition"));
        assert!(events_overlap("s3:ObjectCreated:*", "s3:ObjectCreated:Put"));
        assert!(!events_overlap("s3:ObjectCreated:Copy", "s3:ObjectCreated:Put"));
        assert!(!events_overlap("s3:ObjectCreated:*", "s3:ObjectRemoved:*"));
    }

    fn target(id: &str, events: &[&str], prefix: Option<&str>, suffix: Option<&str>) -> NotificationTarget {
        NotificationTarget {
            id: id.into(),
            kind: "queue".into(),
            destination: "arn:roll:queues:/:q".into(),
            events: events.iter().map(|e| e.to_string()).collect(),
            prefix: prefix.map(Into::into),
            suffix: suffix.map(Into::into),
        }
    }

    #[test]
    fn notification_rules() {
        let check = |ts: Vec<NotificationTarget>| NotificationConfig { configurations: ts, event_bridge: false }.validate();
        assert!(check(vec![target("a", &["s3:ObjectCreated:*"], Some("img/"), None), target("b", &["s3:ObjectCreated:*"], Some("doc/"), None)]).is_ok());
        assert!(check(vec![target("a", &["s3:ObjectCreated:*"], Some("img/"), None), target("b", &["s3:ObjectCreated:Put"], Some("img/x"), None)]).unwrap_err().contains("overlap"));
        assert!(check(vec![target("a", &["s3:ObjectCreated:*"], None, Some(".jpg")), target("b", &["s3:ObjectCreated:*"], None, Some(".png"))]).is_ok());
        assert!(check(vec![target("a", &["s3:ObjectCreated:*"], None, None), target("b", &["s3:ObjectRemoved:*"], None, None)]).is_ok());
        assert!(check(vec![target("a", &["s3:Nope"], None, None)]).unwrap_err().contains("not supported"));
        assert!(check(vec![target("a", &[], None, None)]).is_err());
        assert!(check(vec![target("a", &["s3:ObjectCreated:*"], None, None), target("a", &["s3:ObjectRemoved:*"], None, None)]).is_err());
        let mut c = NotificationConfig { configurations: vec![target("", &["s3:ObjectCreated:*"], Some(""), None)], event_bridge: false };
        c.validate().unwrap();
        assert!(!c.configurations[0].id.is_empty(), "ids are generated");
        assert_eq!(c.configurations[0].prefix, None, "empty filters are no filter");
        let t = target("a", &["s3:ObjectCreated:*"], Some("img/"), Some(".jpg"));
        assert!(t.matches("ObjectCreated:Put", "img/a.jpg"));
        assert!(!t.matches("ObjectCreated:Put", "img/a.png"));
        assert!(!t.matches("ObjectRemoved:Delete", "img/a.jpg"));
    }

    #[test]
    fn replication_rules() {
        let rule = |id: &str, p: Option<i64>| ReplicationRule { id: id.into(), priority: p, enabled: true, filter: Some(RuleFilter::default()), destination: "dst".into(), ..Default::default() };
        let mut c = ReplicationConfig { role: String::new(), rules: vec![rule("a", Some(1)), rule("b", Some(1))] };
        assert!(c.validate("src").unwrap_err().contains("priority"));
        c.rules[1].priority = Some(2);
        c.validate("src").unwrap();
        assert!(c.validate("dst").unwrap_err().contains("same as the source"));
        let mut tags = BTreeMap::new();
        let r = ReplicationRule { filter: Some(RuleFilter { prefix: Some("a/".into()), tags: vec![("k".into(), "v".into())], and: true }), ..rule("x", None) };
        assert!(!r.matches("a/1", &tags));
        tags.insert("k".into(), "v".into());
        assert!(r.matches("a/1", &tags));
        assert!(!r.matches("b/1", &tags));
        assert!(!r.replicates_markers());
        assert!(ReplicationRule { legacy_prefix: Some(String::new()), ..Default::default() }.replicates_markers());
        assert_eq!(sequencer(255), "0000000000000000FF");
    }
}
