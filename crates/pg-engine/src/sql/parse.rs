//! Parsing SQL with Postgres's own grammar (libpg_query), keeping the error position that the
//! safe wrapper drops.

use crate::error::{Error, Result, code};
use pg_query::protobuf::{ParseResult, RawStmt};
use prost::Message;
use std::ffi::{CStr, CString, c_char, c_int};

#[repr(C)]
struct PgQueryError {
    message: *mut c_char,
    funcname: *mut c_char,
    filename: *mut c_char,
    lineno: c_int,
    cursorpos: c_int,
    context: *mut c_char,
}

#[repr(C)]
struct PgQueryProtobuf {
    len: usize,
    data: *mut c_char,
}

#[repr(C)]
struct PgQueryProtobufParseResult {
    parse_tree: PgQueryProtobuf,
    stderr_buffer: *mut c_char,
    error: *mut PgQueryError,
}

unsafe extern "C" {
    fn pg_query_parse_protobuf_opts(input: *const c_char, options: c_int) -> PgQueryProtobufParseResult;
    fn pg_query_free_protobuf_parse_result(result: PgQueryProtobufParseResult);
}

/// Parse a query string into its statements. Syntax errors carry Postgres's message and the
/// 1-based character position. Strings are read the standard way: this is for text the engine
/// keeps (see `parse_live` for what a client or a function body sends).
pub fn parse(sql: &str) -> Result<Vec<RawStmt>> {
    parse_opts(sql, 0)
}

/// Parse a query string as the scanner reads it under the session's settings
/// (`standard_conforming_strings`, `backslash_quote`, `escape_string_warning`). Returns the
/// text the statements' locations point into: with `standard_conforming_strings` off, the
/// query with its ordinary literals made standard (character positions kept). Warnings go
/// to `scs::take_warnings`.
pub fn parse_live(sql: &str) -> Result<(std::borrow::Cow<'_, str>, Vec<RawStmt>)> {
    use super::scs;
    use std::borrow::Cow;
    let o = scs::opts();
    // Where a syntax error stopped the scanner.
    let stop_at = |r: &Result<Vec<RawStmt>>| match r {
        Err(e) => Some(
            e.position
                .map_or(sql.len(), |p| char_to_byte(sql, p as usize)),
        ),
        Ok(_) => None,
    };
    if o.is_default() {
        let r = with_param_junk(sql, true, parse(sql));
        scs::push_warnings(scs::with_long_identifiers(sql, true, stop_at(&r), Vec::new()));
        return Ok((Cow::Borrowed(sql), r?));
    }
    let mut bits = 0;
    if o.no_backslash_quote {
        bits |= pg_query::ParserOptions::DISABLE_BACKSLASH_QUOTE.bits();
    }
    if !o.standard_strings {
        bits |= pg_query::ParserOptions::DISABLE_STANDARD_CONFORMING_STRINGS.bits();
    }
    let checked = with_param_junk(sql, o.standard_strings, parse_opts(sql, bits));
    // Warnings come from the tokens scanned before an error.
    let stop = stop_at(&checked);
    if o.standard_strings {
        scs::push_warnings(scs::with_long_identifiers(sql, true, stop, Vec::new()));
        return checked.map(|s| (Cow::Borrowed(sql), s));
    }
    let scan = scs::scan(sql, o, stop);
    scs::push_warnings(scs::with_long_identifiers(sql, false, stop, scan.warnings));
    let stmts = checked?;
    if let Some(e) = scan.error {
        return Err(e);
    }
    match scan.rewritten {
        Some(text) => {
            let stmts = parse(&text)?;
            Ok((Cow::Owned(text), stmts))
        }
        None => Ok((Cow::Borrowed(sql), stmts)),
    }
}

/// Postgres's scanner rejects `$1a` ("trailing junk after parameter"), which libpg_query reads
/// as `$1 a`: that error, unless the parse failed earlier in the text.
fn with_param_junk(sql: &str, standard: bool, r: Result<Vec<RawStmt>>) -> Result<Vec<RawStmt>> {
    let Some((s, e)) = super::scs::param_junk(sql, standard) else {
        return r;
    };
    let pos = sql[..s].chars().count() + 1;
    if let Err(err) = &r
        && err.position.is_some_and(|p| (p as usize) < pos)
    {
        return r;
    }
    Err(Error::new(
        code::SYNTAX_ERROR,
        format!("trailing junk after parameter at or near \"{}\"", &sql[s..e]),
    )
    .position(pos as u32))
}

/// A routine's body for binding: a `BEGIN ATOMIC` / `RETURN` body is text the engine stored
/// (standard strings); any other is read under the current settings, its warnings left for
/// `scs::take_warnings`.
pub fn parse_body(f: &crate::catalog::Function) -> Result<Vec<RawStmt>> {
    if f.sql_body.is_some() {
        parse(&f.body)
    } else {
        parse_live(&f.body).map(|(_, s)| s)
    }
}

/// `parse_body` only to look at the statements: no warnings.
pub fn parse_body_quiet(f: &crate::catalog::Function) -> Result<Vec<RawStmt>> {
    let n = super::scs::pending();
    let r = parse_body(f);
    super::scs::discard_since(n);
    r
}

/// Whether a SQL function's body runs statement by statement when called (never bound as
/// one query, inlined or evaluated while planning): it has several statements, or a utility
/// statement.
pub fn body_runs_by_statement(f: &crate::catalog::Function) -> bool {
    use pg_query::NodeEnum as N;
    parse_body_quiet(f).is_ok_and(|s| {
        s.len() > 1
            || s.first()
                .and_then(|r| r.stmt.as_deref())
                .and_then(|n| n.node.as_ref())
                .is_some_and(|n| {
                    !matches!(
                        n,
                        N::SelectStmt(_)
                            | N::InsertStmt(_)
                            | N::UpdateStmt(_)
                            | N::DeleteStmt(_)
                            | N::MergeStmt(_)
                    )
                })
    })
}

/// Whether 1-based character position `pos` of `sql` is inside a `U&'...'` literal.
fn in_unicode_literal(sql: &str, pos: c_int) -> bool {
    let at = char_to_byte(sql, pos.max(1) as usize);
    let before = &sql[..at];
    let Some(start) = before.rfind("U&'").max(before.rfind("u&'")) else {
        return false;
    };
    // Inside while the quotes after the opening one pair up ('').
    before[start + 3..].matches('\'').count().is_multiple_of(2)
}

/// The byte offset of 1-based character position `p` in `s`.
fn char_to_byte(s: &str, p: usize) -> usize {
    s.char_indices()
        .nth(p.saturating_sub(1))
        .map_or(s.len(), |(b, _)| b)
}

fn parse_opts(sql: &str, options: c_int) -> Result<Vec<RawStmt>> {
    let input = CString::new(sql).map_err(|_| {
        Error::new(
            code::SYNTAX_ERROR,
            "invalid byte sequence for encoding \"UTF8\": 0x00",
        )
    })?;
    // SAFETY: libpg_query takes a NUL-terminated string and returns owned buffers that we free
    // below exactly once.
    unsafe {
        let r = pg_query_parse_protobuf_opts(input.as_ptr(), options);
        let out = if !r.error.is_null() {
            let e = &*r.error;
            let msg = CStr::from_ptr(e.message).to_string_lossy().to_string();
            // Grammar errors that aren't syntax errors.
            let c = if msg.starts_with("unsafe use of \\' ") {
                code::NONSTANDARD_USE_OF_ESCAPE_CHARACTER
            } else if msg.starts_with("invalid byte sequence for encoding") {
                code::CHARACTER_NOT_IN_REPERTOIRE
            } else if msg == "invalid Unicode escape" && !in_unicode_literal(sql, e.cursorpos) {
                code::INVALID_ESCAPE_SEQUENCE
            } else if msg.starts_with("role name \"") && msg.ends_with("is reserved") {
                code::RESERVED_NAME
            } else if msg.contains("is no longer supported")
                || msg.contains("not supported") && !msg.starts_with("syntax error")
                || msg.starts_with("only string constants are supported")
                || msg.starts_with("unsafe use of string constant")
                || msg.contains("constraints cannot be marked")
            {
                code::FEATURE_NOT_SUPPORTED
            } else {
                code::SYNTAX_ERROR
            };
            let hint = match msg.as_str() {
                "missing argument" => {
                    Some("Use NONE to denote the missing argument of a unary operator.")
                }
                // U&'...' (`str_udeescape`) or E'...' (the scanner).
                "invalid Unicode escape" if in_unicode_literal(sql, e.cursorpos) => {
                    Some("Unicode escapes must be \\XXXX or \\+XXXXXX.")
                }
                "invalid Unicode escape" => Some("Unicode escapes must be \\uXXXX or \\UXXXXXXXX."),
                m if m.starts_with("unrecognized row security option") => {
                    Some("Only PERMISSIVE or RESTRICTIVE policies are supported currently.")
                }
                "UNENCRYPTED PASSWORD is no longer supported" => {
                    Some("Remove UNENCRYPTED to store the password in encrypted form instead.")
                }
                "LIMIT #,# syntax is not supported" => Some("Use separate LIMIT and OFFSET clauses."),
                "unsafe use of \\' in a string literal" => Some(
                    "Use '' to write quotes in strings. \\' is insecure in client-only encodings.",
                ),
                _ => None,
            };
            let unicode_off = msg == "unsafe use of string constant with Unicode escapes";
            let pub_list = msg == "invalid publication object list";
            let mut err = Error::new(c, msg);
            if unicode_off {
                err = err.detail(
                    "String constants with Unicode escapes cannot be used when \"standard_conforming_strings\" is off.",
                );
            }
            if pub_list {
                err = err.detail(
                    "One of TABLE or TABLES IN SCHEMA must be specified before a standalone table or schema name.",
                );
            }
            if let Some(h) = hint {
                err = err.hint(h);
            }
            if e.cursorpos > 0 {
                err = err.position(e.cursorpos as u32);
            }
            Err(err)
        } else {
            let data = std::slice::from_raw_parts(r.parse_tree.data as *const u8, r.parse_tree.len);
            ParseResult::decode(data)
                .map(|p| p.stmts)
                .map_err(|e| crate::error::internal(format!("parse tree: {e}")))
        };
        pg_query_free_protobuf_parse_result(r);
        out
    }
}

/// Where `stmt_text` starts in `sql`.
pub fn stmt_offset(sql: &str, s: &RawStmt) -> usize {
    let start = (s.stmt_location.max(0) as usize).min(sql.len());
    start + sql[start..].len() - sql[start..].trim_start().len()
}

/// The text of one statement out of a multi-statement string.
pub fn stmt_text<'a>(sql: &'a str, s: &RawStmt) -> &'a str {
    let start = (s.stmt_location.max(0) as usize).min(sql.len());
    let end = if s.stmt_len > 0 {
        (start + s.stmt_len as usize).min(sql.len())
    } else {
        sql.len()
    };
    sql.get(start..end).unwrap_or(sql).trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn syntax_error_position() {
        let e = parse("SELECT 1 FRM t").unwrap_err();
        assert_eq!(e.message, "syntax error at or near \"t\"");
        assert_eq!(e.position, Some(14));
        assert_eq!(parse("select 1; select 2").unwrap().len(), 2);
    }
}
