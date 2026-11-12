//! The parsed (and, after binding, resolved) SQL.

use crate::ts::DatePart;
use crate::value::Value;

/// A name as written: unquoted names match case-insensitively, quoted ones exactly.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Name {
    pub text: String,
    pub quoted: bool,
}

impl Name {
    pub fn matches(&self, other: &str) -> bool {
        if self.quoted { self.text == other } else { self.text.eq_ignore_ascii_case(other) }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Select {
    pub items: SelectList,
    pub from: From,
    pub filter: Option<Expr>,
    pub limit: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SelectList {
    Star,
    Items(Vec<SelectItem>),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SelectItem {
    pub expr: Expr,
    pub alias: Option<Name>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct From {
    /// `S3Object[*]…`: the path starts with the root wildcard.
    pub wildcard: bool,
    pub steps: Vec<FromStep>,
    pub alias: Option<Name>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum FromStep {
    Name(Name),
    Index(usize),
    /// `[*]`
    Wildcard,
    /// `.*`
    Fields,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Step {
    Name(Name),
    Index(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IsWhat {
    Null,
    Missing,
    True,
    False,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CastType {
    Int,
    Float,
    Decimal,
    String,
    Bool,
    Timestamp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TrimMode {
    Leading,
    Trailing,
    Both,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Func {
    Lower,
    Upper,
    CharLength,
    Substring,
    Coalesce,
    NullIf,
    ToString,
    ToTimestamp,
    UtcNow,
}

impl Func {
    pub fn name(self) -> &'static str {
        match self {
            Func::Lower => "LOWER",
            Func::Upper => "UPPER",
            Func::CharLength => "CHAR_LENGTH",
            Func::Substring => "SUBSTRING",
            Func::Coalesce => "COALESCE",
            Func::NullIf => "NULLIF",
            Func::ToString => "TO_STRING",
            Func::ToTimestamp => "TO_TIMESTAMP",
            Func::UtcNow => "UTCNOW",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AggFunc {
    Count,
    Sum,
    Avg,
    Min,
    Max,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Expr {
    Lit(Value),
    /// A bare name (a column, alias or JSON attribute); resolved by binding.
    Ident(Name),
    /// `base.name`, `base[0]`, …
    Path(Box<Expr>, Vec<Step>),
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Arith(ArithOp, Box<Expr>, Box<Expr>),
    Cmp(CmpOp, Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Or(Box<Expr>, Box<Expr>),
    Concat(Box<Expr>, Box<Expr>),
    Like { expr: Box<Expr>, pattern: Box<Expr>, escape: Option<Box<Expr>>, negated: bool },
    Between { expr: Box<Expr>, low: Box<Expr>, high: Box<Expr>, negated: bool },
    In { expr: Box<Expr>, list: Vec<Expr>, negated: bool },
    Is { expr: Box<Expr>, what: IsWhat, negated: bool },
    Case { operand: Option<Box<Expr>>, whens: Vec<(Expr, Expr)>, otherwise: Option<Box<Expr>> },
    Cast(Box<Expr>, CastType),
    Call(Func, Vec<Expr>),
    Trim { mode: TrimMode, chars: Option<Box<Expr>>, expr: Box<Expr> },
    Extract(DatePart, Box<Expr>),
    DateAdd(DatePart, Box<Expr>, Box<Expr>),
    DateDiff(DatePart, Box<Expr>, Box<Expr>),
    /// `None` is COUNT(*).
    Agg(AggFunc, Option<Box<Expr>>),

    // Resolved by binding:
    /// A CSV column (0-based).
    Col(usize),
    /// The JSON row.
    Row,
    /// JSON `_1`: the row's `_1` attribute if it has one, else the row.
    RowOr1,
    /// An aggregate's slot.
    AggRef(usize),
}

impl Expr {
    /// Visit this expression and its children (pre-order).
    pub fn walk(&self, f: &mut dyn FnMut(&Expr)) {
        f(self);
        let mut go = |e: &Expr| e.walk(f);
        match self {
            Expr::Lit(_) | Expr::Ident(_) | Expr::Col(_) | Expr::Row | Expr::RowOr1 | Expr::AggRef(_) => {}
            Expr::Path(b, _) | Expr::Neg(b) | Expr::Not(b) | Expr::Cast(b, _) | Expr::Extract(_, b) => go(b),
            Expr::Arith(_, a, b)
            | Expr::Cmp(_, a, b)
            | Expr::And(a, b)
            | Expr::Or(a, b)
            | Expr::Concat(a, b)
            | Expr::DateAdd(_, a, b)
            | Expr::DateDiff(_, a, b) => {
                go(a);
                go(b);
            }
            Expr::Like { expr, pattern, escape, .. } => {
                go(expr);
                go(pattern);
                if let Some(e) = escape {
                    go(e);
                }
            }
            Expr::Between { expr, low, high, .. } => {
                go(expr);
                go(low);
                go(high);
            }
            Expr::In { expr, list, .. } => {
                go(expr);
                list.iter().for_each(go);
            }
            Expr::Is { expr, .. } => go(expr),
            Expr::Case { operand, whens, otherwise } => {
                if let Some(o) = operand {
                    go(o);
                }
                for (w, t) in whens {
                    go(w);
                    go(t);
                }
                if let Some(o) = otherwise {
                    go(o);
                }
            }
            Expr::Call(_, args) => args.iter().for_each(go),
            Expr::Trim { chars, expr, .. } => {
                if let Some(c) = chars {
                    go(c);
                }
                go(expr);
            }
            Expr::Agg(_, a) => {
                if let Some(a) = a {
                    go(a);
                }
            }
        }
    }
}
