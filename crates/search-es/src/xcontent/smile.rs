//! SMILE, Jackson's binary JSON (FasterXML's "Smile Data Format" specification), read only:
//! what Elasticsearch writes its snapshot metadata in (`snap-*.dat`, `meta-*.dat`, shard
//! snapshots). Header `:)\n` and a flags byte; tokens for literals, zigzag variable-length
//! integers, floats in 7-bit groups, ASCII and Unicode strings of three length classes,
//! back references to earlier names and short values (when the header enables them), raw and
//! 7-bit encoded binary.
//!
//! The input is someone else's: nesting is bounded ([`Limits::depth`]), every length is checked
//! against the bytes left, and what the parsed tree takes is counted as it is built and handed
//! to [`Limits::charge`] in steps, so a caller can charge it to a breaker before it is held.
//! Binary values come back as [`Value::Str`] of their base64 (as Jackson renders binary in
//! JSON), or raw through [`parse_with_binary`].

use crate::json::{Map, Number, Value};

/// Bounds on one parse.
pub struct Limits<'a> {
    pub depth: usize,
    /// Called with bytes built, in steps of at least [`STEP`]: an error stops the parse.
    pub charge: &'a mut dyn FnMut(usize) -> Result<(), String>,
}

/// How much is built between two [`Limits::charge`] calls.
pub const STEP: usize = 64 << 10;

const SHARED_MAX: usize = 1024;

/// Jackson's `StreamReadConstraints` default: numbers of at most 1000 digits. A big integer's
/// two's complement bytes: 1000 decimal digits take at most 416 of them (and turning more into
/// digits costs quadratic time).
const MAX_NUMBER_BYTES: usize = 416;

struct Parser<'a, 'l, 'f> {
    b: &'a [u8],
    pos: usize,
    shared_names: bool,
    shared_values: bool,
    names: Vec<String>,
    values: Vec<String>,
    limits: Limits<'l>,
    built: usize,
    uncharged: usize,
    binary: &'f mut dyn FnMut(&[u8]) -> Value,
}

/// Parses a SMILE document into a value; binary values become base64 strings.
pub fn parse(b: &[u8], limits: Limits) -> Result<Value, String> {
    parse_with_binary(b, limits, &mut |raw| Value::Str(base64(raw)))
}

/// [`parse`], with binary values turned into values by `binary`.
pub fn parse_with_binary(b: &[u8], limits: Limits, binary: &mut dyn FnMut(&[u8]) -> Value) -> Result<Value, String> {
    if b.len() < 4 || &b[..3] != b":)\n" {
        return Err("not SMILE: no `:)\\n` header".into());
    }
    let flags = b[3];
    if flags >> 4 != 0 {
        return Err(format!("unsupported SMILE version [{}]", flags >> 4));
    }
    let mut p = Parser { b, pos: 4, shared_names: flags & 1 != 0, shared_values: flags & 2 != 0, names: Vec::new(), values: Vec::new(), limits, built: 0, uncharged: 0, binary };
    let v = p.value(0)?;
    p.flush()?;
    // Trailing content: an end marker, or nothing.
    if p.pos < p.b.len() && p.b[p.pos] != 0xFF {
        return Err(format!("trailing bytes at [{}]", p.pos));
    }
    Ok(v)
}

fn base64(raw: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(raw)
}

impl Parser<'_, '_, '_> {
    fn eof<T>(&self) -> Result<T, String> {
        Err(format!("unexpected end of SMILE content at [{}]", self.pos))
    }

    fn byte(&mut self) -> Result<u8, String> {
        match self.b.get(self.pos) {
            Some(&x) => {
                self.pos += 1;
                Ok(x)
            }
            None => self.eof(),
        }
    }

    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        if n > self.b.len() - self.pos {
            return self.eof();
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn built(&mut self, n: usize) -> Result<(), String> {
        self.built += n;
        self.uncharged += n;
        if self.uncharged >= STEP {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), String> {
        if self.uncharged > 0 {
            let n = std::mem::take(&mut self.uncharged);
            (self.limits.charge)(n)?;
        }
        Ok(())
    }

    /// An unsigned variable-length integer: 7 bits a byte, the last byte (high bit set) 6.
    fn vint(&mut self) -> Result<u64, String> {
        let mut v: u64 = 0;
        for _ in 0..10 {
            let x = self.byte()?;
            if x & 0x80 != 0 {
                return Ok(v << 6 | (x & 0x3F) as u64);
            }
            if v >> 57 != 0 {
                return Err("SMILE integer overflows".into());
            }
            v = v << 7 | x as u64;
        }
        Err("SMILE integer too long".into())
    }

    /// A big number's byte length, refused past [`MAX_NUMBER_BYTES`].
    fn number_len(&mut self) -> Result<usize, String> {
        let n = self.vint()?;
        if n > MAX_NUMBER_BYTES as u64 {
            return Err("a SMILE number longer than 1000 digits".into());
        }
        Ok(n as usize)
    }

    fn zigzag(v: u64) -> i64 {
        ((v >> 1) as i64) ^ -((v & 1) as i64)
    }

    /// `n` raw bytes in 7-bit groups (8 bytes for each 7; the last group's final byte holds the
    /// remaining bits, right-aligned).
    fn seven_bit(&mut self, n: usize) -> Result<Vec<u8>, String> {
        let encoded = n / 7 * 8 + if n.is_multiple_of(7) { 0 } else { n % 7 + 1 };
        if encoded > self.b.len() - self.pos {
            return self.eof();
        }
        let mut out = Vec::with_capacity(n);
        let mut i = 0;
        while out.len() + 7 <= n {
            let mut acc: u64 = 0;
            for _ in 0..8 {
                acc = acc << 7 | (self.b[self.pos + i] & 0x7F) as u64;
                i += 1;
            }
            out.extend_from_slice(&acc.to_be_bytes()[1..]);
        }
        let rest = n - out.len();
        if rest > 0 {
            let mut acc: u64 = 0;
            for _ in 0..rest {
                acc = acc << 7 | (self.b[self.pos + i] & 0x7F) as u64;
                i += 1;
            }
            let last_bits = rest * 8 - rest * 7;
            acc = acc << last_bits | (self.b[self.pos + i] as u64 & ((1 << last_bits) - 1));
            i += 1;
            for k in (0..rest).rev() {
                out.push((acc >> (8 * k)) as u8);
            }
        }
        self.pos += i;
        Ok(out)
    }

    fn text(&mut self, n: usize) -> Result<String, String> {
        let s = self.take(n)?;
        String::from_utf8(s.to_vec()).map_err(|_| "a SMILE string isn't UTF-8".to_string())
    }

    fn until_end_marker(&mut self) -> Result<String, String> {
        let Some(end) = self.b[self.pos..].iter().position(|&x| x == 0xFC) else { return self.eof() };
        let s = self.text(end)?;
        self.pos += 1;
        Ok(s)
    }

    fn remember(table: &mut Vec<String>, s: &str) {
        if table.len() >= SHARED_MAX {
            table.clear();
        }
        table.push(s.to_string());
    }

    fn shared_value(&mut self, i: usize) -> Result<Value, String> {
        match self.values.get(i) {
            Some(s) => {
                let s = s.clone();
                self.built(s.len() + std::mem::size_of::<Value>())?;
                Ok(Value::Str(s))
            }
            None => Err(format!("invalid SMILE shared value reference [{i}]")),
        }
    }

    fn short_string(&mut self, n: usize) -> Result<Value, String> {
        let s = self.text(n)?;
        if self.shared_values && n <= 64 {
            Self::remember(&mut self.values, &s);
        }
        self.built(s.len() + std::mem::size_of::<Value>())?;
        Ok(Value::Str(s))
    }

    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > self.limits.depth {
            return Err(format!("SMILE content nested deeper than [{}]", self.limits.depth));
        }
        let t = self.byte()?;
        let v = match t {
            0x01..=0x1F => return self.shared_value(t as usize - 1),
            0x20 => Value::Str(String::new()),
            0x21 => Value::Null,
            0x22 => Value::Bool(false),
            0x23 => Value::Bool(true),
            0x24 => Value::Num(Number::Int(Self::zigzag(self.vint()?) as i32 as i64)),
            0x25 => Value::Num(Number::Int(Self::zigzag(self.vint()?))),
            0x26 => {
                let n = self.number_len()?;
                let raw = self.seven_bit(n)?;
                Value::Num(big_integer(&raw))
            }
            0x28 => {
                let raw = self.take(5)?;
                let mut v: u64 = 0;
                for &x in raw {
                    v = v << 7 | (x & 0x7F) as u64;
                }
                Value::Num(Number::Float(f32::from_bits(v as u32) as f64))
            }
            0x29 => {
                let raw = self.take(10)?;
                let mut v: u128 = 0;
                for &x in raw {
                    v = v << 7 | (x & 0x7F) as u128;
                }
                Value::Num(Number::Float(f64::from_bits(v as u64)))
            }
            0x2A => {
                let scale = Self::zigzag(self.vint()?);
                let n = self.number_len()?;
                let raw = self.seven_bit(n)?;
                let unscaled = match big_integer(&raw) {
                    Number::Int(i) => i as f64,
                    Number::Big(d) => d.parse::<f64>().unwrap_or(f64::NAN),
                    _ => f64::NAN,
                };
                Value::Num(Number::Float(unscaled / 10f64.powi(scale.clamp(-400, 400) as i32)))
            }
            0x40..=0x5F => return self.short_string((t & 0x1F) as usize + 1),
            0x60..=0x7F => return self.short_string((t & 0x1F) as usize + 33),
            0x80..=0x9F => return self.short_string((t & 0x1F) as usize + 2),
            0xA0..=0xBF => return self.short_string((t & 0x1F) as usize + 34),
            0xC0..=0xDF => Value::Num(Number::Int(Self::zigzag((t & 0x1F) as u64))),
            0xE0 | 0xE4 => {
                let s = self.until_end_marker()?;
                self.built(s.len())?;
                Value::Str(s)
            }
            0xE8 => {
                let n = self.vint()? as usize;
                let raw = self.seven_bit(n)?;
                self.built(raw.len() * 2)?;
                (self.binary)(&raw)
            }
            0xEC..=0xEF => {
                let i = ((t & 3) as usize) << 8 | self.byte()? as usize;
                return self.shared_value(i);
            }
            0xF8 => {
                let mut a = Vec::new();
                loop {
                    if self.b.get(self.pos) == Some(&0xF9) {
                        self.pos += 1;
                        break;
                    }
                    a.push(self.value(depth + 1)?);
                }
                Value::Arr(a)
            }
            0xFA => {
                let mut m = Map::new();
                while let Some(key) = self.key()? {
                    let v = self.value(depth + 1)?;
                    self.built(key.len() + 16)?;
                    m.insert(key, v);
                }
                Value::Obj(m)
            }
            0xFD => {
                let n = self.vint()? as usize;
                let raw = self.take(n)?.to_vec();
                self.built(raw.len() * 2)?;
                (self.binary)(&raw)
            }
            t => return Err(format!("unexpected SMILE token [{t:#x}] at [{}]", self.pos - 1)),
        };
        self.built(std::mem::size_of::<Value>())?;
        Ok(v)
    }

    /// An object's next key, `None` at its end.
    fn key(&mut self) -> Result<Option<String>, String> {
        let t = self.byte()?;
        let name = match t {
            0xFB => return Ok(None),
            0x20 => return Ok(Some(String::new())),
            0x30..=0x33 => {
                let i = ((t & 3) as usize) << 8 | self.byte()? as usize;
                return self.names.get(i).cloned().map(Some).ok_or_else(|| format!("invalid SMILE shared name reference [{i}]"));
            }
            0x34 => return self.until_end_marker().map(Some),
            0x40..=0x7F => {
                let i = (t & 0x3F) as usize;
                return self.names.get(i).cloned().map(Some).ok_or_else(|| format!("invalid SMILE shared name reference [{i}]"));
            }
            0x80..=0xBF => self.text((t & 0x3F) as usize + 1)?,
            0xC0..=0xF7 => self.text((t & 0x3F) as usize + 2)?,
            t => return Err(format!("unexpected SMILE key token [{t:#x}] at [{}]", self.pos - 1)),
        };
        if self.shared_names {
            Self::remember(&mut self.names, &name);
        }
        Ok(Some(name))
    }
}

/// A two's complement big-endian integer.
fn big_integer(raw: &[u8]) -> Number {
    if raw.len() <= 8 && !raw.is_empty() {
        let neg = raw[0] & 0x80 != 0;
        let mut v: i64 = if neg { -1 } else { 0 };
        for &x in raw {
            v = v << 8 | x as i64;
        }
        return Number::Int(v);
    }
    // Wider: its decimal digits (rare in snapshot metadata).
    let neg = raw.first().is_some_and(|b| b & 0x80 != 0);
    let mut mag: Vec<u8> = if neg { raw.iter().map(|b| !b).collect() } else { raw.to_vec() };
    if neg {
        for b in mag.iter_mut().rev() {
            let (n, carry) = b.overflowing_add(1);
            *b = n;
            if !carry {
                break;
            }
        }
    }
    let mut digits = vec![0u8];
    for byte in mag {
        let mut carry = byte as u32;
        for d in digits.iter_mut() {
            let v = *d as u32 * 256 + carry;
            *d = (v % 10) as u8;
            carry = v / 10;
        }
        while carry > 0 {
            digits.push((carry % 10) as u8);
            carry /= 10;
        }
    }
    let mut s: String = digits.iter().rev().map(|d| (b'0' + d) as char).collect();
    if neg {
        s.insert(0, '-');
    }
    Number::Big(s.into_boxed_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_all(b: &[u8]) -> Result<Value, String> {
        let mut charged = 0;
        parse(b, Limits { depth: 10, charge: &mut |n| {
            charged += n;
            Ok(())
        } })
    }

    #[test]
    fn tokens() {
        // {"a":1,"bb":[true,null,"x"],"a":-3} with shared names: the second "a" is a reference.
        let doc = [b':', b')', b'\n', 0x03, 0xFA, 0x80, b'a', 0xC2, 0x81, b'b', b'b', 0xF8, 0x23, 0x21, 0x40, b'x', 0xF9, 0x40, 0xC5, 0xFB];
        let v = parse_all(&doc).unwrap();
        assert_eq!(v.to_json(), r#"{"a":-3,"bb":[true,null,"x"]}"#);
        // A long, a float32 and a raw binary value.
        let doc = [b':', b')', b'\n', 0x04, 0xF8, 0x25, 0x01, 0xBF, 0x28, 0x03, 0x7C, 0x00, 0x00, 0x00, 0xFD, 0x82, 1, 2, 0xF9];
        let v = parse_all(&doc).unwrap();
        assert_eq!(v.to_json(), r#"[-64,1.0,"AQI="]"#);
    }

    #[test]
    fn hostile_input() {
        assert!(parse_all(b"{}").is_err());
        // A string longer than the input, a binary declaring a huge length, deep nesting.
        assert!(parse_all(&[b':', b')', b'\n', 0, 0x7F, b'a']).is_err());
        assert!(parse_all(&[b':', b')', b'\n', 0, 0xFD, 0x7F, 0x7F, 0x7F, 0xBF]).is_err());
        let mut deep = vec![b':', b')', b'\n', 0];
        deep.extend(std::iter::repeat_n(0xF8, 100));
        assert!(parse_all(&deep).unwrap_err().contains("nested deeper"));
        // A big integer of 5,000 bytes (12,000 digits).
        let mut big = vec![b':', b')', b'\n', 0, 0x26, 0x4E, 0x88];
        big.extend(std::iter::repeat_n(0x7F, 5714));
        assert!(parse_all(&big).unwrap_err().contains("1000 digits"));
        // A reference to a name never seen.
        assert!(parse_all(&[b':', b')', b'\n', 1, 0xFA, 0x45, 0x21, 0xFB]).is_err());
    }
}
