//! Sorting (`SortBuilder.buildSortField`): `_score`, `_doc`, field sorts with `order`, `mode`,
//! `missing`, `unmapped_type`, `numeric_type`, `nested`, `format`; `_geo_distance` and `_script`.
//! Sort values are rendered as Elasticsearch prints them (`DocValueFormat.formatSortValue`):
//! booleans as booleans, IPs as strings, keywords as strings, dates as epoch numbers unless a
//! `format` is given.

use super::query::QueryCtx;
use crate::engine::fields::{FieldInfo, Kind};
use crate::error::{EsError, EsResult};
use crate::json::{Number, Value};
use crate::search_source::{Sort, SortOrder};
use search_codec::points::encode;
use search_index::collector::top_field::{FieldSort, GeoDistanceSort, GeoDistanceType, Missing, NestedSort, SortKey, SortMode, SortType, SortValue};
use search_index::query::TermQuery;
use std::sync::Arc;

/// How a sort value prints.
#[derive(Clone, Debug)]
pub enum ValueFormat {
    Score,
    Doc,
    /// `_shard_doc` (`ShardDocSortField`): the shard request index in the high 32 bits, the doc
    /// in the low ones. Shards encode their hits' values (`encode_shard_doc`).
    ShardDoc,
    Long,
    Double,
    Float,
    Boolean,
    Ip,
    Keyword,
    /// A `version` field's encoded terms (decoded for display, encoded from `search_after`).
    Version,
    /// A `string` / `version` script sort: shard-global ordinals of the script values (bytes
    /// once rescaled).
    ScriptTerms(Arc<super::script_terms_sort::ScriptTerms>),
    UnsignedLong,
    /// `unsigned_long` under `numeric_type: double` (`DoubleValuesComparatorSource`): the
    /// shard sorts the sign-flipped longs (the same order), values print as doubles and a
    /// missing value as an infinity.
    UnsignedLongDouble,
    /// Epoch millis / nanos (`nanos`, the unit sort values are reported in), formatted when
    /// `format` is given. `field_nanos` is the unit the shard's doc values hold (they differ
    /// under `numeric_type`); `parser` reads string `search_after` values (the sort `format`,
    /// else the field's).
    Date { nanos: bool, field_nanos: bool, format: Option<Arc<crate::time::DateFormatter>>, parser: Arc<crate::time::DateFormatter> },
    HalfFloat,
    ScaledFloat(f64),
    /// A scaled float under `numeric_type: long | date`: the shard sorts its scaled longs,
    /// [`rescale`] truncates them to the longs Elasticsearch sorts and prints
    /// (`FieldData.castToLong` over the scaled doubles).
    ScaledLong(f64),
    /// Missing everywhere (`unmapped_type`).
    Unmapped(Option<Kind>),
    /// `_tsid` (hex doc values printed base64url).
    Tsid,
}

/// A built sort: engine keys and how to print their values.
#[derive(Clone, Debug, Default)]
pub struct BuiltSort {
    pub keys: Vec<SortKey>,
    pub formats: Vec<ValueFormat>,
    /// Whether the sort is just `_score` desc (plain top hits).
    pub by_score: bool,
}

fn no_mapping(ctx: &QueryCtx, field: &str) -> EsError {
    super::query::qse(ctx, format!("No mapping found for [{field}] in order to sort on"))
}

/// A field sort's `mode`: `FieldSortBuilder.build` refuses the numeric-only modes on other
/// field types with a `QueryShardException`.
fn field_mode(ctx: &QueryCtx, m: Option<&str>, reverse: bool, numeric: bool) -> EsResult<SortMode> {
    if !numeric && matches!(m, Some("sum" | "avg" | "median")) {
        return Err(super::query::qse(ctx, "we only support AVG, MEDIAN and SUM on number based fields"));
    }
    mode_of(m, reverse, numeric)
}

fn mode_of(m: Option<&str>, reverse: bool, numeric: bool) -> EsResult<SortMode> {
    Ok(match m {
        None => {
            if reverse {
                SortMode::Max
            } else {
                SortMode::Min
            }
        }
        Some("min") => SortMode::Min,
        Some("max") => SortMode::Max,
        Some("sum") if numeric => SortMode::Sum,
        Some("avg") if numeric => SortMode::Avg,
        Some("median") if numeric => SortMode::Median,
        Some(s @ ("sum" | "avg" | "median")) => return Err(EsError::illegal_argument(format!("Sort mode [{s}] is not supported for this field type"))),
        Some(other) => return Err(EsError::illegal_argument(format!("Unknown SortMode [{other}]"))),
    })
}

/// Builds the sort of a request.
pub fn build(ctx: &QueryCtx, sorts: &[Sort]) -> EsResult<BuiltSort> {
    let mut out = BuiltSort::default();
    if sorts.is_empty() {
        out.by_score = true;
        return Ok(out);
    }
    for s in sorts {
        match s {
            Sort::Score { order } => {
                out.keys.push(SortKey::Score { reverse: *order == SortOrder::Asc });
                out.formats.push(ValueFormat::Score);
            }
            Sort::Field { field, order, missing, unmapped_type, mode, numeric_type, format, nested } => {
                if field == "_doc" {
                    out.keys.push(SortKey::Doc { reverse: *order == Some(SortOrder::Desc) });
                    out.formats.push(ValueFormat::Doc);
                    continue;
                }
                if field == "_shard_doc" {
                    out.keys.push(SortKey::Doc { reverse: *order == Some(SortOrder::Desc) });
                    out.formats.push(ValueFormat::ShardDoc);
                    continue;
                }
                if field == "_score" {
                    out.keys.push(SortKey::Score { reverse: *order == Some(SortOrder::Asc) });
                    out.formats.push(ValueFormat::Score);
                    continue;
                }
                let reverse = *order == Some(SortOrder::Desc);
                if field == "_tsid" && crate::engine::tsdb::is_time_series(ctx.idx) {
                    out.keys.push(SortKey::Field(FieldSort {
                        field: crate::engine::fields::TSID_FIELD,
                        kind: SortType::Keyword,
                        reverse,
                        missing: keyword_missing(missing.as_ref())?,
                        mode: field_mode(ctx, mode.as_deref(), reverse, false)?,
                        nested: None,
                    }));
                    out.formats.push(ValueFormat::Tsid);
                    continue;
                }
                if field == "_ts_routing_hash" && crate::engine::tsdb::is_time_series(ctx.idx) {
                    out.keys.push(SortKey::Field(FieldSort {
                        field: crate::engine::fields::TS_ROUTING_HASH_FIELD,
                        kind: SortType::Keyword,
                        reverse,
                        missing: Missing::Last,
                        mode: SortMode::Min,
                        nested: None,
                    }));
                    out.formats.push(ValueFormat::Keyword);
                    continue;
                }
                if field == "_seq_no" {
                    ctx.check_seq_no_queryable()?;
                }
                // `_index` (`ConstantIndexFieldData`) and `constant_keyword` fields hold one value
                // per shard: no doc has doc values, and every doc sorts as that value.
                let constant = if field == "_index" {
                    Some(Some(ctx.idx.name.clone()))
                } else {
                    ctx.field(field).filter(|f| f.kind == Kind::ConstantKeyword).map(|f| f.constant_value.clone())
                };
                if let Some(value) = constant {
                    out.keys.push(SortKey::Field(FieldSort {
                        field: u32::MAX,
                        kind: SortType::Keyword,
                        reverse,
                        missing: match value {
                            Some(v) => Missing::Value(SortValue::Bytes(v.into_bytes())),
                            None => keyword_missing(missing.as_ref())?,
                        },
                        mode: field_mode(ctx, mode.as_deref(), reverse, false)?,
                        nested: None,
                    }));
                    out.formats.push(ValueFormat::Keyword);
                    continue;
                }
                if let Some(rf) = ctx.runtime_field(field) {
                    use super::runtime::{RtType, SortMissing};
                    if rf.ty == RtType::GeoPoint {
                        return Err(EsError::illegal_argument("can't sort on geo_point field without using specific sorting feature, like geo_distance"));
                    }
                    let bytes = matches!(rf.ty, RtType::Keyword | RtType::Ip);
                    let sort_mode = field_mode(ctx, mode.as_deref(), reverse, !bytes)?;
                    let missing = match missing.as_ref() {
                        None => SortMissing::Last,
                        Some(Value::Str(s)) if s == "_last" => SortMissing::Last,
                        Some(Value::Str(s)) if s == "_first" => SortMissing::First,
                        Some(v) if rf.ty == RtType::Ip => {
                            let text = super::query::value_text(v);
                            SortMissing::Bytes(crate::document::values::parse_ip(&text).ok_or_else(|| EsError::illegal_argument(format!("'{text}' is not an IP string literal.")))?.to_vec())
                        }
                        Some(v) if bytes => SortMissing::Bytes(super::query::value_text(v).into_bytes()),
                        Some(Value::Num(n)) => SortMissing::Number(n.as_f64()),
                        Some(v) => {
                            let text = super::query::value_text(v);
                            SortMissing::Number(text.parse::<f64>().map_err(|_| EsError::number_format(&text))?)
                        }
                    };
                    let ev = Arc::new(super::runtime::Evaluator::new(&ctx.cluster.scripts, ctx.st, ctx.table, ctx.shard as i32, &ctx.idx.name, rf)?.counted());
                    let (key, format) = super::runtime::sort_key(ev, ctx.searcher.clone(), reverse, sort_mode, missing);
                    out.keys.push(key);
                    out.formats.push(format);
                    continue;
                }
                let Some(f) = ctx.field(field) else {
                    if field == "_id" && crate::engine::tsdb::is_time_series(ctx.idx) {
                        return Err(EsError::illegal_argument("Fielddata is not supported on [_id] field in [time_series] indices"));
                    }
                    if field == "_id" {
                        if !super::id_fielddata::enabled(ctx.st) {
                            return Err(super::id_fielddata::disabled());
                        }
                        out.keys.push(SortKey::Field(FieldSort {
                            field: super::id_fielddata::FIELD,
                            kind: SortType::Keyword,
                            reverse,
                            missing: keyword_missing(missing.as_ref())?,
                            mode: field_mode(ctx, mode.as_deref(), reverse, false)?,
                            nested: None,
                        }));
                        out.formats.push(ValueFormat::Keyword);
                        continue;
                    }
                    match unmapped_type {
                        Some(t) => {
                            out.keys.push(unmapped_key(ctx, Kind::of(t), reverse, missing.as_ref())?);
                            out.formats.push(ValueFormat::Unmapped(Some(Kind::of(t))));
                            continue;
                        }
                        None => return Err(no_mapping(ctx, field)),
                    }
                };
                if f.kind == Kind::AggregateMetric {
                    match super::aggregate_metric::sort_key(&f, reverse, missing.as_ref())? {
                        Ok(key) => {
                            out.keys.push(key);
                            out.formats.push(ValueFormat::Double);
                        }
                        Err(m) => {
                            out.keys.push(field_key(ctx, &m, reverse, Some(&Value::from(0.0)), mode.as_deref(), numeric_type.as_deref(), nested.as_ref())?);
                            out.formats.push(ValueFormat::Double);
                        }
                    }
                    continue;
                }
                // A keyed `flattened` sub-field sorts by its key's values, the key stripped
                // (`KeyedFlattenedFieldData`), not by the raw `key\0value` terms.
                if f.kind == Kind::Flattened && f.doc_values && !crate::opensearch::active()
                    && let Some(k) = &f.flattened_key
                {
                    use super::runtime::SortMissing;
                    let missing = match missing.as_ref() {
                        None => SortMissing::Last,
                        Some(Value::Str(s)) if s == "_last" => SortMissing::Last,
                        Some(Value::Str(s)) if s == "_first" => SortMissing::First,
                        Some(v) => SortMissing::Bytes(super::query::value_text(v).into_bytes()),
                    };
                    let sort_mode = field_mode(ctx, mode.as_deref(), reverse, false)?;
                    let terms = Arc::new(super::script_terms_sort::ScriptTerms::keyed(f.number, k, ctx.searcher.clone(), reverse, sort_mode, missing));
                    let sc = Arc::new(super::script_terms_sort::TermsSortScript(terms.clone()));
                    out.keys.push(SortKey::Script(search_index::collector::top_field::ScriptSort { script: sc, reverse, mode: sort_mode, nested: None }));
                    out.formats.push(ValueFormat::ScriptTerms(terms));
                    continue;
                }
                let key = field_key(ctx, &f, reverse, missing.as_ref(), mode.as_deref(), numeric_type.as_deref(), nested.as_ref())?;
                out.formats.push(field_format(&f, format.as_deref(), numeric_type.as_deref())?);
                out.keys.push(key);
            }
            Sort::Script { script, ty, order, mode, nested } => {
                let reverse = *order == Some(SortOrder::Desc);
                if ty == "string" || ty == "version" {
                    let p = super::extra::painless(ctx, script, if ty == "string" { search_painless::Context::StringSort } else { search_painless::Context::BytesRefSort })?;
                    let terms = Arc::new(super::script_terms_sort::ScriptTerms::new(p, ctx.searcher.clone(), ty == "version", reverse));
                    let sc = Arc::new(super::script_terms_sort::TermsSortScript(terms.clone()));
                    out.keys.push(SortKey::Script(search_index::collector::top_field::ScriptSort {
                        script: sc,
                        reverse,
                        mode: search_index::collector::top_field::SortMode::Min,
                        nested: None,
                    }));
                    out.formats.push(ValueFormat::ScriptTerms(terms));
                    continue;
                }
                let key = super::extra::script_sort(ctx, script, ty, reverse, mode.as_deref(), nested.as_ref())?;
                out.formats.push(if ty == "string" { ValueFormat::Keyword } else { ValueFormat::Double });
                out.keys.push(key);
            }
            Sort::GeoDistance(m) => {
                let (key, _) = geo_distance(ctx, m)?;
                out.keys.push(key);
                out.formats.push(ValueFormat::Double);
            }
        }
    }
    out.by_score = matches!(out.keys.as_slice(), [SortKey::Score { reverse: false }]);
    Ok(out)
}

/// Whether a request's sort is only `_score` descending (OpenSearch then has no sort, as
/// `SortAndFormats` is null for it).
pub fn build_is_score_only(sorts: &[Sort]) -> bool {
    match sorts {
        [Sort::Score { order }] => *order != SortOrder::Asc,
        _ => false,
    }
}

fn sort_type(f: &FieldInfo) -> Option<SortType> {
    Some(match f.kind {
        // OpenSearch computes `sum` / `avg` / `median` of unsigned longs in unsigned arithmetic.
        Kind::UnsignedLong if crate::opensearch::active() => SortType::UnsignedLong,
        Kind::Long | Kind::Date | Kind::DateNanos | Kind::Boolean | Kind::ScaledFloat(_) | Kind::UnsignedLong => SortType::Long,
        Kind::Integer | Kind::Short | Kind::Byte | Kind::TokenCount => SortType::Int,
        Kind::Double => SortType::Double,
        Kind::Float => SortType::Float,
        Kind::HalfFloat => SortType::HalfFloat,
        Kind::Keyword | Kind::Ip | Kind::Version | Kind::Wildcard | Kind::ConstantKeyword | Kind::Flattened | Kind::CountedKeyword | Kind::Join => SortType::Keyword,
        _ => return None,
    })
}

#[allow(clippy::too_many_arguments)]
fn field_key(ctx: &QueryCtx, f: &FieldInfo, reverse: bool, missing: Option<&Value>, mode: Option<&str>, numeric_type: Option<&str>, nested: Option<&crate::search_source::NestedSort>) -> EsResult<SortKey> {
    if f.kind.is_text() {
        if let Some(fd) = f.subfield("fielddata") {
            return Ok(SortKey::Field(FieldSort { field: fd, kind: SortType::Keyword, reverse, missing: keyword_missing(missing)?, mode: field_mode(ctx, mode, reverse, false)?, nested: None }));
        }
        return Err(EsError::illegal_argument(crate::search::aggs::fielddata_disabled_in(ctx.idx.dialect == crate::cluster::Dialect::OpenSearch, &f.name, &ctx.idx.name)));
    }
    if !f.doc_values {
        return Err(EsError::illegal_argument(format!("Can't load fielddata on [{}] because fielddata is unsupported on fields of type [{}]. Use doc values instead.", f.name, f.ty)));
    }
    // OpenSearch's `FlatObjectFieldType.docValueFormat` refuses the root field.
    if f.ty == "flat_object" && f.flattened_key.is_none() {
        return Err(EsError::illegal_argument(format!("Field [{}] of type [flat_object] does not support doc_value in root field", f.name)));
    }
    let Some(mut kind) = sort_type(f) else {
        // OpenSearch's `AbstractLatLonPointIndexFieldData.sortField`.
        if f.kind == Kind::GeoPoint && ctx.idx.dialect == crate::cluster::Dialect::OpenSearch {
            return Err(EsError::illegal_argument("can't sort on geo_point field without using specific sorting feature, like geo_distance"));
        }
        // The histogram field data's `sortField`.
        if f.kind == Kind::Histogram {
            return Err(EsError::illegal_argument(format!("can't sort on the [{}] field", f.ty)));
        }
        return Err(EsError::illegal_argument(format!("Can't sort on field [{}] of type [{}]", f.name, f.ty)));
    };
    if let Some(nt) = numeric_type {
        // `DocValueFormat.withNanosecondResolution`: only a date field's format converts.
        if nt == "date_nanos" && !matches!(f.kind, Kind::Date | Kind::DateNanos) && !crate::opensearch::active() {
            return Err(EsError::illegal_argument("trying to convert a known date time formatter to a nanosecond one, wrong field used?"));
        }
        kind = match nt {
            "long" | "date" | "date_nanos" => {
                if kind == SortType::Double || kind == SortType::Float || kind == SortType::HalfFloat {
                    kind
                } else {
                    SortType::Long
                }
            }
            // Integral values compare as doubles; a missing one is an infinity, so the key
            // takes `Long`'s extremes, which print as infinities (`render_value`).
            "double" if kind == SortType::Int => SortType::Long,
            "double" => kind,
            other => return Err(EsError::illegal_argument(format!("invalid value for [numeric_type], must be one of [long, double, date, date_nanos], got [{other}]"))),
        };
    }
    let numeric = kind != SortType::Keyword;
    let miss = if numeric { numeric_missing(f.kind, missing)? } else { keyword_missing(missing)? };
    // `LongValuesComparatorSource.missingObject`: sorting as nanoseconds, `_first` / `_last`
    // become 0 and the largest nanosecond instead of the long extremes (no negative nanos),
    // so they are values `search_after` compares with.
    let nanos = match numeric_type {
        Some(t) => t == "date_nanos",
        None => f.kind == Kind::DateNanos,
    };
    let miss = match miss {
        // OpenSearch keeps the long extremes for nanosecond sorts too.
        Missing::First | Missing::Last if nanos && !crate::opensearch::active() => {
            let min = matches!(miss, Missing::First) ^ reverse;
            Missing::Value(SortValue::Long(if min { 0 } else { i64::MAX }))
        }
        m => m,
    };
    let nested = match nested {
        Some(n) => Some(nested_sort(ctx, n)?),
        // Inside a nested scope (inner hits) the docs sorted are nested docs themselves:
        // their own values sort (`validateMissingNestedPath` returns early there).
        None if ctx.nested.borrow().is_some() => None,
        None => match &f.nested_path {
            // Fields inside nested objects sort through their nested docs.
            Some(p) => Some(nested_sort(ctx, &crate::search_source::NestedSort { path: p.clone(), filter: None, max_children: None, nested: None })?),
            None => None,
        },
    };
    Ok(SortKey::Field(FieldSort { field: f.number, kind, reverse, missing: miss, mode: field_mode(ctx, mode, reverse, numeric)?, nested }))
}

fn nested_sort(ctx: &QueryCtx, n: &crate::search_source::NestedSort) -> EsResult<NestedSort> {
    let scope = ctx.nested.borrow().clone();
    let mut levels = Vec::new();
    let mut cur = Some(n);
    while let Some(l) = cur {
        if !ctx.table.objects.get(&l.path).is_some_and(|o| o.nested) {
            return Err(super::query::qse(ctx, format!("[nested] nested object under path [{}] is not of nested type", l.path)));
        }
        let filter = match &l.filter {
            Some(fq) => {
                let prev = ctx.nested.replace(Some(l.path.clone()));
                let q = ctx.to_filter(fq);
                ctx.nested.replace(prev);
                Some(q?)
            }
            None => None,
        };
        levels.push((l.path.as_str(), filter));
        cur = l.nested.as_deref();
    }
    Ok(nested_chain(scope.as_deref(), &levels, n.max_children.map(|m| m.max(0) as usize)))
}

/// `FieldSortBuilder.resolveNested`: the docs sorted are the parents of the enclosing nested
/// scope (the root docs at the top level), and the values come from the deepest level's docs
/// in each parent's block, each level restricted to the children of the previous level's
/// matching docs (`ToChildBlockJoinQuery`). So a sort on `a.b.x` with path `a.b` alone reads
/// every `a.b` doc of the root's block.
pub(crate) fn nested_chain(scope: Option<&str>, levels: &[(&str, Option<search_index::QueryRef>)], max_children: Option<usize>) -> NestedSort {
    use crate::engine::fields::NESTED_PATH_FIELD;
    use search_index::query::{BooleanQuery, Occur};
    let type_filter = |p: &str| -> search_index::QueryRef { Arc::new(TermQuery::new(NESTED_PATH_FIELD, p.as_bytes().to_vec())) };
    let parent_filter = match scope {
        None => search_index::query::nested::root_filter(),
        Some(p) => type_filter(p),
    };
    let mut child: Option<search_index::QueryRef> = None;
    let mut parent_mapper = scope;
    for (path, filter) in levels {
        let mut q = type_filter(path);
        if let Some(f) = filter {
            q = Arc::new(BooleanQuery::new().add(Occur::Filter, q).add(Occur::Filter, f.clone()));
        }
        if let (Some(prev), Some(pm)) = (child.take(), parent_mapper) {
            let to_child: search_index::QueryRef = Arc::new(search_index::query::nested::ToChildQuery { parent: prev, parent_filter: type_filter(pm) });
            q = Arc::new(BooleanQuery::new().add(Occur::Filter, q).add(Occur::Filter, to_child));
        }
        child = Some(q);
        parent_mapper = Some(path);
    }
    NestedSort { parent_filter, child_filter: child.expect("a nested sort has a path"), max_children }
}

fn keyword_missing(m: Option<&Value>) -> EsResult<Missing> {
    Ok(match m {
        None => Missing::Last,
        Some(Value::Str(s)) if s == "_last" => Missing::Last,
        Some(Value::Str(s)) if s == "_first" => Missing::First,
        Some(v) => Missing::Value(SortValue::Bytes(super::query::value_text(v).into_bytes())),
    })
}

/// The key of an `unmapped_type` sort on a field the index doesn't map: Elasticsearch sorts
/// with an empty field of that type, so every doc sorts as `missing` (the type's value or
/// extreme), then by doc id, and merges with the indices that map the field.
fn unmapped_key(ctx: &QueryCtx, k: Kind, reverse: bool, missing: Option<&Value>) -> EsResult<SortKey> {
    if k.is_text() {
        return Err(EsError::illegal_argument(crate::search::aggs::fielddata_disabled_in(
            ctx.idx.dialect == crate::cluster::Dialect::OpenSearch,
            "__anonymous_",
            &ctx.idx.name,
        )));
    }
    if matches!(k, Kind::ScaledFloat(_)) {
        return Err(EsError::illegal_argument("Field [scaling_factor] is required"));
    }
    let kind = match k {
        Kind::Integer | Kind::Short | Kind::Byte => SortType::Int,
        Kind::Double => SortType::Double,
        Kind::Float => SortType::Float,
        Kind::HalfFloat => SortType::HalfFloat,
        k if k.is_numeric() || k.is_date() || k == Kind::Boolean => SortType::Long,
        _ => SortType::Keyword,
    };
    let miss = if kind == SortType::Keyword { keyword_missing(missing)? } else { numeric_missing(k, missing)? };
    // As for mapped nanosecond dates: `_first` / `_last` are 0 and the largest nanosecond.
    let miss = match miss {
        Missing::First | Missing::Last if k == Kind::DateNanos && !crate::opensearch::active() => {
            let min = matches!(miss, Missing::First) ^ reverse;
            Missing::Value(SortValue::Long(if min { 0 } else { i64::MAX }))
        }
        m => m,
    };
    Ok(SortKey::Field(FieldSort { field: u32::MAX, kind, reverse, missing: miss, mode: SortMode::Min, nested: None }))
}

fn numeric_missing(kind: Kind, m: Option<&Value>) -> EsResult<Missing> {
    let v = match m {
        None => return Ok(Missing::Last),
        Some(Value::Str(s)) if s == "_last" => return Ok(Missing::Last),
        Some(Value::Str(s)) if s == "_first" => return Ok(Missing::First),
        Some(v) => v,
    };
    let d = match v {
        Value::Num(n) => n.as_f64(),
        Value::Str(s) => s.parse::<f64>().map_err(|_| EsError::number_format(s))?,
        other => return Err(EsError::number_format(&other.to_es_string())),
    };
    Ok(Missing::Value(match kind {
        Kind::Double => SortValue::Double(d),
        Kind::Float | Kind::HalfFloat => SortValue::Float(d as f32),
        Kind::ScaledFloat(sf) => SortValue::Long((d * sf).round() as i64),
        _ => SortValue::Long(match v {
            Value::Num(Number::Int(i)) => *i,
            _ => d as i64,
        }),
    }))
}

fn field_format(f: &FieldInfo, format: Option<&str>, numeric_type: Option<&str>) -> EsResult<ValueFormat> {
    Ok(match f.kind {
        Kind::Date | Kind::DateNanos => {
            let fmt = match format {
                Some(p) => Some(Arc::new(
                    match &f.date_format {
                        Some(d) => d.derive(p),
                        None => crate::time::DateFormatter::new(p),
                    }
                    .map_err(EsError::illegal_argument)?,
                )),
                None => None,
            };
            let field_nanos = f.kind == Kind::DateNanos;
            let nanos = match numeric_type {
                Some("date_nanos") => true,
                Some("date") => false,
                _ => field_nanos,
            };
            let parser = match (&fmt, &f.date_format) {
                (Some(p), _) | (None, Some(p)) => p.clone(),
                (None, None) => Arc::new(crate::time::DateFormatter::new(crate::time::DateFormatter::DEFAULT).map_err(EsError::illegal_argument)?),
            };
            ValueFormat::Date { nanos, field_nanos, format: fmt, parser }
        }
        Kind::Boolean => ValueFormat::Boolean,
        Kind::Ip => ValueFormat::Ip,
        Kind::Version => ValueFormat::Version,
        // `numeric_type: long | date | date_nanos` reads a floating field through
        // `LongValuesComparatorSource` (`FieldData.castToLong`): the values truncate to longs,
        // which [`rescale`] applies to the shard's values.
        Kind::Double | Kind::Float | Kind::HalfFloat if matches!(numeric_type, Some("long" | "date" | "date_nanos")) => ValueFormat::Long,
        Kind::Double => ValueFormat::Double,
        // `numeric_type: double` sorts a float field's values as doubles (`Float.doubleValue`).
        Kind::Float | Kind::HalfFloat if numeric_type == Some("double") => ValueFormat::Double,
        Kind::Float => ValueFormat::Float,
        Kind::HalfFloat => ValueFormat::HalfFloat,
        Kind::ScaledFloat(sf) if matches!(numeric_type, Some("long" | "date")) => ValueFormat::ScaledLong(sf),
        Kind::ScaledFloat(sf) => ValueFormat::ScaledFloat(sf),
        Kind::UnsignedLong if numeric_type == Some("double") => ValueFormat::UnsignedLongDouble,
        Kind::UnsignedLong => ValueFormat::UnsignedLong,
        k if k.is_integral() => {
            if numeric_type == Some("double") {
                ValueFormat::Double
            } else {
                ValueFormat::Long
            }
        }
        _ => ValueFormat::Keyword,
    })
}

/// The keys hits of several shards merge by: a `string` script or runtime field sort (whose
/// shard-local ordinals became bytes, [`rescale`]) merges as a keyword field does, its
/// missing values last (or first) whatever the direction.
pub fn merge_keys(keys: Vec<SortKey>, formats: &[ValueFormat]) -> Vec<SortKey> {
    keys.into_iter()
        .zip(formats.iter().map(Some).chain(std::iter::repeat(None)))
        .map(|(k, f)| match (k, f) {
            (SortKey::Script(s), Some(ValueFormat::ScriptTerms(t))) => SortKey::Field(FieldSort {
                field: u32::MAX,
                kind: SortType::Keyword,
                reverse: s.reverse,
                missing: if t.missing_first() { Missing::First } else { Missing::Last },
                mode: s.mode,
                nested: None,
            }),
            (k, _) => k,
        })
        .collect()
}

/// A sort value as the response prints it.
pub fn render_value(v: &SortValue, f: &ValueFormat, doc: u32, score: f32) -> Value {
    match (f, v) {
        (ValueFormat::Score, _) => Value::Num(Number::Float(crate::fetch::java_float_as_double(score))),
        (ValueFormat::Doc, SortValue::Long(l)) => Value::from(*l),
        (ValueFormat::Doc, _) => Value::from(doc as i64),
        (ValueFormat::ShardDoc, SortValue::Long(l)) => Value::from(*l),
        (ValueFormat::ShardDoc, _) => Value::from(doc as i64),
        (_, SortValue::Null) => Value::Null,
        (ValueFormat::Ip, SortValue::Bytes(b)) if b.len() == 16 => Value::Str(search_index::query::range::inet_to_string(b)),
        (ValueFormat::Keyword, SortValue::Bytes(b)) => Value::Str(String::from_utf8_lossy(b).into_owned()),
        (ValueFormat::Version, SortValue::Bytes(b)) => Value::Str(super::version_field::decode(b)),
        (ValueFormat::ScriptTerms(t), SortValue::Bytes(b)) if t.version => Value::Str(super::version_field::decode(b)),
        (ValueFormat::ScriptTerms(t), SortValue::Bytes(b)) if t.ip && b.len() == 16 => Value::Str(search_index::query::range::inet_to_string(b)),
        (ValueFormat::Tsid, SortValue::Bytes(b)) => Value::Str(crate::engine::tsdb::tsid_display(b)),
        (ValueFormat::UnsignedLong, SortValue::Long(l)) => {
            let u = (*l as u64) ^ (1 << 63);
            if u <= i64::MAX as u64 { Value::from(u as i64) } else { Value::Num(Number::UInt(u)) }
        }
        (ValueFormat::UnsignedLongDouble, SortValue::Long(i64::MAX)) => Value::Num(Number::Float(f64::INFINITY)),
        (ValueFormat::UnsignedLongDouble, SortValue::Long(i64::MIN)) => Value::Num(Number::Float(f64::NEG_INFINITY)),
        (ValueFormat::UnsignedLongDouble, SortValue::Long(l)) => Value::Num(Number::Float(((*l as u64) ^ (1 << 63)) as f64)),
        // A missing date (sorted with the type's extreme) prints as null once formatted.
        (ValueFormat::Date { format: Some(_), .. }, SortValue::Long(i64::MAX | i64::MIN)) => Value::Null,
        (ValueFormat::Date { nanos, format: Some(fmt), .. }, SortValue::Long(l)) => Value::Str(fmt.format(crate::fetch::instant_of(*nanos, *l), &crate::time::Zone::UTC)),
        // Elasticsearch sorts scaled floats as doubles: a missing value is an infinity.
        (ValueFormat::ScaledFloat(_), SortValue::Long(i64::MAX)) => Value::Num(Number::Float(f64::INFINITY)),
        (ValueFormat::ScaledFloat(_), SortValue::Long(i64::MIN)) => Value::Num(Number::Float(f64::NEG_INFINITY)),
        (ValueFormat::ScaledFloat(sf), SortValue::Long(l)) => Value::Num(Number::Float(super::hits::scaled_double(*l, *sf))),
        // `numeric_type: double` on an integral field: missing values sort as infinities.
        (ValueFormat::Double, SortValue::Long(i64::MAX)) => Value::Num(Number::Float(f64::INFINITY)),
        (ValueFormat::Double, SortValue::Long(i64::MIN)) => Value::Num(Number::Float(f64::NEG_INFINITY)),
        (ValueFormat::Double, SortValue::Long(l)) => Value::Num(Number::Float(*l as f64)),
        (ValueFormat::Double, SortValue::Float(x)) => Value::Num(Number::Float(f64::from(*x))),
        // A runtime field's numbers sort as doubles; longs print as longs.
        (ValueFormat::Long, SortValue::Double(d)) => Value::from(*d as i64),
        (ValueFormat::Long, SortValue::Float(x)) => Value::from(*x as i64),
        (_, SortValue::Long(l)) => Value::from(*l),
        (_, SortValue::Double(d)) => Value::Num(Number::Float(*d)),
        (_, SortValue::Float(x)) => Value::Num(Number::Float(crate::fetch::java_float_as_double(*x))),
        (_, SortValue::Bytes(b)) => Value::Str(String::from_utf8_lossy(b).into_owned()),
    }
}

/// Lucene's `SortField.Type` of a merged sort key, from the shard's value format
/// (`SortFieldValidation.getType`); `None` for keys every shard sorts alike (score, doc) or
/// whose type isn't known (`CUSTOM`).
fn merge_type(f: &ValueFormat) -> Option<&'static str> {
    Some(match f {
        ValueFormat::Score | ValueFormat::Doc | ValueFormat::ShardDoc => return None,
        // `INT` and `LONG` merge as longs, which print the same: one type.
        ValueFormat::Long | ValueFormat::Date { .. } | ValueFormat::Boolean | ValueFormat::UnsignedLong | ValueFormat::ScaledLong(_) => "LONG",
        ValueFormat::Double | ValueFormat::ScaledFloat(_) | ValueFormat::UnsignedLongDouble => "DOUBLE",
        ValueFormat::Float | ValueFormat::HalfFloat => "FLOAT",
        ValueFormat::Keyword | ValueFormat::Ip | ValueFormat::Version | ValueFormat::ScriptTerms(_) | ValueFormat::Tsid => "STRING",
        ValueFormat::Unmapped(Some(k)) => match k {
            Kind::Double | Kind::ScaledFloat(_) => "DOUBLE",
            Kind::Float | Kind::HalfFloat => "FLOAT",
            k if k.is_integral() || matches!(k, Kind::Date | Kind::DateNanos | Kind::Boolean | Kind::UnsignedLong) => "LONG",
            _ => "STRING",
        },
        ValueFormat::Unmapped(None) => return None,
    })
}

/// Elasticsearch's `SortFieldValidation.validateAndMaybeRewrite` over the formats of the
/// shards that collected hits (the first one setting the expected types): the sort keys whose
/// numeric types differ with a float or a double among them, which merge as doubles. Any
/// other disagreement fails the merge.
pub fn merge_widening(shards: &[&[ValueFormat]], sorts: &[Sort]) -> EsResult<Vec<usize>> {
    let mut widen = Vec::new();
    let Some((first, rest)) = shards.split_first() else { return Ok(widen) };
    // An unknown type stops the check, as `CUSTOM` does.
    if shards.iter().any(|fs| fs.iter().any(|f| matches!(f, ValueFormat::Unmapped(None)))) {
        return Ok(widen);
    }
    let first_types: Vec<Option<&str>> = first.iter().map(merge_type).collect();
    for fs in rest {
        for (i, f) in fs.iter().enumerate() {
            let (Some(Some(a)), Some(b)) = (first_types.get(i), merge_type(f)) else { continue };
            if *a == b {
                continue;
            }
            let numeric = |t: &str| t != "STRING";
            if numeric(a) && numeric(b) && (matches!(*a, "FLOAT" | "DOUBLE") || matches!(b, "FLOAT" | "DOUBLE")) {
                if !widen.contains(&i) {
                    widen.push(i);
                }
                continue;
            }
            let field = match sorts.get(i) {
                Some(Sort::Field { field, .. }) => field.as_str(),
                _ => "null",
            };
            return Err(EsError::illegal_argument(format!("Can't sort on field [{field}]; the field has incompatible sort types: [{a}] and [{b}] across shards!")));
        }
    }
    Ok(widen)
}

/// A shard's values of the widened keys as the doubles they merge as (`Number.doubleValue`),
/// after [`merge_values`].
pub fn widen_values(values: &mut [SortValue], widen: &[usize]) {
    for &i in widen {
        let Some(v) = values.get_mut(i) else { continue };
        let d = match &*v {
            SortValue::Long(l) => *l as f64,
            SortValue::Float(x) => f64::from(*x),
            _ => continue,
        };
        *v = SortValue::Double(d);
    }
}

/// OpenSearch merges the shards' hits with the first shard's comparators whatever the other
/// shards' sort types: a numeric key meeting a string one fails the cast in `TopDocs.merge`
/// (a 500 `class_cast_exception` naming the numeric value's class).
pub fn merge_cast_failure(shards: &[&[ValueFormat]]) -> Option<EsError> {
    let (first, rest) = shards.split_first()?;
    for fs in rest {
        for (a, b) in first.iter().zip(fs.iter()) {
            let (Some(ta), Some(tb)) = (merge_type(a), merge_type(b)) else { continue };
            if (ta == "STRING") == (tb == "STRING") {
                continue;
            }
            let class = match if ta == "STRING" { tb } else { ta } {
                "DOUBLE" => "Double",
                "FLOAT" => "Float",
                _ => "Long",
            };
            return Some(EsError::new(
                500,
                "class_cast_exception",
                format!("class java.lang.{class} cannot be cast to class org.apache.lucene.util.BytesRef (java.lang.{class} is in module java.base of loader 'bootstrap'; org.apache.lucene.util.BytesRef is in unnamed module of loader 'app')"),
            ));
        }
    }
    None
}

/// A shard's values as the coordinator merges them: the keys a shard sorts as doubles but
/// holds otherwise (scaled floats as their scaled longs, integral and float fields under
/// `numeric_type: double`) become those doubles, so they compare with other indices' values
/// of the field. A missing value's extremes are infinities there.
pub fn merge_values(values: &mut [SortValue], formats: &[ValueFormat]) {
    for (v, f) in values.iter_mut().zip(formats) {
        let d = match (f, &*v) {
            (ValueFormat::ScaledFloat(_) | ValueFormat::Double, SortValue::Long(i64::MAX)) => f64::INFINITY,
            (ValueFormat::ScaledFloat(_) | ValueFormat::Double, SortValue::Long(i64::MIN)) => f64::NEG_INFINITY,
            (ValueFormat::ScaledFloat(sf), SortValue::Long(l)) => super::hits::scaled_double(*l, *sf),
            (ValueFormat::Double, SortValue::Long(l)) => *l as f64,
            (ValueFormat::Double, SortValue::Float(x)) => f64::from(*x),
            _ => continue,
        };
        *v = SortValue::Double(d);
    }
}

/// A shard's formats once [`widen_values`] made the widened keys doubles.
pub fn widen_formats(formats: &[ValueFormat], widen: &[usize]) -> Vec<ValueFormat> {
    formats.iter().enumerate().map(|(i, f)| if widen.contains(&i) { ValueFormat::Double } else { f.clone() }).collect()
}

/// `_shard_doc`'s `search_after` value as one shard compares with it
/// (`ShardDocSortField.setTopValue`): the doc on the shard it names, every doc (-1) on the
/// shards after it and none (`i32::MAX`) on those before, whatever the order.
pub fn shard_doc_after(value: i64, shard_index: usize) -> i64 {
    let top = (value >> 32) as i32 as i64;
    let me = shard_index as i64;
    if me == top {
        i64::from(value as i32)
    } else if me < top {
        i64::from(i32::MAX)
    } else {
        -1
    }
}

/// A shard's `_shard_doc` sort value (`ShardDocSortField.encodeShardAndDoc`).
pub fn encode_shard_doc(shard_index: usize, doc: i64) -> i64 {
    ((shard_index as i64) << 32) | (doc & 0xFFFF_FFFF)
}

/// The scaled-long bound a shard compares its scaled floats with for the `search_after`
/// long `b` of a `numeric_type: long` sort: a scaled value `s` comes after the bound exactly
/// when its truncation `(long) (s / sf)` comes after `b` (truncation rounds towards zero, so
/// each side of zero has its own edge).
fn scaled_long_after(b: i64, sf: f64, reverse: bool) -> i64 {
    let b = b as f64;
    if !reverse {
        // Ascending, after `b`: `trunc(x) > b`, so `x >= b + 1` from zero up, `x > b` below.
        if b >= 0.0 { ((b + 1.0) * sf).ceil() as i64 - 1 } else { (b * sf).floor() as i64 }
    } else if b > 0.0 {
        // Descending, after `b`: `trunc(x) < b`, so `x < b` above zero, `x <= b - 1` from zero down.
        (b * sf).ceil() as i64
    } else {
        ((b - 1.0) * sf).floor() as i64 + 1
    }
}

/// A date sort value from unit `from_nanos` into unit `to_nanos` (`numeric_type` mixing
/// `date` and `date_nanos`: millis scale up saturating, nanos truncate to millis). The
/// missing-value extremes stay extremes.
pub fn convert_date(l: i64, from_nanos: bool, to_nanos: bool) -> i64 {
    match (from_nanos, to_nanos) {
        _ if l == i64::MAX || l == i64::MIN => l,
        (false, true) => l.saturating_mul(1_000_000),
        (true, false) => l.div_euclid(1_000_000),
        _ => l,
    }
}

/// Converts a shard's date sort values into the units the request reports
/// (`numeric_type`), in place.
pub fn rescale(values: &mut [SortValue], formats: &[ValueFormat]) {
    for (v, f) in values.iter_mut().zip(formats) {
        if let (SortValue::Double(o), ValueFormat::ScriptTerms(t)) = (&*v, f) {
            *v = t.value_of(*o);
            continue;
        }
        // A floating field sorted under `numeric_type: long`: Java's `(long)` cast (towards
        // zero, saturating, NaN to 0), so shards of other indices merge on the same longs.
        // The infinities a missing value sorts with become `Long`'s extremes, as Elasticsearch
        // fills them.
        if matches!(f, ValueFormat::Long) {
            match *v {
                SortValue::Double(d) => *v = SortValue::Long(d as i64),
                SortValue::Float(x) => *v = SortValue::Long(x as i64),
                _ => {}
            }
        }
        if let (SortValue::Long(l), ValueFormat::ScaledLong(sf)) = (&mut *v, f)
            && *l != i64::MAX
            && *l != i64::MIN
        {
            *l = super::hits::scaled_double(*l, *sf) as i64;
        }
        if let (SortValue::Long(l), ValueFormat::Date { nanos, field_nanos, .. }) = (&mut *v, f)
            && nanos != field_nanos
        {
            *l = convert_date(*l, *field_nanos, *nanos);
        }
        // Nanosecond dates start at the epoch: Elasticsearch's `NumericType.DATE_NANOSECONDS`
        // sorts a missing value first as 0, not `Long.MIN_VALUE` (OpenSearch keeps the latter).
        if let (SortValue::Long(l), ValueFormat::Date { nanos: true, .. }) = (&mut *v, f)
            && *l == i64::MIN
            && !crate::opensearch::active()
        {
            *l = 0;
        }
    }
}

/// Lucene's `SortField.Type` of a sort key, as `search_after` errors name it.
fn sort_type_name(key: &SortKey, f: &ValueFormat) -> &'static str {
    match key {
        SortKey::Score { .. } => "SCORE",
        SortKey::Doc { .. } if matches!(f, ValueFormat::ShardDoc) => "LONG",
        SortKey::Doc { .. } => "DOC",
        SortKey::GeoDistance(_) => "DOUBLE",
        SortKey::Script(_) => match f {
            ValueFormat::Keyword | ValueFormat::Version | ValueFormat::ScriptTerms(_) => "STRING",
            _ => "DOUBLE",
        },
        SortKey::Field(fs) => match (fs.kind, f) {
            (SortType::Keyword, _) => "STRING",
            (SortType::Double | SortType::Float | SortType::HalfFloat, ValueFormat::Long) | (_, ValueFormat::ScaledLong(_)) => "LONG",
            (SortType::Float | SortType::HalfFloat, ValueFormat::Double) => "DOUBLE",
            (_, ValueFormat::ScaledFloat(_)) | (SortType::Double, _) => "DOUBLE",
            (SortType::Float | SortType::HalfFloat, _) => "FLOAT",
            (SortType::Int, _) => "INT",
            (SortType::Long | SortType::UnsignedLong, _) => "LONG",
        },
    }
}

/// `Number.longValue()` of a JSON number as Jackson reads it (`BigInteger` keeps its low 64
/// bits, doubles saturate).
fn java_long_value(n: &Number) -> i64 {
    match n {
        Number::Int(i) => *i,
        Number::UInt(u) => *u as i64,
        Number::Float(f) => *f as i64,
        Number::Big(s) => {
            let (neg, digits) = match s.strip_prefix('-') {
                Some(d) => (true, d),
                None => (false, &**s),
            };
            let v = digits.bytes().fold(0u64, |acc, b| acc.wrapping_mul(10).wrapping_add(u64::from(b.wrapping_sub(b'0'))));
            if neg { v.wrapping_neg() as i64 } else { v as i64 }
        }
    }
}

/// `Number.intValue()`.
fn java_int_value(n: &Number) -> i32 {
    match n {
        Number::Float(f) => *f as i32,
        other => java_long_value(other) as i32,
    }
}

/// `Value.toString()` of a search_after value (Java's boxed types).
fn java_string(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => other.to_es_string(),
    }
}

/// A `search_after` value for the sort key at `pos` (`SearchAfterBuilder.buildFieldDoc` and
/// `convertValueFromSortType`): numbers convert as Java's `Number` accessors do, strings parse
/// (`Long.parseLong` first, so long values keep every digit), nulls are only allowed for
/// string sorts, and number format errors are wrapped with the field name.
pub fn parse_after(v: &Value, key: &SortKey, f: &ValueFormat, now_ms: i64, pos: usize, field: &str) -> EsResult<SortValue> {
    if v.is_null() {
        let ty = sort_type_name(key, f);
        if ty == "STRING" {
            return Ok(SortValue::Null);
        }
        return Err(EsError::illegal_argument(format!("[search_after] value at position {pos} cannot be null for sort field [{field}] of type [{ty}].")));
    }
    let wrap = |e: EsError| {
        if e.kind == "number_format_exception" {
            EsError::illegal_argument(format!("Failed to parse search_after value for field [{field}].")).caused_by(e)
        } else {
            e
        }
    };
    let nfe = |s: &str| EsError::number_format(s);
    let double = |v: &Value| -> EsResult<f64> {
        match v {
            Value::Num(n) => Ok(n.as_f64()),
            other => {
                let s = java_string(other);
                crate::units::parse_java_double(&s).ok_or_else(|| nfe(&s))
            }
        }
    };
    let float = |v: &Value| -> EsResult<f32> {
        match v {
            Value::Num(n) => Ok(n.as_f64() as f32),
            other => crate::query::xparser::java_parse_float(&java_string(other)),
        }
    };
    let int = |v: &Value, clamp: bool| -> EsResult<i64> {
        match v {
            Value::Num(n) if clamp && java_long_value(n) > i64::from(i32::MAX) => Ok(i64::from(i32::MAX)),
            Value::Num(n) => Ok(i64::from(java_int_value(n))),
            other => {
                let s = java_string(other);
                crate::units::parse_java_int(&s).map(i64::from).ok_or_else(|| nfe(&s))
            }
        }
    };
    // `DocValueFormat.RAW.parseLong`: a long, else a double rounded down.
    let long = |v: &Value| -> EsResult<i64> {
        match v {
            Value::Num(n) => Ok(java_long_value(n)),
            other => {
                let s = java_string(other);
                if let Some(l) = crate::units::parse_java_long(&s) {
                    return Ok(l);
                }
                crate::units::parse_java_double(&s).map(|d| d.floor() as i64).ok_or_else(|| nfe(&s))
            }
        }
    };
    let out = (|| -> EsResult<SortValue> {
        Ok(match key {
            SortKey::Score { .. } => SortValue::Float(float(v)?),
            SortKey::Doc { .. } if matches!(f, ValueFormat::ShardDoc) => SortValue::Long(long(v)?),
            SortKey::Doc { .. } => SortValue::Long(int(v, false)?),
            SortKey::Field(fs) => match (fs.kind, f) {
                (SortType::Keyword, ValueFormat::Ip) => {
                    let s = super::query::value_text(v);
                    SortValue::Bytes(crate::document::values::parse_ip(&s).ok_or_else(|| EsError::illegal_argument(format!("'{s}' is not an IP string literal.")))?.to_vec())
                }
                (SortType::Keyword, ValueFormat::Tsid) => SortValue::Bytes(crate::engine::tsdb::tsid_parse(&super::query::value_text(v))?),
                (SortType::Keyword, ValueFormat::Version) => SortValue::Bytes(super::version_field::encode(&super::query::value_text(v))?),
                (SortType::Keyword, _) => SortValue::Bytes(super::query::value_text(v).into_bytes()),
                (_, ValueFormat::ScaledFloat(sf)) => SortValue::Long((double(v)? * sf).round() as i64),
                (_, ValueFormat::ScaledLong(sf)) => SortValue::Long(scaled_long_after(long(v)?, *sf, fs.reverse)),
                // `numeric_type: long` on a floating field parses the bound as a long.
                (SortType::Double | SortType::Float | SortType::HalfFloat, ValueFormat::Long) => SortValue::Long(long(v)?),
                (SortType::Double, _) => SortValue::Double(double(v)?),
                (SortType::Float | SortType::HalfFloat, _) => SortValue::Float(float(v)?),
                (_, ValueFormat::Boolean) => SortValue::Long(match v {
                    Value::Num(n) => java_long_value(n),
                    other => match java_string(other).as_str() {
                        "true" => 1,
                        "false" => 0,
                        s => return Err(EsError::illegal_argument(format!("Cannot parse boolean [{s}], expected either [true] or [false]"))),
                    },
                }),
                (_, ValueFormat::Date { parser, nanos, field_nanos, .. }) => {
                    // `DocValueFormat.DateTime.parseLong`: strings go through the date math parser,
                    // numbers are taken as they are; then into the shard's unit.
                    let l = match v {
                        Value::Num(n) => java_long_value(n),
                        other => {
                            let s = java_string(other);
                            let i = crate::time::parse_date_math(&s, now_ms, false, None, parser).map_err(|e| e.to_es())?;
                            if *nanos { crate::time::to_epoch_nanos(i).map_err(EsError::illegal_argument)? } else { i.millis() }
                        }
                    };
                    SortValue::Long(convert_date(l, *nanos, *field_nanos))
                }
                (_, ValueFormat::UnsignedLongDouble) => {
                    // A double `search_after` value: the unsigned long it stands for.
                    let d = double(v)?;
                    SortValue::Long(((d.max(0.0) as u64) ^ (1 << 63)) as i64)
                }
                (_, ValueFormat::UnsignedLong) => {
                    let s = super::query::value_text(v);
                    let u: u64 = s.parse().map_err(|_| nfe(&s))?;
                    SortValue::Long((u ^ (1 << 63)) as i64)
                }
                (SortType::Int, _) => SortValue::Long(int(v, true)?),
                _ => SortValue::Long(long(v)?),
            },
            SortKey::GeoDistance(_) => SortValue::Double(double(v)?),
            SortKey::Script(_) => match f {
                ValueFormat::Keyword => SortValue::Bytes(super::query::value_text(v).into_bytes()),
                ValueFormat::Version => SortValue::Bytes(super::version_field::encode(&super::query::value_text(v))?),
                ValueFormat::ScriptTerms(t) => SortValue::Double(t.after_ord(&super::query::value_text(v))?),
                _ => SortValue::Double(double(v)?),
            },
        })
    })();
    out.map_err(wrap)
}

/// `_geo_distance` (`GeoDistanceSortBuilder.fromXContent`): the field's origin points, `unit`,
/// `distance_type`, `mode`, `order`, `ignore_unmapped`, `nested`.
pub fn geo_distance(ctx: &QueryCtx, m: &crate::json::Map) -> EsResult<(SortKey, String)> {
    let mut field: Option<(String, Vec<(f64, f64)>)> = None;
    let mut unit = 1.0f64;
    let mut dtype = GeoDistanceType::Arc;
    let mut reverse = false;
    let mut mode = None;
    let mut ignore_unmapped = false;
    let mut nested = None;
    for (k, v) in m {
        match k.as_str() {
            "unit" => unit = crate::query::geo::distance_unit(&super::query::value_text(v))?,
            "distance_type" => {
                dtype = match super::query::value_text(v).to_lowercase().as_str() {
                    "plane" => GeoDistanceType::Plane,
                    "arc" => GeoDistanceType::Arc,
                    other => return Err(EsError::illegal_argument(format!("No geo distance for [{other}]"))),
                }
            }
            "order" => reverse = crate::search_source::SortOrder::parse(&super::query::value_text(v))? == SortOrder::Desc,
            "mode" => mode = Some(super::query::value_text(v)),
            "ignore_unmapped" => ignore_unmapped = v.as_bool().unwrap_or(false),
            "validation_method" => {}
            "nested" => nested = Some(v.clone()),
            name => {
                let pts = parse_points(v)?;
                field = Some((name.to_string(), pts.into_iter().map(|p| (p.lat, p.lon)).collect()));
            }
        }
    }
    let Some((name, points)) = field else { return Err(EsError::illegal_argument("_geo_distance sort requires a field")) };
    if let Some(rf) = ctx.runtime_field(&name).filter(|f| f.ty == super::runtime::RtType::GeoPoint) {
        let mode = match mode.as_deref() {
            None => None,
            Some("sum") => return Err(EsError::illegal_argument("sort_mode [sum] isn't supported for sorting by geo distance")),
            Some(m) => Some(mode_of(Some(m), reverse, true)?),
        };
        let ev = super::runtime::Evaluator::new(&ctx.cluster.scripts, ctx.st, ctx.table, ctx.shard as i32, &ctx.idx.name, rf)?.counted();
        return Ok((super::runtime_geo::sort_key(Arc::new(ev), points, dtype, unit, reverse, mode), name));
    }
    let f = match ctx.field(&name) {
        Some(f) => f,
        None if ignore_unmapped => {
            return Ok((SortKey::Field(FieldSort { field: u32::MAX, kind: SortType::Double, reverse, missing: Missing::Last, mode: SortMode::Min, nested: None }), name));
        }
        None => return Err(super::query::qse(ctx, format!("failed to find mapper for [{name}] for geo distance based sort"))),
    };
    if f.kind != Kind::GeoPoint {
        return Err(EsError::illegal_argument(format!("field [{}] of type [{}] is not supported for geo distance sorting", f.name, f.ty)));
    }
    let mode = match mode.as_deref() {
        None => None,
        Some("sum") => return Err(EsError::illegal_argument("sort_mode [sum] isn't supported for sorting by geo distance")),
        Some(m) => Some(mode_of(Some(m), reverse, true)?),
    };
    let nested = match nested {
        Some(Value::Obj(n)) => {
            let path = n.get("path").and_then(Value::as_str).unwrap_or_default().to_string();
            Some(nested_sort(ctx, &crate::search_source::NestedSort { path, filter: None, max_children: None, nested: None })?)
        }
        _ => f.nested_path.as_ref().map(|p| nested_sort(ctx, &crate::search_source::NestedSort { path: p.clone(), filter: None, max_children: None, nested: None })).transpose()?,
    };
    let _ = encode::i32_bytes;
    Ok((SortKey::GeoDistance(GeoDistanceSort { field: f.number, points, distance_type: dtype, unit_meters: unit, reverse, mode, nested }), name))
}

/// Origin points of a geo sort or query value: one point (`[lon, lat]`, `"lat,lon"`, a
/// geohash, `{lat, lon}`) or an array of them.
pub fn parse_points(v: &Value) -> EsResult<Vec<crate::query::geo::GeoPoint>> {
    let one = |v: &Value| -> EsResult<crate::query::geo::GeoPoint> {
        let mut p = crate::query::XParser::from_value(v);
        p.next_token();
        crate::query::geo::parse_point(&mut p)
    };
    match v {
        Value::Arr(a) if a.first().is_some_and(|x| matches!(x, Value::Num(_))) => Ok(vec![one(v)?]),
        Value::Arr(a) => a.iter().map(one).collect(),
        other => Ok(vec![one(other)?]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str) -> Sort {
        Sort::Field { field: name.into(), order: None, missing: None, unmapped_type: None, mode: None, numeric_type: None, format: None, nested: None }
    }

    #[test]
    fn mixed_numeric_shards_widen_to_doubles() {
        let (long, double, float, kw) = ([ValueFormat::Long], [ValueFormat::Double], [ValueFormat::Float], [ValueFormat::Keyword]);
        let sorts = [field("f")];
        assert_eq!(merge_widening(&[&long, &long], &sorts).unwrap(), Vec::<usize>::new());
        assert_eq!(merge_widening(&[&long, &double], &sorts).unwrap(), vec![0]);
        assert_eq!(merge_widening(&[&double, &float], &sorts).unwrap(), vec![0]);
        assert_eq!(merge_widening(&[&double], &sorts).unwrap(), Vec::<usize>::new());
        let e = merge_widening(&[&long, &kw], &sorts).unwrap_err();
        assert_eq!(e.reason(), "Can't sort on field [f]; the field has incompatible sort types: [LONG] and [STRING] across shards!");
        // Score and doc keys never take part.
        assert!(merge_widening(&[&[ValueFormat::Score], &[ValueFormat::Doc]], &[]).unwrap().is_empty());
        let mut v = vec![SortValue::Long(3), SortValue::Float(7.9)];
        widen_values(&mut v, &[0, 1]);
        assert_eq!(v, vec![SortValue::Double(3.0), SortValue::Double(f64::from(7.9f32))]);
        assert!(matches!(widen_formats(&long, &[0])[0], ValueFormat::Double));
    }

    #[test]
    fn merge_values_hold_what_shards_compare() {
        let mut v = vec![SortValue::Long(123), SortValue::Long(i64::MAX), SortValue::Long(5), SortValue::Float(1.5)];
        merge_values(&mut v, &[ValueFormat::ScaledFloat(100.0), ValueFormat::ScaledFloat(100.0), ValueFormat::Double, ValueFormat::Double]);
        assert_eq!(v, vec![SortValue::Double(1.23), SortValue::Double(f64::INFINITY), SortValue::Double(5.0), SortValue::Double(1.5)]);
        assert!(merge_cast_failure(&[&[ValueFormat::Long], &[ValueFormat::Keyword]]).is_some_and(|e| e.status == 500 && e.reason().starts_with("class java.lang.Long cannot be cast")));
        assert!(merge_cast_failure(&[&[ValueFormat::Long], &[ValueFormat::Double]]).is_none());
    }

    #[test]
    fn numeric_type_long_truncates_floating_values() {
        let mut v = vec![SortValue::Double(2.5), SortValue::Double(-1.75), SortValue::Float(7.9), SortValue::Double(f64::NEG_INFINITY)];
        rescale(&mut v, &[ValueFormat::Long, ValueFormat::Long, ValueFormat::Long, ValueFormat::Long]);
        assert_eq!(v, vec![SortValue::Long(2), SortValue::Long(-1), SortValue::Long(7), SortValue::Long(i64::MIN)]);
        assert_eq!(render_value(&SortValue::Float(7.9), &ValueFormat::Double, 0, 0.0), Value::Num(Number::Float(f64::from(7.9f32))));
    }

    #[test]
    fn numeric_type_long_truncates_scaled_floats() {
        // Scaled by 100: 145.36, -1.5, a missing value sorted last.
        let mut v = vec![SortValue::Long(14536), SortValue::Long(-150), SortValue::Long(i64::MAX)];
        rescale(&mut v, &[ValueFormat::ScaledLong(100.0), ValueFormat::ScaledLong(100.0), ValueFormat::ScaledLong(100.0)]);
        assert_eq!(v, vec![SortValue::Long(145), SortValue::Long(-1), SortValue::Long(i64::MAX)]);
        // Every scaled value of -500..500 is after the bound exactly when its truncation is.
        for b in -4..=4i64 {
            for reverse in [false, true] {
                let bound = scaled_long_after(b, 100.0, reverse);
                for s in -500..=500i64 {
                    let t = (s as f64 / 100.0) as i64;
                    let want = if reverse { t < b } else { t > b };
                    let got = if reverse { s < bound } else { s > bound };
                    assert_eq!(got, want, "b={b} reverse={reverse} s={s}");
                }
            }
        }
    }
}
