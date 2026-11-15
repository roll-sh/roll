//! Errors carrying Elasticsearch's exception type, reason text and HTTP status.

use std::fmt;

/// An analysis error as Elasticsearch reports it: `kind` is the exception type
/// (`illegal_argument_exception`, `settings_exception` …), `reason` its message, `status` the HTTP
/// status Elasticsearch answers with, and `caused_by` the nested cause it renders. An empty
/// `reason` is a Java exception without a message (`"reason": null`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnalysisError {
    pub kind: &'static str,
    pub reason: String,
    pub status: u16,
    pub caused_by: Option<Box<AnalysisError>>,
}

impl AnalysisError {
    /// `illegal_argument_exception` (400), the type of almost every analysis error.
    pub fn illegal(reason: impl Into<String>) -> AnalysisError {
        AnalysisError {
            kind: "illegal_argument_exception",
            reason: reason.into(),
            status: 400,
            caused_by: None,
        }
    }

    /// `settings_exception`, which Elasticsearch answers with 500 at index creation.
    pub fn settings(reason: impl Into<String>) -> AnalysisError {
        AnalysisError {
            kind: "settings_exception",
            reason: reason.into(),
            status: 500,
            caused_by: None,
        }
    }

    pub fn with_cause(mut self, cause: AnalysisError) -> AnalysisError {
        self.caused_by = Some(Box::new(cause));
        self
    }

    pub(crate) fn other(
        kind: &'static str,
        status: u16,
        reason: impl Into<String>,
    ) -> AnalysisError {
        AnalysisError {
            kind,
            reason: reason.into(),
            status,
            caused_by: None,
        }
    }

    /// Elasticsearch's `root_cause` entry for this error (`guessRootCauses`): it descends to a
    /// cause only while both the error and its cause are Elasticsearch exceptions, so a plain
    /// Java exception (which analysis code mostly throws) or an Elasticsearch exception caused
    /// by one is its own root cause.
    pub fn root_cause(&self) -> &AnalysisError {
        let mut cur = self;
        while !is_java_exception(cur.kind)
            && let Some(c) = cur.caused_by.as_deref()
            && !is_java_exception(c.kind)
        {
            cur = c;
        }
        cur
    }

    /// Renders `{"type", "reason", "caused_by"?}` as Elasticsearch does inside `error`.
    pub fn to_json(&self) -> serde_json::Value {
        let mut m = serde_json::Map::new();
        m.insert("type".into(), self.kind.into());
        // An empty reason stands for an exception without a message, which renders as null.
        let reason = if self.reason.is_empty() {
            serde_json::Value::Null
        } else {
            self.reason.clone().into()
        };
        m.insert("reason".into(), reason);
        if let Some(c) = &self.caused_by {
            m.insert("caused_by".into(), c.to_json());
        }
        serde_json::Value::Object(m)
    }
}

impl fmt::Display for AnalysisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.reason)
    }
}

impl std::error::Error for AnalysisError {}

pub(crate) type Result<T> = std::result::Result<T, AnalysisError>;

/// The exception types analysis errors carry that are plain Java exceptions rather than
/// `ElasticsearchException`s.
pub fn is_java_exception(kind: &str) -> bool {
    matches!(
        kind,
        "illegal_argument_exception"
            | "illegal_state_exception"
            | "number_format_exception"
            | "unsupported_operation_exception"
            | "array_index_out_of_bounds_exception"
            | "null_pointer_exception"
            | "pattern_syntax_exception"
            | "class_cast_exception"
            | "runtime_exception"
            | "i_o_exception"
            | "no_such_file_exception"
            | "class_not_found_exception"
            | "not_entitled_exception"
            | "no_such_element_exception"
            | "malformed_input_exception"
            | "exception"
    )
}
