//! Built-in functions and operators, and Postgres's overload resolution
//! (func_select_candidate / oper_select_candidate).

pub mod admin;
pub mod amutils;
pub mod amvalidate;
pub mod pgstatfuncs;
pub mod array;
pub mod bits;
pub mod btcmp;
pub mod datetime;
pub mod formatting;
pub mod geo;
pub mod json;
pub mod largeobj;
pub mod math;
pub mod monitor;
pub mod net;
pub mod opfuncs;
pub mod ops;
pub mod pg_oids;
pub mod privs;
pub mod pubsub;
pub mod range;
pub mod sqljson;
pub mod string;
pub mod system;
pub mod tsearch;
pub mod xact;
pub mod xml;
pub mod xmlmap;
pub mod xmltable;
pub mod walfuncs;

use crate::error::{Error, Result, code};
use crate::exec::ExecCtx;
use crate::sql::expr::Expr;
use crate::types::cast::{CastContext, cast_context, common_type};
use crate::types::{Category, DataType, Value};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

/// What a function implementation sees besides its argument values.
pub struct FnCtx<'a> {
    pub exec: &'a ExecCtx<'a>,
    pub ret: &'a DataType,
    pub args: &'a [Expr],
    /// The input collation (None: the database default).
    pub coll: Option<&'a crate::types::collation::Collation>,
}

impl FnCtx<'_> {
    pub fn arg_type(&self, i: usize) -> DataType {
        self.args
            .get(i)
            .map(|e| e.ty())
            .unwrap_or(DataType::Unknown)
    }
}

pub type FnImpl = fn(&[Value], &FnCtx) -> Result<Value>;

/// Rows produced by a set-returning function. Iterators own their state.
pub type SrfIter = Box<dyn Iterator<Item = Result<Vec<Value>>> + Send>;
pub type SrfImpl = fn(&[Value], &FnCtx) -> Result<SrfIter>;

fn no_scalar(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(crate::error::internal(
        "set-returning function called in a scalar context",
    ))
}

#[derive(Clone, Debug)]
pub enum Ret {
    Fixed(DataType),
    /// The resolved polymorphic type (anyelement / anycompatible).
    Poly,
    /// Array of the resolved polymorphic type.
    PolyArray,
    /// Same type as argument i (keeping typmod).
    Arg(usize),
}

#[derive(Clone)]
pub struct FuncDef {
    pub oid: u32,
    pub name: &'static str,
    pub args: Vec<DataType>,
    /// The last argument is VARIADIC (declared as an array type).
    pub variadic: bool,
    /// A variadic function called with its variadic arguments as one array (`VARIADIC arr`):
    /// the array's elements are passed as the arguments.
    pub spread: bool,
    pub ret: Ret,
    pub strict: bool,
    /// 'i' immutable, 's' stable, 'v' volatile.
    pub volatility: char,
    pub imp: FnImpl,
    /// Minimum number of arguments (for defaults).
    pub min_args: usize,
    /// Set-returning implementation.
    pub srf: Option<SrfImpl>,
    /// Output columns of a set-returning function with more than one column.
    pub out_cols: Vec<(&'static str, DataType)>,
    /// Parameter names (for named-argument calls like make_interval(days => 1)).
    pub arg_names: Vec<&'static str>,
    /// Default values of the last `defaults.len()` parameters (for omitted named arguments).
    pub defaults: Vec<Value>,
    /// The collation the call runs under (its input collation), when that isn't the
    /// database default.
    pub coll: Option<Arc<crate::types::collation::Collation>>,
}

impl FuncDef {
    /// This variadic function taking its variadic arguments as one array, as a call with
    /// `VARIADIC` passes them. A `VARIADIC "any"` parameter takes any array.
    pub fn spread(&self) -> FuncDef {
        let mut args = self.args.clone();
        if let Some(last) = args.last_mut()
            && matches!(last, DataType::Array(e) if matches!(**e, DataType::Any))
        {
            *last = DataType::AnyArray;
        }
        FuncDef {
            oid: self.oid,
            name: self.name,
            min_args: args.len(),
            args,
            variadic: false,
            spread: true,
            ret: self.ret.clone(),
            strict: self.strict,
            volatility: self.volatility,
            imp: self.imp,
            srf: self.srf,
            out_cols: self.out_cols.clone(),
            arg_names: self.arg_names.clone(),
            defaults: self.defaults.clone(),
            coll: self.coll.clone(),
        }
    }
}

impl std::fmt::Debug for FuncDef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}({:?})", self.name, self.args)
    }
}

#[derive(Default)]
pub struct Registry {
    pub funcs: HashMap<&'static str, Vec<Arc<FuncDef>>>,
    /// Binary operators by symbol.
    pub binops: HashMap<&'static str, Vec<Arc<FuncDef>>>,
    /// Prefix operators by symbol.
    pub prefix: HashMap<&'static str, Vec<Arc<FuncDef>>>,
    /// The operators' pg_operator rows, by (name, left type, right type) oids (left 0: prefix).
    pub opers: HashMap<(&'static str, u32, u32), OperRow>,
    next_oid: u32,
}

/// A built-in operator's pg_operator columns besides its name, types and function.
#[derive(Clone, Copy, Default)]
pub struct OperRow {
    pub oid: u32,
    pub com: u32,
    pub negate: u32,
    pub merges: bool,
    pub hashes: bool,
    pub rest: u32,
    pub join: u32,
}

/// The first oid for built-ins Postgres doesn't have: 8000-9999 is never used by a release.
const SYNTHETIC_OIDS: u32 = 8000;
/// ... and for such aggregates.
pub(crate) const SYNTHETIC_AGG_OIDS: u32 = 9900;

/// A parameter's type as pg_proc lists it: `VARIADIC "any"` is `any`, not an array of it.
pub fn param_oid(t: &DataType) -> u32 {
    match t {
        DataType::Array(e) if matches!(**e, DataType::Any) => crate::types::oid::ANY,
        t => t.oid(),
    }
}

fn param_oids(args: &[DataType]) -> Vec<u32> {
    args.iter().map(param_oid).collect()
}

/// The key of an operator definition in `Registry::opers`.
pub fn oper_key(name: &'static str, def: &FuncDef, prefix: bool) -> (&'static str, u32, u32) {
    if prefix {
        (name, 0, param_oid(&def.args[0]))
    } else {
        (name, param_oid(&def.args[0]), param_oid(&def.args[1]))
    }
}

/// Postgres's oid for each built-in function of `name` and argument types: an exact match, else
/// the only function of that name, else a synthetic one. Registration order breaks ties.
pub(crate) fn pg_proc_oids(sigs: &[(&'static str, Vec<u32>, u32)], next: &mut u32) -> Vec<u32> {
    use std::collections::HashSet;
    let mut exact: HashMap<(&str, &[u32]), u32> = HashMap::new();
    let mut by_name: HashMap<&str, Vec<u32>> = HashMap::new();
    for (oid, name, args) in pg_oids::PROCS {
        exact.insert((name, args), *oid);
        by_name.entry(name).or_default().push(*oid);
    }
    let mut out: Vec<Option<u32>> = vec![None; sigs.len()];
    let mut taken: HashSet<u32> = HashSet::new();
    // Already real (range constructors), then exact matches, then the only one of the name.
    for (i, (_, _, cur)) in sigs.iter().enumerate() {
        if *cur < 16384 && *cur != 0 && taken.insert(*cur) {
            out[i] = Some(*cur);
        }
    }
    for (i, (name, args, _)) in sigs.iter().enumerate() {
        if out[i].is_none()
            && let Some(o) = exact.get(&(*name, args.as_slice()))
            && taken.insert(*o)
        {
            out[i] = Some(*o);
        }
    }
    for (i, (name, _, _)) in sigs.iter().enumerate() {
        if out[i].is_none()
            && let Some([o]) = by_name.get(name).map(Vec::as_slice)
            && taken.insert(*o)
        {
            out[i] = Some(*o);
        }
    }
    out.into_iter()
        .map(|o| {
            o.unwrap_or_else(|| {
                *next += 1;
                *next - 1
            })
        })
        .collect()
}

impl Registry {
    /// Gives every built-in Postgres's own oid (pg_oids), operators their pg_operator rows,
    /// and registers each operator's implementation function under its pg_proc name when no
    /// function by that oid exists yet (`int4pl`, `network_sub`).
    fn assign_pg_oids(&mut self) {
        // Functions, in registration order.
        let mut funcs: Vec<(&'static str, usize, u32)> = self
            .funcs
            .iter()
            .flat_map(|(n, v)| v.iter().enumerate().map(move |(i, d)| (*n, i, d.oid)))
            .collect();
        funcs.sort_by_key(|f| f.2);
        let sigs: Vec<(&'static str, Vec<u32>, u32)> = funcs
            .iter()
            .map(|(n, i, cur)| (*n, param_oids(&self.funcs[n][*i].args), *cur))
            .collect();
        let mut next = SYNTHETIC_OIDS;
        let oids = pg_proc_oids(&sigs, &mut next);
        for ((n, i, _), oid) in funcs.iter().zip(oids) {
            Arc::get_mut(&mut self.funcs.get_mut(n).unwrap()[*i])
                .unwrap()
                .oid = oid;
        }
        // Operators: the implementing function's oid, and the pg_operator row.
        let pg: HashMap<(&str, u32, u32), &pg_oids::Oper> = pg_oids::OPERS
            .iter()
            .map(|o| ((o.name, o.left, o.right), o))
            .collect();
        let names: HashMap<u32, (&'static str, &'static [u32])> = pg_oids::PROCS
            .iter()
            .map(|(o, n, a)| (*o, (*n, *a)))
            .collect();
        let mut ops: Vec<(bool, &'static str, usize, u32)> = self
            .binops
            .iter()
            .map(|(n, v)| (false, n, v))
            .chain(self.prefix.iter().map(|(n, v)| (true, n, v)))
            .flat_map(|(p, n, v)| v.iter().enumerate().map(move |(i, d)| (p, *n, i, d.oid)))
            .collect();
        ops.sort_by_key(|o| o.3);
        let mut have: std::collections::HashSet<u32> =
            self.funcs.values().flatten().map(|d| d.oid).collect();
        for (prefix, name, i, _) in ops {
            let list = if prefix {
                self.prefix.get_mut(name)
            } else {
                self.binops.get_mut(name)
            }
            .unwrap();
            let key = oper_key(name, &list[i], prefix);
            let (row, code) = match pg.get(&(key.0, key.1, key.2)) {
                Some(o) => (
                    OperRow {
                        oid: o.oid,
                        com: o.com,
                        negate: o.negate,
                        merges: o.merges,
                        hashes: o.hashes,
                        rest: o.rest,
                        join: o.join,
                    },
                    o.code,
                ),
                None => {
                    next += 2;
                    (
                        OperRow {
                            oid: next - 2,
                            ..Default::default()
                        },
                        next - 1,
                    )
                }
            };
            let d = Arc::get_mut(&mut list[i]).unwrap();
            d.oid = code;
            let def = d.clone();
            self.opers.insert(key, row);
            if let Some((fname, fargs)) = names.get(&code)
                && have.insert(code)
            {
                let args: Option<Vec<DataType>> =
                    fargs.iter().map(|o| DataType::from_oid(*o)).collect();
                let args = args
                    .filter(|a| a.len() == def.args.len())
                    .unwrap_or(def.args);
                let list = self.funcs.entry(fname).or_default();
                if !list
                    .iter()
                    .any(|f| param_oids(&f.args) == param_oids(&args))
                {
                    list.push(Arc::new(FuncDef {
                        name: fname,
                        min_args: args.len(),
                        args,
                        ..def
                    }));
                }
            }
        }
        self.next_oid = next;
    }

    fn mk(&mut self, name: &'static str, args: Vec<DataType>, ret: Ret, imp: FnImpl) -> FuncDef {
        self.next_oid += 1;
        let n = args.len();
        FuncDef {
            oid: self.next_oid,
            name,
            args,
            variadic: false,
            spread: false,
            ret,
            strict: true,
            volatility: 'i',
            imp,
            min_args: n,
            srf: None,
            out_cols: Vec::new(),
            arg_names: Vec::new(),
            defaults: vec![],
            coll: None,
        }
    }

    /// Register an immutable, strict function.
    pub fn func(
        &mut self,
        name: &'static str,
        args: Vec<DataType>,
        ret: DataType,
        imp: FnImpl,
    ) -> &mut FuncDef {
        let d = self.mk(name, args, Ret::Fixed(ret), imp);
        self.add_func(d)
    }

    pub fn func_ret(
        &mut self,
        name: &'static str,
        args: Vec<DataType>,
        ret: Ret,
        imp: FnImpl,
    ) -> &mut FuncDef {
        let d = self.mk(name, args, ret, imp);
        self.add_func(d)
    }

    fn add_func(&mut self, d: FuncDef) -> &mut FuncDef {
        let list = self.funcs.entry(d.name).or_default();
        list.push(Arc::new(d));
        Arc::get_mut(list.last_mut().unwrap()).unwrap()
    }

    /// Register built-in function `oid` with the signature pg_proc gives it: `imp` for a
    /// scalar, or `srf` for one returning rows (a set, or a record of OUT columns).
    pub fn from_pg(&mut self, oid: u32, imp: Option<FnImpl>, srf: Option<SrfImpl>) -> &mut FuncDef {
        let sig = crate::catalog::system::builtin_proc_sigs()
            .iter()
            .find(|s| s.oid == oid)
            .unwrap_or_else(|| panic!("no built-in function {oid}"));
        let ty = |o: u32| DataType::from_oid(o).unwrap_or(DataType::Unknown);
        let ret = if sig.ret == crate::types::oid::RECORD && !sig.out.is_empty() {
            DataType::Record
        } else {
            ty(sig.ret)
        };
        let mut d = self.mk(
            sig.name,
            sig.args.iter().map(|a| ty(*a)).collect(),
            Ret::Fixed(ret),
            imp.unwrap_or(no_scalar),
        );
        d.srf = srf;
        if sig.out.len() > 1 {
            d.out_cols = sig
                .out
                .iter()
                .map(|(n, t)| (&*Box::leak(n.clone().into_boxed_str()), ty(*t)))
                .collect();
        }
        d.strict = sig.strict;
        d.volatility = sig.volatility;
        d.min_args = sig.args.len() - sig.nargdefaults;
        self.add_func(d)
    }

    /// Register a set-returning function with one output column.
    pub fn srf(
        &mut self,
        name: &'static str,
        args: Vec<DataType>,
        ret: DataType,
        imp: SrfImpl,
    ) -> &mut FuncDef {
        let mut d = self.mk(name, args, Ret::Fixed(ret), no_scalar);
        d.srf = Some(imp);
        self.add_func(d)
    }

    pub fn srf_ret(
        &mut self,
        name: &'static str,
        args: Vec<DataType>,
        ret: Ret,
        imp: SrfImpl,
    ) -> &mut FuncDef {
        let mut d = self.mk(name, args, ret, no_scalar);
        d.srf = Some(imp);
        self.add_func(d)
    }

    /// Register a set-returning function with several named output columns.
    pub fn srf_cols(
        &mut self,
        name: &'static str,
        args: Vec<DataType>,
        cols: Vec<(&'static str, DataType)>,
        imp: SrfImpl,
    ) -> &mut FuncDef {
        let mut d = self.mk(name, args, Ret::Fixed(DataType::Record), no_scalar);
        d.srf = Some(imp);
        d.out_cols = cols;
        self.add_func(d)
    }

    pub fn binop(
        &mut self,
        op: &'static str,
        l: DataType,
        r: DataType,
        ret: DataType,
        imp: FnImpl,
    ) {
        let d = self.mk(op, vec![l, r], Ret::Fixed(ret), imp);
        self.binops.entry(op).or_default().push(Arc::new(d));
    }

    pub fn binop_ret(&mut self, op: &'static str, l: DataType, r: DataType, ret: Ret, imp: FnImpl) {
        let d = self.mk(op, vec![l, r], ret, imp);
        self.binops.entry(op).or_default().push(Arc::new(d));
    }

    pub fn prefix_op(&mut self, op: &'static str, arg: DataType, ret: DataType, imp: FnImpl) {
        let d = self.mk(op, vec![arg], Ret::Fixed(ret), imp);
        self.prefix.entry(op).or_default().push(Arc::new(d));
    }
}

impl FuncDef {
    pub fn volatile(&mut self) -> &mut Self {
        self.volatility = 'v';
        self
    }

    pub fn stable(&mut self) -> &mut Self {
        self.volatility = 's';
        self
    }

    pub fn non_strict(&mut self) -> &mut Self {
        self.strict = false;
        self
    }

    pub fn variadic(&mut self) -> &mut Self {
        self.variadic = true;
        self
    }

    pub fn min_args(&mut self, n: usize) -> &mut Self {
        self.min_args = n;
        self
    }

    pub fn names(&mut self, names: &[&'static str]) -> &mut Self {
        self.arg_names = names.to_vec();
        self
    }

    /// Default values of the trailing parameters.
    pub fn defaults(&mut self, values: Vec<Value>) -> &mut Self {
        self.defaults = values;
        self
    }

    /// Output column name of a single-column set-returning function.
    pub fn out_name(&mut self, name: &'static str) -> &mut Self {
        let ty = match &self.ret {
            Ret::Fixed(t) => t.clone(),
            _ => DataType::AnyElement,
        };
        self.out_cols = vec![(name, ty)];
        self
    }

    pub fn is_srf(&self) -> bool {
        self.srf.is_some()
    }
}

pub fn registry() -> &'static Registry {
    static R: OnceLock<Registry> = OnceLock::new();
    R.get_or_init(|| {
        let mut r = Registry {
            next_oid: 100_000,
            ..Default::default()
        };
        ops::register(&mut r);
        math::register(&mut r);
        string::register(&mut r);
        datetime::register(&mut r);
        json::register(&mut r);
        array::register(&mut r);
        system::register(&mut r);
        privs::register(&mut r);
        largeobj::register(&mut r);
        admin::register(&mut r);
        amutils::register(&mut r);
        pgstatfuncs::register(&mut r);
        btcmp::register(&mut r);
        range::register(&mut r);
        geo::register(&mut r);
        bits::register(&mut r);
        net::register(&mut r);
        tsearch::register(&mut r);
        xml::register(&mut r);
        xmlmap::register(&mut r);
        monitor::register(&mut r);
        walfuncs::register(&mut r);
        pubsub::register(&mut r);
        xact::register(&mut r);
        register_type_io(&mut r);
        // Last: operator functions are the operators' implementations.
        opfuncs::register(&mut r);
        r.assign_pg_oids();
        r
    })
}

/// The built-in types' text I/O functions, callable directly as in Postgres
/// (`tsvectorin(tsvectorout(v))`, `int4in('5')`): the input function parses its cstring as
/// the type, the output function prints its argument. Polymorphic and pseudo-type ones
/// (array_in, record_out, enum_in, ...) and names registered already are left out.
fn register_type_io(r: &mut Registry) {
    let mut seen = std::collections::HashSet::new();
    let sigs = crate::catalog::system::builtin_proc_sigs();
    for (typ, procs) in crate::catalog::type_procs::TYPE_PROCS {
        let Some(t) = DataType::from_oid(*typ) else {
            continue;
        };
        if matches!(t.category(), Category::Pseudo | Category::Unknown) || t.is_polymorphic() {
            continue;
        }
        for (oid, imp) in [(procs[1], type_input as FnImpl), (procs[2], type_output)] {
            if oid == 0 || !seen.insert(oid) {
                continue;
            }
            let Some(sig) = sigs.iter().find(|s| s.oid == oid) else {
                continue;
            };
            let concrete = |o: &u32| {
                DataType::from_oid(*o).is_some_and(|d| !d.is_polymorphic() && d != DataType::Unknown)
            };
            if r.funcs.contains_key(sig.name) || !sig.args.iter().all(concrete) || !concrete(&sig.ret) {
                continue;
            }
            r.from_pg(oid, Some(imp), None);
        }
    }
}

fn type_input(a: &[Value], c: &FnCtx) -> Result<Value> {
    crate::types::text::parse_value(text_arg(&a[0]), c.ret, c.exec.fmt())
}

fn type_output(a: &[Value], c: &FnCtx) -> Result<Value> {
    Ok(Value::text(crate::types::text::format_value(
        &a[0],
        &c.arg_type(0),
        c.exec.fmt(),
    )))
}

/// A built-in operator, as pg_operator lists it.
pub struct OperInfo {
    pub oid: u32,
    pub name: &'static str,
    /// None: a prefix operator.
    pub left: Option<DataType>,
    pub right: DataType,
}

/// A built-in operator definition's pg_operator row.
pub fn oper_row(name: &str, def: &FuncDef) -> OperRow {
    let r = registry();
    let Some((name, _)) = r
        .binops
        .get_key_value(name)
        .or_else(|| r.prefix.get_key_value(name))
    else {
        return OperRow::default();
    };
    let prefix = def.args.len() == 1;
    r.opers
        .get(&oper_key(name, def, prefix))
        .copied()
        .unwrap_or_default()
}

/// The built-in operators, by name.
pub fn operators_named(name: &str) -> Vec<OperInfo> {
    let r = registry();
    let key = |prefix| {
        if prefix {
            r.prefix.get_key_value(name)
        } else {
            r.binops.get_key_value(name)
        }
    };
    let mut out = Vec::new();
    for prefix in [false, true] {
        if let Some((k, defs)) = key(prefix) {
            for d in defs {
                let oid = r.opers.get(&oper_key(k, d, prefix)).map_or(0, |o| o.oid);
                let (left, right) = if prefix {
                    (None, d.args[0].clone())
                } else {
                    (Some(d.args[0].clone()), d.args[1].clone())
                };
                out.push(OperInfo {
                    oid,
                    name: k,
                    left,
                    right,
                });
            }
        }
    }
    out
}

/// A built-in operator by its pg_operator oid.
pub fn operator_by_oid(oid: u32) -> Option<OperInfo> {
    static BY_OID: OnceLock<HashMap<u32, &'static str>> = OnceLock::new();
    let names = BY_OID.get_or_init(|| {
        registry()
            .opers
            .iter()
            .map(|((n, _, _), o)| (o.oid, *n))
            .collect()
    });
    let name = names.get(&oid)?;
    operators_named(name).into_iter().find(|o| o.oid == oid)
}

/// A resolved call: the definition, the types each argument must be coerced to, and the result.
pub struct Resolved {
    pub def: Arc<FuncDef>,
    pub arg_types: Vec<DataType>,
    pub ret: DataType,
}

fn is_unknown(t: &DataType) -> bool {
    matches!(t, DataType::Unknown)
}

/// Whether `input` can be passed where `param` is declared (implicitly).
pub(crate) fn can_coerce(input: &DataType, param: &DataType) -> bool {
    if is_unknown(input) {
        return true;
    }
    if param.is_polymorphic() || matches!(param, DataType::Any) {
        return match param {
            DataType::AnyArray => poly_elem(input).is_some() || matches!(input, DataType::AnyArray),
            DataType::AnyCompatibleArray => poly_elem(input).is_some(),
            DataType::AnyRange | DataType::AnyCompatibleRange => {
                matches!(input, DataType::Range(_))
            }
            DataType::AnyMultirange | DataType::AnyCompatibleMultirange => {
                matches!(input, DataType::Multirange(_))
            }
            DataType::AnyNonArray | DataType::AnyCompatibleNonArray => poly_elem(input).is_none(),
            DataType::AnyEnum => matches!(input, DataType::Enum(..)),
            _ => true,
        };
    }
    if let (DataType::Array(_), DataType::Array(_)) = (input, param) {
        return input.same_base(param) || cast_context(input, param) == Some(CastContext::Implicit);
    }
    // A record goes to any composite type (`can_coerce_type`; checked when coerced).
    if matches!(input, DataType::Row(r) if r.oid == 0) && param.row_oid().is_some() {
        return true;
    }
    // A table's row type goes to an ancestor's (typeInheritsFrom) and a typed table's to
    // its type (typeIsOfTypedTable).
    if let (DataType::Row(f), DataType::Row(t)) = (input, param)
        && f.oid != 0
        && t.oid != 0
        && f.oid != t.oid
        && crate::session::current::with_current(|c| {
            c.txn
                .try_borrow()
                .is_ok_and(|x| row_type_inherits(&x.catalog(), f.oid, t.oid, 0))
        }) == Some(true)
    {
        return true;
    }
    input.same_base(param) || cast_context(input, param) == Some(CastContext::Implicit)
}

/// Whether the table whose row type is `child` inherits (through any depth) from the one
/// whose row type is `parent`, or is a typed table of type `parent`.
fn row_type_inherits(cat: &crate::catalog::Catalog, child: u32, parent: u32, depth: usize) -> bool {
    if depth > 64 {
        return false;
    }
    let Some(t) = cat.tables.values().find(|t| t.rowtype == child) else {
        return false;
    };
    if t.of_type == parent {
        return true;
    }
    t.inherits.iter().any(|p| {
        cat.tables.get(p).is_some_and(|pt| {
            pt.rowtype == parent || row_type_inherits(cat, pt.rowtype, parent, depth + 1)
        })
    })
}

/// The element type an array-like input gives `anyarray` (int2vector / oidvector count too).
fn poly_elem(t: &DataType) -> Option<DataType> {
    match t {
        DataType::Int2Vector => Some(DataType::Int2),
        DataType::OidVector => Some(DataType::Oid),
        t => t.elem().cloned(),
    }
}

/// Expand a variadic candidate's parameter list to `n` arguments.
fn params_for(def: &FuncDef, n: usize) -> Option<Vec<DataType>> {
    if def.variadic {
        let fixed = def.args.len() - 1;
        if n < def.min_args.max(fixed) {
            return None;
        }
        let elem = match &def.args[fixed] {
            DataType::Array(e) => (**e).clone(),
            DataType::AnyArray => DataType::AnyElement,
            DataType::AnyCompatibleArray => DataType::AnyCompatible,
            t => t.clone(),
        };
        let mut p = def.args[..fixed].to_vec();
        p.extend(std::iter::repeat_n(elem, n - fixed));
        Some(p)
    } else if n <= def.args.len() && n >= def.min_args {
        Some(def.args[..n].to_vec())
    } else {
        None
    }
}

/// What polymorphic parameters resolved to: the element type, and the range type when a
/// range or multirange is involved, for the anyelement family and (separately) the
/// anycompatible family.
#[derive(Clone, Default)]
struct Poly {
    elem: Option<DataType>,
    range: Option<Arc<crate::types::range::RangeType>>,
    /// An input declared `anyarray` itself (a statistics column) passed for `anyarray`.
    pseudo_array: bool,
    compat: Option<DataType>,
    compat_range: Option<Arc<crate::types::range::RangeType>>,
}

impl Poly {
    fn elem_or_text(&self) -> DataType {
        self.elem.clone().unwrap_or(DataType::Text)
    }

    fn compat_or_text(&self) -> DataType {
        self.compat.clone().unwrap_or(DataType::Text)
    }

    /// Either family's type (a built-in declared with `Ret::Poly`).
    fn any_or_text(&self) -> DataType {
        self.elem
            .clone()
            .or_else(|| self.compat.clone())
            .unwrap_or(DataType::Text)
    }

    fn range_type(&self) -> Option<Arc<crate::types::range::RangeType>> {
        self.range.clone().or_else(|| {
            self.elem
                .as_ref()
                .and_then(crate::types::range::builtin_for_subtype)
        })
    }

    fn compat_range_type(&self) -> Option<Arc<crate::types::range::RangeType>> {
        self.compat_range.clone().or_else(|| {
            self.compat
                .as_ref()
                .and_then(crate::types::range::builtin_for_subtype)
        })
    }
}

/// Resolve polymorphic parameters against the actual inputs. Returns the concrete parameter
/// types and what the polymorphic types stand for, or None if the inputs are inconsistent
/// (`check_generic_type_consistency`).
fn resolve_poly(params: &[DataType], inputs: &[DataType]) -> Option<(Vec<DataType>, Poly)> {
    let mut elem: Option<DataType> = None;
    let mut range: Option<Arc<crate::types::range::RangeType>> = None;
    let mut compat: Vec<DataType> = Vec::new();
    let mut compat_range: Option<Arc<crate::types::range::RangeType>> = None;
    let mut pseudo_array = false;
    let set_elem = |elem: &mut Option<DataType>, e: DataType| -> Option<()> {
        match elem {
            None => *elem = Some(e),
            Some(x) if x.same_base(&e) => {}
            Some(_) => return None,
        }
        Some(())
    };
    for (p, i) in params.iter().zip(inputs) {
        if is_unknown(i) {
            continue;
        }
        match p {
            DataType::AnyElement | DataType::AnyNonArray | DataType::AnyEnum => {
                set_elem(&mut elem, i.without_typmod())?
            }
            DataType::AnyArray if matches!(i, DataType::AnyArray) => pseudo_array = true,
            DataType::AnyArray => set_elem(&mut elem, poly_elem(i)?.without_typmod())?,
            DataType::AnyRange | DataType::AnyMultirange => {
                let (DataType::Range(rt) | DataType::Multirange(rt)) = i else {
                    return None;
                };
                if matches!(
                    (p, i),
                    (DataType::AnyRange, DataType::Multirange(_))
                        | (DataType::AnyMultirange, DataType::Range(_))
                ) {
                    return None;
                }
                match &range {
                    None => range = Some(rt.clone()),
                    Some(r) if r == rt => {}
                    Some(_) => return None,
                }
                set_elem(&mut elem, rt.subtype.clone())?;
            }
            DataType::AnyCompatibleRange | DataType::AnyCompatibleMultirange => {
                let (DataType::Range(rt) | DataType::Multirange(rt)) = i else {
                    return None;
                };
                match &compat_range {
                    None => compat_range = Some(rt.clone()),
                    Some(r) if r == rt => {}
                    Some(_) => return None,
                }
            }
            DataType::AnyCompatible => compat.push(i.clone()),
            DataType::AnyCompatibleNonArray if poly_elem(i).is_some() => return None,
            DataType::AnyCompatibleNonArray => compat.push(i.clone()),
            DataType::AnyCompatibleArray => compat.push(poly_elem(i)?),
            _ => {}
        }
    }
    // anycompatiblerange fixes the common type to its subtype: the others must coerce to it.
    let compat = match &compat_range {
        Some(rt) => {
            if !compat.iter().all(|c| can_coerce(c, &rt.subtype)) {
                return None;
            }
            Some(rt.subtype.clone())
        }
        None if compat.is_empty() => None,
        None => Some(common_type(&compat, "argument").ok()?),
    };
    // anyarray for anyarray only when nothing else is polymorphic.
    if pseudo_array && (elem.is_some() || range.is_some()) {
        return None;
    }
    let poly = Poly {
        elem,
        range,
        pseudo_array,
        compat,
        compat_range,
    };
    let concrete = params.iter().map(|p| concrete_param(p, &poly)).collect();
    Some((concrete, poly))
}

/// A parameter's type once its polymorphic types are known (unresolvable ones stay as they are).
fn concrete_param(p: &DataType, poly: &Poly) -> DataType {
    match p {
        DataType::AnyElement | DataType::AnyNonArray | DataType::AnyEnum => poly.elem_or_text(),
        DataType::AnyCompatible | DataType::AnyCompatibleNonArray => poly.compat_or_text(),
        DataType::AnyArray if poly.pseudo_array => DataType::AnyArray,
        DataType::AnyArray => DataType::array_of(poly.elem_or_text()),
        DataType::AnyCompatibleArray => DataType::array_of(poly.compat_or_text()),
        DataType::AnyRange => poly
            .range
            .clone()
            .map_or_else(|| p.clone(), DataType::Range),
        DataType::AnyMultirange => poly
            .range
            .clone()
            .map_or_else(|| p.clone(), DataType::Multirange),
        DataType::AnyCompatibleRange => poly
            .compat_range
            .clone()
            .map_or_else(|| p.clone(), DataType::Range),
        DataType::AnyCompatibleMultirange => poly
            .compat_range
            .clone()
            .map_or_else(|| p.clone(), DataType::Multirange),
        p => p.clone(),
    }
}

fn ret_type(def: &FuncDef, concrete: &[DataType], poly: &Poly, inputs: &[DataType]) -> DataType {
    match &def.ret {
        Ret::Fixed(t) => match t {
            DataType::AnyElement | DataType::AnyNonArray => poly.elem_or_text(),
            DataType::AnyCompatible | DataType::AnyCompatibleNonArray => poly.compat_or_text(),
            DataType::AnyArray if poly.pseudo_array => DataType::AnyArray,
            // (array_in: nothing to resolve it from.)
            DataType::AnyArray if !def.args.iter().any(DataType::is_polymorphic) => {
                DataType::AnyArray
            }
            DataType::AnyArray => DataType::array_of(poly.elem_or_text()),
            DataType::AnyCompatibleArray => DataType::array_of(poly.compat_or_text()),
            DataType::AnyRange => poly.range_type().map_or(DataType::Text, DataType::Range),
            DataType::AnyMultirange => poly
                .range_type()
                .map_or(DataType::Text, DataType::Multirange),
            DataType::AnyCompatibleRange => poly
                .compat_range_type()
                .map_or(DataType::Text, DataType::Range),
            DataType::AnyCompatibleMultirange => poly
                .compat_range_type()
                .map_or(DataType::Text, DataType::Multirange),
            t => t.clone(),
        },
        Ret::Poly => poly.any_or_text(),
        Ret::PolyArray => DataType::array_of(poly.any_or_text()),
        Ret::Arg(i) => match inputs.get(*i) {
            Some(t) if !is_unknown(t) => t.clone(),
            _ => concrete.get(*i).cloned().unwrap_or(DataType::Text),
        },
    }
}

/// `select_candidate`'s answer when step 2 decides it (exactly one exact match), found
/// without building every candidate's parameter list: for inputs of known types, when that
/// one exact match is a plain (non-variadic) candidate. None: run the full rules.
fn select_exact(cands: &[Arc<FuncDef>], inputs: &[DataType]) -> Option<Resolved> {
    if inputs.iter().any(is_unknown) {
        return None;
    }
    let n = inputs.len();
    let mut found: Option<&Arc<FuncDef>> = None;
    for c in cands {
        if c.variadic {
            // (A variadic expansion could match exactly too, and then lose to a plain
            // function: leave that to the full rules.)
            let exact = params_for(c, n).is_some_and(|p| {
                !c.args.iter().any(DataType::is_polymorphic)
                    && p.iter().zip(inputs).all(|(p, i)| p.same_base(i))
            });
            if exact {
                return None;
            }
            continue;
        }
        if n > c.args.len() || n < c.min_args {
            continue;
        }
        let params = &c.args[..n];
        // A non-polymorphic candidate's concrete parameters are its declared ones.
        if c.args.iter().any(DataType::is_polymorphic)
            || !params.iter().zip(inputs).all(|(p, i)| p.same_base(i))
            || !params.iter().zip(inputs).all(|(p, i)| can_coerce(i, p))
        {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(c);
    }
    let def = found?.clone();
    let concrete = def.args[..n].to_vec();
    let poly = Poly::default();
    let ret = ret_type(&def, &concrete, &poly, inputs);
    Some(Resolved {
        def,
        arg_types: concrete,
        ret,
    })
}

/// Pick the best candidate for `inputs`, following Postgres's rules.
pub fn select_candidate(
    cands: &[Arc<FuncDef>],
    inputs: &[DataType],
    is_binop: bool,
) -> Result<Option<Resolved>, Vec<Arc<FuncDef>>> {
    if let Some(r) = select_exact(cands, inputs) {
        return Ok(Some(r));
    }
    // Applicable candidates with their concrete parameter lists.
    let mut viable: Vec<(Arc<FuncDef>, Vec<DataType>, Poly)> = Vec::new();
    for c in cands {
        let Some(params) = params_for(c, inputs.len()) else {
            continue;
        };
        let Some((concrete, poly)) = resolve_poly(&params, inputs) else {
            continue;
        };
        if params.iter().zip(inputs).all(|(p, i)| can_coerce(i, p)) {
            viable.push((c.clone(), concrete, poly));
        }
    }
    if viable.is_empty() {
        return Ok(None);
    }
    // A variadic expansion that matches a plain function's signature loses to it.
    if viable.iter().any(|v| !v.0.variadic) {
        let plain: Vec<Vec<DataType>> = viable
            .iter()
            .filter(|v| !v.0.variadic)
            .map(|v| v.1.clone())
            .collect();
        viable.retain(|v| {
            !v.0.variadic
                || !plain.iter().any(|p| {
                    p.len() == v.1.len() && p.iter().zip(&v.1).all(|(a, b)| a.same_base(b))
                })
        });
    }
    let finish = |(def, concrete, poly): (Arc<FuncDef>, Vec<DataType>, Poly)| {
        let ret = ret_type(&def, &concrete, &poly, inputs);
        Resolved {
            def,
            arg_types: concrete,
            ret,
        }
    };
    // 2. Exact match (an unknown binop argument assumed to be the other's type).
    let effective: Vec<DataType> = if is_binop && inputs.len() == 2 {
        match (is_unknown(&inputs[0]), is_unknown(&inputs[1])) {
            (true, false) => vec![inputs[1].clone(), inputs[1].clone()],
            (false, true) => vec![inputs[0].clone(), inputs[0].clone()],
            _ => inputs.to_vec(),
        }
    } else {
        inputs.to_vec()
    };
    let exact: Vec<_> = viable
        .iter()
        .filter(|(d, concrete, _)| {
            !d.args.iter().any(|a| a.is_polymorphic())
                && concrete
                    .iter()
                    .zip(&effective)
                    .all(|(p, i)| !is_unknown(i) && p.same_base(i))
        })
        .cloned()
        .collect();
    if exact.len() == 1 {
        return Ok(Some(finish(exact.into_iter().next().unwrap())));
    }
    if viable.len() == 1 {
        return Ok(Some(finish(viable.into_iter().next().unwrap())));
    }
    // 3c. Most exact matches (of the declared types: a polymorphic parameter matches none).
    let poly_at = |def: &FuncDef, pos: usize| {
        params_for(def, inputs.len())
            .and_then(|p| p.get(pos).cloned())
            .is_some_and(|p| p.is_polymorphic())
    };
    let exact_count = |v: &(Arc<FuncDef>, Vec<DataType>, Poly)| {
        v.1.iter()
            .zip(inputs)
            .enumerate()
            .filter(|(pos, (p, i))| !is_unknown(i) && !poly_at(&v.0, *pos) && p.same_base(i))
            .count()
    };
    let best = viable.iter().map(exact_count).max().unwrap_or(0);
    let mut cands: Vec<_> = viable
        .into_iter()
        .filter(|v| exact_count(v) == best)
        .collect();
    if cands.len() == 1 {
        return Ok(Some(finish(cands.pop().unwrap())));
    }
    // 3d. Most preferred types where coercion is needed.
    let pref_count = |v: &(Arc<FuncDef>, Vec<DataType>, Poly)| {
        v.1.iter()
            .zip(inputs)
            .enumerate()
            .filter(|(pos, (p, i))| {
                !is_unknown(i)
                    && !poly_at(&v.0, *pos)
                    && !p.same_base(i)
                    && p.is_preferred()
                    && p.category() == i.category()
            })
            .count()
    };
    let best = cands.iter().map(pref_count).max().unwrap_or(0);
    if best > 0 {
        cands.retain(|v| pref_count(v) == best);
    }
    if cands.len() == 1 {
        return Ok(Some(finish(cands.pop().unwrap())));
    }
    // 3e. Unknown arguments: a category per position (STRING if any candidate takes it,
    // else the one all candidates agree on), and whether a candidate takes a preferred type
    // of it there; then candidates off any of them go (all are kept if none is left).
    if inputs.iter().any(is_unknown) {
        // A polymorphic parameter counts as its pseudo-type (not preferred).
        let declared = |c: &(Arc<FuncDef>, Vec<DataType>, Poly), pos: usize| {
            params_for(&c.0, inputs.len()).and_then(|p| p.get(pos).cloned())
        };
        let cat_at = |c: &(Arc<FuncDef>, Vec<DataType>, Poly), pos: usize| match declared(c, pos) {
            Some(p) if p.is_polymorphic() => Category::Pseudo,
            _ => c.1[pos].category(),
        };
        let pref_at = |c: &(Arc<FuncDef>, Vec<DataType>, Poly), pos: usize| match declared(c, pos) {
            Some(p) if p.is_polymorphic() => false,
            _ => c.1[pos].is_preferred(),
        };
        let mut slots: Vec<(usize, Category, bool)> = Vec::new();
        let mut resolved = true;
        for (pos, _) in inputs.iter().enumerate().filter(|(_, t)| is_unknown(t)) {
            let cats: Vec<Category> = cands.iter().map(|c| cat_at(c, pos)).collect();
            let cat = if cats.contains(&Category::String) {
                Category::String
            } else if cats.iter().all(|c| *c == cats[0]) {
                cats[0]
            } else {
                resolved = false;
                break;
            };
            let pref = cands
                .iter()
                .any(|c| cat_at(c, pos) == cat && pref_at(c, pos));
            slots.push((pos, cat, pref));
        }
        if resolved {
            let kept: Vec<_> = cands
                .iter()
                .filter(|c| {
                    slots.iter().all(|&(pos, cat, pref)| {
                        cat_at(c, pos) == cat && (!pref || pref_at(c, pos))
                    })
                })
                .cloned()
                .collect();
            if !kept.is_empty() {
                cands = kept;
            }
            if cands.len() == 1 {
                return Ok(Some(finish(cands.pop().unwrap())));
            }
        }
        // 3f. Unknowns assumed to be the (single) known type.
        let known: Vec<&DataType> = inputs.iter().filter(|t| !is_unknown(t)).collect();
        if !known.is_empty() && known.iter().all(|t| t.same_base(known[0])) {
            let k = known[0].clone();
            // (can_coerce_type over all the inputs: polymorphic parameters consistent too)
            let subst: Vec<DataType> = inputs
                .iter()
                .map(|i| if is_unknown(i) { k.clone() } else { i.clone() })
                .collect();
            let fits: Vec<_> = cands
                .iter()
                .filter(|c| {
                    c.1.iter()
                        .zip(inputs)
                        .all(|(p, i)| !is_unknown(i) || can_coerce(&k, p))
                        && params_for(&c.0, inputs.len()).is_some_and(|decl| {
                            !decl.iter().any(|p| p.is_polymorphic())
                                || resolve_poly(&decl, &subst).is_some_and(|(conc, _)| {
                                    conc.iter().zip(&subst).all(|(p, i)| can_coerce(i, p))
                                })
                        })
                })
                .cloned()
                .collect();
            if fits.len() == 1 {
                return Ok(Some(finish(fits.into_iter().next().unwrap())));
            }
        }
    }
    // Prefer non-polymorphic candidates when still tied (not for an internal-category
    // input, like "char" or pg_node_tree, which Postgres finds ambiguous).
    if inputs.iter().any(|t| t.category() == Category::Internal) {
        return Err(cands.into_iter().map(|c| c.0).collect());
    }
    let concrete: Vec<_> = cands
        .iter()
        .filter(|c| !c.0.args.iter().any(|a| a.is_polymorphic()))
        .cloned()
        .collect();
    if concrete.len() == 1 {
        return Ok(Some(finish(concrete.into_iter().next().unwrap())));
    }
    Err(cands.into_iter().map(|c| c.0).collect())
}

/// Argument types as error messages show them (format_type_be: no typmods).
pub fn type_list(inputs: &[DataType]) -> String {
    inputs
        .iter()
        .map(|t| t.without_typmod().display())
        .collect::<Vec<_>>()
        .join(", ")
}

pub fn resolve_function(name: &str, inputs: &[DataType]) -> Result<Resolved> {
    resolve_among(
        registry().funcs.get(name).map_or(&[][..], Vec::as_slice),
        name,
        inputs,
        false,
    )
}

/// Resolve a call whose variadic arguments are passed as one array (`VARIADIC arr`).
pub fn resolve_spread_call(name: &str, inputs: &[DataType]) -> Result<Resolved> {
    resolve_among(
        registry().funcs.get(name).map_or(&[][..], Vec::as_slice),
        name,
        inputs,
        true,
    )
}

/// Resolve a call of `name` among `cands`; `spread`: called with `VARIADIC arr`.
pub fn resolve_among(
    cands: &[Arc<FuncDef>],
    name: &str,
    inputs: &[DataType],
    spread: bool,
) -> Result<Resolved> {
    let spread_cands: Vec<Arc<FuncDef>>;
    let cands = if spread {
        spread_cands = cands
            .iter()
            .map(|d| {
                if d.variadic {
                    Arc::new(d.spread())
                } else {
                    d.clone()
                }
            })
            .collect();
        &spread_cands[..]
    } else {
        cands
    };
    if cands.is_empty() {
        return Err(no_function(name, inputs));
    }
    match select_candidate(cands, inputs, false) {
        Ok(Some(res)) => Ok(res),
        Ok(None) => Err(no_function(name, inputs)),
        Err(_) => Err(Error::new(code::AMBIGUOUS_FUNCTION, format!("function {name}({}) is not unique", type_list(inputs)))
            .hint("Could not choose a best candidate function. You might need to add explicit type casts.")),
    }
}

/// Whether a built-in takes `VARIADIC "any"` (any array after VARIADIC in a call).
pub fn takes_variadic_any(name: &str) -> bool {
    registry().funcs.get(name).into_iter().flatten().any(|d| {
        d.variadic
            && matches!(d.args.last(), Some(DataType::Array(e)) if matches!(**e, DataType::Any))
    })
}

pub fn function_exists(name: &str) -> bool {
    registry().funcs.contains_key(name)
}

pub fn no_function(name: &str, inputs: &[DataType]) -> Error {
    Error::new(code::UNDEFINED_FUNCTION, format!("function {name}({}) does not exist", type_list(inputs)))
        .hint("No function matches the given name and argument types. You might need to add explicit type casts.")
}

pub fn resolve_binop(op: &str, l: &DataType, r: &DataType) -> Result<Resolved> {
    let reg = registry();
    let inputs = [l.clone(), r.clone()];
    let err = || {
        Error::new(code::UNDEFINED_FUNCTION, format!("operator does not exist: {} {op} {}", l.without_typmod().display(), r.without_typmod().display()))
            .hint("No operator matches the given name and argument types. You might need to add explicit type casts.")
    };
    let Some(cands) = reg.binops.get(op) else {
        return Err(err());
    };
    match select_candidate(cands, &inputs, true) {
        Ok(Some(res)) => Ok(res),
        Ok(None) => Err(err()),
        Err(_) => Err(Error::new(code::AMBIGUOUS_FUNCTION, format!("operator is not unique: {} {op} {}", l.without_typmod().display(), r.without_typmod().display()))
            .hint("Could not choose a best candidate operator. You might need to add explicit type casts.")),
    }
}

pub fn resolve_prefix(op: &str, t: &DataType) -> Result<Resolved> {
    let reg = registry();
    let err = || {
        Error::new(code::UNDEFINED_FUNCTION, format!("operator does not exist: {op} {}", t.without_typmod().display()))
            .hint("No operator matches the given name and argument type. You might need to add an explicit type cast.")
    };
    let Some(cands) = reg.prefix.get(op) else {
        return Err(err());
    };
    match select_candidate(cands, std::slice::from_ref(t), false) {
        Ok(Some(res)) => Ok(res),
        Ok(None) => Err(err()),
        Err(_) => Err(Error::new(
            code::AMBIGUOUS_FUNCTION,
            format!("operator is not unique: {op} {}", t.without_typmod().display()),
        )),
    }
}

/// Call a resolved operator on two values (used by IN lists, CASE, sorting helpers).
/// Call a binary operator's implementation; `ty` is the operands' type (enum operators need it).
pub fn call2(def: &FuncDef, a: Value, b: Value, ty: &DataType, ctx: &ExecCtx) -> Result<Value> {
    if def.strict && (a.is_null() || b.is_null()) {
        return Ok(Value::Null);
    }
    let ret = DataType::Bool;
    let args = [
        Expr::Const(Value::Null, ty.clone()),
        Expr::Const(Value::Null, ty.clone()),
    ];
    let fc = FnCtx {
        exec: ctx,
        ret: &ret,
        args: &args,
        coll: def.coll.as_deref(),
    };
    let _g = def
        .coll
        .as_ref()
        .filter(|_| matches!(ty, DataType::Range(_) | DataType::Multirange(_)))
        .map(|c| crate::types::range::under(Some(c.clone())));
    (def.imp)(&[a, b], &fc)
}

// ── helpers for implementations ──

pub fn text_arg(v: &Value) -> &str {
    match v {
        Value::Text(s) | Value::Json(s) => s,
        _ => "",
    }
}

pub fn int_arg(v: &Value) -> i64 {
    v.as_i64().unwrap_or(0)
}

pub fn float_arg(v: &Value) -> f64 {
    v.as_f64().unwrap_or(0.0)
}

pub fn text(s: impl Into<Arc<str>>) -> Value {
    Value::Text(s.into())
}

fn display_only(_: &[Value], _: &FnCtx) -> Result<Value> {
    Err(crate::error::internal(
        "display-only function reference evaluated",
    ))
}

/// A name as `&'static str` (FuncDef names), each distinct name leaked once.
pub fn intern(s: &str) -> &'static str {
    static NAMES: OnceLock<std::sync::Mutex<std::collections::HashSet<&'static str>>> =
        OnceLock::new();
    let mut names = NAMES.get_or_init(Default::default).lock().unwrap();
    match names.get(s) {
        Some(n) => n,
        None => {
            let n: &'static str = Box::leak(s.to_string().into_boxed_str());
            names.insert(n);
            n
        }
    }
}

/// A built-in function by its pg_proc oid.
pub fn def_by_oid(oid: u32) -> Option<Arc<FuncDef>> {
    registry()
        .funcs
        .values()
        .flatten()
        .find(|f| f.oid == oid)
        .cloned()
}

/// A candidate standing for something else (a user-defined operator) in `select_candidate`.
pub fn candidate_def(oid: u32, args: Vec<DataType>, ret: DataType) -> Arc<FuncDef> {
    Arc::new(FuncDef {
        oid,
        name: "",
        min_args: args.len(),
        args,
        variadic: false,
        spread: false,
        ret: Ret::Fixed(ret),
        strict: true,
        volatility: 'i',
        imp: display_only,
        srf: None,
        out_cols: vec![],
        arg_names: vec![],
        defaults: vec![],
        coll: None,
    })
}

/// A call target for a user function in a tree bound only for display (it is never run). Names
/// are interned, so repeated display of the same function doesn't grow memory.
/// An operator only shown, never run (a deparsed row comparison's).
pub fn display_op(name: &str, args: Vec<DataType>, ret: DataType) -> Arc<FuncDef> {
    Arc::new(FuncDef {
        oid: 0,
        name: intern(name),
        min_args: args.len(),
        args,
        variadic: false,
        spread: false,
        ret: Ret::Fixed(ret),
        strict: true,
        volatility: 'i',
        imp: display_only,
        srf: None,
        out_cols: vec![],
        arg_names: vec![],
        defaults: vec![],
        coll: None,
    })
}

pub fn display_def(f: &crate::catalog::Function) -> Arc<FuncDef> {
    Arc::new(FuncDef {
        oid: f.oid,
        name: intern(&f.name),
        args: f.args.iter().map(|(_, t)| t.clone()).collect(),
        variadic: false,
        spread: false,
        ret: Ret::Fixed(f.ret.clone()),
        strict: f.strict,
        volatility: f.volatility,
        imp: display_only,
        min_args: f.args.len(),
        srf: None,
        out_cols: vec![],
        arg_names: vec![],
        defaults: vec![],
        coll: None,
    })
}
