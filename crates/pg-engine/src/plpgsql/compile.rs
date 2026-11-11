//! Compiling a PL/pgSQL body the way Postgres does (`pl_gram.y`, `pl_scanner.c`,
//! `pl_comp.c`): the errors its grammar raises, with the same messages, positions and order.
//!
//! The body is scanned like the core scanner does, with the PL/pgSQL keyword lists, and
//! identifiers are looked up in a namespace of the variables declared so far, so assignment
//! and INTO targets, cursors, labels and declarations are checked as Postgres checks them. SQL
//! expressions and statements are syntax-checked with the core parser when `check_syntax` is
//! on (validation, and DO blocks with check_function_bodies). The executable tree is still
//! built by `parse`; this only reports what Postgres reports while compiling.
//!
//! Error positions are 1-based byte offsets into the body. An error without one gets the line
//! compiling had got to (`plpgsql_latest_lineno`), returned alongside it.

use crate::error::{Error, Result, Severity, code};

const RESERVED: &[&str] = &[
    "all", "begin", "by", "case", "declare", "else", "end", "execute", "for", "foreach", "from",
    "if", "in", "into", "loop", "not", "null", "or", "strict", "then", "to", "using", "when",
    "while",
];

const UNRESERVED: &[&str] = &[
    "absolute",
    "alias",
    "and",
    "array",
    "assert",
    "backward",
    "call",
    "chain",
    "close",
    "collate",
    "column",
    "column_name",
    "commit",
    "constant",
    "constraint",
    "constraint_name",
    "continue",
    "current",
    "cursor",
    "datatype",
    "debug",
    "default",
    "detail",
    "diagnostics",
    "do",
    "dump",
    "elseif",
    "elsif",
    "errcode",
    "error",
    "exception",
    "exit",
    "fetch",
    "first",
    "forward",
    "get",
    "hint",
    "import",
    "info",
    "insert",
    "is",
    "last",
    "log",
    "merge",
    "message",
    "message_text",
    "move",
    "next",
    "no",
    "notice",
    "open",
    "option",
    "perform",
    "pg_context",
    "pg_datatype_name",
    "pg_exception_context",
    "pg_exception_detail",
    "pg_exception_hint",
    "pg_routine_oid",
    "print_strict_params",
    "prior",
    "query",
    "raise",
    "relative",
    "return",
    "returned_sqlstate",
    "reverse",
    "rollback",
    "row_count",
    "rowtype",
    "schema",
    "schema_name",
    "scroll",
    "slice",
    "sqlstate",
    "stacked",
    "table",
    "table_name",
    "type",
    "use_column",
    "use_variable",
    "variable_conflict",
    "warning",
];

/// A token's code (`yylex`'s result).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum K {
    Eof,
    /// The core scanner's IDENT / PARAM, before PL/pgSQL looks it up.
    Ident,
    Param,
    /// T_WORD, T_CWORD, T_DATUM.
    Word,
    CWord,
    Datum,
    /// A PL/pgSQL keyword (reserved, or unreserved and not a variable here).
    Kw(&'static str),
    Sconst,
    Bconst,
    Xconst,
    Iconst,
    Fconst,
    Op,
    Char(u8),
    ColonEquals,
    EqualsGreater,
    DotDot,
    Typecast,
    LessLess,
    GreaterGreater,
    LessEquals,
    GreaterEquals,
    NotEquals,
}

#[derive(Clone, Debug)]
struct Tok {
    k: K,
    loc: usize,
    len: usize,
    /// IDENT / T_WORD / keyword: the (downcased) word; SCONST: its value; PARAM: `$n`.
    s: String,
    quoted: bool,
    /// T_CWORD / T_DATUM of more than one name.
    idents: Vec<String>,
    /// T_DATUM: the datum.
    dno: usize,
}

impl Tok {
    fn new(k: K, loc: usize, len: usize) -> Tok {
        Tok {
            k,
            loc,
            len,
            s: String::new(),
            quoted: false,
            idents: Vec::new(),
            dno: 0,
        }
    }

    fn is(&self, c: u8) -> bool {
        self.k == K::Char(c)
    }

    fn kw(&self, kw: &str) -> bool {
        matches!(self.k, K::Kw(k) if k == kw)
    }

    /// `tok_is_keyword`: the keyword, or a variable spelled like it.
    fn is_kw(&self, kw: &str) -> bool {
        self.kw(kw) || self.k == K::Datum && !self.quoted && self.idents.is_empty() && self.s == kw
    }

    fn unreserved(&self) -> bool {
        matches!(self.k, K::Kw(k) if !RESERVED.contains(&k))
    }

    /// NameOfDatum / NameListToString.
    fn name(&self) -> String {
        if self.idents.is_empty() {
            self.s.clone()
        } else {
            self.idents.join(".")
        }
    }
}

/// What the scanner does with identifiers (`plpgsql_IdentifierLookup`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lookup {
    Normal,
    Declare,
    Expr,
}

/// How an expression is handed to the core parser (`RawParseMode`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Default,
    Expr,
    Assign,
}

/// A declared type, as far as compiling cares.
#[derive(Clone, Debug)]
pub struct VarType {
    pub kind: TypeKind,
    pub refcursor: bool,
    pub collatable: bool,
    /// `format_type_be`, for messages.
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TypeKind {
    Scalar,
    /// A composite or `record`: a record variable.
    Rec,
    Pseudo,
}

impl VarType {
    pub fn scalar(name: &str) -> VarType {
        VarType {
            kind: TypeKind::Scalar,
            refcursor: false,
            collatable: false,
            name: name.into(),
        }
    }

    fn record() -> VarType {
        VarType {
            kind: TypeKind::Rec,
            refcursor: false,
            collatable: false,
            name: "record".into(),
        }
    }
}

/// A type a declaration names.
pub enum TypeRef<'a> {
    /// A SQL type name, as written.
    Name(&'a str),
    /// `table%ROWTYPE` (one or more names).
    Row(&'a [String]),
    /// `table.column%TYPE` (the last name is the column).
    Column(&'a [String]),
}

/// What compiling needs from the catalog, and where its warnings go.
pub trait Env {
    /// Resolve a declared type. Errors for `TypeRef::Name` may carry a 1-based position into
    /// the name.
    fn resolve(&mut self, t: TypeRef) -> Result<VarType>;
    /// Check a COLLATE name exists.
    fn collation(&mut self, names: &[String]) -> Result<()>;
    /// A warning (extra_warnings), with its body position.
    fn warning(&mut self, e: Error);
}

/// A parameter of the function.
#[derive(Clone, Debug)]
pub struct Param {
    pub name: Option<String>,
    pub ty: VarType,
    /// `i`, `o`, `b`, `v`, `t` (proargmodes).
    pub mode: char,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    None,
    Dml,
    Event,
}

/// The function being compiled.
pub struct Spec<'a> {
    pub name: &'a str,
    pub params: Vec<Param>,
    pub set: bool,
    pub void: bool,
    pub procedure: bool,
    /// A polymorphic result without OUT parameters: `$0` is declared.
    pub dollar_zero: Option<VarType>,
    pub trigger: Trigger,
    /// Syntax-check embedded SQL (`plpgsql_check_syntax`).
    pub check_syntax: bool,
    /// extra_warnings / extra_errors `shadowed_variables`: `Some(true)` for an error.
    pub shadow: Option<bool>,
}

/// What a successful compile found in the body's options.
#[derive(Default, Debug)]
pub struct Options {
    pub print_strict_params: Option<bool>,
}

/// A compile error and the line compiling had got to.
pub struct Failed {
    pub err: Error,
    pub line: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LabelKind {
    Block,
    Loop,
    Other,
}

enum Ns {
    Label(String, LabelKind),
    /// A name for a datum; `rec`: it names a record (`PLPGSQL_NSTYPE_REC`).
    Var(String, usize, bool),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DKind {
    Var,
    Promise,
    Rec,
    Row,
    RecField(usize),
}

struct Datum {
    kind: DKind,
    name: String,
    isconst: bool,
    ty: Option<VarType>,
    /// A bound cursor: its parameter names (`None`: not bound).
    cursor: Option<Vec<String>>,
}

pub fn compile(
    src: &str,
    spec: &Spec,
    env: &mut dyn Env,
) -> std::result::Result<Options, Box<Failed>> {
    let mut c = Compiler {
        src,
        pos: 0,
        pushback: Vec::new(),
        prev: K::Eof,
        lookup: Lookup::Normal,
        ns: Vec::new(),
        datums: Vec::new(),
        spec,
        env,
        line: 1,
        opts: Options::default(),
        warned: 0,
    };
    match c.function() {
        Ok(()) => Ok(c.opts),
        Err(err) => Err(Box::new(Failed {
            err,
            line: c.line,
        })),
    }
}

struct Compiler<'s, 'e> {
    src: &'s str,
    pos: usize,
    pushback: Vec<Tok>,
    /// The last token returned (`plpgsql_yytoken`).
    prev: K,
    lookup: Lookup,
    ns: Vec<Ns>,
    datums: Vec<Datum>,
    spec: &'e Spec<'e>,
    env: &'e mut dyn Env,
    /// `plpgsql_latest_lineno`.
    line: u32,
    opts: Options,
    /// Where the literals the scanner warned about end (tokens can be scanned again).
    warned: usize,
}

fn syntax(msg: impl Into<String>) -> Error {
    Error::new(code::SYNTAX_ERROR, msg)
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_ident_cont(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

const OP_CHARS: &[u8] = b"~!@#^&|`?+-*/%<>=";
const SELF_CHARS: &[u8] = b",()[].;:+-*/%^<>=";

impl Compiler<'_, '_> {
    // ── scanning ──

    fn lineno(&mut self, loc: usize) {
        self.line = self.src[..loc.min(self.src.len())].matches('\n').count() as u32 + 1;
    }

    fn text(&self, t: &Tok) -> &str {
        &self.src[t.loc..(t.loc + t.len).min(self.src.len())]
    }

    /// `plpgsql_yyerror`.
    fn yyerror(&self, t: &Tok, msg: &str) -> Error {
        if t.k == K::Eof {
            syntax(format!("{msg} at end of input")).position(t.loc as u32 + 1)
        } else {
            syntax(format!("{msg} at or near \"{}\"", self.text(t))).position(t.loc as u32 + 1)
        }
    }

    fn err_at(&self, code: &'static str, msg: impl Into<String>, loc: usize) -> Error {
        Error::new(code, msg).position(loc as u32 + 1)
    }

    /// A core scanner error (`scanner_yyerror`): the rest of the input from `at`.
    fn scan_error(&self, msg: &str, at: usize, end: Option<usize>) -> Error {
        let text = &self.src[at..end.unwrap_or(self.src.len())];
        if text.is_empty() {
            syntax(format!("{msg} at end of input")).position(at as u32 + 1)
        } else {
            syntax(format!("{msg} at or near \"{text}\"")).position(at as u32 + 1)
        }
    }

    /// The core scanner (`core_yylex`) with the PL/pgSQL reserved keywords.
    fn core(&mut self) -> Result<Tok> {
        let src = self.src;
        let b = src.as_bytes();
        let mut i = self.pos;
        loop {
            while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) {
                i += 1;
            }
            if b[i..].starts_with(b"--") {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if b[i..].starts_with(b"/*") {
                let start = i;
                let mut depth = 0;
                while i < b.len() {
                    if b[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if b[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                if depth != 0 {
                    self.pos = b.len();
                    return Err(self.scan_error("unterminated /* comment", start, None));
                }
                continue;
            }
            break;
        }
        let start = i;
        if i >= b.len() {
            self.pos = i;
            return Ok(Tok::new(K::Eof, b.len(), 0));
        }
        let c = b[i];
        let tok = |k: K, end: usize| Tok::new(k, start, end - start);
        // Prefixed literals.
        let lower = c.to_ascii_lowercase();
        if matches!(lower, b'e' | b'b' | b'x') && b.get(i + 1) == Some(&b'\'') {
            let (k, msg) = match lower {
                b'e' => (K::Sconst, "unterminated quoted string"),
                b'b' => (K::Bconst, "unterminated bit string literal"),
                _ => (K::Xconst, "unterminated hexadecimal string literal"),
            };
            let end = self.quoted(start, i + 1, b'\'', lower == b'e', msg)?;
            let mut t = tok(k, end);
            t.s = super::parse::string_value(&src[start..end]);
            self.pos = end;
            return Ok(t);
        }
        if lower == b'u' && b.get(i + 1) == Some(&b'&') && matches!(b.get(i + 2), Some(b'\'' | b'"'))
        {
            let q = b[i + 2];
            let msg = if q == b'"' {
                "unterminated quoted identifier"
            } else {
                "unterminated quoted string"
            };
            let end = self.quoted(start, i + 2, q, false, msg)?;
            let mut t = tok(if q == b'"' { K::Ident } else { K::Sconst }, end);
            t.s = src[i + 3..end - 1].replace(if q == b'"' { "\"\"" } else { "''" }, if q == b'"' { "\"" } else { "'" });
            t.quoted = q == b'"';
            self.pos = end;
            return Ok(t);
        }
        if lower == b'n' && b.get(i + 1) == Some(&b'\'') {
            // NCHAR: the keyword, then the literal.
            let mut t = tok(K::Ident, i + 1);
            t.s = "nchar".into();
            self.pos = i + 1;
            return Ok(t);
        }
        if is_ident_start(c) {
            let mut j = i;
            while j < b.len() && is_ident_cont(b[j]) {
                j += 1;
            }
            let word = src[i..j].to_ascii_lowercase();
            self.pos = j;
            if let Some(kw) = RESERVED.iter().find(|k| **k == word) {
                let mut t = tok(K::Kw(kw), j);
                t.s = word;
                return Ok(t);
            }
            let mut t = tok(K::Ident, j);
            t.s = word;
            return Ok(t);
        }
        if c.is_ascii_digit() || c == b'.' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
            return self.number(start);
        }
        match c {
            b'\'' => {
                let standard = crate::sql::scs::opts().standard_strings;
                let end = self.quoted(start, i, b'\'', !standard, "unterminated quoted string")?;
                // The core scanner's warning for backslashes, once per literal scanned.
                if !standard && start >= self.warned {
                    self.warned = end;
                    if let Some(mut w) = crate::sql::scs::literal_warning(&src[start..end]) {
                        let p = w.position.unwrap_or(1) as usize;
                        let off = src[start..end].char_indices().nth(p - 1).map_or(0, |(b, _)| b);
                        w.position = Some((start + off) as u32 + 1);
                        // (Dropped when the position goes into a CREATE statement.)
                        w.context = Some(
                            format!(
                                "compilation of PL/pgSQL function \"{}\" near line {}",
                                self.spec.name, self.line
                            )
                            .into(),
                        );
                        self.env.warning(w);
                    }
                }
                let mut t = tok(K::Sconst, end);
                t.s = super::parse::string_value(&src[start..end]);
                self.pos = end;
                Ok(t)
            }
            b'"' => {
                let end = self.quoted(start, i, b'"', false, "unterminated quoted identifier")?;
                if end == i + 2 {
                    self.pos = end;
                    return Err(self.scan_error("zero-length delimited identifier", i, Some(end)));
                }
                let mut t = tok(K::Ident, end);
                t.s = src[i + 1..end - 1].replace("\"\"", "\"");
                t.quoted = true;
                self.pos = end;
                Ok(t)
            }
            b'$' => {
                if b.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
                    let mut j = i + 1;
                    while j < b.len() && b[j].is_ascii_digit() {
                        j += 1;
                    }
                    if j < b.len() && is_ident_start(b[j]) {
                        let mut k = j;
                        while k < b.len() && is_ident_cont(b[k]) {
                            k += 1;
                        }
                        self.pos = k;
                        return Err(self.scan_error("trailing junk after parameter", i, Some(k)));
                    }
                    let mut t = tok(K::Param, j);
                    t.s = src[i..j].to_string();
                    self.pos = j;
                    return Ok(t);
                }
                // $tag$ ... $tag$
                let mut j = i + 1;
                if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_' || b[j] >= 0x80) {
                    while j < b.len()
                        && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80)
                    {
                        j += 1;
                    }
                }
                if b.get(j) != Some(&b'$') {
                    self.pos = i + 1;
                    return Ok(tok(K::Char(b'$'), i + 1));
                }
                let tag = &src[i..=j];
                match src[j + 1..].find(tag) {
                    Some(k) => {
                        let end = j + 1 + k + tag.len();
                        let mut t = tok(K::Sconst, end);
                        t.s = src[j + 1..j + 1 + k].to_string();
                        self.pos = end;
                        Ok(t)
                    }
                    None => {
                        self.pos = b.len();
                        Err(self.scan_error("unterminated dollar-quoted string", i, None))
                    }
                }
            }
            b':' if b.get(i + 1) == Some(&b'=') => {
                self.pos = i + 2;
                Ok(tok(K::ColonEquals, i + 2))
            }
            b':' if b.get(i + 1) == Some(&b':') => {
                self.pos = i + 2;
                Ok(tok(K::Typecast, i + 2))
            }
            b'.' if b.get(i + 1) == Some(&b'.') => {
                self.pos = i + 2;
                Ok(tok(K::DotDot, i + 2))
            }
            _ if OP_CHARS.contains(&c) => {
                let mut j = i;
                while j < b.len() && OP_CHARS.contains(&b[j]) {
                    j += 1;
                }
                let mut n = j - i;
                let op = &b[i..j];
                for k in 1..op.len() {
                    if op[k..].starts_with(b"/*") || op[k..].starts_with(b"--") {
                        n = k;
                        break;
                    }
                }
                if n > 1
                    && matches!(op[n - 1], b'+' | b'-')
                    && !op[..n].iter().any(|c| b"~!@#^&|`?%".contains(c))
                {
                    while n > 1 && matches!(op[n - 1], b'+' | b'-') {
                        n -= 1;
                    }
                }
                let end = i + n;
                self.pos = end;
                let text = &src[i..end];
                let k = match text {
                    t if t.len() == 1 && SELF_CHARS.contains(&c) => K::Char(c),
                    "=>" => K::EqualsGreater,
                    ">=" => K::GreaterEquals,
                    "<=" => K::LessEquals,
                    "<>" | "!=" => K::NotEquals,
                    "<<" => K::LessLess,
                    ">>" => K::GreaterGreater,
                    "#" => K::Char(b'#'),
                    _ => K::Op,
                };
                let mut t = tok(k, end);
                t.s = text.to_string();
                Ok(t)
            }
            _ => {
                let len = src[i..].chars().next().map_or(1, char::len_utf8);
                self.pos = i + len;
                Ok(tok(K::Char(c), i + len))
            }
        }
    }

    /// Skip a quoted literal or identifier whose quote is at `i` (the token starts at
    /// `start`, before any prefix); returns the index after it.
    fn quoted(&mut self, start: usize, i: usize, q: u8, backslash: bool, msg: &str) -> Result<usize> {
        let b = self.src.as_bytes();
        let mut j = i + 1;
        while j < b.len() {
            if backslash && b[j] == b'\\' {
                j += 2;
                continue;
            }
            if b[j] == q {
                if b.get(j + 1) == Some(&q) {
                    j += 2;
                    continue;
                }
                return Ok(j + 1);
            }
            j += 1;
        }
        self.pos = b.len();
        Err(self.scan_error(msg, start, None))
    }

    fn number(&mut self, start: usize) -> Result<Tok> {
        let b = self.src.as_bytes();
        let digits = |mut j: usize, ok: &dyn Fn(u8) -> bool| {
            while j < b.len() {
                if ok(b[j]) {
                    j += 1;
                } else if b[j] == b'_' && b.get(j + 1).is_some_and(|c| ok(*c)) {
                    j += 2;
                } else {
                    break;
                }
            }
            j
        };
        let mut j = start;
        let mut k = K::Iconst;
        let radix = (b[j] == b'0')
            .then(|| b.get(j + 1).map(|c| c.to_ascii_lowercase()))
            .flatten()
            .filter(|c| matches!(c, b'x' | b'o' | b'b'));
        if let Some(r) = radix {
            let ok: &dyn Fn(u8) -> bool = match r {
                b'x' => &|c: u8| c.is_ascii_hexdigit(),
                b'o' => &|c: u8| (b'0'..=b'7').contains(&c),
                _ => &|c: u8| c == b'0' || c == b'1',
            };
            let mut e = j + 2;
            if e < b.len() && b[e] == b'_' {
                e += 1;
            }
            j = digits(e, ok);
        } else {
            j = digits(j, &|c: u8| c.is_ascii_digit());
            if j < b.len() && b[j] == b'.' && b.get(j + 1) != Some(&b'.') {
                k = K::Fconst;
                j = digits(j + 1, &|c: u8| c.is_ascii_digit());
            }
            if j < b.len() && matches!(b[j], b'e' | b'E') {
                let mut e = j + 1;
                if e < b.len() && matches!(b[e], b'+' | b'-') {
                    e += 1;
                }
                if e < b.len() && b[e].is_ascii_digit() {
                    k = K::Fconst;
                    j = digits(e, &|c: u8| c.is_ascii_digit());
                } else if e > j + 1 {
                    self.pos = e;
                    return Err(self.scan_error("trailing junk after numeric literal", start, Some(e)));
                }
            }
        }
        if j < b.len() && is_ident_start(b[j]) {
            let mut e = j;
            while e < b.len() && is_ident_cont(b[e]) {
                e += 1;
            }
            self.pos = e;
            return Err(self.scan_error("trailing junk after numeric literal", start, Some(e)));
        }
        self.pos = j;
        let mut t = Tok::new(k, start, j - start);
        t.s = self.src[start..j].to_string();
        Ok(t)
    }

    /// `internal_yylex`: a pushed-back token, or the next one.
    fn internal(&mut self) -> Result<Tok> {
        match self.pushback.pop() {
            Some(t) => Ok(t),
            None => self.core(),
        }
    }

    fn push_back(&mut self, t: Tok) {
        self.pushback.push(t);
    }

    /// `plpgsql_peek`: the next token's code, left to be read.
    fn peek(&mut self) -> Result<K> {
        let t = self.internal()?;
        let k = t.k;
        self.push_back(t);
        Ok(k)
    }

    fn at_stmt_start(&self) -> bool {
        matches!(self.prev, K::Char(b';'))
            || matches!(self.prev, K::Kw("begin" | "then" | "else" | "loop"))
    }

    /// `plpgsql_yylex`: compound names and variables resolved.
    fn yylex(&mut self) -> Result<Tok> {
        let mut t1 = self.internal()?;
        if matches!(t1.k, K::Ident | K::Param) {
            let t2 = self.internal()?;
            if t2.is(b'.') {
                let t3 = self.internal()?;
                if t3.k == K::Ident {
                    let t4 = self.internal()?;
                    if t4.is(b'.') {
                        let t5 = self.internal()?;
                        if t5.k == K::Ident {
                            let names = [t1.s.clone(), t3.s.clone(), t5.s.clone()];
                            self.tripword(&mut t1, &names);
                            t1.len = t5.loc - t1.loc + t5.len;
                        } else {
                            self.push_back(t5);
                            self.push_back(t4);
                            let names = [t1.s.clone(), t3.s.clone()];
                            self.dblword(&mut t1, &names);
                            t1.len = t3.loc - t1.loc + t3.len;
                        }
                    } else {
                        self.push_back(t4);
                        let names = [t1.s.clone(), t3.s.clone()];
                        self.dblword(&mut t1, &names);
                        t1.len = t3.loc - t1.loc + t3.len;
                    }
                } else {
                    self.push_back(t3);
                    self.push_back(t2);
                    self.word(&mut t1, true);
                }
            } else {
                let lookup = !self.at_stmt_start()
                    || matches!(t2.k, K::Char(b'=') | K::ColonEquals | K::Char(b'['));
                self.push_back(t2);
                self.word(&mut t1, lookup);
            }
        }
        self.prev = t1.k;
        Ok(t1)
    }

    /// `plpgsql_parse_word` and the unreserved keyword lookup.
    fn word(&mut self, t: &mut Tok, lookup: bool) {
        if lookup
            && self.lookup == Lookup::Normal
            && let Some((dno, _, _)) = self.ns_lookup(false, &t.s, None, None)
        {
            t.k = K::Datum;
            t.dno = dno;
            return;
        }
        if t.k == K::Ident
            && !t.quoted
            && let Some(kw) = UNRESERVED.iter().find(|k| **k == t.s)
        {
            t.k = K::Kw(if *kw == "elseif" { "elsif" } else { kw });
            return;
        }
        t.k = K::Word;
    }

    fn dblword(&mut self, t: &mut Tok, names: &[String; 2]) {
        t.idents = names.to_vec();
        if self.lookup != Lookup::Declare
            && let Some((dno, rec, used)) = self.ns_lookup(false, &names[0], Some(&names[1]), None)
        {
            t.k = K::Datum;
            t.dno = if rec && used == 1 {
                self.recfield(dno, &names[1])
            } else {
                dno
            };
            return;
        }
        t.k = K::CWord;
    }

    fn tripword(&mut self, t: &mut Tok, names: &[String; 3]) {
        t.idents = names.to_vec();
        if self.lookup != Lookup::Declare
            && let Some((dno, true, used)) =
                self.ns_lookup(false, &names[0], Some(&names[1]), Some(&names[2]))
        {
            t.k = K::Datum;
            if used == 1 {
                t.idents.truncate(2);
                t.dno = self.recfield(dno, &names[1]);
            } else {
                t.dno = self.recfield(dno, &names[2]);
            }
            return;
        }
        t.k = K::CWord;
    }

    // ── namespace ──

    fn ns_push(&mut self, label: Option<&str>, kind: LabelKind) {
        self.ns.push(Ns::Label(label.unwrap_or("").to_string(), kind));
    }

    fn ns_pop(&mut self) {
        while let Some(n) = self.ns.pop() {
            if matches!(n, Ns::Label(..)) {
                break;
            }
        }
    }

    fn ns_add(&mut self, name: &str, dno: usize) {
        let rec = matches!(self.datums[dno].kind, DKind::Rec);
        self.ns.push(Ns::Var(name.to_string(), dno, rec));
    }

    /// `plpgsql_ns_lookup`: the datum, whether it's a record, and how many names it used.
    fn ns_lookup(
        &self,
        local: bool,
        n1: &str,
        n2: Option<&str>,
        n3: Option<&str>,
    ) -> Option<(usize, bool, u32)> {
        let mut cur = self.ns.len();
        while cur > 0 {
            // This level: items down to its label.
            let mut i = cur;
            let mut label = None;
            while i > 0 {
                i -= 1;
                match &self.ns[i] {
                    Ns::Label(name, _) => {
                        label = Some((i, name.as_str()));
                        break;
                    }
                    Ns::Var(name, dno, rec) => {
                        if name == n1 && (n2.is_none() || *rec) {
                            return Some((*dno, *rec, 1));
                        }
                    }
                }
            }
            let (at, lname) = label?;
            if let Some(n2) = n2
                && lname == n1
            {
                for item in self.ns[at + 1..cur].iter().rev() {
                    if let Ns::Var(name, dno, rec) = item
                        && name == n2
                        && (n3.is_none() || *rec)
                    {
                        return Some((*dno, *rec, 2));
                    }
                }
            }
            if local {
                break;
            }
            cur = at;
        }
        None
    }

    fn ns_lookup_label(&self, name: &str) -> Option<LabelKind> {
        self.ns.iter().rev().find_map(|n| match n {
            Ns::Label(l, k) if l == name => Some(*k),
            _ => None,
        })
    }

    fn in_loop(&self) -> bool {
        self.ns
            .iter()
            .any(|n| matches!(n, Ns::Label(_, LabelKind::Loop)))
    }

    fn add_datum(&mut self, d: Datum) -> usize {
        self.datums.push(d);
        self.datums.len() - 1
    }

    fn recfield(&mut self, rec: usize, field: &str) -> usize {
        self.add_datum(Datum {
            kind: DKind::RecField(rec),
            name: field.to_string(),
            isconst: false,
            ty: None,
            cursor: None,
        })
    }

    /// `plpgsql_build_variable`.
    fn build_variable(&mut self, name: &str, ty: VarType, add: bool) -> Result<usize> {
        let kind = match ty.kind {
            TypeKind::Scalar => DKind::Var,
            TypeKind::Rec => DKind::Rec,
            TypeKind::Pseudo => {
                return Err(Error::new(
                    code::FEATURE_NOT_SUPPORTED,
                    format!("variable \"{name}\" has pseudo-type {}", ty.name),
                ));
            }
        };
        let dno = self.add_datum(Datum {
            kind,
            name: name.to_string(),
            isconst: false,
            ty: Some(ty),
            cursor: None,
        });
        if add {
            self.ns_add(name, dno);
        }
        Ok(dno)
    }

    fn kind(&self, dno: usize) -> DKind {
        self.datums[dno].kind
    }

    // ── the function ──

    fn function(&mut self) -> Result<()> {
        let spec = self.spec;
        self.ns_push(Some(spec.name), LabelKind::Block);
        match spec.trigger {
            Trigger::None => {
                for (i, p) in spec.params.iter().enumerate() {
                    let n = format!("${}", i + 1);
                    if p.ty.kind == TypeKind::Pseudo {
                        return Err(Error::new(
                            code::FEATURE_NOT_SUPPORTED,
                            format!("PL/pgSQL functions cannot accept type {}", p.ty.name),
                        ));
                    }
                    let name = p.name.clone().filter(|n| !n.is_empty());
                    let dno = self.build_variable(name.as_deref().unwrap_or(&n), p.ty.clone(), false)?;
                    self.param_name(&n, dno)?;
                    if let Some(name) = name {
                        self.param_name(&name, dno)?;
                    }
                }
                if let Some(t) = &spec.dollar_zero {
                    self.build_variable("$0", t.clone(), true)?;
                }
            }
            Trigger::Dml => {
                for n in ["new", "old"] {
                    self.build_variable(n, VarType::record(), true)?;
                }
                for n in [
                    "tg_name",
                    "tg_when",
                    "tg_level",
                    "tg_op",
                    "tg_relid",
                    "tg_relname",
                    "tg_table_name",
                    "tg_table_schema",
                    "tg_nargs",
                    "tg_argv",
                ] {
                    let d = self.build_variable(n, VarType::scalar("text"), true)?;
                    self.datums[d].kind = DKind::Promise;
                }
            }
            Trigger::Event => {
                for n in ["tg_event", "tg_tag"] {
                    let d = self.build_variable(n, VarType::scalar("text"), true)?;
                    self.datums[d].kind = DKind::Promise;
                }
            }
        }
        self.build_variable("found", VarType::scalar("boolean"), true)?;
        // comp_options
        let mut t = self.yylex()?;
        while t.is(b'#') {
            let o = self.yylex()?;
            if o.kw("option") {
                let d = self.yylex()?;
                if !d.kw("dump") {
                    return Err(self.yyerror(&d, "syntax error"));
                }
            } else if o.kw("print_strict_params") {
                let v = self.yylex()?;
                if !(v.k == K::Word || v.unreserved()) {
                    return Err(self.yyerror(&v, "syntax error"));
                }
                match v.s.as_str() {
                    "on" => self.opts.print_strict_params = Some(true),
                    "off" => self.opts.print_strict_params = Some(false),
                    other => {
                        return Err(Error::new(
                            code::INTERNAL_ERROR,
                            format!("unrecognized print_strict_params option {other}"),
                        ));
                    }
                }
            } else if o.kw("variable_conflict") {
                let v = self.yylex()?;
                if !(v.kw("error") || v.kw("use_variable") || v.kw("use_column")) {
                    return Err(self.yyerror(&v, "syntax error"));
                }
            } else {
                return Err(self.yyerror(&o, "syntax error"));
            }
            t = self.yylex()?;
        }
        let label = if t.k == K::LessLess {
            let l = self.any_identifier()?;
            let g = self.yylex()?;
            if g.k != K::GreaterGreater {
                return Err(self.yyerror(&g, "syntax error"));
            }
            t = self.yylex()?;
            Some(l)
        } else {
            None
        };
        if !(t.kw("declare") || t.kw("begin")) {
            return Err(self.yyerror(&t, "syntax error"));
        }
        self.pl_block(label, t)?;
        let mut t = self.yylex()?;
        if t.is(b';') {
            t = self.yylex()?;
        }
        if t.k != K::Eof {
            return Err(self.yyerror(&t, "syntax error"));
        }
        Ok(())
    }

    /// `add_parameter_name`.
    fn param_name(&mut self, name: &str, dno: usize) -> Result<()> {
        if self.ns_lookup(true, name, None, None).is_some() {
            return Err(Error::new(
                code::INVALID_FUNCTION_DEFINITION,
                format!("parameter name \"{name}\" used more than once"),
            ));
        }
        self.ns_add(name, dno);
        Ok(())
    }

    /// `any_identifier`: T_WORD, an unreserved keyword, or a variable's (simple) name.
    fn any_identifier(&mut self) -> Result<String> {
        let t = self.yylex()?;
        self.any_identifier_of(&t)
    }

    fn any_identifier_of(&self, t: &Tok) -> Result<String> {
        match t.k {
            K::Word => Ok(t.s.clone()),
            K::Kw(_) if t.unreserved() => Ok(t.s.clone()),
            K::Datum if t.idents.is_empty() => Ok(t.s.clone()),
            _ => Err(self.yyerror(t, "syntax error")),
        }
    }

    /// `opt_label` after END / END LOOP: the label and where it is, or the token that follows.
    fn opt_label(&mut self) -> Result<Option<(String, usize)>> {
        let t = self.yylex()?;
        if t.k == K::Word || t.unreserved() || t.k == K::Datum {
            return Ok(Some((self.any_identifier_of(&t)?, t.loc)));
        }
        self.push_back(t);
        Ok(None)
    }

    fn check_labels(&self, start: &Option<String>, end: Option<(String, usize)>) -> Result<()> {
        if let Some((end, at)) = end {
            match start {
                None => {
                    return Err(syntax(format!(
                        "end label \"{end}\" specified for unlabeled block"
                    ))
                    .position(at as u32 + 1));
                }
                Some(s) if *s != end => {
                    return Err(syntax(format!(
                        "end label \"{end}\" differs from block's label \"{s}\""
                    ))
                    .position(at as u32 + 1));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn expect(&mut self, c: u8) -> Result<Tok> {
        let t = self.yylex()?;
        if !t.is(c) {
            return Err(self.yyerror(&t, "syntax error"));
        }
        Ok(t)
    }

    fn expect_kw(&mut self, kw: &str) -> Result<Tok> {
        let t = self.yylex()?;
        if !t.kw(kw) {
            return Err(self.yyerror(&t, "syntax error"));
        }
        Ok(t)
    }

    /// `pl_block`, from its DECLARE / BEGIN.
    fn pl_block(&mut self, label: Option<String>, first: Tok) -> Result<()> {
        self.ns_push(label.as_deref(), LabelKind::Block);
        let mut t = first;
        if t.kw("declare") {
            self.lookup = Lookup::Declare;
            loop {
                t = self.yylex()?;
                if t.kw("begin") {
                    break;
                }
                if t.kw("declare") {
                    continue;
                }
                if t.k == K::LessLess {
                    self.any_identifier()?;
                    let g = self.yylex()?;
                    if g.k != K::GreaterGreater {
                        return Err(self.yyerror(&g, "syntax error"));
                    }
                    return Err(syntax("block label must be placed before DECLARE, not after")
                        .position(t.loc as u32 + 1));
                }
                self.decl_statement(t)?;
            }
            self.lookup = Lookup::Normal;
        }
        if !t.kw("begin") {
            return Err(self.yyerror(&t, "syntax error"));
        }
        let begin = t.loc;
        let mut t = self.proc_sect()?;
        if t.kw("exception") {
            t = self.exception_sect(t)?;
        }
        if !t.kw("end") {
            return Err(self.yyerror(&t, "syntax error"));
        }
        let end = self.opt_label()?;
        self.lineno(begin);
        self.check_labels(&label, end)?;
        self.ns_pop();
        Ok(())
    }

    /// `decl_varname`, with its duplicate and shadowing checks.
    fn decl_varname(&mut self, t: &Tok) -> Result<String> {
        if !(t.k == K::Word || t.unreserved()) {
            return Err(self.yyerror(t, "syntax error"));
        }
        let name = t.s.clone();
        self.lineno(t.loc);
        if self.ns_lookup(true, &name, None, None).is_some() {
            return Err(self.yyerror(t, "duplicate declaration"));
        }
        if let Some(error) = self.spec.shadow
            && self.ns_lookup(false, &name, None, None).is_some()
        {
            let e = Error::new(
                code::DUPLICATE_ALIAS,
                format!("variable \"{name}\" shadows a previously defined variable"),
            )
            .position(t.loc as u32 + 1);
            if error {
                return Err(e);
            }
            let mut e = e;
            e.severity = Severity::Warning;
            self.env.warning(e);
        }
        Ok(name)
    }

    fn decl_statement(&mut self, t: Tok) -> Result<()> {
        let name = self.decl_varname(&t)?;
        let mut t = self.yylex()?;
        if t.kw("alias") {
            self.expect_kw("for")?;
            let item = self.yylex()?;
            let found = match item.k {
                K::Word => self.ns_lookup(false, &item.s, None, None),
                K::Kw(_) if item.unreserved() => self.ns_lookup(false, &item.s, None, None),
                K::CWord => match item.idents.as_slice() {
                    [a, b] => self.ns_lookup(false, a, Some(b), None),
                    [a, b, c] => self.ns_lookup(false, a, Some(b), Some(c)),
                    _ => None,
                },
                _ => return Err(self.yyerror(&item, "syntax error")),
            };
            let Some((dno, _, _)) = found else {
                return Err(self.err_at(
                    code::UNDEFINED_OBJECT,
                    format!("variable \"{}\" does not exist", item.name()),
                    item.loc,
                ));
            };
            self.expect(b';')?;
            self.ns_add(&name, dno);
            return Ok(());
        }
        let scrollable = if t.kw("no") {
            let s = self.yylex()?;
            if !s.kw("scroll") {
                return Err(self.yyerror(&s, "syntax error"));
            }
            t = self.yylex()?;
            true
        } else if t.kw("scroll") {
            t = self.yylex()?;
            true
        } else {
            false
        };
        if scrollable || t.kw("cursor") {
            if !t.kw("cursor") {
                return Err(self.yyerror(&t, "syntax error"));
            }
            return self.decl_cursor(&name);
        }
        let constant = t.kw("constant");
        if constant {
            t = self.yylex()?;
        }
        let ty = self.read_datatype(t)?;
        // decl_collate
        let mut t = self.yylex()?;
        let mut collate_at = None;
        if t.kw("collate") {
            let n = self.yylex()?;
            let names = match n.k {
                K::Word => vec![n.s.clone()],
                K::Kw(_) if n.unreserved() => vec![n.s.clone()],
                K::CWord => n.idents.clone(),
                _ => return Err(self.yyerror(&n, "syntax error")),
            };
            self.env.collation(&names)?;
            collate_at = Some(t.loc);
            t = self.yylex()?;
        }
        let mut notnull_at = None;
        if t.kw("not") {
            let n = self.yylex()?;
            if !n.kw("null") {
                return Err(self.yyerror(&n, "syntax error"));
            }
            notnull_at = Some(t.loc);
            t = self.yylex()?;
        }
        let has_default = if t.is(b';') {
            false
        } else if t.is(b'=') || t.k == K::ColonEquals || t.kw("default") {
            self.read_sql_expression(&[K::Char(b';')], ";")?;
            true
        } else {
            return Err(self.yyerror(&t, "syntax error"));
        };
        if let Some(at) = collate_at
            && !ty.collatable
        {
            return Err(self.err_at(
                code::DATATYPE_MISMATCH,
                format!("collations are not supported by type {}", ty.name),
                at,
            ));
        }
        let dno = self.build_variable(&name, ty, true)?;
        self.datums[dno].isconst = constant;
        if let Some(at) = notnull_at
            && !has_default
        {
            return Err(self.err_at(
                code::NULL_VALUE_NOT_ALLOWED,
                format!(
                    "variable \"{name}\" must have a default value, since it's declared NOT NULL"
                ),
                at,
            ));
        }
        Ok(())
    }

    fn decl_cursor(&mut self, name: &str) -> Result<()> {
        self.ns_push(Some(name), LabelKind::Other);
        let mut t = self.yylex()?;
        let mut args = None;
        if t.is(b'(') {
            let open = t.loc;
            let mut names = Vec::new();
            loop {
                let a = self.yylex()?;
                let an = self.decl_varname(&a)?;
                let first = self.yylex()?;
                let ty = self.read_datatype(first)?;
                self.build_variable(&an, ty, true)?;
                names.push(an);
                let sep = self.yylex()?;
                if sep.is(b')') {
                    break;
                }
                if !sep.is(b',') {
                    return Err(self.yyerror(&sep, "syntax error"));
                }
            }
            self.lineno(open);
            args = Some(names);
            t = self.yylex()?;
        }
        if !(t.kw("is") || t.kw("for")) {
            return Err(self.yyerror(&t, "syntax error"));
        }
        self.read_sql_stmt()?;
        self.ns_pop();
        let ty = VarType {
            kind: TypeKind::Scalar,
            refcursor: true,
            collatable: false,
            name: "refcursor".into(),
        };
        let dno = self.build_variable(name, ty, true)?;
        // A bound cursor (without arguments: an empty list).
        self.datums[dno].cursor = Some(args.unwrap_or_default());
        Ok(())
    }

    /// `read_datatype`, from the type's first token; the token after it is pushed back.
    fn read_datatype(&mut self, first: Tok) -> Result<VarType> {
        let start = first.loc;
        let mut t = first;
        let mut result: Option<VarType> = None;
        if t.k == K::Word || t.unreserved() || t.k == K::CWord {
            let compound = t.k == K::CWord;
            let names = if compound {
                t.idents.clone()
            } else {
                vec![t.s.clone()]
            };
            t = self.yylex()?;
            if t.is(b'%') {
                t = self.yylex()?;
                if t.is_kw("type") {
                    result = Some(self.type_of(&names)?);
                } else if t.is_kw("rowtype") {
                    result = Some(self.env.resolve(TypeRef::Row(&names))?);
                }
            }
        }
        if let Some(mut r) = result {
            let mut is_array = false;
            t = self.yylex()?;
            if t.is_kw("array") {
                is_array = true;
                t = self.yylex()?;
            }
            while t.is(b'[') {
                is_array = true;
                t = self.yylex()?;
                if t.k == K::Iconst {
                    t = self.yylex()?;
                }
                if !t.is(b']') {
                    return Err(self.yyerror(&t, "syntax error, expected \"]\""));
                }
                t = self.yylex()?;
            }
            self.push_back(t);
            if is_array {
                r = VarType {
                    kind: TypeKind::Scalar,
                    refcursor: false,
                    collatable: r.collatable,
                    name: format!("{}[]", r.name),
                };
            }
            return Ok(r);
        }
        let mut depth = 0;
        while !t.is(b';') {
            if t.k == K::Eof {
                let msg = if depth != 0 {
                    "mismatched parentheses"
                } else {
                    "incomplete data type declaration"
                };
                return Err(self.yyerror(&t, msg));
            }
            if t.kw("collate")
                || t.kw("not")
                || t.is(b'=')
                || t.k == K::ColonEquals
                || t.kw("default")
            {
                break;
            }
            if (t.is(b',') || t.is(b')')) && depth == 0 {
                break;
            }
            if t.is(b'(') {
                depth += 1;
            } else if t.is(b')') {
                depth -= 1;
            }
            t = self.yylex()?;
        }
        let name = &self.src[start..t.loc.max(start)];
        if name.is_empty() {
            return Err(self.yyerror(&t, "missing data type declaration"));
        }
        let r = self.env.resolve(TypeRef::Name(name)).map_err(|mut e| {
            e.position = Some(match e.position {
                Some(p) => (start as u32 + p).max(1),
                None => start as u32 + 1,
            });
            e
        })?;
        self.push_back(t);
        Ok(r)
    }

    /// `plpgsql_parse_wordtype` / `plpgsql_parse_cwordtype`.
    fn type_of(&mut self, names: &[String]) -> Result<VarType> {
        let datum_type = |me: &Self, dno: usize| {
            me.datums[dno]
                .ty
                .clone()
                .unwrap_or_else(|| VarType::scalar("unknown"))
        };
        match names {
            [n] => match self.ns_lookup(false, n, None, None) {
                Some((dno, _, _)) => Ok(datum_type(self, dno)),
                None => Err(Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("variable \"{n}\" does not exist"),
                )),
            },
            [a, b] => match self.ns_lookup(false, a, Some(b), None) {
                Some((dno, false, _)) | Some((dno, true, 2)) => Ok(datum_type(self, dno)),
                _ => self.env.resolve(TypeRef::Column(names)),
            },
            _ => self.env.resolve(TypeRef::Column(names)),
        }
    }

    // ── statements ──

    /// `proc_sect`: statements up to one that ends it; returns that token.
    fn proc_sect(&mut self) -> Result<Tok> {
        loop {
            let t = self.yylex()?;
            if !self.proc_stmt(&t)? {
                return Ok(t);
            }
        }
    }

    /// One statement starting at `t`; false when `t` doesn't start one.
    fn proc_stmt(&mut self, t: &Tok) -> Result<bool> {
        let t = t.clone();
        match t.k {
            K::Kw("begin" | "declare") => {
                self.pl_block(None, t)?;
                self.expect(b';')?;
            }
            K::LessLess => {
                let label = self.any_identifier()?;
                let g = self.yylex()?;
                if g.k != K::GreaterGreater {
                    return Err(self.yyerror(&g, "syntax error"));
                }
                let n = self.yylex()?;
                match n.k {
                    K::Kw("begin" | "declare") => {
                        self.pl_block(Some(label), n)?;
                        self.expect(b';')?;
                    }
                    K::Kw("loop" | "while" | "for" | "foreach") => self.loop_stmt(Some(label), n)?,
                    _ => return Err(self.yyerror(&n, "syntax error")),
                }
            }
            K::Kw("loop" | "while" | "for" | "foreach") => self.loop_stmt(None, t)?,
            K::Datum => self.stmt_assign(t)?,
            K::Kw("if") => self.stmt_if(t)?,
            K::Kw("case") => self.stmt_case(t)?,
            K::Kw("exit" | "continue") => self.stmt_exit(t)?,
            K::Kw("return") => self.stmt_return(t)?,
            K::Kw("raise") => self.stmt_raise(t)?,
            K::Kw("assert") => {
                self.lineno(t.loc);
                let (_, end) =
                    self.read_sql_expression2(&[K::Char(b','), K::Char(b';')], ", or ;")?;
                if end.is(b',') {
                    self.read_sql_expression(&[K::Char(b';')], ";")?;
                }
            }
            K::Kw("import" | "insert" | "merge") => self.make_execsql_stmt(t)?,
            K::Word | K::CWord => {
                let n = self.yylex()?;
                let assign = matches!(
                    n.k,
                    K::Char(b'=') | K::ColonEquals | K::Char(b'[') | K::Char(b'.')
                );
                self.push_back(n);
                if assign {
                    return Err(self.not_a_variable(&t));
                }
                self.make_execsql_stmt(t)?;
            }
            K::Kw("execute") => self.stmt_dynexecute(t)?,
            K::Kw("perform") => {
                self.lineno(t.loc);
                self.push_back(t);
                let (text, start, _) =
                    self.read_sql_construct(&[K::Char(b';')], ";", true, false)?;
                let sql = format!("SELECT{}", &text[7.min(text.len())..]);
                self.check_sql(&sql, Mode::Default, start + 1)?;
            }
            K::Kw("call" | "do") => {
                self.lineno(t.loc);
                self.push_back(t);
                self.read_sql_stmt()?;
            }
            K::Kw("get") => self.stmt_getdiag(t)?,
            K::Kw("open") => self.stmt_open(t)?,
            K::Kw("fetch" | "move") => self.stmt_fetch(t)?,
            K::Kw("close") => {
                self.cursor_variable()?;
                self.expect(b';')?;
                self.lineno(t.loc);
            }
            K::Kw("null") => {
                self.expect(b';')?;
            }
            K::Kw("commit" | "rollback") => {
                let mut n = self.yylex()?;
                if n.kw("and") {
                    n = self.yylex()?;
                    if n.kw("no") {
                        n = self.yylex()?;
                    }
                    if !n.kw("chain") {
                        return Err(self.yyerror(&n, "syntax error"));
                    }
                    n = self.yylex()?;
                }
                if !n.is(b';') {
                    return Err(self.yyerror(&n, "syntax error"));
                }
                self.lineno(t.loc);
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    fn not_a_variable(&self, t: &Tok) -> Error {
        match t.k {
            K::Word | K::CWord => self.err_at(
                code::SYNTAX_ERROR,
                format!("\"{}\" is not a known variable", t.name()),
                t.loc,
            ),
            _ => self.yyerror(t, "syntax error"),
        }
    }

    /// `check_assignable`.
    fn check_assignable(&self, dno: usize, at: usize) -> Result<()> {
        let d = &self.datums[dno];
        match d.kind {
            DKind::Var | DKind::Promise | DKind::Rec if d.isconst => Err(self.err_at(
                "22005",
                format!("variable \"{}\" is declared CONSTANT", d.name),
                at,
            )),
            DKind::RecField(p) => self.check_assignable(p, at),
            _ => Ok(()),
        }
    }

    fn stmt_assign(&mut self, t: Tok) -> Result<()> {
        self.check_assignable(t.dno, t.loc)?;
        self.lineno(t.loc);
        self.push_back(t);
        self.read_sql_construct_mode(&[K::Char(b';')], ";", Mode::Assign, false, true)?;
        Ok(())
    }

    fn stmt_if(&mut self, t: Tok) -> Result<()> {
        self.read_sql_expression(&[K::Kw("then")], "THEN")?;
        let mut n = self.proc_sect()?;
        while n.kw("elsif") {
            self.read_sql_expression(&[K::Kw("then")], "THEN")?;
            let at = n.loc;
            n = self.proc_sect()?;
            self.lineno(at);
        }
        if n.kw("else") {
            n = self.proc_sect()?;
        }
        if !n.kw("end") {
            return Err(self.yyerror(&n, "syntax error"));
        }
        self.expect_kw("if")?;
        self.expect(b';')?;
        self.lineno(t.loc);
        Ok(())
    }

    fn stmt_case(&mut self, t: Tok) -> Result<()> {
        let n = self.yylex()?;
        if !n.kw("when") {
            self.push_back(n);
            let (_, w) = self.read_sql_expression2(&[K::Kw("when")], "WHEN")?;
            self.push_back(w);
        } else {
            self.push_back(n);
        }
        let mut n = self.yylex()?;
        if !n.kw("when") {
            return Err(self.yyerror(&n, "syntax error"));
        }
        while n.kw("when") {
            let at = n.loc;
            self.read_sql_expression(&[K::Kw("then")], "THEN")?;
            n = self.proc_sect()?;
            self.lineno(at);
        }
        if n.kw("else") {
            n = self.proc_sect()?;
        }
        if !n.kw("end") {
            return Err(self.yyerror(&n, "syntax error"));
        }
        self.expect_kw("case")?;
        self.expect(b';')?;
        self.lineno(t.loc);
        Ok(())
    }

    /// `loop_body`: statements, END LOOP [label] ;. Returns the end label.
    fn loop_body(&mut self) -> Result<Option<(String, usize)>> {
        let n = self.proc_sect()?;
        if !n.kw("end") {
            return Err(self.yyerror(&n, "syntax error"));
        }
        self.expect_kw("loop")?;
        let label = self.opt_label()?;
        self.expect(b';')?;
        Ok(label)
    }

    fn loop_stmt(&mut self, label: Option<String>, t: Tok) -> Result<()> {
        self.ns_push(label.as_deref(), LabelKind::Loop);
        match t.k {
            K::Kw("loop") => {}
            K::Kw("while") => {
                self.read_sql_expression(&[K::Kw("loop")], "LOOP")?;
            }
            K::Kw("for") => {
                let v = self.for_variable()?;
                let i = self.yylex()?;
                if !i.kw("in") {
                    return Err(self.yyerror(&i, "syntax error"));
                }
                self.for_control(&v)?;
            }
            _ => {
                // FOREACH
                let v = self.for_variable()?;
                let mut n = self.yylex()?;
                if n.kw("slice") {
                    let s = self.yylex()?;
                    if s.k != K::Iconst {
                        return Err(self.yyerror(&s, "syntax error"));
                    }
                    n = self.yylex()?;
                }
                if !n.kw("in") {
                    return Err(self.yyerror(&n, "syntax error"));
                }
                let a = self.yylex()?;
                if !a.kw("array") {
                    return Err(self.yyerror(&a, "syntax error"));
                }
                self.read_sql_expression(&[K::Kw("loop")], "LOOP")?;
                let end = self.loop_body()?;
                self.lineno(t.loc);
                match (v.row, v.scalar) {
                    (Some(d), _) | (None, Some(d)) => self.check_assignable(d, v.loc)?,
                    _ => {
                        return Err(self.err_at(
                            code::SYNTAX_ERROR,
                            "loop variable of FOREACH must be a known variable or list of variables",
                            v.loc,
                        ));
                    }
                }
                self.check_labels(&label, end)?;
                self.ns_pop();
                return Ok(());
            }
        }
        let end = self.loop_body()?;
        self.lineno(t.loc);
        self.check_labels(&label, end)?;
        self.ns_pop();
        Ok(())
    }

    /// `for_variable`.
    fn for_variable(&mut self) -> Result<ForVar> {
        let t = self.yylex()?;
        match t.k {
            K::Datum => {
                let name = t.name();
                self.lineno(t.loc);
                let mut v = ForVar {
                    name,
                    loc: t.loc,
                    scalar: None,
                    row: None,
                };
                if matches!(self.kind(t.dno), DKind::Row | DKind::Rec) {
                    v.row = Some(t.dno);
                } else {
                    v.scalar = Some(t.dno);
                    let n = self.yylex()?;
                    let comma = n.is(b',');
                    self.push_back(n);
                    if comma {
                        let row = self.read_into_scalar_list(t.dno, t.loc)?;
                        v.row = Some(row);
                    }
                }
                Ok(v)
            }
            K::Word => {
                self.lineno(t.loc);
                let n = self.yylex()?;
                let comma = n.is(b',');
                self.push_back(n);
                if comma {
                    return Err(self.not_a_variable(&t));
                }
                Ok(ForVar {
                    name: t.s.clone(),
                    loc: t.loc,
                    scalar: None,
                    row: None,
                })
            }
            K::CWord => Err(self.not_a_variable(&t)),
            _ => Err(self.yyerror(&t, "syntax error")),
        }
    }

    /// `for_control`, after FOR var IN.
    fn for_control(&mut self, v: &ForVar) -> Result<()> {
        let t = self.yylex()?;
        let tokloc = t.loc;
        if t.kw("execute") {
            let (_, term) =
                self.read_sql_expression2(&[K::Kw("loop"), K::Kw("using")], "LOOP or USING")?;
            match (v.row, v.scalar) {
                (Some(d), _) | (None, Some(d)) => self.check_assignable(d, v.loc)?,
                _ => {
                    return Err(self.err_at(
                        code::DATATYPE_MISMATCH,
                        "loop variable of loop over rows must be a record variable or list of scalar variables",
                        v.loc,
                    ));
                }
            }
            if term.kw("using") {
                loop {
                    let (_, term) = self
                        .read_sql_expression2(&[K::Char(b','), K::Kw("loop")], ", or LOOP")?;
                    if !term.is(b',') {
                        break;
                    }
                }
            }
            return Ok(());
        }
        if t.k == K::Datum
            && self.kind(t.dno) == DKind::Var
            && self.datums[t.dno].ty.as_ref().is_some_and(|ty| ty.refcursor)
        {
            if v.scalar.is_some() && v.row.is_some() {
                return Err(self.err_at(
                    code::SYNTAX_ERROR,
                    "cursor FOR loop must have only one target variable",
                    v.loc,
                ));
            }
            if self.datums[t.dno].cursor.is_none() {
                return Err(self.err_at(
                    code::SYNTAX_ERROR,
                    "cursor FOR loop must use a bound cursor variable",
                    tokloc,
                ));
            }
            self.read_cursor_args(t.dno, K::Kw("loop"))?;
            self.build_variable(&v.name, VarType::record(), true)?;
            return Ok(());
        }
        let reverse = t.is_kw("reverse");
        if !reverse {
            self.push_back(t);
        }
        let (text, start, term) =
            self.read_sql_construct(&[K::DotDot, K::Kw("loop")], "LOOP", true, false)?;
        if term.k == K::DotDot {
            self.check_sql(&text, Mode::Expr, start)?;
            let (_, term) = self.read_sql_expression2(&[K::Kw("loop"), K::Kw("by")], "LOOP")?;
            if term.kw("by") {
                self.read_sql_expression(&[K::Kw("loop")], "LOOP")?;
            }
            if v.scalar.is_some() && v.row.is_some() {
                return Err(self.err_at(
                    code::SYNTAX_ERROR,
                    "integer FOR loop must have only one target variable",
                    v.loc,
                ));
            }
            self.build_variable(&v.name, VarType::scalar("integer"), true)?;
            return Ok(());
        }
        if reverse {
            return Err(self.err_at(
                code::SYNTAX_ERROR,
                "cannot specify REVERSE in query FOR loop",
                tokloc,
            ));
        }
        self.check_sql(&text, Mode::Default, start)?;
        match (v.row, v.scalar) {
            (Some(d), _) | (None, Some(d)) => self.check_assignable(d, v.loc),
            _ => Err(self.err_at(
                code::SYNTAX_ERROR,
                "loop variable of loop over rows must be a record variable or list of scalar variables",
                v.loc,
            )),
        }
    }

    fn stmt_exit(&mut self, t: Tok) -> Result<()> {
        let exit = t.kw("exit");
        let mut n = self.yylex()?;
        let mut label = None;
        if n.k == K::Word || n.unreserved() || n.k == K::Datum {
            label = Some((self.any_identifier_of(&n)?, n.loc));
            n = self.yylex()?;
        }
        if n.kw("when") {
            self.read_sql_expression(&[K::Char(b';')], ";")?;
        } else if !n.is(b';') {
            return Err(self.yyerror(&n, "syntax error"));
        }
        self.lineno(t.loc);
        match label {
            Some((l, at)) => match self.ns_lookup_label(&l) {
                None => Err(self.err_at(
                    code::SYNTAX_ERROR,
                    format!(
                        "there is no label \"{l}\" attached to any block or loop enclosing this statement"
                    ),
                    at,
                )),
                Some(k) if k != LabelKind::Loop && !exit => Err(self.err_at(
                    code::SYNTAX_ERROR,
                    format!("block label \"{l}\" cannot be used in CONTINUE"),
                    at,
                )),
                Some(_) => Ok(()),
            },
            None if !self.in_loop() => Err(self.err_at(
                code::SYNTAX_ERROR,
                if exit {
                    "EXIT cannot be used outside a loop, unless it has a label"
                } else {
                    "CONTINUE cannot be used outside a loop"
                },
                t.loc,
            )),
            None => Ok(()),
        }
    }

    fn stmt_return(&mut self, t: Tok) -> Result<()> {
        let n = self.yylex()?;
        if n.k == K::Eof {
            return Err(self.yyerror(&n, "unexpected end of function definition"));
        }
        let spec = self.spec;
        let out = spec.params.iter().any(|p| matches!(p.mode, 'o' | 'b' | 't'));
        let ret_void = spec.void || spec.trigger == Trigger::Event;
        if n.is_kw("next") {
            if !spec.set {
                return Err(self.err_at(
                    code::DATATYPE_MISMATCH,
                    "cannot use RETURN NEXT in a non-SETOF function",
                    t.loc,
                ));
            }
            self.lineno(t.loc);
            if out {
                let e = self.yylex()?;
                if !e.is(b';') {
                    return Err(self.err_at(
                        code::DATATYPE_MISMATCH,
                        "RETURN NEXT cannot have a parameter in function with OUT parameters",
                        e.loc,
                    ));
                }
                return Ok(());
            }
            return self.return_value();
        }
        if n.is_kw("query") {
            if !spec.set {
                return Err(self.err_at(
                    code::DATATYPE_MISMATCH,
                    "cannot use RETURN QUERY in a non-SETOF function",
                    t.loc,
                ));
            }
            self.lineno(t.loc);
            let e = self.yylex()?;
            if !e.kw("execute") {
                self.push_back(e);
                self.read_sql_stmt()?;
                return Ok(());
            }
            let (_, term) =
                self.read_sql_expression2(&[K::Char(b';'), K::Kw("using")], "; or USING")?;
            if term.kw("using") {
                loop {
                    let (_, term) =
                        self.read_sql_expression2(&[K::Char(b','), K::Char(b';')], ", or ;")?;
                    if !term.is(b',') {
                        break;
                    }
                }
            }
            return Ok(());
        }
        self.push_back(n);
        self.lineno(t.loc);
        if spec.set {
            let e = self.yylex()?;
            if !e.is(b';') {
                return Err(self
                    .err_at(
                        code::DATATYPE_MISMATCH,
                        "RETURN cannot have a parameter in function returning set",
                        e.loc,
                    )
                    .hint("Use RETURN NEXT or RETURN QUERY."));
            }
        } else if ret_void && !(spec.procedure && out) {
            let e = self.yylex()?;
            if !e.is(b';') {
                return Err(if spec.procedure {
                    self.err_at(
                        code::SYNTAX_ERROR,
                        "RETURN cannot have a parameter in a procedure",
                        e.loc,
                    )
                } else {
                    self.err_at(
                        code::DATATYPE_MISMATCH,
                        "RETURN cannot have a parameter in function returning void",
                        e.loc,
                    )
                });
            }
        } else if out {
            let e = self.yylex()?;
            if !e.is(b';') {
                return Err(self.err_at(
                    code::DATATYPE_MISMATCH,
                    "RETURN cannot have a parameter in function with OUT parameters",
                    e.loc,
                ));
            }
        } else {
            return self.return_value();
        }
        Ok(())
    }

    /// RETURN / RETURN NEXT's value: a variable, or an expression.
    fn return_value(&mut self) -> Result<()> {
        let e = self.yylex()?;
        if e.k == K::Datum
            && matches!(
                self.kind(e.dno),
                DKind::Var | DKind::Promise | DKind::Row | DKind::Rec
            )
            && self.peek()? == K::Char(b';')
        {
            self.yylex()?;
            return Ok(());
        }
        self.push_back(e);
        self.read_sql_expression(&[K::Char(b';')], ";")?;
        Ok(())
    }

    fn stmt_raise(&mut self, t: Tok) -> Result<()> {
        self.lineno(t.loc);
        let mut n = self.yylex()?;
        if n.k == K::Eof {
            return Err(self.yyerror(&n, "unexpected end of function definition"));
        }
        let mut message: Option<String> = None;
        let mut params = 0;
        if !n.is(b';') {
            if ["exception", "warning", "notice", "info", "log", "debug"]
                .iter()
                .any(|l| n.is_kw(l))
            {
                n = self.yylex()?;
            }
            if n.k == K::Eof {
                return Err(self.yyerror(&n, "unexpected end of function definition"));
            }
            if n.k == K::Sconst {
                message = Some(n.s.clone());
                n = self.yylex()?;
                if !(n.is(b',') || n.is(b';') || n.kw("using")) {
                    return Err(self.yyerror(&n, "syntax error"));
                }
                while n.is(b',') {
                    let (_, _, term) = self.read_sql_construct_mode(
                        &[K::Char(b','), K::Char(b';'), K::Kw("using")],
                        ", or ; or USING",
                        Mode::Expr,
                        true,
                        true,
                    )?;
                    params += 1;
                    n = term;
                }
            } else if !n.kw("using") {
                if n.is_kw("sqlstate") {
                    let s = self.yylex()?;
                    if s.k != K::Sconst {
                        return Err(self.yyerror(&s, "syntax error"));
                    }
                    if !valid_sqlstate(&s.s) {
                        return Err(self.yyerror(&s, "invalid SQLSTATE code"));
                    }
                } else {
                    let name = if n.k == K::Word || n.unreserved() {
                        n.s.clone()
                    } else {
                        return Err(self.yyerror(&n, "syntax error"));
                    };
                    if !super::conditions::CONDITIONS.iter().any(|(c, _)| *c == name) {
                        return Err(Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("unrecognized exception condition \"{name}\""),
                        ));
                    }
                }
                n = self.yylex()?;
                if !(n.is(b';') || n.kw("using")) {
                    return Err(self.yyerror(&n, "syntax error"));
                }
            }
            if n.kw("using") {
                loop {
                    let o = self.yylex()?;
                    if o.k == K::Eof {
                        return Err(self.yyerror(&o, "unexpected end of function definition"));
                    }
                    if ![
                        "errcode",
                        "message",
                        "detail",
                        "hint",
                        "column",
                        "constraint",
                        "datatype",
                        "table",
                        "schema",
                    ]
                    .iter()
                    .any(|k| o.is_kw(k))
                    {
                        return Err(self.yyerror(&o, "unrecognized RAISE statement option"));
                    }
                    let eq = self.yylex()?;
                    if !(eq.is(b'=') || eq.k == K::ColonEquals) {
                        return Err(self.yyerror(&eq, "syntax error, expected \"=\""));
                    }
                    let (_, term) =
                        self.read_sql_expression2(&[K::Char(b','), K::Char(b';')], ", or ;")?;
                    if term.is(b';') {
                        break;
                    }
                }
            }
        }
        if let Some(m) = message {
            let want = super::parse::placeholders(&m);
            if want < params {
                return Err(syntax("too many parameters specified for RAISE"));
            }
            if want > params {
                return Err(syntax("too few parameters specified for RAISE"));
            }
        }
        Ok(())
    }

    fn stmt_dynexecute(&mut self, t: Tok) -> Result<()> {
        let (_, _, mut end) = self.read_sql_construct_mode(
            &[K::Kw("into"), K::Kw("using"), K::Char(b';')],
            "INTO or USING or ;",
            Mode::Expr,
            true,
            true,
        )?;
        self.lineno(t.loc);
        let (mut into, mut using) = (false, false);
        loop {
            if end.kw("into") {
                if into {
                    return Err(self.yyerror(&end, "syntax error"));
                }
                into = true;
                self.read_into_target(true)?;
                end = self.yylex()?;
            } else if end.kw("using") {
                if using {
                    return Err(self.yyerror(&end, "syntax error"));
                }
                using = true;
                loop {
                    let (_, _, term) = self.read_sql_construct_mode(
                        &[K::Char(b','), K::Char(b';'), K::Kw("into")],
                        ", or ; or INTO",
                        Mode::Expr,
                        true,
                        true,
                    )?;
                    end = term;
                    if !end.is(b',') {
                        break;
                    }
                }
            } else if end.is(b';') {
                return Ok(());
            } else {
                return Err(self.yyerror(&end, "syntax error"));
            }
        }
    }

    /// `cursor_variable`: the cursor's datum.
    fn cursor_variable(&mut self) -> Result<usize> {
        let t = self.yylex()?;
        match t.k {
            K::Datum => {
                if self.kind(t.dno) != DKind::Var || self.peek()? == K::Char(b'[') {
                    return Err(self.err_at(
                        code::DATATYPE_MISMATCH,
                        "cursor variable must be a simple variable",
                        t.loc,
                    ));
                }
                if !self.datums[t.dno].ty.as_ref().is_some_and(|ty| ty.refcursor) {
                    return Err(self.err_at(
                        code::DATATYPE_MISMATCH,
                        format!(
                            "variable \"{}\" must be of type cursor or refcursor",
                            self.datums[t.dno].name
                        ),
                        t.loc,
                    ));
                }
                Ok(t.dno)
            }
            _ => Err(self.not_a_variable(&t)),
        }
    }

    fn stmt_open(&mut self, t: Tok) -> Result<()> {
        let cur = self.cursor_variable()?;
        self.lineno(t.loc);
        if self.datums[cur].cursor.is_some() {
            self.read_cursor_args(cur, K::Char(b';'))?;
            return Ok(());
        }
        let mut n = self.yylex()?;
        if n.is_kw("no") {
            n = self.yylex()?;
            if n.is_kw("scroll") {
                n = self.yylex()?;
            }
        } else if n.is_kw("scroll") {
            n = self.yylex()?;
        }
        if !n.kw("for") {
            return Err(self.yyerror(&n, "syntax error, expected \"FOR\""));
        }
        let n = self.yylex()?;
        if n.kw("execute") {
            let (_, mut end) =
                self.read_sql_expression2(&[K::Kw("using"), K::Char(b';')], "USING or ;")?;
            if end.kw("using") {
                loop {
                    let (_, term) =
                        self.read_sql_expression2(&[K::Char(b','), K::Char(b';')], ", or ;")?;
                    end = term;
                    if !end.is(b',') {
                        break;
                    }
                }
            }
        } else {
            self.push_back(n);
            self.read_sql_stmt()?;
        }
        Ok(())
    }

    /// `read_cursor_args`, then the `until` token.
    fn read_cursor_args(&mut self, cur: usize, until: K) -> Result<()> {
        let t = self.yylex()?;
        let name = self.datums[cur].name.clone();
        let params = self.datums[cur].cursor.clone().unwrap_or_default();
        if params.is_empty() {
            if t.is(b'(') {
                return Err(self.err_at(
                    code::SYNTAX_ERROR,
                    format!("cursor \"{name}\" has no arguments"),
                    t.loc,
                ));
            }
            if t.k != until {
                return Err(self.yyerror(&t, "syntax error"));
            }
            return Ok(());
        }
        if !t.is(b'(') {
            return Err(self.err_at(
                code::SYNTAX_ERROR,
                format!("cursor \"{name}\" has arguments"),
                t.loc,
            ));
        }
        let mut seen = vec![false; params.len()];
        for argc in 0..params.len() {
            // plpgsql_peek2
            let t1 = self.internal()?;
            let t2 = self.internal()?;
            let arglocation = t1.loc;
            let named = t1.k == K::Ident && matches!(t2.k, K::ColonEquals | K::EqualsGreater);
            self.push_back(t2);
            self.push_back(t1);
            let argpos = if named {
                let save = self.lookup;
                self.lookup = Lookup::Declare;
                let a = self.yylex()?;
                self.lookup = save;
                let Some(p) = params.iter().position(|p| *p == a.s) else {
                    return Err(self.err_at(
                        code::SYNTAX_ERROR,
                        format!("cursor \"{name}\" has no argument named \"{}\"", a.s),
                        a.loc,
                    ));
                };
                let eq = self.yylex()?;
                if !matches!(eq.k, K::ColonEquals | K::EqualsGreater) {
                    return Err(self.yyerror(&eq, "syntax error"));
                }
                p
            } else {
                argc
            };
            if seen[argpos] {
                return Err(self.err_at(
                    code::SYNTAX_ERROR,
                    format!(
                        "value for parameter \"{}\" of cursor \"{name}\" specified more than once",
                        params[argpos]
                    ),
                    arglocation,
                ));
            }
            seen[argpos] = true;
            let (_, _, end) = self.read_sql_construct_mode(
                &[K::Char(b','), K::Char(b')')],
                ",\" or \")",
                Mode::Expr,
                true,
                true,
            )?;
            let last = argc == params.len() - 1;
            if end.is(b')') && !last {
                return Err(self.err_at(
                    code::SYNTAX_ERROR,
                    format!("not enough arguments for cursor \"{name}\""),
                    end.loc,
                ));
            }
            if end.is(b',') && last {
                return Err(self.err_at(
                    code::SYNTAX_ERROR,
                    format!("too many arguments for cursor \"{name}\""),
                    end.loc,
                ));
            }
        }
        let t = self.yylex()?;
        if t.k != until {
            return Err(self.yyerror(&t, "syntax error"));
        }
        Ok(())
    }

    fn stmt_fetch(&mut self, t: Tok) -> Result<()> {
        let fetch = t.kw("fetch");
        let multiple = self.read_fetch_direction()?;
        self.cursor_variable()?;
        if fetch {
            let i = self.yylex()?;
            if !i.kw("into") {
                return Err(self.yyerror(&i, "syntax error"));
            }
            self.read_into_target(false)?;
            let s = self.yylex()?;
            if !s.is(b';') {
                return Err(self.yyerror(&s, "syntax error"));
            }
            if multiple {
                return Err(self.err_at(
                    code::FEATURE_NOT_SUPPORTED,
                    "FETCH statement cannot return multiple rows",
                    t.loc,
                ));
            }
        } else {
            self.expect(b';')?;
        }
        self.lineno(t.loc);
        Ok(())
    }

    /// `read_fetch_direction`: whether it can return several rows.
    fn read_fetch_direction(&mut self) -> Result<bool> {
        let t = self.yylex()?;
        if t.k == K::Eof {
            return Err(self.yyerror(&t, "unexpected end of function definition"));
        }
        let from_in = [K::Kw("from"), K::Kw("in")];
        let mut check_from = true;
        let mut multiple = false;
        if t.is_kw("next") || t.is_kw("prior") || t.is_kw("first") || t.is_kw("last") {
        } else if t.is_kw("absolute") || t.is_kw("relative") {
            self.read_sql_expression2(&from_in, "FROM or IN")?;
            check_from = false;
        } else if t.is_kw("all") {
            multiple = true;
        } else if t.is_kw("forward") || t.is_kw("backward") {
            let n = self.yylex()?;
            if n.k == K::Eof {
                return Err(self.yyerror(&n, "unexpected end of function definition"));
            }
            if n.kw("from") || n.kw("in") {
                check_from = false;
            } else if n.kw("all") {
                multiple = true;
            } else {
                self.push_back(n);
                self.read_sql_expression2(&from_in, "FROM or IN")?;
                multiple = true;
                check_from = false;
            }
        } else if t.kw("from") || t.kw("in") {
            check_from = false;
        } else if t.k == K::Datum {
            self.push_back(t);
            check_from = false;
        } else {
            self.push_back(t);
            self.read_sql_expression2(&from_in, "FROM or IN")?;
            multiple = true;
            check_from = false;
        }
        if check_from {
            let n = self.yylex()?;
            if !(n.kw("from") || n.kw("in")) {
                return Err(self.yyerror(&n, "expected FROM or IN"));
            }
        }
        Ok(multiple)
    }

    fn stmt_getdiag(&mut self, t: Tok) -> Result<()> {
        let mut n = self.yylex()?;
        let stacked = n.kw("stacked");
        if stacked || n.kw("current") {
            n = self.yylex()?;
        }
        if !n.kw("diagnostics") {
            return Err(self.yyerror(&n, "syntax error"));
        }
        let mut kinds = Vec::new();
        loop {
            let target = self.yylex()?;
            match target.k {
                K::Datum => {
                    if matches!(self.kind(target.dno), DKind::Row | DKind::Rec)
                        || self.peek()? == K::Char(b'[')
                    {
                        return Err(self.err_at(
                            code::SYNTAX_ERROR,
                            format!("\"{}\" is not a scalar variable", target.name()),
                            target.loc,
                        ));
                    }
                    self.check_assignable(target.dno, target.loc)?;
                }
                _ => return Err(self.not_a_variable(&target)),
            }
            let eq = self.yylex()?;
            if !(eq.is(b'=') || eq.k == K::ColonEquals) {
                return Err(self.yyerror(&eq, "syntax error"));
            }
            let item = self.yylex()?;
            let Some(kind) = [
                "row_count",
                "pg_routine_oid",
                "pg_context",
                "pg_exception_detail",
                "pg_exception_hint",
                "pg_exception_context",
                "column_name",
                "constraint_name",
                "pg_datatype_name",
                "message_text",
                "table_name",
                "schema_name",
                "returned_sqlstate",
            ]
            .into_iter()
            .find(|k| item.is_kw(k)) else {
                return Err(self.yyerror(&item, "unrecognized GET DIAGNOSTICS item"));
            };
            kinds.push(kind);
            let sep = self.yylex()?;
            if sep.is(b';') {
                break;
            }
            if !sep.is(b',') {
                return Err(self.yyerror(&sep, "syntax error"));
            }
        }
        for k in kinds {
            let current_only = matches!(k, "row_count" | "pg_routine_oid");
            let stacked_only = !current_only && k != "pg_context";
            if current_only && stacked || stacked_only && !stacked {
                return Err(self.err_at(
                    code::SYNTAX_ERROR,
                    format!(
                        "diagnostics item {} is not allowed in GET {} DIAGNOSTICS",
                        k.to_uppercase(),
                        if stacked { "STACKED" } else { "CURRENT" }
                    ),
                    t.loc,
                ));
            }
        }
        self.lineno(t.loc);
        Ok(())
    }

    fn exception_sect(&mut self, t: Tok) -> Result<Tok> {
        self.lineno(t.loc);
        for n in ["sqlstate", "sqlerrm"] {
            let d = self.build_variable(n, VarType::scalar("text"), true)?;
            self.datums[d].isconst = true;
        }
        let mut n = self.yylex()?;
        if !n.kw("when") {
            return Err(self.yyerror(&n, "syntax error"));
        }
        while n.kw("when") {
            let at = n.loc;
            loop {
                let c = self.yylex()?;
                let name = self.any_identifier_of(&c)?;
                if name != "sqlstate" {
                    if name != "others"
                        && !super::conditions::CONDITIONS.iter().any(|(c, _)| *c == name)
                    {
                        return Err(Error::new(
                            code::UNDEFINED_OBJECT,
                            format!("unrecognized exception condition \"{name}\""),
                        ));
                    }
                } else {
                    let s = self.yylex()?;
                    if s.k != K::Sconst {
                        return Err(self.yyerror(&s, "syntax error"));
                    }
                    if !valid_sqlstate(&s.s) {
                        return Err(self.yyerror(&s, "invalid SQLSTATE code"));
                    }
                }
                let o = self.yylex()?;
                if o.kw("or") {
                    continue;
                }
                if !o.kw("then") {
                    return Err(self.yyerror(&o, "syntax error"));
                }
                break;
            }
            n = self.proc_sect()?;
            self.lineno(at);
        }
        Ok(n)
    }

    // ── INTO targets ──

    /// `read_into_target`, after INTO.
    fn read_into_target(&mut self, strict: bool) -> Result<()> {
        let mut t = self.yylex()?;
        if strict && t.kw("strict") {
            t = self.yylex()?;
        }
        if t.k != K::Datum {
            return Err(self.not_a_variable(&t));
        }
        if matches!(self.kind(t.dno), DKind::Row | DKind::Rec) {
            self.check_assignable(t.dno, t.loc)?;
            let n = self.yylex()?;
            if n.is(b',') {
                return Err(self.err_at(
                    code::SYNTAX_ERROR,
                    "record variable cannot be part of multiple-item INTO list",
                    n.loc,
                ));
            }
            self.push_back(n);
            return Ok(());
        }
        self.read_into_scalar_list(t.dno, t.loc)?;
        Ok(())
    }

    /// `read_into_scalar_list`: the row it makes.
    fn read_into_scalar_list(&mut self, first: usize, loc: usize) -> Result<usize> {
        self.check_assignable(first, loc)?;
        let mut n = 1;
        loop {
            let t = self.yylex()?;
            if !t.is(b',') {
                self.push_back(t);
                break;
            }
            if n >= 1024 {
                return Err(self.err_at(
                    code::PROGRAM_LIMIT_EXCEEDED,
                    "too many INTO variables specified",
                    t.loc,
                ));
            }
            let v = self.yylex()?;
            if v.k != K::Datum {
                return Err(self.not_a_variable(&v));
            }
            self.check_assignable(v.dno, v.loc)?;
            if matches!(self.kind(v.dno), DKind::Row | DKind::Rec) {
                return Err(self.err_at(
                    code::SYNTAX_ERROR,
                    format!("\"{}\" is not a scalar variable", v.name()),
                    v.loc,
                ));
            }
            n += 1;
        }
        self.lineno(loc);
        Ok(self.add_datum(Datum {
            kind: DKind::Row,
            name: "(unnamed row)".into(),
            isconst: false,
            ty: None,
            cursor: None,
        }))
    }

    // ── SQL text ──

    fn read_sql_expression(&mut self, until: &[K], expected: &str) -> Result<String> {
        Ok(self
            .read_sql_construct_mode(until, expected, Mode::Expr, true, true)?
            .0)
    }

    fn read_sql_expression2(&mut self, until: &[K], expected: &str) -> Result<(String, Tok)> {
        let (s, _, t) = self.read_sql_construct_mode(until, expected, Mode::Expr, true, true)?;
        Ok((s, t))
    }

    fn read_sql_stmt(&mut self) -> Result<String> {
        Ok(self
            .read_sql_construct_mode(&[K::Char(b';')], ";", Mode::Default, false, true)?
            .0)
    }

    /// `read_sql_construct` in the default mode, unchecked unless `check`.
    fn read_sql_construct(
        &mut self,
        until: &[K],
        expected: &str,
        isexpr: bool,
        check: bool,
    ) -> Result<(String, usize, Tok)> {
        self.read_sql_construct_mode(until, expected, Mode::Default, isexpr, check)
    }

    /// `read_sql_construct`: the text up to a terminator, where it starts, and the terminator.
    fn read_sql_construct_mode(
        &mut self,
        until: &[K],
        expected: &str,
        mode: Mode,
        isexpr: bool,
        check: bool,
    ) -> Result<(String, usize, Tok)> {
        let save = self.lookup;
        self.lookup = Lookup::Expr;
        let mut start = None;
        let mut end = 0usize;
        let mut depth = 0i32;
        let t = loop {
            let t = self.yylex()?;
            start.get_or_insert(t.loc);
            // An unused terminator slot is token 0: the end of the body ends it too.
            if depth == 0 && (until.contains(&t.k) || t.k == K::Eof && until.len() < 3) {
                break t;
            }
            if t.is(b'(') || t.is(b'[') {
                depth += 1;
            } else if t.is(b')') || t.is(b']') {
                depth -= 1;
                if depth < 0 {
                    return Err(self.yyerror(&t, "mismatched parentheses"));
                }
            }
            if t.k == K::Eof || t.is(b';') {
                if depth != 0 {
                    return Err(self.yyerror(&t, "mismatched parentheses"));
                }
                let what = if isexpr { "expression" } else { "statement" };
                return Err(self.err_at(
                    code::SYNTAX_ERROR,
                    format!("missing \"{expected}\" at end of SQL {what}"),
                    t.loc,
                ));
            }
            end = t.loc + t.len;
        };
        self.lookup = save;
        let start = start.unwrap_or(t.loc);
        if start >= end {
            return Err(self.yyerror(
                &t,
                if isexpr {
                    "missing expression"
                } else {
                    "missing SQL statement"
                },
            ));
        }
        let text = self.src[start..end].to_string();
        if check {
            self.check_sql(&text, mode, start)?;
        }
        Ok((text, start, t))
    }

    /// `make_execsql_stmt`, from its first token.
    fn make_execsql_stmt(&mut self, first: Tok) -> Result<()> {
        let save = self.lookup;
        self.lookup = Lookup::Expr;
        let location = first.loc;
        let mut tokens = [0u8; 4];
        let mut count = 0;
        if first.k == K::Word && first.s == "create" {
            tokens[0] = b'c';
        }
        count += 1;
        let (mut paren, mut begin) = (0, 0);
        let mut in_routine = false;
        let mut prev = first.k;
        let mut into: Option<(usize, Option<usize>)> = None;
        let end = loop {
            let t = self.yylex()?;
            if let Some((_, e @ None)) = &mut into {
                *e = Some(t.loc);
            }
            if tokens[0] == b'c' && count < tokens.len() {
                if t.kw("or") {
                    tokens[count] = b'o';
                } else if t.k == K::Word && t.s == "replace" {
                    tokens[count] = b'r';
                } else if t.k == K::Word && (t.s == "function" || t.s == "procedure") {
                    tokens[count] = b'f';
                }
                if tokens[1] == b'f' || tokens[1] == b'o' && tokens[2] == b'r' && tokens[3] == b'f' {
                    in_routine = true;
                }
                count += 1;
            }
            if t.is(b'(') {
                paren += 1;
            } else if t.is(b')') && paren > 0 {
                paren -= 1;
            }
            if in_routine && paren == 0 {
                if t.kw("begin") || t.kw("case") {
                    begin += 1;
                } else if t.kw("end") && begin > 0 {
                    begin -= 1;
                }
            }
            if t.is(b';') && paren == 0 && begin == 0 {
                break t.loc;
            }
            if t.k == K::Eof {
                return Err(self.yyerror(&t, "unexpected end of function definition"));
            }
            if t.kw("into") {
                if prev == K::Kw("insert") || prev == K::Kw("merge") || first.kw("import") {
                    prev = t.k;
                    continue;
                }
                if into.is_some() {
                    return Err(self.yyerror(&t, "INTO specified more than once"));
                }
                into = Some((t.loc, None));
                self.lookup = Lookup::Normal;
                self.read_into_target(true)?;
                self.lookup = Lookup::Expr;
            }
            prev = t.k;
        };
        self.lookup = save;
        let mut text = match into {
            Some((a, b)) => {
                let b = b.unwrap_or(end);
                format!(
                    "{}{}{}",
                    &self.src[location..a],
                    " ".repeat(b - a),
                    &self.src[b..end]
                )
            }
            None => self.src[location..end].to_string(),
        };
        while text.ends_with(|c: char| c.is_ascii_whitespace()) {
            text.pop();
        }
        self.check_sql(&text, Mode::Default, location)?;
        self.lineno(location);
        Ok(())
    }

    /// `check_sql_expr`: the core parser's syntax check, its position moved into the body.
    fn check_sql(&mut self, text: &str, mode: Mode, loc: usize) -> Result<()> {
        if !self.spec.check_syntax {
            return Ok(());
        }
        let (sql, shift) = match mode {
            Mode::Default => (text.to_string(), 0),
            Mode::Expr => (format!("SELECT {text}"), 7),
            Mode::Assign => return self.check_assign(text, loc),
        };
        // Into the body: a 1-based position in `sql`.
        let at = |p: u32| {
            let b = char_to_byte(&sql, p as usize) as isize - shift;
            if b >= 0 {
                (loc as isize + b + 1) as u32
            } else {
                loc as u32 + 1
            }
        };
        let parsed = crate::sql::parse::parse_live(&sql).map(|_| ());
        for mut w in crate::sql::scs::take_warnings() {
            // (A message without a position of its own points at the construct's start:
            // plpgsql_sql_error_callback.)
            w.position = Some(w.position.map_or(loc as u32 + 1, at));
            self.env.warning(w);
        }
        parsed.map_err(|mut e| {
            e.position = Some(match e.position {
                Some(p) => at(p),
                None => loc as u32 + 1,
            });
            e
        })?;
        // An expression (`PLpgSQL_Expr`) is a SELECT's target list and clauses: no INTO, and
        // no set operation at the top.
        if mode == Mode::Expr
            && let Some((a, b)) = top_level_violation(&sql)
        {
            return Err(syntax(format!("syntax error at or near \"{}\"", &sql[a..b]))
                .position((loc as isize + a as isize - shift + 1) as u32));
        }
        Ok(())
    }

    /// An assignment (`RAW_PARSE_PLPGSQL_ASSIGNn`): the target, its subscripts and fields,
    /// `:=` or `=`, then an expression.
    fn check_assign(&mut self, text: &str, loc: usize) -> Result<()> {
        let b = text.as_bytes();
        let mut i = 0;
        let mut depth = 0;
        let mut eq = None;
        let mut quote = None;
        while i < b.len() {
            let c = b[i];
            if let Some(q) = quote {
                if c == q {
                    quote = None;
                }
                i += 1;
                continue;
            }
            match c {
                b'"' | b'\'' => quote = Some(c),
                b'[' | b'(' => depth += 1,
                b']' | b')' => depth -= 1,
                b':' if depth == 0 && b.get(i + 1) == Some(&b'=') => {
                    eq = Some((i, i + 2));
                    break;
                }
                b'=' if depth == 0 => {
                    eq = Some((i, i + 1));
                    break;
                }
                _ => {}
            }
            i += 1;
        }
        let Some((lhs, rhs)) = eq else {
            return Err(syntax("syntax error at end of input").position((loc + text.len()) as u32 + 1));
        };
        // The target is a column name with subscripts or fields, as UPDATE's SET has (a
        // parameter `$n` stands as a name of its length).
        let mut target = text[..lhs].to_string();
        if target.starts_with('$') {
            let n = target[1..].bytes().take_while(u8::is_ascii_digit).count();
            target.replace_range(..=n, &"p".repeat(n + 1));
        }
        const PREFIX: &str = "UPDATE x SET ";
        let sql = format!("{PREFIX}{target} = NULL");
        if let Err(mut e) = crate::sql::parse::parse(&sql) {
            let b = e.position.map_or(0, |p| char_to_byte(&sql, p as usize))
                .saturating_sub(PREFIX.len())
                .min(target.len());
            e.position = Some((loc + b) as u32 + 1);
            return Err(e);
        }
        self.check_sql(&text[rhs..], Mode::Expr, loc + rhs)
    }
}

struct ForVar {
    name: String,
    loc: usize,
    scalar: Option<usize>,
    row: Option<usize>,
}

/// A top-level INTO or UNION / INTERSECT / EXCEPT in `SELECT ...`: where it is.
fn top_level_violation(sql: &str) -> Option<(usize, usize)> {
    use pg_query::protobuf::Token as T;
    let scan = pg_query::scan(sql).ok()?;
    let mut depth = 0;
    for t in scan.tokens {
        match T::try_from(t.token) {
            Ok(T::Ascii40 | T::Ascii91) => depth += 1,
            Ok(T::Ascii41 | T::Ascii93) => depth -= 1,
            Ok(T::Into | T::Union | T::Intersect | T::Except) if depth == 0 => {
                return Some((t.start as usize, t.end as usize));
            }
            _ => {}
        }
    }
    None
}

fn valid_sqlstate(s: &str) -> bool {
    s.len() == 5
        && s
            .bytes()
            .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
}

/// The byte offset of 1-based character position `p` in `s`.
fn char_to_byte(s: &str, p: usize) -> usize {
    s.char_indices()
        .nth(p.saturating_sub(1))
        .map_or(s.len(), |(b, _)| b)
}
