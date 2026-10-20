//! json / jsonb operators and functions.

use super::{FnCtx, Registry, Ret, SrfIter, int_arg, text, text_arg};
use crate::error::{Error, Result, code};
use crate::types::array::ArrayValue;
use crate::types::json::{Jsonb, parse_jsonb, write_json_string};
use crate::types::jsonpath;
use crate::types::numeric::Numeric;
use crate::types::text::{FormatCtx, format_value, parse_value};
use crate::types::{DataType as T, Value};
use crate::{bail, err};
use std::sync::Arc;

fn jb(v: &Value) -> Result<Arc<Jsonb>> {
    match v {
        Value::Jsonb(j) => Ok(j.clone()),
        Value::Json(s) | Value::Text(s) => Ok(Arc::new(parse_jsonb(s)?)),
        _ => Ok(Arc::new(Jsonb::Null)),
    }
}

fn jv(j: Jsonb) -> Value {
    Value::Jsonb(Arc::new(j))
}

fn json_text(s: impl Into<Arc<str>>) -> Value {
    Value::Json(s.into())
}

// ── raw json navigation (json keeps its text, so `->` returns the original slice) ──

/// Minimal scanner over valid JSON text, returning byte spans.
struct Scan<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Scan<'a> {
    fn ws(&mut self) {
        while self.pos < self.b.len() && matches!(self.b[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }
    fn string_end(&mut self) {
        self.pos += 1;
        while self.pos < self.b.len() {
            match self.b[self.pos] {
                b'\\' => self.pos += 2,
                b'"' => {
                    self.pos += 1;
                    return;
                }
                _ => self.pos += 1,
            }
        }
    }
    /// Skip one value, returning its span.
    fn value(&mut self) -> (usize, usize) {
        self.ws();
        let start = self.pos;
        match self.b.get(self.pos) {
            Some(b'"') => self.string_end(),
            Some(b'{') | Some(b'[') => {
                let mut depth = 0i32;
                while self.pos < self.b.len() {
                    match self.b[self.pos] {
                        b'"' => {
                            self.string_end();
                            continue;
                        }
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                self.pos += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    self.pos += 1;
                }
            }
            _ => {
                while self.pos < self.b.len()
                    && !matches!(
                        self.b[self.pos],
                        b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r'
                    )
                {
                    self.pos += 1;
                }
            }
        }
        (start, self.pos)
    }
}

pub(super) fn unescape_json_string(s: &str) -> String {
    match parse_jsonb(s) {
        Ok(Jsonb::String(x)) => x,
        _ => s.trim_matches('"').to_string(),
    }
}

/// Members of a raw json object: (key, value span text).
pub(super) fn raw_object(s: &str) -> Option<Vec<(String, &str)>> {
    let mut sc = Scan {
        b: s.as_bytes(),
        pos: 0,
    };
    sc.ws();
    if sc.b.get(sc.pos) != Some(&b'{') {
        return None;
    }
    sc.pos += 1;
    let mut out = Vec::new();
    loop {
        sc.ws();
        match sc.b.get(sc.pos) {
            Some(b'}') | None => break,
            Some(b',') => {
                sc.pos += 1;
                continue;
            }
            _ => {}
        }
        let (ks, ke) = sc.value();
        sc.ws();
        sc.pos += 1; // ':'
        let (vs, ve) = sc.value();
        out.push((unescape_json_string(&s[ks..ke]), &s[vs..ve]));
    }
    Some(out)
}

pub(super) fn raw_array(s: &str) -> Option<Vec<&str>> {
    let mut sc = Scan {
        b: s.as_bytes(),
        pos: 0,
    };
    sc.ws();
    if sc.b.get(sc.pos) != Some(&b'[') {
        return None;
    }
    sc.pos += 1;
    let mut out = Vec::new();
    loop {
        sc.ws();
        match sc.b.get(sc.pos) {
            Some(b']') | None => break,
            Some(b',') => {
                sc.pos += 1;
                continue;
            }
            _ => {}
        }
        let (vs, ve) = sc.value();
        out.push(&s[vs..ve]);
    }
    Some(out)
}

fn raw_get_key<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    // Postgres returns the last matching key for json.
    raw_object(s)?
        .into_iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

fn raw_get_index(s: &str, i: i64) -> Option<&str> {
    let items = raw_array(s)?;
    let idx = if i < 0 { items.len() as i64 + i } else { i };
    if idx < 0 {
        None
    } else {
        items.get(idx as usize).copied()
    }
}

/// json scalar text for ->>: strings unescaped, null → NULL.
fn raw_as_text(s: &str) -> Value {
    let t = s.trim();
    if t == "null" {
        Value::Null
    } else if t.starts_with('"') {
        text(unescape_json_string(t))
    } else {
        text(t)
    }
}

pub(super) fn raw_typeof(s: &str) -> &'static str {
    match s.trim_start().as_bytes().first() {
        Some(b'{') => "object",
        Some(b'[') => "array",
        Some(b'"') => "string",
        Some(b't') | Some(b'f') => "boolean",
        Some(b'n') => "null",
        _ => "number",
    }
}

// ── operators ──

/// Functions that walk json text de-escape its strings (the lexer's need_escapes), so a bad
/// \u escape anywhere fails them even though json input accepted it.
fn check_escapes(s: &str) -> Result<()> {
    if s.contains("\\u") {
        parse_jsonb(s)?;
    }
    Ok(())
}

fn get_field(a: &[Value], _: &FnCtx) -> Result<Value> {
    if let Value::Json(s) = &a[0] {
        check_escapes(s)?;
    }
    match &a[0] {
        Value::Json(s) => Ok(match &a[1] {
            Value::Text(k) => raw_get_key(s, k).map(json_text).unwrap_or(Value::Null),
            k => raw_get_index(s, int_arg(k))
                .map(json_text)
                .unwrap_or(Value::Null),
        }),
        v => {
            let j = jb(v)?;
            let r = match &a[1] {
                Value::Text(k) => j.get(k).cloned(),
                // A scalar is stored as a one-element array, so 0 and -1 reach it.
                k if !matches!(*j, Jsonb::Array(_) | Jsonb::Object(_)) => {
                    matches!(int_arg(k), 0 | -1).then(|| (*j).clone())
                }
                k => j.index(int_arg(k)).cloned(),
            };
            Ok(r.map(jv).unwrap_or(Value::Null))
        }
    }
}

fn get_field_text(a: &[Value], c: &FnCtx) -> Result<Value> {
    if let Value::Json(s) = &a[0] {
        check_escapes(s)?;
    }
    match &a[0] {
        Value::Json(s) => Ok(match &a[1] {
            Value::Text(k) => raw_get_key(s, k).map(raw_as_text).unwrap_or(Value::Null),
            k => raw_get_index(s, int_arg(k))
                .map(raw_as_text)
                .unwrap_or(Value::Null),
        }),
        _ => Ok(match get_field(a, c)? {
            Value::Jsonb(j) => j.as_text().map(text).unwrap_or(Value::Null),
            v => v,
        }),
    }
}

fn path_of(v: &Value) -> Vec<Option<String>> {
    match v {
        Value::Array(a) => a
            .items
            .iter()
            .map(|x| x.as_str().map(|s| s.to_string()))
            .collect(),
        _ => Vec::new(),
    }
}

fn jsonb_path_get(j: &Jsonb, path: &[Option<String>]) -> Option<Jsonb> {
    let mut cur = j;
    for p in path {
        let p = p.as_ref()?;
        cur = match cur {
            Jsonb::Object(_) => cur.get(p)?,
            Jsonb::Array(_) => cur.index(p.trim().parse::<i64>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur.clone())
}

fn raw_path_get<'a>(s: &'a str, path: &[Option<String>]) -> Option<&'a str> {
    let mut cur = s;
    for p in path {
        let p = p.as_ref()?;
        cur = match raw_typeof(cur) {
            "object" => raw_get_key(cur, p)?,
            "array" => raw_get_index(cur, p.trim().parse::<i64>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

fn get_path(a: &[Value], _: &FnCtx) -> Result<Value> {
    if let Value::Json(s) = &a[0] {
        check_escapes(s)?;
    }
    let path = path_of(&a[1]);
    match &a[0] {
        Value::Json(s) => Ok(raw_path_get(s, &path).map(json_text).unwrap_or(Value::Null)),
        v => Ok(jsonb_path_get(&*jb(v)?, &path)
            .map(jv)
            .unwrap_or(Value::Null)),
    }
}

fn get_path_text(a: &[Value], c: &FnCtx) -> Result<Value> {
    if let Value::Json(s) = &a[0] {
        check_escapes(s)?;
    }
    match &a[0] {
        Value::Json(s) => Ok(raw_path_get(s, &path_of(&a[1]))
            .map(raw_as_text)
            .unwrap_or(Value::Null)),
        _ => Ok(match get_path(a, c)? {
            Value::Jsonb(j) => j.as_text().map(text).unwrap_or(Value::Null),
            v => v,
        }),
    }
}

fn extract_path(a: &[Value], c: &FnCtx) -> Result<Value> {
    let path: Vec<Value> = a[1..].to_vec();
    let arr = Value::Array(Arc::new(ArrayValue::from_vec(T::Text, path)));
    get_path(&[a[0].clone(), arr], c)
}
fn extract_path_text(a: &[Value], c: &FnCtx) -> Result<Value> {
    let path: Vec<Value> = a[1..].to_vec();
    let arr = Value::Array(Arc::new(ArrayValue::from_vec(T::Text, path)));
    get_path_text(&[a[0].clone(), arr], c)
}

fn contains(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(jb(&a[0])?.contains(&*jb(&a[1])?)))
}
fn contained(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(jb(&a[1])?.contains(&*jb(&a[0])?)))
}

fn has_key(j: &Jsonb, k: &str) -> bool {
    match j {
        Jsonb::Object(_) => j.get(k).is_some(),
        Jsonb::Array(items) => items
            .iter()
            .any(|i| matches!(i, Jsonb::String(s) if s == k)),
        Jsonb::String(s) => s == k,
        _ => false,
    }
}
fn exists(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(Value::Bool(has_key(&*jb(&a[0])?, text_arg(&a[1]))))
}
fn exists_any(a: &[Value], _: &FnCtx) -> Result<Value> {
    let j = jb(&a[0])?;
    Ok(Value::Bool(
        path_of(&a[1]).iter().flatten().any(|k| has_key(&j, k)),
    ))
}
fn exists_all(a: &[Value], _: &FnCtx) -> Result<Value> {
    let j = jb(&a[0])?;
    Ok(Value::Bool(
        path_of(&a[1]).iter().flatten().all(|k| has_key(&j, k)),
    ))
}

fn concat(a: &[Value], _: &FnCtx) -> Result<Value> {
    let (x, y) = (jb(&a[0])?, jb(&a[1])?);
    Ok(jv(match ((*x).clone(), (*y).clone()) {
        (Jsonb::Object(mut p), Jsonb::Object(q)) => {
            p.extend(q);
            Jsonb::object(p)
        }
        (Jsonb::Array(mut p), Jsonb::Array(q)) => {
            p.extend(q);
            Jsonb::Array(p)
        }
        (Jsonb::Array(mut p), other) => {
            p.push(other);
            Jsonb::Array(p)
        }
        (other, Jsonb::Array(q)) => {
            let mut v = vec![other];
            v.extend(q);
            Jsonb::Array(v)
        }
        (p, q) => Jsonb::Array(vec![p, q]),
    }))
}

fn delete_key(a: &[Value], _: &FnCtx) -> Result<Value> {
    let j = jb(&a[0])?;
    let k = text_arg(&a[1]);
    Ok(jv(match &*j {
        Jsonb::Object(p) => Jsonb::Object(p.iter().filter(|(kk, _)| kk != k).cloned().collect()),
        Jsonb::Array(items) => Jsonb::Array(
            items
                .iter()
                .filter(|i| !matches!(i, Jsonb::String(s) if s == k))
                .cloned()
                .collect(),
        ),
        _ => bail!(INVALID_PARAMETER_VALUE, "cannot delete from scalar"),
    }))
}
fn delete_keys(a: &[Value], _: &FnCtx) -> Result<Value> {
    let j = jb(&a[0])?;
    let keys: Vec<String> = path_of(&a[1]).into_iter().flatten().collect();
    Ok(jv(match &*j {
        Jsonb::Object(p) => Jsonb::Object(
            p.iter()
                .filter(|(kk, _)| !keys.contains(kk))
                .cloned()
                .collect(),
        ),
        Jsonb::Array(items) => Jsonb::Array(
            items
                .iter()
                .filter(|i| !matches!(i, Jsonb::String(s) if keys.contains(s)))
                .cloned()
                .collect(),
        ),
        _ => bail!(INVALID_PARAMETER_VALUE, "cannot delete from scalar"),
    }))
}
fn delete_index(a: &[Value], _: &FnCtx) -> Result<Value> {
    let j = jb(&a[0])?;
    let i = int_arg(&a[1]);
    Ok(jv(match &*j {
        Jsonb::Array(items) => {
            let len = items.len() as i64;
            let idx = if i < 0 { len + i } else { i };
            Jsonb::Array(
                items
                    .iter()
                    .enumerate()
                    .filter(|(n, _)| *n as i64 != idx)
                    .map(|(_, v)| v.clone())
                    .collect(),
            )
        }
        Jsonb::Object(_) => bail!(
            INVALID_PARAMETER_VALUE,
            "cannot delete from object using integer index"
        ),
        _ => bail!(INVALID_PARAMETER_VALUE, "cannot delete from scalar"),
    }))
}

/// jsonb_delete_path (setPath with JB_PATH_DELETE) from `level`.
fn delete_path_rec(j: &Jsonb, path: &[Option<String>], level: usize) -> Result<Jsonb> {
    if level >= path.len() {
        return Ok(j.clone());
    }
    let Some(elem) = &path[level] else {
        return Err(null_path_element(level));
    };
    let last = level + 1 == path.len();
    Ok(match j {
        Jsonb::Object(p) => {
            let mut out = Vec::with_capacity(p.len());
            for (k, v) in p {
                if k == elem {
                    if last {
                        continue;
                    }
                    out.push((k.clone(), delete_path_rec(v, path, level + 1)?));
                } else {
                    out.push((k.clone(), v.clone()));
                }
            }
            Jsonb::Object(out)
        }
        Jsonb::Array(items) => {
            let i = path_index(elem, level)?;
            let len = items.len() as i64;
            let idx = if i < 0 { len + i } else { i };
            let mut out = Vec::with_capacity(items.len());
            for (n, v) in items.iter().enumerate() {
                if n as i64 == idx {
                    if last {
                        continue;
                    }
                    out.push(delete_path_rec(v, path, level + 1)?);
                } else {
                    out.push(v.clone());
                }
            }
            Jsonb::Array(out)
        }
        j => j.clone(),
    })
}

fn delete_path(a: &[Value], _: &FnCtx) -> Result<Value> {
    let j = jb(&a[0])?;
    if !matches!(&*j, Jsonb::Object(_) | Jsonb::Array(_)) {
        bail!(INVALID_PARAMETER_VALUE, "cannot delete path in scalar");
    }
    // An empty container comes back as it is.
    if matches!(&*j, Jsonb::Object(m) if m.is_empty()) || matches!(&*j, Jsonb::Array(xs) if xs.is_empty()) {
        return Ok(a[0].clone());
    }
    let path = path_of(&a[1]);
    Ok(jv(delete_path_rec(&j, &path, 0)?))
}

// ── conversions ──

/// Convert a SQL value to jsonb (to_jsonb semantics).
pub fn value_to_jsonb(v: &Value, ty: &T, fmt: &FormatCtx) -> Result<Jsonb> {
    Ok(match v {
        Value::Null => Jsonb::Null,
        Value::Bool(b) => Jsonb::Bool(*b),
        Value::Int2(i) => Jsonb::Number(Numeric::from_i64(*i as i64)),
        Value::Int4(i) => Jsonb::Number(Numeric::from_i64(*i as i64)),
        Value::Int8(i) => Jsonb::Number(Numeric::from_i64(*i)),
        Value::Float4(f) if f.is_finite() => Jsonb::Number(
            crate::types::text::format_float4(*f, 1)
                .parse()
                .unwrap_or(Numeric::zero()),
        ),
        Value::Float8(f) if f.is_finite() => Jsonb::Number(
            crate::types::text::format_float8(*f, 1)
                .parse()
                .unwrap_or(Numeric::zero()),
        ),
        Value::Numeric(n) if n.is_finite() => Jsonb::Number((**n).clone()),
        Value::Jsonb(j) => (**j).clone(),
        Value::Json(s) => parse_jsonb(s)?,
        Value::Array(a) => array_to_jsonb(a, fmt)?,
        Value::Record(fields) => {
            let types = crate::types::record_field_types(ty, fields.len());
            Jsonb::object(
                fields
                    .iter()
                    .zip(types)
                    .map(|(f, (name, t))| Ok((name, value_to_jsonb(f, &t, fmt)?)))
                    .collect::<Result<Vec<_>>>()?,
            )
        }
        Value::Timestamp(_)
        | Value::Timestamptz(_)
        | Value::Date(_)
        | Value::Time(_)
        | Value::Timetz(..) => Jsonb::String(json_datetime(v, ty, fmt)),
        v => Jsonb::String(format_value(v, ty, fmt)),
    })
}

/// Dates in json use ISO 8601 ("T" separator, XSD zone offsets) whatever the DateStyle:
/// JsonEncodeDateTime.
fn json_datetime(v: &Value, ty: &T, fmt: &FormatCtx) -> String {
    use crate::types::datetime as dt;
    match v {
        Value::Date(d) => dt::json_encode_date(*d),
        Value::Time(t) => dt::format_time_of_day(*t),
        Value::Timetz(t, z) => dt::json_encode_timetz(*t, *z),
        Value::Timestamp(t) => dt::json_encode_timestamp(*t, None),
        Value::Timestamptz(t) => dt::json_encode_timestamp(*t, Some(&fmt.tz)),
        v => format_value(v, ty, fmt),
    }
}

fn array_to_jsonb(a: &ArrayValue, fmt: &FormatCtx) -> Result<Jsonb> {
    if a.dims.is_empty() {
        return Ok(Jsonb::Array(Vec::new()));
    }
    fn rec(a: &ArrayValue, d: usize, off: usize, fmt: &FormatCtx) -> Result<Jsonb> {
        let stride: usize = a.dims[d + 1..].iter().map(|x| x.0).product();
        let mut out = Vec::new();
        for i in 0..a.dims[d].0 {
            if d + 1 == a.dims.len() {
                out.push(value_to_jsonb(&a.items[off + i], &a.elem, fmt)?);
            } else {
                out.push(rec(a, d + 1, off + i * stride, fmt)?);
            }
        }
        Ok(Jsonb::Array(out))
    }
    rec(a, 0, 0, fmt)
}

/// Convert a SQL value to json text (to_json semantics: json keeps its text).
pub fn value_to_json_text(v: &Value, ty: &T, fmt: &FormatCtx, out: &mut String) -> Result<()> {
    match v {
        Value::Json(s) => out.push_str(s),
        Value::Record(fields) => {
            out.push('{');
            let types = crate::types::record_field_types(ty, fields.len());
            for (i, (f, (name, t))) in fields.iter().zip(types).enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(out, &name);
                out.push(':');
                value_to_json_text(f, &t, fmt, out)?;
            }
            out.push('}');
        }
        Value::Array(a) => {
            if a.dims.is_empty() {
                out.push_str("[]");
                return Ok(());
            }
            fn rec(
                a: &ArrayValue,
                d: usize,
                off: usize,
                fmt: &FormatCtx,
                out: &mut String,
            ) -> Result<()> {
                let stride: usize = a.dims[d + 1..].iter().map(|x| x.0).product();
                out.push('[');
                for i in 0..a.dims[d].0 {
                    if i > 0 {
                        out.push(',');
                    }
                    if d + 1 == a.dims.len() {
                        value_to_json_text(&a.items[off + i], &a.elem, fmt, out)?;
                    } else {
                        rec(a, d + 1, off + i * stride, fmt, out)?;
                    }
                }
                out.push(']');
                Ok(())
            }
            rec(a, 0, 0, fmt, out)?;
        }
        Value::Text(s) if !matches!(ty, T::Json | T::Jsonb) => write_json_string(out, s),
        Value::Float4(f) if !f.is_finite() => {
            write_json_string(out, &crate::types::text::format_float4(*f, 1))
        }
        Value::Float8(f) if !f.is_finite() => {
            write_json_string(out, &crate::types::text::format_float8(*f, 1))
        }
        Value::Numeric(n) if !n.is_finite() => write_json_string(out, &n.to_string()),
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Int2(_)
        | Value::Int4(_)
        | Value::Int8(_)
        | Value::Float4(_)
        | Value::Float8(_)
        | Value::Numeric(_) => out.push_str(&format_value(v, ty, fmt)),
        Value::Jsonb(j) => j.write(out),
        Value::Timestamp(_)
        | Value::Timestamptz(_)
        | Value::Date(_)
        | Value::Time(_)
        | Value::Timetz(..) => write_json_string(out, &json_datetime(v, ty, fmt)),
        v => write_json_string(out, &format_value(v, ty, fmt)),
    }
    Ok(())
}

fn to_json(a: &[Value], c: &FnCtx) -> Result<Value> {
    let mut s = String::new();
    value_to_json_text(&a[0], &c.arg_type(0), c.exec.fmt(), &mut s)?;
    Ok(json_text(s))
}
fn to_jsonb(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(jv(value_to_jsonb(&a[0], &c.arg_type(0), c.exec.fmt())?))
}
fn array_to_json(a: &[Value], c: &FnCtx) -> Result<Value> {
    let pretty = a.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
    let mut s = String::new();
    value_to_json_text(&a[0], &c.arg_type(0), c.exec.fmt(), &mut s)?;
    if pretty && let Value::Array(arr) = &a[0] {
        let items: Vec<String> = arr
            .items
            .iter()
            .map(|v| {
                let mut o = String::new();
                value_to_json_text(v, &arr.elem, c.exec.fmt(), &mut o).map(|_| o)
            })
            .collect::<Result<_>>()?;
        s = format!("[{}]", items.join(",\n "));
    }
    Ok(json_text(s))
}
fn row_to_json(a: &[Value], c: &FnCtx) -> Result<Value> {
    let pretty = a.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
    let Value::Record(fields) = &a[0] else {
        bail!(DATATYPE_MISMATCH, "argument must be a composite type")
    };
    let types = crate::types::record_field_types(&c.arg_type(0), fields.len());
    let mut s = String::from("{");
    for (i, (f, (name, t))) in fields.iter().zip(types).enumerate() {
        if i > 0 {
            s.push_str(if pretty { ",\n " } else { "," });
        }
        write_json_string(&mut s, &name);
        s.push(':');
        value_to_json_text(f, &t, c.exec.fmt(), &mut s)?;
    }
    s.push('}');
    Ok(json_text(s))
}

fn build_array(a: &[Value], c: &FnCtx) -> Result<Value> {
    let mut s = String::from("[");
    for (i, v) in a.iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        value_to_json_text(v, &c.arg_type(i), c.exec.fmt(), &mut s)?;
    }
    s.push(']');
    Ok(json_text(s))
}
fn jsonb_build_array(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(jv(Jsonb::Array(
        a.iter()
            .enumerate()
            .map(|(i, v)| value_to_jsonb(v, &c.arg_type(i), c.exec.fmt()))
            .collect::<Result<_>>()?,
    )))
}

/// Whether user type `ty` has a cast to json (json_categorize_type's JSONTYPE_CAST).
pub(super) fn has_json_cast(ty: &T, c: &FnCtx) -> bool {
    let cat = c.exec.catalog();
    let oid = crate::catalog::system::type_oid(&cat, ty);
    oid >= crate::catalog::FIRST_USER_OID
        && cat
            .casts_all()
            .iter()
            .any(|x| x.source == oid && x.target == 114 && x.method == 'f')
}

fn key_text(v: &Value, ty: &T, c: &FnCtx) -> Result<String> {
    match v {
        Value::Null => Err(err!(
            NULL_VALUE_NOT_ALLOWED,
            "null value not allowed for object key"
        )),
        Value::Array(_) | Value::Record(_) | Value::Json(_) | Value::Jsonb(_) => Err(err!(
            INVALID_PARAMETER_VALUE,
            "key value must be scalar, not array, composite, or json"
        )),
        _ if !matches!(ty, T::Text) && has_json_cast(ty, c) => Err(err!(
            INVALID_PARAMETER_VALUE,
            "key value must be scalar, not array, composite, or json"
        )),
        Value::Text(s) => Ok(s.to_string()),
        v => Ok(format_value(v, ty, c.exec.fmt())),
    }
}

fn build_object(a: &[Value], c: &FnCtx) -> Result<Value> {
    if !a.len().is_multiple_of(2) {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "argument list must have even number of elements",
        )
        .hint(
            "The arguments of json_build_object() must consist of alternating keys and values.",
        ));
    }
    let mut s = String::from("{");
    for i in (0..a.len()).step_by(2) {
        if i > 0 {
            s.push_str(", ");
        }
        let k = key_text(&a[i], &c.arg_type(i), c).map_err(|e| {
            if a[i].is_null() {
                err!(NULL_VALUE_NOT_ALLOWED, "null value not allowed for object key")
            } else {
                e
            }
        })?;
        write_json_string(&mut s, &k);
        s.push_str(" : ");
        value_to_json_text(&a[i + 1], &c.arg_type(i + 1), c.exec.fmt(), &mut s)?;
    }
    s.push('}');
    Ok(json_text(s))
}
fn jsonb_build_object(a: &[Value], c: &FnCtx) -> Result<Value> {
    if !a.len().is_multiple_of(2) {
        return Err(Error::new(
            code::INVALID_PARAMETER_VALUE,
            "argument list must have even number of elements",
        )
        .hint(
            "The arguments of jsonb_build_object() must consist of alternating keys and values.",
        ));
    }
    let mut pairs = Vec::new();
    for i in (0..a.len()).step_by(2) {
        let k = key_text(&a[i], &c.arg_type(i), c).map_err(|e| {
            if a[i].is_null() {
                err!(
                    NULL_VALUE_NOT_ALLOWED,
                    "argument {}: key must not be null",
                    i + 1
                )
            } else {
                e
            }
        })?;
        pairs.push((
            k,
            value_to_jsonb(&a[i + 1], &c.arg_type(i + 1), c.exec.fmt())?,
        ));
    }
    Ok(jv(Jsonb::object(pairs)))
}

fn object_pairs(a: &[Value]) -> Result<Vec<(String, Option<String>)>> {
    if a.len() == 2 {
        let ndims = |v: &Value| v.as_array().map_or(0, |x| x.dims.len());
        let (nk, nv) = (ndims(&a[0]), ndims(&a[1]));
        if nk > 1 || nk != nv {
            bail!(ARRAY_SUBSCRIPT_ERROR, "wrong number of array subscripts");
        }
        let keys = a[0].as_array().map(|x| x.items.clone()).unwrap_or_default();
        let vals = a[1].as_array().map(|x| x.items.clone()).unwrap_or_default();
        if keys.len() != vals.len() {
            bail!(ARRAY_SUBSCRIPT_ERROR, "mismatched array dimensions");
        }
        return keys
            .iter()
            .zip(vals)
            .map(|(k, v)| match k {
                Value::Null => Err(err!(
                    NULL_VALUE_NOT_ALLOWED,
                    "null value not allowed for object key"
                )),
                k => Ok((
                    k.as_str().unwrap_or("").to_string(),
                    v.as_str().map(|s| s.to_string()),
                )),
            })
            .collect();
    }
    let Some(arr) = a[0].as_array() else {
        return Ok(Vec::new());
    };
    let pairs: Vec<Value> = arr.items.clone();
    match arr.dims.len() {
        0 => Ok(Vec::new()),
        1 => {
            if !pairs.len().is_multiple_of(2) {
                bail!(
                    ARRAY_SUBSCRIPT_ERROR,
                    "array must have even number of elements"
                );
            }
            pairs
                .chunks(2)
                .map(|p| match &p[0] {
                    Value::Null => Err(err!(
                        NULL_VALUE_NOT_ALLOWED,
                        "null value not allowed for object key"
                    )),
                    k => Ok((
                        k.as_str().unwrap_or("").to_string(),
                        p[1].as_str().map(|s| s.to_string()),
                    )),
                })
                .collect()
        }
        2 => {
            if arr.dims[1].0 != 2 {
                bail!(ARRAY_SUBSCRIPT_ERROR, "array must have two columns");
            }
            pairs
                .chunks(2)
                .map(|p| match &p[0] {
                    Value::Null => Err(err!(
                        NULL_VALUE_NOT_ALLOWED,
                        "null value not allowed for object key"
                    )),
                    k => Ok((
                        k.as_str().unwrap_or("").to_string(),
                        p[1].as_str().map(|s| s.to_string()),
                    )),
                })
                .collect()
        }
        _ => bail!(ARRAY_SUBSCRIPT_ERROR, "wrong number of array subscripts"),
    }
}
fn json_object(a: &[Value], _: &FnCtx) -> Result<Value> {
    let mut s = String::from("{");
    for (i, (k, v)) in object_pairs(a)?.into_iter().enumerate() {
        if i > 0 {
            s.push_str(", ");
        }
        write_json_string(&mut s, &k);
        s.push_str(" : ");
        match v {
            Some(v) => write_json_string(&mut s, &v),
            None => s.push_str("null"),
        }
    }
    s.push('}');
    Ok(json_text(s))
}
fn jsonb_object(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(jv(Jsonb::object(
        object_pairs(a)?
            .into_iter()
            .map(|(k, v)| (k, v.map(Jsonb::String).unwrap_or(Jsonb::Null)))
            .collect(),
    )))
}

fn array_length(a: &[Value], _: &FnCtx) -> Result<Value> {
    match &a[0] {
        Value::Json(s) => match raw_array(s) {
            Some(items) => Ok(Value::Int4(items.len() as i32)),
            None => bail!(
                INVALID_PARAMETER_VALUE,
                "cannot get array length of a {}",
                if raw_typeof(s) == "object" {
                    "non-array"
                } else {
                    "scalar"
                }
            ),
        },
        v => match &*jb(v)? {
            Jsonb::Array(items) => Ok(Value::Int4(items.len() as i32)),
            Jsonb::Object(_) => bail!(
                INVALID_PARAMETER_VALUE,
                "cannot get array length of a non-array"
            ),
            _ => bail!(
                INVALID_PARAMETER_VALUE,
                "cannot get array length of a scalar"
            ),
        },
    }
}
fn typeof_(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(text(match &a[0] {
        Value::Json(s) => raw_typeof(s),
        v => jb(v)?.type_name(),
    }))
}
fn pretty(a: &[Value], _: &FnCtx) -> Result<Value> {
    Ok(text(jb(&a[0])?.pretty()))
}

fn strip_nulls_rec(j: &Jsonb, arrays: bool) -> Jsonb {
    match j {
        Jsonb::Object(p) => Jsonb::Object(
            p.iter()
                .filter(|(_, v)| !matches!(v, Jsonb::Null))
                .map(|(k, v)| (k.clone(), strip_nulls_rec(v, arrays)))
                .collect(),
        ),
        Jsonb::Array(items) => Jsonb::Array(
            items
                .iter()
                .filter(|v| !arrays || !matches!(v, Jsonb::Null))
                .map(|v| strip_nulls_rec(v, arrays))
                .collect(),
        ),
        o => o.clone(),
    }
}
fn strip_nulls(a: &[Value], _: &FnCtx) -> Result<Value> {
    let arrays = a.get(1).and_then(|v| v.as_bool()).unwrap_or(false);
    let r = strip_nulls_rec(&*jb(&a[0])?, arrays);
    Ok(match &a[0] {
        Value::Json(_) => {
            let mut s = String::new();
            write_compact(&r, &mut s);
            json_text(s)
        }
        _ => jv(r),
    })
}
fn write_compact(j: &Jsonb, out: &mut String) {
    match j {
        Jsonb::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_compact(v, out);
            }
            out.push(']');
        }
        Jsonb::Object(p) => {
            out.push('{');
            for (i, (k, v)) in p.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(out, k);
                out.push(':');
                write_compact(v, out);
            }
            out.push('}');
        }
        o => o.write(out),
    }
}

/// A path element as an array subscript (setPathArray's strtoint).
fn path_index(p: &str, level: usize) -> Result<i64> {
    p.parse::<i32>().map(i64::from).map_err(|_| {
        err!(
            INVALID_TEXT_REPRESENTATION,
            "path element at position {} is not an integer: \"{}\"",
            level + 1,
            p
        )
    })
}

fn null_path_element(level: usize) -> Error {
    err!(
        INVALID_PARAMETER_VALUE,
        "path element at position {} is null",
        level + 1
    )
}

/// jsonb_set / jsonb_insert core (setPath): walk `path` from `level`, replacing or
/// inserting at its end.
fn set_path(
    j: &Jsonb,
    path: &[Option<String>],
    level: usize,
    new: &Jsonb,
    create: bool,
    insert_after: Option<bool>,
) -> Result<Jsonb> {
    if level >= path.len() {
        return Ok(j.clone());
    }
    let Some(elem) = &path[level] else {
        return Err(null_path_element(level));
    };
    let last = level + 1 == path.len();
    match j {
        Jsonb::Object(p) => {
            let key = elem;
            let mut out = p.clone();
            match out.iter_mut().find(|(k, _)| k == key) {
                Some((_, v)) => {
                    if last {
                        if insert_after.is_some() {
                            return Err(err!(
                                INVALID_PARAMETER_VALUE,
                                "cannot replace existing key"
                            )
                            .hint("Try using the function jsonb_set to replace key value."));
                        }
                        *v = new.clone();
                    } else {
                        *v = set_path(v, path, level + 1, new, create, insert_after)?;
                    }
                }
                None => {
                    if last && (create || insert_after.is_some()) {
                        out.push((key.clone(), new.clone()));
                    }
                }
            }
            Ok(Jsonb::object(out))
        }
        Jsonb::Array(items) => {
            let i = path_index(elem, level)?;
            let len = items.len() as i64;
            let mut out = items.clone();
            if last {
                match insert_after {
                    Some(after) => {
                        let mut idx = if i < 0 { len + i } else { i };
                        if after {
                            idx += 1;
                        }
                        let idx = idx.clamp(0, len) as usize;
                        out.insert(idx, new.clone());
                    }
                    None => {
                        let idx = if i < 0 { len + i } else { i };
                        if idx >= 0 && idx < len {
                            out[idx as usize] = new.clone();
                        } else if create {
                            if idx < 0 {
                                out.insert(0, new.clone());
                            } else {
                                out.push(new.clone());
                            }
                        }
                    }
                }
            } else {
                let idx = if i < 0 { len + i } else { i };
                if idx >= 0 && idx < len {
                    out[idx as usize] =
                        set_path(&out[idx as usize], path, level + 1, new, create, insert_after)?;
                }
            }
            Ok(Jsonb::Array(out))
        }
        _ => Ok(j.clone()),
    }
}
fn jsonb_set(a: &[Value], _: &FnCtx) -> Result<Value> {
    let j = jb(&a[0])?;
    if !matches!(&*j, Jsonb::Object(_) | Jsonb::Array(_)) {
        bail!(INVALID_PARAMETER_VALUE, "cannot set path in scalar");
    }
    let path = path_of(&a[1]);
    let create = a.get(3).and_then(|v| v.as_bool()).unwrap_or(true);
    Ok(jv(set_path(&j, &path, 0, &*jb(&a[2])?, create, None)?))
}
fn jsonb_set_lax(a: &[Value], c: &FnCtx) -> Result<Value> {
    if a[0].is_null() || a[1].is_null() {
        return Ok(Value::Null);
    }
    let treatment = match a.get(4) {
        Some(Value::Null) => None,
        Some(v) => v.as_str().map(|s| s.to_string()),
        None => Some("use_json_null".into()),
    };
    let Some(treatment) = treatment else {
        bail!(
            INVALID_PARAMETER_VALUE,
            "null_value_treatment must be \"delete_key\", \"return_target\", \"use_json_null\", or \"raise_exception\""
        );
    };
    if !a[2].is_null() {
        return jsonb_set(&a[..a.len().min(4)], c);
    }
    match treatment.as_str() {
        "raise_exception" => Err(err!(NULL_VALUE_NOT_ALLOWED, "JSON value must not be null")
            .detail("Exception was raised because null_value_treatment is \"raise_exception\".")
            .hint("To avoid, either change the null_value_treatment argument or ensure that an SQL NULL is not passed.")),
        "return_target" => Ok(a[0].clone()),
        "delete_key" => delete_path(&a[..2], c),
        "use_json_null" => {
            let mut b = a.to_vec();
            b[2] = jv(Jsonb::Null);
            jsonb_set(&b[..b.len().min(4)], c)
        }
        _ => bail!(
            INVALID_PARAMETER_VALUE,
            "null_value_treatment must be \"delete_key\", \"return_target\", \"use_json_null\", or \"raise_exception\""
        ),
    }
}
fn jsonb_insert(a: &[Value], _: &FnCtx) -> Result<Value> {
    let j = jb(&a[0])?;
    if !matches!(&*j, Jsonb::Object(_) | Jsonb::Array(_)) {
        bail!(INVALID_PARAMETER_VALUE, "cannot set path in scalar");
    }
    let path = path_of(&a[1]);
    let after = a.get(3).and_then(|v| v.as_bool()).unwrap_or(false);
    Ok(jv(set_path(&j, &path, 0, &*jb(&a[2])?, false, Some(after))?))
}

fn json_scalar(a: &[Value], c: &FnCtx) -> Result<Value> {
    to_json(a, c)
}

// ── set-returning ──

fn each_rows(a: &[Value], as_text: bool, jsonb_out: bool) -> Result<SrfIter> {
    if let Value::Json(s) = &a[0] {
        check_escapes(s)?;
    }
    let rows: Vec<Vec<Value>> = match &a[0] {
        Value::Json(s) => match raw_object(s) {
            Some(members) => members
                .into_iter()
                .map(|(k, v)| {
                    vec![
                        text(k),
                        if as_text {
                            raw_as_text(v)
                        } else {
                            json_text(v)
                        },
                    ]
                })
                .collect(),
            None => bail!(
                INVALID_PARAMETER_VALUE,
                "cannot call json_each on a non-object"
            ),
        },
        v => match &*jb(v)? {
            Jsonb::Object(p) => p
                .iter()
                .map(|(k, v)| {
                    vec![
                        text(k.as_str()),
                        if as_text {
                            v.as_text().map(text).unwrap_or(Value::Null)
                        } else if jsonb_out {
                            jv(v.clone())
                        } else {
                            json_text(v.to_string())
                        },
                    ]
                })
                .collect(),
            _ => bail!(
                INVALID_PARAMETER_VALUE,
                "cannot call jsonb_each on a non-object"
            ),
        },
    };
    Ok(Box::new(rows.into_iter().map(Ok)))
}
// ── json → records ──

/// populate_array_report_expected_array: a JSON value where an array belongs. `path`: the
/// element's indices (empty: the key's value itself).
fn expected_array(key: Option<&str>, path: &[usize]) -> Error {
    let e = err!(INVALID_TEXT_REPRESENTATION, "expected JSON array");
    let at: String = path.iter().map(|i| format!("[{i}]")).collect();
    match (key, path.is_empty()) {
        (Some(k), true) => e.hint(format!("See the value of key \"{k}\".")),
        (None, true) => e,
        (Some(k), false) => e.hint(format!("See the array element {at} of key \"{k}\".")),
        (None, false) => e.hint(format!("See the array element {at}.")),
    }
}

fn malformed_json_array() -> Error {
    err!(INVALID_TEXT_REPRESENTATION, "malformed JSON array")
        .detail("Multidimensional arrays must have sub-arrays with matching dimensions.")
}

/// A JSON array or object as an array or row of type `ty` (json_populate_type's
/// populate_array / populate_composite).
pub fn populate_value(j: &Jsonb, ty: &T, fmt: &FormatCtx) -> Result<Value> {
    json_to_column_key(j, ty, fmt, None)
}

/// A JSON value as a column of type `ty`, as json_populate_record converts it; `key`: the
/// field it is the value of (named in errors).
fn json_to_column_key(j: &Jsonb, ty: &T, fmt: &FormatCtx, key: Option<&str>) -> Result<Value> {
    Ok(match (j, ty) {
        (Jsonb::Null, _) => Value::Null,
        (_, T::Jsonb) => jv(j.clone()),
        (_, T::Json) => json_text(j.to_string()),
        (Jsonb::Array(_), T::Array(elem)) => {
            // Nested JSON arrays become a multi-dimensional array; they must be rectangular.
            let mut dims = Vec::new();
            let mut cur = j;
            while let Jsonb::Array(items) = cur {
                dims.push((items.len(), 1));
                match items.first() {
                    Some(first) if !matches!(**elem, T::Jsonb | T::Json) => cur = first,
                    _ => break,
                }
            }
            let mut items = Vec::new();
            #[allow(clippy::too_many_arguments)]
            fn flatten(
                j: &Jsonb,
                depth: usize,
                dims: &[(usize, i32)],
                elem: &T,
                fmt: &FormatCtx,
                key: Option<&str>,
                path: &mut Vec<usize>,
                out: &mut Vec<Value>,
            ) -> Result<()> {
                match j {
                    Jsonb::Array(xs) if depth < dims.len() => {
                        if xs.len() != dims[depth].0 {
                            return Err(malformed_json_array());
                        }
                        for (i, x) in xs.iter().enumerate() {
                            path.push(i);
                            flatten(x, depth + 1, dims, elem, fmt, key, path, out)?;
                            path.pop();
                        }
                    }
                    _ if depth < dims.len() => return Err(expected_array(key, path)),
                    x => out.push(json_to_column_key(x, elem, fmt, key)?),
                }
                Ok(())
            }
            flatten(j, 0, &dims, elem, fmt, key, &mut Vec::new(), &mut items)?;
            if items.is_empty() {
                Value::Array(Arc::new(ArrayValue::empty((**elem).clone())))
            } else {
                Value::Array(Arc::new(ArrayValue {
                    elem: (**elem).clone(),
                    dims,
                    items,
                }))
            }
        }
        // A string holds an array literal; anything else isn't an array.
        (Jsonb::Number(_) | Jsonb::Bool(_) | Jsonb::Object(_), T::Array(_)) => {
            return Err(expected_array(key, &[]));
        }
        (Jsonb::Object(_), T::Row(fields)) => json_to_record_value(j, fields, None, fmt)?,
        (Jsonb::String(s), ty) => parse_value(s, ty, fmt)?,
        (other, ty) => parse_value(&other.to_string(), ty, fmt)?,
    })
}

/// An object as a row of `fields`; keys that are absent keep `base`'s value.
fn json_to_record_value(
    j: &Jsonb,
    fields: &[(String, T)],
    base: Option<&[Value]>,
    fmt: &FormatCtx,
) -> Result<Value> {
    Ok(Value::Record(Arc::new(json_to_row(j, fields, base, fmt)?)))
}

fn json_to_row(
    j: &Jsonb,
    fields: &[(String, T)],
    base: Option<&[Value]>,
    fmt: &FormatCtx,
) -> Result<Vec<Value>> {
    fields
        .iter()
        .enumerate()
        .map(|(i, (name, ty))| match j.get(name) {
            Some(v) => json_to_column_key(v, ty, fmt, Some(name)),
            None => Ok(base.and_then(|b| b.get(i).cloned()).unwrap_or(Value::Null)),
        })
        .collect()
}

// ── json_populate_record & co. (jsonfuncs.c's populate_* functions) ──

/// A json or jsonb value being populated from: json keeps its text (a string's token is its
/// unescaped contents, anything else its source text), jsonb its tree.
#[derive(Clone, Copy)]
enum Jv<'a> {
    Json(&'a str),
    Jsonb(&'a Jsonb),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum JvKind {
    Null,
    String,
    Scalar,
    Array,
    Object,
}

impl<'a> Jv<'a> {
    fn kind(&self) -> JvKind {
        match self {
            Jv::Json(s) => match s.trim_start().as_bytes().first() {
                Some(b'"') => JvKind::String,
                Some(b'[') => JvKind::Array,
                Some(b'{') => JvKind::Object,
                Some(b'n') | None => JvKind::Null,
                _ => JvKind::Scalar,
            },
            Jv::Jsonb(j) => match j {
                Jsonb::Null => JvKind::Null,
                Jsonb::String(_) => JvKind::String,
                Jsonb::Array(_) => JvKind::Array,
                Jsonb::Object(_) => JvKind::Object,
                _ => JvKind::Scalar,
            },
        }
    }

    fn items(&self) -> Vec<Jv<'a>> {
        match self {
            Jv::Json(s) => raw_array(s).unwrap_or_default().into_iter().map(Jv::Json).collect(),
            Jv::Jsonb(Jsonb::Array(xs)) => xs.iter().map(Jv::Jsonb).collect(),
            _ => Vec::new(),
        }
    }
}

/// A JSON object's fields by name (json: the last of duplicate keys).
enum JsObject<'a> {
    Json(Vec<(String, &'a str)>),
    Jsonb(&'a Jsonb),
}

impl<'a> JsObject<'a> {
    /// JsValueToJsObject.
    fn of(v: Jv<'a>) -> Result<Self> {
        match (v, v.kind()) {
            (Jv::Json(s), JvKind::Object) => Ok(JsObject::Json(raw_object(s).unwrap_or_default())),
            (Jv::Jsonb(j), JvKind::Object) => Ok(JsObject::Jsonb(j)),
            (_, JvKind::Array) => bail!(
                INVALID_PARAMETER_VALUE,
                "cannot call populate_composite on an array"
            ),
            _ => bail!(
                INVALID_PARAMETER_VALUE,
                "cannot call populate_composite on a scalar"
            ),
        }
    }

    fn get(&self, key: &str) -> Option<Jv<'a>> {
        match self {
            JsObject::Json(m) => m.iter().rev().find(|(k, _)| k == key).map(|(_, v)| Jv::Json(v)),
            JsObject::Jsonb(j) => j.get(key).map(Jv::Jsonb),
        }
    }

    fn is_empty(&self) -> bool {
        match self {
            JsObject::Json(m) => m.is_empty(),
            JsObject::Jsonb(Jsonb::Object(m)) => m.is_empty(),
            JsObject::Jsonb(_) => true,
        }
    }
}

/// The domain each field of composite `row` is declared as.
fn field_domains(row: &crate::types::RowType, c: &FnCtx) -> Vec<Option<u32>> {
    if row.oid == 0 {
        return vec![None; row.fields.len()];
    }
    let cat = c.exec.catalog();
    if let Some(t) = cat.tables.values().find(|t| t.rowtype == row.oid) {
        return t.live_columns().map(|(_, col)| col.domain).collect();
    }
    match cat.composite_domains.get(&row.oid) {
        Some(d) => row.fields.iter().map(|(n, _)| d.get(n).copied()).collect(),
        None => vec![None; row.fields.len()],
    }
}

fn domain_check(oid: u32, v: &Value, c: &FnCtx) -> Result<()> {
    if let Some(dc) = crate::sql::binder::Binder::new(c.exec, Vec::new()).domain_coerce(oid)? {
        dc.check(v, c.exec)?;
    }
    Ok(())
}

/// populate_scalar: the value's text through `ty`'s input function.
fn populate_scalar(v: Jv, ty: &T, c: &FnCtx) -> Result<Value> {
    let s: String = match v {
        Jv::Json(raw) => {
            let raw = raw.trim();
            if raw.starts_with('"') {
                let unescaped = unescape_json_string(raw);
                if matches!(ty, T::Json | T::Jsonb) {
                    let mut out = String::new();
                    write_json_string(&mut out, &unescaped);
                    out
                } else {
                    unescaped
                }
            } else {
                raw.to_string()
            }
        }
        Jv::Jsonb(j) => match (j, ty) {
            (_, T::Jsonb) => return Ok(jv(j.clone())),
            (Jsonb::String(x), T::Json) => {
                let mut out = String::new();
                write_json_string(&mut out, x);
                out
            }
            (Jsonb::String(x), _) => x.clone(),
            (j, _) => j.to_string(),
        },
    };
    super::sqljson::input(&s, ty, c)
}

/// populate_array (populate_array_json's events, which jsonb's walk matches).
struct ArrayCtx<'k> {
    colname: Option<&'k str>,
    ndims: usize,
    dims: Vec<i64>,
    sizes: Vec<usize>,
    items: Vec<Value>,
}

impl ArrayCtx<'_> {
    fn expected_array(&self, ndim: usize) -> Error {
        let e = err!(INVALID_TEXT_REPRESENTATION, "expected JSON array");
        if ndim == 0 {
            return match self.colname {
                Some(k) => e.hint(format!("See the value of key \"{k}\".")),
                None => e,
            };
        }
        let at: String = self.sizes[..ndim].iter().map(|i| format!("[{i}]")).collect();
        match self.colname {
            Some(k) => e.hint(format!("See the array element {at} of key \"{k}\".")),
            None => e.hint(format!("See the array element {at}.")),
        }
    }

    fn assign_ndims(&mut self, ndims: usize) -> Result<()> {
        if ndims == 0 {
            return Err(self.expected_array(0));
        }
        self.ndims = ndims;
        self.dims = vec![-1; ndims];
        self.sizes = vec![0; ndims];
        Ok(())
    }

    fn check_dimension(&mut self, ndim: usize) -> Result<()> {
        let dim = self.sizes[ndim] as i64;
        if self.dims[ndim] == -1 {
            self.dims[ndim] = dim;
        } else if self.dims[ndim] != dim {
            return Err(malformed_json_array());
        }
        self.sizes[ndim] = 0;
        if ndim > 0 {
            self.sizes[ndim - 1] += 1;
        }
        Ok(())
    }
}

fn populate_array(v: Jv, elem: &T, colname: Option<&str>, c: &FnCtx) -> Result<Value> {
    let mut ctx = ArrayCtx {
        colname,
        ndims: 0,
        dims: Vec::new(),
        sizes: Vec::new(),
        items: Vec::new(),
    };
    fn element(ctx: &mut ArrayCtx, e: Jv, ndim: usize, elem: &T, c: &FnCtx) -> Result<()> {
        let v = populate_field(Some(e), elem, None, None, None, c)?;
        ctx.items.push(v);
        ctx.sizes[ndim - 1] += 1;
        Ok(())
    }
    // An array at lexical level `level` (0: the value itself).
    fn walk(ctx: &mut ArrayCtx, v: Jv, level: usize, elem: &T, c: &FnCtx) -> Result<()> {
        for e in v.items() {
            let ndim = level + 1;
            let is_array = e.kind() == JvKind::Array;
            if ctx.ndims == 0 && !is_array {
                ctx.assign_ndims(ndim)?;
                element(ctx, e, ndim, elem, c)?;
            } else if ctx.ndims > 0 && ndim == ctx.ndims {
                element(ctx, e, ndim, elem, c)?;
            } else if ctx.ndims > 0 && !is_array {
                return Err(ctx.expected_array(ndim));
            } else {
                walk(ctx, e, ndim, elem, c)?;
            }
        }
        if ctx.ndims == 0 {
            ctx.assign_ndims(level + 1)?;
        }
        if level < ctx.ndims {
            ctx.check_dimension(level)?;
        }
        Ok(())
    }
    if v.kind() != JvKind::Array {
        return Err(ctx.expected_array(0));
    }
    walk(&mut ctx, v, 0, elem, c)?;
    let dims: Vec<(usize, i32)> = ctx.dims.iter().map(|d| (*d.max(&0) as usize, 1)).collect();
    if ctx.items.is_empty() {
        return Ok(Value::Array(Arc::new(ArrayValue::empty(elem.clone()))));
    }
    Ok(Value::Array(Arc::new(ArrayValue {
        elem: elem.clone(),
        dims,
        items: ctx.items,
    })))
}

/// populate_record: `obj`'s fields as a row of `row`; `default` (a record argument or an
/// enclosing record's field) keeps the fields the object lacks.
fn populate_record(
    row: &crate::types::RowType,
    default: Option<&[Value]>,
    obj: &JsObject,
    c: &FnCtx,
) -> Result<Vec<Value>> {
    if let Some(d) = default
        && obj.is_empty()
    {
        return Ok(d.to_vec());
    }
    let domains = field_domains(row, c);
    let mut values: Vec<Value> = match default {
        Some(d) => {
            let mut d = d.to_vec();
            d.resize(row.fields.len(), Value::Null);
            d
        }
        None => vec![Value::Null; row.fields.len()],
    };
    for (i, (name, ty)) in row.fields.iter().enumerate() {
        let field = obj.get(name);
        if default.is_some() && field.is_none() {
            continue;
        }
        let dflt = (!values[i].is_null()).then(|| values[i].clone());
        values[i] = populate_field(
            field,
            ty,
            domains.get(i).copied().flatten(),
            Some(name),
            dflt.as_ref(),
            c,
        )?;
    }
    Ok(values)
}

/// populate_composite (domain: a domain over the composite type).
fn populate_composite(
    v: Option<Jv>,
    row: &crate::types::RowType,
    domain: Option<u32>,
    default: Option<&Value>,
    c: &FnCtx,
) -> Result<Value> {
    let result = match v.filter(|v| v.kind() != JvKind::Null) {
        None => Value::Null,
        Some(v) => {
            let obj = JsObject::of(v)?;
            let default = match default {
                Some(Value::Record(r)) => Some(r.to_vec()),
                _ => None,
            };
            Value::Record(Arc::new(populate_record(row, default.as_deref(), &obj, c)?))
        }
    };
    if let Some(d) = domain {
        domain_check(d, &result, c)?;
    }
    Ok(result)
}

/// populate_record_field: a JSON value (None: the key is absent) as `ty`, declared as
/// `domain` if any.
fn populate_field(
    v: Option<Jv>,
    ty: &T,
    domain: Option<u32>,
    colname: Option<&str>,
    default: Option<&Value>,
    c: &FnCtx,
) -> Result<Value> {
    let kind = v.map_or(JvKind::Null, |v| v.kind());
    if let Some(d) = domain {
        // A domain over a composite type has its own path; any other is checked over its
        // base type's value.
        if let (T::Row(r), false) = (ty, kind == JvKind::String) {
            return populate_composite(v, r, Some(d), default, c);
        }
        let value = if kind == JvKind::Null {
            Value::Null
        } else {
            populate_field(v, ty, None, colname, None, c)?
        };
        domain_check(d, &value, c)?;
        return Ok(value);
    }
    let Some(v) = v.filter(|_| kind != JvKind::Null) else {
        return Ok(Value::Null);
    };
    match ty {
        // A string goes through the input function, whatever the type.
        _ if kind == JvKind::String => populate_scalar(v, ty, c),
        T::Array(elem) if !matches!(**elem, T::Unknown) => populate_array(v, elem, colname, c),
        T::Row(r) => populate_composite(Some(v), r, None, default, c),
        _ => populate_scalar(v, ty, c),
    }
}

/// The result row type for a call of `func` without a usable record argument
/// (get_record_type_from_query).
fn record_fields(c: &FnCtx, func: &str) -> Result<Arc<crate::types::RowType>> {
    match c.ret {
        T::Row(f) => Ok(f.clone()),
        _ => Err(Error::new(
            code::FEATURE_NOT_SUPPORTED,
            format!("could not determine row type for result of {func}"),
        )
        .hint(
            "Provide a non-null record argument, or call the function in the FROM clause using a column definition list.",
        )),
    }
}

/// The row type a json{b}_populate_record{set} call populates, its domain (over a composite)
/// and the record argument.
fn populate_target(
    a: &[Value],
    c: &FnCtx,
    func: &str,
) -> Result<(Arc<crate::types::RowType>, Option<u32>, Option<Vec<Value>>)> {
    let domain = match c.args.first() {
        Some(crate::sql::expr::Expr::CoerceToDomain(_, d)) => Some(d.oid),
        _ => None,
    };
    let rec = match &a[0] {
        Value::Record(r) => Some(r.to_vec()),
        _ => None,
    };
    let row = match c.arg_type(0) {
        T::Row(r) => {
            // A record argument's own row type, which a column definition list must match
            // (tupledesc_match).
            if r.oid == 0
                && rec.is_some()
                && let T::Row(want) = c.ret
                && !Arc::ptr_eq(&r, want)
            {
                if want.fields.len() != r.fields.len() {
                    let n = r.fields.len();
                    return Err(Error::new(
                        code::DATATYPE_MISMATCH,
                        "function return row and query-specified return row do not match",
                    )
                    .detail(format!(
                        "Returned row contains {n} attribute{}, but query expects {}.",
                        if n == 1 { "" } else { "s" },
                        want.fields.len()
                    )));
                }
                for (i, ((_, got), (_, exp))) in r.fields.iter().zip(&want.fields).enumerate() {
                    if got.oid() != exp.oid() {
                        return Err(Error::new(
                            code::DATATYPE_MISMATCH,
                            "function return row and query-specified return row do not match",
                        )
                        .detail(format!(
                            "Returned type {} at ordinal position {}, but query expects {}.",
                            got.display(),
                            i + 1,
                            exp.display()
                        )));
                    }
                }
            }
            r
        }
        T::Record | T::Unknown => record_fields(c, func)?,
        _ => {
            return Err(Error::new(
                code::DATATYPE_MISMATCH,
                format!("first argument of {func} must be a row type"),
            ));
        }
    };
    Ok((row, domain, rec))
}

fn json_arg(v: &Value) -> Jv<'_> {
    match v {
        Value::Json(s) | Value::Text(s) => Jv::Json(s),
        Value::Jsonb(j) => Jv::Jsonb(j),
        _ => Jv::Jsonb(&Jsonb::Null),
    }
}

/// populate_record_worker: one row (None: a NULL record).
fn record_worker(a: &[Value], c: &FnCtx, func: &str, have_record_arg: bool) -> Result<Option<Vec<Value>>> {
    let (row, domain, rec) = if have_record_arg {
        populate_target(a, c, func)?
    } else {
        (record_fields(c, func)?, None, None)
    };
    let json = &a[usize::from(have_record_arg)];
    if json.is_null() {
        return Ok(rec);
    }
    let rec_value = rec.map(|r| Value::Record(Arc::new(r)));
    match populate_composite(Some(json_arg(json)), &row, domain, rec_value.as_ref(), c)? {
        Value::Record(r) => Ok(Some(r.to_vec())),
        _ => Ok(None),
    }
}

/// populate_recordset_worker.
fn recordset_worker(a: &[Value], c: &FnCtx, func: &str, have_record_arg: bool) -> Result<SrfIter> {
    let (row, domain, rec) = if have_record_arg {
        populate_target(a, c, func)?
    } else {
        (record_fields(c, func)?, None, None)
    };
    let json = &a[usize::from(have_record_arg)];
    if json.is_null() {
        return Ok(Box::new(std::iter::empty()));
    }
    let v = json_arg(json);
    let elems = match (v, v.kind()) {
        (_, JvKind::Array) => v.items(),
        (Jv::Json(_), JvKind::Object) => {
            bail!(INVALID_PARAMETER_VALUE, "cannot call {func} on an object")
        }
        (Jv::Json(_), _) => bail!(INVALID_PARAMETER_VALUE, "cannot call {func} on a scalar"),
        _ => bail!(INVALID_PARAMETER_VALUE, "cannot call {func} on a non-array"),
    };
    let mut rows = Vec::with_capacity(elems.len());
    for e in elems {
        if e.kind() != JvKind::Object {
            bail!(
                INVALID_PARAMETER_VALUE,
                "argument of {func} must be an array of objects"
            );
        }
        let obj = JsObject::of(e)?;
        let r = populate_record(&row, rec.as_deref(), &obj, c)?;
        if let Some(d) = domain {
            domain_check(d, &Value::Record(Arc::new(r.clone())), c)?;
        }
        rows.push(r);
    }
    Ok(Box::new(rows.into_iter().map(Ok)))
}

fn one_row(r: Option<Vec<Value>>, c: &FnCtx) -> SrfIter {
    let n = match c.ret {
        T::Row(r) => r.fields.len(),
        _ => 1,
    };
    Box::new(std::iter::once(Ok(r.unwrap_or_else(|| vec![Value::Null; n]))))
}

fn json_to_record(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    Ok(one_row(record_worker(a, c, "json_to_record", false)?, c))
}
fn jsonb_to_record(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    Ok(one_row(record_worker(a, c, "jsonb_to_record", false)?, c))
}
fn json_to_recordset(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    recordset_worker(a, c, "json_to_recordset", false)
}
fn jsonb_to_recordset(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    recordset_worker(a, c, "jsonb_to_recordset", false)
}
fn json_populate_record(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    Ok(one_row(record_worker(a, c, "json_populate_record", true)?, c))
}
fn jsonb_populate_record(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    Ok(one_row(record_worker(a, c, "jsonb_populate_record", true)?, c))
}
/// jsonb_populate_record_valid: whether jsonb_populate_record would succeed (its errors
/// are soft ones).
fn jsonb_populate_record_valid(a: &[Value], c: &FnCtx) -> Result<Value> {
    let (row, domain, rec) = populate_target(a, c, "jsonb_populate_record")?;
    if a[1].is_null() {
        return Ok(Value::Bool(true));
    }
    let rec_value = rec.map(|r| Value::Record(Arc::new(r)));
    Ok(Value::Bool(
        populate_composite(Some(json_arg(&a[1])), &row, domain, rec_value.as_ref(), c).is_ok(),
    ))
}
fn json_populate_recordset(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    recordset_worker(a, c, "json_populate_recordset", true)
}
fn jsonb_populate_recordset(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    recordset_worker(a, c, "jsonb_populate_recordset", true)
}

fn json_each(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    each_rows(a, false, false)
}
fn jsonb_each(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    each_rows(a, false, true)
}
fn each_text(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    each_rows(a, true, false)
}

fn elements(a: &[Value], as_text: bool) -> Result<SrfIter> {
    if let Value::Json(s) = &a[0] {
        check_escapes(s)?;
    }
    let rows: Vec<Vec<Value>> = match &a[0] {
        Value::Json(s) => match raw_array(s) {
            Some(items) => items
                .into_iter()
                .map(|v| {
                    vec![if as_text {
                        raw_as_text(v)
                    } else {
                        json_text(v)
                    }]
                })
                .collect(),
            None => bail!(
                INVALID_PARAMETER_VALUE,
                "cannot extract elements from {}",
                if raw_typeof(s) == "object" {
                    "an object"
                } else {
                    "a scalar"
                }
            ),
        },
        v => match &*jb(v)? {
            Jsonb::Array(items) => items
                .iter()
                .map(|v| {
                    vec![if as_text {
                        v.as_text().map(text).unwrap_or(Value::Null)
                    } else {
                        jv(v.clone())
                    }]
                })
                .collect(),
            Jsonb::Object(_) => bail!(
                INVALID_PARAMETER_VALUE,
                "cannot extract elements from an object"
            ),
            _ => bail!(
                INVALID_PARAMETER_VALUE,
                "cannot extract elements from a scalar"
            ),
        },
    };
    Ok(Box::new(rows.into_iter().map(Ok)))
}
fn array_elements(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    elements(a, false)
}
fn array_elements_text(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    elements(a, true)
}
fn object_keys(a: &[Value], _: &FnCtx) -> Result<SrfIter> {
    if let Value::Json(s) = &a[0] {
        check_escapes(s)?;
    }
    let keys: Vec<String> = match &a[0] {
        Value::Json(s) => match raw_object(s) {
            Some(m) => m.into_iter().map(|(k, _)| k).collect(),
            None => bail!(
                INVALID_PARAMETER_VALUE,
                "cannot call json_object_keys on {}",
                if raw_typeof(s) == "array" {
                    "an array"
                } else {
                    "a scalar"
                }
            ),
        },
        v => match &*jb(v)? {
            Jsonb::Object(p) => p.iter().map(|(k, _)| k.clone()).collect(),
            Jsonb::Array(_) => bail!(
                INVALID_PARAMETER_VALUE,
                "cannot call jsonb_object_keys on an array"
            ),
            _ => bail!(
                INVALID_PARAMETER_VALUE,
                "cannot call jsonb_object_keys on a scalar"
            ),
        },
    };
    Ok(Box::new(keys.into_iter().map(|k| Ok(vec![text(k)]))))
}

pub fn register(r: &mut Registry) {
    use T::*;
    for j in [Json, Jsonb] {
        let out = j.clone();
        r.binop("->", j.clone(), Text, out.clone(), get_field);
        r.binop("->", j.clone(), Int4, out.clone(), get_field);
        r.binop("->>", j.clone(), Text, Text, get_field_text);
        r.binop("->>", j.clone(), Int4, Text, get_field_text);
        r.binop("#>", j.clone(), T::array_of(Text), out.clone(), get_path);
        r.binop("#>>", j.clone(), T::array_of(Text), Text, get_path_text);
    }
    r.func(
        "json_extract_path",
        vec![Json, T::array_of(Text)],
        Json,
        extract_path,
    )
    .variadic();
    r.func(
        "jsonb_extract_path",
        vec![Jsonb, T::array_of(Text)],
        Jsonb,
        extract_path,
    )
    .variadic();
    r.func(
        "json_extract_path_text",
        vec![Json, T::array_of(Text)],
        Text,
        extract_path_text,
    )
    .variadic();
    r.func(
        "jsonb_extract_path_text",
        vec![Jsonb, T::array_of(Text)],
        Text,
        extract_path_text,
    )
    .variadic();
    r.func("json_object_field", vec![Json, Text], Json, get_field);
    r.func("jsonb_object_field", vec![Jsonb, Text], Jsonb, get_field);
    r.func(
        "json_object_field_text",
        vec![Json, Text],
        Text,
        get_field_text,
    );
    r.func(
        "jsonb_object_field_text",
        vec![Jsonb, Text],
        Text,
        get_field_text,
    );
    r.func("json_array_element", vec![Json, Int4], Json, get_field);
    r.func("jsonb_array_element", vec![Jsonb, Int4], Jsonb, get_field);
    r.func(
        "json_array_element_text",
        vec![Json, Int4],
        Text,
        get_field_text,
    );
    r.func(
        "jsonb_array_element_text",
        vec![Jsonb, Int4],
        Text,
        get_field_text,
    );
    r.binop("@>", Jsonb, Jsonb, Bool, contains);
    r.binop("<@", Jsonb, Jsonb, Bool, contained);
    r.func("jsonb_contains", vec![Jsonb, Jsonb], Bool, contains);
    r.func("jsonb_contained", vec![Jsonb, Jsonb], Bool, contained);
    r.binop("?", Jsonb, Text, Bool, exists);
    // SQL/JSON path: (target, path [, vars [, silent]]); the operators are silent.
    let names: &[&'static str] = &["target", "path", "vars", "silent"];
    r.func(
        "jsonb_path_exists",
        vec![Jsonb, JsonPath, Jsonb, Bool],
        Bool,
        path_exists,
    )
    .min_args(2)
    .names(names)
    .defaults(path_defaults());
    r.func(
        "jsonb_path_exists_tz",
        vec![Jsonb, JsonPath, Jsonb, Bool],
        Bool,
        path_exists_tz,
    )
    .min_args(2)
    .names(names)
    .defaults(path_defaults())
    .stable();
    r.func(
        "jsonb_path_match",
        vec![Jsonb, JsonPath, Jsonb, Bool],
        Bool,
        path_match,
    )
    .min_args(2)
    .names(names)
    .defaults(path_defaults());
    r.func(
        "jsonb_path_match_tz",
        vec![Jsonb, JsonPath, Jsonb, Bool],
        Bool,
        path_match_tz,
    )
    .min_args(2)
    .names(names)
    .defaults(path_defaults())
    .stable();
    r.func(
        "jsonb_path_query_array",
        vec![Jsonb, JsonPath, Jsonb, Bool],
        Jsonb,
        path_query_array,
    )
    .min_args(2)
    .names(names)
    .defaults(path_defaults());
    r.func(
        "jsonb_path_query_array_tz",
        vec![Jsonb, JsonPath, Jsonb, Bool],
        Jsonb,
        path_query_array_tz,
    )
    .min_args(2)
    .names(names)
    .defaults(path_defaults())
    .stable();
    r.func(
        "jsonb_path_query_first",
        vec![Jsonb, JsonPath, Jsonb, Bool],
        Jsonb,
        path_query_first,
    )
    .min_args(2)
    .names(names)
    .defaults(path_defaults());
    r.func(
        "jsonb_path_query_first_tz",
        vec![Jsonb, JsonPath, Jsonb, Bool],
        Jsonb,
        path_query_first_tz,
    )
    .min_args(2)
    .names(names)
    .defaults(path_defaults())
    .stable();
    r.srf(
        "jsonb_path_query",
        vec![Jsonb, JsonPath, Jsonb, Bool],
        Jsonb,
        path_query,
    )
    .min_args(2)
    .names(names)
    .defaults(path_defaults());
    r.srf(
        "jsonb_path_query_tz",
        vec![Jsonb, JsonPath, Jsonb, Bool],
        Jsonb,
        path_query_tz,
    )
    .min_args(2)
    .names(names)
    .defaults(path_defaults())
    .stable();
    r.binop("@?", Jsonb, JsonPath, Bool, path_exists_opr);
    r.binop("@@", Jsonb, JsonPath, Bool, path_match_opr);
    r.binop("?|", Jsonb, T::array_of(Text), Bool, exists_any);
    r.binop("?&", Jsonb, T::array_of(Text), Bool, exists_all);
    r.func("jsonb_exists", vec![Jsonb, Text], Bool, exists);
    r.func(
        "jsonb_exists_any",
        vec![Jsonb, T::array_of(Text)],
        Bool,
        exists_any,
    );
    r.func(
        "jsonb_exists_all",
        vec![Jsonb, T::array_of(Text)],
        Bool,
        exists_all,
    );
    r.binop("||", Jsonb, Jsonb, Jsonb, concat);
    r.binop("-", Jsonb, Text, Jsonb, delete_key);
    r.binop("-", Jsonb, T::array_of(Text), Jsonb, delete_keys);
    r.binop("-", Jsonb, Int4, Jsonb, delete_index);
    r.binop("#-", Jsonb, T::array_of(Text), Jsonb, delete_path);
    r.func("to_json", vec![AnyElement], Json, to_json).stable();
    r.func("to_jsonb", vec![AnyElement], Jsonb, to_jsonb)
        .stable();
    r.func("array_to_json", vec![AnyArray], Json, array_to_json)
        .stable();
    r.func("array_to_json", vec![AnyArray, Bool], Json, array_to_json)
        .stable();
    r.func("row_to_json", vec![Record], Json, row_to_json)
        .stable();
    r.func("row_to_json", vec![Record, Bool], Json, row_to_json)
        .stable();
    r.func(
        "json_build_array",
        vec![T::array_of(Any)],
        Json,
        build_array,
    )
    .variadic()
    .non_strict()
    .stable()
    .min_args(0);
    r.func("json_build_array", vec![], Json, build_array)
        .non_strict();
    r.func(
        "jsonb_build_array",
        vec![T::array_of(Any)],
        Jsonb,
        jsonb_build_array,
    )
    .variadic()
    .non_strict()
    .stable();
    r.func("jsonb_build_array", vec![], Jsonb, jsonb_build_array)
        .non_strict();
    r.func(
        "json_build_object",
        vec![T::array_of(Any)],
        Json,
        build_object,
    )
    .variadic()
    .non_strict()
    .stable();
    r.func("json_build_object", vec![], Json, build_object)
        .non_strict();
    r.func(
        "jsonb_build_object",
        vec![T::array_of(Any)],
        Jsonb,
        jsonb_build_object,
    )
    .variadic()
    .non_strict()
    .stable();
    r.func("jsonb_build_object", vec![], Jsonb, jsonb_build_object)
        .non_strict();
    r.func("json_object", vec![T::array_of(Text)], Json, json_object);
    r.func(
        "json_object",
        vec![T::array_of(Text), T::array_of(Text)],
        Json,
        json_object,
    );
    r.func("jsonb_object", vec![T::array_of(Text)], Jsonb, jsonb_object);
    r.func(
        "jsonb_object",
        vec![T::array_of(Text), T::array_of(Text)],
        Jsonb,
        jsonb_object,
    );
    r.srf("json_to_record", vec![Json], Record, json_to_record);
    r.srf("jsonb_to_record", vec![Jsonb], Record, jsonb_to_record);
    r.srf("json_to_recordset", vec![Json], Record, json_to_recordset);
    r.srf(
        "jsonb_to_recordset",
        vec![Jsonb],
        Record,
        jsonb_to_recordset,
    );
    r.srf(
        "json_populate_record",
        vec![AnyElement, Json],
        AnyElement,
        json_populate_record,
    )
    .non_strict();
    r.srf(
        "jsonb_populate_record",
        vec![AnyElement, Jsonb],
        AnyElement,
        jsonb_populate_record,
    )
    .non_strict();
    r.func(
        "jsonb_populate_record_valid",
        vec![AnyElement, Jsonb],
        Bool,
        jsonb_populate_record_valid,
    )
    .non_strict();
    r.srf(
        "json_populate_recordset",
        vec![AnyElement, Json],
        AnyElement,
        json_populate_recordset,
    )
    .non_strict();
    r.srf(
        "jsonb_populate_recordset",
        vec![AnyElement, Jsonb],
        AnyElement,
        jsonb_populate_recordset,
    )
    .non_strict();
    r.func("json_array_length", vec![Json], Int4, array_length);
    r.func("jsonb_array_length", vec![Jsonb], Int4, array_length);
    r.func("json_typeof", vec![Json], Text, typeof_);
    r.func("jsonb_typeof", vec![Jsonb], Text, typeof_);
    r.func("jsonb_pretty", vec![Jsonb], Text, pretty);
    r.func("json_strip_nulls", vec![Json], Json, strip_nulls);
    r.func("json_strip_nulls", vec![Json, Bool], Json, strip_nulls);
    r.func("jsonb_strip_nulls", vec![Jsonb], Jsonb, strip_nulls);
    r.func("jsonb_strip_nulls", vec![Jsonb, Bool], Jsonb, strip_nulls);
    r.func(
        "jsonb_set",
        vec![Jsonb, T::array_of(Text), Jsonb],
        Jsonb,
        jsonb_set,
    );
    r.func(
        "jsonb_set",
        vec![Jsonb, T::array_of(Text), Jsonb, Bool],
        Jsonb,
        jsonb_set,
    );
    r.func(
        "jsonb_set_lax",
        vec![Jsonb, T::array_of(Text), Jsonb, Bool, Text],
        Jsonb,
        jsonb_set_lax,
    )
    .non_strict()
    .min_args(3)
    .names(&[
        "jsonb_in",
        "path",
        "replacement",
        "create_if_missing",
        "null_value_treatment",
    ])
    .defaults(vec![Value::Bool(true), text("use_json_null")]);
    r.func(
        "jsonb_insert",
        vec![Jsonb, T::array_of(Text), Jsonb],
        Jsonb,
        jsonb_insert,
    );
    r.func(
        "jsonb_insert",
        vec![Jsonb, T::array_of(Text), Jsonb, Bool],
        Jsonb,
        jsonb_insert,
    );
    r.func(
        "jsonb_delete_path",
        vec![Jsonb, T::array_of(Text)],
        Jsonb,
        delete_path,
    );
    r.func("json_scalar", vec![AnyElement], Json, json_scalar)
        .stable();
    r.srf_cols(
        "json_each",
        vec![Json],
        vec![("key", Text), ("value", Json)],
        json_each,
    );
    r.srf_cols(
        "jsonb_each",
        vec![Jsonb],
        vec![("key", Text), ("value", Jsonb)],
        jsonb_each,
    );
    r.srf_cols(
        "json_each_text",
        vec![Json],
        vec![("key", Text), ("value", Text)],
        each_text,
    );
    r.srf_cols(
        "jsonb_each_text",
        vec![Jsonb],
        vec![("key", Text), ("value", Text)],
        each_text,
    );
    r.srf("json_array_elements", vec![Json], Json, array_elements)
        .out_name("value");
    r.srf("jsonb_array_elements", vec![Jsonb], Jsonb, array_elements)
        .out_name("value");
    r.srf(
        "json_array_elements_text",
        vec![Json],
        Text,
        array_elements_text,
    )
    .out_name("value");
    r.srf(
        "jsonb_array_elements_text",
        vec![Jsonb],
        Text,
        array_elements_text,
    )
    .out_name("value");
    r.srf("json_object_keys", vec![Json], Text, object_keys);
    r.srf("jsonb_object_keys", vec![Jsonb], Text, object_keys);
    let _ = Ret::Poly;
}

// ── SQL/JSON path ──

/// `vars` defaults to `'{}'`, `silent` to false.
fn path_defaults() -> Vec<Value> {
    vec![jv(Jsonb::Object(vec![])), Value::Bool(false)]
}

/// Arguments of the jsonb_path_* functions: target, parsed path, vars, silent.
fn path_args(a: &[Value]) -> Result<(Arc<Jsonb>, jsonpath::JsonPath, Option<Arc<Jsonb>>, bool)> {
    let path = jsonpath::parse(text_arg(&a[1]))?;
    let vars = a.get(2).map(jb).transpose()?;
    let silent = matches!(a.get(3), Some(Value::Bool(true)));
    Ok((jb(&a[0])?, path, vars, silent))
}

fn path_run(a: &[Value], c: &FnCtx, tz: bool, silent: Option<bool>) -> Result<Option<Vec<Jsonb>>> {
    let (target, path, vars, s) = path_args(a)?;
    // (a silenced error keeps the items found before it)
    jsonpath::query_partial_with(
        &target,
        &path,
        vars.as_deref(),
        silent.unwrap_or(s),
        tz,
        &c.exec.fmt().tz,
        &|e| c.exec.notice(e),
    )
    .map(Some)
}

fn path_exists_any(a: &[Value], c: &FnCtx, tz: bool, silent: Option<bool>) -> Result<Value> {
    let (target, path, vars, s) = path_args(a)?;
    Ok(
        match jsonpath::exists_with(
            &target,
            &path,
            vars.as_deref(),
            silent.unwrap_or(s),
            tz,
            &c.exec.fmt().tz,
            &|e| c.exec.notice(e),
        )? {
            Some(b) => Value::Bool(b),
            None => Value::Null,
        },
    )
}

fn path_match_any(a: &[Value], c: &FnCtx, tz: bool, silent: Option<bool>) -> Result<Value> {
    let (target, path, vars, s) = path_args(a)?;
    Ok(
        match jsonpath::matches_with(
            &target,
            &path,
            vars.as_deref(),
            silent.unwrap_or(s),
            tz,
            &c.exec.fmt().tz,
            &|e| c.exec.notice(e),
        )? {
            Some(b) => Value::Bool(b),
            None => Value::Null,
        },
    )
}

fn path_exists(a: &[Value], c: &FnCtx) -> Result<Value> {
    path_exists_any(a, c, false, None)
}
fn path_exists_tz(a: &[Value], c: &FnCtx) -> Result<Value> {
    path_exists_any(a, c, true, None)
}
fn path_exists_opr(a: &[Value], c: &FnCtx) -> Result<Value> {
    path_exists_any(a, c, false, Some(true))
}
fn path_match(a: &[Value], c: &FnCtx) -> Result<Value> {
    path_match_any(a, c, false, None)
}
fn path_match_tz(a: &[Value], c: &FnCtx) -> Result<Value> {
    path_match_any(a, c, true, None)
}
fn path_match_opr(a: &[Value], c: &FnCtx) -> Result<Value> {
    path_match_any(a, c, false, Some(true))
}
fn path_query_array(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(jv(Jsonb::Array(
        path_run(a, c, false, None)?.unwrap_or_default(),
    )))
}
fn path_query_array_tz(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(jv(Jsonb::Array(
        path_run(a, c, true, None)?.unwrap_or_default(),
    )))
}
fn path_query_first(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(path_run(a, c, false, None)?
        .and_then(|r| r.into_iter().next())
        .map(jv)
        .unwrap_or(Value::Null))
}
fn path_query_first_tz(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(path_run(a, c, true, None)?
        .and_then(|r| r.into_iter().next())
        .map(jv)
        .unwrap_or(Value::Null))
}
fn path_query(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let rows = path_run(a, c, false, None)?.unwrap_or_default();
    Ok(Box::new(rows.into_iter().map(|j| Ok(vec![jv(j)]))))
}
fn path_query_tz(a: &[Value], c: &FnCtx) -> Result<SrfIter> {
    let rows = path_run(a, c, true, None)?.unwrap_or_default();
    Ok(Box::new(rows.into_iter().map(|j| Ok(vec![jv(j)]))))
}
