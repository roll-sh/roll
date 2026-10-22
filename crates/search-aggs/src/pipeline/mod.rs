//! Pipeline aggregations: parsing, request validation, and their application at the final
//! reduce.
//!
//! - Sibling pipelines (`avg_bucket`, `sum_bucket`, `min_bucket`, `max_bucket`, `stats_bucket`,
//!   `extended_stats_bucket`, `percentiles_bucket`) read the buckets of a multi-bucket
//!   aggregation named by `buckets_path` and add one result next to it.
//! - Parent pipelines (`derivative`, `cumulative_sum`, `cumulative_cardinality`, `serial_diff`,
//!   `moving_fn`, `bucket_script`, `bucket_selector`, `bucket_sort`, `normalize`) rewrite the
//!   buckets of the multi-bucket aggregation they are declared in.
//!
//! Application follows `InternalAggregations.topLevelReduce`: each aggregation first applies
//! the pipelines of its sub-aggregations bucket by bucket, then its own pipelines in dependency
//! order (`resolvePipelineAggregatorOrder`); top-level sibling pipelines run last, each seeing
//! the results of the ones before it. `buckets_path` values resolve like
//! `BucketHelpers.resolveBucketValue`, with the `gap_policy`.

pub mod movfn;
pub mod values;

use std::sync::Arc;

use crate::error::{AggError, Result};
use crate::format::DocValueFormat;
use crate::internal::{Agg, AggValue, Aggs, MultiBuckets, Prop, ReduceCtx, bucket_property};
use crate::json::Json;
use crate::metrics::simple::{ExtendedStats, Stats};
use crate::request::{AggDef, AggPath, GapPolicy, PathElement, PipelineDef};
use crate::script::{Script, ScriptKind};
use crate::xparser::{Field, Token, Tokens, XParser};

use self::movfn::NativeMovFn;
use self::values::{BucketMetricValue, Derivative, ExtendedStatsBucket, PercentilesBucket, SimpleLongValue, SimpleValue, StatsBucket};

/// Which `*_bucket` aggregation.
#[derive(Clone, Debug, PartialEq)]
pub enum BucketMetricKind {
    Avg,
    Sum,
    Min,
    Max,
    Stats,
    ExtendedStats { sigma: f64 },
    Percentiles { percents: Vec<f64>, keyed: bool },
}

/// A parsed pipeline aggregation.
#[derive(Clone, Debug)]
pub enum PipelineKind {
    BucketMetric { kind: BucketMetricKind, path: String, gap: GapPolicy, format: DocValueFormat },
    CumulativeSum { paths: Vec<String>, format: DocValueFormat },
    CumulativeCardinality { path: String, format: DocValueFormat },
    Derivative { paths: Vec<String>, gap: GapPolicy, format: DocValueFormat, unit: Option<String> },
    SerialDiff { paths: Vec<String>, gap: GapPolicy, format: DocValueFormat, lag: i32 },
    MovFn { path: String, script: Script, native: Option<NativeMovFn>, window: i32, shift: i32, gap: GapPolicy, format: DocValueFormat },
    BucketScript { paths: Vec<(String, String)>, script: Script, gap: GapPolicy, format: DocValueFormat },
    BucketSelector { paths: Vec<(String, String)>, script: Script, gap: GapPolicy },
    BucketSort { sorts: Vec<(String, bool)>, from: i32, size: Option<i32>, gap: GapPolicy },
    Normalize { paths: Vec<String>, method: String, format: DocValueFormat },
}

impl PipelineKind {
    pub fn buckets_paths(&self) -> Vec<String> {
        match self {
            PipelineKind::BucketMetric { path, .. } | PipelineKind::CumulativeCardinality { path, .. } | PipelineKind::MovFn { path, .. } => {
                vec![path.clone()]
            }
            PipelineKind::CumulativeSum { paths, .. }
            | PipelineKind::Derivative { paths, .. }
            | PipelineKind::SerialDiff { paths, .. }
            | PipelineKind::Normalize { paths, .. } => paths.clone(),
            PipelineKind::BucketScript { paths, .. } | PipelineKind::BucketSelector { paths, .. } => {
                // `new TreeMap<>(bucketsPathsMap).values()`.
                let mut p = paths.clone();
                p.sort_by(|a, b| a.0.cmp(&b.0));
                p.into_iter().map(|x| x.1).collect()
            }
            PipelineKind::BucketSort { sorts, .. } => sorts.iter().map(|s| s.0.clone()).collect(),
        }
    }

    /// Sibling pipelines add a result next to the aggregation they read.
    pub fn is_sibling(&self) -> bool {
        matches!(self, PipelineKind::BucketMetric { .. })
    }
}

/// Names of every pipeline aggregation type.
pub const PIPELINE_TYPES: &[&str] = &[
    "avg_bucket",
    "sum_bucket",
    "min_bucket",
    "max_bucket",
    "stats_bucket",
    "extended_stats_bucket",
    "percentiles_bucket",
    "cumulative_sum",
    "cumulative_cardinality",
    "derivative",
    "serial_diff",
    "moving_fn",
    "bucket_script",
    "bucket_selector",
    "bucket_sort",
    "normalize",
    "moving_percentiles",
    "inference",
    "bucket_count_ks_test",
    "bucket_correlation",
    "change_point",
];

/// How pipeline `format`s become formatters.
pub type DecimalFactory<'a> = &'a dyn Fn(&str) -> Result<DocValueFormat>;

fn format_of(f: Option<String>, factory: DecimalFactory<'_>) -> Result<DocValueFormat> {
    match f {
        None => Ok(DocValueFormat::Raw),
        Some(s) => factory(&s),
    }
}

/// Parses a pipeline aggregation body (the parser is on its `{`).
pub fn parse(p: &mut XParser<'_>, type_name: &str, name: &str, formats: DecimalFactory<'_>) -> Result<PipelineKind> {
    match type_name {
        "avg_bucket" | "sum_bucket" | "min_bucket" | "max_bucket" | "stats_bucket" | "extended_stats_bucket" | "percentiles_bucket" => {
            parse_bucket_metrics(p, type_name, name, formats)
        }
        "derivative" | "serial_diff" => parse_derivative_like(p, type_name, name, formats),
        "cumulative_sum" | "cumulative_cardinality" | "normalize" => parse_constructing(p, type_name, formats),
        "moving_fn" => parse_movfn(p, formats),
        "bucket_script" => parse_bucket_script(p, formats),
        "bucket_selector" => parse_bucket_selector(p, name),
        "bucket_sort" => parse_bucket_sort(p),
        other => Err(AggError::new(501, "unsupported_operation_exception", format!("pipeline aggregation [{other}] is not supported by roll search yet"))),
    }
}

fn parse_buckets_path_array(p: &mut XParser<'_>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    loop {
        match p.next_token() {
            Some(Token::EndArray) | None => break,
            _ => out.push(p.text()?),
        }
    }
    Ok(out)
}

/// `BucketMetricsParser.parse`.
fn parse_bucket_metrics(p: &mut XParser<'_>, type_name: &str, name: &str, formats: DecimalFactory<'_>) -> Result<PipelineKind> {
    let mut paths: Option<Vec<String>> = None;
    let mut format = None;
    let mut gap = None;
    let mut percents: Option<Vec<f64>> = None;
    let mut keyed = true;
    let mut sigma = 2.0;
    let mut cur = String::new();
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => cur = p.current_name().unwrap_or("").to_string(),
            Token::String if cur == "format" => format = Some(p.text()?),
            Token::String if cur == "buckets_path" => paths = Some(vec![p.text()?]),
            Token::String if cur == "gap_policy" => gap = Some(GapPolicy::parse(p)?),
            Token::StartArray if cur == "buckets_path" => paths = Some(parse_buckets_path_array(p)?),
            Token::StartArray if cur == "percents" && type_name == "percentiles_bucket" => {
                let mut v = Vec::new();
                loop {
                    match p.next_token() {
                        Some(Token::EndArray) | None => break,
                        _ => v.push(p.double_value()?),
                    }
                }
                percents = Some(v);
            }
            Token::Boolean if cur == "keyed" && type_name == "percentiles_bucket" => keyed = p.boolean_value()?,
            Token::Number if cur == "sigma" && type_name == "extended_stats_bucket" => sigma = p.double_value()?,
            t => return Err(p.parsing_error(format!("Unexpected token {t} [{cur}] in [{name}]"))),
        }
    }
    let Some(paths) = paths else {
        return Err(p.parsing_error(format!("Missing required field [buckets_path] for aggregation [{name}]")));
    };
    let kind = match type_name {
        "avg_bucket" => BucketMetricKind::Avg,
        "sum_bucket" => BucketMetricKind::Sum,
        "min_bucket" => BucketMetricKind::Min,
        "max_bucket" => BucketMetricKind::Max,
        "stats_bucket" => BucketMetricKind::Stats,
        "extended_stats_bucket" => {
            if sigma < 0.0 {
                return Err(AggError::illegal_argument("sigma must be a non-negative double"));
            }
            BucketMetricKind::ExtendedStats { sigma }
        }
        _ => {
            let percents = percents.unwrap_or_else(|| vec![1.0, 5.0, 25.0, 50.0, 75.0, 95.0, 99.0]);
            if percents.iter().any(|p| !(0.0..=100.0).contains(p)) {
                return Err(AggError::illegal_argument("percents must only contain non-null doubles from 0.0-100.0 inclusive"));
            }
            BucketMetricKind::Percentiles { percents, keyed }
        }
    };
    let path = paths.first().cloned().unwrap_or_default();
    let _ = paths;
    Ok(PipelineKind::BucketMetric { kind, path, gap: gap.unwrap_or_default(), format: format_of(format, formats)? })
}

/// `derivative` and `serial_diff` parsers.
fn parse_derivative_like(p: &mut XParser<'_>, type_name: &str, name: &str, formats: DecimalFactory<'_>) -> Result<PipelineKind> {
    let mut paths: Option<Vec<String>> = None;
    let mut format = None;
    let mut gap = None;
    let mut unit = None;
    let mut lag = 1;
    let mut cur = String::new();
    let serial = type_name == "serial_diff";
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => cur = p.current_name().unwrap_or("").to_string(),
            Token::String => match cur.as_str() {
                "format" => format = Some(p.text()?),
                "buckets_path" => paths = Some(vec![p.text()?]),
                "gap_policy" => gap = Some(GapPolicy::parse(p)?),
                "unit" if !serial => unit = Some(p.text()?),
                _ => return Err(p.parsing_error(format!("Unknown key for a VALUE_STRING in [{name}]: [{cur}]."))),
            },
            Token::Number if serial && cur == "lag" => {
                lag = p.int_value()?;
                if lag <= 0 {
                    return Err(p.parsing_error(format!("Lag must be a positive, non-zero integer.  Value supplied was{lag} in [{name}]: [{cur}].")));
                }
            }
            Token::Number => return Err(p.parsing_error(format!("Unknown key for a VALUE_NUMBER in [{name}]: [{cur}]."))),
            Token::StartArray if cur == "buckets_path" => paths = Some(parse_buckets_path_array(p)?),
            Token::StartArray => return Err(p.parsing_error(format!("Unknown key for a START_ARRAY in [{name}]: [{cur}]."))),
            t => return Err(p.parsing_error(format!("Unexpected token {t} in [{name}]."))),
        }
    }
    let Some(paths) = paths else {
        return Err(p.parsing_error(format!("Missing required field [buckets_path] for derivative aggregation [{name}]")));
    };
    let format = format_of(format, formats)?;
    let gap = gap.unwrap_or_default();
    Ok(if serial { PipelineKind::SerialDiff { paths, gap, format, lag } } else { PipelineKind::Derivative { paths, gap, format, unit } })
}

/// `cumulative_sum`, `cumulative_cardinality`, `normalize` (`ConstructingObjectParser`s).
fn parse_constructing(p: &mut XParser<'_>, type_name: &str, formats: DecimalFactory<'_>) -> Result<PipelineKind> {
    let normalize = type_name == "normalize";
    let mut fields = vec![
        Field::new("buckets_path", if type_name == "cumulative_cardinality" { Tokens::STRING } else { Tokens::STRING_ARRAY }),
        Field::new("format", Tokens::STRING),
    ];
    if normalize {
        fields.push(Field::new("method", Tokens::STRING));
    }
    let mut paths: Option<Vec<String>> = None;
    let mut format = None;
    let mut method: Option<String> = None;
    p.object(type_name, &fields, |p, i| {
        match i {
            0 => {
                paths = Some(if p.token() == Token::StartArray { parse_buckets_path_array(p)? } else { vec![p.text()?] });
            }
            1 => format = Some(p.text()?),
            _ => {
                let m = p.text()?;
                if !matches!(m.as_str(), "rescale_0_1" | "rescale_0_100" | "mean" | "z-score" | "percent_of_sum" | "softmax") {
                    return Err(AggError::illegal_argument(format!("invalid method [{m}]")));
                }
                method = Some(m);
            }
        }
        Ok(())
    })?;
    let mut missing = Vec::new();
    if paths.is_none() {
        missing.push("buckets_path");
    }
    if normalize && method.is_none() {
        missing.push("method");
    }
    if !missing.is_empty() {
        return Err(AggError::illegal_argument(format!("Required [{}]", missing.join(", "))));
    }
    let paths = paths.unwrap_or_default();
    let format = format_of(format, formats)?;
    Ok(match type_name {
        "cumulative_sum" => PipelineKind::CumulativeSum { paths, format },
        "cumulative_cardinality" => PipelineKind::CumulativeCardinality { path: paths.into_iter().next().unwrap_or_default(), format },
        _ => PipelineKind::Normalize { paths, method: method.unwrap_or_default(), format },
    })
}

fn parse_movfn(p: &mut XParser<'_>, formats: DecimalFactory<'_>) -> Result<PipelineKind> {
    let fields = [
        Field::new("buckets_path", Tokens::STRING),
        Field::new("script", Tokens::OBJECT_OR_STRING),
        Field::new("window", Tokens::INT),
        Field::new("shift", Tokens::INT),
        Field::new("format", Tokens::STRING),
        Field::new("gap_policy", Tokens::STRING),
    ];
    let mut path = None;
    let mut script = None;
    let mut window = None;
    let mut shift = 0;
    let mut format = None;
    let mut gap = GapPolicy::Skip;
    p.object("moving_fn", &fields, |p, i| {
        match i {
            0 => path = Some(p.text()?),
            1 => script = Some(Script::parse(p)?),
            2 => window = Some(p.int_value()?),
            3 => shift = p.int_value()?,
            4 => format = Some(p.text()?),
            _ => {
                let t = p.text()?.to_lowercase();
                gap = match t.as_str() {
                    "insert_zeros" => GapPolicy::InsertZeros,
                    "skip" => GapPolicy::Skip,
                    "keep_values" => GapPolicy::KeepValues,
                    _ => return Err(p.parsing_error(format!("Invalid gap policy: [{t}], accepted values: [insert_zeros, skip, keep_values]"))),
                };
            }
        }
        Ok(())
    })?;
    let mut missing = Vec::new();
    if path.is_none() {
        missing.push("buckets_path");
    }
    if script.is_none() {
        missing.push("script");
    }
    if window.is_none() {
        missing.push("window");
    }
    if !missing.is_empty() {
        return Err(AggError::illegal_argument(format!("Required [{}]", missing.join(", "))));
    }
    let window = window.unwrap_or(0);
    if window <= 0 {
        return Err(AggError::illegal_argument("[window] must be a positive, non-zero integer."));
    }
    let script = script.expect("script");
    let native = script.source.as_deref().and_then(NativeMovFn::parse);
    Ok(PipelineKind::MovFn { path: path.unwrap_or_default(), script, native, window, shift, gap, format: format_of(format, formats)? })
}

fn parse_paths_map(p: &mut XParser<'_>) -> Result<Vec<(String, String)>> {
    Ok(match p.token() {
        Token::String => vec![("_value".to_string(), p.text()?)],
        Token::StartArray => parse_buckets_path_array(p)?.into_iter().enumerate().map(|(i, s)| (format!("_value{i}"), s)).collect(),
        _ => {
            let m = p.map()?;
            m.as_object()
                .unwrap_or(&[])
                .iter()
                .map(|(k, v)| (k.clone(), match v {
                    Json::Str(s) => s.clone(),
                    other => other.to_java_string(),
                }))
                .collect()
        }
    })
}

fn parse_bucket_script(p: &mut XParser<'_>, formats: DecimalFactory<'_>) -> Result<PipelineKind> {
    let fields = [
        Field::new("buckets_path", Tokens::OBJECT_ARRAY_OR_STRING),
        Field::new("script", Tokens::OBJECT_OR_STRING),
        Field::new("format", Tokens::STRING),
        Field::new("gap_policy", Tokens::STRING),
    ];
    let mut paths = None;
    let mut script = None;
    let mut format = None;
    let mut gap = GapPolicy::Skip;
    p.object("bucket_script", &fields, |p, i| {
        match i {
            0 => paths = Some(parse_paths_map(p)?),
            1 => script = Some(Script::parse(p)?),
            2 => format = Some(p.text()?),
            _ => {
                let t = p.text()?.to_lowercase();
                gap = match t.as_str() {
                    "insert_zeros" => GapPolicy::InsertZeros,
                    "skip" => GapPolicy::Skip,
                    "keep_values" => GapPolicy::KeepValues,
                    _ => return Err(p.parsing_error(format!("Invalid gap policy: [{t}], accepted values: [insert_zeros, skip, keep_values]"))),
                };
            }
        }
        Ok(())
    })?;
    let mut missing = Vec::new();
    if paths.is_none() {
        missing.push("buckets_path");
    }
    if script.is_none() {
        missing.push("script");
    }
    if !missing.is_empty() {
        return Err(AggError::illegal_argument(format!("Required [{}]", missing.join(", "))));
    }
    Ok(PipelineKind::BucketScript { paths: paths.unwrap_or_default(), script: script.expect("script"), gap, format: format_of(format, formats)? })
}

fn parse_bucket_selector(p: &mut XParser<'_>, name: &str) -> Result<PipelineKind> {
    let mut paths: Option<Vec<(String, String)>> = None;
    let mut script = None;
    let mut gap = None;
    let mut cur = String::new();
    while let Some(t) = p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => cur = p.current_name().unwrap_or("").to_string(),
            Token::String => match cur.as_str() {
                "buckets_path" => paths = Some(vec![("_value".into(), p.text()?)]),
                "gap_policy" => gap = Some(GapPolicy::parse(p)?),
                "script" => script = Some(Script::parse(p)?),
                _ => return Err(p.parsing_error(format!("Unknown key for a VALUE_STRING in [{name}]: [{cur}]."))),
            },
            Token::StartArray if cur == "buckets_path" => {
                paths = Some(parse_buckets_path_array(p)?.into_iter().enumerate().map(|(i, s)| (format!("_value{i}"), s)).collect())
            }
            Token::StartArray => return Err(p.parsing_error(format!("Unknown key for a START_ARRAY in [{name}]: [{cur}]."))),
            Token::StartObject if cur == "script" => script = Some(Script::parse(p)?),
            Token::StartObject if cur == "buckets_path" => {
                let m = p.map()?;
                paths = Some(
                    m.as_object()
                        .unwrap_or(&[])
                        .iter()
                        .map(|(k, v)| (k.clone(), match v {
                            Json::Str(s) => s.clone(),
                            other => other.to_java_string(),
                        }))
                        .collect(),
                );
            }
            Token::StartObject => return Err(p.parsing_error(format!("Unknown key for a START_OBJECT in [{name}]: [{cur}]."))),
            t => return Err(p.parsing_error(format!("Unexpected token {t} in [{name}]."))),
        }
    }
    let Some(paths) = paths else {
        return Err(p.parsing_error(format!("Missing required field [buckets_path] for bucket_selector aggregation [{name}]")));
    };
    let Some(script) = script else {
        return Err(p.parsing_error(format!("Missing required field [script] for bucket_selector aggregation [{name}]")));
    };
    Ok(PipelineKind::BucketSelector { paths, script, gap: gap.unwrap_or_default() })
}

/// `bucket_sort` (`sort` follows `SortBuilder.fromXContent`: field names, `{field: order}`,
/// `{field: {order}}`).
fn parse_bucket_sort(p: &mut XParser<'_>) -> Result<PipelineKind> {
    let fields = [
        Field::new("sort", Tokens::OBJECT_ARRAY_OR_STRING),
        Field::new("from", Tokens::INT),
        Field::new("size", Tokens::INT),
        Field::new("gap_policy", Tokens::STRING),
    ];
    let mut sorts = Vec::new();
    let mut from = 0;
    let mut size = None;
    let mut gap = GapPolicy::Skip;
    p.object("bucket_sort", &fields, |p, i| {
        match i {
            0 => {
                let v = p.take_value();
                sorts = parse_sorts(&v)?;
            }
            1 => {
                let v = p.int_value()?;
                if v < 0 {
                    return Err(AggError::illegal_argument(format!("[from] must be a non-negative integer: [{v}]")));
                }
                from = v;
            }
            2 => {
                let v = p.int_value()?;
                if v <= 0 {
                    return Err(AggError::illegal_argument(format!("[size] must be a positive integer: [{v}]")));
                }
                size = Some(v);
            }
            _ => {
                let t = p.text()?.to_lowercase();
                gap = match t.as_str() {
                    "insert_zeros" => GapPolicy::InsertZeros,
                    "skip" => GapPolicy::Skip,
                    "keep_values" => GapPolicy::KeepValues,
                    _ => return Err(p.parsing_error(format!("Invalid gap policy: [{t}], accepted values: [insert_zeros, skip, keep_values]"))),
                };
            }
        }
        Ok(())
    })?;
    Ok(PipelineKind::BucketSort { sorts, from, size, gap })
}

fn parse_sorts(v: &Json) -> Result<Vec<(String, bool)>> {
    let one = |e: &Json, out: &mut Vec<(String, bool)>| -> Result<()> {
        match e {
            Json::Str(s) => {
                out.push((s.clone(), s != "_score"));
                Ok(())
            }
            Json::Obj(m) => {
                for (k, v) in m {
                    let order = match v {
                        Json::Str(o) => o.clone(),
                        Json::Obj(_) => v.get("order").and_then(Json::as_str).unwrap_or(if k == "_score" { "desc" } else { "asc" }).to_string(),
                        _ => "asc".into(),
                    };
                    let asc = match order.to_lowercase().as_str() {
                        "asc" => true,
                        "desc" => false,
                        _ => return Err(AggError::illegal_argument(format!("Unknown SortOrder [{order}]"))),
                    };
                    if k == "_score" || k == "_doc" || k == "_geo_distance" || k == "_script" {
                        return Err(AggError::illegal_argument(format!("[bucket_sort] only supports field based sorting; incompatible sort: [{k}]")));
                    }
                    out.push((k.clone(), asc));
                }
                Ok(())
            }
            _ => Err(AggError::illegal_argument("malformed sort")),
        }
    };
    let mut out = Vec::new();
    match v {
        Json::Arr(a) => {
            for e in a {
                one(e, &mut out)?;
            }
        }
        e => one(e, &mut out)?,
    }
    Ok(out)
}

/// `AggregationBuilder.bucketCardinality()`.
fn bucket_cardinality(def: &AggDef) -> u8 {
    match def.type_name {
        "filter" | "global" | "missing" | "nested" | "reverse_nested" | "sampler" | "diversified_sampler" | "random_sampler" | "children" | "parent" => 1,
        t if crate::parse::is_metric(t) => 0,
        _ => 2,
    }
}

/// The builder class Elasticsearch names in some validation messages.
fn builder_class(type_name: &str) -> String {
    let pkg = |p: &str, c: &str| format!("org.elasticsearch.search.aggregations.{p}.{c}");
    match type_name {
        "avg" => pkg("metrics", "AvgAggregationBuilder"),
        "sum" => pkg("metrics", "SumAggregationBuilder"),
        "min" => pkg("metrics", "MinAggregationBuilder"),
        "max" => pkg("metrics", "MaxAggregationBuilder"),
        "stats" => pkg("metrics", "StatsAggregationBuilder"),
        "extended_stats" => pkg("metrics", "ExtendedStatsAggregationBuilder"),
        "value_count" => pkg("metrics", "ValueCountAggregationBuilder"),
        "cardinality" => pkg("metrics", "CardinalityAggregationBuilder"),
        "percentiles" => pkg("metrics", "PercentilesAggregationBuilder"),
        "filter" => pkg("bucket.filter", "FilterAggregationBuilder"),
        "nested" => pkg("bucket.nested", "NestedAggregationBuilder"),
        "global" => pkg("bucket.global", "GlobalAggregationBuilder"),
        other => format!("org.elasticsearch.search.aggregations.{other}"),
    }
}

/// `AggregatorFactories.Builder.validate`: the messages of an `action_request_validation_exception`.
pub fn validate(aggs: &[Arc<AggDef>], pipelines: &[Arc<PipelineDef>]) -> Vec<String> {
    let mut errors = Vec::new();
    validate_level(aggs, pipelines, None, &mut errors);
    validate_children(aggs, &mut errors);
    errors
}

fn validate_children(aggs: &[Arc<AggDef>], errors: &mut Vec<String>) {
    for a in aggs {
        validate_level(&a.subs, &a.pipelines, Some(a), errors);
        validate_children(&a.subs, errors);
    }
}

fn validate_level(aggs: &[Arc<AggDef>], pipelines: &[Arc<PipelineDef>], parent: Option<&AggDef>, errors: &mut Vec<String>) {
    let ordered = match resolve_order(aggs, pipelines) {
        Ok(o) => o,
        Err(e) => {
            errors.push(e);
            return;
        }
    };
    for p in ordered {
        validate_pipeline(&p, aggs, parent, errors);
    }
}

fn sequential_ordered(type_name: &str, name: &str, parent: Option<&AggDef>, without_gaps: bool, errors: &mut Vec<String>) {
    match parent {
        None => errors.push(format!(
            "{type_name} aggregation [{name}] must have a histogram, date_histogram or auto_date_histogram as parent but doesn't have a parent"
        )),
        Some(p) => match p.type_name {
            "histogram" | "date_histogram" => {
                if without_gaps && !crate::parse::histogram_min_doc_count_zero(p) {
                    errors.push(format!("parent histogram of {type_name} aggregation [{name}] must have min_doc_count of 0"));
                }
            }
            "auto_date_histogram" => {}
            _ => errors.push(format!("{type_name} aggregation [{name}] must have a histogram, date_histogram or auto_date_histogram as parent")),
        },
    }
}

fn validate_pipeline(p: &PipelineDef, siblings: &[Arc<AggDef>], parent: Option<&AggDef>, errors: &mut Vec<String>) {
    let name = &p.name;
    let t = p.type_name;
    let paths = p.kind.buckets_paths();
    match &p.kind {
        PipelineKind::BucketMetric { .. } => {
            if paths.len() != 1 {
                errors.push(format!("buckets_path must contain a single entry for aggregation [{name}]"));
                return;
            }
            let Ok(path) = AggPath::parse(&paths[0]) else { return };
            let els = &path.elements;
            let mut pos = 0;
            let mut cur = &els[pos];
            pos += 1;
            let mut agg = siblings.iter().find(|a| a.name == cur.name).cloned();
            if agg.is_none() {
                errors.push(format!("buckets_path aggregation does not exist for aggregation [{name}]: {}", paths[0]));
                return;
            }
            while let Some(a) = agg.clone() {
                if pos >= els.len() {
                    break;
                }
                let card = bucket_cardinality(&a);
                let go = (card == 2 && cur.key.is_some()) || (card == 1 && !a.subs.is_empty());
                if !go {
                    break;
                }
                cur = &els[pos];
                pos += 1;
                agg = a.subs.iter().find(|s| s.name == cur.name).cloned();
            }
            match agg {
                None => {
                    let shown: Vec<String> = els[..pos]
                        .iter()
                        .flat_map(|e| {
                            let mut v = vec![e.name.clone()];
                            if let Some(k) = &e.key {
                                v.push(k.clone());
                            }
                            if let Some(m) = &e.metric {
                                v.push(m.clone());
                            }
                            v
                        })
                        .collect();
                    errors.push(format!("buckets_path aggregation does not exist for aggregation [{name}]: [{}]", shown.join(", ")));
                }
                Some(a) => {
                    if bucket_cardinality(&a) != 2 {
                        errors.push(format!(
                            "Unable to find unqualified multi-bucket aggregation in buckets_path. Path must include a multi-bucket aggregation for aggregation [{name}] found :{} for buckets path: {}",
                            builder_class(a.type_name),
                            paths[0]
                        ));
                    }
                }
            }
        }
        PipelineKind::CumulativeSum { .. } => {
            if paths.len() != 1 {
                errors.push(format!("buckets_path must contain a single entry for aggregation [{name}]"));
            }
            sequential_ordered(t, name, parent, true, errors);
        }
        PipelineKind::Derivative { .. } => {
            if paths.len() != 1 {
                errors.push(format!("buckets_path must contain a single entry for aggregation [{name}]"));
            }
            sequential_ordered(t, name, parent, true, errors);
        }
        PipelineKind::CumulativeCardinality { .. } => {
            if paths.len() != 1 {
                errors.push(format!("buckets_path must contain a single entry for aggregation [{name}]"));
            }
            sequential_ordered(t, name, parent, false, errors);
        }
        PipelineKind::SerialDiff { .. } => sequential_ordered(t, name, parent, false, errors),
        PipelineKind::MovFn { .. } => sequential_ordered(t, name, parent, true, errors),
        PipelineKind::BucketScript { .. } | PipelineKind::BucketSelector { .. } => {
            if parent.is_none() {
                errors.push(format!("{t} aggregation [{name}] must be declared inside of another aggregation"));
            }
        }
        PipelineKind::BucketSort { sorts, from, size, .. } => {
            if parent.is_none() {
                errors.push(format!("{t} aggregation [{name}] must be declared inside of another aggregation"));
            }
            if sorts.is_empty() && size.is_none() && *from == 0 {
                errors.push(format!("[{name}] is configured to perform nothing. Please set either of [sort, size, from] to use bucket_sort"));
            }
        }
        PipelineKind::Normalize { .. } => {
            if paths.len() != 1 {
                errors.push(format!("buckets_path must contain a single entry for aggregation [{name}]"));
            }
            if parent.is_none() {
                errors.push(format!("{t} aggregation [{name}] must be declared inside of another aggregation"));
            }
        }
    }
}

/// `resolvePipelineAggregatorOrder`: dependency order of a level's pipelines.
pub fn resolve_order(aggs: &[Arc<AggDef>], pipelines: &[Arc<PipelineDef>]) -> std::result::Result<Vec<Arc<PipelineDef>>, String> {
    let mut ordered: Vec<Arc<PipelineDef>> = Vec::new();
    let mut unmarked: Vec<Arc<PipelineDef>> = pipelines.to_vec();
    let mut temp: Vec<String> = Vec::new();
    while let Some(first) = unmarked.first().cloned() {
        visit(&first, aggs, pipelines, &mut ordered, &mut unmarked, &mut temp)?;
    }
    Ok(ordered)
}

fn visit(
    p: &Arc<PipelineDef>,
    aggs: &[Arc<AggDef>],
    pipelines: &[Arc<PipelineDef>],
    ordered: &mut Vec<Arc<PipelineDef>>,
    unmarked: &mut Vec<Arc<PipelineDef>>,
    temp: &mut Vec<String>,
) -> std::result::Result<(), String> {
    if temp.contains(&p.name) {
        return Err(format!("Cyclical dependency found with pipeline aggregator [{}]", p.name));
    }
    if !unmarked.iter().any(|u| Arc::ptr_eq(u, p)) {
        return Ok(());
    }
    temp.push(p.name.clone());
    for bp in p.kind.buckets_paths() {
        let Ok(path) = AggPath::parse(&bp) else { continue };
        let Some(first) = path.elements.first() else { continue };
        if bp == "_count" || bp == "_key" {
            continue;
        }
        if let Some(mut agg) = aggs.iter().find(|a| a.name == first.name).cloned() {
            for i in 1..path.elements.len() {
                let el = &path.elements[i];
                let last = i == path.elements.len() - 1;
                if last && (el.name.eq_ignore_ascii_case("_key") || el.name == "_count") {
                    break;
                }
                if let Some(sub) = agg.subs.iter().find(|s| s.name == el.name) {
                    agg = sub.clone();
                    continue;
                }
                if last && agg.pipelines.iter().any(|sp| sp.name == el.name) {
                    continue;
                }
                temp.retain(|n| n != &p.name);
                return Err(format!("No aggregation [{}] found for path [{bp}]", el.name));
            }
            continue;
        }
        match pipelines.iter().find(|x| x.name == first.name) {
            Some(dep) => visit(dep, aggs, pipelines, ordered, unmarked, temp)?,
            None => return Err(format!("No aggregation found for path [{bp}]")),
        }
    }
    unmarked.retain(|u| !Arc::ptr_eq(u, p));
    temp.retain(|n| n != &p.name);
    ordered.push(p.clone());
    Ok(())
}

/// Applies pipelines to the top level of a final reduce.
pub fn apply_top(aggs: &mut Aggs, defs: &[Arc<AggDef>], pipelines: &[Arc<PipelineDef>], ctx: &mut ReduceCtx<'_>) -> Result<()> {
    for agg in aggs.list.iter_mut() {
        if let Some(def) = defs.iter().find(|d| d.name == agg.name) {
            reduce_pipelines(agg, def, ctx)?;
        }
    }
    let ordered = resolve_order(defs, pipelines).map_err(AggError::illegal_argument)?;
    for p in ordered {
        let v = sibling_reduce(&p, aggs, ctx)?;
        aggs.list.push(Agg { name: p.name.clone(), meta: p.meta.clone(), value: v });
    }
    Ok(())
}

/// `InternalAggregation.reducePipelines` for one aggregation and its definition.
fn reduce_pipelines(agg: &mut Agg, def: &AggDef, ctx: &mut ReduceCtx<'_>) -> Result<()> {
    if !def.subs.is_empty() {
        if let Some(m) = agg.value.multi_mut() {
            for i in 0..m.len() {
                let bucket_aggs = m.aggs_mut(i);
                for sub in bucket_aggs.list.iter_mut() {
                    if let Some(sd) = def.subs.iter().find(|d| d.name == sub.name) {
                        reduce_pipelines(sub, sd, ctx)?;
                    }
                }
            }
        } else if let Some(a) = agg.value.single_aggs_mut() {
            for sub in a.list.iter_mut() {
                if let Some(sd) = def.subs.iter().find(|d| d.name == sub.name) {
                    reduce_pipelines(sub, sd, ctx)?;
                }
            }
        }
    }
    if def.pipelines.is_empty() {
        return Ok(());
    }
    let ordered = resolve_order(&def.subs, &def.pipelines).map_err(AggError::illegal_argument)?;
    for p in ordered {
        if p.kind.is_sibling() {
            // SiblingPipelineAggregator.reduce: one result appended to every bucket.
            if let Some(m) = agg.value.multi_mut() {
                for i in 0..m.len() {
                    let v = sibling_reduce(&p, m.aggs(i), ctx)?;
                    m.aggs_mut(i).list.push(Agg { name: p.name.clone(), meta: p.meta.clone(), value: v });
                }
            } else if let Some(a) = agg.value.single_aggs_mut() {
                let v = sibling_reduce(&p, a, ctx)?;
                a.list.push(Agg { name: p.name.clone(), meta: p.meta.clone(), value: v });
            }
        } else {
            parent_reduce(&p, agg, ctx)?;
        }
    }
    Ok(())
}

/// The value a `buckets_path` resolves to in a bucket (`BucketHelpers.resolveBucketValue`);
/// `None` when the path doesn't resolve (an invalid aggregation path).
pub fn resolve_bucket_value<M: MultiBuckets + ?Sized>(m: &M, agg_name: &str, i: usize, path: &[String], gap: Option<GapPolicy>) -> Result<Option<f64>> {
    let prop = match bucket_property(agg_name, m.doc_count(i), || m.key(i), m.aggs(i), path) {
        Ok(p) => p,
        Err(e) if e.kind == "invalid_aggregation_path_exception" => return Ok(None),
        Err(e) => return Err(e),
    };
    let value = prop_to_f64(prop, agg_name, path)?;
    if path.len() == 1 && path[0] == "_count" {
        return Ok(Some(value));
    }
    Ok(Some(match gap {
        Some(g) => g.process(m.doc_count(i), value),
        None => value,
    }))
}

fn prop_to_f64(prop: Prop<'_>, agg_name: &str, path: &[String]) -> Result<f64> {
    let (cur_name, cur_class): (String, &str) = (path.first().cloned().unwrap_or_else(|| agg_name.to_string()), "");
    let _ = cur_class;
    match prop {
        Prop::Num(v) => Ok(v),
        Prop::Agg(a) => {
            if let Some(v) = a.value.single_value() {
                return Ok(v);
            }
            if a.value.is_multi_value_metric() {
                let last = path.last().cloned().unwrap_or_default();
                return match a.value.multi_value(&last) {
                    Ok(v) => Ok(v),
                    Err(e) if e.kind == "number_format_exception" => Err(AggError::illegal_argument(format!(
                        "buckets_path must reference either a number value or a single value numeric metric aggregation, but [{cur_name}] contains multiple values. Please specify which to use."
                    ))),
                    Err(e) => Err(e),
                };
            }
            Err(AggError::illegal_argument(format!(
                "buckets_path must reference either a number value or a single value numeric metric aggregation, got: [{}] at aggregation [{cur_name}]",
                a.value.class_name()
            )))
        }
        Prop::Value(v) => Err(AggError::illegal_argument(format!(
            "buckets_path must reference either a number value or a single value numeric metric aggregation, got: [{}] at aggregation [{cur_name}]",
            match v {
                Json::Str(_) => "String",
                Json::Int(_) => "Long",
                Json::Float(_) => "Double",
                Json::Bool(_) => "Boolean",
                Json::Arr(_) => "String[]",
                _ => "Object",
            }
        ))),
        Prop::Array(_) => Err(AggError::illegal_argument(format!(
            "buckets_path must reference either a number value or a single value numeric metric aggregation, got: [Object[]] at aggregation [{cur_name}]"
        ))),
    }
}

/// `BucketMetricsPipelineAggregator.doReduce` over the aggregations of one level.
fn sibling_reduce(p: &PipelineDef, aggs: &Aggs, ctx: &mut ReduceCtx<'_>) -> Result<AggValue> {
    let _ = &ctx;
    let PipelineKind::BucketMetric { kind, path, gap, format } = &p.kind else {
        return Err(AggError::illegal_state(format!("[{}] is not a sibling pipeline", p.name)));
    };
    let parsed = AggPath::parse(path)?;
    let els = &parsed.elements;
    let mut collector = MetricCollector::new(kind.clone());
    for a in &aggs.list {
        if a.name != els[0].name {
            continue;
        }
        let mut idx = 0usize;
        let mut cur: Option<&Agg> = Some(a);
        while idx < els.len() - 1 {
            let Some(c) = cur else {
                return Err(AggError::invalid_path(format!(
                    "bucket_path [{path}] expected aggregation with name [{}] but was missing in search response",
                    els[idx].name
                )));
            };
            if let Some(sub) = c.value.single_aggs() {
                idx += 1;
                cur = sub.get(&els[idx].name);
            } else if let Some(key) = &els[idx].key {
                match c.value.multi() {
                    Some(m) => {
                        let bi = (0..m.len()).find(|&i| m.key_string(i) == *key);
                        let Some(bi) = bi else {
                            return Err(AggError::invalid_path(format!(
                                "missing bucket [{key}] for agg [{}] while extracting bucket path [{path}]",
                                c.name
                            )));
                        };
                        if idx == els.len() - 1 {
                            return Err(AggError::execution(format!("invalid bucket path ends at [{key}]")));
                        }
                        idx += 1;
                        cur = m.aggs(bi).get(&els[idx].name);
                    }
                    None => {
                        return Err(AggError::invalid_path(format!(
                            "bucket_path [{path}] indicates bucket_key [{key}] at position [{idx}] but encountered on agg [{}] which is not a multi_bucket aggregation",
                            c.name
                        )));
                    }
                }
            } else {
                break;
            }
        }
        let Some(c) = cur else {
            return Err(AggError::invalid_path("did not find multi-bucket aggregation for extraction."));
        };
        let Some(m) = c.value.multi() else {
            return Err(AggError::invalid_path(format!("did not find multi-bucket aggregation for extraction. Found [{}]", c.name)));
        };
        let rest: Vec<String> = flatten_path(&els[idx..]).into_iter().skip(1).collect();
        for i in 0..m.len() {
            if let Some(v) = resolve_bucket_value(m, &c.name, i, &rest, Some(*gap))?
                && !v.is_nan()
            {
                collector.collect(m.key_string(i), v);
            }
        }
    }
    Ok(collector.build(format.clone(), &p.name))
}

fn flatten_path(els: &[PathElement]) -> Vec<String> {
    let mut out = Vec::new();
    for e in els {
        out.push(e.name.clone());
        if let Some(k) = &e.key {
            out.push(k.clone());
        }
        if let Some(m) = &e.metric {
            out.push(m.clone());
        }
    }
    out
}

struct MetricCollector {
    kind: BucketMetricKind,
    count: u64,
    sum: f64,
    min: f64,
    max: f64,
    sum_sq: f64,
    keys: Vec<String>,
    best: f64,
    data: Vec<f64>,
}

impl MetricCollector {
    fn new(kind: BucketMetricKind) -> MetricCollector {
        let best = match kind {
            BucketMetricKind::Min => f64::INFINITY,
            _ => f64::NEG_INFINITY,
        };
        MetricCollector { kind, count: 0, sum: 0.0, min: f64::INFINITY, max: f64::NEG_INFINITY, sum_sq: 0.0, keys: Vec::new(), best, data: Vec::new() }
    }

    fn collect(&mut self, key: String, v: f64) {
        match &self.kind {
            BucketMetricKind::Max => {
                if v > self.best {
                    self.keys.clear();
                    self.keys.push(key);
                    self.best = v;
                } else if v == self.best || (v.is_nan() && self.best.is_nan()) {
                    self.keys.push(key);
                }
            }
            BucketMetricKind::Min => {
                if v < self.best {
                    self.keys.clear();
                    self.keys.push(key);
                    self.best = v;
                } else if v == self.best || (v.is_nan() && self.best.is_nan()) {
                    self.keys.push(key);
                }
            }
            BucketMetricKind::Percentiles { .. } => self.data.push(v),
            _ => {
                self.count += 1;
                self.sum += v;
                self.min = crate::metrics::simple::java_min(self.min, v);
                self.max = crate::metrics::simple::java_max(self.max, v);
                self.sum_sq += v * v;
            }
        }
    }

    fn build(mut self, format: DocValueFormat, name: &str) -> AggValue {
        match &self.kind {
            BucketMetricKind::Avg => {
                AggValue::SimpleValue(Box::new(SimpleValue { value: if self.count == 0 { f64::NAN } else { self.sum / self.count as f64 }, format }))
            }
            BucketMetricKind::Sum => AggValue::SimpleValue(Box::new(SimpleValue { value: self.sum, format })),
            BucketMetricKind::Max | BucketMetricKind::Min => {
                AggValue::BucketMetricValue(Box::new(BucketMetricValue { keys: self.keys, value: self.best, format }))
            }
            BucketMetricKind::Stats => {
                AggValue::StatsBucket(Box::new(StatsBucket(Stats { count: self.count, sum: self.sum, min: self.min, max: self.max, format })))
            }
            BucketMetricKind::ExtendedStats { sigma } => AggValue::ExtendedStatsBucket(Box::new(ExtendedStatsBucket(ExtendedStats {
                count: self.count,
                sum: self.sum,
                min: self.min,
                max: self.max,
                sum_of_sqrs: self.sum_sq,
                sigma: *sigma,
                format,
            }))),
            BucketMetricKind::Percentiles { percents, keyed } => {
                // Collections.sort on Doubles: Double.compareTo (NaN last, -0.0 < 0.0).
                self.data.sort_by(|a, b| crate::request::java_double_compare(*a, *b));
                let values = percents
                    .iter()
                    .map(|p| {
                        if self.data.is_empty() {
                            f64::NAN
                        } else {
                            let idx = crate::format::java_round((p / 100.0) * (self.data.len() - 1) as f64) as usize;
                            self.data[idx.min(self.data.len() - 1)]
                        }
                    })
                    .collect();
                AggValue::PercentilesBucket(Box::new(PercentilesBucket { percents: percents.clone(), values, keyed: *keyed, format, name: name.to_string() }))
            }
        }
    }
}

/// The x-axis unit of a derivative (`unit`) in millis.
fn derivative_units(unit: &str) -> Result<f64> {
    use crate::rounding::DateUnit;
    if let Some(u) = DateUnit::parse_calendar(unit) {
        return Ok(match u {
            DateUnit::Year => 31_556_952_000.0,
            DateUnit::Quarter => 7_889_238_000.0,
            DateUnit::Month => 2_629_746_000.0,
            DateUnit::Week => 604_800_000.0,
            DateUnit::Day => 86_400_000.0,
            DateUnit::Hour => 3_600_000.0,
            DateUnit::Minute => 60_000.0,
            DateUnit::Second => 1000.0,
        });
    }
    Ok(crate::rounding::parse_time_value_millis(unit, "DerivativePipelineAggregationBuilder.unit")? as f64)
}

fn push_value(m: &mut dyn MultiBuckets, i: usize, p: &PipelineDef, v: AggValue) {
    m.aggs_mut(i).list.push(Agg { name: p.name.clone(), meta: p.meta.clone(), value: v });
}

/// Applies a parent pipeline to the multi-bucket aggregation it is declared in.
fn parent_reduce(p: &PipelineDef, agg: &mut Agg, ctx: &mut ReduceCtx<'_>) -> Result<()> {
    let agg_name = agg.name.clone();
    let histogram_like = is_histogram_like(&agg.value);
    let class = agg.value.class_name();
    let Some(m) = agg.value.multi_mut() else {
        return Err(AggError::class_cast(format!(
            "class org.elasticsearch.search.aggregations.{class} cannot be cast to class org.elasticsearch.search.aggregations.InternalMultiBucketAggregation"
        )));
    };
    let needs_histogram = matches!(
        p.kind,
        PipelineKind::Derivative { .. } | PipelineKind::CumulativeSum { .. } | PipelineKind::SerialDiff { .. } | PipelineKind::MovFn { .. } | PipelineKind::CumulativeCardinality { .. }
    );
    if needs_histogram && !histogram_like {
        return Err(AggError::class_cast(format!(
            "class org.elasticsearch.search.aggregations.bucket.{class} cannot be cast to class org.elasticsearch.search.aggregations.bucket.histogram.HistogramFactory"
        )));
    }
    let list = |path: &str| -> Result<Vec<String>> { Ok(AggPath::parse(path)?.as_string_list()) };
    match &p.kind {
        PipelineKind::CumulativeSum { paths, format } => {
            let path = list(&paths[0])?;
            let mut sum = 0.0;
            for i in 0..m.len() {
                let v = resolve_bucket_value(m, &agg_name, i, &path, Some(GapPolicy::InsertZeros))?;
                if let Some(v) = v
                    && v.is_finite()
                {
                    sum += v;
                }
                push_value(m, i, p, AggValue::SimpleValue(Box::new(SimpleValue { value: sum, format: format.clone() })));
            }
        }
        PipelineKind::Derivative { paths, gap, format, unit } => {
            let path = list(&paths[0])?;
            let units = match unit {
                Some(u) => Some(derivative_units(u)?),
                None => None,
            };
            let mut last: Option<(f64, Option<f64>)> = None;
            for i in 0..m.len() {
                let key = m.numeric_key(i).unwrap_or(0.0);
                let v = resolve_bucket_value(m, &agg_name, i, &path, Some(*gap))?;
                if let (Some((lk, Some(lv))), Some(tv)) = (last, v) {
                    let gradient = tv - lv;
                    let x_diff = match units {
                        Some(u) => (key - lk) / u,
                        None => -1.0,
                    };
                    push_value(m, i, p, AggValue::Derivative(Box::new(Derivative { value: gradient, normalization_factor: x_diff, format: format.clone() })));
                }
                last = Some((key, v));
            }
        }
        PipelineKind::SerialDiff { paths, gap, format, lag } => {
            let path = list(&paths[0])?;
            let lag = *lag as usize;
            if lag >= m.len() {
                return Ok(());
            }
            let mut window: std::collections::VecDeque<f64> = std::collections::VecDeque::new();
            for i in 0..m.len() {
                let v = resolve_bucket_value(m, &agg_name, i, &path, Some(*gap))?.unwrap_or(f64::NAN);
                let lag_value = if i < lag { f64::NAN } else { window.front().copied().unwrap_or(f64::NAN) };
                if !v.is_nan() && !lag_value.is_nan() {
                    push_value(m, i, p, AggValue::SimpleValue(Box::new(SimpleValue { value: v - lag_value, format: format.clone() })));
                }
                window.push_back(v);
                if window.len() > lag {
                    window.pop_front();
                }
            }
        }
        PipelineKind::MovFn { path, script, native, window, shift, gap, format } => {
            let path = list(path)?;
            let resolved: Vec<Option<f64>> = (0..m.len()).map(|i| resolve_bucket_value(m, &agg_name, i, &path, Some(*gap))).collect::<Result<_>>()?;
            let values: Vec<f64> = resolved.iter().filter_map(|v| *v).filter(|v| !v.is_nan()).collect();
            let host_script = match native {
                Some(_) => None,
                None => {
                    let host = ctx.opts.scripts.clone().ok_or_else(|| AggError::illegal_argument("scripts are not supported"))?;
                    Some(host.moving_fn(script)?)
                }
            };
            let clamp = |x: i64| -> usize { x.clamp(0, values.len() as i64) as usize };
            let mut index: i64 = 0;
            for (i, v) in resolved.iter().enumerate() {
                let Some(v) = v else { continue };
                if v.is_nan() {
                    continue;
                }
                let from = clamp(index - *window as i64 + *shift as i64);
                let to = clamp(index + *shift as i64);
                let win = if from <= to { &values[from..to] } else { &values[0..0] };
                let result = match (native, &host_script) {
                    (Some(n), _) => n.eval(win, &script.params)?,
                    (None, Some(h)) => h.run(win, &script.params)?,
                    _ => f64::NAN,
                };
                push_value(m, i, p, AggValue::SimpleValue(Box::new(SimpleValue { value: result, format: format.clone() })));
                index += 1;
            }
        }
        PipelineKind::CumulativeCardinality { path, format } => {
            let path = list(path)?;
            let mut hll: Option<crate::metrics::cardinality::Hll> = None;
            let mut cardinality: i64 = 0;
            for i in 0..m.len() {
                let state = crate::metrics::cardinality::bucket_hll(m, &agg_name, i, &path)?;
                if let Some(s) = state {
                    let h = hll.get_or_insert_with(|| crate::metrics::cardinality::Hll::new(s.precision()));
                    h.merge(s);
                    cardinality = h.cardinality() as i64;
                }
                push_value(m, i, p, AggValue::SimpleLongValue(Box::new(SimpleLongValue { value: cardinality, format: format.clone() })));
            }
        }
        PipelineKind::BucketScript { paths, script, gap, format } => {
            let host = ctx.opts.scripts.clone().ok_or_else(|| AggError::illegal_argument("scripts are not supported"))?;
            let compiled = host.bucket_script(script, ScriptKind::BucketScript)?;
            let parsed: Vec<(String, Vec<String>)> = paths.iter().map(|(k, v)| Ok((k.clone(), list(v)?))).collect::<Result<_>>()?;
            for i in 0..m.len() {
                let mut vars: Vec<(String, Json)> = script.params.as_object().map(|o| o.to_vec()).unwrap_or_default();
                let mut skip = false;
                for (var, path) in &parsed {
                    let v = resolve_bucket_value(m, &agg_name, i, path, Some(*gap))?;
                    if gap.is_skippable() && v.is_none_or(|x| x.is_nan()) {
                        skip = true;
                        break;
                    }
                    vars.push((var.clone(), v.map(Json::Float).unwrap_or(Json::Null)));
                }
                if skip {
                    continue;
                }
                let r = compiled.run(&vars)?;
                if r.is_null() {
                    continue;
                }
                let value = r.as_f64().ok_or_else(|| AggError::illegal_argument("bucket_script must return a number"))?;
                push_value(m, i, p, AggValue::SimpleValue(Box::new(SimpleValue { value, format: format.clone() })));
            }
        }
        PipelineKind::BucketSelector { paths, script, gap } => {
            let parsed: Vec<(String, Vec<String>)> = paths.iter().map(|(k, v)| Ok((k.clone(), list(v)?))).collect::<Result<_>>()?;
            let native = native_selector(script);
            let compiled = match native {
                Some(_) => None,
                None => {
                    let host = ctx.opts.scripts.clone().ok_or_else(|| AggError::illegal_argument("scripts are not supported"))?;
                    Some(host.bucket_script(script, ScriptKind::BucketSelector)?)
                }
            };
            let mut keep = Vec::with_capacity(m.len());
            for i in 0..m.len() {
                let mut vars: Vec<(String, Json)> = script.params.as_object().map(|o| o.to_vec()).unwrap_or_default();
                for (var, path) in &parsed {
                    let v = resolve_bucket_value(m, &agg_name, i, path, Some(*gap))?;
                    vars.push((var.clone(), v.map(Json::Float).unwrap_or(Json::Null)));
                }
                let k = match (&native, &compiled) {
                    (Some(n), _) => n.eval(&vars)?,
                    (None, Some(c)) => match c.run(&vars)? {
                        Json::Bool(b) => b,
                        other => return Err(AggError::illegal_argument(format!("bucket_selector script must return a boolean, got [{other}]"))),
                    },
                    _ => true,
                };
                keep.push(k);
            }
            m.retain_buckets(&keep);
        }
        PipelineKind::BucketSort { sorts, from, size, gap } => {
            let n = m.len();
            let from = *from as usize;
            let size = size.map(|s| s as usize).unwrap_or(n);
            if from >= n {
                m.retain_buckets(&vec![false; n]);
                return Ok(());
            }
            if sorts.is_empty() {
                let keep: Vec<bool> = (0..n).map(|i| i >= from && i < (from + size).min(n)).collect();
                m.retain_buckets(&keep);
                return Ok(());
            }
            enum SV {
                Key(Json),
                Num(f64),
            }
            let parsed: Vec<(Vec<String>, bool, bool)> = sorts.iter().map(|(f, asc)| Ok((list(f)?, *asc, f == "_key"))).collect::<Result<_>>()?;
            let mut rows: Vec<(usize, Vec<Option<SV>>)> = Vec::new();
            for i in 0..n {
                let mut vals = Vec::with_capacity(parsed.len());
                for (path, _, is_key) in &parsed {
                    if *is_key {
                        vals.push(Some(SV::Key(m.key(i))));
                        continue;
                    }
                    let v = resolve_bucket_value(m, &agg_name, i, path, Some(*gap))?;
                    if gap.is_skippable() && v.is_none_or(|x| x.is_nan()) {
                        vals.push(None);
                    } else {
                        vals.push(v.map(SV::Num));
                    }
                }
                if vals.iter().all(|v| v.is_none()) {
                    continue;
                }
                rows.push((i, vals));
            }
            let cmp_sv = |a: &SV, b: &SV| -> std::cmp::Ordering {
                match (a, b) {
                    (SV::Num(x), SV::Num(y)) => crate::request::java_double_compare(*x, *y),
                    (SV::Key(x), SV::Key(y)) => match (x, y) {
                        (Json::Str(s), Json::Str(t)) => s.encode_utf16().cmp(t.encode_utf16()),
                        _ => crate::request::java_double_compare(x.as_f64().unwrap_or(f64::NAN), y.as_f64().unwrap_or(f64::NAN)),
                    },
                    _ => std::cmp::Ordering::Equal,
                }
            };
            rows.sort_by(|a, b| {
                for (k, (_, asc, _)) in parsed.iter().enumerate() {
                    let c = match (&a.1[k], &b.1[k]) {
                        (None, None) => continue,
                        (None, _) => return std::cmp::Ordering::Greater,
                        (_, None) => return std::cmp::Ordering::Less,
                        (Some(x), Some(y)) => {
                            let c = cmp_sv(x, y);
                            if *asc { c } else { c.reverse() }
                        }
                    };
                    if c != std::cmp::Ordering::Equal {
                        return c;
                    }
                }
                std::cmp::Ordering::Equal
            });
            let limit = (from + size).min(rows.len());
            let order: Vec<usize> = if from < limit { rows[from..limit].iter().map(|r| r.0).collect() } else { Vec::new() };
            m.reorder(&order);
        }
        PipelineKind::Normalize { paths, method, format } => {
            let path = list(&paths[0])?;
            let values: Vec<f64> = (0..m.len())
                .map(|i| resolve_bucket_value(m, &agg_name, i, &path, Some(GapPolicy::Skip)).map(|v| v.unwrap_or(f64::NAN)))
                .collect::<Result<_>>()?;
            let f = normalizer(method, &values);
            for (i, &v) in values.iter().enumerate() {
                let out = if v.is_nan() { f64::NAN } else { f(v) };
                push_value(m, i, p, AggValue::SimpleValue(Box::new(SimpleValue { value: out, format: format.clone() })));
            }
        }
        PipelineKind::BucketMetric { .. } => {}
    }
    Ok(())
}

/// `NormalizePipelineMethods`.
fn normalizer(method: &str, values: &[f64]) -> Box<dyn Fn(f64) -> f64> {
    let mut count = 0usize;
    let mut sum = 0.0;
    let mut min = f64::MAX;
    // `Double.MIN_VALUE` (the smallest positive double), as Elasticsearch initializes it.
    let mut max = f64::from_bits(1);
    for &v in values {
        if !v.is_nan() {
            count += 1;
            min = crate::metrics::simple::java_min(v, min);
            max = crate::metrics::simple::java_max(v, max);
            sum += v;
        }
    }
    let mean = if count == 0 { f64::NAN } else { sum / count as f64 };
    match method {
        "rescale_0_1" => Box::new(move |v| (v - min) / (max - min)),
        "rescale_0_100" => Box::new(move |v| 100.0 * (v - min) / (max - min)),
        "mean" => Box::new(move |v| (v - mean) / (max - min)),
        "percent_of_sum" => Box::new(move |v| v / sum),
        "z-score" => {
            let mut var = 0.0;
            for &v in values {
                if !v.is_nan() {
                    var += (v - mean) * (v - mean);
                }
            }
            let stdev = (var / count as f64).sqrt();
            Box::new(move |v| (v - mean) / stdev)
        }
        _ => {
            let mut sum_exp = 0.0;
            for &v in values {
                if !v.is_nan() {
                    sum_exp += v.exp();
                }
            }
            Box::new(move |v| v.exp() / sum_exp)
        }
    }
}

/// A native `bucket_selector` for `params.x <op> <literal|params.y>` scripts.
struct NativeSelector {
    lhs: Operand,
    op: &'static str,
    rhs: Operand,
}

enum Operand {
    Var(String),
    Num(f64),
}

fn native_selector(script: &Script) -> Option<NativeSelector> {
    let src = script.source.as_deref()?.trim().trim_end_matches(';').trim();
    let src = src.strip_prefix("return ").unwrap_or(src).trim();
    for op in [">=", "<=", "==", "!=", ">", "<"] {
        if let Some((a, b)) = src.split_once(op) {
            let parse = |s: &str| -> Option<Operand> {
                let s = s.trim();
                if let Some(v) = s.strip_prefix("params.") {
                    if v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !v.is_empty() {
                        return Some(Operand::Var(v.to_string()));
                    }
                    return None;
                }
                s.parse::<f64>().ok().map(Operand::Num)
            };
            let (l, r) = (parse(a)?, parse(b)?);
            return Some(NativeSelector { lhs: l, op, rhs: r });
        }
    }
    None
}

impl NativeSelector {
    fn eval(&self, vars: &[(String, Json)]) -> Result<bool> {
        let get = |o: &Operand| -> Result<f64> {
            match o {
                Operand::Num(n) => Ok(*n),
                Operand::Var(v) => match vars.iter().rev().find(|(k, _)| k == v) {
                    Some((_, Json::Null)) | None => Err(AggError::new(
                        400,
                        "script_exception",
                        "runtime error".to_string(),
                    )),
                    Some((_, j)) => j.as_f64().ok_or_else(|| AggError::illegal_argument(format!("params.{v} is not a number"))),
                },
            }
        };
        let (a, b) = (get(&self.lhs)?, get(&self.rhs)?);
        Ok(match self.op {
            ">=" => a >= b,
            "<=" => a <= b,
            "==" => a == b,
            "!=" => a != b,
            ">" => a > b,
            _ => a < b,
        })
    }
}

/// Results that implement Elasticsearch's `HistogramFactory`.
fn is_histogram_like(v: &AggValue) -> bool {
    matches!(v, AggValue::Histogram(_) | AggValue::DateHistogram(_) | AggValue::AutoDateHistogram(_) | AggValue::VariableWidthHistogram(_))
}
