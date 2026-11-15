//! Logstash central management: `PUT/GET/DELETE _logstash/pipeline/<id>`.
//!
//! Elasticsearch keeps each pipeline as a document of the `.logstash` system index (strict
//! mapping) and returns it as stored; roll keeps the same object in the cluster metadata. A get
//! lists the pipelines matching the comma-separated ids and wildcards, answering `404 {}` when
//! none matches, as Elasticsearch does.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::rest::{Ctx, Reply};
use crate::xpack::store;

pub const PIPELINES: &str = "logstash.pipeline";

const REQUIRED: [&str; 5] = ["last_modified", "pipeline_metadata", "username", "pipeline", "pipeline_settings"];
const KNOWN: [&str; 6] = ["description", "last_modified", "pipeline_metadata", "username", "pipeline", "pipeline_settings"];

/// `^[a-zA-Z_][a-zA-Z0-9_\-]*$`, Logstash's pipeline id rule.
fn valid_id(id: &str) -> bool {
    let mut b = id.bytes();
    matches!(b.next(), Some(c) if c.is_ascii_alphabetic() || c == b'_') && b.all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

pub async fn put_pipeline(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    if !valid_id(&id) {
        return Err(EsError::illegal_argument(format!(
            "Invalid pipeline [{id}] ID received. Pipeline ID must begin with a letter or underscore and can contain only letters, underscores, dashes, hyphens, and numbers"
        )));
    }
    let body = ctx.body_map()?.unwrap_or_default();
    let missing: Vec<&str> = REQUIRED.iter().copied().filter(|k| !body.contains_key(*k)).collect();
    if !missing.is_empty() {
        return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
    }
    if let Some(Value::Str(s)) = body.get("last_modified")
        && crate::time::DateFormatter::new("strict_date_optional_time").expect("built-in").parse_millis(s).is_err()
    {
        let (line, col) = crate::xpack::key_loc(ctx, "last_modified");
        return Err(EsError::x_content_parse(line, col, "[pipeline] failed to parse field [last_modified]")
            .caused_by(EsError::java(400, "date_time_parse_exception", format!("Text '{s}' could not be parsed at index 0"))));
    }
    // The `.logstash` index maps the pipeline strictly.
    if let Some(k) = body.keys().find(|k| !KNOWN.contains(&k.as_str())) {
        let (line, col) = crate::xpack::value_loc(ctx, k);
        return Err(EsError::new(
            400,
            "strict_dynamic_mapping_exception",
            format!("[{line}:{col}] mapping set to strict, dynamic introduction of [{k}] within [_doc] is not allowed"),
        ));
    }
    let now = ctx.now_ms;
    let created = ctx.cluster.update(move |st| store::put(st, PIPELINES, &id, Value::Obj(body), now)).await?;
    Ok(Reply::Empty(if created { 201 } else { 200 }))
}

pub fn get_pipeline(ctx: &Ctx) -> EsResult<Reply> {
    let st = ctx.state();
    let ids = ctx.list_param("id").unwrap_or_default();
    let mut out = Map::new();
    for (id, p) in st.metadata.xpack.iter(PIPELINES) {
        if ids.is_empty() || ids.iter().any(|pat| crate::xcontent::filter::glob_match(pat, id)) {
            out.insert(id.clone(), p.clone());
        }
    }
    let status = if out.is_empty() && !ids.is_empty() { 404 } else { 200 };
    Ok(Reply::Body(status, Value::Obj(out)))
}

pub async fn delete_pipeline(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let found = ctx.cluster.update(move |st| Ok(store::remove(st, PIPELINES, &id).is_some())).await?;
    Ok(Reply::Empty(if found { 200 } else { 404 }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids() {
        assert!(valid_id("logstash_test_pipeline"));
        assert!(valid_id("_a-1"));
        assert!(!valid_id("1a"));
        assert!(!valid_id("Bad Id"));
        assert!(!valid_id(""));
    }
}
