//! Imports: getting production data into a roll database (docs/nosql/preview-environments.md,
//! "Get production data in").
//!
//! **`cql://user:pass@host:port[?ssl=true]`** — a live import over CQL that needs only a
//! read-only role on the source:
//!
//! 1. The source's keyspaces (`--keyspaces`, or every non-system one) are recreated from its
//!    server-side `DESCRIBE KEYSPACE` (types, tables, indexes, views, functions).
//! 2. Each table is read by parallel token-range scans, `SELECT … , WRITETIME(c), TTL(c) …
//!    WHERE token(pk) > ? AND token(pk) <= ?`, page by page, and written into the database
//!    through the engine with the original write times and remaining TTLs: cells sharing a
//!    (timestamp, TTL) are one `INSERT … USING TIMESTAMP ? AND TTL ?`, static cells are written
//!    once per partition, counters are added.
//! 3. The database is flushed: every run is a new manifest version, so branches can be taken
//!    from any refresh. With `every`, the import runs again on that schedule; a refresh empties
//!    the imported tables first, so rows deleted at the source go too.
//!
//! What CQL can't read back exactly: the write time and TTL of non-frozen collections and UDTs
//! (written with the row's newest write time and no TTL) and of row markers (rows whose
//! columns are all null get the import's start time).
//!
//! **`s3://bucket/path`** (and `file:///path`): SSTable backups, in [`crate::import_sstables`].

use crate::control::{Control, ControlError, Result, new_import_id, now};
use crate::store::{Import, NosqlDb};
use bytes::Bytes;
use futures::StreamExt;
use nosql_engine as eng;
use nosql_proto::client::{Client, ClientConfig, TlsConfig};
use nosql_proto::{QueryParams, Value as Wire};
use nosql_types::Dialect;
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// `POST /databases/{id}/imports`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ImportReq {
    pub from: String,
    #[serde(default)]
    pub keyspaces: Vec<String>,
    /// Refresh interval in seconds.
    pub every: Option<i64>,
}

/// A parsed `cql://` source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub addr: String,
    pub host: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub tls: bool,
}

impl Source {
    pub fn parse(from: &str) -> std::result::Result<Source, String> {
        let u = url::Url::parse(from).map_err(|e| format!("bad source URL: {e}"))?;
        if u.scheme() != "cql" {
            return Err(format!("unsupported source scheme {}:// (cql://, s3:// or file://)", u.scheme()));
        }
        let host = u.host_str().filter(|h| !h.is_empty()).ok_or("the source URL has no host")?.trim_matches(['[', ']']).to_string();
        let port = u.port().unwrap_or(9042);
        let dec = |s: &str| urlencoding::decode(s).map(|c| c.into_owned()).map_err(|_| "bad percent-encoding in the source URL".to_string());
        let user = if u.username().is_empty() { None } else { Some(dec(u.username())?) };
        let password = u.password().map(dec).transpose()?;
        let tls = u.query_pairs().any(|(k, v)| (k == "ssl" || k == "tls") && (v == "true" || v == "1"));
        let addr = if host.contains(':') { format!("[{host}]:{port}") } else { format!("{host}:{port}") };
        Ok(Source { addr, host, user, password, tls })
    }

    pub async fn connect(&self) -> anyhow::Result<Client> {
        let mut cfg = ClientConfig::new(self.addr.clone());
        cfg.request_timeout = Some(std::time::Duration::from_secs(120));
        if let (Some(u), Some(p)) = (&self.user, &self.password) {
            cfg = cfg.credentials(u.clone(), p.clone());
        }
        if self.tls {
            cfg = cfg.tls(TlsConfig::native_roots(self.host.clone())?);
        }
        Ok(Client::connect(cfg).await?)
    }
}

/// System keyspaces of either product (never imported).
pub fn is_system_keyspace(k: &str) -> bool {
    k.starts_with("system") || k == "dse_system" || k == "audit"
}

fn text(c: &Option<Bytes>) -> String {
    c.as_ref().map(|b| String::from_utf8_lossy(b).into_owned()).unwrap_or_default()
}

fn int(c: &Option<Bytes>) -> i32 {
    match c.as_deref() {
        Some(b) if b.len() == 4 => i32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        _ => -1,
    }
}

/// A table's columns as the import reads and writes them.
#[derive(Clone, Debug, Default)]
pub struct TableShape {
    pub keyspace: String,
    pub name: String,
    pub partition_key: Vec<String>,
    pub clustering: Vec<String>,
    pub statics: Vec<String>,
    pub regulars: Vec<String>,
    /// Non-frozen collections and UDTs (no WRITETIME/TTL per column).
    pub multicell: HashSet<String>,
    pub counter: bool,
}

/// Whether `ty` (as `system_schema.columns.type` prints it) is a multi-cell column.
pub fn is_multicell(ty: &str, udts: &HashSet<String>) -> bool {
    let t = ty.trim();
    if t.starts_with("frozen<") {
        return false;
    }
    t.starts_with("list<") || t.starts_with("set<") || t.starts_with("map<") || udts.contains(t) || udts.contains(t.trim_matches('"'))
}

impl TableShape {
    /// `(column, kind, position, type)` rows of `system_schema.columns`.
    pub fn from_columns(keyspace: &str, name: &str, cols: &[(String, String, i32, String)], udts: &HashSet<String>) -> TableShape {
        let mut s = TableShape { keyspace: keyspace.into(), name: name.into(), ..Default::default() };
        let mut pk: Vec<(i32, String)> = vec![];
        let mut ck: Vec<(i32, String)> = vec![];
        for (c, kind, pos, ty) in cols {
            match kind.as_str() {
                "partition_key" => pk.push((*pos, c.clone())),
                "clustering" => ck.push((*pos, c.clone())),
                "static" => s.statics.push(c.clone()),
                _ => s.regulars.push(c.clone()),
            }
            if ty == "counter" {
                s.counter = true;
            }
            if kind != "partition_key" && kind != "clustering" && is_multicell(ty, udts) {
                s.multicell.insert(c.clone());
            }
        }
        pk.sort();
        ck.sort();
        s.partition_key = pk.into_iter().map(|x| x.1).collect();
        s.clustering = ck.into_iter().map(|x| x.1).collect();
        s.statics.sort();
        s.regulars.sort();
        s
    }

    fn timed(&self, c: &str) -> bool {
        !self.counter && !self.multicell.contains(c)
    }

    /// Selected columns: keys, statics, regulars, then `WRITETIME(c), TTL(c)` of every timed
    /// static and regular column.
    pub fn select(&self, d: Dialect) -> String {
        let q = |c: &str| nosql_types::quote_identifier(c, d).into_owned();
        let pk: Vec<String> = self.partition_key.iter().map(|c| q(c)).collect();
        format!("{} WHERE token({pk}) > ? AND token({pk}) <= ?", self.select_from(d), pk = pk.join(", "))
    }

    /// The same columns as [`TableShape::select`], of one partition (`pk = ?` for each
    /// partition key column).
    pub fn select_partition(&self, d: Dialect) -> String {
        let q = |c: &str| nosql_types::quote_identifier(c, d).into_owned();
        let wh: Vec<String> = self.partition_key.iter().map(|c| format!("{} = ?", q(c))).collect();
        format!("{} WHERE {}", self.select_from(d), wh.join(" AND "))
    }

    fn select_from(&self, d: Dialect) -> String {
        let q = |c: &str| nosql_types::quote_identifier(c, d).into_owned();
        let mut cols: Vec<String> = self.partition_key.iter().chain(&self.clustering).chain(&self.statics).chain(&self.regulars).map(|c| q(c)).collect();
        for c in self.statics.iter().chain(&self.regulars).filter(|c| self.timed(c)) {
            cols.push(format!("WRITETIME({0}), TTL({0})", q(c)));
        }
        format!("SELECT {} FROM {}.{}", cols.join(", "), q(&self.keyspace), q(&self.name))
    }

    /// `SELECT <clustering> … WHERE <every key column> = ?`: whether a clustering row exists.
    pub fn select_row(&self, d: Dialect) -> String {
        let q = |c: &str| nosql_types::quote_identifier(c, d).into_owned();
        let wh: Vec<String> = self.partition_key.iter().chain(&self.clustering).map(|c| format!("{} = ?", q(c))).collect();
        let first = self.clustering.first().or(self.partition_key.first()).map(|c| q(c)).unwrap_or_default();
        format!("SELECT {first} FROM {}.{} WHERE {}", q(&self.keyspace), q(&self.name), wh.join(" AND "))
    }

    /// A row with its full primary key and no static or regular value: either a row with only
    /// its marker, or (ScyllaDB 2026.3 range scans) a static-only partition shown with the
    /// clustering key of the row before it.
    pub fn key_only(&self, row: &[Option<Bytes>]) -> bool {
        let nk = self.partition_key.len() + self.clustering.len();
        let plain = nk + self.statics.len() + self.regulars.len();
        !self.clustering.is_empty() && row.len() >= plain && row[self.partition_key.len()..nk].iter().all(Option::is_some) && row[nk..plain].iter().skip(self.statics.len()).all(Option::is_none)
    }
}

/// `n` contiguous token ranges `(start, end]` covering the ring (Murmur3 never yields `MIN`).
pub fn token_ranges(n: usize) -> Vec<(i64, i64)> {
    let n = n.max(1) as i128;
    let (lo, hi) = (i64::MIN as i128, i64::MAX as i128);
    let step = (hi - lo) / n;
    (0..n).map(|i| ((lo + i * step) as i64, if i == n - 1 { hi as i64 } else { (lo + (i + 1) * step) as i64 })).collect()
}

/// One write the import makes: a statement and its values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Write {
    pub cql: String,
    pub values: Vec<Bytes>,
}

/// The writes reproducing one source row (`row` in [`TableShape::select`]'s column order).
/// `partition_start`: the first row of its partition in this scan (statics are written once).
pub fn writes_for(shape: &TableShape, row: &[Option<Bytes>], partition_start: bool, import_ts: i64, d: Dialect) -> Vec<Write> {
    let q = |c: &str| nosql_types::quote_identifier(c, d).into_owned();
    let table = format!("{}.{}", q(&shape.keyspace), q(&shape.name));
    let nk = shape.partition_key.len() + shape.clustering.len();
    let ns = shape.statics.len();
    let base: Vec<&String> = shape.partition_key.iter().chain(&shape.clustering).chain(&shape.statics).chain(&shape.regulars).collect();
    let cell = |i: usize| row.get(i).cloned().flatten();
    // WRITETIME/TTL pairs follow the plain columns, for timed columns in order.
    let mut timing: HashMap<&str, (Option<Bytes>, Option<Bytes>)> = HashMap::new();
    let mut at = base.len();
    for c in shape.statics.iter().chain(&shape.regulars).filter(|c| shape.timed(c)) {
        timing.insert(c, (cell(at), cell(at + 1)));
        at += 2;
    }
    let pk: Vec<(String, Bytes)> = shape.partition_key.iter().enumerate().filter_map(|(i, c)| cell(i).map(|v| (c.clone(), v))).collect();
    if pk.len() != shape.partition_key.len() {
        return vec![];
    }
    let ck: Vec<(String, Bytes)> =
        shape.clustering.iter().enumerate().filter_map(|(i, c)| cell(shape.partition_key.len() + i).map(|v| (c.clone(), v))).collect();
    let has_row = ck.len() == shape.clustering.len();
    let mut out = vec![];

    if shape.counter {
        // Counters: add each value (no write time or TTL in CQL).
        let set: Vec<(String, Bytes)> = shape.statics.iter().enumerate().filter(|_| partition_start).filter_map(|(i, c)| cell(nk + i).map(|v| (c.clone(), v))).collect();
        if !set.is_empty() {
            let assigns: Vec<String> = set.iter().map(|(c, _)| format!("{0} = {0} + ?", q(c))).collect();
            let wh: Vec<String> = pk.iter().map(|(c, _)| format!("{} = ?", q(c))).collect();
            out.push(Write { cql: format!("UPDATE {table} SET {} WHERE {}", assigns.join(", "), wh.join(" AND ")), values: set.iter().chain(&pk).map(|x| x.1.clone()).collect() });
        }
        if has_row {
            let set: Vec<(String, Bytes)> = shape.regulars.iter().enumerate().filter_map(|(i, c)| cell(nk + ns + i).map(|v| (c.clone(), v))).collect();
            if !set.is_empty() {
                let assigns: Vec<String> = set.iter().map(|(c, _)| format!("{0} = {0} + ?", q(c))).collect();
                let wh: Vec<String> = pk.iter().chain(&ck).map(|(c, _)| format!("{} = ?", q(c))).collect();
                out.push(Write {
                    cql: format!("UPDATE {table} SET {} WHERE {}", assigns.join(", "), wh.join(" AND ")),
                    values: set.iter().chain(&pk).chain(&ck).map(|x| x.1.clone()).collect(),
                });
            }
        }
        return out;
    }

    let ts_bytes = |t: i64| Bytes::copy_from_slice(&t.to_be_bytes());
    let zero_ttl = Bytes::copy_from_slice(&0i32.to_be_bytes());
    // Groups of (timestamp, ttl) → columns with values.
    let insert = |keys: &[(String, Bytes)], cols: &[(String, Bytes)], ts: Bytes, ttl: Bytes| {
        let names: Vec<String> = keys.iter().chain(cols).map(|(c, _)| q(c)).collect();
        let marks = vec!["?"; names.len()].join(", ");
        let mut values: Vec<Bytes> = keys.iter().chain(cols).map(|x| x.1.clone()).collect();
        values.push(ts);
        values.push(ttl);
        Write { cql: format!("INSERT INTO {table} ({}) VALUES ({marks}) USING TIMESTAMP ? AND TTL ?", names.join(", ")), values }
    };
    let grouped = |cols: &[(usize, &String)]| {
        let mut groups: BTreeMap<(Bytes, Bytes), Vec<(String, Bytes)>> = BTreeMap::new();
        let mut untimed: Vec<(String, Bytes)> = vec![];
        for (i, c) in cols {
            let Some(v) = cell(*i) else { continue };
            match timing.get(c.as_str()) {
                Some((Some(wt), ttl)) => groups.entry((wt.clone(), ttl.clone().unwrap_or_else(|| zero_ttl.clone()))).or_default().push(((*c).clone(), v)),
                _ => untimed.push(((*c).clone(), v)),
            }
        }
        // Non-frozen collections join the newest group (or the import's time).
        if !untimed.is_empty() {
            let newest = groups.keys().max_by_key(|(wt, _)| <[u8; 8]>::try_from(wt.as_ref()).map(i64::from_be_bytes).unwrap_or(i64::MIN)).cloned();
            match newest {
                Some(k) => groups.get_mut(&k).into_iter().for_each(|g| g.append(&mut untimed)),
                None => {
                    groups.insert((ts_bytes(import_ts), zero_ttl.clone()), untimed);
                }
            }
        }
        groups
    };
    if partition_start && ns > 0 {
        let cols: Vec<(usize, &String)> = shape.statics.iter().enumerate().map(|(i, c)| (nk + i, c)).collect();
        for ((ts, ttl), cs) in grouped(&cols) {
            out.push(insert(&pk, &cs, ts, ttl));
        }
    }
    if has_row {
        let keys: Vec<(String, Bytes)> = pk.iter().chain(&ck).cloned().collect();
        let cols: Vec<(usize, &String)> = shape.regulars.iter().enumerate().map(|(i, c)| (nk + ns + i, c)).collect();
        let groups = grouped(&cols);
        if groups.is_empty() && (!shape.clustering.is_empty() || ns == 0) {
            // Only the primary key: the row marker.
            out.push(insert(&keys, &[], ts_bytes(import_ts), zero_ttl.clone()));
        }
        for ((ts, ttl), cs) in groups {
            out.push(insert(&keys, &cs, ts, ttl));
        }
    }
    out
}

/// Executes writes through one engine session, preparing each statement text once.
struct Writer {
    session: eng::Session,
    prepared: HashMap<String, Bytes>,
}

impl Writer {
    async fn run(&mut self, w: &Write) -> anyhow::Result<()> {
        for attempt in 0..2 {
            let id = match self.prepared.get(&w.cql) {
                Some(id) => id.clone(),
                None => {
                    let p = self.session.prepare(&w.cql, None).await.map_err(|e| anyhow::anyhow!("{}: {}", w.cql, e.message))?;
                    self.prepared.insert(w.cql.clone(), p.id.clone());
                    p.id
                }
            };
            let params = eng::QueryParams { values: w.values.iter().cloned().map(eng::response::Param::Set).collect(), ..Default::default() };
            match self.session.execute(&id, params).await {
                Ok(_) => return Ok(()),
                Err(e) if e.code == eng::ErrorCode::Unprepared && attempt == 0 => {
                    self.prepared.remove(&w.cql);
                }
                Err(e) => anyhow::bail!("{}: {}", w.cql, e.message),
            }
        }
        anyhow::bail!("{}: statement could not be prepared", w.cql)
    }
}

impl Control {
    pub fn imports(&self, id: &str) -> Result<Vec<Import>> {
        let d = self.get(id)?;
        Ok(self.store.imports(&d.id)?)
    }

    /// Starts an import (it runs in the background; `GET …/imports` shows its progress).
    pub fn start_import(self: &Arc<Self>, id: &str, r: ImportReq) -> Result<Import> {
        let d = self.get(id)?;
        let from = r.from.trim().to_string();
        if crate::import_sstables::is_backup_url(&from) {
            crate::import_sstables::validate(&from)?;
        } else {
            Source::parse(&from).map_err(ControlError::Invalid)?;
        }
        if let Some(e) = r.every
            && e < 60
        {
            return Err(ControlError::Invalid("every must be at least 60 seconds".into()));
        }
        for k in &r.keyspaces {
            if k.is_empty() || is_system_keyspace(k) {
                return Err(ControlError::Invalid(format!("can't import keyspace {k:?}")));
            }
        }
        if d.read_only {
            return Err(ControlError::Conflict(format!("{} is read-only", d.name)));
        }
        if self.store.imports(&d.id)?.iter().any(|i| i.state == "running" || i.state == "queued") {
            return Err(ControlError::Conflict(format!("an import into {} is already running", d.name)));
        }
        let imp = Import {
            id: new_import_id(),
            db_id: d.id.clone(),
            source: from,
            keyspaces: r.keyspaces,
            every: r.every,
            state: "queued".into(),
            rows: 0,
            tables_done: 0,
            tables_total: 0,
            error: None,
            runs: 0,
            started_at: None,
            finished_at: None,
            next_run_at: None,
            created_at: now(),
        };
        self.store.insert_import(&imp)?;
        self.spawn_import(imp.id.clone());
        Ok(imp)
    }

    /// Runs scheduled imports that are due.
    pub(crate) fn run_due_imports(self: Arc<Self>) -> anyhow::Result<()> {
        for i in self.store.due_imports(now())? {
            self.spawn_import(i.id);
        }
        Ok(())
    }

    fn spawn_import(self: &Arc<Self>, import_id: String) {
        if !self.running_imports.lock().unwrap_or_else(|e| e.into_inner()).insert(import_id.clone()) {
            return;
        }
        let c = self.clone();
        tokio::spawn(async move {
            if let Err(e) = c.run_import(&import_id).await {
                tracing::warn!(import = import_id, error = %e, "nosql import failed");
            }
            c.running_imports.lock().unwrap_or_else(|e| e.into_inner()).remove(&import_id);
        });
    }

    /// One run of an import (also called by tests, which wait for it).
    pub async fn run_import(&self, import_id: &str) -> anyhow::Result<()> {
        let Some(mut imp) = self.store.import(import_id)? else { return Ok(()) };
        let Some(d) = self.store.db(&imp.db_id)? else { return Ok(()) };
        imp.state = "running".into();
        imp.started_at = Some(now());
        imp.finished_at = None;
        imp.error = None;
        imp.rows = 0;
        imp.tables_done = 0;
        imp.next_run_at = None;
        self.store.update_import(&imp)?;
        let rows = Arc::new(AtomicI64::new(0));
        let result = if crate::import_sstables::is_backup_url(&imp.source) { self.import_sstables(&d, &mut imp, &rows).await } else { self.import_cql(&d, &mut imp, &rows).await };
        imp.rows = rows.load(Ordering::Relaxed);
        imp.finished_at = Some(now());
        imp.next_run_at = imp.every.map(|e| now() + e);
        match &result {
            Ok(warnings) => {
                imp.state = "done".into();
                imp.runs += 1;
                imp.error = (!warnings.is_empty()).then(|| format!("completed with warnings: {}", warnings.join("; ")));
            }
            Err(e) => {
                imp.state = "failed".into();
                imp.error = Some(format!("{e:#}"));
            }
        }
        // The database may have been deleted meanwhile (the row is gone then).
        if self.store.import(import_id)?.is_some() {
            self.store.update_import(&imp)?;
        }
        result.map(|_| ())
    }

    /// Returns warnings (statements of the source schema the database didn't take that
    /// don't hold data: indexes, views, functions).
    async fn import_cql(&self, d: &NosqlDb, imp: &mut Import, rows: &Arc<AtomicI64>) -> anyhow::Result<Vec<String>> {
        let src = Source::parse(&imp.source).map_err(|e| anyhow::anyhow!(e))?;
        let client = src.connect().await.map_err(|e| anyhow::anyhow!("connecting to {}: {e}", src.addr))?;
        let source_product = if client.supported().has("SCYLLA_SHARD") { "ScyllaDB" } else { "Cassandra" };
        let lease = self.lease(d).await?;
        let dialect = lease.db().dialect();
        let import_ts = eng::Database::timestamp(lease.db());
        let mut warnings = vec![];

        // Keyspaces.
        let all = client.pages("SELECT keyspace_name FROM system_schema.keyspaces", QueryParams { page_size: Some(1000), ..Default::default() }).collect().await?;
        let all: Vec<String> = all.iter().map(|r| text(&r[0])).collect();
        let keyspaces: Vec<String> = if imp.keyspaces.is_empty() {
            let mut k: Vec<String> = all.into_iter().filter(|k| !is_system_keyspace(k)).collect();
            k.sort();
            k
        } else {
            for k in &imp.keyspaces {
                if !all.contains(k) {
                    anyhow::bail!("the source has no keyspace {k}");
                }
            }
            imp.keyspaces.clone()
        };

        // Schema and table shapes.
        let mut session = Self::session(&lease, d, "roll import");
        let mut shapes = vec![];
        for ks in &keyspaces {
            let qks = nosql_types::quote_identifier(ks, dialect).into_owned();
            let desc = client.query(&format!("DESCRIBE KEYSPACE {qks}"), QueryParams::default()).await.map_err(|e| anyhow::anyhow!("DESCRIBE KEYSPACE {ks} on the source: {e}"))?;
            let nosql_proto::ResultBody::Rows(desc) = desc else { anyhow::bail!("DESCRIBE KEYSPACE {ks} returned no rows") };
            let stmt_col = desc.metadata.columns.iter().position(|c| c.name == "create_statement").unwrap_or(3);
            let type_col = desc.metadata.columns.iter().position(|c| c.name == "type").unwrap_or(1);
            for row in desc.to_vec() {
                let stmt = text(&row.get(stmt_col).cloned().flatten());
                let kind = text(&row.get(type_col).cloned().flatten());
                if stmt.trim().is_empty() {
                    continue;
                }
                if let Err(e) = session.query(&stmt, eng::QueryParams::default()).await {
                    let msg = e.message.to_ascii_lowercase();
                    if e.code == eng::ErrorCode::AlreadyExists || msg.contains("already exist") {
                        continue;
                    }
                    let essential = matches!(kind.as_str(), "keyspace" | "type" | "table");
                    let what = format!("{kind} from {ks}: {} (the source is {source_product}, this database's dialect is {})", e.message, dialect.name());
                    if essential {
                        anyhow::bail!("recreating {what}");
                    }
                    warnings.push(format!("skipped {what}"));
                }
            }
            let udts: HashSet<String> = client
                .pages("SELECT type_name FROM system_schema.types WHERE keyspace_name = ?", QueryParams { values: vec![Wire::Set(Bytes::from(ks.clone()))], page_size: Some(1000), ..Default::default() })
                .collect()
                .await?
                .iter()
                .map(|r| text(&r[0]))
                .collect();
            let tables = client
                .pages("SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?", QueryParams { values: vec![Wire::Set(Bytes::from(ks.clone()))], page_size: Some(1000), ..Default::default() })
                .collect()
                .await?;
            for t in tables.iter().map(|r| text(&r[0])).filter(|t| !t.ends_with("_scylla_cdc_log")) {
                let cols = client
                    .pages(
                        "SELECT column_name, kind, position, type FROM system_schema.columns WHERE keyspace_name = ? AND table_name = ?",
                        QueryParams { values: vec![Wire::Set(Bytes::from(ks.clone())), Wire::Set(Bytes::from(t.clone()))], page_size: Some(1000), ..Default::default() },
                    )
                    .collect()
                    .await?;
                let cols: Vec<(String, String, i32, String)> = cols.iter().map(|r| (text(&r[0]), text(&r[1]), int(&r[2]), text(&r[3]))).collect();
                shapes.push(TableShape::from_columns(ks, &t, &cols, &udts));
            }
        }
        imp.tables_total = shapes.len() as i64;
        self.store.update_import(imp)?;

        // A refresh replaces the previous run's rows.
        if imp.runs > 0 {
            for s in &shapes {
                let q = |c: &str| nosql_types::quote_identifier(c, dialect).into_owned();
                session.query(&format!("TRUNCATE {}.{}", q(&s.keyspace), q(&s.name)), eng::QueryParams::default()).await.map_err(|e| anyhow::anyhow!("TRUNCATE {}.{}: {}", s.keyspace, s.name, e.message))?;
            }
        }
        drop(session);

        // Data.
        let parallelism = self.cfg.import_parallelism.max(1);
        for shape in &shapes {
            let select = shape.select(dialect);
            let ranges = token_ranges(parallelism * 4);
            let results: Vec<anyhow::Result<()>> = futures::stream::iter(ranges.into_iter().map(|(lo, hi)| {
                let client = client.clone();
                let select = select.clone();
                let rows = rows.clone();
                let mut w = Writer { session: Self::session(&lease, d, "roll import"), prepared: HashMap::new() };
                async move {
                    let params = QueryParams {
                        values: vec![Wire::Set(Bytes::copy_from_slice(&lo.to_be_bytes())), Wire::Set(Bytes::copy_from_slice(&hi.to_be_bytes()))],
                        page_size: Some(1000),
                        ..Default::default()
                    };
                    let mut pager = client.pages(&select, params);
                    let mut last_pk: Option<Vec<Option<Bytes>>> = None;
                    let npk = shape.partition_key.len();
                    while let Some(page) = pager.next_page().await.map_err(|e| anyhow::anyhow!("reading {}.{}: {e}", shape.keyspace, shape.name))? {
                        for mut row in page.to_vec() {
                            if shape.key_only(&row) {
                                // Check the row exists (single-partition reads show static-only
                                // partitions correctly on both references).
                                let keys = shape.partition_key.len() + shape.clustering.len();
                                let values = row[..keys].iter().map(|c| c.clone().map_or(Wire::Null, Wire::Set)).collect();
                                let found = client.query(&shape.select_row(dialect), QueryParams { values, ..Default::default() }).await.map_err(|e| anyhow::anyhow!("reading {}.{}: {e}", shape.keyspace, shape.name))?;
                                if matches!(found, nosql_proto::ResultBody::Rows(r) if r.row_count == 0) {
                                    for c in &mut row[npk..keys] {
                                        *c = None;
                                    }
                                }
                            }
                            let pk = row.get(..npk).map(|s| s.to_vec());
                            let start = pk != last_pk;
                            last_pk = pk;
                            for write in writes_for(shape, &row, start, import_ts, dialect) {
                                w.run(&write).await?;
                            }
                            rows.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    anyhow::Ok(())
                }
            }))
            .buffer_unordered(parallelism)
            .collect()
            .await;
            for r in results {
                r?;
            }
            imp.tables_done += 1;
            imp.rows = rows.load(Ordering::Relaxed);
            self.store.update_import(imp)?;
        }
        // Every run is a new manifest version.
        lease.db().flush().await?;
        lease.db().store().write_manifest(true).await?;
        Ok(warnings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sources() {
        let s = Source::parse("cql://reader:p%40ss@db.example.com:19042?ssl=true").unwrap();
        assert_eq!(s, Source { addr: "db.example.com:19042".into(), host: "db.example.com".into(), user: Some("reader".into()), password: Some("p@ss".into()), tls: true });
        let s = Source::parse("cql://127.0.0.1").unwrap();
        assert_eq!((s.addr.as_str(), s.user, s.tls), ("127.0.0.1:9042", None, false));
        assert_eq!(Source::parse("cql://[::1]:9042").unwrap().addr, "[::1]:9042");
        assert!(Source::parse("http://x").unwrap_err().contains("unsupported source scheme"));
        assert!(Source::parse("cql://").is_err());
        assert!(Source::parse("not a url").is_err());
    }

    #[test]
    fn ranges_cover_the_ring() {
        let r = token_ranges(7);
        assert_eq!(r.len(), 7);
        assert_eq!(r[0].0, i64::MIN);
        assert_eq!(r[6].1, i64::MAX);
        for w in r.windows(2) {
            assert_eq!(w[0].1, w[1].0);
            assert!(w[0].0 < w[0].1);
        }
        assert_eq!(token_ranges(0), vec![(i64::MIN, i64::MAX)]);
    }

    fn shape() -> TableShape {
        let udts: HashSet<String> = ["addr".to_string()].into();
        let cols = vec![
            ("k".into(), "partition_key".into(), 0, "int".into()),
            ("c".into(), "clustering".into(), 0, "text".into()),
            ("s".into(), "static".into(), -1, "text".into()),
            ("a".into(), "regular".into(), -1, "int".into()),
            ("b".into(), "regular".into(), -1, "text".into()),
            ("l".into(), "regular".into(), -1, "list<int>".into()),
            ("f".into(), "regular".into(), -1, "frozen<list<int>>".into()),
            ("u".into(), "regular".into(), -1, "addr".into()),
        ];
        TableShape::from_columns("ks", "t", &cols, &udts)
    }

    #[test]
    fn shapes_and_selects() {
        let s = shape();
        assert_eq!(s.partition_key, ["k"]);
        assert_eq!(s.clustering, ["c"]);
        assert_eq!(s.statics, ["s"]);
        assert_eq!(s.regulars, ["a", "b", "f", "l", "u"]);
        assert_eq!(s.multicell, HashSet::from(["l".to_string(), "u".to_string()]));
        assert_eq!(
            s.select(Dialect::Scylla),
            "SELECT k, c, s, a, b, f, l, u, WRITETIME(s), TTL(s), WRITETIME(a), TTL(a), WRITETIME(b), TTL(b), WRITETIME(f), TTL(f) FROM ks.t WHERE token(k) > ? AND token(k) <= ?"
        );
        assert!(!is_multicell("frozen<map<int, int>>", &HashSet::new()));
        assert!(is_multicell("set<text>", &HashSet::new()));
    }

    fn b(v: &[u8]) -> Option<Bytes> {
        Some(Bytes::copy_from_slice(v))
    }

    fn ts(t: i64) -> Option<Bytes> {
        b(&t.to_be_bytes())
    }

    fn ttl(t: i32) -> Option<Bytes> {
        b(&t.to_be_bytes())
    }

    #[test]
    fn writes_group_by_time() {
        let s = shape();
        // k, c, s, a, b, f, l, u, wt(s), ttl(s), wt(a), ttl(a), wt(b), ttl(b), wt(f), ttl(f)
        let row = vec![b(&[0, 0, 0, 1]), b(b"x"), b(b"st"), b(&[0, 0, 0, 2]), b(b"bee"), None, b(b"L"), None, ts(5), None, ts(10), ttl(100), ts(20), None, None, None];
        let w = writes_for(&s, &row, true, 99, Dialect::Scylla);
        assert_eq!(w.len(), 3, "{w:#?}");
        assert_eq!(w[0].cql, "INSERT INTO ks.t (k, s) VALUES (?, ?) USING TIMESTAMP ? AND TTL ?");
        assert_eq!(w[0].values[2], ts(5).unwrap());
        assert_eq!(w[0].values[3], ttl(0).unwrap());
        // a (10, 100); b (20, none) with the list l (newest group).
        assert_eq!(w[1].cql, "INSERT INTO ks.t (k, c, a) VALUES (?, ?, ?) USING TIMESTAMP ? AND TTL ?");
        assert_eq!(w[1].values[4], ttl(100).unwrap());
        assert_eq!(w[2].cql, "INSERT INTO ks.t (k, c, b, l) VALUES (?, ?, ?, ?) USING TIMESTAMP ? AND TTL ?");
        assert_eq!(w[2].values[4], ts(20).unwrap());
        // The next row of the partition doesn't repeat the static.
        assert_eq!(writes_for(&s, &row, false, 99, Dialect::Scylla).len(), 2);
        // A row with only its key: the marker at the import's time.
        let bare = vec![b(&[0, 0, 0, 1]), b(b"y"), None, None, None, None, None, None, None, None, None, None, None, None, None, None];
        let w = writes_for(&s, &bare, false, 99, Dialect::Scylla);
        assert_eq!(w, vec![Write { cql: "INSERT INTO ks.t (k, c) VALUES (?, ?) USING TIMESTAMP ? AND TTL ?".into(), values: vec![b(&[0, 0, 0, 1]).unwrap(), Bytes::from_static(b"y"), ts(99).unwrap(), ttl(0).unwrap()] }]);
        // A static-only partition (no clustering row).
        let st = vec![b(&[0, 0, 0, 3]), None, b(b"st"), None, None, None, None, None, ts(5), ttl(7), None, None, None, None, None, None];
        let w = writes_for(&s, &st, true, 99, Dialect::Scylla);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].values[3], ttl(7).unwrap());
        // A row without its partition key is skipped.
        assert!(writes_for(&s, &[None], true, 99, Dialect::Scylla).is_empty());
    }

    #[test]
    fn key_only_rows_and_point_selects() {
        let s = shape();
        let mut row = vec![b(&[0, 0, 0, 1]), b(b"x")];
        row.resize(16, None);
        assert!(s.key_only(&row));
        row[2] = b(b"static");
        assert!(s.key_only(&row), "statics don't make a clustering row");
        row[3] = b(&[0, 0, 0, 2]);
        assert!(!s.key_only(&row));
        let mut no_ck = vec![b(&[0, 0, 0, 1]), None];
        no_ck.resize(16, None);
        assert!(!s.key_only(&no_ck));
        assert!(s.select_partition(Dialect::Scylla).ends_with("FROM ks.t WHERE k = ?"));
        assert_eq!(s.select_row(Dialect::Scylla), "SELECT c FROM ks.t WHERE k = ? AND c = ?");
        assert_eq!(s.select_row(Dialect::Cassandra), "SELECT c FROM ks.t WHERE k = ? AND c = ?");
    }

    #[test]
    fn counter_writes() {
        let cols = vec![("k".into(), "partition_key".into(), 0, "int".into()), ("n".into(), "regular".into(), -1, "counter".into()), ("m".into(), "regular".into(), -1, "counter".into())];
        let s = TableShape::from_columns("ks", "c", &cols, &HashSet::new());
        assert!(s.counter);
        assert_eq!(s.select(Dialect::Cassandra), "SELECT k, m, n FROM ks.c WHERE token(k) > ? AND token(k) <= ?");
        let w = writes_for(&s, &[b(&[0, 0, 0, 1]), None, b(&5i64.to_be_bytes())], true, 0, Dialect::Cassandra);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].cql, "UPDATE ks.c SET n = n + ? WHERE k = ?");
    }
}
