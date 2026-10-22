//! Assertion semantics (`match`, `length`, `is_true`, `gt`, `contains`, `close_to`, ...).
//!
//! These follow Elasticsearch's `section/*Assertion.java` classes, including the details that
//! decide whether a borderline test passes:
//!
//! - `match` treats a string expectation wrapped in slashes as a verbose (`COMMENTS`) regex that
//!   must be *found* in a string value; two numbers of different kinds compare by double value at
//!   the top level, but nested values compare with Java `equals` (an integer never equals a float);
//! - `is_true`/`is_false` look at `toString()` (`""`, `"0"` and `"false"` are false, `0.0` is true);
//! - `length` counts UTF-16 units for strings, like `String.length()`;
//! - `contains` finds a map in a list by entry subset, or a substring in a string.
//!
//! Ordering comparisons (`gt`, `lt`, ...) compare like `Comparable.compareTo`: an integer never
//! compares with a floating point number (Java throws a cast error), so `gte: { took: 0 }` fails
//! against `0.5` exactly as it does in Elasticsearch's runner. That catches a target returning
//! `5.0` where Elasticsearch returns `5`.

use std::cmp::Ordering;

use crate::regex_java::RegexCache;
use crate::value::Value;

/// A failed assertion with the values involved, for reports.
#[derive(Debug, Clone)]
pub struct Mismatch {
    pub message: String,
    pub expected: Option<Box<Value>>,
    pub actual: Option<Box<Value>>,
}

impl Mismatch {
    fn new(message: impl Into<String>, expected: Option<&Value>, actual: Option<&Value>) -> Mismatch {
        Mismatch {
            message: message.into(),
            expected: expected.map(|v| Box::new(v.clone())),
            actual: actual.map(|v| Box::new(v.clone())),
        }
    }
}

type Result = std::result::Result<(), Mismatch>;

fn class_of(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "null".into(),
        Some(v) => format!("class java.lang.{}", v.java_class()),
    }
}

fn show(v: Option<&Value>) -> String {
    v.map(Value::java_string).unwrap_or_else(|| "null".into())
}

/// Whether `expected` is a `/regex/` (after trimming).
pub fn regex_of(expected: &str) -> Option<&str> {
    let t = expected.trim();
    (t.len() > 2 && t.starts_with('/') && t.ends_with('/')).then(|| &t[1..t.len() - 1])
}

/// `MatchAssertion.assertMatches`.
pub fn assert_match(field: &str, actual: Option<&Value>, expected: &Value, rx: &mut RegexCache) -> Result {
    if let Value::Str(s) = expected
        && let Some(pattern) = regex_of(s)
    {
        let Some(Value::Str(text)) = actual else {
            return Err(Mismatch::new(
                format!("field [{field}] was expected to be of type String but is an instanceof [{}]", class_of(actual)),
                Some(expected),
                actual,
            ));
        };
        return match rx.find(pattern, true, text) {
            Ok(true) => Ok(()),
            Ok(false) => Err(Mismatch::new(
                format!("field [{field}] was expected to match the provided regex but didn't"),
                Some(expected),
                actual,
            )),
            Err(e) => Err(Mismatch::new(e, Some(expected), actual)),
        };
    }
    if expected.is_null() {
        return match actual {
            None | Some(Value::Null) => Ok(()),
            Some(a) => Err(Mismatch::new(
                format!("field [{field}] should be null but was [{}]", a.java_string()),
                Some(expected),
                actual,
            )),
        };
    }
    let Some(actual_v) = actual.filter(|v| !v.is_null()) else {
        return Err(Mismatch::new(format!("field [{field}] is null"), Some(expected), None));
    };
    match (actual_v, expected) {
        (Value::Int(_), Value::Float(_)) | (Value::Float(_), Value::Int(_)) => {
            let (a, e) = (actual_v.as_f64().unwrap_or(f64::NAN), expected.as_f64().unwrap_or(f64::NAN));
            // Hamcrest's equalTo on boxed doubles is Double.equals (bitwise).
            if a.to_bits() == e.to_bits() {
                return Ok(());
            }
            return Err(Mismatch::new(
                format!("field [{field}] doesn't match the expected value"),
                Some(expected),
                actual,
            ));
        }
        (Value::Map(_), Value::Map(_)) | (Value::List(_), Value::List(_)) => {
            let mut diffs = Vec::new();
            diff(String::new(), actual_v, expected, &mut diffs);
            if diffs.is_empty() {
                return Ok(());
            }
            let mut message = format!("field [{field}] does not match the expected value:");
            for d in diffs.iter().take(20) {
                message.push_str("\n  ");
                message.push_str(d);
            }
            if diffs.len() > 20 {
                message.push_str(&format!("\n  ... and {} more differences", diffs.len() - 20));
            }
            return Err(Mismatch::new(message, Some(expected), actual));
        }
        (_, Value::Map(_)) => {
            return Err(Mismatch::new(
                format!("field [{field}]: expected an instance of java.util.Map but <{}> is a {}", actual_v.java_string(), class_of(actual)),
                Some(expected),
                actual,
            ));
        }
        (_, Value::List(_)) => {
            return Err(Mismatch::new(
                format!("field [{field}]: expected an instance of java.util.List but <{}> is a {}", actual_v.java_string(), class_of(actual)),
                Some(expected),
                actual,
            ));
        }
        _ => {}
    }
    if actual_v == expected {
        Ok(())
    } else {
        Err(Mismatch::new(format!("field [{field}] does not match the expected value"), Some(expected), actual))
    }
}

/// Collects the paths where two trees differ, like `MapMatcher`'s description.
fn diff(path: String, actual: &Value, expected: &Value, out: &mut Vec<String>) {
    let at = |p: &str| if p.is_empty() { "<root>".to_owned() } else { p.to_owned() };
    match (actual, expected) {
        (Value::Map(a), Value::Map(e)) => {
            for (k, ev) in e {
                let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                match a.get(k) {
                    Some(av) => diff(p, av, ev, out),
                    None => out.push(format!("{}: expected {} but was missing", at(&p), ev.to_json())),
                }
            }
            for (k, av) in a {
                if !e.contains_key(k) {
                    let p = if path.is_empty() { k.clone() } else { format!("{path}.{k}") };
                    out.push(format!("{}: unexpected {}", at(&p), av.to_json()));
                }
            }
        }
        (Value::List(a), Value::List(e)) => {
            for (i, ev) in e.iter().enumerate() {
                let p = if path.is_empty() { i.to_string() } else { format!("{path}.{i}") };
                match a.get(i) {
                    Some(av) => diff(p, av, ev, out),
                    None => out.push(format!("{}: expected {} but the list ended", at(&p), ev.to_json())),
                }
            }
            for (i, av) in a.iter().enumerate().skip(e.len()) {
                let p = if path.is_empty() { i.to_string() } else { format!("{path}.{i}") };
                out.push(format!("{}: unexpected {}", at(&p), av.to_json()));
            }
        }
        _ => {
            if actual != expected {
                out.push(format!("{}: expected {} but was {}", at(&path), expected.to_json(), actual.to_json()));
            }
        }
    }
}

/// `IsTrueAssertion`.
pub fn assert_true(field: &str, actual: Option<&Value>) -> Result {
    let msg = || Mismatch::new(format!("field [{field}] doesn't have a true value"), None, actual);
    let Some(v) = actual.filter(|v| !v.is_null()) else { return Err(msg()) };
    let s = v.java_string();
    if s.is_empty() || s.eq_ignore_ascii_case("false") || s == "0" {
        return Err(msg());
    }
    Ok(())
}

/// `IsFalseAssertion`.
pub fn assert_false(field: &str, actual: Option<&Value>) -> Result {
    let Some(v) = actual.filter(|v| !v.is_null()) else { return Ok(()) };
    let s = v.java_string();
    if s.is_empty() || s.eq_ignore_ascii_case("false") || s == "0" {
        Ok(())
    } else {
        Err(Mismatch::new(format!("field [{field}] has a true value but it shouldn't"), None, actual))
    }
}

/// `LengthAssertion`.
pub fn assert_length(field: &str, actual: Option<&Value>, expected: &Value) -> Result {
    let want = match expected {
        Value::Int(i) => *i as i64,
        Value::Float(f) => *f as i64,
        _ => {
            return Err(Mismatch::new(
                format!("expected value of [{field}] is not numeric (got [{}]", class_of(Some(expected))),
                Some(expected),
                actual,
            ));
        }
    };
    let got = match actual {
        Some(Value::Str(s)) => s.encode_utf16().count(),
        Some(Value::List(l)) => l.len(),
        Some(Value::Map(m)) => m.len(),
        _ => {
            return Err(Mismatch::new(
                format!("value is of unsupported type [{}]", class_of(actual)),
                Some(expected),
                actual,
            ));
        }
    };
    if got as i64 == want {
        Ok(())
    } else {
        Err(Mismatch::new(
            format!("field [{field}] doesn't have length [{}] (actual length {got})", expected.java_string()),
            Some(expected),
            actual,
        ))
    }
}

/// The ordering operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Gt,
    Gte,
    Lt,
    Lte,
}

fn compare(a: &Value, b: &Value, mixed_numbers: bool) -> Option<Ordering> {
    match (a, b) {
        (Value::Int(x), Value::Int(y)) => Some(x.cmp(y)),
        (Value::Str(x), Value::Str(y)) => {
            // String.compareTo compares UTF-16 code units.
            Some(x.encode_utf16().cmp(y.encode_utf16()))
        }
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        // Double.compareTo: a total order (NaN above +Infinity, -0.0 below 0.0).
        (Value::Float(x), Value::Float(y)) => Some(x.total_cmp(y)),
        _ if mixed_numbers && a.is_number() && b.is_number() => a.as_f64()?.partial_cmp(&b.as_f64()?),
        _ => None,
    }
}

/// `GreaterThanAssertion` and friends. `mixed_numbers` lets an integer compare with a float (the
/// official clients' runners, in Python or JavaScript, don't tell them apart).
pub fn assert_cmp(op: Cmp, field: &str, actual: Option<&Value>, expected: &Value, mixed_numbers: bool) -> Result {
    let comparable = |v: &Value| matches!(v, Value::Int(_) | Value::Float(_) | Value::Str(_) | Value::Bool(_));
    let Some(a) = actual.filter(|v| comparable(v)) else {
        return Err(Mismatch::new(
            format!("value of [{field}] is not comparable (got [{}])", class_of(actual)),
            Some(expected),
            actual,
        ));
    };
    if !comparable(expected) {
        return Err(Mismatch::new(
            format!("expected value of [{field}] is not comparable (got [{}])", class_of(Some(expected))),
            Some(expected),
            actual,
        ));
    }
    let Some(ord) = compare(a, expected, mixed_numbers) else {
        return Err(Mismatch::new(
            format!(
                "cast error while checking ({field}): cannot compare [{}] with [{}]",
                class_of(actual),
                class_of(Some(expected))
            ),
            Some(expected),
            actual,
        ));
    };
    let (ok, what) = match op {
        Cmp::Gt => (ord == Ordering::Greater, "greater than"),
        Cmp::Gte => (ord != Ordering::Less, "greater than or equal to"),
        Cmp::Lt => (ord == Ordering::Less, "less than"),
        Cmp::Lte => (ord != Ordering::Greater, "less than or equal to"),
    };
    if ok {
        Ok(())
    } else {
        Err(Mismatch::new(
            format!("field [{field}] is not {what} [{}] (was [{}])", expected.java_string(), a.java_string()),
            Some(expected),
            actual,
        ))
    }
}

/// `ContainsAssertion`.
pub fn assert_contains(field: &str, actual: Option<&Value>, expected: &Value) -> Result {
    match (actual, expected) {
        (Some(Value::List(list)), Value::Map(want)) => {
            let candidates: Vec<&Value> = list
                .iter()
                .filter(|item| item.as_map().is_some_and(|m| want.keys().all(|k| m.contains_key(k))))
                .collect();
            if candidates.is_empty() {
                let keys: Vec<&str> = want.keys().map(String::as_str).collect();
                return Err(Mismatch::new(
                    format!("{field} expected to be a list with at least one object that has keys: [{}] but it was {}", keys.join(", "), show(actual)),
                    Some(expected),
                    actual,
                ));
            }
            let hit = candidates
                .iter()
                .any(|item| item.as_map().is_some_and(|m| want.iter().all(|(k, v)| m.get(k).is_some_and(|a| a == v))));
            if hit {
                Ok(())
            } else {
                Err(Mismatch::new(
                    format!("{field} expected to be a list with at least on object that matches {} but was {}", expected.java_string(), show(actual)),
                    Some(expected),
                    actual,
                ))
            }
        }
        (Some(Value::Str(s)), Value::Str(want)) => {
            if s.contains(want.as_str()) {
                Ok(())
            } else {
                Err(Mismatch::new(
                    format!("field [{field}]: expected a string containing \"{want}\" but was \"{s}\""),
                    Some(expected),
                    actual,
                ))
            }
        }
        _ => Err(Mismatch::new(
            "'contains' only supports checking an object against a list of objects or a string against a string",
            Some(expected),
            actual,
        )),
    }
}

/// `CloseToAssertion`.
pub fn assert_close_to(field: &str, actual: Option<&Value>, expected: &Value, error: f64) -> Result {
    let Some(want) = expected.as_f64() else {
        return Err(Mismatch::new(
            format!("Expected value should be a number, but was [{}], which is not a number", expected.java_string()),
            Some(expected),
            actual,
        ));
    };
    let Some(got) = actual.and_then(Value::as_f64) else {
        return Err(Mismatch::new(
            format!("expected a value close to {} but got {}, which is not a number", expected.java_string(), show(actual)),
            Some(expected),
            actual,
        ));
    };
    if (got - want).abs() - error <= 0.0 {
        Ok(())
    } else {
        Err(Mismatch::new(
            format!(
                "field [{field}]: expected a numeric value within <{}> of <{}> but <{}> differed by <{}>",
                crate::value::java_double(error),
                crate::value::java_double(want),
                crate::value::java_double(got),
                crate::value::java_double((got - want).abs() - error)
            ),
            Some(expected),
            actual,
        ))
    }
}

/// `MatchesInAnyOrderAssertion`.
pub fn assert_any_order(field: &str, actual: Option<&Value>, expected: &Value, rx: &mut RegexCache) -> Result {
    let (a_list, e_list) = match (actual, expected) {
        (Some(Value::List(a)), Value::List(e)) => (a, e),
        (Some(Value::List(_)), _) => {
            return Err(Mismatch::new(
                format!("field [{field}] expected value must be a list but was [{}] of type [{}]", expected.java_string(), class_of(Some(expected))),
                Some(expected),
                actual,
            ));
        }
        (_, Value::List(_)) => {
            return Err(Mismatch::new(
                format!("field [{field}] expected to be a list but was [{}] of type [{}]", show(actual), class_of(actual)),
                Some(expected),
                actual,
            ));
        }
        _ => return assert_match(field, actual, expected, rx),
    };
    let mut remaining: Vec<&Value> = a_list.iter().collect();
    for e in e_list {
        let pos = remaining.iter().position(|a| assert_match(field, Some(a), e, rx).is_ok());
        match pos {
            Some(i) => {
                remaining.remove(i);
            }
            None => {
                return Err(Mismatch::new(
                    format!(
                        "field [{field}] expected to contain the same values in any order but no value matched [{}] in [{}]",
                        e.java_string(),
                        show(actual)
                    ),
                    Some(expected),
                    actual,
                ));
            }
        }
    }
    if remaining.is_empty() {
        Ok(())
    } else {
        let rest: Vec<String> = remaining.iter().map(|v| v.java_string()).collect();
        Err(Mismatch::new(
            format!(
                "field [{field}] expected to contain the same values in any order but contained unexpected values [{}] in {}",
                rest.join(", "),
                show(actual)
            ),
            Some(expected),
            actual,
        ))
    }
}

/// `Instant.parse`: an ISO-8601 instant with `Z` or an offset.
fn parse_instant(s: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(s).ok()
}

/// `IsAfterAssertion`.
pub fn assert_is_after(field: &str, actual: Option<&Value>, expected: &Value) -> Result {
    let Some(a) = actual.filter(|v| !v.is_null()) else {
        return Err(Mismatch::new(format!("field [{field}] is null"), Some(expected), actual));
    };
    let Some(at) = parse_instant(&a.java_string()) else {
        return Err(Mismatch::new(
            format!("field [{field}] cannot be parsed to Instant, got [{}]", a.java_string()),
            Some(expected),
            actual,
        ));
    };
    let Some(et) = parse_instant(&expected.java_string()) else {
        return Err(Mismatch::new(
            format!("value to test against [{}] cannot be parsed to Instant", expected.java_string()),
            Some(expected),
            actual,
        ));
    };
    if at > et {
        Ok(())
    } else {
        Err(Mismatch::new(
            format!("field [{field}] should be after [{}], but was not", a.java_string()),
            Some(expected),
            actual,
        ))
    }
}

/// `ExistsAssertion` / `NotExistsAssertion`.
pub fn assert_exists(field: &str, actual: Option<&Value>, should_exist: bool) -> Result {
    let exists = actual.is_some_and(|v| !v.is_null());
    match (exists, should_exist) {
        (true, true) | (false, false) => Ok(()),
        (false, true) => Err(Mismatch::new(format!("field [{field}] does not exist"), None, actual)),
        (true, false) => Err(Mismatch::new(format!("field [{field}] exists, but should not."), None, actual)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::json;

    fn m(actual: &str, expected: &str) -> bool {
        let mut rx = RegexCache::default();
        let a = json(actual);
        assert_match("f", Some(&a), &json(expected), &mut rx).is_ok()
    }

    #[test]
    fn match_semantics() {
        assert!(m("1", "1"));
        assert!(m("1.0", "1"), "top-level numbers compare by double");
        assert!(m("1", "1.0"));
        assert!(!m("1", "2"));
        assert!(!m(r#"{"a":1.0}"#, r#"{"a":1}"#), "nested numbers use Java equals");
        assert!(m(r#"{"a":1,"b":[1,"x"]}"#, r#"{"b":[1,"x"],"a":1}"#));
        assert!(!m(r#"{"a":1,"b":2}"#, r#"{"a":1}"#), "maps must match exactly");
        assert!(!m(r#""1""#, "1"));
        assert!(m("null", "null"));
        assert!(!m(r#"{"a":"/x/"}"#, r#"{"a":"/y/"}"#), "nested regexes are plain strings");
        assert!(m(r#""index_not_found_exception""#, r#""/not_found/""#));
        assert!(m(r#""  abc""#, r#""  /^\\s+ a b c $/  ""#), "regex is trimmed and verbose");
        assert!(!m("12", r#""/1/""#), "regex needs a string");
        let mut rx = RegexCache::default();
        let err = assert_match("f", None, &json("1"), &mut rx).unwrap_err();
        assert_eq!(err.message, "field [f] is null");
        assert!(assert_match("f", None, &Value::Null, &mut rx).is_ok());
    }

    #[test]
    fn diff_messages() {
        let mut rx = RegexCache::default();
        let a = json(r#"{"a":{"b":[1,2]},"x":1}"#);
        let err = assert_match("f", Some(&a), &json(r#"{"a":{"b":[1,3]},"y":2}"#), &mut rx).unwrap_err();
        assert!(err.message.contains("a.b.1: expected 3 but was 2"), "{}", err.message);
        assert!(err.message.contains("y: expected 2 but was missing"), "{}", err.message);
        assert!(err.message.contains("x: unexpected 1"), "{}", err.message);
    }

    #[test]
    fn truthiness() {
        for v in ["true", "1", "\"x\"", "0.0", "[]", "{}", "-1"] {
            assert!(assert_true("f", Some(&json(v))).is_ok(), "{v}");
            assert!(assert_false("f", Some(&json(v))).is_err(), "{v}");
        }
        for v in ["false", "0", "\"\"", "\"FALSE\"", "null"] {
            assert!(assert_true("f", Some(&json(v))).is_err(), "{v}");
            assert!(assert_false("f", Some(&json(v))).is_ok(), "{v}");
        }
        assert!(assert_false("f", None).is_ok());
        assert!(assert_true("f", None).is_err());
    }

    #[test]
    fn lengths() {
        assert!(assert_length("f", Some(&json(r#""héllo""#)), &json("5")).is_ok());
        assert!(assert_length("f", Some(&json(r#""😀""#)), &json("2")).is_ok(), "UTF-16 units");
        assert!(assert_length("f", Some(&json("[1,2,3]")), &json("3")).is_ok());
        assert!(assert_length("f", Some(&json(r#"{"a":1}"#)), &json("1")).is_ok());
        assert!(assert_length("f", Some(&json("[1]")), &json("2")).is_err());
        assert!(assert_length("f", Some(&json("5")), &json("1")).is_err());
        assert!(assert_length("f", None, &json("0")).is_err());
    }

    #[test]
    fn comparisons() {
        assert!(assert_cmp(Cmp::Gt, "f", Some(&json("5")), &json("4"), false).is_ok());
        assert!(assert_cmp(Cmp::Gt, "f", Some(&json("4")), &json("4"), false).is_err());
        assert!(assert_cmp(Cmp::Gte, "f", Some(&json("4")), &json("4"), false).is_ok());
        assert!(assert_cmp(Cmp::Lt, "f", Some(&json("0.5")), &json("1.0"), false).is_ok());
        assert!(assert_cmp(Cmp::Lte, "f", Some(&json("1.0")), &json("1.0"), false).is_ok());
        // Integer vs Double is a cast error in Java, whatever the values.
        let e = assert_cmp(Cmp::Lt, "f", Some(&json("0.5")), &json("1"), false).unwrap_err();
        assert!(e.message.starts_with("cast error while checking"), "{}", e.message);
        assert!(assert_cmp(Cmp::Gte, "f", Some(&json("1")), &json("1.0"), false).is_err());
        // Long vs Integer and big integers compare exactly.
        assert!(assert_cmp(Cmp::Gt, "f", Some(&json("9223372036854775807")), &json("1"), false).is_ok());
        assert!(assert_cmp(Cmp::Lt, "f", Some(&json("0.5")), &json("1"), true).is_ok());
        assert!(assert_cmp(Cmp::Gt, "f", Some(&json(r#""b""#)), &json(r#""a""#), false).is_ok());
        assert!(assert_cmp(Cmp::Gt, "f", None, &json("1"), false).is_err());
        assert!(assert_cmp(Cmp::Gt, "f", Some(&json(r#""1""#)), &json("1"), false).is_err());
    }

    #[test]
    fn contains_variants() {
        let list = json(r#"[{"name":"a","v":1},{"name":"b","v":2}]"#);
        assert!(assert_contains("f", Some(&list), &json(r#"{"name":"b"}"#)).is_ok());
        assert!(assert_contains("f", Some(&list), &json(r#"{"name":"b","v":1}"#)).is_err());
        assert!(assert_contains("f", Some(&list), &json(r#"{"other":1}"#)).is_err());
        assert!(assert_contains("f", Some(&json(r#""must be mapped""#)), &json(r#""be map""#)).is_ok());
        assert!(assert_contains("f", Some(&json("[1,2]")), &json("1")).is_err());
    }

    #[test]
    fn close_to_and_any_order_and_after() {
        assert!(assert_close_to("f", Some(&json("0.1234")), &json("0.123"), 0.001).is_ok());
        assert!(assert_close_to("f", Some(&json("0.13")), &json("0.123"), 0.001).is_err());
        assert!(assert_close_to("f", Some(&json(r#""x""#)), &json("1"), 0.1).is_err());
        let mut rx = RegexCache::default();
        let a = json(r#"[{"_id":"1"},{"_id":"2"},"b"]"#);
        assert!(assert_any_order("f", Some(&a), &json(r#"["b",{"_id":"2"},{"_id":"1"}]"#), &mut rx).is_ok());
        assert!(assert_any_order("f", Some(&a), &json(r#"["b",{"_id":"2"}]"#), &mut rx).is_err());
        assert!(assert_any_order("f", Some(&json(r#"["a","a"]"#)), &json(r#"["a","b"]"#), &mut rx).is_err());
        assert!(assert_any_order("f", Some(&json("1")), &json("1"), &mut rx).is_ok());
        let t = json(r#""2023-05-25T12:30:00.001Z""#);
        assert!(assert_is_after("f", Some(&t), &json(r#""2023-05-25T12:30:00.000Z""#)).is_ok());
        assert!(assert_is_after("f", Some(&t), &json(r#""2023-05-25T12:30:00.001Z""#)).is_err());
        assert!(assert_is_after("f", Some(&json(r#""nope""#)), &t).is_err());
    }

    #[test]
    fn existence() {
        assert!(assert_exists("f", Some(&json(r#""""#)), true).is_ok());
        assert!(assert_exists("f", None, true).is_err());
        assert!(assert_exists("f", None, false).is_ok());
        assert!(assert_exists("f", Some(&json("[]")), false).is_err());
    }
}
