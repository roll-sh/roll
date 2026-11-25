//! Algolia's filters as an Elasticsearch query: the `filters` string (`brand:Acme AND price < 100`,
//! `NOT _tags:sale`, `price:10 TO 20`, parentheses) and the `facetFilters` / `numericFilters`
//! arrays (inner arrays are ORs). Anything else (`<score=…>`, optional filters, tag filters'
//! special forms) is refused with why, and the rule says so.
//!
//! Facet values compare against the attribute's exact field (its `keyword` sub-field when it
//! has text). A filter is at most [`MAX_LEN`] bytes and [`MAX_DEPTH`] levels deep.

use serde_json::{json, Value};

const MAX_LEN: usize = 4096;
const MAX_DEPTH: usize = 32;
/// The most entries a `facetFilters` / `numericFilters` list (inner lists counted) may hold.
const MAX_ENTRIES: usize = 100;

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    Quoted(String),
    Open,
    Close,
    Op(&'static str),
}

fn lex(s: &str) -> Result<Vec<Tok>, String> {
    let mut out = Vec::new();
    let cs: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        match c {
            c if c.is_whitespace() => i += 1,
            '(' => {
                out.push(Tok::Open);
                i += 1;
            }
            ')' => {
                out.push(Tok::Close);
                i += 1;
            }
            '"' | '\'' => {
                let q = c;
                let mut v = String::new();
                i += 1;
                loop {
                    match cs.get(i) {
                        None => return Err("a quoted value isn't closed".into()),
                        Some('\\') => {
                            v.extend(cs.get(i + 1));
                            i += 2;
                        }
                        Some(&x) if x == q => {
                            i += 1;
                            break;
                        }
                        Some(&x) => {
                            v.push(x);
                            i += 1;
                        }
                    }
                }
                out.push(Tok::Quoted(v));
            }
            '<' | '>' | '!' | '=' | ':' => {
                let two: String = cs[i..(i + 2).min(cs.len())].iter().collect();
                let op = match two.as_str() {
                    "<=" => "<=",
                    ">=" => ">=",
                    "!=" => "!=",
                    _ => match c {
                        '<' => "<",
                        '>' => ">",
                        '=' => "=",
                        ':' => ":",
                        _ => return Err("a lone `!`".into()),
                    },
                };
                i += op.len();
                out.push(Tok::Op(op));
            }
            _ => {
                let mut v = String::new();
                while let Some(&x) = cs.get(i) {
                    if x.is_whitespace() || "()<>!=:\"'".contains(x) {
                        break;
                    }
                    v.push(x);
                    i += 1;
                }
                out.push(Tok::Word(v));
            }
        }
    }
    Ok(out)
}

struct Parser<'a, F: Fn(&str) -> String> {
    toks: Vec<Tok>,
    at: usize,
    exact: &'a F,
}

fn keyword(t: Option<&Tok>, k: &str) -> bool {
    matches!(t, Some(Tok::Word(w)) if w.eq_ignore_ascii_case(k))
}

/// A facet value (`attr:value`): always a string, as Algolia compares facet values (`zip:02139`,
/// `sku:1e5` stay as written; a numeric field takes the string as Elasticsearch coerces it).
fn facet_value(t: &Tok) -> Value {
    match t {
        Tok::Quoted(s) | Tok::Word(s) => json!(s),
        _ => Value::Null,
    }
}

/// A comparison's value as JSON: a number when it reads as one, `true`/`false`, else a string.
fn value(t: &Tok) -> Value {
    match t {
        Tok::Quoted(s) => json!(s),
        Tok::Word(w) => {
            if let Ok(n) = w.parse::<i64>() {
                json!(n)
            } else if let Ok(f) = w.parse::<f64>().map_err(|_| ()).and_then(|f| if f.is_finite() { Ok(f) } else { Err(()) }) {
                json!(f)
            } else if w == "true" || w == "false" {
                json!(w == "true")
            } else {
                json!(w)
            }
        }
        _ => Value::Null,
    }
}

impl<F: Fn(&str) -> String> Parser<'_, F> {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.at).cloned();
        self.at += 1;
        t
    }

    fn or(&mut self, depth: usize) -> Result<Value, String> {
        let mut parts = vec![self.and(depth)?];
        while keyword(self.peek(), "OR") {
            self.at += 1;
            parts.push(self.and(depth)?);
        }
        Ok(if parts.len() == 1 { parts.pop().expect("one") } else { json!({ "bool": { "should": parts, "minimum_should_match": 1 } }) })
    }

    fn and(&mut self, depth: usize) -> Result<Value, String> {
        let mut parts = vec![self.not(depth)?];
        while keyword(self.peek(), "AND") {
            self.at += 1;
            parts.push(self.not(depth)?);
        }
        Ok(if parts.len() == 1 { parts.pop().expect("one") } else { json!({ "bool": { "filter": parts } }) })
    }

    fn not(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err("the filter nests too deep".into());
        }
        if keyword(self.peek(), "NOT") {
            self.at += 1;
            let inner = self.not(depth + 1)?;
            return Ok(json!({ "bool": { "must_not": [inner] } }));
        }
        if self.peek() == Some(&Tok::Open) {
            self.at += 1;
            let inner = self.or(depth + 1)?;
            if self.next() != Some(Tok::Close) {
                return Err("a parenthesis isn't closed".into());
            }
            return Ok(inner);
        }
        self.comparison()
    }

    fn comparison(&mut self) -> Result<Value, String> {
        let attr = match self.next() {
            Some(Tok::Word(w)) | Some(Tok::Quoted(w)) if super::template_safe(&w) && !super::is_meta(&w) => w,
            Some(Tok::Word(w)) | Some(Tok::Quoted(w)) => return Err(format!("[{w}] isn't an attribute roll can filter on")),
            _ => return Err("a filter that isn't `attribute:value` or `attribute < number`".into()),
        };
        let field = (self.exact)(&attr);
        let op = match self.next() {
            Some(Tok::Op(op)) => op,
            _ => return Err(format!("[{attr}] has no `:` or comparison after it")),
        };
        let v = self.next().ok_or_else(|| format!("[{attr}] has no value"))?;
        if matches!(v, Tok::Open | Tok::Close | Tok::Op(_)) {
            return Err(format!("[{attr}] has no value"));
        }
        let val = value(&v);
        if op == ":" {
            if keyword(self.peek(), "TO") {
                self.at += 1;
                let hi = self.next().map(|t| value(&t)).filter(Value::is_number).ok_or("a range's end isn't a number")?;
                if !val.is_number() {
                    return Err("a range's start isn't a number".into());
                }
                return Ok(json!({ "range": { attr: { "gte": val, "lte": hi } } }));
            }
            return Ok(json!({ "term": { field: facet_value(&v) } }));
        }
        if !val.is_number() {
            return Err(format!("[{attr}] {op} compares with a number"));
        }
        Ok(match op {
            "=" => json!({ "term": { attr: val } }),
            "!=" => json!({ "bool": { "must_not": [{ "term": { attr: val } }] } }),
            "<" => json!({ "range": { attr: { "lt": val } } }),
            "<=" => json!({ "range": { attr: { "lte": val } } }),
            ">" => json!({ "range": { attr: { "gt": val } } }),
            _ => json!({ "range": { attr: { "gte": val } } }),
        })
    }
}

/// Algolia's `filters` string as one query; `exact` names an attribute's exact field.
pub(crate) fn filters(s: &str, exact: &impl Fn(&str) -> String) -> Result<Value, String> {
    if s.len() > MAX_LEN {
        return Err(format!("the filter is longer than the {MAX_LEN} bytes roll reads"));
    }
    // A brace would be a Mustache tag once the filter is in a search template.
    if s.contains(['{', '}']) {
        return Err("a filter with `{` or `}` can't go in a search template".into());
    }
    if s.contains("<score=") {
        return Err("filter scores (`<score=…>`) have no Elasticsearch equivalent".into());
    }
    let mut p = Parser { toks: lex(s)?, at: 0, exact };
    if p.toks.is_empty() {
        return Err("the filter is empty".into());
    }
    let q = p.or(0)?;
    if p.at != p.toks.len() {
        return Err("the filter has more after its end".into());
    }
    Ok(q)
}

/// `facetFilters` / `numericFilters`: a string, or an array whose entries AND and whose inner
/// arrays OR. A facet filter's leading `-` negates it.
pub(crate) fn filter_list(v: &Value, exact: &impl Fn(&str) -> String) -> Result<Value, String> {
    let one = |s: &str| -> Result<Value, String> {
        match s.strip_prefix('-') {
            Some(rest) => Ok(json!({ "bool": { "must_not": [filters(rest, exact)?] } })),
            None => filters(s, exact),
        }
    };
    let entries = match v {
        Value::Array(a) => a.iter().map(|x| x.as_array().map_or(1, Vec::len)).sum(),
        _ => 1,
    };
    if entries > MAX_ENTRIES {
        return Err(format!("{entries} entries, more than the {MAX_ENTRIES} roll maps"));
    }
    match v {
        Value::String(s) => one(s),
        Value::Array(a) => {
            let mut all = Vec::new();
            for x in a {
                all.push(match x {
                    Value::String(s) => one(s)?,
                    Value::Array(or) => {
                        let parts: Result<Vec<Value>, String> = or.iter().map(|y| y.as_str().ok_or("an entry that isn't a string".to_string()).and_then(one)).collect();
                        json!({ "bool": { "should": parts?, "minimum_should_match": 1 } })
                    }
                    _ => return Err("an entry that isn't a string or a list".into()),
                });
            }
            Ok(json!({ "bool": { "filter": all } }))
        }
        _ => Err("not a string or a list".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn algolia_filters_become_queries() {
        let exact = |a: &str| if a == "brand" { "brand.keyword".to_string() } else { a.to_string() };
        assert_eq!(
            filters(r#"brand:"Acme Co" AND (price < 10 OR NOT _tags:sale) AND year:2000 TO 2010"#, &exact).unwrap(),
            json!({ "bool": { "filter": [
                { "term": { "brand.keyword": "Acme Co" } },
                { "bool": { "should": [{ "range": { "price": { "lt": 10 } } }, { "bool": { "must_not": [{ "term": { "_tags": "sale" } }] } }], "minimum_should_match": 1 } },
                { "range": { "year": { "gte": 2000, "lte": 2010 } } },
            ] } })
        );
        assert_eq!(filter_list(&json!([["brand:Acme", "brand:Brewer"], "-color:red"]), &exact).unwrap()["bool"]["filter"][1], json!({ "bool": { "must_not": [{ "term": { "color": "red" } }] } }));
        // Facet values stay as written; braces never reach a template.
        assert_eq!(filters("zip:02139 AND sku:1e5", &exact).unwrap()["bool"]["filter"], json!([{ "term": { "zip": "02139" } }, { "term": { "sku": "1e5" } }]));
        for bad in ["brand:", "(brand:a", "price < cheap", "_id:x", "{{x}}:1", "brand:a OR", "a<score=2>:b", "brand:{{{query}}}", r#"brand:"}}""#] {
            assert!(filters(bad, &exact).is_err(), "{bad}");
        }
        let deep = format!("{}a:1{}", "(".repeat(100), ")".repeat(100));
        assert!(filters(&deep, &exact).is_err());
    }
}
