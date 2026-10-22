//! `org.elasticsearch.ingest.common.Processors`: the ingest processors as functions of ingest
//! scripts (`Processors.bytes`, `lowercase`, `uppercase`, `json`, `jsonLenient`, `urlDecode`,
//! `communityId`, `uriParts`).
//!
//! Each behaves like the processor it wraps, down to its errors: `ByteSizeValue`'s parse
//! exceptions and their causes, Jackson's messages for bad JSON (see [`crate::jackson`]), the
//! JDK's `URLDecoder` and `URI`/`URL` rules, and the Community ID flow checks, each with the
//! frames Elasticsearch's stack trace shows.

use std::rc::Rc;

use base64::Engine as _;
use sha1::Digest as _;

use super::uri::{parse_uri, parse_url};
use crate::jackson::{Tok, XContentReader};
use crate::lookup::Builder;
use crate::types::cls;
use crate::value::{JMap, Value};
use crate::vm::{npe, Throw, Thrown, Vm, VR};

const IC: &str = "org.elasticsearch.ingest.common@9.5.5/org.elasticsearch.ingest.common.";
const SERVER: &str = "org.elasticsearch.server@9.5.5/org.elasticsearch.";
const WEB: &str = "org.elasticsearch.web@9.5.5/org.elasticsearch.web.";

fn ic(frames: &[&str]) -> Vec<String> {
    frames.iter().map(|f| format!("{IC}{f}")).collect()
}

fn with_frames(mut t: Thrown, frames: Vec<String>) -> Thrown {
    t.frames.extend(frames);
    t
}

fn simple(class: crate::types::ClassId, msg: impl Into<String>) -> Throw {
    *Throw::new(class, Some(msg.into()))
}

// ---------------------------------------------------------------------------------- bytes

const SETTING: &str = "Ingest Field";

/// An `ElasticsearchParseException` with its `ByteSizeValue` frames (innermost first) and an
/// optional cause.
fn parse_exception(msg: String, frames: &[String], cause: Option<Throw>) -> Thrown {
    let mut t = Throw::new(cls::RUNTIME_EXCEPTION, Some(msg));
    t.es_type = Some("parse_exception");
    t.es_root = true;
    t.frames = frames.iter().map(|f| format!("{SERVER}common.unit.ByteSizeValue.{f}")).collect();
    match cause {
        Some(c) => t.with_cause(&c),
        None => t,
    }
}

/// Records `suppressed` (`Throwable.addSuppressed`) on an exception.
fn suppress(mut t: Thrown, suppressed: &Throw) -> Thrown {
    t.cause_fields.push(("suppressed".into(), serde_json::json!([suppressed.cause_json()])));
    t
}

/// `Long.parseLong`, or the message of its `NumberFormatException`.
fn parse_long(s: &str) -> Result<i64, String> {
    crate::java::parse_long(s, 10).ok_or_else(|| crate::java::number_format_message(s, 10))
}

const UNITS: [(&str, u32, i64); 10] = [
    ("k", 271, 1 << 10),
    ("kb", 273, 1 << 10),
    ("m", 275, 1 << 20),
    ("mb", 277, 1 << 20),
    ("g", 279, 1 << 30),
    ("gb", 281, 1 << 30),
    ("t", 283, 1 << 40),
    ("tb", 285, 1 << 40),
    ("p", 287, 1 << 50),
    ("pb", 289, 1 << 50),
];

fn unit_suffix(unit: i64) -> &'static str {
    match unit {
        1 => "b",
        0x400 => "kb",
        0x10_0000 => "mb",
        0x4000_0000 => "gb",
        0x100_0000_0000 => "tb",
        _ => "pb",
    }
}

/// `ByteSizeValue.of(size, unit)`: the byte count, or the `IllegalArgumentException` message.
fn size_of(size: i64, unit: i64) -> Result<i64, String> {
    if size < -1 || (size == -1 && unit != 1) {
        return Err(format!("Values less than -1 bytes are not supported: {size}{}", unit_suffix(unit)));
    }
    if size > i64::MAX / unit {
        return Err(format!("Values greater than {} bytes are not supported: {size}{}", i64::MAX, unit_suffix(unit)));
    }
    Ok(size * unit)
}

/// A `BigDecimal` literal: its unscaled digits (leading zeros removed), scale and sign; `None`
/// for a `NumberFormatException`.
fn big_decimal(s: &str) -> Option<(Vec<u8>, i64, bool)> {
    let mut cs = s.chars().peekable();
    let mut neg = false;
    if let Some(&c) = cs.peek()
        && (c == '+' || c == '-')
    {
        neg = c == '-';
        cs.next();
    }
    let digit = |c: char| -> Option<u8> {
        let d = crate::unicode::digit(c as i32, 10);
        (d >= 0).then_some(d as u8)
    };
    let (mut digits, mut frac, mut seen_dot, mut any) = (Vec::new(), 0i64, false, false);
    while let Some(&c) = cs.peek() {
        if let Some(d) = digit(c) {
            digits.push(d);
            any = true;
            if seen_dot {
                frac += 1;
            }
        } else if c == '.' && !seen_dot {
            seen_dot = true;
        } else {
            break;
        }
        cs.next();
    }
    if !any {
        return None;
    }
    let mut exp: i64 = 0;
    if let Some(&c) = cs.peek()
        && (c == 'e' || c == 'E')
    {
        cs.next();
        let mut eneg = false;
        if let Some(&c) = cs.peek()
            && (c == '+' || c == '-')
        {
            eneg = c == '-';
            cs.next();
        }
        let mut eany = false;
        while let Some(&c) = cs.peek() {
            let d = digit(c)?;
            exp = exp.checked_mul(10)?.checked_add(d as i64)?;
            if exp > i32::MAX as i64 + 1 {
                return None;
            }
            eany = true;
            cs.next();
        }
        if !eany {
            return None;
        }
        if eneg {
            exp = -exp;
        }
        if i32::try_from(exp).is_err() {
            return None;
        }
    }
    if cs.next().is_some() {
        return None;
    }
    let start = digits.iter().position(|&d| d != 0).unwrap_or(digits.len());
    let scale = frac - exp;
    if i32::try_from(scale).is_err() {
        return None;
    }
    Some((digits[start..].to_vec(), scale, neg))
}

/// `decimal * unit` rounded up, if it fits a long.
fn decimal_bytes(digits: &[u8], scale: i64, unit: i64) -> Option<i64> {
    if digits.is_empty() {
        return Some(0);
    }
    if digits.len() > 38 {
        return None;
    }
    let mut v: u128 = 0;
    for &d in digits {
        v = v * 10 + d as u128;
    }
    let mut v = v.checked_mul(unit as u128)?;
    if scale <= 0 {
        for _ in 0..(-scale) {
            v = v.checked_mul(10)?;
        }
    } else {
        let div = 10u128.pow(scale as u32);
        v = v.div_ceil(div);
    }
    i64::try_from(v).ok()
}

/// `ByteSizeValue.parseBytesSizeValue(value, null, "Ingest Field").getBytes()`.
fn parse_bytes(value: &str) -> Result<i64, Thrown> {
    match value {
        "0" | "0b" | "0B" => return Ok(0),
        "1b" | "1B" => return Ok(1),
        "-1" | "-1b" | "-1B" => return Ok(-1),
        _ => {}
    }
    let lower = crate::lib_lang::java_lower(value);
    let lower = crate::java::java_trim(&lower);
    let entry = |line: u32| format!("parseBytesSizeValue(ByteSizeValue.java:{line})");
    for (suffix, line, unit) in UNITS {
        // `k` before `kb`: "1kb" does not end with "k"
        if !lower.ends_with(suffix) {
            continue;
        }
        let s = crate::java::java_trim(&lower[..lower.len() - suffix.len()]);
        return match parse_long(s) {
            Ok(n) => size_of(n, unit).map_err(|m| {
                parse_exception(
                    format!("failed to parse setting [{SETTING}] with value [{value}] as a size in bytes"),
                    &["parse(ByteSizeValue.java:337)".into(), entry(line)],
                    Some(simple(cls::ILLEGAL_ARGUMENT_EXCEPTION, m)),
                )
            }),
            Err(nfe) => {
                let at = |l: u32| [format!("parseDecimal(ByteSizeValue.java:{l})"), "parse(ByteSizeValue.java:332)".into(), entry(line)];
                let Some((digits, scale, neg)) = big_decimal(s) else {
                    let t = parse_exception(
                        format!("failed to parse setting [{SETTING}] with value [{value}]"),
                        &at(365),
                        Some(simple(cls::NUMBER_FORMAT_EXCEPTION, nfe)),
                    );
                    // `new BigDecimal(s)`'s own exception rides along
                    return Err(match crate::bignum::BigDec::parse(s) {
                        Err(e) => suppress(t, &e),
                        Ok(_) => t,
                    });
                };
                if neg && !digits.is_empty() {
                    return Err(parse_exception(format!("failed to parse setting [{SETTING}] with value [{value}]"), &at(375), None));
                }
                if scale > 2 {
                    return Err(parse_exception(format!("failed to parse setting [{SETTING}] with more than two decimals in value [{value}]"), &at(377), None));
                }
                decimal_bytes(&digits, scale, unit).ok_or_else(|| {
                    let t = parse_exception(
                        format!("failed to parse setting [{SETTING}] with value beyond {}: [{value}]", i64::MAX),
                        &["convertToBytes(ByteSizeValue.java:414)".into(), "parse(ByteSizeValue.java:333)".into(), entry(line)],
                        Some(simple(cls::ARITHMETIC_EXCEPTION, "Overflow")),
                    );
                    suppress(t, &simple(cls::NUMBER_FORMAT_EXCEPTION, nfe.clone()))
                })
            }
        };
    }
    if let Some(rest) = lower.strip_suffix('b') {
        let s = crate::java::java_trim(rest);
        let frames = |l: u32| [format!("parseBytes(ByteSizeValue.java:{l})"), entry(291)];
        return match parse_long(s) {
            Ok(n) => size_of(n, 1).map_err(|m| {
                parse_exception(
                    format!("failed to parse setting [{SETTING}] with value [{value}] as a size in bytes"),
                    &frames(309),
                    Some(simple(cls::ILLEGAL_ARGUMENT_EXCEPTION, m)),
                )
            }),
            Err(nfe) => Err(parse_exception(
                format!("failed to parse setting [{SETTING}] with value [{value}]"),
                &frames(307),
                Some(simple(cls::NUMBER_FORMAT_EXCEPTION, nfe)),
            )),
        };
    }
    Err(parse_exception(
        format!("failed to parse setting [{SETTING}] with value [{value}] as a size in bytes: unit is missing or unrecognized"),
        &[entry(294)],
        None,
    ))
}

fn p_bytes(_: &mut Vm, a: &mut [Value]) -> VR {
    let frames = ic(&["BytesProcessor.apply(BytesProcessor.java:29)", "Processors.bytes(Processors.java:30)"]);
    let Value::Str(s) = &a[0] else {
        return Err(with_frames(
            npe("Cannot invoke \"org.elasticsearch.common.unit.ByteSizeValue.getBytes()\" because the return value of \"org.elasticsearch.common.unit.ByteSizeValue.parseBytesSizeValue(String, org.elasticsearch.common.unit.ByteSizeValue, String)\" is null"),
            frames,
        ));
    };
    parse_bytes(s.as_str()).map(Value::Long).map_err(|t| with_frames(t, frames))
}

// ---------------------------------------------------------------------------------- case

fn p_lowercase(_: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::Str(s) => Ok(Value::string(crate::lib_lang::java_lower(s.as_str()))),
        _ => Err(with_frames(
            npe("Cannot invoke \"String.toLowerCase(java.util.Locale)\" because \"value\" is null"),
            ic(&["LowercaseProcessor.apply(LowercaseProcessor.java:29)", "Processors.lowercase(Processors.java:41)"]),
        )),
    }
}

fn p_uppercase(_: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::Str(s) => Ok(Value::string(crate::lib_lang::java_upper(s.as_str()))),
        _ => Err(with_frames(
            npe("Cannot invoke \"String.toUpperCase(java.util.Locale)\" because \"value\" is null"),
            ic(&["UppercaseProcessor.apply(UppercaseProcessor.java:28)", "Processors.uppercase(Processors.java:52)"]),
        )),
    }
}

// ---------------------------------------------------------------------------------- json

/// `JsonProcessor.apply(fieldValue, false, strict)` with `outer` the frames below it.
fn json_apply(field_value: &Value, strict: bool, outer: &[String]) -> VR {
    let text = if field_value.is_null() { "null".to_string() } else { field_value.to_java_string() };
    let mut r = XContentReader::new(&text, cls::HASH_MAP);
    let at = |line: u32| -> Vec<String> {
        let mut f = vec![format!("{IC}JsonProcessor.apply(JsonProcessor.java:{line})")];
        f.extend(outer.iter().cloned());
        f
    };
    let fail = |e: crate::jackson::ReadError, line: u32| -> Thrown {
        let f = at(line);
        let f: Vec<&str> = f.iter().map(String::as_str).collect();
        e.thrown(&f)
    };
    let value = match r.next_token().map_err(|e| fail(e, 99))? {
        Some(Tok::Str) => Value::string(r.text().map_err(|e| fail(e, 104))?),
        Some(Tok::Int | Tok::Float) => r.number_value(),
        Some(Tok::True) => Value::Bool(true),
        Some(Tok::False) => Value::Bool(false),
        Some(Tok::StartObject) => r.map().map_err(|e| fail(e, 110))?,
        Some(Tok::StartArray) => r.list().map_err(|e| fail(e, 112))?,
        _ => Value::Null,
    };
    if strict {
        let msg = format!("The input {text} is not valid JSON and the strict_json_parsing parameter is true");
        match r.next_token() {
            Err(e) => {
                let cause = fail(e, 99);
                let mut t = Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some(msg));
                t.frames = at(132);
                return Err(t.with_cause(&cause));
            }
            Ok(Some(_)) => {
                let mut t = Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some(msg));
                t.frames = at(135);
                return Err(t);
            }
            Ok(None) => {}
        }
    }
    Ok(value)
}

fn json_value(a: &mut [Value], strict: bool, line: u32, name: &str) -> VR {
    json_apply(&a[0], strict, &[format!("{IC}Processors.{name}(Processors.java:{line})")])
}

fn p_json(_: &mut Vm, a: &mut [Value]) -> VR {
    json_value(a, true, 62, "json")
}

fn p_json_lenient(_: &mut Vm, a: &mut [Value]) -> VR {
    json_value(a, false, 74, "jsonLenient")
}

/// `JsonProcessor.apply(map, field, false, REPLACE, strict)`: the field's JSON object merged
/// into the map.
fn json_into(vm: &mut Vm, a: &mut [Value], strict: bool, line: u32, name: &str) -> VR {
    let entry = format!("{IC}Processors.{name}(Processors.java:{line})");
    if a[0].is_null() {
        return Err(with_frames(
            npe("Cannot invoke \"java.util.Map.get(Object)\" because \"ctx\" is null"),
            vec![format!("{IC}JsonProcessor.apply(JsonProcessor.java:151)"), entry],
        ));
    }
    let field = a[1].clone();
    let raw = crate::lib_coll::map_get(vm, &a[0], &field)?;
    let parsed = json_apply(&raw, strict, &[format!("{IC}JsonProcessor.apply(JsonProcessor.java:151)"), entry.clone()])?;
    let Value::Map(m) = parsed else {
        let mut t = Throw::new(cls::ILLEGAL_ARGUMENT_EXCEPTION, Some("cannot add non-map fields to root of document".into()));
        t.frames = vec![format!("{IC}JsonProcessor.mergeParsedJson(JsonProcessor.java:165)"), format!("{IC}JsonProcessor.apply(JsonProcessor.java:152)"), entry];
        return Err(t);
    };
    for (k, v) in m.entries() {
        crate::lib_coll::map_put(vm, &a[0], k, v)?;
    }
    Ok(Value::Null)
}

fn p_json_map(vm: &mut Vm, a: &mut [Value]) -> VR {
    json_into(vm, a, true, 86, "json")
}

fn p_json_lenient_map(vm: &mut Vm, a: &mut [Value]) -> VR {
    json_into(vm, a, false, 101, "jsonLenient")
}

// ---------------------------------------------------------------------------------- urlDecode

/// `URLDecoder.decode(s, UTF_8)`: `+` is a space, runs of `%XY` are UTF-8 bytes.
fn url_decode(s: &str) -> Result<String, (String, u32)> {
    let u: Vec<u16> = s.encode_utf16().collect();
    let n = u.len();
    let mut out: Vec<u16> = Vec::with_capacity(n);
    let mut i = 0;
    while i < n {
        let c = u[i];
        if c == b'+' as u16 {
            out.push(b' ' as u16);
            i += 1;
        } else if c == b'%' as u16 {
            let mut bytes = Vec::new();
            let mut c = c;
            while i + 2 < n && c == b'%' as u16 {
                let mut v = 0u8;
                for &h in &u[i + 1..i + 3] {
                    let d = char::from_u32(h as u32).filter(|ch| ch.is_ascii_hexdigit()).and_then(|ch| ch.to_digit(16));
                    let Some(d) = d else {
                        let ch = String::from_utf16_lossy(&[h]);
                        return Err((format!("URLDecoder: Illegal hex characters in escape (%) pattern - not a hexadecimal digit: \"{ch}\" = {h}"), 226));
                    };
                    v = (v << 4) | d as u8;
                }
                bytes.push(v);
                i += 3;
                if i < n {
                    c = u[i];
                }
            }
            if i < n && c == b'%' as u16 {
                return Err(("URLDecoder: Incomplete trailing escape (%) pattern".into(), 219));
            }
            out.extend(String::from_utf8_lossy(&bytes).encode_utf16());
        } else {
            out.push(c);
            i += 1;
        }
    }
    Ok(String::from_utf16_lossy(&out))
}

fn p_url_decode(_: &mut Vm, a: &mut [Value]) -> VR {
    let frames = |line: u32| {
        let mut f = vec![format!("java.base/java.net.URLDecoder.decode(URLDecoder.java:{line})")];
        f.extend(ic(&["URLDecodeProcessor.apply(URLDecodeProcessor.java:28)", "Processors.urlDecode(Processors.java:111)"]));
        f
    };
    let Value::Str(s) = &a[0] else {
        return Err(with_frames(npe("Cannot invoke \"String.length()\" because \"s\" is null"), frames(174)));
    };
    url_decode(s.as_str()).map(Value::string).map_err(|(m, line)| with_frames(crate::vm::iae(m), frames(line)))
}

// ---------------------------------------------------------------------------------- uriParts

fn p_uri_parts(_: &mut Vm, a: &mut [Value]) -> VR {
    let tail = || {
        let mut f = vec![format!("{WEB}UriParts.parse(UriParts.java:60)")];
        f.extend(ic(&["UriPartsProcessor.apply(UriPartsProcessor.java:88)", "Processors.uriParts(Processors.java:194)"]));
        f
    };
    let Value::Str(s) = &a[0] else {
        let mut f = vec![
            "java.base/java.net.URI$Parser.parse(URI.java:3199)".to_string(),
            "java.base/java.net.URI.<init>(URI.java:651)".to_string(),
            format!("{WEB}UriParts.parse(UriParts.java:69)"),
        ];
        f.extend(tail());
        return Err(with_frames(npe("Cannot invoke \"String.length()\" because \"this.input\" is null"), f));
    };
    let s = s.as_str();
    let Some(parts) = parse_uri(s).or_else(|| parse_url(s)) else {
        let mut f = vec![format!("{WEB}UriParts.parse(UriParts.java:75)")];
        f.extend(tail());
        return Err(with_frames(crate::vm::iae(format!("unable to parse URI [{s}]")), f));
    };
    let m = JMap::new(cls::HASH_MAP);
    let put = |k: &str, v: Value| {
        m.insert_raw(Value::str(k), v);
    };
    if let Some(h) = parts.host {
        put("domain", Value::string(h));
    }
    if let Some(f) = parts.fragment {
        put("fragment", Value::string(f));
    }
    if let Some(p) = parts.path {
        if let Some(seg) = p.rfind('/') {
            let last = &p[seg..];
            if let Some(dot) = last.rfind('.') {
                put("extension", Value::str(&last[dot + 1..]));
            }
        }
        put("path", Value::string(p));
    }
    if parts.port >= 0 {
        put("port", Value::Int(parts.port));
    }
    if let Some(q) = parts.query {
        put("query", Value::string(q));
    }
    if let Some(sc) = parts.scheme {
        put("scheme", Value::string(sc));
    }
    if let Some(u) = parts.user_info {
        if let Some(colon) = u.find(':') {
            put("username", Value::str(&u[..colon]));
            put("password", Value::str(&u[colon + 1..]));
        }
        put("user_info", Value::string(u));
    }
    Ok(Value::Map(Rc::new(m)))
}

// ---------------------------------------------------------------------------------- communityId

#[derive(Clone, Copy, PartialEq, Eq)]
enum Transport {
    Icmp,
    Tcp,
    Udp,
    IcmpV6,
    Sctp,
    Other,
}

fn transport_of(n: i32) -> Transport {
    match n {
        1 => Transport::Icmp,
        6 => Transport::Tcp,
        17 => Transport::Udp,
        58 => Transport::IcmpV6,
        132 => Transport::Sctp,
        _ => Transport::Other,
    }
}

/// `Number.intValue()` of a boxed number.
fn int_value(v: &Value) -> Option<i32> {
    Some(match v {
        Value::Byte(b) => *b as i32,
        Value::Short(s) => *s as i32,
        Value::Int(i) => *i,
        Value::Long(l) => *l as i32,
        Value::Float(f) => crate::java::d2i(*f as f64),
        Value::Double(d) => crate::java::d2i(*d),
        // `BigInteger`/`BigDecimal` transport numbers are not supported
        _ => return None,
    })
}

struct Flow<'f> {
    frames: &'f dyn Fn(&[&str]) -> Vec<String>,
}

impl Flow<'_> {
    fn iae(&self, msg: String, inner: &[&str]) -> Thrown {
        with_frames(crate::vm::iae(msg), (self.frames)(inner))
    }

    fn transport(&self, o: &Value) -> Result<(i32, Transport), Thrown> {
        let from_number = |n: i32, via: &str| -> Result<(i32, Transport), Thrown> {
            if !(0..255).contains(&n) {
                return Err(self.iae(
                    format!("invalid transport protocol number [{n}]"),
                    &["CommunityIdProcessor$Transport.fromNumber(CommunityIdProcessor.java:472)", via, "CommunityIdProcessor.buildFlow(CommunityIdProcessor.java:227)"],
                ));
            }
            Ok((n, transport_of(n)))
        };
        if let Some(n) = int_value(o).filter(|_| !matches!(o, Value::Str(_) | Value::Bool(_) | Value::Char(_))) {
            return from_number(n, "CommunityIdProcessor$Transport.fromObject(CommunityIdProcessor.java:494)");
        }
        if let Value::Str(s) = o {
            let name = crate::lib_lang::java_lower(s.as_str());
            let named = match name.as_str() {
                "icmp" => Some(1),
                "igmp" => Some(2),
                "tcp" => Some(6),
                "udp" => Some(17),
                "gre" => Some(47),
                "ipv6-icmp" | "icmpv6" => Some(58),
                "eigrp" => Some(88),
                "ospf" => Some(89),
                "pim" => Some(103),
                "sctp" => Some(132),
                _ => None,
            };
            if let Some(n) = named {
                return Ok((n, transport_of(n)));
            }
            if let Some(n) = crate::java::parse_int(s.as_str(), 10) {
                return from_number(n, "CommunityIdProcessor$Transport.fromObject(CommunityIdProcessor.java:504)");
            }
            return Err(self.iae(
                format!("could not convert string [{}] to transport protocol", s.as_str()),
                &["CommunityIdProcessor$Transport.fromObject(CommunityIdProcessor.java:509)", "CommunityIdProcessor.buildFlow(CommunityIdProcessor.java:227)"],
            ));
        }
        Err(self.iae(
            format!("could not convert value of type [{}] to transport protocol", crate::lookup::java_binary_name(o.class())),
            &["CommunityIdProcessor$Transport.fromObject(CommunityIdProcessor.java:512)", "CommunityIdProcessor.buildFlow(CommunityIdProcessor.java:227)"],
        ))
    }

    /// `parseIntFromObjectOrString`.
    fn int(&self, o: &Value, what: &str, line: u32) -> Result<i32, Thrown> {
        match o {
            Value::Null => return Ok(0),
            Value::Str(s) => {
                if let Some(n) = crate::java::parse_int(s.as_str(), 10) {
                    return Ok(n);
                }
            }
            Value::Bool(_) | Value::Char(_) => {}
            _ => {
                if let Some(n) = int_value(o) {
                    return Ok(n);
                }
            }
        }
        let call = format!("CommunityIdProcessor.buildFlow(CommunityIdProcessor.java:{line})");
        Err(self.iae(
            format!("unable to parse {what} [{}]", o.to_java_string()),
            &["CommunityIdProcessor.parseIntFromObjectOrString(CommunityIdProcessor.java:281)", &call],
        ))
    }
}

fn icmp_equivalent(t: i32, v6: bool) -> Option<i32> {
    let v4: &[(i32, i32)] = &[(8, 0), (0, 8), (13, 14), (14, 13), (15, 16), (10, 9), (9, 10), (17, 18), (18, 17)];
    let v6m: &[(i32, i32)] = &[(128, 129), (129, 128), (133, 134), (134, 133), (136, 135), (135, 136), (130, 131), (139, 140), (140, 139), (144, 145), (145, 144)];
    (if v6 { v6m } else { v4 }).iter().find(|(k, _)| *k == t).map(|(_, v)| *v)
}

fn uint16(n: i32, frames: &dyn Fn(&[&str]) -> Vec<String>, inner: &[&str]) -> Result<[u8; 2], Thrown> {
    if !(0..=65535).contains(&n) {
        let mut all = vec!["CommunityIdProcessor.toUint16(CommunityIdProcessor.java:261)"];
        all.extend_from_slice(inner);
        return Err(with_frames(Throw::new(cls::ILLEGAL_STATE_EXCEPTION, Some(format!("number [{n}] must be a value between 0 and 65535"))), frames(&all)));
    }
    Ok([(n >> 8) as u8, n as u8])
}

/// `CommunityIdProcessor.apply(...)`; `nine` is the overload with a seed.
fn community_id(a: &[Value], seed: i32, nine: bool) -> VR {
    let tail: Vec<String> = if nine {
        ic(&["Processors.communityId(Processors.java:139)"])
    } else {
        ic(&["CommunityIdProcessor.apply(CommunityIdProcessor.java:195)", "Processors.communityId(Processors.java:175)"])
    };
    let frames = |inner: &[&str]| -> Vec<String> {
        let mut f = ic(inner);
        f.extend(tail.iter().cloned());
        f
    };
    let build = |inner: &[&str]| -> Vec<String> {
        let mut all = inner.to_vec();
        all.push("CommunityIdProcessor.apply(CommunityIdProcessor.java:167)");
        frames(&all)
    };
    let flow = Flow { frames: &build };
    let no_flow = || with_frames(crate::vm::iae("unable to construct flow from document"), frames(&["CommunityIdProcessor.apply(CommunityIdProcessor.java:179)"]));
    let (Value::Str(src), Value::Str(dst)) = (&a[0], &a[1]) else {
        return Err(no_flow());
    };
    let ip = |s: &str, line: u32| -> Result<Vec<u8>, Thrown> {
        match super::misc::parse_ip(s) {
            Ok(std::net::IpAddr::V4(v)) => Ok(v.octets().to_vec()),
            Ok(std::net::IpAddr::V6(v)) => Ok(v.octets().to_vec()),
            Err(_) => {
                let call = format!("{IC}CommunityIdProcessor.buildFlow(CommunityIdProcessor.java:{line})");
                let mut f = vec![
                    format!("{SERVER}common.network.InetAddresses.forString(InetAddresses.java:452)"),
                    format!("{SERVER}common.network.InetAddresses.forString(InetAddresses.java:431)"),
                    call,
                    format!("{IC}CommunityIdProcessor.apply(CommunityIdProcessor.java:167)"),
                ];
                f.extend(tail.iter().cloned());
                Err(with_frames(crate::vm::iae(format!("'{s}' is not an IP string literal.")), f))
            }
        }
    };
    let source = ip(src.as_str(), 217)?;
    let destination = ip(dst.as_str(), 218)?;
    let protocol = if a[2].is_null() { &a[3] } else { &a[2] };
    if protocol.is_null() {
        return Err(no_flow());
    }
    let (number, kind) = flow.transport(protocol)?;
    let (mut sport, mut dport, mut icmp_type, mut icmp_code) = (0, 0, 0, 0);
    match kind {
        Transport::Tcp | Transport::Udp | Transport::Sctp => {
            sport = flow.int(&a[4], "source port", 231)?;
            if !(1..=65535).contains(&sport) {
                return Err(flow.iae(format!("invalid source port [{}]", a[4].to_java_string()), &["CommunityIdProcessor.buildFlow(CommunityIdProcessor.java:233)"]));
            }
            dport = flow.int(&a[5], "destination port", 235)?;
            if !(1..=65535).contains(&dport) {
                return Err(flow.iae(format!("invalid destination port [{}]", a[5].to_java_string()), &["CommunityIdProcessor.buildFlow(CommunityIdProcessor.java:237)"]));
            }
        }
        Transport::Icmp | Transport::IcmpV6 => {
            icmp_type = flow.int(&a[6], "icmp type", 242)?;
            icmp_code = flow.int(&a[7], "icmp code", 243)?;
        }
        Transport::Other => {}
    }
    let seed = uint16(seed, &frames, &["CommunityIdProcessor.apply(CommunityIdProcessor.java:181)"])?;
    // `Flow.toBytes`
    let to_bytes = &["CommunityIdProcessor$Flow.toCommunityId(CommunityIdProcessor.java:403)", "CommunityIdProcessor.apply(CommunityIdProcessor.java:181)"];
    let has_port = kind != Transport::Other;
    let icmp = matches!(kind, Transport::Icmp | Transport::IcmpV6);
    let mut one_way = false;
    if icmp {
        let eq = icmp_equivalent(icmp_type, kind == Transport::IcmpV6);
        one_way = eq.is_none();
        sport = icmp_type;
        dport = eq.unwrap_or(icmp_code);
    }
    let magnitude = |b: &[u8]| -> Vec<u8> { b.iter().copied().skip_while(|&x| x == 0).collect() };
    let (ms, md) = (magnitude(&source), magnitude(&destination));
    let cmp = ms.len().cmp(&md.len()).then_with(|| ms.cmp(&md));
    let ordered = cmp.is_lt() || (cmp.is_eq() && sport < dport);
    let keep = ordered || (icmp && one_way);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(if keep { &source } else { &destination });
    bytes.extend_from_slice(if keep { &destination } else { &source });
    let u16_at = |n: i32, l: u32| -> Result<[u8; 2], Thrown> {
        let at = format!("CommunityIdProcessor$Flow.toBytes(CommunityIdProcessor.java:{l})");
        let mut inner = vec![at.as_str()];
        inner.extend_from_slice(to_bytes);
        uint16(n, &frames, &inner)
    };
    bytes.extend_from_slice(&u16_at(number << 8, 389)?);
    if has_port {
        bytes.extend_from_slice(&u16_at(if keep { sport } else { dport }, 392)?);
        bytes.extend_from_slice(&u16_at(if keep { dport } else { sport }, 393)?);
    }
    let mut h = sha1::Sha1::new();
    h.update(seed);
    h.update(&bytes);
    let digest = h.finalize();
    Ok(Value::string(format!("1:{}", base64::engine::general_purpose::STANDARD.encode(digest))))
}

fn p_community_id(_: &mut Vm, a: &mut [Value]) -> VR {
    community_id(a, 0, false)
}

fn p_community_id_seed(_: &mut Vm, a: &mut [Value]) -> VR {
    let Value::Int(seed) = a[8] else { return Err(crate::vm::cce("seed")) };
    community_id(a, seed, true)
}

pub fn register(b: &mut Builder) {
    use cls::*;
    b.supers(INGEST_PROCESSORS, &[OBJECT]);
    b.s(INGEST_PROCESSORS, "long bytes(String)", p_bytes);
    b.s(INGEST_PROCESSORS, "String lowercase(String)", p_lowercase);
    b.s(INGEST_PROCESSORS, "String uppercase(String)", p_uppercase);
    b.s(INGEST_PROCESSORS, "Object json(Object)", p_json);
    b.s(INGEST_PROCESSORS, "Object jsonLenient(Object)", p_json_lenient);
    b.s(INGEST_PROCESSORS, "void json(Map,String)", p_json_map);
    b.s(INGEST_PROCESSORS, "void jsonLenient(Map,String)", p_json_lenient_map);
    b.s(INGEST_PROCESSORS, "String urlDecode(String)", p_url_decode);
    b.s(INGEST_PROCESSORS, "String communityId(String,String,Object,Object,Object,Object,Object,Object,int)", p_community_id_seed);
    b.s(INGEST_PROCESSORS, "String communityId(String,String,Object,Object,Object,Object,Object,Object)", p_community_id);
    b.s(INGEST_PROCESSORS, "Map uriParts(String)", p_uri_parts);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_sizes() {
        let ok = |s: &str| parse_bytes(s).ok();
        assert_eq!(ok("1kb"), Some(1024));
        assert_eq!(ok("1.5k"), Some(1536));
        assert_eq!(ok(" 2 mb "), Some(2 << 20));
        assert_eq!(ok("1.1kb"), Some(1127));
        assert_eq!(ok("1e2k"), Some(102400));
        assert_eq!(ok("１kb"), Some(1024));
        assert_eq!(ok("10.5pb"), Some(11821949021847552));
        assert_eq!(ok("1"), None);
        assert_eq!(ok("1.555k"), None);
        assert_eq!(ok("9007199254740993.5k"), None);
    }

    #[test]
    fn url_decoding() {
        assert_eq!(url_decode("a%20b+c").unwrap(), "a b c");
        assert_eq!(url_decode("%E2%82%AC").unwrap(), "€");
        assert_eq!(url_decode("%C3").unwrap(), "\u{FFFD}");
        assert_eq!(url_decode("%").unwrap_err().1, 219);
        assert_eq!(url_decode("%zz").unwrap_err().1, 226);
    }
}
