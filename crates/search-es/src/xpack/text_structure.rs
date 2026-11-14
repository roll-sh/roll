//! Text structure: `_text_structure/find_structure` (a text sample), `find_message_structure`
//! (a list of messages), `find_field_structure` (a field of an index) and
//! `test_grok_pattern`.
//!
//! The finder follows Elasticsearch's `TextStructureFinderManager`: it tries NDJSON, then
//! delimited formats (comma, tab, semicolon with at least 4 fields, vertical line with at least
//! 5) and falls back to semi-structured text keyed on a leading timestamp. Field types are
//! guessed like `TextStructureUtils.guessScalarMapping` (boolean, date, long, double, ip, then
//! keyword unless a value is long or has many spaces, which makes it text), the timestamp
//! field is the first field whose every value has one timestamp format, and field statistics
//! are `FieldStatsCalculator`'s (count, cardinality, numeric min/max/mean/median, date
//! earliest/latest, top hits by count then value). The header heuristic of delimited text is
//! Elasticsearch's length and Levenshtein test. Timestamp formats cover Elasticsearch's common
//! candidates (ISO 8601 and its space-separated variants, `UNIX`, `UNIX_MS`, HTTP dates,
//! syslog, `EEE MMM dd HH:mm:ss yyyy`, date-only ISO); XML samples and rarer timestamp
//! formats are not recognised and fall through to the next format.

use crate::error::{EsError, EsResult};
use crate::json::{Map, Value};
use crate::obj;
use crate::rest::{Ctx, Reply};
use std::collections::BTreeMap;

const KEYWORD_MAX_LEN: usize = 256;
const KEYWORD_MAX_SPACES: usize = 5;
const NUM_TOP_HITS: usize = 10;
const LONG_FIELD_THRESHOLD: usize = 100;
const MAX_LEVENSHTEIN_COMPARISONS: usize = 100;

/// Request overrides.
#[derive(Default, Clone)]
struct Overrides {
    column_names: Option<Vec<String>>,
    delimiter: Option<char>,
    quote: Option<char>,
    has_header_row: Option<bool>,
    format: Option<String>,
    timestamp_field: Option<String>,
    should_trim_fields: Option<bool>,
    ecs_compatibility: Option<String>,
    explain: bool,
}

fn overrides(ctx: &Ctx) -> EsResult<Overrides> {
    let mut o = Overrides {
        column_names: ctx.query("column_names").map(|s| s.split(',').map(str::to_string).collect()),
        delimiter: None,
        quote: None,
        has_header_row: ctx.opt_bool_param("has_header_row")?,
        format: ctx.query("format").map(str::to_lowercase),
        timestamp_field: ctx.query("timestamp_field").map(str::to_string),
        should_trim_fields: ctx.opt_bool_param("should_trim_fields")?,
        ecs_compatibility: ctx.query("ecs_compatibility").map(str::to_string),
        explain: ctx.bool_param("explain", false)?,
    };
    if let Some(f) = &o.format
        && !matches!(f.as_str(), "ndjson" | "xml" | "delimited" | "semi_structured_text")
    {
        return Err(EsError::illegal_argument(format!("No enum constant org.elasticsearch.xpack.core.textstructure.structurefinder.TextStructure.Format.{}", f.to_uppercase())));
    }
    if let Some(d) = ctx.query("delimiter") {
        let mut cs = d.chars();
        match (cs.next(), cs.next()) {
            (Some(c), None) => o.delimiter = Some(c),
            _ => return Err(EsError::illegal_argument(format!("[delimiter] must be a single character, got [{d}]"))),
        }
    }
    if let Some(q) = ctx.query("quote") {
        let mut cs = q.chars();
        match (cs.next(), cs.next()) {
            (Some(c), None) => o.quote = Some(c),
            _ => return Err(EsError::illegal_argument(format!("[quote] must be a single character, got [{q}]"))),
        }
    }
    if let Some(e) = &o.ecs_compatibility
        && !matches!(e.as_str(), "disabled" | "v1")
    {
        return Err(EsError::illegal_argument(format!("[ecs_compatibility] must be one of [disabled, v1] if specified, got [{e}]")));
    }
    let delimited_only = o.column_names.is_some() || o.delimiter.is_some() || o.quote.is_some() || o.has_header_row.is_some() || o.should_trim_fields.is_some();
    if delimited_only && o.format.as_deref().is_some_and(|f| f != "delimited") {
        let which = if o.column_names.is_some() {
            "column_names"
        } else if o.has_header_row.is_some() {
            "has_header_row"
        } else if o.delimiter.is_some() {
            "delimiter"
        } else if o.quote.is_some() {
            "quote"
        } else {
            "should_trim_fields"
        };
        return Err(EsError::illegal_argument(format!("[{which}] may only be specified if [format] is [delimited]")));
    }
    if let Some(n) = ctx.query("lines_to_sample") {
        let v = crate::units::parse_java_int(n).ok_or_else(|| EsError::illegal_argument(format!("Failed to parse int parameter [lines_to_sample] with value [{n}]")))?;
        if v < 2 {
            return Err(EsError::illegal_argument(format!("[lines_to_sample] must be at least [2] if specified, got [{v}]")));
        }
    }
    if let Some(n) = ctx.query("line_merge_size_limit") {
        let v = crate::units::parse_java_int(n).ok_or_else(|| EsError::illegal_argument(format!("Failed to parse int parameter [line_merge_size_limit] with value [{n}]")))?;
        if v <= 0 {
            return Err(EsError::illegal_argument(format!("[line_merge_size_limit] must be positive if specified, got [{v}]")));
        }
    }
    if ctx.has_query("timeout") {
        ctx.time_param("timeout", crate::units::TimeValue::seconds(25))?;
    }
    Ok(o)
}

// --- timestamps ---------------------------------------------------------------------------------

/// A recognised timestamp format.
#[derive(Clone, Debug, PartialEq)]
struct TsFormat {
    java: String,
    joda: String,
    /// The mapping's `format` (`None`: the default date format parses it).
    mapping_format: Option<String>,
    need_tz: bool,
    /// The grok pattern of semi-structured text.
    grok: &'static str,
    /// A regex for the start of a message.
    start_regex: &'static str,
}

fn re(p: &str) -> regex::Regex {
    regex::Regex::new(p).expect("valid timestamp regex")
}

/// Recognises a whole value as a timestamp.
fn timestamp_format(v: &str) -> Option<TsFormat> {
    use std::sync::OnceLock;
    static ISO_T: OnceLock<regex::Regex> = OnceLock::new();
    static ISO_SPACE: OnceLock<regex::Regex> = OnceLock::new();
    static DATE: OnceLock<regex::Regex> = OnceLock::new();
    static HTTP: OnceLock<regex::Regex> = OnceLock::new();
    static SYSLOG: OnceLock<regex::Regex> = OnceLock::new();
    static CTIME: OnceLock<regex::Regex> = OnceLock::new();
    let iso_t = ISO_T.get_or_init(|| re(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}(?::\d{2}(?:[.,]\d{1,9})?)?(?:Z|[+-]\d{2}:?\d{2})?$"));
    let iso_space = ISO_SPACE.get_or_init(|| re(r"^(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2})(?:([.,:])(\d{3,9}))?( ?(?:Z|[+-]\d{2}:?\d{2}))?$"));
    let date = DATE.get_or_init(|| re(r"^\d{4}-\d{2}-\d{2}$"));
    let http = HTTP.get_or_init(|| re(r"^\d{2}/[A-Z][a-z]{2}/\d{4}:\d{2}:\d{2}:\d{2} [+-]\d{4}$"));
    let syslog = SYSLOG.get_or_init(|| re(r"^[A-Z][a-z]{2} {1,2}\d{1,2} \d{2}:\d{2}:\d{2}$"));
    let ctime = CTIME.get_or_init(|| re(r"^[A-Z][a-z]{2} [A-Z][a-z]{2} \d{2} \d{2}:\d{2}:\d{2} \d{4}$"));
    let simple = |java: &str, joda: &str, mapping: Option<&str>, need_tz: bool, grok: &'static str, start: &'static str| TsFormat {
        java: java.into(),
        joda: joda.into(),
        mapping_format: mapping.map(str::to_string),
        need_tz,
        grok,
        start_regex: start,
    };
    if v.len() == 13 && v.bytes().all(|b| b.is_ascii_digit()) {
        return Some(simple("UNIX_MS", "UNIX_MS", Some("epoch_millis"), false, "POSINT", r"\b\d{13}\b"));
    }
    if (v.len() == 10 && v.bytes().all(|b| b.is_ascii_digit()))
        || (v.len() > 11 && v.as_bytes()[10] == b'.' && v[..10].bytes().all(|b| b.is_ascii_digit()) && (14..=20).contains(&v.len()) && v[11..].bytes().all(|b| b.is_ascii_digit()))
    {
        return Some(simple("UNIX", "UNIX", Some("epoch_second"), false, "NUMBER", r"\b\d{10}\b"));
    }
    if iso_t.is_match(v) {
        let tz = v.ends_with('Z') || v[10..].contains(['+']) || v[11..].contains('-');
        return Some(simple("ISO8601", "ISO8601", Some("iso8601"), !tz, "TIMESTAMP_ISO8601", r"\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}"));
    }
    if let Some(c) = iso_space.captures(v) {
        let mut java = "yyyy-MM-dd HH:mm:ss".to_string();
        if let (Some(sep), Some(frac)) = (c.get(2), c.get(3)) {
            java.push_str(sep.as_str());
            java.push_str(&"S".repeat(frac.as_str().len()));
        }
        let tz = c.get(4).map(|t| t.as_str());
        if let Some(t) = tz {
            let body = t.trim_start();
            if t.starts_with(' ') {
                java.push(' ');
            }
            java.push_str(if body == "Z" || body.contains(':') { "XXX" } else { "XX" });
        }
        let joda = java.replacen("yyyy", "YYYY", 1).replace("XXX", "ZZ").replace("XX", "Z");
        return Some(TsFormat {
            mapping_format: Some(java.clone()),
            java,
            joda,
            need_tz: tz.is_none(),
            grok: "TIMESTAMP_ISO8601",
            start_regex: r"\b\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}",
        });
    }
    if http.is_match(v) {
        return Some(simple("dd/MMM/yyyy:HH:mm:ss XX", "dd/MMM/YYYY:HH:mm:ss Z", Some("dd/MMM/yyyy:HH:mm:ss XX"), false, "HTTPDATE", r"\b\d{2}/[A-Z]\S{2}/\d{4}:\d{2}:\d{2}:\d{2} "));
    }
    if ctime.is_match(v) {
        return Some(simple("EEE MMM dd HH:mm:ss yyyy", "EEE MMM dd HH:mm:ss YYYY", Some("EEE MMM dd HH:mm:ss yyyy"), true, "HTTPDERROR_DATE", r"\b[A-Z]\S{2} [A-Z]\S{2} \d{2} \d{2}:\d{2}:\d{2} \d{4}\b"));
    }
    if syslog.is_match(v) {
        let two = v.as_bytes()[4] == b' ';
        let (java, joda): (&str, &str) = if two || v[4..].split(' ').next().is_some_and(|d| d.len() == 1) {
            ("MMM d HH:mm:ss", "MMM d HH:mm:ss")
        } else {
            ("MMM dd HH:mm:ss", "MMM dd HH:mm:ss")
        };
        return Some(TsFormat {
            java: java.into(),
            joda: joda.into(),
            mapping_format: Some(java.into()),
            need_tz: true,
            grok: "SYSLOGTIMESTAMP",
            start_regex: r"\b[A-Z]\S{2,8} {1,2}\d{1,2} \d{2}:\d{2}:\d{2}\b",
        });
    }
    if date.is_match(v) {
        return Some(simple("ISO8601", "ISO8601", Some("iso8601"), true, "CUSTOM_TIMESTAMP", r"\b\d{4}-\d{2}-\d{2}\b"));
    }
    None
}

/// The format every value shares (`None` when they don't share one).
fn common_format<'a>(values: impl Iterator<Item = &'a str>) -> Option<TsFormat> {
    let mut fmt: Option<TsFormat> = None;
    let mut any = false;
    for v in values {
        any = true;
        let f = timestamp_format(v)?;
        match &fmt {
            None => fmt = Some(f),
            Some(x) if x.java == f.java => {}
            Some(_) => return None,
        }
    }
    if any { fmt } else { None }
}

/// Epoch millis of a value in a recognised format (for earliest/latest).
fn ts_millis(v: &str, f: &TsFormat) -> Option<i128> {
    match f.java.as_str() {
        "UNIX_MS" => v.parse::<i128>().ok(),
        "UNIX" => v.parse::<f64>().ok().map(|s| (s * 1000.0) as i128),
        _ => {
            // Every other recognised format orders lexicographically within the sample except
            // month names; normalise those.
            let months = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
            let digits: String = months.iter().enumerate().fold(v.to_string(), |s, (i, m)| s.replace(m, &format!("{:02}", i + 1))).chars().filter(char::is_ascii_digit).collect();
            let key = if f.java.starts_with("dd/MMM/yyyy") && digits.len() >= 8 {
                format!("{}{}{}{}", &digits[4..8], &digits[2..4], &digits[0..2], &digits[8..])
            } else {
                digits
            };
            key.get(..18.min(key.len()))?.parse::<i128>().ok()
        }
    }
}

// --- field typing and statistics -------------------------------------------------------------------

fn is_number(s: &str) -> bool {
    // Grok's NUMBER: optional sign, digits with an optional fraction (or a bare fraction).
    let t = s.strip_prefix(['+', '-']).unwrap_or(s);
    if t.is_empty() {
        return false;
    }
    let (int, frac) = match t.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (t, None),
    };
    int.bytes().all(|b| b.is_ascii_digit()) && frac.is_none_or(|f| f.bytes().all(|b| b.is_ascii_digit())) && (!int.is_empty() || frac.is_some_and(|f| !f.is_empty()))
}

fn more_like_text(s: &str) -> bool {
    let len = s.chars().count();
    len > KEYWORD_MAX_LEN || s.chars().filter(|c| c.is_whitespace()).count() > KEYWORD_MAX_SPACES
}

/// The guessed mapping of a field's values.
fn guess_mapping(values: &[String]) -> (Map, Option<TsFormat>) {
    let mut m = Map::new();
    if values.iter().all(|v| v == "true" || v == "false") {
        m.insert("type".into(), "boolean".into());
        return (m, None);
    }
    if let Some(f) = common_format(values.iter().map(String::as_str)) {
        m.insert("type".into(), "date".into());
        if let Some(fmt) = &f.mapping_format {
            m.insert("format".into(), fmt.clone().into());
        }
        return (m, Some(f));
    }
    if values.iter().all(|v| is_number(v)) {
        let ty = if values.iter().all(|v| v.parse::<i64>().is_ok()) { "long" } else { "double" };
        m.insert("type".into(), ty.into());
        return (m, None);
    }
    if values.iter().all(|v| v.parse::<std::net::IpAddr>().is_ok()) {
        m.insert("type".into(), "ip".into());
        return (m, None);
    }
    m.insert("type".into(), if values.iter().any(|v| more_like_text(v)) { "text" } else { "keyword" }.into());
    (m, None)
}

/// A double as a JSON value, integral values as integers (`FieldStats.toIntegerIfInteger`).
fn num_value(d: f64) -> Value {
    if d.is_finite() && d == d.trunc() && d.abs() < 9.007_199_254_740_992e15 {
        Value::from(d as i64)
    } else {
        Value::from(d)
    }
}

fn field_stats(values: &[String], mapping: &Map, ts: Option<&TsFormat>) -> Value {
    let ty = mapping.get("type").and_then(Value::as_str).unwrap_or("keyword");
    let mut m = Map::new();
    m.insert("count".into(), values.len().into());
    if matches!(ty, "long" | "double") {
        let mut nums: Vec<f64> = values.iter().filter_map(|v| v.parse::<f64>().ok()).collect();
        nums.sort_by(f64::total_cmp);
        let mut counts: Vec<(f64, usize)> = Vec::new();
        for n in &nums {
            match counts.last_mut() {
                Some((v, c)) if *v == *n => *c += 1,
                _ => counts.push((*n, 1)),
            }
        }
        m.insert("cardinality".into(), counts.len().into());
        if let (Some(min), Some(max)) = (nums.first(), nums.last()) {
            m.insert("min_value".into(), num_value(*min));
            m.insert("max_value".into(), num_value(*max));
            // A running mean over distinct values, as Elasticsearch computes it.
            let (mut run_count, mut mean) = (0.0f64, f64::NAN);
            for (v, c) in &counts {
                let c = *c as f64;
                let next = run_count + c;
                mean = if run_count > 0.0 { mean * (run_count / next) + v * (c / next) } else { *v };
                run_count = next;
            }
            m.insert("mean_value".into(), num_value(mean));
            let n = nums.len();
            let median = if n % 2 == 1 { nums[n / 2] } else { (nums[n / 2 - 1] + nums[n / 2]) / 2.0 };
            m.insert("median_value".into(), num_value(median));
        }
        let mut top = counts.clone();
        top.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.total_cmp(&b.0)));
        m.insert("top_hits".into(), Value::Arr(top.into_iter().take(NUM_TOP_HITS).map(|(v, c)| obj! { "value" => num_value(v), "count" => c }).collect()));
        return Value::Obj(m);
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for v in values {
        *counts.entry(v.as_str()).or_default() += 1;
    }
    m.insert("cardinality".into(), counts.len().into());
    if let Some(f) = ts {
        let mut earliest: Option<(i128, &str)> = None;
        let mut latest: Option<(i128, &str)> = None;
        for v in values {
            let Some(t) = ts_millis(v, f) else { continue };
            if earliest.is_none_or(|(e, _)| t < e) {
                earliest = Some((t, v));
            }
            if latest.is_none_or(|(l, _)| t > l) {
                latest = Some((t, v));
            }
        }
        if let (Some(e), Some(l)) = (earliest, latest) {
            m.insert("earliest".into(), e.1.into());
            m.insert("latest".into(), l.1.into());
        }
    }
    // Java's `String.compareTo` order breaks count ties (UTF-16 units; ASCII the same).
    let mut top: Vec<(&str, usize)> = counts.into_iter().collect();
    top.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.encode_utf16().cmp(b.0.encode_utf16())));
    m.insert("top_hits".into(), Value::Arr(top.into_iter().take(NUM_TOP_HITS).map(|(v, c)| obj! { "value" => v, "count" => c }).collect()));
    Value::Obj(m)
}

// --- the finder ------------------------------------------------------------------------------------

/// A finder's result before rendering.
struct Found {
    format: &'static str,
    lines_analyzed: usize,
    messages_analyzed: usize,
    sample_start: String,
    /// Format-specific keys, in output order, before `ecs_compatibility`.
    extra: Vec<(String, Value)>,
    timestamp: Option<(String, TsFormat)>,
    mappings: BTreeMap<String, Value>,
    stats: BTreeMap<String, Value>,
    /// Pipeline processors before the date processor, and after it.
    pre: Vec<Value>,
    post: Vec<Value>,
    explanation: Vec<String>,
}

fn render(f: Found, o: &Overrides, with_bom: Option<bool>) -> Value {
    let mut m = Map::new();
    m.insert("num_lines_analyzed".into(), f.lines_analyzed.into());
    m.insert("num_messages_analyzed".into(), f.messages_analyzed.into());
    m.insert("sample_start".into(), f.sample_start.into());
    m.insert("charset".into(), "UTF-8".into());
    if let Some(b) = with_bom {
        m.insert("has_byte_order_marker".into(), b.into());
    }
    m.insert("format".into(), f.format.into());
    for (k, v) in f.extra {
        m.insert(k, v);
    }
    m.insert("ecs_compatibility".into(), o.ecs_compatibility.clone().unwrap_or_else(|| "disabled".into()).into());
    let mut mappings = f.mappings;
    let mut processors = f.pre;
    if let Some((field, ts)) = &f.timestamp {
        m.insert("timestamp_field".into(), field.clone().into());
        m.insert("joda_timestamp_formats".into(), crate::arr![ts.joda.clone()]);
        m.insert("java_timestamp_formats".into(), crate::arr![ts.java.clone()]);
        m.insert("need_client_timezone".into(), ts.need_tz.into());
        mappings.insert("@timestamp".into(), obj! { "type" => "date" });
        let mut d = Map::new();
        d.insert("field".into(), field.clone().into());
        if ts.need_tz {
            d.insert("timezone".into(), "{{ event.timezone }}".into());
        }
        d.insert("formats".into(), crate::arr![ts.java.clone()]);
        processors.push(obj! { "date" => Value::Obj(d) });
    } else {
        m.insert("need_client_timezone".into(), false.into());
    }
    processors.extend(f.post);
    let props: Map = mappings.into_iter().collect();
    m.insert("mappings".into(), obj! { "properties" => Value::Obj(props) });
    if !processors.is_empty() {
        m.insert("ingest_pipeline".into(), obj! { "description" => "Ingest pipeline created by text structure finder", "processors" => Value::Arr(processors) });
    }
    m.insert("field_stats".into(), Value::Obj(f.stats.into_iter().collect()));
    if o.explain {
        m.insert("explanation".into(), Value::Arr(f.explanation.into_iter().map(Value::from).collect()));
    }
    Value::Obj(m)
}

fn scalar_string(v: &Value) -> Option<String> {
    match v {
        Value::Null | Value::Obj(_) | Value::Arr(_) => None,
        other => Some(other.to_es_string()),
    }
}

/// Collects leaf values of NDJSON records by dotted field name (first-seen order).
fn collect(prefix: &str, v: &Value, out: &mut Vec<(String, Vec<String>)>, objects: &mut Vec<String>) {
    match v {
        Value::Obj(m) => {
            if !prefix.is_empty() && !objects.contains(&prefix.to_string()) {
                objects.push(prefix.to_string());
            }
            for (k, x) in m {
                let name = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                collect(&name, x, out, objects);
            }
        }
        Value::Arr(a) => {
            for x in a {
                collect(prefix, x, out, objects);
            }
        }
        other => {
            let entry = match out.iter_mut().position(|(n, _)| n == prefix) {
                Some(i) => &mut out[i],
                None => {
                    out.push((prefix.to_string(), Vec::new()));
                    out.last_mut().expect("pushed")
                }
            };
            if let Some(s) = scalar_string(other) {
                entry.1.push(s);
            }
        }
    }
}

fn sample_start(lines: &[&str]) -> String {
    let mut s = String::new();
    for l in lines.iter().take(2) {
        s.push_str(l);
        s.push('\n');
    }
    s
}

fn pick_timestamp(fields: &[(String, Vec<String>)], o: &Overrides, explanation: &mut Vec<String>) -> EsResult<Option<(String, TsFormat)>> {
    if let Some(tf) = &o.timestamp_field {
        let Some((_, vals)) = fields.iter().find(|(n, _)| n == tf) else {
            return Err(EsError::illegal_argument(format!("Specified timestamp field [{tf}] is not present in record [{}]", "{}")));
        };
        return match common_format(vals.iter().map(String::as_str)) {
            Some(f) => Ok(Some((tf.clone(), f))),
            None => Err(EsError::illegal_argument(format!("Specified timestamp field [{tf}] is not a valid timestamp"))),
        };
    }
    for (name, vals) in fields {
        if let Some(f) = common_format(vals.iter().map(String::as_str)) {
            explanation.push(format!("Guessing timestamp field is [{name}] with format [{}]", f.java));
            return Ok(Some((name.clone(), f)));
        }
    }
    Ok(None)
}

fn ndjson(lines: &[&str], o: &Overrides) -> Option<EsResult<Found>> {
    let mut records = Vec::with_capacity(lines.len());
    for l in lines {
        match crate::json::parse(l.as_bytes()) {
            Ok(Some(v @ Value::Obj(_))) => records.push(v),
            _ => return None,
        }
    }
    if records.is_empty() {
        return None;
    }
    let mut explanation = vec!["Using character encoding [UTF-8], which matched the input with [100%] confidence".to_string(), "Deciding sample is newline-delimited JSON".to_string()];
    let mut fields: Vec<(String, Vec<String>)> = Vec::new();
    let mut objects = Vec::new();
    for r in &records {
        collect("", r, &mut fields, &mut objects);
    }
    fields.retain(|(_, v)| !v.is_empty());
    let ts = match pick_timestamp(&fields, o, &mut explanation) {
        Ok(t) => t,
        Err(e) => return Some(Err(e)),
    };
    let mut mappings = BTreeMap::new();
    let mut stats = BTreeMap::new();
    for (name, vals) in &fields {
        let (mapping, f) = guess_mapping(vals);
        stats.insert(name.clone(), field_stats(vals, &mapping, f.as_ref()));
        insert_mapping(&mut mappings, name, Value::Obj(mapping));
    }
    Some(Ok(Found {
        format: "ndjson",
        lines_analyzed: lines.len(),
        messages_analyzed: records.len(),
        sample_start: sample_start(lines),
        extra: Vec::new(),
        timestamp: ts,
        mappings,
        stats,
        pre: Vec::new(),
        post: Vec::new(),
        explanation,
    }))
}

/// Inserts a dotted field's mapping as nested `properties`.
fn insert_mapping(root: &mut BTreeMap<String, Value>, name: &str, mapping: Value) {
    match name.split_once('.') {
        None => {
            root.insert(name.to_string(), mapping);
        }
        Some((head, rest)) => {
            let entry = root.entry(head.to_string()).or_insert_with(|| obj! { "properties" => obj! {} });
            if entry.get("properties").is_none() {
                *entry = obj! { "properties" => obj! {} };
            }
            if let Some(Value::Obj(props)) = entry.as_object_mut().and_then(|m| m.get_mut("properties")) {
                let mut sub: BTreeMap<String, Value> = props.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
                insert_mapping(&mut sub, rest, mapping);
                *props = sub.into_iter().collect();
            }
        }
    }
}

/// Splits one delimited line (quotes double to escape; `None`: an unterminated quote).
fn split_row(line: &str, delim: char, quote: char) -> Option<Vec<String>> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    let mut chars = line.chars().peekable();
    let mut was_quoted = false;
    while let Some(c) = chars.next() {
        if in_q {
            if c == quote {
                if chars.peek() == Some(&quote) {
                    cur.push(quote);
                    chars.next();
                } else {
                    in_q = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == quote && cur.is_empty() && !was_quoted {
            in_q = true;
            was_quoted = true;
        } else if c == delim {
            out.push(std::mem::take(&mut cur));
            was_quoted = false;
        } else {
            cur.push(c);
        }
    }
    if in_q {
        return None;
    }
    out.push(cur);
    Some(out)
}

fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + usize::from(a[i - 1] != b[j - 1]));
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

fn rows_distance(a: &[String], b: &[String], short: &[bool]) -> usize {
    let n = a.len().max(b.len());
    (0..n)
        .filter(|i| short.get(*i).copied().unwrap_or(false))
        .map(|i| levenshtein(a.get(i).map_or("", String::as_str), b.get(i).map_or("", String::as_str)))
        .sum()
}

fn first_row_unusual(rows: &[Vec<String>]) -> bool {
    let joined: Vec<usize> = rows.iter().map(|r| r.iter().map(|f| f.chars().count()).sum()).collect();
    let first = joined[0] as f64;
    let others = &joined[1..];
    let (min, max) = (*others.iter().min().expect("rows") as f64, *others.iter().max().expect("rows") as f64);
    let range = max - min;
    if first < min - range / 10.0 || first > max + range / 10.0 {
        return true;
    }
    let width = rows.iter().map(Vec::len).max().unwrap_or(0);
    let short: Vec<bool> = (0..width).map(|i| rows.iter().all(|r| r.get(i).is_none_or(|f| f.chars().count() < LONG_FIELD_THRESHOLD))).collect();
    let other_rows = &rows[1..];
    let first_avg = {
        let ds: Vec<usize> = other_rows.iter().take(MAX_LEVENSHTEIN_COMPARISONS).map(|r| rows_distance(&rows[0], r, &short)).collect();
        ds.iter().sum::<usize>() as f64 / ds.len().max(1) as f64
    };
    let proportion = other_rows.len() / MAX_LEVENSHTEIN_COMPARISONS;
    let inc = 1 + proportion * proportion;
    let mut ds = Vec::new();
    'outer: for i in 0..other_rows.len() {
        let mut j = i + 1;
        while j < other_rows.len() {
            if ds.len() >= MAX_LEVENSHTEIN_COMPARISONS {
                break 'outer;
            }
            ds.push(rows_distance(&other_rows[i], &other_rows[j], &short));
            j += inc;
        }
    }
    let other_avg = if ds.is_empty() { 0.0 } else { ds.iter().sum::<usize>() as f64 / ds.len() as f64 };
    first_avg > other_avg * 1.2
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.[]{}()*+?^$|".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn delimited(lines: &[&str], o: &Overrides, delim: char, min_fields: usize, explanation: &mut Vec<String>) -> Option<EsResult<Found>> {
    let quote = o.quote.unwrap_or('"');
    let name = match delim {
        ',' => "CSV",
        '\t' => "TSV",
        ';' => "semicolon delimited values",
        '|' => "vertical line delimited values",
        _ => "delimited values",
    };
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(lines.len());
    for l in lines {
        rows.push(split_row(l, delim, quote)?);
    }
    let first_len = rows.first()?.len();
    if first_len < min_fields {
        explanation.push(format!("Not {name} because the first row has fewer than [{min_fields}] fields: [{first_len}]"));
        return None;
    }
    let bad: Vec<String> = rows.iter().enumerate().skip(1).filter(|(_, r)| r.len() != first_len).map(|(i, _)| (i + 1).to_string()).collect();
    if !bad.is_empty() {
        explanation.push(format!("Not {name} because 1 or more rows did not have the same number of fields as the first row ({first_len}). Bad rows [{}]", bad.join(", ")));
        return None;
    }
    if rows.len() < 2 && o.format.as_deref() != Some("delimited") {
        explanation.push(format!("Not {name} because there are too few rows"));
        return None;
    }
    let trim = o.should_trim_fields.unwrap_or(delim == '|');
    let has_dup = {
        let mut seen = std::collections::HashSet::new();
        rows[0].iter().filter(|f| !f.is_empty()).any(|f| !seen.insert(f.clone()))
    };
    let header = match o.has_header_row {
        Some(h) => h,
        None if has_dup => false,
        None if rows.len() < 3 => true,
        None => first_row_unusual(&rows),
    };
    let columns: Vec<String> = match &o.column_names {
        Some(c) => {
            if c.len() != first_len {
                return Some(Err(EsError::illegal_argument(format!("[{}] column names were specified [{}] but there are [{first_len}] columns in the sample", c.len(), c.join(",")))));
            }
            c.clone()
        }
        None => (0..first_len)
            .map(|i| {
                let raw = if header { rows[0][i].clone() } else { String::new() };
                let raw = if trim { raw.trim().to_string() } else { raw };
                if raw.is_empty() { format!("column{}", i + 1) } else { raw.replace('.', "_") }
            })
            .collect(),
    };
    let data = if header { &rows[1..] } else { &rows[..] };
    let mut fields: Vec<(String, Vec<String>)> = columns.iter().map(|c| (c.clone(), Vec::new())).collect();
    for r in data {
        for (i, v) in r.iter().enumerate() {
            let v = if trim { v.trim() } else { v.as_str() };
            if !v.is_empty() {
                fields[i].1.push(v.to_string());
            }
        }
    }
    let ts = match pick_timestamp(&fields, o, explanation) {
        Ok(t) => t,
        Err(e) => return Some(Err(e)),
    };
    let mut mappings = BTreeMap::new();
    let mut stats = BTreeMap::new();
    let mut converts = Vec::new();
    for (name, vals) in &fields {
        if vals.is_empty() {
            mappings.insert(name.clone(), obj! { "type" => "keyword" });
            continue;
        }
        let (mapping, f) = guess_mapping(vals);
        if let Some(t) = mapping.get("type").and_then(Value::as_str)
            && matches!(t, "long" | "double" | "boolean")
        {
            converts.push(obj! { "convert" => obj! { "field" => name.clone(), "type" => t, "ignore_missing" => true } });
        }
        stats.insert(name.clone(), field_stats(vals, &mapping, f.as_ref()));
        mappings.insert(name.clone(), Value::Obj(mapping));
    }
    let mut extra: Vec<(String, Value)> = Vec::new();
    if header {
        let q = regex_escape(&quote.to_string());
        let parts: Vec<String> = rows[0].iter().map(|f| format!("{q}?{}{q}?", regex_escape(f))).collect();
        extra.push(("exclude_lines_pattern".into(), format!("^{}", parts.join(&regex_escape(&delim.to_string()))).into()));
    }
    extra.push(("column_names".into(), Value::Arr(columns.iter().map(|c| Value::from(c.clone())).collect())));
    extra.push(("has_header_row".into(), header.into()));
    extra.push(("delimiter".into(), delim.to_string().into()));
    extra.push(("quote".into(), quote.to_string().into()));
    if trim {
        extra.push(("should_trim_fields".into(), true.into()));
    }
    let mut csv = Map::new();
    csv.insert("field".into(), "message".into());
    csv.insert("target_fields".into(), Value::Arr(columns.iter().map(|c| Value::from(c.clone())).collect()));
    if delim != ',' {
        csv.insert("separator".into(), delim.to_string().into());
    }
    if quote != '"' {
        csv.insert("quote".into(), quote.to_string().into());
    }
    csv.insert("ignore_missing".into(), false.into());
    if trim {
        csv.insert("trim".into(), true.into());
    }
    let mut post = converts;
    if !columns.iter().any(|c| c == "message") {
        post.push(obj! { "remove" => obj! { "field" => "message" } });
    }
    Some(Ok(Found {
        format: "delimited",
        lines_analyzed: lines.len(),
        messages_analyzed: data.len(),
        sample_start: sample_start(lines),
        extra,
        timestamp: ts,
        mappings,
        stats,
        pre: vec![obj! { "csv" => Value::Obj(csv) }],
        post,
        explanation: std::mem::take(explanation),
    }))
}

/// Finds the longest prefix timestamp of a line.
fn leading_timestamp(line: &str) -> Option<(usize, TsFormat)> {
    let mut ends: Vec<usize> = line.char_indices().filter(|(_, c)| *c == ' ' || *c == ']' || *c == ',' || *c == '|').map(|(i, _)| i).collect();
    ends.push(line.len());
    let start = usize::from(line.starts_with('['));
    let mut best = None;
    for e in ends {
        if e <= start {
            continue;
        }
        if let Some(f) = timestamp_format(&line[start..e]) {
            best = Some((e, f));
        }
    }
    best
}

fn semi_structured(lines: &[&str], o: &Overrides, merge_limit: usize, explanation: &mut Vec<String>) -> EsResult<Found> {
    let first = lines.iter().find(|l| !l.trim().is_empty()).copied().unwrap_or("");
    let Some((_, fmt)) = leading_timestamp(first) else {
        return Err(EsError::illegal_argument("Could not find a timestamp in the sample provided"));
    };
    let start = regex::Regex::new(&format!("^\\[?{}", fmt.start_regex)).map_err(|e| EsError::illegal_argument(e.to_string()))?;
    let mut messages: Vec<String> = Vec::new();
    let mut ts_values = Vec::new();
    for l in lines {
        if start.is_match(l) || messages.is_empty() {
            if let Some((e, _)) = leading_timestamp(l) {
                ts_values.push(l[usize::from(l.starts_with('['))..e].to_string());
            }
            messages.push(l.to_string());
        } else if let Some(m) = messages.last_mut() {
            if m.len() + l.len() + 1 > merge_limit {
                return Err(EsError::illegal_argument(format!(
                    "Merging lines into messages resulted in an unacceptably long message. Merged message would have been [{}] characters long. [line_merge_size_limit] was set to [{merge_limit}]. This probably means the input is not a sequence of log messages, or the [line_merge_size_limit] needs increasing.",
                    m.len() + l.len() + 1
                )));
            }
            m.push('\n');
            m.push_str(l);
        }
    }
    explanation.push(format!("Most likely timestamp format is [{}]", fmt.java));
    let grok = format!("{}%{{{}:timestamp}}.*", if first.starts_with('[') { "\\[" } else { "" }, fmt.grok);
    let mut mappings = BTreeMap::new();
    mappings.insert("message".to_string(), obj! { "type" => "text" });
    let mut stats = BTreeMap::new();
    let (tm, _) = guess_mapping(&ts_values);
    if !ts_values.is_empty() {
        stats.insert("timestamp".to_string(), field_stats(&ts_values, &tm, Some(&fmt)));
    }
    let multiline = format!("^{}", fmt.start_regex);
    let _ = o;
    Ok(Found {
        format: "semi_structured_text",
        lines_analyzed: lines.len(),
        messages_analyzed: messages.len(),
        sample_start: sample_start(lines),
        extra: vec![("multiline_start_pattern".into(), multiline.into()), ("grok_pattern".into(), grok.clone().into())],
        timestamp: Some(("timestamp".into(), fmt)),
        mappings,
        stats,
        pre: vec![obj! { "grok" => obj! { "field" => "message", "patterns" => crate::arr![grok], "ecs_compatibility" => "disabled" } }],
        post: vec![obj! { "remove" => obj! { "field" => "timestamp" } }],
        explanation: std::mem::take(explanation),
    })
}

/// Runs the finder over sample lines.
fn find(lines: &[&str], o: &Overrides, merge_limit: usize) -> EsResult<Found> {
    let lines: Vec<&str> = lines.iter().copied().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return Err(EsError::illegal_argument("Input did not contain any messages"));
    }
    let want = o.format.as_deref();
    let mut explanation = vec!["Using character encoding [UTF-8], which matched the input with [100%] confidence".to_string()];
    if want.is_none_or(|f| f == "ndjson") {
        match ndjson(&lines, o) {
            Some(r) => return r,
            None => explanation.push("Not NDJSON because there was a parsing exception".to_string()),
        }
    }
    if want.is_none_or(|f| f == "delimited") {
        let candidates: Vec<(char, usize)> = match o.delimiter {
            Some(d) => vec![(d, 2)],
            None => vec![(',', 2), ('\t', 2), (';', 4), ('|', 5)],
        };
        for (d, min) in candidates {
            if let Some(r) = delimited(&lines, o, d, min, &mut explanation) {
                return r;
            }
        }
    }
    if want == Some("xml") {
        return Err(EsError::illegal_argument("Input did not match the specified format [xml]"));
    }
    semi_structured(&lines, o, merge_limit, &mut explanation)
}

// --- REST --------------------------------------------------------------------------------------------

pub fn find_structure(ctx: &Ctx) -> EsResult<Reply> {
    let o = overrides(ctx)?;
    let lines_to_sample = ctx.int_param("lines_to_sample", 1000)? as usize;
    let merge_limit = ctx.int_param("line_merge_size_limit", 10_000)? as usize;
    let body: &[u8] = &ctx.body;
    if body.is_empty() {
        return Err(EsError::action_request_validation(&["sample must be specified".into()]));
    }
    let (bom, body) = match body.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let text = std::str::from_utf8(body).map_err(|_| EsError::illegal_argument("Could not determine a usable character encoding for the input - could it be binary data?"))?;
    let lines: Vec<&str> = text.lines().take(lines_to_sample).collect();
    let found = find(&lines, &o, merge_limit)?;
    Reply::ok(render(found, &o, Some(bom)))
}

fn analyze_messages(ctx: &Ctx, messages: &[String]) -> EsResult<Reply> {
    let o = overrides(ctx)?;
    if messages.is_empty() {
        return Err(EsError::illegal_argument("Input did not contain any messages"));
    }
    // Each message is one record; a message spanning lines is kept whole.
    let lines: Vec<&str> = messages.iter().map(String::as_str).collect();
    let mut found = find(&lines, &o, usize::MAX)?;
    if found.format == "semi_structured_text" {
        found.messages_analyzed = messages.len();
        found.extra.retain(|(k, _)| k != "multiline_start_pattern");
    }
    Reply::ok(render(found, &o, None))
}

pub fn find_message_structure(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let Some(list) = body.get("messages").and_then(Value::as_array) else {
        return Err(EsError::action_request_validation(&["messages must be specified".into()]));
    };
    let messages: Vec<String> = list.iter().map(Value::to_es_string).collect();
    analyze_messages(ctx, &messages)
}

pub async fn find_field_structure(ctx: &Ctx<'_>) -> EsResult<Reply> {
    let mut errors = Vec::new();
    let index = ctx.query("index").map(str::to_string);
    let field = ctx.query("field").map(str::to_string);
    if index.as_deref().is_none_or(str::is_empty) {
        errors.push("index must be specified".to_string());
    }
    if field.as_deref().is_none_or(str::is_empty) {
        errors.push("field must be specified".to_string());
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    let (index, field) = (index.unwrap_or_default(), field.unwrap_or_default());
    let docs = ctx.int_param("documents_to_sample", 1000)?;
    if docs < 2 {
        return Err(EsError::illegal_argument(format!("[documents_to_sample] must be at least [2] if specified, got [{docs}]")));
    }
    let body = obj! {
        "size" => docs,
        "query" => obj! { "exists" => obj! { "field" => field.clone() } },
        "_source" => false,
        "fields" => crate::arr![field.clone()],
        "track_total_hits" => false,
    };
    let r = crate::xpack::internal::call(ctx.cluster, "POST", &format!("/{}/_search", crate::xpack::enc(&index)), Some(&body)).await?;
    let mut messages = Vec::new();
    for h in r.pointer(&["hits", "hits"]).and_then(Value::as_array).into_iter().flatten() {
        if let Some(Value::Arr(vals)) = h.pointer(&["fields", field.as_str()])
            && let Some(v) = vals.first()
        {
            messages.push(v.to_es_string());
        }
    }
    if messages.is_empty() {
        return Err(EsError::illegal_argument("Input did not contain any messages"));
    }
    analyze_messages(ctx, &messages)
}

pub fn test_grok_pattern(ctx: &Ctx) -> EsResult<Reply> {
    let body = ctx.body()?.unwrap_or_else(Value::object);
    let mut errors = Vec::new();
    let pattern = body.get("grok_pattern").and_then(Value::as_str).map(str::to_string);
    let texts: Option<Vec<String>> = body.get("text").and_then(Value::as_array).map(|a| a.iter().map(Value::to_es_string).collect());
    if pattern.is_none() {
        errors.push("[grok_pattern] is required".to_string());
    }
    if texts.is_none() {
        errors.push("[text] is required".to_string());
    }
    if !errors.is_empty() {
        return Err(EsError::action_request_validation(&errors));
    }
    let bank = match ctx.query("ecs_compatibility") {
        Some("v1") => search_painless::grok::PatternBank::ecs_v1(),
        _ => search_painless::grok::PatternBank::legacy(),
    };
    let grok = search_painless::grok::Grok::new(bank, pattern.as_deref().unwrap_or_default()).map_err(|e| EsError::illegal_argument(e.message))?;
    let mut matches = Vec::new();
    for t in texts.unwrap_or_default() {
        let r = grok.capture_ranges(&t).map_err(|e| EsError::illegal_argument(e.reason()))?;
        matches.push(match r {
            None => obj! { "matched" => false },
            Some(caps) => {
                let mut fields = Map::new();
                for (name, ranges) in caps {
                    fields.insert(name, Value::Arr(ranges.into_iter().map(|(m, off, len)| obj! { "match" => m, "offset" => off, "length" => len }).collect()));
                }
                obj! { "matched" => true, "fields" => Value::Obj(fields) }
            }
        });
    }
    Reply::ok(obj! { "matches" => Value::Arr(matches) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str) -> Value {
        let lines: Vec<&str> = text.lines().collect();
        render(find(&lines, &Overrides::default(), 10_000).unwrap(), &Overrides::default(), Some(false))
    }

    #[test]
    fn ndjson_matches_elasticsearch() {
        let v = run("{\"airline\":\"AAL\",\"responsetime\":132.2046,\"sourcetype\":\"text-structure-test\",\"time\":1403481600}\n{\"airline\":\"JZA\",\"responsetime\":990.4628,\"sourcetype\":\"text-structure-test\",\"time\":1403481700}\n{\"airline\":\"AAL\",\"responsetime\":134.2046,\"sourcetype\":\"text-structure-test\",\"time\":1403481800}\n");
        // Captured from Elasticsearch 9.5.5.
        let want = r#"{"num_lines_analyzed":3,"num_messages_analyzed":3,"sample_start":"{\"airline\":\"AAL\",\"responsetime\":132.2046,\"sourcetype\":\"text-structure-test\",\"time\":1403481600}\n{\"airline\":\"JZA\",\"responsetime\":990.4628,\"sourcetype\":\"text-structure-test\",\"time\":1403481700}\n","charset":"UTF-8","has_byte_order_marker":false,"format":"ndjson","ecs_compatibility":"disabled","timestamp_field":"time","joda_timestamp_formats":["UNIX"],"java_timestamp_formats":["UNIX"],"need_client_timezone":false,"mappings":{"properties":{"@timestamp":{"type":"date"},"airline":{"type":"keyword"},"responsetime":{"type":"double"},"sourcetype":{"type":"keyword"},"time":{"type":"date","format":"epoch_second"}}},"ingest_pipeline":{"description":"Ingest pipeline created by text structure finder","processors":[{"date":{"field":"time","formats":["UNIX"]}}]},"field_stats":{"airline":{"count":3,"cardinality":2,"top_hits":[{"value":"AAL","count":2},{"value":"JZA","count":1}]},"responsetime":{"count":3,"cardinality":3,"min_value":132.2046,"max_value":990.4628,"mean_value":418.95733333333334,"median_value":134.2046,"top_hits":[{"value":132.2046,"count":1},{"value":134.2046,"count":1},{"value":990.4628,"count":1}]},"sourcetype":{"count":3,"cardinality":1,"top_hits":[{"value":"text-structure-test","count":3}]},"time":{"count":3,"cardinality":3,"earliest":"1403481600","latest":"1403481800","top_hits":[{"value":"1403481600","count":1},{"value":"1403481700","count":1},{"value":"1403481800","count":1}]}}}"#;
        assert_eq!(v.to_json(), want);
    }

    #[test]
    fn csv_matches_elasticsearch() {
        let lines = ["a,b,c", "1,2,3", "4,5,6"];
        let o = Overrides::default();
        let v = render(find(&lines, &o, 10_000).unwrap(), &o, None);
        let want = r#"{"num_lines_analyzed":3,"num_messages_analyzed":3,"sample_start":"a,b,c\n1,2,3\n","charset":"UTF-8","format":"delimited","column_names":["column1","column2","column3"],"has_header_row":false,"delimiter":",","quote":"\"","ecs_compatibility":"disabled","need_client_timezone":false,"mappings":{"properties":{"column1":{"type":"keyword"},"column2":{"type":"keyword"},"column3":{"type":"keyword"}}},"ingest_pipeline":{"description":"Ingest pipeline created by text structure finder","processors":[{"csv":{"field":"message","target_fields":["column1","column2","column3"],"ignore_missing":false}},{"remove":{"field":"message"}}]},"field_stats":{"column1":{"count":3,"cardinality":3,"top_hits":[{"value":"1","count":1},{"value":"4","count":1},{"value":"a","count":1}]},"column2":{"count":3,"cardinality":3,"top_hits":[{"value":"2","count":1},{"value":"5","count":1},{"value":"b","count":1}]},"column3":{"count":3,"cardinality":3,"top_hits":[{"value":"3","count":1},{"value":"6","count":1},{"value":"c","count":1}]}}}"#;
        assert_eq!(v.to_json(), want);
    }

    #[test]
    fn timestamps() {
        assert_eq!(timestamp_format("1403481600").unwrap().java, "UNIX");
        assert_eq!(timestamp_format("1403481600123").unwrap().java, "UNIX_MS");
        assert_eq!(timestamp_format("2024-01-02T10:11:12Z").unwrap().java, "ISO8601");
        let f = timestamp_format("2024-01-02 10:11:12,123").unwrap();
        assert_eq!((f.java.as_str(), f.joda.as_str(), f.need_tz), ("yyyy-MM-dd HH:mm:ss,SSS", "YYYY-MM-dd HH:mm:ss,SSS", true));
        assert_eq!(timestamp_format("10/Oct/2000:13:55:36 -0700").unwrap().java, "dd/MMM/yyyy:HH:mm:ss XX");
        assert!(timestamp_format("hello").is_none());
    }

    #[test]
    fn header_heuristics() {
        let rows = |v: &[&[&str]]| v.iter().map(|r| r.iter().map(|s| s.to_string()).collect()).collect::<Vec<Vec<String>>>();
        assert!(!first_row_unusual(&rows(&[&["a", "b"], &["1", "2"], &["4", "5"]])));
        assert!(first_row_unusual(&rows(&[&["name", "count"], &["x", "1"], &["y", "2"], &["z", "3"]])));
        assert_eq!(split_row("a,\"b,c\",\"d\"\"e\"", ',', '"'), Some(vec!["a".to_string(), "b,c".into(), "d\"e".into()]));
    }

    #[test]
    fn semi_structured_text() {
        let v = run("2024-01-02T10:11:12Z INFO started\n  continued line\n2024-01-02T10:11:13Z WARN slow\n");
        assert_eq!(v.get("format").and_then(Value::as_str), Some("semi_structured_text"));
        assert_eq!(v.get("num_messages_analyzed").and_then(Value::as_i64), Some(2));
        assert_eq!(v.get("grok_pattern").and_then(Value::as_str), Some("%{TIMESTAMP_ISO8601:timestamp}.*"));
    }
}
