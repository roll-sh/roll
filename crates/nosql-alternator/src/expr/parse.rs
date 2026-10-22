//! Lexer and parser for the DynamoDB expression languages.
//!
//! ScyllaDB parses expressions with an ANTLR3 grammar and reports the first error as
//! `syntax error at char N` (N is the offending token's column; end of input is column 0) or,
//! for a character no token can start with, a bare `syntax error`. A few semantic checks run
//! inside the grammar's actions, so they are reported as parse failures as soon as the
//! construct is complete (`Single value must be a function`, a repeated `SET` clause, a list
//! index beyond `i32`). The parser here is a hand-written LL(1) equivalent that fails at the
//! same token.
//!
//! The whole input is tokenised before parsing, as ANTLR3's token stream does, so a lexical
//! error anywhere wins over a syntax error.

use super::ast::*;
use super::resolve::MAX_PATH_DEPTH;

/// Why parsing failed; the caller formats `Failed parsing <Kind> '<text>': <reason>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseFailure {
    /// `syntax error at char N`, or a bare `syntax error` (lexer) when `None`.
    Syntax(Option<usize>),
    /// A check made by a grammar action.
    Semantic(String),
}

impl ParseFailure {
    pub fn reason(&self) -> String {
        match self {
            ParseFailure::Syntax(Some(c)) => format!("syntax error at char {c}"),
            ParseFailure::Syntax(None) => "syntax error".into(),
            ParseFailure::Semantic(s) => s.clone(),
        }
    }
}

type PResult<T> = Result<T, ParseFailure>;

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Name(String),
    NameRef(String),
    ValueRef(String),
    Int(String),
    Kw(Kw),
    Punct(&'static str),
    Eof,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kw {
    And,
    Or,
    Not,
    Between,
    In,
    Set,
    Remove,
    Add,
    Delete,
}

struct Token {
    tok: Tok,
    /// Column of the token's first character (0 for end of input).
    col: usize,
}

fn keyword(word: &str) -> Option<Kw> {
    Some(match word.to_ascii_uppercase().as_str() {
        "AND" => Kw::And,
        "OR" => Kw::Or,
        "NOT" => Kw::Not,
        "BETWEEN" => Kw::Between,
        "IN" => Kw::In,
        "SET" => Kw::Set,
        "REMOVE" => Kw::Remove,
        "ADD" => Kw::Add,
        "DELETE" => Kw::Delete,
        _ => return None,
    })
}

fn lex(src: &str) -> PResult<Vec<Token>> {
    let b = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line_start = 0;
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    while i < b.len() {
        let c = b[i];
        let col = i - line_start;
        match c {
            b' ' | b'\t' | b'\r' => i += 1,
            b'\n' => {
                i += 1;
                line_start = i;
            }
            b'a'..=b'z' | b'A'..=b'Z' => {
                let s = i;
                while i < b.len() && word(b[i]) {
                    i += 1;
                }
                let w = &src[s..i];
                let tok = match keyword(w) {
                    Some(k) => Tok::Kw(k),
                    None => Tok::Name(w.to_string()),
                };
                out.push(Token { tok, col });
            }
            b'0'..=b'9' => {
                let s = i;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                out.push(Token {
                    tok: Tok::Int(src[s..i].to_string()),
                    col,
                });
            }
            b'#' | b':' => {
                let s = i;
                i += 1;
                while i < b.len() && word(b[i]) {
                    i += 1;
                }
                if i == s + 1 {
                    return Err(ParseFailure::Syntax(None));
                }
                let w = src[s..i].to_string();
                let tok = if c == b'#' {
                    Tok::NameRef(w)
                } else {
                    Tok::ValueRef(w)
                };
                out.push(Token { tok, col });
            }
            b'<' | b'>' | b'=' | b'(' | b')' | b'[' | b']' | b',' | b'.' | b'+' | b'-' => {
                let p = match c {
                    b'<' => "<",
                    b'>' => ">",
                    b'=' => "=",
                    b'(' => "(",
                    b')' => ")",
                    b'[' => "[",
                    b']' => "]",
                    b',' => ",",
                    b'.' => ".",
                    b'+' => "+",
                    _ => "-",
                };
                i += 1;
                out.push(Token {
                    tok: Tok::Punct(p),
                    col,
                });
            }
            _ => return Err(ParseFailure::Syntax(None)),
        }
    }
    out.push(Token {
        tok: Tok::Eof,
        col: 0,
    });
    Ok(out)
}

struct Parser {
    toks: Vec<Token>,
    pos: usize,
    /// Nesting of parentheses / NOT / function calls, bounded so input can't exhaust the stack.
    depth: usize,
}

/// Deepest nesting of parentheses, `NOT`s and function calls.
const MAX_DEPTH: usize = 400;

impl Parser {
    fn new(src: &str) -> PResult<Parser> {
        Ok(Parser {
            toks: lex(src)?,
            pos: 0,
            depth: 0,
        })
    }

    fn peek(&self) -> &Tok {
        &self.toks[self.pos.min(self.toks.len() - 1)].tok
    }

    fn peek2(&self) -> &Tok {
        &self.toks[(self.pos + 1).min(self.toks.len() - 1)].tok
    }

    fn col(&self) -> usize {
        self.toks[self.pos.min(self.toks.len() - 1)].col
    }

    fn fail<T>(&self) -> PResult<T> {
        Err(ParseFailure::Syntax(Some(self.col())))
    }

    fn bump(&mut self) {
        if self.pos < self.toks.len() - 1 {
            self.pos += 1;
        }
    }

    fn is_punct(&self, p: &str) -> bool {
        matches!(self.peek(), Tok::Punct(q) if *q == p)
    }

    fn is_kw(&self, k: Kw) -> bool {
        matches!(self.peek(), Tok::Kw(q) if *q == k)
    }

    fn expect_punct(&mut self, p: &str) -> PResult<()> {
        if self.is_punct(p) {
            self.bump();
            Ok(())
        } else {
            self.fail()
        }
    }

    fn expect_eof(&self) -> PResult<()> {
        if matches!(self.peek(), Tok::Eof) {
            Ok(())
        } else {
            self.fail()
        }
    }

    fn name_component(&mut self) -> PResult<Name> {
        let n = match self.peek() {
            Tok::Name(n) => Name::Plain(n.clone()),
            Tok::NameRef(n) => Name::Ref(n.clone()),
            _ => return self.fail(),
        };
        self.bump();
        Ok(n)
    }

    fn path(&mut self) -> PResult<Path> {
        let root = self.name_component()?;
        let mut ops = Vec::new();
        loop {
            if self.is_punct(".") {
                self.bump();
                ops.push(PathOp::Field(self.name_component()?));
            } else if self.is_punct("[") {
                self.bump();
                let Tok::Int(digits) = self.peek().clone() else {
                    return self.fail();
                };
                self.bump();
                self.expect_punct("]")?;
                let index: i32 = digits.parse().map_err(|_| {
                    ParseFailure::Semantic("list index out of integer range".into())
                })?;
                ops.push(PathOp::Index(index as u32));
            } else {
                if ops.len() + 1 > MAX_PATH_DEPTH {
                    return Err(ParseFailure::Semantic(
                        "Document path exceeded 32 nesting levels".into(),
                    ));
                }
                return Ok(Path { root, ops });
            }
        }
    }

    /// `path | :value | name(args)`.
    fn operand(&mut self) -> PResult<Operand> {
        match self.peek().clone() {
            Tok::ValueRef(v) => {
                self.bump();
                Ok(Operand::Value(v))
            }
            Tok::Name(n) if matches!(self.peek2(), Tok::Punct("(")) => {
                self.enter()?;
                self.bump();
                self.bump();
                let mut args = vec![self.operand()?];
                while self.is_punct(",") {
                    self.bump();
                    args.push(self.operand()?);
                }
                self.expect_punct(")")?;
                self.depth -= 1;
                Ok(Operand::Call(Call { name: n, args }))
            }
            Tok::Name(_) | Tok::NameRef(_) => Ok(Operand::Path(self.path()?)),
            _ => self.fail(),
        }
    }

    /// Counts one level of nesting (a parenthesis, `NOT` or function call) at the current token.
    fn enter(&mut self) -> PResult<()> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(ParseFailure::Semantic(format!(
                "expression nested too deeply at char {}",
                self.col()
            )));
        }
        Ok(())
    }

    /// `condition := term (OR term)*`, `term := factor (AND factor)*`,
    /// `factor := NOT* (primitive | '(' condition ')')`, parsed with an explicit stack so
    /// nesting costs no native stack.
    fn condition(&mut self) -> PResult<Cond> {
        struct Group {
            ors: Vec<Cond>,
            ands: Vec<Cond>,
            /// `NOT`s in front of this parenthesised group.
            nots: usize,
        }
        fn fold(mut v: Vec<Cond>, f: fn(Vec<Cond>) -> Cond) -> Cond {
            if v.len() == 1 { v.remove(0) } else { f(v) }
        }
        // Same-operator groups are flattened, `a AND (b AND (c ...))` included, so only
        // alternating parentheses nest.
        fn push_and(v: &mut Vec<Cond>, c: Cond) {
            match c {
                Cond::And(inner) => v.extend(inner),
                c => v.push(c),
            }
        }
        fn push_or(v: &mut Vec<Cond>, c: Cond) {
            match c {
                Cond::Or(inner) => v.extend(inner),
                c => v.push(c),
            }
        }
        fn close(g: &mut Group) -> Cond {
            let ands = std::mem::take(&mut g.ands);
            push_or(&mut g.ors, fold(ands, Cond::And));
            fold(std::mem::take(&mut g.ors), Cond::Or)
        }
        fn wrap(c: Cond, nots: usize) -> Cond {
            match nots {
                0 => c,
                n if n % 2 == 1 => Cond::Not(Box::new(c)),
                _ => Cond::Not(Box::new(Cond::Not(Box::new(c)))),
            }
        }
        let mut stack: Vec<Group> = Vec::new();
        let mut cur = Group {
            ors: Vec::new(),
            ands: Vec::new(),
            nots: 0,
        };
        'factor: loop {
            let mut nots = 0;
            while self.is_kw(Kw::Not) {
                self.enter()?;
                self.bump();
                nots += 1;
            }
            if self.is_punct("(") {
                self.enter()?;
                self.bump();
                stack.push(std::mem::replace(
                    &mut cur,
                    Group {
                        ors: Vec::new(),
                        ands: Vec::new(),
                        nots,
                    },
                ));
                continue;
            }
            let mut c = wrap(self.primitive()?, nots);
            self.depth -= nots;
            loop {
                push_and(&mut cur.ands, c);
                if self.is_kw(Kw::And) {
                    self.bump();
                    continue 'factor;
                }
                if self.is_kw(Kw::Or) {
                    self.bump();
                    let ands = std::mem::take(&mut cur.ands);
                    push_or(&mut cur.ors, fold(ands, Cond::And));
                    continue 'factor;
                }
                let group = close(&mut cur);
                let Some(parent) = stack.pop() else {
                    return Ok(group);
                };
                self.expect_punct(")")?;
                self.depth -= 1 + cur.nots;
                c = wrap(group, cur.nots);
                cur = parent;
            }
        }
    }

    fn primitive(&mut self) -> PResult<Cond> {
        let start = self.col();
        let first = self.operand()?;
        // The lexer has one-character tokens: `<>`, `<=` and `>=` are two tokens (so `< =` is
        // `<=`, as in ScyllaDB's grammar).
        let second = |p: &Parser, want: &str| matches!(p.peek2(), Tok::Punct(q) if *q == want);
        let cmp = match self.peek() {
            Tok::Punct("=") => Some((CmpOp::Eq, 1)),
            Tok::Punct("<") if second(self, ">") => Some((CmpOp::Ne, 2)),
            Tok::Punct("<") if second(self, "=") => Some((CmpOp::Le, 2)),
            Tok::Punct("<") => Some((CmpOp::Lt, 1)),
            Tok::Punct(">") if second(self, "=") => Some((CmpOp::Ge, 2)),
            Tok::Punct(">") => Some((CmpOp::Gt, 1)),
            _ => None,
        };
        if let Some((op, n)) = cmp {
            // A lone `<` / `>` must be followed by something that starts an operand, or the
            // operator itself is the error (ANTLR decides between `<`, `<=` and `<>` there).
            let at = self.col();
            if n == 1
                && matches!(op, CmpOp::Lt | CmpOp::Gt)
                && !matches!(
                    self.peek2(),
                    Tok::Name(_) | Tok::NameRef(_) | Tok::ValueRef(_)
                )
            {
                return Err(ParseFailure::Syntax(Some(at)));
            }
            for _ in 0..n {
                self.bump();
            }
            let second = self.operand()?;
            return Ok(Cond::Cmp(first, op, second));
        }
        if self.is_kw(Kw::Between) {
            self.bump();
            let lo = self.operand()?;
            if !self.is_kw(Kw::And) {
                return self.fail();
            }
            self.bump();
            let hi = self.operand()?;
            return Ok(Cond::Between(first, lo, hi));
        }
        if self.is_kw(Kw::In) {
            self.bump();
            self.expect_punct("(")?;
            let mut list = vec![self.operand()?];
            while self.is_punct(",") {
                self.bump();
                list.push(self.operand()?);
            }
            self.expect_punct(")")?;
            return Ok(Cond::In(first, list));
        }
        // ANTLR predicts the single-value alternative only when the next token can follow a
        // value somewhere in the grammar; otherwise the decision fails at the primitive's start.
        let follows = matches!(
            self.peek(),
            Tok::Eof | Tok::Kw(Kw::And) | Tok::Kw(Kw::Or) | Tok::Punct(")" | "," | "+" | "-")
        );
        if !follows {
            return Err(ParseFailure::Syntax(Some(start)));
        }
        match first {
            Operand::Call(c) => Ok(Cond::Call(c)),
            _ => Err(ParseFailure::Semantic(
                "Single value must be a function".into(),
            )),
        }
    }

    fn set_value(&mut self) -> PResult<Operand> {
        self.operand()
    }

    fn update(&mut self) -> PResult<Update> {
        let mut u = Update::default();
        let mut seen = [false; 4];
        loop {
            let (k, slot) = match self.peek() {
                Tok::Kw(Kw::Set) => (Kw::Set, 0),
                Tok::Kw(Kw::Remove) => (Kw::Remove, 1),
                Tok::Kw(Kw::Add) => (Kw::Add, 2),
                Tok::Kw(Kw::Delete) => (Kw::Delete, 3),
                Tok::Eof if seen.iter().any(|s| *s) => return Ok(u),
                _ => return self.fail(),
            };
            self.bump();
            loop {
                let path = self.path()?;
                match k {
                    Kw::Set => {
                        self.expect_punct("=")?;
                        let a = self.set_value()?;
                        let rhs = if self.is_punct("+") {
                            self.bump();
                            SetRhs::Plus(a, self.set_value()?)
                        } else if self.is_punct("-") {
                            self.bump();
                            SetRhs::Minus(a, self.set_value()?)
                        } else {
                            SetRhs::Value(a)
                        };
                        u.actions.push(Action::Set(path, rhs));
                    }
                    Kw::Remove => u.actions.push(Action::Remove(path)),
                    _ => {
                        let Tok::ValueRef(v) = self.peek().clone() else {
                            return self.fail();
                        };
                        self.bump();
                        u.actions.push(if k == Kw::Add {
                            Action::Add(path, v)
                        } else {
                            Action::Delete(path, v)
                        });
                    }
                }
                if self.is_punct(",") {
                    self.bump();
                } else {
                    break;
                }
            }
            if seen[slot] {
                return Err(ParseFailure::Semantic(
                    "Each of SET, REMOVE, ADD, DELETE may only appear once in UpdateExpression"
                        .into(),
                ));
            }
            seen[slot] = true;
        }
    }
}

/// Parses a ConditionExpression, FilterExpression or KeyConditionExpression.
pub fn parse_condition(src: &str) -> Result<Cond, ParseFailure> {
    let mut p = Parser::new(src)?;
    let c = p.condition()?;
    p.expect_eof()?;
    Ok(c)
}

/// Parses an UpdateExpression.
pub fn parse_update(src: &str) -> Result<Update, ParseFailure> {
    let mut p = Parser::new(src)?;
    p.update()
}

/// Parses a ProjectionExpression.
pub fn parse_projection(src: &str) -> Result<Vec<Path>, ParseFailure> {
    let mut p = Parser::new(src)?;
    let mut out = vec![p.path()?];
    while p.is_punct(",") {
        p.bump();
        out.push(p.path()?);
    }
    p.expect_eof()?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cerr(s: &str) -> String {
        parse_condition(s).unwrap_err().reason()
    }

    fn uerr(s: &str) -> String {
        parse_update(s).unwrap_err().reason()
    }

    #[test]
    fn condition_errors_match_scylla() {
        // Captured from ScyllaDB 2026.3.3.
        let cases = [
            ("a = ", "syntax error at char 0"),
            ("a = = :v", "syntax error at char 4"),
            ("= :v", "syntax error at char 0"),
            ("a ==:v", "syntax error at char 3"),
            ("a = :v)", "syntax error at char 6"),
            ("(a = :v", "syntax error at char 0"),
            ("a IN ()", "syntax error at char 6"),
            ("a = :v AND", "syntax error at char 0"),
            ("a.b[x] = :v", "syntax error at char 4"),
            ("a[-1] = :v", "syntax error at char 2"),
            ("a < :v < :v", "syntax error at char 7"),
            ("attribute_exists(a", "syntax error at char 0"),
            ("a = :v xyz", "syntax error at char 7"),
            ("#", "syntax error"),
            (":", "syntax error"),
            ("a = :v,", "syntax error at char 6"),
            ("NOT", "syntax error at char 0"),
            ("a BETWEEN :v", "syntax error at char 0"),
            ("a.1 = :v", "syntax error at char 2"),
            ("a[99999999999] = :v", "list index out of integer range"),
            ("a = 'x'", "syntax error"),
            ("1a = :v", "syntax error at char 0"),
            ("_a = :v", "syntax error"),
            ("and = :v", "syntax error at char 0"),
            ("a = :v OR", "syntax error at char 0"),
            ("a != :v", "syntax error"),
            ("a IN (:v,)", "syntax error at char 9"),
            (":v", "Single value must be a function"),
            ("a", "Single value must be a function"),
            ("a.b", "Single value must be a function"),
            ("é = :v", "syntax error"),
            ("a-b = :v", "Single value must be a function"),
            ("a = :v - :v", "syntax error at char 7"),
            ("a = :v + :v", "syntax error at char 7"),
        ];
        for (src, want) in cases {
            assert_eq!(cerr(src), want, "{src}");
        }
        for ok in [
            "a1 = :v",
            "a = :v and a = :v",
            "a=:v",
            "a.#n = :v",
            "a[1][2].b = :v",
            "a [ 1 ] = :v",
            "a . b = :v",
            "SIZE(a) = :v",
            "Attribute_Exists(a)",
            "a <> :v",
            "a < > :v",
            "a < = :v",
            "a = :v AND (a = :v OR a = :v)",
            "NOT NOT a = :v",
            "a in (:v)",
            "size(a) BETWEEN :v AND :v",
            ":v = :v",
            "begins_with(a)",
            "contains(a, :v, :v)",
        ] {
            assert!(parse_condition(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn update_errors_match_scylla() {
        let cases = [
            (
                "SET a = :v SET b = :v",
                "Each of SET, REMOVE, ADD, DELETE may only appear once in UpdateExpression",
            ),
            ("SET a = :v + :v + :v", "syntax error at char 16"),
            ("ADD a b", "syntax error at char 6"),
            ("SET a :v", "syntax error at char 6"),
            ("SET a = :v,", "syntax error at char 0"),
            ("SET", "syntax error at char 0"),
            ("", "syntax error at char 0"),
            ("SET a = (:v)", "syntax error at char 8"),
            ("REMOVE :v", "syntax error at char 7"),
        ];
        for (src, want) in cases {
            assert_eq!(uerr(src), want, "{src}");
        }
        for ok in [
            "SET a = :v",
            "set a = :v, b = :v remove c",
            "SET a = if_not_exists(b, :v) + :v",
            "ADD a :v, b :v",
            "REMOVE a, b[1], c.d",
            "SET a = foo(:v)",
            "SET a=:v REMOVE b ADD c :v DELETE d :v",
        ] {
            assert!(parse_update(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn nesting_limits_match_scylla() {
        let ok = "(".repeat(400) + "a = :v" + &")".repeat(400);
        assert!(parse_condition(&ok).is_ok());
        let deep = "(".repeat(401) + "a = :v" + &")".repeat(401);
        assert_eq!(cerr(&deep), "expression nested too deeply at char 400");
        let nots = "not ".repeat(400) + "a<b";
        assert!(parse_condition(&nots).is_ok());
        let nots = "not ".repeat(1000) + "a<b";
        assert_eq!(cerr(&nots), "expression nested too deeply at char 1600");
        let calls = "a < ".to_string() + &"size(".repeat(401) + "b" + &")".repeat(401);
        assert_eq!(cerr(&calls), "expression nested too deeply at char 2004");
        let calls = "a < ".to_string() + &"f(".repeat(1355) + "b" + &")".repeat(1355);
        assert_eq!(cerr(&calls), "expression nested too deeply at char 804");
        let flat = vec!["a<b"; 2000].join(" or ");
        assert!(parse_condition(&flat).is_ok());
        let path = "a".to_string() + &".x".repeat(31);
        assert!(parse_projection(&path).is_ok());
        let path = "a".to_string() + &".x".repeat(32);
        assert_eq!(
            parse_projection(&path).unwrap_err().reason(),
            "Document path exceeded 32 nesting levels"
        );
        assert_eq!(cerr("a NOT = :v"), "syntax error at char 0");
        assert_eq!(cerr("a :v"), "syntax error at char 0");
        assert_eq!(cerr("(a) = :t"), "Single value must be a function");
        assert_eq!(cerr("()"), "syntax error at char 1");
        assert_eq!(cerr("a = (:t)"), "syntax error at char 4");
        assert_eq!(cerr("aN BETWEEN :lo OR :hi"), "syntax error at char 15");
        assert_eq!(cerr("size() = :n"), "syntax error at char 5");
        assert_eq!(cerr("not = :v"), "syntax error at char 4");
        assert_eq!(cerr("a = :v OR OR a = :v"), "syntax error at char 10");
    }
}
