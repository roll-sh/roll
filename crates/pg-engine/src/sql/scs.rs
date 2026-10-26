//! `standard_conforming_strings = off`, `backslash_quote` and `escape_string_warning`: how the
//! core scanner (scan.l) reads ordinary `'...'` literals when backslashes are escapes.
//!
//! Queries are parsed under the session's settings (libpg_query's parser options give the
//! scanner's errors), and then rewritten into the text a `standard_conforming_strings = on`
//! scanner reads the same way: every ordinary literal becomes one holding its value, padded
//! with spaces after its closing quote to keep the character positions of everything else.
//! The rest of the engine, which keeps statement text (views, defaults, function bodies) and
//! parses it again, then only ever sees standard literals. The warnings the scanner raises
//! (`check_string_escape_warning`) come from the same scan.

use crate::error::{Error, Severity, code};
use std::cell::{Cell, RefCell};

/// The scanner settings in effect (`backslash_quote`, `escape_string_warning`,
/// `standard_conforming_strings`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LexOpts {
    pub standard_strings: bool,
    pub escape_warning: bool,
    /// `\'` is rejected: `backslash_quote = off`, or `safe_encoding` with a client-only
    /// client encoding.
    pub no_backslash_quote: bool,
    /// The database's encoding (escapes in literals make its characters: srvlit.rs).
    pub encoding: i32,
}

impl Default for LexOpts {
    fn default() -> Self {
        LexOpts {
            standard_strings: true,
            escape_warning: true,
            no_backslash_quote: false,
            encoding: crate::types::mbconv::UTF8,
        }
    }
}

impl LexOpts {
    /// From the settings' values (`client_encoding` decides `safe_encoding`).
    pub fn from_settings(scs: bool, warn: bool, backslash_quote: &str, client_encoding: &str) -> Self {
        let client_only = matches!(
            client_encoding.to_ascii_uppercase().as_str(),
            "SJIS" | "BIG5" | "GBK" | "UHC" | "GB18030" | "JOHAB"
        );
        LexOpts {
            standard_strings: scs,
            escape_warning: warn,
            no_backslash_quote: match backslash_quote.to_ascii_lowercase().as_str() {
                "off" | "false" | "no" | "0" => true,
                "on" | "true" | "yes" | "1" => false,
                _ => client_only,
            },
            encoding: crate::types::mbconv::UTF8,
        }
    }

    /// Whether parsing differs from the defaults at all.
    pub fn is_default(&self) -> bool {
        self.standard_strings && !self.no_backslash_quote
    }
}

thread_local! {
    /// One thread runs one session at a time; the session sets these before it parses.
    static OPTS: Cell<LexOpts> = Cell::new(LexOpts::default());
    /// Scanning text whose warnings were already given (a statement of a body bound again).
    static QUIET: Cell<bool> = const { Cell::new(false) };
    /// The query string being run with its literals made standard, and as the client sent
    /// it.
    static SOURCE: RefCell<Option<(String, String)>> = const { RefCell::new(None) };
    /// Scanner warnings not yet reported.
    static WARNINGS: RefCell<Vec<Error>> = const { RefCell::new(Vec::new()) };
}

pub fn set_opts(o: LexOpts) {
    OPTS.with(|c| c.set(o));
}

pub fn opts() -> LexOpts {
    OPTS.with(|c| c.get())
}

/// Run `f` with the scanner reading strings the standard way (text the engine wrote itself).
pub fn with_standard<T>(f: impl FnOnce() -> T) -> T {
    let _g = crate::scoped::set(&OPTS, LexOpts::default(), "scs::OPTS");
    f()
}

/// Remember the query string being run: (as made standard, as sent) when they differ.
pub fn set_source(s: Option<(String, String)>) {
    SOURCE.with(|c| *c.borrow_mut() = s);
}

/// `sql`, a piece of the (standard) query string being run (at byte `offset` when known),
/// as the client wrote it. Positions agree: literals keep their character positions.
pub fn source_of(sql: &str, offset: Option<usize>) -> Option<String> {
    SOURCE.with(|c| {
        let c = c.borrow();
        let (lexed, orig) = c.as_ref()?;
        let at = match offset {
            Some(o) => (lexed.get(o..o + sql.len()) == Some(sql)).then_some(o)?,
            None => lexed.find(sql)?,
        };
        let from = lexed[..at].chars().count();
        let mut len = sql.chars().count();
        // A literal's padding the piece's end trimmed: the literal's end as sent.
        let (mut l, mut o) = (lexed.chars().skip(from + len), orig.chars().skip(from + len));
        while let (Some(' '), Some(c)) = (l.next(), o.next()) {
            if c == ' ' {
                break;
            }
            len += 1;
        }
        let start = orig.char_indices().nth(from).map(|(b, _)| b)?;
        let end = orig[start..]
            .char_indices()
            .nth(len)
            .map_or(orig.len(), |(b, _)| start + b);
        Some(orig[start..end].to_string())
    })
}

/// `match_prosrc_to_query`: where in `query` a routine's source `prosrc` is, as `$tag$` or
/// a `'...'` literal (backslashes and doubled quotes skipped), with 1-based character
/// position `cursor` in `prosrc` moved there. None when it's not found exactly once.
pub fn prosrc_position(query: &str, prosrc: &str, cursor: usize) -> Option<usize> {
    let q = query.as_bytes();
    let p = prosrc.as_bytes();
    if q.len() <= p.len() {
        return None;
    }
    let mut found = None;
    for cur in 0..q.len() - p.len() {
        let hit = if q[cur] == b'$'
            && q[cur + 1..].starts_with(p)
            && q.get(cur + 1 + p.len()) == Some(&b'$')
        {
            Some(cursor)
        } else if q[cur] == b'\'' {
            literal_match(prosrc, &q[cur + 1..], cursor)
        } else {
            None
        };
        if let Some(newcp) = hit {
            if found.is_some() {
                return None;
            }
            found = Some(String::from_utf8_lossy(&q[..cur + 1]).chars().count() + newcp);
        }
    }
    found
}

/// `match_prosrc_to_literal`.
fn literal_match(prosrc: &str, lit: &[u8], mut cursor: usize) -> Option<usize> {
    let mut newcp = cursor;
    let mut l = 0;
    for ch in prosrc.chars() {
        cursor = cursor.wrapping_sub(1);
        let before = (cursor as isize) > 0;
        if lit.get(l) == Some(&b'\\') {
            l += 1;
            if before {
                newcp += 1;
            }
        } else if lit.get(l) == Some(&b'\'') {
            if lit.get(l + 1) != Some(&b'\'') {
                return None;
            }
            l += 1;
            if before {
                newcp += 1;
            }
        }
        let mut buf = [0u8; 4];
        let enc = ch.encode_utf8(&mut buf).as_bytes();
        if lit.get(l..l + enc.len()) != Some(enc) {
            return None;
        }
        l += enc.len();
    }
    (lit.get(l) == Some(&b'\'') && lit.get(l + 1) != Some(&b'\'')).then_some(newcp)
}

/// Run `f` without scanner warnings (text scanned before, warnings given then).
pub fn quietly<T>(f: impl FnOnce() -> T) -> T {
    let _g = crate::scoped::set(&QUIET, true, "scs::QUIET");
    f()
}

/// The flags not at rest (`scoped::unsettled_for_test`).
pub(crate) fn unsettled(out: &mut Vec<&'static str>) {
    if QUIET.get() {
        out.push("scs::QUIET");
    }
}

/// The scanner warnings raised since the last call.
pub fn take_warnings() -> Vec<Error> {
    WARNINGS.with(|w| std::mem::take(&mut *w.borrow_mut()))
}

/// How many warnings are waiting (for `discard_since`).
pub fn pending() -> usize {
    WARNINGS.with(|w| w.borrow().len())
}

/// Drop the warnings raised since `pending()` was `n` (a parse only probing text).
pub fn discard_since(n: usize) {
    WARNINGS.with(|w| w.borrow_mut().truncate(n));
}

pub(crate) fn push_warnings(ws: Vec<Error>) {
    if !ws.is_empty() && !QUIET.with(|q| q.get()) {
        WARNINGS.with(|w| w.borrow_mut().extend(ws));
    }
}

/// What a scan found.
pub(crate) struct Scan {
    /// Warnings with their 1-based character positions.
    pub warnings: Vec<Error>,
    /// The text with every ordinary literal made standard (None: nothing changed).
    pub rewritten: Option<String>,
    /// A literal's value is not valid UTF8 (`pg_verifymbstr`).
    pub error: Option<Error>,
}

fn warn(msg: &str, hint: &str, pos: u32) -> Error {
    let mut e = Error::new(code::NONSTANDARD_USE_OF_ESCAPE_CHARACTER, msg)
        .hint(hint)
        .position(pos);
    e.severity = Severity::Warning;
    e
}

fn escape_warning(pos: u32) -> Error {
    warn(
        "nonstandard use of escape in a string literal",
        "Use the escape string syntax for escapes, e.g., E'\\r\\n'.",
        pos,
    )
}

/// The scanner's message for the value bytes of a literal that are not valid UTF8.
fn invalid_encoding(v: &[u8]) -> Error {
    let at = match std::str::from_utf8(v) {
        Ok(_) => v.iter().position(|b| *b == 0).unwrap_or(0),
        Err(e) => e.valid_up_to(),
    };
    let first = v[at];
    let len = match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
    .min(v.len() - at);
    let bytes: Vec<String> = v[at..at + len].iter().map(|b| format!("0x{b:02x}")).collect();
    Error::new(
        code::CHARACTER_NOT_IN_REPERTOIRE,
        format!(
            "invalid byte sequence for encoding \"UTF8\": {}",
            bytes.join(" ")
        ),
    )
}

/// Scan `sql` the way scan.l does with `standard_conforming_strings = off`, stopping at
/// byte `stop` (where a parse error was found) or at the first scanner error.
pub(crate) fn scan(sql: &str, o: LexOpts, stop: Option<usize>) -> Scan {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = Scan {
        warnings: Vec::new(),
        rewritten: None,
        error: None,
    };
    // (byte start, byte end, replacement) of literal segments that change.
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    // Character positions are counted incrementally.
    let mut pos_cache = (0usize, 0u32);
    let mut char_pos = |at: usize| -> u32 {
        let (cb, cc) = pos_cache;
        let (from, base) = if at >= cb { (cb, cc) } else { (0, 0) };
        let c = base + sql[from..at].chars().count() as u32;
        pos_cache = (at, c);
        c + 1
    };
    let is_ident_start = |c: u8| c.is_ascii_alphabetic() || c == b'_' || c >= 0x80;
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
    let mut i = 0;
    'outer: while i < n {
        if stop.is_some_and(|s| i > s) {
            break;
        }
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < n && b[i] != b'\n' && b[i] != b'\r' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 0;
            while i < n {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            continue;
        }
        let next = b.get(i + 1).copied();
        // What kind of quoted thing starts here: (quote index, backslash escapes, warn).
        let (q, escapes, warns) = match c {
            b'\'' => (i, true, true),
            b'e' | b'E' if next == Some(b'\'') => (i + 1, true, false),
            b'n' | b'N' if next == Some(b'\'') => (i + 1, true, true),
            b'b' | b'B' | b'x' | b'X' if next == Some(b'\'') => (i + 1, false, false),
            b'u' | b'U' if next == Some(b'&') && matches!(b.get(i + 2), Some(b'\'') | Some(b'"')) => {
                // U&'...' is an error with these settings (the parser reported it); U&"..."
                // is an identifier.
                if b[i + 2] == b'\'' {
                    break;
                }
                i = skip_quoted(b, i + 2, b'"');
                continue;
            }
            b'"' => {
                i = skip_quoted(b, i, b'"');
                continue;
            }
            b'$' => {
                if next.is_some_and(|d| d.is_ascii_digit()) {
                    i += 1;
                    while i < n && b[i].is_ascii_digit() {
                        i += 1;
                    }
                    continue;
                }
                let mut j = i + 1;
                if j < n && (b[j].is_ascii_alphabetic() || b[j] == b'_' || b[j] >= 0x80) {
                    while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
                        j += 1;
                    }
                }
                if b.get(j) != Some(&b'$') {
                    i += 1;
                    continue;
                }
                let tag = &sql[i..=j];
                match sql[j + 1..].find(tag) {
                    Some(k) => i = j + 1 + k + tag.len(),
                    None => break,
                }
                continue;
            }
            _ if is_ident_start(c) => {
                while i < n && is_ident(b[i]) {
                    i += 1;
                }
                continue;
            }
            b'0'..=b'9' => {
                while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                    if b[i] == b'.' && b.get(i + 1) == Some(&b'.') {
                        break;
                    }
                    i += 1;
                }
                continue;
            }
            _ => {
                i += 1;
                continue;
            }
        };
        if !escapes {
            // B'..' / X'..': no escapes, quotes end them.
            let mut j = q;
            loop {
                let mut k = j + 1;
                while k < n && b[k] != b'\'' {
                    k += 1;
                }
                if k >= n {
                    break 'outer;
                }
                match continuation(b, k + 1) {
                    Some(nq) => j = nq,
                    None => {
                        i = k + 1;
                        break;
                    }
                }
            }
            continue;
        }
        // An escaped literal: E'..' or, with standard_conforming_strings off, '..'.
        let start_pos = char_pos(q);
        let mut warn_first = warns && o.escape_warning;
        let mut seg = q;
        let mut segs: Vec<(usize, usize, Vec<u8>)> = Vec::new();
        loop {
            let mut v: Vec<u8> = Vec::new();
            let mut j = seg + 1;
            loop {
                if j >= n {
                    break 'outer;
                }
                match b[j] {
                    b'\'' => {
                        if b.get(j + 1) == Some(&b'\'') {
                            v.push(b'\'');
                            j += 2;
                            continue;
                        }
                        j += 1;
                        break;
                    }
                    b'\\' => {
                        let Some(&e) = b.get(j + 1) else {
                            break 'outer;
                        };
                        match e {
                            b'0'..=b'7' => {
                                let mut k = j + 1;
                                let mut val: u32 = 0;
                                while k < n && k < j + 4 && (b'0'..=b'7').contains(&b[k]) {
                                    val = val * 8 + (b[k] - b'0') as u32;
                                    k += 1;
                                }
                                v.push(val as u8);
                                j = k;
                                if warn_first {
                                    out.warnings.push(escape_warning(start_pos));
                                    warn_first = false;
                                }
                            }
                            b'x' if b.get(j + 2).is_some_and(u8::is_ascii_hexdigit) => {
                                let mut k = j + 2;
                                let mut val: u32 = 0;
                                while k < n && k < j + 4 && b[k].is_ascii_hexdigit() {
                                    val = val * 16 + (b[k] as char).to_digit(16).unwrap();
                                    k += 1;
                                }
                                v.push(val as u8);
                                j = k;
                                if warn_first {
                                    out.warnings.push(escape_warning(start_pos));
                                    warn_first = false;
                                }
                            }
                            b'u' | b'U' => {
                                let len = if e == b'u' { 4 } else { 8 };
                                let hex = |at: usize| -> Option<u32> {
                                    let s = sql.get(at..at + len)?;
                                    s.bytes()
                                        .all(|d| d.is_ascii_hexdigit())
                                        .then(|| u32::from_str_radix(s, 16).ok())
                                        .flatten()
                                };
                                // Invalid escapes and surrogates are errors the parser
                                // reported.
                                let Some(mut cp) = hex(j + 2) else {
                                    break 'outer;
                                };
                                // (scan.l warns with the cursor at the string's start
                                // here too, before moving it to the escape.)
                                if warn_first {
                                    out.warnings.push(escape_warning(start_pos));
                                    warn_first = false;
                                }
                                let mut k = j + 2 + len;
                                if (0xD800..0xDC00).contains(&cp) {
                                    let lo = (b.get(k) == Some(&b'\\'))
                                        .then(|| match b.get(k + 1) {
                                            Some(b'u') => hex(k + 2).map(|h| (h, 4)),
                                            Some(b'U') => sql
                                                .get(k + 2..k + 10)
                                                .filter(|s| s.bytes().all(|d| d.is_ascii_hexdigit()))
                                                .and_then(|s| u32::from_str_radix(s, 16).ok())
                                                .map(|h| (h, 8)),
                                            _ => None,
                                        })
                                        .flatten();
                                    match lo {
                                        Some((lo, l)) if (0xDC00..0xE000).contains(&lo) => {
                                            cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                            k += 2 + l;
                                        }
                                        _ => break 'outer,
                                    }
                                }
                                match char::from_u32(cp).filter(|c| *c != '\0') {
                                    Some(ch) => {
                                        let mut buf = [0u8; 4];
                                        v.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                                    }
                                    None => break 'outer,
                                }
                                j = k;
                            }
                            _ => {
                                if e == b'\'' && o.no_backslash_quote {
                                    break 'outer;
                                }
                                let ch_len = sql[j + 1..].chars().next().map_or(1, char::len_utf8);
                                let val: &[u8] = match e {
                                    b'b' => b"\x08",
                                    b'f' => b"\x0c",
                                    b'n' => b"\n",
                                    b'r' => b"\r",
                                    b't' => b"\t",
                                    b'v' => b"\x0b",
                                    _ => &b[j + 1..j + 1 + ch_len],
                                };
                                v.extend_from_slice(val);
                                j += 1 + ch_len;
                                if warn_first {
                                    out.warnings.push(match e {
                                        b'\'' => warn(
                                            "nonstandard use of \\' in a string literal",
                                            "Use '' to write quotes in strings, or use the escape string syntax (E'...').",
                                            start_pos,
                                        ),
                                        b'\\' => warn(
                                            "nonstandard use of \\\\ in a string literal",
                                            "Use the escape string syntax for backslashes, e.g., E'\\\\'.",
                                            start_pos,
                                        ),
                                        _ => escape_warning(start_pos),
                                    });
                                    warn_first = false;
                                }
                            }
                        }
                    }
                    _ => {
                        v.push(b[j]);
                        j += 1;
                    }
                }
            }
            segs.push((seg, j, v));
            match continuation(b, j) {
                Some(nq) => seg = nq,
                None => {
                    i = j;
                    break;
                }
            }
        }
        if warns {
            // An ordinary literal: rewrite it as a standard one.
            for (s, e, v) in segs {
                let Ok(text) = std::str::from_utf8(&v).ok().filter(|t| !t.contains('\0')).ok_or(())
                else {
                    out.error = Some(invalid_encoding(&v));
                    break 'outer;
                };
                let mut lit = String::with_capacity(v.len() + 2);
                lit.push('\'');
                lit.push_str(&text.replace('\'', "''"));
                lit.push('\'');
                let orig = &sql[s..e];
                if lit != orig {
                    let pad = orig.chars().count().saturating_sub(lit.chars().count());
                    lit.extend(std::iter::repeat_n(' ', pad));
                    edits.push((s, e, lit));
                }
            }
        } else if let Some((_, _, v)) = segs
            .iter()
            .find(|(_, _, v)| std::str::from_utf8(v).is_err() || v.contains(&0))
        {
            out.error = Some(invalid_encoding(v));
            break;
        }
    }
    if !edits.is_empty() {
        let mut s = String::with_capacity(sql.len());
        let mut at = 0;
        for (a, e, lit) in edits {
            s.push_str(&sql[at..a]);
            s.push_str(&lit);
            at = e;
        }
        s.push_str(&sql[at..]);
        out.rewritten = Some(s);
    }
    out
}

/// SQL the engine deparsed (with standard literals, as everything inside the engine has
/// them) as `ruleutils.c` prints it for this session: with `standard_conforming_strings`
/// off, `simple_quote_literal` doubles the backslashes of ordinary literals.
pub fn session_literals(sql: &str) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if opts().standard_strings || !sql.contains('\\') {
        return Cow::Borrowed(sql);
    }
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = String::with_capacity(n + 8);
    let mut at = 0;
    let mut i = 0;
    while i < n {
        let c = b[i];
        if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 {
            let start = i;
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 0x80) {
                i += 1;
            }
            if i == start + 1 && b.get(i) == Some(&b'\'') {
                // E'..' keeps its escapes; B'..' / X'..' have none.
                let esc = matches!(c, b'e' | b'E');
                i += 1;
                while i < n {
                    if esc && b[i] == b'\\' {
                        i += 2;
                        continue;
                    }
                    if b[i] == b'\'' {
                        if b.get(i + 1) == Some(&b'\'') {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            continue;
        }
        match c {
            b'"' => i = skip_quoted(b, i, b'"'),
            b'$' => {
                let mut j = i + 1;
                while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
                    j += 1;
                }
                if b.get(j) == Some(&b'$') && !b.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    let tag = &sql[i..=j];
                    i = sql[j + 1..].find(tag).map_or(n, |k| j + 1 + k + tag.len());
                } else {
                    i = j.max(i + 1);
                }
            }
            b'\'' => {
                let end = skip_quoted(b, i, b'\'');
                out.push_str(&sql[at..i]);
                out.push_str(&sql[i..end].replace('\\', "\\\\"));
                at = end;
                i = end;
            }
            _ => i += 1,
        }
    }
    if at == 0 {
        return Cow::Borrowed(sql);
    }
    out.push_str(&sql[at..]);
    Cow::Owned(out)
}

/// The value of an escaped literal's text between its quotes (`''` and backslash escapes, as
/// in `E'...'`); invalid escapes are kept as written.
pub fn unescape(body: &str) -> String {
    let b = body.as_bytes();
    let mut v: Vec<u8> = Vec::with_capacity(b.len());
    let mut j = 0;
    while j < b.len() {
        match b[j] {
            b'\'' if b.get(j + 1) == Some(&b'\'') => {
                v.push(b'\'');
                j += 2;
            }
            b'\\' if j + 1 < b.len() => {
                let e = b[j + 1];
                match e {
                    b'0'..=b'7' => {
                        let mut k = j + 1;
                        let mut val: u32 = 0;
                        while k < b.len() && k < j + 4 && (b'0'..=b'7').contains(&b[k]) {
                            val = val * 8 + (b[k] - b'0') as u32;
                            k += 1;
                        }
                        v.push(val as u8);
                        j = k;
                    }
                    b'x' if b.get(j + 2).is_some_and(u8::is_ascii_hexdigit) => {
                        let mut k = j + 2;
                        let mut val: u32 = 0;
                        while k < b.len() && k < j + 4 && b[k].is_ascii_hexdigit() {
                            val = val * 16 + (b[k] as char).to_digit(16).unwrap();
                            k += 1;
                        }
                        v.push(val as u8);
                        j = k;
                    }
                    b'u' | b'U' => {
                        let len = if e == b'u' { 4 } else { 8 };
                        let cp = body
                            .get(j + 2..j + 2 + len)
                            .filter(|s| s.bytes().all(|d| d.is_ascii_hexdigit()))
                            .and_then(|s| u32::from_str_radix(s, 16).ok());
                        let mut k = j + 2 + len;
                        let mut ch = cp.and_then(char::from_u32);
                        if let Some(hi) = cp.filter(|c| (0xD800..0xDC00).contains(c))
                            && body.get(k..k + 2) == Some("\\u")
                            && let Some(lo) = body
                                .get(k + 2..k + 6)
                                .and_then(|s| u32::from_str_radix(s, 16).ok())
                                .filter(|c| (0xDC00..0xE000).contains(c))
                        {
                            ch = char::from_u32(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00));
                            k += 6;
                        }
                        match ch {
                            Some(ch) => {
                                let mut buf = [0u8; 4];
                                v.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                                j = k;
                            }
                            None => {
                                v.push(e);
                                j += 2;
                            }
                        }
                    }
                    _ => {
                        let ch_len = body[j + 1..].chars().next().map_or(1, char::len_utf8);
                        match e {
                            b'b' => v.push(0x08),
                            b'f' => v.push(0x0c),
                            b'n' => v.push(b'\n'),
                            b'r' => v.push(b'\r'),
                            b't' => v.push(b'\t'),
                            b'v' => v.push(0x0b),
                            _ => v.extend_from_slice(&b[j + 1..j + 1 + ch_len]),
                        }
                        j += 1 + ch_len;
                    }
                }
            }
            c => {
                v.push(c);
                j += 1;
            }
        }
    }
    String::from_utf8_lossy(&v).into_owned()
}

/// The warning the scanner gives for the ordinary literal `lit` (quotes included) under
/// `standard_conforming_strings = off`, with its position counted from 1 at the literal.
pub fn literal_warning(lit: &str) -> Option<Error> {
    let o = opts();
    if o.standard_strings || !o.escape_warning {
        return None;
    }
    scan(lit, o, None).warnings.into_iter().next()
}

/// Skip a `q`-quoted token whose quote is at `i` (`""` inside); returns the index after it.
fn skip_quoted(b: &[u8], i: usize, q: u8) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == q {
            if b.get(j + 1) == Some(&q) {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    b.len()
}

/// `{quotecontinue}`: after a closing quote at `i - 1`, whitespace with a newline (comments
/// allowed) and another quote continue the literal. Returns that quote's index.
fn continuation(b: &[u8], mut i: usize) -> Option<usize> {
    let n = b.len();
    let comment = |mut i: usize| {
        while i < n && b[i] != b'\n' && b[i] != b'\r' {
            i += 1;
        }
        i
    };
    // {horiz_whitespace}*
    loop {
        match b.get(i) {
            Some(b' ' | b'\t' | b'\x0c') => i += 1,
            Some(b'-') if b.get(i + 1) == Some(&b'-') => i = comment(i),
            _ => break,
        }
    }
    // {newline}
    if !matches!(b.get(i), Some(b'\n' | b'\r')) {
        return None;
    }
    i += 1;
    // {special_whitespace}*
    loop {
        match b.get(i) {
            Some(b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'\x0b') => i += 1,
            Some(b'-') if b.get(i + 1) == Some(&b'-') => {
                let e = comment(i);
                if e >= n {
                    return None;
                }
                i = e + 1;
            }
            _ => break,
        }
    }
    (b.get(i) == Some(&b'\'')).then_some(i)
}

/// NAMEDATALEN - 1: the longest identifier, in bytes.
const MAX_IDENT: usize = 63;

/// `truncate_identifier`'s NOTICE for an identifier of `NAMEDATALEN` bytes or more (clipped
/// on a character boundary, `pg_mbcliplen`).
pub fn truncation_notice(ident: &str) -> Option<Error> {
    if ident.len() <= MAX_IDENT {
        return None;
    }
    let short = crate::catalog::truncate_ident(ident);
    let mut e = Error::new(
        code::NAME_TOO_LONG,
        format!("identifier \"{ident}\" will be truncated to \"{short}\""),
    );
    e.severity = Severity::Notice;
    Some(e)
}

/// The NOTICEs scan.l gives for overlength identifiers (`downcase_truncate_identifier` and
/// `truncate_identifier` with `warn`), unquoted, quoted and `U&"..."`, in token order, each
/// with the byte offset of its token. Tokens starting after byte `stop` (where a syntax error
/// stopped the parser) are not scanned.
pub(crate) fn long_identifiers(sql: &str, standard_strings: bool, stop: Option<usize>) -> Vec<(usize, Error)> {
    let b = sql.as_bytes();
    let n = b.len();
    let mut out = Vec::new();
    // Quick exit: no run of 64 bytes without a separator can hold an overlong identifier.
    if n <= MAX_IDENT {
        return out;
    }
    let is_ident_start = |c: u8| c.is_ascii_alphabetic() || c == b'_' || c >= 0x80;
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
    // Skip a '...' literal whose quote is at `q` (backslash escapes with `esc`); returns the
    // index after it (n: unterminated).
    let skip_string = |q: usize, esc: bool| -> usize {
        let mut j = q + 1;
        while j < n {
            match b[j] {
                b'\'' if b.get(j + 1) == Some(&b'\'') => j += 2,
                b'\'' => return j + 1,
                b'\\' if esc => j += 2,
                _ => j += 1,
            }
        }
        n
    };
    let mut i = 0;
    while i < n {
        if stop.is_some_and(|s| i > s) {
            break;
        }
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b'-' && next == Some(b'-') {
            while i < n && b[i] != b'\n' && b[i] != b'\r' {
                i += 1;
            }
        } else if c == b'/' && next == Some(b'*') {
            let mut depth = 0;
            while i < n {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if c == b'\'' {
            i = skip_string(i, !standard_strings);
        } else if matches!(c, b'e' | b'E') && next == Some(b'\'') {
            i = skip_string(i + 1, true);
        } else if matches!(c, b'n' | b'N') && next == Some(b'\'') {
            i = skip_string(i + 1, !standard_strings);
        } else if matches!(c, b'b' | b'B' | b'x' | b'X') && next == Some(b'\'') {
            i = skip_string(i + 1, false);
        } else if matches!(c, b'u' | b'U') && next == Some(b'&') && b.get(i + 2) == Some(&b'\'') {
            i = skip_string(i + 2, false);
        } else if c == b'"' || matches!(c, b'u' | b'U') && next == Some(b'&') && b.get(i + 2) == Some(&b'"') {
            let unicode = c != b'"';
            let q = if unicode { i + 2 } else { i };
            let end = skip_quoted(b, q, b'"');
            if end > n || b.get(end - 1) != Some(&b'"') || end - q < 2 {
                break;
            }
            let raw = sql[q + 1..end - 1].replace("\"\"", "\"");
            let ident = if unicode {
                let (esc, after) = uescape(sql, end);
                match decode_uident(&raw, esc) {
                    Some(s) => {
                        // (The parser truncates it once it has seen the UESCAPE clause.)
                        if let Some(e) = truncation_notice(&s) {
                            out.push((i, e));
                        }
                        i = after;
                        continue;
                    }
                    None => break,
                }
            } else {
                raw
            };
            if let Some(e) = truncation_notice(&ident) {
                out.push((i, e));
            }
            i = end;
        } else if c == b'$' {
            if next.is_some_and(|d| d.is_ascii_digit()) {
                i += 1;
                while i < n && b[i].is_ascii_digit() {
                    i += 1;
                }
                continue;
            }
            let mut j = i + 1;
            if j < n && is_ident_start(b[j]) {
                while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
                    j += 1;
                }
            }
            if b.get(j) != Some(&b'$') {
                i += 1;
                continue;
            }
            let tag = &sql[i..=j];
            match sql[j + 1..].find(tag) {
                Some(k) => i = j + 1 + k + tag.len(),
                None => break,
            }
        } else if is_ident_start(c) {
            let s = i;
            while i < n && is_ident(b[i]) {
                i += 1;
            }
            if i - s > MAX_IDENT {
                let ident = sql[s..i].to_ascii_lowercase();
                if let Some(e) = truncation_notice(&ident) {
                    out.push((s, e));
                }
            }
        } else if c.is_ascii_digit() || c == b'.' && next.is_some_and(|d| d.is_ascii_digit()) {
            i += 1;
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                if b[i] == b'.' && b.get(i + 1) == Some(&b'.') {
                    break;
                }
                // An exponent's sign.
                if matches!(b[i], b'e' | b'E') && matches!(b.get(i + 1), Some(b'+' | b'-')) {
                    i += 1;
                }
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    out
}

/// `warnings` (with character positions, from `scan`) and the overlength-identifier NOTICEs
/// of `sql` in the order the scanner raises them.
pub(crate) fn with_long_identifiers(
    sql: &str,
    standard_strings: bool,
    stop: Option<usize>,
    warnings: Vec<Error>,
) -> Vec<Error> {
    let idents = long_identifiers(sql, standard_strings, stop);
    if idents.is_empty() {
        return warnings;
    }
    let mut out = Vec::with_capacity(warnings.len() + idents.len());
    let mut ws = warnings.into_iter().peekable();
    for (at, e) in idents {
        let pos = sql[..at].chars().count() as u32 + 1;
        while let Some(w) = ws.next_if(|w| w.position.is_some_and(|p| p <= pos)) {
            out.push(w);
        }
        out.push(e);
    }
    out.extend(ws);
    out
}

/// The escape character of a `U&"..."` identifier ending before byte `end`: `\`, or the one
/// a following `UESCAPE 'c'` gives; and where scanning goes on.
fn uescape(sql: &str, end: usize) -> (char, usize) {
    let rest = &sql[end..];
    let t = rest.trim_start();
    let skipped = rest.len() - t.len();
    if t.as_bytes().get(..7).is_some_and(|p| p.eq_ignore_ascii_case(b"uescape")) {
        let u = t[7..].trim_start();
        let at = end + skipped + 7 + (t.len() - 7 - u.len());
        let mut ch = u.chars();
        if ch.next() == Some('\'')
            && let Some(e) = ch.next()
            && ch.next() == Some('\'')
        {
            return (e, at + 2 + e.len_utf8());
        }
    }
    ('\\', end)
}

/// A `U&"..."` identifier's text with its escapes (`\XXXX`, `\+XXXXXX`, `\\`) decoded; None
/// for an invalid escape (the parser reported it).
fn decode_uident(raw: &str, esc: char) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut it = raw.char_indices().peekable();
    while let Some((at, c)) = it.next() {
        if c != esc {
            out.push(c);
            continue;
        }
        let rest = &raw[at + c.len_utf8()..];
        if rest.starts_with(esc) {
            out.push(esc);
            it.next();
            continue;
        }
        let (digits, skip) = if let Some(r) = rest.strip_prefix('+') {
            (r.get(..6)?, 7)
        } else {
            (rest.get(..4)?, 4)
        };
        let cp = u32::from_str_radix(digits, 16).ok()?;
        out.push(char::from_u32(cp)?);
        for _ in 0..skip {
            it.next();
        }
    }
    Some(out)
}

/// `{param_junk}` (`$1a`): libpg_query is patched to read it as a parameter and an alias, but
/// Postgres's scanner rejects it with "trailing junk after parameter". Returns the byte range
/// of the first such token outside literals, identifiers and comments.
pub(crate) fn param_junk(sql: &str, standard_strings: bool) -> Option<(usize, usize)> {
    let b = sql.as_bytes();
    let n = b.len();
    if !b.windows(2).any(|w| w[0] == b'$' && w[1].is_ascii_digit()) {
        return None;
    }
    let is_ident_start = |c: u8| c.is_ascii_alphabetic() || c == b'_' || c >= 0x80;
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
    let skip_string = |q: usize, esc: bool| -> usize {
        let mut j = q + 1;
        while j < n {
            match b[j] {
                b'\'' if b.get(j + 1) == Some(&b'\'') => j += 2,
                b'\'' => return j + 1,
                b'\\' if esc => j += 2,
                _ => j += 1,
            }
        }
        n
    };
    let mut i = 0;
    while i < n {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c == b'-' && next == Some(b'-') {
            while i < n && b[i] != b'\n' && b[i] != b'\r' {
                i += 1;
            }
        } else if c == b'/' && next == Some(b'*') {
            let mut depth = 0;
            while i < n {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
        } else if c == b'\'' {
            i = skip_string(i, !standard_strings);
        } else if matches!(c, b'e' | b'E') && next == Some(b'\'') {
            i = skip_string(i + 1, true);
        } else if c == b'"' {
            i = skip_quoted(b, i, b'"');
        } else if c == b'$' {
            if next.is_some_and(|d| d.is_ascii_digit()) {
                let s = i;
                i += 1;
                while i < n && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i < n && is_ident_start(b[i]) {
                    while i < n && is_ident(b[i]) {
                        i += 1;
                    }
                    return Some((s, i));
                }
                continue;
            }
            let mut j = i + 1;
            if j < n && is_ident_start(b[j]) {
                while j < n && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
                    j += 1;
                }
            }
            if b.get(j) != Some(&b'$') {
                i += 1;
                continue;
            }
            let tag = &sql[i..=j];
            let k = sql[j + 1..].find(tag)?;
            i = j + 1 + k + tag.len();
        } else if is_ident_start(c) {
            // Whole identifiers and keywords: `x$1a` is one identifier.
            while i < n && is_ident(b[i]) {
                i += 1;
            }
        } else if c.is_ascii_digit() {
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'.') {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn off() -> LexOpts {
        LexOpts {
            standard_strings: false,
            ..LexOpts::default()
        }
    }

    #[test]
    fn rewrite_keeps_positions() {
        let s = scan("select 'a\\\\b', 'it\\'s', 'x\\ny', E'q\\\\', 1", off(), None);
        assert_eq!(
            s.rewritten.as_deref(),
            Some("select 'a\\b' , 'it''s', 'x\ny' , E'q\\\\', 1")
        );
        let pos: Vec<_> = s.warnings.iter().map(|w| w.position.unwrap()).collect();
        assert_eq!(pos, vec![8, 16, 25]);
    }

    #[test]
    fn quoted_body_positions() {
        let q = r"create function f() returns text language sql as 'select ''a\\b'''";
        assert_eq!(prosrc_position(q, r"select 'a\b'", 8), Some(58));
        assert_eq!(prosrc_position("as $$ select 1 $$", " select 1 ", 2), Some(7));
    }

    #[test]
    fn parser_positions_are_chars() {
        let e = crate::sql::parse::parse("select 'éé' frm x").unwrap_err();
        assert_eq!(e.position, Some(17));
    }

    #[test]
    fn continuation_lines() {
        let s = scan("select 'a' -- c\n'b\\\\'", off(), None);
        assert_eq!(s.rewritten.as_deref(), Some("select 'a' -- c\n'b\\' "));
        assert_eq!(s.warnings.len(), 1);
        assert_eq!(s.warnings[0].position, Some(8));
    }
}
