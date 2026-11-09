//! Functions: the registry, overload resolution and the native functions.
//!
//! Resolution follows Cassandra's `cql3.functions.FunctionResolver` (Apache-2.0): candidates are
//! the functions of that name (user functions of the statement's keyspace first, then `system`);
//! with one candidate its arity and argument types are checked; with several, an exact match wins,
//! otherwise exactly one weakly-assignable candidate must remain. Messages are each dialect's.
//!
//! # Adding a function
//!
//! - A fixed signature: implement [`Function`] (or use [`Native`] with a plain `fn`) and push it
//!   in the right family's `register` (`native.rs`, `time.rs`, …).
//! - A family generic over its argument types (`token`, `toJson`, `min`, …): implement
//!   [`Factory`], which builds a [`Function`] for the arguments at hand.
//! - User-defined functions come from a [`UserFunctions`] provider installed by the UDF subsystem
//!   (`Database::set_user_functions`); they are tried before `system` functions.

pub mod aggregates;
pub mod arith;
pub mod bigdec;
pub mod collection;
pub mod floor;
pub mod masking;
pub mod math;
pub mod native;
pub mod params;
pub mod scylla;
pub mod time;

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use nosql_types::{CqlType, Dialect};

use crate::error::{Error, Result};

/// A function's qualified name (`system.now`).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FnName {
    pub keyspace: String,
    pub name: String,
}

impl FnName {
    pub fn system(name: &str) -> Self {
        FnName {
            keyspace: "system".into(),
            name: name.into(),
        }
    }
}

impl std::fmt::Display for FnName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.keyspace, self.name)
    }
}

/// What a function call can see at execution.
pub struct FnCtx {
    pub dialect: Dialect,
    /// Protocol version of the request (`toJson` / `fromJson` formats don't depend on it, but UDFs
    /// do).
    pub protocol_version: u8,
}

/// A function (scalar or aggregate) resolved for concrete argument types.
pub trait Function: Send + Sync + std::fmt::Debug {
    fn name(&self) -> &FnName;
    fn arg_types(&self) -> &[CqlType];
    fn return_type(&self) -> &CqlType;

    fn is_aggregate(&self) -> bool {
        false
    }

    /// `false` for `now()`, `uuid()`, … (never pre-evaluated at prepare time).
    fn is_deterministic(&self) -> bool {
        true
    }

    /// `RETURNS NULL ON NULL INPUT`: a null argument returns null without calling.
    fn null_on_null_input(&self) -> bool {
        false
    }

    /// Calls a scalar function.
    fn execute(&self, _ctx: &FnCtx, _args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        Err(Error::server(format!(
            "{} is not a scalar function",
            self.name()
        )))
    }

    /// A fresh accumulator of an aggregate function.
    fn new_aggregate(&self) -> Option<Box<dyn Aggregate>> {
        None
    }

    /// The functions a call also runs (a user aggregate's state, final and reduce functions),
    /// for `EXECUTE` checks (`Function.addFunctionsTo`).
    fn dependencies(&self) -> Vec<Arc<dyn Function>> {
        Vec::new()
    }

    /// The signature as messages show it (`system.sum : (int) -> int` in Cassandra).
    fn signature(&self, d: Dialect) -> String {
        match d {
            Dialect::Scylla => self.name().to_string(),
            Dialect::Cassandra => {
                let args: Vec<String> =
                    self.arg_types().iter().map(nosql_types::cql_name).collect();
                format!(
                    "{} : ({}) -> {}",
                    self.name(),
                    args.join(", "),
                    nosql_types::cql_name(self.return_type())
                )
            }
        }
    }
}

/// The running state of an aggregate over a group of rows.
pub trait Aggregate: Send {
    fn add(&mut self, ctx: &FnCtx, args: &[Option<Bytes>]) -> Result<()>;
    fn compute(&self, ctx: &FnCtx) -> Result<Option<Bytes>>;
}

pub type ScalarFn = fn(&FnCtx, &[Option<Bytes>]) -> Result<Option<Bytes>>;

/// A native scalar function with a fixed signature.
#[derive(Clone)]
pub struct Native {
    pub name: FnName,
    pub args: Vec<CqlType>,
    pub ret: CqlType,
    pub imp: ScalarFn,
    pub deterministic: bool,
    pub null_on_null: bool,
}

impl std::fmt::Debug for Native {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}({:?}) -> {:?}", self.name, self.args, self.ret)
    }
}

impl Native {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(name: &str, args: Vec<CqlType>, ret: CqlType, imp: ScalarFn) -> Arc<dyn Function> {
        Arc::new(Native {
            name: FnName::system(name),
            args,
            ret,
            imp,
            deterministic: true,
            null_on_null: true,
        })
    }

    pub fn nondeterministic(
        name: &str,
        args: Vec<CqlType>,
        ret: CqlType,
        imp: ScalarFn,
    ) -> Arc<dyn Function> {
        Arc::new(Native {
            name: FnName::system(name),
            args,
            ret,
            imp,
            deterministic: false,
            null_on_null: true,
        })
    }
}

impl Function for Native {
    fn name(&self) -> &FnName {
        &self.name
    }
    fn arg_types(&self) -> &[CqlType] {
        &self.args
    }
    fn return_type(&self) -> &CqlType {
        &self.ret
    }
    fn is_deterministic(&self) -> bool {
        self.deterministic
    }
    fn null_on_null_input(&self) -> bool {
        self.null_on_null
    }
    fn execute(&self, ctx: &FnCtx, args: &[Option<Bytes>]) -> Result<Option<Bytes>> {
        (self.imp)(ctx, args)
    }
}

/// How well an argument fits a parameter type (`AssignmentTestable.TestResult`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Assign {
    No,
    Weak,
    Exact,
}

/// What resolution knows about an argument.
pub trait ArgInfo {
    /// How the argument fits a parameter of type `ty`.
    fn test(&self, ty: &CqlType, d: Dialect) -> Assign;
    /// A bind marker (fits anything; validated at execution).
    fn is_marker(&self) -> bool {
        false
    }
    /// The type if known (columns, function results, hints).
    fn exact_type(&self) -> Option<CqlType>;
    /// A type the argument fits if known (`getCompatibleTypeIfKnown`: also a literal's preferred
    /// type and a collection literal's).
    fn compatible_type(&self) -> Option<CqlType> {
        self.exact_type()
    }
    /// The element count of an array literal (`Selectable.WithArrayLiteral`).
    fn array_len(&self) -> Option<usize> {
        None
    }
    /// The argument as written, for messages.
    fn text(&self) -> String;
    /// A `NULL` constant.
    fn is_null(&self) -> bool {
        false
    }
    /// ScyllaDB's type of the argument: a constant's as ScyllaDB infers it (strings are
    /// `text`, collection literals typed by their elements), else [`ArgInfo::exact_type`].
    fn scylla_type(&self) -> Option<CqlType> {
        self.exact_type()
    }
    /// The argument as ScyllaDB prints it in messages (`null`, `inf`).
    fn scylla_text(&self) -> String {
        self.text()
    }
}

/// A family of overloads built for the arguments at hand (`token`, `toJson`, `count`, …).
pub trait Factory: Send + Sync {
    fn create(
        &self,
        args: &[&dyn ArgInfo],
        receiver: Option<&CqlType>,
        d: Dialect,
    ) -> Result<Option<Arc<dyn Function>>>;
}

/// User-defined functions (provided by the UDF subsystem).
pub trait UserFunctions: Send + Sync {
    /// Candidates named `name` in `keyspace`.
    fn get(&self, keyspace: &str, name: &str) -> Vec<Arc<dyn Function>>;
}

/// The native functions of a dialect.
pub struct Registry {
    dialect: Dialect,
    fixed: HashMap<String, Vec<Arc<dyn Function>>>,
    factories: HashMap<String, Arc<dyn Factory>>,
}

impl Registry {
    pub fn new(d: Dialect) -> Registry {
        let mut r = Registry {
            dialect: d,
            fixed: HashMap::new(),
            factories: HashMap::new(),
        };
        native::register(&mut r);
        time::register(&mut r);
        aggregates::register(&mut r);
        arith::register(&mut r);
        math::register(&mut r);
        collection::register(&mut r);
        masking::register(&mut r);
        floor::register(&mut r);
        scylla::register(&mut r);
        r
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    pub fn add(&mut self, f: Arc<dyn Function>) {
        self.fixed.entry(f.name().name.clone()).or_default().push(f);
    }

    pub fn add_factory(&mut self, name: &str, f: Arc<dyn Factory>) {
        self.factories.insert(name.to_string(), f);
    }

    /// Whether a native function of that name exists.
    pub fn exists(&self, name: &str) -> bool {
        self.fixed.contains_key(name) || self.factories.contains_key(name)
    }

    /// Resolves a call. `keyspace` is the function's keyspace as written (`None`: unqualified).
    /// `Ok(None)` when no function has that name.
    pub fn resolve(
        &self,
        keyspace: Option<&str>,
        name: &str,
        args: &[&dyn ArgInfo],
        receiver: Option<&CqlType>,
        user: Option<(&dyn UserFunctions, &str)>,
    ) -> Result<Option<Arc<dyn Function>>> {
        self.resolve_named(keyspace, name, name, args, receiver, user)
    }

    /// `token(…)` on a table: its arguments are typed as the table's partition key columns
    /// (`FunctionResolver.get` → `TokenFct(table)`), so wrong arities and types fail as for any
    /// fixed signature.
    pub fn resolve_token(
        &self,
        partition_key: &[CqlType],
        args: &[&dyn ArgInfo],
    ) -> Result<Arc<dyn Function>> {
        let f = native::token_function(partition_key);
        validate_types(&f, args, self.dialect)?;
        Ok(f)
    }

    /// [`Registry::resolve`] with the name user functions are looked up by (`user_name`: as
    /// written, case-sensitive when quoted; `name`: lower-cased for native functions).
    pub fn resolve_named(
        &self,
        keyspace: Option<&str>,
        name: &str,
        user_name: &str,
        args: &[&dyn ArgInfo],
        receiver: Option<&CqlType>,
        user: Option<(&dyn UserFunctions, &str)>,
    ) -> Result<Option<Arc<dyn Function>>> {
        let d = self.dialect;
        let mut candidates: Vec<Arc<dyn Function>> = Vec::new();
        if let Some((uf, current)) = user {
            let ks = keyspace.unwrap_or(current);
            if ks != "system" {
                candidates.extend(uf.get(ks, user_name));
            }
        }
        if keyspace.is_none_or(|k| k == "system") {
            if let Some(v) = self.fixed.get(name) {
                candidates.extend(v.iter().cloned());
            }
            if let Some(f) = self.factories.get(name)
                && let Some(made) = f.create(args, receiver, d)?
            {
                candidates.push(made);
            }
        }
        if candidates.is_empty() {
            return Ok(None);
        }
        if candidates.len() == 1 {
            let f = candidates.pop().unwrap_or_else(|| unreachable!());
            validate_types(&f, args, d)?;
            return Ok(Some(f));
        }
        let display = match keyspace {
            Some(k) => format!("{k}.{name}"),
            None => name.to_string(),
        };
        let mut compatible: Vec<Arc<dyn Function>> = Vec::new();
        for f in &candidates {
            if let Some(r) = receiver
                && test_type(f.return_type(), r) == Assign::No
            {
                continue;
            }
            match match_arguments(f, args, d) {
                Assign::Exact => return Ok(Some(f.clone())),
                Assign::Weak => compatible.push(f.clone()),
                Assign::No => {}
            }
        }
        if compatible.is_empty() {
            if let Some(op) = arith::operator_of(name) {
                return Err(Error::invalid(format!(
                    "the '{op}' operation is not supported between {} and {}",
                    args.first().map(|a| a.text()).unwrap_or_default(),
                    args.get(1).map(|a| a.text()).unwrap_or_default()
                )));
            }
            let sigs: Vec<String> = candidates.iter().map(|f| f.signature(d)).collect();
            return Err(Error::invalid(format!(
                "Invalid call to function {display}, none of its type signatures match (known type signatures: {})",
                sigs.join(", ")
            )));
        }
        if compatible.len() > 1 {
            if let Some(op) = arith::operator_of(name) {
                if let Some(r) = receiver
                    && !args.iter().any(|a| a.is_marker())
                    && let Some(f) = compatible
                        .iter()
                        .find(|f| f.arg_types().iter().all(|t| t == r))
                {
                    return Ok(Some(f.clone()));
                }
                return Err(Error::invalid(format!(
                    "Ambiguous '{op}' operation with args {} and {}: use type hint to disambiguate, example '(int) ?'",
                    args.first().map(|a| a.text()).unwrap_or_default(),
                    args.get(1).map(|a| a.text()).unwrap_or_default()
                )));
            }
            if name == arith::NEGATE {
                return Err(Error::invalid(
                    "Ambiguous negation: use type casts to disambiguate",
                ));
            }
            let sigs: Vec<String> = compatible.iter().map(|f| f.signature(d)).collect();
            return Err(Error::invalid(format!(
                "Ambiguous call to function {display} (can be matched by following signatures: {}): use type casts to disambiguate",
                sigs.join(", ")
            )));
        }
        Ok(compatible.pop())
    }
}

fn validate_types(f: &Arc<dyn Function>, args: &[&dyn ArgInfo], d: Dialect) -> Result<()> {
    if args.len() != f.arg_types().len() {
        return Err(Error::invalid(format!(
            "Invalid number of arguments in call to function {}: {} required but {} provided",
            f.name(),
            f.arg_types().len(),
            args.len()
        )));
    }
    for (i, (a, t)) in args.iter().zip(f.arg_types()).enumerate() {
        if a.is_marker() {
            continue;
        }
        if a.test(t, d) == Assign::No {
            return Err(Error::invalid(format!(
                "Type error: {} cannot be passed as argument {i} of function {} of type {}",
                a.text(),
                f.name(),
                nosql_types::cql_name(t)
            )));
        }
    }
    Ok(())
}

fn match_arguments(f: &Arc<dyn Function>, args: &[&dyn ArgInfo], d: Dialect) -> Assign {
    if args.len() != f.arg_types().len() {
        return Assign::No;
    }
    let mut res = Assign::Exact;
    for (a, t) in args.iter().zip(f.arg_types()) {
        if a.is_marker() {
            res = res.min(Assign::Weak);
            continue;
        }
        match a.test(t, d) {
            Assign::No => return Assign::No,
            Assign::Weak => res = Assign::Weak,
            Assign::Exact => {}
        }
    }
    res
}

/// `AbstractType.testAssignment`: a value of type `have` given where `want` is expected.
pub fn test_type(have: &CqlType, want: &CqlType) -> Assign {
    let want_frozen;
    let want = if have.supports_freezing() && !have.is_multicell() && want.is_multicell() {
        want_frozen = want.freeze();
        &want_frozen
    } else {
        want
    };
    if have == want || (matches!(have, CqlType::Counter) && matches!(want, CqlType::Counter)) {
        return Assign::Exact;
    }
    if want.is_value_compatible_with(have) {
        return Assign::Weak;
    }
    Assign::No
}

/// An argument of known type (a column, a function result, a hinted term).
pub struct Typed {
    pub ty: CqlType,
    pub text: String,
}

impl ArgInfo for Typed {
    fn test(&self, ty: &CqlType, _d: Dialect) -> Assign {
        test_type(&self.ty, ty)
    }
    fn exact_type(&self) -> Option<CqlType> {
        Some(self.ty.clone())
    }
    fn text(&self) -> String {
        self.text.clone()
    }
}

/// Helpers for implementations.
pub mod util {
    use super::*;

    pub fn arg(args: &[Option<Bytes>], i: usize) -> Option<&Bytes> {
        args.get(i).and_then(|a| a.as_ref())
    }

    pub fn i64_of(b: &[u8]) -> Option<i64> {
        <[u8; 8]>::try_from(b).ok().map(i64::from_be_bytes)
    }

    pub fn bytes_i64(v: i64) -> Option<Bytes> {
        Some(Bytes::copy_from_slice(&v.to_be_bytes()))
    }

    pub fn bytes_i32(v: i32) -> Option<Bytes> {
        Some(Bytes::copy_from_slice(&v.to_be_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Lit(nosql_types::LiteralKind, &'static str);

    impl ArgInfo for Lit {
        fn test(&self, ty: &CqlType, d: Dialect) -> Assign {
            if nosql_types::literal::is_assignable(self.0, ty, d) {
                Assign::Weak
            } else {
                Assign::No
            }
        }
        fn exact_type(&self) -> Option<CqlType> {
            None
        }
        fn text(&self) -> String {
            self.1.to_string()
        }
    }

    #[test]
    fn resolution_messages() {
        let c = Registry::new(Dialect::Cassandra);
        let s = Registry::new(Dialect::Scylla);
        let txt = Typed {
            ty: CqlType::Text,
            text: "txt".into(),
        };
        let e = c.resolve(None, "sum", &[&txt], None, None).unwrap_err();
        assert!(e.message.starts_with("Invalid call to function sum, none of its type signatures match (known type signatures: system.sum : ("), "{}", e.message);
        let e = s.resolve(None, "sum", &[&txt], None, None).unwrap_err();
        assert!(e.message.starts_with("Invalid call to function sum, none of its type signatures match (known type signatures: system.sum, system.sum"), "{}", e.message);
        let v = Typed {
            ty: CqlType::Int,
            text: "v".into(),
        };
        let e = c
            .resolve(None, "blobasint", &[&v, &v], None, None)
            .unwrap_err();
        assert_eq!(
            e.message,
            "Invalid number of arguments in call to function system.blobasint: 1 required but 2 provided"
        );
        assert!(
            c.resolve(None, "nofunc", &[], None, None)
                .unwrap()
                .is_none()
        );
        let f = c.resolve(None, "sum", &[&v], None, None).unwrap().unwrap();
        assert_eq!(f.return_type(), &CqlType::Int);
        let now = c.resolve(None, "now", &[], None, None).unwrap().unwrap();
        assert!(!now.is_deterministic());
        let one = Lit(nosql_types::LiteralKind::Integer, "1");
        assert_eq!(
            c.resolve(None, "now", &[&one], None, None)
                .unwrap_err()
                .message,
            "Invalid number of arguments in call to function system.now: 0 required but 1 provided"
        );
    }

    #[test]
    fn type_tests() {
        assert_eq!(test_type(&CqlType::Int, &CqlType::Int), Assign::Exact);
        assert_eq!(test_type(&CqlType::Int, &CqlType::Varint), Assign::Weak);
        assert_eq!(test_type(&CqlType::Text, &CqlType::Int), Assign::No);
        assert_eq!(
            test_type(
                &CqlType::list(CqlType::Int, true),
                &CqlType::list(CqlType::Int, false)
            ),
            Assign::Exact
        );
    }
}
