//! JSON read as text, never as a tree: a source's documents and objects can hold millions of
//! values, and a tree of them costs many times their bytes. [`one_object`] checks a document's
//! text is exactly one JSON object (what a `_bulk` source line must be) and [`values_text`]
//! lists a JSON file's values (an `s3` object's text), both in one pass with memory bounded by
//! the nesting depth.

use std::borrow::Cow;

/// Nesting deeper than this isn't read.
const MAX_DEPTH: usize = 512;

/// A scalar the scanner passed (its raw text: a string with its quotes and escapes).
enum Scalar<'a> {
    Key,
    Str(&'a str),
    Num(&'a str),
    Lit(&'a str),
}

fn ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | b'\r')) {
        i += 1;
    }
    i
}

/// Past the string starting at `b[i]` (a quote), checking its escapes; a raw control character
/// (a newline included) isn't JSON.
fn string(b: &[u8], i: usize) -> Result<usize, &'static str> {
    let mut j = i + 1;
    loop {
        match *b.get(j).ok_or("is cut short in a string")? {
            b'"' => return Ok(j + 1),
            b'\\' => {
                j += match *b.get(j + 1).ok_or("is cut short in a string")? {
                    b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't' => 2,
                    b'u' if b.get(j + 2..j + 6).is_some_and(|h| h.iter().all(u8::is_ascii_hexdigit)) => 6,
                    _ => return Err("has an invalid escape in a string"),
                };
            }
            c if c < 0x20 => return Err("has a control character in a string"),
            _ => j += 1,
        }
    }
}

fn number(b: &[u8], i: usize) -> Result<usize, &'static str> {
    let digits = |mut j: usize| {
        let s = j;
        while b.get(j).is_some_and(u8::is_ascii_digit) {
            j += 1;
        }
        (j > s).then_some(j)
    };
    let mut j = i + usize::from(b[i] == b'-');
    j = match b.get(j) {
        Some(b'0') => j + 1,
        Some(b'1'..=b'9') => digits(j).ok_or("has an invalid number")?,
        _ => return Err("has an invalid number"),
    };
    if b.get(j) == Some(&b'.') {
        j = digits(j + 1).ok_or("has an invalid number")?;
    }
    if matches!(b.get(j), Some(b'e' | b'E')) {
        j += 1;
        if matches!(b.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        j = digits(j).ok_or("has an invalid number")?;
    }
    Ok(j)
}

#[derive(Clone, Copy, PartialEq)]
enum Want {
    Value,
    /// A value or the end of an empty array.
    First,
    /// A key or the end of an empty object.
    FirstKey,
    Key,
    Colon,
    Next,
}

/// Scans the JSON value at or after `from` (blanks first), validating it: where it ends. `on`
/// sees each scalar.
fn value<'a>(text: &'a str, from: usize, on: &mut dyn FnMut(Scalar<'a>)) -> Result<usize, &'static str> {
    let b = text.as_bytes();
    let mut stack: Vec<bool> = Vec::new(); // true: an object
    let mut want = Want::Value;
    let mut i = from;
    loop {
        if want == Want::Next && stack.is_empty() {
            return Ok(i);
        }
        i = ws(b, i);
        let c = *b.get(i).ok_or("is cut short")?;
        match want {
            Want::First if c == b']' => {
                stack.pop();
                i += 1;
                want = Want::Next;
            }
            Want::FirstKey if c == b'}' => {
                stack.pop();
                i += 1;
                want = Want::Next;
            }
            Want::Value | Want::First => {
                let start = i;
                match c {
                    b'{' | b'[' => {
                        if stack.len() >= MAX_DEPTH {
                            return Err("is nested too deeply");
                        }
                        stack.push(c == b'{');
                        i += 1;
                        want = if c == b'{' { Want::FirstKey } else { Want::First };
                        continue;
                    }
                    b'"' => {
                        i = string(b, i)?;
                        on(Scalar::Str(&text[start..i]));
                    }
                    b'-' | b'0'..=b'9' => {
                        i = number(b, i)?;
                        on(Scalar::Num(&text[start..i]));
                    }
                    _ => {
                        let lit = ["true", "false", "null"].into_iter().find(|l| b[i..].starts_with(l.as_bytes())).ok_or("isn't JSON")?;
                        i += lit.len();
                        on(Scalar::Lit(lit));
                    }
                }
                want = Want::Next;
            }
            Want::FirstKey | Want::Key => {
                if c != b'"' {
                    return Err("has an object key that isn't a string");
                }
                i = string(b, i)?;
                on(Scalar::Key);
                want = Want::Colon;
            }
            Want::Colon => {
                if c != b':' {
                    return Err("has an object key without a value");
                }
                i += 1;
                want = Want::Value;
            }
            Want::Next => {
                let object = stack.last().copied().unwrap_or(false);
                match (c, object) {
                    (b',', true) => want = Want::Key,
                    (b',', false) => want = Want::Value,
                    (b'}', true) | (b']', false) => {
                        stack.pop();
                    }
                    _ => return Err("isn't JSON"),
                }
                i += 1;
            }
        }
    }
}

/// `text` when it is exactly one JSON object (blanks around it allowed), with its raw line
/// breaks (only possible between tokens, since a string can't hold one raw) made spaces: it can
/// then be one line of a `_bulk` body and nothing more. Otherwise why not.
pub fn one_object(text: &str) -> Result<Cow<'_, str>, &'static str> {
    let b = text.as_bytes();
    let start = ws(b, 0);
    if b.get(start) != Some(&b'{') {
        return Err("isn't a JSON object");
    }
    let end = value(text, start, &mut |_| {})?;
    if ws(b, end) != b.len() {
        return Err("has more after its JSON object");
    }
    Ok(if text.contains(['\n', '\r']) { Cow::Owned(text.replace(['\n', '\r'], " ")) } else { Cow::Borrowed(text) })
}

/// The string, number and boolean values of JSON text (one value, or several separated by
/// blanks, as NDJSON), one a line: `None` when it isn't JSON. Never longer than `text`.
pub fn values_text(text: &str) -> Option<String> {
    let b = text.as_bytes();
    let mut out = String::new();
    let mut i = ws(b, 0);
    while i < b.len() {
        i = value(text, i, &mut |s| {
            let raw = match s {
                Scalar::Key | Scalar::Lit("null") => return,
                Scalar::Str(raw) | Scalar::Num(raw) | Scalar::Lit(raw) => raw,
            };
            if !out.is_empty() {
                out.push('\n');
            }
            match raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
                Some(inner) => unescape_into(inner, &mut out),
                None => out.push_str(raw),
            }
        })
        .ok()?;
        i = ws(b, i);
    }
    Some(out)
}

/// A checked string's content ([`string`]) unescaped onto `out`.
fn unescape_into(s: &str, out: &mut String) {
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('b') => out.push('\u{8}'),
            Some('f') => out.push('\u{c}'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some('u') => {
                let hex = |chars: &mut std::str::Chars| u32::from_str_radix(&chars.by_ref().take(4).collect::<String>(), 16).unwrap_or(0xFFFD);
                let hi = hex(&mut chars);
                let code = if (0xD800..0xDC00).contains(&hi) && chars.as_str().starts_with("\\u") {
                    let mut ahead = chars.clone();
                    ahead.nth(1);
                    let lo = hex(&mut ahead);
                    if (0xDC00..0xE000).contains(&lo) {
                        chars = ahead;
                        0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                    } else {
                        0xFFFD
                    }
                } else {
                    hi
                };
                out.push(char::from_u32(code).unwrap_or('\u{FFFD}'));
            }
            Some(other) => out.push(other),
            None => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_document_is_one_object_on_one_line() {
        // Pretty-printed: valid, its line breaks made spaces.
        assert_eq!(one_object("{\n  \"a\": [1,\r\n 2],\n  \"b\": {\"c\": \"x\\ny\"}\n}").unwrap(), "{   \"a\": [1,   2],   \"b\": {\"c\": \"x\\ny\"} }");
        // A bulk action smuggled after the object, or anything that isn't one object.
        for bad in ["{\"a\":1}\n{\"delete\":{\"_index\":\"victim\",\"_id\":\"1\"}}", "{\"a\":\"x\ny\"}", "[1]", "{\"a\":1", "{\"a\":01}", "{\"a\":tru}", "{\"a\":1,}", "{1:2}", "{\"a\":\"\\q\"}"] {
            assert!(one_object(bad).is_err(), "{bad:?}");
        }
        assert!(one_object(&format!("{}{}", "[".repeat(600), "]".repeat(600))).is_err());
    }

    #[test]
    fn values_of_json_and_ndjson() {
        assert_eq!(values_text(r#"{"a":"x\u00e9\ud83d\ude00","b":[1,true,null,{"c":"y\n"}]}"#).as_deref(), Some("x\u{e9}\u{1F600}\n1\ntrue\ny\n"));
        assert_eq!(values_text("{\"a\":\"1\"}\n{\"a\":\"2\"}\n").as_deref(), Some("1\n2"));
        assert_eq!(values_text("not json"), None);
    }
}
