//! The DynamoDB backend: membership and leases as items of one table (`pk`/`sk`,
//! docs/cloud/README.md "Coordination: DynamoDB"). DynamoDB has no clock a statement can read, so
//! nothing here compares clocks between machines. Liveness follows the AWS DynamoDB lock client's
//! record-version-number scheme, with one record per node instead of one per lock:
//!
//! | item | pk / sk | attributes |
//! | --- | --- | --- |
//! | node | `node` / `<id>` | `url`, `zone`, `version`, `draining`, `left`, `inc` (incarnation), `hb` (a random token rewritten by every renewal), `gen` (bumped when another node takes one of its leases), `rel` (bumped after releases) |
//! | lease | `lease` / `<key>` | `holder`, `inc` (the holder's incarnation), `epoch`, `free`, `rvn` (a random token rewritten by every change) |
//!
//! - **Renewals are per node.** A node renews every lease it holds with one conditional write of
//!   its node item (`hb` gets a new token; condition: same `inc` and `gen`). A lease is valid
//!   while its holder's `hb` keeps changing and its epoch is unchanged, so the table sees
//!   nodes × renewal rate writes, whatever the number of leases.
//! - **Liveness without clocks.** Every node reads the node items (a consistent query each
//!   heartbeat) and remembers, on its own monotonic clock, when it first saw each `hb` value. A
//!   node whose `hb` hasn't changed for `node_ttl` drops out of placement; once it hasn't changed
//!   for `lease_ttl`, its leases may be taken.
//! - **Takeover** is one transaction: the lease's `rvn` must be the one read, and the holder's
//!   node item must still carry the `hb` that was watched (its `gen` goes up). If the holder
//!   renewed meanwhile, the transaction fails. A holder whose node item is gone (cleaned up after
//!   a minute of silence) or carries another incarnation is gone for good: its leases are taken
//!   on the lease's `rvn` alone.
//! - **Self-fencing.** The holder acts on its leases until `sent + lease_ttl - lease_margin`,
//!   where `sent` is when its last successful renewal was sent, on its own clock. A contender
//!   can only take a lease after watching that renewal's `hb` for `lease_ttl`, and it first saw
//!   it no earlier than it was written, which is after it was sent: the holder stops
//!   `lease_margin` before anyone can start (with clock rates within `lease_margin / lease_ttl` of
//!   each other). A renewal that lands late can't extend a lease taken meanwhile: the takeover
//!   bumped `gen`, so the renewal's condition fails and the node re-reads which leases are still
//!   its own before it renews again.
//! - **Acquisitions** ride on the node's last confirmed renewal (contenders watch the same
//!   `hb`), and aren't tried once that's past its validity.
//! - Conditional-check failures are outcomes (a race lost, a lease that moved), not errors. The
//!   SDK retries throttling and network errors with backoff; a call that fails leaves the
//!   deadline where the last success put it, and a lease write whose outcome is unknown is read
//!   again at the next renewal.

use super::{Backend, Confirmed, Holder, Member, Registration, Timing};
use crate::{Error, Node, Result, clock};
use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue, BillingMode, KeySchemaElement, KeyType, KeysAndAttributes, ScalarAttributeType, TransactWriteItem, Update,
};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::{Duration, Instant};

type Item = HashMap<String, AttributeValue>;

/// Where the table is.
#[derive(Clone, Debug)]
pub struct DynamoDbConfig {
    /// The table (`roll-cluster`).
    pub table: String,
    /// DynamoDB Local or another endpoint; default is AWS (region and credentials from the
    /// environment, as the SDK finds them).
    pub endpoint: Option<String>,
    pub region: Option<String>,
    /// Create the table if it doesn't exist (local development and tests; in the cloud the stack
    /// owns it).
    pub create_table: bool,
    /// How long one call may take, retries included (2 s).
    pub request_timeout: Duration,
}

impl DynamoDbConfig {
    pub fn new(table: impl Into<String>) -> DynamoDbConfig {
        DynamoDbConfig { table: table.into(), endpoint: None, region: None, create_table: false, request_timeout: Duration::from_secs(2) }
    }
}

/// What this node knows about a node item.
#[derive(Clone)]
struct Seen {
    url: String,
    zone: Option<String>,
    inc: String,
    hb: String,
    /// When this `hb` was first seen here.
    since: Duration,
    draining: bool,
    left: bool,
    rel: i64,
    /// A sweep already looked for its leases since this `hb` went silent for a lease length.
    swept_silent: bool,
}

struct State {
    /// This process's incarnation; a new one if its node item was cleaned up while it stalled.
    inc: String,
    /// The `gen` of our node item as last read.
    generation: i64,
    /// When the last successful renewal (or registration) was sent.
    confirmed: Option<Duration>,
    /// Leases naming this incarnation, as far as we know, with their epochs.
    mine: HashMap<String, i64>,
    /// Keys whose last write of ours has an unknown outcome: read again at the next renewal.
    doubt: HashSet<String>,
    seen: HashMap<String, Seen>,
    /// Something happened that may have freed leases (a release, a node gone or silent).
    scan_due: bool,
    /// The last scan found takeable leases: look again.
    found: bool,
    last_scan: Option<Duration>,
    cursor: Option<Item>,
}

struct Me {
    reg: Registration,
    timing: Timing,
}

/// One node's handle on the table.
pub struct DynamoDb {
    rt: Option<tokio::runtime::Runtime>,
    client: Client,
    table: String,
    me: std::sync::OnceLock<Me>,
    state: Mutex<State>,
}

impl Drop for DynamoDb {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

fn s(v: impl Into<String>) -> AttributeValue {
    AttributeValue::S(v.into())
}
fn n(v: i64) -> AttributeValue {
    AttributeValue::N(v.to_string())
}
fn yes() -> AttributeValue {
    AttributeValue::Bool(true)
}
fn get_s(item: &Item, k: &str) -> String {
    item.get(k).and_then(|v| v.as_s().ok()).cloned().unwrap_or_default()
}
fn get_n(item: &Item, k: &str) -> i64 {
    item.get(k).and_then(|v| v.as_n().ok()).and_then(|s| s.parse().ok()).unwrap_or(0)
}
fn get_b(item: &Item, k: &str) -> bool {
    item.get(k).and_then(|v| v.as_bool().ok()).copied().unwrap_or(false)
}
fn key(pk: &str, sk: &str) -> Item {
    HashMap::from([("pk".to_string(), s(pk)), ("sk".to_string(), s(sk))])
}
fn values(v: Vec<(&str, AttributeValue)>) -> Item {
    v.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// A random token: unique enough that a value never comes back.
fn token() -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut out = String::with_capacity(32);
    for i in 0..2u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        h.write_u64(N.fetch_add(1, Ordering::Relaxed) ^ (i << 63));
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

/// What a conditional write ended in.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    Done,
    /// A condition didn't hold, or a transaction ran into another: an expected answer.
    Refused,
}

fn sdk<E: ProvideErrorMetadata + std::fmt::Debug, R: std::fmt::Debug>(what: &str, e: SdkError<E, R>) -> Error {
    let detail = match e.as_service_error() {
        Some(s) => format!("{}: {}", s.code().unwrap_or("error"), s.message().unwrap_or("")),
        None => format!("{e:?}"),
    };
    Error::Backend(format!("dynamodb {what}: {detail}"))
}

/// A write's result: a refused condition is an outcome, anything else an error.
fn outcome<T, E: ProvideErrorMetadata + std::fmt::Debug, R: std::fmt::Debug>(what: &str, r: std::result::Result<T, SdkError<E, R>>) -> Result<Outcome> {
    let Err(e) = r else { return Ok(Outcome::Done) };
    let (code, msg) = match e.as_service_error() {
        Some(s) => (s.code().unwrap_or("").to_string(), s.message().unwrap_or("").to_string()),
        None => (String::new(), String::new()),
    };
    match code.as_str() {
        "ConditionalCheckFailedException" | "TransactionConflictException" => Ok(Outcome::Refused),
        "TransactionCanceledException" if msg.contains("ConditionalCheckFailed") || msg.contains("TransactionConflict") => Ok(Outcome::Refused),
        _ => Err(sdk(what, e)),
    }
}

fn run_on<T: Send + 'static>(rt: &tokio::runtime::Runtime, f: impl Future<Output = T> + Send + 'static) -> T {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    rt.spawn(async move {
        let _ = tx.send(f.await);
    });
    rx.recv().expect("the DynamoDB backend's runtime stopped")
}

fn build_err(e: aws_sdk_dynamodb::error::BuildError) -> Error {
    Error::Config(e.to_string())
}

impl DynamoDb {
    /// Connect (and create the table if configured to). Blocks: call it outside async code, or
    /// in `spawn_blocking`.
    pub fn connect(cfg: DynamoDbConfig) -> Result<DynamoDb> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("roll-cluster-ddb")
            .enable_all()
            .build()
            .map_err(|e| Error::Config(format!("runtime: {e}")))?;
        let c = cfg.clone();
        let client = run_on(&rt, async move {
            let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest())
                .retry_config(aws_config::retry::RetryConfig::standard().with_max_attempts(3))
                .timeout_config(
                    aws_config::timeout::TimeoutConfig::builder()
                        .operation_timeout(c.request_timeout)
                        .operation_attempt_timeout(c.request_timeout / 2)
                        .connect_timeout(c.request_timeout / 2)
                        .build(),
                );
            if let Some(r) = &c.region {
                loader = loader.region(aws_config::Region::new(r.clone()));
            }
            if let Some(e) = &c.endpoint {
                loader = loader.endpoint_url(e);
                if std::env::var("AWS_ACCESS_KEY_ID").is_err() {
                    loader = loader.credentials_provider(aws_sdk_dynamodb::config::Credentials::new("local", "local", None, None, "roll"));
                    if c.region.is_none() {
                        loader = loader.region(aws_config::Region::new("us-east-1"));
                    }
                }
            }
            Client::new(&loader.load().await)
        });
        let db = DynamoDb {
            rt: Some(rt),
            client,
            table: cfg.table.clone(),
            me: std::sync::OnceLock::new(),
            state: Mutex::new(State {
                inc: token(),
                generation: 0,
                confirmed: None,
                mine: HashMap::new(),
                doubt: HashSet::new(),
                seen: HashMap::new(),
                scan_due: true,
                found: false,
                last_scan: None,
                cursor: None,
            }),
        };
        if cfg.create_table {
            db.create_table()?;
        }
        Ok(db)
    }

    /// Run `f` on this backend's runtime and wait for it. Works from any thread, async ones
    /// included (it blocks, as a database call would).
    fn run<T: Send + 'static>(&self, f: impl Future<Output = T> + Send + 'static) -> T {
        run_on(self.rt.as_ref().expect("runtime"), f)
    }

    fn create_table(&self) -> Result<()> {
        let (c, table) = (self.client.clone(), self.table.clone());
        self.run(async move {
            if c.describe_table().table_name(&table).send().await.is_ok() {
                return Ok(());
            }
            let attr = |name: &str| AttributeDefinition::builder().attribute_name(name).attribute_type(ScalarAttributeType::S).build();
            let ks = |name: &str, t: KeyType| KeySchemaElement::builder().attribute_name(name).key_type(t).build();
            let req = c
                .create_table()
                .table_name(&table)
                .billing_mode(BillingMode::PayPerRequest)
                .key_schema(ks("pk", KeyType::Hash).map_err(build_err)?)
                .key_schema(ks("sk", KeyType::Range).map_err(build_err)?)
                .attribute_definitions(attr("pk").map_err(build_err)?)
                .attribute_definitions(attr("sk").map_err(build_err)?);
            match req.send().await {
                Ok(_) => {}
                Err(e) if e.as_service_error().and_then(|s| s.code()) == Some("ResourceInUseException") => {}
                Err(e) => return Err(sdk("create table", e)),
            }
            for _ in 0..120 {
                if let Ok(d) = c.describe_table().table_name(&table).send().await
                    && d.table().and_then(|t| t.table_status()).is_some_and(|s| s.as_str() == "ACTIVE")
                {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }
            Err(Error::Backend(format!("table {table} never became active")))
        })
    }

    fn me(&self) -> &Me {
        self.me.get().expect("joined")
    }

    fn id(&self) -> &str {
        &self.me().reg.node_id
    }

    fn inc(&self) -> String {
        self.state.lock().inc.clone()
    }

    fn get(&self, pk: &str, sk: &str) -> Result<Option<Item>> {
        let (c, table, k) = (self.client.clone(), self.table.clone(), key(pk, sk));
        self.run(async move { c.get_item().table_name(table).set_key(Some(k)).consistent_read(true).send().await })
            .map(|o| o.item)
            .map_err(|e| sdk("get", e))
    }

    /// Consistent reads of many leases (the ones that exist), by key.
    fn get_leases(&self, keys: &[String]) -> Result<HashMap<String, Item>> {
        let mut out = HashMap::new();
        for chunk in keys.chunks(100) {
            let mut want: Vec<Item> = chunk.iter().map(|k| key("lease", k)).collect();
            for attempt in 0.. {
                if want.is_empty() {
                    break;
                }
                if attempt == 10 {
                    return Err(Error::Backend("dynamodb batch get: keys left unprocessed".into()));
                }
                let ka = KeysAndAttributes::builder().set_keys(Some(std::mem::take(&mut want))).consistent_read(true).build().map_err(build_err)?;
                let (c, table) = (self.client.clone(), self.table.clone());
                let r = self.run(async move { c.batch_get_item().request_items(table, ka).send().await }).map_err(|e| sdk("batch get", e))?;
                for item in r.responses.unwrap_or_default().into_values().flatten() {
                    out.insert(get_s(&item, "sk"), item);
                }
                want = r.unprocessed_keys.unwrap_or_default().into_values().flat_map(|ka| ka.keys).collect();
                if !want.is_empty() {
                    std::thread::sleep(Duration::from_millis(20 << attempt.min(4)));
                }
            }
        }
        Ok(out)
    }

    /// Whether a lease item is this incarnation's (and not released).
    fn is_mine(&self, l: &Item, inc: &str) -> bool {
        get_s(l, "holder") == self.id() && get_s(l, "inc") == inc && !get_b(l, "free")
    }

    /// Record a node item, keeping when its `hb` was first seen. True if it may have freed
    /// leases (released some, or new to us).
    fn observe(st: &mut State, id: &str, item: &Item, now: Duration) -> bool {
        let (hb, inc, rel) = (get_s(item, "hb"), get_s(item, "inc"), get_n(item, "rel"));
        let prev = st.seen.get(id);
        let (since, swept_silent) = match prev {
            Some(p) if p.hb == hb && p.inc == inc => (p.since, p.swept_silent),
            _ => (now, false),
        };
        let event = prev.is_none_or(|p| p.inc != inc || p.rel != rel);
        let zone = Some(get_s(item, "zone")).filter(|z| !z.is_empty());
        st.seen.insert(id.to_string(), Seen { url: get_s(item, "url"), zone, inc, hb, since, draining: get_b(item, "draining"), left: get_b(item, "left"), rel, swept_silent });
        event
    }

    /// Whether a lease held by `(id, inc)` may be taken, by what this node has seen: `Some(hb)`
    /// while the holder is silent (the takeover checks that `hb` is still there), `Some("")` when
    /// that incarnation is gone for good, `None` while it's alive.
    fn holder_gone(&self, st: &State, id: &str, inc: &str, now: Duration) -> Option<String> {
        match st.seen.get(id) {
            None => Some(String::new()),
            Some(s) if s.inc != inc => Some(String::new()),
            Some(s) if s.since + self.me().timing.lease_ttl <= now => Some(s.hb.clone()),
            Some(_) => None,
        }
    }

    fn put_node(&self, reg: &Registration, inc: &str, condition: &str, vals: Item) -> Result<Outcome> {
        let mut item = key("node", &reg.node_id);
        item.insert("url".into(), s(&reg.url));
        if let Some(z) = &reg.zone {
            item.insert("zone".into(), s(z));
        }
        item.insert("version".into(), s(env!("CARGO_PKG_VERSION")));
        item.insert("inc".into(), s(inc));
        item.insert("hb".into(), s(token()));
        item.insert("gen".into(), n(0));
        item.insert("rel".into(), n(0));
        item.insert("draining".into(), AttributeValue::Bool(false));
        item.insert("left".into(), AttributeValue::Bool(false));
        let (c, table, cond) = (self.client.clone(), self.table.clone(), condition.to_string());
        let vals = Some(vals).filter(|v| !v.is_empty());
        outcome(
            "register",
            self.run(async move { c.put_item().table_name(table).set_item(Some(item)).condition_expression(cond).set_expression_attribute_values(vals).send().await }),
        )
    }

    /// A conditional update. `names` maps `#x` placeholders.
    fn update(&self, pk: &str, sk: &str, set: &str, cond: &str, vals: Item, names: &[(&str, &str)]) -> Result<Outcome> {
        let (c, table, k) = (self.client.clone(), self.table.clone(), key(pk, sk));
        let (set, cond) = (set.to_string(), cond.to_string());
        let names: HashMap<String, String> = names.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        let names = Some(names).filter(|n| !n.is_empty());
        outcome(
            "update",
            self.run(async move {
                c.update_item()
                    .table_name(table)
                    .set_key(Some(k))
                    .update_expression(set)
                    .condition_expression(cond)
                    .set_expression_attribute_values(Some(vals))
                    .set_expression_attribute_names(names)
                    .send()
                    .await
            }),
        )
    }

    /// A conditional delete.
    fn delete_item(&self, pk: &str, sk: &str, cond: &str, vals: Item) -> Result<Outcome> {
        let (c, table, k, cond) = (self.client.clone(), self.table.clone(), key(pk, sk), cond.to_string());
        outcome(
            "delete",
            self.run(async move { c.delete_item().table_name(table).set_key(Some(k)).condition_expression(cond).set_expression_attribute_values(Some(vals)).send().await }),
        )
    }

    /// Taking a lease: holder, incarnation and epoch, on the `rvn` that was read (or on there
    /// being no item).
    fn take(&self, rvn: Option<&str>, inc: &str, epoch: i64) -> (String, String, Item) {
        let mut vals = values(vec![(":h", s(self.id())), (":i", s(inc)), (":e", n(epoch)), (":f", AttributeValue::Bool(false)), (":r", s(token()))]);
        let cond = match rvn {
            Some(r) => {
                vals.insert(":old".into(), s(r));
                "rvn = :old"
            }
            None => "attribute_not_exists(pk)",
        };
        ("SET holder = :h, inc = :i, epoch = :e, #f = :f, rvn = :r".into(), cond.into(), vals)
    }

    /// Take one key if it's free, ours, or its holder is gone. `Some(epoch)` if it's ours now.
    /// Take `k` if we may: its epoch, and whether it was taken from a holder that never
    /// released it (gone, or silent for a lease length).
    fn acquire_one(&self, k: &str) -> Result<Option<(i64, bool)>> {
        let inc = self.inc();
        let cur = self.get("lease", k)?;
        // (rvn read, new epoch, a silent holder whose hb the takeover checks)
        let (rvn, epoch, silent, took) = match &cur {
            None => (None, 1, None, false),
            Some(l) => {
                let (holder, linc, epoch, free) = (get_s(l, "holder"), get_s(l, "inc"), get_n(l, "epoch"), get_b(l, "free"));
                let ours = holder == self.id() && linc == inc;
                if ours && !free {
                    return Ok(Some((epoch, false)));
                }
                let next = if ours { epoch } else { epoch + 1 };
                let rvn = Some(get_s(l, "rvn"));
                if free || ours {
                    (rvn, next, None, false)
                } else {
                    // Look at the holder's node item now (and remember what we saw).
                    let item = self.get("node", &holder)?;
                    let now = clock::now();
                    let mut st = self.state.lock();
                    match &item {
                        Some(i) => {
                            if Self::observe(&mut st, &holder, i, now) {
                                st.scan_due = true;
                            }
                        }
                        None => {
                            st.seen.remove(&holder);
                        }
                    }
                    match self.holder_gone(&st, &holder, &linc, now) {
                        None => return Ok(None),
                        Some(hb) if hb.is_empty() => (rvn, next, None, true),
                        Some(hb) => (rvn, next, Some((holder, linc, hb)), true),
                    }
                }
            }
        };
        let (set, cond, vals) = self.take(rvn.as_deref(), &inc, epoch);
        let r = match silent {
            None => self.update("lease", k, &set, &cond, vals, &[("#f", "free")]),
            Some((holder, hinc, hb)) => {
                // The holder must still be silent (its hb unchanged). Its gen goes up, so a
                // renewal of its that lands after this fails, and it reads its leases again.
                let node = Update::builder()
                    .table_name(&self.table)
                    .set_key(Some(key("node", &holder)))
                    .update_expression("SET gen = gen + :one")
                    .condition_expression("hb = :hb AND inc = :inc")
                    .set_expression_attribute_values(Some(values(vec![(":one", n(1)), (":hb", s(hb)), (":inc", s(hinc))])))
                    .build()
                    .map_err(build_err)?;
                let lease = Update::builder()
                    .table_name(&self.table)
                    .set_key(Some(key("lease", k)))
                    .update_expression(set)
                    .condition_expression(cond)
                    .set_expression_attribute_values(Some(vals))
                    .expression_attribute_names("#f", "free")
                    .build()
                    .map_err(build_err)?;
                let items = vec![TransactWriteItem::builder().update(node).build(), TransactWriteItem::builder().update(lease).build()];
                let c = self.client.clone();
                outcome("take over", self.run(async move { c.transact_write_items().set_transact_items(Some(items)).send().await }))
            }
        };
        let mut st = self.state.lock();
        match r {
            Ok(Outcome::Done) => {
                st.mine.insert(k.to_string(), epoch);
                st.doubt.remove(k);
                Ok(Some((epoch, took)))
            }
            Ok(Outcome::Refused) => Ok(None),
            Err(e) => {
                // It may have been written: look again at the next renewal.
                st.doubt.insert(k.to_string());
                Err(e)
            }
        }
    }

    /// Settle keys whose last write had an unknown outcome.
    fn settle_doubts(&self) -> Result<()> {
        let doubt: Vec<String> = self.state.lock().doubt.iter().cloned().collect();
        if doubt.is_empty() {
            return Ok(());
        }
        let items = self.get_leases(&doubt)?;
        let mut st = self.state.lock();
        let inc = st.inc.clone();
        for k in &doubt {
            st.doubt.remove(k);
            match items.get(k).filter(|l| self.is_mine(l, &inc)) {
                Some(l) => st.mine.insert(k.clone(), get_n(l, "epoch")),
                None => st.mine.remove(k),
            };
        }
        Ok(())
    }
}

impl Backend for DynamoDb {
    fn name(&self) -> &'static str {
        "dynamodb"
    }

    /// Register as a new incarnation: at once if the id is unused, else once the previous
    /// process's `hb` stayed unchanged for `lease_ttl` (as seen from here).
    fn join(&self, reg: &Registration, t: Timing) -> Result<()> {
        if t.renew_every * 2 > t.node_ttl {
            return Err(Error::Config("on DynamoDB, renew_every must be at most half of node_ttl (renewals are the heartbeat)".into()));
        }
        let _ = self.me.set(Me { reg: reg.clone(), timing: t });
        let deadline = Instant::now() + t.node_ttl + t.lease_ttl + Duration::from_secs(1);
        let mut watched: Option<(String, String, Duration)> = None;
        loop {
            let sent = clock::now();
            let inc = token();
            let done = match self.get("node", &reg.node_id)? {
                None => self.put_node(reg, &inc, "attribute_not_exists(pk)", Item::new())? == Outcome::Done,
                Some(item) => {
                    let (hb, old) = (get_s(&item, "hb"), get_s(&item, "inc"));
                    let now = clock::now();
                    match &watched {
                        Some((h, i, since)) if *h == hb && *i == old => {
                            *since + t.lease_ttl <= now
                                && self.put_node(reg, &inc, "hb = :hb AND inc = :inc", values(vec![(":hb", s(&hb)), (":inc", s(&old))]))? == Outcome::Done
                        }
                        _ => {
                            watched = Some((hb, old, now));
                            false
                        }
                    }
                }
            };
            if done {
                let mut st = self.state.lock();
                st.inc = inc;
                st.generation = 0;
                st.confirmed = Some(sent);
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(Error::NodeIdInUse(reg.node_id.clone()));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Renewals are the heartbeat on DynamoDB.
    fn heartbeat(&self, _draining: bool) -> Result<()> {
        Ok(())
    }

    fn members(&self) -> Result<Vec<Member>> {
        let mut items: Vec<Item> = Vec::new();
        let mut start: Option<Item> = None;
        loop {
            let (c, table, from) = (self.client.clone(), self.table.clone(), start.take());
            let r = self
                .run(async move {
                    c.query()
                        .table_name(table)
                        .key_condition_expression("pk = :pk")
                        .expression_attribute_values(":pk", s("node"))
                        .consistent_read(true)
                        .set_exclusive_start_key(from)
                        .send()
                        .await
                })
                .map_err(|e| sdk("query nodes", e))?;
            items.extend(r.items.unwrap_or_default());
            match r.last_evaluated_key {
                Some(k) => start = Some(k),
                None => break,
            }
        }
        let now = clock::now();
        let t = self.me().timing;
        let mut st = self.state.lock();
        let ids: HashSet<String> = items.iter().map(|i| get_s(i, "sk")).collect();
        let before = st.seen.len();
        st.seen.retain(|id, _| ids.contains(id));
        let mut event = st.seen.len() != before;
        for item in &items {
            event |= Self::observe(&mut st, &get_s(item, "sk"), item, now);
        }
        if event {
            st.scan_due = true;
        }
        let mut v: Vec<Member> = st
            .seen
            .iter()
            .filter(|(_, s)| !s.left && s.since + t.node_ttl > now)
            .map(|(id, s)| Member { node: Node { id: id.clone(), url: s.url.clone() }, zone: s.zone.clone(), draining: s.draining })
            .collect();
        v.sort_by(|a, b| a.node.id.cmp(&b.node.id));
        Ok(v)
    }

    fn set_draining(&self) -> Result<()> {
        self.update("node", self.id(), "SET draining = :t", "inc = :inc", values(vec![(":t", yes()), (":inc", s(self.inc()))]), &[])?;
        Ok(())
    }

    fn renew(&self) -> Result<Confirmed> {
        self.settle_doubts()?;
        for _ in 0..4 {
            let (inc, generation) = {
                let st = self.state.lock();
                (st.inc.clone(), st.generation)
            };
            let basis = clock::now();
            let wrote =
                self.update("node", self.id(), "SET hb = :hb", "inc = :inc AND gen = :gen", values(vec![(":hb", s(token())), (":inc", s(&inc)), (":gen", n(generation))]), &[])?;
            if wrote == Outcome::Done {
                let mut st = self.state.lock();
                st.confirmed = Some(basis);
                let leases = st.mine.iter().map(|(k, e)| (k.clone(), *e)).collect();
                return Ok(Confirmed { leases, basis, taken_over: 0 });
            }
            // Our node item changed under us: cleaned up, taken by another process, or another
            // node took one of our leases (gen).
            match self.get("node", self.id())? {
                None => {
                    // Cleaned up while this node was silent for a long time: come back as a new
                    // incarnation. Leases of the old one are anyone's (this node's too).
                    let new = token();
                    let reg = self.me().reg.clone();
                    if self.put_node(&reg, &new, "attribute_not_exists(pk)", Item::new())? == Outcome::Done {
                        let mut st = self.state.lock();
                        (st.inc, st.generation, st.confirmed) = (new, 0, Some(basis));
                        st.mine.clear();
                        tracing::warn!(node = %self.id(), "this node's record was cleaned up while it was silent; registered again");
                        return Ok(Confirmed { leases: Vec::new(), basis, taken_over: 0 });
                    }
                }
                Some(item) if get_s(&item, "inc") != inc => return Err(Error::Superseded),
                Some(item) => {
                    let keys: Vec<(String, i64)> = self.state.lock().mine.iter().map(|(k, e)| (k.clone(), *e)).collect();
                    let names: Vec<String> = keys.iter().map(|(k, _)| k.clone()).collect();
                    let items = self.get_leases(&names)?;
                    let mut st = self.state.lock();
                    let mut lost = 0;
                    for (k, e) in &keys {
                        if !items.get(k).is_some_and(|l| self.is_mine(l, &inc) && get_n(l, "epoch") == *e) {
                            st.mine.remove(k);
                            lost += 1;
                        }
                    }
                    if lost > 0 {
                        tracing::info!(node = %self.id(), leases = lost, "another node took leases of this one");
                    }
                    st.generation = get_n(&item, "gen");
                }
            }
        }
        Err(Error::Backend("dynamodb: renewing kept racing with changes to this node's record".into()))
    }

    fn acquire(&self, keys: &[String]) -> Result<Confirmed> {
        let t = self.me().timing;
        let now = clock::now();
        // A lease is only as good as the renewal it rides on: past that renewal's validity,
        // taking one would make it look held while this node can't act on it.
        let Some(basis) = self.state.lock().confirmed.filter(|b| *b + t.lease_ttl > now) else {
            return Ok(Confirmed { leases: Vec::new(), basis: now.saturating_sub(t.lease_ttl), taken_over: 0 });
        };
        let (mut leases, mut taken_over) = (Vec::new(), 0);
        for k in keys {
            // On an error, what was won so far is adopted by the next renewal (it's in `mine`).
            if let Some((e, took)) = self.acquire_one(k)? {
                taken_over += usize::from(took);
                leases.push((k.clone(), e));
            }
        }
        Ok(Confirmed { leases, basis, taken_over })
    }

    fn release(&self, leases: &[(String, i64)]) -> Result<()> {
        let inc = self.inc();
        let mut err = None;
        for (k, e) in leases {
            let vals = values(vec![(":t", yes()), (":r", s(token())), (":h", s(self.id())), (":inc", s(&inc)), (":e", n(*e))]);
            let r = self.update("lease", k, "SET #f = :t, rvn = :r", "holder = :h AND inc = :inc AND epoch = :e", vals, &[("#f", "free")]);
            let mut st = self.state.lock();
            match r {
                Ok(_) => {
                    st.mine.remove(k);
                }
                Err(e) => {
                    st.doubt.insert(k.clone());
                    err = Some(e);
                }
            }
        }
        // Tell the other nodes to look for free leases.
        self.update("node", self.id(), "SET rel = rel + :one", "inc = :inc", values(vec![(":one", n(1)), (":inc", s(&inc))]), &[])?;
        err.map_or(Ok(()), Err)
    }

    fn delete(&self, leases: &[(String, i64)]) -> Result<()> {
        let inc = self.inc();
        let mut err = None;
        for (k, e) in leases {
            let r = self.delete_item("lease", k, "holder = :h AND inc = :inc AND epoch = :e", values(vec![(":h", s(self.id())), (":inc", s(&inc)), (":e", n(*e))]));
            let mut st = self.state.lock();
            match r {
                Ok(_) => {
                    st.mine.remove(k);
                }
                Err(e) => {
                    st.doubt.insert(k.clone());
                    err = Some(e);
                }
            }
        }
        err.map_or(Ok(()), Err)
    }

    /// Lease pages are read only when something may have freed a lease (a release, a node gone,
    /// a holder silent for a lease length since the last look, the last look found some), and
    /// every `3 × lease_ttl` regardless. Whether a holder went silent is decided here, from what
    /// the last heartbeat saw, so a dead node's leases are looked for at the first sweep after
    /// they become takeable (not at the next periodic scan).
    fn sweep(&self, page: usize) -> Result<(Vec<String>, bool)> {
        let t = self.me().timing;
        let from = {
            let mut st = self.state.lock();
            if st.cursor.is_none() {
                let now = clock::now();
                let silent = |s: &Seen| s.since + t.lease_ttl <= now;
                let newly_silent = st.seen.values().any(|s| !s.swept_silent && silent(s));
                if !(st.scan_due || st.found || newly_silent || st.last_scan.is_none_or(|l| l + t.lease_ttl * 3 <= now)) {
                    return Ok((Vec::new(), false));
                }
                (st.scan_due, st.found) = (false, false);
                for s in st.seen.values_mut().filter(|s| silent(s)) {
                    s.swept_silent = true;
                }
            }
            st.cursor.take()
        };
        let (c, table) = (self.client.clone(), self.table.clone());
        let r = self.run(async move {
            c.query()
                .table_name(table)
                .key_condition_expression("pk = :pk")
                .expression_attribute_values(":pk", s("lease"))
                .consistent_read(true)
                .limit(page.clamp(1, 1000) as i32)
                .set_exclusive_start_key(from)
                .send()
                .await
        });
        let r = r.map_err(|e| {
            self.state.lock().scan_due = true;
            sdk("query leases", e)
        })?;
        let now = clock::now();
        let mut st = self.state.lock();
        let inc = st.inc.clone();
        let keys: Vec<String> = r
            .items
            .unwrap_or_default()
            .into_iter()
            .filter(|l| !self.is_mine(l, &inc) && (get_b(l, "free") || self.holder_gone(&st, &get_s(l, "holder"), &get_s(l, "inc"), now).is_some()))
            .map(|l| get_s(&l, "sk"))
            .collect();
        st.found |= !keys.is_empty();
        st.cursor = r.last_evaluated_key;
        let more = st.cursor.is_some();
        if !more {
            st.last_scan = Some(now);
        }
        Ok((keys, more))
    }

    fn lookup(&self, key: &str) -> Result<Holder> {
        let t = self.me().timing;
        let Some(l) = self.get("lease", key)? else { return Ok(Holder::Free) };
        if get_b(&l, "free") {
            return Ok(Holder::Free);
        }
        let (holder, inc) = (get_s(&l, "holder"), get_s(&l, "inc"));
        if holder == self.id() {
            let st = self.state.lock();
            if inc != st.inc {
                return Ok(Holder::Free);
            }
            let left = st.confirmed.map_or(Duration::ZERO, |c| (c + t.lease_ttl).saturating_sub(clock::now()));
            return Ok(Holder::Me { left: left.max(Duration::from_millis(1)) });
        }
        if !self.state.lock().seen.contains_key(&holder)
            && let Some(i) = self.get("node", &holder)?
        {
            Self::observe(&mut self.state.lock(), &holder, &i, clock::now());
        }
        let now = clock::now();
        let st = self.state.lock();
        let Some(seen) = st.seen.get(&holder).filter(|_| self.holder_gone(&st, &holder, &inc, now).is_none()) else { return Ok(Holder::Free) };
        Ok(Holder::Other {
            id: holder.clone(),
            url: Some(seen.url.clone()),
            live: !seen.left && seen.since + t.node_ttl > now,
            left: (seen.since + t.lease_ttl).saturating_sub(now).max(Duration::from_millis(1)),
        })
    }

    fn leave(&self, released_all: bool) -> Result<()> {
        let vals = values(vec![(":inc", s(self.inc()))]);
        if released_all {
            self.delete_item("node", self.id(), "inc = :inc", vals)?;
        } else {
            // Leases still held (requests in flight) lapse as a dead node's do: `hb` stays.
            let mut vals = vals;
            vals.insert(":t".into(), yes());
            self.update("node", self.id(), "SET #l = :t", "inc = :inc", vals, &[("#l", "left")])?;
        }
        Ok(())
    }

    fn cleanup(&self) -> Result<()> {
        let t = self.me().timing;
        let age = (t.node_ttl * 12).max(Duration::from_secs(60));
        let now = clock::now();
        let dead: Vec<(String, String, String)> = {
            let st = self.state.lock();
            st.seen.iter().filter(|(id, s)| *id != self.id() && s.since + age <= now).map(|(id, s)| (id.clone(), s.hb.clone(), s.inc.clone())).collect()
        };
        for (id, hb, inc) in dead {
            self.delete_item("node", &id, "hb = :hb AND inc = :inc", values(vec![(":hb", s(hb)), (":inc", s(inc))]))?;
            self.state.lock().seen.remove(&id);
        }
        Ok(())
    }

    fn holder(&self, key: &str) -> Result<Option<(String, i64)>> {
        Ok(self.get("lease", key)?.map(|l| (get_s(&l, "holder"), get_n(&l, "epoch"))))
    }
}
