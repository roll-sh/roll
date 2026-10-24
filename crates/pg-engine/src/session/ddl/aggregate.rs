//! CREATE AGGREGATE (DefineAggregate, AggregateCreate): user-defined aggregates (pg_proc
//! entries of kind `a` with a pg_aggregate entry), running a transition function (user or
//! built-in) over the rows and an optional final function; moving-aggregate support
//! functions, combine / serialization functions, sort operators and ordered-set aggregates
//! are recorded as Postgres records them.

use super::Ddl;
use crate::catalog::{AggSpec, Function};
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{self, inner, names};
use crate::types::DataType;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

const INTERNAL: u32 = 2281;
const BYTEA: u32 = 17;
const ANY: u32 = 2276;

fn invalid(msg: &str) -> Error {
    Error::new(code::INVALID_FUNCTION_DEFINITION, msg)
}

fn is_internal(t: &DataType) -> bool {
    t.oid() == INTERNAL
}

/// check_valid_polymorphic_signature: a polymorphic result needs an input of its family.
fn poly_unresolved(result: &DataType, inputs: &[DataType]) -> Option<String> {
    let family1 = |t: &DataType| {
        matches!(
            t,
            DataType::AnyElement
                | DataType::AnyArray
                | DataType::AnyNonArray
                | DataType::AnyEnum
                | DataType::AnyRange
                | DataType::AnyMultirange
        )
    };
    let family2 = |t: &DataType| {
        matches!(
            t,
            DataType::AnyCompatible
                | DataType::AnyCompatibleArray
                | DataType::AnyCompatibleRange
                | DataType::AnyCompatibleMultirange
        ) || t.display() == "anycompatiblenonarray"
    };
    if family1(result) && !inputs.iter().any(family1) {
        return Some(format!(
            "A result of type {} requires at least one input of type anyelement, anyarray, anynonarray, anyenum, anyrange, or anymultirange.",
            result.display()
        ));
    }
    if family2(result) && !inputs.iter().any(family2) {
        return Some(format!(
            "A result of type {} requires at least one input of type anycompatible, anycompatiblearray, anycompatiblenonarray, anycompatiblerange, or anycompatiblemultirange.",
            result.display()
        ));
    }
    None
}

/// A function name in a definition (`SFUNC = s.f`).
fn def_names(d: &pb::DefElem) -> Vec<String> {
    match d.arg.as_deref().and_then(inner) {
        Some(NodeEnum::TypeName(tn)) => names(&tn.names),
        Some(NodeEnum::List(l)) => names(&l.items),
        Some(NodeEnum::String(s)) => vec![s.sval.clone()],
        _ => Vec::new(),
    }
}

/// A function that could be a support function: its signature as pg_proc has it.
struct Cand {
    oid: u32,
    args: Vec<DataType>,
    ret: DataType,
    retset: bool,
    strict: bool,
}

impl Ddl<'_, '_> {
    /// A type by oid, for signatures (pseudo-types included).
    fn sig_type(&self, oid: u32) -> DataType {
        self.cat
            .user_data_type(oid)
            .or_else(|| DataType::from_oid(oid))
            .or_else(|| crate::types::BaseType::other_pseudo(oid))
            .unwrap_or(DataType::Unknown)
    }

    fn agg_candidates(&self, parts: &[String], nargs: usize) -> Vec<Cand> {
        let name = parts.last().cloned().unwrap_or_default();
        let schemas: Vec<u32> = match parts {
            [.., sc, _] => self
                .cat
                .schema_by_name(sc)
                .map(|s| vec![s.oid])
                .unwrap_or_default(),
            _ => self.cat.effective_path(self.search_path()),
        };
        let mut out = Vec::new();
        for s in schemas {
            for f in self.cat.functions.values().filter(|f| {
                f.schema == s && f.name == name && !f.procedure && f.agg.is_none() && f.args.len() == nargs
            }) {
                out.push(Cand {
                    oid: f.oid,
                    args: f.args.iter().map(|a| a.1.clone()).collect(),
                    ret: f.ret.clone(),
                    retset: f.returns_set,
                    strict: f.strict,
                });
            }
            if s == crate::catalog::PG_CATALOG_OID {
                for p in crate::catalog::system::builtin_proc_sigs()
                    .iter()
                    .filter(|p| p.name == name && p.args.len() == nargs)
                {
                    out.push(Cand {
                        oid: p.oid,
                        args: p.args.iter().map(|o| self.sig_type(*o)).collect(),
                        ret: self.sig_type(p.ret),
                        retset: p.retset,
                        strict: p.strict,
                    });
                }
            }
        }
        out
    }

    /// IsBinaryCoercible (domains, and the binary-coercible implicit casts).
    fn binary_ok(&self, from: &DataType, to: &DataType) -> bool {
        if from.oid() == to.oid() || to.oid() == ANY || to.is_polymorphic() {
            return true;
        }
        if matches!(from, DataType::Array(_)) && matches!(to, DataType::AnyArray) {
            return true;
        }
        self.cat
            .casts_all()
            .iter()
            .any(|c| c.source == from.oid() && c.target == to.oid() && c.method == 'b' && c.context == 'i')
    }

    /// The polymorphic parameters of `declared` resolved from `actual`
    /// (enforce_generic_type_consistency): None when they conflict.
    fn resolve_poly(&self, declared: &[DataType], actual: &[DataType]) -> Option<(Vec<DataType>, Box<dyn Fn(&DataType) -> DataType>)> {
        let mut elem: Option<DataType> = None;
        let mut compat: Vec<DataType> = Vec::new();
        for (d, a) in declared.iter().zip(actual) {
            if a.is_polymorphic() {
                continue;
            }
            let e = match d {
                DataType::AnyElement | DataType::AnyNonArray | DataType::AnyEnum => Some(a.clone()),
                DataType::AnyArray => Some(a.elem()?.clone()),
                DataType::AnyCompatible | DataType::AnyCompatibleNonArray => {
                    compat.push(a.clone());
                    None
                }
                DataType::AnyCompatibleArray => {
                    compat.push(a.elem()?.clone());
                    None
                }
                _ => None,
            };
            if let Some(e) = e {
                match &elem {
                    Some(x) if x.oid() != e.oid() => return None,
                    _ => elem = Some(e),
                }
            }
        }
        let compat = if compat.is_empty() {
            None
        } else {
            Some(crate::types::cast::common_type(&compat, "").ok()?)
        };
        let subst = move |t: &DataType| -> DataType {
            match (t, &elem, &compat) {
                (DataType::AnyElement | DataType::AnyNonArray | DataType::AnyEnum, Some(e), _) => e.clone(),
                (DataType::AnyArray, Some(e), _) => DataType::array_of(e.clone()),
                (DataType::AnyCompatible | DataType::AnyCompatibleNonArray, _, Some(c)) => {
                    c.clone()
                }
                (DataType::AnyCompatibleArray, _, Some(c)) => DataType::array_of(c.clone()),
                (t, _, _) => t.clone(),
            }
        };
        let resolved = declared.iter().map(&subst).collect();
        Some((resolved, Box::new(subst)))
    }

    /// lookup_agg_function: a support function taking `args`; its oid, result type
    /// (polymorphism resolved) and strictness.
    fn agg_function(&self, parts: &[String], args: &[DataType]) -> Result<(u32, DataType, bool)> {
        let sig = |ts: &[DataType]| {
            let t: Vec<String> = ts.iter().map(|t| self.type_text(crate::catalog::system::type_oid(&self.cat, t))).collect();
            format!("{}({})", parts.join("."), t.join(", "))
        };
        let cands = self.agg_candidates(parts, args.len());
        // An exact match first, else one the arguments convert to (func_get_detail).
        let exact = cands
            .iter()
            .find(|c| c.args.iter().zip(args).all(|(d, a)| d.oid() == a.oid()));
        let fits = |c: &&Cand| {
            c.args.iter().zip(args).all(|(d, a)| {
                d.oid() == a.oid()
                    || d.oid() == ANY
                    || (d.is_polymorphic() && (a.is_polymorphic() || match d {
                        DataType::AnyArray | DataType::AnyCompatibleArray => a.is_array(),
                        DataType::AnyNonArray => !a.is_array(),
                        _ => true,
                    }))
                    || crate::types::cast::cast_context(a, d)
                        == Some(crate::types::cast::CastContext::Implicit)
            }) && generic_consistent(&c.args, args)
                && self.resolve_poly(&c.args, args).is_some()
        };
        let Some(c) = exact.or_else(|| cands.iter().find(fits)) else {
            return Err(Error::new(
                code::UNDEFINED_FUNCTION,
                format!("function {} does not exist", sig(args)),
            ));
        };
        if c.retset {
            return Err(Error::new(
                code::DATATYPE_MISMATCH,
                format!("function {} returns a set", sig(args)),
            ));
        }
        let Some((truetypes, subst)) = self.resolve_poly(&c.args, args) else {
            return Err(Error::new(
                code::UNDEFINED_FUNCTION,
                format!("function {} does not exist", sig(args)),
            ));
        };
        let ret = subst(&c.ret);
        for (a, t) in args.iter().zip(&truetypes) {
            if !self.binary_ok(a, t) {
                return Err(Error::new(
                    code::DATATYPE_MISMATCH,
                    format!("function {} requires run-time type coercion", sig(&truetypes)),
                ));
            }
        }
        if let Some(f) = self.cat.functions.get(&c.oid)
            && !self.is_superuser()
        {
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
        Ok((c.oid, ret, c.strict))
    }

    /// A transition type (stype / mstype): not a pseudo-type, but polymorphic, or internal
    /// for superusers.
    fn agg_trans_type(&self, d: &pb::DefElem) -> Result<DataType> {
        let Some(NodeEnum::TypeName(tn)) = d.arg.as_deref().and_then(inner) else {
            return Err(Error::new(code::SYNTAX_ERROR, format!("{} requires a type name", d.defname)));
        };
        let (t, dom) = crate::sql::binder::resolve_type_name(&self.cat, self.search_path(), tn)
            .map_err(|e| Error { position: None, ..e })?;
        self.require_type_usage(&t, dom)?;
        let pseudo = t.category() == crate::types::Category::Pseudo && !t.is_polymorphic();
        if pseudo && !(is_internal(&t) && self.is_superuser()) {
            return Err(invalid(&format!(
                "aggregate transition data type cannot be {}",
                t.display()
            )));
        }
        Ok(t)
    }

    pub(super) fn create_aggregate(&mut self, s: &pb::DefineStmt) -> Result<String> {
        const TAG: &str = "CREATE AGGREGATE";
        let parts = names(&s.defnames);
        let (schema, name) = match parts.as_slice() {
            [.., sc, n] => (self.cat.schema_oid(sc)?, n.clone()),
            [n] => (self.creation_schema()?, n.clone()),
            [] => return Err(crate::error::internal("aggregate name")),
        };
        self.require_schema_create(schema)?;
        // The arguments, their modes, and the number of direct ones (-1: not ordered-set).
        let mut args: Vec<(Option<String>, DataType)> = Vec::new();
        let mut modes: Vec<char> = Vec::new();
        // The domain each argument was declared as.
        let mut arg_domains: Vec<Option<u32>> = Vec::new();
        let mut variadic: Option<DataType> = None;
        let mut num_direct = -1i64;
        if !s.oldstyle {
            for a in &s.args {
                match inner(a) {
                    Some(NodeEnum::List(l)) => {
                        for p in &l.items {
                            let Some(NodeEnum::FunctionParameter(fp)) = inner(p) else {
                                continue;
                            };
                            let tn = fp
                                .arg_type
                                .as_ref()
                                .ok_or_else(|| crate::error::internal("aggregate argument type"))?;
                            let (ty, dom) = crate::sql::binder::resolve_type_name_shell(
                                &self.cat,
                                self.search_path(),
                                tn,
                            )
                            .map_err(|e| Error { position: None, ..e })?;
                            if matches!(&ty, DataType::Base(b) if b.is_shell()) {
                                return Err(invalid(&format!(
                                    "aggregate cannot accept shell type {}",
                                    names(&tn.names).join(".")
                                )));
                            }
                            self.require_type_usage(&ty, dom)?;
                            arg_domains.push(dom);
                            let m = if fp.mode == pb::FunctionParameterMode::FuncParamVariadic as i32 {
                                variadic = Some(ty.clone());
                                'v'
                            } else {
                                'i'
                            };
                            modes.push(m);
                            args.push(((!fp.name.is_empty()).then(|| fp.name.clone()), ty));
                        }
                    }
                    Some(NodeEnum::Integer(i)) => num_direct = i.ival as i64,
                    _ => {}
                }
            }
        }
        let mut kind = if num_direct >= 0 { 'o' } else { 'n' };
        let num_direct = num_direct.max(0) as usize;
        let mut d_sfunc = Vec::new();
        let mut d_finalfn = Vec::new();
        let mut d_combine = Vec::new();
        let mut d_serial = Vec::new();
        let mut d_deserial = Vec::new();
        let mut d_msfunc = Vec::new();
        let mut d_minvfunc = Vec::new();
        let mut d_mfinal = Vec::new();
        let mut d_sortop = Vec::new();
        let (mut finalextra, mut mfinalextra) = (false, false);
        let (mut finalmodify, mut mfinalmodify) = (None, None);
        let mut basetype: Option<&pb::DefElem> = None;
        let mut stype: Option<&pb::DefElem> = None;
        let mut mstype: Option<&pb::DefElem> = None;
        let (mut transspace, mut mtransspace) = (0i32, 0i32);
        let (mut initval, mut minitval): (Option<String>, Option<String>) = (None, None);
        let mut parallel: Option<String> = None;
        let boolean = |d: &pb::DefElem| -> bool {
            d.arg.is_none()
                || ast::def_arg_str(d).is_some_and(|v| {
                    matches!(v.to_lowercase().as_str(), "true" | "on" | "1" | "yes")
                })
        };
        let modify = |d: &pb::DefElem| -> Result<char> {
            let v = ast::def_arg_str(d).unwrap_or_default().to_lowercase();
            Ok(match v.as_str() {
                "read_only" => 'r',
                "shareable" => 's',
                "read_write" => 'w',
                _ => {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        format!(
                            "parameter \"{}\" must be READ_ONLY, SHAREABLE, or READ_WRITE",
                            d.defname
                        ),
                    ));
                }
            })
        };
        let string = |d: &pb::DefElem| {
            ast::def_arg_str(d).or_else(|| d.arg.as_deref().and_then(ast::ival).map(|i| i.to_string()))
        };
        let int = |d: &pb::DefElem| {
            string(d).and_then(|v| v.parse::<i32>().ok()).unwrap_or(0)
        };
        for d in super::def_elems(&s.definition) {
            match d.defname.as_str() {
                "sfunc" | "sfunc1" => d_sfunc = def_names(d),
                "finalfunc" => d_finalfn = def_names(d),
                "combinefunc" => d_combine = def_names(d),
                "serialfunc" => d_serial = def_names(d),
                "deserialfunc" => d_deserial = def_names(d),
                "msfunc" => d_msfunc = def_names(d),
                "minvfunc" => d_minvfunc = def_names(d),
                "mfinalfunc" => d_mfinal = def_names(d),
                "finalfunc_extra" => finalextra = boolean(d),
                "mfinalfunc_extra" => mfinalextra = boolean(d),
                "finalfunc_modify" => finalmodify = Some(modify(d)?),
                "mfinalfunc_modify" => mfinalmodify = Some(modify(d)?),
                "sortop" => d_sortop = def_names(d),
                "basetype" => basetype = Some(d),
                "hypothetical" => {
                    if boolean(d) {
                        if kind == 'n' {
                            return Err(invalid("only ordered-set aggregates can be hypothetical"));
                        }
                        kind = 'h';
                    }
                }
                "stype" | "stype1" => stype = Some(d),
                "sspace" => transspace = int(d),
                "mstype" => mstype = Some(d),
                "msspace" => mtransspace = int(d),
                "initcond" | "initcond1" => initval = string(d),
                "minitcond" => minitval = string(d),
                "parallel" => parallel = string(d),
                other => self.ctx.notice(crate::session::warning(
                    code::SYNTAX_ERROR,
                    format!("aggregate attribute \"{other}\" not recognized"),
                )),
            }
        }
        let Some(stype) = stype else {
            return Err(invalid("aggregate stype must be specified"));
        };
        if d_sfunc.is_empty() {
            return Err(invalid("aggregate sfunc must be specified"));
        }
        if mstype.is_some() {
            if d_msfunc.is_empty() {
                return Err(invalid("aggregate msfunc must be specified when mstype is specified"));
            }
            if d_minvfunc.is_empty() {
                return Err(invalid("aggregate minvfunc must be specified when mstype is specified"));
            }
        } else {
            for (bad, what) in [
                (!d_msfunc.is_empty(), "msfunc"),
                (!d_minvfunc.is_empty(), "minvfunc"),
                (!d_mfinal.is_empty(), "mfinalfunc"),
                (mtransspace != 0, "msspace"),
                (minitval.is_some(), "minitcond"),
            ] {
                if bad {
                    return Err(invalid(&format!(
                        "aggregate {what} must not be specified without mstype"
                    )));
                }
            }
        }
        let default_modify = if kind == 'n' { 'r' } else { 'w' };
        let finalmodify = finalmodify.unwrap_or(default_modify);
        let mfinalmodify = mfinalmodify.unwrap_or(default_modify);
        if s.oldstyle {
            let Some(b) = basetype else {
                return Err(invalid("aggregate input type must be specified"));
            };
            let any = match b.arg.as_deref().and_then(inner) {
                Some(NodeEnum::TypeName(tn)) => names(&tn.names).join(".").eq_ignore_ascii_case("any"),
                _ => ast::def_arg_str(b).is_some_and(|v| v.eq_ignore_ascii_case("any")),
            };
            if !any {
                let Some(NodeEnum::TypeName(tn)) = b.arg.as_deref().and_then(inner) else {
                    return Err(invalid("aggregate input type must be specified"));
                };
                let t = crate::sql::binder::resolve_type_name(&self.cat, self.search_path(), tn)
                    .map_err(|e| Error { position: None, ..e })?
                    .0;
                args = vec![(None, t)];
            }
        } else if basetype.is_some() {
            return Err(invalid(
                "basetype is redundant with aggregate input type specification",
            ));
        }
        let transtype = self.agg_trans_type(stype)?;
        if !d_serial.is_empty() && !d_deserial.is_empty() {
            if !is_internal(&transtype) {
                return Err(invalid(
                    "serialization functions may be specified only when the aggregate transition data type is internal",
                ));
            }
        } else if !d_serial.is_empty() || !d_deserial.is_empty() {
            return Err(invalid(
                "must specify both or neither of serialization and deserialization functions",
            ));
        }
        let mtranstype = match mstype {
            Some(d) => Some(self.agg_trans_type(d)?),
            None => None,
        };
        let pseudo = |t: &DataType| t.category() == crate::types::Category::Pseudo;
        if let Some(v) = &initval
            && !pseudo(&transtype)
        {
            crate::types::text::parse_value(v, &transtype, self.ctx.fmt())?;
        }
        if let (Some(v), Some(mt)) = (&minitval, &mtranstype)
            && !pseudo(mt)
        {
            crate::types::text::parse_value(v, mt, self.ctx.fmt())?;
        }
        let parallel = match parallel.as_deref().map(str::to_lowercase).as_deref() {
            None | Some("unsafe") => 'u',
            Some("safe") => 's',
            Some("restricted") => 'r',
            Some(_) => {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    "parameter \"parallel\" must be SAFE, RESTRICTED, or UNSAFE",
                ));
            }
        };

        // AggregateCreate.
        let input: Vec<DataType> = args.iter().map(|a| a.1.clone()).collect();
        if let Some(detail) = poly_unresolved(&transtype, &input) {
            return Err(invalid("cannot determine transition data type").detail(detail));
        }
        if let Some(mt) = &mtranstype
            && let Some(detail) = poly_unresolved(mt, &input)
        {
            return Err(invalid("cannot determine transition data type").detail(detail));
        }
        if kind != 'n' && variadic.as_ref().is_some_and(|v| v.oid() != ANY) {
            return Err(invalid("a variadic ordered-set aggregate must use VARIADIC type ANY"));
        }
        if kind == 'h' && num_direct < input.len() {
            let numaggr = input.len() - num_direct;
            if variadic.is_some() || num_direct < numaggr {
                return Err(invalid(
                    "a hypothetical-set aggregate must have direct arguments matching its aggregated arguments",
                ));
            }
            for i in 0..numaggr {
                if input[num_direct - numaggr + i].oid() != input[num_direct + i].oid() {
                    return Err(invalid(
                        "a hypothetical-set aggregate must have direct arguments matching its aggregated arguments",
                    ));
                }
            }
        }
        // The transition function's arguments.
        let mut targs = vec![transtype.clone()];
        if kind != 'n' {
            let n = if num_direct < input.len() {
                input.len() - num_direct
            } else {
                1
            };
            targs.extend(input[input.len() - n..].iter().cloned());
        } else {
            targs.extend(input.iter().cloned());
        }
        let (tfn, tret, tstrict) = self.agg_function(&d_sfunc, &targs)?;
        if tret.oid() != transtype.oid() {
            return Err(Error::new(
                code::DATATYPE_MISMATCH,
                format!(
                    "return type of transition function {} is not {}",
                    d_sfunc.join("."),
                    transtype.display()
                ),
            ));
        }
        let strict_bad = |init: &Option<String>, tt: &DataType| {
            init.is_none() && input.first().is_none_or(|t| !self.binary_ok(t, tt))
        };
        if tstrict && strict_bad(&initval, &transtype) {
            return Err(invalid(
                "must not omit initial value when transition function is strict and transition type is not compatible with input type",
            ));
        }
        let (mut mtfn, mut minvfn, mut mfinalfn) = (0, 0, 0);
        if let Some(mt) = &mtranstype {
            let mut margs = targs.clone();
            margs[0] = mt.clone();
            let (f, r, strict) = self.agg_function(&d_msfunc, &margs)?;
            if r.oid() != mt.oid() {
                return Err(Error::new(
                    code::DATATYPE_MISMATCH,
                    format!(
                        "return type of transition function {} is not {}",
                        d_msfunc.join("."),
                        mt.display()
                    ),
                ));
            }
            if strict && strict_bad(&minitval, mt) {
                return Err(invalid(
                    "must not omit initial value when transition function is strict and transition type is not compatible with input type",
                ));
            }
            mtfn = f;
            let (fi, ri, istrict) = self.agg_function(&d_minvfunc, &margs)?;
            if ri.oid() != mt.oid() {
                return Err(Error::new(
                    code::DATATYPE_MISMATCH,
                    format!(
                        "return type of inverse transition function {} is not {}",
                        d_minvfunc.join("."),
                        mt.display()
                    ),
                ));
            }
            if istrict != strict {
                return Err(invalid(
                    "strictness of aggregate's forward and inverse transition functions must match",
                ));
            }
            minvfn = fi;
        }
        // The final function: (state, direct args[, aggregated args]).
        let final_args = |st: &DataType, extra: bool| {
            let mut a = vec![st.clone()];
            let n = if extra { input.len() } else { num_direct.min(input.len()) };
            a.extend(input[..n].iter().cloned());
            a
        };
        let (ffn, finaltype) = if d_finalfn.is_empty() {
            (0, transtype.clone())
        } else {
            let (f, r, strict) = self.agg_function(&d_finalfn, &final_args(&transtype, finalextra))?;
            if finalextra && strict {
                return Err(invalid(
                    "final function with extra arguments must not be declared STRICT",
                ));
            }
            (f, r)
        };
        let mut combinefn = 0;
        if !d_combine.is_empty() {
            let (f, r, strict) =
                self.agg_function(&d_combine, &[transtype.clone(), transtype.clone()])?;
            if r.oid() != transtype.oid() {
                return Err(Error::new(
                    code::DATATYPE_MISMATCH,
                    format!(
                        "return type of combine function {} is not {}",
                        d_combine.join("."),
                        transtype.display()
                    ),
                ));
            }
            if is_internal(&transtype) && strict {
                return Err(invalid(
                    "combine function with transition type internal must not be declared STRICT",
                ));
            }
            combinefn = f;
        }
        let internal = self.sig_type(INTERNAL);
        let bytea = self.sig_type(BYTEA);
        let mut serialfn = 0;
        if !d_serial.is_empty() {
            let (f, r, _) = self.agg_function(&d_serial, std::slice::from_ref(&internal))?;
            if r.oid() != BYTEA {
                return Err(Error::new(
                    code::DATATYPE_MISMATCH,
                    format!(
                        "return type of serialization function {} is not bytea",
                        d_serial.join(".")
                    ),
                ));
            }
            serialfn = f;
        }
        let mut deserialfn = 0;
        if !d_deserial.is_empty() {
            let (f, r, _) = self.agg_function(&d_deserial, &[bytea, internal.clone()])?;
            if r.oid() != INTERNAL {
                return Err(Error::new(
                    code::DATATYPE_MISMATCH,
                    format!(
                        "return type of deserialization function {} is not internal",
                        d_deserial.join(".")
                    ),
                ));
            }
            deserialfn = f;
        }
        if let Some(detail) = poly_unresolved(&finaltype, &input) {
            return Err(invalid("cannot determine result data type").detail(detail));
        }
        if is_internal(&finaltype) && !input.iter().any(is_internal) {
            return Err(invalid("unsafe use of pseudo-type \"internal\"").detail(
                "A result of type internal requires at least one input of type internal.",
            ));
        }
        if let Some(mt) = &mtranstype {
            let mret = if d_mfinal.is_empty() {
                mt.clone()
            } else {
                let (f, r, strict) = self.agg_function(&d_mfinal, &final_args(mt, mfinalextra))?;
                if mfinalextra && strict {
                    return Err(invalid(
                        "final function with extra arguments must not be declared STRICT",
                    ));
                }
                mfinalfn = f;
                r
            };
            if mret.oid() != finaltype.oid() {
                return Err(invalid(&format!(
                    "moving-aggregate implementation returns type {}, but plain implementation returns type {}",
                    mret.display(),
                    finaltype.display()
                )));
            }
        }
        let mut sortop = 0;
        if !d_sortop.is_empty() {
            if input.len() != 1 {
                return Err(invalid(
                    "sort operator can only be specified for single-argument aggregates",
                ));
            }
            let t = crate::catalog::system::type_oid(&self.cat, &input[0]);
            sortop = match self.find_oper(&d_sortop, t, t)? {
                Some(o) => o.oid,
                None => {
                    return Err(Error::new(
                        code::UNDEFINED_FUNCTION,
                        format!(
                            "operator does not exist: {} {} {}",
                            input[0].display(),
                            d_sortop.join("."),
                            input[0].display()
                        ),
                    ));
                }
            };
        }
        let spec = AggSpec {
            kind,
            num_direct: num_direct as i16,
            transfn: tfn,
            finalfn: ffn,
            combinefn,
            serialfn,
            deserialfn,
            mtransfn: mtfn,
            minvtransfn: minvfn,
            mfinalfn,
            finalextra,
            mfinalextra,
            finalmodify,
            mfinalmodify,
            sortop,
            transtype,
            transspace,
            mtranstype,
            mtransspace,
            initval,
            minitval,
        };
        // ProcedureCreate: the same name and arguments replaces with OR REPLACE.
        let existing = self
            .cat
            .functions
            .values()
            .find(|f| {
                f.schema == schema
                    && f.name == name
                    && f.args.iter().map(|a| a.1.oid()).eq(input.iter().map(|t| t.oid()))
            })
            .cloned();
        let (oid, owner) = match existing {
            Some(f) if s.replace => {
                self.require_owner(f.owner, "function", &f.name)?;
                let Some(old) = f.agg.as_deref() else {
                    let what = if f.procedure { "procedure" } else { "function" };
                    return Err(
                        Error::new(code::WRONG_OBJECT_TYPE, "cannot change routine kind")
                            .detail(format!("\"{}\" is a {what}.", f.name)),
                    );
                };
                if f.ret.oid() != finaltype.oid() {
                    let sp = self.search_path().to_vec();
                    return Err(Error::new(
                        code::INVALID_FUNCTION_DEFINITION,
                        "cannot change return type of existing function",
                    )
                    .hint(format!(
                        "Use DROP AGGREGATE {} first.",
                        crate::catalog::depend::procedure_description(&self.cat, f.oid, &sp)
                            .unwrap_or_default()
                    )));
                }
                if old.kind != kind {
                    return Err(
                        Error::new(code::WRONG_OBJECT_TYPE, "cannot change routine kind").detail(
                            match old.kind {
                                'o' => format!("\"{}\" is an ordered-set aggregate.", f.name),
                                'h' => format!("\"{}\" is a hypothetical-set aggregate.", f.name),
                                _ => format!("\"{}\" is an ordinary aggregate function.", f.name),
                            },
                        ),
                    );
                }
                if old.num_direct as usize != num_direct {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        "cannot change number of direct arguments of an aggregate function",
                    ));
                }
                (f.oid, f.owner)
            }
            Some(_) => {
                return Err(Error::new(
                    code::DUPLICATE_FUNCTION,
                    format!("function \"{name}\" already exists with same argument types"),
                ));
            }
            None => (self.cat.alloc_oid(), self.ctx.current_user()),
        };
        if modes.iter().all(|m| *m == 'i') {
            modes.clear();
        }
        if !arg_domains.iter().any(Option::is_some) || arg_domains.len() != args.len() {
            arg_domains.clear();
        }
        let func = Function {
            oid,
            schema,
            name,
            args,
            ret: finaltype,
            returns_set: false,
            language: "internal".into(),
            body: "aggregate_dummy".into(),
            volatility: 'i',
            strict: false,
            defaults: Vec::new(),
            out_cols: Vec::new(),
            owner,
            security_definer: false,
            procedure: false,
            modes,
            agg: Some(Box::new(spec)),
            cost: 0.0,
            rows: 0.0,
            parallel,
            leakproof: false,
            config: Vec::new(),
            arg_domains,
            ret_domain: None,
            out_domains: Vec::new(),
            sql_body: None,
            atomic_returns: Vec::new(),
            probin: None,
        };
        std::sync::Arc::make_mut(&mut self.cat.functions).insert(oid, Arc::new(func));
        self.save()?;
        Ok(TAG.into())
    }
}

/// check_generic_type_consistency's anyelement family: the arguments for anyelement (and the
/// element types of those for anyarray) all alike, a polymorphic argument counting as its own
/// type (an `anyelement` state argument doesn't match `anyarray` given `int[]`).
fn generic_consistent(declared: &[DataType], actual: &[DataType]) -> bool {
    let mut elem: Option<u32> = None;
    for (d, a) in declared.iter().zip(actual) {
        if matches!(a, DataType::Unknown) {
            continue;
        }
        let e = match d {
            DataType::AnyElement | DataType::AnyNonArray | DataType::AnyEnum => a.oid(),
            // anyarray given anyarray: allowed here (enforcing may complain later).
            DataType::AnyArray if matches!(a, DataType::AnyArray) => continue,
            DataType::AnyArray => match a.elem() {
                Some(e) => e.oid(),
                None => return false,
            },
            _ => continue,
        };
        match elem {
            Some(x) if x != e => return false,
            _ => elem = Some(e),
        }
    }
    true
}
