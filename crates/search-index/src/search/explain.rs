//! Score explanations, shaped like Lucene's `Explanation`.
//!
//! Elasticsearch's `_explain` and `explain: true` render these trees as JSON and its tests
//! compare the descriptions verbatim, so every query and similarity here builds the same tree
//! with the same texts as Lucene/Elasticsearch. Values keep their Java number type (`Float`,
//! `Double`, `Integer`, `Long`): Elasticsearch prints `"value": 3` for a doc count and
//! `"value": 3.0` for a float.

use crate::util::java::{double_to_string, float_to_string};

/// A Java `Number` as stored in an explanation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ExplValue {
    Float(f32),
    Double(f64),
    Int(i32),
    Long(i64),
}

impl ExplValue {
    /// `Number.floatValue()`.
    pub fn as_f32(self) -> f32 {
        match self {
            ExplValue::Float(v) => v,
            ExplValue::Double(v) => v as f32,
            ExplValue::Int(v) => v as f32,
            ExplValue::Long(v) => v as f32,
        }
    }

    /// `Number.doubleValue()`.
    pub fn as_f64(self) -> f64 {
        match self {
            ExplValue::Float(v) => v as f64,
            ExplValue::Double(v) => v,
            ExplValue::Int(v) => v as f64,
            ExplValue::Long(v) => v as f64,
        }
    }

    /// `String.valueOf(number)` (Java formatting).
    pub fn to_java_string(self) -> String {
        match self {
            ExplValue::Float(v) => float_to_string(v),
            ExplValue::Double(v) => double_to_string(v),
            ExplValue::Int(v) => v.to_string(),
            ExplValue::Long(v) => v.to_string(),
        }
    }

    /// Whether Elasticsearch renders it as an integer.
    pub fn is_integral(self) -> bool {
        matches!(self, ExplValue::Int(_) | ExplValue::Long(_))
    }
}

impl From<f32> for ExplValue {
    fn from(v: f32) -> Self {
        ExplValue::Float(v)
    }
}
impl From<f64> for ExplValue {
    fn from(v: f64) -> Self {
        ExplValue::Double(v)
    }
}
impl From<i32> for ExplValue {
    fn from(v: i32) -> Self {
        ExplValue::Int(v)
    }
}
impl From<i64> for ExplValue {
    fn from(v: i64) -> Self {
        ExplValue::Long(v)
    }
}
impl From<u32> for ExplValue {
    fn from(v: u32) -> Self {
        ExplValue::Int(v as i32)
    }
}
impl From<u64> for ExplValue {
    fn from(v: u64) -> Self {
        ExplValue::Long(v as i64)
    }
}

/// One node of an explanation tree.
#[derive(Clone, Debug, PartialEq)]
pub struct Explanation {
    pub matched: bool,
    pub value: ExplValue,
    pub description: String,
    pub details: Vec<Explanation>,
}

impl Explanation {
    /// `Explanation.match(value, description, details…)`.
    pub fn matched(value: impl Into<ExplValue>, description: impl Into<String>, details: Vec<Explanation>) -> Explanation {
        Explanation { matched: true, value: value.into(), description: description.into(), details }
    }

    /// A matched leaf.
    pub fn leaf(value: impl Into<ExplValue>, description: impl Into<String>) -> Explanation {
        Explanation::matched(value, description, Vec::new())
    }

    /// `Explanation.noMatch(description, details…)` (value 0).
    pub fn no_match(description: impl Into<String>, details: Vec<Explanation>) -> Explanation {
        Explanation { matched: false, value: ExplValue::Int(0), description: description.into(), details }
    }

    /// The value as a float (`getValue().floatValue()`).
    pub fn value(&self) -> f32 {
        self.value.as_f32()
    }

    pub fn is_match(&self) -> bool {
        self.matched
    }

    /// Lucene's `Explanation.toString()` (indented text), handy in tests and logs.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        self.text(&mut s, 0);
        s
    }

    fn text(&self, out: &mut String, depth: usize) {
        for _ in 0..depth {
            out.push_str("  ");
        }
        out.push_str(&self.value.to_java_string());
        out.push_str(" = ");
        out.push_str(&self.description);
        out.push('\n');
        for d in &self.details {
            d.text(out, depth + 1);
        }
    }
}
