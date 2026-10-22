//! YAML output as Elasticsearch's Jackson YAML generator (SnakeYAML underneath) writes it.
//!
//! The document starts with `---`, mappings and sequences are in block style with two-space
//! indents and "indentless" sequences under mapping keys (`key:\n- a`), every string value is
//! double-quoted (Elasticsearch doesn't enable `MINIMIZE_QUOTES`), and keys are plain unless
//! they would read back as something else (`"y"`, `"123"`, `"a: b"`).

use crate::json::{Value, write::write_number};

pub fn write_yaml(out: &mut Vec<u8>, v: &Value) {
    out.extend_from_slice(b"---");
    match v {
        Value::Obj(m) if !m.is_empty() => {
            out.push(b'\n');
            write_map(out, v, 0, false);
        }
        Value::Arr(a) if !a.is_empty() => {
            out.push(b'\n');
            write_seq(out, v, 0, false);
        }
        other => {
            out.push(b' ');
            write_flow_or_scalar(out, other);
            out.push(b'\n');
        }
    }
}

fn spaces(out: &mut Vec<u8>, n: usize) {
    out.extend(std::iter::repeat_n(b' ', n));
}

fn write_flow_or_scalar(out: &mut Vec<u8>, v: &Value) {
    match v {
        Value::Obj(_) => out.extend_from_slice(b"{}"),
        Value::Arr(_) => out.extend_from_slice(b"[]"),
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(b) => out.extend_from_slice(if *b { b"true" } else { b"false" }),
        Value::Num(n) => write_number(out, n),
        Value::Str(s) => write_quoted(out, s),
    }
}

fn is_container(v: &Value) -> bool {
    match v {
        Value::Obj(m) => !m.is_empty(),
        Value::Arr(a) => !a.is_empty(),
        _ => false,
    }
}

/// Writes a non-empty mapping whose entries start at column `indent`. `inline` means the first
/// entry continues the current line (after `- `).
fn write_map(out: &mut Vec<u8>, v: &Value, indent: usize, inline: bool) {
    let Value::Obj(m) = v else { return };
    for (i, (k, val)) in m.iter().enumerate() {
        if i > 0 || !inline {
            spaces(out, indent);
        }
        write_key(out, k);
        out.push(b':');
        match val {
            Value::Obj(_) if is_container(val) => {
                out.push(b'\n');
                crate::stack::maybe_grow(|| write_map(out, val, indent + 2, false));
            }
            Value::Arr(_) if is_container(val) => {
                out.push(b'\n');
                crate::stack::maybe_grow(|| write_seq(out, val, indent, false));
            }
            scalar => {
                out.push(b' ');
                write_flow_or_scalar(out, scalar);
                out.push(b'\n');
            }
        }
    }
}

fn write_seq(out: &mut Vec<u8>, v: &Value, indent: usize, inline: bool) {
    let Value::Arr(a) = v else { return };
    for (i, item) in a.iter().enumerate() {
        if i > 0 || !inline {
            spaces(out, indent);
        }
        out.extend_from_slice(b"- ");
        match item {
            Value::Obj(_) if is_container(item) => crate::stack::maybe_grow(|| write_map(out, item, indent + 2, true)),
            Value::Arr(_) if is_container(item) => crate::stack::maybe_grow(|| write_seq(out, item, indent + 2, true)),
            scalar => {
                write_flow_or_scalar(out, scalar);
                out.push(b'\n');
            }
        }
    }
}

const RESERVED: &[&str] = &[
    "false", "False", "FALSE", "n", "N", "no", "No", "NO", "null", "Null", "NULL", "on", "On", "ON", "off", "Off", "OFF", "true", "True",
    "TRUE", "y", "Y", "yes", "Yes", "YES", "~",
];

fn key_needs_quotes(k: &str) -> bool {
    if k.is_empty() || RESERVED.contains(&k) {
        return true;
    }
    let b = k.as_bytes();
    let first = b[0];
    if first.is_ascii_digit() || ((first == b'-' || first == b'+' || first == b'.') && b.get(1).is_some_and(|c| c.is_ascii_digit())) {
        return true;
    }
    if b"-?:,[]{}#&*!|>'\"%@`".contains(&first) && !(first == b'-' && b.len() > 1 && b[1] != b' ') {
        return true;
    }
    if first == b' ' || *b.last().unwrap() == b' ' || k.ends_with(':') {
        return true;
    }
    if k.contains(": ") || k.contains(" #") {
        return true;
    }
    k.chars().any(|c| !is_printable(c) || c == '\n' || c == '\r' || c == '\t')
}

fn write_key(out: &mut Vec<u8>, k: &str) {
    if key_needs_quotes(k) { write_quoted(out, k) } else { out.extend_from_slice(k.as_bytes()) }
}

fn is_printable(c: char) -> bool {
    matches!(c as u32, 0x09 | 0x0a | 0x0d | 0x20..=0x7e | 0x85 | 0xa0..=0xd7ff | 0xe000..=0xfffd | 0x10000..=0x10ffff)
}

/// SnakeYAML's double-quoted scalar.
fn write_quoted(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for c in s.chars() {
        let esc: Option<&str> = match c {
            '\0' => Some("\\0"),
            '\u{7}' => Some("\\a"),
            '\u{8}' => Some("\\b"),
            '\t' => Some("\\t"),
            '\n' => Some("\\n"),
            '\u{b}' => Some("\\v"),
            '\u{c}' => Some("\\f"),
            '\r' => Some("\\r"),
            '\u{1b}' => Some("\\e"),
            '"' => Some("\\\""),
            '\\' => Some("\\\\"),
            '\u{85}' => Some("\\N"),
            '\u{a0}' => Some("\\_"),
            '\u{2028}' => Some("\\L"),
            '\u{2029}' => Some("\\P"),
            _ => None,
        };
        if let Some(e) = esc {
            out.extend_from_slice(e.as_bytes());
        } else if (' '..='~').contains(&c) || (is_printable(c) && c != '\u{feff}') {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        } else {
            let cp = c as u32;
            let e = if cp <= 0xff {
                format!("\\x{cp:02x}")
            } else if cp <= 0xffff {
                format!("\\u{cp:04x}")
            } else {
                format!("\\U{cp:08x}")
            };
            out.extend_from_slice(e.as_bytes());
        }
    }
    out.push(b'"');
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{arr, obj};

    fn y(v: &Value) -> String {
        let mut out = Vec::new();
        write_yaml(&mut out, v);
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn layout_matches_elasticsearch() {
        let v = obj! {
            "fmt" => obj! { "a" => arr![1, arr![2, 3], obj! { "x" => Value::Null }], "e" => obj! {}, "ea" => arr![],
            "s" => "line\nbreak \"q\" \u{1} é / \\", "y" => "yes", "num" => "123", "f" => 1.0e7 }
        };
        assert_eq!(
            y(&v),
            "---\nfmt:\n  a:\n  - 1\n  - - 2\n    - 3\n  - x: null\n  e: {}\n  ea: []\n  s: \"line\\nbreak \\\"q\\\" \\x01 é / \\\\\"\n  \"y\": \"yes\"\n  num: \"123\"\n  f: 1.0E7\n"
        );
    }

    #[test]
    fn root_list() {
        let v = arr![obj! { "health" => "green", "index" => "i" }];
        assert_eq!(y(&v), "---\n- health: \"green\"\n  index: \"i\"\n");
    }
}
