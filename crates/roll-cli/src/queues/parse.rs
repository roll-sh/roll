//! Argument parsing for `roll queues`: `k=v` pairs, message properties and bodies.

use anyhow::Context;
use base64::Engine;
use serde_json::{Map, Value};
use std::io::Read;

/// clap value parser for `k=v` (splits on the first `=`; the value may be empty or contain `=`).
pub fn kv(s: &str) -> Result<(String, String), String> {
    let (k, v) = s
        .split_once('=')
        .ok_or_else(|| format!("expected key=value, got {s:?}"))?;
    let k = k.trim();
    if k.is_empty() {
        return Err(format!("missing key in {s:?}"));
    }
    Ok((k.to_string(), v.to_string()))
}

/// A `k=v` value: JSON when it parses (`10`, `true`, `"10"`, `{"a":1}`), otherwise a string.
pub fn infer(v: &str) -> Value {
    serde_json::from_str(v).unwrap_or_else(|_| Value::String(v.to_string()))
}

/// `--arg k=v ...` → a JSON object with inferred values.
pub fn object(pairs: &[(String, String)]) -> Map<String, Value> {
    pairs.iter().map(|(k, v)| (k.clone(), infer(v))).collect()
}

/// AMQP 0-9-1 basic properties accepted by `--property` (`headers` come from `--header`).
pub const PROPERTIES: &[&str] = &[
    "content_type",
    "content_encoding",
    "delivery_mode",
    "priority",
    "correlation_id",
    "reply_to",
    "expiration",
    "message_id",
    "timestamp",
    "type",
    "user_id",
    "app_id",
    "cluster_id",
];

/// `--property k=v` and `--header k=v` → the `properties` object for publish.
/// Keys may use dashes (`content-type`); numeric properties are validated here.
pub fn properties(
    props: &[(String, String)],
    headers: &[(String, String)],
) -> Result<Map<String, Value>, String> {
    let mut out = Map::new();
    for (k, v) in props {
        let key = k.to_ascii_lowercase().replace('-', "_");
        let value = match key.as_str() {
            "delivery_mode" => Value::from(match v.to_ascii_lowercase().as_str() {
                "2" | "persistent" => 2,
                "1" | "transient" | "non_persistent" | "non-persistent" => 1,
                _ => {
                    return Err(format!(
                        "delivery_mode must be 1, 2, transient or persistent, got {v:?}"
                    ));
                }
            }),
            "priority" => Value::from(
                v.parse::<u8>()
                    .map_err(|_| format!("priority must be 0-255, got {v:?}"))?,
            ),
            "timestamp" => Value::from(
                v.parse::<i64>()
                    .map_err(|_| format!("timestamp must be unix seconds, got {v:?}"))?,
            ),
            "headers" => return Err("use --header k=v for headers".into()),
            k if PROPERTIES.contains(&k) => Value::String(v.clone()),
            _ => {
                return Err(format!(
                    "unknown property {k:?}; one of: {}",
                    PROPERTIES.join(", ")
                ));
            }
        };
        out.insert(key, value);
    }
    if !headers.is_empty() {
        out.insert("headers".into(), Value::Object(object(headers)));
    }
    Ok(out)
}

/// A message body argument: literal text, `@path` for a file, or `-` for stdin.
pub fn read_body(arg: &str, stdin: impl Read) -> anyhow::Result<Vec<u8>> {
    let mut stdin = stdin;
    if arg == "-" {
        let mut buf = Vec::new();
        stdin
            .read_to_end(&mut buf)
            .context("reading the body from stdin")?;
        Ok(buf)
    } else if let Some(path) = arg.strip_prefix('@') {
        std::fs::read(path).with_context(|| format!("reading the body from {path}"))
    } else {
        Ok(arg.as_bytes().to_vec())
    }
}

/// Body bytes → (body, body_encoding) for the publish API: UTF-8 as is, anything else base64.
pub fn encode_body(bytes: Vec<u8>) -> (String, &'static str) {
    match String::from_utf8(bytes) {
        Ok(s) => (s, "utf8"),
        Err(e) => (
            base64::engine::general_purpose::STANDARD.encode(e.into_bytes()),
            "base64",
        ),
    }
}

/// A JSON object argument such as `--definition '{"max-length":1000}'`.
pub fn json_object(s: &str) -> Result<Map<String, Value>, String> {
    match serde_json::from_str::<Value>(s) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err("expected a JSON object, e.g. '{\"max-length\": 1000}'".into()),
        Err(e) => Err(format!("invalid JSON: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pairs(xs: &[(&str, &str)]) -> Vec<(String, String)> {
        xs.iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn kv_pairs() {
        assert_eq!(kv("a=b"), Ok(("a".into(), "b".into())));
        assert_eq!(kv("x-match=all=1"), Ok(("x-match".into(), "all=1".into())));
        assert_eq!(kv("empty="), Ok(("empty".into(), "".into())));
        assert_eq!(kv(" k =v"), Ok(("k".into(), "v".into())));
        assert!(kv("novalue").is_err());
        assert!(kv("=v").is_err());
    }

    #[test]
    fn inferred_values() {
        assert_eq!(infer("1000"), json!(1000));
        assert_eq!(infer("true"), json!(true));
        assert_eq!(infer("\"1000\""), json!("1000"));
        assert_eq!(infer("lazy"), json!("lazy"));
        assert_eq!(infer(""), json!(""));
        assert_eq!(infer("[1,2]"), json!([1, 2]));
        let o = object(&pairs(&[("x-max-length", "10"), ("x-queue-mode", "lazy")]));
        assert_eq!(
            Value::Object(o),
            json!({"x-max-length": 10, "x-queue-mode": "lazy"})
        );
    }

    #[test]
    fn message_properties() {
        let p = properties(
            &pairs(&[
                ("content-type", "application/json"),
                ("delivery_mode", "persistent"),
                ("priority", "5"),
                ("expiration", "60000"),
            ]),
            &pairs(&[("trace", "abc"), ("retries", "3")]),
        )
        .unwrap();
        assert_eq!(
            Value::Object(p),
            json!({
                "content_type": "application/json",
                "delivery_mode": 2,
                "priority": 5,
                "expiration": "60000",
                "headers": {"trace": "abc", "retries": 3}
            })
        );
        assert!(properties(&pairs(&[("priority", "300")]), &[]).is_err());
        assert!(properties(&pairs(&[("delivery_mode", "3")]), &[]).is_err());
        assert!(properties(&pairs(&[("colour", "blue")]), &[]).is_err());
        assert!(properties(&pairs(&[("headers", "x")]), &[]).is_err());
        assert!(properties(&[], &[]).unwrap().is_empty());
    }

    #[test]
    fn bodies() {
        assert_eq!(read_body("hello", std::io::empty()).unwrap(), b"hello");
        assert_eq!(read_body("-", &b"from stdin"[..]).unwrap(), b"from stdin");

        let path = std::env::temp_dir().join(format!("roll-cli-body-{}.bin", std::process::id()));
        std::fs::write(&path, [0xff, 0x00, 0x01]).unwrap();
        let arg = format!("@{}", path.display());
        let bytes = read_body(&arg, std::io::empty()).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(bytes, [0xff, 0x00, 0x01]);
        assert_eq!(encode_body(bytes), ("/wAB".to_string(), "base64"));
        assert_eq!(encode_body(b"hi".to_vec()), ("hi".to_string(), "utf8"));

        assert!(read_body("@/definitely/not/here", std::io::empty()).is_err());
    }

    #[test]
    fn json_objects() {
        assert_eq!(
            Value::Object(json_object(r#"{"max-length": 5}"#).unwrap()),
            json!({"max-length": 5})
        );
        assert!(json_object("[1]").is_err());
        assert!(json_object("{nope").is_err());
    }
}
