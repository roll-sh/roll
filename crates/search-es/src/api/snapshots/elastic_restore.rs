//! Restoring a snapshot Elasticsearch wrote ([`super::elastic`]): every live document of each
//! shard is read from the snapshot's Lucene files (`search_lucene`) and indexed again from its
//! `_source`, with its `_id`, routing, sequence number, version and primary term.
//!
//! - **Plan.** Each index's metadata (settings, mappings, aliases) becomes a create-index
//!   request, checked as `PUT <index>` checks one (no templates apply, as on Elasticsearch).
//!   Settings the cluster that made the index set, shard allocation, blocks, lifecycle and
//!   storage settings are dropped and reported, as the migration from a live cluster drops them.
//!   An index without a stored `_source` (disabled, synthetic: logsdb and time series by default)
//!   can't come back and is refused with its reason; `partial` restores the rest. Fields a
//!   mapping's `_source.excludes` left out are reported as missing.
//! - **Run.** A task (`cluster:admin/snapshot/restore`), one shard after another, at most one
//!   such restore per cluster and [`POOL`] on the node. A shard's files are read on a blocking
//!   thread through ranged reads (paying the repository's and the recovery throttles), their
//!   documents handed over a bounded channel and indexed like any write, so they're logged and
//!   fenced as writes are on the fleet. `_recovery` and `_cat/recovery` show each shard's files
//!   and bytes as they're read. The task obeys cancellation between documents.
//! - **Takeover.** The restore's state is kept in each index's recovery source: an owner that
//!   opens a cluster with a restore still running starts that index's restore again (documents
//!   already indexed are indexed again under the same ids), and one whose repository is gone
//!   records the failure.
//! - **Bounds.** Memory held (doc values, vector ordinals, chunk buffers, read windows,
//!   documents in flight) is charged to the `request` breaker as it is built; a stored fields
//!   chunk may not decompress past `http.max_content_length` plus a margin; every Lucene file
//!   read is checksummed.

use super::elastic::{self, EsData, EsSnapshot, Source};
use super::*;
use crate::write::{CopyTarget, DocCopy};
use bytes::Bytes;
use search_lucene::segment::{self, Files, Limits};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicU8, Ordering};

/// Restores from Elasticsearch repositories running on the node at once.
pub(super) static POOL: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
/// Documents between the reading thread and the indexing side.
const IN_FLIGHT: usize = 256;
/// Bytes read at a time from a large Lucene file.
const WINDOW: usize = 8 << 20;
/// The doc values and vector ordinals one segment may keep in memory.
const KEPT: usize = 512 << 20;
/// Small Lucene files read whole.
const SMALL_FILE: usize = 64 << 20;

/// The options of `POST _snapshot/{repo}/{snapshot}/_restore`.
pub(super) struct Request {
    pub(super) repo: String,
    pub(super) snap: String,
    pub(super) wait: bool,
    pub(super) patterns: Vec<String>,
    pub(super) ignore_unavailable: bool,
    pub(super) include_global_state: bool,
    pub(super) include_aliases: bool,
    pub(super) partial: bool,
    pub(super) rename: Option<(regex::Regex, String)>,
    pub(super) index_settings: Settings,
    pub(super) ignore_settings: Vec<String>,
}

/// One shard's progress, for `_recovery`.
#[derive(Default)]
pub struct ShardProgress {
    /// 0 init, 1 index, 2 done.
    stage: AtomicU8,
    files_total: AtomicU64,
    files_done: AtomicU64,
    bytes_total: AtomicU64,
    bytes_done: AtomicU64,
    start_ms: AtomicI64,
    stop_ms: AtomicI64,
}

/// What `_recovery` shows of a shard being restored.
pub struct ShardView {
    pub stage: &'static str,
    pub files: (u64, u64),
    pub bytes: (u64, u64),
    pub start_ms: i64,
    pub stop_ms: i64,
}

impl ShardProgress {
    pub fn view(&self) -> ShardView {
        ShardView {
            stage: match self.stage.load(Ordering::Relaxed) {
                0 => "INIT",
                1 => "INDEX",
                _ => "DONE",
            },
            files: (self.files_done.load(Ordering::Relaxed), self.files_total.load(Ordering::Relaxed)),
            bytes: (self.bytes_done.load(Ordering::Relaxed), self.bytes_total.load(Ordering::Relaxed)),
            start_ms: self.start_ms.load(Ordering::Relaxed),
            stop_ms: self.stop_ms.load(Ordering::Relaxed),
        }
    }
}

/// Restores from Elasticsearch repositories running for this cluster.
#[derive(Default)]
pub struct Running {
    busy: AtomicBool,
    /// By restored index uuid, one entry per shard.
    progress: parking_lot::Mutex<HashMap<String, Arc<Vec<ShardProgress>>>>,
}

impl Running {
    /// The shards of restored index `uuid` while its restore runs.
    pub fn progress(&self, uuid: &str) -> Option<Arc<Vec<ShardProgress>>> {
        self.progress.lock().get(uuid).cloned()
    }
}

/// Clears [`Running::busy`] when dropped.
struct Busy<'a>(&'a Running);

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.busy.store(false, Ordering::SeqCst);
    }
}

fn restore_error(repo: &str, snap: &str, uuid: &str, msg: impl std::fmt::Display) -> EsError {
    EsError::new(500, "snapshot_restore_exception", format!("[{repo}:{snap}/{uuid}] {msg}"))
}

/// Why a setting of the snapshot's index isn't kept, if it isn't (the rules the migration from
/// a live cluster uses).
fn drop_reason(key: &str) -> Option<&'static str> {
    const MADE_BY_THE_CLUSTER: &[&str] = &[
        "index.uuid",
        "index.creation_date",
        "index.creation_date_string",
        "index.provided_name",
        "index.history.uuid",
        "index.verified_before_close",
        "index.version.",
        "index.resize.",
        "index.shrink.",
        "index.downsample.",
        "index.rollup.",
        "index.frozen",
        "index.search.throttled",
    ];
    let starts = |prefixes: &[&str]| prefixes.iter().any(|p| key == *p || (p.ends_with('.') && key.starts_with(p)));
    if starts(MADE_BY_THE_CLUSTER) {
        return Some("set by the cluster that made the index");
    }
    if starts(&["index.routing.allocation.", "index.routing.rebalance."]) {
        return Some("roll places shards itself");
    }
    if starts(&["index.blocks."]) {
        return Some("a block would stop the restore: set it again once it's done");
    }
    if starts(&["index.lifecycle.", "index.plugins.index_state_management.", "index.opendistro.index_state_management."]) {
        return Some("lifecycle policies aren't attached: attach one on roll");
    }
    if starts(&["index.store."]) {
        return Some("roll keeps the index in its own storage");
    }
    None
}

/// An index of the snapshot, ready to create and fill.
struct Planned {
    source: String,
    target: String,
    index_id: String,
    shards: u32,
    settings: Settings,
    mappings: Option<Map>,
    aliases: Option<Value>,
    /// Vector fields to read back from the vector files (Elasticsearch 9 leaves them out of
    /// `_source`), by path.
    vectors: Vec<String>,
    notes: Vec<String>,
}

/// Paths of `type` fields in a mapping's properties; `nested`: whether under a nested object.
fn fields_of_type(props: &Map, prefix: &str, nested: bool, ty: &[&str], out: &mut Vec<(String, bool)>) {
    for (name, def) in props {
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
        let t = def.get("type").and_then(Value::as_str).unwrap_or("object");
        if ty.contains(&t) {
            out.push((path.clone(), nested));
        }
        if let Some(Value::Obj(sub)) = def.get("properties") {
            fields_of_type(sub, &path, nested || t == "nested", ty, out);
        }
    }
}

/// The `_doc` mapping of Elasticsearch's index metadata (a list of one `{"_doc": …}`, or the
/// map itself).
fn mapping_of(meta: &Value) -> Option<Map> {
    let m = meta.get("mappings")?;
    let one = match m {
        Value::Arr(a) => a.first()?.clone(),
        other => other.clone(),
    };
    match one {
        Value::Obj(o) if o.len() == 1 && o.contains_key("_doc") => o.get("_doc").and_then(Value::as_object).cloned(),
        Value::Obj(o) => Some(o),
        _ => None,
    }
}

/// Plans index `name` of the snapshot from its metadata: `Err(reason)` when it can't be
/// restored.
fn plan_index(req: &Request, name: &str, meta: &Value, index_id: &str) -> EsResult<Result<Planned, String>> {
    let meta = meta.get(name).or_else(|| meta.as_object().filter(|o| o.len() == 1).and_then(|o| o.values().next())).ok_or_else(|| EsError::internal("index metadata without the index"))?;
    let raw = Settings::from_json(meta.get("settings").unwrap_or(&Value::Null)).unwrap_or_default();
    let shards = raw.get("index.number_of_shards").and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
    if shards == 0 || shards > elastic::MAX_SHARDS {
        return Ok(Err(format!("it has [{shards}] shards")));
    }
    let mapping = mapping_of(meta);
    let source = mapping.as_ref().and_then(|m| m.get("_source")).cloned().unwrap_or(Value::Null);
    let mode = raw.get("index.mode").unwrap_or("standard").to_ascii_lowercase();
    let source_mode = raw.get("index.mapping.source.mode").map(str::to_ascii_lowercase).or_else(|| source.get("mode").and_then(Value::as_str).map(str::to_ascii_lowercase));
    if source.get("enabled").and_then(Value::as_bool) == Some(false) || source_mode.as_deref() == Some("disabled") {
        return Ok(Err("its documents have no stored _source (_source.enabled: false), so they can't be indexed again".into()));
    }
    match source_mode.as_deref() {
        Some("synthetic") => return Ok(Err("it keeps synthetic _source, not the documents' own, so they can't be indexed again".into())),
        Some("stored") => {}
        _ if mode == "logsdb" || mode == "time_series" => {
            return Ok(Err(format!("an index.mode [{mode}] index keeps synthetic _source unless set otherwise, so its documents can't be indexed again")));
        }
        _ => {}
    }
    let mut notes = Vec::new();
    let excludes: Vec<String> = source.get("excludes").and_then(Value::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect()).unwrap_or_default();
    if !excludes.is_empty() {
        notes.push(format!("index [{name}]: fields [{}] were left out of _source and don't come back", excludes.join(", ")));
    }
    // Vectors Elasticsearch 9 leaves out of `_source` come back from the vector files.
    let created = raw.get("index.version.created").and_then(|v| v.parse::<i64>().ok()).unwrap_or(0);
    let excluded = match raw.get("index.mapping.exclude_source_vectors") {
        Some(v) => v == "true",
        None => created >= 9_000_000,
    };
    let mut vectors = Vec::new();
    if let Some(Value::Obj(props)) = mapping.as_ref().and_then(|m| m.get("properties")) {
        let mut found = Vec::new();
        fields_of_type(props, "", false, &["dense_vector"], &mut found);
        for (path, nested) in found {
            if nested && excluded {
                return Ok(Err(format!("the vectors of [{path}], inside a nested object, were left out of _source")));
            }
            if excluded {
                vectors.push(path);
            }
        }
        let mut other = Vec::new();
        fields_of_type(props, "", false, &["sparse_vector", "rank_vectors"], &mut other);
        if excluded && let Some((path, _)) = other.first() {
            return Ok(Err(format!("the values of [{path}] may have been left out of _source (index.mapping.exclude_source_vectors), and roll can't read them back")));
        }
    }
    let target = match &req.rename {
        Some((re, rep)) => re.replace_all(name, rep.as_str()).into_owned(),
        None => name.to_string(),
    };
    let mut settings = Settings::new();
    let mut dropped = Vec::new();
    for (k, v) in raw.iter() {
        if req.ignore_settings.iter().any(|i| glob_match(i, k)) {
            continue;
        }
        match drop_reason(k) {
            Some(why) => dropped.push(format!("[{k}] ({why})")),
            None => settings.insert(k.clone(), v.clone()),
        }
    }
    // Elasticsearch's default depends on the version that made the index: kept as it was.
    settings.set("index.mapping.exclude_source_vectors", if excluded { "true" } else { "false" });
    settings.merge(&req.index_settings);
    if !dropped.is_empty() {
        notes.push(format!("index [{name}]: settings not restored: {}", dropped.join(", ")));
    }
    let aliases = req.include_aliases.then(|| meta.get("aliases").cloned()).flatten().filter(|a| a.as_object().is_some_and(|o| !o.is_empty()));
    let mut mappings = mapping;
    if let Some(m) = &mut mappings {
        // Elasticsearch's own bookkeeping in a mapping.
        m.shift_remove("_data_stream_timestamp");
    }
    Ok(Ok(Planned { source: name.to_string(), target, index_id: index_id.to_string(), shards, settings, mappings, aliases, vectors, notes }))
}

/// Creates `p`'s index in `st` (templates don't apply), its recovery source naming the
/// restore.
fn create_planned(st: &mut ClusterState, req: &Request, snapshot_uuid: &str, p: &Planned, now: i64) -> EsResult<()> {
    if let Some(existing) = st.metadata.indices.get(&p.target)
        && !existing.is_closed()
    {
        return Err(restore_error(&req.repo, &req.snap, snapshot_uuid, format!(
            "cannot restore index [{}] because an open index with same name already exists in the cluster. Either close or delete the existing index or restore the index under a different name by providing a rename pattern and replacement name",
            p.target
        )));
    }
    let mut body = Map::new();
    body.insert("settings".into(), p.settings.to_nested_json());
    if let Some(m) = &p.mappings {
        body.insert("mappings".into(), Value::Obj(m.clone()));
    }
    if let Some(a) = &p.aliases {
        body.insert("aliases".into(), a.clone());
    }
    let create = crate::api::indices::parse_create_body(Some(body))?;
    let mut scratch = st.clone();
    // A closed index of the same name is replaced, as roll's own restores replace it.
    scratch.metadata.indices.remove(&p.target);
    scratch.metadata.index_templates.clear();
    scratch.metadata.templates.clear();
    crate::api::indices::create_index(&mut scratch, &p.target, &p.target, create, now)?;
    let Some(created) = scratch.metadata.indices.get(&p.target).cloned() else { return Err(EsError::internal("the restored index wasn't created")) };
    let mut idx = (*created).clone();
    let mut rs = Map::new();
    rs.insert("repository".into(), req.repo.clone().into());
    rs.insert("snapshot".into(), req.snap.clone().into());
    rs.insert("version".into(), crate::version::ES_VERSION.into());
    rs.insert("index".into(), p.source.clone().into());
    rs.insert("restoreUUID".into(), random_uuid().into());
    rs.insert(
        "roll_elastic".into(),
        obj! {
            "state" => "running", "snapshot_uuid" => snapshot_uuid.to_string(), "index_id" => p.index_id.clone(),
            "shards" => p.shards as i64, "vectors" => Value::from(p.vectors.clone()),
        },
    );
    idx.recovery_source = Some(Arc::new(rs));
    st.metadata.indices.insert(p.target.clone(), Arc::new(idx));
    st.check_unique_names()?;
    Ok(())
}

/// Global state items: their API path and body, from Elasticsearch's global metadata.
fn global_items(g: &Value) -> Vec<(String, String, Value)> {
    let g = g.get("meta-data").unwrap_or(g);
    let managed = |v: &Value| v.pointer(&["_meta", "managed"]).and_then(Value::as_bool) == Some(true);
    let mut out = Vec::new();
    for (name, t) in g.pointer(&["component_template", "component_template"]).and_then(Value::as_object).into_iter().flatten() {
        if !managed(t) && !name.starts_with('.') {
            out.push((format!("component template [{name}]"), format!("/_component_template/{name}"), t.clone()));
        }
    }
    for (name, p) in g.pointer(&["index_lifecycle", "policies"]).and_then(Value::as_object).into_iter().flatten() {
        let Some(Value::Obj(policy)) = p.get("policy") else { continue };
        let mut policy = policy.clone();
        policy.shift_remove("name");
        if !managed(&Value::Obj(policy.clone())) && !name.starts_with('.') {
            out.push((format!("lifecycle policy [{name}]"), format!("/_ilm/policy/{name}"), obj! { "policy" => Value::Obj(policy) }));
        }
    }
    for p in g.pointer(&["ingest", "pipeline"]).and_then(Value::as_array).into_iter().flatten() {
        let (Some(id), Some(config)) = (p.get("id").and_then(Value::as_str), p.get("config")) else { continue };
        if !managed(config) && !id.starts_with('.') {
            out.push((format!("pipeline [{id}]"), format!("/_ingest/pipeline/{id}"), config.clone()));
        }
    }
    for (name, t) in g.pointer(&["index_template", "index_template"]).and_then(Value::as_object).into_iter().flatten() {
        if !managed(t) && !name.starts_with('.') {
            out.push((format!("index template [{name}]"), format!("/_index_template/{name}"), t.clone()));
        }
    }
    for (name, t) in g.get("templates").and_then(Value::as_object).into_iter().flatten() {
        if name.starts_with('.') {
            continue;
        }
        let mut t = t.as_object().cloned().unwrap_or_default();
        if let Some(Value::Obj(m)) = t.get("mappings").cloned()
            && let Some(doc) = m.get("_doc")
        {
            t.insert("mappings".into(), doc.clone());
        }
        out.push((format!("legacy template [{name}]"), format!("/_template/{name}"), Value::Obj(t)));
    }
    out
}

/// Elasticsearch's index metadata for `name` in snapshot `s`.
async fn index_metadata(ctx: &Ctx<'_>, src: &Source, data: &EsData, s: &EsSnapshot, name: &str) -> EsResult<(Value, String)> {
    let Some(index) = data.index(name) else { return Err(restore_error(&src.repo, &s.name, &s.uuid, format!("index [{name}] isn't in the repository data"))) };
    let key = data.index_metadata_key(s, &index.id).ok_or_else(|| restore_error(&src.repo, &s.name, &s.uuid, format!("index [{name}] has no metadata in the repository data")))?;
    match elastic::read_blob(ctx.cluster, src, &key, "index-metadata", &mut elastic::base64_binary).await? {
        Some((v, _held)) => Ok((v, index.id.clone())),
        None => Err(restore_error(&src.repo, &s.name, &s.uuid, format!("the metadata of index [{name}] ([{key}]) is missing"))),
    }
}

/// `POST _snapshot/{repo}/{snapshot}/_restore` on a repository Elasticsearch wrote.
pub(super) async fn restore(ctx: &Ctx<'_>, req: Request, r: Arc<Repository>) -> EsResult<Reply> {
    let Some((src, data)) = elastic::sync(ctx.cluster, &req.repo, &r).await? else { return Err(repository_missing(&req.repo)) };
    let Some(s) = data.snapshot(&req.snap).cloned() else { return Err(snapshot_missing(&req.repo, &req.snap)) };
    let record = ctx.state().metadata.repository_data.get(&r.location_key()).and_then(|d| d.find(&req.snap).cloned()).ok_or_else(|| snapshot_missing(&req.repo, &req.snap))?;
    if !matches!(record.state.as_str(), "SUCCESS" | "PARTIAL") {
        return Err(restore_error(&req.repo, &req.snap, &s.uuid, format!("unsupported snapshot state [{}]", record.state)));
    }
    // Which indices.
    let mut selected: Vec<String> = Vec::new();
    if req.patterns.is_empty() || req.patterns.iter().any(|p| p == "_all" || p == "*") {
        selected = record.indices.iter().filter(|i| !i.starts_with('.')).cloned().collect();
    } else {
        for p in &req.patterns {
            if let Some(ex) = p.strip_prefix('-') {
                selected.retain(|i| !glob_match(ex, i));
                continue;
            }
            let hits: Vec<String> = record.indices.iter().filter(|i| glob_match(p, i)).cloned().collect();
            if hits.is_empty() && !p.contains('*') && !req.ignore_unavailable {
                return Err(restore_error(&req.repo, &req.snap, &s.uuid, format!("index [{p}] not found")));
            }
            for h in hits {
                if !selected.contains(&h) {
                    selected.push(h);
                }
            }
        }
    }
    // Plans from each index's metadata.
    let mut planned = Vec::new();
    let mut refused: Vec<(String, String)> = Vec::new();
    for name in &selected {
        let (meta, id) = index_metadata(ctx, &src, &data, &s, name).await?;
        match plan_index(&req, name, &meta, &id)? {
            Ok(p) => {
                crate::meta::names::validate_index_name(&p.target)?;
                planned.push(p);
            }
            Err(why) => refused.push((name.clone(), why)),
        }
    }
    // Indices roll's index creation refuses (a mapping or setting it doesn't take).
    {
        let mut st = (*ctx.state()).clone();
        let mut kept = Vec::new();
        for p in planned {
            match create_planned(&mut st, &req, &s.uuid, &p, ctx.now_ms) {
                Ok(()) => kept.push(p),
                Err(e) if e.kind == "snapshot_restore_exception" => return Err(e),
                Err(e) => refused.push((p.source.clone(), e.reason().to_string())),
            }
        }
        planned = kept;
    }
    if !refused.is_empty() && !req.partial {
        let list: Vec<String> = refused.iter().map(|(i, why)| format!("index [{i}]: {why}")).collect();
        return Err(restore_error(&req.repo, &req.snap, &s.uuid, format!("cannot restore {}; set [partial] to restore the other indices", list.join("; "))));
    }
    let mut notes: Vec<String> = refused.iter().map(|(i, why)| format!("index [{i}] not restored: {why}")).collect();
    for p in &planned {
        notes.extend(p.notes.iter().cloned());
    }
    // Global state, item by item through the APIs that set them, as the requester.
    if req.include_global_state {
        if !record.include_global_state {
            notes.push("the snapshot holds no global state".into());
        } else if let Some((g, _held)) = elastic::read_blob(ctx.cluster, &src, &format!("meta-{}.dat", s.uuid), "metadata", &mut elastic::base64_binary).await? {
            for (what, path, body) in global_items(&g) {
                if let Err(e) = crate::xpack::internal::call_as(ctx, ctx.cluster, "PUT", &path, Some(&body)).await {
                    notes.push(format!("{what} not restored: {}", e.reason()));
                }
            }
        }
    }
    let busy = &ctx.cluster.snapshot_data.elastic;
    if busy.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
        return Err(EsError::new(503, "concurrent_snapshot_execution_exception", format!("[{}:{}] a restore from a repository Elasticsearch wrote is already running in this cluster", req.repo, req.snap)));
    }
    let busy = Busy(busy);
    let (snap_uuid, repo2, snap2) = (s.uuid.clone(), req.repo.clone(), req.snap.clone());
    let now = ctx.now_ms;
    let plan_copy: Vec<Planned> = planned.iter().map(|p| Planned { notes: Vec::new(), ..clone_planned(p) }).collect();
    let req_for_update = Request {
        repo: req.repo.clone(),
        snap: req.snap.clone(),
        wait: req.wait,
        patterns: Vec::new(),
        ignore_unavailable: false,
        include_global_state: false,
        include_aliases: req.include_aliases,
        partial: req.partial,
        rename: None,
        index_settings: Settings::new(),
        ignore_settings: Vec::new(),
    };
    ctx.cluster
        .update(move |st| {
            for p in &plan_copy {
                create_planned(st, &req_for_update, &snap_uuid, p, now)?;
            }
            Ok(())
        })
        .await?;
    ctx.warn_all(notes);
    let st = ctx.state();
    let mut jobs = Vec::new();
    for p in &planned {
        let Some(idx) = st.metadata.indices.get(&p.target) else { continue };
        let progress: Arc<Vec<ShardProgress>> = Arc::new((0..p.shards).map(|_| ShardProgress::default()).collect());
        ctx.cluster.snapshot_data.elastic.progress.lock().insert(idx.uuid.clone(), progress.clone());
        jobs.push(IndexJob { target: idx.clone(), index_id: p.index_id.clone(), shards: p.shards, vectors: p.vectors.clone(), progress });
    }
    let total: i64 = planned.iter().map(|p| p.shards as i64).sum();
    let targets: Vec<String> = planned.iter().map(|p| p.target.clone()).collect();
    let job = Job { src: Arc::new(src), snapshot: s.uuid.clone(), name: snap2.clone(), indices: jobs };
    let guard = ctx.cluster.tasks.start(crate::cluster::NewTask {
        action: "cluster:admin/snapshot/restore".into(),
        description: format!("restore snapshot [{repo2}:{snap2}] from an Elasticsearch repository"),
        describe: None,
        parent: None,
        headers: Vec::new(),
        cancellable: true,
        cancelled: None,
    });
    let cancelled = ctx.cluster.tasks.get(guard.id).map(|t| t.cancelled).unwrap_or_default();
    if !req.wait
        && let Some(cluster) = ctx.cluster.handle()
    {
        let id = guard.detach();
        std::mem::forget(busy);
        tokio::spawn(async move {
            let guard = cluster.tasks.adopt(id);
            let _busy = Busy(&cluster.snapshot_data.elastic);
            // A failure is recorded in each index's recovery source (`set_state`).
            let _ = run(&cluster, &job, &cancelled).await;
            drop(guard);
        });
        return Reply::ok(obj! { "accepted" => true });
    }
    let outcome = run(ctx.cluster, &job, &cancelled).await;
    drop(guard);
    drop(busy);
    let failed = outcome?;
    if !req.wait {
        return Reply::ok(obj! { "accepted" => true });
    }
    Reply::ok(obj! {
        "snapshot" => obj! {
            "snapshot" => req.snap.clone(),
            "indices" => Value::from(targets),
            "shards" => obj! { "total" => total, "failed" => failed, "successful" => total - failed },
        }
    })
}

fn clone_planned(p: &Planned) -> Planned {
    Planned {
        source: p.source.clone(),
        target: p.target.clone(),
        index_id: p.index_id.clone(),
        shards: p.shards,
        settings: p.settings.clone(),
        mappings: p.mappings.clone(),
        aliases: p.aliases.clone(),
        vectors: p.vectors.clone(),
        notes: p.notes.clone(),
    }
}

/// An index to fill.
struct IndexJob {
    target: Arc<IndexMeta>,
    index_id: String,
    shards: u32,
    vectors: Vec<String>,
    progress: Arc<Vec<ShardProgress>>,
}

struct Job {
    src: Arc<Source>,
    snapshot: String,
    name: String,
    indices: Vec<IndexJob>,
}

/// Records the restore's outcome in the index's recovery source.
async fn set_state(cluster: &crate::Cluster, index: &str, state: &str, failure: Option<String>) {
    let (index, state) = (index.to_string(), state.to_string());
    let _ = cluster
        .update(move |st| {
            let Some(idx) = st.metadata.indices.get(&index) else { return Ok(()) };
            let mut idx = (**idx).clone();
            if let Some(rs) = &idx.recovery_source {
                let mut rs = (**rs).clone();
                if let Some(Value::Obj(e)) = rs.get_mut("roll_elastic") {
                    e.insert("state".into(), state.into());
                    if let Some(f) = &failure {
                        e.insert("failure".into(), f.clone().into());
                    }
                }
                idx.recovery_source = Some(Arc::new(rs));
            }
            st.metadata.indices.insert(index, Arc::new(idx));
            Ok(())
        })
        .await;
}

/// Fills every index of `job`: the shards that failed (the first failure ends the restore).
async fn run(cluster: &crate::Cluster, job: &Job, cancelled: &Arc<AtomicBool>) -> EsResult<i64> {
    let _permit = POOL.acquire().await.map_err(|e| EsError::internal(e.to_string()))?;
    for ix in &job.indices {
        let result = async {
            for shard in 0..ix.shards {
                restore_shard(cluster, job, ix, shard, cancelled).await?;
            }
            CopyTarget::new(cluster, &ix.target)?.finish()
        }
        .await;
        cluster.snapshot_data.elastic.progress.lock().remove(&ix.target.uuid);
        match result {
            Ok(()) => set_state(cluster, &ix.target.name, "done", None).await,
            Err(e) => {
                set_state(cluster, &ix.target.name, "failed", Some(e.reason().to_string())).await;
                for later in &job.indices {
                    cluster.snapshot_data.elastic.progress.lock().remove(&later.target.uuid);
                }
                return Err(e);
            }
        }
    }
    Ok(0)
}

/// A Lucene file of a shard snapshot.
#[derive(Clone)]
struct FileInfo {
    blob: String,
    length: u64,
    part_size: Option<u64>,
    /// A small file kept in the shard snapshot itself (`v__` blobs).
    content: Option<Bytes>,
}

/// One live document, read on the blocking side.
struct Item {
    id: String,
    routing: Option<String>,
    source: Bytes,
    seq_no: Option<i64>,
    version: Option<i64>,
    primary_term: Option<i64>,
}

/// The files a shard snapshot lists, by Lucene file name.
fn shard_files(v: &Value, repo: &str, rel: &str) -> EsResult<HashMap<String, FileInfo>> {
    let damaged = |why: String| repository_exception(repo, format!("the shard snapshot [{rel}] is damaged: {why}"));
    let files = v.get("files").and_then(Value::as_array).ok_or_else(|| damaged("it lists no files".into()))?;
    if files.len() > elastic::MAX_FILES {
        return Err(damaged(format!("it lists more than [{}] files", elastic::MAX_FILES)));
    }
    let mut out = HashMap::with_capacity(files.len());
    for f in files {
        let (Some(blob), Some(physical), Some(length)) = (f.get("name").and_then(Value::as_str), f.get("physical_name").and_then(Value::as_str), f.get("length").and_then(Value::as_i64)) else {
            return Err(damaged("a file without a name, physical name or length".into()));
        };
        if !elastic::valid_name(blob) || !(physical.len() <= 255 && physical.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'.' || b == b'-')) || length < 0 {
            return Err(damaged(format!("invalid file [{}] [{}]", blob.escape_debug(), physical.escape_debug())));
        }
        if let Some(w) = f.get("written_by").and_then(Value::as_str)
            && let Some(major) = w.split('.').next().and_then(|m| m.parse::<u32>().ok())
            && major < 9
        {
            return Err(EsError::new(500, "snapshot_restore_exception", format!(
                "[{repo}] the file [{physical}] was written by Lucene {w} (Elasticsearch 7.x or earlier); roll restores segments Lucene 9 and 10 wrote (Elasticsearch 8.0 and later)"
            )));
        }
        let content = if blob.starts_with("v__") {
            let raw = f.get("meta_hash").and_then(Value::as_str).and_then(|b| {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD.decode(b).ok()
            });
            match raw {
                Some(r) if r.len() as i64 == length => Some(Bytes::from(r)),
                _ => return Err(damaged(format!("the content of [{physical}] isn't in the snapshot"))),
            }
        } else {
            None
        };
        let part_size = f.get("part_size").and_then(Value::as_i64).filter(|p| *p > 0 && *p < length).map(|p| p as u64);
        if let Some(p) = part_size
            && (length as u64).div_ceil(p) > 1 << 24
        {
            return Err(damaged(format!("[{physical}] is in more than [{}] parts", 1 << 24)));
        }
        out.insert(physical.to_string(), FileInfo { blob: blob.to_string(), length: length as u64, part_size, content });
    }
    Ok(out)
}

/// The blocking side's view of a shard's files.
struct ShardDir {
    src: Arc<Source>,
    dir: String,
    files: HashMap<String, FileInfo>,
    handle: tokio::runtime::Handle,
    held: crate::breaker::Reservation,
    cancelled: Arc<AtomicBool>,
    progress: Arc<Vec<ShardProgress>>,
    shard: usize,
    /// How far each file was read.
    read_to: HashMap<String, u64>,
    failure: Option<EsError>,
}

impl ShardDir {
    fn fail(&mut self, e: EsError) -> search_lucene::Error {
        let msg = e.reason().to_string();
        self.failure = Some(e);
        search_lucene::Error::Corrupt(msg)
    }

    fn fetch(&mut self, name: &str, offset: u64, len: usize) -> EsResult<Vec<u8>> {
        let Some(f) = self.files.get(name).cloned() else { return Err(EsError::internal(format!("no file [{name}]"))) };
        if offset + len as u64 > f.length {
            return Err(repository_exception(&self.src.repo, format!("a read of [{name}] past its [{}] bytes", f.length)));
        }
        if let Some(c) = &f.content {
            return Ok(c[offset as usize..offset as usize + len].to_vec());
        }
        let mut out = Vec::with_capacity(len);
        let mut at = offset;
        let end = offset + len as u64;
        while at < end {
            let (blob, part_off, n) = match f.part_size {
                Some(ps) => {
                    let part = at / ps;
                    let within = at % ps;
                    (format!("{}/{}.part{part}", self.dir, f.blob), within, (ps - within).min(end - at))
                }
                None => (format!("{}/{}", self.dir, f.blob), at, end - at),
            };
            let piece = self.handle.block_on(self.src.range(&blob, part_off, n as usize))?;
            out.extend_from_slice(&piece);
            at += n;
        }
        let to = self.read_to.entry(name.to_string()).or_insert(0);
        if end > *to {
            let p = &self.progress[self.shard];
            p.bytes_done.fetch_add(end - *to, Ordering::Relaxed);
            if *to < f.length && end == f.length {
                p.files_done.fetch_add(1, Ordering::Relaxed);
            }
            *to = end;
        }
        Ok(out)
    }
}

impl Files for ShardDir {
    fn len(&mut self, name: &str) -> search_lucene::Result<Option<u64>> {
        Ok(self.files.get(name).map(|f| f.length))
    }

    fn read(&mut self, name: &str, offset: u64, len: usize) -> search_lucene::Result<Vec<u8>> {
        self.fetch(name, offset, len).map_err(|e| self.fail(e))
    }

    fn charge(&mut self, delta: i64) -> search_lucene::Result<()> {
        if delta >= 0 {
            self.held.add(delta as u64, "<elasticsearch_restore>").map_err(|e| self.fail(e))
        } else {
            self.held.release(delta.unsigned_abs());
            Ok(())
        }
    }

    fn check(&mut self) -> search_lucene::Result<()> {
        if self.cancelled.load(Ordering::Relaxed) {
            return Err(self.fail(EsError::new(400, "task_cancelled_exception", "task cancelled [by user request]")));
        }
        Ok(())
    }
}

/// `_source` as JSON: Elasticsearch keeps it in the content type it was sent in.
fn source_json(raw: &[u8], budget: &Arc<search_aggs::memory::MemoryBudget>) -> Result<Bytes, String> {
    let first = raw.iter().find(|b| !b.is_ascii_whitespace()).copied();
    match first {
        Some(b'{') => Ok(Bytes::copy_from_slice(raw)),
        _ if raw.starts_with(b":)\n") => {
            let mut held = crate::breaker::Reservation::new(budget.clone());
            let mut charge = |n: usize| held.add(n as u64, "<elasticsearch_restore>").map_err(|e| e.reason().to_string());
            let v = crate::xcontent::smile::parse(raw, crate::xcontent::smile::Limits { depth: 200, charge: &mut charge })?;
            Ok(Bytes::from(v.to_json()))
        }
        Some(0xBF | 0xA0..=0xBB) => crate::xcontent::cbor::read_cbor(raw).map(|v| Bytes::from(v.to_json())),
        _ => Err("a _source that isn't JSON, SMILE or CBOR".into()),
    }
}

/// Puts the vectors `_source` left out back into it: appended as the last fields of a JSON
/// object when they're top-level fields, so the rest of the source keeps its bytes.
fn with_vectors(source: Bytes, vectors: &[(&str, Vec<f32>)]) -> Result<Bytes, String> {
    if vectors.is_empty() {
        return Ok(source);
    }
    let parsed = crate::json::parse(&source).map_err(|e| e.to_string())?.ok_or("an empty _source")?;
    let Value::Obj(mut top) = parsed else { return Err("a _source that isn't an object".into()) };
    let render = |v: &[f32]| Value::Arr(v.iter().map(|x| Value::Num(crate::json::Number::Float(*x as f64))).collect());
    let missing: Vec<&(&str, Vec<f32>)> = vectors.iter().filter(|(path, _)| path_get(&top, path).is_none()).collect();
    if missing.is_empty() {
        return Ok(source);
    }
    if missing.iter().all(|(p, _)| !p.contains('.'))
        && let Some(end) = source.iter().rposition(|b| *b == b'}')
    {
        let mut out = source[..end].to_vec();
        let empty = top.is_empty();
        for (i, (path, v)) in missing.iter().enumerate() {
            if !(empty && i == 0) {
                out.push(b',');
            }
            out.extend_from_slice(Value::Str(path.to_string()).to_json().as_bytes());
            out.push(b':');
            out.extend_from_slice(f32_array(v).as_bytes());
        }
        out.extend_from_slice(&source[end..]);
        return Ok(Bytes::from(out));
    }
    for (path, v) in missing {
        path_set(&mut top, path, render(v));
    }
    Ok(Bytes::from(Value::Obj(top).to_json()))
}

/// A vector as Elasticsearch prints a float vector: each value as the shortest `float`.
fn f32_array(v: &[f32]) -> String {
    let mut s = String::from("[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        if x.is_finite() && x.fract() == 0.0 && x.abs() < 1e7 {
            s.push_str(&format!("{x:.1}"));
        } else {
            s.push_str(&format!("{x}"));
        }
    }
    s.push(']');
    s
}

fn path_get<'a>(m: &'a Map, path: &str) -> Option<&'a Value> {
    if let Some(v) = m.get(path) {
        return Some(v);
    }
    let (head, rest) = path.split_once('.')?;
    match m.get(head)? {
        Value::Obj(sub) => path_get(sub, rest),
        _ => None,
    }
}

fn path_set(m: &mut Map, path: &str, v: Value) {
    match path.split_once('.') {
        None => {
            m.insert(path.to_string(), v);
        }
        Some((head, rest)) => {
            let e = m.entry(head.to_string()).or_insert_with(|| Value::Obj(Map::new()));
            if let Value::Obj(sub) = e {
                path_set(sub, rest, v);
            } else {
                m.insert(path.to_string(), v);
            }
        }
    }
}

/// Restores shard `shard` of `ix` from the snapshot.
async fn restore_shard(cluster: &crate::Cluster, job: &Job, ix: &IndexJob, shard: u32, cancelled: &Arc<AtomicBool>) -> EsResult<()> {
    let src = job.src.clone();
    let progress = &ix.progress[shard as usize];
    progress.start_ms.store(crate::time::now_millis(), Ordering::Relaxed);
    let dir = format!("indices/{}/{shard}", ix.index_id);
    let rel = format!("{dir}/snap-{}.dat", job.snapshot);
    let Some((meta, _held)) = elastic::read_blob(cluster, &src, &rel, "snapshot", &mut elastic::base64_binary).await? else {
        return Err(EsError::new(500, "snapshot_restore_exception", format!("[{}:{}] the shard snapshot [{rel}] is missing", src.repo, job.name)));
    };
    let files = shard_files(&meta, &src.repo, &rel)?;
    drop(meta);
    progress.files_total.store(files.len() as u64, Ordering::Relaxed);
    progress.bytes_total.store(files.values().map(|f| f.length).sum(), Ordering::Relaxed);
    progress.stage.store(1, Ordering::Relaxed);
    let segments: Vec<(&String, i64)> = files.keys().filter_map(|n| search_lucene::segments::generation_of(n).map(|g| (n, g))).collect();
    let [(commit_name, generation)] = segments[..] else {
        return Err(EsError::new(500, "snapshot_restore_exception", format!("[{}:{}] the shard snapshot [{rel}] has [{}] commit points, not one", src.repo, job.name, segments.len())));
    };
    let commit_name = commit_name.clone();
    let source_max = s3::source_max(cluster);
    let budget = cluster.breakers.request(cluster, &cluster.state());
    let limits = Limits { small_file: SMALL_FILE, chunk: source_max.saturating_add(1 << 20), kept: KEPT, window: WINDOW };
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Item>(IN_FLIGHT);
    let mut dir_state = ShardDir {
        src,
        dir,
        files,
        handle: tokio::runtime::Handle::current(),
        held: crate::breaker::Reservation::new(budget.clone()),
        cancelled: cancelled.clone(),
        progress: ix.progress.clone(),
        shard: shard as usize,
        read_to: HashMap::new(),
        failure: None,
    };
    let vectors = ix.vectors.clone();
    let budget2 = budget.clone();
    let repo_name = job.src.repo.clone();
    let reader = tokio::task::spawn_blocking(move || -> EsResult<()> {
        let d = &mut dir_state;
        let map = |d: &mut ShardDir, file: &str, e: search_lucene::Error| -> EsError {
            if let Some(f) = d.failure.take() {
                return f;
            }
            match e {
                search_lucene::Error::Corrupt(m) => EsError::new(500, "snapshot_restore_exception", format!("[{repo_name}] the file [{file}] of the snapshot is damaged: {m}")),
                search_lucene::Error::Unsupported(m) => EsError::new(500, "snapshot_restore_exception", format!("[{repo_name}] the file [{file}] can't be restored: {m}")),
            }
        };
        let len = d.files.get(&commit_name).map(|f| f.length).unwrap_or(0);
        if len > SMALL_FILE as u64 {
            return Err(EsError::new(500, "snapshot_restore_exception", format!("[{repo_name}] [{commit_name}] is larger than [{SMALL_FILE}] bytes")));
        }
        let bytes = d.fetch(&commit_name, 0, len as usize)?;
        let commit = search_lucene::segments::read_commit(&bytes, generation).map_err(|e| map(d, &commit_name, e))?;
        drop(bytes);
        let mut seq_fallback = 0i64;
        for seg in &commit.segments {
            let si_name = format!("{}.si", seg.name);
            let len = d.files.get(&si_name).map(|f| f.length).ok_or_else(|| map(d, &si_name, search_lucene::Error::Corrupt("missing".into())))?;
            if len > SMALL_FILE as u64 {
                return Err(map(d, &si_name, search_lucene::Error::Unsupported("too large".into())));
            }
            let raw = d.fetch(&si_name, 0, len as usize)?;
            let si = search_lucene::segments::read_segment_info(&raw, &seg.id).map_err(|e| map(d, &si_name, e))?;
            let mut stop = None;
            let mut on_doc = |doc: segment::Doc| -> search_lucene::Result<bool> {
                let Some(id) = elastic::decode_id(doc.id) else {
                    return Err(search_lucene::Error::Corrupt(format!("document [{}] has an invalid _id", doc.doc)));
                };
                let Some(raw) = doc.source else {
                    return Err(search_lucene::Error::Unsupported(format!("document [{id}] has no stored _source")));
                };
                let source = source_json(raw, &budget2).and_then(|s| with_vectors(s, &doc.vectors)).map_err(|e| search_lucene::Error::Corrupt(format!("the _source of [{id}]: {e}")))?;
                if source.len() > source_max {
                    return Err(search_lucene::Error::Unsupported(format!("the _source of [{id}] is [{}] bytes, more than http.max_content_length [{source_max}]", source.len())));
                }
                let seq_no = doc.seq_no.or_else(|| {
                    seq_fallback += 1;
                    Some(seq_fallback - 1)
                });
                let item = Item { id, routing: doc.routing.map(str::to_string), source, seq_no, version: doc.version, primary_term: doc.primary_term };
                if tx.blocking_send(item).is_err() {
                    stop = Some(());
                    return Ok(false);
                }
                Ok(true)
            };
            let name = format!("{}.*", seg.name);
            segment::read(d, seg, &si, &vectors, &limits, &mut on_doc).map_err(|e| map(d, &name, e))?;
            if stop.is_some() {
                break;
            }
        }
        Ok(())
    });
    let target = CopyTarget::new(cluster, &ix.target)?;
    let mut indexed: EsResult<()> = Ok(());
    while let Some(item) = rx.recv().await {
        if cancelled.load(Ordering::Relaxed) {
            indexed = Err(EsError::new(400, "task_cancelled_exception", "task cancelled [by user request]"));
            break;
        }
        let r = (|| {
            let _in_flight = cluster.breakers.charge_in_flight(cluster, &cluster.state(), item.source.len() as u64)?;
            target.index(&DocCopy {
                id: item.id,
                routing: item.routing,
                source: item.source,
                seq_no: item.seq_no.unwrap_or(0),
                primary_term: item.primary_term.unwrap_or(1).max(1),
                version: item.version.unwrap_or(1).max(1),
                ignored_source: Vec::new(),
            })
        })();
        if let Err(e) = r {
            indexed = Err(e);
            break;
        }
    }
    drop(rx);
    let read = reader.await.map_err(|e| EsError::internal(e.to_string()))?;
    indexed?;
    read?;
    progress.stage.store(2, Ordering::Relaxed);
    progress.stop_ms.store(crate::time::now_millis(), Ordering::Relaxed);
    Ok(())
}

/// Starts again, on an owner that just opened the cluster, the restores from Elasticsearch
/// repositories its indices record as still running (the previous owner stopped mid-way).
pub fn resume(cluster: &Arc<crate::Cluster>) {
    let st = cluster.state();
    // (repository, snapshot, snapshot uuid) → (index, its id in the repository, shards, vectors).
    type Unfinished = HashMap<(String, String, String), Vec<(Arc<IndexMeta>, String, u32, Vec<String>)>>;
    let mut by_snapshot: Unfinished = HashMap::new();
    for idx in st.metadata.indices.values() {
        let Some(rs) = &idx.recovery_source else { continue };
        let Some(e) = rs.get("roll_elastic") else { continue };
        if e.get("state").and_then(Value::as_str) != Some("running") {
            continue;
        }
        let s = |m: &Map, k: &str| m.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let key = (s(rs, "repository"), s(rs, "snapshot"), e.get("snapshot_uuid").and_then(Value::as_str).unwrap_or_default().to_string());
        let vectors = e.get("vectors").and_then(Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()).unwrap_or_default();
        let shards = e.get("shards").and_then(Value::as_i64).unwrap_or(idx.number_of_shards() as i64) as u32;
        by_snapshot.entry(key).or_default().push((idx.clone(), e.get("index_id").and_then(Value::as_str).unwrap_or_default().to_string(), shards, vectors));
    }
    for ((repo, snap, uuid), indices) in by_snapshot {
        let cluster = cluster.clone();
        tokio::spawn(async move {
            let names: Vec<String> = indices.iter().map(|i| i.0.name.clone()).collect();
            let fail = |why: String| {
                let cluster = cluster.clone();
                let names = names.clone();
                async move {
                    for n in &names {
                        set_state(&cluster, n, "failed", Some(why.clone())).await;
                    }
                }
            };
            let Some(r) = cluster.state().metadata.repositories.get(&repo).cloned() else {
                return fail(format!("the repository [{repo}] is gone")).await;
            };
            let src = match elastic::open(&cluster, &repo, &r).await {
                Ok(Some(s)) => s,
                Ok(None) => return fail(format!("[{repo}] no longer holds an Elasticsearch repository")).await,
                Err(e) => return fail(e.reason().to_string()).await,
            };
            if cluster.snapshot_data.elastic.busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
                return fail("another restore from an Elasticsearch repository is running".into()).await;
            }
            let _busy = Busy(&cluster.snapshot_data.elastic);
            let mut jobs = Vec::new();
            for (idx, index_id, shards, vectors) in indices {
                let progress: Arc<Vec<ShardProgress>> = Arc::new((0..shards).map(|_| ShardProgress::default()).collect());
                cluster.snapshot_data.elastic.progress.lock().insert(idx.uuid.clone(), progress.clone());
                jobs.push(IndexJob { target: idx, index_id, shards, vectors, progress });
            }
            let job = Job { src: Arc::new(src), snapshot: uuid, name: snap.clone(), indices: jobs };
            let guard = cluster.tasks.start(crate::cluster::NewTask {
                action: "cluster:admin/snapshot/restore".into(),
                description: format!("restore snapshot [{repo}:{snap}] from an Elasticsearch repository (resumed)"),
                describe: None,
                parent: None,
                headers: Vec::new(),
                cancellable: true,
                cancelled: None,
            });
            let cancelled = cluster.tasks.get(guard.id).map(|t| t.cancelled).unwrap_or_default();
            let _ = run(&cluster, &job, &cancelled).await;
            drop(guard);
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_go_back_at_the_end_keeping_the_source_bytes() {
        let src = Bytes::from_static(b"{\"a\": 1, \"b\":{\"c\":2} }");
        let out = with_vectors(src.clone(), &[("v", vec![0.5, -1.0, 0.1])]).unwrap();
        assert_eq!(&out[..], b"{\"a\": 1, \"b\":{\"c\":2} ,\"v\":[0.5,-1.0,0.1]}");
        // Already there (an 8.x source): unchanged.
        assert_eq!(with_vectors(out.clone(), &[("v", vec![9.0])]).unwrap(), out);
        // Under an object: put in place.
        let out = with_vectors(src, &[("b.v", vec![1.0])]).unwrap();
        assert_eq!(&out[..], b"{\"a\":1,\"b\":{\"c\":2,\"v\":[1.0]}}");
        let out = with_vectors(Bytes::from_static(b"{}"), &[("v", vec![2.5])]).unwrap();
        assert_eq!(&out[..], b"{\"v\":[2.5]}");
    }

    #[test]
    fn settings_dropped_as_the_migration_drops_them() {
        for k in ["index.uuid", "index.version.created", "index.routing.allocation.include._tier_preference", "index.blocks.write", "index.lifecycle.name", "index.store.type"] {
            assert!(drop_reason(k).is_some(), "{k}");
        }
        for k in ["index.number_of_shards", "index.refresh_interval", "index.analysis.analyzer.x.type", "index.routing_partition_size"] {
            assert!(drop_reason(k).is_none(), "{k}");
        }
    }

    #[test]
    fn indices_without_their_source_are_refused() {
        let req = Request {
            repo: "r".into(),
            snap: "s".into(),
            wait: true,
            patterns: vec![],
            ignore_unavailable: false,
            include_global_state: false,
            include_aliases: true,
            partial: false,
            rename: None,
            index_settings: Settings::new(),
            ignore_settings: vec![],
        };
        let meta = |settings: Value, mapping: Value| obj! { "i" => obj! { "settings" => settings, "mappings" => Value::Arr(vec![obj! { "_doc" => mapping }]) } };
        let refused = |m: Value| plan_index(&req, "i", &m, "id").unwrap().err();
        assert!(refused(meta(obj! { "index.number_of_shards" => "1" }, obj! { "_source" => obj! { "enabled" => false } })).unwrap().contains("enabled: false"));
        assert!(refused(meta(obj! { "index.mapping.source.mode" => "synthetic" }, obj! {})).unwrap().contains("synthetic"));
        assert!(refused(meta(obj! { "index.mode" => "logsdb" }, obj! {})).unwrap().contains("logsdb"));
        assert!(refused(meta(obj! { "index.mode" => "logsdb", "index.mapping.source.mode" => "stored" }, obj! {})).is_none());
        let p = plan_index(&req, "i", &meta(obj! { "index.number_of_shards" => "2", "index.uuid" => "x", "index.version.created" => "9111000" }, obj! { "properties" => obj! { "v" => obj! { "type" => "dense_vector" } }, "_source" => obj! { "excludes" => Value::from(vec!["secret".to_string()]) } }), "id").unwrap().unwrap();
        assert_eq!((p.shards, p.vectors.clone()), (2, vec!["v".to_string()]));
        assert!(p.settings.get("index.uuid").is_none() && p.notes.iter().any(|n| n.contains("secret")) && p.notes.iter().any(|n| n.contains("index.uuid")));
    }

    #[test]
    fn global_state_items_skip_elasticsearchs_own() {
        let g = obj! { "meta-data" => obj! {
            "index_template" => obj! { "index_template" => obj! {
                "mine" => obj! { "index_patterns" => Value::from(vec!["m-*".to_string()]) },
                "logs" => obj! { "index_patterns" => Value::from(vec!["logs-*".to_string()]), "_meta" => obj! { "managed" => true } },
            } },
            "ingest" => obj! { "pipeline" => Value::Arr(vec![obj! { "id" => "p", "config" => obj! { "processors" => Value::Arr(vec![]) } }]) },
            "templates" => obj! { ".monitoring" => obj! {}, "old" => obj! { "index_patterns" => Value::from(vec!["o-*".to_string()]), "mappings" => obj! { "_doc" => obj! { "properties" => obj! {} } } } },
        } };
        let items = global_items(&g);
        let paths: Vec<&str> = items.iter().map(|(_, p, _)| p.as_str()).collect();
        assert_eq!(paths, ["/_ingest/pipeline/p", "/_index_template/mine", "/_template/old"]);
        assert!(items[2].2.get("mappings").and_then(|m| m.get("properties")).is_some());
    }
}
