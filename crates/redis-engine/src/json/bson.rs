//! BSON input (`JSON.SET … FORMAT BSON`): the argument is a BSON document whose first element
//! is the value, turned into relaxed extended JSON (ObjectIds as `{"$oid": …}`, dates as
//! `{"$date": "…"}`, …) and read like JSON text, as RedisJSON does it through the `bson` crate.
//! Malformed documents fail with that crate's messages.

use super::value::{Arr, J, N, Obj};
use super::ser::compact;

/// Nested documents deeper than this are refused rather than recursed into.
const MAX_NESTING: usize = 1024;

struct Error {
    key: Option<String>,
    msg: String,
}

impl Error {
    fn malformed(msg: impl Into<String>) -> Self {
        Error { key: None, msg: format!("malformed value: {:?}", msg.into()) }
    }

    fn utf8(e: std::str::Utf8Error) -> Self {
        Error { key: None, msg: format!("utf-8 encoding error: {e}") }
    }

    fn with_key(mut self, key: &str) -> Self {
        self.key = Some(key.to_string());
        self
    }

    fn text(self) -> String {
        match self.key {
            Some(k) => format!("error at key \"{k}\": {}", self.msg),
            None => self.msg,
        }
    }
}

type Result<T> = std::result::Result<T, Error>;

/// The document's first value as JSON text; `None` for an empty document.
pub fn first_value(input: &[u8]) -> std::result::Result<Option<String>, String> {
    let Some(head) = input.get(..4) else { return Err("failed to fill whole buffer".into()) };
    let length = i32::from_le_bytes(head.try_into().expect("4 bytes"));
    if length < 5 {
        return Err("document size too small".into());
    }
    let buf = &input[..(length as usize).min(input.len())];
    let doc = document(buf).map_err(Error::text)?;
    let mut first: Option<(&str, J)> = None;
    for item in elements(doc) {
        let (key, el) = item.map_err(Error::text)?;
        let v = el.value(0).map_err(Error::text)?;
        match &mut first {
            None => first = Some((key, v)),
            // A repeated key keeps its place and takes the later value.
            Some((k, slot)) if *k == key => *slot = v,
            Some(_) => {}
        }
    }
    Ok(first.map(|(_, v)| compact(&v)))
}

fn document(data: &[u8]) -> Result<&[u8]> {
    if data.len() < 5 {
        return Err(Error::malformed("document too short"));
    }
    let length = i32_at(data)?;
    if data.len() as i64 != length as i64 {
        return Err(Error::malformed("document length incorrect"));
    }
    if data[data.len() - 1] != 0 {
        return Err(Error::malformed("document not null-terminated"));
    }
    Ok(data)
}

fn i32_at(b: &[u8]) -> Result<i32> {
    b.get(..4)
        .map(|s| i32::from_le_bytes(s.try_into().expect("4 bytes")))
        .ok_or_else(|| Error::malformed(format!("expected 4 bytes to read i32, instead got {}", b.len())))
}

fn i64_at(b: &[u8]) -> Result<i64> {
    b.get(..8)
        .map(|s| i64::from_le_bytes(s.try_into().expect("8 bytes")))
        .ok_or_else(|| Error::malformed(format!("expected 8 bytes to read i64, instead got {}", b.len())))
}

fn cstring_at(doc: &[u8], at: usize) -> Result<&str> {
    let buf = &doc[at.min(doc.len())..];
    match buf.iter().position(|&b| b == 0) {
        Some(end) => std::str::from_utf8(&buf[..end]).map_err(Error::utf8),
        None => Err(Error::malformed("expected null terminator")),
    }
}

/// The length of a length-prefixed string, prefix included.
fn string_len(buf: &[u8]) -> Result<usize> {
    if buf.len() < 4 {
        return Err(Error::malformed(format!(
            "expected buffer with string to contain at least 4 bytes, but it only has {}",
            buf.len()
        )));
    }
    let length = i32_at(buf)?;
    let length = usize::try_from(length).map_err(|e| Error::malformed(e.to_string()))?;
    let end = length.checked_add(4).ok_or_else(|| Error::malformed("attempted to add with overflow"))?;
    if end < 5 {
        return Err(Error::malformed(format!("BSON length encoded string needs to be at least 5 bytes, instead got {end}")));
    }
    if buf.len() < end {
        return Err(Error::malformed(format!("expected buffer to contain at least {end} bytes, but it only has {}", buf.len())));
    }
    if buf[end - 1] != 0 {
        return Err(Error::malformed("expected string to be null-terminated"));
    }
    Ok(length + 4)
}

fn lenencoded(buf: &[u8]) -> Result<&str> {
    let end = string_len(buf)?;
    std::str::from_utf8(&buf[4..end - 1]).map_err(Error::utf8)
}

struct Element<'a> {
    key: &'a str,
    tag: u8,
    doc: &'a [u8],
    at: usize,
    size: usize,
}

fn enough(doc: &[u8], start: usize, n: usize) -> Result<()> {
    let end = start.checked_add(n).ok_or_else(|| Error::malformed("attempted to add with overflow"))?;
    if doc.get(start..end).is_none() {
        return Err(Error::malformed(format!(
            "length exceeds remaining length of buffer: {n} vs {}",
            doc.len().saturating_sub(start)
        )));
    }
    Ok(())
}

fn nested_len(doc: &[u8], at: usize) -> Result<usize> {
    enough(doc, at, 5)?;
    let size = i32_at(&doc[at..])? as usize;
    if size < 5 {
        return Err(Error::malformed(format!("document too small: {size} bytes")));
    }
    enough(doc, at, size)?;
    if doc[at + size - 1] != 0 {
        return Err(Error::malformed("not null terminated"));
    }
    Ok(size)
}

/// The elements of a document, stopping at the first malformed one.
fn elements(doc: &[u8]) -> impl Iterator<Item = Result<(&str, Element<'_>)>> + '_ {
    let mut offset = 4;
    let mut valid = true;
    std::iter::from_fn(move || {
        if !valid {
            return None;
        }
        if offset == doc.len() - 1 {
            if doc[offset] == 0 {
                return None;
            }
            valid = false;
            return Some(Err(Error::malformed("document not null terminated")));
        }
        if offset >= doc.len() {
            valid = false;
            return Some(Err(Error::malformed("iteration overflowed document")));
        }
        let key = match cstring_at(doc, offset + 1) {
            Ok(k) => k,
            Err(e) => {
                valid = false;
                return Some(Err(e));
            }
        };
        let tag = doc[offset];
        let at = offset + 1 + key.len() + 1;
        let size = (|| -> Result<usize> {
            let size = match tag {
                0x08 => 1,
                0x10 => 4,
                0x01 | 0x09 | 0x11 | 0x12 => 8,
                0x07 => 12,
                0x13 => 16,
                0x0A | 0x06 | 0xFF | 0x7F => 0,
                0x02 | 0x0D | 0x0E => string_len(&doc[at.min(doc.len())..])?,
                0x03 | 0x04 => nested_len(doc, at)?,
                0x05 => {
                    let len = i32_at(&doc[at.min(doc.len())..])?;
                    if len < 0 {
                        return Err(Error::malformed("lengths can't be negative"));
                    }
                    len as usize + 4 + 1
                }
                0x0B => {
                    let pattern = cstring_at(doc, at)?;
                    let options = cstring_at(doc, at + pattern.len() + 1)?;
                    pattern.len() + 1 + options.len() + 1
                }
                0x0C => string_len(&doc[at.min(doc.len())..])? + 12,
                0x0F => {
                    let len = i32_at(&doc[at.min(doc.len())..])?;
                    if len < 0 {
                        return Err(Error::malformed("lengths can't be negative"));
                    }
                    len as usize
                }
                _ => return Err(Error::malformed(format!("invalid tag: {tag}"))),
            };
            enough(doc, at, size)?;
            Ok(size)
        })()
        .map_err(|e| e.with_key(key));
        match size {
            Ok(size) => {
                offset = at + size;
                Some(Ok((key, Element { key, tag, doc, at, size })))
            }
            Err(e) => {
                valid = false;
                Some(Err(e))
            }
        }
    })
}

fn obj(members: Vec<(&str, J)>) -> J {
    let mut o = Obj::with_capacity(members.len());
    for (k, v) in members {
        o.insert(k, v);
    }
    J::Obj(Box::new(o))
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn base64(b: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

/// Days since 1970-01-01 as a civil date.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `{"$date": RFC 3339}` for dates in years 1970..=9999, else the milliseconds.
fn date(ms: i64) -> J {
    let (y, m, d) = civil(ms.div_euclid(86_400_000));
    if ms < 0 || y > 9999 {
        return obj(vec![("$date", obj(vec![("$numberLong", J::str(&ms.to_string()))]))]);
    }
    let in_day = ms.rem_euclid(86_400_000);
    let (h, mi, s, milli) = (in_day / 3_600_000, in_day / 60_000 % 60, in_day / 1000 % 60, in_day % 1000);
    let mut text = format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}");
    if milli != 0 {
        let frac = format!("{milli:03}");
        text.push('.');
        text.push_str(frac.trim_end_matches('0'));
    }
    text.push('Z');
    obj(vec![("$date", J::str(&text))])
}

/// Decimal128 as text (the `bson` crate's rendering).
fn decimal128(bytes: &[u8]) -> String {
    let v = u128::from_le_bytes(bytes.try_into().expect("16 bytes"));
    let sign = v >> 127 == 1;
    if (v >> 123) & 0xF == 0xF {
        if (v >> 122) & 1 == 1 {
            return "NaN".into();
        }
        return if sign { "-Infinity".into() } else { "Infinity".into() };
    }
    let (exp, coeff) = if (v >>125) & 3 == 3 {
        ((v >> 111) & 0x3FFF, (4u128 << 111) | (v & ((1u128 << 111) - 1)))
    } else {
        ((v >> 113) & 0x3FFF, v & ((1u128 << 113) - 1))
    };
    let coeff = if coeff > 9_999_999_999_999_999_999_999_999_999_999_999 { 0 } else { coeff };
    let exp = exp as i16 - 6176;
    let digits = coeff.to_string();
    let adj = exp + digits.len() as i16 - 1;
    let mut out = String::from(if sign { "-" } else { "" });
    if exp <= 0 && adj >= -6 {
        if exp == 0 {
            out.push_str(&digits);
        } else {
            let dec = exp.unsigned_abs() as usize;
            if dec >= digits.len() {
                out.push_str("0.");
                out.push_str(&"0".repeat(dec - digits.len()));
                out.push_str(&digits);
            } else {
                let (pre, post) = digits.split_at(digits.len() - dec);
                out.push_str(pre);
                out.push('.');
                out.push_str(post);
            }
        }
    } else {
        let (pre, post) = digits.split_at(1);
        out.push_str(pre);
        if !post.is_empty() {
            out.push('.');
            out.push_str(post);
        }
        out.push('E');
        if adj > 0 {
            out.push('+');
        }
        out.push_str(&adj.to_string());
    }
    out
}

fn special_double(s: &str) -> J {
    obj(vec![("$numberDouble", J::str(s))])
}

impl Element<'_> {
    fn slice(&self) -> &[u8] {
        &self.doc[self.at..self.at + self.size]
    }

    fn malformed(&self, msg: impl Into<String>) -> Error {
        Error::malformed(msg).with_key(self.key)
    }

    fn string(&self) -> Result<&str> {
        std::str::from_utf8(&self.slice()[4..self.size - 1]).map_err(Error::utf8)
    }

    fn oid(&self, at: usize) -> String {
        hex(&self.doc[at..at + 12])
    }

    /// The value in relaxed extended JSON.
    fn value(&self, depth: usize) -> Result<J> {
        if depth > MAX_NESTING {
            return Err(self.malformed("document nested too deeply"));
        }
        let s = self.slice();
        Ok(match self.tag {
            0x0A => J::Null,
            0x06 => obj(vec![("$undefined", J::Bool(true))]),
            0xFF => obj(vec![("$minKey", J::Num(N::I(1)))]),
            0x7F => obj(vec![("$maxKey", J::Num(N::I(1)))]),
            0x07 => obj(vec![("$oid", J::str(&self.oid(self.at)))]),
            0x10 => J::Num(N::I(i32_at(s)? as i64)),
            0x12 => J::Num(N::I(i64_at(s)?)),
            0x01 => {
                let v = f64::from_le_bytes(s.try_into().expect("8 bytes"));
                if v.is_nan() {
                    special_double(if v.is_sign_negative() { "-NaN" } else { "NaN" })
                } else if v.is_infinite() {
                    special_double(if v < 0.0 { "-Infinity" } else { "Infinity" })
                } else {
                    J::Num(N::F(v))
                }
            }
            0x02 => J::str(self.string()?),
            0x0D => obj(vec![("$code", J::str(self.string()?))]),
            0x0E => obj(vec![("$symbol", J::str(self.string()?))]),
            0x03 => {
                let d = document(s)?;
                let mut members: Vec<(&str, J)> = Vec::new();
                for item in elements(d) {
                    let (k, el) = item?;
                    let v = el.value(depth + 1)?;
                    match members.iter_mut().find(|(m, _)| *m == k) {
                        Some(slot) => slot.1 = v,
                        None => members.push((k, v)),
                    }
                }
                obj(members)
            }
            0x04 => {
                let d = document(s)?;
                let mut items = Vec::new();
                for item in elements(d) {
                    items.push(item?.1.value(depth + 1)?);
                }
                J::Arr(Box::new(Arr::het_from(items)))
            }
            0x08 => match s[0] {
                0 => J::Bool(false),
                1 => J::Bool(true),
                b => return Err(self.malformed(format!("boolean must be stored as 0 or 1, got {b}"))),
            },
            0x09 => date(i64_at(s)?),
            0x13 => obj(vec![("$numberDecimal", J::str(&decimal128(s)))]),
            0x0C => {
                let ns = lenencoded(s)?;
                let id = self.oid(self.at + self.size - 12);
                obj(vec![("$dbPointer", obj(vec![("$ref", J::str(ns)), ("$id", obj(vec![("$oid", J::str(&id))]))]))])
            }
            0x0B => {
                let pattern = cstring_at(self.doc, self.at)?;
                let options = cstring_at(self.doc, self.at + pattern.len() + 1)?;
                let mut opts: Vec<char> = options.chars().collect();
                opts.sort_unstable();
                let options: String = opts.into_iter().collect();
                obj(vec![("$regularExpression", obj(vec![("pattern", J::str(pattern)), ("options", J::str(&options))]))])
            }
            0x11 => {
                let increment = u32::from_le_bytes(s[..4].try_into().expect("4 bytes"));
                let time = u32::from_le_bytes(s[4..8].try_into().expect("4 bytes"));
                obj(vec![("$timestamp", obj(vec![("t", J::Num(N::I(time as i64))), ("i", J::Num(N::I(increment as i64)))]))])
            }
            0x05 => {
                let len = self.size.checked_sub(5).ok_or_else(|| self.malformed(format!("length exceeds maximum: {}", self.size)))?;
                if self.size >= i32::MAX as usize {
                    return Err(self.malformed(format!("binary length exceeds maximum: {len}")));
                }
                let subtype = self.doc[self.at + 4];
                let start = self.at + 5;
                let data = if subtype == 2 {
                    if len < 4 {
                        return Err(self.malformed("old binary subtype has no inner declared length"));
                    }
                    let old = i32_at(&self.doc[start..])? as usize;
                    if old.checked_add(4) != Some(len) {
                        return Err(self.malformed("old binary subtype has wrong inner declared length"));
                    }
                    &self.doc[start + 4..start + len]
                } else {
                    &self.doc[start..start + len]
                };
                obj(vec![("$binary", obj(vec![("base64", J::str(&base64(data))), ("subType", J::str(&hex(&[subtype])))]))])
            }
            0x0F => {
                if self.size < 14 {
                    return Err(self.malformed("code with scope length too small"));
                }
                let code = lenencoded(&s[4..])?;
                let scope_at = 4 + 4 + code.len() + 1;
                let scope = Element { key: self.key, tag: 0x03, doc: s, at: scope_at, size: s.len() - scope_at };
                obj(vec![("$code", J::str(code)), ("$scope", scope.value(depth + 1)?)])
            }
            _ => unreachable!("element types checked while iterating"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(elems: &[(u8, &str, &[u8])]) -> Vec<u8> {
        let mut body = Vec::new();
        for (t, k, v) in elems {
            body.push(*t);
            body.extend_from_slice(k.as_bytes());
            body.push(0);
            body.extend_from_slice(v);
        }
        let mut out = ((body.len() + 5) as i32).to_le_bytes().to_vec();
        out.extend(body);
        out.push(0);
        out
    }

    #[test]
    fn values_as_relaxed_extended_json() {
        let inner = doc(&[(0x10, "x", &5i32.to_le_bytes()), (0x01, "y", &1.5f64.to_le_bytes())]);
        let d = doc(&[(0x03, "a", &inner)]);
        assert_eq!(first_value(&d).unwrap().unwrap(), r#"{"x":5,"y":1.5}"#);
        let d = doc(&[(0x09, "d", &1_500i64.to_le_bytes())]);
        assert_eq!(first_value(&d).unwrap().unwrap(), r#"{"$date":"1970-01-01T00:00:01.5Z"}"#);
        let d = doc(&[(0x05, "b", &[3, 0, 0, 0, 0, b'a', b'b', b'c'])]);
        assert_eq!(first_value(&d).unwrap().unwrap(), r#"{"$binary":{"base64":"YWJj","subType":"00"}}"#);
        assert_eq!(first_value(&doc(&[])).unwrap(), None);
        assert_eq!(decimal128(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0x30]), "1");
    }

    #[test]
    fn malformed() {
        assert_eq!(first_value(b"ab").unwrap_err(), "failed to fill whole buffer");
        assert_eq!(first_value(&[3, 0, 0, 0]).unwrap_err(), "document size too small");
        assert_eq!(first_value(&[9, 0, 0, 0, 0]).unwrap_err(), r#"malformed value: "document length incorrect""#);
    }
}
