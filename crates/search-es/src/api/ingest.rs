//! Ingest pipeline storage: `PUT/GET/DELETE _ingest/pipeline/{id}` and the grok pattern list.
//!
//! Pipelines are validated the way Elasticsearch's `Pipeline.create` and processor factories
//! validate them — `processors` must be a list of single-key objects naming a known processor,
//! each processor's required properties must be present and no unknown property may remain —
//! and then stored as given, with `created_date_millis`/`modified_date_millis` appended.
//! Execution comes with the document APIs.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use crate::xcontent::filter::glob_match;

/// Properties every processor accepts.
const COMMON: &[&str] = &["description", "if", "ignore_failure", "on_failure", "tag"];

/// `(processor, required properties, optional properties)`. A required entry `a|b` means one
/// of them (the first is reported when both are missing).
const PROCESSORS: &[(&str, &[&str], &[&str])] = &[
    ("append", &["field", "value|copy_from"], &["allow_duplicates", "media_type"]),
    ("attachment", &["field"], &["target_field", "indexed_chars", "indexed_chars_field", "properties", "ignore_missing", "remove_binary", "resource_name"]),
    ("bytes", &["field"], &["target_field", "ignore_missing"]),
    ("cef", &["field"], &["target_field", "ignore_missing", "ignore_empty_values", "timezone"]),
    ("circle", &["field", "error_distance", "shape_type"], &["target_field", "ignore_missing"]),
    ("community_id", &[], &["source_ip", "source_port", "destination_ip", "destination_port", "iana_number", "icmp_type", "icmp_code", "transport", "target_field", "seed", "ignore_missing"]),
    ("convert", &["field", "type"], &["target_field", "ignore_missing"]),
    ("csv", &["field", "target_fields"], &["separator", "quote", "trim", "empty_value", "ignore_missing"]),
    ("date", &["field", "formats"], &["target_field", "timezone", "locale", "output_format"]),
    ("date_index_name", &["field", "date_rounding"], &["index_name_prefix", "date_formats", "timezone", "locale", "index_name_format"]),
    ("dissect", &["field", "pattern"], &["append_separator", "ignore_missing"]),
    ("dot_expander", &["field"], &["path", "override"]),
    ("drop", &[], &[]),
    ("enrich", &["policy_name", "field", "target_field"], &["ignore_missing", "override", "max_matches", "shape_relation"]),
    ("fail", &["message"], &[]),
    ("fingerprint", &["fields"], &["target_field", "salt", "method", "ignore_missing"]),
    ("foreach", &["field", "processor"], &["ignore_missing"]),
    ("geo_grid", &["field", "tile_type"], &["target_field", "parent_field", "children_field", "non_children_field", "precision_field", "ignore_missing", "target_format"]),
    ("geoip", &["field"], &["target_field", "database_file", "properties", "ignore_missing", "first_only", "download_database_on_pipeline_creation"]),
    ("grok", &["field", "patterns"], &["pattern_definitions", "trace_match", "ignore_missing", "ecs_compatibility"]),
    ("gsub", &["field", "pattern", "replacement"], &["target_field", "ignore_missing"]),
    ("html_strip", &["field"], &["target_field", "ignore_missing"]),
    ("inference", &["model_id"], &["target_field", "field_map", "inference_config", "input_output", "ignore_missing"]),
    ("ip_location", &["field"], &["target_field", "database_file", "properties", "ignore_missing", "first_only", "download_database_on_pipeline_creation"]),
    ("join", &["field", "separator"], &["target_field"]),
    ("json", &["field"], &["target_field", "add_to_root", "add_to_root_conflict_strategy", "allow_duplicate_keys", "strict_json_parsing"]),
    ("kv", &["field", "field_split", "value_split"], &["target_field", "include_keys", "exclude_keys", "ignore_missing", "prefix", "trim_key", "trim_value", "strip_brackets"]),
    ("lowercase", &["field"], &["target_field", "ignore_missing"]),
    ("network_direction", &[], &["source_ip", "destination_ip", "target_field", "internal_networks", "internal_networks_field", "ignore_missing"]),
    ("normalize_for_stream", &[], &[]),
    ("pipeline", &["name"], &["ignore_missing_pipeline"]),
    ("recover_failure_document", &[], &[]),
    ("redact", &["field", "patterns"], &["pattern_definitions", "prefix", "suffix", "ignore_missing", "skip_if_unlicensed", "trace_redact"]),
    ("registered_domain", &["field"], &["target_field", "ignore_missing"]),
    ("remove", &["field|keep"], &["ignore_missing"]),
    ("rename", &["field", "target_field"], &["ignore_missing", "override"]),
    ("reroute", &[], &["destination", "dataset", "namespace"]),
    ("script", &[], &["lang", "id", "source", "params", "options"]),
    ("set", &["field", "value|copy_from"], &["override", "ignore_empty_value", "media_type"]),
    ("set_security_user", &["field"], &["properties"]),
    ("sort", &["field"], &["order", "target_field"]),
    ("split", &["field", "separator"], &["target_field", "ignore_missing", "preserve_trailing"]),
    ("terminate", &[], &[]),
    ("trim", &["field"], &["target_field", "ignore_missing"]),
    ("uppercase", &["field"], &["target_field", "ignore_missing"]),
    ("uri_parts", &["field"], &["target_field", "keep_original", "remove_if_successful", "ignore_missing"]),
    ("urldecode", &["field"], &["target_field", "ignore_missing"]),
    ("user_agent", &["field"], &["target_field", "regex_file", "properties", "extract_device_type", "ignore_missing"]),
];

fn parse_err(msg: impl Into<String>) -> EsError {
    EsError::new(400, "parse_exception", msg)
}

/// The Java class name Elasticsearch prints for a JSON value (`property isn't a list, but of
/// type [java.util.HashMap]`).
fn java_class(v: &Value) -> &'static str {
    match v {
        Value::Obj(_) => "java.util.HashMap",
        Value::Arr(_) => "java.util.ArrayList",
        Value::Str(_) => "java.lang.String",
        Value::Bool(_) => "java.lang.Boolean",
        Value::Num(n) if n.is_integer() => "java.lang.Integer",
        Value::Num(_) => "java.lang.Double",
        Value::Null => "null",
    }
}

fn validate_processor(ptype: &str, config: &Value) -> EsResult<()> {
    let Some((_, required, optional)) = PROCESSORS.iter().find(|(n, _, _)| *n == ptype) else {
        return Err(parse_err(format!("No processor type exists with name [{ptype}]")).with_meta("processor_type", ptype));
    };
    let Value::Obj(cfg) = config else {
        // `{"drop": null}` style configs are accepted by processors without properties.
        if config.is_null() && required.is_empty() {
            return Ok(());
        }
        return Err(parse_err(format!("[{ptype}] processor config isn't a map")).with_meta("processor_type", ptype));
    };
    let tag = cfg.get("tag").map(Value::to_es_string);
    let with_tag = |e: EsError| match &tag {
        Some(t) => e.with_meta("processor_tag", t.clone()),
        None => e,
    };
    for r in *required {
        let alts: Vec<&str> = r.split('|').collect();
        if !alts.iter().any(|a| cfg.contains_key(*a)) {
            return Err(with_tag(
                parse_err(format!("[{}] required property is missing", alts[0])).with_meta("property_name", alts[0]).with_meta("processor_type", ptype),
            ));
        }
    }
    if ptype == "script" && !cfg.contains_key("source") && !cfg.contains_key("id") {
        return Err(with_tag(EsError::new(400, "parse_exception", "must specify either [source] for an inline script or [id] for a stored script").with_meta("processor_type", ptype)));
    }
    if let Some(on_failure) = cfg.get("on_failure") {
        validate_processors(on_failure, "on_failure")?;
    }
    if ptype == "foreach"
        && let Some(Value::Obj(inner)) = cfg.get("processor")
        && let Some((n, c)) = inner.iter().next()
    {
        validate_processor(n, c)?;
    }
    let unknown: Vec<&String> = cfg
        .keys()
        .filter(|k| !COMMON.contains(&k.as_str()) && !required.iter().any(|r| r.split('|').any(|a| a == k.as_str())) && !optional.contains(&k.as_str()))
        .collect();
    if !unknown.is_empty() {
        let list: Vec<&str> = unknown.iter().map(|s| s.as_str()).collect();
        return Err(with_tag(
            parse_err(format!("processor [{ptype}] doesn't support one or more provided configuration parameters [{}]", list.join(", ")))
                .with_meta("processor_type", ptype),
        ));
    }
    Ok(())
}

fn validate_processors(v: &Value, property: &str) -> EsResult<()> {
    let Value::Arr(list) = v else {
        return Err(parse_err(format!("[{property}] property isn't a list, but of type [{}]", java_class(v))).with_meta("property_name", property));
    };
    for p in list {
        let Value::Obj(m) = p else {
            return Err(parse_err(format!("[{property}] property isn't a list, but of type [{}]", java_class(v))).with_meta("property_name", property));
        };
        for (ptype, config) in m {
            validate_processor(ptype, config)?;
        }
    }
    Ok(())
}

/// Validates a pipeline definition (`Pipeline.create`).
pub fn validate_pipeline(id: &str, body: &Map) -> EsResult<()> {
    let processors = body.get("processors").ok_or_else(|| parse_err("[processors] required property is missing").with_meta("property_name", "processors"))?;
    validate_processors(processors, "processors")?;
    if let Some(of) = body.get("on_failure") {
        validate_processors(of, "on_failure")?;
        if of.as_array().is_some_and(Vec::is_empty) {
            return Err(parse_err("pipeline [".to_string() + id + "] cannot have an empty on-failure option defined"));
        }
    }
    let unknown: Vec<&String> =
        body.keys().filter(|k| !matches!(k.as_str(), "description" | "processors" | "on_failure" | "version" | "_meta" | "deprecated" | "field_access_pattern")).collect();
    if !unknown.is_empty() {
        let list: Vec<&str> = unknown.iter().map(|s| s.as_str()).collect();
        return Err(parse_err(format!("pipeline [{id}] doesn't support one or more provided configuration parameters [{}]", list.join(", "))));
    }
    if let Some(v) = body.get("version")
        && !matches!(v, Value::Num(n) if n.is_integer())
        && !v.is_null()
    {
        return Err(parse_err(format!("[version] property isn't a number, but of type [{}]", java_class(v))).with_meta("property_name", "version"));
    }
    if let Some(m) = body.get("_meta")
        && !matches!(m, Value::Obj(_))
    {
        return Err(parse_err(format!("[_meta] property isn't a map, but of type [{}]", java_class(m))).with_meta("property_name", "_meta"));
    }
    Ok(())
}

pub async fn put(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let id = ctx.path_param("id").unwrap_or_default().to_string();
    let if_version = match ctx.query("if_version") {
        None => None,
        Some(v) => Some(
            crate::units::parse_java_int(v).ok_or_else(|| EsError::illegal_argument(format!("Failed to parse int parameter [if_version] with value [{v}]")))?,
        ),
    };
    let body = ctx.body_map()?.ok_or_else(|| EsError::action_request_validation(&["request body or source parameter is required".into()]))?;
    for field in ["created_date", "created_date_millis", "modified_date", "modified_date_millis"] {
        if body.contains_key(field) {
            return Err(EsError::illegal_argument(format!("Provided a pipeline property which is managed by the system: {field}.")));
        }
    }
    validate_pipeline(&id, &body)?;
    let now = ctx.now_ms;
    ctx.cluster
        .update(move |st| {
            let existing = st.metadata.pipelines.get(&id);
            if let Some(req) = if_version {
                let current = existing.and_then(|p| p.get("version")).and_then(Value::as_i64);
                if current != Some(req as i64) {
                    let shown = current.map(|c| c.to_string()).unwrap_or_else(|| "null".into());
                    return Err(EsError::illegal_argument(format!(
                        "version conflict, required version [{req}] for pipeline [{id}] but current version is [{shown}]"
                    )));
                }
            }
            let created = existing.and_then(|p| p.get("created_date_millis")).and_then(Value::as_i64).unwrap_or(now);
            let mut stored = body.clone();
            stored.shift_remove("created_date_millis");
            stored.shift_remove("modified_date_millis");
            stored.insert("created_date_millis".into(), created.into());
            stored.insert("modified_date_millis".into(), now.into());
            st.metadata.pipelines.insert(id, Value::Obj(stored));
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

fn selected(patterns: &[String], id: &str) -> bool {
    patterns.is_empty() || patterns.iter().any(|p| p == "*" || p == "_all" || glob_match(p, id))
}

pub fn get(ctx: &Ctx) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let patterns = ctx.list_param("id").unwrap_or_default();
    let summary = ctx.bool_param("summary", false)?;
    let st = ctx.state();
    let mut out = Map::new();
    for (id, p) in &st.metadata.pipelines {
        if selected(&patterns, id) {
            out.insert(id.clone(), if summary { obj! {} } else { p.clone() });
        }
    }
    if out.is_empty() && !patterns.is_empty() {
        return Ok(Reply::Body(404, obj! {}));
    }
    let mut v = Value::Obj(out);
    if ctx.human {
        crate::units::humanize(&mut v);
    }
    Reply::ok(v)
}

pub async fn delete(ctx: &Ctx<'_>) -> EsResult<Reply> {
    ctx.validate_timeouts()?;
    let patterns = ctx.list_param("id").unwrap_or_default();
    let raw = ctx.path_param("id").unwrap_or_default().to_string();
    ctx.cluster
        .update(move |st| {
            let matched: Vec<String> = st.metadata.pipelines.keys().filter(|id| selected(&patterns, id)).cloned().collect();
            if matched.is_empty() {
                return Err(EsError::resource_not_found(format!("pipeline [{raw}] is missing")));
            }
            // Pipelines used as an index's default or final pipeline can't be deleted.
            for idx in st.metadata.indices.values() {
                for key in ["index.default_pipeline", "index.final_pipeline"] {
                    if let Some(p) = idx.settings.get(key)
                        && matched.iter().any(|m| m == p)
                    {
                        let kind = if key.ends_with("default_pipeline") { "default" } else { "final" };
                        return Err(EsError::illegal_argument(format!(
                            "pipeline [{p}] cannot be deleted because it is the {kind} pipeline for 1 index(es) including [{}]",
                            idx.name
                        )));
                    }
                }
            }
            for m in matched {
                st.metadata.pipelines.remove(&m);
            }
            Ok(())
        })
        .await?;
    Reply::acknowledged()
}

pub fn grok(ctx: &Ctx) -> EsResult<Reply> {
    let ecs = match ctx.query("ecs_compatibility") {
        None | Some("disabled") => false,
        Some("v1") => true,
        Some(other) => {
            return Err(EsError::illegal_argument(format!(
                "unsupported ECS compatibility mode [{other}], expected one of [disabled, v1]"
            )));
        }
    };
    let raw: &'static [u8] = if ecs { include_bytes!("../../assets/grok_ecs_v1.json") } else { include_bytes!("../../assets/grok_legacy.json") };
    if ctx.bool_param("s", false)? {
        let mut v = crate::json::parse(raw).ok().flatten().unwrap_or_default();
        if let Some(Value::Obj(p)) = v.as_object_mut().and_then(|m| m.get_mut("patterns")) {
            p.sort_keys();
        }
        return Reply::ok(v);
    }
    let v = crate::json::parse(raw).ok().flatten().unwrap_or_default();
    Reply::ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn check(body: &str) -> Result<(), String> {
        let v = parse(body.as_bytes()).unwrap().unwrap();
        validate_pipeline("p", v.as_object().unwrap()).map_err(|e| e.reason().to_string())
    }

    #[test]
    fn texts_match_elasticsearch() {
        assert!(check(r#"{"description":"d","processors":[{"set":{"field":"a","value":1}}]}"#).is_ok());
        assert_eq!(check(r#"{"processors":[{"bogus":{}}]}"#).unwrap_err(), "No processor type exists with name [bogus]");
        assert_eq!(check(r#"{"processors":[{"set":{}}]}"#).unwrap_err(), "[field] required property is missing");
        assert_eq!(check(r#"{"processors":[{"set":{"field":"a"}}]}"#).unwrap_err(), "[value] required property is missing");
        assert_eq!(check(r#"{}"#).unwrap_err(), "[processors] required property is missing");
        assert_eq!(
            check(r#"{"processors":[{"set":{"field":"a","value":1,"bogus":2}}]}"#).unwrap_err(),
            "processor [set] doesn't support one or more provided configuration parameters [bogus]"
        );
        assert_eq!(check(r#"{"processors":{"set":{}}}"#).unwrap_err(), "[processors] property isn't a list, but of type [java.util.HashMap]");
        assert_eq!(
            check(r#"{"processors":[],"bogus":1}"#).unwrap_err(),
            "pipeline [p] doesn't support one or more provided configuration parameters [bogus]"
        );
        assert!(check(r#"{"processors":[{"set":{"field":"a","copy_from":"b"}}]}"#).is_ok());
        assert_eq!(check(r#"{"processors":[{"set":{"field":"a","value":1,"on_failure":[{"bogus":{}}]}}]}"#).unwrap_err(), "No processor type exists with name [bogus]");
    }
}
