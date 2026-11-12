//! Attribute values and items.
//!
//! An attribute value is DynamoDB JSON: a one-member object `{"<type>": <payload>}` with type
//! `S`, `N`, `B`, `BOOL`, `NULL`, `M`, `L`, `SS`, `NS` or `BS`. Alternator keeps values as JSON
//! and validates only what it must:
//!
//! - **Top-level values** (item attributes, ExpressionAttributeValues, AttributeUpdates values)
//!   are checked shallowly by [`validate`]: the object shape, the tag, `S`/`B`/`N` payload types,
//!   base64, the number, and that sets are non-empty arrays. `NULL`, `M` and `L` payloads and set
//!   elements are not checked at all.
//! - **Nested values** inside `M` and `L` are never validated; they are stored as the JSON they
//!   were sent as (re-serialised compactly; see [`crate::json`]) and returned verbatim.
//!
//! [`AttrValue`] is the typed view the evaluator uses; anything that does not have a valid shape
//! is [`AttrValue::Other`].

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use base64::engine::{GeneralPurpose, GeneralPurposeConfig};

use crate::error::{Error, ErrorKind, Result};
use crate::json::Json;
use crate::number::Number;
use crate::storage::single_member;

/// An item: attribute name to attribute value (DynamoDB JSON), sorted by name as the `:attrs`
/// map is.
pub type Item = BTreeMap<String, Json>;

/// The attribute type names.
pub const TYPES: [&str; 10] = ["S", "N", "B", "BOOL", "NULL", "M", "L", "SS", "NS", "BS"];

/// Base64 as Alternator reads it: the standard alphabet, padding required, but non-zero
/// trailing bits accepted (`AB==` decodes like `AA==`).
const LENIENT: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_allow_trailing_bits(true),
);

pub fn decode_base64(s: &str) -> Option<Vec<u8>> {
    LENIENT.decode(s).ok()
}

pub fn encode_base64(b: &[u8]) -> String {
    STANDARD.encode(b)
}

fn invalid_base64() -> Error {
    Error::new(ErrorKind::Serialization, "Invalid base64 data")
}

/// Validates a top-level attribute value. `ctx` prefixes the messages (`PutItem`,
/// `ExpressionAttributeValues`, `AttributeUpdates`, ...).
pub fn validate(v: &Json, ctx: &str) -> Result<()> {
    validate_shape(v, ctx)?;
    validate_payload(v)
}

/// The checks of [`validate`] that don't look inside N and B payloads (an
/// ExpressionAttributeValues entry is only checked this far until it is stored).
pub fn validate_shape(v: &Json, ctx: &str) -> Result<()> {
    let Some((tag, payload)) = single_member(v) else {
        return Err(Error::validation(format!(
            "{ctx}: improperly formatted value '{}'",
            v.to_json_string()
        )));
    };
    match tag {
        "S" => {
            if payload.as_str().is_none() {
                return Err(improper(ctx, v));
            }
        }
        "N" => {
            if payload.as_str().is_none() {
                return Err(Error::new(
                    ErrorKind::Serialization,
                    format!(
                        "{ctx}: number value must be encoded as string '{}'",
                        v.to_json_string()
                    ),
                ));
            }
        }
        "B" => {
            if payload.as_str().is_none() {
                return Err(improper(ctx, v));
            }
        }
        "BOOL" | "NULL" | "M" | "L" => {}
        "FLOAT32VECTOR" => {
            // A ScyllaDB extension: a list of JSON numbers stored as 32-bit floats. Its errors
            // always say `PutItem`.
            let Some(a) = payload.as_array() else {
                return Err(Error::validation(format!(
                    "PutItem: improperly formatted vector '{}'",
                    v.to_json_string()
                )));
            };
            for e in a {
                let Json::Number(n) = e else {
                    return Err(Error::validation(format!(
                        "PutItem: vector elements must be numbers, got '{}'",
                        e.to_json_string()
                    )));
                };
                if !(n.as_f64() as f32).is_finite() {
                    return Err(Error::validation(format!(
                        "PutItem: vector element '{}' cannot be represented as a 32-bit float",
                        n.text()
                    )));
                }
            }
        }
        "SS" | "NS" | "BS" => {
            let Some(a) = payload.as_array() else {
                return Err(Error::validation(format!(
                    "{ctx}: improperly formatted set '{}'",
                    v.to_json_string()
                )));
            };
            if a.is_empty() {
                return Err(Error::validation(format!("{ctx}: empty set not allowed")));
            }
        }
        _ => {
            return Err(Error::validation(format!(
                "{ctx}: unknown type {tag} for value {}",
                v.to_json_string()
            )));
        }
    }
    Ok(())
}

/// The payload checks of a value about to be stored: N parses as a number, B as base64.
pub fn validate_payload(v: &Json) -> Result<()> {
    match single_member(v) {
        Some(("BOOL", p)) if p.as_bool().is_none() => return Err(json_assert("IsBool()")),
        Some(("N", Json::String(s))) => {
            Number::parse(s)?;
        }
        Some(("B", Json::String(s))) if decode_base64(s).is_none() => {
            return Err(invalid_base64());
        }
        _ => {}
    }
    Ok(())
}

fn improper(ctx: &str, v: &Json) -> Error {
    Error::validation(format!(
        "{ctx}: improperly formatted value '{}'",
        v.to_json_string()
    ))
}

/// ScyllaDB's fallback for a request JSON value of an unexpected type: RapidJSON's assertion
/// text, which ScyllaDB follows with a backtrace (omitted here).
pub fn json_assert(condition: &str) -> Error {
    Error::validation(format!(
        "JSON assert failed on condition '{condition}', at: "
    ))
}

/// A typed view of an attribute value.
#[derive(Clone, Debug)]
pub enum AttrValue<'a> {
    S(&'a str),
    N(Number),
    B(Vec<u8>),
    Bool(bool),
    Null,
    M(&'a [(String, Json)]),
    L(&'a [Json]),
    SS(&'a [Json]),
    NS(&'a [Json]),
    BS(&'a [Json]),
    /// Not a well-formed value (an unvalidated nested value, a bad number...).
    Other,
}

impl<'a> AttrValue<'a> {
    pub fn of(v: &'a Json) -> AttrValue<'a> {
        let Some((tag, p)) = single_member(v) else {
            return AttrValue::Other;
        };
        match (tag, p) {
            ("S", Json::String(s)) => AttrValue::S(s),
            ("N", Json::String(s)) => match Number::parse(s) {
                Ok(n) => AttrValue::N(n),
                Err(_) => AttrValue::Other,
            },
            ("B", Json::String(s)) => match decode_base64(s) {
                Some(b) => AttrValue::B(b),
                None => AttrValue::Other,
            },
            ("BOOL", Json::Bool(b)) => AttrValue::Bool(*b),
            ("NULL", _) => AttrValue::Null,
            ("M", Json::Object(m)) => AttrValue::M(m),
            ("L", Json::Array(l)) => AttrValue::L(l),
            ("SS", Json::Array(l)) => AttrValue::SS(l),
            ("NS", Json::Array(l)) => AttrValue::NS(l),
            ("BS", Json::Array(l)) => AttrValue::BS(l),
            _ => AttrValue::Other,
        }
    }
}

/// The type tag of a value (`"S"`, `"M"`, ...), if it is a one-member object.
pub fn type_tag(v: &Json) -> Option<&str> {
    single_member(v).map(|(t, _)| t)
}

/// `{"S": s}` and friends.
pub fn s(v: impl Into<String>) -> Json {
    Json::object().with("S", Json::String(v.into()))
}

pub fn n(v: &Number) -> Json {
    Json::object().with("N", Json::String(v.text().to_string()))
}

pub fn tagged(tag: &str, payload: Json) -> Json {
    Json::object().with(tag, payload)
}
