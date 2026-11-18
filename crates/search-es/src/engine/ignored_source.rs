//! `_ignored_source` entries as Elasticsearch stores them (`IgnoredSourceFieldMapper`,
//! `XContentDataHelper`), returned by `stored_fields` / `fields` naming `_ignored_source`.
//!
//! Synthetic `_source` keeps what it can't rebuild from columns verbatim ([`super::synthetic`]
//! decides what). Elasticsearch stores each such value as a binary entry: the field's full name,
//! the offset of its leaf name in it (the parent path's length plus the dot), and the value
//! encoded by its token: `S` and the UTF-8 bytes for a string, `I`/`L` and a little-endian
//! int/long, `i` and a `BigInteger`'s two's-complement bytes, `D` and a little-endian double,
//! `t`/`f`, `n` for `null`, and objects and arrays rewritten by the request's content type's
//! generator after a type byte (`j` JSON, `y` YAML, `c` CBOR).
//!
//! Two layouts, by `index.use_time_series_doc_values_format` (the logs and time series default):
//!
//! - one entry per value, in document order: a little-endian int holding the name's byte length
//!   plus `65536 ×` the parent offset, the name, the value;
//! - otherwise one entry per field name ("coalesced"): a vint count, the name's vint length and
//!   bytes, then per value (document order) its vint parent offset, vint length and bytes. The
//!   entries come in the iteration order of a `HashMap` that received the names last value
//!   first.
//!
//! roll keeps the entries as byte values of the `_ignored` stored field (its names are strings).

use crate::json::{Map, Number, Value};
use crate::xcontent::XContentType;
use bytes::Bytes;

/// One value kept verbatim: its full name, the length of its parent's path (0 at the root) and
/// the value as the document gave it.
#[derive(Debug)]
pub struct Entry {
    pub name: String,
    pub parent: String,
    pub value: Value,
}

impl Entry {
    /// `NameValue.parentOffset`: where the leaf name starts (UTF-16 units, Java's `substring`).
    fn parent_offset(&self) -> usize {
        if self.parent.is_empty() { 0 } else { self.parent.encode_utf16().count() + 1 }
    }
}

/// The stored entries of a document's verbatim values (`entries` in document order).
pub fn encode(entries: &[Entry], ty: XContentType, per_value: bool) -> Vec<Bytes> {
    if per_value {
        return entries
            .iter()
            .map(|e| {
                let mut out = Vec::with_capacity(e.name.len() + 16);
                let head = e.name.len() as u32 + ((e.parent_offset() as u32) << 16);
                out.extend_from_slice(&head.to_le_bytes());
                out.extend_from_slice(e.name.as_bytes());
                encode_value(&e.value, ty, &mut out);
                Bytes::from(out)
            })
            .collect();
    }
    // Names in the order their last value reached the map, iterated as a `HashMap` does.
    let mut names: Vec<&str> = Vec::new();
    for e in entries.iter().rev() {
        if !names.contains(&e.name.as_str()) {
            names.push(&e.name);
        }
    }
    let order = crate::json::java_hash_order(&names, 16);
    order
        .into_iter()
        .map(|i| {
            let name = names[i];
            let values: Vec<&Entry> = entries.iter().filter(|e| e.name == name).collect();
            let mut out = Vec::new();
            put_vint(&mut out, values.len() as u32);
            put_vint(&mut out, name.len() as u32);
            out.extend_from_slice(name.as_bytes());
            let mut v = Vec::new();
            for e in values {
                v.clear();
                encode_value(&e.value, ty, &mut v);
                put_vint(&mut out, e.parent_offset() as u32);
                put_vint(&mut out, v.len() as u32);
                out.extend_from_slice(&v);
            }
            Bytes::from(out)
        })
        .collect()
}

/// The whole document of an index whose root mapping is disabled: one `_doc` value.
pub fn whole_document(doc: &Map) -> Entry {
    Entry { name: "_doc".into(), parent: String::new(), value: Value::Obj(doc.clone()) }
}

fn put_vint(out: &mut Vec<u8>, mut v: u32) {
    while v >= 0x80 {
        out.push((v as u8 & 0x7f) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// `XContentDataHelper.encodeToken`.
fn encode_value(v: &Value, ty: XContentType, out: &mut Vec<u8>) {
    match v {
        Value::Null => out.push(b'n'),
        Value::Bool(true) => out.push(b't'),
        Value::Bool(false) => out.push(b'f'),
        Value::Str(s) => {
            out.push(b'S');
            out.extend_from_slice(s.as_bytes());
        }
        Value::Num(Number::Int(i)) => match i32::try_from(*i) {
            Ok(n) => {
                out.push(b'I');
                out.extend_from_slice(&n.to_le_bytes());
            }
            Err(_) => {
                out.push(b'L');
                out.extend_from_slice(&i.to_le_bytes());
            }
        },
        Value::Num(Number::UInt(u)) => {
            out.push(b'i');
            out.extend_from_slice(&java_big_integer(&u.to_string()));
        }
        Value::Num(Number::Big(s)) => {
            out.push(b'i');
            out.extend_from_slice(&java_big_integer(s));
        }
        Value::Num(Number::Float(f)) => {
            out.push(b'D');
            out.extend_from_slice(&f.to_bits().to_le_bytes());
        }
        Value::Arr(_) | Value::Obj(_) => {
            out.push(match ty {
                XContentType::Yaml => b'y',
                XContentType::Cbor => b'c',
                XContentType::Smile => b's',
                XContentType::Json => b'j',
            });
            match ty {
                XContentType::Yaml => crate::xcontent::yaml::write_yaml(out, v),
                XContentType::Cbor => crate::xcontent::cbor::write_cbor(out, v),
                _ => crate::json::write::write_json(out, v, false),
            }
        }
    }
}

/// `BigInteger.toByteArray()` of a decimal integer: big-endian two's complement, shortest.
fn java_big_integer(s: &str) -> Vec<u8> {
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s),
    };
    let mut mag = crate::xcontent::cbor::decimal_to_be_bytes(digits);
    if !neg {
        if mag[0] & 0x80 != 0 {
            mag.insert(0, 0);
        }
        return mag;
    }
    // -m = !(m - 1), over one more byte than the magnitude needs, then the redundant sign
    // bytes dropped.
    let mut m = mag;
    m.insert(0, 0);
    for b in m.iter_mut().rev() {
        let (x, borrow) = b.overflowing_sub(1);
        *b = x;
        if !borrow {
            break;
        }
    }
    let mut out: Vec<u8> = m.into_iter().map(|b| !b).collect();
    while out.len() > 1 && out[0] == 0xff && out[1] & 0x80 != 0 {
        out.remove(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;

    fn b64(b: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(b)
    }

    fn entry(name: &str, parent: &str, v: Value) -> Entry {
        Entry { name: name.into(), parent: parent.into(), value: v }
    }

    /// Entries captured from Elasticsearch 9.5.5.
    #[test]
    fn encodes_as_elasticsearch() {
        let obj = crate::json::parse(br#"{"name":"foo","value":10}"#).unwrap().unwrap();
        let yaml = encode(&[entry("object", "", obj.clone())], XContentType::Yaml, true);
        assert_eq!(b64(&yaml[0]), "BgAAAG9iamVjdHktLS0KbmFtZTogImZvbyIKdmFsdWU6IDEwCg==");
        let dynamic = encode(&[entry("value", "", Value::from(1i64)), entry("id", "", Value::from("f5t7-66gt"))], XContentType::Json, true);
        assert_eq!(dynamic.iter().map(|b| b64(b)).collect::<Vec<_>>(), ["BQAAAHZhbHVlSQEAAAA=", "AgAAAGlkU2Y1dDctNjZndA=="]);
        let big = crate::json::parse(b"[123456789012345678901234567890, 18446744073709551615, -9223372036854775808, 2.5]").unwrap().unwrap();
        let Value::Arr(big) = big else { panic!() };
        let out = encode(&[entry("a.c.big", "a.c", big[0].clone()), entry("d.u", "d", big[1].clone()), entry("d.neg", "d", big[2].clone()), entry("d.f", "d", big[3].clone())], XContentType::Json, true);
        assert_eq!(&out[0][4 + 7..], b"i\x01\x8e\xe9\x0f\xf6\xc3\x73\xe0\xee\x4e\x3f\x0a\xd2");
        assert_eq!(&out[0][..4], &(7u32 + (4 << 16)).to_le_bytes());
        assert_eq!(&out[1][4 + 3..], b"i\x00\xff\xff\xff\xff\xff\xff\xff\xff");
        assert_eq!(&out[2][4 + 5..], b"L\x00\x00\x00\x00\x00\x00\x00\x80");
        assert_eq!(&out[3][4 + 3..], b"D\x00\x00\x00\x00\x00\x00\x04@");
        assert_eq!(java_big_integer("-1"), [0xff]);
        assert_eq!(java_big_integer("-128"), [0x80]);
        assert_eq!(java_big_integer("-129"), [0xff, 0x7f]);
    }

    /// One entry per name, in a `HashMap`'s order of the names inserted last value first, the
    /// values of a name in document order (Elasticsearch 9.5.5).
    #[test]
    fn coalesces_by_name() {
        let o = |s: &str| crate::json::parse(s.as_bytes()).unwrap().unwrap();
        let entries = [
            entry("p.b", "p", o(r#"{"x":1}"#)),
            entry("p.z", "p", Value::from(3i64)),
            entry("p.b", "p", o(r#"{"x":2}"#)),
            entry("p.b", "p", Value::from("s")),
        ];
        let out = encode(&entries, XContentType::Json, false);
        assert_eq!(out[0].as_ref(), b"\x03\x03p.b\x02\x08j{\"x\":1}\x02\x08j{\"x\":2}\x02\x02Ss");
        assert_eq!(out[1].as_ref(), b"\x01\x03p.z\x02\x05I\x03\x00\x00\x00");
        let mut names: Vec<String> = (1..=14).map(|i| format!("k{i}")).collect();
        names.extend(["o", "r", "f", "neg"].map(String::from));
        let entries: Vec<Entry> = names.iter().map(|n| entry(n, "", Value::from(1i64))).collect();
        let order: Vec<String> = encode(&entries, XContentType::Json, false).iter().map(|b| String::from_utf8_lossy(&b[2..2 + b[1] as usize]).into_owned()).collect();
        assert_eq!(order, ["f", "k1", "k2", "k3", "k4", "k11", "k5", "k10", "k6", "k13", "k7", "k12", "k8", "k9", "o", "k14", "neg", "r"]);
    }
}
