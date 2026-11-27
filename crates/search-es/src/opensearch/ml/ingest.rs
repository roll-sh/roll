//! The neural-search plugin's `text_embedding` ingest processor (`TextEmbeddingProcessor`):
//! the texts of `field_map`'s source fields embedded by `model_id` into its target fields (a
//! string gets a vector, a list of strings a list of `{"knn": vector}`), one model call per
//! document for all of them, as OpenSearch makes it.
//!
//! Ingest processors run synchronously; a model call doesn't. So the processor reads its texts
//! where it runs, and the vectors are written when its pipeline has finished ([`settle`]), before
//! the next pipeline or the write. A processor after it in the same pipeline doesn't see the
//! vectors, and a failed call fails the document (neither `ignore_failure` nor `on_failure`
//! catch it); docs/search/opensearch-neural.md lists both.

use crate::cluster::Cluster;
use crate::error::{EsError, EsResult};
use crate::ingest::doc::IngestDoc;
use crate::ingest::processors::config_error;
use crate::json::{Map, Value};

/// Texts a `text_embedding` processor read, waiting for their vectors.
#[derive(Clone, Debug)]
pub struct Pending {
    model_id: String,
    /// (target field, whether the source was a list, its texts).
    items: Vec<(String, bool, Vec<String>)>,
}

/// The processor's configuration: (model id, source → target fields).
pub fn compile(cfg: &Map, tag: Option<&str>) -> EsResult<(String, Vec<(String, String)>)> {
    const T: &str = "text_embedding";
    let model = cfg.get("model_id").and_then(Value::as_str).filter(|m| !m.is_empty()).ok_or_else(|| config_error(T, tag, "model_id", "required property is missing"))?;
    let Some(Value::Obj(map)) = cfg.get("field_map") else { return Err(config_error(T, tag, "field_map", "required property is missing")) };
    if map.is_empty() {
        return Err(config_error(T, tag, "field_map", "Unable to create the processor as field_map has invalid key or value"));
    }
    let mut out = Vec::new();
    for (source, target) in map {
        match target {
            Value::Str(t) if !t.is_empty() && !source.is_empty() => out.push((source.clone(), t.clone())),
            Value::Obj(_) => return Err(config_error(T, tag, "field_map", "roll's text_embedding takes flat field names (`a.b`), not nested field maps")),
            _ => return Err(config_error(T, tag, "field_map", "Unable to create the processor as field_map has invalid key or value")),
        }
    }
    Ok((model.to_string(), out))
}

/// Reads the processor's texts from `d` and leaves them for [`settle`].
pub fn collect(model_id: &str, field_map: &[(String, String)], d: &mut IngestDoc) -> EsResult<()> {
    let mut items = Vec::new();
    for (source, target) in field_map {
        let texts = match d.get_opt(source) {
            None | Some(Value::Null) => continue,
            Some(Value::Str(s)) => (false, vec![s.clone()]),
            Some(Value::Arr(a)) => {
                let texts: Option<Vec<String>> = a.iter().map(|v| v.as_str().map(str::to_owned)).collect();
                (true, texts.ok_or_else(|| EsError::illegal_argument(format!("list type field [{source}] has non string value")))?)
            }
            Some(_) => return Err(EsError::illegal_argument(format!("field [{source}] is neither string nor nested type, cannot process it"))),
        };
        if texts.1.iter().any(String::is_empty) {
            return Err(EsError::illegal_argument(format!("field [{source}] has empty string, cannot process it")));
        }
        items.push((target.clone(), texts.0, texts.1));
    }
    if !items.is_empty() {
        d.embeddings.push(Pending { model_id: model_id.to_string(), items });
    }
    Ok(())
}

/// Embeds what the pipeline's `text_embedding` processors read and writes the vectors to
/// their targets: one call per processor for all its texts.
pub async fn settle(cluster: &Cluster, d: &mut IngestDoc) -> EsResult<()> {
    // One document embeds at most `MAX_TEXTS` texts, over all its processors, before any call.
    let n: usize = d.embeddings.iter().flat_map(|p| p.items.iter()).map(|(_, _, t)| t.len()).sum();
    if n > super::MAX_TEXTS {
        d.embeddings.clear();
        return Err(EsError::illegal_argument(format!("[{n}] texts to embed in one document, more than roll's [{}]", super::MAX_TEXTS)));
    }
    for p in std::mem::take(&mut d.embeddings) {
        let texts: Vec<String> = p.items.iter().flat_map(|(_, _, t)| t.iter().cloned()).collect();
        let vectors = super::embed(cluster, &p.model_id, &texts, d.user.as_deref(), false).await?;
        let mut it = vectors.into_iter();
        for (target, list, t) in p.items {
            let vs: Vec<Value> = it.by_ref().take(t.len()).map(|v| Value::Arr(v.into_iter().map(super::float_value).collect())).collect();
            let value = if list { Value::Arr(vs.into_iter().map(|v| crate::obj! { "knn" => v }).collect()) } else { vs.into_iter().next().unwrap_or(Value::Null) };
            d.set(&target, value)?;
        }
    }
    Ok(())
}
