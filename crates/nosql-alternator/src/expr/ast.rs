//! Expression syntax trees, before `#name` / `:value` references are resolved.

/// A path component name: a plain attribute name or a `#ref` into ExpressionAttributeNames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Name {
    Plain(String),
    Ref(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PathOp {
    Field(Name),
    Index(u32),
}

/// A document path: `a.b[2].#c`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Path {
    pub root: Name,
    pub ops: Vec<PathOp>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    /// As written (function names are matched case-sensitively by the evaluator).
    pub name: String,
    pub args: Vec<Operand>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Operand {
    Path(Path),
    /// `:name`, including the colon.
    Value(String),
    Call(Call),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl CmpOp {
    pub fn symbol(self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Ne => "<>",
            CmpOp::Lt => "<",
            CmpOp::Le => "<=",
            CmpOp::Gt => ">",
            CmpOp::Ge => ">=",
        }
    }
}

/// A condition. `AND` / `OR` chains are n-ary and parentheses leave no node, so a long flat
/// chain doesn't nest; a run of `NOT`s keeps its parity only (at most two levels).
#[derive(Clone, Debug, PartialEq)]
pub enum Cond {
    Or(Vec<Cond>),
    And(Vec<Cond>),
    Not(Box<Cond>),
    Cmp(Operand, CmpOp, Operand),
    Between(Operand, Operand, Operand),
    In(Operand, Vec<Operand>),
    Call(Call),
}

#[derive(Clone, Debug, PartialEq)]
pub enum SetRhs {
    Value(Operand),
    Plus(Operand, Operand),
    Minus(Operand, Operand),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Set(Path, SetRhs),
    Remove(Path),
    /// `ADD path :value`
    Add(Path, String),
    /// `DELETE path :value`
    Delete(Path, String),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Update {
    /// In source order.
    pub actions: Vec<Action>,
}

/// Calls `f` on every operand of a condition, depth first, in source order.
pub fn for_each_operand<'a>(c: &'a Cond, f: &mut dyn FnMut(&'a Operand)) {
    match c {
        Cond::Or(v) | Cond::And(v) => {
            for c in v {
                for_each_operand(c, f);
            }
        }
        Cond::Not(a) => for_each_operand(a, f),
        Cond::Cmp(a, _, b) => {
            walk_operand(a, f);
            walk_operand(b, f);
        }
        Cond::Between(a, b, c) => {
            walk_operand(a, f);
            walk_operand(b, f);
            walk_operand(c, f);
        }
        Cond::In(a, list) => {
            walk_operand(a, f);
            for x in list {
                walk_operand(x, f);
            }
        }
        Cond::Call(call) => {
            for x in &call.args {
                walk_operand(x, f);
            }
        }
    }
}

/// Calls `f` on `o` and, for function calls, on each argument.
pub fn walk_operand<'a>(o: &'a Operand, f: &mut dyn FnMut(&'a Operand)) {
    f(o);
    if let Operand::Call(c) = o {
        for x in &c.args {
            walk_operand(x, f);
        }
    }
}
