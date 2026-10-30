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
    fn assign_max_stack_depth(newval: c_int, extra: *mut std::ffi::c_void);
}

/// The stack libpg_query gets: its tree output checks a limit (`max_stack_depth`) it sizes
/// from the thread's stack below the call (less a 512kB slop, at most 2MB), so on the
/// thread's own stack 1MB is enough; on a segment of the engine's (outside the bounds it
/// reads) it keeps the 2MB set here, and gets that with its slop.
const C_PARSE_STACK: usize = 1 << 20;
const C_PARSE_STACK_ON_SEGMENT: usize = 3 << 20;

/// The stack of a parse the 2MB limit stopped: libpg_query's limit is this less 1MB (its
/// frames take up to 2kB a level unoptimised; Postgres analyses 4,000 levels in its 2MB).
const C_DEEP_STACK: usize = 32 << 20;

/// libpg_query's check_stack_depth error.
const DEPTH_MSG: &[u8] = b"stack depth limit exceeded";

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
    // (In a database whose encoding isn't UTF8, escapes in literals make its characters.)
    if let Some((text, ws)) = super::srvlit::rewrite(sql, o.encoding, o.standard_strings)? {
        let n = scs::pending();
        let r = parse_scanned(&text, o);
        if !ws.is_empty() {
            // (The rewritten literals' warnings, in token order with the others.)
            let mut all = scs::take_warnings();
            let mut mine = all.split_off(n.min(all.len()));
            mine.extend(ws);
            mine.sort_by_key(|w| w.position.unwrap_or(0));
            all.extend(mine);
            scs::push_warnings(all);
        }
        let (t, stmts) = r?;
        return Ok((Cow::Owned(t.into_owned()), stmts));
    }
    parse_scanned(sql, o)
}

fn parse_scanned(sql: &str, o: super::scs::LexOpts) -> Result<(std::borrow::Cow<'_, str>, Vec<RawStmt>)> {
    use super::scs;
    use std::borrow::Cow;
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

/// Parse trees of texts parsed lately, by (options, text), shared by every session: a parse
/// is a pure function of them, and the same texts come again and again (a client's repeated
/// simple queries, function bodies, stored defaults, checks and view bodies deparsed for
/// each catalog read), from new connections too. Bounded by an estimate of the trees' bytes.
static PARSED: crate::lru::Shared<(c_int, Box<str>), std::sync::Arc<Trees>> =
    crate::lru::Shared::new("parse trees", PARSE_CACHE_BYTES);

const PARSE_CACHE_BYTES: usize = 32 << 20;
/// Longer texts aren't kept.
const PARSE_CACHE_MAX_TEXT: usize = 16 << 10;

/// A cached parse and the length of its text: whichever session drops it last drops it with
/// the stack its depth needs.
struct Trees(Result<Vec<RawStmt>>, usize);

impl Drop for Trees {
    fn drop(&mut self) {
        let trees = std::mem::replace(&mut self.0, Ok(Vec::new()));
        crate::stack::drop_deep(trees, self.1);
    }
}

/// The parse tree cache's footprint (`lru::footprints`).
pub(crate) fn parsed_footprint() -> (&'static str, crate::lru::Footprint) {
    PARSED.footprint()
}

/// `parse` through `PARSED` (a copy of the tree kept for `tag` and `sql`).
pub fn cached_parse(
    tag: c_int,
    sql: &str,
    parse: impl FnOnce() -> Result<Vec<RawStmt>>,
) -> Result<Vec<RawStmt>> {
    if sql.len() > PARSE_CACHE_MAX_TEXT {
        return parse();
    }
    // (Copying a tree recurses as deep as it goes.)
    let copy = |r: &Result<Vec<RawStmt>>| {
        crate::stack::with_headroom(crate::stack::tree_headroom(sql.len()), || r.clone())
    };
    let key = (tag, Box::<str>::from(sql));
    if let Some(t) = PARSED.get(&key) {
        return copy(&t.0);
    }
    let r = parse();
    // (Trees are a few times their text.)
    let trees = std::sync::Arc::new(Trees(copy(&r), sql.len()));
    drop(PARSED.insert(key, trees, sql.len() * 8 + 256));
    r
}

fn parse_opts(sql: &str, options: c_int) -> Result<Vec<RawStmt>> {
    cached_parse(options, sql, || parse_uncached(sql, options))
}

fn parse_uncached(sql: &str, options: c_int) -> Result<Vec<RawStmt>> {
    let input = CString::new(sql).map_err(|_| {
        Error::new(
            code::SYNTAX_ERROR,
            "invalid byte sequence for encoding \"UTF8\": 0x00",
        )
    })?;
    // SAFETY: libpg_query takes a NUL-terminated string and returns owned buffers that we free
    // below exactly once; the setting is libpg_query's thread-local.
    let parse = || unsafe {
        assign_max_stack_depth(2048, std::ptr::null_mut());
        pg_query_parse_protobuf_opts(input.as_ptr(), options)
    };
    // It runs with that stack below it: on the thread's own, or on a segment of 3MB.
    let mut r = if crate::stack::on_thread_stack(C_PARSE_STACK) {
        parse()
    } else {
        crate::stack::with_headroom(C_PARSE_STACK_ON_SEGMENT, parse)
    };
    // Its tree output takes more stack per level than Postgres's analysis does, so a tree
    // Postgres takes can exceed that 2MB: then it parses again with more, on a stack below
    // the thread's (where libpg_query keeps the limit it is given), and the depth limits
    // that apply are the engine's own.
    // SAFETY: as above.
    if unsafe { !r.error.is_null() && CStr::from_ptr((*r.error).message).to_bytes() == DEPTH_MSG }
    {
        let kb = C_DEEP_STACK / 1024 - 1024;
        let deep = crate::stack::below_thread_stack(C_DEEP_STACK, || unsafe {
            assign_max_stack_depth(kb as c_int, std::ptr::null_mut());
            pg_query_parse_protobuf_opts(input.as_ptr(), options)
        });
        if let Some(deep) = deep {
            // SAFETY: freed once; `r` is replaced.
            unsafe { pg_query_free_protobuf_parse_result(r) };
            r = deep;
        }
    }
    unsafe {
        let out = if !r.error.is_null() {
            let e = &*r.error;
            let msg = CStr::from_ptr(e.message).to_string_lossy().to_string();
            // (libpg_query's tree output also stops at 10,000 nested messages: Postgres has
            // no such limit, but fails with this error not much deeper.)
            if msg.as_bytes() == DEPTH_MSG
                || msg == "parse tree is nested too deeply to serialize to protobuf"
            {
                pg_query_free_protobuf_parse_result(r);
                return Err(crate::stack::depth_error());
            }
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
            decode_tree(data)
        };
        pg_query_free_protobuf_parse_result(r);
        out
    }
}

/// The parse tree's protobuf form decoded. Its nesting is the grammar's (Postgres takes a
/// UNION of hundreds of arms, a list nested as deep), so prost's recursion limit is off; the
/// decoder recurses once per level, at most one level per two bytes, so it runs with stack
/// enough for the tree's size.
fn decode_tree(data: &[u8]) -> Result<Vec<RawStmt>> {
    crate::stack::with_headroom(crate::stack::tree_headroom(data.len()), || {
        ParseResult::decode(data)
            .map(|p| p.stmts)
            .map_err(|e| crate::error::internal(format!("parse tree: {e}")))
    })
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
