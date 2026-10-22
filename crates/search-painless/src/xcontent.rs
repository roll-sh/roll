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
///
/// The walk keeps its own stack of open containers instead of recursing, so the nesting limit
/// (1,000, as Jackson's `StreamWriteConstraints`) costs no native stack: hosts call this on
/// ordinary 2 MiB threads.
pub fn to_xcontent(v: &Value) -> Result<String, XContentError> {
    let mut w = Writer { out: String::new(), path: Vec::new(), open: Vec::new() };
    w.value(v)?;
    while let Some(top) = w.open.last_mut() {
        match top {
            Open::Array { items, next } => {
                if *next == items.len() {
                    w.out.push(']');
                    w.close();
                    continue;
                }
                let x = std::mem::take(&mut items[*next]);
                *next += 1;
                too_large(&w.out)?;
                if *next > 1 {
                    w.out.push(',');
                }
                w.value(&x)?;
            }
            Open::Object { entries, next } => {
                if *next == entries.len() {
                    w.out.push('}');
                    w.close();
                    continue;
                }
                let (k, x) = std::mem::take(&mut entries[*next]);
                *next += 1;
                too_large(&w.out)?;
                if *next > 1 {
                    w.out.push(',');
                }
                field_name(&k, &mut w.out)?;
                w.out.push(':');
                w.value(&x)?;
            }
        }
    }
    Ok(w.out)
}

/// Checks that an ingest script's `_source` is one Elasticsearch accepts: first
/// `CollectionUtils.ensureNoSelfReferences` (see [`ensure_no_self_references`], hint
/// `source document`, as both the simulate API and real ingest report it), then
/// `IngestDocument.deepCopy`, which the simulate pipeline API runs over the document after every
/// processor: maps, lists, sets, `byte[]`, `double[]`, strings, `Integer`, `Long`, `Float`,
/// `Double`, `Boolean`, `ZonedDateTime` and `Date`. Anything else fails with
/// `unexpected value type [class …]`.
///
/// Both walks use an explicit stack: a script can build a cyclic or arbitrarily deep value, and
/// neither may recurse on the caller's stack.
pub fn check_ingest_value(v: &Value) -> Result<(), XContentError> {
    ensure_no_self_references(v, Some("source document"))?;
    let fail = |class: &str| Err(XContentError { kind: "illegal_argument_exception", reason: format!("unexpected value type [class {class}]") });
    // depth first, children in document order, like the recursive `deepCopy`
    let mut stack: Vec<Value> = vec![v.clone()];
    let push_rev = |stack: &mut Vec<Value>, items: Vec<Value>| stack.extend(items.into_iter().rev());
    while let Some(v) = stack.pop() {
        match &v {
            Value::Null | Value::Bool(_) | Value::Int(_) | Value::Long(_) | Value::Float(_) | Value::Double(_) | Value::Str(_) => {}
            Value::Byte(_) => return fail("java.lang.Byte"),
            Value::Short(_) => return fail("java.lang.Short"),
            Value::Char(_) => return fail("java.lang.Character"),
            Value::List(l) => {
                l.sync_view();
                let items = l.v.borrow().clone();
                push_rev(&mut stack, items);
            }
            Value::Set(s) => push_rev(&mut stack, s.keys()),
            // keys are copied by reference
            Value::Map(m) => push_rev(&mut stack, m.values()),
            Value::Obj(o) => match &**o {
                Obj::Array(a) => match &*a.data.borrow() {
                    ArrayData::Byte(_) | ArrayData::Double(_) => {}
                    // `double[][]`
                    ArrayData::Ref(x) if a.elem.dims == 1 && a.elem.class == crate::types::cls::DOUBLE => stack.extend(x.iter().rev().cloned()),
                    _ => return fail(&a.descriptor()),
                },
                Obj::Time(TimeVal::Zoned { .. } | TimeVal::UtilDate(_)) => {}
                Obj::MapView { map, kind: ViewKind::Keys } => push_rev(&mut stack, map.keys()),
                // an entry set is a `Set` too; its entries are not copyable
                Obj::MapView { map, kind: ViewKind::Entries } if !map.is_empty() => return fail(entry_class(map.order())),
                Obj::MapView { kind: ViewKind::Entries, .. } => {}
                Obj::Es(crate::es::EsObj::Ctx(c)) => push_rev(&mut stack, c.entries().into_iter().map(|(_, x)| x).collect()),
                _ => return fail(java_binary_name(v.class())),
            },
        }
    }
    Ok(())
}

/// Elasticsearch's `CollectionUtils.ensureNoSelfReferences`: fails with `Iterable object is
/// self-referencing itself` (plus ` (<hint>)`) when a non-empty map, an iterable or an object
/// array contains itself (its keys and values, at any depth). Values reached twice along
/// different paths are fine; only ancestors count. Iterative, so a deep value cannot overflow the
/// stack (Elasticsearch's recursion dies with a `StackOverflowError` there).
pub fn ensure_no_self_references(v: &Value, hint: Option<&str>) -> Result<(), XContentError> {
    enum Step {
        Enter(Value),
        Leave(usize),
    }
    // the common case, a flat document of scalars, allocates nothing beyond the root's children
    let mut ancestors: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut stack = vec![Step::Enter(v.clone())];
    while let Some(step) = stack.pop() {
        let v = match step {
            Step::Leave(id) => {
                ancestors.remove(&id);
                continue;
            }
            Step::Enter(v) => v,
        };
        let children: Vec<Value> = match &v {
            Value::List(l) => {
                l.sync_view();
                l.v.borrow().clone()
            }
            Value::Set(s) => s.keys(),
            Value::Map(m) if m.is_empty() => continue,
            Value::Map(m) => m.entries().into_iter().flat_map(|(k, x)| [k, x]).collect(),
            Value::Obj(o) => match &**o {
                Obj::Array(a) => match &*a.data.borrow() {
                    ArrayData::Ref(x) => x.clone(),
                    _ => continue,
                },
                Obj::MapView { map, kind: ViewKind::Keys } => map.keys(),
                Obj::MapView { map, kind: ViewKind::Values } => map.values(),
                // the entries themselves are neither maps nor iterables
                Obj::MapView { kind: ViewKind::Entries, .. } => Vec::new(),
                Obj::Es(crate::es::EsObj::Ctx(c)) => {
                    let e = c.entries();
                    if e.is_empty() {
                        continue;
                    }
                    e.into_iter().flat_map(|(k, x)| [k, x]).collect()
                }
                Obj::Es(crate::es::EsObj::Values(sdv)) => sdv.vals.to_vec(),
                _ => continue,
            },
            _ => continue,
        };
        let Some(id) = identity(&v) else { continue };
        if !ancestors.insert(id) {
            let reason = match hint {
                Some(h) if !h.is_empty() => format!("Iterable object is self-referencing itself ({h})"),
                _ => "Iterable object is self-referencing itself".to_string(),
            };
            return Err(XContentError { kind: "illegal_argument_exception", reason });
        }
        stack.push(Step::Leave(id));
        stack.extend(children.into_iter().rev().filter(|c| matches!(c, Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Obj(_))).map(Step::Enter));
    }
    Ok(())
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

/// A container being written: its remaining children (taken one by one).
enum Open {
    Array { items: Vec<Value>, next: usize },
    Object { entries: Vec<(Value, Value)>, next: usize },
}

struct Writer {
    out: String,
    /// Identities of the open containers, for the self-reference check (one per `open` frame).
    path: Vec<usize>,
    open: Vec<Open>,
}

impl Writer {
    /// Opens a container: Jackson's nesting limit, then the self-reference check.
    fn enter(&mut self, v: &Value, frame: Open) -> Result<(), XContentError> {
        if self.path.len() >= crate::value::MAX_JSON_DEPTH {
            return Err(XContentError { kind: "stream_constraints_exception", reason: crate::value::nesting_depth_message(self.path.len() + 1) });
        }
        // a value's identity; primitive arrays converted on the fly have none to repeat
        let id = identity(v).unwrap_or(0);
        if id != 0 && self.path.contains(&id) {
            return Err(XContentError { kind: "illegal_argument_exception", reason: "Iterable object is self-referencing itself".into() });
        }
        self.out.push(if matches!(frame, Open::Array { .. }) { '[' } else { '{' });
        self.path.push(id);
        self.open.push(frame);
        Ok(())
    }

    fn close(&mut self) {
        self.path.pop();
        self.open.pop();
    }

    fn array(&mut self, v: &Value, items: Vec<Value>) -> Result<(), XContentError> {
        self.enter(v, Open::Array { items, next: 0 })
    }

    /// Writes a scalar, or opens a container whose children the caller's loop writes.
    fn value(&mut self, v: &Value) -> Result<(), XContentError> {
        let out = &mut self.out;
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
                l.sync_view();
                let items = l.v.borrow().clone();
                self.array(v, items)?;
            }
            Value::Set(s) => self.array(v, s.keys())?,
            Value::Map(m) => self.enter(v, Open::Object { entries: m.entries(), next: 0 })?,
            Value::Obj(o) => match &**o {
                Obj::Array(a) => {
                    let items: Vec<Value> = match &*a.data.borrow() {
                        ArrayData::Bool(x) => x.iter().map(|&b| Value::Bool(b)).collect(),
                        // `byte[]` is binary: Jackson writes it base64-encoded
                        ArrayData::Byte(x) => {
                            use base64::Engine;
                            let bytes: Vec<u8> = x.iter().map(|&b| b as u8).collect();
                            escape_into(&base64::engine::general_purpose::STANDARD.encode(bytes), out);
                            return Ok(());
                        }
                        ArrayData::Short(x) => x.iter().map(|&b| Value::Short(b)).collect(),
                        ArrayData::Char(_) => return Err(unknown("[C")),
                        ArrayData::Int(x) => x.iter().map(|&b| Value::Int(b)).collect(),
                        ArrayData::Long(x) => x.iter().map(|&b| Value::Long(b)).collect(),
                        ArrayData::Float(x) => x.iter().map(|&b| Value::Float(b)).collect(),
                        ArrayData::Double(x) => x.iter().map(|&b| Value::Double(b)).collect(),
                        ArrayData::Ref(x) => x.clone(),
                    };
                    self.array(v, items)?;
                }
                Obj::MapView { map, kind } => match kind {
                    ViewKind::Keys => self.array(v, map.keys())?,
                    ViewKind::Values => self.array(v, map.values())?,
                    // an entry set is iterable, but its entries are not writable
                    ViewKind::Entries => {
                        if !map.is_empty() {
                            return Err(unknown(entry_class(map.order())));
                        }
                        self.array(v, Vec::new())?;
                    }
                },
                Obj::BigInt(b) => out.push_str(&b.to_string()),
                Obj::BigDec(d) => out.push_str(&d.to_java_string()),
                Obj::Time(t) => time(t, out)?,
                Obj::Es(crate::es::EsObj::Ctx(c)) => self.enter(v, Open::Object { entries: c.entries(), next: 0 })?,
                // `ScriptDocValues` are lists
                Obj::Es(crate::es::EsObj::Values(sdv)) => self.array(v, sdv.vals.to_vec())?,
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
}

/// A map key: Elasticsearch casts it to `String`.
fn field_name(k: &Value, out: &mut String) -> Result<(), XContentError> {
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

    fn nested(depth: usize) -> Value {
        let mut v = crate::value::JList::array_list(vec![Value::Int(1)]);
        for _ in 1..depth {
            v = crate::value::JList::array_list(vec![v]);
        }
        v
    }

    /// Neither walk recurses on the caller's stack: deep values are fine on a small thread.
    #[test]
    fn deep_values_use_no_native_stack() {
        std::thread::Builder::new()
            // room for the (bounded) recursive drop of nested values, not for the walks
            .stack_size(512 * 1024)
            .spawn(|| {
                let ok = to_xcontent(&nested(1000)).unwrap();
                assert_eq!(ok.len(), 2 * 1000 + 1);
                let e = to_xcontent(&nested(1001)).unwrap_err();
                assert_eq!((e.kind, e.reason.as_str()), ("stream_constraints_exception", crate::value::nesting_depth_message(1001).as_str()));
                let deep = nested(300_000);
                assert_eq!(check_ingest_value(&deep), Ok(()));
                assert_eq!(to_xcontent(&deep).unwrap_err().kind, "stream_constraints_exception");
                // a cycle at the bottom of a deep value
                let mut v = crate::value::JList::array_list(vec![]);
                let bottom = v.clone();
                for _ in 0..100_000 {
                    v = crate::value::JList::array_list(vec![v]);
                }
                if let Value::List(b) = &bottom {
                    b.v.borrow_mut().push(v.clone());
                }
                let e = check_ingest_value(&v).unwrap_err();
                assert_eq!(e.reason, "Iterable object is self-referencing itself (source document)");
                if let Value::List(b) = &bottom {
                    b.v.borrow_mut().clear();
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    /// Only ancestors count: a container reached twice along different paths is no cycle.
    #[test]
    fn shared_children_are_not_self_references() {
        let shared = crate::value::JList::array_list(vec![Value::Int(1)]);
        let v = crate::value::JList::array_list(vec![shared.clone(), shared.clone(), crate::value::JList::array_list(vec![shared])]);
        assert_eq!(ensure_no_self_references(&v, None), Ok(()));
        assert_eq!(to_xcontent(&v).unwrap(), "[[1],[1],[[1]]]");
    }
}
