//! The field types roll search accepts in mappings, with their parameters in Elasticsearch's
//! serialization order (as `GET _mapping?include_defaults` prints them), defaults, merge rules
//! and type-specific validation.
//!
//! Object-like types (`object`, `nested`, `passthrough`) and `alias` are not here: they have
//! their own mapper kinds in [`super`].

use super::params::{Dv, Kind, Merge, NumKind, ParamDef, Ser, SettingDefault, FALSE, TRUE, p};
use super::{FieldMapper, MappingCtx};
use crate::error::EsError;
use crate::json::Value;

/// A type's cross-parameter validation.
pub type Validator = fn(&FieldMapper, &MappingCtx) -> Result<(), EsError>;

#[derive(Debug)]
pub struct TypeDef {
    pub name: &'static str,
    pub params: &'static [ParamDef],
    /// Index in `params` before which `fields`, `copy_to` and `synthetic_source_keep` are
    /// printed (`params.len()` = after all of them).
    pub fields_at: usize,
    /// Accepts `fields` (multi-fields) and `copy_to`.
    pub multi_fields: bool,
    /// May itself be a multi-field.
    pub in_multi_field: bool,
    /// Cross-parameter validation after parsing (and after merges).
    pub validate: Option<Validator>,
    /// The family for `field_caps`-style grouping and type-change messages.
    pub family: Family,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Family {
    Text,
    Keyword,
    Numeric,
    Date,
    Boolean,
    Binary,
    Ip,
    Range,
    Geo,
    Vector,
    Other,
}

use Kind::*;
use Merge::{Fixed, TrueToFalse, Updatable};

const INT_MAX: i64 = i32::MAX as i64;

const INDEX: ParamDef = p("index", Bool, Dv::Fn(default_index), Fixed);

/// `index.mapping.index_disabled_by_default` (on in strict columnar indices): fields are only
/// columns unless asked to be indexed; text stays searchable.
fn default_index(f: &FieldMapper, ctx: &MappingCtx) -> Value {
    Value::Bool(!ctx.index_disabled_by_default || matches!(f.ty, "text" | "match_only_text" | "search_as_you_type" | "annotated_text"))
}
const DOC_VALUES: ParamDef = p("doc_values", DocValues, Dv::Fn(default_doc_values_on), Fixed);
const DOC_VALUES_OFF: ParamDef = p("doc_values", DocValues, Dv::Fn(default_doc_values_off), Fixed);
/// Text fields print `doc_values` whenever it is given.
const TEXT_DOC_VALUES: ParamDef = p("doc_values", DocValues, Dv::Fn(default_doc_values_off), Fixed).ser(Ser::WhenSet);

/// In strict columnar indices the types with column configurations default to it.
fn default_doc_values_on(f: &FieldMapper, ctx: &MappingCtx) -> Value {
    if ctx.columnar.is_some() && super::params::extended_doc_values(f.ty) {
        return super::params::columnar_doc_values(ctx.dv_multi_value, ctx.dv_nullability);
    }
    Value::Bool(true)
}

fn default_doc_values_off(f: &FieldMapper, ctx: &MappingCtx) -> Value {
    if ctx.columnar.is_some() && super::params::extended_doc_values(f.ty) {
        return super::params::columnar_doc_values(ctx.dv_multi_value, ctx.dv_nullability);
    }
    Value::Bool(false)
}
const STORE: ParamDef = p("store", Bool, FALSE, Fixed);
const META: ParamDef = p("meta", Meta, Dv::EmptyObj, Updatable);
const SCRIPT: ParamDef = p("script", Script, Dv::Null, Fixed).nullable();
const ON_SCRIPT_ERROR: ParamDef = p("on_script_error", Enum(&["fail", "continue"]), Dv::Str("fail"), Updatable);
const IGNORE_MALFORMED: ParamDef = p("ignore_malformed", Bool, Dv::SettingBool(SettingDefault::IgnoreMalformed), Updatable).ser(Ser::WhenSet);
/// Date mappers print `ignore_malformed` only when it differs from the index default.
const DATE_IGNORE_MALFORMED: ParamDef = p("ignore_malformed", Bool, Dv::SettingBool(SettingDefault::IgnoreMalformed), Updatable).ser(Ser::Configured);
const COERCE: ParamDef = p("coerce", Bool, Dv::SettingBool(SettingDefault::Coerce), Updatable).ser(Ser::WhenSet);
const TS_DIMENSION: ParamDef = p("time_series_dimension", Bool, FALSE, Fixed);
const TS_METRIC: ParamDef = p("time_series_metric", Enum(&["gauge", "counter"]), Dv::Null, Fixed).nullable();
const EAGER_GLOBAL_ORDINALS: ParamDef = p("eager_global_ordinals", Bool, FALSE, Updatable);
const IGNORE_ABOVE: ParamDef = p("ignore_above", Int { min: 0, max: INT_MAX, msg: Some("[ignore_above] must be positive, got [{v}]") }, Dv::IgnoreAbove, Updatable);
const SIMILARITY: ParamDef = p("similarity", Similarity, Dv::Null, Fixed).nullable();
const SPLIT_QUERIES: ParamDef = p("split_queries_on_whitespace", Bool, FALSE, Updatable);
const IGNORE_Z: ParamDef = p("ignore_z_value", Bool, TRUE, Updatable).ser(Ser::WhenSet);
const ORIENTATION: ParamDef =
    p("orientation", Enum(&["right", "RIGHT", "ccw", "CCW", "counterclockwise", "COUNTERCLOCKWISE", "left", "LEFT", "cw", "CW", "clockwise", "CLOCKWISE"]), Dv::Str("RIGHT"), Updatable);
const TERM_VECTOR: ParamDef = p(
    "term_vector",
    Enum(&["no", "yes", "with_positions", "with_offsets", "with_positions_offsets", "with_positions_payloads", "with_positions_offsets_payloads"]),
    Dv::Str("no"),
    Fixed,
);
const TEXT_INDEX_OPTIONS: ParamDef = p("index_options", Enum(&["positions", "docs", "freqs", "offsets"]), Dv::Str("positions"), Fixed);
const KEYWORD_INDEX_OPTIONS: ParamDef = p("index_options", Enum(&["docs", "freqs"]), Dv::Str("docs"), Fixed);

/// `analyzer` prints when configured or when the search analyzers differ from it.
fn ser_index_analyzer(f: &FieldMapper, ctx: &MappingCtx) -> bool {
    let a = f.effective("analyzer", ctx);
    f.is_configured("analyzer", ctx) || a != f.effective("search_analyzer", ctx) || a != f.effective("search_quote_analyzer", ctx)
}

fn ser_search_analyzer(f: &FieldMapper, ctx: &MappingCtx) -> bool {
    f.is_configured("search_analyzer", ctx) || f.effective("search_analyzer", ctx) != f.effective("search_quote_analyzer", ctx)
}

const ANALYZER: ParamDef = p("analyzer", Analyzer, Dv::Str("default"), Fixed).ser(Ser::Fn(ser_index_analyzer));
const SEARCH_ANALYZER: ParamDef = p("search_analyzer", Analyzer, Dv::Param("analyzer"), Updatable).ser(Ser::Fn(ser_search_analyzer));
const SEARCH_QUOTE_ANALYZER: ParamDef = p("search_quote_analyzer", Analyzer, Dv::Param("search_analyzer"), Updatable);

pub static KEYWORD: &[ParamDef] = &[
    INDEX,
    DOC_VALUES,
    STORE,
    p("null_value", Str, Dv::Null, Fixed).nullable(),
    EAGER_GLOBAL_ORDINALS,
    IGNORE_ABOVE,
    KEYWORD_INDEX_OPTIONS,
    p("norms", Bool, FALSE, TrueToFalse),
    SIMILARITY,
    p("normalizer", Normalizer, Dv::Null, Fixed).nullable(),
    p("normalizer_skip_store_original_value", Bool, Dv::Fn(default_skip_store), Fixed),
    SPLIT_QUERIES,
    SCRIPT,
    ON_SCRIPT_ERROR,
    META,
    TS_DIMENSION,
];

fn default_skip_store(f: &FieldMapper, ctx: &MappingCtx) -> Value {
    Value::Bool(f.effective("normalizer", ctx).as_str() == Some("lowercase"))
}

pub static TEXT: &[ParamDef] = &[
    INDEX,
    STORE,
    TEXT_DOC_VALUES,
    META,
    TEXT_INDEX_OPTIONS,
    TERM_VECTOR,
    p("norms", Bool, TRUE, TrueToFalse),
    ANALYZER,
    SEARCH_ANALYZER,
    SEARCH_QUOTE_ANALYZER,
    SIMILARITY,
    EAGER_GLOBAL_ORDINALS,
    p("position_increment_gap", Int { min: 0, max: INT_MAX, msg: Some("[position_increment_gap] must be positive, got [{v}]") }, Dv::Int(100), Fixed),
    p("fielddata", Bool, FALSE, Updatable),
    p("fielddata_frequency_filter", FreqFilter, Dv::Fn(default_freq_filter), Updatable),
    p("index_prefixes", IndexPrefixes, Dv::Null, Fixed).nullable(),
    p("index_phrases", Bool, FALSE, Fixed),
];

fn default_freq_filter(_: &FieldMapper, _: &MappingCtx) -> Value {
    crate::obj! { "min" => 0.0, "max" => INT_MAX as f64, "min_segment_size" => 0 }
}

pub static MATCH_ONLY_TEXT: &[ParamDef] = &[META, TEXT_DOC_VALUES, INDEX];

macro_rules! numeric {
    ($name:ident, $kind:expr) => {
        pub static $name: &[ParamDef] = &[
            INDEX,
            DOC_VALUES,
            STORE,
            IGNORE_MALFORMED,
            COERCE,
            p("null_value", NullNumber($kind), Dv::Null, Fixed).nullable(),
            SCRIPT,
            ON_SCRIPT_ERROR,
            META,
            TS_DIMENSION,
            TS_METRIC,
        ];
    };
}

numeric!(LONG, NumKind::Long);
numeric!(INTEGER, NumKind::Integer);
numeric!(SHORT, NumKind::Short);
numeric!(BYTE, NumKind::Byte);
numeric!(DOUBLE, NumKind::Double);
numeric!(FLOAT, NumKind::Float);
numeric!(HALF_FLOAT, NumKind::HalfFloat);

pub static SCALED_FLOAT: &[ParamDef] = &[
    INDEX,
    DOC_VALUES,
    STORE,
    IGNORE_MALFORMED,
    META,
    p("scaling_factor", Double { positive: true }, Dv::Null, Fixed).ser(Ser::Always),
    COERCE,
    p("null_value", NullNumber(NumKind::ScaledFloat), Dv::Null, Fixed).nullable(),
    TS_METRIC,
];

pub static UNSIGNED_LONG: &[ParamDef] = &[
    INDEX,
    DOC_VALUES,
    STORE,
    IGNORE_MALFORMED,
    p("null_value", NullNumber(NumKind::UnsignedLong), Dv::Null, Fixed).nullable(),
    META,
    TS_DIMENSION,
    TS_METRIC,
];

pub static DATE: &[ParamDef] = &[
    INDEX,
    DOC_VALUES,
    STORE,
    p("format", Format, Dv::Str("strict_date_optional_time||epoch_millis"), Fixed),
    p("locale", Locale, Dv::Str("en"), Fixed),
    p("null_value", Str, Dv::Null, Fixed).nullable(),
    DATE_IGNORE_MALFORMED,
    SCRIPT,
    ON_SCRIPT_ERROR,
    META,
];

pub static DATE_NANOS: &[ParamDef] = &[
    INDEX,
    DOC_VALUES,
    STORE,
    p("format", Format, Dv::Str("strict_date_optional_time_nanos||epoch_millis"), Fixed),
    p("locale", Locale, Dv::Str("en"), Fixed),
    p("null_value", Str, Dv::Null, Fixed).nullable(),
    DATE_IGNORE_MALFORMED,
    SCRIPT,
    ON_SCRIPT_ERROR,
    META,
];

pub static BOOLEAN: &[ParamDef] = &[
    META,
    DOC_VALUES,
    INDEX,
    p("null_value", NullBool, Dv::Null, Fixed).nullable(),
    STORE,
    SCRIPT,
    ON_SCRIPT_ERROR,
    IGNORE_MALFORMED,
    TS_DIMENSION,
];

pub static BINARY: &[ParamDef] = &[META, STORE, DOC_VALUES_OFF];

pub static IP: &[ParamDef] = &[
    INDEX,
    DOC_VALUES,
    STORE,
    IGNORE_MALFORMED,
    p("null_value", NullIp, Dv::Null, Fixed).nullable(),
    SCRIPT,
    ON_SCRIPT_ERROR,
    META,
    TS_DIMENSION,
];

pub static RANGE: &[ParamDef] = &[
    INDEX,
    DOC_VALUES,
    STORE,
    COERCE,
    // Accepted on every range type, but only date ranges may set a format (`validate_range`).
    p("format", Format, Dv::Null, Fixed).ser(Ser::Never),
    p("locale", Locale, Dv::Null, Fixed).ser(Ser::Never),
    META,
];

pub static DATE_RANGE: &[ParamDef] = &[
    INDEX,
    DOC_VALUES,
    STORE,
    COERCE,
    p("format", Format, Dv::Str("strict_date_optional_time||epoch_millis"), Fixed),
    p("locale", Locale, Dv::Str("en"), Fixed),
    META,
];

pub static FLATTENED: &[ParamDef] = &[
    INDEX,
    DOC_VALUES,
    p("depth_limit", Int { min: 0, max: INT_MAX, msg: Some("[depth_limit] must be positive, got [{v}]") }, Dv::Int(20), Updatable),
    p("null_value", Str, Dv::Null, Fixed).nullable(),
    EAGER_GLOBAL_ORDINALS,
    IGNORE_ABOVE,
    KEYWORD_INDEX_OPTIONS,
    SIMILARITY,
    SPLIT_QUERIES,
    META,
    p("time_series_dimensions", StrList, Dv::EmptyList, Fixed),
    p("passthrough", Json, Dv::Null, Updatable).nullable().ser(Ser::NonNull),
    p("preserve_leaf_arrays", Enum(&["lossy", "exact"]), Dv::Str("lossy"), Fixed),
];

pub static GEO_POINT: &[ParamDef] = &[
    DOC_VALUES,
    INDEX,
    STORE,
    IGNORE_MALFORMED,
    IGNORE_Z,
    p("null_value", Json, Dv::Null, Fixed).nullable(),
    SCRIPT,
    ON_SCRIPT_ERROR,
    META,
    TS_DIMENSION,
    p("time_series_metric", Enum(&["position"]), Dv::Null, Fixed).nullable(),
];

pub static GEO_SHAPE: &[ParamDef] = &[
    INDEX,
    DOC_VALUES,
    STORE,
    IGNORE_MALFORMED,
    IGNORE_Z,
    p("coerce", Bool, FALSE, Updatable).ser(Ser::WhenSet),
    ORIENTATION,
    SCRIPT,
    ON_SCRIPT_ERROR,
    META,
];

pub static POINT: &[ParamDef] =
    &[INDEX, DOC_VALUES, STORE, IGNORE_MALFORMED, IGNORE_Z, p("null_value", Json, Dv::Null, Fixed).nullable(), META];

pub static SHAPE: &[ParamDef] =
    &[INDEX, DOC_VALUES, IGNORE_MALFORMED, IGNORE_Z, p("coerce", Bool, FALSE, Updatable).ser(Ser::WhenSet), ORIENTATION, META];

fn dense_vector_ser_indexed(f: &FieldMapper, ctx: &MappingCtx) -> bool {
    f.effective("index", ctx).as_bool() == Some(true)
}

pub static DENSE_VECTOR: &[ParamDef] = &[
    p("element_type", Enum(&["float", "byte", "bit", "bfloat16"]), Dv::Fn(default_element_type), Fixed).ser(Ser::Fn(ser_element_type)),
    // The range depends on `element_type` (checked in `validate_dense_vector`).
    p("dims", Int { min: i32::MIN as i64, max: INT_MAX, msg: None }, Dv::Null, Fixed).ser(Ser::WhenSet),
    p("index", Bool, Dv::Fn(default_vector_index), Fixed).ser(Ser::Always),
    p("similarity", Enum(&["l2_norm", "cosine", "dot_product", "max_inner_product"]), Dv::Fn(default_similarity), Fixed).ser(Ser::Fn(dense_vector_ser_indexed)),
    p("index_options", Json, Dv::Fn(default_vector_index_options), Merge::Custom { allowed: vector_options_updatable, show: vector_options_string })
        .ser(Ser::Fn(ser_vector_index_options)),
    META,
];

fn vector_options_m(v: &Value) -> i64 {
    v.get("m").and_then(Value::as_i64).unwrap_or(16)
}

/// `DenseVectorIndexOptions.updatableTo`: which index formats a field may move to. Graphs may
/// only gain connections (`m`), and quantization may only get coarser or move to a graph.
fn vector_options_updatable(cur: &Value, new: &Value) -> bool {
    let (Some(from), Some(to)) = (cur.get("type").and_then(Value::as_str), new.get("type").and_then(Value::as_str)) else { return false };
    let m_ok = || vector_options_m(new) >= vector_options_m(cur);
    match from {
        "flat" => true,
        "int8_flat" => matches!(to, "int8_flat" | "hnsw" | "int8_hnsw" | "int4_hnsw" | "bbq_hnsw" | "int4_flat" | "bbq_flat"),
        "int4_flat" => matches!(to, "int4_flat" | "hnsw" | "int8_hnsw" | "int4_hnsw" | "bbq_hnsw" | "bbq_flat"),
        "hnsw" => matches!(to, "hnsw" | "int8_hnsw" | "int4_hnsw" | "bbq_hnsw") && m_ok(),
        "int8_hnsw" => matches!(to, "int8_hnsw" | "int4_hnsw" | "bbq_hnsw") && m_ok(),
        "int4_hnsw" => matches!(to, "int4_hnsw" | "bbq_hnsw") && m_ok(),
        "bbq_hnsw" => to == "bbq_hnsw" && m_ok(),
        "bbq_flat" => matches!(to, "bbq_flat" | "bbq_hnsw"),
        "bbq_disk" => {
            to == "bbq_disk"
                && ["do_precondition", "auto_calibrate"].iter().all(|k| cur.get(k).and_then(Value::as_bool).unwrap_or(false) == new.get(k).and_then(Value::as_bool).unwrap_or(false))
        }
        _ => false,
    }
}

const VECTOR_INDEX_TYPES: &[&str] = &["hnsw", "int8_hnsw", "int4_hnsw", "bbq_hnsw", "flat", "int8_flat", "int4_flat", "bbq_flat", "bbq_disk"];

/// A Java `float` as the JSON number Jackson prints for it.
fn float_value(f: f32) -> Value {
    Value::from(crate::json::write::java_float(f).parse::<f64>().unwrap_or(f as f64))
}

/// `VectorIndexType.parseIndexOptions`: validates a `dense_vector` field's `index_options` and
/// returns them as Elasticsearch prints them (defaults filled in, in its field order).
pub fn normalize_vector_index_options(field: &str, opts: &crate::json::Map, ctx: &MappingCtx) -> Result<Value, EsError> {
    use super::params::{node_bool, node_double, node_int};
    let ty = match opts.get("type") {
        None => return Err(EsError::mapper_parsing("[index_options] requires field [type] to be configured")),
        Some(t) => t.to_es_string(),
    };
    if !VECTOR_INDEX_TYPES.contains(&ty.as_str()) {
        return Err(EsError::mapper_parsing(format!("Unknown vector index options type [{ty}] for field [{field}]")));
    }
    let mut rest = opts.clone();
    rest.shift_remove("type");
    let mut take = |k: &str| rest.shift_remove(k);
    let graph = ty.ends_with("hnsw");
    let quantized = !matches!(ty.as_str(), "hnsw" | "flat");
    let mut out = crate::json::Map::new();
    out.insert("type".into(), Value::from(ty.clone()));
    let m = take("m").map(|v| node_int(&v)).transpose()?.unwrap_or(16);
    let ef = take("ef_construction").map(|v| node_int(&v)).transpose()?.unwrap_or(100);
    let confidence = if matches!(ty.as_str(), "int8_hnsw" | "int4_hnsw" | "int8_flat" | "int4_flat") {
        take("confidence_interval").map(|v| node_double(&v).map(|d| d as f32)).transpose()?
    } else {
        None
    };
    if confidence.is_some() {
        ctx.warn(format!("Parameter [confidence_interval] in [index_options] for dense_vector field [{field}] is deprecated and will be removed in a future version"));
    }
    let on_disk = take("on_disk_rescore").map(|v| node_bool(&v)).transpose()?.unwrap_or(false);
    if on_disk && !(graph && quantized) && ty != "bbq_disk" {
        return Err(EsError::illegal_argument("on_disk_rescore is only supported for indexed and quantized vector types"));
    }
    let threshold = if graph || ty == "bbq_disk" { take("flat_index_threshold").map(|v| node_int(&v)).transpose()? } else { None };
    let mut rescore = if quantized {
        match take("rescore_vector") {
            None => None,
            Some(Value::Obj(r)) => {
                let Some(o) = r.get("oversample") else {
                    return Err(EsError::illegal_argument("Invalid rescore_vector value. Missing required field oversample"));
                };
                let o = node_double(o)? as f32;
                if o < 1.0 && o != 0.0 {
                    return Err(EsError::illegal_argument("oversample must be greater than 1 or exactly 0"));
                } else if o > 10.0 {
                    return Err(EsError::illegal_argument("oversample must be less than or equal to 10"));
                }
                Some(o)
            }
            Some(other) => return Err(EsError::mapper_parsing(format!("[rescore_vector] should be a map but was of type: {}", super::params::conflict_string(&other)))),
        }
    } else {
        None
    };
    if rescore.is_none() && matches!(ty.as_str(), "bbq_hnsw" | "bbq_flat") {
        rescore = Some(3.0);
    }
    let mut disk = None;
    if ty == "bbq_disk" {
        let cluster_size = match take("cluster_size") {
            Some(v) => {
                let n = node_int(&v)?;
                if !(64..=65536).contains(&n) {
                    return Err(EsError::illegal_argument(format!("cluster_size must be between 64 and 65536, got: {n}")));
                }
                n
            }
            None => 384,
        };
        if let Some(t) = threshold
            && t < -1
        {
            return Err(EsError::illegal_argument(format!("flat_index_threshold must be -1 (dynamic), 0 (disabled), or > 0, got: {t} for field [{field}]")));
        }
        let visit = match take("default_visit_percentage") {
            Some(v) => {
                let p = node_double(&v)? as f32 as f64;
                if !(0.0..=100.0).contains(&p) {
                    return Err(EsError::illegal_argument(format!(
                        "default_visit_percentage must be between 0.0 and 100.0, got: {} for field [{field}]",
                        crate::json::write::java_double(p)
                    )));
                }
                p
            }
            None => 0.0,
        };
        let bits = take("bits").map(|v| node_int(&v)).transpose()?.unwrap_or(1);
        if !matches!(bits, 1 | 2 | 4 | 7) {
            return Err(EsError::illegal_argument(format!("'bits' must be 1, 2, 4 or 7, got: {bits} for field [{field}]")));
        }
        if rescore.is_none() {
            rescore = match bits {
                2 => Some(1.5),
                1 => Some(3.0),
                _ => None,
            };
        }
        let precondition = take("precondition").map(|v| node_bool(&v)).transpose()?.unwrap_or(false);
        let auto_calibrate = take("auto_calibrate").map(|v| node_bool(&v)).transpose()?.unwrap_or(false);
        disk = Some((cluster_size, visit, bits, precondition, auto_calibrate));
    }
    if !rest.is_empty() {
        let mut s = String::new();
        for (k, v) in &rest {
            s.push_str(&format!(" [{k} : {}]", super::params::conflict_string(v)));
        }
        return Err(EsError::mapper_parsing(format!("Mapping definition for [{field}] has unsupported parameters: {s}")));
    }
    let rescore_value = |o: f32| crate::obj! { "oversample" => float_value(o) };
    if let Some((cluster_size, visit, bits, precondition, auto_calibrate)) = disk {
        out.insert("cluster_size".into(), cluster_size.into());
        out.insert("flat_index_threshold".into(), threshold.unwrap_or(-1).into());
        out.insert("default_visit_percentage".into(), float_value(visit as f32));
        if on_disk {
            out.insert("on_disk_rescore".into(), true.into());
        }
        if let Some(o) = rescore {
            out.insert("rescore_vector".into(), rescore_value(o));
        }
        out.insert("bits".into(), bits.into());
        if precondition {
            out.insert("precondition".into(), true.into());
        }
        if auto_calibrate {
            out.insert("auto_calibrate".into(), true.into());
        }
        return Ok(Value::Obj(out));
    }
    if graph {
        out.insert("m".into(), m.into());
        out.insert("ef_construction".into(), ef.into());
    }
    if let Some(c) = confidence {
        out.insert("confidence_interval".into(), float_value(c));
    }
    if on_disk {
        out.insert("on_disk_rescore".into(), true.into());
    }
    if let Some(o) = rescore {
        out.insert("rescore_vector".into(), rescore_value(o));
    }
    if let Some(t) = threshold.filter(|t| *t >= 0) {
        out.insert("flat_index_threshold".into(), t.into());
    }
    Ok(Value::Obj(out))
}

/// The index options as Elasticsearch's `toString` prints them in merge conflicts.
fn vector_options_string(v: &Value) -> String {
    let Some(ty) = v.get("type").and_then(Value::as_str) else { return "null".into() };
    let int = |k: &str, d: i64| v.get(k).and_then(Value::as_i64).unwrap_or(d);
    let on_disk = v.get("on_disk_rescore").and_then(Value::as_bool).unwrap_or(false);
    let rescore = |none: &str| match v.get("rescore_vector").and_then(|r| r.get("oversample")).and_then(Value::as_f64) {
        Some(o) => format!("RescoreVector[oversample={}]", crate::json::write::java_float(o as f32)),
        None => none.to_string(),
    };
    let threshold = int("flat_index_threshold", -1);
    match ty {
        "hnsw" => format!("{{type=hnsw, m={}, ef_construction={}, flat_index_threshold={threshold}}}", int("m", 16), int("ef_construction", 100)),
        "int8_hnsw" | "int4_hnsw" | "bbq_hnsw" => format!(
            "{{type={ty}, m={}, ef_construction={}, on_disk_rescore={on_disk}, rescore_vector={}, flat_index_threshold={threshold}}}",
            int("m", 16),
            int("ef_construction", 100),
            rescore("none")
        ),
        "int4_flat" | "int8_flat" | "bbq_flat" => format!("{{type={ty}, rescore_vector={}}}", rescore("null")),
        // Elasticsearch prints the object's identity hash, which only that JVM knows.
        "flat" => "org.elasticsearch.index.mapper.vectors.DenseVectorFieldMapper$FlatIndexOptions@1b2c3d4e".to_string(),
        other => format!("{{type={other}}}"),
    }
}

/// `bfloat16` in `vectordb_document` indices, `float` elsewhere.
fn default_element_type(_f: &FieldMapper, ctx: &MappingCtx) -> Value {
    Value::from(if ctx.vectordb { "bfloat16" } else { "float" })
}

/// `vectordb_document` indices always print the element type.
fn ser_element_type(f: &FieldMapper, ctx: &MappingCtx) -> bool {
    ctx.vectordb || f.is_configured("element_type", ctx)
}

/// Vectors are indexed (HNSW) by default, except in strict columnar modes.
fn default_vector_index(_f: &FieldMapper, ctx: &MappingCtx) -> Value {
    Value::Bool(ctx.columnar.is_none())
}

/// Printed when set, or when the default is a quantized format (which Elasticsearch records).
fn ser_vector_index_options(f: &FieldMapper, ctx: &MappingCtx) -> bool {
    if f.is_configured("index_options", ctx) {
        return true;
    }
    let eff = f.effective("index_options", ctx);
    dense_vector_ser_indexed(f, ctx) && eff.get("type").and_then(Value::as_str).is_some_and(|t| t != "hnsw")
}

fn default_similarity(f: &FieldMapper, ctx: &MappingCtx) -> Value {
    if f.effective("index", ctx).as_bool() != Some(true) {
        return Value::Null;
    }
    match f.effective("element_type", ctx).as_str() {
        Some("bit") => Value::from("l2_norm"),
        _ => Value::from("cosine"),
    }
}

fn default_vector_index_options(f: &FieldMapper, ctx: &MappingCtx) -> Value {
    if f.effective("index", ctx).as_bool() != Some(true) {
        return Value::Null;
    }
    // Without dims the format is chosen when the first vector arrives.
    let Some(dims) = f.effective("dims", ctx).as_i64() else { return Value::Null };
    match f.effective("element_type", ctx).as_str() {
        Some("float") | Some("bfloat16") | None if dims >= 384 => crate::obj! { "type" => "bbq_hnsw", "m" => 16, "ef_construction" => 100, "rescore_vector" => crate::obj! { "oversample" => 3.0 } },
        Some("float") | Some("bfloat16") | None => crate::obj! { "type" => "int8_hnsw", "m" => 16, "ef_construction" => 100 },
        _ => crate::obj! { "type" => "hnsw", "m" => 16, "ef_construction" => 100 },
    }
}

pub static SPARSE_VECTOR: &[ParamDef] = &[p("store", Bool, TRUE, Fixed), META, p("index_options", Json, Dv::Fn(default_sparse_options), Updatable).nullable()];

fn default_sparse_options(_: &FieldMapper, _: &MappingCtx) -> Value {
    crate::obj! { "prune" => true, "pruning_config" => crate::obj! { "tokens_freq_ratio_threshold" => 5.0, "tokens_weight_threshold" => 0.4 } }
}

pub static RANK_FEATURE: &[ParamDef] =
    &[p("positive_score_impact", Bool, TRUE, Fixed), p("null_value", NullNumber(NumKind::Float), Dv::Null, Fixed).nullable(), META];

pub static RANK_FEATURES: &[ParamDef] = &[p("positive_score_impact", Bool, TRUE, Fixed), META];

pub static COMPLETION: &[ParamDef] = &[
    p("analyzer", Analyzer, Dv::Str("simple"), Fixed).ser(Ser::Always),
    p("search_analyzer", Analyzer, Dv::Param("analyzer"), Updatable),
    p("preserve_separators", Bool, TRUE, Fixed).ser(Ser::Always),
    p("preserve_position_increments", Bool, TRUE, Fixed).ser(Ser::Always),
    p("max_input_length", Int { min: 1, max: INT_MAX, msg: Some("[max_input_length] must be > 0 but was [{v}]") }, Dv::Int(50), Updatable).ser(Ser::Always),
    p("contexts", Json, Dv::Null, Fixed).nullable().ser(Ser::NonNull),
    META,
];

pub static SEARCH_AS_YOU_TYPE: &[ParamDef] = &[
    INDEX,
    STORE,
    p("doc_values", DocValues, FALSE, Fixed).ser(Ser::Always),
    p("max_shingle_size", Int { min: 2, max: 4, msg: Some("[max_shingle_size] must be at least [2] and at most [4], got [{v}]") }, Dv::Int(3), Fixed).ser(Ser::Always),
    ANALYZER,
    SEARCH_ANALYZER,
    SEARCH_QUOTE_ANALYZER,
    SIMILARITY,
    TEXT_INDEX_OPTIONS,
    p("norms", Bool, TRUE, TrueToFalse),
    TERM_VECTOR,
    META,
];

pub static TOKEN_COUNT: &[ParamDef] = &[
    INDEX,
    DOC_VALUES,
    STORE,
    p("analyzer", Analyzer, Dv::Null, Fixed).ser(Ser::Always),
    p("null_value", NullNumber(NumKind::Integer), Dv::Null, Fixed).nullable(),
    p("enable_position_increments", Bool, TRUE, Fixed),
    META,
];

pub static META_ONLY: &[ParamDef] = &[META];

/// `value` can be set once (on a field created without one), never changed or removed.
pub static CONSTANT_KEYWORD: &[ParamDef] =
    &[p("value", Str, Dv::Null, Merge::Check { check: set_once, show: crate::mapping::params::conflict_string }).nullable().ser(Ser::NonNull), META];

fn set_once(cur: &Value, new: &Value, _: &mut Vec<String>) -> bool {
    cur.is_null() || cur == new
}

pub static WILDCARD: &[ParamDef] = &[IGNORE_ABOVE, p("null_value", Str, Dv::Null, Fixed).nullable(), META];

pub static JOIN: &[ParamDef] = &[
    p("eager_global_ordinals", Bool, TRUE, Updatable).ser(Ser::Always),
    p("relations", Json, Dv::EmptyObj, Merge::Check { check: join_relations_merge, show: join_relations_string }).ser(Ser::Always),
];

/// `relations` as `(parent, children)` in the order given (`ParentJoinFieldMapper.Relations`).
fn join_relations(v: &Value) -> Vec<(String, Vec<String>)> {
    let Value::Obj(m) = v else { return Vec::new() };
    m.iter()
        .map(|(parent, c)| {
            let mut children: Vec<String> = match c {
                Value::Arr(a) => a.iter().map(Value::to_es_string).collect(),
                other => vec![other.to_es_string()],
            };
            children.dedup();
            (parent.clone(), children)
        })
        .collect()
}

/// `[Relations[parent=p, children=[c, d]], …]` (children are a `HashSet`).
fn join_relations_string(v: &Value) -> String {
    let parts: Vec<String> = join_relations(v)
        .into_iter()
        .map(|(parent, children)| {
            let order = crate::json::java_hash_order(&children.iter().collect::<Vec<_>>(), 16);
            let shown: Vec<&str> = order.into_iter().map(|i| children[i].as_str()).collect();
            format!("Relations[parent={parent}, children=[{}]]", shown.join(", "))
        })
        .collect();
    format!("[{}]", parts.join(", "))
}

/// `Joiner.canMerge`: parents and children can be added, never removed or moved.
fn join_relations_merge(cur: &Value, new: &Value, conflicts: &mut Vec<String>) -> bool {
    struct J {
        parents: Vec<String>,
        children: Vec<(String, String)>,
    }
    // Iterated in `HashMap` order, as the Joiner's maps are.
    fn joiner(v: &Value) -> J {
        let rel = join_relations(v);
        let mut parents: Vec<String> = Vec::new();
        let mut children: Vec<(String, String)> = Vec::new();
        for (p, cs) in &rel {
            if !parents.contains(p) {
                parents.push(p.clone());
            }
            for c in cs {
                if !children.iter().any(|(k, _)| k == c) {
                    children.push((c.clone(), p.clone()));
                }
            }
        }
        let po = crate::json::java_hash_order(&parents.iter().collect::<Vec<_>>(), 16);
        let co = crate::json::java_hash_order(&children.iter().map(|(k, _)| k).collect::<Vec<_>>(), 16);
        J { parents: po.into_iter().map(|i| parents[i].clone()).collect(), children: co.into_iter().map(|i| children[i].clone()).collect() }
    }
    let (a, b) = (joiner(cur), joiner(new));
    let parent_of = |j: &J, c: &str| j.children.iter().find(|(k, _)| k == c).map(|(_, p)| p.clone());
    let n = conflicts.len();
    let mut push = |m: String| conflicts.push(format!("Conflict in parameter [relations]: {m}"));
    for p in &a.parents {
        if !b.parents.contains(p) {
            push(format!("Cannot remove parent [{p}]"));
        }
    }
    for (c, _) in &a.children {
        if parent_of(&b, c).is_none() {
            push(format!("Cannot remove child [{c}]"));
        }
    }
    for p in &b.parents {
        if parent_of(&a, p).is_some() && !a.parents.contains(p) {
            push(format!("Cannot create parent [{p}] from an existing child"));
        }
    }
    for (c, _) in &b.children {
        if parent_of(&a, c).is_some() && parent_of(&a, c) != parent_of(&b, c) {
            push(format!("Cannot change parent of [{c}]"));
        }
        if a.parents.contains(c) && parent_of(&a, c) != parent_of(&b, c) {
            push(format!("Cannot create child [{c}] from an existing root"));
        }
    }
    conflicts.len() == n
}

pub static HISTOGRAM: &[ParamDef] = &[IGNORE_MALFORMED, COERCE, META, p("time_series_metric", Enum(&["histogram"]), Dv::Null, Fixed).nullable()];

pub static AGGREGATE_METRIC_DOUBLE: &[ParamDef] = &[
    IGNORE_MALFORMED,
    p("metrics", StrList, Dv::Null, Updatable).ser(Ser::Always),
    p("default_metric", Enum(&["min", "max", "sum", "value_count"]), Dv::Null, Updatable).ser(Ser::Always),
    META,
    p("time_series_metric", Enum(&["gauge"]), Dv::Null, Fixed).nullable(),
];

pub static COUNTED_KEYWORD: &[ParamDef] = &[META, INDEX];

pub static VERSION: &[ParamDef] = &[META];

macro_rules! def {
    ($name:expr, $params:expr, $family:expr) => {
        TypeDef { name: $name, params: $params, fields_at: usize::MAX, multi_fields: true, in_multi_field: true, validate: None, family: $family }
    };
}

/// Every field type, by name.
pub static TYPES: &[TypeDef] = &[
    TypeDef { validate: Some(validate_aggregate_metric), multi_fields: false, ..def!("aggregate_metric_double", AGGREGATE_METRIC_DOUBLE, Family::Numeric) },
    def!("binary", BINARY, Family::Binary),
    TypeDef { validate: Some(validate_scripted), ..def!("boolean", BOOLEAN, Family::Boolean) },
    def!("byte", BYTE, Family::Numeric).with_validate(validate_number),
    TypeDef { multi_fields: false, validate: Some(validate_completion), ..def!("completion", COMPLETION, Family::Other) },
    TypeDef { multi_fields: false, ..def!("constant_keyword", CONSTANT_KEYWORD, Family::Keyword) },
    TypeDef { multi_fields: false, ..def!("counted_keyword", COUNTED_KEYWORD, Family::Keyword) },
    def!("date", DATE, Family::Date).with_validate(validate_date),
    def!("date_nanos", DATE_NANOS, Family::Date).with_validate(validate_date),
    def!("date_range", DATE_RANGE, Family::Range),
    TypeDef { multi_fields: false, in_multi_field: false, validate: Some(validate_dense_vector), ..def!("dense_vector", DENSE_VECTOR, Family::Vector) },
    def!("double", DOUBLE, Family::Numeric).with_validate(validate_number),
    def!("double_range", RANGE, Family::Range).with_validate(validate_range),
    TypeDef { multi_fields: false, ..def!("exponential_histogram", HISTOGRAM, Family::Other) },
    TypeDef { multi_fields: false, validate: Some(validate_flattened), ..def!("flattened", FLATTENED, Family::Other) },
    def!("float", FLOAT, Family::Numeric).with_validate(validate_number),
    def!("float_range", RANGE, Family::Range).with_validate(validate_range),
    def!("geo_point", GEO_POINT, Family::Geo).with_validate(validate_scripted),
    def!("geo_shape", GEO_SHAPE, Family::Geo),
    def!("half_float", HALF_FLOAT, Family::Numeric).with_validate(validate_number),
    TypeDef { multi_fields: false, ..def!("histogram", HISTOGRAM, Family::Other) },
    def!("integer", INTEGER, Family::Numeric).with_validate(validate_number),
    def!("integer_range", RANGE, Family::Range).with_validate(validate_range),
    def!("ip", IP, Family::Ip).with_validate(validate_scripted),
    def!("ip_range", RANGE, Family::Range).with_validate(validate_range),
    TypeDef { multi_fields: false, in_multi_field: false, validate: Some(validate_join), ..def!("join", JOIN, Family::Other) },
    TypeDef { validate: Some(validate_keyword), ..def!("keyword", KEYWORD, Family::Keyword) },
    def!("long", LONG, Family::Numeric).with_validate(validate_number),
    def!("long_range", RANGE, Family::Range).with_validate(validate_range),
    def!("match_only_text", MATCH_ONLY_TEXT, Family::Text),
    TypeDef { multi_fields: false, in_multi_field: false, ..def!("percolator", META_ONLY, Family::Other) },
    def!("point", POINT, Family::Geo),
    TypeDef { multi_fields: false, ..def!("rank_feature", RANK_FEATURE, Family::Other) },
    TypeDef { multi_fields: false, ..def!("rank_features", RANK_FEATURES, Family::Other) },
    def!("scaled_float", SCALED_FLOAT, Family::Numeric).with_validate(validate_number),
    TypeDef { multi_fields: false, ..def!("search_as_you_type", SEARCH_AS_YOU_TYPE, Family::Text) },
    def!("shape", SHAPE, Family::Geo),
    def!("short", SHORT, Family::Numeric).with_validate(validate_number),
    TypeDef { multi_fields: false, in_multi_field: false, ..def!("sparse_vector", SPARSE_VECTOR, Family::Vector) },
    TypeDef { fields_at: 3, validate: Some(validate_text), ..def!("text", TEXT, Family::Text) },
    TypeDef { validate: Some(validate_token_count), ..def!("token_count", TOKEN_COUNT, Family::Numeric) },
    TypeDef { validate: Some(validate_unsigned_long), ..def!("unsigned_long", UNSIGNED_LONG, Family::Numeric) },
    def!("version", VERSION, Family::Keyword),
    def!("wildcard", WILDCARD, Family::Keyword),
];

/// OpenSearch's `flat_object`: a JSON object stored as keyword values (`<field>._value`) and
/// path-value pairs (`<field>._valueAndPath`); it takes no parameters.
pub static OS_FLAT_OBJECT: &[ParamDef] = &[];

/// OpenSearch's `wildcard`: `normalizer` is any name, `doc_values` is always printed.
pub static OS_WILDCARD: &[ParamDef] = &[
    p("null_value", Str, Dv::Null, Fixed).nullable(),
    p("ignore_above", Int { min: i32::MIN as i64, max: INT_MAX, msg: None }, Dv::Int(INT_MAX), Updatable),
    p("normalizer", Str, Dv::Str("default"), Fixed),
    p("doc_values", Bool, TRUE, Fixed).ser(Ser::Always),
    META,
];

/// OpenSearch's `match_only_text`: text parameters, with positions, norms, phrases and prefixes
/// refused (`validate_os_match_only_text`).
pub static OS_MATCH_ONLY_TEXT: &[ParamDef] = &[
    INDEX,
    STORE,
    META,
    p("index_options", Enum(&["docs"]), Dv::Str("docs"), Fixed),
    TERM_VECTOR,
    p("norms", Bool, FALSE, TrueToFalse),
    ANALYZER,
    SEARCH_ANALYZER,
    SEARCH_QUOTE_ANALYZER,
    SIMILARITY,
    EAGER_GLOBAL_ORDINALS,
    p("position_increment_gap", Int { min: 0, max: INT_MAX, msg: Some("[positions_increment_gap] must be positive, got [{v}]") }, Dv::Int(-1), Fixed),
    p("fielddata", Bool, FALSE, Updatable),
    p("fielddata_frequency_filter", FreqFilter, Dv::Fn(default_freq_filter), Updatable),
    p("index_prefixes", IndexPrefixes, Dv::Null, Fixed).nullable(),
    p("index_phrases", Bool, FALSE, Fixed),
];

/// OpenSearch's `xy_point` (geo module).
pub static OS_XY_POINT: &[ParamDef] =
    &[INDEX, DOC_VALUES, STORE, META, IGNORE_MALFORMED, IGNORE_Z, p("null_value", Json, Dv::Null, Fixed).nullable()];

/// OpenSearch's `xy_shape` (geo module).
pub static OS_XY_SHAPE: &[ParamDef] = &[
    INDEX,
    DOC_VALUES_OFF,
    STORE,
    META,
    IGNORE_MALFORMED,
    IGNORE_Z,
    p("coerce", Bool, FALSE, Updatable).ser(Ser::WhenSet),
    ORIENTATION,
];

/// OpenSearch's `constant_keyword`: `value` is required and can't change.
pub static OS_CONSTANT_KEYWORD: &[ParamDef] = &[p("value", Str, Dv::Null, Fixed).nullable().ser(Ser::NonNull), META];

/// Types whose OpenSearch definition differs from Elasticsearch's, or that only OpenSearch has.
pub static OS_TYPES: &[TypeDef] = &[
    TypeDef { multi_fields: false, validate: Some(validate_os_constant_keyword), ..def!("constant_keyword", OS_CONSTANT_KEYWORD, Family::Keyword) },
    TypeDef { multi_fields: false, in_multi_field: false, ..def!("flat_object", OS_FLAT_OBJECT, Family::Other) },
    TypeDef { validate: Some(validate_os_match_only_text), ..def!("match_only_text", OS_MATCH_ONLY_TEXT, Family::Text) },
    def!("wildcard", OS_WILDCARD, Family::Keyword),
    def!("xy_point", OS_XY_POINT, Family::Geo),
    def!("xy_shape", OS_XY_SHAPE, Family::Geo),
];

/// Elasticsearch types OpenSearch doesn't have (x-pack, or newer than the fork).
const NOT_IN_OPENSEARCH: &[&str] = &[
    "aggregate_metric_double",
    "counted_keyword",
    "dense_vector",
    "exponential_histogram",
    "flattened",
    "histogram",
    "point",
    "shape",
    "sparse_vector",
    "version",
];

/// The type named `name` in a dialect.
pub fn lookup_in(dialect: crate::cluster::Dialect, name: &str) -> Option<&'static TypeDef> {
    if dialect == crate::cluster::Dialect::OpenSearch {
        if let Ok(i) = OS_TYPES.binary_search_by(|t| t.name.cmp(name)) {
            return Some(&OS_TYPES[i]);
        }
        if NOT_IN_OPENSEARCH.contains(&name) {
            return None;
        }
    }
    lookup(name)
}

/// The types whose OpenSearch mappers print `boost` when it isn't 1 (the others accept and
/// drop it).
pub fn os_prints_boost(type_name: &str) -> bool {
    matches!(type_name, "text" | "match_only_text" | "keyword" | "date" | "date_nanos" | "boolean" | "xy_point" | "xy_shape")
}

fn validate_os_match_only_text(f: &FieldMapper, ctx: &MappingCtx) -> Result<(), EsError> {
    if f.effective("norms", ctx).as_bool() == Some(true) {
        return Err(EsError::mapper_parsing("Norms cannot be enabled on for match_only_text field"));
    }
    if f.effective("index_phrases", ctx).as_bool() == Some(true) {
        return Err(EsError::mapper_parsing("Index phrases cannot be enabled on for match_only_text field. Use text field instead"));
    }
    if f.raw("index_prefixes").is_some_and(|v| !v.is_null()) {
        return Err(EsError::mapper_parsing("Index prefixes cannot be enabled on for match_only_text field. Use text field instead"));
    }
    Ok(())
}

fn validate_os_constant_keyword(f: &FieldMapper, ctx: &MappingCtx) -> Result<(), EsError> {
    if f.effective("value", ctx).is_null() {
        return Err(EsError::parse(format!("Field [{}] is missing required parameter [value]", f.name)));
    }
    Ok(())
}

impl TypeDef {
    const fn with_validate(mut self, f: fn(&FieldMapper, &MappingCtx) -> Result<(), EsError>) -> TypeDef {
        self.validate = Some(f);
        self
    }

    pub fn param_index(&self, name: &str) -> Option<usize> {
        self.params.iter().position(|p| p.name == name)
    }
}

pub fn lookup(name: &str) -> Option<&'static TypeDef> {
    TYPES.binary_search_by(|t| t.name.cmp(name)).ok().map(|i| &TYPES[i])
}

/// Types Elasticsearch knows but roll search refuses, with the reason.
pub fn unsupported(name: &str) -> Option<&'static str> {
    match name {
        "semantic_text" => Some("semantic_text fields need an inference service, which roll search doesn't provide"),
        "rank_vectors" => Some("rank_vectors fields are not supported by roll search"),
        "pattern_text" => Some("pattern_text fields are not supported by roll search"),
        _ => None,
    }
}

// --- Validation hooks (messages from Elasticsearch). ---

fn precludes(f: &FieldMapper, ctx: &MappingCtx, param: &str, others: &[&str]) -> Result<(), EsError> {
    if f.is_configured(param, ctx) {
        for o in others {
            if f.is_configured(o, ctx) {
                return Err(EsError::illegal_argument(format!("Field [{o}] cannot be set in conjunction with field [{param}]")));
            }
        }
    }
    Ok(())
}

fn requires(f: &FieldMapper, ctx: &MappingCtx, param: &str, other: &str) -> Result<(), EsError> {
    if f.is_configured(param, ctx) && !f.is_configured(other, ctx) {
        return Err(EsError::illegal_argument(format!("Field [{param}] requires field [{other}] to be configured")));
    }
    Ok(())
}

fn validate_scripted(f: &FieldMapper, ctx: &MappingCtx) -> Result<(), EsError> {
    if f.def().param_index("on_script_error").is_some() {
        requires(f, ctx, "on_script_error", "script")?;
    }
    if !f.effective("script", ctx).is_null() {
        let indexed = f.effective("index", ctx).as_bool().unwrap_or(true);
        let dv = f.effective("doc_values", ctx).as_bool().unwrap_or(true);
        if !indexed && !dv {
            return Err(EsError::mapper_parsing("Cannot define script on field with index:false and doc_values:false"));
        }
        if !f.fields.is_empty() {
            return Err(EsError::mapper_parsing("Cannot define multifields on a field with a script"));
        }
        if !f.copy_to.is_empty() {
            return Err(EsError::mapper_parsing("Cannot define copy_to parameter on a field with a script"));
        }
    }
    Ok(())
}

fn validate_number(f: &FieldMapper, ctx: &MappingCtx) -> Result<(), EsError> {
    validate_scripted(f, ctx)?;
    if f.def().param_index("script").is_some() {
        precludes(f, ctx, "script", &["ignore_malformed", "coerce", "null_value"])?;
    }
    precludes(f, ctx, "time_series_metric", &["time_series_dimension"])?;
    if f.ty == "scaled_float" && f.effective("scaling_factor", ctx).is_null() {
        return Err(EsError::illegal_argument("Field [scaling_factor] is required"));
    }
    Ok(())
}

fn validate_unsigned_long(f: &FieldMapper, ctx: &MappingCtx) -> Result<(), EsError> {
    precludes(f, ctx, "time_series_metric", &["time_series_dimension"])
}

fn validate_date(f: &FieldMapper, ctx: &MappingCtx) -> Result<(), EsError> {
    validate_scripted(f, ctx)?;
    precludes(f, ctx, "script", &["null_value", "ignore_malformed"])?;
    Ok(())
}

fn validate_keyword(f: &FieldMapper, ctx: &MappingCtx) -> Result<(), EsError> {
    validate_scripted(f, ctx)?;
    precludes(f, ctx, "script", &["null_value"])?;
    precludes(f, ctx, "time_series_dimension", &["normalizer"])?;
    Ok(())
}

fn validate_text(f: &FieldMapper, ctx: &MappingCtx) -> Result<(), EsError> {
    let indexed = f.effective("index", ctx).as_bool().unwrap_or(true);
    if !indexed {
        for p in ["index_prefixes", "index_phrases"] {
            if f.is_configured(p, ctx) {
                return Err(EsError::illegal_argument(format!("Cannot set {p} on unindexed field [{}]", f.name)));
            }
        }
    }
    let opts = f.effective("index_options", ctx);
    if f.effective("index_phrases", ctx).as_bool() == Some(true) && matches!(opts.as_str(), Some("docs") | Some("freqs")) {
        return Err(EsError::illegal_argument(format!("Cannot set index_phrases on field [{}] if positions are not enabled", f.name)));
    }
    Ok(())
}

fn validate_token_count(f: &FieldMapper, ctx: &MappingCtx) -> Result<(), EsError> {
    if f.effective("analyzer", ctx).is_null() {
        return Err(EsError::mapper_parsing(format!("Analyzer must be set for field [{}] but wasn't.", f.name)));
    }
    Ok(())
}

fn validate_completion(f: &FieldMapper, ctx: &MappingCtx) -> Result<(), EsError> {
    let _ = (f, ctx);
    Ok(())
}

/// `RangeFieldMapper.Builder.build`: a format means nothing outside date ranges.
fn validate_range(f: &FieldMapper, _ctx: &MappingCtx) -> Result<(), EsError> {
    if f.raw("format").is_some_and(|v| !v.is_null()) {
        return Err(EsError::illegal_argument(format!("field [{}] of type [range] should not define a dateTimeFormatter unless it is a DATE type", f.name)));
    }
    Ok(())
}

fn validate_dense_vector(f: &FieldMapper, ctx: &MappingCtx) -> Result<(), EsError> {
    let indexed = f.effective("index", ctx).as_bool() == Some(true);
    if !indexed {
        for p in ["similarity", "index_options"] {
            if f.is_set(p) && !f.raw(p).is_some_and(Value::is_null) {
                return Err(EsError::illegal_argument(format!("Field [{p}] can only be specified for a field of type [dense_vector] when it is indexed")));
            }
        }
    }
    let bit = f.effective("element_type", ctx).as_str() == Some("bit");
    if let Some(d) = f.raw("dims").and_then(Value::as_i64) {
        // `dims`' validator: bits come in bytes, up to 4096 bytes.
        let (min, max) = if bit { (8, 32768) } else { (1, 4096) };
        if d < min || d > max {
            return Err(EsError::mapper_parsing(format!("The number of dimensions should be in the range [{min}, {max}] but was [{d}]")));
        }
        if bit && d % 8 != 0 {
            return Err(EsError::mapper_parsing(format!("The number of dimensions for should be a multiple of 8 but was [{d}]")));
        }
    }
    if bit
        && let Some(s) = f.raw("similarity").and_then(Value::as_str)
        && s != "l2_norm"
    {
        return Err(EsError::illegal_argument(format!("The [{s}] similarity is not supported for [element_type] [bit]")));
    }
    if let Some(Value::Obj(opts)) = f.raw("index_options")
        && let Some(ty) = opts.get("type").and_then(Value::as_str)
    {
        // `validateElementType` then `validateDimension`.
        let element = f.effective("element_type", ctx);
        let element = element.as_str().unwrap_or("float");
        let float_only = !matches!(ty, "hnsw" | "flat");
        if float_only && !matches!(element, "float" | "bfloat16") {
            return Err(EsError::illegal_argument(format!("[element_type] cannot be [{element}] when using index type [{ty}]")));
        }
        if let Some(d) = f.effective("dims", ctx).as_i64() {
            if matches!(ty, "int4_hnsw" | "int4_flat") && d % 2 != 0 {
                return Err(EsError::illegal_argument(format!("{ty} only supports even dimensions; provided={d}")));
            }
            if matches!(ty, "bbq_hnsw" | "bbq_flat") && d < 64 {
                return Err(EsError::illegal_argument(format!("{ty} does not support dimensions fewer than 64; provided={d}")));
            }
        }
    }
    Ok(())
}

/// `passthrough: {priority: n}` makes the mapped sub-fields resolvable from the root.
fn validate_flattened(f: &FieldMapper, _: &MappingCtx) -> Result<(), EsError> {
    let Some(v) = f.raw("passthrough").filter(|v| !v.is_null()) else { return Ok(()) };
    let Value::Obj(m) = v else {
        return Err(EsError::mapper_parsing(format!(
            "[passthrough] must be an object with a [priority] field, got [{}]",
            super::params::conflict_string(v)
        )));
    };
    let Some(p) = m.get("priority") else { return Err(EsError::mapper_parsing("[passthrough] requires a [priority] field")) };
    let priority = super::params::parse_number(NumKind::Integer, p, true).map_err(|e| EsError::illegal_argument(e.reason().to_string()))?;
    if priority.as_i64().is_some_and(|n| n < 0) {
        return Err(EsError::illegal_argument(format!("[passthrough.priority] must be non-negative, got [{}]", priority.to_es_string())));
    }
    Ok(())
}

fn validate_join(f: &FieldMapper, _: &MappingCtx) -> Result<(), EsError> {
    if let Some(rel) = f.raw("relations")
        && !matches!(rel, Value::Obj(_))
    {
        return Err(EsError::mapper_parsing(format!("[relations] must be an object on field [{}]", f.name)));
    }
    Ok(())
}

fn validate_aggregate_metric(f: &FieldMapper, ctx: &MappingCtx) -> Result<(), EsError> {
    let metrics = f.effective("metrics", ctx);
    let Some(list) = metrics.as_array().filter(|l| !l.is_empty()) else {
        return Err(EsError::illegal_argument("Property [metrics] is required for field [".to_string() + &f.name + "]."));
    };
    let names: Vec<String> = list.iter().map(Value::to_es_string).collect();
    for m in &names {
        if !matches!(m.as_str(), "min" | "max" | "sum" | "value_count") {
            return Err(EsError::mapper_parsing(format!("Metric [{m}] is not supported.")));
        }
    }
    match f.effective("default_metric", ctx).as_str() {
        Some(d) if !names.iter().any(|m| m == d) => {
            return Err(EsError::illegal_argument(format!("Default metric [{d}] is not defined in the metrics of field [{}].", f.name)));
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_sorted_and_unique_params() {
        for w in TYPES.windows(2) {
            assert!(w[0].name < w[1].name, "{} >= {}", w[0].name, w[1].name);
        }
        for t in TYPES {
            let mut names: Vec<&str> = t.params.iter().map(|p| p.name).collect();
            names.sort();
            names.dedup();
            assert_eq!(names.len(), t.params.len(), "duplicate params on {}", t.name);
        }
    }

    #[test]
    fn unused_defaults_compile() {
        let _ = (SettingDefault::Coerce, Dv::Float(0.0));
    }
}
