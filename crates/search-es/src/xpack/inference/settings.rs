//! Reading an endpoint's settings maps the way Elasticsearch's `ServiceUtils` and
//! `InferenceUtils` extractors do: each `extract_*` removes its key from the map, type-checks it
//! as Java would (`removeAsType`) and collects a validation error instead of failing at once, so
//! one request reports every bad setting, in extraction order, as a `validation_exception`.
//! Whatever is left in a map afterwards is "unknown to the service" ([`unknown`]).

use crate::error::{EsError, EsResult};
use crate::json::{Map, Number, Value};

/// A settings map being read, with the errors found so far.
pub struct Reader {
    pub map: Map,
    scope: &'static str,
    errors: Vec<String>,
}

/// `Validation Failed: 1: …;` as a `validation_exception` (Elasticsearch's `ValidationException`).
pub fn validation(errors: &[String]) -> EsError {
    let mut s = String::from("Validation Failed: ");
    for (i, e) in errors.iter().enumerate() {
        s.push_str(&format!("{}: {e};", i + 1));
    }
    EsError::new(400, "validation_exception", s)
}

/// Java's `toString` of a parsed JSON value (`{a=1, b=[x, y]}`), as messages print maps.
pub fn java_string(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Num(Number::Float(f)) => crate::json::write::java_double(*f),
        Value::Num(_) => v.to_es_string(),
        Value::Str(s) => s.clone(),
        Value::Arr(a) => format!("[{}]", a.iter().map(java_string).collect::<Vec<_>>().join(", ")),
        Value::Obj(m) => {
            let m = crate::json::java_hash_map(m.clone(), 16);
            format!("{{{}}}", m.iter().map(|(k, v)| format!("{k}={}", java_string(v))).collect::<Vec<_>>().join(", "))
        }
    }
}

/// `Configuration contains settings [{…}] unknown to the [service] service`.
pub fn unknown(map: &Map, service: &str) -> EsError {
    EsError::status(400, format!("Configuration contains settings [{}] unknown to the [{service}] service", java_string(&Value::Obj(map.clone()))))
}

/// Fails when settings are left over.
pub fn check_empty(map: &Map, service: &str) -> EsResult<()> {
    if map.is_empty() { Ok(()) } else { Err(unknown(map, service)) }
}

fn type_error(key: &str, v: &Value, ty: &str) -> String {
    format!("field [{key}] is not of the expected type. The value [{}] cannot be converted to a [{ty}]", java_string(v))
}

/// A JSON integer that fits Java's `int` (what Jackson parses as an `Integer`).
fn as_int(v: &Value) -> Option<i64> {
    match v {
        Value::Num(Number::Int(i)) if i32::try_from(*i).is_ok() => Some(*i),
        _ => None,
    }
}

impl Reader {
    pub fn new(map: Map, scope: &'static str) -> Reader {
        Reader { map, scope, errors: Vec::new() }
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.errors.push(msg.into());
    }

    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// The errors so far as a `validation_exception`, or the map that is left.
    pub fn finish(self) -> EsResult<Map> {
        if self.errors.is_empty() { Ok(self.map) } else { Err(validation(&self.errors)) }
    }

    /// Raises the errors so far without consuming the reader.
    pub fn check(&mut self) -> EsResult<()> {
        if self.errors.is_empty() { Ok(()) } else { Err(validation(&std::mem::take(&mut self.errors))) }
    }

    fn take(&mut self, key: &str) -> Option<Value> {
        self.map.shift_remove(key)
    }

    /// `removeAsType(map, key, String.class)`.
    fn take_str(&mut self, key: &str) -> Option<String> {
        match self.take(key)? {
            Value::Str(s) => Some(s),
            other => {
                self.errors.push(type_error(key, &other, "String"));
                None
            }
        }
    }

    pub fn required_str(&mut self, key: &str) -> Option<String> {
        let before = self.errors.len();
        let s = self.take_str(key);
        if self.errors.len() > before {
            return None;
        }
        match s {
            None => {
                self.errors.push(format!("[{}] does not contain the required setting [{key}]", self.scope));
                None
            }
            Some(s) if s.is_empty() => {
                self.errors.push(format!("[{}] Invalid value empty string. [{key}] must be a non-empty string", self.scope));
                None
            }
            Some(s) => Some(s),
        }
    }

    pub fn optional_str(&mut self, key: &str) -> Option<String> {
        let s = self.take_str(key)?;
        if s.is_empty() {
            self.errors.push(format!("[{}] Invalid value empty string. [{key}] must be a non-empty string", self.scope));
            return None;
        }
        Some(s)
    }

    /// A string that must be one of `allowed` (compared ignoring case, as `fromString` does);
    /// returns it in lower case.
    pub fn optional_enum(&mut self, key: &str, allowed: &[&str]) -> Option<String> {
        let s = self.optional_str(key)?;
        let lower = s.to_ascii_lowercase();
        if allowed.contains(&lower.as_str()) {
            return Some(lower);
        }
        let mut sorted: Vec<&str> = allowed.to_vec();
        sorted.sort_unstable();
        self.errors.push(format!("[{}] Invalid value [{s}] received. [{key}] must be one of [{}]", self.scope, sorted.join(", ")));
        None
    }

    pub fn optional_int(&mut self, key: &str) -> Option<i64> {
        let v = self.take(key)?;
        match as_int(&v) {
            Some(i) => Some(i),
            None => {
                self.errors.push(type_error(key, &v, "Integer"));
                None
            }
        }
    }

    pub fn optional_positive_int(&mut self, key: &str) -> Option<i64> {
        let i = self.optional_int(key)?;
        if i <= 0 {
            self.errors.push(format!("[{}] Invalid value [{i}]. [{key}] must be a positive integer", self.scope));
            return None;
        }
        Some(i)
    }

    pub fn required_positive_int(&mut self, key: &str) -> Option<i64> {
        let before = self.errors.len();
        let v = self.optional_positive_int(key);
        if v.is_none() && self.errors.len() == before {
            self.errors.push(format!("[{}] does not contain the required setting [{key}]", self.scope));
        }
        v
    }

    pub fn optional_bool(&mut self, key: &str) -> Option<bool> {
        match self.take(key)? {
            Value::Bool(b) => Some(b),
            other => {
                self.errors.push(type_error(key, &other, "Boolean"));
                None
            }
        }
    }

    /// A number read as Java's `Float` (`removeAsType(…, Float.class)` accepts any number).
    pub fn optional_float(&mut self, key: &str) -> Option<f64> {
        match self.take(key)? {
            Value::Num(n) => Some(n.as_f64()),
            other => {
                self.errors.push(type_error(key, &other, "Float"));
                None
            }
        }
    }

    pub fn optional_map(&mut self, key: &str) -> Option<Map> {
        match self.take(key)? {
            Value::Obj(m) => Some(m),
            other => {
                self.errors.push(type_error(key, &other, "Map"));
                None
            }
        }
    }

    /// `extractOptionalUri`: a URL Java's `URI` accepts (an absolute one, for roll: it is sent
    /// requests).
    pub fn optional_url(&mut self, key: &str) -> Option<String> {
        let s = self.optional_str(key)?;
        match reqwest::Url::parse(&s) {
            Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some_and(|h| !h.is_empty()) => Some(s),
            Ok(_) => {
                self.errors.push(format!("[{}] Invalid url [{s}] received for field [{key}]. Error: unable to parse url [{s}]. Reason: Expected scheme-specific part", self.scope));
                None
            }
            Err(e) => {
                self.errors.push(format!("[{}] Invalid url [{s}] received for field [{key}]. Error: unable to parse url [{s}]. Reason: {e}", self.scope));
                None
            }
        }
    }

    /// `extractSimilarity`: `cosine`, `dot_product` or `l2_norm`.
    pub fn similarity(&mut self) -> Option<String> {
        self.optional_enum("similarity", &["cosine", "dot_product", "l2_norm"])
    }

    /// `RateLimitSettings.of`: `{"requests_per_minute": n}`, `default` when absent.
    pub fn rate_limit(&mut self, default: i64) -> Value {
        let mut rpm = default;
        if let Some(m) = self.optional_map("rate_limit") {
            let mut r = Reader::new(m, "rate_limit");
            if let Some(n) = r.optional_positive_int_long("requests_per_minute") {
                rpm = n;
            }
            self.errors.append(&mut r.errors);
        }
        crate::obj! { "requests_per_minute" => rpm }
    }

    /// A positive `Long` (`extractOptionalPositiveLong`).
    fn optional_positive_int_long(&mut self, key: &str) -> Option<i64> {
        let v = self.take(key)?;
        let Some(i) = v.as_i64() else {
            self.errors.push(type_error(key, &v, "Long"));
            return None;
        };
        if i <= 0 {
            self.errors.push(format!("[{}] Invalid value [{i}]. [{key}] must be a positive long", self.scope));
            return None;
        }
        Some(i)
    }

    /// A map of string values (`headers`): every value must be a string.
    pub fn optional_string_map(&mut self, key: &str) -> Option<Map> {
        let m = self.optional_map(key)?;
        if m.is_empty() {
            self.errors.push(format!("[{}] Invalid value empty map. [{key}] must be a non-empty map", self.scope));
            return None;
        }
        for (k, v) in &m {
            if !matches!(v, Value::Str(_)) {
                self.errors.push(format!(
                    "Map field [{key}] has an entry that is not valid, [{k} => {}]. Value type of [{}] is not one of [String].",
                    java_string(v),
                    java_type(v)
                ));
                return None;
            }
        }
        Some(m)
    }
}

/// The Java class name of a parsed JSON value.
fn java_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "Boolean",
        Value::Num(Number::Float(_)) => "Double",
        Value::Num(n) if n.as_i64().is_some_and(|i| i32::try_from(i).is_ok()) => "Integer",
        Value::Num(_) => "Long",
        Value::Str(_) => "String",
        Value::Arr(_) => "ArrayList",
        Value::Obj(_) => "LinkedHashMap",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obj;

    #[test]
    fn errors_accumulate_in_extraction_order() {
        let Value::Obj(m) = obj!("dimensions" => -1, "model_id" => "", "similarity" => "manhattan", "rest" => 1) else { unreachable!() };
        let mut r = Reader::new(m, "service_settings");
        assert_eq!(r.similarity(), None);
        assert_eq!(r.optional_positive_int("dimensions"), None);
        assert_eq!(r.required_str("model_id"), None);
        assert_eq!(r.required_str("url"), None);
        let e = r.finish().unwrap_err();
        assert_eq!(
            e.reason(),
            "Validation Failed: 1: [service_settings] Invalid value [manhattan] received. [similarity] must be one of [cosine, dot_product, l2_norm];\
             2: [service_settings] Invalid value [-1]. [dimensions] must be a positive integer;\
             3: [service_settings] Invalid value empty string. [model_id] must be a non-empty string;\
             4: [service_settings] does not contain the required setting [url];"
        );
    }

    #[test]
    fn leftovers_print_as_java_maps() {
        let Value::Obj(m) = obj!("extra" => 2) else { unreachable!() };
        assert_eq!(unknown(&m, "openai").reason(), "Configuration contains settings [{extra=2}] unknown to the [openai] service");
    }
}
