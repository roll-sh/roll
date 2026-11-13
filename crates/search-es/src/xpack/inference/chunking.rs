//! Chunking settings (`chunking_settings` of an endpoint or a `semantic_text` field):
//! Elasticsearch's `ChunkingSettingsBuilder` and the four strategies' `fromMap` / `toXContent`.

use super::settings::{Reader, java_string, validation};
use crate::error::{EsError, EsResult};
use crate::json::write::java_double;
use crate::json::{Map, Value};
use crate::obj;

/// How text is cut into chunks before it is embedded.
#[derive(Clone, Debug, PartialEq)]
pub enum Chunking {
    /// One chunk: the whole text.
    None,
    /// Up to `max_chunk_size` words, consecutive chunks sharing `overlap` words.
    Word { max_chunk_size: i64, overlap: i64 },
    /// Whole sentences up to `max_chunk_size` words, overlapping by 0 or 1 sentence.
    Sentence { max_chunk_size: i64, sentence_overlap: i64 },
    /// Split on the first separator (a regex) that brings pieces under `max_chunk_size` words.
    Recursive { max_chunk_size: i64, separators: Vec<String> },
}

/// Sentences of up to 250 words overlapping by one (`ChunkingSettingsBuilder.DEFAULT_SETTINGS`).
pub const DEFAULT: Chunking = Chunking::Sentence { max_chunk_size: 250, sentence_overlap: 1 };

/// Words, 250 per chunk with 100 shared: what endpoints created before chunking settings existed
/// use (`OLD_DEFAULT_SETTINGS`).
pub const OLD_DEFAULT: Chunking = Chunking::Word { max_chunk_size: 250, overlap: 100 };

const PLAINTEXT: &[&str] = &["(?<!\\n)\\n\\n(?!\\n)", "(?<!\\n)\\n(?!\\n)"];
const MARKDOWN: &[&str] =
    &["\n# ", "\n## ", "\n### ", "\n#### ", "\n##### ", "\n###### ", "\n^(?!\\s*$).*\\n-{1,}\\n", "\n^(?!\\s*$).*\\n={1,}\\n"];

/// Most separators a recursive strategy may list in a request.
const MAX_SEPARATORS: usize = 64;

/// `Arrays.toString` of the keys not in `valid`, in Java `HashMap` order; `None` when all are.
fn invalid_keys(m: &Map, valid: &[&str]) -> Option<String> {
    let bad: Vec<String> = crate::json::java_hash_map(m.clone(), 16).keys().filter(|k| !valid.contains(&k.as_str())).cloned().collect();
    (!bad.is_empty()).then(|| format!("[{}]", bad.join(", ")))
}

fn at_least(r: &mut Reader, key: &str, min: i64) -> Option<i64> {
    let v = r.required_positive_int(key)?;
    if v < min {
        r.error(format!("[chunking_settings] Invalid value [{}]. [{key}] must be greater than or equal to [{}]", java_double(v as f64), java_double(min as f64)));
        return None;
    }
    Some(v)
}

impl Chunking {
    /// Reads a request's `chunking_settings` (`fromMap(settings, true, true)`): `None` (the key
    /// absent) is the old default, `{}` the default.
    pub fn from_request(v: Option<&Value>) -> EsResult<Chunking> {
        match v {
            None => Ok(OLD_DEFAULT),
            Some(Value::Obj(m)) if m.is_empty() => Ok(DEFAULT),
            Some(Value::Obj(m)) => Chunking::parse(m, true),
            Some(other) => Err(EsError::java(
                400,
                "illegal_argument_exception",
                format!("field [chunking_settings] is not of the expected type. The value [{}] cannot be converted to a [Map]", java_string(other)),
            )),
        }
    }

    /// Reads stored settings (no request limits).
    pub fn from_stored(v: &Value) -> Option<Chunking> {
        match v {
            Value::Obj(m) if !m.is_empty() => Chunking::parse(m, false).ok(),
            _ => None,
        }
    }

    fn parse(m: &Map, enforce_limits: bool) -> EsResult<Chunking> {
        let Some(strategy) = m.get("strategy") else {
            return Err(EsError::java(400, "illegal_argument_exception", "Can't generate Chunker without ChunkingStrategy provided"));
        };
        let strategy = strategy.to_es_string();
        let mut r = Reader::new(m.clone(), "chunking_settings");
        r.map.shift_remove("strategy");
        match strategy.as_str() {
            "none" => match invalid_keys(m, &["strategy"]) {
                Some(bad) => Err(validation(&[format!("When chunking is disabled (none), settings can not have the following: {bad}")])),
                None => Ok(Chunking::None),
            },
            "word" => {
                if let Some(bad) = invalid_keys(m, &["strategy", "max_chunk_size", "overlap"]) {
                    r.error(format!("Word based chunking settings can not have the following settings: {bad}"));
                }
                let max = at_least(&mut r, "max_chunk_size", 10);
                let overlap = max.and_then(|max| {
                    let o = r.required_positive_int("overlap")?;
                    if o > max / 2 {
                        r.error(format!(
                            "[chunking_settings] Invalid value [{}]. [overlap] must be less than or equal to [{}]",
                            java_double(o as f64),
                            java_double((max / 2) as f64)
                        ));
                    }
                    Some(o)
                });
                r.finish()?;
                Ok(Chunking::Word { max_chunk_size: max.unwrap_or(0), overlap: overlap.unwrap_or(0) })
            }
            "sentence" => {
                if let Some(bad) = invalid_keys(m, &["strategy", "max_chunk_size", "sentence_overlap"]) {
                    r.error(format!("Sentence based chunking settings can not have the following settings: {bad}"));
                }
                let max = at_least(&mut r, "max_chunk_size", 20);
                let overlap = match r.optional_int("sentence_overlap") {
                    None => 1,
                    Some(o) if !(0..=1).contains(&o) => {
                        r.error(format!("sentence_overlap[{o}] must be either 0 or 1"));
                        1
                    }
                    Some(o) => o,
                };
                r.finish()?;
                Ok(Chunking::Sentence { max_chunk_size: max.unwrap_or(0), sentence_overlap: overlap })
            }
            "recursive" => {
                if let Some(bad) = invalid_keys(m, &["strategy", "max_chunk_size", "separators", "separator_group"]) {
                    r.error(format!("Recursive chunking settings can not have the following settings: {bad}"));
                }
                let max = at_least(&mut r, "max_chunk_size", 10);
                let group = r.optional_enum("separator_group", &["plaintext", "markdown"]);
                let listed: Option<Vec<String>> = match r.map.shift_remove("separators") {
                    None => None,
                    Some(Value::Arr(a)) if a.iter().all(|x| matches!(x, Value::Str(_))) => Some(a.iter().map(Value::to_es_string).collect()),
                    Some(other) => {
                        r.error(format!("field [separators] is not of the expected type. The value [{}] cannot be converted to a [List]", java_string(&other)));
                        None
                    }
                };
                if listed.is_some() && group.is_some() {
                    r.error("Recursive chunking settings can not have both separators and separator_set");
                }
                let separators = match (group.as_deref(), listed) {
                    (Some("markdown"), _) => MARKDOWN.iter().map(|s| s.to_string()).collect(),
                    (Some(_), _) | (None, None) => PLAINTEXT.iter().map(|s| s.to_string()).collect(),
                    (None, Some(s)) => {
                        if s.is_empty() {
                            r.error("Recursive chunking settings can not have an empty list of separators");
                        }
                        if enforce_limits && s.len() > MAX_SEPARATORS {
                            r.error(format!("separators list size [{}] must not exceed [{MAX_SEPARATORS}]", s.len()));
                        }
                        s
                    }
                };
                r.finish()?;
                Ok(Chunking::Recursive { max_chunk_size: max.unwrap_or(0), separators })
            }
            other => Err(EsError::java(400, "illegal_argument_exception", format!("Invalid chunkingStrategy {other}"))),
        }
    }

    pub fn to_value(&self) -> Value {
        match self {
            Chunking::None => obj! { "strategy" => "none" },
            Chunking::Word { max_chunk_size, overlap } => obj! { "strategy" => "word", "max_chunk_size" => *max_chunk_size, "overlap" => *overlap },
            Chunking::Sentence { max_chunk_size, sentence_overlap } => {
                obj! { "strategy" => "sentence", "max_chunk_size" => *max_chunk_size, "sentence_overlap" => *sentence_overlap }
            }
            Chunking::Recursive { max_chunk_size, separators } => obj! {
                "strategy" => "recursive",
                "max_chunk_size" => *max_chunk_size,
                "separators" => Value::Arr(separators.iter().map(|s| Value::from(s.as_str())).collect()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: Value) -> EsResult<Chunking> {
        Chunking::from_request(Some(&v))
    }

    #[test]
    fn defaults_and_strategies() {
        assert_eq!(Chunking::from_request(None).unwrap(), OLD_DEFAULT);
        assert_eq!(parse(obj! {}).unwrap(), DEFAULT);
        assert_eq!(parse(obj! { "strategy" => "word", "max_chunk_size" => 20, "overlap" => 2 }).unwrap(), Chunking::Word { max_chunk_size: 20, overlap: 2 });
        assert_eq!(parse(obj! { "strategy" => "none" }).unwrap().to_value(), obj! { "strategy" => "none" });
        let e = parse(obj! { "strategy" => "sentence", "max_chunk_size" => 5 }).unwrap_err();
        assert_eq!(e.reason(), "Validation Failed: 1: [chunking_settings] Invalid value [5.0]. [max_chunk_size] must be greater than or equal to [20.0];");
        let e = parse(obj! { "strategy" => "word", "max_chunk_size" => 20, "overlap" => 11 }).unwrap_err();
        assert_eq!(e.reason(), "Validation Failed: 1: [chunking_settings] Invalid value [11.0]. [overlap] must be less than or equal to [10.0];");
        assert_eq!(parse(obj! { "strategy" => "bogus" }).unwrap_err().reason(), "Invalid chunkingStrategy bogus");
        assert_eq!(parse(obj! { "max_chunk_size" => 5 }).unwrap_err().reason(), "Can't generate Chunker without ChunkingStrategy provided");
    }
}
