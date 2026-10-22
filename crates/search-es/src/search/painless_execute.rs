//! `POST _scripts/painless/_execute` (`PainlessExecuteAction`): runs one inline script in a
//! named context and returns its result.
//!
//! `painless_test` (the default) runs without a document and prints the result with Java's
//! `toString`. Document contexts (`filter`, `score`, the runtime field contexts) need a
//! `context_setup`: its `document` is parsed through the index's mapping (dynamic fields map
//! for this request only; nothing is published) into a one-document shard of a private
//! in-memory engine, which the script then reads like any shard. `score` scripts see the
//! `_score` of the setup's `query` (match-all by default).

use super::scripts::{DocHost, Leaf};
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::query::script::{Script, ScriptType};
use crate::rest::{Ctx, Reply};
use search_painless::Context;
use std::sync::Arc;

/// The script contexts the API runs a document in, and the runtime type they emit.
fn runtime_type(ctx: Context) -> Option<&'static str> {
    Some(match ctx {
        Context::BooleanField => "boolean",
        Context::DateField => "date",
        Context::DoubleField => "double",
        Context::GeoPointField => "geo_point",
        Context::IpField => "ip",
        Context::KeywordField => "keyword",
        Context::LongField => "long",
        Context::CompositeField => "composite",
        _ => return None,
    })
}

/// `POST _scripts/painless/_execute`.
pub fn execute(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body_map()?.unwrap_or_default();
    let mut script: Option<Script> = None;
    let mut context_name: Option<String> = None;
    let mut setup: Option<Map> = None;
    for (k, v) in &body {
        match k.as_str() {
            "script" => {
                let mut p = crate::query::XParser::from_value(v);
                p.next_token();
                script = Some(Script::parse(&mut p)?);
            }
            "context" => context_name = Some(v.to_es_string()),
            "context_setup" => setup = v.as_object().cloned(),
            other => return Err(EsError::x_content_parse_bare(format!("[painless_execute_request] unknown field [{other}]"))),
        }
    }
    let context = match context_name.as_deref() {
        None => Context::PainlessTest,
        Some(n) => match Context::from_name(n) {
            Some(c) if c == Context::PainlessTest || matches!(c, Context::Filter | Context::Score) || runtime_type(c).is_some() => c,
            _ => {
                return Err(EsError::x_content_parse_bare("Failed to build [painless_execute_request] after last required field arrived")
                    .caused_by(EsError::java(500, "unsupported_operation_exception", format!("unsupported script context name [{n}]"))));
            }
        },
    };
    let Some(script) = script else {
        return Err(EsError::action_request_validation(&["script is missing".into()]));
    };
    if script.ty == ScriptType::Stored {
        return Err(EsError::action_request_validation(&["only inline scripts are supported".into()]));
    }
    if context != Context::PainlessTest && setup.is_none() {
        return Err(EsError::java(500, "null_pointer_exception", "Cannot read field \"index\" because \"this.contextSetup\" is null"));
    }
    let st = ctx.state();
    let bound = ctx.cluster.scripts.compile(&st, &script, context)?;
    let Some(compiled) = bound.painless().cloned() else {
        return Err(EsError::illegal_argument("only painless scripts are supported"));
    };
    if context == Context::PainlessTest {
        let mut inst = compiled.instance();
        inst.set_params(&bound.params);
        let v = inst.execute(&mut search_painless::NoHost).map_err(crate::scripting::script_error)?;
        return Reply::ok(obj! { "result" => v.to_java_string() });
    }
    let Some(setup) = setup else {
        return Err(EsError::java(500, "null_pointer_exception", "Cannot read field \"index\" because \"this.contextSetup\" is null"));
    };
    let index = setup.get("index").map(Value::to_es_string).ok_or_else(|| EsError::illegal_argument("index is a required parameter for current context"))?;
    let document = setup.get("document").cloned().ok_or_else(|| EsError::illegal_argument("document is a required parameter for current context"))?;
    let idx = crate::api::document::single_index(ctx, &index)?;
    let doc = OneDoc::build(ctx, &idx, &document)?;
    let table = doc.engine.table();
    let searcher = doc.engine.shard(0).searcher();
    let Some(seg) = searcher.segments().first().cloned() else {
        return Err(EsError::illegal_state("the document was not indexed"));
    };
    let local = 0u32;
    let host = || DocHost::new(table.clone(), seg.clone(), 0, idx.name.clone());
    let script_err = crate::scripting::script_error;
    match context {
        Context::Filter => {
            let mut leaf = Leaf::new(&compiled, &bound.params, host());
            let r = leaf.run_bool(local).map_err(script_err)?;
            Reply::ok(obj! { "result" => r })
        }
        Context::Score => {
            let score = match setup.get("query") {
                Some(q) => {
                    let (parsed, _) = crate::query::parse_query(q, None, &[])?;
                    let qctx = super::query::QueryCtx::new(ctx.cluster, &st, &idx, &table, &searcher, ctx.now_ms);
                    let query = qctx.to_query(&parsed)?;
                    let e = searcher.explain(&query, seg.base + local).map_err(crate::engine::convert::index_error)?;
                    if e.is_match() { e.value() as f64 } else { 0.0 }
                }
                None => 0.0,
            };
            let mut leaf = Leaf::new(&compiled, &bound.params, host());
            let r = leaf.run_f64(local, score).map_err(script_err)?;
            Reply::ok(obj! { "result" => r })
        }
        Context::CompositeField => {
            let mut leaf = Leaf::new(&compiled, &bound.params, host());
            leaf.set_field_name("composite_field");
            leaf.run_emit(local).map_err(script_err)?;
            let mut out = Map::new();
            for (name, vals) in leaf.emitted_fields() {
                let name = if name.starts_with("composite_field.") { name.clone() } else { format!("composite_field.{name}") };
                out.insert(name, Value::Arr(vals.iter().map(super::scripts::to_value).collect()));
            }
            Reply::ok(obj! { "result" => Value::Obj(out) })
        }
        c => {
            let ty = runtime_type(c).unwrap_or("keyword");
            let mut defs = Map::new();
            defs.insert(format!("{}_field", ty), obj! { "type" => ty, "script" => script_value(&script) });
            let rt = super::runtime::RuntimeFields::of(&idx.mapping, Some(&defs))?;
            let Some(f) = rt.get(&format!("{ty}_field")).cloned() else { return Reply::ok(obj! { "result" => Value::Arr(Vec::new()) }) };
            let ev = super::runtime::Evaluator::new(&ctx.cluster.scripts, &st, &table, 0, &idx.name, f.clone())?;
            let vals = ev.leaf(&seg)?.values(local)?;
            Reply::ok(obj! { "result" => Value::Arr(super::runtime::render(&f, &vals, None)?) })
        }
    }
}

/// The script as a runtime field definition holds it.
fn script_value(s: &Script) -> Value {
    let mut m = Map::new();
    m.insert("source".into(), Value::Str(s.id_or_code.clone()));
    if let Some(l) = &s.lang {
        m.insert("lang".into(), Value::Str(l.clone()));
    }
    if !s.params.is_empty() {
        m.insert("params".into(), Value::Obj(s.params.clone()));
    }
    Value::Obj(m)
}

/// A document indexed alone, for the script to read.
struct OneDoc {
    engine: Arc<crate::engine::IndexEngine>,
}

impl OneDoc {
    fn build(ctx: &Ctx, idx: &Arc<crate::meta::IndexMeta>, document: &Value) -> EsResult<OneDoc> {
        let st = ctx.state();
        let mut meta = (**idx).clone();
        meta.settings.set("index.number_of_shards", "1");
        let mctx = meta.mapping_ctx();
        let source = bytes::Bytes::from(document.to_json());
        // Dynamic fields map for this request only.
        let mut parsed = None;
        for _ in 0..8 {
            let p = crate::document::parse_document(&meta.mapping, &mctx, document, Some(&source), Some("_id"))?;
            match &p.dynamic_update {
                Some(u) => meta.mapping = Arc::new(meta.mapping.merge(u, &mctx, crate::mapping::MergeReason::MappingUpdate)?),
                None => {
                    parsed = Some(p);
                    break;
                }
            }
        }
        let parsed = parsed.ok_or_else(|| EsError::illegal_state("mapping updates did not converge"))?;
        let private = crate::engine::Engine::with_storage(Arc::new(crate::engine::MemoryShards));
        let engine = private.index(ctx.cluster, &st, &meta)?;
        let table = engine.table();
        let stored = crate::write::stored_source(&meta, &table, &source, document);
        let doc = crate::engine::convert::to_indexable(&table, &parsed, &crate::engine::convert::DocMeta { id: "_id", routing: None, tsid: None, source: stored })?;
        let shard = engine.shard(0).clone();
        shard.shard.index(search_index::IndexOp::new(doc)).map_err(crate::engine::convert::index_error)?;
        shard.wrote();
        shard.refresh()?;
        Ok(OneDoc { engine })
    }
}
