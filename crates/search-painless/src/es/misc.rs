//! Smaller Elasticsearch APIs: `Debug.explain`, `Json.load/dump` (Jackson's output format),
//! `String.sha1/sha256/sha512`, `CIDR`, `IPAddress`, `java.nio.ByteBuffer`, and the objects
//! similarity, interval and analysis scripts receive.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::rc::Rc;

use super::{es, es_error, es_iae, es_value, EsObj};
use crate::lookup::Builder;
use crate::types::{cls, ClassId};
use crate::value::{Obj, Value};
use crate::vm::{Throw, Thrown, Vm, VR};

// ------------------------------------------------------------------------------ Debug

fn debug_explain(vm: &mut Vm, a: &mut [Value]) -> VR {
    let v = &a[0];
    let mut t = Throw::new(cls::THROWABLE, None);
    t.catchable = false;
    t.es_type = Some("painless_explain_error");
    let (to_string, java, painless) = if v.is_null() {
        ("null".to_string(), None, None)
    } else {
        let c = v.class();
        let java = crate::lookup::java_binary_name(c).to_string();
        let painless = if vm.reg.class(c).short.is_some() || !is_internal(c) { Some(crate::lookup::class_name(c).to_string()) } else { None };
        (v.to_java_string(), Some(java), painless)
    };
    if let Some(p) = painless {
        t.metadata.push(("painless_class".into(), p));
    }
    t.metadata.push(("to_string".into(), to_string));
    if let Some(j) = java {
        t.metadata.push(("java_class".into(), j));
    }
    Err(t)
}

/// Implementation classes scripts cannot name (no `painless_class` in explanations).
fn is_internal(c: ClassId) -> bool {
    let name = crate::lookup::class_name(c);
    name.ends_with(".Impl") || name.contains("Iterator") || c == cls::DOC_MAP || c == cls::CTX_MAP || c == cls::DENSE_VECTOR_EMPTY
}

// ------------------------------------------------------------------------------ Json

/// `Json.load(String)`: maps are `HashMap`s, arrays `ArrayList`s, numbers `Integer`, `Long`,
/// `BigInteger` or `Double`.
pub fn json_load(s: &str) -> VR {
    json_parse(s, cls::HASH_MAP)
}

/// Parses JSON into script values with the given map class.
pub fn json_parse(s: &str, map_class: ClassId) -> VR {
    match crate::jackson::load(s, map_class) {
        Ok(Some(v)) => Ok(v),
        Ok(None) => Err(crate::vm::npe(
            "Cannot invoke \"org.elasticsearch.xcontent.XContentParser$Token.ordinal()\" because the return value of \"org.elasticsearch.xcontent.XContentParser.nextToken()\" is null",
        )),
        Err(e) => Err(e.thrown(&[])),
    }
}

fn json_escape(s: &str, out: &mut String) {
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04X}", c as u32)),
            c if (c as u32) >= 0x10000 => {
                let mut buf = [0u16; 2];
                for u in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{:04X}", u));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Writes a value the way `XContentBuilder.value(Object)` does (Jackson's generator; `pretty`
/// is its default pretty printer with two-space indents).
pub fn json_dump(vm: &mut Vm, v: &Value, pretty: bool) -> Result<String, Thrown> {
    let mut out = String::new();
    let mut chain = Vec::new();
    dump_value(vm, v, pretty, 0, &mut out, &mut chain)?;
    Ok(out)
}

/// Jackson's `StreamWriteConstraints` failure for a container one level deeper than allowed.
/// `chain` holds whether each enclosing container (outermost first) is an object; the frames
/// are those of `XContentBuilder` recursing through them, cut at the JVM's 1,024 frames.
fn nesting_error(chain: &[bool]) -> Thrown {
    const XB: &str = "org.elasticsearch.xcontent@9.5.5/org.elasticsearch.xcontent.XContentBuilder";
    let mut frames = vec![
        "com.fasterxml.jackson.core@2.21.7/com.fasterxml.jackson.core.StreamWriteConstraints._constructException(StreamWriteConstraints.java:177)".to_string(),
        "com.fasterxml.jackson.core@2.21.7/com.fasterxml.jackson.core.StreamWriteConstraints.validateNestingDepth(StreamWriteConstraints.java:162)".to_string(),
    ];
    let innermost_object = chain.last().copied().unwrap_or(false);
    if innermost_object {
        frames.push("com.fasterxml.jackson.core@2.21.7/com.fasterxml.jackson.core.json.UTF8JsonGenerator.writeStartObject(UTF8JsonGenerator.java:380)".into());
        frames.push("org.elasticsearch.xcontent.impl@9.5.5/org.elasticsearch.xcontent.provider.json.JsonXContentGenerator.writeStartObject(JsonXContentGenerator.java:151)".into());
        frames.push(format!("{XB}.startObject(XContentBuilder.java:339)"));
        frames.push(format!("{XB}.map(XContentBuilder.java:1165)"));
    } else {
        frames.push("com.fasterxml.jackson.core@2.21.7/com.fasterxml.jackson.core.json.UTF8JsonGenerator.writeStartArray(UTF8JsonGenerator.java:315)".into());
        frames.push("org.elasticsearch.xcontent.impl@9.5.5/org.elasticsearch.xcontent.provider.json.JsonXContentGenerator.writeStartArray(JsonXContentGenerator.java:172)".into());
        frames.push(format!("{XB}.startArray(XContentBuilder.java:359)"));
        frames.push(format!("{XB}.value(XContentBuilder.java:1196)"));
    }
    // each level: the dispatch on the child's type, then the parent's loop over its children
    for pair in chain.windows(2).rev() {
        if frames.len() >= 1024 {
            break;
        }
        let (parent_object, child_object) = (pair[0], pair[1]);
        frames.push(format!("{XB}.unknownValue(XContentBuilder.java:{})", if child_object { 1008 } else { 1010 }));
        frames.push(if parent_object { format!("{XB}.map(XContentBuilder.java:1170)") } else { format!("{XB}.value(XContentBuilder.java:1199)") });
    }
    let mut t = Throw::new(cls::RUNTIME_EXCEPTION, Some(crate::value::nesting_depth_message(chain.len())));
    t.es_type = Some("stream_constraints_exception");
    if frames.len() >= 1024 {
        // the script's own frame is beyond the cut: the error has no script position
        frames.truncate(1024);
        t.offset = Some(crate::vm::NO_SCRIPT_FRAME);
    }
    t.frames = frames;
    t
}

/// Enters a container while dumping: fails past Jackson's nesting limit.
fn enter(chain: &mut Vec<bool>, object: bool) -> Result<(), Thrown> {
    chain.push(object);
    if chain.len() > crate::value::MAX_JSON_DEPTH {
        return Err(nesting_error(chain));
    }
    Ok(())
}

fn newline(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

fn dump_value(vm: &mut Vm, v: &Value, pretty: bool, depth: usize, out: &mut String, chain: &mut Vec<bool>) -> Result<(), Thrown> {
    if crate::guard::stack_exhausted() {
        return Err(crate::guard::stack_overflow());
    }
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Byte(x) => out.push_str(&x.to_string()),
        Value::Short(x) => out.push_str(&x.to_string()),
        Value::Int(x) => out.push_str(&x.to_string()),
        Value::Long(x) => out.push_str(&x.to_string()),
        Value::Float(f) => {
            if f.is_finite() {
                out.push_str(&crate::java::fmt_float(*f))
            } else {
                json_escape(&crate::java::fmt_float(*f), out)
            }
        }
        Value::Double(d) => {
            if d.is_finite() {
                out.push_str(&crate::java::fmt_double(*d))
            } else {
                json_escape(&crate::java::fmt_double(*d), out)
            }
        }
        Value::Str(s) => json_escape(s.as_str(), out),
        Value::Char(_) => {
            return Err(es_error(
                cls::ILLEGAL_ARGUMENT_EXCEPTION,
                "cannot write xcontent for unknown value of type class java.lang.Character",
                &[],
            ))
        }
        Value::Map(_) => {
            let entries = crate::lib_coll::map_entries(vm, v)?;
            dump_object(vm, &entries, pretty, depth, out, chain)?;
        }
        Value::Obj(o) => match &**o {
            Obj::Time(t) => json_escape(&t.to_xcontent_string(), out),
            Obj::Es(EsObj::Ctx(c)) => {
                let entries = c.entries();
                dump_object(vm, &entries, pretty, depth, out, chain)?;
            }
            _ => match crate::lib_coll::elements_opt(vm, v)? {
                Some(items) => dump_array(vm, &items, pretty, depth, out, chain)?,
                None => match &**o {
                    Obj::Array(_) => dump_array(vm, &crate::lib_coll::array_values(v), pretty, depth, out, chain)?,
                    _ => json_escape(&v.to_java_string(), out),
                },
            },
        },
        Value::List(_) | Value::Set(_) => {
            let items = crate::lib_coll::elements(vm, v)?;
            dump_array(vm, &items, pretty, depth, out, chain)?;
        }
    }
    Ok(())
}

fn dump_object(vm: &mut Vm, entries: &[(Value, Value)], pretty: bool, depth: usize, out: &mut String, chain: &mut Vec<bool>) -> Result<(), Thrown> {
    enter(chain, true)?;
    let r = dump_entries(vm, entries, pretty, depth, out, chain);
    chain.pop();
    r
}

fn dump_entries(vm: &mut Vm, entries: &[(Value, Value)], pretty: bool, depth: usize, out: &mut String, chain: &mut Vec<bool>) -> Result<(), Thrown> {
    if entries.is_empty() {
        out.push_str(if pretty { "{ }" } else { "{}" });
        return Ok(());
    }
    out.push('{');
    for (i, (k, x)) in entries.iter().enumerate() {
        crate::guard::check_string(out.len())?;
        if i > 0 {
            out.push(',');
        }
        if pretty {
            newline(out, depth + 1);
        }
        json_escape(&k.to_java_string(), out);
        out.push_str(if pretty { " : " } else { ":" });
        dump_value(vm, x, pretty, depth + 1, out, chain)?;
    }
    if pretty {
        newline(out, depth);
    }
    out.push('}');
    Ok(())
}

fn dump_array(vm: &mut Vm, items: &[Value], pretty: bool, depth: usize, out: &mut String, chain: &mut Vec<bool>) -> Result<(), Thrown> {
    enter(chain, false)?;
    let r = dump_items(vm, items, pretty, depth, out, chain);
    chain.pop();
    r
}

fn dump_items(vm: &mut Vm, items: &[Value], pretty: bool, depth: usize, out: &mut String, chain: &mut Vec<bool>) -> Result<(), Thrown> {
    if items.is_empty() {
        out.push_str(if pretty { "[ ]" } else { "[]" });
        return Ok(());
    }
    out.push('[');
    for (i, x) in items.iter().enumerate() {
        crate::guard::check_string(out.len())?;
        if i > 0 {
            out.push(',');
        }
        if pretty {
            newline(out, depth + 1);
        }
        dump_value(vm, x, pretty, depth + 1, out, chain)?;
    }
    if pretty {
        newline(out, depth);
    }
    out.push(']');
    Ok(())
}

fn j_load(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[0].is_null() {
        return Err(crate::vm::npe("Cannot invoke \"String.length()\" because \"content\" is null"));
    }
    json_load(&a[0].to_java_string())
}

fn j_dump(vm: &mut Vm, a: &mut [Value]) -> VR {
    let v = a[0].clone();
    Ok(Value::string(json_dump(vm, &v, false)?))
}

fn j_dump_pretty(vm: &mut Vm, a: &mut [Value]) -> VR {
    let v = a[0].clone();
    Ok(Value::string(json_dump(vm, &v, crate::lib_lang::boolean(&a[1]))?))
}

// ------------------------------------------------------------------------------ digests

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn str_sha1(_: &mut Vm, a: &mut [Value]) -> VR {
    use sha1::Digest;
    Ok(Value::string(hex(&sha1::Sha1::digest(a[0].to_java_string().as_bytes()))))
}

fn str_sha256(_: &mut Vm, a: &mut [Value]) -> VR {
    use sha2::Digest;
    Ok(Value::string(hex(&sha2::Sha256::digest(a[0].to_java_string().as_bytes()))))
}

fn str_sha512(_: &mut Vm, a: &mut [Value]) -> VR {
    use sha2::Digest;
    Ok(Value::string(hex(&sha2::Sha512::digest(a[0].to_java_string().as_bytes()))))
}

// ------------------------------------------------------------------------------ IP addresses

/// `InetAddresses.forString`: IPv4-mapped IPv6 addresses become IPv4 like `InetAddress`.
pub fn parse_ip(s: &str) -> Result<IpAddr, Thrown> {
    let t = s.strip_prefix('[').and_then(|x| x.strip_suffix(']')).unwrap_or(s);
    let parsed = if t.contains(':') { t.split('%').next().unwrap_or(t).parse::<Ipv6Addr>().map(IpAddr::V6).ok() } else { parse_v4(t).map(IpAddr::V4) };
    match parsed {
        Some(IpAddr::V6(v6)) => Ok(match v6.to_ipv4_mapped() {
            Some(v4) => IpAddr::V4(v4),
            None => IpAddr::V6(v6),
        }),
        Some(ip) => Ok(ip),
        None => Err(es_iae(
            format!("'{s}' is not an IP string literal."),
            &["common.network.InetAddresses.forString(InetAddresses.java:452)", "common.network.InetAddresses.forString(InetAddresses.java:431)"],
        )),
    }
}

fn parse_v4(s: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = s.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut b = [0u8; 4];
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() || p.len() > 3 || !p.bytes().all(|c| c.is_ascii_digit()) || (p.len() > 1 && p.starts_with('0')) {
            return None;
        }
        b[i] = p.parse().ok()?;
    }
    Some(Ipv4Addr::from(b))
}

/// `InetAddresses.toAddrString`.
pub fn ip_to_string(ip: &IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => {
            // RFC 5952 without the dotted-quad forms Rust uses for compatible addresses
            let seg = v6.segments();
            let (mut best, mut best_len, mut cur, mut cur_len) = (usize::MAX, 0, 0, 0);
            for (i, s) in seg.iter().enumerate() {
                if *s == 0 {
                    if cur_len == 0 {
                        cur = i;
                    }
                    cur_len += 1;
                    if cur_len > best_len {
                        best = cur;
                        best_len = cur_len;
                    }
                } else {
                    cur_len = 0;
                }
            }
            if best_len < 2 {
                best = usize::MAX;
            }
            let mut out = String::new();
            let mut i = 0;
            while i < 8 {
                if i == best {
                    out.push_str("::");
                    i += best_len;
                    continue;
                }
                if !out.is_empty() && !out.ends_with(':') {
                    out.push(':');
                }
                out.push_str(&format!("{:x}", seg[i]));
                i += 1;
            }
            out
        }
    }
}

fn ip_bytes(ip: &IpAddr) -> Vec<u8> {
    match ip {
        IpAddr::V4(v) => v.octets().to_vec(),
        IpAddr::V6(v) => v.octets().to_vec(),
    }
}

/// `CIDR`: inclusive byte range.
pub struct Cidr {
    lower: Vec<u8>,
    upper: Vec<u8>,
}

fn encode16(b: &[u8]) -> Vec<u8> {
    if b.len() == 16 {
        return b.to_vec();
    }
    let mut out = vec![0u8; 10];
    out.extend_from_slice(&[0xff, 0xff]);
    out.extend_from_slice(b);
    out
}

fn cidr_new(_: &mut Vm, a: &mut [Value]) -> VR {
    let s = a[0].to_java_string();
    let c = match s.split_once('/') {
        Some((ip, prefix)) => {
            let addr = parse_ip(ip)?;
            let bytes = ip_bytes(&addr);
            let bits = bytes.len() * 8;
            let p: usize = prefix.parse().ok().filter(|p| *p <= bits).ok_or_else(|| {
                es_iae(format!("Expected [ip/prefix] but was [{s}]"), &["common.network.InetAddresses.parseCidr(InetAddresses.java:395)"])
            })?;
            let mut lower = bytes.clone();
            let mut upper = bytes;
            for bit in p..bits {
                lower[bit / 8] &= !(0x80 >> (bit % 8));
                upper[bit / 8] |= 0x80 >> (bit % 8);
            }
            Cidr { lower, upper }
        }
        None => {
            let b = ip_bytes(&parse_ip(&s)?);
            Cidr { lower: b.clone(), upper: b }
        }
    };
    Ok(es_value(EsObj::Cidr(c)))
}

fn cidr_contains(_: &mut Vm, a: &mut [Value]) -> VR {
    let Some(EsObj::Cidr(c)) = es(&a[0]) else { return Err(crate::vm::cce("not a CIDR")) };
    if a[1].is_null() {
        return Ok(Value::Bool(false));
    }
    let s = a[1].to_java_string();
    if s.is_empty() {
        return Ok(Value::Bool(false));
    }
    let addr = ip_bytes(&parse_ip(&s)?);
    let (addr, lower, upper) = if addr.len() != c.lower.len() { (encode16(&addr), encode16(&c.lower), encode16(&c.upper)) } else { (addr, c.lower.clone(), c.upper.clone()) };
    Ok(Value::Bool(lower <= addr && upper >= addr))
}

fn ip_new(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(es_value(EsObj::Ip(parse_ip(&a[0].to_java_string())?)))
}

fn ip_v4(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(matches!(es(&a[0]), Some(EsObj::Ip(IpAddr::V4(_))))))
}

fn ip_v6(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(matches!(es(&a[0]), Some(EsObj::Ip(IpAddr::V6(_))))))
}

// ------------------------------------------------------------------------------ ByteBuffer

/// Element type of a `java.nio` buffer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NioKind {
    Byte,
    Char,
    Short,
    Int,
    Long,
    Float,
    Double,
}

impl NioKind {
    fn size(self) -> usize {
        match self {
            NioKind::Byte => 1,
            NioKind::Char | NioKind::Short => 2,
            NioKind::Int | NioKind::Float => 4,
            NioKind::Long | NioKind::Double => 8,
        }
    }
    fn name(self) -> &'static str {
        match self {
            NioKind::Byte => "Byte",
            NioKind::Char => "Char",
            NioKind::Short => "Short",
            NioKind::Int => "Int",
            NioKind::Long => "Long",
            NioKind::Float => "Float",
            NioKind::Double => "Double",
        }
    }
}

/// A `java.nio` buffer over shared bytes: a `HeapByteBuffer` (`ByteBuffer.wrap`, binary doc
/// values) or one of its typed views (`asIntBuffer()` → `ByteBufferAsIntBufferB`). Positions,
/// limits and capacities count elements; element `i` starts at byte `off + i * size`. Scripts
/// cannot write buffers or move their position, so only the byte order of a `ByteBuffer`
/// changes after creation (a view keeps the order it was created with).
pub struct NioBuf {
    pub bytes: Rc<[u8]>,
    pub off: usize,
    pub pos: usize,
    pub lim: usize,
    pub cap: usize,
    pub big: std::cell::Cell<bool>,
    pub kind: NioKind,
}

impl NioBuf {
    /// `ByteBuffer.wrap(bytes)`.
    pub fn wrap(bytes: Rc<[u8]>) -> NioBuf {
        let n = bytes.len();
        NioBuf { bytes, off: 0, pos: 0, lim: n, cap: n, big: std::cell::Cell::new(true), kind: NioKind::Byte }
    }

    pub fn class(&self) -> ClassId {
        let big = self.big.get();
        match (self.kind, big) {
            (NioKind::Byte, _) => cls::HEAP_BYTE_BUFFER,
            (NioKind::Char, true) => cls::CHAR_BUFFER_B,
            (NioKind::Char, false) => cls::CHAR_BUFFER_L,
            (NioKind::Short, true) => cls::SHORT_BUFFER_B,
            (NioKind::Short, false) => cls::SHORT_BUFFER_L,
            (NioKind::Int, true) => cls::INT_BUFFER_B,
            (NioKind::Int, false) => cls::INT_BUFFER_L,
            (NioKind::Long, true) => cls::LONG_BUFFER_B,
            (NioKind::Long, false) => cls::LONG_BUFFER_L,
            (NioKind::Float, true) => cls::FLOAT_BUFFER_B,
            (NioKind::Float, false) => cls::FLOAT_BUFFER_L,
            (NioKind::Double, true) => cls::DOUBLE_BUFFER_B,
            (NioKind::Double, false) => cls::DOUBLE_BUFFER_L,
        }
    }

    /// The `n` bytes at byte index `at` (relative to `off`) in big-endian order.
    fn raw(&self, at: usize, n: usize) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[..n].copy_from_slice(&self.bytes[self.off + at..self.off + at + n]);
        if !self.big.get() {
            out[..n].reverse();
        }
        out
    }

    /// A value of `kind` read at byte index `at`.
    fn read(&self, at: usize, kind: NioKind) -> Value {
        let b = self.raw(at, kind.size());
        match kind {
            NioKind::Byte => Value::Byte(b[0] as i8),
            NioKind::Char => Value::Char(u16::from_be_bytes([b[0], b[1]])),
            NioKind::Short => Value::Short(i16::from_be_bytes([b[0], b[1]])),
            NioKind::Int => Value::Int(i32::from_be_bytes([b[0], b[1], b[2], b[3]])),
            NioKind::Float => Value::Float(f32::from_be_bytes([b[0], b[1], b[2], b[3]])),
            NioKind::Long => Value::Long(i64::from_be_bytes(b)),
            NioKind::Double => Value::Double(f64::from_be_bytes(b)),
        }
    }

    /// Element `i` (absolute index, unchecked).
    fn elem(&self, i: usize) -> Value {
        self.read(i * self.kind.size(), self.kind)
    }

    /// The remaining elements (`position()` to `limit()`).
    fn remaining(&self) -> impl Iterator<Item = Value> + '_ {
        (self.pos..self.lim).map(|i| self.elem(i))
    }

    /// `CharBuffer.toString()`: the remaining chars.
    pub fn chars_string(&self) -> String {
        let units: Vec<u16> = self.remaining().map(|v| if let Value::Char(c) = v { c } else { 0 }).collect();
        String::from_utf16_lossy(&units)
    }

    pub fn write_string(&self, out: &mut String) {
        if self.kind == NioKind::Char {
            out.push_str(&self.chars_string());
        } else {
            out.push_str(&format!("{}[pos={} lim={} cap={}]", crate::lookup::java_binary_name(self.class()), self.pos, self.lim, self.cap));
        }
    }

    /// `X-Buffer.hashCode()`: `31 * h + (int) get(i)` from the limit down to the position.
    pub fn java_hash(&self) -> i32 {
        let mut h: i32 = 1;
        for i in (self.pos..self.lim).rev() {
            let x = match self.elem(i) {
                Value::Byte(v) => v as i32,
                Value::Char(v) => v as i32,
                Value::Short(v) => v as i32,
                Value::Int(v) => v,
                Value::Long(v) => v as i32,
                Value::Float(v) => crate::java::d2i(v as f64),
                Value::Double(v) => crate::java::d2i(v),
                _ => 0,
            };
            h = h.wrapping_mul(31).wrapping_add(x);
        }
        h
    }

    fn same_elem(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Float(x), Value::Float(y)) => x == y || (x.is_nan() && y.is_nan()),
            (Value::Double(x), Value::Double(y)) => x == y || (x.is_nan() && y.is_nan()),
            _ => a.java_equals(b),
        }
    }

    /// `equals`: same element type, same remaining elements (`-0.0` equals `0.0`, NaNs are equal).
    pub fn equals(&self, other: &NioBuf) -> bool {
        self.kind == other.kind && self.lim - self.pos == other.lim - other.pos && self.remaining().zip(other.remaining()).all(|(a, b)| NioBuf::same_elem(&a, &b))
    }

    /// `compareTo`: the element type's `compare` at the first mismatch, else the difference of
    /// the remaining counts. `None` when the other buffer has another element type.
    pub fn compare_to(&self, other: &NioBuf) -> Option<i32> {
        if self.kind != other.kind {
            return None;
        }
        for (a, b) in self.remaining().zip(other.remaining()) {
            if NioBuf::same_elem(&a, &b) && !matches!((&a, &b), (Value::Float(x), Value::Float(y)) if x.to_bits() != y.to_bits() && !x.is_nan())
                && !matches!((&a, &b), (Value::Double(x), Value::Double(y)) if x.to_bits() != y.to_bits() && !x.is_nan())
            {
                continue;
            }
            let c = match (a, b) {
                (Value::Byte(x), Value::Byte(y)) => x as i32 - y as i32,
                (Value::Char(x), Value::Char(y)) => x as i32 - y as i32,
                (Value::Short(x), Value::Short(y)) => x as i32 - y as i32,
                (Value::Int(x), Value::Int(y)) => x.cmp(&y) as i32,
                (Value::Long(x), Value::Long(y)) => x.cmp(&y) as i32,
                (Value::Float(x), Value::Float(y)) => nio_fcmp(x as f64, y as f64),
                (Value::Double(x), Value::Double(y)) => nio_fcmp(x, y),
                _ => 0,
            };
            return Some(c);
        }
        Some((self.lim - self.pos) as i32 - (other.lim - other.pos) as i32)
    }
}

/// `FloatBuffer.compare`/`DoubleBuffer.compare`: `-0.0` equals `0.0`, NaN is greatest.
fn nio_fcmp(x: f64, y: f64) -> i32 {
    if x < y {
        -1
    } else if x > y {
        1
    } else if x == y {
        0
    } else if x.is_nan() {
        if y.is_nan() { 0 } else { 1 }
    } else {
        -1
    }
}

fn buf(v: &Value) -> Result<&NioBuf, Thrown> {
    match es(v) {
        Some(EsObj::ByteBuffer(b)) => Ok(b),
        _ => Err(crate::vm::cce("not a Buffer")),
    }
}

pub fn nio(b: NioBuf) -> Value {
    es_value(EsObj::ByteBuffer(Rc::new(b)))
}

/// The `IndexOutOfBoundsException` of `Buffer.checkIndex` (`two`: the `(index, size)` overload)
/// thrown from `method` (a frame without the `java.base/java.nio.` prefix).
fn nio_oob(two: bool, method: &str) -> Thrown {
    let mut t = Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, None);
    t.frames = vec![
        "java.base/java.nio.Buffer$1.apply(Buffer.java:756)".into(),
        "java.base/java.nio.Buffer$1.apply(Buffer.java:753)".into(),
        "java.base/jdk.internal.util.Preconditions$4.apply(Preconditions.java:213)".into(),
        "java.base/jdk.internal.util.Preconditions$4.apply(Preconditions.java:210)".into(),
        "java.base/jdk.internal.util.Preconditions.outOfBounds(Preconditions.java:98)".into(),
        "java.base/jdk.internal.util.Preconditions.outOfBoundsCheckIndex(Preconditions.java:106)".into(),
        "java.base/jdk.internal.util.Preconditions.checkIndex(Preconditions.java:302)".into(),
        format!("java.base/java.nio.Buffer.checkIndex(Buffer.java:{})", if two { 778 } else { 767 }),
        format!("java.base/java.nio.{method}"),
    ];
    t
}

/// `ByteBuffer.getX(int)`: `kind` read at byte index `i`, checked against the limit.
fn bb_read(a: &[Value], kind: NioKind) -> VR {
    let b = buf(&a[0])?;
    let i = crate::lib_lang::int(&a[1]);
    let n = kind.size() as i64;
    if i < 0 || (i as i64) > b.lim as i64 - n {
        let line = match kind {
            NioKind::Byte => "get(HeapByteBuffer.java:180)",
            NioKind::Char => "getChar(HeapByteBuffer.java:392)",
            NioKind::Short => "getShort(HeapByteBuffer.java:444)",
            NioKind::Int => "getInt(HeapByteBuffer.java:496)",
            NioKind::Long => "getLong(HeapByteBuffer.java:548)",
            NioKind::Float => "getFloat(HeapByteBuffer.java:601)",
            NioKind::Double => "getDouble(HeapByteBuffer.java:657)",
        };
        return Err(nio_oob(kind != NioKind::Byte, &format!("HeapByteBuffer.{line}")));
    }
    Ok(b.read(i as usize, kind))
}

fn bb_get(_: &mut Vm, a: &mut [Value]) -> VR {
    let b = buf(&a[0])?;
    if b.kind == NioKind::Byte {
        return bb_read(a, NioKind::Byte);
    }
    // a typed view's `get(int)`
    let i = crate::lib_lang::int(&a[1]);
    if i < 0 || i as usize >= b.lim {
        let c = crate::lookup::java_binary_name(b.class()).trim_start_matches("java.nio.");
        return Err(nio_oob(false, &format!("{c}.get({c}.java:151)")));
    }
    Ok(b.elem(i as usize))
}
fn bb_get_char(_: &mut Vm, a: &mut [Value]) -> VR {
    bb_read(a, NioKind::Char)
}
fn bb_get_short(_: &mut Vm, a: &mut [Value]) -> VR {
    bb_read(a, NioKind::Short)
}
fn bb_get_int(_: &mut Vm, a: &mut [Value]) -> VR {
    bb_read(a, NioKind::Int)
}
fn bb_get_long(_: &mut Vm, a: &mut [Value]) -> VR {
    bb_read(a, NioKind::Long)
}
fn bb_get_float(_: &mut Vm, a: &mut [Value]) -> VR {
    bb_read(a, NioKind::Float)
}
fn bb_get_double(_: &mut Vm, a: &mut [Value]) -> VR {
    bb_read(a, NioKind::Double)
}
fn bb_order(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(es_value(EsObj::ByteOrder(buf(&a[0])?.big.get())))
}
fn bb_set_order(_: &mut Vm, a: &mut [Value]) -> VR {
    // `order(null)` selects little endian: the JDK compares with BIG_ENDIAN
    let big = matches!(es(&a[1]), Some(EsObj::ByteOrder(true)));
    buf(&a[0])?.big.set(big);
    Ok(a[0].clone())
}
fn bb_limit(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(buf(&a[0])?.lim as i32))
}
/// `asXBuffer()`: a view of the remaining bytes in the buffer's current byte order.
fn bb_view(a: &[Value], kind: NioKind) -> VR {
    let b = buf(&a[0])?;
    let n = (b.lim - b.pos) / kind.size();
    Ok(nio(NioBuf { bytes: b.bytes.clone(), off: b.off + b.pos, pos: 0, lim: n, cap: n, big: std::cell::Cell::new(b.big.get()), kind }))
}
fn bb_as_char(_: &mut Vm, a: &mut [Value]) -> VR {
    bb_view(a, NioKind::Char)
}
fn bb_as_short(_: &mut Vm, a: &mut [Value]) -> VR {
    bb_view(a, NioKind::Short)
}
fn bb_as_int(_: &mut Vm, a: &mut [Value]) -> VR {
    bb_view(a, NioKind::Int)
}
fn bb_as_long(_: &mut Vm, a: &mut [Value]) -> VR {
    bb_view(a, NioKind::Long)
}
fn bb_as_float(_: &mut Vm, a: &mut [Value]) -> VR {
    bb_view(a, NioKind::Float)
}
fn bb_as_double(_: &mut Vm, a: &mut [Value]) -> VR {
    bb_view(a, NioKind::Double)
}

/// `CharBuffer` as a `CharSequence`: `length()`, or `None` for other receivers.
pub fn char_buffer_len(v: &Value) -> Option<i32> {
    match es(v) {
        Some(EsObj::ByteBuffer(b)) if b.kind == NioKind::Char => Some((b.lim - b.pos) as i32),
        _ => None,
    }
}

/// `CharBuffer.charAt(i)`, or `None` for other receivers.
pub fn char_buffer_char_at(v: &Value, i: i32) -> Option<VR> {
    match es(v) {
        Some(EsObj::ByteBuffer(b)) if b.kind == NioKind::Char => Some(if i < 0 || i as usize >= b.lim - b.pos {
            Err(nio_oob(true, "CharBuffer.charAt(CharBuffer.java:2024)"))
        } else {
            Ok(b.elem(b.pos + i as usize))
        }),
        _ => None,
    }
}

/// `CharBuffer.subSequence(start, end)`: a buffer over the same chars with the position and
/// limit moved, or `None` for other receivers.
pub fn char_buffer_sub_sequence(v: &Value, start: i32, end: i32) -> Option<VR> {
    match es(v) {
        Some(EsObj::ByteBuffer(b)) if b.kind == NioKind::Char => {
            let len = (b.lim - b.pos) as i32;
            if start < 0 || start > end || end > len {
                let c = crate::lookup::java_binary_name(b.class()).trim_start_matches("java.nio.");
                let mut t = Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Range [{start}, {end}) out of bounds for length {len}")));
                t.frames = vec![
                    "java.base/jdk.internal.util.Preconditions.outOfBounds(Preconditions.java:100)".into(),
                    "java.base/jdk.internal.util.Preconditions.outOfBoundsCheckFromToIndex(Preconditions.java:112)".into(),
                    "java.base/jdk.internal.util.Preconditions.checkFromToIndex(Preconditions.java:349)".into(),
                    "java.base/java.util.Objects.checkFromToIndex(Objects.java:391)".into(),
                    format!("java.base/java.nio.{c}.subSequence({c}.java:242)"),
                    format!("java.base/java.nio.{c}.subSequence({c}.java:39)"),
                ];
                return Some(Err(t));
            }
            Some(Ok(nio(NioBuf {
                bytes: b.bytes.clone(),
                off: b.off,
                pos: b.pos + start as usize,
                lim: b.pos + end as usize,
                cap: b.cap,
                big: std::cell::Cell::new(b.big.get()),
                kind: NioKind::Char,
            })))
        }
        _ => None,
    }
}

/// `Comparable.compareTo` between buffers (`None`: the receiver is not a buffer).
pub fn buffer_compare(a: &Value, b: &Value) -> Option<VR> {
    let Some(EsObj::ByteBuffer(x)) = es(a) else { return None };
    if b.is_null() {
        let c = x.kind.name();
        return Some(Err(crate::vm::npe(format!("Cannot invoke \"java.nio.{c}Buffer.position()\" because \"that\" is null")).with_frames(&[
            &format!("java.base/java.nio.{c}Buffer.compareTo({c}Buffer.java:1847)"),
            &format!("java.base/java.nio.{c}Buffer.compareTo({c}Buffer.java:292)"),
        ])));
    }
    match es(b) {
        Some(EsObj::ByteBuffer(y)) if x.kind == y.kind => Some(Ok(Value::Int(x.compare_to(y).unwrap_or(0)))),
        _ => {
            let c = x.kind.name();
            let target = format!("java.nio.{c}Buffer");
            let t = crate::vm::cce(crate::cast::checkcast_message(crate::lookup::java_binary_name(b.class()), &target))
                .with_frames(&[&format!("java.base/java.nio.{c}Buffer.compareTo({c}Buffer.java:292)")]);
            Some(Err(t))
        }
    }
}

fn byte_array(v: &Value) -> Result<Vec<u8>, Thrown> {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Array(arr) => match &*arr.data.borrow() {
                crate::value::ArrayData::Byte(b) => Ok(b.iter().map(|x| *x as u8).collect()),
                _ => Err(crate::vm::cce("not a byte[]")),
            },
            _ => Err(crate::vm::cce("not a byte[]")),
        },
        _ => Err(crate::vm::npe("Cannot read the array length because \"array\" is null").with_frames(&["java.base/java.nio.ByteBuffer.wrap(ByteBuffer.java:472)"])),
    }
}
fn bb_wrap(_: &mut Vm, a: &mut [Value]) -> VR {
    let b = byte_array(&a[0])?;
    Ok(nio(NioBuf::wrap(Rc::from(b))))
}
fn bb_wrap3(_: &mut Vm, a: &mut [Value]) -> VR {
    let b = byte_array(&a[0])?;
    let (off, len) = (crate::lib_lang::int(&a[1]) as i64, crate::lib_lang::int(&a[2]) as i64);
    if off < 0 || len < 0 || off + len > b.len() as i64 {
        let mut t = Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, None);
        t.frames = vec!["java.base/java.nio.ByteBuffer.wrap(ByteBuffer.java:445)".into()];
        return Err(t);
    }
    let mut nb = NioBuf::wrap(Rc::from(b));
    nb.pos = off as usize;
    nb.lim = (off + len) as usize;
    Ok(nio(nb))
}

// ------------------------------------------------------------------------------ similarity & co

/// Objects of similarity, interval filter and analysis predicate scripts.
pub enum SimObj {
    Query { boost: f32 },
    Field { doc_count: i64, sum_doc_freq: i64, sum_total_term_freq: i64 },
    Term { doc_freq: i64, total_term_freq: i64 },
    Doc { length: i32, freq: f32 },
    Interval { start: i32, end: i32, gaps: i32 },
    Token(Token),
    /// The token's attributes as `getTerm()` returns them: Lucene's
    /// `PackedTokenAttributeImpl`, a `CharSequence` of the term.
    TermAttr(Token),
}

/// `AnalysisPredicateScript.Token`.
#[derive(Clone, Debug, Default)]
pub struct Token {
    pub term: String,
    pub position: i32,
    pub position_increment: i32,
    pub position_length: i32,
    pub start_offset: i32,
    pub end_offset: i32,
    pub type_: String,
    pub keyword: bool,
}

impl SimObj {
    pub fn class(&self) -> ClassId {
        match self {
            SimObj::Query { .. } => cls::SIMILARITY_QUERY,
            SimObj::Field { .. } => cls::SIMILARITY_FIELD,
            SimObj::Term { .. } => cls::SIMILARITY_TERM,
            SimObj::Doc { .. } => cls::SIMILARITY_DOC,
            SimObj::Interval { .. } => cls::INTERVAL,
            SimObj::Token(_) => cls::ANALYSIS_TOKEN,
            SimObj::TermAttr(_) => cls::PACKED_TOKEN_ATTRIBUTE,
        }
    }
}

/// `ScriptedSimilarity.Query`.
pub fn similarity_query(boost: f32) -> Value {
    es_value(EsObj::Similarity(SimObj::Query { boost }))
}
/// `ScriptedSimilarity.Field`.
pub fn similarity_field(doc_count: i64, sum_doc_freq: i64, sum_total_term_freq: i64) -> Value {
    es_value(EsObj::Similarity(SimObj::Field { doc_count, sum_doc_freq, sum_total_term_freq }))
}
/// `ScriptedSimilarity.Term`.
pub fn similarity_term(doc_freq: i64, total_term_freq: i64) -> Value {
    es_value(EsObj::Similarity(SimObj::Term { doc_freq, total_term_freq }))
}
/// `ScriptedSimilarity.Doc`.
pub fn similarity_doc(length: i32, freq: f32) -> Value {
    es_value(EsObj::Similarity(SimObj::Doc { length, freq }))
}
/// `IntervalFilterScript.Interval`.
pub fn interval(start: i32, end: i32, gaps: i32) -> Value {
    es_value(EsObj::Similarity(SimObj::Interval { start, end, gaps }))
}
impl Token {
    /// `PackedTokenAttributeImpl.hashCode()`: the term's hash (`CharTermAttributeImpl`: length,
    /// then the chars last to first), then offsets, increment, length, type and frequency.
    pub fn attribute_hash(&self) -> i32 {
        let units: Vec<u16> = self.term.encode_utf16().collect();
        let chars = units.iter().rev().fold(0i32, |h, &c| h.wrapping_mul(31).wrapping_add(c as i32));
        let mut h = (units.len() as i32).wrapping_mul(31).wrapping_add(chars);
        for v in [self.start_offset, self.end_offset, self.position_increment, self.position_length] {
            h = h.wrapping_mul(31).wrapping_add(v);
        }
        h = h.wrapping_mul(31).wrapping_add(crate::java::string_hash(&self.type_));
        // term frequency
        h.wrapping_mul(31).wrapping_add(1)
    }

    /// `PackedTokenAttributeImpl.equals`.
    pub fn attribute_equals(&self, o: &Token) -> bool {
        self.term == o.term
            && self.start_offset == o.start_offset
            && self.end_offset == o.end_offset
            && self.position_increment == o.position_increment
            && self.position_length == o.position_length
            && self.type_ == o.type_
    }
}

/// `ScriptHeuristic.LongAccessor` holding `v` (the `_subset_freq`, `_superset_freq`,
/// `_subset_size` and `_superset_size` params of a `script_heuristic`).
pub fn long_accessor(v: i64) -> Value {
    es_value(EsObj::LongAccessor(std::cell::Cell::new(v)))
}

/// Updates a [`long_accessor`] in place (the aggregation reuses one params map per shard).
pub fn set_long_accessor(acc: &Value, v: i64) {
    if let Some(EsObj::LongAccessor(c)) = es(acc) {
        c.set(v);
    }
}

/// `AnalysisPredicateScript.Token`.
pub fn analysis_token(t: Token) -> Value {
    es_value(EsObj::Similarity(SimObj::Token(t)))
}

fn sim(v: &Value) -> Result<&SimObj, Thrown> {
    match es(v) {
        Some(EsObj::Similarity(s)) => Ok(s),
        _ => Err(crate::vm::cce("not a similarity object")),
    }
}

macro_rules! sim_getter {
    ($name:ident, $pat:pat => $e:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            match sim(&a[0])? {
                $pat => Ok($e),
                _ => Err(crate::vm::cce("wrong similarity object")),
            }
        }
    };
}

sim_getter!(q_boost, SimObj::Query { boost } => Value::Float(*boost));
sim_getter!(f_doc_count, SimObj::Field { doc_count, .. } => Value::Long(*doc_count));
sim_getter!(f_sum_doc_freq, SimObj::Field { sum_doc_freq, .. } => Value::Long(*sum_doc_freq));
sim_getter!(f_sum_ttf, SimObj::Field { sum_total_term_freq, .. } => Value::Long(*sum_total_term_freq));
sim_getter!(t_doc_freq, SimObj::Term { doc_freq, .. } => Value::Long(*doc_freq));
sim_getter!(t_ttf, SimObj::Term { total_term_freq, .. } => Value::Long(*total_term_freq));
sim_getter!(d_length, SimObj::Doc { length, .. } => Value::Int(*length));
sim_getter!(d_freq, SimObj::Doc { freq, .. } => Value::Float(*freq));
sim_getter!(i_start, SimObj::Interval { start, .. } => Value::Int(*start));
sim_getter!(i_end, SimObj::Interval { end, .. } => Value::Int(*end));
sim_getter!(i_gaps, SimObj::Interval { gaps, .. } => Value::Int(*gaps));
sim_getter!(tk_term, SimObj::Token(t) => es_value(EsObj::Similarity(SimObj::TermAttr(t.clone()))));
sim_getter!(tk_position, SimObj::Token(t) => Value::Int(t.position));
sim_getter!(tk_pos_inc, SimObj::Token(t) => Value::Int(t.position_increment));
sim_getter!(tk_pos_len, SimObj::Token(t) => Value::Int(t.position_length));
sim_getter!(tk_start, SimObj::Token(t) => Value::Int(t.start_offset));
sim_getter!(tk_end, SimObj::Token(t) => Value::Int(t.end_offset));
sim_getter!(tk_type, SimObj::Token(t) => Value::str(&t.type_));
sim_getter!(tk_keyword, SimObj::Token(t) => Value::Bool(t.keyword));

pub fn register(b: &mut Builder) {
    use cls::*;
    b.supers(DEBUG, &[OBJECT]);
    b.s(DEBUG, "void explain(Object)", debug_explain);
    b.supers(JSON, &[OBJECT]);
    b.s(JSON, "def load(String)", j_load);
    b.s(JSON, "String dump(def)", j_dump);
    b.s(JSON, "String dump(def,boolean)", j_dump_pretty);
    for (sig, f) in [("String sha1()", str_sha1 as crate::lookup::Native), ("String sha256()", str_sha256), ("String sha512()", str_sha512)] {
        let m = b.m(STRING, sig, f);
        b.aug(m, "org.elasticsearch.painless.api.Augmentation");
    }
    b.supers(CIDR, &[OBJECT]);
    b.c(CIDR, "(String)", cidr_new);
    b.m(CIDR, "boolean contains(String)", cidr_contains);
    b.supers(IP_ADDRESS, &[OBJECT]);
    b.c(IP_ADDRESS, "(String)", ip_new);
    b.m(IP_ADDRESS, "boolean isV4()", ip_v4);
    b.m(IP_ADDRESS, "boolean isV6()", ip_v6);

    b.supers(BYTE_ORDER, &[OBJECT]);
    b.sf(BYTE_ORDER, "ByteOrder BIG_ENDIAN", |_, _| Ok(es_value(EsObj::ByteOrder(true))));
    b.sf(BYTE_ORDER, "ByteOrder LITTLE_ENDIAN", |_, _| Ok(es_value(EsObj::ByteOrder(false))));
    b.supers(NIO_BUFFER, &[OBJECT]);
    b.m(NIO_BUFFER, "int limit()", bb_limit);
    b.supers(BYTE_BUFFER, &[NIO_BUFFER, COMPARABLE]);
    b.supers(HEAP_BYTE_BUFFER, &[BYTE_BUFFER]);
    b.supers(CHAR_BUFFER, &[NIO_BUFFER, COMPARABLE, APPENDABLE, CHAR_SEQUENCE]);
    for c in [SHORT_BUFFER, INT_BUFFER, LONG_BUFFER, FLOAT_BUFFER, DOUBLE_BUFFER] {
        b.supers(c, &[NIO_BUFFER, COMPARABLE]);
    }
    for (view, sup) in [
        (CHAR_BUFFER_B, CHAR_BUFFER),
        (CHAR_BUFFER_L, CHAR_BUFFER),
        (SHORT_BUFFER_B, SHORT_BUFFER),
        (SHORT_BUFFER_L, SHORT_BUFFER),
        (INT_BUFFER_B, INT_BUFFER),
        (INT_BUFFER_L, INT_BUFFER),
        (LONG_BUFFER_B, LONG_BUFFER),
        (LONG_BUFFER_L, LONG_BUFFER),
        (FLOAT_BUFFER_B, FLOAT_BUFFER),
        (FLOAT_BUFFER_L, FLOAT_BUFFER),
        (DOUBLE_BUFFER_B, DOUBLE_BUFFER),
        (DOUBLE_BUFFER_L, DOUBLE_BUFFER),
    ] {
        b.supers(view, &[sup]);
    }
    b.m(CHAR_BUFFER, "char get(int)", bb_get);
    b.m(SHORT_BUFFER, "short get(int)", bb_get);
    b.m(INT_BUFFER, "int get(int)", bb_get);
    b.m(LONG_BUFFER, "long get(int)", bb_get);
    b.m(FLOAT_BUFFER, "float get(int)", bb_get);
    b.m(DOUBLE_BUFFER, "double get(int)", bb_get);
    b.m(BYTE_BUFFER, "CharBuffer asCharBuffer()", bb_as_char);
    b.m(BYTE_BUFFER, "DoubleBuffer asDoubleBuffer()", bb_as_double);
    b.m(BYTE_BUFFER, "FloatBuffer asFloatBuffer()", bb_as_float);
    b.m(BYTE_BUFFER, "IntBuffer asIntBuffer()", bb_as_int);
    b.m(BYTE_BUFFER, "LongBuffer asLongBuffer()", bb_as_long);
    b.m(BYTE_BUFFER, "ShortBuffer asShortBuffer()", bb_as_short);
    b.m(BYTE_BUFFER, "byte get(int)", bb_get);
    b.m(BYTE_BUFFER, "char getChar(int)", bb_get_char);
    b.m(BYTE_BUFFER, "double getDouble(int)", bb_get_double);
    b.m(BYTE_BUFFER, "float getFloat(int)", bb_get_float);
    b.m(BYTE_BUFFER, "int getInt(int)", bb_get_int);
    b.m(BYTE_BUFFER, "long getLong(int)", bb_get_long);
    b.m(BYTE_BUFFER, "short getShort(int)", bb_get_short);
    b.m(BYTE_BUFFER, "ByteOrder order()", bb_order);
    b.m(BYTE_BUFFER, "ByteBuffer order(ByteOrder)", bb_set_order);
    b.s(BYTE_BUFFER, "ByteBuffer wrap(byte[])", bb_wrap);
    b.s(BYTE_BUFFER, "ByteBuffer wrap(byte[],int,int)", bb_wrap3);

    for c in [SIMILARITY_QUERY, SIMILARITY_FIELD, SIMILARITY_TERM, SIMILARITY_DOC, INTERVAL, ANALYSIS_TOKEN] {
        b.supers(c, &[OBJECT]);
    }
    b.m(SIMILARITY_QUERY, "float getBoost()", q_boost);
    b.m(SIMILARITY_FIELD, "long getDocCount()", f_doc_count);
    b.m(SIMILARITY_FIELD, "long getSumDocFreq()", f_sum_doc_freq);
    b.m(SIMILARITY_FIELD, "long getSumTotalTermFreq()", f_sum_ttf);
    b.m(SIMILARITY_TERM, "long getDocFreq()", t_doc_freq);
    b.m(SIMILARITY_TERM, "long getTotalTermFreq()", t_ttf);
    b.m(SIMILARITY_DOC, "int getLength()", d_length);
    b.m(SIMILARITY_DOC, "float getFreq()", d_freq);
    b.m(INTERVAL, "int getStart()", i_start);
    b.m(INTERVAL, "int getEnd()", i_end);
    b.m(INTERVAL, "int getGaps()", i_gaps);
    // the token's `CharTermAttribute` (a `CharSequence`, not a `String`)
    b.supers(PACKED_TOKEN_ATTRIBUTE, &[OBJECT, CHAR_SEQUENCE]);
    b.supers(LONG_ACCESSOR, &[NUMBER, OBJECT]);
    b.m(ANALYSIS_TOKEN, "CharSequence getTerm()", tk_term);
    b.m(ANALYSIS_TOKEN, "int getPosition()", tk_position);
    b.m(ANALYSIS_TOKEN, "int getPositionIncrement()", tk_pos_inc);
    b.m(ANALYSIS_TOKEN, "int getPositionLength()", tk_pos_len);
    b.m(ANALYSIS_TOKEN, "int getStartOffset()", tk_start);
    b.m(ANALYSIS_TOKEN, "int getEndOffset()", tk_end);
    b.m(ANALYSIS_TOKEN, "String getType()", tk_type);
    b.m(ANALYSIS_TOKEN, "boolean isKeyword()", tk_keyword);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv6_format() {
        assert_eq!(ip_to_string(&parse_ip("::1").unwrap()), "::1");
        assert_eq!(ip_to_string(&parse_ip("2001:db8:0:0:1:0:0:1").unwrap()), "2001:db8::1:0:0:1");
        assert_eq!(ip_to_string(&parse_ip("::ffff:1.2.3.4").unwrap()), "1.2.3.4");
        assert_eq!(ip_to_string(&parse_ip("::").unwrap()), "::");
        assert!(parse_ip("bad").is_err());
        assert!(parse_ip("01.2.3.4").is_err());
    }
}
