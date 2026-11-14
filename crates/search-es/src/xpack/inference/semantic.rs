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

    /// The settings of an endpoint's embeddings (`new MinimalServiceSettings(model)`).
    pub fn of_endpoint(ep: &super::Endpoint) -> Option<ModelSettings> {
        let ss = &ep.service_settings;
        match ep.task {
            TaskType::SparseEmbedding => Some(ModelSettings { task: ep.task, dimensions: None, similarity: None, element_type: None }),
            TaskType::TextEmbedding | TaskType::Embedding => Some(ModelSettings {
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
