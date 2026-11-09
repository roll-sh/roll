//! JSON input: the FROM path's rows, and a streaming reader for JSON DOCUMENT input.
//!
//! A JSON DOCUMENT object is a stream of top-level values (`S3Object[*]`). The reader walks the
//! FROM path through the stream without building the documents: containers on the path are
//! descended into, values off it are skipped, and only the values the path selects (the rows)
//! are buffered and parsed, each at most 1 MiB (OverMaxRecordSize). So `S3Object[*].root[*]`
//! over a `{"root": [...]}` document of any size runs in bounded memory.
//!
//! Path semantics (AWS's): a name or index that doesn't match, and a wildcard over an empty or
//! mismatched container, yield one MISSING row ("wildcards always emit at least one record").

use crate::ast::FromStep;
use crate::csv::{BOM, Flow};
use crate::error::{Result, SelectError};
use crate::eval::MAX_RECORD_BYTES;
use crate::value::{MAX_JSON_DEPTH, Value, parse_json, parse_json_string};

/// The rows a FROM path selects from one document (used for JSON LINES, whose records are
/// already bounded).
pub(crate) fn from_path(v: Value, steps: &[FromStep], out: &mut Vec<Value>) {
    let Some((step, rest)) = steps.split_first() else {
        out.push(v);
        return;
    };
    match (step, v) {
        (FromStep::Name(n), Value::Struct(fields)) => match fields.into_iter().find(|(k, _)| n.matches(k)) {
            Some((_, child)) => from_path(child, rest, out),
            None => out.push(Value::Missing),
        },
        (FromStep::Index(i), Value::List(items)) => match items.into_iter().nth(*i) {
            Some(child) => from_path(child, rest, out),
            None => out.push(Value::Missing),
        },
        (FromStep::Wildcard, Value::List(items)) if !items.is_empty() => {
            for child in items {
                from_path(child, rest, out);
            }
        }
        (FromStep::Fields, Value::Struct(fields)) if !fields.is_empty() => {
            for (_, child) in fields {
                from_path(child, rest, out);
            }
        }
        _ => out.push(Value::Missing),
    }
}

/// Byte-level scanning of one value (to skip it, or to find where a captured one ends).
#[derive(Clone, Copy, Debug)]
struct Scan {
    depth: usize,
    in_str: bool,
    esc: bool,
    /// A number or literal: it ends at the first byte that can't be part of one.
    bare: bool,
}

impl Scan {
    fn start(b: u8) -> Scan {
        match b {
            b'{' | b'[' => Scan { depth: 1, in_str: false, esc: false, bare: false },
            b'"' => Scan { depth: 0, in_str: true, esc: false, bare: false },
            _ => Scan { depth: 0, in_str: false, esc: false, bare: true },
        }
    }

    /// `None`: the value goes on (`b` is part of it). `Some(true)`: it ended with `b`.
    /// `Some(false)`: it ended before `b`.
    fn step(&mut self, b: u8) -> Result<Option<bool>> {
        if self.bare {
            return Ok(if b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.') { None } else { Some(false) });
        }
        if self.in_str {
            if self.esc {
                self.esc = false;
            } else if b == b'\\' {
                self.esc = true;
            } else if b == b'"' {
                self.in_str = false;
                if self.depth == 0 {
                    return Ok(Some(true));
                }
            }
            return Ok(None);
        }
        match b {
            b'"' => self.in_str = true,
            b'{' | b'[' => {
                self.depth += 1;
                if self.depth > MAX_SKIP_DEPTH {
                    return Err(SelectError::json("nesting too deep"));
                }
            }
            b'}' | b']' => {
                self.depth -= 1;
                if self.depth == 0 {
                    return Ok(Some(true));
                }
            }
            _ => {}
        }
        Ok(None)
    }
}

/// Nesting limit for skipped values (only a counter is kept for them).
const MAX_SKIP_DEPTH: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FState {
    /// After `{` or `[`.
    Open,
    /// After a key: expect `:`.
    Colon,
    /// After `:`: expect a value.
    Value,
    /// After a value: expect `,` or the close.
    Next,
    /// After `,`.
    Comma,
}

/// A container on the FROM path that is being walked.
#[derive(Debug)]
struct Frame {
    obj: bool,
    /// Path steps applied to reach this container; `steps[level]` selects its children.
    level: usize,
    state: FState,
    /// The next array index.
    index: usize,
    /// Some child was selected (so no MISSING row is due when it closes).
    matched: bool,
    /// The value after the current key is selected.
    take: bool,
}

#[derive(Clone, Copy, Debug)]
enum Mode {
    Top,
    Frame,
    Key { esc: bool },
    Capture(Scan),
    Skip(Scan),
}

/// The streaming JSON DOCUMENT reader.
pub(crate) struct DocReader {
    steps: Vec<FromStep>,
    frames: Vec<Frame>,
    mode: Mode,
    cap: Vec<u8>,
    key: Vec<u8>,
    pos: u64,
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

fn is_value_start(b: u8) -> bool {
    matches!(b, b'{' | b'[' | b'"' | b'-' | b'0'..=b'9' | b't' | b'f' | b'n')
}

impl DocReader {
    pub fn new(steps: Vec<FromStep>) -> DocReader {
        DocReader { steps, frames: Vec::new(), mode: Mode::Top, cap: Vec::new(), key: Vec::new(), pos: 0 }
    }

    fn err(&self, what: &str) -> SelectError {
        SelectError::json(format_args!("{what} at byte {}", self.pos))
    }

    /// A value starts with `b` (consumed) at `level` steps down the path.
    fn start_value(&mut self, b: u8, level: usize, emit: &mut dyn FnMut(Value) -> Result<Flow>) -> Result<Flow> {
        if !is_value_start(b) {
            return Err(self.err("expected a value"));
        }
        if level == self.steps.len() {
            self.cap.clear();
            self.cap.push(b);
            self.mode = Mode::Capture(Scan::start(b));
            return Ok(Flow::Continue);
        }
        let obj = match (&self.steps[level], b) {
            (FromStep::Name(_) | FromStep::Fields, b'{') => true,
            (FromStep::Index(_) | FromStep::Wildcard, b'[') => false,
            _ => {
                self.mode = Mode::Skip(Scan::start(b));
                return emit(Value::Missing);
            }
        };
        if self.frames.len() >= MAX_JSON_DEPTH {
            return Err(self.err("nesting too deep"));
        }
        self.frames.push(Frame { obj, level, state: FState::Open, index: 0, matched: false, take: false });
        self.mode = Mode::Frame;
        Ok(Flow::Continue)
    }

    /// The current value is complete: back to its container.
    fn value_done(&mut self) {
        match self.frames.last_mut() {
            None => self.mode = Mode::Top,
            Some(f) => {
                f.state = FState::Next;
                self.mode = Mode::Frame;
            }
        }
    }

    fn close_frame(&mut self, emit: &mut dyn FnMut(Value) -> Result<Flow>) -> Result<Flow> {
        let f = self.frames.pop().ok_or_else(|| SelectError::internal("JSON frame underflow"))?;
        self.value_done();
        if f.matched { Ok(Flow::Continue) } else { emit(Value::Missing) }
    }

    /// One byte. Returns whether it was consumed, and the consumer's flow.
    fn step(&mut self, b: u8, emit: &mut dyn FnMut(Value) -> Result<Flow>) -> Result<(bool, Flow)> {
        match self.mode {
            Mode::Top => {
                if is_ws(b) || (self.pos < 3 && BOM.get(self.pos as usize) == Some(&b)) {
                    return Ok((true, Flow::Continue));
                }
                Ok((true, self.start_value(b, 0, emit)?))
            }
            Mode::Capture(mut s) => {
                let r = s.step(b)?;
                if r != Some(false) {
                    if self.cap.len() >= MAX_RECORD_BYTES {
                        return Err(SelectError::over_max_record());
                    }
                    self.cap.push(b);
                }
                match r {
                    None => {
                        self.mode = Mode::Capture(s);
                        Ok((true, Flow::Continue))
                    }
                    Some(consumed) => {
                        let v = parse_json(&self.cap)?;
                        self.value_done();
                        Ok((consumed, emit(v)?))
                    }
                }
            }
            Mode::Skip(mut s) => match s.step(b)? {
                None => {
                    self.mode = Mode::Skip(s);
                    Ok((true, Flow::Continue))
                }
                Some(consumed) => {
                    self.value_done();
                    Ok((consumed, Flow::Continue))
                }
            },
            Mode::Key { esc } => {
                if self.key.len() >= MAX_RECORD_BYTES {
                    return Err(SelectError::over_max_record());
                }
                self.key.push(b);
                if esc {
                    self.mode = Mode::Key { esc: false };
                } else if b == b'\\' {
                    self.mode = Mode::Key { esc: true };
                } else if b == b'"' {
                    let key = parse_json_string(&self.key)?;
                    let f = self.frames.last_mut().ok_or_else(|| SelectError::internal("JSON key outside an object"))?;
                    f.take = match &self.steps[f.level] {
                        FromStep::Name(n) => !f.matched && n.matches(&key),
                        _ => true,
                    };
                    f.matched |= f.take;
                    f.state = FState::Colon;
                    self.mode = Mode::Frame;
                }
                Ok((true, Flow::Continue))
            }
            Mode::Frame => {
                if is_ws(b) {
                    return Ok((true, Flow::Continue));
                }
                let (obj, state) = {
                    let f = self.frames.last().ok_or_else(|| SelectError::internal("JSON frame missing"))?;
                    (f.obj, f.state)
                };
                let close = if obj { b'}' } else { b']' };
                match (obj, state) {
                    (_, FState::Open | FState::Next) if b == close => Ok((true, self.close_frame(emit)?)),
                    (_, FState::Next) if b == b',' => {
                        if let Some(f) = self.frames.last_mut() {
                            f.state = FState::Comma;
                        }
                        Ok((true, Flow::Continue))
                    }
                    (true, FState::Open | FState::Comma) if b == b'"' => {
                        self.key.clear();
                        self.key.push(b);
                        self.mode = Mode::Key { esc: false };
                        Ok((true, Flow::Continue))
                    }
                    (true, FState::Colon) if b == b':' => {
                        if let Some(f) = self.frames.last_mut() {
                            f.state = FState::Value;
                        }
                        Ok((true, Flow::Continue))
                    }
                    (true, FState::Value) => {
                        let (take, level) = self.frames.last().map(|f| (f.take, f.level)).unwrap_or_default();
                        if take {
                            Ok((true, self.start_value(b, level + 1, emit)?))
                        } else if is_value_start(b) {
                            self.mode = Mode::Skip(Scan::start(b));
                            Ok((true, Flow::Continue))
                        } else {
                            Err(self.err("expected a value"))
                        }
                    }
                    (false, FState::Open | FState::Comma) => {
                        let f = self.frames.last_mut().ok_or_else(|| SelectError::internal("JSON frame missing"))?;
                        let take = match &self.steps[f.level] {
                            FromStep::Index(n) => f.index == *n,
                            _ => true,
                        };
                        f.index += 1;
                        f.matched |= take;
                        let level = f.level;
                        if take {
                            Ok((true, self.start_value(b, level + 1, emit)?))
                        } else if is_value_start(b) {
                            self.mode = Mode::Skip(Scan::start(b));
                            Ok((true, Flow::Continue))
                        } else {
                            Err(self.err("expected a value"))
                        }
                    }
                    _ => Err(self.err(&format!("unexpected {:?}", b as char))),
                }
            }
        }
    }

    /// Feed bytes; `emit` gets each row. Returns how much was consumed (all unless `emit` paused
    /// or finished).
    pub fn feed(&mut self, data: &[u8], emit: &mut dyn FnMut(Value) -> Result<Flow>) -> Result<(usize, Flow)> {
        let mut i = 0;
        while i < data.len() {
            let (consumed, flow) = self.step(data[i], emit)?;
            if consumed {
                i += 1;
                self.pos += 1;
            }
            if flow != Flow::Continue {
                return Ok((i, flow));
            }
        }
        Ok((data.len(), Flow::Continue))
    }

    /// The input ended: a trailing number or literal ends here; anything else open is an error.
    pub fn finish(&mut self, emit: &mut dyn FnMut(Value) -> Result<Flow>) -> Result<Flow> {
        match self.mode {
            Mode::Top => Ok(Flow::Continue),
            Mode::Capture(s) if s.bare => {
                let v = parse_json(&self.cap)?;
                self.value_done();
                if !matches!(self.mode, Mode::Top) {
                    return Err(self.err("unexpected end of the JSON document"));
                }
                emit(v)
            }
            Mode::Skip(s) if s.bare && self.frames.is_empty() => Ok(Flow::Continue),
            _ => Err(self.err("unexpected end of the JSON document")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::write_json;

    fn steps(path: &str) -> Vec<FromStep> {
        crate::parser::parse(&format!("select * from s3object{path}")).unwrap().from.steps
    }

    fn text(v: &Value) -> String {
        match v {
            Value::Missing => "MISSING".into(),
            v => {
                let mut s = String::new();
                write_json(&mut s, v);
                s
            }
        }
    }

    fn stream(doc: &str, path: &str, chunk: usize) -> Result<Vec<String>> {
        let mut r = DocReader::new(steps(path));
        let mut out = Vec::new();
        let mut emit = |v: Value| {
            out.push(text(&v));
            Ok(Flow::Continue)
        };
        for c in doc.as_bytes().chunks(chunk.max(1)) {
            r.feed(c, &mut emit)?;
        }
        r.finish(&mut emit)?;
        Ok(out)
    }

    fn materialized(doc: &str, path: &str) -> Vec<String> {
        // Documents are whitespace-separated here, one per line.
        let mut out = Vec::new();
        for line in doc.lines().filter(|l| !l.trim().is_empty()) {
            let mut rows = Vec::new();
            from_path(parse_json(line.as_bytes()).unwrap(), &steps(path), &mut rows);
            out.extend(rows.iter().map(text));
        }
        out
    }

    const DOC: &str = r#"{ "Rules": [ {"id": "1"}, {"expr": "y > x"}, {"id": "2", "expr": "z = DEBUG"} ]}
{ "created": "June 27", "modified": "July 6" }
{"root": [{"c1": 1, "c2": [1, {"x": "]}\"["}]}, {"c1": 2}], "Rules": []}
[1, 2, {"id": 3}]
"text"
42
true
{"a": {"b": {"c": [10, 20, 30]}}, "A": 1}
"#;

    #[test]
    fn streaming_matches_materialized() {
        for path in [
            "", "[*]", "[*].Rules", "[*].Rules[*]", "[*].Rules[*].id", "[*].root[*].c1", "[*].root[*].c2[1].x", "[*][1]",
            "[*].a.b.c[*]", "[*].a.b.c[2]", "[*].*", "[*].a.*", "[*].\"A\"", "[*].a", "[*][*]", "[*].nope[*].x",
        ] {
            let want = materialized(DOC, path);
            for chunk in [1, 2, 3, 7, 64, DOC.len()] {
                assert_eq!(stream(DOC, path, chunk).unwrap(), want, "path {path:?}, chunk {chunk}");
            }
        }
        // AWS's documented example.
        assert_eq!(
            stream(DOC, "[*].Rules[*].id", 5).unwrap()[..5],
            ["\"1\"", "MISSING", "\"2\"", "MISSING", "MISSING"]
        );
    }

    #[test]
    fn concatenated_and_bare_documents() {
        assert_eq!(stream("1 2\n3", "", 1).unwrap(), ["1", "2", "3"]);
        assert_eq!(stream("{\"a\":1}{\"a\":2}", "[*].a", 1).unwrap(), ["1", "2"]);
        assert_eq!(stream("\u{feff}{\"a\":1}", "", 1).unwrap(), ["{\"a\":1}"]);
        assert_eq!(stream("", "", 1).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn errors() {
        assert_eq!(stream("{\"a\":1", "", 3).unwrap_err().code, "JSONParsingError");
        assert_eq!(stream("{\"a\" 1}", "[*].a", 3).unwrap_err().code, "JSONParsingError");
        assert_eq!(stream("{\"a\":1,}", "", 3).unwrap_err().code, "JSONParsingError");
        assert_eq!(stream("]", "", 1).unwrap_err().code, "JSONParsingError");
        assert_eq!(stream("{\"a\":[1,2", "[*].a[*]", 1).unwrap_err().code, "JSONParsingError");
        let big = format!("{{\"a\":\"{}\"}}", "x".repeat(MAX_RECORD_BYTES));
        assert_eq!(stream(&big, "", 4096).unwrap_err().code, "OverMaxRecordSize");
        // The same value skipped (off the path) is fine.
        assert_eq!(stream(&big, "[*].b", 4096).unwrap(), ["MISSING"]);
    }

    #[test]
    fn huge_array_streams_in_bounded_memory() {
        let mut r = DocReader::new(steps("[*].root[*]"));
        let mut n = 0u64;
        let mut emit = |_: Value| {
            n += 1;
            Ok(Flow::Continue)
        };
        r.feed(b"{\"root\": [", &mut emit).unwrap();
        let chunk = "{\"c1\": 12345, \"c2\": \"abcdefgh\"},".repeat(1000);
        for _ in 0..200 {
            r.feed(chunk.as_bytes(), &mut emit).unwrap();
            assert!(r.cap.capacity() < 4096 && r.key.capacity() < 4096);
        }
        r.feed(b"{}]}", &mut emit).unwrap();
        r.finish(&mut emit).unwrap();
        assert_eq!(n, 200_001);
    }

    #[test]
    fn case_rules_in_from_paths() {
        assert_eq!(stream("{\"ROOT\": 1}", "[*].root", 1).unwrap(), ["1"]);
        assert_eq!(stream("{\"ROOT\": 1}", "[*].\"root\"", 1).unwrap(), ["MISSING"]);
    }
}
