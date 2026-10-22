//! `EsError`: exceptions rendered the way Elasticsearch renders them.
//!
//! Elasticsearch's REST error body is
//! `{"error":{"root_cause":[…],"type":…,"reason":…,<metadata>,"caused_by":{…}},"status":N}`.
//! The pieces that clients and the YAML suites look at are reproduced exactly:
//!
//! - `type` is the snake_case exception name (`index_not_found_exception`), `reason` the
//!   message, followed by metadata fields in insertion order (`resource.type`, `index_uuid`,
//!   `index`, …), then `caused_by`, `header`, `stack_trace` (only with `error_trace=true`) and
//!   `suppressed`.
//! - `root_cause` is Elasticsearch's guess: follow `caused_by` while the cause is an
//!   `ElasticsearchException` and report the deepest one. Plain Java exceptions
//!   (`illegal_argument_exception`, …) are never descended into; a chain that starts with one
//!   reports itself. Root causes render without their causes.
//! - The HTTP status comes from the outermost exception.
//!
//! Constructors for the exceptions roll returns live here so every call site uses the same
//! texts.

use crate::json::{Map, Value};
use crate::obj;
use std::borrow::Cow;
use std::fmt;

/// An exception as Elasticsearch renders it. Kept small (the rarely used parts are boxed) so
/// `Result<_, EsError>` stays cheap to move on the success path.
#[derive(Clone, Debug, PartialEq)]
pub struct EsError {
    pub status: u16,
    /// `true` for subclasses of `ElasticsearchException` (affects root-cause guessing).
    pub es: bool,
    pub kind: Cow<'static, str>,
    /// `None` renders as `"reason": null`.
    pub reason: Option<String>,
    pub caused_by: Option<Box<EsError>>,
    ext: Option<Box<Ext>>,
}

#[derive(Clone, Debug, PartialEq, Default)]
struct Ext {
    /// Extra fields after `reason`, in order.
    meta: Vec<(Cow<'static, str>, Value)>,
    /// Explicit root causes (e.g. one per failed shard) instead of the guessed one.
    root_causes: Option<Vec<EsError>>,
    /// The `header` object (e.g. `WWW-Authenticate`), also sent as HTTP headers.
    headers: Vec<(String, Vec<String>)>,
    suppressed: Vec<EsError>,
}

pub type EsResult<T> = Result<T, EsError>;

impl fmt::Display for EsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.reason.as_deref().unwrap_or("null"))
    }
}

impl std::error::Error for EsError {}

impl EsError {
    /// An `ElasticsearchException` subclass.
    pub fn new(status: u16, kind: impl Into<Cow<'static, str>>, reason: impl Into<String>) -> EsError {
        EsError { status, es: true, kind: kind.into(), reason: Some(reason.into()), caused_by: None, ext: None }
    }

    /// A plain Java exception (`IllegalArgumentException`, …).
    pub fn java(status: u16, kind: impl Into<Cow<'static, str>>, reason: impl Into<String>) -> EsError {
        EsError { es: false, ..EsError::new(status, kind, reason) }
    }

    fn ext_mut(&mut self) -> &mut Ext {
        self.ext.get_or_insert_with(Default::default)
    }

    pub fn with_meta(mut self, key: impl Into<Cow<'static, str>>, v: impl Into<Value>) -> EsError {
        self.ext_mut().meta.push((key.into(), v.into()));
        self
    }

    /// Extra fields after `reason`.
    pub fn meta(&self) -> &[(Cow<'static, str>, Value)] {
        self.ext.as_ref().map(|e| e.meta.as_slice()).unwrap_or(&[])
    }

    /// The `header` entries (also sent as HTTP headers).
    pub fn headers(&self) -> &[(String, Vec<String>)] {
        self.ext.as_ref().map(|e| e.headers.as_slice()).unwrap_or(&[])
    }

    pub fn suppressed(&self) -> &[EsError] {
        self.ext.as_ref().map(|e| e.suppressed.as_slice()).unwrap_or(&[])
    }

    pub fn with_suppressed(mut self, others: Vec<EsError>) -> EsError {
        self.ext_mut().suppressed = others;
        self
    }

    /// Explicit root causes (one per failed shard, …) instead of the guessed one.
    pub fn with_root_causes(mut self, causes: Vec<EsError>) -> EsError {
        self.ext_mut().root_causes = Some(causes);
        self
    }

    fn root_causes(&self) -> Option<&Vec<EsError>> {
        self.ext.as_ref().and_then(|e| e.root_causes.as_ref())
    }

    pub fn with_header_values(mut self, name: impl Into<String>, values: Vec<String>) -> EsError {
        self.ext_mut().headers.push((name.into(), values));
        self
    }

    /// The first line of the stack trace: `ElasticsearchException.toString()`, which puts the
    /// index (`[name]`, or `[name/uuid]` when the uuid is known) and shard in front.
    fn trace_head(&self) -> String {
        let meta = |k: &str| self.meta().iter().find(|(mk, _)| mk == k).map(|(_, v)| v.to_es_string());
        let mut out = String::new();
        if self.es
            && let Some(index) = meta("index")
        {
            match meta("index_uuid").filter(|u| u != "_na_") {
                Some(uuid) => out.push_str(&format!("[{index}/{uuid}]")),
                None => out.push_str(&format!("[{index}]")),
            }
            if let Some(shard) = meta("shard") {
                out.push_str(&format!("[{shard}]"));
            }
            out.push(' ');
        }
        out.push_str(&format!("{}: {}", java_class(&self.kind), self.reason()));
        out
    }

    pub fn caused_by(mut self, cause: EsError) -> EsError {
        self.caused_by = Some(Box::new(cause));
        self
    }

    pub fn with_status(mut self, status: u16) -> EsError {
        self.status = status;
        self
    }

    pub fn with_header(self, name: impl Into<String>, value: impl Into<String>) -> EsError {
        self.with_header_values(name, vec![value.into()])
    }

    /// Renders `"reason": null` (exceptions without a message).
    pub fn with_null_reason(mut self) -> EsError {
        self.reason = None;
        self
    }

    pub fn reason(&self) -> &str {
        self.reason.as_deref().unwrap_or("")
    }

    /// The deepest `ElasticsearchException` in the cause chain (Elasticsearch's guess).
    pub fn guess_root_causes(&self) -> Vec<&EsError> {
        if let Some(rc) = self.root_causes() {
            return rc.iter().collect();
        }
        if !self.es {
            return vec![self];
        }
        let mut cur = self;
        while let Some(c) = &cur.caused_by {
            if !c.es {
                break;
            }
            if let Some(rc) = c.root_causes() {
                return rc.iter().collect();
            }
            cur = c;
        }
        vec![cur]
    }

    fn render_into(&self, m: &mut Map, with_cause: bool, trace: bool, depth: usize) {
        self.render_into_with(m, with_cause, with_cause, trace, depth)
    }

    fn render_into_with(&self, m: &mut Map, with_cause: bool, with_suppressed: bool, trace: bool, depth: usize) {
        if depth > 100 {
            m.insert("type".into(), "illegal_state_exception".into());
            m.insert("reason".into(), "too many nested exceptions".into());
            return;
        }
        m.insert("type".into(), Value::from(self.kind.as_ref()));
        m.insert("reason".into(), self.reason.clone().into());
        for (k, v) in self.meta() {
            m.insert(k.to_string(), v.clone());
        }
        if with_cause && let Some(c) = &self.caused_by {
            let mut cm = Map::new();
            c.render_into(&mut cm, true, trace, depth + 1);
            m.insert("caused_by".into(), Value::Obj(cm));
        }
        if !self.headers().is_empty() {
            let mut h = Map::new();
            for (k, vs) in self.headers() {
                let v = if vs.len() == 1 { Value::from(vs[0].clone()) } else { Value::from(vs.clone()) };
                h.insert(k.clone(), v);
            }
            m.insert("header".into(), Value::Obj(h));
        }
        if trace {
            m.insert("stack_trace".into(), Value::from(self.trace_head()));
        }
        // Root cause entries leave out causes and (in Elasticsearch) suppressed exceptions.
        if with_suppressed && !self.suppressed().is_empty() {
            let items = self
                .suppressed()
                .iter()
                .map(|s| {
                    let mut sm = Map::new();
                    s.render_into(&mut sm, true, trace, depth + 1);
                    Value::Obj(sm)
                })
                .collect();
            m.insert("suppressed".into(), Value::Arr(items));
        }
    }

    /// The exception as a nested object (`caused_by` / shard failure entries).
    pub fn to_value(&self, trace: bool) -> Value {
        let mut m = Map::new();
        self.render_into(&mut m, true, trace, 0);
        Value::Obj(m)
    }

    /// The full REST error body: `{"error":{…},"status":N}`.
    pub fn to_body(&self, trace: bool) -> Value {
        self.to_body_with(trace, false)
    }

    /// [`EsError::to_body`]; OpenSearch keeps `suppressed` in root cause entries.
    pub fn to_body_with(&self, trace: bool, roots_keep_suppressed: bool) -> Value {
        let mut err = Map::new();
        let roots = self
            .guess_root_causes()
            .into_iter()
            .map(|r| {
                let mut rm = Map::new();
                r.render_into_with(&mut rm, false, roots_keep_suppressed, trace, 0);
                Value::Obj(rm)
            })
            .collect::<Vec<_>>();
        err.insert("root_cause".into(), Value::Arr(roots));
        self.render_into(&mut err, true, trace, 0);
        obj! { "error" => Value::Obj(err), "status" => self.status as i64 }
    }

    // --- Constructors with Elasticsearch's texts. ---

    pub fn illegal_argument(reason: impl Into<String>) -> EsError {
        EsError::java(400, "illegal_argument_exception", reason)
    }

    /// Java's `NumberFormatException` for an unparsable number: `For input string: "<s>"`.
    pub fn number_format(s: &str) -> EsError {
        EsError::java(400, "number_format_exception", format!("For input string: \"{s}\""))
    }

    pub fn illegal_state(reason: impl Into<String>) -> EsError {
        EsError::java(500, "illegal_state_exception", reason)
    }

    pub fn parse(reason: impl Into<String>) -> EsError {
        EsError::new(400, "parse_exception", reason)
    }

    /// `ElasticsearchParseException` raised while reading a request body at `[line:col]`.
    pub fn x_content_parse(line: usize, col: usize, reason: impl Into<String>) -> EsError {
        EsError::new(400, "x_content_parse_exception", format!("[{line}:{col}] {}", reason.into()))
    }

    /// `XContentParseException` without a location.
    pub fn x_content_parse_bare(reason: impl Into<String>) -> EsError {
        EsError::new(400, "x_content_parse_exception", reason)
    }

    /// `ParsingException` (query and search source parsing): `line` and `col` as fields.
    pub fn parsing(line: usize, col: usize, reason: impl Into<String>) -> EsError {
        EsError::new(400, "parsing_exception", reason).with_meta("line", line as i64).with_meta("col", col as i64)
    }

    pub fn mapper_parsing(reason: impl Into<String>) -> EsError {
        EsError::new(400, "mapper_parsing_exception", reason)
    }

    pub fn document_parsing(reason: impl Into<String>) -> EsError {
        EsError::new(400, "document_parsing_exception", reason)
    }

    pub fn status(status: u16, reason: impl Into<String>) -> EsError {
        EsError::new(status, "status_exception", reason)
    }

    pub fn resource_not_found(reason: impl Into<String>) -> EsError {
        EsError::new(404, "resource_not_found_exception", reason)
    }

    pub fn resource_already_exists(reason: impl Into<String>) -> EsError {
        EsError::new(400, "resource_already_exists_exception", reason)
    }

    pub fn action_request_validation(errors: &[String]) -> EsError {
        let mut s = String::from("Validation Failed: ");
        for (i, e) in errors.iter().enumerate() {
            s.push_str(&format!("{}: {e};", i + 1));
        }
        EsError::new(400, "action_request_validation_exception", s)
    }

    pub fn index_not_found(index: &str) -> EsError {
        EsError::new(404, "index_not_found_exception", format!("no such index [{index}]"))
            .with_meta("resource.type", "index_or_alias")
            .with_meta("resource.id", index)
            .with_meta("index_uuid", "_na_")
            .with_meta("index", index)
    }

    /// `IndexNotFoundException` with a custom message (e.g. `no such index [x] and …`).
    /// `IndexNotFoundException` built from a name alone (no `resource.*` fields).
    pub fn index_not_found_bare(index: &str) -> EsError {
        EsError::new(404, "index_not_found_exception", format!("no such index [{index}]")).with_meta("index_uuid", "_na_").with_meta("index", index)
    }

    pub fn index_not_found_msg(index: &str, msg: impl Into<String>) -> EsError {
        EsError::new(404, "index_not_found_exception", msg)
            .with_meta("resource.type", "index_or_alias")
            .with_meta("resource.id", index)
            .with_meta("index_uuid", "_na_")
            .with_meta("index", index)
    }

    pub fn index_already_exists(index: &str, uuid: &str) -> EsError {
        EsError::resource_already_exists(format!("index [{index}/{uuid}] already exists"))
            .with_meta("index_uuid", uuid)
            .with_meta("index", index)
    }

    pub fn invalid_index_name(index: &str, why: &str) -> EsError {
        EsError::new(400, "invalid_index_name_exception", format!("Invalid index name [{index}], {why}"))
            .with_meta("index_uuid", "_na_")
            .with_meta("index", index)
    }

    /// `InvalidIndexNameException(name, why)` as the expression resolver raises it (no comma
    /// before the reason's own punctuation is added).
    pub fn invalid_index_name_dot(index: &str, why: &str) -> EsError {
        EsError::invalid_index_name(index, why)
    }

    pub fn invalid_alias_name(alias: &str, why: &str) -> EsError {
        EsError::new(400, "invalid_alias_name_exception", format!("Invalid alias name [{alias}]: {why}"))
    }

    /// `AliasesNotFoundException`: `aliases [a, b] missing`, names in the order of the
    /// `HashSet` Elasticsearch collects them in.
    pub fn alias_not_found(names: &[String]) -> EsError {
        let mut unique: Vec<&String> = Vec::new();
        for n in names {
            if !unique.contains(&n) {
                unique.push(n);
            }
        }
        let names: Vec<String> = crate::json::java_hash_order(&unique, 16).into_iter().map(|i| unique[i].clone()).collect();
        let e = EsError::new(404, "aliases_not_found_exception", format!("aliases [{}] missing", names.join(", "))).with_meta("resource.type", "aliases");
        match names.len() {
            0 => e,
            1 => e.with_meta("resource.id", Value::from(names[0].clone())),
            _ => e.with_meta("resource.id", Value::from(names.clone())),
        }
    }

    pub fn index_template_missing(name: &str) -> EsError {
        EsError::new(404, "index_template_missing_exception", format!("index_template [{name}] missing"))
    }

    pub fn invalid_index_template(name: &str, why: &str) -> EsError {
        EsError::new(400, "invalid_index_template_exception", format!("index_template [{name}] invalid, cause [{why}]"))
    }

    pub fn closed_index(index: &str, uuid: &str) -> EsError {
        EsError::new(400, "index_closed_exception", "closed").with_meta("index_uuid", uuid).with_meta("index", index)
    }

    pub fn not_implemented(what: &str) -> EsError {
        EsError::java(
            501,
            "unsupported_operation_exception",
            format!("[{what}] is not supported by roll search yet"),
        )
    }

    pub fn timeout(reason: impl Into<String>) -> EsError {
        EsError::new(408, "timeout_exception", reason)
    }

    pub fn security(status: u16, reason: impl Into<String>) -> EsError {
        EsError::new(status, "security_exception", reason)
    }

    pub fn too_many_requests(reason: impl Into<String>) -> EsError {
        EsError::new(429, "es_rejected_execution_exception", reason)
    }

    pub fn content_too_long(len: u64, max: u64) -> EsError {
        EsError::new(413, "content_too_long_exception", format!("Request content length [{len}] exceeds the limit of [{max}] bytes"))
    }

    pub fn internal(reason: impl Into<String>) -> EsError {
        EsError::new(500, "exception", reason)
    }
}

/// The Java class Elasticsearch would name in a stack trace.
fn java_class(kind: &str) -> String {
    match kind {
        "illegal_argument_exception" => "java.lang.IllegalArgumentException".into(),
        "illegal_state_exception" => "java.lang.IllegalStateException".into(),
        "unsupported_operation_exception" => "java.lang.UnsupportedOperationException".into(),
        "number_format_exception" => "java.lang.NumberFormatException".into(),
        "index_not_found_exception" => "org.elasticsearch.index.IndexNotFoundException".into(),
        "json_parse_exception" => "com.fasterxml.jackson.core.JsonParseException".into(),
        "json_e_o_f_exception" => "com.fasterxml.jackson.core.io.JsonEOFException".into(),
        "x_content_parse_exception" => "org.elasticsearch.xcontent.XContentParseException".into(),
        "mapper_parsing_exception" => "org.elasticsearch.index.mapper.MapperParsingException".into(),
        "resource_already_exists_exception" => "org.elasticsearch.ResourceAlreadyExistsException".into(),
        "resource_not_found_exception" => "org.elasticsearch.ResourceNotFoundException".into(),
        other => {
            // snake_case → CamelCase under org.elasticsearch.
            let camel: String = other
                .split('_')
                .map(|p| {
                    let mut c = p.chars();
                    c.next().map(|f| f.to_ascii_uppercase().to_string() + c.as_str()).unwrap_or_default()
                })
                .collect();
            format!("org.elasticsearch.{camel}")
        }
    }
}

/// Converts a JSON parse failure into Elasticsearch's exception chain.
impl From<crate::json::JsonError> for EsError {
    fn from(e: crate::json::JsonError) -> EsError {
        let full = e.full_message();
        if e.eof {
            EsError::new(400, "x_content_e_o_f_exception", format!("[{}:{}] Unexpected end of file", e.line, e.column))
                .caused_by(EsError::java(400, "json_e_o_f_exception", full))
        } else {
            EsError::new(400, "x_content_parse_exception", format!("[{}:{}] {full}", e.line, e.column))
                .caused_by(EsError::java(400, "json_parse_exception", full))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_not_found_body() {
        let e = EsError::index_not_found("nope");
        assert_eq!(
            e.to_body(false).to_json(),
            r#"{"error":{"root_cause":[{"type":"index_not_found_exception","reason":"no such index [nope]","resource.type":"index_or_alias","resource.id":"nope","index_uuid":"_na_","index":"nope"}],"type":"index_not_found_exception","reason":"no such index [nope]","resource.type":"index_or_alias","resource.id":"nope","index_uuid":"_na_","index":"nope"},"status":404}"#
        );
    }

    #[test]
    fn root_cause_stops_at_java_exceptions() {
        let e = EsError::mapper_parsing("Failed to parse mapping: x").caused_by(EsError::illegal_argument("bad"));
        let body = e.to_body(false);
        assert_eq!(body.pointer(&["error", "root_cause"]).unwrap().to_json(), r#"[{"type":"mapper_parsing_exception","reason":"Failed to parse mapping: x"}]"#);
        let e = EsError::illegal_argument("x").caused_by(EsError::mapper_parsing("y"));
        assert_eq!(e.to_body(false).pointer(&["error", "root_cause"]).unwrap().to_json(), r#"[{"type":"illegal_argument_exception","reason":"x"}]"#);
        let e = EsError::mapper_parsing("a").caused_by(EsError::mapper_parsing("b").caused_by(EsError::illegal_argument("c")));
        assert_eq!(e.to_body(false).pointer(&["error", "root_cause"]).unwrap().to_json(), r#"[{"type":"mapper_parsing_exception","reason":"b"}]"#);
    }

    #[test]
    fn json_errors_map_to_x_content() {
        let e: EsError = crate::json::parse(b"{bad").unwrap_err().into();
        assert_eq!(e.kind, "x_content_parse_exception");
        assert!(e.reason().starts_with("[1:2] Unexpected character ('b' (code 98))"));
        assert_eq!(e.caused_by.as_ref().unwrap().kind, "json_parse_exception");
        let e: EsError = crate::json::parse(b"{").unwrap_err().into();
        assert_eq!(e.reason(), "[1:2] Unexpected end of file");
    }

    #[test]
    fn validation_text() {
        let e = EsError::action_request_validation(&["index is missing".into(), "type is missing".into()]);
        assert_eq!(e.reason(), "Validation Failed: 1: index is missing;2: type is missing;");
    }
}

#[cfg(test)]
mod size_tests {
    #[test]
    fn errors_stay_small() {
        assert!(std::mem::size_of::<super::EsError>() <= 80, "{}", std::mem::size_of::<super::EsError>());
    }
}
