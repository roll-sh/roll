//! The RDB form of a document (`ReJSON-RL` encoding version 4): CBOR, with numeric arrays as
//! RFC 8746 typed arrays (little-endian bytes under a tag; bfloat16 under tag 0x10000).
//! Integers take their shortest form, doubles the narrowest float that holds them exactly.

use super::value::{Arr, Data, J, MAX_DEPTH, N, Obj, each};
use half::{bf16, f16};

const TAG_U8: u64 = 64;
const TAG_U16: u64 = 69;
const TAG_U32: u64 = 70;
const TAG_U64: u64 = 71;
const TAG_I8: u64 = 72;
const TAG_I16: u64 = 77;
const TAG_I32: u64 = 78;
const TAG_I64: u64 = 79;
const TAG_F16: u64 = 84;
const TAG_F32: u64 = 85;
const TAG_F64: u64 = 86;
const TAG_BF16: u64 = 0x10000;

fn head(out: &mut Vec<u8>, major: u8, n: u64) {
    let m = major << 5;
    if n < 24 {
        out.push(m | n as u8);
    } else if n <= u8::MAX as u64 {
        out.extend([m | 24, n as u8]);
    } else if n <= u16::MAX as u64 {
        out.push(m | 25);
        out.extend((n as u16).to_be_bytes());
    } else if n <= u32::MAX as u64 {
        out.push(m | 26);
        out.extend((n as u32).to_be_bytes());
    } else {
        out.push(m | 27);
        out.extend(n.to_be_bytes());
    }
}

fn float(out: &mut Vec<u8>, v: f64) {
    let h = f16::from_f64(v);
    if f64::from(h) == v {
        out.push(0xf9);
        out.extend(h.to_bits().to_be_bytes());
    } else if f64::from(v as f32) == v {
        out.push(0xfa);
        out.extend((v as f32).to_bits().to_be_bytes());
    } else {
        out.push(0xfb);
        out.extend(v.to_bits().to_be_bytes());
    }
}

trait Le: Copy {
    const SIZE: usize;
    const TAG: u64;
    fn put(self, out: &mut Vec<u8>);
    fn get(b: &[u8]) -> Self;
}

macro_rules! le {
    ($($t:ty, $n:literal, $tag:expr);* $(;)?) => {$(
        impl Le for $t {
            const SIZE: usize = $n;
            const TAG: u64 = $tag;
            fn put(self, out: &mut Vec<u8>) {
                out.extend(self.to_le_bytes());
            }
            fn get(b: &[u8]) -> Self {
                <$t>::from_le_bytes(b.try_into().expect("element width"))
            }
        }
    )*};
}
le!(u8, 1, TAG_U8; i8, 1, TAG_I8; u16, 2, TAG_U16; i16, 2, TAG_I16; u32, 4, TAG_U32; i32, 4, TAG_I32;
    u64, 8, TAG_U64; i64, 8, TAG_I64; f16, 2, TAG_F16; bf16, 2, TAG_BF16; f32, 4, TAG_F32; f64, 8, TAG_F64);

fn typed<T: Le>(out: &mut Vec<u8>, v: &[T]) {
    head(out, 6, T::TAG);
    head(out, 2, (v.len() * T::SIZE) as u64);
    for x in v {
        x.put(out);
    }
}

fn put(out: &mut Vec<u8>, v: &J) {
    match v {
        J::Null => out.push(0xf6),
        J::Bool(b) => out.push(if *b { 0xf5 } else { 0xf4 }),
        J::Num(N::I(i)) if *i < 0 => head(out, 1, !(*i as u64)),
        J::Num(N::I(i)) => head(out, 0, *i as u64),
        J::Num(N::U(u)) => head(out, 0, *u),
        J::Num(N::F(f)) => float(out, *f),
        J::Str(s) => {
            head(out, 3, s.len() as u64);
            out.extend(s.as_bytes());
        }
        J::Arr(a) => each!(a.data(), d => typed(out, d), h => {
            head(out, 4, h.len() as u64);
            for x in h {
                put(out, x);
            }
        }),
        J::Obj(o) => {
            head(out, 5, o.len() as u64);
            for (k, v) in o.iter() {
                head(out, 3, k.len() as u64);
                out.extend(k.as_bytes());
                put(out, v);
            }
        }
    }
}

pub fn encode(v: &J) -> Vec<u8> {
    let mut out = Vec::new();
    put(&mut out, v);
    out
}

/// A CBOR item as read.
enum Item {
    Null,
    Bool(bool),
    Int(i128),
    Float(f64),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Item>),
    Map(Vec<(Item, Item)>),
    Tag(u64, Box<Item>),
    Other,
}

struct Reader<'a> {
    b: &'a [u8],
}

type R<T> = Result<T, ()>;

impl Reader<'_> {
    fn byte(&mut self) -> R<u8> {
        let (&x, rest) = self.b.split_first().ok_or(())?;
        self.b = rest;
        Ok(x)
    }

    fn take(&mut self, n: usize) -> R<&[u8]> {
        if self.b.len() < n {
            return Err(());
        }
        let (x, rest) = self.b.split_at(n);
        self.b = rest;
        Ok(x)
    }

    /// The argument of an initial byte; `None` for indefinite length.
    fn arg(&mut self, info: u8) -> R<Option<u64>> {
        Ok(Some(match info {
            0..=23 => info as u64,
            24 => self.byte()? as u64,
            25 => u16::from_be_bytes(self.take(2)?.try_into().map_err(|_| ())?) as u64,
            26 => u32::from_be_bytes(self.take(4)?.try_into().map_err(|_| ())?) as u64,
            27 => u64::from_be_bytes(self.take(8)?.try_into().map_err(|_| ())?),
            31 => return Ok(None),
            _ => return Err(()),
        }))
    }

    fn at_break(&self) -> bool {
        self.b.first() == Some(&0xff)
    }

    /// Definite or indefinite (chunked) string bytes of major type `major`.
    fn chunks(&mut self, major: u8, len: Option<u64>) -> R<Vec<u8>> {
        match len {
            Some(n) => Ok(self.take(usize::try_from(n).map_err(|_| ())?)?.to_vec()),
            None => {
                let mut out = Vec::new();
                while !self.at_break() {
                    let ib = self.byte()?;
                    if ib >> 5 != major {
                        return Err(());
                    }
                    let n = self.arg(ib & 31)?.ok_or(())?;
                    out.extend(self.take(usize::try_from(n).map_err(|_| ())?)?);
                }
                self.byte()?;
                Ok(out)
            }
        }
    }

    fn item(&mut self, depth: usize) -> R<Item> {
        if depth > 256 {
            return Err(());
        }
        let ib = self.byte()?;
        let (major, info) = (ib >> 5, ib & 31);
        if major == 7 {
            return Ok(match info {
                20 => Item::Bool(false),
                21 => Item::Bool(true),
                22 | 23 => Item::Null,
                25 => Item::Float(f64::from(f16::from_bits(u16::from_be_bytes(self.take(2)?.try_into().map_err(|_| ())?)))),
                26 => Item::Float(f64::from(f32::from_bits(u32::from_be_bytes(self.take(4)?.try_into().map_err(|_| ())?)))),
                27 => Item::Float(f64::from_bits(u64::from_be_bytes(self.take(8)?.try_into().map_err(|_| ())?))),
                0..=19 => Item::Other,
                24 => {
                    self.byte()?;
                    Item::Other
                }
                _ => return Err(()),
            });
        }
        let len = self.arg(info)?;
        Ok(match major {
            0 => Item::Int(len.ok_or(())? as i128),
            1 => Item::Int(-1 - len.ok_or(())? as i128),
            2 => Item::Bytes(self.chunks(2, len)?),
            3 => Item::Text(String::from_utf8(self.chunks(3, len)?).map_err(|_| ())?),
            4 => {
                let mut v = Vec::new();
                match len {
                    Some(n) => {
                        for _ in 0..n {
                            v.push(self.item(depth + 1)?);
                        }
                    }
                    None => {
                        while !self.at_break() {
                            v.push(self.item(depth + 1)?);
                        }
                        self.byte()?;
                    }
                }
                Item::Array(v)
            }
            5 => {
                let mut v = Vec::new();
                match len {
                    Some(n) => {
                        for _ in 0..n {
                            v.push((self.item(depth + 1)?, self.item(depth + 1)?));
                        }
                    }
                    None => {
                        while !self.at_break() {
                            v.push((self.item(depth + 1)?, self.item(depth + 1)?));
                        }
                        self.byte()?;
                    }
                }
                Item::Map(v)
            }
            _ => Item::Tag(len.ok_or(())?, Box::new(self.item(depth + 1)?)),
        })
    }
}

fn le_vec<T: Le>(b: &[u8]) -> R<Vec<T>> {
    if !b.len().is_multiple_of(T::SIZE) {
        return Err(());
    }
    Ok(b.chunks_exact(T::SIZE).map(T::get).collect())
}

fn to_json(item: Item, depth: usize) -> R<J> {
    if depth >= MAX_DEPTH {
        return Err(());
    }
    Ok(match item {
        Item::Null => J::Null,
        Item::Bool(b) => J::Bool(b),
        Item::Float(f) => N::float(f).map_or(J::Null, J::Num),
        Item::Int(i) => {
            if let Ok(v) = i64::try_from(i) {
                J::Num(N::I(v))
            } else {
                J::Num(N::uint(u64::try_from(i).map_err(|_| ())?))
            }
        }
        Item::Text(s) => J::str(&s),
        Item::Array(items) => {
            let mut a = Arr::with_capacity(items.len().min(1024));
            for v in items {
                a.push(to_json(v, depth + 1)?);
            }
            J::Arr(Box::new(a))
        }
        Item::Map(entries) => {
            let mut o = Obj::with_capacity(entries.len());
            for (k, v) in entries {
                let Item::Text(k) = k else { return Err(()) };
                o.insert(&k, to_json(v, depth + 1)?);
            }
            J::Obj(Box::new(o))
        }
        Item::Tag(tag, inner) => {
            let Item::Bytes(b) = *inner else { return Err(()) };
            let data = match tag {
                TAG_U8 => Data::U8(le_vec(&b)?),
                TAG_I8 => Data::I8(le_vec(&b)?),
                TAG_U16 => Data::U16(le_vec(&b)?),
                TAG_I16 => Data::I16(le_vec(&b)?),
                TAG_F16 => Data::F16(le_vec(&b)?),
                TAG_BF16 => Data::BF16(le_vec(&b)?),
                TAG_U32 => Data::U32(le_vec(&b)?),
                TAG_I32 => Data::I32(le_vec(&b)?),
                TAG_F32 => Data::F32(le_vec(&b)?),
                TAG_U64 => Data::U64(le_vec(&b)?),
                TAG_I64 => Data::I64(le_vec(&b)?),
                TAG_F64 => Data::F64(le_vec(&b)?),
                _ => return Err(()),
            };
            J::Arr(Box::new(Arr::from_data(data)))
        }
        Item::Bytes(_) | Item::Other => return Err(()),
    })
}

/// A document from its RDB bytes; `None` for anything RedisJSON would refuse.
pub fn decode(b: &[u8]) -> Option<J> {
    let mut r = Reader { b };
    let item = r.item(0).ok()?;
    to_json(item, 0).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::de::parse;
    use crate::json::ser::compact;
    use crate::json::value::Tag;

    #[test]
    fn matches_redisjson_bytes() {
        let v = parse(r#"{"a":[1,2.5,"x"],"b":{"c":null}}"#, None).unwrap();
        assert_eq!(encode(&v), [0xa2, 0x61, 0x61, 0x83, 0x01, 0xf9, 0x41, 0x00, 0x61, 0x78, 0x61, 0x62, 0xa1, 0x61, 0x63, 0xf6]);
    }

    #[test]
    fn round_trips() {
        for (text, fpha) in [
            (r#"[1,-1,-200,300,70000,5000000000,18446744073709551615,0.1,1e300,"long string here",true,null,{}]"#, None),
            ("[1,2,3]", None),
            ("[-1,2]", None),
            ("[0.5,1.25]", Some(Tag::F32)),
            ("[0.5,1.25]", Some(Tag::BF16)),
        ] {
            let v = parse(text, fpha).unwrap();
            let back = decode(&encode(&v)).unwrap();
            assert_eq!(compact(&back), compact(&v));
            assert_eq!(back.as_arr().unwrap().tag(), v.as_arr().unwrap().tag());
        }
    }
}
