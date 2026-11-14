//! Rollup job configurations (`RollupJobConfig`, `GroupConfig`, `DateHistogramGroupConfig`,
//! `HistogramGroupConfig`, `TermsGroupConfig`, `MetricConfig`): parsing with Elasticsearch's
//! messages, the validation against the source indices' field capabilities, the canonical
//! form `GET _rollup/job` and the rollup index's `_meta` print, and the capabilities
//! (`RollupJobCaps`) the caps APIs and `_rollup_search` read.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;

/// `RollupField.SUPPORTED_METRICS` in Elasticsearch's order (a `HashSet` prints them so).
const SUPPORTED_METRICS: [&str; 5] = ["avg", "min", "max", "sum", "value_count"];
/// `RollupField.SUPPORTED_DATE_METRICS`.
const SUPPORTED_DATE_METRICS: [&str; 3] = ["max", "min", "value_count"];
/// `RollupField.NUMERIC_FIELD_MAPPER_TYPES`.
const NUMERIC_TYPES: [&str; 8] = ["half_float", "float", "double", "byte", "short", "integer", "long", "scaled_float"];
/// `RollupField.DATE_FIELD_MAPPER_TYPES`.
const DATE_TYPES: [&str; 2] = ["date", "date_nanos"];

/// The interval kind of a date_histogram group: what its interval key is printed as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntervalKind {
    Calendar,
    Fixed,
}

impl IntervalKind {
    pub fn key(self) -> &'static str {
        match self {
            IntervalKind::Calendar => "calendar_interval",
            IntervalKind::Fixed => "fixed_interval",
        }
    }
}

#[derive(Clone, Debug)]
pub struct DateHisto {
    pub field: String,
    pub interval: String,
    pub kind: IntervalKind,
    pub delay: Option<String>,
    pub time_zone: String,
}

#[derive(Clone, Debug)]
pub struct Histo {
    pub interval: i64,
    pub fields: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Groups {
    pub date_histogram: DateHisto,
    pub histogram: Option<Histo>,
    pub terms: Option<Vec<String>>,
}

#[derive(Clone, Debug)]
pub struct Metric {
    pub field: String,
    pub metrics: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct JobConfig {
    pub id: String,
    pub index_pattern: String,
    pub rollup_index: String,
    pub cron: String,
    pub page_size: i64,
    pub groups: Option<Groups>,
    pub metrics: Vec<Metric>,
    pub timeout: String,
}

/// Where a parse error points: the location of a top-level key of the request body.
pub type Loc<'a> = &'a dyn Fn(&str) -> (usize, usize);

fn no_loc(_: &str) -> (usize, usize) {
    (1, 1)
}

/// `[parser] failed to parse field [key]`, caused by `cause`.
fn field_failure(loc: Loc, parser: &str, key: &str, cause: EsError) -> EsError {
    let (l, c) = loc(key);
    EsError::x_content_parse(l, c, format!("[{parser}] failed to parse field [{key}]")).caused_by(cause)
}

/// A constructor's `IllegalArgumentException`, as `ConstructingObjectParser` reports it.
fn build_failure(parser: &str, reason: impl Into<String>) -> EsError {
    EsError::x_content_parse_bare(format!("Failed to build [{parser}] after last required field arrived")).caused_by(EsError::illegal_argument(reason))
}

fn unknown(loc: Loc, parser: &str, key: &str) -> EsError {
    let (l, c) = loc(key);
    EsError::x_content_parse(l, c, format!("[{parser}] unknown field [{key}]"))
}

fn string(loc: Loc, parser: &str, key: &str, v: &Value) -> EsResult<String> {
    match v {
        Value::Str(s) => Ok(s.clone()),
        Value::Num(_) | Value::Bool(_) => Ok(v.to_es_string()),
        _ => {
            let (l, c) = loc(key);
            Err(EsError::x_content_parse(l, c, format!("[{parser}] {key} doesn't support values of type: {}", token_name(v))))
        }
    }
}

fn token_name(v: &Value) -> &'static str {
    match v {
        Value::Obj(_) => "START_OBJECT",
        Value::Arr(_) => "START_ARRAY",
        Value::Null => "VALUE_NULL",
        Value::Bool(_) => "VALUE_BOOLEAN",
        Value::Num(_) => "VALUE_NUMBER",
        Value::Str(_) => "VALUE_STRING",
    }
}

fn strings(loc: Loc, parser: &str, key: &str, v: &Value) -> EsResult<Vec<String>> {
    match v {
        Value::Arr(a) => a.iter().map(|x| string(loc, parser, key, x)).collect(),
        other => Ok(vec![string(loc, parser, key, other)?]),
    }
}

fn object<'v>(loc: Loc, parser: &str, key: &str, v: &'v Value) -> EsResult<&'v Map> {
    v.as_object().ok_or_else(|| {
        let (l, c) = loc(key);
        EsError::x_content_parse(l, c, format!("[{parser}] {key} doesn't support values of type: {}", token_name(v)))
    })
}

fn required(parser: &str, missing: &[&str]) -> EsResult<()> {
    if missing.is_empty() {
        return Ok(());
    }
    Err(build_failure(parser, format!("Required [{}]", missing.join(", "))))
}

/// `DateHistogramAggregationBuilder.DATE_FIELD_UNITS`: the base unit's duration (`ChronoUnit`)
/// in millis, which `estimateMillis` and the calendar interval checks compare.
pub fn calendar_unit_millis(s: &str) -> Option<i64> {
    use search_aggs::rounding::DateUnit;
    Some(match DateUnit::parse_calendar(s)? {
        DateUnit::Second => 1_000,
        DateUnit::Minute => 60_000,
        DateUnit::Hour => 3_600_000,
        DateUnit::Day => 86_400_000,
        DateUnit::Week => 604_800_000,
        DateUnit::Month => 2_629_746_000,
        DateUnit::Quarter => 7_889_238_000,
        DateUnit::Year => 31_556_952_000,
    })
}

/// `ZoneId.of(timeZone, ZoneId.SHORT_IDS)`.
pub fn zone(tz: &str) -> EsResult<search_aggs::format::TimeZone> {
    search_aggs::format::TimeZone::parse(tz).map_err(|_| EsError::java(400, "date_time_exception", format!("Unknown time-zone ID: {tz}")))
}

/// Whether two zones have the same rules (`ZoneId.getRules().equals`): an alias and its zone
/// (`Canada/Mountain`, `America/Edmonton`) do.
pub fn same_rules(a: &str, b: &str) -> bool {
    use search_aggs::format::TimeZone;
    match (zone(a), zone(b)) {
        (Ok(TimeZone::Named(x)), Ok(TimeZone::Named(y))) => x == y || search_aggs::rounding::ZoneRules::of(x).same_rules(&search_aggs::rounding::ZoneRules::of(y)),
        (Ok(TimeZone::Fixed { offset_secs: x, .. }), Ok(TimeZone::Fixed { offset_secs: y, .. })) => x == y,
        _ => false,
    }
}

impl DateHisto {
    fn parse(loc: Loc, m: &Map) -> EsResult<DateHisto> {
        const P: &str = "date_histogram";
        let (mut field, mut interval, mut calendar, mut fixed, mut delay, mut tz) = (None, None, None, None, None, None);
        for (k, v) in m {
            let s = || string(loc, P, k, v);
            match k.as_str() {
                "field" => field = Some(s()?),
                "interval" => interval = Some(s()?),
                "calendar_interval" => calendar = Some(s()?),
                "fixed_interval" => fixed = Some(s()?),
                "delay" => delay = Some(s()?),
                "time_zone" => tz = Some(s()?),
                _ => return Err(unknown(loc, P, k)),
            }
        }
        required(P, &if field.is_none() { vec!["field"] } else { vec![] })?;
        let field = field.unwrap_or_default();
        let (interval, kind) = match (interval, calendar, fixed) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
                return Err(build_failure(P, "Cannot use [interval] with [fixed_interval] or [calendar_interval] configuration options."));
            }
            (Some(i), None, None) => {
                let kind = if calendar_unit_millis(&i).is_some() { IntervalKind::Calendar } else { IntervalKind::Fixed };
                (i, kind)
            }
            (None, Some(c), None) => (c, IntervalKind::Calendar),
            (None, None, Some(f)) => (f, IntervalKind::Fixed),
            (None, Some(_), Some(_)) => return Err(build_failure(P, "Cannot set both [fixed_interval] and [calendar_interval] at the same time")),
            (None, None, None) => return Err(build_failure(P, "An interval is required.  Use [fixed_interval] or [calendar_interval].")),
        };
        let wrap = |e: EsError| EsError::x_content_parse_bare(format!("Failed to build [{P}] after last required field arrived")).caused_by(e);
        if field.is_empty() {
            return Err(build_failure(P, "Field must be a non-null, non-empty string"));
        }
        let time_zone = tz.filter(|t| !t.is_empty()).unwrap_or_else(|| "UTC".to_string());
        // `createRounding`: a calendar unit or a time value, in a valid zone.
        if calendar_unit_millis(&interval).is_none() {
            crate::units::TimeValue::parse(&interval, "createRounding").map_err(wrap)?;
        }
        zone(&time_zone).map_err(wrap)?;
        if let Some(d) = &delay {
            crate::units::TimeValue::parse(d, "delay").map_err(wrap)?;
        }
        match kind {
            IntervalKind::Fixed => {
                crate::units::TimeValue::parse(&interval, "date_histogram.FixedInterval").map_err(wrap)?;
            }
            IntervalKind::Calendar => {
                if calendar_unit_millis(&interval).is_none() {
                    return Err(build_failure(P, format!("The supplied interval [{interval}] could not be parsed as a calendar interval.")));
                }
            }
        }
        Ok(DateHisto { field, interval, kind, delay, time_zone })
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert(self.kind.key().into(), self.interval.clone().into());
        m.insert("field".into(), self.field.clone().into());
        if let Some(d) = &self.delay {
            m.insert("delay".into(), d.clone().into());
        }
        m.insert("time_zone".into(), self.time_zone.clone().into());
        Value::Obj(m)
    }

    /// The job's rounding (`createRounding`), in its zone.
    pub fn rounding(&self) -> Option<search_aggs::rounding::Rounding> {
        use search_aggs::rounding::{DateUnit, Rounding, RoundingKind};
        let kind = match DateUnit::parse_calendar(&self.interval) {
            Some(u) => RoundingKind::Unit(u),
            None => RoundingKind::Interval(crate::units::TimeValue::parse(&self.interval, "interval").ok()?.as_millis()),
        };
        Rounding::new(kind, zone(&self.time_zone).ok()?, 0).ok()
    }
}

impl Groups {
    fn parse(loc: Loc, m: &Map) -> EsResult<Groups> {
        const P: &str = "groups";
        let (mut date, mut histo, mut terms) = (None, None, None);
        for (k, v) in m {
            let wrap = |e: EsError| field_failure(&no_loc, P, k, e);
            match k.as_str() {
                "date_histogram" => date = Some(DateHisto::parse(loc, object(loc, P, k, v)?).map_err(wrap)?),
                "histogram" => histo = Some(parse_histo(loc, object(loc, P, k, v)?).map_err(wrap)?),
                "terms" => terms = Some(parse_terms(loc, object(loc, P, k, v)?).map_err(wrap)?),
                _ => return Err(unknown(loc, P, k)),
            }
        }
        let Some(date_histogram) = date else { return Err(build_failure(P, "Required [date_histogram]")) };
        Ok(Groups { date_histogram, histogram: histo, terms })
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("date_histogram".into(), self.date_histogram.to_value());
        if let Some(h) = &self.histogram {
            m.insert("histogram".into(), obj! { "interval" => h.interval, "fields" => strs(&h.fields) });
        }
        if let Some(t) = &self.terms {
            m.insert("terms".into(), obj! { "fields" => strs(t) });
        }
        Value::Obj(m)
    }

    /// Every field the groups read.
    fn fields(&self) -> Vec<String> {
        let mut out = vec![self.date_histogram.field.clone()];
        out.extend(self.histogram.iter().flat_map(|h| h.fields.iter().cloned()));
        out.extend(self.terms.iter().flatten().cloned());
        out
    }
}

fn strs(v: &[String]) -> Value {
    Value::Arr(v.iter().map(|s| Value::from(s.clone())).collect())
}

fn parse_histo(loc: Loc, m: &Map) -> EsResult<Histo> {
    const P: &str = "histogram";
    let (mut interval, mut fields) = (None, None);
    for (k, v) in m {
        match k.as_str() {
            "interval" => {
                interval = Some(match v {
                    Value::Num(n) if n.as_i64().is_some() => n.as_i64().unwrap_or_default(),
                    other => crate::units::parse_java_long(&other.to_es_string()).ok_or_else(|| field_failure(loc, P, k, EsError::number_format(&other.to_es_string())))?,
                })
            }
            "fields" => fields = Some(strings(loc, P, k, v)?),
            _ => return Err(unknown(loc, P, k)),
        }
    }
    let mut missing = Vec::new();
    if interval.is_none() {
        missing.push("interval");
    }
    if fields.is_none() {
        missing.push("fields");
    }
    required(P, &missing)?;
    let (interval, fields) = (interval.unwrap_or_default(), fields.unwrap_or_default());
    if interval <= 0 {
        return Err(build_failure(P, "Interval must be a positive long"));
    }
    if fields.is_empty() {
        return Err(build_failure(P, "Fields must have at least one value"));
    }
    Ok(Histo { interval, fields })
}

fn parse_terms(loc: Loc, m: &Map) -> EsResult<Vec<String>> {
    const P: &str = "terms";
    let mut fields = None;
    for (k, v) in m {
        match k.as_str() {
            "fields" => fields = Some(strings(loc, P, k, v)?),
            _ => return Err(unknown(loc, P, k)),
        }
    }
    let Some(fields) = fields else { return Err(build_failure(P, "Required [fields]")) };
    if fields.is_empty() {
        return Err(build_failure(P, "Fields must have at least one value"));
    }
    Ok(fields)
}

fn parse_metric(loc: Loc, m: &Map) -> EsResult<Metric> {
    const P: &str = "metrics";
    let (mut field, mut metrics) = (None, None);
    for (k, v) in m {
        match k.as_str() {
            "field" => field = Some(string(loc, P, k, v)?),
            "metrics" => metrics = Some(strings(loc, P, k, v)?),
            _ => return Err(unknown(loc, P, k)),
        }
    }
    let mut missing = Vec::new();
    if field.is_none() {
        missing.push("field");
    }
    if metrics.is_none() {
        missing.push("metrics");
    }
    required(P, &missing)?;
    let (field, metrics) = (field.unwrap_or_default(), metrics.unwrap_or_default());
    if field.is_empty() {
        return Err(build_failure(P, "Field must be a non-null, non-empty string"));
    }
    if metrics.is_empty() {
        return Err(build_failure(P, "Metrics must be a non-null, non-empty array of strings"));
    }
    if let Some(bad) = metrics.iter().find(|m| !SUPPORTED_METRICS.contains(&m.as_str())) {
        return Err(build_failure(P, format!("Unsupported metric [{bad}]. Supported metrics include: [{}]", SUPPORTED_METRICS.join(", "))));
    }
    Ok(Metric { field, metrics })
}

/// Simple wildcard matching (`Regex.simpleMatch`).
pub fn simple_match(pattern: &str, s: &str) -> bool {
    crate::xcontent::filter::glob_match(pattern, s)
}

impl JobConfig {
    /// `RollupJobConfig.fromXContent(parser, id)`: a request body (`id` from the path unless
    /// the body names one) or a config stored in a rollup index's `_meta`.
    pub fn parse(m: &Map, path_id: Option<&str>, loc: Loc) -> EsResult<JobConfig> {
        const P: &str = "xpack/rollup/jobconfig";
        let mut id = None;
        let (mut pattern, mut rollup_index, mut cron, mut page_size, mut groups, mut metrics, mut timeout) = (None, None, None, None, None, None, None);
        for (k, v) in m {
            match k.as_str() {
                "id" => id = Some(string(loc, P, k, v)?),
                "index_pattern" => pattern = Some(string(loc, P, k, v)?),
                "rollup_index" => rollup_index = Some(string(loc, P, k, v)?),
                "cron" => cron = Some(string(loc, P, k, v)?),
                "page_size" => {
                    page_size = Some(match v {
                        Value::Num(n) => n.as_i64().filter(|i| i32::try_from(*i).is_ok()).ok_or_else(|| field_failure(loc, P, k, EsError::number_format(&v.to_es_string())))?,
                        other => crate::units::parse_java_int(&other.to_es_string()).map(i64::from).ok_or_else(|| field_failure(loc, P, k, EsError::number_format(&other.to_es_string())))?,
                    })
                }
                "groups" => groups = Some(Groups::parse(loc, object(loc, P, k, v)?).map_err(|e| field_failure(loc, P, k, e))?),
                "metrics" => {
                    let list = match v {
                        Value::Arr(a) => a.iter().map(|x| object(loc, P, k, x).and_then(|o| parse_metric(loc, o))).collect::<EsResult<Vec<_>>>(),
                        Value::Obj(o) => parse_metric(loc, o).map(|x| vec![x]),
                        other => Err(EsError::x_content_parse_bare(format!("[{P}] metrics doesn't support values of type: {}", token_name(other)))),
                    };
                    metrics = Some(list.map_err(|e| field_failure(loc, P, k, e))?);
                }
                "timeout" => {
                    timeout = match v {
                        Value::Null => None,
                        other => {
                            let s = string(loc, P, k, other)?;
                            crate::units::TimeValue::parse(&s, "timeout").map_err(|e| field_failure(loc, P, k, e))?;
                            Some(s)
                        }
                    }
                }
                _ => return Err(unknown(loc, P, k)),
            }
        }
        let mut missing = Vec::new();
        for (name, absent) in [("index_pattern", pattern.is_none()), ("rollup_index", rollup_index.is_none()), ("cron", cron.is_none()), ("page_size", page_size.is_none())] {
            if absent {
                missing.push(name);
            }
        }
        if !missing.is_empty() {
            return Err(EsError::illegal_argument(format!("Required [{}]", missing.join(", "))));
        }
        let id = id.or_else(|| path_id.map(str::to_string)).unwrap_or_default();
        let (pattern, rollup_index, cron, page_size) = (pattern.unwrap_or_default(), rollup_index.unwrap_or_default(), cron.unwrap_or_default(), page_size.unwrap_or_default());
        let fail = |r: &str| build_failure(P, r.to_string());
        if id.is_empty() {
            return Err(fail("Id must be a non-null, non-empty string"));
        }
        if pattern.is_empty() {
            return Err(fail("Index pattern must be a non-null, non-empty string"));
        }
        for index in pattern.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if index == "*" || index == "_all" {
                return Err(fail("Index pattern must not match all indices (as it would match it's own rollup index"));
            }
            if index.contains('*') && simple_match(index, &rollup_index) {
                return Err(fail("Index pattern would match rollup index name which is not allowed"));
            }
            if index == rollup_index {
                return Err(fail("Rollup index may not be the same as the index pattern"));
            }
        }
        if rollup_index.is_empty() {
            return Err(fail("Rollup index must be a non-null, non-empty string"));
        }
        if cron.is_empty() {
            return Err(fail("Cron schedule must be a non-null, non-empty string"));
        }
        if page_size <= 0 {
            return Err(fail("Page size is mandatory and  must be a positive long"));
        }
        let metrics = metrics.unwrap_or_default();
        if groups.is_none() && metrics.is_empty() {
            return Err(fail("At least one grouping or metric must be configured"));
        }
        Ok(JobConfig { id, index_pattern: pattern, rollup_index, cron, page_size, groups, metrics, timeout: timeout.unwrap_or_else(|| "20s".to_string()) })
    }

    /// A config stored in a rollup index's `_meta._rollup` (`None` when it doesn't parse).
    pub fn from_meta(v: &Value) -> Option<JobConfig> {
        JobConfig::parse(v.as_object()?, None, &no_loc).ok()
    }

    pub fn to_value(&self) -> Value {
        let mut m = Map::new();
        m.insert("id".into(), self.id.clone().into());
        m.insert("index_pattern".into(), self.index_pattern.clone().into());
        m.insert("rollup_index".into(), self.rollup_index.clone().into());
        m.insert("cron".into(), self.cron.clone().into());
        if let Some(g) = &self.groups {
            m.insert("groups".into(), g.to_value());
        }
        m.insert("metrics".into(), Value::Arr(self.metrics.iter().map(|x| obj! { "field" => x.field.clone(), "metrics" => strs(&x.metrics) }).collect()));
        m.insert("timeout".into(), self.timeout.clone().into());
        m.insert("page_size".into(), self.page_size.into());
        Value::Obj(m)
    }

    /// The source indices (`index_pattern` split at commas).
    pub fn indices(&self) -> Vec<String> {
        self.index_pattern.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect()
    }

    /// `getAllFields()`.
    pub fn all_fields(&self) -> Vec<String> {
        let mut out: Vec<String> = self.groups.iter().flat_map(Groups::fields).collect();
        out.extend(self.metrics.iter().map(|m| m.field.clone()));
        out.sort();
        out.dedup();
        out
    }

    /// `validateMappings` against a `_field_caps` response's `fields`: every problem, in
    /// Elasticsearch's order (groups, then metrics).
    pub fn validate_mappings(&self, caps: &Map) -> Vec<String> {
        let mut errors = Vec::new();
        if caps.is_empty() {
            errors.push("Could not find any fields in the index/index-pattern that were configured in job".to_string());
            return errors;
        }
        let types = |f: &str| caps.get(f).and_then(Value::as_object).filter(|m| !m.is_empty());
        let aggregatable = |c: &Value| c.get("aggregatable").and_then(Value::as_bool).unwrap_or(false);
        let found = |m: &Map| format!("[{}]", m.keys().cloned().collect::<Vec<_>>().join(", "));
        if let Some(g) = &self.groups {
            let d = &g.date_histogram;
            match types(&d.field) {
                Some(m) => {
                    let date = DATE_TYPES.iter().find(|t| m.contains_key(**t) && m.len() == 1);
                    match date {
                        Some(t) if aggregatable(&m[*t]) => {}
                        Some(_) => errors.push(format!("The field [{}] must be aggregatable across all indices, but is not.", d.field)),
                        None => errors.push(format!(
                            "The field referenced by a date_histo group must be one of type [{}] across all indices in the index pattern.  Found: {} for field [{}]",
                            DATE_TYPES.join(","),
                            found(m),
                            d.field
                        )),
                    }
                }
                None => errors.push(format!("Could not find one of [{}] fields with name [{}] in any of the indices matching the index pattern.", DATE_TYPES.join(","), d.field)),
            }
            for f in g.histogram.iter().flat_map(|h| h.fields.iter()) {
                match types(f) {
                    Some(m) => {
                        for (ty, c) in m {
                            if NUMERIC_TYPES.contains(&ty.as_str()) {
                                if !aggregatable(c) {
                                    errors.push(format!("The field [{f}] must be aggregatable across all indices, but is not."));
                                }
                            } else {
                                errors.push(format!("The field referenced by a histo group must be a [numeric] type, but found {} for field [{f}]", found(m)));
                            }
                        }
                    }
                    None => errors.push(format!("Could not find a [numeric] field with name [{f}] in any of the indices matching the index pattern.")),
                }
            }
            for f in g.terms.iter().flatten() {
                match types(f) {
                    Some(m) => {
                        for (ty, c) in m {
                            let ok_type = matches!(ty.as_str(), "keyword" | "text" | "half_float" | "float" | "double" | "scaled_float" | "byte" | "short" | "integer" | "long");
                            if ok_type {
                                if !aggregatable(c) {
                                    errors.push(format!("The field [{f}] must be aggregatable across all indices, but is not."));
                                }
                            } else {
                                errors.push(format!("The field referenced by a terms group must be a [numeric] or [keyword/text] type, but found {} for field [{f}]", found(m)));
                            }
                        }
                    }
                    None => errors.push(format!("Could not find a [numeric] or [keyword/text] field with name [{f}] in any of the indices matching the index pattern.")),
                }
            }
        }
        for metric in &self.metrics {
            let f = &metric.field;
            match types(f) {
                Some(m) => {
                    for (ty, c) in m {
                        if !aggregatable(c) {
                            errors.push(format!("The field [{f}] must be aggregatable across all indices, but is not."));
                        }
                        if NUMERIC_TYPES.contains(&ty.as_str()) {
                        } else if DATE_TYPES.contains(&ty.as_str()) {
                            let unsupported: Vec<&String> = metric.metrics.iter().filter(|x| !SUPPORTED_DATE_METRICS.contains(&x.as_str())).collect();
                            if !unsupported.is_empty() {
                                errors.push(format!(
                                    "Only the metrics [{}] are supported for [{ty}] types, but unsupported metrics [{}] supplied for field [{f}]",
                                    SUPPORTED_DATE_METRICS.join(", "),
                                    unsupported.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
                                ));
                            }
                        } else {
                            errors.push(format!(
                                "The field referenced by a metric group must be a [numeric] or [{}] type, but found {} for field [{f}]",
                                DATE_TYPES.join(","),
                                found(m)
                            ));
                        }
                    }
                }
                None => errors.push(format!("Could not find a [numeric] or [{}] field with name [{f}] in any of the indices matching the index pattern.", DATE_TYPES.join(","))),
            }
        }
        errors
    }

    /// `RollupJobCaps(config)`.
    pub fn caps(&self) -> JobCaps {
        let mut fields: indexmap::IndexMap<String, Vec<Map>> = indexmap::IndexMap::new();
        if let Some(g) = &self.groups {
            let d = &g.date_histogram;
            let mut cap = Map::new();
            cap.insert("agg".into(), "date_histogram".into());
            cap.insert(d.kind.key().into(), d.interval.clone().into());
            if let Some(delay) = &d.delay {
                cap.insert("delay".into(), delay.clone().into());
            }
            cap.insert("time_zone".into(), d.time_zone.clone().into());
            fields.entry(d.field.clone()).or_default().push(cap);
            if let Some(h) = &g.histogram {
                for f in &h.fields {
                    let mut cap = Map::new();
                    cap.insert("agg".into(), "histogram".into());
                    cap.insert("interval".into(), h.interval.into());
                    fields.entry(f.clone()).or_default().push(cap);
                }
            }
            for f in g.terms.iter().flatten() {
                let mut cap = Map::new();
                cap.insert("agg".into(), "terms".into());
                fields.entry(f.clone()).or_default().push(cap);
            }
        }
        for m in &self.metrics {
            for metric in &m.metrics {
                let mut cap = Map::new();
                cap.insert("agg".into(), metric.clone().into());
                fields.entry(m.field.clone()).or_default().push(cap);
            }
        }
        JobCaps { job_id: self.id.clone(), rollup_index: self.rollup_index.clone(), index_pattern: self.index_pattern.clone(), fields }
    }
}

/// `RollupJobCaps`: what one job's rollup documents can answer, per source field.
#[derive(Clone, Debug)]
pub struct JobCaps {
    pub job_id: String,
    pub rollup_index: String,
    pub index_pattern: String,
    pub fields: indexmap::IndexMap<String, Vec<Map>>,
}

impl JobCaps {
    pub fn to_value(&self) -> Value {
        let fields: Map = self.fields.iter().map(|(k, v)| (k.clone(), Value::Arr(v.iter().cloned().map(Value::Obj).collect()))).collect();
        obj! { "job_id" => self.job_id.clone(), "rollup_index" => self.rollup_index.clone(), "index_pattern" => self.index_pattern.clone(), "fields" => Value::Obj(fields) }
    }

    /// The aggregation capabilities of `field` (empty when the job doesn't have it).
    pub fn aggs(&self, field: &str) -> &[Map] {
        self.fields.get(field).map_or(&[], Vec::as_slice)
    }
}

/// The jobs of an index's mapping `_meta._rollup` (`findRollupIndexCaps`): empty for an index
/// that isn't a rollup index. Configs that don't parse are skipped.
pub fn index_jobs(idx: &crate::meta::IndexMeta) -> Vec<JobConfig> {
    let Some(meta) = &idx.mapping.meta else { return Vec::new() };
    let Some(Value::Obj(jobs)) = meta.get("_rollup") else { return Vec::new() };
    jobs.values().filter_map(JobConfig::from_meta).collect()
}

/// `DateUtils.DEPRECATED_LONG_TIMEZONES`: deprecated tz database names and their replacements.
pub fn deprecated_zone(tz: &str) -> Option<&'static str> {
    const ZONES: &[(&str, &str)] = &[
        ("Africa/Asmera", "Africa/Nairobi"),
        ("Africa/Timbuktu", "Africa/Abidjan"),
        ("America/Argentina/ComodRivadavia", "America/Argentina/Catamarca"),
        ("America/Atka", "America/Adak"),
        ("America/Buenos_Aires", "America/Argentina/Buenos_Aires"),
        ("America/Catamarca", "America/Argentina/Catamarca"),
        ("America/Coral_Harbour", "America/Atikokan"),
        ("America/Cordoba", "America/Argentina/Cordoba"),
        ("America/Ensenada", "America/Tijuana"),
        ("America/Fort_Wayne", "America/Indiana/Indianapolis"),
        ("America/Indianapolis", "America/Indiana/Indianapolis"),
        ("America/Jujuy", "America/Argentina/Jujuy"),
        ("America/Knox_IN", "America/Indiana/Knox"),
        ("America/Louisville", "America/Kentucky/Louisville"),
        ("America/Mendoza", "America/Argentina/Mendoza"),
        ("America/Montreal", "America/Toronto"),
        ("America/Porto_Acre", "America/Rio_Branco"),
        ("America/Rosario", "America/Argentina/Cordoba"),
        ("America/Santa_Isabel", "America/Tijuana"),
        ("America/Shiprock", "America/Denver"),
        ("America/Virgin", "America/Port_of_Spain"),
        ("Antarctica/South_Pole", "Pacific/Auckland"),
        ("Asia/Ashkhabad", "Asia/Ashgabat"),
        ("Asia/Calcutta", "Asia/Kolkata"),
        ("Asia/Chongqing", "Asia/Shanghai"),
        ("Asia/Chungking", "Asia/Shanghai"),
        ("Asia/Dacca", "Asia/Dhaka"),
        ("Asia/Harbin", "Asia/Shanghai"),
        ("Asia/Istanbul", "Europe/Istanbul"),
        ("Asia/Kashgar", "Asia/Urumqi"),
        ("Asia/Katmandu", "Asia/Kathmandu"),
        ("Asia/Macao", "Asia/Macau"),
        ("Asia/Rangoon", "Asia/Yangon"),
        ("Asia/Saigon", "Asia/Ho_Chi_Minh"),
        ("Asia/Tel_Aviv", "Asia/Jerusalem"),
        ("Asia/Thimbu", "Asia/Thimphu"),
        ("Asia/Ujung_Pandang", "Asia/Makassar"),
        ("Asia/Ulan_Bator", "Asia/Ulaanbaatar"),
        ("Atlantic/Faeroe", "Atlantic/Faroe"),
        ("Atlantic/Jan_Mayen", "Europe/Oslo"),
        ("Australia/ACT", "Australia/Sydney"),
        ("Australia/Canberra", "Australia/Sydney"),
        ("Australia/LHI", "Australia/Lord_Howe"),
        ("Australia/NSW", "Australia/Sydney"),
        ("Australia/North", "Australia/Darwin"),
        ("Australia/Queensland", "Australia/Brisbane"),
        ("Australia/South", "Australia/Adelaide"),
        ("Australia/Tasmania", "Australia/Hobart"),
        ("Australia/Victoria", "Australia/Melbourne"),
        ("Australia/West", "Australia/Perth"),
        ("Australia/Yancowinna", "Australia/Broken_Hill"),
        ("Brazil/Acre", "America/Rio_Branco"),
        ("Brazil/DeNoronha", "America/Noronha"),
        ("Brazil/East", "America/Sao_Paulo"),
        ("Brazil/West", "America/Manaus"),
        ("Canada/Atlantic", "America/Halifax"),
        ("Canada/Central", "America/Winnipeg"),
        ("Canada/East-Saskatchewan", "America/Regina"),
        ("Canada/Eastern", "America/Toronto"),
        ("Canada/Mountain", "America/Edmonton"),
        ("Canada/Newfoundland", "America/St_Johns"),
        ("Canada/Pacific", "America/Vancouver"),
        ("Canada/Yukon", "America/Whitehorse"),
        ("Chile/Continental", "America/Santiago"),
        ("Chile/EasterIsland", "Pacific/Easter"),
        ("Cuba", "America/Havana"),
        ("Egypt", "Africa/Cairo"),
        ("Eire", "Europe/Dublin"),
        ("Europe/Belfast", "Europe/London"),
        ("Europe/Tiraspol", "Europe/Chisinau"),
        ("GB", "Europe/London"),
        ("GB-Eire", "Europe/London"),
        ("Greenwich", "Etc/GMT"),
        ("Hongkong", "Asia/Hong_Kong"),
        ("Iceland", "Atlantic/Reykjavik"),
        ("Iran", "Asia/Tehran"),
        ("Israel", "Asia/Jerusalem"),
        ("Jamaica", "America/Jamaica"),
        ("Japan", "Asia/Tokyo"),
        ("Kwajalein", "Pacific/Kwajalein"),
        ("Libya", "Africa/Tripoli"),
        ("Mexico/BajaNorte", "America/Tijuana"),
        ("Mexico/BajaSur", "America/Mazatlan"),
        ("Mexico/General", "America/Mexico_City"),
        ("NZ", "Pacific/Auckland"),
        ("NZ-CHAT", "Pacific/Chatham"),
        ("Navajo", "America/Denver"),
        ("PRC", "Asia/Shanghai"),
        ("Pacific/Johnston", "Pacific/Honolulu"),
        ("Pacific/Ponape", "Pacific/Pohnpei"),
        ("Pacific/Samoa", "Pacific/Pago_Pago"),
        ("Pacific/Truk", "Pacific/Chuuk"),
        ("Pacific/Yap", "Pacific/Chuuk"),
        ("Poland", "Europe/Warsaw"),
        ("Portugal", "Europe/Lisbon"),
        ("ROC", "Asia/Taipei"),
        ("ROK", "Asia/Seoul"),
        ("Singapore", "Asia/Singapore"),
        ("Turkey", "Europe/Istanbul"),
        ("UCT", "Etc/UCT"),
        ("US/Alaska", "America/Anchorage"),
        ("US/Aleutian", "America/Adak"),
        ("US/Arizona", "America/Phoenix"),
        ("US/Central", "America/Chicago"),
        ("US/East-Indiana", "America/Indiana/Indianapolis"),
        ("US/Eastern", "America/New_York"),
        ("US/Hawaii", "Pacific/Honolulu"),
        ("US/Indiana-Starke", "America/Indiana/Knox"),
        ("US/Michigan", "America/Detroit"),
        ("US/Mountain", "America/Denver"),
        ("US/Pacific", "America/Los_Angeles"),
        ("US/Samoa", "Pacific/Pago_Pago"),
        ("Universal", "Etc/UTC"),
        ("W-SU", "Europe/Moscow"),
        ("Zulu", "Etc/UTC"),
    ];
    ZONES.iter().find(|(old, _)| *old == tz).map(|(_, new)| *new)
}
