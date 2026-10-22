//! The syntax tree. It records what was written, not what it means: names keep their spelling and
//! quoting, literals keep their kind and text, and types are [`RawType`]s. The engine resolves
//! names against the schema and types the terms. Dialect-specific syntax is visible in the tree
//! (for example [`Select::bypass_cache`], [`UsingClause::timeout`], [`Masking`]), so the engine
//! can tell which product a statement needs.
//!
//! Text borrows from the parsed string where it can (`Cow::Borrowed`). [`IntoStatic`] turns any
//! node into a `'static` one for caching.

use std::borrow::Cow;

/// Converts a node that borrows from the query text into one that owns its text.
pub trait IntoStatic {
    type Static;
    fn into_static(self) -> Self::Static;
}

impl IntoStatic for Cow<'_, str> {
    type Static = Cow<'static, str>;
    fn into_static(self) -> Cow<'static, str> {
        match self {
            Cow::Borrowed(s) => Cow::Owned(s.to_owned()),
            Cow::Owned(s) => Cow::Owned(s),
        }
    }
}

impl<T: IntoStatic> IntoStatic for Vec<T> {
    type Static = Vec<T::Static>;
    fn into_static(self) -> Self::Static {
        self.into_iter().map(IntoStatic::into_static).collect()
    }
}

impl<T: IntoStatic> IntoStatic for Option<T> {
    type Static = Option<T::Static>;
    fn into_static(self) -> Self::Static {
        self.map(IntoStatic::into_static)
    }
}

impl<T: IntoStatic> IntoStatic for Box<T> {
    type Static = Box<T::Static>;
    fn into_static(self) -> Self::Static {
        Box::new((*self).into_static())
    }
}

impl<A: IntoStatic, B: IntoStatic> IntoStatic for (A, B) {
    type Static = (A::Static, B::Static);
    fn into_static(self) -> Self::Static {
        (self.0.into_static(), self.1.into_static())
    }
}

macro_rules! static_copy {
    ($($t:ty),* $(,)?) => {
        $(impl IntoStatic for $t {
            type Static = $t;
            fn into_static(self) -> $t {
                self
            }
        })*
    };
}

macro_rules! static_struct {
    ($name:ident { $($f:ident),* $(,)? }) => {
        impl IntoStatic for $name<'_> {
            type Static = $name<'static>;
            fn into_static(self) -> $name<'static> {
                $name { $($f: self.$f.into_static()),* }
            }
        }
    };
}

macro_rules! static_enum {
    ($name:ident { $($v:ident $(($x:ident))?),* $(,)? }) => {
        impl IntoStatic for $name<'_> {
            type Static = $name<'static>;
            fn into_static(self) -> $name<'static> {
                match self {
                    $($name::$v $(($x))? => $name::$v $(($x.into_static()))?,)*
                }
            }
        }
    };
}

/// A text span of the query, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

static_copy!(bool, u32, i64, Span);

// ---------------------------------------------------------------------------------------------
// Names
// ---------------------------------------------------------------------------------------------

/// An identifier as written. `text` is unescaped for quoted identifiers (`"a""b"` is `a"b`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Ident<'a> {
    pub text: Cow<'a, str>,
    pub quoted: bool,
}
static_struct!(Ident { text, quoted });

impl<'a> Ident<'a> {
    pub fn unquoted(text: impl Into<Cow<'a, str>>) -> Self {
        Ident { text: text.into(), quoted: false }
    }

    pub fn quoted(text: impl Into<Cow<'a, str>>) -> Self {
        Ident { text: text.into(), quoted: true }
    }

    /// The name CQL means: unquoted identifiers are case-insensitive and fold to lower case.
    pub fn name(&self) -> Cow<'_, str> {
        if self.quoted || !self.text.bytes().any(|b| b.is_ascii_uppercase()) {
            Cow::Borrowed(&self.text)
        } else {
            Cow::Owned(self.text.to_ascii_lowercase())
        }
    }
}

/// `[keyspace.]name` for tables, views, indexes and the like.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct QualifiedName<'a> {
    pub keyspace: Option<Ident<'a>>,
    pub name: Ident<'a>,
}
static_struct!(QualifiedName { keyspace, name });

/// A function name, `[keyspace.]name`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FunctionName<'a> {
    pub keyspace: Option<Ident<'a>>,
    pub name: Ident<'a>,
    /// The name was an unreserved keyword (`map`, `filtering`, ...). Cassandra keeps such names in
    /// the case they were written, unlike other unquoted names.
    pub keyword: bool,
}
static_struct!(FunctionName { keyspace, name, keyword });

impl FunctionName<'_> {
    /// The function's name as Cassandra resolves it.
    pub fn resolved(&self) -> Cow<'_, str> {
        if self.keyword { Cow::Borrowed(&self.name.text) } else { self.name.name() }
    }
}

/// How a role, user or service level name was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NameForm {
    /// An identifier or unreserved keyword: case-insensitive.
    Ident,
    /// `"Name"`: case-sensitive.
    Quoted,
    /// `'Name'`: case-sensitive.
    String,
}
static_copy!(NameForm);

/// A role, user, identity or service level name.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoleName<'a> {
    pub text: Cow<'a, str>,
    pub form: NameForm,
}
static_struct!(RoleName { text, form });

impl RoleName<'_> {
    /// The name as stored: identifiers fold to lower case.
    pub fn name(&self) -> Cow<'_, str> {
        match self.form {
            NameForm::Ident => Cow::Owned(self.text.to_lowercase()),
            _ => Cow::Borrowed(&self.text),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------------------------

/// A native CQL type name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum NativeType {
    Ascii,
    Bigint,
    Blob,
    Boolean,
    Counter,
    Decimal,
    Double,
    Duration,
    Float,
    Inet,
    Int,
    Smallint,
    Text,
    Timestamp,
    Tinyint,
    Uuid,
    Varchar,
    Varint,
    Timeuuid,
    Date,
    Time,
}
static_copy!(NativeType);

impl NativeType {
    pub fn as_str(self) -> &'static str {
        match self {
            NativeType::Ascii => "ascii",
            NativeType::Bigint => "bigint",
            NativeType::Blob => "blob",
            NativeType::Boolean => "boolean",
            NativeType::Counter => "counter",
            NativeType::Decimal => "decimal",
            NativeType::Double => "double",
            NativeType::Duration => "duration",
            NativeType::Float => "float",
            NativeType::Inet => "inet",
            NativeType::Int => "int",
            NativeType::Smallint => "smallint",
            NativeType::Text => "text",
            NativeType::Timestamp => "timestamp",
            NativeType::Tinyint => "tinyint",
            NativeType::Uuid => "uuid",
            NativeType::Varchar => "varchar",
            NativeType::Varint => "varint",
            NativeType::Timeuuid => "timeuuid",
            NativeType::Date => "date",
            NativeType::Time => "time",
        }
    }
}

/// A type as written (`comparatorType`). The engine resolves user types and validates nesting.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RawType<'a> {
    Native(NativeType),
    List(Box<RawType<'a>>),
    Set(Box<RawType<'a>>),
    Map(Box<(RawType<'a>, RawType<'a>)>),
    Tuple(Vec<RawType<'a>>),
    /// `vector<elem, dimension>`; the dimension is kept as written.
    Vector(Box<(RawType<'a>, Cow<'a, str>)>),
    Frozen(Box<RawType<'a>>),
    /// A user-defined type, `[keyspace.]name`.
    User(QualifiedName<'a>),
    /// `'org.apache.cassandra.db.marshal.X'`: a custom type by class name.
    Custom(Cow<'a, str>),
}
static_enum!(RawType { Native(x), List(x), Set(x), Map(x), Tuple(x), Vector(x), Frozen(x), User(x), Custom(x) });

// ---------------------------------------------------------------------------------------------
// Terms
// ---------------------------------------------------------------------------------------------

/// The lexical kind of a constant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LiteralKind {
    /// `'text'` or `$$text$$`; the text is the unescaped content.
    String,
    Integer,
    /// A decimal or exponent number, or `NaN` / `Infinity` / `-Infinity` (normalised spellings,
    /// as Cassandra keeps them).
    Float,
    Boolean,
    Duration,
    Uuid,
    /// `0x...`, including the prefix.
    Hex,
    /// A list of constants as a ScyllaDB property map value (rack lists in `replication`):
    /// `text` holds the elements' texts, each followed by a NUL ([`Literal::list_items`]).
    List,
}
static_copy!(LiteralKind);

/// A constant as written.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Literal<'a> {
    pub kind: LiteralKind,
    pub text: Cow<'a, str>,
    /// For strings: written with `$$` delimiters.
    pub dollar: bool,
}
static_struct!(Literal { kind, text, dollar });

impl Literal<'_> {
    /// The elements of a [`LiteralKind::List`] value.
    pub fn list_items(&self) -> Vec<&str> {
        self.text.split_terminator('\0').collect()
    }
}

/// A bind marker: `?` or `:name`. Markers are numbered from 0 in source order.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BindMarker<'a> {
    pub index: u32,
    pub name: Option<Ident<'a>>,
}
static_struct!(BindMarker { index, name });

/// An arithmetic operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}
static_copy!(ArithOp);

impl ArithOp {
    pub fn as_str(self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
            ArithOp::Mod => "%",
        }
    }
}

/// A value expression (`term` in `Parser.g`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Term<'a> {
    Literal(Literal<'a>),
    Null,
    Bind(BindMarker<'a>),
    /// `[a, b]`; also the vector literal.
    List(Vec<Term<'a>>),
    /// `{a, b}`; `{}` is an empty set (or map: the engine decides from the receiver).
    Set(Vec<Term<'a>>),
    Map(Vec<(Term<'a>, Term<'a>)>),
    /// `{field: value, ...}`.
    Udt(Vec<(Ident<'a>, Term<'a>)>),
    Tuple(Vec<Term<'a>>),
    Function(FunctionCall<'a>),
    /// `(type) term`.
    TypeHint(Box<TypeHint<'a>>),
    /// `CAST(term AS type)`.
    Cast(Box<Cast<'a, Term<'a>>>),
    Binary(Box<BinaryOp<Term<'a>>>),
    Negate(Box<Term<'a>>),
}

/// `name(args)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FunctionCall<'a> {
    pub name: FunctionName<'a>,
    pub args: Vec<Term<'a>>,
}
static_struct!(FunctionCall { name, args });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TypeHint<'a> {
    pub ty: RawType<'a>,
    pub term: Term<'a>,
}
static_struct!(TypeHint { ty, term });

/// `CAST(arg AS type)` for terms and selectors.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Cast<'a, T> {
    pub arg: T,
    pub ty: NativeType,
    #[doc(hidden)]
    pub _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a, T> Cast<'a, T> {
    pub fn new(arg: T, ty: NativeType) -> Self {
        Cast { arg, ty, _marker: std::marker::PhantomData }
    }
}

impl<T: IntoStatic> IntoStatic for Cast<'_, T> {
    type Static = Cast<'static, T::Static>;
    fn into_static(self) -> Self::Static {
        Cast::new(self.arg.into_static(), self.ty)
    }
}

/// `left op right` for terms and selectors.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct BinaryOp<T> {
    pub op: ArithOp,
    pub left: T,
    pub right: T,
}

impl<T: IntoStatic> IntoStatic for BinaryOp<T> {
    type Static = BinaryOp<T::Static>;
    fn into_static(self) -> Self::Static {
        BinaryOp { op: self.op, left: self.left.into_static(), right: self.right.into_static() }
    }
}

static_enum!(Term {
    Literal(x),
    Null,
    Bind(x),
    List(x),
    Set(x),
    Map(x),
    Udt(x),
    Tuple(x),
    Function(x),
    TypeHint(x),
    Cast(x),
    Binary(x),
    Negate(x),
});

// ---------------------------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------------------------

/// `WRITETIME`, `MAXWRITETIME` or `TTL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MetadataKind {
    WriteTime,
    MaxWriteTime,
    Ttl,
}
static_copy!(MetadataKind);

/// A selector expression (`unaliasedSelector`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Selectable<'a> {
    Column(Ident<'a>),
    /// A constant, `NULL` or a bind marker.
    Term(Term<'a>),
    /// `COUNT(*)`.
    CountRows,
    /// `WRITETIME(c)`, `TTL(c[k])`, `MAXWRITETIME(c.f)`: the argument is a column with optional
    /// element/field selections.
    Metadata(MetadataKind, Box<Selectable<'a>>),
    Cast(Box<Cast<'a, Selectable<'a>>>),
    Function(FunctionName<'a>, Vec<Selectable<'a>>),
    /// `receiver.field`.
    Field(Box<Selectable<'a>>, Ident<'a>),
    /// `receiver[key]`.
    Element(Box<Selectable<'a>>, Term<'a>),
    /// `receiver[from..to]`, either bound optional.
    Slice(Box<SliceSelection<'a>>),
    /// `(type) selector`.
    TypeHint(RawType<'a>, Box<Selectable<'a>>),
    List(Vec<Selectable<'a>>),
    Set(Vec<Selectable<'a>>),
    /// `{k: v, ...}`: a map or a UDT literal, decided by the engine.
    MapOrUdt(Vec<(Selectable<'a>, Selectable<'a>)>),
    /// `(a)` or `(a, b, ...)`: one element is a parenthesised selector, more are a tuple.
    Tuple(Vec<Selectable<'a>>),
    Binary(Box<BinaryOp<Selectable<'a>>>),
    Negate(Box<Selectable<'a>>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SliceSelection<'a> {
    pub receiver: Selectable<'a>,
    pub from: Option<Term<'a>>,
    pub to: Option<Term<'a>>,
}
static_struct!(SliceSelection { receiver, from, to });

impl IntoStatic for Selectable<'_> {
    type Static = Selectable<'static>;
    fn into_static(self) -> Selectable<'static> {
        use Selectable as S;
        match self {
            S::Column(x) => S::Column(x.into_static()),
            S::Term(x) => S::Term(x.into_static()),
            S::CountRows => S::CountRows,
            S::Metadata(k, x) => S::Metadata(k, x.into_static()),
            S::Cast(x) => S::Cast(x.into_static()),
            S::Function(n, a) => S::Function(n.into_static(), a.into_static()),
            S::Field(r, f) => S::Field(r.into_static(), f.into_static()),
            S::Element(r, k) => S::Element(r.into_static(), k.into_static()),
            S::Slice(x) => S::Slice(x.into_static()),
            S::TypeHint(t, x) => S::TypeHint(t.into_static(), x.into_static()),
            S::List(x) => S::List(x.into_static()),
            S::Set(x) => S::Set(x.into_static()),
            S::MapOrUdt(x) => S::MapOrUdt(x.into_static()),
            S::Tuple(x) => S::Tuple(x.into_static()),
            S::Binary(x) => S::Binary(x.into_static()),
            S::Negate(x) => S::Negate(x.into_static()),
        }
    }
}

/// A selector with its optional alias.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Selector<'a> {
    pub expr: Selectable<'a>,
    pub alias: Option<Ident<'a>>,
}
static_struct!(Selector { expr, alias });

// ---------------------------------------------------------------------------------------------
// WHERE and IF
// ---------------------------------------------------------------------------------------------

/// A relation or condition operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Operator {
    Eq,
    Lt,
    Lte,
    Gt,
    Gte,
    Neq,
    In,
    /// ScyllaDB.
    NotIn,
    Contains,
    ContainsKey,
    Like,
    /// `IS NOT NULL` (the right-hand side is [`Term::Null`]).
    IsNot,
}
static_copy!(Operator);

impl Operator {
    pub fn as_str(self) -> &'static str {
        match self {
            Operator::Eq => "=",
            Operator::Lt => "<",
            Operator::Lte => "<=",
            Operator::Gt => ">",
            Operator::Gte => ">=",
            Operator::Neq => "!=",
            Operator::In => "IN",
            Operator::NotIn => "NOT IN",
            Operator::Contains => "CONTAINS",
            Operator::ContainsKey => "CONTAINS KEY",
            Operator::Like => "LIKE",
            Operator::IsNot => "IS NOT",
        }
    }
}

/// The right-hand side of a relation or condition.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Value<'a> {
    Term(Term<'a>),
    /// `IN (a, b, ...)`.
    List(Vec<Term<'a>>),
    /// `IN ?`: one marker for the whole list.
    ListMarker(BindMarker<'a>),
}
static_enum!(Value { Term(x), List(x), ListMarker(x) });

/// A `WHERE` clause element.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Relation<'a> {
    /// `col op value`, `col[key] op value`.
    Single(SingleRelation<'a>),
    /// `(a, b) op (1, 2)`, `(a, b) IN ((1, 2), ...)`. Tuple values are [`Term::Tuple`] or a bind
    /// marker.
    MultiColumn(MultiColumnRelation<'a>),
    /// `token(a, b) op term`.
    Token(TokenRelation<'a>),
    /// `expr(index, term)`: a custom index expression (Cassandra).
    Expression(CustomExpression<'a>),
    /// `f(args) op term` (ScyllaDB, e.g. `BM25(col, 'q') > 0`).
    Function(FunctionRelation<'a>),
}
static_enum!(Relation { Single(x), MultiColumn(x), Token(x), Expression(x), Function(x) });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SingleRelation<'a> {
    pub column: Ident<'a>,
    /// `col[key]`.
    pub key: Option<Term<'a>>,
    pub op: Operator,
    pub value: Value<'a>,
}
static_struct!(SingleRelation { column, key, op, value });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct MultiColumnRelation<'a> {
    pub columns: Vec<Ident<'a>>,
    pub op: Operator,
    pub value: Value<'a>,
}
static_struct!(MultiColumnRelation { columns, op, value });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct TokenRelation<'a> {
    pub columns: Vec<Ident<'a>>,
    pub op: Operator,
    pub value: Term<'a>,
}
static_struct!(TokenRelation { columns, op, value });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CustomExpression<'a> {
    pub index: QualifiedName<'a>,
    pub value: Term<'a>,
}
static_struct!(CustomExpression { index, value });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FunctionRelation<'a> {
    pub function: FunctionName<'a>,
    pub args: Vec<Selectable<'a>>,
    pub op: Operator,
    pub value: Term<'a>,
}
static_struct!(FunctionRelation { function, args, op, value });

/// Where a condition applies within a column.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ConditionTarget<'a> {
    Column,
    /// `col[element]`.
    Element(Term<'a>),
    /// `col.field`.
    Field(Ident<'a>),
}
static_enum!(ConditionTarget { Column, Element(x), Field(x) });

/// An `IF` condition: `col op value`, `col[k] op value`, `col.f op value`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Condition<'a> {
    pub column: Ident<'a>,
    pub target: ConditionTarget<'a>,
    pub op: Operator,
    pub value: Value<'a>,
}
static_struct!(Condition { column, target, op, value });

/// The `IF` clause of a modification.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub enum Conditions<'a> {
    #[default]
    None,
    Exists,
    List(Vec<Condition<'a>>),
}
static_enum!(Conditions { None, Exists, List(x) });

// ---------------------------------------------------------------------------------------------
// DML
// ---------------------------------------------------------------------------------------------

/// `USING TTL ... AND TIMESTAMP ... AND TIMEOUT ...`. Values are integer literals, durations or
/// bind markers. `order` keeps the order the parameters were written in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct UsingClause<'a> {
    pub timestamp: Option<Term<'a>>,
    pub ttl: Option<Term<'a>>,
    /// ScyllaDB's `TIMEOUT`.
    pub timeout: Option<Term<'a>>,
    /// ScyllaDB's `CONCURRENCY` (`PRUNE MATERIALIZED VIEW`).
    pub concurrency: Option<Term<'a>>,
    /// ScyllaDB's `SERVICE LEVEL name` (SELECT).
    pub service_level: Option<RoleName<'a>>,
    pub order: Vec<UsingKind>,
}
static_struct!(UsingClause { timestamp, ttl, timeout, concurrency, service_level, order });

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum UsingKind {
    Timestamp,
    Ttl,
    Timeout,
    Concurrency,
    ServiceLevel,
}
static_copy!(UsingKind);

impl UsingClause<'_> {
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn get(&self, kind: UsingKind) -> Option<&Term<'_>> {
        match kind {
            UsingKind::Timestamp => self.timestamp.as_ref(),
            UsingKind::Ttl => self.ttl.as_ref(),
            UsingKind::Timeout => self.timeout.as_ref(),
            UsingKind::Concurrency => self.concurrency.as_ref(),
            UsingKind::ServiceLevel => None,
        }
    }
}

/// `ASC` / `DESC`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Direction {
    Asc,
    Desc,
}
static_copy!(Direction);

/// An `ORDER BY` element.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct OrderBy<'a> {
    pub expr: OrderByExpr<'a>,
    /// `None` when not written (ascending).
    pub direction: Option<Direction>,
}
static_struct!(OrderBy { expr, direction });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OrderByExpr<'a> {
    Column(Ident<'a>),
    /// `col ANN OF vector`.
    Ann(Ident<'a>, Term<'a>),
    /// `f(args)` (ScyllaDB, e.g. `BM25(col, 'q')`).
    Function(FunctionName<'a>, Vec<Selectable<'a>>),
}

impl IntoStatic for OrderByExpr<'_> {
    type Static = OrderByExpr<'static>;
    fn into_static(self) -> OrderByExpr<'static> {
        match self {
            OrderByExpr::Column(c) => OrderByExpr::Column(c.into_static()),
            OrderByExpr::Ann(c, t) => OrderByExpr::Ann(c.into_static(), t.into_static()),
            OrderByExpr::Function(f, a) => OrderByExpr::Function(f.into_static(), a.into_static()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Select<'a> {
    pub json: bool,
    pub distinct: bool,
    /// Empty for `*`.
    pub selectors: Vec<Selector<'a>>,
    pub table: QualifiedName<'a>,
    pub where_clause: Vec<Relation<'a>>,
    pub group_by: Vec<Selectable<'a>>,
    pub order_by: Vec<OrderBy<'a>>,
    pub per_partition_limit: Option<Term<'a>>,
    pub limit: Option<Term<'a>>,
    pub allow_filtering: bool,
    /// ScyllaDB's `BYPASS CACHE`.
    pub bypass_cache: bool,
    /// ScyllaDB's `USING TIMEOUT`.
    pub using: UsingClause<'a>,
    /// ScyllaDB's `FROM MUTATION_FRAGMENTS(table)`: the table's raw mutation fragments.
    pub mutation_fragments: bool,
}
static_struct!(Select {
    json,
    distinct,
    selectors,
    table,
    where_clause,
    group_by,
    order_by,
    per_partition_limit,
    limit,
    allow_filtering,
    bypass_cache,
    using,
    mutation_fragments,
});

/// What `INSERT ... JSON ... DEFAULT` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JsonDefault {
    Null,
    Unset,
}
static_copy!(JsonDefault);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum InsertValues<'a> {
    /// `(cols) VALUES (terms)`.
    Values { columns: Vec<Ident<'a>>, values: Vec<Term<'a>> },
    /// `JSON 'text' | ? [DEFAULT NULL|UNSET]`: a string literal or a bind marker.
    Json { value: Term<'a>, default: Option<JsonDefault> },
}

impl IntoStatic for InsertValues<'_> {
    type Static = InsertValues<'static>;
    fn into_static(self) -> InsertValues<'static> {
        match self {
            InsertValues::Values { columns, values } => {
                InsertValues::Values { columns: columns.into_static(), values: values.into_static() }
            }
            InsertValues::Json { value, default } => InsertValues::Json { value: value.into_static(), default },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Insert<'a> {
    pub table: QualifiedName<'a>,
    pub values: InsertValues<'a>,
    pub if_not_exists: bool,
    pub using: UsingClause<'a>,
}
static_struct!(Insert { table, values, if_not_exists, using });

/// An `UPDATE ... SET` operation on one column.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Operation<'a> {
    /// `c = t`.
    Set(Term<'a>),
    /// `c[k] = t`.
    SetElement(Term<'a>, Term<'a>),
    /// `c.f = t`.
    SetField(Ident<'a>, Term<'a>),
    /// `c = c + t`, `c += t`, and `c = c -3` (adds `-3`).
    Add(Term<'a>, OpForm),
    /// `c = c - t`, `c -= t`.
    Subtract(Term<'a>, OpForm),
    /// `c = t + c`.
    Prepend(Term<'a>),
}

/// How an addition or subtraction was written.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OpForm {
    /// `c = c + t`.
    Long,
    /// `c += t`.
    Shorthand,
    /// `c = c -3`: a negative integer right after the column.
    Juxtaposed,
}
static_copy!(OpForm);

impl IntoStatic for Operation<'_> {
    type Static = Operation<'static>;
    fn into_static(self) -> Operation<'static> {
        match self {
            Operation::Set(t) => Operation::Set(t.into_static()),
            Operation::SetElement(k, t) => Operation::SetElement(k.into_static(), t.into_static()),
            Operation::SetField(f, t) => Operation::SetField(f.into_static(), t.into_static()),
            Operation::Add(t, f) => Operation::Add(t.into_static(), f),
            Operation::Subtract(t, f) => Operation::Subtract(t.into_static(), f),
            Operation::Prepend(t) => Operation::Prepend(t.into_static()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Assignment<'a> {
    pub column: Ident<'a>,
    pub op: Operation<'a>,
}
static_struct!(Assignment { column, op });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Update<'a> {
    pub table: QualifiedName<'a>,
    pub using: UsingClause<'a>,
    pub assignments: Vec<Assignment<'a>>,
    pub where_clause: Vec<Relation<'a>>,
    pub conditions: Conditions<'a>,
}
static_struct!(Update { table, using, assignments, where_clause, conditions });

/// A `DELETE` target.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Deletion<'a> {
    Column(Ident<'a>),
    Element(Ident<'a>, Term<'a>),
    Field(Ident<'a>, Ident<'a>),
}

impl IntoStatic for Deletion<'_> {
    type Static = Deletion<'static>;
    fn into_static(self) -> Deletion<'static> {
        match self {
            Deletion::Column(c) => Deletion::Column(c.into_static()),
            Deletion::Element(c, t) => Deletion::Element(c.into_static(), t.into_static()),
            Deletion::Field(c, f) => Deletion::Field(c.into_static(), f.into_static()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Delete<'a> {
    pub columns: Vec<Deletion<'a>>,
    pub table: QualifiedName<'a>,
    pub using: UsingClause<'a>,
    pub where_clause: Vec<Relation<'a>>,
    pub conditions: Conditions<'a>,
    /// ScyllaDB's `BYPASS LARGE_DATA_GUARDRAILS`.
    pub bypass_guardrails: bool,
}
static_struct!(Delete { columns, table, using, where_clause, conditions, bypass_guardrails });

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BatchKind {
    Logged,
    Unlogged,
    Counter,
}
static_copy!(BatchKind);

/// A statement inside a batch.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BatchStatement<'a> {
    Insert(Insert<'a>),
    Update(Update<'a>),
    Delete(Delete<'a>),
}
static_enum!(BatchStatement { Insert(x), Update(x), Delete(x) });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Batch<'a> {
    pub kind: BatchKind,
    pub using: UsingClause<'a>,
    pub statements: Vec<BatchStatement<'a>>,
}
static_struct!(Batch { kind, using, statements });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Truncate<'a> {
    pub table: QualifiedName<'a>,
    /// ScyllaDB's `USING TIMEOUT`.
    pub using: UsingClause<'a>,
}
static_struct!(Truncate { table, using });

/// ScyllaDB's `PRUNE MATERIALIZED VIEW`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PruneView<'a> {
    pub view: QualifiedName<'a>,
    pub where_clause: Vec<Relation<'a>>,
    pub using: UsingClause<'a>,
}
static_struct!(PruneView { view, where_clause, using });

// ---------------------------------------------------------------------------------------------
// DDL
// ---------------------------------------------------------------------------------------------

/// A property value: a constant (kept as its raw text) or a map of constants.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum PropertyValue<'a> {
    /// A constant or an unreserved keyword (`WITH x = true`, `WITH x = default`).
    Simple(Literal<'a>),
    /// `{'k': 'v', ...}`; keys and values are constants.
    Map(Vec<(Literal<'a>, Literal<'a>)>),
}
static_enum!(PropertyValue { Simple(x), Map(x) });

/// `name = value` in a `WITH` clause.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Property<'a> {
    pub name: Ident<'a>,
    pub value: PropertyValue<'a>,
}
static_struct!(Property { name, value });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CreateKeyspace<'a> {
    pub if_not_exists: bool,
    pub name: Ident<'a>,
    pub properties: Vec<Property<'a>>,
}
static_struct!(CreateKeyspace { if_not_exists, name, properties });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AlterKeyspace<'a> {
    pub if_exists: bool,
    pub name: Ident<'a>,
    pub properties: Vec<Property<'a>>,
}
static_struct!(AlterKeyspace { if_exists, name, properties });

/// `DROP KEYSPACE|TABLE|TYPE|INDEX|MATERIALIZED VIEW [IF EXISTS] name`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DropObject<'a> {
    pub if_exists: bool,
    pub name: QualifiedName<'a>,
}
static_struct!(DropObject { if_exists, name });

/// `MASKED WITH fn(args)` / `MASKED WITH DEFAULT` (Cassandra 5).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Masking<'a> {
    /// `None` for `DEFAULT` (`mask_default`).
    pub function: Option<FunctionName<'a>>,
    pub args: Vec<Term<'a>>,
}
static_struct!(Masking { function, args });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ColumnDef<'a> {
    pub name: Ident<'a>,
    pub ty: RawType<'a>,
    pub is_static: bool,
    pub mask: Option<Masking<'a>>,
    /// ScyllaDB's per-row TTL column marker (`expiration timestamp TTL`).
    pub ttl: bool,
    /// `PRIMARY KEY` written after the column.
    pub primary_key: bool,
}
static_struct!(ColumnDef { name, ty, is_static, mask, ttl, primary_key });

/// `PRIMARY KEY ((a, b), c, d)`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PrimaryKey<'a> {
    pub partition: Vec<Ident<'a>>,
    /// The partition key was parenthesised.
    pub composite: bool,
    pub clustering: Vec<Ident<'a>>,
}
static_struct!(PrimaryKey { partition, composite, clustering });

/// A `WITH` element of `CREATE TABLE` / `CREATE MATERIALIZED VIEW`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TableOption<'a> {
    Property(Property<'a>),
    CompactStorage,
    ClusteringOrder(Vec<(Ident<'a>, Direction)>),
}
static_enum!(TableOption { Property(x), CompactStorage, ClusteringOrder(x) });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CreateTable<'a> {
    pub if_not_exists: bool,
    pub name: QualifiedName<'a>,
    pub columns: Vec<ColumnDef<'a>>,
    /// Every `PRIMARY KEY (...)` element, in order (the engine rejects more than one key).
    pub primary_keys: Vec<PrimaryKey<'a>>,
    pub options: Vec<TableOption<'a>>,
}
static_struct!(CreateTable { if_not_exists, name, columns, primary_keys, options });

/// A column added by `ALTER TABLE ... ADD`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AddColumn<'a> {
    pub name: Ident<'a>,
    pub ty: RawType<'a>,
    pub is_static: bool,
    pub mask: Option<Masking<'a>>,
}
static_struct!(AddColumn { name, ty, is_static, mask });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AlterTableAction<'a> {
    /// `ALTER col TYPE t`.
    AlterType { column: Ident<'a>, ty: RawType<'a> },
    /// `ALTER [IF EXISTS] col MASKED WITH ...` / `DROP MASKED` (`mask` is `None`).
    Mask { if_exists: bool, column: Ident<'a>, mask: Option<Masking<'a>> },
    /// `ADD [IF NOT EXISTS] col type` or `ADD (col type, ...)`.
    Add { if_not_exists: bool, columns: Vec<AddColumn<'a>>, parenthesized: bool },
    /// `DROP [IF EXISTS] col` or `DROP (a, b)`, optionally `USING TIMESTAMP n` (an integer
    /// literal; ScyllaDB also takes a bind marker).
    DropObject { if_exists: bool, columns: Vec<Ident<'a>>, parenthesized: bool, timestamp: Option<Term<'a>> },
    /// `RENAME [IF EXISTS] a TO b AND c TO d`.
    Rename { if_exists: bool, renames: Vec<(Ident<'a>, Ident<'a>)> },
    DropCompactStorage,
    With(Vec<Property<'a>>),
    /// ScyllaDB's `TTL col` / `TTL NULL` (per-row TTL).
    Ttl(Option<Ident<'a>>),
}

impl IntoStatic for AlterTableAction<'_> {
    type Static = AlterTableAction<'static>;
    fn into_static(self) -> AlterTableAction<'static> {
        use AlterTableAction as A;
        match self {
            A::AlterType { column, ty } => A::AlterType { column: column.into_static(), ty: ty.into_static() },
            A::Mask { if_exists, column, mask } => {
                A::Mask { if_exists, column: column.into_static(), mask: mask.into_static() }
            }
            A::Add { if_not_exists, columns, parenthesized } => {
                A::Add { if_not_exists, columns: columns.into_static(), parenthesized }
            }
            A::DropObject { if_exists, columns, parenthesized, timestamp } => A::DropObject {
                if_exists,
                columns: columns.into_static(),
                parenthesized,
                timestamp: timestamp.into_static(),
            },
            A::Rename { if_exists, renames } => A::Rename { if_exists, renames: renames.into_static() },
            A::DropCompactStorage => A::DropCompactStorage,
            A::With(p) => A::With(p.into_static()),
            A::Ttl(c) => A::Ttl(c.into_static()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AlterTable<'a> {
    pub if_exists: bool,
    pub name: QualifiedName<'a>,
    pub action: AlterTableAction<'a>,
}
static_struct!(AlterTable { if_exists, name, action });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CreateType<'a> {
    pub if_not_exists: bool,
    pub name: QualifiedName<'a>,
    pub fields: Vec<(Ident<'a>, RawType<'a>)>,
}
static_struct!(CreateType { if_not_exists, name, fields });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum AlterTypeAction<'a> {
    AlterField { field: Ident<'a>, ty: RawType<'a> },
    Add { if_not_exists: bool, field: Ident<'a>, ty: RawType<'a> },
    Rename { if_exists: bool, renames: Vec<(Ident<'a>, Ident<'a>)> },
}

impl IntoStatic for AlterTypeAction<'_> {
    type Static = AlterTypeAction<'static>;
    fn into_static(self) -> AlterTypeAction<'static> {
        match self {
            AlterTypeAction::AlterField { field, ty } => {
                AlterTypeAction::AlterField { field: field.into_static(), ty: ty.into_static() }
            }
            AlterTypeAction::Add { if_not_exists, field, ty } => {
                AlterTypeAction::Add { if_not_exists, field: field.into_static(), ty: ty.into_static() }
            }
            AlterTypeAction::Rename { if_exists, renames } => {
                AlterTypeAction::Rename { if_exists, renames: renames.into_static() }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AlterType<'a> {
    pub if_exists: bool,
    pub name: QualifiedName<'a>,
    pub action: AlterTypeAction<'a>,
}
static_struct!(AlterType { if_exists, name, action });

/// What an index is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IndexTargetKind {
    /// `col`.
    Simple,
    /// `VALUES(col)`.
    Values,
    /// `KEYS(col)`.
    Keys,
    /// `ENTRIES(col)`.
    Entries,
    /// `FULL(col)`.
    Full,
}
static_copy!(IndexTargetKind);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum IndexTarget<'a> {
    Column(IndexTargetKind, Ident<'a>),
    /// ScyllaDB's local index partition key, `((a, b), c)`.
    PartitionKey(Vec<Ident<'a>>),
}

impl IntoStatic for IndexTarget<'_> {
    type Static = IndexTarget<'static>;
    fn into_static(self) -> IndexTarget<'static> {
        match self {
            IndexTarget::Column(k, c) => IndexTarget::Column(k, c.into_static()),
            IndexTarget::PartitionKey(c) => IndexTarget::PartitionKey(c.into_static()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CreateIndex<'a> {
    pub custom: bool,
    pub if_not_exists: bool,
    pub name: Option<Ident<'a>>,
    pub table: QualifiedName<'a>,
    pub targets: Vec<IndexTarget<'a>>,
    /// `USING 'class'`.
    pub using: Option<Cow<'a, str>>,
    pub properties: Vec<Property<'a>>,
    /// ScyllaDB's view options other than properties (`COMPACT STORAGE`, `CLUSTERING ORDER BY`),
    /// which an index refuses after parsing them.
    pub view_options: Vec<TableOption<'a>>,
}
static_struct!(CreateIndex { custom, if_not_exists, name, table, targets, using, properties, view_options });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CreateView<'a> {
    pub if_not_exists: bool,
    pub name: QualifiedName<'a>,
    /// Empty for `*`.
    pub selectors: Vec<Selector<'a>>,
    pub base: QualifiedName<'a>,
    pub where_clause: Vec<Relation<'a>>,
    pub primary_key: PrimaryKey<'a>,
    pub options: Vec<TableOption<'a>>,
}
static_struct!(CreateView { if_not_exists, name, selectors, base, where_clause, primary_key, options });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AlterView<'a> {
    pub if_exists: bool,
    pub name: QualifiedName<'a>,
    pub properties: Vec<Property<'a>>,
}
static_struct!(AlterView { if_exists, name, properties });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CreateFunction<'a> {
    pub or_replace: bool,
    pub if_not_exists: bool,
    pub name: FunctionName<'a>,
    pub args: Vec<(Ident<'a>, RawType<'a>)>,
    /// `CALLED ON NULL INPUT` (vs `RETURNS NULL ON NULL INPUT`).
    pub called_on_null_input: bool,
    pub return_type: RawType<'a>,
    /// Lower-cased, as Cassandra stores it.
    pub language: Cow<'a, str>,
    pub body: Literal<'a>,
}
static_struct!(CreateFunction {
    or_replace,
    if_not_exists,
    name,
    args,
    called_on_null_input,
    return_type,
    language,
    body,
});

/// `DROP FUNCTION|AGGREGATE [IF EXISTS] name [(types)]`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DropFunction<'a> {
    pub if_exists: bool,
    pub name: FunctionName<'a>,
    pub arg_types: Option<Vec<RawType<'a>>>,
}
static_struct!(DropFunction { if_exists, name, arg_types });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CreateAggregate<'a> {
    pub or_replace: bool,
    pub if_not_exists: bool,
    pub name: FunctionName<'a>,
    pub arg_types: Vec<RawType<'a>>,
    pub state_function: FunctionName<'a>,
    pub state_type: RawType<'a>,
    /// ScyllaDB's `REDUCEFUNC`.
    pub reduce_function: Option<FunctionName<'a>>,
    pub final_function: Option<FunctionName<'a>>,
    pub initial_condition: Option<Term<'a>>,
}
static_struct!(CreateAggregate {
    or_replace,
    if_not_exists,
    name,
    arg_types,
    state_function,
    state_type,
    reduce_function,
    final_function,
    initial_condition,
});

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CreateTrigger<'a> {
    pub if_not_exists: bool,
    pub name: Ident<'a>,
    pub table: QualifiedName<'a>,
    pub class: Cow<'a, str>,
}
static_struct!(CreateTrigger { if_not_exists, name, table, class });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DropTrigger<'a> {
    pub if_exists: bool,
    pub name: Ident<'a>,
    pub table: QualifiedName<'a>,
}
static_struct!(DropTrigger { if_exists, name, table });

// ---------------------------------------------------------------------------------------------
// DCL
// ---------------------------------------------------------------------------------------------

/// A permission name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Permission {
    Create,
    Alter,
    Drop,
    Select,
    Modify,
    Authorize,
    Describe,
    Execute,
    Unmask,
    SelectMasked,
}
static_copy!(Permission);

impl Permission {
    pub const ALL: [Permission; 10] = [
        Permission::Create,
        Permission::Alter,
        Permission::Drop,
        Permission::Select,
        Permission::Modify,
        Permission::Authorize,
        Permission::Describe,
        Permission::Execute,
        Permission::Unmask,
        Permission::SelectMasked,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Permission::Create => "CREATE",
            Permission::Alter => "ALTER",
            Permission::Drop => "DROP",
            Permission::Select => "SELECT",
            Permission::Modify => "MODIFY",
            Permission::Authorize => "AUTHORIZE",
            Permission::Describe => "DESCRIBE",
            Permission::Execute => "EXECUTE",
            Permission::Unmask => "UNMASK",
            Permission::SelectMasked => "SELECT_MASKED",
        }
    }
}

/// The permissions of a `GRANT`, `REVOKE` or `LIST`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Permissions {
    /// `ALL` or `ALL PERMISSIONS`.
    All { keyword: bool },
    /// As written, each with or without the `PERMISSION` keyword.
    List(Vec<(Permission, bool)>),
}
static_copy!(Permissions);

impl Permissions {
    /// The named permissions (every permission for `ALL`), deduplicated.
    pub fn set(&self) -> Vec<Permission> {
        let mut v: Vec<Permission> = match self {
            Permissions::All { .. } => Permission::ALL.to_vec(),
            Permissions::List(l) => l.iter().map(|(p, _)| *p).collect(),
        };
        v.sort();
        v.dedup();
        v
    }
}

/// A resource of a permission.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Resource<'a> {
    AllKeyspaces,
    Keyspace(Ident<'a>),
    /// `[TABLE] name`; `keyword` records whether `TABLE` was written.
    Table { name: QualifiedName<'a>, keyword: bool },
    /// `ALL TABLES IN KEYSPACE ks` (Cassandra 5).
    AllTablesInKeyspace(Ident<'a>),
    AllRoles,
    Role(RoleName<'a>),
    AllFunctions,
    AllFunctionsInKeyspace(Ident<'a>),
    Function { name: FunctionName<'a>, arg_types: Vec<RawType<'a>> },
    AllMbeans,
    /// `MBEAN 'name'` / `MBEANS 'pattern'`.
    Mbean { name: Cow<'a, str>, plural: bool },
}

impl IntoStatic for Resource<'_> {
    type Static = Resource<'static>;
    fn into_static(self) -> Resource<'static> {
        use Resource as R;
        match self {
            R::AllKeyspaces => R::AllKeyspaces,
            R::Keyspace(k) => R::Keyspace(k.into_static()),
            R::Table { name, keyword } => R::Table { name: name.into_static(), keyword },
            R::AllTablesInKeyspace(k) => R::AllTablesInKeyspace(k.into_static()),
            R::AllRoles => R::AllRoles,
            R::Role(r) => R::Role(r.into_static()),
            R::AllFunctions => R::AllFunctions,
            R::AllFunctionsInKeyspace(k) => R::AllFunctionsInKeyspace(k.into_static()),
            R::Function { name, arg_types } => R::Function { name: name.into_static(), arg_types: arg_types.into_static() },
            R::AllMbeans => R::AllMbeans,
            R::Mbean { name, plural } => R::Mbean { name: name.into_static(), plural },
        }
    }
}

impl Resource<'_> {
    /// The permissions that apply to this resource (`IResource.applicablePermissions` in
    /// Cassandra; ScyllaDB has no `UNMASK` / `SELECT_MASKED` and no role-level `DESCRIBE`).
    pub fn applicable_permissions(&self, dialect: crate::Dialect) -> &'static [Permission] {
        use Permission::*;
        let scylla = dialect == crate::Dialect::Scylla;
        match self {
            Resource::AllKeyspaces | Resource::Keyspace(_) | Resource::AllTablesInKeyspace(_) if scylla => {
                &[Create, Alter, Drop, Select, Modify, Authorize]
            }
            Resource::AllKeyspaces | Resource::Keyspace(_) | Resource::AllTablesInKeyspace(_) => {
                &[Create, Alter, Drop, Select, Modify, Authorize, Unmask, SelectMasked]
            }
            Resource::Table { .. } if scylla => &[Alter, Drop, Select, Modify, Authorize],
            Resource::Table { .. } => &[Alter, Drop, Select, Modify, Authorize, Unmask, SelectMasked],
            Resource::AllRoles => &[Create, Alter, Drop, Authorize, Describe],
            Resource::Role(_) if scylla => &[Alter, Drop, Authorize],
            Resource::Role(_) => &[Alter, Drop, Authorize, Describe],
            Resource::AllFunctions | Resource::AllFunctionsInKeyspace(_) => &[Create, Alter, Drop, Authorize, Execute],
            Resource::Function { .. } => &[Alter, Drop, Authorize, Execute],
            Resource::AllMbeans | Resource::Mbean { .. } => &[Authorize, Describe, Execute, Modify, Select],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PermissionsStatement<'a> {
    pub permissions: Permissions,
    pub resource: Resource<'a>,
    /// The grantee (`TO`) or revokee (`FROM`).
    pub role: RoleName<'a>,
}
static_struct!(PermissionsStatement { permissions, resource, role });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ListPermissions<'a> {
    pub permissions: Permissions,
    pub resource: Option<Resource<'a>>,
    pub of: Option<RoleName<'a>>,
    pub recursive: bool,
}
static_struct!(ListPermissions { permissions, resource, of, recursive });

/// `GRANT role TO grantee` / `REVOKE role FROM revokee`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoleGrant<'a> {
    pub role: RoleName<'a>,
    pub grantee: RoleName<'a>,
}
static_struct!(RoleGrant { role, grantee });

/// An option of `CREATE ROLE` / `ALTER ROLE`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum RoleOption<'a> {
    Password(Cow<'a, str>),
    HashedPassword(Cow<'a, str>),
    Options(Vec<(Literal<'a>, Literal<'a>)>),
    Superuser(bool),
    Login(bool),
    AccessToAllDatacenters,
    AccessToDatacenters(Vec<Cow<'a, str>>),
    AccessFromAllCidrs,
    AccessFromCidrs(Vec<Cow<'a, str>>),
}

impl IntoStatic for RoleOption<'_> {
    type Static = RoleOption<'static>;
    fn into_static(self) -> RoleOption<'static> {
        use RoleOption as O;
        match self {
            O::Password(p) => O::Password(p.into_static()),
            O::HashedPassword(p) => O::HashedPassword(p.into_static()),
            O::Options(o) => O::Options(o.into_static()),
            O::Superuser(b) => O::Superuser(b),
            O::Login(b) => O::Login(b),
            O::AccessToAllDatacenters => O::AccessToAllDatacenters,
            O::AccessToDatacenters(d) => O::AccessToDatacenters(d.into_static()),
            O::AccessFromAllCidrs => O::AccessFromAllCidrs,
            O::AccessFromCidrs(c) => O::AccessFromCidrs(c.into_static()),
        }
    }
}

/// `CREATE ROLE` (`if_exists` means `IF NOT EXISTS`) or `ALTER ROLE` (`IF EXISTS`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RoleStatement<'a> {
    pub if_exists: bool,
    pub name: RoleName<'a>,
    /// `WITH` was written.
    pub with: bool,
    pub options: Vec<RoleOption<'a>>,
}
static_struct!(RoleStatement { if_exists, name, with, options });

/// `DROP ROLE|USER [IF EXISTS] name`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DropRole<'a> {
    pub if_exists: bool,
    pub name: RoleName<'a>,
}
static_struct!(DropRole { if_exists, name });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ListRoles<'a> {
    pub of: Option<RoleName<'a>>,
    pub recursive: bool,
}
static_struct!(ListRoles { of, recursive });

/// `WITH [HASHED] PASSWORD 'p'` of the legacy user statements.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum UserPassword<'a> {
    Plain(Cow<'a, str>),
    Hashed(Cow<'a, str>),
}
static_enum!(UserPassword { Plain(x), Hashed(x) });

/// `CREATE USER` (`if_exists` means `IF NOT EXISTS`) / `ALTER USER`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct UserStatement<'a> {
    pub if_exists: bool,
    pub name: RoleName<'a>,
    pub password: Option<UserPassword<'a>>,
    /// `SUPERUSER` (`Some(true)`) or `NOSUPERUSER` (`Some(false)`).
    pub superuser: Option<bool>,
}
static_struct!(UserStatement { if_exists, name, password, superuser });

/// `ADD IDENTITY [IF NOT EXISTS] 'id' TO ROLE 'role'` (Cassandra 5).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct AddIdentity<'a> {
    pub if_not_exists: bool,
    pub identity: RoleName<'a>,
    pub role: RoleName<'a>,
}
static_struct!(AddIdentity { if_not_exists, identity, role });

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DropIdentity<'a> {
    pub if_exists: bool,
    pub identity: RoleName<'a>,
}
static_struct!(DropIdentity { if_exists, identity });

/// ScyllaDB's `CREATE|ALTER SERVICE LEVEL`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ServiceLevelStatement<'a> {
    /// `IF NOT EXISTS` for `CREATE`, `IF EXISTS` for `ALTER`.
    pub if_exists: bool,
    pub name: RoleName<'a>,
    pub properties: Vec<Property<'a>>,
}
static_struct!(ServiceLevelStatement { if_exists, name, properties });

/// ScyllaDB's `LIST ... SERVICE LEVEL(S)` forms.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ListServiceLevels<'a> {
    /// `LIST SERVICE LEVEL name`.
    One(RoleName<'a>),
    /// `LIST ALL SERVICE LEVELS`.
    All,
    /// `LIST ATTACHED SERVICE LEVEL OF role`.
    AttachedOf(RoleName<'a>),
    /// `LIST ALL ATTACHED SERVICE LEVELS`.
    AllAttached,
    /// `LIST EFFECTIVE SERVICE LEVEL OF role`.
    EffectiveOf(RoleName<'a>),
}
static_enum!(ListServiceLevels { One(x), All, AttachedOf(x), AllAttached, EffectiveOf(x) });

// ---------------------------------------------------------------------------------------------
// DESCRIBE
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DescribeTarget<'a> {
    Cluster,
    Schema { full: bool },
    Keyspaces,
    /// `[ONLY] KEYSPACE [name]`.
    Keyspace { only: bool, name: Option<Ident<'a>> },
    Tables,
    Table(QualifiedName<'a>),
    Index(QualifiedName<'a>),
    MaterializedView(QualifiedName<'a>),
    Types,
    Type(QualifiedName<'a>),
    Functions,
    Function(FunctionName<'a>),
    Aggregates,
    Aggregate(FunctionName<'a>),
    /// `DESCRIBE [ks.]name`: whatever the name refers to.
    Generic(QualifiedName<'a>),
}

impl IntoStatic for DescribeTarget<'_> {
    type Static = DescribeTarget<'static>;
    fn into_static(self) -> DescribeTarget<'static> {
        use DescribeTarget as D;
        match self {
            D::Cluster => D::Cluster,
            D::Schema { full } => D::Schema { full },
            D::Keyspaces => D::Keyspaces,
            D::Keyspace { only, name } => D::Keyspace { only, name: name.into_static() },
            D::Tables => D::Tables,
            D::Table(n) => D::Table(n.into_static()),
            D::Index(n) => D::Index(n.into_static()),
            D::MaterializedView(n) => D::MaterializedView(n.into_static()),
            D::Types => D::Types,
            D::Type(n) => D::Type(n.into_static()),
            D::Functions => D::Functions,
            D::Function(n) => D::Function(n.into_static()),
            D::Aggregates => D::Aggregates,
            D::Aggregate(n) => D::Aggregate(n.into_static()),
            D::Generic(n) => D::Generic(n.into_static()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Describe<'a> {
    /// `DESC` rather than `DESCRIBE`.
    pub short: bool,
    pub target: DescribeTarget<'a>,
    pub with_internals: bool,
    /// ScyllaDB's `WITH INTERNALS AND PASSWORDS`.
    pub with_passwords: bool,
}
static_struct!(Describe { short, target, with_internals, with_passwords });

// ---------------------------------------------------------------------------------------------
// Statements
// ---------------------------------------------------------------------------------------------

/// One statement.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StatementKind<'a> {
    Select(Box<Select<'a>>),
    Insert(Box<Insert<'a>>),
    Update(Box<Update<'a>>),
    Delete(Box<Delete<'a>>),
    Batch(Box<Batch<'a>>),
    Use(Ident<'a>),
    /// `TRUNCATE [TABLE] name`.
    Truncate(Box<Truncate<'a>>),
    CreateKeyspace(Box<CreateKeyspace<'a>>),
    AlterKeyspace(Box<AlterKeyspace<'a>>),
    DropKeyspace(Box<DropObject<'a>>),
    CreateTable(Box<CreateTable<'a>>),
    AlterTable(Box<AlterTable<'a>>),
    DropTable(Box<DropObject<'a>>),
    CreateIndex(Box<CreateIndex<'a>>),
    DropIndex(Box<DropObject<'a>>),
    CreateType(Box<CreateType<'a>>),
    AlterType(Box<AlterType<'a>>),
    DropType(Box<DropObject<'a>>),
    CreateView(Box<CreateView<'a>>),
    AlterView(Box<AlterView<'a>>),
    DropView(Box<DropObject<'a>>),
    CreateFunction(Box<CreateFunction<'a>>),
    DropFunction(Box<DropFunction<'a>>),
    CreateAggregate(Box<CreateAggregate<'a>>),
    DropAggregate(Box<DropFunction<'a>>),
    CreateTrigger(Box<CreateTrigger<'a>>),
    DropTrigger(Box<DropTrigger<'a>>),
    Grant(Box<PermissionsStatement<'a>>),
    Revoke(Box<PermissionsStatement<'a>>),
    ListPermissions(Box<ListPermissions<'a>>),
    CreateRole(Box<RoleStatement<'a>>),
    AlterRole(Box<RoleStatement<'a>>),
    DropRole(Box<DropRole<'a>>),
    ListRoles(Box<ListRoles<'a>>),
    GrantRole(Box<RoleGrant<'a>>),
    RevokeRole(Box<RoleGrant<'a>>),
    CreateUser(Box<UserStatement<'a>>),
    AlterUser(Box<UserStatement<'a>>),
    DropUser(Box<DropRole<'a>>),
    ListUsers,
    AddIdentity(Box<AddIdentity<'a>>),
    DropIdentity(Box<DropIdentity<'a>>),
    Describe(Box<Describe<'a>>),
    // ScyllaDB.
    PruneView(Box<PruneView<'a>>),
    CreateServiceLevel(Box<ServiceLevelStatement<'a>>),
    AlterServiceLevel(Box<ServiceLevelStatement<'a>>),
    DropServiceLevel(Box<DropRole<'a>>),
    /// `ATTACH SERVICE LEVEL sl TO role` (`role` is the grantee).
    AttachServiceLevel(Box<RoleGrant<'a>>),
    /// `DETACH SERVICE LEVEL FROM role`.
    DetachServiceLevel(RoleName<'a>),
    ListServiceLevels(Box<ListServiceLevels<'a>>),
}

static_enum!(StatementKind {
    Select(x),
    Insert(x),
    Update(x),
    Delete(x),
    Batch(x),
    Use(x),
    Truncate(x),
    CreateKeyspace(x),
    AlterKeyspace(x),
    DropKeyspace(x),
    CreateTable(x),
    AlterTable(x),
    DropTable(x),
    CreateIndex(x),
    DropIndex(x),
    CreateType(x),
    AlterType(x),
    DropType(x),
    CreateView(x),
    AlterView(x),
    DropView(x),
    CreateFunction(x),
    DropFunction(x),
    CreateAggregate(x),
    DropAggregate(x),
    CreateTrigger(x),
    DropTrigger(x),
    Grant(x),
    Revoke(x),
    ListPermissions(x),
    CreateRole(x),
    AlterRole(x),
    DropRole(x),
    ListRoles(x),
    GrantRole(x),
    RevokeRole(x),
    CreateUser(x),
    AlterUser(x),
    DropUser(x),
    ListUsers,
    AddIdentity(x),
    DropIdentity(x),
    Describe(x),
    PruneView(x),
    CreateServiceLevel(x),
    AlterServiceLevel(x),
    DropServiceLevel(x),
    AttachServiceLevel(x),
    DetachServiceLevel(x),
    ListServiceLevels(x),
});

/// A parsed statement with its bind markers.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Statement<'a> {
    pub kind: StatementKind<'a>,
    /// One entry per bind marker, in source order: its name for `:name`, `None` for `?`.
    pub bind_markers: Vec<Option<Ident<'a>>>,
    /// The statement's text within the parsed string (without the trailing `;`).
    pub span: Span,
}
static_struct!(Statement { kind, bind_markers, span });
