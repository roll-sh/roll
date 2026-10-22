//! The query snippet Cassandra appends to parser errors, a port of
//! `org.apache.cassandra.cql3.ErrorCollector.appendSnippet`.
//!
//! It shows the tokens from ten before the offending one to two after it, in the stream as
//! ANTLR has fetched it (hidden whitespace and comment tokens count), with the offending token in
//! brackets: `(SELECT * [FRM]...)`. Lines are joined without separators, as Cassandra does.
//! Positions and lengths are Java `String` indices (UTF-16 units), and token lengths are those of
//! the token *text*, which for strings and quoted names is the unescaped content; Cassandra has
//! the same quirk, so the brackets can land off the token.

use crate::Dialect;
use crate::lexer::{self, Token};
use crate::tokens as t;

const FIRST_TOKEN_OFFSET: usize = 10;
const LAST_TOKEN_OFFSET: usize = 2;

/// The snippet for an error at raw token `index`, with `size` raw tokens fetched. `None` when
/// a token involved is end of input (Cassandra's EOF token has no position).
pub(crate) fn snippet(src: &str, dialect: Dialect, index: usize, size: usize) -> Option<String> {
    let all = lexer::lex(src, dialect, true).tokens;
    if size == 0 {
        return None;
    }
    let get = |i: usize| all.get(i).copied();
    let from = get(index.saturating_sub(FIRST_TOKEN_OFFSET))?;
    let to = get((size - 1).min(index + LAST_TOKEN_OFFSET))?;
    let offending = get(index.min(size - 1))?;
    if [from, to, offending].iter().any(|tk| tk.ty == t::EOF) {
        return None;
    }
    let len = |tk: &Token| lexer::token_text(src, tk).encode_utf16().count();
    let last_pos = |tk: &Token| tk.col as usize + len(tk);

    // Java's `query.split("\n")`: trailing empty strings are dropped.
    let mut lines: Vec<Vec<u16>> = src.split('\n').map(|l| l.encode_utf16().collect()).collect();
    while lines.len() > 1 && lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    if lines.len() == 1 && lines[0].is_empty() {
        return None;
    }
    let line_idx = |tk: &Token| tk.line as usize - 1;
    if [from, to, offending].iter().any(|tk| line_idx(tk) >= lines.len()) {
        return None;
    }

    let include_start = from.line == 1 && from.col == 0;
    let include_end = to.line as usize == lines.len() && last_pos(&to) == lines[lines.len() - 1].len();

    let mut out = String::from(" (");
    if !include_start {
        out.push_str("...");
    }
    let to_line = line_idx(&to);
    let to_end = last_pos(&to);
    if to_end < lines[to_line].len() {
        lines[to_line].truncate(to_end);
    }
    let off_line = line_idx(&offending);
    let highlighted = highlight(&lines[off_line], offending.col as usize, last_pos(&offending))?;
    lines[off_line] = highlighted;
    let from_line = line_idx(&from);
    let from_col = from.col as usize;
    if from_col > lines[from_line].len() {
        return None;
    }
    lines[from_line] = lines[from_line][from_col..].to_vec();
    for line in &lines[from_line..=to_line] {
        out.push_str(&String::from_utf16_lossy(line));
    }
    if !include_end {
        out.push_str("...");
    }
    out.push(')');
    Some(out)
}

/// Inserts `]` at `end` then `[` at `start`. Java throws when an index is past the end of the
/// line; this returns `None` and the caller omits the snippet.
fn highlight(line: &[u16], start: usize, end: usize) -> Option<Vec<u16>> {
    if end > line.len() || start > end {
        return None;
    }
    let mut v = Vec::with_capacity(line.len() + 2);
    v.extend_from_slice(&line[..start]);
    v.push(b'[' as u16);
    v.extend_from_slice(&line[start..end]);
    v.push(b']' as u16);
    v.extend_from_slice(&line[end..]);
    Some(v)
}
