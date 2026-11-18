//! Text of the objects the `s3` connector reads: plain text as it is, JSON's string and number
//! values, and HTML's and XML's visible text. Other formats have no text (roll has no
//! `attachment` processor yet).

use crate::json::Value;

/// How an object's text is read, by its key's extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Json,
    Markup,
}

pub fn kind_of(key: &str) -> Option<Kind> {
    let ext = extension(key);
    match ext.as_str() {
        ".txt" | ".text" | ".md" | ".markdown" | ".csv" | ".tsv" | ".log" | ".ini" | ".yaml" | ".yml" | ".toml" | ".rst" => Some(Kind::Text),
        ".json" | ".ndjson" | ".jsonl" => Some(Kind::Json),
        ".html" | ".htm" | ".xhtml" | ".xml" => Some(Kind::Markup),
        _ => None,
    }
}

/// A key's extension, lowercase with its dot (`""` when it has none).
pub fn extension(key: &str) -> String {
    let name = key.rsplit('/').next().unwrap_or(key);
    match name.rfind('.') {
        Some(i) if i > 0 => name[i..].to_ascii_lowercase(),
        _ => String::new(),
    }
}

pub fn text(kind: Kind, bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    match kind {
        Kind::Text => s.into_owned(),
        Kind::Json => json_text(&s),
        Kind::Markup => markup_text(&s),
    }
}

/// The string and number values of a JSON document (or of each line of NDJSON), one a line.
fn json_text(s: &str) -> String {
    fn walk(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::Str(s) => out.push(s.clone()),
            Value::Num(_) | Value::Bool(_) => out.push(v.to_es_string()),
            Value::Arr(a) => a.iter().for_each(|x| walk(x, out)),
            Value::Obj(m) => m.values().for_each(|x| walk(x, out)),
            Value::Null => {}
        }
    }
    let mut out = Vec::new();
    match crate::json::parse(s.as_bytes()) {
        Ok(Some(v)) => walk(&v, &mut out),
        _ => {
            for line in s.lines().filter(|l| !l.trim().is_empty()) {
                match crate::json::parse(line.as_bytes()) {
                    Ok(Some(v)) => walk(&v, &mut out),
                    _ => return s.to_string(),
                }
            }
        }
    }
    out.join("\n")
}

/// The visible text of HTML or XML: tags, comments, scripts and styles dropped, entities
/// decoded, whitespace collapsed.
fn markup_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len() / 2);
    let lower = s.to_ascii_lowercase();
    let mut i = 0;
    let b = s.as_bytes();
    while i < b.len() {
        if b[i] == b'<' {
            let rest = &lower[i..];
            let skip_to = |end: &str| rest.find(end).map_or(b.len(), |e| i + e + end.len());
            i = if rest.starts_with("<!--") {
                skip_to("-->")
            } else if rest.starts_with("<script") {
                skip_to("</script>")
            } else if rest.starts_with("<style") {
                skip_to("</style>")
            } else if rest.starts_with("<![cdata[") {
                let end = rest.find("]]>").map_or(b.len(), |e| i + e);
                out.push_str(&s[i + 9..end.max(i + 9)]);
                (end + 3).min(b.len())
            } else {
                out.push(' ');
                rest.find('>').map_or(b.len(), |e| i + e + 1)
            };
            continue;
        }
        let next = s[i..].find('<').map_or(b.len(), |e| i + e);
        out.push_str(&decode_entities(&s[i..next]));
        i = next;
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let end = rest[..rest.len().min(12)].find(';');
        let decoded = end.and_then(|e| {
            let name = &rest[1..e];
            let c = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                n if n.starts_with("#x") || n.starts_with("#X") => u32::from_str_radix(&n[2..], 16).ok().and_then(char::from_u32),
                n if n.starts_with('#') => n[1..].parse().ok().and_then(char::from_u32),
                _ => None,
            };
            c.map(|c| (c, e + 1))
        });
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_visible_text() {
        let html = "<html><head><style>p{}</style><script>var x='<p>';</script></head><body><p>Fish &amp; chips</p><!-- no -->\n<b>caf&#233;</b></body></html>";
        assert_eq!(text(Kind::Markup, html.as_bytes()), "Fish & chips café");
        assert_eq!(text(Kind::Json, br#"{"a":"one","b":[2,{"c":"three"}],"d":null}"#), "one\n2\nthree");
        assert_eq!(kind_of("docs/Read.Me.HTML"), Some(Kind::Markup));
        assert_eq!(kind_of("x.pdf"), None);
    }
}
