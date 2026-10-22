//! The stash and response paths, with the Java runner's exact rules.
//!
//! - A path is split on unescaped dots (`hits.hits.0._source`, `settings.index\.number_of_shards`).
//!   Like Elasticsearch's `ObjectPath.parsePath`, a backslash is dropped and makes the *next* dot
//!   literal, wherever that dot is.
//! - A path element that starts with `$` or contains `${name}` is replaced by the stashed value
//!   (`nodes.$master.http`, `field1.e${placeholder}ments`), and `_arbitrary_key_` picks a key of a
//!   map.
//! - `$name` (or `$name.sub.path`) unstashes a value of any type; `prefix ${name} suffix`
//!   interpolates `toString()` of the value; `\${name}` stays literal (minus the backslash).
//! - Request bodies and expected maps are unstashed recursively, keys included; `$_path` expands
//!   to the location being unstashed (used by the docs tests).

use std::borrow::Cow;

use crate::value::{Map, Value};

/// Values saved by `set`/`transform_and_set`, plus `body` (the last response).
#[derive(Debug, Default, Clone)]
pub struct Stash {
    root: Value,
}

/// A failure while resolving a path or a stashed value; the message mirrors the Java runner's.
#[derive(Debug, Clone, PartialEq)]
pub struct StashError(pub String);

impl std::fmt::Display for StashError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StashError {}

type Result<T> = std::result::Result<T, StashError>;

fn err<T>(msg: impl Into<String>) -> Result<T> {
    Err(StashError(msg.into()))
}

/// Finds the next `${...}` reference in `s` at or after `from`: `(start, end, escaped, name)` where
/// `start` includes a preceding backslash if there is one.
fn next_extended_key(s: &str, from: usize) -> Option<(usize, usize, bool, &str)> {
    let bytes = s.as_bytes();
    let mut i = from;
    while let Some(off) = s[i..].find("${") {
        let open = i + off;
        let name_start = open + 2;
        match s[name_start..].find('}') {
            Some(len) if len > 0 => {
                let escaped = open > from && bytes[open - 1] == b'\\';
                let start = if escaped { open - 1 } else { open };
                return Some((start, name_start + len + 1, escaped, &s[name_start..name_start + len]));
            }
            // `${}` (empty name) is not a reference; keep looking after it.
            Some(_) => i = name_start,
            None => return None,
        }
    }
    None
}

impl Stash {
    pub fn new() -> Stash {
        Stash { root: Value::Map(Map::new()) }
    }

    pub fn clear(&mut self) {
        self.root = Value::Map(Map::new());
    }

    pub fn set(&mut self, key: &str, value: Value) {
        if let Value::Map(m) = &mut self.root {
            m.insert(key.to_owned(), value);
        } else {
            let mut m = Map::new();
            m.insert(key.to_owned(), value);
            self.root = Value::Map(m);
        }
    }

    pub fn get_raw(&self, key: &str) -> Option<&Value> {
        self.root.get(key)
    }

    /// The whole stash as a map (for failure dumps).
    pub fn as_value(&self) -> &Value {
        &self.root
    }

    /// `Stash.containsStashedValue`: starts with `$` or contains a `${...}` reference.
    pub fn contains_stashed_value(s: &str) -> bool {
        s.starts_with('$') || next_extended_key(s, 0).is_some()
    }

    /// `Stash.isStashedValue`: starts with `$` or is exactly one `${...}` reference.
    pub fn is_stashed_value(s: &str) -> bool {
        s.starts_with('$') || matches!(next_extended_key(s, 0), Some((0, end, false, _)) if end == s.len())
    }

    /// `Stash.getValue`: `$path` returns the stashed value; strings with `${...}` are interpolated.
    pub fn get_value(&self, key: &str) -> Result<Value> {
        let b = key.as_bytes();
        if b.first() == Some(&b'$') && b.get(1) != Some(&b'{') {
            return self.unstash(&key[1..]).map(Cow::into_owned);
        }
        let Some(mut found) = next_extended_key(key, 0) else {
            return err(format!("Doesn't contain any stash keys [{key}]"));
        };
        let mut out = String::with_capacity(key.len());
        let mut last = 0;
        loop {
            let (start, end, escaped, name) = found;
            out.push_str(&key[last..start]);
            if escaped {
                out.push_str(&key[start + 1..end]);
            } else {
                out.push_str(&self.unstash(name)?.java_string());
            }
            last = end;
            match next_extended_key(key, end) {
                Some(f) => found = f,
                None => break,
            }
        }
        out.push_str(&key[last..]);
        Ok(Value::Str(out))
    }

    fn unstash(&self, path: &str) -> Result<Cow<'_, Value>> {
        match evaluate(&self.root, path, self)? {
            Some(v) if !v.is_null() => Ok(v),
            _ => err(format!("stashed value not found for key [{path}]")),
        }
    }

    /// `Stash.replaceStashedValues`: unstashes every key and string value of a tree.
    pub fn replace_stashed_values(&self, value: &Value) -> Result<Value> {
        let mut path = Vec::new();
        self.unstash_object(&mut path, value)
    }

    fn unstash_object(&self, path: &mut Vec<String>, value: &Value) -> Result<Value> {
        match value {
            Value::List(items) => {
                let mut out = Vec::with_capacity(items.len());
                for (i, item) in items.iter().enumerate() {
                    path.push(i.to_string());
                    let v = match item {
                        Value::Str(s) if Stash::contains_stashed_value(s) => self.get_value_at(path, s)?,
                        _ => self.unstash_object(path, item)?,
                    };
                    path.pop();
                    out.push(v);
                }
                Ok(Value::List(out))
            }
            Value::Map(map) => {
                let mut out = Map::with_capacity(map.len());
                for (k, v) in map {
                    let key = if Stash::contains_stashed_value(k) {
                        self.get_value(k)?.java_string()
                    } else {
                        k.clone()
                    };
                    path.push(key.clone());
                    let v = match v {
                        Value::Str(s) if Stash::contains_stashed_value(s) => self.get_value_at(path, s)?,
                        _ => self.unstash_object(path, v)?,
                    };
                    path.pop();
                    if out.contains_key(&key) {
                        return err(format!(
                            "Unstashing has caused a key conflict! The map is [{}] and the key is [{k}] which unstashes to [{key}]",
                            Value::Map(out).java_string()
                        ));
                    }
                    out.insert(key, v);
                }
                Ok(Value::Map(out))
            }
            other => Ok(other.clone()),
        }
    }

    /// `getValue` with `$_path` support: `$_path` becomes the dotted location being unstashed.
    fn get_value_at(&self, path: &[String], key: &str) -> Result<Value> {
        if !key.contains("$_path") {
            return self.get_value(key);
        }
        let built = path.iter().map(|p| p.replace('.', "\\.")).collect::<Vec<_>>().join(".");
        self.get_value(&key.replace("$_path", &built))
    }
}

/// `ObjectPath.parsePath`.
pub fn parse_path(path: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut escape = false;
    for c in path.chars() {
        if c == '\\' {
            escape = true;
            continue;
        }
        if c == '.' {
            if escape {
                escape = false;
            } else {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                continue;
            }
        }
        current.push(c);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// `ObjectPath.evaluate(path, stash)`: `Ok(None)` when a level is missing, an error when a level
/// can't be descended into (a scalar, a non-numeric list index, an index out of bounds).
pub fn evaluate<'a>(root: &'a Value, path: &str, stash: &Stash) -> Result<Option<Cow<'a, Value>>> {
    let mut current = Cow::Borrowed(root);
    for part in parse_path(path) {
        let key = if Stash::contains_stashed_value(&part) {
            stash.get_value(&part)?.java_string()
        } else {
            part
        };
        let next = match current {
            Cow::Borrowed(v) => step(v, &key)?.map(|s| match s {
                Step::Child(c) => Cow::Borrowed(c),
                Step::Key(k) => Cow::Owned(Value::Str(k)),
            }),
            Cow::Owned(v) => step(&v, &key)?.map(|s| match s {
                Step::Child(c) => Cow::Owned(c.clone()),
                Step::Key(k) => Cow::Owned(Value::Str(k)),
            }),
        };
        match next {
            Some(v) if !v.is_null() => current = v,
            _ => return Ok(None),
        }
    }
    Ok(Some(current))
}

enum Step<'a> {
    Child(&'a Value),
    Key(String),
}

fn step<'a>(v: &'a Value, key: &str) -> Result<Option<Step<'a>>> {
    match v {
        Value::Map(map) => {
            if key == "_arbitrary_key_" {
                if map.is_empty() {
                    return err(format!("requested [{key}] but the map was empty"));
                }
                if map.contains_key(key) {
                    return err(format!("requested meta-key [{key}] but the map unexpectedly contains this key"));
                }
                return Ok(map.keys().next().map(|k| Step::Key(k.clone())));
            }
            Ok(map.get(key).map(Step::Child))
        }
        Value::List(items) => {
            let Ok(i) = key.parse::<i32>() else {
                return err(format!("element was a list, but [{key}] was not numeric"));
            };
            match usize::try_from(i).ok().and_then(|i| items.get(i)) {
                Some(item) => Ok(Some(Step::Child(item))),
                None => err(format!(
                    "element was a list with {} elements, but [{key}] was out of bounds",
                    items.len()
                )),
            }
        }
        other => err(format!(
            "no object found for [{key}] within object of class [class java.lang.{}]",
            other.java_class()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::json;

    fn stash() -> Stash {
        let mut s = Stash::new();
        s.set("id", Value::Str("abc".into()));
        s.set("n", Value::Int(5));
        s.set("f", Value::Float(1.5));
        s.set("master", Value::Str("node-1".into()));
        s.set("body", json(r#"{"hits":{"total":3},"nodes":{"node-1":{"name":"x"}}}"#));
        s
    }

    #[test]
    fn path_parsing() {
        assert_eq!(parse_path("a.b.0.c"), ["a", "b", "0", "c"]);
        assert_eq!(parse_path("settings.index\\.number_of_shards"), ["settings", "index.number_of_shards"]);
        assert_eq!(parse_path(""), Vec::<String>::new());
        assert_eq!(parse_path("a..b."), ["a", "b"]);
        // The backslash is dropped and escapes the next dot, wherever it is (Java quirk).
        assert_eq!(parse_path("a\\b.c.d"), ["ab.c", "d"]);
    }

    #[test]
    fn stashed_value_detection() {
        assert!(Stash::contains_stashed_value("$id"));
        assert!(Stash::contains_stashed_value("pre${id}post"));
        assert!(Stash::contains_stashed_value("pre\\${id}post"));
        assert!(!Stash::contains_stashed_value("pre${}post"));
        assert!(!Stash::contains_stashed_value("plain"));
        assert!(Stash::is_stashed_value("${id}"));
        assert!(!Stash::is_stashed_value("x${id}"));
        assert!(Stash::is_stashed_value("$body"));
    }

    #[test]
    fn get_value_variants() {
        let s = stash();
        assert_eq!(s.get_value("$n").unwrap(), Value::Int(5));
        assert_eq!(s.get_value("${n}").unwrap(), Value::Str("5".into()));
        assert_eq!(s.get_value("v${f}-${id}").unwrap(), Value::Str("v1.5-abc".into()));
        assert_eq!(s.get_value("\\${id}x").unwrap(), Value::Str("${id}x".into()));
        assert_eq!(s.get_value("$body.hits.total").unwrap(), Value::Int(3));
        assert_eq!(s.get_value("$missing").unwrap_err().0, "stashed value not found for key [missing]");
        assert_eq!(s.get_value("$body.nodes.$master.name").unwrap(), Value::Str("x".into()));
    }

    #[test]
    fn evaluate_paths() {
        let s = stash();
        let v = json(r#"{"a":{"b":[{"c":1},{"c":2}]},"m":{"k1":1},"e":{"elements":true},"s":"x"}"#);
        let get = |p: &str| evaluate(&v, p, &s).map(|o| o.map(Cow::into_owned));
        assert_eq!(get("a.b.1.c").unwrap(), Some(Value::Int(2)));
        assert_eq!(get("a.x.y").unwrap(), None);
        assert_eq!(get("").unwrap(), Some(v.clone()));
        assert_eq!(get("m._arbitrary_key_").unwrap(), Some(Value::Str("k1".into())));
        assert_eq!(get("a.b.x").unwrap_err().0, "element was a list, but [x] was not numeric");
        assert_eq!(get("a.b.5").unwrap_err().0, "element was a list with 2 elements, but [5] was out of bounds");
        assert!(get("s.t").unwrap_err().0.contains("no object found for [t]"));
        let mut s2 = s.clone();
        s2.set("placeholder", Value::Str("le".into()));
        assert_eq!(
            evaluate(&v, "e.e${placeholder}ments", &s2).unwrap().map(Cow::into_owned),
            Some(Value::Bool(true))
        );
    }

    #[test]
    fn replace_in_trees() {
        let s = stash();
        // "a$b" neither starts with '$' nor contains "${": not a reference.
        let out = s.replace_stashed_values(&json(r#"{"keep":"a$b"}"#)).unwrap();
        assert_eq!(out, json(r#"{"keep":"a$b"}"#));
        let body = json(r#"{"q":"$id","$master":{"x":["${n}","$n",{"deep":"$f"}]},"keep":"a b"}"#);
        let out = s.replace_stashed_values(&body).unwrap();
        assert_eq!(out, json(r#"{"q":"abc","node-1":{"x":["5",5,{"deep":1.5}]},"keep":"a b"}"#));
    }

    #[test]
    fn path_marker() {
        let mut s = Stash::new();
        s.set("body", json(r#"{"a":{"b.c":[10,20]}}"#));
        let out = s.replace_stashed_values(&json(r#"{"a":{"b.c":["$body.$_path"]}}"#)).unwrap();
        assert_eq!(out, json(r#"{"a":{"b.c":[10]}}"#));
    }

    #[test]
    fn key_conflict() {
        let mut s = Stash::new();
        s.set("k", Value::Str("a".into()));
        let e = s.replace_stashed_values(&json(r#"{"a":1,"$k":2}"#)).unwrap_err();
        assert!(e.0.starts_with("Unstashing has caused a key conflict!"), "{e}");
    }
}
