//! Provider tokens of completions, counted per cluster for metering (`docs/search/rag.md`,
//! "Metering"). The providers bill them; roll only counts them, so an owner sees what completions
//! and chat answers use beside the limits that bound them.
//!
//! Counted: `_inference/completion` (answered or streamed), `_inference/chat_completion`'s
//! streams, and chat answers (`_roll/chat`, which count their own `usage`). Each call counts the
//! provider's figures when its answer carries them ([`Reported::see`]: OpenAI's, Anthropic's,
//! Cohere's, Bedrock's and Google's shapes), else roll's estimate of the text ([`estimate`]),
//! figure by figure. A stream counts when it ends, also when its client went away (the provider
//! generated until the connection closed): the count is in the stream's state, which the
//! node's stream lease outlives. Calls roll makes to validate an endpoint aren't counted.
//!
//! The node takes the counts on each metering pass (`search_server::Node::take_tokens`), those of
//! clusters unloaded in between too.

use crate::json::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// A cluster's tokens since the last metering pass.
#[derive(Debug, Default)]
pub struct Tokens {
    input: AtomicU64,
    output: AtomicU64,
}

impl Tokens {
    pub fn add(&self, input: u64, output: u64) {
        // Saturating: an endpoint (the custom service) may report any figure.
        let add = |a: &AtomicU64, n: u64| {
            let _ = a.try_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some(v.saturating_add(n)));
        };
        add(&self.input, input);
        add(&self.output, output);
    }

    /// `(input, output)` counted since the previous call.
    pub fn take(&self) -> (u64, u64) {
        (self.input.swap(0, Ordering::Relaxed), self.output.swap(0, Ordering::Relaxed))
    }
}

/// The figures a provider reported, each when it did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Reported {
    pub input: Option<u64>,
    pub output: Option<u64>,
}

impl Reported {
    /// The figures of one answer.
    pub fn of(v: &Value) -> Reported {
        let mut r = Reported::default();
        r.see(v);
        r
    }

    /// Takes in the figures an answer or a stream's event carries: `usage` (OpenAI and the
    /// services shaped like it, Anthropic, Bedrock's Converse, Cohere v2's `billed_units`),
    /// Anthropic's `message.usage` and Cohere's `delta.usage` in streams, Cohere v1's `meta`, and
    /// Google's `usageMetadata`. Streams repeat running totals, so each figure keeps its largest.
    pub fn see(&mut self, v: &Value) {
        let num = |u: &Value, keys: &[&str]| keys.iter().find_map(|k| u.get(k).and_then(Value::as_i64)).map(|n| n.max(0) as u64);
        for at in [&["usage"][..], &["message", "usage"], &["delta", "usage"], &["meta"], &["usageMetadata"]] {
            let Some(u) = v.pointer(at).filter(|u| u.as_object().is_some()) else { continue };
            let u = u.get("billed_units").filter(|b| b.as_object().is_some()).unwrap_or(u);
            let input = num(u, &["prompt_tokens", "input_tokens", "inputTokens", "promptTokenCount"]);
            // Google bills a model's thinking as output.
            let output = num(u, &["completion_tokens", "output_tokens", "outputTokens", "candidatesTokenCount"])
                .map(|o| o.saturating_add(num(u, &["thoughtsTokenCount"]).unwrap_or(0)));
            self.input = self.input.max(input);
            self.output = self.output.max(output);
        }
    }

    /// `(input, output)`: each figure the provider's, else the estimate given.
    pub fn or(self, input: u64, output: u64) -> (u64, u64) {
        (self.input.unwrap_or(input), self.output.unwrap_or(output))
    }
}

/// The estimated tokens of `s`, as roll has no tokenizer per model: an ASCII character is a
/// third of one, any other character one (which overestimates for common tokenizers).
pub fn estimate(s: &str) -> u64 {
    let (mut ascii, mut other) = (0u64, 0u64);
    for c in s.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    ascii.div_ceil(3) + other
}

/// The estimated tokens of a chat completion request's messages (`content` as a string or as
/// parts with `text`).
pub fn estimate_messages(body: &Value) -> u64 {
    let Some(messages) = body.get("messages").and_then(Value::as_array) else { return 0 };
    messages
        .iter()
        .map(|m| match m.get("content") {
            Some(Value::Str(s)) => estimate(s),
            Some(Value::Arr(parts)) => parts.iter().filter_map(|p| p.get("text").and_then(Value::as_str)).map(estimate).sum(),
            _ => 0,
        })
        .sum()
}

/// What a stream used, added to its cluster's [`Tokens`] when it's dropped (ended, failed, or
/// its client went away).
pub struct Count {
    tokens: Arc<Tokens>,
    reported: Reported,
    input: u64,
    output: u64,
}

impl Count {
    /// A stream whose request is estimated at `input` tokens.
    pub fn new(tokens: Arc<Tokens>, input: u64) -> Count {
        Count { tokens, reported: Reported::default(), input, output: 0 }
    }

    /// Takes in the provider's events as they came (`(event name, data)`).
    pub fn upstream(&mut self, events: &[(Option<String>, String)]) {
        for (_, data) in events {
            // Every shape [`Reported::see`] reads names its figures `…tokens`, `…Tokens` or
            // `…TokenCount`: other events aren't parsed twice.
            if data.contains("oken")
                && let Ok(Some(v)) = crate::json::parse(data.as_bytes())
            {
                self.reported.see(&v);
            }
        }
    }

    /// Takes in the events sent on: a completion's `delta`s, a chat completion's content and
    /// tool call arguments (the estimate of the output).
    pub fn sent(&mut self, values: &[Value]) {
        let text = |v: Option<&Value>| v.and_then(Value::as_str).map_or(0, estimate);
        for v in values {
            for c in v.get("completion").and_then(Value::as_array).into_iter().flatten() {
                self.output += text(c.get("delta"));
            }
            for c in v.get("choices").and_then(Value::as_array).into_iter().flatten() {
                let Some(d) = c.get("delta") else { continue };
                self.output += text(d.get("content"));
                for t in d.get("tool_calls").and_then(Value::as_array).into_iter().flatten() {
                    self.output += text(t.pointer(&["function", "arguments"]));
                }
            }
        }
    }
}

impl Drop for Count {
    fn drop(&mut self) {
        let (i, o) = self.reported.or(self.input, self.output);
        self.tokens.add(i, o);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::json::raw::parse;

    #[test]
    fn each_providers_figures_are_read_else_estimated() {
        let cases = [
            (r#"{"usage":{"prompt_tokens":12,"completion_tokens":30}}"#, (Some(12), Some(30))),
            (r#"{"usage":{"input_tokens":7,"output_tokens":9}}"#, (Some(7), Some(9))),
            (r#"{"message":{"usage":{"input_tokens":5,"output_tokens":1}}}"#, (Some(5), Some(1))),
            (r#"{"usage":{"inputTokens":3,"outputTokens":4}}"#, (Some(3), Some(4))),
            (r#"{"usage":{"billed_units":{"input_tokens":8,"output_tokens":2},"tokens":{"input_tokens":80,"output_tokens":2}}}"#, (Some(8), Some(2))),
            (r#"{"meta":{"billed_units":{"input_tokens":6,"output_tokens":3}}}"#, (Some(6), Some(3))),
            (r#"{"usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":5,"thoughtsTokenCount":6}}"#, (Some(4), Some(11))),
            (r#"{"usage":null,"choices":[]}"#, (None, None)),
        ];
        for (json, (i, o)) in cases {
            assert_eq!(Reported::of(&parse(json)), Reported { input: i, output: o }, "{json}");
        }
        assert_eq!(Reported { input: Some(2), output: None }.or(10, 20), (2, 20));
    }

    #[test]
    fn a_stream_counts_once_when_dropped_with_running_totals_kept_at_their_largest() {
        let tokens = Arc::new(Tokens::default());
        let mut c = Count::new(tokens.clone(), 100);
        c.upstream(&[(Some("message_start".into()), r#"{"message":{"usage":{"input_tokens":40,"output_tokens":1}}}"#.into())]);
        c.sent(&[parse(r#"{"completion":[{"delta":"abcdef"}]}"#)]);
        c.upstream(&[(Some("message_delta".into()), r#"{"usage":{"output_tokens":25}}"#.into())]);
        assert_eq!(tokens.take(), (0, 0));
        drop(c);
        assert_eq!(tokens.take(), (40, 25));
        // Nothing reported: the request's estimate and what was sent on.
        let mut c = Count::new(tokens.clone(), 9);
        c.sent(&[parse(r#"{"choices":[{"delta":{"content":"abcdef","tool_calls":[{"function":{"arguments":"{}"}}]}}]}"#)]);
        drop(c);
        assert_eq!(tokens.take(), (9, 3));
        assert_eq!(tokens.take(), (0, 0));
    }
}
