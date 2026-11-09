//! How Alternator items are stored in CQL, byte for byte as ScyllaDB stores them.
//!
//! Table `T` is the CQL table `alternator_T.T` in keyspace `alternator_T`:
//!
//! ```cql
//! CREATE TABLE alternator_T.T (
//!     p text,                       -- hash key: S → text, N → decimal, B → blob
//!     c decimal,                    -- range key, if any (clustering column, ASC)
//!     ":attrs" map<text, blob>,     -- every other attribute
//!     PRIMARY KEY (p, c)
//! ) WITH CLUSTERING ORDER BY (c ASC)
//! ```
//!
//! Key attributes are ordinary columns in their CQL type (a number is a `decimal` holding the
//! unscaled value and scale exactly as sent). Every other top-level attribute is one entry of the
//! `:attrs` map, keyed by attribute name, whose value is a one-byte type tag and a payload:
//!
//! | tag  | type | payload |
//! |------|------|---------|
//! | `00` | S    | the UTF-8 string |
//! | `01` | B    | the raw bytes (base64-decoded) |
//! | `02` | BOOL | `00` or `01` |
//! | `03` | N    | a CQL `decimal`: big-endian `i32` scale, then the two's-complement unscaled value |
//! | `05` | FLOAT32VECTOR (a ScyllaDB extension) | big-endian 32-bit floats |
//! | `04` | anything else (NULL, M, L, SS, NS, BS) | the attribute value's DynamoDB JSON, e.g. `{"SS":["a","b"]}`, as Alternator serialises it (compact, members in order, nested numbers as sent) |
//!
//! Verified on ScyllaDB 2026.3.3: `{"n":{"N":"-12.3e5"}}` is stored as `03 fffffffc 85`
//! (scale -4, unscaled -123), `{"t":{"BOOL":true}}` as `02 01`, `{"e":{"S":""}}` as `00`, and
//! `{"z":{"NULL":true}}` as `04 {"NULL":true}`. Reading an `N` back prints the decimal in
//! ScyllaDB's form, so `1.50` reads back as `1.5` while a nested `1.50` stays `1.50`.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::json::{self, Json};
use crate::number::Number;

pub const TAG_S: u8 = 0;
pub const TAG_B: u8 = 1;
pub const TAG_BOOL: u8 = 2;
pub const TAG_N: u8 = 3;
pub const TAG_JSON: u8 = 4;
pub const TAG_FLOAT32VECTOR: u8 = 5;

/// The `:attrs` column name.
pub const ATTRS_COLUMN: &str = ":attrs";

/// The CQL keyspace of Alternator table `table`.
pub fn keyspace_name(table: &str) -> String {
    format!("alternator_{table}")
}

/// Serialises a validated top-level attribute value (`{"T": payload}`) into its `:attrs` blob.
pub fn encode_value(v: &Json) -> Vec<u8> {
    if let Some((tag, payload)) = single_member(v) {
        match (tag, payload) {
            ("S", Json::String(s)) => return prefixed(TAG_S, s.as_bytes()),
            ("B", Json::String(s)) => {
                if let Some(b) = crate::value::decode_base64(s) {
                    return prefixed(TAG_B, &b);
                }
            }
            ("BOOL", Json::Bool(b)) => return vec![TAG_BOOL, *b as u8],
            ("N", Json::String(s)) => {
                if let Ok(n) = Number::parse(s) {
                    return prefixed(TAG_N, &n.to_decimal_bytes());
                }
            }
            ("FLOAT32VECTOR", Json::Array(a)) => {
                let mut out = Vec::with_capacity(1 + 4 * a.len());
                out.push(TAG_FLOAT32VECTOR);
                for e in a {
                    let f = e.as_number().map_or(0.0, |n| n.as_f64()) as f32;
                    out.extend_from_slice(&f.to_be_bytes());
                }
                return out;
            }
            _ => {}
        }
    }
    let mut out = vec![TAG_JSON];
    out.extend_from_slice(v.to_json_string().as_bytes());
    out
}

/// Deserialises an `:attrs` blob. `None` if it is malformed (never produced by Alternator).
pub fn decode_value(b: &[u8]) -> Option<Json> {
    let (&tag, payload) = b.split_first()?;
    let one = |t: &str, v: Json| Json::object().with(t, v);
    Some(match tag {
        TAG_S => one("S", Json::String(String::from_utf8(payload.to_vec()).ok()?)),
        TAG_B => one("B", Json::String(STANDARD.encode(payload))),
        TAG_BOOL => one("BOOL", Json::Bool(*payload.first()? != 0)),
        TAG_N => one(
            "N",
            Json::String(Number::from_decimal_bytes(payload)?.text().to_string()),
        ),
        TAG_JSON => json::parse(payload).ok()?,
        TAG_FLOAT32VECTOR => one(
            "FLOAT32VECTOR",
            Json::Array(
                payload
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| Json::f64(f32::from_be_bytes(*c) as f64))
                    .collect(),
            ),
        ),
        _ => return None,
    })
}

/// The CQL serialisation of a key attribute value (`text`, `blob` or `decimal`).
pub fn encode_key(v: &Json) -> Option<Vec<u8>> {
    let (tag, payload) = single_member(v)?;
    let s = payload.as_str()?;
    match tag {
        "S" => Some(s.as_bytes().to_vec()),
        "B" => crate::value::decode_base64(s),
        "N" => Some(Number::parse(s).ok()?.to_decimal_bytes()),
        _ => None,
    }
}

/// The attribute value of a key column.
pub fn decode_key(ty: crate::schema::ScalarType, b: &[u8]) -> Option<Json> {
    use crate::schema::ScalarType;
    let (t, v) = match ty {
        ScalarType::S => ("S", String::from_utf8(b.to_vec()).ok()?),
        ScalarType::B => ("B", STANDARD.encode(b)),
        ScalarType::N => ("N", Number::from_decimal_bytes(b)?.text().to_string()),
    };
    Some(Json::object().with(t, Json::String(v)))
}

fn prefixed(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 1);
    out.push(tag);
    out.extend_from_slice(payload);
    out
}

/// `(tag, payload)` of a one-member object.
pub fn single_member(v: &Json) -> Option<(&str, &Json)> {
    match v.as_object()? {
        [(k, p)] => Some((k.as_str(), p)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::ScalarType;

    fn j(s: &str) -> Json {
        json::parse(s.as_bytes()).unwrap()
    }

    #[test]
    fn encodings_match_scylla() {
        assert_eq!(
            hex::encode(encode_value(&j(r#"{"N":"-12.3e5"}"#))),
            "03fffffffc85"
        );
        assert_eq!(hex::encode(encode_value(&j(r#"{"BOOL":true}"#))), "0201");
        assert_eq!(hex::encode(encode_value(&j(r#"{"BOOL":false}"#))), "0200");
        assert_eq!(hex::encode(encode_value(&j(r#"{"S":""}"#))), "00");
        assert_eq!(
            hex::encode(encode_value(&j(r#"{"S":"hello"}"#))),
            "0068656c6c6f"
        );
        assert_eq!(hex::encode(encode_value(&j(r#"{"B":"AQID"}"#))), "01010203");
        assert_eq!(
            encode_value(&j(r#"{"NULL":true}"#)),
            b"\x04{\"NULL\":true}".to_vec()
        );
        assert_eq!(
            encode_value(&j(r#"{"NS":["3","1","2.0"]}"#)),
            b"\x04{\"NS\":[\"3\",\"1\",\"2.0\"]}".to_vec()
        );
        assert_eq!(
            encode_value(&j(r#"{"M":{"x":{"N":"1"},"y":{"S":"q"}}}"#)),
            b"\x04{\"M\":{\"x\":{\"N\":\"1\"},\"y\":{\"S\":\"q\"}}}".to_vec()
        );
    }

    #[test]
    fn round_trips_normalise_top_level_numbers() {
        let back = |s: &str| decode_value(&encode_value(&j(s))).unwrap().to_json_string();
        assert_eq!(back(r#"{"N":"1.50"}"#), r#"{"N":"1.5"}"#);
        assert_eq!(back(r#"{"N":"-12.3e5"}"#), r#"{"N":"-1230000"}"#);
        assert_eq!(back(r#"{"L":[{"N":"1.50"}]}"#), r#"{"L":[{"N":"1.50"}]}"#);
        assert_eq!(back(r#"{"B":"AQID"}"#), r#"{"B":"AQID"}"#);
        assert_eq!(back(r#"{"S":"é"}"#), r#"{"S":"é"}"#);
        assert!(decode_value(&[9]).is_none());
        // Verified on ScyllaDB: [1, 0.5, -2] is stored as 05 3f800000 3f000000 c0000000.
        let v = encode_value(&j(r#"{"FLOAT32VECTOR":[1,0.5,-2]}"#));
        assert_eq!(hex::encode(&v), "053f8000003f000000c0000000");
        assert_eq!(
            decode_value(&v).unwrap().to_json_string(),
            r#"{"FLOAT32VECTOR":[1.0,0.5,-2.0]}"#
        );
        assert!(decode_value(&[]).is_none());
    }

    #[test]
    fn keys() {
        assert_eq!(
            encode_key(&j(r#"{"N":"1.50"}"#)).unwrap(),
            vec![0, 0, 0, 2, 0, 150]
        );
        assert_eq!(encode_key(&j(r#"{"S":"ab"}"#)).unwrap(), b"ab".to_vec());
        let n = decode_key(ScalarType::N, &[0, 0, 0, 2, 0, 150]).unwrap();
        assert_eq!(n.to_json_string(), r#"{"N":"1.5"}"#);
        assert_eq!(keyspace_name("T"), "alternator_T");
    }
}
