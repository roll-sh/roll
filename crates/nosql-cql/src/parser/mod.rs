//! The parser: a hand-written recursive-descent port of the parser ANTLR 3.5.2 generates from
//! Cassandra 5.0.9's `Parser.g` (Apache-2.0), extended with ScyllaDB's grammar.
//!
//! The port keeps the generated parser's control flow rather than just the grammar, because the
//! error messages depend on it:
//!
//! - Every decision is taken as the generated code takes it: inline `LA(k)` tests where ANTLR
//!   inlines them, and the extracted prediction tables ([`dfa`]) where ANTLR builds a table, with
//!   the same syntactic predicates. So the parser fails at the same token, with "no viable
//!   alternative" where the generated parser has no viable alternative and "mismatched input"
//!   where a `match` fails.
//! - Cassandra's messages end with a snippet built from the token stream as fetched so far (see
//!   [`snippet`]), so the parser tracks the furthest token any decision has looked at.
//! - The first error wins, as in Cassandra's `ErrorCollector`; a lexer error counts once the
//!   parser has fetched the tokens past it. ANTLR keeps going after an error, but only the first
//!   is reported, so this parser stops there.
//!
//! ScyllaDB's parser is also ANTLR 3 (C++ target), with its own grammar and error reporting:
//! no snippet, "no viable alternative" at the token where the decision started, and on a failed
//! `match` single-token recovery messages: `Unexpected 'x'` when the token after the bad one is
//! the expected one, `Missing X` when the bad token can follow the expected one, else
//! `Syntax error`. Rules here take a `follow` set where ScyllaDB's `Missing` matters.

pub(crate) mod dfa;
mod dfa_tables;
mod dcl;
mod ddl;
mod dml;
mod snippet;

use std::borrow::Cow;
use std::collections::HashMap;

use crate::Dialect;
use crate::ast::*;
use crate::error::SyntaxError;
use crate::lexer::{self, LexError, Token};
use crate::tokens::{self as t, Tt};
use dfa::Dfa;

/// Parsing stopped; the error is in [`Parser::error`] (or nothing was recorded because the
/// parser was evaluating a syntactic predicate).
#[derive(Debug)]
pub(crate) struct Fail;

pub(crate) type PResult<T> = Result<T, Fail>;

/// Pseudo token types for follow sets.
pub(crate) const F_IDENT: Tt = 1;
/// The expected token ends its rule: ScyllaDB's recovery then always reports it as missing.
pub(crate) const F_END: Tt = 2;
pub(crate) const F_TERM: Tt = 3;

/// The deepest nesting of terms, selectors, types and relations accepted.
const MAX_NESTING: u32 = 128;

pub(crate) struct Parser<'a> {
    pub(crate) src: &'a str,
    toks: Vec<Token>,
    pos: usize,
    /// The furthest visible token fetched by any lookahead.
    hw: usize,
    pub(crate) d: Dialect,
    /// Depth of syntactic-predicate evaluation; errors are not recorded while it is non-zero.
    spec: u32,
    binds: Vec<Option<Ident<'a>>>,
    lex_error: Option<LexError>,
    pub(crate) error: Option<SyntaxError>,
    /// The recorded error is a recognition error (a failed match or prediction), after which
    /// Cassandra's parser carries on and may hit a `NullPointerException`.
    recognition: bool,
    /// The next `function_name` is on the left of a ScyllaDB relation, where any identifier
    /// names a function.
    pub(crate) relation_function: bool,
    /// Current nesting of recursive rules.
    depth: u32,
    /// Syntactic predicate results by (predicate, token index).
    memo: HashMap<(u16, usize), (bool, usize)>,
}

/// Parses exactly one statement, optionally followed by semicolons, as a server parses the text
/// of a `QUERY` or `PREPARE` message.
pub fn parse_statement(src: &str, dialect: Dialect) -> Result<Statement<'_>, SyntaxError> {
    let lexed = lexer::lex(src, dialect, false);
    let mut p = Parser {
        src,
        toks: lexed.tokens,
        pos: 0,
        hw: 0,
        d: dialect,
        spec: 0,
        binds: Vec::new(),
        lex_error: lexed.error,
        error: None,
        recognition: false,
        relation_function: false,
        depth: 0,
        memo: HashMap::new(),
    };
    match p.query() {
        Ok(_) if p.error.is_some() => Err(p.error.take().unwrap_or_else(|| SyntaxError::custom("syntax error"))),
        Ok(kind) => {
            if let Some(e) = p.lex_error {
                return Err(p.lex_error_to_syntax(e));
            }
            let end = p.statement_end();
            let start = p.toks.first().filter(|t| t.ty != t::EOF).map_or(0, |t| t.start);
            Ok(Statement { kind, bind_markers: std::mem::take(&mut p.binds), span: Span { start, end } })
        }
        Err(Fail) => Err(p.error.take().unwrap_or_else(|| SyntaxError::custom("syntax error"))),
    }
}

impl<'a> Parser<'a> {
    // -----------------------------------------------------------------------------------------
    // Token stream
    // -----------------------------------------------------------------------------------------

    fn idx(&self, k: usize) -> usize {
        (self.pos + k).min(self.toks.len() - 1)
    }

    /// The type of the `k`-th token ahead (1-based), fetching it.
    pub(crate) fn la(&mut self, k: usize) -> Tt {
        let i = self.idx(k.saturating_sub(1));
        self.hw = self.hw.max(i);
        self.toks[i].ty
    }

    pub(crate) fn lt(&mut self, k: usize) -> Token {
        let i = self.idx(k.saturating_sub(1));
        self.hw = self.hw.max(i);
        self.toks[i]
    }

    /// Consumes the current token (fetching the next, as ANTLR's stream does).
    pub(crate) fn consume(&mut self) -> Token {
        let tok = self.toks[self.pos];
        if self.pos + 1 < self.toks.len() {
            self.pos += 1;
        }
        self.hw = self.hw.max(self.pos);
        tok
    }

    pub(crate) fn at(&mut self, ty: Tt) -> bool {
        self.la(1) == ty
    }

    /// Consumes the current token if it is `ty`.
    pub(crate) fn eat(&mut self, ty: Tt) -> bool {
        if self.la(1) == ty {
            self.consume();
            true
        } else {
            false
        }
    }

    pub(crate) fn expect(&mut self, ty: Tt) -> PResult<Token> {
        self.expect_f(ty, &[])
    }

    /// Matches `ty`; `follow` is what can come next (for ScyllaDB's `Missing` messages).
    pub(crate) fn expect_f(&mut self, ty: Tt, follow: &[Tt]) -> PResult<Token> {
        if self.la(1) == ty {
            return Ok(self.consume());
        }
        self.mismatch(ty, follow)
    }

    pub(crate) fn text(&self, tok: &Token) -> Cow<'a, str> {
        lexer::token_text(self.src, tok)
    }

    fn statement_end(&self) -> u32 {
        // The span ends at the last non-`;` visible token.
        let mut i = self.toks.len() - 1;
        while i > 0 && matches!(self.toks[i].ty, t::EOF | t::SEMI) {
            i -= 1;
        }
        self.toks.get(i).map_or(0, |t| t.end)
    }

    pub(crate) fn scylla(&self) -> bool {
        self.d == Dialect::Scylla
    }

    pub(crate) fn cassandra(&self) -> bool {
        self.d == Dialect::Cassandra
    }

    // -----------------------------------------------------------------------------------------
    // Errors
    // -----------------------------------------------------------------------------------------

    fn lex_error_to_syntax(&self, e: LexError) -> SyntaxError {
        let col = match self.d {
            Dialect::Cassandra => e.col,
            Dialect::Scylla => e.bcol,
        };
        SyntaxError { code: crate::ErrorCode::Syntax, message: e.message(self.d), position: Some((e.line as i32, col as i32)) }
    }

    /// Records `err` unless an earlier error exists or a lexer error was reached first.
    fn record(&mut self, err: SyntaxError) -> Fail {
        if self.spec > 0 || self.error.is_some() {
            return Fail;
        }
        // Cassandra's token stream is filled as the parser asks for tokens, so a lexer error
        // counts once the parser has looked past it; ScyllaDB's is filled before parsing starts.
        if let Some(le) = self.lex_error
            && (self.scylla() || le.raw <= self.toks[self.hw].raw)
        {
            self.error = Some(self.lex_error_to_syntax(le));
            return Fail;
        }
        self.error = Some(err);
        Fail
    }

    /// Records a recognition error (see [`Parser::recognition`]).
    fn record_recognition(&mut self, err: SyntaxError) -> Fail {
        let had = self.error.is_some();
        let f = self.record(err);
        if !had && self.spec == 0 && self.error.as_ref().is_some_and(|e| e.position.is_some()) {
            let lexer = self.lex_error.is_some_and(|le| le.raw <= self.toks[self.hw].raw);
            self.recognition = !lexer;
        }
        f
    }

    /// Cassandra's parser keeps going after a recognition error with `null` results; where an
    /// action then dereferences one, the `NullPointerException` replaces the syntax error.
    pub(crate) fn null_pointer(&mut self, msg: &str) -> Fail {
        if self.spec == 0 && self.recognition && self.cassandra() {
            self.error = Some(SyntaxError::custom(format!(
                "Failed parsing statement: [{}] reason: NullPointerException {msg}",
                self.src
            )));
            self.recognition = false;
        }
        Fail
    }

    /// A custom message (`addRecognitionError`). Ignored while speculating, like the actions
    /// that raise them.
    pub(crate) fn custom_error(&mut self, msg: impl Into<String>) -> PResult<()> {
        if self.spec > 0 {
            return Ok(());
        }
        Err(self.record(SyntaxError::custom(msg)))
    }

    /// An exception a Cassandra parser action throws (rather than collects): it aborts the parse
    /// and replaces any error collected so far. Actions do not run in syntactic predicates.
    pub(crate) fn throw(&mut self, err: SyntaxError) -> Fail {
        if self.spec == 0 {
            self.error = Some(err);
            self.recognition = false;
        }
        Fail
    }

    /// A Java runtime exception escaping the parser, which Cassandra reports with the statement.
    pub(crate) fn throw_runtime(&mut self, class: &str, msg: &str) -> Fail {
        let m = format!("Failed parsing statement: [{}] reason: {class} {msg}", self.src);
        self.throw(SyntaxError::custom(m))
    }

    /// The display of a token in a message: `'text'` with control characters escaped.
    fn display(&self, tok: &Token) -> String {
        let text = self.text(tok);
        let text = text.replace('\n', "\\n").replace('\r', "\\r").replace('\t', "\\t");
        format!("'{text}'")
    }

    fn header(&self, tok: &Token) -> (String, (i32, i32)) {
        let (line, col) = match self.d {
            Dialect::Cassandra if tok.ty == t::EOF => (0, -1),
            Dialect::Cassandra => (tok.line as i32, tok.col as i32),
            Dialect::Scylla if tok.ty == t::EOF => (tok.line as i32, 0),
            Dialect::Scylla => (tok.line as i32, tok.bcol as i32),
        };
        (format!("line {line}:{col}"), (line, col))
    }

    fn with_snippet(&self, mut msg: String, index_tok: usize) -> String {
        if self.d == Dialect::Cassandra {
            let index = self.toks[index_tok].raw as usize;
            let size = self.toks[self.hw].raw as usize + 1;
            if let Some(s) = snippet::snippet(self.src, self.d, index, size) {
                msg.push_str(&s);
            }
        }
        msg
    }

    /// A failed `match` of `expected` at the current token.
    pub(crate) fn mismatch<T>(&mut self, expected: Tt, follow: &[Tt]) -> PResult<T> {
        let tok = self.lt(1);
        let (hdr, pos) = self.header(&tok);
        let msg = match self.d {
            Dialect::Cassandra => {
                let m = format!("{hdr} mismatched input {} expecting {}", self.display(&tok), t::token_name(expected));
                self.with_snippet(m, self.pos)
            }
            Dialect::Scylla => {
                if self.la(2) == expected {
                    format!("{hdr} : Unexpected {}", self.display(&tok))
                } else if self.in_follow(tok.ty, follow) {
                    format!("{hdr} : Missing {}", t::token_name(expected))
                } else {
                    format!("{hdr} : Syntax error")
                }
            }
        };
        Err(self.record_recognition(SyntaxError { code: crate::ErrorCode::Syntax, message: msg, position: Some(pos) }))
    }

    fn in_follow(&self, ty: Tt, follow: &[Tt]) -> bool {
        follow.iter().any(|&f| match f {
            F_IDENT => t::is_ident(ty, self.d),
            F_END => true,
            F_TERM => is_term_start(ty, self.d) || ty == t::MINUS,
            f => f == ty,
        })
    }

    /// No viable alternative for the decision that started at the current token, failing at
    /// lookahead offset `deep` (0 = the current token).
    pub(crate) fn no_viable<T>(&mut self, deep: usize) -> PResult<T> {
        let start = self.pos;
        let tok = match self.d {
            Dialect::Cassandra => self.lt(deep + 1),
            Dialect::Scylla => self.lt(1),
        };
        let (hdr, pos) = self.header(&tok);
        let m = format!("{hdr} no viable alternative at input {}", self.display(&tok));
        let msg = self.with_snippet(m, start);
        Err(self.record_recognition(SyntaxError { code: crate::ErrorCode::Syntax, message: msg, position: Some(pos) }))
    }

    /// No viable alternative reported at lookahead offset `deep` in both dialects (ScyllaDB's
    /// table-driven decisions report the token where they failed).
    pub(crate) fn no_viable_deep<T>(&mut self, deep: usize) -> PResult<T> {
        let start = self.pos;
        let tok = self.lt(deep + 1);
        let (hdr, pos) = self.header(&tok);
        let m = format!("{hdr} no viable alternative at input {}", self.display(&tok));
        let msg = self.with_snippet(m, start);
        Err(self.record_recognition(SyntaxError { code: crate::ErrorCode::Syntax, message: msg, position: Some(pos) }))
    }

    /// Runs `f` one nesting level deeper. Nesting is bounded so that deeply nested input cannot
    /// exhaust the stack; past the bound the statement fails as Java's parser would, with a stack
    /// overflow.
    pub(crate) fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> PResult<T> {
        if self.depth >= MAX_NESTING {
            if self.spec > 0 {
                return Err(Fail);
            }
            return Err(self.throw_runtime("StackOverflowError", "null"));
        }
        self.depth += 1;
        let r = f(self);
        self.depth -= 1;
        r
    }

    /// Runs `f` as a syntactic predicate: whether it parses, without consuming or recording.
    pub(crate) fn speculate<T>(&mut self, f: impl FnOnce(&mut Self) -> PResult<T>) -> bool {
        let saved = self.pos;
        let binds = self.binds.len();
        self.spec += 1;
        let ok = f(self).is_ok();
        self.spec -= 1;
        self.pos = saved;
        self.binds.truncate(binds);
        ok
    }

    /// `MismatchedSetException` at the current token.
    pub(crate) fn mismatched_set<T>(&mut self) -> PResult<T> {
        let tok = self.lt(1);
        let (hdr, pos) = self.header(&tok);
        let msg = match self.d {
            Dialect::Cassandra => {
                let m = format!("{hdr} mismatched input {} expecting set null", self.display(&tok));
                self.with_snippet(m, self.pos)
            }
            Dialect::Scylla => format!("{hdr} : Syntax error"),
        };
        Err(self.record_recognition(SyntaxError { code: crate::ErrorCode::Syntax, message: msg, position: Some(pos) }))
    }

    // -----------------------------------------------------------------------------------------
    // Prediction
    // -----------------------------------------------------------------------------------------

    /// The symbol a prediction table reads for a token.
    fn dfa_symbol(&self, ty: Tt) -> u32 {
        if ty == t::EOF {
            return dfa::EOF_SYMBOL;
        }
        if self.d == Dialect::Scylla
            && (ty >= t::K_BYPASS || matches!(ty, t::K_DESC | t::K_DESCRIBE | t::K_EXECUTE))
            && t::is_unreserved(ty, Dialect::Scylla)
        {
            return t::IDENT as u32;
        }
        ty as u32
    }

    /// Runs a prediction table at the current position: the predicted alternative (1-based), or
    /// a "no viable alternative" error.
    pub(crate) fn predict(&mut self, table: &'static Dfa) -> PResult<u32> {
        let start = self.pos;
        self.predict_walk(table, start)
    }

    fn predict_walk(&mut self, table: &'static Dfa, start: usize) -> PResult<u32> {
        let t = table.tables();
        let mut s: usize = 0;
        let mut i: usize = 0;
        let sym = |p: &mut Self, i: usize| -> u32 {
            let j = (start + i).min(p.toks.len() - 1);
            p.hw = p.hw.max(j);
            p.dfa_symbol(p.toks[j].ty)
        };
        for _ in 0..100_000 {
            let sp = t.special.get(s).copied().unwrap_or(-1);
            if sp >= 0 {
                sym(self, i);
                let Some((pred, yes, no)) = table.special(sp) else { return self.no_viable(i) };
                s = if self.synpred(pred, start) { yes as usize } else { no as usize };
                i += 1;
                sym(self, i);
                continue;
            }
            let acc = t.accept.get(s).copied().unwrap_or(-1);
            if acc >= 1 {
                return Ok(acc as u32);
            }
            let c = sym(self, i);
            let (lo, hi) = (t.min.get(s).copied().unwrap_or(1), t.max.get(s).copied().unwrap_or(0));
            let eot = t.eot.get(s).copied().unwrap_or(-1);
            if c >= lo && c <= hi {
                let next = t.transition.get(s).and_then(|row| row.get((c - lo) as usize)).copied().unwrap_or(-1);
                if next >= 0 {
                    s = next as usize;
                } else if eot >= 0 {
                    s = eot as usize;
                } else {
                    return self.no_viable(i);
                }
                i += 1;
                sym(self, i);
                continue;
            }
            if eot >= 0 {
                s = eot as usize;
                i += 1;
                sym(self, i);
                continue;
            }
            let eof = t.eof.get(s).copied().unwrap_or(-1);
            if c == dfa::EOF_SYMBOL && eof >= 0 {
                return Ok(t.accept.get(eof as usize).copied().unwrap_or(0).max(0) as u32);
            }
            return self.no_viable(i);
        }
        self.no_viable(i)
    }

    /// Evaluates syntactic predicate `n` of `Parser.g` from token `start`, restoring the
    /// position afterwards. Lookahead it fetches stays fetched, as in ANTLR.
    fn synpred(&mut self, n: u16, start: usize) -> bool {
        // Predicates nest (a selector's predicate parses inner selectors, whose decisions run
        // predicates again), so results are memoised per position, with the lookahead they
        // fetched, to keep parsing linear.
        if let Some(&(ok, hw)) = self.memo.get(&(n, start)) {
            self.hw = self.hw.max(hw);
            return ok;
        }
        let ok = self.synpred_eval(n, start);
        self.memo.insert((n, start), (ok, self.hw));
        ok
    }

    fn synpred_eval(&mut self, n: u16, start: usize) -> bool {
        let saved = self.pos;
        let binds = self.binds.len();
        self.pos = start;
        self.spec += 1;
        let ok = match n {
            // (K_JSON selectClause)=>
            1 => self.expect(t::K_JSON).is_ok() && self.select_clause().is_ok(),
            // (K_DISTINCT selectors)=>
            2 => self.expect(t::K_DISTINCT).is_ok() && self.selectors().is_ok(),
            // (selectionGroupWithField)=>
            3 => self.selection_group_with_field().is_ok(),
            // (selectionTypeHint)=>
            4 => self.selection_type_hint().is_ok(),
            5 => self.expect(t::K_CLUSTER).is_ok(),
            6 => self.expect(t::K_KEYSPACES).is_ok(),
            7 => self.expect(t::K_TABLES).is_ok(),
            8 => self.expect(t::K_TYPES).is_ok(),
            9 => self.expect(t::K_FUNCTIONS).is_ok(),
            10 => self.expect(t::K_AGGREGATES).is_ok(),
            _ => false,
        };
        self.spec -= 1;
        self.pos = saved;
        self.binds.truncate(binds);
        ok
    }

    // -----------------------------------------------------------------------------------------
    // The statement level
    // -----------------------------------------------------------------------------------------

    /// `query: cqlStatement (';')* EOF`.
    fn query(&mut self) -> PResult<StatementKind<'a>> {
        self.la(1);
        let stmt = self.cql_statement()?;
        while self.eat(t::SEMI) {}
        self.expect_f(t::EOF, &[])?;
        Ok(stmt)
    }

    // -----------------------------------------------------------------------------------------
    // Names
    // -----------------------------------------------------------------------------------------

    pub(crate) fn ident_from(&self, tok: &Token) -> Ident<'a> {
        match tok.ty {
            t::QUOTED_NAME => Ident { text: self.text(tok), quoted: true },
            t::EMPTY_QUOTED_NAME => Ident { text: Cow::Borrowed(""), quoted: true },
            _ => Ident { text: Cow::Borrowed(&self.src[tok.start as usize..tok.end as usize]), quoted: false },
        }
    }

    /// `ident`, `fident`, `noncol_ident`: `IDENT | QUOTED_NAME | unreserved_keyword`.
    pub(crate) fn ident(&mut self) -> PResult<Ident<'a>> {
        let ty = self.la(1);
        if t::is_ident(ty, self.d) {
            let tok = self.consume();
            return Ok(self.ident_from(&tok));
        }
        self.no_viable(0)
    }

    /// `cident`: an identifier or the empty quoted name `""`.
    pub(crate) fn cident(&mut self) -> PResult<Ident<'a>> {
        if self.at(t::EMPTY_QUOTED_NAME) {
            let tok = self.consume();
            return Ok(self.ident_from(&tok));
        }
        let ty = self.la(1);
        if t::is_ident(ty, self.d) {
            let tok = self.consume();
            return Ok(self.ident_from(&tok));
        }
        self.no_viable(0)
    }

    /// `ksName`, `cfName`, `idxName`: an identifier, or `?` with a custom error.
    fn object_name(&mut self, what: &str) -> PResult<Ident<'a>> {
        if self.at(t::QMARK) {
            self.consume();
            self.custom_error(format!("Bind variables cannot be used for {what} names"))?;
            return Ok(Ident::unquoted("?"));
        }
        self.ident()
    }

    pub(crate) fn keyspace_name(&mut self) -> PResult<Ident<'a>> {
        self.object_name("keyspace")
    }

    /// `columnFamilyName: (ksName '.')? cfName`.
    pub(crate) fn column_family_name(&mut self) -> PResult<QualifiedName<'a>> {
        let has_ks = if self.cassandra() { self.predict(&dfa_tables::DFA209)? == 1 } else { self.scylla_qualified()? };
        let keyspace = if has_ks {
            let ks = self.object_name("keyspace")?;
            self.expect_f(t::DOT, &[F_IDENT, t::QMARK])?;
            Some(ks)
        } else {
            None
        };
        let name = self.object_name("table")?;
        Ok(QualifiedName { keyspace, name })
    }

    /// `indexName: (ksName '.')? idxName`.
    pub(crate) fn index_name(&mut self) -> PResult<QualifiedName<'a>> {
        let has_ks = if self.cassandra() { self.predict(&dfa_tables::DFA208)? == 1 } else { self.scylla_qualified()? };
        let keyspace = if has_ks {
            let ks = self.object_name("keyspace")?;
            self.expect_f(t::DOT, &[F_IDENT, t::QMARK])?;
            Some(ks)
        } else {
            None
        };
        let name = self.object_name("index")?;
        Ok(QualifiedName { keyspace, name })
    }

    /// ScyllaDB's `(ksName '.')?` decision: a name followed by `.`.
    fn scylla_qualified(&mut self) -> PResult<bool> {
        let ty = self.la(1);
        if !(t::is_ident(ty, self.d) || ty == t::QMARK) {
            return self.no_viable(0);
        }
        Ok(self.la(2) == t::DOT)
    }

    /// `userTypeName: (noncol_ident '.')? non_type_ident`.
    pub(crate) fn user_type_name(&mut self) -> PResult<QualifiedName<'a>> {
        self.user_type_name_staged(&mut false)
    }

    /// `userTypeName`; `in_name` is set when it fails in `non_type_ident` (rather than in the
    /// keyspace part), which decides what Cassandra's `DESCRIBE TYPE` dereferences.
    pub(crate) fn user_type_name_staged(&mut self, in_name: &mut bool) -> PResult<QualifiedName<'a>> {
        let ty = self.la(1);
        let has_ks = if t::is_ident(ty, self.d) && !self.is_non_type_ident(ty) {
            true
        } else if t::is_ident(ty, self.d) {
            self.la(2) == t::DOT
        } else {
            false
        };
        let keyspace = if has_ks {
            let ks = self.ident()?;
            self.expect_f(t::DOT, &[F_IDENT])?;
            Some(ks)
        } else {
            None
        };
        *in_name = true;
        let name = self.non_type_ident()?;
        Ok(QualifiedName { keyspace, name })
    }

    fn is_non_type_ident(&self, ty: Tt) -> bool {
        ty == t::IDENT || ty == t::QUOTED_NAME || ty == t::K_KEY || t::is_basic_unreserved(ty, self.d)
    }

    /// `non_type_ident`: an identifier that is not a type name; some names are reserved.
    fn non_type_ident(&mut self) -> PResult<Ident<'a>> {
        let ty = self.la(1);
        if !self.is_non_type_ident(ty) {
            return self.no_viable(0);
        }
        let tok = self.consume();
        let id = self.ident_from(&tok);
        if ty == t::IDENT {
            let reserved: &[&str] = match self.d {
                Dialect::Cassandra => &["byte", "complex", "enum", "date", "interval", "macaddr", "bitstring"],
                Dialect::Scylla => &["byte", "complex", "enum", "date", "interval", "macaddr", "bitstring"],
            };
            if reserved.contains(&id.text.as_ref()) {
                self.custom_error(format!("Invalid (reserved) user type name {}", id.text))?;
            }
        }
        Ok(id)
    }

    /// `roleName` / `userOrRoleName`.
    pub(crate) fn role_name(&mut self) -> PResult<RoleName<'a>> {
        self.role_or_service_level_name(false)
    }

    /// A role name, or a ScyllaDB service level name (which has its own bind-marker message).
    pub(crate) fn role_or_service_level_name(&mut self, service_level: bool) -> PResult<RoleName<'a>> {
        let ty = self.la(1);
        let form = match ty {
            t::IDENT => NameForm::Ident,
            t::STRING_LITERAL => NameForm::String,
            t::QUOTED_NAME => NameForm::Quoted,
            t::QMARK => {
                self.consume();
                let msg = if service_level {
                    "Bind variables cannot be used for service levels or role names"
                } else {
                    "Bind variables cannot be used for role names"
                };
                self.custom_error(msg)?;
                return Ok(RoleName { text: Cow::Borrowed("?"), form: NameForm::Ident });
            }
            _ if t::is_unreserved(ty, self.d) => NameForm::Ident,
            // ScyllaDB's built-in service level is called `default`.
            t::K_DEFAULT if service_level && self.scylla() => NameForm::Ident,
            _ => return self.no_viable(0),
        };
        let tok = self.consume();
        let text = match form {
            NameForm::Ident => Cow::Borrowed(&self.src[tok.start as usize..tok.end as usize]),
            _ => self.text(&tok),
        };
        Ok(RoleName { text, form })
    }

    // -----------------------------------------------------------------------------------------
    // Constants and terms
    // -----------------------------------------------------------------------------------------

    pub(crate) fn literal_from(&self, tok: &Token) -> Literal<'a> {
        let raw = &self.src[tok.start as usize..tok.end as usize];
        let (kind, text) = match tok.ty {
            t::STRING_LITERAL => (LiteralKind::String, self.text(tok)),
            t::INTEGER => (LiteralKind::Integer, Cow::Borrowed(raw)),
            t::FLOAT => (LiteralKind::Float, Cow::Borrowed(raw)),
            t::BOOLEAN => (LiteralKind::Boolean, Cow::Borrowed(raw)),
            t::DURATION => (LiteralKind::Duration, Cow::Borrowed(raw)),
            t::UUID => (LiteralKind::Uuid, Cow::Borrowed(raw)),
            t::HEXNUMBER => (LiteralKind::Hex, Cow::Borrowed(raw)),
            t::K_POSITIVE_NAN | t::K_NEGATIVE_NAN => (LiteralKind::Float, Cow::Borrowed("NaN")),
            t::K_POSITIVE_INFINITY => (LiteralKind::Float, Cow::Borrowed("Infinity")),
            t::K_NEGATIVE_INFINITY => (LiteralKind::Float, Cow::Borrowed("-Infinity")),
            _ => (LiteralKind::String, Cow::Borrowed(raw)),
        };
        Literal { kind, text, dollar: tok.ty == t::STRING_LITERAL && raw.starts_with('$') }
    }

    /// `constant`.
    pub(crate) fn constant(&mut self) -> PResult<Literal<'a>> {
        match self.la(1) {
            t::STRING_LITERAL
            | t::INTEGER
            | t::FLOAT
            | t::BOOLEAN
            | t::DURATION
            | t::UUID
            | t::HEXNUMBER
            | t::K_POSITIVE_NAN
            | t::K_NEGATIVE_NAN
            | t::K_POSITIVE_INFINITY
            | t::K_NEGATIVE_INFINITY => {
                let tok = self.consume();
                let lit = self.literal_from(&tok);
                // Cassandra types a float constant while parsing (`new BigDecimal(text)` compared
                // with its double value), which fails for values a double cannot hold.
                if tok.ty == t::FLOAT
                    && self.cassandra()
                    && self.spec == 0
                    && lit.text.parse::<f64>().is_ok_and(f64::is_infinite)
                {
                    return Err(self.throw_runtime("NumberFormatException", "Infinite or NaN"));
                }
                Ok(lit)
            }
            _ => self.no_viable(0),
        }
    }

    pub(crate) fn new_bind(&mut self, name: Option<Ident<'a>>) -> BindMarker<'a> {
        let index = self.binds.len() as u32;
        if self.spec == 0 {
            self.binds.push(name.clone());
        }
        BindMarker { index, name }
    }

    /// `':' noncol_ident` or `QMARK`.
    pub(crate) fn bind_marker(&mut self) -> PResult<BindMarker<'a>> {
        if self.eat(t::QMARK) {
            return Ok(self.new_bind(None));
        }
        if !self.at(t::COLON) {
            return self.no_viable(0);
        }
        self.consume();
        let name = self.ident()?;
        Ok(self.new_bind(Some(name)))
    }

    /// `intValue: INTEGER | ':' noncol_ident | QMARK`.
    pub(crate) fn int_value(&mut self) -> PResult<Term<'a>> {
        match self.la(1) {
            t::INTEGER => {
                let tok = self.consume();
                Ok(Term::Literal(self.literal_from(&tok)))
            }
            t::COLON | t::QMARK => Ok(Term::Bind(self.bind_marker()?)),
            _ => self.no_viable(0),
        }
    }

    /// `term`.
    pub(crate) fn term(&mut self) -> PResult<Term<'a>> {
        self.nested(|p| p.term_unguarded())
    }

    fn term_unguarded(&mut self) -> PResult<Term<'a>> {
        if self.scylla() {
            if self.at(t::MINUS) {
                return self.scylla_minus();
            }
            return self.simple_term();
        }
        let mut left = self.term_multiplication()?;
        loop {
            let op = match self.predict(&dfa_tables::DFA233)? {
                1 => ArithOp::Add,
                2 => ArithOp::Sub,
                _ => return Ok(left),
            };
            self.consume();
            let right = self.term_multiplication()?;
            left = Term::Binary(Box::new(BinaryOp { op, left, right }));
        }
    }

    fn term_multiplication(&mut self) -> PResult<Term<'a>> {
        let mut left = self.term_group()?;
        loop {
            let op = match self.la(1) {
                t::STAR => ArithOp::Mul,
                t::SLASH => ArithOp::Div,
                t::PERCENT => ArithOp::Mod,
                _ => return Ok(left),
            };
            self.consume();
            let right = self.term_group()?;
            left = Term::Binary(Box::new(BinaryOp { op, left, right }));
        }
    }

    fn term_group(&mut self) -> PResult<Term<'a>> {
        let ty = self.la(1);
        if is_term_start(ty, self.d) {
            return self.simple_term();
        }
        if ty == t::MINUS {
            self.consume();
            let inner = self.simple_term()?;
            return Ok(Term::Negate(Box::new(inner)));
        }
        self.no_viable(0)
    }

    /// `simpleTerm: value | function | '(' comparatorType ')' simpleTerm | CAST(...)`.
    pub(crate) fn simple_term(&mut self) -> PResult<Term<'a>> {
        self.nested(|p| p.simple_term_unguarded())
    }

    fn simple_term_unguarded(&mut self) -> PResult<Term<'a>> {
        let alt = if self.cassandra() { self.predict(&dfa_tables::DFA236)? } else { self.scylla_simple_term_alt()? };
        match alt {
            1 => self.value(),
            2 => Ok(Term::Function(self.function()?)),
            3 => {
                self.expect(t::LPAREN)?;
                let ty = self.comparator_type()?;
                self.expect_f(t::RPAREN, &[F_TERM])?;
                let term = self.simple_term()?;
                Ok(Term::TypeHint(Box::new(TypeHint { ty, term })))
            }
            _ if self.scylla() => self.no_viable(0),
            _ => {
                self.expect(t::K_CAST)?;
                self.expect(t::LPAREN)?;
                let arg = self.simple_term()?;
                self.expect(t::K_AS)?;
                let at = self.pos;
                let ty = match self.native_type() {
                    Ok(ty) => ty,
                    Err(f) if self.pos != at || self.spec > 0 => return Err(f),
                    Err(_) => {
                        // `FunctionCall.Raw.newCast(t, null)` dereferences the missing type.
                        self.expect(t::RPAREN)?;
                        return Err(self.null_pointer("Cannot invoke \"Object.toString()\" because \"type\" is null"));
                    }
                };
                self.expect(t::RPAREN)?;
                Ok(Term::Cast(Box::new(Cast::new(arg, ty))))
            }
        }
    }

    /// ScyllaDB's `- NaN` / `- Infinity`; a `-` before anything else is reported as an
    /// unexpected next token.
    pub(crate) fn scylla_minus(&mut self) -> PResult<Term<'a>> {
        self.consume();
        let tok = self.lt(1);
        match tok.ty {
            t::K_POSITIVE_NAN => {
                self.consume();
                Ok(Term::Literal(Literal { kind: LiteralKind::Float, text: Cow::Borrowed("NaN"), dollar: false }))
            }
            t::K_POSITIVE_INFINITY => {
                self.consume();
                Ok(Term::Literal(Literal { kind: LiteralKind::Float, text: Cow::Borrowed("-Infinity"), dollar: false }))
            }
            _ => Err(self.scylla_unexpected(&tok)),
        }
    }

    /// ScyllaDB's `simpleTerm` decision: value, function call or type hint.
    fn scylla_simple_term_alt(&mut self) -> PResult<u32> {
        let ty = self.la(1);
        if ty == t::LPAREN {
            let second = self.la(2);
            let third = self.la(3);
            let hint = (t::is_ident(second, self.d) && matches!(third, t::RPAREN | t::LT))
                || (t::is_ident(second, self.d) && third == t::DOT && self.la(5) == t::RPAREN);
            if hint {
                return Ok(3);
            }
            // A tuple: its first element must start a term (an identifier only as a function).
            let ident_term = (t::is_ident(second, self.d) || matches!(second, t::K_TOKEN | t::K_COUNT))
                && matches!(third, t::LPAREN | t::DOT);
            let ok = ident_term
                || (is_term_start(second, self.d) && !t::is_ident(second, self.d) && second != t::K_TOKEN && second != t::K_COUNT)
                || second == t::MINUS;
            return if ok { Ok(1) } else { self.no_viable(0) };
        }
        if is_constant(ty) || matches!(ty, t::LBRACKET | t::LBRACE | t::K_NULL | t::COLON | t::QMARK) {
            return Ok(1);
        }
        // Names that can only be a keyspace in a function name are taken as one.
        if matches!(ty, t::K_KEY | t::K_TTL | t::K_WRITETIME) {
            return Ok(2);
        }
        if t::is_ident(ty, self.d) || ty == t::K_TOKEN || ty == t::K_COUNT {
            let next = self.la(2);
            if next == t::LPAREN {
                return Ok(2);
            }
            if matches!(ty, t::K_TOKEN | t::K_COUNT) {
                return self.no_viable_deep(1);
            }
            if next == t::DOT {
                if !t::is_ident(self.la(3), self.d) {
                    return self.no_viable_deep(2);
                }
                if self.la(4) != t::LPAREN {
                    return self.no_viable_deep(3);
                }
                return Ok(2);
            }
            return self.no_viable_deep(1);
        }
        self.no_viable(0)
    }

    /// ScyllaDB's `value` decision. A bind marker is decided with the token after it.
    fn scylla_value_alt(&mut self) -> PResult<u32> {
        const FOLLOW: &[Tt] = &[
            t::COMMA,
            t::RPAREN,
            t::RBRACKET,
            t::RBRACE,
            t::COLON,
            t::K_AND,
            t::K_WHERE,
            t::K_IF,
            t::K_USING,
            t::EOF,
            t::SEMI,
            t::K_LIMIT,
            t::K_ALLOW,
            t::K_ORDER,
            t::K_GROUP,
            t::K_PER,
            t::K_BYPASS,
            t::K_APPLY,
            t::K_INSERT,
            t::K_UPDATE,
            t::K_DELETE,
            t::K_ASC,
            t::K_DESC,
            t::K_SET,
            t::PLUS,
            t::RANGE,
            t::K_AS,
            // `CREATE MATERIALIZED VIEW ... WHERE p = ? PRIMARY KEY (p)`.
            t::K_PRIMARY,
        ];
        let ty = self.la(1);
        if ty == t::QMARK && !FOLLOW.contains(&self.la(2)) {
            return self.no_viable(0);
        }
        if ty == t::COLON && !t::is_ident(self.la(2), self.d) {
            return self.no_viable_deep(1);
        }
        Ok(match self.la(1) {
            t::LBRACKET => 2,
            t::LBRACE => {
                let second = self.la(2);
                if t::is_ident(second, self.d) {
                    match self.la(3) {
                        t::COLON => 3,
                        t::LPAREN | t::DOT => 2,
                        _ => return self.no_viable(0),
                    }
                } else {
                    2
                }
            }
            t::LPAREN => 4,
            t::K_NULL => 5,
            t::COLON | t::QMARK => 6,
            ty if is_constant(ty) => 1,
            _ => return self.no_viable(0),
        })
    }

    /// `value`.
    pub(crate) fn value(&mut self) -> PResult<Term<'a>> {
        let alt = if self.cassandra() { self.predict(&dfa_tables::DFA227)? } else { self.scylla_value_alt()? };
        match alt {
            1 => Ok(Term::Literal(self.constant()?)),
            2 => self.collection_literal(),
            3 => self.usertype_literal(),
            4 => self.tuple_literal(),
            5 => {
                self.expect_f(t::K_NULL, &[F_END])?;
                Ok(Term::Null)
            }
            _ => Ok(Term::Bind(self.bind_marker()?)),
        }
    }

    /// `collectionLiteral: listLiteral | '{' term setOrMapLiteral '}' | '{' '}'`.
    fn collection_literal(&mut self) -> PResult<Term<'a>> {
        let ty = self.la(1);
        if ty == t::LBRACKET {
            return self.list_literal();
        }
        if ty != t::LBRACE {
            return self.no_viable(0);
        }
        let next = self.la(2);
        if next == t::RBRACE {
            self.consume();
            self.consume();
            return Ok(Term::Set(Vec::new()));
        }
        if !is_term_start(next, self.d) && next != t::MINUS {
            return self.no_viable(1);
        }
        self.consume();
        let first = self.term()?;
        let out = match self.la(1) {
            t::COLON => {
                self.consume();
                let v = self.term()?;
                let mut entries = vec![(first, v)];
                while self.eat(t::COMMA) {
                    let k = self.term()?;
                    self.expect_f(t::COLON, &[F_TERM])?;
                    let v = self.term()?;
                    entries.push((k, v));
                }
                Term::Map(entries)
            }
            t::COMMA | t::RBRACE => {
                let mut items = vec![first];
                while self.eat(t::COMMA) {
                    items.push(self.term()?);
                }
                Term::Set(items)
            }
            _ => return self.no_viable(0),
        };
        self.expect_f(t::RBRACE, &[F_END])?;
        Ok(out)
    }

    fn list_literal(&mut self) -> PResult<Term<'a>> {
        self.expect(t::LBRACKET)?;
        let mut items = Vec::new();
        let ty = self.la(1);
        if is_term_start(ty, self.d) || ty == t::MINUS {
            items.push(self.term()?);
            while self.eat(t::COMMA) {
                items.push(self.term()?);
            }
        }
        self.expect_f(t::RBRACKET, &[F_END])?;
        Ok(Term::List(items))
    }

    fn usertype_literal(&mut self) -> PResult<Term<'a>> {
        self.expect(t::LBRACE)?;
        let mut fields = Vec::new();
        loop {
            let k = self.ident()?;
            self.expect_f(t::COLON, &[F_TERM])?;
            let v = self.term()?;
            fields.push((k, v));
            if !self.eat(t::COMMA) {
                break;
            }
        }
        self.expect_f(t::RBRACE, &[F_END])?;
        Ok(Term::Udt(fields))
    }

    pub(crate) fn tuple_literal(&mut self) -> PResult<Term<'a>> {
        self.expect_f(t::LPAREN, &[F_TERM])?;
        let mut items = vec![self.term()?];
        while self.eat(t::COMMA) {
            items.push(self.term()?);
        }
        self.expect_f(t::RPAREN, &[F_END])?;
        Ok(Term::Tuple(items))
    }

    /// `function: functionName '(' functionArgs? ')'`.
    fn function(&mut self) -> PResult<FunctionCall<'a>> {
        let alt = if self.cassandra() { self.predict(&dfa_tables::DFA231)? } else { 2 };
        let name = self.function_name()?;
        if self.scylla() && self.at(t::LPAREN) {
            let next = self.la(2);
            if !(is_term_start(next, self.d) || next == t::RPAREN || next == t::MINUS) {
                return self.no_viable(0);
            }
        }
        self.expect_f(t::LPAREN, &[F_TERM, t::RPAREN])?;
        let mut args = Vec::new();
        if alt == 2 && !(self.scylla() && self.at(t::RPAREN)) {
            args.push(self.term()?);
            while self.eat(t::COMMA) {
                args.push(self.term()?);
            }
        }
        self.expect_f(t::RPAREN, &[F_END])?;
        Ok(FunctionCall { name, args })
    }

    /// `functionName: (keyspaceName '.')? allowedFunctionName`.
    pub(crate) fn function_name(&mut self) -> PResult<FunctionName<'a>> {
        let has_ks = if self.cassandra() {
            self.predict(&dfa_tables::DFA229)? == 1
        } else {
            let ty = self.la(1);
            // Names that cannot name a function are taken as a keyspace, except on the left of a
            // relation, where any identifier names a function.
            ty == t::QMARK
                || (!self.relation_function && matches!(ty, t::K_KEY | t::K_TTL | t::K_WRITETIME))
                || (t::is_ident(ty, self.d) && self.la(2) == t::DOT)
        };
        let keyspace = if has_ks {
            let ks = self.keyspace_name()?;
            self.expect_f(t::DOT, &[F_IDENT])?;
            Some(ks)
        } else {
            None
        };
        if self.relation_function && t::is_ident(self.la(1), self.d) {
            self.relation_function = false;
            let tok = self.consume();
            return Ok(FunctionName { keyspace, name: self.ident_from(&tok), keyword: false });
        }
        self.relation_function = false;
        let (name, keyword) = self.allowed_function_name()?;
        Ok(FunctionName { keyspace, name, keyword })
    }

    /// `allowedFunctionName`: the name and whether it was a keyword kept in its written case.
    pub(crate) fn allowed_function_name(&mut self) -> PResult<(Ident<'a>, bool)> {
        let ty = self.la(1);
        if ty == t::IDENT || ty == t::QUOTED_NAME || ty == t::K_TOKEN || ty == t::K_COUNT {
            let tok = self.consume();
            return Ok((self.ident_from(&tok), false));
        }
        if t::is_unreserved_function_keyword(ty, self.d) {
            let tok = self.consume();
            // Basic unreserved keywords keep their case; native type names are lower-cased.
            return Ok((self.ident_from(&tok), !t::is_native_type(ty)));
        }
        self.no_viable(0)
    }

    // -----------------------------------------------------------------------------------------
    // Types
    // -----------------------------------------------------------------------------------------

    /// `comparatorType`.
    pub(crate) fn comparator_type(&mut self) -> PResult<RawType<'a>> {
        self.nested(|p| p.comparator_type_unguarded())
    }

    fn comparator_type_unguarded(&mut self) -> PResult<RawType<'a>> {
        if self.cassandra() && self.spec == 0 {
            return self.type_r().ok_or(Fail);
        }
        self.comparator_type_strict()
    }

    /// `comparatorType` stopping at the first error.
    fn comparator_type_strict(&mut self) -> PResult<RawType<'a>> {
        let alt = if self.cassandra() { self.predict(&dfa_tables::DFA262)? } else { self.scylla_type_alt()? };
        match alt {
            1 => Ok(RawType::Native(self.native_type()?)),
            2 => self.collection_type(),
            3 => {
                self.expect(t::K_TUPLE)?;
                self.expect(t::LT)?;
                let mut types = vec![self.comparator_type()?];
                while self.eat(t::COMMA) {
                    types.push(self.comparator_type()?);
                }
                self.expect_f(t::GT, &[F_END, t::COMMA, t::RPAREN, F_IDENT])?;
                Ok(RawType::Tuple(types))
            }
            4 => {
                self.expect(t::K_VECTOR)?;
                self.expect(t::LT)?;
                let elem = self.comparator_type()?;
                self.expect(t::COMMA)?;
                let dim = self.expect(t::INTEGER)?;
                let dim_text = self.text(&dim);
                if dim_text.parse::<i32>().is_err() && self.cassandra() {
                    return Err(self.number_format_error(&dim_text));
                }
                self.expect_f(t::GT, &[F_END, t::COMMA, t::RPAREN, F_IDENT])?;
                Ok(RawType::Vector(Box::new((elem, dim_text))))
            }
            5 => Ok(RawType::User(self.user_type_name()?)),
            6 => {
                self.expect(t::K_FROZEN)?;
                self.expect(t::LT)?;
                let inner = self.comparator_type()?;
                self.expect_f(t::GT, &[F_END, t::COMMA, t::RPAREN, F_IDENT])?;
                if let RawType::Native(n) = inner {
                    self.custom_error(format!(
                        "frozen<> is only allowed on collections, tuples, and user-defined types (got {})",
                        n.as_str()
                    ))?;
                }
                if let RawType::Custom(ref c) = inner {
                    let msg = format!(
                        "frozen<> is only allowed on collections, tuples, and user-defined types (got '{}')",
                        crate::custom_type::display(c)
                    );
                    self.custom_error(msg)?;
                }
                Ok(RawType::Frozen(Box::new(inner)))
            }
            _ if self.scylla() => self.no_viable(0),
            _ => {
                let tok = self.expect(t::STRING_LITERAL)?;
                let class = self.text(&tok);
                if let Some(msg) = crate::custom_type::check(&class) {
                    self.custom_error(msg)?;
                }
                Ok(RawType::Custom(class))
            }
        }
    }

    /// `comparatorType` with Cassandra's error recovery: a failed sub-rule yields `None` and the
    /// enclosing rule carries on, as ANTLR's generated rules do, so the `NullPointerException`s
    /// Cassandra's actions then raise (`tuple<int,>`, `frozen<>`) are reproduced. Returns `None`
    /// after recording an error.
    fn type_r(&mut self) -> Option<RawType<'a>> {
        self.nested(|p| p.type_r_unguarded().ok_or(Fail)).ok()
    }

    fn type_r_unguarded(&mut self) -> Option<RawType<'a>> {
        let alt = self.predict(&dfa_tables::DFA262).ok()?;
        let m = |p: &mut Self, ty: Tt| p.expect(ty).is_ok();
        let close: &[Tt] = &[F_END, t::COMMA, t::RPAREN, F_IDENT];
        match alt {
            1 => self.native_type().ok().map(RawType::Native),
            2 => {
                let kw = self.la(1);
                if !matches!(kw, t::K_MAP | t::K_LIST | t::K_SET) {
                    let _ = self.no_viable::<()>(0);
                    return None;
                }
                self.consume();
                if !m(self, t::LT) {
                    return None;
                }
                let first = self.type_r();
                let second = if kw == t::K_MAP {
                    if !m(self, t::COMMA) {
                        return None;
                    }
                    Some(self.type_r())
                } else {
                    None
                };
                if self.expect_f(t::GT, close).is_err() {
                    return None;
                }
                match (kw, first, second) {
                    (t::K_MAP, Some(k), Some(Some(v))) => Some(RawType::Map(Box::new((k, v)))),
                    (t::K_LIST, Some(e), _) => Some(RawType::List(Box::new(e))),
                    (t::K_SET, Some(e), _) => Some(RawType::Set(Box::new(e))),
                    _ => None,
                }
            }
            3 => {
                if !m(self, t::K_TUPLE) || !m(self, t::LT) {
                    return None;
                }
                let mut types = vec![self.type_r()];
                while self.eat(t::COMMA) {
                    types.push(self.type_r());
                }
                if self.expect_f(t::GT, close).is_err() {
                    return None;
                }
                if types.iter().any(Option::is_none) {
                    self.null_pointer(
                        "Cannot invoke \"org.apache.cassandra.cql3.CQL3Type$Raw.supportsFreezing()\" because \"t\" is null",
                    );
                    return None;
                }
                Some(RawType::Tuple(types.into_iter().flatten().collect()))
            }
            4 => {
                if !m(self, t::K_VECTOR) || !m(self, t::LT) {
                    return None;
                }
                let elem = self.type_r();
                if !m(self, t::COMMA) {
                    return None;
                }
                let dim = self.expect(t::INTEGER).ok()?;
                let dim_text = self.text(&dim);
                if dim_text.parse::<i32>().is_err() {
                    self.number_format_error(&dim_text);
                    return None;
                }
                if self.expect_f(t::GT, close).is_err() {
                    return None;
                }
                Some(RawType::Vector(Box::new((elem?, dim_text))))
            }
            5 => {
                // `non_type_ident` failing still yields a `UTName` (with a null name).
                let mut in_name = false;
                match self.user_type_name_staged(&mut in_name) {
                    Ok(n) => Some(RawType::User(n)),
                    Err(_) if in_name => Some(RawType::User(QualifiedName { keyspace: None, name: Ident::unquoted("") })),
                    Err(_) => None,
                }
            }
            6 => {
                if !m(self, t::K_FROZEN) || !m(self, t::LT) {
                    return None;
                }
                let inner = self.type_r();
                if self.expect_f(t::GT, close).is_err() {
                    return None;
                }
                let Some(inner) = inner else {
                    self.null_pointer("Cannot invoke \"org.apache.cassandra.cql3.CQL3Type$Raw.freeze()\" because \"f\" is null");
                    return None;
                };
                let got = match &inner {
                    RawType::Native(n) => Some(n.as_str().to_owned()),
                    RawType::Custom(c) => Some(format!("'{}'", crate::custom_type::display(c))),
                    _ => None,
                };
                if let Some(got) = got {
                    let _ = self.custom_error(format!(
                        "frozen<> is only allowed on collections, tuples, and user-defined types (got {got})"
                    ));
                    return None;
                }
                Some(RawType::Frozen(Box::new(inner)))
            }
            _ => {
                let tok = self.expect(t::STRING_LITERAL).ok()?;
                let class = self.text(&tok);
                if let Some(msg) = crate::custom_type::check(&class) {
                    let _ = self.custom_error(msg);
                    return None;
                }
                Some(RawType::Custom(class))
            }
        }
    }

    /// ScyllaDB's `comparatorType` decision. A type name must be followed by something that can
    /// follow a type, else the decision fails at the name.
    fn scylla_type_alt(&mut self) -> PResult<u32> {
        const FOLLOW: &[Tt] = &[
            t::COMMA,
            t::RPAREN,
            t::GT,
            t::K_STATIC,
            t::K_PRIMARY,
            t::K_TTL,
            t::EOF,
            t::SEMI,
            t::K_LANGUAGE,
            t::K_REDUCEFUNC,
            t::K_FINALFUNC,
            t::K_INITCOND,
        ];
        let ty = self.la(1);
        let next = self.la(2);
        if ty == t::K_EMPTY {
            // `empty` passes as a type name, then fails as a reserved one.
            self.consume();
            self.custom_error("Invalid (reserved) user type name empty")?;
            return self.no_viable(0);
        }
        Ok(match ty {
            t::K_SET => 2,
            t::K_MAP | t::K_LIST if next == t::LT => 2,
            t::K_TUPLE if next == t::LT => 3,
            t::K_VECTOR if next == t::LT => 4,
            t::K_FROZEN if next == t::LT => 6,
            _ if t::is_ident(ty, self.d) && next == t::DOT => 5,
            _ if t::is_native_type(ty) || matches!(ty, t::K_MAP | t::K_LIST | t::K_SET | t::K_TUPLE | t::K_VECTOR | t::K_FROZEN) => {
                if FOLLOW.contains(&next) && t::is_native_type(ty) {
                    1
                } else if FOLLOW.contains(&next) {
                    5
                } else {
                    return self.no_viable(0);
                }
            }
            _ if t::is_ident(ty, self.d) => 5,
            _ => return self.no_viable(0),
        })
    }

    /// Java's `NumberFormatException` (`Integer.parseInt`, `Long.parseLong`) escaping the parser.
    pub(crate) fn number_format_error(&mut self, text: &str) -> Fail {
        self.throw_runtime("NumberFormatException", &format!("For input string: \"{text}\""))
    }

    fn collection_type(&mut self) -> PResult<RawType<'a>> {
        let follow: &[Tt] = &[F_END, t::COMMA, t::RPAREN, F_IDENT];
        match self.la(1) {
            t::K_MAP => {
                self.consume();
                self.expect_f(t::LT, &[F_IDENT])?;
                let k = self.comparator_type()?;
                self.expect(t::COMMA)?;
                let v = self.comparator_type()?;
                self.expect_f(t::GT, follow)?;
                Ok(RawType::Map(Box::new((k, v))))
            }
            t::K_LIST => {
                self.consume();
                self.expect_f(t::LT, &[F_IDENT])?;
                let e = self.comparator_type()?;
                self.expect_f(t::GT, follow)?;
                Ok(RawType::List(Box::new(e)))
            }
            t::K_SET => {
                self.consume();
                self.expect_f(t::LT, &[F_IDENT])?;
                let e = self.comparator_type()?;
                self.expect_f(t::GT, follow)?;
                Ok(RawType::Set(Box::new(e)))
            }
            _ => self.no_viable(0),
        }
    }

    /// `native_type`.
    pub(crate) fn native_type(&mut self) -> PResult<NativeType> {
        let n = match self.la(1) {
            t::K_ASCII => NativeType::Ascii,
            t::K_BIGINT => NativeType::Bigint,
            t::K_BLOB => NativeType::Blob,
            t::K_BOOLEAN => NativeType::Boolean,
            t::K_COUNTER => NativeType::Counter,
            t::K_DECIMAL => NativeType::Decimal,
            t::K_DOUBLE => NativeType::Double,
            t::K_DURATION => NativeType::Duration,
            t::K_FLOAT => NativeType::Float,
            t::K_INET => NativeType::Inet,
            t::K_INT => NativeType::Int,
            t::K_SMALLINT => NativeType::Smallint,
            t::K_TEXT => NativeType::Text,
            t::K_TIMESTAMP => NativeType::Timestamp,
            t::K_TINYINT => NativeType::Tinyint,
            t::K_UUID => NativeType::Uuid,
            t::K_VARCHAR => NativeType::Varchar,
            t::K_VARINT => NativeType::Varint,
            t::K_TIMEUUID => NativeType::Timeuuid,
            t::K_DATE => NativeType::Date,
            t::K_TIME => NativeType::Time,
            _ => return self.no_viable(0),
        };
        self.consume();
        Ok(n)
    }
}

/// Whether a token is a `constant`.
pub(crate) fn is_constant(ty: Tt) -> bool {
    matches!(
        ty,
        t::STRING_LITERAL
            | t::INTEGER
            | t::FLOAT
            | t::BOOLEAN
            | t::DURATION
            | t::UUID
            | t::HEXNUMBER
            | t::K_POSITIVE_NAN
            | t::K_NEGATIVE_NAN
            | t::K_POSITIVE_INFINITY
            | t::K_NEGATIVE_INFINITY
    )
}

/// FIRST(simpleTerm): what can start a term (without the unary minus).
pub(crate) fn is_term_start(ty: Tt, d: Dialect) -> bool {
    matches!(
        ty,
        t::STRING_LITERAL
            | t::INTEGER
            | t::FLOAT
            | t::BOOLEAN
            | t::DURATION
            | t::UUID
            | t::HEXNUMBER
            | t::K_POSITIVE_NAN
            | t::K_NEGATIVE_NAN
            | t::K_POSITIVE_INFINITY
            | t::K_NEGATIVE_INFINITY
            | t::K_NULL
            | t::QMARK
            | t::COLON
            | t::LBRACKET
            | t::LBRACE
            | t::LPAREN
            | t::K_TOKEN
            | t::K_COUNT
            | t::IDENT
            | t::QUOTED_NAME
    ) || (ty == t::K_CAST && d == Dialect::Cassandra)
        || t::is_unreserved(ty, d)
}
