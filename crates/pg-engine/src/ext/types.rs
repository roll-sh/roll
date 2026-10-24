//! The extensions' data types the engine represents natively. A value of one is
//! `Value::Ext(kind, bytes)`: the kind says which type it is (and is stored with it), the
//! bytes are the type's own form. Its default btree operator class's order and equality are
//! the value's (`cmp`, `canon`), so sorting, grouping, hashing and index keys follow the
//! type as PostgreSQL's would, whatever runs them.
//!
//! A type becomes native when an extension's script creates it (CREATE TYPE in the script
//! of `extension`, named `name`): `crate::session::ddl` records the kind on the type.

use crate::Result;
use crate::types::Value;
use std::borrow::Cow;
use std::cmp::Ordering;

/// Kinds are stored with values: never renumber one.
pub const CITEXT: u16 = 1;

/// What the engine knows of one native type.
pub struct ExtType {
    pub kind: u16,
    /// The extension whose script creates it, and its name there.
    pub extension: &'static str,
    pub name: &'static str,
    /// The input function: text to the type's bytes.
    pub input: fn(&str) -> Result<Vec<u8>>,
    /// The output function.
    pub output: fn(&[u8]) -> String,
    /// The default btree operator class's comparison.
    pub cmp: fn(&[u8], &[u8]) -> Ordering,
    /// One form for all equal values (hashing).
    pub canon: fn(&[u8]) -> Cow<'_, [u8]>,
    /// An order-preserving index key (equal values, equal keys).
    pub key: fn(&[u8], &mut Vec<u8>),
    /// The built-in value a binary-coercible cast reads it as (citext as text), if any.
    pub to_builtin: Option<fn(&[u8]) -> Value>,
    /// The type's value made of a built-in one (a binary-coercible cast to it).
    pub from_builtin: Option<fn(&Value) -> Option<Vec<u8>>>,
}

static TYPES: &[ExtType] = &[super::citext::TYPE];

/// The native type `kind`.
pub fn get(kind: u16) -> Option<&'static ExtType> {
    TYPES.iter().find(|t| t.kind == kind)
}

/// The native type extension `ext`'s script creates as `name`, if the engine has one.
pub fn kind_for(ext: &str, name: &str) -> u16 {
    TYPES
        .iter()
        .find(|t| t.extension == ext && t.name == name)
        .map_or(0, |t| t.kind)
}

pub fn cmp(kind: u16, a: &[u8], b: &[u8]) -> Ordering {
    match get(kind) {
        Some(t) => (t.cmp)(a, b),
        None => a.cmp(b),
    }
}

pub fn canon(kind: u16, a: &[u8]) -> Cow<'_, [u8]> {
    match get(kind) {
        Some(t) => (t.canon)(a),
        None => Cow::Borrowed(a),
    }
}

pub fn key(kind: u16, a: &[u8], out: &mut Vec<u8>) {
    match get(kind) {
        Some(t) => (t.key)(a, out),
        None => out.extend_from_slice(a),
    }
}

pub fn output(kind: u16, a: &[u8]) -> String {
    match get(kind) {
        Some(t) => (t.output)(a),
        None => String::new(),
    }
}

pub fn input(kind: u16, s: &str) -> Result<Value> {
    let t = get(kind).ok_or_else(|| crate::error::internal("unknown native type"))?;
    Ok(Value::Ext(kind, (t.input)(s)?.into()))
}

/// `v` read as the built-in value a binary-coercible cast (or an internal function over the
/// type's representation) sees.
pub fn to_builtin(v: Value) -> Value {
    match &v {
        Value::Ext(k, b) => match get(*k).and_then(|t| t.to_builtin) {
            Some(f) => f(b),
            None => v,
        },
        _ => v,
    }
}

/// Whether a function parameter of type `t` takes a built-in value rather than an extension
/// type's own (a concrete built-in type, not a polymorphic one or a native type).
pub fn wants_builtin(t: &crate::types::DataType) -> bool {
    use crate::types::DataType as T;
    !t.is_polymorphic() && !matches!(t, T::Any | T::Unknown | T::Record | T::Row(_) | T::Array(_)) && !matches!(t, T::Base(b) if b.ext != 0)
}

/// Built-in value `v` as native type `kind`'s (unchanged when it can't be).
pub fn from_builtin(kind: u16, v: Value) -> Value {
    if matches!(v, Value::Ext(..) | Value::Null) {
        return v;
    }
    match get(kind).and_then(|t| t.from_builtin).and_then(|f| f(&v)) {
        Some(b) => Value::Ext(kind, b.into()),
        None => v,
    }
}

/// Put an order-preserving byte string in a key (0 escaped, terminated).
pub fn key_bytes(b: &[u8], out: &mut Vec<u8>) {
    for &c in b {
        if c == 0 {
            out.extend_from_slice(&[0x00, 0xFF]);
        } else {
            out.push(c);
        }
    }
    out.extend_from_slice(&[0x00, 0x01]);
}
