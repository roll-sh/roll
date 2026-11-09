//! Parsing JSON text into documents. serde_json does the reading, as in RedisJSON, so syntax
//! errors carry the same messages ("expected value at line 1 column 1", nesting past 128
//! "recursion limit exceeded …"); containers are built element by element the way ijson builds
//! them, which decides their capacities and array element types.

use super::value::{Arr, J, N, Obj, Tag};
use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use std::fmt;

/// Parse a document. `fpha` stores numeric arrays as that float type (`JSON.SET … FPHA`).
pub fn parse(text: &str, fpha: Option<Tag>) -> Result<J, String> {
    let mut de = serde_json::Deserializer::from_str(text);
    let v = Seed(fpha).deserialize(&mut de).map_err(|e| e.to_string())?;
    de.end().map_err(|e| e.to_string())?;
    Ok(v)
}

#[derive(Clone, Copy)]
struct Seed(Option<Tag>);

impl<'de> DeserializeSeed<'de> for Seed {
    type Value = J;

    fn deserialize<D: Deserializer<'de>>(self, d: D) -> Result<J, D::Error> {
        d.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for Seed {
    type Value = J;

    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("any valid JSON value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<J, E> {
        Ok(J::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<J, E> {
        Ok(J::Num(N::I(v)))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<J, E> {
        Ok(J::Num(N::uint(v)))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<J, E> {
        Ok(N::float(v).map_or(J::Null, J::Num))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<J, E> {
        Ok(J::str(v))
    }

    fn visit_unit<E: de::Error>(self) -> Result<J, E> {
        Ok(J::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<J, A::Error> {
        let mut arr = Arr::new();
        while let Some(v) = seq.next_element_seed(self)? {
            match self.0 {
                Some(ft) => arr.push_fp(v, ft).map_err(de::Error::custom)?,
                None => arr.push(v),
            }
        }
        Ok(J::Arr(Box::new(arr)))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<J, A::Error> {
        let mut obj = Obj::new();
        while let Some(k) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            let v = map.next_value_seed(self)?;
            obj.insert(&k, v);
        }
        Ok(J::Obj(Box::new(obj)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_read_like_serde_json() {
        assert_eq!(parse("x", None).unwrap_err(), "expected value at line 1 column 1");
        assert_eq!(parse("foo", None).unwrap_err(), "expected ident at line 1 column 2");
        assert_eq!(parse("[1,2", None).unwrap_err(), "EOF while parsing a list at line 1 column 4");
        assert_eq!(parse("[1] x", None).unwrap_err(), "trailing characters at line 1 column 5");
        let deep = "[".repeat(128) + &"]".repeat(128);
        assert!(parse(&deep, None).unwrap_err().starts_with("recursion limit exceeded"));
        assert!(parse(&("[".repeat(127) + &"]".repeat(127)), None).is_ok());
    }

    #[test]
    fn containers_build_like_ijson() {
        let v = parse(r#"{"a":[1,2.5,"x"],"b":{"c":null}}"#, None).unwrap();
        assert_eq!(v.mem() + 8, 200);
        // Duplicate keys keep the first position.
        let v = parse(r#"{"a":1,"b":2,"a":3}"#, None).unwrap();
        let o = v.as_obj().unwrap();
        assert_eq!(o.keys().collect::<Vec<_>>(), ["a", "b"]);
        assert!(o.get("a").unwrap().same(&J::Num(N::I(3))));
    }

    #[test]
    fn fpha_rounds_into_the_float_type() {
        let v = parse("[0.1, 2, 3.3]", Some(Tag::F16)).unwrap();
        assert_eq!(v.as_arr().unwrap().tag(), Tag::F16);
        assert_eq!(parse("[1e10]", Some(Tag::F16)).unwrap_err(), "value out of range for F16 at line 1 column 6");
    }
}
