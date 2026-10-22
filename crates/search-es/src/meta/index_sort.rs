//! Index sorting (`index.sort.*`): the checks Elasticsearch runs when an index is created with a
//! sort, in its order, with its texts.
//!
//! 1. [`validate_settings`] — the settings on their own (`IndexSortConfig.validateSortSettings`
//!    and the setting parsers): values of `order`, `mode` and `missing`, `order`/`mode`/`missing`
//!    without `field`, and list sizes.
//! 2. [`validate_mapping`] — what `DocumentMapper.validate` checks on every mapping change of a
//!    sorted index: no sort field under a nested object, no runtime field shadowing a sort field.
//! 3. [`validate_fields`] — `IndexSortConfig.buildIndexSort` when the index starts: every sort
//!    field must be a concrete (non-alias) field with doc values of a sortable type.
//!
//! Some types pass these checks but make Lucene refuse the sort when the shard opens
//! (`wildcard`, `constant_keyword`, `scaled_float`, `text` with `fielddata`); Elasticsearch then
//! creates the index with red shards. roll accepts them like the other sortable types.

use crate::error::{EsError, EsResult};
use crate::mapping::{Mapping, MappingCtx, MapperRef};
use crate::meta::settings::Settings;

const FIELD: &str = "index.sort.field";
const ORDER: &str = "index.sort.order";
const MODE: &str = "index.sort.mode";
const MISSING: &str = "index.sort.missing";

fn list(settings: &Settings, key: &str) -> Option<Vec<String>> {
    settings.get_list(key)
}

/// The sort fields (`index.sort.field`), empty when the index isn't sorted.
pub fn sort_fields(settings: &Settings) -> Vec<String> {
    list(settings, FIELD).unwrap_or_default()
}

/// The defaults of the `index.sort.*` settings (`IndexSortConfigDefaults`).
pub struct SortDefaults {
    pub fields: Vec<String>,
    pub order: Vec<String>,
    pub mode: Vec<String>,
    pub missing: Vec<String>,
}

/// Each setting's default: from the explicit sort fields when set, otherwise the index mode's
/// built-in sort (`time_series`: `_tsid` then `@timestamp`; `logsdb`: `@timestamp`, optionally
/// after `host.name` and the message template).
pub fn sort_defaults(settings: &Settings) -> SortDefaults {
    let v = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<String>>();
    if let Some(fields) = list(settings, FIELD) {
        let n = fields.len();
        let order = list(settings, ORDER);
        let mode = (0..n).map(|i| if order.as_ref().and_then(|o| o.get(i)).is_some_and(|o| o == "desc") { "max".to_string() } else { "min".to_string() }).collect();
        return SortDefaults { fields, order: vec!["asc".to_string(); n], mode, missing: vec!["_last".to_string(); n] };
    }
    let mode = settings.get("index.mode").map(str::to_ascii_lowercase);
    let flag = |k: &str| settings.get(k).is_some_and(|v| v == "true");
    match mode.as_deref() {
        Some("time_series") => SortDefaults { fields: v(&["_tsid", "@timestamp"]), order: v(&["asc", "desc"]), mode: v(&["min", "max"]), missing: v(&["_last", "_last"]) },
        Some("logsdb" | "logsdb_columnar") => {
            match (flag("index.logsdb.sort_on_host_name"), flag("index.logsdb.sort_on_message_template")) {
                (true, true) => SortDefaults {
                    fields: v(&["host.name", "message.template_id", "@timestamp"]),
                    order: v(&["asc", "asc", "desc"]),
                    mode: v(&["min", "min", "max"]),
                    missing: v(&["_last", "_last", "_last"]),
                },
                (true, false) => SortDefaults { fields: v(&["host.name", "@timestamp"]), order: v(&["asc", "desc"]), mode: v(&["min", "max"]), missing: v(&["_last", "_last"]) },
                (false, true) => SortDefaults {
                    fields: v(&["message.template_id", "@timestamp"]),
                    order: v(&["asc", "desc"]),
                    mode: v(&["min", "max"]),
                    missing: v(&["_last", "_last"]),
                },
                (false, false) => SortDefaults { fields: v(&["@timestamp"]), order: v(&["desc"]), mode: v(&["max"]), missing: v(&["_last"]) },
            }
        }
        _ => SortDefaults { fields: Vec::new(), order: Vec::new(), mode: Vec::new(), missing: Vec::new() },
    }
}

/// Checks the `index.sort.*` settings alone.
pub fn validate_settings(settings: &Settings) -> EsResult<()> {
    let order = list(settings, ORDER);
    let mode = list(settings, MODE);
    let missing = list(settings, MISSING);
    // The setting parsers run first (index setting validation).
    if let Some(o) = &order {
        for v in o {
            if !v.eq_ignore_ascii_case("asc") && !v.eq_ignore_ascii_case("desc") {
                return Err(EsError::illegal_argument(format!("Illegal sort order:{v}")));
            }
        }
    }
    if let Some(m) = &mode {
        for v in m {
            let up = v.to_ascii_uppercase();
            if !MULTI_VALUE_MODES.contains(&up.as_str()) {
                return Err(EsError::illegal_argument(format!("Illegal sort mode: {v}")));
            }
            if up != "MIN" && up != "MAX" {
                return Err(EsError::illegal_argument(format!("Illegal index sort mode:[{up}], must be one of [MAX, MIN]")));
            }
        }
    }
    if let Some(m) = &missing {
        for v in m {
            if v != "_last" && v != "_first" {
                return Err(EsError::illegal_argument(format!("Illegal missing value:[{v}], must be one of [_last, _first]")));
            }
        }
    }
    let Some(fields) = list(settings, FIELD) else {
        for (key, set) in [(ORDER, &order), (MODE, &mode), (MISSING, &missing)] {
            if set.is_some() {
                return Err(EsError::illegal_argument(format!("setting [{key}] requires [{FIELD}] to be configured")));
            }
        }
        return Ok(());
    };
    // `index.sort.mode`'s default reads `index.sort.order` per field, past its end when it's
    // shorter: Elasticsearch reports that `IndexOutOfBoundsException` against `index.mode`.
    if mode.is_none()
        && let Some(o) = &order
        && o.len() < fields.len()
    {
        let mode_name = settings.get("index.mode").unwrap_or("standard");
        // An empty list is an array, a non-empty one an immutable list: different exceptions.
        let cause = if o.is_empty() {
            EsError::java(400, "array_index_out_of_bounds_exception", "Index 0 out of bounds for length 0")
        } else {
            EsError::java(400, "index_out_of_bounds_exception", format!("Index: {} Size: {}", o.len(), o.len()))
        };
        return Err(EsError::illegal_argument(format!("Failed to parse value [{mode_name}] for setting [index.mode]")).caused_by(cause));
    }
    let shown = |l: &[String]| format!("[{}]", l.join(", "));
    if let Some(o) = &order
        && o.len() != fields.len()
    {
        let o: Vec<String> = o.iter().map(|v| v.to_ascii_lowercase()).collect();
        return Err(EsError::illegal_argument(format!("{FIELD}:{} {ORDER}:{}, size mismatch", shown(&fields), shown(&o))));
    }
    if let Some(m) = &mode
        && m.len() != fields.len()
    {
        let m: Vec<String> = m.iter().map(|v| v.to_ascii_uppercase()).collect();
        return Err(EsError::illegal_argument(format!("{FIELD}:{} {MODE}:{}, size mismatch", shown(&fields), shown(&m))));
    }
    if let Some(m) = &missing
        && m.len() != fields.len()
    {
        return Err(EsError::illegal_argument(format!("{FIELD}:{} {MISSING}:{}, size mismatch", shown(&fields), shown(m))));
    }
    Ok(())
}

/// `MultiValueMode` constants (`fromString` upper-cases).
const MULTI_VALUE_MODES: &[&str] = &["SUM", "AVG", "MEDIAN", "MIN", "MAX"];

/// The nested object a path lies in (the deepest one), if any.
fn nested_parent(mapping: &Mapping, path: &str) -> Option<String> {
    if let Some(MapperRef::Object(o)) = mapping.lookup(path)
        && o.is_nested()
    {
        return Some(path.to_string());
    }
    let mut best = None;
    for (i, _) in path.match_indices('.') {
        let head = &path[..i];
        if let Some(MapperRef::Object(o)) = mapping.lookup(head)
            && o.is_nested()
        {
            best = Some(head.to_string());
        }
    }
    best
}

fn has_nested(mapping: &Mapping) -> bool {
    mapping.field_paths().iter().any(|(_, m)| matches!(m, MapperRef::Object(o) if o.is_nested()))
}

/// `DocumentMapper.validate`'s index sort checks (on creation and on every mapping update).
pub fn validate_mapping(settings: &Settings, mapping: &Mapping) -> EsResult<()> {
    validate_mapping_in(crate::cluster::Dialect::Elasticsearch, settings, mapping, "")
}

/// [`validate_mapping`]; OpenSearch sorts indices with nested objects as long as no sort field
/// lies inside one (and words the error differently).
pub fn validate_mapping_in(d: crate::cluster::Dialect, settings: &Settings, mapping: &Mapping, index: &str) -> EsResult<()> {
    // The index mode's built-in sort counts too (TSDB's `@timestamp`).
    let fields = sort_defaults(settings).fields;
    if fields.is_empty() {
        return Ok(());
    }
    if has_nested(mapping) {
        for f in &fields {
            if let Some(parent) = nested_parent(mapping, f) {
                if d == crate::cluster::Dialect::OpenSearch {
                    return Err(EsError::illegal_argument(format!(
                        "index sorting on nested fields is not supported: found nested sort field [{f}] in [{index}]"
                    )));
                }
                return Err(EsError::illegal_argument(format!("cannot apply index sort to field [{f}] under nested object [{parent}]")));
            }
        }
    }
    for (name, rf) in &mapping.runtime {
        // A composite runtime field defines `name.sub` fields.
        let shadows = if rf.ty == "composite" {
            rf.params
                .get("fields")
                .and_then(|v| v.as_object())
                .is_some_and(|subs| subs.keys().any(|s| fields.iter().any(|f| *f == format!("{name}.{s}"))))
        } else {
            fields.iter().any(|f| f == name)
        };
        if shadows {
            let shadowed = if rf.ty == "composite" {
                rf.params
                    .get("fields")
                    .and_then(|v| v.as_object())
                    .and_then(|subs| subs.keys().map(|s| format!("{name}.{s}")).find(|p| fields.contains(p)))
                    .unwrap_or_else(|| name.clone())
            } else {
                name.clone()
            };
            return Err(EsError::mapper_parsing(format!(
                "runtime field [{shadowed}] shadows an index sort field, which would prevent shards from being allocated"
            )));
        }
    }
    Ok(())
}

/// `IndexSortConfig.buildIndexSort`: each sort field must resolve to a sortable field with doc
/// values.
pub fn validate_fields(settings: &Settings, mapping: &Mapping, index: &str) -> EsResult<()> {
    let fields = sort_fields(settings);
    if fields.is_empty() {
        return Ok(());
    }
    let ctx = MappingCtx::from_settings(settings);
    let tsdb = settings.get("index.mode").is_some_and(|m| m.eq_ignore_ascii_case("time_series"));
    for f in &fields {
        let found = mapping.lookup(f);
        let field = match found {
            Some(MapperRef::Field(field)) => field,
            Some(MapperRef::Alias(_)) => return Err(EsError::illegal_argument(format!("Cannot use alias [{f}] as an index sort field"))),
            Some(MapperRef::Object(_)) | None => {
                let mut err = format!("unknown index sort field:[{f}]");
                if tsdb {
                    err.push_str(" required by [index.mode=time_series]");
                }
                return Err(EsError::illegal_argument(err));
            }
        };
        let no_dv = |cause: String| {
            EsError::illegal_argument(format!("docvalues not found for index sort field:[{f}]")).caused_by(EsError::illegal_argument(cause))
        };
        let unsupported = |ty: &str| format!("Can't load fielddata on [{f}] because fielddata is unsupported on fields of type [{ty}]. Use doc values instead.");
        let ty = field.ty;
        match ty {
            "text" | "annotated_text" => {
                if !field.bool_param("fielddata", &ctx) {
                    return Err(no_dv(format!(
                        "Fielddata is disabled on [{f}] in [{index}]. Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [{f}] in order to load field data by uninverting the inverted index. Note that this can use significant memory."
                    )));
                }
            }
            "match_only_text" => return Err(no_dv("match_only_text fields do not support sorting and aggregations".into())),
            "pattern_text" => return Err(no_dv("pattern_text fields do not support sorting and aggregations".into())),
            "completion" | "search_as_you_type" => return Err(no_dv(format!("Fielddata is not supported on field [{f}] of type [{ty}]"))),
            "rank_feature" | "rank_features" | "semantic_text" => {
                return Err(no_dv(format!("[{ty}] fields do not support sorting, scripting or aggregating")));
            }
            "dense_vector" | "sparse_vector" | "rank_vectors" => {
                return Err(EsError::illegal_argument(format!("Field [{f}] of type [{ty}] doesn't support sort")));
            }
            "geo_point" | "point" => {
                return Err(EsError::illegal_argument("can't sort on geo_point or point field without using specific sorting feature, like geo_distance"));
            }
            "geo_shape" | "shape" => {
                return Err(EsError::illegal_argument("can't sort on geo_shape field without using specific sorting feature, like geo_distance"));
            }
            "integer_range" | "long_range" | "float_range" | "double_range" | "date_range" | "ip_range" => {
                return Err(EsError::illegal_argument(format!("Sorting by range field [{f}] is not supported")));
            }
            "histogram" | "counted_keyword" | "exponential_histogram" | "tdigest" => {
                return Err(EsError::illegal_argument(format!("can't sort on the [{ty}] field")));
            }
            "half_float" => {
                if !field.bool_param("doc_values", &ctx) {
                    return Err(no_dv(unsupported(ty)));
                }
                // Lucene sorts half floats as `HALF_FLOAT`, not one of the index sort types.
                return Err(EsError::illegal_argument(format!("invalid index sort field:[{f}]")));
            }
            _ => {
                if field.def().param_index("doc_values").is_some() && !field.bool_param("doc_values", &ctx) {
                    return Err(no_dv(unsupported(ty)));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(pairs: &[(&str, &str)]) -> Settings {
        let mut s = Settings::new();
        for (k, v) in pairs {
            s.set(*k, *v);
        }
        s
    }

    #[test]
    fn settings_checks_match_elasticsearch() {
        let e = validate_settings(&st(&[(ORDER, "up"), (FIELD, "k")])).unwrap_err();
        assert_eq!(e.reason.as_deref(), Some("Illegal sort order:up"));
        let e = validate_settings(&st(&[(FIELD, "k"), (MODE, "avg")])).unwrap_err();
        assert_eq!(e.reason.as_deref(), Some("Illegal index sort mode:[AVG], must be one of [MAX, MIN]"));
        let e = validate_settings(&st(&[(FIELD, "k"), (MISSING, "_foo")])).unwrap_err();
        assert_eq!(e.reason.as_deref(), Some("Illegal missing value:[_foo], must be one of [_last, _first]"));
        let e = validate_settings(&st(&[(FIELD, "k,l"), (MODE, "min")])).unwrap_err();
        assert_eq!(e.reason.as_deref(), Some("index.sort.field:[k, l] index.sort.mode:[MIN], size mismatch"));
        let e = validate_settings(&st(&[(FIELD, "k,l"), (MISSING, "_last")])).unwrap_err();
        assert_eq!(e.reason.as_deref(), Some("index.sort.field:[k, l] index.sort.missing:[_last], size mismatch"));
        let e = validate_settings(&st(&[(FIELD, "k,l"), (ORDER, "asc")])).unwrap_err();
        assert_eq!(e.reason.as_deref(), Some("Failed to parse value [standard] for setting [index.mode]"));
        let e = validate_settings(&st(&[(ORDER, "asc")])).unwrap_err();
        assert_eq!(e.reason.as_deref(), Some("setting [index.sort.order] requires [index.sort.field] to be configured"));
        validate_settings(&st(&[(FIELD, "k"), (ORDER, "desc"), (MODE, "max"), (MISSING, "_first")])).unwrap();
    }
}
