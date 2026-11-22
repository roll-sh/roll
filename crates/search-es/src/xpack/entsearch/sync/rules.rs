//! Sync rules: Elastic's basic rules (applied to each document before it is indexed) and the
//! check of a draft that makes its `validation` `valid` or `invalid`.

use crate::json::{Map, Value};
use crate::obj;

enum Cond {
    Equals(String),
    Contains(String),
    StartsWith(String),
    EndsWith(String),
    Regex(regex::Regex),
    Gt(String),
    Lt(String),
}

struct Rule {
    field: String,
    include: bool,
    cond: Cond,
}

/// A connector's basic rules in order (the first that matches decides; none matching includes).
#[derive(Default)]
pub struct Rules(Vec<Rule>);

/// A rule that doesn't parse: its id and why.
pub struct Problem {
    pub id: String,
    pub message: String,
}

impl Rules {
    /// The rules of a filtering's `active` (or `draft`) object.
    pub fn compile(filtering: &Value) -> Result<Rules, Vec<Problem>> {
        let mut list: Vec<&Value> = filtering.get("rules").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default();
        list.sort_by_key(|r| r.get("order").and_then(Value::as_i64).unwrap_or(0));
        let mut rules = Vec::new();
        let mut problems = Vec::new();
        for r in list {
            let s = |k: &str| r.get(k).map(Value::to_es_string).unwrap_or_default();
            let (id, value) = (s("id"), s("value"));
            let cond = match s("rule").as_str() {
                "equals" => Cond::Equals(value),
                "contains" => Cond::Contains(value),
                "starts_with" => Cond::StartsWith(value),
                "ends_with" => Cond::EndsWith(value),
                ">" => Cond::Gt(value),
                "<" => Cond::Lt(value),
                "regex" => match regex::RegexBuilder::new(&format!("^(?:{value})")).size_limit(1 << 20).build() {
                    Ok(re) => Cond::Regex(re),
                    Err(_) => {
                        problems.push(Problem { id, message: format!("[{value}] isn't a valid regular expression") });
                        continue;
                    }
                },
                other => {
                    problems.push(Problem { id, message: format!("unknown rule [{other}]") });
                    continue;
                }
            };
            rules.push(Rule { field: s("field"), include: s("policy") != "exclude", cond });
        }
        if problems.is_empty() { Ok(Rules(rules)) } else { Err(problems) }
    }

    /// The fields the rules read (none when every document is included whatever it holds).
    pub fn fields(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.0.iter().map(|r| r.field.as_str()).filter(|f| *f != "_").collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Whether `doc` is indexed.
    pub fn includes(&self, doc: &Map) -> bool {
        for r in &self.0 {
            // The default rule (field `_`) matches everything.
            let matched = if r.field == "_" {
                true
            } else {
                match doc.get(&r.field) {
                    None | Some(Value::Null) => false,
                    Some(v) => r.cond.test(Some(v)),
                }
            };
            if matched {
                return r.include;
            }
        }
        true
    }
}

impl Cond {
    fn test(&self, v: Option<&Value>) -> bool {
        let Some(v) = v else { return false };
        let text = v.to_es_string();
        let num = |s: &str| s.trim().parse::<f64>().ok();
        let cmp = |want: &str| match (num(&text), num(want)) {
            (Some(a), Some(b)) => a.partial_cmp(&b),
            _ => Some(text.as_str().cmp(want)),
        };
        match self {
            Cond::Equals(w) => &text == w || matches!((num(&text), num(w)), (Some(a), Some(b)) if a == b),
            Cond::Contains(w) => text.contains(w.as_str()),
            Cond::StartsWith(w) => text.starts_with(w.as_str()),
            Cond::EndsWith(w) => text.ends_with(w.as_str()),
            Cond::Regex(re) => re.is_match(&text),
            Cond::Gt(w) => cmp(w) == Some(std::cmp::Ordering::Greater),
            Cond::Lt(w) => cmp(w) == Some(std::cmp::Ordering::Less),
        }
    }
}

/// The top-level fields `wanted` of the JSON object `text`, read by scanning its tokens: nothing
/// else of it is parsed, so a document whose other fields hold millions of values costs nothing
/// here. A scalar is parsed; an object or array is kept as its JSON text (what a rule compares).
/// Text that isn't an object gives what was read before it stopped making sense.
pub fn pick(text: &str, wanted: &[&str]) -> Map {
    let b = text.as_bytes();
    let mut out = Map::new();
    let mut i = ws(b, 0);
    if b.get(i) != Some(&b'{') {
        return out;
    }
    i += 1;
    while out.len() < wanted.len() {
        i = ws(b, i);
        if b.get(i) != Some(&b'"') {
            break;
        }
        let Some(key_end) = string_end(b, i) else { break };
        let key = &text[i..key_end];
        i = ws(b, key_end);
        if b.get(i) != Some(&b':') {
            break;
        }
        i = ws(b, i + 1);
        let Some(end) = value_end(b, i) else { break };
        let name = if key.contains('\\') {
            crate::json::parse(key.as_bytes()).ok().flatten().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
        } else {
            key[1..key.len() - 1].to_string()
        };
        if wanted.contains(&name.as_str()) {
            let raw = &text[i..end];
            let v = match b[i] {
                b'{' | b'[' => Value::Str(raw.to_string()),
                _ => crate::json::parse(raw.as_bytes()).ok().flatten().unwrap_or(Value::Null),
            };
            out.insert(name, v);
        }
        i = ws(b, end);
        if b.get(i) != Some(&b',') {
            break;
        }
        i += 1;
    }
    out
}

fn ws(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(|c| c.is_ascii_whitespace()) {
        i += 1;
    }
    i
}

/// Past the string starting at `b[i]` (a quote).
fn string_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
    None
}

/// Past the value starting at `b[i]`.
fn value_end(b: &[u8], i: usize) -> Option<usize> {
    match *b.get(i)? {
        b'"' => string_end(b, i),
        b'{' | b'[' => {
            let (mut depth, mut j) = (0usize, i);
            while j < b.len() {
                match b[j] {
                    b'"' => {
                        j = string_end(b, j)?;
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
            None
        }
        _ => {
            let mut j = i;
            while j < b.len() && !matches!(b[j], b',' | b'}' | b']') && !b[j].is_ascii_whitespace() {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// An `s3` connector's advanced rule (Elastic's shape).
pub struct S3Rule {
    pub bucket: String,
    pub prefix: String,
    pub extensions: Vec<String>,
}

/// The advanced snippet's value when it is set (a non-empty array or object).
pub fn advanced(filtering: &Value) -> Option<&Value> {
    let v = filtering.pointer(&["advanced_snippet", "value"])?;
    match v {
        Value::Arr(a) if !a.is_empty() => Some(v),
        Value::Obj(m) if !m.is_empty() => Some(v),
        _ => None,
    }
}

/// An `s3` connector's advanced rules, which may only name buckets of its `buckets`: they choose
/// prefixes and extensions, never another bucket the stored key happens to read.
pub fn s3_rules_within(v: &Value, buckets: &[String]) -> Result<Vec<S3Rule>, String> {
    let rules = s3_rules(v)?;
    match rules.iter().find(|r| !buckets.contains(&r.bucket)) {
        Some(r) => Err(format!("an advanced sync rule names bucket [{}], which isn't in the connector's [buckets]", r.bucket)),
        None => Ok(rules),
    }
}

/// An `s3` connector's advanced rules.
pub fn s3_rules(v: &Value) -> Result<Vec<S3Rule>, String> {
    let Value::Arr(a) = v else { return Err("advanced sync rules for s3 are an array of {\"bucket\", \"prefix\", \"extension\"}".into()) };
    let mut out = Vec::new();
    for r in a {
        let Some(bucket) = r.get("bucket").and_then(Value::as_str).filter(|b| !b.is_empty()) else {
            return Err("every advanced sync rule for s3 names a [bucket]".into());
        };
        let prefix = match r.get("prefix") {
            None | Some(Value::Null) => String::new(),
            Some(Value::Str(p)) => p.clone(),
            Some(_) => return Err("an advanced sync rule's [prefix] is a string".into()),
        };
        let extensions = match r.get("extension") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Arr(e)) if e.iter().all(|x| x.as_str().is_some()) => e.iter().filter_map(Value::as_str).map(|x| x.to_ascii_lowercase()).collect(),
            Some(_) => return Err("an advanced sync rule's [extension] is an array of strings".into()),
        };
        out.push(S3Rule { bucket: bucket.to_string(), prefix, extensions });
    }
    Ok(out)
}

/// The `validation` of a draft filtering of a `service` connector roll runs (`buckets`: an `s3`
/// connector's configured buckets).
pub fn validate(service: &str, draft: &Value, buckets: &[String]) -> Value {
    let mut errors: Vec<Value> = match Rules::compile(draft) {
        Ok(_) => Vec::new(),
        Err(p) => p.into_iter().map(|p| obj! { "ids" => crate::arr![p.id], "messages" => crate::arr![p.message] }).collect(),
    };
    if let Some(a) = advanced(draft) {
        let problem = match service {
            "s3" => s3_rules_within(a, buckets).err(),
            _ => Some(format!("advanced sync rules aren't supported for {service} connectors yet")),
        };
        if let Some(m) = problem {
            errors.push(obj! { "ids" => crate::arr!["advanced_snippet"], "messages" => crate::arr![m] });
        }
    }
    let state = if errors.is_empty() { "valid" } else { "invalid" };
    obj! { "errors" => Value::Arr(errors), "state" => state }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(json: &str) -> Rules {
        Rules::compile(&crate::json::parse(json.as_bytes()).unwrap().unwrap()).ok().unwrap()
    }

    #[test]
    fn first_matching_rule_decides() {
        let r = rules(
            r#"{"rules":[
                {"id":"a","order":0,"field":"price","policy":"exclude","rule":">","value":"100"},
                {"id":"b","order":1,"field":"name","policy":"include","rule":"regex","value":"ap"},
                {"id":"c","order":2,"field":"name","policy":"exclude","rule":"contains","value":""},
                {"id":"DEFAULT","order":3,"field":"_","policy":"include","rule":"regex","value":".*"}]}"#,
        );
        let doc = |name: &str, price: i64| crate::json::parse(format!(r#"{{"name":"{name}","price":{price}}}"#).as_bytes()).unwrap().unwrap().as_object().unwrap().clone();
        assert!(r.includes(&doc("apple", 5)));
        assert!(!r.includes(&doc("apple", 500)), "numbers compare as numbers");
        assert!(!r.includes(&doc("pear", 5)), "regex anchors at the start");
        assert!(Rules::compile(&crate::json::parse(br#"{"rules":[{"id":"x","rule":"regex","value":"("}]}"#).unwrap().unwrap()).is_err());
        // Read from a document's text without parsing the rest of it.
        let text = r#"{ "zeros" : [0,0,[0,"]"],{"a":"}"}], "na\u006de" : "apple", "price": 500 , "x":{"y":1}}"#;
        let picked = pick(text, &r.fields());
        assert_eq!(picked.get("name").and_then(Value::as_str), Some("apple"));
        assert_eq!(picked.get("price").and_then(Value::as_i64), Some(500));
        assert!(!r.includes(&picked));
        assert!(pick(text, &["zeros"]).get("zeros").and_then(Value::as_str).is_some_and(|z| z.ends_with("}]")));
    }

    /// An advanced rule naming a bucket outside the configured ones makes the draft invalid.
    #[test]
    fn s3_advanced_rules_stay_within_the_configured_buckets() {
        let draft = |bucket: &str| crate::json::parse(format!(r#"{{"advanced_snippet":{{"value":[{{"bucket":"{bucket}","prefix":"a/"}}]}},"rules":[]}}"#).as_bytes()).unwrap().unwrap();
        let buckets = vec!["docs".to_string()];
        assert_eq!(validate("s3", &draft("docs"), &buckets).get("state").and_then(Value::as_str), Some("valid"));
        let v = validate("s3", &draft("payroll"), &buckets);
        assert_eq!(v.get("state").and_then(Value::as_str), Some("invalid"));
        assert!(v.to_json().contains("payroll"));
    }
}
