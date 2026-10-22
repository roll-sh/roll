//! Field type value parsers (`parseCreateField` of each mapper): from the current token to the
//! typed values a field indexes, with each type's coercion rules and exceptions.

use super::ValueError;
use std::collections::BTreeSet;
use crate::error::{EsError, EsResult};
use crate::json::{Number, Value};
use crate::mapping::{FieldMapper, MappingCtx};
use crate::query::geo::{Geometry, GeoPoint};
use crate::query::{Token, XParser};

/// A typed field value, before analysis.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldValue {
    /// `text`, `match_only_text`, `search_as_you_type`, `annotated_text`: analyzed at indexing.
    Text(String),
    /// `keyword` (the normalizer applies at indexing), `wildcard`, `constant_keyword`, `version`.
    Keyword(String),
    /// `long`, `integer`, `short`, `byte`, and `scaled_float` as its scaled long.
    Long(i64),
    UnsignedLong(u64),
    Double(f64),
    /// `float` and `half_float` (already rounded to half precision).
    Float(f32),
    Boolean(bool),
    /// `date` in milliseconds or `date_nanos` in nanoseconds since the epoch.
    Date(i64),
    /// An address as 16 bytes (IPv4 mapped into IPv6).
    Ip([u8; 16]),
    Binary(Vec<u8>),
    GeoPoint(GeoPoint),
    /// `point` (cartesian).
    Point { x: f64, y: f64 },
    /// `geo_shape` and `shape`.
    Shape(Geometry),
    /// A range field value; bounds normalized to inclusive where Elasticsearch does.
    Range { from: Value, to: Value, include_from: bool, include_to: bool },
    DenseVector(Vec<f32>),
    ByteVector(Vec<i8>),
    /// `sparse_vector` and `rank_features`: token weights.
    Features(Vec<(String, f32)>),
    /// `rank_feature`.
    Feature(f32),
    /// `flattened`: each leaf as `key` (dotted) and value text.
    Flattened(Vec<(String, String)>),
    /// `completion`: inputs and weight.
    Completion { inputs: Vec<String>, weight: Option<i64> },
    /// `token_count`: the text whose tokens are counted at indexing.
    TokenCount(String),
}

/// What a value parser works with: the parser on the value, the index's mapping context, the
/// document's `_ignored` set and description.
pub(crate) struct Cx<'a, 'p> {
    pub p: &'a mut XParser<'p>,
    pub ctx: &'a MappingCtx,
    pub ignored: &'a mut BTreeSet<String>,
    pub desc: &'a str,
}

impl Cx<'_, '_> {
    pub(crate) fn ignore(&mut self, name: &str) {
        self.ignored.insert(name.to_string());
    }

    pub(crate) fn doc_error(&self, msg: impl Into<String>) -> EsError {
        let (l, c) = self.p.token_location();
        EsError::document_parsing(format!("[{l}:{c}] {}", msg.into()))
    }
}

/// Whether a field consumes a whole array as one value.
pub(crate) fn takes_array(f: &FieldMapper, p: &XParser<'_>) -> bool {
    match f.ty {
        "dense_vector" | "rank_vectors" => true,
        // `[lon, lat]` is one point; `[[lon, lat], …]` and strings are several.
        "geo_point" | "point" => matches!(p.value(), Value::Arr(a) if a.first().is_some_and(|x| matches!(x, Value::Num(_)))),
        _ => false,
    }
}

fn param_i64(f: &FieldMapper, name: &str, ctx: &MappingCtx) -> Option<i64> {
    f.effective(name, ctx).as_i64()
}

fn coerce(f: &FieldMapper, ctx: &MappingCtx) -> bool {
    if f.def().param_index("coerce").is_some() { f.bool_param("coerce", ctx) } else { true }
}

fn ignore_malformed(f: &FieldMapper, ctx: &MappingCtx) -> bool {
    f.def().param_index("ignore_malformed").is_some() && f.bool_param("ignore_malformed", ctx)
}

/// The raw text of the current number token (Jackson keeps the original digits).
fn number_text(p: &XParser<'_>) -> String {
    p.raw_text()
}

/// Parses the current token as `f`'s value(s). `w` records `_ignored` fields.
pub(crate) fn parse_value(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str) -> Result<Vec<FieldValue>, ValueError> {
    let ctx = w.ctx;
    let token = w.p.token();
    // `null` (and for numbers, the empty string) takes the field's `null_value`.
    let null_value = f.raw("null_value").filter(|v| !v.is_null()).cloned();
    let is_null = token == Token::Null || (token == Token::String && is_numeric(f.ty) && matches!(w.p.value(), Value::Str(s) if s.is_empty()));
    if is_null {
        return match null_value {
            Some(v) => Ok(parse_detached_value(w, f, full, &v, value_token(&v))?.unwrap_or_default()),
            None => Ok(Vec::new()),
        };
    }
    let r = parse_non_null(w, f, full, ctx);
    match r {
        Err(e) if ignore_malformed(f, ctx) && !matches!(e.wrap, super::Wrap::None) && w.p.token() != Token::StartObject => {
            w.p.skip_children();
            w.ignore(full);
            Ok(Vec::new())
        }
        other => other,
    }
}

fn value_token(v: &Value) -> Token {
    match v {
        Value::Obj(_) => Token::StartObject,
        Value::Arr(_) => Token::StartArray,
        Value::Str(_) => Token::String,
        Value::Num(_) => Token::Number,
        Value::Bool(_) => Token::Boolean,
        Value::Null => Token::Null,
    }
}

fn is_numeric(ty: &str) -> bool {
    matches!(ty, "long" | "integer" | "short" | "byte" | "double" | "float" | "half_float" | "scaled_float" | "unsigned_long" | "boolean")
}

/// Parses a value that isn't under the walker's parser (multi-fields, `copy_to`, `null_value`).
pub(crate) fn parse_detached(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str, v: &Value, token: Token) -> EsResult<Option<Vec<FieldValue>>> {
    parse_detached_value(w, f, full, v, token).map_err(|e| e.cause)
}

fn parse_detached_value(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str, v: &Value, _token: Token) -> Result<Option<Vec<FieldValue>>, ValueError> {
    let mut tmp = XParser::started(v, None, Vec::new());
    let mut cx = Cx { p: &mut tmp, ctx: w.ctx, ignored: w.ignored, desc: w.desc };
    parse_value(&mut cx, f, full).map(Some)
}

pub(crate) fn half_float(v: f32) -> f32 {
    // Round to the nearest half-precision value.
    let bits = v.to_bits();
    let sign = (bits >> 16) & 0x8000;
    let exp = ((bits >> 23) & 0xff) as i32 - 127 + 15;
    let mant = bits & 0x7f_ffff;
    let h: u32 = if exp >= 31 {
        sign | 0x7c00 | if v.is_nan() { 0x200 } else { 0 }
    } else if exp <= 0 {
        if exp < -10 {
            sign
        } else {
            let m = (mant | 0x80_0000) >> (1 - exp);
            let round = (m >> 12) & 1;
            sign | ((m >> 13) + round)
        }
    } else {
        let m = mant >> 13;
        let round = (mant >> 12) & 1;
        let h = sign | ((exp as u32) << 10) | m;
        h + round
    };
    // Back to f32.
    let s = (h & 0x8000) << 16;
    let e = (h >> 10) & 0x1f;
    let m = h & 0x3ff;
    let f = if e == 0 {
        if m == 0 { s } else {
            let mut e2 = 127 - 15 + 1;
            let mut m2 = m;
            while m2 & 0x400 == 0 {
                m2 <<= 1;
                e2 -= 1;
            }
            s | ((e2 as u32) << 23) | ((m2 & 0x3ff) << 13)
        }
    } else if e == 31 {
        s | 0x7f80_0000 | (m << 13)
    } else {
        s | ((e + 127 - 15) << 23) | (m << 13)
    };
    f32::from_bits(f)
}

/// `AbstractXContentParser.checkCoerceString`.
fn no_coerce_string(class: &str) -> ValueError {
    EsError::illegal_argument(format!("{class} value passed as String")).into()
}

fn parse_long_like(w: &mut Cx<'_, '_>, f: &FieldMapper, ctx: &MappingCtx) -> Result<i64, ValueError> {
    let co = coerce(f, ctx);
    let p = &w.p;
    match f.ty {
        "long" => {
            if p.token() == Token::String && !co {
                return Err(no_coerce_string("Long"));
            }
            if p.token() == Token::StartObject {
                return Err(EsError::illegal_argument("Cannot parse object as number").into());
            }
            let v = p.long_value()?;
            if !co && let Value::Num(Number::Float(d)) = p.value()
                && d.fract() != 0.0
            {
                return Err(EsError::illegal_argument(format!("{} cannot be converted to Long without data loss", crate::json::write::java_double(*d))).into());
            }
            Ok(v)
        }
        "integer" | "short" | "byte" => {
            let class = match f.ty {
                "integer" => "Integer",
                "short" => "Short",
                _ => "Byte",
            };
            if p.token() == Token::String && !co {
                return Err(no_coerce_string(if f.ty == "byte" { "Integer" } else { class }));
            }
            if p.token() == Token::StartObject {
                return Err(EsError::illegal_argument("Cannot parse object as number").into());
            }
            let v: i64 = if f.ty == "short" {
                match (p.token(), p.value()) {
                    (Token::Number, Value::Num(n)) => {
                        let d = n.as_f64();
                        if !(i16::MIN as f64..=i16::MAX as f64).contains(&d.trunc()) {
                            return Err(p
                                .jackson_error("input_coercion_exception", &format!("Numeric value ({}) out of range of Java short", number_text(p)))
                                .into());
                        }
                        d as i64
                    }
                    (Token::String, _) => {
                        let s = p.text()?;
                        let d = super::super::query::xparser::java_parse_double(&s)?;
                        if !(i16::MIN as f64..=i16::MAX as f64).contains(&d) {
                            return Err(EsError::illegal_argument(format!("Value [{s}] is out of range for a short")).into());
                        }
                        d as i64
                    }
                    _ => p.int_value()? as i64,
                }
            } else {
                p.int_value()? as i64
            };
            if !co && let Value::Num(Number::Float(d)) = p.value()
                && d.fract() != 0.0
            {
                return Err(EsError::illegal_argument(format!("{} cannot be converted to {class} without data loss", crate::json::write::java_double(*d))).into());
            }
            if f.ty == "byte" && !(i8::MIN as i64..=i8::MAX as i64).contains(&v) {
                return Err(EsError::illegal_argument(format!("Value [{v}] is out of range for a byte")).into());
            }
            Ok(v)
        }
        _ => Ok(0),
    }
}

fn finite_check(ty: &str, d: f64, shown: String) -> Result<(), ValueError> {
    if !d.is_finite() {
        return Err(EsError::illegal_argument(format!("[{ty}] supports only finite values, but got [{shown}]")).into());
    }
    Ok(())
}

/// `UnsignedLongFieldMapper.parseUnsignedLong`.
pub(crate) fn parse_unsigned(v: &Value) -> Result<u64, EsError> {
    match v {
        Value::Num(Number::Int(i)) => {
            if *i < 0 {
                return Err(EsError::illegal_argument(format!("Value [{i}] is out of range for unsigned long.")));
            }
            Ok(*i as u64)
        }
        Value::Num(Number::UInt(u)) => Ok(*u),
        Value::Num(Number::Big(b)) => Err(EsError::illegal_argument(format!("Value [{b}] is out of range for unsigned long"))),
        Value::Num(Number::Float(d)) => {
            if d.fract() != 0.0 {
                return Err(EsError::illegal_argument(format!("Value \"{}\" has a decimal part", crate::json::write::java_double(*d))));
            }
            // The exact integer of the double (`BigDecimal.toBigIntegerExact`).
            if *d < 0.0 || *d >= 18_446_744_073_709_551_616.0 {
                return Err(EsError::illegal_argument(format!("Value [{d:.0}] is out of range for unsigned long")));
            }
            Ok(*d as u64)
        }
        other => {
            let s = other.to_es_string();
            if let Ok(u) = s.parse::<u64>()
                && !s.starts_with('+')
            {
                return Ok(u);
            }
            // BigDecimal semantics: an exact integer or a decimal part.
            let t = s.trim();
            let looks_numeric = !t.is_empty() && t == s && t.bytes().all(|b| b.is_ascii_digit() || b"+-.eE".contains(&b));
            if !looks_numeric {
                return Err(EsError::illegal_argument(format!("For input string: \"{s}\"")));
            }
            match t.parse::<f64>() {
                Ok(d) if d.fract() != 0.0 => Err(EsError::illegal_argument(format!("Value \"{s}\" has a decimal part"))),
                Ok(d) if d < 0.0 => Err(EsError::illegal_argument(format!("Value [{}] is out of range for unsigned long", d as i64))),
                Ok(d) if d > u64::MAX as f64 => Err(EsError::illegal_argument(format!("Value [{s}] is out of range for unsigned long"))),
                Ok(d) => Ok(d as u64),
                Err(_) => Err(EsError::illegal_argument(format!("For input string: \"{s}\""))),
            }
        }
    }
}

/// `InetAddresses.forString`.
pub fn parse_ip(s: &str) -> Option<[u8; 16]> {
    let v4 = |s: &str| -> Option<[u8; 4]> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 4 {
            return None;
        }
        let mut out = [0u8; 4];
        for (i, p) in parts.iter().enumerate() {
            if p.is_empty() || p.len() > 3 || !p.bytes().all(|b| b.is_ascii_digit()) || (p.len() > 1 && p.starts_with('0')) {
                return None;
            }
            out[i] = p.parse::<u16>().ok().filter(|v| *v <= 255)? as u8;
        }
        Some(out)
    };
    if let Some(a) = v4(s) {
        let mut out = [0u8; 16];
        out[10] = 0xff;
        out[11] = 0xff;
        out[12..].copy_from_slice(&a);
        return Some(out);
    }
    let addr = s.split('%').next().unwrap_or(s);
    if !addr.contains(':') {
        return None;
    }
    addr.parse::<std::net::Ipv6Addr>().ok().map(|a| a.octets())
}

fn text_of(p: &XParser<'_>) -> Result<String, ValueError> {
    Ok(p.text()?)
}

fn parse_non_null(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str, ctx: &MappingCtx) -> Result<Vec<FieldValue>, ValueError> {
    let token = w.p.token();
    Ok(match f.ty {
        "long" | "integer" | "short" | "byte" => vec![FieldValue::Long(parse_long_like(w, f, ctx)?)],
        "double" | "float" | "half_float" | "scaled_float" => {
            let co = coerce(f, ctx);
            if token == Token::String && !co {
                return Err(no_coerce_string(if f.ty == "double" || f.ty == "scaled_float" { "Double" } else { "Float" }));
            }
            if token == Token::StartObject {
                return Err(EsError::illegal_argument("Cannot parse object as number").into());
            }
            match f.ty {
                "double" => {
                    let d = w.p.double_value()?;
                    finite_check("double", d, crate::json::write::java_double(d))?;
                    vec![FieldValue::Double(d)]
                }
                "scaled_float" => {
                    let d = w.p.double_value()?;
                    finite_check("scaled_float", d, crate::json::write::java_double(d))?;
                    let factor = f.effective("scaling_factor", ctx).as_f64().unwrap_or(1.0);
                    let scaled = (d * factor).round();
                    if scaled > i64::MAX as f64 || scaled < i64::MIN as f64 {
                        return Err(EsError::illegal_argument(format!(
                            "[scaled_float] exceeds the maximum value of long: [{}]",
                            crate::json::write::java_double(d)
                        ))
                        .into());
                    }
                    vec![FieldValue::Long(scaled as i64)]
                }
                ty => {
                    let v = w.p.float_value()?;
                    let stored = if ty == "half_float" { half_float(v) } else { v };
                    if !stored.is_finite() {
                        return Err(EsError::illegal_argument(format!("[{ty}] supports only finite values, but got [{}]", crate::json::write::java_float(v))).into());
                    }
                    vec![FieldValue::Float(stored)]
                }
            }
        }
        "unsigned_long" => vec![FieldValue::UnsignedLong(parse_unsigned(w.p.value())?)],
        "boolean" => {
            if token == Token::String && matches!(w.p.value(), Value::Str(s) if s.is_empty()) {
                return Ok(Vec::new());
            }
            vec![FieldValue::Boolean(w.p.boolean_value()?)]
        }
        "date" | "date_nanos" => {
            let text = match token {
                Token::Number => number_text(w.p),
                _ => w.p.text()?,
            };
            let fmt = f.date_formatter(ctx)?;
            let instant = fmt.parse_detailed(&text, None, false).map_err(|e| e.to_es())?;
            if f.ty == "date_nanos" {
                vec![FieldValue::Date(crate::time::to_epoch_nanos(instant).map_err(EsError::illegal_argument)?)]
            } else {
                vec![FieldValue::Date(instant.millis())]
            }
        }
        "ip" => {
            let s = match token {
                Token::Number => number_text(w.p),
                _ => w.p.text()?,
            };
            match parse_ip(&s) {
                Some(b) => vec![FieldValue::Ip(b)],
                None => return Err(EsError::illegal_argument(format!("'{s}' is not an IP string literal.")).into()),
            }
        }
        "keyword" | "wildcard" | "version" => {
            let s = text_of(w.p)?;
            if f.ty == "keyword"
                && let Some(limit) = param_i64(f, "ignore_above", ctx)
                && s.chars().count() as i64 > limit
            {
                w.ignore(full);
                return Ok(Vec::new());
            }
            vec![FieldValue::Keyword(s)]
        }
        "constant_keyword" => {
            let s = text_of(w.p)?;
            match f.raw("value").and_then(Value::as_str) {
                Some(v) if v != s => {
                    return Err(EsError::illegal_argument(format!(
                        "[constant_keyword] field [{full}] only accepts values that are equal to the value defined in the mappings [{v}], but got [{s}]"
                    ))
                    .into());
                }
                _ => vec![FieldValue::Keyword(s)],
            }
        }
        "text" | "match_only_text" | "search_as_you_type" | "annotated_text" | "pattern_text" => vec![FieldValue::Text(text_of(w.p)?)],
        "token_count" => vec![FieldValue::TokenCount(text_of(w.p)?)],
        "binary" => {
            // Neither stored nor in doc values (the default): the value isn't read at all.
            let stored = f.def().param_index("store").is_some() && f.bool_param("store", ctx);
            let dv = f.def().param_index("doc_values").is_some() && f.bool_param("doc_values", ctx);
            if !stored && !dv {
                w.p.skip_children();
                return Ok(Vec::new());
            }
            use base64::Engine as _;
            let s = text_of(w.p)?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(s.as_bytes())
                .map_err(|_| w.p.jackson_error("json_parse_exception", "Failed to decode VALUE_STRING as base64 (MIME-NO-LINEFEEDS)"))?;
            vec![FieldValue::Binary(bytes)]
        }
        "geo_point" => {
            let pt = crate::query::geo::parse_point(w.p).map_err(ValueError::typed)?;
            let im = ignore_malformed(f, ctx);
            if !im {
                if !(-90.0..=90.0).contains(&pt.lat) {
                    let msg = format!("illegal latitude value [{}] for {full}", crate::json::write::java_double(pt.lat));
                    return Err(if token == Token::StartArray { ValueError::plain(EsError::illegal_argument(msg)) } else { ValueError::typed(EsError::illegal_argument(msg)) });
                }
                if !(-180.0..=180.0).contains(&pt.lon) {
                    let msg = format!("illegal longitude value [{}] for {full}", crate::json::write::java_double(pt.lon));
                    return Err(if token == Token::StartArray { ValueError::plain(EsError::illegal_argument(msg)) } else { ValueError::typed(EsError::illegal_argument(msg)) });
                }
            }
            vec![FieldValue::GeoPoint(pt)]
        }
        "point" => {
            let pt = crate::query::geo::parse_point(w.p).map_err(ValueError::typed)?;
            vec![FieldValue::Point { x: pt.lon, y: pt.lat }]
        }
        "geo_shape" | "shape" => vec![FieldValue::Shape(crate::query::geo::parse_geometry(w.p).map_err(ValueError::typed)?)],
        "integer_range" | "long_range" | "float_range" | "double_range" | "date_range" | "ip_range" => vec![parse_range(w, f, full, ctx)?],
        "dense_vector" => parse_dense_vector(w, f, full, ctx)?,
        "rank_feature" => {
            let v = w.p.float_value()?;
            check_feature(v, full, "_feature").map_err(ValueError::from)?;
            vec![FieldValue::Feature(v)]
        }
        "rank_features" | "sparse_vector" => {
            if token != Token::StartObject {
                return Err(EsError::new(
                    400,
                    "parsing_exception",
                    format!("Failed to parse object: expecting token of type [START_OBJECT] but found [{token}]"),
                )
                .into());
            }
            let mut out = Vec::new();
            let mut key = String::new();
            while let Some(t) = w.p.next_token() {
                match t {
                    Token::EndObject => break,
                    Token::FieldName => key = w.p.current_name().unwrap_or("").to_string(),
                    _ => {
                        let v = w.p.float_value()?;
                        if f.ty == "rank_features" {
                            check_feature(v, &key, full).map_err(ValueError::plain)?;
                        }
                        out.push((key.clone(), v));
                    }
                }
            }
            vec![FieldValue::Features(out)]
        }
        "flattened" => {
            if token != Token::StartObject {
                return Err(EsError::new(
                    400,
                    "parsing_exception",
                    format!("Failed to parse object: expecting token of type [START_OBJECT] but found [{token}]"),
                )
                .into());
            }
            let v = w.p.take_value();
            let mut out = Vec::new();
            flatten(&v, "", &mut out);
            vec![FieldValue::Flattened(out)]
        }
        "completion" => vec![parse_completion(w)?],
        "alias" => return Err(ValueError::none(w.doc_error(format!("Cannot write to a field alias [{full}].")))),
        "percolator" | "semantic_text" | "aggregate_metric_double" | "histogram" | "join" | "counted_keyword" => {
            // Kept in _source; indexing these types comes with their engines.
            w.p.skip_children();
            Vec::new()
        }
        _ => {
            w.p.skip_children();
            Vec::new()
        }
    })
}

fn check_feature(v: f32, feature: &str, field: &str) -> Result<(), EsError> {
    if !(v.is_finite() && v >= f32::MIN_POSITIVE) {
        return Err(EsError::illegal_argument(format!(
            "featureValue must be a positive normal float, got: {} for feature {feature} on field {field} which is less than the minimum positive normal float: 1.1754944E-38",
            crate::json::write::java_float(v)
        )));
    }
    Ok(())
}

fn flatten(v: &Value, prefix: &str, out: &mut Vec<(String, String)>) {
    match v {
        Value::Obj(m) => {
            for (k, x) in m {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                crate::stack::maybe_grow(|| flatten(x, &key, out));
            }
        }
        Value::Arr(a) => {
            for x in a {
                crate::stack::maybe_grow(|| flatten(x, prefix, out));
            }
        }
        Value::Null => {}
        other => out.push((prefix.to_string(), other.to_es_string())),
    }
}

fn parse_completion(w: &mut Cx<'_, '_>) -> Result<FieldValue, ValueError> {
    match w.p.token() {
        Token::String | Token::Number | Token::Boolean => Ok(FieldValue::Completion { inputs: vec![w.p.text()?], weight: None }),
        Token::StartObject => {
            let mut inputs = Vec::new();
            let mut weight = None;
            let mut key = String::new();
            while let Some(t) = w.p.next_token() {
                match t {
                    Token::EndObject => break,
                    Token::FieldName => {
                        key = w.p.current_name().unwrap_or("").to_string();
                        if !matches!(key.as_str(), "input" | "weight" | "contexts") {
                            return Err(ValueError::plain(EsError::illegal_argument(format!(
                                "unknown field name [{key}], must be one of [input, weight, contexts]"
                            ))));
                        }
                    }
                    Token::StartArray if key == "input" => {
                        while let Some(t) = w.p.next_token() {
                            if t == Token::EndArray {
                                break;
                            }
                            inputs.push(w.p.text()?);
                        }
                    }
                    Token::StartObject | Token::StartArray => w.p.skip_children(),
                    _ => match key.as_str() {
                        "input" => inputs.push(w.p.text()?),
                        "weight" => {
                            let v = match w.p.value() {
                                Value::Num(n) => n.as_f64(),
                                Value::Str(s) => s.parse::<f64>().map_err(|_| EsError::number_format(s))?,
                                _ => 0.0,
                            };
                            if !(0.0..=i32::MAX as f64).contains(&v) || v.fract() != 0.0 {
                                let shown = if v.fract() == 0.0 { format!("{}", v as i64) } else { crate::json::write::java_double(v) };
                                return Err(ValueError::plain(EsError::illegal_argument(format!(
                                    "weight must be in the interval [0..2147483647], but was [{shown}]"
                                ))));
                            }
                            weight = Some(v as i64);
                        }
                        _ => {}
                    },
                }
            }
            Ok(FieldValue::Completion { inputs, weight })
        }
        t => Err(EsError::illegal_argument(format!("failed to parse [{t}]: expected text or object, but got {t}")).into()),
    }
}

fn parse_range(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str, ctx: &MappingCtx) -> Result<FieldValue, ValueError> {
    let token = w.p.token();
    if f.ty == "ip_range" && token == Token::String {
        let s = w.p.text()?;
        let (addr, bits) = s.split_once('/').ok_or_else(|| EsError::illegal_argument(format!("Expected [ip/prefix] but was [{s}]")))?;
        parse_ip(addr).ok_or_else(|| EsError::illegal_argument(format!("'{addr}' is not an IP string literal.")))?;
        let _bits: u32 = bits.parse().map_err(|_| EsError::number_format(bits))?;
        return Ok(FieldValue::Range { from: Value::Str(s.clone()), to: Value::Str(s), include_from: true, include_to: true });
    }
    if token != Token::StartObject {
        let (l, c) = w.p.token_location();
        return Err(EsError::document_parsing(format!("[{l}:{c}] error parsing field [{full}], expected an object but got {full}")).into());
    }
    let (mut from, mut to) = (Value::Null, Value::Null);
    let (mut inc_from, mut inc_to) = (true, true);
    let mut key = String::new();
    while let Some(t) = w.p.next_token() {
        match t {
            Token::EndObject => break,
            Token::FieldName => key = w.p.current_name().unwrap_or("").to_string(),
            _ => {
                let v = range_bound(w, f, ctx)?;
                match key.as_str() {
                    "gte" => {
                        from = v;
                        inc_from = true;
                    }
                    "gt" => {
                        from = v;
                        inc_from = false;
                    }
                    "lte" => {
                        to = v;
                        inc_to = true;
                    }
                    "lt" => {
                        to = v;
                        inc_to = false;
                    }
                    other => {
                        let (l, c) = w.p.token_location();
                        return Err(EsError::document_parsing(format!("[{l}:{c}] error parsing field [{full}], with unknown parameter [{other}]")).into());
                    }
                }
            }
        }
    }
    if let (Some(a), Some(b)) = (from.as_f64(), to.as_f64())
        && a > b
    {
        let show = |v: &Value| v.to_es_string();
        return Err(EsError::illegal_argument(format!("min value ({}) is greater than max value ({})", show(&from), show(&to))).into());
    }
    Ok(FieldValue::Range { from, to, include_from: inc_from, include_to: inc_to })
}

/// One bound of a range value, parsed with the range's element type.
fn range_bound(w: &mut Cx<'_, '_>, f: &FieldMapper, ctx: &MappingCtx) -> Result<Value, ValueError> {
    let p = &w.p;
    if p.token() == Token::Null {
        return Ok(Value::Null);
    }
    Ok(match f.ty {
        "integer_range" => Value::from(p.int_value()?),
        "long_range" => Value::from(p.long_value()?),
        "float_range" => crate::query::xparser::float_value(p.float_value()?),
        "double_range" => Value::Num(Number::Float(p.double_value()?)),
        "date_range" => {
            let text = p.text()?;
            if text.starts_with("now") {
                return Err(EsError::parse("could not read the current timestamp")
                    .caused_by(EsError::illegal_argument("now is not used at indexing time"))
                    .into());
            }
            let fmt = f.date_formatter(ctx)?;
            let ms = fmt.parse_millis(&text).map_err(|e| EsError::parse(e.clone()).caused_by(EsError::illegal_argument(e)))?;
            Value::from(ms)
        }
        "ip_range" => {
            let s = p.text()?;
            parse_ip(&s).ok_or_else(|| EsError::illegal_argument(format!("'{s}' is not an IP string literal.")))?;
            Value::Str(s)
        }
        _ => p.value().clone(),
    })
}

fn parse_dense_vector(w: &mut Cx<'_, '_>, f: &FieldMapper, _full: &str, ctx: &MappingCtx) -> Result<Vec<FieldValue>, ValueError> {
    let dims = param_i64(f, "dims", ctx);
    let element = f.effective("element_type", ctx).as_str().unwrap_or("float").to_string();
    let full = f.name.clone();
    let desc = w.desc.to_string();
    let parsing = |msg: String| ValueError::plain(EsError::new(400, "parsing_exception", format!("Failed to parse object: {msg}")));
    let dims_err = |n: usize, d: i64| {
        ValueError::plain(EsError::illegal_argument(format!(
            "The [dense_vector] field [{full}] in doc [{desc}] has a different number of dimensions [{n}] than defined in the mapping [{d}]"
        )))
    };
    match w.p.token() {
        Token::StartArray => {
            let mut floats: Vec<f64> = Vec::new();
            let byte = element == "byte";
            while let Some(t) = w.p.next_token() {
                match t {
                    Token::EndArray => break,
                    Token::Number => {
                        let v = w.p.double_value()?;
                        let i = floats.len();
                        if byte && v.fract() != 0.0 {
                            return Err(ValueError::plain(EsError::illegal_argument(format!(
                                "element_type [byte] vectors only support non-decimal values but found decimal value [{}] at dim [{i}];",
                                crate::json::write::java_double(v)
                            ))));
                        }
                        if byte && !(-128.0..=127.0).contains(&v) {
                            return Err(ValueError::plain(EsError::illegal_argument(format!(
                                "element_type [byte] vectors only support integers between [-128, 127] but found [{}] at dim [{i}];",
                                v as i64
                            ))));
                        }
                        floats.push(v);
                    }
                    t => return Err(parsing(format!("expecting token of type [VALUE_NUMBER] but found [{t}]"))),
                }
            }
            if let Some(d) = dims
                && floats.len() as i64 != d
            {
                return Err(dims_err(floats.len(), d));
            }
            if byte {
                Ok(vec![FieldValue::ByteVector(floats.into_iter().map(|v| v as i8).collect())])
            } else {
                Ok(vec![FieldValue::DenseVector(floats.into_iter().map(|x| x as f32).collect())])
            }
        }
        Token::String => {
            let s = w.p.text()?;
            let d = dims.unwrap_or(0) as usize;
            if element == "byte" && s.len() == d * 2 && s.bytes().all(|b| b.is_ascii_hexdigit()) {
                let bytes = (0..d).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap_or(0) as i8).collect();
                return Ok(vec![FieldValue::ByteVector(bytes)]);
            }
            use base64::Engine as _;
            let lenient = base64::engine::GeneralPurpose::new(
                &base64::alphabet::STANDARD,
                base64::engine::GeneralPurposeConfig::new().with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
            );
            let decoded = lenient.decode(s.as_bytes()).unwrap_or_default();
            if decoded.len() != d * 4 && decoded.len() != d * 2 {
                return Err(parsing(format!(
                    "Base64 decoded vector byte length [{}] does not match the expected length of [{}] or [{}] for dimension count [{d}]",
                    decoded.len(),
                    d * 4,
                    d * 2
                )));
            }
            let floats = decoded.chunks_exact(4).map(|c| f32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect();
            Ok(vec![FieldValue::DenseVector(floats)])
        }
        t => Err(parsing(format!("expecting token of type [START_ARRAY] but found [{t}]"))),
    }
}
