//! The services of Elasticsearch's `inference-service-test` plugin
//! (`x-pack/plugin/inference/qa/test-service-plugin`), which the clusters of its YAML suites
//! load: deterministic fake models, reproduced exactly (Java's `String.hashCode`, float
//! arithmetic and orderings), so the suites' expected embeddings and rankings come out the same.
//!
//! They exist only on nodes started as YAML test clusters ([`super::TEST_SERVICES`]); a real
//! cluster has no service of these names.

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::xpack::inference::results::{Ranked, Results, Sparse};
use crate::xpack::inference::settings::validation;
use crate::xpack::inference::{Endpoint, Request, TaskType, unsupported_task};

/// One of the plugin's services.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `test_service`: sparse embeddings.
    Sparse,
    /// `alternate_sparse_embedding_test_service`: the same, under another name.
    AlternateSparse,
    /// `text_embedding_test_service`: dense embeddings.
    Dense,
    /// `test_reranking_service`.
    Rerank,
    /// `completion_test_service`.
    Completion,
    /// `streaming_completion_test_service`.
    Streaming,
}

const KINDS: [Kind; 6] = [Kind::Sparse, Kind::AlternateSparse, Kind::Dense, Kind::Rerank, Kind::Completion, Kind::Streaming];

/// `TestDenseInferenceServiceExtension.DEFAULT_EMBEDDING_DIMENSIONS`.
const DEFAULT_DIMENSIONS: i64 = 128;
/// `TestRerankingServiceExtension.RERANK_WINDOW_SIZE`.
pub const RERANK_WINDOW_SIZE: usize = 333;

fn intentional_failure() -> EsError {
    EsError::java(500, "runtime_exception", "validation call intentionally failed based on task settings")
}

/// Java's `String.hashCode` (over UTF-16 code units).
pub fn java_hash(s: &str) -> i32 {
    s.encode_utf16().fold(0i32, |h, c| h.wrapping_mul(31).wrapping_add(i32::from(c)))
}

fn str_of(m: &Map, k: &str) -> Option<String> {
    m.get(k).and_then(Value::as_str).map(str::to_owned)
}

impl Kind {
    pub fn find(name: &str) -> Option<Kind> {
        if name == "streaming_completion_test_service_alias" {
            return Some(Kind::Streaming);
        }
        KINDS.into_iter().find(|k| k.name() == name)
    }

    pub fn name(self) -> &'static str {
        match self {
            Kind::Sparse => "test_service",
            Kind::AlternateSparse => "alternate_sparse_embedding_test_service",
            Kind::Dense => "text_embedding_test_service",
            Kind::Rerank => "test_reranking_service",
            Kind::Completion => "completion_test_service",
            Kind::Streaming => "streaming_completion_test_service",
        }
    }

    pub fn task_types(self) -> &'static [TaskType] {
        match self {
            Kind::Sparse | Kind::AlternateSparse => &[TaskType::SparseEmbedding],
            Kind::Dense => &[TaskType::TextEmbedding, TaskType::Embedding],
            Kind::Rerank => &[TaskType::Rerank],
            Kind::Completion => &[TaskType::Completion],
            Kind::Streaming => &[TaskType::Completion, TaskType::ChatCompletion, TaskType::SparseEmbedding],
        }
    }

    /// `parseRequestConfig`: the service settings (`model` or `model_id` required), the
    /// secret's `api_key` from them, task settings; everything else is ignored, chunking
    /// settings included (the plugin's models have none).
    pub fn parse(self, cluster: &Cluster, id: &str, task: TaskType, mut config: Map) -> EsResult<Endpoint> {
        let mut ss = config.shift_remove("service_settings").and_then(|v| v.as_object().cloned()).unwrap_or_default();
        let ts = config.shift_remove("task_settings").and_then(|v| v.as_object().cloned()).unwrap_or_default();
        let service_settings = self.service_settings(&mut ss)?;
        let mut secrets = Map::new();
        match ss.shift_remove("api_key") {
            Some(Value::Str(k)) => {
                secrets.insert("api_key".into(), crate::seal::seal_given(cluster, crate::xpack::inference::SECRET_KIND, &k).into());
            }
            _ => return Err(validation(&["missing api_key".into()])),
        }
        Ok(Endpoint {
            id: id.to_string(),
            task,
            service: self.name().to_string(),
            service_settings,
            task_settings: self.task_settings(&ts, None),
            chunking: None,
            secrets,
        })
    }

    fn model(ss: &mut Map, missing: &str) -> EsResult<String> {
        match ss.shift_remove("model_id").or_else(|| ss.shift_remove("model")) {
            Some(Value::Str(m)) => Ok(m),
            _ => Err(validation(&[missing.into()])),
        }
    }

    fn service_settings(self, ss: &mut Map) -> EsResult<Map> {
        let mut out = Map::new();
        match self {
            Kind::Sparse | Kind::AlternateSparse | Kind::Completion => {
                out.insert("model".into(), Kind::model(ss, "missing model")?.into());
                if let Some(h) = ss.shift_remove("hidden_field") {
                    out.insert("hidden_field".into(), h);
                }
                let ret = ss.shift_remove("should_return_hidden_field").and_then(|v| v.as_bool()).unwrap_or(false);
                out.insert("should_return_hidden_field".into(), ret.into());
            }
            Kind::Dense => {
                out.insert("model".into(), Kind::model(ss, "missing model")?.into());
                for k in ["dimensions", "similarity", "element_type"] {
                    if let Some(v) = ss.shift_remove(k) {
                        let v = if k == "dimensions" { v } else { Value::from(v.to_es_string().to_ascii_lowercase()) };
                        out.insert(k.into(), v);
                    }
                }
            }
            Kind::Rerank => {
                out.insert("model_id".into(), Kind::model(ss, "missing model")?.into());
            }
            Kind::Streaming => {
                out.insert("model".into(), Kind::model(ss, "missing model id")?.into());
            }
        }
        Ok(out)
    }

    /// Task settings in their stored form; `base`: the current ones an update merges into.
    fn task_settings(self, given: &Map, base: Option<&Map>) -> Map {
        let mut out = Map::new();
        if self == Kind::Rerank {
            let pick = |k: &str| given.get(k).cloned().or_else(|| base.and_then(|b| b.get(k).cloned()));
            let boolean = |k: &str| pick(k).map(|v| v.to_es_string().eq_ignore_ascii_case("true")).unwrap_or(false);
            let float = |k: &str, d: f32| pick(k).and_then(|v| v.to_es_string().parse::<f32>().ok()).unwrap_or(d);
            out.insert("should_fail_validation".into(), boolean("should_fail_validation").into());
            out.insert("use_text_length".into(), boolean("use_text_length").into());
            // An unset minimum score is random, as the plugin's is.
            let min = float("min_score", rand::random::<f32>() * 2.0 - 1.0);
            out.insert("min_score".into(), crate::query::xparser::float_value(min));
            out.insert("result_diff".into(), crate::query::xparser::float_value(float("result_diff", 0.2)));
            if let Some(n) = pick("top_n").and_then(|v| v.to_es_string().parse::<i64>().ok()) {
                out.insert("top_n".into(), n.into());
            }
            out.insert("hide_top_n".into(), boolean("hide_top_n").into());
            return out;
        }
        let mut src = base.cloned().unwrap_or_default();
        for (k, v) in given {
            src.insert(k.clone(), v.clone());
        }
        if let Some(t) = src.get("temperature") {
            out.insert("temperature".into(), t.clone());
        }
        if let Some(f) = src.get("should_fail_validation") {
            out.insert("should_fail_validation".into(), f.clone());
        }
        out
    }

    pub fn update(self, ep: &mut Endpoint, mut ss: Map, ts: Map) -> EsResult<()> {
        if let Some(Value::Str(k)) = ss.shift_remove("api_key") {
            ep.secrets.insert("api_key".into(), k.into());
        }
        ep.task_settings = self.task_settings(&ts, Some(&ep.task_settings));
        Ok(())
    }

    /// `getFilteredXContentObject`.
    pub fn exposed(self, ep: &Endpoint) -> Map {
        let mut m = ep.service_settings.clone();
        if matches!(self, Kind::Sparse | Kind::AlternateSparse | Kind::Completion) {
            let show = m.shift_remove("should_return_hidden_field").and_then(|v| v.as_bool()).unwrap_or(false);
            if !show {
                m.shift_remove("hidden_field");
            }
        }
        m
    }

    /// `updateModelWithEmbeddingDetails`: the dense service records the size it returned.
    pub fn embedding_details(self, ep: &mut Endpoint, size: usize) {
        if self == Kind::Dense {
            let ss = &mut ep.service_settings;
            match ss.get_index_of("dimensions") {
                Some(_) => {
                    ss.insert("dimensions".into(), (size as i64).into());
                }
                None => {
                    ss.shift_insert(1, "dimensions".into(), (size as i64).into());
                }
            }
        }
    }

    fn fails(ep: &Endpoint) -> bool {
        ep.task_settings.get("should_fail_validation").and_then(Value::as_bool) == Some(true)
    }

    pub fn infer(self, ep: &Endpoint, req: &Request) -> EsResult<Results> {
        match self {
            Kind::Sparse | Kind::AlternateSparse => {
                if Kind::fails(ep) {
                    return Err(intentional_failure());
                }
                if ep.task != TaskType::SparseEmbedding {
                    return Err(unsupported_task(ep.task, self.name()));
                }
                Ok(Results::Sparse(req.input.iter().map(|s| sparse(s)).collect()))
            }
            Kind::Dense => {
                if Kind::fails(ep) {
                    return Err(intentional_failure());
                }
                if ep.task != TaskType::TextEmbedding {
                    return Err(unsupported_task(ep.task, self.name()));
                }
                Ok(dense_results(ep, &req.input))
            }
            Kind::Rerank => {
                if ep.task_settings.get("should_fail_validation").and_then(Value::as_bool) == Some(true) {
                    return Err(intentional_failure());
                }
                let ts = self.task_settings(&req.task_settings, Some(&ep.task_settings));
                Ok(Results::Rerank(rerank(&req.input, &ts)))
            }
            Kind::Completion => {
                if Kind::fails(ep) {
                    return Err(intentional_failure());
                }
                if ep.task != TaskType::Completion {
                    return Err(unsupported_task(ep.task, self.name()));
                }
                let temperature = req.task_settings.get("temperature").and_then(|t| t.as_f64()).map(|t| t as i64);
                Ok(Results::Completion(
                    req.input
                        .iter()
                        .map(|s| {
                            let up = s.to_uppercase();
                            match temperature {
                                Some(t) => format!("temperature:{t} {up}"),
                                None => up,
                            }
                        })
                        .collect(),
                ))
            }
            Kind::Streaming => {
                if Kind::fails(ep) {
                    return Err(intentional_failure());
                }
                match ep.task {
                    TaskType::Completion => Ok(Results::Completion(req.input.iter().map(|s| s.to_uppercase()).collect())),
                    // Creating a sparse endpoint validates with a dense answer, so the suites can
                    // check that streaming one fails.
                    TaskType::SparseEmbedding => Ok(Results::Floats(req.input.iter().map(|_| (0..5).map(|_| rand::random::<f32>()).collect()).collect())),
                    other => Err(unsupported_task(other, self.name())),
                }
            }
        }
    }

    pub fn stream_completion(self, ep: &Endpoint, req: &Request) -> EsResult<Vec<Value>> {
        if self != Kind::Streaming || ep.task != TaskType::Completion {
            return Err(unsupported_task(ep.task, self.name()));
        }
        Ok(req.input.iter().map(|s| obj! { "completion" => crate::arr![obj! { "delta" => s.to_uppercase() }] }).collect())
    }

    pub fn stream_chat(self, ep: &Endpoint, body: &Value) -> EsResult<Vec<Value>> {
        if self != Kind::Streaming || ep.task != TaskType::ChatCompletion {
            return Err(unsupported_task(ep.task, self.name()));
        }
        let req = super::openai::unified_request(body)?;
        Ok(req
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|m| {
                let content = m.get("content").map(Value::to_es_string).unwrap_or_default().to_uppercase();
                obj! {
                    "id" => "id",
                    "choices" => crate::arr![obj! { "delta" => obj! { "content" => content }, "index" => 0 }],
                    "model" => "gpt-4o-2024-08-06",
                    "object" => "chat.completion.chunk",
                }
            })
            .collect())
    }
}

/// `AbstractSparseTestInferenceService.generateEmbedding` for features 0..5.
pub fn sparse(input: &str) -> Sparse {
    let h = java_hash(input);
    let abs = if h == i32::MIN { i32::MAX } else { h.abs() };
    Sparse { tokens: (0..5).map(|j| (format!("feature_{j}"), abs as f32 + 1.0f32 + j as f32)).collect(), truncated: false }
}

/// The dense service's element type and similarity (`float` and `cosine` unless set).
fn dense_options(ep: &Endpoint) -> (String, String, i64) {
    let ss = &ep.service_settings;
    (
        str_of(ss, "element_type").unwrap_or_else(|| "float".into()),
        str_of(ss, "similarity").unwrap_or_else(|| "cosine".into()),
        ss.get("dimensions").and_then(Value::as_i64).unwrap_or(DEFAULT_DIMENSIONS),
    )
}

/// `TestDenseInferenceServiceExtension.generateEmbedding`: the decimal digits of the input's
/// hash code, as floats, repeated to the length; a unit vector for `dot_product`.
pub fn dense(input: &str, dims: i64, element: &str, similarity: &str) -> Vec<f32> {
    let len = if element == "bit" { (dims / 8) as usize } else { dims as usize };
    let digits: Vec<f32> = java_hash(input).to_string().bytes().map(|b| b as f32).collect();
    let mut e: Vec<f32> = (0..len).map(|i| digits[i % digits.len()]).collect();
    if similarity == "dot_product" {
        let magnitude = e.iter().fold(0f32, |a, b| a + b * b).sqrt();
        e = e.into_iter().map(|v| v / magnitude).collect();
    }
    e
}

pub fn dense_results(ep: &Endpoint, input: &[String]) -> Results {
    let (element, similarity, dims) = dense_options(ep);
    let floats: Vec<Vec<f32>> = input.iter().map(|s| dense(s, dims, &element, &similarity)).collect();
    // Java's `(byte) f`: truncate to an int, keep its low byte.
    let to_bytes = |v: &Vec<f32>| v.iter().map(|f| *f as i32 as i8).collect::<Vec<i8>>();
    match element.as_str() {
        "byte" => Results::Bytes(floats.iter().map(to_bytes).collect()),
        "bit" => Results::Bits(floats.iter().map(to_bytes).collect()),
        _ => Results::Floats(floats),
    }
}

/// `TestRerankingServiceExtension.makeResults`: inputs that all parse as floats are their own
/// scores; otherwise scores fall by `result_diff` from `min_score` (or are `1/len` with
/// `use_text_length`), sorted by score, cut to `top_n`.
fn rerank(input: &[String], ts: &Map) -> Vec<Ranked> {
    let use_len = ts.get("use_text_length").and_then(Value::as_bool) == Some(true);
    let top_n = if ts.get("hide_top_n").and_then(Value::as_bool) == Some(true) { None } else { ts.get("top_n").and_then(Value::as_i64) };
    let as_scores: Option<Vec<f32>> = if use_len { None } else { input.iter().map(|s| java_parse_float(s)).collect() };
    let mut out: Vec<Ranked> = match as_scores {
        Some(scores) => scores.into_iter().enumerate().map(|(i, s)| Ranked { index: i, score: s, text: Some(input[i].clone()) }).collect(),
        None => {
            let n = input.len();
            let min = ts.get("min_score").and_then(Value::as_f64).unwrap_or(0.0) as f32;
            let diff = ts.get("result_diff").and_then(Value::as_f64).unwrap_or(0.2) as f32;
            (0..n)
                .map(|i| {
                    let text = &input[n - 1 - i];
                    let score = if use_len { 1f32 / text.encode_utf16().count() as f32 } else { min + diff * (n - i) as f32 };
                    Ranked { index: n - 1 - i, score, text: Some(text.clone()) }
                })
                .collect()
        }
    };
    // `RankedDoc.compareTo`: by score, highest first (a stable sort, as `List.sort` is).
    out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    if let Some(n) = top_n
        && n >= 0
        && (n as usize) < out.len()
    {
        out.truncate(n as usize);
    }
    out
}

/// `Float.parseFloat`: surrounding whitespace allowed, an optional `f`/`d` suffix, and only
/// Java's spellings of the special values.
fn java_parse_float(s: &str) -> Option<f32> {
    let t = s.trim_matches(|c: char| c <= ' ');
    let t = t.strip_suffix(['f', 'F', 'd', 'D']).unwrap_or(t);
    let word = t.trim_start_matches(['+', '-']);
    if word.chars().any(|c| c.is_alphabetic() && c != 'e' && c != 'E') && word != "Infinity" && word != "NaN" {
        return None;
    }
    t.parse::<f32>().ok()
}

/// The plugin's `_services` configurations.
pub fn configurations() -> Vec<Value> {
    let field = |label: &str, sensitive: bool, task: &str| {
        obj! {
            "description" => "", "label" => label, "required" => true, "sensitive" => sensitive, "updatable" => false, "type" => "str",
            "supported_task_types" => crate::arr![task],
        }
    };
    let service = |name: &str, tasks: Value, configs: Value| obj! { "service" => name, "name" => name, "task_types" => tasks, "configurations" => configs };
    vec![
        service("alternate_sparse_embedding_test_service", crate::arr!["sparse_embedding"], obj! {
            "hidden_field" => field("Hidden Field", false, "sparse_embedding"), "model" => field("Model", false, "sparse_embedding"),
        }),
        service("completion_test_service", crate::arr!["completion"], obj! { "model_id" => field("Model ID", true, "completion") }),
        service("streaming_completion_test_service", crate::arr!["completion", "chat_completion", "sparse_embedding"], obj! {
            "model_id" => field("Model ID", true, "completion"),
        }),
        service("test_reranking_service", crate::arr!["rerank"], obj! { "model" => field("Model", true, "rerank") }),
        service("test_service", crate::arr!["sparse_embedding"], obj! {
            "hidden_field" => field("Hidden Field", false, "sparse_embedding"), "model" => field("Model", false, "sparse_embedding"),
        }),
        service("text_embedding_test_service", crate::arr!["text_embedding", "embedding"], obj! { "model" => field("Model", true, "text_embedding") }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_hash_and_fake_embeddings() {
        assert_eq!(java_hash(""), 0);
        assert_eq!(java_hash("hello"), 99162322);
        assert_eq!(java_hash("polygenelubricants"), i32::MIN);
        let s = sparse("hello");
        assert_eq!(s.tokens[0], ("feature_0".to_string(), 99162323.0));
        // The digits of -2108123016 ("-" is 45, "2" is 50, …), repeated.
        let d = dense("these are not the droids you're looking for", 4, "float", "cosine");
        assert_eq!(d.len(), 4);
        assert!(d.iter().all(|v| (45.0..=57.0).contains(v)));
        let unit = dense("x", 3, "float", "dot_product");
        assert!((unit.iter().map(|v| v * v).sum::<f32>() - 1.0).abs() < 1e-5);
        assert_eq!(dense("x", 16, "bit", "cosine").len(), 2);
    }

    #[test]
    fn rerank_orders() {
        let Value::Obj(ts) = obj!("min_score" => 0.0, "result_diff" => 0.2) else { unreachable!() };
        let r = rerank(&["0.1".into(), "0.9".into()], &ts);
        assert_eq!(r.iter().map(|d| d.index).collect::<Vec<_>>(), vec![1, 0]);
        let r = rerank(&["a".into(), "b".into(), "c".into()], &ts);
        assert_eq!(r.iter().map(|d| d.index).collect::<Vec<_>>(), vec![2, 1, 0]);
    }
}
