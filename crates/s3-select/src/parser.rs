//! The SQL parser: `SELECT … FROM S3Object… [WHERE …] [LIMIT n] [;]`.
//!
//! Precedence, loosest first: OR; AND; NOT; `=` `!=` `<>` and `IS [NOT] NULL|MISSING|TRUE|FALSE`
//! (left to right); `<` `<=` `>` `>=`; `[NOT] LIKE`, `[NOT] BETWEEN`, `[NOT] IN`; `||`; `+` `-`;
//! `*` `/` `%`; unary `-` `+`.

use crate::ast::*;
use crate::error::{Result, SelectError};
use crate::lexer::{Tok, Token, is_reserved, lex, location};
use crate::ts::DatePart;
use crate::value::Value;

/// AWS's limit on the expression's length.
pub const MAX_EXPRESSION_BYTES: usize = 256 * 1024;
/// Nesting limit (parentheses, function calls) so the parser's recursion stays bounded.
pub const MAX_NESTING: usize = 32;
/// Limit on the depth of the expression tree (nesting plus chained operators), so evaluating and
/// dropping it stays well within a thread's stack.
pub const MAX_DEPTH: usize = 200;

pub(crate) fn parse(src: &str) -> Result<Select> {
    if src.len() > MAX_EXPRESSION_BYTES {
        return Err(SelectError::new("ExpressionTooLong", "The SQL expression is too long: the maximum byte-length is 256 KB."));
    }
    let toks = lex(src)?;
    let mut p = Parser { src, toks, i: 0, depth: 0, ops: 0 };
    let q = p.query()?;
    Ok(q)
}

struct Parser<'a> {
    src: &'a str,
    toks: Vec<Token>,
    i: usize,
    depth: usize,
    /// An upper bound on the depth of the tree being built.
    ops: usize,
}

impl Parser<'_> {
    fn peek(&self) -> &Tok {
        &self.toks[self.i].tok
    }

    fn peek_at(&self, n: usize) -> &Tok {
        &self.toks[(self.i + n).min(self.toks.len() - 1)].tok
    }

    fn bump(&mut self) -> Tok {
        let t = self.toks[self.i].tok.clone();
        if self.i + 1 < self.toks.len() {
            self.i += 1;
        }
        t
    }

    fn err(&self, code: &'static str, what: impl AsRef<str>) -> SelectError {
        let t = &self.toks[self.i];
        SelectError::new(code, format!("{} {}.", what.as_ref(), location(self.src, t.pos)))
    }

    fn describe(&self) -> String {
        match self.peek() {
            Tok::Eof => "end of expression".into(),
            Tok::Word(w) => format!("'{w}'"),
            Tok::Quoted(w) => format!("\"{w}\""),
            Tok::Str(s) => format!("'{s}'"),
            Tok::Int(i) => i.to_string(),
            Tok::Float(f) => f.to_string(),
            t => format!("{t:?}"),
        }
    }

    fn unexpected(&self) -> SelectError {
        match self.peek() {
            Tok::Word(w) if is_reserved(w) => self.err("ParseUnExpectedKeyword", format!("Unexpected keyword {}", self.describe())),
            _ => self.err("ParseUnexpectedToken", format!("Unexpected token {}", self.describe())),
        }
    }

    fn is_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Tok::Word(w) if w.eq_ignore_ascii_case(kw))
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if self.is_kw(kw) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_kw(&mut self, kw: &str) -> Result<()> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(self.err("ParseExpectedKeyword", format!("Expected {} but found {}", kw.to_uppercase(), self.describe())))
        }
    }

    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: &Tok, what: &str) -> Result<()> {
        if self.eat(t) {
            Ok(())
        } else {
            Err(self.err("ParseExpectedTokenType", format!("Expected {what} but found {}", self.describe())))
        }
    }

    fn expect_rparen(&mut self) -> Result<()> {
        if self.eat(&Tok::RParen) {
            Ok(())
        } else {
            Err(self.err("ParseExpectedRightParenBuiltinFunctionCall", format!("Expected ')' but found {}", self.describe())))
        }
    }

    fn too_complex() -> SelectError {
        SelectError::new("MaxOperatorsExceeded", "Failed to parse SQL expression, try reducing complexity: it is nested too deeply.")
    }

    fn enter(&mut self) -> Result<()> {
        self.depth += 1;
        self.ops += 1;
        if self.depth > MAX_NESTING || self.ops > MAX_DEPTH {
            return Err(Self::too_complex());
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
        self.ops -= 1;
    }

    /// One more operator in a chain (`a + b + c` nests to the left).
    fn chain(&mut self) -> Result<()> {
        self.ops += 1;
        if self.ops > MAX_DEPTH {
            return Err(Self::too_complex());
        }
        Ok(())
    }

    fn query(&mut self) -> Result<Select> {
        if !self.eat_kw("select") {
            return Err(if matches!(self.peek(), Tok::Word(_)) {
                self.err("ParseUnsupportedSyntax", "Only SELECT is supported, found ".to_string() + &self.describe())
            } else {
                self.err("ParseExpectedKeyword", format!("Expected SELECT but found {}", self.describe()))
            });
        }
        if self.is_kw("distinct") {
            return Err(self.err("ParseUnsupportedSyntax", "DISTINCT is not supported"));
        }
        let items = self.select_list()?;
        if !self.eat_kw("from") {
            return Err(self.err("ParseSelectMissingFrom", format!("Expected FROM after the SELECT list but found {}", self.describe())));
        }
        let from = self.from()?;
        let filter = if self.eat_kw("where") { Some(self.expr()?) } else { None };
        let limit = if self.eat_kw("limit") {
            match self.bump() {
                Tok::Int(n) if n >= 0 => Some(n as u64),
                Tok::Minus if matches!(self.peek(), Tok::Int(_)) => {
                    return Err(SelectError::new("EvaluatorNegativeLimit", "LIMIT must not be negative."));
                }
                _ => {
                    self.i -= 1;
                    return Err(self.err("ParseExpectedNumber", format!("Expected a number after LIMIT but found {}", self.describe())));
                }
            }
        } else {
            None
        };
        self.eat(&Tok::Semi);
        if *self.peek() != Tok::Eof {
            for (kw, code) in [("group", "ParseUnsupportedLiteralsGroupBy"), ("order", "ParseUnsupportedSyntax"), ("having", "ParseUnsupportedSyntax")] {
                if self.is_kw(kw) {
                    return Err(self.err(code, format!("{} is not supported", kw.to_uppercase())));
                }
            }
            return Err(self.unexpected());
        }
        Ok(Select { items, from, filter, limit })
    }

    fn select_list(&mut self) -> Result<SelectList> {
        if self.is_kw("from") {
            return Err(self.err("ParseEmptySelect", "The SELECT list is empty"));
        }
        let is_star = |p: &Parser| {
            *p.peek() == Tok::Star
                || (matches!(p.peek(), Tok::Word(_) | Tok::Quoted(_)) && *p.peek_at(1) == Tok::Dot && *p.peek_at(2) == Tok::Star)
        };
        let mut items = Vec::new();
        let mut star = false;
        loop {
            if is_star(self) {
                if *self.peek() != Tok::Star {
                    self.bump();
                    self.bump();
                }
                self.bump();
                star = true;
            } else {
                let expr = self.expr()?;
                let alias = if self.eat_kw("as") {
                    Some(self.name().ok_or_else(|| self.err("ParseExpectedIdentForAlias", "Expected a name after AS"))?)
                } else {
                    match self.peek() {
                        Tok::Word(w) if !is_reserved(w) => self.name(),
                        Tok::Quoted(_) => self.name(),
                        _ => None,
                    }
                };
                items.push(SelectItem { expr, alias });
            }
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        if star {
            if !items.is_empty() {
                return Err(SelectError::new(
                    "ParseAsteriskIsNotAloneInSelectList",
                    "Other expressions are not allowed in the SELECT list when * is used.",
                ));
            }
            return Ok(SelectList::Star);
        }
        Ok(SelectList::Items(items))
    }

    /// An identifier (a non-reserved word or a quoted name).
    fn name(&mut self) -> Option<Name> {
        match self.peek().clone() {
            Tok::Word(w) if !is_reserved(&w) => {
                self.bump();
                Some(Name { text: w, quoted: false })
            }
            Tok::Quoted(w) => {
                self.bump();
                Some(Name { text: w, quoted: true })
            }
            _ => None,
        }
    }

    fn from(&mut self) -> Result<From> {
        let table = match self.peek().clone() {
            Tok::Word(w) | Tok::Quoted(w) => w,
            _ => return Err(self.err("ParseExpectedTokenType", format!("Expected S3Object after FROM but found {}", self.describe()))),
        };
        // `stdin` is Ceph's name for the object; nothing else is a valid table in S3 Select.
        if !table.eq_ignore_ascii_case("s3object") && !table.eq_ignore_ascii_case("stdin") {
            return Err(self.err("ParseUnsupportedSyntax", format!("FROM must name S3Object, not {}", self.describe())));
        }
        self.bump();
        let mut from = From { wildcard: false, steps: Vec::new(), alias: None };
        if *self.peek() == Tok::LBracket {
            if *self.peek_at(1) != Tok::Star || *self.peek_at(2) != Tok::RBracket {
                return Err(self.err("InvalidKeyPath", "A path in FROM must start with S3Object[*]"));
            }
            self.i += 3;
            from.wildcard = true;
            loop {
                match self.peek().clone() {
                    Tok::Dot => {
                        self.bump();
                        match self.bump() {
                            Tok::Star => from.steps.push(FromStep::Fields),
                            Tok::Word(w) => from.steps.push(FromStep::Name(Name { text: w, quoted: false })),
                            Tok::Quoted(w) => from.steps.push(FromStep::Name(Name { text: w, quoted: true })),
                            _ => {
                                self.i -= 1;
                                return Err(self.err("ParseInvalidPathComponent", format!("Invalid path component {}", self.describe())));
                            }
                        }
                    }
                    Tok::LBracket => {
                        self.bump();
                        match self.bump() {
                            Tok::Star => from.steps.push(FromStep::Wildcard),
                            Tok::Int(n) if n >= 0 => from.steps.push(FromStep::Index(n as usize)),
                            Tok::Str(s) => from.steps.push(FromStep::Name(Name { text: s, quoted: true })),
                            _ => {
                                self.i -= 1;
                                return Err(self.err("ParseInvalidPathComponent", format!("Invalid path component {}", self.describe())));
                            }
                        }
                        self.expect(&Tok::RBracket, "']'")?;
                    }
                    _ => break,
                }
            }
        } else if *self.peek() == Tok::Dot {
            return Err(self.err("InvalidKeyPath", "A path in FROM must start with S3Object[*]"));
        }
        if self.eat_kw("as") {
            from.alias = Some(self.name().ok_or_else(|| self.err("ParseExpectedIdentForAlias", "Expected a name after AS"))?);
        } else if self.is_kw("join") || *self.peek() == Tok::Comma {
            return Err(self.err("ParseMalformedJoin", "JOIN is not supported"));
        } else {
            from.alias = self.name();
        }
        if self.is_kw("join") || *self.peek() == Tok::Comma {
            return Err(self.err("ParseMalformedJoin", "JOIN is not supported"));
        }
        Ok(from)
    }

    pub(crate) fn expr(&mut self) -> Result<Expr> {
        self.enter()?;
        let r = self.or();
        self.leave();
        r
    }

    fn or(&mut self) -> Result<Expr> {
        let base = self.ops;
        let mut l = self.and()?;
        while self.eat_kw("or") {
            self.chain()?;
            let r = self.and()?;
            l = Expr::Or(Box::new(l), Box::new(r));
        }
        self.ops = base;
        Ok(l)
    }

    fn and(&mut self) -> Result<Expr> {
        let base = self.ops;
        let mut l = self.not()?;
        while self.eat_kw("and") {
            self.chain()?;
            let r = self.not()?;
            l = Expr::And(Box::new(l), Box::new(r));
        }
        self.ops = base;
        Ok(l)
    }

    fn not(&mut self) -> Result<Expr> {
        if self.eat_kw("not") {
            self.enter()?;
            let e = self.not();
            self.leave();
            return Ok(Expr::Not(Box::new(e?)));
        }
        self.equality()
    }

    fn equality(&mut self) -> Result<Expr> {
        let base = self.ops;
        let mut l = self.relational()?;
        loop {
            let op = match self.peek() {
                Tok::Eq => CmpOp::Eq,
                Tok::Ne => CmpOp::Ne,
                Tok::Word(w) if w.eq_ignore_ascii_case("is") => {
                    self.bump();
                    let negated = self.eat_kw("not");
                    let what = match self.bump() {
                        Tok::Word(w) if w.eq_ignore_ascii_case("null") => IsWhat::Null,
                        Tok::Word(w) if w.eq_ignore_ascii_case("missing") => IsWhat::Missing,
                        Tok::Word(w) if w.eq_ignore_ascii_case("true") => IsWhat::True,
                        Tok::Word(w) if w.eq_ignore_ascii_case("false") => IsWhat::False,
                        _ => {
                            self.i -= 1;
                            return Err(self.err(
                                "ParseExpectedKeyword",
                                format!("Expected NULL or MISSING after IS but found {}", self.describe()),
                            ));
                        }
                    };
                    self.chain()?;
                    l = Expr::Is { expr: Box::new(l), what, negated };
                    continue;
                }
                _ => {
                    self.ops = base;
                    return Ok(l);
                }
            };
            self.bump();
            self.chain()?;
            let r = self.relational()?;
            l = Expr::Cmp(op, Box::new(l), Box::new(r));
        }
    }

    fn relational(&mut self) -> Result<Expr> {
        let base = self.ops;
        let mut l = self.predicate()?;
        loop {
            let op = match self.peek() {
                Tok::Lt => CmpOp::Lt,
                Tok::Le => CmpOp::Le,
                Tok::Gt => CmpOp::Gt,
                Tok::Ge => CmpOp::Ge,
                _ => {
                    self.ops = base;
                    return Ok(l);
                }
            };
            self.bump();
            self.chain()?;
            let r = self.predicate()?;
            l = Expr::Cmp(op, Box::new(l), Box::new(r));
        }
    }

    fn predicate(&mut self) -> Result<Expr> {
        let base = self.ops;
        let mut l = self.concat()?;
        loop {
            let negated = self.is_kw("not")
                && matches!(self.peek_at(1), Tok::Word(w) if ["like", "between", "in"].iter().any(|k| w.eq_ignore_ascii_case(k)));
            if negated {
                self.bump();
            }
            if self.is_kw("like") || self.is_kw("between") || self.is_kw("in") {
                self.chain()?;
            }
            if self.eat_kw("like") {
                let pattern = self.concat()?;
                let escape = if self.eat_kw("escape") { Some(Box::new(self.concat()?)) } else { None };
                l = Expr::Like { expr: Box::new(l), pattern: Box::new(pattern), escape, negated };
            } else if self.eat_kw("between") {
                let low = self.concat()?;
                self.expect_kw("and")?;
                let high = self.concat()?;
                l = Expr::Between { expr: Box::new(l), low: Box::new(low), high: Box::new(high), negated };
            } else if self.eat_kw("in") {
                if !self.eat(&Tok::LParen) {
                    return Err(self.err("ParseExpectedLeftParenValueConstructor", format!("Expected '(' after IN but found {}", self.describe())));
                }
                let mut list = vec![self.expr()?];
                while self.eat(&Tok::Comma) {
                    list.push(self.expr()?);
                }
                self.expect(&Tok::RParen, "')'")?;
                l = Expr::In { expr: Box::new(l), list, negated };
            } else {
                self.ops = base;
                return Ok(l);
            }
        }
    }

    fn concat(&mut self) -> Result<Expr> {
        let base = self.ops;
        let mut l = self.additive()?;
        while self.eat(&Tok::Concat) {
            self.chain()?;
            let r = self.additive()?;
            l = Expr::Concat(Box::new(l), Box::new(r));
        }
        self.ops = base;
        Ok(l)
    }

    fn additive(&mut self) -> Result<Expr> {
        let base = self.ops;
        let mut l = self.multiplicative()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => ArithOp::Add,
                Tok::Minus => ArithOp::Sub,
                _ => {
                    self.ops = base;
                    return Ok(l);
                }
            };
            self.bump();
            self.chain()?;
            let r = self.multiplicative()?;
            l = Expr::Arith(op, Box::new(l), Box::new(r));
        }
    }

    fn multiplicative(&mut self) -> Result<Expr> {
        let base = self.ops;
        let mut l = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => ArithOp::Mul,
                Tok::Slash => ArithOp::Div,
                Tok::Percent => ArithOp::Mod,
                _ => {
                    self.ops = base;
                    return Ok(l);
                }
            };
            self.bump();
            self.chain()?;
            let r = self.unary()?;
            l = Expr::Arith(op, Box::new(l), Box::new(r));
        }
    }

    fn unary(&mut self) -> Result<Expr> {
        match self.peek() {
            Tok::Minus | Tok::Plus => {
                let neg = self.bump() == Tok::Minus;
                self.enter()?;
                let e = self.unary();
                self.leave();
                let e = e?;
                Ok(match (neg, e) {
                    (true, Expr::Lit(Value::Int(i))) if i != i64::MIN => Expr::Lit(Value::Int(-i)),
                    (true, Expr::Lit(Value::Float(f))) => Expr::Lit(Value::Float(-f)),
                    (true, e) => Expr::Neg(Box::new(e)),
                    (false, e) => e,
                })
            }
            _ => self.postfix(),
        }
    }

    fn postfix(&mut self) -> Result<Expr> {
        let base = self.primary()?;
        if !matches!(base, Expr::Ident(_)) {
            return Ok(base);
        }
        let mut steps = Vec::new();
        loop {
            match self.peek() {
                Tok::Dot => {
                    self.bump();
                    match self.bump() {
                        // After a dot only a name can follow, so reserved words are accepted.
                        Tok::Word(w) => steps.push(Step::Name(Name { text: w, quoted: false })),
                        Tok::Quoted(w) => steps.push(Step::Name(Name { text: w, quoted: true })),
                        Tok::Star => {
                            self.i -= 1;
                            return Err(self.err("ParseInvalidContextForWildcardInSelectList", "A wildcard is not valid here"));
                        }
                        _ => {
                            self.i -= 1;
                            return Err(self.err("ParseInvalidPathComponent", format!("Invalid path component {}", self.describe())));
                        }
                    }
                }
                Tok::LBracket => {
                    self.bump();
                    match self.bump() {
                        Tok::Int(n) if n >= 0 => steps.push(Step::Index(n as usize)),
                        Tok::Str(s) => steps.push(Step::Name(Name { text: s, quoted: true })),
                        Tok::Star => {
                            self.i -= 1;
                            return Err(self.err("ParseCannotMixSqbAndWildcardInSelectList", "A [*] wildcard is not valid here"));
                        }
                        _ => {
                            self.i -= 1;
                            return Err(self.err("ParseInvalidPathComponent", format!("Invalid path component {}", self.describe())));
                        }
                    }
                    self.expect(&Tok::RBracket, "']'")?;
                }
                _ => break,
            }
        }
        Ok(if steps.is_empty() { base } else { Expr::Path(Box::new(base), steps) })
    }

    fn primary(&mut self) -> Result<Expr> {
        match self.peek().clone() {
            Tok::Int(i) => {
                self.bump();
                Ok(Expr::Lit(Value::Int(i)))
            }
            Tok::Float(f) => {
                self.bump();
                Ok(Expr::Lit(Value::Float(f)))
            }
            Tok::Str(s) => {
                self.bump();
                Ok(Expr::Lit(Value::Str(s)))
            }
            Tok::Ion(s) => {
                let t = crate::ts::Timestamp::parse(&s).ok_or_else(|| self.err("LexerInvalidIONLiteral", format!("Unsupported Ion literal `{s}`")))?;
                self.bump();
                Ok(Expr::Lit(Value::Timestamp(t)))
            }
            Tok::Quoted(w) => {
                self.bump();
                Ok(Expr::Ident(Name { text: w, quoted: true }))
            }
            Tok::LParen => {
                self.bump();
                let e = self.expr()?;
                self.expect(&Tok::RParen, "')'")?;
                Ok(e)
            }
            Tok::Word(w) => {
                let lw = w.to_ascii_lowercase();
                match lw.as_str() {
                    "true" | "false" => {
                        self.bump();
                        return Ok(Expr::Lit(Value::Bool(lw == "true")));
                    }
                    "null" => {
                        self.bump();
                        return Ok(Expr::Lit(Value::Null));
                    }
                    "missing" => {
                        self.bump();
                        return Ok(Expr::Lit(Value::Missing));
                    }
                    "case" => {
                        self.bump();
                        return self.case();
                    }
                    "cast" => {
                        self.bump();
                        return self.cast();
                    }
                    _ => {}
                }
                if *self.peek_at(1) == Tok::LParen {
                    self.bump();
                    self.bump();
                    self.enter()?;
                    let r = self.call(&lw, &w);
                    self.leave();
                    return r;
                }
                if is_reserved(&w) {
                    return Err(self.unexpected());
                }
                self.bump();
                Ok(Expr::Ident(Name { text: w, quoted: false }))
            }
            Tok::Eof | Tok::RParen | Tok::Comma | Tok::Semi => {
                Err(self.err("ParseExpectedExpression", format!("Expected an expression but found {}", self.describe())))
            }
            _ => Err(self.err("ParseExpectedExpression", format!("Expected an expression but found {}", self.describe()))),
        }
    }

    fn case(&mut self) -> Result<Expr> {
        let operand = if self.is_kw("when") { None } else { Some(Box::new(self.expr()?)) };
        let mut whens = Vec::new();
        while self.eat_kw("when") {
            let w = self.expr()?;
            self.expect_kw("then")?;
            let t = self.expr()?;
            whens.push((w, t));
        }
        if whens.is_empty() {
            return Err(self.err("ParseExpectedWhenClause", format!("Expected WHEN but found {}", self.describe())));
        }
        let otherwise = if self.eat_kw("else") { Some(Box::new(self.expr()?)) } else { None };
        self.expect_kw("end")?;
        Ok(Expr::Case { operand, whens, otherwise })
    }

    fn cast(&mut self) -> Result<Expr> {
        if !self.eat(&Tok::LParen) {
            return Err(self.err("ParseExpectedLeftParenAfterCast", format!("Expected '(' after CAST but found {}", self.describe())));
        }
        let e = self.expr()?;
        if !self.eat_kw("as") {
            return Err(self.err("ParseCastArity", format!("Expected AS in CAST but found {}", self.describe())));
        }
        let ty = self.type_name()?;
        self.expect_rparen()?;
        Ok(Expr::Cast(Box::new(e), ty))
    }

    fn type_name(&mut self) -> Result<CastType> {
        let w = match self.peek() {
            Tok::Word(w) => w.to_ascii_lowercase(),
            _ => return Err(self.err("ParseExpectedTypeName", format!("Expected a type name but found {}", self.describe()))),
        };
        let ty = match w.as_str() {
            "int" | "integer" | "bigint" | "smallint" => CastType::Int,
            "float" | "real" | "double" => CastType::Float,
            "decimal" | "numeric" | "dec" => CastType::Decimal,
            "string" | "varchar" | "char" | "character" | "text" => CastType::String,
            "bool" | "boolean" => CastType::Bool,
            "timestamp" => CastType::Timestamp,
            _ => return Err(self.err("ParseExpectedTypeName", format!("Unsupported type {}", self.describe()))),
        };
        self.bump();
        if ty == CastType::Float && w == "double" {
            self.eat_kw("precision");
        }
        if matches!(ty, CastType::Decimal | CastType::String) && self.eat(&Tok::LParen) {
            // DECIMAL(p[, s]) / VARCHAR(n): S3 Select ignores precision and scale.
            loop {
                match self.bump() {
                    Tok::Int(_) => {}
                    _ => {
                        self.i -= 1;
                        return Err(self.err("ParseInvalidTypeParam", format!("Invalid type parameter {}", self.describe())));
                    }
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(&Tok::RParen, "')'")?;
        }
        Ok(ty)
    }

    fn date_part(&mut self) -> Result<DatePart> {
        let p = match self.peek() {
            Tok::Word(w) => DatePart::parse(w),
            _ => None,
        };
        match p {
            Some(p) => {
                self.bump();
                Ok(p)
            }
            None => Err(self.err("ParseExpectedDatePart", format!("Expected a date part but found {}", self.describe()))),
        }
    }

    fn args(&mut self) -> Result<Vec<Expr>> {
        let mut args = Vec::new();
        if self.eat(&Tok::RParen) {
            return Ok(args);
        }
        loop {
            args.push(self.expr()?);
            if self.eat(&Tok::Comma) {
                continue;
            }
            if *self.peek() != Tok::RParen {
                return Err(self.err("ParseExpectedArgumentDelimiter", format!("Expected ',' or ')' but found {}", self.describe())));
            }
            self.bump();
            return Ok(args);
        }
    }

    /// A function call; `(` is consumed.
    fn call(&mut self, lw: &str, w: &str) -> Result<Expr> {
        let arity = |args: &Vec<Expr>, ok: &dyn Fn(usize) -> bool, f: &str| {
            if ok(args.len()) {
                Ok(())
            } else {
                Err(SelectError::new("EvaluatorInvalidArguments", format!("Incorrect number of arguments in the call to {f}.")))
            }
        };
        let agg = match lw {
            "count" => Some(AggFunc::Count),
            "sum" => Some(AggFunc::Sum),
            "avg" => Some(AggFunc::Avg),
            "min" => Some(AggFunc::Min),
            "max" => Some(AggFunc::Max),
            _ => None,
        };
        if let Some(f) = agg {
            if *self.peek() == Tok::Star {
                if f != AggFunc::Count {
                    return Err(self.err("ParseUnsupportedCallWithStar", "Only COUNT accepts *"));
                }
                self.bump();
                self.expect_rparen()?;
                return Ok(Expr::Agg(f, None));
            }
            if self.is_kw("distinct") {
                return Err(self.err("ParseUnsupportedSyntax", "DISTINCT is not supported"));
            }
            self.eat_kw("all");
            let mut args = self.args()?;
            if args.len() != 1 {
                return Err(SelectError::new(
                    "ParseNonUnaryAgregateFunctionCall",
                    format!("Only one argument is supported for aggregate functions; {} has {}.", w.to_uppercase(), args.len()),
                ));
            }
            return Ok(Expr::Agg(f, Some(Box::new(args.remove(0)))));
        }
        match lw {
            "substring" => {
                let s = self.expr()?;
                let mut args = vec![s];
                if self.eat_kw("from") {
                    args.push(self.expr()?);
                    if self.eat_kw("for") {
                        args.push(self.expr()?);
                    }
                    self.expect_rparen()?;
                } else if self.eat(&Tok::Comma) {
                    args.extend(self.args()?);
                } else {
                    self.expect_rparen()?;
                }
                arity(&args, &|n| n == 2 || n == 3, "SUBSTRING")?;
                Ok(Expr::Call(Func::Substring, args))
            }
            "trim" => {
                let mode = if self.eat_kw("leading") {
                    Some(TrimMode::Leading)
                } else if self.eat_kw("trailing") {
                    Some(TrimMode::Trailing)
                } else if self.eat_kw("both") {
                    Some(TrimMode::Both)
                } else {
                    None
                };
                let (chars, expr) = if mode.is_some() && self.eat_kw("from") {
                    (None, self.expr()?)
                } else {
                    let first = self.expr()?;
                    if self.eat_kw("from") {
                        (Some(Box::new(first)), self.expr()?)
                    } else if mode.is_some() {
                        return Err(self.err("ParseExpectedKeyword", format!("Expected FROM in TRIM but found {}", self.describe())));
                    } else {
                        (None, first)
                    }
                };
                self.expect_rparen()?;
                Ok(Expr::Trim { mode: mode.unwrap_or(TrimMode::Both), chars, expr: Box::new(expr) })
            }
            "extract" => {
                let part = self.date_part()?;
                self.expect_kw("from")?;
                let e = self.expr()?;
                self.expect_rparen()?;
                Ok(Expr::Extract(part, Box::new(e)))
            }
            "date_add" | "date_diff" => {
                let part = self.date_part()?;
                if matches!(part, DatePart::TimezoneHour | DatePart::TimezoneMinute) {
                    self.i -= 1;
                    return Err(self.err("ParseExpectedDatePart", format!("Expected a date part but found {}", self.describe())));
                }
                self.expect(&Tok::Comma, "','")?;
                let a = self.expr()?;
                self.expect(&Tok::Comma, "','")?;
                let b = self.expr()?;
                self.expect_rparen()?;
                Ok(if lw == "date_add" {
                    Expr::DateAdd(part, Box::new(a), Box::new(b))
                } else {
                    Expr::DateDiff(part, Box::new(a), Box::new(b))
                })
            }
            // Ceph's INT(x) and FLOAT(x) shorthands for CAST: INT and FLOAT are reserved words,
            // so these can't mean anything else in S3 Select.
            "int" | "integer" | "float" => {
                let mut args = self.args()?;
                arity(&args, &|n| n == 1, &w.to_uppercase())?;
                let ty = if lw == "float" { CastType::Float } else { CastType::Int };
                Ok(Expr::Cast(Box::new(args.remove(0)), ty))
            }
            _ => {
                let f = match lw {
                    "lower" => Func::Lower,
                    "upper" => Func::Upper,
                    "char_length" | "character_length" => Func::CharLength,
                    "coalesce" => Func::Coalesce,
                    "nullif" => Func::NullIf,
                    "to_string" => Func::ToString,
                    "to_timestamp" => Func::ToTimestamp,
                    "utcnow" => Func::UtcNow,
                    _ => {
                        return Err(SelectError::new("UnsupportedFunction", format!("Unsupported SQL function {}.", w.to_uppercase())));
                    }
                };
                let args = self.args()?;
                let ok: &dyn Fn(usize) -> bool = match f {
                    Func::Lower | Func::Upper | Func::CharLength | Func::ToTimestamp => &|n| n == 1,
                    Func::NullIf | Func::ToString => &|n| n == 2,
                    Func::Coalesce => &|n| n >= 1,
                    Func::UtcNow => &|n| n == 0,
                    Func::Substring => &|n| n == 2 || n == 3,
                };
                arity(&args, ok, f.name())?;
                Ok(Expr::Call(f, args))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Select {
        parse(s).unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    fn code(s: &str) -> &'static str {
        parse(s).map(|_| ()).unwrap_err().code
    }

    fn id(s: &str) -> Expr {
        Expr::Ident(Name { text: s.into(), quoted: false })
    }

    fn int(i: i64) -> Expr {
        Expr::Lit(Value::Int(i))
    }

    fn item(s: &str) -> Expr {
        match p(&format!("select {s} from s3object")).items {
            SelectList::Items(mut v) => v.remove(0).expr,
            _ => panic!(),
        }
    }

    #[test]
    fn select_forms() {
        let q = p("SELECT * FROM S3Object");
        assert_eq!(q.items, SelectList::Star);
        assert_eq!(q.from, From { wildcard: false, steps: vec![], alias: None });
        let q = p("select count(0) from s3object;");
        assert!(matches!(q.items, SelectList::Items(ref v) if v.len() == 1));
        let q = p("SELECT s.* FROM S3Object s LIMIT 5");
        assert_eq!(q.items, SelectList::Star);
        assert_eq!(q.from.alias, Some(Name { text: "s".into(), quoted: false }));
        assert_eq!(q.limit, Some(5));
        let q = p("SELECT s._1 AS a, s.\"x y\" b, _2 \"C\" FROM S3Object AS s WHERE s._1 > 1");
        match q.items {
            SelectList::Items(v) => {
                assert_eq!(v[0].alias.as_ref().unwrap().text, "a");
                assert_eq!(v[1].alias.as_ref().unwrap().text, "b");
                assert_eq!(v[1].expr, Expr::Path(Box::new(id("s")), vec![Step::Name(Name { text: "x y".into(), quoted: true })]));
                assert_eq!(v[2].alias, Some(Name { text: "C".into(), quoted: true }));
            }
            _ => panic!(),
        }
        assert!(q.filter.is_some());
        let q = p("select * from stdin");
        assert_eq!(q.items, SelectList::Star);
    }

    #[test]
    fn from_paths() {
        let q = p("SELECT * FROM S3Object[*].a.\"B\"[*][2]['c'].* x");
        assert!(q.from.wildcard);
        assert_eq!(
            q.from.steps,
            vec![
                FromStep::Name(Name { text: "a".into(), quoted: false }),
                FromStep::Name(Name { text: "B".into(), quoted: true }),
                FromStep::Wildcard,
                FromStep::Index(2),
                FromStep::Name(Name { text: "c".into(), quoted: true }),
                FromStep::Fields,
            ]
        );
        assert_eq!(q.from.alias.unwrap().text, "x");
        assert_eq!(code("SELECT * FROM S3Object.a"), "InvalidKeyPath");
        assert_eq!(code("SELECT * FROM S3Object[0]"), "InvalidKeyPath");
        assert_eq!(code("SELECT * FROM t"), "ParseUnsupportedSyntax");
        assert_eq!(code("SELECT * FROM S3Object a JOIN S3Object b"), "ParseMalformedJoin");
    }

    #[test]
    fn precedence() {
        // 1 + 2 * 3
        assert_eq!(
            item("1 + 2 * 3"),
            Expr::Arith(ArithOp::Add, Box::new(int(1)), Box::new(Expr::Arith(ArithOp::Mul, Box::new(int(2)), Box::new(int(3)))))
        );
        // a OR b AND NOT c
        assert_eq!(
            item("a or b and not c"),
            Expr::Or(Box::new(id("a")), Box::new(Expr::And(Box::new(id("b")), Box::new(Expr::Not(Box::new(id("c")))))))
        );
        // NOT a IS NULL is NOT (a IS NULL)
        assert_eq!(
            item("not a is null"),
            Expr::Not(Box::new(Expr::Is { expr: Box::new(id("a")), what: IsWhat::Null, negated: false }))
        );
        // a = b = true is (a = b) = true
        assert_eq!(
            item("a = b = true"),
            Expr::Cmp(
                CmpOp::Eq,
                Box::new(Expr::Cmp(CmpOp::Eq, Box::new(id("a")), Box::new(id("b")))),
                Box::new(Expr::Lit(Value::Bool(true)))
            )
        );
        // a < b = c < d
        assert_eq!(
            item("a < b = c < d"),
            Expr::Cmp(
                CmpOp::Eq,
                Box::new(Expr::Cmp(CmpOp::Lt, Box::new(id("a")), Box::new(id("b")))),
                Box::new(Expr::Cmp(CmpOp::Lt, Box::new(id("c")), Box::new(id("d"))))
            )
        );
        // 'a' || 'b' LIKE 'a%'
        assert!(matches!(item("'a' || 'b' like 'a%'"), Expr::Like { expr, .. } if matches!(*expr, Expr::Concat(..))));
        // 1 - -2, unary minus folds into literals
        assert_eq!(item("1 - -2"), Expr::Arith(ArithOp::Sub, Box::new(int(1)), Box::new(int(-2))));
        assert_eq!(item("-a"), Expr::Neg(Box::new(id("a"))));
        // x BETWEEN 1 AND 2 AND y
        assert!(matches!(item("x between 1 and 2 and y"), Expr::And(a, _) if matches!(*a, Expr::Between { .. })));
        assert!(matches!(item("x not in (1, 2)"), Expr::In { negated: true, ref list, .. } if list.len() == 2));
        assert!(matches!(item("x not like 'a' escape '!'"), Expr::Like { negated: true, escape: Some(_), .. }));
        assert!(matches!(item("x is not missing"), Expr::Is { what: IsWhat::Missing, negated: true, .. }));
        assert_eq!(item("(1 + 2) * 3"), Expr::Arith(ArithOp::Mul, Box::new(Expr::Arith(ArithOp::Add, Box::new(int(1)), Box::new(int(2)))), Box::new(int(3))));
    }

    #[test]
    fn functions() {
        assert!(matches!(item("cast(_1 as int)"), Expr::Cast(_, CastType::Int)));
        assert!(matches!(item("CAST(x AS DECIMAL(10, 2))"), Expr::Cast(_, CastType::Decimal)));
        assert!(matches!(item("int(_1)"), Expr::Cast(_, CastType::Int)));
        assert!(matches!(item("substring(x from 2 for 3)"), Expr::Call(Func::Substring, ref a) if a.len() == 3));
        assert!(matches!(item("substring(x, 2)"), Expr::Call(Func::Substring, ref a) if a.len() == 2));
        assert!(matches!(item("trim(x)"), Expr::Trim { mode: TrimMode::Both, chars: None, .. }));
        assert!(matches!(item("trim(leading from x)"), Expr::Trim { mode: TrimMode::Leading, chars: None, .. }));
        assert!(matches!(item("trim(trailing 'ab' from x)"), Expr::Trim { mode: TrimMode::Trailing, chars: Some(_), .. }));
        assert!(matches!(item("trim('ab' from x)"), Expr::Trim { mode: TrimMode::Both, chars: Some(_), .. }));
        assert!(matches!(item("extract(timezone_hour from x)"), Expr::Extract(DatePart::TimezoneHour, _)));
        assert!(matches!(item("date_add(day, 1, x)"), Expr::DateAdd(DatePart::Day, _, _)));
        assert!(matches!(item("date_diff(year, x, y)"), Expr::DateDiff(DatePart::Year, _, _)));
        assert!(matches!(item("count(*)"), Expr::Agg(AggFunc::Count, None)));
        assert!(matches!(item("utcnow()"), Expr::Call(Func::UtcNow, ref a) if a.is_empty()));
        assert!(matches!(item("case when a then 1 else 2 end"), Expr::Case { operand: None, .. }));
        assert!(matches!(item("case a when 1 then 'x' when 2 then 'y' end"), Expr::Case { operand: Some(_), ref whens, otherwise: None } if whens.len() == 2));
        assert!(matches!(item("`2010-01-01T`"), Expr::Lit(Value::Timestamp(_))));
        assert!(matches!(item("s.a[1].b"), Expr::Path(_, ref s) if s.len() == 3));
        assert!(matches!(item("s.\"CAST\""), Expr::Path(..)));
    }

    #[test]
    fn errors() {
        assert_eq!(code("select"), "ParseExpectedExpression");
        assert_eq!(code("select from s3object"), "ParseEmptySelect");
        assert_eq!(code("select a"), "ParseSelectMissingFrom");
        assert_eq!(code("select a from s3object where"), "ParseExpectedExpression");
        assert_eq!(code("select a from s3object extra junk"), "ParseUnexpectedToken");
        assert_eq!(code("select a from s3object where a = 1 order by a"), "ParseUnsupportedSyntax");
        assert_eq!(code("select a from s3object group by a"), "ParseUnsupportedLiteralsGroupBy");
        assert_eq!(code("select *, a from s3object"), "ParseAsteriskIsNotAloneInSelectList");
        assert_eq!(code("select cast 1 from s3object"), "ParseExpectedLeftParenAfterCast");
        assert_eq!(code("select cast(1 as blob) from s3object"), "ParseExpectedTypeName");
        assert_eq!(code("select case end from s3object"), "ParseUnExpectedKeyword");
        assert_eq!(code("select case 1 end from s3object"), "ParseExpectedWhenClause");
        assert_eq!(code("select sum(*) from s3object"), "ParseUnsupportedCallWithStar");
        assert_eq!(code("select sum(a, b) from s3object"), "ParseNonUnaryAgregateFunctionCall");
        assert_eq!(code("select foo(a) from s3object"), "UnsupportedFunction");
        assert_eq!(code("select lower(a, b) from s3object"), "EvaluatorInvalidArguments");
        assert_eq!(code("select extract(week from a) from s3object"), "ParseExpectedDatePart");
        assert_eq!(code("select a from s3object limit -1"), "EvaluatorNegativeLimit");
        assert_eq!(code("select a from s3object limit x"), "ParseExpectedNumber");
        assert_eq!(code("select s.CAST, date from s3object s"), "ParseUnExpectedKeyword");
        assert_eq!(code("select a from s3object where a like"), "ParseExpectedExpression");
        assert_eq!(code("delete from s3object"), "ParseUnsupportedSyntax");
        assert_eq!(code("select a from s3object where a in 1"), "ParseExpectedLeftParenValueConstructor");
        assert_eq!(code("select (1 from s3object"), "ParseExpectedTokenType");
        assert_eq!(code("select lower(a b) from s3object"), "ParseExpectedArgumentDelimiter");
        assert_eq!(code("select distinct a from s3object"), "ParseUnsupportedSyntax");
        assert_eq!(code("select a as from s3object"), "ParseExpectedIdentForAlias");
        assert_eq!(code("select a is 3 from s3object"), "ParseExpectedKeyword");
        let deep = format!("select {}1{} from s3object", "(".repeat(300), ")".repeat(300));
        assert_eq!(code(&deep), "MaxOperatorsExceeded");
        let chain = format!("select 1{} from s3object", " + 1".repeat(MAX_DEPTH + 1));
        assert_eq!(code(&chain), "MaxOperatorsExceeded");
        p(&format!("select 1{} from s3object", " + 1".repeat(MAX_DEPTH - 10)));
        p(&format!("select a from s3object where a in ({})", vec!["1"; 5000].join(",")));
        let long = format!("select '{}' from s3object", "x".repeat(MAX_EXPRESSION_BYTES));
        assert_eq!(code(&long), "ExpressionTooLong");
        // A trailing ';' is fine, two are not.
        p("select a from s3object;");
        assert_eq!(code("select a from s3object;;"), "ParseUnexpectedToken");
    }
}
