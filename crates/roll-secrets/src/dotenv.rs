//! `.env` files in and out (bulk paste, `roll secrets import`, `…/values?format=`).
//!
//! Parsing follows the common dotenv rules: `KEY=value`, an optional `export `, `#` comments,
//! single quotes literal, double quotes with `\n`, `\"` and `\\` escapes and several lines,
//! unquoted values trimmed and ended by ` #`.

use crate::Value;
use std::collections::BTreeMap;

#[derive(Debug, PartialEq)]
pub struct Entry {
    pub key: String,
    pub value: Value,
    /// The comment on the line before, if any.
    pub comment: String,
}

/// The entries of a `.env` file in order, or the first bad line (1-based) and why.
pub fn parse(text: &str) -> Result<Vec<Entry>, String> {
    let mut out: Vec<Entry> = vec![];
    let mut comment = String::new();
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i].trim_start_matches('\u{feff}');
        let n = i + 1;
        i += 1;
        let t = line.trim();
        if t.is_empty() {
            comment.clear();
            continue;
        }
        if let Some(c) = t.strip_prefix('#') {
            comment = c.trim().to_string();
            continue;
        }
        let t = t.strip_prefix("export ").map(str::trim_start).unwrap_or(t);
        let (key, rest) = t.split_once('=').ok_or_else(|| format!("line {n}: expected KEY=value"))?;
        let key = key.trim();
        crate::names::key(key).map_err(|e| format!("line {n}: {e}"))?;
        let rest = rest.trim_start();
        let value = if let Some(body) = rest.strip_prefix('"') {
            let mut acc = String::new();
            let mut cur = body.to_string();
            loop {
                match closing(&cur) {
                    Some(end) => {
                        acc.push_str(&cur[..end]);
                        break;
                    }
                    None if i < lines.len() => {
                        acc.push_str(&cur);
                        acc.push('\n');
                        cur = lines[i].to_string();
                        i += 1;
                    }
                    None => return Err(format!("line {n}: the double quote isn't closed")),
                }
            }
            unescape(&acc)
        } else if let Some(body) = rest.strip_prefix('\'') {
            let end = body.find('\'').ok_or_else(|| format!("line {n}: the single quote isn't closed"))?;
            body[..end].to_string()
        } else {
            let v = match rest.find(" #") {
                Some(at) => &rest[..at],
                None => rest,
            };
            v.trim().to_string()
        };
        crate::names::value(&value).map_err(|e| format!("line {n}: {e}"))?;
        let entry = Entry { key: key.to_string(), value: Value::new(value), comment: std::mem::take(&mut comment) };
        match out.iter_mut().find(|e| e.key == entry.key) {
            Some(e) => *e = entry,
            None => out.push(entry),
        }
    }
    Ok(out)
}

/// The position of the first `"` not escaped by a backslash.
fn closing(s: &str) -> Option<usize> {
    let mut escaped = false;
    for (i, ch) in s.char_indices() {
        match ch {
            '\\' if !escaped => escaped = true,
            '"' if !escaped => return Some(i),
            _ => escaped = false,
        }
    }
    None
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some('t') => out.push('\t'),
            Some(o) => out.push(o),
            None => out.push('\\'),
        }
    }
    out
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Dotenv,
    Shell,
    Json,
    Yaml,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        Some(match s {
            "dotenv" | "env" => Format::Dotenv,
            "shell" | "sh" => Format::Shell,
            "json" => Format::Json,
            "yaml" | "yml" => Format::Yaml,
            _ => return None,
        })
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Format::Json => "application/json",
            Format::Yaml => "application/yaml",
            _ => "text/plain; charset=utf-8",
        }
    }
}

/// Values written out in `f`. The result holds values: callers zeroize or drop it promptly.
pub fn format(values: &BTreeMap<String, Value>, f: Format) -> zeroize::Zeroizing<String> {
    let mut out = zeroize::Zeroizing::new(String::new());
    match f {
        Format::Json => {
            let m: BTreeMap<&str, &str> = values.iter().map(|(k, v)| (k.as_str(), v.expose())).collect();
            out.push_str(&serde_json::to_string_pretty(&m).expect("JSON"));
            out.push('\n');
        }
        Format::Dotenv => {
            for (k, v) in values {
                out.push_str(&format!("{k}={}\n", dq(v.expose())));
            }
        }
        Format::Shell => {
            for (k, v) in values {
                out.push_str(&format!("export {k}='{}'\n", v.expose().replace('\'', "'\\''")));
            }
        }
        Format::Yaml => {
            for (k, v) in values {
                out.push_str(&format!("{k}: {}\n", serde_json::to_string(v.expose()).expect("JSON")));
            }
        }
    }
    out
}

/// A double-quoted dotenv value, when it needs quoting.
fn dq(v: &str) -> String {
    let plain = !v.is_empty() && v.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:@+,=%".contains(c));
    if plain {
        return v.to_string();
    }
    let mut s = String::from("\"");
    for c in v.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\r' => s.push_str("\\r"),
            '$' => s.push_str("\\$"),
            o => s.push(o),
        }
    }
    s.push('"');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_round_trips() {
        let text = "# the database\nexport DATABASE_URL=postgres://u:p@h/db # prod\nNAME='it''s'\nKEY=\"line1\\nline2 \\\"q\\\"\"\nPEM=\"-----BEGIN-----\nabc\n-----END-----\"\n\nEMPTY=\n";
        let e = parse(text).unwrap();
        let got: Vec<(&str, &str, &str)> = e.iter().map(|e| (e.key.as_str(), e.value.expose(), e.comment.as_str())).collect();
        assert_eq!(
            got,
            [
                ("DATABASE_URL", "postgres://u:p@h/db", "the database"),
                ("NAME", "it", ""),
                ("KEY", "line1\nline2 \"q\"", ""),
                ("PEM", "-----BEGIN-----\nabc\n-----END-----", ""),
                ("EMPTY", "", "")
            ]
        );
        let values: BTreeMap<String, Value> = e.into_iter().map(|e| (e.key, e.value)).collect();
        let again = parse(&format(&values, Format::Dotenv)).unwrap();
        assert_eq!(again.into_iter().map(|e| (e.key, e.value)).collect::<BTreeMap<_, _>>(), values);
        assert!(parse("1BAD=x").unwrap_err().starts_with("line 1"));
        assert!(parse("A=\"open").is_err());
    }
}
