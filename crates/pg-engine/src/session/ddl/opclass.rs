//! CREATE / ALTER / DROP OPERATOR FAMILY and OPERATOR CLASS (opclasscmds.c): families and
//! classes of index access methods, with their operators (pg_amop) and support functions
//! (pg_amproc).

use super::Ddl;
use crate::catalog::PG_CATALOG_OID;
use crate::catalog::userobj::{AmInfo, ClassInfo, FamilyInfo, OpClass, OpFamily, OpMember, am_traits};
use crate::error::{Error, Result, code};
use crate::sql::binder::ast::{inner, names};
use crate::sql::binder::resolve_type_name;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

const BTREE: u32 = 403;
const HASH: u32 = 405;
const INTERNAL: u32 = 2281;
const VOID: u32 = 2278;
const BOOL: u32 = 16;
const INT4: u32 = 23;
const INT8: u32 = 20;

/// An operator as pg_operator has it.
#[derive(Clone, Debug)]
pub(crate) struct OperInfo {
    pub oid: u32,
    pub kind: char,
    pub left: u32,
    pub right: u32,
    pub result: u32,
}

/// A function as pg_proc has it.
#[derive(Clone, Debug)]
pub(crate) struct ProcInfo {
    pub oid: u32,
    pub args: Vec<u32>,
    pub ret: u32,
}

/// OpFamilyMember: an operator or support function a statement names.
#[derive(Clone, Debug)]
struct Member {
    func: bool,
    object: u32,
    number: i16,
    left: u32,
    right: u32,
    sortfamily: u32,
    /// The types of the operator's or function's inputs (typeDepNeeded).
    inputs: Vec<u32>,
    hard: bool,
    refclass: u32,
}

fn invalid(msg: impl Into<String>) -> Error {
    Error::new(code::INVALID_OBJECT_DEFINITION, msg)
}

/// The rows of the built-in pg_operator: oid, name, left, right, result, kind.
fn builtin_operators() -> &'static [(u32, &'static str, u32, u32, u32, char)] {
    static ROWS: std::sync::OnceLock<Vec<(u32, &'static str, u32, u32, u32, char)>> =
        std::sync::OnceLock::new();
    ROWS.get_or_init(|| {
        include_str!("../../catalog/data/pg_operator.tsv")
            .lines()
            .map(|l| l.split('\t').collect::<Vec<_>>())
            .filter(|f| f.len() >= 12)
            .map(|f| {
                let n = |i: usize| f[i].parse::<u32>().unwrap_or(0);
                (n(0), f[1], n(7), n(8), n(9), f[4].chars().next().unwrap_or('b'))
            })
            .collect()
    })
}

/// The operator `oid`, built-in or made.
pub(crate) fn operator_info(cat: &crate::catalog::Catalog, oid: u32) -> Option<OperInfo> {
    if let Some(o) = cat.operators.get(&oid) {
        return Some(OperInfo {
            oid,
            kind: if o.left.is_some() { 'b' } else { 'l' },
            left: o.left.as_ref().map_or(0, |t| t.oid()),
            right: o.right.oid(),
            result: o.result.as_ref().map_or(0, |t| t.oid()),
        });
    }
    builtin_operators()
        .iter()
        .find(|r| r.0 == oid)
        .map(|r| OperInfo {
            oid,
            kind: r.5,
            left: r.2,
            right: r.3,
            result: r.4,
        })
}

/// The function `oid`, built-in or made.
pub(crate) fn proc_info(cat: &crate::catalog::Catalog, oid: u32) -> Option<ProcInfo> {
    if let Some(f) = cat.functions.get(&oid) {
        return Some(ProcInfo {
            oid,
            args: (0..f.args.len())
                .map(|i| {
                    f.arg_domain(i)
                        .unwrap_or_else(|| crate::catalog::system::type_oid(cat, &f.args[i].1))
                })
                .collect(),
            ret: f
                .ret_domain
                .unwrap_or_else(|| crate::catalog::system::type_oid(cat, &f.ret)),
        });
    }
    crate::catalog::system::builtin_proc_sigs()
        .iter()
        .find(|p| p.oid == oid)
        .map(|p| ProcInfo {
            oid,
            args: p.args.clone(),
            ret: p.ret,
        })
}

impl Ddl<'_, '_> {
    /// typenameTypeId: a type name's oid (a domain's own; pseudo-types Postgres's internals
    /// use, like `internal`, included).
    pub(super) fn type_name_oid(&self, tn: &pb::TypeName) -> Result<u32> {
        match resolve_type_name(&self.cat, self.search_path(), tn) {
            Ok((t, d)) => Ok(d.unwrap_or_else(|| crate::catalog::system::type_oid(&self.cat, &t))),
            Err(e) => {
                let parts = names(&tn.names);
                let plain = match parts.as_slice() {
                    [n] => Some(n.as_str()),
                    [s, n] if s == "pg_catalog" => Some(n.as_str()),
                    _ => None,
                };
                if tn.array_bounds.is_empty()
                    && let Some(o) = plain.and_then(crate::types::oid::other_pseudo_oid)
                {
                    return Ok(o);
                }
                Err(e)
            }
        }
    }

    /// format_type_be.
    pub(super) fn type_text(&self, oid: u32) -> String {
        crate::sql::func::system::format_type_oid(oid, -1, &self.cat, self.search_path())
            .unwrap_or_else(|| "???".into())
    }

    /// The schemas a (possibly qualified) name is looked up in (LookupExplicitNamespace).
    fn lookup_schemas(&self, parts: &[String]) -> Result<Vec<u32>> {
        Ok(match parts {
            [.., sc, _] => vec![self.cat.schema_oid(sc)?],
            _ => self.cat.effective_path(self.search_path()),
        })
    }

    /// OpernameGetOprid: an operator by name and exact input types (`left` 0 for a prefix
    /// operator).
    pub(crate) fn find_oper(&self, parts: &[String], left: u32, right: u32) -> Result<Option<OperInfo>> {
        let name = parts.last().cloned().unwrap_or_default();
        for s in self.lookup_schemas(parts)? {
            if s == PG_CATALOG_OID
                && let Some(r) = builtin_operators()
                    .iter()
                    .find(|r| r.1 == name && r.2 == left && r.3 == right)
            {
                return Ok(operator_info(&self.cat, r.0));
            }
            if let Some(o) = self.cat.operators.values().find(|o| {
                o.schema == s
                    && o.name == name
                    && o.left.as_ref().map_or(0, |t| t.oid()) == left
                    && o.right.oid() == right
            }) {
                return Ok(operator_info(&self.cat, o.oid));
            }
        }
        Ok(None)
    }

    /// LookupOperName (an exact match, else an error).
    fn lookup_oper(&self, parts: &[String], left: u32, right: u32) -> Result<OperInfo> {
        if let Some(o) = self.find_oper(parts, left, right)? {
            return Ok(o);
        }
        let name = parts.join(".");
        let sig = if left == 0 {
            format!("{name} {}", self.type_text(right))
        } else {
            format!("{} {name} {}", self.type_text(left), self.type_text(right))
        };
        Err(Error::new(
            code::UNDEFINED_FUNCTION,
            format!("operator does not exist: {sig}"),
        ))
    }

    /// LookupOperWithArgs.
    pub(crate) fn lookup_oper_args(&self, owa: &pb::ObjectWithArgs) -> Result<OperInfo> {
        let mut types = Vec::new();
        for a in &owa.objargs {
            types.push(match inner(a) {
                Some(NodeEnum::TypeName(tn)) => self.type_name_oid(tn)?,
                _ => 0,
            });
        }
        let (left, right) = match types.as_slice() {
            [l, r] => (*l, *r),
            [r] => (0, *r),
            _ => (0, 0),
        };
        self.lookup_oper(&names(&owa.objname), left, right)
    }

    /// LookupFuncWithArgs(OBJECT_FUNCTION): a function by name and exact argument types.
    pub(crate) fn lookup_proc(&self, owa: &pb::ObjectWithArgs) -> Result<ProcInfo> {
        let parts = names(&owa.objname);
        let name = parts.last().cloned().unwrap_or_default();
        let mut types = Vec::new();
        for a in &owa.objargs {
            if let Some(NodeEnum::TypeName(tn)) = inner(a) {
                types.push(self.type_name_oid(tn)?);
            }
        }
        for s in self.lookup_schemas(&parts)? {
            let mine = self.cat.functions_named(&name).find(|f| {
                f.schema == s
                    && proc_info(&self.cat, f.oid).is_some_and(|p| p.args == types)
            });
            if let Some(f) = mine {
                if f.procedure {
                    return Err(Error::new(
                        code::WRONG_OBJECT_TYPE,
                        format!("{} is not a function", self.func_sig_text(&parts, &types)),
                    ));
                }
                return Ok(proc_info(&self.cat, f.oid).unwrap());
            }
            if s == PG_CATALOG_OID
                && let Some(p) = crate::catalog::system::builtin_proc_sigs()
                    .iter()
                    .find(|p| p.name == name && p.args == types)
            {
                return Ok(ProcInfo {
                    oid: p.oid,
                    args: p.args.clone(),
                    ret: p.ret,
                });
            }
        }
        Err(Error::new(
            code::UNDEFINED_FUNCTION,
            format!(
                "function {} does not exist",
                self.func_sig_text(&parts, &types)
            ),
        ))
    }

    /// func_signature_string.
    fn func_sig_text(&self, parts: &[String], types: &[u32]) -> String {
        let t: Vec<String> = types.iter().map(|o| self.type_text(*o)).collect();
        format!("{}({})", parts.join("."), t.join(", "))
    }

    /// QualifiedNameGetCreationNamespace.
    fn creation_target(&mut self, parts: &[String]) -> Result<(u32, String)> {
        match parts {
            [.., sc, n] => {
                let s = if sc == "pg_temp" {
                    self.temp_schema()?
                } else {
                    self.cat.schema_oid(sc)?
                };
                Ok((s, crate::catalog::truncate_ident(n)))
            }
            [n] => Ok((
                self.cat.creation_schema(self.search_path())?,
                crate::catalog::truncate_ident(n),
            )),
            [] => Err(crate::error::internal("object name")),
        }
    }

    /// get_opfamily_oid.
    pub(super) fn opfamily_named(&self, am: &AmInfo, parts: &[String]) -> Result<FamilyInfo> {
        if let [.., sc, _] = parts {
            self.require_schema_usage(sc)?;
        }
        self.cat
            .find_opfamily(am.oid, parts, self.search_path())?
            .ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!(
                        "operator family \"{}\" does not exist for access method \"{}\"",
                        parts.join("."),
                        am.name
                    ),
                )
            })
    }

    /// get_opclass_oid.
    pub(super) fn opclass_named(&self, am: &AmInfo, parts: &[String]) -> Result<ClassInfo> {
        if let [.., sc, _] = parts {
            self.require_schema_usage(sc)?;
        }
        self.cat
            .find_opclass(am.oid, parts, self.search_path())?
            .ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!(
                        "operator class \"{}\" does not exist for access method \"{}\"",
                        parts.join("."),
                        am.name
                    ),
                )
            })
    }

    fn insert_opfamily(&mut self, am: &AmInfo, schema: u32, name: &str) -> Result<u32> {
        if self
            .cat
            .opfamilies_all()
            .iter()
            .any(|f| f.method == am.oid && f.schema == schema && f.name == name)
        {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!(
                    "operator family \"{name}\" for access method \"{}\" already exists",
                    am.name
                ),
            ));
        }
        let oid = self.cat.alloc_oid();
        self.cat.opfamilies.insert(
            oid,
            Arc::new(OpFamily {
                oid,
                method: am.oid,
                name: name.to_string(),
                schema,
                owner: self.me(),
            }),
        );
        Ok(oid)
    }

    /// CREATE OPERATOR FAMILY.
    pub(super) fn create_opfamily(&mut self, s: &pb::CreateOpFamilyStmt) -> Result<String> {
        let (schema, name) = self.creation_target(&names(&s.opfamilyname))?;
        self.require_schema_create(schema)?;
        let am = self.cat.index_am(&s.amname)?;
        if !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "must be superuser to create an operator family",
            ));
        }
        self.insert_opfamily(&am, schema, &name)?;
        self.save()?;
        Ok("CREATE OPERATOR FAMILY".into())
    }

    /// assignOperTypes.
    fn assign_oper_types(&self, m: &mut Member, am: &AmInfo, op: &OperInfo) -> Result<()> {
        if op.kind != 'b' {
            return Err(invalid("index operators must be binary"));
        }
        if m.sortfamily != 0 {
            if !am_traits(&self.cat, am.handler).is_some_and(|t| t.can_order_by_op) {
                return Err(invalid(format!(
                    "access method \"{}\" does not support ordering operators",
                    am.name
                )));
            }
        } else if op.result != BOOL {
            return Err(invalid("index search operators must return boolean"));
        }
        if m.left == 0 {
            m.left = op.left;
        }
        if m.right == 0 {
            m.right = op.right;
        }
        m.inputs = vec![op.left, op.right];
        Ok(())
    }

    /// assignProcTypes.
    fn assign_proc_types(
        &self,
        m: &mut Member,
        am: &AmInfo,
        typeoid: u32,
        opts_proc: i32,
        p: &ProcInfo,
    ) -> Result<()> {
        let n = m.number as i32;
        let builtin_am = self.cat.access_method(am.oid).map_or(am.oid, |a| {
            // A made access method checks support functions as its handler's would.
            match a.handler {
                330 => BTREE,
                331 => HASH,
                _ => a.oid,
            }
        });
        if n == opts_proc {
            if typeoid != 0 {
                if (m.left != 0 && m.left != typeoid) || (m.right != 0 && m.right != typeoid) {
                    return Err(invalid("associated data types for operator class options parsing functions must match opclass input type"));
                }
            } else if m.left != m.right {
                return Err(invalid("left and right associated data types for operator class options parsing functions must match"));
            }
            if p.ret != VOID || p.args.len() != 1 || p.args[0] != INTERNAL {
                return Err(invalid("invalid operator class options parsing function")
                    .hint("Valid signature of operator class options parsing function is (internal) RETURNS void."));
            }
        } else if builtin_am == BTREE {
            match n {
                1 => {
                    if p.args.len() != 2 {
                        return Err(invalid("ordering comparison functions must have two arguments"));
                    }
                    if p.ret != INT4 {
                        return Err(invalid("ordering comparison functions must return integer"));
                    }
                    if m.left == 0 {
                        m.left = p.args[0];
                    }
                    if m.right == 0 {
                        m.right = p.args[1];
                    }
                }
                2 => {
                    if p.args.len() != 1 || p.args[0] != INTERNAL {
                        return Err(invalid("btree sort support functions must accept type \"internal\""));
                    }
                    if p.ret != VOID {
                        return Err(invalid("btree sort support functions must return void"));
                    }
                }
                3 => {
                    if p.args.len() != 5 {
                        return Err(invalid("btree in_range functions must have five arguments"));
                    }
                    if p.ret != BOOL {
                        return Err(invalid("btree in_range functions must return boolean"));
                    }
                    if m.left == 0 {
                        m.left = p.args[0];
                    }
                    if m.right == 0 {
                        m.right = p.args[2];
                    }
                }
                4 => {
                    if p.args.len() != 1 {
                        return Err(invalid("btree equal image functions must have one argument"));
                    }
                    if p.ret != BOOL {
                        return Err(invalid("btree equal image functions must return boolean"));
                    }
                    if m.left != m.right {
                        return Err(invalid("ordering equal image functions must not be cross-type"));
                    }
                }
                6 => {
                    if p.args.len() != 1 || p.args[0] != INTERNAL {
                        return Err(invalid("btree skip support functions must accept type \"internal\""));
                    }
                    if p.ret != VOID {
                        return Err(invalid("btree skip support functions must return void"));
                    }
                    if m.left != m.right {
                        return Err(invalid("btree skip support functions must not be cross-type"));
                    }
                }
                _ => {}
            }
        } else if builtin_am == HASH {
            if n == 1 {
                if p.args.len() != 1 {
                    return Err(invalid("hash function 1 must have one argument"));
                }
                if p.ret != INT4 {
                    return Err(invalid("hash function 1 must return integer"));
                }
            } else if n == 2 {
                if p.args.len() != 2 {
                    return Err(invalid("hash function 2 must have two arguments"));
                }
                if p.ret != INT8 {
                    return Err(invalid("hash function 2 must return bigint"));
                }
            }
            if m.left == 0 {
                m.left = p.args.first().copied().unwrap_or(0);
            }
            if m.right == 0 {
                m.right = p.args.first().copied().unwrap_or(0);
            }
        }
        if m.left == 0 {
            m.left = typeoid;
        }
        if m.right == 0 {
            m.right = typeoid;
        }
        if m.left == 0 || m.right == 0 {
            return Err(invalid(
                "associated data types must be specified for index support function",
            ));
        }
        m.inputs = p.args.clone();
        Ok(())
    }

    /// addFamilyMember.
    fn add_opc_member(&self, list: &mut Vec<Member>, m: Member) -> Result<()> {
        if list
            .iter()
            .any(|o| o.number == m.number && o.left == m.left && o.right == m.right)
        {
            return Err(invalid(format!(
                "{} number {} for ({},{}) appears more than once",
                if m.func { "function" } else { "operator" },
                m.number,
                self.type_text(m.left),
                self.type_text(m.right)
            )));
        }
        list.push(m);
        Ok(())
    }

    /// processTypesSpec.
    fn types_spec(&self, args: &[pb::Node]) -> Result<(u32, u32)> {
        let mut oids = Vec::new();
        for a in args {
            if let Some(NodeEnum::TypeName(tn)) = inner(a) {
                if oids.len() == 2 {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        "one or two argument types must be specified",
                    ));
                }
                oids.push(self.type_name_oid(tn)?);
            }
        }
        Ok(match oids.as_slice() {
            [l] => (*l, *l),
            [l, r] => (*l, *r),
            _ => (0, 0),
        })
    }

    /// The opclass whose input type is `ty` in family `family` (opclass_for_family_datatype).
    fn class_for_family_type(&self, am: u32, family: u32, ty: u32) -> u32 {
        self.cat
            .opclasses_all()
            .iter()
            .find(|c| c.method == am && c.family == family && c.intype == ty)
            .map_or(0, |c| c.oid)
    }

    /// The access method's amadjustmembers: which members are hard dependencies of their
    /// class.
    fn adjust_members(&self, am: &AmInfo, family: u32, class: u32, members: &mut [Member]) {
        let soft = |m: &mut Member| {
            m.hard = false;
            m.refclass = 0;
        };
        let opcintype = self.cat.opclass_info(class).map_or(0, |c| c.intype);
        match am.handler {
            330 | 331 => {
                // BTORDER_PROC / HASHSTANDARD_PROC.
                let required = 1;
                let mut cache: Option<(u32, u32)> = None;
                for m in members.iter_mut() {
                    if (m.func && m.number != required) || m.left != m.right {
                        soft(m);
                        continue;
                    }
                    let c = if class != 0 && m.left == opcintype {
                        class
                    } else {
                        match cache {
                            Some((t, c)) if t == m.left => c,
                            _ => {
                                let c = self.class_for_family_type(am.oid, family, m.left);
                                cache = Some((m.left, c));
                                c
                            }
                        }
                    };
                    if c != 0 {
                        m.hard = true;
                        m.refclass = c;
                    } else {
                        soft(m);
                    }
                }
            }
            332..=335 => {
                let required: &[i16] = match am.handler {
                    332 => &[1, 2, 5, 6, 7],
                    333 => &[2, 3],
                    334 => &[1, 2, 3, 4, 5],
                    _ => &[1, 2, 3, 4],
                };
                for m in members.iter_mut() {
                    if m.func && required.contains(&m.number) {
                        m.hard = true;
                    } else {
                        soft(m);
                    }
                }
            }
            _ => {}
        }
    }

    /// storeOperators / storeProcedures.
    fn store_members(
        &mut self,
        family_name: &str,
        family: u32,
        members: Vec<Member>,
        is_add: bool,
    ) -> Result<()> {
        for m in members {
            if is_add
                && self.cat.opmembers_all(m.func).iter().any(|x| {
                    x.family == family && x.left == m.left && x.right == m.right && x.number == m.number
                })
            {
                return Err(Error::new(
                    code::DUPLICATE_OBJECT,
                    format!(
                        "{} {}({},{}) already exists in operator family \"{family_name}\"",
                        if m.func { "function" } else { "operator" },
                        m.number,
                        self.type_text(m.left),
                        self.type_text(m.right)
                    ),
                ));
            }
            if !is_add
                && self.cat.opmembers_all(m.func).iter().any(|x| {
                    x.family == family && x.left == m.left && x.right == m.right && x.number == m.number
                })
            {
                let (index, cols) = if m.func {
                    (
                        "pg_amproc_fam_proc_index",
                        "amprocfamily, amproclefttype, amprocrighttype, amprocnum",
                    )
                } else {
                    (
                        "pg_amop_fam_strat_index",
                        "amopfamily, amoplefttype, amoprighttype, amopstrategy",
                    )
                };
                return Err(Error::new(
                    code::UNIQUE_VIOLATION,
                    format!("duplicate key value violates unique constraint \"{index}\""),
                )
                .detail(format!(
                    "Key ({cols})=({family}, {}, {}, {}) already exists.",
                    m.left, m.right, m.number
                )));
            }
            let oid = self.cat.alloc_oid();
            self.cat.opmembers.insert(
                oid,
                Arc::new(OpMember {
                    oid,
                    family,
                    func: m.func,
                    left: m.left,
                    right: m.right,
                    number: m.number,
                    object: m.object,
                    sortfamily: m.sortfamily,
                    hard: m.hard,
                    refclass: m.refclass,
                    inputs: m.inputs,
                }),
            );
        }
        Ok(())
    }

    /// The members a CREATE OPERATOR CLASS / ALTER OPERATOR FAMILY ADD lists.
    #[allow(clippy::too_many_arguments)]
    fn collect_members(
        &self,
        am: &AmInfo,
        items: &[pb::Node],
        typeoid: u32,
        alter: bool,
        storage: &mut u32,
        ops: &mut Vec<Member>,
        procs: &mut Vec<Member>,
    ) -> Result<()> {
        let t = am_traits(&self.cat, am.handler).unwrap_or(crate::catalog::userobj::AmTraits {
            strategies: 0,
            support: 0,
            opts_proc: 0,
            can_order_by_op: false,
            storage: false,
        });
        let max_op = if t.strategies == 0 { i16::MAX as i32 } else { t.strategies };
        let bad_number = |what: &str, n: i32, max: i32| {
            Error::new(
                code::INVALID_OBJECT_DEFINITION,
                format!("invalid {what} number {n}, must be between 1 and {max}"),
            )
        };
        for it in items {
            let Some(NodeEnum::CreateOpClassItem(item)) = inner(it) else {
                continue;
            };
            match item.itemtype {
                1 => {
                    if item.number <= 0 || item.number > max_op {
                        return Err(bad_number("operator", item.number, max_op));
                    }
                    let owa = item.name.clone().unwrap_or_default();
                    let op = if !owa.objargs.is_empty() {
                        self.lookup_oper_args(&owa)?
                    } else if alter {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            "operator argument types must be specified in ALTER OPERATOR FAMILY",
                        ));
                    } else {
                        self.lookup_oper(&names(&owa.objname), typeoid, typeoid)?
                    };
                    let sortfamily = if item.order_family.is_empty() {
                        0
                    } else {
                        let bt = self.cat.index_am("btree")?;
                        self.opfamily_named(&bt, &names(&item.order_family))?.oid
                    };
                    let mut m = Member {
                        func: false,
                        object: op.oid,
                        number: item.number as i16,
                        left: 0,
                        right: 0,
                        sortfamily,
                        inputs: Vec::new(),
                        hard: false,
                        refclass: 0,
                    };
                    self.assign_oper_types(&mut m, am, &op)?;
                    self.add_opc_member(ops, m)?;
                }
                2 => {
                    if item.number <= 0 || item.number > t.support {
                        return Err(bad_number("function", item.number, t.support));
                    }
                    let owa = item.name.clone().unwrap_or_default();
                    let p = self.lookup_proc(&owa)?;
                    let (left, right) = if item.class_args.is_empty() {
                        (0, 0)
                    } else {
                        self.types_spec(&item.class_args)?
                    };
                    let mut m = Member {
                        func: true,
                        object: p.oid,
                        number: item.number as i16,
                        left,
                        right,
                        sortfamily: 0,
                        inputs: Vec::new(),
                        hard: false,
                        refclass: 0,
                    };
                    self.assign_proc_types(&mut m, am, typeoid, t.opts_proc, &p)?;
                    self.add_opc_member(procs, m)?;
                }
                _ => {
                    if alter {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            "STORAGE cannot be specified in ALTER OPERATOR FAMILY",
                        ));
                    }
                    if *storage != 0 {
                        return Err(invalid("storage type specified more than once"));
                    }
                    if let Some(tn) = &item.storedtype {
                        *storage = self.type_name_oid(tn)?;
                    }
                }
            }
        }
        Ok(())
    }

    /// CREATE OPERATOR CLASS (DefineOpClass).
    pub(super) fn create_opclass(&mut self, s: &pb::CreateOpClassStmt) -> Result<String> {
        let (schema, name) = self.creation_target(&names(&s.opclassname))?;
        self.require_schema_create(schema)?;
        let am = self.cat.index_am(&s.amname)?;
        if !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "must be superuser to create an operator class",
            ));
        }
        let typeoid = match &s.datatype {
            Some(tn) => self.type_name_oid(tn)?,
            None => 0,
        };
        let family = if !s.opfamilyname.is_empty() {
            self.opfamily_named(&am, &names(&s.opfamilyname))?.oid
        } else {
            match self
                .cat
                .opfamilies_all()
                .into_iter()
                .find(|f| f.method == am.oid && f.schema == schema && f.name == name)
            {
                Some(f) => f.oid,
                None => self.insert_opfamily(&am, schema, &name)?,
            }
        };
        let mut storage = 0;
        let (mut ops, mut procs) = (Vec::new(), Vec::new());
        self.collect_members(&am, &s.items, typeoid, false, &mut storage, &mut ops, &mut procs)?;
        if storage != 0 {
            if storage == typeoid {
                storage = 0;
            } else if !am_traits(&self.cat, am.handler).is_some_and(|t| t.storage) {
                return Err(invalid(format!(
                    "storage type cannot be different from data type for access method \"{}\"",
                    am.name
                )));
            }
        }
        let all = self.cat.opclasses_all();
        if all
            .iter()
            .any(|c| c.method == am.oid && c.schema == schema && c.name == name)
        {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!(
                    "operator class \"{name}\" for access method \"{}\" already exists",
                    am.name
                ),
            ));
        }
        if s.is_default
            && let Some(c) = all
                .iter()
                .find(|c| c.method == am.oid && c.intype == typeoid && c.default)
        {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!(
                    "could not make operator class \"{name}\" be default for type {}",
                    s.datatype.as_ref().map(|t| names(&t.names).join(".")).unwrap_or_default()
                ),
            )
            .detail(format!(
                "Operator class \"{}\" already is the default.",
                c.name
            )));
        }
        let oid = self.cat.alloc_oid();
        self.cat.opclasses.insert(
            oid,
            Arc::new(OpClass {
                oid,
                method: am.oid,
                name,
                schema,
                owner: self.me(),
                family,
                intype: typeoid,
                default: s.is_default,
                keytype: storage,
            }),
        );
        // CREATE OPERATOR CLASS members are part of the class, unless the access method
        // says otherwise.
        for m in ops.iter_mut().chain(procs.iter_mut()) {
            m.hard = true;
            m.refclass = oid;
        }
        self.adjust_members(&am, family, oid, &mut ops);
        self.adjust_members(&am, family, oid, &mut procs);
        let fname = names(&s.opfamilyname).join(".");
        self.store_members(&fname, family, ops, false)?;
        self.store_members(&fname, family, procs, false)?;
        self.save()?;
        Ok("CREATE OPERATOR CLASS".into())
    }

    /// ALTER OPERATOR FAMILY ... ADD / DROP.
    pub(super) fn alter_opfamily(&mut self, s: &pb::AlterOpFamilyStmt) -> Result<String> {
        let am = self.cat.index_am(&s.amname)?;
        let parts = names(&s.opfamilyname);
        let family = self.opfamily_named(&am, &parts)?;
        if !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "must be superuser to alter an operator family",
            ));
        }
        let fname = parts.join(".");
        if s.is_drop {
            let t = am_traits(&self.cat, am.handler).unwrap_or(crate::catalog::userobj::AmTraits {
                strategies: 0,
                support: 0,
                opts_proc: 0,
                can_order_by_op: false,
                storage: false,
            });
            let max_op = if t.strategies == 0 { i16::MAX as i32 } else { t.strategies };
            let mut list: Vec<Member> = Vec::new();
            for it in &s.items {
                let Some(NodeEnum::CreateOpClassItem(item)) = inner(it) else {
                    continue;
                };
                let func = item.itemtype == 2;
                let (max, what) = if func {
                    (t.support, "function")
                } else {
                    (max_op, "operator")
                };
                if item.number <= 0 || item.number > max {
                    return Err(Error::new(
                        code::INVALID_OBJECT_DEFINITION,
                        format!("invalid {what} number {}, must be between 1 and {max}", item.number),
                    ));
                }
                let (left, right) = self.types_spec(&item.class_args)?;
                let m = Member {
                    func,
                    object: 0,
                    number: item.number as i16,
                    left,
                    right,
                    sortfamily: 0,
                    inputs: Vec::new(),
                    hard: false,
                    refclass: 0,
                };
                let mut tmp = list.iter().filter(|x| x.func == func).cloned().collect();
                self.add_opc_member(&mut tmp, m.clone())?;
                list.push(m);
            }
            for func in [false, true] {
                for m in list.iter().filter(|m| m.func == func) {
                    let found = self.cat.opmembers_all(func).into_iter().find(|x| {
                        x.family == family.oid
                            && x.left == m.left
                            && x.right == m.right
                            && x.number == m.number
                    });
                    let Some(x) = found else {
                        return Err(Error::new(
                            code::UNDEFINED_OBJECT,
                            format!(
                                "{} {}({},{}) does not exist in operator family \"{fname}\"",
                                if func { "function" } else { "operator" },
                                m.number,
                                self.type_text(m.left),
                                self.type_text(m.right)
                            ),
                        ));
                    };
                    let class = if func {
                        crate::catalog::depend::AMPROC
                    } else {
                        crate::catalog::depend::AMOP
                    };
                    self.drop_objects(&[(class, x.oid, 0)], false)?;
                }
            }
        } else {
            let mut storage = 0;
            let (mut ops, mut procs) = (Vec::new(), Vec::new());
            self.collect_members(&am, &s.items, 0, true, &mut storage, &mut ops, &mut procs)?;
            self.adjust_members(&am, family.oid, 0, &mut ops);
            self.adjust_members(&am, family.oid, 0, &mut procs);
            self.store_members(&fname, family.oid, ops, true)?;
            self.store_members(&fname, family.oid, procs, true)?;
        }
        self.save()?;
        Ok("ALTER OPERATOR FAMILY".into())
    }

    /// The access method and name of a DROP / ALTER OPERATOR CLASS / FAMILY object
    /// (`USING am` comes first in its name list).
    pub(super) fn opc_object(&self, list: &[String]) -> Result<(AmInfo, Vec<String>)> {
        let am_name = list.first().cloned().unwrap_or_default();
        let am = self.cat.index_am(&am_name)?;
        Ok((am, list[1..].to_vec()))
    }

    /// The operator class or family a DROP names, checked droppable; None when missing with
    /// IF EXISTS (after the notice).
    pub(super) fn opc_to_drop(
        &self,
        family: bool,
        list: &[String],
        missing_ok: bool,
    ) -> Result<Option<u32>> {
        let (am, parts) = self.opc_object(list)?;
        let what = if family { "operator family" } else { "operator class" };
        let found = if family {
            self.cat
                .find_opfamily(am.oid, &parts, self.search_path())
                .map(|f| f.map(|f| (f.oid, f.owner, f.schema, f.name)))
        } else {
            self.cat
                .find_opclass(am.oid, &parts, self.search_path())
                .map(|c| c.map(|c| (c.oid, c.owner, c.schema, c.name)))
        };
        let found = match found {
            Err(e) if missing_ok && e.code == code::UNDEFINED_SCHEMA => {
                self.notice(format!(
                    "schema \"{}\" does not exist, skipping",
                    parts[parts.len() - 2]
                ));
                return Ok(None);
            }
            r => r?,
        };
        match found {
            Some((oid, owner, schema, name)) => {
                self.require_owner_or_schema(owner, schema, what, &name)?;
                Ok(Some(oid))
            }
            None if missing_ok => {
                self.notice(format!(
                    "{what} \"{}\" does not exist for access method \"{}\", skipping",
                    parts.join("."),
                    am.name
                ));
                Ok(None)
            }
            None => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!(
                    "{what} \"{}\" does not exist for access method \"{}\"",
                    parts.join("."),
                    am.name
                ),
            )),
        }
    }

    /// The name list of an ALTER ... RENAME / OWNER / SET SCHEMA object.
    pub(super) fn object_list(obj: Option<&pb::Node>) -> Vec<String> {
        match obj.and_then(inner) {
            Some(NodeEnum::List(l)) => names(&l.items),
            _ => Vec::new(),
        }
    }

    fn opc_lookup(&self, family: bool, list: &[String]) -> Result<ClassOrFamily> {
        let (am, parts) = self.opc_object(list)?;
        Ok(if family {
            let f = self.opfamily_named(&am, &parts)?;
            ClassOrFamily {
                oid: f.oid,
                am,
                name: f.name,
                schema: f.schema,
                owner: f.owner,
            }
        } else {
            let c = self.opclass_named(&am, &parts)?;
            ClassOrFamily {
                oid: c.oid,
                am,
                name: c.name,
                schema: c.schema,
                owner: c.owner,
            }
        })
    }

    fn opc_conflict(&self, family: bool, x: &ClassOrFamily, name: &str, schema: u32) -> Result<()> {
        let taken = if family {
            self.cat
                .opfamilies_all()
                .iter()
                .any(|f| f.method == x.am.oid && f.schema == schema && f.name == name)
        } else {
            self.cat
                .opclasses_all()
                .iter()
                .any(|c| c.method == x.am.oid && c.schema == schema && c.name == name)
        };
        if taken {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!(
                    "{} \"{name}\" for access method \"{}\" already exists in schema \"{}\"",
                    if family { "operator family" } else { "operator class" },
                    x.am.name,
                    self.cat.schema_name(schema)
                ),
            ));
        }
        Ok(())
    }

    fn opc_update(&mut self, family: bool, oid: u32, f: impl FnOnce(&mut String, &mut u32, &mut u32)) -> Result<()> {
        if family {
            let Some(x) = self.cat.opfamilies.get(&oid).cloned() else {
                return Err(builtin_alter());
            };
            let mut x = (*x).clone();
            f(&mut x.name, &mut x.schema, &mut x.owner);
            self.cat.opfamilies.insert(oid, Arc::new(x));
        } else {
            let Some(x) = self.cat.opclasses.get(&oid).cloned() else {
                return Err(builtin_alter());
            };
            let mut x = (*x).clone();
            f(&mut x.name, &mut x.schema, &mut x.owner);
            self.cat.opclasses.insert(oid, Arc::new(x));
        }
        Ok(())
    }

    /// ALTER OPERATOR CLASS / FAMILY ... RENAME TO (AlterObjectRename_internal).
    pub(super) fn rename_opc(&mut self, family: bool, s: &pb::RenameStmt) -> Result<String> {
        let x = self.opc_lookup(family, &Self::object_list(s.object.as_deref()))?;
        let what = if family { "operator family" } else { "operator class" };
        let newname = crate::catalog::truncate_ident(&s.newname);
        if !self.is_superuser() {
            self.require_owner(x.owner, what, &x.name)?;
            self.require_schema_create(x.schema)?;
        }
        self.opc_conflict(family, &x, &newname, x.schema)?;
        self.opc_update(family, x.oid, |n, _, _| *n = newname)?;
        self.save()?;
        Ok(format!("ALTER {}", what.to_uppercase()))
    }

    /// ALTER OPERATOR CLASS / FAMILY ... OWNER TO (AlterObjectOwner_internal).
    pub(super) fn owner_opc(&mut self, family: bool, obj: Option<&pb::Node>, new: u32) -> Result<()> {
        let x = self.opc_lookup(family, &Self::object_list(obj))?;
        let what = if family { "operator family" } else { "operator class" };
        if x.owner == new {
            return Ok(());
        }
        if !self.is_superuser() {
            self.require_owner(x.owner, what, &x.name)?;
            self.check_new_owner(new, Some(x.schema))?;
        }
        self.opc_update(family, x.oid, |_, _, o| *o = new)
    }

    /// ALTER OPERATOR CLASS / FAMILY ... SET SCHEMA (AlterObjectNamespace_internal).
    pub(super) fn set_schema_opc(&mut self, family: bool, s: &pb::AlterObjectSchemaStmt) -> Result<String> {
        let what = if family { "operator family" } else { "operator class" };
        let tag = format!("ALTER {}", what.to_uppercase());
        let x = self.opc_lookup(family, &Self::object_list(s.object.as_deref()))?;
        let nsp = self.cat.schema_oid(&s.newschema)?;
        self.check_set_namespace(x.schema, nsp)?;
        if x.schema == nsp {
            return Ok(tag);
        }
        if !self.is_superuser() {
            self.require_owner(x.owner, what, &x.name)?;
            self.require_schema_create(nsp)?;
        }
        self.opc_conflict(family, &x, &x.name.clone(), nsp)?;
        self.opc_update(family, x.oid, |_, sc, _| *sc = nsp)?;
        self.save()?;
        Ok(tag)
    }

    /// CheckSetNamespace.
    pub(super) fn check_set_namespace(&self, old: u32, new: u32) -> Result<()> {
        let temp = |o: u32| {
            self.cat
                .schemas
                .get(&o)
                .is_some_and(|s| s.name.starts_with("pg_temp") || s.name.starts_with("pg_toast_temp"))
        };
        if temp(old) || temp(new) {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of temporary schemas",
            ));
        }
        if old == crate::catalog::PG_TOAST_OID || new == crate::catalog::PG_TOAST_OID {
            return Err(Error::new(
                code::FEATURE_NOT_SUPPORTED,
                "cannot move objects into or out of TOAST schema",
            ));
        }
        Ok(())
    }
}

/// An operator class or family being altered.
struct ClassOrFamily {
    oid: u32,
    am: AmInfo,
    name: String,
    schema: u32,
    owner: u32,
}

fn builtin_alter() -> Error {
    Error::new(
        code::INSUFFICIENT_PRIVILEGE,
        "permission denied: system catalog objects cannot be altered",
    )
}
