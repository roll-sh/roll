//! CREATE / ALTER / DROP OPERATOR: user-defined operators (pg_operator), each calling a
//! function (a user one or a built-in like int4eq).

use super::Ddl;
use crate::catalog::Operator;
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{self, inner, names};
use crate::sql::binder::resolve_type_name;
use crate::types::DataType;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

fn invalid(msg: &str) -> Error {
    Error::new(code::INVALID_FUNCTION_DEFINITION, msg)
}

/// A qualified name in a definition: `FUNCTION = f` (a type name to the grammar),
/// `COMMUTATOR = OPERATOR(s.===)`, `RESTRICT = 'eqsel'`.
fn def_names(d: &pb::DefElem) -> Vec<String> {
    match d.arg.as_deref().and_then(inner) {
        Some(NodeEnum::TypeName(tn)) => names(&tn.names),
        Some(NodeEnum::List(l)) => names(&l.items),
        Some(NodeEnum::String(s)) => vec![s.sval.clone()],
        _ => Vec::new(),
    }
}

/// A row of the built-in pg_operator.
struct BuiltinOperator {
    oid: u32,
    owner: u32,
    commutator: u32,
    negator: u32,
}

fn builtin_rows() -> impl Iterator<Item = Vec<&'static str>> {
    include_str!("../../catalog/data/pg_operator.tsv")
        .lines()
        .map(|l| l.split('\t').collect::<Vec<_>>())
        .filter(|f| f.len() >= 12)
}

/// The built-in operator with this name and exact argument types (left 0 = prefix).
fn builtin_operator(name: &str, left: Option<u32>, right: u32) -> Option<BuiltinOperator> {
    let num = |s: &str| s.parse::<u32>().unwrap_or(0);
    builtin_rows()
        .find(|f| f[1] == name && num(f[7]) == left.unwrap_or(0) && num(f[8]) == right)
        .map(|f| BuiltinOperator {
            oid: num(f[0]),
            owner: num(f[3]),
            commutator: num(f[10]),
            negator: num(f[11]),
        })
}

fn builtin_operator_name(oid: u32) -> Option<String> {
    let oid = oid.to_string();
    builtin_rows().find(|f| f[0] == oid).map(|f| f[1].to_string())
}

/// The operator a type's display needs: `integer === integer`, `=== integer`.
fn shown(name: &str, left: Option<&DataType>, right: &DataType) -> String {
    match left {
        Some(l) => format!("{} {name} {}", l.display(), right.display()),
        None => format!("{name} {}", right.display()),
    }
}

impl Ddl<'_, '_> {
    /// A function by name and exact argument types: a user function, else a built-in.
    /// Returns its oid and result type.
    fn operator_function(&self, parts: &[String], args: &[DataType]) -> Result<(u32, DataType)> {
        let name = parts.last().cloned().unwrap_or_default();
        let schemas: Vec<u32> = match parts {
            [.., sc, _] => self
                .cat
                .schema_by_name(sc)
                .map(|s| vec![s.oid])
                .unwrap_or_default(),
            _ => self.cat.effective_path(self.search_path()),
        };
        let same = |a: &[DataType]| {
            a.len() == args.len() && a.iter().zip(args).all(|(a, b)| a.same_base(b))
        };
        if let Some(f) = schemas.iter().find_map(|sc| {
            self.cat.functions.values().find(|f| {
                f.schema == *sc
                    && f.name == name
                    && !f.procedure
                    && same(&f.args.iter().map(|a| a.1.clone()).collect::<Vec<_>>())
            })
        }) {
            if !self.is_superuser() {
                let (p, _) = self.cat.obj_mask(
                    crate::catalog::acl::ObjKind::Function,
                    f.oid,
                    self.me(),
                    self.db_owner(),
                );
                if p & crate::catalog::acl::EXECUTE == 0 {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied for function {}", f.name),
                    ));
                }
            }
            return Ok((f.oid, f.ret.clone()));
        }
        let builtin = parts.len() == 1 || parts.len() == 2 && parts[0] == "pg_catalog";
        if builtin
            && let Some(d) = crate::sql::func::registry()
                .funcs
                .get(name.as_str())
                .into_iter()
                .flatten()
                .find(|d| !d.variadic && same(&d.args))
        {
            // A polymorphic result is declared as its pseudo-type (unnest: anyelement).
            let compat = d.args.iter().any(|a| {
                matches!(
                    a,
                    DataType::AnyCompatible
                        | DataType::AnyCompatibleArray
                        | DataType::AnyCompatibleRange
                        | DataType::AnyCompatibleMultirange
                )
            });
            let ret = match &d.ret {
                crate::sql::func::Ret::Fixed(t) => t.clone(),
                crate::sql::func::Ret::Poly if compat => DataType::AnyCompatible,
                crate::sql::func::Ret::Poly => DataType::AnyElement,
                crate::sql::func::Ret::PolyArray if compat => DataType::AnyCompatibleArray,
                crate::sql::func::Ret::PolyArray => DataType::AnyArray,
                crate::sql::func::Ret::Arg(i) => d.args.get(*i).cloned().unwrap_or(DataType::Any),
            };
            return Ok((d.oid, ret));
        }
        let types: Vec<String> = args.iter().map(|t| t.display()).collect();
        Err(Error::new(
            code::UNDEFINED_FUNCTION,
            format!(
                "function {}({}) does not exist",
                parts.join("."),
                types.join(", ")
            ),
        ))
    }

    /// RESTRICT / JOIN: a selectivity estimator of the right signature.
    fn estimator(&self, parts: &[String], join: bool) -> Result<String> {
        let name = parts.last().cloned().unwrap_or_default();
        if (parts.len() == 1 || parts[0] == "pg_catalog")
            && crate::catalog::ESTIMATORS
                .iter()
                .any(|(_, n)| *n == name && n.ends_with("joinsel") == join)
        {
            return Ok(name);
        }
        if self.cat.user_estimator(&name, join).is_some() {
            return Ok(name);
        }
        let sig = if join {
            "internal, oid, internal, smallint, internal"
        } else {
            "internal, oid, internal, integer"
        };
        Err(Error::new(
            code::UNDEFINED_FUNCTION,
            format!("function {}({sig}) does not exist", parts.join(".")),
        ))
    }

    /// Schema and name of an operator name list (the creation schema when unqualified).
    fn operator_target(&self, parts: &[String]) -> Result<(u32, String)> {
        match parts {
            [.., sc, n] => Ok((self.cat.schema_oid(sc)?, n.clone())),
            [n] => Ok((self.creation_schema()?, n.clone())),
            [] => Err(crate::error::internal("operator name")),
        }
    }

    /// The commutator or negator an operator names: an existing one, the new operator itself
    /// (None), or a new shell.
    fn other_operator(
        &mut self,
        parts: &[String],
        left: Option<&DataType>,
        right: &DataType,
        me: (&str, u32, Option<&DataType>, &DataType),
        neg: bool,
    ) -> Result<Option<u32>> {
        let (schema, name) = self.operator_target(parts)?;
        let same_types =
            |a: Option<&DataType>, b: Option<&DataType>| a.map(|t| t.oid()) == b.map(|t| t.oid());
        if name == me.0 && schema == me.1 && same_types(left, me.2) && right.same_base(me.3) {
            return Ok(None);
        }
        // A built-in operator (pg_catalog is searched first): its commutator and negator links
        // are fixed, so naming one that is already linked fails as in Postgres rather than
        // making a shell that would shadow it.
        if (parts.len() == 1 || parts[0] == "pg_catalog")
            && let Some(b) = builtin_operator(&name, left.map(|t| t.oid()), right.oid())
        {
            self.require_owner(b.owner, "operator", &name)?;
            let (link, what) = if neg {
                (b.negator, "negator")
            } else {
                (b.commutator, "commutator")
            };
            if link != 0 {
                return Err(invalid(&format!(
                    "{what} operator {name} is already the {what} of operator {}",
                    builtin_operator_name(link).unwrap_or_default()
                )));
            }
            return Ok(Some(b.oid));
        }
        if let Some(o) = self.cat.find_operator(&[schema], &name, left, right) {
            self.require_owner(o.owner, "operator", &o.name)?;
            return Ok(Some(o.oid));
        }
        self.require_schema_create(schema)?;
        let oid = self.cat.alloc_oid();
        let shell = Operator {
            oid,
            name,
            schema,
            owner: self.me(),
            left: left.cloned(),
            right: right.clone(),
            func: 0,
            result: None,
            commutator: 0,
            negator: 0,
            restrict: None,
            join: None,
            hashes: false,
            merges: false,
        };
        self.cat.operators.insert(oid, Arc::new(shell));
        Ok(Some(oid))
    }

    pub(super) fn create_operator(&mut self, s: &pb::DefineStmt) -> Result<String> {
        const TAG: &str = "CREATE OPERATOR";
        let (schema, name) = self.operator_target(&names(&s.defnames))?;
        let mut left = None;
        let mut right = None;
        let mut function = Vec::new();
        let (mut commutator, mut negator) = (None, None);
        let (mut restrict, mut join) = (None, None);
        let (mut hashes, mut merges) = (false, false);
        for d in super::def_elems(&s.definition) {
            let flag = || {
                d.arg.is_none()
                    || ast::def_arg_str(d).is_some_and(|v| {
                        matches!(v.to_lowercase().as_str(), "true" | "on" | "1" | "yes")
                    })
            };
            match d.defname.as_str() {
                "leftarg" | "rightarg" => {
                    let Some(NodeEnum::TypeName(tn)) = d.arg.as_deref().and_then(inner) else {
                        continue;
                    };
                    if tn.setof {
                        return Err(invalid("SETOF type not allowed for operator argument"));
                    }
                    let (ty, dom) = resolve_type_name(&self.cat, self.search_path(), tn)
                        .map_err(|e| Error {
                            position: None,
                            ..e
                        })?;
                    self.require_type_usage(&ty, dom)?;
                    // (typenameTypeId: an argument type has no modifier; `bit` isn't bit(1).)
                    let ty = ty.without_typmod();
                    if d.defname == "leftarg" {
                        left = Some(ty)
                    } else {
                        right = Some(ty)
                    }
                }
                "function" | "procedure" => function = def_names(d),
                "commutator" => commutator = Some(def_names(d)),
                "negator" => negator = Some(def_names(d)),
                "restrict" => restrict = Some(def_names(d)),
                "join" => join = Some(def_names(d)),
                "hashes" => hashes = flag(),
                "merges" | "sort1" | "sort2" | "ltcmp" | "gtcmp" => merges |= flag(),
                other => self.ctx.notice(crate::session::warning(
                    code::SYNTAX_ERROR,
                    format!("operator attribute \"{other}\" not recognized"),
                )),
            }
        }
        if function.is_empty() {
            return Err(invalid("operator function must be specified"));
        }
        let Some(right) = right else {
            if left.is_none() {
                return Err(invalid("operator argument types must be specified"));
            }
            return Err(invalid("operator right argument type must be specified")
                .detail("Postfix operators are not supported."));
        };
        // USAGE on the argument types, then the result type (aclcheck_error_type).
        let usage = |me: &Self, t: &DataType| -> Result<()> {
            let oid = crate::catalog::system::type_oid(&me.cat, t);
            if me.cat.user_type_name(oid).is_some() && !me.is_superuser() {
                let (p, _) = me.cat.obj_mask(
                    crate::catalog::acl::ObjKind::Type,
                    oid,
                    me.me(),
                    me.db_owner(),
                );
                if p & crate::catalog::acl::USAGE == 0 {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied for type {}", me.type_text(oid)),
                    ));
                }
            }
            Ok(())
        };
        for t in left.iter().chain([&right]) {
            usage(self, t)?;
        }
        let args: Vec<DataType> = left.iter().cloned().chain([right.clone()]).collect();
        let (func, result) = self.operator_function(&function, &args)?;
        usage(self, &result)?;
        let restrict = restrict.map(|p| self.estimator(&p, false)).transpose()?;
        let join = join.map(|p| self.estimator(&p, true)).transpose()?;
        if left.is_none() {
            if commutator.is_some() {
                return Err(invalid("only binary operators can have commutators"));
            }
            if join.is_some() {
                return Err(invalid("only binary operators can have join selectivity"));
            }
            if merges {
                return Err(invalid("only binary operators can merge join"));
            }
            if hashes {
                return Err(invalid("only binary operators can hash"));
            }
        }
        if result != DataType::Bool {
            if negator.is_some() {
                return Err(invalid("only boolean operators can have negators"));
            }
            if restrict.is_some() {
                return Err(invalid(
                    "only boolean operators can have restriction selectivity",
                ));
            }
            if join.is_some() {
                return Err(invalid("only boolean operators can have join selectivity"));
            }
            if merges {
                return Err(invalid("only boolean operators can merge join"));
            }
            if hashes {
                return Err(invalid("only boolean operators can hash"));
            }
        }
        // An existing shell is filled in; a full operator is a duplicate.
        let existing = self
            .cat
            .find_operator(&[schema], &name, left.as_ref(), &right);
        if let Some(o) = &existing {
            if o.func != 0 {
                return Err(Error::new(
                    code::DUPLICATE_FUNCTION,
                    format!("operator {name} already exists"),
                ));
            }
            self.require_owner(o.owner, "operator", &o.name)?;
        } else {
            self.require_schema_create(schema)?;
        }
        let oid = existing
            .as_ref()
            .map_or_else(|| self.cat.alloc_oid(), |o| o.oid);
        let me = (name.as_str(), schema, left.as_ref(), &right);
        // The commutator takes the operands the other way round.
        let com = match &commutator {
            Some(p) => match left.as_ref() {
                Some(l) => self
                    .other_operator(p, Some(&right), l, me, false)?
                    .or(Some(oid)),
                None => None,
            },
            None => None,
        };
        let neg = match &negator {
            Some(p) => match self.other_operator(p, left.as_ref(), &right, me, true)? {
                None => return Err(invalid("operator cannot be its own negator")),
                some => some,
            },
            None => None,
        };
        let show = |cat: &crate::catalog::Catalog, o: u32| {
            cat.operators
                .get(&o)
                .map(|x| x.name.clone())
                .unwrap_or_default()
        };
        // The other operators point back (unless they already point elsewhere).
        if let Some(c) = com.filter(|c| *c != oid)
            && let Some(x) = self.cat.operators.get(&c).cloned()
        {
            if x.commutator != 0 && x.commutator != oid {
                return Err(invalid(&format!(
                    "commutator operator {} is already the commutator of operator {}",
                    x.name,
                    show(&self.cat, x.commutator)
                )));
            }
            self.cat.operators.insert(
                c,
                Arc::new(Operator {
                    commutator: oid,
                    ..(*x).clone()
                }),
            );
        }
        if let Some(n) = neg
            && let Some(x) = self.cat.operators.get(&n).cloned()
        {
            if x.negator != 0 && x.negator != oid {
                return Err(invalid(&format!(
                    "negator operator {} is already the negator of operator {}",
                    x.name,
                    show(&self.cat, x.negator)
                )));
            }
            self.cat.operators.insert(
                n,
                Arc::new(Operator {
                    negator: oid,
                    ..(*x).clone()
                }),
            );
        }
        let op = Operator {
            oid,
            name,
            schema,
            owner: existing.as_ref().map_or(self.me(), |o| o.owner),
            left,
            right,
            func,
            result: Some(result),
            commutator: com.unwrap_or(0),
            negator: neg.unwrap_or(0),
            restrict,
            join,
            hashes,
            merges,
        };
        self.cat.operators.insert(oid, Arc::new(op));
        self.save()?;
        Ok(TAG.into())
    }

    /// `name (left, right)` of DROP / ALTER / COMMENT ON OPERATOR. `missing_ok`: None (after a
    /// notice) when it or its types don't exist.
    pub(super) fn operator_object(
        &self,
        owa: &pb::ObjectWithArgs,
        missing_ok: bool,
    ) -> Result<Option<Arc<Operator>>> {
        let parts = names(&owa.objname);
        let name = parts.last().cloned().unwrap_or_default();
        let mut types = Vec::new();
        for a in &owa.objargs {
            match inner(a) {
                Some(NodeEnum::TypeName(tn)) => {
                    match resolve_type_name(&self.cat, self.search_path(), tn) {
                        Ok((t, _)) => types.push(Some(t)),
                        Err(e) if missing_ok && e.code == code::UNDEFINED_OBJECT => {
                            self.notice(format!(
                                "type \"{}\" does not exist, skipping",
                                names(&tn.names).join(".")
                            ));
                            return Ok(None);
                        }
                        Err(e) => {
                            return Err(Error {
                                position: None,
                                ..e
                            });
                        }
                    }
                }
                _ => types.push(None),
            }
        }
        let (left, right) = match types.as_slice() {
            [l, Some(r)] => (l.clone(), r.clone()),
            [Some(_), None] => {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    "postfix operators are not supported",
                ));
            }
            _ => {
                return Err(Error::new(code::SYNTAX_ERROR, "missing argument")
                    .hint("Use NONE to denote the missing argument of a unary operator."));
            }
        };
        let schemas = match &parts[..] {
            [.., sc, _] => match self.cat.schema_by_name(sc) {
                Some(s) => vec![s.oid],
                None if missing_ok => {
                    self.notice(format!("schema \"{sc}\" does not exist, skipping"));
                    return Ok(None);
                }
                None => Vec::new(),
            },
            _ => self.cat.effective_path(self.search_path()),
        };
        if let Some(o) = self
            .cat
            .find_operator(&schemas, &name, left.as_ref(), &right)
        {
            return Ok(Some(o));
        }
        if missing_ok {
            self.notice(format!("operator {name} does not exist, skipping"));
            return Ok(None);
        }
        Err(Error::new(
            code::UNDEFINED_FUNCTION,
            format!(
                "operator does not exist: {}",
                shown(&parts.join("."), left.as_ref(), &right)
            ),
        ))
    }

    /// ALTER OPERATOR ... SET (RESTRICT = ..., JOIN = ..., COMMUTATOR = ..., ...): as
    /// AlterOperator checks and changes it.
    pub(super) fn alter_operator(&mut self, s: &pb::AlterOperatorStmt) -> Result<String> {
        const TAG: &str = "ALTER OPERATOR";
        let Some(owa) = &s.opername else {
            return Ok(TAG.into());
        };
        // A built-in operator: only what the options say is checked (it can't change here).
        let builtin = {
            let parts = names(&owa.objname);
            let mut tys = Vec::new();
            for a in &owa.objargs {
                tys.push(match inner(a) {
                    Some(NodeEnum::TypeName(tn)) => {
                        resolve_type_name(&self.cat, self.search_path(), tn)
                            .ok()
                            .map(|(t, _)| t.oid())
                    }
                    _ => None,
                });
            }
            match (parts.as_slice(), tys.as_slice()) {
                ([.., n], [l, Some(r)]) if parts.len() == 1 || parts[0] == "pg_catalog" => {
                    builtin_operator(n, *l, *r)
                }
                _ => None,
            }
        };
        let o = match builtin {
            Some(_) => None,
            None => match self.operator_object(owa, false)? {
                Some(o) => Some(o),
                None => return Ok(TAG.into()),
            },
        };
        let (mut restrict, mut join) = (None::<Option<Vec<String>>>, None::<Option<Vec<String>>>);
        let (mut commutator, mut negator) = (None::<Vec<String>>, None::<Vec<String>>);
        let (mut merges, mut hashes) = (None::<bool>, None::<bool>);
        for d in super::def_elems(&s.options) {
            let none = d.arg.is_none()
                || matches!(d.arg.as_deref().and_then(inner), Some(NodeEnum::String(v)) if v.sval.eq_ignore_ascii_case("none"));
            let flag = || {
                d.arg.is_none()
                    || ast::def_arg_str(d).is_some_and(|v| {
                        matches!(v.to_lowercase().as_str(), "true" | "on" | "1" | "yes")
                    })
            };
            match d.defname.as_str() {
                "restrict" => restrict = Some((!none).then(|| def_names(d))),
                "join" => join = Some((!none).then(|| def_names(d))),
                "commutator" => commutator = Some(def_names(d)),
                "negator" => negator = Some(def_names(d)),
                "merges" => merges = Some(flag()),
                "hashes" => hashes = Some(flag()),
                "leftarg" | "rightarg" | "function" | "procedure" => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!("operator attribute \"{}\" cannot be changed", d.defname),
                    ));
                }
                other => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!("operator attribute \"{other}\" not recognized"),
                    ));
                }
            }
        }
        let Some(o) = o else {
            if let Some(b) = builtin {
                self.require_owner(b.owner, "operator", &names(&owa.objname).join("."))?;
            }
            return Err(crate::error::unsupported("ALTER OPERATOR on a built-in operator"));
        };
        self.require_owner(o.owner, "operator", &o.name)?;
        let restrict = match restrict {
            Some(Some(p)) => Some(Some(self.estimator(&p, false)?)),
            Some(None) => Some(None),
            None => None,
        };
        let join = match join {
            Some(Some(p)) => Some(Some(self.estimator(&p, true)?)),
            Some(None) => Some(None),
            None => None,
        };
        // ValidateOperatorReference: an existing (not shell) operator the user owns.
        let reference = |d: &Self, parts: &[String], l: Option<&DataType>, r: &DataType| -> Result<Arc<Operator>> {
            let (schemas, name) = match parts {
                [.., sc, n] => (vec![d.cat.schema_oid(sc)?], n.clone()),
                [n] => (d.cat.effective_path(d.search_path()), n.clone()),
                [] => return Err(crate::error::internal("operator name")),
            };
            let Some(x) = d.cat.find_operator(&schemas, &name, l, r) else {
                return Err(Error::new(
                    code::UNDEFINED_FUNCTION,
                    format!("operator does not exist: {}", shown(&parts.join("."), l, r)),
                ));
            };
            if x.func == 0 {
                return Err(Error::new(
                    code::UNDEFINED_FUNCTION,
                    format!("operator is only a shell: {}", shown(&parts.join("."), l, r)),
                ));
            }
            d.require_owner(x.owner, "operator", &parts.join("."))?;
            Ok(x)
        };
        let comm = match &commutator {
            Some(p) => {
                let Some(l) = &o.left else {
                    return Err(invalid("only binary operators can have commutators"));
                };
                Some(reference(self, p, Some(&o.right), l)?)
            }
            None => None,
        };
        let neg = match &negator {
            Some(p) => {
                let n = reference(self, p, o.left.as_ref(), &o.right)?;
                if n.oid == o.oid {
                    return Err(invalid("operator cannot be its own negator"));
                }
                Some(n)
            }
            None => None,
        };
        // Attributes plans may rely on change only from unset.
        let set_already = |what: &str| {
            invalid(&format!(
                "operator attribute \"{what}\" cannot be changed if it has already been set"
            ))
        };
        if let Some(c) = &comm
            && o.commutator != 0
            && c.oid != o.commutator
        {
            return Err(set_already("commutator"));
        }
        if let Some(n) = &neg
            && o.negator != 0
            && n.oid != o.negator
        {
            return Err(set_already("negator"));
        }
        if merges == Some(false) && o.merges {
            return Err(set_already("merges"));
        }
        if hashes == Some(false) && o.hashes {
            return Err(set_already("hashes"));
        }
        // OperatorValidateParams.
        let binary = o.left.is_some();
        let boolean = o.result.as_ref() == Some(&DataType::Bool);
        let can_merge = merges.unwrap_or(false);
        let can_hash = hashes.unwrap_or(false);
        let has_join = matches!(join, Some(Some(_)));
        let has_restrict = matches!(restrict, Some(Some(_)));
        if !binary {
            if has_join {
                return Err(invalid("only binary operators can have join selectivity"));
            }
            if can_merge {
                return Err(invalid("only binary operators can merge join"));
            }
            if can_hash {
                return Err(invalid("only binary operators can hash"));
            }
        }
        if !boolean {
            if neg.is_some() {
                return Err(invalid("only boolean operators can have negators"));
            }
            if has_restrict {
                return Err(invalid("only boolean operators can have restriction selectivity"));
            }
            if has_join {
                return Err(invalid("only boolean operators can have join selectivity"));
            }
            if can_merge {
                return Err(invalid("only boolean operators can merge join"));
            }
            if can_hash {
                return Err(invalid("only boolean operators can hash"));
            }
        }
        let mut x = (*o).clone();
        if let Some(r) = restrict {
            x.restrict = r;
        }
        if let Some(j) = join {
            x.join = j;
        }
        if let Some(m) = merges {
            x.merges = m;
        }
        if let Some(h) = hashes {
            x.hashes = h;
        }
        if let Some(c) = &comm {
            x.commutator = c.oid;
        }
        if let Some(n) = &neg {
            x.negator = n.oid;
        }
        self.cat.operators.insert(x.oid, Arc::new(x));
        // OperatorUpd: the other operators point back (unless they point elsewhere).
        let third = |c: &Self, oid: u32| {
            c.cat
                .operators
                .get(&oid)
                .map(|t| t.name.clone())
                .or_else(|| builtin_operator_name(oid))
                .unwrap_or_else(|| oid.to_string())
        };
        if let Some(c) = comm
            && c.oid != o.oid
            && c.commutator != o.oid
        {
            if c.commutator != 0 {
                return Err(invalid(&format!(
                    "commutator operator {} is already the commutator of operator {}",
                    c.name,
                    third(self, c.commutator)
                )));
            }
            let mut y = (*c).clone();
            y.commutator = o.oid;
            self.cat.operators.insert(y.oid, Arc::new(y));
        }
        if let Some(n) = neg
            && n.negator != o.oid
        {
            if n.negator != 0 {
                return Err(invalid(&format!(
                    "negator operator {} is already the negator of operator {}",
                    n.name,
                    third(self, n.negator)
                )));
            }
            let mut y = (*n).clone();
            y.negator = o.oid;
            self.cat.operators.insert(y.oid, Arc::new(y));
        }
        self.save()?;
        Ok(TAG.into())
    }

    pub(super) fn operator_owner(&mut self, owa: &pb::ObjectWithArgs, new: u32) -> Result<()> {
        let Some(o) = self.operator_object(owa, false)? else {
            return Ok(());
        };
        if o.owner != new {
            self.require_owner(o.owner, "operator", &o.name)?;
            self.check_new_owner(new, Some(o.schema))?;
            self.cat.operators.insert(
                o.oid,
                Arc::new(Operator {
                    owner: new,
                    ..(*o).clone()
                }),
            );
        }
        Ok(())
    }

    pub(super) fn operator_set_schema(
        &mut self,
        owa: &pb::ObjectWithArgs,
        schema: &str,
    ) -> Result<()> {
        let Some(o) = self.operator_object(owa, false)? else {
            return Ok(());
        };
        self.require_owner(o.owner, "operator", &o.name)?;
        let sc = self.cat.schema_oid(schema)?;
        if sc == o.schema {
            return Ok(());
        }
        self.require_schema_create(sc)?;
        if self
            .cat
            .find_operator(&[sc], &o.name, o.left.as_ref(), &o.right)
            .is_some()
        {
            let sig = format!(
                "{}({},{})",
                o.name,
                o.left.as_ref().map_or("NONE".into(), |t| t.display()),
                o.right.display()
            );
            return Err(Error::new(
                code::DUPLICATE_FUNCTION,
                format!("operator {sig} already exists in schema \"{schema}\""),
            ));
        }
        self.cat.operators.insert(
            o.oid,
            Arc::new(Operator {
                schema: sc,
                ..(*o).clone()
            }),
        );
        Ok(())
    }
}
