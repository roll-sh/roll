//! Recursive-descent parser for the Painless grammar (`PainlessParser.g4`).
//!
//! The grammar is LL(*) in a few places that ANTLR resolves with full-context prediction; here
//! those spots use bounded token scans instead: telling declarations from expression statements,
//! functions from statements, casts from parenthesized expressions, and lambdas/function
//! references from expressions in argument lists. Where ANTLR would pick between two viable
//! parses it picks the lower-numbered alternative, and so do we (`(x)[0]` indexes `x`).
//!
//! Each node gets the offset of the first token of its grammar rule — what Elasticsearch's
//! `Walker` stores as the node location — so error positions match. Syntax errors use the same
//! message shapes as Elasticsearch's `ParserErrorStrategy`.

use crate::ast::*;
use crate::error::CompileError;
use crate::lexer::{error_display, tokenize_lazy, Tok, Token};

pub fn parse(src: &str) -> Result<Source, CompileError> {
    let (toks, lex_error) = tokenize_lazy(src);
    let mut p = Parser { src, toks, pos: 0, empty_lambda: false, lex_error, pred: 0 };
    let out = p.source()?;
    if let Some(e) = p.lex_error.take() {
        return Err(e);
    }
    if p.empty_lambda {
        // Elasticsearch's tree builder fails on a lambda with an empty block body (after the
        // parse succeeded, before any analysis): a message-less NPE with no location
        return Err(CompileError::new(crate::error::NO_LOCATION, "null_pointer_exception", ""));
    }
    Ok(out)
}

struct Parser<'a> {
    src: &'a str,
    toks: Vec<Token>,
    pos: usize,
    /// A lambda with an empty block body was parsed.
    empty_lambda: bool,
    /// The lexing error behind a trailing [`Tok::LexError`] token.
    lex_error: Option<CompileError>,
    /// How many enclosing ANTLR predictions are still scanning ahead: a block's statement
    /// loop (which cannot tell `statement` from the trailing `dstatement` before the
    /// terminator) or an `expression` (which cannot pick its alternative before the end of
    /// its first `noncondexpression`). A syntax error under one is a no-viable-alternative
    /// error at the token where every alternative died, never an input mismatch.
    pred: u32,
}

type PResult<T> = Result<T, CompileError>;

impl<'a> Parser<'a> {
    fn tok(&self) -> Token {
        self.toks[self.pos]
    }
    fn kind(&self) -> Tok {
        self.toks[self.pos].kind
    }
    fn la(&self, n: usize) -> Tok {
        self.toks.get(self.pos + n).map(|t| t.kind).unwrap_or(Tok::Eof)
    }
    fn at(&self) -> u32 {
        self.toks[self.pos].start
    }
    fn text(&self, t: Token) -> &'a str {
        t.text(self.src)
    }
    fn bump(&mut self) -> Token {
        let t = self.toks[self.pos];
        if t.kind != Tok::Eof {
            self.pos += 1;
        }
        t
    }
    fn eat(&mut self, k: Tok) -> bool {
        if self.kind() == k {
            self.bump();
            true
        } else {
            false
        }
    }

    fn display(&self, t: Token) -> String {
        if t.kind == Tok::Eof {
            "'<EOF>'".to_string()
        } else {
            format!("'{}'", error_display(self.text(t)))
        }
    }

    fn err_at(&self, t: Token, msg: String) -> CompileError {
        if let Some(e) = self.lex_error(t) {
            return e;
        }
        CompileError::illegal_argument(t.start, msg)
    }

    /// The lexer's error when the parser reached the token it failed on.
    fn lex_error(&self, t: Token) -> Option<CompileError> {
        if t.kind == Tok::LexError { self.lex_error.clone() } else { None }
    }

    /// `NoViableAltException` in ANTLR's terms.
    fn no_viable(&self) -> CompileError {
        self.no_viable_at(self.tok())
    }

    /// `NoViableAltException` with `t` as the offending token.
    fn no_viable_at(&self, t: Token) -> CompileError {
        if let Some(e) = self.lex_error(t) {
            return e;
        }
        let e = if t.kind == Tok::Eof {
            self.err_at(t, "unexpected end of script.".into())
        } else {
            self.err_at(t, format!("invalid sequence of tokens near [{}].", self.display(t)))
        };
        e.caused_by("no_viable_alt_exception", None)
    }

    /// `InputMismatchException` / `recoverInline` in ANTLR's terms.
    fn mismatch(&self, expected: &str) -> CompileError {
        let t = self.tok();
        if let Some(e) = self.lex_error(t) {
            return e;
        }
        if self.pred > 0 {
            return self.no_viable_at(t);
        }
        self.err_at(t, format!("unexpected token [{}] was expecting one of [{}].", self.display(t), expected))
    }

    /// Matches a closing token inside an expression: at the end of input ANTLR's prediction
    /// fails before the match, which it reports as an unexpected end of script.
    /// Inside expressions ANTLR's adaptive prediction fails before the match, so a wrong token
    /// is reported as a no-viable-alternative error at that token.
    fn close(&mut self, k: Tok) -> PResult<Token> {
        if self.kind() == k {
            return Ok(self.bump());
        }
        Err(self.no_viable())
    }

    /// Matches a token that the statement structure requires.
    fn expect(&mut self, k: Tok) -> PResult<Token> {
        if self.kind() == k {
            return Ok(self.bump());
        }
        Err(self.mismatch(k.display_name()))
    }

    // ---------------------------------------------------------------- source & statements

    fn source(&mut self) -> PResult<Source> {
        let mut functions = Vec::new();
        // `function*`: the loop decision scans until it can tell a function from a statement
        loop {
            if self.is_function_start() {
                functions.push(self.function()?);
                continue;
            }
            if let Some(d) = self.decision_dies(Tok::LP, &[Tok::Semicolon, Tok::Eof]) {
                return Err(self.no_viable_at(self.toks[d]));
            }
            break;
        }
        if self.kind() != Tok::Eof && !self.starts_statement() {
            return Err(self.no_viable());
        }
        let mut stmts = Vec::new();
        loop {
            if self.kind() == Tok::Eof {
                break;
            }
            if !self.starts_statement() {
                return Err(self.mismatch("<EOF>"));
            }
            stmts.push(self.statement(None)?);
        }
        let end = self.at();
        Ok(Source { functions, stmts, end })
    }

    /// Scans a `decltype` (`type ('[' ']')*`) starting at token `i`; returns the index after it.
    fn scan_decltype(&self, mut i: usize) -> Option<usize> {
        let k = self.toks.get(i)?.kind;
        match k {
            Tok::Def | Tok::Primitive => i += 1,
            Tok::Id => {
                i += 1;
                while self.toks.get(i)?.kind == Tok::Dot && self.toks.get(i + 1)?.kind == Tok::DotId {
                    i += 2;
                }
            }
            _ => return None,
        }
        while self.toks.get(i)?.kind == Tok::LBrace && self.toks.get(i + 1)?.kind == Tok::RBrace {
            i += 2;
        }
        Some(i)
    }

    /// Whether the decision between a `decltype ID <other> ...` alternative (a function, with
    /// `other` = `(`, or a for-each, with `other` = `:`) and a declaration-or-expression
    /// alternative (ending in one of `terms`) fails as a whole: returns the index of the token
    /// where both died. The caller has already ruled out `decltype ID <other>`.
    fn decision_dies(&self, other: Tok, terms: &[Tok]) -> Option<usize> {
        let start = self.pos;
        let i = self.scan_decltype(start)?;
        let kind = |j: usize| self.toks.get(j).map_or(Tok::Eof, |t| t.kind);
        let dims = i > start + 1 && kind(i - 1) == Tok::RBrace;
        if kind(i) == Tok::Id {
            let d = i + 1;
            let k = kind(d);
            if k == other || matches!(k, Tok::Assign | Tok::Comma) || terms.contains(&k) {
                return None;
            }
            return Some(d.min(self.toks.len() - 1));
        }
        // only a type so far: an expression (a plain name or field chain) may go on
        let k = kind(i);
        if kind(start) == Tok::Id && !dims && Self::continues_expression(k, terms) {
            return None;
        }
        // `type [` is still a type until the token after the bracket
        let d = if k == Tok::LBrace { i + 1 } else { i };
        Some(d.min(self.toks.len() - 1))
    }

    /// Whether `k` may follow a name or field access in an expression statement.
    fn continues_expression(k: Tok, terms: &[Tok]) -> bool {
        Self::binop(k).is_some()
            || terms.contains(&k)
            || matches!(
                k,
                Tok::Cond
                    | Tok::Elvis
                    | Tok::Instanceof
                    | Tok::Dot
                    | Tok::NsDot
                    | Tok::LBrace
                    | Tok::LP
                    | Tok::Incr
                    | Tok::Decr
                    | Tok::Assign
                    | Tok::AAdd
                    | Tok::ASub
                    | Tok::AMul
                    | Tok::ADiv
                    | Tok::ARem
                    | Tok::AAnd
                    | Tok::AXor
                    | Tok::AOr
                    | Tok::ALsh
                    | Tok::ARsh
                    | Tok::AUsh
            )
    }

    fn is_function_start(&self) -> bool {
        match self.scan_decltype(self.pos) {
            Some(i) => {
                self.toks.get(i).map(|t| t.kind) == Some(Tok::Id) && self.toks.get(i + 1).map(|t| t.kind) == Some(Tok::LP)
            }
            None => false,
        }
    }

    fn starts_statement(&self) -> bool {
        matches!(self.kind(), Tok::If | Tok::While | Tok::For | Tok::Try | Tok::Do | Tok::Continue | Tok::Break | Tok::Return | Tok::Throw)
            || self.starts_expression()
            || matches!(self.kind(), Tok::Def | Tok::Primitive)
    }

    fn starts_expression(&self) -> bool {
        matches!(
            self.kind(),
            Tok::Incr
                | Tok::Decr
                | Tok::Add
                | Tok::Sub
                | Tok::BoolNot
                | Tok::BwNot
                | Tok::LP
                | Tok::Octal
                | Tok::Hex
                | Tok::Integer
                | Tok::Decimal
                | Tok::True
                | Tok::False
                | Tok::Null
                | Tok::Str
                | Tok::Regex
                | Tok::LBrace
                | Tok::Id
                | Tok::Dollar
                | Tok::New
        )
    }

    fn decltype_text(&mut self) -> PResult<String> {
        let mut s = String::new();
        match self.kind() {
            Tok::Def | Tok::Primitive => {
                let t = self.bump();
                s.push_str(self.text(t));
            }
            Tok::Id => {
                let t = self.bump();
                s.push_str(self.text(t));
                while self.kind() == Tok::Dot && self.la(1) == Tok::DotId {
                    self.bump();
                    let t = self.bump();
                    s.push('.');
                    s.push_str(self.text(t));
                }
            }
            _ => return Err(self.no_viable()),
        }
        while self.kind() == Tok::LBrace {
            self.bump();
            self.expect(Tok::RBrace)?;
            s.push_str("[]");
        }
        Ok(s)
    }

    /// `type`: `DEF | PRIMITIVE | ID (DOT DOTID)*` (no array dimensions).
    fn type_text(&mut self) -> PResult<String> {
        let mut s = String::new();
        match self.kind() {
            Tok::Def | Tok::Primitive => {
                let t = self.bump();
                s.push_str(self.text(t));
            }
            Tok::Id => {
                let t = self.bump();
                s.push_str(self.text(t));
                while self.kind() == Tok::Dot && self.la(1) == Tok::DotId {
                    self.bump();
                    let t = self.bump();
                    s.push('.');
                    s.push_str(self.text(t));
                }
            }
            _ => return Err(self.no_viable()),
        }
        Ok(s)
    }

    fn function(&mut self) -> PResult<Function> {
        let loc = self.at();
        let ret = self.decltype_text()?;
        let name_tok = self.expect(Tok::Id)?;
        let name = self.text(name_tok).to_string();
        self.expect(Tok::LP)?;
        let mut params = Vec::new();
        if matches!(self.kind(), Tok::Def | Tok::Primitive | Tok::Id) {
            loop {
                let ty = self.decltype_text()?;
                let n = self.expect(Tok::Id)?;
                params.push((ty, self.text(n).to_string()));
                if !self.eat(Tok::Comma) {
                    break;
                }
            }
        }
        self.expect(Tok::RP)?;
        let body = self.block()?.unwrap_or(Block { loc, stmts: Vec::new() });
        Ok(Function { loc, ret, name, params, body: Block { loc, stmts: body.stmts } })
    }

    /// `block: '{' statement* dstatement? '}'`; an empty block is `None`, as in the Walker.
    fn block(&mut self) -> PResult<Option<Block>> {
        let loc = self.at();
        self.expect(Tok::LBrack)?;
        let mut stmts = Vec::new();
        loop {
            match self.kind() {
                Tok::RBrack => {
                    self.bump();
                    break;
                }
                Tok::If | Tok::While | Tok::For | Tok::Try => stmts.push(self.rstatement()?),
                _ => {
                    if !self.starts_statement() {
                        return Err(self.no_viable());
                    }
                    self.pred += 1;
                    let s = self.dstatement();
                    self.pred -= 1;
                    stmts.push(s?);
                    if self.eat(Tok::Semicolon) {
                        continue;
                    }
                    if self.kind() == Tok::RBrack {
                        continue;
                    }
                    // ANTLR's prediction between `statement` and the block's trailing
                    // `dstatement` fails at the token after the statement
                    return Err(self.no_viable());
                }
            }
        }
        if stmts.is_empty() {
            Ok(None)
        } else {
            Ok(Some(Block { loc, stmts }))
        }
    }

    /// `statement: rstatement | dstatement (';' | EOF)`.
    fn statement(&mut self, _ctx: Option<()>) -> PResult<Stmt> {
        crate::guard::compile_stack()?;
        match self.kind() {
            Tok::If | Tok::While | Tok::For | Tok::Try => self.rstatement(),
            _ => {
                if !self.starts_statement() {
                    return Err(self.no_viable());
                }
                let s = self.dstatement()?;
                if self.eat(Tok::Semicolon) || self.kind() == Tok::Eof {
                    Ok(s)
                } else {
                    Err(self.mismatch("{<EOF>, ';'}"))
                }
            }
        }
    }

    /// `trailer: block | statement` — always a block in the tree.
    fn trailer(&mut self) -> PResult<Option<Block>> {
        if self.kind() == Tok::LBrack {
            return self.block();
        }
        let loc = self.at();
        let s = self.statement(None)?;
        Ok(Some(Block { loc, stmts: vec![s] }))
    }

    fn paren_expr(&mut self) -> PResult<Expr> {
        self.expect(Tok::LP)?;
        let e = self.expression()?;
        self.expect(Tok::RP)?;
        Ok(e)
    }

    fn rstatement(&mut self) -> PResult<Stmt> {
        let loc = self.at();
        match self.kind() {
            Tok::If => {
                self.bump();
                let cond = self.paren_expr()?;
                let then = self.trailer()?;
                if self.eat(Tok::Else) {
                    let els = self.trailer()?;
                    Ok(Stmt { loc, kind: StmtKind::IfElse { cond, then, els } })
                } else {
                    Ok(Stmt { loc, kind: StmtKind::If { cond, then } })
                }
            }
            Tok::While => {
                self.bump();
                let cond = self.paren_expr()?;
                let body = if self.eat(Tok::Semicolon) { None } else { self.trailer()? };
                Ok(Stmt { loc, kind: StmtKind::While { cond, body } })
            }
            Tok::For => {
                self.bump();
                self.expect(Tok::LP)?;
                // `for (x in list)`
                if self.kind() == Tok::Id && self.la(1) == Tok::In {
                    let n = self.bump();
                    let name = self.text(n).to_string();
                    self.bump();
                    let iterable = self.expression()?;
                    self.expect(Tok::RP)?;
                    let body = self.trailer()?;
                    return Ok(Stmt { loc, kind: StmtKind::Each { ty: "def".into(), name, iterable, body } });
                }
                // `for (T x : list)`
                if let Some(i) = self.scan_decltype(self.pos)
                    && self.toks.get(i).map(|t| t.kind) == Some(Tok::Id) && self.toks.get(i + 1).map(|t| t.kind) == Some(Tok::Colon) {
                        let ty = self.decltype_text()?;
                        let n = self.bump();
                        let name = self.text(n).to_string();
                        self.bump();
                        let iterable = self.expression()?;
                        self.expect(Tok::RP)?;
                        let body = self.trailer()?;
                        return Ok(Stmt { loc, kind: StmtKind::Each { ty, name, iterable, body } });
                    }
                if let Some(d) = self.decision_dies(Tok::Colon, &[Tok::Semicolon]) {
                    return Err(self.no_viable_at(self.toks[d]));
                }
                let init = if self.kind() == Tok::Semicolon {
                    None
                } else if self.is_declaration() {
                    let dl = self.at();
                    Some(Init::Decl(dl, self.declaration()?))
                } else {
                    Some(Init::Expr(self.expression()?))
                };
                self.expect(Tok::Semicolon)?;
                let cond = if self.kind() == Tok::Semicolon { None } else { Some(self.expression()?) };
                self.expect(Tok::Semicolon)?;
                let after = if self.kind() == Tok::RP { None } else { Some(self.expression()?) };
                self.expect(Tok::RP)?;
                let body = if self.eat(Tok::Semicolon) { None } else { self.trailer()? };
                Ok(Stmt { loc, kind: StmtKind::For { init, cond, after, body } })
            }
            Tok::Try => {
                self.bump();
                let block = self.block()?;
                let mut catches = Vec::new();
                if self.kind() != Tok::Catch {
                    return Err(self.mismatch("'catch'"));
                }
                while self.kind() == Tok::Catch {
                    let cl = self.at();
                    self.bump();
                    self.expect(Tok::LP)?;
                    let ty = self.type_text()?;
                    let n = self.expect(Tok::Id)?;
                    let name = self.text(n).to_string();
                    self.expect(Tok::RP)?;
                    let block = self.block()?;
                    catches.push(Catch { loc: cl, ty, name, block });
                }
                Ok(Stmt { loc, kind: StmtKind::Try { block, catches } })
            }
            _ => Err(self.no_viable()),
        }
    }

    fn is_declaration(&self) -> bool {
        match self.kind() {
            Tok::Def | Tok::Primitive => true,
            // `T[]` can only start a declaration
            Tok::Id => match self.scan_decltype(self.pos) {
                Some(i) => self.toks.get(i).map(|t| t.kind) == Some(Tok::Id) || self.toks[i - 1].kind == Tok::RBrace,
                None => false,
            },
            _ => false,
        }
    }

    fn declaration(&mut self) -> PResult<Vec<Decl>> {
        let ty = self.decltype_text()?;
        let mut out = Vec::new();
        if self.kind() != Tok::Id {
            return Err(self.mismatch("ID"));
        }
        loop {
            let loc = self.at();
            let n = self.expect(Tok::Id)?;
            let name = self.text(n).to_string();
            let value = if self.eat(Tok::Assign) { Some(self.expression()?) } else { None };
            out.push(Decl { loc, ty: ty.clone(), name, value });
            if !self.eat(Tok::Comma) {
                break;
            }
        }
        Ok(out)
    }

    fn dstatement(&mut self) -> PResult<Stmt> {
        let loc = self.at();
        match self.kind() {
            Tok::Do => {
                self.bump();
                let body = self.block()?;
                self.expect(Tok::While)?;
                let cond = self.paren_expr()?;
                Ok(Stmt { loc, kind: StmtKind::Do { cond, body } })
            }
            Tok::Continue => {
                self.bump();
                Ok(Stmt { loc, kind: StmtKind::Continue })
            }
            Tok::Break => {
                self.bump();
                Ok(Stmt { loc, kind: StmtKind::Break })
            }
            Tok::Return => {
                self.bump();
                let e = if self.starts_expression() { Some(self.expression()?) } else { None };
                Ok(Stmt { loc, kind: StmtKind::Return(e) })
            }
            Tok::Throw => {
                self.bump();
                let e = self.expression()?;
                Ok(Stmt { loc, kind: StmtKind::Throw(e) })
            }
            _ if self.is_declaration() => Ok(Stmt { loc, kind: StmtKind::Decl(self.declaration()?) }),
            _ => {
                let e = self.expression()?;
                Ok(Stmt { loc, kind: StmtKind::Expr(e) })
            }
        }
    }

    // ---------------------------------------------------------------- expressions

    fn expression(&mut self) -> PResult<Expr> {
        crate::guard::compile_stack()?;
        let start = self.at();
        self.pred += 1;
        let lhs = self.noncond(0);
        self.pred -= 1;
        let lhs = lhs?;
        let op = match self.kind() {
            Tok::Cond => {
                self.bump();
                let t = self.expression()?;
                self.close(Tok::Colon)?;
                let f = self.expression()?;
                return Ok(Expr { loc: start, kind: ExprKind::Conditional { cond: Box::new(lhs), t: Box::new(t), f: Box::new(f) } });
            }
            Tok::Assign => None,
            Tok::AAdd => Some(Op::Add),
            Tok::ASub => Some(Op::Sub),
            Tok::AMul => Some(Op::Mul),
            Tok::ADiv => Some(Op::Div),
            Tok::ARem => Some(Op::Rem),
            Tok::AAnd => Some(Op::BwAnd),
            Tok::AXor => Some(Op::Xor),
            Tok::AOr => Some(Op::BwOr),
            Tok::ALsh => Some(Op::Lsh),
            Tok::ARsh => Some(Op::Rsh),
            Tok::AUsh => Some(Op::Ush),
            _ => return Ok(lhs),
        };
        self.bump();
        let rhs = self.expression()?;
        Ok(Expr { loc: start, kind: ExprKind::Assign { lhs: Box::new(lhs), rhs: Box::new(rhs), op, post: false } })
    }

    fn binop(k: Tok) -> Option<(u8, Op)> {
        Some(match k {
            Tok::Mul => (13, Op::Mul),
            Tok::Div => (13, Op::Div),
            Tok::Rem => (13, Op::Rem),
            Tok::Add => (12, Op::Add),
            Tok::Sub => (12, Op::Sub),
            Tok::Find => (11, Op::Find),
            Tok::Match => (11, Op::Match),
            Tok::Lsh => (10, Op::Lsh),
            Tok::Rsh => (10, Op::Rsh),
            Tok::Ush => (10, Op::Ush),
            Tok::Lt => (9, Op::Lt),
            Tok::Lte => (9, Op::Lte),
            Tok::Gt => (9, Op::Gt),
            Tok::Gte => (9, Op::Gte),
            Tok::Eq => (7, Op::Eq),
            Tok::Eqr => (7, Op::Eqr),
            Tok::Ne => (7, Op::Ne),
            Tok::Ner => (7, Op::Ner),
            Tok::BwAnd => (6, Op::BwAnd),
            Tok::Xor => (5, Op::Xor),
            Tok::BwOr => (4, Op::BwOr),
            Tok::BoolAnd => (3, Op::And),
            Tok::BoolOr => (2, Op::Or),
            _ => return None,
        })
    }

    /// Precedence climbing over `noncondexpression`; every binary node is located at the first
    /// token of its left operand, like ANTLR's left-recursive rule contexts.
    fn noncond(&mut self, min: u8) -> PResult<Expr> {
        let start = self.at();
        let mut left = self.unary()?;
        loop {
            let k = self.kind();
            if k == Tok::Instanceof {
                if 8 < min {
                    break;
                }
                self.bump();
                let ty = self.decltype_text()?;
                left = Expr { loc: start, kind: ExprKind::Instanceof { expr: Box::new(left), ty } };
                continue;
            }
            if k == Tok::Elvis {
                if 1 < min {
                    break;
                }
                self.bump();
                let right = self.noncond(1)?;
                left = Expr { loc: start, kind: ExprKind::Elvis { left: Box::new(left), right: Box::new(right) } };
                continue;
            }
            let Some((prec, op)) = Self::binop(k) else { break };
            if prec < min {
                break;
            }
            self.bump();
            let right = self.noncond(prec + 1)?;
            let kind = match op {
                Op::Lt | Op::Lte | Op::Gt | Op::Gte | Op::Eq | Op::Eqr | Op::Ne | Op::Ner => {
                    ExprKind::Comp { op, left: Box::new(left), right: Box::new(right) }
                }
                Op::And | Op::Or => ExprKind::BoolOp { op, left: Box::new(left), right: Box::new(right) },
                _ => ExprKind::Binary { op, left: Box::new(left), right: Box::new(right) },
            };
            left = Expr { loc: start, kind };
        }
        Ok(left)
    }

    fn one(&self, loc: u32) -> Expr {
        Expr { loc, kind: ExprKind::Numeric { text: "1".into(), radix: 10 } }
    }

    fn unary(&mut self) -> PResult<Expr> {
        crate::guard::compile_stack()?;
        let loc = self.at();
        match self.kind() {
            Tok::Incr | Tok::Decr => {
                let op = if self.bump().kind == Tok::Incr { Op::Add } else { Op::Sub };
                let chain = self.chain()?;
                Ok(Expr { loc, kind: ExprKind::Assign { lhs: Box::new(chain), rhs: Box::new(self.one(loc)), op: Some(op), post: false } })
            }
            Tok::Add | Tok::Sub => {
                let op = if self.bump().kind == Tok::Add { Op::Add } else { Op::Sub };
                let child = self.unary()?;
                Ok(Expr { loc, kind: ExprKind::Unary { op, child: Box::new(child) } })
            }
            _ => self.unary_not_addsub(),
        }
    }

    fn unary_not_addsub(&mut self) -> PResult<Expr> {
        let loc = self.at();
        match self.kind() {
            Tok::BoolNot | Tok::BwNot => {
                let op = if self.bump().kind == Tok::BoolNot { Op::Not } else { Op::BwNot };
                let child = self.unary()?;
                Ok(Expr { loc, kind: ExprKind::Unary { op, child: Box::new(child) } })
            }
            Tok::LP => {
                if let Some(e) = self.try_cast()? {
                    return Ok(e);
                }
                self.postfix_chain_after_primary()
            }
            _ => self.postfix_chain_after_primary(),
        }
    }

    fn postfix_chain_after_primary(&mut self) -> PResult<Expr> {
        let loc = self.at();
        let chain = self.chain()?;
        match self.kind() {
            Tok::Incr | Tok::Decr => {
                let op = if self.bump().kind == Tok::Incr { Op::Add } else { Op::Sub };
                Ok(Expr { loc, kind: ExprKind::Assign { lhs: Box::new(chain), rhs: Box::new(self.one(loc)), op: Some(op), post: true } })
            }
            _ => Ok(chain),
        }
    }

    fn can_start_unary_not_addsub(k: Tok) -> bool {
        matches!(
            k,
            Tok::BoolNot
                | Tok::BwNot
                | Tok::LP
                | Tok::Octal
                | Tok::Hex
                | Tok::Integer
                | Tok::Decimal
                | Tok::True
                | Tok::False
                | Tok::Null
                | Tok::Str
                | Tok::Regex
                | Tok::LBrace
                | Tok::Id
                | Tok::Dollar
                | Tok::New
        )
    }

    /// Recognizes `castexpression` at a `(`.
    fn try_cast(&mut self) -> PResult<Option<Expr>> {
        let loc = self.at();
        let i = self.pos + 1;
        let k = self.toks[i].kind;
        // ( PRIMITIVE|DEF ) unary
        if matches!(k, Tok::Primitive | Tok::Def) && self.toks.get(i + 1).map(|t| t.kind) == Some(Tok::RP) {
            self.bump();
            let t = self.bump();
            let ty = self.text(t).to_string();
            self.bump();
            let child = self.unary()?;
            return Ok(Some(Expr { loc, kind: ExprKind::Explicit { ty, child: Box::new(child) } }));
        }
        let Some(end) = self.scan_decltype(i) else { return Ok(None) };
        if self.toks.get(end).map(|t| t.kind) != Some(Tok::RP) {
            if matches!(k, Tok::Primitive | Tok::Def) {
                // only a cast starts `( int`: its prediction fails after the type
                self.pos = end;
                return Err(self.no_viable());
            }
            return Ok(None);
        }
        let dims = (end - i) > 1 && self.toks[end - 1].kind == Tok::RBrace;
        if matches!(k, Tok::Primitive | Tok::Def) && !dims {
            return Ok(None);
        }
        let next = self.toks.get(end + 1).map(|t| t.kind).unwrap_or(Tok::Eof);
        if !Self::can_start_unary_not_addsub(next) {
            return Ok(None);
        }
        if next == Tok::LBrace && !dims {
            // `(x)[i]` is a brace access on `x` when that parses (ANTLR prefers that
            // alternative); otherwise it is a cast of a list/map initializer
            let save = self.pos;
            let attempt = self.postfix_chain_after_primary();
            if attempt.is_ok() {
                return Ok(Some(attempt?));
            }
            self.pos = save;
        }
        self.bump();
        let ty = self.decltype_text()?;
        self.bump();
        let child = self.unary_not_addsub()?;
        Ok(Some(Expr { loc, kind: ExprKind::Explicit { ty, child: Box::new(child) } }))
    }

    fn chain(&mut self) -> PResult<Expr> {
        if self.kind() == Tok::New && self.is_array_creation() {
            return self.array_initializer();
        }
        let prim = self.primary()?;
        self.postfixes(prim)
    }

    fn is_array_creation(&self) -> bool {
        // NEW type '[' …
        let mut i = self.pos + 1;
        match self.toks.get(i).map(|t| t.kind) {
            Some(Tok::Def | Tok::Primitive) => i += 1,
            Some(Tok::Id) => {
                i += 1;
                while self.toks.get(i).map(|t| t.kind) == Some(Tok::Dot) && self.toks.get(i + 1).map(|t| t.kind) == Some(Tok::DotId) {
                    i += 2;
                }
            }
            _ => return false,
        }
        self.toks.get(i).map(|t| t.kind) == Some(Tok::LBrace)
    }

    fn array_initializer(&mut self) -> PResult<Expr> {
        let loc = self.at();
        self.bump(); // new
        let base = self.type_text()?;
        if self.kind() == Tok::LBrace && self.la(1) == Tok::RBrace {
            // new T[] { a, b }
            self.bump();
            self.bump();
            if self.kind() != Tok::LBrack {
                return Err(self.no_viable());
            }
            self.bump();
            let mut values = Vec::new();
            if self.kind() != Tok::RBrack {
                loop {
                    values.push(self.expression()?);
                    if !self.eat(Tok::Comma) {
                        break;
                    }
                }
            }
            self.close(Tok::RBrack)?;
            let e = Expr { loc, kind: ExprKind::NewArray { ty: format!("{base}[]"), values, init: true } };
            return self.postfixes(e);
        }
        let mut ty = base;
        let mut values = Vec::new();
        while self.kind() == Tok::LBrace {
            self.bump();
            ty.push_str("[]");
            values.push(self.expression()?);
            self.close(Tok::RBrace)?;
        }
        let e = Expr { loc, kind: ExprKind::NewArray { ty, values, init: false } };
        // ( postdot postfix* )?
        if matches!(self.kind(), Tok::Dot | Tok::NsDot) {
            let e = self.postfix(e)?;
            return self.postfixes(e);
        }
        Ok(e)
    }

    fn postfixes(&mut self, mut e: Expr) -> PResult<Expr> {
        while matches!(self.kind(), Tok::Dot | Tok::NsDot | Tok::LBrace) {
            e = self.postfix(e)?;
        }
        Ok(e)
    }

    fn postfix(&mut self, prefix: Expr) -> PResult<Expr> {
        let loc = self.at();
        match self.kind() {
            Tok::Dot | Tok::NsDot => {
                let null_safe = self.bump().kind == Tok::NsDot;
                match self.kind() {
                    Tok::DotId => {
                        let t = self.bump();
                        let name = self.text(t).to_string();
                        if self.kind() == Tok::LP {
                            let args = self.arguments()?;
                            Ok(Expr { loc, kind: ExprKind::Call { prefix: Box::new(prefix), name, args, null_safe } })
                        } else {
                            Ok(Expr { loc, kind: ExprKind::Dot { prefix: Box::new(prefix), index: name, null_safe } })
                        }
                    }
                    Tok::DotInteger => {
                        let t = self.bump();
                        let index = self.text(t).to_string();
                        Ok(Expr { loc, kind: ExprKind::Dot { prefix: Box::new(prefix), index, null_safe } })
                    }
                    _ => Err(self.no_viable()),
                }
            }
            Tok::LBrace => {
                self.bump();
                let index = self.expression()?;
                self.close(Tok::RBrace)?;
                Ok(Expr { loc, kind: ExprKind::Brace { prefix: Box::new(prefix), index: Box::new(index) } })
            }
            _ => Err(self.no_viable()),
        }
    }

    fn primary(&mut self) -> PResult<Expr> {
        let t = self.tok();
        let loc = t.start;
        match t.kind {
            Tok::LP => {
                self.bump();
                let e = self.expression()?;
                self.close(Tok::RP)?;
                Ok(e)
            }
            Tok::Decimal => {
                self.bump();
                Ok(Expr { loc, kind: ExprKind::Decimal(self.text(t).to_string()) })
            }
            Tok::Hex => {
                self.bump();
                Ok(Expr { loc, kind: ExprKind::Numeric { text: self.text(t)[2..].to_string(), radix: 16 } })
            }
            Tok::Integer => {
                self.bump();
                Ok(Expr { loc, kind: ExprKind::Numeric { text: self.text(t).to_string(), radix: 10 } })
            }
            Tok::Octal => {
                self.bump();
                Ok(Expr { loc, kind: ExprKind::Numeric { text: self.text(t)[1..].to_string(), radix: 8 } })
            }
            Tok::True => {
                self.bump();
                Ok(Expr { loc, kind: ExprKind::Bool(true) })
            }
            Tok::False => {
                self.bump();
                Ok(Expr { loc, kind: ExprKind::Bool(false) })
            }
            Tok::Null => {
                self.bump();
                Ok(Expr { loc, kind: ExprKind::Null })
            }
            Tok::Str => {
                self.bump();
                let raw = self.text(t);
                let inner = &raw[1..raw.len() - 1];
                let mut s = String::with_capacity(inner.len());
                let mut it = inner.chars();
                while let Some(c) = it.next() {
                    if c == '\\' {
                        if let Some(n) = it.next() {
                            s.push(n);
                        }
                    } else {
                        s.push(c);
                    }
                }
                Ok(Expr { loc, kind: ExprKind::Str(s) })
            }
            Tok::Regex => {
                self.bump();
                let raw = self.text(t);
                let last = raw.rfind('/').unwrap_or(0);
                Ok(Expr { loc, kind: ExprKind::Regex { pattern: raw[1..last].to_string(), flags: raw[last + 1..].to_string() } })
            }
            Tok::LBrace => self.list_or_map(),
            Tok::Id => {
                self.bump();
                let name = self.text(t).to_string();
                if self.kind() == Tok::LP {
                    let args = self.arguments()?;
                    Ok(Expr { loc, kind: ExprKind::CallLocal { name, args } })
                } else {
                    Ok(Expr { loc, kind: ExprKind::Symbol(name) })
                }
            }
            Tok::Dollar => {
                self.bump();
                if self.kind() != Tok::LP {
                    return Err(self.mismatch("'('"));
                }
                let args = self.arguments()?;
                Ok(Expr { loc, kind: ExprKind::CallLocal { name: "$".into(), args } })
            }
            Tok::New => {
                self.bump();
                let ty = self.type_text()?;
                if self.kind() != Tok::LP {
                    // the prediction between object and array creation fails here
                    return Err(self.no_viable());
                }
                let args = self.arguments()?;
                Ok(Expr { loc, kind: ExprKind::NewObj { ty, args } })
            }
            _ => Err(self.no_viable()),
        }
    }

    fn list_or_map(&mut self) -> PResult<Expr> {
        let loc = self.at();
        self.bump();
        if self.eat(Tok::RBrace) {
            return Ok(Expr { loc, kind: ExprKind::ListInit(Vec::new()) });
        }
        if self.kind() == Tok::Colon && self.la(1) == Tok::RBrace {
            self.bump();
            self.bump();
            return Ok(Expr { loc, kind: ExprKind::MapInit(Vec::new()) });
        }
        let first = self.expression()?;
        if self.eat(Tok::Colon) {
            let v = self.expression()?;
            let mut pairs = vec![(first, v)];
            while self.eat(Tok::Comma) {
                let k = self.expression()?;
                self.close(Tok::Colon)?;
                let v = self.expression()?;
                pairs.push((k, v));
            }
            self.close(Tok::RBrace)?;
            return Ok(Expr { loc, kind: ExprKind::MapInit(pairs) });
        }
        let mut values = vec![first];
        while self.eat(Tok::Comma) {
            values.push(self.expression()?);
        }
        self.close(Tok::RBrace)?;
        Ok(Expr { loc, kind: ExprKind::ListInit(values) })
    }

    fn arguments(&mut self) -> PResult<Vec<Expr>> {
        self.close(Tok::LP)?;
        let mut args = Vec::new();
        if self.kind() == Tok::RP {
            self.bump();
            return Ok(args);
        }
        loop {
            args.push(self.argument()?);
            if self.eat(Tok::Comma) {
                continue;
            }
            self.close(Tok::RP)?;
            return Ok(args);
        }
    }

    fn argument(&mut self) -> PResult<Expr> {
        if let Some(e) = self.try_lambda()? {
            return Ok(e);
        }
        if let Some(e) = self.try_funcref()? {
            return Ok(e);
        }
        self.expression()
    }

    fn try_lambda(&mut self) -> PResult<Option<Expr>> {
        let loc = self.at();
        let mut params: Vec<(Option<String>, String)> = Vec::new();
        if self.kind() == Tok::Id && self.la(1) == Tok::Arrow {
            let t = self.bump();
            params.push((None, self.text(t).to_string()));
            self.bump();
        } else if let Some(j) = self.scan_decltype(self.pos)
            && self.toks.get(j).map(|t| t.kind) == Some(Tok::Id)
            && self.toks.get(j + 1).map(|t| t.kind) == Some(Tok::Arrow)
        {
            // `lamtype` without parentheses may still be typed: `int x -> x + 1`
            let ty = self.decltype_text()?;
            let t = self.expect(Tok::Id)?;
            params.push((Some(ty), self.text(t).to_string()));
            self.bump();
        } else if self.kind() == Tok::LP {
            // scan `( (decltype? ID (, decltype? ID)*)? ) ->`
            let mut i = self.pos + 1;
            let mut ok = true;
            if self.toks[i].kind != Tok::RP {
                loop {
                    let j = match self.scan_decltype(i) {
                        Some(j) if self.toks.get(j).map(|t| t.kind) == Some(Tok::Id) => j + 1,
                        _ if self.toks[i].kind == Tok::Id => i + 1,
                        _ => {
                            ok = false;
                            break;
                        }
                    };
                    i = j;
                    match self.toks.get(i).map(|t| t.kind) {
                        Some(Tok::Comma) => i += 1,
                        Some(Tok::RP) => break,
                        _ => {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            if !ok || self.toks.get(i + 1).map(|t| t.kind) != Some(Tok::Arrow) {
                return Ok(None);
            }
            self.bump();
            while self.kind() != Tok::RP {
                if self.kind() == Tok::Id && matches!(self.la(1), Tok::Comma | Tok::RP) {
                    let t = self.bump();
                    params.push((None, self.text(t).to_string()));
                } else {
                    let ty = self.decltype_text()?;
                    let t = self.expect(Tok::Id)?;
                    params.push((Some(ty), self.text(t).to_string()));
                }
                self.eat(Tok::Comma);
            }
            self.bump();
            self.bump();
        } else {
            return Ok(None);
        }
        let body = if self.kind() == Tok::LBrack {
            let bl = self.at();
            let b = self.block()?.unwrap_or(Block { loc: bl, stmts: Vec::new() });
            if b.stmts.is_empty() {
                self.empty_lambda = true;
            }
            b
        } else {
            let e = self.expression()?;
            Block { loc, stmts: vec![Stmt { loc, kind: StmtKind::Return(Some(e)) }] }
        };
        Ok(Some(Expr { loc, kind: ExprKind::Lambda { params, body } }))
    }

    fn try_funcref(&mut self) -> PResult<Option<Expr>> {
        let loc = self.at();
        if self.kind() == Tok::This && self.la(1) == Tok::Ref {
            self.bump();
            self.bump();
            let t = self.expect(Tok::Id)?;
            return Ok(Some(Expr { loc, kind: ExprKind::FuncRef { symbol: "this".into(), method: self.text(t).to_string() } }));
        }
        let Some(end) = self.scan_decltype(self.pos) else { return Ok(None) };
        if self.toks.get(end).map(|t| t.kind) != Some(Tok::Ref) {
            return Ok(None);
        }
        let has_dims = self.toks[end - 1].kind == Tok::RBrace;
        let symbol = self.decltype_text()?;
        self.bump(); // ::
        if self.kind() == Tok::New {
            self.bump();
            if has_dims {
                return Ok(Some(Expr { loc, kind: ExprKind::NewArrayFuncRef { ty: symbol } }));
            }
            return Ok(Some(Expr { loc, kind: ExprKind::FuncRef { symbol, method: "new".into() } }));
        }
        let t = self.expect(Tok::Id)?;
        Ok(Some(Expr { loc, kind: ExprKind::FuncRef { symbol, method: self.text(t).to_string() } }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(src: &str) -> (u32, String) {
        let e = parse(src).unwrap_err();
        (e.offset, e.message)
    }

    #[test]
    fn statement_level_errors_match_elasticsearch() {
        assert_eq!(err("int x = 1 int y = 2"), (10, "unexpected token ['int'] was expecting one of [{<EOF>, ';'}].".into()));
        assert_eq!(err("return (1"), (9, "unexpected end of script.".into()));
        assert_eq!(err("return 1)"), (8, "unexpected token [')'] was expecting one of [{<EOF>, ';'}].".into()));
        assert_eq!(err("x."), (2, "unexpected end of script.".into()));
        assert_eq!(err("if (true) { return 1 "), (21, "unexpected end of script.".into()));
        assert_eq!(err("return [1,2"), (11, "unexpected end of script.".into()));
        assert_eq!(err("foo("), (4, "unexpected end of script.".into()));
        assert_eq!(err("int x = ;"), (8, "invalid sequence of tokens near [';'].".into()));
        assert_eq!(err("return 1 +"), (10, "unexpected end of script.".into()));
        assert_eq!(err("return a b"), (9, "unexpected token ['b'] was expecting one of [{<EOF>, ';'}].".into()));
        assert_eq!(err("for (int i = 0; i < 3; i++"), (26, "unexpected token ['<EOF>'] was expecting one of [')'].".into()));
        assert_eq!(err("int[] a = new int[]; return a"), (19, "invalid sequence of tokens near [';'].".into()));
        assert_eq!(err("return new"), (10, "unexpected end of script.".into()));
        assert_eq!(err("{"), (0, "invalid sequence of tokens near ['{'].".into()));
        assert_eq!(err("}"), (0, "invalid sequence of tokens near ['}'].".into()));
        assert_eq!(err(";"), (0, "invalid sequence of tokens near [';'].".into()));
        assert_eq!(err("return 1;;"), (9, "unexpected token [';'] was expecting one of [<EOF>].".into()));
        assert_eq!(err("return (int)"), (12, "unexpected end of script.".into()));
    }

    #[test]
    fn parses_functions_lambdas_and_casts() {
        let s = parse("def f() { return 1 } return f()").unwrap();
        assert_eq!(s.functions.len(), 1);
        assert_eq!(s.stmts.len(), 1);
        let s = parse("List l = [1]; return (l)[0]").unwrap();
        match &s.stmts[1].kind {
            StmtKind::Return(Some(Expr { kind: ExprKind::Brace { .. }, .. })) => {}
            other => panic!("{other:?}"),
        }
        let s = parse("return (List) x").unwrap();
        assert!(matches!(&s.stmts[0].kind, StmtKind::Return(Some(Expr { kind: ExprKind::Explicit { .. }, .. }))));
        let s = parse("l.sort((a, b) -> a - b); l.forEach(x -> x); l.stream().map(String::valueOf)").unwrap();
        assert_eq!(s.stmts.len(), 3);
        let s = parse("return new int[] {1, 2}[0]").unwrap();
        assert!(matches!(&s.stmts[0].kind, StmtKind::Return(Some(Expr { kind: ExprKind::Brace { .. }, .. }))));
    }

    #[test]
    fn locations_follow_rule_starts() {
        let s = parse("List l = new ArrayList(); l.get(3)").unwrap();
        match &s.stmts[1].kind {
            StmtKind::Expr(e) => {
                assert_eq!(e.loc, 27);
                assert_eq!(s.stmts[1].loc, 26);
            }
            other => panic!("{other:?}"),
        }
        let s = parse("int x = 1.5").unwrap();
        match &s.stmts[0].kind {
            StmtKind::Decl(d) => {
                assert_eq!(d[0].loc, 4);
                assert_eq!(d[0].value.as_ref().unwrap().loc, 8);
            }
            other => panic!("{other:?}"),
        }
    }
}
