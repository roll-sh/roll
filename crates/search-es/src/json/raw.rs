//! Pre-rendered JSON inside a [`Value`] tree: a stored `_source` returned as the bytes it was
//! indexed with.
//!
//! A search page of 1,000 hits spends most of its time parsing every hit's `_source` into a
//! tree only to print it again; Elasticsearch copies the stored bytes into a compact JSON
//! response as they are (its pretty, YAML and CBOR responses re-read them). [`raw`] wraps the
//! text in a string value tagged with a per-process random marker, so the tree keeps its shape
//! and type everywhere it travels; the JSON writer splices the text in compact output and
//! parses it for pretty output, and [`materialize`] turns raw values back into trees for the
//! other renderers and for `filter_path`, which look inside them.
//!
//! Raw values are created only where the whole response is rendered by the REST layer (the
//! fetch phase of `_search` and `_search/scroll`, see [`with_enabled`]); code that reads hits
//! back as trees (aggregations, OpenSearch search pipelines, by-query APIs) never sees one.
//! The marker is random per process and starts with a NUL, so no indexed or requested string
//! can be mistaken for one.

use super::Value;
use std::cell::Cell;
use std::sync::OnceLock;

fn marker() -> &'static str {
    static M: OnceLock<String> = OnceLock::new();
    M.get_or_init(|| format!("\u{0}roll-raw-json:{:016x}{:016x}\u{0}", rand::random::<u64>(), rand::random::<u64>()))
}

/// A value printing as `json` (one valid JSON value) in compact output.
pub fn raw(json: &str) -> Value {
    let m = marker();
    let mut s = String::with_capacity(m.len() + json.len());
    s.push_str(m);
    s.push_str(json);
    Value::Str(s)
}

/// A buffer to write one raw value's JSON text into (it starts with the marker), turned into
/// the value by [`from_buffer`] without the copy [`raw`] makes of a large text.
pub fn buffer(cap: usize) -> Vec<u8> {
    let m = marker();
    let mut b = Vec::with_capacity(m.len() + cap);
    b.extend_from_slice(m.as_bytes());
    b
}

/// The raw value of a [`buffer`] holding one valid JSON value after the marker.
pub fn from_buffer(b: Vec<u8>) -> Value {
    match String::from_utf8(b) {
        Ok(s) => Value::Str(s),
        // The JSON writers emit UTF-8 only; keep the value valid regardless.
        Err(e) => {
            let text = String::from_utf8_lossy(&e.as_bytes()[marker().len()..]).into_owned();
            raw(&text)
        }
    }
}

/// The JSON text of a raw value.
#[inline]
pub fn as_raw(s: &str) -> Option<&str> {
    // Plain strings almost never start with NUL: one byte compared on the common path.
    if s.as_bytes().first() != Some(&0) {
        return None;
    }
    s.strip_prefix(marker())
}

/// Whether `v` holds a raw value anywhere.
pub fn contains_raw(v: &Value) -> bool {
    match v {
        Value::Str(s) => as_raw(s).is_some(),
        Value::Arr(a) => a.iter().any(contains_raw),
        Value::Obj(m) => m.values().any(contains_raw),
        _ => false,
    }
}

/// Replaces every raw value of `v` with its parsed tree.
pub fn materialize(v: &mut Value) {
    match v {
        Value::Str(s) => {
            if let Some(text) = as_raw(s) {
                *v = parse(text);
            }
        }
        Value::Arr(a) => a.iter_mut().for_each(materialize),
        Value::Obj(m) => m.values_mut().for_each(materialize),
        _ => {}
    }
}

/// The tree of a raw value's text (valid JSON by construction; `null` otherwise).
pub fn parse(text: &str) -> Value {
    super::parse(text.as_bytes()).ok().flatten().unwrap_or(Value::Null)
}

thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
}

/// Runs `f` with raw values allowed in the hits it renders (the REST `_search` and scroll
/// handlers, whose response goes straight to the REST renderer).
pub fn with_enabled<R>(on: bool, f: impl FnOnce() -> R) -> R {
    let prev = ENABLED.with(|e| e.replace(on));
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            ENABLED.with(|e| e.set(self.0));
        }
    }
    let _restore = Restore(prev);
    f()
}

/// Whether [`with_enabled`] allows raw values on this thread.
pub fn enabled() -> bool {
    ENABLED.with(|e| e.get())
}

/// The end of the first JSON value in `b` (which starts with `{`), if everything after it is
/// whitespace. Stored sources were valid when indexed; this only guards the splice against
/// trailing content the document parser ignored.
pub fn single_object_end(b: &[u8]) -> Option<usize> {
    if b.first() != Some(&b'{') {
        return None;
    }
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'"' => {
                // Strings are most of a source: jump between quotes and escapes with memchr.
                i += 1;
                loop {
                    i += memchr::memchr2(b'"', b'\\', b.get(i..)?)?;
                    if b[i] == b'"' {
                        break;
                    }
                    i += 2;
                }
            }
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let end = i + 1;
                    return b[end..].iter().all(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r')).then_some(end);
                }
            }
            // Comments would have to be stripped from the output.
            b'/' => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::write::write_json;

    fn render(v: &Value, pretty: bool) -> String {
        let mut out = Vec::new();
        write_json(&mut out, v, pretty);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn compact_output_splices_and_pretty_output_parses() {
        let src = r#"{ "a" :  1, "b":"xé\"q", "c": 1.50e1 }"#;
        let mut hit = crate::json::Map::new();
        hit.insert("_id".into(), Value::from("1"));
        hit.insert("_source".into(), raw(src));
        let v = Value::Arr(vec![Value::Obj(hit)]);
        assert_eq!(render(&v, false), format!(r#"[{{"_id":"1","_source":{src}}}]"#));
        let mut tree = v.clone();
        materialize(&mut tree);
        assert!(!contains_raw(&tree) && contains_raw(&v));
        assert_eq!(render(&v, true), render(&tree, true));
        assert_eq!(render(&tree, false), r#"[{"_id":"1","_source":{"a":1,"b":"xé\"q","c":15.0}}]"#);
    }

    #[test]
    fn ordinary_strings_are_never_raw() {
        for s in ["", "\u{0}", "\u{0}roll-raw-json:", "{\"a\":1}"] {
            assert!(as_raw(s).is_none(), "{s:?}");
            assert_eq!(render(&Value::from(s), false), crate::json::Value::from(s).to_json());
        }
    }

    #[test]
    fn single_objects_are_recognized() {
        assert_eq!(single_object_end(br#"{"a":"}"}"#), Some(9));
        assert_eq!(single_object_end(b"{\"a\":[1,{\"b\":\"\\\"}\"}]} \n"), Some(21));
        assert_eq!(single_object_end(br#"{"a":1} {"b":2}"#), None);
        assert_eq!(single_object_end(br#"{"a":1} x"#), None);
        assert_eq!(single_object_end(br#"{"a":1 /* c */}"#), None);
        assert_eq!(single_object_end(br#"[1]"#), None);
        assert_eq!(single_object_end(br#"{"a":1"#), None);
        assert_eq!(single_object_end(br#"{"a":"x"#), None);
        assert_eq!(single_object_end(b"{\"a\":\"x\\"), None);
        assert_eq!(single_object_end(b"{\"a\":\"\\\\\"}"), Some(10));
        assert_eq!(single_object_end(b"{\"a\":\"\\\"\\\\\",\"b\":[\"]\"]}"), Some(22));
    }
}
