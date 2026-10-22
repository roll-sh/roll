//! A lenient YAML parser for test files.
//!
//! Elasticsearch reads the suites with SnakeYAML, which is more forgiving than the YAML spec and
//! than the strict Rust parsers: a flow collection or a quoted scalar may continue on lines that
//! are indented less than its parent (`- match: {` followed by keys at column 4, a long quoted
//! reason wrapped to column 2), and `{_ignored:["x"]}` is a mapping. The suites rely on that,
//! so this parser implements the subset of YAML they use with the same leniency:
//!
//! - block mappings and sequences (including compact `- key: value` entries and sequences at the
//!   indentation of their parent key);
//! - flow mappings and sequences, spanning lines freely, with comments;
//! - plain (multi-line), single- and double-quoted (all escapes, line folding) and block (`|`,
//!   `>` with chomping and indentation indicators) scalars;
//! - anchors, aliases and tags; documents separated by `---` / `...`.
//!
//! Inside flow collections indentation is ignored entirely; in block context it decides structure
//! exactly as YAML says. Every node records the line it starts on.

use std::collections::HashMap;

use super::{Node, NodeKind, Style, YamlError};

type Result<T> = std::result::Result<T, YamlError>;

pub(super) struct Parser<'a> {
    b: &'a [u8],
    src: &'a str,
    pos: usize,
    line: usize,
    line_start: usize,
    anchors: HashMap<String, Node>,
    /// Current collection nesting; bounded so hostile input can't overflow the stack.
    depth: usize,
}

/// Deeper nesting than any test file uses (Jackson stops at 1000; our frames are bigger).
const MAX_DEPTH: usize = 256;

fn is_ws(c: u8) -> bool {
    c == b' ' || c == b'\t'
}

fn is_ws_or_eol(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0)
}

fn is_flow_indicator(c: u8) -> bool {
    matches!(c, b',' | b'[' | b']' | b'{' | b'}')
}

/// Node properties: `&anchor` and `!tag`.
#[derive(Default)]
struct Props {
    anchor: Option<String>,
    tag: Option<String>,
}

impl Props {
    fn is_empty(&self) -> bool {
        self.anchor.is_none() && self.tag.is_none()
    }
}

impl<'a> Parser<'a> {
    pub(super) fn new(src: &'a str) -> Parser<'a> {
        Parser { b: src.as_bytes(), src, pos: 0, line: 1, line_start: 0, anchors: HashMap::new(), depth: 0 }
    }

    fn enter(&mut self) -> Result<()> {
        if self.depth >= MAX_DEPTH {
            return self.err(format!("nesting deeper than {MAX_DEPTH} levels"));
        }
        self.depth += 1;
        Ok(())
    }

    fn err<T>(&self, msg: impl Into<String>) -> Result<T> {
        Err(YamlError { line: self.line, message: msg.into() })
    }

    fn peek(&self) -> u8 {
        self.b.get(self.pos).copied().unwrap_or(0)
    }

    fn peek_at(&self, off: usize) -> u8 {
        self.b.get(self.pos + off).copied().unwrap_or(0)
    }

    fn eof(&self) -> bool {
        self.pos >= self.b.len()
    }

    fn col(&self) -> usize {
        self.pos - self.line_start
    }

    fn bump(&mut self) {
        if let Some(&c) = self.b.get(self.pos) {
            self.pos += 1;
            if c == b'\n' {
                self.line += 1;
                self.line_start = self.pos;
            }
        }
    }

    /// Advances over one UTF-8 character and returns it.
    fn bump_char(&mut self) -> char {
        let c = self.src[self.pos..].chars().next().unwrap_or('\0');
        if c == '\n' {
            self.bump();
        } else {
            self.pos += c.len_utf8();
        }
        c
    }

    fn save(&self) -> (usize, usize, usize) {
        (self.pos, self.line, self.line_start)
    }

    fn restore(&mut self, s: (usize, usize, usize)) {
        (self.pos, self.line, self.line_start) = s;
    }

    fn skip_spaces(&mut self) {
        while is_ws(self.peek()) {
            self.pos += 1;
        }
    }

    fn at_eol(&self) -> bool {
        matches!(self.peek(), b'\n' | 0)
    }

    /// At a comment start (`#` at line start or after whitespace) or the end of the line.
    fn at_comment_or_eol(&self) -> bool {
        self.at_eol() || (self.peek() == b'#' && (self.pos == self.line_start || is_ws(self.b[self.pos - 1])))
    }

    fn skip_to_eol(&mut self) {
        while !self.at_eol() {
            self.pos += 1;
        }
    }

    /// After a complete inline value: only spaces and a comment may follow on the line.
    fn end_of_line(&mut self) -> Result<()> {
        self.skip_spaces();
        if self.at_comment_or_eol() {
            self.skip_to_eol();
            Ok(())
        } else {
            let rest: String = self.src[self.pos..].chars().take_while(|c| *c != '\n').take(40).collect();
            self.err(format!("unexpected content after a value: [{rest}]"))
        }
    }

    fn at_doc_marker(&self) -> bool {
        self.pos == self.line_start
            && (self.b[self.pos..].starts_with(b"---") || self.b[self.pos..].starts_with(b"..."))
            && is_ws_or_eol(self.peek_at(3))
    }

    /// Moves to the first non-space character of the next line with content (skipping blank and
    /// comment-only lines) and returns its column; `None` at the end of input or a document marker
    /// (left at the marker's line start).
    fn skip_blank_lines(&mut self) -> Option<usize> {
        loop {
            if self.eof() {
                return None;
            }
            if self.pos == self.line_start && self.at_doc_marker() {
                return None;
            }
            self.skip_spaces();
            match self.peek() {
                0 => return None,
                b'\n' => self.bump(),
                b'#' if self.pos == self.line_start || is_ws(self.b[self.pos - 1]) => {
                    self.skip_to_eol();
                }
                _ => return Some(self.col()),
            }
        }
    }

    pub(super) fn documents(&mut self) -> Result<Vec<Node>> {
        let mut docs = Vec::new();
        loop {
            match self.skip_blank_lines() {
                None => {
                    if self.eof() {
                        break;
                    }
                    let is_start = self.b[self.pos..].starts_with(b"---");
                    self.pos += 3;
                    self.skip_spaces();
                    if is_start && !self.at_comment_or_eol() {
                        // Content on the marker line: `--- value`.
                        let node = self.node_at(-1)?;
                        docs.push(node);
                        self.expect_document_end()?;
                    } else {
                        self.skip_to_eol();
                    }
                }
                Some(_) => {
                    let node = self.node_at(-1)?;
                    docs.push(node);
                    self.expect_document_end()?;
                }
            }
        }
        Ok(docs)
    }

    fn expect_document_end(&mut self) -> Result<()> {
        if self.skip_blank_lines().is_some() {
            return self.err("expected the end of the document (bad indentation?)");
        }
        Ok(())
    }

    fn props(&mut self) -> Result<Props> {
        let mut p = Props::default();
        loop {
            match self.peek() {
                b'&' => {
                    self.pos += 1;
                    let start = self.pos;
                    while !is_ws_or_eol(self.peek()) && !is_flow_indicator(self.peek()) {
                        self.pos += 1;
                    }
                    if start == self.pos {
                        return self.err("empty anchor name");
                    }
                    p.anchor = Some(self.src[start..self.pos].to_owned());
                    self.skip_spaces();
                }
                b'!' => {
                    let start = self.pos;
                    if self.peek_at(1) == b'<' {
                        while !self.at_eol() && self.peek() != b'>' {
                            self.pos += 1;
                        }
                        if self.peek() != b'>' {
                            return self.err("unterminated verbatim tag");
                        }
                        self.pos += 1;
                    } else {
                        while !is_ws_or_eol(self.peek()) && !is_flow_indicator(self.peek()) {
                            self.pos += 1;
                        }
                    }
                    let raw = &self.src[start..self.pos];
                    p.tag = Some(
                        raw.strip_prefix("!!")
                            .or_else(|| raw.strip_prefix("!<tag:yaml.org,2002:").and_then(|r| r.strip_suffix('>')))
                            .map(str::to_owned)
                            .unwrap_or_else(|| raw.to_owned()),
                    );
                    self.skip_spaces();
                }
                _ => return Ok(p),
            }
        }
    }

    fn finish(&mut self, mut node: Node, props: Props) -> Node {
        if let Some(tag) = props.tag
            && let NodeKind::Scalar { tag: t, .. } = &mut node.kind
        {
            *t = Some(tag);
        }
        if let Some(anchor) = props.anchor {
            self.anchors.insert(anchor, node.clone());
        }
        node
    }

    fn null_node(&self, line: usize) -> Node {
        Node { line, kind: NodeKind::Scalar { text: String::new(), style: Style::Plain, tag: None } }
    }

    fn alias(&mut self) -> Result<Node> {
        let line = self.line;
        self.pos += 1;
        let start = self.pos;
        while !is_ws_or_eol(self.peek()) && !is_flow_indicator(self.peek()) {
            self.pos += 1;
        }
        let name = &self.src[start..self.pos];
        match self.anchors.get(name) {
            Some(n) => Ok(Node { line, kind: n.kind.clone() }),
            None => self.err(format!("unknown alias [{name}]")),
        }
    }

    /// A node starting at the current position (column `indent`) in block context; `parent` is
    /// the indentation of the enclosing block collection (-1 at the top).
    fn node_at(&mut self, parent: isize) -> Result<Node> {
        self.enter()?;
        let r = self.node_at_inner(parent);
        self.depth -= 1;
        r
    }

    fn node_at_inner(&mut self, parent: isize) -> Result<Node> {
        let line = self.line;
        let props = self.props()?;
        if !props.is_empty() && self.at_comment_or_eol() {
            // Properties alone on a line: the node follows on the next lines.
            self.skip_to_eol();
            let node = match self.skip_blank_lines() {
                Some(i) if i as isize > parent => self.node_at(parent)?,
                Some(i) if i as isize == parent && self.peek() == b'-' && is_ws_or_eol(self.peek_at(1)) => self.block_seq(i)?,
                _ => self.null_node(line),
            };
            return Ok(self.finish(node, props));
        }
        let c = self.peek();
        let node = if c == b'-' && is_ws_or_eol(self.peek_at(1)) {
            let col = self.col();
            self.block_seq(col)?
        } else if self.block_key_ahead() {
            let col = self.col();
            self.block_map(col)?
        } else {
            self.inline_value(parent)?
        };
        Ok(self.finish(node, props))
    }

    /// Whether the current line, from the current position, is `key: ...`.
    fn block_key_ahead(&self) -> bool {
        let b = self.b;
        let mut i = self.pos;
        match b.get(i) {
            Some(b'"') | Some(b'\'') => {
                let q = b[i];
                i += 1;
                loop {
                    match b.get(i) {
                        None | Some(b'\n') => return false,
                        Some(b'\\') if q == b'"' => i += 2,
                        Some(&c) if c == q => {
                            if q == b'\'' && b.get(i + 1) == Some(&b'\'') {
                                i += 2;
                                continue;
                            }
                            i += 1;
                            break;
                        }
                        _ => i += 1,
                    }
                }
                while matches!(b.get(i), Some(b' ' | b'\t')) {
                    i += 1;
                }
                b.get(i) == Some(&b':') && is_ws_or_eol(b.get(i + 1).copied().unwrap_or(0))
            }
            Some(b'[' | b'{' | b'|' | b'>' | b'*' | b'#') | None => false,
            _ => {
                while let Some(&c) = b.get(i) {
                    match c {
                        b'\n' => return false,
                        b':' if is_ws_or_eol(b.get(i + 1).copied().unwrap_or(0)) => return true,
                        b'#' if i > self.pos && is_ws(b[i - 1]) => return false,
                        _ => i += 1,
                    }
                }
                false
            }
        }
    }

    fn block_map(&mut self, indent: usize) -> Result<Node> {
        let line = self.line;
        let mut entries = Vec::new();
        let mut keys = KeySet::default();
        loop {
            let key = self.block_key()?;
            keys.add(&entries, &key)?;
            let value = self.block_value(indent)?;
            entries.push((key, value));
            match self.skip_blank_lines() {
                Some(i) if i == indent => {
                    if self.peek() == b'-' && is_ws_or_eol(self.peek_at(1)) {
                        return self.err("a sequence entry where a mapping key was expected");
                    }
                    if !self.block_key_ahead() {
                        return self.err("expected a mapping key (`key: value`)");
                    }
                }
                Some(i) if i > indent => return self.err("bad indentation of a mapping entry"),
                _ => break,
            }
        }
        Ok(Node { line, kind: NodeKind::Map(entries) })
    }

    /// Reads `key:` (plain or quoted) and the colon.
    fn block_key(&mut self) -> Result<Node> {
        let line = self.line;
        let props = self.props()?;
        let key = match self.peek() {
            b'"' => Node { line, kind: NodeKind::Scalar { text: self.double_quoted()?, style: Style::Quoted, tag: None } },
            b'\'' => Node { line, kind: NodeKind::Scalar { text: self.single_quoted()?, style: Style::Quoted, tag: None } },
            _ => {
                let start = self.pos;
                loop {
                    match self.peek() {
                        b':' if is_ws_or_eol(self.peek_at(1)) => break,
                        b'\n' | 0 => return self.err("expected `:` after a mapping key"),
                        _ => {
                            self.bump_char();
                        }
                    }
                }
                let text = self.src[start..self.pos].trim_end_matches([' ', '\t']).to_owned();
                Node { line, kind: NodeKind::Scalar { text, style: Style::Plain, tag: None } }
            }
        };
        self.skip_spaces();
        if self.peek() != b':' {
            return self.err("expected `:` after a mapping key");
        }
        self.pos += 1;
        Ok(self.finish(key, props))
    }

    /// The value after `key:` of a block mapping at `indent`.
    fn block_value(&mut self, indent: usize) -> Result<Node> {
        let line = self.line;
        self.skip_spaces();
        let props = self.props()?;
        let node = if self.at_comment_or_eol() {
            self.skip_to_eol();
            match self.skip_blank_lines() {
                Some(i) if i > indent => self.node_at(indent as isize)?,
                // A sequence may sit at its key's indentation.
                Some(i) if i == indent && self.peek() == b'-' && is_ws_or_eol(self.peek_at(1)) => self.block_seq(i)?,
                _ => self.null_node(line),
            }
        } else {
            self.inline_value(indent as isize)?
        };
        Ok(self.finish(node, props))
    }

    fn block_seq(&mut self, indent: usize) -> Result<Node> {
        let line = self.line;
        let mut items = Vec::new();
        loop {
            let item_line = self.line;
            self.pos += 1; // '-'
            self.skip_spaces();
            let item = if self.at_comment_or_eol() {
                self.skip_to_eol();
                match self.skip_blank_lines() {
                    Some(i) if i > indent => self.node_at(indent as isize)?,
                    _ => self.null_node(item_line),
                }
            } else {
                self.node_at(indent as isize)?
            };
            items.push(item);
            match self.skip_blank_lines() {
                Some(i) if i == indent && self.peek() == b'-' && is_ws_or_eol(self.peek_at(1)) => {}
                Some(i) if i > indent => return self.err("bad indentation of a sequence entry"),
                _ => break,
            }
        }
        Ok(Node { line, kind: NodeKind::Seq(items) })
    }

    /// A value that starts on the current line in block context: flow collection, quoted, block
    /// or plain scalar, or alias. `parent` is the indentation continuation lines must exceed.
    fn inline_value(&mut self, parent: isize) -> Result<Node> {
        let line = self.line;
        match self.peek() {
            b'[' | b'{' => {
                let node = self.flow_node()?;
                self.end_of_line()?;
                Ok(node)
            }
            b'"' => {
                let text = self.double_quoted()?;
                self.end_of_line()?;
                Ok(Node { line, kind: NodeKind::Scalar { text, style: Style::Quoted, tag: None } })
            }
            b'\'' => {
                let text = self.single_quoted()?;
                self.end_of_line()?;
                Ok(Node { line, kind: NodeKind::Scalar { text, style: Style::Quoted, tag: None } })
            }
            b'|' | b'>' => {
                let text = self.block_scalar(parent)?;
                Ok(Node { line, kind: NodeKind::Scalar { text, style: Style::Block, tag: None } })
            }
            b'*' => {
                let node = self.alias()?;
                self.end_of_line()?;
                Ok(node)
            }
            _ => {
                let text = self.plain_block(parent)?;
                Ok(Node { line, kind: NodeKind::Scalar { text, style: Style::Plain, tag: None } })
            }
        }
    }

    /// The rest of a plain scalar line (up to a comment or the end of the line), trimmed.
    fn plain_line(&mut self) -> &'a str {
        let start = self.pos;
        while !self.at_eol() {
            if self.peek() == b'#' && self.pos > start && is_ws(self.b[self.pos - 1]) {
                break;
            }
            self.pos += 1;
        }
        let src: &'a str = self.src;
        src[start..self.pos].trim_end_matches([' ', '\t'])
    }

    /// A plain scalar in block context, folding continuation lines indented more than `parent`.
    fn plain_block(&mut self, parent: isize) -> Result<String> {
        let mut text = self.plain_line().to_owned();
        let had_comment = self.peek() == b'#';
        self.skip_to_eol();
        if had_comment {
            return Ok(text);
        }
        loop {
            let saved = self.save();
            // Count blank lines, then look at the next content line.
            let mut blanks = 0;
            let mut cont = None;
            while !self.eof() {
                self.bump(); // the '\n'
                if self.at_doc_marker() {
                    break;
                }
                self.skip_spaces();
                match self.peek() {
                    b'\n' => blanks += 1,
                    0 | b'#' => break,
                    _ => {
                        cont = Some(self.col());
                        break;
                    }
                }
            }
            match cont {
                Some(i) if i as isize > parent => {
                    if blanks == 0 {
                        text.push(' ');
                    } else {
                        text.extend(std::iter::repeat_n('\n', blanks));
                    }
                    text.push_str(self.plain_line());
                    let had_comment = self.peek() == b'#';
                    self.skip_to_eol();
                    if had_comment {
                        return Ok(text);
                    }
                }
                _ => {
                    self.restore(saved);
                    return Ok(text);
                }
            }
        }
    }

    /// Folds a line break inside a quoted scalar: trailing spaces of the line are dropped, then
    /// one break becomes a space and `n` empty lines become `n` newlines.
    fn fold_quoted_break(&mut self, out: &mut String) {
        while out.ends_with([' ', '\t']) {
            out.pop();
        }
        self.bump(); // '\n'
        let mut blanks = 0;
        loop {
            self.skip_spaces();
            if self.peek() == b'\n' {
                blanks += 1;
                self.bump();
            } else {
                break;
            }
        }
        if blanks == 0 {
            out.push(' ');
        } else {
            out.extend(std::iter::repeat_n('\n', blanks));
        }
    }

    fn single_quoted(&mut self) -> Result<String> {
        let start_line = self.line;
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                0 => {
                    return Err(YamlError { line: start_line, message: "unterminated single-quoted scalar".into() });
                }
                b'\'' => {
                    if self.peek_at(1) == b'\'' {
                        out.push('\'');
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                        return Ok(out);
                    }
                }
                b'\n' => self.fold_quoted_break(&mut out),
                _ => out.push(self.bump_char()),
            }
        }
    }

    fn double_quoted(&mut self) -> Result<String> {
        let start_line = self.line;
        self.pos += 1;
        let mut out = String::new();
        loop {
            match self.peek() {
                0 => {
                    return Err(YamlError { line: start_line, message: "unterminated double-quoted scalar".into() });
                }
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\n' => self.fold_quoted_break(&mut out),
                b'\\' => {
                    self.pos += 1;
                    let e = self.peek();
                    if e == b'\n' {
                        // An escaped line break joins the lines without a space.
                        self.bump();
                        self.skip_spaces();
                        continue;
                    }
                    self.pos += 1;
                    let simple = match e {
                        b'0' => Some('\0'),
                        b'a' => Some('\x07'),
                        b'b' => Some('\x08'),
                        b't' | b'\t' => Some('\t'),
                        b'n' => Some('\n'),
                        b'v' => Some('\x0b'),
                        b'f' => Some('\x0c'),
                        b'r' => Some('\r'),
                        b'e' => Some('\x1b'),
                        b' ' => Some(' '),
                        b'"' => Some('"'),
                        b'/' => Some('/'),
                        b'\\' => Some('\\'),
                        b'N' => Some('\u{85}'),
                        b'_' => Some('\u{a0}'),
                        b'L' => Some('\u{2028}'),
                        b'P' => Some('\u{2029}'),
                        _ => None,
                    };
                    if let Some(c) = simple {
                        out.push(c);
                        continue;
                    }
                    let digits = match e {
                        b'x' => 2,
                        b'u' => 4,
                        b'U' => 8,
                        _ => return self.err(format!("unknown escape character [\\{}]", e as char)),
                    };
                    let hex = self.src.get(self.pos..self.pos + digits).unwrap_or("");
                    let Some(code) = u32::from_str_radix(hex, 16).ok().filter(|_| hex.len() == digits) else {
                        return self.err(format!("bad escape [\\{}{hex}]", e as char));
                    };
                    self.pos += digits;
                    match char::from_u32(code) {
                        Some(c) => out.push(c),
                        None => {
                            // A UTF-16 surrogate pair written as two \u escapes.
                            if (0xD800..0xDC00).contains(&code) && self.src[self.pos..].starts_with("\\u") {
                                let lo = self.src.get(self.pos + 2..self.pos + 6).and_then(|h| u32::from_str_radix(h, 16).ok());
                                if let Some(lo) = lo.filter(|l| (0xDC00..0xE000).contains(l)) {
                                    self.pos += 6;
                                    let c = 0x10000 + ((code - 0xD800) << 10) + (lo - 0xDC00);
                                    out.push(char::from_u32(c).unwrap_or('\u{fffd}'));
                                    continue;
                                }
                            }
                            out.push('\u{fffd}');
                        }
                    }
                }
                _ => out.push(self.bump_char()),
            }
        }
    }

    /// `|` or `>` block scalar; `parent` is the indentation of the enclosing collection.
    fn block_scalar(&mut self, parent: isize) -> Result<String> {
        let folded = self.peek() == b'>';
        self.pos += 1;
        let mut chomp = 0i8; // -1 strip, 0 clip, 1 keep
        let mut explicit: Option<usize> = None;
        for _ in 0..2 {
            match self.peek() {
                b'-' => {
                    chomp = -1;
                    self.pos += 1;
                }
                b'+' => {
                    chomp = 1;
                    self.pos += 1;
                }
                c @ b'1'..=b'9' => {
                    explicit = Some((c - b'0') as usize);
                    self.pos += 1;
                }
                _ => {}
            }
        }
        self.end_of_line()?;
        let mut content_indent = explicit.map(|d| parent.max(0) as usize + d);
        let mut lines: Vec<&str> = Vec::new();
        // Each entry: the line's text after the content indentation ("" for empty lines).
        loop {
            if self.eof() {
                break;
            }
            let saved = self.save();
            self.bump(); // '\n' ending the previous line
            if self.eof() {
                break;
            }
            if self.at_doc_marker() {
                self.restore(saved);
                break;
            }
            let ls = self.pos;
            let mut indent = 0;
            while self.peek() == b' ' {
                self.pos += 1;
                indent += 1;
            }
            let blank = self.at_eol();
            if content_indent.is_none() && !blank {
                if (indent as isize) <= parent {
                    self.restore(saved);
                    break;
                }
                content_indent = Some(indent);
            }
            let ci = content_indent.unwrap_or(usize::MAX);
            if blank {
                // Whitespace beyond the content indentation is content.
                let text = if content_indent.is_some() && indent > ci { &self.src[ls + ci..self.pos] } else { "" };
                lines.push(text);
                continue;
            }
            if indent < ci {
                self.restore(saved);
                break;
            }
            self.skip_to_eol();
            lines.push(&self.src[ls + ci..self.pos]);
        }
        // Trailing empty lines are chomped, not content.
        let mut end = lines.len();
        while end > 0 && lines[end - 1].is_empty() {
            end -= 1;
        }
        let trailing = lines.len() - end;
        let body = &lines[..end];
        let mut out = String::new();
        if folded {
            let mut prev_more_indented = false;
            let mut seen_content = false;
            let mut pending_breaks = 0usize;
            for l in body {
                if l.is_empty() {
                    pending_breaks += 1;
                    continue;
                }
                let more_indented = l.starts_with([' ', '\t']);
                if !seen_content {
                    out.extend(std::iter::repeat_n('\n', pending_breaks));
                } else if pending_breaks == 0 {
                    out.push(if more_indented || prev_more_indented { '\n' } else { ' ' });
                } else {
                    let extra = usize::from(more_indented || prev_more_indented);
                    out.extend(std::iter::repeat_n('\n', pending_breaks + extra));
                }
                seen_content = true;
                pending_breaks = 0;
                out.push_str(l);
                prev_more_indented = more_indented;
            }
        } else {
            for (i, l) in body.iter().enumerate() {
                if i > 0 {
                    out.push('\n');
                }
                out.push_str(l);
            }
        }
        match chomp {
            -1 => {}
            0 => {
                if !body.is_empty() {
                    out.push('\n');
                }
            }
            _ => {
                if !body.is_empty() {
                    out.push('\n');
                }
                out.extend(std::iter::repeat_n('\n', trailing));
            }
        }
        // Leave the position at the end of the last consumed line.
        Ok(out)
    }

    fn flow_ws(&mut self) {
        loop {
            match self.peek() {
                b' ' | b'\t' | b'\r' => self.pos += 1,
                b'\n' => self.bump(),
                b'#' if self.pos == self.line_start || is_ws(self.b[self.pos - 1]) => self.skip_to_eol(),
                _ => return,
            }
        }
    }

    /// `:` acting as a value indicator inside a flow collection.
    fn at_flow_value_indicator(&self) -> bool {
        self.peek() == b':' && {
            let n = self.peek_at(1);
            is_ws_or_eol(n) || is_flow_indicator(n)
        }
    }

    fn flow_node(&mut self) -> Result<Node> {
        self.enter()?;
        let r = self.flow_node_inner();
        self.depth -= 1;
        r
    }

    fn flow_node_inner(&mut self) -> Result<Node> {
        self.flow_ws();
        let line = self.line;
        let props = self.props()?;
        self.flow_ws();
        let node = match self.peek() {
            b'[' => self.flow_seq()?,
            b'{' => self.flow_map()?,
            b'"' => Node { line, kind: NodeKind::Scalar { text: self.double_quoted()?, style: Style::Quoted, tag: None } },
            b'\'' => Node { line, kind: NodeKind::Scalar { text: self.single_quoted()?, style: Style::Quoted, tag: None } },
            b'*' => self.alias()?,
            0 => return self.err("unterminated flow collection"),
            _ => Node { line, kind: NodeKind::Scalar { text: self.plain_flow(), style: Style::Plain, tag: None } },
        };
        Ok(self.finish(node, props))
    }

    /// A plain scalar inside a flow collection (may span lines).
    fn plain_flow(&mut self) -> String {
        let mut out = String::new();
        loop {
            let start = self.pos;
            while !self.at_eol() {
                let c = self.peek();
                if is_flow_indicator(c) || self.at_flow_value_indicator() {
                    break;
                }
                if c == b'#' && self.pos > start && is_ws(self.b[self.pos - 1]) {
                    break;
                }
                self.pos += 1;
            }
            out.push_str(self.src[start..self.pos].trim_end_matches([' ', '\t']));
            if !self.at_eol() {
                break;
            }
            // Continue onto the next line unless it starts with an indicator.
            let saved = self.save();
            let mut blanks = 0;
            loop {
                self.bump();
                self.skip_spaces();
                if self.peek() == b'\n' {
                    blanks += 1;
                } else {
                    break;
                }
            }
            let c = self.peek();
            if c == 0 || is_flow_indicator(c) || c == b'#' || self.at_flow_value_indicator() {
                self.restore(saved);
                break;
            }
            if blanks == 0 {
                out.push(' ');
            } else {
                out.extend(std::iter::repeat_n('\n', blanks));
            }
        }
        out
    }

    fn flow_seq(&mut self) -> Result<Node> {
        let line = self.line;
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.flow_ws();
            match self.peek() {
                b']' => {
                    self.pos += 1;
                    break;
                }
                0 => return self.err("unterminated flow sequence"),
                _ => {}
            }
            let mut item = self.flow_node()?;
            self.flow_ws();
            if self.peek() == b':' {
                // A single-pair mapping inside a sequence: `[a: 1]`.
                self.pos += 1;
                self.flow_ws();
                let value = if matches!(self.peek(), b',' | b']') { self.null_node(self.line) } else { self.flow_node()? };
                let l = item.line;
                item = Node { line: l, kind: NodeKind::Map(vec![(item, value)]) };
                self.flow_ws();
            }
            items.push(item);
            match self.peek() {
                b',' => self.pos += 1,
                b']' => {
                    self.pos += 1;
                    break;
                }
                _ => return self.err("expected `,` or `]` in a flow sequence"),
            }
        }
        Ok(Node { line, kind: NodeKind::Seq(items) })
    }

    fn flow_map(&mut self) -> Result<Node> {
        let line = self.line;
        self.pos += 1;
        let mut entries = Vec::new();
        let mut keys = KeySet::default();
        loop {
            self.flow_ws();
            match self.peek() {
                b'}' => {
                    self.pos += 1;
                    break;
                }
                0 => return self.err("unterminated flow mapping"),
                b'?' if is_ws_or_eol(self.peek_at(1)) => self.pos += 1,
                _ => {}
            }
            let key = if self.at_flow_value_indicator() { self.null_node(self.line) } else { self.flow_node()? };
            self.flow_ws();
            let value = if self.peek() == b':' {
                self.pos += 1;
                self.flow_ws();
                if matches!(self.peek(), b',' | b'}') { self.null_node(self.line) } else { self.flow_node()? }
            } else {
                self.null_node(self.line)
            };
            keys.add(&entries, &key)?;
            entries.push((key, value));
            self.flow_ws();
            match self.peek() {
                b',' => self.pos += 1,
                b'}' => {
                    self.pos += 1;
                    break;
                }
                _ => return self.err("expected `,` or `}` in a flow mapping"),
            }
        }
        Ok(Node { line, kind: NodeKind::Map(entries) })
    }
}

/// Duplicate-key detection for one mapping, as Jackson's `STRICT_DUPLICATE_DETECTION` (which
/// Elasticsearch's YAML and JSON parsers enable) rejects `{ f: 1, f: 2 }` with `Duplicate field 'f'`.
/// Keys compare by text, so `a` and `"a"` are the same field. Small mappings (nearly all) are
/// scanned linearly; a large one switches to a hash set so a big YAML response stays linear.
#[derive(Default)]
struct KeySet {
    seen: Option<std::collections::HashSet<String>>,
}

impl KeySet {
    const LINEAR: usize = 32;

    fn add(&mut self, entries: &[(Node, Node)], key: &Node) -> Result<()> {
        let text = key.key_text();
        let duplicate = match &mut self.seen {
            Some(seen) => !seen.insert(text.clone()),
            None if entries.len() < Self::LINEAR => entries.iter().any(|(k, _)| k.text().map_or_else(|| k.key_text() == text, |t| t == text)),
            None => {
                let mut seen: std::collections::HashSet<String> = entries.iter().map(|(k, _)| k.key_text()).collect();
                let dup = !seen.insert(text.clone());
                self.seen = Some(seen);
                dup
            }
        };
        if duplicate {
            return Err(YamlError { line: key.line, message: format!("Duplicate field '{text}'") });
        }
        Ok(())
    }
}
