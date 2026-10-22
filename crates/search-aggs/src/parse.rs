//! Parsing the `aggs` object (Elasticsearch's `AggregatorFactories.parseAggregators`) and
//! dispatching each aggregation type to its parser and, per shard, to its aggregator.
//!
//! The walk is token by token in document order, so the first error is Elasticsearch's first
//! error, with its exception type, text and location: `parsing_exception` with `line`/`col` for
//! structural problems (unknown types with suggestions, two type definitions, a missing
//! definition), `x_content_parse_exception` from the per-type `ObjectParser`s, and plain
//! `illegal_argument_exception`s where Elasticsearch throws them unwrapped (sub-aggregations under
//! a metric, duplicate sibling names, the nesting limit).

use std::sync::Arc;

use crate::bucket;
use crate::collect::{Aggregator, BindCtx};
use crate::error::{AggError, Result, suggest};
use crate::format::DocValueFormat;
use crate::json::Json;
use crate::metrics;
use crate::pipeline::{self, PipelineKind};
use crate::request::{AggDef, AggRequest, PipelineDef};
use crate::xparser::{Seg, Token, XParser};

/// `AggregatorFactories.MAX_NESTED_DEPTH`.
pub const MAX_NESTED_DEPTH: usize = 100;

/// Every aggregation type Elasticsearch 9.5 registers (for "did you mean" suggestions).
pub const ALL_TYPES: &[&str] = &[
    "avg",
    "sum",
    "min",
    "max",
    "stats",
    "extended_stats",
    "value_count",
    "percentiles",
    "percentile_ranks",
    "median_absolute_deviation",
    "cardinality",
    "global",
    "missing",
    "filter",
    "filters",
    "random_sampler",
    "sampler",
    "diversified_sampler",
    "terms",
    "rare_terms",
    "significant_terms",
    "significant_text",
    "range",
    "date_range",
    "ip_prefix",
    "ip_range",
    "histogram",
    "date_histogram",
    "variable_width_histogram",
    "geo_distance",
    "geohash_grid",
    "geotile_grid",
    "nested",
    "reverse_nested",
    "top_hits",
    "geo_bounds",
    "geo_centroid",
    "scripted_metric",
    "composite",
    "weighted_avg",
    "adjacency_matrix",
    "auto_date_histogram",
    "matrix_stats",
    "time_series",
    "children",
    "parent",
    "string_stats",
    "boxplot",
    "top_metrics",
    "t_test",
    "rate",
    "multi_terms",
    "categorize_text",
    "frequent_item_sets",
    "geo_line",
    "geohex_grid",
    "cartesian_bounds",
    "cartesian_centroid",
];

/// Metric (leaf) aggregation types: they can't have sub-aggregations.
pub fn is_metric(t: &str) -> bool {
    matches!(
        t,
        "avg"
            | "sum"
            | "min"
            | "max"
            | "stats"
            | "extended_stats"
            | "value_count"
            | "percentiles"
            | "percentile_ranks"
            | "median_absolute_deviation"
            | "cardinality"
            | "top_hits"
            | "geo_bounds"
            | "geo_centroid"
            | "scripted_metric"
            | "weighted_avg"
            | "matrix_stats"
            | "string_stats"
            | "boxplot"
            | "top_metrics"
            | "rate"
            | "t_test"
            | "geo_line"
            | "cartesian_bounds"
            | "cartesian_centroid"
    )
}

/// A parsed aggregation of any type.
#[derive(Clone, Debug)]
pub enum AggKind {
    Simple(metrics::simple::SimpleSpec),
    WeightedAvg(metrics::simple::WeightedAvgSpec),
    Cardinality(metrics::cardinality::CardinalitySpec),
    Percentiles(metrics::percentiles::PercentilesSpec),
    Sketch(metrics::sketch::SketchSpec),
    TopHits(metrics::top_hits::TopHitsSpec),
    StringStats(metrics::string_stats::StringStatsSpec),
    TopMetrics(metrics::top_metrics::TopMetricsSpec),
    Rate(metrics::rate::RateSpec),
    MatrixStats(metrics::matrix_stats::MatrixStatsSpec),
    ScriptedMetric(metrics::scripted_metric::ScriptedMetricSpec),
    TTest(metrics::t_test::TTestSpec),
    Cartesian(metrics::cartesian::CartesianSpec),
    GeoLine(metrics::geo_line::GeoLineSpec),
    TimeSeries(bucket::time_series::TimeSeriesSpec),
    Composite(bucket::composite::CompositeSpec),
    GeoGrid(bucket::geogrid::GridSpec),
    MultiTerms(bucket::multi_terms::MultiTermsSpec),
    RareTerms(bucket::rare_terms::RareTermsSpec),
    Significant(bucket::significant::SignificantSpec),
    SignificantText(bucket::significant::SigTextSpec),
    GeoMetric(metrics::geo::GeoMetricSpec),
    Terms(bucket::terms::TermsSpec),
    Histogram(bucket::histogram::HistogramSpec),
    DateHistogram(bucket::histogram::DateHistogramSpec),
    AutoDateHistogram(bucket::auto_histogram::AutoDateHistogramSpec),
    VariableWidthHistogram(bucket::variable_width::VariableWidthSpec),
    Join(bucket::join::JoinSpec),
    RandomSampler(bucket::random_sampler::RandomSamplerSpec),
    Range(bucket::range::RangeSpec),
    IpRange(bucket::range::IpRangeSpec),
    IpPrefix(bucket::range::IpPrefixSpec),
    Single(bucket::single::SingleSpec),
    Filters(bucket::filters::FiltersSpec),
    Adjacency(bucket::filters::AdjacencySpec),
}

/// Validates an embedded query at parse time (`query`, its path in the body).
pub type QueryCheck<'a> = &'a dyn Fn(&Json, &[Seg]) -> Result<()>;

/// Builds a `java.text.DecimalFormat` for a pattern.
pub type DecimalFormatFactory<'a> = &'a dyn Fn(&str) -> Result<Arc<dyn crate::format::NumberFormatter>>;

/// Parse-time options.
#[derive(Default)]
pub struct ParseOptions<'a> {
    /// The raw request body the `aggs` object came from (for `[line:col]` locations).
    pub src: Option<&'a [u8]>,
    /// Where the `aggs` object sits in the body (`["aggs"]`, `["aggregations"]`, …).
    pub base: Vec<Seg>,
    /// Validates an embedded query (filters) the way Elasticsearch parses it at this point.
    pub check_query: Option<QueryCheck<'a>>,
    /// Builds a `java.text.DecimalFormat` for pipeline `format`s.
    pub decimal_format: Option<DecimalFormatFactory<'a>>,
}

/// Parses an `aggs` object.
pub fn parse_aggs(aggs: &Json, opts: &ParseOptions<'_>) -> Result<AggRequest> {
    let mut p = XParser::new(aggs, opts.src, opts.base.clone());
    match p.next_token() {
        Some(Token::StartObject) => {}
        Some(t) => return Err(p.parsing_error(format!("Unexpected token {t} in [aggs]: aggregations definitions must start with the name of the aggregation."))),
        None => return Ok(AggRequest::default()),
    }
    let (aggs, pipelines) = parse_level(&mut p, 0, opts)?;
    Ok(AggRequest { aggs, pipelines, warnings: std::mem::take(&mut p.warnings) })
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && !n.contains(['[', ']', '>'])
}

type Level = (Vec<Arc<AggDef>>, Vec<Arc<PipelineDef>>);

fn parse_level(p: &mut XParser<'_>, level: usize, opts: &ParseOptions<'_>) -> Result<Level> {
    let mut aggs: Vec<Arc<AggDef>> = Vec::new();
    let mut pipelines: Vec<Arc<PipelineDef>> = Vec::new();
    loop {
        let t = p.next_token();
        match t {
            Some(Token::EndObject) | None => break,
            Some(Token::FieldName) => {}
            Some(t) => {
                return Err(p.parsing_error(format!(
                    "Unexpected token {t} in [aggs]: aggregations definitions must start with the name of the aggregation."
                )));
            }
        }
        let name = p.current_name().unwrap_or("").to_string();
        if !valid_name(&name) {
            return Err(p.parsing_error(format!(
                "Invalid aggregation name [{name}]. Aggregation names can contain any character except '[', ']', and '>'"
            )));
        }
        if level >= MAX_NESTED_DEPTH {
            return Err(p.parsing_error(format!(
                "The nested depth of the aggregations exceeds the maximum nested depth for aggregations of [{MAX_NESTED_DEPTH}]"
            )));
        }
        let t = p.next_token();
        if t != Some(Token::StartObject) {
            return Err(p.parsing_error(format!(
                "Aggregation definition for [{name} starts with a [{}], expected a [START_OBJECT].",
                t.map(Token::name).unwrap_or("null")
            )));
        }
        let mut kind: Option<(&'static str, Parsed)> = None;
        let mut subs: Option<Level> = None;
        let mut meta: Option<Json> = None;
        loop {
            let t = p.next_token();
            match t {
                Some(Token::EndObject) | None => break,
                Some(Token::FieldName) => {}
                Some(t) => {
                    return Err(p.parsing_error(format!("Expected [FIELD_NAME] under a [START_OBJECT], but got a [{t}] in [{name}]")));
                }
            }
            let field = p.current_name().unwrap_or("").to_string();
            let t = p.next_token();
            if t != Some(Token::StartObject) {
                return Err(p.parsing_error(format!(
                    "Expected [START_OBJECT] under [{field}], but got a [{}] in [{name}]",
                    t.map(Token::name).unwrap_or("null")
                )));
            }
            match field.as_str() {
                "meta" => meta = Some(p.map()?),
                "aggregations" | "aggs" => {
                    if subs.is_some() {
                        return Err(p.parsing_error(format!("Found two sub aggregation definitions under [{name}]")));
                    }
                    subs = Some(parse_level(p, level + 1, opts)?);
                }
                ty => {
                    if let Some((prev, _)) = &kind {
                        return Err(p.parsing_error(format!("Found two aggregation type definitions in [{name}]: [{prev}] and [{ty}]")));
                    }
                    let Some(static_ty) = ALL_TYPES.iter().chain(pipeline::PIPELINE_TYPES.iter()).find(|x| **x == ty).copied() else {
                        let loc = p.token_location();
                        let mut cands: Vec<&str> = ALL_TYPES.to_vec();
                        cands.extend_from_slice(pipeline::PIPELINE_TYPES);
                        return Err(AggError::parsing(loc, format!("Unknown aggregation type [{ty}]{}", suggest(ty, &cands)))
                            .caused_by(AggError::new(400, "named_object_not_found_exception", format!("[{}:{}] unknown field [{ty}]", loc.0, loc.1))));
                    };
                    kind = Some((static_ty, parse_type(p, static_ty, &name, opts)?));
                }
            }
        }
        let Some((type_name, parsed)) = kind else {
            return Err(p.parsing_error(format!("Missing definition for aggregation [{name}]")));
        };
        let meta = meta.map(Arc::new);
        match parsed {
            Parsed::Pipeline(k) => {
                if subs.is_some() {
                    return Err(AggError::illegal_argument(format!("Aggregation [{name}] cannot define sub-aggregations")));
                }
                pipelines.push(Arc::new(PipelineDef { name, type_name, meta, kind: k }));
            }
            Parsed::Agg(k) => {
                // `scripted_metric` accepts sub-aggregations and never builds them.
                if type_name == "scripted_metric" {
                    subs = subs.map(|(_, pipes)| (Vec::new(), pipes));
                }
                if subs.is_some() && type_name != "scripted_metric" && (is_metric(type_name) || type_name == "significant_text") {
                    return Err(AggError::illegal_argument(format!("Aggregator [{name}] of type [{type_name}] cannot accept sub-aggregations")));
                }
                if aggs.iter().any(|a| a.name == name) {
                    return Err(AggError::illegal_argument(format!("Two sibling aggregations cannot have the same name: [{name}]")));
                }
                let (sub_aggs, sub_pipes) = subs.unwrap_or_default();
                aggs.push(Arc::new(AggDef { name, type_name, meta, kind: k, subs: sub_aggs, pipelines: sub_pipes }));
            }
        }
    }
    Ok((aggs, pipelines))
}

/// One parsed definition (short-lived: variant sizes don't matter).
#[allow(clippy::large_enum_variant)]
enum Parsed {
    Agg(AggKind),
    Pipeline(PipelineKind),
}

fn unsupported(ty: &str) -> AggError {
    AggError::new(501, "unsupported_operation_exception", format!("aggregation [{ty}] is not supported by roll search yet"))
}

fn parse_type(p: &mut XParser<'_>, ty: &'static str, name: &str, opts: &ParseOptions<'_>) -> Result<Parsed> {
    use metrics::simple::SimpleKind;
    if pipeline::PIPELINE_TYPES.contains(&ty) {
        let factory = |pattern: &str| -> Result<DocValueFormat> {
            let f = match opts.decimal_format {
                Some(f) => f(pattern)?,
                None => crate::format::builtin_decimal_format(pattern)?,
            };
            Ok(DocValueFormat::Decimal(f))
        };
        return Ok(Parsed::Pipeline(pipeline::parse(p, ty, name, &factory)?));
    }
    let mut check = |p: &XParser<'_>| -> Result<()> {
        if let Some(c) = opts.check_query {
            c(p.value(), &p.path())?;
        }
        Ok(())
    };
    let kind = match ty {
        "avg" => AggKind::Simple(metrics::simple::parse(p, SimpleKind::Avg, name)?),
        "sum" => AggKind::Simple(metrics::simple::parse(p, SimpleKind::Sum, name)?),
        "min" => AggKind::Simple(metrics::simple::parse(p, SimpleKind::Min, name)?),
        "max" => AggKind::Simple(metrics::simple::parse(p, SimpleKind::Max, name)?),
        "value_count" => AggKind::Simple(metrics::simple::parse(p, SimpleKind::ValueCount, name)?),
        "stats" => AggKind::Simple(metrics::simple::parse(p, SimpleKind::Stats, name)?),
        "extended_stats" => AggKind::Simple(metrics::simple::parse(p, SimpleKind::ExtendedStats, name)?),
        "weighted_avg" => AggKind::WeightedAvg(metrics::simple::parse_weighted_avg(p)?),
        "cardinality" => AggKind::Cardinality(metrics::cardinality::parse(p, name)?),
        "percentiles" => AggKind::Percentiles(metrics::percentiles::parse(p, false, name)?),
        "percentile_ranks" => AggKind::Percentiles(metrics::percentiles::parse(p, true, name)?),
        "median_absolute_deviation" => AggKind::Sketch(metrics::sketch::parse(p, metrics::sketch::SketchKind::Mad, name)?),
        "boxplot" => AggKind::Sketch(metrics::sketch::parse(p, metrics::sketch::SketchKind::Boxplot, name)?),
        "top_hits" => AggKind::TopHits(metrics::top_hits::parse(p, name)?),
        "string_stats" => AggKind::StringStats(metrics::string_stats::parse(p)?),
        "top_metrics" => AggKind::TopMetrics(metrics::top_metrics::parse(p)?),
        "rate" => AggKind::Rate(metrics::rate::parse(p)?),
        "matrix_stats" => AggKind::MatrixStats(metrics::matrix_stats::parse(p, name)?),
        "scripted_metric" => AggKind::ScriptedMetric(metrics::scripted_metric::parse(p)?),
        "t_test" => AggKind::TTest(metrics::t_test::parse(p, name, &mut check)?),
        "geo_line" => AggKind::GeoLine(metrics::geo_line::parse(p)?),
        "time_series" => AggKind::TimeSeries(bucket::time_series::parse(p)?),
        "cartesian_bounds" => AggKind::Cartesian(metrics::cartesian::parse(p, metrics::cartesian::CartesianKind::Bounds)?),
        "cartesian_centroid" => AggKind::Cartesian(metrics::cartesian::parse(p, metrics::cartesian::CartesianKind::Centroid)?),
        "composite" => AggKind::Composite(bucket::composite::parse(p)?),
        "multi_terms" => AggKind::MultiTerms(bucket::multi_terms::parse(p, name)?),
        "rare_terms" => AggKind::RareTerms(bucket::rare_terms::parse(p, name)?),
        "significant_terms" => AggKind::Significant(bucket::significant::parse(p, name, &mut check)?),
        "significant_text" => AggKind::SignificantText(bucket::significant::parse_text(p, name, &mut check)?),
        "geohash_grid" => AggKind::GeoGrid(bucket::geogrid::parse(p, bucket::geogrid::GridKind::GeoHash, name)?),
        "geotile_grid" => AggKind::GeoGrid(bucket::geogrid::parse(p, bucket::geogrid::GridKind::GeoTile, name)?),
        "geohex_grid" => AggKind::GeoGrid(bucket::geogrid::parse(p, bucket::geogrid::GridKind::GeoHex, name)?),
        "geo_bounds" => AggKind::GeoMetric(metrics::geo::parse(p, metrics::geo::GeoMetricKind::Bounds)?),
        "geo_centroid" => AggKind::GeoMetric(metrics::geo::parse(p, metrics::geo::GeoMetricKind::Centroid)?),
        "terms" => AggKind::Terms(bucket::terms::parse(p, name)?),
        "histogram" => AggKind::Histogram(bucket::histogram::parse_histogram(p, name)?),
        "date_histogram" => AggKind::DateHistogram(bucket::histogram::parse_date_histogram(p, name)?),
        "auto_date_histogram" => AggKind::AutoDateHistogram(bucket::auto_histogram::parse(p, name)?),
        "variable_width_histogram" => AggKind::VariableWidthHistogram(bucket::variable_width::parse(p, name)?),
        "children" => AggKind::Join(bucket::join::parse(p, name, false)?),
        "parent" => AggKind::Join(bucket::join::parse(p, name, true)?),
        "random_sampler" => AggKind::RandomSampler(bucket::random_sampler::parse(p)?),
        "range" | "date_range" | "geo_distance" => AggKind::Range(bucket::range::parse_range(p, ty)?),
        "ip_range" => AggKind::IpRange(bucket::range::parse_ip_range(p)?),
        "ip_prefix" => AggKind::IpPrefix(bucket::range::parse_ip_prefix(p)?),
        "filter" => {
            check(p)?;
            let q = p.take_value();
            AggKind::Single(bucket::single::SingleSpec::Filter(q))
        }
        "global" => {
            // GlobalAggregationBuilder.parse consumes exactly one token.
            p.next_token();
            AggKind::Single(bucket::single::SingleSpec::Global)
        }
        "missing" => {
            let opts = crate::values::VsOpts::new(true, true, false);
            let fields = crate::values::VsSpec::fields(opts);
            let names: Vec<&'static str> = fields.iter().map(|f| f.name).collect();
            let mut vs = crate::values::VsSpec::default();
            p.object("missing", &fields, |p, i| {
                vs.parse_field(p, names[i])?;
                Ok(())
            })?;
            vs.check_required(opts)?;
            AggKind::Single(bucket::single::SingleSpec::Missing(vs))
        }
        "nested" => AggKind::Single(bucket::single::parse_path_agg(p, name, true)?),
        "reverse_nested" => AggKind::Single(bucket::single::parse_path_agg(p, name, false)?),
        "sampler" => AggKind::Single(bucket::single::parse_sampler(p, name)?),
        "diversified_sampler" => AggKind::Single(bucket::single::parse_diversified(p, name)?),
        "filters" => AggKind::Filters(bucket::filters::parse_filters(p, name, &mut check)?),
        "adjacency_matrix" => AggKind::Adjacency(bucket::filters::parse_adjacency(p, &mut check)?),
        other => return Err(unsupported(other)),
    };
    Ok(Parsed::Agg(kind))
}

/// Binds a definition to a shard.
pub fn create(def: &Arc<AggDef>, ctx: &mut BindCtx<'_>) -> Result<Box<dyn Aggregator>> {
    match &def.kind {
        AggKind::Simple(s) => metrics::simple::create(def, s, ctx),
        AggKind::WeightedAvg(s) => metrics::simple::create_weighted_avg(def, s, ctx),
        AggKind::Cardinality(s) => metrics::cardinality::create(def, s, ctx),
        AggKind::Percentiles(s) => metrics::percentiles::create(def, s, ctx),
        AggKind::Sketch(s) => metrics::sketch::create(def, s, ctx),
        AggKind::TopHits(s) => metrics::top_hits::create(def, s, ctx),
        AggKind::StringStats(s) => metrics::string_stats::create(def, s, ctx),
        AggKind::TopMetrics(s) => metrics::top_metrics::create(def, s, ctx),
        AggKind::Rate(s) => metrics::rate::create(def, s, ctx),
        AggKind::MatrixStats(s) => metrics::matrix_stats::create(def, s, ctx),
        AggKind::ScriptedMetric(s) => metrics::scripted_metric::create(def, s, ctx),
        AggKind::TTest(s) => metrics::t_test::create(def, s, ctx),
        AggKind::Cartesian(s) => metrics::cartesian::create(def, s, ctx),
        AggKind::GeoLine(s) => metrics::geo_line::create(def, s, ctx),
        AggKind::TimeSeries(s) => bucket::time_series::create(def, s, ctx),
        AggKind::Composite(s) => bucket::composite::create(def, s, ctx),
        AggKind::GeoGrid(s) => bucket::geogrid::create(def, s, ctx),
        AggKind::MultiTerms(s) => bucket::multi_terms::create(def, s, ctx),
        AggKind::RareTerms(s) => bucket::rare_terms::create(def, s, ctx),
        AggKind::Significant(s) => bucket::significant::create(def, s, ctx),
        AggKind::SignificantText(s) => bucket::significant::create_text(def, s, ctx),
        AggKind::GeoMetric(s) => metrics::geo::create(def, s, ctx),
        AggKind::Terms(s) => bucket::terms::create(def, s, ctx),
        AggKind::Histogram(s) => bucket::histogram::create_histogram(def, s, ctx),
        AggKind::DateHistogram(s) => bucket::histogram::create_date_histogram(def, s, ctx),
        AggKind::AutoDateHistogram(s) => bucket::auto_histogram::create(def, s, ctx),
        AggKind::VariableWidthHistogram(s) => bucket::variable_width::create(def, s, ctx),
        AggKind::Join(s) => bucket::join::create(def, s, ctx),
        AggKind::RandomSampler(s) => bucket::random_sampler::create(def, s, ctx),
        AggKind::Range(s) => bucket::range::create_range(def, s, ctx),
        AggKind::IpRange(s) => bucket::range::create_ip_range(def, s, ctx),
        AggKind::IpPrefix(s) => bucket::range::create_ip_prefix(def, s, ctx),
        AggKind::Single(s) => bucket::single::create(def, s, ctx),
        AggKind::Filters(s) => bucket::filters::create_filters(def, s, ctx),
        AggKind::Adjacency(s) => bucket::filters::create_adjacency(def, s, ctx),
    }
}

/// Whether a histogram definition has `min_doc_count: 0` (pipeline validation).
pub fn histogram_min_doc_count_zero(def: &AggDef) -> bool {
    match &def.kind {
        AggKind::Histogram(h) => h.min_doc_count == 0,
        AggKind::DateHistogram(h) => h.min_doc_count == 0,
        _ => true,
    }
}
