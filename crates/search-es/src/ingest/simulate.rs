//! `POST _ingest/_simulate` (`simulate.ingest`, `TransportSimulateBulkAction`): runs each
//! document through the pipelines its index would apply — the request's `pipeline`, else the
//! index's `default_pipeline`, then its `final_pipeline` — with `pipeline_substitutions`
//! standing in for stored pipelines, then checks the result against the index's mapping
//! (or, for an index that doesn't exist yet, the mapping its templates would give it). Nothing
//! is indexed and no mapping is updated.

use super::doc::IngestDoc;
use super::pipeline;
use super::processors::Flow;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::mapping::{Mapping, MappingCtx};
use crate::meta::settings::Settings;
use crate::rest::{Ctx, Reply};
use std::collections::HashMap;
use std::sync::Arc;

/// What a document's target index contributes: its settings and mapping.
struct TargetIndex {
    settings: Settings,
    mapping: Option<Mapping>,
    ctx: MappingCtx,
}

fn target_index(ctx: &Ctx, name: &str) -> EsResult<TargetIndex> {
    let st = ctx.state();
    if let Some(idx) = crate::write::resolve_existing(&st, name) {
        let mctx = idx.mapping_ctx();
        return Ok(TargetIndex { settings: idx.settings.clone(), mapping: Some((*idx.mapping).clone()), ctx: mctx });
    }
    // A new index: what its templates would give it.
    let composed = crate::meta::templates::for_new_index(&st.metadata, name, false)?;
    let mctx = MappingCtx::from_settings_in(st.dialect, &composed.settings);
    let mut sources = composed.mappings.clone();
    if let Some((_, t)) = crate::meta::templates::find_composable(&st.metadata, name, false)
        && let Some(ds) = &t.data_stream
    {
        let custom_routing = ds.get("allow_custom_routing").and_then(Value::as_bool).unwrap_or(false);
        sources.insert(0, crate::api::indices::default_timestamp_mapping(custom_routing));
        sources.push(crate::api::indices::data_stream_mapping_snippet());
    }
    let mapping = if sources.is_empty() { None } else { Some(crate::meta::templates::compose_mappings(&sources, &mctx)?) };
    Ok(TargetIndex { settings: composed.settings, mapping, ctx: mctx })
}

/// The pipeline setting of an index (`_none` is none).
fn pipeline_setting(s: &Settings, key: &str) -> Option<String> {
    s.get(key).filter(|p| *p != "_none" && !p.is_empty()).map(str::to_string)
}

pub fn simulate_ingest(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body_map()?.ok_or_else(|| EsError::parse("request body is required"))?;
    let default_index = ctx.path_param("index").map(str::to_string);
    let request_pipeline = ctx.query("pipeline").map(str::to_string);
    let mut subs: HashMap<String, Arc<pipeline::Pipeline>> = HashMap::new();
    if let Some(Value::Obj(ps)) = body.get("pipeline_substitutions") {
        for (name, def) in ps {
            // Substitutions are validated as stored pipelines are.
            if let Value::Obj(m) = def {
                for field in ["created_date", "created_date_millis", "modified_date", "modified_date_millis"] {
                    if m.contains_key(field) {
                        return Err(EsError::parse(format!("Provided a pipeline property which is managed by the system: {field}.")));
                    }
                }
                crate::api::ingest::validate_pipeline(name, m)?;
            }
            subs.insert(name.clone(), Arc::new(pipeline::compile(ctx.cluster, name, def)?));
        }
    }
    let docs = match body.get("docs") {
        Some(Value::Arr(d)) => d.clone(),
        _ => return Err(EsError::illegal_argument("must specify at least one document in [docs]")),
    };
    let mut out = Vec::with_capacity(docs.len());
    for d in &docs {
        let Value::Obj(dm) = d else { return Err(EsError::illegal_argument("malformed [docs] section, should include an inner object")) };
        let index = dm.get("_index").map(Value::to_es_string).or_else(|| default_index.clone()).unwrap_or_default();
        let id = dm.get("_id").map(Value::to_es_string);
        let source = match dm.get("_source") {
            Some(Value::Obj(m)) => m.clone(),
            _ => return Err(EsError::illegal_argument("must specify [_source] for each document")),
        };
        let target = target_index(ctx, &index)?;
        let first = match request_pipeline.as_deref() {
            Some("_none") => None,
            Some(p) => Some(p.to_string()),
            None => pipeline_setting(&target.settings, "index.default_pipeline"),
        };
        let fin = pipeline_setting(&target.settings, "index.final_pipeline");
        let mut meta = Map::new();
        meta.insert("_index".into(), Value::Str(index.clone()));
        meta.insert("_id".into(), Value::Str(id.clone().unwrap_or_else(|| "auto-generated".into())));
        meta.insert("_version".into(), Value::from(-3));
        let mut idoc = IngestDoc::new(source, meta, ctx.now_ms);
        // The simulated source is a default-sized hash map (16 buckets to start).
        idoc.initial_len = 0;
        let mut executed: Vec<String> = Vec::new();
        let run = pipeline::with_substitutions(&subs, || -> EsResult<bool> {
            for pid in first.iter().chain(fin.iter()) {
                let pl = pipeline::get(ctx.cluster, pid)?.ok_or_else(|| EsError::illegal_argument(format!("pipeline with id [{pid}] does not exist")))?;
                executed.push(pid.clone());
                if let Flow::Drop = pipeline::run_pipeline(ctx.cluster, &pl, &mut idoc, 0)? {
                    return Ok(false);
                }
            }
            Ok(true)
        });
        let id_shown = id.clone().unwrap_or_else(|| "<n/a>".into());
        let mut doc = Map::new();
        match run {
            Err(e) => {
                doc.insert("_id".into(), Value::Str(id.unwrap_or_else(|| "auto-generated".into())));
                doc.insert("_index".into(), Value::Str(index));
                let mut ev = e.to_value(false);
                // Per-document failures carry no processor headers here.
                if let Value::Obj(m) = &mut ev {
                    m.shift_remove("header");
                }
                doc.insert("error".into(), ev);
                out.push(crate::obj! { "doc" => Value::Obj(doc) });
                continue;
            }
            Ok(false) => {
                out.push(Value::Null);
                continue;
            }
            Ok(true) => {}
        }
        // Without a pipeline the document keeps its key order; ingest re-serializes it.
        let src = if executed.is_empty() { idoc.source.clone() } else { idoc.ordered_source() };
        let final_index = idoc.meta.get("_index").map(Value::to_es_string).unwrap_or(index);
        doc.insert("_id".into(), Value::Str(id_shown.clone()));
        doc.insert("_index".into(), Value::Str(final_index));
        doc.insert("_version".into(), idoc.meta.get("_version").cloned().unwrap_or(Value::from(-3)));
        doc.insert("_source".into(), Value::Obj(src.clone()));
        doc.insert("executed_pipelines".into(), Value::Arr(executed.into_iter().map(Value::Str).collect()));
        // The mapping check: the document parsed as the index would parse it.
        if let Some(mapping) = &target.mapping {
            let v = Value::Obj(src);
            let raw = v.to_json();
            if let Err(e) = crate::document::parse_document(mapping, &target.ctx, &v, Some(raw.as_bytes()), Some(&id_shown)) {
                doc.insert("error".into(), e.to_value(false));
            }
            let mv = mapping.to_value(&target.ctx, false);
            doc.insert("effective_mapping".into(), crate::obj! { "_doc" => mv });
        } else {
            doc.insert("effective_mapping".into(), Value::Obj(Map::new()));
        }
        out.push(crate::obj! { "doc" => Value::Obj(doc) });
    }
    Reply::ok(crate::obj! { "docs" => Value::Arr(out) })
}
