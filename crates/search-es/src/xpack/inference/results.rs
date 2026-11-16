//! What an inference call returns, and how `POST _inference` prints it (Elasticsearch's
//! `DenseEmbeddingFloatResults`, `DenseEmbeddingByteResults`, `DenseEmbeddingBitResults`,
//! `SparseEmbeddingResults`, `RankedDocsResults`, `ChatCompletionResults`).

use crate::json::{Map, Value};
use crate::obj;
use crate::query::xparser::float_value;

/// One sparse embedding: weighted tokens in the order the model gave them.
#[derive(Clone, Debug, PartialEq)]
pub struct Sparse {
    pub tokens: Vec<(String, f32)>,
    pub truncated: bool,
}

/// One reranked document.
#[derive(Clone, Debug, PartialEq)]
pub struct Ranked {
    pub index: usize,
    pub score: f32,
    pub text: Option<String>,
}

/// Results of one call, one entry per input (rerank: per ranked document).
#[derive(Clone, Debug, PartialEq)]
pub enum Results {
    Floats(Vec<Vec<f32>>),
    Bytes(Vec<Vec<i8>>),
    Bits(Vec<Vec<i8>>),
    Sparse(Vec<Sparse>),
    Rerank(Vec<Ranked>),
    Completion(Vec<String>),
}

impl Results {
    /// How many dimensions the first dense embedding has (bits count eight per byte), as
    /// `DenseEmbeddingResults.getFirstEmbeddingSize` reports it.
    pub fn first_embedding_size(&self) -> Option<usize> {
        match self {
            Results::Floats(v) => v.first().map(Vec::len),
            Results::Bytes(v) => v.first().map(Vec::len),
            Results::Bits(v) => v.first().map(|e| e.len() * 8),
            _ => None,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Results::Floats(v) => v.len(),
            Results::Bytes(v) | Results::Bits(v) => v.len(),
            Results::Sparse(v) => v.len(),
            Results::Rerank(v) => v.len(),
            Results::Completion(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The answer of `POST _inference`.
    pub fn to_value(&self) -> Value {
        let (key, items): (&str, Vec<Value>) = match self {
            Results::Floats(v) => {
                ("text_embedding", v.iter().map(|e| obj! { "embedding" => Value::Arr(e.iter().map(|f| float_value(*f)).collect()) }).collect())
            }
            Results::Bytes(v) => ("text_embedding_bytes", v.iter().map(|e| obj! { "embedding" => bytes(e) }).collect()),
            Results::Bits(v) => ("text_embedding_bits", v.iter().map(|e| obj! { "embedding" => bytes(e) }).collect()),
            Results::Sparse(v) => ("sparse_embedding", v.iter().map(|s| obj! { "is_truncated" => s.truncated, "embedding" => weights(&s.tokens) }).collect()),
            Results::Rerank(v) => (
                "rerank",
                v.iter()
                    .map(|r| {
                        let mut m = Map::new();
                        m.insert("index".into(), (r.index as i64).into());
                        m.insert("relevance_score".into(), float_value(r.score));
                        if let Some(t) = &r.text {
                            m.insert("text".into(), t.clone().into());
                        }
                        Value::Obj(m)
                    })
                    .collect(),
            ),
            Results::Completion(v) => ("completion", v.iter().map(|r| obj! { "result" => r.clone() }).collect()),
        };
        let mut m = Map::new();
        m.insert(key.into(), Value::Arr(items));
        Value::Obj(m)
    }
}

fn bytes(e: &[i8]) -> Value {
    Value::Arr(e.iter().map(|b| i64::from(*b).into()).collect())
}

/// Sparse weights as an object, token → weight.
pub fn weights(tokens: &[(String, f32)]) -> Value {
    Value::Obj(tokens.iter().map(|(t, w)| (t.clone(), float_value(*w))).collect())
}
