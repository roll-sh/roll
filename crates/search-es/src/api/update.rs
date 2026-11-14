//! The update API (`UpdateHelper`): partial documents merged into the current `_source`
//! (`detect_noop`), scripts run in the Painless update context (`ctx._source`, `ctx.op`),
//! upserts (`upsert`, `doc_as_upsert`, `scripted_upsert`), compare-and-set against the version
//! read, `retry_on_conflict`, and the updated `_source` in the response (`get`).

use crate::api::document;
use crate::api::Reply;
use crate::error::{EsError, EsResult};
use crate::fetch::FetchSource;
use crate::json::{Map, Value};
use crate::query::script::Script;
use crate::query::xparser::{Field, Tokens};
use crate::query::XParser;
use crate::rest::Ctx;
use crate::write::{self, RefreshPolicy, WriteArgs, WriteOutcome};
use bytes::Bytes;
use search_painless::es::write::{CtxKind, ctx_of, ctx_value};
use search_painless::{Context, NoHost};

/// A parsed update request body.
#[derive(Clone, Debug, Default)]
pub struct UpdateBody {
    pub doc: Option<Map>,
    pub upsert: Option<Map>,
    pub doc_as_upsert: bool,
    pub scripted_upsert: bool,
    pub detect_noop: bool,
    pub script: Option<Script>,
    pub source: Option<FetchSource>,
}

impl UpdateBody {
    /// `UpdateRequest.fromXContent`.
    pub fn parse(body: &Value, raw: Option<&[u8]>) -> EsResult<UpdateBody> {
        const FIELDS: &[Field] = &[
            Field::new("script", Tokens::OBJECT_OR_STRING),
            Field::new("scripted_upsert", Tokens::BOOLEAN),
            Field::new("upsert", Tokens::OBJECT),
            Field::new("doc", Tokens::OBJECT),
            Field::new("doc_as_upsert", Tokens::BOOLEAN),
            Field::new("detect_noop", Tokens::BOOLEAN),
            Field::new("_source", Tokens::ANY),
        ];
        let mut out = UpdateBody { detect_noop: true, ..Default::default() };
        let mut p = XParser::new(body, raw, Vec::new());
        p.object("UpdateRequest", FIELDS, |p, i| {
            match i {
                0 => out.script = Some(Script::parse(p)?),
                1 => out.scripted_upsert = p.boolean_value()?,
                2 => {
                    out.upsert = Some(p.map()?);
                }
                3 => {
                    out.doc = Some(p.map()?);
                }
                4 => out.doc_as_upsert = p.boolean_value()?,
                5 => out.detect_noop = p.boolean_value()?,
                _ => {
                    let v = p.take_value();
                    out.source = Some(FetchSource::from_value(&v)?);
                }
            }
            Ok(())
        })?;
        Ok(out)
    }

    /// `UpdateRequest.validate` (`retries` is `retry_on_conflict`).
    pub fn validate(&self, args: &WriteArgs, retries: i64) -> Vec<String> {
        let mut a = args.clone();
        a.id = None;
        let mut errs = write::validate_args(&a, write::Action::Update);
        if args.if_seq_no.is_some() {
            if retries > 0 {
                errs.push("compare and write operations can not be retried".into());
            }
            if self.doc_as_upsert {
                errs.push("compare and write operations can not be used with upsert".into());
            }
            if self.upsert.is_some() {
                errs.push("upsert requests don't support `if_seq_no` and `if_primary_term`".into());
            }
        }
        if self.script.is_none() && self.doc.is_none() {
            errs.push("script or doc is missing".into());
        }
        if self.script.is_some() && self.doc.is_some() {
            errs.push("can't provide both script and doc".into());
        }
        if self.doc.is_none() && self.doc_as_upsert {
            errs.push("doc must be specified if doc_as_upsert is enabled".into());
        }
        errs
    }
}

/// `XContentHelper.update`: merges `changes` into `source`; whether anything changed.
pub fn merge_doc(source: &mut Map, changes: &Map, check_noop: bool) -> bool {
    let mut modified = false;
    for (k, v) in changes {
        match (source.get_mut(k), v) {
            (Some(Value::Obj(old)), Value::Obj(new)) => {
                if crate::stack::maybe_grow(|| merge_doc(old, new, check_noop)) {
                    modified = true;
                }
            }
            (Some(old), new) => {
                if !check_noop || old != new {
                    modified = true;
                }
                *old = new.clone();
            }
            (None, new) => {
                modified = true;
                source.insert(k.clone(), new.clone());
            }
        }
    }
    modified
}

/// The result of an update before it is rendered.
pub struct UpdateOutcome {
    pub write: WriteOutcome,
    /// The `_source` after the update (for `get`), when it was indexed or is unchanged.
    pub source: Option<Bytes>,
}

fn script_failed(e: EsError) -> EsError {
    EsError::illegal_argument("failed to execute script").caused_by(e)
}

/// What a script decided.
enum ScriptOp {
    Index(String),
    Delete,
    Noop,
}

#[allow(clippy::too_many_arguments)]
fn run_script(ctx: &Ctx, script: &Script, kind: CtxKind, source: &str, index: &str, id: &str, version: Option<i64>, routing: Option<&str>) -> EsResult<ScriptOp> {
    let st = ctx.state();
    let bound = ctx.cluster.scripts.compile(&st, script, Context::Update).map_err(script_failed)?;
    let Some(sc) = bound.painless() else {
        return Err(EsError::illegal_argument("failed to execute script"));
    };
    let parsed = match kind {
        CtxKind::Upsert => search_painless::xcontent::parse_source_unordered(source),
        _ => search_painless::xcontent::parse_source(source),
    }
    .map_err(|e| EsError::parse(format!("{e:?}")))?;
    use search_painless::Value as PV;
    let mut meta: Vec<(&str, PV)> = vec![("_index", PV::str(index)), ("_id", PV::str(id))];
    if kind == CtxKind::Upsert {
        meta.push(("op", PV::str("create")));
    } else {
        meta.push(("_version", PV::Long(version.unwrap_or(1))));
        meta.push(("_routing", routing.map(PV::str).unwrap_or(PV::Null)));
        meta.push(("_type", PV::str("_doc")));
        meta.push(("op", PV::str("index")));
    }
    meta.push(("_now", PV::Long(ctx.now_ms)));
    let cv = ctx_value(kind, parsed, &meta, None).map_err(EsError::illegal_argument)?;
    let mut inst = sc.instance();
    inst.set_params(&bound.params);
    inst.set_var("ctx", cv.clone());
    inst.execute(&mut NoHost).map_err(|e| script_failed(crate::scripting::script_error(e)))?;
    let c = ctx_of(&cv).ok_or_else(|| EsError::illegal_state("update script lost its ctx"))?;
    let op = c.op().unwrap_or_default();
    let created = kind == CtxKind::Upsert;
    match op.as_str() {
        "index" | "create" if (op == "create") == created => {
            let s = search_painless::xcontent::to_xcontent(&c.source()).map_err(|e| EsError::illegal_argument(e.reason.clone()))?;
            Ok(ScriptOp::Index(s))
        }
        "delete" if !created => Ok(ScriptOp::Delete),
        "none" | "noop" => Ok(ScriptOp::Noop),
        other => {
            ctx.warn(format!("[op] must be one of [create, none], not [{other}]"));
            Ok(ScriptOp::Noop)
        }
    }
}

/// Runs one update (with retries on conflicts).
pub async fn update_one(ctx: &Ctx<'_>, expr: &str, id: &str, args: &WriteArgs, body: &UpdateBody, retries: i64) -> EsResult<UpdateOutcome> {
    let mut attempt = 0;
    loop {
        match update_attempt(ctx, expr, id, args, body).await {
            Err(e) if e.kind == "version_conflict_engine_exception" && attempt < retries && args.if_seq_no.is_none() => {
                attempt += 1;
            }
            r => return r,
        }
    }
}

async fn update_attempt(ctx: &Ctx<'_>, expr: &str, id: &str, args: &WriteArgs, body: &UpdateBody) -> EsResult<UpdateOutcome> {
    let mut wa = args.clone();
    wa.create = true;
    let target = write::resolve_target(ctx.cluster, expr, &wa, ctx.now_ms).await?;
    let idx = target.index.clone();
    if crate::engine::tsdb::is_time_series(&idx) {
        return Err(crate::engine::tsdb::unsupported(&idx, "update"));
    }
    let routing = write::effective_routing(&target, args.routing.as_deref(), id)?;
    if write::seq_no_disabled(&idx) {
        let shard = crate::meta::routing::shard_for(&idx, id, routing.as_deref());
        return Err(write::seq_no_unsupported(&idx, shard, "update_not_supported_exception", "Updates are"));
    }
    write::check_routing(&idx, id, routing.as_deref())?;
    let gargs = document::GetArgs { routing: routing.clone(), realtime: true, ..Default::default() };
    let got = document::get_one(ctx, &idx, id, &gargs)?;
    let shard = crate::meta::routing::shard_for(&idx, id, routing.as_deref());
    // A noop update counts in the shard's `indexing.noop_update_total`.
    let count_noop = || {
        if let Some(e) = ctx.cluster.engine.loaded(&idx.uuid) {
            e.shard(shard).counters.noop_update_total.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    };
    let noop = |version: i64, seq_no: i64, primary_term: i64, source: Option<Bytes>| UpdateOutcome {
        write: WriteOutcome {
            index: idx.name.clone(),
            uuid: idx.uuid.clone(),
            id: id.to_string(),
            shard,
            version,
            result: "noop",
            seq_no,
            primary_term,
            total: 0,
            forced_refresh: false,
                failure_store: None,
                executed_pipelines: None,
        },
        source,
    };
    let Some(g) = got else {
        // Upserts.
        let source: String = if body.doc_as_upsert {
            Value::Obj(body.doc.clone().unwrap_or_default()).to_json()
        } else if let Some(u) = &body.upsert {
            let raw = Value::Obj(u.clone()).to_json();
            if body.scripted_upsert
                && let Some(s) = &body.script
            {
                match run_script(ctx, s, CtxKind::Upsert, &raw, &idx.name, id, None, routing.as_deref())? {
                    ScriptOp::Index(src) => src,
                    ScriptOp::Noop | ScriptOp::Delete => return Ok(noop(-1, -2, 0, None)),
                }
            } else {
                raw
            }
        } else {
            return Err(write::shard_error(EsError::new(404, "document_missing_exception", format!("[{id}]: document missing")), &idx, shard));
        };
        let mut a = args.clone();
        a.id = Some(id.to_string());
        a.routing = routing.clone();
        a.create = true;
        a.internal = true;
        let bytes = Bytes::from(source);
        let out = document::index_one(ctx, expr, a, bytes.clone(), false).await?;
        return Ok(UpdateOutcome { write: out, source: Some(bytes) });
    };
    let d = &g.doc;
    if let (Some(s), Some(t)) = (args.if_seq_no, args.if_primary_term)
        && (d.seq_no != s || d.primary_term != t)
    {
        return Err(write::shard_error(
            EsError::new(
                409,
                "version_conflict_engine_exception",
                format!("[{id}]: version conflict, required seqNo [{s}], primary term [{t}]. current document has seqNo [{}] and primary term [{}]", d.seq_no, d.primary_term),
            ),
            &idx,
            shard,
        ));
    }
    let src_bytes = crate::fetch::source_bytes(&d.fields).map(|b| b.to_vec());
    let Some(src_bytes) = src_bytes else {
        return Err(EsError::new(400, "document_source_missing_exception", format!("[{id}]: document source missing"))
            .with_meta("index_uuid", idx.uuid.as_str())
            .with_meta("shard", shard.to_string())
            .with_meta("index", idx.name.as_str()));
    };
    // Vectors kept out of `_source` come back from the index (as floats) for the update.
    let src_bytes = {
        let table = ctx.cluster.engine.index(ctx.cluster, &ctx.state(), &idx)?.table();
        if table.has_vectors && idx.settings.get("index.mapping.exclude_source_vectors") != Some("false") {
            match crate::fetch::parse_source(&src_bytes)? {
                Value::Obj(mut m) => {
                    crate::engine::synthetic::vectors(&table, "", &mut m, false);
                    Value::Obj(m).to_json().into_bytes()
                }
                _ => src_bytes,
            }
        } else {
            src_bytes
        }
    };
    let new_source: Bytes = if let Some(doc) = &body.doc {
        let mut cur = match crate::fetch::parse_source(&src_bytes)? {
            Value::Obj(m) => m,
            _ => Map::new(),
        };
        let changed = merge_doc(&mut cur, doc, body.detect_noop);
        if !changed && body.detect_noop {
            count_noop();
            return Ok(noop(d.version, d.seq_no, d.primary_term, Some(Bytes::from(src_bytes))));
        }
        Bytes::from(Value::Obj(cur).to_json())
    } else if let Some(s) = &body.script {
        let raw = String::from_utf8_lossy(&src_bytes).into_owned();
        let existing_routing = crate::fetch::routing(&d.fields);
        match run_script(ctx, s, CtxKind::Update, &raw, &idx.name, id, Some(d.version), existing_routing.as_deref())? {
            ScriptOp::Index(src) => Bytes::from(src),
            ScriptOp::Noop => {
                count_noop();
                return Ok(noop(d.version, d.seq_no, d.primary_term, Some(Bytes::from(src_bytes))));
            }
            ScriptOp::Delete => {
                let mut a = args.clone();
                a.if_seq_no = Some(d.seq_no);
                a.if_primary_term = Some(d.primary_term);
                let out = write::apply_delete(ctx.cluster, &idx, id, routing.as_deref(), &a).map_err(conflict_as_update)?;
                return Ok(UpdateOutcome { write: out, source: None });
            }
        }
    } else {
        return Err(EsError::action_request_validation(&["script or doc is missing".into()]));
    };
    // `UpdateHelper.calculateRouting`: the updated document keeps the routing it was indexed
    // with (the request's routing only finds it).
    let routing = crate::fetch::routing(&d.fields).or(routing);
    let mut a = args.clone();
    a.id = Some(id.to_string());
    a.routing = routing.clone();
    a.if_seq_no = Some(d.seq_no);
    a.if_primary_term = Some(d.primary_term);
    a.internal = true;
    a.create = false;
    // `semantic_text` fields whose text the update changed lose their stale embeddings (they're
    // embedded again as the document is written); unchanged ones keep theirs.
    let new_source = match crate::xpack::inference::semantic::drop_stale(&idx.mapping, &src_bytes, &new_source) {
        Some(b) => b,
        None => new_source,
    };
    let value = document::parse_doc(&new_source)?;
    let prepared = write::prepare(ctx.cluster, idx.clone(), id, routing.as_deref(), &new_source, &value, None).await?;
    let out = write::apply_index(prepared, &a, false, id)?;
    Ok(UpdateOutcome { write: out, source: Some(new_source) })
}

fn conflict_as_update(e: EsError) -> EsError {
    e
}

/// The update response: the write response plus `get` when `_source` was asked for.
pub fn render(out: &UpdateOutcome, fs: Option<&FetchSource>) -> EsResult<Value> {
    let mut m = Map::new();
    out.write.write_into(&mut m);
    if let Some(fs) = fs
        && fs.fetch
        && let Some(src) = &out.source
        && out.write.result != "deleted"
    {
        let mut g = Map::new();
        if out.write.seq_no >= 0 {
            g.insert("_seq_no".into(), Value::from(out.write.seq_no));
            g.insert("_primary_term".into(), Value::from(out.write.primary_term));
        }
        g.insert("found".into(), Value::Bool(true));
        let v = crate::fetch::parse_source(src)?;
        let v = match (fs.filter(), v) {
            (Some(f), Value::Obj(sm)) => Value::Obj(f.apply(&sm)),
            (_, v) => v,
        };
        g.insert("_source".into(), v);
        m.insert("get".into(), Value::Obj(g));
    }
    Ok(Value::Obj(m))
}

/// `POST /{index}/_update/{id}`.
pub async fn update(ctx: &Ctx<'_>) -> EsResult<Reply> {
    if ctx.has_query("version") || ctx.has_query("version_type") {
        return Err(EsError::action_request_validation(&[
            "internal versioning can not be used for optimistic concurrency control. Please use `if_seq_no` and `if_primary_term` instead".into(),
        ]));
    }
    let mut args = document::write_args(ctx)?;
    RefreshPolicy::parse(ctx.query("refresh"))?;
    let body_v = ctx.body()?.unwrap_or(Value::Obj(Map::new()));
    let mut body = UpdateBody::parse(&body_v, ctx.raw_json())?;
    if let Some(fs) = FetchSource::from_params(ctx.query("_source"), ctx.query("_source_includes"), ctx.query("_source_excludes"))? {
        body.source = Some(fs);
    }
    let retries = ctx.int_param("retry_on_conflict", 0)?;
    let errs = body.validate(&args, retries);
    if !errs.is_empty() {
        return Err(EsError::action_request_validation(&errs));
    }
    let expr = ctx.path_param("index").unwrap_or_default().to_string();
    let id = args.id.take().unwrap_or_default();
    if let Some(e) = crate::opensearch::append_only_violation(&ctx.state(), &expr, "UPDATE", Some(&id)) {
        return Err(e);
    }
    let mut out = update_one(ctx, &expr, &id, &args, &body, retries).await?;
    match RefreshPolicy::parse(ctx.query("refresh"))? {
        RefreshPolicy::None => {}
        p => {
            if out.write.result != "noop" {
                write::refresh_shards(ctx.cluster, &[(out.write.uuid.clone(), out.write.shard)])?;
                out.write.forced_refresh = p == RefreshPolicy::Immediate;
            }
        }
    }
    let status = out.write.status();
    Ok(Reply::Body(status, render(&out, body.source.as_ref())?))
}
