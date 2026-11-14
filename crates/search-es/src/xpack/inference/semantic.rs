//! `semantic_text` fields: their mapping parameters (`SemanticTextFieldMapper`), the model
//! settings the first embedded document records (`MinimalServiceSettings`), and which endpoints a
//! mapping uses.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::mapping::{FieldMapper, Mapper, Mapping, ObjectMapper};
use crate::xpack::inference::TaskType;

/// The endpoint a `semantic_text` field uses when its mapping names none (a cluster without
/// Elastic's hosted inference service falls back to ELSER on ML nodes).
pub const DEFAULT_INFERENCE_ID: &str = ".elser-2-elasticsearch";

/// What a field's embeddings are: the task, and for dense ones their shape.
#[derive(Clone, Debug, PartialEq)]
pub struct ModelSettings {
    /// The endpoint's service (printed first; not compared).
    pub service: Option<String>,
    pub task: TaskType,
    pub dimensions: Option<i64>,
    pub similarity: Option<String>,
    pub element_type: Option<String>,
}

fn iae(msg: impl Into<String>) -> EsError {
    EsError::illegal_argument(msg)
}

impl ModelSettings {
    /// `SemanticTextField.parseModelSettingsFromMap`.
    pub fn parse(v: &Value) -> EsResult<ModelSettings> {
        let Value::Obj(m) = v else { return Err(iae("[model_settings] must be an object")) };
        for k in m.keys() {
            if !matches!(k.as_str(), "task_type" | "dimensions" | "similarity" | "element_type" | "service") {
                return Err(EsError::x_content_parse_bare(format!("[model_settings] unknown field [{k}]")));
            }
        }
        let task_name = m.get("task_type").map(Value::to_es_string).ok_or_else(|| iae("Required [task_type]"))?;
        let task = TaskType::parse(&task_name).ok_or_else(|| iae(format!("Unknown task_type [{task_name}]")))?;
        let s = ModelSettings {
            service: m.get("service").and_then(Value::as_str).map(str::to_owned),
            task,
            dimensions: m.get("dimensions").and_then(Value::as_i64),
            similarity: m.get("similarity").map(|v| v.to_es_string().to_ascii_lowercase()),
            element_type: m.get("element_type").map(|v| v.to_es_string().to_ascii_lowercase()),
        };
        let upper = task.upper();
        let dense = matches!(task, TaskType::TextEmbedding | TaskType::Embedding);
        for (field, present) in [("dimensions", s.dimensions.is_some()), ("similarity", s.similarity.is_some()), ("element_type", s.element_type.is_some())] {
            if dense && !present {
                return Err(iae(format!("required [{field}] field is missing for task_type [{upper}]")));
            }
            if !dense && present {
                return Err(iae(format!("[{field}] is not allowed for task_type [{upper}]")));
            }
        }
        if !matches!(task, TaskType::SparseEmbedding | TaskType::TextEmbedding | TaskType::Embedding) {
            return Err(iae(format!("Wrong [task_type], expected text_embedding or sparse_embedding, got {}", task.name())));
        }
        Ok(s)
    }

    /// `getFilteredXContentObject`.
    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        if let Some(s) = &self.service {
            m.insert("service".into(), s.clone().into());
        }
        m.insert("task_type".into(), self.task.name().into());
        if let Some(d) = self.dimensions {
            m.insert("dimensions".into(), d.into());
        }
        if let Some(s) = &self.similarity {
            m.insert("similarity".into(), s.clone().into());
        }
        if let Some(e) = &self.element_type {
            m.insert("element_type".into(), e.clone().into());
        }
        Value::Obj(m)
    }

    /// `MinimalServiceSettings.canMergeWith`: the same embeddings, whatever the service.
    pub fn compatible(&self, o: &ModelSettings) -> bool {
        self.task == o.task && self.dimensions == o.dimensions && self.similarity == o.similarity && self.element_type == o.element_type
    }

    /// The settings of an endpoint's embeddings (`new MinimalServiceSettings(model)`).
    pub fn of_endpoint(ep: &super::Endpoint) -> Option<ModelSettings> {
        let ss = &ep.service_settings;
        match ep.task {
            TaskType::SparseEmbedding => Some(ModelSettings { service: Some(ep.service.clone()), task: ep.task, dimensions: None, similarity: None, element_type: None }),
            TaskType::TextEmbedding | TaskType::Embedding => Some(ModelSettings {
                service: Some(ep.service.clone()),
                task: ep.task,
                dimensions: ss.get("dimensions").and_then(Value::as_i64),
                similarity: Some(ss.get("similarity").map(|v| v.to_es_string()).unwrap_or_else(|| "cosine".into())),
                element_type: Some(
                    ss.get("element_type").or_else(|| ss.get("embedding_type")).map(|v| v.to_es_string()).map(|t| match t.as_str() {
                        "int8" => "byte".to_string(),
                        "binary" => "bit".to_string(),
                        _ => t,
                    }).unwrap_or_else(|| "float".into()),
                ),
            }),
            _ => None,
        }
    }
}

/// The mapping parameters' own checks (`SemanticFieldMapper.Builder`'s validators).
pub fn validate_mapper(f: &FieldMapper) -> EsResult<()> {
    for k in ["inference_id", "search_inference_id"] {
        if let Some(Value::Str(s)) = f.raw(k)
            && s.is_empty()
        {
            return Err(iae(format!("[{k}] on mapper [{}] of type [semantic_text] must not be empty", f.name)));
        }
    }
    if let Some(m) = f.raw("model_settings").filter(|v| !v.is_null()) {
        // Read by an object parser, whose failure Elasticsearch's mapping parser reports wrapped.
        ModelSettings::parse(m).map_err(|e| {
            let msg = "Failed to build [model_settings] after last required field arrived";
            EsError::new(500, "exception", format!("org.elasticsearch.xcontent.XContentParseException: {msg}")).caused_by(EsError::x_content_parse_bare(msg).caused_by(e))
        })?;
    }
    if let Some(c) = f.raw("chunking_settings").filter(|v| !v.is_null()) {
        super::chunking::Chunking::from_request(Some(c)).map_err(|e| {
            let class = if e.kind == "validation_exception" { "org.elasticsearch.common.ValidationException" } else { "java.lang.IllegalArgumentException" };
            // A `ValidationException` isn't an `ElasticsearchException`: the wrapper is the root cause.
            EsError::new(500, "exception", format!("{class}: {}", e.reason())).caused_by(EsError::java(400, e.kind.clone(), e.reason().to_string()))
        })?;
    }
    Ok(())
}

/// The path of a field's chunks (nested documents) and of their embeddings.
pub fn chunks_path(field: &str) -> String {
    format!("{field}.inference.chunks")
}

pub fn embeddings_path(field: &str) -> String {
    format!("{field}.inference.chunks.embeddings")
}

/// The hidden mappers below a `semantic_text` field (`SemanticTextFieldMapper.createChunksField`):
/// `inference` → `chunks` (nested, not dynamic) → `embeddings`, a `sparse_vector` or a
/// `dense_vector` shaped by the model settings, with the field's `index_options`. Built by
/// parsing the equivalent mapping, so they behave exactly like mapped fields of those types.
pub fn hidden_mapper(f: &FieldMapper) -> Option<ObjectMapper> {
    let ms = model_settings(f)?;
    let options = f.raw("index_options").filter(|v| !v.is_null());
    let mut emb = Map::new();
    match ms.task {
        TaskType::SparseEmbedding => {
            emb.insert("type".into(), "sparse_vector".into());
            if let Some(o) = options.and_then(|o| o.get("sparse_vector")) {
                emb.insert("index_options".into(), o.clone());
            }
        }
        _ => {
            emb.insert("type".into(), "dense_vector".into());
            emb.insert("dims".into(), ms.dimensions.unwrap_or(1).into());
            emb.insert("similarity".into(), ms.similarity.clone().unwrap_or_else(|| "cosine".into()).into());
            emb.insert("element_type".into(), ms.element_type.clone().unwrap_or_else(|| "float".into()).into());
            if let Some(o) = options.and_then(|o| o.get("dense_vector")) {
                emb.insert("index_options".into(), o.clone());
            }
        }
    }
    let mapping = crate::obj! {
        "properties" => crate::obj! {
            "inference" => crate::obj! {
                "properties" => crate::obj! {
                    "chunks" => crate::obj! { "type" => "nested", "dynamic" => "false", "properties" => crate::obj! { "embeddings" => Value::Obj(emb) } },
                },
            },
        },
    };
    let ctx = crate::mapping::MappingCtx::from_settings(&crate::meta::settings::Settings::default());
    Mapping::parse(mapping.as_object()?, &ctx).ok().map(|m| m.root)
}

/// A field's recorded model settings.
pub fn model_settings(f: &FieldMapper) -> Option<ModelSettings> {
    f.raw("model_settings").filter(|v| !v.is_null()).and_then(|v| ModelSettings::parse(v).ok())
}

/// The endpoint a field embeds its text with.
pub fn inference_id(f: &FieldMapper) -> String {
    f.raw("inference_id").and_then(Value::as_str).unwrap_or(DEFAULT_INFERENCE_ID).to_string()
}

/// The endpoint a field embeds queries with.
pub fn search_inference_id(f: &FieldMapper) -> String {
    f.raw("search_inference_id").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| inference_id(f))
}

/// The `inference_id` of every `semantic_text` field of a mapping (one entry per field).
pub fn inference_ids(mapping: &Mapping) -> Vec<String> {
    fields(mapping).into_iter().map(|(_, f)| inference_id(f)).collect()
}

/// Every `semantic_text` field of a mapping with its full path.
pub fn fields(mapping: &Mapping) -> Vec<(String, &FieldMapper)> {
    let mut out = Vec::new();
    walk(&mapping.root, "", &mut out);
    out
}

fn walk<'a>(o: &'a ObjectMapper, prefix: &str, out: &mut Vec<(String, &'a FieldMapper)>) {
    for (k, m) in &o.children {
        let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        match m {
            Mapper::Object(c) => walk(c, &path, out),
            Mapper::Field(f) if f.ty == "semantic_text" => out.push((path, f)),
            _ => {}
        }
    }
}

/// Whether a root key of a document holds inference results (`_inference_fields`, or one
/// field's as a dotted key `_inference_fields.<field>`).
pub fn is_inference_key(k: &str) -> bool {
    k == "_inference_fields" || k.starts_with("_inference_fields.")
}

/// The inference results a document carries, by field path, with the root key each came from.
pub fn inference_entries(doc: &Map) -> Vec<(String, &Value, &str)> {
    let mut out = Vec::new();
    for (k, v) in doc {
        if k == "_inference_fields" {
            if let Value::Obj(m) = v {
                for (f, e) in m {
                    out.push((f.clone(), e, k.as_str()));
                }
            }
        } else if let Some(f) = k.strip_prefix("_inference_fields.") {
            out.push((f.to_string(), v, k.as_str()));
        }
    }
    out
}

/// Reads a document's `_inference_fields` (`SemanticTextFieldMapper.parseCreateField`): checks
/// each field's results against its mapping and turns every chunk into a nested document holding
/// its embeddings. Model settings the field hasn't recorded yet are returned as mapping updates
/// (path, settings), as the first embedded document records them on Elasticsearch.
pub fn index_inference(
    mapping: &Mapping,
    doc: &Map,
    source: Option<&[u8]>,
    desc: &str,
    out: &mut crate::document::ParsedDocument,
) -> EsResult<Vec<(String, Value)>> {
    use crate::document::{DocField, FieldValue, NestedDoc};
    let mut updates = Vec::new();
    for (path, entry, key) in inference_entries(doc) {
        let at = source.and_then(|s| crate::json::locate::locate(s, &[crate::json::locate::Seg::Key(key)], crate::json::locate::Target::Value)).unwrap_or((1, 1));
        let located = |msg: String| EsError::document_parsing(format!("[{}:{}] {msg}", at.0, at.1));
        let wrapped = |cause: EsError| {
            let preview = super::settings::java_string(entry);
            EsError::document_parsing(format!(
                "[{}:{}] failed to parse field [_inference_fields] of type [_inference_fields] in {desc}. Preview of field's value: '{preview}'",
                at.0, at.1
            ))
            .caused_by(cause)
        };
        let Some(crate::mapping::MapperRef::Field(f)) = mapping.lookup(&path).filter(|m| matches!(m, crate::mapping::MapperRef::Field(f) if f.ty == "semantic_text")) else {
            return Err(wrapped(iae(format!("Field [{path}] is not registered as a [semantic_text] field type"))));
        };
        let inference = entry.get("inference").ok_or_else(|| wrapped(iae("Required [inference]")))?;
        let id = inference.get("inference_id").and_then(Value::as_str).ok_or_else(|| wrapped(iae("Required [inference_id]")))?;
        let configured = inference_id(f);
        if id != configured {
            return Err(located(format!(
                "The configured inference_id [{id}] for field [{path}] doesn't match the inference_id [{configured}] reported in the document."
            )));
        }
        let settings = match inference.get("model_settings").filter(|v| !v.is_null()) {
            Some(v) => Some(ModelSettings::parse(v).map_err(wrapped)?),
            None => None,
        };
        match (&settings, model_settings(f)) {
            (Some(s), Some(m)) if !s.compatible(&m) => {
                return Err(located(format!("Incompatible model settings for field [{path}]. Check that the inference_id is not using different model settings"))
                    .caused_by(iae(format!("Cannot update parameter [model_settings] for field [{path}]"))));
            }
            (Some(s), None) => updates.push((path.clone(), s.to_value())),
            _ => {}
        }
        let chunks = inference.get("chunks").ok_or_else(|| wrapped(iae("Required [chunks]")))?;
        let Value::Obj(by_source) = chunks else { return Err(wrapped(iae("[chunks] must be an object of source fields"))) };
        let name: std::sync::Arc<str> = embeddings_path(&path).into();
        for list in by_source.values() {
            for chunk in list.as_array().map(Vec::as_slice).unwrap_or_default() {
                let (Some(start), Some(end)) = (chunk.get("start_offset").and_then(Value::as_i64), chunk.get("end_offset").and_then(Value::as_i64)) else {
                    return Err(wrapped(iae("Missing chunk text")));
                };
                if start < 0 || end < 0 {
                    return Err(wrapped(iae(format!("Illegal offsets, expected positive numbers, got: {start}:{end}"))));
                }
                if start > end {
                    return Err(wrapped(iae(format!("Illegal offsets, expected start < end, got: {start} > {end}"))));
                }
                let Some(settings) = settings.as_ref() else { return Err(wrapped(iae("Required [model_settings] for chunks with embeddings"))) };
                let emb = chunk.get("embeddings").ok_or_else(|| wrapped(iae("Required [embeddings]")))?;
                let value = match (settings.task, emb) {
                    (TaskType::SparseEmbedding, Value::Obj(m)) => FieldValue::Features(m.iter().map(|(k, w)| (k.clone(), w.as_f64().unwrap_or(0.0) as f32)).collect()),
                    (TaskType::SparseEmbedding, _) => return Err(wrapped(iae("Required [embeddings] as an object of weighted tokens"))),
                    (_, Value::Arr(a)) => {
                        let floats: Vec<f32> = a.iter().map(|x| x.as_f64().unwrap_or(0.0) as f32).collect();
                        let dims = settings.dimensions.unwrap_or(0) as usize;
                        let expected = if settings.element_type.as_deref() == Some("bit") { dims / 8 } else { dims };
                        if floats.len() != expected {
                            return Err(located(format!("Incompatible model settings for field [{path}]. Check that the inference_id is not using different model settings"))
                                .caused_by(iae(format!("The number of dimensions [{}] doesn't match [{dims}]", floats.len()))));
                        }
                        match settings.element_type.as_deref() {
                            Some("byte") | Some("bit") => FieldValue::ByteVector(floats.iter().map(|f| *f as i8).collect()),
                            _ => FieldValue::DenseVector(floats),
                        }
                    }
                    (_, _) => return Err(wrapped(iae("Required [embeddings] as an array of numbers"))),
                };
                out.nested.push(NestedDoc { path: chunks_path(&path), fields: vec![DocField { name: name.clone(), value }] });
            }
        }
    }
    Ok(updates)
}

/// The values of a `semantic_text` field in a document, as the texts to embed: strings as
/// they are, numbers and booleans as their text, arrays value by value (`null`s skipped).
fn texts_at(doc: &Map, path: &str) -> Option<Vec<String>> {
    fn scalar(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Null | Value::Obj(_) => {}
            Value::Arr(a) => a.iter().for_each(|x| scalar(x, out)),
            other => out.push(other.to_es_string()),
        }
    }
    if let Some(v) = doc.get(path) {
        let mut out = Vec::new();
        scalar(v, &mut out);
        return Some(out);
    }
    for (i, _) in path.match_indices('.') {
        if let Some(Value::Obj(child)) = doc.get(&path[..i])
            && let Some(t) = texts_at(child, &path[i + 1..])
        {
            return Some(t);
        }
    }
    None
}

/// One field of one document waiting for embeddings.
struct Need {
    doc: usize,
    path: String,
    inference_id: String,
    values: Vec<String>,
    chunking: Option<super::chunking::Chunking>,
}

/// One chunk to embed: which need, which value, its span in the value and the value's offset in
/// the field's values joined by one separator.
struct Input {
    need: usize,
    span: super::chunking::Span,
    adjust: usize,
    text: String,
}

/// Inputs per call: what Elasticsearch's request chunker batches per service.
fn batch_size(service: &str) -> usize {
    match service {
        "openai" => 2048,
        "cohere" => 96,
        _ => 512,
    }
}

/// `inference_exception` for a field whose endpoint failed (`ShardBulkInferenceActionFilter`),
/// with the cause's status.
fn field_failure(id: &str, field: &str, cause: EsError) -> EsError {
    EsError::new(cause.status, "inference_exception", format!("Exception when running inference id [{id}] on field [{field}]")).caused_by(cause)
}

fn embedding_value(r: &super::results::Results, i: usize) -> Option<Value> {
    use super::results::Results;
    Some(match r {
        Results::Floats(v) => Value::Arr(v.get(i)?.iter().map(|f| crate::query::xparser::float_value(*f)).collect()),
        Results::Bytes(v) | Results::Bits(v) => Value::Arr(v.get(i)?.iter().map(|b| Value::from(i64::from(*b))).collect()),
        Results::Sparse(v) => super::results::weights(&v.get(i)?.tokens),
        _ => return None,
    })
}

/// Embeds the `semantic_text` values of documents that carry no results for them (the work of
/// Elasticsearch's `ShardBulkInferenceActionFilter`): values are chunked (the field's chunking
/// settings, else the endpoint's), and every endpoint is called once per batch of chunks,
/// whichever documents they come from. Each document gets the `_inference_fields` entries to
/// add (field path → entry), or the failure of the first field whose endpoint failed.
///
/// Runs with the cluster's authority: a writer needs only its index privileges, as on
/// Elasticsearch, whose filter calls the inference services directly.
pub async fn infer_documents(cluster: &crate::cluster::Cluster, docs: &[(&Mapping, &Map)]) -> Vec<EsResult<Map>> {
    let st = cluster.state();
    let mut out: Vec<EsResult<Map>> = docs.iter().map(|_| Ok(Map::new())).collect();
    let mut needs: Vec<Need> = Vec::new();
    for (d, (mapping, doc)) in docs.iter().enumerate() {
        let present: Vec<String> = inference_entries(doc).into_iter().map(|(f, _, _)| f).collect();
        for (path, f) in fields(mapping) {
            if present.contains(&path) {
                continue;
            }
            let Some(values) = texts_at(doc, &path) else { continue };
            if values.is_empty() {
                continue;
            }
            let chunking = f.raw("chunking_settings").and_then(super::chunking::Chunking::from_stored);
            needs.push(Need { doc: d, path, inference_id: inference_id(f), values, chunking });
        }
    }
    if needs.is_empty() {
        return out;
    }
    // Inputs per endpoint, in document order.
    let mut by_endpoint: Vec<(String, Vec<Input>)> = Vec::new();
    let mut endpoints: std::collections::HashMap<String, super::Endpoint> = std::collections::HashMap::new();
    for (n, need) in needs.iter().enumerate() {
        if out[need.doc].is_err() {
            continue;
        }
        let ep = match endpoints.get(&need.inference_id).cloned().or_else(|| super::endpoint(cluster, &st, &need.inference_id)) {
            Some(ep) => ep,
            None => {
                out[need.doc] = Err(EsError::resource_not_found(format!("Inference id [{}] not found for field [{}]", need.inference_id, need.path)));
                continue;
            }
        };
        let chunking = need.chunking.clone().or_else(|| ep.chunking.clone());
        endpoints.insert(need.inference_id.clone(), ep);
        let inputs = match by_endpoint.iter_mut().find(|(id, _)| *id == need.inference_id) {
            Some((_, v)) => v,
            None => {
                by_endpoint.push((need.inference_id.clone(), Vec::new()));
                &mut by_endpoint.last_mut().expect("just pushed").1
            }
        };
        let mut adjust = 0;
        for v in &need.values {
            let len = v.encode_utf16().count();
            if !v.trim().is_empty() {
                let spans = match &chunking {
                    Some(c) => c.chunk(v),
                    None => vec![super::chunking::Span { start: 0, end: len }],
                };
                for span in spans {
                    inputs.push(Input { need: n, span, adjust, text: super::chunking::substring(v, span) });
                }
            }
            adjust += len + 1;
        }
    }
    // Chunk embeddings per need, in input order.
    let mut chunks: Vec<Vec<Value>> = needs.iter().map(|_| Vec::new()).collect();
    for (id, inputs) in &by_endpoint {
        let ep = &endpoints[id];
        let Some(service) = super::services::Service::find(cluster, &ep.service) else { continue };
        for batch in inputs.chunks(batch_size(&ep.service)) {
            // Documents already failed by another field are left out of the call.
            let live: Vec<&Input> = batch.iter().filter(|i| out[needs[i.need].doc].is_ok()).collect();
            if live.is_empty() {
                continue;
            }
            let mut req = super::Request::new(live.iter().map(|i| i.text.clone()).collect(), std::time::Duration::from_secs(30));
            req.input_type = Some("internal_ingest".into());
            match service.infer(cluster, ep, &req).await {
                Ok(results) if results.len() == live.len() => {
                    for (k, input) in live.iter().enumerate() {
                        let Some(emb) = embedding_value(&results, k) else {
                            let need = &needs[input.need];
                            out[need.doc] = Err(field_failure(id, &need.path, EsError::status(400, format!("Inference id [{id}] doesn't produce embeddings ({})", ep.task.name()))));
                            continue;
                        };
                        let mut c = Map::new();
                        c.insert("start_offset".into(), ((input.span.start + input.adjust) as i64).into());
                        c.insert("end_offset".into(), ((input.span.end + input.adjust) as i64).into());
                        c.insert("embeddings".into(), emb);
                        chunks[input.need].push(Value::Obj(c));
                    }
                }
                Ok(results) => {
                    let e = EsError::status(500, format!("Inference id [{id}] returned [{}] results for [{}] inputs", results.len(), live.len()));
                    for input in &live {
                        let need = &needs[input.need];
                        if out[need.doc].is_ok() {
                            out[need.doc] = Err(field_failure(id, &need.path, e.clone()));
                        }
                    }
                }
                Err(e) => {
                    for input in &live {
                        let need = &needs[input.need];
                        if out[need.doc].is_ok() {
                            out[need.doc] = Err(field_failure(id, &need.path, e.clone()));
                        }
                    }
                }
            }
        }
    }
    for (n, need) in needs.iter().enumerate() {
        let Ok(entries) = &mut out[need.doc] else { continue };
        let Some(ep) = endpoints.get(&need.inference_id) else { continue };
        let mut inference = Map::new();
        inference.insert("inference_id".into(), need.inference_id.clone().into());
        inference.insert("model_settings".into(), ModelSettings::of_endpoint(ep).map_or(Value::Null, |m| m.to_value()));
        if let Some(c) = &need.chunking {
            inference.insert("chunking_settings".into(), c.to_value());
        }
        let mut by_source = Map::new();
        by_source.insert(need.path.clone(), Value::Arr(std::mem::take(&mut chunks[n])));
        inference.insert("chunks".into(), Value::Obj(by_source));
        entries.insert(need.path.clone(), crate::obj! { "inference" => Value::Obj(inference) });
    }
    out
}

/// A document's source with inference entries added under `_inference_fields`: appended to the
/// original bytes when the document has none yet (so the client's bytes stay as sent), merged
/// into the parsed document otherwise.
pub fn with_inference(source: &[u8], doc: &Map, entries: &Map) -> (bytes::Bytes, Value) {
    let mut doc = doc.clone();
    if !doc.keys().any(|k| is_inference_key(k))
        && let Some(end) = source.iter().rposition(|b| *b == b'}')
    {
        let mut raw = source[..end].to_vec();
        let empty = doc.is_empty();
        let addition = Value::Obj(entries.clone());
        raw.extend_from_slice(if empty { b"\"_inference_fields\":" } else { b",\"_inference_fields\":" });
        raw.extend_from_slice(addition.to_json().as_bytes());
        raw.extend_from_slice(&source[end..]);
        doc.insert("_inference_fields".into(), addition);
        return (bytes::Bytes::from(raw), Value::Obj(doc));
    }
    let mut merged = match doc.shift_remove("_inference_fields") {
        Some(Value::Obj(m)) => m,
        _ => Map::new(),
    };
    for (k, v) in entries {
        merged.insert(k.clone(), v.clone());
    }
    doc.insert("_inference_fields".into(), Value::Obj(merged));
    let v = Value::Obj(doc);
    (bytes::Bytes::from(v.to_json()), v)
}

/// An updated document's source without the inference results of the `semantic_text` fields
/// whose text the update changed (`None`: nothing to drop).
pub fn drop_stale(mapping: &Mapping, old: &[u8], new: &[u8]) -> Option<bytes::Bytes> {
    let semantic = fields(mapping);
    if semantic.is_empty() {
        return None;
    }
    let (Ok(Some(Value::Obj(old))), Ok(Some(Value::Obj(mut new)))) = (crate::json::parse(old), crate::json::parse(new)) else { return None };
    let stale: Vec<String> = semantic.into_iter().map(|(p, _)| p).filter(|p| texts_at(&old, p) != texts_at(&new, p)).collect();
    let mut changed = false;
    for p in &stale {
        if new.shift_remove(&format!("_inference_fields.{p}")).is_some() {
            changed = true;
        }
        if let Some(Value::Obj(m)) = new.get_mut("_inference_fields")
            && m.shift_remove(p).is_some()
        {
            changed = true;
        }
    }
    if let Some(Value::Obj(m)) = new.get("_inference_fields")
        && m.is_empty()
    {
        new.shift_remove("_inference_fields");
    }
    changed.then(|| bytes::Bytes::from(Value::Obj(new).to_json()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obj;

    #[test]
    fn model_settings_follow_minimal_service_settings() {
        let dense = obj! { "task_type" => "text_embedding", "dimensions" => 4, "similarity" => "cosine", "element_type" => "float" };
        assert_eq!(ModelSettings::parse(&dense).unwrap().to_value(), dense);
        let e = ModelSettings::parse(&obj! { "task_type" => "text_embedding", "dimensions" => 4 }).unwrap_err();
        assert_eq!(e.reason(), "required [similarity] field is missing for task_type [TEXT_EMBEDDING]");
        let e = ModelSettings::parse(&obj! { "task_type" => "sparse_embedding", "dimensions" => 4 }).unwrap_err();
        assert_eq!(e.reason(), "[dimensions] is not allowed for task_type [SPARSE_EMBEDDING]");
    }
}
