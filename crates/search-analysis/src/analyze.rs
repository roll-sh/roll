//! The `_analyze` API: request parsing, validation, analyzer selection and the response body,
//! following `TransportAnalyzeAction`.
//!
//! The REST layer parses the URL (index) and resolves `field` against the mapping; everything
//! else (which analyzer runs, inline component resolution, explain stages, token and character
//! limits, error types and texts) happens here so the output matches Elasticsearch's.

use serde_json::{Value, json};

use crate::analysis::Analysis;
use crate::analyzer::Analyzer;
use crate::error::{AnalysisError, Result};

/// A component given by name or inline definition.
#[derive(Clone, Debug, PartialEq)]
pub enum NameOrDefinition {
    Name(String),
    Definition(Value),
}

impl NameOrDefinition {
    fn from_json(v: &Value) -> Result<NameOrDefinition> {
        match v {
            Value::String(s) => Ok(NameOrDefinition::Name(s.clone())),
            Value::Object(_) => Ok(NameOrDefinition::Definition(v.clone())),
            other => Err(AnalysisError::other(
                "x_content_parse_exception",
                400,
                format!(
                    "Expected [VALUE_STRING] or [START_OBJECT], got {}",
                    token_name(other)
                ),
            )),
        }
    }
}

fn token_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "VALUE_NULL",
        Value::Bool(_) => "VALUE_BOOLEAN",
        Value::Number(_) => "VALUE_NUMBER",
        Value::String(_) => "VALUE_STRING",
        Value::Array(_) => "START_ARRAY",
        Value::Object(_) => "START_OBJECT",
    }
}

/// An `_analyze` request body.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AnalyzeRequest {
    pub text: Vec<String>,
    pub analyzer: Option<String>,
    pub tokenizer: Option<NameOrDefinition>,
    pub filter: Vec<NameOrDefinition>,
    pub char_filter: Vec<NameOrDefinition>,
    pub field: Option<String>,
    pub explain: bool,
    pub attributes: Vec<String>,
    pub normalizer: Option<String>,
}

/// `_analyze` limits (index settings `index.analyze.max_token_count` and
/// `index.analyze.max_char_count`).
#[derive(Clone, Copy, Debug)]
pub struct AnalyzeLimits {
    pub max_token_count: usize,
    pub max_char_count: usize,
}

impl Default for AnalyzeLimits {
    fn default() -> Self {
        AnalyzeLimits {
            max_token_count: 10_000,
            max_char_count: 1_000_000,
        }
    }
}

fn parse_error(msg: String) -> AnalysisError {
    AnalysisError::other("x_content_parse_exception", 400, msg)
}

fn unsupported(field: &str, v: &Value) -> AnalysisError {
    parse_error(format!(
        "[analyze_request] {field} doesn't support values of type: {}",
        token_name(v)
    ))
}

fn failed_field(field: &str) -> AnalysisError {
    parse_error(format!("[analyze_request] failed to parse field [{field}]"))
}

/// `declareStringArray`: a string, or an array of strings, numbers and booleans.
fn string_array(field: &str, v: &Value) -> Result<Vec<String>> {
    match v {
        Value::String(s) => Ok(vec![s.clone()]),
        Value::Array(a) => a
            .iter()
            .map(|v| match v {
                Value::String(s) => Ok(s.clone()),
                Value::Number(n) => Ok(n.to_string()),
                Value::Bool(b) => Ok(b.to_string()),
                Value::Array(_) => Err(failed_field(field).with_cause(AnalysisError::other(
                    "illegal_state_exception",
                    500,
                    "expected value but got [START_ARRAY]",
                ))),
                // Jackson says "Expected text at <line>:<column> but found …"; the location
                // is the REST layer's to add, as for the outer error.
                other => Err(
                    failed_field(field).with_cause(AnalysisError::illegal(format!(
                        "Expected text but found {}",
                        token_name(other)
                    ))),
                ),
            })
            .collect(),
        other => Err(unsupported(field, other)),
    }
}

/// `declareString`: only a string.
fn string(field: &str, v: &Value) -> Result<String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        other => Err(unsupported(field, other)),
    }
}

/// A `NameOrDefinition` field (`tokenizer`): a string or an object.
fn name_or_definition(field: &str, v: &Value) -> Result<NameOrDefinition> {
    match v {
        Value::String(_) | Value::Object(_) => NameOrDefinition::from_json(v),
        other => Err(unsupported(field, other)),
    }
}

/// `declareObjectArray` of `NameOrDefinition`s (`filter`, `char_filter`): an array of strings
/// and objects, or a single object.
fn components(field: &str, v: &Value) -> Result<Vec<NameOrDefinition>> {
    match v {
        Value::Array(a) => a
            .iter()
            .map(|v| match v {
                Value::Array(_) => Err(failed_field(field).with_cause(AnalysisError::other(
                    "illegal_state_exception",
                    500,
                    "expected value but got [START_ARRAY]",
                ))),
                v => NameOrDefinition::from_json(v).map_err(|e| failed_field(field).with_cause(e)),
            })
            .collect(),
        Value::Object(_) => Ok(vec![NameOrDefinition::from_json(v)?]),
        other => Err(unsupported(field, other)),
    }
}

impl AnalyzeRequest {
    /// Parses a request body (`AnalyzeAction.Request.fromXContent`). Unknown fields fail with
    /// `[analyze_request] unknown field [x]`; the REST layer prefixes the `[line:column]` of the
    /// field as Elasticsearch does.
    pub fn from_json(body: &Value) -> Result<AnalyzeRequest> {
        let Value::Object(m) = body else {
            return Err(parse_error(
                "[analyze_request] Expected START_OBJECT".into(),
            ));
        };
        let mut r = AnalyzeRequest::default();
        for (k, v) in m {
            match k.as_str() {
                "text" => r.text = string_array("text", v)?,
                "analyzer" => r.analyzer = Some(string("analyzer", v)?),
                "tokenizer" => r.tokenizer = Some(name_or_definition("tokenizer", v)?),
                "filter" => r.filter = components("filter", v)?,
                "char_filter" => r.char_filter = components("char_filter", v)?,
                "field" => r.field = Some(string("field", v)?),
                "explain" => {
                    r.explain = match v {
                        Value::Bool(b) => *b,
                        Value::String(s) if s == "true" => true,
                        Value::String(s) if s == "false" => false,
                        Value::String(s) => {
                            return Err(failed_field("explain").with_cause(
                                AnalysisError::illegal(format!(
                                    "Failed to parse value [{s}] as only [true] or [false] are allowed."
                                )),
                            ));
                        }
                        other => {
                            return Err(unsupported("explain", other));
                        }
                    }
                }
                "attributes" => r.attributes = string_array("attributes", v)?,
                "normalizer" => r.normalizer = Some(string("normalizer", v)?),
                other => {
                    return Err(parse_error(format!(
                        "[analyze_request] unknown field [{other}]"
                    )));
                }
            }
        }
        Ok(r)
    }

    /// Only the component fields (for [`Analysis::build_ad_hoc_json`]).
    pub(crate) fn components_from_json(def: &Value) -> Result<AnalyzeRequest> {
        let mut r = AnalyzeRequest::default();
        if let Value::Object(m) = def {
            if let Some(t) = m.get("tokenizer") {
                r.tokenizer = Some(name_or_definition("tokenizer", t)?);
            }
            if let Some(f) = m.get("filter") {
                r.filter = components("filter", f)?;
            }
            if let Some(c) = m.get("char_filter") {
                r.char_filter = components("char_filter", c)?;
            }
        }
        Ok(r)
    }

    /// `AnalyzeAction.Request.validate()`.
    pub fn validate(&self, has_index: bool) -> Result<()> {
        let mut errors = Vec::new();
        if self.text.is_empty() {
            errors.push("text is missing");
        }
        if !has_index && self.normalizer.is_some() {
            errors.push("index is required if normalizer is specified");
        }
        if self.normalizer.is_some() && (self.tokenizer.is_some() || self.analyzer.is_some()) {
            errors.push("tokenizer/analyze should be null if normalizer is specified");
        }
        let extra =
            self.tokenizer.is_some() || !self.char_filter.is_empty() || !self.filter.is_empty();
        if self.analyzer.is_some() && extra {
            errors.push("cannot define extra components on a named analyzer");
        }
        if self.normalizer.is_some() && extra {
            errors.push("cannot define extra components on a named normalizer");
        }
        if self.field.is_some() && extra {
            errors.push("cannot define extra components on a field-specific analyzer");
        }
        if errors.is_empty() {
            return Ok(());
        }
        let mut msg = String::from("Validation Failed: ");
        for (i, e) in errors.iter().enumerate() {
            msg.push_str(&format!("{}: {e};", i + 1));
        }
        Err(AnalysisError::other(
            "action_request_validation_exception",
            400,
            msg,
        ))
    }
}

fn char_limit_error(max: usize) -> AnalysisError {
    AnalysisError::other(
        "status_exception",
        400,
        format!(
            "The number of characters produced by calling _analyze has exceeded the allowed maximum of [{max}]. This limit can be set by changing the [index.analyze.max_char_count] index level setting."
        ),
    )
}

impl Analysis {
    /// Runs an `_analyze` request against this index's analysis ([`Analysis::global`] when the
    /// request names no index) and returns the response body. `field_analyzer` is the index
    /// analyzer of `request.field` when the field exists and is a text field (the REST layer
    /// answers for non-text fields itself); an unknown field falls back to the default analyzer.
    pub fn analyze(
        &self,
        req: &AnalyzeRequest,
        field_analyzer: Option<&Analyzer>,
        limits: AnalyzeLimits,
    ) -> Result<Value> {
        req.validate(!self.is_global())?;
        let analyzer = self.select(req, field_analyzer)?;
        let texts: Vec<&str> = req.text.iter().map(String::as_str).collect();
        for t in &texts {
            if analyzer.char_filtered_utf16_len(t)? > limits.max_char_count {
                return Err(char_limit_error(limits.max_char_count));
            }
        }
        if req.explain {
            let explain =
                analyzer.explain_limited(&texts, &req.attributes, limits.max_token_count)?;
            return Ok(json!({ "detail": explain.to_json() }));
        }
        let tokens = analyzer.simple_tokens(&texts, limits.max_token_count)?;
        Ok(json!({ "tokens": tokens.iter().map(|t| t.to_json()).collect::<Vec<_>>() }))
    }

    /// Which analyzer an `_analyze` request runs (`TransportAnalyzeAction.analyze`).
    fn select(&self, req: &AnalyzeRequest, field_analyzer: Option<&Analyzer>) -> Result<Analyzer> {
        if req.tokenizer.is_some() || !req.filter.is_empty() || !req.char_filter.is_empty() {
            return self
                .build_ad_hoc(req.tokenizer.as_ref(), &req.char_filter, &req.filter)
                .map_err(|e| {
                    if e.kind == "illegal_state_exception" {
                        AnalysisError::illegal("Can not build a custom analyzer").with_cause(e)
                    } else {
                        e
                    }
                });
        }
        if let Some(name) = &req.analyzer {
            return match self.is_global() {
                true => crate::analysis::builtin_analyzer(name)
                    .transpose()?
                    .ok_or_else(|| {
                        AnalysisError::illegal(format!("failed to find global analyzer [{name}]"))
                    }),
                false => self.analyzer(name).ok_or_else(|| {
                    AnalysisError::illegal(format!("failed to find analyzer [{name}]"))
                }),
            };
        }
        if let Some(name) = &req.normalizer {
            if self.is_global() {
                return Err(AnalysisError::illegal(
                    "analysis based on a normalizer requires an index",
                ));
            }
            return self
                .normalizer(name)
                .map(|n| n.analyzer().clone())
                .ok_or_else(|| {
                    AnalysisError::illegal(format!("failed to find normalizer under [{name}]"))
                });
        }
        if req.field.is_some() {
            if self.is_global() {
                return Err(AnalysisError::illegal(
                    "analysis based on a specific field requires an index",
                ));
            }
            if let Some(a) = field_analyzer {
                return Ok(a.clone());
            }
        }
        if self.is_global() {
            return crate::analysis::builtin_analyzer("standard").expect("standard is built in");
        }
        Ok(self.default_analyzer())
    }
}
