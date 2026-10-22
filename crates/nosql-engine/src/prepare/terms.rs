//! Terms: values in statements (constants, bind markers, collection / tuple / UDT literals,
//! function calls, type hints, arithmetic), typed against their receiver.
//!
//! Follows Cassandra's `cql3.Constants`, `Lists`, `Sets`, `Maps`, `Tuples`, `UserTypes`,
//! `Vectors`, `FunctionCall`, `TypeCast` and `AssignmentTestable` (Apache-2.0), with ScyllaDB's
//! messages where they differ. Constant sub-terms are serialised at prepare time; a prepared
//! statement re-evaluates only what depends on bind values or non-deterministic functions.

use std::sync::Arc;

use bytes::Bytes;
use nosql_cql::ast::{self, LiteralKind as AstKind};
use nosql_types::{CqlType, Dialect, LiteralKind};

use super::{PrepareCtx, Receiver};
use crate::error::{Error, Result};
use crate::functions::{ArgInfo, Assign, FnCtx, Function, arith, test_type};
use crate::messages as msg;
use crate::response::Param;

/// A prepared term.
#[derive(Clone, Debug)]
pub enum Term {
    /// A constant (`None` is null).
    Const(Option<Bytes>),
    /// A bind marker; the value is validated against `ty` when bound.
    Bind {
        index: usize,
        ty: CqlType,
        receiver: String,
    },
    /// A collection, tuple, UDT or vector literal with non-constant parts.
    Literal {
        ty: CqlType,
        kind: LitKind,
        elems: Vec<Term>,
    },
    /// A function call with non-constant (or non-deterministic) parts.
    Call {
        f: Arc<dyn Function>,
        args: Vec<Term>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LitKind {
    List,
    Set,
    /// Elements alternate key, value.
    Map,
    /// Tuples and UDTs (missing trailing fields are null).
    Tuple,
    Vector,
}

/// A bound value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Bound {
    Null,
    Unset,
    Val(Bytes),
}

impl Bound {
    pub fn into_option(self) -> Option<Bytes> {
        match self {
            Bound::Val(b) => Some(b),
            _ => None,
        }
    }

    pub fn is_unset(&self) -> bool {
        matches!(self, Bound::Unset)
    }
}

/// What evaluation can see.
pub struct EvalCtx<'a> {
    pub params: &'a [Param],
    pub dialect: Dialect,
    pub fn_ctx: FnCtx,
    /// The request's "now" in seconds (protocol v5 `now_in_seconds`), when given.
    pub now: Option<i64>,
    /// Column masks a read applies to selected values (`crate::masking`).
    pub masks: Option<std::sync::Arc<crate::masking::ReadMasks>>,
}

impl<'a> EvalCtx<'a> {
    pub fn new(params: &'a [Param], dialect: Dialect) -> Self {
        EvalCtx {
            params,
            dialect,
            fn_ctx: FnCtx {
                dialect,
                protocol_version: 4,
            },
            now: None,
            masks: None,
        }
    }
}

impl Term {
    pub fn null() -> Term {
        Term::Const(None)
    }

    pub fn is_const(&self) -> bool {
        matches!(self, Term::Const(_))
    }

    pub fn const_value(&self) -> Option<&Option<Bytes>> {
        match self {
            Term::Const(v) => Some(v),
            _ => None,
        }
    }

    /// Whether the term contains a bind marker.
    pub fn has_bind(&self) -> bool {
        match self {
            Term::Bind { .. } => true,
            Term::Literal { elems, .. } => elems.iter().any(Term::has_bind),
            Term::Call { args, .. } => args.iter().any(Term::has_bind),
            Term::Const(_) => false,
        }
    }

    pub fn eval(&self, cx: &EvalCtx) -> Result<Bound> {
        match self {
            Term::Const(None) => Ok(Bound::Null),
            Term::Const(Some(b)) => Ok(Bound::Val(b.clone())),
            Term::Bind {
                index,
                ty,
                receiver,
            } => match cx.params.get(*index) {
                Some(Param::Set(b)) => {
                    validate_bound(ty, b, receiver, cx.dialect)?;
                    if has_unordered(ty)
                        && let Some(n) = normalized(ty, b)
                    {
                        return Ok(Bound::Val(n));
                    }
                    Ok(Bound::Val(b.clone()))
                }
                Some(Param::Null) => Ok(Bound::Null),
                Some(Param::Unset) => Ok(Bound::Unset),
                None => Err(Error::invalid(format!(
                    "Missing value for bind variable {index}"
                ))),
            },
            Term::Literal { ty, kind, elems } => {
                let mut vals = Vec::with_capacity(elems.len());
                for (i, e) in elems.iter().enumerate() {
                    match e.eval(cx)? {
                        Bound::Val(b) => vals.push(Some(b)),
                        Bound::Null => vals.push(None),
                        Bound::Unset => return Err(unset_element(ty, *kind, i, e, cx.dialect)),
                    }
                }
                build_literal(ty, *kind, vals, cx.dialect).map(Bound::Val)
            }
            Term::Call { f, args } => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    match a.eval(cx)? {
                        Bound::Unset => {
                            return Err(match (cx.dialect, a) {
                                (Dialect::Scylla, Term::Bind { index, .. }) => {
                                    unexpected_unset(*index)
                                }
                                _ => Error::invalid(format!(
                                    "Invalid unset value for argument in call to function {}",
                                    f.name().name
                                )),
                            });
                        }
                        b => vals.push(b.into_option()),
                    }
                }
                if f.null_on_null_input() && vals.iter().any(|v| v.is_none()) {
                    return Ok(Bound::Null);
                }
                Ok(f.execute(&cx.fn_ctx, &vals)?
                    .map(Bound::Val)
                    .unwrap_or(Bound::Null))
            }
        }
    }

    /// Evaluates without bind values (constants and functions only).
    pub fn eval_const(&self, d: Dialect) -> Result<Option<Bytes>> {
        Ok(self.eval(&EvalCtx::new(&[], d))?.into_option())
    }
}

/// ScyllaDB's error for an unset value where a value is needed.
pub fn unexpected_unset(index: usize) -> Error {
    Error::invalid(format!("Unexpected unset value for bind variable {index}"))
}

/// An unset value inside a literal: ScyllaDB names the bind variable, Cassandra the tuple field
/// (`Tuples.Value`), the UDT field (`UserTypes.DelayedValue`) or the collection.
fn unset_element(ty: &CqlType, kind: LitKind, i: usize, e: &Term, d: Dialect) -> Error {
    if let (Dialect::Scylla, Term::Bind { index, .. }) = (d, e) {
        return unexpected_unset(*index);
    }
    match (kind, ty) {
        (LitKind::Tuple, CqlType::Udt(def, _)) => Error::invalid(format!(
            "Invalid unset value for field '{}' of user defined type {}",
            def.fields.get(i).map_or("", |(n, _)| n.as_str()),
            def.name
        )),
        (LitKind::Tuple, _) => {
            Error::invalid(format!("Invalid unset value for tuple field number {i}"))
        }
        _ => Error::invalid("Invalid unset value in collection"),
    }
}

/// Whether values of `ty` hold a set or a map (whose serialised form a client may send out of
/// order or with duplicates).
pub fn has_unordered(ty: &CqlType) -> bool {
    match ty {
        CqlType::Set { .. } | CqlType::Map { .. } => true,
        CqlType::List { elem, .. } | CqlType::Vector { elem, .. } => has_unordered(elem),
        CqlType::Tuple(ts) => ts.iter().any(has_unordered),
        CqlType::Udt(def, _) => def.fields.iter().any(|(_, t)| has_unordered(t)),
        _ => false,
    }
}

/// A bound value with its sets and maps in the type's order without duplicates, at every
/// nesting level, as the references store it (Cassandra's `Sets.Value.fromSerialized` /
/// `Maps.Value.fromSerialized` re-serialise a sorted collection; a duplicate map key keeps its
/// last value): a frozen set sent out of order equals the sorted one. `None` for values this
/// can't take apart (validation reports those).
pub fn normalized(ty: &CqlType, v: &[u8]) -> Option<Bytes> {
    fn elems(ty: &CqlType, v: &[u8]) -> Option<Vec<Bytes>> {
        nosql_types::collection_elements(v)
            .ok()?
            .into_iter()
            .map(|e| e.and_then(|e| norm(ty, e)))
            .collect()
    }
    fn norm(ty: &CqlType, v: &[u8]) -> Option<Bytes> {
        Some(match ty {
            CqlType::Set { elem, .. } => {
                let mut es = elems(elem, v)?;
                es.sort_by(|a, b| nosql_types::compare(elem, a, b));
                es.dedup_by(|a, b| nosql_types::compare(elem, a, b).is_eq());
                nosql_types::pack_collection(es.iter().map(|e| &e[..]))
            }
            CqlType::List { elem, .. } if has_unordered(elem) => {
                let es = elems(elem, v)?;
                nosql_types::pack_collection(es.iter().map(|e| &e[..]))
            }
            CqlType::Map { key, value, .. } => {
                let mut es: Vec<(Bytes, Bytes)> = nosql_types::map_entries(v)
                    .ok()?
                    .into_iter()
                    .map(|(k, x)| Some((norm(key, k)?, norm(value, x)?)))
                    .collect::<Option<_>>()?;
                // Stable: of equal keys the last one stays.
                es.reverse();
                es.sort_by(|a, b| nosql_types::compare(key, &a.0, &b.0));
                es.dedup_by(|a, b| nosql_types::compare(key, &a.0, &b.0).is_eq());
                nosql_types::pack_map(es.iter().map(|(k, x)| (&k[..], &x[..])))
            }
            CqlType::Tuple(_) | CqlType::Udt(..) if has_unordered(ty) => {
                let types: Vec<&CqlType> = match ty {
                    CqlType::Tuple(ts) => ts.iter().collect(),
                    CqlType::Udt(def, _) => def.fields.iter().map(|(_, t)| t).collect(),
                    _ => Vec::new(),
                };
                let fields = nosql_types::tuple_fields(v).ok()?;
                let mut out: Vec<Option<Bytes>> = Vec::with_capacity(fields.len());
                for (i, f) in fields.into_iter().enumerate() {
                    out.push(match f {
                        Some(f) => Some(norm(types.get(i)?, f)?),
                        None => None,
                    });
                }
                nosql_types::pack_fields(out.iter().map(|f| f.as_deref()))
            }
            _ => Bytes::copy_from_slice(v),
        })
    }
    norm(ty, v)
}

/// Validates a bound value against its receiver's type, with the dialect's message.
pub fn validate_bound(ty: &CqlType, b: &[u8], receiver: &str, d: Dialect) -> Result<()> {
    let check = if ty.is_counter() {
        &CqlType::Bigint
    } else {
        ty
    };
    nosql_types::validate(check, b, d).map_err(|e| {
        if d.is_scylla() && e.message.starts_with("marshaling error:") {
            Error::invalid(format!(
                "Exception while binding column {receiver}: {}",
                e.message
            ))
        } else {
            Error::invalid(e.message)
        }
    })
}

/// Maps a literal error to the error the dialect sends (some ScyllaDB parse failures surface as
/// server errors).
pub fn marshal_error(e: nosql_types::MarshalError, d: Dialect) -> Error {
    if d.is_scylla()
        && (e.message == "A hex string representing bytes must have an even length"
            || e.message.starts_with("Non-hex characters in"))
    {
        return Error::server(e.message);
    }
    Error::invalid(e.message)
}

fn literal_kind(k: AstKind) -> LiteralKind {
    match k {
        AstKind::String => LiteralKind::String,
        AstKind::Integer => LiteralKind::Integer,
        AstKind::Float => LiteralKind::Float,
        AstKind::Boolean => LiteralKind::Boolean,
        AstKind::Duration => LiteralKind::Duration,
        AstKind::Uuid => LiteralKind::Uuid,
        AstKind::Hex => LiteralKind::Hex,
        // Only in property maps.
        AstKind::List => LiteralKind::String,
    }
}

/// `Constants.Type.getPreferedTypeFor`.
pub fn preferred_type(l: &ast::Literal) -> Option<CqlType> {
    Some(match l.kind {
        AstKind::String => {
            if l.text.is_ascii() {
                CqlType::Ascii
            } else {
                CqlType::Text
            }
        }
        AstKind::Integer => match l.text.parse::<i64>() {
            Ok(v) if v >= i32::MIN as i64 && v <= i32::MAX as i64 => CqlType::Int,
            Ok(_) => CqlType::Bigint,
            Err(_) => CqlType::Varint,
        },
        AstKind::Float => {
            if matches!(l.text.as_ref(), "NaN" | "-NaN" | "Infinity" | "-Infinity") {
                CqlType::Double
            } else {
                match (
                    nosql_types::num::Decimal::parse(&l.text),
                    l.text.parse::<f64>(),
                ) {
                    (Some(d), Ok(f))
                        if nosql_types::num::Decimal::parse(&nosql_types::float::java_double(
                            f,
                        ))
                        .is_some_and(|x| x.cmp_numeric(&d).is_eq()) =>
                    {
                        CqlType::Double
                    }
                    (Some(_), _) => CqlType::Decimal,
                    _ => CqlType::Double,
                }
            }
        }
        AstKind::Boolean => CqlType::Boolean,
        AstKind::Hex => CqlType::Tinyint,
        AstKind::Duration => CqlType::Duration,
        // Only in property maps.
        AstKind::List => return None,
        AstKind::Uuid => {
            if nosql_types::timeuuid::parse_uuid(&l.text)
                .is_some_and(|u| nosql_types::timeuuid::version(&u) == 1)
            {
                CqlType::Timeuuid
            } else {
                CqlType::Uuid
            }
        }
    })
}

fn unwrap_hint<'t, 'a>(t: &'t ast::Term<'a>) -> &'t ast::Term<'a> {
    t
}

/// `AssignmentTestable.testAssignment` of a raw term against `ty`.
pub fn assign(t: &ast::Term, ty: &CqlType, cx: &PrepareCtx) -> Assign {
    let d = cx.dialect;
    match unwrap_hint(t) {
        ast::Term::Literal(l) => {
            if !nosql_types::literal::is_assignable(literal_kind(l.kind), ty, d) {
                return Assign::No;
            }
            if let CqlType::Custom(_) = ty {
                return Assign::Weak;
            }
            if preferred_type(l).as_ref() == Some(ty) {
                Assign::Exact
            } else {
                Assign::Weak
            }
        }
        ast::Term::Null => {
            if ty.is_counter() {
                Assign::No
            } else {
                Assign::Weak
            }
        }
        ast::Term::Bind(_) => Assign::Weak,
        ast::Term::List(elems) => match ty {
            CqlType::List { elem, .. } | CqlType::Vector { elem, .. } => {
                all(elems.iter().map(|e| assign(e, elem, cx)))
            }
            CqlType::Set { elem, .. } if d.is_scylla() => {
                all(elems.iter().map(|e| assign(e, elem, cx))).min(Assign::Weak)
            }
            _ => Assign::No,
        },
        ast::Term::Set(elems) => match ty {
            CqlType::Set { elem, .. } => all(elems.iter().map(|e| assign(e, elem, cx))),
            CqlType::Map { .. } if elems.is_empty() => Assign::Weak,
            _ => Assign::No,
        },
        ast::Term::Map(entries) => match ty {
            CqlType::Map { key, value, .. } => all(entries
                .iter()
                .flat_map(|(k, v)| [assign(k, key, cx), assign(v, value, cx)])),
            _ => Assign::No,
        },
        ast::Term::Udt(fields) => match ty {
            CqlType::Udt(def, _) => {
                let mut r = Assign::Exact;
                for (name, v) in fields {
                    let Some(i) = def.field_index(&name.name()) else {
                        return Assign::No;
                    };
                    r = r.min(assign(v, &def.fields[i].1, cx));
                }
                r
            }
            _ => Assign::No,
        },
        ast::Term::Tuple(elems) => match ty {
            CqlType::Tuple(types) => {
                if elems.len() > types.len() {
                    return Assign::No;
                }
                all(elems.iter().zip(types).map(|(e, t)| assign(e, t, cx)))
            }
            _ => Assign::No,
        },
        ast::Term::TypeHint(h) => {
            match super::super::schema::types::resolve(&h.ty, &cx.keyspace, &cx.udt_resolver(), d) {
                Ok(hint) => test_type(&hint, ty),
                Err(_) => Assign::No,
            }
        }
        ast::Term::Function(call) => {
            let args: Vec<RawArg> = call.args.iter().map(|a| RawArg { term: a, cx }).collect();
            let refs: Vec<&dyn ArgInfo> = args.iter().map(|a| a as &dyn ArgInfo).collect();
            let ks = call.name.keyspace.as_ref().map(|k| k.name().to_string());
            // User functions too (a UDF call inside a collection argument).
            let user = cx.user_functions.map(|u| (u, cx.keyspace.as_str()));
            match cx.functions.resolve_named(
                ks.as_deref(),
                &call.name.resolved().to_ascii_lowercase(),
                &call.name.resolved(),
                &refs,
                None,
                user,
            ) {
                Ok(Some(f)) => test_type(f.return_type(), ty),
                _ => Assign::No,
            }
        }
        ast::Term::Cast(c) => test_type(&super::super::schema::types::native(c.ty), ty),
        ast::Term::Binary(_) | ast::Term::Negate(_) => {
            if matches!(
                ty,
                CqlType::Tinyint
                    | CqlType::Smallint
                    | CqlType::Int
                    | CqlType::Bigint
                    | CqlType::Float
                    | CqlType::Double
                    | CqlType::Decimal
                    | CqlType::Varint
                    | CqlType::Counter
                    | CqlType::Timestamp
                    | CqlType::Date
                    | CqlType::Text
                    | CqlType::Ascii
            ) {
                Assign::Weak
            } else {
                Assign::No
            }
        }
    }
}

fn all(it: impl Iterator<Item = Assign>) -> Assign {
    it.fold(Assign::Exact, Assign::min)
}

/// A raw argument of a function call, for overload resolution.
pub struct RawArg<'t, 'a, 'c> {
    pub term: &'t ast::Term<'a>,
    pub cx: &'t PrepareCtx<'c>,
}

impl ArgInfo for RawArg<'_, '_, '_> {
    fn test(&self, ty: &CqlType, _d: Dialect) -> Assign {
        assign(self.term, ty, self.cx)
    }
    fn is_marker(&self) -> bool {
        matches!(self.term, ast::Term::Bind(_))
    }
    fn exact_type(&self) -> Option<CqlType> {
        exact_type(self.term, self.cx)
    }
    fn compatible_type(&self) -> Option<CqlType> {
        compatible_type(self.term, self.cx)
    }
    fn array_len(&self) -> Option<usize> {
        if let ast::Term::List(elems) = self.term {
            Some(elems.len())
        } else {
            None
        }
    }
    fn text(&self) -> String {
        self.term.to_string()
    }
    fn is_null(&self) -> bool {
        matches!(self.term, ast::Term::Null)
    }
    fn scylla_type(&self) -> Option<CqlType> {
        // A collection literal is a frozen value.
        super::selection::scylla_literal_type(self.term)
            .map(|t| if t.is_collection() { t.freeze() } else { t })
            .or_else(|| self.exact_type())
    }
    fn scylla_text(&self) -> String {
        super::selection::scylla_literal_name(self.term)
    }
}

/// `Term.Raw.getExactTypeIfKnown`.
pub fn exact_type(t: &ast::Term, cx: &PrepareCtx) -> Option<CqlType> {
    match t {
        ast::Term::Literal(l) => preferred_type(l),
        ast::Term::TypeHint(h) => super::super::schema::types::resolve(
            &h.ty,
            &cx.keyspace,
            &cx.udt_resolver(),
            cx.dialect,
        )
        .ok(),
        ast::Term::Cast(c) => Some(super::super::schema::types::native(c.ty)),
        ast::Term::Function(call) => {
            let args: Vec<RawArg> = call.args.iter().map(|a| RawArg { term: a, cx }).collect();
            let refs: Vec<&dyn ArgInfo> = args.iter().map(|a| a as &dyn ArgInfo).collect();
            let ks = call.name.keyspace.as_ref().map(|k| k.name().to_string());
            cx.functions
                .resolve(
                    ks.as_deref(),
                    &call.name.resolved().to_ascii_lowercase(),
                    &refs,
                    None,
                    None,
                )
                .ok()
                .flatten()
                .map(|f| f.return_type().clone())
        }
        _ => None,
    }
}

/// `Term.Raw.getCompatibleTypeIfKnown`: collection literals have the type their elements'
/// preferred types fit (`Lists` / `Sets` / `Maps.getPreferredCompatibleType`).
pub fn compatible_type(t: &ast::Term, cx: &PrepareCtx) -> Option<CqlType> {
    let common = |elems: &mut dyn Iterator<Item = &ast::Term>| -> Option<CqlType> {
        let mut types: Vec<CqlType> = Vec::new();
        for e in elems {
            if let Some(ty) = compatible_type(e, cx)
                && !types.contains(&ty)
            {
                types.push(ty);
            }
        }
        types
            .iter()
            .find(|cand| {
                types
                    .iter()
                    .all(|t| crate::functions::test_type(t, cand) != crate::functions::Assign::No)
            })
            .cloned()
    };
    match t {
        ast::Term::List(v) => common(&mut v.iter()).map(|e| CqlType::list(e, true)),
        ast::Term::Set(v) => common(&mut v.iter()).map(|e| CqlType::set(e, true)),
        ast::Term::Map(v) => {
            let k = common(&mut v.iter().map(|(k, _)| k))?;
            let val = common(&mut v.iter().map(|(_, x)| x))?;
            Some(CqlType::map(k, val, true))
        }
        ast::Term::Tuple(items) => items
            .iter()
            .map(|e| compatible_type(e, cx))
            .collect::<Option<Vec<_>>>()
            .map(CqlType::Tuple),
        _ => exact_type(t, cx),
    }
}

fn type_text(ty: &CqlType, d: Dialect) -> String {
    nosql_types::cql_name_in(ty, d)
}

/// Prepares `t` for `r`, recording bind marker specs in `cx`.
pub fn prepare(t: &ast::Term, r: &Receiver, cx: &mut PrepareCtx) -> Result<Term> {
    let d = cx.dialect;
    match t {
        ast::Term::Literal(l) => {
            let ty = if r.ty.is_counter() {
                CqlType::Counter
            } else {
                r.ty.clone()
            };
            // A string constant becomes a tuple through `TupleType.fromString` (`'1:a'`).
            let tuple_string = matches!(ty, CqlType::Tuple(_)) && l.kind == AstKind::String;
            if !tuple_string
                && matches!(
                    ty,
                    CqlType::List { .. }
                        | CqlType::Set { .. }
                        | CqlType::Map { .. }
                        | CqlType::Tuple(_)
                        | CqlType::Udt(..)
                        | CqlType::Vector { .. }
                )
            {
                let shown = l.text.to_string();
                return Err(Error::invalid(format!(
                    "Invalid {} constant ({shown}) for \"{}\" of type {}",
                    literal_kind(l.kind).name(),
                    r.name,
                    type_text(&ty, d)
                )));
            }
            nosql_types::literal_to_bytes(literal_kind(l.kind), &l.text, &r.name, &ty, d)
                .map(|b| Term::Const(Some(b)))
                .map_err(|e| marshal_error(e, d))
        }
        ast::Term::Null => {
            if r.ty.is_counter() {
                return Err(Error::invalid(
                    "Invalid null value for counter increment/decrement",
                ));
            }
            Ok(Term::Const(None))
        }
        ast::Term::Bind(m) => {
            let index = cx.bind(m, r);
            Ok(Term::Bind {
                index,
                ty: r.ty.clone(),
                receiver: r.name.clone(),
            })
        }
        ast::Term::List(elems) => prepare_list(t, elems, r, cx),
        ast::Term::Set(elems) => prepare_set(t, elems, r, cx),
        ast::Term::Map(entries) => prepare_map(t, entries, r, cx),
        ast::Term::Udt(fields) => prepare_udt(fields, r, cx),
        ast::Term::Tuple(elems) => prepare_tuple(elems, r, cx),
        ast::Term::Function(call) => prepare_call(&call.name, &call.args, r, cx),
        ast::Term::TypeHint(h) => {
            let hint =
                super::super::schema::types::resolve(&h.ty, &cx.keyspace, &cx.udt_resolver(), d)?;
            // ScyllaDB widens an integer function result (`(bigint)blobasint(…)`).
            let widened = d.is_scylla()
                && matches!(h.term, ast::Term::Function(_))
                && exact_type(&h.term, cx).is_some_and(|from| {
                    integer_rank(&from).is_some() && integer_rank(&from) < integer_rank(&hint)
                });
            if !widened && assign(&h.term, &hint, cx) == Assign::No {
                return Err(Error::invalid(format!(
                    "Cannot cast value {} to type {}",
                    h.term,
                    type_text(&hint, d)
                )));
            }
            if !r.ty.is_value_compatible_with(&hint) && test_type(&hint, &r.ty) == Assign::No {
                return Err(Error::invalid(format!(
                    "Cannot assign value {t} to {} of type {}",
                    r.name,
                    type_text(&r.ty, d)
                )));
            }
            prepare(&h.term, &r.derive(r.name.clone(), hint), cx)
        }
        ast::Term::Cast(c) => {
            let to = super::super::schema::types::native(c.ty);
            let inner_ty = exact_type(&c.arg, cx).ok_or_else(|| {
                Error::invalid(format!("Cannot infer type of argument {} in CAST", c.arg))
            })?;
            let inner = prepare(&c.arg, &r.derive(r.name.clone(), inner_ty.clone()), cx)?;
            let plan =
                nosql_types::cast::plan(&c.arg.to_string(), &inner_ty, &to, c.ty.as_str(), d)
                    .map_err(|e| Error::invalid(e.message))?;
            let f: Arc<dyn Function> = Arc::new(CastFn::new(inner_ty, to, plan));
            fold_call(f, vec![inner], d)
        }
        ast::Term::Binary(b) => {
            let name = arith::function_of(b.op);
            let call_args = [b.left.clone(), b.right.clone()];
            prepare_operator(name, &call_args, r, cx)
        }
        ast::Term::Negate(x) => {
            if let ast::Term::Literal(l) = x.as_ref()
                && matches!(l.kind, AstKind::Integer | AstKind::Float)
            {
                let neg = ast::Term::Literal(ast::Literal {
                    kind: l.kind,
                    text: format!("-{}", l.text).into(),
                    dollar: false,
                });
                return prepare(&neg, r, cx);
            }
            prepare_operator(arith::NEGATE, std::slice::from_ref(x.as_ref()), r, cx)
        }
    }
}

fn prepare_operator(
    name: &str,
    args: &[ast::Term],
    r: &Receiver,
    cx: &mut PrepareCtx,
) -> Result<Term> {
    let fname = ast::FunctionName {
        keyspace: None,
        name: ast::Ident::unquoted(name.to_string()),
        keyword: true,
    };
    prepare_call(&fname, args, r, cx)
}

/// Prepares a function call whose result goes to `r`.
pub fn prepare_call(
    name: &ast::FunctionName,
    args: &[ast::Term],
    r: &Receiver,
    cx: &mut PrepareCtx,
) -> Result<Term> {
    let d = cx.dialect;
    let ks = name.keyspace.as_ref().map(|k| k.name().to_string());
    let fname = name.resolved().to_ascii_lowercase();
    let f = {
        let raw: Vec<RawArg> = args.iter().map(|a| RawArg { term: a, cx }).collect();
        let refs: Vec<&dyn ArgInfo> = raw.iter().map(|a| a as &dyn ArgInfo).collect();
        let user = cx.user_functions.map(|u| (u, cx.keyspace.as_str()));
        let table = cx.schema.table(&r.keyspace, &r.table);
        match table {
            Some(t) if fname == "token" && ks.as_deref().is_none_or(|k| k == "system") => {
                let pk: Vec<CqlType> = t.partition_key_columns().map(|c| c.ty.clone()).collect();
                Some(cx.functions.resolve_token(&pk, &refs)?)
            }
            _ => cx.functions.resolve_named(
                ks.as_deref(),
                &fname,
                &name.resolved(),
                &refs,
                Some(&r.ty),
                user,
            )?,
        }
    };
    let Some(f) = f else {
        return Err(unknown_function(d, ks.as_deref(), &name.resolved()));
    };
    cx.record_call(&f);
    if f.is_aggregate() {
        return Err(Error::invalid(format!(
            "Aggregation functions are not supported in terms: {}",
            f.name()
        )));
    }
    // ScyllaDB widens an integer result into a wider integer receiver (`blobasint(…)` into a
    // `bigint` column; probed for every pair).
    let widen = d.is_scylla()
        && integer_rank(f.return_type()) < integer_rank(&r.ty)
        && integer_rank(f.return_type()).is_some();
    if !widen
        && test_type(f.return_type(), &r.ty) == Assign::No
        && !r.ty.is_value_compatible_with(f.return_type())
    {
        return Err(Error::invalid(format!(
            "Type error: cannot assign result of function {} (type {}) to {} (type {})",
            f.name(),
            nosql_types::cql_name(f.return_type()),
            r.name,
            nosql_types::cql_name(&r.ty)
        )));
    }
    let mut prepared = Vec::with_capacity(args.len());
    for (i, (a, ty)) in args.iter().zip(f.arg_types().to_vec()).enumerate() {
        let spec = r.derive(
            format!("arg{i}({})", f.name().to_string().to_lowercase()),
            ty,
        );
        prepared.push(prepare(a, &spec, cx)?);
    }
    if widen {
        let from = f.return_type().clone();
        let call = fold_call(f, prepared, d)?;
        let plan = nosql_types::cast::plan("", &from, &r.ty, &nosql_types::cql_name(&r.ty), d)
            .map_err(|e| Error::invalid(e.message))?;
        let cast: Arc<dyn Function> = Arc::new(CastFn::new(from, r.ty.clone(), plan));
        return fold_call(cast, vec![call], d);
    }
    fold_call(f, prepared, d)
}

/// The width order of the integer types (`None` for others).
fn integer_rank(t: &CqlType) -> Option<u8> {
    Some(match t {
        CqlType::Tinyint => 0,
        CqlType::Smallint => 1,
        CqlType::Int => 2,
        CqlType::Bigint | CqlType::Counter => 3,
        CqlType::Varint => 4,
        _ => return None,
    })
}

/// Pre-evaluates a deterministic call on constants.
pub fn fold_call(f: Arc<dyn Function>, args: Vec<Term>, d: Dialect) -> Result<Term> {
    if f.is_deterministic() && args.iter().all(Term::is_const) {
        let t = Term::Call { f, args };
        return Ok(Term::Const(t.eval_const(d)?));
    }
    Ok(Term::Call { f, args })
}

pub fn unknown_function(d: Dialect, ks: Option<&str>, name: &str) -> Error {
    let shown = match ks {
        Some(k) => format!("{k}.{name}"),
        None => name.to_string(),
    };
    Error::invalid(match d {
        Dialect::Scylla => format!("Unknown function {shown} called"),
        Dialect::Cassandra => format!("Unknown function '{shown}'"),
    })
}

fn element_error(
    kind: &str,
    r: &Receiver,
    what: &str,
    t: &ast::Term,
    ty: &CqlType,
    d: Dialect,
) -> Error {
    Error::invalid(format!(
        "Invalid {kind} literal for {}: {what} {t} is not of type {}",
        r.name,
        type_text(ty, d)
    ))
}

fn reject_markers(kind: &str, elems: &[&ast::Term], r: &Receiver, d: Dialect) -> Result<()> {
    if !d.is_scylla() && elems.iter().any(|e| contains_marker(e)) {
        return Err(Error::invalid(format!(
            "Invalid {kind} literal for {}: bind variables are not supported inside collection literals",
            r.name
        )));
    }
    Ok(())
}

fn contains_marker(t: &ast::Term) -> bool {
    match t {
        ast::Term::Bind(_) => true,
        ast::Term::List(v) | ast::Term::Set(v) | ast::Term::Tuple(v) => {
            v.iter().any(contains_marker)
        }
        ast::Term::Map(v) => v
            .iter()
            .any(|(k, x)| contains_marker(k) || contains_marker(x)),
        ast::Term::Udt(v) => v.iter().any(|(_, x)| contains_marker(x)),
        _ => false,
    }
}

fn null_check(elems: &[Term], kind: LitKind, d: Dialect) -> Result<()> {
    if elems.iter().any(|e| matches!(e, Term::Const(None))) {
        return Err(match kind {
            LitKind::List => msg::null_list_element(d),
            LitKind::Vector => Error::invalid("null is not supported inside vectors"),
            _ => Error::invalid("null is not supported inside collections"),
        });
    }
    Ok(())
}

fn finish(ty: CqlType, kind: LitKind, elems: Vec<Term>, d: Dialect) -> Result<Term> {
    if elems.iter().all(Term::is_const) {
        let vals: Vec<Option<Bytes>> = elems
            .into_iter()
            .map(|e| if let Term::Const(v) = e { v } else { None })
            .collect();
        return build_literal(&ty, kind, vals, d).map(|b| Term::Const(Some(b)));
    }
    Ok(Term::Literal { ty, kind, elems })
}

fn prepare_list(
    t: &ast::Term,
    elems: &[ast::Term],
    r: &Receiver,
    cx: &mut PrepareCtx,
) -> Result<Term> {
    let d = cx.dialect;
    match &r.ty {
        CqlType::List { elem, .. } => {
            let refs: Vec<&ast::Term> = elems.iter().collect();
            reject_markers("list", &refs, r, d)?;
            let er = r.derive(format!("value({})", r.name), (**elem).clone());
            let mut out = Vec::with_capacity(elems.len());
            for e in elems {
                if assign(e, elem, cx) == Assign::No {
                    return Err(element_error("list", r, "value", e, elem, d));
                }
                out.push(prepare(e, &er, cx)?);
            }
            null_check(&out, LitKind::List, d)?;
            finish(r.ty.clone(), LitKind::List, out, d)
        }
        CqlType::Vector { elem, dim } => {
            if elems.len() != *dim as usize {
                return Err(msg::vector_wrong_dimension(
                    d,
                    &r.name,
                    &type_text(&r.ty, d),
                    *dim as usize,
                    elems.len(),
                ));
            }
            let er = r.derive(format!("value({})", r.name), (**elem).clone());
            let mut out = Vec::with_capacity(elems.len());
            for e in elems {
                if assign(e, elem, cx) == Assign::No {
                    return Err(element_error("vector", r, "value", e, elem, d));
                }
                out.push(prepare(e, &er, cx)?);
            }
            null_check(&out, LitKind::Vector, d)?;
            finish(r.ty.clone(), LitKind::Vector, out, d)
        }
        CqlType::Set { .. } => Err(msg::list_literal_for_set(d, &r.name, &type_text(&r.ty, d))),
        _ => {
            let _ = t;
            Err(match d {
                Dialect::Scylla => Error::invalid(format!(
                    "Invalid list literal for {} of type {}",
                    r.name,
                    type_text(&r.ty, d)
                )),
                Dialect::Cassandra => Error::invalid(format!(
                    "Unexpected receiver type '{}'; only list and vector are expected",
                    type_text(&r.ty, d)
                )),
            })
        }
    }
}

fn prepare_set(
    t: &ast::Term,
    elems: &[ast::Term],
    r: &Receiver,
    cx: &mut PrepareCtx,
) -> Result<Term> {
    let d = cx.dialect;
    match &r.ty {
        CqlType::Set { elem, .. } => {
            let refs: Vec<&ast::Term> = elems.iter().collect();
            reject_markers("set", &refs, r, d)?;
            let er = r.derive(format!("value({})", r.name), (**elem).clone());
            let mut out = Vec::with_capacity(elems.len());
            for e in elems {
                if assign(e, elem, cx) == Assign::No {
                    return Err(element_error("set", r, "value", e, elem, d));
                }
                out.push(prepare(e, &er, cx)?);
            }
            null_check(&out, LitKind::Set, d)?;
            finish(r.ty.clone(), LitKind::Set, out, d)
        }
        CqlType::Map { .. } if elems.is_empty() => {
            finish(r.ty.clone(), LitKind::Map, Vec::new(), d)
        }
        _ => {
            let _ = t;
            Err(Error::invalid(format!(
                "Invalid set literal for {} of type {}",
                r.name,
                type_text(&r.ty, d)
            )))
        }
    }
}

fn prepare_map(
    t: &ast::Term,
    entries: &[(ast::Term, ast::Term)],
    r: &Receiver,
    cx: &mut PrepareCtx,
) -> Result<Term> {
    let d = cx.dialect;
    let CqlType::Map { key, value, .. } = &r.ty else {
        let _ = t;
        return Err(Error::invalid(format!(
            "Invalid map literal for {} of type {}",
            r.name,
            type_text(&r.ty, d)
        )));
    };
    let refs: Vec<&ast::Term> = entries.iter().flat_map(|(k, v)| [k, v]).collect();
    reject_markers("map", &refs, r, d)?;
    let kr = r.derive(format!("key({})", r.name), (**key).clone());
    let vr = r.derive(format!("value({})", r.name), (**value).clone());
    let mut out = Vec::with_capacity(entries.len() * 2);
    for (k, v) in entries {
        if assign(k, key, cx) == Assign::No {
            return Err(element_error("map", r, "key", k, key, d));
        }
        if assign(v, value, cx) == Assign::No {
            return Err(element_error("map", r, "value", v, value, d));
        }
        out.push(prepare(k, &kr, cx)?);
        out.push(prepare(v, &vr, cx)?);
    }
    null_check(&out, LitKind::Map, d)?;
    finish(r.ty.clone(), LitKind::Map, out, d)
}

fn prepare_udt(
    fields: &[(ast::Ident, ast::Term)],
    r: &Receiver,
    cx: &mut PrepareCtx,
) -> Result<Term> {
    let d = cx.dialect;
    let CqlType::Udt(def, _) = &r.ty else {
        // `{}`-style literals of other receivers report as maps / sets.
        return Err(Error::invalid(format!(
            "Invalid user type literal for {} of type {}",
            r.name,
            type_text(&r.ty, d)
        )));
    };
    let mut values: Vec<Option<(&ast::Term, usize)>> = vec![None; def.fields.len()];
    for (name, v) in fields {
        let n = name.name();
        let Some(i) = def.field_index(&n) else {
            return Err(Error::invalid(format!(
                "Unknown field '{n}' in value of user defined type {}",
                def.name
            )));
        };
        values[i] = Some((v, i));
    }
    let mut out = Vec::with_capacity(def.fields.len());
    for (i, slot) in values.iter().enumerate() {
        let (fname, fty) = &def.fields[i];
        match slot {
            None => out.push(Term::Const(None)),
            Some((v, _)) => {
                if assign(v, fty, cx) == Assign::No {
                    return Err(Error::invalid(format!(
                        "Invalid user type literal for {}: field {fname} is not of type {}",
                        r.name,
                        type_text(fty, d)
                    )));
                }
                out.push(prepare(
                    v,
                    &r.derive(format!("{}.{fname}", r.name), fty.clone()),
                    cx,
                )?);
            }
        }
    }
    finish(r.ty.clone(), LitKind::Tuple, out, d)
}

fn prepare_tuple(elems: &[ast::Term], r: &Receiver, cx: &mut PrepareCtx) -> Result<Term> {
    prepare_tuple_named(elems, r, None, cx)
}

/// A tuple literal whose components have their own receiver names: Cassandra types the
/// components of a multi-column relation's tuple as the columns themselves.
pub fn prepare_tuple_named(
    elems: &[ast::Term],
    r: &Receiver,
    names: Option<&[String]>,
    cx: &mut PrepareCtx,
) -> Result<Term> {
    let d = cx.dialect;
    let types: Vec<CqlType> = match &r.ty {
        CqlType::Tuple(ts) => ts.clone(),
        // ScyllaDB takes a tuple literal as a user type value, field by field (probed; the
        // encodings are the same).
        CqlType::Udt(def, _) if d.is_scylla() => def.fields.iter().map(|(_, t)| t.clone()).collect(),
        // Cassandra too for a frozen one (`UserType extends TupleType`); a non-frozen one fails
        // casting the tuple value (probed).
        CqlType::Udt(def, true) => def.fields.iter().map(|(_, t)| t.clone()).collect(),
        CqlType::Udt(..) => {
            return Err(Error::server(
                "java.lang.ClassCastException: class org.apache.cassandra.cql3.Tuples$Value cannot be cast to class org.apache.cassandra.cql3.UserTypes$Value (org.apache.cassandra.cql3.Tuples$Value and org.apache.cassandra.cql3.UserTypes$Value are in unnamed module of loader 'app')",
            ));
        }
        _ => {
            return Err(Error::invalid(format!(
                "Invalid tuple type literal for {} of type {}",
                r.name,
                type_text(&r.ty, d)
            )));
        }
    };
    if elems.len() > types.len() {
        return Err(Error::invalid(format!(
            "Invalid tuple literal for {}: too many elements. Type {} expects {} but got {}",
            r.name,
            type_text(&r.ty, d),
            types.len(),
            elems.len()
        )));
    }
    let mut out = Vec::with_capacity(types.len());
    for (i, (e, ty)) in elems.iter().zip(&types).enumerate() {
        if assign(e, ty, cx) == Assign::No {
            return Err(Error::invalid(format!(
                "Invalid tuple literal for {}: component {i} is not of type {}",
                r.name,
                type_text(ty, d)
            )));
        }
        let name = match names.and_then(|n| n.get(i)) {
            Some(n) => n.clone(),
            None => format!("{}[{i}]", r.name),
        };
        out.push(prepare(e, &r.derive(name, ty.clone()), cx)?);
    }
    finish(r.ty.clone(), LitKind::Tuple, out, d)
}

/// Unsigned VInt (`VIntCoding.writeUnsignedVInt`) used for variable-size vector elements.
fn put_unsigned_vint(out: &mut Vec<u8>, v: u64) {
    let bits = 64 - (v | 1).leading_zeros();
    let extra = if bits <= 7 {
        0
    } else {
        ((bits - 1) / 7).min(8) as usize
    };
    if extra == 0 {
        out.push(v as u8);
        return;
    }
    let mut buf = [0u8; 9];
    let mut x = v;
    for i in (0..=extra).rev() {
        buf[i] = x as u8;
        x >>= 8;
    }
    let first_mask = !(0xffu16 >> extra) as u8;
    buf[0] |= first_mask;
    out.extend_from_slice(&buf[..=extra]);
}

/// Serialises a literal's evaluated elements as the receiver type stores them.
pub fn build_literal(
    ty: &CqlType,
    kind: LitKind,
    vals: Vec<Option<Bytes>>,
    d: Dialect,
) -> Result<Bytes> {
    match kind {
        LitKind::List => {
            if vals.iter().any(|v| v.is_none()) {
                return Err(msg::null_list_element(d));
            }
            Ok(nosql_types::pack_collection(
                vals.iter().map(|v| v.as_deref().unwrap_or(&[])),
            ))
        }
        LitKind::Set => {
            if vals.iter().any(|v| v.is_none()) {
                return Err(Error::invalid("null is not supported inside collections"));
            }
            let elem = match ty {
                CqlType::Set { elem, .. } => (**elem).clone(),
                _ => CqlType::Blob,
            };
            let mut v: Vec<Bytes> = vals.into_iter().flatten().collect();
            v.sort_by(|a, b| nosql_types::compare(&elem, a, b));
            v.dedup_by(|a, b| nosql_types::compare(&elem, a, b).is_eq());
            Ok(nosql_types::pack_collection(v.iter().map(|b| &b[..])))
        }
        LitKind::Map => {
            if vals.iter().any(|v| v.is_none()) {
                return Err(Error::invalid("null is not supported inside collections"));
            }
            let key = match ty {
                CqlType::Map { key, .. } => (**key).clone(),
                _ => CqlType::Blob,
            };
            let flat: Vec<Bytes> = vals.into_iter().flatten().collect();
            let mut entries: Vec<(Bytes, Bytes)> = flat
                .chunks(2)
                .filter(|c| c.len() == 2)
                .map(|c| (c[0].clone(), c[1].clone()))
                .collect();
            // Later entries win over earlier ones with an equal key.
            entries.reverse();
            entries.sort_by(|a, b| nosql_types::compare(&key, &a.0, &b.0));
            entries.dedup_by(|a, b| nosql_types::compare(&key, &a.0, &b.0).is_eq());
            Ok(nosql_types::pack_map(
                entries.iter().map(|(k, v)| (&k[..], &v[..])),
            ))
        }
        LitKind::Tuple => Ok(nosql_types::pack_fields(vals.iter().map(|v| v.as_deref()))),
        LitKind::Vector => {
            if vals.iter().any(|v| v.is_none()) {
                return Err(Error::invalid("null is not supported inside vectors"));
            }
            let fixed = match ty {
                CqlType::Vector { elem, .. } => elem.value_length_if_fixed(),
                _ => None,
            };
            let mut out = Vec::new();
            for v in vals.into_iter().flatten() {
                if fixed.is_none() {
                    put_unsigned_vint(&mut out, v.len() as u64);
                }
                out.extend_from_slice(&v);
            }
            Ok(Bytes::from(out))
        }
    }
}

/// `CAST(x AS type)` as a function.
#[derive(Debug)]
pub struct CastFn {
    name: crate::functions::FnName,
    args: Vec<CqlType>,
    to: CqlType,
    plan: nosql_types::cast::CastPlan,
}

impl CastFn {
    pub fn new(from: CqlType, to: CqlType, plan: nosql_types::cast::CastPlan) -> Self {
        let name =
            crate::functions::FnName::system(&format!("cast_as_{}", nosql_types::cql_name(&to)));
        CastFn {
            name,
            args: vec![from],
            to,
            plan,
        }
    }
}

impl Function for CastFn {
    fn name(&self) -> &crate::functions::FnName {
        &self.name
    }
    fn arg_types(&self) -> &[CqlType] {
        &self.args
    }
    fn return_type(&self) -> &CqlType {
        &self.to
    }
    fn null_on_null_input(&self) -> bool {
        true
    }
    fn execute(&self, ctx: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        let Some(Some(b)) = args.first() else {
            return Ok(None);
        };
        match &self.plan {
            nosql_types::cast::CastPlan::Identity => Ok(Some(b.clone())),
            nosql_types::cast::CastPlan::Convert { param } => {
                let from = if self.args[0].is_counter() {
                    CqlType::Bigint
                } else {
                    param.clone()
                };
                nosql_types::cast::cast(b, &from, &self.to, ctx.dialect)
                    .map_err(|e| Error::invalid(e.message))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::functions::Registry;
    use crate::schema::Schema;

    fn parse_term(src: &str) -> ast::Term<'static> {
        let q = format!("INSERT INTO t (a) VALUES ({src})");
        let st = nosql_cql::parse_statement(&q, nosql_cql::Dialect::Cassandra).unwrap();
        use nosql_cql::ast::IntoStatic;
        match st.kind.into_static() {
            ast::StatementKind::Insert(i) => match i.values {
                ast::InsertValues::Values { mut values, .. } => values.remove(0),
                _ => unreachable!(),
            },
            _ => unreachable!(),
        }
    }

    fn run(d: Dialect, src: &str, ty: CqlType) -> Result<Term> {
        let schema = Arc::new(Schema::default());
        let reg = Registry::new(d);
        let mut cx = PrepareCtx::new(d, &schema, "ks", &reg, &[None, None, None]);
        let t = parse_term(src);
        prepare(&t, &Receiver::new("ks", "t", "c", ty), &mut cx)
    }

    fn val(d: Dialect, src: &str, ty: CqlType) -> Bytes {
        run(d, src, ty).unwrap().eval_const(d).unwrap().unwrap()
    }

    fn err(d: Dialect, src: &str, ty: CqlType) -> String {
        run(d, src, ty).unwrap_err().message
    }

    #[test]
    fn collection_literals() {
        let d = Dialect::Cassandra;
        let set = val(d, "{3, 1, 2, 1}", CqlType::set(CqlType::Int, false));
        assert_eq!(
            nosql_types::decode(&CqlType::set(CqlType::Int, false), &set).unwrap(),
            nosql_types::Value::Set(vec![
                nosql_types::Value::Int(1),
                nosql_types::Value::Int(2),
                nosql_types::Value::Int(3)
            ])
        );
        let map = val(
            d,
            "{2: 'a', 1: 'b', 2: 'c'}",
            CqlType::map(CqlType::Int, CqlType::Text, false),
        );
        let decoded =
            nosql_types::decode(&CqlType::map(CqlType::Int, CqlType::Text, false), &map).unwrap();
        assert_eq!(
            decoded,
            nosql_types::Value::Map(vec![
                (
                    nosql_types::Value::Int(1),
                    nosql_types::Value::Text("b".into())
                ),
                (
                    nosql_types::Value::Int(2),
                    nosql_types::Value::Text("c".into())
                )
            ])
        );
        assert_eq!(
            err(d, "[1, 'a']", CqlType::list(CqlType::Int, false)),
            "Invalid list literal for c: value 'a' is not of type int"
        );
        assert_eq!(
            err(d, "{1}", CqlType::list(CqlType::Int, false)),
            "Invalid set literal for c of type list<int>"
        );
        assert_eq!(
            err(d, "[1]", CqlType::set(CqlType::Int, false)),
            "Unexpected receiver type 'set<int>'; only list and vector are expected"
        );
        assert_eq!(
            err(Dialect::Scylla, "[1]", CqlType::set(CqlType::Int, false)),
            "Invalid list literal for c of type set<int>"
        );
        assert_eq!(
            err(d, "[1, null]", CqlType::list(CqlType::Int, false)),
            "null is not supported inside collections"
        );
        assert_eq!(
            err(
                Dialect::Scylla,
                "[1, null]",
                CqlType::list(CqlType::Int, false)
            ),
            "Invalid NULL element in list"
        );
        assert_eq!(
            err(
                d,
                "{'a': 1}",
                CqlType::map(CqlType::Int, CqlType::Int, false)
            ),
            "Invalid map literal for c: key 'a' is not of type int"
        );
        assert_eq!(
            err(d, "[?]", CqlType::list(CqlType::Int, false)),
            "Invalid list literal for c: bind variables are not supported inside collection literals"
        );
        assert!(run(Dialect::Scylla, "[?]", CqlType::list(CqlType::Int, false)).is_ok());
        assert_eq!(
            val(d, "{}", CqlType::map(CqlType::Int, CqlType::Int, false))[..],
            [0, 0, 0, 0]
        );
    }

    #[test]
    fn tuples_vectors_and_udts() {
        let d = Dialect::Cassandra;
        let tup = CqlType::Tuple(vec![CqlType::Int, CqlType::Text]);
        assert_eq!(
            err(d, "(1, 'a', 3)", tup.clone()),
            "Invalid tuple literal for c: too many elements. Type frozen<tuple<int, text>> expects 2 but got 3"
        );
        assert_eq!(
            err(d, "('a', 'a')", tup.clone()),
            "Invalid tuple literal for c: component 0 is not of type int"
        );
        let v = CqlType::vector(CqlType::Float, 3);
        assert_eq!(
            err(Dialect::Scylla, "[1.0, 2.0]", v.clone()),
            "Invalid vector literal for c: type vector<float, 3> expects 3 elements but got 2"
        );
        assert_eq!(
            err(d, "[1.0, 2.0]", v.clone()),
            "Invalid vector literal for c of type vector<float, 3>; expected 3 elements, but given 2"
        );
        assert_eq!(
            err(d, "[1.0, null, 2.0]", v.clone()),
            "null is not supported inside vectors"
        );
        assert_eq!(val(d, "[1.0, 2.0, 3.0]", v).len(), 12);
        let vt = CqlType::vector(CqlType::Text, 2);
        assert_eq!(val(d, "['a', 'bc']", vt)[..], *b"\x01a\x02bc");
        let udt = CqlType::Udt(
            Arc::new(nosql_types::UdtDef::new(
                "ks",
                "ty",
                vec![("x".into(), CqlType::Int), ("y".into(), CqlType::Text)],
            )),
            true,
        );
        assert_eq!(
            err(d, "{x: 1, z: 'a'}", udt.clone()),
            "Unknown field 'z' in value of user defined type ty"
        );
        assert_eq!(
            err(d, "{x: 'a'}", udt.clone()),
            "Invalid user type literal for c: field x is not of type int"
        );
        assert_eq!(
            nosql_types::tuple_fields(&val(d, "{y: 'q'}", udt)).unwrap(),
            vec![None, Some(&b"q"[..])]
        );
    }

    #[test]
    fn functions_and_literals() {
        let d = Dialect::Scylla;
        assert_eq!(
            err(d, "now()", CqlType::Text),
            "Type error: cannot assign result of function system.now (type timeuuid) to c (type text)"
        );
        assert!(matches!(
            run(d, "now()", CqlType::Timeuuid).unwrap(),
            Term::Call { .. }
        ));
        assert_eq!(
            val(d, "blobAsInt(0x00000001)", CqlType::Int)[..],
            [0, 0, 0, 1]
        );
        assert_eq!(
            err(d, "'a'", CqlType::Int),
            "Invalid STRING constant (a) for \"c\" of type int"
        );
        assert_eq!(
            run(d, "0x123", CqlType::Blob).unwrap_err().code,
            crate::ErrorCode::Server
        );
        assert_eq!(
            err(Dialect::Cassandra, "0x123", CqlType::Blob),
            "cannot parse '123' as hex bytes"
        );
        assert_eq!(val(d, "-5", CqlType::Int)[..], (-5i32).to_be_bytes());
        assert_eq!(
            val(Dialect::Cassandra, "1 + 2", CqlType::Int)[..],
            3i32.to_be_bytes()
        );
        assert_eq!(
            err(d, "nofunc()", CqlType::Int),
            "Unknown function nofunc called"
        );
        assert_eq!(val(d, "(int) 5", CqlType::Int)[..], 5i32.to_be_bytes());
    }

    #[test]
    fn binds_validate_with_dialect_messages() {
        let t = Term::Bind {
            index: 0,
            ty: CqlType::Int,
            receiver: "v".into(),
        };
        let params = [Param::new(Bytes::from_static(&[1, 2]))];
        let e = t.eval(&EvalCtx::new(&params, Dialect::Scylla)).unwrap_err();
        assert!(
            e.message
                .starts_with("Exception while binding column v: marshaling error:"),
            "{}",
            e.message
        );
        let e = t
            .eval(&EvalCtx::new(&params, Dialect::Cassandra))
            .unwrap_err();
        assert!(!e.message.contains("binding"), "{}", e.message);
        assert_eq!(
            t.eval(&EvalCtx::new(&[Param::Unset], Dialect::Scylla))
                .unwrap(),
            Bound::Unset
        );
        assert_eq!(
            t.eval(&EvalCtx::new(&[Param::Null], Dialect::Scylla))
                .unwrap(),
            Bound::Null
        );
    }

    #[test]
    fn preferred_types() {
        let lit = |k, t: &'static str| ast::Literal {
            kind: k,
            text: t.into(),
            dollar: false,
        };
        assert_eq!(
            preferred_type(&lit(AstKind::Integer, "1")),
            Some(CqlType::Int)
        );
        assert_eq!(
            preferred_type(&lit(AstKind::Integer, "3000000000")),
            Some(CqlType::Bigint)
        );
        assert_eq!(
            preferred_type(&lit(AstKind::Integer, "99999999999999999999")),
            Some(CqlType::Varint)
        );
        assert_eq!(
            preferred_type(&lit(AstKind::String, "a")),
            Some(CqlType::Ascii)
        );
        assert_eq!(
            preferred_type(&lit(AstKind::String, "é")),
            Some(CqlType::Text)
        );
        assert_eq!(
            preferred_type(&lit(AstKind::Float, "1.5")),
            Some(CqlType::Double)
        );
        let mut v = Vec::new();
        put_unsigned_vint(&mut v, 300);
        assert_eq!(v, [0x81, 0x2c]);
    }
}
