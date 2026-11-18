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

/// The `validation` of a draft filtering of a `service` connector roll runs.
pub fn validate(service: &str, draft: &Value) -> Value {
    let mut errors: Vec<Value> = match Rules::compile(draft) {
        Ok(_) => Vec::new(),
        Err(p) => p.into_iter().map(|p| obj! { "ids" => crate::arr![p.id], "messages" => crate::arr![p.message] }).collect(),
    };
    if let Some(a) = advanced(draft) {
        let problem = match service {
            "s3" => s3_rules(a).err(),
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
    }
}
