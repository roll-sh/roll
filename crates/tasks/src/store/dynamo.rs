//! DynamoDB store: one on-demand table (scales with traffic, costs nothing idle, multi-AZ),
//! plus S3 for bodies too large to keep inline.
//!
//! Layout (`pk`, `sk`):
//!
//! | item            | pk                     | sk                   |
//! | --------------- | ---------------------- | -------------------- |
//! | message         | `m#<id>`               | `m`                  |
//! | body            | `m#<id>`               | `b`                  |
//! | attempt         | `m#<id>`               | `a#<n>#<ulid>`       |
//! | due index       | `d#<lane>`             | `<due ms, 15>#<id>`  |
//! | ordering group  | `g#<group>`            | `<id>`               |
//! | kv record       | `k#<key>`              | `k`                  |
//! | document        | `doc#<kind>`           | `<id>`               |
//! | run / step      | `r#<id>`               | `r` / `s#<step>`     |
//! | event           | `e#<id>`               | `e`                  |
//! | wait            | `w#<event>#<key>`      | `<id>`               |
//! | usage           | `u`                    | `<day, 10>`          |
//!
//! Four sparse, overloaded GSIs (`g1`…`g4`, sort key = id, so newest first) list messages, runs
//! and events; their partition keys are sharded so no list becomes a hot partition.
//!
//! Due-index entries carry the version the message has when the entry is valid. Writers add
//! the new entry before the conditional message update and remove the old one after, and
//! readers drop entries whose message has moved past them, so a crash at any point leaves at
//! worst a stale entry that heals itself — never a message nobody will find.

use super::*;
use crate::ids::ulid;
use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue, BillingMode, DeleteRequest, GlobalSecondaryIndex, KeySchemaElement, KeyType, KeysAndAttributes,
    Projection, ProjectionType, ReturnValue, ScalarAttributeType, Select, TimeToLiveSpecification, WriteRequest,
};
use object_store::{ObjectStore, ObjectStoreExt};
use std::collections::HashMap;
use std::sync::Arc;

type Item = HashMap<String, AttributeValue>;

#[derive(Clone, Debug)]
pub struct DynamoConfig {
    pub table: String,
    /// DynamoDB Local, LocalStack …; default is AWS.
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub shards: u32,
    /// S3 bucket for large bodies; without one, bodies must fit inline.
    pub bucket: Option<String>,
    pub s3_endpoint: Option<String>,
    /// Bodies up to this size live in DynamoDB.
    pub inline_max: usize,
    /// Create the table (and TTL) if it doesn't exist.
    pub create_table: bool,
}

pub struct DynamoStore {
    db: Client,
    table: String,
    shards: u32,
    s3: Option<(Arc<dyn ObjectStore>, String)>,
    inline_max: usize,
}

const LIST_SHARDS: u32 = 16;
const QUEUE_SHARDS: u32 = 8;
const OWNER_SHARDS: u32 = 4;
/// Statically-known attributes projected into the GSIs for filtering.
const PROJECTED: [&str; 8] = ["k", "q", "tp", "sch", "run", "lb", "url", "st0"];
/// An index entry ahead of its message for this long is an orphan from a crashed writer.
const ORPHAN_MS: i64 = 5 * 60_000;
/// Bodies expire at least this long after their message was created (more if it lives longer),
/// so finishing a message rarely needs a write to its body: DynamoDB TTL deletes are free and a
/// few weeks of storage costs less than one write.
const BODY_GRACE_MS: i64 = 60 * 86_400_000;
/// Hard cap on days kept for attempts and steps that outlive their owners' expiry bookkeeping.
const ATTEMPT_TTL_S: i64 = 30 * 86_400;

fn s(v: impl Into<String>) -> AttributeValue {
    AttributeValue::S(v.into())
}
fn n(v: i64) -> AttributeValue {
    AttributeValue::N(v.to_string())
}
fn get_s<'a>(item: &'a Item, k: &str) -> Option<&'a str> {
    item.get(k).and_then(|v| v.as_s().ok()).map(String::as_str)
}
fn get_n(item: &Item, k: &str) -> Option<i64> {
    item.get(k).and_then(|v| v.as_n().ok()).and_then(|s| s.parse().ok())
}
fn key(pk: impl Into<String>, sk: impl Into<String>) -> Item {
    HashMap::from([("pk".to_string(), s(pk)), ("sk".to_string(), s(sk))])
}
fn msg_pk(id: &str) -> String {
    format!("m#{id}")
}
fn index_sk(due: Ms, id: &str) -> String {
    format!("{:015}#{id}", due.max(0))
}
fn ttl_s(ms: Ms) -> AttributeValue {
    n(ms.div_euclid(1000))
}

fn cond_failed<E: ProvideErrorMetadata, R>(e: &SdkError<E, R>) -> bool {
    e.as_service_error().and_then(|s| s.code()) == Some("ConditionalCheckFailedException")
}

fn is_user(kind: &str) -> bool {
    matches!(kind, "http" | "pull")
}

fn decode<T: DeserializeOwned>(item: &Item) -> Result<T> {
    Ok(serde_json::from_str(get_s(item, "d").ok_or_else(|| anyhow::anyhow!("item without data"))?)?)
}

/// `(pk, sk)` of a message's due-index entry in its current state.
fn index_of(m: &Message) -> Option<(String, String)> {
    matches!(m.state, MsgState::Pending | MsgState::Active).then(|| (format!("d#{}", m.lane), index_sk(m.due_at, &m.id)))
}

impl DynamoStore {
    pub async fn connect(c: DynamoConfig) -> Result<Self> {
        let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
            .retry_config(aws_config::retry::RetryConfig::standard().with_max_attempts(10));
        if let Some(r) = &c.region {
            loader = loader.region(aws_config::Region::new(r.clone()));
        }
        if let Some(e) = &c.endpoint {
            loader = loader.endpoint_url(e);
            if std::env::var("AWS_ACCESS_KEY_ID").is_err() {
                loader = loader.credentials_provider(aws_sdk_dynamodb::config::Credentials::new("local", "local", None, None, "roll"));
            }
        }
        let db = Client::new(&loader.load().await);
        let s3 = match &c.bucket {
            Some(bucket) => {
                let mut b = object_store::aws::AmazonS3Builder::from_env().with_bucket_name(bucket);
                if let Some(r) = &c.region {
                    b = b.with_region(r);
                }
                if let Some(e) = &c.s3_endpoint {
                    b = b.with_endpoint(e).with_allow_http(e.starts_with("http://")).with_virtual_hosted_style_request(false);
                }
                Some((Arc::new(b.build()?) as Arc<dyn ObjectStore>, bucket.clone()))
            }
            None => None,
        };
        let store = Self { db, table: c.table, shards: c.shards.max(1), s3, inline_max: c.inline_max.min(350 << 10) };
        if c.create_table {
            store.create_table().await?;
        }
        Ok(store)
    }

    /// Create the table, its indexes and TTL (idempotent).
    pub async fn create_table(&self) -> Result<()> {
        if self.db.describe_table().table_name(&self.table).send().await.is_ok() {
            return Ok(());
        }
        let attr = |name: &str| AttributeDefinition::builder().attribute_name(name).attribute_type(ScalarAttributeType::S).build();
        let ks = |name: &str, t: KeyType| KeySchemaElement::builder().attribute_name(name).key_type(t).build();
        let mut req = self
            .db
            .create_table()
            .table_name(&self.table)
            .billing_mode(BillingMode::PayPerRequest)
            .key_schema(ks("pk", KeyType::Hash)?)
            .key_schema(ks("sk", KeyType::Range)?)
            .attribute_definitions(attr("pk")?)
            .attribute_definitions(attr("sk")?);
        for g in ["g1", "g2", "g3", "g4"] {
            req = req.attribute_definitions(attr(&format!("{g}pk"))?).attribute_definitions(attr(&format!("{g}sk"))?);
            let mut projection = Projection::builder().projection_type(ProjectionType::Include);
            for a in PROJECTED {
                projection = projection.non_key_attributes(a);
            }
            req = req.global_secondary_indexes(
                GlobalSecondaryIndex::builder()
                    .index_name(format!("{g}ix"))
                    .key_schema(ks(&format!("{g}pk"), KeyType::Hash)?)
                    .key_schema(ks(&format!("{g}sk"), KeyType::Range)?)
                    .projection(projection.build())
                    .build()?,
            );
        }
        match req.send().await {
            Ok(_) => {}
            Err(e) if e.as_service_error().and_then(|s| s.code()) == Some("ResourceInUseException") => {}
            Err(e) => return Err(anyhow::anyhow!("creating table {}: {:?}", self.table, e.into_service_error())),
        }
        for _ in 0..120 {
            let d = self.db.describe_table().table_name(&self.table).send().await?;
            if d.table().and_then(|t| t.table_status()).map(|s| s.as_str() == "ACTIVE").unwrap_or(false) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        let _ = self
            .db
            .update_time_to_live()
            .table_name(&self.table)
            .time_to_live_specification(TimeToLiveSpecification::builder().enabled(true).attribute_name("ttl").build()?)
            .send()
            .await;
        Ok(())
    }

    async fn get(&self, k: Item, consistent: bool) -> Result<Option<Item>> {
        let out = self.db.get_item().table_name(&self.table).set_key(Some(k)).consistent_read(consistent).send().await?;
        Ok(out.item)
    }

    async fn delete(&self, k: Item) -> Result<()> {
        self.db.delete_item().table_name(&self.table).set_key(Some(k)).send().await?;
        Ok(())
    }

    /// Fetch many items by key (any order; missing ones are skipped).
    async fn batch_get(&self, keys: Vec<Item>, consistent: bool) -> Result<Vec<Item>> {
        let mut out = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(100) {
            let mut pending = Some(KeysAndAttributes::builder().set_keys(Some(chunk.to_vec())).consistent_read(consistent).build()?);
            let mut tries = 0;
            while let Some(req) = pending.take() {
                let resp = self.db.batch_get_item().request_items(&self.table, req).send().await?;
                if let Some(mut r) = resp.responses {
                    out.extend(r.remove(&self.table).unwrap_or_default());
                }
                if let Some(mut u) = resp.unprocessed_keys
                    && let Some(left) = u.remove(&self.table)
                    && !left.keys().is_empty()
                {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(20 << tries.min(6))).await;
                    pending = Some(left);
                }
            }
        }
        Ok(out)
    }

    async fn batch_delete(&self, keys: Vec<Item>) -> Result<()> {
        for chunk in keys.chunks(25) {
            let mut reqs: Vec<WriteRequest> =
                chunk.iter().map(|k| WriteRequest::builder().delete_request(DeleteRequest::builder().set_key(Some(k.clone())).build().unwrap()).build()).collect();
            let mut tries = 0;
            while !reqs.is_empty() {
                let resp = self.db.batch_write_item().request_items(&self.table, reqs).send().await?;
                reqs = resp.unprocessed_items.and_then(|mut u| u.remove(&self.table)).unwrap_or_default();
                if !reqs.is_empty() {
                    tries += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(20 << tries.min(6))).await;
                }
            }
        }
        Ok(())
    }

    /// All items of a partition (optionally a sort-key prefix).
    async fn query_all(&self, pk: &str, prefix: Option<&str>, consistent: bool) -> Result<Vec<Item>> {
        let mut out = Vec::new();
        let mut start: Option<Item> = None;
        loop {
            let mut q = self
                .db
                .query()
                .table_name(&self.table)
                .consistent_read(consistent)
                .expression_attribute_values(":pk", s(pk))
                .set_exclusive_start_key(start.take());
            q = match prefix {
                Some(p) => q.key_condition_expression("pk = :pk AND begins_with(sk, :p)").expression_attribute_values(":p", s(p)),
                None => q.key_condition_expression("pk = :pk"),
            };
            let resp = q.send().await?;
            out.extend(resp.items.unwrap_or_default());
            match resp.last_evaluated_key {
                Some(k) if !k.is_empty() => start = Some(k),
                _ => return Ok(out),
            }
        }
    }

    /// Newest-first ids from GSI `index` across partitions `pks`, below `cursor`, matching an
    /// optional filter on projected attributes. Returns up to `want` ids.
    async fn list_ids(
        &self,
        index: &str,
        pks: Vec<String>,
        cursor: Option<&str>,
        want: usize,
        filter: &Option<Filter>,
    ) -> Result<Vec<String>> {
        let futs = pks.into_iter().map(|pk| {
            let filter = filter.clone();
            async move {
                let mut ids: Vec<String> = Vec::new();
                let mut start: Option<Item> = None;
                for _ in 0..20 {
                    let mut q = self
                        .db
                        .query()
                        .table_name(&self.table)
                        .index_name(format!("{index}ix"))
                        .scan_index_forward(false)
                        .limit((want as i32).max(10))
                        .expression_attribute_values(":pk", s(&pk))
                        .set_exclusive_start_key(start.take());
                    q = match cursor {
                        Some(c) => q
                            .key_condition_expression(format!("{index}pk = :pk AND {index}sk < :cur"))
                            .expression_attribute_values(":cur", s(c)),
                        None => q.key_condition_expression(format!("{index}pk = :pk")),
                    };
                    if let Some((expr, names, vals)) = &filter {
                        q = q.filter_expression(expr);
                        for (k, v) in names {
                            q = q.expression_attribute_names(k, v);
                        }
                        for (k, v) in vals {
                            q = q.expression_attribute_values(k, v.clone());
                        }
                    }
                    let resp = q.send().await?;
                    for item in resp.items.unwrap_or_default() {
                        if let Some(id) = get_s(&item, &format!("{index}sk")) {
                            ids.push(id.to_string());
                        }
                    }
                    match resp.last_evaluated_key {
                        Some(k) if !k.is_empty() && ids.len() < want => start = Some(k),
                        _ => break,
                    }
                }
                Ok::<_, anyhow::Error>(ids)
            }
        });
        let mut all: Vec<String> = Vec::new();
        for r in futures::future::join_all(futs).await {
            all.extend(r?);
        }
        all.sort_unstable_by(|a, b| b.cmp(a));
        all.dedup();
        all.truncate(want);
        Ok(all)
    }

    async fn put_body(&self, id: &str, body: &[u8], expires_at: Ms) -> Result<()> {
        let mut item = key(msg_pk(id), "b");
        item.insert("ttl".into(), ttl_s(expires_at));
        if body.len() > self.inline_max {
            let (s3, _) = self
                .s3
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("body of {} bytes is over the inline limit and no S3 bucket is configured", body.len()))?;
            let path = object_store::path::Path::from(format!("bodies/{id}"));
            s3.put(&path, bytes::Bytes::copy_from_slice(body).into()).await?;
            item.insert("s3".into(), s(path.to_string()));
        } else {
            item.insert("b".into(), AttributeValue::B(aws_sdk_dynamodb::primitives::Blob::new(body)));
        }
        self.db.put_item().table_name(&self.table).set_item(Some(item)).send().await?;
        Ok(())
    }

    fn static_attrs(m: &Message, item: &mut Item) {
        let kind = m.target.kind();
        item.insert("k".into(), s(kind));
        let mut opt = |k: &str, v: &Option<String>| {
            if let Some(v) = v {
                item.insert(k.into(), s(v.clone()));
            }
        };
        opt("q", &m.queue);
        opt("tp", &m.topic);
        opt("sch", &m.schedule_id);
        opt("run", &m.run_id);
        opt("lb", &m.label);
        opt("url", &m.target.url().map(String::from));
        // User messages and internal ones (ticks, timers, invocations) list separately, so the
        // common listing never wades through internal traffic.
        let list = if is_user(kind) { "ml" } else { "mi" };
        item.insert("g1pk".into(), s(format!("{list}#{}", shard_of(&m.id, LIST_SHARDS))));
        item.insert("g1sk".into(), s(m.id.clone()));
        if let Some(q) = &m.queue {
            item.insert("g3pk".into(), s(format!("mq#{q}#{}", shard_of(&m.id, QUEUE_SHARDS))));
            item.insert("g3sk".into(), s(m.id.clone()));
        }
        let owner = m.run_id.as_ref().map(|r| format!("mo#r#{r}")).or_else(|| m.schedule_id.as_ref().map(|sc| format!("mo#s#{sc}")));
        if let Some(o) = owner {
            item.insert("g2pk".into(), s(o));
            item.insert("g2sk".into(), s(m.id.clone()));
        }
    }

    async fn put_index(&self, m: &Message, version: u64) -> Result<Option<(String, String)>> {
        let Some((pk, sk)) = index_of(m) else { return Ok(None) };
        let mut item = key(&pk, &sk);
        item.insert("id".into(), s(m.id.clone()));
        item.insert("v".into(), n(version as i64));
        item.insert("st".into(), s(if m.state == MsgState::Active { "a" } else { "p" }));
        item.insert("at".into(), n(now_ms()));
        self.db.put_item().table_name(&self.table).set_item(Some(item)).send().await?;
        Ok(Some((pk, sk)))
    }

    fn message_item(m: &Message, version: u64) -> Result<Item> {
        let stored = Message { version, ..m.clone() };
        let mut item = key(msg_pk(&m.id), "m");
        item.insert("d".into(), s(serde_json::to_string(&stored)?));
        item.insert("v".into(), n(version as i64));
        item.insert("st".into(), s(m.state.as_str()));
        if let Some((pk, sk)) = index_of(m) {
            item.insert("ix".into(), s(format!("{pk}\n{sk}")));
        }
        if m.state == MsgState::Failed {
            item.insert("g4pk".into(), s(format!("md#{}", shard_of(&m.id, LIST_SHARDS))));
            item.insert("g4sk".into(), s(m.id.clone()));
        }
        if let Some(e) = m.expires_at {
            item.insert("ttl".into(), ttl_s(e));
        }
        Ok(item)
    }

    /// Messages for the given ids, in that order.
    async fn messages_by_id(&self, ids: &[String], consistent: bool) -> Result<Vec<Message>> {
        let items = self.batch_get(ids.iter().map(|id| key(msg_pk(id), "m")).collect(), consistent).await?;
        let mut by_id: HashMap<String, Message> = HashMap::new();
        for item in &items {
            let mut m: Message = decode(item)?;
            m.version = get_n(item, "v").unwrap_or(0) as u64;
            by_id.insert(m.id.clone(), m);
        }
        Ok(ids.iter().filter_map(|id| by_id.remove(id)).collect())
    }

    async fn group_entry_delete(&self, m: &Message) {
        if let Some(g) = &m.group {
            let _ = self.delete(key(format!("g#{g}"), m.id.clone())).await;
        }
    }
}

type Filter = (String, HashMap<String, String>, HashMap<String, AttributeValue>);

fn message_filter(q: &MessageQuery, skip: &[&str]) -> Option<Filter> {
    let mut parts = Vec::new();
    let mut names = HashMap::new();
    let mut vals = HashMap::new();
    let mut eq = |attr: &str, v: &Option<String>| {
        if let Some(v) = v
            && !skip.contains(&attr)
        {
            // Placeholders throughout: `url` is a reserved word.
            parts.push(format!("#{attr} = :{attr}"));
            names.insert(format!("#{attr}"), attr.to_string());
            vals.insert(format!(":{attr}"), s(v.clone()));
        }
    };
    eq("q", &q.queue);
    eq("tp", &q.topic);
    eq("sch", &q.schedule_id);
    eq("run", &q.run_id);
    eq("lb", &q.label);
    eq("url", &q.url);
    if q.user_only {
        parts.push("(#k = :k1 OR #k = :k2)".into());
        names.insert("#k".into(), "k".into());
        vals.insert(":k1".into(), s("http"));
        vals.insert(":k2".into(), s("pull"));
    }
    (!parts.is_empty()).then(|| (parts.join(" AND "), names, vals))
}

#[async_trait]
impl Store for DynamoStore {
    fn describe(&self) -> String {
        match &self.s3 {
            Some((_, b)) => format!("dynamodb {} + s3://{b}", self.table),
            None => format!("dynamodb {}", self.table),
        }
    }

    fn shards(&self) -> u32 {
        self.shards
    }

    async fn msg_insert(&self, m: &Message, body: &[u8]) -> Result<()> {
        self.put_body(&m.id, body, m.due_at.max(m.created_at) + BODY_GRACE_MS).await?;
        if let Some(g) = &m.group {
            let mut item = key(format!("g#{g}"), m.id.clone());
            item.insert("at".into(), n(now_ms()));
            self.db.put_item().table_name(&self.table).set_item(Some(item)).send().await?;
        }
        self.put_index(m, 1).await?;
        let mut item = Self::message_item(m, 1)?;
        Self::static_attrs(m, &mut item);
        self.db
            .put_item()
            .table_name(&self.table)
            .set_item(Some(item))
            .condition_expression("attribute_not_exists(pk)")
            .send()
            .await
            .map_err(|e| anyhow::anyhow!("inserting message {}: {}", m.id, e.into_service_error()))?;
        Ok(())
    }

    async fn msg_get(&self, id: &str) -> Result<Option<Message>> {
        Ok(match self.get(key(msg_pk(id), "m"), true).await? {
            Some(item) => {
                let mut m: Message = decode(&item)?;
                m.version = get_n(&item, "v").unwrap_or(0) as u64;
                Some(m)
            }
            None => None,
        })
    }

    async fn msg_body(&self, id: &str) -> Result<Option<Vec<u8>>> {
        let Some(item) = self.get(key(msg_pk(id), "b"), false).await? else { return Ok(None) };
        if let Some(AttributeValue::B(b)) = item.get("b") {
            return Ok(Some(b.as_ref().to_vec()));
        }
        if let (Some(path), Some((s3, _))) = (get_s(&item, "s3"), &self.s3) {
            let got = s3.get(&object_store::path::Path::from(path)).await?;
            return Ok(Some(got.bytes().await?.to_vec()));
        }
        Ok(Some(Vec::new()))
    }

    async fn msg_update(&self, m: &Message, expect: u64) -> Result<bool> {
        let new_version = expect + 1;
        let new_ix = self.put_index(m, new_version).await?;
        let mut item = Self::message_item(m, new_version)?;
        let mut sets = vec!["d = :d", "v = :v", "st = :st"];
        let mut removes = Vec::new();
        let mut vals: HashMap<String, AttributeValue> = HashMap::from([
            (":d".to_string(), item.remove("d").unwrap()),
            (":v".to_string(), item.remove("v").unwrap()),
            (":st".to_string(), item.remove("st").unwrap()),
            (":expect".to_string(), n(expect as i64)),
        ]);
        match item.remove("ix") {
            Some(ix) => {
                sets.push("ix = :ix");
                vals.insert(":ix".into(), ix);
            }
            None => removes.push("ix"),
        }
        match (item.remove("g4pk"), item.remove("g4sk")) {
            (Some(p), Some(sk)) => {
                sets.push("g4pk = :g4pk");
                sets.push("g4sk = :g4sk");
                vals.insert(":g4pk".into(), p);
                vals.insert(":g4sk".into(), sk);
            }
            _ => {
                removes.push("g4pk");
                removes.push("g4sk");
            }
        }
        match item.remove("ttl") {
            Some(t) => {
                sets.push("#ttl = :ttl");
                vals.insert(":ttl".into(), t);
            }
            None => removes.push("#ttl"),
        }
        if let Some(lb) = &m.label {
            sets.push("lb = :lb");
            vals.insert(":lb".into(), s(lb.clone()));
        }
        let mut expr = format!("SET {}", sets.join(", "));
        if !removes.is_empty() {
            expr.push_str(&format!(" REMOVE {}", removes.join(", ")));
        }
        let result = self
            .db
            .update_item()
            .table_name(&self.table)
            .set_key(Some(key(msg_pk(&m.id), "m")))
            .update_expression(expr)
            .condition_expression("v = :expect")
            .expression_attribute_names("#ttl", "ttl")
            .set_expression_attribute_values(Some(vals))
            .return_values(ReturnValue::UpdatedOld)
            .send()
            .await;
        let old = match result {
            Ok(out) => out.attributes.unwrap_or_default(),
            Err(e) if cond_failed(&e) => {
                // Lost the race: drop our entry unless it's the winner's too.
                if let Some((pk, sk)) = new_ix {
                    let current = self.msg_get(&m.id).await?;
                    let keep = current.as_ref().is_some_and(|c| c.version == new_version && index_of(c).as_ref() == Some(&(pk.clone(), sk.clone())));
                    if !keep {
                        let _ = self.delete(key(pk, sk)).await;
                    }
                }
                return Ok(false);
            }
            Err(e) => return Err(anyhow::anyhow!("updating message {}: {}", m.id, e.into_service_error())),
        };
        if let Some(old_ix) = get_s(&old, "ix")
            && let Some((pk, sk)) = old_ix.split_once('\n')
            && new_ix.as_ref().is_none_or(|(np, ns)| np != pk || ns != sk)
        {
            let _ = self.delete(key(pk, sk)).await;
        }
        let was_final = get_s(&old, "st").and_then(MsgState::parse).is_some_and(|s| s.is_final());
        if m.state.is_final() {
            if !was_final {
                self.group_entry_delete(m).await;
            }
            // The body is kept until at least created_at + BODY_GRACE_MS; extend it only if the
            // message outlives that.
            if let Some(e) = m.expires_at
                && e > m.created_at + BODY_GRACE_MS
            {
                let _ = self
                    .db
                    .update_item()
                    .table_name(&self.table)
                    .set_key(Some(key(msg_pk(&m.id), "b")))
                    .update_expression("SET #ttl = :ttl")
                    .expression_attribute_names("#ttl", "ttl")
                    .expression_attribute_values(":ttl", ttl_s(e))
                    .send()
                    .await;
            }
        } else if was_final {
            // Back from the dead-letter queue: keep the body a grace period past its new due
            // time (still at least created_at + BODY_GRACE_MS), rejoin the group.
            let _ = self
                .db
                .update_item()
                .table_name(&self.table)
                .set_key(Some(key(msg_pk(&m.id), "b")))
                .update_expression("SET #ttl = :ttl")
                .expression_attribute_names("#ttl", "ttl")
                .expression_attribute_values(":ttl", ttl_s(m.due_at.max(now_ms()) + BODY_GRACE_MS))
                .send()
                .await;
            if let Some(g) = &m.group {
                let mut item = key(format!("g#{g}"), m.id.clone());
                item.insert("at".into(), n(now_ms()));
                self.db.put_item().table_name(&self.table).set_item(Some(item)).send().await?;
            }
        }
        Ok(true)
    }

    async fn msg_delete(&self, id: &str) -> Result<()> {
        if let Some(m) = self.msg_get(id).await? {
            if let Some((pk, sk)) = index_of(&m) {
                let _ = self.delete(key(pk, sk)).await;
            }
            self.group_entry_delete(&m).await;
        }
        if let Some(item) = self.get(key(msg_pk(id), "b"), false).await?
            && let (Some(path), Some((s3, _))) = (get_s(&item, "s3"), &self.s3)
        {
            let _ = s3.delete(&object_store::path::Path::from(path)).await;
        }
        let all = self.query_all(&msg_pk(id), None, true).await?;
        let keys = all.iter().filter_map(|i| Some(key(get_s(i, "pk")?, get_s(i, "sk")?))).collect();
        self.batch_delete(keys).await
    }

    async fn msg_due(&self, lane: &str, now: Ms, limit: usize) -> Result<Due> {
        let pk = format!("d#{lane}");
        let resp = self
            .db
            .query()
            .table_name(&self.table)
            .key_condition_expression("pk = :pk AND sk <= :now")
            .expression_attribute_values(":pk", s(&pk))
            .expression_attribute_values(":now", s(format!("{:015}#~", now.max(0))))
            .limit(limit.clamp(1, 100) as i32)
            .send()
            .await?;
        let entries = resp.items.unwrap_or_default();
        let mut due = Due::default();
        if !entries.is_empty() {
            let ids: Vec<String> = entries.iter().filter_map(|e| get_s(e, "id").map(String::from)).collect();
            let msgs: HashMap<String, Message> = self.messages_by_id(&ids, true).await?.into_iter().map(|m| (m.id.clone(), m)).collect();
            let mut stale = Vec::new();
            for e in &entries {
                let (Some(id), Some(sk)) = (get_s(e, "id"), get_s(e, "sk")) else { continue };
                let v = get_n(e, "v").unwrap_or(0) as u64;
                let at = get_n(e, "at").unwrap_or(0);
                match msgs.get(id) {
                    None if now - at > ORPHAN_MS => stale.push(key(&pk, sk)),
                    None => {}
                    Some(m) if m.version < v => {
                        // Its writer is mid-update (or crashed long ago).
                        if now - at > ORPHAN_MS {
                            stale.push(key(&pk, sk));
                        }
                    }
                    Some(m) => {
                        let current = index_of(m);
                        if current.as_ref().is_some_and(|(p, s2)| p == &pk && s2 == sk) {
                            due.ids.push(id.to_string());
                        } else if m.version >= v {
                            stale.push(key(&pk, sk));
                        }
                    }
                }
            }
            if !stale.is_empty() {
                let _ = self.batch_delete(stale).await;
            }
        }
        if due.ids.len() < limit {
            let next = self
                .db
                .query()
                .table_name(&self.table)
                .key_condition_expression("pk = :pk AND sk > :now")
                .expression_attribute_values(":pk", s(&pk))
                .expression_attribute_values(":now", s(format!("{:015}#~", now.max(0))))
                .limit(1)
                .send()
                .await?;
            due.next_at = next.items.unwrap_or_default().first().and_then(|e| get_s(e, "sk")).and_then(|sk| sk.split('#').next()).and_then(|t| t.parse().ok());
        }
        Ok(due)
    }

    async fn msg_group_head(&self, group: &str) -> Result<Option<String>> {
        let pk = format!("g#{group}");
        for _ in 0..20 {
            let resp = self
                .db
                .query()
                .table_name(&self.table)
                .key_condition_expression("pk = :pk")
                .expression_attribute_values(":pk", s(&pk))
                .consistent_read(true)
                .limit(1)
                .send()
                .await?;
            let Some(entry) = resp.items.unwrap_or_default().into_iter().next() else { return Ok(None) };
            let id = get_s(&entry, "sk").unwrap_or_default().to_string();
            match self.msg_get(&id).await? {
                Some(m) if !m.state.is_final() => return Ok(Some(id)),
                Some(_) => self.delete(key(&pk, &id)).await?,
                // Inserted moments ago and not visible yet, or an orphan.
                None if now_ms() - get_n(&entry, "at").unwrap_or(0) < ORPHAN_MS => return Ok(Some(id)),
                None => self.delete(key(&pk, &id)).await?,
            }
        }
        Ok(None)
    }

    async fn msg_list(&self, q: &MessageQuery) -> Result<Page<Message>> {
        let limit = q.limit.clamp(1, 1000);
        let (index, pks, skip): (&str, Vec<String>, Vec<&str>) = if q.state == Some(MsgState::Failed) {
            ("g4", (0..LIST_SHARDS).map(|i| format!("md#{i}")).collect(), vec![])
        } else if let Some(r) = &q.run_id {
            ("g2", vec![format!("mo#r#{r}")], vec!["run"])
        } else if let Some(sc) = &q.schedule_id {
            ("g2", vec![format!("mo#s#{sc}")], vec!["sch"])
        } else if let Some(qn) = &q.queue {
            ("g3", (0..QUEUE_SHARDS).map(|i| format!("mq#{qn}#{i}")).collect(), vec!["q"])
        } else if q.user_only {
            ("g1", (0..LIST_SHARDS).map(|i| format!("ml#{i}")).collect(), vec![])
        } else {
            ("g1", (0..LIST_SHARDS).flat_map(|i| [format!("ml#{i}"), format!("mi#{i}")]).collect(), vec![])
        };
        let filter = message_filter(q, &skip);
        let mut items: Vec<Message> = Vec::new();
        let mut cursor = q.cursor.clone();
        let mut exhausted = false;
        for _ in 0..10 {
            let want = limit + 1 - items.len();
            let ids = self.list_ids(index, pks.clone(), cursor.as_deref(), want.max(20), &filter).await?;
            if ids.len() < want.max(20) {
                exhausted = true;
            }
            let Some(last) = ids.last().cloned() else { break };
            for m in self.messages_by_id(&ids, false).await? {
                let state_ok = match q.state {
                    Some(MsgState::Pending | MsgState::Active) => matches!(m.state, MsgState::Pending | MsgState::Active),
                    Some(st) => m.state == st,
                    None => true,
                };
                if state_ok && items.len() <= limit {
                    items.push(m);
                }
            }
            cursor = Some(last);
            if items.len() > limit || exhausted {
                break;
            }
        }
        let cursor = if items.len() > limit {
            items.truncate(limit);
            items.last().map(|m| m.id.clone())
        } else if !exhausted {
            cursor
        } else {
            None
        };
        Ok(Page { items, cursor })
    }

    async fn lane_depth(&self, lane: &str, now: Ms) -> Result<(u64, u64, u64)> {
        let pk = format!("d#{lane}");
        let border = format!("{:015}#~", now.max(0));
        let count = |cond: &'static str, st: &'static str, filter_due: bool| {
            let pk = pk.clone();
            let border = border.clone();
            async move {
                let mut total = 0u64;
                let mut start: Option<Item> = None;
                for _ in 0..5 {
                    let mut q = self
                        .db
                        .query()
                        .table_name(&self.table)
                        .select(Select::Count)
                        .key_condition_expression(cond)
                        .expression_attribute_values(":pk", s(&pk))
                        .filter_expression("st = :st")
                        .expression_attribute_values(":st", s(st))
                        .set_exclusive_start_key(start.take());
                    if filter_due {
                        q = q.expression_attribute_values(":b", s(&border));
                    }
                    let resp = q.send().await?;
                    total += resp.count as u64;
                    match resp.last_evaluated_key {
                        Some(k) if !k.is_empty() => start = Some(k),
                        _ => break,
                    }
                }
                Ok::<u64, anyhow::Error>(total)
            }
        };
        let (ready, scheduled, active) = tokio::try_join!(
            count("pk = :pk AND sk <= :b", "p", true),
            count("pk = :pk AND sk > :b", "p", true),
            count("pk = :pk", "a", false),
        )?;
        Ok((ready, scheduled, active))
    }

    async fn attempt_add(&self, a: &Attempt) -> Result<()> {
        let mut item = key(msg_pk(&a.message_id), format!("a#{:06}#{}", a.n, ulid()));
        item.insert("d".into(), s(serde_json::to_string(a)?));
        item.insert("ttl".into(), n(a.at / 1000 + ATTEMPT_TTL_S));
        self.db.put_item().table_name(&self.table).set_item(Some(item)).send().await?;
        Ok(())
    }

    async fn attempts(&self, message_id: &str) -> Result<Vec<Attempt>> {
        self.query_all(&msg_pk(message_id), Some("a#"), true).await?.iter().map(decode).collect()
    }

    async fn kv_get(&self, k: &str) -> Result<Option<Versioned<Value>>> {
        let Some(item) = self.get(key(format!("k#{k}"), "k"), true).await? else { return Ok(None) };
        if get_n(&item, "exp").is_some_and(|e| e <= now_ms()) {
            return Ok(None);
        }
        Ok(Some(Versioned { value: serde_json::from_str(get_s(&item, "val").unwrap_or("null"))?, version: get_n(&item, "v").unwrap_or(0) as u64 }))
    }

    async fn kv_put(&self, k: &str, value: &Value, expect: Expect, expires_at: Option<Ms>) -> Result<bool> {
        let now = now_ms();
        let mut req = self
            .db
            .update_item()
            .table_name(&self.table)
            .set_key(Some(key(format!("k#{k}"), "k")))
            .expression_attribute_values(":val", s(serde_json::to_string(value)?))
            .expression_attribute_values(":one", n(1))
            .expression_attribute_values(":zero", n(0));
        req = match expires_at {
            Some(e) => req
                .update_expression("SET val = :val, v = if_not_exists(v, :zero) + :one, exp = :exp, #ttl = :ttl")
                .expression_attribute_names("#ttl", "ttl")
                .expression_attribute_values(":exp", n(e))
                .expression_attribute_values(":ttl", ttl_s(e + 60_000)),
            None => req
                .update_expression("SET val = :val, v = if_not_exists(v, :zero) + :one REMOVE exp, #ttl")
                .expression_attribute_names("#ttl", "ttl"),
        };
        req = match expect {
            Expect::Any => req,
            Expect::Absent => req.condition_expression("attribute_not_exists(pk) OR exp <= :now").expression_attribute_values(":now", n(now)),
            Expect::Version(ver) => req
                .condition_expression("v = :expect AND (attribute_not_exists(exp) OR exp > :now)")
                .expression_attribute_values(":expect", n(ver as i64))
                .expression_attribute_values(":now", n(now)),
        };
        match req.send().await {
            Ok(_) => Ok(true),
            Err(e) if cond_failed(&e) => Ok(false),
            Err(e) => Err(anyhow::anyhow!("kv {k}: {}", e.into_service_error())),
        }
    }

    async fn kv_delete(&self, k: &str, expect: Expect) -> Result<bool> {
        let mut req = self.db.delete_item().table_name(&self.table).set_key(Some(key(format!("k#{k}"), "k")));
        req = match expect {
            Expect::Version(ver) => req.condition_expression("v = :expect").expression_attribute_values(":expect", n(ver as i64)),
            _ => req.condition_expression("attribute_exists(pk)"),
        };
        match req.send().await {
            Ok(_) => Ok(true),
            Err(e) if cond_failed(&e) => Ok(false),
            Err(e) => Err(anyhow::anyhow!("kv {k}: {}", e.into_service_error())),
        }
    }

    async fn doc_get(&self, kind: &str, id: &str) -> Result<Option<Versioned<Value>>> {
        let Some(item) = self.get(key(format!("doc#{kind}"), id), true).await? else { return Ok(None) };
        Ok(Some(Versioned { value: serde_json::from_str(get_s(&item, "val").unwrap_or("null"))?, version: get_n(&item, "v").unwrap_or(0) as u64 }))
    }

    async fn doc_put(&self, kind: &str, id: &str, value: &Value, expect: Expect) -> Result<bool> {
        let mut req = self
            .db
            .update_item()
            .table_name(&self.table)
            .set_key(Some(key(format!("doc#{kind}"), id)))
            .update_expression("SET val = :val, v = if_not_exists(v, :zero) + :one")
            .expression_attribute_values(":val", s(serde_json::to_string(value)?))
            .expression_attribute_values(":one", n(1))
            .expression_attribute_values(":zero", n(0));
        req = match expect {
            Expect::Any => req,
            Expect::Absent => req.condition_expression("attribute_not_exists(pk)"),
            Expect::Version(v) => req.condition_expression("v = :expect").expression_attribute_values(":expect", n(v as i64)),
        };
        match req.send().await {
            Ok(_) => Ok(true),
            Err(e) if cond_failed(&e) => Ok(false),
            Err(e) => Err(anyhow::anyhow!("doc {kind}/{id}: {}", e.into_service_error())),
        }
    }

    async fn doc_delete(&self, kind: &str, id: &str) -> Result<bool> {
        match self
            .db
            .delete_item()
            .table_name(&self.table)
            .set_key(Some(key(format!("doc#{kind}"), id)))
            .condition_expression("attribute_exists(pk)")
            .send()
            .await
        {
            Ok(_) => Ok(true),
            Err(e) if cond_failed(&e) => Ok(false),
            Err(e) => Err(anyhow::anyhow!("doc {kind}/{id}: {}", e.into_service_error())),
        }
    }

    async fn doc_list(&self, kind: &str) -> Result<Vec<Versioned<Value>>> {
        let mut out: Vec<(String, Versioned<Value>)> = Vec::new();
        for item in self.query_all(&format!("doc#{kind}"), None, true).await? {
            out.push((
                get_s(&item, "sk").unwrap_or_default().to_string(),
                Versioned { value: serde_json::from_str(get_s(&item, "val").unwrap_or("null"))?, version: get_n(&item, "v").unwrap_or(0) as u64 },
            ));
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out.into_iter().map(|(_, v)| v).collect())
    }

    async fn run_get(&self, id: &str) -> Result<Option<Run>> {
        Ok(match self.get(key(format!("r#{id}"), "r"), true).await? {
            Some(item) => {
                let mut r: Run = decode(&item)?;
                r.version = get_n(&item, "v").unwrap_or(0) as u64;
                Some(r)
            }
            None => None,
        })
    }

    async fn run_put(&self, run: &Run, expect: Expect) -> Result<bool> {
        let version = match expect {
            Expect::Absent => 1,
            Expect::Version(v) => v + 1,
            Expect::Any => run.version + 1,
        };
        let stored = Run { version, ..run.clone() };
        let mut item = key(format!("r#{}", run.id), "r");
        item.insert("d".into(), s(serde_json::to_string(&stored)?));
        item.insert("v".into(), n(version as i64));
        item.insert("st".into(), s(run.status.as_str()));
        item.insert("fn".into(), s(run.function_id.clone()));
        item.insert("g1pk".into(), s(format!("rl#{}", shard_of(&run.id, LIST_SHARDS))));
        item.insert("g1sk".into(), s(run.id.clone()));
        item.insert("g2pk".into(), s(format!("rf#{}#{}", run.function_id, shard_of(&run.id, OWNER_SHARDS))));
        item.insert("g2sk".into(), s(run.id.clone()));
        if let Some(e) = run.expires_at {
            item.insert("ttl".into(), ttl_s(e));
        }
        let mut req = self.db.put_item().table_name(&self.table).set_item(Some(item));
        req = match expect {
            Expect::Absent => req.condition_expression("attribute_not_exists(pk)"),
            Expect::Version(v) => req.condition_expression("v = :expect").expression_attribute_values(":expect", n(v as i64)),
            Expect::Any => req,
        };
        match req.send().await {
            Ok(_) => {}
            Err(e) if cond_failed(&e) => return Ok(false),
            Err(e) => return Err(anyhow::anyhow!("run {}: {}", run.id, e.into_service_error())),
        }
        // Steps expire with their run.
        if let Some(e) = run.expires_at {
            for step in self.query_all(&format!("r#{}", run.id), Some("s#"), false).await? {
                if get_n(&step, "ttl").is_none()
                    && let (Some(pk), Some(sk)) = (get_s(&step, "pk"), get_s(&step, "sk"))
                {
                    let _ = self
                        .db
                        .update_item()
                        .table_name(&self.table)
                        .set_key(Some(key(pk, sk)))
                        .update_expression("SET #ttl = :ttl")
                        .expression_attribute_names("#ttl", "ttl")
                        .expression_attribute_values(":ttl", ttl_s(e))
                        .send()
                        .await;
                }
            }
        }
        Ok(true)
    }

    async fn run_list(&self, q: &RunQuery) -> Result<Page<Run>> {
        let limit = q.limit.clamp(1, 1000);
        let (index, pks) = match &q.function_id {
            Some(f) => ("g2", (0..OWNER_SHARDS).map(|i| format!("rf#{f}#{i}")).collect::<Vec<_>>()),
            None => ("g1", (0..LIST_SHARDS).map(|i| format!("rl#{i}")).collect()),
        };
        let mut items: Vec<Run> = Vec::new();
        let mut cursor = q.cursor.clone();
        let mut exhausted = false;
        for _ in 0..10 {
            let want = (limit + 1 - items.len()).max(20);
            let ids = self.list_ids(index, pks.clone(), cursor.as_deref(), want, &None).await?;
            exhausted = ids.len() < want;
            let Some(last) = ids.last().cloned() else { break };
            let fetched = self.batch_get(ids.iter().map(|id| key(format!("r#{id}"), "r")).collect(), false).await?;
            let mut by_id: HashMap<String, Run> = HashMap::new();
            for item in &fetched {
                let mut r: Run = decode(item)?;
                r.version = get_n(item, "v").unwrap_or(0) as u64;
                by_id.insert(r.id.clone(), r);
            }
            for id in &ids {
                if let Some(r) = by_id.remove(id)
                    && q.status.is_none_or(|s| s == r.status)
                    && items.len() <= limit
                {
                    items.push(r);
                }
            }
            cursor = Some(last);
            if items.len() > limit || exhausted {
                break;
            }
        }
        let cursor = if items.len() > limit {
            items.truncate(limit);
            items.last().map(|r| r.id.clone())
        } else if !exhausted {
            cursor
        } else {
            None
        };
        Ok(Page { items, cursor })
    }

    async fn steps(&self, run_id: &str) -> Result<Vec<Step>> {
        let mut steps: Vec<Step> = self.query_all(&format!("r#{run_id}"), Some("s#"), true).await?.iter().map(decode).collect::<Result<_>>()?;
        steps.sort_by_key(|s| s.at);
        Ok(steps)
    }

    async fn step_put(&self, step: &Step) -> Result<()> {
        let mut item = key(format!("r#{}", step.run_id), format!("s#{}", step.id));
        item.insert("d".into(), s(serde_json::to_string(step)?));
        self.db.put_item().table_name(&self.table).set_item(Some(item)).send().await?;
        Ok(())
    }

    async fn event_put(&self, e: &Event) -> Result<()> {
        let mut item = key(format!("e#{}", e.id), "e");
        item.insert("d".into(), s(serde_json::to_string(e)?));
        item.insert("g1pk".into(), s(format!("el#{}", shard_of(&e.id, LIST_SHARDS))));
        item.insert("g1sk".into(), s(e.id.clone()));
        item.insert("g2pk".into(), s(format!("en#{}#{}", e.name, shard_of(&e.id, OWNER_SHARDS))));
        item.insert("g2sk".into(), s(e.id.clone()));
        if let Some(x) = e.expires_at {
            item.insert("ttl".into(), ttl_s(x));
        }
        self.db.put_item().table_name(&self.table).set_item(Some(item)).send().await?;
        Ok(())
    }

    async fn event_get(&self, id: &str) -> Result<Option<Event>> {
        Ok(match self.get(key(format!("e#{id}"), "e"), true).await? {
            Some(item) => Some(decode(&item)?),
            None => None,
        })
    }

    async fn event_list(&self, q: &EventQuery) -> Result<Page<Event>> {
        let limit = q.limit.clamp(1, 1000);
        let (index, pks) = match &q.name {
            Some(name) => ("g2", (0..OWNER_SHARDS).map(|i| format!("en#{name}#{i}")).collect::<Vec<_>>()),
            None => ("g1", (0..LIST_SHARDS).map(|i| format!("el#{i}")).collect()),
        };
        let ids = self.list_ids(index, pks, q.cursor.as_deref(), limit + 1, &None).await?;
        let fetched = self.batch_get(ids.iter().map(|id| key(format!("e#{id}"), "e")).collect(), false).await?;
        let mut by_id: HashMap<String, Event> = HashMap::new();
        for item in &fetched {
            let e: Event = decode(item)?;
            by_id.insert(e.id.clone(), e);
        }
        let mut items: Vec<Event> = ids.iter().filter_map(|id| by_id.remove(id)).collect();
        let cursor = if items.len() > limit {
            items.truncate(limit);
            items.last().map(|e| e.id.clone())
        } else {
            None
        };
        Ok(Page { items, cursor })
    }

    async fn wait_put(&self, w: &Wait) -> Result<()> {
        let mut item = key(format!("w#{}#{}", w.event, w.key.as_deref().unwrap_or("")), w.id.clone());
        item.insert("d".into(), s(serde_json::to_string(w)?));
        item.insert("exp".into(), n(w.expires_at));
        item.insert("ttl".into(), ttl_s(w.expires_at + 60_000));
        self.db.put_item().table_name(&self.table).set_item(Some(item)).send().await?;
        Ok(())
    }

    async fn wait_delete(&self, w: &Wait) -> Result<()> {
        self.delete(key(format!("w#{}#{}", w.event, w.key.as_deref().unwrap_or("")), w.id.clone())).await
    }

    async fn waits(&self, event: &str, k: Option<&str>) -> Result<Vec<Wait>> {
        let now = now_ms();
        let mut out: Vec<Wait> = Vec::new();
        for item in self.query_all(&format!("w#{event}#{}", k.unwrap_or("")), None, true).await? {
            if get_n(&item, "exp").unwrap_or(0) > now {
                out.push(decode(&item)?);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    async fn usage_add(&self, day: i64, u: &Usage) -> Result<()> {
        let mut adds = Vec::new();
        let mut req = self.db.update_item().table_name(&self.table).set_key(Some(key("u", format!("{day:010}"))));
        for (field, v) in u.fields() {
            if v > 0 {
                adds.push(format!("{field} :{field}"));
                req = req.expression_attribute_values(format!(":{field}"), n(v as i64));
            }
        }
        if adds.is_empty() {
            return Ok(());
        }
        req.update_expression(format!("ADD {}", adds.join(", "))).send().await?;
        Ok(())
    }

    async fn usage(&self, from_day: i64) -> Result<Vec<(i64, Usage)>> {
        let mut out = Vec::new();
        let mut start: Option<Item> = None;
        loop {
            let resp = self
                .db
                .query()
                .table_name(&self.table)
                .key_condition_expression("pk = :pk AND sk >= :from")
                .expression_attribute_values(":pk", s("u"))
                .expression_attribute_values(":from", s(format!("{:010}", from_day.max(0))))
                .set_exclusive_start_key(start.take())
                .send()
                .await?;
            for item in resp.items.unwrap_or_default() {
                let Some(day) = get_s(&item, "sk").and_then(|d| d.parse().ok()) else { continue };
                let mut u = Usage::default();
                for (field, _) in Usage::default().fields() {
                    if let Some(v) = get_n(&item, field) {
                        u.set(field, v as u64);
                    }
                }
                out.push((day, u));
            }
            match resp.last_evaluated_key {
                Some(k) if !k.is_empty() => start = Some(k),
                _ => return Ok(out),
            }
        }
    }

    async fn sweep(&self, _now: Ms) -> Result<u64> {
        // DynamoDB TTL deletes expired items (and S3 lifecycle rules expire large bodies).
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    /// Runs when `ROLL_TASKS_E2E_DYNAMO` points at DynamoDB (e.g. DynamoDB Local on :8000).
    #[tokio::test]
    async fn conformance() {
        let Ok(endpoint) = std::env::var("ROLL_TASKS_E2E_DYNAMO") else {
            eprintln!("skipping: set ROLL_TASKS_E2E_DYNAMO=http://127.0.0.1:8000");
            return;
        };
        let store = super::DynamoStore::connect(super::DynamoConfig {
            table: format!("roll-tasks-conformance-{}", crate::ids::ulid()),
            endpoint: Some(endpoint),
            region: Some("us-east-1".into()),
            shards: 4,
            bucket: None,
            s3_endpoint: None,
            inline_max: 32 << 10,
            create_table: true,
        })
        .await
        .unwrap();
        super::super::conformance::run(std::sync::Arc::new(store)).await;
    }
}
