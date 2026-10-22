//! Errors, rendered the way Elasticsearch renders a Painless `ScriptException`.
//!
//! A compile error carries the byte offset of the offending node; a runtime error carries the
//! offset of the instruction that threw plus the statement boundaries of the script, which is
//! everything Elasticsearch's `convertToScriptException` needs: the script stack (Java frames
//! of the throwing library method, then a snippet and a `^---- HERE` pointer) and the position.
//! Offsets reported to clients are UTF-16 code units, as Java counts them.

use serde_json::{json, Map, Value as Json};

/// An error found while compiling: a type (`illegal_argument_exception`, …), a message and the
/// byte offset it points at.
#[derive(Clone, Debug, PartialEq)]
pub struct CompileError {
    pub offset: u32,
    pub kind: &'static str,
    pub message: String,
    /// The exception Elasticsearch reports as this error's own `caused_by` (ANTLR's
    /// `no_viable_alt_exception`, a regex `pattern_syntax_exception`).
    pub nested: Option<Json>,
}

impl CompileError {
    pub fn new(offset: u32, kind: &'static str, message: impl Into<String>) -> CompileError {
        CompileError { offset, kind, message: message.into(), nested: None }
    }
    pub fn illegal_argument(offset: u32, message: impl Into<String>) -> CompileError {
        CompileError::new(offset, "illegal_argument_exception", message)
    }
    pub fn class_cast(offset: u32, message: impl Into<String>) -> CompileError {
        CompileError::new(offset, "class_cast_exception", message)
    }
    pub fn illegal_state(offset: u32, message: impl Into<String>) -> CompileError {
        CompileError::new(offset, "illegal_state_exception", message)
    }
    /// Adds the nested cause (`type`, `reason`).
    pub fn caused_by(mut self, kind: &str, reason: Option<String>) -> CompileError {
        self.nested = Some(json!({ "type": kind, "reason": reason }));
        self
    }
    /// An error Elasticsearch raises without a source location (no `position`, empty stack).
    pub fn unlocated(kind: &'static str, message: impl Into<String>) -> CompileError {
        CompileError::new(NO_LOCATION, kind, message)
    }
}

/// Offset of a [`CompileError`] that has no location.
pub const NO_LOCATION: u32 = u32::MAX;

/// `ScriptException.Position`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Position {
    pub offset: u32,
    pub start: u32,
    pub end: u32,
}

/// What a script failure looks like to Elasticsearch clients: a `script_exception` wrapping a
/// cause.
#[derive(Clone, Debug, PartialEq)]
pub struct ScriptError {
    /// `"compile error"` or `"runtime error"` (or a reason of a non-script error such as the
    /// compilation circuit breaker).
    pub reason: String,
    pub script_stack: Vec<String>,
    /// The script source (compile errors) or name (runtime errors; the source for inline
    /// scripts).
    pub script: String,
    pub lang: &'static str,
    pub position: Option<Position>,
    /// Exception type of the cause, e.g. `illegal_argument_exception`.
    pub cause_type: String,
    pub cause_reason: Option<String>,
    /// When true the cause is reported as the root cause (Elasticsearch does that for errors it
    /// wraps in `ErrorCauseWrapper`: `painless_error`, `painless_explain_error`, stack overflows).
    pub cause_is_root: bool,
    /// Extra metadata rendered as top-level fields (`painless_class`, `to_string`,
    /// `java_class` for `Debug.explain`), in order.
    pub metadata: Vec<(String, String)>,
    /// HTTP status Elasticsearch answers with (400 for script errors).
    pub status: u16,
    /// Not a `script_exception` at all (size limit, compilation rate): rendered as the cause.
    pub plain: bool,
    /// Extra fields of the cause (`bytes_wanted`, `durability` of circuit breakers).
    pub cause_fields: Vec<(String, Json)>,
}

impl std::fmt::Display for ScriptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.reason, self.cause_type)?;
        if let Some(r) = &self.cause_reason {
            write!(f, ": {r}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ScriptError {}

impl ScriptError {
    /// The `caused_by` object.
    pub fn cause_json(&self) -> Json {
        let mut m = Map::new();
        m.insert("type".into(), json!(self.cause_type));
        m.insert("reason".into(), json!(self.cause_reason));
        for (k, v) in &self.cause_fields {
            m.insert(k.clone(), v.clone());
        }
        Json::Object(m)
    }

    fn body(&self, with_cause: bool) -> Map<String, Json> {
        let mut m = Map::new();
        if self.plain {
            m.insert("type".into(), json!(self.cause_type));
            m.insert("reason".into(), json!(self.cause_reason));
            return m;
        }
        m.insert("type".into(), json!("script_exception"));
        m.insert("reason".into(), json!(self.reason));
        for (k, v) in &self.metadata {
            m.insert(k.clone(), json!(v));
        }
        m.insert("script_stack".into(), json!(self.script_stack));
        m.insert("script".into(), json!(self.script));
        m.insert("lang".into(), json!(self.lang));
        if let Some(p) = self.position {
            m.insert("position".into(), json!({ "offset": p.offset, "start": p.start, "end": p.end }));
        }
        if with_cause {
            m.insert("caused_by".into(), self.cause_json());
        }
        m
    }

    /// The exception object (`type`, `reason`, `script_stack`, …, `caused_by`).
    pub fn to_json(&self) -> Json {
        Json::Object(self.body(true))
    }

    /// The `root_cause` entries Elasticsearch reports for this error.
    pub fn root_causes(&self) -> Vec<Json> {
        if self.plain {
            return vec![Json::Object(self.body(false))];
        }
        if self.cause_is_root {
            let mut c = self.cause_json();
            if let Json::Object(o) = &mut c {
                o.remove("caused_by");
            }
            vec![c]
        } else {
            vec![Json::Object(self.body(false))]
        }
    }

    /// The complete error response body: `{"error": {...}, "status": 400}`.
    pub fn to_response(&self) -> Json {
        let mut err = Map::new();
        err.insert("root_cause".into(), Json::Array(self.root_causes()));
        for (k, v) in self.body(true) {
            err.insert(k, v);
        }
        json!({ "error": Json::Object(err), "status": self.status })
    }
}

/// UTF-16 offset of a byte offset.
pub fn utf16_offset(src: &str, byte: usize) -> u32 {
    let byte = byte.min(src.len());
    if src.is_ascii() {
        return byte as u32;
    }
    src[..floor_boundary(src, byte)].encode_utf16().count() as u32
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Substring by UTF-16 offsets, as Java's `String.substring`.
pub fn utf16_substring(src: &str, start: u32, end: u32) -> String {
    if src.is_ascii() {
        let s = (start as usize).min(src.len());
        let e = (end as usize).min(src.len()).max(s);
        return src[s..e].to_string();
    }
    let units: Vec<u16> = src.encode_utf16().collect();
    let s = (start as usize).min(units.len());
    let e = (end as usize).min(units.len()).max(s);
    String::from_utf16_lossy(&units[s..e])
}

pub fn utf16_len(src: &str) -> u32 {
    if src.is_ascii() {
        src.len() as u32
    } else {
        src.encode_utf16().count() as u32
    }
}

/// Builds the `ScriptException` for a compile error (`PainlessScriptEngine.convertToScriptException`):
/// the snippet spans 25 characters either side of the error.
pub fn compile_error(src: &str, e: &CompileError) -> ScriptError {
    if e.offset == NO_LOCATION {
        return ScriptError {
            reason: "compile error".into(),
            script_stack: Vec::new(),
            script: src.to_string(),
            lang: "painless",
            position: None,
            cause_type: e.kind.to_string(),
            cause_reason: if e.message.is_empty() { None } else { Some(e.message.clone()) },
            // Elasticsearch wraps errors (not exceptions) in `ErrorCauseWrapper`
            cause_is_root: e.kind.ends_with("_error"),
            metadata: Vec::new(),
            status: 400,
            plain: false,
            cause_fields: e.nested.iter().map(|n| ("caused_by".to_string(), n.clone())).collect(),
        };
    }
    let len = utf16_len(src);
    let offset = utf16_offset(src, e.offset as usize);
    let start = offset.saturating_sub(25);
    let end = (offset + 25).min(len);
    let mut snippet = String::new();
    if start > 0 {
        snippet.push_str("... ");
    }
    snippet.push_str(&utf16_substring(src, start, end));
    if end < len {
        snippet.push_str(" ...");
    }
    let mut pointer = String::new();
    if start > 0 {
        pointer.push_str("    ");
    }
    for _ in start..offset {
        pointer.push(' ');
    }
    pointer.push_str("^---- HERE");
    ScriptError {
        reason: "compile error".into(),
        script_stack: vec![snippet, pointer],
        script: src.to_string(),
        lang: "painless",
        position: Some(Position { offset, start, end }),
        cause_type: e.kind.to_string(),
        cause_reason: Some(e.message.clone()),
        cause_is_root: false,
        metadata: Vec::new(),
        status: 400,
        plain: false,
        cause_fields: e.nested.iter().map(|n| ("caused_by".to_string(), n.clone())).collect(),
    }
}

/// Builds the `ScriptException` for a runtime failure (`PainlessScript.convertToScriptException`):
/// Java frames of the throwing library code first, then the statement that failed and a pointer
/// at the failing expression. `statements` are the sorted UTF-16 offsets of statement starts.
#[allow(clippy::too_many_arguments)]
pub fn runtime_error(
    src: &str,
    name: &str,
    statements: &[u32],
    offset_byte: Option<u32>,
    java_frames: &[String],
    cause_type: &str,
    cause_reason: Option<String>,
    cause_is_root: bool,
) -> ScriptError {
    let mut stack: Vec<String> = java_frames.to_vec();
    let mut position = None;
    match offset_byte {
        Some(ob) => {
            let offset = utf16_offset(src, ob as usize);
            let start = statements.iter().rev().find(|&&s| s <= offset).copied().unwrap_or(0);
            let end = statements.iter().find(|&&s| s > start).copied().unwrap_or_else(|| utf16_len(src));
            stack.push(utf16_substring(src, start, end));
            let mut pointer = String::new();
            for _ in start..offset {
                pointer.push(' ');
            }
            pointer.push_str("^---- HERE");
            stack.push(pointer);
            position = Some(Position { offset, start, end });
        }
        None => stack.push("<<< unknown portion of script >>>".into()),
    }
    ScriptError {
        reason: "runtime error".into(),
        script_stack: stack,
        script: name.to_string(),
        lang: "painless",
        position,
        cause_type: cause_type.to_string(),
        cause_reason,
        cause_is_root,
        metadata: Vec::new(),
        status: 400,
        plain: false,
        cause_fields: Vec::new(),
    }
}

/// Elasticsearch's snake-case exception name for a Java exception class simple name
/// (`ElasticsearchException.getExceptionName`).
pub fn exception_name(simple: &str) -> String {
    let mut out = String::with_capacity(simple.len() + 8);
    for (i, c) in simple.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compile_error_snippet() {
        let src = "for (int i = 0; i < 3; i++";
        let e = CompileError::illegal_argument(26, "x");
        let se = compile_error(src, &e);
        assert_eq!(se.script_stack, vec!["... or (int i = 0; i < 3; i++".to_string(), "                             ^---- HERE".to_string()]);
        assert_eq!(se.position, Some(Position { offset: 26, start: 1, end: 26 }));
    }

    #[test]
    fn runtime_error_snippet() {
        let src = "List l = new ArrayList(); l.get(3)";
        let se = runtime_error(src, src, &[5, 26], Some(27), &[], "index_out_of_bounds_exception", Some("m".into()), false);
        assert_eq!(se.script_stack, vec!["l.get(3)".to_string(), " ^---- HERE".to_string()]);
        assert_eq!(se.position, Some(Position { offset: 27, start: 26, end: 34 }));
    }

    #[test]
    fn exception_names() {
        assert_eq!(exception_name("IllegalArgumentException"), "illegal_argument_exception");
        assert_eq!(exception_name("PainlessError"), "painless_error");
    }
}
