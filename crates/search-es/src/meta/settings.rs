//! `Settings`: Elasticsearch's flat, sorted string map of settings.
//!
//! Settings arrive as nested JSON (`{"index":{"number_of_shards":1}}`), dotted keys
//! (`"index.number_of_shards":"1"`) or a mix; they are flattened into dotted keys with string
//! values (numbers and booleans print as Java does, arrays stay lists of strings, `null` marks a
//! reset in updates). Output is either flat (`flat_settings=true`) or nested again, as
//! `GET _settings` prints them.

use crate::error::EsError;
use crate::json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub enum SettingValue {
    Str(String),
    List(Vec<String>),
    /// `null` in an update: reset to the default.
    Null,
}

impl SettingValue {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            SettingValue::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            SettingValue::Str(s) => Value::Str(s.clone()),
            SettingValue::List(l) => Value::Arr(l.iter().map(|s| Value::Str(s.clone())).collect()),
            SettingValue::Null => Value::Null,
        }
    }

    /// The value as Elasticsearch prints it in error messages (`[a, b]` for lists).
    pub fn display(&self) -> String {
        match self {
            SettingValue::Str(s) => s.clone(),
            SettingValue::List(l) => format!("[{}]", l.join(", ")),
            SettingValue::Null => "null".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Settings {
    map: BTreeMap<String, SettingValue>,
}

impl Settings {
    pub fn new() -> Settings {
        Settings::default()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.map.get(key).and_then(SettingValue::as_str)
    }

    pub fn get_value(&self, key: &str) -> Option<&SettingValue> {
        self.map.get(key)
    }

    pub fn get_list(&self, key: &str) -> Option<Vec<String>> {
        match self.map.get(key)? {
            SettingValue::List(l) => Some(l.clone()),
            SettingValue::Str(s) => Some(s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()),
            SettingValue::Null => None,
        }
    }

    pub fn contains(&self, key: &str) -> bool {
        self.map.contains_key(key)
    }

    pub fn insert(&mut self, key: impl Into<String>, v: SettingValue) {
        self.map.insert(key.into(), v);
    }

    pub fn set(&mut self, key: impl Into<String>, v: impl Into<String>) {
        self.map.insert(key.into(), SettingValue::Str(v.into()));
    }

    pub fn remove(&mut self, key: &str) -> Option<SettingValue> {
        self.map.remove(key)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &SettingValue)> {
        self.map.iter()
    }

    pub fn keys(&self) -> impl Iterator<Item = &String> {
        self.map.keys()
    }

    /// Keys under `prefix` (which should end with `.`), with the prefix removed.
    pub fn by_prefix(&self, prefix: &str) -> Settings {
        let mut out = Settings::new();
        for (k, v) in self.map.range(prefix.to_string()..) {
            let Some(rest) = k.strip_prefix(prefix) else { break };
            out.map.insert(rest.to_string(), v.clone());
        }
        out
    }

    /// Settings from a JSON object, flattened. Fails like `Settings.fromXContent` on keys that
    /// flatten to the same name.
    pub fn from_json(v: &Value) -> Result<Settings, EsError> {
        let mut s = Settings::new();
        if let Value::Obj(m) = v {
            flatten(&mut s.map, "", m)?;
        }
        Ok(s)
    }

    /// `Settings.Builder.normalizePrefix`: keys without `prefix` get it (replacing a prefixed
    /// key with the same name).
    pub fn normalize_prefix(&mut self, prefix: &str) {
        let unprefixed: Vec<String> = self.map.keys().filter(|k| !k.starts_with(prefix)).cloned().collect();
        for k in unprefixed {
            let v = self.map.remove(&k).unwrap();
            self.map.insert(format!("{prefix}{k}"), v);
        }
    }

    /// Overlays `other` (its `Null`s remove keys; a `null` key with `*` removes every match).
    pub fn merge(&mut self, other: &Settings) {
        for (k, v) in &other.map {
            match v {
                SettingValue::Null if k.contains('*') => {
                    self.map.retain(|key, _| !crate::xcontent::filter::glob_match(k, key));
                }
                SettingValue::Null => {
                    self.map.remove(k);
                }
                v => {
                    self.map.insert(k.clone(), v.clone());
                }
            }
        }
    }

    /// Overlays `other` keeping its `null`s as values (index creation does this).
    pub fn overlay_keeping_nulls(&mut self, other: &Settings) {
        for (k, v) in &other.map {
            self.map.insert(k.clone(), v.clone());
        }
    }

    /// Flat JSON object (`flat_settings=true`), keys sorted.
    pub fn to_flat_json(&self) -> Value {
        let mut m = Map::new();
        for (k, v) in &self.map {
            m.insert(k.clone(), v.to_value());
        }
        Value::Obj(m)
    }

    /// Nested JSON object as Elasticsearch prints it (`Settings.getAsStructuredMap`): groups
    /// are `HashMap`s, so keys come in Java's hash order; a value and a group with the same
    /// name keep the group's keys dotted; groups whose keys are `0..n` become arrays.
    pub fn to_nested_json(&self) -> Value {
        let mut root = JMap::default();
        for (k, v) in &self.map {
            process_setting(&mut root, "", k, v.to_value());
        }
        root.into_value(true)
    }

    pub fn to_json(&self, flat: bool) -> Value {
        if flat { self.to_flat_json() } else { self.to_nested_json() }
    }
}

/// A `HashMap` under construction: entries in insertion order, printed in hash order.
#[derive(Default)]
struct JMap {
    entries: Vec<(String, JVal)>,
}

enum JVal {
    Leaf(Value),
    Map(JMap),
}

impl JMap {
    fn get_mut(&mut self, k: &str) -> Option<&mut JVal> {
        self.entries.iter_mut().find(|(ek, _)| ek == k).map(|(_, v)| v)
    }

    fn put(&mut self, k: String, v: JVal) {
        match self.entries.iter_mut().find(|(ek, _)| *ek == k) {
            Some(e) => e.1 = v,
            None => self.entries.push((k, v)),
        }
    }

    /// `Maps.newMapWithExpectedSize(2)`: a `HashMap` of capacity 3, a table of 4.
    const CAPACITY: usize = 4;

    fn into_value(self, top: bool) -> Value {
        let keys: Vec<&String> = self.entries.iter().map(|(k, _)| k).collect();
        let order = crate::json::java_hash_order(&keys, Self::CAPACITY);
        let mut slots: Vec<Option<(String, JVal)>> = self.entries.into_iter().map(Some).collect();
        let mut ordered: Vec<(String, JVal)> = Vec::with_capacity(slots.len());
        for i in order {
            if let Some(e) = slots[i].take() {
                ordered.push(e);
            }
        }
        // `convertMapsToArrays` (not applied to the top level itself).
        if !top && !ordered.is_empty() {
            let max = ordered.iter().map(|(k, _)| if !k.is_empty() && k.bytes().all(|b| b.is_ascii_digit()) { k.parse::<usize>().ok() } else { None }).collect::<Option<Vec<_>>>();
            if let Some(idx) = max
                && idx.iter().max().is_some_and(|m| m + 1 == ordered.len())
            {
                let mut arr: Vec<Option<Value>> = (0..ordered.len()).map(|_| None).collect();
                for ((_, v), i) in ordered.into_iter().zip(idx) {
                    arr[i] = Some(v.into_value());
                }
                if arr.iter().all(Option::is_some) {
                    return Value::Arr(arr.into_iter().flatten().collect());
                }
                return Value::Null;
            }
        }
        let mut m = Map::with_capacity(ordered.len());
        for (k, v) in ordered {
            m.insert(k, v.into_value());
        }
        Value::Obj(m)
    }
}

impl JVal {
    fn into_value(self) -> Value {
        match self {
            JVal::Leaf(v) => v,
            JVal::Map(m) => crate::stack::maybe_grow(|| m.into_value(false)),
        }
    }
}

/// `Settings.processSetting`.
fn process_setting(map: &mut JMap, prefix: &str, setting: &str, value: Value) {
    match setting.find('.') {
        None => {
            let key = format!("{prefix}{setting}");
            if let Some(JVal::Map(inner)) = map.get_mut(&key) {
                // A value where a group exists: the group's entries move up, dotted.
                let moved: Vec<(String, JVal)> = std::mem::take(&mut inner.entries);
                for (k, v) in moved {
                    map.put(format!("{key}.{k}"), v);
                }
            }
            map.put(key, JVal::Leaf(value));
        }
        Some(i) => {
            let (key, rest) = (&setting[..i], &setting[i + 1..]);
            let full = format!("{prefix}{key}");
            match map.get_mut(&full) {
                None => {
                    let mut inner = JMap::default();
                    crate::stack::maybe_grow(|| process_setting(&mut inner, "", rest, value));
                    map.put(full, JVal::Map(inner));
                }
                Some(JVal::Map(inner)) => crate::stack::maybe_grow(|| process_setting(inner, "", rest, value)),
                Some(JVal::Leaf(_)) => crate::stack::maybe_grow(|| process_setting(map, &format!("{full}."), rest, value)),
            }
        }
    }
}

fn scalar_string(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => other.to_es_string(),
    }
}

fn flatten(out: &mut BTreeMap<String, SettingValue>, prefix: &str, m: &Map) -> Result<(), EsError> {
    for (k, v) in m {
        let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        match v {
            Value::Obj(inner) => crate::stack::maybe_grow(|| flatten(out, &key, inner))?,
            Value::Arr(items) => {
                let mut list = Vec::with_capacity(items.len());
                for (i, it) in items.iter().enumerate() {
                    match it {
                        Value::Obj(inner) => crate::stack::maybe_grow(|| flatten(out, &format!("{key}.{i}"), inner))?,
                        Value::Arr(_) => list.push(it.to_json()),
                        Value::Null => {}
                        other => list.push(scalar_string(other)),
                    }
                }
                put(out, key, SettingValue::List(list))?;
            }
            Value::Null => put(out, key, SettingValue::Null)?,
            other => put(out, key, SettingValue::Str(scalar_string(other)))?,
        }
    }
    Ok(())
}

fn put(out: &mut BTreeMap<String, SettingValue>, key: String, v: SettingValue) -> Result<(), EsError> {
    if let Some(prev) = out.get(&key) {
        return Err(EsError::new(
            400,
            "settings_exception",
            format!("duplicate settings key [{key}] found, previous value [{}], current value [{}]", prev.display(), v.display()),
        ));
    }
    out.insert(key, v);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::parse;

    fn s(json: &str) -> Settings {
        Settings::from_json(&parse(json.as_bytes()).unwrap().unwrap()).unwrap()
    }

    #[test]
    fn flatten_and_normalize() {
        let mut st = s(r#"{"number_of_shards":2,"index":{"refresh_interval":"5s"},"index.blocks.write":false,"analysis":{"analyzer":{"a":{"stopwords":["x","y"]}}},"f":1.0}"#);
        st.normalize_prefix("index.");
        assert_eq!(st.get("index.number_of_shards"), Some("2"));
        assert_eq!(st.get("index.blocks.write"), Some("false"));
        assert_eq!(st.get("index.f"), Some("1.0"));
        assert_eq!(st.get_list("index.analysis.analyzer.a.stopwords").unwrap(), vec!["x", "y"]);
        assert_eq!(
            st.to_nested_json().to_json(),
            r#"{"index":{"refresh_interval":"5s","analysis":{"analyzer":{"a":{"stopwords":["x","y"]}}},"number_of_shards":"2","blocks":{"write":"false"},"f":"1.0"}}"#
        );
    }

    #[test]
    fn unprefixed_wins() {
        let mut st = s(r#"{"index":{"number_of_shards":1},"number_of_shards":2}"#);
        st.normalize_prefix("index.");
        assert_eq!(st.get("index.number_of_shards"), Some("2"));
    }

    #[test]
    fn duplicate_flattened_keys() {
        let v = parse(br#"{"a.b":1,"a":{"b":2}}"#).unwrap().unwrap();
        let e = Settings::from_json(&v).unwrap_err();
        assert_eq!(e.kind, "settings_exception");
    }

    #[test]
    fn prefix_views() {
        let st = s(r#"{"index.routing.allocation.include._tier_preference":"data_content","index.routing.allocation.include.zone":"a","index.uuid":"x"}"#);
        let inc = st.by_prefix("index.routing.allocation.include.");
        assert_eq!(inc.len(), 2);
        assert_eq!(inc.get("zone"), Some("a"));
    }
}
