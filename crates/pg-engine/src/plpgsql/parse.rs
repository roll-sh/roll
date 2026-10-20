//! PL/pgSQL parser: blocks, declarations and statements. Expressions and SQL statements stay
//! as source text (checked with the SQL parser) and are bound when they first run.

use crate::error::{Error, Result, code};

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    /// Unquoted identifier or keyword, lowercased.
    Word(String),
    /// "Quoted" identifier.
    QIdent(String),
    Str,
    Num,
    Param,
    Op(String),
    Punct(char),
    /// `:=`
    Assign,
    /// `..`
    DotDot,
}

#[derive(Clone, Debug)]
struct Token {
    tok: Tok,
    start: usize,
    end: usize,
}

/// An expression or SQL statement, as text.
#[derive(Clone, Debug)]
pub struct PlExpr {
    pub id: usize,
    pub text: String,
    /// Body offset of `text`'s first byte, when `text` is a verbatim slice of the body
    /// (negative when a prefix was added), for error positions.
    pub start: Option<isize>,
}

#[derive(Clone, Debug)]
pub enum TypeSpec {
    /// A SQL type name.
    Named(String),
    Record,
    /// `table%ROWTYPE`
    RowType(String),
    /// `table.column%TYPE` or `variable%TYPE`
    TypeOf(Vec<String>),
    /// `%TYPE` / `%ROWTYPE` with array decoration (`[]`, `ARRAY`).
    ArrayOf(Box<TypeSpec>),
}

#[derive(Clone, Debug)]
pub enum DeclKind {
    Var {
        ty: TypeSpec,
        constant: bool,
        not_null: bool,
        default: Option<PlExpr>,
        /// `COLLATE name` (possibly qualified); empty without one.
        collate: Vec<String>,
    },
    /// `name ALIAS FOR $n` / `ALIAS FOR other`
    Alias(String),
    /// A bound cursor: its query, parameters and [NO] SCROLL.
    Cursor {
        query: PlExpr,
        args: Vec<(String, TypeSpec)>,
        scroll: Option<bool>,
    },
}

#[derive(Clone, Debug)]
pub struct Decl {
    pub name: String,
    pub kind: DeclKind,
    pub line: u32,
    /// Where in the body each type it declares starts (a variable's; a cursor's arguments').
    pub types_at: Vec<usize>,
}

#[derive(Clone, Debug)]
pub enum Condition {
    Name(String),
    SqlState(String),
}

#[derive(Clone, Debug)]
pub struct Handler {
    pub conditions: Vec<Condition>,
    pub body: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub label: Option<String>,
    pub decls: Vec<Decl>,
    pub body: Vec<Stmt>,
    pub handlers: Vec<Handler>,
    pub line: u32,
}

/// An assignment / INTO target: `x`, `rec.field`, `label.x`, `arr[i]`.
#[derive(Clone, Debug)]
pub struct Target {
    pub parts: Vec<String>,
    pub subscripts: Vec<PlExpr>,
}

#[derive(Clone, Debug)]
pub struct IntoClause {
    pub targets: Vec<Target>,
    pub strict: bool,
}

#[derive(Clone, Debug)]
pub enum QuerySrc {
    Static(PlExpr),
    Dynamic(PlExpr, Vec<PlExpr>),
}

/// What OPEN runs.
#[derive(Clone, Debug)]
pub enum OpenSrc {
    /// A bound cursor, with argument values (named ones as `name := value`).
    Bound(Vec<(Option<String>, PlExpr)>),
    Query(PlExpr),
    Execute(PlExpr, Vec<PlExpr>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchKind {
    Next,
    Prior,
    First,
    Last,
    Absolute,
    Relative,
    Forward,
    Backward,
    ForwardAll,
    BackwardAll,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RaiseLevel {
    Debug,
    Log,
    Info,
    Notice,
    Warning,
    Exception,
}

#[derive(Clone, Debug)]
pub struct Raise {
    pub level: RaiseLevel,
    pub condition: Option<Condition>,
    pub message: Option<String>,
    pub params: Vec<PlExpr>,
    pub options: Vec<(String, PlExpr)>,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    /// `shown`: the statement as written (`x := expr`), as CONTEXT lines quote it.
    Assign {
        target: Target,
        expr: PlExpr,
        shown: String,
    },
    If {
        branches: Vec<(PlExpr, Vec<Stmt>)>,
        else_: Option<Vec<Stmt>>,
    },
    /// `CASE x WHEN ...` is rewritten to searched form over a hidden variable.
    Case {
        operand: Option<(String, PlExpr)>,
        whens: Vec<(PlExpr, Vec<Stmt>)>,
        else_: Option<Vec<Stmt>>,
    },
    Loop {
        label: Option<String>,
        body: Vec<Stmt>,
    },
    While {
        label: Option<String>,
        cond: PlExpr,
        body: Vec<Stmt>,
    },
    ForInt {
        label: Option<String>,
        var: String,
        reverse: bool,
        lower: PlExpr,
        upper: PlExpr,
        step: Option<PlExpr>,
        body: Vec<Stmt>,
    },
    ForQuery {
        label: Option<String>,
        targets: Vec<Target>,
        query: QuerySrc,
        body: Vec<Stmt>,
    },
    ForEach {
        label: Option<String>,
        targets: Vec<Target>,
        slice: usize,
        array: PlExpr,
        body: Vec<Stmt>,
    },
    Exit {
        exit: bool,
        label: Option<String>,
        cond: Option<PlExpr>,
    },
    Return(Option<PlExpr>),
    ReturnNext(Option<PlExpr>),
    ReturnQuery(QuerySrc),
    Raise(Raise),
    Assert {
        cond: PlExpr,
        message: Option<PlExpr>,
    },
    /// `shown`: the statement as CONTEXT lines quote it (INTO blanked out).
    Sql {
        sql: PlExpr,
        into: Option<IntoClause>,
        shown: String,
    },
    Perform(PlExpr),
    Execute {
        query: PlExpr,
        into: Option<IntoClause>,
        using: Vec<PlExpr>,
    },
    GetDiag {
        stacked: bool,
        items: Vec<(Target, String)>,
    },
    Block(Block),
    Open {
        cursor: String,
        scroll: Option<bool>,
        src: OpenSrc,
    },
    /// FETCH / MOVE: direction, optional count, and (FETCH) where the row goes.
    Fetch {
        cursor: String,
        kind: FetchKind,
        count: Option<PlExpr>,
        into: Option<IntoClause>,
        is_move: bool,
    },
    Close(String),
    ForCursor {
        label: Option<String>,
        targets: Vec<Target>,
        cursor: String,
        args: Vec<(Option<String>, PlExpr)>,
        body: Vec<Stmt>,
    },
    Null,
    /// COMMIT / ROLLBACK [AND [NO] CHAIN] (procedures and DO blocks).
    EndTxn {
        commit: bool,
        chain: bool,
    },
    /// `CALL proc(...)`; output parameters go back into the variables passed for them.
    Call(PlExpr),
    /// Parsed but not supported.
    Unsupported(String),
}

#[derive(Clone, Debug)]
pub struct Stmt {
    pub line: u32,
    /// Body offset of the statement's first keyword (after any label).
    pub at: usize,
    pub kind: StmtKind,
}

/// The `#variable_conflict error | use_variable | use_column` directive at the start of a body.
pub fn variable_conflict(src: &str) -> Option<crate::sql::binder::VarConflict> {
    directive(src, "variable_conflict").and_then(|v| crate::sql::binder::VarConflict::parse(&v))
}

/// The `#print_strict_params on | off` directive at the start of a body.
pub fn print_strict_params(src: &str) -> Option<bool> {
    match directive(src, "print_strict_params")?.as_str() {
        "on" => Some(true),
        "off" => Some(false),
        _ => None,
    }
}

/// The value of the last `#name value` option at the start of a body (before its first
/// block; `--` comment lines may come between).
fn directive(src: &str, name: &str) -> Option<String> {
    let mut found = None;
    let mut in_comment = false;
    for line in src.lines() {
        let mut line = line.trim();
        if in_comment {
            match line.find("*/") {
                Some(k) => {
                    in_comment = false;
                    line = line[k + 2..].trim();
                }
                None => continue,
            }
        }
        if let Some(rest) = line.strip_prefix("/*") {
            match rest.find("*/") {
                Some(k) => line = rest[k + 2..].trim(),
                None => {
                    in_comment = true;
                    continue;
                }
            }
        }
        if line.is_empty() || line.starts_with("--") {
            continue;
        }
        let Some(d) = line.strip_prefix('#') else {
            break;
        };
        let mut words = d.split_whitespace();
        if words.next().is_some_and(|w| w.eq_ignore_ascii_case(name)) {
            found = words.next().map(|w| w.to_lowercase());
        }
    }
    found
}

pub fn parse(src: &str) -> Result<Block> {
    LATEST_LINE.with(|l| l.set(1));
    let toks = lex(src).inspect_err(|_| LATEST_LINE.with(|l| l.set(line_of(src, src.len()))))?;
    let mut p = Parser {
        src,
        toks,
        pos: 0,
        next_id: 0,
        cursors: Vec::new(),
    };
    (|| {
        let label = p.opt_label()?;
        let block = p.block(label)?;
        if p.is_punct(';') {
            p.pos += 1;
        }
        if let Some(t) = p.toks.get(p.pos) {
            return Err(p.syntax_error_at(t));
        }
        Ok(block)
    })()
}

thread_local! {
    static LATEST_LINE: std::cell::Cell<u32> = const { std::cell::Cell::new(1) };
}

/// The line compiling a body last got to (`plpgsql_latest_lineno`), for errors without a
/// position.
pub fn latest_line() -> u32 {
    LATEST_LINE.with(|l| l.get())
}

/// The 1-based line of byte `at` in `src`.
fn line_of(src: &str, at: usize) -> u32 {
    src[..at.min(src.len())].matches('\n').count() as u32 + 1
}

fn syntax(msg: impl Into<String>) -> Error {
    Error::new(code::SYNTAX_ERROR, msg)
}

fn lex(src: &str) -> Result<Vec<Token>> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let is_ident_start = |c: u8| c.is_ascii_alphabetic() || c == b'_' || c >= 0x80;
    let is_ident = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80;
    // `#variable_conflict ...` style directives at the start (comments may come between).
    while i < b.len() {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < b.len() && (b[i] == b'#' || b[i..].starts_with(b"--")) {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            match src[i + 2..].find("*/") {
                Some(k) => i += k + 4,
                None => break,
            }
        } else {
            break;
        }
    }
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 0;
            while i < b.len() {
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
            if depth != 0 {
                return Err(syntax("unterminated /* comment"));
            }
            continue;
        }
        // Prefixed strings: E'..', B'..', X'..', N'..', U&'..'.
        if matches!(c.to_ascii_lowercase(), b'e' | b'b' | b'x' | b'n')
            && b.get(i + 1) == Some(&b'\'')
        {
            let escapes = c.eq_ignore_ascii_case(&b'e')
                || c.eq_ignore_ascii_case(&b'n') && !crate::sql::scs::opts().standard_strings;
            i = quoted(b, i + 1, b'\'', escapes)?;
            out.push(Token {
                tok: Tok::Str,
                start,
                end: i,
            });
            continue;
        }
        if c.eq_ignore_ascii_case(&b'u')
            && b.get(i + 1) == Some(&b'&')
            && b.get(i + 2) == Some(&b'\'')
        {
            i = quoted(b, i + 2, b'\'', false)?;
            out.push(Token {
                tok: Tok::Str,
                start,
                end: i,
            });
            continue;
        }
        if is_ident_start(c) {
            while i < b.len() && is_ident(b[i]) {
                i += 1;
            }
            // (The core scanner truncates identifiers to NAMEDATALEN - 1 bytes.)
            out.push(Token {
                tok: Tok::Word(crate::catalog::truncate_ident(&src[start..i].to_lowercase())),
                start,
                end: i,
            });
            continue;
        }
        match c {
            b'\'' => {
                i = quoted(b, i, b'\'', !crate::sql::scs::opts().standard_strings)?;
                out.push(Token {
                    tok: Tok::Str,
                    start,
                    end: i,
                });
            }
            b'"' => {
                i = quoted(b, i, b'"', false)?;
                let name = src[start + 1..i - 1].replace("\"\"", "\"");
                out.push(Token {
                    tok: Tok::QIdent(crate::catalog::truncate_ident(&name)),
                    start,
                    end: i,
                });
            }
            b'$' => {
                if b.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                    out.push(Token {
                        tok: Tok::Param,
                        start,
                        end: i,
                    });
                } else {
                    // $tag$ ... $tag$
                    let mut j = i + 1;
                    while j < b.len() && b[j] != b'$' && is_ident(b[j]) {
                        j += 1;
                    }
                    if j >= b.len() || b[j] != b'$' {
                        out.push(Token {
                            tok: Tok::Op("$".into()),
                            start,
                            end: i + 1,
                        });
                        i += 1;
                        continue;
                    }
                    let tag = &src[i..=j];
                    match src[j + 1..].find(tag) {
                        Some(k) => i = j + 1 + k + tag.len(),
                        None => return Err(syntax("unterminated dollar-quoted string")),
                    }
                    out.push(Token {
                        tok: Tok::Str,
                        start,
                        end: i,
                    });
                }
            }
            b'0'..=b'9' => {
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                if i < b.len() && b[i] == b'.' && b.get(i + 1) != Some(&b'.') {
                    i += 1;
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
                    let mut j = i + 1;
                    if j < b.len() && (b[j] == b'+' || b[j] == b'-') {
                        j += 1;
                    }
                    if j < b.len() && b[j].is_ascii_digit() {
                        i = j;
                        while i < b.len() && b[i].is_ascii_digit() {
                            i += 1;
                        }
                    }
                }
                out.push(Token {
                    tok: Tok::Num,
                    start,
                    end: i,
                });
            }
            b'.' if b.get(i + 1) == Some(&b'.') => {
                i += 2;
                out.push(Token {
                    tok: Tok::DotDot,
                    start,
                    end: i,
                });
            }
            b'.' if b.get(i + 1).is_some_and(|d| d.is_ascii_digit()) => {
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                out.push(Token {
                    tok: Tok::Num,
                    start,
                    end: i,
                });
            }
            b':' if b.get(i + 1) == Some(&b'=') => {
                i += 2;
                out.push(Token {
                    tok: Tok::Assign,
                    start,
                    end: i,
                });
            }
            b':' if b.get(i + 1) == Some(&b':') => {
                i += 2;
                out.push(Token {
                    tok: Tok::Op("::".into()),
                    start,
                    end: i,
                });
            }
            b'(' | b')' | b'[' | b']' | b',' | b';' | b'.' | b':' => {
                i += 1;
                out.push(Token {
                    tok: Tok::Punct(c as char),
                    start,
                    end: i,
                });
            }
            _ => {
                const OPS: &[u8] = b"+-*/<>=~!@#%^&|`?";
                if !OPS.contains(&c) {
                    return Err(syntax(format!("syntax error at or near \"{}\"", c as char)));
                }
                while i < b.len()
                    && OPS.contains(&b[i])
                    && !(b[i] == b'-' && b.get(i + 1) == Some(&b'-'))
                    && !(b[i] == b'/' && b.get(i + 1) == Some(&b'*'))
                {
                    i += 1;
                }
                out.push(Token {
                    tok: Tok::Op(src[start..i].to_string()),
                    start,
                    end: i,
                });
            }
        }
    }
    Ok(out)
}

/// Skip a quoted literal starting at `b[i] == q`; returns the index after it.
fn quoted(b: &[u8], mut i: usize, q: u8, backslash: bool) -> Result<usize> {
    i += 1;
    while i < b.len() {
        if backslash && b[i] == b'\\' {
            i += 2;
            continue;
        }
        if b[i] == q {
            if b.get(i + 1) == Some(&q) {
                i += 2;
                continue;
            }
            return Ok(i + 1);
        }
        i += 1;
    }
    Err(syntax(if q == b'"' {
        "unterminated quoted identifier"
    } else {
        "unterminated quoted string"
    }))
}

/// The value of a string literal token.
pub fn string_value(raw: &str) -> String {
    if let Some(rest) = raw.strip_prefix('$') {
        let tag_end = rest.find('$').map(|k| k + 2).unwrap_or(1);
        return raw[tag_end..raw.len() - tag_end].to_string();
    }
    // Ordinary literals have escapes with standard_conforming_strings off.
    let plain = !crate::sql::scs::opts().standard_strings;
    let (escapes, body) = match raw.as_bytes()[0].to_ascii_lowercase() {
        b'e' => (true, &raw[2..raw.len() - 1]),
        b'n' => (plain, &raw[2..raw.len() - 1]),
        b'b' | b'x' => (false, &raw[2..raw.len() - 1]),
        b'u' => (false, &raw[3..raw.len() - 1]),
        _ => (plain, &raw[1..raw.len() - 1]),
    };
    if !escapes {
        return body.replace("''", "'");
    }
    crate::sql::scs::unescape(body)
}

struct Parser<'s> {
    src: &'s str,
    toks: Vec<Token>,
    pos: usize,
    next_id: usize,
    /// Bound cursors declared so far (`FOR r IN cur LOOP` loops over one).
    /// Bound cursors declared so far, with their parameter names.
    cursors: Vec<(String, Vec<String>)>,
}

const BLOCK_ENDS: &[&str] = &["end", "exception"];

impl<'s> Parser<'s> {
    fn line_at(&self, byte: usize) -> u32 {
        self.src[..byte.min(self.src.len())]
            .bytes()
            .filter(|&c| c == b'\n')
            .count() as u32
            + 1
    }

    fn line(&self) -> u32 {
        self.toks
            .get(self.pos)
            .map(|t| self.line_at(t.start))
            .unwrap_or_else(|| self.line_at(self.src.len()))
    }

    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|t| &t.tok)
    }

    fn peek_at(&self, k: usize) -> Option<&Tok> {
        self.toks.get(self.pos + k).map(|t| &t.tok)
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Word(w)) if w == kw)
    }

    fn is_punct(&self, c: char) -> bool {
        matches!(self.peek(), Some(Tok::Punct(p)) if *p == c)
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn syntax_error_at(&self, t: &Token) -> Error {
        syntax(format!(
            "syntax error at or near \"{}\"",
            &self.src[t.start..t.end]
        ))
        .position(t.start as u32 + 1)
    }

    fn eof(&self) -> Error {
        syntax("syntax error at end of input").position(self.src.len() as u32 + 1)
    }

    fn unexpected(&self) -> Error {
        match self.toks.get(self.pos) {
            Some(t) => self.syntax_error_at(t),
            None => self.eof(),
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn expect_punct(&mut self, c: char) -> Result<()> {
        if self.is_punct(c) {
            self.pos += 1;
            Ok(())
        } else {
            Err(self.unexpected())
        }
    }

    fn ident(&mut self) -> Result<String> {
        match self.peek() {
            Some(Tok::Word(w)) => {
                let w = w.clone();
                self.pos += 1;
                Ok(w)
            }
            Some(Tok::QIdent(w)) => {
                let w = w.clone();
                self.pos += 1;
                Ok(w)
            }
            _ => Err(self.unexpected()),
        }
    }

    fn expr(&mut self, start: usize, end: usize) -> PlExpr {
        self.next_id += 1;
        let raw = &self.src[start..end];
        let lead = raw.len() - raw.trim_start().len();
        PlExpr {
            id: self.next_id,
            text: raw.trim().to_string(),
            start: Some((start + lead) as isize),
        }
    }

    /// Read source text up to (not including) a terminator at nesting depth 0.
    fn until(&mut self, stop: &dyn Fn(&Tok) -> bool) -> Result<(usize, usize)> {
        let start = self
            .toks
            .get(self.pos)
            .map(|t| t.start)
            .unwrap_or(self.src.len());
        let mut depth = 0i32;
        let mut end = start;
        loop {
            let Some(t) = self.toks.get(self.pos) else {
                return Err(self.eof());
            };
            if depth == 0 && stop(&t.tok) {
                break;
            }
            match t.tok {
                Tok::Punct('(') | Tok::Punct('[') => depth += 1,
                Tok::Punct(')') | Tok::Punct(']') => depth -= 1,
                _ => {}
            }
            end = t.end;
            self.pos += 1;
        }
        Ok((start, end))
    }

    fn expr_until(&mut self, stop: &dyn Fn(&Tok) -> bool) -> Result<PlExpr> {
        let at = self.pos;
        let (s, e) = self.until(stop)?;
        if self.pos == at {
            return Err(self.unexpected());
        }
        Ok(self.expr(s, e))
    }

    fn expr_to_semi(&mut self) -> Result<PlExpr> {
        let e = self.expr_until(&|t| *t == Tok::Punct(';'))?;
        self.expect_punct(';')?;
        Ok(e)
    }

    fn block(&mut self, label: Option<String>) -> Result<Block> {
        let line = self.line();
        let mut decls = Vec::new();
        if self.eat_kw("declare") {
            while !self.is_kw("begin") {
                if self.peek().is_none() {
                    return Err(self.unexpected());
                }
                if self.eat_kw("declare") {
                    continue;
                }
                decls.push(self.decl()?);
            }
        }
        self.expect_kw("begin")?;
        let body = self.stmts(BLOCK_ENDS)?;
        let mut handlers = Vec::new();
        if self.eat_kw("exception") {
            while self.eat_kw("when") {
                let mut conditions = Vec::new();
                loop {
                    if self.eat_kw("sqlstate") {
                        let Some(Token {
                            tok: Tok::Str,
                            start,
                            end,
                        }) = self.toks.get(self.pos).cloned()
                        else {
                            return Err(self.unexpected());
                        };
                        self.pos += 1;
                        let st = string_value(&self.src[start..end]);
                        if st.len() != 5
                            || !st
                                .bytes()
                                .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
                        {
                            return Err(Error::new(
                                code::SYNTAX_ERROR,
                                format!("invalid SQLSTATE code \"{st}\""),
                            ));
                        }
                        conditions.push(Condition::SqlState(st));
                    } else {
                        let name = self.ident()?;
                        if name != "others"
                            && !super::conditions::CONDITIONS
                                .iter()
                                .any(|(n, _)| *n == name)
                        {
                            return Err(Error::new(
                                code::UNDEFINED_OBJECT,
                                format!("unrecognized exception condition \"{name}\""),
                            ));
                        }
                        conditions.push(Condition::Name(name));
                    }
                    if !self.eat_kw("or") {
                        break;
                    }
                }
                self.expect_kw("then")?;
                let body = self.stmts(&["when", "end"])?;
                handlers.push(Handler { conditions, body });
            }
            if handlers.is_empty() {
                return Err(self.unexpected());
            }
        }
        self.expect_kw("end")?;
        self.end_label(&label)?;
        Ok(Block {
            label,
            decls,
            body,
            handlers,
            line,
        })
    }

    /// Optional label after END / END LOOP; it must match the opening label.
    fn end_label(&mut self, label: &Option<String>) -> Result<()> {
        if matches!(self.peek(), Some(Tok::Word(_)) | Some(Tok::QIdent(_))) {
            let at = self.pos;
            let l = self.ident()?;
            match label {
                Some(x) if *x == l => {}
                Some(x) => {
                    return Err(syntax(format!(
                        "end label \"{l}\" differs from block's label \"{x}\""
                    )));
                }
                None => {
                    let _ = at;
                    return Err(syntax(format!(
                        "end label \"{l}\" specified for unlabeled block"
                    )));
                }
            }
        }
        Ok(())
    }

    fn decl(&mut self) -> Result<Decl> {
        let line = self.line();
        let name = self.ident()?;
        if self.eat_kw("alias") {
            self.expect_kw("for")?;
            let target = match self.toks.get(self.pos).cloned() {
                Some(Token {
                    tok: Tok::Param,
                    start,
                    end,
                }) => {
                    self.pos += 1;
                    self.src[start..end].to_string()
                }
                _ => {
                    // `name`, or a label-qualified `label.name`.
                    let mut t = self.ident()?;
                    while self.is_punct('.') {
                        self.pos += 1;
                        t = format!("{t}.{}", self.ident()?);
                    }
                    t
                }
            };
            self.expect_punct(';')?;
            return Ok(Decl {
                name,
                kind: DeclKind::Alias(target),
                line,
                types_at: Vec::new(),
            });
        }
        let save = self.pos;
        let scroll = if self.eat_kw("no") {
            self.eat_kw("scroll").then_some(false)
        } else {
            self.eat_kw("scroll").then_some(true)
        };
        if self.eat_kw("cursor") {
            let mut args = Vec::new();
            let mut types_at = Vec::new();
            if self.is_punct('(') {
                self.pos += 1;
                loop {
                    let arg = self.ident()?;
                    let (s, e) = self.until(&|t| matches!(t, Tok::Punct(',') | Tok::Punct(')')))?;
                    let text = &self.src[s..e];
                    types_at.push(s + text.len() - text.trim_start().len());
                    args.push((arg, type_spec(text.trim())));
                    if self.is_punct(',') {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                self.expect_punct(')')?;
            }
            if !self.eat_kw("for") {
                self.expect_kw("is")?;
            }
            let query = self.expr_to_semi()?;
            self.cursors
                .push((name.clone(), args.iter().map(|a| a.0.clone()).collect()));
            return Ok(Decl {
                name,
                kind: DeclKind::Cursor {
                    query,
                    args,
                    scroll,
                },
                line,
                types_at,
            });
        }
        self.pos = save;
        let constant = self.eat_kw("constant");
        let (s, e) = self.until(&|t| {
            matches!(t, Tok::Punct(';') | Tok::Assign)
                || matches!(t, Tok::Op(o) if o == "=")
                || matches!(t, Tok::Word(w) if w == "default" || w == "not" || w == "collate")
        })?;
        if s == e {
            return Err(self.unexpected());
        }
        let ty_text = self.src[s..e].trim().to_string();
        let ty_at = s + self.src[s..e].len() - self.src[s..e].trim_start().len();
        let mut collate = Vec::new();
        if self.eat_kw("collate") {
            collate.push(self.ident()?);
            // A schema-qualified collation.
            while self.is_punct('.') {
                self.pos += 1;
                collate.push(self.ident()?);
            }
        }
        let mut not_null = false;
        if self.eat_kw("not") {
            self.expect_kw("null")?;
            not_null = true;
        }
        let has_default = if self.eat_kw("default") {
            true
        } else if matches!(self.peek(), Some(Tok::Assign))
            || matches!(self.peek(), Some(Tok::Op(o)) if o == "=")
        {
            self.pos += 1;
            true
        } else {
            false
        };
        let default = if has_default {
            Some(self.expr_until(&|t| *t == Tok::Punct(';'))?)
        } else {
            None
        };
        self.expect_punct(';')?;
        if not_null && default.is_none() {
            return Err(syntax(format!(
                "variable \"{name}\" must have a default value, since it's declared NOT NULL"
            )));
        }
        let ty = type_spec(&ty_text);
        Ok(Decl {
            name,
            kind: DeclKind::Var {
                ty,
                constant,
                not_null,
                default,
                collate,
            },
            line,
            types_at: vec![ty_at],
        })
    }

    fn stmts(&mut self, ends: &[&str]) -> Result<Vec<Stmt>> {
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => return Err(self.unexpected()),
                Some(Tok::Word(w)) if ends.contains(&w.as_str()) => return Ok(out),
                _ => out.push(self.stmt()?),
            }
        }
    }

    fn opt_label(&mut self) -> Result<Option<String>> {
        if matches!(self.peek(), Some(Tok::Op(o)) if o == "<<") {
            self.pos += 1;
            let l = self.ident()?;
            match self.peek() {
                Some(Tok::Op(o)) if o == ">>" => self.pos += 1,
                _ => return Err(self.unexpected()),
            }
            return Ok(Some(l));
        }
        Ok(None)
    }

    fn end_loop(&mut self, label: &Option<String>) -> Result<()> {
        self.expect_kw("end")?;
        self.expect_kw("loop")?;
        self.end_label(label)?;
        self.expect_punct(';')
    }

    fn stmt(&mut self) -> Result<Stmt> {
        let line = self.line();
        LATEST_LINE.with(|l| l.set(line));
        let label = self.opt_label()?;
        let at = self
            .toks
            .get(self.pos)
            .map(|t| t.start)
            .unwrap_or(self.src.len());
        let kw = match self.peek() {
            Some(Tok::Word(w)) => w.clone(),
            _ if label.is_some() => return Err(self.unexpected()),
            _ => String::new(),
        };
        if label.is_some()
            && !matches!(
                kw.as_str(),
                "begin" | "declare" | "loop" | "while" | "for" | "foreach"
            )
        {
            return Err(self.unexpected());
        }
        // An unreserved keyword can name a variable: `return := return + 1` assigns.
        let reserved = matches!(
            kw.as_str(),
            "all" | "begin" | "by" | "case" | "declare" | "else" | "end" | "execute" | "for"
                | "foreach" | "from" | "if" | "in" | "into" | "loop" | "not" | "null" | "or"
                | "strict" | "then" | "to" | "using" | "when" | "while"
        );
        let kw = if !kw.is_empty() && !reserved && matches!(self.peek_at(1), Some(Tok::Assign)) {
            String::new()
        } else {
            kw
        };
        let kind = match kw.as_str() {
            "begin" | "declare" => {
                let b = self.block(label)?;
                self.expect_punct(';')?;
                StmtKind::Block(b)
            }
            "if" => {
                self.pos += 1;
                let mut branches = Vec::new();
                let cond = self.expr_until(&|t| matches!(t, Tok::Word(w) if w == "then"))?;
                self.pos += 1;
                let body = self.stmts(&["elsif", "elseif", "else", "end"])?;
                branches.push((cond, body));
                let mut else_ = None;
                loop {
                    if self.eat_kw("elsif") || self.eat_kw("elseif") {
                        let cond =
                            self.expr_until(&|t| matches!(t, Tok::Word(w) if w == "then"))?;
                        self.pos += 1;
                        let body = self.stmts(&["elsif", "elseif", "else", "end"])?;
                        branches.push((cond, body));
                    } else if self.eat_kw("else") {
                        else_ = Some(self.stmts(&["end"])?);
                    } else {
                        break;
                    }
                }
                self.expect_kw("end")?;
                self.expect_kw("if")?;
                self.expect_punct(';')?;
                StmtKind::If { branches, else_ }
            }
            "case" => {
                self.pos += 1;
                let operand = if self.is_kw("when") {
                    None
                } else {
                    let e = self.expr_until(&|t| matches!(t, Tok::Word(w) if w == "when"))?;
                    Some((format!("__plpgsql_case_{}", e.id), e))
                };
                let mut whens = Vec::new();
                while self.eat_kw("when") {
                    let (s, e) = self.until(&|t| matches!(t, Tok::Word(w) if w == "then"))?;
                    self.pos += 1;
                    let cond = match &operand {
                        Some((var, _)) => {
                            let list = self.src[s..e].trim().to_string();
                            self.next_id += 1;
                            PlExpr {
                                id: self.next_id,
                                text: format!("{var} IN ({list})"),
                                start: None,
                            }
                        }
                        None => self.expr(s, e),
                    };
                    let body = self.stmts(&["when", "else", "end"])?;
                    whens.push((cond, body));
                }
                if whens.is_empty() {
                    return Err(self.unexpected());
                }
                let else_ = if self.eat_kw("else") {
                    Some(self.stmts(&["end"])?)
                } else {
                    None
                };
                self.expect_kw("end")?;
                self.expect_kw("case")?;
                self.expect_punct(';')?;
                StmtKind::Case {
                    operand,
                    whens,
                    else_,
                }
            }
            "loop" => {
                self.pos += 1;
                let body = self.stmts(&["end"])?;
                self.end_loop(&label)?;
                StmtKind::Loop { label, body }
            }
            "while" => {
                self.pos += 1;
                let cond = self.expr_until(&|t| matches!(t, Tok::Word(w) if w == "loop"))?;
                self.pos += 1;
                let body = self.stmts(&["end"])?;
                self.end_loop(&label)?;
                StmtKind::While { label, cond, body }
            }
            "for" => self.for_loop(label)?,
            "foreach" => {
                self.pos += 1;
                let targets = self.targets()?;
                let mut slice = 0;
                if self.eat_kw("slice") {
                    let Some(Token {
                        tok: Tok::Num,
                        start,
                        end,
                    }) = self.toks.get(self.pos).cloned()
                    else {
                        return Err(self.unexpected());
                    };
                    self.pos += 1;
                    slice = self.src[start..end]
                        .parse()
                        .map_err(|_| self.unexpected())?;
                }
                self.expect_kw("in")?;
                self.expect_kw("array")?;
                let array = self.expr_until(&|t| matches!(t, Tok::Word(w) if w == "loop"))?;
                self.pos += 1;
                let body = self.stmts(&["end"])?;
                self.end_loop(&label)?;
                StmtKind::ForEach {
                    label,
                    targets,
                    slice,
                    array,
                    body,
                }
            }
            "exit" | "continue" => {
                self.pos += 1;
                let label = if matches!(self.peek(), Some(Tok::Word(w)) if w != "when")
                    || matches!(self.peek(), Some(Tok::QIdent(_)))
                {
                    Some(self.ident()?)
                } else {
                    None
                };
                let cond = if self.eat_kw("when") {
                    Some(self.expr_until(&|t| *t == Tok::Punct(';'))?)
                } else {
                    None
                };
                self.expect_punct(';')?;
                StmtKind::Exit {
                    exit: kw == "exit",
                    label,
                    cond,
                }
            }
            "return" => {
                self.pos += 1;
                if self.eat_kw("next") {
                    if self.is_punct(';') {
                        self.pos += 1;
                        StmtKind::ReturnNext(None)
                    } else {
                        StmtKind::ReturnNext(Some(self.expr_to_semi()?))
                    }
                } else if self.eat_kw("query") {
                    if self.eat_kw("execute") {
                        let q = self.expr_until(&|t| {
                            *t == Tok::Punct(';') || matches!(t, Tok::Word(w) if w == "using")
                        })?;
                        let using = self.using()?;
                        self.expect_punct(';')?;
                        StmtKind::ReturnQuery(QuerySrc::Dynamic(q, using))
                    } else {
                        StmtKind::ReturnQuery(QuerySrc::Static(self.expr_to_semi()?))
                    }
                } else if self.is_punct(';') {
                    self.pos += 1;
                    StmtKind::Return(None)
                } else {
                    StmtKind::Return(Some(self.expr_to_semi()?))
                }
            }
            "raise" => self.raise()?,
            "assert" => {
                self.pos += 1;
                let cond = self.expr_until(&|t| matches!(t, Tok::Punct(';') | Tok::Punct(',')))?;
                let message = if self.is_punct(',') {
                    self.pos += 1;
                    Some(self.expr_until(&|t| *t == Tok::Punct(';'))?)
                } else {
                    None
                };
                self.expect_punct(';')?;
                StmtKind::Assert { cond, message }
            }
            "perform" if matches!(self.peek_at(1), Some(Tok::Punct(';'))) => {
                // `PERFORM;` runs `SELECT`.
                self.pos += 2;
                self.next_id += 1;
                StmtKind::Perform(PlExpr {
                    id: self.next_id,
                    text: "SELECT".into(),
                    start: Some(at as isize - 1),
                })
            }
            "perform" => {
                self.pos += 1;
                let e = self.expr_to_semi()?;
                StmtKind::Perform(PlExpr {
                    id: e.id,
                    text: format!("SELECT {}", e.text),
                    start: e.start.map(|s| s - 7),
                })
            }
            "execute" => {
                self.pos += 1;
                let query = self.expr_until(&|t| {
                    *t == Tok::Punct(';')
                        || matches!(t, Tok::Word(w) if w == "into" || w == "using")
                })?;
                let mut into = None;
                let mut using = Vec::new();
                loop {
                    if self.eat_kw("into") {
                        let strict = self.eat_kw("strict");
                        into = Some(IntoClause {
                            targets: self.targets()?,
                            strict,
                        });
                    } else if self.is_kw("using") {
                        using = self.using()?;
                    } else {
                        break;
                    }
                }
                self.expect_punct(';')?;
                StmtKind::Execute { query, into, using }
            }
            "get" => {
                self.pos += 1;
                let stacked = self.eat_kw("stacked");
                if !stacked {
                    let _ = self.eat_kw("current");
                }
                self.expect_kw("diagnostics")?;
                let mut items = Vec::new();
                loop {
                    let target = self.target()?;
                    match self.peek() {
                        Some(Tok::Assign) => self.pos += 1,
                        Some(Tok::Op(o)) if o == "=" => self.pos += 1,
                        _ => return Err(self.unexpected()),
                    }
                    let item = self.ident()?.to_uppercase();
                    items.push((target, item));
                    if self.is_punct(',') {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
                self.expect_punct(';')?;
                StmtKind::GetDiag { stacked, items }
            }
            "null" if matches!(self.peek_at(1), Some(Tok::Punct(';'))) => {
                self.pos += 2;
                StmtKind::Null
            }
            "open" => {
                self.pos += 1;
                let cursor = self.ident()?;
                let bound = self.cursors.iter().any(|c| c.0 == cursor);
                let scroll = if bound {
                    None
                } else if self.eat_kw("no") {
                    self.expect_kw("scroll")?;
                    Some(false)
                } else {
                    self.eat_kw("scroll").then_some(true)
                };
                let src = if !bound && self.eat_kw("for") {
                    if self.eat_kw("execute") {
                        let q = self.expr_until(&|t| {
                            *t == Tok::Punct(';') || matches!(t, Tok::Word(w) if w == "using")
                        })?;
                        let using = self.using()?;
                        self.expect_punct(';')?;
                        OpenSrc::Execute(q, using)
                    } else {
                        OpenSrc::Query(self.expr_to_semi()?)
                    }
                } else {
                    let args = self.cursor_args(&cursor)?;
                    self.expect_punct(';')?;
                    OpenSrc::Bound(args)
                };
                StmtKind::Open {
                    cursor,
                    scroll,
                    src,
                }
            }
            "fetch" | "move" => {
                let is_move = kw == "move";
                self.pos += 1;
                let from_in = |t: &Tok| matches!(t, Tok::Word(w) if w == "from" || w == "in");
                let word = match self.peek() {
                    Some(Tok::Word(w)) => w.clone(),
                    _ => String::new(),
                };
                // A direction only when something follows it before the cursor name.
                let followed = !matches!(self.peek_at(1), Some(Tok::Punct(';')))
                    && !matches!(self.peek_at(1), Some(Tok::Word(w)) if w == "into");
                let mut count = None;
                let kind = match word.as_str() {
                    "next" if followed => FetchKind::Next,
                    "prior" if followed => FetchKind::Prior,
                    "first" if followed => FetchKind::First,
                    "last" if followed => FetchKind::Last,
                    "absolute" | "relative" if followed => {
                        self.pos += 1;
                        count = Some(self.expr_until(&from_in)?);
                        self.pos -= 1;
                        if word == "absolute" {
                            FetchKind::Absolute
                        } else {
                            FetchKind::Relative
                        }
                    }
                    "forward" | "backward" if followed => {
                        let fwd = word == "forward";
                        if matches!(self.peek_at(1), Some(Tok::Word(w)) if w == "all") {
                            self.pos += 1;
                            if fwd {
                                FetchKind::ForwardAll
                            } else {
                                FetchKind::BackwardAll
                            }
                        } else {
                            if !matches!(self.peek_at(1), Some(t) if from_in(t)) {
                                self.pos += 1;
                                count = Some(self.expr_until(&from_in)?);
                                self.pos -= 1;
                            }
                            if fwd {
                                FetchKind::Forward
                            } else {
                                FetchKind::Backward
                            }
                        }
                    }
                    _ => {
                        self.pos -= 1;
                        FetchKind::Next
                    }
                };
                self.pos += 1;
                let _ = self.eat_kw("from") || self.eat_kw("in");
                let cursor = self.ident()?;
                let into = if is_move {
                    None
                } else {
                    self.expect_kw("into")?;
                    let strict = self.eat_kw("strict");
                    Some(IntoClause {
                        targets: self.targets()?,
                        strict,
                    })
                };
                self.expect_punct(';')?;
                StmtKind::Fetch {
                    cursor,
                    kind,
                    count,
                    into,
                    is_move,
                }
            }
            "close" => {
                self.pos += 1;
                let cursor = self.ident()?;
                self.expect_punct(';')?;
                StmtKind::Close(cursor)
            }
            "commit" | "rollback" => {
                self.pos += 1;
                let chain = if self.eat_kw("and") {
                    let no = self.eat_kw("no");
                    self.expect_kw("chain")?;
                    !no
                } else {
                    false
                };
                self.expect_punct(';')?;
                StmtKind::EndTxn {
                    commit: kw == "commit",
                    chain,
                }
            }
            "call" => StmtKind::Call(self.expr_to_semi()?),
            _ if self.is_assignment() => {
                let from = self.toks[self.pos].start;
                let target = self.target()?;
                self.pos += 1; // := or =
                let expr = self.expr_to_semi()?;
                let to = self.toks[self.pos - 2].end.max(from);
                StmtKind::Assign {
                    target,
                    expr,
                    shown: self.src[from..to].to_string(),
                }
            }
            _ => self.sql_stmt()?,
        };
        Ok(Stmt { line, at, kind })
    }

    /// `(value, name := value, ...)` after a bound cursor's name; empty when absent.
    /// A bound cursor's arguments, checked against its parameters and put in their order.
    fn cursor_args(&mut self, cursor: &str) -> Result<Vec<(Option<String>, PlExpr)>> {
        let params = self
            .cursors
            .iter()
            .rev()
            .find(|c| c.0 == cursor)
            .map(|c| c.1.clone())
            .unwrap_or_default();
        let here = |p: &Parser| p.toks.get(p.pos).map_or(p.src.len(), |t| t.start) as u32 + 1;
        if params.is_empty() {
            if self.is_punct('(') {
                return Err(
                    syntax(format!("cursor \"{cursor}\" has no arguments")).position(here(self))
                );
            }
            return Ok(Vec::new());
        }
        if !self.is_punct('(') {
            return Err(syntax(format!("cursor \"{cursor}\" has arguments")).position(here(self)));
        }
        self.pos += 1;
        let mut args: Vec<Option<PlExpr>> = vec![None; params.len()];
        for i in 0..params.len() {
            let named = match (self.peek(), self.peek_at(1)) {
                (Some(Tok::Word(w) | Tok::QIdent(w)), Some(Tok::Assign)) => Some(w.clone()),
                (Some(Tok::Word(w) | Tok::QIdent(w)), Some(Tok::Op(o))) if o == "=>" => {
                    Some(w.clone())
                }
                _ => None,
            };
            let pos = here(self);
            let at = match named {
                Some(w) => {
                    let Some(at) = params.iter().position(|p| *p == w) else {
                        return Err(syntax(format!(
                            "cursor \"{cursor}\" has no argument named \"{w}\""
                        ))
                        .position(pos));
                    };
                    self.pos += 2;
                    at
                }
                None => i,
            };
            if args[at].is_some() {
                return Err(syntax(format!(
                    "value for parameter \"{}\" of cursor \"{cursor}\" specified more than once",
                    params[at]
                ))
                .position(pos));
            }
            args[at] = Some(self.expr_until(&|t| matches!(t, Tok::Punct(',') | Tok::Punct(')')))?);
            let last = i + 1 == params.len();
            if self.is_punct(')') && !last {
                return Err(
                    syntax(format!("not enough arguments for cursor \"{cursor}\""))
                        .position(here(self)),
                );
            }
            if self.is_punct(',') && last {
                return Err(
                    syntax(format!("too many arguments for cursor \"{cursor}\""))
                        .position(here(self)),
                );
            }
            if !last {
                self.expect_punct(',')?;
            }
        }
        self.expect_punct(')')?;
        Ok(args
            .into_iter()
            .map(|a| (None, a.expect("every parameter has a value")))
            .collect())
    }

    fn using(&mut self) -> Result<Vec<PlExpr>> {
        let mut out = Vec::new();
        if self.eat_kw("using") {
            loop {
                out.push(self.expr_until(&|t| {
                    matches!(t, Tok::Punct(';') | Tok::Punct(','))
                        || matches!(t, Tok::Word(w) if w == "into" || w == "loop")
                })?);
                if self.is_punct(',') {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        Ok(out)
    }

    /// `ident(.ident)*([expr])* :=`
    fn is_assignment(&self) -> bool {
        let mut i = self.pos;
        let t = |i: usize| self.toks.get(i).map(|t| &t.tok);
        if !matches!(t(i), Some(Tok::Word(_)) | Some(Tok::QIdent(_)) | Some(Tok::Param)) {
            return false;
        }
        i += 1;
        while matches!(t(i), Some(Tok::Punct('.')))
            && matches!(t(i + 1), Some(Tok::Word(_)) | Some(Tok::QIdent(_)))
        {
            i += 2;
        }
        while matches!(t(i), Some(Tok::Punct('['))) {
            let mut depth = 0;
            loop {
                match t(i) {
                    Some(Tok::Punct('[')) => depth += 1,
                    Some(Tok::Punct(']')) => {
                        depth -= 1;
                        if depth == 0 {
                            i += 1;
                            break;
                        }
                    }
                    None => return false,
                    _ => {}
                }
                i += 1;
            }
        }
        matches!(t(i), Some(Tok::Assign)) || matches!(t(i), Some(Tok::Op(o)) if o == "=")
    }

    fn target(&mut self) -> Result<Target> {
        // (`$n` names a parameter too.)
        let first = match self.toks.get(self.pos) {
            Some(Token {
                tok: Tok::Param,
                start,
                end,
            }) => {
                let s = self.src[*start..*end].to_string();
                self.pos += 1;
                s
            }
            _ => self.ident()?,
        };
        let mut parts = vec![first];
        while self.is_punct('.')
            && matches!(self.peek_at(1), Some(Tok::Word(_)) | Some(Tok::QIdent(_)))
        {
            self.pos += 1;
            parts.push(self.ident()?);
        }
        let mut subscripts = Vec::new();
        while self.is_punct('[') {
            self.pos += 1;
            subscripts.push(self.expr_until(&|t| *t == Tok::Punct(']'))?);
            self.pos += 1;
        }
        Ok(Target { parts, subscripts })
    }

    fn targets(&mut self) -> Result<Vec<Target>> {
        let mut out = vec![self.target()?];
        while self.is_punct(',') {
            self.pos += 1;
            out.push(self.target()?);
        }
        Ok(out)
    }

    fn for_loop(&mut self, label: Option<String>) -> Result<StmtKind> {
        self.pos += 1;
        let targets = self.targets()?;
        self.expect_kw("in")?;
        let reverse = self.eat_kw("reverse");
        if self.eat_kw("execute") {
            let q =
                self.expr_until(&|t| matches!(t, Tok::Word(w) if w == "loop" || w == "using"))?;
            let using = self.using()?;
            self.expect_kw("loop")?;
            let body = self.stmts(&["end"])?;
            self.end_loop(&label)?;
            return Ok(StmtKind::ForQuery {
                label,
                targets,
                query: QuerySrc::Dynamic(q, using),
                body,
            });
        }
        // Integer loop when `..` appears before LOOP.
        let mut i = self.pos;
        let mut depth = 0;
        let mut is_int = false;
        while let Some(t) = self.toks.get(i) {
            match &t.tok {
                Tok::Punct('(') | Tok::Punct('[') => depth += 1,
                Tok::Punct(')') | Tok::Punct(']') => depth -= 1,
                Tok::DotDot if depth == 0 => {
                    is_int = true;
                    break;
                }
                Tok::Word(w) if depth == 0 && w == "loop" => break,
                _ => {}
            }
            i += 1;
        }
        if is_int {
            if targets.len() != 1 || targets[0].parts.len() != 1 {
                return Err(syntax(
                    "integer FOR loop must have only one target variable",
                ));
            }
            let lower = self.expr_until(&|t| *t == Tok::DotDot)?;
            self.pos += 1;
            let upper =
                self.expr_until(&|t| matches!(t, Tok::Word(w) if w == "loop" || w == "by"))?;
            let step = if self.eat_kw("by") {
                Some(self.expr_until(&|t| matches!(t, Tok::Word(w) if w == "loop"))?)
            } else {
                None
            };
            self.expect_kw("loop")?;
            let body = self.stmts(&["end"])?;
            self.end_loop(&label)?;
            return Ok(StmtKind::ForInt {
                label,
                var: targets[0].parts[0].clone(),
                reverse,
                lower,
                upper,
                step,
                body,
            });
        }
        if reverse {
            return Err(self.unexpected());
        }
        if let Some(Tok::Word(w) | Tok::QIdent(w)) = self.peek()
            && self.cursors.iter().any(|c| c.0 == *w)
            && (matches!(self.peek_at(1), Some(Tok::Punct('(')))
                || matches!(self.peek_at(1), Some(Tok::Word(l)) if l == "loop"))
        {
            let cursor = w.clone();
            self.pos += 1;
            let args = self.cursor_args(&cursor)?;
            self.expect_kw("loop")?;
            let body = self.stmts(&["end"])?;
            self.end_loop(&label)?;
            return Ok(StmtKind::ForCursor {
                label,
                targets,
                cursor,
                args,
                body,
            });
        }
        let query = self.expr_until(&|t| matches!(t, Tok::Word(w) if w == "loop"))?;
        self.pos += 1;
        let body = self.stmts(&["end"])?;
        self.end_loop(&label)?;
        Ok(StmtKind::ForQuery {
            label,
            targets,
            query: QuerySrc::Static(query),
            body,
        })
    }

    fn raise(&mut self) -> Result<StmtKind> {
        self.pos += 1;
        if self.is_punct(';') {
            self.pos += 1;
            return Ok(StmtKind::Raise(Raise {
                level: RaiseLevel::Exception,
                condition: None,
                message: None,
                params: Vec::new(),
                options: Vec::new(),
            }));
        }
        let level = match self.peek() {
            Some(Tok::Word(w)) => match w.as_str() {
                "debug" => Some(RaiseLevel::Debug),
                "log" => Some(RaiseLevel::Log),
                "info" => Some(RaiseLevel::Info),
                "notice" => Some(RaiseLevel::Notice),
                "warning" => Some(RaiseLevel::Warning),
                "exception" => Some(RaiseLevel::Exception),
                _ => None,
            },
            _ => None,
        };
        if level.is_some() {
            self.pos += 1;
        }
        let mut r = Raise {
            level: level.unwrap_or(RaiseLevel::Exception),
            condition: None,
            message: None,
            params: Vec::new(),
            options: Vec::new(),
        };
        match self.toks.get(self.pos).cloned() {
            Some(Token {
                tok: Tok::Str,
                start,
                end,
            }) => {
                self.pos += 1;
                let msg = string_value(&self.src[start..end]);
                while self.is_punct(',') {
                    self.pos += 1;
                    r.params.push(self.expr_until(&|t| {
                        matches!(t, Tok::Punct(';') | Tok::Punct(','))
                            || matches!(t, Tok::Word(w) if w == "using")
                    })?);
                }
                let want = placeholders(&msg);
                if want > r.params.len() {
                    return Err(syntax("too few parameters specified for RAISE"));
                }
                if want < r.params.len() {
                    return Err(syntax("too many parameters specified for RAISE"));
                }
                r.message = Some(msg);
            }
            Some(Token {
                tok: Tok::Word(w), ..
            }) if w == "sqlstate" => {
                self.pos += 1;
                let Some(Token {
                    tok: Tok::Str,
                    start,
                    end,
                }) = self.toks.get(self.pos).cloned()
                else {
                    return Err(self.unexpected());
                };
                self.pos += 1;
                let st = string_value(&self.src[start..end]);
                if st.len() != 5
                    || !st
                        .bytes()
                        .all(|c| c.is_ascii_digit() || c.is_ascii_uppercase())
                {
                    return Err(syntax(format!("invalid SQLSTATE code \"{st}\"")));
                }
                r.condition = Some(Condition::SqlState(st));
            }
            Some(Token {
                tok: Tok::Word(w), ..
            }) if w != "using" => {
                self.pos += 1;
                if !super::conditions::CONDITIONS.iter().any(|(n, _)| *n == w) {
                    return Err(Error::new(
                        code::UNDEFINED_OBJECT,
                        format!("unrecognized exception condition \"{w}\""),
                    ));
                }
                r.condition = Some(Condition::Name(w));
            }
            _ => {}
        }
        if self.eat_kw("using") {
            loop {
                let name = self.ident()?.to_lowercase();
                if !matches!(
                    name.as_str(),
                    "message"
                        | "detail"
                        | "hint"
                        | "errcode"
                        | "column"
                        | "constraint"
                        | "datatype"
                        | "table"
                        | "schema"
                ) {
                    return Err(syntax(format!(
                        "unrecognized RAISE statement option \"{name}\""
                    )));
                }
                match self.peek() {
                    Some(Tok::Assign) => self.pos += 1,
                    Some(Tok::Op(o)) if o == "=" => self.pos += 1,
                    _ => return Err(self.unexpected()),
                }
                let e = self.expr_until(&|t| matches!(t, Tok::Punct(';') | Tok::Punct(',')))?;
                r.options.push((name, e));
                if self.is_punct(',') {
                    self.pos += 1;
                } else {
                    break;
                }
            }
        }
        self.expect_punct(';')?;
        Ok(StmtKind::Raise(r))
    }

    /// A SQL statement, with an optional `INTO [STRICT] targets` taken out.
    fn sql_stmt(&mut self) -> Result<StmtKind> {
        let first = self.pos;
        let start = self
            .toks
            .get(first)
            .map(|t| t.start)
            .unwrap_or(self.src.len());
        let skip_first_into = matches!(self.peek(), Some(Tok::Word(w)) if w == "insert" || w == "merge" || w == "import");
        let mut seen_into = false;
        let mut depth = 0i32;
        let mut into: Option<(usize, usize, IntoClause)> = None;
        // Inside CREATE [OR REPLACE] FUNCTION / PROCEDURE, BEGIN (or CASE) ... END nest and
        // their semicolons don't end the statement (make_execsql_stmt).
        let word = |i: usize| match self.toks.get(i).map(|t| &t.tok) {
            Some(Tok::Word(w)) => w.as_str(),
            _ => "",
        };
        let routine = word(first) == "create"
            && (matches!(word(first + 1), "function" | "procedure")
                || word(first + 1) == "or"
                    && word(first + 2) == "replace"
                    && matches!(word(first + 3), "function" | "procedure"));
        let mut begins = 0;
        loop {
            let Some(t) = self.toks.get(self.pos).cloned() else {
                return Err(self.eof());
            };
            if routine && depth == 0 {
                match &t.tok {
                    Tok::Word(w) if w == "begin" || w == "case" => begins += 1,
                    Tok::Word(w) if w == "end" && begins > 0 => begins -= 1,
                    _ => {}
                }
            }
            match &t.tok {
                Tok::Punct(';') if depth == 0 && begins == 0 => break,
                Tok::Punct('(') | Tok::Punct('[') => depth += 1,
                Tok::Punct(')') | Tok::Punct(']') => depth -= 1,
                Tok::Word(w) if depth == 0 && w == "into" => {
                    if skip_first_into && !seen_into {
                        seen_into = true;
                    } else if into.is_none() {
                        self.pos += 1;
                        let strict = self.eat_kw("strict");
                        let targets = self.targets()?;
                        let after = self
                            .toks
                            .get(self.pos)
                            .map(|t| t.start)
                            .unwrap_or(self.src.len());
                        into = Some((t.start, after, IntoClause { targets, strict }));
                        continue;
                    } else {
                        return Err(syntax("INTO specified more than once"));
                    }
                }
                _ => {}
            }
            self.pos += 1;
        }
        let end = self.toks[self.pos - 1].end.max(start);
        self.pos += 1; // ;
        if self.pos - 1 == first {
            return Err(self.unexpected());
        }
        // INTO is blanked out of the query (keeping positions).
        let text = match &into {
            Some((a, b, _)) => {
                let b = (*b).min(end);
                format!(
                    "{}{}{}",
                    &self.src[start..*a],
                    " ".repeat(b - *a),
                    &self.src[b..end]
                )
            }
            None => self.src[start..end].to_string(),
        };
        // (make_execsql_stmt trims trailing whitespace, for neatness.)
        let shown = text.trim_end().to_string();
        self.next_id += 1;
        let start = into.is_none().then_some(start as isize);
        Ok(StmtKind::Sql {
            sql: PlExpr {
                id: self.next_id,
                text: text.trim().to_string(),
                start,
            },
            into: into.map(|(_, _, i)| i),
            shown,
        })
    }
}

pub(super) fn placeholders(fmt: &str) -> usize {
    let mut n = 0;
    let mut it = fmt.chars().peekable();
    while let Some(c) = it.next() {
        if c == '%' {
            if it.peek() == Some(&'%') {
                it.next();
            } else {
                n += 1;
            }
        }
    }
    n
}

fn type_spec(t: &str) -> TypeSpec {
    let compact: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    let lower = compact.to_lowercase();
    if lower == "record" {
        return TypeSpec::Record;
    }
    let split = |s: &str| {
        s.split('.')
            .map(|p| {
                if p.starts_with('"') {
                    p.trim_matches('"').to_string()
                } else {
                    p.to_lowercase()
                }
            })
            .collect::<Vec<_>>()
    };
    // `x%TYPE[]`, `t%ROWTYPE ARRAY`: an array of it.
    if let Some(at) = lower.rfind('%') {
        let rest = &lower[at..];
        let base = rest.trim_end_matches(|c: char| c == '[' || c == ']' || c.is_ascii_digit());
        let base = base.strip_suffix("array").unwrap_or(base);
        if base.len() < rest.len() && matches!(base, "%type" | "%rowtype") {
            let inner = type_spec(&compact[..at + base.len()]);
            return TypeSpec::ArrayOf(Box::new(inner));
        }
    }
    if let Some(base) = lower.strip_suffix("%rowtype") {
        let base = &compact[..base.len()];
        return TypeSpec::RowType(base.to_string());
    }
    if let Some(base) = lower.strip_suffix("%type") {
        return TypeSpec::TypeOf(split(&compact[..base.len()]));
    }
    TypeSpec::Named(t.to_string())
}

/// Every embedded expression and SQL statement, for syntax checks at CREATE time.
/// What the body is compiled for, for the checks Postgres makes while parsing.
#[derive(Clone, Copy)]
pub struct Shape {
    pub void: bool,
    pub set: bool,
    pub out: bool,
    pub procedure: bool,
}
