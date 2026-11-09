//! A small expression language for keys, trigger filters and event matching:
//!
//! ```text
//! event.data.user.id
//! event.data.plan == 'pro' && event.data.amount >= 100
//! async.data.order_id == event.data.order_id
//! event.data.items[0].sku + ':' + event.data.region
//! ```
//!
//! Paths read from the context object (`event`, `async`, …); a missing field is `null`.
//! Operators: `!` `-` `*` `/` `%` `+` `-` `<` `<=` `>` `>=` `==` `!=` `&&` `||`, parentheses.

use serde_json::{Number, Value};

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Lit(Value),
    Path(Vec<Seg>),
    Not(Box<Expr>),
    Neg(Box<Expr>),
    Bin(Op, Box<Expr>, Box<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Seg {
    Key(String),
    Index(i64),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Or,
    And,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Num(f64),
    Str(String),
    Ident(String),
    Sym(&'static str),
}

const SYMBOLS: [&str; 21] =
    ["==", "!=", "<=", ">=", "&&", "||", "<", ">", "!", "+", "-", "*", "/", "%", "(", ")", "[", "]", ".", ",", "="];

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.' || chars[i] == '_') {
                // `1.x` is not a number followed by a path; only digits after a dot continue it.
                if chars[i] == '.' && !chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
                    break;
                }
                i += 1;
            }
            let s: String = chars[start..i].iter().filter(|c| **c != '_').collect();
            out.push(Tok::Num(s.parse().map_err(|_| format!("bad number {s:?}"))?));
        } else if c == '\'' || c == '"' {
            let quote = c;
            i += 1;
            let mut s = String::new();
            loop {
                match chars.get(i) {
                    None => return Err("unterminated string".into()),
                    Some(&ch) if ch == quote => {
                        i += 1;
                        break;
                    }
                    Some('\\') => {
                        let esc = chars.get(i + 1).ok_or("unterminated string")?;
                        s.push(match esc {
                            'n' => '\n',
                            't' => '\t',
                            'r' => '\r',
                            other => *other,
                        });
                        i += 2;
                    }
                    Some(&ch) => {
                        s.push(ch);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Str(s));
        } else if c.is_alphabetic() || c == '_' || c == '$' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$' || chars[i] == '-') {
                i += 1;
            }
            out.push(Tok::Ident(chars[start..i].iter().collect()));
        } else {
            let rest: String = chars[i..chars.len().min(i + 2)].iter().collect();
            let sym = SYMBOLS.iter().find(|s| rest.starts_with(**s)).ok_or_else(|| format!("unexpected {c:?}"))?;
            if *sym == "=" {
                return Err("use == to compare".into());
            }
            out.push(Tok::Sym(sym));
            i += sym.len();
        }
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }
    fn eat(&mut self, sym: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Sym(s)) if *s == sym) {
            self.pos += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, sym: &str) -> Result<(), String> {
        if self.eat(sym) { Ok(()) } else { Err(format!("expected {sym:?}")) }
    }

    fn binary(&mut self, level: usize) -> Result<Expr, String> {
        const LEVELS: [&[(&str, Op)]; 6] = [
            &[("||", Op::Or)],
            &[("&&", Op::And)],
            &[("==", Op::Eq), ("!=", Op::Ne)],
            &[("<=", Op::Le), (">=", Op::Ge), ("<", Op::Lt), (">", Op::Gt)],
            &[("+", Op::Add), ("-", Op::Sub)],
            &[("*", Op::Mul), ("/", Op::Div), ("%", Op::Rem)],
        ];
        if level == LEVELS.len() {
            return self.unary();
        }
        let mut left = self.binary(level + 1)?;
        'outer: loop {
            for (sym, op) in LEVELS[level] {
                if self.eat(sym) {
                    let right = self.binary(level + 1)?;
                    left = Expr::Bin(*op, Box::new(left), Box::new(right));
                    continue 'outer;
                }
            }
            return Ok(left);
        }
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if self.eat("!") {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        if self.eat("-") {
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, String> {
        let tok = self.peek().cloned().ok_or("unexpected end")?;
        self.pos += 1;
        match tok {
            Tok::Num(n) => Ok(Expr::Lit(Number::from_f64(n).map(Value::Number).unwrap_or(Value::Null))),
            Tok::Str(s) => Ok(Expr::Lit(Value::String(s))),
            Tok::Ident(id) => match id.as_str() {
                "true" => Ok(Expr::Lit(Value::Bool(true))),
                "false" => Ok(Expr::Lit(Value::Bool(false))),
                "null" => Ok(Expr::Lit(Value::Null)),
                _ => {
                    let mut segs = vec![Seg::Key(id)];
                    loop {
                        if self.eat(".") {
                            match self.peek().cloned() {
                                Some(Tok::Ident(k)) => {
                                    self.pos += 1;
                                    segs.push(Seg::Key(k));
                                }
                                Some(Tok::Num(n)) if n.fract() == 0.0 => {
                                    self.pos += 1;
                                    segs.push(Seg::Index(n as i64));
                                }
                                _ => return Err("expected a field after '.'".into()),
                            }
                        } else if self.eat("[") {
                            let negative = self.eat("-");
                            match self.peek().cloned() {
                                Some(Tok::Num(n)) if n.fract() == 0.0 => {
                                    segs.push(Seg::Index(if negative { -(n as i64) } else { n as i64 }))
                                }
                                Some(Tok::Str(s)) => segs.push(Seg::Key(s)),
                                _ => return Err("expected an index or a quoted key in [...]".into()),
                            }
                            self.pos += 1;
                            self.expect("]")?;
                        } else {
                            break;
                        }
                    }
                    Ok(Expr::Path(segs))
                }
            },
            Tok::Sym("(") => {
                let e = self.binary(0)?;
                self.expect(")")?;
                Ok(e)
            }
            Tok::Sym(s) => Err(format!("unexpected {s:?}")),
        }
    }
}

pub fn parse(src: &str) -> Result<Expr, String> {
    let mut p = Parser { toks: lex(src)?, pos: 0 };
    if p.toks.is_empty() {
        return Err("empty expression".into());
    }
    let e = p.binary(0)?;
    if p.pos != p.toks.len() {
        return Err(format!("unexpected {:?}", p.toks[p.pos]));
    }
    Ok(e)
}

pub fn get_path<'a>(ctx: &'a Value, segs: &[Seg]) -> Option<&'a Value> {
    let mut cur = ctx;
    for seg in segs {
        cur = match (seg, cur) {
            (Seg::Key(k), Value::Object(m)) => m.get(k)?,
            (Seg::Index(i), Value::Array(a)) => {
                let idx = if *i < 0 { a.len() as i64 + i } else { *i };
                a.get(usize::try_from(idx).ok()?)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

fn num(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() < 9e15 {
        Value::Number(Number::from(f as i64))
    } else {
        Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
    }
}

fn equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

fn order(a: &Value, b: &Value) -> Option<std::cmp::Ordering> {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64()?.partial_cmp(&y.as_f64()?),
        (Value::String(x), Value::String(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

impl Expr {
    pub fn eval(&self, ctx: &Value) -> Value {
        match self {
            Expr::Lit(v) => v.clone(),
            Expr::Path(segs) => get_path(ctx, segs).cloned().unwrap_or(Value::Null),
            Expr::Not(e) => Value::Bool(!truthy(&e.eval(ctx))),
            Expr::Neg(e) => e.eval(ctx).as_f64().map(|f| num(-f)).unwrap_or(Value::Null),
            Expr::Bin(Op::And, a, b) => {
                let l = a.eval(ctx);
                if truthy(&l) { b.eval(ctx) } else { l }
            }
            Expr::Bin(Op::Or, a, b) => {
                let l = a.eval(ctx);
                if truthy(&l) { l } else { b.eval(ctx) }
            }
            Expr::Bin(op, a, b) => {
                let (l, r) = (a.eval(ctx), b.eval(ctx));
                use std::cmp::Ordering::*;
                match op {
                    Op::Eq => Value::Bool(equal(&l, &r)),
                    Op::Ne => Value::Bool(!equal(&l, &r)),
                    Op::Lt => Value::Bool(order(&l, &r) == Some(Less)),
                    Op::Le => Value::Bool(matches!(order(&l, &r), Some(Less | Equal))),
                    Op::Gt => Value::Bool(order(&l, &r) == Some(Greater)),
                    Op::Ge => Value::Bool(matches!(order(&l, &r), Some(Greater | Equal))),
                    Op::Add => match (&l, &r) {
                        (Value::Number(x), Value::Number(y)) => num(x.as_f64().unwrap_or(0.0) + y.as_f64().unwrap_or(0.0)),
                        (Value::Null, Value::Null) => Value::Null,
                        _ => Value::String(format!("{}{}", key_string(&l), key_string(&r))),
                    },
                    Op::Sub | Op::Mul | Op::Div | Op::Rem => match (l.as_f64(), r.as_f64()) {
                        (Some(x), Some(y)) => match op {
                            Op::Sub => num(x - y),
                            Op::Mul => num(x * y),
                            Op::Div if y != 0.0 => num(x / y),
                            Op::Rem if y != 0.0 => num(x % y),
                            _ => Value::Null,
                        },
                        _ => Value::Null,
                    },
                    Op::And | Op::Or => unreachable!(),
                }
            }
        }
    }

    pub fn test(&self, ctx: &Value) -> bool {
        truthy(&self.eval(ctx))
    }
}

/// How a value reads as a key: strings as themselves, everything else as JSON.
pub fn key_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Evaluate `src` against `ctx`; a bad expression is `null`.
pub fn eval_str(src: &str, ctx: &Value) -> Result<Value, String> {
    Ok(parse(src)?.eval(ctx))
}

/// A `match` path like `data.order_id` (relative to an event).
pub fn parse_match_path(src: &str) -> Result<Vec<Seg>, String> {
    let src = src.trim();
    let src = src.strip_prefix("event.").unwrap_or(src);
    match parse(src)? {
        Expr::Path(segs) => Ok(segs),
        _ => Err(format!("{src:?} is not a field path")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> Value {
        json!({
            "event": {"name": "order.created", "data": {"id": "o1", "amount": 120, "plan": "pro",
                      "items": [{"sku": "a"}, {"sku": "b"}], "tags": [], "flag": false}},
            "async": {"data": {"id": "o1", "amount": 99.5}}
        })
    }

    fn ev(s: &str) -> Value {
        parse(s).unwrap_or_else(|e| panic!("{s}: {e}")).eval(&ctx())
    }

    #[test]
    fn paths() {
        assert_eq!(ev("event.data.id"), json!("o1"));
        assert_eq!(ev("event.data.items[1].sku"), json!("b"));
        assert_eq!(ev("event.data.items.0.sku"), json!("a"));
        assert_eq!(ev("event.data.items[-1].sku"), json!("b"));
        assert_eq!(ev("event.data['plan']"), json!("pro"));
        assert_eq!(ev("event.data.missing.deep"), Value::Null);
        assert_eq!(ev("nothing"), Value::Null);
    }

    #[test]
    fn comparisons_and_logic() {
        assert_eq!(ev("event.data.amount >= 100 && event.data.plan == 'pro'"), json!(true));
        assert_eq!(ev("event.data.amount < 100 || event.data.plan != \"pro\""), json!(false));
        assert_eq!(ev("async.data.id == event.data.id"), json!(true));
        assert_eq!(ev("async.data.amount > 99"), json!(true));
        assert_eq!(ev("!event.data.flag"), json!(true));
        assert_eq!(ev("!(event.data.amount == 120)"), json!(false));
        assert_eq!(ev("event.data.amount == 120.0"), json!(true));
        assert_eq!(ev("'b' > 'a'"), json!(true));
        assert_eq!(ev("1 < 'a'"), json!(false));
        assert_eq!(ev("event.data.missing == null"), json!(true));
        assert_eq!(ev("true && false || true"), json!(true));
    }

    #[test]
    fn arithmetic() {
        assert_eq!(ev("1 + 2 * 3"), json!(7));
        assert_eq!(ev("(1 + 2) * 3"), json!(9));
        assert_eq!(ev("10 / 4"), json!(2.5));
        assert_eq!(ev("10 % 4 - -1"), json!(3));
        assert_eq!(ev("1 / 0"), Value::Null);
        assert_eq!(ev("event.data.id + ':' + event.data.amount"), json!("o1:120"));
        assert_eq!(ev("1_000 + 0.5"), json!(1000.5));
    }

    #[test]
    fn errors() {
        for bad in ["", "event.", "1 +", "(1", "a = b", "'open", "event.data[x]", "1 2", "#"] {
            assert!(parse(bad).is_err(), "{bad:?} should not parse");
        }
    }

    #[test]
    fn keys_and_match_paths() {
        assert_eq!(key_string(&json!("x")), "x");
        assert_eq!(key_string(&json!(5)), "5");
        assert_eq!(key_string(&json!({"a": 1})), "{\"a\":1}");
        assert_eq!(parse_match_path("data.id").unwrap(), vec![Seg::Key("data".into()), Seg::Key("id".into())]);
        assert_eq!(parse_match_path("event.data.id").unwrap().len(), 2);
        assert!(parse_match_path("data.id == 1").is_err());
        assert!(truthy(&json!([])) && !truthy(&json!("")) && !truthy(&json!(0)));
    }
}
