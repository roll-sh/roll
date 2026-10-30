//! pg_depend, derived from the catalog: the dependencies Postgres records as each object is
//! created (the `recordDependencyOn*` calls of heap.c, index.c, pg_constraint.c, …), and
//! `getObjectDescription` for `pg_describe_object`.

use super::system::{SysTable, domain_constraint_oid, type_oid};
use super::{Catalog, ConstraintKind, IndexKey, PUBLIC_OID, Table, quote_ident};
use crate::exec::ExecCtx;
use crate::sql::binder::{Binder, Clause};
use crate::sql::expr::Expr;
use crate::types::{DataType, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

pub const REL: u32 = 1259;
pub const TYPE: u32 = 1247;
pub const PROC: u32 = 1255;
pub const NAMESPACE: u32 = 2615;
pub const CONSTRAINT: u32 = 2606;
pub const ATTRDEF: u32 = 2604;
pub const REWRITE: u32 = 2618;
pub const TRIGGER: u32 = 2620;
pub const POLICY: u32 = 3256;
pub const LANGUAGE: u32 = 2612;
pub const EXTENSION: u32 = 3079;
pub const STATEXT: u32 = 3381;
pub const OPERATOR: u32 = 2617;
pub const CAST: u32 = 2605;
pub const TSDICT: u32 = 3600;
pub const TSPARSER: u32 = 3601;
pub const TSCONFIG: u32 = 3602;
pub const TSTEMPLATE: u32 = 3764;
pub const COLLATION: u32 = 3456;
pub const AM: u32 = 2601;
pub const AMOP: u32 = 2602;
pub const AMPROC: u32 = 2603;
pub const OPCLASS: u32 = 2616;
pub const OPFAMILY: u32 = 2753;
pub const CONVERSION: u32 = 2607;
pub const TRANSFORM: u32 = 3576;
pub const FDW: u32 = 2328;
pub const SERVER: u32 = 1417;
pub const USER_MAPPING: u32 = 1418;
pub const PUBLICATION: u32 = 6104;
pub const PUBLICATION_REL: u32 = 6106;
pub const PUBLICATION_NAMESPACE: u32 = 6237;
pub const SUBSCRIPTION: u32 = 6100;
pub const EVENT_TRIGGER: u32 = 3466;

/// The pg_depend class of a kind of text search object.
pub fn ts_class(kind: super::tsearch::TsKind) -> u32 {
    use super::tsearch::TsKind as K;
    match kind {
        K::Config => TSCONFIG,
        K::Dict => TSDICT,
        K::Parser => TSPARSER,
        K::Template => TSTEMPLATE,
    }
}

/// The kind of text search object a pg_depend class holds.
pub fn ts_kind(class: u32) -> Option<super::tsearch::TsKind> {
    use super::tsearch::TsKind as K;
    Some(match class {
        TSCONFIG => K::Config,
        TSDICT => K::Dict,
        TSPARSER => K::Parser,
        TSTEMPLATE => K::Template,
        _ => return None,
    })
}

/// plpgsql: the language, its extension and the extension's functions.
pub const PLPGSQL_LANG: u32 = 13681;
pub const PLPGSQL_EXT: u32 = 13677;
const PLPGSQL_FUNCS: [u32; 3] = [13678, 13679, 13680];

/// An object: (catalog, oid, sub-id).
pub type Addr = (u32, u32, i32);

/// One pg_depend row: `obj` depends on `refobj`.
pub struct Depend {
    pub obj: Addr,
    pub refobj: Addr,
    pub kind: char,
}

/// The `_RETURN` rule of a view from before views kept their rule's oid (pg_rewrite): one
/// made up from the view's.
pub fn rule_oid(view: u32) -> u32 {
    view.wrapping_mul(1000) | 0x8000_0000
}

/// Objects made by initdb are pinned: nothing records a dependency on them (`IsPinnedObject`);
/// the public schema isn't.
fn pinned((class, oid, _): Addr) -> bool {
    oid < 12000 && !(class == NAMESPACE && oid == PUBLIC_OID)
}

/// A user type `t` stands for (`domain` when declared as one).
fn user_type(cat: &Catalog, t: &DataType, domain: Option<u32>) -> Option<u32> {
    let oid = domain.unwrap_or_else(|| type_oid(cat, t));
    let known = USER_TYPES.with(|u| u.borrow().as_ref().map(|s| s.contains(&oid)));
    match known {
        Some(true) => Some(oid),
        Some(false) => None,
        None => cat.user_type_name(oid).map(|_| oid),
    }
}

thread_local! {
    /// While `derived_depends` runs: the oids `Catalog::user_type_name` knows, looked up once
    /// (a search of every type and relation for each column made each DROP slow).
    static USER_TYPES: std::cell::RefCell<Option<std::collections::HashSet<u32>>> =
        const { std::cell::RefCell::new(None) };
}

/// `USER_TYPES` for the catalog `derived_depends` reads, until dropped.
struct UserTypes(Option<std::collections::HashSet<u32>>);

impl UserTypes {
    fn set(cat: &Catalog) -> UserTypes {
        UserTypes(USER_TYPES.with(|u| u.replace(Some(cat.user_type_oids()))))
    }
}

impl Drop for UserTypes {
    fn drop(&mut self) {
        USER_TYPES.with(|u| *u.borrow_mut() = self.0.take());
    }
}

fn is_relation(cat: &Catalog, oid: u32) -> bool {
    cat.tables.contains_key(&oid)
        || cat.views.contains_key(&oid)
        || cat.sequences.contains_key(&oid)
        || cat.indexes.contains_key(&oid)
}

/// What an expression mentions (`find_expr_references`): columns of `table` (as `Col`s),
/// functions, types it is coerced or cast to, and objects named by reg* constants.
fn expr_refs(cat: &Catalog, e: &Expr, table: Option<u32>, out: &mut Vec<Addr>) {
    let mut push = |a: Addr| {
        if !out.contains(&a) {
            out.push(a);
        }
    };
    e.walk(&mut |x| match x {
        Expr::Col(i, _) => {
            if let Some(t) = table {
                push((REL, t, *i as i32 + 1));
            }
        }
        Expr::Const(v, ty) => {
            if let Some(oid) = user_type(cat, ty, None) {
                push((TYPE, oid, 0));
            }
            if let Value::Oid(o) = v {
                match ty {
                    DataType::Regclass if is_relation(cat, *o) => push((REL, *o, 0)),
                    DataType::Regproc | DataType::Regprocedure if cat.functions.contains_key(o) => {
                        push((PROC, *o, 0))
                    }
                    DataType::Regtype if cat.user_type_name(*o).is_some() => push((TYPE, *o, 0)),
                    DataType::Regnamespace if cat.schemas.contains_key(o) => {
                        push((NAMESPACE, *o, 0))
                    }
                    DataType::Regoper | DataType::Regoperator if cat.operators.contains_key(o) => {
                        push((OPERATOR, *o, 0))
                    }
                    DataType::Regconfig if cat.ts_config(*o).is_some() => push((TSCONFIG, *o, 0)),
                    DataType::Regdictionary if cat.ts_dict(*o).is_some() => push((TSDICT, *o, 0)),
                    _ => {}
                }
            }
        }
        Expr::Cast(_, _, to, _) => {
            if let Some(oid) = user_type(cat, to, None) {
                push((TYPE, oid, 0));
            }
        }
        Expr::Call(f, ..)
            if cat.functions.contains_key(&f.oid)
                || cat.ranges.values().any(|r| r.fn_oids.contains(&f.oid)) =>
        {
            // A user operator's call (named as the operator): a use of the operator.
            let op = (!f.name.starts_with(|c: char| c.is_alphabetic() || c == '_')
                || f.name.starts_with("OPERATOR("))
            .then(|| {
                cat.operators.values().find(|o| {
                    o.func == f.oid
                        && (f.name == o.name || f.name.ends_with(&format!(".{})", o.name)))
                })
            })
            .flatten();
            match op {
                Some(o) => push((OPERATOR, o.oid, 0)),
                None => push((PROC, f.oid, 0)),
            }
        }
        Expr::CoerceToDomain(_, d) => push((TYPE, d.oid, 0)),
        Expr::Row(_, t @ DataType::Row(r)) if r.oid != 0 => {
            if let Some(oid) = user_type(cat, t, None) {
                push((TYPE, oid, 0));
            }
        }
        _ => {}
    });
}

/// A binder collecting dependencies, as parse analysis sees the expression.
fn dep_binder<'a, 'b>(ctx: &'b ExecCtx<'a>, cat: &Arc<Catalog>) -> Binder<'a, 'b> {
    let mut b = Binder::new(ctx, Vec::new());
    b.cat = cat.clone();
    b.deparse = true;
    b.deps = Some(Vec::new());
    b
}

/// The columns of FROM functions' named composite results view `v`'s own query reads
/// (`ViewDef::rowtype_cols`), with their names and types now.
pub fn view_rowtype_cols(
    ctx: &ExecCtx,
    cat: &Arc<Catalog>,
    v: &super::ViewDef,
) -> Vec<(u32, i16, String, DataType)> {
    let mut b = dep_binder(ctx, cat);
    if b.bind_view_query(v).is_err() {
        return Vec::new();
    }
    b.rowtype_deps
        .iter()
        .filter_map(|&(rel, attnum)| {
            let c = cat.table(rel)?.columns.get(attnum.checked_sub(1)? as usize)?;
            (!c.dropped).then(|| (rel, attnum, c.name.clone(), c.ty.clone()))
        })
        .collect()
}

/// What `b` noted while binding: relations and columns read or written, functions called,
/// types coerced to.
fn binder_refs(cat: &Catalog, b: &Binder, out: &mut Vec<Addr>) {
    let mut push = |a: Addr| {
        if !out.contains(&a) {
            out.push(a);
        }
    };
    for p in &b.perms.rels {
        let cols: Vec<i16> = p
            .select_cols
            .iter()
            .chain(&p.insert_cols)
            .chain(&p.update_cols)
            .copied()
            .filter(|c| *c > 0)
            .collect();
        push((REL, p.oid, 0));
        for c in cols {
            push((REL, p.oid, c as i32));
        }
    }
    for f in &b.perms.funcs {
        if cat.functions.contains_key(f) {
            push((PROC, *f, 0));
        }
    }
    for (class, oid) in b.deps.iter().flatten() {
        push((*class, *oid, 0));
    }
    // Columns of named composite results of FROM functions.
    for &(rel, attnum) in &b.rowtype_deps {
        push((REL, rel, attnum as i32));
    }
}

/// `eliminate_duplicate_dependencies`: a reference to a column makes one to its whole
/// relation redundant.
fn dedupe(refs: &mut Vec<Addr>) {
    let all = refs.clone();
    refs.retain(|&(c, o, s)| {
        s != 0
            || !all
                .iter()
                .any(|&(c2, o2, s2)| c2 == c && o2 == o && s2 != 0)
    });
}

/// The objects an expression over `t`'s row refers to (unbindable: none).
fn table_expr_refs(
    ctx: &ExecCtx,
    cat: &Arc<Catalog>,
    t: &Arc<Table>,
    sql: &str,
    clause: Clause,
    coerce: Option<(&DataType, Option<u32>)>,
) -> Vec<Addr> {
    let mut out = Vec::new();
    let mut b = dep_binder(ctx, cat);
    let Ok(node) = crate::sql::binder::ast::parse_expr(sql) else {
        return out;
    };
    let Ok(mut e) = b.bind_over_table(t, &node, clause) else {
        return out;
    };
    if let Some((ty, domain)) = coerce {
        match b.coerce_assign(e, ty, "") {
            Ok(x) => e = x,
            Err(_) => return out,
        }
        if let Some(d) = domain {
            out.push((TYPE, d, 0));
        }
    }
    // A whole-row reference to the table (`t.*`) is a reference to the relation itself
    // (find_expr_references: a Var of attno 0), not to its columns or row type.
    let mut whole = false;
    if let Ok(x) = e.clone().transform(&mut |x| {
        Ok(match x {
            Expr::Row(args, DataType::Row(r))
                if r.oid == t.rowtype
                    && t.rowtype != 0
                    && !args.is_empty()
                    && args.iter().all(|a| matches!(a, Expr::Col(..))) =>
            {
                whole = true;
                Expr::Const(Value::Null, DataType::Unknown)
            }
            x => x,
        })
    }) {
        e = x;
    }
    if whole {
        out.push((REL, t.oid, 0));
    }
    expr_refs(cat, &e, Some(t.oid), &mut out);
    binder_refs(cat, &b, &mut out);
    dedupe(&mut out);
    out
}

struct Out {
    rows: Vec<Depend>,
}

impl Out {
    fn add(&mut self, obj: Addr, refobj: Addr, kind: char) {
        if !pinned(refobj) && refobj.1 != 0 && obj != refobj {
            self.rows.push(Depend { obj, refobj, kind });
        }
    }

    fn ty(&mut self, cat: &Catalog, obj: Addr, t: &DataType, domain: Option<u32>, kind: char) {
        if let Some(oid) = user_type(cat, t, domain) {
            self.add(obj, (TYPE, oid, 0), kind);
        }
    }

    /// `recordDependencyOnSingleRelExpr`: references to `table`'s own columns as `own`
    /// (reversed when `reverse`), others as `other`.
    fn rel_expr(
        &mut self,
        obj: Addr,
        table: u32,
        refs: &[Addr],
        other: char,
        own: char,
        reverse: bool,
    ) {
        for &r in refs {
            if r.0 == REL && r.1 == table {
                if reverse {
                    self.add(r, obj, own);
                } else {
                    self.add(obj, r, own);
                }
            } else {
                self.add(obj, r, other);
            }
        }
    }
}

/// Every dependency between objects of the catalog, with the rows superuser DML on
/// pg_depend removed or added.
pub fn depends(ctx: &ExecCtx, cat: &Arc<Catalog>) -> Vec<Depend> {
    depends_scoped(ctx, cat, None)
}

/// `depends`, of the objects of `scope` alone (and of every object of the kinds a `Scope`
/// doesn't name): what a DROP whose dependents `scoped_dependents` finds needs.
pub fn depends_in(ctx: &ExecCtx, cat: &Arc<Catalog>, scope: &Scope) -> Vec<Depend> {
    depends_scoped(ctx, cat, Some(scope))
}

fn depends_scoped(ctx: &ExecCtx, cat: &Arc<Catalog>, scope: Option<&Scope>) -> Vec<Depend> {
    // Stored expressions are read with the session's search_path (a system view's query
    // runs with pg_catalog's alone).
    let session_path = ctx.sess.search_path();
    let mut rows = if ctx.search_path == session_path {
        derived_depends(ctx, cat, scope)
    } else {
        let child = ctx.child_with_path(ctx.params, session_path);
        derived_depends(&child, cat, scope)
    };
    let edits = &cat.depend_edits;
    if !edits.is_empty() {
        rows.retain(|d| !edits.removed.contains(&(d.obj, d.refobj, d.kind)));
        rows.extend(edits.added.iter().map(|&(obj, refobj, kind)| Depend { obj, refobj, kind }));
    }
    rows
}

/// The objects whose dependencies `derived_depends` derives, of the kinds whose derivation
/// grows with the catalog (relations, functions, domains, and what hangs off tables: binding
/// their stored expressions, or one per relation). Objects of other kinds (types other than
/// domains and row types, operators, casts, operator classes, extensions, ...) are always
/// all derived. Each set is in oid order, as the catalog's maps are, so the rows come in the
/// order of the full derivation's.
#[derive(Default, Clone, Debug)]
pub struct Scope {
    /// Tables, with their row types, constraints, defaults, TOAST tables and their foreign
    /// keys' and deferrable keys' internal triggers.
    pub tables: BTreeSet<u32>,
    pub indexes: BTreeSet<u32>,
    pub sequences: BTreeSet<u32>,
    /// Views and materialized views, with their row types, rules and TOAST tables.
    pub views: BTreeSet<u32>,
    pub functions: BTreeSet<u32>,
    pub domains: BTreeSet<u32>,
    /// User triggers.
    pub triggers: BTreeSet<u32>,
    /// CREATE RULE rules.
    pub rules: BTreeSet<u32>,
    pub policies: BTreeSet<u32>,
    pub statistics: BTreeSet<u32>,
    /// Publications' relations (pg_publication_rel).
    pub pubrels: BTreeSet<u32>,
}

/// The scope entry whose derivation makes an object's own rows (pg_depend rows with it as
/// `objid`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Entity {
    Table(u32),
    Index(u32),
    Sequence(u32),
    View(u32),
    Function(u32),
    Domain(u32),
    Trigger(u32),
    Rule(u32),
    Policy(u32),
    Statistics(u32),
    PubRel(u32),
    /// Of a kind always derived whole (or an internal trigger, a view's rule: made by the
    /// derivation of the table or view the walk reached it from).
    Always,
}

impl Scope {
    /// Add `e`; whether it is new.
    fn add(&mut self, e: Entity) -> bool {
        match e {
            Entity::Table(o) => self.tables.insert(o),
            Entity::Index(o) => self.indexes.insert(o),
            Entity::Sequence(o) => self.sequences.insert(o),
            Entity::View(o) => self.views.insert(o),
            Entity::Function(o) => self.functions.insert(o),
            Entity::Domain(o) => self.domains.insert(o),
            Entity::Trigger(o) => self.triggers.insert(o),
            Entity::Rule(o) => self.rules.insert(o),
            Entity::Policy(o) => self.policies.insert(o),
            Entity::Statistics(o) => self.statistics.insert(o),
            Entity::PubRel(o) => self.pubrels.insert(o),
            Entity::Always => false,
        }
    }
}

/// The entity deriving `a`'s rows. None: an object of a kind the scoped walk doesn't handle
/// (schemas, functions' ranges, domain constraints, ...), or one that doesn't exist.
fn entity_of(cat: &Catalog, (class, oid, _): Addr) -> Option<Entity> {
    Some(match class {
        REL => {
            if cat.tables.contains_key(&oid) {
                Entity::Table(oid)
            } else if cat.indexes.contains_key(&oid) {
                Entity::Index(oid)
            } else if cat.sequences.contains_key(&oid) {
                Entity::Sequence(oid)
            } else if cat.views.contains_key(&oid) {
                Entity::View(oid)
            } else if let Some((t, _)) = cat.toast_owner(oid) {
                if cat.tables.contains_key(&t.oid) {
                    Entity::Table(t.oid)
                } else {
                    Entity::View(t.oid)
                }
            } else if cat.composite_of_rel(oid).is_some() {
                Entity::Always
            } else {
                return None;
            }
        }
        TYPE => {
            if let Some((t, _)) = cat.tables.ix().rowtypes.get(&oid) {
                Entity::Table(*t)
            } else if let Some((v, _)) = cat.views.ix().rowtypes.get(&oid) {
                Entity::View(*v)
            } else if let Some(d) = cat.domains.get(&oid) {
                Entity::Domain(d.oid)
            } else if let Some(d) = cat.domains.values().find(|d| d.array_oid == oid) {
                Entity::Domain(d.oid)
            } else if cat.user_type_name(oid).is_some() {
                Entity::Always
            } else {
                return None;
            }
        }
        CONSTRAINT => Entity::Table(cat.tables.ix().con_owner.get(&oid)?.keys().next()?),
        ATTRDEF => Entity::Table(cat.tables.ix().def_owner.get(&oid)?.keys().next()?),
        TRIGGER if cat.triggers.contains_key(&oid) => Entity::Trigger(oid),
        // (An internal trigger: its rows are its foreign key table's.)
        TRIGGER => Entity::Always,
        REWRITE if cat.rules.contains_key(&oid) => Entity::Rule(oid),
        // (A view's rule: the view's.)
        REWRITE => Entity::View(*cat.views.ix().rules.get(&oid)?),
        POLICY if cat.policies.contains_key(&oid) => Entity::Policy(oid),
        STATEXT if cat.statistics.contains_key(&oid) => Entity::Statistics(oid),
        PUBLICATION_REL => Entity::PubRel(oid),
        PROC if cat.functions.contains_key(&oid) => Entity::Function(oid),
        _ => return None,
    })
}

/// Whether type `t` holds composite type `row` (directly, in an array, a range, another
/// composite), as `DataType::contains_row` finds it.
fn type_holds(t: &DataType, row: u32) -> bool {
    match t {
        DataType::Row(r) => r.oid == row || r.fields.iter().any(|(_, f)| type_holds(f, row)),
        DataType::Array(e) => type_holds(e, row),
        DataType::Range(rt) | DataType::Multirange(rt) => type_holds(&rt.subtype, row),
        _ => false,
    }
}

/// A function's types (arguments, results, transition types).
fn function_types(f: &super::Function) -> impl Iterator<Item = &DataType> {
    (f.args.iter().map(|(_, t)| t))
        .chain(f.out_cols.iter().map(|(_, t)| t))
        .chain(std::iter::once(&f.ret))
        .chain(f.agg.iter().flat_map(|a| std::iter::once(&a.transtype).chain(a.mtranstype.as_ref())))
}

/// The stored texts `derived_depends` binds of a function (defaults, a SQL-standard body).
fn function_texts(f: &super::Function) -> Vec<&str> {
    let mut out: Vec<&str> = f.defaults.iter().flatten().map(String::as_str).collect();
    if f.sql_body.is_some() {
        out.push(&f.body);
    }
    out
}

/// What `scoped_dependents` collects for a batch of objects the walk reached: the entities
/// to add, and the names that stored texts referring to the objects contain.
#[derive(Default)]
struct Wanted {
    entities: Vec<Entity>,
    words: Vec<String>,
    /// Row types whose users (functions, domains holding them) may depend on them.
    rowtypes: Vec<u32>,
}

impl Wanted {
    /// A table (or materialized view) and the objects hanging off it: its indexes,
    /// triggers, rules, policies, statistics objects and publication memberships.
    fn with_attached(&mut self, cat: &Catalog, table: u32) {
        self.entities.push(if cat.tables.contains_key(&table) {
            Entity::Table(table)
        } else {
            Entity::View(table)
        });
        if let Some(t) = cat.index_table(table) {
            for i in t.indexes.iter().chain(t.pkey_index.iter()) {
                self.entities.push(Entity::Index(*i));
            }
        }
        // (A constraint trigger also depends on its FROM table.)
        for tg in cat.triggers.values().filter(|tg| tg.table == table || tg.constr_rel == table) {
            self.entities.push(Entity::Trigger(tg.oid));
        }
        for r in cat.rules.values().filter(|r| r.table == table) {
            self.entities.push(Entity::Rule(r.oid));
        }
        for p in cat.policies.values().filter(|p| p.table == table) {
            self.entities.push(Entity::Policy(p.oid));
        }
        for s in cat.statistics.values().filter(|s| s.table == table) {
            self.entities.push(Entity::Statistics(s.oid));
        }
        for p in cat.publications.values() {
            for r in p.rels.iter().filter(|r| r.rel == table) {
                self.entities.push(Entity::PubRel(r.oid));
            }
        }
    }

    /// The objects that may record a dependency on `a` (pg_depend rows with it as
    /// `refobjid`), beyond those of the kinds always derived. None: an object the scoped
    /// walk can't tell this of.
    fn add(&mut self, cat: &Catalog, (class, oid, _): Addr) -> Option<()> {
        match class {
            REL => {
                let name = if let Some(t) = cat.tables.get(&oid) {
                    self.rowtypes.push(t.rowtype);
                    t.name.clone()
                } else if let Some(v) = cat.views.get(&oid) {
                    self.rowtypes.push(v.rowtype);
                    v.name.clone()
                } else if let Some(ix) = cat.indexes.get(&oid) {
                    // Foreign keys use it; indexes are attached to it.
                    let fks = cat.tables.ix().fk_refs.get(&ix.table);
                    self.entities.extend(fks.into_iter().flat_map(|c| c.keys()).map(Entity::Table));
                    let attached = cat.indexes.ix().attached.get(&oid);
                    self.entities.extend(attached.into_iter().flat_map(|c| c.keys()).map(Entity::Index));
                    ix.name.clone()
                } else if let Some(s) = cat.sequences.get(&oid) {
                    s.name.clone()
                } else if cat.toast_owner(oid).is_some() {
                    // (Only its own table refers to a TOAST table.)
                    return Some(());
                } else {
                    return None;
                };
                self.words.push(name);
                if cat.tables.contains_key(&oid) || cat.views.contains_key(&oid) {
                    self.with_attached(cat, oid);
                    self.entities.extend(cat.children(oid).iter().map(|c| Entity::Table(*c)));
                    let fks = cat.tables.ix().fk_refs.get(&oid);
                    self.entities.extend(fks.into_iter().flat_map(|c| c.keys()).map(Entity::Table));
                    let owned = cat.sequences.ix().owned.get(&oid);
                    self.entities.extend(owned.into_iter().flat_map(|v| v.iter()).map(|s| Entity::Sequence(*s)));
                }
                let readers = cat.views.ix().rowtype_rels.get(&oid);
                self.entities.extend(readers.into_iter().flat_map(|c| c.keys()).map(Entity::View));
            }
            TYPE => {
                let (owner, _) = (cat.tables.ix().rowtypes.get(&oid))
                    .or_else(|| cat.views.ix().rowtypes.get(&oid))
                    .copied()?;
                let (rowtype, name) = match (cat.tables.get(&owner), cat.views.get(&owner)) {
                    (Some(t), _) => (t.rowtype, t.name.clone()),
                    (_, Some(v)) => (v.rowtype, v.name.clone()),
                    _ => return None,
                };
                self.entities.push(entity_of(cat, (REL, owner, 0))?);
                self.words.push(name);
                self.rowtypes.push(rowtype);
                // Columns holding it, and the expressions over them.
                for t in cat.tables.ix().row_users.get(&rowtype).into_iter().flat_map(|c| c.keys()) {
                    self.with_attached(cat, t);
                }
                let views = cat.views.ix().row_users.get(&rowtype);
                self.entities.extend(views.into_iter().flat_map(|c| c.keys()).map(Entity::View));
            }
            CONSTRAINT => {
                // Its indexes, internal triggers and the constraints partitions inherit; the
                // queries relying on a key (a view grouping by it: the texts naming its
                // table).
                let t = cat.tables.ix().con_owner.get(&oid)?.keys().next()?;
                self.words.push(cat.tables.get(&t)?.name.clone());
                self.with_attached(cat, t);
                for c in cat.children(t) {
                    self.with_attached(cat, *c);
                }
            }
            TRIGGER => {
                self.entities.push(entity_of(cat, (class, oid, 0))?);
                for tg in cat.triggers.values().filter(|tg| tg.parent == oid) {
                    self.entities.push(Entity::Trigger(tg.oid));
                }
            }
            ATTRDEF | REWRITE | POLICY | STATEXT | PUBLICATION_REL => {
                self.entities.push(entity_of(cat, (class, oid, 0))?);
            }
            _ => return None,
        }
        Some(())
    }

    /// The objects whose stored texts may name `words` (through the word indexes, or by
    /// reading the texts of the kinds without one), and those using `rowtypes`, into the
    /// entities. None: a word the indexes can't look up.
    fn resolve(&mut self, cat: &Catalog) -> Option<()> {
        let rowtypes = std::mem::take(&mut self.rowtypes);
        let holds = |t: &DataType| rowtypes.iter().any(|r| *r != 0 && type_holds(t, *r));
        if rowtypes.iter().any(|r| *r != 0) {
            // Functions taking or returning the row types: named in the texts using them.
            for f in cat.functions.values() {
                if function_types(f).any(holds) {
                    self.entities.push(Entity::Function(f.oid));
                    self.words.push(f.name.clone());
                }
            }
            for d in cat.domains.values() {
                if holds(&d.base) {
                    self.entities.push(Entity::Domain(d.oid));
                }
            }
        }
        let words = std::mem::take(&mut self.words);
        if words.is_empty() {
            return Some(());
        }
        let mut hits = BTreeSet::new();
        if !super::relindex::words_lookup(&cat.tables.ix().words, &words, &mut hits) {
            return None;
        }
        self.entities.extend(hits.iter().map(|t| Entity::Table(*t)));
        hits.clear();
        super::relindex::words_lookup(&cat.indexes.ix().words, &words, &mut hits);
        self.entities.extend(hits.iter().map(|t| Entity::Index(*t)));
        hits.clear();
        super::relindex::words_lookup(&cat.views.ix().words, &words, &mut hits);
        self.entities.extend(hits.iter().map(|t| Entity::View(*t)));
        // (The kinds without a word index: their texts are read.)
        let named = |s: &str| crate::session::ddl::mentions(s, &words);
        for f in cat.functions.values() {
            if function_texts(f).into_iter().any(named) {
                self.entities.push(Entity::Function(f.oid));
            }
        }
        for d in cat.domains.values() {
            if d.default.as_deref().is_some_and(named) || d.checks.iter().any(|(_, c)| named(c)) {
                self.entities.push(Entity::Domain(d.oid));
            }
        }
        for tg in cat.triggers.values() {
            if tg.when.as_deref().is_some_and(named) {
                self.entities.push(Entity::Trigger(tg.oid));
            }
        }
        for r in cat.rules.values() {
            if r.qual.as_deref().is_some_and(named) || r.actions.iter().any(|a| named(a)) {
                self.entities.push(Entity::Rule(r.oid));
            }
        }
        for p in cat.policies.values() {
            if p.qual.as_deref().is_some_and(named) || p.with_check.as_deref().is_some_and(named) {
                self.entities.push(Entity::Policy(p.oid));
            }
        }
        for s in cat.statistics.values() {
            if s.exprs.iter().any(|e| named(e)) {
                self.entities.push(Entity::Statistics(s.oid));
            }
        }
        for p in cat.publications.values() {
            for r in &p.rels {
                if r.qual.as_deref().is_some_and(named) {
                    self.entities.push(Entity::PubRel(r.oid));
                }
            }
        }
        Some(())
    }
}

/// What dropping `roots` deletes (`find_dependents`), found by deriving the dependencies of
/// only the objects that may depend on what the walk reaches, as `findDependentObjects`
/// reads pg_depend through its index on the referenced object: the roots and, for each
/// object the walk reaches, what may refer to it (`Wanted::add`: its sub-objects, the
/// tables referencing it by foreign keys or inheriting from it, the objects whose stored
/// texts name it, through `relindex`'s word indexes, those holding its row type, ...),
/// again until the walk reaches nothing new. None when it reaches an object of a kind it
/// can't follow (a function, a schema, a type other than a row type, ...), when the
/// catalog has pg_depend edits, or when the walk fails (a required object): the whole
/// catalog's dependencies are then derived, as before.
pub fn scoped_dependents(ctx: &ExecCtx, cat: &Arc<Catalog>, roots: &[Addr]) -> Option<Vec<Target>> {
    if roots.is_empty() || !cat.depend_edits.is_empty() {
        return None;
    }
    let mut scope = Scope::default();
    let mut seen: std::collections::HashSet<(u32, u32)> = Default::default();
    let mut todo: Vec<Addr> = roots.to_vec();
    // (Each round adds to the scope, which the catalog bounds.)
    loop {
        let mut w = Wanted::default();
        for a in todo.drain(..) {
            if seen.insert((a.0, a.1)) {
                w.entities.push(entity_of(cat, a)?);
                w.add(cat, a)?;
            }
        }
        w.resolve(cat)?;
        for e in w.entities {
            scope.add(e);
        }
        let deps = depends_in(ctx, cat, &scope);
        let targets = find_dependents_in(cat, &deps, roots, Some(&scope)).ok()?;
        todo = (targets.iter().map(|t| t.obj))
            .filter(|o| !seen.contains(&(o.0, o.1)))
            .collect();
        if todo.is_empty() {
            return Some(targets);
        }
    }
}

/// The values of `map` whose oids are in `set` (all of them without one), in oid order.
fn pick<'a, V>(map: &'a super::pmap::PMap<V>, set: Option<&'a BTreeSet<u32>>) -> Box<dyn Iterator<Item = &'a V> + 'a> {
    match set {
        None => Box::new(map.values()),
        Some(s) => Box::new(s.iter().filter_map(|o| map.get(o))),
    }
}

/// `pick` for a plain map.
fn pick_b<'a, V>(map: &'a BTreeMap<u32, V>, set: Option<&'a BTreeSet<u32>>) -> Box<dyn Iterator<Item = &'a V> + 'a> {
    match set {
        None => Box::new(map.values()),
        Some(s) => Box::new(s.iter().filter_map(|o| map.get(o))),
    }
}

/// The dependencies the catalog's objects record (of the objects of `scope` only, when
/// given: `depends_in`).
fn derived_depends(ctx: &ExecCtx, cat: &Arc<Catalog>, scope: Option<&Scope>) -> Vec<Depend> {
    // (A scoped derivation reads few objects: `user_type` looks each type up instead.)
    let _types = scope.is_none().then(|| UserTypes::set(cat));
    let mut out = Out { rows: Vec::new() };
    // plpgsql is an extension: its language and handlers are its members.
    for f in PLPGSQL_FUNCS {
        out.add((PROC, f, 0), (EXTENSION, PLPGSQL_EXT, 0), 'e');
    }
    out.add(
        (LANGUAGE, PLPGSQL_LANG, 0),
        (EXTENSION, PLPGSQL_EXT, 0),
        'e',
    );
    for (oid, _, schema, _) in &cat.extensions {
        out.add((EXTENSION, *oid, 0), (NAMESPACE, *schema, 0), 'n');
        // What it requires, and its members (recordDependencyOnCurrentExtension).
        if let Some(info) = cat.extension_info.get(oid) {
            for r in &info.requires {
                out.add((EXTENSION, *oid, 0), (EXTENSION, *r, 0), 'n');
            }
            for &(class, member) in &info.members {
                out.add((class, member, 0), (EXTENSION, *oid, 0), 'e');
            }
        }
    }
    for e in cat.enums.values() {
        out.add((TYPE, e.oid, 0), (NAMESPACE, e.schema, 0), 'n');
        out.add((TYPE, e.array_oid, 0), (TYPE, e.oid, 0), 'i');
    }
    for d in pick_b(&cat.domains, scope.map(|s| &s.domains)) {
        let me = (TYPE, d.oid, 0);
        out.add(me, (NAMESPACE, d.schema, 0), 'n');
        // A domain over a made base type uses its output, send and analyze functions.
        if d.parent.is_none()
            && let DataType::Base(b) = &d.base
            && let Some(bt) = cat.base_types.get(&b.oid)
        {
            for f in [bt.output, bt.send, bt.analyze] {
                if f != 0 {
                    out.add(me, (PROC, f, 0), 'n');
                }
            }
        }
        match d.parent.or(d.base_domain) {
            Some(p) => out.add(me, (TYPE, p, 0), 'n'),
            None => out.ty(cat, me, &d.base, None, 'n'),
        }
        if let Some(def) = &d.default {
            let mut b = dep_binder(ctx, cat);
            if let Ok(e) = crate::sql::binder::ast::parse_expr(def).and_then(|n| b.bind_expr(&n)) {
                let mut refs = Vec::new();
                expr_refs(cat, &e, None, &mut refs);
                binder_refs(cat, &b, &mut refs);
                for r in refs {
                    out.add(me, r, 'n');
                }
            }
        }
        out.add((TYPE, d.array_oid, 0), me, 'i');
        if d.not_null {
            out.add((CONSTRAINT, domain_constraint_oid(d, 63), 0), me, 'a');
        }
        for (i, (_, sql)) in d.checks.iter().enumerate() {
            let con = (CONSTRAINT, domain_constraint_oid(d, i), 0);
            out.add(con, me, 'a');
            let mut b = dep_binder(ctx, cat);
            if let Ok(e) = crate::sql::binder::ast::parse_expr(sql)
                .and_then(|n| b.bind_domain_check(&n, &d.base))
            {
                let mut refs = Vec::new();
                expr_refs(cat, &e, None, &mut refs);
                binder_refs(cat, &b, &mut refs);
                for r in refs {
                    out.add(con, r, 'n');
                }
            }
        }
    }
    for d in cat.base_types.values() {
        let me = (TYPE, d.oid(), 0);
        out.add(me, (NAMESPACE, d.ty.schema, 0), 'n');
        if d.is_shell() {
            continue;
        }
        for f in [
            d.input,
            d.output,
            d.receive,
            d.send,
            d.typmod_in,
            d.typmod_out,
            d.analyze,
            d.subscript,
        ] {
            if f != 0 {
                out.add(me, (PROC, f, 0), 'n');
            }
        }
        if d.element != 0 {
            out.add(me, (TYPE, d.element, 0), 'n');
        }
        out.add((TYPE, d.ty.array_oid, 0), me, 'i');
    }
    for c in cat.composites.values() {
        out.add((TYPE, c.oid, 0), (NAMESPACE, c.schema, 0), 'n');
        out.add((REL, c.relid, 0), (TYPE, c.oid, 0), 'i');
        out.add((TYPE, c.array_oid, 0), (TYPE, c.oid, 0), 'i');
        for (i, (_, t)) in c.fields.iter().enumerate() {
            out.ty(cat, (REL, c.relid, i as i32 + 1), t, None, 'n');
        }
    }
    for r in cat.ranges.values() {
        let (me, multi) = ((TYPE, r.oid, 0), (TYPE, r.multi_oid, 0));
        out.add(me, (NAMESPACE, r.schema, 0), 'n');
        out.ty(
            cat,
            me,
            &r.subtype,
            r.subtype_domain,
            'n',
        );
        // (RangeCreate: the subtype's operator class.)
        out.add(me, (OPCLASS, r.subtype_opc, 0), 'n');
        out.add((TYPE, r.array_oid, 0), me, 'i');
        out.add(multi, (NAMESPACE, cat.multirange_schema(r), 0), 'n');
        out.add(multi, me, 'i');
        out.add((TYPE, r.multi_array_oid, 0), multi, 'i');
        if let [c2, c3, m0, m1, m2, cast] = r.fn_oids[..] {
            let rt = DataType::Range(r.range_type());
            for (f, args, ret) in [
                (c2, 2, me),
                (c3, 2, me),
                (m0, 0, multi),
                (m1, 1, multi),
                (m2, 1, multi),
            ] {
                let f = (PROC, f, 0);
                out.add(f, (NAMESPACE, r.schema, 0), 'n');
                if args == 2 {
                    out.ty(cat, f, &r.subtype, None, 'n');
                } else if args == 1 {
                    out.ty(
                        cat,
                        f,
                        &if f.1 == m1 {
                            rt.clone()
                        } else {
                            DataType::array_of(rt.clone())
                        },
                        None,
                        'n',
                    );
                }
                out.add(f, ret, 'n');
                out.add(f, ret, 'i');
            }
            out.add((CAST, cast, 0), (PROC, m1, 0), 'i');
            out.add((CAST, cast, 0), me, 'i');
            out.add((CAST, cast, 0), multi, 'i');
        }
    }
    // (Tables' hidden row id sequences aren't objects of their own.)
    let rowid_seqs: std::collections::HashSet<u32> = match scope {
        None => cat.tables.values().filter_map(|t| t.rowid_seq).collect(),
        Some(sc) => {
            let owners: Vec<u32> = (sc.sequences.iter())
                .filter_map(|s| cat.sequences.get(s)?.owned_by.map(|o| o.0))
                .collect();
            if owners.len() == sc.sequences.len() {
                owners.iter().filter_map(|t| cat.tables.get(t)?.rowid_seq).collect()
            } else {
                cat.tables.values().filter_map(|t| t.rowid_seq).collect()
            }
        }
    };
    for s in pick(&cat.sequences, scope.map(|s| &s.sequences)) {
        if rowid_seqs.contains(&s.oid) {
            continue;
        }
        out.add((REL, s.oid, 0), (NAMESPACE, s.schema, 0), 'n');
        if let Some((t, col)) = s.owned_by {
            // (Only the column's identity sequence: a serial's sequence kept when the column
            // became an identity is still just owned by it.)
            let identity = cat
                .tables
                .get(&t)
                .and_then(|t| t.columns.get(col))
                .is_some_and(|c| c.identity.is_some() && c.seq == Some(s.oid));
            out.add(
                (REL, s.oid, 0),
                (REL, t, col as i32 + 1),
                if identity { 'i' } else { 'a' },
            );
        }
    }
    for t in pick(&cat.tables, scope.map(|s| &s.tables)) {
        table_depends(ctx, cat, t, &mut out);
    }
    // TOAST tables are part of their table; their index goes with their key columns.
    let toasted: Box<dyn Iterator<Item = &Arc<Table>>> = match scope {
        None => Box::new(cat.toasted()),
        Some(sc) => Box::new(
            pick(&cat.tables, Some(&sc.tables))
                .chain(pick(&cat.views, Some(&sc.views)).filter_map(|v| v.storage.as_ref()))
                .filter(|t| t.toast != 0),
        ),
    };
    for t in toasted {
        out.add((REL, t.toast, 0), (REL, t.oid, 0), 'i');
        for col in 1..=2 {
            out.add((REL, t.toast + 1, 0), (REL, t.toast, col), 'a');
        }
    }
    for ix in pick(&cat.indexes, scope.map(|s| &s.indexes)) {
        let Some(t) = cat.index_table(ix.table) else {
            continue;
        };
        let me = (REL, ix.oid, 0);
        if ix.parent != 0 {
            out.add(me, (REL, ix.parent, 0), 'P');
            out.add(me, (REL, ix.table, 0), 'S');
        }
        if let Some(con) = t
            .constraints
            .iter()
            .find(|c| c.kind.index() == Some(ix.oid))
        {
            out.add(me, (CONSTRAINT, con.oid, 0), 'i');
            continue;
        }
        let simple: Vec<usize> = ix
            .columns
            .iter()
            .filter_map(|c| match c.key {
                IndexKey::Column(i) => Some(i),
                IndexKey::Expr(_) => None,
            })
            .chain(ix.include.iter().copied())
            .collect();
        // (record_object_address_dependencies drops duplicates: (c1, c1) is one.)
        let mut seen = Vec::new();
        for i in &simple {
            if seen.contains(i) {
                continue;
            }
            seen.push(*i);
            out.add(me, (REL, ix.table, *i as i32 + 1), 'a');
        }
        if simple.is_empty() {
            out.add(me, (REL, ix.table, 0), 'a');
        }
        let mut refs = Vec::new();
        for c in &ix.columns {
            if let IndexKey::Expr(e) = &c.key {
                refs.extend(
                    table_expr_refs(ctx, cat, &t, e, Clause::IndexExpr, None)
                        .into_iter()
                        .filter(|r| !refs.contains(r))
                        .collect::<Vec<_>>(),
                );
            }
        }
        if let Some(p) = &ix.predicate {
            refs.extend(
                table_expr_refs(ctx, cat, &t, p, Clause::IndexExpr, None)
                    .into_iter()
                    .filter(|r| !refs.contains(r))
                    .collect::<Vec<_>>(),
            );
        }
        let refs: Vec<Addr> = refs
            .into_iter()
            .map(|(c, o, s)| {
                if c == REL && o == t.oid {
                    (c, ix.table, s)
                } else {
                    (c, o, s)
                }
            })
            .collect();
        out.rel_expr(me, ix.table, &refs, 'n', 'a', false);
    }
    for v in pick(&cat.views, scope.map(|s| &s.views)) {
        let me = (REL, v.oid, 0);
        out.add(me, (NAMESPACE, v.schema, 0), 'n');
        if v.rowtype != 0 {
            out.add((TYPE, v.rowtype, 0), me, 'i');
            out.add((TYPE, v.array_type(), 0), (TYPE, v.rowtype, 0), 'i');
        }
        for (i, (_, ty)) in v.columns.iter().enumerate() {
            out.ty(
                cat,
                (REL, v.oid, i as i32 + 1),
                ty,
                v.domains.get(i).copied().flatten(),
                'n',
            );
        }
        let rule = (REWRITE, v.rule_oid(), 0);
        let mut b = dep_binder(ctx, cat);
        if b.bind_view_query(v).is_ok() {
            // Only what the query names itself, not what the views it reads do.
            b.perms.rels.retain(|p| v.deps.contains(&p.oid));
            b.perms.funcs.retain(|f| v.deps.contains(f));
            let mut refs = Vec::new();
            binder_refs(cat, &b, &mut refs);
            dedupe(&mut refs);
            for r in refs {
                out.add(rule, r, 'n');
            }
        }
        out.add(rule, me, 'i');
    }
    for f in pick(&cat.functions, scope.map(|s| &s.functions)) {
        function_depends(ctx, cat, f, &mut out);
    }
    for o in cat.operators.values() {
        let me = (OPERATOR, o.oid, 0);
        out.add(me, (NAMESPACE, o.schema, 0), 'n');
        let mut types: Vec<u32> = Vec::new();
        for t in [o.left.as_ref(), Some(&o.right), o.result.as_ref()]
            .into_iter()
            .flatten()
        {
            if let Some(oid) = user_type(cat, t, None)
                && !types.contains(&oid)
            {
                types.push(oid);
                out.add(me, (TYPE, oid, 0), 'n');
            }
        }
        out.add(me, (PROC, o.func, 0), 'n');
        // User functions as its estimators.
        for (est, join) in [(&o.restrict, false), (&o.join, true)] {
            if let Some(f) = est.as_deref().and_then(|n| cat.user_estimator(n, join)) {
                out.add(me, (PROC, f.oid, 0), 'n');
            }
        }
    }
    // Foreign keys' and deferrable unique keys' internal triggers (their functions are
    // pinned).
    let internal = match scope {
        None => crate::exec::trigger::internal_triggers(cat),
        Some(sc) => {
            // (Made from the constraints of the tables that have them: the scope's, and
            // those referencing them by foreign keys.)
            let keep = |t: u32| sc.tables.contains(&t) || !cat.tables.contains_key(&t);
            let mut from: BTreeSet<u32> = sc.tables.clone();
            for t in &sc.tables {
                from.extend(cat.tables.ix().fk_refs.get(t).into_iter().flat_map(|c| c.keys()));
            }
            crate::exec::trigger::internal_triggers_of(cat, &from)
                .into_iter()
                .filter(|r| keep(r.fk_table) || keep(r.trig.table) || keep(r.other))
                .collect()
        }
    };
    for r in internal {
        out.add((TRIGGER, r.trig.oid, 0), (CONSTRAINT, r.constraint, 0), 'i');
    }
    for tg in pick_b(&cat.triggers, scope.map(|s| &s.triggers)) {
        let me = (TRIGGER, tg.oid, 0);
        out.add(me, (PROC, tg.function, 0), 'n');
        if tg.parent != 0 {
            out.add(me, (TRIGGER, tg.parent, 0), 'P');
            out.add(me, (REL, tg.table, 0), 'S');
        }
        out.add(me, (REL, tg.table, 0), 'a');
        // CREATE CONSTRAINT TRIGGER: its constraint goes with the trigger and the table, and
        // the trigger with the table named in FROM.
        if tg.constraint && tg.con_oid != 0 {
            out.add((CONSTRAINT, tg.con_oid, 0), (REL, tg.table, 0), 'a');
            out.add((CONSTRAINT, tg.con_oid, 0), me, 'i');
        }
        if tg.constr_rel != 0 {
            out.add(me, (REL, tg.constr_rel, 0), 'a');
        }
        for c in &tg.columns {
            out.add(me, (REL, tg.table, *c as i32 + 1), 'n');
        }
        // (A partition's clone of a trigger gets its WHEN as a stored tree, whose OLD and
        // NEW aren't resolved to the table.)
        if let (Some(w), Some(t), 0) = (&tg.when, cat.tables.get(&tg.table), tg.parent) {
            let mut b = dep_binder(ctx, cat);
            if let Ok(e) = b.bind_trigger_when(t, w) {
                let n = t.columns.len();
                // (A whole-row `OLD.*` depends on no column: its columns are counted out.)
                let mut uses: std::collections::BTreeMap<usize, i64> = Default::default();
                e.walk(&mut |x| match x {
                    Expr::Col(i, _) => *uses.entry(i % n).or_default() += 1,
                    Expr::Row(items, _) if crate::sql::binder::is_whole_row(x) => {
                        for it in items {
                            if let Expr::Col(i, _) = it {
                                *uses.entry(i % n).or_default() -= 1;
                            }
                        }
                    }
                    _ => {}
                });
                let refs: Vec<_> = uses.into_iter().filter(|&(_, k)| k > 0).map(|(i, _)| (REL, t.oid, i as i32 + 1)).collect();
                let mut others = Vec::new();
                expr_refs(cat, &e, None, &mut others);
                binder_refs(cat, &b, &mut others);
                for r in refs.into_iter().chain(others) {
                    out.add(me, r, 'n');
                }
            }
        }
    }
    for r in pick_b(&cat.rules, scope.map(|s| &s.rules)) {
        rule_depends(ctx, cat, r, &mut out);
    }
    for e in cat.event_triggers.values() {
        out.add((EVENT_TRIGGER, e.oid, 0), (PROC, e.function, 0), 'n');
    }
    for p in pick_b(&cat.policies, scope.map(|s| &s.policies)) {
        let me = (POLICY, p.oid, 0);
        out.add(me, (REL, p.table, 0), 'a');
        let Some(t) = cat.tables.get(&p.table) else {
            continue;
        };
        // (CreatePolicy records USING's and WITH CHECK's references apart: each list is
        // deduplicated, not the two together.)
        for sql in [&p.qual, &p.with_check].into_iter().flatten() {
            for r in table_expr_refs(ctx, cat, t, sql, Clause::Policy, None) {
                out.add(me, r, 'n');
            }
        }
    }
    for s in pick_b(&cat.statistics, scope.map(|s| &s.statistics)) {
        let me = (STATEXT, s.oid, 0);
        for c in &s.columns {
            out.add(me, (REL, s.table, *c as i32 + 1), 'a');
        }
        if s.columns.is_empty() {
            out.add(me, (REL, s.table, 0), 'a');
        }
        if let Some(t) = cat.tables.get(&s.table) {
            for e in &s.exprs {
                let refs = table_expr_refs(ctx, cat, t, e, Clause::IndexExpr, None);
                out.rel_expr(me, t.oid, &refs, 'n', 'a', false);
            }
        }
        out.add(me, (NAMESPACE, s.schema, 0), 'n');
    }
    ts_depends(cat, &mut out);
    collation_depends(cat, scope, &mut out);
    userobj_depends(cat, scope, &mut out);
    publication_depends(ctx, cat, scope, &mut out);
    out.rows
}

/// A publication's tables and schemas depend on it and on what they name automatically; a
/// column list's columns and what a row filter uses, normally (publication_add_relation,
/// publication_add_schema).
fn publication_depends(ctx: &ExecCtx, cat: &Arc<Catalog>, scope: Option<&Scope>, out: &mut Out) {
    for p in cat.publications.values() {
        for r in p.rels.iter().filter(|r| scope.is_none_or(|s| s.pubrels.contains(&r.oid))) {
            let me = (PUBLICATION_REL, r.oid, 0);
            out.add(me, (PUBLICATION, p.oid, 0), 'a');
            out.add(me, (REL, r.rel, 0), 'a');
            if let (Some(q), Some(t)) = (&r.qual, cat.tables.get(&r.rel)) {
                let refs = table_expr_refs(ctx, cat, t, q, Clause::Where, None);
                out.rel_expr(me, t.oid, &refs, 'n', 'n', false);
            }
            for a in r.cols.iter().flatten() {
                out.add(me, (REL, r.rel, *a as i32), 'n');
            }
        }
        for sc in &p.schemas {
            let me = (PUBLICATION_NAMESPACE, sc.oid, 0);
            out.add(me, (PUBLICATION, p.oid, 0), 'a');
            out.add(me, (NAMESPACE, sc.schema, 0), 'a');
        }
    }
}

/// Operator families and classes, their members, casts, access methods, conversions,
/// languages and transforms (opclasscmds.c, CastCreate, ...), and what uses made operator
/// classes (index keys, partition keys).
/// A rule depends on its relation (automatically), and on what its condition and actions
/// name: relations and their columns, functions, types.
fn rule_depends(ctx: &ExecCtx, cat: &Arc<Catalog>, r: &super::Rule, out: &mut Out) {
    let me = (REWRITE, r.oid, 0);
    out.add(me, (REL, r.table, 0), 'a');
    // InsertRule records the actions' references (their range tables hold OLD and NEW, the
    // rule's relation: a reference to it unless they read its columns) and the WHERE
    // condition's apart, each list deduplicated on its own.
    let refs_of = |rule: &super::Rule| -> Vec<Addr> {
        let mut refs = Vec::new();
        let mut b = dep_binder(ctx, cat);
        // (OLD / NEW read the rule's relation's columns: noted where they resolve.)
        b.refs = Some(Box::default());
        if b.rule_refs(rule).is_ok() {
            binder_refs(cat, &b, &mut refs);
            for c in b.refs.as_deref().map_or(&[][..], |l| &l.cols[..]) {
                if c.src == Some(-1) && c.rel == rule.table && c.attnum > 0 {
                    let a = (REL, c.rel, c.attnum as i32);
                    if !refs.contains(&a) {
                        refs.push(a);
                    }
                }
            }
        }
        refs
    };
    let mut actions = refs_of(&super::Rule { qual: None, ..r.clone() });
    if !actions.iter().any(|x| x.0 == REL && x.1 == r.table) {
        actions.push((REL, r.table, 0));
    }
    dedupe(&mut actions);
    for x in actions {
        out.add(me, x, 'n');
    }
    if r.qual.is_some() {
        let mut qual = refs_of(&super::Rule { actions: Vec::new(), instead: true, ..r.clone() });
        dedupe(&mut qual);
        for x in qual.into_iter().filter(|x| !(x.0 == REL && x.1 == r.table && x.2 == 0)) {
            out.add(me, x, 'n');
        }
    }
}

fn userobj_depends(cat: &Catalog, scope: Option<&Scope>, out: &mut Out) {
    for f in cat.opfamilies.values() {
        out.add((OPFAMILY, f.oid, 0), (NAMESPACE, f.schema, 0), 'n');
        // (On a built-in access method the reference is to a pinned object: not recorded.)
        if cat.access_methods.contains_key(&f.method) {
            out.add((OPFAMILY, f.oid, 0), (AM, f.method, 0), 'a');
        }
    }
    for c in cat.opclasses.values() {
        let me = (OPCLASS, c.oid, 0);
        out.add(me, (NAMESPACE, c.schema, 0), 'n');
        out.add(me, (OPFAMILY, c.family, 0), 'a');
        out.add(me, (TYPE, c.intype, 0), 'n');
        if c.keytype != 0 {
            out.add(me, (TYPE, c.keytype, 0), 'n');
        }
    }
    for m in cat.opmembers.values() {
        let me = (if m.func { AMPROC } else { AMOP }, m.oid, 0);
        let soft_hard = |hard: bool, h: char, s: char| if hard { h } else { s };
        out.add(
            me,
            (if m.func { PROC } else { OPERATOR }, m.object, 0),
            soft_hard(m.hard, 'n', 'a'),
        );
        let owner = if m.refclass != 0 {
            (OPCLASS, m.refclass, 0)
        } else {
            (OPFAMILY, m.family, 0)
        };
        out.add(me, owner, soft_hard(m.hard, 'i', 'a'));
        if !m.inputs.contains(&m.left) {
            out.add(me, (TYPE, m.left, 0), 'a');
        }
        if m.right != m.left && !m.inputs.contains(&m.right) {
            out.add(me, (TYPE, m.right, 0), 'a');
        }
        if m.sortfamily != 0 {
            out.add(me, (OPFAMILY, m.sortfamily, 0), soft_hard(m.hard, 'n', 'a'));
        }
    }
    for c in cat.casts.values() {
        let me = (CAST, c.oid, 0);
        out.add(me, (TYPE, c.source, 0), 'n');
        out.add(me, (TYPE, c.target, 0), 'n');
        if c.func != 0 {
            out.add(me, (PROC, c.func, 0), 'n');
        }
        for x in [c.incast, c.outcast] {
            if x != 0 {
                out.add(me, (CAST, x, 0), 'n');
            }
        }
    }
    for a in cat.access_methods.values() {
        out.add((AM, a.oid, 0), (PROC, a.handler, 0), 'n');
    }
    let rel_in = |rel: &u32| {
        scope.is_none_or(|s| s.tables.contains(rel) || s.views.contains(rel) || s.sequences.contains(rel))
    };
    for (rel, am) in &cat.rel_am {
        if !cat.indexes.contains_key(rel) && rel_in(rel) {
            out.add((REL, *rel, 0), (AM, *am, 0), 'n');
        }
    }
    for c in cat.conversions.values() {
        let me = (CONVERSION, c.oid, 0);
        out.add(me, (NAMESPACE, c.schema, 0), 'n');
        out.add(me, (PROC, c.proc, 0), 'n');
    }
    for l in cat.languages.values() {
        let me = (LANGUAGE, l.oid, 0);
        for f in [l.handler, l.inline, l.validator] {
            if f != 0 {
                out.add(me, (PROC, f, 0), 'n');
            }
        }
    }
    for f in cat.fdws.values() {
        for p in [f.handler, f.validator] {
            if p != 0 {
                out.add((FDW, f.oid, 0), (PROC, p, 0), 'n');
            }
        }
    }
    for x in cat.servers.values() {
        out.add((SERVER, x.oid, 0), (FDW, x.fdw, 0), 'n');
    }
    for m in cat.user_mappings.values() {
        out.add((USER_MAPPING, m.oid, 0), (SERVER, m.server, 0), 'n');
    }
    for t in cat.transforms.values() {
        let me = (TRANSFORM, t.oid, 0);
        out.add(me, (TYPE, t.ty, 0), 'n');
        out.add(me, (LANGUAGE, t.lang, 0), 'n');
        for f in [t.fromsql, t.tosql] {
            if f != 0 {
                out.add(me, (PROC, f, 0), 'n');
            }
        }
    }
    if cat.opclasses.is_empty() {
        return;
    }
    for ix in pick(&cat.indexes, scope.map(|s| &s.indexes)) {
        for c in &ix.columns {
            if cat.opclasses.contains_key(&c.opclass) {
                out.add((REL, ix.oid, 0), (OPCLASS, c.opclass, 0), 'n');
            }
        }
    }
    for t in pick(&cat.tables, scope.map(|s| &s.tables)) {
        let Some(key) = &t.partkey else { continue };
        let am = if key.strategy == super::PartStrategy::Hash { 405 } else { 403 };
        for k in &key.cols {
            if let Some(oid) = k
                .opclass
                .as_ref()
                .and_then(|n| cat.opclasses.values().find(|c| c.method == am && &c.name == n))
                .map(|c| c.oid)
            {
                out.add((REL, t.oid, 0), (OPCLASS, oid, 0), 'n');
            }
        }
    }
}

/// Collations' own dependencies (on their schema), and those of columns, domains and index
/// keys declared with a CREATE COLLATION collation.
fn collation_depends(cat: &Catalog, scope: Option<&Scope>, out: &mut Out) {
    if cat.collations.is_empty() {
        return;
    }
    for d in cat.collations.values() {
        out.add((COLLATION, d.oid, 0), (NAMESPACE, d.namespace, 0), 'n');
    }
    let user = |name: &Option<String>| -> Option<u32> {
        let n = name.as_deref()?;
        cat.collations.values().find(|d| d.name == n).map(|d| d.oid)
    };
    for t in pick(&cat.tables, scope.map(|s| &s.tables)) {
        for (i, c) in t.live_columns() {
            if let Some(oid) = user(&c.collation) {
                out.add((REL, t.oid, i as i32 + 1), (COLLATION, oid, 0), 'n');
            }
        }
        // StorePartitionKey: each key's collation (written, or its column's).
        for k in t.partkey.iter().flat_map(|k| &k.cols) {
            let coll = k.collation.clone().or_else(|| t.columns.get(k.col?)?.collation.clone());
            if let Some(oid) = user(&coll) {
                out.add((REL, t.oid, 0), (COLLATION, oid, 0), 'n');
            }
        }
    }
    for d in pick_b(&cat.domains, scope.map(|s| &s.domains)) {
        if let Some(oid) = user(&d.collation) {
            out.add((TYPE, d.oid, 0), (COLLATION, oid, 0), 'n');
            // (Its array type has the collation too.)
            out.add((TYPE, d.array_oid, 0), (COLLATION, oid, 0), 'n');
        }
    }
    for c in cat.composites.values() {
        for (i, (f, _)) in c.fields.iter().enumerate() {
            if let Some(oid) = user(&c.collations.get(f).cloned()) {
                out.add((REL, c.relid, i as i32 + 1), (COLLATION, oid, 0), 'n');
            }
        }
    }
    for ix in pick(&cat.indexes, scope.map(|s| &s.indexes)) {
        for c in &ix.columns {
            if let Some(oid) = user(&c.collation) {
                out.add((REL, ix.oid, 0), (COLLATION, oid, 0), 'n');
            }
        }
    }
    // (The views' rules depend on the collations their expressions name or relabel to,
    // noted while binding them; not on their columns' collations.)
    for v in pick(&cat.views, scope.map(|s| &s.views)) {
        for (i, c) in v.collations.iter().enumerate() {
            if let Some(oid) = user(c) {
                out.add((REL, v.oid, i as i32 + 1), (COLLATION, oid, 0), 'n');
            }
        }
    }
}

/// The text search objects' dependencies (makeParserDependencies & co).
fn ts_depends(cat: &Catalog, out: &mut Out) {
    use super::tsearch::TsKind;
    for (oid, ns, _) in cat.ts_objects(TsKind::Parser) {
        let Some(p) = cat.ts_parser(oid) else {
            continue;
        };
        let me = (TSPARSER, oid, 0);
        out.add(me, (NAMESPACE, ns, 0), 'n');
        for f in [p.start, p.token, p.end, p.lextype, p.headline] {
            out.add(me, (PROC, f, 0), 'n');
        }
    }
    for (oid, ns, _) in cat.ts_objects(TsKind::Template) {
        let Some(t) = cat.ts_template(oid) else {
            continue;
        };
        let me = (TSTEMPLATE, oid, 0);
        out.add(me, (NAMESPACE, ns, 0), 'n');
        out.add(me, (PROC, t.lexize, 0), 'n');
        out.add(me, (PROC, t.init, 0), 'n');
    }
    for (oid, ns, _) in cat.ts_objects(TsKind::Dict) {
        let Some(d) = cat.ts_dict(oid) else { continue };
        let me = (TSDICT, oid, 0);
        out.add(me, (NAMESPACE, ns, 0), 'n');
        out.add(me, (TSTEMPLATE, d.template, 0), 'n');
    }
    for (oid, ns, _) in cat.ts_objects(TsKind::Config) {
        let Some(c) = cat.ts_config(oid) else {
            continue;
        };
        let me = (TSCONFIG, oid, 0);
        out.add(me, (NAMESPACE, ns, 0), 'n');
        out.add(me, (TSPARSER, c.parser, 0), 'n');
        let mut dicts: Vec<u32> = c.map.iter().flatten().copied().collect();
        dicts.sort_unstable();
        dicts.dedup();
        for d in dicts {
            out.add(me, (TSDICT, d, 0), 'n');
        }
    }
}

fn table_depends(ctx: &ExecCtx, cat: &Arc<Catalog>, t: &Arc<Table>, out: &mut Out) {
    let me = (REL, t.oid, 0);
    out.add(me, (NAMESPACE, t.schema, 0), 'n');
    if t.partbound.is_some() {
        for p in &t.inherits {
            out.add(me, (REL, *p, 0), 'a');
        }
    } else {
        for p in &t.inherits {
            out.add(me, (REL, *p, 0), 'n');
        }
    }
    if t.of_type != 0 {
        out.add(me, (TYPE, t.of_type, 0), 'n');
    }
    if let Some(f) = &t.foreign {
        out.add(me, (SERVER, f.server, 0), 'n');
    }
    if t.rowtype != 0 {
        out.add((TYPE, t.rowtype, 0), me, 'i');
        out.add((TYPE, t.array_type(), 0), (TYPE, t.rowtype, 0), 'i');
    }
    for (i, c) in t.live_columns() {
        out.ty(cat, (REL, t.oid, i as i32 + 1), &c.ty, c.domain, 'n');
    }
    if let Some(key) = &t.partkey {
        for k in &key.cols {
            match (&k.col, &k.expr) {
                (Some(i), _) => out.add((REL, t.oid, *i as i32 + 1), me, 'i'),
                (None, Some(e)) => {
                    let refs = table_expr_refs(ctx, cat, t, e, Clause::IndexExpr, None);
                    out.rel_expr(me, t.oid, &refs, 'n', 'i', true);
                }
                _ => {}
            }
        }
    }
    for (i, c) in t.live_columns() {
        let Some(sql) = c.default.as_ref().or(c.generated.as_ref()) else {
            continue;
        };
        let def = (ATTRDEF, c.def_oid, 0);
        out.add(
            def,
            (REL, t.oid, i as i32 + 1),
            if c.generated.is_some() { 'i' } else { 'a' },
        );
        let clause = if c.generated.is_some() {
            Clause::Generated
        } else {
            Clause::Default
        };
        for r in table_expr_refs(ctx, cat, t, sql, clause, Some((&c.ty, c.domain))) {
            out.add(def, r, 'n');
        }
    }
    for con in &t.constraints {
        let me_con = (CONSTRAINT, con.oid, 0);
        let parent_on = |p: u32| t.constraints.iter().any(|c| c.oid == p);
        match &con.kind {
            ConstraintKind::NotNull { column } => {
                out.add(me_con, (REL, t.oid, *column as i32 + 1), 'a')
            }
            ConstraintKind::Check { expr, .. } => {
                let refs = table_expr_refs(ctx, cat, t, expr, Clause::Check, None);
                let own: Vec<Addr> = refs
                    .iter()
                    .copied()
                    .filter(|r| r.0 == REL && r.1 == t.oid && r.2 != 0)
                    .collect();
                for &r in &own {
                    out.add(me_con, r, 'a');
                }
                if own.is_empty() {
                    out.add(me_con, me, 'a');
                }
                // (A whole-row reference to the table itself records nothing more.)
                for r in refs.into_iter().filter(|r| *r != me) {
                    out.add(me_con, r, 'n');
                }
            }
            ConstraintKind::PrimaryKey { index }
            | ConstraintKind::Unique { index }
            | ConstraintKind::Exclusion { index, .. } => {
                if let Some(ix) = cat.indexes.get(index) {
                    for c in &ix.columns {
                        if let IndexKey::Column(i) = c.key {
                            out.add(me_con, (REL, t.oid, i as i32 + 1), 'a');
                        }
                    }
                }
                if con.parent != 0 {
                    out.add(me_con, (CONSTRAINT, con.parent, 0), 'P');
                    out.add(me_con, me, 'S');
                }
            }
            ConstraintKind::ForeignKey {
                columns,
                ref_table,
                ref_columns,
                ..
            } => {
                for c in columns {
                    out.add(me_con, (REL, t.oid, *c as i32 + 1), 'a');
                }
                for c in ref_columns {
                    out.add(me_con, (REL, *ref_table, *c as i32 + 1), 'n');
                }
                if let Some(ix) = fk_index(cat, *ref_table, ref_columns) {
                    out.add(me_con, (REL, ix, 0), 'n');
                }
                if con.parent != 0 {
                    if parent_on(con.parent) {
                        out.add(me_con, (CONSTRAINT, con.parent, 0), 'i');
                    } else {
                        out.add(me_con, (CONSTRAINT, con.parent, 0), 'P');
                        out.add(me_con, me, 'S');
                    }
                }
            }
        }
    }
}

/// The unique index of `table` a foreign key over `columns` uses (pg_constraint.conindid).
pub fn fk_index(cat: &Catalog, table: u32, columns: &[usize]) -> Option<u32> {
    let p = cat.tables.get(&table)?;
    let mut want = columns.to_vec();
    want.sort_unstable();
    // The key columns in any order (transformFkeyCheckAttrs); not a deferrable constraint's.
    p.pkey_index
        .iter()
        .chain(&p.indexes)
        .filter_map(|i| cat.indexes.get(i))
        .find(|ix| {
            let mut cols: Vec<usize> = (ix.columns.iter())
                .filter_map(|c| {
                    if let IndexKey::Column(i) = c.key {
                        Some(i)
                    } else {
                        None
                    }
                })
                .collect();
            cols.sort_unstable();
            // (A foreign key using PERIOD references an exclusion constraint's index.)
            let exclusion = (p.constraints.iter()).any(|c| {
                c.kind.index() == Some(ix.oid) && matches!(c.kind, ConstraintKind::Exclusion { .. })
            });
            (ix.unique || ix.primary || exclusion)
                && ix.predicate.is_none()
                && cols.len() == ix.columns.len()
                && cols == want
                && !(p.constraints.iter()).any(|c| c.kind.index() == Some(ix.oid) && c.deferrable)
        })
        .map(|ix| ix.oid)
}

fn function_depends(ctx: &ExecCtx, cat: &Arc<Catalog>, f: &Arc<super::Function>, out: &mut Out) {
    let me = (PROC, f.oid, 0);
    out.add(me, (NAMESPACE, f.schema, 0), 'n');
    // ProcedureCreate: each type once, then the language.
    let mut types: Vec<u32> = Vec::new();
    let mut ty = |t: &DataType, d: Option<u32>| {
        if let Some(oid) = user_type(cat, t, d)
            && !types.contains(&oid)
        {
            types.push(oid);
        }
    };
    for (i, (_, t)) in f.args.iter().enumerate() {
        ty(t, f.arg_domain(i));
    }
    for (i, (_, t)) in f.out_cols.iter().enumerate() {
        ty(t, f.out_domains.get(i).copied().flatten());
    }
    ty(&f.ret, f.ret_domain);
    if let Some(a) = &f.agg {
        ty(&a.transtype, None);
        if let Some(m) = &a.mtranstype {
            ty(m, None);
        }
    }
    for t in types {
        out.add(me, (TYPE, t, 0), 'n');
    }
    if f.language == "plpgsql" {
        out.add(me, (LANGUAGE, PLPGSQL_LANG, 0), 'n');
    }
    if let Some(a) = &f.agg {
        for p in [
            a.transfn,
            a.finalfn,
            a.combinefn,
            a.serialfn,
            a.deserialfn,
            a.mtransfn,
            a.minvtransfn,
            a.mfinalfn,
        ] {
            out.add(me, (PROC, p, 0), 'n');
        }
        if cat.operators.contains_key(&a.sortop) {
            out.add(me, (OPERATOR, a.sortop, 0), 'n');
        }
    }
    let mut refs = Vec::new();
    for d in f.defaults.iter().flatten() {
        let mut b = dep_binder(ctx, cat);
        if let Ok(e) = crate::sql::binder::ast::parse_expr(d).and_then(|n| b.bind_expr(&n)) {
            expr_refs(cat, &e, None, &mut refs);
            binder_refs(cat, &b, &mut refs);
        }
    }
    refs.extend(sql_body_refs(ctx, cat, f));
    dedupe(&mut refs);
    for r in refs {
        out.add(me, r, 'n');
    }
}

/// What a SQL-standard function body's statements use.
pub fn sql_body_refs(ctx: &ExecCtx, cat: &Arc<Catalog>, f: &super::Function) -> Vec<Addr> {
    let mut refs = Vec::new();
    if f.sql_body.is_none() {
        return refs;
    }
    let args: Vec<Expr> = f
        .args
        .iter()
        .map(|(_, t)| Expr::Const(Value::Null, t.clone()))
        .collect();
    if let Ok(stmts) = crate::sql::parse::parse(&f.body) {
        for raw in &stmts {
            let text = crate::sql::parse::stmt_text(&f.body, raw)
                .trim_end_matches(';')
                .to_string();
            let one = super::Function {
                body: text,
                config: Vec::new(),
                ..f.clone()
            };
            let mut b = dep_binder(ctx, cat);
            if b.bind_user_body(&one, &args).is_ok() {
                binder_refs(cat, &b, &mut refs);
            }
        }
    }
    refs
}

// ── descriptions ──

/// A relation's description (`getRelationDescription`), its name qualified unless visible.
fn relation_description(cat: &Catalog, oid: u32, sp: &[String]) -> Option<String> {
    let q = |schema: u32, name: &str| cat.qualified_name(schema, name, sp);
    if let Some(t) = cat.tables.get(&oid) {
        let kind = if t.foreign.is_some() { "foreign table" } else { "table" };
        return Some(format!("{kind} {}", q(t.schema, &t.name)));
    }
    if let Some(v) = cat.views.get(&oid) {
        return Some(format!(
            "{} {}",
            if v.materialized {
                "materialized view"
            } else {
                "view"
            },
            q(v.schema, &v.name)
        ));
    }
    if let Some(s) = cat.sequences.get(&oid) {
        return Some(format!("sequence {}", q(s.schema, &s.name)));
    }
    if let Some(ix) = cat.indexes.get(&oid) {
        let schema = cat.index_table(ix.table).map(|t| t.schema).unwrap_or(0);
        return Some(format!("index {}", q(schema, &ix.name)));
    }
    if let Some(c) = cat.composite_of_rel(oid) {
        return Some(format!("composite type {}", q(c.schema, &c.name)));
    }
    if let Some((t, index)) = cat.toast_owner(oid) {
        return Some(format!(
            "{} {}",
            if index { "index" } else { "toast table" },
            q(cat.toast_schema(t), &t.toast_relname(index))
        ));
    }
    let t = SysTable::by_oid(oid)?;
    let kind = if t.is_view() { "view" } else { "table" };
    Some(if t.schema() == super::PG_CATALOG_OID {
        format!("{kind} {}", t.name())
    } else {
        format!("{kind} information_schema.{}", t.name())
    })
}

fn column_name(cat: &Catalog, rel: u32, attnum: i32) -> Option<String> {
    let i = usize::try_from(attnum - 1).ok()?;
    if let Some(t) = cat.tables.get(&rel) {
        return t.columns.get(i).map(|c| c.name.clone());
    }
    if let Some(v) = cat.views.get(&rel) {
        return v.columns.get(i).map(|c| c.0.clone());
    }
    if let Some(ix) = cat.indexes.get(&rel) {
        let t = cat.index_table(ix.table)?;
        return super::system::index_att_names(&t, ix).get(i).cloned();
    }
    if let Some(c) = cat.composite_of_rel(rel) {
        return c.fields.get(i).map(|f| f.0.clone());
    }
    if let Some((_, index)) = cat.toast_owner(rel) {
        return super::TOAST_COLUMNS[..if index { 2 } else { 3 }]
            .get(i)
            .map(|c| c.to_string());
    }
    None
}

/// A function's signature as `format_procedure` writes it.
pub fn procedure_description(cat: &Catalog, oid: u32, sp: &[String]) -> Option<String> {
    if let Some(f) = cat.functions.get(&oid) {
        return Some(format!(
            "{}({})",
            cat.function_name(f, sp),
            cat.param_type_names(f, sp).join(",")
        ));
    }
    for r in cat.ranges.values() {
        if let Some(i) = r.fn_oids.iter().take(5).position(|o| *o == oid) {
            let rt = DataType::Range(r.range_type());
            let st = cat.format_type(&r.subtype, None, sp);
            let args = match i {
                0 => format!("{st},{st}"),
                1 => format!("{st},{st},text"),
                2 => String::new(),
                3 => cat.format_type(&rt, None, sp),
                _ => cat.format_type(&DataType::array_of(rt), None, sp),
            };
            let name = if i < 2 {
                (&r.ctors.0, &r.name)
            } else {
                (&r.ctors.1, &r.multi_name)
            };
            let name = if name.0.is_empty() { name.1 } else { name.0 };
            let visible = cat.effective_path(sp).contains(&r.schema);
            let name = if visible {
                quote_ident(name)
            } else {
                format!(
                    "{}.{}",
                    quote_ident(cat.schema_name(r.schema)),
                    quote_ident(name)
                )
            };
            return Some(format!("{name}({args})"));
        }
    }
    let builtin = [
        (2246, "fmgr_internal_validator(oid)"),
        (2247, "fmgr_c_validator(oid)"),
        (2248, "fmgr_sql_validator(oid)"),
        (13678, "plpgsql_call_handler()"),
        (13679, "plpgsql_inline_handler(internal)"),
        (13680, "plpgsql_validator(oid)"),
    ];
    if let Some((_, s)) = builtin.iter().find(|b| b.0 == oid) {
        return Some(s.to_string());
    }
    // pg_proc's own signature (format_procedure: the name quoted as an identifier).
    if let Some(p) = super::system::builtin_proc_sigs().iter().find(|p| p.oid == oid) {
        let args: Vec<String> = p
            .args
            .iter()
            .map(|a| {
                crate::sql::func::system::format_type_oid(*a, -1, cat, sp)
                    .unwrap_or_else(|| "???".into())
            })
            .collect();
        return Some(format!("{}({})", quote_ident(p.name), args.join(",")));
    }
    let reg = crate::sql::func::registry();
    if let Some(f) = reg.funcs.values().flatten().find(|f| f.oid == oid) {
        // VARIADIC "any" is listed as "any", as pg_proc lists it.
        let shown = |t: &DataType| match t {
            DataType::Array(e) if matches!(**e, DataType::Any) => DataType::Any.display(),
            t => t.display(),
        };
        return Some(format!(
            "{}({})",
            f.name,
            f.args.iter().map(shown).collect::<Vec<_>>().join(",")
        ));
    }
    // Internal-only built-ins, by Postgres's own signatures.
    let (_, name, args) = crate::sql::func::pg_oids::PROCS
        .iter()
        .find(|p| p.0 == oid)?;
    let args: Vec<String> = args
        .iter()
        .map(|a| {
            crate::sql::func::system::format_type_oid(*a, -1, cat, sp)
                .unwrap_or_else(|| "???".into())
        })
        .collect();
    Some(format!("{name}({})", args.join(",")))
}

/// `getObjectDescription`: `None` for an object that doesn't exist.
pub fn describe(cat: &Catalog, sp: &[String], (class, oid, sub): Addr) -> Option<String> {
    match class {
        super::shdepend::AUTH_MEMBERS => {
            let m = cat.members.iter().find(|m| m.oid == oid)?;
            let name = |r: u32| cat.roles.get(&r).map(|x| x.name.clone());
            Some(format!(
                "privileges for membership of role {} in role {}",
                name(m.member)?,
                name(m.role)?
            ))
        }
        REL => {
            let rel = relation_description(cat, oid, sp)?;
            if sub > 0 {
                Some(format!("column {} of {rel}", column_name(cat, oid, sub)?))
            } else {
                Some(rel)
            }
        }
        TYPE => {
            if cat.user_type_name(oid).is_some() {
                return Some(format!(
                    "type {}",
                    cat.format_type(&DataType::Unknown, Some(oid), sp)
                ));
            }
            DataType::from_oid(oid).map(|t| format!("type {}", t.display()))
        }
        PROC => Some(format!("function {}", procedure_description(cat, oid, sp)?)),
        NAMESPACE => Some(format!(
            "schema {}",
            quote_ident(cat.schemas.get(&oid).map(|s| s.name.as_str())?)
        )),
        CONSTRAINT => {
            // (The first table with it, by the tables map's index of constraint owners.)
            let owners = cat.tables.ix().con_owner.get(&oid);
            for t in owners.into_iter().flat_map(|c| c.keys()).filter_map(|t| cat.tables.get(&t)) {
                if let Some(c) = t.constraints.iter().find(|c| c.oid == oid) {
                    return Some(format!(
                        "constraint {} on {}",
                        quote_ident(&c.name),
                        relation_description(cat, t.oid, sp)?
                    ));
                }
            }
            for d in cat.domains.values() {
                if d.not_null && domain_constraint_oid(d, 63) == oid {
                    return Some(format!(
                        "constraint {}",
                        quote_ident(
                            &d.not_null_name
                                .clone()
                                .unwrap_or_else(|| format!("{}_not_null", d.name))
                        )
                    ));
                }
                if let Some((n, _)) = d
                    .checks
                    .iter()
                    .enumerate()
                    .find(|(i, _)| domain_constraint_oid(d, *i) == oid)
                    .map(|(_, c)| c)
                {
                    return Some(format!("constraint {}", quote_ident(n)));
                }
            }
            // A constraint trigger's constraint (contype 't').
            if let Some(tg) = cat.triggers.values().find(|tg| tg.con_oid == oid && tg.con_oid != 0) {
                return Some(format!(
                    "constraint {} on {}",
                    quote_ident(&tg.name),
                    relation_description(cat, tg.table, sp)?
                ));
            }
            None
        }
        ATTRDEF => {
            let owners = cat.tables.ix().def_owner.get(&oid);
            for t in owners.into_iter().flat_map(|c| c.keys()).filter_map(|t| cat.tables.get(&t)) {
                for (_, c) in t.live_columns() {
                    if (c.default.is_some() || c.generated.is_some()) && c.def_oid == oid {
                        return Some(format!(
                            "default value for column {} of {}",
                            c.name,
                            relation_description(cat, t.oid, sp)?
                        ));
                    }
                }
            }
            None
        }
        EVENT_TRIGGER => {
            let e = cat.event_triggers.get(&oid)?;
            Some(format!("event trigger {}", quote_ident(&e.name)))
        }
        REWRITE => {
            if let Some(r) = cat.rules.get(&oid) {
                return Some(format!(
                    "rule {} on {}",
                    r.name,
                    relation_description(cat, r.table, sp)?
                ));
            }
            let v = cat.views.get(cat.views.ix().rules.get(&oid)?)?;
            debug_assert!(
                cat.views.values().find(|v| v.rule_oid() == oid).is_some_and(|x| x.oid == v.oid),
                "views' rules index"
            );
            Some(format!(
                "rule _RETURN on {}",
                relation_description(cat, v.oid, sp)?
            ))
        }
        TRIGGER => {
            let (name, table) = match cat.triggers.get(&oid) {
                Some(tg) => (tg.name.clone(), tg.table),
                None => {
                    let r = crate::exec::trigger::ri_trigger(cat, oid)?;
                    (r.trig.name, r.trig.table)
                }
            };
            Some(format!(
                "trigger {name} on {}",
                relation_description(cat, table, sp)?
            ))
        }
        POLICY => {
            let p = cat.policies.get(&oid)?;
            Some(format!(
                "policy {} on {}",
                quote_ident(&p.name),
                relation_description(cat, p.table, sp)?
            ))
        }
        LANGUAGE => cat
            .language(oid)
            .map(|l| format!("language {}", l.name)),
        EXTENSION => {
            if oid == PLPGSQL_EXT {
                return Some("extension plpgsql".into());
            }
            cat.extensions
                .iter()
                .find(|e| e.0 == oid)
                .map(|e| format!("extension {}", e.1))
        }
        STATEXT => {
            let s = cat.statistics.get(&oid)?;
            // StatisticsObjIsVisible: the first statistics object of the name in the path
            // (the temporary schema isn't searched).
            let first = (cat.routine_path(sp).into_iter())
                .find(|n| cat.statistics.values().any(|x| x.schema == *n && x.name == s.name));
            Some(if first == Some(s.schema) {
                format!("statistics object {}", quote_ident(&s.name))
            } else {
                let schema = quote_ident(cat.schema_name(s.schema));
                format!("statistics object {schema}.{}", quote_ident(&s.name))
            })
        }
        COLLATION => {
            let d = crate::types::collation::def_by_oid(cat, oid)?;
            Some(format!("collation {}", collation_name_in_path(cat, sp, &d)))
        }
        OPERATOR => cat
            .operators
            .get(&oid)
            .map(|o| format!("operator {}", cat.operator_signature(o, sp))),
        TSCONFIG | TSDICT | TSPARSER | TSTEMPLATE => {
            let kind = ts_kind(class)?;
            Some(format!(
                "{} {}",
                kind.noun(),
                cat.ts_qualified(kind, oid, sp)?
            ))
        }
        super::identify::AUTHID => cat
            .roles
            .get(&oid)
            .map(|r| format!("role {}", quote_ident(&r.name))),
        // Whether it exists is the caller's to check: large objects aren't in the catalog.
        crate::largeobj::CLASS_OID => Some(format!("large object {oid}")),
        super::identify::PARAMETER_ACL => cat
            .parameters
            .iter()
            .find(|p| p.0 == oid)
            .map(|p| format!("parameter {}", p.1)),
        super::shdepend::DEFACL => {
            let d = cat.default_acls.iter().find(|d| d.oid == oid)?;
            let what = match d.objtype {
                'r' => "relations",
                'S' => "sequences",
                'f' => "functions",
                'T' => "types",
                'n' => "schemas",
                _ => "large objects",
            };
            let role = cat
                .roles
                .get(&d.role)
                .map_or_else(|| d.role.to_string(), |r| r.name.clone());
            Some(match d.schema {
                0 => format!("default privileges on new {what} belonging to role {role}"),
                s => format!(
                    "default privileges on new {what} belonging to role {role} in schema {}",
                    cat.schema_name(s)
                ),
            })
        }
        OPCLASS => {
            let c = cat.opclass_info(oid)?;
            Some(format!(
                "operator class {} for access method {}",
                opc_name(cat, sp, c.schema, &c.name, cat.opclass_visible(&c, sp)),
                cat.access_method(c.method)?.name
            ))
        }
        OPFAMILY => opfamily_description(cat, sp, oid),
        AMOP | AMPROC => {
            let m = cat.opmember_info(class == AMPROC, oid)?;
            let ty = |t: u32| {
                crate::sql::func::system::format_type_oid(t, -1, cat, sp)
                    .unwrap_or_else(|| "???".into())
            };
            let what = if m.func {
                procedure_description(cat, m.object, sp)?
            } else {
                operator_description(cat, sp, m.object)?
            };
            Some(format!(
                "{} {} ({}, {}) of {}: {what}",
                if m.func { "function" } else { "operator" },
                m.number,
                ty(m.left),
                ty(m.right),
                opfamily_description(cat, sp, m.family)?
            ))
        }
        FDW => cat
            .fdws
            .get(&oid)
            .map(|f| format!("foreign-data wrapper {}", f.name)),
        SERVER => cat.servers.get(&oid).map(|x| format!("server {}", x.name)),
        PUBLICATION => cat
            .publications
            .get(&oid)
            .map(|p| format!("publication {}", p.name)),
        PUBLICATION_REL => {
            let (p, r) = cat.publications.values().find_map(|p| {
                p.rels.iter().find(|r| r.oid == oid).map(|r| (p, r))
            })?;
            Some(format!(
                "publication of {} in publication {}",
                relation_description(cat, r.rel, sp)?,
                p.name
            ))
        }
        PUBLICATION_NAMESPACE => {
            let (p, s) = cat.publications.values().find_map(|p| {
                p.schemas.iter().find(|s| s.oid == oid).map(|s| (p, s))
            })?;
            Some(format!(
                "publication of schema {} in publication {}",
                cat.schema_name(s.schema),
                p.name
            ))
        }
        SUBSCRIPTION => cat
            .subscriptions
            .get(&oid)
            .map(|x| format!("subscription {}", x.name)),
        super::shdepend::TABLESPACE => cat.tablespace_name(oid).map(|n| format!("tablespace {n}")),
        USER_MAPPING => {
            let m = cat.user_mappings.get(&oid)?;
            let user = if m.user == 0 {
                "public".to_string()
            } else {
                cat.roles.get(&m.user).map(|r| r.name.clone())?
            };
            Some(format!(
                "user mapping for {user} on server {}",
                cat.servers.get(&m.server)?.name
            ))
        }
        AM => cat
            .access_method(oid)
            .map(|a| format!("access method {}", a.name)),
        CONVERSION => {
            let c = cat.conversions_all().into_iter().find(|c| c.oid == oid)?;
            // ConversionIsVisible: the first conversion of its name along the path.
            let all = cat.conversions_all();
            let visible = cat
                .effective_path(sp)
                .into_iter()
                .find(|s| all.iter().any(|x| x.schema == *s && x.name == c.name))
                == Some(c.schema);
            Some(format!(
                "conversion {}",
                opc_name(cat, sp, c.schema, &c.name, visible)
            ))
        }
        TRANSFORM => {
            let t = cat.transforms.get(&oid)?;
            Some(format!(
                "transform for {} language {}",
                crate::sql::func::system::format_type_oid(t.ty, -1, cat, sp)?,
                cat.language(t.lang)?.name
            ))
        }
        CAST if cat.casts.contains_key(&oid) => {
            let c = &cat.casts[&oid];
            let ty = |t: u32| {
                crate::sql::func::system::format_type_oid(t, -1, cat, sp)
                    .unwrap_or_else(|| "???".into())
            };
            Some(format!("cast from {} to {}", ty(c.source), ty(c.target)))
        }
        CAST => {
            if let Some((_, from, to)) = super::system::builtin_casts()
                .into_iter()
                .find(|c| c.0 == oid)
            {
                return Some(format!("cast from {} to {}", from.display(), to.display()));
            }
            let r = cat
                .ranges
                .values()
                .find(|r| r.fn_oids.get(5) == Some(&oid))?;
            let rt = r.range_type();
            Some(format!(
                "cast from {} to {}",
                cat.format_type(&DataType::Range(rt.clone()), None, sp),
                cat.format_type(&DataType::Multirange(rt), None, sp)
            ))
        }
        _ => None,
    }
}

/// An operator family's or class's name, qualified unless visible.
fn opc_name(cat: &Catalog, sp: &[String], schema: u32, name: &str, visible: bool) -> String {
    let _ = sp;
    if visible {
        quote_ident(name)
    } else {
        format!("{}.{}", quote_ident(cat.schema_name(schema)), quote_ident(name))
    }
}

/// getOpFamilyDescription.
pub fn opfamily_description(cat: &Catalog, sp: &[String], oid: u32) -> Option<String> {
    let f = cat.opfamily_info(oid)?;
    Some(format!(
        "operator family {} for access method {}",
        opc_name(cat, sp, f.schema, &f.name, cat.opfamily_visible(&f, sp)),
        cat.access_method(f.method)?.name
    ))
}

/// format_operator: `name(lefttype,righttype)`, the name qualified unless visible.
pub fn operator_description(cat: &Catalog, sp: &[String], oid: u32) -> Option<String> {
    if let Some(o) = cat.operators.get(&oid) {
        return Some(cat.operator_signature(o, sp));
    }
    let row = super::static_rows::rows(SysTable::PgOperator);
    let r = row.iter().find(|r| super::static_rows::oid_at(r, 0) == oid)?;
    let ty = |t: u32| {
        if t == 0 {
            "NONE".to_string()
        } else {
            crate::sql::func::system::format_type_oid(t, -1, cat, sp).unwrap_or_else(|| "???".into())
        }
    };
    Some(format!(
        "{}({},{})",
        super::static_rows::text_at(r, 1),
        ty(super::static_rows::oid_at(r, 7)),
        ty(super::static_rows::oid_at(r, 8))
    ))
}

// ── dropping ──

/// How an object was reached for deletion (dependency.c's DEPFLAG_*).
pub mod flag {
    pub const ORIGINAL: u16 = 1;
    pub const NORMAL: u16 = 2;
    pub const AUTO: u16 = 4;
    pub const INTERNAL: u16 = 8;
    pub const PARTITION: u16 = 16;
    pub const EXTENSION: u16 = 32;
    pub const REVERSE: u16 = 64;
    pub const IS_PART: u16 = 128;
    pub const SUBOBJECT: u16 = 256;
}

/// An object to delete, with how it was reached and from what.
pub struct Target {
    pub obj: Addr,
    pub flags: u16,
    pub dependee: Option<Addr>,
}

/// Why a deletion can't go ahead: `obj` is part of `owner` (dropped only with it).
pub struct Required {
    pub obj: Addr,
    pub owner: Addr,
}

struct Walk<'d> {
    deps: &'d [Depend],
    pending: &'d [Addr],
    targets: Vec<Target>,
    /// Where objects without an allocated OID of their own sort.
    keys: std::collections::HashMap<(u32, u32), u64>,
    /// `deps` by depending object and by referenced object ((class, oid): positions in
    /// order), and `targets` by object: each visit looked through all of them, quadratic
    /// in a cascade over a large catalog (DROP SCHEMA ... CASCADE).
    by_obj: std::collections::HashMap<(u32, u32), Vec<u32>>,
    by_ref: std::collections::HashMap<(u32, u32), Vec<u32>>,
    target_ix: std::collections::HashMap<(u32, u32), Vec<u32>>,
}

/// `deps`' positions by (class, oid) of what `key` picks, in order.
fn deps_by(
    deps: &[Depend],
    key: impl Fn(&Depend) -> Addr,
) -> std::collections::HashMap<(u32, u32), Vec<u32>> {
    let mut m: std::collections::HashMap<(u32, u32), Vec<u32>> = std::collections::HashMap::new();
    for (i, d) in deps.iter().enumerate() {
        let a = key(d);
        m.entry((a.0, a.1)).or_default().push(i as u32);
    }
    m
}

/// The visiting order of objects with synthetic OIDs (view rules from before those were
/// kept, domain constraints): as if allocated when Postgres allocates theirs, just after their
/// owner's.
fn sort_keys(cat: &Catalog, scope: Option<&Scope>) -> std::collections::HashMap<(u32, u32), u64> {
    let mut keys = std::collections::HashMap::new();
    for v in pick(&cat.views, scope.map(|s| &s.views)).filter(|v| v.rule == 0) {
        keys.insert(
            (REWRITE, v.rule_oid()),
            (u64::from(v.rowtype.max(v.oid)) + 1) << 16,
        );
    }
    for d in pick_b(&cat.domains, scope.map(|s| &s.domains)) {
        let base = (u64::from(d.oid.max(d.array_oid)) + 1) << 16;
        keys.insert((CONSTRAINT, domain_constraint_oid(d, 63)), base);
        for i in 0..d.checks.len() {
            keys.insert(
                (CONSTRAINT, domain_constraint_oid(d, i)),
                base | (i as u64 + 1),
            );
        }
    }
    keys
}

fn same_object(a: Addr, b: Addr) -> bool {
    a.0 == b.0 && a.1 == b.1
}

impl Walk<'_> {
    /// `object_address_present_add_flags`.
    fn present_add_flags(&mut self, obj: Addr, flags: u16) -> bool {
        let mut found = false;
        let Some(ix) = self.target_ix.get(&(obj.0, obj.1)) else {
            return false;
        };
        for &i in ix.iter().rev() {
            let t = &mut self.targets[i as usize];
            if !same_object(t.obj, obj) {
                continue;
            }
            if t.obj.2 == obj.2 {
                t.flags |= flags;
                found = true;
            } else if t.obj.2 == 0 {
                found = true;
            } else if obj.2 == 0 && flags != 0 {
                t.flags |= flags | flag::SUBOBJECT;
            }
        }
        found
    }

    /// `findDependentObjects`: `obj` and everything that has to go with it, dependents
    /// first, into `targets`. `stack`: the objects being visited, with their flags.
    fn find(
        &mut self,
        obj: Addr,
        mut flags: u16,
        stack: &mut Vec<(Addr, u16)>,
    ) -> Result<(), Required> {
        if stack_present_add_flags(stack, obj, flags) || self.present_add_flags(obj, flags) {
            return Ok(());
        }
        // What this object is part of.
        let mut owning: Option<(Addr, char)> = None;
        let mut partition: Option<Addr> = None;
        let deps = self.deps;
        let mine: Vec<u32> = self.by_obj.get(&(obj.0, obj.1)).cloned().unwrap_or_default();
        for d in mine
            .iter()
            .map(|&i| &deps[i as usize])
            .filter(|d| same_object(d.obj, obj) && (obj.2 == 0 || d.obj.2 == obj.2))
        {
            let other = d.refobj;
            if same_object(other, obj) && obj.2 == 0 {
                continue;
            }
            match d.kind {
                // A member of the extension whose script is running may go (its update
                // scripts drop objects of their own).
                'e' if other.0 == EXTENSION
                    && crate::session::ddl::extension::creating_extension() == Some(other.1) => {}
                'i' | 'e' => {
                    if stack.is_empty() {
                        if self.pending.contains(&other) {
                            return Ok(());
                        }
                        if owning.is_none() || d.kind == 'e' {
                            owning = Some((other, d.kind));
                        }
                        continue;
                    }
                    if stack_present_add_flags(stack, other, 0) {
                        continue;
                    }
                    // Delete the owner instead (which takes this along).
                    self.find(other, flag::REVERSE, stack)?;
                    self.present_add_flags(obj, flags);
                    return Ok(());
                }
                'P' => {
                    flags |= flag::IS_PART;
                    partition = Some(other);
                }
                'S' => {
                    if flags & flag::IS_PART == 0 {
                        partition = Some(other);
                    }
                    flags |= flag::IS_PART;
                }
                _ => {}
            }
        }
        if let Some((owner, _)) = owning {
            return Err(Required {
                obj,
                owner: partition.unwrap_or(owner),
            });
        }
        // Its dependents, newest first.
        let mut dependents: Vec<(Addr, u16)> = (self.by_ref.get(&(obj.0, obj.1)))
            .into_iter()
            .flatten()
            .map(|&i| &deps[i as usize])
            .filter(|d| same_object(d.refobj, obj) && (obj.2 == 0 || d.refobj.2 == obj.2))
            .filter(|d| !(same_object(d.obj, obj) && obj.2 == 0))
            .map(|d| {
                let f = match d.kind {
                    'n' => flag::NORMAL,
                    'a' | 'x' => flag::AUTO,
                    'i' => flag::INTERNAL,
                    'P' | 'S' => flag::PARTITION,
                    _ => flag::EXTENSION,
                };
                (d.obj, f)
            })
            .collect();
        let key = |a: &Addr| {
            self.keys
                .get(&(a.0, a.1))
                .copied()
                .unwrap_or(u64::from(a.1) << 16)
        };
        dependents.sort_by(|(a, _), (b, _)| {
            key(b)
                .cmp(&key(a))
                .then(a.0.cmp(&b.0))
                .then((a.2 as u32).cmp(&(b.2 as u32)))
        });
        stack.push((obj, flags));
        for (o, f) in dependents {
            self.find(o, f, stack)?;
        }
        let (_, flags) = stack.pop().unwrap();
        let dependee = if flags & flag::IS_PART != 0 {
            partition
        } else {
            stack.last().map(|s| s.0)
        };
        self.target_ix
            .entry((obj.0, obj.1))
            .or_default()
            .push(self.targets.len() as u32);
        self.targets.push(Target {
            obj,
            flags,
            dependee,
        });
        Ok(())
    }
}

/// `stack_address_present_add_flags`.
fn stack_present_add_flags(stack: &mut [(Addr, u16)], obj: Addr, flags: u16) -> bool {
    let mut found = false;
    for (s, f) in stack.iter_mut() {
        if !same_object(*s, obj) {
            continue;
        }
        if s.2 == obj.2 {
            *f |= flags;
            found = true;
        } else if s.2 == 0 {
            found = true;
        } else if obj.2 == 0 && flags != 0 {
            *f |= flags | flag::SUBOBJECT;
        }
    }
    found
}

/// Everything deleting `roots` (together) deletes, in deletion order (`performMultipleDeletions`).
pub fn find_dependents(
    cat: &Catalog,
    deps: &[Depend],
    roots: &[Addr],
) -> Result<Vec<Target>, Required> {
    find_dependents_in(cat, deps, roots, None)
}

/// `find_dependents` over the rows of `scope`'s objects (`depends_in`).
fn find_dependents_in(
    cat: &Catalog,
    deps: &[Depend],
    roots: &[Addr],
    scope: Option<&Scope>,
) -> Result<Vec<Target>, Required> {
    let mut w = Walk {
        deps,
        pending: roots,
        targets: Vec::new(),
        keys: sort_keys(cat, scope),
        by_obj: deps_by(deps, |d| d.obj),
        by_ref: deps_by(deps, |d| d.refobj),
        target_ix: Default::default(),
    };
    for &r in roots {
        w.find(r, flag::ORIGINAL, &mut Vec::new())?;
    }
    Ok(w.targets)
}

/// What `reportDependentObjects` says about `targets`.
pub enum Report {
    /// Partition-dependent `obj` would go without what it is a partition of.
    Required(Required),
    /// Without CASCADE: "X depends on Y" lines.
    Restricted(Vec<String>, usize),
    /// With CASCADE: "drop cascades to X" lines (and how many weren't listed).
    Cascades(Vec<String>, usize),
}

/// Postgres lists at most this many dependents in a message.
const MAX_REPORTED_DEPS: usize = 100;

pub fn report(cat: &Catalog, sp: &[String], targets: &[Target], cascade: bool) -> Report {
    for t in targets {
        if t.flags & flag::IS_PART != 0 && t.flags & flag::PARTITION == 0 {
            return Report::Required(Required {
                obj: t.obj,
                owner: t.dependee.unwrap_or((0, 0, 0)),
            });
        }
    }
    let mut lines = Vec::new();
    let mut more = 0;
    let mut ok = true;
    for t in targets.iter().rev() {
        if t.flags & (flag::ORIGINAL | flag::SUBOBJECT) != 0 {
            continue;
        }
        let Some(desc) = describe(cat, sp, t.obj) else {
            continue;
        };
        if t.flags & (flag::AUTO | flag::INTERNAL | flag::PARTITION | flag::EXTENSION) != 0 {
            continue;
        }
        let line = if cascade {
            format!("drop cascades to {desc}")
        } else {
            ok = false;
            let Some(other) = t.dependee.and_then(|d| describe(cat, sp, d)) else {
                continue;
            };
            format!("{desc} depends on {other}")
        };
        if lines.len() < MAX_REPORTED_DEPS {
            lines.push(line);
        } else {
            more += 1;
        }
    }
    if ok {
        Report::Cascades(lines, more)
    } else {
        Report::Restricted(lines, more)
    }
}

/// A collation's name as messages show it: qualified unless the search path finds it.
pub fn collation_name_in_path(
    cat: &Catalog,
    sp: &[String],
    d: &crate::types::collation::CollationDef,
) -> String {
    let found = crate::types::collation::resolve(cat, sp, std::slice::from_ref(&d.name));
    if found.is_some_and(|f| f.oid == d.oid) {
        quote_ident(&d.name)
    } else {
        format!(
            "{}.{}",
            quote_ident(cat.schemas.get(&d.namespace).map_or("?", |s| s.name.as_str())),
            quote_ident(&d.name)
        )
    }
}
