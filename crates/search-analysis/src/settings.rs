//! Elasticsearch's `Settings` model for analysis component definitions.
//!
//! Elasticsearch flattens settings JSON into dotted keys whose values are strings or lists of
//! strings (numbers and booleans keep their JSON text), and every analysis factory reads its
//! parameters through `get`, `getAsList`, `getAsInt` and `getAsBoolean` with their particular
//! leniencies and error texts: a scalar where a list is expected is split on commas, a list where
//! a scalar is expected reads as `"[a, b]"`, `"1.0"` is not an int, `"TRUE"` is not a boolean.
//! Factories here read parameters the same way so the same definitions are accepted and rejected.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::{AnalysisError, Result};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SettingValue {
    Str(String),
    List(Vec<String>),
    Null,
}

/// A flattened settings group (keys relative to the group).
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Settings {
    map: BTreeMap<String, SettingValue>,
}

fn scalar_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

impl Settings {
    pub fn new() -> Settings {
        Settings::default()
    }

    /// Flattens a JSON object (nested objects or dotted keys, or both).
    pub fn from_json(v: &Value) -> Result<Settings> {
        let mut s = Settings::default();
        if let Value::Object(_) = v {
            s.load(v, String::new())?;
        } else if !v.is_null() {
            return Err(AnalysisError::illegal(format!(
                "expected an object for analysis settings but got [{v}]"
            )));
        }
        Ok(s)
    }

    fn load(&mut self, v: &Value, prefix: String) -> Result<()> {
        match v {
            Value::Object(m) => {
                for (k, v) in m {
                    let key = if prefix.is_empty() {
                        k.clone()
                    } else {
                        format!("{prefix}.{k}")
                    };
                    match v {
                        Value::Object(_) => self.load(v, key)?,
                        _ => self.load(v, key)?,
                    }
                }
            }
            Value::Array(items) => {
                let mut list = Vec::with_capacity(items.len());
                for it in items {
                    match scalar_text(it) {
                        Some(s) => list.push(s),
                        None => {
                            return Err(AnalysisError::other(
                                "illegal_state_exception",
                                500,
                                "only value lists are allowed in serialized settings",
                            ));
                        }
                    }
                }
                self.map.insert(prefix, SettingValue::List(list));
            }
            Value::Null => {
                self.map.insert(prefix, SettingValue::Null);
            }
            other => {
                self.map.insert(
                    prefix,
                    SettingValue::Str(scalar_text(other).unwrap_or_default()),
                );
            }
        }
        Ok(())
    }

    pub fn put(&mut self, key: &str, value: &str) {
        self.map
            .insert(key.to_string(), SettingValue::Str(value.to_string()));
    }

    pub fn put_list(&mut self, key: &str, values: Vec<String>) {
        self.map.insert(key.to_string(), SettingValue::List(values));
    }

    pub fn contains(&self, key: &str) -> bool {
        matches!(
            self.map.get(key),
            Some(SettingValue::Str(_) | SettingValue::List(_))
        )
    }

    /// `Settings.get`: lists read as their `toString` (`[a, b]`).
    pub fn get(&self, key: &str) -> Option<String> {
        match self.map.get(key)? {
            SettingValue::Str(s) => Some(s.clone()),
            SettingValue::List(l) => Some(format!("[{}]", l.join(", "))),
            SettingValue::Null => None,
        }
    }

    /// `Settings.getAsList(key, null)`: a list as is, a string split on commas and trimmed
    /// (Java's `split(",")`, so trailing empty items are dropped); None when absent or empty.
    pub fn get_list(&self, key: &str) -> Option<Vec<String>> {
        match self.map.get(key)? {
            SettingValue::List(l) => Some(l.clone()),
            SettingValue::Str(s) => {
                let parts = java_split_comma(s);
                if parts.is_empty() {
                    None
                } else {
                    Some(parts.into_iter().map(|p| p.trim().to_string()).collect())
                }
            }
            SettingValue::Null => None,
        }
    }

    /// `Settings.getAsList(key, null, false)`: a string is one item (trimmed), not split.
    pub fn get_list_unsplit(&self, key: &str) -> Option<Vec<String>> {
        match self.map.get(key)? {
            SettingValue::List(l) => Some(l.clone()),
            SettingValue::Str(s) => Some(vec![s.trim_matches(|c: char| c <= ' ').to_string()]),
            SettingValue::Null => None,
        }
    }

    /// `getAsList(key)` with an empty default.
    pub fn list_or_empty(&self, key: &str) -> Vec<String> {
        self.get_list(key).unwrap_or_default()
    }

    /// `getAsBoolean(key, default)`: `Booleans.parseBoolean(value, default)`, where a value
    /// without text (empty or whitespace only) also gives the default.
    pub fn get_bool(&self, key: &str, default: bool) -> Result<bool> {
        match self.get(key) {
            None => Ok(default),
            Some(v) if v.chars().all(char::is_whitespace) => Ok(default),
            Some(v) => parse_bool(&v),
        }
    }

    /// Java's `Boolean.parseBoolean` (what Lucene's factories use): true only for `true` in
    /// any case, anything else is false.
    pub fn get_java_bool(&self, key: &str, default: bool) -> bool {
        match self.get(key) {
            None => default,
            Some(v) => v.eq_ignore_ascii_case("true"),
        }
    }

    /// `getAsInt(key, default)`.
    pub fn get_int(&self, key: &str, default: i32) -> Result<i32> {
        match self.get(key) {
            None => Ok(default),
            Some(v) => parse_int(key, &v),
        }
    }

    /// `getGroups(prefix)`: `prefix.name.rest` keys grouped by `name`.
    pub fn groups(&self, prefix: &str) -> BTreeMap<String, Settings> {
        let mut out: BTreeMap<String, Settings> = BTreeMap::new();
        let p = format!("{prefix}.");
        for (k, v) in self.map.range(p.clone()..) {
            let Some(rest) = k.strip_prefix(&p) else {
                break;
            };
            let Some((name, sub)) = rest.split_once('.') else {
                continue;
            };
            out.entry(name.to_string())
                .or_default()
                .map
                .insert(sub.to_string(), v.clone());
        }
        out
    }

    /// The sub-settings under `prefix.` (`getAsSettings`).
    pub fn by_prefix(&self, prefix: &str) -> Settings {
        let p = format!("{prefix}.");
        let mut s = Settings::default();
        for (k, v) in self.map.range(p.clone()..) {
            let Some(rest) = k.strip_prefix(&p) else {
                break;
            };
            s.map.insert(rest.to_string(), v.clone());
        }
        s
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.map.keys().map(String::as_str)
    }

    /// Renders as Elasticsearch's `Strings.toString(settings)`: flat keys, sorted, string values.
    pub fn to_flat_json(&self) -> String {
        let mut m = serde_json::Map::new();
        for (k, v) in &self.map {
            let v = match v {
                SettingValue::Str(s) => serde_json::Value::String(s.clone()),
                SettingValue::List(l) => serde_json::Value::Array(
                    l.iter()
                        .map(|x| serde_json::Value::String(x.clone()))
                        .collect(),
                ),
                SettingValue::Null => serde_json::Value::Null,
            };
            m.insert(k.clone(), v);
        }
        serde_json::Value::Object(m).to_string()
    }
}

/// Java's `String.split(",")`: trailing empty strings are removed.
pub(crate) fn java_split_comma(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut parts: Vec<&str> = s.split(',').collect();
    while parts.last().is_some_and(|p| p.is_empty()) {
        parts.pop();
    }
    parts
}

/// `Booleans.parseBoolean`: exactly `true` or `false`.
pub(crate) fn parse_bool(v: &str) -> Result<bool> {
    match v {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(AnalysisError::illegal(format!(
            "Failed to parse value [{v}] as only [true] or [false] are allowed."
        ))),
    }
}

/// `Integer.parseInt` (an optional sign and decimal digits, any Unicode digits), with
/// `Settings.getAsInt`'s error.
pub(crate) fn parse_int(key: &str, v: &str) -> Result<i32> {
    java_parse_int(v).ok_or_else(|| {
        AnalysisError::settings(format!(
            "Failed to parse int setting [{key}] with value [{v}]"
        ))
        .with_cause(AnalysisError::other(
            "number_format_exception",
            400,
            format!("For input string: \"{v}\""),
        ))
    })
}

pub(crate) fn java_parse_int(v: &str) -> Option<i32> {
    let (neg, digits) = match v.as_bytes().first()? {
        b'-' => (true, &v[1..]),
        b'+' => (false, &v[1..]),
        _ => (false, v),
    };
    if digits.is_empty() {
        return None;
    }
    let mut n: i64 = 0;
    for c in digits.chars() {
        let d = if c.is_ascii_digit() {
            c as i64 - '0' as i64
        } else if crate::unicode::is_digit(c) {
            crate::unicode::digit_value(c)? as i64
        } else {
            return None;
        };
        n = n * 10 + d;
        if n > i32::MAX as i64 + 1 {
            return None;
        }
    }
    let n = if neg { -n } else { n };
    i32::try_from(n).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn flattens_and_groups() {
        let s = Settings::from_json(&json!({
            "filter": {"my_stop": {"type": "stop", "stopwords": ["a", "b"], "ignore_case": true}, "x": {"type": "lowercase"}},
            "analyzer.a.tokenizer": "standard",
            "analyzer": {"a": {"filter": "x, my_stop"}, "b": {"min": 2}},
        }))
        .unwrap();
        let g = s.groups("filter");
        assert_eq!(g.len(), 2);
        let stop = &g["my_stop"];
        assert_eq!(stop.get("type").as_deref(), Some("stop"));
        assert_eq!(stop.get_list("stopwords").unwrap(), vec!["a", "b"]);
        assert_eq!(stop.get("stopwords").as_deref(), Some("[a, b]"));
        assert!(stop.get_bool("ignore_case", false).unwrap());
        let a = &s.groups("analyzer")["a"];
        assert_eq!(a.get("tokenizer").as_deref(), Some("standard"));
        assert_eq!(a.get_list("filter").unwrap(), vec!["x", "my_stop"]);
        assert_eq!(s.groups("analyzer")["b"].get_int("min", 0).unwrap(), 2);
    }

    #[test]
    fn java_parsing() {
        assert_eq!(java_parse_int("12"), Some(12));
        assert_eq!(java_parse_int("+3"), Some(3));
        assert_eq!(java_parse_int("-2147483648"), Some(i32::MIN));
        assert_eq!(java_parse_int("2147483648"), None);
        assert_eq!(java_parse_int("1.0"), None);
        assert_eq!(java_parse_int(" 1"), None);
        assert_eq!(java_parse_int("٣"), Some(3));
        let e = parse_int("min_gram", "abc").unwrap_err();
        assert_eq!(e.kind, "settings_exception");
        assert_eq!(
            e.reason,
            "Failed to parse int setting [min_gram] with value [abc]"
        );
        assert_eq!(e.caused_by.unwrap().reason, "For input string: \"abc\"");
        assert!(parse_bool("TRUE").is_err());
        assert_eq!(java_split_comma("a,,b,,"), vec!["a", "", "b"]);
        assert!(java_split_comma(",").is_empty());
    }
}
