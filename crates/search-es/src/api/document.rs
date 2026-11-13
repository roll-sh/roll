//! Single-document APIs: index / create, delete, get, exists, get_source, exists_source and
//! mget, with Elasticsearch's parameters, validation, response shapes and errors.

use crate::api::Reply;
use crate::error::{EsError, EsResult};
use crate::fetch::FetchSource;
use crate::json::{Map, Value};
use crate::meta::IndexMeta;
use crate::meta::resolve::{self, IndicesOptions};
use crate::obj;
use crate::rest::Ctx;
use crate::write::{self, Action, RefreshPolicy, WriteArgs, WriteOutcome};
use bytes::Bytes;
use search_index::VersionType;
use std::sync::Arc;

fn opt_long(ctx: &Ctx, name: &str) -> EsResult<Option<i64>> {
    if ctx.has_query(name) { ctx.long_param(name, 0).map(Some) } else { Ok(None) }
}

/// `SliceIndexing.parseRoutingOrSliceWithProvenance`: the `routing` parameter, or `_slice`
/// (slice indexing's name for it, validated), never both.
pub fn routing_or_slice(ctx: &Ctx) -> EsResult<Option<String>> {
    let routing = ctx.query("routing");
    let Some(slice) = ctx.query("_slice") else { return Ok(routing.filter(|r| !r.is_empty()).map(str::to_string)) };
    validate_slice_value(slice)?;
    if routing.is_some() {
        return Err(EsError::illegal_argument("[routing] is not allowed together with [_slice]"));
    }
    Ok(Some(slice.to_string()))
}

/// `SliceIndexing.validateUserSliceValue`.
pub fn validate_slice_value(slice: &str) -> EsResult<()> {
    if slice.is_empty() {
        return Err(EsError::illegal_argument("invalid [_slice] value: value must be non-empty"));
    }
    let len = slice.encode_utf16().count();
    if len > 128 {
        return Err(EsError::illegal_argument(format!("invalid [_slice] value [{slice}]: length [{len}] exceeds max [128]")));
    }
    if slice == "_all" {
        return Err(EsError::illegal_argument(format!("invalid [_slice] value [{slice}]: value is reserved")));
    }
    // `[a-zA-Z0-9](?:[a-zA-Z0-9._:-]*[a-zA-Z0-9])?`
    let b = slice.as_bytes();
    let edge = |c: u8| c.is_ascii_alphanumeric();
    let inner = |c: u8| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b':' | b'-');
    if !(edge(b[0]) && edge(b[b.len() - 1]) && b.iter().all(|&c| inner(c))) {
        return Err(EsError::illegal_argument(format!("invalid [_slice] value [{slice}]: only [a-zA-Z0-9._:-] are allowed and max length is [128]")));
    }
    Ok(())
}

/// Write parameters common to index, create, delete and update.
pub fn write_args(ctx: &Ctx) -> EsResult<WriteArgs> {
    Ok(WriteArgs {
        id: ctx.path_param("id").map(str::to_string),
        routing: routing_or_slice(ctx)?,
        create: false,
        version: opt_long(ctx, "version")?,
        version_type: ctx.query("version_type").map(write::parse_version_type).transpose()?,
        if_seq_no: opt_long(ctx, "if_seq_no")?,
        if_primary_term: opt_long(ctx, "if_primary_term")?,
        pipeline: ctx.query("pipeline").map(str::to_string),
        require_alias: ctx.bool_param("require_alias", false)?,
        require_data_stream: ctx.bool_param("require_data_stream", false)?,
        dynamic_templates: None,
        dynamic_template_params: None,
        internal: false,
        list_executed_pipelines: false,
        principal: Some(std::sync::Arc::new(crate::xpack::security::principal(ctx).clone())),
    })
}

/// `wait_for_active_shards` beyond the primary on a node with no replicas assigned: waits for
/// `timeout`, then fails as Elasticsearch does.
pub async fn check_active_shards(ctx: &Ctx<'_>, idx: &IndexMeta, shard: u32) -> EsResult<()> {
    check_active_shards_for(ctx, idx, shard, None).await
}

/// [`check_active_shards`] for a shard-level request `request` describes (`, request: [..]`
/// ends the message, as replication requests print it).
pub async fn check_active_shards_for(ctx: &Ctx<'_>, idx: &IndexMeta, shard: u32, request: Option<String>) -> EsResult<()> {
    let Some(w) = ctx.query("wait_for_active_shards").or_else(|| idx.settings.get("index.write.wait_for_active_shards")) else { return Ok(()) };
    let needed = match w {
        "all" => 1 + idx.number_of_replicas() as i64,
        n => n.parse::<i64>().unwrap_or(1),
    };
    if needed <= 1 {
        return Ok(());
    }
    let timeout = ctx.time_param("timeout", crate::units::TimeValue::minutes(1))?;
    let ms = timeout.as_millis().clamp(0, 60_000) as u64;
    tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    let shown = if w == "all" { "ALL".to_string() } else { w.to_string() };
    Err(EsError::new(
        503,
        "unavailable_shards_exception",
        format!(
            "[{}][{shard}] Not enough active copies to meet shard count of [{shown}] (have 1, needed {needed}). Timeout: [{}]{}",
            idx.name,
            timeout.string_rep(),
            request.map(|r| format!(", request: [{r}]")).unwrap_or_default()
        ),
    )
    .with_meta("index_uuid", idx.uuid.as_str())
    .with_meta("shard", shard.to_string())
    .with_meta("index", idx.name.as_str()))
}

fn finish_refresh(ctx: &Ctx, out: &mut WriteOutcome) -> EsResult<()> {
    match RefreshPolicy::parse(ctx.query("refresh"))? {
        RefreshPolicy::None => {}
        RefreshPolicy::Immediate => {
            write::refresh_shards(ctx.cluster, &[(out.uuid.clone(), out.shard)])?;
            out.forced_refresh = true;
        }
        RefreshPolicy::WaitFor => write::refresh_shards(ctx.cluster, &[(out.uuid.clone(), out.shard)])?,
    }
    Ok(())
}

/// `PUT/POST /{index}/_doc/{id}`, `POST /{index}/_doc`, `PUT/POST /{index}/_create/{id}`.
pub async fn index(ctx: &Ctx<'_>, create_api: bool) -> EsResult<Reply> {
    // Enabled streams restrict the parameters of writes to them (`RestIndexAction`); the
    // auto-id and create handlers add `op_type` first.
    crate::xpack::streams::check_index_request(ctx, ctx.path_param("index").unwrap_or_default(), create_api || ctx.path_param("id").is_none())?;
    let mut args = write_args(ctx)?;
    RefreshPolicy::parse(ctx.query("refresh"))?;
    match ctx.query("op_type") {
        None => {}
        Some("create") => args.create = true,
        Some("index") => {}
        Some(o) => return Err(EsError::illegal_argument(format!("opType must be 'create' or 'index', found: [{o}]"))),
    }
    if create_api {
        args.create = true;
    }
    if ctx.body.is_empty() {
        return Err(EsError::parse("request body is required"));
    }
    let expr = ctx.path_param("index").unwrap_or_default().to_string();
    let auto_id = args.id.is_none();
    // `POST /{index}/_doc` creates unless the request names its `op_type` (time series indices
    // generate ids that can repeat: `op_type=index` overwrites).
    if auto_id && ctx.query("op_type").is_none() {
        args.create = true;
    }
    if let Some(id) = &args.id
        && id.is_empty()
    {
        return Err(EsError::action_request_validation(&["if _id is specified it must not be empty".into()]));
    }
    let errs = write::validate_args(&args, if args.create { Action::Create } else { Action::Index });
    if !errs.is_empty() {
        return Err(EsError::action_request_validation(&errs));
    }
    if !args.create
        && let Some(e) = crate::opensearch::append_only_violation(&ctx.state(), &expr, "INDEX", args.id.as_deref())
    {
        return Err(e);
    }
    if let Some(e) = crate::xpack::streams::check_item(ctx, &expr, args.pipeline.as_deref()) {
        return Err(e);
    }
    let body = ctx.body.clone();
    let out = index_one(ctx, &expr, args, body, auto_id).await?;
    let mut out = out;
    finish_refresh(ctx, &mut out)?;
    Ok(Reply::Body(out.status(), out.to_value()))
}

/// Indexes one document through the full write path (target, pipelines, routing, mapping);
/// a document failure in a data stream with a failure store goes to its failure store.
pub async fn index_one(ctx: &Ctx<'_>, expr: &str, args: WriteArgs, source: Bytes, auto_id: bool) -> EsResult<WriteOutcome> {
    match index_one_ahead(ctx, expr, args, source, auto_id, None).await? {
        Indexed::Done(o) => Ok(o),
        Indexed::Ready(w) => w.apply(),
    }
}

/// A document prepared ahead of its turn (`_bulk`'s parallel preparation): given the index the
/// write resolved to, the prepared document if it is still valid for it.
pub type PreparedAhead<'a> = &'a mut (dyn FnMut(&std::sync::Arc<crate::meta::IndexMeta>) -> Option<write::Prepared> + Send);

/// `_bulk`'s side of an index item: the document prepared ahead, and whether to hand the write
/// back once every check before its apply passed.
pub struct Ahead<'a> {
    pub take: PreparedAhead<'a>,
    /// With `Some`, a write that uses the prepared document comes back as [`Indexed::Ready`]
    /// (`_bulk` applies the writes of different shards in parallel, as Elasticsearch runs a
    /// request's shard-level bulks). The callback applies every write handed back so far; it
    /// runs before this item writes any other way, so each shard sees its items in order.
    pub defer: Option<&'a mut (dyn FnMut() + Send)>,
}

/// An index write done, or (with [`Ahead::defer`]) ready to apply.
pub enum Indexed {
    Done(WriteOutcome),
    Ready(Box<ReadyWrite>),
}

/// A write whose checks passed: what [`write::apply_index`] takes.
pub struct ReadyWrite {
    pub prepared: write::Prepared,
    pub args: WriteArgs,
    pub auto_id: bool,
    pub id: String,
}

impl ReadyWrite {
    pub fn apply(self) -> EsResult<WriteOutcome> {
        write::apply_index(self.prepared, &self.args, self.auto_id, &self.id)
    }
}

/// [`index_one`], using a document prepared ahead when the write goes straight to an index.
pub async fn index_one_ahead(ctx: &Ctx<'_>, expr: &str, args: WriteArgs, source: Bytes, auto_id: bool, ahead: Option<Ahead<'_>>) -> EsResult<Indexed> {
    let cluster = ctx.cluster;
    let existed = ctx.state().metadata.data_streams.contains_key(expr);
    let target = write::resolve_target(cluster, expr, &args, ctx.now_ms).await?;
    // Backing indices only take writes that carry their sequence numbers (`IndexRequest`
    // targeting a backing index directly).
    let direct = target.index.name == expr;
    if direct
        && let Some(ds) = ctx.state().metadata.data_stream_of(&target.index.name)
        && ds.indices.contains(&target.index.name)
    {
        if args.create {
            return Err(EsError::illegal_argument(format!(
                "index request with op_type=create targeting backing indices is disallowed, target corresponding data stream [{}] instead",
                ds.name
            )));
        }
        if args.if_seq_no.is_none() || args.if_primary_term.is_none() {
            return Err(EsError::illegal_argument(format!(
                "index request with op_type=index and no if_primary_term and if_seq_no set targeting backing indices is disallowed, target corresponding data stream [{}] instead",
                ds.name
            )));
        }
    }
    let initial_ds = target.data_stream.clone().filter(|_| !direct);
    let mut ahead = ahead;
    if initial_ds.is_none()
        && let Some(a) = &mut ahead
        && let Some(r) = index_prepared(ctx, &target, &args, auto_id, a).await
    {
        return r;
    }
    // Any other write applies here: the writes handed back go first.
    if let Some(barrier) = ahead.and_then(|a| a.defer) {
        barrier();
    }
    let Some(initial_ds) = initial_ds else {
        return index_resolved(ctx, target, args, source, auto_id).await.map(Indexed::Done);
    };
    let original = parse_doc(&source).ok();
    let mut args = args;
    // The failure document names the id a write was given or generated (ingest runs before
    // ids are generated, so its failures name only a given one).
    if args.id.is_none() {
        args.id = Some(crate::meta::names::time_based_id());
    }
    let given_id = args.id.clone();
    let routing = args.routing.clone();
    let r = index_resolved(ctx, target, args, source, auto_id).await;
    let e = match r {
        Ok(o) => return Ok(Indexed::Done(o)),
        Err(e) => e,
    };
    let (ingest, created) = match crate::ingest::pipeline::failure_of(&e) {
        Some((f, c)) => (Some(f), c),
        None => (None, Vec::new()),
    };
    // An ingest failure goes to the failure store of the target its pipeline started from.
    let store_of = ingest.as_ref().and_then(|f| f.index.clone()).unwrap_or_else(|| initial_ds.clone());
    match crate::failure_store::target(cluster, &store_of) {
        crate::failure_store::Store::Enabled(ds) if crate::failure_store::eligible(&e) => {
            // A data stream this request created never received the document: the failure
            // happened in ingest, before Elasticsearch would have created it.
            if !existed && ingest.is_some() && ds != initial_ds {
                let n = initial_ds.clone();
                let _ = cluster.update(move |st| crate::api::data_streams::delete_data_streams(st, std::slice::from_ref(&n))).await;
            }
            let id = if ingest.is_some() && auto_id { None } else { given_id };
            let src = e.hidden("ingested_source").cloned().or(original).unwrap_or(Value::Obj(Map::new()));
            let fresh = (!existed && ingest.is_some() && ds == initial_ds) || created.contains(&ds);
            crate::failure_store::redirect(cluster, &ds, id.as_deref(), routing.as_deref(), &src, &e, ingest.as_ref(), Some(&initial_ds), fresh, ctx.now_ms).await.map(Indexed::Done)
        }
        crate::failure_store::Store::Enabled(_) => Err(e),
        // A data stream without a failure store says so on document failures.
        crate::failure_store::Store::Disabled | crate::failure_store::Store::None => {
            let disabled = crate::failure_store::target(cluster, &store_of) == crate::failure_store::Store::Disabled
                || crate::failure_store::target(cluster, &initial_ds) == crate::failure_store::Store::Disabled;
            // Ingest runs before missing data streams are created: a document failing there
            // leaves none behind.
            if !existed && ingest.is_some() {
                let n = initial_ds.clone();
                let _ = cluster.update(move |st| crate::api::data_streams::delete_data_streams(st, std::slice::from_ref(&n))).await;
            }
            Err(if disabled && crate::failure_store::eligible(&e) { e.with_trailing("failure_store", "not_enabled") } else { e })
        }
    }
}

async fn index_resolved(ctx: &Ctx<'_>, target: write::Target, mut args: WriteArgs, source: Bytes, mut auto_id: bool) -> EsResult<WriteOutcome> {
    let cluster = ctx.cluster;
    let mut id = args.id.clone().unwrap_or_else(crate::meta::names::time_based_id);
    let mut executed = Vec::new();
    let value = parse_doc(&source)?;
    // Ingest runs before auto ids exist.
    let ingest_args = if auto_id { std::borrow::Cow::Owned(WriteArgs { id: None, ..args.clone() }) } else { std::borrow::Cow::Borrowed(&args) };
    let requested = target.index.name.clone();
    let (target, value, source) = match crate::ingest::run_for_write(cluster, target, &ingest_args, &id, value, source, ctx.now_ms, Some(&ctx.ingest_nanos)).await? {
        crate::ingest::Outcome::Index(t, v, s, ingested) => {
            // A pipeline that moved the document (`_index`, `reroute`): the write is authorized
            // again where it lands.
            if t.index.name != requested {
                crate::xpack::security::check_moved_write(ctx, &t.index.name)?;
            }
            if let Some(ingested) = ingested {
                let crate::ingest::Ingested { args: changed, executed: ran } = *ingested;
                let new_id = changed.id.clone().unwrap_or_else(|| id.clone());
                if new_id != id {
                    id = new_id;
                    auto_id = false;
                }
                args = WriteArgs { id: if auto_id { args.id } else { Some(id.clone()) }, ..changed };
                executed = ran;
            }
            (t, v, s)
        }
        crate::ingest::Outcome::Dropped(idx) => {
            return Ok(WriteOutcome {
                index: idx,
                uuid: String::new(),
                id,
                shard: 0,
                version: -3,
                result: "noop",
                seq_no: -2,
                primary_term: 0,
                total: 0,
                forced_refresh: false,
                failure_store: None,
                executed_pipelines: None,
            });
        }
    };
    let target = crate::tsds::route(cluster, target, args.create, &source, &value, ctx.now_ms).await?;
    if crate::engine::tsdb::is_time_series(&target.index) {
        // The id comes from the dimensions and the timestamp; the routing from the routing path.
        if args.routing.is_some() || target.alias_routing.is_some() {
            return Err(crate::engine::tsdb::unsupported(&target.index, "specifying routing"));
        }
        let hash = crate::engine::tsdb::routing_hash(&target.index, &value)?;
        let routing = crate::engine::tsdb::encode_hash(hash);
        let shard = crate::meta::routing::shard_for(&target.index, "", Some(&routing));
        check_active_shards(ctx, &target.index, shard).await?;
        let given = if auto_id { String::new() } else { args.id.clone().unwrap_or_default() };
        let check = |idx: &IndexMeta| crate::xpack::security::check_mapping_update(ctx, idx);
        let prepared = write::prepare_checked(cluster, target.index.clone(), &given, Some(&routing), &source, &value, Some(&args), Some(&check)).await?;
        let id = prepared.id.clone();
        return write::apply_index(prepared, &args, false, &id).map(|o| with_executed(o, &args, executed));
    }
    let routing = write::effective_routing(&target, args.routing.as_deref(), &id)?;
    write::check_routing(&target.index, &id, routing.as_deref())?;
    let shard = crate::meta::routing::shard_for(&target.index, &id, routing.as_deref());
    check_active_shards(ctx, &target.index, shard).await?;
    // A document that fails to index after a pipeline rewrote it goes to the failure store as
    // rewritten (`IndexRequest.source()` is the ingested document by then).
    let ingested = |e: EsError| if executed.is_empty() { e } else { e.with_hidden("ingested_source", value.clone()) };
    let check = |idx: &IndexMeta| crate::xpack::security::check_mapping_update(ctx, idx);
    let prepared = write::prepare_checked(cluster, target.index.clone(), &id, routing.as_deref(), &source, &value, Some(&args), Some(&check)).await.map_err(ingested)?;
    write::apply_index(prepared, &args, auto_id, &id).map(|o| with_executed(o, &args, executed))
}

/// Lists the pipelines a write ran when the bulk item asked (`list_executed_pipelines`).
fn with_executed(mut o: WriteOutcome, args: &WriteArgs, executed: Vec<String>) -> WriteOutcome {
    if args.list_executed_pipelines {
        o.executed_pipelines = Some(executed);
    }
    o
}

/// The tail of [`index_resolved`] for a document prepared ahead: `None` (nothing done) unless
/// the write goes straight to a plain index, runs no pipeline and the prepared document is
/// still valid for that index and routing; then routing checks and the apply, as there.
async fn index_prepared(ctx: &Ctx<'_>, target: &write::Target, args: &WriteArgs, auto_id: bool, ahead: &mut Ahead<'_>) -> Option<EsResult<Indexed>> {
    let idx = &target.index;
    if target.data_stream.is_some() || target.alias_routing.is_some() || crate::engine::tsdb::is_time_series(idx) {
        return None;
    }
    let none = |k: &str| idx.settings.get(k).is_none_or(|p| p == "_none");
    let default_skipped = args.pipeline.as_deref() == Some("_none");
    if args.pipeline.as_deref().is_some_and(|p| p != "_none") || !none("index.final_pipeline") || !(default_skipped || none("index.default_pipeline")) || args.list_executed_pipelines {
        return None;
    }
    let id = args.id.as_deref()?;
    let p = (ahead.take)(idx)?;
    if p.id != id || p.routing.as_deref() != args.routing.as_deref() {
        return None;
    }
    let defer = ahead.defer.is_some();
    let r = async {
        let routing = write::effective_routing(target, args.routing.as_deref(), id)?;
        write::check_routing(idx, id, routing.as_deref())?;
        let shard = crate::meta::routing::shard_for(idx, id, routing.as_deref());
        check_active_shards(ctx, idx, shard).await?;
        let w = ReadyWrite { prepared: p, args: args.clone(), auto_id, id: id.to_string() };
        if defer { Ok(Indexed::Ready(Box::new(w))) } else { w.apply().map(Indexed::Done) }
    };
    Some(r.await)
}

/// The document of an index request: an object (`Malformed content, must start with an
/// object` otherwise).
pub fn parse_doc(source: &[u8]) -> EsResult<Value> {
    let v = match crate::json::parse(source).map_err(EsError::from) {
        Ok(v) => v,
        Err(e) if e.kind.starts_with("x_content_") => {
            // Jackson reports where the current token started; the document parser wraps the
            // syntax error with that location.
            // `XContentParseException` is no `ElasticsearchException`: the wrapper is the root.
            let wrap = |reason: String, e: EsError| {
                let d = EsError::document_parsing(reason);
                let root = d.clone();
                d.caused_by(e).with_root_causes(vec![root])
            };
            return Err(match token_location(source) {
                Some(Loc { line, col, trailing: true }) => wrap(format!("[{line}:{col}] {}", e.reason()), e),
                Some(Loc { line, col, trailing: false }) => wrap(format!("[{line}:{col}] failed to parse: {}", e.reason()), e),
                None => e,
            });
        }
        Err(e) => return Err(e),
    };
    match v {
        Some(v @ Value::Obj(_)) => Ok(v),
        _ => Err(EsError::document_parsing("Malformed content, must start with an object")),
    }
}

/// `DELETE /{index}/_doc/{id}`.
pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let args = write_args(ctx)?;
    RefreshPolicy::parse(ctx.query("refresh"))?;
    let errs = write::validate_args(&args, Action::Delete);
    if !errs.is_empty() {
        return Err(EsError::action_request_validation(&errs));
    }
    let expr = ctx.path_param("index").unwrap_or_default().to_string();
    let id = args.id.clone().unwrap_or_default();
    let st = ctx.state();
    if let Some(e) = crate::opensearch::append_only_violation(&st, &expr, "DELETE", Some(&id)) {
        return Err(e);
    }
    let target = match write_target_existing(ctx, &st, &expr, &args).await? {
        Some(t) => t,
        None => return Err(EsError::index_not_found(&expr)),
    };
    if args.routing.is_some() && crate::engine::tsdb::is_time_series(&target.index) {
        return Err(crate::engine::tsdb::unsupported(&target.index, "specifying routing"));
    }
    let routing = write::effective_routing(&target, args.routing.as_deref(), &id)?;
    write::check_routing(&target.index, &id, routing.as_deref())?;
    let shard = crate::meta::routing::shard_for(&target.index, &id, routing.as_deref());
    check_active_shards(ctx, &target.index, shard).await?;
    let mut out = write::apply_delete(ctx.cluster, &target.index, &id, routing.as_deref(), &args)?;
    finish_refresh(ctx, &mut out)?;
    Ok(Reply::Body(out.status(), out.to_value()))
}

/// The write index of `expr` for deletes (which auto-create only with external versioning).
async fn write_target_existing(ctx: &Ctx<'_>, _st: &crate::meta::ClusterState, expr: &str, args: &WriteArgs) -> EsResult<Option<write::Target>> {
    write::resolve_delete_target(ctx.cluster, expr, args, ctx.now_ms).await.map(Some)
}

/// Options of a get.
#[derive(Clone, Debug, Default)]
pub struct GetArgs {
    pub routing: Option<String>,
    pub realtime: bool,
    pub refresh: bool,
    pub stored_fields: Option<Vec<String>>,
    pub source: Option<FetchSource>,
    pub version: Option<i64>,
    pub version_type: Option<VersionType>,
    pub force_synthetic: bool,
}

/// The concrete index of a single-index read (`concreteSingleIndex`). OpenSearch's resolver
/// reports a missing name as an `index_expression` resource.
pub fn single_index(ctx: &Ctx, expr: &str) -> EsResult<Arc<IndexMeta>> {
    let r = single_index_in(ctx, expr);
    if ctx.cluster.dialect == crate::cluster::Dialect::OpenSearch {
        return r.map_err(|e| if e.kind == "index_not_found_exception" { e.set_meta("resource.type", "index_expression") } else { e });
    }
    r
}

fn single_index_in(ctx: &Ctx, expr: &str) -> EsResult<Arc<IndexMeta>> {
    let st = ctx.state();
    let opts = IndicesOptions { allow_no_indices: false, ..IndicesOptions::STRICT_EXPAND_OPEN };
    let names = resolve::resolve(&st, &[expr.to_string()], &opts, ctx.now_ms)?;
    match names.len() {
        0 => Err(EsError::index_not_found(expr)),
        1 => {
            let idx = st.metadata.indices.get(&names[0]).cloned().ok_or_else(|| EsError::index_not_found(expr))?;
            if idx.is_closed() {
                return Err(EsError::closed_index(&idx.name, &idx.uuid));
            }
            Ok(idx)
        }
        _ => {
            if let Some(ds) = st.metadata.data_streams.get(expr)
                && let Some(w) = ds.write_index().and_then(|w| st.metadata.indices.get(w)) {
                    return Ok(w.clone());
                }
            let mut n = names.clone();
            n.sort();
            Err(EsError::illegal_argument(format!(
                "alias [{expr}] has more than one index associated with it [{}], can't execute a single index op",
                n.join(", ")
            )))
        }
    }
}

/// A found document.
pub struct GotDoc {
    pub index: Arc<IndexMeta>,
    pub doc: search_index::StoredDoc,
}

/// Reads one document (realtime by default).
pub fn get_one(ctx: &Ctx, idx: &Arc<IndexMeta>, id: &str, args: &GetArgs) -> EsResult<Option<GotDoc>> {
    write::check_routing(idx, id, args.routing.as_deref())?;
    let st = ctx.state();
    let engine = ctx.cluster.engine.index(ctx.cluster, &st, idx)?;
    if args.routing.is_some() && crate::engine::tsdb::is_time_series(idx) {
        return Err(crate::engine::tsdb::unsupported(idx, "specifying routing"));
    }
    if crate::engine::tsdb::is_time_series(idx) && crate::engine::tsdb::id_routing_hash(idx, id).is_none() {
        return Err(crate::engine::tsdb::invalid_id(idx, id));
    }
    let routing = args.routing.clone();
    let shard_id = crate::meta::routing::shard_for(idx, id, routing.as_deref());
    let shard = engine.shard(shard_id);
    if args.refresh {
        shard.refresh()?;
    }
    let t = std::time::Instant::now();
    if args.realtime {
        shard.shard.note_realtime_get(id.as_bytes()).map_err(crate::engine::convert::index_error)?;
    }
    let doc = shard.shard.get(id.as_bytes(), args.realtime).map_err(crate::engine::convert::index_error)?;
    let c = &shard.counters;
    c.get_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    c.get_time_nanos.fetch_add(t.elapsed().as_nanos() as u64, std::sync::atomic::Ordering::Relaxed);
    match &doc {
        Some(_) => c.get_exists.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        None => c.get_missing.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    };
    let Some(doc) = doc else { return Ok(None) };
    // Document level security: a document the reader may not see is not found.
    if let Some(sec) = crate::xpack::security::dls::ReadSecurity::of(ctx)
        && let Some(q) = sec.documents(ctx.cluster, crate::xpack::security::dls::READ, &idx.name)?
        && !crate::search::matches_doc(ctx.cluster, &st, idx, &engine, shard, id, &q)?
    {
        return Ok(None);
    }
    if let Some(v) = args.version {
        // `VersionType.isVersionConflictForReads`: every version type wants the exact version.
        let _ = args.version_type;
        if doc.version != v {
            let why = format!("current version [{}] is different than the one provided [{v}]", doc.version);
            return Err(write::shard_error(EsError::new(409, "version_conflict_engine_exception", format!("[{id}]: version conflict, {why}")), idx, shard_id));
        }
    }
    Ok(Some(GotDoc { index: idx.clone(), doc }))
}

/// The get response body (`GetResult.toXContent`).
pub fn render_get(ctx: &Ctx, index: &str, id: &str, got: Option<&GotDoc>, args: &GetArgs) -> EsResult<Value> {
    let mut m = Map::new();
    m.insert("_index".into(), Value::from(index));
    m.insert("_id".into(), Value::from(id));
    let Some(g) = got else {
        m.insert("found".into(), Value::Bool(false));
        return Ok(Value::Obj(m));
    };
    let d = &g.doc;
    m.insert("_version".into(), Value::from(d.version));
    // Indices without sequence numbers leave them out.
    if !write::seq_no_disabled(&g.index) {
        m.insert("_seq_no".into(), Value::from(d.seq_no));
        m.insert("_primary_term".into(), Value::from(d.primary_term));
    }
    if let Some(r) = crate::fetch::routing(&d.fields).filter(|_| crate::fetch::routing_shown(&g.index)) {
        m.insert("_routing".into(), Value::Str(r));
    }
    // `_ignored` lives in doc values: only `stored_fields` asking for it loads it.
    let ignored = if args.stored_fields.as_ref().is_some_and(|sf| sf.iter().any(|f| f == "_ignored")) { crate::fetch::ignored(&d.fields) } else { Vec::new() };
    if !ignored.is_empty() {
        m.insert("_ignored".into(), Value::Arr(ignored.into_iter().map(Value::Str).collect()));
    }
    m.insert("found".into(), Value::Bool(true));
    // `stored_fields` given without `_source` turns the source off.
    let fs = match (&args.source, &args.stored_fields) {
        (Some(fs), _) => fs.clone(),
        (None, Some(sf)) => FetchSource { fetch: sf.iter().any(|f| f == "_source"), ..Default::default() },
        (None, None) => FetchSource::default(),
    };
    let st = ctx.state();
    let engine = ctx.cluster.engine.index(ctx.cluster, &st, &g.index)?;
    let table = engine.table();
    let view = crate::fetch::SourceView { idx: &g.index, table: &table, force_synthetic: args.force_synthetic, search: false };
    let fls = crate::xpack::security::dls::ReadSecurity::of(ctx).and_then(|s| s.fields(crate::xpack::security::dls::READ, &g.index.name));
    if let Some(src) = crate::fetch::render_source_view(&d.fields, &fs, &view)? {
        m.insert("_source".into(), match &fls {
            Some(f) => f.filter_source(src),
            None => src,
        });
    }
    if let Some(sf) = &args.stored_fields {
        let mut fields = crate::fetch::stored_fields(&table, &d.fields, sf);
        if let Some(f) = &fls {
            fields = f.filter_fields(fields);
        }
        if !fields.is_empty() {
            m.insert("fields".into(), Value::Obj(fields));
        }
    }
    Ok(Value::Obj(m))
}

fn get_args(ctx: &Ctx) -> EsResult<GetArgs> {
    Ok(GetArgs {
        routing: routing_or_slice(ctx)?,
        realtime: ctx.bool_param("realtime", true)?,
        refresh: ctx.bool_param("refresh", false)?,
        stored_fields: ctx.query("stored_fields").map(|s| s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(str::to_string).collect()),
        source: FetchSource::from_params(ctx.query("_source"), ctx.query("_source_includes"), ctx.query("_source_excludes"))?,
        version: opt_long(ctx, "version")?,
        version_type: ctx.query("version_type").map(write::parse_version_type).transpose()?,
        force_synthetic: ctx.bool_param("force_synthetic_source", false)?,
    })
    .and_then(|mut a: GetArgs| {
        if let Some(ev) = ctx.opt_bool_param("_source_exclude_vectors")? {
            a.source.get_or_insert_with(FetchSource::default).exclude_vectors = Some(ev);
        }
        Ok(a)
    })
}

/// `GET /{index}/_doc/{id}` (and `HEAD` for exists).
pub fn get(ctx: &Ctx, head: bool) -> EsResult<Reply> {
    let expr = ctx.path_param("index").unwrap_or_default();
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let args = get_args(ctx)?;
    let idx = single_index(ctx, expr)?;
    crate::api::warn_system_access(ctx, std::slice::from_ref(&idx.name));
    if let Some(source) = crate::xpack::security::users::security_document(&ctx.state(), &idx.name, &id) {
        if head {
            return Ok(Reply::Empty(200));
        }
        let mut m = Map::new();
        for (k, v) in [("_index", Value::from(idx.name.clone())), ("_id", Value::from(id)), ("_version", Value::from(1)), ("_seq_no", Value::from(0)), ("_primary_term", Value::from(1)), ("found", Value::Bool(true))] {
            m.insert(k.into(), v);
        }
        if args.source.as_ref().is_none_or(|fs| fs.fetch) {
            m.insert("_source".into(), source);
        }
        return Ok(Reply::Body(200, Value::Obj(m)));
    }
    let got = get_one(ctx, &idx, &id, &args)?;
    if head {
        return Ok(Reply::Empty(if got.is_some() { 200 } else { 404 }));
    }
    let status = if got.is_some() { 200 } else { 404 };
    let body = render_get(ctx, &idx.name, &id, got.as_ref(), &args)?;
    Ok(Reply::Body(status, body))
}

/// `GET /{index}/_source/{id}` (and `HEAD`).
pub fn get_source(ctx: &Ctx, head: bool) -> EsResult<Reply> {
    let expr = ctx.path_param("index").unwrap_or_default();
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let mut args = get_args(ctx)?;
    if ctx.has_query("stored_fields") {
        return Err(EsError::action_request_validation(&[
            "RestGetSourceAction does not support stored_fields: use the get API with stored_fields instead".into(),
        ]));
    }
    if args.source.as_ref().is_some_and(|fs| !fs.fetch) {
        return Err(EsError::action_request_validation(&["fetching source can not be disabled".into()]));
    }
    args.stored_fields = None;
    let idx = single_index(ctx, expr)?;
    let got = get_one(ctx, &idx, &id, &args)?;
    let not_found = || EsError::resource_not_found(format!("Document not found [{}]/[{id}]", idx.name));
    let Some(g) = got else {
        if head {
            return Ok(Reply::Empty(404));
        }
        return Err(not_found());
    };
    let fs = args.source.clone().unwrap_or_default();
    let st = ctx.state();
    let engine = ctx.cluster.engine.index(ctx.cluster, &st, &g.index)?;
    let table = engine.table();
    let view = crate::fetch::SourceView { idx: &g.index, table: &table, force_synthetic: args.force_synthetic, search: false };
    let fls = crate::xpack::security::dls::ReadSecurity::of(ctx).and_then(|s| s.fields(crate::xpack::security::dls::READ, &g.index.name));
    match crate::fetch::render_source_view(&g.doc.fields, &FetchSource { fetch: true, ..fs }, &view)?.map(|v| match &fls {
        Some(f) => f.filter_source(v),
        None => v,
    }) {
        Some(v) => {
            if head {
                return Ok(Reply::Empty(200));
            }
            Ok(Reply::Body(200, v))
        }
        None => {
            if head {
                return Ok(Reply::Empty(404));
            }
            Err(EsError::resource_not_found(format!("Source not available [{}]/[{id}]", idx.name)))
        }
    }
}

/// `GET /_mget`, `GET /{index}/_mget`.
pub fn mget(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body_map()?.ok_or_else(|| EsError::parse("request body is required"))?;
    let default_index = ctx.path_param("index").map(str::to_string);
    let defaults = get_args(ctx)?;
    let mut items: Vec<(Option<String>, Option<String>, GetArgs)> = Vec::new();
    for (k, v) in &body {
        match (k.as_str(), v) {
            ("docs", Value::Arr(docs)) => {
                for d in docs {
                    let Value::Obj(dm) = d else {
                        return Err(EsError::illegal_argument("docs array element should include an object"));
                    };
                    let mut a = defaults.clone();
                    let mut index = default_index.clone();
                    let mut id = None;
                    for (dk, dv) in dm {
                        match dk.as_str() {
                            "_index" => index = dv.as_str().map(str::to_string),
                            "_id" => id = Some(dv.to_es_string()),
                            "routing" => a.routing = Some(dv.to_es_string()),
                            "stored_fields" => {
                                a.stored_fields = Some(match dv {
                                    Value::Arr(x) => x.iter().map(|y| y.to_es_string()).collect(),
                                    other => vec![other.to_es_string()],
                                })
                            }
                            "_source" => a.source = Some(FetchSource::from_value(dv)?),
                            "version" => a.version = dv.as_i64(),
                            "version_type" => a.version_type = dv.as_str().map(write::parse_version_type).transpose()?,
                            other => return Err(EsError::parse(format!("failed to parse multi get request. unknown field [{other}]"))),
                        }
                    }
                    items.push((index, id, a));
                }
            }
            ("ids", Value::Arr(ids)) => {
                for i in ids {
                    items.push((default_index.clone(), Some(i.to_es_string()), defaults.clone()));
                }
            }
            ("docs" | "ids", _) => return Err(EsError::parse(format!("{k} array element should include an object"))),
            (other, _) => return Err(EsError::parse(format!("unknown key [{other}] for a {} in [{other}].", token_name(v)))),
        }
    }
    let mut errs = Vec::new();
    for (i, (index, id, _)) in items.iter().enumerate() {
        if index.is_none() {
            errs.push(format!("index is missing for doc {i}"));
        }
        if id.is_none() {
            errs.push(format!("id is missing for doc {i}"));
        }
    }
    if items.is_empty() {
        errs.push("no documents to get".into());
    }
    if !errs.is_empty() {
        return Err(EsError::action_request_validation(&errs));
    }
    // Documents are read per shard, each shard request authorized on the names its documents
    // were asked by; one that doesn't resolve fails on its own first.
    let denied = if crate::xpack::security::checks_items(ctx) {
        let resolved: Vec<Option<(String, (String, u32))>> = items
            .iter()
            .map(|(index, id, a)| {
                let index = index.as_deref().unwrap_or_default();
                let idx = single_index(ctx, index).ok()?;
                Some((index.to_string(), (idx.name.clone(), crate::meta::routing::shard_for(&idx, id.as_deref().unwrap_or_default(), a.routing.as_deref()))))
            })
            .collect();
        let docs: Vec<(String, (String, u32))> = resolved.iter().flatten().cloned().collect();
        let mut checked = crate::xpack::security::authorize_gets(ctx, &docs).into_iter();
        resolved.iter().map(|r| r.as_ref().and_then(|_| checked.next().flatten())).collect()
    } else {
        Vec::new()
    };
    let mut docs = Vec::with_capacity(items.len());
    for (n, (index, id, a)) in items.iter().enumerate() {
        let index = index.as_deref().unwrap_or_default();
        let id = id.as_deref().unwrap_or_default().to_string();
        let id = &id;
        // Failures after the index resolved name the concrete index.
        let mut shown = index.to_string();
        let r = single_index(ctx, index).and_then(|idx| {
            shown = idx.name.clone();
            if let Some(e) = denied.get(n).cloned().flatten() {
                return Err(e);
            }
            let got = get_one(ctx, &idx, id, a)?;
            render_get(ctx, &idx.name, id, got.as_ref(), a)
        });
        match r {
            Ok(v) => docs.push(v),
            Err(e) => docs.push(obj! { "_index" => shown, "_id" => id.as_str(), "error" => crate::opensearch::item_error(ctx, &e, true) }),
        }
    }
    Reply::ok(obj! { "docs" => Value::Arr(docs) })
}

fn token_name(v: &Value) -> &'static str {
    match v {
        Value::Arr(_) => "START_ARRAY",
        Value::Obj(_) => "START_OBJECT",
        _ => "VALUE",
    }
}

/// Location of Jackson's current token when parsing `src` fails (byte columns, 1-based), and
/// whether the failure is content after the root object.
struct Loc {
    line: usize,
    col: usize,
    trailing: bool,
}

/// A small JSON scanner that mirrors where Jackson's `getTokenLocation()` points when a syntax
/// error is thrown: field names become the current token before their value is read (so
/// errors in scalar values point at the name), strings are read lazily (errors point at the
/// string), and array elements update the location before they are read.
fn token_location(src: &[u8]) -> Option<Loc> {
    struct Sc<'a> {
        b: &'a [u8],
        pos: usize,
        line: usize,
        line_start: usize,
        tok: (usize, usize),
    }
    impl Sc<'_> {
        fn here(&self) -> (usize, usize) {
            (self.line, self.pos - self.line_start + 1)
        }
        fn ws(&mut self) {
            while let Some(&c) = self.b.get(self.pos) {
                match c {
                    b'\n' => {
                        self.pos += 1;
                        self.line += 1;
                        self.line_start = self.pos;
                    }
                    b' ' | b'\t' | b'\r' => self.pos += 1,
                    _ => break,
                }
            }
        }
        fn peek(&self) -> Option<u8> {
            self.b.get(self.pos).copied()
        }
        fn string(&mut self) -> Option<()> {
            self.pos += 1;
            loop {
                match self.peek()? {
                    b'"' => {
                        self.pos += 1;
                        return Some(());
                    }
                    b'\\' => {
                        let e = *self.b.get(self.pos + 1)?;
                        if e == b'u' {
                            let hex = self.b.get(self.pos + 2..self.pos + 6)?;
                            if !hex.iter().all(u8::is_ascii_hexdigit) {
                                return None;
                            }
                            self.pos += 6;
                        } else if b"\"\\/bfnrt".contains(&e) {
                            self.pos += 2;
                        } else {
                            return None;
                        }
                    }
                    c if c < 0x20 => return None,
                    _ => self.pos += 1,
                }
            }
        }
        fn scalar(&mut self) -> Option<()> {
            let start = self.pos;
            while let Some(c) = self.peek() {
                if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'+' | b'.') {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            let t = &self.b[start..self.pos];
            let ok = matches!(t, b"true" | b"false" | b"null") || std::str::from_utf8(t).ok().is_some_and(|s| !s.is_empty() && s.parse::<f64>().is_ok() && !s.starts_with('+'));
            ok.then_some(())
        }
        /// A value whose first byte is at `pos` (location already set by the caller).
        fn value(&mut self, depth: usize) -> Option<()> {
            if depth > 2000 {
                return None;
            }
            match self.peek()? {
                b'{' => {
                    self.pos += 1;
                    let mut first = true;
                    // Jackson's strict duplicate detection fails on a repeated name, while the
                    // previous token is still the current one.
                    let mut names: Vec<&[u8]> = Vec::new();
                    loop {
                        self.ws();
                        let c = self.peek()?;
                        if c == b'}' && first {
                            self.tok = self.here();
                            self.pos += 1;
                            return Some(());
                        }
                        if !first {
                            if c == b'}' {
                                self.tok = self.here();
                                self.pos += 1;
                                return Some(());
                            }
                            if c != b',' {
                                return None;
                            }
                            self.pos += 1;
                            self.ws();
                        }
                        first = false;
                        if self.peek()? != b'"' {
                            return None;
                        }
                        let name = self.here();
                        let start = self.pos;
                        self.string()?;
                        let key = &self.b[start..self.pos];
                        if names.contains(&key) {
                            return None;
                        }
                        names.push(key);
                        self.tok = name;
                        self.ws();
                        if self.peek()? != b':' {
                            return None;
                        }
                        self.pos += 1;
                        self.ws();
                        match self.peek()? {
                            b'"' | b'{' | b'[' => {
                                self.tok = self.here();
                                self.value(depth + 1)?;
                            }
                            _ => {
                                let at = self.here();
                                self.scalar()?;
                                self.tok = at;
                            }
                        }
                    }
                }
                b'[' => {
                    self.pos += 1;
                    let mut first = true;
                    loop {
                        self.ws();
                        let c = self.peek()?;
                        if c == b']' {
                            self.tok = self.here();
                            self.pos += 1;
                            return Some(());
                        }
                        if !first {
                            if c != b',' {
                                return None;
                            }
                            self.pos += 1;
                            self.ws();
                            self.peek()?;
                        }
                        first = false;
                        self.tok = self.here();
                        self.value(depth + 1)?;
                    }
                }
                b'"' => self.string(),
                _ => self.scalar(),
            }
        }
    }
    let mut sc = Sc { b: src, pos: 0, line: 1, line_start: 0, tok: (1, 1) };
    sc.ws();
    sc.tok = sc.here();
    if sc.value(0).is_none() {
        return Some(Loc { line: sc.tok.0, col: sc.tok.1, trailing: false });
    }
    sc.ws();
    if sc.pos < src.len() {
        let (line, col) = sc.here();
        return Some(Loc { line, col, trailing: true });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(s: &str) -> (usize, usize, bool) {
        let l = token_location(s.as_bytes()).unwrap();
        (l.line, l.col, l.trailing)
    }

    #[test]
    fn jackson_token_locations() {
        assert_eq!(loc(r#"{"a":"#), (1, 2, false));
        assert_eq!(loc(r#"{"a":1,}"#), (1, 6, false));
        assert_eq!(loc(r#"{"a" 1}"#), (1, 2, false));
        assert_eq!(loc(r#"{"a":tru}"#), (1, 2, false));
        assert_eq!(loc(r#"{"a":{"b":[1,2,}}"#), (1, 16, false));
        assert_eq!(loc(r#"{"a":1}}"#), (1, 8, true));
        assert_eq!(loc(r#"{"a":"x\q"}"#), (1, 6, false));
        assert_eq!(loc("{\n \"a\":\n   [1, x]}"), (3, 8, false));
        assert_eq!(loc("{}x"), (1, 3, true));
        // Duplicate names: the token before the repeated name.
        assert_eq!(loc(r#"{"a":1,"a":2}"#), (1, 6, false));
        assert_eq!(loc(r#"{"x":{"b":"q","b":[1]}}"#), (1, 11, false));
        assert_eq!(loc("{\n \"a\": {\"c\":1},\n \"a\": 2}"), (2, 13, false));
        assert_eq!(loc(r#"{"a":{"b":1},"b":{"b":2}}"#.replace("}}", "}}x").as_str()), (1, 26, true));
    }

    #[test]
    fn duplicate_keys_are_document_parsing_errors() {
        let e = parse_doc(br#"{"a":1,"a":2}"#).unwrap_err();
        assert_eq!(e.kind, "document_parsing_exception");
        assert!(e.reason().starts_with("[1:6] failed to parse: [1:11] Duplicate field 'a'"), "{}", e.reason());
    }
}
