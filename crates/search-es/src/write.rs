//! The document write path shared by the index, create, delete, bulk, update, by-query and
//! reindex APIs (Elasticsearch's `TransportBulkAction` + `TransportShardBulkAction`):
//!
//! 1. **Target.** The request's index expression becomes one concrete write index: an alias's
//!    write index, a data stream's last backing index (only `op_type=create`), or a missing
//!    index created on the fly from the matching templates (`action.auto_create_index`,
//!    `require_alias`, `require_data_stream`).
//! 2. **Validation.** `IndexRequest.validate`: id length, versioning combinations.
//! 3. **Routing.** `_routing` required checks, alias index routing, the shard by Murmur3.
//! 4. **Parsing.** The document against the mapping; a dynamic mapping update is merged and
//!    published first (the cluster state update serializes concurrent updates, and a conflicting
//!    one fails the document), then the document is parsed again against the new mapping.
//! 5. **Engine.** The parsed document becomes an `IndexableDoc` and is applied to the shard
//!    with its version checks; the outcome carries seq_no, primary term and version.
//! 6. **Log.** The applied operation goes to the shard's operation log
//!    ([`crate::engine::ShardLog`], search-server's WAL) with its seq_no, term and version, and
//!    a seq_no a failed write used up as a no-op; search-server acknowledges once it is durable.

use crate::cluster::Cluster;
use crate::engine::convert::{self, DocMeta};
use crate::engine::{IndexEngine, LoggedOp, ShardEngine};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::mapping::{MappingCtx, MergeReason};
use crate::meta::{ClusterState, IndexMeta};
use crate::obj;
use bytes::Bytes;
use search_index::{DeleteOp, IndexOp, OpResult, OpType, VersionType};
use std::sync::Arc;

/// What a write is (bulk action names).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Index,
    Create,
    Update,
    Delete,
}

impl Action {
    pub fn name(self) -> &'static str {
        match self {
            Action::Index => "index",
            Action::Create => "create",
            Action::Update => "update",
            Action::Delete => "delete",
        }
    }
}

/// Parameters of one index or delete operation.
#[derive(Clone, Debug, Default)]
pub struct WriteArgs {
    pub id: Option<String>,
    pub routing: Option<String>,
    pub create: bool,
    pub version: Option<i64>,
    pub version_type: Option<VersionType>,
    pub if_seq_no: Option<i64>,
    pub if_primary_term: Option<i64>,
    pub pipeline: Option<String>,
    pub require_alias: bool,
    pub require_data_stream: bool,
    pub dynamic_templates: Option<Map>,
    /// The op comes from an internal action (update, reindex): no versioning validation.
    pub internal: bool,
    /// Bulk `list_executed_pipelines`.
    pub list_executed_pipelines: bool,
}

/// The outcome of one write.
#[derive(Clone, Debug)]
pub struct WriteOutcome {
    pub index: String,
    pub uuid: String,
    pub id: String,
    pub shard: u32,
    pub version: i64,
    /// `created`, `updated`, `deleted`, `not_found`, `noop`.
    pub result: &'static str,
    pub seq_no: i64,
    pub primary_term: i64,
    /// Shard copies the write would go to (primary + replicas).
    pub total: u32,
    pub forced_refresh: bool,
    /// `failure_store: used` when the document went to its data stream's failure store.
    pub failure_store: Option<&'static str>,
    /// `executed_pipelines` (bulk `list_executed_pipelines`): the pipelines the document went
    /// through, outermost first.
    pub executed_pipelines: Option<Vec<String>>,
}

impl WriteOutcome {
    pub fn status(&self) -> u16 {
        match self.result {
            "created" => 201,
            "not_found" => 404,
            _ => 200,
        }
    }

    /// `DocWriteResponse.innerToXContent`.
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        self.write_into(&mut m);
        Value::Obj(m)
    }

    pub fn write_into(&self, m: &mut Map) {
        m.insert("_index".into(), Value::from(self.index.as_str()));
        m.insert("_id".into(), Value::from(self.id.as_str()));
        m.insert("_version".into(), Value::from(self.version));
        m.insert("result".into(), Value::from(self.result));
        if self.forced_refresh {
            m.insert("forced_refresh".into(), Value::Bool(true));
        }
        m.insert("_shards".into(), shards_value(self.total, if self.result == "noop" { 0 } else { 1 }));
        // Unassigned sequence numbers (indices without them) still print; dropped documents
        // have none.
        if self.seq_no >= 0 || self.result != "noop" {
            m.insert("_seq_no".into(), Value::from(self.seq_no));
            m.insert("_primary_term".into(), Value::from(self.primary_term));
        }
        if let Some(f) = self.failure_store {
            m.insert("failure_store".into(), Value::from(f));
        }
    }
}

impl WriteOutcome {
    /// `{"<action>":{...,"status":N}}`: the bulk item [`write_into`](Self::write_into) and the
    /// item status make, written straight as compact JSON (a 5,000-item bulk otherwise builds
    /// 10 maps per item only to print them). `forced_refresh` adds the flag after `result`,
    /// where the bulk handler inserts it.
    pub fn write_bulk_item(&self, out: &mut Vec<u8>, action: &str, forced_refresh: bool) {
        use crate::json::write::write_str;
        let mut num = itoa::Buffer::new();
        out.extend_from_slice(b"{");
        write_str(out, action);
        out.extend_from_slice(b":{\"_index\":");
        write_str(out, &self.index);
        out.extend_from_slice(b",\"_id\":");
        write_str(out, &self.id);
        out.extend_from_slice(b",\"_version\":");
        out.extend_from_slice(num.format(self.version).as_bytes());
        out.extend_from_slice(b",\"result\":");
        write_str(out, self.result);
        if self.forced_refresh || forced_refresh {
            out.extend_from_slice(b",\"forced_refresh\":true");
        }
        out.extend_from_slice(b",\"_shards\":{\"total\":");
        out.extend_from_slice(num.format(self.total).as_bytes());
        out.extend_from_slice(if self.result == "noop" { b",\"successful\":0,\"failed\":0}" } else { b",\"successful\":1,\"failed\":0}" });
        if self.seq_no >= 0 || self.result != "noop" {
            out.extend_from_slice(b",\"_seq_no\":");
            out.extend_from_slice(num.format(self.seq_no).as_bytes());
            out.extend_from_slice(b",\"_primary_term\":");
            out.extend_from_slice(num.format(self.primary_term).as_bytes());
        }
        if let Some(p) = &self.executed_pipelines {
            out.extend_from_slice(b",\"executed_pipelines\":[");
            for (i, name) in p.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_str(out, name);
            }
            out.push(b']');
        }
        if let Some(f) = self.failure_store {
            out.extend_from_slice(b",\"failure_store\":");
            write_str(out, f);
        }
        out.extend_from_slice(b",\"status\":");
        out.extend_from_slice(num.format(self.status()).as_bytes());
        out.extend_from_slice(b"}}");
    }
}

pub fn shards_value(total: u32, successful: u32) -> Value {
    obj! { "total" => total as i64, "successful" => successful as i64, "failed" => 0 }
}

/// Shard copies of an index (primary plus configured replicas).
pub fn copies(idx: &IndexMeta) -> u32 {
    1 + idx.number_of_replicas()
}

/// Shard-level metadata Elasticsearch attaches to engine exceptions.
pub fn shard_error(e: EsError, idx: &IndexMeta, shard: u32) -> EsError {
    e.with_meta("index_uuid", idx.uuid.as_str()).with_meta("shard", shard.to_string()).with_meta("index", idx.name.as_str())
}

/// `IndexRequest.validate` / `DeleteRequest.validate` messages.
pub fn validate_args(args: &WriteArgs, action: Action) -> Vec<String> {
    let mut errs = Vec::new();
    let vt = args.version_type.unwrap_or(VersionType::Internal);
    if let Some(id) = &args.id
        && id.len() > 512
    {
        errs.push(format!("id [{id}] is too long, must be no longer than 512 bytes but was: {}", id.len()));
    }
    if action == Action::Create || args.create {
        if vt != VersionType::Internal {
            errs.push("create operations only support internal versioning. use index instead".into());
        }
        if args.if_seq_no.is_some() || args.if_primary_term.is_some() {
            // Elasticsearch allows `if_seq_no` with create only for internal ops.
        }
    }
    if args.internal {
        return errs;
    }
    let cas = args.if_seq_no.is_some() || args.if_primary_term.is_some();
    if cas && (args.version.is_some() || vt != VersionType::Internal) {
        errs.push("compare and write operations can not use versioning".into());
    }
    if let Some(s) = args.if_seq_no
        && s < 0
    {
        errs.push(format!("sequence number must be non negative. got [{s}]."));
    }
    if let Some(t) = args.if_primary_term
        && t < 0
    {
        errs.push(format!("primary term must be non negative. got [{t}]"));
    }
    if args.if_seq_no.is_some() && args.if_primary_term.unwrap_or(0) == 0 {
        errs.push(format!("ifSeqNo is set, but primary term is [{}]", args.if_primary_term.unwrap_or(0)));
    }
    if args.if_seq_no.is_none() && args.if_primary_term.is_some_and(|t| t != 0) {
        errs.push(format!("ifSeqNo is unassigned, but primary term is [{}]", args.if_primary_term.unwrap_or(0)));
    }
    match vt {
        VersionType::Internal => {
            if args.version.is_some() && !cas && action != Action::Create && !args.create {
                errs.push("internal versioning can not be used for optimistic concurrency control. Please use `if_seq_no` and `if_primary_term` instead".into());
            }
        }
        VersionType::External | VersionType::ExternalGte => {
            let v = args.version.unwrap_or(-3);
            if v < 0 {
                let name = if vt == VersionType::External { "EXTERNAL" } else { "EXTERNAL_GTE" };
                errs.push(format!("illegal version value [{v}] for version type [{name}]"));
            }
        }
    }
    errs
}

/// `version_type` parameter values.
pub fn parse_version_type(s: &str) -> EsResult<VersionType> {
    match s {
        "internal" => Ok(VersionType::Internal),
        "external" => Ok(VersionType::External),
        "external_gte" => Ok(VersionType::ExternalGte),
        other => Err(EsError::illegal_argument(format!("No version type match [{other}]"))),
    }
}

/// `AutoCreateIndex.shouldAutoCreate` for a write to `name` that names no existing index: the
/// matching composable template's `allow_auto_create`, else `action.auto_create_index`.
pub fn check_may_auto_create(st: &ClusterState, name: &str) -> EsResult<()> {
    if write_index_of(st, name).ok().flatten().is_some() {
        return Ok(());
    }
    // An ES|QL view's name is no index to write to (Elasticsearch resolves it as missing).
    if crate::xpack::esql::is_view(st, name) {
        return Err(EsError::index_not_found(name));
    }
    match crate::meta::templates::find_composable(&st.metadata, name, false).map(|(n, t)| (n.clone(), t.allow_auto_create)) {
        Some((n, Some(false))) => Err(EsError::index_not_found_msg(name, format!("no such index [{name}] and composable template [{n}] forbids index auto creation"))),
        Some((_, Some(true))) => Ok(()),
        _ => check_auto_create(st, name),
    }
}

/// `action.auto_create_index` against `name`: `Ok(())` when it may be created.
fn check_auto_create(st: &ClusterState, name: &str) -> EsResult<()> {
    let setting = st.metadata.cluster_setting("action.auto_create_index").unwrap_or("true").to_string();
    match setting.as_str() {
        "true" => return Ok(()),
        "false" => {
            return Err(EsError::index_not_found_msg(name, format!("no such index [{name}] and [action.auto_create_index] is [false]")));
        }
        _ => {}
    }
    for p in setting.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (include, pat) = match p.as_bytes()[0] {
            b'+' => (true, &p[1..]),
            b'-' => (false, &p[1..]),
            _ => (true, p),
        };
        if crate::xcontent::filter::glob_match(pat, name) {
            if include {
                return Ok(());
            }
            return Err(EsError::index_not_found_msg(
                name,
                format!("no such index [{name}] and [action.auto_create_index] contains [{p}] which forbids automatic creation of the index"),
            ));
        }
    }
    Err(EsError::index_not_found_msg(name, format!("no such index [{name}] and [action.auto_create_index] ([{setting}]) doesn't match")))
}

/// The concrete index a write to `expr` goes to, with the routing an alias imposes.
pub struct Target {
    pub index: Arc<IndexMeta>,
    /// The alias's `index_routing`, when written through one.
    pub alias_routing: Option<String>,
    pub alias: Option<String>,
    pub data_stream: Option<String>,
}

/// The existing index a write to `expr` lands in (index, data stream or alias), if any.
pub fn resolve_existing(st: &ClusterState, expr: &str) -> Option<Arc<IndexMeta>> {
    write_index_of(st, expr).ok().flatten().map(|t| t.index)
}

fn write_index_of(st: &ClusterState, expr: &str) -> EsResult<Option<Target>> {
    if let Some(idx) = st.metadata.indices.get(expr) {
        return Ok(Some(Target { index: idx.clone(), alias_routing: None, alias: None, data_stream: st.metadata.data_stream_of(expr).map(|d| d.name.clone()) }));
    }
    if let Some(ds) = st.metadata.data_streams.get(expr) {
        let Some(w) = ds.write_index() else { return Ok(None) };
        let idx = st.metadata.indices.get(w).cloned().ok_or_else(|| EsError::index_not_found(w))?;
        return Ok(Some(Target { index: idx, alias_routing: None, alias: None, data_stream: Some(ds.name.clone()) }));
    }
    if let Some(dsa) = st.metadata.data_stream_aliases.get(expr) {
        let Some(w) = dsa.write_data_stream.as_ref() else {
            return Err(EsError::illegal_argument(format!(
                "no write index is defined for alias [{expr}]. The write index may be explicitly disabled using is_write_index=false or the alias points to multiple indices without one being designated as a write index"
            )));
        };
        return write_index_of(st, w);
    }
    let aliases = st.aliases();
    if let Some(members) = aliases.get(expr) {
        let writer = members.iter().find(|(_, a)| a.is_write_index == Some(true)).or_else(|| {
            if members.len() == 1 && members[0].1.is_write_index != Some(false) { Some(&members[0]) } else { None }
        });
        let Some((idx, a)) = writer else {
            return Err(EsError::illegal_argument(format!(
                "no write index is defined for alias [{expr}]. The write index may be explicitly disabled using is_write_index=false or the alias points to multiple indices without one being designated as a write index"
            )));
        };
        let idx = st.metadata.indices.get(&idx.name).cloned().ok_or_else(|| EsError::index_not_found(expr))?;
        return Ok(Some(Target { index: idx, alias_routing: a.index_routing.clone(), alias: Some(expr.to_string()), data_stream: None }));
    }
    Ok(None)
}

/// The write index of a delete. `TransportBulkAction` creates missing indices only for
/// deletes with external versioning; any other delete of a missing index fails (OpenSearch's
/// resolver reports the expression as an `index_expression` resource).
pub async fn resolve_delete_target(cluster: &Cluster, expr: &str, args: &WriteArgs, now_ms: i64) -> EsResult<Target> {
    let name = crate::meta::resolve::resolve_date_math(expr, now_ms)?;
    let external = matches!(args.version_type, Some(VersionType::External | VersionType::ExternalGte));
    if !external && write_index_of(&cluster.state(), &name)?.is_none() && !crate::meta::resolve::is_wildcard(&name) {
        let e = EsError::index_not_found(&name);
        return Err(if cluster.dialect == crate::cluster::Dialect::OpenSearch { e.set_meta("resource.type", "index_expression") } else { e });
    }
    let mut a = args.clone();
    // Deletes may target data streams' backing indices directly; the data stream itself only
    // takes creates, which `resolve_target` enforces for index requests.
    a.create = true;
    resolve_target(cluster, expr, &a, now_ms).await
}

/// Resolves (and if needed creates) the write index for `expr`.
pub async fn resolve_target(cluster: &Cluster, expr: &str, args: &WriteArgs, now_ms: i64) -> EsResult<Target> {
    let name = crate::meta::resolve::resolve_date_math(expr, now_ms)?;
    let st = cluster.state();
    if args.require_alias && !st.has_alias(&name) && !st.metadata.data_stream_aliases.contains_key(&name) {
        return Err(EsError::index_not_found_msg(&name, format!("no such index [{name}] and [require_alias] request flag is [true] and [{name}] is not an alias")));
    }
    if let Some(t) = write_index_of(&st, &name)? {
        // A write through a data stream marked for lazy rollover rolls it over first.
        if let Some(ds) = t.data_stream.clone()
            && t.index.name != name
            && st.metadata.data_streams.get(&ds).is_some_and(|d| d.rollover_on_write)
        {
            cluster.update(move |st| crate::api::rollover::rollover_on_write(st, &ds, now_ms)).await?;
            let st = cluster.state();
            if let Some(t) = write_index_of(&st, &name)? {
                check_target(&st, &t, args, &name)?;
                return Ok(t);
            }
        }
        check_target(&st, &t, args, &name)?;
        return Ok(t);
    }
    if crate::meta::resolve::is_wildcard(&name) {
        return Err(EsError::invalid_index_name(&name, "must not contain the following characters [ , \", *, \\, <, |, ,, >, /, ?]"));
    }
    // An ES|QL view's name is no index to write to (Elasticsearch resolves it as missing).
    if crate::xpack::esql::is_view(&st, &name) {
        return Err(EsError::index_not_found(&name));
    }
    // Auto-create: a data stream when a data stream template matches, an index otherwise.
    let template = crate::meta::templates::find_composable(&st.metadata, &name, false).map(|(n, t)| (n.clone(), t.data_stream.is_some(), t.allow_auto_create));
    if args.require_data_stream && !template.as_ref().is_some_and(|t| t.1) {
        return Err(EsError::new(404, "index_not_found_exception", format!("no such index [{name}] and the index creation request requires a data stream, but no matching index template with data stream template was found for it"))
            .with_meta("index_uuid", "_na_")
            .with_meta("index", name.as_str()));
    }
    match &template {
        Some((_, _, Some(false))) => {
            return Err(EsError::index_not_found_msg(&name, format!("no such index [{name}] and composable template [{}] forbids index auto creation", template.as_ref().unwrap().0)));
        }
        Some((_, _, Some(true))) => {}
        _ => check_auto_create(&st, &name)?,
    }
    let is_ds = template.as_ref().is_some_and(|t| t.1);
    let n2 = name.clone();
    let created = cluster
        .update(move |st| {
            if write_index_of(st, &n2)?.is_some() {
                return Ok(());
            }
            if is_ds {
                crate::api::data_streams::create_data_stream(st, &n2, now_ms)?;
            } else {
                crate::meta::names::validate_index_name(&n2)?;
                crate::api::indices::create_index(st, &n2, &n2, Default::default(), now_ms)?;
            }
            Ok(())
        })
        .await;
    created?;
    let st = cluster.state();
    let t = write_index_of(&st, &name)?.ok_or_else(|| EsError::index_not_found(&name))?;
    check_target(&st, &t, args, &name)?;
    Ok(t)
}

fn check_target(st: &ClusterState, t: &Target, args: &WriteArgs, expr: &str) -> EsResult<()> {
    if t.data_stream.is_some() && st.metadata.data_streams.contains_key(expr) && !args.create {
        return Err(EsError::illegal_argument("only write ops with an op_type of create are allowed in data streams"));
    }
    if args.require_data_stream && t.data_stream.is_none() {
        return Err(EsError::resource_not_found(format!("[require_data_stream] request flag is [true] and [{expr}] is not a data stream")));
    }
    if t.index.is_closed() {
        return Err(EsError::closed_index(&t.index.name, &t.index.uuid));
    }
    crate::meta::blocks::check(std::iter::once(t.index.as_ref()), crate::meta::blocks::Level::Write)?;
    Ok(())
}

/// The routing of a write: the request's, or the alias's index routing.
pub fn effective_routing(t: &Target, routing: Option<&str>, id: &str) -> EsResult<Option<String>> {
    match (&t.alias_routing, routing) {
        (Some(ar), Some(r)) if ar != r => Err(EsError::illegal_argument(format!(
            "Alias [{}] has index routing associated with it [{ar}], and was provided with routing value [{r}], rejecting operation",
            t.alias.as_deref().unwrap_or("")
        ))),
        (Some(ar), _) => Ok(Some(ar.clone())),
        (None, r) => {
            let _ = id;
            Ok(r.map(str::to_string))
        }
    }
}

/// `routing is required for [index]/[id]`.
pub fn check_routing(idx: &IndexMeta, id: &str, routing: Option<&str>) -> EsResult<()> {
    if routing.is_none() && idx.mapping.routing_required == Some(true) {
        return Err(EsError::new(400, "routing_missing_exception", format!("routing is required for [{}]/[{id}]", idx.name))
            .with_meta("index_uuid", "_na_")
            .with_meta("index", idx.name.as_str()));
    }
    Ok(())
}

/// A parsed document ready for the engine.
pub struct Prepared {
    pub engine: Arc<IndexEngine>,
    pub shard: Arc<ShardEngine>,
    pub index: Arc<IndexMeta>,
    pub doc: search_index::IndexableDoc,
    /// The document's `_id` (generated for time series indices).
    pub id: String,
    /// Time series documents: `tsid@timestamp`, as version conflicts name them.
    pub tsdb: Option<String>,
    /// The document as indexed and the routing it was prepared with: what the shard's
    /// operation log keeps to apply it again on recovery.
    pub source: Bytes,
    pub routing: Option<String>,
}

/// Parses `source` for `idx` (merging a dynamic mapping update first when the document needs
/// one) and builds the engine document.
///
/// `dynamic_templates` is the request's field path → dynamic template name map.
pub async fn prepare(cluster: &Cluster, idx: Arc<IndexMeta>, id: &str, routing: Option<&str>, source: &Bytes, value: &Value, dynamic_templates: Option<&Map>) -> EsResult<Prepared> {
    let mut idx = idx;
    let requested = dynamic_templates.filter(|m| !m.is_empty()).map(|m| Arc::new(m.clone()));
    for _attempt in 0..8 {
        let st = cluster.state();
        let mut ctx = idx.mapping_ctx();
        ctx.requested_templates = requested.clone();
        ctx.join_routing = Some(routing.is_some());
        let parsed = crate::document::parse_document(&idx.mapping, &ctx, value, Some(source), Some(id))?;
        if let Some(update) = parsed.dynamic_update {
            idx = apply_dynamic_update(cluster, &idx, update).await?;
            continue;
        }
        // `dense_vector` fields without `dims` take them from their first vector.
        let mut dims: Vec<(String, i64)> = Vec::new();
        for f in parsed.fields.iter().chain(parsed.nested.iter().flat_map(|n| n.fields.iter())) {
            let n = match &f.value {
                crate::document::FieldValue::DenseVector(v) => v.len() as i64,
                crate::document::FieldValue::ByteVector(v) => v.len() as i64,
                _ => continue,
            };
            if let Some(crate::mapping::MapperRef::Field(fm)) = idx.mapping.lookup(&f.name)
                && fm.ty == "dense_vector"
                && fm.raw("dims").is_none()
                && !dims.iter().any(|(p, _)| p.as_str() == &*f.name)
            {
                let n = if fm.effective("element_type", &ctx).as_str() == Some("bit") { n * 8 } else { n };
                dims.push((f.name.to_string(), n));
            }
        }
        if !dims.is_empty() {
            // The dims the document implies must pass the mapping's validation (bbq needs 64+,
            // int4 even counts, 1..=4096), or the update would be a mapping that can't load.
            for (path, n) in &dims {
                if let Some(crate::mapping::MapperRef::Field(fm)) = idx.mapping.lookup(path) {
                    let mut f = fm.clone();
                    f.set("dims", Value::from(*n));
                    f.validate_type(&ctx)?;
                }
            }
            idx = set_vector_dims(cluster, &idx, dims).await?;
            continue;
        }
        let engine = cluster.engine.index(cluster, &st, &idx)?;
        let table = engine.table();
        // The table never lags the metadata it was synced with (`Engine::sync_table` only moves
        // forward), and fields absent from it would be silently left unindexed: never convert
        // against a table older than the mapping the document was parsed with.
        if table.key.0 < idx.mapping_version {
            continue;
        }
        // Percolator queries must convert against the mapping (unmapped fields fail).
        for f in &parsed.fields {
            if let crate::document::FieldValue::Percolator(q, (l, c)) = &f.value {
                crate::search::percolate::check_query(cluster, &st, &idx, &table, &engine, q)
                    .map_err(|e| EsError::document_parsing(format!("[{l}:{c}] failed to parse: {}", e.reason())).caused_by(e))?;
            }
        }
        let stored = stored_source(&idx, &table, source, value);
        if crate::engine::tsdb::is_time_series(&idx) {
            let (gen_id, tsid, desc) = time_series_id(&idx, &parsed, id, routing).map_err(|e| {
                // Raised after the document was read: located at its closing brace.
                if e.kind == "illegal_argument_exception" {
                    let (l, c) = end_location(source);
                    EsError::document_parsing(format!("[{l}:{c}] failed to parse: {}", e.reason())).caused_by(e)
                } else {
                    e
                }
            })?;
            let meta = DocMeta { id: &gen_id, routing: None, source: stored, tsid: Some((&tsid, routing.unwrap_or(""))) };
            let doc = convert::to_indexable(&table, &parsed, &meta)?;
            let shard_id = crate::meta::routing::shard_for(&idx, &gen_id, routing);
            let shard = engine.shard(shard_id).clone();
            return Ok(Prepared { engine, shard, index: idx, doc, id: gen_id, tsdb: Some(desc), source: source.clone(), routing: routing.map(str::to_string) });
        }
        // Data stream documents need their timestamp (`DataStreamTimestampFieldMapper`).
        if idx.mapping.data_stream_timestamp == Some(true) {
            let n = parsed.fields.iter().filter(|f| &*f.name == "@timestamp").count();
            let problem = match n {
                0 => Some("data stream timestamp field [@timestamp] is missing"),
                1 => None,
                _ => Some("data stream timestamp field [@timestamp] encountered multiple values"),
            };
            if let Some(msg) = problem {
                let (l, c) = end_location(source);
                let cause = EsError::illegal_argument(msg);
                return Err(EsError::document_parsing(format!("[{l}:{c}] failed to parse: {msg}")).caused_by(cause));
            }
        }
        let meta = DocMeta { id, routing, source: stored, tsid: None };
        let doc = convert::to_indexable(&table, &parsed, &meta)?;
        let shard_id = crate::meta::routing::shard_for(&idx, id, routing);
        let shard = engine.shard(shard_id).clone();
        return Ok(Prepared { engine, shard, index: idx, doc, id: id.to_string(), tsdb: None, source: source.clone(), routing: routing.map(str::to_string) });
    }
    Err(EsError::illegal_state("mapping updates did not converge"))
}

/// Jackson's location of a JSON document's last token (its closing brace).
fn end_location(src: &[u8]) -> (usize, usize) {
    let end = src.iter().rposition(|b| !b.is_ascii_whitespace()).unwrap_or(0);
    let text = String::from_utf8_lossy(&src[..=end.min(src.len().saturating_sub(1))]);
    let line = text.matches('\n').count() + 1;
    let col = text.rsplit('\n').next().map_or(0, |l| l.chars().count());
    (line, col)
}

/// The `_id`, `_tsid` and `tsid@timestamp` of a time series document
/// (`TsidExtractingIdFieldMapper`, `DataStreamTimestampFieldMapper`): the routing is the
/// encoded routing hash; a requested `id` must be the one generated.
pub(crate) fn time_series_id(idx: &IndexMeta, parsed: &crate::document::ParsedDocument, id: &str, routing: Option<&str>) -> EsResult<(String, Vec<u8>, String)> {
    use crate::engine::tsdb;
    let nanos = matches!(idx.mapping.lookup("@timestamp"), Some(crate::mapping::MapperRef::Field(f)) if f.ty == "date_nanos");
    let ts = parsed.fields.iter().find(|f| &*f.name == "@timestamp").and_then(|f| match f.value {
        crate::document::FieldValue::Date(v) | crate::document::FieldValue::Long(v) => Some(v),
        _ => None,
    });
    let Some(ts) = ts else {
        return Err(EsError::illegal_argument("data stream timestamp field [@timestamp] is missing"));
    };
    let instant = |v: i64, nanos: bool| -> String {
        if nanos {
            crate::time::java_instant_string_nanos(crate::time::Instant { secs: v.div_euclid(1_000_000_000), nanos: v.rem_euclid(1_000_000_000) as u32 })
        } else {
            crate::time::java_instant_string(v)
        }
    };
    let ms = if nanos { ts.div_euclid(1_000_000) } else { ts };
    let bound = |key: &str| idx.settings.get(key).and_then(|v| crate::meta::index_settings::parse_date_setting(v).ok());
    if let Some(start) = bound("index.time_series.start_time")
        && ms < start
    {
        return Err(EsError::illegal_argument(format!("time series index @timestamp value [{}] must be larger than {}", instant(ts, nanos), crate::time::java_instant_string(start))));
    }
    if let Some(end) = bound("index.time_series.end_time")
        && ms >= end
    {
        return Err(EsError::illegal_argument(format!("time series index @timestamp value [{}] must be smaller than {}", instant(ts, nanos), crate::time::java_instant_string(end))));
    }
    let dims = tsdb::dimensions(idx, parsed);
    let tsid = tsdb::build_tsid(&dims);
    let hash = routing.and_then(tsdb::decode_hash).unwrap_or(0);
    let generated = if tsdb::synthetic_ids(idx) { tsdb::create_synthetic_id(hash, &tsid, ts) } else { tsdb::create_id(hash, &tsid, ts) };
    if !id.is_empty() && id != generated {
        return Err(EsError::illegal_argument(format!(
            "_id must be unset or set to [{generated}] but was [{id}] because [{}] is in time_series mode",
            idx.name
        )));
    }
    let desc = tsdb::describe(&tsid, &crate::time::java_instant_string(ms));
    Ok((generated, tsid, desc))
}

/// The `_source` bytes the index keeps: as sent, filtered by the mapping's `_source`
/// includes/excludes, or nothing when `_source` is disabled.
pub fn stored_source(idx: &IndexMeta, table: &crate::engine::fields::FieldTable, source: &Bytes, value: &Value) -> Option<Bytes> {
    use crate::engine::synthetic;
    let sc = &idx.mapping.source;
    match synthetic::source_mode(idx) {
        synthetic::SourceMode::Disabled => return None,
        synthetic::SourceMode::Synthetic => {
            let Value::Obj(m) = value else { return Some(source.clone()) };
            let ctx = idx.mapping_ctx();
            let doc = synthetic::synthesize(&idx.mapping, &ctx, table, synthetic::options(idx), m);
            return Some(Bytes::from(Value::Obj(doc).to_json()));
        }
        synthetic::SourceMode::Stored => {}
    }
    if sc.enabled == Some(false) {
        return None;
    }
    // OpenSearch k-NN indices derive `knn_vector` values from the stored vectors.
    let derived;
    let (source, value) = match value {
        Value::Obj(m) if crate::opensearch::knn::derives_source(idx) => {
            let mut m = m.clone();
            if crate::opensearch::knn::derive_source(idx, &mut m) {
                derived = (Bytes::from(Value::Obj(m.clone()).to_json()), Value::Obj(m));
                (&derived.0, &derived.1)
            } else {
                (source, value)
            }
        }
        _ => (source, value),
    };
    let inc = sc.includes.clone().unwrap_or_default();
    let exc = sc.excludes.clone().unwrap_or_default();
    if inc.is_empty() && exc.is_empty() {
        return Some(source.clone());
    }
    let Value::Obj(m) = value else { return Some(source.clone()) };
    let f = crate::engine::source::SourceFilter::new(&inc, &exc);
    Some(Bytes::from(Value::Obj(f.apply(m)).to_json()))
}

/// Merges a document's dynamic mapping update into the index's mapping and publishes it.
pub async fn apply_dynamic_update(cluster: &Cluster, idx: &IndexMeta, update: crate::mapping::Mapping) -> EsResult<Arc<IndexMeta>> {
    // Elasticsearch's shards start with the index, so what they configure from the mapping
    // when they open (the `@timestamp` segment order) predates the first document's update;
    // roll opens them lazily, so open them now, before the mapping changes.
    if cluster.engine.loaded(&idx.uuid).is_none() {
        cluster.engine.index(cluster, &cluster.state(), idx)?;
    }
    let name = idx.name.clone();
    let uuid = idx.uuid.clone();
    cluster
        .update(move |st| {
            let Some(cur) = st.metadata.indices.get(&name).cloned() else { return Err(EsError::index_not_found(&name)) };
            if cur.uuid != uuid {
                return Err(EsError::index_not_found(&name));
            }
            let ctx: MappingCtx = cur.mapping_ctx();
            let merged = cur.mapping.merge(&update, &ctx, MergeReason::MappingUpdate)?;
            merged.validate(&ctx)?;
            // `IndexMode.validateMapping` on the dynamic update (routing fields must be
            // dimensions, not runtime fields).
            let mode = crate::meta::index_mode::IndexMode::of(&cur.settings);
            if mode == crate::meta::index_mode::IndexMode::TimeSeries {
                crate::meta::index_mode::validate_routing_path(mode, &merged, &cur.settings)?;
            }
            if merged != *cur.mapping {
                let mut ni = (*cur).clone();
                ni.mapping = Arc::new(merged);
                ni.mapping_version += 1;
                ni.version += 1;
                let ni = Arc::new(ni);
                st.metadata.indices.insert(name.clone(), ni.clone());
                return Ok(ni);
            }
            Ok(cur)
        })
        .await
}

/// Sets `dims` of `dense_vector` fields (the first document decides them).
async fn set_vector_dims(cluster: &Cluster, idx: &IndexMeta, dims: Vec<(String, i64)>) -> EsResult<Arc<IndexMeta>> {
    let name = idx.name.clone();
    cluster
        .update(move |st| {
            let Some(cur) = st.metadata.indices.get(&name).cloned() else { return Err(EsError::index_not_found(&name)) };
            let mut mapping = (*cur.mapping).clone();
            let mut changed = false;
            for (path, n) in &dims {
                if let Some(f) = field_mut(&mut mapping.root, path)
                    && f.raw("dims").is_none()
                {
                    f.set("dims", Value::from(*n));
                    changed = true;
                }
            }
            if !changed {
                return Ok(cur);
            }
            let mut ni = (*cur).clone();
            ni.mapping = Arc::new(mapping);
            ni.mapping_version += 1;
            ni.version += 1;
            let ni = Arc::new(ni);
            st.metadata.indices.insert(name.clone(), ni.clone());
            Ok(ni)
        })
        .await
}

/// The field mapper at a dotted path (objects walked, dotted leaf names tried whole).
fn field_mut<'a>(o: &'a mut crate::mapping::ObjectMapper, path: &str) -> Option<&'a mut crate::mapping::FieldMapper> {
    // Find the split first (immutably), then descend once.
    let split = if o.children.contains_key(path) {
        None
    } else {
        path.match_indices('.').map(|(i, _)| i).find(|&i| matches!(o.children.get(&path[..i]), Some(crate::mapping::Mapper::Object(_))))
    };
    match split {
        None => match o.children.get_mut(path) {
            Some(crate::mapping::Mapper::Field(f)) => Some(f),
            _ => None,
        },
        Some(i) => match o.children.get_mut(&path[..i]) {
            Some(crate::mapping::Mapper::Object(child)) => field_mut(child, &path[i + 1..]),
            _ => None,
        },
    }
}

/// `index.disable_sequence_numbers`: responses carry the unassigned sequence number (`-2`)
/// and primary term (`0`), and sequence-number-based features are refused.
pub fn seq_no_disabled(idx: &IndexMeta) -> bool {
    match idx.settings.get("index.disable_sequence_numbers") {
        Some(v) => v == "true",
        // The strict columnar modes default to no sequence numbers.
        None => crate::meta::index_mode::IndexMode::of(&idx.settings).is_strict_columnar(),
    }
}

/// The unassigned `(seq_no, primary_term)` of indices without sequence numbers.
fn visible_seq(idx: &IndexMeta, seq_no: i64, primary_term: i64) -> (i64, i64) {
    if seq_no_disabled(idx) { (-2, 0) } else { (seq_no, primary_term) }
}

/// `OCCNotSupportedException` / `UpdateNotSupportedException` of indices without sequence
/// numbers.
pub fn seq_no_unsupported(idx: &IndexMeta, shard: u32, kind: &'static str, what: &str) -> EsError {
    shard_error(EsError::new(400, kind, format!("{what} not supported on indices with sequence numbers disabled")), idx, shard)
}

/// Applies an index operation to its shard.
pub fn apply_index(p: Prepared, args: &WriteArgs, auto_id: bool, id: &str) -> EsResult<WriteOutcome> {
    if args.if_seq_no.is_some() && seq_no_disabled(&p.index) {
        return Err(seq_no_unsupported(&p.index, p.shard.id, "o_c_c_not_supported_exception", "Optimistic concurrency control is"));
    }
    let mut op = IndexOp::new(p.doc);
    op.op_type = if args.create { OpType::Create } else { OpType::Index };
    op.version = args.version;
    op.version_type = args.version_type.unwrap_or(VersionType::Internal);
    op.if_seq_no = args.if_seq_no;
    op.if_primary_term = args.if_primary_term;
    op.auto_generated_id = auto_id;
    if let Some(log) = &p.shard.log {
        log.check()?;
    }
    let t = std::time::Instant::now();
    let r = p.shard.shard.index(op);
    let c = &p.shard.counters;
    c.index_time_nanos.fetch_add(t.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
    let r = match r {
        Ok(r) => r,
        Err(e) => {
            c.index_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if let Some((seq_no, primary_term)) = e.seq_no() {
                p.shard.wrote();
                log_op(&p.shard, &LoggedOp::NoOp { seq_no, primary_term })?;
            }
            return Err(convert::index_error(e));
        }
    };
    p.shard.wrote();
    match r {
        OpResult::Indexed { seq_no, primary_term, version, created } => {
            log_op(&p.shard, &LoggedOp::Index { id: &p.id, routing: p.routing.as_deref(), source: &p.source, seq_no, primary_term, version })?;
            c.index_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let (seq_no, primary_term) = visible_seq(&p.index, seq_no, primary_term);
            Ok(WriteOutcome {
                index: p.index.name.clone(),
                uuid: p.index.uuid.clone(),
                id: id.to_string(),
                shard: p.shard.id,
                version,
                result: if created { "created" } else { "updated" },
                seq_no,
                primary_term,
                total: copies(&p.index),
                forced_refresh: false,
                failure_store: None,
                executed_pipelines: None,
            })
        }
        OpResult::Conflict(vc) => {
            c.index_failed.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let mut reason = vc.reason(id);
            if let Some(d) = &p.tsdb {
                // `[id][tsid@timestamp]: …` for time series documents.
                let head = format!("[{id}]");
                if let Some(rest) = reason.strip_prefix(&head) {
                    reason = format!("{head}[{d}]{rest}");
                }
            }
            Err(shard_error(EsError::new(409, "version_conflict_engine_exception", reason), &p.index, p.shard.id))
        }
        other => Err(EsError::internal(format!("unexpected index outcome {other:?}"))),
    }
}

/// Deletes a document.
pub fn apply_delete(cluster: &Cluster, idx: &Arc<IndexMeta>, id: &str, routing: Option<&str>, args: &WriteArgs) -> EsResult<WriteOutcome> {
    if crate::engine::tsdb::is_time_series(idx) && crate::engine::tsdb::id_routing_hash(idx, id).is_none() {
        return Err(crate::engine::tsdb::invalid_id(idx, id));
    }
    let st = cluster.state();
    let engine = cluster.engine.index(cluster, &st, idx)?;
    let shard_id = crate::meta::routing::shard_for(idx, id, routing);
    let shard = engine.shard(shard_id);
    if args.if_seq_no.is_some() && seq_no_disabled(idx) {
        return Err(seq_no_unsupported(idx, shard.id, "o_c_c_not_supported_exception", "Optimistic concurrency control is"));
    }
    let mut op = DeleteOp::new(Bytes::copy_from_slice(id.as_bytes()));
    op.version = args.version;
    op.version_type = args.version_type.unwrap_or(VersionType::Internal);
    op.if_seq_no = args.if_seq_no;
    op.if_primary_term = args.if_primary_term;
    if let Some(log) = &shard.log {
        log.check()?;
    }
    let t = std::time::Instant::now();
    let r = shard.shard.delete(op).map_err(convert::index_error)?;
    shard.counters.delete_time_nanos.fetch_add(t.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
    shard.wrote();
    match r {
        OpResult::Deleted { seq_no, primary_term, version, found } => {
            log_op(shard, &LoggedOp::Delete { id, seq_no, primary_term, version })?;
            if found {
                shard.counters.delete_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            let (seq_no, primary_term) = visible_seq(idx, seq_no, primary_term);
            Ok(WriteOutcome {
                index: idx.name.clone(),
                uuid: idx.uuid.clone(),
                id: id.to_string(),
                shard: shard.id,
                version,
                result: if found { "deleted" } else { "not_found" },
                seq_no,
                primary_term,
                total: copies(idx),
                forced_refresh: false,
                failure_store: None,
                executed_pipelines: None,
            })
        }
        OpResult::Conflict(vc) => Err(shard_error(EsError::new(409, "version_conflict_engine_exception", vc.reason(id)), idx, shard.id)),
        other => Err(EsError::internal(format!("unexpected delete outcome {other:?}"))),
    }
}

/// Appends an applied operation to the shard's log (search-server's WAL), if it has one, and
/// counts it in the shard's translog stats.
fn log_op(shard: &ShardEngine, op: &LoggedOp<'_>) -> EsResult<()> {
    if let Some(log) = &shard.log {
        log.append(op)?;
    }
    shard.logged(op);
    Ok(())
}

/// The `refresh` parameter of write APIs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefreshPolicy {
    None,
    Immediate,
    WaitFor,
}

impl RefreshPolicy {
    pub fn parse(v: Option<&str>) -> EsResult<RefreshPolicy> {
        match v {
            None | Some("false") => Ok(RefreshPolicy::None),
            Some("" | "true") => Ok(RefreshPolicy::Immediate),
            Some("wait_for") => Ok(RefreshPolicy::WaitFor),
            Some(o) => Err(EsError::illegal_argument(format!("Unknown value for refresh: [{o}]."))),
        }
    }
}

/// Refreshes the shards written (`refresh=true|wait_for`).
pub fn refresh_shards(cluster: &Cluster, written: &[(String, u32)]) -> EsResult<()> {
    let mut seen: Vec<&(String, u32)> = Vec::new();
    let mut engines = Vec::new();
    for w in written {
        if seen.contains(&w) {
            continue;
        }
        seen.push(w);
        if let Some(e) = cluster.engine.loaded(&w.0) {
            engines.push((e, w.1));
        }
    }
    crate::engine::refresh_shards(&engines.iter().map(|(e, s)| &**e.shard(*s)).collect::<Vec<_>>())
}

/// Copies every document of `src` into `dst` keeping ids, routing, versions and sequence
/// numbers (shrink, split, clone and restore start from the source's documents).
pub fn copy_index(cluster: &Cluster, src: &Arc<IndexMeta>, dst: &Arc<IndexMeta>) -> EsResult<()> {
    let Some(se) = cluster.engine.loaded(&src.uuid) else { return Ok(()) };
    let mut searchers = Vec::with_capacity(se.shards.len());
    for shard in &se.shards {
        shard.shard.refresh().map_err(convert::index_error)?;
        searchers.push(shard.searcher());
    }
    copy_docs(cluster, &searchers, dst)
}

/// Indexes every document of `searchers` (an index's shards as of some point, e.g. a snapshot)
/// into `dst` keeping ids, routing, versions and sequence numbers, logged like writes.
pub fn copy_docs(cluster: &Cluster, searchers: &[search_index::Searcher], dst: &Arc<IndexMeta>) -> EsResult<()> {
    let st = cluster.state();
    let de = cluster.engine.index(cluster, &st, dst)?;
    let table = de.table();
    let ctx = dst.mapping_ctx();
    let root = search_index::query::nested::root_filter();
    for searcher in searchers {
        let mut docs = Vec::new();
        struct All<'a>(&'a mut Vec<u32>);
        struct Leaf<'a> {
            out: &'a mut Vec<u32>,
            base: u32,
        }
        impl search_index::LeafCollector for Leaf<'_> {
            fn collect(&mut self, doc: u32, _s: &mut dyn search_index::Scorable) -> search_index::Result<()> {
                self.out.push(self.base + doc);
                Ok(())
            }
        }
        impl search_index::Collector for All<'_> {
            fn score_mode(&self) -> search_index::ScoreMode {
                search_index::ScoreMode::CompleteNoScores
            }
            fn leaf<'a>(&'a mut self, seg: &search_index::SegmentContext) -> search_index::Result<Box<dyn search_index::LeafCollector + 'a>> {
                Ok(Box::new(Leaf { out: self.0, base: seg.base }))
            }
        }
        searcher.search(&root, &mut All(&mut docs)).map_err(convert::index_error)?;
        for doc in docs {
            let Some((seg, local)) = searcher.segment_of(doc) else { continue };
            let stored = searcher.stored(doc).map_err(convert::index_error)?;
            let id = crate::search::hits::stored_id(&stored).unwrap_or_default();
            let routing = crate::fetch::routing(&stored);
            let source = crate::fetch::source_bytes(&stored).map(Bytes::copy_from_slice).unwrap_or_else(|| Bytes::from_static(b"{}"));
            let value = crate::fetch::parse_source(&source)?;
            let parsed = crate::document::parse_document(&dst.mapping, &ctx, &value, Some(&source), Some(&id))?;
            // Time series documents keep their `_tsid` and the routing hash their `_id` starts
            // with (`_ts_routing_hash`).
            let ts = crate::engine::tsdb::is_time_series(dst).then(|| {
                let tsid = crate::engine::tsdb::build_tsid(&crate::engine::tsdb::dimensions(dst, &parsed));
                let hash = crate::engine::tsdb::id_routing_hash(dst, &id).map(crate::engine::tsdb::encode_hash).unwrap_or_default();
                (tsid, hash)
            });
            let meta = DocMeta {
                id: &id,
                routing: if ts.is_some() { None } else { routing.as_deref() },
                source: stored_source(dst, &table, &source, &value),
                tsid: ts.as_ref().map(|(t, h)| (t.as_slice(), h.as_str())),
            };
            let idoc = convert::to_indexable(&table, &parsed, &meta)?;
            let seq_no = crate::search::hits::meta_dv(seg, local, search_index::schema::SEQ_NO_FIELD);
            let primary_term = crate::search::hits::meta_dv(seg, local, search_index::schema::PRIMARY_TERM_FIELD);
            let version = crate::search::hits::meta_dv(seg, local, search_index::schema::VERSION_FIELD);
            let mut op = IndexOp::new(idoc);
            op.origin = search_index::Origin::Replay { seq_no, primary_term, version };
            let target_shard = crate::meta::routing::shard_for(dst, &id, routing.as_deref());
            let ts = de.shard(target_shard);
            if let Some(log) = &ts.log {
                log.check()?;
            }
            if let search_index::OpResult::Indexed { .. } = ts.shard.index(op).map_err(convert::index_error)? {
                log_op(ts, &LoggedOp::Index { id: &id, routing: routing.as_deref(), source: &source, seq_no, primary_term, version })?;
            }
            ts.wrote();
        }
    }
    de.refresh_all()
}
