//! Writing documents as JSON text, through serde_json like RedisJSON (ryu for doubles, itoa for
//! integers, the same escaping), with its INDENT/NEWLINE/SPACE formatter.
//!
//! Elements of float arrays print in their own precision: F32 as the shortest f32, F16/BF16 as
//! the fewest significant digits (up to 5) that read back to the same half value.

use super::value::{Arr, Data, J, N, Node, Obj};
use half::{bf16, f16};
use serde::ser::{SerializeMap, SerializeSeq, Serializer};
use serde::Serialize;
use std::io;

impl Serialize for N {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match *self {
            N::I(v) => s.serialize_i64(v),
            N::U(v) => s.serialize_u64(v),
            N::F(v) => s.serialize_f64(v),
        }
    }
}

impl Serialize for J {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            J::Null => s.serialize_unit(),
            J::Bool(b) => s.serialize_bool(*b),
            J::Num(n) => n.serialize(s),
            J::Str(v) => s.serialize_str(v),
            J::Arr(a) => a.serialize(s),
            J::Obj(o) => o.serialize(s),
        }
    }
}

impl Serialize for Node<'_> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Node::Ref(j) => j.serialize(s),
            Node::Num(n) => n.serialize(s),
        }
    }
}

fn round_to_sig_digits(v: f64, digits: i32) -> f64 {
    let magnitude = v.abs().log10().floor() as i32;
    let scale = 10f64.powi(digits - 1 - magnitude);
    (v * scale).round() / scale
}

/// The shortest decimal (1 to 5 significant digits) that reads back as the same half value.
pub fn shortest_half(v: f64, roundtrips: impl Fn(f64) -> bool) -> f64 {
    if !v.is_finite() || v.fract() == 0.0 {
        return v;
    }
    (1..=5).map(|d| round_to_sig_digits(v, d)).find(|&r| roundtrips(r)).unwrap_or(v)
}

pub fn f16_out(v: f16) -> f64 {
    shortest_half(f64::from(v), |p| f16::from_f64(p) == v)
}

pub fn bf16_out(v: bf16) -> f64 {
    shortest_half(f64::from(v), |p| bf16::from_f64(p) == v)
}

impl Serialize for Arr {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(self.len()))?;
        match self.data() {
            Data::F32(v) => v.iter().try_for_each(|x| seq.serialize_element(x))?,
            Data::F16(v) => v.iter().try_for_each(|x| seq.serialize_element(&f16_out(*x)))?,
            Data::BF16(v) => v.iter().try_for_each(|x| seq.serialize_element(&bf16_out(*x)))?,
            _ => self.iter().try_for_each(|x| seq.serialize_element(&x))?,
        }
        seq.end()
    }
}

impl Serialize for Obj {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.len()))?;
        for (k, v) in self.iter() {
            m.serialize_entry(k, v)?;
        }
        m.end()
    }
}

/// A value as its JSON text holds it: float array elements in the precision they print with,
/// every array heterogeneous. Computed results (projections) work on these.
pub fn plain(v: Node) -> J {
    match v {
        Node::Num(n) => J::Num(n),
        Node::Ref(j) => match j {
            J::Arr(a) => {
                let items: Vec<J> = match a.data() {
                    Data::F32(v) => v.iter().map(|x| J::Num(N::F(f64::from(*x)))).collect(),
                    Data::F16(v) => v.iter().map(|x| J::Num(N::F(f16_out(*x)))).collect(),
                    Data::BF16(v) => v.iter().map(|x| J::Num(N::F(bf16_out(*x)))).collect(),
                    _ => a.iter().map(plain).collect(),
                };
                J::Arr(Box::new(Arr::het_from(items)))
            }
            J::Obj(o) => {
                let mut out = Obj::with_capacity(o.len());
                for (k, v) in o.iter() {
                    out.insert(k, plain(Node::Ref(v)));
                }
                J::Obj(Box::new(out))
            }
            _ => j.clone(),
        },
    }
}

/// INDENT / NEWLINE / SPACE of JSON.GET.
#[derive(Clone, Copy, Debug, Default)]
pub struct Format<'a> {
    pub indent: Option<&'a str>,
    pub newline: Option<&'a str>,
    pub space: Option<&'a str>,
}

struct Formatter<'a> {
    depth: usize,
    has_value: bool,
    f: Format<'a>,
}

impl Formatter<'_> {
    fn new_line<W: io::Write + ?Sized>(&self, w: &mut W) -> io::Result<()> {
        if let Some(n) = self.f.newline {
            w.write_all(n.as_bytes())?;
        }
        if let Some(i) = self.f.indent {
            for _ in 0..self.depth {
                w.write_all(i.as_bytes())?;
            }
        }
        Ok(())
    }
}

impl serde_json::ser::Formatter for Formatter<'_> {
    // ryu like the serde_json RedisJSON links (1e20, not 1e+20); only finite values get here.
    fn write_f64<W: io::Write + ?Sized>(&mut self, w: &mut W, v: f64) -> io::Result<()> {
        w.write_all(ryu::Buffer::new().format_finite(v).as_bytes())
    }

    fn write_f32<W: io::Write + ?Sized>(&mut self, w: &mut W, v: f32) -> io::Result<()> {
        w.write_all(ryu::Buffer::new().format_finite(v).as_bytes())
    }

    fn begin_array<W: io::Write + ?Sized>(&mut self, w: &mut W) -> io::Result<()> {
        self.depth += 1;
        self.has_value = false;
        w.write_all(b"[")
    }

    fn end_array<W: io::Write + ?Sized>(&mut self, w: &mut W) -> io::Result<()> {
        self.depth -= 1;
        if self.has_value {
            self.new_line(w)?;
        }
        w.write_all(b"]")
    }

    fn begin_array_value<W: io::Write + ?Sized>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if !first {
            w.write_all(b",")?;
        }
        self.new_line(w)
    }

    fn end_array_value<W: io::Write + ?Sized>(&mut self, _w: &mut W) -> io::Result<()> {
        self.has_value = true;
        Ok(())
    }

    fn begin_object<W: io::Write + ?Sized>(&mut self, w: &mut W) -> io::Result<()> {
        self.depth += 1;
        self.has_value = false;
        w.write_all(b"{")
    }

    fn end_object<W: io::Write + ?Sized>(&mut self, w: &mut W) -> io::Result<()> {
        self.depth -= 1;
        if self.has_value {
            self.new_line(w)?;
        }
        w.write_all(b"}")
    }

    fn begin_object_key<W: io::Write + ?Sized>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
        if !first {
            w.write_all(b",")?;
        }
        self.new_line(w)
    }

    fn begin_object_value<W: io::Write + ?Sized>(&mut self, w: &mut W) -> io::Result<()> {
        w.write_all(b":")?;
        match self.f.space {
            Some(s) => w.write_all(s.as_bytes()),
            None => Ok(()),
        }
    }

    fn end_object_value<W: io::Write + ?Sized>(&mut self, _w: &mut W) -> io::Result<()> {
        self.has_value = true;
        Ok(())
    }
}

/// `v` as JSON text.
pub fn to_string<T: Serialize + ?Sized>(v: &T, f: Format) -> String {
    let mut out = serde_json::Serializer::with_formatter(Vec::new(), Formatter { depth: 0, has_value: false, f });
    v.serialize(&mut out).expect("serializing to memory");
    String::from_utf8(out.into_inner()).expect("JSON text is UTF-8")
}

/// Compact JSON text.
pub fn compact<T: Serialize + ?Sized>(v: &T) -> String {
    to_string(v, Format::default())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::de::parse;
    use crate::json::value::Tag;

    #[test]
    fn numbers_print_in_their_precision() {
        let v = parse(r#"[1,2.5,"x\n\u0001",1e20,-0.0,18446744073709551615]"#, None).unwrap();
        assert_eq!(compact(&v), r#"[1,2.5,"x\n\u0001",1e20,-0.0,18446744073709551615]"#);
        let v = parse("[0.1,0.2]", Some(Tag::F32)).unwrap();
        assert_eq!(compact(&v), "[0.1,0.2]");
        let v = parse("[0.1,3.14159]", Some(Tag::F16)).unwrap();
        assert_eq!(compact(&v), "[0.1,3.14]");
        assert_eq!(compact(&plain(Node::Ref(&parse("[0.1]", Some(Tag::F32)).unwrap()))), "[0.10000000149011612]");
    }

    #[test]
    fn formatting() {
        let v = parse(r#"{"a":[1,{}],"b":[]}"#, None).unwrap();
        let f = Format { indent: Some("  "), newline: Some("\n"), space: Some(" ") };
        assert_eq!(to_string(&v, f), "{\n  \"a\": [\n    1,\n    {}\n  ],\n  \"b\": []\n}");
    }
}
