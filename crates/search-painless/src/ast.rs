//! The user tree: what the parser produces and the analyzer consumes.
//!
//! It follows Elasticsearch's `node` package one-to-one (`SIf`, `EBinary`, `ECall`, …) so the
//! semantic checks — and the order in which they fire — can be ported faithfully. Every node
//! carries the byte offset of the first token of its parse rule, which is the location
//! Elasticsearch reports in errors and runtime script stacks. Type names stay strings here
//! (`"Map.Entry"`, `"int[]"`): resolving them is the analyzer's job, with the analyzer's errors.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Mul,
    Div,
    Rem,
    Add,
    Sub,
    Find,
    Match,
    Lsh,
    Rsh,
    Ush,
    BwAnd,
    Xor,
    BwOr,
    Lt,
    Lte,
    Gt,
    Gte,
    Eq,
    Eqr,
    Ne,
    Ner,
    And,
    Or,
    Not,
    BwNot,
}

impl Op {
    /// `Operation.symbol` in Elasticsearch.
    pub fn symbol(self) -> &'static str {
        use Op::*;
        match self {
            Mul => "*",
            Div => "/",
            Rem => "%",
            Add => "+",
            Sub => "-",
            Find => "=~",
            Match => "==~",
            Lsh => "<<",
            Rsh => ">>",
            Ush => ">>>",
            BwAnd => "&",
            Xor => "^",
            BwOr => "|",
            Lt => "<",
            Lte => "<=",
            Gt => ">",
            Gte => ">=",
            Eq => "==",
            Eqr => "===",
            Ne => "!=",
            Ner => "!==",
            And => "&&",
            Or => "||",
            Not => "!",
            BwNot => "~",
        }
    }

    /// `Operation.name` in Elasticsearch.
    pub fn name(self) -> &'static str {
        use Op::*;
        match self {
            Mul => "multiplication",
            Div => "division",
            Rem => "remainder",
            Add => "addition",
            Sub => "subtraction",
            Find => "find",
            Match => "match",
            Lsh => "left shift",
            Rsh => "right shift",
            Ush => "unsigned shift",
            BwAnd => "bitwise and",
            Xor => "bitwise xor",
            BwOr => "boolean or",
            Lt => "less than",
            Lte => "less than or equals",
            Gt => "greater than",
            Gte => "greater than or equals",
            Eq => "equals",
            Eqr => "reference equals",
            Ne => "not equals",
            Ner => "reference not equals",
            And => "boolean and",
            Or => "boolean or",
            Not => "boolean not",
            BwNot => "bitwise not",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Expr {
    pub loc: u32,
    pub kind: ExprKind,
}

#[derive(Clone, Debug)]
pub enum ExprKind {
    /// Integer literal text (without the radix prefix) and radix.
    Numeric { text: String, radix: u32 },
    Decimal(String),
    Str(String),
    Bool(bool),
    Null,
    Regex { pattern: String, flags: String },
    Symbol(String),
    ListInit(Vec<Expr>),
    MapInit(Vec<(Expr, Expr)>),
    /// `new T[a][b]` (`init == false`, values are dimensions) or `new T[] {…}`.
    NewArray { ty: String, values: Vec<Expr>, init: bool },
    NewObj { ty: String, args: Vec<Expr> },
    CallLocal { name: String, args: Vec<Expr> },
    Call { prefix: Box<Expr>, name: String, args: Vec<Expr>, null_safe: bool },
    Dot { prefix: Box<Expr>, index: String, null_safe: bool },
    Brace { prefix: Box<Expr>, index: Box<Expr> },
    /// Assignment, compound assignment and `++`/`--` (`post` marks the postfix forms).
    Assign { lhs: Box<Expr>, rhs: Box<Expr>, op: Option<Op>, post: bool },
    Unary { op: Op, child: Box<Expr> },
    Binary { op: Op, left: Box<Expr>, right: Box<Expr> },
    Comp { op: Op, left: Box<Expr>, right: Box<Expr> },
    BoolOp { op: Op, left: Box<Expr>, right: Box<Expr> },
    Instanceof { expr: Box<Expr>, ty: String },
    Conditional { cond: Box<Expr>, t: Box<Expr>, f: Box<Expr> },
    Elvis { left: Box<Expr>, right: Box<Expr> },
    Explicit { ty: String, child: Box<Expr> },
    Lambda { params: Vec<(Option<String>, String)>, body: Block },
    FuncRef { symbol: String, method: String },
    NewArrayFuncRef { ty: String },
}

/// A block; like every node it records where it starts, whether or not analysis reads it.
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Block {
    pub loc: u32,
    pub stmts: Vec<Stmt>,
}

#[derive(Clone, Debug)]
pub struct Decl {
    pub loc: u32,
    pub ty: String,
    pub name: String,
    pub value: Option<Expr>,
}

#[derive(Clone, Debug)]
pub struct Catch {
    pub loc: u32,
    pub ty: String,
    pub name: String,
    pub block: Option<Block>,
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub enum Init {
    Decl(u32, Vec<Decl>),
    Expr(Expr),
}

#[derive(Clone, Debug)]
pub struct Stmt {
    pub loc: u32,
    pub kind: StmtKind,
}

#[derive(Clone, Debug)]
pub enum StmtKind {
    If { cond: Expr, then: Option<Block> },
    IfElse { cond: Expr, then: Option<Block>, els: Option<Block> },
    While { cond: Expr, body: Option<Block> },
    Do { cond: Expr, body: Option<Block> },
    For { init: Option<Init>, cond: Option<Expr>, after: Option<Expr>, body: Option<Block> },
    Each { ty: String, name: String, iterable: Expr, body: Option<Block> },
    Decl(Vec<Decl>),
    Continue,
    Break,
    Return(Option<Expr>),
    Throw(Expr),
    Expr(Expr),
    Try { block: Option<Block>, catches: Vec<Catch> },
}

#[derive(Clone, Debug)]
pub struct Function {
    pub loc: u32,
    pub ret: String,
    pub name: String,
    pub params: Vec<(String, String)>,
    pub body: Block,
}

#[derive(Clone, Debug)]
pub struct Source {
    pub functions: Vec<Function>,
    pub stmts: Vec<Stmt>,
    /// Offset of the end-of-input token.
    pub end: u32,
}
