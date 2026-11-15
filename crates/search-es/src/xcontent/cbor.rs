//! CBOR (RFC 8949) bodies, as Jackson's CBOR generator writes them for Elasticsearch: objects
//! and arrays with indefinite length (`0xbf … 0xff`), text strings with definite length,
//! integers in the shortest encoding, doubles as float64 and big integers as tag 2/3 bignums.
//!
//! The reader accepts any well-formed CBOR data item that maps onto JSON (definite and
//! indefinite containers, half/single/double floats, bignums) and never sizes an allocation
//! from a length it hasn't seen the bytes for.

use crate::json::{Map, Number, Value};

pub fn write_cbor(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Null => out.push(0xf6),
        Value::Bool(b) => out.push(if *b { 0xf5 } else { 0xf4 }),
        Value::Num(Number::Int(i)) => {
            if *i >= 0 {
                head(out, 0, *i as u64);
            } else {
                head(out, 1, (-1 - *i) as u64);
            }
        }
        Value::Num(Number::UInt(u)) => head(out, 0, *u),
        Value::Num(Number::Big(s)) => {
            let (neg, digits) = match s.strip_prefix('-') {
                Some(d) => (true, d),
                None => (false, &s[..]),
            };
            let mut mag = decimal_to_be_bytes(digits);
            if neg {
                // Tag 3 encodes -1 - n.
                sub_one(&mut mag);
            }
            out.push(if neg { 0xc3 } else { 0xc2 });
            head(out, 2, mag.len() as u64);
            out.extend_from_slice(&mag);
        }
        Value::Num(Number::Float(f)) => {
            out.push(0xfb);
            out.extend_from_slice(&f.to_bits().to_be_bytes());
        }
        Value::Str(s) => {
            head(out, 3, s.len() as u64);
            out.extend_from_slice(s.as_bytes());
        }
        Value::Arr(a) => {
            out.push(0x9f);
            for v in a {
                crate::stack::maybe_grow(|| write_cbor(out, v));
            }
            out.push(0xff);
        }
        Value::Obj(m) => {
            out.push(0xbf);
            for (k, v) in m {
                head(out, 3, k.len() as u64);
                out.extend_from_slice(k.as_bytes());
                crate::stack::maybe_grow(|| write_cbor(out, v));
            }
            out.push(0xff);
        }
    }
}

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    if n < 24 {
        out.push(m | n as u8);
    } else if n <= 0xff {
        out.extend_from_slice(&[m | 24, n as u8]);
    } else if n <= 0xffff {
        out.push(m | 25);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else if n <= 0xffff_ffff {
        out.push(m | 26);
        out.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend_from_slice(&n.to_be_bytes());
    }
}

fn decimal_to_be_bytes(digits: &str) -> Vec<u8> {
    // Big-endian base-256 accumulator.
    let mut mag: Vec<u8> = vec![0];
    for d in digits.bytes().filter(u8::is_ascii_digit) {
        let mut carry = (d - b'0') as u32;
        for b in mag.iter_mut().rev() {
            let x = *b as u32 * 10 + carry;
            *b = (x & 0xff) as u8;
            carry = x >> 8;
        }
        while carry > 0 {
            mag.insert(0, (carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    while mag.len() > 1 && mag[0] == 0 {
        mag.remove(0);
    }
    mag
}

fn sub_one(mag: &mut Vec<u8>) {
    for b in mag.iter_mut().rev() {
        if *b > 0 {
            *b -= 1;
            break;
        }
        *b = 0xff;
    }
    while mag.len() > 1 && mag[0] == 0 {
        mag.remove(0);
    }
}

fn add_one(mag: &mut Vec<u8>) {
    for b in mag.iter_mut().rev() {
        if *b < 0xff {
            *b += 1;
            return;
        }
        *b = 0;
    }
    mag.insert(0, 1);
}

fn be_bytes_to_decimal(mag: &[u8]) -> String {
    let mut digits: Vec<u8> = vec![0];
    for &byte in mag {
        let mut carry = byte as u32;
        for d in digits.iter_mut() {
            let x = *d as u32 * 256 + carry;
            *d = (x % 10) as u8;
            carry = x / 10;
        }
        while carry > 0 {
            digits.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    while digits.len() > 1 && *digits.last().unwrap() == 0 {
        digits.pop();
    }
    digits.iter().rev().map(|d| (b'0' + d) as char).collect()
}

/// Reads one CBOR data item.
pub fn read_cbor(input: &[u8]) -> Result<Value, String> {
    let mut r = Reader { s: input, pos: 0, depth: 0 };
    r.item()
}

struct Reader<'a> {
    s: &'a [u8],
    pos: usize,
    depth: usize,
}

const BREAK: u8 = 0xff;

impl<'a> Reader<'a> {
    fn byte(&mut self) -> Result<u8, String> {
        let b = *self.s.get(self.pos).ok_or("Unexpected end-of-input in CBOR content")?;
        self.pos += 1;
        Ok(b)
    }

    fn take(&mut self, n: u64) -> Result<&'a [u8], String> {
        let n = usize::try_from(n).map_err(|_| "CBOR length out of range")?;
        if self.s.len() - self.pos < n {
            return Err("Unexpected end-of-input in CBOR content".into());
        }
        let out = &self.s[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    fn arg(&mut self, info: u8) -> Result<Option<u64>, String> {
        Ok(Some(match info {
            0..=23 => info as u64,
            24 => self.byte()? as u64,
            25 => u16::from_be_bytes(self.take(2)?.try_into().unwrap()) as u64,
            26 => u32::from_be_bytes(self.take(4)?.try_into().unwrap()) as u64,
            27 => u64::from_be_bytes(self.take(8)?.try_into().unwrap()),
            31 => return Ok(None),
            _ => return Err(format!("Invalid CBOR additional info {info}")),
        }))
    }

    fn string_chunks(&mut self, major: u8, info: u8) -> Result<Vec<u8>, String> {
        match self.arg(info)? {
            Some(n) => Ok(self.take(n)?.to_vec()),
            None => {
                let mut out = Vec::new();
                loop {
                    let b = self.byte()?;
                    if b == BREAK {
                        return Ok(out);
                    }
                    if b >> 5 != major {
                        return Err("Invalid chunk in indefinite-length CBOR string".into());
                    }
                    let n = self.arg(b & 0x1f)?.ok_or("Nested indefinite-length CBOR string")?;
                    out.extend_from_slice(self.take(n)?);
                }
            }
        }
    }

    fn item(&mut self) -> Result<Value, String> {
        self.depth += 1;
        if self.depth > 1000 {
            return Err("CBOR nesting too deep".into());
        }
        let b = self.byte()?;
        let (major, info) = (b >> 5, b & 0x1f);
        let v = match major {
            0 => Value::from(self.arg(info)?.ok_or("Invalid indefinite-length integer")?),
            1 => {
                let n = self.arg(info)?.ok_or("Invalid indefinite-length integer")?;
                if n <= i64::MAX as u64 {
                    Value::Num(Number::Int(-1 - n as i64))
                } else {
                    let mut mag = n.to_be_bytes().to_vec();
                    add_one(&mut mag);
                    Value::Num(Number::Big(format!("-{}", be_bytes_to_decimal(&mag)).into()))
                }
            }
            2 => {
                let bytes = self.string_chunks(2, info)?;
                use base64::Engine;
                Value::Str(base64::engine::general_purpose::STANDARD.encode(bytes))
            }
            3 => {
                let bytes = self.string_chunks(3, info)?;
                Value::Str(String::from_utf8(bytes).map_err(|_| "Invalid UTF-8 in CBOR text string")?)
            }
            4 => {
                let mut items = Vec::new();
                match self.arg(info)? {
                    Some(n) => {
                        for _ in 0..n {
                            items.push(crate::stack::maybe_grow(|| self.item())?);
                        }
                    }
                    None => loop {
                        if self.s.get(self.pos) == Some(&BREAK) {
                            self.pos += 1;
                            break;
                        }
                        items.push(crate::stack::maybe_grow(|| self.item())?);
                    },
                }
                Value::Arr(items)
            }
            5 => {
                let mut m = Map::new();
                let n = self.arg(info)?;
                let mut i = 0u64;
                loop {
                    match n {
                        Some(n) if i >= n => break,
                        None if self.s.get(self.pos) == Some(&BREAK) => {
                            self.pos += 1;
                            break;
                        }
                        _ => {}
                    }
                    let k = match self.item()? {
                        Value::Str(s) => s,
                        other => other.to_es_string(),
                    };
                    let v = crate::stack::maybe_grow(|| self.item())?;
                    if m.contains_key(&k) {
                        return Err(format!("Duplicate field '{k}'"));
                    }
                    m.insert(k, v);
                    i += 1;
                }
                Value::Obj(m)
            }
            6 => {
                let tag = self.arg(info)?.ok_or("Invalid CBOR tag")?;
                let inner = crate::stack::maybe_grow(|| self.item())?;
                match (tag, &inner) {
                    (2 | 3, Value::Str(b64)) => {
                        use base64::Engine;
                        let mut mag = base64::engine::general_purpose::STANDARD.decode(b64).unwrap_or_default();
                        if tag == 3 {
                            add_one(&mut mag);
                        }
                        let s = be_bytes_to_decimal(&mag);
                        let s = if tag == 3 { format!("-{s}") } else { s };
                        match s.parse::<i64>() {
                            Ok(i) => Value::from(i),
                            Err(_) => Value::Num(Number::Big(s.into())),
                        }
                    }
                    _ => inner,
                }
            }
            _ => match info {
                20 => Value::Bool(false),
                21 => Value::Bool(true),
                22 | 23 => Value::Null,
                25 => {
                    let h = u16::from_be_bytes(self.take(2)?.try_into().unwrap());
                    Value::from(half_to_f64(h))
                }
                26 => Value::from(f32::from_bits(u32::from_be_bytes(self.take(4)?.try_into().unwrap())) as f64),
                27 => Value::from(f64::from_bits(u64::from_be_bytes(self.take(8)?.try_into().unwrap()))),
                _ => return Err(format!("Unexpected CBOR simple value {info}")),
            },
        };
        self.depth -= 1;
        Ok(v)
    }
}

fn half_to_f64(h: u16) -> f64 {
    let exp = (h >> 10) & 0x1f;
    let mant = (h & 0x3ff) as f64;
    let v = match exp {
        0 => mant * 2f64.powi(-24),
        31 => {
            if mant == 0.0 {
                f64::INFINITY
            } else {
                f64::NAN
            }
        }
        e => (mant + 1024.0) * 2f64.powi(e as i32 - 25),
    };
    if h & 0x8000 != 0 { -v } else { v }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{arr, obj};

    #[test]
    fn round_trip() {
        let v = obj! {
            "a" => 1, "b" => -500, "c" => 1.5, "d" => "héllo", "e" => arr![true, false, Value::Null],
            "f" => Value::Num(Number::Big("123456789012345678901234567890".into())),
            "g" => Value::Num(Number::Big("-123456789012345678901234567890".into())),
            "h" => u64::MAX, "i" => obj! {}
        };
        let mut out = Vec::new();
        write_cbor(&mut out, &v);
        assert_eq!(out[0], 0xbf);
        assert_eq!(read_cbor(&out).unwrap(), v);
    }

    #[test]
    fn definite_and_half() {
        // {"a": [1, 1.5 (half)]}
        let bytes = [0xa1, 0x61, b'a', 0x82, 0x01, 0xf9, 0x3e, 0x00];
        assert_eq!(read_cbor(&bytes).unwrap().to_json(), r#"{"a":[1,1.5]}"#);
    }

    #[test]
    fn hostile_lengths() {
        // A text string claiming 2^63 bytes.
        let bytes = [0x7b, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff];
        assert!(read_cbor(&bytes).is_err());
        // An array claiming 2^32 items with none present.
        assert!(read_cbor(&[0x9a, 0xff, 0xff, 0xff, 0xff]).is_err());
    }
}
