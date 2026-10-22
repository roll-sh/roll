//! The SQL lexer.

use crate::error::{Result, SelectError};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Tok {
    /// An unquoted identifier or keyword, as written.
    Word(String),
    /// A `"double quoted"` identifier.
    Quoted(String),
    /// A `'single quoted'` string.
    Str(String),
    Int(i64),
    Float(f64),
    /// A `` `backtick` `` Ion literal.
    Ion(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Star,
    Plus,
    Minus,
    Slash,
    Percent,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Concat,
    Semi,
    Eof,
}

#[derive(Clone, Debug)]
pub(crate) struct Token {
    pub tok: Tok,
    /// Byte offset in the expression.
    pub pos: usize,
}

/// "at line L, column C" for a byte offset (1-based, in characters).
pub(crate) fn location(src: &str, pos: usize) -> String {
    let before = &src[..pos.min(src.len())];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    format!("at line {line}, column {col}")
}

pub(crate) fn lex(src: &str) -> Result<Vec<Token>> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let err = |code: &'static str, what: &str, at: usize| SelectError::new(code, format!("{what} {}.", location(src, at)));
    while i < b.len() {
        let c = b[i];
        let start = i;
        let tok = match c {
            b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' => {
                i += 1;
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let end = src[i + 2..].find("*/").ok_or_else(|| err("LexerInvalidChar", "Unterminated comment", start))?;
                i += 2 + end + 2;
                continue;
            }
            b'(' => Tok::LParen,
            b')' => Tok::RParen,
            b'[' => Tok::LBracket,
            b']' => Tok::RBracket,
            b',' => Tok::Comma,
            b'*' => Tok::Star,
            b'+' => Tok::Plus,
            b'-' => Tok::Minus,
            b'/' => Tok::Slash,
            b'%' => Tok::Percent,
            b';' => Tok::Semi,
            b'=' => {
                if b.get(i + 1) == Some(&b'=') {
                    i += 1;
                }
                Tok::Eq
            }
            b'!' if b.get(i + 1) == Some(&b'=') => {
                i += 1;
                Tok::Ne
            }
            b'<' => match b.get(i + 1) {
                Some(b'=') => {
                    i += 1;
                    Tok::Le
                }
                Some(b'>') => {
                    i += 1;
                    Tok::Ne
                }
                _ => Tok::Lt,
            },
            b'>' => {
                if b.get(i + 1) == Some(&b'=') {
                    i += 1;
                    Tok::Ge
                } else {
                    Tok::Gt
                }
            }
            b'|' if b.get(i + 1) == Some(&b'|') => {
                i += 1;
                Tok::Concat
            }
            b'\'' | b'"' => {
                let (s, end) = quoted(src, i, c).ok_or_else(|| {
                    err("LexerInvalidLiteral", if c == b'"' { "Unterminated quoted identifier" } else { "Unterminated string" }, start)
                })?;
                out.push(Token { tok: if c == b'"' { Tok::Quoted(s) } else { Tok::Str(s) }, pos: start });
                i = end;
                continue;
            }
            b'`' => {
                let end = src[i + 1..].find('`').ok_or_else(|| err("LexerInvalidIONLiteral", "Unterminated Ion literal", start))?;
                out.push(Token { tok: Tok::Ion(src[i + 1..i + 1 + end].to_string()), pos: start });
                i += end + 2;
                continue;
            }
            b'.' if !b.get(i + 1).is_some_and(|d| d.is_ascii_digit()) => Tok::Dot,
            b'0'..=b'9' | b'.' => {
                let mut j = i;
                while j < b.len() && b[j].is_ascii_digit() {
                    j += 1;
                }
                let mut float = false;
                if j < b.len() && b[j] == b'.' && b.get(j + 1).is_none_or(|d| d.is_ascii_digit() || !d.is_ascii_alphabetic()) {
                    float = true;
                    j += 1;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                }
                if j < b.len() && (b[j] == b'e' || b[j] == b'E') {
                    let mut k = j + 1;
                    if k < b.len() && (b[k] == b'+' || b[k] == b'-') {
                        k += 1;
                    }
                    if k < b.len() && b[k].is_ascii_digit() {
                        while k < b.len() && b[k].is_ascii_digit() {
                            k += 1;
                        }
                        float = true;
                        j = k;
                    }
                }
                if j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    return Err(err("LexerInvalidLiteral", "Invalid number", start));
                }
                let text = &src[i..j];
                i = j;
                let tok = if float {
                    Tok::Float(text.parse().map_err(|_| err("LexerInvalidLiteral", "Invalid number", start))?)
                } else {
                    match text.parse::<i64>() {
                        Ok(v) => Tok::Int(v),
                        Err(_) => Tok::Float(text.parse().map_err(|_| err("LexerInvalidLiteral", "Invalid number", start))?),
                    }
                };
                out.push(Token { tok, pos: start });
                continue;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let mut j = i;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'$') {
                    j += 1;
                }
                out.push(Token { tok: Tok::Word(src[i..j].to_string()), pos: start });
                i = j;
                continue;
            }
            b'!' | b'|' | b'&' | b'^' | b'~' => return Err(err("LexerInvalidOperator", "Invalid operator", start)),
            _ => {
                let ch = src[i..].chars().next().unwrap_or('?');
                return Err(err("LexerInvalidChar", &format!("Invalid character {ch:?}"), start));
            }
        };
        i += 1;
        out.push(Token { tok, pos: start });
    }
    out.push(Token { tok: Tok::Eof, pos: src.len() });
    Ok(out)
}

/// A quoted token starting at `i` with quote `q`; a doubled quote is a literal quote. Returns the
/// content and the offset after the closing quote.
fn quoted(src: &str, i: usize, q: u8) -> Option<(String, usize)> {
    let b = src.as_bytes();
    let mut s = String::new();
    let mut j = i + 1;
    loop {
        let k = j + b[j..].iter().position(|&c| c == q)?;
        s.push_str(&src[j..k]);
        if b.get(k + 1) == Some(&q) {
            s.push(q as char);
            j = k + 2;
        } else {
            return Some((s, k + 1));
        }
    }
}

/// S3 Select's reserved keywords (they must be double-quoted to be used as names).
const RESERVED: &[&str] = &[
    "absolute", "action", "add", "all", "allocate", "alter", "and", "any", "are", "as", "asc", "assertion", "at",
    "authorization", "avg", "bag", "begin", "between", "bit", "bit_length", "blob", "bool", "boolean", "both", "by",
    "cascade", "cascaded", "case", "cast", "catalog", "char", "char_length", "character", "character_length", "check",
    "clob", "close", "coalesce", "collate", "collation", "column", "commit", "connect", "connection", "constraint",
    "constraints", "continue", "convert", "corresponding", "count", "create", "cross", "current", "current_date",
    "current_time", "current_timestamp", "current_user", "cursor", "date", "day", "deallocate", "dec", "decimal",
    "declare", "default", "deferrable", "deferred", "delete", "desc", "describe", "descriptor", "diagnostics",
    "disconnect", "distinct", "domain", "double", "drop", "else", "end", "end-exec", "escape", "except", "exception",
    "exec", "execute", "exists", "external", "extract", "false", "fetch", "first", "float", "for", "foreign", "found",
    "from", "full", "get", "global", "go", "goto", "grant", "group", "having", "hour", "identity", "immediate", "in",
    "indicator", "initially", "inner", "input", "insensitive", "insert", "int", "integer", "intersect", "interval",
    "into", "is", "isolation", "join", "key", "language", "last", "leading", "left", "level", "like", "limit", "list",
    "local", "lower", "match", "max", "min", "minute", "missing", "module", "month", "names", "national", "natural",
    "nchar", "next", "no", "not", "null", "nullif", "numeric", "octet_length", "of", "on", "only", "open", "option",
    "or", "order", "outer", "output", "overlaps", "pad", "partial", "pivot", "position", "precision", "prepare",
    "preserve", "primary", "prior", "privileges", "procedure", "public", "read", "real", "references", "relative",
    "restrict", "revoke", "right", "rollback", "rows", "schema", "scroll", "second", "section", "select", "session",
    "session_user", "set", "sexp", "size", "smallint", "some", "space", "sql", "sqlcode", "sqlerror", "sqlstate",
    "string", "struct", "substring", "sum", "symbol", "system_user", "table", "temporary", "then", "time", "timestamp",
    "timezone_hour", "timezone_minute", "to", "trailing", "transaction", "translate", "translation", "trim", "true",
    "tuple", "union", "unique", "unknown", "unpivot", "update", "upper", "usage", "user", "using", "value", "values",
    "varchar", "varying", "view", "when", "whenever", "where", "with", "work", "write", "year", "zone",
];

pub(crate) fn is_reserved(word: &str) -> bool {
    let w = word.to_ascii_lowercase();
    RESERVED.binary_search(&w.as_str()).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &str) -> Vec<Tok> {
        lex(s).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn reserved_list_is_sorted() {
        assert!(RESERVED.windows(2).all(|w| w[0] < w[1]));
        assert!(is_reserved("SELECT") && is_reserved("Cast") && !is_reserved("s3object") && !is_reserved("utcnow"));
    }

    #[test]
    fn tokens() {
        assert_eq!(
            toks("SELECT s._1, 'it''s', \"a \"\"b\"\"\" FROM S3Object[*] -- c\n"),
            vec![
                Tok::Word("SELECT".into()),
                Tok::Word("s".into()),
                Tok::Dot,
                Tok::Word("_1".into()),
                Tok::Comma,
                Tok::Str("it's".into()),
                Tok::Comma,
                Tok::Quoted("a \"b\"".into()),
                Tok::Word("FROM".into()),
                Tok::Word("S3Object".into()),
                Tok::LBracket,
                Tok::Star,
                Tok::RBracket,
                Tok::Eof
            ]
        );
        assert_eq!(
            toks("1 2.5 .5 1e3 3. a<>b != <= >= || == /* x */ `2010T`"),
            vec![
                Tok::Int(1),
                Tok::Float(2.5),
                Tok::Float(0.5),
                Tok::Float(1000.0),
                Tok::Float(3.0),
                Tok::Word("a".into()),
                Tok::Ne,
                Tok::Word("b".into()),
                Tok::Ne,
                Tok::Le,
                Tok::Ge,
                Tok::Concat,
                Tok::Eq,
                Tok::Ion("2010T".into()),
                Tok::Eof
            ]
        );
        assert_eq!(toks("99999999999999999999"), vec![Tok::Float(1e20), Tok::Eof]);
        assert_eq!(toks("x[0].y"), vec![Tok::Word("x".into()), Tok::LBracket, Tok::Int(0), Tok::RBracket, Tok::Dot, Tok::Word("y".into()), Tok::Eof]);
    }

    #[test]
    fn errors() {
        assert_eq!(lex("select #").unwrap_err().code, "LexerInvalidChar");
        assert_eq!(lex("select 'abc").unwrap_err().code, "LexerInvalidLiteral");
        assert_eq!(lex("select \"abc").unwrap_err().code, "LexerInvalidLiteral");
        assert_eq!(lex("select 12abc").unwrap_err().code, "LexerInvalidLiteral");
        assert_eq!(lex("select a ! b").unwrap_err().code, "LexerInvalidOperator");
        assert_eq!(lex("select `abc").unwrap_err().code, "LexerInvalidIONLiteral");
        let e = lex("select\n  @").unwrap_err();
        assert!(e.message.contains("line 2, column 3"), "{}", e.message);
    }
}
