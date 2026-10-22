//! `java.lang`: `Object`, `String`, the boxes, `Math`, `StringBuilder`, exceptions.
//!
//! Strings index by UTF-16 code unit like Java's; numbers print and parse with Java's rules
//! (see [`crate::java`]); `Math` uses fdlibm (the `libm` crate), which is what `StrictMath`
//! specifies, so results are the same on every platform.

use std::cell::RefCell;
use std::rc::Rc;

use crate::java;
use crate::lookup::Builder;
use crate::types::{cls, ClassId};
use crate::value::{ArrayData, JArray, JException, JList, JStr, Obj, Value};
use crate::vm::{iae, npe, Throw, Thrown, Vm, VR};
use crate::types::Ty;

// ------------------------------------------------------------------------------ arg helpers

pub fn int(v: &Value) -> i32 {
    match *v {
        Value::Int(i) => i,
        Value::Byte(i) => i as i32,
        Value::Short(i) => i as i32,
        Value::Char(i) => i as i32,
        Value::Long(i) => i as i32,
        Value::Float(f) => f as i32,
        Value::Double(f) => f as i32,
        _ => 0,
    }
}

pub fn long(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

pub fn double(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

pub fn float(v: &Value) -> f32 {
    match *v {
        Value::Float(f) => f,
        _ => v.as_f64().unwrap_or(0.0) as f32,
    }
}

pub fn boolean(v: &Value) -> bool {
    matches!(v, Value::Bool(true))
}

pub fn char16(v: &Value) -> u16 {
    match *v {
        Value::Char(c) => c,
        _ => v.as_i64().unwrap_or(0) as u16,
    }
}

/// A `String` argument; null throws the NPE the JDK method would.
pub fn string<'v>(v: &'v Value, what: &str) -> Result<&'v Rc<JStr>, Thrown> {
    match v {
        Value::Str(s) => Ok(s),
        Value::Null => Err(npe(format!("Cannot invoke \"String.{what}\" because \"str\" is null"))),
        _ => Err(crate::vm::cce("expected a String")),
    }
}

/// A `CharSequence` argument (a string or a builder) as an owned string.
pub fn char_seq(v: &Value) -> Result<String, Thrown> {
    match v {
        Value::Str(s) => Ok(s.as_str().to_string()),
        Value::Obj(o) => match &**o {
            Obj::Builder { s, .. } => Ok(s.borrow().clone()),
            _ => Ok(v.to_java_string()),
        },
        Value::Null => Err(npe("Cannot invoke \"java.lang.CharSequence.toString()\" because \"s\" is null")),
        _ => Ok(v.to_java_string()),
    }
}

fn s(v: &Value) -> &str {
    match v {
        Value::Str(s) => s.as_str(),
        _ => "",
    }
}


fn frames(f: &[&str]) -> Vec<String> {
    f.iter().map(|s| format!("java.base/{s}")).collect()
}

const PRECONDITIONS_RANGE: &[&str] = &[
    "jdk.internal.util.Preconditions$1.apply(Preconditions.java:55)",
    "jdk.internal.util.Preconditions$1.apply(Preconditions.java:52)",
    "jdk.internal.util.Preconditions$4.apply(Preconditions.java:213)",
    "jdk.internal.util.Preconditions$4.apply(Preconditions.java:210)",
    "jdk.internal.util.Preconditions.outOfBounds(Preconditions.java:98)",
    "jdk.internal.util.Preconditions.outOfBoundsCheckFromToIndex(Preconditions.java:112)",
    "jdk.internal.util.Preconditions.checkFromToIndex(Preconditions.java:349)",
    "java.lang.String.checkBoundsBeginEnd(String.java:5312)",
];

fn sioobe_range(begin: i64, end: i64, len: usize, extra: &[&str]) -> Thrown {
    let mut t = Throw::new(cls::STRING_INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Range [{begin}, {end}) out of bounds for length {len}")));
    let mut f: Vec<&str> = PRECONDITIONS_RANGE.to_vec();
    f.extend_from_slice(extra);
    t.frames = frames(&f);
    t
}

/// The frames of `StringBuilder`/`StringBuffer` method `op` above `AbstractStringBuilder`
/// (JDK 27 line numbers; `StringBuffer` overrides every method `synchronized`).
fn builder_frames(class: ClassId, op: &str) -> &'static [&'static str] {
    let buffer = class == cls::STRING_BUFFER;
    match (op, buffer) {
        ("charAt", false) => &["java.lang.String.checkIndex(String.java:5279)", "java.lang.AbstractStringBuilder.charAt(AbstractStringBuilder.java:429)", "java.lang.StringBuilder.charAt(StringBuilder.java:91)"],
        ("charAt", true) => &["java.lang.String.checkIndex(String.java:5279)", "java.lang.AbstractStringBuilder.charAt(AbstractStringBuilder.java:429)", "java.lang.StringBuffer.charAt(StringBuffer.java:243)"],
        ("delete", false) => &["java.lang.AbstractStringBuilder.delete(AbstractStringBuilder.java:1026)", "java.lang.StringBuilder.delete(StringBuilder.java:289)"],
        ("delete", true) => &["java.lang.AbstractStringBuilder.delete(AbstractStringBuilder.java:1026)", "java.lang.StringBuffer.delete(StringBuffer.java:472)"],
        ("deleteCharAt", false) => &["java.lang.String.checkIndex(String.java:5279)", "java.lang.AbstractStringBuilder.deleteCharAt(AbstractStringBuilder.java:1081)", "java.lang.StringBuilder.deleteCharAt(StringBuilder.java:298)"],
        ("deleteCharAt", true) => &["java.lang.String.checkIndex(String.java:5279)", "java.lang.AbstractStringBuilder.deleteCharAt(AbstractStringBuilder.java:1081)", "java.lang.StringBuffer.deleteCharAt(StringBuffer.java:483)"],
        ("insert", false) => &[
            "java.lang.String.checkOffset(String.java:5287)",
            "java.lang.AbstractStringBuilder.insert(AbstractStringBuilder.java:1298)",
            "java.lang.StringBuilder.insert(StringBuilder.java:336)",
            "java.lang.StringBuilder.insert(StringBuilder.java:91)",
            "java.lang.AbstractStringBuilder.insert(AbstractStringBuilder.java:1261)",
            "java.lang.StringBuilder.insert(StringBuilder.java:327)",
        ],
        ("insert", true) => &["java.lang.String.checkOffset(String.java:5287)", "java.lang.AbstractStringBuilder.insert(AbstractStringBuilder.java:1298)", "java.lang.StringBuffer.insert(StringBuffer.java:544)"],
        ("replace", false) => &["java.lang.AbstractStringBuilder.replace(AbstractStringBuilder.java:1113)", "java.lang.StringBuilder.replace(StringBuilder.java:307)"],
        ("replace", true) => &["java.lang.AbstractStringBuilder.replace(AbstractStringBuilder.java:1113)", "java.lang.StringBuffer.replace(StringBuffer.java:494)"],
        ("setLength", false) => &["java.lang.AbstractStringBuilder.setLength(AbstractStringBuilder.java:392)", "java.lang.StringBuilder.setLength(StringBuilder.java:91)"],
        ("setLength", true) => &["java.lang.AbstractStringBuilder.setLength(AbstractStringBuilder.java:392)", "java.lang.StringBuffer.setLength(StringBuffer.java:234)"],
        ("substring", false) => &[
            "java.lang.AbstractStringBuilder.substring(AbstractStringBuilder.java:1189)",
            "java.lang.StringBuilder.substring(StringBuilder.java:91)",
            "java.lang.AbstractStringBuilder.substring(AbstractStringBuilder.java:1141)",
            "java.lang.StringBuilder.substring(StringBuilder.java:91)",
        ],
        ("substring", true) => &["java.lang.AbstractStringBuilder.substring(AbstractStringBuilder.java:1189)", "java.lang.StringBuffer.substring(StringBuffer.java:522)", "java.lang.StringBuffer.substring(StringBuffer.java:504)"],
        ("substring2", false) => &["java.lang.AbstractStringBuilder.substring(AbstractStringBuilder.java:1189)", "java.lang.StringBuilder.substring(StringBuilder.java:91)"],
        ("substring2", true) => &["java.lang.AbstractStringBuilder.substring(AbstractStringBuilder.java:1189)", "java.lang.StringBuffer.substring(StringBuffer.java:522)"],
        _ => &[],
    }
}

/// A range failure of `AbstractStringBuilder` (`checkFromToIndex` without `String`'s own
/// `checkBoundsBeginEnd` frame), with the builder frames after it.
fn sb_range_error(begin: i64, end: i64, len: usize, extra: &[&str]) -> Thrown {
    let mut t = Throw::new(cls::STRING_INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Range [{begin}, {end}) out of bounds for length {len}")));
    let mut f: Vec<&str> = PRECONDITIONS_RANGE[..PRECONDITIONS_RANGE.len() - 1].to_vec();
    f.extend_from_slice(extra);
    t.frames = frames(&f);
    t
}

fn sioobe_index(i: i64, len: usize, extra: &[&str]) -> Thrown {
    let mut t = Throw::new(cls::STRING_INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index {i} out of bounds for length {len}")));
    let mut f: Vec<&str> = vec![
        "jdk.internal.util.Preconditions$1.apply(Preconditions.java:55)",
        "jdk.internal.util.Preconditions$1.apply(Preconditions.java:52)",
        "jdk.internal.util.Preconditions$4.apply(Preconditions.java:213)",
        "jdk.internal.util.Preconditions$4.apply(Preconditions.java:210)",
        "jdk.internal.util.Preconditions.outOfBounds(Preconditions.java:98)",
        "jdk.internal.util.Preconditions.outOfBoundsCheckIndex(Preconditions.java:106)",
        "jdk.internal.util.Preconditions.checkIndex(Preconditions.java:302)",
    ];
    f.extend_from_slice(extra);
    t.frames = frames(&f);
    t
}

/// Number-format failure with the JDK frames of the parser.
pub fn nfe(msg: String, frame: &[&str]) -> Thrown {
    let mut t = Throw::new(cls::NUMBER_FORMAT_EXCEPTION, Some(msg));
    t.frames = frames(frame);
    t
}

// ------------------------------------------------------------------------------ Object

fn obj_equals(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(a[0].java_equals(&a[1])))
}
fn obj_hash(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(a[0].java_hash()))
}
fn obj_to_string(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(a[0].to_java_string()))
}

// ------------------------------------------------------------------------------ CharSequence / String

/// Runs `f` with the receiver's text (a string or a builder).
fn with_cs<R>(v: &Value, f: impl FnOnce(&str) -> R) -> R {
    match v {
        Value::Str(s) => f(s.as_str()),
        Value::Obj(o) => match &**o {
            Obj::Builder { s, .. } => f(&s.borrow()),
            _ => f(&v.to_java_string()),
        },
        _ => f(""),
    }
}

fn cs_char_at(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = int(&a[1]);
    if let Some(r) = crate::es::misc::char_buffer_char_at(&a[0], i) {
        return r;
    }
    with_cs(&a[0], |s| {
        let len = java::utf16_len(s);
        if i < 0 || i as usize >= len {
            if a[0].class() == cls::PACKED_TOKEN_ATTRIBUTE {
                let mut t = Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Index {i} out of bounds for length {len}")));
                t.frames = [
                    "java.base/jdk.internal.util.Preconditions.outOfBounds(Preconditions.java:100)",
                    "java.base/jdk.internal.util.Preconditions.outOfBoundsCheckIndex(Preconditions.java:106)",
                    "java.base/jdk.internal.util.Preconditions.checkIndex(Preconditions.java:302)",
                    "java.base/java.util.Objects.checkIndex(Objects.java:365)",
                    "org.apache.lucene.core@10.5.1/org.apache.lucene.analysis.tokenattributes.CharTermAttributeImpl.charAt(CharTermAttributeImpl.java:105)",
                ]
                .iter()
                .map(|s| s.to_string())
                .collect();
                return Err(t);
            }
            let is_builder = !matches!(a[0], Value::Str(_));
            if is_builder {
                return Err(sioobe_index(i as i64, len, builder_frames(a[0].class(), "charAt")));
            }
            return Err(sioobe_index(
                i as i64,
                len,
                &["java.lang.String.checkIndex(String.java:5279)", "java.lang.StringLatin1.charAt(StringLatin1.java:58)", "java.lang.String.charAt(String.java:1843)"],
            ));
        }
        Ok(Value::Char(java::utf16_at(s, i as usize).unwrap()))
    })
}

fn cs_length(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(match &a[0] {
        Value::Str(s) => s.len16() as i32,
        v => match crate::es::misc::char_buffer_len(v) {
            Some(n) => n,
            None => with_cs(v, |s| java::utf16_len(s) as i32),
        },
    }))
}

fn cs_sub_sequence(vm: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(r) = crate::es::misc::char_buffer_sub_sequence(&a[0], int(&a[1]), int(&a[2])) {
        return r;
    }
    let is_str = matches!(a[0], Value::Str(_));
    let text = with_cs(&a[0], |s| s.to_string());
    let v = Value::str(&text);
    let mut args = [v, a[1].clone(), a[2].clone()];
    str_substring2(vm, &mut args).map_err(|mut e| {
        // `String.subSequence` delegates to `substring`
        if is_str && e.class == cls::STRING_INDEX_OUT_OF_BOUNDS_EXCEPTION {
            e.frames.push("java.base/java.lang.String.subSequence(String.java:3312)".into());
        }
        e
    })
}

fn cs_to_string(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(match &a[0] {
        Value::Str(_) => a[0].clone(),
        v => Value::string(with_cs(v, |s| s.to_string())),
    })
}

fn cs_chars(vm: &mut Vm, a: &mut [Value]) -> VR {
    let items: Vec<Value> = with_cs(&a[0], |s| s.encode_utf16().map(|u| Value::Int(u as i32)).collect());
    crate::stream::int_stream(vm, items)
}

fn cs_code_points(vm: &mut Vm, a: &mut [Value]) -> VR {
    let items: Vec<Value> = with_cs(&a[0], |s| s.chars().map(|c| Value::Int(c as i32)).collect());
    crate::stream::int_stream(vm, items)
}

fn str_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::str(""))
}

fn substring_range(st: &JStr, begin: i64, end: i64, extra: &[&str]) -> VR {
    let len = st.len16();
    if begin < 0 || end > len as i64 || begin > end {
        return Err(sioobe_range(begin, end, len, extra));
    }
    let (b, e) = (st.byte_index(begin as usize).unwrap(), st.byte_index(end as usize).unwrap());
    Ok(Value::str(&st.as_str()[b..e]))
}

fn str_substring1(_: &mut Vm, a: &mut [Value]) -> VR {
    let Value::Str(st) = &a[0] else { return Ok(Value::Null) };
    let begin = int(&a[1]) as i64;
    substring_range(st, begin, st.len16() as i64, &["java.lang.String.substring(String.java:3274)", "java.lang.String.substring(String.java:3247)"])
}

fn str_substring2(_: &mut Vm, a: &mut [Value]) -> VR {
    let Value::Str(st) = &a[0] else { return Ok(Value::Null) };
    substring_range(st, int(&a[1]) as i64, int(&a[2]) as i64, &["java.lang.String.substring(String.java:3274)"])
}

fn str_compare_to(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        return Err(npe("Cannot read field \"value\" because \"anotherString\" is null").with_frames(&["java.base/java.lang.String.compareTo(String.java:2406)"]));
    }
    let o = string(&a[1], "compareTo")?;
    Ok(Value::Int(java::compare_strings(s(&a[0]), o.as_str())))
}

fn fold_case(c: u16) -> u16 {
    java::char_lower(java::char_upper(c))
}

/// `StringBuilder.compareTo(StringBuilder)` (and `StringBuffer`'s): the `String.compareTo`
/// order over the current contents; the argument must be a builder of the receiver's class.
fn sb_compare_to(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        let b = if a[0].class() == crate::types::cls::STRING_BUFFER { "StringBuffer" } else { "StringBuilder" };
        let lines = if b == "StringBuffer" { ("StringBuffer.java:200", "StringBuffer.java:112") } else { ("StringBuilder.java:168", "StringBuilder.java:91") };
        return Err(npe("Cannot read field \"value\" because \"another\" is null").with_frames(&[
            "java.base/java.lang.AbstractStringBuilder.compareTo(AbstractStringBuilder.java:178)",
            &format!("java.base/java.lang.{b}.compareTo({})", lines.0),
            &format!("java.base/java.lang.{b}.compareTo({})", lines.1),
        ]));
    }
    let (x, y) = match (&a[0], &a[1]) {
        (Value::Obj(p), Value::Obj(q)) => match (&**p, &**q) {
            (Obj::Builder { s: x, .. }, Obj::Builder { s: y, .. }) if a[0].class() == a[1].class() => (x.borrow().clone(), y.borrow().clone()),
            _ => return Err(sb_compare_cce(a)),
        },
        _ => return Err(sb_compare_cce(a)),
    };
    Ok(Value::Int(java::compare_strings(&x, &y)))
}

/// The bridge `compareTo(Object)`'s checkcast failure.
fn sb_compare_cce(a: &[Value]) -> Thrown {
    let t = crate::vm::cce(crate::cast::checkcast_message(crate::lookup::java_binary_name(a[1].class()), crate::lookup::java_binary_name(a[0].class())));
    if a[0].class() == cls::STRING_BUILDER {
        t.with_frames(&["java.base/java.lang.StringBuilder.compareTo(StringBuilder.java:91)"])
    } else {
        t.with_frames(&["java.base/java.lang.StringBuffer.compareTo(StringBuffer.java:112)"])
    }
}

fn str_compare_ignore_case(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        return Err(npe("Cannot read field \"value\" because \"s2\" is null").with_frames(&["java.base/java.lang.String$CaseInsensitiveComparator.compare(String.java:2442)", "java.base/java.lang.String$CaseInsensitiveComparator.compare(String.java:2434)", "java.base/java.lang.String.compareToIgnoreCase(String.java:2479)"]));
    }
    let o = string(&a[1], "compareToIgnoreCase")?.clone();
    let (x, y): (Vec<u16>, Vec<u16>) = (s(&a[0]).encode_utf16().collect(), o.as_str().encode_utf16().collect());
    for i in 0..x.len().min(y.len()) {
        let (c1, c2) = (x[i], y[i]);
        if c1 != c2 {
            let (u1, u2) = (java::char_upper(c1), java::char_upper(c2));
            if u1 != u2 {
                let (l1, l2) = (java::char_lower(u1), java::char_lower(u2));
                if l1 != l2 {
                    return Ok(Value::Int(l1 as i32 - l2 as i32));
                }
            }
        }
    }
    Ok(Value::Int(x.len() as i32 - y.len() as i32))
}

fn str_concat(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        return Err(npe("Cannot invoke \"String.isEmpty()\" because \"str\" is null").with_frames(&["java.base/java.lang.String.concat(String.java:3336)"]));
    }
    let o = string(&a[1], "isEmpty()")?;
    Ok(Value::string(format!("{}{}", s(&a[0]), o.as_str())))
}

fn str_contains(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        return Err(npe("Cannot invoke \"java.lang.CharSequence.toString()\" because \"s\" is null").with_frames(&["java.base/java.lang.String.contains(String.java:3421)"]));
    }
    let needle = match &a[1] {
        Value::Null => return Err(npe("Cannot invoke \"java.lang.CharSequence.toString()\" because \"s\" is null")),
        v => char_seq(v)?,
    };
    Ok(Value::Bool(s(&a[0]).contains(needle.as_str())))
}

fn str_content_equals(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        return Err(npe("Cannot invoke \"java.lang.CharSequence.length()\" because \"cs\" is null").with_frames(&["java.base/java.lang.String.contentEquals(String.java:2253)"]));
    }
    let o = char_seq(&a[1])?;
    Ok(Value::Bool(s(&a[0]) == o))
}

fn str_ends_with(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        return Err(npe("Cannot invoke \"String.length()\" because \"suffix\" is null").with_frames(&["java.base/java.lang.String.endsWith(String.java:2780)"]));
    }
    let o = string(&a[1], "length()")?;
    Ok(Value::Bool(s(&a[0]).ends_with(o.as_str())))
}

fn str_starts_with(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        return Err(npe("Cannot invoke \"String.length()\" because \"prefix\" is null").with_frames(&["java.base/java.lang.String.startsWith(String.java:2723)", "java.base/java.lang.String.startsWith(String.java:2765)"]));
    }
    let o = string(&a[1], "length()")?;
    Ok(Value::Bool(s(&a[0]).starts_with(o.as_str())))
}

fn str_starts_with2(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        return Err(npe("Cannot invoke \"String.length()\" because \"prefix\" is null").with_frames(&["java.base/java.lang.String.startsWith(String.java:2723)"]));
    }
    let o = string(&a[1], "length()")?.clone();
    let off = int(&a[2]);
    let Value::Str(st) = &a[0] else { return Ok(Value::Bool(false)) };
    if off < 0 || off as usize > st.len16() {
        return Ok(Value::Bool(false));
    }
    let b = st.byte_index(off as usize).unwrap();
    Ok(Value::Bool(st.as_str()[b..].starts_with(o.as_str())))
}

fn str_equals_ignore_case(_: &mut Vm, a: &mut [Value]) -> VR {
    let Value::Str(o) = &a[1] else { return Ok(Value::Bool(false)) };
    let (x, y): (Vec<u16>, Vec<u16>) = (s(&a[0]).encode_utf16().collect(), o.as_str().encode_utf16().collect());
    if x.len() != y.len() {
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(x.iter().zip(y.iter()).all(|(c1, c2)| c1 == c2 || fold_case(*c1) == fold_case(*c2))))
}

fn index_of_impl(hay: &JStr, needle: &str, from: i64) -> i32 {
    let len = hay.len16() as i64;
    let from = from.max(0);
    if from > len {
        return if needle.is_empty() { len as i32 } else { -1 };
    }
    let b = hay.byte_index(from as usize).unwrap();
    match hay.as_str()[b..].find(needle) {
        Some(i) => java::byte_to_utf16(hay.as_str(), b + i) as i32,
        None => -1,
    }
}

fn last_index_of_impl(hay: &JStr, needle: &str, from: i64) -> i32 {
    let len = hay.len16() as i64;
    let nlen = java::utf16_len(needle) as i64;
    let start = from.min(len - nlen);
    if start < 0 {
        return -1;
    }
    // search the prefix that can contain a match starting at or before `start`
    let end_u16 = (start + nlen) as usize;
    let end_b = hay.byte_index(end_u16).unwrap_or(hay.as_str().len());
    match hay.as_str()[..end_b].rfind(needle) {
        Some(i) => java::byte_to_utf16(hay.as_str(), i) as i32,
        None => -1,
    }
}

fn str_index_of(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        return Err(npe("Cannot invoke \"String.coder()\" because \"str\" is null").with_frames(&["java.base/java.lang.String.indexOf(String.java:3033)"]));
    }
    let n = string(&a[1], "indexOf")?.clone();
    let Value::Str(h) = &a[0] else { return Ok(Value::Int(-1)) };
    Ok(Value::Int(index_of_impl(h, n.as_str(), 0)))
}

fn str_index_of2(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        return Err(npe("Cannot invoke \"String.length()\" because \"tgtStr\" is null").with_frames(&["java.base/java.lang.String.indexOf(String.java:3124)", "java.base/java.lang.String.indexOf(String.java:3074)"]));
    }
    let n = string(&a[1], "indexOf")?.clone();
    let Value::Str(h) = &a[0] else { return Ok(Value::Int(-1)) };
    Ok(Value::Int(index_of_impl(h, n.as_str(), int(&a[2]) as i64)))
}

fn str_last_index_of(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        return Err(npe("Cannot read field \"value\" because \"tgtStr\" is null").with_frames(&["java.base/java.lang.String.lastIndexOf(String.java:3199)", "java.base/java.lang.String.lastIndexOf(String.java:3183)", "java.base/java.lang.String.lastIndexOf(String.java:3162)"]));
    }
    let n = string(&a[1], "lastIndexOf")?.clone();
    let Value::Str(h) = &a[0] else { return Ok(Value::Int(-1)) };
    Ok(Value::Int(last_index_of_impl(h, n.as_str(), h.len16() as i64)))
}

fn str_last_index_of2(_: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        return Err(npe("Cannot read field \"value\" because \"tgtStr\" is null").with_frames(&["java.base/java.lang.String.lastIndexOf(String.java:3199)", "java.base/java.lang.String.lastIndexOf(String.java:3183)"]));
    }
    let n = string(&a[1], "lastIndexOf")?.clone();
    let Value::Str(h) = &a[0] else { return Ok(Value::Int(-1)) };
    Ok(Value::Int(last_index_of_impl(h, n.as_str(), int(&a[2]) as i64)))
}

fn str_is_empty(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(s(&a[0]).is_empty()))
}

fn str_join(vm: &mut Vm, a: &mut [Value]) -> VR {
    for (k, line) in [(0usize, 4010), (1, 4011)] {
        if a[k].is_null() {
            let mut t = npe("");
            t.message = None;
            return Err(t.with_frames(&["java.base/java.util.Objects.requireNonNull(Objects.java:220)", &format!("java.base/java.lang.String.join(String.java:{line})")]));
        }
    }
    let sep = char_seq(&a[0])?;
    let items = crate::lib_coll::elements(vm, &a[1])?;
    let mut out = String::new();
    for (i, x) in items.iter().enumerate() {
        if i > 0 {
            out.push_str(&sep);
        }
        x.write_java_string(&mut out, 0);
        crate::guard::check_string(out.len())?;
    }
    Ok(Value::string(out))
}

fn str_replace(_: &mut Vm, a: &mut [Value]) -> VR {
    if !a[1].is_null() && a[2].is_null() {
        return Err(npe("Cannot invoke \"java.lang.CharSequence.toString()\" because \"replacement\" is null").with_frames(&["java.base/java.lang.String.replace(String.java:3526)"]));
    }
    if a[1].is_null() {
        return Err(npe("Cannot invoke \"java.lang.CharSequence.toString()\" because \"target\" is null").with_frames(&["java.base/java.lang.String.replace(String.java:3525)"]));
    }
    let target = match &a[1] {
        Value::Null => return Err(npe("Cannot invoke \"java.lang.CharSequence.toString()\" because \"target\" is null")),
        v => char_seq(v)?,
    };
    let repl = match &a[2] {
        Value::Null => return Err(npe("Cannot invoke \"java.lang.CharSequence.toString()\" because \"replacement\" is null")),
        v => char_seq(v)?,
    };
    let src = s(&a[0]);
    if target.is_empty() {
        // Java inserts the replacement between every char and at both ends
        crate::guard::check_string(src.len().saturating_mul(repl.len() + 1).saturating_add(repl.len()))?;
        let mut out = String::with_capacity(src.len() * (repl.len() + 1) + repl.len());
        out.push_str(&repl);
        for c in src.chars() {
            out.push(c);
            out.push_str(&repl);
        }
        return Ok(Value::string(out));
    }
    if repl.len() > target.len() {
        let bound = src.len().saturating_add((src.len() / target.len()).saturating_mul(repl.len() - target.len()));
        if bound > crate::guard::max_object_bytes() {
            let n = src.matches(target.as_str()).count();
            crate::guard::check_string(src.len().saturating_add(n.saturating_mul(repl.len() - target.len())))?;
        }
    }
    Ok(Value::string(src.replace(target.as_str(), &repl)))
}

fn str_to_char_array(_: &mut Vm, a: &mut [Value]) -> VR {
    let v: Vec<u16> = s(&a[0]).encode_utf16().collect();
    Ok(Value::obj(Obj::Array(JArray::from_data(Ty::CHAR, ArrayData::Char(v)))))
}

/// `String.toLowerCase()` in the ROOT locale (Java's default locale in Elasticsearch).
pub fn java_lower(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_lowercase();
    }
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == '\u{03A3}' {
            // final sigma when preceded by a letter and not followed by one
            let prev = i > 0 && chars[i - 1].is_alphabetic();
            let next = chars.get(i + 1).is_some_and(|n| n.is_alphabetic());
            out.push(if prev && !next { 'ς' } else { 'σ' });
        } else if c == 'İ' {
            out.push_str("i\u{0307}");
        } else {
            out.extend(c.to_lowercase());
        }
    }
    out
}

pub fn java_upper(s: &str) -> String {
    if s.is_ascii() {
        return s.to_ascii_uppercase();
    }
    s.chars().flat_map(|c| c.to_uppercase()).collect()
}

fn str_lower(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(java_lower(s(&a[0]))))
}

fn str_lower_locale(_: &mut Vm, a: &mut [Value]) -> VR {
    let lang = locale_lang(&a[1]);
    let src = s(&a[0]);
    if lang == "tr" || lang == "az" {
        let mut out = String::new();
        for c in src.chars() {
            match c {
                'I' => out.push('ı'),
                'İ' => out.push('i'),
                c => out.extend(c.to_lowercase()),
            }
        }
        return Ok(Value::string(out));
    }
    Ok(Value::string(java_lower(src)))
}

fn str_upper(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(java_upper(s(&a[0]))))
}

fn str_upper_locale(_: &mut Vm, a: &mut [Value]) -> VR {
    let lang = locale_lang(&a[1]);
    let src = s(&a[0]);
    if lang == "tr" || lang == "az" {
        let mut out = String::new();
        for c in src.chars() {
            match c {
                'i' => out.push('İ'),
                'ı' => out.push('I'),
                c => out.extend(c.to_uppercase()),
            }
        }
        return Ok(Value::string(out));
    }
    Ok(Value::string(java_upper(src)))
}

fn locale_lang(v: &Value) -> String {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Locale(l) => l.split(['_', '-']).next().unwrap_or("").to_ascii_lowercase(),
            _ => String::new(),
        },
        _ => String::new(),
    }
}

fn str_trim(_: &mut Vm, a: &mut [Value]) -> VR {
    let src = s(&a[0]);
    let t = java::java_trim(src);
    if t.len() == src.len() {
        return Ok(a[0].clone());
    }
    Ok(Value::str(t))
}

fn str_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(match &a[0] {
        Value::Obj(o) => match &**o {
            Obj::Array(arr) if arr.elem == Ty::CHAR => match &*arr.data.borrow() {
                ArrayData::Char(c) => Value::string(String::from_utf16_lossy(c)),
                _ => Value::string(a[0].to_java_string()),
            },
            _ => Value::string(a[0].to_java_string()),
        },
        v => Value::string(v.to_java_string()),
    })
}

fn str_code_point_at(_: &mut Vm, a: &mut [Value]) -> VR {
    let units: Vec<u16> = s(&a[0]).encode_utf16().collect();
    let i = int(&a[1]);
    if i < 0 || i as usize >= units.len() {
        let extra: &[&str] = if matches!(a[0], Value::Str(_)) { &["java.lang.String.checkIndex(String.java:5279)", "java.lang.String.codePointAt(String.java:1873)"] } else { &[] };
        return Err(sioobe_index(i as i64, units.len(), extra));
    }
    let i = i as usize;
    let c = units[i];
    if (0xD800..0xDC00).contains(&c) && i + 1 < units.len() && (0xDC00..0xE000).contains(&units[i + 1]) {
        let cp = 0x10000 + (((c as u32) - 0xD800) << 10) + ((units[i + 1] as u32) - 0xDC00);
        return Ok(Value::Int(cp as i32));
    }
    Ok(Value::Int(c as i32))
}

fn str_code_point_before(_: &mut Vm, a: &mut [Value]) -> VR {
    let units: Vec<u16> = s(&a[0]).encode_utf16().collect();
    let i = int(&a[1]);
    if i < 1 || i as usize > units.len() {
        return Err(sioobe_index(i as i64, units.len(), &[]));
    }
    let i = i as usize - 1;
    let c = units[i];
    if (0xDC00..0xE000).contains(&c) && i > 0 && (0xD800..0xDC00).contains(&units[i - 1]) {
        let cp = 0x10000 + (((units[i - 1] as u32) - 0xD800) << 10) + ((c as u32) - 0xDC00);
        return Ok(Value::Int(cp as i32));
    }
    Ok(Value::Int(c as i32))
}

fn str_code_point_count(_: &mut Vm, a: &mut [Value]) -> VR {
    let units: Vec<u16> = s(&a[0]).encode_utf16().collect();
    let (b, e) = (int(&a[1]), int(&a[2]));
    if b < 0 || e as usize > units.len() || b > e {
        return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Range [{b}, {e}) out of bounds for length {}", units.len()))));
    }
    let n = String::from_utf16_lossy(&units[b as usize..e as usize]).chars().count();
    Ok(Value::Int(n as i32))
}

fn str_offset_by_code_points(_: &mut Vm, a: &mut [Value]) -> VR {
    let units: Vec<u16> = s(&a[0]).encode_utf16().collect();
    let (mut i, n) = (int(&a[1]) as i64, int(&a[2]) as i64);
    if i < 0 || i as usize > units.len() {
        return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, None));
    }
    let is_hi = |u: u16| (0xD800..0xDC00).contains(&u);
    let is_lo = |u: u16| (0xDC00..0xE000).contains(&u);
    if n >= 0 {
        for _ in 0..n {
            if i as usize >= units.len() {
                return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, None));
            }
            if is_hi(units[i as usize]) && (i as usize + 1) < units.len() && is_lo(units[i as usize + 1]) {
                i += 2;
            } else {
                i += 1;
            }
        }
    } else {
        for _ in 0..(-n) {
            if i <= 0 {
                return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, None));
            }
            i -= 1;
            if is_lo(units[i as usize]) && i > 0 && is_hi(units[i as usize - 1]) {
                i -= 1;
            }
        }
    }
    Ok(Value::Int(i as i32))
}

fn str_region_matches(vm: &mut Vm, a: &mut [Value]) -> VR {
    // regionMatches(boolean ignoreCase, int toffset, String other, int ooffset, int len)
    let ic = boolean(&a[1]);
    let mut args = [a[0].clone(), a[2].clone(), a[3].clone(), a[4].clone(), a[5].clone()];
    region(vm, &mut args, ic)
}

fn str_region_matches4(vm: &mut Vm, a: &mut [Value]) -> VR {
    region(vm, a, false)
}

fn region(_: &mut Vm, a: &mut [Value], ic: bool) -> VR {
    if a[2].is_null() {
        let line = if ic { 2688 } else { 2594 };
        return Err(npe("Cannot invoke \"String.length()\" because \"other\" is null").with_frames(&[&format!("java.base/java.lang.String.regionMatches(String.java:{line})")]));
    }
    let x: Vec<u16> = s(&a[0]).encode_utf16().collect();
    let o = string(&a[2], "length()")?.clone();
    let y: Vec<u16> = o.as_str().encode_utf16().collect();
    let (to, oo, len) = (int(&a[1]) as i64, int(&a[3]) as i64, int(&a[4]) as i64);
    if to < 0 || oo < 0 || to > x.len() as i64 - len || oo > y.len() as i64 - len {
        return Ok(Value::Bool(false));
    }
    for k in 0..len.max(0) as usize {
        let (c1, c2) = (x[to as usize + k], y[oo as usize + k]);
        if c1 == c2 {
            continue;
        }
        if ic && (java::char_upper(c1) == java::char_upper(c2) || fold_case(c1) == fold_case(c2)) {
            continue;
        }
        return Ok(Value::Bool(false));
    }
    Ok(Value::Bool(true))
}

fn str_copy_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::Obj(o) => match &**o {
            Obj::Array(arr) => match &*arr.data.borrow() {
                ArrayData::Char(c) => Ok(Value::string(String::from_utf16_lossy(c))),
                _ => Ok(Value::str("")),
            },
            _ => Ok(Value::str("")),
        },
        _ => Err(npe("Cannot read the array length because \"value\" is null")),
    }
}

fn str_copy_value_of3(_: &mut Vm, a: &mut [Value]) -> VR {
    let (off, count) = (int(&a[1]), int(&a[2]));
    match &a[0] {
        Value::Obj(o) => match &**o {
            Obj::Array(arr) => match &*arr.data.borrow() {
                ArrayData::Char(c) => {
                    if off < 0 || count < 0 || (off as usize + count as usize) > c.len() {
                        return Err(sioobe_range(off as i64, off as i64 + count as i64, c.len(), &[]));
                    }
                    Ok(Value::string(String::from_utf16_lossy(&c[off as usize..(off + count) as usize])))
                }
                _ => Ok(Value::str("")),
            },
            _ => Ok(Value::str("")),
        },
        _ => Err(npe("Cannot read the array length because \"value\" is null")),
    }
}

fn str_get_chars(_: &mut Vm, a: &mut [Value]) -> VR {
    let src: Vec<u16> = with_cs(&a[0], |s| s.encode_utf16().collect());
    let (b, e, db) = (int(&a[1]), int(&a[3 - 1]), int(&a[4]));
    let Value::Obj(o) = &a[3] else { return Err(npe("dst is null")) };
    let Obj::Array(arr) = &**o else { return Ok(Value::Null) };
    if b < 0 || e as usize > src.len() || b > e {
        return Err(sioobe_range(b as i64, e as i64, src.len(), &[]));
    }
    let mut d = arr.data.borrow_mut();
    let ArrayData::Char(dst) = &mut *d else { return Ok(Value::Null) };
    let n = (e - b) as usize;
    if db < 0 || db as usize + n > dst.len() {
        return Err(Throw::new(cls::ARRAY_INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("Range [{db}, {}) out of bounds for length {}", db as usize + n, dst.len()))));
    }
    dst[db as usize..db as usize + n].copy_from_slice(&src[b as usize..e as usize]);
    Ok(Value::Null)
}

fn str_format(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = string(&a[0], "length()")?.clone();
    let args = crate::lib_coll::array_values(&a[1]);
    Ok(Value::string(crate::format::format(vm, f.as_str(), &args)?))
}

fn str_format_locale(vm: &mut Vm, a: &mut [Value]) -> VR {
    let f = string(&a[1], "length()")?.clone();
    let args = crate::lib_coll::array_values(&a[2]);
    let locale = match &a[0] {
        Value::Obj(o) => match &**o {
            Obj::Locale(l) => Some(l.to_string()),
            _ => None,
        },
        _ => None,
    };
    Ok(Value::string(crate::format::format_locale(vm, Some(locale.as_deref().unwrap_or("")), f.as_str(), &args)?))
}

fn str_encode_base64(_: &mut Vm, a: &mut [Value]) -> VR {
    use base64::Engine;
    Ok(Value::string(base64::engine::general_purpose::STANDARD.encode(s(&a[0]).as_bytes())))
}

fn str_decode_base64(_: &mut Vm, a: &mut [Value]) -> VR {
    use base64::Engine;
    match base64::engine::general_purpose::STANDARD.decode(s(&a[0]).as_bytes()) {
        Ok(bytes) => Ok(Value::string(String::from_utf8_lossy(&bytes).into_owned())),
        Err(e) => Err(iae(base64_error(&e))),
    }
}

pub fn base64_error(e: &base64::DecodeError) -> String {
    match e {
        base64::DecodeError::InvalidByte(i, b) => format!("Illegal base64 character {:x}", b).replace(&format!("{i}"), &format!("{i}")),
        base64::DecodeError::InvalidLength(_) => "Input byte array has wrong 4-byte ending unit".into(),
        base64::DecodeError::InvalidLastSymbol(_, _) => "Last unit does not have enough valid bits".into(),
        base64::DecodeError::InvalidPadding => "Input byte array has incorrect ending byte".into(),
    }
}

/// `Augmentation.splitOnToken`: splits on a literal token up to `limit` pieces (`limit < 1`
/// means no limit); empty pieces are kept.
fn split_on_token(src: &str, token: &str, mut limit: i32) -> Result<Vec<String>, Thrown> {
    if src.is_empty() || token.is_empty() || java::utf16_len(src) < java::utf16_len(token) {
        return Ok(vec![src.to_string()]);
    }
    let mut out: Vec<String> = Vec::new();
    let mut pos = 0usize;
    while limit != 1 {
        match src[pos..].find(token) {
            Some(i) => {
                out.push(src[pos..pos + i].to_string());
                pos += i + token.len();
            }
            None => break,
        }
        limit = limit.wrapping_sub(1);
    }
    out.push(src[pos..].to_string());
    Ok(out)
}

fn string_array(items: Vec<String>) -> Value {
    let v: Vec<Value> = items.into_iter().map(Value::string).collect();
    Value::obj(Obj::Array(JArray::from_data(Ty::STRING, ArrayData::Ref(v))))
}

fn str_split_on_token(_: &mut Vm, a: &mut [Value]) -> VR {
    let t = string(&a[1], "length()")?.clone();
    Ok(string_array(split_on_token(s(&a[0]), t.as_str(), -1)?))
}

fn str_split_on_token2(_: &mut Vm, a: &mut [Value]) -> VR {
    let t = string(&a[1], "length()")?.clone();
    Ok(string_array(split_on_token(s(&a[0]), t.as_str(), int(&a[2]))?))
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

fn str_sha1(_: &mut Vm, a: &mut [Value]) -> VR {
    use sha1::Digest;
    Ok(Value::string(hex(&sha1::Sha1::digest(s(&a[0]).as_bytes()))))
}
fn str_sha256(_: &mut Vm, a: &mut [Value]) -> VR {
    use sha2::Digest;
    Ok(Value::string(hex(&sha2::Sha256::digest(s(&a[0]).as_bytes()))))
}
fn str_sha512(_: &mut Vm, a: &mut [Value]) -> VR {
    use sha2::Digest;
    Ok(Value::string(hex(&sha2::Sha512::digest(s(&a[0]).as_bytes()))))
}

// ------------------------------------------------------------------------------ boxes

/// `Number.xValue()`: the primitive conversion of a box, or of another `Number`'s long (or
/// double) value.
fn number_value(v: &Value, p: crate::ir::Prim) -> Value {
    use crate::ir::Prim;
    match v.other_number() {
        Some((d, _)) if matches!(p, Prim::Float | Prim::Double) => crate::cast::convert(&Value::Double(d), p),
        Some((_, l)) => crate::cast::convert(&Value::Long(l), p),
        None => crate::cast::convert(v, p),
    }
}
fn num_byte(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(number_value(&a[0], crate::ir::Prim::Byte))
}
fn num_short(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(number_value(&a[0], crate::ir::Prim::Short))
}
fn num_int(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(number_value(&a[0], crate::ir::Prim::Int))
}
fn num_long(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(number_value(&a[0], crate::ir::Prim::Long))
}
fn num_float(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(number_value(&a[0], crate::ir::Prim::Float))
}
fn num_double(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(number_value(&a[0], crate::ir::Prim::Double))
}

fn identity(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(std::mem::take(&mut a[0]))
}

fn compare_values(_: &mut Vm, a: &mut [Value]) -> VR {
    if let Some(r) = crate::es::misc::buffer_compare(&a[0], &a[1]) {
        return r;
    }
    let o = crate::func::natural_compare(&a[0], &a[1]);
    match o {
        Ok(o) => Ok(Value::Int(o as i32)),
        Err(e) => Err(e),
    }
}

fn compare_to(vm: &mut Vm, a: &mut [Value]) -> VR {
    if a[1].is_null() {
        // the parameter names and lines of JDK 27's `compareTo` methods
        let (name, frame) = match &a[0] {
            Value::Int(_) => ("anotherInteger", "Integer.compareTo(Integer.java:1377)"),
            Value::Long(_) => ("anotherLong", "Long.compareTo(Long.java:1360)"),
            Value::Double(_) => ("anotherDouble", "Double.compareTo(Double.java:1487)"),
            Value::Float(_) => ("anotherFloat", "Float.compareTo(Float.java:1273)"),
            Value::Short(_) => ("anotherShort", "Short.compareTo(Short.java:505)"),
            Value::Byte(_) => ("anotherByte", "Byte.compareTo(Byte.java:499)"),
            Value::Char(_) => ("anotherCharacter", "Character.compareTo(Character.java:12209)"),
            Value::Bool(_) => ("b", "Boolean.compareTo(Boolean.java:290)"),
            _ => ("another", ""),
        };
        let t = npe(format!("Cannot read field \"value\" because \"{name}\" is null"));
        return Err(if frame.is_empty() { t } else { t.with_frames(&[&format!("java.base/java.lang.{frame}")]) });
    }
    // Integer.compare / Double.compare semantics
    match (&a[0], &a[1]) {
        (Value::Double(x), Value::Double(y)) => Ok(Value::Int(crate::func::java_double_cmp(*x, *y) as i32)),
        (Value::Float(x), Value::Float(y)) => Ok(Value::Int(crate::func::java_double_cmp(*x as f64, *y as f64) as i32)),
        (Value::Char(x), Value::Char(y)) => Ok(Value::Int(*x as i32 - *y as i32)),
        (Value::Short(x), Value::Short(y)) => Ok(Value::Int(*x as i32 - *y as i32)),
        (Value::Byte(x), Value::Byte(y)) => Ok(Value::Int(*x as i32 - *y as i32)),
        (Value::Bool(x), Value::Bool(y)) => Ok(Value::Int(if x == y { 0 } else if *x { 1 } else { -1 })),
        _ => compare_values(vm, a),
    }
}

fn int_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    parse_int_impl(&a[0], 10, true)
}
fn int_parse_radix(_: &mut Vm, a: &mut [Value]) -> VR {
    parse_int_impl(&a[0], int(&a[1]) as u32, false)
}

/// Integer parsing failures with the frames of the JDK Elasticsearch 9.5 runs. `prim` names
/// the entry point (`Integer`, `Long`) and `wrapper` the one-argument overload's frame when
/// the script called that one.
pub(crate) fn parse_failure(st: Option<&str>, radix: u32, long: bool, wrapper: bool) -> Thrown {
    let (cls_name, file, null_line, radix_lo, radix_hi, empty_line, bad_line, wrap_line) = if long {
        ("Long.parseLong", "Long.java", 518, 523, 527, 533, 556, 660)
    } else {
        ("Integer.parseInt", "Integer.java", 491, 496, 500, 506, 529, 626)
    };
    let mut fr: Vec<String> = Vec::new();
    let msg = match st {
        None => {
            fr.push(format!("java.lang.{cls_name}({file}:{null_line})"));
            "Cannot parse null string".to_string()
        }
        Some(_) if radix < 2 => {
            fr.push(format!("java.lang.{cls_name}({file}:{radix_lo})"));
            format!("radix {radix} less than Character.MIN_RADIX")
        }
        Some(_) if radix > 36 => {
            fr.push(format!("java.lang.{cls_name}({file}:{radix_hi})"));
            format!("radix {radix} greater than Character.MAX_RADIX")
        }
        Some(st) => {
            fr.push("java.lang.NumberFormatException.forInputString(NumberFormatException.java:67)".into());
            fr.push(format!("java.lang.{cls_name}({file}:{})", if st.is_empty() { empty_line } else { bad_line }));
            java::number_format_message(st, radix)
        }
    };
    if wrapper {
        fr.push(format!("java.lang.{cls_name}({file}:{wrap_line})"));
    }
    let refs: Vec<&str> = fr.iter().map(|s| s.as_str()).collect();
    nfe(msg, &refs)
}


fn parse_int_impl(v: &Value, radix: u32, wrapper: bool) -> VR {
    let st = match v {
        Value::Str(s) => s.as_str(),
        _ => return Err(parse_failure(None, radix, false, wrapper)),
    };
    if !(2..=36).contains(&radix) {
        return Err(parse_failure(Some(st), radix, false, wrapper));
    }
    match java::parse_int(st, radix) {
        Some(i) => Ok(Value::Int(i)),
        None => Err(parse_failure(Some(st), radix, false, wrapper)),
    }
}

fn long_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    parse_long_impl(&a[0], 10, true)
}
fn long_parse_radix(_: &mut Vm, a: &mut [Value]) -> VR {
    parse_long_impl(&a[0], int(&a[1]) as u32, false)
}

fn parse_long_impl(v: &Value, radix: u32, wrapper: bool) -> VR {
    let st = match v {
        Value::Str(s) => s.as_str(),
        _ => return Err(parse_failure(None, radix, true, wrapper)),
    };
    if !(2..=36).contains(&radix) {
        return Err(parse_failure(Some(st), radix, true, wrapper));
    }
    match java::parse_long(st, radix) {
        Some(i) => Ok(Value::Long(i)),
        None => Err(parse_failure(Some(st), radix, true, wrapper)),
    }
}

fn short_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    parse_small(&a[0], 10, 16, true)
}
fn short_parse_radix(_: &mut Vm, a: &mut [Value]) -> VR {
    parse_small(&a[0], int(&a[1]) as u32, 16, false)
}
fn byte_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    parse_small(&a[0], 10, 8, true)
}
fn byte_parse_radix(_: &mut Vm, a: &mut [Value]) -> VR {
    parse_small(&a[0], int(&a[1]) as u32, 8, false)
}

/// `Byte.parseByte` / `Short.parseShort`: `Integer.parseInt` plus a range check.
fn parse_small(v: &Value, radix: u32, bits: u32, wrapper: bool) -> VR {
    let (name, file, call_line, range_line, wrap_line) =
        if bits == 8 { ("Byte.parseByte", "Byte.java", 194, 196, 220) } else { ("Short.parseShort", "Short.java", 136, 138, 162) };
    let add_frames = |mut t: Thrown, line: u32| {
        t.frames.push(format!("java.base/java.lang.{name}({file}:{line})"));
        if wrapper {
            t.frames.push(format!("java.base/java.lang.{name}({file}:{wrap_line})"));
        }
        t
    };
    let i = match parse_int_impl(v, radix, false) {
        Ok(Value::Int(i)) => i,
        Err(t) => return Err(add_frames(t, call_line)),
        _ => 0,
    };
    let st = v.to_java_string();
    let (lo, hi) = if bits == 8 { (-128, 127) } else { (-32768, 32767) };
    if i < lo || i > hi {
        let t = nfe(format!("Value out of range. Value:\"{st}\" Radix:{radix}"), &[]);
        return Err(add_frames(t, range_line));
    }
    Ok(if bits == 8 { Value::Byte(i as i8) } else { Value::Short(i as i16) })
}

/// `Double.parseDouble` / `Float.parseFloat` failures (`FloatingDecimal` frames).
pub(crate) fn float_failure(v: &Value, float: bool) -> Thrown {
    let (entry, outer) = if float {
        ("jdk.internal.math.FloatingDecimal.parseFloat(FloatingDecimal.java:73)", "java.lang.Float.parseFloat(Float.java:595)")
    } else {
        ("jdk.internal.math.FloatingDecimal.parseDouble(FloatingDecimal.java:61)", "java.lang.Double.parseDouble(Double.java:993)")
    };
    match v {
        Value::Str(st) => {
            if java::java_trim(st.as_str()).is_empty() {
                nfe("empty String".into(), &["jdk.internal.math.FloatingDecimal.readJavaFormatString(FloatingDecimal.java:1319)", entry, outer])
            } else {
                nfe(
                    format!("For input string: \"{}\"", st.as_str()),
                    &[
                        "jdk.internal.math.FloatingDecimal.check(FloatingDecimal.java:1751)",
                        "jdk.internal.math.FloatingDecimal.readJavaFormatString(FloatingDecimal.java:1374)",
                        entry,
                        outer,
                    ],
                )
            }
        }
        _ => {
            let mut t = npe("Cannot invoke \"String.length()\" because \"in\" is null");
            t.frames = frames(&["jdk.internal.math.FloatingDecimal.readJavaFormatString(FloatingDecimal.java:1314)", entry, outer]);
            t
        }
    }
}

fn double_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::Str(st) => match java::parse_double(st.as_str()) {
            Some(d) => Ok(Value::Double(d)),
            None => Err(float_failure(&a[0], false)),
        },
        other => Err(float_failure(other, false)),
    }
}

fn float_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    match &a[0] {
        Value::Str(st) => match java::parse_float(st.as_str()) {
            Some(d) => Ok(Value::Float(d)),
            None => Err(float_failure(&a[0], true)),
        },
        other => Err(float_failure(other, true)),
    }
}

fn bool_parse(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(matches!(&a[0], Value::Str(s) if s.as_str().eq_ignore_ascii_case("true"))))
}

fn decode_number(st: &str) -> Option<i64> {
    let (neg, rest) = match st.as_bytes().first() {
        Some(b'-') => (true, &st[1..]),
        Some(b'+') => (false, &st[1..]),
        _ => (false, st),
    };
    let (radix, digits) = if let Some(r) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")).or_else(|| rest.strip_prefix('#')) {
        (16, r)
    } else if rest.starts_with('0') && rest.len() > 1 {
        (8, &rest[1..])
    } else {
        (10, rest)
    };
    if digits.starts_with(['-', '+']) {
        return None;
    }
    let s = if neg { format!("-{digits}") } else { digits.to_string() };
    java::parse_long(&s, radix)
}

fn int_decode(_: &mut Vm, a: &mut [Value]) -> VR {
    let st = s(&a[0]).to_string();
    match decode_number(&st).and_then(|v| i32::try_from(v).ok()) {
        Some(v) => Ok(Value::Int(v)),
        None => Err(nfe(format!("For input string: \"{st}\""), &[])),
    }
}
fn long_decode(_: &mut Vm, a: &mut [Value]) -> VR {
    let st = s(&a[0]).to_string();
    match decode_number(&st) {
        Some(v) => Ok(Value::Long(v)),
        None => Err(nfe(format!("For input string: \"{st}\""), &[])),
    }
}
fn short_decode(_: &mut Vm, a: &mut [Value]) -> VR {
    let st = s(&a[0]).to_string();
    match decode_number(&st).and_then(|v| i16::try_from(v).ok()) {
        Some(v) => Ok(Value::Short(v)),
        None => Err(nfe(format!("For input string: \"{st}\""), &[])),
    }
}
fn byte_decode(_: &mut Vm, a: &mut [Value]) -> VR {
    let st = s(&a[0]).to_string();
    match decode_number(&st).and_then(|v| i8::try_from(v).ok()) {
        Some(v) => Ok(Value::Byte(v)),
        None => Err(nfe(format!("For input string: \"{st}\""), &[])),
    }
}

fn int_value_of_radix(vm: &mut Vm, a: &mut [Value]) -> VR {
    int_parse_radix(vm, a)
}
fn long_value_of_radix(vm: &mut Vm, a: &mut [Value]) -> VR {
    long_parse_radix(vm, a)
}

fn to_string_static(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(a[0].to_java_string()))
}

fn int_to_string_radix(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(java::int_to_string_radix(int(&a[0]) as i64, int(&a[1]) as u32)))
}
fn long_to_string_radix(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(java::int_to_string_radix(long(&a[0]), int(&a[1]) as u32)))
}
fn int_to_binary(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(format!("{:b}", int(&a[0]) as u32)))
}
fn int_to_hex(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(format!("{:x}", int(&a[0]) as u32)))
}
fn int_to_octal(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(format!("{:o}", int(&a[0]) as u32)))
}
fn long_to_binary(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(format!("{:b}", long(&a[0]) as u64)))
}
fn long_to_hex(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(format!("{:x}", long(&a[0]) as u64)))
}
fn long_to_octal(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(format!("{:o}", long(&a[0]) as u64)))
}
fn int_unsigned_string(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string((int(&a[0]) as u32).to_string()))
}
fn int_unsigned_string_radix(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(java::unsigned_to_string_radix(int(&a[0]) as u32 as u64, int(&a[1]) as u32)))
}
fn long_unsigned_string(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string((long(&a[0]) as u64).to_string()))
}
fn long_unsigned_string_radix(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(java::unsigned_to_string_radix(long(&a[0]) as u64, int(&a[1]) as u32)))
}
fn int_to_unsigned_long(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(int(&a[0]) as u32 as i64))
}

macro_rules! int_fn {
    ($name:ident, |$x:ident| $body:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            let $x = int(&a[0]);
            Ok(Value::Int($body))
        }
    };
}
macro_rules! int2_fn {
    ($name:ident, |$x:ident, $y:ident| $body:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            let ($x, $y) = (int(&a[0]), int(&a[1]));
            Ok(Value::Int($body))
        }
    };
}
macro_rules! long_fn {
    ($name:ident, $ret:ident, |$x:ident| $body:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            let $x = long(&a[0]);
            Ok(Value::$ret($body))
        }
    };
}
macro_rules! long2_fn {
    ($name:ident, $ret:ident, |$x:ident, $y:ident| $body:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            let ($x, $y) = (long(&a[0]), long(&a[1]));
            Ok(Value::$ret($body))
        }
    };
}

int_fn!(int_bit_count, |x| x.count_ones() as i32);
int2_fn!(int_compare, |x, y| (x > y) as i32 - (x < y) as i32);
int2_fn!(int_compare_unsigned, |x, y| { let (x, y) = (x as u32, y as u32); (x > y) as i32 - (x < y) as i32 });
fn int_divide_unsigned(_: &mut Vm, a: &mut [Value]) -> VR {
    let (x, y) = (int(&a[0]) as u32, int(&a[1]) as u32);
    if y == 0 {
        return Err(Throw::new(cls::ARITHMETIC_EXCEPTION, Some("/ by zero".into())));
    }
    Ok(Value::Int((x / y) as i32))
}
fn int_remainder_unsigned(_: &mut Vm, a: &mut [Value]) -> VR {
    let (x, y) = (int(&a[0]) as u32, int(&a[1]) as u32);
    if y == 0 {
        return Err(Throw::new(cls::ARITHMETIC_EXCEPTION, Some("/ by zero".into())));
    }
    Ok(Value::Int((x % y) as i32))
}
int_fn!(int_hash, |x| x);
int_fn!(int_highest_one_bit, |x| if x == 0 { 0 } else { (1u32 << (31 - (x as u32).leading_zeros())) as i32 });
int_fn!(int_lowest_one_bit, |x| x & x.wrapping_neg());
int2_fn!(int_max, |x, y| x.max(y));
int2_fn!(int_min, |x, y| x.min(y));
int_fn!(int_nlz, |x| x.leading_zeros() as i32);
int_fn!(int_ntz, |x| x.trailing_zeros() as i32);
int_fn!(int_reverse, |x| x.reverse_bits());
int_fn!(int_reverse_bytes, |x| x.swap_bytes());
int2_fn!(int_rotate_left, |x, y| x.rotate_left((y & 31) as u32));
int2_fn!(int_rotate_right, |x, y| x.rotate_right((y & 31) as u32));
int_fn!(int_signum, |x| x.signum());
fn int_parse_unsigned(_: &mut Vm, a: &mut [Value]) -> VR {
    parse_unsigned(&a[0], 10, false)
}
fn int_parse_unsigned_radix(_: &mut Vm, a: &mut [Value]) -> VR {
    parse_unsigned(&a[0], int(&a[1]) as u32, false)
}
fn long_parse_unsigned(_: &mut Vm, a: &mut [Value]) -> VR {
    parse_unsigned(&a[0], 10, true)
}
fn long_parse_unsigned_radix(_: &mut Vm, a: &mut [Value]) -> VR {
    parse_unsigned(&a[0], int(&a[1]) as u32, true)
}
fn parse_unsigned(v: &Value, radix: u32, wide: bool) -> VR {
    let st = s(v).to_string();
    let digits = st.strip_prefix('+').unwrap_or(&st);
    let parsed = if digits.is_empty() || digits.starts_with('-') { None } else { u64::from_str_radix(digits, radix).ok() };
    match parsed {
        Some(v) if wide => Ok(Value::Long(v as i64)),
        Some(v) if v <= u32::MAX as u64 => Ok(Value::Int(v as u32 as i32)),
        _ => Err(nfe(java::number_format_message(&st, radix), &[])),
    }
}

long_fn!(long_bit_count, Int, |x| x.count_ones() as i32);
long2_fn!(long_compare, Int, |x, y| (x > y) as i32 - (x < y) as i32);
long2_fn!(long_compare_unsigned, Int, |x, y| { let (x, y) = (x as u64, y as u64); (x > y) as i32 - (x < y) as i32 });
fn long_divide_unsigned(_: &mut Vm, a: &mut [Value]) -> VR {
    let (x, y) = (long(&a[0]) as u64, long(&a[1]) as u64);
    if y == 0 {
        return Err(Throw::new(cls::ARITHMETIC_EXCEPTION, Some("/ by zero".into())));
    }
    Ok(Value::Long((x / y) as i64))
}
fn long_remainder_unsigned(_: &mut Vm, a: &mut [Value]) -> VR {
    let (x, y) = (long(&a[0]) as u64, long(&a[1]) as u64);
    if y == 0 {
        return Err(Throw::new(cls::ARITHMETIC_EXCEPTION, Some("/ by zero".into())));
    }
    Ok(Value::Long((x % y) as i64))
}
long_fn!(long_hash, Int, |x| java::long_hash(x));
long_fn!(long_highest_one_bit, Long, |x| if x == 0 { 0 } else { (1u64 << (63 - (x as u64).leading_zeros())) as i64 });
long_fn!(long_lowest_one_bit, Long, |x| x & x.wrapping_neg());
long2_fn!(long_max, Long, |x, y| x.max(y));
long2_fn!(long_min, Long, |x, y| x.min(y));
long_fn!(long_nlz, Int, |x| x.leading_zeros() as i32);
long_fn!(long_ntz, Int, |x| x.trailing_zeros() as i32);
long_fn!(long_reverse, Long, |x| x.reverse_bits());
long_fn!(long_reverse_bytes, Long, |x| x.swap_bytes());
fn long_rotate_left(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(long(&a[0]).rotate_left((int(&a[1]) & 63) as u32)))
}
fn long_rotate_right(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(long(&a[0]).rotate_right((int(&a[1]) & 63) as u32)))
}
long_fn!(long_signum, Int, |x| x.signum() as i32);
long2_fn!(long_sum, Long, |x, y| x.wrapping_add(y));

fn double_compare(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(crate::func::java_double_cmp(double(&a[0]), double(&a[1])) as i32))
}
fn double_to_long_bits(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = double(&a[0]);
    Ok(Value::Long(if d.is_nan() { 0x7ff8000000000000 } else { d.to_bits() as i64 }))
}
fn double_to_raw_long_bits(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(double(&a[0]).to_bits() as i64))
}
fn double_hash(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(java::double_hash(double(&a[0]))))
}
fn double_is_finite(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(double(&a[0]).is_finite()))
}
fn double_is_infinite(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(double(&a[0]).is_infinite()))
}
fn double_is_nan(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(double(&a[0]).is_nan()))
}
fn double_from_bits(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(f64::from_bits(long(&a[0]) as u64)))
}
fn double_max(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(java_max(double(&a[0]), double(&a[1]))))
}
fn double_min(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(java_min(double(&a[0]), double(&a[1]))))
}
fn double_sum(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(double(&a[0]) + double(&a[1])))
}
fn double_to_hex(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(java::double_to_hex(double(&a[0]))))
}
fn double_to_string(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(java::fmt_double(double(&a[0]))))
}
fn double_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(double(&a[0])))
}
fn float_compare(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(crate::func::java_double_cmp(float(&a[0]) as f64, float(&a[1]) as f64) as i32))
}
fn float_to_int_bits(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(java::float_hash(float(&a[0]))))
}
fn float_to_raw_int_bits(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(float(&a[0]).to_bits() as i32))
}
fn float_hash(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(java::float_hash(float(&a[0]))))
}
fn float_from_bits(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Float(f32::from_bits(int(&a[0]) as u32)))
}
fn float_max(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Float(java_max(float(&a[0]) as f64, float(&a[1]) as f64) as f32))
}
fn float_min(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Float(java_min(float(&a[0]) as f64, float(&a[1]) as f64) as f32))
}
fn float_sum(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Float(float(&a[0]) + float(&a[1])))
}
fn float_to_hex(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(java::double_to_hex(float(&a[0]) as f64)))
}
fn float_to_string(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(java::fmt_float(float(&a[0]))))
}
fn float_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Float(float(&a[0])))
}
fn bool_compare(_: &mut Vm, a: &mut [Value]) -> VR {
    let (x, y) = (boolean(&a[0]), boolean(&a[1]));
    Ok(Value::Int(if x == y { 0 } else if x { 1 } else { -1 }))
}
fn bool_hash(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(if boolean(&a[0]) { 1231 } else { 1237 }))
}
fn bool_and(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(boolean(&a[0]) && boolean(&a[1])))
}
fn bool_or(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(boolean(&a[0]) || boolean(&a[1])))
}
fn bool_xor(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(boolean(&a[0]) ^ boolean(&a[1])))
}
fn bool_value(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool(boolean(&a[0])))
}
fn bool_true(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Bool(true))
}
fn bool_false(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Bool(false))
}
fn small_compare(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(int(&a[0]) - int(&a[1])))
}
fn small_hash(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(int(&a[0])))
}
fn byte_to_unsigned_int(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(int(&a[0]) & 0xff))
}
fn byte_to_unsigned_long(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long((int(&a[0]) & 0xff) as i64))
}
fn short_to_unsigned_int(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(int(&a[0]) & 0xffff))
}
fn short_to_unsigned_long(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long((int(&a[0]) & 0xffff) as i64))
}
fn short_reverse_bytes(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Short((int(&a[0]) as i16).swap_bytes()))
}
fn byte_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Byte(int(&a[0]) as i8))
}
fn byte_value_of_radix(vm: &mut Vm, a: &mut [Value]) -> VR {
    byte_parse_radix(vm, a)
}
fn short_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Short(int(&a[0]) as i16))
}
fn short_value_of_radix(vm: &mut Vm, a: &mut [Value]) -> VR {
    short_parse_radix(vm, a)
}
fn int_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(int(&a[0])))
}
fn long_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(long(&a[0])))
}

// --- Character
fn ch(a: &[Value], i: usize) -> Option<char> {
    match a[i] {
        Value::Char(c) => char::from_u32(c as u32),
        _ => char::from_u32(int(&a[i]) as u32),
    }
}
macro_rules! char_pred {
    ($name:ident, |$c:ident| $body:expr) => {
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            Ok(Value::Bool(match ch(a, 0) {
                Some($c) => $body,
                None => false,
            }))
        }
    };
}
char_pred!(char_is_digit, |c| c.is_numeric() && general_category_digit(c));
char_pred!(char_is_letter, |c| c.is_alphabetic() && !is_mark_or_number(c));
char_pred!(char_is_letter_or_digit, |c| (c.is_alphabetic() && !is_mark_or_number(c)) || general_category_digit(c));
char_pred!(char_is_alphabetic, |c| c.is_alphabetic());
char_pred!(char_is_lower, |c| c.is_lowercase());
char_pred!(char_is_upper, |c| c.is_uppercase());
char_pred!(char_is_whitespace, |c| is_java_whitespace(c));
char_pred!(char_is_space_char, |c| matches!(c, ' ' | '\u{00A0}' | '\u{1680}' | '\u{2000}'..='\u{200A}' | '\u{2028}' | '\u{2029}' | '\u{202F}' | '\u{205F}' | '\u{3000}'));
char_pred!(char_is_iso_control, |c| (c as u32) <= 0x1F || (0x7F..=0x9F).contains(&(c as u32)));
char_pred!(char_is_title_case, |c| matches!(c, 'ǅ' | 'ǈ' | 'ǋ' | 'ǲ'));
char_pred!(char_is_defined, |c| { let _ = c; true });
char_pred!(char_is_ideographic, |c| ('\u{4E00}'..='\u{9FFF}').contains(&c) || ('\u{3400}'..='\u{4DBF}').contains(&c) || ('\u{20000}'..='\u{2A6DF}').contains(&c));
char_pred!(char_is_java_id_start, |c| c.is_alphabetic() || c == '_' || c == '$');
char_pred!(char_is_java_id_part, |c| c.is_alphanumeric() || c == '_' || c == '$');
char_pred!(char_is_unicode_id_start, |c| c.is_alphabetic());
char_pred!(char_is_unicode_id_part, |c| c.is_alphanumeric() || c == '_');
char_pred!(char_is_mirrored, |c| matches!(c, '(' | ')' | '<' | '>' | '[' | ']' | '{' | '}' | '«' | '»'));
char_pred!(char_is_id_ignorable, |c| (c as u32) <= 8 || (0x0E..=0x1B).contains(&(c as u32)) || (0x7F..=0x9F).contains(&(c as u32)));

fn is_mark_or_number(c: char) -> bool {
    // Rust's is_alphabetic includes letter numbers (Nl) and some marks; Java's isLetter does not
    matches!(c as u32, 0x16EE..=0x16F0 | 0x2160..=0x2188 | 0x3007 | 0x3021..=0x3029 | 0x3038..=0x303A)
}

fn general_category_digit(c: char) -> bool {
    if c.is_ascii_digit() {
        return true;
    }
    // decimal digits (Nd) of common scripts
    let u = c as u32;
    [0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66, 0x0CE6, 0x0D66, 0x0E50, 0x0ED0, 0x0F20, 0x1040, 0xFF10]
        .iter()
        .any(|&z| (z..z + 10).contains(&u))
}

fn is_java_whitespace(c: char) -> bool {
    match c {
        '\u{00A0}' | '\u{2007}' | '\u{202F}' => false,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | '\u{001C}'..='\u{001F}' => true,
        c => c.is_whitespace(),
    }
}

fn char_to_lower(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Char(java::char_lower(char16(&a[0]))))
}
fn char_to_upper(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Char(java::char_upper(char16(&a[0]))))
}
fn char_to_title(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Char(java::char_upper(char16(&a[0]))))
}
fn char_to_string(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(java::char_to_string(char16(&a[0]))))
}
fn char_value_of(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Char(char16(&a[0])))
}
fn char_compare(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(char16(&a[0]) as i32 - char16(&a[1]) as i32))
}
fn char_digit(_: &mut Vm, a: &mut [Value]) -> VR {
    let radix = int(&a[1]) as u32;
    let d = ch(a, 0).and_then(|c| if (2..=36).contains(&radix) { c.to_digit(radix) } else { None });
    Ok(Value::Int(d.map(|d| d as i32).unwrap_or(-1)))
}
fn char_for_digit(_: &mut Vm, a: &mut [Value]) -> VR {
    let (d, radix) = (int(&a[0]), int(&a[1]));
    if !(2..=36).contains(&radix) || d < 0 || d >= radix {
        return Ok(Value::Char(0));
    }
    Ok(Value::Char(std::char::from_digit(d as u32, radix as u32).unwrap() as u16))
}
fn char_numeric_value(_: &mut Vm, a: &mut [Value]) -> VR {
    let d = ch(a, 0).and_then(|c| c.to_digit(36));
    Ok(Value::Int(d.map(|d| d as i32).unwrap_or(-1)))
}
fn char_char_count(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(if int(&a[0]) >= 0x10000 { 2 } else { 1 }))
}
fn char_is_bmp(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool((int(&a[0]) as u32) >> 16 == 0))
}
fn char_is_supplementary(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = int(&a[0]);
    Ok(Value::Bool((0x10000..=0x10FFFF).contains(&c)))
}
fn char_is_valid_cp(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = int(&a[0]);
    Ok(Value::Bool((0..=0x10FFFF).contains(&c)))
}
fn char_is_high_surrogate(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool((0xD800..0xDC00).contains(&char16(&a[0]))))
}
fn char_is_surrogate(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool((0xD800..0xE000).contains(&char16(&a[0]))))
}
fn char_is_surrogate_pair(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Bool((0xD800..0xDC00).contains(&char16(&a[0])) && (0xDC00..0xE000).contains(&char16(&a[1]))))
}
fn char_to_code_point(_: &mut Vm, a: &mut [Value]) -> VR {
    let (h, l) = (char16(&a[0]) as i32, char16(&a[1]) as i32);
    Ok(Value::Int(((h - 0xD800) << 10) + (l - 0xDC00) + 0x10000))
}
fn char_high_surrogate(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = int(&a[0]);
    Ok(Value::Char((((c - 0x10000) >> 10) + 0xD800) as u16))
}
fn char_low_surrogate(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = int(&a[0]);
    Ok(Value::Char((((c - 0x10000) & 0x3FF) + 0xDC00) as u16))
}
fn char_to_chars(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = int(&a[0]);
    let Some(ch) = char::from_u32(c as u32) else {
        return Err(iae(format!("Not a valid Unicode code point: 0x{:X}", c as u32)));
    };
    let mut buf = [0u16; 2];
    let v = ch.encode_utf16(&mut buf).to_vec();
    Ok(Value::obj(Obj::Array(JArray::from_data(Ty::CHAR, ArrayData::Char(v)))))
}
fn char_reverse_bytes(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Char(char16(&a[0]).swap_bytes()))
}
fn char_get_type(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = ch(a, 0);
    Ok(Value::Int(match c {
        Some(c) if c.is_uppercase() => 1,
        Some(c) if c.is_lowercase() => 2,
        Some(c) if general_category_digit(c) => 9,
        Some(c) if c.is_alphabetic() => 5,
        Some(' ') => 12,
        Some(c) if c.is_control() => 15,
        Some(_) => 24,
        None => 0,
    }))
}
fn char_get_name(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Null)
}
fn char_code_point_at_cs(_: &mut Vm, a: &mut [Value]) -> VR {
    let units: Vec<u16> = char_seq(&a[0])?.encode_utf16().collect();
    let i = int(&a[1]) as usize;
    if i >= units.len() {
        return Err(sioobe_index(i as i64, units.len(), &[]));
    }
    let c = units[i];
    if (0xD800..0xDC00).contains(&c) && i + 1 < units.len() && (0xDC00..0xE000).contains(&units[i + 1]) {
        return Ok(Value::Int(0x10000 + (((c as i32) - 0xD800) << 10) + (units[i + 1] as i32 - 0xDC00)));
    }
    Ok(Value::Int(c as i32))
}

// ------------------------------------------------------------------------------ Math

pub fn java_max(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() && b.is_sign_negative() { -0.0 } else { 0.0 };
    }
    a.max(b)
}

pub fn java_min(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        return f64::NAN;
    }
    if a == 0.0 && b == 0.0 {
        return if a.is_sign_negative() || b.is_sign_negative() { -0.0 } else { 0.0 };
    }
    a.min(b)
}

// Each `Math` function is a native plus, in a module of the same name, the plain `f64`
// function behind it (`math_log::d`), which the expression fast path calls directly.
macro_rules! math1 {
    ($name:ident, |$x:ident| $body:expr) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;
            pub fn d($x: f64) -> f64 {
                $body
            }
        }
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            Ok(Value::Double($name::d(double(&a[0]))))
        }
    };
}
macro_rules! math2 {
    ($name:ident, |$x:ident, $y:ident| $body:expr) => {
        mod $name {
            #[allow(unused_imports)]
            use super::*;
            pub fn d($x: f64, $y: f64) -> f64 {
                $body
            }
        }
        fn $name(_: &mut Vm, a: &mut [Value]) -> VR {
            Ok(Value::Double($name::d(double(&a[0]), double(&a[1]))))
        }
    };
}

/// A `double`-only `Math`/`StrictMath` function as a plain function pointer.
#[derive(Clone, Copy)]
pub(crate) enum MathFn {
    F1(fn(f64) -> f64),
    F2(fn(f64, f64) -> f64),
}

/// The plain function behind `Math.<name>` (`strict`: `StrictMath`) with the given arity,
/// exactly what the registered native computes.
pub(crate) fn math_fn(name: &str, arity: usize, strict: bool) -> Option<MathFn> {
    use MathFn::{F1, F2};
    Some(match (name, arity) {
        ("abs", 1) => F1(math_abs::d),
        ("acos", 1) => F1(math_acos::d),
        ("asin", 1) => F1(math_asin::d),
        ("atan", 1) => F1(math_atan::d),
        ("atan2", 2) => F2(math_atan2::d),
        ("cbrt", 1) => F1(math_cbrt::d),
        ("ceil", 1) => F1(math_ceil::d),
        ("cos", 1) => F1(if strict { math_cos::d } else { math_cos_i::d }),
        ("cosh", 1) => F1(math_cosh::d),
        ("exp", 1) => F1(if strict { math_exp::d } else { math_exp_i::d }),
        ("expm1", 1) => F1(math_expm1::d),
        ("floor", 1) => F1(math_floor::d),
        ("hypot", 2) => F2(math_hypot::d),
        ("log", 1) => F1(if strict { math_log::d } else { math_log_i::d }),
        ("log10", 1) => F1(if strict { math_log10::d } else { math_log10_i::d }),
        ("log1p", 1) => F1(math_log1p::d),
        ("max", 2) => F2(math_max::d),
        ("min", 2) => F2(math_min::d),
        ("pow", 2) => F2(if strict { math_pow::d } else { math_pow_i::d }),
        ("rint", 1) => F1(math_rint::d),
        ("signum", 1) => F1(math_signum::d),
        ("sin", 1) => F1(if strict { math_sin::d } else { math_sin_i::d }),
        ("sinh", 1) => F1(math_sinh::d),
        ("sqrt", 1) => F1(math_sqrt::d),
        ("tan", 1) => F1(if strict { math_tan::d } else { math_tan_i::d }),
        ("tanh", 1) => F1(math_tanh::d),
        ("toDegrees", 1) => F1(math_to_degrees::d),
        ("toRadians", 1) => F1(math_to_radians::d),
        _ => return None,
    })
}
math1!(math_abs, |x| x.abs());
math1!(math_acos, |x| libm::acos(x));
math1!(math_asin, |x| libm::asin(x));
math1!(math_atan, |x| libm::atan(x));
math2!(math_atan2, |x, y| libm::atan2(x, y));
math1!(math_cbrt, |x| libm::cbrt(x));
math1!(math_ceil, |x| x.ceil());
math2!(math_copy_sign, |x, y| x.copysign(y));
math1!(math_cos, |x| libm::cos(x));
math1!(math_cosh, |x| libm::cosh(x));
math1!(math_exp, |x| libm::exp(x));
math1!(math_expm1, |x| libm::expm1(x));
math1!(math_floor, |x| x.floor());
math2!(math_hypot, |x, y| libm::hypot(x, y));
math2!(math_ieee_rem, |x, y| java::ieee_remainder(x, y));
math1!(math_log, |x| libm::log(x));
math1!(math_log10, |x| libm::log10(x));
math1!(math_log1p, |x| libm::log1p(x));
math2!(math_max, |x, y| java_max(x, y));
math2!(math_min, |x, y| java_min(x, y));
math2!(math_next_after, |x, y| java::next_after(x, y));
math1!(math_next_down, |x| java::next_down(x));
math1!(math_next_up, |x| java::next_up(x));
math2!(math_pow, |x, y| java_pow(x, y));
math1!(math_rint, |x| java::math_rint(x));
math1!(math_signum, |x| java::math_signum(x));
math1!(math_sin, |x| libm::sin(x));
math1!(math_sinh, |x| libm::sinh(x));
math1!(math_sqrt, |x| x.sqrt());
math1!(math_tan, |x| libm::tan(x));
math1!(math_tanh, |x| libm::tanh(x));
math1!(math_to_degrees, |x| x * 57.29577951308232);
math1!(math_to_radians, |x| x * 0.017453292519943295);
math1!(math_ulp, |x| java::ulp(x));
// `Math` (not `StrictMath`) runs these as HotSpot intrinsics, which round like the platform's
// correctly-rounding libm rather than like fdlibm
math1!(math_cos_i, |x| x.cos());
math1!(math_exp_i, |x| x.exp());
math1!(math_log_i, |x| x.ln());
math1!(math_log10_i, |x| x.log10());
math1!(math_sin_i, |x| x.sin());
math1!(math_tan_i, |x| x.tan());
math2!(math_pow_i, |x, y| java_pow_with(x, y, f64::powf));

/// `Math.pow` special cases differ from C's for `pow(1, NaN)` and `pow(-1, ±Inf)`.
fn java_pow(x: f64, y: f64) -> f64 {
    java_pow_with(x, y, libm::pow)
}

fn java_pow_with(x: f64, y: f64, f: fn(f64, f64) -> f64) -> f64 {
    if y.is_nan() {
        return f64::NAN;
    }
    if (x == 1.0 || x == -1.0) && y.is_infinite() {
        return f64::NAN;
    }
    if x.is_nan() && y != 0.0 {
        return f64::NAN;
    }
    f(x, y)
}

fn math_round(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Long(java::math_round(double(&a[0]))))
}
fn math_scalb(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Double(libm::scalbn(double(&a[0]), int(&a[1]))))
}
fn math_random(vm: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Double(crate::lib_util::thread_random(vm)))
}
fn math_e(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Double(std::f64::consts::E))
}
fn math_pi(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::Double(std::f64::consts::PI))
}

// ------------------------------------------------------------------------------ StringBuilder

fn builder(class: ClassId, s: String) -> Value {
    Value::obj(Obj::Builder { class, s: RefCell::new(s) })
}

fn sb_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(builder(cls::STRING_BUILDER, String::new()))
}
fn sb_new_cs(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(builder(cls::STRING_BUILDER, char_seq(&a[0])?))
}
fn sbuf_new(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(builder(cls::STRING_BUFFER, String::new()))
}
fn sbuf_new_cs(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(builder(cls::STRING_BUFFER, char_seq(&a[0])?))
}

fn with_sb<R>(v: &Value, f: impl FnOnce(&mut String) -> R) -> R {
    match v {
        Value::Obj(o) => match &**o {
            Obj::Builder { s, .. } => f(&mut s.borrow_mut()),
            _ => f(&mut String::new()),
        },
        _ => f(&mut String::new()),
    }
}

/// Fails like the JVM when a builder would grow past the object size limit.
fn sb_grow(v: &Value, extra: usize) -> Result<(), Thrown> {
    with_sb(v, |s| crate::guard::check_string(s.len().saturating_add(extra)))
}

fn sb_append(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = match &a[1] {
        Value::Obj(o) if matches!(&**o, Obj::Builder { .. }) => char_seq(&a[1])?,
        Value::Obj(o) if matches!(&**o, Obj::Array(arr) if arr.elem == Ty::CHAR) => match &**o {
            Obj::Array(arr) => match &*arr.data.borrow() {
                ArrayData::Char(c) => String::from_utf16_lossy(c),
                _ => String::new(),
            },
            _ => String::new(),
        },
        v => v.to_java_string(),
    };
    sb_grow(&a[0], text.len())?;
    with_sb(&a[0], |s| s.push_str(&text));
    Ok(a[0].clone())
}

fn sb_append_range(_: &mut Vm, a: &mut [Value]) -> VR {
    let text = match &a[1] {
        Value::Null => "null".to_string(),
        v => char_seq(v)?,
    };
    let (b, e) = (int(&a[2]), int(&a[3]));
    let st = JStr::new(text.as_str());
    let len = st.len16();
    if b < 0 || e < b || e as usize > len {
        return Err(Throw::new(cls::INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("start {b}, end {e}, length {len}"))));
    }
    let piece = &st.as_str()[st.byte_index(b as usize).unwrap()..st.byte_index(e as usize).unwrap()];
    sb_grow(&a[0], piece.len())?;
    with_sb(&a[0], |s| s.push_str(piece));
    Ok(a[0].clone())
}

fn sb_append_code_point(_: &mut Vm, a: &mut [Value]) -> VR {
    let c = int(&a[1]);
    let Some(ch) = char::from_u32(c as u32) else {
        return Err(iae(format!("Not a valid Unicode code point: 0x{:X}", c as u32)));
    };
    with_sb(&a[0], |s| s.push(ch));
    Ok(a[0].clone())
}

fn sb_capacity(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(with_sb(&a[0], |s| (java::utf16_len(s) + 16).max(16) as i32)))
}

fn sb_range_check(s: &str, b: i32, e: i32, len_is_end: bool) -> Result<(usize, usize), Thrown> {
    let len = java::utf16_len(s) as i32;
    let e = if len_is_end { e.min(len) } else { e };
    if b < 0 || b > e || e > len {
        return Err(sioobe_range(b as i64, e as i64, len as usize, &[]));
    }
    Ok((java::utf16_to_byte(s, b as usize).unwrap(), java::utf16_to_byte(s, e as usize).unwrap()))
}

fn sb_delete(_: &mut Vm, a: &mut [Value]) -> VR {
    let (b, e) = (int(&a[1]), int(&a[2]));
    let class = a[0].class();
    with_sb(&a[0], |s| {
        let len = java::utf16_len(s) as i32;
        let e = e.min(len);
        if b < 0 || b > e {
            return Err(sb_range_error(b as i64, e as i64, len as usize, builder_frames(class, "delete")));
        }
        let (bb, eb) = sb_range_check(s, b, e, true)?;
        s.replace_range(bb..eb, "");
        Ok::<_, Thrown>(())
    })?;
    Ok(a[0].clone())
}

fn sb_delete_char_at(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = int(&a[1]);
    let class = a[0].class();
    with_sb(&a[0], |s| {
        let len = java::utf16_len(s);
        if i < 0 || i as usize >= len {
            return Err(sioobe_index(i as i64, len, builder_frames(class, "deleteCharAt")));
        }
        let b = java::utf16_to_byte(s, i as usize).unwrap();
        let e = java::utf16_to_byte(s, i as usize + 1).unwrap_or(s.len());
        s.replace_range(b..e.max(b + 1).min(s.len()), "");
        Ok(())
    })?;
    Ok(a[0].clone())
}

fn sb_insert(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = int(&a[1]);
    let class = a[0].class();
    let text = match &a[2] {
        Value::Obj(o) if matches!(&**o, Obj::Builder { .. }) => char_seq(&a[2])?,
        v => v.to_java_string(),
    };
    sb_grow(&a[0], text.len())?;
    with_sb(&a[0], |s| {
        let len = java::utf16_len(s);
        if i < 0 || i as usize > len {
            // insert(int, Object) goes through String.valueOf to insert(int, String)
            return Err(sb_range_error(i as i64, len as i64, len, builder_frames(class, "insert")));
        }
        let b = java::utf16_to_byte(s, i as usize).unwrap();
        s.insert_str(b, &text);
        Ok(())
    })?;
    Ok(a[0].clone())
}

fn sb_replace(_: &mut Vm, a: &mut [Value]) -> VR {
    let (b, e) = (int(&a[1]), int(&a[2]));
    let class = a[0].class();
    let text = string(&a[3], "length()")?.as_str().to_string();
    sb_grow(&a[0], text.len())?;
    with_sb(&a[0], |s| {
        let len = java::utf16_len(s) as i32;
        let e = e.min(len);
        if b < 0 || b > e {
            return Err(sb_range_error(b as i64, e as i64, len as usize, builder_frames(class, "replace")));
        }
        let (bb, eb) = (java::utf16_to_byte(s, b as usize).unwrap(), java::utf16_to_byte(s, e as usize).unwrap());
        s.replace_range(bb..eb, &text);
        Ok(())
    })?;
    Ok(a[0].clone())
}

fn sb_reverse(_: &mut Vm, a: &mut [Value]) -> VR {
    with_sb(&a[0], |s| {
        let r: String = s.chars().rev().collect();
        *s = r;
    });
    Ok(a[0].clone())
}

fn sb_set_char_at(_: &mut Vm, a: &mut [Value]) -> VR {
    let i = int(&a[1]);
    let c = char16(&a[2]);
    with_sb(&a[0], |s| {
        // ASCII in, ASCII out: one byte changes (loops over a builder stay linear)
        if s.is_ascii() && c < 0x80 {
            if i < 0 || i as usize >= s.len() {
                return Err(sioobe_index(i as i64, s.len(), &[]));
            }
            // SAFETY: an ASCII byte replaces an ASCII byte, so the string stays UTF-8
            unsafe { s.as_bytes_mut()[i as usize] = c as u8 };
            return Ok(());
        }
        let mut u: Vec<u16> = s.encode_utf16().collect();
        if i < 0 || i as usize >= u.len() {
            return Err(sioobe_index(i as i64, u.len(), &[]));
        }
        u[i as usize] = c;
        *s = String::from_utf16_lossy(&u);
        Ok(())
    })?;
    Ok(Value::Null)
}

fn sb_set_length(_: &mut Vm, a: &mut [Value]) -> VR {
    let n = int(&a[1]);
    if n < 0 {
        let mut t = Throw::new(cls::STRING_INDEX_OUT_OF_BOUNDS_EXCEPTION, Some(format!("String index out of range: {n}")));
        t.frames = frames(builder_frames(a[0].class(), "setLength"));
        return Err(t);
    }
    crate::guard::check_string(n as usize)?;
    with_sb(&a[0], |s| {
        if s.is_ascii() {
            if (n as usize) <= s.len() {
                s.truncate(n as usize);
            } else {
                let pad = n as usize - s.len();
                s.extend(std::iter::repeat_n('\0', pad));
            }
            return;
        }
        let mut u: Vec<u16> = s.encode_utf16().collect();
        u.resize(n as usize, 0);
        *s = String::from_utf16_lossy(&u);
    });
    Ok(Value::Null)
}

fn sb_to_jstr(v: &Value) -> Rc<JStr> {
    Rc::new(JStr::new(with_sb(v, |s| s.clone())))
}

fn sb_index_of(vm: &mut Vm, a: &mut [Value]) -> VR {
    let mut args = [Value::Str(sb_to_jstr(&a[0])), a[1].clone()];
    str_index_of(vm, &mut args)
}
fn sb_index_of2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let mut args = [Value::Str(sb_to_jstr(&a[0])), a[1].clone(), a[2].clone()];
    str_index_of2(vm, &mut args)
}
fn sb_last_index_of(vm: &mut Vm, a: &mut [Value]) -> VR {
    let mut args = [Value::Str(sb_to_jstr(&a[0])), a[1].clone()];
    str_last_index_of(vm, &mut args)
}
fn sb_last_index_of2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let mut args = [Value::Str(sb_to_jstr(&a[0])), a[1].clone(), a[2].clone()];
    str_last_index_of2(vm, &mut args)
}
fn sb_substring(vm: &mut Vm, a: &mut [Value]) -> VR {
    let st = sb_to_jstr(&a[0]);
    let (b, len) = (int(&a[1]), st.len16() as i32);
    if b < 0 || b > len {
        return Err(sb_range_error(b as i64, len as i64, len as usize, builder_frames(a[0].class(), "substring")));
    }
    let mut args = [Value::Str(st), a[1].clone()];
    str_substring1(vm, &mut args)
}
fn sb_substring2(vm: &mut Vm, a: &mut [Value]) -> VR {
    let st = sb_to_jstr(&a[0]);
    let (b, e, len) = (int(&a[1]), int(&a[2]), st.len16() as i32);
    if b < 0 || b > e || e > len {
        return Err(sb_range_error(b as i64, e as i64, len as usize, builder_frames(a[0].class(), "substring2")));
    }
    let mut args = [Value::Str(st), a[1].clone(), a[2].clone()];
    str_substring2(vm, &mut args)
}
fn sb_code_point_at(vm: &mut Vm, a: &mut [Value]) -> VR {
    let mut args = [Value::Str(sb_to_jstr(&a[0])), a[1].clone()];
    str_code_point_at(vm, &mut args)
}
fn sb_code_point_before(vm: &mut Vm, a: &mut [Value]) -> VR {
    let mut args = [Value::Str(sb_to_jstr(&a[0])), a[1].clone()];
    str_code_point_before(vm, &mut args)
}
fn sb_code_point_count(vm: &mut Vm, a: &mut [Value]) -> VR {
    let mut args = [Value::Str(sb_to_jstr(&a[0])), a[1].clone(), a[2].clone()];
    str_code_point_count(vm, &mut args)
}
fn sb_offset_by_code_points(vm: &mut Vm, a: &mut [Value]) -> VR {
    let mut args = [Value::Str(sb_to_jstr(&a[0])), a[1].clone(), a[2].clone()];
    str_offset_by_code_points(vm, &mut args)
}

// ------------------------------------------------------------------------------ System

fn sys_current_millis(vm: &mut Vm, _: &mut [Value]) -> VR {
    let _ = vm;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    Ok(Value::Long(now))
}
fn sys_nano_time(_: &mut Vm, _: &mut [Value]) -> VR {
    static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    let s = START.get_or_init(std::time::Instant::now);
    Ok(Value::Long(s.elapsed().as_nanos() as i64))
}
fn sys_arraycopy(_: &mut Vm, a: &mut [Value]) -> VR {
    let (src, sp, dst, dp, n) = (&a[0], int(&a[1]), &a[2], int(&a[3]), int(&a[4]));
    let (Value::Obj(so), Value::Obj(dobj)) = (src, dst) else {
        return Err(npe("Cannot invoke arraycopy with a null array"));
    };
    let (Obj::Array(sa), Obj::Array(da)) = (&**so, &**dobj) else {
        return Err(Throw::new(cls::ARRAY_STORE_EXCEPTION, Some("arraycopy: source type is not an array".into())));
    };
    let (sl, dl) = (sa.len() as i32, da.len() as i32);
    if sp < 0 || dp < 0 || n < 0 || sp + n > sl || dp + n > dl {
        return Err(Throw::new(
            cls::ARRAY_INDEX_OUT_OF_BOUNDS_EXCEPTION,
            Some(format!("arraycopy: last source index {} out of bounds for length {sl}", sp + n)),
        ));
    }
    let vals: Vec<Value> = (0..n).map(|i| sa.data.borrow().get((sp + i) as usize)).collect();
    let mut d = da.data.borrow_mut();
    for (i, v) in vals.into_iter().enumerate() {
        d.set(dp as usize + i, v);
    }
    Ok(Value::Null)
}

// ------------------------------------------------------------------------------ exceptions

fn exception_ctor(class: ClassId) -> impl Fn(&mut Vm, &mut [Value]) -> VR {
    move |_, a| {
        let message = a.first().and_then(|v| if v.is_null() { None } else { Some(v.to_java_string()) });
        Ok(Value::obj(Obj::Exception(JException::new(class, message))))
    }
}

fn ex_get_message(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(match &a[0] {
        Value::Obj(o) => match &**o {
            Obj::Exception(e) => e.message.clone().map(Value::string).unwrap_or(Value::Null),
            _ => Value::Null,
        },
        _ => Value::Null,
    })
}

fn ex_get_stack_trace(_: &mut Vm, _: &mut [Value]) -> VR {
    Ok(Value::obj(Obj::Array(JArray::new(Ty::of(cls::STACK_TRACE_ELEMENT), 0))))
}

macro_rules! exception_ctors {
    ($b:ident, $($c:ident),*) => {
        $(
            {
                fn c0(vm: &mut Vm, a: &mut [Value]) -> VR { exception_ctor(cls::$c)(vm, a) }
                $b.c(cls::$c, "()", c0);
                $b.c(cls::$c, "(String)", c0);
            }
        )*
    };
}

fn enum_name(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::string(a[0].to_java_string()))
}
fn enum_ordinal(_: &mut Vm, a: &mut [Value]) -> VR {
    Ok(Value::Int(match &a[0] {
        Value::Obj(o) => match &**o {
            Obj::Time(t) => t.ordinal(),
            _ => 0,
        },
        _ => 0,
    }))
}

fn unboxed_list(items: Vec<Value>) -> Value {
    JList::array_list(items)
}

pub fn register(b: &mut Builder) {
    use cls::*;
    // hierarchy
    b.supers(OBJECT, &[]);
    b.interface(CHAR_SEQUENCE, &[]);
    b.interface(COMPARABLE, &[]);
    b.interface(ITERABLE, &[]);
    b.interface(APPENDABLE, &[]);
    b.supers(STRING, &[OBJECT, CHAR_SEQUENCE, COMPARABLE]);
    b.supers(NUMBER, &[OBJECT]);
    b.supers(BOOLEAN_BOX, &[OBJECT, COMPARABLE]);
    b.supers(CHARACTER, &[OBJECT, COMPARABLE]);
    for c in [BYTE_BOX, SHORT_BOX, INTEGER, LONG_BOX, FLOAT_BOX, DOUBLE_BOX] {
        b.supers(c, &[NUMBER, COMPARABLE]);
    }
    // both are Comparable to their own class since JDK 11
    b.supers(STRING_BUILDER, &[OBJECT, CHAR_SEQUENCE, APPENDABLE, COMPARABLE]);
    b.supers(STRING_BUFFER, &[OBJECT, CHAR_SEQUENCE, APPENDABLE, COMPARABLE]);
    b.supers(ENUM, &[OBJECT, COMPARABLE]);
    b.supers(THROWABLE, &[OBJECT]);
    b.supers(EXCEPTION, &[THROWABLE]);
    b.supers(RUNTIME_EXCEPTION, &[EXCEPTION]);
    b.supers(LAMBDA_CONVERSION_EXCEPTION, &[EXCEPTION]);
    for c in [
        ARITHMETIC_EXCEPTION,
        WRONG_METHOD_TYPE_EXCEPTION,
        INDEX_OUT_OF_BOUNDS_EXCEPTION,
        ARRAY_STORE_EXCEPTION,
        CLASS_CAST_EXCEPTION,
        ILLEGAL_ARGUMENT_EXCEPTION,
        ILLEGAL_MONITOR_STATE_EXCEPTION,
        ILLEGAL_STATE_EXCEPTION,
        NEGATIVE_ARRAY_SIZE_EXCEPTION,
        NULL_POINTER_EXCEPTION,
        SECURITY_EXCEPTION,
        UNSUPPORTED_OPERATION_EXCEPTION,
        CONCURRENT_MODIFICATION_EXCEPTION,
        NO_SUCH_ELEMENT_EXCEPTION,
        EMPTY_STACK_EXCEPTION,
        DATE_TIME_EXCEPTION,
    ] {
        b.supers(c, &[RUNTIME_EXCEPTION]);
    }
    b.supers(ARRAY_INDEX_OUT_OF_BOUNDS_EXCEPTION, &[INDEX_OUT_OF_BOUNDS_EXCEPTION]);
    b.supers(STRING_INDEX_OUT_OF_BOUNDS_EXCEPTION, &[INDEX_OUT_OF_BOUNDS_EXCEPTION]);
    b.supers(NUMBER_FORMAT_EXCEPTION, &[ILLEGAL_ARGUMENT_EXCEPTION]);
    b.supers(PATTERN_SYNTAX_EXCEPTION, &[ILLEGAL_ARGUMENT_EXCEPTION]);
    b.supers(ILLEGAL_THREAD_STATE_EXCEPTION, &[ILLEGAL_ARGUMENT_EXCEPTION]);
    b.supers(REFLECTIVE_OPERATION_EXCEPTION, &[EXCEPTION]);
    for c in [CLASS_NOT_FOUND_EXCEPTION, ILLEGAL_ACCESS_EXCEPTION, INSTANTIATION_EXCEPTION, NO_SUCH_FIELD_EXCEPTION, NO_SUCH_METHOD_EXCEPTION] {
        b.supers(c, &[REFLECTIVE_OPERATION_EXCEPTION]);
    }
    b.supers(CLONE_NOT_SUPPORTED_EXCEPTION, &[EXCEPTION]);
    b.supers(INTERRUPTED_EXCEPTION, &[EXCEPTION]);
    b.supers(DATE_TIME_PARSE_EXCEPTION, &[DATE_TIME_EXCEPTION]);
    b.supers(UNSUPPORTED_TEMPORAL_TYPE_EXCEPTION, &[DATE_TIME_EXCEPTION]);
    b.supers(ZONE_RULES_EXCEPTION, &[DATE_TIME_EXCEPTION]);
    for c in [ENUM_CONSTANT_NOT_PRESENT_EXCEPTION, TYPE_NOT_PRESENT_EXCEPTION, FORMATTER_CLOSED_EXCEPTION, ILLFORMED_LOCALE_EXCEPTION, MISSING_RESOURCE_EXCEPTION] {
        b.supers(c, &[RUNTIME_EXCEPTION]);
    }
    b.supers(ILLEGAL_FORMAT_EXCEPTION, &[ILLEGAL_ARGUMENT_EXCEPTION]);
    for c in [
        DUPLICATE_FORMAT_FLAGS_EXCEPTION,
        ILLEGAL_FORMAT_ARGUMENT_INDEX_EXCEPTION,
        FORMAT_FLAGS_CONVERSION_MISMATCH_EXCEPTION,
        ILLEGAL_FORMAT_CODE_POINT_EXCEPTION,
        ILLEGAL_FORMAT_CONVERSION_EXCEPTION,
        ILLEGAL_FORMAT_FLAGS_EXCEPTION,
        ILLEGAL_FORMAT_PRECISION_EXCEPTION,
        ILLEGAL_FORMAT_WIDTH_EXCEPTION,
        MISSING_FORMAT_ARGUMENT_EXCEPTION,
        MISSING_FORMAT_WIDTH_EXCEPTION,
        UNKNOWN_FORMAT_CONVERSION_EXCEPTION,
        UNKNOWN_FORMAT_FLAGS_EXCEPTION,
    ] {
        b.supers(c, &[ILLEGAL_FORMAT_EXCEPTION]);
    }
    b.supers(INPUT_MISMATCH_EXCEPTION, &[NO_SUCH_ELEMENT_EXCEPTION]);
    b.supers(TOO_MANY_LISTENERS_EXCEPTION, &[EXCEPTION]);
    b.supers(TEXT_PARSE_EXCEPTION, &[EXCEPTION]);

    // Object
    b.m(OBJECT, "boolean equals(Object)", obj_equals);
    b.m(OBJECT, "int hashCode()", obj_hash);
    b.m(OBJECT, "String toString()", obj_to_string);

    // CharSequence
    b.m(CHAR_SEQUENCE, "char charAt(int)", cs_char_at);
    b.m(CHAR_SEQUENCE, "IntStream chars()", cs_chars);
    b.m(CHAR_SEQUENCE, "IntStream codePoints()", cs_code_points);
    b.m(CHAR_SEQUENCE, "int length()", cs_length);
    b.m(CHAR_SEQUENCE, "CharSequence subSequence(int,int)", cs_sub_sequence);
    b.m(CHAR_SEQUENCE, "String toString()", cs_to_string);
    let m = b.m(CHAR_SEQUENCE, "String replaceAll(Pattern,Function)", crate::regex::cs_replace_all);
    b.aug(m, "java.util.regex.Pattern.matcher(java.lang.CharSequence)");
    let m = b.m(CHAR_SEQUENCE, "String replaceFirst(Pattern,Function)", crate::regex::cs_replace_first);
    b.aug(m, "java.util.regex.Pattern.matcher(java.lang.CharSequence)");
    b.m(COMPARABLE, "int compareTo(def)", compare_values);

    // String
    b.c(STRING, "()", str_new);
    b.m(STRING, "int codePointAt(int)", str_code_point_at);
    b.m(STRING, "int codePointBefore(int)", str_code_point_before);
    b.m(STRING, "int codePointCount(int,int)", str_code_point_count);
    b.m(STRING, "int compareTo(String)", str_compare_to);
    b.m(STRING, "int compareToIgnoreCase(String)", str_compare_ignore_case);
    b.m(STRING, "String concat(String)", str_concat);
    let m = b.m(STRING, "boolean contains(CharSequence)", str_contains);
    b.aug(m, "String.contains(java.lang.CharSequence)");
    b.m(STRING, "boolean contentEquals(CharSequence)", str_content_equals);
    b.s(STRING, "String copyValueOf(char[])", str_copy_value_of);
    b.s(STRING, "String copyValueOf(char[],int,int)", str_copy_value_of3);
    let m = b.m(STRING, "String decodeBase64()", str_decode_base64);
    b.aug(m, "String.getBytes(java.nio.charset.Charset)");
    let m = b.m(STRING, "String encodeBase64()", str_encode_base64);
    b.aug(m, "String.getBytes(java.nio.charset.Charset)");
    let m = b.m(STRING, "String[] splitOnToken(String)", str_split_on_token);
    b.aug(m, "String.length()");
    let m = b.m(STRING, "String[] splitOnToken(String,int)", str_split_on_token2);
    b.aug(m, "String.length()");
    b.m(STRING, "boolean endsWith(String)", str_ends_with);
    b.m(STRING, "boolean equalsIgnoreCase(String)", str_equals_ignore_case);
    b.s(STRING, "String format(Locale,String,def[])", str_format_locale);
    b.s(STRING, "String format(String,def[])", str_format);
    b.m(STRING, "void getChars(int,int,char[],int)", str_get_chars);
    let m = b.m(STRING, "int indexOf(String)", str_index_of);
    b.aug(m, "String.indexOf(String)");
    let m = b.m(STRING, "int indexOf(String,int)", str_index_of2);
    b.aug(m, "String.indexOf(String, int)");
    b.m(STRING, "boolean isEmpty()", str_is_empty);
    b.s(STRING, "String join(CharSequence,Iterable)", str_join);
    let m = b.m(STRING, "int lastIndexOf(String)", str_last_index_of);
    b.aug(m, "String.lastIndexOf(String)");
    let m = b.m(STRING, "int lastIndexOf(String,int)", str_last_index_of2);
    b.aug(m, "String.lastIndexOf(String, int)");
    b.m(STRING, "int offsetByCodePoints(int,int)", str_offset_by_code_points);
    b.m(STRING, "boolean regionMatches(boolean,int,String,int,int)", str_region_matches);
    b.m(STRING, "boolean regionMatches(int,String,int,int)", str_region_matches4);
    let m = b.m(STRING, "String replace(CharSequence,CharSequence)", str_replace);
    b.aug(m, "String.replace(java.lang.CharSequence, java.lang.CharSequence)");
    b.m(STRING, "boolean startsWith(String)", str_starts_with);
    b.m(STRING, "boolean startsWith(String,int)", str_starts_with2);
    b.m(STRING, "String substring(int)", str_substring1);
    b.m(STRING, "String substring(int,int)", str_substring2);
    b.m(STRING, "char[] toCharArray()", str_to_char_array);
    b.m(STRING, "String toLowerCase()", str_lower);
    b.m(STRING, "String toLowerCase(Locale)", str_lower_locale);
    b.m(STRING, "String toUpperCase()", str_upper);
    b.m(STRING, "String toUpperCase(Locale)", str_upper_locale);
    b.m(STRING, "String trim()", str_trim);
    b.s(STRING, "String valueOf(def)", str_value_of);
    let m = b.m(STRING, "String sha1()", str_sha1);
    b.aug(m, "String.getBytes(java.nio.charset.Charset)");
    let m = b.m(STRING, "String sha256()", str_sha256);
    b.aug(m, "String.getBytes(java.nio.charset.Charset)");
    let m = b.m(STRING, "String sha512()", str_sha512);
    b.aug(m, "String.getBytes(java.nio.charset.Charset)");

    // Number and boxes
    b.m(NUMBER, "byte byteValue()", num_byte);
    b.m(NUMBER, "short shortValue()", num_short);
    b.m(NUMBER, "int intValue()", num_int);
    b.m(NUMBER, "long longValue()", num_long);
    b.m(NUMBER, "float floatValue()", num_float);
    b.m(NUMBER, "double doubleValue()", num_double);

    b.sf(BOOLEAN_BOX, "Boolean TRUE", bool_true);
    b.sf(BOOLEAN_BOX, "Boolean FALSE", bool_false);
    b.m(BOOLEAN_BOX, "boolean booleanValue()", bool_value);
    b.s(BOOLEAN_BOX, "int compare(boolean,boolean)", bool_compare);
    b.m(BOOLEAN_BOX, "int compareTo(Boolean)", compare_to);
    b.s(BOOLEAN_BOX, "int hashCode(boolean)", bool_hash);
    b.s(BOOLEAN_BOX, "boolean logicalAnd(boolean,boolean)", bool_and);
    b.s(BOOLEAN_BOX, "boolean logicalOr(boolean,boolean)", bool_or);
    b.s(BOOLEAN_BOX, "boolean logicalXor(boolean,boolean)", bool_xor);
    b.s(BOOLEAN_BOX, "boolean parseBoolean(String)", bool_parse);
    b.s(BOOLEAN_BOX, "String toString(boolean)", to_string_static);
    b.s(BOOLEAN_BOX, "Boolean valueOf(boolean)", bool_value);

    fn byte_bytes(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(1))
    }
    fn byte_max(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Byte(i8::MAX))
    }
    fn byte_min(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Byte(i8::MIN))
    }
    fn byte_size(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(8))
    }
    b.sf(BYTE_BOX, "int BYTES", byte_bytes);
    b.sf(BYTE_BOX, "byte MAX_VALUE", byte_max);
    b.sf(BYTE_BOX, "byte MIN_VALUE", byte_min);
    b.sf(BYTE_BOX, "int SIZE", byte_size);
    b.s(BYTE_BOX, "int compare(byte,byte)", small_compare);
    b.m(BYTE_BOX, "int compareTo(Byte)", compare_to);
    b.s(BYTE_BOX, "Byte decode(String)", byte_decode);
    b.s(BYTE_BOX, "int hashCode(byte)", small_hash);
    b.s(BYTE_BOX, "byte parseByte(String)", byte_parse);
    b.s(BYTE_BOX, "byte parseByte(String,int)", byte_parse_radix);
    b.s(BYTE_BOX, "String toString(byte)", to_string_static);
    b.s(BYTE_BOX, "int toUnsignedInt(byte)", byte_to_unsigned_int);
    b.s(BYTE_BOX, "long toUnsignedLong(byte)", byte_to_unsigned_long);
    b.s(BYTE_BOX, "Byte valueOf(byte)", byte_value_of);
    b.s(BYTE_BOX, "Byte valueOf(String,int)", byte_value_of_radix);

    fn short_bytes(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(2))
    }
    fn short_max(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Short(i16::MAX))
    }
    fn short_min(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Short(i16::MIN))
    }
    fn short_size(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(16))
    }
    b.sf(SHORT_BOX, "int BYTES", short_bytes);
    b.sf(SHORT_BOX, "short MAX_VALUE", short_max);
    b.sf(SHORT_BOX, "short MIN_VALUE", short_min);
    b.sf(SHORT_BOX, "int SIZE", short_size);
    b.s(SHORT_BOX, "int compare(short,short)", small_compare);
    b.m(SHORT_BOX, "int compareTo(Short)", compare_to);
    b.s(SHORT_BOX, "Short decode(String)", short_decode);
    b.s(SHORT_BOX, "int hashCode(short)", small_hash);
    b.s(SHORT_BOX, "short parseShort(String)", short_parse);
    b.s(SHORT_BOX, "short parseShort(String,int)", short_parse_radix);
    b.s(SHORT_BOX, "short reverseBytes(short)", short_reverse_bytes);
    b.s(SHORT_BOX, "String toString(short)", to_string_static);
    b.s(SHORT_BOX, "int toUnsignedInt(short)", short_to_unsigned_int);
    b.s(SHORT_BOX, "long toUnsignedLong(short)", short_to_unsigned_long);
    b.s(SHORT_BOX, "Short valueOf(short)", short_value_of);
    b.s(SHORT_BOX, "Short valueOf(String,int)", short_value_of_radix);

    fn int_bytes(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(4))
    }
    fn int_max_v(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(i32::MAX))
    }
    fn int_min_v(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(i32::MIN))
    }
    fn int_size(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(32))
    }
    b.sf(INTEGER, "int BYTES", int_bytes);
    b.sf(INTEGER, "int MAX_VALUE", int_max_v);
    b.sf(INTEGER, "int MIN_VALUE", int_min_v);
    b.sf(INTEGER, "int SIZE", int_size);
    b.s(INTEGER, "int bitCount(int)", int_bit_count);
    b.s(INTEGER, "int compare(int,int)", int_compare);
    b.m(INTEGER, "int compareTo(Integer)", compare_to);
    b.s(INTEGER, "int compareUnsigned(int,int)", int_compare_unsigned);
    b.s(INTEGER, "Integer decode(String)", int_decode);
    b.s(INTEGER, "int divideUnsigned(int,int)", int_divide_unsigned);
    b.s(INTEGER, "int hashCode(int)", int_hash);
    b.s(INTEGER, "int highestOneBit(int)", int_highest_one_bit);
    b.s(INTEGER, "int lowestOneBit(int)", int_lowest_one_bit);
    b.s(INTEGER, "int max(int,int)", int_max);
    b.s(INTEGER, "int min(int,int)", int_min);
    b.s(INTEGER, "int numberOfLeadingZeros(int)", int_nlz);
    b.s(INTEGER, "int numberOfTrailingZeros(int)", int_ntz);
    b.s(INTEGER, "int parseInt(String)", int_parse);
    b.s(INTEGER, "int parseInt(String,int)", int_parse_radix);
    b.s(INTEGER, "int parseUnsignedInt(String)", int_parse_unsigned);
    b.s(INTEGER, "int parseUnsignedInt(String,int)", int_parse_unsigned_radix);
    b.s(INTEGER, "int remainderUnsigned(int,int)", int_remainder_unsigned);
    b.s(INTEGER, "int reverse(int)", int_reverse);
    b.s(INTEGER, "int reverseBytes(int)", int_reverse_bytes);
    b.s(INTEGER, "int rotateLeft(int,int)", int_rotate_left);
    b.s(INTEGER, "int rotateRight(int,int)", int_rotate_right);
    b.s(INTEGER, "int signum(int)", int_signum);
    b.s(INTEGER, "String toBinaryString(int)", int_to_binary);
    b.s(INTEGER, "String toHexString(int)", int_to_hex);
    b.s(INTEGER, "String toOctalString(int)", int_to_octal);
    b.s(INTEGER, "String toString(int)", to_string_static);
    b.s(INTEGER, "String toString(int,int)", int_to_string_radix);
    b.s(INTEGER, "long toUnsignedLong(int)", int_to_unsigned_long);
    b.s(INTEGER, "String toUnsignedString(int)", int_unsigned_string);
    b.s(INTEGER, "String toUnsignedString(int,int)", int_unsigned_string_radix);
    b.s(INTEGER, "Integer valueOf(int)", int_value_of);
    b.s(INTEGER, "Integer valueOf(String,int)", int_value_of_radix);

    fn long_bytes(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(8))
    }
    fn long_max_v(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Long(i64::MAX))
    }
    fn long_min_v(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Long(i64::MIN))
    }
    fn long_size(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(64))
    }
    b.sf(LONG_BOX, "int BYTES", long_bytes);
    b.sf(LONG_BOX, "long MAX_VALUE", long_max_v);
    b.sf(LONG_BOX, "long MIN_VALUE", long_min_v);
    b.sf(LONG_BOX, "int SIZE", long_size);
    b.s(LONG_BOX, "int bitCount(long)", long_bit_count);
    b.s(LONG_BOX, "int compare(long,long)", long_compare);
    b.m(LONG_BOX, "int compareTo(Long)", compare_to);
    b.s(LONG_BOX, "int compareUnsigned(long,long)", long_compare_unsigned);
    b.s(LONG_BOX, "Long decode(String)", long_decode);
    b.s(LONG_BOX, "long divideUnsigned(long,long)", long_divide_unsigned);
    b.s(LONG_BOX, "int hashCode(long)", long_hash);
    b.s(LONG_BOX, "long highestOneBit(long)", long_highest_one_bit);
    b.s(LONG_BOX, "long lowestOneBit(long)", long_lowest_one_bit);
    b.s(LONG_BOX, "long max(long,long)", long_max);
    b.s(LONG_BOX, "long min(long,long)", long_min);
    b.s(LONG_BOX, "int numberOfLeadingZeros(long)", long_nlz);
    b.s(LONG_BOX, "int numberOfTrailingZeros(long)", long_ntz);
    b.s(LONG_BOX, "long parseLong(String)", long_parse);
    b.s(LONG_BOX, "long parseLong(String,int)", long_parse_radix);
    b.s(LONG_BOX, "long parseUnsignedLong(String)", long_parse_unsigned);
    b.s(LONG_BOX, "long parseUnsignedLong(String,int)", long_parse_unsigned_radix);
    b.s(LONG_BOX, "long remainderUnsigned(long,long)", long_remainder_unsigned);
    b.s(LONG_BOX, "long reverse(long)", long_reverse);
    b.s(LONG_BOX, "long reverseBytes(long)", long_reverse_bytes);
    b.s(LONG_BOX, "long rotateLeft(long,int)", long_rotate_left);
    b.s(LONG_BOX, "long rotateRight(long,int)", long_rotate_right);
    b.s(LONG_BOX, "int signum(long)", long_signum);
    b.s(LONG_BOX, "long sum(long,long)", long_sum);
    b.s(LONG_BOX, "String toBinaryString(long)", long_to_binary);
    b.s(LONG_BOX, "String toHexString(long)", long_to_hex);
    b.s(LONG_BOX, "String toOctalString(long)", long_to_octal);
    b.s(LONG_BOX, "String toString(long)", to_string_static);
    b.s(LONG_BOX, "String toString(long,int)", long_to_string_radix);
    b.s(LONG_BOX, "String toUnsignedString(long)", long_unsigned_string);
    b.s(LONG_BOX, "String toUnsignedString(long,int)", long_unsigned_string_radix);
    b.s(LONG_BOX, "Long valueOf(long)", long_value_of);
    b.s(LONG_BOX, "Long valueOf(String,int)", long_value_of_radix);

    fn d_bytes(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(8))
    }
    fn d_max_exp(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(1023))
    }
    fn d_max(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Double(f64::MAX))
    }
    fn d_min_exp(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(-1022))
    }
    fn d_min_normal(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Double(f64::MIN_POSITIVE))
    }
    fn d_min(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Double(f64::from_bits(1)))
    }
    fn d_nan(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Double(f64::NAN))
    }
    fn d_ninf(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Double(f64::NEG_INFINITY))
    }
    fn d_pinf(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Double(f64::INFINITY))
    }
    fn d_size(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(64))
    }
    b.sf(DOUBLE_BOX, "int BYTES", d_bytes);
    b.sf(DOUBLE_BOX, "int MAX_EXPONENT", d_max_exp);
    b.sf(DOUBLE_BOX, "double MAX_VALUE", d_max);
    b.sf(DOUBLE_BOX, "int MIN_EXPONENT", d_min_exp);
    b.sf(DOUBLE_BOX, "double MIN_NORMAL", d_min_normal);
    b.sf(DOUBLE_BOX, "double MIN_VALUE", d_min);
    b.sf(DOUBLE_BOX, "double NaN", d_nan);
    b.sf(DOUBLE_BOX, "double NEGATIVE_INFINITY", d_ninf);
    b.sf(DOUBLE_BOX, "double POSITIVE_INFINITY", d_pinf);
    b.sf(DOUBLE_BOX, "int SIZE", d_size);
    b.s(DOUBLE_BOX, "int compare(double,double)", double_compare);
    b.m(DOUBLE_BOX, "int compareTo(Double)", compare_to);
    b.s(DOUBLE_BOX, "long doubleToLongBits(double)", double_to_long_bits);
    b.s(DOUBLE_BOX, "long doubleToRawLongBits(double)", double_to_raw_long_bits);
    b.s(DOUBLE_BOX, "int hashCode(double)", double_hash);
    b.s(DOUBLE_BOX, "boolean isFinite(double)", double_is_finite);
    b.m(DOUBLE_BOX, "boolean isInfinite()", double_is_infinite);
    b.s(DOUBLE_BOX, "boolean isInfinite(double)", double_is_infinite);
    b.m(DOUBLE_BOX, "boolean isNaN()", double_is_nan);
    b.s(DOUBLE_BOX, "boolean isNaN(double)", double_is_nan);
    b.s(DOUBLE_BOX, "double longBitsToDouble(long)", double_from_bits);
    b.s(DOUBLE_BOX, "double max(double,double)", double_max);
    b.s(DOUBLE_BOX, "double min(double,double)", double_min);
    b.s(DOUBLE_BOX, "double parseDouble(String)", double_parse);
    b.s(DOUBLE_BOX, "double sum(double,double)", double_sum);
    b.s(DOUBLE_BOX, "String toHexString(double)", double_to_hex);
    b.s(DOUBLE_BOX, "String toString(double)", double_to_string);
    b.s(DOUBLE_BOX, "Double valueOf(double)", double_value_of);

    fn f_bytes(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(4))
    }
    fn f_max_exp(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(127))
    }
    fn f_max(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Float(f32::MAX))
    }
    fn f_min_exp(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(-126))
    }
    fn f_min_normal(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Float(f32::MIN_POSITIVE))
    }
    fn f_min(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Float(f32::from_bits(1)))
    }
    fn f_nan(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Float(f32::NAN))
    }
    fn f_ninf(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Float(f32::NEG_INFINITY))
    }
    fn f_pinf(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Float(f32::INFINITY))
    }
    fn f_size(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(32))
    }
    b.sf(FLOAT_BOX, "int BYTES", f_bytes);
    b.sf(FLOAT_BOX, "int MAX_EXPONENT", f_max_exp);
    b.sf(FLOAT_BOX, "float MAX_VALUE", f_max);
    b.sf(FLOAT_BOX, "int MIN_EXPONENT", f_min_exp);
    b.sf(FLOAT_BOX, "float MIN_NORMAL", f_min_normal);
    b.sf(FLOAT_BOX, "float MIN_VALUE", f_min);
    b.sf(FLOAT_BOX, "float NaN", f_nan);
    b.sf(FLOAT_BOX, "float NEGATIVE_INFINITY", f_ninf);
    b.sf(FLOAT_BOX, "float POSITIVE_INFINITY", f_pinf);
    b.sf(FLOAT_BOX, "int SIZE", f_size);
    b.s(FLOAT_BOX, "int compare(float,float)", float_compare);
    b.m(FLOAT_BOX, "int compareTo(Float)", compare_to);
    b.s(FLOAT_BOX, "int floatToIntBits(float)", float_to_int_bits);
    b.s(FLOAT_BOX, "int floatToRawIntBits(float)", float_to_raw_int_bits);
    b.s(FLOAT_BOX, "int hashCode(float)", float_hash);
    b.s(FLOAT_BOX, "float intBitsToFloat(int)", float_from_bits);
    b.s(FLOAT_BOX, "boolean isFinite(float)", double_is_finite);
    b.m(FLOAT_BOX, "boolean isInfinite()", double_is_infinite);
    b.s(FLOAT_BOX, "boolean isInfinite(float)", double_is_infinite);
    b.m(FLOAT_BOX, "boolean isNaN()", double_is_nan);
    b.s(FLOAT_BOX, "boolean isNaN(float)", double_is_nan);
    b.s(FLOAT_BOX, "float max(float,float)", float_max);
    b.s(FLOAT_BOX, "float min(float,float)", float_min);
    b.s(FLOAT_BOX, "float parseFloat(String)", float_parse);
    b.s(FLOAT_BOX, "float sum(float,float)", float_sum);
    b.s(FLOAT_BOX, "String toHexString(float)", float_to_hex);
    b.s(FLOAT_BOX, "String toString(float)", float_to_string);
    b.s(FLOAT_BOX, "Float valueOf(float)", float_value_of);

    // Character
    fn c_bytes(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(2))
    }
    fn c_max_cp(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(0x10FFFF))
    }
    fn c_min_cp(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(0))
    }
    fn c_max_radix(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(36))
    }
    fn c_min_radix(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(2))
    }
    fn c_max_value(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Char(0xFFFF))
    }
    fn c_min_value(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Char(0))
    }
    fn c_min_supp(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(0x10000))
    }
    fn c_size(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Int(16))
    }
    fn c_min_hi(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Char(0xD800))
    }
    fn c_max_hi(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Char(0xDBFF))
    }
    fn c_min_lo(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Char(0xDC00))
    }
    fn c_max_lo(_: &mut Vm, _: &mut [Value]) -> VR {
        Ok(Value::Char(0xDFFF))
    }
    b.sf(CHARACTER, "int BYTES", c_bytes);
    b.sf(CHARACTER, "int MAX_CODE_POINT", c_max_cp);
    b.sf(CHARACTER, "int MIN_CODE_POINT", c_min_cp);
    b.sf(CHARACTER, "int MAX_RADIX", c_max_radix);
    b.sf(CHARACTER, "int MIN_RADIX", c_min_radix);
    b.sf(CHARACTER, "char MAX_VALUE", c_max_value);
    b.sf(CHARACTER, "char MIN_VALUE", c_min_value);
    b.sf(CHARACTER, "int MIN_SUPPLEMENTARY_CODE_POINT", c_min_supp);
    b.sf(CHARACTER, "int SIZE", c_size);
    b.sf(CHARACTER, "char MIN_HIGH_SURROGATE", c_min_hi);
    b.sf(CHARACTER, "char MAX_HIGH_SURROGATE", c_max_hi);
    b.sf(CHARACTER, "char MIN_LOW_SURROGATE", c_min_lo);
    b.sf(CHARACTER, "char MAX_LOW_SURROGATE", c_max_lo);
    b.sf(CHARACTER, "char MIN_SURROGATE", c_min_hi);
    b.sf(CHARACTER, "char MAX_SURROGATE", c_max_lo);
    b.s(CHARACTER, "int charCount(int)", char_char_count);
    b.m(CHARACTER, "char charValue()", identity);
    b.s(CHARACTER, "int codePointAt(CharSequence,int)", char_code_point_at_cs);
    b.s(CHARACTER, "int compare(char,char)", char_compare);
    b.m(CHARACTER, "int compareTo(Character)", compare_to);
    b.s(CHARACTER, "int digit(int,int)", char_digit);
    b.s(CHARACTER, "char forDigit(int,int)", char_for_digit);
    b.s(CHARACTER, "String getName(int)", char_get_name);
    b.s(CHARACTER, "int getNumericValue(int)", char_numeric_value);
    b.s(CHARACTER, "int getType(int)", char_get_type);
    b.s(CHARACTER, "int hashCode(char)", small_hash);
    b.s(CHARACTER, "char highSurrogate(int)", char_high_surrogate);
    b.s(CHARACTER, "boolean isAlphabetic(int)", char_is_alphabetic);
    b.s(CHARACTER, "boolean isBmpCodePoint(int)", char_is_bmp);
    b.s(CHARACTER, "boolean isDefined(int)", char_is_defined);
    b.s(CHARACTER, "boolean isDigit(int)", char_is_digit);
    b.s(CHARACTER, "boolean isHighSurrogate(char)", char_is_high_surrogate);
    b.s(CHARACTER, "boolean isIdentifierIgnorable(int)", char_is_id_ignorable);
    b.s(CHARACTER, "boolean isIdeographic(int)", char_is_ideographic);
    b.s(CHARACTER, "boolean isISOControl(int)", char_is_iso_control);
    b.s(CHARACTER, "boolean isJavaIdentifierPart(int)", char_is_java_id_part);
    b.s(CHARACTER, "boolean isJavaIdentifierStart(int)", char_is_java_id_start);
    b.s(CHARACTER, "boolean isLetter(int)", char_is_letter);
    b.s(CHARACTER, "boolean isLetterOrDigit(int)", char_is_letter_or_digit);
    b.s(CHARACTER, "boolean isLowerCase(int)", char_is_lower);
    b.s(CHARACTER, "boolean isMirrored(int)", char_is_mirrored);
    b.s(CHARACTER, "boolean isSpaceChar(int)", char_is_space_char);
    b.s(CHARACTER, "boolean isSupplementaryCodePoint(int)", char_is_supplementary);
    b.s(CHARACTER, "boolean isSurrogate(char)", char_is_surrogate);
    b.s(CHARACTER, "boolean isSurrogatePair(char,char)", char_is_surrogate_pair);
    b.s(CHARACTER, "boolean isTitleCase(int)", char_is_title_case);
    b.s(CHARACTER, "boolean isUnicodeIdentifierPart(int)", char_is_unicode_id_part);
    b.s(CHARACTER, "boolean isUnicodeIdentifierStart(int)", char_is_unicode_id_start);
    b.s(CHARACTER, "boolean isUpperCase(int)", char_is_upper);
    b.s(CHARACTER, "boolean isValidCodePoint(int)", char_is_valid_cp);
    b.s(CHARACTER, "boolean isWhitespace(int)", char_is_whitespace);
    b.s(CHARACTER, "char lowSurrogate(int)", char_low_surrogate);
    b.s(CHARACTER, "char reverseBytes(char)", char_reverse_bytes);
    b.s(CHARACTER, "char[] toChars(int)", char_to_chars);
    b.s(CHARACTER, "int toCodePoint(char,char)", char_to_code_point);
    b.s(CHARACTER, "char toLowerCase(char)", char_to_lower);
    b.s(CHARACTER, "String toString(char)", char_to_string);
    b.s(CHARACTER, "char toTitleCase(char)", char_to_title);
    b.s(CHARACTER, "char toUpperCase(char)", char_to_upper);
    b.s(CHARACTER, "Character valueOf(char)", char_value_of);

    // Math / StrictMath
    for c in [MATH, STRICT_MATH] {
        let strict = c == STRICT_MATH;
        b.sf(c, "double E", math_e);
        b.sf(c, "double PI", math_pi);
        b.s(c, "double abs(double)", math_abs);
        b.s(c, "double acos(double)", math_acos);
        b.s(c, "double asin(double)", math_asin);
        b.s(c, "double atan(double)", math_atan);
        b.s(c, "double atan2(double,double)", math_atan2);
        b.s(c, "double cbrt(double)", math_cbrt);
        b.s(c, "double ceil(double)", math_ceil);
        b.s(c, "double copySign(double,double)", math_copy_sign);
        b.s(c, "double cos(double)", if strict { math_cos } else { math_cos_i });
        b.s(c, "double cosh(double)", math_cosh);
        b.s(c, "double exp(double)", if strict { math_exp } else { math_exp_i });
        b.s(c, "double expm1(double)", math_expm1);
        b.s(c, "double floor(double)", math_floor);
        b.s(c, "double hypot(double,double)", math_hypot);
        b.s(c, "double IEEEremainder(double,double)", math_ieee_rem);
        b.s(c, "double log(double)", if strict { math_log } else { math_log_i });
        b.s(c, "double log10(double)", if strict { math_log10 } else { math_log10_i });
        b.s(c, "double log1p(double)", math_log1p);
        b.s(c, "double max(double,double)", math_max);
        b.s(c, "double min(double,double)", math_min);
        b.s(c, "double nextAfter(double,double)", math_next_after);
        b.s(c, "double nextDown(double)", math_next_down);
        b.s(c, "double nextUp(double)", math_next_up);
        b.s(c, "double pow(double,double)", if strict { math_pow } else { math_pow_i });
        let m = b.s(c, "double random()", math_random);
        b.nondeterministic(m);
        b.s(c, "double rint(double)", math_rint);
        b.s(c, "long round(double)", math_round);
        b.s(c, "double scalb(double,int)", math_scalb);
        b.s(c, "double signum(double)", math_signum);
        b.s(c, "double sin(double)", if strict { math_sin } else { math_sin_i });
        b.s(c, "double sinh(double)", math_sinh);
        b.s(c, "double sqrt(double)", math_sqrt);
        b.s(c, "double tan(double)", if strict { math_tan } else { math_tan_i });
        b.s(c, "double tanh(double)", math_tanh);
        b.s(c, "double toDegrees(double)", math_to_degrees);
        b.s(c, "double toRadians(double)", math_to_radians);
        b.s(c, "double ulp(double)", math_ulp);
    }

    // StringBuilder / StringBuffer
    b.c(STRING_BUILDER, "()", sb_new);
    b.c(STRING_BUILDER, "(CharSequence)", sb_new_cs);
    b.c(STRING_BUFFER, "()", sbuf_new);
    b.c(STRING_BUFFER, "(CharSequence)", sbuf_new_cs);
    for (c, n) in [(STRING_BUILDER, "StringBuilder"), (STRING_BUFFER, "StringBuffer")] {
        b.m(c, &format!("{n} append(def)"), sb_append);
        b.m(c, &format!("{n} append(CharSequence,int,int)"), sb_append_range);
        b.m(c, &format!("{n} appendCodePoint(int)"), sb_append_code_point);
        b.m(c, "int capacity()", sb_capacity);
        b.m(c, "int compareTo(def)", sb_compare_to);
        b.m(c, "int codePointAt(int)", sb_code_point_at);
        b.m(c, "int codePointBefore(int)", sb_code_point_before);
        b.m(c, "int codePointCount(int,int)", sb_code_point_count);
        b.m(c, &format!("{n} delete(int,int)"), sb_delete);
        b.m(c, &format!("{n} deleteCharAt(int)"), sb_delete_char_at);
        b.m(c, "void getChars(int,int,char[],int)", str_get_chars);
        b.m(c, "int indexOf(String)", sb_index_of);
        b.m(c, "int indexOf(String,int)", sb_index_of2);
        b.m(c, &format!("{n} insert(int,def)"), sb_insert);
        b.m(c, "int lastIndexOf(String)", sb_last_index_of);
        b.m(c, "int lastIndexOf(String,int)", sb_last_index_of2);
        b.m(c, "int offsetByCodePoints(int,int)", sb_offset_by_code_points);
        b.m(c, &format!("{n} replace(int,int,String)"), sb_replace);
        b.m(c, &format!("{n} reverse()"), sb_reverse);
        b.m(c, "void setCharAt(int,char)", sb_set_char_at);
        b.m(c, "void setLength(int)", sb_set_length);
        b.m(c, "String substring(int)", sb_substring);
        b.m(c, "String substring(int,int)", sb_substring2);
    }

    // System
    b.s(SYSTEM, "void arraycopy(Object,int,Object,int,int)", sys_arraycopy);
    let m = b.s(SYSTEM, "long currentTimeMillis()", sys_current_millis);
    b.nondeterministic(m);
    let m = b.s(SYSTEM, "long nanoTime()", sys_nano_time);
    b.nondeterministic(m);

    // Enum
    b.m(ENUM, "int compareTo(Enum)", compare_values);
    b.m(ENUM, "String name()", enum_name);
    b.m(ENUM, "int ordinal()", enum_ordinal);

    // exceptions
    b.m(EXCEPTION, "String getLocalizedMessage()", ex_get_message);
    b.m(EXCEPTION, "String getMessage()", ex_get_message);
    b.m(EXCEPTION, "StackTraceElement[] getStackTrace()", ex_get_stack_trace);
    exception_ctors!(
        b,
        EXCEPTION,
        RUNTIME_EXCEPTION,
        ARITHMETIC_EXCEPTION,
        ARRAY_INDEX_OUT_OF_BOUNDS_EXCEPTION,
        ARRAY_STORE_EXCEPTION,
        CLASS_CAST_EXCEPTION,
        CLASS_NOT_FOUND_EXCEPTION,
        CLONE_NOT_SUPPORTED_EXCEPTION,
        ILLEGAL_ACCESS_EXCEPTION,
        ILLEGAL_ARGUMENT_EXCEPTION,
        ILLEGAL_MONITOR_STATE_EXCEPTION,
        ILLEGAL_STATE_EXCEPTION,
        ILLEGAL_THREAD_STATE_EXCEPTION,
        INDEX_OUT_OF_BOUNDS_EXCEPTION,
        INSTANTIATION_EXCEPTION,
        INTERRUPTED_EXCEPTION,
        NEGATIVE_ARRAY_SIZE_EXCEPTION,
        NO_SUCH_FIELD_EXCEPTION,
        NO_SUCH_METHOD_EXCEPTION,
        NULL_POINTER_EXCEPTION,
        NUMBER_FORMAT_EXCEPTION,
        REFLECTIVE_OPERATION_EXCEPTION,
        STRING_INDEX_OUT_OF_BOUNDS_EXCEPTION,
        UNSUPPORTED_OPERATION_EXCEPTION,
        CONCURRENT_MODIFICATION_EXCEPTION,
        NO_SUCH_ELEMENT_EXCEPTION
    );
    fn ese_new(vm: &mut Vm, a: &mut [Value]) -> VR {
        exception_ctor(cls::EMPTY_STACK_EXCEPTION)(vm, a)
    }
    b.c(EMPTY_STACK_EXCEPTION, "()", ese_new);
    fn dte_new(vm: &mut Vm, a: &mut [Value]) -> VR {
        exception_ctor(cls::DATE_TIME_EXCEPTION)(vm, a)
    }
    b.c(DATE_TIME_EXCEPTION, "(String)", dte_new);
    fn zre_new(vm: &mut Vm, a: &mut [Value]) -> VR {
        exception_ctor(cls::ZONE_RULES_EXCEPTION)(vm, a)
    }
    b.c(ZONE_RULES_EXCEPTION, "(String)", zre_new);
    exception_ctors!(b, ILLFORMED_LOCALE_EXCEPTION, INPUT_MISMATCH_EXCEPTION, TOO_MANY_LISTENERS_EXCEPTION);
    macro_rules! string_ctor {
        ($($c:ident),*) => {$(
            b.c($c, "(String)", |vm, a| exception_ctor(cls::$c)(vm, a));
        )*};
    }
    string_ctor!(
        DUPLICATE_FORMAT_FLAGS_EXCEPTION,
        ILLEGAL_FORMAT_FLAGS_EXCEPTION,
        MISSING_FORMAT_ARGUMENT_EXCEPTION,
        MISSING_FORMAT_WIDTH_EXCEPTION,
        UNKNOWN_FORMAT_CONVERSION_EXCEPTION,
        UNKNOWN_FORMAT_FLAGS_EXCEPTION
    );
    b.c(FORMATTER_CLOSED_EXCEPTION, "()", |vm, a| exception_ctor(cls::FORMATTER_CLOSED_EXCEPTION)(vm, a));
    let _ = unboxed_list;
}
