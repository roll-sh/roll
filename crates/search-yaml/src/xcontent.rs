//! Bodies in Elasticsearch's other content types: YAML, CBOR and Smile.
//!
//! Elasticsearch's Java runner sends every request body in a random content type
//! (`ClientYamlTestExecutionContext.getContentType`: any `XContentType` for one body, JSON or Smile
//! for the multi-document bodies of `bulk`/`msearch`), and Elasticsearch answers in the request's
//! type, so its suites also exercise every parser and generator. `--content-type` reproduces that:
//! this module encodes [`Value`]s like Jackson's generators do and decodes the responses.
//!
//! - **YAML**: block style after a `---` line, every string and key double-quoted (so nothing is
//!   re-typed by the reader) and floats in the form Jackson's YAML reader recognises
//!   (`1.0e+21`: a dot and a signed exponent).
//! - **CBOR** (RFC 8949): maps and arrays with indefinite lengths, as Jackson writes them; doubles
//!   as 8-byte floats, integers in the smallest head, beyond 64 bits as bignum tags. The decoder
//!   takes definite and indefinite lengths, chunked strings, half/single/double floats and tags.
//! - **Smile** (Jackson's binary JSON): the `:)\n` header with shared property names enabled, as
//!   Jackson's `SmileGenerator` defaults; ints as small/zigzag VInts, doubles as 7-bit groups,
//!   big integers as 7-bit-encoded two's complement. The decoder also handles shared string
//!   values, long strings, big decimals and raw or 7-bit binary.
//!
//! Binary values (CBOR byte strings, Smile binary) decode to base64 strings, which is how the JSON
//! form of the same response carries them. Decoders bound every allocation by the bytes actually
//! present and limit nesting, so a broken target can't make the runner allocate or recurse
//! without bound.

use std::fmt::Write as _;

use base64::Engine as _;

use crate::value::{Map, Value};

/// A content type the runner can send and read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    Json,
    Yaml,
    Cbor,
    Smile,
}

/// A request media type: a format, optionally as Elasticsearch's vendor type
/// (`application/vnd.elasticsearch+json`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MediaType {
    pub format: Format,
    pub vendor: bool,
}

impl MediaType {
    pub const JSON: MediaType = MediaType { format: Format::Json, vendor: false };

    /// Every type `XContentType.values()` lists: what the Java runner picks from for one body.
    pub const ALL: [MediaType; 8] = [
        MediaType { format: Format::Json, vendor: false },
        MediaType { format: Format::Smile, vendor: false },
        MediaType { format: Format::Yaml, vendor: false },
        MediaType { format: Format::Cbor, vendor: false },
        MediaType { format: Format::Json, vendor: true },
        MediaType { format: Format::Smile, vendor: true },
        MediaType { format: Format::Yaml, vendor: true },
        MediaType { format: Format::Cbor, vendor: true },
    ];

    /// The types multi-document bodies can use (`STREAMING_CONTENT_TYPES`).
    pub const STREAMING: [MediaType; 2] = [MediaType { format: Format::Json, vendor: false }, MediaType { format: Format::Smile, vendor: false }];

    /// The `Content-Type` value (`XContentType.mediaTypeWithoutParameters()`).
    pub fn mime(self) -> &'static str {
        match (self.format, self.vendor) {
            (Format::Json, false) => "application/json",
            (Format::Yaml, false) => "application/yaml",
            (Format::Cbor, false) => "application/cbor",
            (Format::Smile, false) => "application/smile",
            (Format::Json, true) => "application/vnd.elasticsearch+json",
            (Format::Yaml, true) => "application/vnd.elasticsearch+yaml",
            (Format::Cbor, true) => "application/vnd.elasticsearch+cbor",
            (Format::Smile, true) => "application/vnd.elasticsearch+smile",
        }
    }

    /// The `Content-Type` of a request body, as the Java runner's entity sends it
    /// (`ContentType.create(mediaTypeWithoutParameters(), UTF_8)`): `application/json; charset=UTF-8`.
    pub fn request_content_type(self) -> String {
        format!("{}; charset=UTF-8", self.mime())
    }

    /// Parses a `--content-type` value: `json`, `yaml`, `cbor`, `smile`, optionally `vnd-` prefixed.
    pub fn parse(s: &str) -> Option<MediaType> {
        let (vendor, name) = match s.strip_prefix("vnd-") {
            Some(rest) => (true, rest),
            None => (false, s),
        };
        let format = match name {
            "json" => Format::Json,
            "yaml" => Format::Yaml,
            "cbor" => Format::Cbor,
            "smile" => Format::Smile,
            _ => return None,
        };
        Some(MediaType { format, vendor })
    }
}

impl Format {
    /// The format of a response `Content-Type` (`XContentType.fromMediaType`), `None` for text.
    pub fn from_content_type(content_type: &str) -> Option<Format> {
        let mime = content_type.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
        let sub = mime.rsplit('/').next().unwrap_or("");
        let sub = sub.rsplit('+').next().unwrap_or(sub);
        match sub {
            "json" => Some(Format::Json),
            "yaml" => Some(Format::Yaml),
            "cbor" => Some(Format::Cbor),
            "smile" => Some(Format::Smile),
            _ => None,
        }
    }

    /// The byte between documents of a multi-document body (`XContent.bulkSeparator()`).
    pub fn bulk_separator(self) -> u8 {
        match self {
            Format::Smile => 0xFF,
            _ => b'\n',
        }
    }

    /// Appends one document.
    pub fn encode(self, v: &Value, out: &mut Vec<u8>) {
        match self {
            Format::Json => serde_json::to_writer(&mut *out, v).expect("writing JSON to a Vec"),
            Format::Yaml => encode_yaml(v, out),
            Format::Cbor => encode_cbor(v, out),
            Format::Smile => SmileWriter::default().document(v, out),
        }
    }

    /// Decodes one document.
    pub fn decode(self, bytes: &[u8]) -> Result<Value, String> {
        match self {
            Format::Json => Value::from_json(bytes).map_err(|e| format!("unable to parse the JSON response: {e}")),
            Format::Yaml => crate::yaml::parse_value(&String::from_utf8_lossy(bytes)).map_err(|e| format!("unable to parse the YAML response: {e}")),
            Format::Cbor => decode_cbor(bytes).map_err(|e| format!("unable to parse the CBOR response: {e}")),
            Format::Smile => decode_smile(bytes).map_err(|e| format!("unable to parse the Smile response: {e}")),
        }
    }

    pub fn is_binary(self) -> bool {
        matches!(self, Format::Cbor | Format::Smile)
    }
}

/// Adds a decoded field, rejecting a repeated name as Jackson's `STRICT_DUPLICATE_DETECTION`
/// (enabled by every Elasticsearch XContent parser) does.
fn insert_unique(m: &mut Map, key: String, v: Value) -> Result<(), String> {
    match m.entry(key) {
        indexmap::map::Entry::Occupied(e) => Err(format!("Duplicate field '{}'", e.key())),
        indexmap::map::Entry::Vacant(e) => {
            e.insert(v);
            Ok(())
        }
    }
}

/// Nesting limit for the binary decoders (Elasticsearch's own parsers stop far earlier).
const MAX_DEPTH: usize = 128;

fn base64(bytes: &[u8]) -> Value {
    Value::Str(base64::engine::general_purpose::STANDARD.encode(bytes))
}

// ---------------------------------------------------------------------------------------------
// YAML

fn encode_yaml(v: &Value, out: &mut Vec<u8>) {
    let mut s = String::from("---");
    match v {
        Value::Map(m) if !m.is_empty() => {
            s.push('\n');
            yaml_map(m, 0, &mut s);
        }
        Value::List(l) if !l.is_empty() => {
            s.push('\n');
            yaml_list(l, 0, &mut s);
        }
        other => {
            s.push(' ');
            yaml_scalar(other, &mut s);
            s.push('\n');
        }
    }
    out.extend_from_slice(s.as_bytes());
}

fn indent(n: usize, s: &mut String) {
    for _ in 0..n {
        s.push(' ');
    }
}

/// `key: value` lines at `level`; the first line's indentation is already written when
/// `first_inline` (a map that starts a list item).
fn yaml_map_entries(m: &Map, level: usize, first_inline: bool, s: &mut String) {
    for (i, (k, v)) in m.iter().enumerate() {
        if i > 0 || !first_inline {
            indent(level, s);
        }
        yaml_string(k, s);
        s.push(':');
        yaml_value_after_key(v, level, s);
    }
}

fn yaml_map(m: &Map, level: usize, s: &mut String) {
    yaml_map_entries(m, level, false, s);
}

fn yaml_value_after_key(v: &Value, level: usize, s: &mut String) {
    match v {
        Value::Map(m) if !m.is_empty() => {
            s.push('\n');
            yaml_map(m, level + 2, s);
        }
        Value::List(l) if !l.is_empty() => {
            s.push('\n');
            yaml_list(l, level + 2, s);
        }
        other => {
            s.push(' ');
            yaml_scalar(other, s);
            s.push('\n');
        }
    }
}

fn yaml_list(l: &[Value], level: usize, s: &mut String) {
    for item in l {
        indent(level, s);
        yaml_item(item, level, s);
    }
}

/// One `- item` (the indentation before the dash is written).
fn yaml_item(item: &Value, level: usize, s: &mut String) {
    s.push_str("- ");
    match item {
        Value::Map(m) if !m.is_empty() => yaml_map_entries(m, level + 2, true, s),
        Value::List(l) if !l.is_empty() => {
            for (i, inner) in l.iter().enumerate() {
                if i > 0 {
                    indent(level + 2, s);
                }
                yaml_item(inner, level + 2, s);
            }
        }
        other => {
            yaml_scalar(other, s);
            s.push('\n');
        }
    }
}

fn yaml_scalar(v: &Value, s: &mut String) {
    match v {
        Value::Null => s.push_str("null"),
        Value::Bool(b) => s.push_str(if *b { "true" } else { "false" }),
        Value::Int(i) => {
            let _ = write!(s, "{i}");
        }
        Value::Float(f) => yaml_float(*f, s),
        Value::Str(text) => yaml_string(text, s),
        Value::Map(_) => s.push_str("{}"),
        Value::List(_) => s.push_str("[]"),
    }
}

/// A float as `[-+]?[0-9]*\.[0-9]*([eE][-+][0-9]+)?`, the only shape Jackson's YAML reader takes
/// for a float.
fn yaml_float(f: f64, s: &mut String) {
    if f.is_nan() {
        s.push_str(".nan");
        return;
    }
    if f.is_infinite() {
        s.push_str(if f > 0.0 { ".inf" } else { "-.inf" });
        return;
    }
    let text = format!("{f:?}");
    match text.split_once('e') {
        Some((mantissa, exp)) => {
            s.push_str(mantissa);
            if !mantissa.contains('.') {
                s.push_str(".0");
            }
            s.push('e');
            if !exp.starts_with('-') {
                s.push('+');
            }
            s.push_str(exp);
        }
        None => s.push_str(&text),
    }
}

/// A double-quoted YAML string (escapes for `"`, `\`, control characters and the YAML line
/// breaks NEL, LS and PS).
fn yaml_string(text: &str, s: &mut String) {
    s.push('"');
    for c in text.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '\t' => s.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7F}' || c == '\u{85}' || c == '\u{2028}' || c == '\u{2029}' || c == '\u{FEFF}' => {
                let _ = write!(s, "\\u{:04X}", c as u32);
            }
            c => s.push(c),
        }
    }
    s.push('"');
}

// ---------------------------------------------------------------------------------------------
// CBOR

fn cbor_head(major: u8, n: u64, out: &mut Vec<u8>) {
    let m = major << 5;
    if n < 24 {
        out.push(m | n as u8);
    } else if n <= 0xFF {
        out.extend_from_slice(&[m | 24, n as u8]);
    } else if n <= 0xFFFF {
        out.push(m | 25);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n <= 0xFFFF_FFFF {
        out.push(m | 26);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

fn encode_cbor(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(0xF6),
        Value::Bool(b) => out.push(if *b { 0xF5 } else { 0xF4 }),
        Value::Int(i) => {
            if let Ok(u) = u64::try_from(*i) {
                cbor_head(0, u, out);
            } else if let Ok(n) = u64::try_from(-1 - *i) {
                cbor_head(1, n, out);
            } else {
                // Bignum: tag 2 (positive) or 3 (negative, -1 - n) over the big-endian magnitude.
                let (tag, magnitude) = if *i > 0 { (2, *i as u128) } else { (3, (-1 - *i) as u128) };
                cbor_head(6, tag, out);
                let bytes = magnitude.to_be_bytes();
                let first = bytes.iter().position(|b| *b != 0).unwrap_or(bytes.len() - 1);
                cbor_head(2, (bytes.len() - first) as u64, out);
                out.extend_from_slice(&bytes[first..]);
            }
        }
        Value::Float(f) => {
            out.push(0xFB);
            out.extend_from_slice(&f.to_be_bytes());
        }
        Value::Str(s) => {
            cbor_head(3, s.len() as u64, out);
            out.extend_from_slice(s.as_bytes());
        }
        Value::List(items) => {
            out.push(0x9F);
            for item in items {
                encode_cbor(item, out);
            }
            out.push(0xFF);
        }
        Value::Map(m) => {
            out.push(0xBF);
            for (k, item) in m {
                cbor_head(3, k.len() as u64, out);
                out.extend_from_slice(k.as_bytes());
                encode_cbor(item, out);
            }
            out.push(0xFF);
        }
    }
}

/// Decodes one CBOR data item (trailing bytes are an error).
pub fn decode_cbor(bytes: &[u8]) -> Result<Value, String> {
    let mut r = Reader { b: bytes, pos: 0 };
    let v = cbor_item(&mut r, 0)?.ok_or("unexpected break")?;
    if r.pos != bytes.len() {
        return Err(format!("{} trailing bytes", bytes.len() - r.pos));
    }
    Ok(v)
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, String> {
        let b = *self.b.get(self.pos).ok_or("unexpected end of input")?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        if self.b.len() - self.pos < n {
            return Err(format!("needed {n} bytes, {} left", self.b.len() - self.pos));
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn remaining(&self) -> usize {
        self.b.len() - self.pos
    }
}

/// The argument of a head byte (`None` for an indefinite length).
fn cbor_arg(r: &mut Reader<'_>, info: u8) -> Result<Option<u64>, String> {
    Ok(Some(match info {
        0..=23 => info as u64,
        24 => r.byte()? as u64,
        25 => u16::from_be_bytes(r.take(2)?.try_into().expect("2 bytes")) as u64,
        26 => u32::from_be_bytes(r.take(4)?.try_into().expect("4 bytes")) as u64,
        27 => u64::from_be_bytes(r.take(8)?.try_into().expect("8 bytes")),
        31 => return Ok(None),
        _ => return Err(format!("reserved additional information {info}")),
    }))
}

fn cbor_len(n: u64, r: &Reader<'_>) -> Result<usize, String> {
    usize::try_from(n).ok().filter(|n| *n <= r.remaining()).ok_or_else(|| format!("length {n} exceeds the {} bytes left", r.remaining()))
}

/// A byte or text string (definite, or indefinite as definite chunks of the same major type).
fn cbor_bytes(r: &mut Reader<'_>, major: u8, info: u8) -> Result<Vec<u8>, String> {
    match cbor_arg(r, info)? {
        Some(n) => {
            let n = cbor_len(n, r)?;
            Ok(r.take(n)?.to_vec())
        }
        None => {
            let mut out = Vec::new();
            loop {
                let b = r.byte()?;
                if b == 0xFF {
                    return Ok(out);
                }
                if b >> 5 != major {
                    return Err("indefinite string chunk of another type".into());
                }
                match cbor_arg(r, b & 0x1F)? {
                    Some(n) => {
                        let n = cbor_len(n, r)?;
                        out.extend_from_slice(r.take(n)?);
                    }
                    None => return Err("nested indefinite string chunk".into()),
                }
            }
        }
    }
}

fn half_to_f64(h: u16) -> f64 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = (h >> 10) & 0x1F;
    let frac = (h & 0x3FF) as f64;
    match exp {
        0 => sign * frac * 2f64.powi(-24),
        31 => {
            if frac == 0.0 {
                sign * f64::INFINITY
            } else {
                f64::NAN
            }
        }
        e => sign * (1.0 + frac / 1024.0) * 2f64.powi(e as i32 - 15),
    }
}

fn big_from_bytes(negative: bool, magnitude: &[u8]) -> Value {
    let digits = &magnitude[magnitude.iter().position(|b| *b != 0).unwrap_or(magnitude.len())..];
    if digits.len() <= 16 {
        let mut n: u128 = 0;
        for &b in digits {
            n = (n << 8) | b as u128;
        }
        if let Ok(i) = i128::try_from(n) {
            return Value::Int(if negative { -1 - i } else { i });
        }
    }
    let mut f = 0f64;
    for &b in digits {
        f = f * 256.0 + b as f64;
    }
    Value::Float(if negative { -1.0 - f } else { f })
}

/// One item; `None` is a break byte (only valid inside indefinite containers).
fn cbor_item(r: &mut Reader<'_>, depth: usize) -> Result<Option<Value>, String> {
    if depth > MAX_DEPTH {
        return Err("nested too deeply".into());
    }
    let b = r.byte()?;
    let (major, info) = (b >> 5, b & 0x1F);
    Ok(Some(match major {
        0 => Value::Int(cbor_arg(r, info)?.ok_or("indefinite integer")? as i128),
        1 => Value::Int(-1 - cbor_arg(r, info)?.ok_or("indefinite integer")? as i128),
        2 => base64(&cbor_bytes(r, 2, info)?),
        3 => Value::Str(String::from_utf8(cbor_bytes(r, 3, info)?).map_err(|_| "invalid UTF-8 in a text string")?),
        4 => {
            let mut items = Vec::new();
            match cbor_arg(r, info)? {
                Some(n) => {
                    // Every item takes at least one byte.
                    let n = cbor_len(n, r)?;
                    items.reserve(n);
                    for _ in 0..n {
                        items.push(cbor_item(r, depth + 1)?.ok_or("unexpected break in a definite array")?);
                    }
                }
                None => {
                    while let Some(item) = cbor_item(r, depth + 1)? {
                        items.push(item);
                    }
                }
            }
            Value::List(items)
        }
        5 => {
            let mut m = Map::new();
            let mut entry = |r: &mut Reader<'_>, key: Value| -> Result<(), String> {
                let key = match key {
                    Value::Str(s) => s,
                    other => other.java_string(),
                };
                let v = cbor_item(r, depth + 1)?.ok_or("unexpected break after a map key")?;
                insert_unique(&mut m, key, v)
            };
            match cbor_arg(r, info)? {
                Some(n) => {
                    let n = cbor_len(n.saturating_mul(2), r)? / 2;
                    for _ in 0..n {
                        let k = cbor_item(r, depth + 1)?.ok_or("unexpected break in a definite map")?;
                        entry(r, k)?;
                    }
                }
                None => {
                    while let Some(k) = cbor_item(r, depth + 1)? {
                        entry(r, k)?;
                    }
                }
            }
            Value::Map(m)
        }
        6 => {
            let tag = cbor_arg(r, info)?.ok_or("indefinite tag")?;
            let inner = cbor_item(r, depth + 1)?.ok_or("unexpected break after a tag")?;
            match (tag, &inner) {
                (2 | 3, Value::Str(b64)) => {
                    let bytes = base64::engine::general_purpose::STANDARD.decode(b64).map_err(|e| e.to_string())?;
                    big_from_bytes(tag == 3, &bytes)
                }
                // Decimal fraction [exponent, mantissa]: mantissa * 10^exponent.
                (4, Value::List(parts)) if parts.len() == 2 => match (&parts[0], &parts[1]) {
                    (Value::Int(e), m) if m.is_number() => {
                        let e = i32::try_from(*e).map_err(|_| "decimal exponent out of range")?;
                        Value::Float(format!("{}e{e}", m.java_string()).parse::<f64>().map_err(|e| e.to_string())?)
                    }
                    _ => return Err("malformed decimal fraction".into()),
                },
                _ => inner,
            }
        }
        _ => match info {
            20 => Value::Bool(false),
            21 => Value::Bool(true),
            22 | 23 => Value::Null,
            24 => Value::Int(r.byte()? as i128),
            25 => Value::Float(half_to_f64(u16::from_be_bytes(r.take(2)?.try_into().expect("2 bytes")))),
            26 => Value::Float(f32::from_be_bytes(r.take(4)?.try_into().expect("4 bytes")) as f64),
            27 => Value::Float(f64::from_be_bytes(r.take(8)?.try_into().expect("8 bytes"))),
            31 => return Ok(None),
            0..=19 => Value::Int(info as i128),
            _ => return Err(format!("reserved simple value {info}")),
        },
    }))
}

// ---------------------------------------------------------------------------------------------
// Smile

const SMILE_HEADER: [u8; 3] = *b":)\n";
/// Version 0, shared property names on, shared string values off, raw binary off.
const SMILE_FLAGS: u8 = 0x01;
const MAX_SHARED: usize = 1024;

/// Writes Smile documents with Jackson's default back-references for property names.
#[derive(Default)]
struct SmileWriter {
    names: std::collections::HashMap<String, usize>,
    count: usize,
}

fn zigzag32(i: i32) -> u32 {
    ((i << 1) ^ (i >> 31)) as u32
}

fn zigzag64(i: i64) -> u64 {
    ((i << 1) ^ (i >> 63)) as u64
}

/// A Smile VInt: 7-bit groups, most significant first; the last byte has the high bit set and
/// carries 6 bits.
fn smile_vint(v: u64, out: &mut Vec<u8>) {
    let mut groups = [0u8; 11];
    let mut n = 0;
    groups[n] = 0x80 | (v & 0x3F) as u8;
    n += 1;
    let mut rest = v >> 6;
    while rest != 0 {
        groups[n] = (rest & 0x7F) as u8;
        n += 1;
        rest >>= 7;
    }
    for i in (0..n).rev() {
        out.push(groups[i]);
    }
}

/// Jackson's 7-bit encoding of binary data: every 7 bytes become 8 bytes of 7 bits; a tail of
/// k bytes becomes k bytes of 7 bits plus one byte with the last k bits, right-aligned.
fn seven_bit_encode(data: &[u8], out: &mut Vec<u8>) {
    for chunk in data.chunks(7) {
        let k = chunk.len();
        let mut acc: u64 = 0;
        for &b in chunk {
            acc = (acc << 8) | b as u64;
        }
        let bits = 8 * k;
        for g in 0..k {
            out.push(((acc >> (bits - 7 * (g + 1))) & 0x7F) as u8);
        }
        let last_bits = bits - 7 * k;
        out.push((acc & ((1u64 << last_bits) - 1)) as u8);
    }
}

/// The inverse of [`seven_bit_encode`] for `len` decoded bytes.
fn seven_bit_decode(r: &mut Reader<'_>, len: usize) -> Result<Vec<u8>, String> {
    // 8 encoded bytes per 7 decoded: bound the allocation by what is there.
    if len / 7 * 8 > r.remaining() {
        return Err(format!("7-bit binary of {len} bytes exceeds the {} bytes left", r.remaining()));
    }
    let mut out = Vec::with_capacity(len);
    let mut left = len;
    while left > 0 {
        let k = left.min(7);
        let mut acc: u64 = 0;
        for _ in 0..k {
            acc = (acc << 7) | (r.byte()? & 0x7F) as u64;
        }
        acc = (acc << k) | (r.byte()? as u64 & ((1u64 << k) - 1));
        for i in (0..k).rev() {
            out.push((acc >> (8 * i)) as u8);
        }
        left -= k;
    }
    Ok(out)
}

impl SmileWriter {
    fn document(&mut self, v: &Value, out: &mut Vec<u8>) {
        out.extend_from_slice(&SMILE_HEADER);
        out.push(SMILE_FLAGS);
        self.value(v, out);
    }

    fn value(&mut self, v: &Value, out: &mut Vec<u8>) {
        match v {
            Value::Null => out.push(0x21),
            Value::Bool(b) => out.push(if *b { 0x23 } else { 0x22 }),
            Value::Int(i) => smile_int(*i, out),
            Value::Float(f) => {
                out.push(0x29);
                let bits = f.to_bits();
                for k in 0..10 {
                    out.push(((bits >> (7 * (9 - k))) & 0x7F) as u8);
                }
            }
            Value::Str(s) => {
                let len = s.len();
                let ascii = s.is_ascii();
                if len == 0 {
                    out.push(0x20);
                } else if ascii && len <= 64 {
                    out.push(0x40 + (len - 1) as u8);
                    out.extend_from_slice(s.as_bytes());
                } else if !ascii && (2..=65).contains(&len) {
                    out.push(0x80 + (len - 2) as u8);
                    out.extend_from_slice(s.as_bytes());
                } else {
                    out.push(if ascii { 0xE0 } else { 0xE4 });
                    out.extend_from_slice(s.as_bytes());
                    out.push(0xFC);
                }
            }
            Value::List(items) => {
                out.push(0xF8);
                for item in items {
                    self.value(item, out);
                }
                out.push(0xF9);
            }
            Value::Map(m) => {
                out.push(0xFA);
                for (k, item) in m {
                    self.key(k, out);
                    self.value(item, out);
                }
                out.push(0xFB);
            }
        }
    }

    fn key(&mut self, k: &str, out: &mut Vec<u8>) {
        if k.is_empty() {
            out.push(0x20);
            return;
        }
        if let Some(&ix) = self.names.get(k) {
            if ix < 64 {
                out.push(0x40 + ix as u8);
            } else {
                out.extend_from_slice(&[0x30 + (ix >> 8) as u8, ix as u8]);
            }
            return;
        }
        let len = k.len();
        if k.is_ascii() && len <= 64 {
            out.push(0x80 + (len - 1) as u8);
            out.extend_from_slice(k.as_bytes());
        } else if !k.is_ascii() && (2..=56).contains(&len) {
            out.push(0xC0 + (len - 2) as u8);
            out.extend_from_slice(k.as_bytes());
        } else {
            out.push(0x34);
            out.extend_from_slice(k.as_bytes());
            out.push(0xFC);
        }
        // `SmileGenerator._addSeenName`: the table restarts when full; indices whose low byte
        // would be 0xFE or 0xFF are counted but never referenced.
        if self.count == MAX_SHARED {
            self.names.clear();
            self.count = 0;
        }
        if (self.count & 0xFF) < 0xFE {
            self.names.insert(k.to_owned(), self.count);
        }
        self.count += 1;
    }
}

fn smile_int(i: i128, out: &mut Vec<u8>) {
    if (-16..=15).contains(&i) {
        out.push(0xC0 + zigzag32(i as i32) as u8);
    } else if let Ok(v) = i32::try_from(i) {
        out.push(0x24);
        smile_vint(zigzag32(v) as u64, out);
    } else if let Ok(v) = i64::try_from(i) {
        out.push(0x25);
        smile_vint(zigzag64(v), out);
    } else {
        // BigInteger: minimal big-endian two's complement, 7-bit encoded.
        let bytes = i.to_be_bytes();
        let mut start = 0;
        while start < 15 {
            let (b, next) = (bytes[start], bytes[start + 1]);
            if (b == 0 && next & 0x80 == 0) || (b == 0xFF && next & 0x80 != 0) {
                start += 1;
            } else {
                break;
            }
        }
        out.push(0x26);
        smile_vint((16 - start) as u64, out);
        seven_bit_encode(&bytes[start..], out);
    }
}

/// Decodes one Smile document (header required, as Elasticsearch's parser requires it).
pub fn decode_smile(bytes: &[u8]) -> Result<Value, String> {
    let mut r = Reader { b: bytes, pos: 0 };
    if r.take(3).map_err(|_| "missing the Smile header")? != SMILE_HEADER {
        return Err("missing the Smile header".into());
    }
    let flags = r.byte()?;
    if flags >> 4 != 0 {
        return Err(format!("unsupported Smile version {}", flags >> 4));
    }
    let mut st = SmileState {
        names: if flags & 0x01 != 0 { Some(Vec::new()) } else { None },
        values: if flags & 0x02 != 0 { Some(Vec::new()) } else { None },
    };
    let v = smile_value(&mut r, &mut st, 0)?.ok_or("empty document")?;
    // An end marker (0xFF) may follow.
    if r.remaining() > 0 && !(r.remaining() == 1 && bytes[r.pos] == 0xFF) {
        return Err(format!("{} trailing bytes", r.remaining()));
    }
    Ok(v)
}

struct SmileState {
    names: Option<Vec<String>>,
    values: Option<Vec<String>>,
}

fn remember(table: &mut Option<Vec<String>>, s: &str) {
    if let Some(t) = table {
        // `_expandSeenNames`: a full table starts over.
        if t.len() == MAX_SHARED {
            t.clear();
        }
        t.push(s.to_owned());
    }
}

fn shared(table: &Option<Vec<String>>, index: usize, what: &str) -> Result<String, String> {
    table.as_ref().and_then(|t| t.get(index)).cloned().ok_or_else(|| format!("invalid shared {what} reference {index}"))
}

fn read_vint(r: &mut Reader<'_>) -> Result<u64, String> {
    let mut v: u64 = 0;
    for _ in 0..11 {
        let b = r.byte()?;
        if b & 0x80 != 0 {
            return Ok((v << 6) | (b & 0x3F) as u64);
        }
        v = (v << 7) | b as u64;
    }
    Err("VInt too long".into())
}

fn read_until_fc<'a>(r: &mut Reader<'a>) -> Result<&'a [u8], String> {
    let rest = &r.b[r.pos..];
    let end = rest.iter().position(|b| *b == 0xFC).ok_or("unterminated long string")?;
    r.pos += end + 1;
    Ok(&rest[..end])
}

fn utf8(bytes: &[u8]) -> Result<String, String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| "invalid UTF-8".to_owned())
}

fn smile_big_integer(r: &mut Reader<'_>) -> Result<Value, String> {
    let len = usize::try_from(read_vint(r)?).map_err(|_| "length out of range")?;
    let bytes = seven_bit_decode(r, len)?;
    let negative = bytes.first().is_some_and(|b| b & 0x80 != 0);
    if bytes.len() <= 16 {
        let mut full = [if negative { 0xFF } else { 0 }; 16];
        full[16 - bytes.len()..].copy_from_slice(&bytes);
        return Ok(Value::Int(i128::from_be_bytes(full)));
    }
    // Wider than 128 bits: as a double (two's complement → magnitude).
    let mut mag = bytes.clone();
    if negative {
        for b in &mut mag {
            *b = !*b;
        }
    }
    let f = mag.iter().fold(0f64, |acc, b| acc * 256.0 + *b as f64);
    Ok(Value::Float(if negative { -(f + 1.0) } else { f }))
}

/// One value; `None` for an end-of-array marker (only valid inside arrays).
fn smile_value(r: &mut Reader<'_>, st: &mut SmileState, depth: usize) -> Result<Option<Value>, String> {
    if depth > MAX_DEPTH {
        return Err("nested too deeply".into());
    }
    let ch = r.byte()?;
    Ok(Some(match ch {
        0x01..=0x1F => Value::Str(shared(&st.values, (ch - 1) as usize, "string value")?),
        0x20 => Value::Str(String::new()),
        0x21 => Value::Null,
        0x22 => Value::Bool(false),
        0x23 => Value::Bool(true),
        0x24 => {
            let z = read_vint(r)?;
            let z = u32::try_from(z).map_err(|_| "32-bit VInt out of range")?;
            Value::Int(((z >> 1) as i32 ^ -((z & 1) as i32)) as i128)
        }
        0x25 => {
            let z = read_vint(r)?;
            Value::Int(((z >> 1) as i64 ^ -((z & 1) as i64)) as i128)
        }
        0x26 => smile_big_integer(r)?,
        0x28 => {
            let mut bits: u32 = 0;
            for _ in 0..5 {
                bits = (bits << 7) | (r.byte()? & 0x7F) as u32;
            }
            Value::Float(f32::from_bits(bits) as f64)
        }
        0x29 => {
            let mut bits: u64 = 0;
            for _ in 0..10 {
                bits = (bits << 7) | (r.byte()? & 0x7F) as u64;
            }
            Value::Float(f64::from_bits(bits))
        }
        0x2A => {
            // BigDecimal: zigzag scale, then the unscaled value as a big integer.
            let z = u32::try_from(read_vint(r)?).map_err(|_| "scale out of range")?;
            let scale = (z >> 1) as i32 ^ -((z & 1) as i32);
            let unscaled = smile_big_integer(r)?;
            Value::Float(format!("{}e{}", unscaled.java_string(), -(scale as i64)).parse::<f64>().map_err(|e| e.to_string())?)
        }
        0x40..=0x7F => {
            let s = utf8(r.take(((ch & 0x3F) + 1) as usize)?)?;
            remember(&mut st.values, &s);
            Value::Str(s)
        }
        0x80..=0xBF => {
            let s = utf8(r.take(((ch & 0x3F) + 2) as usize)?)?;
            remember(&mut st.values, &s);
            Value::Str(s)
        }
        0xC0..=0xDF => {
            let z = (ch & 0x1F) as i32;
            Value::Int(((z >> 1) ^ -(z & 1)) as i128)
        }
        0xE0 | 0xE4 => Value::Str(utf8(read_until_fc(r)?)?),
        0xE8 => {
            let len = usize::try_from(read_vint(r)?).map_err(|_| "length out of range")?;
            base64(&seven_bit_decode(r, len)?)
        }
        0xEC..=0xEF => {
            let index = (((ch & 0x03) as usize) << 8) | r.byte()? as usize;
            Value::Str(shared(&st.values, index, "string value")?)
        }
        0xF8 => {
            let mut items = Vec::new();
            while let Some(v) = smile_value(r, st, depth + 1)? {
                items.push(v);
            }
            Value::List(items)
        }
        0xF9 => return Ok(None),
        0xFA => {
            let mut m = Map::new();
            loop {
                let k = r.byte()?;
                let key = match k {
                    0xFB => break,
                    0x20 => String::new(),
                    0x30..=0x33 => {
                        let index = (((k & 0x03) as usize) << 8) | r.byte()? as usize;
                        shared(&st.names, index, "name")?
                    }
                    0x34 => {
                        let s = utf8(read_until_fc(r)?)?;
                        remember(&mut st.names, &s);
                        s
                    }
                    0x40..=0x7F => shared(&st.names, (k & 0x3F) as usize, "name")?,
                    0x80..=0xBF => {
                        let s = utf8(r.take(((k & 0x3F) + 1) as usize)?)?;
                        remember(&mut st.names, &s);
                        s
                    }
                    0xC0..=0xF7 => {
                        let s = utf8(r.take(((k & 0x3F) + 2) as usize)?)?;
                        remember(&mut st.names, &s);
                        s
                    }
                    other => return Err(format!("invalid key token 0x{other:02X}")),
                };
                let v = smile_value(r, st, depth + 1)?.ok_or("end of array in an object")?;
                insert_unique(&mut m, key, v)?;
            }
            Value::Map(m)
        }
        0xFD => {
            let len = usize::try_from(read_vint(r)?).map_err(|_| "length out of range")?;
            base64(r.take(len)?)
        }
        other => return Err(format!("invalid value token 0x{other:02X}")),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::json;

    fn sample() -> Value {
        json(
            r#"{"a": 1, "b": -17, "big": 12345678901234, "huge": 123456789012345678901234567890, "neg_huge": -123456789012345678901234567890,
                "f": 0.1, "e": 1e21, "tiny": 1.5e-7, "t": true, "n": null, "s": "line\nbreak \"q\" \\ é ✓ \u0001",
                "long": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
                "list": [1, [2, [3, {"x": []}]], {"k": {}, "": "empty key"}, "", -0.0],
                "nested": {"deeper": {"deepest": [{"a": 1, "b": [true, false]}]}},
                "éé": "unicode key", "a2": 2}"#,
        )
    }

    #[test]
    fn round_trips_through_every_format() {
        let mut v = sample();
        if let Value::Map(m) = &mut v {
            for (i, n) in [u64::MAX as i128, i64::MIN as i128, 255, -16, 15, 16, -17, 24, -25, 65536].into_iter().enumerate() {
                m.insert(format!("int{i}"), Value::Int(n));
            }
        }
        for f in [Format::Json, Format::Yaml, Format::Cbor, Format::Smile] {
            let mut out = Vec::new();
            f.encode(&v, &mut out);
            let back = f.decode(&out).unwrap_or_else(|e| panic!("{f:?}: {e}\n{}", String::from_utf8_lossy(&out)));
            assert_eq!(back, v, "{f:?}: {}", String::from_utf8_lossy(&out));
        }
    }

    #[test]
    fn yaml_shapes() {
        let mut out = Vec::new();
        Format::Yaml.encode(&json(r#"{"a": {"b": [1, {"c": 2.5, "d": [[]]}]}, "e": 1e21, "f": [[1, 2], [3]]}"#), &mut out);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(
            text,
            "---\n\"a\":\n  \"b\":\n    - 1\n    - \"c\": 2.5\n      \"d\":\n        - []\n\"e\": 1.0e+21\n\"f\":\n  - - 1\n    - 2\n  - - 3\n"
        );
        let mut out = Vec::new();
        Format::Yaml.encode(&json("{}"), &mut out);
        assert_eq!(out, b"--- {}\n");
    }

    #[test]
    fn cbor_matches_the_rfc_examples() {
        // RFC 8949 appendix A.
        let cases: &[(&[u8], Value)] = &[
            (&[0x00], Value::Int(0)),
            (&[0x18, 0x64], Value::Int(100)),
            (&[0x1b, 0, 0, 0, 0xe8, 0xd4, 0xa5, 0x10, 0x00], Value::Int(1_000_000_000_000)),
            (&[0xc2, 0x49, 0x01, 0, 0, 0, 0, 0, 0, 0, 0], Value::Int(18446744073709551616)),
            (&[0x3b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff], Value::Int(-18446744073709551616)),
            (&[0x39, 0x03, 0xe7], Value::Int(-1000)),
            (&[0xf9, 0x3c, 0x00], Value::Float(1.0)),
            (&[0xf9, 0x7b, 0xff], Value::Float(65504.0)),
            (&[0xf9, 0x00, 0x01], Value::Float(5.960464477539063e-8)),
            (&[0xfa, 0x47, 0xc3, 0x50, 0x00], Value::Float(100000.0)),
            (&[0xfb, 0x3f, 0xf1, 0x99, 0x99, 0x99, 0x99, 0x99, 0x9a], Value::Float(1.1)),
            (&[0xf6], Value::Null),
            (&[0xf7], Value::Null),
            (&[0x7f, 0x65, b's', b't', b'r', b'e', b'a', 0x64, b'm', b'i', b'n', b'g', 0xff], Value::Str("streaming".into())),
            (&[0x5f, 0x42, 1, 2, 0x43, 3, 4, 5, 0xff], Value::Str("AQIDBAU=".into())),
            (&[0x9f, 0x01, 0x82, 0x02, 0x03, 0x9f, 0x04, 0x05, 0xff, 0xff], json("[1, [2, 3], [4, 5]]")),
            (&[0xbf, 0x61, b'a', 0x01, 0x61, b'b', 0x9f, 0x02, 0x03, 0xff, 0xff], json(r#"{"a": 1, "b": [2, 3]}"#)),
            (&[0xa2, 0x01, 0x02, 0x03, 0x04], json(r#"{"1": 2, "3": 4}"#)),
            (&[0xc4, 0x82, 0x21, 0x19, 0x6a, 0xb3], Value::Float(273.15)),
            (&[0xc1, 0x1a, 0x51, 0x4b, 0x67, 0xb0], Value::Int(1363896240)),
        ];
        for (bytes, want) in cases {
            assert_eq!(&decode_cbor(bytes).unwrap(), want, "{bytes:02x?}");
        }
        // Beyond 64 bits: bignum tags, both ways.
        for i in [i128::MAX, i128::MIN, u64::MAX as i128 + 1, -(u64::MAX as i128) - 2] {
            let mut out = Vec::new();
            encode_cbor(&Value::Int(i), &mut out);
            assert_eq!(decode_cbor(&out).unwrap(), Value::Int(i));
        }
        let half_nan = decode_cbor(&[0xf9, 0x7e, 0x00]).unwrap();
        assert!(matches!(half_nan, Value::Float(f) if f.is_nan()));
        let mut out = Vec::new();
        encode_cbor(&json(r#"{"a": [1, -1000]}"#), &mut out);
        assert_eq!(out, [0xbf, 0x61, b'a', 0x9f, 0x01, 0x39, 0x03, 0xe7, 0xff, 0xff]);
    }

    #[test]
    fn smile_matches_jackson() {
        // What Jackson's SmileGenerator writes for {"a":1,"b":[true,null],"a2":"xyz"} with its
        // defaults (header with shared names on).
        let mut m = Map::new();
        m.insert("a".into(), Value::Int(1));
        m.insert("b".into(), json("[true, null]"));
        m.insert("a2".into(), Value::Str("xyz".into()));
        let mut out = Vec::new();
        Format::Smile.encode(&Value::Map(m), &mut out);
        assert_eq!(out, [b':', b')', b'\n', 0x01, 0xFA, 0x80, b'a', 0xC2, 0x80, b'b', 0xF8, 0x23, 0x21, 0xF9, 0x81, b'a', b'2', 0x42, b'x', b'y', b'z', 0xFB]);
        // A back-reference and a 32-bit VInt: {"a":{"a":300}}.
        let mut out = Vec::new();
        Format::Smile.encode(&json(r#"{"a": {"a": 300}}"#), &mut out);
        // 300 → zigzag 600 → 0b1001_011000 → [0x09, 0x80 | 0x18].
        assert_eq!(out[4..], [0xFA, 0x80, b'a', 0xFA, 0x40, 0x24, 0x09, 0x98, 0xFB, 0xFB]);
        // Doubles: 1.0 is 0x3FF0000000000000 in ten 7-bit groups.
        let mut out = Vec::new();
        Format::Smile.encode(&Value::Float(1.0), &mut out);
        assert_eq!(out[4..], [0x29, 0x00, 0x3F, 0x78, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(decode_smile(&out).unwrap(), Value::Float(1.0));
    }

    #[test]
    fn smile_name_table_restarts_when_full() {
        // 1100 distinct keys, then all of them again: the second round references the second
        // table (after the restart at 1024) or writes names afresh, and decodes back.
        let mut m = Map::new();
        for i in 0..1100 {
            m.insert(format!("k{i}"), Value::Int(i));
        }
        let v = Value::List(vec![Value::Map(m.clone()), Value::Map(m)]);
        let mut out = Vec::new();
        Format::Smile.encode(&v, &mut out);
        assert_eq!(decode_smile(&out).unwrap(), v);
    }

    #[test]
    fn smile_shared_values_long_strings_and_binary() {
        // Header with shared values: "abc" then a short reference (0x01 = index 0) and a long one.
        let doc = [b':', b')', b'\n', 0x03, 0xF8, 0x42, b'a', b'b', b'c', 0x01, 0xEC, 0x00, 0xE0, b'l', b'o', b'n', b'g', 0xFC, 0xFD, 0x82, 1, 2, 0xF9];
        assert_eq!(decode_smile(&doc).unwrap(), json(r#"["abc", "abc", "abc", "long", "AQI="]"#));
        // 7-bit binary of 3 bytes: 0xE8, VInt 3, 4 bytes.
        let mut enc = Vec::new();
        seven_bit_encode(&[0xFF, 0x00, 0xAB], &mut enc);
        let mut doc = vec![b':', b')', b'\n', 0x00, 0xE8, 0x83];
        doc.extend_from_slice(&enc);
        assert_eq!(decode_smile(&doc).unwrap(), Value::Str("/wCr".into()));
        for len in 0..30 {
            let data: Vec<u8> = (0..len).map(|i| (i * 37 + 11) as u8).collect();
            let mut enc = Vec::new();
            seven_bit_encode(&data, &mut enc);
            assert!(enc.iter().all(|b| *b < 0x80));
            let mut r = Reader { b: &enc, pos: 0 };
            assert_eq!(seven_bit_decode(&mut r, len).unwrap(), data);
            assert_eq!(r.pos, enc.len());
        }
        // Big integers both ways, and a BigDecimal (unscaled 12345, scale 2).
        for i in [i64::MAX as i128 + 1, i64::MIN as i128 - 1, i128::MAX, i128::MIN] {
            let mut out = Vec::new();
            Format::Smile.encode(&Value::Int(i), &mut out);
            assert_eq!(decode_smile(&out).unwrap(), Value::Int(i));
        }
        let mut doc = vec![b':', b')', b'\n', 0x00, 0x2A, 0x84, 0x82];
        seven_bit_encode(&12345u16.to_be_bytes(), &mut doc);
        assert_eq!(decode_smile(&doc).unwrap(), Value::Float(123.45));
    }

    #[test]
    fn decoders_reject_garbage_without_panicking() {
        let v = sample();
        for f in [Format::Cbor, Format::Smile] {
            let mut good = Vec::new();
            f.encode(&v, &mut good);
            for cut in 0..good.len() {
                let _ = f.decode(&good[..cut]);
            }
            for i in 0..good.len() {
                let mut bad = good.clone();
                bad[i] ^= 0x5A;
                let _ = f.decode(&bad);
            }
        }
        // Lengths beyond the input are errors, not allocations.
        assert!(decode_cbor(&[0x9b, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).is_err());
        assert!(decode_cbor(&[0x5b, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]).is_err());
        assert!(decode_smile(&[b':', b')', b'\n', 0, 0xE8, 0x7F, 0x7F, 0x7F, 0x7F, 0x7F, 0xBF]).is_err());
        let deep = [0x9fu8; 2000];
        assert!(decode_cbor(&deep).unwrap_err().contains("deeply"));
        assert!(decode_smile(b"nope").is_err());
    }

    #[test]
    fn media_types() {
        assert_eq!(MediaType::parse("vnd-smile").unwrap().mime(), "application/vnd.elasticsearch+smile");
        assert_eq!(MediaType::parse("cbor").unwrap().mime(), "application/cbor");
        assert_eq!(MediaType::JSON.request_content_type(), "application/json; charset=UTF-8");
        assert_eq!(MediaType::parse("vnd-smile").unwrap().request_content_type(), "application/vnd.elasticsearch+smile; charset=UTF-8");
        assert!(MediaType::parse("xml").is_none());
        assert_eq!(Format::from_content_type("application/vnd.elasticsearch+cbor;compatible-with=9"), Some(Format::Cbor));
        assert_eq!(Format::from_content_type("application/smile"), Some(Format::Smile));
        assert_eq!(Format::from_content_type("application/yaml; charset=UTF-8"), Some(Format::Yaml));
        assert_eq!(Format::from_content_type("text/plain; charset=UTF-8"), None);
        assert_eq!(Format::Smile.bulk_separator(), 0xFF);
    }
}
