//! populate_record (`#=`) and the JSON conversions of hstore (hstore_io.c).

use super::Hs;
use super::build::hs_bytes;
use crate::Result;
use crate::error::{Error, code};
use crate::sql::func::FnCtx;
use crate::types::json::{Jsonb, write_json_string};
use crate::types::{DataType, Value};
use std::sync::Arc;

/// `hstore_populate_record`: a record of the first argument's type with the fields the
/// hstore has keys for replaced by their values (each through its type's input, its domain
/// checked); a NULL record starts all NULL, and then every field is checked.
pub(super) fn populate_record(a: &[Value], c: &FnCtx) -> Result<Value> {
    let ty = c.arg_type(0);
    let DataType::Row(rt) = &ty else {
        return Err(Error::new(code::DATATYPE_MISMATCH, "first argument must be a rowtype"));
    };
    let rec = match &a[0] {
        Value::Record(r) => Some(r.clone()),
        _ => None,
    };
    if a[1].is_null() {
        return Ok(rec.map_or(Value::Null, Value::Record));
    }
    let h = Hs::new(hs_bytes(&a[1]));
    if h.count() == 0
        && let Some(r) = &rec
    {
        return Ok(Value::Record(r.clone()));
    }
    let domains = crate::sql::func::json::field_domains(rt, c);
    let mut vals: Vec<Value> = match &rec {
        Some(r) => (**r).clone(),
        None => vec![Value::Null; rt.fields.len()],
    };
    for (i, (name, fty)) in rt.fields.iter().enumerate() {
        if name.starts_with("........pg.dropped.") {
            vals[i] = Value::Null;
            continue;
        }
        let idx = h.find(None, name.as_bytes());
        if idx.is_none() && rec.is_some() {
            continue;
        }
        let v = match idx.and_then(|i| h.val(i)) {
            Some(text) => {
                let s = String::from_utf8_lossy(text);
                crate::types::text::parse_value(&s, fty, c.exec.fmt())?
            }
            None => Value::Null,
        };
        if let Some(d) = domains.get(i).copied().flatten() {
            crate::sql::func::json::domain_check(d, &v, c)?;
        }
        vals[i] = v;
    }
    Ok(Value::Record(Arc::new(vals)))
}

/// `IsValidJsonNumber`: JSON's number syntax, the whole string.
fn json_number(s: &[u8]) -> bool {
    let mut i = 0;
    if s.first() == Some(&b'-') {
        i += 1;
    }
    let digits = |i: &mut usize| {
        let st = *i;
        while *i < s.len() && s[*i].is_ascii_digit() {
            *i += 1;
        }
        *i > st
    };
    match s.get(i) {
        Some(b'0') => i += 1,
        Some(c) if c.is_ascii_digit() => {
            digits(&mut i);
        }
        _ => return false,
    }
    if s.get(i) == Some(&b'.') {
        i += 1;
        if !digits(&mut i) {
            return false;
        }
    }
    if matches!(s.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(s.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !digits(&mut i) {
            return false;
        }
    }
    i == s.len()
}

fn text(b: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(b)
}

fn json(a: &[Value], loose: bool) -> Value {
    let h = Hs::new(hs_bytes(&a[0]));
    if h.count() == 0 {
        return Value::Json("{}".into());
    }
    let mut out = String::from("{");
    for (i, (k, v)) in h.pairs().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write_json_string(&mut out, &text(k));
        out.push_str(": ");
        match v {
            None => out.push_str("null"),
            Some(b"t") if loose => out.push_str("true"),
            Some(b"f") if loose => out.push_str("false"),
            Some(v) if loose && json_number(v) => out.push_str(&text(v)),
            Some(v) => write_json_string(&mut out, &text(v)),
        }
    }
    out.push('}');
    Value::Json(out.into())
}

pub(super) fn to_json(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(json(a, false))
}

pub(super) fn to_json_loose(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(json(a, true))
}

fn jsonb(a: &[Value], loose: bool) -> Result<Value> {
    let h = Hs::new(hs_bytes(&a[0]));
    let mut obj = Vec::with_capacity(h.count());
    for (k, v) in h.pairs() {
        let val = match v {
            None => Jsonb::Null,
            Some(b"t") if loose => Jsonb::Bool(true),
            Some(b"f") if loose => Jsonb::Bool(false),
            Some(v) if loose && json_number(v) => {
                Jsonb::Number(crate::types::numeric::Numeric::parse_pg(&text(v))?)
            }
            Some(v) => Jsonb::String(text(v).into_owned()),
        };
        obj.push((text(k).into_owned(), val));
    }
    // (Keys in hstore's order, which is jsonb's: by length, then bytes.)
    Ok(Value::Jsonb(Arc::new(Jsonb::Object(obj))))
}

pub(super) fn to_jsonb(a: &[Value], _: &FnCtx) -> Result<Value> {
    jsonb(a, false)
}

pub(super) fn to_jsonb_loose(a: &[Value], _: &FnCtx) -> Result<Value> {
    jsonb(a, true)
}

#[cfg(test)]
mod tests {
    #[test]
    fn json_numbers() {
        for (s, ok) in [("1", true), ("012345", false), ("-1.5e+3", true), ("2.345e+4", true), ("1.", false), ("-", false), ("+1", false), ("0.345e-4", true), ("2016-01-01", false)] {
            assert_eq!(super::json_number(s.as_bytes()), ok, "{s}");
        }
    }
}
