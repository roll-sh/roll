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
    Long,
    Double,
    Float,
    Boolean,
    Ip,
    Keyword,
    UnsignedLong,
    /// Epoch millis / nanos (`nanos`, the unit sort values are reported in), formatted when
    /// `format` is given. `field_nanos` is the unit the shard's doc values hold (they differ
    /// under `numeric_type`); `parser` reads string `search_after` values (the sort `format`,
    /// else the field's).
    Date { nanos: bool, field_nanos: bool, format: Option<Arc<crate::time::DateFormatter>>, parser: Arc<crate::time::DateFormatter> },
    HalfFloat,
    ScaledFloat(f64),
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
                        mode: mode_of(mode.as_deref(), reverse, false)?,
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
                let Some(f) = ctx.field(field) else {
                    if field == "_id" && crate::engine::tsdb::is_time_series(ctx.idx) {
                        return Err(EsError::illegal_argument("Fielddata is not supported on [_id] field in [time_series] indices"));
                    }
                    if field == "_id" {
                        return Err(EsError::illegal_argument(
                            "Fielddata access on the _id field is disallowed, you can re-enable it by updating the dynamic cluster setting: indices.id_field_data.enabled",
                        ));
                    }
                    match unmapped_type {
                        Some(t) => {
                            out.keys.push(SortKey::Doc { reverse: false });
                            out.formats.push(ValueFormat::Unmapped(Some(Kind::of(t))));
                            // Unmapped fields sort as missing: every doc ties, then by doc id.
                            out.keys.pop();
                            out.formats.pop();
                            out.keys.push(SortKey::Field(FieldSort {
                                field: u32::MAX,
                                kind: if Kind::of(t).is_numeric() || Kind::of(t).is_date() { SortType::Long } else { SortType::Keyword },
                                reverse,
                                missing: Missing::Last,
                                mode: SortMode::Min,
                                nested: None,
                            }));
                            out.formats.push(ValueFormat::Unmapped(Some(Kind::of(t))));
                            continue;
                        }
                        None => return Err(no_mapping(ctx, field)),
                    }
                };
                let key = field_key(ctx, &f, reverse, missing.as_ref(), mode.as_deref(), numeric_type.as_deref(), nested.as_ref())?;
                out.formats.push(field_format(&f, format.as_deref(), numeric_type.as_deref())?);
                out.keys.push(key);
            }
            Sort::Script { script, ty, order, mode, nested } => {
                let reverse = *order == Some(SortOrder::Desc);
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

fn sort_type(f: &FieldInfo) -> Option<SortType> {
    Some(match f.kind {
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
            return Ok(SortKey::Field(FieldSort { field: fd, kind: SortType::Keyword, reverse, missing: keyword_missing(missing)?, mode: mode_of(mode, reverse, false)?, nested: None }));
        }
        return Err(EsError::illegal_argument(format!(
            "Fielddata is disabled on [{0}] in [{1}]. Text fields are not optimised for operations that require per-document field data like aggregations and sorting, so these operations are disabled by default. Please use a keyword field instead. Alternatively, set fielddata=true on [{0}] in order to load field data by uninverting the inverted index. Note that this can use significant memory.",
            f.name, ctx.idx.name
        )));
    }
    if !f.doc_values {
        return Err(EsError::illegal_argument(format!("Can't load fielddata on [{}] because fielddata is unsupported on fields of type [{}]. Use doc values instead.", f.name, f.ty)));
    }
    let Some(mut kind) = sort_type(f) else {
        return Err(EsError::illegal_argument(format!("Can't sort on field [{}] of type [{}]", f.name, f.ty)));
    };
    if let Some(nt) = numeric_type {
        kind = match nt {
            "long" | "date" | "date_nanos" => {
                if kind == SortType::Double || kind == SortType::Float || kind == SortType::HalfFloat {
                    kind
                } else {
                    SortType::Long
                }
            }
            "double" => kind,
            other => return Err(EsError::illegal_argument(format!("invalid value for [numeric_type], must be one of [long, double, date, date_nanos], got [{other}]"))),
        };
    }
    let numeric = kind != SortType::Keyword;
    let miss = if numeric { numeric_missing(f, missing)? } else { keyword_missing(missing)? };
    let nested = match nested {
        Some(n) => Some(nested_sort(ctx, n)?),
        None => match &f.nested_path {
            // Fields inside nested objects sort through their nested docs.
            Some(p) => Some(nested_sort(ctx, &crate::search_source::NestedSort { path: p.clone(), filter: None, max_children: None, nested: None })?),
            None => None,
        },
    };
    Ok(SortKey::Field(FieldSort { field: f.number, kind, reverse, missing: miss, mode: mode_of(mode, reverse, numeric)?, nested }))
}

fn nested_sort(ctx: &QueryCtx, n: &crate::search_source::NestedSort) -> EsResult<NestedSort> {
    let Some(o) = ctx.table.objects.get(&n.path).filter(|o| o.nested) else {
        return Err(super::query::qse(ctx, format!("[nested] nested object under path [{}] is not of nested type", n.path)));
    };
    let parent_filter: search_index::QueryRef = match &o.parent_nested {
        None => search_index::query::nested::root_filter(),
        Some(p) => Arc::new(TermQuery::new(crate::engine::fields::NESTED_PATH_FIELD, p.as_bytes().to_vec())),
    };
    let mut child: search_index::QueryRef = Arc::new(TermQuery::new(crate::engine::fields::NESTED_PATH_FIELD, n.path.as_bytes().to_vec()));
    if let Some(fq) = &n.filter {
        let prev = ctx.nested.replace(Some(n.path.clone()));
        let q = ctx.to_filter(fq);
        ctx.nested.replace(prev);
        child = Arc::new(search_index::query::BooleanQuery::new().add(search_index::query::Occur::Filter, child).add(search_index::query::Occur::Filter, q?));
    }
    Ok(NestedSort { parent_filter, child_filter: child, max_children: n.max_children.map(|m| m.max(0) as usize) })
}

fn keyword_missing(m: Option<&Value>) -> EsResult<Missing> {
    Ok(match m {
        None => Missing::Last,
        Some(Value::Str(s)) if s == "_last" => Missing::Last,
        Some(Value::Str(s)) if s == "_first" => Missing::First,
        Some(v) => Missing::Value(SortValue::Bytes(super::query::value_text(v).into_bytes())),
    })
}

fn numeric_missing(f: &FieldInfo, m: Option<&Value>) -> EsResult<Missing> {
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
    Ok(Missing::Value(match f.kind {
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
                Some(p) => Some(Arc::new(crate::time::DateFormatter::new(p).map_err(EsError::illegal_argument)?)),
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
        Kind::Double => ValueFormat::Double,
        Kind::Float => ValueFormat::Float,
        Kind::HalfFloat => ValueFormat::HalfFloat,
        Kind::ScaledFloat(sf) => ValueFormat::ScaledFloat(sf),
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

/// A sort value as the response prints it.
pub fn render_value(v: &SortValue, f: &ValueFormat, doc: u32, score: f32) -> Value {
    match (f, v) {
        (ValueFormat::Score, _) => Value::Num(Number::Float(crate::fetch::java_float_as_double(score))),
        (ValueFormat::Doc, SortValue::Long(l)) => Value::from(*l),
        (ValueFormat::Doc, _) => Value::from(doc as i64),
        (_, SortValue::Null) => Value::Null,
        (ValueFormat::Ip, SortValue::Bytes(b)) if b.len() == 16 => Value::Str(search_index::query::range::inet_to_string(b)),
        (ValueFormat::Keyword, SortValue::Bytes(b)) => Value::Str(String::from_utf8_lossy(b).into_owned()),
        (ValueFormat::Tsid, SortValue::Bytes(b)) => Value::Str(crate::engine::tsdb::tsid_display(b)),
        (ValueFormat::UnsignedLong, SortValue::Long(l)) => {
            let u = (*l as u64) ^ (1 << 63);
            if u <= i64::MAX as u64 { Value::from(u as i64) } else { Value::Num(Number::UInt(u)) }
        }
        // A missing date (sorted with the type's extreme) prints as null once formatted.
        (ValueFormat::Date { format: Some(_), .. }, SortValue::Long(i64::MAX | i64::MIN)) => Value::Null,
        (ValueFormat::Date { nanos, format: Some(fmt), .. }, SortValue::Long(l)) => Value::Str(fmt.format(crate::fetch::instant_of(*nanos, *l), &crate::time::Zone::UTC)),
        (ValueFormat::ScaledFloat(sf), SortValue::Long(l)) => Value::Num(Number::Float(*l as f64 / sf)),
        (ValueFormat::Double, SortValue::Long(l)) => Value::Num(Number::Float(*l as f64)),
        (_, SortValue::Long(l)) => Value::from(*l),
        (_, SortValue::Double(d)) => Value::Num(Number::Float(*d)),
        (_, SortValue::Float(x)) => Value::Num(Number::Float(crate::fetch::java_float_as_double(*x))),
        (_, SortValue::Bytes(b)) => Value::Str(String::from_utf8_lossy(b).into_owned()),
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
        if let (SortValue::Long(l), ValueFormat::Date { nanos, field_nanos, .. }) = (&mut *v, f)
            && nanos != field_nanos
        {
            *l = convert_date(*l, *field_nanos, *nanos);
        }
    }
}

/// Lucene's `SortField.Type` of a sort key, as `search_after` errors name it.
fn sort_type_name(key: &SortKey, f: &ValueFormat) -> &'static str {
    match key {
        SortKey::Score { .. } => "SCORE",
        SortKey::Doc { .. } => "DOC",
        SortKey::GeoDistance(_) => "DOUBLE",
        SortKey::Script(_) => match f {
            ValueFormat::Keyword => "STRING",
            _ => "DOUBLE",
        },
        SortKey::Field(fs) => match (fs.kind, f) {
            (SortType::Keyword, _) => "STRING",
            (_, ValueFormat::ScaledFloat(_)) | (SortType::Double, _) => "DOUBLE",
            (SortType::Float | SortType::HalfFloat, _) => "FLOAT",
            (SortType::Int, _) => "INT",
            (SortType::Long, _) => "LONG",
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
            SortKey::Doc { .. } => SortValue::Long(int(v, false)?),
            SortKey::Field(fs) => match (fs.kind, f) {
                (SortType::Keyword, ValueFormat::Ip) => {
                    let s = super::query::value_text(v);
                    SortValue::Bytes(crate::document::values::parse_ip(&s).ok_or_else(|| EsError::illegal_argument(format!("'{s}' is not an IP string literal.")))?.to_vec())
                }
                (SortType::Keyword, ValueFormat::Tsid) => SortValue::Bytes(crate::engine::tsdb::tsid_parse(&super::query::value_text(v))?),
                (SortType::Keyword, _) => SortValue::Bytes(super::query::value_text(v).into_bytes()),
                (_, ValueFormat::ScaledFloat(sf)) => SortValue::Long((double(v)? * sf).round() as i64),
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
