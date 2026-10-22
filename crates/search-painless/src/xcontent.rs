//! Serializing script values the way Elasticsearch's `XContentBuilder.value(Object)` does.
//!
//! Update, update-by-query, reindex and ingest scripts hand back a `_source` map that
//! Elasticsearch writes with Jackson: numbers as Java prints them (`1.0E300`, `-0.0`, floats
//! from `Float.toString`), `BigDecimal` by `toString` (`1E+3`), non-finite doubles as strings,
//! `java.time` values through its date formatters, primitive and object arrays and any
//! `Iterable` as JSON arrays. Anything else (a `Character`, a `Pattern`, an `Optional`…) fails
//! with `cannot write xcontent for unknown value of type class …`, which the write APIs report
//! as an `illegal_argument_exception`. Hosts store the returned bytes as the new `_source`.

use crate::lookup::java_binary_name;
use crate::time::TimeVal;
use crate::value::{ArrayData, Obj, Value, ViewKind};

/// Why a value cannot be written: Elasticsearch's exception type and reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XContentError {
    pub kind: &'static str,
    pub reason: String,
}

fn unknown(class: &str) -> XContentError {
    XContentError { kind: "illegal_argument_exception", reason: format!("cannot write xcontent for unknown value of type class {class}") }
}

/// Parses a document `_source` the way Elasticsearch hands it to write scripts: objects are
/// `LinkedHashMap`s in document order, arrays `ArrayList`s, integers `Integer`, `Long` or
/// `BigInteger`, other numbers `Double`.
pub fn parse_source(json: &str) -> Result<Value, XContentError> {
    parse_json(json, crate::types::cls::LINKED_HASH_MAP)
}

/// Like [`parse_source`] with `HashMap`s, as Elasticsearch parses sources it reads unordered
/// (the `upsert` document of a scripted upsert).
pub fn parse_source_unordered(json: &str) -> Result<Value, XContentError> {
    parse_json(json, crate::types::cls::HASH_MAP)
}

fn parse_json(json: &str, map_class: crate::types::ClassId) -> Result<Value, XContentError> {
    crate::es::misc::json_parse(json, map_class).map_err(|t| XContentError {
        kind: if t.es_type == Some("x_content_e_o_f_exception") { "x_content_e_o_f_exception" } else { "x_content_parse_exception" },
        reason: t.message.clone().unwrap_or_default(),
    })
}

/// Writes `v` as compact JSON.
pub fn to_xcontent(v: &Value) -> Result<String, XContentError> {
    let mut out = String::new();
    let mut path = Vec::new();
    write(v, &mut out, &mut path)?;
    Ok(out)
}

/// Checks that a value survives `IngestDocument.deepCopy`, which the simulate pipeline API runs
/// over the document after every processor: maps, lists, sets, `byte[]`, `double[]`, strings,
/// `Integer`, `Long`, `Float`, `Double`, `Boolean`, `ZonedDateTime` and `Date`. Anything else
/// fails with `unexpected value type [class …]`.
pub fn check_ingest_value(v: &Value) -> Result<(), XContentError> {
    let fail = |class: &str| Err(XContentError { kind: "illegal_argument_exception", reason: format!("unexpected value type [class {class}]") });
    match v {
        Value::Null | Value::Bool(_) | Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) | Value::Str(_) => Ok(()),
        Value::Byte(_) => fail("java.lang.Byte"),
        Value::Short(_) => fail("java.lang.Short"),
        Value::Char(_) => fail("java.lang.Character"),
        Value::List(l) => l.v.borrow().iter().try_for_each(check_ingest_value),
        Value::Set(s) => s.keys().iter().try_for_each(check_ingest_value),
        // keys are copied by reference
        Value::Map(m) => m.entries().iter().try_for_each(|(_, x)| check_ingest_value(x)),
        Value::Obj(o) => match &**o {
            Obj::Array(a) => match &*a.data.borrow() {
                ArrayData::Byte(_) | ArrayData::Double(_) => Ok(()),
                // `double[][]`
                ArrayData::Ref(x) if a.elem.dims == 1 && a.elem.class == crate::types::cls::DOUBLE => x.iter().try_for_each(check_ingest_value),
                _ => fail(&a.descriptor()),
            },
            Obj::Time(TimeVal::Zoned { .. } | TimeVal::UtilDate(_)) => Ok(()),
            Obj::MapView { map, kind: ViewKind::Keys } => map.keys().iter().try_for_each(check_ingest_value),
            Obj::Es(crate::es::EsObj::Ctx(c)) => c.entries().iter().try_for_each(|(_, x)| check_ingest_value(x)),
            _ => fail(java_binary_name(v.class())),
        },
    }
}

/// Jackson's string escaping: quotes, backslashes and control characters (short forms for
/// `\b \t \n \f \r`, `\u00XX` otherwise); everything else is written as is.
pub fn escape_into(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\u{c}' => out.push_str("\\f"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                const HEX: &[u8; 16] = b"0123456789ABCDEF";
                out.push_str("\\u00");
                out.push(HEX[(c as usize) >> 4] as char);
                out.push(HEX[(c as usize) & 15] as char);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn double(d: f64, out: &mut String) {
    let s = crate::java::fmt_double(d);
    if d.is_finite() {
        out.push_str(&s);
    } else {
        escape_into(&s, out);
    }
}

fn float(f: f32, out: &mut String) {
    let s = crate::java::fmt_float(f);
    if f.is_finite() {
        out.push_str(&s);
    } else {
        escape_into(&s, out);
    }
}

/// Identity of a container, for the self-reference check.
fn identity(v: &Value) -> Option<usize> {
    match v {
        Value::List(l) => Some(std::rc::Rc::as_ptr(l) as *const u8 as usize),
        Value::Map(m) | Value::Set(m) => Some(std::rc::Rc::as_ptr(m) as *const u8 as usize),
        Value::Obj(o) => Some(std::rc::Rc::as_ptr(o) as *const u8 as usize),
        _ => None,
    }
}

/// The JSON being written outgrew the object size limit.
fn too_large(out: &str) -> Result<(), XContentError> {
    if out.len() > crate::guard::max_object_bytes() {
        return Err(XContentError { kind: "out_of_memory_error", reason: "Java heap space".into() });
    }
    Ok(())
}

fn array<'a>(items: impl Iterator<Item = &'a Value>, out: &mut String, path: &mut Vec<usize>) -> Result<(), XContentError> {
    out.push('[');
    for (i, x) in items.enumerate() {
        too_large(out)?;
        if i > 0 {
            out.push(',');
        }
        write(x, out, path)?;
    }
    out.push(']');
    Ok(())
}

fn object(entries: &[(Value, Value)], out: &mut String, path: &mut Vec<usize>) -> Result<(), XContentError> {
    out.push('{');
    for (i, (k, x)) in entries.iter().enumerate() {
        too_large(out)?;
        if i > 0 {
            out.push(',');
        }
        match k {
            Value::Str(s) => escape_into(s.as_str(), out),
            Value::Null => return Err(XContentError { kind: "null_pointer_exception", reason: "Field name cannot be null".into() }),
            other => {
                let c = java_binary_name(other.class());
                return Err(XContentError {
                    kind: "class_cast_exception",
                    reason: format!("class {c} cannot be cast to class java.lang.String ({c} and java.lang.String are in module java.base of loader 'bootstrap')"),
                });
            }
        }
        out.push(':');
        write(x, out, path)?;
    }
    out.push('}');
    Ok(())
}

fn write(v: &Value, out: &mut String, path: &mut Vec<usize>) -> Result<(), XContentError> {
    // maps and iterables that contain themselves would recurse forever
    let id = identity(v);
    if let Some(id) = id {
        if path.len() >= crate::value::MAX_JSON_DEPTH && matches!(v, Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Obj(_)) {
            return Err(XContentError { kind: "stream_constraints_exception", reason: crate::value::nesting_depth_message(path.len() + 1) });
        }
        if path.contains(&id) {
            return Err(XContentError { kind: "illegal_argument_exception", reason: "Iterable object is self-referencing itself".into() });
        }
        path.push(id);
    }
    let r = write_inner(v, out, path);
    if id.is_some() {
        path.pop();
    }
    r
}

fn prims<T: Copy>(x: &[T], f: impl Fn(T) -> Value, out: &mut String, path: &mut Vec<usize>) -> Result<(), XContentError> {
    let vals: Vec<Value> = x.iter().map(|&b| f(b)).collect();
    array(vals.iter(), out, path)
}

fn write_inner(v: &Value, out: &mut String, path: &mut Vec<usize>) -> Result<(), XContentError> {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Byte(x) => out.push_str(&x.to_string()),
        Value::Short(x) => out.push_str(&x.to_string()),
        Value::Int(x) => out.push_str(&x.to_string()),
        Value::Long(x) => out.push_str(&x.to_string()),
        Value::Float(f) => float(*f, out),
        Value::Double(d) => double(*d, out),
        Value::Char(_) => return Err(unknown("java.lang.Character")),
        Value::Str(s) => escape_into(s.as_str(), out),
        Value::List(l) => {
            let items = l.v.borrow();
            array(items.iter(), out, path)?;
        }
        Value::Set(s) => array(s.keys().iter(), out, path)?,
        Value::Map(m) => object(&m.entries(), out, path)?,
        Value::Obj(o) => match &**o {
            Obj::Array(a) => match &*a.data.borrow() {
                ArrayData::Bool(x) => prims(x, Value::Bool, out, path)?,
                // `byte[]` is binary: Jackson writes it base64-encoded
                ArrayData::Byte(x) => {
                    use base64::Engine;
                    let bytes: Vec<u8> = x.iter().map(|&b| b as u8).collect();
                    escape_into(&base64::engine::general_purpose::STANDARD.encode(bytes), out);
                }
                ArrayData::Short(x) => prims(x, Value::Short, out, path)?,
                ArrayData::Char(_) => return Err(unknown("[C")),
                ArrayData::Int(x) => prims(x, Value::Int, out, path)?,
                ArrayData::Long(x) => prims(x, Value::Long, out, path)?,
                ArrayData::Float(x) => prims(x, Value::Float, out, path)?,
                ArrayData::Double(x) => prims(x, Value::Double, out, path)?,
                ArrayData::Ref(x) => array(x.iter(), out, path)?,
            },
            Obj::MapView { map, kind } => match kind {
                ViewKind::Keys => array(map.keys().iter(), out, path)?,
                ViewKind::Values => array(map.values().iter(), out, path)?,
                // an entry set is iterable, but its entries are not writable
                ViewKind::Entries => {
                    if !map.is_empty() {
                        return Err(unknown(entry_class(map.order())));
                    }
                    out.push_str("[]");
                }
            },
            Obj::BigInt(b) => out.push_str(&b.to_string()),
            Obj::BigDec(d) => out.push_str(&d.to_java_string()),
            Obj::Time(t) => time(t, out)?,
            Obj::Es(crate::es::EsObj::Ctx(c)) => object(&c.entries(), out, path)?,
            // `ScriptDocValues` are lists
            Obj::Es(crate::es::EsObj::Values(sdv)) => array(sdv.vals.iter(), out, path)?,
            // `GeoPoint` is a `ToXContentFragment` writing `latlon`
            Obj::Es(crate::es::EsObj::GeoPoint(lat, lon)) => {
                out.push_str("{\"lat\":");
                double(*lat, out);
                out.push_str(",\"lon\":");
                double(*lon, out);
                out.push('}');
            }
            // `GeoBoundingBox` writes its corners
            Obj::Es(crate::es::EsObj::BoundingBox(tl, br)) => {
                out.push_str("{\"top_left\":{\"lat\":");
                double(tl.0, out);
                out.push_str(",\"lon\":");
                double(tl.1, out);
                out.push_str("},\"bottom_right\":{\"lat\":");
                double(br.0, out);
                out.push_str(",\"lon\":");
                double(br.1, out);
                out.push_str("}}");
            }
            _ => return Err(unknown(java_binary_name(v.class()))),
        },
    }
    Ok(())
}

fn entry_class(order: crate::value::MapOrder) -> &'static str {
    match order {
        crate::value::MapOrder::Hash => "java.util.HashMap$Node",
        crate::value::MapOrder::Insertion => "java.util.LinkedHashMap$Entry",
        crate::value::MapOrder::Sorted => "java.util.TreeMap$Entry",
    }
}

/// `strict_date_optional_time_nanos`, `HH:mm:ss.SSS` and the `toString` forms the
/// `XContentElasticsearchExtension` writers use.
fn time(t: &TimeVal, out: &mut String) -> Result<(), XContentError> {
    use crate::time::{format_date, offset_id};
    let millis = |n: i64| format!(".{:03}", n / 1_000_000);
    let s = match t {
        TimeVal::Zoned { .. } | TimeVal::OffsetDateTime { .. } | TimeVal::Instant { .. } => t.to_xcontent_string(),
        TimeVal::LocalDateTime(d, tm) => TimeVal::Zoned { d: *d, t: *tm, offset: 0, zone: crate::time::Zone::Offset(0) }.to_xcontent_string(),
        TimeVal::UtilDate(ms) => TimeVal::Instant { secs: ms.get().div_euclid(1000), nanos: (ms.get().rem_euclid(1000) * 1_000_000) as i32 }.to_xcontent_string(),
        TimeVal::LocalDate(d) => format_date(*d),
        TimeVal::LocalTime(tm) => format!("{:02}:{:02}:{:02}{}", tm.hour(), tm.minute(), tm.second(), millis(tm.nano())),
        TimeVal::OffsetTime { t: tm, offset } => format!("{:02}:{:02}:{:02}{}{}", tm.hour(), tm.minute(), tm.second(), millis(tm.nano()), offset_id(*offset)),
        TimeVal::DayOfWeek(_) | TimeVal::Month(_) | TimeVal::MonthDay(..) | TimeVal::Year(_) | TimeVal::Duration { .. } | TimeVal::Period { .. } => t.to_java_string(),
        // enums write their `toString`
        TimeVal::TUnit(_) | TimeVal::TField(_) | TimeVal::Enum(..) => t.to_java_string(),
        _ => return Err(unknown(java_binary_name(Value::obj(Obj::Time(t.clone())).class()))),
    };
    escape_into(&s, out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_like_jackson() {
        let mut s = String::new();
        escape_into("a\"b\\c/\u{1}\u{7f}\n\t\u{2028}é", &mut s);
        assert_eq!(s, "\"a\\\"b\\\\c/\\u0001\u{7f}\\n\\t\u{2028}é\"");
    }

    #[test]
    fn numbers_print_as_java() {
        let v = crate::value::JList::array_list(vec![Value::Double(1e300), Value::Double(-0.0), Value::Float(1.5), Value::Double(f64::NAN), Value::Long(1 << 40)]);
        assert_eq!(to_xcontent(&v).unwrap(), "[1.0E300,-0.0,1.5,\"NaN\",1099511627776]");
        assert_eq!(to_xcontent(&Value::Char(97)).unwrap_err().reason, "cannot write xcontent for unknown value of type class java.lang.Character");
    }

    #[test]
    fn self_references_fail() {
        let l = crate::value::JList::array_list(vec![]);
        if let Value::List(inner) = &l {
            inner.v.borrow_mut().push(l.clone());
        }
        assert_eq!(to_xcontent(&l).unwrap_err().reason, "Iterable object is self-referencing itself");
        if let Value::List(inner) = &l {
            inner.v.borrow_mut().clear();
        }
    }
}
