//! Imports of SSTable backups: `s3://bucket/path` (and `file:///path`), with no source cluster
//! (docs/nosql/preview-environments.md, "Restore from backups").
//!
//! 1. The backup is found in a listing of its location ([`nosql_sstable_import::layout`]):
//!    `nodetool snapshot` directories of Cassandra or ScyllaDB (the newest tag, or `?tag=`),
//!    ScyllaDB Manager backups (`?tag=`, `?cluster=`), Medusa backups (`?backup=`), or copies of
//!    data directories. Every node's SSTables of a table are imported: replicas reconcile.
//! 2. The schema is recreated from the backup's CQL (`schema.cql`, Medusa's schema, ScyllaDB
//!    Manager's schema file): keyspaces (a keyspace without a definition gets
//!    `NetworkTopologyStrategy` with `replication_factor` 1), user types (also rebuilt from the
//!    SSTables' own type names, which ScyllaDB's `schema.cql` lacks), tables and their dropped
//!    columns. Options the database's dialect refuses are dropped with a warning. A table with no
//!    CQL at all is recreated from its `Scylla.db` schema.
//! 3. Every SSTable is converted into roll SSTables of its table ([`nosql_sstable_import::load`]:
//!    write times, TTLs, tombstones, counters, collections, statics and range tombstones as
//!    stored), uploaded, and all tables' new SSTables are swapped in with one manifest version
//!    ([`nosql_storage::ingest`]), so a branch can be taken right after. A refresh (`every`)
//!    replaces the previous run's data.
//! 4. Indexes and materialized views are created after the data, so the engine builds them from
//!    it (a refresh drops and recreates them). Their own SSTables and CDC logs aren't imported.

use crate::control::{Control, Result as CResult};
use crate::store::{Import, NosqlDb};
use anyhow::{Context, Result, bail};
use futures::StreamExt;
use nosql_engine as eng;
use nosql_sstable_import as si;
use nosql_sstable_import::layout::{self, Backup, Kind, SchemaFile, TableFiles};
use nosql_sstable_import::load::{LoadOptions, Loaded, Progress, Target, TargetColumn};
use nosql_sstable_import::schema::{Stmt, StmtKind, classify, ordered, split, strip_options};
use nosql_sstable_import::source::Location;
use nosql_sstable_import::sstable::{SstFiles, Store};
use nosql_sstable_import::types::{SType, quote};
use nosql_storage::ingest::Ingested;
use nosql_types::{CqlType, Dialect};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// Whether `from` is a backup location this module imports.
pub fn is_backup_url(from: &str) -> bool {
    from.starts_with("s3://") || from.starts_with("file://")
}

/// Validate a backup URL (at request time).
pub fn check_url(from: &str) -> std::result::Result<(), String> {
    Location::parse(from).map(|_| ()).map_err(|e| format!("{e:#}"))
}

/// Concurrent SSTable conversions (each holds a read-ahead window, a chunk, a block and a
/// bloom filter: well under 32 MiB).
const MAX_PARALLEL: usize = 4;
/// Largest manifest or schema file read whole.
const MAX_META: u64 = 256 << 20;

/// The structure of an engine type as [`SType::shape`] spells it.
fn shape(t: &CqlType) -> String {
    if let Some(n) = t.native_name() {
        return n.to_string();
    }
    match t {
        CqlType::List { elem, .. } => format!("list<{}>", shape(elem)),
        CqlType::Set { elem, .. } => format!("set<{}>", shape(elem)),
        CqlType::Map { key, value, .. } => format!("map<{},{}>", shape(key), shape(value)),
        CqlType::Tuple(ts) => format!("tuple<{}>", ts.iter().map(shape).collect::<Vec<_>>().join(",")),
        CqlType::Udt(def, _) => def.name.to_ascii_lowercase(),
        CqlType::Vector { elem, dim } => format!("vector<{},{dim}>", shape(elem)),
        CqlType::Custom(c) => c.to_string(),
        _ => format!("{t:?}"),
    }
}

/// The import target of an engine table.
fn target(db: &eng::Database, t: &eng::schema::Table) -> Result<Target> {
    let storage = db.storage_table(t).map_err(|e| anyhow::anyhow!("{}", e.message))?;
    let mut columns = HashMap::new();
    for c in t.columns.iter().filter(|c| !c.is_primary_key()) {
        columns.insert(
            c.name.as_bytes().to_vec(),
            TargetColumn { id: c.id, is_static: c.is_static(), complex: c.ty.is_multicell(), counter: c.is_counter(), shape: Some(shape(&c.ty)), dropped_at: t.dropped_before(&c.name) },
        );
    }
    for (name, d) in &t.dropped {
        if t.column(name).is_none()
            && let Some(id) = t.column_ids.get(name)
        {
            columns.insert(
                name.as_bytes().to_vec(),
                TargetColumn { id: *id, is_static: d.kind == eng::schema::ColumnKind::Static, complex: d.ty.is_multicell(), counter: d.ty.is_counter(), shape: None, dropped_at: Some(d.dropped_at) },
            );
        }
    }
    let dialect = db.dialect();
    Ok(Target {
        keyspace: t.keyspace.clone(),
        table: t.name.clone(),
        storage,
        columns,
        clustering_columns: t.clustering.len(),
        counter: t.is_counter(),
        token: Arc::new(move |k: &[u8]| eng::storage::token_of(k, dialect)),
    })
}

async fn get(loc: &Location, key: &str, size: u64) -> Result<Vec<u8>> {
    if size > MAX_META {
        bail!("{key} is too large ({size} bytes)");
    }
    Ok(loc.objects.get(key).await?.ok_or_else(|| anyhow::anyhow!("{key} disappeared"))?.to_vec())
}

/// Find the backup in a listing.
async fn discover(loc: &Location, listing: &[(String, u64)]) -> Result<Backup> {
    let index: HashMap<&str, u64> = listing.iter().map(|(k, s)| (k.as_str(), *s)).collect();
    let plan = layout::plan(listing);
    let mut warnings = Vec::new();
    if !plan.manager_manifests.is_empty() {
        let chosen = layout::choose_manager(&plan.manager_manifests, loc.param("tag"), loc.param("cluster"))?;
        let mut with = Vec::new();
        for m in &chosen {
            let raw = get(loc, &m.key, index[m.key.as_str()]).await?;
            let json = si::tar::gunzip(&raw, MAX_META)?;
            let idx: layout::ManagerIndex = serde_json::from_slice(&json).with_context(|| format!("parsing {}", m.key))?;
            with.push((*m, idx));
        }
        let tables = layout::manager_tables(&with, &index)?;
        let schema = layout::manager_schema(&chosen, &index);
        if schema.is_empty() {
            warnings.push("the ScyllaDB Manager backup has no schema file (it is only uploaded when Manager has CQL credentials); tables were recreated from their SSTables".into());
        }
        return Ok(Backup { kind: Kind::ScyllaManager, tag: chosen.first().map(|m| m.tag.clone()), tables, schema, warnings });
    }
    if !plan.medusa.is_empty() {
        let chosen = layout::choose_medusa(&plan.medusa, loc.param("backup").or(loc.param("tag")))?;
        let mut with = Vec::new();
        let mut schema = Vec::new();
        for b in &chosen {
            let raw = get(loc, &b.manifest, index[b.manifest.as_str()]).await?;
            let v: serde_json::Value = serde_json::from_slice(&raw).with_context(|| format!("parsing {}", b.manifest))?;
            with.push((*b, v));
            let s = format!("{}{}/{}/meta/schema.cql", b.root, b.fqdn, b.name);
            if schema.is_empty() && index.contains_key(s.as_str()) {
                schema.push(SchemaFile::Cql(s));
            }
        }
        let tables = layout::medusa_tables(&with, &index)?;
        return Ok(Backup { kind: Kind::Medusa, tag: chosen.first().map(|b| b.name.clone()), tables, schema, warnings });
    }
    let mut created = HashMap::new();
    for (tag, manifests) in &plan.snapshot_tags {
        if let Some(m) = manifests.first()
            && let Ok(raw) = get(loc, m, index[m.as_str()]).await
            && let Some(t) = layout::snapshot_created(&raw)
        {
            created.insert(tag.clone(), t);
        }
    }
    let tag = layout::choose_snapshot_tag(&plan, loc.param("tag"), &created)?;
    if let Some(t) = &tag {
        for m in plan.snapshot_tags.get(t).into_iter().flatten() {
            let raw = get(loc, m, index[m.as_str()]).await?;
            let dir = &m[..m.len() - "manifest.json".len()];
            let missing = layout::snapshot_missing(&raw, dir, &index);
            if let Some(f) = missing.first() {
                bail!("the snapshot is incomplete: {f} (listed in {m}) is missing");
            }
        }
    }
    let tables = layout::snapshot_tables(listing, tag.as_deref())?;
    Ok(Backup { kind: if tag.is_some() { Kind::Snapshot } else { Kind::DataDirectory }, tag, tables, schema: vec![], warnings })
}

/// A table's first SSTable's statistics and ScyllaDB metadata.
type Meta = Option<(si::stats::Statistics, Option<si::scylla::ScyllaMetadata>)>;

/// [`Meta`] of a table (blocking reads).
fn first_metadata(store: &dyn Store, t: &TableFiles) -> Result<Meta> {
    let Some(s) = t.sstables.first() else { return Ok(None) };
    let get = |c: &str| -> Result<Option<bytes::Bytes>> {
        match s.components.get(c) {
            Some((k, _)) => store.get(k),
            None => Ok(None),
        }
    };
    let Some(raw) = get("Statistics.db")? else { return Ok(None) };
    let stats = si::stats::Statistics::parse(raw, &s.version).with_context(|| format!("{}: Statistics.db", s.data_key()))?;
    let scylla = get("Scylla.db")?.map(|r| si::scylla::ScyllaMetadata::parse(&r)).transpose()?;
    Ok(Some((stats, scylla)))
}

async fn statements(loc: &Location, backup: &mut Backup, store: &si::source::ObjectStore, keyspaces: &[String]) -> Result<Vec<Stmt>> {
    let mut texts: Vec<String> = Vec::new();
    for f in &backup.schema {
        match f {
            SchemaFile::Cql(k) => texts.extend(split(&String::from_utf8_lossy(&get(loc, k, 0).await?))),
            SchemaFile::ManagerJson(k) => {
                for s in layout::manager_json_schema(&get(loc, k, 0).await?)? {
                    texts.extend(split(&s));
                }
            }
            SchemaFile::ManagerTar(k) => {
                for s in layout::manager_tar_schema(&get(loc, k, 0).await?)? {
                    texts.extend(split(&s));
                }
            }
        }
    }
    for t in &backup.tables {
        if let Some(k) = t.schema_keys.first() {
            texts.extend(split(&String::from_utf8_lossy(&get(loc, k, 0).await?)));
        }
    }
    let mut stmts: Vec<Stmt> = texts.iter().map(|t| classify(t)).filter(|s| s.kind != StmtKind::Other).collect();
    // Tables the path doesn't name: their schema.cql's table, or their Scylla.db schema.
    let mut meta = Vec::new();
    for t in backup.tables.iter_mut() {
        let st = store.clone();
        let tf = t.clone();
        let m = tokio::task::spawn_blocking(move || first_metadata(&st, &tf)).await??;
        if t.keyspace.is_none() {
            let mut named = None;
            if let Some(k) = t.schema_keys.first() {
                let own: Vec<Stmt> = split(&String::from_utf8_lossy(&get(loc, k, 0).await?)).iter().map(|x| classify(x)).collect();
                named = own.iter().find(|s| s.kind == StmtKind::Table).and_then(|s| Some((s.keyspace.clone()?, s.name.clone()?)));
            }
            if named.is_none() {
                named = m.as_ref().and_then(|(_, sc)| sc.as_ref()?.schema.as_ref().map(|s| (s.keyspace.clone(), s.table.clone())));
            }
            let Some((ks, name)) = named else { bail!("can't tell which table {} belongs to (no schema.cql or Scylla.db schema next to it)", t.sstables[0].data_key()) };
            t.keyspace = Some(ks);
            t.table = Some(name);
        }
        meta.push(m);
    }
    // Merge tables that resolved to the same name.
    let mut merged: BTreeMap<(String, String), TableFiles> = BTreeMap::new();
    let mut metas: BTreeMap<(String, String), Meta> = BTreeMap::new();
    for (t, m) in backup.tables.drain(..).zip(meta) {
        let k = (t.keyspace.clone().unwrap_or_default(), t.table.clone().unwrap_or_default());
        metas.entry(k.clone()).or_insert(m);
        match merged.get_mut(&k) {
            Some(e) => {
                e.sstables.extend(t.sstables);
                e.sources += t.sources;
            }
            None => {
                merged.insert(k, t);
            }
        }
    }
    let wanted = |ks: &str| !crate::import::is_system_keyspace(ks) && (keyspaces.is_empty() || keyspaces.iter().any(|k| k == ks));
    backup.tables = merged.into_values().filter(|t| t.keyspace.as_deref().is_some_and(wanted)).collect();
    for k in keyspaces {
        if !backup.tables.iter().any(|t| t.keyspace.as_ref() == Some(k)) && !stmts.iter().any(|s| s.kind == StmtKind::Keyspace && s.name.as_ref() == Some(k)) {
            bail!("the backup has no keyspace {k}");
        }
    }
    stmts.retain(|s| s.keyspace.as_deref().is_none_or(wanted));
    // User types from the SSTables' type names, CREATE TABLE from Scylla.db when missing.
    let has = |stmts: &[Stmt], kind: StmtKind, ks: &str, name: &str| stmts.iter().any(|s| s.kind == kind && s.keyspace.as_deref() == Some(ks) && s.name.as_deref() == Some(name));
    let mut extra = Vec::new();
    for ((ks, table), m) in &metas {
        if !wanted(ks) {
            continue;
        }
        let Some((stats, scylla)) = m else { continue };
        let h = &stats.header;
        let mut udts: Vec<&SType> = Vec::new();
        for ty in std::iter::once(&h.key_type).chain(&h.clustering).chain(h.statics.iter().map(|c| &c.ty)).chain(h.regulars.iter().map(|c| &c.ty)) {
            ty.udts(&mut udts);
        }
        for u in udts {
            if let SType::Udt { keyspace, name, .. } = u
                && !has(&stmts, StmtKind::Type, keyspace, name)
                && !has(&extra, StmtKind::Type, keyspace, name)
                && let Some(c) = u.create_type()
            {
                extra.push(classify(&c));
            }
        }
        if !has(&stmts, StmtKind::Table, ks, table) && !stmts.iter().any(|s| s.kind == StmtKind::View && s.keyspace.as_deref() == Some(ks) && s.name.as_deref() == Some(table)) {
            match scylla.as_ref().and_then(|s| s.schema.as_ref()) {
                Some(s) => {
                    backup.warnings.push(format!("{ks}.{table}: no CQL schema in the backup; recreated from its SSTables with default options"));
                    extra.push(classify(&s.create_table()));
                }
                None => bail!("the backup has no schema for {ks}.{table} (no schema.cql)"),
            }
        }
    }
    stmts.extend(extra);
    let named: BTreeSet<String> = stmts.iter().filter(|s| s.kind == StmtKind::Keyspace).filter_map(|s| s.name.clone()).collect();
    let mut need: BTreeSet<String> = stmts.iter().filter_map(|s| s.keyspace.clone()).collect();
    need.extend(backup.tables.iter().filter_map(|t| t.keyspace.clone()));
    for ks in need.difference(&named) {
        stmts.push(classify(&format!("CREATE KEYSPACE IF NOT EXISTS {} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}", quote(ks))));
    }
    Ok(ordered(stmts))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Created,
    Existed,
    Skipped,
}

/// A keyspace statement with every replication factor set to 1 (`None`: nothing to change).
fn replication_factor_1(text: &str) -> Option<String> {
    let lower = text.to_ascii_lowercase();
    let at = lower.find("replication")?;
    let open = at + text[at..].find('{')?;
    let close = open + text[open..].find('}')?;
    let entries: Vec<String> = text[open + 1..close]
        .split(',')
        .map(|e| match e.split_once(':') {
            Some((k, v)) if !k.trim().trim_matches('\'').eq_ignore_ascii_case("class") && v.trim().trim_matches('\'').parse::<u32>().is_ok_and(|n| n > 1) => format!("{}: '1'", k.trim()),
            _ => e.trim().to_string(),
        })
        .collect();
    let out = format!("{}{{{}}}{}", &text[..open], entries.join(", "), &text[close + 1..]);
    (out != text).then_some(out)
}

/// Run one schema statement.
async fn run_ddl(session: &mut eng::Session, s: &Stmt, dialect: Dialect, warnings: &mut Vec<String>) -> Result<Outcome> {
    let exists = |e: &eng::Error| e.code == eng::ErrorCode::AlreadyExists || e.message.to_ascii_lowercase().contains("already exist");
    let err = match session.query(&s.text, eng::QueryParams::default()).await {
        Ok(_) => return Ok(Outcome::Created),
        Err(e) if exists(&e) => return Ok(Outcome::Existed),
        Err(e) => e,
    };
    let what = format!("{:?} {}", s.kind, s.name.as_deref().unwrap_or("")).to_ascii_lowercase();
    match s.kind {
        StmtKind::Table | StmtKind::View => {
            if let Some(plain) = strip_options(&s.text) {
                match session.query(&plain, eng::QueryParams::default()).await {
                    Ok(_) => {
                        warnings.push(format!("{what}: created without options the {} dialect refused ({})", dialect.name(), err.message));
                        return Ok(Outcome::Created);
                    }
                    Err(e) if exists(&e) => return Ok(Outcome::Existed),
                    Err(_) => {}
                }
            }
            if s.kind == StmtKind::View {
                warnings.push(format!("skipped {what}: {}", err.message));
                return Ok(Outcome::Skipped);
            }
            bail!("recreating {what}: {}", err.message)
        }
        StmtKind::Keyspace => {
            let ks = s.name.clone().unwrap_or_default();
            let plain = format!("CREATE KEYSPACE IF NOT EXISTS {} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}", quote(&ks));
            match session.query(&plain, eng::QueryParams::default()).await {
                Ok(_) => {
                    warnings.push(format!("keyspace {ks}: created with replication_factor 1 ({})", err.message));
                    Ok(Outcome::Created)
                }
                Err(e) if exists(&e) => Ok(Outcome::Existed),
                Err(e) => bail!("recreating keyspace {ks}: {}", e.message),
            }
        }
        StmtKind::Type => bail!("recreating {what}: {}", err.message),
        _ => {
            warnings.push(format!("skipped {what}: {}", err.message));
            Ok(Outcome::Skipped)
        }
    }
}

impl Control {
    /// Import an SSTable backup. Returns warnings.
    pub(crate) async fn import_sstables(&self, d: &NosqlDb, imp: &mut Import, rows: &Arc<AtomicI64>) -> Result<Vec<String>> {
        let loc = Location::parse(&imp.source)?;
        let listing = loc.list().await.with_context(|| format!("listing {}", loc.url))?;
        if listing.is_empty() {
            bail!("{} is empty or doesn't exist", loc.url);
        }
        let mut backup = discover(&loc, &listing).await?;
        let rt = tokio::runtime::Handle::current();
        let store = loc.store(rt.clone());
        let stmts = statements(&loc, &mut backup, &store, &imp.keyspaces).await?;
        if backup.tables.is_empty() {
            bail!("no SSTables found at {} ({})", loc.url, backup.kind.name());
        }
        let mut warnings = backup.warnings.clone();
        tracing::info!(import = imp.id, kind = backup.kind.name(), tag = ?backup.tag, tables = backup.tables.len(), "nosql sstable import");

        let lease = self.lease(d).await?;
        let db = lease.db().clone();
        let dialect = db.dialect();
        let mut session = Self::session(&lease, d, "roll import");
        let (derived, plain): (Vec<&Stmt>, Vec<&Stmt>) = stmts.iter().partition(|s| s.kind.is_derived());
        for s in plain {
            let outcome = run_ddl(&mut session, s, dialect, &mut warnings).await?;
            // A ScyllaDB tablets keyspace needs as many nodes as replicas, and a roll database
            // presents one: a keyspace this import created gets replication factor 1.
            if outcome == Outcome::Created
                && s.kind == StmtKind::Keyspace
                && let Some(ks) = &s.name
                && let Some(k) = db.schema().keyspace(ks).cloned()
                && k.tablets.is_some()
                && k.replication.options.values().any(|rf| rf.parse::<u32>().is_ok_and(|n| n > 1))
                && let Some(one) = replication_factor_1(&s.text)
            {
                let drop = format!("DROP KEYSPACE {}", quote(ks));
                session.query(&drop, eng::QueryParams::default()).await.map_err(|e| anyhow::anyhow!("{drop}: {}", e.message))?;
                session.query(&one, eng::QueryParams::default()).await.map_err(|e| anyhow::anyhow!("recreating keyspace {ks}: {}", e.message))?;
                warnings.push(format!("keyspace {ks}: replication factors set to 1 (a tablets keyspace needs a node per replica; this database is one node)"));
            }
        }
        // A refresh rebuilds indexes and views from the new data.
        if imp.runs > 0 {
            for s in &derived {
                let (Some(ks), Some(name)) = (&s.keyspace, &s.name) else { continue };
                let drop = match s.kind {
                    StmtKind::Index => format!("DROP INDEX IF EXISTS {}.{}", quote(ks), quote(name)),
                    _ => format!("DROP MATERIALIZED VIEW IF EXISTS {}.{}", quote(ks), quote(name)),
                };
                if let Err(e) = session.query(&drop, eng::QueryParams::default()).await {
                    warnings.push(format!("{drop}: {}", e.message));
                }
            }
        }

        // Targets.
        let schema = db.schema();
        let mut jobs: Vec<(usize, Option<SstFiles>)> = Vec::new();
        let mut targets: Vec<Arc<Target>> = Vec::new();
        for t in &backup.tables {
            let (Some(ks), Some(name)) = (&t.keyspace, &t.table) else { continue };
            let Some(et) = schema.table(ks, name) else {
                warnings.push(format!("{ks}.{name}: not in the recreated schema; its SSTables were skipped"));
                continue;
            };
            if et.is_view() || name.ends_with("_scylla_cdc_log") {
                continue; // rebuilt by the engine
            }
            let target = Arc::new(target(&db, et)?);
            let i = targets.len();
            if target.counter {
                jobs.push((i, None));
            } else {
                jobs.extend(t.sstables.iter().cloned().map(|s| (i, Some(s))));
            }
            targets.push(target);
        }
        let sources: Vec<Vec<SstFiles>> = backup.tables.iter().filter(|t| t.keyspace.as_ref().zip(t.table.as_ref()).and_then(|(k, n)| schema.table(k, n)).is_some_and(|et| !et.is_view() && !et.name.ends_with("_scylla_cdc_log"))).map(|t| t.sstables.clone()).collect();
        imp.tables_total = targets.len() as i64;
        self.store.update_import(imp)?;

        let staging = format!("import-{}", imp.id);
        let result = self.convert(&store, &rt, &targets, &sources, jobs, imp, rows, &staging, &db, &mut warnings).await;
        let loaded = match result {
            Ok(l) => l,
            Err(e) => {
                let _ = db.store().clear_snapshot(&staging).await;
                return Err(e);
            }
        };
        let replace = imp.runs > 0;
        let batches: Vec<Ingested> = targets.iter().zip(loaded).map(|(t, ssts)| Ingested { table: t.storage.id, sstables: ssts, replace }).collect();
        let ingest = db.store().ingest(batches).await;
        let _ = db.store().clear_snapshot(&staging).await;
        ingest?;

        for s in derived {
            run_ddl(&mut session, s, dialect, &mut warnings).await?;
        }
        db.flush().await?;
        db.store().write_manifest(true).await?;
        drop(session);
        Ok(warnings)
    }

    /// Convert every job; returns each target's new SSTables.
    #[allow(clippy::too_many_arguments)]
    async fn convert(
        &self,
        store: &si::source::ObjectStore,
        rt: &tokio::runtime::Handle,
        targets: &[Arc<Target>],
        sources: &[Vec<SstFiles>],
        jobs: Vec<(usize, Option<SstFiles>)>,
        imp: &mut Import,
        rows: &Arc<AtomicI64>,
        staging: &str,
        db: &Arc<eng::Database>,
        warnings: &mut Vec<String>,
    ) -> Result<Vec<Vec<Arc<nosql_storage::sstable::SsTable>>>> {
        let progress = Arc::new(Progress::default());
        let mut remaining: Vec<usize> = vec![0; targets.len()];
        for (i, _) in &jobs {
            remaining[*i] += 1;
        }
        let mut out: Vec<Vec<Arc<nosql_storage::sstable::SsTable>>> = vec![vec![]; targets.len()];
        // Tables without SSTables are done already.
        imp.tables_done = remaining.iter().filter(|r| **r == 0).count() as i64;
        let parallel = self.cfg.import_parallelism.clamp(1, MAX_PARALLEL);
        let opts = LoadOptions::default();
        let mut stream = futures::stream::iter(jobs.into_iter().map(|(i, job)| {
            let (store, rt, target, opts, progress) = (store.clone(), rt.clone(), targets[i].clone(), opts.clone(), progress.clone());
            let all = sources[i].clone();
            async move {
                let r = tokio::task::spawn_blocking(move || -> Result<Loaded> {
                    match job {
                        Some(f) => si::load::convert_sstable(&store, &f, &target, &opts, &rt, &progress),
                        None => si::load::convert_counters(&store, &all, &target, &opts, &rt, &progress),
                    }
                })
                .await
                .map_err(|e| anyhow::anyhow!("conversion task: {e}"))
                .and_then(|r| r);
                (i, r)
            }
        }))
        .buffer_unordered(parallel);
        while let Some((i, r)) = stream.next().await {
            let loaded = r.with_context(|| format!("importing {}.{}", targets[i].keyspace, targets[i].table))?;
            for w in loaded.warnings {
                if !warnings.contains(&w) {
                    warnings.push(w);
                }
            }
            out[i].extend(loaded.sstables);
            remaining[i] -= 1;
            if remaining[i] == 0 {
                imp.tables_done += 1;
            }
            rows.store(progress.rows.load(Ordering::Relaxed) as i64, Ordering::Relaxed);
            imp.rows = rows.load(Ordering::Relaxed);
            self.store.update_import(imp)?;
            let staged: Vec<(nosql_storage::manifest::TableId, Vec<Arc<nosql_storage::sstable::SsTable>>)> = targets.iter().zip(&out).filter(|(_, s)| !s.is_empty()).map(|(t, s)| (t.storage.id, s.clone())).collect();
            db.store().stage_ingest(staging, &staged).await?;
        }
        Ok(out)
    }
}

/// `start_import` checks for backup URLs.
pub(crate) fn validate(from: &str) -> CResult<()> {
    check_url(from).map_err(crate::control::ControlError::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replication_factors() {
        assert_eq!(
            replication_factor_1("CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'dc1': '3', 'dc2': 2} AND durable_writes = true").unwrap(),
            "CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'dc1': '1', 'dc2': '1'} AND durable_writes = true"
        );
        assert!(replication_factor_1("CREATE KEYSPACE ks WITH replication = {'class': 'NetworkTopologyStrategy', 'replication_factor': 1}").is_none());
    }

    #[test]
    fn shapes() {
        assert_eq!(shape(&CqlType::map(CqlType::Text, CqlType::list(CqlType::Int, true), false)), "map<text,list<int>>");
        assert_eq!(SType::parse("org.apache.cassandra.db.marshal.MapType(org.apache.cassandra.db.marshal.UTF8Type,org.apache.cassandra.db.marshal.FrozenType(org.apache.cassandra.db.marshal.ListType(org.apache.cassandra.db.marshal.Int32Type)))").unwrap().shape(), "map<text,list<int>>");
    }
}
