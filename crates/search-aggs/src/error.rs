//! Aggregation errors with Elasticsearch's exception types and texts.
//!
//! Every failure a request can cause carries the Elasticsearch `type` (`parsing_exception`,
//! `x_content_parse_exception`, `illegal_argument_exception`, `aggregation_execution_exception`,
//! `too_many_buckets_exception`, `circuit_breaking_exception`, …), the exact `reason`, the HTTP
//! status and the extra fields Elasticsearch prints next to them (`line`/`col` of parsing
//! exceptions, `max_buckets`, `bytes_wanted`), plus the `caused_by` chain. `search-es` renders
//! it inside its error envelope; [`AggError::to_json`] gives the bare exception object.

use std::borrow::Cow;

use crate::json::Json;

/// An Elasticsearch-typed aggregation failure.
#[derive(Clone, Debug, PartialEq)]
pub struct AggError {
    pub kind: Cow<'static, str>,
    pub reason: String,
    pub status: u16,
    /// Extra fields printed after `reason` (`line`, `col`, `max_buckets`, …), in order.
    pub meta: Vec<(String, Json)>,
    pub caused_by: Option<Box<AggError>>,
}

pub type Result<T, E = AggError> = std::result::Result<T, E>;

impl std::fmt::Display for AggError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.kind, self.reason)
    }
}

impl std::error::Error for AggError {}

impl AggError {
    pub fn new(status: u16, kind: impl Into<Cow<'static, str>>, reason: impl Into<String>) -> AggError {
        AggError { kind: kind.into(), reason: reason.into(), status, meta: Vec::new(), caused_by: None }
    }

    /// `IllegalArgumentException` (400).
    pub fn illegal_argument(reason: impl Into<String>) -> AggError {
        AggError::new(400, "illegal_argument_exception", reason)
    }

    /// `IllegalStateException` (500).
    pub fn illegal_state(reason: impl Into<String>) -> AggError {
        AggError::new(500, "illegal_state_exception", reason)
    }

    /// `ParsingException` at `line:col` (the location goes in its own fields).
    pub fn parsing(line_col: (usize, usize), reason: impl Into<String>) -> AggError {
        AggError::new(400, "parsing_exception", reason)
            .with_meta("line", Json::from(line_col.0 as u64))
            .with_meta("col", Json::from(line_col.1 as u64))
    }

    /// `XContentParseException`: the location prefixes the reason (`[1:24] …`).
    pub fn x_content(line_col: (usize, usize), reason: impl AsRef<str>) -> AggError {
        AggError::new(400, "x_content_parse_exception", format!("[{}:{}] {}", line_col.0, line_col.1, reason.as_ref()))
    }

    /// `AggregationExecutionException` (500 unless the caller says otherwise).
    pub fn execution(reason: impl Into<String>) -> AggError {
        AggError::new(500, "aggregation_execution_exception", reason)
    }

    /// `AggregationExecutionException.InvalidPath` (400; its type is the nested class name).
    pub fn invalid_path(reason: impl Into<String>) -> AggError {
        AggError::new(400, "invalid_path", reason)
    }

    /// `InvalidAggregationPathException` (400).
    pub fn invalid_agg_path(reason: impl Into<String>) -> AggError {
        AggError::new(400, "invalid_aggregation_path_exception", reason)
    }

    /// `IllegalArgumentException` raised by the cast of an aggregation of the wrong kind.
    pub fn class_cast(reason: impl Into<String>) -> AggError {
        AggError::new(500, "class_cast_exception", reason)
    }

    /// `MultiBucketConsumerService.TooManyBucketsException`.
    pub fn too_many_buckets(limit: usize) -> AggError {
        AggError::new(
            400,
            "too_many_buckets_exception",
            format!(
                "Trying to create too many buckets. Must be less than or equal to: [{limit}] but this number of buckets was exceeded. This limit can be set by changing the [search.max_buckets] cluster level setting."
            ),
        )
        .with_meta("max_buckets", Json::from(limit as u64))
    }

    /// `NumberFormatException` with Java's text.
    pub fn number_format(s: &str) -> AggError {
        AggError::new(400, "number_format_exception", format!("For input string: \"{s}\""))
    }

    pub fn with_meta(mut self, key: impl Into<String>, v: Json) -> AggError {
        self.meta.push((key.into(), v));
        self
    }

    pub fn caused_by(mut self, cause: AggError) -> AggError {
        self.caused_by = Some(Box::new(cause));
        self
    }

    pub fn with_status(mut self, status: u16) -> AggError {
        self.status = status;
        self
    }

    /// The innermost cause (what Elasticsearch reports as `root_cause` for a plain chain).
    pub fn root_cause(&self) -> &AggError {
        let mut e = self;
        while let Some(c) = &e.caused_by {
            // Elasticsearch's `guessRootCauses` stops at the first ElasticsearchException
            // carrying a cause that is not one; plain chains go to the bottom.
            if is_java_exception(&c.kind) {
                break;
            }
            e = c;
        }
        e
    }

    /// The exception object (`{type, reason, …, caused_by}`).
    pub fn to_json(&self) -> Json {
        let mut o = Json::obj().with("type", Json::str(self.kind.as_ref())).with("reason", Json::str(self.reason.clone()));
        for (k, v) in &self.meta {
            o.push(k.clone(), v.clone());
        }
        if let Some(c) = &self.caused_by {
            o.push("caused_by", c.to_json());
        }
        o
    }
}

/// Plain Java exceptions (not `ElasticsearchException`s) that may appear in cause chains.
pub fn is_java_exception(kind: &str) -> bool {
    matches!(
        kind,
        "illegal_argument_exception"
            | "illegal_state_exception"
            | "number_format_exception"
            | "class_cast_exception"
            | "arithmetic_exception"
            | "null_pointer_exception"
            | "unsupported_operation_exception"
            | "date_time_parse_exception"
            | "pattern_syntax_exception"
    )
}

impl From<search_index::Error> for AggError {
    fn from(e: search_index::Error) -> AggError {
        match e {
            search_index::Error::Es { kind, reason, status } => AggError { kind, reason, status, meta: Vec::new(), caused_by: None },
            other => AggError::new(other.status(), other.kind().to_string(), other.to_string()),
        }
    }
}

impl From<AggError> for search_index::Error {
    fn from(e: AggError) -> search_index::Error {
        search_index::Error::Es { kind: e.kind, reason: e.reason, status: e.status }
    }
}

/// `SuggestingErrorOnUnknown.suggest`: ` did you mean [x]?` / ` did you mean any of [x, y]?`
/// for candidates more than half similar (Lucene's `LevenshteinDistance`), or `""`.
pub fn suggest(unknown: &str, candidates: &[&str]) -> String {
    let mut scored: Vec<(f32, &str)> =
        candidates.iter().map(|c| (levenshtein_similarity(unknown, c), *c)).filter(|(d, _)| *d > 0.5).collect();
    if scored.is_empty() {
        return String::new();
    }
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal).then_with(|| a.1.cmp(b.1)));
    if scored.len() == 1 {
        return format!(" did you mean [{}]?", scored[0].1);
    }
    let names: Vec<&str> = scored.iter().map(|(_, c)| *c).collect();
    format!(" did you mean any of [{}]?", names.join(", "))
}

/// Lucene's `LevenshteinDistance.getDistance`: `1 - distance / max(len)`.
pub fn levenshtein_similarity(target: &str, other: &str) -> f32 {
    let s: Vec<char> = target.chars().collect();
    let t: Vec<char> = other.chars().collect();
    let (n, m) = (s.len(), t.len());
    if n == 0 || m == 0 {
        return if n == m { 1.0 } else { 0.0 };
    }
    let mut p: Vec<usize> = (0..=n).collect();
    let mut d: Vec<usize> = vec![0; n + 1];
    for j in 1..=m {
        let t_j = t[j - 1];
        d[0] = j;
        for i in 1..=n {
            let cost = if s[i - 1] == t_j { 0 } else { 1 };
            d[i] = (d[i - 1] + 1).min(p[i] + 1).min(p[i - 1] + cost);
        }
        std::mem::swap(&mut p, &mut d);
    }
    1.0 - (p[n] as f32 / m.max(n) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions() {
        assert_eq!(suggest("fieldx", &["field", "script", "missing"]), " did you mean [field]?");
        assert_eq!(suggest("zzz", &["field"]), "");
    }

    #[test]
    fn too_many_buckets_shape() {
        let e = AggError::too_many_buckets(65536);
        let j = e.to_json();
        assert_eq!(j.get("max_buckets"), Some(&Json::Int(65536)));
        assert_eq!(e.status, 400);
    }
}
