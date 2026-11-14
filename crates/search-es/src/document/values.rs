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
    Completion { inputs: Vec<String>, weight: Option<i64>, contexts: Option<crate::json::Map> },
    /// `token_count`: the text whose tokens are counted at indexing.
    TokenCount(String),
    /// `join`: the relation name and, for a child, its parent's id.
    Join { name: String, parent: Option<String> },
    /// `percolator`: the stored query, as JSON, and where its value ends in the document
    /// (errors checking it against the mapping point there).
    Percolator(String, (usize, usize)),
    /// `aggregate_metric_double`: one value's metrics (`value_count` is an integer).
    AggregateMetric { min: Option<f64>, max: Option<f64>, sum: Option<f64>, value_count: Option<i64> },
    /// `histogram`: its centroids as `EncodedTDigest.encodeCentroids` writes them.
    Histogram(Vec<u8>),
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
        "dense_vector" | "rank_vectors" | "knn_vector" => true,
        // One value per document: a second array element fails (`AggregateMetricDoubleFieldMapper`).
        "aggregate_metric_double" | "histogram" | "exponential_histogram" | "tdigest" => matches!(p.token(), Token::StartArray),
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
    if f.ty == "join" {
        return parse_join(w, f, full).map(|v| vec![v]);
    }
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

/// [`parse_detached`] of a copy of the walker's current number, whose original text is `raw`
/// ([`raw_number_of`]).
pub(crate) fn parse_detached_raw(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str, v: &Value, raw: Option<&str>) -> EsResult<Option<Vec<FieldValue>>> {
    let mut tmp = XParser::started(v, None, Vec::new());
    tmp.raw_number = raw.map(str::to_string);
    let mut cx = Cx { p: &mut tmp, ctx: w.ctx, ignored: w.ignored, desc: w.desc };
    parse_value(&mut cx, f, full).map(Some).map_err(|e| e.cause)
}

fn parse_detached_value(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str, v: &Value, _token: Token) -> Result<Option<Vec<FieldValue>>, ValueError> {
    let mut tmp = XParser::started(v, None, Vec::new());
    let mut cx = Cx { p: &mut tmp, ctx: w.ctx, ignored: w.ignored, desc: w.desc };
    parse_value(&mut cx, f, full).map(Some)
}

/// Parses one value of `f` outside a document walk (synthetic `_source`): `null_value`,
/// coercion and `ignore_malformed` apply; names of ignored values land in `ignored`. `None`
/// when the value doesn't parse at all.
pub(crate) fn parse_standalone(f: &FieldMapper, full: &str, v: &Value, ctx: &MappingCtx, ignored: &mut BTreeSet<String>) -> Option<Vec<FieldValue>> {
    let mut tmp = XParser::started(v, None, Vec::new());
    let mut cx = Cx { p: &mut tmp, ctx, ignored, desc: "" };
    parse_value(&mut cx, f, full).ok()
}

/// A geo point in any of Elasticsearch's forms (`GeoUtils.parseGeoPoint`), or `None`.
pub fn geo_point_of(v: &Value) -> Option<GeoPoint> {
    let mut p = XParser::from_value(v);
    p.next_token();
    crate::query::geo::parse_point(&mut p).ok()
}

/// A `scaled_float`'s scaling factor.
pub(crate) fn scaling_factor(f: &FieldMapper, ctx: &MappingCtx) -> f64 {
    f.effective("scaling_factor", ctx).as_f64().unwrap_or(1.0)
}

/// A finite `scaled_float` value as the long it's indexed as: Java's `Math.round(d * factor)`
/// (ties go up: -22.5 is -22); `None` past a long's range.
pub(crate) fn scale(d: f64, factor: f64) -> Option<i64> {
    let x = d * factor;
    let scaled = if x - x.floor() >= 0.5 { x.floor() + 1.0 } else { x.floor() };
    (scaled <= i64::MAX as f64 && scaled >= i64::MIN as f64).then_some(scaled as i64)
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
            // Exactly, as `BigDecimal` does: `10223372036854775807.23` has a decimal part even
            // though no double can tell.
            match Decimal::parse(t) {
                Some(d) if d.fraction => Err(EsError::illegal_argument(format!("Value \"{s}\" has a decimal part"))),
                Some(d) if d.negative && !d.is_zero() => {
                    let shown = t.parse::<f64>().map(|v| v as i64).unwrap_or(i64::MIN);
                    Err(EsError::illegal_argument(format!("Value [{shown}] is out of range for unsigned long")))
                }
                Some(d) => d.to_u64().ok_or_else(|| EsError::illegal_argument(format!("Value [{s}] is out of range for unsigned long"))),
                None => Err(EsError::illegal_argument(format!("For input string: \"{s}\""))),
            }
        }
    }
}

/// A decimal number's integer part, exactly (Java's `BigDecimal` of a JSON number or numeric
/// string, then `toBigInteger`): the digits without leading zeros, and whether a non-zero
/// fraction was cut off.
#[derive(Debug, PartialEq)]
pub(crate) struct Decimal {
    pub negative: bool,
    /// Integer digits, no leading zeros (empty for zero); `None` when the exponent makes the
    /// number too large to spell out.
    pub digits: Option<String>,
    pub fraction: bool,
}

impl Decimal {
    /// `new BigDecimal(s)`: `[+-]digits[.digits][(e|E)[+-]digits]`, nothing else.
    pub fn parse(s: &str) -> Option<Decimal> {
        let b = s.as_bytes();
        let mut i = 0;
        let negative = match b.first() {
            Some(b'-') => {
                i = 1;
                true
            }
            Some(b'+') => {
                i = 1;
                false
            }
            _ => false,
        };
        let (mut int, mut frac) = (String::new(), String::new());
        while i < b.len() && b[i].is_ascii_digit() {
            int.push(b[i] as char);
            i += 1;
        }
        if i < b.len() && b[i] == b'.' {
            i += 1;
            while i < b.len() && b[i].is_ascii_digit() {
                frac.push(b[i] as char);
                i += 1;
            }
        }
        if int.is_empty() && frac.is_empty() {
            return None;
        }
        let mut exp: i64 = 0;
        if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
            i += 1;
            let neg = match b.get(i) {
                Some(b'-') => {
                    i += 1;
                    true
                }
                Some(b'+') => {
                    i += 1;
                    false
                }
                _ => false,
            };
            let start = i;
            while i < b.len() && b[i].is_ascii_digit() {
                exp = exp.saturating_mul(10).saturating_add(i64::from(b[i] - b'0'));
                i += 1;
            }
            if i == start {
                return None;
            }
            if neg {
                exp = -exp;
            }
        }
        if i != b.len() {
            return None;
        }
        // All digits, with the decimal point `point` digits from the left.
        let all = format!("{int}{frac}");
        let point = int.len() as i64 + exp;
        let (digits, fraction) = if point <= 0 {
            (Some(String::new()), all.bytes().any(|c| c != b'0'))
        } else if point as usize >= all.len() {
            // Integer digits followed by zeros: bounded, as a value past 10^400 only ever
            // reaches an out-of-range message.
            if point > 400 { (None, false) } else { (Some(format!("{all}{}", "0".repeat(point as usize - all.len()))), false) }
        } else {
            let (a, f) = all.split_at(point as usize);
            (Some(a.to_string()), f.bytes().any(|c| c != b'0'))
        };
        let digits = digits.map(|d| d.trim_start_matches('0').to_string());
        Some(Decimal { negative, digits, fraction })
    }

    pub fn is_zero(&self) -> bool {
        self.digits.as_deref() == Some("")
    }

    /// The integer part as a `u64` (sign ignored), `None` past `u64::MAX`.
    pub fn to_u64(&self) -> Option<u64> {
        match self.digits.as_deref() {
            Some("") => Some(0),
            Some(d) => d.parse().ok(),
            None => None,
        }
    }

    /// `BigInteger.toString()` of the integer part.
    pub fn integer_string(&self) -> String {
        match self.digits.as_deref() {
            Some("") => "0".into(),
            Some(d) => format!("{}{d}", if self.negative { "-" } else { "" }),
            None => "Infinity".into(),
        }
    }
}

/// OpenSearch's `unsigned_long` value (`AbstractXContentParser.bigIntegerValue` and
/// `Numbers.toUnsignedLong`): numbers and numeric strings are exact decimals truncated
/// toward zero (`coerce`, the default), or refused when they have a fraction (`coerce: false`).
fn parse_unsigned_os(p: &XParser<'_>, coerce: bool) -> Result<u64, ValueError> {
    let range = |shown: &str| EsError::illegal_argument(format!("Value [{shown}] is out of range for an unsigned long"));
    match p.token() {
        Token::String => {
            let s = p.text()?;
            if !coerce {
                return Err(no_coerce_string("BigInteger"));
            }
            let d = Decimal::parse(&s).ok_or_else(|| EsError::illegal_argument(format!("For input string: \"{s}\"")))?;
            if d.negative && !d.is_zero() || d.to_u64().is_none() {
                return Err(range(&s).into());
            }
            Ok(d.to_u64().unwrap_or(0))
        }
        Token::Number => {
            let raw = p.raw_text();
            let d = Decimal::parse(&raw).ok_or_else(|| EsError::illegal_argument(format!("For input string: \"{raw}\"")))?;
            if d.fraction && !coerce {
                return Err(EsError::illegal_argument(format!("Value [{raw}] has a decimal part")).into());
            }
            if d.negative && !d.is_zero() || d.to_u64().is_none() {
                return Err(range(&d.integer_string()).into());
            }
            Ok(d.to_u64().unwrap_or(0))
        }
        Token::StartObject => Err(EsError::illegal_argument("Cannot parse object as number").into()),
        _ => Ok(parse_unsigned(p.value())?),
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

/// `text()`: a number reads as its text as written (`1.10`, `1E5`, `-0`), as Jackson's does.
fn text_of(p: &XParser<'_>) -> Result<String, ValueError> {
    if p.current_token() == Some(Token::Number) && raw_may_differ(p.value()) {
        return Ok(p.raw_text());
    }
    Ok(p.text()?)
}

/// Whether a number's text as written may differ from its rendering: JSON integers have no
/// leading zeros or `+`, so only zero (`-0`) and non-integers can.
fn raw_may_differ(v: &Value) -> bool {
    matches!(v, Value::Num(Number::Float(_) | Number::Int(0) | Number::Big(_)))
}

/// The original text of the number the walker's parser is on, when it can differ from the
/// value's rendering (for multi-fields and `copy_to` targets, which re-read the token).
pub(crate) fn raw_number_of(p: &XParser<'_>) -> Option<String> {
    (p.current_token() == Some(Token::Number) && raw_may_differ(p.value())).then(|| p.raw_text())
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
                    let Some(scaled) = scale(d, scaling_factor(f, ctx)) else {
                        return Err(EsError::illegal_argument(format!(
                            "[scaled_float] exceeds the maximum value of long: [{}]",
                            crate::json::write::java_double(d)
                        ))
                        .into());
                    };
                    vec![FieldValue::Long(scaled)]
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
        "unsigned_long" if crate::opensearch::active() => vec![FieldValue::UnsignedLong(parse_unsigned_os(w.p, coerce(f, ctx))?)],
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
            // `version` has no `ignore_above`; a wildcard past it is left out like a keyword.
            if f.ty != "version"
                && let Some(limit) = param_i64(f, "ignore_above", ctx)
                && (limit < 0 || super::exceeds_ignore_above(&s, limit as usize))
            {
                w.ignore(full);
                return Ok(Vec::new());
            }
            // `VersionEncoder.encodeVersion` fails on some legal versions (a digit group of 128+
            // digits, a leading number beyond `int`); the value is encoded again when indexed.
            if f.ty == "version"
                && let Err(e) = search_aggs::version_string::encode(&s)
            {
                return Err(EsError::java(400, e.kind, e.reason).into());
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
            let mut pts = if token == Token::StartArray {
                geo_point_array(w, f, full, ctx, crate::query::geo::PointKind::Geo)?
            } else if crate::opensearch::active() {
                vec![crate::query::geo::parse_point(w.p).map_err(ValueError::typed)?]
            } else {
                vec![crate::query::geo::parse_field_point(w.p, crate::query::geo::PointKind::Geo, ignore_z(f, ctx)).map_err(ValueError::typed)?]
            };
            let mut out = Vec::with_capacity(pts.len());
            for pt in &mut pts {
                out.push(check_geo_point(w, f, full, ctx, token, pt)?);
            }
            out
        }
        "point" => {
            let kind = crate::query::geo::PointKind::Cartesian;
            let pts = if token == Token::StartArray { geo_point_array(w, f, full, ctx, kind)? } else { vec![crate::query::geo::parse_field_point(w.p, kind, ignore_z(f, ctx)).map_err(ValueError::typed)?] };
            pts.into_iter().map(|pt| FieldValue::Point { x: pt.lon, y: pt.lat }).collect()
        }
        "geo_shape" | "shape" => vec![FieldValue::Shape(crate::query::geo::parse_geometry(w.p).map_err(ValueError::typed)?)],
        "integer_range" | "long_range" | "float_range" | "double_range" | "date_range" | "ip_range" => vec![parse_range(w, f, full, ctx)?],
        // The text is in `_source`; its embeddings come from `_inference_fields`.
        "semantic_text" => {
            if token == Token::StartObject {
                return Err(EsError::illegal_argument(format!("[semantic_text] field [{full}] does not support object values")).into());
            }
            Vec::new()
        }
        "dense_vector" => parse_dense_vector(w, f, full, ctx)?,
        "knn_vector" => parse_knn_vector(w, f, full, ctx)?,
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
            flatten(&v, "", f.raw("null_value").and_then(Value::as_str), &mut out);
            vec![FieldValue::Flattened(out)]
        }
        "flat_object" => {
            // `FlatObjectFieldMapper.parseObjectPathParts`: an array reaches the mapper one
            // element at a time, and every element must be an object.
            if token != Token::StartObject {
                return Err(w.p.parsing_error(format!("[{full}] unexpected token [{}] in flat_object field value", token.name())).into());
            }
            let mut out = Vec::new();
            let mut path = String::new();
            flat_object_entries(w, &mut path, &mut out)?;
            vec![FieldValue::Flattened(out)]
        }
        "completion" => vec![parse_completion(w)?],
        "alias" => {
            let e = crate::opensearch::alias_write_error(full).unwrap_or_else(|| w.doc_error(format!("Cannot write to a field alias [{full}].")));
            return Err(ValueError::none(e));
        }
        "percolator" => {
            // `PercolatorFieldMapper`: the value is a query, parsed (and later checked against
            // the mapping) when the document is indexed.
            let q = crate::query::parse_inner(w.p).map_err(ValueError::plain)?;
            vec![FieldValue::Percolator(q.to_value().to_json(), w.p.token_location())]
        }
        "aggregate_metric_double" => parse_aggregate_metric(w, f, full)?,
        "histogram" | "exponential_histogram" | "tdigest" => parse_histogram(w, f, full)?,
        // `CountedKeywordFieldMapper.parseCreateField`: strings only (nulls skipped).
        "counted_keyword" => match token {
            Token::String => vec![FieldValue::Keyword(w.p.text()?)],
            t => {
                return Err(EsError::java(400, "illegal_argument_exception", format!("Encountered unexpected token [{}].", t.name())).into());
            }
        },
        _ => {
            w.p.skip_children();
            Vec::new()
        }
    })
}

/// The metrics an `aggregate_metric_double` mapping declares, in `Metric` order.
pub(crate) fn aggregate_metrics(f: &FieldMapper) -> Vec<&'static str> {
    let declared: Vec<String> = match f.raw("metrics") {
        Some(Value::Arr(a)) => a.iter().map(Value::to_es_string).collect(),
        Some(v) => vec![v.to_es_string()],
        None => Vec::new(),
    };
    ["min", "max", "sum", "value_count"].into_iter().filter(|m| declared.iter().any(|d| d == m)).collect()
}

/// `AggregateMetricDoubleFieldMapper.parseCreateField`: an object of exactly the declared
/// metrics, numbers only (`value_count` a non-negative integer, `max` not below `min`), once
/// per document. With `ignore_malformed` a bad value is skipped whole and the field recorded
/// in `_ignored`.
fn parse_aggregate_metric(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str) -> Result<Vec<FieldValue>, ValueError> {
    let lenient = ignore_malformed(f, w.ctx);
    // Containers entered so far, closed again when a malformed value is skipped.
    let mut depth = 0usize;
    match aggregate_metric_value(w, f, full, &mut depth) {
        Ok(v) => Ok(v),
        Err(e) if lenient && !matches!(e.wrap, super::Wrap::None) => {
            if matches!(w.p.token(), Token::StartObject | Token::StartArray) {
                w.p.skip_children();
            }
            while depth > 0 {
                match w.p.next_token() {
                    Some(Token::StartObject | Token::StartArray) => w.p.skip_children(),
                    Some(Token::EndObject | Token::EndArray) => depth -= 1,
                    None => break,
                    _ => {}
                }
            }
            w.ignore(full);
            Ok(Vec::new())
        }
        Err(e) => Err(e),
    }
}

fn aggregate_metric_value(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str, depth: &mut usize) -> Result<Vec<FieldValue>, ValueError> {
    if w.p.token() != Token::StartArray {
        return Ok(vec![aggregate_metric_object(w, f, full, depth)?]);
    }
    *depth += 1;
    let mut out = Vec::new();
    while let Some(t) = w.p.next_token() {
        match t {
            Token::EndArray => {
                *depth -= 1;
                break;
            }
            Token::Null => {}
            _ => {
                let v = aggregate_metric_object(w, f, full, depth)?;
                if !out.is_empty() {
                    return Err(EsError::illegal_argument(format!(
                        "Field [{full}] of type [aggregate_metric_double] does not support indexing multiple values for the same field in the same document"
                    ))
                    .into());
                }
                out.push(v);
            }
        }
    }
    Ok(out)
}

fn aggregate_metric_object(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str, depth: &mut usize) -> Result<FieldValue, ValueError> {
    let expecting = |w: &Cx<'_, '_>, want: &str, found: Token| {
        let (l, c) = w.p.token_location();
        ValueError::from(EsError::parsing(l, c, format!("Failed to parse object: expecting token of type [{want}] but found [{found}]")))
    };
    let token = w.p.token();
    if token != Token::StartObject {
        return Err(expecting(w, "START_OBJECT", token));
    }
    *depth += 1;
    let declared = aggregate_metrics(f);
    let (mut min, mut max, mut sum, mut value_count) = (None, None, None, None);
    let mut parsed = 0usize;
    loop {
        let t = w.p.next_token().unwrap_or(Token::EndObject);
        if t == Token::EndObject {
            *depth -= 1;
            break;
        }
        if t != Token::FieldName {
            return Err(expecting(w, "FIELD_NAME", t));
        }
        let name = w.p.current_name().unwrap_or("").to_string();
        let Some(metric) = ["min", "max", "sum", "value_count"].into_iter().find(|m| *m == name) else {
            return Err(EsError::illegal_argument(format!("No enum constant org.elasticsearch.xpack.aggregatemetric.mapper.AggregateMetricDoubleFieldMapper.Metric.{name}")).into());
        };
        if !declared.contains(&metric) {
            return Err(EsError::illegal_argument(format!("Aggregate metric [{metric}] does not exist in the mapping of field [{full}]")).into());
        }
        let vt = w.p.next_token().unwrap_or(Token::EndObject);
        if vt != Token::Number {
            return Err(expecting(w, "VALUE_NUMBER", vt));
        }
        let sub = |e: EsError| ValueError::from(EsError::illegal_argument(format!("failed to parse [{metric}] sub field: {}", e.reason())).caused_by(e));
        let first = match metric {
            "value_count" => {
                // An integer without coercion: a decimal part is data loss.
                let v = w.p.int_value().map_err(sub)?;
                let d = w.p.double_value().map_err(sub)?;
                if v as f64 != d {
                    return Err(sub(EsError::illegal_argument(format!("{} cannot be converted to Integer without data loss", crate::json::write::java_double(d)))));
                }
                value_count.replace(i64::from(v)).is_none()
            }
            m => {
                let v = w.p.double_value().map_err(sub)?;
                let slot = match m {
                    "min" => &mut min,
                    "max" => &mut max,
                    _ => &mut sum,
                };
                slot.replace(v).is_none()
            }
        };
        if first {
            parsed += 1;
        }
    }
    if let (Some(lo), Some(hi)) = (min, max)
        && hi < lo
    {
        return Err(EsError::illegal_argument(format!("Aggregate metric field [{full}] max value cannot be smaller than min value")).into());
    }
    if value_count.is_some_and(|c| c < 0) {
        return Err(EsError::illegal_argument(format!("Aggregate metric [value_count] of field [{full}] cannot be a negative number")).into());
    }
    if parsed != declared.len() {
        return Err(EsError::illegal_argument(format!("Aggregate metric field [{full}] must contain all metrics [{}]", declared.join(", "))).into());
    }
    Ok(FieldValue::AggregateMetric { min, max, sum, value_count })
}

/// The pre-aggregated histogram types (`histogram`, `exponential_histogram`, `tdigest`): one
/// object per document (`HistogramFieldMapper.parse`, `ExponentialHistogramFieldMapper.parse`,
/// `TDigestFieldMapper.parse`); `ignore_malformed` skips a bad value whole.
fn parse_histogram(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str) -> Result<Vec<FieldValue>, ValueError> {
    let lenient = ignore_malformed(f, w.ctx);
    let object: fn(&mut Cx<'_, '_>, &str, &mut usize) -> Result<FieldValue, ValueError> = match f.ty {
        "exponential_histogram" => exp_histogram_object,
        "tdigest" => tdigest_object,
        _ => histogram_object,
    };
    let ty = f.ty;
    let mut depth = 0usize;
    let r = (|| -> Result<Vec<FieldValue>, ValueError> {
        if w.p.token() != Token::StartArray {
            return Ok(vec![object(w, full, &mut depth)?]);
        }
        depth += 1;
        let mut out = Vec::new();
        while let Some(t) = w.p.next_token() {
            match t {
                Token::EndArray => {
                    depth -= 1;
                    break;
                }
                Token::Null => {}
                _ => {
                    let v = object(w, full, &mut depth)?;
                    if !out.is_empty() {
                        return Err(ValueError::typed(EsError::illegal_argument(format!(
                            "Field [{full}] of type [{ty}] doesn't support indexing multiple values for the same field in the same document"
                        ))));
                    }
                    out.push(v);
                }
            }
        }
        Ok(out)
    })();
    match r {
        Err(e) if lenient && !matches!(e.wrap, super::Wrap::None) => {
            if matches!(w.p.token(), Token::StartObject | Token::StartArray) {
                w.p.skip_children();
            }
            while depth > 0 {
                match w.p.next_token() {
                    Some(Token::StartObject | Token::StartArray) => w.p.skip_children(),
                    Some(Token::EndObject | Token::EndArray) => depth -= 1,
                    None => break,
                    _ => {}
                }
            }
            w.ignore(full);
            Ok(Vec::new())
        }
        other => other,
    }
}

/// `XContentParserUtils.ensureExpectedToken`'s error at the current token.
fn expecting(w: &Cx<'_, '_>, want: &str, found: Token) -> ValueError {
    let (l, c) = w.p.token_location();
    ValueError::typed(EsError::parsing(l, c, format!("Failed to parse object: expecting token of type [{want}] but found [{found}]")))
}

/// `DocumentParsingException(location, "error parsing field [f], " + msg)` at the current token.
fn field_error(w: &Cx<'_, '_>, full: &str, msg: String) -> ValueError {
    ValueError::typed(w.doc_error(format!("error parsing field [{full}], {msg}")))
}

/// The next token, which must be `want` (`ensureExpectedToken`).
fn expect_next(w: &mut Cx<'_, '_>, want: Token) -> Result<(), ValueError> {
    let t = w.p.next_token().unwrap_or(Token::EndObject);
    if t != want {
        return Err(expecting(w, want.name(), t));
    }
    Ok(())
}

fn histogram_object(w: &mut Cx<'_, '_>, full: &str, depth: &mut usize) -> Result<FieldValue, ValueError> {
    let field_error = |w: &Cx<'_, '_>, msg: String| field_error(w, full, msg);
    let token = w.p.token();
    if token != Token::StartObject {
        return Err(expecting(w, "START_OBJECT", token));
    }
    *depth += 1;
    let (mut values, mut counts): (Option<Vec<f64>>, Option<Vec<u64>>) = (None, None);
    loop {
        let t = w.p.next_token().unwrap_or(Token::EndObject);
        if t == Token::EndObject {
            *depth -= 1;
            break;
        }
        if t != Token::FieldName {
            return Err(expecting(w, "FIELD_NAME", t));
        }
        let name = w.p.current_name().unwrap_or("").to_string();
        if name != "values" && name != "counts" {
            return Err(field_error(w, format!("with unknown parameter [{name}]")));
        }
        let at = w.p.next_token().unwrap_or(Token::EndObject);
        if at != Token::StartArray {
            return Err(expecting(w, "START_ARRAY", at));
        }
        *depth += 1;
        let (mut vs, mut cs) = (Vec::new(), Vec::new());
        let mut previous = -f64::MAX;
        loop {
            let et = w.p.next_token().unwrap_or(Token::EndArray);
            if et == Token::EndArray {
                *depth -= 1;
                break;
            }
            if et != Token::Number {
                return Err(expecting(w, "VALUE_NUMBER", et));
            }
            if name == "values" {
                let v = w.p.double_value().map_err(ValueError::typed)?;
                if v < previous {
                    let (jv, jp) = (crate::json::write::java_double(v), crate::json::write::java_double(previous));
                    return Err(field_error(w, format!("[values] values must be in increasing order, got [{jv}] but previous value was [{jp}]")));
                }
                previous = v;
                vs.push(v);
            } else {
                let c = w.p.long_value().map_err(ValueError::typed)?;
                if c < 0 {
                    return Err(field_error(w, format!("[counts] elements must be >= 0 but got {c}")));
                }
                cs.push(c as u64);
            }
        }
        if name == "values" {
            values = Some(vs);
        } else {
            counts = Some(cs);
        }
    }
    let Some(values) = values else { return Err(field_error(w, "expected field called [values]".into())) };
    let Some(counts) = counts else { return Err(field_error(w, "expected field called [counts]".into())) };
    if values.len() != counts.len() {
        return Err(field_error(w, format!("expected same length from [values] and [counts] but got [{} != {}]", values.len(), counts.len())));
    }
    Ok(FieldValue::Histogram(search_aggs::histogram_field::encode_histogram(&values, &counts)))
}

/// `ExponentialHistogramParser.parse`: `{scale, sum?, min?, max?, zero?: {threshold?, count?},
/// positive?/negative?: {indices, counts}}`, validated as Elasticsearch does: per value while
/// reading, per bucket list at its end, the whole histogram at the end of the object.
fn exp_histogram_object(w: &mut Cx<'_, '_>, full: &str, depth: &mut usize) -> Result<FieldValue, ValueError> {
    use search_aggs::exp_histogram::{ExpHistogramValue, MAX_INDEX, MAX_SCALE, MIN_INDEX, MIN_SCALE};
    let token = w.p.token();
    if token != Token::StartObject {
        return Err(expecting(w, "START_OBJECT", token));
    }
    *depth += 1;
    let mut v = ExpHistogramValue::default();
    let mut scale = None;
    loop {
        let t = w.p.next_token().unwrap_or(Token::EndObject);
        if t == Token::EndObject {
            *depth -= 1;
            break;
        }
        if t != Token::FieldName {
            return Err(expecting(w, "FIELD_NAME", t));
        }
        let name = w.p.current_name().unwrap_or("").to_string();
        match name.as_str() {
            "scale" => {
                expect_next(w, Token::Number)?;
                let s = w.p.long_value().map_err(ValueError::typed)?;
                if s < MIN_SCALE as i64 || s > MAX_SCALE as i64 {
                    return Err(field_error(w, full, format!("scale field must be in range [{MIN_SCALE}, {MAX_SCALE}] but got {s}")));
                }
                scale = Some(s as i32);
            }
            "sum" | "min" | "max" => {
                let t = w.p.next_token().unwrap_or(Token::EndObject);
                if t != Token::Number {
                    return Err(field_error(w, full, format!("expected a number but got {t}")));
                }
                let d = w.p.double_value().map_err(ValueError::typed)?;
                match name.as_str() {
                    "sum" => v.sum = Some(d),
                    "min" => v.min = Some(d),
                    _ => v.max = Some(d),
                }
            }
            "zero" => {
                expect_next(w, Token::StartObject)?;
                *depth += 1;
                loop {
                    let t = w.p.next_token().unwrap_or(Token::EndObject);
                    if t == Token::EndObject {
                        *depth -= 1;
                        break;
                    }
                    if t != Token::FieldName {
                        return Err(expecting(w, "FIELD_NAME", t));
                    }
                    let key = w.p.current_name().unwrap_or("").to_string();
                    match key.as_str() {
                        "threshold" => {
                            expect_next(w, Token::Number)?;
                            let d = w.p.double_value().map_err(ValueError::typed)?;
                            if !(d >= 0.0 && d.is_finite()) {
                                let d = crate::json::write::java_double(d);
                                return Err(field_error(w, full, format!("zero.threshold field must be a non-negative, finite number but got {d}")));
                            }
                            v.zero_threshold = d;
                        }
                        "count" => {
                            expect_next(w, Token::Number)?;
                            let c = w.p.long_value().map_err(ValueError::typed)?;
                            if c < 0 {
                                return Err(field_error(w, full, format!("zero.count field must be a non-negative number but got {c}")));
                            }
                            v.zero_count = c as u64;
                        }
                        _ => return Err(field_error(w, full, format!("with unknown parameter for zero sub-object [{key}]"))),
                    }
                }
            }
            "positive" | "negative" => {
                expect_next(w, Token::StartObject)?;
                *depth += 1;
                let (mut indices, mut counts) = (Vec::new(), Vec::new());
                loop {
                    let t = w.p.next_token().unwrap_or(Token::EndObject);
                    if t == Token::EndObject {
                        *depth -= 1;
                        break;
                    }
                    if t != Token::FieldName {
                        return Err(expecting(w, "FIELD_NAME", t));
                    }
                    let key = w.p.current_name().unwrap_or("").to_string();
                    if key != "indices" && key != "counts" {
                        return Err(field_error(w, full, format!("with unknown parameter for {name} sub-object [{key}]")));
                    }
                    expect_next(w, Token::StartArray)?;
                    *depth += 1;
                    loop {
                        let t = w.p.next_token().unwrap_or(Token::EndArray);
                        if t == Token::EndArray {
                            *depth -= 1;
                            break;
                        }
                        if t != Token::Number {
                            return Err(expecting(w, "VALUE_NUMBER", t));
                        }
                        let n = w.p.long_value().map_err(ValueError::typed)?;
                        if key == "indices" {
                            if !(MIN_INDEX..=MAX_INDEX).contains(&n) {
                                return Err(field_error(w, full, format!("{name}.indices values must all be in range [{MIN_INDEX}, {MAX_INDEX}] but got {n}")));
                            }
                            indices.push(n);
                        } else {
                            if n <= 0 {
                                return Err(field_error(w, full, format!("{name}.counts values must all be greater than zero but got {n}")));
                            }
                            counts.push(n as u64);
                        }
                    }
                }
                if indices.len() != counts.len() {
                    let (a, b) = (indices.len(), counts.len());
                    return Err(field_error(w, full, format!("expected same length from [{name}.indices] and [{name}.counts] but got [{a} != {b}]")));
                }
                let mut buckets: Vec<(i64, u64)> = indices.into_iter().zip(counts).collect();
                buckets.sort_by_key(|b| b.0);
                if let Some(d) = buckets.windows(2).find(|p| p[0].0 == p[1].0) {
                    let i = d[0].0;
                    return Err(field_error(w, full, format!("expected entries of [{name}.indices] to be unique, but got {i} multiple times")));
                }
                if name == "positive" {
                    v.positive = buckets;
                } else {
                    v.negative = buckets;
                }
            }
            _ => return Err(field_error(w, full, format!("with unknown parameter [{name}]"))),
        }
    }
    let Some(scale) = scale else { return Err(field_error(w, full, "expected field called [scale]".into())) };
    v.scale = scale;
    let total = v.negative.iter().chain(&v.positive).try_fold(v.zero_count, |t, b| t.checked_add(b.1)).filter(|&t| t <= i64::MAX as u64);
    let Some(total) = total else {
        return Err(ValueError::typed(EsError::illegal_argument(format!(
            "Field [{full}] has a total value count exceeding the allowed maximum value of {}",
            i64::MAX
        ))));
    };
    if total == 0 {
        if let Some(s) = v.sum.filter(|&s| s != 0.0) {
            return Err(field_error(w, full, format!("sum field must be zero if the histogram is empty, but got {}", crate::json::write::java_double(s))));
        }
        for (k, m) in [("min", v.min), ("max", v.max)] {
            if let Some(m) = m {
                return Err(field_error(w, full, format!("{k} field must be null if the histogram is empty, but got {}", crate::json::write::java_double(m))));
            }
        }
    }
    Ok(FieldValue::Histogram(search_aggs::exp_histogram::encode(&v)))
}

/// `TDigestParser.parse`: `{"centroids": [...], "counts": [...]}` (centroids ascending, counts
/// non-negative, same length) with optional `sum`, `min` and `max`; doc value as
/// [`tdigest_doc_value`].
fn tdigest_object(w: &mut Cx<'_, '_>, full: &str, depth: &mut usize) -> Result<FieldValue, ValueError> {
    let token = w.p.token();
    if token != Token::StartObject {
        return Err(expecting(w, "START_OBJECT", token));
    }
    *depth += 1;
    let (mut centroids, mut counts): (Option<Vec<f64>>, Option<Vec<u64>>) = (None, None);
    let (mut sum, mut min, mut max) = (None, None, None);
    loop {
        let t = w.p.next_token().unwrap_or(Token::EndObject);
        if t == Token::EndObject {
            *depth -= 1;
            break;
        }
        if t != Token::FieldName {
            return Err(expecting(w, "FIELD_NAME", t));
        }
        let name = w.p.current_name().unwrap_or("").to_string();
        match name.as_str() {
            "sum" | "min" | "max" => {
                expect_next(w, Token::Number)?;
                let d = Some(w.p.double_value().map_err(ValueError::typed)?);
                match name.as_str() {
                    "sum" => sum = d,
                    "min" => min = d,
                    _ => max = d,
                }
            }
            "centroids" | "counts" => {
                expect_next(w, Token::StartArray)?;
                *depth += 1;
                let (mut vs, mut cs) = (Vec::new(), Vec::new());
                let mut previous = -f64::MAX;
                loop {
                    let t = w.p.next_token().unwrap_or(Token::EndArray);
                    if t == Token::EndArray {
                        *depth -= 1;
                        break;
                    }
                    if t != Token::Number {
                        return Err(expecting(w, "VALUE_NUMBER", t));
                    }
                    if name == "centroids" {
                        let v = w.p.double_value().map_err(ValueError::typed)?;
                        if v < previous {
                            let (jv, jp) = (crate::json::write::java_double(v), crate::json::write::java_double(previous));
                            return Err(field_error(w, full, format!("[centroids] centroids must be in increasing order, got [{jv}] but previous value was [{jp}]")));
                        }
                        previous = v;
                        vs.push(v);
                    } else {
                        let c = w.p.long_value().map_err(ValueError::typed)?;
                        if c < 0 {
                            return Err(field_error(w, full, format!("[counts] elements must be >= 0 but got {c}")));
                        }
                        cs.push(c as u64);
                    }
                }
                if name == "centroids" {
                    centroids = Some(vs);
                } else {
                    counts = Some(cs);
                }
            }
            _ => return Err(field_error(w, full, format!("with unknown parameter [{name}]"))),
        }
    }
    let Some(centroids) = centroids else { return Err(field_error(w, full, "expected field called [centroids]".into())) };
    let Some(counts) = counts else { return Err(field_error(w, full, "expected field called [counts]".into())) };
    if centroids.len() != counts.len() {
        let (a, b) = (centroids.len(), counts.len());
        return Err(field_error(w, full, format!("expected same length from [centroids] and [counts] but got [{a} != {b}]")));
    }
    Ok(FieldValue::Histogram(tdigest_doc_value(&centroids, &counts, sum, min, max)))
}

/// A `tdigest` field's binary doc value: a flag byte (1 when the digest has centroids), then
/// `min`, `max` and `sum` as little-endian doubles (given, or the first and last centroid and
/// the weighted sum, as `TDigestFieldMapper` computes them), then the centroids with a
/// count in `EncodedTDigest`'s encoding ([`search_aggs::histogram_field::encode_histogram`]).
pub(crate) fn tdigest_doc_value(centroids: &[f64], counts: &[u64], sum: Option<f64>, min: Option<f64>, max: Option<f64>) -> Vec<u8> {
    let encoded = search_aggs::histogram_field::encode_histogram(centroids, counts);
    let mut out = Vec::with_capacity(25 + encoded.len());
    match (centroids.first(), centroids.last()) {
        (Some(&first), Some(&last)) => {
            let total = sum.unwrap_or_else(|| centroids.iter().zip(counts).fold(0.0, |s, (v, &c)| s + v * c as f64));
            out.push(1);
            for d in [min.unwrap_or(first), max.unwrap_or(last), total] {
                out.extend_from_slice(&d.to_le_bytes());
            }
        }
        _ => out.push(0),
    }
    out.extend_from_slice(&encoded);
    out
}

/// A `tdigest` doc value's summary (`min`, `max`, `sum`; `None` for an empty digest) and its
/// centroid bytes.
pub(crate) fn tdigest_parts(b: &[u8]) -> Option<(Option<[f64; 3]>, &[u8])> {
    match b.split_first()? {
        (0, rest) => Some((None, rest)),
        (1, rest) if rest.len() >= 24 => {
            let d = |i: usize| f64::from_le_bytes(rest[i * 8..i * 8 + 8].try_into().unwrap_or_default());
            Some((Some([d(0), d(1), d(2)]), &rest[24..]))
        }
        _ => None,
    }
}

/// `ParentJoinFieldMapper.parse`: a relation name, or `{"name": …, "parent": …}`.
fn parse_join(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str) -> Result<FieldValue, ValueError> {
    let (mut name, mut parent) = (None, None);
    match w.p.token() {
        Token::StartObject => {
            let mut key = String::new();
            while let Some(t) = w.p.next_token() {
                match t {
                    Token::EndObject => break,
                    Token::FieldName => key = w.p.current_name().unwrap_or("").to_string(),
                    Token::String if key == "name" => name = Some(w.p.text()?),
                    Token::String if key == "parent" => parent = Some(w.p.text()?),
                    Token::Number if key == "parent" => parent = Some(w.p.raw_text()),
                    Token::String | Token::Number => {
                        return Err(ValueError::plain(EsError::illegal_argument(format!("unknown field name [{key}] in join field [{full}]"))));
                    }
                    Token::StartObject | Token::StartArray => w.p.skip_children(),
                    _ => {}
                }
            }
        }
        Token::String => name = Some(w.p.text()?),
        t => {
            return Err(ValueError::plain(EsError::illegal_state(format!(
                "[{full}] expected START_OBJECT or VALUE_STRING but was: {}",
                t.name()
            ))));
        }
    }
    let Some(name) = name else {
        return Err(ValueError::plain(EsError::illegal_argument(format!("null join name in field [{full}]"))));
    };
    let joiner = crate::join::Joiner::from_mapper(f);
    if !joiner.known(&name) {
        return Err(ValueError::plain(EsError::illegal_argument(format!("unknown join name [{name}] for field [{full}]"))));
    }
    if joiner.is_child(&name) {
        if parent.is_none() {
            return Err(ValueError::plain(EsError::illegal_argument(format!("[parent] is missing for join field [{full}]"))));
        }
        if w.ctx.join_routing == Some(false) {
            return Err(ValueError::plain(EsError::illegal_argument(format!("[routing] is missing for join field [{full}]"))));
        }
    } else {
        parent = None;
    }
    Ok(FieldValue::Join { name, parent })
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

/// The `(key, text)` leaves of a `flattened` value (`FlattenedFieldParser`): a `null` leaf
/// takes the field's `null_value`, if it has one.
pub(crate) fn flatten(v: &Value, prefix: &str, null_value: Option<&str>, out: &mut Vec<(String, String)>) {
    match v {
        Value::Obj(m) => {
            for (k, x) in m {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                crate::stack::maybe_grow(|| flatten(x, &key, null_value, out));
            }
        }
        Value::Arr(a) => {
            for x in a {
                crate::stack::maybe_grow(|| flatten(x, prefix, null_value, out));
            }
        }
        Value::Null => {
            if let Some(n) = null_value {
                out.push((prefix.to_string(), n.to_string()));
            }
        }
        other => out.push((prefix.to_string(), other.to_es_string())),
    }
}

/// The leaves of the object the parser is on (OpenSearch's `flat_object`), as `(path, text)`
/// pairs under `path`. Numbers keep their text as written (Jackson's `textOrNull`: `1.50` stays
/// `1.50`), `null`s are skipped, arrays at any depth contribute each element.
fn flat_object_entries(w: &mut Cx<'_, '_>, path: &mut String, out: &mut Vec<(String, String)>) -> Result<(), ValueError> {
    while let Some(t) = w.p.next_token() {
        match t {
            Token::EndObject => return Ok(()),
            Token::FieldName => {
                let name = w.p.current_name().unwrap_or("");
                let len = path.len();
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(name);
                let vt = w.p.next_token().unwrap_or(Token::Null);
                crate::stack::maybe_grow(|| flat_object_value(w, vt, path, out))?;
                path.truncate(len);
            }
            _ => {}
        }
    }
    Ok(())
}

fn flat_object_value(w: &mut Cx<'_, '_>, t: Token, path: &mut String, out: &mut Vec<(String, String)>) -> Result<(), ValueError> {
    match t {
        Token::StartObject => flat_object_entries(w, path, out),
        Token::StartArray => {
            while let Some(t) = w.p.next_token() {
                if t == Token::EndArray {
                    break;
                }
                crate::stack::maybe_grow(|| flat_object_value(w, t, path, out))?;
            }
            Ok(())
        }
        Token::Null => Ok(()),
        Token::Number => {
            out.push((path.clone(), w.p.raw_text()));
            Ok(())
        }
        _ => {
            out.push((path.clone(), w.p.text()?));
            Ok(())
        }
    }
}

fn parse_completion(w: &mut Cx<'_, '_>) -> Result<FieldValue, ValueError> {
    match w.p.token() {
        Token::String | Token::Number | Token::Boolean => Ok(FieldValue::Completion { inputs: vec![w.p.text()?], weight: None, contexts: None }),
        Token::StartObject => {
            let mut inputs = Vec::new();
            let mut weight = None;
            let mut contexts = None;
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
                    Token::StartObject if key == "contexts" => contexts = Some(w.p.map()?),
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
            Ok(FieldValue::Completion { inputs, weight, contexts })
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
                let v = range_bound(w, f, ctx, matches!(key.as_str(), "gt" | "lte"))?;
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
/// Dates round up for `gt` and `lte` (`RangeType.DATE.parseFrom/parseTo`), so date math such
/// as `||/M` covers whole units.
fn range_bound(w: &mut Cx<'_, '_>, f: &FieldMapper, ctx: &MappingCtx, round_up: bool) -> Result<Value, ValueError> {
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
            let ms = crate::time::parse_date_math(&text, 0, round_up, None, &fmt).map_err(|e| e.to_es())?.millis();
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

/// OpenSearch's `knn_vector` (`KNNVectorFieldMapper.parseCreateField`): an array of numbers
/// (numeric strings too), or one number. Element errors point at the element; dimension
/// errors come after the array (Jackson's preview of its end is `null`).
fn parse_knn_vector(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str, ctx: &MappingCtx) -> Result<Vec<FieldValue>, ValueError> {
    use crate::opensearch::knn;
    let r = knn::resolve(f, ctx);
    let mut values: Vec<f64> = Vec::new();
    let element = |w: &mut Cx<'_, '_>, values: &mut Vec<f64>| -> Result<(), ValueError> {
        let v = match w.p.token() {
            Token::Number => w.p.double_value()?,
            Token::String => {
                let s = w.p.text()?;
                crate::units::parse_java_double(&s).ok_or_else(|| ValueError::from(EsError::number_format(&s)))?
            }
            t => {
                return Err(ValueError::from(EsError::new(
                    400,
                    "input_coercion_exception",
                    format!("Current token ({}) not numeric, cannot use numeric value accessors", t.name()),
                )));
            }
        };
        knn::check_doc_vector(&r, std::slice::from_ref(&v)).or_else(|(at, e)| if at.is_some() { Err(e) } else { Ok(knn::DocVector::Floats(Vec::new())) }).map_err(ValueError::from)?;
        values.push(v);
        Ok(())
    };
    match w.p.token() {
        Token::StartArray => {
            while let Some(t) = w.p.next_token() {
                if t == Token::EndArray {
                    break;
                }
                element(w, &mut values)?;
            }
        }
        Token::StartObject => w.p.skip_children(),
        _ => element(w, &mut values)?,
    }
    match knn::check_doc_vector(&r, &values) {
        Ok(knn::DocVector::Floats(v)) => Ok(vec![FieldValue::DenseVector(v)]),
        Ok(knn::DocVector::Bytes(b)) => Ok(vec![FieldValue::ByteVector(b)]),
        // Whole-vector errors: the plugin has read past the value, so the preview is `null`.
        Err((_, e)) => Err(ValueError::none(
            w.doc_error(format!("failed to parse field [{}] of type [knn_vector] in {}. Preview of field's value: 'null'", full, w.desc)).caused_by(e),
        )),
    }
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
            let bit = element == "bit";
            let byte = element == "byte" || bit;
            while let Some(t) = w.p.next_token() {
                match t {
                    Token::EndArray => break,
                    Token::Number => {
                        let v = w.p.double_value()?;
                        let i = floats.len();
                        if byte && v.fract() != 0.0 {
                            return Err(ValueError::plain(EsError::illegal_argument(format!(
                                "element_type [{element}] vectors only support non-decimal values but found decimal value [{}] at dim [{i}];",
                                crate::json::write::java_double(v)
                            ))));
                        }
                        if byte && !(-128.0..=127.0).contains(&v) {
                            return Err(ValueError::plain(EsError::illegal_argument(format!(
                                "element_type [{element}] vectors only support integers between [-128, 127] but found [{}] at dim [{i}];",
                                v as i64
                            ))));
                        }
                        if let Some(d) = dims
                            && ((floats.len() + 1) * if bit { 8 } else { 1 }) as i64 > d
                        {
                            return Err(ValueError::plain(EsError::illegal_argument(format!(
                                "The [dense_vector] field [{full}] in doc [{desc}] has more dimensions than defined in the mapping [{d}]"
                            ))));
                        }
                        floats.push(v);
                    }
                    t => return Err(parsing(format!("expecting token of type [VALUE_NUMBER] but found [{t}]"))),
                }
            }
            // A bit vector's dims count bits: each array element holds eight.
            let n = if bit { floats.len() * 8 } else { floats.len() };
            if let Some(d) = dims
                && n as i64 != d
            {
                return Err(dims_err(n, d));
            }
            if byte {
                Ok(vec![FieldValue::ByteVector(floats.into_iter().map(|v| v as i8).collect())])
            } else {
                Ok(vec![FieldValue::DenseVector(floats.into_iter().map(|x| x as f32).collect())])
            }
        }
        Token::String => {
            let s = w.p.text()?;
            let bit = element == "bit";
            let byte = element == "byte" || bit;
            // Hex strings (byte and bit vectors): two digits per byte; without `dims` the
            // length decides (dynamic dims).
            let hex = s.len() % 2 == 0 && !s.is_empty() && s.bytes().all(|b| b.is_ascii_hexdigit());
            if byte && hex && dims.is_none_or(|d| s.len() as i64 / 2 * if bit { 8 } else { 1 } == d) {
                let n = s.len() / 2;
                let bytes = (0..n).map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).unwrap_or(0) as i8).collect();
                return Ok(vec![FieldValue::ByteVector(bytes)]);
            }
            let d = dims.unwrap_or(0) as usize;
            if byte {
                use base64::Engine as _;
                let lenient = base64::engine::GeneralPurpose::new(
                    &base64::alphabet::STANDARD,
                    base64::engine::GeneralPurposeConfig::new().with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
                );
                let decoded = lenient.decode(s.as_bytes()).unwrap_or_default();
                let want = if bit { d / 8 } else { d };
                if dims.is_some() && decoded.len() != want {
                    if hex {
                        return Err(dims_err(s.len() / 2 * if bit { 8 } else { 1 }, d as i64));
                    }
                    return Err(parsing(format!("Base64 decoded vector byte length [{}] does not match the expected length of [{want}] for dimension count [{d}]", decoded.len())));
                }
                return Ok(vec![FieldValue::ByteVector(decoded.into_iter().map(|b| b as i8).collect())]);
            }
            use base64::Engine as _;
            let lenient = base64::engine::GeneralPurpose::new(
                &base64::alphabet::STANDARD,
                base64::engine::GeneralPurposeConfig::new().with_decode_padding_mode(base64::engine::DecodePaddingMode::Indifferent),
            );
            let decoded = lenient.decode(s.as_bytes()).unwrap_or_default();
            let d = if dims.is_none() { decoded.len() / 4 } else { d };
            if decoded.len() != d * 4 && decoded.len() != d * 2 {
                return Err(parsing(format!(
                    "Base64 decoded vector byte length [{}] does not match the expected length of [{}] or [{}] for dimension count [{d}]",
                    decoded.len(),
                    d * 4,
                    d * 2
                )));
            }
            // Two bytes per dimension: big-endian bfloat16 (the high half of a float32).
            let floats = if decoded.len() == d * 2 && d * 2 != d * 4 {
                decoded.as_chunks::<2>().0.iter().map(|c| f32::from_bits((u16::from_be_bytes(*c) as u32) << 16)).collect()
            } else {
                decoded.as_chunks::<4>().0.iter().map(|c| f32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect()
            };
            Ok(vec![FieldValue::DenseVector(floats)])
        }
        t => Err(parsing(format!("expecting token of type [START_ARRAY] but found [{t}]"))),
    }
}

/// `GeoUtils.centeredModulus`.
fn centered_modulus(dividend: f64, divisor: f64) -> f64 {
    let mut r = dividend % divisor;
    if r <= 0.0 {
        r += divisor;
    }
    if r > divisor / 2.0 {
        r -= divisor;
    }
    r
}

/// `GeoUtils.normalizePoint(point, true, true)`: latitude folded over the poles (shifting
/// the longitude half a turn), then longitude wrapped into (-180, 180].
pub fn normalize_point(lat: f64, lon: f64) -> (f64, f64) {
    let mut lat = centered_modulus(lat, 360.0);
    let mut lon = lon;
    let mut shift = true;
    if lat < -90.0 {
        lat = -180.0 - lat;
    } else if lat > 90.0 {
        lat = 180.0 - lat;
    } else {
        shift = false;
    }
    if shift {
        lon += 180.0;
    }
    (lat, centered_modulus(lon, 360.0))
}


/// A `geo_point` given as a coordinate array (`AbstractPointGeometryFieldMapper.PointParser`):
/// `[lon, lat]` or `[lon, lat, z]` read with Jackson's numeric accessors (strings coerce; other
/// tokens fail as Jackson does), then any further elements as more points. Accessor and
/// dimension failures happen outside the mapper's `ignore_malformed` guard.
/// A point mapping's `ignore_z_value` (true unless set to false).
fn ignore_z(f: &FieldMapper, ctx: &MappingCtx) -> bool {
    f.effective("ignore_z_value", ctx).as_bool() != Some(false)
}

fn geo_point_array(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str, ctx: &MappingCtx, kind: crate::query::geo::PointKind) -> Result<Vec<crate::query::geo::GeoPoint>, ValueError> {
    use crate::query::geo::GeoPoint;
    // Jackson's and XContent's exceptions aren't Elasticsearch exceptions: the document
    // parsing exception is the root cause.
    fn plain_java(e: &EsError) -> EsError {
        let out = EsError::java(e.status, e.kind.to_string(), e.reason().to_string());
        match &e.caused_by {
            Some(c) => out.caused_by(plain_java(c)),
            None => out,
        }
    }
    let fatal = |w: &mut Cx<'_, '_>, e: EsError| {
        let msg = e.reason().to_string();
        let e = if matches!(&*e.kind, "x_content_parse_exception" | "json_parse_exception") { plain_java(&e) } else { e };
        ValueError::none(w.doc_error(format!("failed to parse: {msg}")).caused_by(e))
    };
    w.p.next_token();
    let lon = match w.p.double_value() {
        Ok(v) => v,
        Err(e) => return Err(fatal(w, e)),
    };
    w.p.next_token();
    let lat = match w.p.double_value() {
        Ok(v) => v,
        Err(e) => return Err(fatal(w, e)),
    };
    let mut out = vec![GeoPoint { lat, lon }];
    match w.p.next_token() {
        Some(Token::EndArray) => return Ok(out),
        Some(Token::Number) => {
            let z = w.p.double_value().map_err(ValueError::typed)?;
            if !ignore_z(f, ctx) {
                let e = EsError::parse(format!("Exception parsing coordinates: found Z value [{}] but [ignore_z_value] parameter is [false]", crate::json::write::java_double(z)));
                return Err(fatal(w, e));
            }
        }
        _ => return Err(fatal(w, EsError::parse("field type does not accept > 3 dimensions"))),
    }
    while let Some(t) = w.p.next_token() {
        if t == Token::EndArray {
            break;
        }
        let next = match kind {
            crate::query::geo::PointKind::Geo if crate::opensearch::active() => crate::query::geo::parse_point(w.p),
            _ => crate::query::geo::parse_field_point(w.p, kind, ignore_z(f, ctx)),
        };
        match next {
            Ok(pt) => out.push(pt),
            // Points before the malformed one were already indexed.
            Err(_) if ignore_malformed(f, ctx) => {
                w.p.skip_children();
                while let Some(t) = w.p.next_token() {
                    match t {
                        Token::EndArray => break,
                        Token::StartObject | Token::StartArray => w.p.skip_children(),
                        _ => {}
                    }
                }
                w.ignore(full);
                return Ok(out);
            }
            Err(e) => return Err(ValueError::typed(e)),
        }
    }
    Ok(out)
}

/// Range checks of a parsed point; `ignore_malformed` wraps out-of-range points instead of
/// rejecting them (`GeoUtils.normalizePoint`).
fn check_geo_point(w: &mut Cx<'_, '_>, f: &FieldMapper, full: &str, ctx: &MappingCtx, token: Token, pt: &mut crate::query::geo::GeoPoint) -> Result<FieldValue, ValueError> {
    let _ = w;
    if ignore_malformed(f, ctx) {
        if !(-90.0..=90.0).contains(&pt.lat) || !(-180.0..=180.0).contains(&pt.lon) {
            (pt.lat, pt.lon) = normalize_point(pt.lat, pt.lon);
        }
    } else {
        let wrap = |msg: String| if token == Token::StartArray { ValueError::plain(EsError::illegal_argument(msg)) } else { ValueError::typed(EsError::illegal_argument(msg)) };
        if !(-90.0..=90.0).contains(&pt.lat) {
            return Err(wrap(format!("illegal latitude value [{}] for {full}", crate::json::write::java_double(pt.lat))));
        }
        if !(-180.0..=180.0).contains(&pt.lon) {
            return Err(wrap(format!("illegal longitude value [{}] for {full}", crate::json::write::java_double(pt.lon))));
        }
    }
    Ok(FieldValue::GeoPoint(*pt))
}

#[cfg(test)]
mod geo_normalize_tests {
    use super::*;

    #[test]
    fn wraps_like_geo_utils() {
        assert_eq!(normalize_point(20.12, 182.22), (20.12, 182.22 - 360.0));
        assert_eq!(normalize_point(95.0, 10.0), (85.0, -170.0));
        assert_eq!(normalize_point(-95.0, 10.0), (-85.0, -170.0));
        assert_eq!(normalize_point(0.0, -190.0), (0.0, 170.0));
        assert_eq!(normalize_point(450.0, 0.0), (90.0, 0.0));
    }

    #[test]
    fn decimal_integer_parts_are_exact() {
        let d = |s: &str| Decimal::parse(s).unwrap();
        assert_eq!(d("10223372036854775807.23").to_u64(), Some(10223372036854775807));
        assert!(d("10223372036854775807.23").fraction);
        assert!(!d("10223372036854775807.00").fraction);
        assert_eq!(d("18446744073709551615.99").to_u64(), Some(u64::MAX));
        assert_eq!(d("18446744073709551616").to_u64(), None);
        assert_eq!(d("1e3").to_u64(), Some(1000));
        assert_eq!(d("1.5e1").to_u64(), Some(15));
        assert_eq!(d("1E+30").integer_string(), "1000000000000000000000000000000");
        assert_eq!(d("-0.5").integer_string(), "0");
        assert!(d("-0.5").is_zero() && d("-0.5").fraction);
        assert_eq!(d("-5").integer_string(), "-5");
        assert_eq!(d("5e-1").to_u64(), Some(0));
        assert_eq!(d(".5").to_u64(), Some(0));
        assert_eq!(d("1e100000").digits, None);
        for bad in ["", " 5", "0x10", "1e", "-", "1.2.3", "abc", "1e+"] {
            assert_eq!(Decimal::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn unsigned_long_strings_keep_their_fraction() {
        // Elasticsearch: `BigDecimal.toBigIntegerExact` refuses a fraction no double can see.
        let e = parse_unsigned(&Value::from("10223372036854775807.23")).unwrap_err();
        assert_eq!(e.reason(), "Value \"10223372036854775807.23\" has a decimal part");
        assert_eq!(parse_unsigned(&Value::from("1e19")).unwrap(), 10_000_000_000_000_000_000);
        assert_eq!(parse_unsigned(&Value::from("-5")).unwrap_err().reason(), "Value [-5] is out of range for unsigned long");
    }

    #[test]
    fn opensearch_unsigned_long_truncates() {
        let run = |json: &str, coerce: bool| {
            let v = crate::json::parse(json.as_bytes()).unwrap().unwrap();
            let mut p = XParser::started(&v, Some(json.as_bytes()), Vec::new());
            p.next_token();
            p.next_token();
            parse_unsigned_os(&p, coerce).map_err(|e| e.cause.reason().to_string())
        };
        assert_eq!(run(r#"{"c":"10223372036854775807.23"}"#, true), Ok(10223372036854775807));
        assert_eq!(run(r#"{"c":18446744073709551615.9}"#, true), Ok(u64::MAX));
        assert_eq!(run(r#"{"c":-0.5}"#, true), Ok(0));
        assert_eq!(run(r#"{"c":1.5}"#, false), Err("Value [1.5] has a decimal part".into()));
        assert_eq!(run(r#"{"c":-5}"#, true), Err("Value [-5] is out of range for an unsigned long".into()));
        assert_eq!(run(r#"{"c":1e30}"#, true), Err("Value [1000000000000000000000000000000] is out of range for an unsigned long".into()));
        assert_eq!(run(r#"{"c":"-1"}"#, true), Err("Value [-1] is out of range for an unsigned long".into()));
        assert_eq!(run(r#"{"c":" 5"}"#, true), Err("For input string: \" 5\"".into()));
    }
}
